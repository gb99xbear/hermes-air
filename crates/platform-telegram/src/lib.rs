// hermes-platform-telegram - Telegram Bot API adapter
use hermes_platform_adapter::{BaseAdapter, BasePlatformConfig, PlatformAdapter};
use hermes_protocol::{
    DeliveryReceipt, DeliveryStatus, IncomingMessage, MessageContent, MessageId, OutgoingMessage,
    Platform, PlatformError, PlatformEvent,
};
use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;
use time::OffsetDateTime;
use tokio::sync::{mpsc, RwLock};
use tracing::{error, info, warn};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelegramConfig {
    pub bot_token: String,
    pub webhook_url: Option<String>,
    pub allowed_updates: Vec<String>,
    pub drop_pending_updates: bool,
    pub timeout_secs: u64,
    #[serde(flatten)]
    pub base: BasePlatformConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelegramUser {
    pub id: i64,
    pub is_bot: bool,
    pub first_name: String,
    pub last_name: Option<String>,
    pub username: Option<String>,
    pub language_code: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelegramChat {
    pub id: i64,
    #[serde(rename = "type")]
    pub chat_type: String,
    pub title: Option<String>,
    pub username: Option<String>,
    pub first_name: Option<String>,
    pub last_name: Option<String>,
    #[serde(default)]
    pub is_forum: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelegramMessageEntity {
    #[serde(rename = "type")]
    pub entity_type: String,
    pub offset: usize,
    pub length: usize,
    pub url: Option<String>,
    pub user: Option<TelegramUser>,
    pub language: Option<String>,
    pub custom_emoji_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelegramMessage {
    pub message_id: i64,
    pub from: Option<TelegramUser>,
    pub chat: TelegramChat,
    pub date: i64,
    #[serde(default)]
    pub message_thread_id: Option<i64>,
    #[serde(default)]
    pub is_topic_message: Option<bool>,
    pub text: Option<String>,
    pub entities: Option<Vec<TelegramMessageEntity>>,
    pub caption: Option<String>,
    pub caption_entities: Option<Vec<TelegramMessageEntity>>,
    pub photo: Option<Vec<TelegramPhotoSize>>,
    pub document: Option<TelegramDocument>,
    pub audio: Option<TelegramAudio>,
    pub voice: Option<TelegramVoice>,
    pub video: Option<TelegramVideo>,
    pub sticker: Option<TelegramSticker>,
    pub location: Option<TelegramLocation>,
    pub contact: Option<TelegramContact>,
    pub reply_to_message: Option<Box<TelegramMessage>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelegramVoice {
    pub file_id: String,
    pub file_unique_id: String,
    pub duration: i64,
    pub mime_type: Option<String>,
    pub file_size: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelegramFile {
    pub file_id: String,
    pub file_unique_id: Option<String>,
    pub file_size: Option<i64>,
    pub file_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelegramPhotoSize {
    pub file_id: String,
    pub file_unique_id: String,
    pub width: i32,
    pub height: i32,
    pub file_size: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelegramDocument {
    pub file_id: String,
    pub file_unique_id: String,
    pub thumbnail: Option<TelegramPhotoSize>,
    pub file_name: Option<String>,
    pub mime_type: Option<String>,
    pub file_size: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelegramAudio {
    pub file_id: String,
    pub file_unique_id: String,
    pub duration: i64,
    pub performer: Option<String>,
    pub title: Option<String>,
    pub mime_type: Option<String>,
    pub file_size: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelegramVideo {
    pub file_id: String,
    pub file_unique_id: String,
    pub width: i32,
    pub height: i32,
    pub duration: i64,
    pub thumbnail: Option<TelegramPhotoSize>,
    pub mime_type: Option<String>,
    pub file_size: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelegramSticker {
    pub file_id: String,
    pub file_unique_id: String,
    pub width: i32,
    pub height: i32,
    pub is_animated: bool,
    pub is_video: bool,
    pub thumbnail: Option<TelegramPhotoSize>,
    pub emoji: Option<String>,
    pub set_name: Option<String>,
    pub file_size: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelegramLocation {
    pub longitude: f64,
    pub latitude: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelegramContact {
    pub phone_number: String,
    pub first_name: String,
    pub last_name: Option<String>,
    pub user_id: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelegramUpdate {
    pub update_id: i64,
    pub message: Option<TelegramMessage>,
    pub edited_message: Option<TelegramMessage>,
    pub channel_post: Option<TelegramMessage>,
    pub edited_channel_post: Option<TelegramMessage>,
    pub callback_query: Option<TelegramCallbackQuery>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelegramCallbackQuery {
    pub id: String,
    pub from: TelegramUser,
    pub message: Option<TelegramMessage>,
    pub inline_message_id: Option<String>,
    pub chat_instance: String,
    pub data: Option<String>,
    pub game_short_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct InlineKeyboardButton {
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub callback_data: Option<String>,
}

impl InlineKeyboardButton {
    pub fn new(text: impl Into<String>, callback_data: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            callback_data: Some(callback_data.into()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct InlineKeyboardMarkup {
    pub inline_keyboard: Vec<Vec<InlineKeyboardButton>>,
}

impl InlineKeyboardMarkup {
    pub fn new(inline_keyboard: Vec<Vec<InlineKeyboardButton>>) -> Self {
        Self { inline_keyboard }
    }
}

#[derive(Debug, Serialize)]
struct SendMessageRequest {
    chat_id: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    message_thread_id: Option<i64>,
    text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    parse_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reply_to_message_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    disable_web_page_preview: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply_markup: Option<InlineKeyboardMarkup>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EditMessageTextRequest {
    pub chat_id: i64,
    pub message_id: i64,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parse_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disable_web_page_preview: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply_markup: Option<InlineKeyboardMarkup>,
}

#[derive(Debug, Serialize)]
struct AnswerCallbackQueryRequest<'a> {
    callback_query_id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    show_alert: Option<bool>,
}

#[derive(Debug, Serialize)]
struct SendPhotoRequest {
    chat_id: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    message_thread_id: Option<i64>,
    photo: String,
    caption: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    parse_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reply_to_message_id: Option<i64>,
}

#[derive(Debug, Serialize)]
struct SendDocumentRequest {
    chat_id: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    message_thread_id: Option<i64>,
    document: String,
    caption: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reply_to_message_id: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SendVoiceRequest {
    pub chat_id: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message_thread_id: Option<i64>,
    pub voice: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub caption: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parse_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply_to_message_id: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SendAudioRequest {
    pub chat_id: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message_thread_id: Option<i64>,
    pub audio: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub caption: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parse_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub performer: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply_to_message_id: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SendVideoRequest {
    pub chat_id: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message_thread_id: Option<i64>,
    pub video: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub caption: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parse_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply_to_message_id: Option<i64>,
}

#[derive(Debug, Serialize)]
struct SendChatActionRequest<'a> {
    chat_id: i64,
    action: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    message_thread_id: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct TelegramResponse<T> {
    ok: bool,
    result: Option<T>,
    error_code: Option<i32>,
    description: Option<String>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct SentMessage {
    pub message_id: i64,
    date: i64,
    chat: TelegramChat,
}

pub fn truncate_caption_1024(caption: Option<&str>) -> Option<String> {
    caption.map(|c| {
        let utf16_count = c.encode_utf16().count();
        if utf16_count <= 1024 {
            c.to_string()
        } else {
            let mut count = 0;
            let mut char_idx = 0;
            for ch in c.chars() {
                let len16 = ch.len_utf16();
                if count + len16 > 1024 {
                    break;
                }
                count += len16;
                char_idx += ch.len_utf8();
            }
            c[..char_idx].to_string()
        }
    })
}

#[allow(dead_code)]
pub fn strip_html_tags(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut in_tag = false;
    for c in input.chars() {
        if c == '<' {
            in_tag = true;
        } else if c == '>' {
            in_tag = false;
        } else if !in_tag {
            out.push(c);
        }
    }
    out
}

pub fn chunk_message(text: &str, max_chunk: usize) -> Vec<String> {
    if text.len() <= max_chunk {
        return vec![text.to_string()];
    }

    let mut chunks = Vec::new();
    let mut remaining = text;

    while remaining.len() > max_chunk {
        let mut split_boundary = max_chunk;
        while split_boundary > 0 && !remaining.is_char_boundary(split_boundary) {
            split_boundary -= 1;
        }

        let region = &remaining[..split_boundary];
        let mut split_at = region.rfind('\n').unwrap_or(0);
        if split_at < max_chunk / 2 {
            if let Some(space_idx) = region.rfind(' ') {
                if space_idx >= max_chunk / 2 {
                    split_at = space_idx;
                }
            }
        }

        if split_at < 1 {
            split_at = split_boundary;
        }

        let chunk = &remaining[..split_at];
        chunks.push(chunk.trim_end().to_string());
        remaining = remaining[split_at..].trim_start_matches(|c| c == '\r' || c == '\n');
    }

    if !remaining.is_empty() {
        chunks.push(remaining.to_string());
    }

    chunks
}

pub fn looks_like_polling_conflict(status_code: u16, error_desc: &str) -> bool {
    if status_code == 409 {
        return true;
    }
    let lower = error_desc.to_lowercase();
    lower.contains("conflict")
        || lower.contains("terminated by other getupdates request")
        || lower.contains("another bot instance is running")
}

pub fn looks_like_message_not_modified(error_desc: &str) -> bool {
    let lower = error_desc.to_lowercase();
    lower.contains("message is not modified") || lower.contains("not modified")
}

pub fn looks_like_parse_error(status_code: u16, error_desc: &str) -> bool {
    let lower = error_desc.to_lowercase();
    lower.contains("can't parse")
        || lower.contains("bad request")
        || lower.contains("entities")
        || lower.contains("parse error")
        || lower.contains("tag")
        || (status_code == 400 && (lower.contains("parse") || lower.contains("markdown") || lower.contains("html")))
}

#[derive(Debug)]
pub struct TelegramAdapter {
    base: BaseAdapter<TelegramConfig>,
    client: Client,
    pub bot_info: Arc<RwLock<Option<TelegramUser>>>,
    pub last_poll_time: Arc<RwLock<Instant>>,
    #[allow(dead_code)]
    pending_messages: Arc<RwLock<HashMap<MessageId, (OutgoingMessage, mpsc::UnboundedSender<DeliveryReceipt>)>>>,
}

impl TelegramAdapter {
    pub fn new() -> Self {
        Self {
            base: BaseAdapter::new(Platform::Telegram),
            client: Client::new(),
            bot_info: Arc::new(RwLock::new(None)),
            last_poll_time: Arc::new(RwLock::new(Instant::now())),
            pending_messages: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    fn api_url(&self, method: &str) -> String {
        let config = self.base.config.as_ref().expect("Not initialized");
        format!("https://api.telegram.org/bot{}/{}", config.bot_token, method)
    }

    async fn make_request<T: for<'de> Deserialize<'de>>(&self, method: &str, body: &impl Serialize) -> Result<T, PlatformError> {
        let url = self.api_url(method);
        let config = self.base.config.as_ref().expect("Not initialized");

        let response = self
            .client
            .post(&url)
            .json(body)
            .timeout(std::time::Duration::from_secs(config.timeout_secs))
            .send()
            .await
            .map_err(|e| PlatformError::Network(e.to_string()))?;

        let status = response.status();
        let tg_response: TelegramResponse<T> = response
            .json()
            .await
            .map_err(|e| PlatformError::Internal(e.to_string()))?;

        if !tg_response.ok {
            return Err(PlatformError::Api {
                code: tg_response.error_code.map(|c| c.to_string()).unwrap_or_else(|| status.to_string()),
                message: tg_response.description.unwrap_or_else(|| "Unknown Telegram API error".to_string()),
            });
        }

        tg_response.result.ok_or_else(|| PlatformError::Internal("Empty result from Telegram API".into()))
    }

    pub fn download_url(&self, file_path: &str) -> Result<String, PlatformError> {
        let config = self.base.config.as_ref().ok_or_else(|| PlatformError::Internal("Not initialized".into()))?;
        Ok(format!("https://api.telegram.org/file/bot{}/{}", config.bot_token, file_path.trim_start_matches('/')))
    }

    pub async fn get_file(&self, file_id: &str) -> Result<TelegramFile, PlatformError> {
        let body = serde_json::json!({ "file_id": file_id });
        self.make_request::<TelegramFile>("getFile", &body).await
    }

    pub async fn download_file_to_cache(&self, file_path: &str, destination: &Path) -> Result<PathBuf, PlatformError> {
        let url = self.download_url(file_path)?;
        if let Some(parent) = destination.parent() {
            tokio::fs::create_dir_all(parent).await.map_err(|e| PlatformError::Internal(e.to_string()))?;
        }

        let resp = self.client.get(&url).send().await.map_err(|e| PlatformError::Network(e.to_string()))?;
        if !resp.status().is_success() {
            return Err(PlatformError::Api {
                code: resp.status().as_u16().to_string(),
                message: format!("Failed to download file from Telegram: HTTP {}", resp.status()),
            });
        }

        let bytes = resp.bytes().await.map_err(|e| PlatformError::Network(e.to_string()))?;
        tokio::fs::write(destination, &bytes).await.map_err(|e| PlatformError::Internal(e.to_string()))?;
        Ok(destination.to_path_buf())
    }

    pub async fn cache_inbound_file(&self, file_id: &str, default_ext: &str) -> Result<String, PlatformError> {
        let file_info = self.get_file(file_id).await?;
        if let Some(file_path) = file_info.file_path {
            let ext = Path::new(&file_path)
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| format!(".{}", e))
                .unwrap_or_else(|| default_ext.to_string());

            let cache_dir = PathBuf::from("/tmp/hermes_telegram_cache");
            tokio::fs::create_dir_all(&cache_dir).await.map_err(|e| PlatformError::Internal(e.to_string()))?;
            let dest = cache_dir.join(format!("{}_{}{}", file_id, Uuid::new_v4().simple(), ext));
            let path = self.download_file_to_cache(&file_path, &dest).await?;
            Ok(path.to_string_lossy().to_string())
        } else {
            Err(PlatformError::Internal("Telegram did not return file_path".into()))
        }
    }

    pub async fn edit_message_text(
        &self,
        chat_id: i64,
        message_id: i64,
        text: &str,
    ) -> Result<(), PlatformError> {
        let html_req = EditMessageTextRequest {
            chat_id,
            message_id,
            text: text.to_string(),
            parse_mode: Some("HTML".to_string()),
            disable_web_page_preview: Some(true),
            reply_markup: None,
        };

        match self.make_request::<serde_json::Value>("editMessageText", &html_req).await {
            Ok(_) => Ok(()),
            Err(PlatformError::Api { code, message }) => {
                if looks_like_message_not_modified(&message) {
                    return Ok(());
                }
                let status_code = code.parse::<u16>().unwrap_or(0);
                if looks_like_parse_error(status_code, &message) {
                    warn!(
                        "[telegram] Edit with HTML formatting failed ({}: {}), retrying as plain text",
                        code, message
                    );
                    let plain_text = strip_html_tags(text);
                    let plain_req = EditMessageTextRequest {
                        chat_id,
                        message_id,
                        text: plain_text,
                        parse_mode: None,
                        disable_web_page_preview: Some(true),
                        reply_markup: None,
                    };
                    match self.make_request::<serde_json::Value>("editMessageText", &plain_req).await {
                        Ok(_) => Ok(()),
                        Err(PlatformError::Api { message: p_msg, .. }) if looks_like_message_not_modified(&p_msg) => {
                            Ok(())
                        }
                        Err(other) => Err(other),
                    }
                } else {
                    Err(PlatformError::Api { code, message })
                }
            }
            Err(other) => Err(other),
        }
    }

    pub async fn edit_text(
        &self,
        chat_id: i64,
        message_id: i64,
        text: &str,
    ) -> Result<(), PlatformError> {
        self.edit_message_text(chat_id, message_id, text).await
    }

    pub async fn delete_message(
        &self,
        chat_id: i64,
        message_id: i64,
    ) -> Result<(), PlatformError> {
        let body = serde_json::json!({
            "chat_id": chat_id,
            "message_id": message_id,
        });
        match self.make_request::<bool>("deleteMessage", &body).await {
            Ok(_) => Ok(()),
            Err(e) => {
                warn!("[telegram] Failed to delete message {} in chat {}: {:?}", message_id, chat_id, e);
                Ok(()) // Treat deletion errors leniently
            }
        }
    }

    pub async fn send_message_with_keyboard(
        &self,
        chat_id: i64,
        text: &str,
        keyboard: InlineKeyboardMarkup,
    ) -> Result<SentMessage, PlatformError> {
        self.send_message_with_keyboard_and_thread(chat_id, None, text, keyboard).await
    }

    pub async fn send_message_with_keyboard_and_thread(
        &self,
        chat_id: i64,
        message_thread_id: Option<i64>,
        text: &str,
        keyboard: InlineKeyboardMarkup,
    ) -> Result<SentMessage, PlatformError> {
        let html_req = SendMessageRequest {
            chat_id,
            message_thread_id,
            text: text.to_string(),
            parse_mode: Some("HTML".to_string()),
            reply_to_message_id: None,
            disable_web_page_preview: Some(true),
            reply_markup: Some(keyboard.clone()),
        };

        match self.make_request::<SentMessage>("sendMessage", &html_req).await {
            Ok(sent) => Ok(sent),
            Err(PlatformError::Api { code, message }) => {
                let status_code = code.parse::<u16>().unwrap_or(0);
                if looks_like_parse_error(status_code, &message) {
                    warn!(
                        "[telegram] Send with keyboard HTML failed ({}: {}), retrying plain text",
                        code, message
                    );
                    let plain_req = SendMessageRequest {
                        chat_id,
                        message_thread_id,
                        text: strip_html_tags(text),
                        parse_mode: None,
                        reply_to_message_id: None,
                        disable_web_page_preview: Some(true),
                        reply_markup: Some(keyboard),
                    };
                    self.make_request::<SentMessage>("sendMessage", &plain_req).await
                } else {
                    Err(PlatformError::Api { code, message })
                }
            }
            Err(e) => Err(e),
        }
    }

    pub async fn edit_message_with_keyboard(
        &self,
        chat_id: i64,
        message_id: i64,
        text: &str,
        keyboard: Option<InlineKeyboardMarkup>,
    ) -> Result<(), PlatformError> {
        let html_req = EditMessageTextRequest {
            chat_id,
            message_id,
            text: text.to_string(),
            parse_mode: Some("HTML".to_string()),
            disable_web_page_preview: Some(true),
            reply_markup: keyboard.clone(),
        };

        match self.make_request::<serde_json::Value>("editMessageText", &html_req).await {
            Ok(_) => Ok(()),
            Err(PlatformError::Api { code, message }) => {
                if looks_like_message_not_modified(&message) {
                    return Ok(());
                }
                let status_code = code.parse::<u16>().unwrap_or(0);
                if looks_like_parse_error(status_code, &message) {
                    warn!(
                        "[telegram] Edit with keyboard HTML failed ({}: {}), retrying plain text",
                        code, message
                    );
                    let plain_req = EditMessageTextRequest {
                        chat_id,
                        message_id,
                        text: strip_html_tags(text),
                        parse_mode: None,
                        disable_web_page_preview: Some(true),
                        reply_markup: keyboard,
                    };
                    match self.make_request::<serde_json::Value>("editMessageText", &plain_req).await {
                        Ok(_) => Ok(()),
                        Err(PlatformError::Api { message: p_msg, .. }) if looks_like_message_not_modified(&p_msg) => {
                            Ok(())
                        }
                        Err(other) => Err(other),
                    }
                } else {
                    Err(PlatformError::Api { code, message })
                }
            }
            Err(other) => Err(other),
        }
    }

    pub async fn answer_callback_query(
        &self,
        callback_query_id: &str,
        text: Option<&str>,
    ) -> Result<(), PlatformError> {
        let req = AnswerCallbackQueryRequest {
            callback_query_id,
            text,
            show_alert: None,
        };
        let _: serde_json::Value = self.make_request("answerCallbackQuery", &req).await?;
        Ok(())
    }

    pub async fn send_chat_action(&self, chat_id: i64, action: &str) -> Result<(), PlatformError> {
        self.send_chat_action_with_thread(chat_id, None, action).await
    }

    pub async fn send_chat_action_with_thread(&self, chat_id: i64, message_thread_id: Option<i64>, action: &str) -> Result<(), PlatformError> {
        let req = SendChatActionRequest { chat_id, action, message_thread_id };
        let _: serde_json::Value = self.make_request("sendChatAction", &req).await?;
        Ok(())
    }

    pub async fn send_chunk_with_fallback(
        &self,
        chat_id: i64,
        chunk: &str,
        reply_to_message_id: Option<i64>,
    ) -> Result<SentMessage, PlatformError> {
        self.send_chunk_with_fallback_and_thread(chat_id, None, chunk, reply_to_message_id).await
    }

    pub async fn send_chunk_with_fallback_and_thread(
        &self,
        chat_id: i64,
        message_thread_id: Option<i64>,
        chunk: &str,
        reply_to_message_id: Option<i64>,
    ) -> Result<SentMessage, PlatformError> {
        let html_req = SendMessageRequest {
            chat_id,
            message_thread_id,
            text: chunk.to_string(),
            parse_mode: Some("HTML".to_string()),
            reply_to_message_id,
            disable_web_page_preview: Some(true),
            reply_markup: None,
        };

        match self.make_request::<SentMessage>("sendMessage", &html_req).await {
            Ok(sent) => Ok(sent),
            Err(PlatformError::Api { code, message }) => {
                let status_code = code.parse::<u16>().unwrap_or(0);
                if looks_like_parse_error(status_code, &message) {
                    warn!(
                        "[telegram] Send with HTML formatting failed ({}: {}), retrying as plain text",
                        code, message
                    );
                    let plain_text = strip_html_tags(chunk);
                    let plain_req = SendMessageRequest {
                        chat_id,
                        message_thread_id,
                        text: plain_text,
                        parse_mode: None,
                        reply_to_message_id,
                        disable_web_page_preview: Some(true),
                        reply_markup: None,
                    };
                    self.make_request::<SentMessage>("sendMessage", &plain_req).await
                } else {
                    Err(PlatformError::Api { code, message })
                }
            }
            Err(other_err) => Err(other_err),
        }
    }

    pub async fn send_photo_file(
        &self,
        chat_id: i64,
        message_thread_id: Option<i64>,
        photo_path: &Path,
        caption: Option<&str>,
        reply_to_message_id: Option<i64>,
    ) -> Result<SentMessage, PlatformError> {
        let file_name = photo_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("photo.png")
            .to_string();
        let file_bytes = tokio::fs::read(photo_path)
            .await
            .map_err(|e| PlatformError::Internal(format!("Failed to read photo file: {}", e)))?;

        let mime_type = match photo_path.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase().as_str() {
            "jpg" | "jpeg" => "image/jpeg",
            "webp" => "image/webp",
            "gif" => "image/gif",
            _ => "image/png",
        };

        let url = self.api_url("sendPhoto");
        let config = self.base.config.as_ref().expect("Not initialized");

        let truncated = truncate_caption_1024(caption);
        let has_caption = truncated.is_some();
        let html_caption = truncated.clone();
        let plain_caption = truncated.as_deref().map(strip_html_tags);

        // Attempt 1: with HTML parse_mode if caption is present
        if has_caption {
            let mut form = reqwest::multipart::Form::new()
                .text("chat_id", chat_id.to_string());
            if let Some(thread_id) = message_thread_id {
                form = form.text("message_thread_id", thread_id.to_string());
            }
            if let Some(reply_id) = reply_to_message_id {
                form = form.text("reply_to_message_id", reply_id.to_string());
            }
            if let Some(cap) = &html_caption {
                form = form.text("caption", cap.clone());
                form = form.text("parse_mode", "HTML");
            }
            let part = reqwest::multipart::Part::bytes(file_bytes.clone())
                .file_name(file_name.clone())
                .mime_str(mime_type)
                .unwrap_or_else(|_| reqwest::multipart::Part::bytes(file_bytes.clone()).file_name(file_name.clone()));
            form = form.part("photo", part);

            let resp = self
                .client
                .post(&url)
                .multipart(form)
                .timeout(std::time::Duration::from_secs(config.timeout_secs))
                .send()
                .await
                .map_err(|e| PlatformError::Network(e.to_string()))?;

            let status = resp.status();
            let tg_resp: TelegramResponse<SentMessage> = resp
                .json()
                .await
                .map_err(|e| PlatformError::Internal(e.to_string()))?;

            if tg_resp.ok {
                if let Some(sent) = tg_resp.result {
                    return Ok(sent);
                }
            } else {
                let code = tg_resp.error_code.map(|c| c.to_string()).unwrap_or_else(|| status.to_string());
                let message = tg_resp.description.unwrap_or_else(|| "Unknown Telegram API error".to_string());
                let status_code = code.parse::<u16>().unwrap_or(0);
                if !looks_like_parse_error(status_code, &message) {
                    return Err(PlatformError::Api { code, message });
                }
                warn!("[telegram] sendPhoto HTML caption failed, retrying plain");
            }
        }

        // Send without parse_mode (or caption is None / fallback)
        let mut form = reqwest::multipart::Form::new()
            .text("chat_id", chat_id.to_string());
        if let Some(thread_id) = message_thread_id {
            form = form.text("message_thread_id", thread_id.to_string());
        }
        if let Some(reply_id) = reply_to_message_id {
            form = form.text("reply_to_message_id", reply_id.to_string());
        }
        if let Some(cap) = &plain_caption {
            form = form.text("caption", cap.clone());
        }
        let part = reqwest::multipart::Part::bytes(file_bytes)
            .file_name(file_name.clone())
            .mime_str(mime_type)
            .unwrap_or_else(|_| reqwest::multipart::Part::bytes(vec![]));
        form = form.part("photo", part);

        let resp = self
            .client
            .post(&url)
            .multipart(form)
            .timeout(std::time::Duration::from_secs(config.timeout_secs))
            .send()
            .await
            .map_err(|e| PlatformError::Network(e.to_string()))?;

        let status = resp.status();
        let tg_resp: TelegramResponse<SentMessage> = resp
            .json()
            .await
            .map_err(|e| PlatformError::Internal(e.to_string()))?;

        if !tg_resp.ok {
            return Err(PlatformError::Api {
                code: tg_resp.error_code.map(|c| c.to_string()).unwrap_or_else(|| status.to_string()),
                message: tg_resp.description.unwrap_or_else(|| "Unknown Telegram API error".to_string()),
            });
        }
        tg_resp.result.ok_or_else(|| PlatformError::Internal("Empty result from Telegram API".into()))
    }

    pub async fn send_voice(
        &self,
        chat_id: i64,
        message_thread_id: Option<i64>,
        audio_path: &Path,
        caption: Option<&str>,
        duration: Option<u32>,
    ) -> Result<SentMessage, PlatformError> {
        let file_name = audio_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("voice.ogg")
            .to_string();
        let file_bytes = tokio::fs::read(audio_path)
            .await
            .map_err(|e| PlatformError::Internal(format!("Failed to read voice audio file: {}", e)))?;

        let url = self.api_url("sendVoice");
        let config = self.base.config.as_ref().expect("Not initialized");

        // Format caption with 1024 char bound and HTML parse error fallback to plain text matching _send_voice_bubble
        let truncated = truncate_caption_1024(caption);
        let has_caption = truncated.is_some();
        let html_caption = truncated.clone();
        let plain_caption = truncated.as_deref().map(strip_html_tags);

        // Attempt 1: with HTML parse_mode if caption is present
        if has_caption {
            let mut form = reqwest::multipart::Form::new()
                .text("chat_id", chat_id.to_string());
            if let Some(thread_id) = message_thread_id {
                form = form.text("message_thread_id", thread_id.to_string());
            }
            if let Some(dur) = duration {
                form = form.text("duration", dur.to_string());
            }
            if let Some(cap) = &html_caption {
                form = form.text("caption", cap.clone());
                form = form.text("parse_mode", "HTML");
            }
            let part = reqwest::multipart::Part::bytes(file_bytes.clone())
                .file_name(file_name.clone())
                .mime_str("audio/ogg")
                .unwrap_or_else(|_| reqwest::multipart::Part::bytes(file_bytes.clone()).file_name(file_name.clone()));
            form = form.part("voice", part);

            let resp = self
                .client
                .post(&url)
                .multipart(form)
                .timeout(std::time::Duration::from_secs(config.timeout_secs))
                .send()
                .await
                .map_err(|e| PlatformError::Network(e.to_string()))?;

            let status = resp.status();
            let tg_resp: TelegramResponse<SentMessage> = resp
                .json()
                .await
                .map_err(|e| PlatformError::Internal(e.to_string()))?;

            if tg_resp.ok {
                if let Some(res) = tg_resp.result {
                    return Ok(res);
                }
            } else {
                let code_str = tg_resp.error_code.map(|c| c.to_string()).unwrap_or_else(|| status.to_string());
                let msg_str = tg_resp.description.unwrap_or_default();
                let status_code = code_str.parse::<u16>().unwrap_or(0);
                if looks_like_parse_error(status_code, &msg_str) {
                    warn!(
                        "[telegram] sendVoice with HTML caption failed ({}: {}), falling back to plain text caption",
                        code_str, msg_str
                    );
                    // Fallback to plain caption without parse_mode
                    let mut plain_form = reqwest::multipart::Form::new()
                        .text("chat_id", chat_id.to_string());
                    if let Some(thread_id) = message_thread_id {
                        plain_form = plain_form.text("message_thread_id", thread_id.to_string());
                    }
                    if let Some(dur) = duration {
                        plain_form = plain_form.text("duration", dur.to_string());
                    }
                    if let Some(pcap) = &plain_caption {
                        plain_form = plain_form.text("caption", pcap.clone());
                    }
                    let part = reqwest::multipart::Part::bytes(file_bytes.clone())
                        .file_name(file_name.clone())
                        .mime_str("audio/ogg")
                        .unwrap_or_else(|_| reqwest::multipart::Part::bytes(file_bytes.clone()).file_name(file_name.clone()));
                    plain_form = plain_form.part("voice", part);

                    let plain_resp = self
                        .client
                        .post(&url)
                        .multipart(plain_form)
                        .timeout(std::time::Duration::from_secs(config.timeout_secs))
                        .send()
                        .await
                        .map_err(|e| PlatformError::Network(e.to_string()))?;

                    let plain_status = plain_resp.status();
                    let plain_tg_resp: TelegramResponse<SentMessage> = plain_resp
                        .json()
                        .await
                        .map_err(|e| PlatformError::Internal(e.to_string()))?;

                    if !plain_tg_resp.ok {
                        return Err(PlatformError::Api {
                            code: plain_tg_resp.error_code.map(|c| c.to_string()).unwrap_or_else(|| plain_status.to_string()),
                            message: plain_tg_resp.description.unwrap_or_else(|| "Unknown Telegram API error".to_string()),
                        });
                    }
                    return plain_tg_resp.result.ok_or_else(|| PlatformError::Internal("Empty result from Telegram API".into()));
                } else {
                    return Err(PlatformError::Api { code: code_str, message: msg_str });
                }
            }
        }

        // Send without parse_mode (or caption is None)
        let mut form = reqwest::multipart::Form::new()
            .text("chat_id", chat_id.to_string());
        if let Some(thread_id) = message_thread_id {
            form = form.text("message_thread_id", thread_id.to_string());
        }
        if let Some(dur) = duration {
            form = form.text("duration", dur.to_string());
        }
        let part = reqwest::multipart::Part::bytes(file_bytes)
            .file_name(file_name.clone())
            .mime_str("audio/ogg")
            .unwrap_or_else(|_| reqwest::multipart::Part::bytes(vec![]));
        form = form.part("voice", part);

        let resp = self
            .client
            .post(&url)
            .multipart(form)
            .timeout(std::time::Duration::from_secs(config.timeout_secs))
            .send()
            .await
            .map_err(|e| PlatformError::Network(e.to_string()))?;

        let status = resp.status();
        let tg_resp: TelegramResponse<SentMessage> = resp
            .json()
            .await
            .map_err(|e| PlatformError::Internal(e.to_string()))?;

        if !tg_resp.ok {
            return Err(PlatformError::Api {
                code: tg_resp.error_code.map(|c| c.to_string()).unwrap_or_else(|| status.to_string()),
                message: tg_resp.description.unwrap_or_else(|| "Unknown Telegram API error".to_string()),
            });
        }
        tg_resp.result.ok_or_else(|| PlatformError::Internal("Empty result from Telegram API".into()))
    }

    pub async fn send_audio(
        &self,
        chat_id: i64,
        message_thread_id: Option<i64>,
        audio_path: &Path,
        caption: Option<&str>,
        duration: Option<u32>,
        title: Option<&str>,
        performer: Option<&str>,
    ) -> Result<SentMessage, PlatformError> {
        let file_name = audio_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("audio.mp3")
            .to_string();
        let file_bytes = tokio::fs::read(audio_path)
            .await
            .map_err(|e| PlatformError::Internal(format!("Failed to read audio file: {}", e)))?;

        let url = self.api_url("sendAudio");
        let config = self.base.config.as_ref().expect("Not initialized");

        let truncated = truncate_caption_1024(caption);
        let mut form = reqwest::multipart::Form::new()
            .text("chat_id", chat_id.to_string());
        if let Some(thread_id) = message_thread_id {
            form = form.text("message_thread_id", thread_id.to_string());
        }
        if let Some(dur) = duration {
            form = form.text("duration", dur.to_string());
        }
        if let Some(t) = title {
            form = form.text("title", t.to_string());
        }
        if let Some(p) = performer {
            form = form.text("performer", p.to_string());
        }
        if let Some(cap) = &truncated {
            form = form.text("caption", cap.clone());
        }
        let part = reqwest::multipart::Part::bytes(file_bytes)
            .file_name(file_name.clone())
            .mime_str("audio/mpeg")
            .unwrap_or_else(|_| reqwest::multipart::Part::bytes(vec![]));
        form = form.part("audio", part);

        let resp = self
            .client
            .post(&url)
            .multipart(form)
            .timeout(std::time::Duration::from_secs(config.timeout_secs))
            .send()
            .await
            .map_err(|e| PlatformError::Network(e.to_string()))?;

        let status = resp.status();
        let tg_resp: TelegramResponse<SentMessage> = resp
            .json()
            .await
            .map_err(|e| PlatformError::Internal(e.to_string()))?;

        if !tg_resp.ok {
            return Err(PlatformError::Api {
                code: tg_resp.error_code.map(|c| c.to_string()).unwrap_or_else(|| status.to_string()),
                message: tg_resp.description.unwrap_or_else(|| "Unknown Telegram API error".to_string()),
            });
        }
        tg_resp.result.ok_or_else(|| PlatformError::Internal("Empty result from Telegram API".into()))
    }

    pub async fn send_video(
        &self,
        chat_id: i64,
        message_thread_id: Option<i64>,
        video_path: &Path,
        caption: Option<&str>,
        duration: Option<u32>,
        width: Option<u32>,
        height: Option<u32>,
    ) -> Result<SentMessage, PlatformError> {
        let file_name = video_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("video.mp4")
            .to_string();
        let file_bytes = tokio::fs::read(video_path)
            .await
            .map_err(|e| PlatformError::Internal(format!("Failed to read video file: {}", e)))?;

        let url = self.api_url("sendVideo");
        let config = self.base.config.as_ref().expect("Not initialized");

        let truncated = truncate_caption_1024(caption);
        let mut form = reqwest::multipart::Form::new()
            .text("chat_id", chat_id.to_string());
        if let Some(thread_id) = message_thread_id {
            form = form.text("message_thread_id", thread_id.to_string());
        }
        if let Some(dur) = duration {
            form = form.text("duration", dur.to_string());
        }
        if let Some(w) = width {
            form = form.text("width", w.to_string());
        }
        if let Some(h) = height {
            form = form.text("height", h.to_string());
        }
        if let Some(cap) = &truncated {
            form = form.text("caption", cap.clone());
        }
        let part = reqwest::multipart::Part::bytes(file_bytes)
            .file_name(file_name.clone())
            .mime_str("video/mp4")
            .unwrap_or_else(|_| reqwest::multipart::Part::bytes(vec![]));
        form = form.part("video", part);

        let resp = self
            .client
            .post(&url)
            .multipart(form)
            .timeout(std::time::Duration::from_secs(config.timeout_secs))
            .send()
            .await
            .map_err(|e| PlatformError::Network(e.to_string()))?;

        let status = resp.status();
        let tg_resp: TelegramResponse<SentMessage> = resp
            .json()
            .await
            .map_err(|e| PlatformError::Internal(e.to_string()))?;

        if !tg_resp.ok {
            return Err(PlatformError::Api {
                code: tg_resp.error_code.map(|c| c.to_string()).unwrap_or_else(|| status.to_string()),
                message: tg_resp.description.unwrap_or_else(|| "Unknown Telegram API error".to_string()),
            });
        }
        tg_resp.result.ok_or_else(|| PlatformError::Internal("Empty result from Telegram API".into()))
    }

    pub fn should_process_message(msg: &TelegramMessage, bot_info: Option<&TelegramUser>) -> bool {
        let chat_type = msg.chat.chat_type.as_str();
        if chat_type == "private" {
            return true;
        }

        if chat_type != "group" && chat_type != "supergroup" {
            return true;
        }

        let bot_username = bot_info.and_then(|b| b.username.as_deref()).map(|u| u.trim_start_matches('@').to_lowercase());
        let bot_id = bot_info.map(|b| b.id);

        let text = msg.text.as_deref().or(msg.caption.as_deref()).unwrap_or("").trim();

        if text.starts_with('/') {
            return true;
        }

        if let Some(ref reply) = msg.reply_to_message {
            if let Some(ref sender) = reply.from {
                if sender.is_bot {
                    if let Some(bid) = bot_id {
                        if sender.id == bid {
                            return true;
                        }
                    }
                    if let (Some(ref b_user), Some(ref s_user)) = (&bot_username, &sender.username) {
                        if s_user.trim_start_matches('@').eq_ignore_ascii_case(b_user) {
                            return true;
                        }
                    }
                }
            }
        }

        if let Some(ref b_user) = bot_username {
            let mention_needle = format!("@{}", b_user);
            if text.to_lowercase().contains(&mention_needle) {
                return true;
            }

            let all_entities = msg.entities.as_ref().into_iter().flatten()
                .chain(msg.caption_entities.as_ref().into_iter().flatten());

            for ent in all_entities {
                if ent.entity_type == "mention" {
                    if ent.offset + ent.length <= text.len() {
                        let span = &text[ent.offset..ent.offset + ent.length];
                        if span.trim_start_matches('@').eq_ignore_ascii_case(b_user) {
                            return true;
                        }
                    }
                } else if ent.entity_type == "text_mention" {
                    if let Some(ref user) = ent.user {
                        if Some(user.id) == bot_id || user.username.as_ref().map(|u| u.trim_start_matches('@').to_lowercase()).as_deref() == Some(b_user) {
                            return true;
                        }
                    }
                }
            }
        }

        false
    }

    pub async fn convert_incoming_with_cache(&self, update: TelegramUpdate) -> Result<Vec<IncomingMessage>, PlatformError> {
        let mut messages = Vec::new();
        let bot_user = self.bot_info.read().await.clone();

        if let Some(msg) = update.message.or(update.edited_message).or(update.channel_post).or(update.edited_channel_post) {
            if !Self::should_process_message(&msg, bot_user.as_ref()) {
                return Ok(Vec::new());
            }

            let session_id = self.session_id_from_chat(&msg.chat, msg.message_thread_id);
            let content = self.convert_message_content_and_cache(&msg).await?;

            let mut incoming = IncomingMessage {
                id: MessageId::new(),
                session_id,
                platform: Platform::Telegram,
                content,
                timestamp: OffsetDateTime::from_unix_timestamp(msg.date)
                    .unwrap_or_else(|_| OffsetDateTime::now_utc()),
                raw: Some(serde_json::to_value(&msg).unwrap_or(serde_json::Value::Null)),
            };

            if let Some(tid) = msg.message_thread_id {
                let mut raw_val = incoming.raw.take().unwrap_or(serde_json::json!({}));
                if let Some(obj) = raw_val.as_object_mut() {
                    obj.insert("thread_id".to_string(), serde_json::json!(tid));
                }
                incoming.raw = Some(raw_val);
            }

            messages.push(incoming);
        }

        if let Some(callback) = update.callback_query {
            if let Some(ref msg) = callback.message {
                let session_id = self.session_id_from_chat(&msg.chat, msg.message_thread_id);
                let content = MessageContent::System {
                    event: "callback_query".to_string(),
                    data: serde_json::json!({
                        "callback_query_id": callback.id,
                        "callback_data": callback.data,
                        "data": callback.data,
                        "message_id": msg.message_id,
                        "chat_id": msg.chat.id,
                        "message_thread_id": msg.message_thread_id,
                        "from": callback.from,
                    }),
                };

                let incoming = IncomingMessage {
                    id: MessageId::new(),
                    session_id,
                    platform: Platform::Telegram,
                    content,
                    timestamp: OffsetDateTime::now_utc(),
                    raw: Some(serde_json::to_value(&callback).unwrap_or_default()),
                };
                messages.push(incoming);
            }
        }

        Ok(messages)
    }

    pub fn convert_incoming(&self, update: TelegramUpdate) -> Result<Vec<IncomingMessage>, PlatformError> {
        let mut messages = Vec::new();

        if let Some(msg) = update.message.or(update.edited_message).or(update.channel_post).or(update.edited_channel_post) {
            let session_id = self.session_id_from_chat(&msg.chat, msg.message_thread_id);
            let content = self.convert_message_content(&msg)?;

            let mut incoming = IncomingMessage {
                id: MessageId::new(),
                session_id,
                platform: Platform::Telegram,
                content,
                timestamp: OffsetDateTime::from_unix_timestamp(msg.date)
                    .unwrap_or_else(|_| OffsetDateTime::now_utc()),
                raw: Some(serde_json::to_value(&msg).unwrap_or(serde_json::Value::Null)),
            };

            if let Some(tid) = msg.message_thread_id {
                let mut raw_val = incoming.raw.take().unwrap_or(serde_json::json!({}));
                if let Some(obj) = raw_val.as_object_mut() {
                    obj.insert("thread_id".to_string(), serde_json::json!(tid));
                }
                incoming.raw = Some(raw_val);
            }

            messages.push(incoming);
        }

        if let Some(callback) = update.callback_query {
            if let Some(ref msg) = callback.message {
                let session_id = self.session_id_from_chat(&msg.chat, msg.message_thread_id);
                let content = MessageContent::System {
                    event: "callback_query".to_string(),
                    data: serde_json::json!({
                        "callback_query_id": callback.id,
                        "callback_data": callback.data,
                        "data": callback.data,
                        "message_id": msg.message_id,
                        "chat_id": msg.chat.id,
                        "message_thread_id": msg.message_thread_id,
                        "from": callback.from,
                    }),
                };

                let incoming = IncomingMessage {
                    id: MessageId::new(),
                    session_id,
                    platform: Platform::Telegram,
                    content,
                    timestamp: OffsetDateTime::now_utc(),
                    raw: Some(serde_json::to_value(&callback).unwrap_or_default()),
                };
                messages.push(incoming);
            }
        }

        Ok(messages)
    }

    pub fn session_id_from_chat(&self, chat: &TelegramChat, thread_id: Option<i64>) -> Uuid {
        let chat_bytes = chat.id.to_le_bytes();
        let mut uuid_bytes = [0u8; 16];
        uuid_bytes[..8].copy_from_slice(&chat_bytes);

        if let Some(tid) = thread_id {
            let thread_bytes = tid.to_le_bytes();
            uuid_bytes[8..].copy_from_slice(&thread_bytes);
        }

        Uuid::from_bytes(uuid_bytes)
    }

    pub async fn convert_message_content_and_cache(&self, msg: &TelegramMessage) -> Result<MessageContent, PlatformError> {
        if let Some(text) = &msg.text {
            return Ok(MessageContent::Text { text: text.clone() });
        }

        if let Some(caption) = &msg.caption {
            if let Some(photo) = &msg.photo {
                let largest = photo.iter().max_by_key(|p| p.width * p.height);
                if let Some(p) = largest {
                    let cached_url = self.cache_inbound_file(&p.file_id, ".jpg").await.unwrap_or_else(|_| p.file_id.clone());
                    return Ok(MessageContent::Image {
                        url: cached_url,
                        caption: Some(caption.clone()),
                    });
                }
            }
            if let Some(doc) = &msg.document {
                let cached_url = self.cache_inbound_file(&doc.file_id, ".bin").await.unwrap_or_else(|_| doc.file_id.clone());
                return Ok(MessageContent::File {
                    url: cached_url,
                    filename: doc.file_name.clone().unwrap_or_else(|| "document".to_string()),
                    mime_type: doc.mime_type.clone(),
                });
            }
        }

        if let Some(photo) = &msg.photo {
            let largest = photo.iter().max_by_key(|p| p.width * p.height);
            if let Some(p) = largest {
                let cached_url = self.cache_inbound_file(&p.file_id, ".jpg").await.unwrap_or_else(|_| p.file_id.clone());
                return Ok(MessageContent::Image {
                    url: cached_url,
                    caption: msg.caption.clone(),
                });
            }
        }

        if let Some(doc) = &msg.document {
            let cached_url = self.cache_inbound_file(&doc.file_id, ".bin").await.unwrap_or_else(|_| doc.file_id.clone());
            return Ok(MessageContent::File {
                url: cached_url,
                filename: doc.file_name.clone().unwrap_or_else(|| "document".to_string()),
                mime_type: doc.mime_type.clone(),
            });
        }

        if let Some(voice) = &msg.voice {
            let cached_url = self.cache_inbound_file(&voice.file_id, ".ogg").await.unwrap_or_else(|_| voice.file_id.clone());
            return Ok(MessageContent::Audio {
                url: cached_url,
                duration: Some(voice.duration as u64),
            });
        }

        if let Some(audio) = &msg.audio {
            let cached_url = self.cache_inbound_file(&audio.file_id, ".mp3").await.unwrap_or_else(|_| audio.file_id.clone());
            return Ok(MessageContent::Audio {
                url: cached_url,
                duration: Some(audio.duration as u64),
            });
        }

        if let Some(video) = &msg.video {
            let cached_url = self.cache_inbound_file(&video.file_id, ".mp4").await.unwrap_or_else(|_| video.file_id.clone());
            return Ok(MessageContent::Video {
                url: cached_url,
                duration: Some(video.duration as u64),
            });
        }

        if let Some(sticker) = &msg.sticker {
            return Ok(MessageContent::Sticker {
                sticker_id: sticker.file_id.clone(),
            });
        }

        if let Some(location) = &msg.location {
            return Ok(MessageContent::Location {
                latitude: location.latitude,
                longitude: location.longitude,
            });
        }

        if let Some(contact) = &msg.contact {
            return Ok(MessageContent::Contact {
                name: format!("{} {}", contact.first_name, contact.last_name.as_ref().map(|s| s.as_str()).unwrap_or("")).trim().to_string(),
                phone: contact.phone_number.clone(),
            });
        }

        Ok(MessageContent::Unknown { data: serde_json::to_value(msg).unwrap_or_default() })
    }

    fn convert_message_content(&self, msg: &TelegramMessage) -> Result<MessageContent, PlatformError> {
        if let Some(text) = &msg.text {
            return Ok(MessageContent::Text { text: text.clone() });
        }
        if let Some(caption) = &msg.caption {
            if let Some(photo) = &msg.photo {
                let largest = photo.iter().max_by_key(|p| p.width * p.height);
                if let Some(p) = largest {
                    return Ok(MessageContent::Image {
                        url: p.file_id.clone(),
                        caption: Some(caption.clone()),
                    });
                }
            }
            if let Some(doc) = &msg.document {
                return Ok(MessageContent::File {
                    url: doc.file_id.clone(),
                    filename: doc.file_name.clone().unwrap_or_else(|| "document".to_string()),
                    mime_type: doc.mime_type.clone(),
                });
            }
        }
        if let Some(photo) = &msg.photo {
            let largest = photo.iter().max_by_key(|p| p.width * p.height);
            if let Some(p) = largest {
                return Ok(MessageContent::Image {
                    url: p.file_id.clone(),
                    caption: msg.caption.clone(),
                });
            }
        }
        if let Some(doc) = &msg.document {
            return Ok(MessageContent::File {
                url: doc.file_id.clone(),
                filename: doc.file_name.clone().unwrap_or_else(|| "document".to_string()),
                mime_type: doc.mime_type.clone(),
            });
        }
        if let Some(voice) = &msg.voice {
            return Ok(MessageContent::Audio {
                url: voice.file_id.clone(),
                duration: Some(voice.duration as u64),
            });
        }
        if let Some(audio) = &msg.audio {
            return Ok(MessageContent::Audio {
                url: audio.file_id.clone(),
                duration: Some(audio.duration as u64),
            });
        }
        if let Some(video) = &msg.video {
            return Ok(MessageContent::Video {
                url: video.file_id.clone(),
                duration: Some(video.duration as u64),
            });
        }
        if let Some(sticker) = &msg.sticker {
            return Ok(MessageContent::Sticker {
                sticker_id: sticker.file_id.clone(),
            });
        }
        if let Some(location) = &msg.location {
            return Ok(MessageContent::Location {
                latitude: location.latitude,
                longitude: location.longitude,
            });
        }
        if let Some(contact) = &msg.contact {
            return Ok(MessageContent::Contact {
                name: format!("{} {}", contact.first_name, contact.last_name.as_ref().map(|s| s.as_str()).unwrap_or("")).trim().to_string(),
                phone: contact.phone_number.clone(),
            });
        }

        Ok(MessageContent::Unknown { data: serde_json::to_value(msg).unwrap_or_default() })
    }

    #[allow(dead_code)]
    fn convert_outgoing(&self, msg: &OutgoingMessage) -> Result<serde_json::Value, PlatformError> {
        let (chat_id, thread_id) = self.chat_and_thread_from_session(msg.session_id, &msg.metadata)?;

        match &msg.content {
            MessageContent::Text { text } => {
                serde_json::to_value(SendMessageRequest {
                    chat_id,
                    message_thread_id: thread_id,
                    text: text.clone(),
                    parse_mode: Some("HTML".to_string()),
                    reply_to_message_id: msg.metadata.get("reply_to_telegram_id").and_then(|v| v.as_i64()),
                    disable_web_page_preview: Some(true),
                    reply_markup: None,
                }).map_err(|e| PlatformError::Internal(e.to_string()))
            }
            MessageContent::Image { url, caption } => {
                serde_json::to_value(SendPhotoRequest {
                    chat_id,
                    message_thread_id: thread_id,
                    photo: url.clone(),
                    caption: caption.clone(),
                    parse_mode: Some("HTML".to_string()),
                    reply_to_message_id: msg.metadata.get("reply_to_telegram_id").and_then(|v| v.as_i64()),
                }).map_err(|e| PlatformError::Internal(e.to_string()))
            }
            MessageContent::File { url, filename: _, mime_type: _ } => {
                serde_json::to_value(SendDocumentRequest {
                    chat_id,
                    message_thread_id: thread_id,
                    document: url.clone(),
                    caption: None,
                    reply_to_message_id: msg.metadata.get("reply_to_telegram_id").and_then(|v| v.as_i64()),
                }).map_err(|e| PlatformError::Internal(e.to_string()))
            }
            MessageContent::Audio { url, duration } => {
                let is_voice = msg.metadata.get("is_voice").and_then(|v| v.as_bool()).unwrap_or(false);
                if is_voice {
                    serde_json::to_value(SendVoiceRequest {
                        chat_id,
                        message_thread_id: thread_id,
                        voice: url.clone(),
                        caption: msg.metadata.get("caption").and_then(|v| v.as_str()).map(ToString::to_string),
                        parse_mode: Some("HTML".to_string()),
                        duration: duration.map(|d| d as u32),
                        reply_to_message_id: msg.metadata.get("reply_to_telegram_id").and_then(|v| v.as_i64()),
                    }).map_err(|e| PlatformError::Internal(e.to_string()))
                } else {
                    serde_json::to_value(SendAudioRequest {
                        chat_id,
                        message_thread_id: thread_id,
                        audio: url.clone(),
                        caption: msg.metadata.get("caption").and_then(|v| v.as_str()).map(ToString::to_string),
                        parse_mode: None,
                        duration: duration.map(|d| d as u32),
                        performer: msg.metadata.get("performer").and_then(|v| v.as_str()).map(ToString::to_string),
                        title: msg.metadata.get("title").and_then(|v| v.as_str()).map(ToString::to_string),
                        reply_to_message_id: msg.metadata.get("reply_to_telegram_id").and_then(|v| v.as_i64()),
                    }).map_err(|e| PlatformError::Internal(e.to_string()))
                }
            }
            MessageContent::Video { url, duration } => {
                serde_json::to_value(SendVideoRequest {
                    chat_id,
                    message_thread_id: thread_id,
                    video: url.clone(),
                    caption: msg.metadata.get("caption").and_then(|v| v.as_str()).map(ToString::to_string),
                    parse_mode: None,
                    duration: duration.map(|d| d as u32),
                    width: msg.metadata.get("width").and_then(|v| v.as_u64()).map(|w| w as u32),
                    height: msg.metadata.get("height").and_then(|v| v.as_u64()).map(|h| h as u32),
                    reply_to_message_id: msg.metadata.get("reply_to_telegram_id").and_then(|v| v.as_i64()),
                }).map_err(|e| PlatformError::Internal(e.to_string()))
            }
            _ => Err(PlatformError::UnsupportedContent(format!("Content type not supported for Telegram: {:?}", msg.content))),
        }
    }

    pub fn chat_and_thread_from_session(
        &self,
        session_id: Uuid,
        metadata: &HashMap<String, serde_json::Value>,
    ) -> Result<(i64, Option<i64>), PlatformError> {
        let chat_id = self.chat_id_from_session(session_id)?;
        let thread_id = metadata
            .get("thread_id")
            .or_else(|| metadata.get("message_thread_id"))
            .and_then(|v| v.as_i64())
            .or_else(|| self.thread_id_from_session(session_id));
        Ok((chat_id, thread_id))
    }

    pub fn thread_id_from_session(&self, session_id: Uuid) -> Option<i64> {
        let bytes = session_id.as_bytes();
        let thread_id = i64::from_le_bytes(bytes[8..16].try_into().ok()?);
        if thread_id != 0 {
            Some(thread_id)
        } else {
            None
        }
    }

    pub fn chat_id_from_session(&self, session_id: Uuid) -> Result<i64, PlatformError> {
        let bytes = session_id.as_bytes();
        let chat_id = i64::from_le_bytes(bytes[..8].try_into().map_err(|_| PlatformError::Internal("Invalid session ID".into()))?);
        Ok(chat_id)
    }
}

#[async_trait]
impl PlatformAdapter for TelegramAdapter {
    type Config = TelegramConfig;
    type IncomingEvent = IncomingMessage;
    type OutgoingMessage = OutgoingMessage;

    fn platform(&self) -> Platform {
        Platform::Telegram
    }

    async fn initialize(&mut self, config: Self::Config) -> Result<(), PlatformError> {
        self.base = BaseAdapter::new(Platform::Telegram).with_config(config.clone());

        if let Some(rate_limit) = config.base.rate_limit.clone() {
            self.base.setup_rate_limit(Some(rate_limit));
        }

        let me: TelegramUser = self.make_request("getMe", &serde_json::json!({})).await?;
        *self.bot_info.write().await = Some(me.clone());
        info!("Telegram bot initialized: @{} (id: {})", me.username.unwrap_or_default(), me.id);

        if let Some(webhook_url) = &config.webhook_url {
            let webhook_config = config.base.webhook.clone();
            let mut params = serde_json::json!({
                "url": webhook_url,
                "allowed_updates": config.allowed_updates,
                "drop_pending_updates": config.drop_pending_updates,
            });
            if let Some(secret) = webhook_config.and_then(|w| w.secret) {
                params["secret_token"] = serde_json::Value::String(secret);
            }
            let _: serde_json::Value = self.make_request("setWebhook", &params).await?;
            info!("Telegram webhook set: {}", webhook_url);
        } else {
            let _: serde_json::Value = self.make_request("deleteWebhook", &serde_json::json!({"drop_pending_updates": config.drop_pending_updates})).await?;
            info!("Telegram webhook deleted, using long polling");
        }

        Ok(())
    }

    async fn start(&mut self, sender: mpsc::UnboundedSender<PlatformEvent>) -> Result<(), PlatformError> {
        self.base.set_sender(sender);
        self.base.started = true;

        let config = self.base.config.as_ref().expect("Not initialized");

        if config.webhook_url.is_none() {
            let client = self.client.clone();
            let bot_token = config.bot_token.clone();
            let timeout = config.timeout_secs;
            let sender = self.base.sender.clone().expect("Sender not set");
            let adapter_template = Arc::new(TelegramAdapter {
                base: BaseAdapter::new(Platform::Telegram).with_config(config.clone()),
                client: client.clone(),
                bot_info: self.bot_info.clone(),
                last_poll_time: self.last_poll_time.clone(),
                pending_messages: self.pending_messages.clone(),
            });
            let last_poll_time = self.last_poll_time.clone();

            tokio::spawn(async move {
                let mut offset = 0;
                let mut conflict_retry_delay = 5;

                loop {
                    let now = Instant::now();
                    *last_poll_time.write().await = now;

                    let params = serde_json::json!({
                        "offset": offset,
                        "timeout": timeout,
                        "allowed_updates": ["message", "edited_message", "channel_post", "edited_channel_post", "callback_query"],
                    });

                    let url = format!("https://api.telegram.org/bot{}/getUpdates", bot_token);

                    match client
                        .post(&url)
                        .json(&params)
                        .timeout(std::time::Duration::from_secs(timeout + 15))
                        .send()
                        .await
                    {
                        Ok(response) => {
                            let status = response.status();
                            let resp_text = response.text().await.unwrap_or_default();

                            match serde_json::from_str::<TelegramResponse<Vec<TelegramUpdate>>>(&resp_text) {
                                Ok(tg_response) => {
                                    if tg_response.ok {
                                        conflict_retry_delay = 5;
                                        if let Some(updates) = tg_response.result {
                                            for update in updates {
                                                offset = update.update_id + 1;

                                                match adapter_template.convert_incoming_with_cache(update).await {
                                                    Ok(messages) => {
                                                        for msg in messages {
                                                            let thread_id = msg.raw.as_ref()
                                                                .and_then(|r| r.get("thread_id"))
                                                                .and_then(|v| v.as_i64());

                                                            if let Ok(chat_id) = adapter_template.chat_id_from_session(msg.session_id) {
                                                                let client_clone = client.clone();
                                                                let bot_token_clone = bot_token.clone();
                                                                tokio::spawn(async move {
                                                                    let action_url = format!("https://api.telegram.org/bot{}/sendChatAction", bot_token_clone);
                                                                    let mut body = serde_json::json!({"chat_id": chat_id, "action": "typing"});
                                                                    if let Some(tid) = thread_id {
                                                                        body["message_thread_id"] = serde_json::json!(tid);
                                                                    }
                                                                    let _ = client_clone
                                                                        .post(&action_url)
                                                                        .json(&body)
                                                                        .timeout(std::time::Duration::from_secs(10))
                                                                        .send()
                                                                        .await;
                                                                });
                                                            }

                                                            let _ = sender.send(PlatformEvent::Incoming(msg));
                                                        }
                                                    }
                                                    Err(e) => {
                                                        warn!("[telegram] Error converting incoming update: {:?}", e);
                                                    }
                                                }
                                            }
                                        }
                                    } else {
                                        let desc = tg_response.description.unwrap_or_default();
                                        let status_code = tg_response.error_code.unwrap_or(status.as_u16() as i32) as u16;

                                        if looks_like_polling_conflict(status_code, &desc) {
                                            warn!(
                                                "[telegram] Long-polling conflict detected (HTTP {} / {}). Backing off for {}s before retry...",
                                                status_code, desc, conflict_retry_delay
                                            );
                                            tokio::time::sleep(std::time::Duration::from_secs(conflict_retry_delay)).await;
                                            conflict_retry_delay = std::cmp::min(conflict_retry_delay * 2, 60);
                                        } else {
                                            warn!("[telegram] Telegram API error in getUpdates: HTTP {} - {}", status_code, desc);
                                            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                                        }
                                    }
                                }
                                Err(e) => {
                                    warn!("[telegram] Failed to parse getUpdates response (status {}): {}. Raw: {}", status, e, resp_text);
                                    if status.as_u16() == 409 || looks_like_polling_conflict(status.as_u16(), &resp_text) {
                                        warn!("[telegram] 409 Conflict indicated by HTTP status. Backing off {}s", conflict_retry_delay);
                                        tokio::time::sleep(std::time::Duration::from_secs(conflict_retry_delay)).await;
                                        conflict_retry_delay = std::cmp::min(conflict_retry_delay * 2, 60);
                                    } else {
                                        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            let stalled_dur = Instant::now().duration_since(*last_poll_time.read().await);
                            if stalled_dur > std::time::Duration::from_secs(timeout + 30) {
                                warn!("[telegram] Polling stall watchdog triggered: request exceeded timeout+30s ({:?}). Backing off and restarting connection...", stalled_dur);
                            } else {
                                error!("[telegram] Telegram long polling network error: {}", e);
                            }
                            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                        }
                    }
                }
            });
        }

        info!("Telegram adapter started");
        Ok(())
    }

    async fn stop(&mut self) -> Result<(), PlatformError> {
        self.base.started = false;

        if let Some(config) = &self.base.config {
            if config.webhook_url.is_some() {
                let _: serde_json::Value = self.make_request("deleteWebhook", &serde_json::json!({})).await?;
            }
        }

        info!("Telegram adapter stopped");
        Ok(())
    }

    async fn send(&self, msg: Self::OutgoingMessage) -> Result<DeliveryReceipt, PlatformError> {
        if let Some(limiter) = &self.base.rate_limiter {
            limiter.acquire().await?;
        }

        let (chat_id, thread_id) = self.chat_and_thread_from_session(msg.session_id, &msg.metadata)?;

        match &msg.content {
            MessageContent::Text { text } => {
                let chunks = chunk_message(text, 4000);
                let reply_to_id = msg.metadata.get("reply_to_telegram_id").and_then(|v| v.as_i64());
                let mut last_message_id = None;

                for (idx, chunk) in chunks.iter().enumerate() {
                    let chunk_reply_to = if idx == 0 { reply_to_id } else { None };
                    let sent = self.send_chunk_with_fallback_and_thread(chat_id, thread_id, chunk, chunk_reply_to).await?;
                    last_message_id = Some(sent.message_id);
                }

                Ok(DeliveryReceipt {
                    message_id: msg.id,
                    platform: Platform::Telegram,
                    status: DeliveryStatus::Sent,
                    timestamp: OffsetDateTime::now_utc(),
                    platform_message_id: last_message_id.map(|id| id.to_string()),
                })
            }
            MessageContent::Image { url, caption } => {
                let reply_to_id = msg.metadata.get("reply_to_telegram_id").and_then(|v| v.as_i64());
                let sent: SentMessage = if let Some(stripped_path) = url.strip_prefix("file://") {
                    self.send_photo_file(chat_id, thread_id, Path::new(stripped_path), caption.as_deref(), reply_to_id).await?
                } else if Path::new(url).exists() {
                    self.send_photo_file(chat_id, thread_id, Path::new(url), caption.as_deref(), reply_to_id).await?
                } else {
                    let req = SendPhotoRequest {
                        chat_id,
                        message_thread_id: thread_id,
                        photo: url.clone(),
                        caption: caption.clone(),
                        parse_mode: Some("HTML".to_string()),
                        reply_to_message_id: reply_to_id,
                    };

                    match self.make_request("sendPhoto", &req).await {
                        Ok(sent) => sent,
                        Err(PlatformError::Api { code, message }) => {
                            let status_code = code.parse::<u16>().unwrap_or(0);
                            if looks_like_parse_error(status_code, &message) {
                                warn!("[telegram] sendPhoto HTML caption failed, retrying without parse_mode");
                                let plain_caption = caption.as_deref().map(strip_html_tags);
                                let plain_req = SendPhotoRequest {
                                    chat_id,
                                    message_thread_id: thread_id,
                                    photo: url.clone(),
                                    caption: plain_caption,
                                    parse_mode: None,
                                    reply_to_message_id: reply_to_id,
                                };
                                self.make_request("sendPhoto", &plain_req).await?
                            } else {
                                return Err(PlatformError::Api { code, message });
                            }
                        }
                        Err(other) => return Err(other),
                    }
                };

                Ok(DeliveryReceipt {
                    message_id: msg.id,
                    platform: Platform::Telegram,
                    status: DeliveryStatus::Sent,
                    timestamp: OffsetDateTime::now_utc(),
                    platform_message_id: Some(sent.message_id.to_string()),
                })
            }
            MessageContent::File { url, .. } => {
                let req = SendDocumentRequest {
                    chat_id,
                    message_thread_id: thread_id,
                    document: url.clone(),
                    caption: None,
                    reply_to_message_id: msg.metadata.get("reply_to_telegram_id").and_then(|v| v.as_i64()),
                };
                let sent: SentMessage = self.make_request("sendDocument", &req).await?;

                Ok(DeliveryReceipt {
                    message_id: msg.id,
                    platform: Platform::Telegram,
                    status: DeliveryStatus::Sent,
                    timestamp: OffsetDateTime::now_utc(),
                    platform_message_id: Some(sent.message_id.to_string()),
                })
            }
            MessageContent::Audio { url, duration } => {
                let is_voice = msg.metadata.get("is_voice").and_then(|v| v.as_bool()).unwrap_or(false);
                let caption = msg.metadata.get("caption").and_then(|v| v.as_str());
                let reply_to_id = msg.metadata.get("reply_to_telegram_id").and_then(|v| v.as_i64());

                let sent: SentMessage = if is_voice {
                    if let Some(stripped_path) = url.strip_prefix("file://") {
                        self.send_voice(chat_id, thread_id, Path::new(stripped_path), caption, duration.map(|d| d as u32)).await?
                    } else if Path::new(url).exists() {
                        self.send_voice(chat_id, thread_id, Path::new(url), caption, duration.map(|d| d as u32)).await?
                    } else {
                        let req = SendVoiceRequest {
                            chat_id,
                            message_thread_id: thread_id,
                            voice: url.clone(),
                            caption: truncate_caption_1024(caption),
                            parse_mode: Some("HTML".to_string()),
                            duration: duration.map(|d| d as u32),
                            reply_to_message_id: reply_to_id,
                        };
                        match self.make_request("sendVoice", &req).await {
                            Ok(sent) => sent,
                            Err(PlatformError::Api { code, message }) => {
                                let status_code = code.parse::<u16>().unwrap_or(0);
                                if looks_like_parse_error(status_code, &message) {
                                    warn!("[telegram] sendVoice HTML caption failed, retrying plain");
                                    let plain_caption = caption.as_deref().map(strip_html_tags);
                                    let plain_req = SendVoiceRequest {
                                        chat_id,
                                        message_thread_id: thread_id,
                                        voice: url.clone(),
                                        caption: truncate_caption_1024(plain_caption.as_deref()),
                                        parse_mode: None,
                                        duration: duration.map(|d| d as u32),
                                        reply_to_message_id: reply_to_id,
                                    };
                                    self.make_request("sendVoice", &plain_req).await?
                                } else {
                                    return Err(PlatformError::Api { code, message });
                                }
                            }
                            Err(other) => return Err(other),
                        }
                    }
                } else {
                    if let Some(stripped_path) = url.strip_prefix("file://") {
                        let title = msg.metadata.get("title").and_then(|v| v.as_str());
                        let performer = msg.metadata.get("performer").and_then(|v| v.as_str());
                        self.send_audio(chat_id, thread_id, Path::new(stripped_path), caption, duration.map(|d| d as u32), title, performer).await?
                    } else if Path::new(url).exists() {
                        let title = msg.metadata.get("title").and_then(|v| v.as_str());
                        let performer = msg.metadata.get("performer").and_then(|v| v.as_str());
                        self.send_audio(chat_id, thread_id, Path::new(url), caption, duration.map(|d| d as u32), title, performer).await?
                    } else {
                        let req = SendAudioRequest {
                            chat_id,
                            message_thread_id: thread_id,
                            audio: url.clone(),
                            caption: truncate_caption_1024(caption),
                            parse_mode: None,
                            duration: duration.map(|d| d as u32),
                            performer: msg.metadata.get("performer").and_then(|v| v.as_str()).map(ToString::to_string),
                            title: msg.metadata.get("title").and_then(|v| v.as_str()).map(ToString::to_string),
                            reply_to_message_id: reply_to_id,
                        };
                        self.make_request("sendAudio", &req).await?
                    }
                };

                Ok(DeliveryReceipt {
                    message_id: msg.id,
                    platform: Platform::Telegram,
                    status: DeliveryStatus::Sent,
                    timestamp: OffsetDateTime::now_utc(),
                    platform_message_id: Some(sent.message_id.to_string()),
                })
            }
            MessageContent::Video { url, duration } => {
                let caption = msg.metadata.get("caption").and_then(|v| v.as_str());
                let reply_to_id = msg.metadata.get("reply_to_telegram_id").and_then(|v| v.as_i64());
                let width = msg.metadata.get("width").and_then(|v| v.as_u64()).map(|w| w as u32);
                let height = msg.metadata.get("height").and_then(|v| v.as_u64()).map(|h| h as u32);

                let sent: SentMessage = if let Some(stripped_path) = url.strip_prefix("file://") {
                    self.send_video(chat_id, thread_id, Path::new(stripped_path), caption, duration.map(|d| d as u32), width, height).await?
                } else if Path::new(url).exists() {
                    self.send_video(chat_id, thread_id, Path::new(url), caption, duration.map(|d| d as u32), width, height).await?
                } else {
                    let req = SendVideoRequest {
                        chat_id,
                        message_thread_id: thread_id,
                        video: url.clone(),
                        caption: truncate_caption_1024(caption),
                        parse_mode: None,
                        duration: duration.map(|d| d as u32),
                        width,
                        height,
                        reply_to_message_id: reply_to_id,
                    };
                    self.make_request("sendVideo", &req).await?
                };

                Ok(DeliveryReceipt {
                    message_id: msg.id,
                    platform: Platform::Telegram,
                    status: DeliveryStatus::Sent,
                    timestamp: OffsetDateTime::now_utc(),
                    platform_message_id: Some(sent.message_id.to_string()),
                })
            }
            _ => Err(PlatformError::UnsupportedContent("Unsupported content type".into())),
        }
    }

    async fn handle_webhook(&self, payload: serde_json::Value) -> Result<Vec<IncomingMessage>, PlatformError> {
        let update: TelegramUpdate = serde_json::from_value(payload)
            .map_err(|e| PlatformError::Internal(e.to_string()))?;
        self.convert_incoming(update)
    }

    async fn health_check(&self) -> Result<(), PlatformError> {
        let _: TelegramUser = self.make_request("getMe", &serde_json::json!({})).await?;
        Ok(())
    }
}

impl Default for TelegramAdapter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_forum_topic_session_id_determinism_and_uniqueness() {
        let adapter = TelegramAdapter::new();
        let chat = TelegramChat {
            id: -1001234567890,
            chat_type: "supergroup".to_string(),
            title: Some("Test Forum".to_string()),
            username: None,
            first_name: None,
            last_name: None,
            is_forum: Some(true),
        };

        // Same thread produces identical deterministic UUID
        let uuid_t10_a = adapter.session_id_from_chat(&chat, Some(10));
        let uuid_t10_b = adapter.session_id_from_chat(&chat, Some(10));
        assert_eq!(uuid_t10_a, uuid_t10_b);

        // Different threads produce different UUIDs
        let uuid_t20 = adapter.session_id_from_chat(&chat, Some(20));
        assert_ne!(uuid_t10_a, uuid_t20);

        // None thread produces different UUID from thread 10
        let uuid_none = adapter.session_id_from_chat(&chat, None);
        assert_ne!(uuid_t10_a, uuid_none);

        // Verification of chat_id and thread_id recovery from UUID
        assert_eq!(adapter.chat_id_from_session(uuid_t10_a).unwrap(), -1001234567890);
        assert_eq!(adapter.thread_id_from_session(uuid_t10_a), Some(10));
        assert_eq!(adapter.thread_id_from_session(uuid_t20), Some(20));
        assert_eq!(adapter.thread_id_from_session(uuid_none), None);
    }

    #[test]
    fn test_group_mention_filtering() {
        let bot = TelegramUser {
            id: 123456,
            is_bot: true,
            first_name: "Hermes Bot".to_string(),
            last_name: None,
            username: Some("HermesBot".to_string()),
            language_code: None,
        };

        let group_chat = TelegramChat {
            id: -100987654321,
            chat_type: "supergroup".to_string(),
            title: Some("Devs Group".to_string()),
            username: None,
            first_name: None,
            last_name: None,
            is_forum: None,
        };

        let private_chat = TelegramChat {
            id: 555666777,
            chat_type: "private".to_string(),
            title: None,
            username: Some("alice".to_string()),
            first_name: Some("Alice".to_string()),
            last_name: None,
            is_forum: None,
        };

        // 1. Private DM message -> always accepted
        let dm_msg = TelegramMessage {
            message_id: 1,
            from: None,
            chat: private_chat,
            date: 1700000000,
            message_thread_id: None,
            is_topic_message: None,
            text: Some("hello bot in dm".to_string()),
            entities: None,
            caption: None,
            caption_entities: None,
            photo: None,
            document: None,
            audio: None,
            voice: None,
            video: None,
            sticker: None,
            location: None,
            contact: None,
            reply_to_message: None,
        };
        assert!(TelegramAdapter::should_process_message(&dm_msg, Some(&bot)));

        // 2. Group message without mention -> ignored
        let group_plain_msg = TelegramMessage {
            message_id: 2,
            from: None,
            chat: group_chat.clone(),
            date: 1700000000,
            message_thread_id: None,
            is_topic_message: None,
            text: Some("random group chatter".to_string()),
            entities: None,
            caption: None,
            caption_entities: None,
            photo: None,
            document: None,
            audio: None,
            voice: None,
            video: None,
            sticker: None,
            location: None,
            contact: None,
            reply_to_message: None,
        };
        assert!(!TelegramAdapter::should_process_message(&group_plain_msg, Some(&bot)));

        // 3. Group command (/status) -> accepted
        let group_cmd_msg = TelegramMessage {
            message_id: 3,
            from: None,
            chat: group_chat.clone(),
            date: 1700000000,
            message_thread_id: None,
            is_topic_message: None,
            text: Some("/status".to_string()),
            entities: None,
            caption: None,
            caption_entities: None,
            photo: None,
            document: None,
            audio: None,
            voice: None,
            video: None,
            sticker: None,
            location: None,
            contact: None,
            reply_to_message: None,
        };
        assert!(TelegramAdapter::should_process_message(&group_cmd_msg, Some(&bot)));

        // 4. Group message mentioning @HermesBot -> accepted
        let group_mention_msg = TelegramMessage {
            message_id: 4,
            from: None,
            chat: group_chat.clone(),
            date: 1700000000,
            message_thread_id: None,
            is_topic_message: None,
            text: Some("Hey @HermesBot please help".to_string()),
            entities: Some(vec![TelegramMessageEntity {
                entity_type: "mention".to_string(),
                offset: 4,
                length: 10,
                url: None,
                user: None,
                language: None,
                custom_emoji_id: None,
            }]),
            caption: None,
            caption_entities: None,
            photo: None,
            document: None,
            audio: None,
            voice: None,
            video: None,
            sticker: None,
            location: None,
            contact: None,
            reply_to_message: None,
        };
        assert!(TelegramAdapter::should_process_message(&group_mention_msg, Some(&bot)));

        // 5. Group message replying to bot -> accepted
        let reply_to_bot_msg = TelegramMessage {
            message_id: 5,
            from: None,
            chat: group_chat.clone(),
            date: 1700000000,
            message_thread_id: None,
            is_topic_message: None,
            text: Some("Yes I agree".to_string()),
            entities: None,
            caption: None,
            caption_entities: None,
            photo: None,
            document: None,
            audio: None,
            voice: None,
            video: None,
            sticker: None,
            location: None,
            contact: None,
            reply_to_message: Some(Box::new(TelegramMessage {
                message_id: 100,
                from: Some(bot.clone()),
                chat: group_chat.clone(),
                date: 1700000000,
                message_thread_id: None,
                is_topic_message: None,
                text: Some("I am bot response".to_string()),
                entities: None,
                caption: None,
                caption_entities: None,
                photo: None,
                document: None,
                audio: None,
                voice: None,
                video: None,
                sticker: None,
                location: None,
                contact: None,
                reply_to_message: None,
            })),
        };
        assert!(TelegramAdapter::should_process_message(&reply_to_bot_msg, Some(&bot)));
    }

    #[test]
    fn test_telegram_file_deserialization_and_download_url() {
        let json = r#"{
            "file_id": "BAACAgIAAxkBAAIBYw...",
            "file_unique_id": "AQADu7cxG...",
            "file_size": 12345,
            "file_path": "photos/file_42.jpg"
        }"#;

        let file: TelegramFile = serde_json::from_str(json).expect("Deserialize TelegramFile");
        assert_eq!(file.file_id, "BAACAgIAAxkBAAIBYw...");
        assert_eq!(file.file_path.as_deref(), Some("photos/file_42.jpg"));
        assert_eq!(file.file_size, Some(12345));

        let mut adapter = TelegramAdapter::new();
        adapter.base.config = Some(TelegramConfig {
            bot_token: "123456:ABC-DEF1234ghIkl-zyx57W2v1u123ew11".to_string(),
            webhook_url: None,
            allowed_updates: vec![],
            drop_pending_updates: false,
            timeout_secs: 30,
            base: BasePlatformConfig {
                enabled: true,
                rate_limit: None,
                webhook: None,
                custom: serde_json::Value::Null,
            },
        });

        let download_url = adapter.download_url(file.file_path.as_deref().unwrap()).unwrap();
        assert_eq!(
            download_url,
            "https://api.telegram.org/file/bot123456:ABC-DEF1234ghIkl-zyx57W2v1u123ew11/photos/file_42.jpg"
        );
    }

    #[test]
    fn test_inline_keyboard_serialization() {
        let btn1 = InlineKeyboardButton::new("⭐ 9Router Gateway", "mp:9router");
        let btn2 = InlineKeyboardButton::new("OpenRouter", "mp:openrouter");
        let cancel_btn = InlineKeyboardButton::new("✗ Cancel", "mx:cancel");

        let markup = InlineKeyboardMarkup::new(vec![
            vec![btn1, btn2],
            vec![cancel_btn],
        ]);

        let json = serde_json::to_string(&markup).expect("Failed to serialize markup");
        let val: serde_json::Value = serde_json::from_str(&json).expect("Failed to parse json");

        let keyboard = val["inline_keyboard"].as_array().expect("Must have inline_keyboard array");
        assert_eq!(keyboard.len(), 2);
        assert_eq!(keyboard[0][0]["text"], "⭐ 9Router Gateway");
        assert_eq!(keyboard[0][0]["callback_data"], "mp:9router");
        assert_eq!(keyboard[0][1]["text"], "OpenRouter");
        assert_eq!(keyboard[0][1]["callback_data"], "mp:openrouter");
        assert_eq!(keyboard[1][0]["text"], "✗ Cancel");
        assert_eq!(keyboard[1][0]["callback_data"], "mx:cancel");
    }

    #[test]
    fn test_edit_message_text_request_serialization() {
        let req = EditMessageTextRequest {
            chat_id: 123456789,
            message_id: 42,
            text: "<b>Hello World</b>".to_string(),
            parse_mode: Some("HTML".to_string()),
            disable_web_page_preview: Some(true),
            reply_markup: None,
        };

        let json = serde_json::to_string(&req).expect("Failed to serialize");
        let val: serde_json::Value = serde_json::from_str(&json).expect("Failed to parse json");

        assert_eq!(val["chat_id"], 123456789);
        assert_eq!(val["message_id"], 42);
        assert_eq!(val["text"], "<b>Hello World</b>");
        assert_eq!(val["parse_mode"], "HTML");
        assert_eq!(val["disable_web_page_preview"], true);

        // Test optional fields omitted when None
        let req_none = EditMessageTextRequest {
            chat_id: 987654321,
            message_id: 100,
            text: "Plain message".to_string(),
            parse_mode: None,
            disable_web_page_preview: None,
            reply_markup: None,
        };
        let json_none = serde_json::to_string(&req_none).expect("Failed to serialize");
        let val_none: serde_json::Value = serde_json::from_str(&json_none).expect("Failed to parse json");

        assert_eq!(val_none["chat_id"], 987654321);
        assert_eq!(val_none["message_id"], 100);
        assert_eq!(val_none["text"], "Plain message");
        assert!(val_none.get("parse_mode").is_none());
        assert!(val_none.get("disable_web_page_preview").is_none());
    }

    #[test]
    fn test_truncate_caption_1024() {
        assert_eq!(truncate_caption_1024(None), None);
        assert_eq!(truncate_caption_1024(Some("hello")), Some("hello".to_string()));

        let exactly_1024 = "a".repeat(1024);
        assert_eq!(truncate_caption_1024(Some(&exactly_1024)), Some(exactly_1024.clone()));

        let over_1024 = "a".repeat(1500);
        let truncated = truncate_caption_1024(Some(&over_1024)).unwrap();
        assert_eq!(truncated.len(), 1024);
        assert_eq!(truncated, exactly_1024);

        // UTF-16 characters (e.g. emoji which take 2 UTF-16 code units)
        let emojis = "🎉".repeat(600); // 600 * 2 = 1200 UTF-16 code units
        let truncated_emojis = truncate_caption_1024(Some(&emojis)).unwrap();
        assert_eq!(truncated_emojis.encode_utf16().count(), 1024);
        assert_eq!(truncated_emojis.chars().count(), 512); // exactly 512 emojis
    }

    #[test]
    fn test_send_voice_request_serialization() {
        let req = SendVoiceRequest {
            chat_id: 123456789,
            message_thread_id: Some(42),
            voice: "file_id_voice_123".to_string(),
            caption: Some("<b>Voice message</b>".to_string()),
            parse_mode: Some("HTML".to_string()),
            duration: Some(15),
            reply_to_message_id: Some(10),
        };

        let json = serde_json::to_string(&req).expect("Failed to serialize SendVoiceRequest");
        let val: serde_json::Value = serde_json::from_str(&json).expect("Failed to parse json");

        assert_eq!(val["chat_id"], 123456789);
        assert_eq!(val["message_thread_id"], 42);
        assert_eq!(val["voice"], "file_id_voice_123");
        assert_eq!(val["caption"], "<b>Voice message</b>");
        assert_eq!(val["parse_mode"], "HTML");
        assert_eq!(val["duration"], 15);
        assert_eq!(val["reply_to_message_id"], 10);

        // Test optional fields omitted when None
        let req_minimal = SendVoiceRequest {
            chat_id: 987654321,
            message_thread_id: None,
            voice: "voice_min".to_string(),
            caption: None,
            parse_mode: None,
            duration: None,
            reply_to_message_id: None,
        };
        let json_min = serde_json::to_string(&req_minimal).unwrap();
        let val_min: serde_json::Value = serde_json::from_str(&json_min).unwrap();
        assert_eq!(val_min["chat_id"], 987654321);
        assert_eq!(val_min["voice"], "voice_min");
        assert!(val_min.get("message_thread_id").is_none());
        assert!(val_min.get("caption").is_none());
        assert!(val_min.get("parse_mode").is_none());
        assert!(val_min.get("duration").is_none());
        assert!(val_min.get("reply_to_message_id").is_none());
    }

    #[test]
    fn test_send_audio_request_serialization() {
        let req = SendAudioRequest {
            chat_id: 123456789,
            message_thread_id: Some(7),
            audio: "https://example.com/audio.mp3".to_string(),
            caption: Some("Nice song".to_string()),
            parse_mode: None,
            duration: Some(180),
            performer: Some("Artist".to_string()),
            title: Some("Song Title".to_string()),
            reply_to_message_id: None,
        };

        let json = serde_json::to_string(&req).expect("Failed to serialize SendAudioRequest");
        let val: serde_json::Value = serde_json::from_str(&json).expect("Failed to parse json");

        assert_eq!(val["chat_id"], 123456789);
        assert_eq!(val["message_thread_id"], 7);
        assert_eq!(val["audio"], "https://example.com/audio.mp3");
        assert_eq!(val["caption"], "Nice song");
        assert_eq!(val["duration"], 180);
        assert_eq!(val["performer"], "Artist");
        assert_eq!(val["title"], "Song Title");
        assert!(val.get("parse_mode").is_none());
        assert!(val.get("reply_to_message_id").is_none());
    }

    #[test]
    fn test_send_video_request_serialization() {
        let req = SendVideoRequest {
            chat_id: 123456789,
            message_thread_id: Some(101),
            video: "video_file_id".to_string(),
            caption: Some("Demo clip".to_string()),
            parse_mode: None,
            duration: Some(60),
            width: Some(1920),
            height: Some(1080),
            reply_to_message_id: Some(50),
        };

        let json = serde_json::to_string(&req).expect("Failed to serialize SendVideoRequest");
        let val: serde_json::Value = serde_json::from_str(&json).expect("Failed to parse json");

        assert_eq!(val["chat_id"], 123456789);
        assert_eq!(val["message_thread_id"], 101);
        assert_eq!(val["video"], "video_file_id");
        assert_eq!(val["caption"], "Demo clip");
        assert_eq!(val["duration"], 60);
        assert_eq!(val["width"], 1920);
        assert_eq!(val["height"], 1080);
        assert_eq!(val["reply_to_message_id"], 50);
    }

    #[test]
    fn test_api_url_endpoints() {
        let mut adapter = TelegramAdapter::new();
        adapter.base.config = Some(TelegramConfig {
            bot_token: "123456:TEST_TOKEN".to_string(),
            webhook_url: None,
            allowed_updates: vec![],
            drop_pending_updates: false,
            timeout_secs: 30,
            base: BasePlatformConfig {
                enabled: true,
                rate_limit: None,
                webhook: None,
                custom: serde_json::Value::Null,
            },
        });

        assert_eq!(adapter.api_url("sendVoice"), "https://api.telegram.org/bot123456:TEST_TOKEN/sendVoice");
        assert_eq!(adapter.api_url("sendAudio"), "https://api.telegram.org/bot123456:TEST_TOKEN/sendAudio");
        assert_eq!(adapter.api_url("sendVideo"), "https://api.telegram.org/bot123456:TEST_TOKEN/sendVideo");
    }

    #[test]
    fn test_voice_caption_html_parse_error_fallback_logic() {
        // Verify that strip_html_tags and truncate work as expected for fallback
        let complex_caption = "<b>Bold title</b> with <a href=\"https://example.com\">link</a> & <tag>broken";
        let truncated = truncate_caption_1024(Some(complex_caption)).unwrap();
        assert_eq!(truncated, complex_caption);

        let stripped = strip_html_tags(&truncated);
        assert_eq!(stripped, "Bold title with link & broken");
        assert_eq!(truncate_caption_1024(Some(&stripped)), Some("Bold title with link & broken".to_string()));

        // Check error classification
        assert!(looks_like_parse_error(400, "Bad Request: can't parse entities: Unexpected end tag"));
        assert!(looks_like_parse_error(400, "Bad Request: tag 'tag' is not supported"));
    }

    #[test]
    fn test_looks_like_message_not_modified() {
        assert!(looks_like_message_not_modified("Bad Request: message is not modified: specified new message content and reply markup are exactly the same as a current content and reply markup of the message"));
        assert!(looks_like_message_not_modified("Error: Message is not modified"));
        assert!(looks_like_message_not_modified("NOT MODIFIED"));
        assert!(looks_like_message_not_modified("not modified"));
        assert!(!looks_like_message_not_modified("Bad Request: chat not found"));
        assert!(!looks_like_message_not_modified("Bad Request: message to edit not found"));
    }
}
