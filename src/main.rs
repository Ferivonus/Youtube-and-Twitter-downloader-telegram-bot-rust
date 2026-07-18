use anyhow::{Context, Result};
use chrono::Utc;
use dotenvy::dotenv;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, env, path::PathBuf, process::Stdio, sync::Arc, time::Duration};
use teloxide::{
    macros::BotCommands,
    payloads::SendMessageSetters,
    prelude::*,
    types::{InputFile, ParseMode},
    utils::command::BotCommands as _,
};
use tokio::{
    fs,
    io::{AsyncBufReadExt, BufReader},
    process::Command,
    sync::RwLock,
    time::Instant,
};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::{fmt::writer::MakeWriterExt, EnvFilter};

const DOWNLOAD_DIR: &str = "downloads";
const LOG_FILE: &str = "download_log.json";
const USERS_FILE: &str = "users.json";
const BOT_LOG_FILE: &str = "bot_log.txt";

#[derive(Debug, Serialize, Deserialize, Clone)]
struct DownloadRecord {
    url: String,
    timestamp: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct UserRecord {
    id: u64,
    username: Option<String>,
    first_name: String,
    language_code: Option<String>,
    first_use: String,
    last_use: String,
    download_count: u32,
    #[serde(default)]
    is_authorized: bool,
}

struct BotState {
    users: RwLock<HashMap<String, UserRecord>>,
    logs: RwLock<HashMap<String, Vec<DownloadRecord>>>,
}

#[derive(BotCommands, Clone)]
#[command(rename_rule = "lowercase", description = "Komutlar:")]
enum SystemCommand {
    #[command(description = "Sistemi başlatır")]
    Start,
    #[command(description = "Kullanım dokümantasyonunu gösterir")]
    Help,
    #[command(description = "Yetkilendirme. Kullanım: /login <şifre>")]
    Login(String),
}

fn init_logging() -> WorkerGuard {
    let file_appender = tracing_appender::rolling::daily("./logs", BOT_LOG_FILE);
    let (non_blocking_file, guard) = tracing_appender::non_blocking(file_appender);
    let (non_blocking_stdout, _) = tracing_appender::non_blocking(std::io::stdout());

    tracing_subscriber::fmt()
        .with_writer(non_blocking_file.and(non_blocking_stdout))
        .with_env_filter(EnvFilter::from_default_env().add_directive(tracing::Level::INFO.into()))
        .init();

    guard
}

fn generate_progress_bar(percent: f32) -> String {
    let total_blocks = 12;
    let filled = ((percent / 100.0) * total_blocks as f32).round() as usize;
    let filled = filled.min(total_blocks);
    format!(
        "{}{}",
        "█".repeat(filled),
        "░".repeat(total_blocks - filled)
    )
}

/// Disk yozlaşmasını (corruption) önlemek için asenkron atomik dosya yazımı
async fn atomic_write(path: &str, data: &str) -> Result<()> {
    let tmp_path = format!("{}.tmp", path);
    fs::write(&tmp_path, data).await?;
    fs::rename(&tmp_path, path).await?;
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let _guard = init_logging();
    dotenv().ok();

    let bot_token = env::var("TELEGRAM_BOT_TOKEN").context("TELEGRAM_BOT_TOKEN missing")?;
    let bot_password =
        Arc::new(env::var("BOT_PASSWORD").unwrap_or_else(|_| "admin123".to_string()));

    // Telegram Bot API yapısı (Yerel sunucu desteğiyle)
    let mut bot = Bot::new(bot_token);
    if let Ok(api_url) = env::var("TELEGRAM_API_URL") {
        tracing::info!(
            "Bypassing default API, routing to Local Server: {}",
            api_url
        );
        bot = bot.set_api_url(reqwest::Url::parse(&api_url).context("Invalid TELEGRAM_API_URL")?);
    }

    let size_limit_mb = env::var("MAX_FILE_SIZE_MB")
        .unwrap_or_else(|_| "50".to_string())
        .parse::<u64>()
        .unwrap_or(50);
    let size_limit_bytes = Arc::new(size_limit_mb * 1024 * 1024);

    let cookies_path = PathBuf::from("cookies.txt");
    let cookies_arg: Arc<Option<String>> =
        Arc::new(cookies_path.exists().then(|| "cookies.txt".to_string()));

    fs::create_dir_all(DOWNLOAD_DIR).await?;

    let users_data = fs::read_to_string(USERS_FILE)
        .await
        .unwrap_or_else(|_| "{}".to_string());
    let logs_data = fs::read_to_string(LOG_FILE)
        .await
        .unwrap_or_else(|_| "{}".to_string());

    let state = Arc::new(BotState {
        users: RwLock::new(serde_json::from_str(&users_data).unwrap_or_default()),
        logs: RwLock::new(serde_json::from_str(&logs_data).unwrap_or_default()),
    });

    bot.set_my_commands(SystemCommand::bot_commands()).await?;

    let handler = dptree::entry()
        .branch(
            Update::filter_message()
                .filter_command::<SystemCommand>()
                .endpoint(handle_command),
        )
        .branch(Update::filter_message().endpoint(handle_url_message));

    tracing::info!(
        "Dispatching initialized. Target size limit: {} MB",
        size_limit_mb
    );

    Dispatcher::builder(bot, handler)
        .dependencies(dptree::deps![
            state,
            cookies_arg,
            bot_password,
            size_limit_bytes
        ])
        .default_handler(|up| async move {
            tracing::warn!("Unhandled update: {:?}", up);
        })
        .enable_ctrlc_handler()
        .build()
        .dispatch()
        .await;

    Ok(())
}

async fn handle_command(
    bot: Bot,
    message: Message,
    command: SystemCommand,
    state: Arc<BotState>,
    bot_password: Arc<String>,
) -> Result<()> {
    // FIX: .from() is deprecated. Using .from field reference.
    let user = message.from.as_ref().context("Missing user context")?;
    let uid_key = user.id.0.to_string();

    track_and_check_auth(&state, user, &uid_key).await?;

    match command {
        SystemCommand::Start => {
            bot.send_message(
                message.chat.id,
                format!(
                    "<b>Sistem Aktif</b> 🟢\nHoş geldiniz, <code>{}</code>.",
                    user.first_name
                ),
            )
            .parse_mode(ParseMode::Html)
            .await?;
        }
        SystemCommand::Help => {
            bot.send_message(message.chat.id, "<b>Docs:</b>\nX (Twitter) veya YouTube linki gönderin. Sistem videoyu işleyip arşivleyecektir.")
                .parse_mode(ParseMode::Html).await?;
        }
        SystemCommand::Login(pwd) => {
            if pwd == *bot_password {
                authorize_user(&state, &uid_key).await?;
                bot.send_message(
                    message.chat.id,
                    "🔐 <b>Auth Başarılı.</b>\nSistem erişimi onaylandı.",
                )
                .parse_mode(ParseMode::Html)
                .await?;
            } else {
                bot.send_message(
                    message.chat.id,
                    "❌ <b>Hata:</b> Kimlik doğrulama reddedildi.",
                )
                .parse_mode(ParseMode::Html)
                .await?;
            }
        }
    }
    Ok(())
}

async fn handle_url_message(
    bot: Bot,
    message: Message,
    state: Arc<BotState>,
    cookies_arg: Arc<Option<String>>,
    size_limit_bytes: Arc<u64>,
) -> Result<()> {
    if let Some(text) = message.text() {
        let msg = text.trim();
        let user = message.from.as_ref().context("Missing user context")?;
        let uid_key = user.id.0.to_string();

        let is_authorized = track_and_check_auth(&state, user, &uid_key).await?;

        if !is_authorized {
            bot.send_message(message.chat.id, "⛔ <b>Erişim Reddedildi.</b>\nYetkilendirme gereklidir: <code>/login <şifre></code>")
                .parse_mode(ParseMode::Html).await?;
            return Ok(());
        }

        if !is_supported_url(msg) {
            bot.send_message(
                message.chat.id,
                "⚠️ <b>Geçersiz Protokol.</b>\nYalnızca X ve YouTube desteklenmektedir.",
            )
            .parse_mode(ParseMode::Html)
            .await?;
            return Ok(());
        }

        let progress_msg = bot
            .send_message(
                message.chat.id,
                "⏳ <b>Başlatılıyor...</b>\nMeta veriler ayrıştırılıyor.",
            )
            .parse_mode(ParseMode::Html)
            .await?;

        if let Err(e) = execute_pipeline(
            msg.to_string(),
            &bot,
            message.chat.id,
            progress_msg.id,
            cookies_arg.as_deref(),
            uid_key.clone(),
            &state,
            *size_limit_bytes,
        )
        .await
        {
            let _ = bot.delete_message(message.chat.id, progress_msg.id).await;
            bot.send_message(
                message.chat.id,
                format!("❌ <b>İşlem Hatası:</b>\n<code>{}</code>", e),
            )
            .parse_mode(ParseMode::Html)
            .await?;
            tracing::error!("Pipeline failed for {}: {}", uid_key, e);
        } else {
            tracing::info!("Pipeline fulfilled for {}", uid_key);
        }
    }
    Ok(())
}

fn is_supported_url(s: &str) -> bool {
    s.contains("twitter.com")
        || s.contains("x.com")
        || s.contains("youtube.com")
        || s.contains("youtu.be")
}

#[allow(clippy::too_many_arguments)]
async fn execute_pipeline(
    url: String,
    bot: &Bot,
    chat_id: teloxide::types::ChatId,
    msg_id: teloxide::types::MessageId,
    cookies_path: Option<&str>,
    uid_key: String,
    state: &Arc<BotState>,
    size_limit_bytes: u64,
) -> Result<()> {
    let download_dir = PathBuf::from(DOWNLOAD_DIR);
    let timestamp = Utc::now().format("%Y%m%d_%H%M%S");
    let file_prefix = format!("{}_{}", uid_key, timestamp);

    let raw_path = download_dir.join(format!("{}_raw.mp4", file_prefix));
    let compressed_path = download_dir.join(format!("{}_final.mp4", file_prefix));

    let mut args = vec![
        "-o".to_string(),
        raw_path.to_string_lossy().to_string(),
        "--merge-output-format".to_string(),
        "mp4".to_string(),
        "--newline".to_string(),
        url.clone(),
    ];

    if let Some(p) = cookies_path {
        args.splice(0..0, vec!["--cookies".to_string(), p.to_string()]);
    }

    let mut child = Command::new("./yt-dlp")
        .args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("yt-dlp ikili dosyası başlatılamadı")?;

    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let re = Regex::new(r"\[download\]\s+(\d{1,3}\.\d)%").unwrap();
    let mut last_update = Instant::now() - Duration::from_secs(5);
    let mut last_pct = String::new();

    while let Some(line) = lines.next_line().await? {
        if let Some(cap) = re.captures(&line) {
            let pct = &cap[1];
            if pct != last_pct && last_update.elapsed() > Duration::from_secs(2) {
                last_pct = pct.to_string();
                last_update = Instant::now();
                let bar = generate_progress_bar(pct.parse().unwrap_or(0.0));
                let _ = bot
                    .edit_message_text(
                        chat_id,
                        msg_id,
                        format!("📥 <b>İndiriliyor...</b>\n<code>{}</code> {}%", bar, pct),
                    )
                    .parse_mode(ParseMode::Html)
                    .await;
            }
        }
    }

    let status = child.wait().await?;
    if !status.success() {
        let mut stderr = String::new();
        if let Some(mut err_stream) = child.stderr.take() {
            use tokio::io::AsyncReadExt;
            let _ = err_stream.read_to_string(&mut stderr).await;
        }
        return Err(anyhow::anyhow!("yt-dlp işlemi çöktü: {}", stderr));
    }

    if !fs::try_exists(&raw_path).await.unwrap_or(false) {
        return Err(anyhow::anyhow!(
            "I/O hatası: İndirilen medya diske yazılamadı."
        ));
    }

    let mut final_path = raw_path.clone();
    let initial_size = fs::metadata(&raw_path).await?.len();

    if initial_size > size_limit_bytes {
        let _ = bot
            .edit_message_text(
                chat_id,
                msg_id,
                "⚙️ <b>Sıkıştırılıyor...</b>\nBoyut sınırı aşıldı, ffmpeg ile işleniyor.",
            )
            .parse_mode(ParseMode::Html)
            .await;

        let ff_args = [
            "-i",
            raw_path.to_str().unwrap(),
            "-vf",
            "scale=-2:480",
            "-c:v",
            "libx264",
            "-preset",
            "fast",
            "-crf",
            "28",
            "-c:a",
            "aac",
            "-b:a",
            "128k",
            "-movflags",
            "+faststart",
            "-threads",
            "0",
            "-y",
            compressed_path.to_str().unwrap(),
        ];

        let ff_status = Command::new("ffmpeg")
            .args(&ff_args)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .status()
            .await?;

        if !ff_status.success() {
            return Err(anyhow::anyhow!("FFmpeg kodlama katmanı başarısız oldu."));
        }

        let compressed_size = fs::metadata(&compressed_path).await?.len();
        if compressed_size > size_limit_bytes {
            let _ = fs::remove_file(&compressed_path).await;
            return Err(anyhow::anyhow!(
                "Sıkıştırma işlemine rağmen dosya boyutu çok büyük: {:.2} MB",
                compressed_size as f64 / 1_048_576.0
            ));
        }

        final_path = compressed_path.clone();
        let _ = fs::remove_file(&raw_path).await; // Optimize edilmiş versiyon varsa raw silinir.
    }

    let _ = bot
        .edit_message_text(
            chat_id,
            msg_id,
            "🚀 <b>Yükleniyor...</b>\nTelegram sunucularına aktarılıyor.",
        )
        .parse_mode(ParseMode::Html)
        .await;

    bot.send_video(chat_id, InputFile::file(&final_path))
        .caption(format!(
            "✅ <b>Arşivlendi:</b> <code>{}</code>",
            final_path.file_name().unwrap().to_string_lossy()
        ))
        .parse_mode(ParseMode::Html)
        .await
        .context("Telegram API üzerinden yükleme esnasında hata")?;

    let _ = bot.delete_message(chat_id, msg_id).await;
    log_successful_download(state, &uid_key, url).await?;

    Ok(())
}

async fn track_and_check_auth(
    state: &Arc<BotState>,
    user: &teloxide::types::User,
    uid_key: &str,
) -> Result<bool> {
    let now = Utc::now().to_rfc3339();

    // Borrow Checker Fix: Daraltılmış scope içerisinde veriyi güncelle ve yetki durumunu al.
    let is_authorized = {
        let mut users = state.users.write().await;
        let record = users
            .entry(uid_key.to_string())
            .or_insert_with(|| UserRecord {
                id: user.id.0,
                username: user.username.clone(),
                first_name: user.first_name.clone(),
                language_code: user.language_code.clone(),
                first_use: now.clone(),
                last_use: now.clone(),
                download_count: 0,
                is_authorized: false,
            });
        record.last_use = now;
        record.is_authorized
    };

    // Bellekteki verinin temiz bir kopyasını alıp I/O (disk) işlemi sırasında kilidi meşgul etmiyoruz.
    let users_snapshot = state.users.read().await.clone();
    atomic_write(USERS_FILE, &serde_json::to_string_pretty(&users_snapshot)?).await?;

    Ok(is_authorized)
}

async fn authorize_user(state: &Arc<BotState>, uid_key: &str) -> Result<()> {
    {
        let mut users = state.users.write().await;
        if let Some(record) = users.get_mut(uid_key) {
            record.is_authorized = true;
        }
    }

    let users_snapshot = state.users.read().await.clone();
    atomic_write(USERS_FILE, &serde_json::to_string_pretty(&users_snapshot)?).await?;

    Ok(())
}

async fn log_successful_download(state: &Arc<BotState>, uid_key: &str, url: String) -> Result<()> {
    let now = Utc::now().to_rfc3339();

    {
        let mut logs = state.logs.write().await;
        logs.entry(uid_key.to_string())
            .or_default()
            .push(DownloadRecord {
                url,
                timestamp: now.clone(),
            });
    }
    let logs_snapshot = state.logs.read().await.clone();
    atomic_write(LOG_FILE, &serde_json::to_string_pretty(&logs_snapshot)?).await?;

    {
        let mut users = state.users.write().await;
        if let Some(record) = users.get_mut(uid_key) {
            record.download_count += 1;
            record.last_use = now;
        }
    }
    let users_snapshot = state.users.read().await.clone();
    atomic_write(USERS_FILE, &serde_json::to_string_pretty(&users_snapshot)?).await?;

    Ok(())
}
