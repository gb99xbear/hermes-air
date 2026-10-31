// Memory Tool - 1-to-1 parity with the Python memory tool + backing store.
//
// Persistent curated memory: MEMORY.md (agent notes) and USER.md (user profile).
// Both files enter the system prompt as a frozen snapshot at session start;
// mid-session writes hit disk but never mutate the prompt. A single `memory`
// tool fronts the store: add / replace / remove (single-op shape) or an
// `operations` array (batch shape, atomic against the FINAL char budget).
//
// Entries are joined by `ENTRY_DELIMITER` and budgets are measured in characters
// (model-independent). File layout stays byte-interchangeable with the Python
// format so either implementation can read the other's files.

use serde_json::{json, Map, Value};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use parking_lot::Mutex;
use uuid::Uuid;

/// Separator joining individual memory entries in the on-disk file.
pub const ENTRY_DELIMITER: &str = "\n§\n";

/// Default character budget for the agent-notes store (MEMORY.md).
pub const DEFAULT_MEMORY_CHAR_LIMIT: usize = 2200;
/// Default character budget for the user-profile store (USER.md).
pub const DEFAULT_USER_CHAR_LIMIT: usize = 1375;

/// Failed consolidation attempts allowed per turn before a terminal result is
/// returned (so a fragile replace/add cannot loop the turn to exhaustion).
const MAX_CONSOLIDATION_FAILURES_PER_TURN: usize = 3;

/// System-prompt block header per target.
pub fn block_header(target: &str) -> &'static str {
    if target == "user" {
        "USER PROFILE (who the user is)"
    } else {
        "MEMORY (your personal notes)"
    }
}

/// Resolve the default base directory for memory files (per-user, never a
/// hardcoded personal path): HERMES_HOME/memories, else HOME/.hermes/memories.
pub fn default_base_dir() -> PathBuf {
    if let Ok(home) = std::env::var("HERMES_HOME") {
        return PathBuf::from(home).join("memories");
    }
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home).join(".hermes").join("memories");
    }
    PathBuf::from(".hermes").join("memories")
}

fn fmt_thousands(n: usize) -> String {
    let s = n.to_string();
    let len = s.chars().count();
    let mut out = String::with_capacity(len + len / 3);
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (len - i) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// `{"success": false, "error": ...}` plus any extra fields (mirrors `_error`).
fn error_value(message: &str) -> Value {
    json!({ "success": false, "error": message })
}

fn error_with(message: &str, extra: Value) -> Value {
    let mut obj = match error_value(message) {
        Value::Object(m) => m,
        other => return other,
    };
    if let Value::Object(ex) = extra {
        for (k, v) in ex {
            obj.insert(k, v);
        }
    }
    Value::Object(obj)
}

/// Locate the entry matching `old_text`. A whole-entry EXACT match takes
/// absolute priority; substring matches are considered only when no entry
/// equals `old_text`. Returns `(index, ambiguous)`.
fn find_unique_match(entries: &[String], old_text: &str) -> (Option<usize>, bool) {
    let exact: Vec<usize> = entries
        .iter()
        .enumerate()
        .filter(|(_, e)| e.as_str() == old_text)
        .map(|(i, _)| i)
        .collect();
    let matches: Vec<usize> = if !exact.is_empty() {
        exact
    } else {
        entries
            .iter()
            .enumerate()
            .filter(|(_, e)| e.contains(old_text))
            .map(|(i, _)| i)
            .collect()
    };
    if matches.is_empty() {
        return (None, false);
    }
    let distinct: HashSet<&String> = matches.iter().map(|&i| &entries[i]).collect();
    if distinct.len() > 1 {
        return (None, true);
    }
    (Some(matches[0]), false)
}

/// Stripped, non-empty entries; splits on the FULL delimiter so a bare "§" survives.
fn parse_entries(raw: &str) -> Vec<String> {
    raw.split(ENTRY_DELIMITER)
        .map(|x| x.trim().to_string())
        .filter(|e| !e.is_empty())
        .collect()
}

/// Strip a leading UTF-8 BOM (Notepad-edited files); byte-identical otherwise.
fn strip_bom(bytes: Vec<u8>) -> Vec<u8> {
    if bytes.len() >= 3 && bytes[0] == 0xEF && bytes[1] == 0xBB && bytes[2] == 0xBF {
        bytes[3..].to_vec()
    } else {
        bytes
    }
}

/// Outcome of a mutation closure: either a directly-returned response, or a
/// committed `(new_entries, message, extra)` triple to be persisted.
enum MutateOutcome {
    Direct(Value),
    Applied(Vec<String>, String, Option<Value>),
}

/// Bounded curated memory backed by MEMORY.md / USER.md.
pub struct MemoryStore {
    memory_entries: Mutex<Vec<String>>,
    user_entries: Mutex<Vec<String>>,
    memory_char_limit: usize,
    user_char_limit: usize,
    memory_enabled: bool,
    user_profile_enabled: bool,
    base_dir: PathBuf,
    consolidation_failures: AtomicUsize,
    write_lock: Mutex<()>,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self::with_options(
            DEFAULT_MEMORY_CHAR_LIMIT,
            DEFAULT_USER_CHAR_LIMIT,
            true,
            true,
            default_base_dir(),
        )
    }

    pub fn with_base_dir(base_dir: PathBuf) -> Self {
        Self::with_options(
            DEFAULT_MEMORY_CHAR_LIMIT,
            DEFAULT_USER_CHAR_LIMIT,
            true,
            true,
            base_dir,
        )
    }

    pub fn with_options(
        memory_char_limit: usize,
        user_char_limit: usize,
        memory_enabled: bool,
        user_profile_enabled: bool,
        base_dir: PathBuf,
    ) -> Self {
        let store = Self {
            memory_entries: Mutex::new(Vec::new()),
            user_entries: Mutex::new(Vec::new()),
            memory_char_limit,
            user_char_limit,
            memory_enabled,
            user_profile_enabled,
            base_dir,
            consolidation_failures: AtomicUsize::new(0),
            write_lock: Mutex::new(()),
        };
        store.load_from_disk();
        store
    }

    pub fn base_dir(&self) -> &Path {
        &self.base_dir
    }

    pub fn target_enabled(&self, target: &str) -> bool {
        if target == "user" {
            self.user_profile_enabled
        } else {
            self.memory_enabled
        }
    }

    fn path_for(&self, target: &str) -> PathBuf {
        self.base_dir
            .join(if target == "user" { "USER.md" } else { "MEMORY.md" })
    }

    pub fn entries_for(&self, target: &str) -> Vec<String> {
        if target == "user" {
            self.user_entries.lock().clone()
        } else {
            self.memory_entries.lock().clone()
        }
    }

    fn set_entries(&self, target: &str, entries: Vec<String>) {
        if target == "user" {
            *self.user_entries.lock() = entries;
        } else {
            *self.memory_entries.lock() = entries;
        }
    }

    fn char_count(&self, target: &str) -> usize {
        self.char_count_entries(&self.entries_for(target))
    }

    fn char_count_entries(&self, entries: &[String]) -> usize {
        entries.join(ENTRY_DELIMITER).chars().count()
    }

    fn char_limit(&self, target: &str) -> usize {
        if target == "user" {
            self.user_char_limit
        } else {
            self.memory_char_limit
        }
    }

    fn usage(&self, target: &str) -> String {
        format!(
            "{}/{}",
            fmt_thousands(self.char_count(target)),
            fmt_thousands(self.char_limit(target))
        )
    }

    fn usage_for(&self, target: &str, entries: &[String]) -> String {
        format!(
            "{}/{}",
            fmt_thousands(self.char_count_entries(entries)),
            fmt_thousands(self.char_limit(target))
        )
    }

    fn usage_pct(&self, target: &str, current: usize) -> String {
        let limit = self.char_limit(target);
        let pct = if limit > 0 {
            std::cmp::min(100, ((current as f64 / limit as f64) * 100.0) as usize)
        } else {
            0
        };
        format!(
            "{}% — {}/{} chars",
            pct,
            fmt_thousands(current),
            fmt_thousands(limit)
        )
    }

    /// Count a consolidation failure; under the per-turn cap returns the
    /// response unchanged, past it a terminal result so the model stops looping.
    fn consolidation_failure(&self, response: Value) -> Value {
        let count = self.consolidation_failures.fetch_add(1, Ordering::SeqCst) + 1;
        if count <= MAX_CONSOLIDATION_FAILURES_PER_TURN {
            return response;
        }
        json!({
            "success": false,
            "done": true,
            "error": format!(
                "Memory consolidation failed {} times this turn. Stop retrying memory calls — leave memory unchanged for now and continue with your reply to the user. The fact can be saved in a later turn.",
                count
            )
        })
    }

    fn failure_with_entries(&self, target: &str, entries: &[String], message: &str) -> Value {
        self.consolidation_failure(error_with(
            message,
            json!({
                "current_entries": entries,
                "usage": self.usage_for(target, entries),
            }),
        ))
    }

    fn batch_failure(&self, target: &str, entries: &[String], message: &str) -> Value {
        self.consolidation_failure(error_with(
            &format!("{} No operations were applied (batch is all-or-nothing).", message),
            json!({ "usage": self.usage_for(target, entries) }),
        ))
    }

    fn success_response(
        &self,
        target: &str,
        message: Option<&str>,
        extra: Option<Value>,
    ) -> Value {
        self.consolidation_failures.store(0, Ordering::SeqCst);
        let mut obj = Map::new();
        obj.insert("success".to_string(), json!(true));
        obj.insert("done".to_string(), json!(true));
        obj.insert("target".to_string(), json!(target));
        obj.insert(
            "usage".to_string(),
            json!(self.usage_pct(target, self.char_count(target))),
        );
        obj.insert(
            "entry_count".to_string(),
            json!(self.entries_for(target).len()),
        );
        if let Some(m) = message {
            obj.insert("message".to_string(), json!(m));
        }
        if let Some(Value::Object(ex)) = extra {
            for (k, v) in ex {
                obj.insert(k, v);
            }
        }
        obj.insert(
            "note".to_string(),
            json!("Write saved. This update is complete — do not repeat it."),
        );
        Value::Object(obj)
    }

    /// Load MEMORY.md / USER.md into live state (order-preserving dedupe).
    fn load_from_disk(&self) {
        for target in ["memory", "user"] {
            let path = self.path_for(target);
            let (raw, _ok) = read_raw_checked(&path);
            let mut seen = HashSet::new();
            let entries: Vec<String> = parse_entries(&raw)
                .into_iter()
                .filter(|e| seen.insert(e.clone()))
                .collect();
            self.set_entries(target, entries);
        }
    }

    /// Lock, re-read from disk, run `mutate(entries, limit)`, persist.
    fn mutate<F>(&self, target: &str, skip_drift: bool, mutate_fn: F) -> Value
    where
        F: FnOnce(&[String], usize) -> MutateOutcome,
    {
        let _guard = self.write_lock.lock();
        let path = self.path_for(target);
        let (raw, read_ok) = read_raw_checked(&path);
        if !read_ok {
            return read_failed_error(&path);
        }

        let bak = if skip_drift {
            None
        } else {
            self.detect_external_drift(target, &raw)
        };

        let mut seen = HashSet::new();
        let entries: Vec<String> = parse_entries(&raw)
            .into_iter()
            .filter(|e| seen.insert(e.clone()))
            .collect();
        self.set_entries(target, entries.clone());

        if let Some(bak_path) = bak {
            return drift_error(&path, &bak_path);
        }

        match mutate_fn(&entries, self.char_limit(target)) {
            MutateOutcome::Direct(v) => v,
            MutateOutcome::Applied(new_entries, message, extra) => {
                self.set_entries(target, new_entries.clone());
                if let Err(e) = write_file(&path, &new_entries) {
                    return error_value(&format!("Failed to write memory file {}: {}", path.display(), e));
                }
                self.success_response(target, Some(&message), extra)
            }
        }
    }

    /// `.bak.<ts>` snapshot path if `raw` shows external drift, else None.
    fn detect_external_drift(&self, target: &str, raw: &str) -> Option<String> {
        let parsed = parse_entries(raw);
        let limit = self.char_limit(target);
        let max_len = parsed.iter().map(|e| e.chars().count()).max().unwrap_or(0);
        let trimmed = raw.trim();
        if trimmed.is_empty()
            || (trimmed == parsed.join(ENTRY_DELIMITER) && max_len <= limit)
        {
            return None;
        }
        let path = self.path_for(target);
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let bak_path = format!("{}.bak.{}", path.display(), ts);
        if std::fs::write(&bak_path, raw).is_err() {
            return Some(format!("{} (BACKUP FAILED — file unchanged on disk)", bak_path));
        }
        Some(bak_path)
    }

    /// Append a new entry; error if it would exceed the char limit.
    pub fn add(&self, target: &str, content: &str) -> Value {
        let content = content.trim();
        if content.is_empty() {
            return error_value("Content cannot be empty.");
        }
        let content = content.to_string();
        self.mutate(target, true, move |entries, limit| {
            if entries.iter().any(|e| e == &content) {
                return MutateOutcome::Direct(self.success_response(
                    target,
                    Some("Entry already exists (no duplicate added)."),
                    None,
                ));
            }
            let mut candidate = entries.to_vec();
            candidate.push(content.clone());
            if self.char_count_entries(&candidate) > limit {
                let count = self.char_count_entries(entries);
                let msg = format!(
                    "Memory at {}/{} chars. Adding this entry ({} chars) would exceed the limit. Consolidate now: use 'replace' to merge overlapping entries into shorter ones or 'remove' stale or less important entries (see current_entries below), then retry this add — all in this turn.",
                    fmt_thousands(count),
                    fmt_thousands(limit),
                    content.chars().count()
                );
                return MutateOutcome::Direct(self.failure_with_entries(target, entries, &msg));
            }
            MutateOutcome::Applied(candidate, "Entry added.".to_string(), None)
        })
    }

    /// Replace the WHOLE entry matching `old_text` with `new_content`.
    pub fn replace(&self, target: &str, old_text: &str, new_content: &str) -> Value {
        let new_content = new_content.trim();
        if old_text.trim().is_empty() {
            return error_value("old_text cannot be empty.");
        }
        if new_content.is_empty() {
            return error_value("new_content cannot be empty. Use 'remove' to delete entries.");
        }
        self.edit(target, old_text.trim(), Some(new_content.to_string()))
    }

    /// Remove the entry containing `old_text`.
    pub fn remove(&self, target: &str, old_text: &str) -> Value {
        if old_text.trim().is_empty() {
            return error_value("old_text cannot be empty.");
        }
        self.edit(target, old_text.trim(), None)
    }

    fn edit(&self, target: &str, old_text: &str, new_content: Option<String>) -> Value {
        let old_text = old_text.to_string();
        self.mutate(target, false, move |entries, limit| {
            let (idx, ambiguous) = find_unique_match(entries, &old_text);
            if ambiguous {
                let matches: Vec<String> = entries
                    .iter()
                    .filter(|e| e.contains(&old_text))
                    .map(|e| {
                        let mut s: String = e.chars().take(80).collect();
                        if e.chars().count() > 80 {
                            s.push_str("...");
                        }
                        s
                    })
                    .collect();
                return MutateOutcome::Direct(error_with(
                    &format!("Multiple entries matched '{}'. Be more specific.", old_text),
                    json!({ "matches": matches }),
                ));
            }
            let idx = match idx {
                Some(i) => i,
                None => {
                    let msg = format!(
                        "No entry matched '{}'. Check current_entries below and retry with the exact text of the entry you want to {}.",
                        old_text,
                        if new_content.is_some() { "replace" } else { "remove" }
                    );
                    return MutateOutcome::Direct(self.consolidation_failure(error_with(
                        &msg,
                        json!({ "current_entries": entries }),
                    )));
                }
            };

            let mut replaced: Vec<String> = Vec::with_capacity(entries.len());
            replaced.extend_from_slice(&entries[..idx]);
            if let Some(ref nc) = new_content {
                replaced.push(nc.clone());
            }
            replaced.extend_from_slice(&entries[idx + 1..]);

            match new_content {
                None => MutateOutcome::Applied(replaced, "Entry removed.".to_string(), None),
                Some(_) => {
                    let new_total = self.char_count_entries(&replaced);
                    if new_total > limit {
                        let msg = format!(
                            "Replacement would put memory at {}/{} chars. Shorten the new content, or 'remove' other stale or less important entries to make room (see current_entries below), then retry — all in this turn.",
                            fmt_thousands(new_total),
                            fmt_thousands(limit)
                        );
                        return MutateOutcome::Direct(self.failure_with_entries(target, entries, &msg));
                    }
                    MutateOutcome::Applied(
                        replaced,
                        "Entry replaced.".to_string(),
                        Some(json!({ "replaced_entry": entries[idx] })),
                    )
                }
            }
        })
    }

    /// Apply one batch op in place; returns `(error message, replaced entry text)`.
    fn apply_batch_op(
        working: &mut Vec<String>,
        act: Option<&str>,
        content: &str,
        old_text: &str,
        pos: &str,
    ) -> (Option<String>, Option<String>) {
        match act {
            Some("add") => {
                if content.is_empty() {
                    return (Some(format!("{}: content is required.", pos)), None);
                }
                if !working.iter().any(|e| e == content) {
                    working.push(content.to_string());
                }
                (None, None)
            }
            Some("replace") | Some("remove") => {
                if old_text.is_empty() {
                    return (Some(format!("{}: old_text is required.", pos)), None);
                }
                if act == Some("replace") && content.is_empty() {
                    return (
                        Some(format!(
                            "{}: content is required (use action='remove' to delete).",
                            pos
                        )),
                        None,
                    );
                }
                let (idx, ambiguous) = find_unique_match(working, old_text);
                if ambiguous {
                    return (
                        Some(format!(
                            "{}: '{}' matched multiple distinct entries -- be more specific.",
                            pos, old_text
                        )),
                        None,
                    );
                }
                let idx = match idx {
                    Some(i) => i,
                    None => {
                        return (Some(format!("{}: no entry matched '{}'.", pos, old_text)), None);
                    }
                };
                let replaced_text = if act == Some("replace") {
                    Some(working[idx].clone())
                } else {
                    None
                };
                if act == Some("replace") {
                    working[idx] = content.to_string();
                } else {
                    working.remove(idx);
                }
                (None, replaced_text)
            }
            _ => (
                Some(format!("{}: unknown action. Use add, replace, or remove.", pos)),
                None,
            ),
        }
    }

    /// Apply add/replace/remove ops atomically against the FINAL budget.
    pub fn apply_batch(&self, target: &str, operations: &[Value]) -> Value {
        if operations.is_empty() {
            return error_value("operations list is empty.");
        }
        let ops: Vec<Map<String, Value>> = operations
            .iter()
            .map(|op| match op {
                Value::Object(m) => m.clone(),
                _ => Map::new(),
            })
            .collect();

        let op_count = ops.len();
        self.mutate(target, false, move |entries, limit| {
            let mut working = entries.to_vec();
            let mut replaced: Map<String, Value> = Map::new();

            for (i, op) in ops.iter().enumerate() {
                let act = op.get("action").and_then(|v| v.as_str());
                let content = op
                    .get("content")
                    .and_then(|v| v.as_str())
                    .or_else(|| op.get("new_text").and_then(|v| v.as_str()))
                    .unwrap_or("")
                    .trim()
                    .to_string();
                let old_text = op
                    .get("old_text")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .trim()
                    .to_string();
                let pos = format!("Operation {} ({})", i + 1, act.unwrap_or("unknown"));

                let (msg, replaced_text) =
                    Self::apply_batch_op(&mut working, act, &content, &old_text, &pos);
                if let Some(m) = msg {
                    return MutateOutcome::Direct(self.batch_failure(target, entries, &m));
                }
                if let Some(rt) = replaced_text {
                    replaced.insert((i + 1).to_string(), json!(rt));
                }
            }

            if !entries.is_empty() && working.is_empty() {
                let label = self
                    .path_for(target)
                    .file_name()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_else(|| target.to_string());
                let msg = format!(
                    "Refusing to empty {}: this batch would remove every entry from a previously non-empty store. Keep at least one entry — merge overlapping entries into a shorter one instead of removing the last one. To delete the final entry deliberately, use single remove() calls.",
                    label
                );
                return MutateOutcome::Direct(self.batch_failure(target, entries, &msg));
            }

            let new_total = self.char_count_entries(&working);
            if new_total > limit {
                let msg = format!(
                    "After applying all {} operations, memory would be at {}/{} chars -- over the limit. Remove or shorten more entries in the same batch, then retry.",
                    op_count,
                    fmt_thousands(new_total),
                    fmt_thousands(limit)
                );
                return MutateOutcome::Direct(self.batch_failure(target, entries, &msg));
            }

            let extra = if replaced.is_empty() {
                None
            } else {
                Some(json!({ "replaced_entries": Value::Object(replaced) }))
            };
            MutateOutcome::Applied(
                working,
                format!("Applied {} operation(s).", op_count),
                extra,
            )
        })
    }
}

impl Default for MemoryStore {
    fn default() -> Self {
        Self::new()
    }
}

/// `(raw, read_ok)`; `read_ok` is false ONLY when the file EXISTS but cannot be read.
fn read_raw_checked(path: &Path) -> (String, bool) {
    if !path.exists() {
        return (String::new(), true);
    }
    match std::fs::read(path) {
        Ok(bytes) => match String::from_utf8(strip_bom(bytes)) {
            Ok(s) => (s, true),
            Err(_) => (String::new(), false),
        },
        Err(_) => (String::new(), false),
    }
}

fn read_failed_error(path: &Path) -> Value {
    error_value(&format!(
        "Refusing to write {}: the file exists on disk but could not be read right now (temporarily locked by another program, a permission change, invalid/corrupt text encoding, or a filesystem error). Treating an unreadable file as empty and saving would wipe existing memory, so the write is refused. Nothing was changed — retry in a moment.",
        file_name(path)
    ))
}

fn drift_error(path: &Path, bak_path: &str) -> Value {
    error_with(
        &format!(
            "Refusing to write {}: file on disk has content that wouldn't round-trip through the memory tool (likely added by the patch tool, a shell append, a manual edit, or a concurrent session). A snapshot was saved to {}. Resolve the drift first — either rewrite the file as a clean §-delimited list of entries, or move the extra content out — then retry. This guard exists to prevent silent data loss.",
            file_name(path),
            bak_path
        ),
        json!({
            "drift_backup": bak_path,
            "remediation": "Open the .bak file, integrate the missing entries into the memory tool one at a time via memory(action=add, content=...), then remove or rewrite the original file to a clean state.",
        }),
    )
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| path.display().to_string())
}

/// Atomic temp-file + rename: readers never see a truncated file.
fn write_file(path: &Path, entries: &[String]) -> std::io::Result<()> {
    let content = entries.join(ENTRY_DELIMITER);
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(".mem_{}.tmp", Uuid::new_v4()));
    std::fs::write(&tmp, content.as_bytes())?;
    std::fs::rename(&tmp, path)
}

/// Shared single validation error for an invalid or disabled target.
fn memory_target_error(store: &MemoryStore, target: &str) -> Option<Value> {
    if target != "memory" && target != "user" {
        return Some(error_value(&format!(
            "Invalid memory target '{}'. Use 'memory' or 'user'.",
            target
        )));
    }
    if store.target_enabled(target) {
        return None;
    }
    let label = if target == "user" { "USER.md" } else { "MEMORY.md" };
    Some(json!({
        "success": false,
        "error": format!("Built-in {} writes are disabled in memory config.", label),
        "target": target,
    }))
}

fn single_op_error(message: &str) -> String {
    error_value(message).to_string()
}

/// Tool entry point; returns a JSON string. Single op (action + content/old_text)
/// or batch (`operations`, atomic against the final budget).
pub fn memory_tool(
    store: &MemoryStore,
    action: Option<&str>,
    target: Option<&str>,
    content: Option<&str>,
    old_text: Option<&str>,
    new_text: Option<&str>,
    operations: Option<&Vec<Value>>,
) -> String {
    let content = if content.is_none() && new_text.is_some() {
        new_text
    } else {
        content
    };
    let target = target.unwrap_or("memory");

    if let Some(err) = memory_target_error(store, target) {
        return err.to_string();
    }

    if let Some(ops) = operations {
        if !ops.is_empty() {
            return store.apply_batch(target, ops).to_string();
        }
    }

    let action = action.unwrap_or("");
    if action != "add" && action != "replace" && action != "remove" {
        return single_op_error(&format!(
            "Unknown action '{}'. Use: add, replace, remove",
            action
        ));
    }

    let content_missing = content.map(|c| c.is_empty()).unwrap_or(true);
    let old_missing = old_text.map(|o| o.is_empty()).unwrap_or(true);

    if action == "add" && content_missing {
        return single_op_error("Content is required for 'add' action.");
    }
    if (action == "replace" || action == "remove") && old_missing {
        let replace_hint = if action == "replace" {
            " For 'replace', content is the COMPLETE new entry -- the whole matched entry is overwritten, not just the old_text span."
        } else {
            ""
        };
        return json!({
            "success": false,
            "error": format!(
                "'{}' needs old_text -- a short unique substring of the entry to {}. None was provided. Reissue the {} with old_text set to part of one of the current_entries below.{}",
                action, action, action, replace_hint
            ),
            "current_entries": store.entries_for(target),
            "usage": store.usage(target),
        })
        .to_string();
    }
    if action == "replace" && content_missing {
        return single_op_error("content is required for 'replace' action.");
    }

    let content = content.unwrap_or("");
    let old_text = old_text.unwrap_or("");
    let result = match action {
        "add" => store.add(target, content),
        "replace" => store.replace(target, old_text, content),
        "remove" => store.remove(target, old_text),
        _ => unreachable!(),
    };
    result.to_string()
}

/// Tool definition for the `memory` tool (mirrors the Python `MEMORY_SCHEMA`).
pub fn get_definition() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": "memory",
            "description": "Save durable facts to persistent memory that survive across sessions. Memory is injected into every future turn, so keep entries compact and high-signal.\n\nHOW: make ALL your changes in ONE call via an 'operations' array (each item: {action, content?, old_text?}). The batch applies atomically and the char limit is checked only on the FINAL result — so a single call can remove/replace stale entries to free room AND add new ones, even when an add alone would overflow. The response reports current/limit chars and confirms completion; one batch call finishes the update, so don't repeat it. Use the bare action/content/old_text fields only for a single lone change.\n\nWHEN: only for facts that apply to EVERY session regardless of task: who the user is, stable environment facts, standing conventions with no task home. Anything learned while doing a task (procedures, pitfalls, and the user's preferences and corrections for that kind of work) belongs in the task's skill via skill_manage, where it loads only when relevant; memory is injected into every turn and must stay small.\n\nIF FULL: an add is rejected with the current entries shown. Reissue as ONE batch that removes or shortens enough stale entries and adds the new one together.\n\nTARGETS: 'user' = who the user is (name, role, preferences, style). 'memory' = your notes (environment, conventions, tool quirks, lessons).\n\nSKIP: trivial/obvious info, easily re-discovered facts, raw data dumps, task progress, completed-work logs, temporary TODO state (use session_search for those). Reusable procedures belong in a skill, not memory.",
            "parameters": {
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["add", "replace", "remove"],
                        "description": "The action to perform (single-op shape). Omit when using 'operations'."
                    },
                    "target": {
                        "type": "string",
                        "enum": ["memory", "user"],
                        "description": "Which memory store: 'memory' for personal notes, 'user' for user profile."
                    },
                    "content": {
                        "type": "string",
                        "description": "The entry content. Required for 'add' and 'replace'. For 'replace' it is the COMPLETE new entry text: the whole matched entry is overwritten, so include everything you want to keep. Alias: 'new_text' is also accepted (same full-entry meaning)."
                    },
                    "old_text": {
                        "type": "string",
                        "description": "REQUIRED for 'replace' and 'remove' (single-op shape): a short unique substring IDENTIFYING the existing entry to modify -- it locates the entry, it is not spliced out. Omit only for 'add'."
                    },
                    "new_text": {
                        "type": "string",
                        "description": "Alias for 'content' (single-op shape): the COMPLETE new entry for 'replace', not a patch of old_text. If both are set, 'content' wins."
                    },
                    "operations": {
                        "type": "array",
                        "description": "Batch shape: a list of operations applied atomically in one call against the final char budget. Preferred when making multiple changes or consolidating to make room. Each item is {action, content?, old_text?}.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "action": {"type": "string", "enum": ["add", "replace", "remove"]},
                                "content": {"type": "string", "description": "Entry content for add/replace. For replace, the COMPLETE new entry (whole entry is overwritten). Alias: 'new_text'."},
                                "new_text": {"type": "string", "description": "Alias for 'content' in a batch op."},
                                "old_text": {"type": "string", "description": "Substring identifying the entry for replace/remove."}
                            },
                            "required": ["action"]
                        }
                    }
                },
                "required": ["target"]
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store(limit: usize) -> (MemoryStore, PathBuf) {
        let dir = std::env::temp_dir().join(format!("hermes_mem_{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = MemoryStore::with_options(limit, 500, true, true, dir.clone());
        (store, dir)
    }

    fn parse(s: &str) -> Value {
        serde_json::from_str(s).unwrap()
    }

    #[test]
    fn test_single_op_add() {
        let (store, dir) = temp_store(2200);
        let out = memory_tool(
            &store,
            Some("add"),
            Some("memory"),
            Some("Environment uses a single-node gateway."),
            None,
            None,
            None,
        );
        let v = parse(&out);
        assert_eq!(v["success"], json!(true));
        assert_eq!(v["done"], json!(true));
        assert_eq!(v["target"], json!("memory"));
        assert_eq!(v["entry_count"], json!(1));
        assert!(v["usage"].as_str().unwrap().contains("chars"));
        assert_eq!(store.entries_for("memory").len(), 1);
        // Persisted to disk in the exact delimiter format.
        let disk = std::fs::read_to_string(dir.join("MEMORY.md")).unwrap();
        assert_eq!(disk, "Environment uses a single-node gateway.");
    }

    #[test]
    fn test_add_overflow_rejected_showing_current_entries() {
        let (store, _dir) = temp_store(40);
        store.add("memory", "first entry that fits fine");
        let out = memory_tool(
            &store,
            Some("add"),
            Some("memory"),
            Some("another entry that will not fit at all"),
            None,
            None,
            None,
        );
        let v = parse(&out);
        assert_eq!(v["success"], json!(false));
        assert!(v["error"].as_str().unwrap().contains("would exceed the limit"));
        assert!(v["error"].as_str().unwrap().contains("Consolidate now"));
        let current = v["current_entries"].as_array().unwrap();
        assert_eq!(current.len(), 1);
        assert_eq!(current[0], json!("first entry that fits fine"));
        assert!(v["usage"].as_str().is_some());
        // Store unchanged.
        assert_eq!(store.entries_for("memory").len(), 1);
    }

    #[test]
    fn test_batch_frees_room_then_adds_atomically() {
        let (store, _dir) = temp_store(75);
        store.add("memory", "stale entry alpha that is here");
        store.add("memory", "stale entry beta that is here now");
        assert_eq!(store.entries_for("memory").len(), 2);

        // A lone add of the new long entry would overflow.
        let lone = store.add("memory", "brand new consolidated entry value here");
        assert_eq!(lone["success"], json!(false));

        // One batch: remove both stale entries AND add the new one -> succeeds.
        let ops = json!([
            {"action": "remove", "old_text": "stale entry alpha"},
            {"action": "remove", "old_text": "stale entry beta"},
            {"action": "add", "content": "brand new consolidated entry value here"}
        ]);
        let out = memory_tool(&store, None, Some("memory"), None, None, None, Some(ops.as_array().unwrap()));
        let v = parse(&out);
        assert_eq!(v["success"], json!(true));
        assert_eq!(v["entry_count"], json!(1));
        assert_eq!(
            store.entries_for("memory"),
            vec!["brand new consolidated entry value here".to_string()]
        );
    }

    #[test]
    fn test_batch_aborts_atomically_on_overflow() {
        let (store, _dir) = temp_store(60);
        store.add("memory", "keep me around");
        let ops = json!([
            {"action": "add", "content": "this addition alone pushes the whole thing far over the limit"}
        ]);
        let out = memory_tool(&store, None, Some("memory"), None, None, None, Some(ops.as_array().unwrap()));
        let v = parse(&out);
        assert_eq!(v["success"], json!(false));
        assert!(v["error"].as_str().unwrap().contains("No operations were applied"));
        // No partial write.
        assert_eq!(store.entries_for("memory"), vec!["keep me around".to_string()]);
    }

    #[test]
    fn test_replace() {
        let (store, _dir) = temp_store(2200);
        store.add("memory", "primary gateway runs on the east node");
        let out = memory_tool(
            &store,
            Some("replace"),
            Some("memory"),
            Some("primary gateway now runs on the west node"),
            Some("east node"),
            None,
            None,
        );
        let v = parse(&out);
        assert_eq!(v["success"], json!(true));
        assert_eq!(
            store.entries_for("memory"),
            vec!["primary gateway now runs on the west node".to_string()]
        );
        assert!(v["replaced_entry"].as_str().unwrap().contains("east node"));
    }

    #[test]
    fn test_remove() {
        let (store, _dir) = temp_store(2200);
        store.add("memory", "entry to be removed");
        store.add("memory", "entry to keep");
        let out = memory_tool(
            &store,
            Some("remove"),
            Some("memory"),
            None,
            Some("to be removed"),
            None,
            None,
        );
        let v = parse(&out);
        assert_eq!(v["success"], json!(true));
        assert_eq!(store.entries_for("memory"), vec!["entry to keep".to_string()]);
    }

    #[test]
    fn test_order_preservation() {
        let (store, _dir) = temp_store(2200);
        store.add("memory", "first");
        store.add("memory", "second");
        store.add("memory", "third");
        store.replace("memory", "second", "second-updated");
        assert_eq!(
            store.entries_for("memory"),
            vec![
                "first".to_string(),
                "second-updated".to_string(),
                "third".to_string()
            ]
        );
    }

    #[test]
    fn test_delimiter_round_trip() {
        let (store, dir) = temp_store(2200);
        store.add("memory", "entry one");
        store.add("memory", "entry two");
        let disk = std::fs::read_to_string(dir.join("MEMORY.md")).unwrap();
        assert_eq!(disk, "entry one\n§\nentry two");

        // Reload from disk into a fresh store: entries survive the delimiter round-trip.
        let reloaded = MemoryStore::with_options(2200, 500, true, true, dir.clone());
        assert_eq!(
            reloaded.entries_for("memory"),
            vec!["entry one".to_string(), "entry two".to_string()]
        );
        // A bare § inside an entry is preserved (only the FULL delimiter splits).
        store.add("memory", "has § inside one entry");
        let disk2 = std::fs::read_to_string(dir.join("MEMORY.md")).unwrap();
        assert!(disk2.contains("has § inside one entry"));
        let reloaded2 = MemoryStore::with_options(2200, 500, true, true, dir.clone());
        assert!(reloaded2
            .entries_for("memory")
            .contains(&"has § inside one entry".to_string()));
    }

    #[test]
    fn test_old_text_required_returns_inventory() {
        let (store, _dir) = temp_store(2200);
        store.add("memory", "some existing entry");
        let out = memory_tool(
            &store,
            Some("remove"),
            Some("memory"),
            None,
            None,
            None,
            None,
        );
        let v = parse(&out);
        assert_eq!(v["success"], json!(false));
        assert!(v["error"].as_str().unwrap().contains("needs old_text"));
        assert_eq!(v["current_entries"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn test_target_separation() {
        let (store, dir) = temp_store(2200);
        store.add("memory", "agent note");
        store.add("user", "user preference");
        assert_eq!(store.entries_for("memory"), vec!["agent note".to_string()]);
        assert_eq!(store.entries_for("user"), vec!["user preference".to_string()]);
        assert!(dir.join("MEMORY.md").exists());
        assert!(dir.join("USER.md").exists());
    }
}
