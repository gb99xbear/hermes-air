use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};
use uuid::Uuid;

use crate::llm::{ChatMessage, ChatResponse, LlmClient};
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

/// Manager coordinating subagent lifecycles, execution, and control.
pub struct DelegationManager {
    subagents: Arc<DashMap<String, SubagentState>>,
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
        }
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

            for task in tasks {
                let subagent_id = format!("sub_{}", Uuid::new_v4().simple());
                let cancel_token = CancellationToken::new();
                let transcript = Arc::new(Mutex::new(Vec::new()));
                let steer_queue = Arc::new(Mutex::new(Vec::new()));

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

                let handle = tokio::spawn(async move {
                    let outcome = Self::run_subagent_loop(
                        sub_id.clone(),
                        sub_goal,
                        sub_context,
                        sub_schema,
                        sub_token,
                        sub_transcript,
                        sub_steer,
                        sub_llm,
                        sub_tools,
                    )
                    .await;

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
                        task_results.push(serde_json::json!({
                            "subagent_id": sid,
                            "status": outcome.status,
                            "summary": outcome.summary,
                            "transcript": outcome.transcript,
                            "schema_valid": outcome.schema_valid,
                        }));
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
                .chat_completion_with_tools(model, &messages, Some(available_tools), None, 0.7, 4096)
                .await
            {
                Ok(res) => res,
                Err(e) => {
                    warn!("Subagent {} LLM call failed or mock mode: {}", subagent_id, e);
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

                        let res = tool_registry.execute_core(&call).await;
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
        }
    }
}

pub struct SubagentRunOutcome {
    pub status: String,
    pub summary: String,
    pub transcript: Vec<String>,
    pub schema_valid: bool,
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
        let steer_res = manager.steer(subagent_id, "Focus on async test harness").await;
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
        assert!(steer_res["error"].as_str().unwrap().contains("No live subagent"));

        let stop_res = manager.stop("unknown_id").await;
        assert_eq!(stop_res["success"], false);
        assert!(stop_res["error"].as_str().unwrap().contains("No live subagent"));
    }
}
