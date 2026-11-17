//! Parity instrumentation for Hermes Air
//!
//! Compatible with Python Hermes `agent.parity` module.
//! Produces identical JSONL output for automated comparison.

pub mod metrics;
pub mod session;
pub mod export;
pub mod hooks;

pub use crate::metrics::TurnMetrics;
pub use crate::session::SessionMetrics;
pub use crate::hooks::TurnGuard;

use std::sync::OnceLock;

/// Global parity state - initialized once per process
static PARITY_STATE: OnceLock<ParityState> = OnceLock::new();

#[derive(Debug)]
pub struct ParityState {
    output_dir: std::path::PathBuf,
}

impl ParityState {
    fn new() -> anyhow::Result<Self> {
        let hermes_home = dirs::home_dir()
            .ok_or_else(|| anyhow::anyhow!("No home directory"))?
            .join(".hermes");
        let output_dir = hermes_home.join("logs").join("parity");
        std::fs::create_dir_all(&output_dir)?;
        Ok(Self { output_dir })
    }

    pub fn get() -> &'static Self {
        PARITY_STATE.get_or_init(|| Self::new().expect("Failed to initialize parity state"))
    }
}

/// Initialize parity instrumentation (call once at startup)
pub fn init_parity() -> anyhow::Result<()> {
    ParityState::get();
    Ok(())
}

/// Start recording a new turn
pub fn start_turn(
    turn_id: String,
    session_id: String,
    platform: String,
    model: String,
    provider: String,
) -> TurnMetrics {
    TurnMetrics::new(turn_id, session_id, platform, model, provider)
}

/// Record an API call within the current turn
pub fn record_api_call(
    metrics: &mut TurnMetrics,
    model: Option<String>,
    provider: Option<String>,
    input_tokens: u64,
    output_tokens: u64,
    cache_tokens: u64,
    latency_ms: u64,
) {
    metrics.record_api_call(model, provider, input_tokens, output_tokens, cache_tokens, latency_ms);
}

/// Record a tool call within the current turn
pub fn record_tool_call(metrics: &mut TurnMetrics, tool_name: String) {
    metrics.record_tool_call(tool_name);
}

/// Record a compression event
pub fn record_compression(metrics: &mut TurnMetrics, compression_type: String) {
    metrics.record_compression(compression_type);
}

/// Record an error in the current turn
pub fn record_error(metrics: &mut TurnMetrics, error: String) {
    metrics.record_error(error);
}

/// Finalize the current turn and persist metrics
pub fn end_turn(
    metrics: TurnMetrics,
    finish_reason: String,
    response_chars: u64,
) -> anyhow::Result<()> {
    let finalized = metrics.finalize(finish_reason, response_chars);
    finalized.persist()?;
    SessionMetrics::update_from_turn(&finalized)?;
    Ok(())
}

/// Export a human-readable parity report for a session
pub fn export_parity_report(session_id: &str) -> anyhow::Result<std::path::PathBuf> {
    export::generate_report(session_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parity_init() {
        init_parity().unwrap();
    }

    #[test]
    fn test_turn_lifecycle() {
        init_parity().unwrap();

        let state = ParityState::get();
        let _ = std::fs::remove_file(state.output_dir.join("session_session_lib_456.json"));

        let mut turn = start_turn(
            "test_123".into(),
            "session_lib_456".into(),
            "telegram".into(),
            "gpt-4".into(),
            "openai".into(),
        );

        record_api_call(&mut turn, None, None, 1000, 500, 200, 1500);
        record_tool_call(&mut turn, "terminal".into());
        record_tool_call(&mut turn, "web_search".into());
        record_compression(&mut turn, "auto".into());
        record_error(&mut turn, "rate limit".into());

        // prompt_tokens must equal the sum of uncached input + cache read.
        assert_eq!(turn.prompt_tokens, 1200);

        end_turn(turn, "stop".into(), 2500).unwrap();

        let report = export_parity_report("session_lib_456").unwrap();
        assert!(report.exists());
    }
}