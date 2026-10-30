// Session Search Tool - 1-to-1 matching Python hermes-agent/tools/session_search_tool.py
// Provides long-term conversation recall across 4 distinct operational shapes:
// 1. Discovery Shape: query provided - keyword match across sessions with relevance scoring
// 2. Browse Shape: no query, no session_id - lists recent sessions with message counts & previews
// 3. Read Shape: session_id provided alone - bounded message history with 2000 char per-message cap
// 4. Scroll Shape: session_id + around_message_id - window of messages centered on an anchor

use hermes_protocol::{ChatMessage, Uuid};
use hermes_session_store::SessionStore;
use serde_json::Value;
use std::sync::Arc;
use tracing::info;

pub const READ_MAX_CONTENT: usize = 2000;
pub const DEFAULT_WINDOW: usize = 5;
pub const DEFAULT_LIMIT: usize = 3;

#[derive(Clone)]
pub struct SessionSearchEngine {
    session_store: Arc<Box<dyn SessionStore>>,
}

impl SessionSearchEngine {
    pub fn new(session_store: Arc<Box<dyn SessionStore>>) -> Self {
        Self { session_store }
    }

    pub fn get_definition() -> Value {
        serde_json::json!({
            "type": "function",
            "function": {
                "name": "session_search",
                "description": "Recall past conversations: search or read old Hermes sessions, or scroll inside one. Four shapes, picked by args: query = discovery (top-N matching sessions, top result fully hydrated); session_id + around_message_id = scroll (window of messages around an anchor); session_id alone = read a whole session (supports head/tail pagination); no args = browse recent sessions. Results are actual stored messages, no LLM hallucinations. Searches conversation history ONLY. When referring the user to a session, write its link value verbatim inline.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "query": {
                            "type": "string",
                            "description": "Search query (discovery shape). Keywords or phrases to find in past sessions. Omit to browse recent sessions."
                        },
                        "limit": {
                            "type": "integer",
                            "description": "Discovery or browse shape. Max sessions to return (default 3, max 10).",
                            "default": 3
                        },
                        "sort": {
                            "type": "string",
                            "enum": ["newest", "oldest"],
                            "description": "Discovery shape only. Sort order: newest (default) or oldest."
                        },
                        "detail": {
                            "type": "string",
                            "enum": ["adaptive", "full"],
                            "description": "Discovery shape only. adaptive (default) fully hydrates top result; full returns full window for every result.",
                            "default": "adaptive"
                        },
                        "session_id": {
                            "type": "string",
                            "description": "Read or scroll shape. Target session ID to read or scroll within."
                        },
                        "around_message_id": {
                            "type": "string",
                            "description": "Scroll shape. Message ID or index to center the message window around."
                        },
                        "window": {
                            "type": "integer",
                            "description": "Scroll shape only. Messages to return on each side of the anchor (clamped 1-20, default 5).",
                            "default": 5
                        },
                        "head": {
                            "type": "integer",
                            "description": "Read shape only. Number of head messages to retrieve (default 20)."
                        },
                        "tail": {
                            "type": "integer",
                            "description": "Read shape only. Number of tail messages to retrieve (default 10)."
                        }
                    },
                    "required": []
                }
            }
        })
    }

    pub async fn execute(&self, args: &Value) -> Result<String, String> {
        let session_id_opt = args.get("session_id")
            .and_then(|v| v.as_str())
            .map(|s| s.trim())
            .filter(|s| !s.is_empty());

        let around_message_id_opt = args.get("around_message_id")
            .and_then(|v| {
                if let Some(s) = v.as_str() {
                    Some(s.trim().to_string())
                } else if let Some(n) = v.as_i64() {
                    Some(n.to_string())
                } else {
                    None
                }
            })
            .filter(|s| !s.is_empty());

        // 1. SCROLL SHAPE: session_id + around_message_id
        if let (Some(sid_str), Some(around_id)) = (session_id_opt, around_message_id_opt) {
            let session_id = Uuid::parse_str(sid_str)
                .map_err(|e| format!("Invalid session_id UUID: {}", e))?;
            let window = args.get("window")
                .and_then(|v| v.as_u64())
                .unwrap_or(DEFAULT_WINDOW as u64) as usize;
            let clamped_window = std::cmp::max(1, std::cmp::min(window, 20));

            return self.execute_scroll(session_id, &around_id, clamped_window).await;
        }

        // 2. READ SHAPE: session_id alone
        if let Some(sid_str) = session_id_opt {
            let session_id = Uuid::parse_str(sid_str)
                .map_err(|e| format!("Invalid session_id UUID: {}", e))?;
            let head = args.get("head").and_then(|v| v.as_u64()).map(|n| n as usize).unwrap_or(20);
            let tail = args.get("tail").and_then(|v| v.as_u64()).map(|n| n as usize).unwrap_or(10);

            return self.execute_read(session_id, head, tail).await;
        }

        let query_opt = args.get("query")
            .and_then(|v| v.as_str())
            .map(|s| s.trim())
            .filter(|s| !s.is_empty());

        let limit = args.get("limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(DEFAULT_LIMIT as u64) as usize;
        let clamped_limit = std::cmp::max(1, std::cmp::min(limit, 10));

        let sort_newest = args.get("sort")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_lowercase() != "oldest")
            .unwrap_or(true);

        // 3. DISCOVERY SHAPE: query provided
        if let Some(query) = query_opt {
            let detail = args.get("detail")
                .and_then(|v| v.as_str())
                .unwrap_or("adaptive");
            return self.execute_discovery(query, clamped_limit, sort_newest, detail).await;
        }

        // 4. BROWSE SHAPE: no query, no session_id
        self.execute_browse(clamped_limit).await
    }

    async fn execute_browse(&self, limit: usize) -> Result<String, String> {
        info!("Executing session_search browse shape with limit: {}", limit);
        let summaries = self.session_store.search_sessions(None, limit, true)
            .await
            .map_err(|e| format!("Failed to browse sessions: {}", e))?;

        let results: Vec<Value> = summaries.into_iter().map(|s| {
            serde_json::json!({
                "session_id": s.session_id.to_string(),
                "link": format!("@session:{}", s.session_id),
                "title": s.title,
                "started_at": s.started_at,
                "last_active": s.last_active,
                "message_count": s.message_count,
                "preview": s.preview,
                "source": s.source,
                "model": s.model
            })
        }).collect();

        let count = results.len();
        Ok(serde_json::json!({
            "success": true,
            "mode": "browse",
            "results": results,
            "count": count,
            "message": format!("Showing {} most recent sessions. Pass query to search or session_id+around_message_id to scroll.", count)
        }).to_string())
    }

    async fn execute_discovery(&self, query: &str, limit: usize, sort_newest: bool, detail: &str) -> Result<String, String> {
        info!("Executing session_search discovery shape: query=\x27{}\x27, limit={}, sort_newest={}", query, limit, sort_newest);
        let summaries = self.session_store.search_sessions(Some(query), limit, sort_newest)
            .await
            .map_err(|e| format!("Failed to search sessions: {}", e))?;

        let is_full = detail == "full";
        let mut results = Vec::new();

        for (idx, summary) in summaries.into_iter().enumerate() {
            let sid = summary.session_id;
            let full_messages = self.session_store.get_messages(sid, None)
                .await
                .unwrap_or_default();

            // Find matching message or default to anchor
            let match_idx = full_messages.iter().position(|m| {
                m.content.as_deref().map(|c| c.to_lowercase().contains(&query.to_lowercase())).unwrap_or(false)
            }).unwrap_or(0);

            let hydrate_full = is_full || idx == 0;
            let (window_messages, bookend_start, bookend_end) = if hydrate_full {
                let start = match_idx.saturating_sub(3);
                let end = std::cmp::min(match_idx + 4, full_messages.len());
                let window_part = full_messages[start..end].iter().map(|m| Self::shape_message(m, READ_MAX_CONTENT)).collect::<Vec<_>>();
                let head_part = full_messages.iter().take(2).map(|m| Self::shape_message(m, 1200)).collect::<Vec<_>>();
                let tail_part = full_messages.iter().rev().take(2).rev().map(|m| Self::shape_message(m, 1200)).collect::<Vec<_>>();
                (window_part, head_part, tail_part)
            } else {
                let single = if let Some(m) = full_messages.get(match_idx) {
                    vec![Self::shape_message(m, 1000)]
                } else {
                    Vec::new()
                };
                (single, Vec::new(), Vec::new())
            };

            let entry = serde_json::json!({
                "session_id": sid.to_string(),
                "link": format!("@session:{}", sid),
                "title": summary.title,
                "when": summary.started_at,
                "source": summary.source,
                "model": summary.model,
                "matched_role": full_messages.get(match_idx).map(|m| m.role.as_str()).unwrap_or("user"),
                "match_message_id": match_idx.to_string(),
                "snippet": summary.preview,
                "messages": window_messages,
                "bookend_start": bookend_start,
                "bookend_end": bookend_end,
                "detail": if hydrate_full { "full" } else { "compact" }
            });
            results.push(entry);
        }

        let count = results.len();
        Ok(serde_json::json!({
            "success": true,
            "mode": "discover",
            "query": query,
            "detail": detail,
            "results": results,
            "count": count,
            "sessions_searched": count,
            "link_hint": "When referring the user to a session, write its  value verbatim inline."
        }).to_string())
    }

    async fn execute_read(&self, session_id: Uuid, head: usize, tail: usize) -> Result<String, String> {
        info!("Executing session_search read shape: session_id={}, head={}, tail={}", session_id, head, tail);
        let all_messages = self.session_store.get_messages(session_id, None)
            .await
            .map_err(|e| format!("Failed to read session {}: {}", session_id, e))?;

        let total = all_messages.len();
        let session = self.session_store.get(session_id).await.unwrap_or(None);

        let shaped: Vec<Value> = all_messages.iter().map(|m| Self::shape_message(m, READ_MAX_CONTENT)).collect();
        let truncated = total > (head + tail);

        let messages_slice = if truncated {
            let mut combined = Vec::new();
            combined.extend(shaped.iter().take(head).cloned());
            combined.extend(shaped.iter().rev().take(tail).rev().cloned());
            combined
        } else {
            shaped
        };

        let session_meta = serde_json::json!({
            "when": session.as_ref().map(|s| s.created_at.to_string()).unwrap_or_else(|| "unknown".to_string()),
            "source": session.as_ref().map(|s| s.platform.as_str().to_string()).unwrap_or_else(|| "unknown".to_string()),
            "title": session.as_ref().and_then(|s| s.metadata.get("title").and_then(|v| v.as_str()).map(|t| t.to_string())),
        });

        Ok(serde_json::json!({
            "success": true,
            "mode": "read",
            "session_id": session_id.to_string(),
            "link": format!("@session:{}", session_id),
            "session_meta": session_meta,
            "message_count": total,
            "truncated": truncated,
            "messages": messages_slice
        }).to_string())
    }

    async fn execute_scroll(&self, session_id: Uuid, around_message_id: &str, window: usize) -> Result<String, String> {
        info!("Executing session_search scroll shape: session_id={}, around={}, window={}", session_id, around_message_id, window);
        let full_messages = self.session_store.get_messages(session_id, None)
            .await
            .map_err(|e| format!("Failed to load session for scrolling: {}", e))?;

        if full_messages.is_empty() {
            return Err(format!("Session {} has no messages to scroll", session_id));
        }

        let session = self.session_store.get(session_id).await.unwrap_or(None);
        let center_idx = match around_message_id.parse::<usize>() {
            Ok(i) if i < full_messages.len() => i,
            _ => {
                full_messages.iter().position(|m| {
                    m.tool_call_id.as_deref() == Some(around_message_id) ||
                    m.content.as_deref().map(|c| c.contains(around_message_id)).unwrap_or(false)
                }).unwrap_or(0)
            }
        };

        let start = center_idx.saturating_sub(window);
        let end = std::cmp::min(center_idx + window + 1, full_messages.len());

        let shaped: Vec<Value> = full_messages[start..end].iter().enumerate().map(|(rel_idx, m)| {
            let mut val = Self::shape_message(m, READ_MAX_CONTENT);
            if start + rel_idx == center_idx {
                if let Some(obj) = val.as_object_mut() {
                    obj.insert("anchor".to_string(), Value::Bool(true));
                }
            }
            val
        }).collect();

        let messages_before = center_idx;
        let messages_after = full_messages.len().saturating_sub(center_idx + 1);

        let session_meta = serde_json::json!({
            "when": session.as_ref().map(|s| s.created_at.to_string()).unwrap_or_else(|| "unknown".to_string()),
            "source": session.as_ref().map(|s| s.platform.as_str().to_string()).unwrap_or_else(|| "unknown".to_string()),
            "title": session.as_ref().and_then(|s| s.metadata.get("title").and_then(|v| v.as_str()).map(|t| t.to_string())),
        });

        Ok(serde_json::json!({
            "success": true,
            "mode": "scroll",
            "session_id": session_id.to_string(),
            "around_message_id": around_message_id,
            "session_meta": session_meta,
            "window": window,
            "messages": shaped,
            "messages_before": messages_before,
            "messages_after": messages_after,
            "hint": "Scroll forward: re-call with around_message_id = the LAST message index; backward: FIRST message index."
        }).to_string())
    }

    fn shape_message(msg: &ChatMessage, max_content: usize) -> Value {
        let (content, truncated, orig_len) = match &msg.content {
            Some(c) => {
                let stripped = Self::strip_ansi(c);
                if stripped.len() > max_content {
                    (Some(format!("{}…", &stripped[..max_content])), true, Some(stripped.len()))
                } else {
                    (Some(stripped), false, None)
                }
            }
            None => (None, false, None),
        };

        let mut map = serde_json::Map::new();
        map.insert("role".to_string(), Value::String(msg.role.clone()));
        if let Some(c) = content {
            map.insert("content".to_string(), Value::String(c));
        }
        if truncated {
            map.insert("content_truncated".to_string(), Value::Bool(true));
            if let Some(orig) = orig_len {
                map.insert("original_content_chars".to_string(), serde_json::json!(orig));
            }
        }
        if let Some(tc) = &msg.tool_calls {
            map.insert("tool_calls".to_string(), serde_json::json!(tc));
        }
        if let Some(tid) = &msg.tool_call_id {
            map.insert("tool_call_id".to_string(), Value::String(tid.clone()));
        }

        Value::Object(map)
    }

        fn strip_ansi(input: &str) -> String {
        let mut out = String::with_capacity(input.len());
        let mut chars = input.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                if chars.peek() == Some(&'[') {
                    chars.next();
                    for b in chars.by_ref() {
                        if (0x40..=0x7E).contains(&(b as u32)) {
                            break;
                        }
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hermes_protocol::{Platform, Session, SessionState};
    use hermes_session_store::MemorySessionStore;

    async fn setup_test_engine() -> (SessionSearchEngine, Uuid, Uuid) {
        let store = Arc::new(Box::new(MemorySessionStore::new()) as Box<dyn SessionStore>);
        let sid1 = Uuid::new_v4();
        let sid2 = Uuid::new_v4();

        let mut sess1 = Session {
            id: sid1,
            profile: "default".to_string(),
            platform: Platform::Telegram,
            platform_session_id: "tg_101".to_string(),
            state: SessionState::Active,
            created_at: time::OffsetDateTime::now_utc(),
            updated_at: time::OffsetDateTime::now_utc(),
            metadata: std::collections::HashMap::new(),
        };
        sess1.metadata.insert("title".to_string(), serde_json::json!("Compiler Optimization Discussion"));
        store.set(&sess1).await.unwrap();

        let mut sess2 = Session {
            id: sid2,
            profile: "default".to_string(),
            platform: Platform::Discord,
            platform_session_id: "dc_202".to_string(),
            state: SessionState::Active,
            created_at: time::OffsetDateTime::now_utc(),
            updated_at: time::OffsetDateTime::now_utc(),
            metadata: std::collections::HashMap::new(),
        };
        sess2.metadata.insert("title".to_string(), serde_json::json!("Database Sharding Deep-Dive"));
        store.set(&sess2).await.unwrap();

        store.append_message(sid1, ChatMessage::user("How does LLVM optimize loop unrolling?")).await.unwrap();
        store.append_message(sid1, ChatMessage::assistant("LLVM utilizes loop vectorization and vector lanes.")).await.unwrap();
        store.append_message(sid1, ChatMessage::user("Show me benchmark metrics.")).await.unwrap();

        store.append_message(sid2, ChatMessage::user("Consistent hashing in distributed systems.")).await.unwrap();
        store.append_message(sid2, ChatMessage::assistant("Consistent hashing minimizes remapped keys upon node resize.")).await.unwrap();

        let engine = SessionSearchEngine::new(store);
        (engine, sid1, sid2)
    }

    #[tokio::test]
    async fn test_session_search_browse_shape() {
        let (engine, _s1, _s2) = setup_test_engine().await;
        let res_str = engine.execute(&serde_json::json!({})).await.unwrap();
        let parsed: Value = serde_json::from_str(&res_str).unwrap();

        assert_eq!(parsed["mode"], "browse");
        assert_eq!(parsed["success"], true);
        assert_eq!(parsed["count"], 2);
        let results = parsed["results"].as_array().unwrap();
        assert_eq!(results.len(), 2);
    }

    #[tokio::test]
    async fn test_session_search_discovery_shape() {
        let (engine, s1, _s2) = setup_test_engine().await;
        let res_str = engine.execute(&serde_json::json!({
            "query": "LLVM loop",
            "limit": 5,
            "detail": "adaptive"
        })).await.unwrap();
        let parsed: Value = serde_json::from_str(&res_str).unwrap();

        assert_eq!(parsed["mode"], "discover");
        assert_eq!(parsed["success"], true);
        assert_eq!(parsed["count"], 1);
        let results = parsed["results"].as_array().unwrap();
        assert_eq!(results[0]["session_id"], s1.to_string());
        assert_eq!(results[0]["detail"], "full");
        assert!(results[0]["messages"].as_array().unwrap().len() >= 1);
    }

    #[tokio::test]
    async fn test_session_search_read_shape() {
        let (engine, s1, _s2) = setup_test_engine().await;
        let res_str = engine.execute(&serde_json::json!({
            "session_id": s1.to_string(),
            "head": 10,
            "tail": 5
        })).await.unwrap();
        let parsed: Value = serde_json::from_str(&res_str).unwrap();

        assert_eq!(parsed["mode"], "read");
        assert_eq!(parsed["success"], true);
        assert_eq!(parsed["session_id"], s1.to_string());
        assert_eq!(parsed["message_count"], 3);
        assert_eq!(parsed["truncated"], false);
        assert_eq!(parsed["messages"].as_array().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn test_session_search_scroll_shape() {
        let (engine, s1, _s2) = setup_test_engine().await;
        let res_str = engine.execute(&serde_json::json!({
            "session_id": s1.to_string(),
            "around_message_id": "1",
            "window": 1
        })).await.unwrap();
        let parsed: Value = serde_json::from_str(&res_str).unwrap();

        assert_eq!(parsed["mode"], "scroll");
        assert_eq!(parsed["success"], true);
        assert_eq!(parsed["around_message_id"], "1");
        let messages = parsed["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[1]["anchor"], true);
    }
}

