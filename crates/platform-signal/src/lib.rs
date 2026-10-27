// hermes-platform-signal - Signal platform adapter using signal-cli JSON-RPC
use hermes_platform_adapter::{PlatformAdapter, BaseAdapter, BasePlatformConfig, RateLimitConfig};
use hermes_protocol::{
    Platform, PlatformEvent, PlatformError, DeliveryReceipt, DeliveryStatus,
    IncomingMessage, OutgoingMessage, MessageId, MessageContent,
};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex, oneshot};
use tracing::{info, warn, error, debug};
use uuid::Uuid;
use time::OffsetDateTime;
use signal_cli_jsonrpc_client::{
    connect_tcp, connect_ipc, RpcClient, SubscriptionClientT, SignalMessage, MessageTarget,
    RecvMessage, Envelope, DataMessage, GroupInfo, SentMessage,
};
use jsonrpsee::core::client::Subscription;
use base64;

/// Signal platform configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignalConfig {
    /// Phone number registered with Signal (E.164 format)
    pub phone_number: String,
    
    /// Path to signal-cli binary
    #[serde(default = "default_signal_cli_path")]
    pub signal_cli_path: String,
    
    /// Path to signal-cli config/data directory
    #[serde(default = "default_config_path")]
    pub config_path: String,
    
    /// DBus mode (optional)
    #[serde(default)]
    pub dbus_mode: bool,
    
    /// JSON-RPC connection: TCP host:port
    #[serde(default)]
    pub tcp_address: Option<String>,
    
    /// JSON-RPC connection: Unix socket path
    #[serde(default)]
    pub unix_socket: Option<String>,
    
    /// JSON-RPC connection: HTTP endpoint
    #[serde(default)]
    pub http_endpoint: Option<String>,
    
    /// Timeout for RPC calls in seconds
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
    
    /// Auto-receive mode
    #[serde(default = "default_true")]
    pub auto_receive: bool,
    
    #[serde(flatten)]
    pub base: BasePlatformConfig,
}

fn default_signal_cli_path() -> String {
    "/usr/bin/signal-cli".to_string()
}

fn default_config_path() -> String {
    "/var/lib/signal-cli".to_string()
}

fn default_timeout() -> u64 {
    30
}

fn default_true() -> bool {
    true
}

/// Commands for the Signal client task
enum ClientCommand {
    Send {
        msg: SignalMessage,
        respond: oneshot::Sender<Result<serde_json::Value, PlatformError>>,
    },
    ListGroups {
        respond: oneshot::Sender<Result<Vec<GroupInfo>, PlatformError>>,
    },
    CreateGroup {
        name: String,
        members: Vec<String>,
        description: Option<String>,
        respond: oneshot::Sender<Result<String, PlatformError>>,
    },
    UpdateGroup {
        group_id: String,
        name: Option<String>,
        members: Vec<String>,
        remove_members: Vec<String>,
        respond: oneshot::Sender<Result<(), PlatformError>>,
    },
    SendReaction {
        recipient: String,
        emoji: String,
        target_author: String,
        target_timestamp: i64,
        group_id: Option<String>,
        respond: oneshot::Sender<Result<i64, PlatformError>>,
    },
    GetAttachment {
        attachment_id: String,
        recipient: Option<String>,
        respond: oneshot::Sender<Result<Vec<u8>, PlatformError>>,
    },
    HealthCheck {
        respond: oneshot::Sender<Result<(), PlatformError>>,
    },
    Shutdown,
}

/// Signal platform adapter
#[derive(Debug)]
pub struct SignalAdapter {
    base: BaseAdapter<SignalConfig>,
    command_tx: Arc<Mutex<Option<mpsc::UnboundedSender<ClientCommand>>>>,
    daemon_handle: Arc<Mutex<Option<tokio::process::Child>>>,
    event_sender: Arc<Mutex<Option<mpsc::UnboundedSender<PlatformEvent>>>>,
    phone_number: Arc<Mutex<Option<String>>>,
}

impl SignalAdapter {
    pub fn new() -> Self {
        Self {
            base: BaseAdapter::new(Platform::Signal),
            command_tx: Arc::new(Mutex::new(None)),
            daemon_handle: Arc::new(Mutex::new(None)),
            event_sender: Arc::new(Mutex::new(None)),
            phone_number: Arc::new(Mutex::new(None)),
        }
    }

    /// Build the signal-cli daemon command
    fn build_daemon_command(&self, config: &SignalConfig) -> tokio::process::Command {
        let mut cmd = tokio::process::Command::new(&config.signal_cli_path);
        cmd.arg("-a").arg(&config.phone_number);
        cmd.arg("--config").arg(&config.config_path);
        cmd.arg("daemon");
        
        if let Some(tcp) = &config.tcp_address {
            cmd.arg("--tcp").arg(tcp);
        }
        if let Some(socket) = &config.unix_socket {
            cmd.arg("--socket").arg(socket);
        }
        if let Some(http) = &config.http_endpoint {
            cmd.arg("--http").arg(http);
        }
        if !config.auto_receive {
            cmd.arg("--receive-mode").arg("manual");
        }
        cmd.arg("--no-receive-stdout");
        
        cmd
    }

    /// Start the signal-cli daemon as a subprocess
    async fn start_daemon(&self, config: &SignalConfig) -> Result<tokio::process::Child, PlatformError> {
        let mut cmd = self.build_daemon_command(config);
        cmd.kill_on_drop(true);
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());
        
        let child = cmd.spawn()
            .map_err(|e| PlatformError::Internal(format!("Failed to start signal-cli daemon: {}", e)))?;
        
        info!("Started signal-cli daemon for {}", config.phone_number);
        Ok(child)
    }

    /// Connect to signal-cli JSON-RPC
    async fn connect_rpc(config: &SignalConfig) -> Result<Box<dyn SubscriptionClientT>, PlatformError> {
        if let Some(tcp) = &config.tcp_address {
            let client = connect_tcp(tcp).await
                .map_err(|e| PlatformError::Network(format!("Failed to connect to signal-cli TCP: {}", e)))?;
            return Ok(Box::new(client));
        }
        
        if let Some(socket) = &config.unix_socket {
            let client = connect_ipc(socket).await
                .map_err(|e| PlatformError::Network(format!("Failed to connect to signal-cli Unix socket: {}", e)))?;
            return Ok(Box::new(client));
        }
        
        if let Some(http) = &config.http_endpoint {
            let client = signal_cli_jsonrpc_client::connect_http(http).await
                .map_err(|e| PlatformError::Network(format!("Failed to connect to signal-cli HTTP: {}", e)))?;
            return Ok(Box::new(client));
        }
        
        // Default: try TCP on localhost:7583
        let client = connect_tcp("127.0.0.1:7583").await
            .map_err(|e| PlatformError::Network(format!("Failed to connect to signal-cli default TCP: {}", e)))?;
        Ok(Box::new(client))
    }

    /// Spawn the client task that owns the JSON-RPC client
    fn spawn_client_task(
        config: SignalConfig,
        command_rx: mpsc::UnboundedReceiver<ClientCommand>,
        event_sender: mpsc::UnboundedSender<PlatformEvent>,
        phone_number: String,
    ) {
        tokio::spawn(async move {
            // Connect to signal-cli
            let mut client = match Self::connect_rpc(&config).await {
                Ok(c) => c,
                Err(e) => {
                    error!("Failed to connect to signal-cli: {}", e);
                    return;
                }
            };
            
            info!("Connected to signal-cli for {}", phone_number);
            
            // Test connection
            if let Err(e) = client.version().await {
                error!("Failed to get version: {}", e);
                return;
            }
            
            // Subscribe to incoming messages if auto_receive
            let mut subscription = if config.auto_receive {
                match client.subscribe_receive(Some(phone_number.clone())).await {
                    Ok(sub) => {
                        info!("Subscribed to Signal messages for {}", phone_number);
                        Some(sub)
                    }
                    Err(e) => {
                        error!("Failed to subscribe to Signal messages: {}", e);
                        None
                    }
                }
            } else {
                None
            };
            
            let mut command_rx = command_rx;
            
            loop {
                tokio::select! {
                    // Handle incoming commands
                    cmd = command_rx.recv() => {
                        match cmd {
                            Some(ClientCommand::Send { msg, respond }) => {
                                let result = client.send(msg).await
                                    .map_err(|e| PlatformError::Api {
                                        code: "SEND_FAILED".to_string(),
                                        message: e.to_string(),
                                    });
                                let _ = respond.send(result);
                            }
                            Some(ClientCommand::ListGroups { respond }) => {
                                let result = client.list_groups(Some(phone_number.clone())).await
                                    .map_err(|e| PlatformError::Api {
                                        code: "LIST_GROUPS_FAILED".to_string(),
                                        message: e.to_string(),
                                    });
                                let _ = respond.send(result);
                            }
                            Some(ClientCommand::CreateGroup { name, members, description, respond }) => {
                                let result = client.create_group(
                                    Some(phone_number.clone()),
                                    name,
                                    members,
                                    description,
                                    None,
                                    None,
                                ).await
                                    .map_err(|e| PlatformError::Api {
                                        code: "CREATE_GROUP_FAILED".to_string(),
                                        message: e.to_string(),
                                    });
                                let _ = respond.send(result);
                            }
                            Some(ClientCommand::UpdateGroup { group_id, name, members, remove_members, respond }) => {
                                let result = client.update_group(
                                    Some(phone_number.clone()),
                                    Some(group_id),
                                    name,
                                    None,
                                    members,
                                    remove_members,
                                    vec![],
                                    vec![],
                                    vec![],
                                    vec![],
                                    false,
                                    None,
                                    None,
                                    None,
                                    None,
                                    None,
                                    None,
                                    None,
                                ).await
                                    .map_err(|e| PlatformError::Api {
                                        code: "UPDATE_GROUP_FAILED".to_string(),
                                        message: e.to_string(),
                                    });
                                let _ = respond.send(result);
                            }
                            Some(ClientCommand::SendReaction { recipient, emoji, target_author, target_timestamp, group_id, respond }) => {
                                let result = client.send_reaction(
                                    Some(phone_number.clone()),
                                    recipient,
                                    emoji,
                                    target_author,
                                    target_timestamp,
                                    false,
                                    group_id.unwrap_or_default(),
                                ).await
                                    .map_err(|e| PlatformError::Api {
                                        code: "REACTION_FAILED".to_string(),
                                        message: e.to_string(),
                                    });
                                let _ = respond.send(result);
                            }
                            Some(ClientCommand::GetAttachment { attachment_id, recipient, respond }) => {
                                let result = client.get_attachment(
                                    Some(phone_number.clone()),
                                    attachment_id,
                                    recipient,
                                    None,
                                ).await
                                    .map_err(|e| PlatformError::Api {
                                        code: "GET_ATTACHMENT_FAILED".to_string(),
                                        message: e.to_string(),
                                    })
                                    .and_then(|data| {
                                        let base64_str = data.as_str()
                                            .ok_or_else(|| PlatformError::Internal("Invalid attachment data".into()))?;
                                        base64::decode(base64_str)
                                            .map_err(|e| PlatformError::Internal(format!("Failed to decode attachment: {}", e)))
                                    });
                                let _ = respond.send(result);
                            }
                            Some(ClientCommand::HealthCheck { respond }) => {
                                let result = client.version().await
                                    .map_err(|e| PlatformError::Network(e.to_string()))
                                    .map(|_| ());
                                let _ = respond.send(result);
                            }
                            Some(ClientCommand::Shutdown) => {
                                info!("Shutting down Signal client task");
                                break;
                            }
                            None => {
                                // Channel closed
                                break;
                            }
                        }
                    }
                    // Handle incoming messages from subscription
                    msg = async {
                        if let Some(sub) = &mut subscription {
                            sub.next().await
                        } else {
                            std::future::pending().await
                        }
                    } => {
                        if let Some(Ok(recv_msg)) = msg {
                            if let Some(envelope) = recv_msg.envelope {
                                let adapter = SignalAdapter::new();
                                if let Ok(messages) = adapter.convert_envelope(envelope, &phone_number) {
                                    for msg in messages {
                                        let _ = event_sender.send(PlatformEvent::Incoming(msg));
                                    }
                                }
                            }
                        } else if let Some(Err(e)) = msg {
                            error!("Subscription error: {}", e);
                            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                            
                            // Try to resubscribe
                            subscription = match client.subscribe_receive(Some(phone_number.clone())).await {
                                Ok(sub) => {
                                    info!("Re-subscribed to Signal messages for {}", phone_number);
                                    Some(sub)
                                }
                                Err(e) => {
                                    error!("Failed to re-subscribe: {}", e);
                                    None
                                }
                            };
                        } else {
                            warn!("Subscription ended");
                            subscription = None;
                        }
                    }
                }
            }
        });
    }

    /// Convert signal-cli envelope to IncomingMessage
    fn convert_envelope(&self, envelope: Envelope, phone_number: &str) -> Result<Vec<IncomingMessage>, PlatformError> {
        let mut messages = Vec::new();
        
        // Extract session ID from source
        let session_id = self.session_id_from_source(&envelope.source);
        
        // Handle different message types
        if let Some(data_msg) = envelope.data_message {
            let content = self.convert_data_message(data_msg, &envelope)?;
            let incoming = IncomingMessage {
                id: MessageId::new(),
                session_id,
                platform: Platform::Signal,
                content,
                timestamp: OffsetDateTime::from_unix_timestamp((envelope.timestamp / 1000) as i64)
                    .unwrap_or_else(|_| OffsetDateTime::now_utc()),
                raw: Some(serde_json::to_value(&envelope).unwrap_or(serde_json::Value::Null)),
            };
            messages.push(incoming);
        }
        
        if let Some(sync_msg) = envelope.sync_message {
            if let Some(sent_msg) = sync_msg.sent_message {
                let content = self.convert_sent_message(sent_msg)?;
                let incoming = IncomingMessage {
                    id: MessageId::new(),
                    session_id,
                    platform: Platform::Signal,
                    content,
                    timestamp: OffsetDateTime::from_unix_timestamp((envelope.timestamp / 1000) as i64)
                        .unwrap_or_else(|_| OffsetDateTime::now_utc()),
                    raw: Some(serde_json::to_value(&envelope).unwrap_or(serde_json::Value::Null)),
                };
                messages.push(incoming);
            }
        }
    
    if let Some(receipt) = envelope.receipt_message {
        let content = MessageContent::System {
            event: "receipt".to_string(),
            data: serde_json::json!({
                "type": receipt.receipt_type,
                "recipient": receipt.recipient,
                "timestamp": receipt.timestamp,
            }),
        };
        let incoming = IncomingMessage {
            id: MessageId::new(),
            session_id,
            platform: Platform::Signal,
            content,
            timestamp: OffsetDateTime::now_utc(),
            raw: Some(serde_json::to_value(&receipt).unwrap_or_default()),
        };
        messages.push(incoming);
    }
    
    if let Some(typing) = envelope.typing_message {
        let content = MessageContent::System {
            event: "typing".to_string(),
            data: serde_json::json!({
                "recipient": typing.recipient,
                "action": typing.action,
            }),
        };
        let incoming = IncomingMessage {
            id: MessageId::new(),
            session_id,
            platform: Platform::Signal,
            content,
            timestamp: OffsetDateTime::now_utc(),
            raw: Some(serde_json::to_value(&typing).unwrap_or_default()),
        };
        messages.push(incoming);
    }
    
    Ok(messages)
    }

    /// Convert DataMessage to MessageContent
    fn convert_data_message(&self, data: DataMessage, _envelope: &Envelope) -> Result<MessageContent, PlatformError> {
        // Handle reactions
        if let Some(reaction) = data.reaction {
            return Ok(MessageContent::Reaction {
                emoji: reaction.emoji,
                target_id: MessageId(Uuid::nil()),
            });
        }
        
        // Handle sticker
        if let Some(sticker) = data.sticker {
            return Ok(MessageContent::Sticker {
                sticker_id: format!("{}:{}", sticker.pack_id, sticker.sticker_id),
            });
        }
        
        // Handle text message
        if let Some(text) = data.message {
            // Check for attachments
            if let Some(attachments) = data.attachments {
                if !attachments.is_empty() {
                    let att = &attachments[0];
                    let mime = att.content_type.clone().unwrap_or_else(|| "application/octet-stream".to_string());
                    if mime.starts_with("image/") {
                        return Ok(MessageContent::Image {
                            url: att.id.clone(),
                            caption: Some(text),
                        });
                    } else if mime.starts_with("video/") {
                        return Ok(MessageContent::Video {
                            url: att.id.clone(),
                            duration: None,
                        });
                    } else if mime.starts_with("audio/") {
                        return Ok(MessageContent::Audio {
                            url: att.id.clone(),
                            duration: None,
                        });
                    } else {
                        return Ok(MessageContent::File {
                            url: att.id.clone(),
                            filename: att.filename.clone().unwrap_or_else(|| "file".to_string()),
                            mime_type: Some(mime),
                        });
                    }
                }
            }
            return Ok(MessageContent::Text { text });
        }
        
        // Handle attachments without text
        if let Some(attachments) = data.attachments {
            if let Some(att) = attachments.first() {
                let mime = att.content_type.clone().unwrap_or_else(|| "application/octet-stream".to_string());
                if mime.starts_with("image/") {
                    return Ok(MessageContent::Image {
                        url: att.id.clone(),
                        caption: None,
                    });
                } else if mime.starts_with("video/") {
                    return Ok(MessageContent::Video {
                        url: att.id.clone(),
                        duration: None,
                    });
                } else if mime.starts_with("audio/") {
                    return Ok(MessageContent::Audio {
                        url: att.id.clone(),
                        duration: None,
                    });
                } else {
                    return Ok(MessageContent::File {
                        url: att.id.clone(),
                        filename: att.filename.clone().unwrap_or_else(|| "file".to_string()),
                        mime_type: Some(mime),
                    });
                }
            }
        }
        
        Ok(MessageContent::Unknown { data: serde_json::to_value(data).unwrap_or_default() })
    }

    /// Convert sent message (sync) to MessageContent
    fn convert_sent_message(&self, sent: signal_cli_jsonrpc_client::SentMessage) -> Result<MessageContent, PlatformError> {
        if let Some(text) = sent.message {
            return Ok(MessageContent::Text { text });
        }
        Ok(MessageContent::Unknown { data: serde_json::to_value(sent).unwrap_or_default() })
    }

    /// Generate session ID from source phone number
    fn session_id_from_source(&self, source: &str) -> Uuid {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        source.hash(&mut hasher);
        let hash = hasher.finish();
        let mut bytes = [0u8; 16];
        bytes[..8].copy_from_slice(&hash.to_le_bytes());
        Uuid::from_bytes(bytes)
    }

    /// Convert OutgoingMessage to SignalMessage
    fn convert_outgoing(&self, msg: &OutgoingMessage) -> Result<SignalMessage, PlatformError> {
        let chat_id = self.chat_id_from_session(msg.session_id)?;
        let phone_number = self.phone_number.blocking_lock().as_ref().cloned().unwrap_or_default();
        
        let target = if chat_id.starts_with("group.") {
            MessageTarget::Group(chat_id)
        } else {
            MessageTarget::Recipients(vec![chat_id])
        };
        
        let signal_msg = match &msg.content {
            MessageContent::Text { text } => {
                SignalMessage {
                    sender: phone_number,
                    target,
                    message: text.clone(),
                    attachments: vec![],
                }
            }
            MessageContent::Image { url, caption } => {
                SignalMessage {
                    sender: phone_number,
                    target,
                    message: caption.clone().unwrap_or_default(),
                    attachments: vec![url.clone()],
                }
            }
            MessageContent::File { url, .. } => {
                SignalMessage {
                    sender: phone_number,
                    target,
                    message: String::new(),
                    attachments: vec![url.clone()],
                }
            }
            MessageContent::Video { url, .. } => {
                SignalMessage {
                    sender: phone_number,
                    target,
                    message: String::new(),
                    attachments: vec![url.clone()],
                }
            }
            MessageContent::Audio { url, .. } => {
                SignalMessage {
                    sender: phone_number,
                    target,
                    message: String::new(),
                    attachments: vec![url.clone()],
                }
            }
            MessageContent::Sticker { sticker_id } => {
                SignalMessage {
                    sender: phone_number,
                    target,
                    message: String::new(),
                    attachments: vec![],
                }
            }
            MessageContent::Reaction { .. } => {
                return Err(PlatformError::UnsupportedContent("Reactions need target message info".into()));
            }
            _ => {
                return Err(PlatformError::UnsupportedContent(format!("Content type not supported: {:?}", msg.content)));
            }
        };
        
        Ok(signal_msg)
    }

    /// Extract chat ID from session UUID
    fn chat_id_from_session(&self, session_id: Uuid) -> Result<String, PlatformError> {
        let bytes = session_id.as_bytes();
        let hash = u64::from_le_bytes(bytes[..8].try_into().map_err(|_| PlatformError::Internal("Invalid session ID".into()))?);
        Ok(format!("+{}", hash))
    }

    /// Send a command to the client task and wait for response
    async fn send_command<F, T>(&self, make_cmd: F) -> Result<T, PlatformError>
    where
        F: FnOnce(oneshot::Sender<Result<T, PlatformError>>) -> ClientCommand,
    {
        let (tx, rx) = oneshot::channel();
        let cmd = make_cmd(tx);
        
        let cmd_tx = self.command_tx.lock().await;
        if let Some(tx) = cmd_tx.as_ref() {
            tx.send(cmd).map_err(|_| PlatformError::Internal("Client task channel closed".into()))?;
        } else {
            return Err(PlatformError::Internal("Not initialized".into()));
        }
        drop(cmd_tx);
        
        rx.await
            .map_err(|_| PlatformError::Internal("Client task response channel closed".into()))?
    }
}

#[async_trait]
impl PlatformAdapter for SignalAdapter {
    type Config = SignalConfig;
    type IncomingEvent = IncomingMessage;
    type OutgoingMessage = OutgoingMessage;

    fn platform(&self) -> Platform {
        Platform::Signal
    }

    async fn initialize(&mut self, config: Self::Config) -> Result<(), PlatformError> {
        // Validate config
        if config.phone_number.is_empty() {
            return Err(PlatformError::Auth("Phone number is required".into()));
        }
        
        self.base = BaseAdapter::new(Platform::Signal).with_config(config.clone());
        
        if let Some(rate_limit) = config.base.rate_limit.clone() {
            self.base.setup_rate_limit(Some(rate_limit));
        }
        
        // Store phone number
        *self.phone_number.lock().await = Some(config.phone_number.clone());
        
        info!("Signal adapter initialized for {}", config.phone_number);
        Ok(())
    }

    async fn start(&mut self, sender: mpsc::UnboundedSender<PlatformEvent>) -> Result<(), PlatformError> {
        let config = self.base.config.as_ref().expect("Not initialized").clone();
        let phone_number = config.phone_number.clone();
        
        // Start signal-cli daemon if not using external endpoint
        if config.tcp_address.is_none() && config.unix_socket.is_none() && config.http_endpoint.is_none() {
            let child = self.start_daemon(&config).await?;
            let mut daemon_handle = self.daemon_handle.lock().await;
            *daemon_handle = Some(child);
        }
        
        // Create command channel
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        *self.command_tx.lock().await = Some(cmd_tx);
        *self.event_sender.lock().await = Some(sender.clone());
        
        // Spawn client task
        Self::spawn_client_task(config, cmd_rx, sender, phone_number);
        
        self.base.set_sender(sender);
        self.base.started = true;
        
        info!("Signal adapter started for {}", phone_number);
        Ok(())
    }

    async fn stop(&mut self) -> Result<(), PlatformError> {
        self.base.started = false;
        
        // Send shutdown command
        if let Some(cmd_tx) = self.command_tx.lock().await.take() {
            let _ = cmd_tx.send(ClientCommand::Shutdown);
        }
        
        // Stop daemon
        let mut daemon_handle = self.daemon_handle.lock().await;
        if let Some(mut child) = daemon_handle.take() {
            let _ = child.kill().await;
            info!("Signal daemon stopped");
        }
        
        *self.event_sender.lock().await = None;
        *self.phone_number.lock().await = None;
        
        info!("Signal adapter stopped");
        Ok(())
    }

    async fn send(&self, msg: Self::OutgoingMessage) -> Result<DeliveryReceipt, PlatformError> {
        // Rate limit
        if let Some(limiter) = &self.base.rate_limiter {
            limiter.acquire().await?;
        }
        
        let signal_msg = self.convert_outgoing(&msg)?;
        
        // Send message via client task
        let result: serde_json::Value = self.send_command(|respond| ClientCommand::Send {
            msg: signal_msg,
            respond,
        }).await?;
        
        // Parse result for timestamp
        let timestamp = result.get("timestamp").and_then(|v| v.as_str())
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap_or_else(|| OffsetDateTime::now_utc().unix_timestamp());
        
        Ok(DeliveryReceipt {
            message_id: msg.id,
            platform: Platform::Signal,
            status: DeliveryStatus::Sent,
            timestamp: OffsetDateTime::from_unix_timestamp(timestamp).unwrap_or_else(|_| OffsetDateTime::now_utc()),
            platform_message_id: Some(timestamp.to_string()),
        })
    }

    async fn handle_webhook(&self, _payload: serde_json::Value) -> Result<Vec<IncomingMessage>, PlatformError> {
        Err(PlatformError::UnsupportedContent("Signal adapter uses JSON-RPC subscription, not webhooks".into()))
    }

    async fn health_check(&self) -> Result<(), PlatformError> {
        self.send_command(|respond| ClientCommand::HealthCheck { respond }).await
    }
}

impl Default for SignalAdapter {
    fn default() -> Self {
        Self::new()
    }
}

/// Group management extensions
impl SignalAdapter {
    /// List all groups
    pub async fn list_groups(&self) -> Result<Vec<GroupInfo>, PlatformError> {
        self.send_command(|respond| ClientCommand::ListGroups { respond }).await
    }
    
    /// Create a new group
    pub async fn create_group(&self, name: String, members: Vec<String>, description: Option<String>) -> Result<String, PlatformError> {
        self.send_command(|respond| ClientCommand::CreateGroup {
            name,
            members,
            description,
            respond,
        }).await
    }
    
    /// Update group (add/remove members, change name, etc.)
    pub async fn update_group(&self, group_id: String, name: Option<String>, members: Vec<String>, remove_members: Vec<String>) -> Result<(), PlatformError> {
        self.send_command(|respond| ClientCommand::UpdateGroup {
            group_id,
            name,
            members,
            remove_members,
            respond,
        }).await
    }
    
    /// Send reaction to a message
    pub async fn send_reaction(&self, recipient: String, emoji: String, target_author: String, target_timestamp: i64, group_id: Option<String>) -> Result<i64, PlatformError> {
        self.send_command(|respond| ClientCommand::SendReaction {
            recipient,
            emoji,
            target_author,
            target_timestamp,
            group_id,
            respond,
        }).await
    }
    
    /// Download attachment
    pub async fn get_attachment(&self, attachment_id: String, recipient: Option<String>) -> Result<Vec<u8>, PlatformError> {
        self.send_command(|respond| ClientCommand::GetAttachment {
            attachment_id,
            recipient,
            respond,
        }).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hermes_protocol::{MessageContent, MessageId};
    use uuid::Uuid;
    
    #[test]
    fn test_session_id_generation() {
        let adapter = SignalAdapter::new();
        let source = "+15551234567";
        let session_id = adapter.session_id_from_source(source);
        
        // Same source should produce same session ID
        let session_id2 = adapter.session_id_from_source(source);
        assert_eq!(session_id, session_id2);
        
        // Different source should produce different session ID
        let session_id3 = adapter.session_id_from_source("+15559876543");
        assert_ne!(session_id, session_id3);
    }
    
    #[test]
    fn test_convert_outgoing_text() {
        let adapter = SignalAdapter::new();
        let session_id = Uuid::now_v7();
        let msg = OutgoingMessage {
            id: MessageId::new(),
            session_id,
            platform: Platform::Signal,
            content: MessageContent::Text { text: "Hello, Signal!".to_string() },
            reply_to: None,
            metadata: HashMap::new(),
        };
        
        let result = adapter.convert_outgoing(&msg);
        assert!(result.is_ok());
        let signal_msg = result.unwrap();
        assert!(signal_msg.message.is_some());
        assert_eq!(signal_msg.message.unwrap(), "Hello, Signal!");
    }
    
    #[test]
    fn test_config_defaults() {
        let config = SignalConfig {
            phone_number: "+15551234567".to_string(),
            signal_cli_path: "/usr/bin/signal-cli".to_string(),
            config_path: "/var/lib/signal-cli".to_string(),
            dbus_mode: false,
            tcp_address: None,
            unix_socket: None,
            http_endpoint: None,
            timeout_secs: 30,
            auto_receive: true,
            base: BasePlatformConfig {
                enabled: true,
                rate_limit: None,
                webhook: None,
                custom: serde_json::Value::Null,
            },
        };
        
        assert_eq!(config.phone_number, "+15551234567");
        assert_eq!(config.timeout_secs, 30);
        assert!(config.auto_receive);
    }
}