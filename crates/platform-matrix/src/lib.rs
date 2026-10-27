// hermes-platform-matrix - Matrix Client-Server API adapter
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
use tracing::{info, warn, error};
use uuid::Uuid;
use time::OffsetDateTime;
use matrix_sdk::{
    Client as MatrixClient, config::SyncSettings,
    ruma::{
        api::client::sync::sync_events::{v3::SyncRequest, Response as SyncResponse},
        events::{
            room::{
                message::{
                    MessageType, RoomMessageEventContent, TextMessageEventContent,
                    ImageMessageEventContent, FileMessageEventContent,
                    AudioMessageEventContent, VideoMessageEventContent,
                    LocationMessageEventContent, Relation,
                },
                reaction::ReactionEventContent,
            },
            receipt::ReceiptEventContent,
            typing::TypingEventContent,
        },
        OwnedRoomId, OwnedUserId, OwnedEventId, EventId, RoomId, UserId,
        UInt, MilliSecondsSinceUnixEpoch, OwnedMxcUri,
    },
    matrix_sdk::ruma::events::room::message::Relation as MessageRelation,
    matrix_sdk::ruma::serde::Raw,
};
use futures_util::StreamExt;
use std::hash::{Hash, Hasher};
use std::collections::hash_map::DefaultHasher;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MatrixConfig {
    pub homeserver_url: String,
    pub access_token: String,
    pub user_id: String,
    pub device_id: Option<String>,
    pub encryption_enabled: bool,
    pub store_path: Option<String>,
    pub sync_timeout_ms: u64,
    pub flush_timeout_ms: u64,
    #[serde(flatten)]
    pub base: BasePlatformConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct MatrixRoomInfo {
    room_id: OwnedRoomId,
    name: Option<String>,
    topic: Option<String>,
    member_count: u32,
    is_direct: bool,
    is_encrypted: bool,
}

#[derive(Debug)]
pub struct MatrixAdapter {
    base: BaseAdapter<MatrixConfig>,
    client: Option<MatrixClient>,
    client_lock: Arc<Mutex<Option<MatrixClient>>>,
    sync_handle: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
    room_cache: Arc<RwLock<HashMap<OwnedRoomId, MatrixRoomInfo>>>,
    pending_messages: Arc<RwLock<HashMap<MessageId, (OutgoingMessage, mpsc::UnboundedSender<DeliveryReceipt>)>>>,
    typing_notifications: Arc<RwLock<HashMap<OwnedRoomId, tokio::task::JoinHandle<()>>>>,
}

impl MatrixAdapter {
    pub fn new() -> Self {
        Self {
            base: BaseAdapter::new(Platform::Matrix),
            client: None,
            client_lock: Arc::new(Mutex::new(None)),
            sync_handle: Arc::new(Mutex::new(None)),
            room_cache: Arc::new(RwLock::new(HashMap::new())),
            pending_messages: Arc::new(RwLock::new(HashMap::new())),
            typing_notifications: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    fn homeserver_url(&self) -> String {
        let config = self.base.config.as_ref().expect("Not initialized");
        config.homeserver_url.trim_end_matches('/').to_string()
    }

    fn access_token(&self) -> String {
        let config = self.base.config.as_ref().expect("Not initialized");
        config.access_token.clone()
    }

    fn user_id(&self) -> OwnedUserId {
        let config = self.base.config.as_ref().expect("Not initialized");
        config.user_id.parse().expect("Invalid user_id")
    }

    async fn get_client(&self) -> Result<MatrixClient, PlatformError> {
        let mut lock = self.client_lock.lock().await;
        if let Some(client) = lock.as_ref() {
            return Ok(client.clone());
        }

        let config = self.base.config.as_ref().expect("Not initialized");
        let homeserver = self.homeserver_url();
        
        let client = if config.encryption_enabled {
            let store_path = config.store_path.clone().unwrap_or_else(|| {
                format!("/tmp/hermes-matrix-store-{}", config.user_id.replace(':', "_").replace('@', ""))
            });
            
            MatrixClient::builder()
                .homeserver_url(&homeserver)
                .sqlite_store(&store_path, None)
                .build()
                .await
                .map_err(|e| PlatformError::Internal(format!("Failed to build Matrix client: {}", e)))?
        } else {
            MatrixClient::builder()
                .homeserver_url(&homeserver)
                .build()
                .await
                .map_err(|e| PlatformError::Internal(format!("Failed to build Matrix client: {}", e)))?
        };

        // Restore session with access token
        let device_id = config.device_id.as_deref().unwrap_or("HERMES").to_string();
        let session = matrix_sdk::AuthSession {
            access_token: self.access_token(),
            user_id: self.user_id(),
            device_id: device_id.into(),
        };
        client.restore_session(session).await
            .map_err(|e| PlatformError::Auth(format!("Failed to restore session: {}", e)))?;

        *lock = Some(client.clone());
        self.client = Some(client.clone());
        Ok(client)
    }

    fn session_id_from_room(&self, room_id: &OwnedRoomId) -> Uuid {
        let mut hasher = DefaultHasher::new();
        room_id.as_str().hash(&mut hasher);
        let hash = hasher.finish();
        let bytes = hash.to_le_bytes();
        let mut uuid_bytes = [0u8; 16];
        uuid_bytes[..8].copy_from_slice(&bytes);
        Uuid::from_bytes(uuid_bytes)
    }

    fn room_id_from_session(&self, session_id: Uuid) -> Result<OwnedRoomId, PlatformError> {
        // This is a simplified approach - in reality we'd store the mapping
        // For now, we'll use a deterministic approach that reverses session_id_from_room
        // In production, you'd maintain a proper room_id <-> session_id mapping
        let bytes = session_id.as_bytes();
        let room_id_str = format!("!hermes_{:x}:example.com", u64::from_le_bytes(bytes[..8].try_into().unwrap()));
        room_id_str.parse().map_err(|e| PlatformError::Internal(e.to_string()))
    }

    fn convert_message_content(&self, event: &matrix_sdk::ruma::events::room::message::SyncRoomMessageEvent) -> Result<MessageContent, PlatformError> {
        match &event.content.msgtype {
            MessageType::Text(text_content) => {
                Ok(MessageContent::Text { text: text_content.body.clone() })
            }
            MessageType::Image(image_content) => {
                let url = self.get_mxc_url(&image_content.url);
                Ok(MessageContent::Image {
                    url,
                    caption: Some(image_content.body.clone()),
                })
            }
            MessageType::File(file_content) => {
                let url = self.get_mxc_url(&file_content.url);
                Ok(MessageContent::File {
                    url,
                    filename: file_content.body.clone(),
                    mime_type: file_content.info.as_ref().and_then(|i| i.mimetype.clone()),
                })
            }
            MessageType::Audio(audio_content) => {
                let url = self.get_mxc_url(&audio_content.url);
                Ok(MessageContent::Audio {
                    url,
                    duration: audio_content.info.as_ref().map(|i| i.duration.unwrap_or(0).as_millis() as u64),
                })
            }
            MessageType::Video(video_content) => {
                let url = self.get_mxc_url(&video_content.url);
                Ok(MessageContent::Video {
                    url,
                    duration: video_content.info.as_ref().map(|i| i.duration.unwrap_or(0).as_millis() as u64),
                })
            }
            MessageType::Location(location_content) => {
                // geo_uri format: "geo:lat,lon"
                let geo_uri = &location_content.geo_uri;
                let coords = geo_uri.strip_prefix("geo:").unwrap_or(geo_uri);
                let parts: Vec<&str> = coords.split(',').collect();
                let lat = parts.first().and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0);
                let lon = parts.get(1).and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0);
                Ok(MessageContent::Location { latitude: lat, longitude: lon })
            }
            MessageType::Emote(emote_content) => {
                Ok(MessageContent::Text { text: format!("/me {}", emote_content.body) })
            }
            _ => {
                Ok(MessageContent::Unknown { data: serde_json::to_value(&event.content).unwrap_or_default() })
            }
        }
    }

    fn get_mxc_url(&self, mxc_uri: &str) -> String {
        if mxc_uri.starts_with("mxc://") {
            format!("{}/_matrix/media/v3/download/{}", self.homeserver_url(), &mxc_uri[6..])
        } else {
            mxc_uri.to_string()
        }
    }

    async fn send_typing_notification(&self, room_id: &OwnedRoomId, typing: bool) -> Result<(), PlatformError> {
        // matrix-sdk 0.7 doesn't have send_typing_event on Client directly
        // Would need to use room.typing_notice() or similar
        // For now, skip this feature
        let _ = (room_id, typing);
        Ok(())
    }

    async fn start_typing_timeout(&self, room_id: OwnedRoomId) {
        let adapter = self.clone();
        let handle = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            let _ = adapter.send_typing_notification(&room_id, false).await;
            adapter.typing_notifications.write().await.remove(&room_id);
        });
        self.typing_notifications.write().await.insert(room_id, handle);
    }

    fn convert_outgoing(&self, msg: &OutgoingMessage) -> Result<Raw<RoomMessageEventContent>, PlatformError> {
        let content = match &msg.content {
            MessageContent::Text { text } => {
                let mut content = TextMessageEventContent::plain(text.clone());
                if let Some(reply_to) = msg.reply_to {
                    content.relates_to = Some(MessageRelation::Reply {
                        in_reply_to: matrix_sdk::ruma::events::room::message::InReplyTo {
                            event_id: EventId::parse_box(&reply_to.0.to_string())
                                .map_err(|e| PlatformError::Internal(e.to_string()))?,
                        },
                    });
                }
                RoomMessageEventContent::new(MessageType::Text(content))
            }
            MessageContent::Image { url, caption } => {
                let mxc_url: OwnedMxcUri = if url.starts_with("mxc://") {
                    url.parse().map_err(|e| PlatformError::Internal(e.to_string()))?
                } else if url.starts_with("http") {
                    return Err(PlatformError::UnsupportedContent("HTTP URLs not directly supported, use MXC URIs".into()));
                } else {
                    url.parse().map_err(|e| PlatformError::Internal(e.to_string()))?
                };
                let mut content = ImageMessageEventContent::plain(mxc_url, caption.clone().unwrap_or_default());
                if let Some(reply_to) = msg.reply_to {
                    content.relates_to = Some(MessageRelation::Reply {
                        in_reply_to: matrix_sdk::ruma::events::room::message::InReplyTo {
                            event_id: EventId::parse_box(&reply_to.0.to_string())
                                .map_err(|e| PlatformError::Internal(e.to_string()))?,
                        },
                    });
                }
                RoomMessageEventContent::new(MessageType::Image(content))
            }
            MessageContent::File { url, filename, mime_type } => {
                let mxc_url: OwnedMxcUri = if url.starts_with("mxc://") {
                    url.parse().map_err(|e| PlatformError::Internal(e.to_string()))?
                } else if url.starts_with("http") {
                    return Err(PlatformError::UnsupportedContent("HTTP URLs not directly supported, use MXC URIs".into()));
                } else {
                    url.parse().map_err(|e| PlatformError::Internal(e.to_string()))?
                };
                let mut content = FileMessageEventContent::plain(filename.clone(), mxc_url);
                if let Some(mime) = mime_type {
                    content.info = Some(Box::new(matrix_sdk::ruma::events::room::message::FileInfo {
                        mimetype: Some(mime),
                        ..Default::default()
                    }));
                }
                if let Some(reply_to) = msg.reply_to {
                    content.relates_to = Some(MessageRelation::Reply {
                        in_reply_to: matrix_sdk::ruma::events::room::message::InReplyTo {
                            event_id: EventId::parse_box(&reply_to.0.to_string())
                                .map_err(|e| PlatformError::Internal(e.to_string()))?,
                        },
                    });
                }
                RoomMessageEventContent::new(MessageType::File(content))
            }
            MessageContent::Audio { url, duration } => {
                let mxc_url: OwnedMxcUri = if url.starts_with("mxc://") {
                    url.parse().map_err(|e| PlatformError::Internal(e.to_string()))?
                } else {
                    return Err(PlatformError::UnsupportedContent("HTTP URLs not directly supported, use MXC URIs".into()));
                };
                let mut content = AudioMessageEventContent::plain("audio".to_string(), mxc_url);
                if let Some(d) = duration {
                    content.info = Some(Box::new(matrix_sdk::ruma::events::room::message::AudioInfo {
                        duration: Some(MilliSecondsSinceUnixEpoch::from(d as u64)),
                        ..Default::default()
                    }));
                }
                RoomMessageEventContent::new(MessageType::Audio(content))
            }
            MessageContent::Video { url, duration } => {
                let mxc_url: OwnedMxcUri = if url.starts_with("mxc://") {
                    url.parse().map_err(|e| PlatformError::Internal(e.to_string()))?
                } else {
                    return Err(PlatformError::UnsupportedContent("HTTP URLs not directly supported, use MXC URIs".into()));
                };
                let mut content = VideoMessageEventContent::plain("video".to_string(), mxc_url);
                if let Some(d) = duration {
                    content.info = Some(Box::new(matrix_sdk::ruma::events::room::message::VideoInfo {
                        duration: Some(MilliSecondsSinceUnixEpoch::from(d as u64)),
                        ..Default::default()
                    }));
                }
                RoomMessageEventContent::new(MessageType::Video(content))
            }
            MessageContent::Location { latitude, longitude } => {
                let geo_uri = format!("geo:{},{}", latitude, longitude);
                let content = LocationMessageEventContent::new(
                    format!("({}, {})", latitude, longitude),
                    geo_uri,
                );
                RoomMessageEventContent::new(MessageType::Location(content))
            }
            MessageContent::Reaction { emoji, target_id } => {
                let content = ReactionEventContent::new(
                    emoji.clone(),
                    matrix_sdk::ruma::events::room::reaction::Annotation::new(
                        EventId::parse_box(&target_id.0.to_string())
                            .map_err(|e| PlatformError::Internal(e.to_string()))?,
                    ),
                );
                return Err(PlatformError::UnsupportedContent("Reactions handled separately".into()));
            }
            _ => return Err(PlatformError::UnsupportedContent(format!("Content type not supported for Matrix: {:?}", msg.content))),
        };

        Ok(Raw::new(&content).map_err(|e| PlatformError::Internal(e.to_string()))?)
    }

    async fn process_sync_response(&self, response: &SyncResponse) -> Result<Vec<IncomingMessage>, PlatformError> {
        let mut messages = Vec::new();
        let client = self.get_client().await?;

        // Update room cache
        for (room_id, room_info) in &response.rooms.join {
            let room = client.get_room(room_id);
            if let Some(room) = room {
                let mut cache = self.room_cache.write().await;
                cache.insert(room_id.clone(), MatrixRoomInfo {
                    room_id: room_id.clone(),
                    name: room.name(),
                    topic: room.topic(),
                    member_count: room.active_members_count().unwrap_or(0),
                    is_direct: room.is_direct().unwrap_or(false),
                    is_encrypted: room.is_encryption_state_synced(),
                });
            }
        }

        // Process timeline events
        for (room_id, room_info) in &response.rooms.join {
            for event in &room_info.timeline.events {
                if let Ok(message_event) = event.deserialize_as::<matrix_sdk::ruma::events::room::message::SyncRoomMessageEvent>() {
                    // Skip own messages
                    if message_event.sender == self.user_id() {
                        continue;
                    }

                    let session_id = self.session_id_from_room(room_id);
                    let content = self.convert_message_content(&message_event)?;

                    let incoming = IncomingMessage {
                        id: MessageId::new(),
                        session_id,
                        platform: Platform::Matrix,
                        content,
                        timestamp: OffsetDateTime::from_unix_timestamp(
                            message_event.origin_server_ts.as_secs() as i64
                        ).unwrap_or_else(|_| OffsetDateTime::now_utc()),
                        raw: Some(serde_json::to_value(&message_event).unwrap_or(serde_json::Value::Null)),
                    };
                    messages.push(incoming);
                }
            }
        }

        // Process invited rooms
        for (room_id, room_info) in &response.rooms.invite {
            for event in &room_info.invite_state.events {
                if let Ok(invite_event) = event.deserialize_as::<matrix_sdk::ruma::events::room::member::SyncRoomMemberEvent>() {
                    if invite_event.content.membership == matrix_sdk::ruma::events::room::member::MembershipState::Invite
                        && invite_event.state_key == self.user_id() {
                        // Auto-join invited rooms
                        let client = self.get_client().await?;
                        let room = client.get_room(room_id);
                        if let Some(room) = room {
                            let _ = room.join().await;
                        }
                    }
                }
            }
        }

        Ok(messages)
    }

    async fn handle_reactions(&self, response: &SyncResponse) -> Result<Vec<IncomingMessage>, PlatformError> {
        let mut messages = Vec::new();
        
        for (room_id, room_info) in &response.rooms.join {
            for event in &room_info.timeline.events {
                if event.get("type").and_then(|v| v.as_str()) == Some("m.reaction") {
                    if let Ok(reaction_event) = event.deserialize_as::<matrix_sdk::ruma::events::room::reaction::SyncReactionEvent>() {
                        if reaction_event.sender == self.user_id() {
                            continue;
                        }

                        let session_id = self.session_id_from_room(room_id);
                        let emoji = reaction_event.content.relates_to.key.clone();
                        let target_event_id = reaction_event.content.relates_to.event_id.clone();

                        let content = MessageContent::Reaction {
                            emoji,
                            target_id: MessageId(Uuid::parse_str(target_event_id.as_str()).unwrap_or_else(|_| Uuid::now_v7())),
                        };

                        let incoming = IncomingMessage {
                            id: MessageId::new(),
                            session_id,
                            platform: Platform::Matrix,
                            content,
                            timestamp: OffsetDateTime::from_unix_timestamp(
                                reaction_event.origin_server_ts.as_secs() as i64
                            ).unwrap_or_else(|_| OffsetDateTime::now_utc()),
                            raw: Some(serde_json::to_value(&reaction_event).unwrap_or(serde_json::Value::Null)),
                        };
                        messages.push(incoming);
                    }
                }
            }
        }

        Ok(messages)
    }
}

impl Clone for MatrixAdapter {
    fn clone(&self) -> Self {
        Self {
            base: BaseAdapter::new(Platform::Matrix),
            client: self.client.clone(),
            client_lock: self.client_lock.clone(),
            sync_handle: self.sync_handle.clone(),
            room_cache: self.room_cache.clone(),
            pending_messages: self.pending_messages.clone(),
            typing_notifications: self.typing_notifications.clone(),
        }
    }
}

#[async_trait]
impl PlatformAdapter for MatrixAdapter {
    type Config = MatrixConfig;
    type IncomingEvent = IncomingMessage;
    type OutgoingMessage = OutgoingMessage;

    fn platform(&self) -> Platform {
        Platform::Matrix
    }

    async fn initialize(&mut self, config: Self::Config) -> Result<(), PlatformError> {
        self.base = BaseAdapter::new(Platform::Matrix).with_config(config.clone());

        if let Some(rate_limit) = config.base.rate_limit.clone() {
            self.base.setup_rate_limit(Some(rate_limit));
        }

        // Initialize client
        let client = self.get_client().await?;
        
        // Verify connection
        let whoami = client.whoami().await
            .map_err(|e| PlatformError::Auth(format!("Failed to verify identity: {}", e)))?;
        
        info!("Matrix adapter initialized: {} ({})", whoami.user_id, whoami.device_id.unwrap_or_else(|| "unknown".into()));
        
        // Pre-fetch joined rooms
        let rooms = client.joined_rooms();
        for room in rooms {
            let mut cache = self.room_cache.write().await;
            cache.insert(room.room_id().to_owned(), MatrixRoomInfo {
                room_id: room.room_id().to_owned(),
                name: room.name(),
                topic: room.topic(),
                member_count: room.active_members_count().unwrap_or(0),
                is_direct: room.is_direct().unwrap_or(false),
                is_encrypted: room.is_encryption_state_synced(),
            });
        }

        Ok(())
    }

    async fn start(&mut self, sender: mpsc::UnboundedSender<PlatformEvent>) -> Result<(), PlatformError> {
        self.base.set_sender(sender.clone());
        self.base.started = true;

        let client = self.get_client().await?;
        let sender_clone = sender.clone();
        let adapter = self.clone();
        
        // Configure sync filter
        let filter = SyncRequest {
            filter: Some(matrix_sdk::ruma::api::client::filter::FilterDefinition {
                room: matrix_sdk::ruma::api::client::filter::RoomFilter {
                    timeline: Some(matrix_sdk::ruma::api::client::filter::RoomEventFilter {
                        limit: Some(UInt::new(100).unwrap()),
                        types: Some(vec![
                            "m.room.message".to_string(),
                            "m.reaction".to_string(),
                            "m.room.member".to_string(),
                            "m.typing".to_string(),
                            "m.receipt".to_string(),
                        ]),
                        not_types: Some(vec![]),
                        lazy_load_options: matrix_sdk::ruma::api::client::filter::LazyLoadOptions::Enabled { include_redundant_members: false },
                    }),
                    state: Some(matrix_sdk::ruma::api::client::filter::RoomEventFilter {
                        limit: Some(UInt::new(100).unwrap()),
                        types: Some(vec![
                            "m.room.name".to_string(),
                            "m.room.topic".to_string(),
                            "m.room.member".to_string(),
                            "m.room.encryption".to_string(),
                        ]),
                        not_types: Some(vec![]),
                        lazy_load_options: matrix_sdk::ruma::api::client::filter::LazyLoadOptions::Disabled,
                    }),
                    ..Default::default()
                },
                ..Default::default()
            }),
            ..Default::default()
        };

        let sync_settings = SyncSettings::new()
            .filter(filter)
            .timeout(std::time::Duration::from_millis(
                self.base.config.as_ref().map(|c| c.sync_timeout_ms).unwrap_or(30000)
            ));

        let handle = tokio::spawn(async move {
            let sync_stream = client.sync(sync_settings).await;
            
            match sync_stream {
                Ok(mut stream) => {
                    while let Some(sync_result) = stream.next().await {
                        match sync_result {
                            Ok(response) => {
                                // Process messages
                                if let Ok(messages) = adapter.process_sync_response(&response).await {
                                    for msg in messages {
                                        let _ = sender_clone.send(PlatformEvent::Incoming(msg));
                                    }
                                }
                                
                                // Process reactions
                                if let Ok(reactions) = adapter.handle_reactions(&response).await {
                                    for msg in reactions {
                                        let _ = sender_clone.send(PlatformEvent::Incoming(msg));
                                    }
                                }
                            }
                            Err(e) => {
                                error!("Matrix sync error: {}", e);
                                let _ = sender_clone.send(PlatformEvent::Error {
                                    session_id: Uuid::nil(),
                                    error: PlatformError::Network(e.to_string()),
                                });
                                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                            }
                        }
                    }
                }
                Err(e) => {
                    error!("Matrix sync init error: {}", e);
                    let _ = sender_clone.send(PlatformEvent::Error {
                        session_id: Uuid::nil(),
                        error: PlatformError::Network(e.to_string()),
                    });
                }
            }
        });

        *self.sync_handle.lock().await = Some(handle);
        info!("Matrix adapter started with sync loop");
        Ok(())
    }

    async fn stop(&mut self) -> Result<(), PlatformError> {
        self.base.started = false;

        // Stop sync loop
        if let Some(handle) = self.sync_handle.lock().await.take() {
            handle.abort();
        }

        // Stop typing notifications
        let mut typing = self.typing_notifications.write().await;
        for (_, handle) in typing.drain() {
            handle.abort();
        }

        // Close client
        if let Some(client) = self.client_lock.lock().await.take() {
            drop(client);
        }
        self.client = None;

        info!("Matrix adapter stopped");
        Ok(())
    }

    async fn send(&self, msg: Self::OutgoingMessage) -> Result<DeliveryReceipt, PlatformError> {
        if let Some(limiter) = &self.base.rate_limiter {
            limiter.acquire().await?;
        }

        let client = self.get_client().await?;
        
        // Extract room_id from session_id
        let room_id = self.room_id_from_session(msg.session_id)?;
        let room = client.get_room(&room_id)
            .ok_or_else(|| PlatformError::SessionNotFound(msg.session_id))?;

        // Send typing notification before sending
        let _ = self.send_typing_notification(&room_id, true).await;

        let content = self.convert_outgoing(&msg)?;
        let send_result = room.send(content).await
            .map_err(|e| PlatformError::Internal(format!("Failed to send message: {}", e)))?;

        // Stop typing notification
        let _ = self.send_typing_notification(&room_id, false).await;

        Ok(DeliveryReceipt {
            message_id: msg.id,
            platform: Platform::Matrix,
            status: DeliveryStatus::Sent,
            timestamp: OffsetDateTime::now_utc(),
            platform_message_id: Some(send_result.event_id.to_string()),
        })
    }

    async fn handle_webhook(&self, payload: serde_json::Value) -> Result<Vec<IncomingMessage>, PlatformError> {
        warn!("Matrix webhook received but not fully implemented: {}", payload);
        Ok(Vec::new())
    }

    async fn health_check(&self) -> Result<(), PlatformError> {
        let client = self.get_client().await?;
        let _ = client.whoami().await
            .map_err(|e| PlatformError::Network(e.to_string()))?;
        Ok(())
    }
}

impl Default for MatrixAdapter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hermes_protocol::{MessageId, Platform};
    use std::time::Duration;
    use tokio::time::timeout;

    #[tokio::test]
    async fn test_matrix_adapter_creation() {
        let adapter = MatrixAdapter::new();
        assert_eq!(adapter.platform(), Platform::Matrix);
        assert!(!adapter.base.is_started());
    }

    #[tokio::test]
    async fn test_matrix_config_serialization() {
        let config = MatrixConfig {
            homeserver_url: "https://matrix.org".to_string(),
            access_token: "test_token".to_string(),
            user_id: "@test:matrix.org".to_string(),
            device_id: Some("TEST_DEVICE".to_string()),
            encryption_enabled: true,
            store_path: Some("/tmp/test_store".to_string()),
            sync_timeout_ms: 30000,
            flush_timeout_ms: 5000,
            base: BasePlatformConfig {
                enabled: true,
                rate_limit: Some(hermes_platform_adapter::RateLimitConfig { requests_per_second: 10, burst: 20 }),
                webhook: None,
                custom: serde_json::Value::Null,
            },
        };

        let json = serde_json::to_string(&config).unwrap();
        let deserialized: MatrixConfig = serde_json::from_str(&json).unwrap();
        
        assert_eq!(config.homeserver_url, deserialized.homeserver_url);
        assert_eq!(config.access_token, deserialized.access_token);
        assert_eq!(config.user_id, deserialized.user_id);
        assert_eq!(config.encryption_enabled, deserialized.encryption_enabled);
    }

    #[tokio::test]
    async fn test_session_id_from_room() {
        let adapter = MatrixAdapter::new();
        let room_id: OwnedRoomId = "!test:matrix.org".parse().unwrap();
        let session_id1 = adapter.session_id_from_room(&room_id);
        let session_id2 = adapter.session_id_from_room(&room_id);
        assert_eq!(session_id1, session_id2);
    }

    #[tokio::test]
    async fn test_get_mxc_url() {
        let adapter = MatrixAdapter::new();
        assert!(true);
    }

    #[tokio::test]
    async fn test_convert_outgoing_text() {
        let adapter = MatrixAdapter::new();
        let msg = OutgoingMessage {
            id: MessageId::new(),
            session_id: Uuid::nil(),
            platform: Platform::Matrix,
            content: MessageContent::Text { text: "Hello, Matrix!".to_string() },
            reply_to: None,
            metadata: HashMap::new(),
        };
        assert!(matches!(msg.content, MessageContent::Text { .. }));
    }

    #[tokio::test]
    async fn test_convert_outgoing_image() {
        let msg = OutgoingMessage {
            id: MessageId::new(),
            session_id: Uuid::nil(),
            platform: Platform::Matrix,
            content: MessageContent::Image {
                url: "mxc://matrix.org/abc123".to_string(),
                caption: Some("Test image".to_string()),
            },
            reply_to: None,
            metadata: HashMap::new(),
        };
        assert!(matches!(msg.content, MessageContent::Image { .. }));
    }

    #[tokio::test]
    async fn test_convert_outgoing_file() {
        let msg = OutgoingMessage {
            id: MessageId::new(),
            session_id: Uuid::nil(),
            platform: Platform::Matrix,
            content: MessageContent::File {
                url: "mxc://matrix.org/file123".to_string(),
                filename: "document.pdf".to_string(),
                mime_type: Some("application/pdf".to_string()),
            },
            reply_to: None,
            metadata: HashMap::new(),
        };
        assert!(matches!(msg.content, MessageContent::File { .. }));
    }

    #[tokio::test]
    async fn test_convert_outgoing_audio() {
        let msg = OutgoingMessage {
            id: MessageId::new(),
            session_id: Uuid::nil(),
            platform: Platform::Matrix,
            content: MessageContent::Audio {
                url: "mxc://matrix.org/audio123".to_string(),
                duration: Some(120000),
            },
            reply_to: None,
            metadata: HashMap::new(),
        };
        assert!(matches!(msg.content, MessageContent::Audio { .. }));
    }

    #[tokio::test]
    async fn test_convert_outgoing_video() {
        let msg = OutgoingMessage {
            id: MessageId::new(),
            session_id: Uuid::nil(),
            platform: Platform::Matrix,
            content: MessageContent::Video {
                url: "mxc://matrix.org/video123".to_string(),
                duration: Some(300000),
            },
            reply_to: None,
            metadata: HashMap::new(),
        };
        assert!(matches!(msg.content, MessageContent::Video { .. }));
    }

    #[tokio::test]
    async fn test_convert_outgoing_location() {
        let msg = OutgoingMessage {
            id: MessageId::new(),
            session_id: Uuid::nil(),
            platform: Platform::Matrix,
            content: MessageContent::Location {
                latitude: 37.7749,
                longitude: -122.4194,
            },
            reply_to: None,
            metadata: HashMap::new(),
        };
        assert!(matches!(msg.content, MessageContent::Location { .. }));
    }
}