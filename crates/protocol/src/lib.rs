// hermes-protocol - Shared types and protocol definitions
use serde::{Deserialize, Serialize};
pub use uuid::Uuid;
use std::collections::HashMap;

/// Unique identifier for messages
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MessageId(pub Uuid);

impl MessageId {
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }
}

impl Default for MessageId {
    fn default() -> Self {
        Self::new()
    }
}

/// Platform identifiers
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Platform {
    Telegram,
    Discord,
    Slack,
    WhatsApp,
    Signal,
    Matrix,
    Webhook,
    Email,
    Sms,
    Ntfy,
    Simplex,
    Feishu,
    WeCom,
    DingTalk,
    Teams,
    GoogleChat,
    QQBot,
    Weixin,
    A2A,
    Raft,
    Photon,
    Buzz,
    Irc,
    Mattermost,
    Line,
    HomeAssistant,
    Other(u16),
}

impl Platform {
    pub fn as_str(&self) -> &'static str {
        match self {
            Platform::Telegram => "telegram",
            Platform::Discord => "discord",
            Platform::Slack => "slack",
            Platform::WhatsApp => "whatsapp",
            Platform::Signal => "signal",
            Platform::Matrix => "matrix",
            Platform::Webhook => "webhook",
            Platform::Email => "email",
            Platform::Sms => "sms",
            Platform::Ntfy => "ntfy",
            Platform::Simplex => "simplex",
            Platform::Feishu => "feishu",
            Platform::WeCom => "wecom",
            Platform::DingTalk => "dingtalk",
            Platform::Teams => "teams",
            Platform::GoogleChat => "google_chat",
            Platform::QQBot => "qqbot",
            Platform::Weixin => "weixin",
            Platform::A2A => "a2a",
            Platform::Raft => "raft",
            Platform::Photon => "photon",
            Platform::Buzz => "buzz",
            Platform::Irc => "irc",
            Platform::Mattermost => "mattermost",
            Platform::Line => "line",
            Platform::HomeAssistant => "homeassistant",
            Platform::Other(n) => return Box::leak(format!("other_{}", n).into_boxed_str()),
        }
    }
}

impl std::fmt::Display for Platform {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// Session state
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    Active,
    Idle,
    Suspended,
    Terminated,
}

/// Session metadata
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: Uuid,
    pub profile: String,
    pub platform: Platform,
    pub platform_session_id: String,
    pub state: SessionState,
    pub created_at: time::OffsetDateTime,
    pub updated_at: time::OffsetDateTime,
    pub metadata: HashMap<String, serde_json::Value>,
}

/// Incoming message from platform
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IncomingMessage {
    pub id: MessageId,
    pub session_id: Uuid,
    pub platform: Platform,
    pub content: MessageContent,
    pub timestamp: time::OffsetDateTime,
    pub raw: Option<serde_json::Value>,
}

/// Outgoing message to platform
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutgoingMessage {
    pub id: MessageId,
    pub session_id: Uuid,
    pub platform: Platform,
    pub content: MessageContent,
    pub reply_to: Option<MessageId>,
    pub metadata: HashMap<String, serde_json::Value>,
}

/// Message content types
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MessageContent {
    Text { text: String },
    Image { url: String, caption: Option<String> },
    File { url: String, filename: String, mime_type: Option<String> },
    Audio { url: String, duration: Option<u64> },
    Video { url: String, duration: Option<u64> },
    Location { latitude: f64, longitude: f64 },
    Contact { name: String, phone: String },
    Sticker { sticker_id: String },
    Reaction { emoji: String, target_id: MessageId },
    System { event: String, data: serde_json::Value },
    Unknown { data: serde_json::Value },
}

/// Platform event (internal)
#[derive(Debug, Clone)]
pub enum PlatformEvent {
    Incoming(IncomingMessage),
    Outgoing(OutgoingMessage),
    Ack { message_id: MessageId, success: bool },
    Error { session_id: Uuid, error: PlatformError },
    SessionCreated(Session),
    SessionUpdated(Session),
    SessionDeleted(Uuid),
}

/// Platform errors
#[derive(Debug, Clone, thiserror::Error, Serialize, Deserialize)]
pub enum PlatformError {
    #[error("Authentication failed: {0}")]
    Auth(String),
    #[error("Rate limited: retry after {retry_after_secs}s")]
    RateLimited { retry_after_secs: u64 },
    #[error("Network error: {0}")]
    Network(String),
    #[error("Platform API error: {code} - {message}")]
    Api { code: String, message: String },
    #[error("Session not found: {0}")]
    SessionNotFound(Uuid),
    #[error("Unsupported content type: {0}")]
    UnsupportedContent(String),
    #[error("Internal error: {0}")]
    Internal(String),
}

/// Delivery receipt
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeliveryReceipt {
    pub message_id: MessageId,
    pub platform: Platform,
    pub status: DeliveryStatus,
    pub timestamp: time::OffsetDateTime,
    pub platform_message_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryStatus {
    Sent,
    Delivered,
    Read,
    Failed,
    Pending,
}

/// Platform adapter trait (defined here for re-export)
pub mod adapter {
    use super::*;
    use async_trait::async_trait;
    use std::fmt::Debug;

    #[async_trait]
    pub trait PlatformAdapter: Send + Sync + Debug {
        type Config: Send + Sync + 'static;
        type IncomingEvent: Send + Sync + 'static;
        type OutgoingMessage: Send + Sync + 'static;

        fn platform(&self) -> Platform;
        async fn initialize(&mut self, config: Self::Config) -> Result<(), PlatformError>;
        async fn start(&mut self, sender: tokio::sync::mpsc::UnboundedSender<PlatformEvent>) -> Result<(), PlatformError>;
        async fn stop(&mut self) -> Result<(), PlatformError>;
        async fn send(&self, msg: Self::OutgoingMessage) -> Result<DeliveryReceipt, PlatformError>;
        fn name(&self) -> &'static str { self.platform().as_str() }
    }
}

pub use adapter::PlatformAdapter;
