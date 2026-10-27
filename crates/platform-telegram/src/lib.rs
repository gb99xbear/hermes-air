// hermes-platform-telegram - Telegram Bot API adapter
use hermes_platform_adapter::{PlatformAdapter, BaseAdapter, BasePlatformConfig};
use hermes_protocol::{
    Platform, PlatformEvent, PlatformError, DeliveryReceipt, DeliveryStatus,
    IncomingMessage, OutgoingMessage, MessageId, MessageContent,
};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock};
use reqwest::Client;
use tracing::{info, error};
use uuid::Uuid;
use time::OffsetDateTime;

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
    parse_mode: Option<String>,
    reply_to_message_id: Option<i64>,
    disable_web_page_preview: Option<bool>,
}

#[derive(Debug, Serialize)]
struct SendPhotoRequest {
    chat_id: i64,
    photo: String,
    caption: Option<String>,
    parse_mode: Option<String>,
    reply_to_message_id: Option<i64>,
}

#[derive(Debug, Serialize)]
struct SendDocumentRequest {
    chat_id: i64,
    document: String,
    caption: Option<String>,
    reply_to_message_id: Option<i64>,
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

#[derive(Debug)]
pub struct TelegramAdapter {
    base: BaseAdapter<TelegramConfig>,
    client: Client,
    bot_info: Arc<RwLock<Option<TelegramUser>>>,
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
        
        let response = self.client
            .post(&url)
            .json(body)
            .timeout(std::time::Duration::from_secs(config.timeout_secs))
            .send()
            .await
            .map_err(|e| PlatformError::Network(e.to_string()))?;

        let status = response.status();
        let tg_response: TelegramResponse<T> = response.json().await
            .map_err(|e| PlatformError::Internal(e.to_string()))?;

        if !tg_response.ok {
            return Err(PlatformError::Api {
                code: tg_response.error_code.map(|c| c.to_string()).unwrap_or_else(|| status.to_string()),
                message: tg_response.description.unwrap_or_else(|| "Unknown Telegram API error".to_string()),
            });
        }

        tg_response.result.ok_or_else(|| PlatformError::Internal("Empty result from Telegram API".into()))
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
        // Deterministic UUID from chat ID
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

    fn chat_id_from_session(&self, session_id: Uuid) -> Result<i64, PlatformError> {
        // Extract chat ID from session UUID (first 8 bytes)
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
        
        // Set sender if already available
        if self.base.sender.is_some() {
            // sender already set
        }

        // Get bot info
        let me: TelegramUser = self.make_request("getMe", &serde_json::json!({})).await?;
        *self.bot_info.write().await = Some(me.clone());
        info!("Telegram bot initialized: @{} (id: {})", me.username.unwrap_or_default(), me.id);

        // Set webhook if configured
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
            // Delete webhook to use long polling
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
            // Start long polling
            let client = self.client.clone();
            let bot_token = config.bot_token.clone();
            let timeout = config.timeout_secs;
            let sender = self.base.sender.clone().expect("Sender not set");
            let _pending = self.pending_messages.clone();
            
            tokio::spawn(async move {
                let mut offset = 0;
                loop {
                    let params = serde_json::json!({
                        "offset": offset,
                        "timeout": timeout,
                        "allowed_updates": ["message", "edited_message", "channel_post", "edited_channel_post", "callback_query"],
                    });
                    
                    let url = format!("https://api.telegram.org/bot{}/getUpdates", bot_token);
                    
                    match client.post(&url).json(&params).send().await {
                        Ok(response) => {
                            if let Ok(tg_response) = response.json::<TelegramResponse<Vec<TelegramUpdate>>>().await {
                                if tg_response.ok {
                                    if let Some(updates) = tg_response.result {
                                        for update in updates {
                                            offset = update.update_id + 1;
                                            
                                            // Convert and emit
                                            let adapter = TelegramAdapter::new();
                                            if let Ok(messages) = adapter.convert_incoming(update) {
                                                for msg in messages {
                                                    let _ = sender.send(PlatformEvent::Incoming(msg));
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            error!("Telegram long polling error: {}", e);
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
        
        // Delete webhook if was set
        if let Some(config) = &self.base.config {
            if config.webhook_url.is_some() {
                let _: serde_json::Value = self.make_request("deleteWebhook", &serde_json::json!({})).await?;
            }
        }
        
        info!("Telegram adapter stopped");
        Ok(())
    }

    async fn send(&self, msg: Self::OutgoingMessage) -> Result<DeliveryReceipt, PlatformError> {
        // Rate limit
        if let Some(limiter) = &self.base.rate_limiter {
            limiter.acquire().await?;
        }

        let payload = self.convert_outgoing(&msg)?;
        let method = match &msg.content {
            MessageContent::Text { .. } => "sendMessage",
            MessageContent::Image { .. } => "sendPhoto",
            MessageContent::File { .. } => "sendDocument",
            _ => return Err(PlatformError::UnsupportedContent("Unsupported content type".into())),
        };

        let sent: SentMessage = self.make_request(method, &payload).await?;
        
        Ok(DeliveryReceipt {
            message_id: msg.id,
            platform: Platform::Telegram,
            status: DeliveryStatus::Sent,
            timestamp: OffsetDateTime::now_utc(),
            platform_message_id: Some(sent.message_id.to_string()),
        })
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
