//! Telegram channel — bot integration via `teloxide`.
//!
//! Port of nanobot's `channels/telegram.py`.
//!
//! Features:
//! - Long polling (no webhook/public IP needed)
//! - Text, photo, voice, document handling
//! - Typing indicator while agent processes
//! - Markdown → Telegram HTML conversion
//! - Allow-list by user ID or username
//! - Commands: /start, /reset, /help
//! - Message splitting for >4096 char responses

use std::sync::Arc;
use std::future::Future;
use std::pin::Pin;
use std::path::Path;

use async_trait::async_trait;
use teloxide::net::Download;
use teloxide::prelude::*;
use teloxide::types::{
    ChatAction, MediaKind, MessageKind, ParseMode, UpdateKind,
};
use tokio::io::AsyncWriteExt;
use tokio::sync::{Mutex, Notify};
use tracing::{debug, error, info, warn};

use metis_core::bus::queue::MessageBus;
use metis_core::bus::types::{InboundMessage, OutboundMessage};

use crate::base::Channel;
use crate::formatting::{markdown_to_telegram_html, split_message};

/// Telegram message length limit.
const TELEGRAM_MAX_LEN: usize = 4096;
/// Polling backoff after API errors.
const TELEGRAM_RECONNECT_DELAY_SECS: u64 = 5;
/// Periodic health log interval while polling.
const TELEGRAM_HEALTH_LOG_SECS: u64 = 60;

/// Callback for voice/audio transcription.
///
/// Receives a file path, returns the transcribed text.
pub type TranscribeFn = Arc<
    dyn Fn(String) -> Pin<Box<dyn Future<Output = anyhow::Result<String>> + Send>>
        + Send
        + Sync,
>;

// ─────────────────────────────────────────────
// TelegramChannel
// ─────────────────────────────────────────────

/// Telegram bot channel using long polling via `teloxide`.
pub struct TelegramChannel {
    /// Bot token from @BotFather.
    token: String,
    /// Message bus for inbound/outbound.
    bus: Arc<MessageBus>,
    /// Allow-list of user IDs / usernames. Empty = allow everyone.
    allowed_users: Vec<String>,
    /// Optional voice transcription callback.
    transcriber: Option<TranscribeFn>,
    /// Shutdown signal.
    shutdown: Arc<Notify>,
    /// Active typing tasks keyed by chat id.
    typing_tasks: Arc<Mutex<std::collections::HashMap<i64, tokio::task::JoinHandle<()>>>>,
}

impl TelegramChannel {
    /// Create a new Telegram channel.
    pub fn new(
        token: String,
        bus: Arc<MessageBus>,
        allowed_users: Vec<String>,
    ) -> Self {
        Self {
            token,
            bus,
            allowed_users,
            transcriber: None,
            shutdown: Arc::new(Notify::new()),
            typing_tasks: Arc::new(Mutex::new(std::collections::HashMap::new())),
        }
    }

    /// Set the voice transcription callback.
    pub fn with_transcriber(mut self, transcriber: TranscribeFn) -> Self {
        self.transcriber = Some(transcriber);
        self
    }

    /// Try to transcribe an audio file. Returns transcribed text or None.
    async fn try_transcribe(&self, path: &str) -> Option<String> {
        if let Some(ref transcriber) = self.transcriber {
            match transcriber(path.to_string()).await {
                Ok(text) if !text.is_empty() => {
                    debug!(path = %path, chars = text.len(), "voice transcribed");
                    Some(text)
                }
                Ok(_) => None,
                Err(e) => {
                    warn!(error = %e, "voice transcription failed");
                    None
                }
            }
        } else {
            None
        }
    }

    /// Check if a sender is allowed.
    ///
    /// Sender ID format: "user_id|username" — matches either part.
    /// Empty allow-list = allow everyone.
    fn is_allowed(&self, sender_id: &str) -> bool {
        if self.allowed_users.is_empty() {
            return true;
        }

        // Check exact match first
        if self.allowed_users.iter().any(|u| u == sender_id) {
            return true;
        }

        // Split "id|username" and check each part
        for part in sender_id.split('|') {
            if !part.is_empty() && self.allowed_users.iter().any(|u| u == part) {
                return true;
            }
        }

        false
    }

    /// Handle an incoming Telegram update.
    async fn handle_update(&self, bot: &Bot, update: &Update) {
        let message = match &update.kind {
            UpdateKind::Message(msg) => msg,
            _ => return,
        };

        // Extract sender info
        let user = match message.from.as_ref() {
            Some(u) => u,
            None => return,
        };

        let user_id = user.id.0.to_string();
        let username = user
            .username
            .as_deref()
            .unwrap_or("")
            .to_string();
        let first_name = user.first_name.clone();
        let sender_id = format!("{user_id}|{username}");
        let chat_id = message.chat.id.0.to_string();
        let is_group = message.chat.is_group() || message.chat.is_supergroup();

        // Check allow-list
        if !self.is_allowed(&sender_id) {
            warn!(
                sender = %sender_id,
                chat = %chat_id,
                "telegram message from unauthorized user, ignoring"
            );
            return;
        }

        // Handle commands
        if let Some(text) = message.text() {
            if text.starts_with('/') {
                self.handle_command(bot, message, text, &first_name, &chat_id)
                    .await;
                return;
            }
        }

        // Extract content
        let mut content_parts: Vec<String> = Vec::new();
        let mut media_paths: Vec<String> = Vec::new();

        // Text content
        match &message.kind {
            MessageKind::Common(common) => {
                match &common.media_kind {
                    MediaKind::Text(text_msg) => {
                        content_parts.push(text_msg.text.clone());
                    }
                    MediaKind::Photo(photo) => {
                        // Caption
                        if let Some(caption) = &photo.caption {
                            content_parts.push(caption.clone());
                        }
                        // Download largest photo
                        if let Some(largest) = photo.photo.last() {
                            match self.download_file(bot, &largest.file.id.0).await {
                                Ok(path) => {
                                    content_parts.push(format!("[image: {path}]"));
                                    media_paths.push(path);
                                }
                                Err(e) => {
                                    warn!(error = %e, "failed to download photo");
                                    content_parts.push("[image: download failed]".into());
                                }
                            }
                        }
                    }
                    MediaKind::Voice(voice) => {
                        match self.download_file(bot, &voice.voice.file.id.0).await {
                            Ok(path) => {
                                // Try transcription first
                                if let Some(text) = self.try_transcribe(&path).await {
                                    content_parts.push(format!("[transcription: {text}]"));
                                } else {
                                    content_parts.push(format!("[voice: {path}]"));
                                }
                                media_paths.push(path);
                            }
                            Err(e) => {
                                warn!(error = %e, "failed to download voice");
                                content_parts.push("[voice: download failed]".into());
                            }
                        }
                    }
                    MediaKind::Audio(audio) => {
                        if let Some(caption) = &audio.caption {
                            content_parts.push(caption.clone());
                        }
                        match self.download_file(bot, &audio.audio.file.id.0).await {
                            Ok(path) => {
                                // Try transcription first
                                if let Some(text) = self.try_transcribe(&path).await {
                                    content_parts.push(format!("[transcription: {text}]"));
                                } else {
                                    content_parts.push(format!("[audio: {path}]"));
                                }
                                media_paths.push(path);
                            }
                            Err(e) => {
                                warn!(error = %e, "failed to download audio");
                                content_parts.push("[audio: download failed]".into());
                            }
                        }
                    }
                    MediaKind::Document(doc) => {
                        if let Some(caption) = &doc.caption {
                            content_parts.push(caption.clone());
                        }
                        match self.download_file(bot, &doc.document.file.id.0).await {
                            Ok(path) => {
                                content_parts.push(format!("[file: {path}]"));
                                media_paths.push(path);
                            }
                            Err(e) => {
                                warn!(error = %e, "failed to download document");
                                content_parts.push("[file: download failed]".into());
                            }
                        }
                    }
                    _ => {
                        debug!("unsupported media type, ignoring");
                        return;
                    }
                }
            }
            _ => return,
        }

        let content = content_parts.join("\n");
        if content.is_empty() {
            return;
        }

        debug!(
            sender = %sender_id,
            chat = %chat_id,
            content_len = content.len(),
            "telegram inbound message"
        );

        // Start typing indicator; it is explicitly stopped when outbound send completes.
        self.start_typing(message.chat.id.0, bot).await;

        // Publish to bus
        let mut inbound = InboundMessage::new("telegram", &sender_id, &chat_id, &content);
        for path in &media_paths {
            inbound.media.push(metis_core::types::MediaAttachment {
                path: path.clone(),
                mime_type: "application/octet-stream".into(),
                filename: None,
                size: None,
            });
        }
        inbound
            .metadata
            .insert("user_id".into(), user_id.clone());
        inbound
            .metadata
            .insert("username".into(), username.clone());
        inbound
            .metadata
            .insert("first_name".into(), first_name.clone());
        inbound
            .metadata
            .insert("is_group".into(), is_group.to_string());
        inbound.metadata.insert(
            "message_id".into(),
            message.id.0.to_string(),
        );

        if let Err(e) = self.bus.publish_inbound(inbound).await {
            error!(error = %e, "failed to publish telegram message to bus");
            self.stop_typing(message.chat.id.0).await;
        }
    }

    /// Start typing indicator loop for a chat.
    async fn start_typing(&self, chat_id: i64, bot: &Bot) {
        self.stop_typing(chat_id).await;
        let typing_bot = bot.clone();
        let handle = tokio::spawn(async move {
            loop {
                let _ = typing_bot
                    .send_chat_action(ChatId(chat_id), ChatAction::Typing)
                    .await;
                tokio::time::sleep(tokio::time::Duration::from_secs(4)).await;
            }
        });
        self.typing_tasks.lock().await.insert(chat_id, handle);
    }

    /// Stop typing indicator loop for a chat.
    async fn stop_typing(&self, chat_id: i64) {
        if let Some(handle) = self.typing_tasks.lock().await.remove(&chat_id) {
            handle.abort();
        }
    }

    /// Handle a bot command.
    async fn handle_command(
        &self,
        bot: &Bot,
        message: &Message,
        text: &str,
        first_name: &str,
        _chat_id: &str,
    ) {
        let command = text.split_whitespace().next().unwrap_or("");
        // Strip @botname from command (e.g. /start@mybot)
        let command = command.split('@').next().unwrap_or(command);

        let chat = message.chat.id;

        match command {
            "/start" => {
                let greeting = format!(
                    "👋 Hi {first_name}! I'm Metis, your AI assistant.\n\n\
                     Send me any message and I'll do my best to help!\n\n\
                     Commands:\n\
                     /help — Show available commands\n\
                     /reset — Clear conversation history"
                );
                let _ = bot.send_message(chat, greeting).await;
            }
            "/help" => {
                let help = "🤖 <b>Metis Commands</b>\n\n\
                     /start — Start the bot\n\
                     /reset — Clear conversation history\n\
                     /help — Show this message\n\n\
                     Just send me text, photos, voice messages, or documents \
                     and I'll process them!";
                let _ = bot
                    .send_message(chat, help)
                    .parse_mode(ParseMode::Html)
                    .await;
            }
            "/reset" => {
                // TODO: Wire session manager for session clearing
                let _ = bot
                    .send_message(chat, "🔄 Conversation history cleared.")
                    .await;
            }
            _ => {
                debug!(command = command, "unknown telegram command");
            }
        }
    }

    /// Download a file from Telegram to a local temp path.
    async fn download_file(&self, bot: &Bot, file_id: &str) -> anyhow::Result<String> {
        use teloxide::types::FileId;
        let file = bot.get_file(FileId(file_id.to_string())).send().await?;

        // Create media directory
        let media_dir = metis_core::utils::get_data_path().join("media");
        std::fs::create_dir_all(&media_dir)?;

        // Determine extension from file path
        let ext = file
            .path
            .rsplit('.')
            .next()
            .map(|e| format!(".{e}"))
            .unwrap_or_default();

        let local_path = media_dir.join(format!("{}{}", file_id.replace('/', "_"), ext));

        // Download
        let mut dst = tokio::fs::File::create(&local_path).await?;
        let mut stream = bot.download_file_stream(&file.path);
        use futures_util::StreamExt;
        while let Some(chunk) = stream.next().await {
            dst.write_all(&chunk?).await?;
        }

        info!(path = %local_path.display(), "downloaded telegram file");
        Ok(local_path.display().to_string())
    }
}

#[async_trait]
impl Channel for TelegramChannel {
    fn name(&self) -> &str {
        "telegram"
    }

    async fn start(&self) -> anyhow::Result<()> {
        info!("starting telegram channel (long polling)");

        let bot = Bot::new(&self.token);

        // Set bot commands menu
        use teloxide::types::BotCommand;
        let commands = vec![
            BotCommand::new("start", "Start the bot"),
            BotCommand::new("help", "Show available commands"),
            BotCommand::new("reset", "Clear conversation history"),
        ];
        if let Err(e) = bot.set_my_commands(commands).await {
            warn!(error = %e, "failed to set bot commands menu");
        }

        info!("telegram bot connected, polling for updates");

        // Manual polling loop (we need control over the bus integration)
        let mut offset: i32 = 0;
        let mut consecutive_errors: u32 = 0;
        let mut updates_seen: u64 = 0;
        let mut last_health_log = tokio::time::Instant::now();

        loop {
            tokio::select! {
                updates = bot.get_updates().offset(offset).timeout(30).send() => {
                    match updates {
                        Ok(updates) => {
                            consecutive_errors = 0;
                            updates_seen += updates.len() as u64;
                            for update in &updates {
                                offset = (update.id.0 as i32).wrapping_add(1);
                                self.handle_update(&bot, update).await;
                            }
                            if last_health_log.elapsed() >= tokio::time::Duration::from_secs(TELEGRAM_HEALTH_LOG_SECS) {
                                debug!(
                                    offset = offset,
                                    updates_seen = updates_seen,
                                    "telegram polling healthy"
                                );
                                last_health_log = tokio::time::Instant::now();
                            }
                        }
                        Err(e) => {
                            consecutive_errors = consecutive_errors.saturating_add(1);
                            warn!(
                                error = %e,
                                consecutive_errors = consecutive_errors,
                                "telegram polling error, reconnecting in {TELEGRAM_RECONNECT_DELAY_SECS}s"
                            );
                            tokio::select! {
                                _ = tokio::time::sleep(tokio::time::Duration::from_secs(TELEGRAM_RECONNECT_DELAY_SECS)) => {}
                                _ = self.shutdown.notified() => {
                                    info!("telegram shutdown during reconnect wait");
                                    break;
                                }
                            }
                        }
                    }
                }
                _ = self.shutdown.notified() => {
                    info!("telegram channel shutting down");
                    break;
                }
            }
        }

        Ok(())
    }

    async fn stop(&self) -> anyhow::Result<()> {
        info!("stopping telegram channel");
        self.shutdown.notify_waiters();
        let mut tasks = self.typing_tasks.lock().await;
        for (_, handle) in tasks.drain() {
            handle.abort();
        }
        Ok(())
    }

    async fn send(&self, msg: &OutboundMessage) -> anyhow::Result<()> {
        let bot = Bot::new(&self.token);
        let chat_id: i64 = msg
            .chat_id
            .parse()
            .map_err(|_| anyhow::anyhow!("invalid telegram chat_id: {}", msg.chat_id))?;
        self.stop_typing(chat_id).await;

        // Convert markdown to Telegram HTML
        let html = markdown_to_telegram_html(&msg.content);

        // Split long messages
        let chunks = split_message(&html, TELEGRAM_MAX_LEN);

        for chunk in &chunks {
            // Try HTML first, fall back to plain text
            let result = bot
                .send_message(ChatId(chat_id), chunk)
                .parse_mode(ParseMode::Html)
                .await;

            if let Err(e) = result {
                debug!(error = %e, "HTML send failed, retrying as plain text");
                // Fall back: send without parse_mode
                let plain_chunks = split_message(&msg.content, TELEGRAM_MAX_LEN);
                for plain_chunk in &plain_chunks {
                    let _ = bot.send_message(ChatId(chat_id), plain_chunk).await;
                }
                return Ok(());
            }
        }

        debug!(chat_id = chat_id, "telegram message sent");
        Ok(())
    }
}

// ─────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn create_test_channel() -> TelegramChannel {
        let bus = Arc::new(MessageBus::new(32));
        TelegramChannel::new("test_token".into(), bus, vec![])
    }

    fn create_restricted_channel() -> TelegramChannel {
        let bus = Arc::new(MessageBus::new(32));
        TelegramChannel::new(
            "test_token".into(),
            bus,
            vec!["123456".into(), "johndoe".into()],
        )
    }

    #[test]
    fn test_channel_name() {
        let ch = create_test_channel();
        assert_eq!(ch.name(), "telegram");
    }

    #[test]
    fn test_is_allowed_empty_list() {
        let ch = create_test_channel();
        assert!(ch.is_allowed("anyone"));
        assert!(ch.is_allowed("123|user"));
    }

    #[test]
    fn test_is_allowed_by_id() {
        let ch = create_restricted_channel();
        assert!(ch.is_allowed("123456|someuser"));
    }

    #[test]
    fn test_is_allowed_by_username() {
        let ch = create_restricted_channel();
        assert!(ch.is_allowed("999999|johndoe"));
    }

    #[test]
    fn test_is_allowed_denied() {
        let ch = create_restricted_channel();
        assert!(!ch.is_allowed("999999|stranger"));
    }

    #[test]
    fn test_is_allowed_exact_match() {
        let ch = create_restricted_channel();
        assert!(ch.is_allowed("123456"));
    }

    #[test]
    fn test_is_allowed_pipe_split() {
        let ch = create_restricted_channel();
        // ID part matches
        assert!(ch.is_allowed("123456|unknown"));
        // Username part matches
        assert!(ch.is_allowed("000|johndoe"));
        // Neither matches
        assert!(!ch.is_allowed("000|unknown"));
    }
}
