//! Parity instrumentation for Hermes Air
//!
//! Compatible with Python Hermes `agent.parity` module.
//! Produces identical JSONL output for automated comparison.

pub mod export;
pub mod hooks;
pub mod metrics;
pub mod session;

pub use crate::hooks::TurnGuard;
pub use crate::metrics::TurnMetrics;
pub use crate::session::SessionMetrics;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

/// Global parity state - initialized once per process.
static PARITY_STATE: OnceLock<ParityState> = OnceLock::new();

/// Parity is OFF unless a host explicitly opts in (see `init_parity_with`).
/// Recording entry points are cheap no-ops while disabled, so an unopted-in
/// build never touches the filesystem.
static ENABLED: AtomicBool = AtomicBool::new(false);

/// Whether parity recording is currently enabled.
pub fn is_enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

#[derive(Debug)]
pub struct ParityState {
    output_dir: std::path::PathBuf,
}

/// Default output dir: $HERMES_HOME/logs/parity, else ~/.hermes/logs/parity.
fn default_parity_dir() -> std::path::PathBuf {
    if let Ok(home) = std::env::var("HERMES_HOME") {
        if !home.trim().is_empty() {
            return std::path::PathBuf::from(home).join("logs").join("parity");
        }
    }
    dirs::home_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join(".hermes")
        .join("logs")
        .join("parity")
}

impl ParityState {
    /// Never panics: falls back to the default dir. Directory creation is
    /// best-effort at write time, so a read-only home simply drops records.
    pub fn get() -> &'static Self {
        PARITY_STATE.get_or_init(|| ParityState {
            output_dir: default_parity_dir(),
        })
    }
}

/// Initialize parity instrumentation (disabled). Kept for API compatibility.
pub fn init_parity() -> anyhow::Result<()> {
    init_parity_with(false, None)
}

/// Initialize parity instrumentation. `enabled` is opt-in; `dir` overrides the
/// output directory. Never panics and never fails the caller.
pub fn init_parity_with(enabled: bool, dir: Option<std::path::PathBuf>) -> anyhow::Result<()> {
    if !enabled {
        ENABLED.store(false, Ordering::Relaxed);
        return Ok(());
    }
    let output_dir = dir.unwrap_or_else(default_parity_dir);
    let _ = PARITY_STATE.set(ParityState { output_dir });
    ENABLED.store(true, Ordering::Relaxed);
    Ok(())
}

/// Test-only: enable parity into a shared temp dir (never the user's home).
#[cfg(test)]
pub(crate) fn test_enable_parity() {
    let dir = std::env::temp_dir().join("hermes_parity_tests");
    let _ = std::fs::create_dir_all(&dir);
    let _ = init_parity_with(true, Some(dir));
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
    metrics.record_api_call(
        model,
        provider,
        input_tokens,
        output_tokens,
        cache_tokens,
        latency_ms,
    );
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
    if !is_enabled() {
        return Ok(());
    }
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
        crate::test_enable_parity();
        assert!(crate::is_enabled());
    }

    #[test]
    fn test_turn_lifecycle() {
        crate::test_enable_parity();

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
