// hermes-gateway-core - Core gateway logic
pub mod commands;
pub mod llm;
pub mod stream;
pub mod prompt;
pub mod providers;
pub mod tools;

pub use commands::{
    handle_clear, handle_command, handle_model, handle_start_or_help, handle_status,
    parse_command, CommandContext, SlashCommand,
};
pub use llm::{ChatMessage, ChatResponse, LlmClient, ToolCallRawExt};
pub use stream::GatewayStreamConsumer;
pub use prompt::PromptAssembler;
pub use providers::{ProviderProfile, ProviderRegistry};
pub use tools::{ToolCall, ToolRegistry, ToolResult};

use hermes_config::GatewayConfig;
use hermes_platform_adapter::PlatformAdapter;
use hermes_platform_telegram::{TelegramAdapter, TelegramConfig};
use hermes_protocol::{IncomingMessage, MessageContent, MessageId, OutgoingMessage, Platform, PlatformEvent};
use hermes_relay::{WsRelayConfig, WsRelayServer};
use hermes_session_store::{SessionStore, SessionStoreFactory};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock};
use tracing::{error, info, warn};

#[allow(dead_code)]
pub struct Gateway {
    config: GatewayConfig,
    relay_server: Arc<WsRelayServer>,
    session_store: Arc<Box<dyn SessionStore>>,
    llm_client: Arc<LlmClient>,
    provider_registry: Arc<ProviderRegistry>,
    tool_registry: Arc<ToolRegistry>,
    prompt_assembler: Arc<PromptAssembler>,
    event_tx: mpsc::UnboundedSender<PlatformEvent>,
    event_rx: Option<mpsc::UnboundedReceiver<PlatformEvent>>,
    active_model_override: Arc<RwLock<Option<String>>>,
}

impl Gateway {
    pub async fn new(config: GatewayConfig) -> anyhow::Result<Self> {
        commands::init_uptime();

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

        // Initialize ProviderRegistry with standard catalog
        let provider_registry = Arc::new(ProviderRegistry::new());

        // Determine default profile
        let default_profile = config
            .profiles
            .get(&config.gateway.default_profile)
            .cloned()
            .or_else(|| config.profiles.values().next().cloned());

        let target_provider_name = default_profile
            .as_ref()
            .map(|p| p.model_provider.as_str())
            .unwrap_or("9router");

        let provider_profile = provider_registry
            .get(target_provider_name)
            .await
            .unwrap_or_else(|| {
                info!("Provider {} not found in registry, falling back to 9router default", target_provider_name);
                provider_registry
                    .get_blocking("9router")
                    .unwrap_or_default()
            });

        // Initialize LLM client via ProviderProfile with explicit env overrides if provided:
        let env_base_url = std::env::var("LLM_BASE_URL")
            .or_else(|_| std::env::var("OPENAI_BASE_URL"))
            .ok();

        let env_api_key = std::env::var("HERMES_CUSTOM_100_75_163_92_20128_API_KEY")
            .or_else(|_| std::env::var("HERMES_CUSTOM_100_77_223_11_20128_API_KEY"))
            .or_else(|_| std::env::var("OPENAI_API_KEY"))
            .ok();

        let llm_client = Arc::new(LlmClient::from_provider_profile(
            &provider_profile,
            env_base_url,
            env_api_key,
        ));

        let tool_registry = Arc::new(ToolRegistry::new());
        let prompt_assembler = Arc::new(PromptAssembler::new());
        let active_model_override = Arc::new(RwLock::new(None));

        Ok(Self {
            config,
            relay_server,
            session_store: Arc::new(session_store),
            llm_client,
            provider_registry,
            tool_registry,
            prompt_assembler,
            event_tx,
            event_rx: Some(event_rx),
            active_model_override,
        })
    }

    pub fn provider_registry(&self) -> Arc<ProviderRegistry> {
        self.provider_registry.clone()
    }

    pub fn llm_client(&self) -> Arc<LlmClient> {
        self.llm_client.clone()
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
        let prompt_assembler = self.prompt_assembler.clone();
        let session_store = self.session_store.clone();
        let active_model_override = self.active_model_override.clone();
        let config_arc = Arc::new(self.config.clone());

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
                        Self::process_incoming(
                            msg,
                            &config_arc,
                            &llm_client,
                            &tool_registry,
                            &prompt_assembler,
                            &session_store,
                            &default_profile,
                            &event_loop_tg,
                            &active_model_override,
                        )
                        .await;
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
        config: &Arc<GatewayConfig>,
        llm_client: &Arc<LlmClient>,
        tool_registry: &Arc<ToolRegistry>,
        prompt_assembler: &Arc<PromptAssembler>,
        session_store: &Arc<Box<dyn SessionStore>>,
        profile: &Option<hermes_config::ProfileConfig>,
        tg_adapter: &Option<Arc<TelegramAdapter>>,
        active_model_override: &Arc<RwLock<Option<String>>>,
    ) {
        let (prompt_text, incoming_id) = match &msg.content {
            MessageContent::Text { text } => (text.clone(), msg.id),
            _ => {
                info!("Ignoring non-text incoming message: {:?}", msg.id);
                return;
            }
        };

        info!("Processing incoming message from {}: {}", msg.platform, prompt_text);

        let default_model = profile
            .as_ref()
            .map(|p| p.model.as_str())
            .unwrap_or("gemini-2.5-flash");

        let active_model = {
            let guard = active_model_override.read().await;
            guard.clone().unwrap_or_else(|| default_model.to_string())
        };

        let profile_name = profile
            .as_ref()
            .map(|p| p.name.as_str())
            .unwrap_or_else(|| config.gateway.default_profile.as_str());

        // Check if incoming text is a Slash Command
        if let Some(cmd) = parse_command(&prompt_text) {
            info!("Detected slash command: {:?}", cmd);

            let is_clear = matches!(cmd, SlashCommand::Clear);

            let ctx = CommandContext {
                gateway_name: &config.gateway.name,
                gateway_version: &config.gateway.version,
                active_profile: profile_name,
                active_model: &active_model,
                telegram_connected: tg_adapter.is_some(),
                memory_stats: None,
            };

            let reply_text = match cmd {
                SlashCommand::Model(Some(ref target)) => {
                    let mut guard = active_model_override.write().await;
                    *guard = Some(target.trim().to_string());
                    info!("Active model dynamically switched to: {}", target.trim());
                    handle_model(&Some(target.clone()), &active_model, |_| Ok(()))
                }
                other => handle_command(other, &ctx, |_| Ok(())),
            };

            if is_clear {
                if let Err(e) = session_store.delete(msg.session_id).await {
                    warn!("Failed to delete session {} on /clear: {:?}", msg.session_id, e);
                } else {
                    info!("Cleared session metadata for session {}", msg.session_id);
                }
                if let Err(e) = session_store.clear_messages(msg.session_id).await {
                    warn!("Failed to clear conversation history for session {} on /clear: {:?}", msg.session_id, e);
                } else {
                    info!("Cleared conversation turn history for session {}", msg.session_id);
                }
            }

            let outgoing = OutgoingMessage {
                id: MessageId::new(),
                session_id: msg.session_id,
                platform: msg.platform,
                reply_to: Some(incoming_id),
                content: MessageContent::Text { text: reply_text },
                metadata: HashMap::new(),
            };

            Self::dispatch_outgoing(outgoing, tg_adapter).await;
            return;
        }

        // Send chat action typing immediately if telegram
        if let Some(ref adapter) = tg_adapter {
            if let Ok(chat_id) = adapter.chat_id_from_session(msg.session_id) {
                let adapter_clone = adapter.clone();
                tokio::spawn(async move {
                    let _ = adapter_clone.send_chat_action(chat_id, "typing").await;
                });
            }
        }

        let system_prompt = profile
            .as_ref()
            .and_then(|p| p.system_prompt.as_deref());
        let temperature = profile.as_ref().map(|p| p.temperature).unwrap_or(0.7);
        let max_tokens = profile.as_ref().map(|p| p.max_tokens).unwrap_or(4096);

        // Build active system prompt dynamically via PromptAssembler
        let active_system_prompt = prompt_assembler.build_system_prompt(system_prompt).await;

        // 1. Load chronological session history from session store (up to last 30 messages)
        let history = session_store
            .get_messages(msg.session_id, Some(30))
            .await
            .unwrap_or_else(|e| {
                warn!("Failed to load conversation history for session {}: {:?}", msg.session_id, e);
                Vec::new()
            });

        // 2. Build full LLM message turn list:
        //    a. System prompt (active_system_prompt)
        //    b. Historical messages (user, assistant, tool)
        //    c. New incoming user message
        let mut messages = Vec::with_capacity(history.len() + 2);
        if !active_system_prompt.is_empty() {
            messages.push(ChatMessage::system(active_system_prompt));
        }
        for hist_msg in history {
            // Do not repeat persisted system messages if any, keep prompt assembler system prompt at head
            if hist_msg.role != "system" {
                messages.push(hist_msg);
            }
        }

        let new_user_msg = ChatMessage::user(prompt_text.clone());
        messages.push(new_user_msg.clone());

        // Append incoming user message to session store turn history
        if let Err(e) = session_store.append_message(msg.session_id, new_user_msg).await {
            warn!("Failed to persist incoming user message for session {}: {:?}", msg.session_id, e);
        }

        let tool_definitions = tool_registry.get_definitions();
        let max_tool_iterations = 10;
        let mut iteration = 0;
        let mut final_text = String::new();
        let mut streamed_delivery = false;

        while iteration < max_tool_iterations {
            iteration += 1;
            info!("Agent loop iteration {}/{}", iteration, max_tool_iterations);

            match llm_client
                .chat_completion_with_tools(
                    &active_model,
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
                            let streamed_text = if iteration == 1 && tg_adapter.is_some() && msg.platform == Platform::Telegram {
                                if let Some(adapter) = tg_adapter {
                                    if let Ok(chat_id) = adapter.chat_id_from_session(msg.session_id) {
                                        match llm_client
                                            .chat_completion_stream(&active_model, &messages, temperature, max_tokens)
                                            .await
                                        {
                                            Ok(rx) => {
                                                info!("Streaming chat response via GatewayStreamConsumer for chat_id: {}", chat_id);
                                                let mut consumer = GatewayStreamConsumer::new(chat_id);
                                                let reply_to_id = msg.raw.as_ref().and_then(|r| r.get("message_id")).and_then(|m| m.as_i64());
                                                match consumer.consume_stream(rx, adapter, reply_to_id).await {
                                                    Ok(acc) => {
                                                        streamed_delivery = true;
                                                        Some(acc)
                                                    }
                                                    Err(e) => {
                                                        warn!("GatewayStreamConsumer failed: {:?}, falling back to non-streamed text", e);
                                                        None
                                                    }
                                                }
                                            }
                                            Err(e) => {
                                                warn!("Failed to initiate LLM stream: {:?}, falling back to non-streamed text", e);
                                                None
                                            }
                                        }
                                    } else {
                                        None
                                    }
                                } else {
                                    None
                                }
                            } else {
                                None
                            };

                            let resolved_text = streamed_text.unwrap_or(text);
                            final_text = resolved_text.clone();
                            // Persist final assistant response to session store
                            let asst_msg = ChatMessage::assistant(resolved_text);
                            if let Err(e) = session_store.append_message(msg.session_id, asst_msg).await {
                                warn!("Failed to persist assistant message for session {}: {:?}", msg.session_id, e);
                            }
                            break;
                        }
                        ChatResponse::ToolCalls(calls) => {
                            // Append assistant message with tool calls to context and store
                            let asst_call_msg = ChatMessage {
                                role: "assistant".to_string(),
                                content: raw_msg.content,
                                tool_calls: raw_msg.tool_calls,
                                tool_call_id: None,
                            };
                            if let Err(e) = session_store.append_message(msg.session_id, asst_call_msg.clone()).await {
                                warn!("Failed to persist assistant tool calls for session {}: {:?}", msg.session_id, e);
                            }
                            messages.push(asst_call_msg);

                            // Execute each tool call and append tool result to context and store
                            for call in calls {
                                let result = tool_registry.execute(&call).await;
                                let tool_msg = ChatMessage::tool(result.tool_call_id, result.output);
                                if let Err(e) = session_store.append_message(msg.session_id, tool_msg.clone()).await {
                                    warn!("Failed to persist tool result for session {}: {:?}", msg.session_id, e);
                                }
                                messages.push(tool_msg);
                            }
                        }
                    }
                }
                Err(e) => {
                    error!("LLM chat completion with tools failed: {}. Falling back to echo.", e);
                    final_text = format!("Echo: {}", prompt_text);
                    let fallback_msg = ChatMessage::assistant(final_text.clone());
                    let _ = session_store.append_message(msg.session_id, fallback_msg).await;
                    break;
                }
            }
        }

        if final_text.is_empty() && iteration >= max_tool_iterations {
            warn!("Agent loop reached maximum tool iterations ({})", max_tool_iterations);
            final_text = "Task halted: maximum tool execution iterations reached.".to_string();
            let halt_msg = ChatMessage::assistant(final_text.clone());
            let _ = session_store.append_message(msg.session_id, halt_msg).await;
        }

        let outgoing = OutgoingMessage {
            id: MessageId::new(),
            session_id: msg.session_id,
            platform: msg.platform,
            reply_to: Some(incoming_id),
            content: MessageContent::Text { text: final_text },
            metadata: HashMap::new(),
        };

        if !streamed_delivery {
            Self::dispatch_outgoing(outgoing, tg_adapter).await;
        }
    }

    async fn dispatch_outgoing(outgoing: OutgoingMessage, tg_adapter: &Option<Arc<TelegramAdapter>>) {
        match outgoing.platform {
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
