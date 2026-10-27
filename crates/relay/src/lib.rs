// hermes-relay - WebSocket transport and message relay
use hermes_protocol::{Platform, PlatformEvent, PlatformError, IncomingMessage, OutgoingMessage, DeliveryReceipt, MessageId, DeliveryStatus};
use hermes_platform_adapter::{PlatformAdapter, DynPlatformAdapter, RateLimiter};
use async_trait::async_trait;
use axum::{
    extract::{ws::WebSocket, State, WebSocketUpgrade},
    response::Response,
    routing::get,
    Router,
};
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock};
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message as TungsteniteMessage};
use axum::extract::ws::Message as AxumWsMessage;
use tracing::{info, warn, error};
use uuid::Uuid;

/// WebSocket message types for relay protocol
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RelayMessage {
    Connect { session_id: Uuid, platform: Platform, profile: String },
    Disconnect { session_id: Uuid },
    Incoming(IncomingMessage),
    Outgoing(OutgoingMessage),
    Ack { message_id: MessageId, status: DeliveryStatus },
    Ping,
    Pong,
    Error { code: String, message: String },
}

/// Relay transport trait
#[async_trait]
pub trait RelayTransport: Send + Sync {
    async fn connect(&mut self, session_id: Uuid, platform: Platform, profile: String) -> Result<(), RelayError>;
    async fn disconnect(&mut self, session_id: Uuid) -> Result<(), RelayError>;
    async fn send(&self, session_id: Uuid, msg: OutgoingMessage) -> Result<DeliveryReceipt, RelayError>;
    async fn receive(&mut self) -> Result<Option<RelayMessage>, RelayError>;
    async fn health_check(&self) -> Result<(), RelayError>;
}

#[derive(Debug, thiserror::Error)]
pub enum RelayError {
    #[error("Connection failed: {0}")]
    Connection(String),
    #[error("Protocol error: {0}")]
    Protocol(String),
    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("Session not found: {0}")]
    SessionNotFound(Uuid),
    #[error("Transport closed")]
    Closed,
    #[error("Internal error: {0}")]
    Internal(String),
}

/// WebSocket relay server (for TUI, desktop, ACP connections)
pub struct WsRelayServer {
    sessions: Arc<RwLock<HashMap<Uuid, WsSession>>>,
    event_tx: mpsc::UnboundedSender<PlatformEvent>,
    config: WsRelayConfig,
}

#[derive(Debug, Clone)]
pub struct WsRelayConfig {
    pub host: String,
    pub port: u16,
    pub path: String,
    pub max_connections: usize,
    pub ping_interval_secs: u64,
    pub connection_timeout_secs: u64,
}

impl Default for WsRelayConfig {
    fn default() -> Self {
        Self {
            host: "0.0.0.0".to_string(),
            port: 8080,
            path: "/ws".to_string(),
            max_connections: 1000,
            ping_interval_secs: 30,
            connection_timeout_secs: 60,
        }
    }
}

#[allow(dead_code)]
struct WsSession {
    id: Uuid,
    platform: Platform,
    profile: String,
    sender: mpsc::UnboundedSender<RelayMessage>,
    connected_at: time::OffsetDateTime,
    last_ping: time::OffsetDateTime,
}

impl WsRelayServer {
    pub fn new(config: WsRelayConfig, event_tx: mpsc::UnboundedSender<PlatformEvent>) -> Self {
        Self {
            sessions: Arc::new(RwLock::new(HashMap::new())),
            event_tx,
            config,
        }
    }

    pub async fn run(self: Arc<Self>) -> Result<(), RelayError> {
        let config = self.config.clone();
        let app = Router::new()
            .route(&config.path, get(ws_handler))
            .with_state(self);

        let addr = format!("{}:{}", config.host, config.port);
        let listener = tokio::net::TcpListener::bind(&addr).await
            .map_err(|e| RelayError::Connection(e.to_string()))?;
        
        info!("WebSocket relay listening on {}", addr);
        axum::serve(listener, app).await
            .map_err(|e| RelayError::Internal(e.to_string()))?;
        Ok(())
    }

    async fn handle_connection(&self, ws: WebSocket, session_id: Uuid, platform: Platform, profile: String) {
        let (mut ws_sender, mut ws_receiver) = ws.split();
        let (tx, mut rx) = mpsc::unbounded_channel::<RelayMessage>();

        // Register session
        let session = WsSession {
            id: session_id,
            platform,
            profile: profile.clone(),
            sender: tx.clone(),
            connected_at: time::OffsetDateTime::now_utc(),
            last_ping: time::OffsetDateTime::now_utc(),
        };
        self.sessions.write().await.insert(session_id, session);

        // Spawn sender task
        let sessions_clone = self.sessions.clone();
        let sender_task = tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                let json = match serde_json::to_string(&msg) {
                    Ok(j) => j,
                    Err(e) => {
                        error!("Failed to serialize relay message: {}", e);
                        continue;
                    }
                };
                if ws_sender.send(AxumWsMessage::Text(json)).await.is_err() {
                    break;
                }
            }
            // Cleanup on disconnect
            sessions_clone.write().await.remove(&session_id);
        });

        // Handle incoming messages
        let event_tx = self.event_tx.clone();
        let sessions_clone = self.sessions.clone();
        
        let receiver_task = tokio::spawn(async move {
            while let Some(msg) = ws_receiver.next().await {
                match msg {
                    Ok(AxumWsMessage::Text(text)) => {
                        if let Ok(relay_msg) = serde_json::from_str::<RelayMessage>(&text) {
                            match relay_msg {
                                RelayMessage::Incoming(incoming) => {
                                    let _ = event_tx.send(PlatformEvent::Incoming(incoming));
                                }
                                RelayMessage::Ping => {
                                    let _ = tx.send(RelayMessage::Pong);
                                    if let Some(session) = sessions_clone.write().await.get_mut(&session_id) {
                                        session.last_ping = time::OffsetDateTime::now_utc();
                                    }
                                }
                                RelayMessage::Disconnect { .. } => break,
                                _ => {}
                            }
                        }
                    }
                    Ok(AxumWsMessage::Close(_)) => break,
                    Err(e) => {
                        warn!("WebSocket error: {}", e);
                        break;
                    }
                    _ => {}
                }
            }
            // Cleanup
            sessions_clone.write().await.remove(&session_id);
            let _ = event_tx.send(PlatformEvent::SessionDeleted(session_id));
            sender_task.abort(); // sender_task is still available here
        });

        // Wait for receiver task to complete
        let _ = receiver_task.await;
    }

    pub async fn send_to_session(&self, session_id: Uuid, msg: OutgoingMessage) -> Result<DeliveryReceipt, RelayError> {
        let sessions = self.sessions.read().await;
        if let Some(session) = sessions.get(&session_id) {
            let relay_msg = RelayMessage::Outgoing(msg.clone());
            session.sender.send(relay_msg)
                .map_err(|_| RelayError::Closed)?;
            Ok(DeliveryReceipt {
                message_id: msg.id,
                platform: msg.platform,
                status: DeliveryStatus::Sent,
                timestamp: time::OffsetDateTime::now_utc(),
                platform_message_id: None,
            })
        } else {
            Err(RelayError::SessionNotFound(session_id))
        }
    }

    pub async fn broadcast(&self, msg: OutgoingMessage) -> Vec<Result<DeliveryReceipt, RelayError>> {
        let sessions = self.sessions.read().await;
        let mut results = Vec::new();
        for (_key, session) in sessions.iter() {
            let relay_msg = RelayMessage::Outgoing(msg.clone());
            let result = session.sender.send(relay_msg)
                .map(|_| DeliveryReceipt {
                    message_id: msg.id,
                    platform: msg.platform,
                    status: DeliveryStatus::Sent,
                    timestamp: time::OffsetDateTime::now_utc(),
                    platform_message_id: None,
                })
                .map_err(|_| RelayError::Closed);
            results.push(result);
        }
        results
    }

    pub async fn session_count(&self) -> usize {
        self.sessions.read().await.len()
    }
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    State(server): State<Arc<WsRelayServer>>,
    axum::extract::Query(params): axum::extract::Query<HashMap<String, String>>,
) -> Response {
    let session_id = params.get("session_id")
        .and_then(|s| Uuid::parse_str(s).ok())
        .unwrap_or_else(Uuid::now_v7);
    let platform = params.get("platform")
        .and_then(|s| match s.to_lowercase().as_str() { "telegram" => Some(hermes_protocol::Platform::Telegram), "discord" => Some(hermes_protocol::Platform::Discord), "slack" => Some(hermes_protocol::Platform::Slack), "whatsapp" => Some(hermes_protocol::Platform::WhatsApp), "signal" => Some(hermes_protocol::Platform::Signal), "matrix" => Some(hermes_protocol::Platform::Matrix), "webhook" => Some(hermes_protocol::Platform::Webhook), "email" => Some(hermes_protocol::Platform::Email), "sms" => Some(hermes_protocol::Platform::Sms), "ntfy" => Some(hermes_protocol::Platform::Ntfy), "simplex" => Some(hermes_protocol::Platform::Simplex), "feishu" => Some(hermes_protocol::Platform::Feishu), "wecom" => Some(hermes_protocol::Platform::WeCom), "dingtalk" => Some(hermes_protocol::Platform::DingTalk), "teams" => Some(hermes_protocol::Platform::Teams), "google_chat" => Some(hermes_protocol::Platform::GoogleChat), "qqbot" => Some(hermes_protocol::Platform::QQBot), "weixin" => Some(hermes_protocol::Platform::Weixin), "a2a" => Some(hermes_protocol::Platform::A2A), "raft" => Some(hermes_protocol::Platform::Raft), "photon" => Some(hermes_protocol::Platform::Photon), "buzz" => Some(hermes_protocol::Platform::Buzz), "irc" => Some(hermes_protocol::Platform::Irc), "mattermost" => Some(hermes_protocol::Platform::Mattermost), "line" => Some(hermes_protocol::Platform::Line), "homeassistant" => Some(hermes_protocol::Platform::HomeAssistant), _ => None })
        .unwrap_or(Platform::Other(0));
    let profile = params.get("profile").cloned().unwrap_or_else(|| "default".to_string());

    let server = server.clone();
    ws.on_upgrade(move |socket| async move {
        server.handle_connection(socket, session_id, platform, profile).await
    })
}

/// WebSocket relay client (for connecting to relay server)
pub struct WsRelayClient {
    url: String,
    session_id: Uuid,
    platform: Platform,
    profile: String,
    sender: Option<mpsc::UnboundedSender<RelayMessage>>,
    receiver: Option<mpsc::UnboundedReceiver<RelayMessage>>,
}

impl WsRelayClient {
    pub fn new(url: String, session_id: Uuid, platform: Platform, profile: String) -> Self {
        Self { url, session_id, platform, profile, sender: None, receiver: None }
    }

    pub async fn connect(&mut self) -> Result<(), RelayError> {
        let url = format!("{}?session_id={}&platform={}&profile={}", 
            self.url, self.session_id, self.platform, self.profile);
        
        let (ws_stream, _) = connect_async(&url).await
            .map_err(|e| RelayError::Connection(e.to_string()))?;
        
        let (mut ws_sender, mut ws_receiver) = ws_stream.split();
        let (tx, rx) = mpsc::unbounded_channel();
        let (_out_tx, mut out_rx) = mpsc::unbounded_channel::<RelayMessage>();

        let tx_for_sender = tx.clone();
        self.sender = Some(tx_for_sender);
        self.receiver = Some(rx);

        // Send connect message
        let connect_msg = RelayMessage::Connect {
            session_id: self.session_id,
            platform: self.platform,
            profile: self.profile.clone(),
        };
        ws_sender.send(TungsteniteMessage::Text(serde_json::to_string(&connect_msg).unwrap())).await
            .map_err(|e| RelayError::Connection(e.to_string()))?;

        // Spawn sender task
        let mut ws_sender = ws_sender;
        let _tx_for_task = tx.clone();
        tokio::spawn(async move {
            while let Some(msg) = out_rx.recv().await {
                let json = serde_json::to_string(&msg).unwrap();
                if ws_sender.send(TungsteniteMessage::Text(json)).await.is_err() {
                    break;
                }
            }
        });

        // Spawn receiver task
        let _session_id = self.session_id;
        tokio::spawn(async move {
            while let Some(msg) = ws_receiver.next().await {
                match msg {
                    Ok(TungsteniteMessage::Text(text)) => {
                        if let Ok(relay_msg) = serde_json::from_str::<RelayMessage>(&text) {
                            let _ = tx.send(relay_msg);
                        }
                    }
                    Ok(TungsteniteMessage::Close(_)) => break,
                    Err(_) => break,
                    _ => {}
                }
            }
        });

        Ok(())
    }

    pub async fn send(&self, msg: OutgoingMessage) -> Result<(), RelayError> {
        if let Some(sender) = &self.sender {
            sender.send(RelayMessage::Outgoing(msg))
                .map_err(|_| RelayError::Closed)?;
            Ok(())
        } else {
            Err(RelayError::Closed)
        }
    }

    pub async fn receive(&mut self) -> Result<Option<RelayMessage>, RelayError> {
        if let Some(receiver) = &mut self.receiver {
            Ok(receiver.recv().await)
        } else {
            Err(RelayError::Closed)
        }
    }

    pub async fn disconnect(&mut self) -> Result<(), RelayError> {
        if let Some(sender) = &self.sender {
            let _ = sender.send(RelayMessage::Disconnect { session_id: self.session_id });
        }
        self.sender = None;
        self.receiver = None;
        Ok(())
    }
}

/// Egress manager for outbound platform messages with rate limiting
pub struct EgressManager {
    adapters: Arc<RwLock<HashMap<Platform, Arc<dyn DynPlatformAdapter>>>>,
    rate_limiters: Arc<RwLock<HashMap<Platform, RateLimiter>>>,
    delivery_ledger: Arc<RwLock<HashMap<MessageId, DeliveryReceipt>>>,
}

impl EgressManager {
    pub fn new() -> Self {
        Self {
            adapters: Arc::new(RwLock::new(HashMap::new())),
            rate_limiters: Arc::new(RwLock::new(HashMap::new())),
            delivery_ledger: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub async fn register_adapter<A>(&self, adapter: A)
    where
        A: PlatformAdapter<Config = serde_json::Value, IncomingEvent = IncomingMessage, OutgoingMessage = OutgoingMessage> + 'static,
    {
        let platform = adapter.platform();
        let mut adapters = self.adapters.write().await;
        adapters.insert(platform, Arc::new(adapter) as Arc<dyn DynPlatformAdapter>);
    }

    pub async fn send(&self, msg: OutgoingMessage) -> Result<DeliveryReceipt, PlatformError> {
        // Check rate limit
        {
            let limiters = self.rate_limiters.read().await;
            if let Some(limiter) = limiters.get(&msg.platform) {
                limiter.acquire().await?;
            }
        }

        // Get adapter
        let adapter = {
            let adapters = self.adapters.read().await;
            adapters.get(&msg.platform).cloned()
        };

        if let Some(adapter) = adapter {
            let receipt = adapter.send(msg.clone()).await?;
            
            // Record in delivery ledger
            let mut ledger = self.delivery_ledger.write().await;
            ledger.insert(msg.id, receipt.clone());
            
            Ok(receipt)
        } else {
            Err(PlatformError::Internal(format!("No adapter for platform: {}", msg.platform)))
        }
    }

    pub async fn get_delivery_status(&self, message_id: MessageId) -> Option<DeliveryReceipt> {
        self.delivery_ledger.read().await.get(&message_id).cloned()
    }
}

impl Default for EgressManager {
    fn default() -> Self {
        Self::new()
    }
}
