// hermes-platform-discord - 1-to-1 Discord Messaging Platform Adapter
use hermes_platform_adapter::{BaseAdapter, BasePlatformConfig, PlatformAdapter};
use hermes_protocol::{
    DeliveryReceipt, DeliveryStatus, IncomingMessage, MessageContent, MessageId, OutgoingMessage,
    Platform, PlatformError, PlatformEvent,
};
use async_trait::async_trait;
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::sync::Arc;
use time::OffsetDateTime;
use tokio::sync::{mpsc, RwLock};
use tracing::info;
use uuid::Uuid;

/// Strict maximum message character count enforced by Discord REST API
pub const DISCORD_MAX_MESSAGE_CHARS: usize = 2000;

/// Maximum number of split message chunks permitted for a single response turn
pub const MAX_SPLIT_MESSAGES: usize = 8;

/// Discord platform configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscordConfig {
    pub bot_token: Option<String>,
    pub client_id: Option<String>,
    pub guild_id: Option<String>,
    #[serde(default = "default_true")]
    pub require_mention: bool,
    #[serde(default)]
    pub free_response_channels: HashSet<String>,
    #[serde(default)]
    pub allowed_channels: HashSet<String>,
    #[serde(default)]
    pub ignored_channels: HashSet<String>,
    #[serde(flatten)]
    pub base: BasePlatformConfig,
}

fn default_true() -> bool {
    true
}

impl Default for DiscordConfig {
    fn default() -> Self {
        Self {
            bot_token: std::env::var("DISCORD_BOT_TOKEN").ok(),
            client_id: std::env::var("DISCORD_CLIENT_ID").ok(),
            guild_id: std::env::var("DISCORD_GUILD_ID").ok(),
            require_mention: true,
            free_response_channels: HashSet::new(),
            allowed_channels: HashSet::new(),
            ignored_channels: HashSet::new(),
            base: BasePlatformConfig {
                enabled: true,
                rate_limit: None,
                webhook: None,
                custom: serde_json::Value::Null,
            },
        }
    }
}

/// Discord User model
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DiscordUser {
    pub id: String,
    pub username: String,
    #[serde(default)]
    pub bot: bool,
    pub discriminator: Option<String>,
    pub avatar: Option<String>,
}

/// Discord Attachment model
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DiscordAttachment {
    pub id: String,
    pub filename: String,
    pub size: usize,
    pub url: String,
    pub proxy_url: Option<String>,
    pub content_type: Option<String>,
}

/// Discord Embed model
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DiscordEmbed {
    pub title: Option<String>,
    #[serde(rename = "type")]
    pub embed_type: Option<String>,
    pub description: Option<String>,
    pub url: Option<String>,
}

/// Discord Message Reference for replies
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DiscordMessageReference {
    pub message_id: Option<String>,
    pub channel_id: Option<String>,
    pub guild_id: Option<String>,
    #[serde(default = "default_true")]
    pub fail_if_not_exists: bool,
}

/// Allowed Mentions configuration for outbound messages
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct AllowedMentions {
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub parse: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub users: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub roles: Vec<String>,
    pub replied_user: bool,
}

/// Discord Message wire model
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DiscordMessage {
    pub id: String,
    pub channel_id: String,
    pub guild_id: Option<String>,
    pub author: DiscordUser,
    pub content: String,
    #[serde(default)]
    pub attachments: Vec<DiscordAttachment>,
    #[serde(default)]
    pub embeds: Vec<DiscordEmbed>,
    #[serde(default)]
    pub mentions: Vec<DiscordUser>,
    pub message_reference: Option<DiscordMessageReference>,
    pub thread_id: Option<String>,
}

/// Outbound message payload to create a new message
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CreateMessageRequest {
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message_reference: Option<DiscordMessageReference>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub embeds: Vec<DiscordEmbed>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allowed_mentions: Option<AllowedMentions>,
}

/// Outbound message payload to edit an existing message
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EditMessageRequest {
    pub content: String,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub embeds: Vec<DiscordEmbed>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allowed_mentions: Option<AllowedMentions>,
}

/// Splits a long message into chunks capped at `max_len`, preferring line and word boundaries.
/// Caps chunks at `MAX_SPLIT_MESSAGES` (matching `_cap_split_chunks`).
pub fn chunk_message(text: &str, max_len: usize) -> Vec<String> {
    if text.is_empty() {
        return vec![];
    }
    if text.len() <= max_len {
        return vec![text.to_string()];
    }

    let mut chunks = Vec::new();
    let mut remaining = text;

    while remaining.len() > max_len {
        let mut split_boundary = max_len;
        while split_boundary > 0 && !remaining.is_char_boundary(split_boundary) {
            split_boundary -= 1;
        }

        let region = &remaining[..split_boundary];
        let mut split_at = region.rfind('\n').unwrap_or(0);
        if split_at < max_len / 2 {
            if let Some(space_idx) = region.rfind(' ') {
                if space_idx >= max_len / 2 {
                    split_at = space_idx;
                }
            }
        }

        if split_at < 1 {
            split_at = split_boundary;
        }

        let chunk = &remaining[..split_at];
        chunks.push(chunk.trim_end().to_string());
        remaining = remaining[split_at..].trim_start_matches(['\r', '\n']);
    }

    if !remaining.is_empty() {
        chunks.push(remaining.to_string());
    }

    cap_split_chunks(chunks)
}

/// Caps the split chunks list at `MAX_SPLIT_MESSAGES` with an informative warning notice
pub fn cap_split_chunks(chunks: Vec<String>) -> Vec<String> {
    if chunks.len() <= MAX_SPLIT_MESSAGES {
        return chunks;
    }

    let mut kept: Vec<String> = chunks.into_iter().take(MAX_SPLIT_MESSAGES - 1).collect();
    let notice = format!(
        "\n\n⚠️ **Response truncated** — this reply exceeded the delivery limit ({} messages). The full response is in the session logs.",
        MAX_SPLIT_MESSAGES
    );
    kept.push(notice);
    kept
}

/// Parses a Snowflake string into a u64, falling back to a deterministic 8-byte hash
pub fn parse_snowflake(val: &str) -> u64 {
    val.parse::<u64>().unwrap_or_else(|_| {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut hasher = DefaultHasher::new();
        val.hash(&mut hasher);
        hasher.finish()
    })
}

/// Derives a deterministic 16-byte UUID from Discord `channel_id` and optional `thread_id`
pub fn session_id_from_discord(channel_id: &str, thread_id: Option<&str>) -> Uuid {
    let chan_u64 = parse_snowflake(channel_id);
    let mut bytes = [0u8; 16];
    bytes[..8].copy_from_slice(&chan_u64.to_le_bytes());

    if let Some(tid) = thread_id {
        let thread_u64 = parse_snowflake(tid);
        bytes[8..].copy_from_slice(&thread_u64.to_le_bytes());
    }

    Uuid::from_bytes(bytes)
}

/// Recovers `(channel_id, thread_id)` snowflake strings from deterministic UUID
pub fn discord_channel_and_thread_from_session(session_id: Uuid) -> (String, Option<String>) {
    let bytes = session_id.as_bytes();
    let chan_u64 = u64::from_le_bytes(bytes[..8].try_into().unwrap_or_default());
    let thread_u64 = u64::from_le_bytes(bytes[8..16].try_into().unwrap_or_default());

    let channel_id = chan_u64.to_string();
    let thread_id = if thread_u64 != 0 {
        Some(thread_u64.to_string())
    } else {
        None
    };

    (channel_id, thread_id)
}

/// Helper to strip bot mention `<@bot_id>` or `<@!bot_id>` from message content
pub fn strip_bot_mention(content: &str, bot_id: &str) -> String {
    let tag1 = format!("<@{}>", bot_id);
    let tag2 = format!("<@!{}>", bot_id);
    content.replace(&tag1, "").replace(&tag2, "").trim().to_string()
}

/// Evaluates inbound message admission policy and returns `(should_process, cleaned_content)`
pub fn should_process_message(
    msg: &DiscordMessage,
    bot_user: Option<&DiscordUser>,
    config: &DiscordConfig,
) -> (bool, String) {
    let bot_id = bot_user.map(|b| b.id.as_str()).unwrap_or("");

    // Ignore messages authored by the bot itself
    if !bot_id.is_empty() && msg.author.id == bot_id {
        return (false, msg.content.clone());
    }

    let is_dm = msg.guild_id.is_none();
    let has_bot_mention = if !bot_id.is_empty() {
        msg.mentions.iter().any(|u| u.id == bot_id)
            || msg.content.contains(&format!("<@{}>", bot_id))
            || msg.content.contains(&format!("<@!{}>", bot_id))
    } else {
        false
    };

    // Direct Messages (DMs) pass freely
    if is_dm {
        let cleaned = if has_bot_mention {
            strip_bot_mention(&msg.content, bot_id)
        } else {
            msg.content.trim().to_string()
        };
        return (true, cleaned);
    }

    // Guild channel checks
    let channel_id = &msg.channel_id;

    // Check ignored_channels
    if config.ignored_channels.contains("*") || config.ignored_channels.contains(channel_id) {
        return (false, msg.content.clone());
    }

    // Check allowed_channels whitelist
    if !config.allowed_channels.is_empty()
        && !config.allowed_channels.contains("*")
        && !config.allowed_channels.contains(channel_id)
    {
        return (false, msg.content.clone());
    }

    // Check free response channels
    let is_free_response = config.free_response_channels.contains("*")
        || config.free_response_channels.contains(channel_id);

    // If require_mention is active and not a free response channel, bot must be mentioned
    if config.require_mention && !is_free_response && !has_bot_mention {
        return (false, msg.content.clone());
    }

    let cleaned = if has_bot_mention {
        strip_bot_mention(&msg.content, bot_id)
    } else {
        msg.content.trim().to_string()
    };

    (true, cleaned)
}

/// Discord Messaging Platform Adapter
#[derive(Debug)]
pub struct DiscordAdapter {
    pub base: BaseAdapter<DiscordConfig>,
    client: Client,
    bot_user: Arc<RwLock<Option<DiscordUser>>>,
}

impl DiscordAdapter {
    pub fn new() -> Self {
        Self {
            base: BaseAdapter::new(Platform::Discord),
            client: Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .unwrap_or_else(|_| Client::new()),
            bot_user: Arc::new(RwLock::new(None)),
        }
    }

    pub fn with_config(mut self, config: DiscordConfig) -> Self {
        self.base = self.base.with_config(config);
        self
    }

    pub async fn bot_user(&self) -> Option<DiscordUser> {
        self.bot_user.read().await.clone()
    }

    fn api_base(&self) -> &'static str {
        "https://discord.com/api/v10"
    }

    fn bot_token(&self) -> Result<String, PlatformError> {
        if let Some(cfg) = &self.base.config {
            if let Some(token) = &cfg.bot_token {
                if !token.is_empty() {
                    return Ok(token.clone());
                }
            }
        }
        std::env::var("DISCORD_BOT_TOKEN")
            .map_err(|_| PlatformError::Auth("DISCORD_BOT_TOKEN is not configured".to_string()))
    }

    /// Fetches the authenticated bot identity from Discord REST `/users/@me`
    pub async fn verify_and_get_bot_user(&self) -> Result<DiscordUser, PlatformError> {
        let token = self.bot_token()?;
        let url = format!("{}/users/@me", self.api_base());
        let res = self
            .client
            .get(&url)
            .header("Authorization", format!("Bot {}", token))
            .send()
            .await
            .map_err(|e| PlatformError::Network(e.to_string()))?;

        if !res.status().is_success() {
            let status = res.status();
            let err_body = res.text().await.unwrap_or_default();
            return Err(PlatformError::Api {
                code: status.to_string(),
                message: format!("Failed to verify Discord bot token: {}", err_body),
            });
        }

        let user: DiscordUser = res
            .json()
            .await
            .map_err(|e| PlatformError::Internal(e.to_string()))?;

        let mut lock = self.bot_user.write().await;
        *lock = Some(user.clone());

        Ok(user)
    }

    /// Sends a message chunk to Discord REST endpoint `/channels/{channel_id}/messages`
    pub async fn send_raw_message(
        &self,
        channel_id: &str,
        request: &CreateMessageRequest,
    ) -> Result<DiscordMessage, PlatformError> {
        let token = self.bot_token()?;
        let url = format!("{}/channels/{}/messages", self.api_base(), channel_id);

        let res = self
            .client
            .post(&url)
            .header("Authorization", format!("Bot {}", token))
            .json(request)
            .send()
            .await
            .map_err(|e| PlatformError::Network(e.to_string()))?;

        if !res.status().is_success() {
            let status = res.status();
            let err_body = res.text().await.unwrap_or_default();
            return Err(PlatformError::Api {
                code: status.to_string(),
                message: format!("Discord message send error: {}", err_body),
            });
        }

        let msg: DiscordMessage = res
            .json()
            .await
            .map_err(|e| PlatformError::Internal(e.to_string()))?;

        Ok(msg)
    }

    /// High-level send_message helper with automatic 2000-char chunking and reply reference
    pub async fn send_message(
        &self,
        channel_id: &str,
        content: &str,
        reply_to: Option<&str>,
    ) -> Result<Vec<DiscordMessage>, PlatformError> {
        let chunks = chunk_message(content, DISCORD_MAX_MESSAGE_CHARS);
        let mut results = Vec::new();

        for (idx, chunk) in chunks.iter().enumerate() {
            let msg_ref = if idx == 0 && reply_to.is_some() {
                Some(DiscordMessageReference {
                    message_id: reply_to.map(ToString::to_string),
                    channel_id: Some(channel_id.to_string()),
                    guild_id: None,
                    fail_if_not_exists: false,
                })
            } else {
                None
            };

            let req = CreateMessageRequest {
                content: chunk.clone(),
                message_reference: msg_ref,
                embeds: vec![],
                allowed_mentions: Some(AllowedMentions {
                    parse: vec!["users".to_string(), "roles".to_string(), "everyone".to_string()],
                    users: vec![],
                    roles: vec![],
                    replied_user: true,
                }),
            };

            let sent = self.send_raw_message(channel_id, &req).await?;
            results.push(sent);
        }

        Ok(results)
    }

    /// Edits an existing Discord message for progressive token streaming updates
    pub async fn edit_message(
        &self,
        channel_id: &str,
        message_id: &str,
        content: &str,
    ) -> Result<DiscordMessage, PlatformError> {
        let token = self.bot_token()?;
        let url = format!("{}/channels/{}/messages/{}", self.api_base(), channel_id, message_id);

        let capped_content = if content.len() > DISCORD_MAX_MESSAGE_CHARS {
            let mut end = DISCORD_MAX_MESSAGE_CHARS;
            while end > 0 && !content.is_char_boundary(end) {
                end -= 1;
            }
            &content[..end]
        } else {
            content
        };

        let req = EditMessageRequest {
            content: capped_content.to_string(),
            embeds: vec![],
            allowed_mentions: None,
        };

        let res = self
            .client
            .patch(&url)
            .header("Authorization", format!("Bot {}", token))
            .json(&req)
            .send()
            .await
            .map_err(|e| PlatformError::Network(e.to_string()))?;

        if !res.status().is_success() {
            let status = res.status();
            let err_body = res.text().await.unwrap_or_default();
            return Err(PlatformError::Api {
                code: status.to_string(),
                message: format!("Discord message edit error: {}", err_body),
            });
        }

        let updated: DiscordMessage = res
            .json()
            .await
            .map_err(|e| PlatformError::Internal(e.to_string()))?;

        Ok(updated)
    }

    /// Triggers Discord typing indicator in the target channel
    pub async fn send_typing(&self, channel_id: &str) -> Result<(), PlatformError> {
        let token = self.bot_token()?;
        let url = format!("{}/channels/{}/typing", self.api_base(), channel_id);

        let res = self
            .client
            .post(&url)
            .header("Authorization", format!("Bot {}", token))
            .send()
            .await
            .map_err(|e| PlatformError::Network(e.to_string()))?;

        if !res.status().is_success() && res.status() != StatusCode::NO_CONTENT {
            let status = res.status();
            let err_body = res.text().await.unwrap_or_default();
            return Err(PlatformError::Api {
                code: status.to_string(),
                message: format!("Discord typing indicator error: {}", err_body),
            });
        }

        Ok(())
    }
}

impl Default for DiscordAdapter {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl PlatformAdapter for DiscordAdapter {
    type Config = DiscordConfig;
    type IncomingEvent = IncomingMessage;
    type OutgoingMessage = OutgoingMessage;

    fn platform(&self) -> Platform {
        Platform::Discord
    }

    async fn initialize(&mut self, config: Self::Config) -> Result<(), PlatformError> {
        self.base = BaseAdapter::new(Platform::Discord).with_config(config.clone());
        if let Some(rate_limit) = config.base.rate_limit.clone() {
            self.base.setup_rate_limit(Some(rate_limit));
        }

        // Verify credentials with Discord API if token is provided
        if config.bot_token.is_some() || std::env::var("DISCORD_BOT_TOKEN").is_ok() {
            let bot = self.verify_and_get_bot_user().await?;
            info!(
                "[discord] Initialized Discord bot identity: {}#{} ({})",
                bot.username,
                bot.discriminator.as_deref().unwrap_or("0000"),
                bot.id
            );
        }

        Ok(())
    }

    async fn start(
        &mut self,
        sender: mpsc::UnboundedSender<PlatformEvent>,
    ) -> Result<(), PlatformError> {
        self.base.set_sender(sender);
        self.base.started = true;
        info!("[discord] Discord platform adapter started");
        Ok(())
    }

    async fn stop(&mut self) -> Result<(), PlatformError> {
        self.base.started = false;
        info!("[discord] Discord platform adapter stopped");
        Ok(())
    }

    async fn send(&self, msg: Self::OutgoingMessage) -> Result<DeliveryReceipt, PlatformError> {
        let (channel_id, thread_id) = discord_channel_and_thread_from_session(msg.session_id);
        let target_id = thread_id.as_deref().unwrap_or(&channel_id);

        let reply_to_id = msg
            .metadata
            .get("reply_to_discord_id")
            .and_then(|v| v.as_str())
            .or_else(|| msg.reply_to.map(|_| ""));

        match &msg.content {
            MessageContent::Text { text } => {
                let sent_messages = self.send_message(target_id, text, reply_to_id).await?;
                let last_id = sent_messages.last().map(|m| m.id.clone());

                Ok(DeliveryReceipt {
                    message_id: msg.id,
                    platform: Platform::Discord,
                    status: DeliveryStatus::Sent,
                    timestamp: OffsetDateTime::now_utc(),
                    platform_message_id: last_id,
                })
            }
            MessageContent::Image { url, caption } => {
                let text = if let Some(cap) = caption {
                    format!("{}\n{}", cap, url)
                } else {
                    url.clone()
                };
                let sent_messages = self.send_message(target_id, &text, reply_to_id).await?;
                let last_id = sent_messages.last().map(|m| m.id.clone());

                Ok(DeliveryReceipt {
                    message_id: msg.id,
                    platform: Platform::Discord,
                    status: DeliveryStatus::Sent,
                    timestamp: OffsetDateTime::now_utc(),
                    platform_message_id: last_id,
                })
            }
            MessageContent::File { url, filename, .. } => {
                let text = format!("Attached: [{}]({})", filename, url);
                let sent_messages = self.send_message(target_id, &text, reply_to_id).await?;
                let last_id = sent_messages.last().map(|m| m.id.clone());

                Ok(DeliveryReceipt {
                    message_id: msg.id,
                    platform: Platform::Discord,
                    status: DeliveryStatus::Sent,
                    timestamp: OffsetDateTime::now_utc(),
                    platform_message_id: last_id,
                })
            }
            _ => Err(PlatformError::UnsupportedContent(
                "Unsupported Discord content type".into(),
            )),
        }
    }

    async fn handle_webhook(
        &self,
        payload: serde_json::Value,
    ) -> Result<Vec<IncomingMessage>, PlatformError> {
        let discord_msg: DiscordMessage = serde_json::from_value(payload)
            .map_err(|e| PlatformError::Internal(e.to_string()))?;

        let bot_user = self.bot_user().await;
        let config = self.base.config.clone().unwrap_or_default();

        let (should_process, cleaned_content) =
            should_process_message(&discord_msg, bot_user.as_ref(), &config);

        if !should_process {
            return Ok(vec![]);
        }

        let session_id = session_id_from_discord(
            &discord_msg.channel_id,
            discord_msg.thread_id.as_deref(),
        );

        let incoming = IncomingMessage {
            id: MessageId::new(),
            session_id,
            platform: Platform::Discord,
            content: MessageContent::Text {
                text: cleaned_content,
            },
            timestamp: OffsetDateTime::now_utc(),
            raw: Some(serde_json::to_value(&discord_msg).unwrap_or_default()),
        };

        Ok(vec![incoming])
    }

    async fn health_check(&self) -> Result<(), PlatformError> {
        self.verify_and_get_bot_user().await?;
        Ok(())
    }
}

pub mod prelude {
    pub use crate::*;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_discord_chunk_message_2000() {
        // 1. Under limit message -> exactly 1 chunk
        let short_msg = "Hello Discord bot!";
        let chunks = chunk_message(short_msg, DISCORD_MAX_MESSAGE_CHARS);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0], short_msg);

        // 2. Exactly 2000 characters
        let exactly_2000 = "a".repeat(2000);
        let chunks_exact = chunk_message(&exactly_2000, DISCORD_MAX_MESSAGE_CHARS);
        assert_eq!(chunks_exact.len(), 1);
        assert_eq!(chunks_exact[0].len(), 2000);

        // 3. Multi-chunk message split cleanly at newlines/spaces
        let long_msg = format!("{}\n\n{}", "Paragraph 1: ".to_string() + &"x".repeat(1500), "Paragraph 2: ".to_string() + &"y".repeat(1000));
        let chunks_multi = chunk_message(&long_msg, DISCORD_MAX_MESSAGE_CHARS);
        assert_eq!(chunks_multi.len(), 2);
        assert!(chunks_multi[0].len() <= DISCORD_MAX_MESSAGE_CHARS);
        assert!(chunks_multi[1].len() <= DISCORD_MAX_MESSAGE_CHARS);
        assert!(chunks_multi[0].starts_with("Paragraph 1: "));
        assert!(chunks_multi[1].starts_with("Paragraph 2: "));

        // 4. Degenerate turn exceeding MAX_SPLIT_MESSAGES capped with warning notice
        let huge_msg = "Super long spam message block.\n".repeat(1000); // ~31,000 chars
        let capped = chunk_message(&huge_msg, DISCORD_MAX_MESSAGE_CHARS);
        assert_eq!(capped.len(), MAX_SPLIT_MESSAGES);
        assert!(capped.last().unwrap().contains("Response truncated"));
        assert!(capped.last().unwrap().contains("exceeded the delivery limit"));
    }

    #[test]
    fn test_discord_mention_filtering() {
        let bot = DiscordUser {
            id: "123456789012345678".to_string(),
            username: "HermesBot".to_string(),
            bot: true,
            discriminator: Some("0001".to_string()),
            avatar: None,
        };

        let config = DiscordConfig {
            bot_token: None,
            client_id: Some("123456789012345678".to_string()),
            guild_id: Some("999999999999999999".to_string()),
            require_mention: true,
            free_response_channels: ["111222333444555666".to_string()].into_iter().collect(),
            allowed_channels: ["111222333444555666".to_string(), "777888999000111222".to_string()].into_iter().collect(),
            ignored_channels: ["888888888888888888".to_string()].into_iter().collect(),
            base: BasePlatformConfig {
                enabled: true,
                rate_limit: None,
                webhook: None,
                custom: serde_json::Value::Null,
            },
        };

        // Case 1: Inbound DM (no guild_id) passes freely without mention
        let dm_msg = DiscordMessage {
            id: "1".to_string(),
            channel_id: "333333333333333333".to_string(),
            guild_id: None,
            author: DiscordUser {
                id: "555".to_string(),
                username: "alice".to_string(),
                bot: false,
                discriminator: None,
                avatar: None,
            },
            content: "hello bot in dm".to_string(),
            attachments: vec![],
            embeds: vec![],
            mentions: vec![],
            message_reference: None,
            thread_id: None,
        };
        let (admit_dm, content_dm) = should_process_message(&dm_msg, Some(&bot), &config);
        assert!(admit_dm);
        assert_eq!(content_dm, "hello bot in dm");

        // Case 2: Inbound Guild channel without mention -> Rejected
        let guild_msg_no_mention = DiscordMessage {
            id: "2".to_string(),
            channel_id: "777888999000111222".to_string(),
            guild_id: Some("999999999999999999".to_string()),
            author: DiscordUser {
                id: "555".to_string(),
                username: "alice".to_string(),
                bot: false,
                discriminator: None,
                avatar: None,
            },
            content: "chatting in channel without mention".to_string(),
            attachments: vec![],
            embeds: vec![],
            mentions: vec![],
            message_reference: None,
            thread_id: None,
        };
        let (admit_guild_no, _) = should_process_message(&guild_msg_no_mention, Some(&bot), &config);
        assert!(!admit_guild_no);

        // Case 3: Inbound Guild channel with `<@bot_id>` mention -> Admitted and stripped
        let guild_msg_mention = DiscordMessage {
            id: "3".to_string(),
            channel_id: "777888999000111222".to_string(),
            guild_id: Some("999999999999999999".to_string()),
            author: DiscordUser {
                id: "555".to_string(),
                username: "alice".to_string(),
                bot: false,
                discriminator: None,
                avatar: None,
            },
            content: "<@123456789012345678> run diagnostic status".to_string(),
            attachments: vec![],
            embeds: vec![],
            mentions: vec![bot.clone()],
            message_reference: None,
            thread_id: None,
        };
        let (admit_mention, content_mention) = should_process_message(&guild_msg_mention, Some(&bot), &config);
        assert!(admit_mention);
        assert_eq!(content_mention, "run diagnostic status");

        // Case 4: Free response channel without mention -> Admitted freely
        let free_ch_msg = DiscordMessage {
            id: "4".to_string(),
            channel_id: "111222333444555666".to_string(),
            guild_id: Some("999999999999999999".to_string()),
            author: DiscordUser {
                id: "555".to_string(),
                username: "alice".to_string(),
                bot: false,
                discriminator: None,
                avatar: None,
            },
            content: "status report".to_string(),
            attachments: vec![],
            embeds: vec![],
            mentions: vec![],
            message_reference: None,
            thread_id: None,
        };
        let (admit_free, content_free) = should_process_message(&free_ch_msg, Some(&bot), &config);
        assert!(admit_free);
        assert_eq!(content_free, "status report");

        // Case 5: Ignored channel -> Always rejected even with mention
        let ignored_ch_msg = DiscordMessage {
            id: "5".to_string(),
            channel_id: "888888888888888888".to_string(),
            guild_id: Some("999999999999999999".to_string()),
            author: DiscordUser {
                id: "555".to_string(),
                username: "alice".to_string(),
                bot: false,
                discriminator: None,
                avatar: None,
            },
            content: "<@123456789012345678> hello".to_string(),
            attachments: vec![],
            embeds: vec![],
            mentions: vec![bot.clone()],
            message_reference: None,
            thread_id: None,
        };
        let (admit_ignored, _) = should_process_message(&ignored_ch_msg, Some(&bot), &config);
        assert!(!admit_ignored);

        // Case 6: Own bot message -> Ignored
        let bot_self_msg = DiscordMessage {
            id: "6".to_string(),
            channel_id: "111222333444555666".to_string(),
            guild_id: Some("999999999999999999".to_string()),
            author: bot.clone(),
            content: "I am replying now".to_string(),
            attachments: vec![],
            embeds: vec![],
            mentions: vec![],
            message_reference: None,
            thread_id: None,
        };
        let (admit_self, _) = should_process_message(&bot_self_msg, Some(&bot), &config);
        assert!(!admit_self);
    }

    #[test]
    fn test_discord_session_id_determinism() {
        let channel_id = "112233445566778899";
        let thread_1 = "998877665544332211";
        let thread_2 = "554433221100998877";

        // Same channel + thread produces identical UUID
        let session_a1 = session_id_from_discord(channel_id, Some(thread_1));
        let session_a2 = session_id_from_discord(channel_id, Some(thread_1));
        assert_eq!(session_a1, session_a2);

        // Different thread produces isolated UUID
        let session_b = session_id_from_discord(channel_id, Some(thread_2));
        assert_ne!(session_a1, session_b);

        // Channel without thread produces distinct UUID
        let session_no_thread = session_id_from_discord(channel_id, None);
        assert_ne!(session_a1, session_no_thread);
        assert_ne!(session_b, session_no_thread);

        // Verify roundtrip recovery of channel and thread IDs
        let (recovered_chan, recovered_thread) = discord_channel_and_thread_from_session(session_a1);
        assert_eq!(recovered_chan, channel_id);
        assert_eq!(recovered_thread.as_deref(), Some(thread_1));

        let (rec_chan_none, rec_thread_none) = discord_channel_and_thread_from_session(session_no_thread);
        assert_eq!(rec_chan_none, channel_id);
        assert_eq!(rec_thread_none, None);
    }

    #[test]
    fn test_discord_api_serialization() {
        let req = CreateMessageRequest {
            content: "Hello from Hermes Gateway!".to_string(),
            message_reference: Some(DiscordMessageReference {
                message_id: Some("123456789".to_string()),
                channel_id: Some("987654321".to_string()),
                guild_id: None,
                fail_if_not_exists: false,
            }),
            embeds: vec![DiscordEmbed {
                title: Some("Execution Report".to_string()),
                embed_type: Some("rich".to_string()),
                description: Some("Platform adapter tests passed".to_string()),
                url: Some("https://example.com".to_string()),
            }],
            allowed_mentions: Some(AllowedMentions {
                parse: vec!["users".to_string()],
                users: vec!["111222".to_string()],
                roles: vec![],
                replied_user: true,
            }),
        };

        let json = serde_json::to_string(&req).expect("Failed to serialize CreateMessageRequest");
        let val: serde_json::Value = serde_json::from_str(&json).expect("Failed to parse json");

        assert_eq!(val["content"], "Hello from Hermes Gateway!");
        assert_eq!(val["message_reference"]["message_id"], "123456789");
        assert_eq!(val["embeds"][0]["title"], "Execution Report");
        assert_eq!(val["allowed_mentions"]["replied_user"], true);

        // Test DiscordUser serialization and deserialization
        let user = DiscordUser {
            id: "1234567890".to_string(),
            username: "tester".to_string(),
            bot: true,
            discriminator: Some("0001".to_string()),
            avatar: Some("avatar_hash_123".to_string()),
        };
        let user_json = serde_json::to_string(&user).unwrap();
        let user_deser: DiscordUser = serde_json::from_str(&user_json).unwrap();
        assert_eq!(user, user_deser);
    }
}
