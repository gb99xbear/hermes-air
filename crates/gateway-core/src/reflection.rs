// hermes-gateway-core - Background Review & Reflection Engine
use crate::llm::{ChatMessage, ChatResponse, LlmClient};
use hermes_skills::{Skill, SkillRegistry};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};
use uuid::Uuid;

/// Configuration options for the Background Review & Reflection Engine
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackgroundReviewConfig {
    /// Whether background reflection/review is enabled
    pub enabled: bool,
    /// Periodic cadence: minimum turns between reviews (0 = every turn with signal)
    pub interval_turns: usize,
    /// Minimum messages required in history snapshot before evaluating review
    pub min_messages: usize,
    /// Maximum conversation history messages digested into reflection prompt
    pub max_history_messages: usize,
}

impl Default for BackgroundReviewConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            interval_turns: 1,
            min_messages: 2,
            max_history_messages: 30,
        }
    }
}

/// Learning signals detected from conversation turn history
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LearningSignal {
    /// Explicit user corrections, complaints, formatting directions, or preferences
    UserCorrection(String),
    /// Successful debugging workarounds, non-trivial execution techniques, and tool patterns
    TechnicalSolution(String),
}

/// Prompt instructing the model to evaluate the conversation and create/update lessons or skills
pub const SKILL_REVIEW_PROMPT: &str = "Review the conversation above and update the skill library. Be ACTIVE — most sessions produce at least one skill update, even if small. A pass that does nothing is a missed learning opportunity, not a neutral outcome.\n\nTarget shape of the library: CLASS-LEVEL skills, each with a SKILL.md of always-on rules and a small `references/` set of topical depth. Not a flat list of narrow one-session skills, and not an umbrella hoarding a references/ file per session. This shapes HOW you update, not WHETHER you update.\n\nWhat a skill IS: the instructions for doing a class of task the most efficient and correct way, to THIS user's specifications — the procedure, the tools and commands that work, the order, the user's preferences for how the result should look, and the pitfalls that cost time. A future session should be able to follow it and produce what the user wants on the first try. Everything below is about writing that well:\n  • Procedure first: the steps in the order they are done, with the concrete commands, tool calls, and decision points. Lessons and pitfalls attach to the step they affect.\n  • A pitfall is a generalizable rule + one clause of WHY (the mechanism), imperative.\n  • No PR/issue numbers, dates, ticket IDs, or quoted user text as content — the rule must stand without the incident behind it.\n  • The same lesson learned twice is ONE rule. Strengthen existing skills before adding new files.\n  • Not a duplicate of what the environment already teaches: repo instructions, tool descriptions, and other always-loaded context.\n  • Always-on rules live in SKILL.md itself. references/ is for depth only needed sometimes.\n\nSignals to look for (any one of these warrants action):\n  • User corrected your style, tone, format, legibility, or verbosity. Frustration signals like 'stop doing X', 'this is too verbose', 'don't format like this', 'why are you explaining', 'just give me the answer', or an explicit 'remember this' are FIRST-CLASS skill signals.\n  • User corrected your workflow, approach, or sequence of steps. Encode the correction as a pitfall or explicit step in the skill that governs that class of task.\n  • Non-trivial technique, fix, workaround, debugging path, or tool-usage pattern emerged that a future session would benefit from. Capture it.\n  • A skill that got loaded or consulted this session turned out to be wrong, missing a step, or outdated. Patch it NOW.\n\nIf no skills or updates need saving, respond with: 'Nothing to save.'";

/// Prompt instructing the model to identify persistent facts or preferences for memory
pub const MEMORY_REVIEW_PROMPT: &str = "Review the conversation above and consider saving to memory if appropriate.\n\nMemory has TWO distinct stores — pick the right one for each fact:\n  • USER.md: who the user is — persona, preferences, communication and work style, personal details they revealed, and expectations about how you should behave.\n  • MEMORY.md: facts about the ENVIRONMENT you operate in — tool quirks, project conventions, config gotchas, paths and endpoints that matter.\n\nOne fact goes to ONE store, never both. If something stands out, extract it clearly. If nothing is worth saving, respond with: 'Nothing to save.'";

/// Combined review prompt covering both skills and memory
pub const COMBINED_REVIEW_PROMPT: &str = "Review the conversation above and update two things:\n\n**Memory**:\n  • USER.md: who the user is — persona, preferences, communication style, and expectations.\n  • MEMORY.md: facts about the environment — tool quirks, project conventions, paths and endpoints.\n\n**Skills**: how to do this class of task. Be ACTIVE — most sessions produce at least one skill update.\n  • User corrected style, tone, format, or approach.\n  • Non-trivial technique, fix, workaround, or debugging path emerged.\n  • Loaded skill was wrong, missing, or outdated.\n\nIf nothing stands out on either dimension, respond with: 'Nothing to save.'";

/// Engine for evaluating completed turns in the background and extracting skills/memories
pub struct BackgroundReviewEngine {
    config: BackgroundReviewConfig,
    llm_client: Arc<LlmClient>,
    skill_registry: Arc<SkillRegistry>,
    turn_counter: AtomicUsize,
    review_lock: Arc<Mutex<()>>,
}

impl BackgroundReviewEngine {
    pub fn new(
        config: BackgroundReviewConfig,
        llm_client: Arc<LlmClient>,
        skill_registry: Arc<SkillRegistry>,
    ) -> Self {
        Self {
            config,
            llm_client,
            skill_registry,
            turn_counter: AtomicUsize::new(0),
            review_lock: Arc::new(Mutex::new(())),
        }
    }

    pub fn config(&self) -> &BackgroundReviewConfig {
        &self.config
    }

    pub fn skill_registry(&self) -> Arc<SkillRegistry> {
        self.skill_registry.clone()
    }

    /// Detect learning signals from message history heuristics
    pub fn detect_learning_signals(messages: &[ChatMessage]) -> Vec<LearningSignal> {
        let mut signals = Vec::new();

        let correction_patterns = [
            "don't ",
            "do not ",
            "stop doing",
            "stop ",
            "never ",
            "remember this",
            "too verbose",
            "format like",
            "no need to",
            "instead of",
            "you should ",
            "you must ",
            "wrong",
            "incorrect",
            "not what i asked",
            "fix this",
            "don't explain",
            "just give me",
        ];

        let tech_solution_patterns = [
            "fixed with",
            "workaround:",
            "the solution is",
            "resolved by",
            "root cause",
            "debugged",
            "solved by",
            "patch applied",
            "compiled successfully",
            "tests pass",
        ];

        for msg in messages {
            let content_lower = msg.content.as_deref().unwrap_or("").to_lowercase();
            if content_lower.is_empty() {
                continue;
            }

            if msg.role == "user" {
                for pattern in &correction_patterns {
                    if content_lower.contains(pattern) {
                        signals.push(LearningSignal::UserCorrection(msg.content.clone().unwrap_or_default()));
                        break;
                    }
                }
            } else if msg.role == "assistant" || msg.role == "tool" {
                for pattern in &tech_solution_patterns {
                    if content_lower.contains(pattern) {
                        signals.push(LearningSignal::TechnicalSolution(msg.content.clone().unwrap_or_default()));
                        break;
                    }
                }
            }
        }

        signals
    }

    /// Check if background review should be triggered based on config and detected signals
    pub fn should_review(&self, messages: &[ChatMessage]) -> bool {
        if !self.config.enabled {
            return false;
        }

        if messages.len() < self.config.min_messages {
            return false;
        }

        let turns = self.turn_counter.fetch_add(1, Ordering::SeqCst);
        if self.config.interval_turns > 0 && (turns % self.config.interval_turns != 0) {
            return false;
        }

        // Check if there are learning signals in the conversation
        let signals = Self::detect_learning_signals(messages);
        !signals.is_empty()
    }

    /// Assemble the prompt for background review given conversation snapshot
    pub fn build_review_messages(
        messages_snapshot: &[ChatMessage],
        custom_prompt: Option<&str>,
        max_history: usize,
    ) -> Vec<ChatMessage> {
        let prompt_text = custom_prompt.unwrap_or(COMBINED_REVIEW_PROMPT);
        let start_idx = if messages_snapshot.len() > max_history {
            messages_snapshot.len() - max_history
        } else {
            0
        };

        let mut review_messages = Vec::with_capacity(messages_snapshot.len() - start_idx + 2);
        review_messages.push(ChatMessage::system(prompt_text.to_string()));

        // Add conversation digest
        for msg in &messages_snapshot[start_idx..] {
            if msg.role != "system" {
                review_messages.push(msg.clone());
            }
        }

        review_messages.push(ChatMessage::user(
            "Evaluate the conversation above and extract or update any relevant skills or persistent memories now."
                .to_string(),
        ));

        review_messages
    }

    /// Spawn non-blocking background review task via tokio::spawn
    pub fn maybe_spawn_review(
        self: &Arc<Self>,
        session_id: Uuid,
        messages_snapshot: &[ChatMessage],
        model: &str,
    ) {
        if !self.should_review(messages_snapshot) {
            debug!("Skipping background review for session {}: no trigger signals or criteria not met", session_id);
            return;
        }

        let engine = self.clone();
        let snapshot = messages_snapshot.to_vec();
        let model = model.to_string();

        tokio::spawn(async move {
            engine.execute_review_run(session_id, snapshot, &model).await;
        });
    }

    /// Execute the review loop in background
    pub async fn execute_review_run(
        &self,
        session_id: Uuid,
        messages_snapshot: Vec<ChatMessage>,
        model: &str,
    ) -> Option<String> {
        let _guard = match self.review_lock.try_lock() {
            Ok(g) => g,
            Err(_) => {
                debug!("Background review already running, skipping overlapping run for session {}", session_id);
                return None;
            }
        };

        info!("Starting background review and reflection for session {}", session_id);

        let review_messages = Self::build_review_messages(
            &messages_snapshot,
            None,
            self.config.max_history_messages,
        );

        match self
            .llm_client
            .chat_completion_with_tools(
                model,
                &review_messages,
                None,
                None,
                0.3,
                2048,
            )
            .await
        {
            Ok((_raw_msg, response)) => {
                let text = match response {
                    ChatResponse::Text(t) => t,
                    ChatResponse::ToolCalls(_) => String::new(),
                };

                let trimmed = text.trim();
                if trimmed.is_empty() || trimmed.contains("Nothing to save.") {
                    info!("Background review completed for session {}: Nothing to save.", session_id);
                } else {
                    info!("Background review extracted reflection for session {}: {}", session_id, trimmed);
                    Self::persist_extracted_learnings(&self.skill_registry, trimmed).await;
                }

                Some(text)
            }
            Err(e) => {
                warn!("Background review LLM request failed for session {}: {:?}", session_id, e);
                None
            }
        }
    }

    /// Parse markdown skill format if returned in reflection output and register with SkillRegistry
    pub async fn persist_extracted_learnings(skill_registry: &Arc<SkillRegistry>, output: &str) {
        if output.contains("---") {
            if let Ok((frontmatter, body)) = Skill::parse_frontmatter(output) {
                let mut name = String::new();
                let mut description = String::new();

                for line in frontmatter.lines() {
                    let trimmed = line.trim();
                    if let Some(n) = trimmed.strip_prefix("name:") {
                        name = n.trim().trim_matches('"').trim_matches('\'').to_string();
                    } else if let Some(d) = trimmed.strip_prefix("description:") {
                        description = d.trim().trim_matches('"').trim_matches('\'').to_string();
                    }
                }

                if !name.is_empty() {
                    let skill = Skill {
                        name: name.clone(),
                        version: "1.0.0".to_string(),
                        description,
                        category: "reflection".to_string(),
                        tags: vec!["autonomous-learning".to_string()],
                        author: "reflection-engine".to_string(),
                        license: "MIT".to_string(),
                        platforms: vec![],
                        dependencies: vec![],
                        triggers: vec![],
                        when_to_use: String::new(),
                        content: body,
                        references: vec![],
                        templates: std::collections::HashMap::new(),
                        scripts: std::collections::HashMap::new(),
                        metadata: None,
                        skill_dir: None,
                        skill_file: None,
                    };
                    info!("Background reflection registering skill: '{}'", name);
                    skill_registry.register(skill).await;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_background_review_config_defaults() {
        let config = BackgroundReviewConfig::default();
        assert!(config.enabled);
        assert_eq!(config.interval_turns, 1);
        assert_eq!(config.min_messages, 2);
        assert_eq!(config.max_history_messages, 30);
    }

    #[test]
    fn test_signal_detection_user_correction() {
        let messages = vec![
            ChatMessage::user("Can you write a poem?".to_string()),
            ChatMessage::assistant("Sure! Here is a long poem...".to_string()),
            ChatMessage::user("Stop doing that, this is too verbose! Just give me the bullet points.".to_string()),
        ];

        let signals = BackgroundReviewEngine::detect_learning_signals(&messages);
        assert_eq!(signals.len(), 1);
        match &signals[0] {
            LearningSignal::UserCorrection(text) => {
                assert!(text.contains("too verbose"));
            }
            _ => panic!("Expected UserCorrection signal"),
        }
    }

    #[test]
    fn test_signal_detection_technical_solution() {
        let messages = vec![
            ChatMessage::user("Why did cargo test fail?".to_string()),
            ChatMessage::tool("call_1".to_string(), "error: unresolved import Foo".to_string()),
            ChatMessage::assistant("The root cause was missing dependency. It was resolved by adding bar to Cargo.toml and tests pass.".to_string()),
        ];

        let signals = BackgroundReviewEngine::detect_learning_signals(&messages);
        assert_eq!(signals.len(), 1);
        match &signals[0] {
            LearningSignal::TechnicalSolution(text) => {
                assert!(text.contains("resolved by"));
            }
            _ => panic!("Expected TechnicalSolution signal"),
        }
    }

    #[test]
    fn test_build_review_messages_structure() {
        let messages = vec![
            ChatMessage::system("Initial system prompt".to_string()),
            ChatMessage::user("Hello".to_string()),
            ChatMessage::assistant("World".to_string()),
        ];

        let review_msgs = BackgroundReviewEngine::build_review_messages(&messages, None, 10);
        assert_eq!(review_msgs.len(), 4);
        assert_eq!(review_msgs[0].role, "system");
        assert!(review_msgs[0].content.as_ref().unwrap().contains("**Memory**"));
        assert_eq!(review_msgs[1].role, "user");
        assert_eq!(review_msgs[1].content.as_deref(), Some("Hello"));
        assert_eq!(review_msgs[2].role, "assistant");
        assert_eq!(review_msgs[2].content.as_deref(), Some("World"));
        assert_eq!(review_msgs[3].role, "user");
        assert!(review_msgs[3].content.as_ref().unwrap().contains("Evaluate the conversation"));
    }

    #[test]
    fn test_should_review_filtering() {
        let provider_profile = crate::providers::ProviderProfile::default();
        let llm = Arc::new(LlmClient::from_provider_profile(&provider_profile, None, None));
        let skill_reg = Arc::new(SkillRegistry::default());

        let mut config = BackgroundReviewConfig::default();
        config.min_messages = 3;

        let engine = BackgroundReviewEngine::new(config.clone(), llm.clone(), skill_reg.clone());

        // Less than min_messages
        let messages = vec![
            ChatMessage::user("Don't do that".to_string()),
        ];
        assert!(!engine.should_review(&messages));

        // Meets min_messages and has signal
        let messages_with_signal = vec![
            ChatMessage::user("Start".to_string()),
            ChatMessage::assistant("Running".to_string()),
            ChatMessage::user("Stop doing that, format like json!".to_string()),
        ];
        assert!(engine.should_review(&messages_with_signal));

        // No signal
        let messages_no_signal = vec![
            ChatMessage::user("What is 2 + 2?".to_string()),
            ChatMessage::assistant("It is 4.".to_string()),
            ChatMessage::user("Thanks!".to_string()),
        ];
        assert!(!engine.should_review(&messages_no_signal));
    }

    #[tokio::test]
    async fn test_persist_extracted_learnings() {
        let skill_reg = Arc::new(SkillRegistry::default());
        let output = r#"---
name: "fast-grep-trick"
description: "How to grep efficiently in large codebases"
---
# Fast Grep Trick
Always use ripgrep or search_files tool.
"#;
        BackgroundReviewEngine::persist_extracted_learnings(&skill_reg, output).await;
        let registered = skill_reg.get("fast-grep-trick").await;
        assert!(registered.is_some());
        assert_eq!(registered.unwrap().description, "How to grep efficiently in large codebases");
    }
}
