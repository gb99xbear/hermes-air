// hermes-gateway-core - Core gateway logic
use hermes_config::GatewayConfig;
use hermes_cron::CronScheduler;
use hermes_platform_adapter::{BasePlatformConfig, PlatformAdapter};
use hermes_platform_telegram::{TelegramAdapter, TelegramConfig};
use hermes_protocol::{
    MessageContent, MessageId, OutgoingMessage, Platform, PlatformEvent,
};
use hermes_relay::{WsRelayConfig, WsRelayServer};
use hermes_session_store::{SessionStore, SessionStoreFactory};
use std::collections::HashMap;
use std::sync::Arc;
use tracing::{error, info};

#[allow(dead_code)]
pub struct Gateway {
    config: GatewayConfig,
    relay_server: Arc<WsRelayServer>,
    session_store: Box<dyn SessionStore>,
}

impl Gateway {
    pub async fn new(config: GatewayConfig) -> anyhow::Result<Self> {
        let relay_config = WsRelayConfig {
            host: config.dashboard.host.clone(),
            port: config.dashboard.port,
            path: "/ws".to_string(),
            max_connections: config.gateway.max_concurrent_sessions,
            ping_interval_secs: 30,
            connection_timeout_secs: 60,
        };

        let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel();
        let relay_server = Arc::new(WsRelayServer::new(relay_config, event_tx));

        let session_store = SessionStoreFactory::create(
            config.session_store.backend,
            config.session_store.redis.clone(),
            config.session_store.local_cache_size,
            config.session_store.default_ttl_secs,
        )
        .await?;

        Ok(Self {
            config,
            relay_server,
            session_store,
        })
    }

    pub async fn run(self) -> anyhow::Result<()> {
        info!("Starting Hermes Gateway");

        // Start relay server
        let relay = self.relay_server.clone();
        tokio::spawn(async move {
            if let Err(e) = relay.run().await {
                tracing::error!("Relay server error: {}", e);
            }
        });

        // 1. Unbounded channel for incoming platform events
        let (platform_tx, mut platform_rx) =
            tokio::sync::mpsc::unbounded_channel::<PlatformEvent>();

        // 2. Start platform adapters
        let mut telegram_adapter_opt: Option<Arc<TelegramAdapter>> = None;
        if let Some(telegram_cfg) = self.config.platforms.get("telegram") {
            if telegram_cfg.enabled {
                info!("Initializing Telegram adapter");
                let mut map = telegram_cfg.config.clone();
                map.insert("enabled".to_string(), serde_json::Value::Bool(telegram_cfg.enabled));
                if let Some(ref rl) = telegram_cfg.rate_limit {
                    map.insert(
                        "rate_limit".to_string(),
                        serde_json::to_value(rl).unwrap_or(serde_json::Value::Null),
                    );
                }
                if let Some(ref wh) = telegram_cfg.webhook {
                    map.insert(
                        "webhook".to_string(),
                        serde_json::to_value(wh).unwrap_or(serde_json::Value::Null),
                    );
                }

                let tg_config: TelegramConfig = match serde_json::from_value(serde_json::Value::Object(map)) {
                    Ok(parsed) => parsed,
                    Err(err) => {
                        error!("Failed to parse telegram configuration: {}", err);
                        let base = BasePlatformConfig {
                            enabled: telegram_cfg.enabled,
                            rate_limit: telegram_cfg.rate_limit.as_ref().map(|rl| {
                                hermes_platform_adapter::RateLimitConfig {
                                    requests_per_second: rl.requests_per_second,
                                    burst: rl.burst,
                                }
                            }),
                            webhook: telegram_cfg.webhook.as_ref().map(|wh| {
                                hermes_platform_adapter::WebhookConfig {
                                    path: wh.path.clone(),
                                    secret: wh.secret.clone(),
                                    verify_signature: wh.verify_signature,
                                }
                            }),
                            custom: serde_json::Value::Object(telegram_cfg.config.clone()),
                        };
                        let bot_token = telegram_cfg
                            .config
                            .get("bot_token")
                            .and_then(|v| v.as_str())
                            .unwrap_or_default()
                            .to_string();
                        let webhook_url = telegram_cfg
                            .config
                            .get("webhook_url")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string());
                        let timeout_secs = telegram_cfg
                            .config
                            .get("timeout_secs")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(30);

                        TelegramConfig {
                            bot_token,
                            webhook_url,
                            allowed_updates: vec![],
                            drop_pending_updates: false,
                            timeout_secs,
                            base,
                        }
                    }
                };

                let mut adapter = TelegramAdapter::new();
                match adapter.initialize(tg_config).await {
                    Ok(_) => {
                        if let Err(e) = adapter.start(platform_tx.clone()).await {
                            error!("Failed to start Telegram adapter: {}", e);
                        } else {
                            info!("Telegram adapter started successfully");
                            telegram_adapter_opt = Some(Arc::new(adapter));
                        }
                    }
                    Err(e) => {
                        error!("Failed to initialize Telegram adapter: {}", e);
                    }
                }
            }
        }

        // 3. Start turn processing
        let tg_adapter_for_turn = telegram_adapter_opt.clone();
        let default_profile_name = self.config.gateway.default_profile.clone();
        let profiles = self.config.profiles.clone();

        tokio::spawn(async move {
            info!("Turn processing loop started");
            while let Some(event) = platform_rx.recv().await {
                match event {
                    PlatformEvent::Incoming(msg) => {
                        let prompt_text = match &msg.content {
                            MessageContent::Text { text } => text.clone(),
                            MessageContent::Image { caption, .. } => {
                                caption.clone().unwrap_or_default()
                            }
                            MessageContent::File { filename, .. } => {
                                format!("[File: {}]", filename)
                            }
                            _ => format!("[Event: {:?}]", msg.content),
                        };

                        info!(
                            session_id = %msg.session_id,
                            platform = %msg.platform,
                            prompt = %prompt_text,
                            "Incoming platform message received"
                        );

                        // Generate response text
                        let response_text = if let Some(profile) = profiles.get(&default_profile_name) {
                            format!(
                                "[Profile: {} | Model: {}] Received: {}",
                                profile.name, profile.model, prompt_text
                            )
                        } else {
                            format!("Echo: {}", prompt_text)
                        };

                        let outgoing = OutgoingMessage {
                            id: MessageId::new(),
                            session_id: msg.session_id,
                            platform: msg.platform,
                            content: MessageContent::Text {
                                text: response_text,
                            },
                            reply_to: Some(msg.id),
                            metadata: HashMap::new(),
                        };

                        match msg.platform {
                            Platform::Telegram => {
                                if let Some(ref adapter) = tg_adapter_for_turn {
                                    if let Err(e) = adapter.send(outgoing).await {
                                        error!("Failed to send Telegram response: {}", e);
                                    } else {
                                        info!(
                                            session_id = %msg.session_id,
                                            "Sent Telegram response successfully"
                                        );
                                    }
                                } else {
                                    error!("Telegram adapter not available for sending response");
                                }
                            }
                            other => {
                                info!(
                                    platform = %other,
                                    "No platform adapter registered for outgoing routing"
                                );
                            }
                        }
                    }
                    PlatformEvent::Outgoing(msg) => {
                        info!(session_id = %msg.session_id, platform = %msg.platform, "Outgoing event queued");
                    }
                    PlatformEvent::Ack { message_id, success } => {
                        info!(?message_id, success, "Platform message ACK");
                    }
                    PlatformEvent::Error { session_id, error } => {
                        error!(%session_id, %error, "Platform event error");
                    }
                    PlatformEvent::SessionCreated(session) => {
                        info!(session_id = %session.id, platform = %session.platform, "Session created");
                    }
                    PlatformEvent::SessionUpdated(session) => {
                        info!(session_id = %session.id, "Session updated");
                    }
                    PlatformEvent::SessionDeleted(id) => {
                        info!(session_id = %id, "Session deleted");
                    }
                }
            }
            info!("Turn processing loop terminated");
        });

        // 4. Start cron scheduler
        let cron = match CronScheduler::new(None).await {
            Ok(c) => {
                if let Err(e) = c.start().await {
                    error!("Failed to start cron scheduler: {}", e);
                    None
                } else {
                    info!("Cron scheduler started");
                    Some(c)
                }
            }
            Err(e) => {
                error!("Failed to initialize cron scheduler: {}", e);
                None
            }
        };

        // 5. Dashboard API placeholder
        if self.config.dashboard.enabled {
            info!(
                host = %self.config.dashboard.host,
                port = self.config.dashboard.port,
                "Dashboard API ready"
            );
        }

        info!("Hermes Gateway running");

        // Keep running
        tokio::signal::ctrl_c().await?;
        info!("Shutting down");

        drop(cron);
        Ok(())
    }
}

pub use hermes_config::GatewayConfig as Config;
