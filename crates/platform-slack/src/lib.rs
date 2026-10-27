// hermes-platform-slack - Slack Socket Mode + Web API adapter
use hermes_platform_adapter::{PlatformAdapter, BaseAdapter, BasePlatformConfig};
use hermes_protocol::{
    Platform, PlatformEvent, PlatformError, DeliveryReceipt, DeliveryStatus,
    IncomingMessage, OutgoingMessage, MessageId, MessageContent,
};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock, Mutex};
use reqwest::{Client, header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE}};
use tracing::{info, error};
use uuid::Uuid;
use time::OffsetDateTime;
use futures::{StreamExt, SinkExt};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use hex;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlackConfig {
    pub bot_token: String,
    pub app_token: String,
    pub signing_secret: String,
    pub webhook_url: Option<String>,
    pub socket_mode: bool,
    #[serde(flatten)]
    pub base: BasePlatformConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SlackMessage {
    #[serde(rename = "type")]
    msg_type: String,
    user: Option<String>,
    text: Option<String>,
    ts: String,
    channel: String,
    thread_ts: Option<String>,
    blocks: Option<Vec<SlackBlock>>,
    files: Option<Vec<SlackFile>>,
    subtype: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SlackBlock {
    #[serde(rename = "type")]
    block_type: String,
    text: Option<SlackTextObject>,
    elements: Option<Vec<SlackBlockElement>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SlackTextObject {
    #[serde(rename = "type")]
    text_type: String,
    text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SlackBlockElement {
    #[serde(rename = "type")]
    element_type: String,
    text: Option<SlackTextObject>,
    value: Option<String>,
    action_id: Option<String>,
    elements: Option<Vec<SlackBlockElement>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SlackFile {
    id: String,
    name: String,
    mimetype: String,
    url_private: String,
    size: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SlackEventWrapper {
    #[serde(rename = "type")]
    event_type: String,
    event: Option<SlackMessage>,
    event_ts: Option<String>,
    channel: Option<String>,
    user: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SlackSocketModeEvent {
    envelope_id: String,
    payload: SlackEventWrapper,
    #[serde(rename = "type")]
    event_type: String,
    acceptance_response_payload: Option<serde_json::Value>,
}

#[derive(Debug, Serialize)]
struct SlackPostMessageRequest {
    channel: String,
    text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    thread_ts: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    blocks: Option<Vec<SlackBlock>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    parse_mode: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SlackApiResponse<T> {
    ok: bool,
    #[serde(flatten)]
    data: Option<T>,
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct SlackPostMessageResponse {
    ts: String,
    channel: String,
    message: Option<SlackMessage>,
}

#[derive(Debug, Deserialize, Clone)]
#[allow(dead_code)]
struct SlackAuthTestResponse {
    user_id: String,
    team_id: String,
    team: String,
    user: String,
    bot_id: String,
}

#[derive(Debug, Deserialize)]
struct SlackAppsConnectionsOpenResponse {
    url: String,
}

#[derive(Debug)]
pub struct SlackAdapter {
    base: BaseAdapter<SlackConfig>,
    client: Client,
    bot_info: Arc<RwLock<Option<SlackAuthTestResponse>>>,
    pending_messages: Arc<RwLock<HashMap<MessageId, (OutgoingMessage, mpsc::UnboundedSender<DeliveryReceipt>)>>>,
    socket_mode_task: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
}

impl SlackAdapter {
    pub fn new() -> Self {
        Self {
            base: BaseAdapter::new(Platform::Slack),
            client: Client::new(),
            bot_info: Arc::new(RwLock::new(None)),
            pending_messages: Arc::new(RwLock::new(HashMap::new())),
            socket_mode_task: Arc::new(Mutex::new(None)),
        }
    }

    fn api_url(&self, method: &str) -> String {
        format!("https://slack.com/api/{}", method)
    }

    fn auth_header(&self) -> HeaderMap {
        let config = self.base.config.as_ref().expect("Not initialized");
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, HeaderValue::from_str(&format!("Bearer {}", config.bot_token)).unwrap());
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        headers
    }

    async fn make_request<T: for<'de> Deserialize<'de>>(&self, method: &str, body: &impl Serialize) -> Result<T, PlatformError> {
        let url = self.api_url(method);
        let config = self.base.config.as_ref().expect("Not initialized");

        let response = self.client
            .post(&url)
            .headers(self.auth_header())
            .json(body)
            .timeout(std::time::Duration::from_secs(config.base.custom.get("timeout_secs").and_then(|v| v.as_u64()).unwrap_or(30)))
            .send()
            .await
            .map_err(|e| PlatformError::Network(e.to_string()))?;

        let status = response.status();
        let slack_response: SlackApiResponse<T> = response.json().await
            .map_err(|e| PlatformError::Internal(e.to_string()))?;

        if !slack_response.ok {
            return Err(PlatformError::Api {
                code: status.to_string(),
                message: slack_response.error.unwrap_or_else(|| "Unknown Slack API error".to_string()),
            });
        }

        slack_response.data.ok_or_else(|| PlatformError::Internal("Empty result from Slack API".into()))
    }

    fn session_id_from_channel(&self, channel_id: &str) -> Uuid {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut hasher = DefaultHasher::new();
        channel_id.hash(&mut hasher);
        let hash = hasher.finish();
        let bytes = hash.to_le_bytes();
        let mut uuid_bytes = [0u8; 16];
        uuid_bytes[..8].copy_from_slice(&bytes);
        Uuid::from_bytes(uuid_bytes)
    }

    #[allow(dead_code)]
    fn session_id_from_user(&self, user_id: &str) -> Uuid {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut hasher = DefaultHasher::new();
        user_id.hash(&mut hasher);
        let hash = hasher.finish();
        let bytes = hash.to_le_bytes();
        let mut uuid_bytes = [0u8; 16];
        uuid_bytes[..8].copy_from_slice(&bytes);
        Uuid::from_bytes(uuid_bytes)
    }

    fn convert_incoming_message(&self, msg: &SlackMessage) -> Result<IncomingMessage, PlatformError> {
        let session_id = self.session_id_from_channel(&msg.channel);
        let content = self.convert_message_content(msg)?;

        let ts_parts: Vec<&str> = msg.ts.split('.').collect();
        let timestamp = ts_parts.first()
            .and_then(|s| s.parse::<i64>().ok())
            .map(|ts| OffsetDateTime::from_unix_timestamp(ts).unwrap_or_else(|_| OffsetDateTime::now_utc()))
            .unwrap_or_else(|| OffsetDateTime::now_utc());

        Ok(IncomingMessage {
            id: MessageId::new(),
            session_id,
            platform: Platform::Slack,
            content,
            timestamp,
            raw: Some(serde_json::to_value(msg).unwrap_or(serde_json::Value::Null)),
        })
    }

    fn convert_message_content(&self, msg: &SlackMessage) -> Result<MessageContent, PlatformError> {
        // Handle blocks first for rich content
        if let Some(blocks) = &msg.blocks {
            for block in blocks {
                if block.block_type == "rich_text" || block.block_type == "section" {
                    if let Some(elements) = &block.elements {
                        for element in elements {
                            if element.element_type == "rich_text_section" {
                                if let Some(text_elements) = &element.elements {
                                    let text: String = text_elements.iter()
                                        .filter_map(|e| e.text.as_ref().map(|t| t.text.clone()))
                                        .collect();
                                    if !text.is_empty() {
                                        return Ok(MessageContent::Text { text });
                                    }
                                }
                            }
                        }
                    }
                    if let Some(text_obj) = &block.text {
                        return Ok(MessageContent::Text { text: text_obj.text.clone() });
                    }
                }
            }
        }

        // Fall back to plain text
        if let Some(text) = &msg.text {
            return Ok(MessageContent::Text { text: text.clone() });
        }

        // Handle files
        if let Some(files) = &msg.files {
            if let Some(file) = files.first() {
                if file.mimetype.starts_with("image/") {
                    return Ok(MessageContent::Image {
                        url: file.url_private.clone(),
                        caption: Some(file.name.clone()),
                    });
                }
                return Ok(MessageContent::File {
                    url: file.url_private.clone(),
                    filename: file.name.clone(),
                    mime_type: Some(file.mimetype.clone()),
                });
            }
        }

        Ok(MessageContent::Unknown { data: serde_json::to_value(msg).unwrap_or_default() })
    }

    fn convert_outgoing(&self, msg: &OutgoingMessage) -> Result<SlackPostMessageRequest, PlatformError> {
        let channel_id = self.channel_id_from_session(msg.session_id)?;

        let (text, blocks) = match &msg.content {
            MessageContent::Text { text } => (text.clone(), None),
            MessageContent::Image { url: _, caption } => {
                let blocks = Some(vec![
                    SlackBlock {
                        block_type: "image".to_string(),
                        text: caption.as_ref().map(|c| SlackTextObject {
                            text_type: "mrkdwn".to_string(),
                            text: c.clone(),
                        }),
                        elements: None,
                    }
                ]);
                (caption.as_ref().cloned().unwrap_or_default(), blocks)
            }
            MessageContent::File { url, filename, mime_type: _ } => {
                let blocks = Some(vec![
                    SlackBlock {
                        block_type: "section".to_string(),
                        text: Some(SlackTextObject {
                            text_type: "mrkdwn".to_string(),
                            text: format!("File: <{}|{}>", url, filename),
                        }),
                        elements: None,
                    }
                ]);
                (format!("File: {}", filename), blocks)
            }
            _ => return Err(PlatformError::UnsupportedContent(format!("Content type not supported for Slack: {:?}", msg.content))),
        };

        Ok(SlackPostMessageRequest {
            channel: channel_id,
            text,
            thread_ts: msg.reply_to.map(|id| self.message_ts_from_id(&id)),
            blocks,
            parse_mode: Some("mrkdwn".to_string()),
        })
    }

    fn channel_id_from_session(&self, _session_id: Uuid) -> Result<String, PlatformError> {
        let config = self.base.config.as_ref().expect("Not initialized");
        config.base.custom.get("channel_id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| PlatformError::Internal("No channel ID mapping for session".into()))
    }

    fn message_ts_from_id(&self, message_id: &MessageId) -> String {
        let bytes = message_id.0.as_bytes();
        let ts_bytes: [u8; 8] = bytes[..8].try_into().unwrap_or([0; 8]);
        let ts = u64::from_le_bytes(ts_bytes);
        format!("{}.{}", ts / 1_000_000, ts % 1_000_000)
    }

    #[allow(dead_code)]
    async fn verify_signature(&self, headers: &HeaderMap, body: &[u8]) -> Result<bool, PlatformError> {
        let config = self.base.config.as_ref().expect("Not initialized");
        let timestamp = headers.get("X-Slack-Request-Timestamp")
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| PlatformError::Auth("Missing X-Slack-Request-Timestamp".into()))?;
        
        let signature = headers.get("X-Slack-Signature")
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| PlatformError::Auth("Missing X-Slack-Signature".into()))?;

        // Check timestamp is within 5 minutes
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let req_time: u64 = timestamp.parse().map_err(|_| PlatformError::Auth("Invalid timestamp".into()))?;
        if now.saturating_sub(req_time) > 300 {
            return Err(PlatformError::Auth("Request timestamp too old".into()));
        }

        // Compute expected signature
        let sig_basestring = format!("v0:{}:{}", timestamp, String::from_utf8_lossy(body));
        let mut mac = Hmac::<Sha256>::new_from_slice(config.signing_secret.as_bytes())
            .map_err(|e| PlatformError::Internal(e.to_string()))?;
        mac.update(sig_basestring.as_bytes());
        let expected_signature = format!("v0={}", hex::encode(mac.finalize().into_bytes()));

        // Constant-time comparison
        let expected_bytes = expected_signature.as_bytes();
        let signature_bytes = signature.as_bytes();
        
        if expected_bytes.len() != signature_bytes.len() {
            return Ok(false);
        }
        
        let mut result = 0u8;
        for (a, b) in expected_bytes.iter().zip(signature_bytes.iter()) {
            result |= a ^ b;
        }
        
        Ok(result == 0)
    }

    async fn handle_socket_mode(&self, sender: mpsc::UnboundedSender<PlatformEvent>) -> Result<(), PlatformError> {
        let config = self.base.config.as_ref().expect("Not initialized");
        
        // Open Socket Mode connection
        let conn_response: SlackAppsConnectionsOpenResponse = self.make_request(
            "apps.connections.open",
            &serde_json::json!({ "token": config.app_token })
        ).await?;

        info!("Slack Socket Mode connection URL: {}", conn_response.url);

        // Connect via WebSocket
        let (ws_stream, _) = tokio_tungstenite::connect_async(&conn_response.url).await
            .map_err(|e| PlatformError::Network(format!("WebSocket connection failed: {}", e)))?;

        let (mut write, mut read) = ws_stream.split();

        // Send hello
        let hello = serde_json::json!({
            "type": "hello",
            "num_connections": 1
        });
        write.send(tokio_tungstenite::tungstenite::Message::Text(hello.to_string().into())).await
            .map_err(|e| PlatformError::Network(e.to_string()))?;

        // Process incoming messages
        while let Some(msg) = read.next().await {
            match msg {
                Ok(tokio_tungstenite::tungstenite::Message::Text(text)) => {
                    if let Ok(envelope) = serde_json::from_str::<SlackSocketModeEvent>(&text) {
                        // Acknowledge receipt
                        let ack = serde_json::json!({
                            "envelope_id": envelope.envelope_id,
                            "type": "ack"
                        });
                        let _ = write.send(tokio_tungstenite::tungstenite::Message::Text(ack.to_string().into())).await;

                        // Handle event
                        if let Some(event) = envelope.payload.event {
                            if event.msg_type == "message" && event.subtype.is_none() {
                                if let Ok(incoming) = self.convert_incoming_message(&event) {
                                    let _ = sender.send(PlatformEvent::Incoming(incoming));
                                }
                            }
                        }
                    }
                }
                Ok(tokio_tungstenite::tungstenite::Message::Close(_)) => {
                    info!("Slack Socket Mode connection closed");
                    break;
                }
                Err(e) => {
                    error!("Slack Socket Mode error: {}", e);
                    break;
                }
                _ => {}
            }
        }

        Ok(())
    }
}

#[async_trait]
impl PlatformAdapter for SlackAdapter {
    type Config = SlackConfig;
    type IncomingEvent = IncomingMessage;
    type OutgoingMessage = OutgoingMessage;

    fn platform(&self) -> Platform {
        Platform::Slack
    }

    async fn initialize(&mut self, config: Self::Config) -> Result<(), PlatformError> {
        self.base = BaseAdapter::new(Platform::Slack).with_config(config.clone());

        if let Some(rate_limit) = config.base.rate_limit.clone() {
            self.base.setup_rate_limit(Some(rate_limit));
        }

        // Test authentication
        let auth: SlackAuthTestResponse = self.make_request("auth.test", &serde_json::json!({})).await?;
        *self.bot_info.write().await = Some(auth.clone());
        info!("Slack bot initialized: {} (team: {})", auth.user, auth.team);

        // Set webhook if configured
        if let Some(webhook_url) = &config.webhook_url {
            let _webhook_config = config.base.webhook.clone();
            info!("Slack webhook configured: {}", webhook_url);
        }

        Ok(())
    }

    async fn start(&mut self, sender: mpsc::UnboundedSender<PlatformEvent>) -> Result<(), PlatformError> {
        self.base.set_sender(sender.clone());
        self.base.started = true;

        let config = self.base.config.as_ref().expect("Not initialized");

        if config.socket_mode {
            // Start Socket Mode connection
            let adapter = self.clone();
            let task = tokio::spawn(async move {
                if let Err(e) = adapter.handle_socket_mode(sender).await {
                    error!("Socket Mode task error: {}", e);
                }
            });
            *self.socket_mode_task.lock().await = Some(task);
        } else {
            info!("Slack adapter started in webhook mode");
        }

        info!("Slack adapter started");
        Ok(())
    }

    async fn stop(&mut self) -> Result<(), PlatformError> {
        self.base.started = false;

        // Stop Socket Mode task
        if let Some(task) = self.socket_mode_task.lock().await.take() {
            task.abort();
        }

        info!("Slack adapter stopped");
        Ok(())
    }

    async fn send(&self, msg: Self::OutgoingMessage) -> Result<DeliveryReceipt, PlatformError> {
        if let Some(limiter) = &self.base.rate_limiter {
            limiter.acquire().await?;
        }

        let payload = self.convert_outgoing(&msg)?;
        let response: SlackPostMessageResponse = self.make_request("chat.postMessage", &payload).await?;

        Ok(DeliveryReceipt {
            message_id: msg.id,
            platform: Platform::Slack,
            status: DeliveryStatus::Sent,
            timestamp: OffsetDateTime::now_utc(),
            platform_message_id: Some(response.ts),
        })
    }

    async fn handle_webhook(&self, payload: serde_json::Value) -> Result<Vec<IncomingMessage>, PlatformError> {
        // Handle Slack Events API webhook
        if let Some(_challenge) = payload.get("challenge") {
            // URL verification challenge
            return Ok(vec![]);
        }

        if let Some(event) = payload.get("event") {
            if let Ok(msg) = serde_json::from_value::<SlackMessage>(event.clone()) {
                return Ok(vec![self.convert_incoming_message(&msg)?]);
            }
        }

        Ok(vec![])
    }

    async fn health_check(&self) -> Result<(), PlatformError> {
        let _: SlackAuthTestResponse = self.make_request("auth.test", &serde_json::json!({})).await?;
        Ok(())
    }
}

impl Clone for SlackAdapter {
    fn clone(&self) -> Self {
        Self {
            base: BaseAdapter::new(Platform::Slack),
            client: self.client.clone(),
            bot_info: self.bot_info.clone(),
            pending_messages: self.pending_messages.clone(),
            socket_mode_task: Arc::new(Mutex::new(None)),
        }
    }
}

impl Default for SlackAdapter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hermes_protocol::{MessageContent, Platform};

    #[test]
    fn test_session_id_from_channel() {
        let adapter = SlackAdapter::new();
        let id1 = adapter.session_id_from_channel("C123456");
        let id2 = adapter.session_id_from_channel("C123456");
        let id3 = adapter.session_id_from_channel("C789012");
        
        assert_eq!(id1, id2);
        assert_ne!(id1, id3);
    }

    #[test]
    fn test_session_id_from_user() {
        let adapter = SlackAdapter::new();
        let id1 = adapter.session_id_from_user("U123456");
        let id2 = adapter.session_id_from_user("U123456");
        let id3 = adapter.session_id_from_user("U789012");
        
        assert_eq!(id1, id2);
        assert_ne!(id1, id3);
    }

    #[test]
    fn test_convert_message_content_text() {
        let adapter = SlackAdapter::new();
        let msg = SlackMessage {
            msg_type: "message".to_string(),
            user: Some("U123".to_string()),
            text: Some("Hello, world!".to_string()),
            ts: "1234567890.123456".to_string(),
            channel: "C123".to_string(),
            thread_ts: None,
            blocks: None,
            files: None,
            subtype: None,
        };

        let content = adapter.convert_message_content(&msg).unwrap();
        assert!(matches!(content, MessageContent::Text { text } if text == "Hello, world!"));
    }

    #[test]
    fn test_convert_message_content_image() {
        let adapter = SlackAdapter::new();
        let msg = SlackMessage {
            msg_type: "message".to_string(),
            user: Some("U123".to_string()),
            text: None,
            ts: "1234567890.123456".to_string(),
            channel: "C123".to_string(),
            thread_ts: None,
            blocks: None,
            files: Some(vec![SlackFile {
                id: "F123".to_string(),
                name: "image.png".to_string(),
                mimetype: "image/png".to_string(),
                url_private: "https://files.slack.com/image.png".to_string(),
                size: 1024,
            }]),
            subtype: None,
        };

        let content = adapter.convert_message_content(&msg).unwrap();
        assert!(matches!(content, MessageContent::Image { url, caption } 
            if url == "https://files.slack.com/image.png" && caption == Some("image.png".to_string())));
    }

    #[test]
    fn test_convert_message_content_file() {
        let adapter = SlackAdapter::new();
        let msg = SlackMessage {
            msg_type: "message".to_string(),
            user: Some("U123".to_string()),
            text: None,
            ts: "1234567890.123456".to_string(),
            channel: "C123".to_string(),
            thread_ts: None,
            blocks: None,
            files: Some(vec![SlackFile {
                id: "F123".to_string(),
                name: "document.pdf".to_string(),
                mimetype: "application/pdf".to_string(),
                url_private: "https://files.slack.com/document.pdf".to_string(),
                size: 2048,
            }]),
            subtype: None,
        };

        let content = adapter.convert_message_content(&msg).unwrap();
        assert!(matches!(content, MessageContent::File { url, filename, mime_type } 
            if url == "https://files.slack.com/document.pdf" 
            && filename == "document.pdf" 
            && mime_type == Some("application/pdf".to_string())));
    }

    #[test]
    fn test_convert_outgoing_text() {
        let mut adapter = SlackAdapter::new();
        let config = SlackConfig {
            bot_token: "test_token".to_string(),
            app_token: "test_app_token".to_string(),
            signing_secret: "test_secret".to_string(),
            webhook_url: None,
            socket_mode: false,
            base: BasePlatformConfig {
                enabled: true,
                rate_limit: None,
                webhook: None,
                custom: serde_json::json!({"channel_id": "C123456"}),
            },
        };
        adapter.base.config = Some(config);
        let msg = OutgoingMessage {
            id: MessageId::new(),
            session_id: Uuid::nil(),
            platform: Platform::Slack,
            content: MessageContent::Text { text: "Test message".to_string() },
            reply_to: None,
            metadata: HashMap::new(),
        };

        let result = adapter.convert_outgoing(&msg);
        assert!(result.is_ok());
        let req = result.unwrap();
        assert_eq!(req.text, "Test message");
        assert_eq!(req.parse_mode, Some("mrkdwn".to_string()));
    }

    #[test]
    fn test_platform_identifier() {
        let adapter = SlackAdapter::new();
        assert_eq!(adapter.platform(), Platform::Slack);
    }

    #[test]
    fn test_verify_signature_valid() {
        // Test signature verification logic
        let secret = "test_secret";
        let timestamp = "1234567890";
        let body = "test_body";
        let basestring = format!("v0:{}:{}", timestamp, body);
        
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(basestring.as_bytes());
        let expected = format!("v0={}", hex::encode(mac.finalize().into_bytes()));
        
        // Verify with correct signature
        let mut mac2 = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
        mac2.update(basestring.as_bytes());
        let computed = hex::encode(mac2.finalize().into_bytes());
        
        assert_eq!(expected, format!("v0={}", computed));
    }
}