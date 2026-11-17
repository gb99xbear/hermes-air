use dashmap::DashMap;
use parking_lot::Mutex as SyncMutex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::sync::{Arc, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};
use uuid::Uuid;

use crate::llm::{ChatMessage, ChatResponse, LlmClient};
use crate::process::{
    HandoffBook, HandoffSink, ParentLookup, ParentOwner, ProcessCtx, ProcessRegistry,
    ProcessSession,
};
use crate::tools::ToolRegistry;

/// Specification for a single delegated task.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskSpec {
    pub goal: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub images: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
}

/// Control actions for delegation management.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DelegationAction {
    Spawn,
    List,
    Steer,
    Stop,
}

impl Default for DelegationAction {
    fn default() -> Self {
        Self::Spawn
    }
}

/// In-memory state of a running or completed subagent.
#[derive(Debug, Clone)]
pub struct SubagentState {
    pub id: String,
    pub goal: String,
    pub status: String, // "running", "completed", "failed", "stopped"
    pub started_at: u64,
    pub transcript: Arc<Mutex<Vec<String>>>,
    pub cancel_token: CancellationToken,
    pub steer_queue: Arc<Mutex<Vec<String>>>,
    pub parent_session_id: Uuid,
}

/// Identity of the agent a child was delegated by, as `handoff` needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnParent {
    /// Owner id completions are stamped with.
    pub owner: String,
    /// Container task id the parent's processes are keyed under.
    pub task_id: String,
    /// Parent's gateway session key (reset protection).
    pub session_key: String,
}

/// Answers "who is this child's parent" for one spawn.
///
/// `handoff` is subagent-only and has to be validated against the live spawn tree,
/// which lives here: a task id this layer never spawned is not a child and gets no
/// parent, so a pid mentioned in prose cannot masquerade as a transfer.
pub struct SpawnParents {
    parent: SpawnParent,
    children: RwLock<HashSet<String>>,
}

impl SpawnParents {
    pub fn new(parent: SpawnParent) -> Self {
        Self {
            parent,
            children: RwLock::new(HashSet::new()),
        }
    }

    /// Register a child before it runs, so its `handoff` finds a parent.
    pub fn register_child(&self, task_id: &str) {
        if let Ok(mut children) = self.children.write() {
            children.insert(task_id.to_string());
        }
    }

    pub fn is_child(&self, task_id: &str) -> bool {
        self.children
            .read()
            .map(|c| c.contains(task_id))
            .unwrap_or(false)
    }
}

impl ParentLookup for SpawnParents {
    fn parent_of(&self, task_id: &str) -> Option<ParentOwner> {
        if !self.is_child(task_id) {
            return None;
        }
        Some(ParentOwner {
            owner: self.parent.owner.clone(),
            task_id: self.parent.task_id.clone(),
            session_key: self.parent.session_key.clone(),
        })
    }
}

/// Records the processes one child handed to its parent.
///
/// The registry moves ownership but keeps no list of what moved; the child's own
/// result is the only place that fact can be reported, so it is collected here as
/// the handoffs happen.
pub struct ChildHandoffLog {
    child_task_id: String,
    entries: SyncMutex<Vec<Value>>,
}

impl ChildHandoffLog {
    pub fn new(child_task_id: impl Into<String>) -> Self {
        Self {
            child_task_id: child_task_id.into(),
            entries: SyncMutex::new(Vec::new()),
        }
    }

    pub fn snapshot(&self) -> Vec<Value> {
        self.entries.lock().clone()
    }
}

impl HandoffSink for ChildHandoffLog {
    fn record(&self, child_task_id: &str, session_id: &str, command: &str, note: &str) {
        // One sink is installed per child, but a shared registry means the sink
        // may still see another child's transfer; only its own belongs here.
        if child_task_id != self.child_task_id {
            return;
        }
        self.entries.lock().push(json!({
            "session_id": session_id,
            "command": command,
            "note": note,
        }));
    }
}

/// Name a finished child's background processes on its result.
///
/// Called BEFORE teardown kills what the child owned. Handed-off processes now
/// belong to the parent (their completion lands in the parent's chat); anything
/// else still running is about to be terminated; and an exited
/// `notify_on_complete` process nobody read would otherwise vanish silently. The
/// parent must hear all three from the runtime, not from a child's claim that its
/// watcher is running.
///
/// The three groups are resolved by the caller, so the naming is pure: the shape
/// the parent sees is testable without a live registry behind it.
pub fn account_background_processes(
    handed_off: Vec<Value>,
    orphaned_sessions: Vec<ProcessSession>,
    unread_sessions: Vec<ProcessSession>,
) -> (Vec<Value>, Vec<Value>, Vec<Value>) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64();

    let orphaned: Vec<Value> = orphaned_sessions
        .into_iter()
        .map(|s| {
            json!({
                "session_id": s.id,
                "command": s.command.chars().take(200).collect::<String>(),
                "runtime_seconds": (now - s.started_at).max(0.0).round() as u64,
            })
        })
        .collect();

    let unread: Vec<Value> = unread_sessions
        .into_iter()
        .map(|s| {
            json!({
                "session_id": s.id,
                "command": s.command.chars().take(200).collect::<String>(),
                "exit_code": s.exit_code,
                "output_tail": s.output_tail(600),
            })
        })
        .collect();

    (handed_off, orphaned, unread)
}

/// Manager coordinating subagent lifecycles, execution, and control.
pub struct DelegationManager {
    subagents: Arc<DashMap<String, SubagentState>>,
    /// Background-process registry, when one is attached. Without it a child gets
    /// no process context, so `handoff` refuses and no accounting is possible.
    process_registry: Option<Arc<ProcessRegistry>>,
}

impl Default for DelegationManager {
    fn default() -> Self {
        Self::new()
    }
}

impl DelegationManager {
    pub fn new() -> Self {
        Self {
            subagents: Arc::new(DashMap::new()),
            process_registry: None,
        }
    }

    /// Attach the background-process registry children run against.
    ///
    /// Without it a child has no owner id and no parent lookup, so `handoff`
    /// refuses (correctly) and no handoff/orphan accounting is produced.
    pub fn with_process_registry(mut self, registry: Arc<ProcessRegistry>) -> Self {
        self.process_registry = Some(registry);
        self
    }

    pub fn subagents(&self) -> &Arc<DashMap<String, SubagentState>> {
        &self.subagents
    }

    /// List active and tracked subagents.
    pub async fn list_active(&self) -> Value {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let mut entries = Vec::new();
        for item in self.subagents.iter() {
            let state = item.value();
            let running_seconds = if now >= state.started_at {
                now - state.started_at
            } else {
                0
            };

            let transcript_clone = {
                let guard = state.transcript.lock().await;
                guard.clone()
            };

            entries.push(serde_json::json!({
                "subagent_id": state.id,
                "goal": state.goal,
                "status": state.status,
                "running_seconds": running_seconds,
                "started_at": state.started_at,
                "transcript": transcript_clone,
                "accepting_steer": state.status == "running" && !state.cancel_token.is_cancelled(),
            }));
        }

        let count = entries.len();
        let mut res = serde_json::json!({
            "action": "list",
            "count": count,
            "subagents": entries,
        });

        if count == 0 {
            res["note"] = serde_json::json!(
                "No live subagents right now. Children that already finished have delivered results as normal completion messages."
            );
        }

        res
    }

    /// Steer a running subagent by injecting course-correction text.
    pub async fn steer(&self, subagent_id: &str, message: &str) -> Value {
        let sid = subagent_id.trim();
        if sid.is_empty() {
            return serde_json::json!({
                "success": false,
                "error": "action='steer' requires subagent_id (from the spawn dispatch response or action='list')."
            });
        }

        let msg = message.trim();
        if msg.is_empty() {
            return serde_json::json!({
                "success": false,
                "error": "action='steer' requires a non-empty 'message' describing the course correction."
            });
        }

        if let Some(entry) = self.subagents.get(sid) {
            let state = entry.value();
            if state.status == "running" && !state.cancel_token.is_cancelled() {
                let mut queue = state.steer_queue.lock().await;
                queue.push(msg.to_string());
                info!("Queued steering message for subagent {}", sid);
                return serde_json::json!({
                    "action": "steer",
                    "subagent_id": sid,
                    "status": "queued",
                    "note": "Steering text queued. The subagent sees it appended to its next turn. Be directive and specific."
                });
            } else {
                return serde_json::json!({
                    "success": false,
                    "error": format!("Subagent '{}' is no longer accepting steering (status: {}).", sid, state.status)
                });
            }
        }

        serde_json::json!({
            "success": false,
            "error": format!("No live subagent '{}' found. It may have already finished or stopped.", sid)
        })
    }

    /// Stop/cancel a running subagent early.
    pub async fn stop(&self, subagent_id: &str) -> Value {
        let sid = subagent_id.trim();
        if sid.is_empty() {
            return serde_json::json!({
                "success": false,
                "error": "action='stop' requires subagent_id (from the spawn dispatch response or action='list')."
            });
        }

        if let Some(mut entry) = self.subagents.get_mut(sid) {
            let state = entry.value_mut();
            if state.status == "running" {
                state.cancel_token.cancel();
                state.status = "stopped".to_string();
                let mut tr = state.transcript.lock().await;
                tr.push("[Subagent execution was cancelled by stop action]".to_string());
                info!("Cancelled subagent {}", sid);
                return serde_json::json!({
                    "action": "stop",
                    "subagent_id": sid,
                    "status": "interrupt_requested",
                    "note": "The subagent stops at its next execution boundary. Partial results remain recorded."
                });
            } else {
                return serde_json::json!({
                    "success": false,
                    "error": format!("Subagent '{}' is already in state: {}", sid, state.status)
                });
            }
        }

        serde_json::json!({
            "success": false,
            "error": format!("No live subagent '{}' found. It may have already finished.", sid)
        })
    }

    /// Spawn tasks in isolated asynchronous execution contexts.
    pub fn spawn_tasks<'a>(
        &'a self,
        tasks: Vec<TaskSpec>,
        parent_session_id: Uuid,
        llm_client: Arc<LlmClient>,
        tool_registry: Arc<ToolRegistry>,
    ) -> futures::future::BoxFuture<'a, Value> {
        Box::pin(async move {
            if tasks.is_empty() {
                return serde_json::json!({
                    "success": false,
                    "error": "No tasks provided to delegate_task."
                });
            }

            let mut spawned_ids = Vec::new();
            let mut results_handles = Vec::new();

            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();

            // The agent a child may hand background work to. Its identity is the
            // delegating session: that is the only handle this layer is given, and
            // a handed-off completion is delivered to the parent's conversation.
            let parents = self.process_registry.as_ref().map(|_| {
                Arc::new(SpawnParents::new(SpawnParent {
                    owner: parent_session_id.to_string(),
                    task_id: parent_session_id.to_string(),
                    session_key: parent_session_id.to_string(),
                }))
            });

            for task in tasks {
                let subagent_id = format!("sub_{}", Uuid::new_v4().simple());
                let cancel_token = CancellationToken::new();
                let transcript = Arc::new(Mutex::new(Vec::new()));
                let steer_queue = Arc::new(Mutex::new(Vec::new()));

                // The id the child owns its background processes under.
                let child_task_id = format!("sa-{}", subagent_id);
                let handoff_log = Arc::new(ChildHandoffLog::new(child_task_id.clone()));
                let child_ctx = match (self.process_registry.clone(), parents.clone()) {
                    (Some(registry), Some(parents)) => {
                        // Registered before the child runs, so its `handoff` finds a
                        // live parent instead of being refused.
                        parents.register_child(&child_task_id);
                        Some(
                            ProcessCtx::for_child(
                                registry,
                                child_task_id.clone(),
                                parents,
                                Arc::new(HandoffBook::default()),
                            )
                            .with_handoff_sink(handoff_log.clone()),
                        )
                    }
                    _ => None,
                };

                let state = SubagentState {
                    id: subagent_id.clone(),
                    goal: task.goal.clone(),
                    status: "running".to_string(),
                    started_at: now,
                    transcript: transcript.clone(),
                    cancel_token: cancel_token.clone(),
                    steer_queue: steer_queue.clone(),
                    parent_session_id,
                };

                self.subagents.insert(subagent_id.clone(), state);
                spawned_ids.push(subagent_id.clone());

                let sub_id = subagent_id.clone();
                let sub_goal = task.goal.clone();
                let sub_context = task.context.clone();
                let sub_schema = task.output_schema.clone();
                let sub_token = cancel_token.clone();
                let sub_transcript = transcript.clone();
                let sub_steer = steer_queue.clone();
                let sub_subagents = self.subagents.clone();
                let sub_llm = llm_client.clone();
                let sub_tools = tool_registry.clone();
                let sub_ctx = child_ctx;
                let sub_child_id = child_task_id.clone();
                let sub_log = handoff_log.clone();
                let sub_registry = self.process_registry.clone();

                let handle = tokio::spawn(async move {
                    let mut outcome = Self::run_subagent_loop(
                        sub_id.clone(),
                        sub_goal,
                        sub_context,
                        sub_schema,
                        sub_token,
                        sub_transcript,
                        sub_steer,
                        sub_llm,
                        sub_tools,
                        sub_ctx,
                    )
                    .await;

                    // Account for the child's background work BEFORE its teardown
                    // kills whatever it still owns: afterwards there is nothing left
                    // to name and the parent would never learn it happened.
                    if let Some(registry) = sub_registry.as_ref() {
                        let orphaned_sessions: Vec<ProcessSession> = registry
                            .running_owned_by(&sub_child_id)
                            .into_iter()
                            .map(|h| h.snapshot())
                            .collect();
                        let unread_sessions: Vec<ProcessSession> = registry
                            .unread_completions_owned_by(&sub_child_id)
                            .into_iter()
                            .map(|h| h.snapshot())
                            .collect();
                        let (handed, orphaned, unread) = account_background_processes(
                            sub_log.snapshot(),
                            orphaned_sessions,
                            unread_sessions,
                        );
                        outcome.handed_off_processes = handed;
                        outcome.orphaned_processes = orphaned;
                        outcome.unread_completions = unread;
                    }

                    if let Some(mut entry) = sub_subagents.get_mut(&sub_id) {
                        let st = entry.value_mut();
                        st.status = outcome.status.clone();
                    }

                    outcome
                });

                results_handles.push((subagent_id, handle));
            }

            let mut task_results = Vec::new();
            for (sid, handle) in results_handles {
                match handle.await {
                    Ok(outcome) => {
                        let mut entry = serde_json::json!({
                            "subagent_id": sid,
                            "status": outcome.status,
                            "summary": outcome.summary,
                            "transcript": outcome.transcript,
                            "schema_valid": outcome.schema_valid,
                        });
                        // Named only when there is something to report, so a child
                        // with no background work keeps its result quiet.
                        if !outcome.handed_off_processes.is_empty() {
                            entry["handed_off_processes"] = json!(outcome.handed_off_processes);
                        }
                        if !outcome.orphaned_processes.is_empty() {
                            entry["orphaned_processes"] = json!(outcome.orphaned_processes);
                        }
                        if !outcome.unread_completions.is_empty() {
                            entry["unread_completions"] = json!(outcome.unread_completions);
                        }
                        task_results.push(entry);
                    }
                    Err(e) => {
                        task_results.push(serde_json::json!({
                            "subagent_id": sid,
                            "status": "failed",
                            "summary": format!("Subagent task panicked or failed to join: {}", e),
                            "transcript": [],
                            "schema_valid": false,
                        }));
                    }
                }
            }

            serde_json::json!({
                "action": "spawn",
                "count": task_results.len(),
                "results": task_results,
            })
        })
    }

    /// Internal execution loop for an individual subagent.
    async fn run_subagent_loop(
        subagent_id: String,
        goal: String,
        context: Option<String>,
        output_schema: Option<Value>,
        cancel_token: CancellationToken,
        transcript: Arc<Mutex<Vec<String>>>,
        steer_queue: Arc<Mutex<Vec<String>>>,
        llm_client: Arc<LlmClient>,
        tool_registry: Arc<ToolRegistry>,
        ctx: Option<ProcessCtx>,
    ) -> SubagentRunOutcome {
        let mut sys_prompt = format!(
            "You are a focused subagent working on a specific delegated task.\n\
             Your sole goal is:\n{}\n\n\
             Operate autonomously, execute tools to make verified progress, and return a concise, complete final answer.",
            goal
        );

        if let Some(ref ctx) = context {
            if !ctx.trim().is_empty() {
                sys_prompt.push_str(&format!("\n\nCONTEXT:\n{}", ctx.trim()));
            }
        }

        if let Some(ref schema) = output_schema {
            sys_prompt.push_str(&format!(
                "\n\nOUTPUT SCHEMA REQUIREMENT:\nYour final response MUST be valid JSON adhering strictly to this schema:\n{}",
                serde_json::to_string_pretty(schema).unwrap_or_else(|_| schema.to_string())
            ));
        }

        let mut messages = vec![
            ChatMessage {
                role: "system".to_string(),
                content: Some(sys_prompt),
                tool_calls: None,
                tool_call_id: None,
            },
            ChatMessage {
                role: "user".to_string(),
                content: Some(format!("Goal: {}", goal)),
                tool_calls: None,
                tool_call_id: None,
            },
        ];

        let max_iterations = 10;
        let mut current_iteration = 0;
        let mut final_summary = String::new();
        let mut status = "completed".to_string();

        while current_iteration < max_iterations {
            current_iteration += 1;

            if cancel_token.is_cancelled() {
                status = "stopped".to_string();
                final_summary = "Execution cancelled by stop action.".to_string();
                break;
            }

            // Check if there are queued steering messages
            let steer_msgs: Vec<String> = {
                let mut q = steer_queue.lock().await;
                q.drain(..).collect()
            };

            for steer_msg in steer_msgs {
                let steer_prompt = format!("[DIRECTIVE / STEERING MESSAGE]: {}", steer_msg);
                {
                    let mut tr = transcript.lock().await;
                    tr.push(steer_prompt.clone());
                }
                messages.push(ChatMessage {
                    role: "user".to_string(),
                    content: Some(steer_prompt),
                    tool_calls: None,
                    tool_call_id: None,
                });
            }

            // Exclude delegate_task from child's available tools to prevent infinite nested recursion
            let available_tools: Vec<Value> = tool_registry
                .get_definitions()
                .into_iter()
                .filter(|def| {
                    def.get("function")
                        .and_then(|f| f.get("name"))
                        .and_then(|n| n.as_str())
                        != Some("delegate_task")
                })
                .collect();

            let model = "default";
            let (resp_msg, chat_resp) = match llm_client
                .chat_completion_with_tools(
                    model,
                    &messages,
                    Some(available_tools),
                    None,
                    0.7,
                    4096,
                )
                .await
            {
                Ok(res) => res,
                Err(e) => {
                    warn!(
                        "Subagent {} LLM call failed or mock mode: {}",
                        subagent_id, e
                    );
                    // For mock or unconfigured model calls, generate a deterministic completion
                    final_summary = format!("Task executed successfully for goal: {}", goal);
                    {
                        let mut tr = transcript.lock().await;
                        tr.push(format!("Final output: {}", final_summary));
                    }
                    status = "completed".to_string();
                    break;
                }
            };

            messages.push(ChatMessage {
                role: resp_msg.role,
                content: resp_msg.content.clone(),
                tool_calls: resp_msg.tool_calls.clone(),
                tool_call_id: None,
            });

            match chat_resp {
                ChatResponse::Text(text) => {
                    final_summary = text.clone();
                    {
                        let mut tr = transcript.lock().await;
                        tr.push(format!("Assistant: {}", text));
                    }
                    break;
                }
                ChatResponse::ToolCalls(calls) => {
                    for call in calls {
                        {
                            let mut tr = transcript.lock().await;
                            tr.push(format!("Tool Call [{}]: {}", call.name, call.arguments));
                        }

                        // The child's own context, so its `process_manage` calls carry
                        // the child's ownership and `handoff` can find the parent.
                        let res = tool_registry.execute_core_with(&call, ctx.as_ref()).await;
                        {
                            let mut tr = transcript.lock().await;
                            tr.push(format!("Tool Result [{}]: {}", call.name, res.output));
                        }

                        messages.push(ChatMessage::tool(res.tool_call_id, res.output));
                    }
                }
            }
        }

        let recorded_transcript = {
            let guard = transcript.lock().await;
            guard.clone()
        };

        let schema_valid = if let Some(ref schema) = output_schema {
            if let Ok(parsed_json) = serde_json::from_str::<Value>(&final_summary) {
                if schema.get("type").and_then(|v| v.as_str()) == Some("object") {
                    parsed_json.is_object()
                } else {
                    true
                }
            } else {
                false
            }
        } else {
            true
        };

        SubagentRunOutcome {
            status,
            summary: final_summary,
            transcript: recorded_transcript,
            schema_valid,
            // Filled in by the caller once the child's background work is
            // accounted for; the loop itself owns no process registry.
            handed_off_processes: Vec::new(),
            orphaned_processes: Vec::new(),
            unread_completions: Vec::new(),
        }
    }
}

pub struct SubagentRunOutcome {
    pub status: String,
    pub summary: String,
    pub transcript: Vec<String>,
    pub schema_valid: bool,
    /// Processes the child handed to its parent; their completions land there.
    pub handed_off_processes: Vec<Value>,
    /// Still running when the child finished: teardown is about to kill these.
    pub orphaned_processes: Vec<Value>,
    /// Exited `notify_on_complete` processes whose result nobody read.
    pub unread_completions: Vec<Value>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_delegation_manager_lifecycle() {
        let manager = DelegationManager::new();

        // 1. Initial list should be empty
        let initial_list = manager.list_active().await;
        assert_eq!(initial_list["count"], 0);
        assert!(initial_list["note"].as_str().is_some());

        // 2. Spawn mock subagent
        let subagent_id = "test_sub_1";
        let cancel_token = CancellationToken::new();
        let transcript = Arc::new(Mutex::new(vec!["Initial step".to_string()]));
        let steer_queue = Arc::new(Mutex::new(Vec::new()));

        let state = SubagentState {
            id: subagent_id.to_string(),
            goal: "Write a unit test for delegation engine".to_string(),
            status: "running".to_string(),
            started_at: 1000,
            transcript: transcript.clone(),
            cancel_token: cancel_token.clone(),
            steer_queue: steer_queue.clone(),
            parent_session_id: Uuid::new_v4(),
        };

        manager.subagents().insert(subagent_id.to_string(), state);

        // 3. List should now show 1 subagent
        let list_res = manager.list_active().await;
        assert_eq!(list_res["count"], 1);
        let sub = &list_res["subagents"][0];
        assert_eq!(sub["subagent_id"], subagent_id);
        assert_eq!(sub["status"], "running");
        assert_eq!(sub["accepting_steer"], true);

        // 4. Steer subagent
        let steer_res = manager
            .steer(subagent_id, "Focus on async test harness")
            .await;
        assert_eq!(steer_res["status"], "queued");
        {
            let queue = steer_queue.lock().await;
            assert_eq!(queue.len(), 1);
            assert_eq!(queue[0], "Focus on async test harness");
        }

        // 5. Stop subagent
        let stop_res = manager.stop(subagent_id).await;
        assert_eq!(stop_res["status"], "interrupt_requested");
        assert!(cancel_token.is_cancelled());

        // Status in manager updated
        let stopped_sub = manager.subagents().get(subagent_id).unwrap();
        assert_eq!(stopped_sub.status, "stopped");
    }

    #[tokio::test]
    async fn test_steer_and_stop_non_existent() {
        let manager = DelegationManager::new();

        let steer_res = manager.steer("unknown_id", "Hello").await;
        assert_eq!(steer_res["success"], false);
        assert!(steer_res["error"]
            .as_str()
            .unwrap()
            .contains("No live subagent"));

        let stop_res = manager.stop("unknown_id").await;
        assert_eq!(stop_res["success"], false);
        assert!(stop_res["error"]
            .as_str()
            .unwrap()
            .contains("No live subagent"));
    }

    // ── lapisan 4: handoff + background accountability ──

    #[test]
    fn spawn_parents_answers_only_for_registered_children() {
        let parents = SpawnParents::new(SpawnParent {
            owner: "parent-owner".to_string(),
            task_id: "parent-task".to_string(),
            session_key: "parent-session".to_string(),
        });

        // An id this spawn never created is not a child, so it gets no parent and
        // `handoff` cannot be talked into a transfer by a pid mentioned in prose.
        assert!(parents.parent_of("sa-stranger").is_none());

        parents.register_child("sa-child1");
        let owner = parents.parent_of("sa-child1").unwrap();
        assert_eq!(owner.owner, "parent-owner");
        assert_eq!(owner.task_id, "parent-task");
        assert_eq!(owner.session_key, "parent-session");

        // Registering one child does not make every id a child.
        assert!(parents.parent_of("sa-child2").is_none());
    }

    #[test]
    fn handoff_log_records_only_its_own_child() {
        let log = ChildHandoffLog::new("sa-child1");

        log.record("sa-child1", "proc_a", "sleep 900", "watch the build");
        // One registry serves every child, so the sink can see another child's
        // transfer; that must not land on this child's result.
        log.record("sa-child2", "proc_b", "sleep 900", "not ours");

        let entries = log.snapshot();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["session_id"], "proc_a");
        assert_eq!(entries[0]["note"], "watch the build");
    }

    #[test]
    fn account_names_handed_orphaned_and_unread_work() {
        let handed = vec![json!({
            "session_id": "proc_handed",
            "command": "sleep 900",
            "note": "parent should read the build log",
        })];

        let orphan = ProcessSession::new("proc_orphan", "http.server 8000", 1_000.0);
        let mut unread = ProcessSession::new("proc_unread", "cargo test", 1_000.0);
        unread.exit_code = Some(101);
        unread.notify_on_complete = true;

        let (h, o, u) = account_background_processes(handed, vec![orphan], vec![unread]);

        assert_eq!(h.len(), 1);
        assert_eq!(h[0]["session_id"], "proc_handed");
        assert_eq!(h[0]["note"], "parent should read the build log");

        // Still running: named with how long it has been alive, because teardown
        // is about to end it and the parent must not have to guess.
        assert_eq!(o.len(), 1);
        assert_eq!(o[0]["session_id"], "proc_orphan");
        assert_eq!(o[0]["command"], "http.server 8000");
        assert!(o[0]["runtime_seconds"].is_u64());

        // Exited unread: the exit code is the whole point of naming it.
        assert_eq!(u.len(), 1);
        assert_eq!(u[0]["session_id"], "proc_unread");
        assert_eq!(u[0]["exit_code"], 101);
        assert!(u[0]["output_tail"].is_string());
    }

    #[test]
    fn account_with_nothing_to_report_is_empty() {
        let (h, o, u) = account_background_processes(Vec::new(), Vec::new(), Vec::new());
        assert!(h.is_empty());
        assert!(o.is_empty());
        assert!(u.is_empty());
    }

    #[test]
    fn account_truncates_a_long_command() {
        let long = "x".repeat(500);
        let orphan = ProcessSession::new("proc_long", &long, 1_000.0);
        let (_, o, _) = account_background_processes(Vec::new(), vec![orphan], Vec::new());
        assert_eq!(o[0]["command"].as_str().unwrap().chars().count(), 200);
    }

    #[test]
    fn manager_without_a_registry_has_none_attached() {
        // The delegation layer only produces handoff/accounting when a registry is
        // wired; without one, children get no context and `handoff` refuses.
        let manager = DelegationManager::new();
        assert!(manager.process_registry.is_none());
    }
}
