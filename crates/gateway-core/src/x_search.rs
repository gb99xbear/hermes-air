//! X Search tool backed by xAI's built-in `x_search` Responses API tool.
//!
//! 1-to-1 port of `tools/x_search_tool.py`. Registers when either xAI
//! credential path is available (`XAI_API_KEY` or an OAuth bearer). At call
//! time an explicit API key WINS (`prefer_api_key`): x_search is API-metered,
//! and the subscription OAuth bearer answers `/v1/responses` without
//! citations. Date filters are validated client-side so a malformed window
//! fails fast instead of burning a billable call.
//!
//! Results carry `degraded: true` when a narrowing filter was active AND xAI
//! returned no citations in either channel — the answer came from model
//! knowledge, not the X index.

use serde_json::{json, Map, Value};
use time::format_description::FormatItem;
use time::macros::format_description;
use time::Date;

/// Default model for the x_search Responses call.
pub const DEFAULT_X_SEARCH_MODEL: &str = "grok-4.5";
pub const DEFAULT_X_SEARCH_TIMEOUT_SECONDS: u64 = 180;
pub const DEFAULT_X_SEARCH_RETRIES: u32 = 2;
/// Accepted `reasoning.effort` values.
pub const X_SEARCH_REASONING_EFFORTS: [&str; 4] = ["low", "medium", "high", "xhigh"];
/// Maximum handles per include/exclude list.
pub const MAX_HANDLES: usize = 10;
/// xAI API base URL when nothing overrides it.
pub const DEFAULT_XAI_BASE_URL: &str = "https://api.x.ai/v1";

const YMD: &[FormatItem<'static>] = format_description!("[year]-[month]-[day]");

// ── Errors ─────────────────────────────────────────────────────────────────

/// Mirrors `tool_error()`: `{"error": <bounded message>}`.
pub fn tool_error(message: &str) -> String {
    json!({ "error": bound_error_text(message) }).to_string()
}

pub fn bound_error_text(text: &str) -> String {
    const MAX: usize = 2048;
    const MARKER: &str = "... [truncated]";
    if text.chars().count() <= MAX {
        return text.to_string();
    }
    format!("{}{}", text.chars().take(MAX).collect::<String>(), MARKER)
}

/// Mirrors `_error_json`: the structured failure body with the Rust-side
/// exception class standing in for Python's exception name.
fn error_json(error: &str, error_type: &str) -> String {
    json!({
        "success": false,
        "provider": "xai",
        "tool": "x_search",
        "error": error,
        "error_type": error_type,
    })
    .to_string()
}

// ── Config ─────────────────────────────────────────────────────────────────

/// The `x_search` config section, already resolved by the caller.
#[derive(Debug, Clone)]
pub struct XSearchConfig {
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub timeout_seconds: Option<u64>,
    pub retries: Option<u32>,
}

impl Default for XSearchConfig {
    fn default() -> Self {
        Self {
            model: None,
            reasoning_effort: None,
            timeout_seconds: None,
            retries: None,
        }
    }
}

impl XSearchConfig {
    /// Validate and normalize `reasoning_effort` → `Ok(None)` when unset.
    pub fn resolved_reasoning_effort(&self) -> Result<Option<String>, String> {
        let effort = self
            .reasoning_effort
            .as_deref()
            .map(|v| v.trim().to_lowercase())
            .unwrap_or_default();
        if !effort.is_empty() && !X_SEARCH_REASONING_EFFORTS.contains(&effort.as_str()) {
            return Err(format!(
                "x_search.reasoning_effort must be one of: {} (got {:?})",
                X_SEARCH_REASONING_EFFORTS.join(", "),
                self.reasoning_effort
                    .as_deref()
                    .unwrap_or("")
            ));
        }
        Ok(if effort.is_empty() { None } else { Some(effort) })
    }

    /// `max(floor, configured)` with the default on any failure.
    pub fn int(&self, key: &str, default: u64, floor: u64) -> u64 {
        let raw: Option<u64> = match key {
            "timeout_seconds" => self.timeout_seconds,
            "retries" => self.retries.map(u64::from),
            _ => None,
        };
        match raw {
            Some(v) => v.max(floor),
            None => default,
        }
    }

    pub fn model_or_default(&self) -> String {
        self.model
            .as_deref()
            .map(|m| m.trim().to_string())
            .filter(|m| !m.is_empty())
            .unwrap_or_else(|| DEFAULT_X_SEARCH_MODEL.to_string())
    }
}

// ── Credentials ────────────────────────────────────────────────────────────

/// A resolved xAI HTTP credential triple.
#[derive(Debug, Clone)]
pub struct XaiCredentials {
    pub api_key: String,
    pub base_url: String,
    /// `"xai-oauth"` or `"xai"`.
    pub source: String,
}

/// Platform seam for xAI credential resolution (env var, secret store, OAuth
/// refresh). Returns `None` when no usable credential exists — the tool then
/// reports a clean error instead of a 401.
///
/// Implementations must check an explicit `XAI_API_KEY` FIRST when
/// `prefer_api_key` is set (the OAuth bearer authorizes but answers without
/// citations), keeping OAuth as the fallback.
pub trait XaiCredentialProvider: Send + Sync {
    fn resolve(&self, prefer_api_key: bool) -> Option<XaiCredentials>;

    /// True when any credential resolves (the tool's availability check).
    fn available(&self) -> bool {
        self.resolve(false).is_some()
    }

    /// User-Agent for xAI HTTP calls.
    fn user_agent(&self) -> String {
        "Hermes-Agent/unknown".to_string()
    }
}

/// Normalize a resolved credential: trim the key, default + trim the base URL.
fn finish_credentials(creds: XaiCredentials) -> Result<XaiCredentials, String> {
    let api_key = creds.api_key.trim().to_string();
    if api_key.is_empty() {
        return Err(
            "No xAI credentials available. Run `hermes auth add xai-oauth` to sign in with your \
             SuperGrok subscription, or set XAI_API_KEY."
                .to_string(),
        );
    }
    let base_url = {
        let raw = creds.base_url.trim();
        let raw = if raw.is_empty() { DEFAULT_XAI_BASE_URL } else { raw };
        raw.trim_end_matches('/').to_string()
    };
    let source = if creds.source.trim().is_empty() {
        "xai".to_string()
    } else {
        creds.source
    };
    Ok(XaiCredentials {
        api_key,
        base_url,
        source,
    })
}

// ── Filter validation ──────────────────────────────────────────────────────

/// Strip leading `@`, trim, drop empties; refuse more than `MAX_HANDLES`.
pub fn normalize_handles(handles: Option<&[String]>, field_name: &str) -> Result<Vec<String>, String> {
    let cleaned: Vec<String> = handles
        .unwrap_or(&[])
        .iter()
        .map(|h| h.trim().trim_start_matches('@').trim().to_string())
        .filter(|h| !h.is_empty())
        .collect();
    if cleaned.len() > MAX_HANDLES {
        return Err(format!(
            "{} supports at most {} handles",
            field_name, MAX_HANDLES
        ));
    }
    Ok(cleaned)
}

/// Strict `YYYY-MM-DD`, or `None` for blank (xAI silently accepts malformed
/// dates and returns no citations).
pub fn parse_iso_date(value: &str, field_name: &str) -> Result<Option<Date>, String> {
    let raw = value.trim();
    if raw.is_empty() {
        return Ok(None);
    }
    Date::parse(raw, YMD)
        .map(Some)
        .map_err(|_| format!("{} must be YYYY-MM-DD (got {:?})", field_name, raw))
}

/// Both parse as `YYYY-MM-DD`; from <= to; from not after today UTC (to may be
/// in the future).
pub fn validate_date_range(from_date: &str, to_date: &str) -> Result<(), String> {
    validate_date_range_at(from_date, to_date, time::OffsetDateTime::now_utc().date())
}

/// `validate_date_range` against an injected "today" so the boundary is testable.
pub fn validate_date_range_at(
    from_date: &str,
    to_date: &str,
    today_utc: Date,
) -> Result<(), String> {
    let parsed_from = parse_iso_date(from_date, "from_date")?;
    let parsed_to = parse_iso_date(to_date, "to_date")?;
    if let (Some(f), Some(t)) = (parsed_from, parsed_to) {
        if f > t {
            return Err(format!(
                "from_date ({}) must be on or before to_date ({})",
                f, t
            ));
        }
    }
    if let Some(f) = parsed_from {
        if f > today_utc {
            return Err(format!(
                "from_date ({}) is in the future; X Search only indexes past posts (today UTC is {})",
                f, today_utc
            ));
        }
    }
    Ok(())
}

/// Build the Responses `tools[]` entry → `(tool_def, active_filters)`.
pub fn build_x_search_tool_def(
    allowed_x_handles: Option<&[String]>,
    excluded_x_handles: Option<&[String]>,
    from_date: &str,
    to_date: &str,
    enable_image_understanding: bool,
    enable_video_understanding: bool,
) -> Result<(Value, Vec<String>), String> {
    let allowed = normalize_handles(allowed_x_handles, "allowed_x_handles")?;
    let excluded = normalize_handles(excluded_x_handles, "excluded_x_handles")?;
    if !allowed.is_empty() && !excluded.is_empty() {
        return Err(
            "allowed_x_handles and excluded_x_handles cannot be used together".to_string(),
        );
    }
    validate_date_range(from_date, to_date)?;

    let mut tool_def = Map::new();
    tool_def.insert("type".to_string(), json!("x_search"));
    let mut active_filters: Vec<String> = Vec::new();
    let filters: [(&str, Value); 4] = [
        ("allowed_x_handles", json!(allowed)),
        ("excluded_x_handles", json!(excluded)),
        ("from_date", json!(from_date.trim())),
        ("to_date", json!(to_date.trim())),
    ];
    for (key, value) in filters {
        let active = match &value {
            Value::String(s) => !s.is_empty(),
            Value::Array(a) => !a.is_empty(),
            _ => false,
        };
        if active {
            tool_def.insert(key.to_string(), value);
            active_filters.push(key.to_string());
        }
    }
    if enable_image_understanding {
        tool_def.insert("enable_image_understanding".to_string(), json!(true));
    }
    if enable_video_understanding {
        tool_def.insert("enable_video_understanding".to_string(), json!(true));
    }
    Ok((Value::Object(tool_def), active_filters))
}

// ── Response extraction ────────────────────────────────────────────────────

/// Every `content[]` entry inside every `output[]` item of type `message`.
fn message_contents(payload: &Value) -> Vec<&Value> {
    let mut out = Vec::new();
    if let Some(items) = payload.get("output").and_then(|v| v.as_array()) {
        for item in items {
            if item.get("type").and_then(|t| t.as_str()) == Some("message") {
                if let Some(contents) = item.get("content").and_then(|c| c.as_array()) {
                    out.extend(contents.iter());
                }
            }
        }
    }
    out
}

/// The answer text: top-level `output_text` first, else joined content parts.
pub fn extract_response_text(payload: &Value) -> String {
    let output_text = payload
        .get("output_text")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if !output_text.is_empty() {
        return output_text;
    }
    let parts: Vec<String> = message_contents(payload)
        .into_iter()
        .filter(|c| {
            matches!(
                c.get("type").and_then(|t| t.as_str()),
                Some("output_text") | Some("text")
            )
        })
        .map(|c| {
            c.get("text")
                .and_then(|t| t.as_str())
                .unwrap_or("")
                .trim()
                .to_string()
        })
        .filter(|p| !p.is_empty())
        .collect();
    parts.join("\n\n").trim().to_string()
}

/// `url_citation` annotations across all message contents.
pub fn extract_inline_citations(payload: &Value) -> Vec<Value> {
    let mut out = Vec::new();
    for content in message_contents(payload) {
        let Some(annotations) = content.get("annotations").and_then(|a| a.as_array()) else {
            continue;
        };
        for a in annotations {
            if a.get("type").and_then(|t| t.as_str()) == Some("url_citation") {
                out.push(json!({
                    "url": a.get("url").cloned().unwrap_or(json!("")),
                    "title": a.get("title").cloned().unwrap_or(json!("")),
                    "start_index": a.get("start_index").cloned().unwrap_or(Value::Null),
                    "end_index": a.get("end_index").cloned().unwrap_or(Value::Null),
                }));
            }
        }
    }
    out
}

/// Human-readable message from an upstream error body.
pub fn http_error_message(body: Option<&Value>, raw_text: &str, fallback: &str) -> String {
    let Some(Value::Object(map)) = body else {
        let text = raw_text.trim();
        return if text.is_empty() {
            fallback.to_string()
        } else {
            text.chars().take(500).collect()
        };
    };
    let code = map
        .get("code")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let message = map
        .get("error")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let message = if message.is_empty() {
        Value::Object(map.clone()).to_string()
    } else {
        message
    };
    if !code.is_empty() && !message.contains(&code) {
        format!("{}: {}", code, message)
    } else if message.is_empty() {
        fallback.to_string()
    } else {
        message
    }
}

// ── Retry policy ───────────────────────────────────────────────────────────

/// What to do after one failed attempt.
#[derive(Debug, PartialEq, Eq)]
pub enum RetryDecision {
    /// Give up and surface the error.
    Stop,
    /// Retry: the failure was upstream (5xx) or transient (timeout/connection).
    Retry,
}

/// Retry on 5xx / timeout / connection errors only; never on 4xx.
/// `status` is `None` for a transport failure.
pub fn retry_decision(status: Option<u16>, attempt: u32, max_retries: u32) -> RetryDecision {
    if attempt >= max_retries {
        return RetryDecision::Stop;
    }
    match status {
        None => RetryDecision::Retry,
        Some(code) if code >= 500 => RetryDecision::Retry,
        Some(_) => RetryDecision::Stop,
    }
}

/// Backoff before the next attempt (mirrors `min(5.0, 1.5 * (attempt + 1))`).
pub fn retry_delay_seconds(attempt: u32) -> f64 {
    (1.5 * (attempt as f64 + 1.0)).min(5.0)
}

// ── Tool entry point ───────────────────────────────────────────────────────

/// Which failure class produced an error (mirrors Python's except ladder).
#[derive(Debug, PartialEq, Eq)]
pub enum XSearchErrorKind {
    /// `requests.HTTPError`
    Http,
    /// `requests.ReadTimeout`
    Timeout,
    /// Any other exception
    Other,
}

/// Build the x_search result JSON from a successful Responses payload. Pure, so
/// the whole result shape is testable without a network call.
pub fn build_result(
    payload: &Value,
    model: &str,
    query: &str,
    source: &str,
    active_filters: &[String],
) -> String {
    let citations = payload
        .get("citations")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let inline_citations = extract_inline_citations(payload);
    // xAI returns 200 with a synthesized answer even when no posts match the
    // narrowing filters; with both citation channels empty the answer came
    // from training data.
    let degraded = !active_filters.is_empty() && citations.is_empty() && inline_citations.is_empty();
    json!({
        "success": true,
        "provider": "xai",
        "credential_source": source,
        "tool": "x_search",
        "model": model,
        "query": query.trim(),
        "answer": extract_response_text(payload),
        "citations": citations,
        "inline_citations": inline_citations,
        "degraded": degraded,
        "degraded_reason": if degraded {
            json!(format!(
                "no citations returned despite filters: {}",
                active_filters.join(", ")
            ))
        } else {
            Value::Null
        },
    })
    .to_string()
}

/// Shape the failure body for one error class.
pub fn build_error(kind: &XSearchErrorKind, detail: &str, timeout_seconds: u64) -> String {
    match kind {
        XSearchErrorKind::Http => error_json(detail, "HTTPError"),
        XSearchErrorKind::Timeout => error_json(
            &format!("xAI x_search timed out after {} seconds", timeout_seconds),
            "ReadTimeout",
        ),
        XSearchErrorKind::Other => error_json(detail, "Error"),
    }
}

/// Validate the call, build the request payload and report the (model, tool_def,
/// active_filters) the transport should use. Pure: no network, no sleeping.
pub fn prepare(
    query: &str,
    allowed_x_handles: Option<&[String]>,
    excluded_x_handles: Option<&[String]>,
    from_date: &str,
    to_date: &str,
    enable_image_understanding: bool,
    enable_video_understanding: bool,
    config: &XSearchConfig,
    creds: Result<XaiCredentials, String>,
) -> Result<PreparedCall, String> {
    if query.trim().is_empty() {
        return Err("query is required for x_search".to_string());
    }
    let creds = finish_credentials(creds?)?;
    let (tool_def, active_filters) = build_x_search_tool_def(
        allowed_x_handles,
        excluded_x_handles,
        from_date,
        to_date,
        enable_image_understanding,
        enable_video_understanding,
    )?;
    let reasoning_effort = config.resolved_reasoning_effort()?;

    let model = config.model_or_default();
    let mut payload = Map::new();
    payload.insert("model".to_string(), json!(model));
    payload.insert(
        "input".to_string(),
        json!([{ "role": "user", "content": query.trim() }]),
    );
    payload.insert("tools".to_string(), json!([tool_def]));
    payload.insert("store".to_string(), json!(false));
    if let Some(effort) = &reasoning_effort {
        payload.insert("reasoning".to_string(), json!({ "effort": effort }));
    }

    Ok(PreparedCall {
        url: format!("{}/responses", creds.base_url),
        model,
        payload: Value::Object(payload),
        active_filters,
        source: creds.source,
        timeout_seconds: config.int(
            "timeout_seconds",
            DEFAULT_X_SEARCH_TIMEOUT_SECONDS,
            30,
        ),
        max_retries: config.int("retries", DEFAULT_X_SEARCH_RETRIES as u64, 0) as u32,
    })
}

/// Everything the transport needs for one x_search call.
#[derive(Debug, Clone)]
pub struct PreparedCall {
    pub url: String,
    pub model: String,
    pub payload: Value,
    pub active_filters: Vec<String>,
    pub source: String,
    pub timeout_seconds: u64,
    pub max_retries: u32,
}

/// Perform a prepared call with the retry policy: retries on 5xx / timeout /
/// connection errors, never on 4xx. Returns the tool result JSON.
pub async fn run_prepared(
    client: &reqwest::Client,
    call: &PreparedCall,
    api_key: &str,
    user_agent: &str,
) -> String {
    let mut attempt: u32 = 0;
    loop {
        let sent = client
            .post(&call.url)
            .header("Authorization", format!("Bearer {}", api_key))
            .header("Content-Type", "application/json")
            .header("User-Agent", user_agent)
            .json(&call.payload)
            .timeout(std::time::Duration::from_secs(call.timeout_seconds))
            .send()
            .await;

        match sent {
            Ok(resp) => {
                let status = resp.status();
                let text = resp.text().await.unwrap_or_default();
                if status.is_success() {
                    return match serde_json::from_str::<Value>(&text) {
                        Ok(data) => build_result(
                            &data,
                            &call.model,
                            "",
                            &call.source,
                            &call.active_filters,
                        ),
                        Err(e) => build_error(
                            &XSearchErrorKind::Other,
                            &format!("invalid JSON from xAI: {}", e),
                            call.timeout_seconds,
                        ),
                    };
                }
                let body: Option<Value> = serde_json::from_str(&text).ok();
                let detail = http_error_message(body.as_ref(), &text, &format!("HTTP {}", status));
                if retry_decision(Some(status.as_u16()), attempt, call.max_retries)
                    == RetryDecision::Stop
                {
                    return build_error(&XSearchErrorKind::Http, &detail, call.timeout_seconds);
                }
                tracing::warn!(
                    "x_search upstream failure on attempt {}/{}: {}",
                    attempt + 1,
                    call.max_retries + 1,
                    detail
                );
            }
            Err(e) => {
                if retry_decision(None, attempt, call.max_retries) == RetryDecision::Stop {
                    let kind = if e.is_timeout() {
                        XSearchErrorKind::Timeout
                    } else {
                        XSearchErrorKind::Other
                    };
                    return build_error(&kind, &e.to_string(), call.timeout_seconds);
                }
                tracing::warn!(
                    "x_search transient failure on attempt {}/{}: {}",
                    attempt + 1,
                    call.max_retries + 1,
                    e
                );
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs_f64(retry_delay_seconds(attempt))).await;
        attempt += 1;
    }
}

/// Tool definition for `x_search` (mirrors the Python `X_SEARCH_SCHEMA`).
pub fn get_definition() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": "x_search",
            "description": "Search X (Twitter) posts, profiles, and threads using xAI's built-in X Search tool. Read-only discovery only: use this for current discussion, reactions, or claims on public X rather than general web pages. Do not use it to post, reply, like, DM, upload media, delete, or inspect the user's authenticated X account — those require a separate authenticated X API surface outside this tool. Available when xAI credentials are configured (SuperGrok OAuth or XAI_API_KEY).",
            "parameters": {
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "What to look up on X."
                    },
                    "allowed_x_handles": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Optional list of X handles to include exclusively (max 10)."
                    },
                    "excluded_x_handles": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Optional list of X handles to exclude (max 10)."
                    },
                    "from_date": {
                        "type": "string",
                        "description": "Optional start date in YYYY-MM-DD format."
                    },
                    "to_date": {
                        "type": "string",
                        "description": "Optional end date in YYYY-MM-DD format."
                    },
                    "enable_image_understanding": {
                        "type": "boolean",
                        "description": "Whether xAI should analyze images attached to matching X posts.",
                        "default": false
                    },
                    "enable_video_understanding": {
                        "type": "boolean",
                        "description": "Whether xAI should analyze videos attached to matching X posts.",
                        "default": false
                    }
                },
                "required": ["query"]
            }
        }
    })
}

/// Argument-shape guard for the flat tool call: `Err(json)` when the call can be
/// rejected before any credential or network work.
pub fn validate_args(query: &str) -> Result<(), String> {
    if query.trim().is_empty() {
        return Err("query is required for x_search".to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::date;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    fn creds() -> XaiCredentials {
        XaiCredentials {
            api_key: "  sk-test  ".to_string(),
            base_url: "https://api.x.ai/v1/".to_string(),
            source: "xai".to_string(),
        }
    }

    // ── credentials ──────────────────────────────────────────────────────
    #[test]
    fn finish_credentials_trims_key_and_url() {
        let c = finish_credentials(creds()).unwrap();
        assert_eq!(c.api_key, "sk-test");
        assert_eq!(c.base_url, "https://api.x.ai/v1");
    }

    #[test]
    fn finish_credentials_empty_key_is_a_clean_refusal() {
        let bad = XaiCredentials {
            api_key: "   ".to_string(),
            base_url: String::new(),
            source: String::new(),
        };
        let err = finish_credentials(bad).unwrap_err();
        assert!(err.contains("No xAI credentials available"));
    }

    #[test]
    fn finish_credentials_defaults_blank_base_url() {
        let c = finish_credentials(XaiCredentials {
            api_key: "k".to_string(),
            base_url: "  ".to_string(),
            source: "  ".to_string(),
        })
        .unwrap();
        assert_eq!(c.base_url, DEFAULT_XAI_BASE_URL);
        assert_eq!(c.source, "xai");
    }

    // ── handles ──────────────────────────────────────────────────────────
    #[test]
    fn normalize_handles_strips_at_and_blanks() {
        let out = normalize_handles(Some(&s(&[" @alice ", "@bob", "", "   "])), "allowed").unwrap();
        assert_eq!(out, s(&["alice", "bob"]));
    }

    #[test]
    fn normalize_handles_caps_at_max() {
        let many: Vec<String> = (0..=MAX_HANDLES).map(|i| format!("h{}", i)).collect();
        let err = normalize_handles(Some(&many), "allowed_x_handles").unwrap_err();
        assert!(err.contains("at most 10"));
    }

    #[test]
    fn normalize_handles_absent_is_empty_not_error() {
        assert!(normalize_handles(None, "allowed").unwrap().is_empty());
    }

    // ── dates ────────────────────────────────────────────────────────────
    #[test]
    fn parse_iso_date_blank_is_none() {
        assert_eq!(parse_iso_date("   ", "from_date").unwrap(), None);
    }

    #[test]
    fn parse_iso_date_strict_rejects_malformed() {
        let err = parse_iso_date("01-02-2026", "from_date").unwrap_err();
        assert!(err.contains("must be YYYY-MM-DD"));
        assert!(parse_iso_date("2026-13-01", "from_date").is_err());
        assert!(parse_iso_date("2026-02-30", "from_date").is_err());
    }

    #[test]
    fn parse_iso_date_accepts_valid() {
        assert_eq!(
            parse_iso_date("2026-03-04", "from_date").unwrap(),
            Some(date!(2026 - 03 - 04))
        );
    }

    #[test]
    fn validate_date_range_rejects_reversed_window() {
        let err = validate_date_range_at("2026-05-02", "2026-05-01", date!(2026 - 06 - 01))
            .unwrap_err();
        assert!(err.contains("must be on or before"));
    }

    #[test]
    fn validate_date_range_rejects_future_from_date() {
        let err = validate_date_range_at("2027-01-01", "", date!(2026 - 06 - 01)).unwrap_err();
        assert!(err.contains("is in the future"));
        assert!(err.contains("today UTC is 2026-06-01"));
    }

    #[test]
    fn validate_date_range_allows_future_to_date() {
        // "to" may be in the future; only "from" is bounded by today.
        assert!(validate_date_range_at("2026-05-01", "2027-01-01", date!(2026 - 06 - 01)).is_ok());
    }

    #[test]
    fn validate_date_range_allows_equal_dates() {
        assert!(validate_date_range_at("2026-05-01", "2026-05-01", date!(2026 - 06 - 01)).is_ok());
    }

    // ── tool def ─────────────────────────────────────────────────────────
    #[test]
    fn tool_def_includes_only_active_filters() {
        let (def, filters) = build_x_search_tool_def(
            Some(&s(&["alice"])),
            None,
            "2026-01-01",
            "",
            false,
            false,
        )
        .unwrap();
        assert_eq!(def["type"], json!("x_search"));
        assert_eq!(def["allowed_x_handles"], json!(["alice"]));
        assert_eq!(def["from_date"], json!("2026-01-01"));
        assert!(def.get("to_date").is_none());
        assert_eq!(filters, s(&["allowed_x_handles", "from_date"]));
    }

    #[test]
    fn tool_def_refuses_allowed_and_excluded_together() {
        let err = build_x_search_tool_def(Some(&s(&["a"])), Some(&s(&["b"])), "", "", false, false)
            .unwrap_err();
        assert!(err.contains("cannot be used together"));
    }

    #[test]
    fn tool_def_sets_understanding_flags() {
        let (def, filters) =
            build_x_search_tool_def(None, None, "", "", true, true).unwrap();
        assert_eq!(def["enable_image_understanding"], json!(true));
        assert_eq!(def["enable_video_understanding"], json!(true));
        // Understanding flags are not "narrowing filters".
        assert!(filters.is_empty());
    }

    // ── extraction ───────────────────────────────────────────────────────
    #[test]
    fn extract_response_text_prefers_output_text() {
        let payload = json!({"output_text": "  direct  ", "output": []});
        assert_eq!(extract_response_text(&payload), "direct");
    }

    #[test]
    fn extract_response_text_joins_content_parts() {
        let payload = json!({
            "output": [
                {"type": "message", "content": [
                    {"type": "output_text", "text": "first"},
                    {"type": "text", "text": "second"},
                    {"type": "other", "text": "ignored"}
                ]}
            ]
        });
        assert_eq!(extract_response_text(&payload), "first\n\nsecond");
    }

    #[test]
    fn extract_response_text_empty_when_nothing() {
        assert_eq!(extract_response_text(&json!({})), "");
        assert_eq!(
            extract_response_text(&json!({"output": [{"type": "tool", "content": []}]})),
            ""
        );
    }

    #[test]
    fn extract_inline_citations_only_url_citation() {
        let payload = json!({
            "output": [
                {"type": "message", "content": [
                    {"type": "output_text", "text": "a", "annotations": [
                        {"type": "url_citation", "url": "https://x.com/1", "title": "T",
                         "start_index": 0, "end_index": 4},
                        {"type": "other_citation", "url": "https://nope"}
                    ]}
                ]}
            ]
        });
        let out = extract_inline_citations(&payload);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["url"], json!("https://x.com/1"));
        assert_eq!(out[0]["end_index"], json!(4));
    }

    // ── http error message ───────────────────────────────────────────────
    #[test]
    fn http_error_message_prefixes_unseen_code() {
        let body = json!({"code": "rate_limit", "error": "too many requests"});
        assert_eq!(
            http_error_message(Some(&body), "", "fallback"),
            "rate_limit: too many requests"
        );
    }

    #[test]
    fn http_error_message_avoids_double_code() {
        let body = json!({"code": "bad", "error": "bad: already named"});
        assert_eq!(
            http_error_message(Some(&body), "", "fallback"),
            "bad: already named"
        );
    }

    #[test]
    fn http_error_message_falls_back_to_raw_text_capped() {
        let long = "x".repeat(900);
        let msg = http_error_message(None, &long, "fallback");
        assert_eq!(msg.chars().count(), 500);
        assert_eq!(http_error_message(None, "   ", "fallback"), "fallback");
    }

    #[test]
    fn http_error_message_uses_whole_body_when_no_error_field() {
        let body = json!({"code": "", "detail": "d"});
        let msg = http_error_message(Some(&body), "", "fallback");
        assert!(msg.contains("detail"));
    }

    // ── retry policy ─────────────────────────────────────────────────────
    #[test]
    fn retry_on_5xx_but_not_4xx() {
        assert_eq!(retry_decision(Some(500), 0, 2), RetryDecision::Retry);
        assert_eq!(retry_decision(Some(503), 0, 2), RetryDecision::Retry);
        assert_eq!(retry_decision(Some(404), 0, 2), RetryDecision::Stop);
        assert_eq!(retry_decision(Some(401), 0, 2), RetryDecision::Stop);
    }

    #[test]
    fn retry_on_transport_failure_until_exhausted() {
        assert_eq!(retry_decision(None, 0, 2), RetryDecision::Retry);
        assert_eq!(retry_decision(None, 1, 2), RetryDecision::Retry);
        assert_eq!(retry_decision(None, 2, 2), RetryDecision::Stop);
    }

    #[test]
    fn no_retries_when_configured_zero() {
        assert_eq!(retry_decision(Some(500), 0, 0), RetryDecision::Stop);
        assert_eq!(retry_decision(None, 0, 0), RetryDecision::Stop);
    }

    #[test]
    fn retry_delay_is_capped_at_five_seconds() {
        assert_eq!(retry_delay_seconds(0), 1.5);
        assert_eq!(retry_delay_seconds(1), 3.0);
        assert_eq!(retry_delay_seconds(5), 5.0);
    }

    // ── config ───────────────────────────────────────────────────────────
    #[test]
    fn reasoning_effort_accepts_known_values() {
        for v in X_SEARCH_REASONING_EFFORTS {
            let cfg = XSearchConfig {
                reasoning_effort: Some(v.to_string()),
                ..Default::default()
            };
            assert_eq!(cfg.resolved_reasoning_effort().unwrap(), Some(v.to_string()));
        }
    }

    #[test]
    fn reasoning_effort_normalises_case_and_blank() {
        let cfg = XSearchConfig {
            reasoning_effort: Some("  HIGH ".to_string()),
            ..Default::default()
        };
        assert_eq!(cfg.resolved_reasoning_effort().unwrap(), Some("high".to_string()));
        let blank = XSearchConfig {
            reasoning_effort: Some("   ".to_string()),
            ..Default::default()
        };
        assert_eq!(blank.resolved_reasoning_effort().unwrap(), None);
    }

    #[test]
    fn reasoning_effort_rejects_unknown() {
        let cfg = XSearchConfig {
            reasoning_effort: Some("turbo".to_string()),
            ..Default::default()
        };
        let err = cfg.resolved_reasoning_effort().unwrap_err();
        assert!(err.contains("must be one of: low, medium, high, xhigh"));
    }

    #[test]
    fn config_int_applies_floor_and_default() {
        let cfg = XSearchConfig {
            timeout_seconds: Some(5),
            retries: Some(9),
            ..Default::default()
        };
        // floor 30 lifts a too-small timeout.
        assert_eq!(cfg.int("timeout_seconds", DEFAULT_X_SEARCH_TIMEOUT_SECONDS, 30), 30);
        assert_eq!(cfg.int("retries", DEFAULT_X_SEARCH_RETRIES as u64, 0), 9);
        let empty = XSearchConfig::default();
        assert_eq!(
            empty.int("timeout_seconds", DEFAULT_X_SEARCH_TIMEOUT_SECONDS, 30),
            DEFAULT_X_SEARCH_TIMEOUT_SECONDS
        );
    }

    #[test]
    fn config_model_defaults_when_blank() {
        assert_eq!(XSearchConfig::default().model_or_default(), DEFAULT_X_SEARCH_MODEL);
        let blank = XSearchConfig {
            model: Some("   ".to_string()),
            ..Default::default()
        };
        assert_eq!(blank.model_or_default(), DEFAULT_X_SEARCH_MODEL);
        let set = XSearchConfig {
            model: Some(" grok-5 ".to_string()),
            ..Default::default()
        };
        assert_eq!(set.model_or_default(), "grok-5");
    }

    // ── prepare ──────────────────────────────────────────────────────────
    #[test]
    fn prepare_builds_url_and_payload() {
        let call = prepare(
            "  what did people say about X?  ",
            None,
            None,
            "",
            "",
            false,
            false,
            &XSearchConfig::default(),
            Ok(creds()),
        )
        .unwrap();
        assert_eq!(call.url, "https://api.x.ai/v1/responses");
        assert_eq!(call.model, DEFAULT_X_SEARCH_MODEL);
        assert_eq!(call.payload["input"][0]["content"], json!("what did people say about X?"));
        assert_eq!(call.payload["store"], json!(false));
        assert_eq!(call.payload["tools"][0]["type"], json!("x_search"));
        assert!(call.payload.get("reasoning").is_none());
        assert_eq!(call.timeout_seconds, DEFAULT_X_SEARCH_TIMEOUT_SECONDS);
        assert_eq!(call.max_retries, DEFAULT_X_SEARCH_RETRIES);
    }

    #[test]
    fn prepare_adds_reasoning_when_configured() {
        let cfg = XSearchConfig {
            reasoning_effort: Some("high".to_string()),
            ..Default::default()
        };
        let call = prepare("q", None, None, "", "", false, false, &cfg, Ok(creds())).unwrap();
        assert_eq!(call.payload["reasoning"]["effort"], json!("high"));
    }

    #[test]
    fn prepare_refuses_empty_query() {
        let err = prepare(
            "   ",
            None,
            None,
            "",
            "",
            false,
            false,
            &XSearchConfig::default(),
            Ok(creds()),
        )
        .unwrap_err();
        assert_eq!(err, "query is required for x_search");
    }

    #[test]
    fn prepare_surfaces_credential_failure() {
        let err = prepare(
            "q",
            None,
            None,
            "",
            "",
            false,
            false,
            &XSearchConfig::default(),
            Err("no creds".to_string()),
        )
        .unwrap_err();
        assert_eq!(err, "no creds");
    }

    // ── result shape ─────────────────────────────────────────────────────
    #[test]
    fn build_result_flags_degraded_when_filters_yield_no_citations() {
        let payload = json!({"output_text": "answer from training data"});
        let out = build_result(&payload, "grok-4.5", "q", "xai", &s(&["from_date"]));
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["success"], json!(true));
        assert_eq!(v["degraded"], json!(true));
        assert_eq!(
            v["degraded_reason"],
            json!("no citations returned despite filters: from_date")
        );
    }

    #[test]
    fn build_result_not_degraded_without_filters() {
        let payload = json!({"output_text": "plain"});
        let out = build_result(&payload, "m", "q", "xai", &[]);
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["degraded"], json!(false));
        assert!(v["degraded_reason"].is_null());
    }

    #[test]
    fn build_result_not_degraded_when_citations_present() {
        let payload = json!({
            "output_text": "a",
            "citations": [{"url": "https://x.com/1"}]
        });
        let out = build_result(&payload, "m", "q", "xai", &s(&["allowed_x_handles"]));
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["degraded"], json!(false));
        assert_eq!(v["citations"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn error_bodies_carry_provider_and_tool() {
        let v: Value =
            serde_json::from_str(&build_error(&XSearchErrorKind::Http, "boom", 180)).unwrap();
        assert_eq!(v["success"], json!(false));
        assert_eq!(v["provider"], json!("xai"));
        assert_eq!(v["tool"], json!("x_search"));
        assert_eq!(v["error"], json!("boom"));
        assert_eq!(v["error_type"], json!("HTTPError"));
    }

    #[test]
    fn timeout_error_names_the_configured_seconds() {
        let v: Value =
            serde_json::from_str(&build_error(&XSearchErrorKind::Timeout, "", 90)).unwrap();
        assert_eq!(v["error"], json!("xAI x_search timed out after 90 seconds"));
        assert_eq!(v["error_type"], json!("ReadTimeout"));
    }

    // ── validate_args + schema ───────────────────────────────────────────
    #[test]
    fn validate_args_requires_query() {
        assert!(validate_args("  ").is_err());
        assert!(validate_args("real query").is_ok());
    }

    #[test]
    fn definition_shape_matches_python_contract() {
        let d = get_definition();
        assert_eq!(d["function"]["name"], json!("x_search"));
        assert_eq!(d["function"]["parameters"]["required"], json!(["query"]));
        let props = &d["function"]["parameters"]["properties"];
        for key in [
            "query",
            "allowed_x_handles",
            "excluded_x_handles",
            "from_date",
            "to_date",
            "enable_image_understanding",
            "enable_video_understanding",
        ] {
            assert!(props.get(key).is_some(), "missing {key}");
        }
    }

    #[test]
    fn error_body_is_bounded() {
        let msg = bound_error_text(&"x".repeat(5000));
        assert!(msg.ends_with("... [truncated]"));
    }
}
