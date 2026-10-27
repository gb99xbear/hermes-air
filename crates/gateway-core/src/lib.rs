// hermes-gateway-core - Core gateway logic
pub mod llm;
pub mod tools;

use hermes_config::GatewayConfig;
use hermes_platform_adapter::PlatformAdapter;
use hermes_platform_telegram::{TelegramAdapter, TelegramConfig};
use hermes_protocol::{IncomingMessage, MessageContent, MessageId, OutgoingMessage, Platform, PlatformEvent};
use hermes_relay::{WsRelayConfig, WsRelayServer};
use hermes_session_store::{SessionStore, SessionStoreFactory};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

pub use llm::{ChatMessage, ChatResponse, LlmClient};
pub use tools::{ToolCall, ToolRegistry, ToolResult};

#[allow(dead_code)]
pub struct Gateway {
    config: GatewayConfig,
    relay_server: Arc<WsRelayServer>,
    session_store: Box<dyn SessionStore>,
    llm_client: Arc<LlmClient>,
    tool_registry: Arc<ToolRegistry>,
    event_tx: mpsc::UnboundedSender<PlatformEvent>,
    event_rx: Option<mpsc::UnboundedReceiver<PlatformEvent>>,
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

        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let relay_server = Arc::new(WsRelayServer::new(relay_config, event_tx.clone()));

        let session_store = SessionStoreFactory::create(
            config.session_store.backend,
            config.session_store.redis.clone(),
            config.session_store.local_cache_size,
            config.session_store.default_ttl_secs,
        )
        .await?;

        // Initialize LLM client:
        // Priority: environment variables -> 9Router default fallback
        // base_url: LLM_BASE_URL or OPENAI_BASE_URL or fallback to http://100.75.163.92:20128/v1
        // api_key: HERMES_CUSTOM_100_75_163_92_20128_API_KEY or HERMES_CUSTOM_100_77_223_11_20128_API_KEY or OPENAI_API_KEY
        let base_url = std::env::var("LLM_BASE_URL")
            .or_else(|_| std::env::var("OPENAI_BASE_URL"))
            .unwrap_or_else(|_| "http://100.75.163.92:20128/v1".to_string());

        let api_key = std::env::var("HERMES_CUSTOM_100_75_163_92_20128_API_KEY")
            .or_else(|_| std::env::var("HERMES_CUSTOM_100_77_223_11_20128_API_KEY"))
            .or_else(|_| std::env::var("OPENAI_API_KEY"))
            .ok();

        let llm_client = Arc::new(LlmClient::new(Some(base_url), api_key));
        let tool_registry = Arc::new(ToolRegistry::new());

        Ok(Self {
            config,
            relay_server,
            session_store,
            llm_client,
            tool_registry,
            event_tx,
            event_rx: Some(event_rx),
        })
    }

    pub async fn run(mut self) -> anyhow::Result<()> {
        info!("Starting Hermes Gateway");

        // Start relay server
        let relay = self.relay_server.clone();
        tokio::spawn(async move {
            if let Err(e) = relay.run().await {
                error!("Relay server error: {}", e);
            }
        });

        // Initialize platform adapters if configured
        let mut telegram_adapter: Option<Arc<TelegramAdapter>> = None;
        if let Some(tg_config_entry) = self.config.platforms.get("telegram") {
            if tg_config_entry.enabled {
                match serde_json::from_value::<TelegramConfig>(
                    serde_json::Value::Object(tg_config_entry.config.clone()),
                ) {
                    Ok(parsed_cfg) => {
                        let mut adapter = TelegramAdapter::new();
                        if let Err(e) = adapter.initialize(parsed_cfg).await {
                            error!("Failed to initialize Telegram adapter: {}", e);
                        } else if let Err(e) = adapter.start(self.event_tx.clone()).await {
                            error!("Failed to start Telegram adapter: {}", e);
                        } else {
                            telegram_adapter = Some(Arc::new(adapter));
                            info!("Telegram adapter started successfully");
                        }
                    }
                    Err(e) => {
                        error!("Failed to parse telegram config: {}", e);
                    }
                }
            }
        }

        // Setup event loop receiver
        let mut event_rx = self
            .event_rx
            .take()
            .expect("event_rx can only be taken once");

        let llm_client = self.llm_client.clone();
        let tool_registry = self.tool_registry.clone();
        let default_profile = self
            .config
            .profiles
            .get(&self.config.gateway.default_profile)
            .cloned()
            .or_else(|| self.config.profiles.values().next().cloned());

        let event_loop_tg = telegram_adapter.clone();

        tokio::spawn(async move {
            info!("TurnProcessor event loop started");
            while let Some(event) = event_rx.recv().await {
                match event {
                    PlatformEvent::Incoming(msg) => {
                        Self::process_incoming(msg, &llm_client, &tool_registry, &default_profile, &event_loop_tg).await;
                    }
                    PlatformEvent::Error { session_id, error } => {
                        warn!("Platform error for session {}: {:?}", session_id, error);
                    }
                    other => {
                        info!("Received event: {:?}", other);
                    }
                }
            }
        });

        info!("Hermes Gateway running");

        // Keep running until shutdown signal
        tokio::signal::ctrl_c().await?;
        info!("Shutting down");

        Ok(())
    }

    async fn process_incoming(
        msg: IncomingMessage,
        llm_client: &Arc<LlmClient>,
        tool_registry: &Arc<ToolRegistry>,
        profile: &Option<hermes_config::ProfileConfig>,
        tg_adapter: &Option<Arc<TelegramAdapter>>,
    ) {
        let (prompt_text, incoming_id) = match &msg.content {
            MessageContent::Text { text } => (text.clone(), msg.id),
            _ => {
                info!("Ignoring non-text incoming message: {:?}", msg.id);
                return;
            }
        };

        info!("Processing incoming message from {}: {}", msg.platform, prompt_text);

        let model = profile
            .as_ref()
            .map(|p| p.model.as_str())
            .unwrap_or("gemini-2.5-flash");
        let system_prompt = profile
            .as_ref()
            .and_then(|p| p.system_prompt.as_deref());
        let temperature = profile.as_ref().map(|p| p.temperature).unwrap_or(0.7);
        let max_tokens = profile.as_ref().map(|p| p.max_tokens).unwrap_or(4096);

        // Tool-calling loop setup
        let mut messages = Vec::new();
        if let Some(sys) = system_prompt {
            if !sys.is_empty() {
                messages.push(ChatMessage {
                    role: "system".to_string(),
                    content: Some(sys.to_string()),
                    tool_calls: None,
                    tool_call_id: None,
                });
            }
        }
        messages.push(ChatMessage {
            role: "user".to_string(),
            content: Some(prompt_text.clone()),
            tool_calls: None,
            tool_call_id: None,
        });

        let tool_definitions = tool_registry.get_definitions();
        let max_tool_iterations = 10;
        let mut iteration = 0;
        let mut final_text = String::new();

        while iteration < max_tool_iterations {
            iteration += 1;
            info!("Agent loop iteration {}/{}", iteration, max_tool_iterations);

            match llm_client
                .chat_completion_with_tools(
                    model,
                    &messages,
                    Some(tool_definitions.clone()),
                    None,
                    temperature,
                    max_tokens,
                )
                .await
            {
                Ok((raw_msg, response)) => {
                    match response {
                        ChatResponse::Text(text) => {
                            final_text = text;
                            break;
                        }
                        ChatResponse::ToolCalls(calls) => {
                            // Append assistant message with tool calls to context
                            messages.push(ChatMessage {
                                role: "assistant".to_string(),
                                content: raw_msg.content,
                                tool_calls: raw_msg.tool_calls,
                                tool_call_id: None,
                            });

                            // Execute each tool call and append tool result
                            for call in calls {
                                let result = tool_registry.execute(&call).await;
                                messages.push(ChatMessage {
                                role: "tool".to_string(),
                                content: Some(result.output),
                                tool_calls: None,
                                tool_call_id: Some(result.tool_call_id),
                            });
                        }
                    }
                }
            }
            Err(e) => {
                error!("LLM chat completion with tools failed: {}. Falling back to echo.", e);
                final_text = format!("Echo: {}", prompt_text);
                break;
            }
        }
    }

    if final_text.is_empty() && iteration >= max_tool_iterations {
        warn!("Agent loop reached maximum tool iterations ({})", max_tool_iterations);
        final_text = "Task halted: maximum tool execution iterations reached.".to_string();
    }

    let outgoing = OutgoingMessage {
        id: MessageId::new(),
        session_id: msg.session_id,
        platform: msg.platform,
        content: MessageContent::Text { text: final_text },
        reply_to: Some(incoming_id),
        metadata: HashMap::new(),
    };

    match msg.platform {
        Platform::Telegram => {
            if let Some(adapter) = tg_adapter {
                if let Err(e) = adapter.send(outgoing).await {
                    error!("Failed to send Telegram reply: {:?}", e);
                } else {
                    info!("Telegram reply sent successfully");
                }
            } else {
                warn!("No active Telegram adapter available to dispatch response");
            }
        }
        platform => {
            warn!("Platform {} not yet hooked up for outbound dispatch", platform);
        }
    }
}
}

pub use hermes_config::GatewayConfig as Config;
