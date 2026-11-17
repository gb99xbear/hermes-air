//! Turn-level metrics for parity comparison

use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::ParityState;

/// Single turn metrics for parity comparison.
///
/// `input_tokens` is the UNcached portion of the prompt; `cache_tokens` is the
/// cache-read portion; `prompt_tokens` is their sum (the full prompt size). The
/// split mirrors Python Hermes' `usage_pricing.normalize_usage`, where
/// `input_tokens = prompt_total - cache_read - cache_write`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnMetrics {
    pub turn_id: String,
    pub session_id: String,
    pub timestamp: String,
    pub platform: String,
    pub model: String,
    pub provider: String,
    pub api_calls: u64,
    pub tool_calls: u64,
    pub tool_names: Vec<String>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_tokens: u64,
    pub prompt_tokens: u64,
    pub latency_ms: u64,
    pub turn_duration_ms: u64,
    pub compression_triggered: bool,
    pub compression_type: String,
    pub errors: Vec<String>,
    pub finish_reason: String,
    pub response_chars: u64,

    // Internal tracking
    #[serde(skip)]
    start_time_ns: u64,
}

impl TurnMetrics {
    pub fn new(
        turn_id: String,
        session_id: String,
        platform: String,
        model: String,
        provider: String,
    ) -> Self {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let timestamp = chrono::DateTime::from_timestamp(now as i64, 0)
            .unwrap()
            .format("%Y-%m-%dT%H:%M:%S%.3fZ")
            .to_string();

        Self {
            turn_id,
            session_id,
            timestamp,
            platform,
            model,
            provider,
            api_calls: 0,
            tool_calls: 0,
            tool_names: Vec::new(),
            input_tokens: 0,
            output_tokens: 0,
            cache_tokens: 0,
            prompt_tokens: 0,
            latency_ms: 0,
            turn_duration_ms: 0,
            compression_triggered: false,
            compression_type: String::new(),
            errors: Vec::new(),
            finish_reason: String::new(),
            response_chars: 0,
            start_time_ns: std::time::Instant::now().elapsed().as_nanos() as u64,
        }
    }

    pub fn record_api_call(
        &mut self,
        model: Option<String>,
        provider: Option<String>,
        input_tokens: u64,
        output_tokens: u64,
        cache_tokens: u64,
        latency_ms: u64,
    ) {
        self.api_calls += 1;
        if let Some(m) = model {
            self.model = m;
        }
        if let Some(p) = provider {
            self.provider = p;
        }
        self.input_tokens += input_tokens;
        self.output_tokens += output_tokens;
        self.cache_tokens += cache_tokens;
        self.prompt_tokens += input_tokens + cache_tokens;
        self.latency_ms += latency_ms;
    }

    pub fn record_tool_call(&mut self, tool_name: String) {
        self.tool_calls += 1;
        if !self.tool_names.contains(&tool_name) {
            self.tool_names.push(tool_name);
        }
    }

    pub fn record_compression(&mut self, compression_type: String) {
        self.compression_triggered = true;
        self.compression_type = compression_type;
    }

    pub fn record_error(&mut self, error: String) {
        self.errors.push(error);
    }

    pub fn finalize(mut self, finish_reason: String, response_chars: u64) -> FinalizedTurnMetrics {
        let elapsed_ns = std::time::Instant::now().elapsed().as_nanos() as u64;
        self.turn_duration_ms = (elapsed_ns.saturating_sub(self.start_time_ns)) / 1_000_000;
        self.finish_reason = finish_reason;
        self.response_chars = response_chars;
        FinalizedTurnMetrics(self)
    }

    pub fn persist(&self) -> anyhow::Result<()> {
        let state = ParityState::get();
        let date_str = chrono::Utc::now().format("%Y%m%d").to_string();
        let filepath = state.output_dir.join(format!("turns_{}.jsonl", date_str));

        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(filepath)?;

        use std::io::Write;
        writeln!(file, "{}", serde_json::to_string(self)?)?;
        Ok(())
    }
}

/// Wrapper for finalized turn metrics (immutable after finalize)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FinalizedTurnMetrics(TurnMetrics);

impl std::ops::Deref for FinalizedTurnMetrics {
    type Target = TurnMetrics;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl FinalizedTurnMetrics {
    pub fn persist(&self) -> anyhow::Result<()> {
        self.0.persist()
    }
}