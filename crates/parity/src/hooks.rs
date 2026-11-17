//! Integration hooks for Hermes Air agent loop
//!
//! Provides convenient wrappers for instrumenting the turn lifecycle.

use std::sync::Mutex;

use crate::metrics::TurnMetrics;
use crate::session::SessionMetrics;
use crate::{record_api_call, record_compression, record_error, record_tool_call, start_turn, end_turn};

lazy_static::lazy_static! {
    static ref CURRENT_TURN: Mutex<Option<TurnMetrics>> = Mutex::new(None);
}

/// RAII guard for a turn - automatically ends the turn on drop
pub struct TurnGuard {
    turn_id: String,
    session_id: String,
    finish_reason: String,
    response_chars: u64,
    ended: bool,
}

impl TurnGuard {
    /// Create a new turn guard and start recording
    pub fn new(
        turn_id: String,
        session_id: String,
        platform: String,
        model: String,
        provider: String,
    ) -> Self {
        let _ = CURRENT_TURN.lock().map(|mut guard| {
            *guard = Some(start_turn(turn_id.clone(), session_id.clone(), platform, model, provider));
        });
        Self {
            turn_id,
            session_id,
            finish_reason: "unknown".into(),
            response_chars: 0,
            ended: false,
        }
    }

    /// Set the finish reason (call before drop)
    pub fn set_finish_reason(&mut self, reason: String) {
        self.finish_reason = reason;
    }

    /// Set response character count (call before drop)
    pub fn set_response_chars(&mut self, chars: u64) {
        self.response_chars = chars;
    }

    /// Manually end the turn
    pub fn end(&mut self) -> anyhow::Result<()> {
        self.ended = true;
        if let Ok(Some(metrics)) = CURRENT_TURN.lock().map(|mut g| g.take()) {
            end_turn(metrics, self.finish_reason.clone(), self.response_chars)?;
        }
        Ok(())
    }
}

impl Drop for TurnGuard {
    fn drop(&mut self) {
        if !self.ended {
            let _ = self.end();
        }
    }
}

/// Record an API call for the current turn
pub fn record_current_api_call(
    model: Option<String>,
    provider: Option<String>,
    input_tokens: u64,
    output_tokens: u64,
    cache_tokens: u64,
    latency_ms: u64,
) {
    if let Ok(mut guard) = CURRENT_TURN.lock() {
        if let Some(ref mut metrics) = *guard {
            record_api_call(metrics, model, provider, input_tokens, output_tokens, cache_tokens, latency_ms);
        }
    }
}

/// Record a tool call for the current turn
pub fn record_current_tool_call(tool_name: String) {
    if let Ok(mut guard) = CURRENT_TURN.lock() {
        if let Some(ref mut metrics) = *guard {
            record_tool_call(metrics, tool_name);
        }
    }
}

/// Record a compression event for the current turn
pub fn record_current_compression(compression_type: String) {
    if let Ok(mut guard) = CURRENT_TURN.lock() {
        if let Some(ref mut metrics) = *guard {
            record_compression(metrics, compression_type);
        }
    }
}

/// Record an error for the current turn
pub fn record_current_error(error: String) {
    if let Ok(mut guard) = CURRENT_TURN.lock() {
        if let Some(ref mut metrics) = *guard {
            record_error(metrics, error);
        }
    }
}

/// Integration with Hermes Air turn lifecycle
/// Call this at the start of each turn
pub fn on_turn_start(
    turn_id: String,
    session_id: String,
    platform: String,
    model: String,
    provider: String,
) -> TurnGuard {
    TurnGuard::new(turn_id, session_id, platform, model, provider)
}

/// Integration with API call completion
/// Call this after each successful model API call
pub fn on_api_response(
    model: String,
    provider: String,
    input_tokens: u64,
    output_tokens: u64,
    cache_tokens: u64,
    latency_ms: u64,
) {
    record_current_api_call(Some(model), Some(provider), input_tokens, output_tokens, cache_tokens, latency_ms);
}

/// Integration with tool execution
/// Call this before executing each tool
pub fn on_tool_call(tool_name: String) {
    record_current_tool_call(tool_name);
}

/// Integration with context compression
/// Call this when compression is triggered
pub fn on_compression(compression_type: String) {
    record_current_compression(compression_type);
}

/// Integration with error handling
/// Call this when an error occurs during turn
pub fn on_error(error: String) {
    record_current_error(error);
}

/// Get current session metrics (for dashboards/debugging)
pub fn get_current_session_metrics(session_id: &str) -> Option<SessionMetrics> {
    SessionMetrics::load(session_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{export_parity_report, init_parity};
    use std::fs;

    #[test]
    fn test_parity_init() {
        init_parity().unwrap();
    }

    #[test]
    fn test_turn_guard() {
        init_parity().unwrap();

        // Clean up any leftover session file from previous runs
        let state = crate::ParityState::get();
        let _ = fs::remove_file(state.output_dir.join("session_test_session.json"));

        let mut guard = TurnGuard::new(
            "test_turn".into(),
            "test_session".into(),
            "telegram".into(),
            "gpt-4".into(),
            "openai".into(),
        );

        record_current_tool_call("terminal".into());
        record_current_api_call(None, None, 1000, 500, 200, 1500);

        guard.set_finish_reason("stop".into());
        guard.set_response_chars(2500);
        guard.end().unwrap();

        let session = get_current_session_metrics("test_session").unwrap();
        assert_eq!(session.total_turns, 1);
        assert_eq!(session.total_tool_calls, 1);
        assert_eq!(session.total_api_calls, 1);
        assert_eq!(session.total_prompt_tokens, 1200);
    }

    #[test]
    fn test_turn_lifecycle_report() {
        init_parity().unwrap();

        let state = crate::ParityState::get();
        let _ = fs::remove_file(state.output_dir.join("session_session_hooks_456.json"));

        let mut turn = start_turn(
            "test_123".into(),
            "session_hooks_456".into(),
            "telegram".into(),
            "gpt-4".into(),
            "openai".into(),
        );

        record_api_call(&mut turn, None, None, 1000, 500, 200, 1500);
        record_tool_call(&mut turn, "terminal".into());
        record_tool_call(&mut turn, "web_search".into());
        record_compression(&mut turn, "auto".into());
        record_error(&mut turn, "rate limit".into());

        end_turn(turn, "stop".into(), 2500).unwrap();

        let report = export_parity_report("session_hooks_456").unwrap();
        assert!(report.exists());
    }
}