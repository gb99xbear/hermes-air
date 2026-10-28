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
use std::sync::Arc;
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
struct TelegramUser {
    id: i64,
    is_bot: bool,
    first_name: String,
    last_name: Option<String>,
    username: Option<String>,
    language_code: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TelegramChat {
    id: i64,
    #[serde(rename = "type")]
    chat_type: String,
    title: Option<String>,
    username: Option<String>,
    first_name: Option<String>,
    last_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TelegramMessage {
    message_id: i64,
    from: Option<TelegramUser>,
    chat: TelegramChat,
    date: i64,
    text: Option<String>,
    caption: Option<String>,
    photo: Option<Vec<TelegramPhotoSize>>,
    document: Option<TelegramDocument>,
    audio: Option<TelegramAudio>,
    video: Option<TelegramVideo>,
    sticker: Option<TelegramSticker>,
    location: Option<TelegramLocation>,
    contact: Option<TelegramContact>,
    reply_to_message: Option<Box<TelegramMessage>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TelegramPhotoSize {
    file_id: String,
    file_unique_id: String,
    width: i32,
    height: i32,
    file_size: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TelegramDocument {
    file_id: String,
    file_unique_id: String,
    thumbnail: Option<TelegramPhotoSize>,
    file_name: Option<String>,
    mime_type: Option<String>,
    file_size: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TelegramAudio {
    file_id: String,
    file_unique_id: String,
    duration: i64,
    performer: Option<String>,
    title: Option<String>,
    mime_type: Option<String>,
    file_size: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TelegramVideo {
    file_id: String,
    file_unique_id: String,
    width: i32,
    height: i32,
    duration: i64,
    thumbnail: Option<TelegramPhotoSize>,
    mime_type: Option<String>,
    file_size: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TelegramSticker {
    file_id: String,
    file_unique_id: String,
    width: i32,
    height: i32,
    is_animated: bool,
    is_video: bool,
    thumbnail: Option<TelegramPhotoSize>,
    emoji: Option<String>,
    set_name: Option<String>,
    file_size: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TelegramLocation {
    longitude: f64,
    latitude: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TelegramContact {
    phone_number: String,
    first_name: String,
    last_name: Option<String>,
    user_id: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TelegramUpdate {
    update_id: i64,
    message: Option<TelegramMessage>,
    edited_message: Option<TelegramMessage>,
    channel_post: Option<TelegramMessage>,
    edited_channel_post: Option<TelegramMessage>,
    callback_query: Option<TelegramCallbackQuery>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TelegramCallbackQuery {
    id: String,
    from: TelegramUser,
    message: Option<TelegramMessage>,
    inline_message_id: Option<String>,
    chat_instance: String,
    data: Option<String>,
    game_short_name: Option<String>,
}

#[derive(Debug, Serialize)]
struct SendMessageRequest {
    chat_id: i64,
    text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    parse_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reply_to_message_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    disable_web_page_preview: Option<bool>,
}

#[derive(Debug, Serialize)]
struct SendPhotoRequest {
    chat_id: i64,
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
    document: String,
    caption: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reply_to_message_id: Option<i64>,
}

#[derive(Debug, Serialize)]
struct SendChatActionRequest<'a> {
    chat_id: i64,
    action: &'a str,
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
struct SentMessage {
    message_id: i64,
    date: i64,
    chat: TelegramChat,
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
    bot_info: Arc<RwLock<Option<TelegramUser>>>,
    #[allow(dead_code)]
    pending_messages: Arc<RwLock<HashMap<MessageId, (OutgoingMessage, mpsc::UnboundedSender<DeliveryReceipt>)>>>,
}

impl TelegramAdapter {
    pub fn new() -> Self {
        Self {
            base: BaseAdapter::new(Platform::Telegram),
            client: Client::new(),
            bot_info: Arc::new(RwLock::new(None)),
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

    pub async fn send_chat_action(&self, chat_id: i64, action: &str) -> Result<(), PlatformError> {
        let req = SendChatActionRequest { chat_id, action };
        let _: serde_json::Value = self.make_request("sendChatAction", &req).await?;
        Ok(())
    }

    async fn send_chunk_with_fallback(
        &self,
        chat_id: i64,
        chunk: &str,
        reply_to_message_id: Option<i64>,
    ) -> Result<SentMessage, PlatformError> {
        let html_req = SendMessageRequest {
            chat_id,
            text: chunk.to_string(),
            parse_mode: Some("HTML".to_string()),
            reply_to_message_id,
            disable_web_page_preview: Some(true),
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
                        text: plain_text,
                        parse_mode: None,
                        reply_to_message_id,
                        disable_web_page_preview: Some(true),
                    };
                    self.make_request::<SentMessage>("sendMessage", &plain_req).await
                } else {
                    Err(PlatformError::Api { code, message })
                }
            }
            Err(other_err) => Err(other_err),
        }
    }

    fn convert_incoming(&self, update: TelegramUpdate) -> Result<Vec<IncomingMessage>, PlatformError> {
        let mut messages = Vec::new();

        if let Some(msg) = update.message.or(update.edited_message).or(update.channel_post).or(update.edited_channel_post) {
            let session_id = self.session_id_from_chat(&msg.chat);
            let content = self.convert_message_content(&msg)?;

            let incoming = IncomingMessage {
                id: MessageId::new(),
                session_id,
                platform: Platform::Telegram,
                content,
                timestamp: OffsetDateTime::from_unix_timestamp(msg.date)
                    .unwrap_or_else(|_| OffsetDateTime::now_utc()),
                raw: Some(serde_json::to_value(&msg).unwrap_or(serde_json::Value::Null)),
            };
            messages.push(incoming);
        }

        if let Some(callback) = update.callback_query {
            if let Some(ref msg) = callback.message {
                let session_id = self.session_id_from_chat(&msg.chat);
                let content = MessageContent::System {
                    event: "callback_query".to_string(),
                    data: serde_json::json!({
                        "callback_query_id": callback.id,
                        "data": callback.data,
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

    fn session_id_from_chat(&self, chat: &TelegramChat) -> Uuid {
        let bytes = chat.id.to_le_bytes();
        let mut uuid_bytes = [0u8; 16];
        uuid_bytes[..8].copy_from_slice(&bytes);
        Uuid::from_bytes(uuid_bytes)
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
        let chat_id = self.chat_id_from_session(msg.session_id)?;

        match &msg.content {
            MessageContent::Text { text } => {
                serde_json::to_value(SendMessageRequest {
                    chat_id,
                    text: text.clone(),
                    parse_mode: Some("HTML".to_string()),
                    reply_to_message_id: msg.metadata.get("reply_to_telegram_id").and_then(|v| v.as_i64()),
                    disable_web_page_preview: Some(true),
                }).map_err(|e| PlatformError::Internal(e.to_string()))
            }
            MessageContent::Image { url, caption } => {
                serde_json::to_value(SendPhotoRequest {
                    chat_id,
                    photo: url.clone(),
                    caption: caption.clone(),
                    parse_mode: Some("HTML".to_string()),
                    reply_to_message_id: msg.metadata.get("reply_to_telegram_id").and_then(|v| v.as_i64()),
                }).map_err(|e| PlatformError::Internal(e.to_string()))
            }
            MessageContent::File { url, filename: _, mime_type: _ } => {
                serde_json::to_value(SendDocumentRequest {
                    chat_id,
                    document: url.clone(),
                    caption: None,
                    reply_to_message_id: msg.metadata.get("reply_to_telegram_id").and_then(|v| v.as_i64()),
                }).map_err(|e| PlatformError::Internal(e.to_string()))
            }
            _ => Err(PlatformError::UnsupportedContent(format!("Content type not supported for Telegram: {:?}", msg.content))),
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
            let adapter_template = TelegramAdapter::new();

            tokio::spawn(async move {
                let mut offset = 0;
                let mut conflict_retry_delay = 5;

                loop {
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

                                                if let Ok(messages) = adapter_template.convert_incoming(update) {
                                                    for msg in messages {
                                                        // Auto send typing indicator immediately
                                                        if let Ok(chat_id) = adapter_template.chat_id_from_session(msg.session_id) {
                                                            let client_clone = client.clone();
                                                            let bot_token_clone = bot_token.clone();
                                                            tokio::spawn(async move {
                                                                let action_url = format!("https://api.telegram.org/bot{}/sendChatAction", bot_token_clone);
                                                                let body = serde_json::json!({"chat_id": chat_id, "action": "typing"});
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
                            error!("[telegram] Telegram long polling network error: {}", e);
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

        let chat_id = self.chat_id_from_session(msg.session_id)?;

        match &msg.content {
            MessageContent::Text { text } => {
                let chunks = chunk_message(text, 4000);
                let reply_to_id = msg.metadata.get("reply_to_telegram_id").and_then(|v| v.as_i64());
                let mut last_message_id = None;

                for (idx, chunk) in chunks.iter().enumerate() {
                    let chunk_reply_to = if idx == 0 { reply_to_id } else { None };
                    let sent = self.send_chunk_with_fallback(chat_id, chunk, chunk_reply_to).await?;
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
                let req = SendPhotoRequest {
                    chat_id,
                    photo: url.clone(),
                    caption: caption.clone(),
                    parse_mode: Some("HTML".to_string()),
                    reply_to_message_id: msg.metadata.get("reply_to_telegram_id").and_then(|v| v.as_i64()),
                };

                let sent: SentMessage = match self.make_request("sendPhoto", &req).await {
                    Ok(sent) => sent,
                    Err(PlatformError::Api { code, message }) => {
                        let status_code = code.parse::<u16>().unwrap_or(0);
                        if looks_like_parse_error(status_code, &message) {
                            warn!("[telegram] sendPhoto HTML caption failed, retrying without parse_mode");
                            let plain_caption = caption.as_deref().map(strip_html_tags);
                            let plain_req = SendPhotoRequest {
                                chat_id,
                                photo: url.clone(),
                                caption: plain_caption,
                                parse_mode: None,
                                reply_to_message_id: msg.metadata.get("reply_to_telegram_id").and_then(|v| v.as_i64()),
                            };
                            self.make_request("sendPhoto", &plain_req).await?
                        } else {
                            return Err(PlatformError::Api { code, message });
                        }
                    }
                    Err(other) => return Err(other),
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
