//! Report export for parity comparison

use std::fs;

use anyhow::Result;

use crate::metrics::FinalizedTurnMetrics;
use crate::session::SessionMetrics;
use crate::ParityState;

/// Generate a human-readable parity report for a session
pub fn generate_report(session_id: &str) -> Result<std::path::PathBuf> {
    let state = crate::ParityState::get();
    let session = SessionMetrics::load(session_id)
        .ok_or_else(|| anyhow::anyhow!("No session metrics for {}", session_id))?;

    // Load all turns for this session
    let mut turns = Vec::new();
    for entry in fs::read_dir(&state.output_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) == Some("jsonl") {
            let content = fs::read_to_string(&path)?;
            for line in content.lines() {
                if let Ok(turn) = serde_json::from_str::<FinalizedTurnMetrics>(line) {
                    if turn.session_id == session_id {
                        turns.push(turn);
                    }
                }
            }
        }
    }

    // Generate markdown report
    let mut lines = Vec::new();
    lines.push(format!("# Parity Report: {}", session_id));
    lines.push(String::new());
    lines.push(format!("**Platform:** {}", session.platform));
    lines.push(format!("**Start:** {}", session.start_time));
    lines.push(format!("**Total Turns:** {}", session.total_turns));
    lines.push(format!("**Total API Calls:** {}", session.total_api_calls));
    lines.push(format!("**Total Tool Calls:** {}", session.total_tool_calls));
    lines.push(format!("**Total Input Tokens (uncached):** {}", session.total_input_tokens));
    lines.push(format!("**Total Prompt Tokens (input + cache):** {}", session.total_prompt_tokens));
    lines.push(format!("**Total Output Tokens:** {}", session.total_output_tokens));
    lines.push(format!("**Avg Latency/Call:** {:.0}ms", session.avg_latency_per_call()));
    lines.push(format!("**Compressions:** {}", session.compressions));
    lines.push(format!("**Errors:** {}", session.errors));
    lines.push(String::new());
    lines.push("## Turn Breakdown".into());
    lines.push(String::new());
    lines.push(
        "| Turn | Model | API Calls | Tools | Prompt | Out Tokens | Latency | Duration | Compression |".into()
    );
    lines.push(
        "|------|-------|-----------|-------|--------|------------|---------|----------|-------------|".into()
    );

    for turn in &turns {
        lines.push(format!(
            "| {} | {} | {} | {} | {} | {} | {}ms | {}ms | {} |",
            &turn.turn_id[..8.min(turn.turn_id.len())],
            turn.model,
            turn.api_calls,
            turn.tool_calls,
            turn.prompt_tokens,
            turn.output_tokens,
            turn.latency_ms,
            turn.turn_duration_ms,
            if turn.compression_type.is_empty() { "-" } else { &turn.compression_type }
        ));
    }

    lines.push(String::new());
    lines.push("## Tool Usage".into());
    lines.push(String::new());

    for (tool, count) in session.tool_usage() {
        lines.push(format!("- **{}**: {} calls", tool, count));
    }

    let report = lines.join("\n");

    let output_path = state.output_dir.join(format!("parity_report_{}.md", session_id));
    fs::write(&output_path, report)?;

    Ok(output_path)
}

/// Compare two parity reports (Python vs Air) and generate a diff summary
pub fn compare_reports(
    python_session_id: &str,
    air_session_id: &str,
) -> Result<ComparisonReport> {
    let python_session = SessionMetrics::load(python_session_id)
        .ok_or_else(|| anyhow::anyhow!("No Python session metrics for {}", python_session_id))?;
    let air_session = SessionMetrics::load(air_session_id)
        .ok_or_else(|| anyhow::anyhow!("No Air session metrics for {}", air_session_id))?;

    let api_calls_diff = percent_diff(python_session.total_api_calls, air_session.total_api_calls);
    let tool_calls_diff = percent_diff(python_session.total_tool_calls, air_session.total_tool_calls);
    let prompt_tokens_diff = percent_diff(python_session.total_prompt_tokens, air_session.total_prompt_tokens);
    let output_tokens_diff = percent_diff(python_session.total_output_tokens, air_session.total_output_tokens);
    let latency_diff = percent_diff(python_session.total_latency_ms, air_session.total_latency_ms);
    let compressions_diff = percent_diff(python_session.compressions, air_session.compressions);

    Ok(ComparisonReport {
        python_session_id: python_session_id.to_string(),
        air_session_id: air_session_id.to_string(),
        api_calls_diff_pct: api_calls_diff,
        tool_calls_diff_pct: tool_calls_diff,
        prompt_tokens_diff_pct: prompt_tokens_diff,
        output_tokens_diff_pct: output_tokens_diff,
        latency_diff_pct: latency_diff,
        compressions_diff_pct: compressions_diff,
        within_tolerance: api_calls_diff.abs() <= 10.0
            && tool_calls_diff.abs() <= 10.0
            && prompt_tokens_diff.abs() <= 10.0
            && output_tokens_diff.abs() <= 10.0
            && latency_diff.abs() <= 10.0
            && compressions_diff.abs() <= 10.0,
    })
}

fn percent_diff(baseline: u64, compare: u64) -> f64 {
    if baseline == 0 {
        return 0.0;
    }
    ((compare as f64 - baseline as f64) / baseline as f64) * 100.0
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ComparisonReport {
    pub python_session_id: String,
    pub air_session_id: String,
    pub api_calls_diff_pct: f64,
    pub tool_calls_diff_pct: f64,
    pub prompt_tokens_diff_pct: f64,
    pub output_tokens_diff_pct: f64,
    pub latency_diff_pct: f64,
    pub compressions_diff_pct: f64,
    pub within_tolerance: bool,
}

impl ComparisonReport {
    pub fn summary(&self) -> String {
        let mut lines = Vec::new();
        lines.push(format!(
            "# Parity Comparison: {} vs {}",
            self.python_session_id, self.air_session_id
        ));
        lines.push(String::new());
        lines.push("| Metric | Diff | Status |".to_string());
        lines.push("|--------|------|--------|".to_string());

        let row = |name: &str, diff: f64| {
            format!(
                "| {} | {:.1}% | {} |",
                name,
                diff,
                if diff.abs() <= 10.0 { "PASS" } else { "FAIL" }
            )
        };
        lines.push(row("API Calls", self.api_calls_diff_pct));
        lines.push(row("Tool Calls", self.tool_calls_diff_pct));
        lines.push(row("Prompt Tokens", self.prompt_tokens_diff_pct));
        lines.push(row("Output Tokens", self.output_tokens_diff_pct));
        lines.push(row("Latency", self.latency_diff_pct));
        lines.push(row("Compressions", self.compressions_diff_pct));

        lines.push(String::new());
        lines.push(format!(
            "**Overall:** {}",
            if self.within_tolerance { "WITHIN TOLERANCE (+/-10%)" } else { "OUTSIDE TOLERANCE" }
        ));

        lines.join("\n")
    }
}