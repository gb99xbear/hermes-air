// hermes-gateway-core - Core gateway logic
use hermes_config::GatewayConfig;
use hermes_relay::{WsRelayServer, WsRelayConfig};
use hermes_session_store::{SessionStore, SessionStoreFactory};
use std::sync::Arc;
use tracing::info;

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
        ).await?;

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

        // TODO: Start platform adapters
        // TODO: Start turn processing
        // TODO: Start cron scheduler
        // TODO: Start dashboard API
        
        info!("Hermes Gateway running");
        
        // Keep running
        tokio::signal::ctrl_c().await?;
        info!("Shutting down");
        
        Ok(())
    }
}

pub use hermes_config::GatewayConfig as Config;
