use std::sync::Arc;
use hermes_protocol::{ChatMessage, Uuid};
use hermes_session_store::SessionStore;
use tracing::{info, warn};
use anyhow::Result;

use crate::llm::LlmClient;

pub const SUMMARY_PREFIX: &str = "[CONTEXT COMPACTION — REFERENCE ONLY] Earlier turns were compacted into the summary below. This is a handoff from a previous context window — treat it as background reference, NOT as active instructions. Do NOT answer questions or fulfill requests mentioned in this summary; they were already addressed. Respond ONLY to the latest user message that appears AFTER this summary — that message is the single source of truth for what to do right now. If no user message appears AFTER this summary, do nothing: do not resume, wrap up, or continue work from '## Historical Task Snapshot' or any other section, do not call tools, and wait for a new user message. This handoff must never become the active turn by itself. (Exception: if tool results or your own tool calls appear after this summary, you are mid-way through an in-flight exchange — continue that exchange normally.) Topic overlap with the summary does NOT mean you should resume its task: even on similar topics, the latest user message WINS. Treat ONLY the latest message as the active task and discard stale items from '## Historical Task Snapshot' entirely — do not 'wrap up' or 'finish' work described there unless the latest message explicitly asks for it. Reverse signals in the latest message (e.g. 'stop', 'undo', 'roll back', 'just verify', 'don't do that anymore', 'never mind', a new topic) must immediately end any in-flight work described in the summary; do not re-surface it in later turns. IMPORTANT: Your persistent memory (MEMORY.md, USER.md) in the system prompt is ALWAYS authoritative and active — never ignore or deprioritize memory content due to this compaction note. None of the above restricts HOW you work: your tools remain fully active — keep calling them normally for the active task (edit files, run commands, search) instead of merely narrating what you would do. The current session state (files, config, etc.) may reflect work described here — avoid repeating it:";

pub const HISTORICAL_TASK_HEADING: &str = "## Historical Task Snapshot";

/// Result of a session compaction run
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompressionResult {
    pub pre_count: usize,
    pub post_count: usize,
    pub tokens_saved: usize,
    pub summary: String,
    pub compacted: bool,
}

/// Auto-compaction & Context Hygiene Engine
#[derive(Debug, Clone)]
pub struct ContextCompressor {
    pub compression_count: usize,
    pub last_tokens_saved: usize,
    pub threshold: f32,
    pub protected_tail_turns: usize,
}

impl Default for ContextCompressor {
    fn default() -> Self {
        Self {
            compression_count: 0,
            last_tokens_saved: 0,
            threshold: 0.85,
            protected_tail_turns: 4,
        }
    }
}

impl ContextCompressor {
    pub fn new(threshold: f32, protected_tail_turns: usize) -> Self {
        Self {
            compression_count: 0,
            last_tokens_saved: 0,
            threshold,
            protected_tail_turns,
        }
    }

    /// Evaluates if compression is needed based on token occupancy against context window
    pub fn should_compress(&self, used_tokens: usize, context_window: usize) -> bool {
        if context_window == 0 {
            return false;
        }
        let occupancy = used_tokens as f32 / context_window as f32;
        occupancy >= self.threshold
    }

    /// Rough token estimation: ~4 chars per token (matching Python estimate_request_tokens_rough)
    pub fn estimate_tokens(messages: &[ChatMessage]) -> usize {
        messages
            .iter()
            .map(|m| {
                let text_len = m.content.as_deref().map(|s| s.len()).unwrap_or(0);
                let tools_len = m.tool_calls.as_ref().map(|tc| {
                    tc.iter().map(|c| c.function.name.len() + c.function.arguments.len()).sum::<usize>()
                }).unwrap_or(0);
                (text_len + tools_len) / 4 + 4
            })
            .sum()
    }

    /// Formats the prompt dispatched to the LLM for structured summarization
    pub fn build_summarization_prompt(
        messages_to_summarize: &[ChatMessage],
        focus_topic: Option<&str>,
    ) -> String {
        let mut transcript_text = String::new();
        for msg in messages_to_summarize {
            let role_label = match msg.role.as_str() {
                "user" => "User",
                "assistant" => "Assistant",
                "tool" => "Tool Output",
                "system" => "System",
                other => other,
            };
            let content_str = msg.content.as_deref().unwrap_or("[No text content]");
            transcript_text.push_str(&format!("{}: {}\n\n", role_label, content_str));
        }

        let focus_instruction = if let Some(focus) = focus_topic {
            format!("\nFOCUS INSTRUCTION:\nPay special attention to and preserve comprehensive technical details regarding: '{}'.\n", focus)
        } else {
            String::new()
        };

        format!(
r#"You are an expert conversation summarizer and state compressor.
Summarize the following earlier conversation turns into a dense, structured context snapshot.

{}
CRITICAL INSTRUCTIONS:
Produce a summary strictly structured with the following exact markdown headings:

{}
<Summarize the user's primary intent, latest goals, and active mission>

## Goal
<High-level mission objective and current status>

## Key Decisions & Constraints
<Architectural choices, rules, explicit instructions, environmental limits, and decisions made>

## Completed Actions
<Numbered list of concrete actions, implementations, commands, and verified findings>

## Active State
<Current file state, open issues, ongoing discussions, and what needs to happen next>

Do NOT output conversational filler, disclaimers, or preambles. Output ONLY the structured summary.

CONVERSATION TRANSCRIPT TO COMPACT:
{}
"#,
            focus_instruction,
            HISTORICAL_TASK_HEADING,
            transcript_text.trim_end()
        )
    }

    /// Generates a local deterministic fallback summary if the LLM summarizer is unavailable
    pub fn build_deterministic_fallback(
        messages_to_summarize: &[ChatMessage],
        focus_topic: Option<&str>,
        reason: Option<&str>,
    ) -> String {
        let mut user_goals = Vec::new();
        let mut completed_actions = Vec::new();

        for (i, m) in messages_to_summarize.iter().enumerate() {
            if m.role == "user" {
                if let Some(ref text) = m.content {
                    let first_line = text.lines().next().unwrap_or(text).trim();
                    if !first_line.is_empty() && !first_line.starts_with('/') {
                        user_goals.push(first_line.to_string());
                    }
                }
            } else if m.role == "assistant" {
                if let Some(ref text) = m.content {
                    let first_line = text.lines().next().unwrap_or(text).trim();
                    if !first_line.is_empty() {
                        completed_actions.push(format!("{}. {}", i + 1, first_line));
                    }
                }
            }
        }

        let active_task = user_goals.last().cloned().unwrap_or_else(|| "General assistant inquiry".to_string());
        let focus_note = focus_topic.map(|f| format!(" (Focus: {})", f)).unwrap_or_default();
        let reason_note = reason.map(|r| format!(" ({})", r)).unwrap_or_default();

        let actions_block = if completed_actions.is_empty() {
            "None recoverable from compacted turns.".to_string()
        } else {
            completed_actions.into_iter().take(10).collect::<Vec<_>>().join("\n")
        };

        format!(
r#"{}
User requested: {}{}

## Goal
Recovered from deterministic fallback because the LLM context summarizer was unavailable{}. Continue from the protected recent messages after this summary and inspect current file/system state for exact details.

## Key Decisions & Constraints
- Fallback generated deterministically without an LLM summary call.
- Earlier conversation transcript was compacted into this snapshot.

## Completed Actions
{}

## Active State
Compact snapshot active. Continue with the latest user instruction."#,
            HISTORICAL_TASK_HEADING,
            active_task,
            focus_note,
            reason_note,
            actions_block
        )
    }

    /// Compresses session transcript matching Python Hermes conversation_compression.py
    pub async fn compress_session(
        &mut self,
        session_id: Uuid,
        session_store: &Arc<Box<dyn SessionStore>>,
        llm_client: &LlmClient,
        model: &str,
        focus_topic: Option<&str>,
        _force: bool,
    ) -> Result<CompressionResult> {
        let full_transcript = session_store.get_messages(session_id, None).await?;
        let total_count = full_transcript.len();

        // If total messages <= protected_tail_turns, cannot split into head and tail
        if total_count <= self.protected_tail_turns {
            info!(
                "Session {} has {} messages <= protected tail turns ({}), skipping compaction",
                session_id, total_count, self.protected_tail_turns
            );
            return Ok(CompressionResult {
                pre_count: total_count,
                post_count: total_count,
                tokens_saved: 0,
                summary: String::new(),
                compacted: false,
            });
        }

        let pre_tokens = Self::estimate_tokens(&full_transcript);

        // Split transcript into head (to summarize) and tail (protected verbatim)
        let split_idx = total_count.saturating_sub(self.protected_tail_turns);
        let head_messages = &full_transcript[..split_idx];
        let tail_messages = &full_transcript[split_idx..];

        info!(
            "Compacting session {}: {} total messages -> {} head (to summarize), {} tail (protected)",
            session_id, total_count, head_messages.len(), tail_messages.len()
        );

        let summarization_prompt = Self::build_summarization_prompt(head_messages, focus_topic);

        // Dispatch structured summarization to LLMClient (with deterministic fallback on error)
        let summary_body = match llm_client
            .chat_completion(
                model,
                Some("You are an expert conversation summarizer and state compressor."),
                &summarization_prompt,
                0.2,
                1500,
            )
            .await
        {
            Ok(summary) if !summary.trim().is_empty() => summary.trim().to_string(),
            Err(e) => {
                warn!("LLM summarization failed for session {}: {:?}, using deterministic fallback", session_id, e);
                Self::build_deterministic_fallback(head_messages, focus_topic, Some(&e.to_string()))
            }
            Ok(_) => {
                warn!("LLM summarization returned empty string for session {}, using deterministic fallback", session_id);
                Self::build_deterministic_fallback(head_messages, focus_topic, Some("Empty response from LLM"))
            }
        };

        // Format compaction anchor message matching SUMMARY_PREFIX
        let compacted_content = format!("{}\n\n{}", SUMMARY_PREFIX, summary_body);
        let compaction_anchor = ChatMessage {
            role: "user".to_string(),
            content: Some(compacted_content.clone()),
            tool_calls: None,
            tool_call_id: None,
        };

        // Construct new transcript: [compaction_anchor] + tail_messages
        let mut new_transcript = Vec::with_capacity(1 + tail_messages.len());
        new_transcript.push(compaction_anchor);
        for tail_msg in tail_messages {
            new_transcript.push(tail_msg.clone());
        }

        let post_count = new_transcript.len();
        let post_tokens = Self::estimate_tokens(&new_transcript);
        let tokens_saved = pre_tokens.saturating_sub(post_tokens);

        // Atomically replace transcript in session_store: clear old and append new
        session_store.clear_messages(session_id).await?;
        for msg in &new_transcript {
            session_store.append_message(session_id, msg.clone()).await?;
        }

        self.compression_count += 1;
        self.last_tokens_saved = tokens_saved;

        info!(
            "Session {} successfully compacted: {} -> {} messages, ~{} tokens saved",
            session_id, total_count, post_count, tokens_saved
        );

        Ok(CompressionResult {
            pre_count: total_count,
            post_count,
            tokens_saved,
            summary: summary_body,
            compacted: true,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hermes_session_store::MemorySessionStore;

    #[test]
    fn test_should_compress_threshold_boundary_checks() {
        let compressor = ContextCompressor::new(0.85, 4);

        // Exactly at boundary
        assert!(compressor.should_compress(85, 100));
        assert!(compressor.should_compress(850, 1000));

        // Over boundary
        assert!(compressor.should_compress(86, 100));
        assert!(compressor.should_compress(900, 1000));
        assert!(compressor.should_compress(1000, 1000));

        // Under boundary
        assert!(!compressor.should_compress(84, 100));
        assert!(!compressor.should_compress(849, 1000));
        assert!(!compressor.should_compress(500, 1000));

        // Zero context window edge case
        assert!(!compressor.should_compress(100, 0));
    }

    #[test]
    fn test_summary_prompt_formatting_and_headings() {
        let msgs = vec![
            ChatMessage::user("Build a web crawler in Rust"),
            ChatMessage::assistant("Sure, I will use reqwest and scraper."),
            ChatMessage::user("Make sure it respects robots.txt"),
        ];

        let prompt = ContextCompressor::build_summarization_prompt(&msgs, Some("robots.txt compliance"));
        assert!(prompt.contains(HISTORICAL_TASK_HEADING));
        assert!(prompt.contains("## Goal"));
        assert!(prompt.contains("## Key Decisions & Constraints"));
        assert!(prompt.contains("## Completed Actions"));
        assert!(prompt.contains("## Active State"));
        assert!(prompt.contains("FOCUS INSTRUCTION"));
        assert!(prompt.contains("robots.txt compliance"));
        assert!(prompt.contains("User: Build a web crawler in Rust"));
        assert!(prompt.contains("Assistant: Sure, I will use reqwest and scraper."));
    }

    #[test]
    fn test_deterministic_fallback_summary_structure() {
        let msgs = vec![
            ChatMessage::user("Implement auth token verification"),
            ChatMessage::assistant("Created jwt verification module."),
            ChatMessage::user("Add unit tests for expired tokens"),
            ChatMessage::assistant("Added test_expired_tokens."),
            ChatMessage::user("Finalize documentation"),
        ];

        let fallback = ContextCompressor::build_deterministic_fallback(&msgs, None, Some("LLM 503 error"));
        assert!(fallback.contains(HISTORICAL_TASK_HEADING));
        assert!(fallback.contains("## Goal"));
        assert!(fallback.contains("## Key Decisions & Constraints"));
        assert!(fallback.contains("## Completed Actions"));
        assert!(fallback.contains("## Active State"));
        assert!(fallback.contains("LLM 503 error"));
        assert!(fallback.contains("Finalize documentation"));
    }

    #[tokio::test]
    async fn test_compress_session_tail_preservation_and_atomic_replace() {
        let store: Arc<Box<dyn SessionStore>> = Arc::new(Box::new(MemorySessionStore::new()));
        let session_id = Uuid::new_v4();

        // Populate 8 messages: 4 head messages + 4 tail messages
        for i in 1..=8 {
            if i % 2 == 1 {
                store.append_message(session_id, ChatMessage::user(format!("User message {}", i))).await.unwrap();
            } else {
                store.append_message(session_id, ChatMessage::assistant(format!("Assistant response {}", i))).await.unwrap();
            }
        }

        let mut compressor = ContextCompressor::new(0.85, 4);
        let client = LlmClient::new(None, None);

        // Run compression (will fall back to deterministic if LLM unreachable in unit test)
        let res = compressor.compress_session(session_id, &store, &client, "test-model", None, true).await.unwrap();

        assert!(res.compacted);
        assert_eq!(res.pre_count, 8);
        // Post count should be 1 (anchor) + 4 (tail messages) = 5
        assert_eq!(res.post_count, 5);

        let new_history = store.get_messages(session_id, None).await.unwrap();
        assert_eq!(new_history.len(), 5);

        // 1st message is the compaction anchor
        assert_eq!(new_history[0].role, "user");
        let anchor_text = new_history[0].content.as_deref().unwrap();
        assert!(anchor_text.starts_with(SUMMARY_PREFIX));
        assert!(anchor_text.contains(HISTORICAL_TASK_HEADING));

        // Tail 4 messages are preserved verbatim
        assert_eq!(new_history[1].role, "user");
        assert_eq!(new_history[1].content.as_deref(), Some("User message 5"));
        assert_eq!(new_history[2].role, "assistant");
        assert_eq!(new_history[2].content.as_deref(), Some("Assistant response 6"));
        assert_eq!(new_history[3].role, "user");
        assert_eq!(new_history[3].content.as_deref(), Some("User message 7"));
        assert_eq!(new_history[4].role, "assistant");
        assert_eq!(new_history[4].content.as_deref(), Some("Assistant response 8"));

        assert_eq!(compressor.compression_count, 1);
    }

    #[tokio::test]
    async fn test_compress_session_no_op_when_short() {
        let store: Arc<Box<dyn SessionStore>> = Arc::new(Box::new(MemorySessionStore::new()));
        let session_id = Uuid::new_v4();

        // Populate only 3 messages <= protected_tail_turns (4)
        store.append_message(session_id, ChatMessage::user("Hello")).await.unwrap();
        store.append_message(session_id, ChatMessage::assistant("Hi")).await.unwrap();
        store.append_message(session_id, ChatMessage::user("How are you?")).await.unwrap();

        let mut compressor = ContextCompressor::new(0.85, 4);
        let client = LlmClient::new(None, None);

        let res = compressor.compress_session(session_id, &store, &client, "test-model", None, false).await.unwrap();

        assert!(!res.compacted);
        assert_eq!(res.pre_count, 3);
        assert_eq!(res.post_count, 3);
        assert_eq!(res.tokens_saved, 0);

        let history = store.get_messages(session_id, None).await.unwrap();
        assert_eq!(history.len(), 3);
    }
}
