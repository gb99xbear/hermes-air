// hermes-gateway-core - Core gateway logic
pub mod commands;
pub mod compression;
pub mod todo;
pub mod llm;
pub mod stream;
pub mod prompt;
pub mod providers;
pub mod fuzzy_match;
pub mod tools;
pub mod web;
pub mod browser;
pub mod code_exec;
pub mod turn_processor;

pub use commands::{
    build_model_keyboard, build_provider_keyboard, default_providers_list, handle_clear,
    handle_command, handle_context, handle_diff, handle_model, handle_profile,
    handle_start_or_help, handle_status, handle_stop, handle_whoami, handle_compress, model_picker_card,
    parse_command, provider_picker_header, BusyPolicy, CommandContext, ProviderItem,
    SlashCommand, MODEL_PAGE_SIZE,
};
pub use compression::{CompressionResult, ContextCompressor, HISTORICAL_TASK_HEADING, SUMMARY_PREFIX};
pub use todo::{TodoItem, TodoStore, todo_tool, TODO_INJECTION_HEADER, VALID_STATUSES, MAX_TODO_CONTENT_CHARS, MAX_TODO_ITEMS};
pub use llm::{ChatMessage, ChatResponse, LlmClient, ToolCallRawExt};
pub use stream::GatewayStreamConsumer;
pub use prompt::PromptAssembler;
pub use providers::{ProviderProfile, ProviderRegistry};
pub use tools::{ToolCall, ToolRegistry, ToolResult};
pub use code_exec::{CodeExecutionEngine, TruncationMetadata, truncate_stdout_text, spill_full_stdout, head_tail_split};
pub use web::{web_extract, web_search, WebExtractItem, WebExtractResponse, WebSearchResponse, WebSearchResultItem};
pub use turn_processor::{TurnProcessor, TurnEvent};

use hermes_config::GatewayConfig;
use hermes_platform_adapter::PlatformAdapter;
use hermes_platform_telegram::{TelegramAdapter, TelegramConfig};
use hermes_protocol::{IncomingMessage, MessageContent, MessageId, OutgoingMessage, Platform, PlatformError, PlatformEvent};
use hermes_relay::{WsRelayConfig, WsRelayServer};
use hermes_session_store::{SessionStore, SessionStoreFactory};
use dashmap::DashMap;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

/// State of an active model picker for a Telegram chat
#[derive(Debug, Clone)]
pub struct ModelPickerState {
    pub message_id: i64,
    pub chat_id: i64,
    pub session_id: uuid::Uuid,
    pub selected_provider: String,
    pub selected_provider_name: String,
    pub current_model: String,
    pub current_provider: String,
    pub model_list: Vec<String>,
    pub model_page: usize,
}

#[allow(dead_code)]
pub struct Gateway {
    config: GatewayConfig,
    relay_server: Arc<WsRelayServer>,
    session_store: Arc<Box<dyn SessionStore>>,
    llm_client: Arc<LlmClient>,
    provider_registry: Arc<ProviderRegistry>,
    tool_registry: Arc<ToolRegistry>,
    prompt_assembler: Arc<PromptAssembler>,
    compressor: Arc<tokio::sync::Mutex<ContextCompressor>>,
    event_tx: mpsc::UnboundedSender<PlatformEvent>,
    event_rx: Option<mpsc::UnboundedReceiver<PlatformEvent>>,
    model_picker_states: Arc<RwLock<HashMap<i64, ModelPickerState>>>,
    active_turns: Arc<DashMap<uuid::Uuid, CancellationToken>>,
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

        let env_api_key = std::env::var("ROUTER9_API_KEY")
            .or_else(|_| std::env::var("NINEROUTER_API_KEY"))
            .or_else(|_| std::env::var("OPENAI_API_KEY"))
            .ok();

        let llm_client = Arc::new(LlmClient::from_provider_profile(
            &provider_profile,
            env_base_url,
            env_api_key,
        ));

        let tool_registry = Arc::new(ToolRegistry::new());
        let prompt_assembler = Arc::new(PromptAssembler::new());

        let active_turns = Arc::new(DashMap::new());
        let compressor = Arc::new(tokio::sync::Mutex::new(ContextCompressor::default()));

        Ok(Self {
            config,
            relay_server,
            session_store: Arc::new(session_store),
            llm_client,
            provider_registry,
            tool_registry,
            prompt_assembler,
            compressor,
            event_tx,
            event_rx: Some(event_rx),
            model_picker_states: Arc::new(RwLock::new(HashMap::new())),
            active_turns,
        })
    }

    pub fn provider_registry(&self) -> Arc<ProviderRegistry> {
        self.provider_registry.clone()
    }

    pub fn llm_client(&self) -> Arc<LlmClient> {
        self.llm_client.clone()
    }

    pub fn active_turns(&self) -> Arc<DashMap<uuid::Uuid, CancellationToken>> {
        self.active_turns.clone()
    }

    pub fn is_session_busy(&self, session_id: &uuid::Uuid) -> bool {
        self.active_turns.contains_key(session_id)
    }

    pub fn cancel_session_turn(&self, session_id: &uuid::Uuid) -> bool {
        if let Some((_, token)) = self.active_turns.remove(session_id) {
            token.cancel();
            true
        } else {
            false
        }
    }

    pub fn turn_processor(&self) -> TurnProcessor {
        let default_profile = self
            .config
            .profiles
            .get(&self.config.gateway.default_profile)
            .cloned()
            .or_else(|| self.config.profiles.values().next().cloned());

        TurnProcessor::new(
            self.llm_client.clone(),
            self.tool_registry.clone(),
            self.prompt_assembler.clone(),
            self.session_store.clone(),
            self.compressor.clone(),
            default_profile,
        )
    }

    pub fn session_store(&self) -> Arc<Box<dyn SessionStore>> {
        self.session_store.clone()
    }


    pub async fn run(mut self) -> anyhow::Result<()> {
        info!("Starting Hermes Air Gateway");

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
        let config_arc = Arc::new(self.config.clone());
        let provider_registry = self.provider_registry.clone();
        let model_picker_states = self.model_picker_states.clone();
        let active_turns = self.active_turns.clone();
        let compressor = self.compressor.clone();

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
                            &provider_registry,
                            &model_picker_states,
                            &default_profile,
                            &event_loop_tg,
                            &active_turns,
                            &compressor,
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

        info!("Hermes Air Gateway running");

        // Keep running until shutdown signal
        tokio::signal::ctrl_c().await?;
        info!("Shutting down");

        Ok(())
    }

    /// Fetch models available for a given provider
    pub async fn fetch_provider_models(
        provider_slug: &str,
        provider_registry: &Arc<ProviderRegistry>,
    ) -> Vec<String> {
        let slug_lower = provider_slug.to_lowercase();
        if slug_lower.contains("9router") || slug_lower == "9router" {
            // Live query 9Router models API
            let url = "http://127.0.0.1:20128/v1/models";
            let client = reqwest::Client::new();
            let mut req = client.get(url).timeout(std::time::Duration::from_secs(3));
            if let Ok(key) = std::env::var("ROUTER9_API_KEY") {
                req = req.bearer_auth(key);
            }
            if let Ok(resp) = req.send().await {
                if let Ok(body) = resp.json::<serde_json::Value>().await {
                    if let Some(data) = body.get("data").and_then(|d| d.as_array()) {
                        let mut models: Vec<String> = data
                            .iter()
                            .filter_map(|m| m.get("id").and_then(|id| id.as_str()).map(|s| s.to_string()))
                            .collect();
                        if !models.is_empty() {
                            models.sort();
                            return models;
                        }
                    }
                }
            }
        }

        // Fallback to provider profile catalog
        if let Some(profile) = provider_registry.get(provider_slug).await {
            if !profile.fallback_models.is_empty() {
                return profile.fallback_models;
            }
        }

        match slug_lower.as_str() {
            "tokenharbor" => vec![
                "thk/deepseek-v4.1-flash:free".to_string(),
                "thk/mimo-v2.6-flash:free".to_string(),
                "thk/gemini-2.5-flash:free".to_string(),
                "thk/claude-3-5-haiku:free".to_string(),
            ],
            "openrouter" => vec![
                "anthropic/claude-sonnet-4.6".to_string(),
                "openai/gpt-5.4".to_string(),
                "deepseek/deepseek-chat".to_string(),
                "google/gemini-3.8-flash".to_string(),
            ],
            "anthropic" => vec![
                "claude-3-5-sonnet-20241022".to_string(),
                "claude-3-5-haiku-20241022".to_string(),
            ],
            "openai" => vec![
                "gpt-4o".to_string(),
                "gpt-4o-mini".to_string(),
                "o1".to_string(),
                "o3-mini".to_string(),
            ],
            _ => vec![format!("{}-default", provider_slug)],
        }
    }

    /// Handle Telegram Callback Queries from the Interactive Model Picker
    pub async fn handle_callback_query(
        callback_data: &str,
        callback_query_id: &str,
        chat_id: i64,
        message_id: i64,
        tg_adapter: &TelegramAdapter,
        session_store: &Arc<Box<dyn SessionStore>>,
        provider_registry: &Arc<ProviderRegistry>,
        picker_states: &Arc<RwLock<HashMap<i64, ModelPickerState>>>,
    ) -> Result<(), PlatformError> {
        info!(
            "[model-picker] Callback: data='{}', query_id='{}', chat_id={}, message_id={}",
            callback_data, callback_query_id, chat_id, message_id
        );

        // 1. Cancel action: mx:cancel or mx
        if callback_data == "mx:cancel" || callback_data == "mx" {
            let _ = tg_adapter.answer_callback_query(callback_query_id, Some("Cancelled")).await;
            picker_states.write().await.remove(&chat_id);
            return tg_adapter
                .edit_message_with_keyboard(
                    chat_id,
                    message_id,
                    "<i>Model selection cancelled.</i>",
                    None,
                )
                .await;
        }

        // No-op (e.g. current page counter button)
        if callback_data == "mx:noop" {
            return tg_adapter.answer_callback_query(callback_query_id, None).await;
        }

        // 2. Back to Provider Selection: mb:back or mb
        if callback_data == "mb:back" || callback_data == "mb" {
            let state_opt = picker_states.read().await.get(&chat_id).cloned();
            let (active_model, active_provider) = if let Some(st) = state_opt {
                (st.current_model, st.current_provider)
            } else {
                ("unknown".to_string(), "9router".to_string())
            };

            let providers = default_providers_list();
            let keyboard = build_provider_keyboard(&providers, &active_provider);
            let header = provider_picker_header(&active_model, &active_provider);

            let _ = tg_adapter.answer_callback_query(callback_query_id, None).await;
            return tg_adapter
                .edit_message_with_keyboard(chat_id, message_id, &header, Some(keyboard))
                .await;
        }

        // 3. Provider selected: mp:<provider_slug>
        if let Some(provider_slug) = callback_data.strip_prefix("mp:") {
            let _ = tg_adapter.answer_callback_query(callback_query_id, None).await;

            let providers = default_providers_list();
            let p_name = providers
                .iter()
                .find(|p| p.slug == provider_slug)
                .map(|p| p.name.clone())
                .unwrap_or_else(|| provider_slug.to_string());

            let mut state = picker_states
                .read()
                .await
                .get(&chat_id)
                .cloned()
                .unwrap_or_else(|| {
                    let bytes = chat_id.to_le_bytes();
                    let mut uuid_bytes = [0u8; 16];
                    uuid_bytes[..8].copy_from_slice(&bytes);
                    let session_id = uuid::Uuid::from_bytes(uuid_bytes);
                    ModelPickerState {
                        message_id,
                        chat_id,
                        session_id,
                        selected_provider: provider_slug.to_string(),
                        selected_provider_name: p_name.clone(),
                        current_model: "gemini-2.5-flash".to_string(),
                        current_provider: "9router".to_string(),
                        model_list: Vec::new(),
                        model_page: 0,
                    }
                });

            state.selected_provider = provider_slug.to_string();
            state.selected_provider_name = p_name.clone();
            state.model_page = 0;

            // Fetch models for provider
            let models = Self::fetch_provider_models(provider_slug, provider_registry).await;
            state.model_list = models.clone();

            picker_states.write().await.insert(chat_id, state.clone());

            let (keyboard, cur_page, total_pages) =
                build_model_keyboard(&models, 0, &state.current_model, provider_slug);

            let start = 0;
            let end = std::cmp::min(MODEL_PAGE_SIZE, models.len());
            let page_models = if !models.is_empty() { &models[start..end] } else { &[] };

            let text = model_picker_card(
                &p_name,
                models.len(),
                cur_page,
                total_pages,
                page_models,
                &state.current_model,
            );

            return tg_adapter
                .edit_message_with_keyboard(chat_id, message_id, &text, Some(keyboard))
                .await;
        }

        // 4. Model Page Navigation: mg:<provider_slug>:<page>
        if let Some(rest) = callback_data.strip_prefix("mg:") {
            let parts: Vec<&str> = rest.split(':').collect();
            if parts.len() >= 2 {
                let provider_slug = parts[0];
                let target_page: usize = parts[1].parse().unwrap_or(0);

                let mut state_opt = picker_states.read().await.get(&chat_id).cloned();
                if let Some(ref mut state) = state_opt {
                    state.model_page = target_page;
                    picker_states.write().await.insert(chat_id, state.clone());

                    let models = &state.model_list;
                    let (keyboard, cur_page, total_pages) =
                        build_model_keyboard(models, target_page, &state.current_model, provider_slug);

                    let start = target_page * MODEL_PAGE_SIZE;
                    let end = std::cmp::min(start + MODEL_PAGE_SIZE, models.len());
                    let page_models = if start < models.len() { &models[start..end] } else { &[] };

                    let text = model_picker_card(
                        &state.selected_provider_name,
                        models.len(),
                        cur_page,
                        total_pages,
                        page_models,
                        &state.current_model,
                    );

                    let _ = tg_adapter.answer_callback_query(callback_query_id, None).await;
                    return tg_adapter
                        .edit_message_with_keyboard(chat_id, message_id, &text, Some(keyboard))
                        .await;
                }
            }
        }

        // 5. Model Refresh: mrf:<provider_slug> or mrf:prov
        if let Some(target) = callback_data.strip_prefix("mrf:") {
            if target == "prov" {
                let state_opt = picker_states.read().await.get(&chat_id).cloned();
                let (active_model, active_provider) = if let Some(st) = state_opt {
                    (st.current_model, st.current_provider)
                } else {
                    ("unknown".to_string(), "9router".to_string())
                };

                let providers = default_providers_list();
                let keyboard = build_provider_keyboard(&providers, &active_provider);
                let header = provider_picker_header(&active_model, &active_provider);

                let _ = tg_adapter.answer_callback_query(callback_query_id, Some("Providers refreshed")).await;
                return tg_adapter
                    .edit_message_with_keyboard(chat_id, message_id, &header, Some(keyboard))
                    .await;
            } else {
                let provider_slug = target;
                let mut state_opt = picker_states.read().await.get(&chat_id).cloned();
                if let Some(ref mut state) = state_opt {
                    let models = Self::fetch_provider_models(provider_slug, provider_registry).await;
                    state.model_list = models.clone();
                    picker_states.write().await.insert(chat_id, state.clone());

                    let (keyboard, cur_page, total_pages) =
                        build_model_keyboard(&models, state.model_page, &state.current_model, provider_slug);

                    let start = state.model_page * MODEL_PAGE_SIZE;
                    let end = std::cmp::min(start + MODEL_PAGE_SIZE, models.len());
                    let page_models = if start < models.len() { &models[start..end] } else { &[] };

                    let text = model_picker_card(
                        &state.selected_provider_name,
                        models.len(),
                        cur_page,
                        total_pages,
                        page_models,
                        &state.current_model,
                    );

                    let _ = tg_adapter.answer_callback_query(callback_query_id, Some("Models refreshed")).await;
                    return tg_adapter
                        .edit_message_with_keyboard(chat_id, message_id, &text, Some(keyboard))
                        .await;
                }
            }
        }

        // 6. Model Selected: mm:<model_id>
        if let Some(new_model) = callback_data.strip_prefix("mm:") {
            let state_opt = picker_states.read().await.get(&chat_id).cloned();
            let session_id = if let Some(ref st) = state_opt {
                st.session_id
            } else {
                let bytes = chat_id.to_le_bytes();
                let mut uuid_bytes = [0u8; 16];
                uuid_bytes[..8].copy_from_slice(&bytes);
                uuid::Uuid::from_bytes(uuid_bytes)
            };

            // Switch session model override in session store!
            if let Err(e) = session_store
                .set_model_override(session_id, Some(new_model.to_string()))
                .await
            {
                warn!("Failed to persist model override: {:?}", e);
            }

            picker_states.write().await.remove(&chat_id);

            let confirmation_text = format!(
                "⚡ <b>HERMES ROUTER // MODEL SELECTOR</b>\n\n⭐ <b>Active model switched to:</b> <code>{}</code>\n\nSubsequent conversation turns will use this model.",
                new_model
            );

            let _ = tg_adapter
                .answer_callback_query(callback_query_id, Some(&format!("Model switched to {}", new_model)))
                .await;

            return tg_adapter
                .edit_message_with_keyboard(chat_id, message_id, &confirmation_text, None)
                .await;
        }

        let _ = tg_adapter.answer_callback_query(callback_query_id, None).await;
        Ok(())
    }

    async fn process_incoming(
        msg: IncomingMessage,
        config: &Arc<GatewayConfig>,
        llm_client: &Arc<LlmClient>,
        tool_registry: &Arc<ToolRegistry>,
        prompt_assembler: &Arc<PromptAssembler>,
        session_store: &Arc<Box<dyn SessionStore>>,
        provider_registry: &Arc<ProviderRegistry>,
        model_picker_states: &Arc<RwLock<HashMap<i64, ModelPickerState>>>,
        profile: &Option<hermes_config::ProfileConfig>,
        tg_adapter: &Option<Arc<TelegramAdapter>>,
        active_turns: &Arc<DashMap<uuid::Uuid, CancellationToken>>,
        compressor: &Arc<tokio::sync::Mutex<ContextCompressor>>,
    ) {
        // Handle Callback Query updates from Telegram (e.g. model selector taps)
        if let MessageContent::System { ref event, ref data } = msg.content {
            if event == "callback_query" {
                if let Some(ref adapter) = tg_adapter {
                    let callback_data = data
                        .get("callback_data")
                        .or_else(|| data.get("data"))
                        .and_then(|d| d.as_str())
                        .unwrap_or_default();

                    let callback_query_id = data
                        .get("callback_query_id")
                        .and_then(|id| id.as_str())
                        .unwrap_or_default();

                    let chat_id = data
                        .get("chat_id")
                        .and_then(|c| c.as_i64())
                        .unwrap_or_else(|| adapter.chat_id_from_session(msg.session_id).unwrap_or(0));

                    let message_id = data
                        .get("message_id")
                        .and_then(|m| m.as_i64())
                        .unwrap_or(0);

                    if let Err(e) = Self::handle_callback_query(
                        callback_data,
                        callback_query_id,
                        chat_id,
                        message_id,
                        adapter,
                        session_store,
                        provider_registry,
                        model_picker_states,
                    )
                    .await
                    {
                        error!("[model-picker] Failed handling callback query: {:?}", e);
                    }
                }
                return;
            }
        }

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

        // Query session-scoped model override from session store (1-to-1 matching Python gateway/session.py)
        let session_override = match session_store.get_model_override(msg.session_id).await {
            Ok(ov) => ov,
            Err(e) => {
                warn!("Failed to query model override for session {}: {:?}", msg.session_id, e);
                None
            }
        };

        let active_model = session_override.unwrap_or_else(|| default_model.to_string());

        let profile_name = profile
            .as_ref()
            .map(|p| p.name.as_str())
            .unwrap_or_else(|| config.gateway.default_profile.as_str());

        let active_provider = profile
            .as_ref()
            .map(|p| p.model_provider.as_str())
            .unwrap_or("9router");

        // Check if incoming text is a Slash Command
        if let Some(cmd) = parse_command(&prompt_text) {
            info!("Detected slash command: {:?}", cmd);

            let is_busy = active_turns.contains_key(&msg.session_id);
            let policy = cmd.busy_policy();

            // 1. Busy guard policy check
            if is_busy {
                match policy {
                    BusyPolicy::Reject => {
                        info!("Rejecting slash command /{} for busy session {}", cmd.name(), msg.session_id);
                        let reject_text = format!(
                            "⏳ Agent is running — `/{}` can't run mid-turn. Wait for the current response or `/stop` first.",
                            cmd.name()
                        );
                        let outgoing = OutgoingMessage {
                            id: MessageId::new(),
                            session_id: msg.session_id,
                            platform: msg.platform,
                            reply_to: Some(incoming_id),
                            content: MessageContent::Text { text: reject_text },
                            metadata: HashMap::new(),
                        };
                        Self::dispatch_outgoing(outgoing, tg_adapter).await;
                        return;
                    }
                    BusyPolicy::InterruptThenDispatch => {
                        info!("Interrupting active turn for busy session {} before executing /{}", msg.session_id, cmd.name());
                        if let Some((_, token)) = active_turns.remove(&msg.session_id) {
                            token.cancel();
                            info!("CancellationToken triggered for session {}", msg.session_id);
                        }
                    }
                    BusyPolicy::Dispatch => {
                        info!("Dispatching concurrent slash command /{} mid-turn for session {}", cmd.name(), msg.session_id);
                    }
                }
            }

            // If command is /model with no arguments and Telegram adapter is active,
            // render the Interactive Inline Keyboard Model Selector!
            if matches!(cmd, SlashCommand::Model(None)) && tg_adapter.is_some() && msg.platform == Platform::Telegram {
                if let Some(adapter) = tg_adapter {
                    if let Ok(chat_id) = adapter.chat_id_from_session(msg.session_id) {
                        let providers = default_providers_list();
                        let keyboard = build_provider_keyboard(&providers, active_provider);
                        let header = provider_picker_header(&active_model, active_provider);

                        match adapter.send_message_with_keyboard(chat_id, &header, keyboard).await {
                            Ok(sent) => {
                                let state = ModelPickerState {
                                    message_id: sent.message_id,
                                    chat_id,
                                    session_id: msg.session_id,
                                    selected_provider: active_provider.to_string(),
                                    selected_provider_name: "9Router Gateway".to_string(),
                                    current_model: active_model.clone(),
                                    current_provider: active_provider.to_string(),
                                    model_list: Vec::new(),
                                    model_page: 0,
                                };
                                model_picker_states.write().await.insert(chat_id, state);
                                info!("[model-picker] Sent interactive picker message {} for chat {}", sent.message_id, chat_id);
                                return;
                            }
                            Err(e) => {
                                warn!("[model-picker] Failed to send interactive picker: {:?}, falling back to text", e);
                            }
                        }
                    }
                }
            }

            let is_clear = matches!(cmd, SlashCommand::Clear);
            let is_stop = matches!(cmd, SlashCommand::Stop);

            if is_stop {
                if let Some((_, token)) = active_turns.remove(&msg.session_id) {
                    token.cancel();
                    info!("Cancelled running turn on /stop for session {}", msg.session_id);
                }
            }

            // Context window metadata calculation for /context and /status
            let history_for_stats = session_store
                .get_messages(msg.session_id, Some(100))
                .await
                .unwrap_or_default();
            let transcript_count = history_for_stats.len();
            let estimated_prompt_tokens: usize = history_for_stats
                .iter()
                .map(|m| m.content.as_deref().unwrap_or("").chars().count() / 4)
                .sum();

            let context_window = profile
                .as_ref()
                .map(|p| p.max_tokens as usize)
                .unwrap_or(128_000);

            let raw_chat_id = msg.raw.as_ref().and_then(|r| r.get("chat")).and_then(|c| c.get("id")).and_then(|id| id.as_i64()).map(|i| i.to_string());
            let raw_user_id = msg.raw.as_ref().and_then(|r| r.get("from")).and_then(|f| f.get("id")).and_then(|id| id.as_i64()).map(|i| i.to_string());
            let raw_chat_type = msg.raw.as_ref().and_then(|r| r.get("chat")).and_then(|c| c.get("type")).and_then(|t| t.as_str());
            let raw_thread_id = msg.raw.as_ref().and_then(|r| r.get("message_thread_id")).and_then(|t| t.as_i64()).map(|i| i.to_string());

            let platform_str = msg.platform.as_str();

            let ctx = CommandContext {
                gateway_name: &config.gateway.name,
                gateway_version: &config.gateway.version,
                active_profile: profile_name,
                active_model: &active_model,
                telegram_connected: tg_adapter.is_some(),
                memory_stats: None,
                session_id: Some(msg.session_id),
                platform: Some(platform_str),
                user_id: raw_user_id.as_deref(),
                chat_id: raw_chat_id.as_deref(),
                chat_type: raw_chat_type,
                thread_id: raw_thread_id.as_deref(),
                transcript_count,
                estimated_prompt_tokens,
                context_window,
            };

            let reply_text = match cmd {
                SlashCommand::Model(Some(ref target)) => {
                    let trimmed_target = target.trim().to_string();
                    if let Err(e) = session_store.set_model_override(msg.session_id, Some(trimmed_target.clone())).await {
                        warn!("Failed to persist model override for session {}: {:?}", msg.session_id, e);
                    } else {
                        info!("Session {} active model dynamically switched to: {}", msg.session_id, trimmed_target);
                    }
                    handle_model(&Some(target.clone()), &active_model, |_| Ok(()))
                }
                SlashCommand::Compress(ref focus) => {
                    let mut comp = compressor.lock().await;
                    match comp.compress_session_with_todo(
                        msg.session_id,
                        session_store,
                        llm_client,
                        &active_model,
                        focus.as_deref(),
                        true,
                        Some(&tool_registry.todo_store),
                    ).await {
                        Ok(res) => {
                            if res.compacted {
                                handle_compress(res.pre_count, res.post_count, res.tokens_saved)
                            } else {
                                format!("ℹ️ Session context is already compact ({} messages <= protected tail turns).", res.pre_count)
                            }
                        }
                        Err(e) => {
                            warn!("Manual compression failed for session {}: {:?}", msg.session_id, e);
                            format!("❌ Failed to compact context: {}", e)
                        }
                    }
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
                if let Err(e) = session_store.set_model_override(msg.session_id, None).await {
                    warn!("Failed to clear model override for session {} on /clear: {:?}", msg.session_id, e);
                } else {
                    info!("Cleared model override for session {}", msg.session_id);
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

        // Register turn in active_turns tracker with CancellationToken
        let turn_token = CancellationToken::new();
        active_turns.insert(msg.session_id, turn_token.clone());

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

        // Autonomous Pre-Flight Turn Gate: Check token occupancy & auto-compress if threshold exceeded
        let context_window = profile
            .as_ref()
            .map(|p| p.max_tokens as usize)
            .unwrap_or(128_000);

        let current_history = session_store
            .get_messages(msg.session_id, None)
            .await
            .unwrap_or_default();

        let current_tokens = ContextCompressor::estimate_tokens(&current_history);
        {
            let mut comp = compressor.lock().await;
            if comp.should_compress(current_tokens, context_window) {
                info!(
                    "Pre-flight compression triggered for session {}: ~{} tokens >= threshold {} of context_window {}",
                    msg.session_id, current_tokens, comp.threshold, context_window
                );
                if let Err(e) = comp.compress_session_with_todo(
                    msg.session_id,
                    session_store,
                    llm_client,
                    &active_model,
                    None,
                    false,
                    Some(&tool_registry.todo_store),
                ).await {
                    warn!("Autonomous pre-flight compaction failed for session {}: {:?}", msg.session_id, e);
                }
            }
        }

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
        let max_tool_iterations = 30;
        let mut iteration = 0;
        let mut final_text = String::new();
        let mut streamed_delivery = false;

        // If tool definitions are empty and Telegram adapter is active, stream directly on single call!
        if tool_definitions.is_empty() && tg_adapter.is_some() && msg.platform == Platform::Telegram {
            if let Some(adapter) = tg_adapter {
                if let Ok(chat_id) = adapter.chat_id_from_session(msg.session_id) {
                    if turn_token.is_cancelled() {
                        info!("Turn cancelled before stream start for session {}", msg.session_id);
                        active_turns.remove(&msg.session_id);
                        return;
                    }

                    match llm_client
                        .chat_completion_stream(&active_model, &messages, temperature, max_tokens)
                        .await
                    {
                        Ok(rx) => {
                            info!("Streaming chat response directly via GatewayStreamConsumer for chat_id: {}", chat_id);
                            let mut consumer = GatewayStreamConsumer::new(chat_id);
                            let reply_to_id: Option<i64> = None; // reply_to_mode=off parity with Python Hermes
                            match consumer.consume_stream(rx, adapter, reply_to_id).await {
                                Ok(acc) => {
                                    if turn_token.is_cancelled() {
                                        info!("Turn cancelled during/after stream consumption for session {}", msg.session_id);
                                        active_turns.remove(&msg.session_id);
                                        return;
                                    }
                                    streamed_delivery = true;
                                    final_text = acc.clone();
                                    let asst_msg = ChatMessage::assistant(acc.clone());
                                    if let Err(e) = session_store.append_message(msg.session_id, asst_msg).await {
                                        warn!("Failed to persist assistant message for session {}: {:?}", msg.session_id, e);
                                    }
                                    // Also check for MEDIA images in streamed response
                                    for img_path in Self::extract_media_image_paths(&acc) {
                                        let p = std::path::Path::new(&img_path);
                                        if p.exists() {
                                            let reply_to_id: Option<i64> = None; // reply_to_mode=off parity with Python Hermes
                                            let _ = adapter.send_photo_file(chat_id, None, p, None, reply_to_id).await;
                                        }
                                    }
                                }
                                Err(e) => {
                                    warn!("GatewayStreamConsumer failed: {:?}, falling back to non-streamed text", e);
                                }
                            }
                        }
                        Err(e) => {
                            error!("Failed to initiate LLM stream: {:?}", e);
                            final_text = format!("⚠️ Upstream LLM error: {}. Sila guna arahan /model untuk tukar model.", e);
                            let fallback_msg = ChatMessage::assistant(final_text.clone());
                            let _ = session_store.append_message(msg.session_id, fallback_msg).await;
                        }
                    }
                }
            }
        }

// If not delivered via direct stream, run recursive tool loop
        if !streamed_delivery && final_text.is_empty() {
            while iteration < max_tool_iterations {
                if turn_token.is_cancelled() {
                    info!("Turn cancelled before iteration {} for session {}", iteration + 1, msg.session_id);
                    active_turns.remove(&msg.session_id);
                    return;
                }

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
                                final_text = text.clone();
                                // Persist final assistant response to session store
                                let asst_msg = ChatMessage::assistant(text.clone());
                                if let Err(e) = session_store.append_message(msg.session_id, asst_msg).await {
                                    warn!("Failed to persist assistant message for session {}: {:?}", msg.session_id, e);
                                }

                                // If Telegram adapter is active and we completed after tool execution, stream or send final text
                                if tg_adapter.is_some() && msg.platform == Platform::Telegram {
                                    if let Some(adapter) = tg_adapter {
                                        if let Ok(chat_id) = adapter.chat_id_from_session(msg.session_id) {
                                            let reply_to_id: Option<i64> = None; // reply_to_mode=off parity with Python Hermes
                                            let mut consumer = GatewayStreamConsumer::new(chat_id);
                                            // Deliver final completed text smoothly via stream consumer
                                            let (tx, rx) = tokio::sync::mpsc::channel(1);
                                            let _ = tx.send(text.clone()).await;
                                            drop(tx);
                                            let _ = consumer.consume_stream(rx, adapter, reply_to_id).await;
                                            streamed_delivery = true;
                                            // Deliver MEDIA images if present
                                            for img_path in Self::extract_media_image_paths(&text) {
                                                let p = std::path::Path::new(&img_path);
                                                if p.exists() {
                                                    let _ = adapter.send_photo_file(chat_id, None, p, None, reply_to_id).await;
                                                }
                                            }
                                        }
                                    }
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

                                // Telegram live tool progress status message
                                let mut progress_bubble_id: Option<(i64, i64)> = None; // (chat_id, message_id)
                                let tg_chat_id = if let Some(ref adapter) = tg_adapter {
                                    if msg.platform == Platform::Telegram {
                                        adapter.chat_id_from_session(msg.session_id).ok()
                                    } else {
                                        None
                                    }
                                } else {
                                    None
                                };

                                // Execute each tool call and append tool result to context and store
                                for call in calls {
                                    if turn_token.is_cancelled() {
                                        info!("Turn cancelled mid-tool execution for session {}", msg.session_id);
                                        if let (Some(ref adapter), Some((cid, mid))) = (&tg_adapter, progress_bubble_id) {
                                            // keep progress bubble permanently
                                        }
                                        active_turns.remove(&msg.session_id);
                                        return;
                                    }

                                    let progress_text = format!("🔧 Running tool: {}...", call.name);
                                    if let (Some(ref adapter), Some(cid)) = (&tg_adapter, tg_chat_id) {
                                        let adapter_clone = adapter.clone();
                                        tokio::spawn(async move {
                                            let _ = adapter_clone.send_chat_action(cid, "typing").await;
                                        });

                                        match progress_bubble_id {
                                            Some((cid, mid)) => {
                                                let _ = adapter.edit_text(cid, mid, &progress_text).await;
                                            }
                                            None => {
                                                let reply_to_id: Option<i64> = None; // reply_to_mode=off parity with Python Hermes
                                                if let Ok(sent) = adapter.send_chunk_with_fallback(cid, &progress_text, reply_to_id).await {
                                                    progress_bubble_id = Some((cid, sent.message_id));
                                                }
                                            }
                                        }
                                    }

                                    info!("Executing tool call {} (id: {}) in agent loop", call.name, call.id);
                                    let result = tool_registry.execute(&call).await;

                                    // If tool output directly emits MEDIA image tags, also deliver them
                                    if let (Some(ref adapter), Some(cid)) = (&tg_adapter, tg_chat_id) {
                                        for img_path in Self::extract_media_image_paths(&result.output) {
                                            let p = std::path::Path::new(&img_path);
                                            if p.exists() {
                                                let reply_to_id: Option<i64> = None; // reply_to_mode=off parity with Python Hermes
                                                let _ = adapter.send_photo_file(cid, None, p, None, reply_to_id).await;
                                            }
                                        }
                                    }

                                    let tool_msg = ChatMessage::tool(result.tool_call_id, result.output);
                                    if let Err(e) = session_store.append_message(msg.session_id, tool_msg.clone()).await {
                                        warn!("Failed to persist tool result for session {}: {:?}", msg.session_id, e);
                                    }
                                    messages.push(tool_msg);
                                }

                                // Delete or clean up progress bubble on tool completion
                                if let (Some(ref adapter), Some((cid, mid))) = (&tg_adapter, progress_bubble_id) {
                                    // keep progress bubble permanently
                                }
                            }
                        }
                    }
                    Err(e) => {
                        error!("LLM chat completion with tools failed: {}", e);
                        final_text = format!("⚠️ Upstream LLM error: {}. Sila guna arahan /model untuk tukar model.", e);
                        let fallback_msg = ChatMessage::assistant(final_text.clone());
                        let _ = session_store.append_message(msg.session_id, fallback_msg).await;
                        break;
                    }
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

        // Turn completed: remove from active turn tracker
        active_turns.remove(&msg.session_id);

        if !streamed_delivery {
            Self::dispatch_outgoing(outgoing, tg_adapter).await;
        }
    }

    pub fn extract_media_image_paths(text: &str) -> Vec<String> {
    let mut images = Vec::new();
    let image_extensions = [".png", ".jpg", ".jpeg", ".webp"];
    let mut search_from = 0;
    while let Some(idx) = text[search_from..].find("MEDIA:") {
        let start = search_from + idx + "MEDIA:".len();
        let rem = &text[start..];
        let end = rem.find(|c: char| c.is_whitespace() || c == '`' || c == '"' || c == '\'').unwrap_or(rem.len());
        let raw_path = rem[..end].trim();
        let lower = raw_path.to_lowercase();
        if image_extensions.iter().any(|ext| lower.ends_with(ext)) {
            images.push(raw_path.to_string());
        }
        search_from = start + end;
    }
    images
}

    async fn dispatch_outgoing(outgoing: OutgoingMessage, tg_adapter: &Option<Arc<TelegramAdapter>>) {
        match outgoing.platform {
            Platform::Telegram => {
                if let Some(adapter) = tg_adapter {
                    // Check if outgoing text contains MEDIA:<path> tags for native image delivery
                    let mut media_images = Vec::new();
                    if let MessageContent::Text { ref text } = outgoing.content {
                        media_images = Self::extract_media_image_paths(text);
                    }

                    if let Err(e) = adapter.send(outgoing.clone()).await {
                        error!("Failed to send Telegram reply: {:?}", e);
                    } else {
                        info!("Telegram reply sent successfully");
                    }

                    // For every MEDIA image file that exists on disk, send native photo
                    for img_path in media_images {
                        let path = std::path::Path::new(&img_path);
                        if path.exists() {
                            info!("Delivering native Telegram photo for MEDIA tag: {}", img_path);
                            let img_msg = OutgoingMessage {
                                id: MessageId::new(),
                                session_id: outgoing.session_id,
                                platform: Platform::Telegram,
                                reply_to: outgoing.reply_to,
                                content: MessageContent::Image {
                                    url: img_path.clone(),
                                    caption: None,
                                },
                                metadata: outgoing.metadata.clone(),
                            };
                            if let Err(e) = adapter.send(img_msg).await {
                                warn!("Failed to deliver native Telegram photo for {}: {:?}", img_path, e);
                            }
                        }
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
