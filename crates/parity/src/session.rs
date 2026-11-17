//! Session-level aggregated metrics

use std::collections::HashMap;
use std::fs;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::metrics::FinalizedTurnMetrics;
use crate::ParityState;

/// Aggregated session metrics
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SessionMetrics {
    pub session_id: String,
    pub start_time: String,
    pub platform: String,
    pub total_turns: u64,
    pub total_api_calls: u64,
    pub total_tool_calls: u64,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub total_prompt_tokens: u64,
    pub total_latency_ms: u64,
    pub compressions: u64,
    pub errors: u64,
    // Internal: track tool usage counts
    #[serde(skip)]
    tool_counts: HashMap<String, u64>,
}

impl SessionMetrics {
    pub fn new(session_id: String, start_time: String, platform: String) -> Self {
        Self {
            session_id,
            start_time,
            platform,
            ..Default::default()
        }
    }

    pub fn update_from_turn(turn: &FinalizedTurnMetrics) -> Result<()> {
        if !crate::is_enabled() {
            return Ok(());
        }
        let state = ParityState::get();
        let _ = std::fs::create_dir_all(&state.output_dir);
        let filepath = state
            .output_dir
            .join(format!("session_{}.json", turn.session_id));

        let mut session = if filepath.exists() {
            let content = fs::read_to_string(&filepath)?;
            serde_json::from_str(&content).unwrap_or_else(|_| {
                SessionMetrics::new(
                    turn.session_id.clone(),
                    turn.timestamp.clone(),
                    turn.platform.clone(),
                )
            })
        } else {
            SessionMetrics::new(
                turn.session_id.clone(),
                turn.timestamp.clone(),
                turn.platform.clone(),
            )
        };

        session.total_turns += 1;
        session.total_api_calls += turn.api_calls;
        session.total_tool_calls += turn.tool_calls;
        session.total_input_tokens += turn.input_tokens;
        session.total_output_tokens += turn.output_tokens;
        session.total_prompt_tokens += turn.prompt_tokens;
        session.total_latency_ms += turn.latency_ms;
        if turn.compression_triggered {
            session.compressions += 1;
        }
        if !turn.errors.is_empty() {
            session.errors += turn.errors.len() as u64;
        }

        for tool in &turn.tool_names {
            *session.tool_counts.entry(tool.clone()).or_insert(0) += 1;
        }

        // Best-effort: never fail a turn because a metrics file could not be written.
        if let Ok(bytes) = serde_json::to_vec(&session) {
            let _ = fs::write(filepath, bytes);
        }
        Ok(())
    }

    pub fn load(session_id: &str) -> Option<Self> {
        let state = ParityState::get();
        let filepath = state
            .output_dir
            .join(format!("session_{}.json", session_id));
        if !filepath.exists() {
            return None;
        }
        let content = fs::read_to_string(filepath).ok()?;
        serde_json::from_str(&content).ok()
    }

    pub fn avg_latency_per_call(&self) -> f64 {
        if self.total_api_calls == 0 {
            0.0
        } else {
            self.total_latency_ms as f64 / self.total_api_calls as f64
        }
    }

    pub fn tool_usage(&self) -> Vec<(String, u64)> {
        let mut usage: Vec<_> = self
            .tool_counts
            .iter()
            .map(|(k, v)| (k.clone(), *v))
            .collect();
        usage.sort_by(|a, b| b.1.cmp(&a.1));
        usage
    }
}
