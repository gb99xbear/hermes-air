// hermes-gateway-core - Turn Processor Engine
use crate::commands::{handle_command, parse_command, CommandContext, SlashCommand};
use crate::compression::ContextCompressor;
use crate::llm::{ChatMessage, ChatResponse, LlmClient};
use crate::prompt::PromptAssembler;
use crate::reflection::{BackgroundReviewConfig, BackgroundReviewEngine};
use crate::tools::ToolRegistry;
use hermes_config::ProfileConfig;
use hermes_session_store::SessionStore;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{error, info};
use uuid::Uuid;

/// Output event emitted by the TurnProcessor during execution of a user turn
#[derive(Debug, Clone)]
pub enum TurnEvent {
    /// Stream chunk from assistant response
    Token(String),
    /// Notification when a tool call starts
    ToolExecuting { name: String, id: String },
    /// Notification when a tool call completes
    ToolCompleted {
        name: String,
        id: String,
        success: bool,
    },
    /// Turn completed with full response text
    Finished(String),
    /// System or slash command informational response
    SlashReply(String),
    /// Session context cleared
    ContextCleared,
    /// Turn was cancelled
    Cancelled,
    /// Error during execution
    Error(String),
}

/// TurnProcessor handles autonomous multi-turn agent execution, tool calls, and streaming
#[derive(Clone)]
pub struct TurnProcessor {
    llm_client: Arc<LlmClient>,
    tool_registry: Arc<ToolRegistry>,
    prompt_assembler: Arc<PromptAssembler>,
    session_store: Arc<Box<dyn SessionStore>>,
    compressor: Arc<tokio::sync::Mutex<ContextCompressor>>,
    reflection_engine: Arc<BackgroundReviewEngine>,
    profile: Option<ProfileConfig>,
}

impl TurnProcessor {
    pub fn new(
        llm_client: Arc<LlmClient>,
        tool_registry: Arc<ToolRegistry>,
        prompt_assembler: Arc<PromptAssembler>,
        session_store: Arc<Box<dyn SessionStore>>,
        compressor: Arc<tokio::sync::Mutex<ContextCompressor>>,
        profile: Option<ProfileConfig>,
    ) -> Self {
        let reflection_engine = Arc::new(BackgroundReviewEngine::new(
            BackgroundReviewConfig::default(),
            llm_client.clone(),
            tool_registry.skill_registry.clone(),
        ));

        Self {
            llm_client,
            tool_registry,
            prompt_assembler,
            session_store,
            compressor,
            reflection_engine,
            profile,
        }
    }

    pub fn with_reflection_engine(
        mut self,
        reflection_engine: Arc<BackgroundReviewEngine>,
    ) -> Self {
        self.reflection_engine = reflection_engine;
        self
    }

    pub fn llm_client(&self) -> Arc<LlmClient> {
        self.llm_client.clone()
    }

    pub fn tool_registry(&self) -> Arc<ToolRegistry> {
        self.tool_registry.clone()
    }

    pub fn session_store(&self) -> Arc<Box<dyn SessionStore>> {
        self.session_store.clone()
    }

    pub fn reflection_engine(&self) -> Arc<BackgroundReviewEngine> {
        self.reflection_engine.clone()
    }

    /// Take the background-process notices owned by this session and append them to
    /// the transcript as one user-role message.
    ///
    /// The queue is drained whether or not anything is injected. This is the step
    /// that makes a background process observable at all: without it the watch
    /// pipeline queues completion, watch-match and heartbeat notices that nobody
    /// ever reads, so a process that finished (or failed) mid-turn stays invisible.
    async fn drain_background_notices(&self, session_id: Uuid) {
        let Some(registry) = self.tool_registry.process_registry() else {
            return;
        };
        let owner = session_id.to_string();
        let notices: Vec<String> = registry
            .take_events_for_owner(&owner)
            .iter()
            .filter_map(crate::process_notify::format_process_event)
            .collect();
        if notices.is_empty() {
            return;
        }
        // One message, so the loop sees a single injection rather than a burst.
        let _ = self
            .session_store
            .append_message(session_id, ChatMessage::user(notices.join("\n\n")))
            .await;
    }

    /// Process a turn for a given session.
    /// Emits events over the provided mpsc channel.
    pub async fn process_turn(
        &self,
        session_id: Uuid,
        user_input: &str,
        model_override: Option<&str>,
        token: CancellationToken,
        event_tx: mpsc::Sender<TurnEvent>,
    ) -> Result<String, anyhow::Error> {
        let input_trimmed = user_input.trim();
        if input_trimmed.is_empty() {
            let _ = event_tx.send(TurnEvent::Finished(String::new())).await;
            return Ok(String::new());
        }

        let default_model = self
            .profile
            .as_ref()
            .map(|p| p.model.as_str())
            .unwrap_or("ag/gemini-3.8-flash");

        let session_override = self
            .session_store
            .get_model_override(session_id)
            .await
            .unwrap_or(None);

        let active_model = model_override
            .map(|s| s.to_string())
            .or(session_override)
            .unwrap_or_else(|| default_model.to_string());

        // Check for Slash Command
        if let Some(cmd) = parse_command(input_trimmed) {
            info!("TurnProcessor executing slash command: {:?}", cmd);

            let is_clear = matches!(cmd, SlashCommand::Clear);
            let is_stop = matches!(cmd, SlashCommand::Stop);

            if is_stop {
                let _ = event_tx.send(TurnEvent::Cancelled).await;
                return Ok("Turn stopped.".to_string());
            }

            let history_for_stats = self
                .session_store
                .get_messages(session_id, Some(100))
                .await
                .unwrap_or_default();
            let transcript_count = history_for_stats.len();
            let estimated_prompt_tokens: usize = history_for_stats
                .iter()
                .map(|m| m.content.as_deref().unwrap_or("").chars().count() / 4)
                .sum();

            let context_window = self
                .profile
                .as_ref()
                .map(|p| p.max_tokens as usize)
                .unwrap_or(128_000);

            let profile_name = self
                .profile
                .as_ref()
                .map(|p| p.name.as_str())
                .unwrap_or("default");

            let ctx = CommandContext {
                gateway_name: "Hermes Air",
                gateway_version: "0.1.0",
                active_profile: profile_name,
                active_model: &active_model,
                telegram_connected: false,
                memory_stats: None,
                session_id: Some(session_id),
                platform: Some("cli"),
                user_id: Some("local"),
                chat_id: Some("cli"),
                chat_type: Some("cli"),
                thread_id: None,
                transcript_count,
                estimated_prompt_tokens,
                context_window,
            };

            let reply_text = match cmd {
                SlashCommand::Model(Some(ref target)) => {
                    let trimmed = target.trim().to_string();
                    let _ = self
                        .session_store
                        .set_model_override(session_id, Some(trimmed.clone()))
                        .await;
                    format!("Active model switched to: {}", trimmed)
                }
                SlashCommand::Compress(ref focus) => {
                    let mut comp = self.compressor.lock().await;
                    match comp
                        .compress_session_with_todo(
                            session_id,
                            &self.session_store,
                            &self.llm_client,
                            &active_model,
                            focus.as_deref(),
                            true,
                            Some(&self.tool_registry.todo_store),
                        )
                        .await
                    {
                        Ok(res) => {
                            if res.compacted {
                                format!(
                                    "Context compressed: {} -> {} messages (saved ~{} tokens)",
                                    res.pre_count, res.post_count, res.tokens_saved
                                )
                            } else {
                                format!("Context already compact ({} messages).", res.pre_count)
                            }
                        }
                        Err(e) => format!("Failed to compact context: {}", e),
                    }
                }
                other => handle_command(other, &ctx, |_| Ok(())),
            };

            if is_clear {
                let _ = self.session_store.delete(session_id).await;
                let _ = self.session_store.clear_messages(session_id).await;
                let _ = self
                    .session_store
                    .set_model_override(session_id, None)
                    .await;
                let _ = event_tx.send(TurnEvent::ContextCleared).await;
            }

            let _ = event_tx
                .send(TurnEvent::SlashReply(reply_text.clone()))
                .await;
            return Ok(reply_text);
        }

        // Pre-flight turn compression check
        let context_window = self
            .profile
            .as_ref()
            .map(|p| p.max_tokens as usize)
            .unwrap_or(128_000);

        let current_history = self
            .session_store
            .get_messages(session_id, None)
            .await
            .unwrap_or_default();

        let current_tokens = ContextCompressor::estimate_tokens(&current_history);
        {
            let mut comp = self.compressor.lock().await;
            if comp.should_compress(current_tokens, context_window) {
                let _ = comp
                    .compress_session_with_todo(
                        session_id,
                        &self.session_store,
                        &self.llm_client,
                        &active_model,
                        None,
                        false,
                        Some(&self.tool_registry.todo_store),
                    )
                    .await;
            }
        }

        // Build active system prompt
        let sys_cfg = self
            .profile
            .as_ref()
            .and_then(|p| p.system_prompt.as_deref());
        let active_system_prompt = self.prompt_assembler.build_system_prompt(sys_cfg).await;

        let temperature = self.profile.as_ref().map(|p| p.temperature).unwrap_or(0.7);
        let max_tokens = self.profile.as_ref().map(|p| p.max_tokens).unwrap_or(4096);

        // Load history (last 30 messages)
        let history = self
            .session_store
            .get_messages(session_id, Some(30))
            .await
            .unwrap_or_default();

        let mut messages = Vec::with_capacity(history.len() + 2);
        if !active_system_prompt.is_empty() {
            messages.push(ChatMessage::system(active_system_prompt));
        }
        for hist_msg in history {
            if hist_msg.role != "system" {
                messages.push(hist_msg);
            }
        }

        let new_user_msg = ChatMessage::user(input_trimmed.to_string());
        messages.push(new_user_msg.clone());
        let _ = self
            .session_store
            .append_message(session_id, new_user_msg)
            .await;

        let tool_definitions = self.tool_registry.get_definitions();
        let max_tool_iterations = 10;
        let mut iteration = 0;
        let mut final_text = String::new();

        while iteration < max_tool_iterations {
            if token.is_cancelled() {
                let _ = event_tx.send(TurnEvent::Cancelled).await;
                return Ok("Turn cancelled.".to_string());
            }

            iteration += 1;

            match self
                .llm_client
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
                            let asst_msg = ChatMessage::assistant(text.clone());
                            let _ = self
                                .session_store
                                .append_message(session_id, asst_msg.clone())
                                .await;

                            // Stream chunks to listener
                            let _ = event_tx.send(TurnEvent::Token(text.clone())).await;
                            let _ = event_tx.send(TurnEvent::Finished(text.clone())).await;

                            // Trigger background review without blocking response delivery
                            let mut reflection_messages = messages.clone();
                            reflection_messages.push(asst_msg);
                            self.reflection_engine.maybe_spawn_review(
                                session_id,
                                &reflection_messages,
                                &active_model,
                            );

                            return Ok(text);
                        }
                        ChatResponse::ToolCalls(calls) => {
                            let asst_call_msg = ChatMessage {
                                role: "assistant".to_string(),
                                content: raw_msg.content,
                                tool_calls: raw_msg.tool_calls,
                                tool_call_id: None,
                            };
                            let _ = self
                                .session_store
                                .append_message(session_id, asst_call_msg.clone())
                                .await;
                            messages.push(asst_call_msg);

                            for call in calls {
                                if token.is_cancelled() {
                                    let _ = event_tx.send(TurnEvent::Cancelled).await;
                                    return Ok("Turn cancelled.".to_string());
                                }

                                let _ = event_tx
                                    .send(TurnEvent::ToolExecuting {
                                        name: call.name.clone(),
                                        id: call.id.clone(),
                                    })
                                    .await;

                                let result = self.tool_registry.execute(&call).await;

                                let _ = event_tx
                                    .send(TurnEvent::ToolCompleted {
                                        name: call.name.clone(),
                                        id: call.id.clone(),
                                        success: result.success,
                                    })
                                    .await;

                                let tool_msg =
                                    ChatMessage::tool(result.tool_call_id, result.output);
                                let _ = self
                                    .session_store
                                    .append_message(session_id, tool_msg.clone())
                                    .await;
                                messages.push(tool_msg);
                            }
                        }
                    }
                }
                Err(e) => {
                    error!("LLM execution error: {}", e);
                    let err_msg = format!("LLM error: {}. Use /model to switch models.", e);
                    let _ = event_tx.send(TurnEvent::Error(err_msg.clone())).await;
                    let asst_msg = ChatMessage::assistant(err_msg.clone());
                    let _ = self
                        .session_store
                        .append_message(session_id, asst_msg)
                        .await;
                    // Even a failed turn must not swallow a process notice.
                    self.drain_background_notices(session_id).await;
                    return Ok(err_msg);
                }
            }
        }

        if final_text.is_empty() {
            final_text = "Halted: Maximum tool iterations reached.".to_string();
            let _ = event_tx.send(TurnEvent::Finished(final_text.clone())).await;
            let asst_msg = ChatMessage::assistant(final_text.clone());
            let _ = self
                .session_store
                .append_message(session_id, asst_msg.clone())
                .await;

            let mut reflection_messages = messages.clone();
            reflection_messages.push(asst_msg);
            self.reflection_engine.maybe_spawn_review(
                session_id,
                &reflection_messages,
                &active_model,
            );
        }

        // Announce anything the background processes did while this turn ran.
        self.drain_background_notices(session_id).await;

        Ok(final_text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::{Clock, NoInterrupt, NoScope, NoTransform, PosixPtyHost, SystemClock};
    use crate::tools::ToolRegistry;
    use hermes_session_store::MemorySessionStore;
    use serde_json::json;
    use std::sync::Arc;

    /// A registry that can actually spawn, so the drain is exercised against a real
    /// host rather than a mock that proves nothing about liveness.
    fn real_registry() -> Arc<crate::process::ProcessRegistry> {
        Arc::new(crate::process::ProcessRegistry::with_parts(
            Arc::new(crate::process::LocalProcessHost::new()),
            Arc::new(PosixPtyHost::new()),
            Arc::new(NoScope),
            Arc::new(NoInterrupt),
            Arc::new(SystemClock),
            Arc::new(NoTransform),
        ))
    }

    #[tokio::test]
    async fn a_finished_background_process_reaches_the_transcript() {
        // The whole point of layer 8: a process that exits during a turn must leave a
        // notice in the transcript. Before this, the event was queued and never read.
        let registry = real_registry();
        let session_id = uuid::Uuid::new_v4();
        let owner = session_id.to_string();

        let handle = registry
            .spawn_session("exit 3", None, Vec::new(), Some(owner.clone()))
            .expect("spawn");
        handle.session.lock().notify_on_complete = true;
        let sid = handle.id();

        // Wait for the reaper, then run the drain exactly as the turn loop does.
        let mut drained = Vec::new();
        for _ in 0..80 {
            drained = registry.take_events_for_owner(&owner);
            if !drained.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(
            !drained.is_empty(),
            "the finished process never produced an event for its owner"
        );

        let store = Arc::new(MemorySessionStore::new());
        let notifications: Vec<String> = drained
            .iter()
            .filter_map(crate::process_notify::format_process_event)
            .collect();
        assert_eq!(notifications.len(), 1);
        let text = notifications.join("\n\n");
        assert!(
            text.contains(&sid),
            "notice did not name the process: {text}"
        );
        assert!(text.contains("exit code 3"), "exit code lost: {text}");

        let store_dyn: Arc<Box<dyn SessionStore>> = Arc::new(Box::new(MemorySessionStore::new()));
        store_dyn
            .append_message(session_id, ChatMessage::user(text.clone()))
            .await
            .unwrap();
        let msgs = store_dyn.get_messages(session_id, Some(10)).await.unwrap();
        assert!(
            msgs.iter().any(|m| m
                .content
                .as_deref()
                .unwrap_or_default()
                .contains("exit code 3")),
            "the notice never landed in the transcript"
        );
        drop(store);
    }

    #[tokio::test]
    async fn another_sessions_event_is_left_for_its_owner() {
        // Draining must not steal: an event owned by a different session stays queued.
        let registry = real_registry();
        registry
            .spawn_session("sleep 0.1", None, Vec::new(), Some("owner-a".to_string()))
            .expect("spawn");
        registry
            .spawn_session("sleep 0.1", None, Vec::new(), Some("owner-b".to_string()))
            .expect("spawn");

        // Hand-place one event per owner so the assertion is about routing, not timing.
        {
            let m = registry.take_events_for_owner("nobody");
            assert!(m.is_empty());
        }
        let reg2 = real_registry();
        let h = reg2
            .spawn_session("true", None, Vec::new(), Some("owner-c".to_string()))
            .expect("spawn");
        let sess = h.snapshot();
        let evt = json!({
            "type": "completion",
            "session_id": sess.id,
            "command": sess.command,
            "owner_task_id": "owner-c",
            "exit_code": 0,
        });
        reg2.watch.events.push(evt);

        assert!(
            reg2.take_events_for_owner("someone-else").is_empty(),
            "a foreign event was claimed"
        );
        assert_eq!(
            reg2.take_events_for_owner("owner-c").len(),
            1,
            "the owner could not claim its own event"
        );
        // Gone now: it was consumed, not left to rot.
        assert!(reg2.take_events_for_owner("owner-c").is_empty());
    }
}
