//! Clarify tool: structured multiple-choice / open-ended questions to the user.
//!
//! 1-to-1 port of `tools/clarify_tool.py`. Schema, validation and result
//! shaping live here; the UI lives in a platform-provided callback (Telegram
//! inline keyboard, CLI prompt, TUI bridge).
//!
//! The `callback` is the interaction seam: the platform injects a function that
//! presents the question(s) and returns the user's raw answer. Callbacks that
//! accept a `questions` batch get the whole list in ONE call (a single
//! rendered form beats N prompts); legacy callbacks are looped one question at
//! a time, and a timeout aborts the loop instead of pestering the user.

use serde_json::{json, Map, Value};

/// The UI always appends an "Other (type your answer)" row, so the model may
/// offer at most `MAX_CHOICES` of its own.
pub const MAX_CHOICES: usize = 4;
/// Independent questions per batch call.
pub const MAX_QUESTIONS: usize = 5;

/// Canonical timeout sentinel. The CLI returns this exact text; the batch loop
/// treats it (like an absent answer) as "the user walked away" and aborts the
/// remaining questions.
pub const TIMEOUT_RESPONSE: &str = "The user did not provide a response within the time limit. \
Use your best judgement to make the choice and proceed.";

/// Applied to the first choice (not per-surface) so every adapter renders it identically.
pub const RECOMMENDED_LABEL: &str = "(Recommended)";

const UNAVAILABLE: &str = "Clarify tool is not available in this execution context.";

/// Mirrors `tool_error()`: `{"error": <bounded message>}`.
fn tool_error(message: &str) -> String {
    json!({ "error": bound_error_text(message) }).to_string()
}

/// Bound an error body destined for model context (`_MAX_TOOL_ERROR_CHARS`).
fn bound_error_text(text: &str) -> String {
    const MAX: usize = 2048;
    const MARKER: &str = "... [truncated]";
    if text.chars().count() <= MAX {
        return text.to_string();
    }
    let head: String = text.chars().take(MAX).collect();
    format!("{}{}", head, MARKER)
}

/// One normalized answer for a question, keyed by its stable wire id.
pub type Answers = std::collections::HashMap<String, String>;

/// Result of invoking a platform callback for one question.
pub type CallbackResult = Result<Option<String>, String>;

/// Platform-injected callback for a SINGLE question.
///
/// `Ok(Some(text))` = the user answered. `Ok(None)` = the user walked away
/// (timeout / dismissed). `Err(msg)` = the platform failed to present the
/// prompt at all.
pub type QuestionCallback<'a> = dyn Fn(&str, Option<&[String]>, bool) -> CallbackResult + 'a;

/// Platform-injected callback for a BATCH of questions in one rendered form.
///
/// Returns `Ok((answers, timed_out))`; `answers` is keyed by the normalized
/// `qid` (`q0`, `q1`, ...). A `None` entry for a qid means that question was
/// skipped (not a timeout), matching the Python contract where a blank answer
/// is a deliberate skip.
pub type QuestionsCallback<'a> = dyn Fn(&[Question]) -> Result<(Answers, bool), String> + 'a;

/// Platform seam: what a UI surface must implement to answer clarify prompts.
///
/// This replaces Python's `inspect.signature` capability sniffing
/// (`_accepts_kwarg`) with two explicit capability methods, which is the same
/// contract stated instead of discovered.
pub trait ClarifyUi: Send + Sync {
    /// Present ONE question. `Ok(Some(text))` = answered, `Ok(None)` = the user
    /// walked away, `Err(msg)` = the prompt could not be delivered.
    fn ask(
        &self,
        question: &str,
        choices: Option<&[String]>,
        multi_select: bool,
    ) -> Result<Option<String>, String>;

    /// True when this surface renders a whole batch in one form (Python's
    /// `questions` kwarg support).
    fn supports_batch(&self) -> bool {
        false
    }

    /// Present a whole batch at once. Only called when `supports_batch()`.
    fn ask_batch(&self, _questions: &[Question]) -> Result<(Answers, bool), String> {
        Ok((Answers::new(), true))
    }
}

/// The callback surface a platform exposes. A platform may support only one of
/// the two shapes; `batch` wins when present.
pub struct ClarifyCallback<'a> {
    pub single: Option<&'a QuestionCallback<'a>>,
    pub batch: Option<&'a QuestionsCallback<'a>>,
}

impl<'a> ClarifyCallback<'a> {
    pub fn single(cb: &'a QuestionCallback<'a>) -> Self {
        Self {
            single: Some(cb),
            batch: None,
        }
    }

    pub fn batch(cb: &'a QuestionsCallback<'a>) -> Self {
        Self {
            single: None,
            batch: Some(cb),
        }
    }

    /// A platform exposing both shapes (the Python `_accepts_kwarg` case).
    pub fn both(single: &'a QuestionCallback<'a>, batch: &'a QuestionsCallback<'a>) -> Self {
        Self {
            single: Some(single),
            batch: Some(batch),
        }
    }

    fn is_empty(&self) -> bool {
        self.single.is_none() && self.batch.is_none()
    }
}

/// One validated question in a batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Question {
    /// Stable wire id (`q<index>`) surfaces key answers by.
    pub qid: String,
    /// The model's own `id` — unvalidated text, only echoed.
    pub id: Option<String>,
    pub question: String,
    /// Decorated choices (first one carries the recommended label).
    pub choices: Option<Vec<String>>,
    /// Bare choices, exactly as the model sent them.
    pub choices_offered: Option<Vec<String>>,
    pub multi_select: bool,
}

// ── Text helpers ───────────────────────────────────────────────────────────

/// Coerce one choice to display text. Handles the dict-shaped choices LLMs
/// sometimes emit: unwrap order `label` > `description` > `text` > `title`
/// (`name`/`value` excluded — raw component enums, not labels). No match → ""
/// and dropped: no choice beats a garbage label.
pub fn flatten_choice(choice: &Value) -> String {
    match choice {
        Value::String(s) => s.trim().to_string(),
        Value::Object(m) => {
            for key in ["label", "description", "text", "title"] {
                if let Some(Value::String(v)) = m.get(key) {
                    let t = v.trim();
                    if !t.is_empty() {
                        return t.to_string();
                    }
                }
            }
            String::new()
        }
        Value::Array(items) => items
            .iter()
            .map(flatten_choice)
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(" ")
            .trim()
            .to_string(),
        Value::Null => String::new(),
        other => other.to_string().trim().to_string(),
    }
}

/// Suffix the first choice (schema says best-first) with `RECOMMENDED_LABEL`;
/// idempotent, and a lone choice is left untouched (nothing to prefer it over).
pub fn mark_recommended(choices: &[String]) -> Vec<String> {
    let first = choices.first().map(|s| s.trim().to_string()).unwrap_or_default();
    if choices.len() < 2 || first != strip_recommended(&first) {
        return choices.to_vec();
    }
    let mut out = Vec::with_capacity(choices.len());
    out.push(format!("{} {}", first, RECOMMENDED_LABEL));
    out.extend(choices[1..].iter().cloned());
    out
}

/// Remove the recommendation label so presentation never leaks into `user_response`.
pub fn strip_recommended(text: &str) -> String {
    let stripped = text.trim();
    let lower = stripped.to_lowercase();
    let label = RECOMMENDED_LABEL.to_lowercase();
    if lower.ends_with(&label) {
        let cut = stripped.len() - RECOMMENDED_LABEL.len();
        return stripped[..cut].trim().to_string();
    }
    stripped.to_string()
}

/// Flatten, drop empties, cap at `MAX_CHOICES`; `None` when nothing survives
/// (open-ended).
pub fn clean_choices(choices: &[Value]) -> Option<Vec<String>> {
    let cleaned: Vec<String> = choices
        .iter()
        .map(flatten_choice)
        .filter(|s| !s.is_empty())
        .collect();
    if cleaned.is_empty() {
        return None;
    }
    Some(cleaned.into_iter().take(MAX_CHOICES).collect())
}

/// Parse a list / JSON array / comma-separated reply into stripped non-empty strings.
pub fn parse_multi_select_response(raw: &str) -> Vec<String> {
    let trimmed = raw.trim();
    let items: Vec<String> = if trimmed.starts_with('[') {
        match serde_json::from_str::<Value>(trimmed) {
            Ok(Value::Array(arr)) => arr.iter().map(|v| value_to_text(v)).collect(),
            _ => trimmed.split(',').map(|s| s.to_string()).collect(),
        }
    } else {
        trimmed.split(',').map(|s| s.to_string()).collect()
    };
    items
        .into_iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn value_to_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// Strip presentation (the label, multi-select JSON) from a locked answer.
fn clean_answer(raw: &str, multi: bool) -> Value {
    if multi {
        json!(parse_multi_select_response(raw)
            .iter()
            .map(|r| strip_recommended(r))
            .collect::<Vec<_>>())
    } else {
        json!(strip_recommended(raw))
    }
}

/// `None` or the sentinel both mean "the user walked away".
fn is_timeout(raw: Option<&str>) -> bool {
    match raw {
        None => true,
        Some(s) => s.trim() == TIMEOUT_RESPONSE,
    }
}

// ── Batch normalization ────────────────────────────────────────────────────

/// Validate the `questions` batch param → `(Some(normalized), None)` or
/// `(None, Some(error))`. An empty list gives `(None, None)`: fall back to the
/// single-question path.
pub fn normalize_questions(questions: &[Value]) -> (Option<Vec<Question>>, Option<String>) {
    if questions.is_empty() {
        return (None, None);
    }
    if questions.len() > MAX_QUESTIONS {
        return (
            None,
            Some(format!("questions supports at most {} items.", MAX_QUESTIONS)),
        );
    }
    let mut normalized = Vec::with_capacity(questions.len());
    for (index, item) in questions.iter().enumerate() {
        // Tolerate bare-string items: LLMs sometimes send ["Q1?", "Q2?"].
        let item = match item {
            Value::String(s) => json!({ "question": s }),
            Value::Object(_) => item.clone(),
            _ => {
                return (
                    None,
                    Some(format!(
                        "questions[{}] must be an object with a 'question'.",
                        index
                    )),
                )
            }
        };
        let text = item
            .get("question")
            .map(value_to_text)
            .unwrap_or_default()
            .trim()
            .to_string();
        if text.is_empty() {
            return (
                None,
                Some(format!("questions[{}].question must be non-empty text.", index)),
            );
        }
        let raw_choices = item.get("choices");
        let cleaned = match raw_choices {
            None | Some(Value::Null) => None,
            Some(Value::Array(arr)) => clean_choices(arr),
            Some(_) => {
                return (
                    None,
                    Some(format!("questions[{}].choices must be a list.", index)),
                )
            }
        };
        let id = item
            .get("id")
            .map(value_to_text)
            .unwrap_or_default()
            .trim()
            .to_string();
        let has_choices = cleaned.is_some();
        normalized.push(Question {
            qid: format!("q{}", index),
            id: if id.is_empty() { None } else { Some(id) },
            question: text,
            choices: cleaned.as_ref().map(|c| mark_recommended(c)),
            choices_offered: cleaned,
            multi_select: item
                .get("multi_select")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
                && has_choices,
        });
    }
    (Some(normalized), None)
}

/// Batch result JSON; unanswered → "". The top-level `timed_out` flag (present
/// only when true) tells the agent whether blanks are deliberate skips or the
/// user walking away; `notice` (surface-supplied, only beside `timed_out`) says
/// WHY the wait ended, so an undeliverable prompt never reads as user inactivity.
pub fn batch_result(
    normalized: &[Question],
    answers: &Answers,
    timed_out: bool,
    notice: Option<&str>,
) -> String {
    let mut responses = Vec::with_capacity(normalized.len());
    for entry in normalized {
        let raw = answers.get(&entry.qid).map(|s| s.as_str());
        let mut row = Map::new();
        if let Some(id) = &entry.id {
            row.insert("id".to_string(), json!(id));
        }
        row.insert("question".to_string(), json!(entry.question));
        row.insert(
            "choices_offered".to_string(),
            match &entry.choices_offered {
                Some(c) => json!(c),
                None => Value::Null,
            },
        );
        row.insert(
            "user_response".to_string(),
            match raw {
                Some(r) if !r.is_empty() => clean_answer(r, entry.multi_select),
                _ => json!(""),
            },
        );
        responses.push(Value::Object(row));
    }
    let mut result = Map::new();
    result.insert("responses".to_string(), json!(responses));
    if timed_out {
        result.insert("timed_out".to_string(), json!(true));
        if let Some(n) = notice {
            result.insert("notice".to_string(), json!(n));
        }
    }
    Value::Object(result).to_string()
}

/// Dispatch a validated batch. Batch-capable callbacks get the whole list once;
/// any other falsy/unparseable reply is a cancel-all (mirrors the
/// single-question skip). Legacy callbacks are looped per question: an empty
/// answer is a skip, a timeout means the user walked away so the loop aborts
/// instead of pestering them; earlier answers are kept.
fn run_batch(normalized: &[Question], cb: &ClarifyCallback<'_>, question: &str) -> String {
    if let Some(batch) = cb.batch {
        match batch(normalized) {
            Ok((answers, timed_out)) => return batch_result(normalized, &answers, timed_out, None),
            Err(e) => return tool_error(&format!("Failed to get user input: {}", e)),
        }
    }
    let Some(single) = cb.single else {
        return tool_error(UNAVAILABLE);
    };
    let mut answers: Answers = Answers::new();
    let mut timed_out = false;
    for entry in normalized {
        match single(
            &entry.question,
            entry.choices.as_deref(),
            entry.multi_select,
        ) {
            Ok(Some(raw)) if !is_timeout(Some(&raw)) => {
                answers.insert(entry.qid.clone(), raw);
            }
            Ok(_) => {
                timed_out = true;
                break;
            }
            Err(e) => return tool_error(&format!("Failed to get user input: {}", e)),
        }
    }
    let _ = question;
    batch_result(normalized, &answers, timed_out, None)
}

// ── Tool entry point ───────────────────────────────────────────────────────

/// Ask one question (`question`/`choices`/`multi_select`) or a batch
/// (`questions` wins when non-empty). Returns the result JSON string.
pub fn clarify_tool(
    question: &str,
    choices: Option<&[Value]>,
    multi_select: bool,
    questions: Option<&[Value]>,
    cb: &ClarifyCallback<'_>,
) -> String {
    if let Some(qs) = questions {
        let (normalized, error) = normalize_questions(qs);
        if let Some(err) = error {
            return tool_error(&err);
        }
        if let Some(list) = normalized {
            if cb.is_empty() {
                return tool_error(UNAVAILABLE);
            }
            return run_batch(&list, cb, question.trim());
        }
        // Empty questions array → fall through to the single-question path.
    }
    if question.trim().is_empty() {
        return tool_error(
            "No question provided. Pass questions=[{question: '...', \
             choices?: [...], multi_select?: bool}, ...] — a single question \
             is a one-entry array.",
        );
    }
    let question = question.trim();

    // `choices` must be a list when present; `None` means open-ended.
    let cleaned: Option<Vec<String>> = match choices {
        None => None,
        Some(c) => clean_choices(c),
    };

    if cb.is_empty() {
        return tool_error(UNAVAILABLE);
    }
    // The bare list goes back to the agent; the "(Recommended)" label is
    // presentation only.
    let shown = cleaned.as_ref().map(|c| mark_recommended(c));
    let raw = if let Some(single) = cb.single {
        match single(question, shown.as_deref(), multi_select) {
            Ok(r) => r,
            Err(e) => return tool_error(&format!("Failed to get user input: {}", e)),
        }
    } else if let Some(batch) = cb.batch {
        // A batch-only platform asked for a single question: wrap it as a
        // one-entry batch, matching the Python single-shape contract.
        let normal = vec![Question {
            qid: "q0".to_string(),
            id: None,
            question: question.to_string(),
            choices: shown.clone(),
            choices_offered: cleaned.clone(),
            multi_select: multi_select && cleaned.is_some(),
        }];
        match batch(&normal) {
            Ok((answers, _timed_out)) => answers.get("q0").cloned(),
            Err(e) => return tool_error(&format!("Failed to get user input: {}", e)),
        }
    } else {
        None
    };

    let is_multi = multi_select && cleaned.is_some();
    let user_response = match &raw {
        Some(r) if !r.is_empty() => clean_answer(r, is_multi),
        _ => json!(""),
    };
    json!({
        "question": question,
        "choices_offered": match &cleaned { Some(c) => json!(c), None => Value::Null },
        "user_response": user_response,
    })
    .to_string()
}

/// Argument-shape validation for the flat tool call; `None` when acceptable.
/// Exposed so the dispatcher can reject a malformed call before touching the UI.
pub fn validate_args(questions: Option<&Value>, question: Option<&str>) -> Option<String> {
    if let Some(Value::Array(arr)) = questions {
        if !arr.is_empty() {
            let (_, err) = normalize_questions(arr);
            return err;
        }
    } else if let Some(other) = questions {
        if !other.is_null() {
            return Some("questions must be an array of question objects.".to_string());
        }
    }
    if questions.is_none() && question.map(|q| q.trim().is_empty()).unwrap_or(true) {
        return Some(
            "No question provided. Pass questions=[{question: '...', choices?: [...], \
             multi_select?: bool}, ...] — a single question is a one-entry array."
                .to_string(),
        );
    }
    None
}

/// Tool definition for `clarify` (mirrors the Python `CLARIFY_SCHEMA`).
pub fn get_definition() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": "clarify",
            "description": format!(
                "Ask the user one or more questions when you need a decision, clarification, or \
                 feedback before proceeding. Pass every question in `questions` (1-{} entries) — a \
                 single question is a one-entry array, and several INDEPENDENT questions belong in \
                 ONE call (one form beats a chain of clarify calls; if one answer would change \
                 another question, ask separately). Per question: single-select (up to {} choices \
                 — put your recommended option FIRST, the UI marks it '{}' and auto-appends an \
                 'Other' free-text row), multi-select (multi_select=true), or open-ended (omit \
                 choices). Options go ONLY in `choices`, never enumerated inside the question text \
                 (choices render as pickable rows; options written into the question are dead prose \
                 the user can't click). Result: {{responses: [...]}} in question order (plus \
                 timed_out=true, and a notice saying why, if the user stopped part-way or the prompt \
                 could not be delivered). Prefer deciding low-stakes questions yourself; don't use \
                 this for dangerous-command confirmation (the terminal tool handles that).",
                MAX_QUESTIONS, MAX_CHOICES, RECOMMENDED_LABEL
            ),
            "parameters": {
                "type": "object",
                "properties": {
                    "questions": {
                        "type": "array",
                        "minItems": 1,
                        "maxItems": MAX_QUESTIONS,
                        "description": "The question(s). Each: question text (options excluded), optional choices (recommended first; omit for free-text), optional multi_select. Responses come back in question order with the question text echoed.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "question": { "type": "string" },
                                "choices": {
                                    "type": "array",
                                    "items": { "type": "string" },
                                    "maxItems": MAX_CHOICES
                                },
                                "multi_select": { "type": "boolean" }
                            },
                            "required": ["question"]
                        }
                    }
                },
                "required": ["questions"]
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // ── Helpers ──────────────────────────────────────────────────────────
    fn qv(question: &str) -> Value {
        json!({ "question": question })
    }

    fn qv_choices(question: &str, choices: &[&str]) -> Value {
        json!({ "question": question, "choices": choices })
    }

    /// Single-question callback that answers from a scripted queue.
    fn scripted_single(answers: Vec<Option<&'static str>>) -> impl Fn(&str, Option<&[String]>, bool) -> CallbackResult {
        let queue: Mutex<Vec<Option<&'static str>>> = Mutex::new(answers);
        move |_q, _c, _m| {
            let mut q = queue.lock().unwrap();
            if q.is_empty() {
                return Ok(None);
            }
            Ok(q.remove(0).map(|s| s.to_string()))
        }
    }

    // ── flatten_choice ───────────────────────────────────────────────────
    #[test]
    fn flatten_choice_string_trims() {
        assert_eq!(flatten_choice(&json!("  alpha  ")), "alpha");
    }

    #[test]
    fn flatten_choice_unwraps_dict_in_priority_order() {
        // label wins over description/text/title.
        assert_eq!(
            flatten_choice(&json!({"title":"T","text":"X","description":"D","label":"L"})),
            "L"
        );
        assert_eq!(flatten_choice(&json!({"description":"D","text":"X"})), "D");
        assert_eq!(flatten_choice(&json!({"text":"X","title":"T"})), "X");
        assert_eq!(flatten_choice(&json!({"title":"T"})), "T");
    }

    #[test]
    fn flatten_choice_excludes_name_and_value_keys() {
        // name/value are raw component enums, not labels -> dropped.
        assert_eq!(flatten_choice(&json!({"name":"opt_a","value":"a"})), "");
    }

    #[test]
    fn flatten_choice_no_label_drops_the_choice() {
        assert_eq!(flatten_choice(&json!({"unknown":"x"})), "");
        assert_eq!(flatten_choice(&Value::Null), "");
        assert_eq!(flatten_choice(&json!({"label":"   "})), "");
    }

    #[test]
    fn flatten_choice_joins_nested_list() {
        assert_eq!(flatten_choice(&json!(["a", "b"])), "a b");
    }

    // ── mark_recommended / strip_recommended ─────────────────────────────
    #[test]
    fn mark_recommended_suffixes_first_of_several() {
        let out = mark_recommended(&["best".to_string(), "second".to_string()]);
        assert_eq!(out[0], format!("best {}", RECOMMENDED_LABEL));
        assert_eq!(out[1], "second");
    }

    #[test]
    fn mark_recommended_leaves_lone_choice_untouched() {
        let out = mark_recommended(&["only".to_string()]);
        assert_eq!(out, vec!["only".to_string()]);
    }

    #[test]
    fn mark_recommended_is_idempotent() {
        let once = mark_recommended(&["best".to_string(), "second".to_string()]);
        let twice = mark_recommended(&once);
        assert_eq!(once, twice);
    }

    #[test]
    fn strip_recommended_removes_label_case_insensitively() {
        assert_eq!(
            strip_recommended(&format!("best {}", RECOMMENDED_LABEL)),
            "best"
        );
        assert_eq!(strip_recommended("best (recommended)"), "best");
        assert_eq!(strip_recommended("plain"), "plain");
    }

    // ── clean_choices ────────────────────────────────────────────────────
    #[test]
    fn clean_choices_caps_at_max_and_drops_empties() {
        let raw = vec![
            json!("a"),
            json!(""),
            json!("b"),
            json!({"nope": 1}),
            json!("c"),
            json!("d"),
            json!("e"),
        ];
        let out = clean_choices(&raw).unwrap();
        assert_eq!(out.len(), MAX_CHOICES);
        assert_eq!(out, vec!["a", "b", "c", "d"]);
    }

    #[test]
    fn clean_choices_none_when_nothing_survives() {
        assert!(clean_choices(&[json!(""), json!({"x": 1})]).is_none());
    }

    // ── parse_multi_select_response ──────────────────────────────────────
    #[test]
    fn parse_multi_select_accepts_json_array() {
        assert_eq!(
            parse_multi_select_response(r#"["one","two"]"#),
            vec!["one".to_string(), "two".to_string()]
        );
    }

    #[test]
    fn parse_multi_select_accepts_comma_separated() {
        assert_eq!(
            parse_multi_select_response("one, two ,, three"),
            vec!["one".to_string(), "two".to_string(), "three".to_string()]
        );
    }

    // ── normalize_questions ──────────────────────────────────────────────
    #[test]
    fn normalize_questions_empty_list_is_no_error_no_batch() {
        let (norm, err) = normalize_questions(&[]);
        assert!(norm.is_none());
        assert!(err.is_none());
    }

    #[test]
    fn normalize_questions_caps_batch_size() {
        let many: Vec<Value> = (0..=MAX_QUESTIONS).map(|i| qv(&format!("q{}", i))).collect();
        let (norm, err) = normalize_questions(&many);
        assert!(norm.is_none());
        assert!(err.unwrap().contains("at most 5"));
    }

    #[test]
    fn normalize_questions_tolerates_bare_strings() {
        let (norm, err) = normalize_questions(&[json!("Q1?"), json!("Q2?")]);
        assert!(err.is_none());
        let n = norm.unwrap();
        assert_eq!(n.len(), 2);
        assert_eq!(n[0].qid, "q0");
        assert_eq!(n[0].question, "Q1?");
        assert!(n[0].choices.is_none());
    }

    #[test]
    fn normalize_questions_rejects_non_object_entries() {
        let (_, err) = normalize_questions(&[json!(42)]);
        assert!(err.unwrap().contains("questions[0] must be an object"));
    }

    #[test]
    fn normalize_questions_rejects_empty_question_text() {
        let (_, err) = normalize_questions(&[json!({"question": "   "})]);
        assert!(err.unwrap().contains("must be non-empty text"));
    }

    #[test]
    fn normalize_questions_rejects_non_list_choices() {
        let (_, err) = normalize_questions(&[json!({"question": "Q", "choices": "nope"})]);
        assert!(err.unwrap().contains("choices must be a list"));
    }

    #[test]
    fn normalize_questions_decorates_but_keeps_bare_choices() {
        let (norm, _) = normalize_questions(&[qv_choices("Pick", &["best", "other"])]);
        let q = &norm.unwrap()[0];
        assert_eq!(
            q.choices.as_ref().unwrap()[0],
            format!("best {}", RECOMMENDED_LABEL)
        );
        assert_eq!(q.choices_offered.as_ref().unwrap()[0], "best");
    }

    #[test]
    fn normalize_questions_multi_select_requires_choices() {
        let (norm, _) = normalize_questions(&[json!({"question": "Q", "multi_select": true})]);
        assert!(!norm.unwrap()[0].multi_select);
    }

    #[test]
    fn normalize_questions_echoes_optional_id() {
        let (norm, _) = normalize_questions(&[json!({"question": "Q", "id": " phase-2 "})]);
        assert_eq!(norm.unwrap()[0].id, Some("phase-2".to_string()));
    }

    // ── batch_result ─────────────────────────────────────────────────────
    #[test]
    fn batch_result_unanswered_is_empty_string() {
        let (norm, _) = normalize_questions(&[qv("Q1?"), qv("Q2?")]);
        let n = norm.unwrap();
        let mut answers = Answers::new();
        answers.insert("q0".to_string(), "yes".to_string());
        let out = batch_result(&n, &answers, false, None);
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["responses"][0]["user_response"], json!("yes"));
        assert_eq!(v["responses"][1]["user_response"], json!(""));
        assert!(v.get("timed_out").is_none());
    }

    #[test]
    fn batch_result_marks_timeout_with_notice() {
        let (norm, _) = normalize_questions(&[qv("Q1?")]);
        let out = batch_result(&norm.unwrap(), &Answers::new(), true, Some("delivery failed"));
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["timed_out"], json!(true));
        assert_eq!(v["notice"], json!("delivery failed"));
    }

    #[test]
    fn batch_result_echoes_question_and_offered_choices() {
        let (norm, _) = normalize_questions(&[json!({"question": "Pick", "choices": ["a"]})]);
        let out = batch_result(&norm.unwrap(), &Answers::new(), false, None);
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["responses"][0]["question"], json!("Pick"));
        assert_eq!(v["responses"][0]["choices_offered"], json!(["a"]));
    }

    // ── Single-question path ─────────────────────────────────────────────
    #[test]
    fn single_question_strips_recommended_from_answer() {
        let cb = scripted_single(vec![Some("Alpha (Recommended)")]);
        let out = clarify_tool(
            "Which?",
            Some(&[json!("Alpha"), json!("Beta")]),
            false,
            None,
            &ClarifyCallback::single(&cb),
        );
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["user_response"], json!("Alpha"));
        assert_eq!(v["choices_offered"], json!(["Alpha", "Beta"]));
    }

    #[test]
    fn single_question_multi_select_returns_list() {
        let cb = scripted_single(vec![Some(r#"["Alpha (Recommended)","Beta"]"#)]);
        let out = clarify_tool(
            "Which?",
            Some(&[json!("Alpha"), json!("Beta")]),
            true,
            None,
            &ClarifyCallback::single(&cb),
        );
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["user_response"], json!(["Alpha", "Beta"]));
    }

    #[test]
    fn single_question_open_ended_has_null_choices() {
        let cb = scripted_single(vec![Some("my free text")]);
        let out = clarify_tool("Why?", None, false, None, &ClarifyCallback::single(&cb));
        let v: Value = serde_json::from_str(&out).unwrap();
        assert!(v["choices_offered"].is_null());
        assert_eq!(v["user_response"], json!("my free text"));
    }

    #[test]
    fn single_question_timeout_sentinel_is_returned_as_the_response() {
        // Fidelity note: Python's single-question path never consults
        // `_is_timeout` — the sentinel text IS the answer, so the model reads
        // "use your best judgement". Only the batch loop treats it as a
        // walk-away. This asserts the faithful behaviour, not a nicer one.
        let cb = scripted_single(vec![Some(TIMEOUT_RESPONSE)]);
        let out = clarify_tool("Q?", None, false, None, &ClarifyCallback::single(&cb));
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["user_response"], json!(TIMEOUT_RESPONSE));
    }

    #[test]
    fn single_question_without_callback_is_unavailable() {
        let none: Option<&QuestionCallback<'_>> = None;
        let cb = ClarifyCallback {
            single: none,
            batch: None,
        };
        let out = clarify_tool("Q?", None, false, None, &cb);
        let v: Value = serde_json::from_str(&out).unwrap();
        assert!(v["error"].as_str().unwrap().contains("not available"));
    }

    #[test]
    fn empty_question_is_refused() {
        let cb = scripted_single(vec![Some("x")]);
        let out = clarify_tool("   ", None, false, None, &ClarifyCallback::single(&cb));
        let v: Value = serde_json::from_str(&out).unwrap();
        assert!(v["error"]
            .as_str()
            .unwrap()
            .contains("No question provided"));
    }

    #[test]
    fn callback_error_is_surfaced() {
        let cb = |_q: &str, _c: Option<&[String]>, _m: bool| -> CallbackResult {
            Err("transport down".to_string())
        };
        let out = clarify_tool("Q?", None, false, None, &ClarifyCallback::single(&cb));
        let v: Value = serde_json::from_str(&out).unwrap();
        assert!(v["error"]
            .as_str()
            .unwrap()
            .contains("Failed to get user input"));
    }

    // ── Batch path ───────────────────────────────────────────────────────
    #[test]
    fn batch_callback_receives_all_questions_at_once() {
        let cb = |qs: &[Question]| -> Result<(Answers, bool), String> {
            assert_eq!(qs.len(), 3);
            let mut a = Answers::new();
            a.insert("q0".to_string(), "one".to_string());
            a.insert("q2".to_string(), "three".to_string());
            Ok((a, false))
        };
        let questions = vec![qv("Q1?"), qv("Q2?"), qv("Q3?")];
        let out = clarify_tool("", None, false, Some(&questions), &ClarifyCallback::batch(&cb));
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["responses"][0]["user_response"], json!("one"));
        assert_eq!(v["responses"][1]["user_response"], json!(""));
        assert_eq!(v["responses"][2]["user_response"], json!("three"));
    }

    #[test]
    fn batch_timeout_flag_propagates() {
        let cb = |_qs: &[Question]| -> Result<(Answers, bool), String> { Ok((Answers::new(), true)) };
        let questions = vec![qv("Q1?")];
        let out = clarify_tool("", None, false, Some(&questions), &ClarifyCallback::batch(&cb));
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["timed_out"], json!(true));
    }

    #[test]
    fn legacy_callback_loops_and_aborts_on_timeout() {
        // First answers, second walks away -> third is never asked.
        let cb = scripted_single(vec![Some("first-answer"), None, Some("never")]);
        let questions = vec![qv("Q1?"), qv("Q2?"), qv("Q3?")];
        let out = clarify_tool("", None, false, Some(&questions), &ClarifyCallback::single(&cb));
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["responses"][0]["user_response"], json!("first-answer"));
        assert_eq!(v["responses"][1]["user_response"], json!(""));
        assert_eq!(v["timed_out"], json!(true));
    }

    #[test]
    fn legacy_callback_keeps_earlier_answers_on_walkaway() {
        let cb = scripted_single(vec![Some("kept"), None]);
        let questions = vec![qv("Q1?"), qv("Q2?")];
        let out = clarify_tool("", None, false, Some(&questions), &ClarifyCallback::single(&cb));
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["responses"][0]["user_response"], json!("kept"));
    }

    #[test]
    fn empty_questions_array_falls_back_to_single_path() {
        let cb = scripted_single(vec![Some("single-answer")]);
        let empty: Vec<Value> = vec![];
        let out = clarify_tool(
            "Standalone?",
            None,
            false,
            Some(&empty),
            &ClarifyCallback::single(&cb),
        );
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["question"], json!("Standalone?"));
        assert_eq!(v["user_response"], json!("single-answer"));
    }

    #[test]
    fn batch_shape_error_is_returned_before_ui() {
        let cb = |_qs: &[Question]| -> Result<(Answers, bool), String> {
            panic!("callback must not run on a shape error")
        };
        let bad = vec![json!({"question": ""})];
        let out = clarify_tool("", None, false, Some(&bad), &ClarifyCallback::batch(&cb));
        let v: Value = serde_json::from_str(&out).unwrap();
        assert!(v["error"].as_str().unwrap().contains("must be non-empty"));
    }

    #[test]
    fn batch_only_platform_can_answer_a_single_question() {
        let cb = |qs: &[Question]| -> Result<(Answers, bool), String> {
            assert_eq!(qs.len(), 1);
            let mut a = Answers::new();
            a.insert("q0".to_string(), "from-batch".to_string());
            Ok((a, false))
        };
        let out = clarify_tool("Solo?", None, false, None, &ClarifyCallback::batch(&cb));
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["user_response"], json!("from-batch"));
    }

    // ── validate_args ────────────────────────────────────────────────────
    #[test]
    fn validate_args_accepts_well_formed_batch() {
        let qs = json!([{"question": "Q?", "choices": ["a", "b"]}]);
        assert!(validate_args(Some(&qs), None).is_none());
    }

    #[test]
    fn validate_args_rejects_non_array_questions() {
        let qs = json!("not an array");
        assert!(validate_args(Some(&qs), None)
            .unwrap()
            .contains("must be an array"));
    }

    #[test]
    fn validate_args_requires_something_to_ask() {
        assert!(validate_args(None, None).is_some());
        assert!(validate_args(None, Some("  ")).is_some());
        assert!(validate_args(None, Some("real question")).is_none());
    }

    #[test]
    fn error_body_is_bounded() {
        let huge = "x".repeat(5000);
        let out = tool_error(&huge);
        let v: Value = serde_json::from_str(&out).unwrap();
        let msg = v["error"].as_str().unwrap();
        assert!(msg.ends_with("... [truncated]"));
        assert_eq!(msg.chars().count(), 2048 + "... [truncated]".chars().count());
    }

    #[test]
    fn definition_shape_matches_python_contract() {
        let d = get_definition();
        assert_eq!(d["function"]["name"], json!("clarify"));
        assert_eq!(
            d["function"]["parameters"]["required"],
            json!(["questions"])
        );
        assert_eq!(
            d["function"]["parameters"]["properties"]["questions"]["maxItems"],
            json!(MAX_QUESTIONS)
        );
        assert_eq!(
            d["function"]["parameters"]["properties"]["questions"]["items"]["properties"]["choices"]
                ["maxItems"],
            json!(MAX_CHOICES)
        );
    }
}
