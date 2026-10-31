//! Skill Manager Tool — agent-managed skill creation & editing (1-to-1 port of
//! `tools/skill_manager_tool.py` + `tools/skill_manager_guards.py` +
//! `tools/skill_manager_batch.py`).
//!
//! Skills are the agent's procedural memory (narrow "how to do X"; MEMORY.md/USER.md
//! are broad, declarative). New skills land in `~/.hermes/skills/`; existing skills
//! are modified in place. Layout:
//! `<skills>/[category/]<skill>/SKILL.md` + optional `references/ templates/ scripts/ assets/`.
//!
//! Correctness rules ported from Python:
//! - A mutation is a read-modify-write under a per-skill lock.
//! - Batch ops are atomic: every touched skill is snapshotted first and any failure
//!   rolls ALL of them back (batch-created skills are removed).
//! - `delete` is only legal as the SOLE op.
//! - Path validation runs BEFORE any effect; traversal is refused.
//! - Deleting a symlinked directory, a skills root itself, or a path outside every
//!   known skills root is refused (the rmtree-follows-symlink wipe class).

use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use parking_lot::Mutex;

/// Resolve the default skills directory: `HERMES_HOME/skills`, else `HOME/.hermes/skills`.
pub fn default_skills_dir() -> PathBuf {
    if let Ok(home) = std::env::var("HERMES_HOME") {
        return PathBuf::from(home).join("skills");
    }
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home).join(".hermes").join("skills");
    }
    PathBuf::from(".hermes").join("skills")
}

/// Maximum skill-name / category length.
pub const MAX_NAME_LENGTH: usize = 64;
/// Maximum frontmatter description length.
pub const MAX_DESCRIPTION_LENGTH: usize = 1024;
/// Maximum SKILL.md body size in characters.
pub const MAX_SKILL_CONTENT_CHARS: usize = 100_000;
/// Maximum size of a single supporting file, in bytes (1 MiB).
pub const MAX_SKILL_FILE_BYTES: usize = 1_048_576;
/// New skills must fit the system-prompt description budget (see `agent/skill_utils.py`).
pub const SKILL_PROMPT_DESC_LIMIT: usize = 60;
/// Maximum ops in one atomic batch call.
pub const BATCH_MAX_OPS: usize = 20;

/// Supporting-file subdirectories a write_file/remove_file may target.
pub const ALLOWED_SUBDIRS: [&str; 4] = ["references", "templates", "scripts", "assets"];

const NAME_RULE: &str = "Use lowercase letters, numbers, hyphens, dots, and underscores.";

/// Actions the batch path accepts. `delete` must be the sole op.
const BATCH_OP_ACTIONS: [&str; 4] = ["create", "patch", "write_file", "remove_file"];

/// Actions in the flat dispatch table.
const ALL_ACTIONS: [&str; 6] = ["create", "edit", "patch", "delete", "write_file", "remove_file"];

// ── Refusal helpers ────────────────────────────────────────────────────────

/// `{"success": false, "error": ...}` plus any extra fields.
pub fn refusal(message: &str) -> Value {
    json!({ "success": false, "error": message })
}

fn refusal_with(message: &str, extra: Value) -> Value {
    let mut obj = match refusal(message) {
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

// ── Validation helpers ─────────────────────────────────────────────────────

/// True when `value` is a filesystem-safe skill identifier.
/// Mirrors `VALID_NAME_RE = ^[a-z0-9][a-z0-9._-]*$`.
fn valid_name_re(value: &str) -> bool {
    let mut chars = value.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() || c.is_ascii_digit() => {}
        _ => return false,
    }
    chars.all(|c| {
        c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '_' || c == '-'
    })
}

fn check_identifier(value: &str, label: &str, invalid: &str) -> Option<String> {
    if value.chars().count() > MAX_NAME_LENGTH {
        return Some(format!("{} exceeds {} characters.", label, MAX_NAME_LENGTH));
    }
    if valid_name_re(value) {
        None
    } else {
        Some(invalid.to_string())
    }
}

/// Validate a skill name; `None` when acceptable.
pub fn validate_name(name: &str) -> Option<String> {
    if name.is_empty() {
        return Some("Skill name is required.".to_string());
    }
    check_identifier(
        name,
        "Skill name",
        &format!(
            "Invalid skill name '{}'. {} Must start with a letter or digit.",
            name, NAME_RULE
        ),
    )
}

/// Validate an optional category; `None` when acceptable (including absent).
pub fn validate_category(category: Option<&str>) -> Option<String> {
    let category = match category {
        None => return None,
        Some(c) if c.trim().is_empty() => return None,
        Some(c) => c.trim().to_string(),
    };
    let invalid = format!(
        "Invalid category '{}'. {} Categories must be a single directory name.",
        category, NAME_RULE
    );
    if category.contains('/') || category.contains('\\') {
        return Some(invalid);
    }
    check_identifier(&category, "Category", &invalid)
}

/// Split a `---` frontmatter block off `content` → `(frontmatter_yaml, body_after_close)`.
///
/// The Python code searches for `\n---\s*\n` inside `content[3:]`; the returned
/// `fm_end` index is relative to that slice, so the frontmatter text is
/// `content[3..3+fm_start]` and the body begins at `3 + fm_end`.
fn split_frontmatter(content: &str) -> Option<(&str, &str)> {
    if !content.starts_with("---") {
        return None;
    }
    let rest = &content[3..];
    let bytes = rest.as_bytes();
    // Scan for "\n" that starts a line which is "---" optionally followed by
    // whitespace then "\n" (mirrors _FRONTMATTER_END_RE = \n---\s*\n, non-greedy).
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'\n' && rest[i + 1..].starts_with("---") {
            // Walk optional whitespace after the dashes.
            let mut j = i + 1 + 3;
            while j < bytes.len() && (bytes[j] as char).is_whitespace() && bytes[j] != b'\n' {
                j += 1;
            }
            if j < bytes.len() && bytes[j] == b'\n' {
                let fm = &rest[..i];
                let body = &rest[j + 1..];
                return Some((fm, body));
            }
        }
        i += 1;
    }
    None
}

/// Parse YAML frontmatter into a JSON object; `Err` carries the parse failure.
fn parse_frontmatter_yaml(fm: &str) -> Result<Value, String> {
    serde_yaml::from_str::<serde_yaml::Value>(fm)
        .map(|v| serde_json::to_value(v).unwrap_or(Value::Null))
        .map_err(|e| format!("YAML frontmatter parse error: {}", e))
}

/// Validate frontmatter (name + description) and a non-empty body. `new_skill`
/// (create only) also enforces `SKILL_PROMPT_DESC_LIMIT` so new skills never lose
/// routing signal to index truncation; edit/patch skip it so existing over-limit
/// skills stay maintainable. Returns the refusal message, or `None` when valid.
pub fn validate_frontmatter(content: &str, new_skill: bool) -> Option<String> {
    if content.trim().is_empty() {
        return Some("Content cannot be empty.".to_string());
    }
    // Tolerate a Windows UTF-8 BOM.
    let content = content.trim_start_matches('\u{feff}');
    if !content.starts_with("---") {
        return Some(
            "SKILL.md must start with YAML frontmatter (---). See existing skills for format."
                .to_string(),
        );
    }
    let (fm, body) = match split_frontmatter(content) {
        Some(parts) => parts,
        None => {
            return Some(
                "SKILL.md frontmatter is not closed. Ensure you have a closing '---' line."
                    .to_string(),
            )
        }
    };
    let parsed = match parse_frontmatter_yaml(fm) {
        Ok(v) => v,
        Err(e) => return Some(e),
    };
    let obj = match parsed.as_object() {
        Some(o) => o,
        None => {
            return Some("Frontmatter must be a YAML mapping (key: value pairs).".to_string())
        }
    };
    for field in ["name", "description"] {
        if !obj.contains_key(field) {
            return Some(format!("Frontmatter must include '{}' field.", field));
        }
    }
    let desc = value_to_string(obj.get("description"));
    if desc.chars().count() > MAX_DESCRIPTION_LENGTH {
        return Some(format!(
            "Description exceeds {} characters.",
            MAX_DESCRIPTION_LENGTH
        ));
    }
    if new_skill {
        let trimmed = desc.trim().trim_matches(|c| c == '\'' || c == '"');
        if trimmed.chars().count() > SKILL_PROMPT_DESC_LIMIT {
            return Some(format!(
                "Description is {} chars — new skills must fit the {} chars system-prompt budget \
                 (one sentence, trigger first, ends with a period). The skill index truncates \
                 longer descriptions to {} chars + '...', destroying the routing signal. Move \
                 detail into the skill body.",
                desc.trim().chars().count(),
                SKILL_PROMPT_DESC_LIMIT,
                SKILL_PROMPT_DESC_LIMIT - 3
            ));
        }
    }
    if body.trim().is_empty() {
        return Some(
            "SKILL.md must have content after the frontmatter (instructions, procedures, etc.)."
                .to_string(),
        );
    }
    None
}

/// Render a frontmatter YAML value as its string form (Python `str(value)`).
fn value_to_string(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Bool(b)) => b.to_string(),
        Some(Value::Number(n)) => n.to_string(),
        Some(other) => other.to_string(),
    }
}

/// Refuse content over the character budget; `None` when acceptable.
pub fn validate_content_size(content: &str, label: &str) -> Option<String> {
    let len = content.chars().count();
    if len > MAX_SKILL_CONTENT_CHARS {
        return Some(format!(
            "{} content is {} characters (limit: {}). Consider splitting into a smaller SKILL.md \
             with supporting files in references/ or templates/.",
            label,
            thousands(len),
            thousands(MAX_SKILL_CONTENT_CHARS)
        ));
    }
    None
}

fn thousands(n: usize) -> String {
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

/// First 120 chars of the frontmatter description; "" on any failure.
fn description_preview(content: &str) -> String {
    let content = content.trim_start_matches('\u{feff}');
    let Some((fm, _)) = split_frontmatter(content) else {
        return String::new();
    };
    let Ok(parsed) = parse_frontmatter_yaml(fm) else {
        return String::new();
    };
    let desc = value_to_string(parsed.get("description"));
    desc.chars().take(120).collect()
}

// ── Path safety ────────────────────────────────────────────────────────────

/// True when any component of `p` is a literal `..`.
fn has_traversal_component(file_path: &str) -> bool {
    Path::new(file_path)
        .components()
        .any(|c| matches!(c, Component::ParentDir))
}

/// Normalise a relative POSIX-style path: strip leading slashes, resolve `.`
/// and `..` lexically (mirrors `posixpath.normpath`). `..` that escapes the root
/// is preserved so the traversal check still rejects it.
fn posix_normpath(p: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for part in p.split('/') {
        match part {
            "" | "." => continue,
            ".." => {
                if matches!(out.last(), Some(&last) if last != "..") {
                    out.pop();
                } else {
                    out.push("..");
                }
            }
            other => out.push(other),
        }
    }
    out.join("/")
}

/// Validate a write_file/remove_file path: under an allowed subdir, no escape.
/// Returns the refusal message, or `None` when acceptable.
pub fn validate_file_path(file_path: &str) -> Option<String> {
    if file_path.is_empty() {
        return Some("file_path is required.".to_string());
    }
    // Traversal first, so the SKILL.md exception stays unreachable by a
    // traversal-laden path.
    if has_traversal_component(file_path) {
        return Some("Path traversal ('..') is not allowed.".to_string());
    }
    let parts: Vec<&str> = Path::new(file_path)
        .components()
        .filter_map(|c| match c {
            Component::Normal(s) => Some(s.to_str().unwrap_or("")),
            _ => None,
        })
        .collect();
    // SKILL.md lives at the skill root; accept 'SKILL.md' and '<skill>/SKILL.md'.
    if parts.last() == Some(&"SKILL.md") && (parts.len() == 1 || parts.len() == 2) {
        return None;
    }
    let Some(first) = parts.first() else {
        let allowed = ALLOWED_SUBDIRS.join(", ");
        return Some(format!(
            "File must be under one of: {}. Got: '{}'",
            allowed, file_path
        ));
    };
    if !ALLOWED_SUBDIRS.contains(first) {
        let allowed = ALLOWED_SUBDIRS.join(", ");
        return Some(format!(
            "File must be under one of: {}. Got: '{}'",
            allowed, file_path
        ));
    }
    if parts.len() < 2 {
        return Some(format!(
            "Provide a file path, not just a directory. Example: '{}/myfile.md'",
            parts[0]
        ));
    }
    None
}

/// Error if `path` does not resolve inside `root` (symlinks and `..` followed).
fn validate_within_dir(path: &Path, root: &Path) -> Option<String> {
    let resolved = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let root_resolved = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    if resolved.starts_with(&root_resolved) {
        None
    } else {
        Some(format!(
            "Path escapes allowed directory: {}",
            resolved.display()
        ))
    }
}

/// True for a symlink or a path whose final component is one (the rmtree redirection class).
fn is_path_redirect(path: &Path) -> bool {
    std::fs::symlink_metadata(path)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
}

// ── Origin / staging state ─────────────────────────────────────────────────
//
// The Python origin markers are process-global ContextVars. Rust mirrors that
// with process-global atomics on the manager, which is the same observable
// contract for a single-run gateway process.

/// Process-global origin flags shared by the manager and the guards.
static BACKGROUND_REVIEW: AtomicBool = AtomicBool::new(false);
/// Set while replaying an approved staged skill write, so the gate does not re-fire.
static GATE_BYPASS: AtomicBool = AtomicBool::new(false);

/// Mark the current run as an autonomous background review (curator fork).
pub fn set_background_review(on: bool) {
    BACKGROUND_REVIEW.store(on, Ordering::SeqCst);
}

/// True inside the autonomous curator review fork.
pub fn is_background_review() -> bool {
    BACKGROUND_REVIEW.load(Ordering::SeqCst)
}

/// Bypass the write gate while replaying an already-approved staged write.
pub fn set_gate_bypass(on: bool) {
    GATE_BYPASS.store(on, Ordering::SeqCst);
}

fn gate_bypassed() -> bool {
    GATE_BYPASS.load(Ordering::SeqCst)
}

// ── Read marks (background-review read-before-write) ───────────────────────

fn read_marks() -> &'static Mutex<HashSet<String>> {
    static MARKS: once_cell_lite::Lazy<Mutex<HashSet<String>>> =
        once_cell_lite::Lazy::new(|| Mutex::new(HashSet::new()));
    &MARKS
}

/// Record that the active background-review fork has read a skill file. The fork
/// must not patch content it only inferred from the transcript, so `skill_view`
/// and `read_file` call this and the write guards require the mark.
pub fn mark_background_review_skill_read(path: &Path) {
    if !is_background_review() {
        return;
    }
    let resolved = resolved_str(path);
    read_marks().lock().insert(resolved);
}

fn background_review_has_read(path: &Path) -> bool {
    read_marks().lock().contains(&resolved_str(path))
}

/// Start a fresh, isolated read set for the current review context.
pub fn reset_background_review_read_marks() {
    read_marks().lock().clear();
}

fn resolved_str(path: &Path) -> String {
    path.canonicalize()
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .to_string()
}

/// Minimal lazy-static shim (avoids an extra dependency for one shared set).
mod once_cell_lite {
    use std::ops::Deref;
    use std::sync::OnceLock;

    pub struct Lazy<T> {
        cell: OnceLock<T>,
        init: fn() -> T,
    }

    impl<T> Lazy<T> {
        pub const fn new(init: fn() -> T) -> Self {
            Self {
                cell: OnceLock::new(),
                init,
            }
        }
    }

    impl<T> Deref for Lazy<T> {
        type Target = T;
        fn deref(&self) -> &T {
            self.cell.get_or_init(self.init)
        }
    }
}

// ── Guards ─────────────────────────────────────────────────────────────────

/// Refusal message if `name` is pinned or essential, else `None`. Pin only guards
/// deletion; patches/edits stay allowed.
pub fn pinned_guard(name: &str, essential: &HashSet<String>, pinned: &HashSet<String>) -> Option<String> {
    if essential.contains(name) {
        return Some(format!(
            "Skill '{}' is essential to Hermes (the agent's own operating manual referenced by \
             the system prompt) and cannot be deleted. Patches and edits are still allowed.",
            name
        ));
    }
    if pinned.contains(name) {
        return Some(format!(
            "Skill '{}' is pinned and cannot be deleted by skill_manage. Ask the user to run \
             `hermes curator unpin {}` if they want to delete it. Patches and edits are allowed \
             on pinned skills; only deletion is blocked.",
            name, name
        ));
    }
    None
}

/// Refuse autonomous curator writes to anything but curator-owned sediment. The
/// review fork has no user in the loop, so it is also blocked on pinned/external/
/// bundled/hub skills. `curator_managed` is the caller-resolved provenance fact.
pub fn background_review_write_guard(
    name: &str,
    action: &str,
    is_pinned: bool,
    is_external: bool,
    is_protected_builtin: bool,
    is_hub_installed: bool,
    is_bundled: bool,
    curator_managed: bool,
    provenance_available: bool,
) -> Option<Value> {
    if !is_background_review() {
        return None;
    }
    let refuse = format!("Refusing background curator {} for", action);
    if is_pinned {
        return Some(refusal(&format!(
            "{} pinned skill '{}': pinned skills are off-limits to autonomous maintenance. Ask \
             the user to run `hermes curator unpin {}` if they want it changed.",
            refuse, name, name
        )));
    }
    if is_external {
        return Some(refusal(&format!(
            "{} skill '{}': the skill lives in skills.external_dirs, which are externally owned \
             and read-only to autonomous curation.",
            refuse, name
        )));
    }
    for (flag, label) in [
        (is_protected_builtin, "protected built-in"),
        (is_hub_installed, "hub-installed"),
        (is_bundled, "bundled"),
    ] {
        if flag {
            return Some(refusal(&format!("{} {} skill '{}'.", refuse, label, name)));
        }
    }
    if !provenance_available {
        return Some(refusal(&format!(
            "{} skill '{}': agent ownership could not be verified because the provenance record \
             is unavailable or unreadable.",
            refuse, name
        )));
    }
    // Not curator-managed (no `created_by: "agent"`) => user-owned. A MISSING record
    // and an explicit null must resolve IDENTICALLY (keying on presence made the policy
    // depend on the guard's own side effect).
    if !curator_managed {
        return Some(refusal(&format!(
            "{} skill '{}': the skill is not curator-managed. User-owned skills are off-limits to \
             autonomous curation. Run `hermes curator adopt {}` to opt it in.",
            refuse, name, name
        )));
    }
    None
}

/// Require review forks to load the exact target before mutating it.
fn background_review_read_before_write_guard(
    name: &str,
    target: &Path,
    action: &str,
    file_label: &str,
) -> Option<Value> {
    if !is_background_review() || background_review_has_read(target) {
        return None;
    }
    Some(refusal_with(
        &format!(
            "Refusing background curator {} for skill '{}': the current {} content has not been \
             loaded in this review turn. Call skill_view(name) for SKILL.md, or \
             skill_view(name, file_path=...) for a supporting file, then retry the write using \
             the content just returned.",
            action, name, file_label
        ),
        json!({ "_read_before_write_required": true }),
    ))
}

/// Fail closed on unverified deletes during the curator consolidation pass. The
/// fork's only legitimate delete is a consolidation declared via
/// `absorbed_into=<umbrella>`; a bare delete here can only be the LLM pass pruning
/// without evidence.
pub fn curator_consolidation_delete_guard(
    name: &str,
    absorbed_into: Option<&str>,
) -> Option<Value> {
    if !is_background_review() {
        return None;
    }
    if absorbed_into.map(|s| !s.trim().is_empty()).unwrap_or(false) {
        return None;
    }
    Some(refusal_with(
        &format!(
            "Refusing background curator delete of skill '{}': the consolidation pass may only \
             archive a skill it has absorbed into an umbrella. Pass absorbed_into=<umbrella> \
             (the umbrella must already exist) to record a verified consolidation. Pruning a \
             skill with no forwarding target is not permitted here — the deterministic \
             inactivity prune handles staleness archival separately. Keeping '{}' active.",
            name, name
        ),
        json!({ "_fail_closed": true }),
    ))
}

/// Last-line guard before a recursive delete: even a poisoned tree must never
/// delete (1) a path outside every known skills root, (2) a skills root itself,
/// (3) a symlink/junction (a recursive delete follows it).
pub fn validate_delete_target(skill_dir: &Path, skills_roots: &[PathBuf]) -> Option<String> {
    if is_path_redirect(skill_dir) {
        return Some(format!(
            "Refusing to delete '{}': the skill directory is a symlink/junction. Remove the link \
             target manually if intended.",
            skill_dir.display()
        ));
    }
    let resolved = match skill_dir.canonicalize() {
        Ok(p) => p,
        Err(e) => {
            return Some(format!(
                "Refusing to delete '{}': could not resolve path ({}).",
                skill_dir.display(),
                e
            ))
        }
    };
    for root in skills_roots {
        let root_resolved = root.canonicalize().unwrap_or_else(|_| root.clone());
        if resolved == root_resolved {
            return Some(format!(
                "Refusing to delete '{}': resolves to the skills root itself, which would remove \
                 every installed skill.",
                skill_dir.display()
            ));
        }
        if resolved.starts_with(&root_resolved) {
            return None;
        }
    }
    Some(format!(
        "Refusing to delete '{}': path does not resolve inside any known skills root.",
        skill_dir.display()
    ))
}

// ── Manager ────────────────────────────────────────────────────────────────

/// Skill manager: dispatch, validation, guards, atomic writes.
pub struct SkillManager {
    skills_dir: PathBuf,
    /// Optional `skills.create_dir` override for NEW skills.
    create_dir: Option<PathBuf>,
    /// `skills.guard_agent_created` — opt-in post-write security scan.
    guard_agent_created: bool,
    /// Names permanently protected from deletion.
    essential_skills: HashSet<String>,
    /// Names pinned by the user (deletion blocked; patches allowed).
    pinned: HashSet<String>,
    /// Per-skill mutation locks (keyed by the skill's directory name).
    locks: Mutex<HashMap<String, std::sync::Arc<Mutex<()>>>>,
    /// Pending staged writes awaiting approval.
    staged: Mutex<Vec<Value>>,
    /// Write-approval gate: OFF by default (mirrors the Python gate being off by
    /// default). When ON, every skill write is staged instead of applied.
    stage_writes: AtomicBool,
}

impl SkillManager {
    pub fn new(skills_dir: PathBuf) -> Self {
        Self {
            skills_dir,
            create_dir: None,
            guard_agent_created: false,
            essential_skills: HashSet::new(),
            pinned: HashSet::new(),
            locks: Mutex::new(HashMap::new()),
            staged: Mutex::new(Vec::new()),
            stage_writes: AtomicBool::new(false),
        }
    }

    pub fn with_create_dir(mut self, dir: Option<PathBuf>) -> Self {
        self.create_dir = dir;
        self
    }

    pub fn with_guard_agent_created(mut self, on: bool) -> Self {
        self.guard_agent_created = on;
        self
    }

    pub fn with_essential_skills(mut self, names: impl IntoIterator<Item = String>) -> Self {
        self.essential_skills = names.into_iter().collect();
        self
    }

    /// Names pinned by the user (deletion blocked; patches/edits allowed).
    pub fn pinned(&self) -> &HashSet<String> {
        &self.pinned
    }

    pub fn skills_dir(&self) -> &Path {
        &self.skills_dir
    }

    /// Staged (pending-approval) write proposals.
    pub fn staged_writes(&self) -> Vec<Value> {
        self.staged.lock().clone()
    }

    /// Enable or disable the write-approval gate. OFF by default: writes apply
    /// immediately. When ON, every skill write is staged for approval instead.
    pub fn set_stage_writes(&self, on: bool) {
        self.stage_writes.store(on, Ordering::SeqCst);
    }

    pub fn staging_enabled(&self) -> bool {
        self.stage_writes.load(Ordering::SeqCst)
    }

    /// Type-level de-dupe lock per skill, so `create`+`patch` on one skill in a
    /// batch never interleave with another writer.
    fn skill_lock(&self, name: &str) -> std::sync::Arc<Mutex<()>> {
        // Keyed on the basename digest-equivalent: Python hashes the basename so
        // `foo` and `category/foo` share one lock.
        let key = Path::new(name)
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| name.to_string());
        let mut locks = self.locks.lock();
        locks.entry(key).or_default().clone()
    }

    /// Every known skills root (the local dir plus the create-dir override).
    fn skills_roots(&self) -> Vec<PathBuf> {
        let mut roots = vec![self.skills_dir.clone()];
        if let Some(extra) = &self.create_dir {
            if !roots.contains(extra) {
                roots.push(extra.clone());
            }
        }
        roots
    }

    /// Root the new-skill dir resolves under (honours `skills.create_dir`).
    fn create_root(&self) -> &Path {
        self.create_dir.as_deref().unwrap_or(&self.skills_dir)
    }

    /// Find a skill directory by bare name or `category/name` relative path.
    pub fn find_skill(&self, name: &str) -> Option<PathBuf> {
        let wants_relative = name.contains('/') || name.contains('\\');
        for root in self.skills_roots() {
            if !root.exists() {
                continue;
            }
            for skill_dir in iter_skill_dirs(&root) {
                if skill_dir
                    .file_name()
                    .map(|s| s.to_string_lossy() == name)
                    .unwrap_or(false)
                {
                    return Some(skill_dir);
                }
                if wants_relative {
                    let resolved = skill_dir
                        .canonicalize()
                        .unwrap_or_else(|_| skill_dir.clone());
                    let root_resolved = root.canonicalize().unwrap_or_else(|_| root.clone());
                    if let Ok(rel) = resolved.strip_prefix(&root_resolved) {
                        if rel.to_string_lossy().replace('\\', "/") == name {
                            return Some(skill_dir);
                        }
                    }
                }
            }
        }
        None
    }

    /// Not-found error naming other profiles that hold the skill.
    fn skill_not_found_error(&self, name: &str, suffix: &str) -> String {
        let base = format!("Skill '{}' not found.", name);
        let others = self.find_skill_in_other_profiles(name);
        let mut msg = base;
        match others.len() {
            0 => msg.push_str(" Use skills_list() to see available skills."),
            1 => {
                let (profile, path) = &others[0];
                msg.push_str(&format!(
                    " A skill by that name exists in profile '{}' ({}). To edit it, switch \
                     profiles (`hermes -p {}`) or edit the file directly (file tools / terminal).",
                    profile,
                    path.display(),
                    profile
                ));
            }
            _ => {
                let names: Vec<String> = others.iter().map(|(p, _)| format!("'{}'", p)).collect();
                msg.push_str(&format!(
                    " Skills by that name exist in other profiles: {}. Switch profiles (`hermes \
                     -p <name>`) to edit there, or edit the files directly (file tools / terminal).",
                    names.join(", ")
                ));
            }
        }
        msg.push_str(suffix);
        msg
    }

    /// `(profile, skill_dir)` pairs for OTHER profiles holding `name`.
    fn find_skill_in_other_profiles(&self, name: &str) -> Vec<(String, PathBuf)> {
        let mut matches = Vec::new();
        let Some(root) = self.skills_dir.parent() else {
            return matches;
        };
        // `${HERMES_HOME}/profiles/<profile>/skills`
        let profiles_root = root.join("profiles");
        if !profiles_root.is_dir() {
            return matches;
        }
        let active = self
            .skills_dir
            .canonicalize()
            .unwrap_or_else(|_| self.skills_dir.clone());
        let Ok(entries) = std::fs::read_dir(&profiles_root) else {
            return matches;
        };
        for entry in entries.flatten() {
            if !entry.path().is_dir() {
                continue;
            }
            let candidate = entry.path().join("skills");
            let resolved = candidate
                .canonicalize()
                .unwrap_or_else(|_| candidate.clone());
            if resolved == active || !candidate.is_dir() {
                continue;
            }
            if let Some(hit) = iter_skill_dirs(&candidate).into_iter().find(|d| {
                d.file_name()
                    .map(|s| s.to_string_lossy() == name)
                    .unwrap_or(false)
            }) {
                matches.push((entry.file_name().to_string_lossy().to_string(), hit));
            }
        }
        matches
    }

    /// Find the skill and run the background-review write guard.
    fn locate_for_write(
        &self,
        name: &str,
        action: &str,
        not_found_suffix: &str,
        pinned: &HashSet<String>,
    ) -> Result<PathBuf, Value> {
        let Some(skill_dir) = self.find_skill(name) else {
            return Err(refusal(&self.skill_not_found_error(name, not_found_suffix)));
        };
        let is_pinned = pinned.contains(name) || self.essential_skills.contains(name);
        if let Some(guard) = background_review_write_guard(
            name,
            action,
            is_pinned,
            false, // external dirs are not modelled as a separate root here
            false,
            false,
            false,
            true,
            true,
        ) {
            return Err(guard);
        }
        Ok(skill_dir)
    }

    /// Atomic write (temp + rename) preserving an existing file's mode.
    fn atomic_write(target: &Path, content: &str) -> std::io::Result<()> {
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = target.with_file_name(format!(
            ".{}.tmp",
            target
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| "skill".to_string())
        ));
        std::fs::write(&tmp, content.as_bytes())?;
        std::fs::rename(&tmp, target)
    }

    /// Read-before-write guard (existing targets only), atomic write, then the
    /// optional security scan; a blocked scan restores the original.
    fn guarded_write(
        &self,
        name: &str,
        target: &Path,
        action: &str,
        label: &str,
        content: &str,
    ) -> Option<Value> {
        let original = if target.exists() {
            if let Some(guard) =
                background_review_read_before_write_guard(name, target, action, label)
            {
                return Some(guard);
            }
            std::fs::read_to_string(target).ok()
        } else {
            None
        };
        if let Err(e) = Self::atomic_write(target, content) {
            return Some(refusal(&format!(
                "Failed to write {}: {}",
                target.display(),
                e
            )));
        }
        // Post-write security scan is opt-in (`skills.guard_agent_created`).
        if !self.guard_agent_created {
            return None;
        }
        // The Rust port has no scanner yet; opt-in is off by default so this is
        // unreachable in the default configuration. When enabled without a
        // scanner, fail closed rather than silently skipping the check.
        if original.is_none() {
            let _ = std::fs::remove_file(target);
        }
        Some(refusal(
            "Agent-created skill security scan is enabled but no scanner is configured; the write \
             was rolled back.",
        ))
    }

    // ── Actions ────────────────────────────────────────────────────────────

    /// `create`: write a new `<skills>[/category]/<name>/SKILL.md`.
    pub fn create(
        &self,
        name: &str,
        content: &str,
        category: Option<&str>,
    ) -> Result<Value, Value> {
        if let Some(err) = validate_name(name)
            .or_else(|| validate_category(category))
            .or_else(|| validate_frontmatter(content, true))
            .or_else(|| validate_content_size(content, "SKILL.md"))
        {
            return Err(refusal(&err));
        }
        if let Some(existing) = self.find_skill(name) {
            return Err(refusal(&format!(
                "A skill named '{}' already exists at {}.",
                name,
                existing.display()
            )));
        }
        let skill_dir = self
            .create_root()
            .join(category.unwrap_or(""))
            .join(name);
        let skill_md = skill_dir.join("SKILL.md");
        if let Err(e) = Self::atomic_write(&skill_md, content) {
            return Err(refusal(&format!("Failed to write {}: {}", skill_md.display(), e)));
        }
        let root = self.skills_dir.clone();
        let display = skill_dir
            .strip_prefix(&root)
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|_| skill_dir.clone());
        let mut result = Map::new();
        result.insert("success".into(), json!(true));
        result.insert("message".into(), json!(format!("Skill '{}' created.", name)));
        result.insert("path".into(), json!(display.to_string_lossy()));
        result.insert("skill_md".into(), json!(skill_md.to_string_lossy()));
        if let Some(cat) = category {
            result.insert("category".into(), json!(cat));
        }
        result.insert(
            "hint".into(),
            json!(format!(
                "To add reference files, templates, or scripts, use \
                 skill_manage(action='write_file', name='{}', \
                 file_path='references/example.md', file_content='...')",
                name
            )),
        );
        result.insert(
            "_change".into(),
            json!({ "description": description_preview(content) }),
        );
        Ok(Value::Object(result))
    }

    /// `edit`: replace the SKILL.md of an existing skill (full rewrite).
    pub fn edit(&self, name: &str, content: &str, pinned: &HashSet<String>) -> Result<Value, Value> {
        if let Some(err) =
            validate_frontmatter(content, false).or_else(|| validate_content_size(content, "SKILL.md"))
        {
            return Err(refusal(&err));
        }
        let skill_dir = self.locate_for_write(name, "edit", "", pinned)?;
        if let Some(guard) = self.guarded_write(name, &skill_dir.join("SKILL.md"), "edit", "SKILL.md", content)
        {
            return Err(guard);
        }
        Ok(json!({
            "success": true,
            "message": format!("Skill '{}' updated (full rewrite).", name),
            "path": skill_dir.to_string_lossy(),
        }))
    }

    /// `patch`: targeted find-and-replace in SKILL.md (default) or a supporting
    /// file; unique match unless `replace_all`.
    pub fn patch(
        &self,
        name: &str,
        old_string: &str,
        new_string: &str,
        file_path: Option<&str>,
        replace_all: bool,
        pinned: &HashSet<String>,
    ) -> Result<Value, Value> {
        if old_string.is_empty() {
            return Err(refusal(PATCH_NEEDS_OLD_STRING));
        }
        let skill_dir = self.locate_for_write(name, "patch", "", pinned)?;
        let target_label = file_path.unwrap_or("SKILL.md");
        let target = if let Some(fp) = file_path {
            self.resolve_supporting_file(&skill_dir, fp)?
        } else {
            skill_dir.join("SKILL.md")
        };
        if !target.exists() {
            let rel = target.strip_prefix(&skill_dir).unwrap_or(&target);
            return Err(refusal(&format!("File not found: {}", rel.display())));
        }
        if let Some(guard) =
            background_review_read_before_write_guard(name, &target, "patch", target_label)
        {
            return Err(guard);
        }
        let content = std::fs::read_to_string(&target)
            .map_err(|e| refusal(&format!("Failed to read {}: {}", target.display(), e)))?;
        // Same fuzzy engine as the file patch tool, so minor formatting
        // mismatches do not fail.
        let (new_content, match_count, _strategy, match_error) =
            crate::fuzzy_match::fuzzy_find_and_replace(&content, old_string, new_string, replace_all);
        if let Some(err) = match_error {
            return Err(refusal_with(
                &err,
                json!({ "file_preview": clip(&content, 500, "...") }),
            ));
        }
        if let Some(err) = validate_content_size(&new_content, target_label) {
            return Err(refusal(&err));
        }
        if file_path.is_none() {
            if let Some(err) = validate_frontmatter(&new_content, false) {
                return Err(refusal(&format!(
                    "Patch would break SKILL.md structure: {}",
                    err
                )));
            }
        }
        if let Some(guard) =
            self.guarded_write(name, &target, "patch", target_label, &new_content)
        {
            return Err(guard);
        }
        Ok(json!({
            "success": true,
            "message": format!(
                "Patched {} in skill '{}' ({} replacement{}).",
                target_label, name, match_count,
                if match_count > 1 { "s" } else { "" }
            ),
        }))
    }

    /// `delete`: remove a skill dir (hard delete). `absorbed_into` records a
    /// curator consolidation.
    pub fn delete(
        &self,
        name: &str,
        absorbed_into: Option<&str>,
        pinned: &HashSet<String>,
    ) -> Result<Value, Value> {
        let skill_dir = self.locate_for_write(name, "delete", "", pinned)?;
        if let Some(guard) = curator_consolidation_delete_guard(name, absorbed_into) {
            return Err(guard);
        }
        if let Some(msg) = pinned_guard(name, &self.essential_skills, pinned) {
            return Err(refusal(&msg));
        }
        let absorbed_target = absorbed_into.map(|s| s.trim().to_string()).unwrap_or_default();
        if !absorbed_target.is_empty() {
            if absorbed_target == name {
                return Err(refusal(&format!(
                    "absorbed_into='{}' cannot equal the skill being deleted.",
                    absorbed_target
                )));
            }
            if self.find_skill(&absorbed_target).is_none() {
                return Err(refusal(&format!(
                    "absorbed_into='{}' does not exist. Create or patch the umbrella skill first, \
                     then retry the delete.",
                    absorbed_target
                )));
            }
        }
        if let Some(unsafe_msg) = validate_delete_target(&skill_dir, &self.skills_roots()) {
            return Err(refusal(&unsafe_msg));
        }
        let absorbed_note = if absorbed_target.is_empty() {
            String::new()
        } else {
            format!(" Content absorbed into '{}'.", absorbed_target)
        };
        if let Err(e) = std::fs::remove_dir_all(&skill_dir) {
            return Err(refusal(&format!(
                "Failed to delete '{}': {}",
                skill_dir.display(),
                e
            )));
        }
        if let Some(parent) = skill_dir.parent() {
            rmdir_if_empty(parent, &self.skills_dir);
        }
        Ok(json!({
            "success": true,
            "message": format!("Skill '{}' deleted.{}", name, absorbed_note),
        }))
    }

    /// `write_file`: add or overwrite a supporting file within a skill directory.
    pub fn write_file(
        &self,
        name: &str,
        file_path: &str,
        file_content: &str,
        pinned: &HashSet<String>,
    ) -> Result<Value, Value> {
        if let Some(err) = validate_file_path(file_path) {
            return Err(refusal(&err));
        }
        let content_bytes = file_content.len();
        if content_bytes > MAX_SKILL_FILE_BYTES {
            return Err(refusal(&format!(
                "File content is {} bytes (limit: {} bytes / 1 MiB). Consider splitting into \
                 smaller files.",
                thousands(content_bytes),
                thousands(MAX_SKILL_FILE_BYTES)
            )));
        }
        if let Some(err) = validate_content_size(file_content, file_path) {
            return Err(refusal(&err));
        }
        let skill_dir = self.locate_for_write(
            name,
            "write_file",
            " Create it first with action='create'.",
            pinned,
        )?;
        let target = self.resolve_supporting_file(&skill_dir, file_path)?;
        if let Some(guard) =
            self.guarded_write(name, &target, "write_file", file_path, file_content)
        {
            return Err(guard);
        }
        Ok(json!({
            "success": true,
            "message": format!("File '{}' written to skill '{}'.", file_path, name),
            "path": target.to_string_lossy(),
        }))
    }

    /// `remove_file`: remove a supporting file from a skill directory.
    pub fn remove_file(
        &self,
        name: &str,
        file_path: &str,
        pinned: &HashSet<String>,
    ) -> Result<Value, Value> {
        if let Some(err) = validate_file_path(file_path) {
            return Err(refusal(&err));
        }
        let skill_dir = self.locate_for_write(name, "remove_file", "", pinned)?;
        let target = self.resolve_supporting_file(&skill_dir, file_path)?;
        if !target.exists() {
            // List what IS there so the model can pick the right path.
            let available: Vec<String> = ALLOWED_SUBDIRS
                .iter()
                .map(|sub| skill_dir.join(sub))
                .filter(|p| p.exists())
                .flat_map(|p| collect_files(&p, &skill_dir))
                .collect();
            return Err(refusal_with(
                &format!("File '{}' not found in skill '{}'.", file_path, name),
                json!({ "available_files": if available.is_empty() { Value::Null } else { json!(available) } }),
            ));
        }
        if let Some(guard) =
            background_review_read_before_write_guard(name, &target, "remove_file", file_path)
        {
            return Err(guard);
        }
        if let Err(e) = std::fs::remove_file(&target) {
            return Err(refusal(&format!("Failed to remove {}: {}", target.display(), e)));
        }
        if let Some(parent) = target.parent() {
            rmdir_if_empty(parent, &skill_dir);
        }
        Ok(json!({
            "success": true,
            "message": format!("File '{}' removed from skill '{}'.", file_path, name),
        }))
    }

    /// Validate `file_path` and resolve it inside `skill_dir`.
    fn resolve_supporting_file(&self, skill_dir: &Path, file_path: &str) -> Result<PathBuf, Value> {
        let target = skill_dir.join(file_path);
        if let Some(err) = validate_file_path(file_path) {
            return Err(refusal(&err));
        }
        if let Some(err) = validate_within_dir(&target, skill_dir) {
            return Err(refusal(&err));
        }
        Ok(target)
    }

    // ── Batch ──────────────────────────────────────────────────────────────

    /// Apply operations atomically: every touched skill is snapshotted first and
    /// any failure rolls ALL of them back. `delete` is only legal as the sole op.
    pub fn apply_batch(
        &self,
        operations: &[Value],
        default_name: Option<&str>,
        pinned: &HashSet<String>,
    ) -> Value {
        if operations.is_empty() {
            return refusal("operations must be a non-empty array.");
        }
        if operations.len() > BATCH_MAX_OPS {
            return refusal(&format!(
                "operations is capped at {} ops per call.",
                BATCH_MAX_OPS
            ));
        }
        let del_ops: Vec<&Value> = operations
            .iter()
            .filter(|op| op.get("action").and_then(|v| v.as_str()) == Some("delete"))
            .collect();
        if !del_ops.is_empty() {
            if operations.len() != 1 {
                return refusal(
                    "delete must be the SOLE op in its call — it doesn't compose with other ops' \
                     rollback.",
                );
            }
            let name = del_ops[0]
                .get("name")
                .and_then(|v| v.as_str())
                .or(default_name);
            let Some(name) = name else {
                return refusal("operations[0] (delete) needs a 'name'.");
            };
            let absorbed = del_ops[0].get("absorbed_into").and_then(|v| v.as_str());
            return match self.delete(name, absorbed, pinned) {
                Ok(v) => v,
                Err(e) => e,
            };
        }

        // ── Shape validation (no side effects) ──
        let mut names: Vec<String> = Vec::new();
        for (i, op) in operations.iter().enumerate() {
            let Some(act) = op.get("action").and_then(|v| v.as_str()) else {
                return refusal(&format!("operations[{}] needs an 'action'.", i));
            };
            if !BATCH_OP_ACTIONS.contains(&act) {
                return refusal(&format!(
                    "operations[{}]: unknown action '{}'. Batchable: {}; delete must be sole.",
                    i,
                    act,
                    BATCH_OP_ACTIONS.join(", ")
                ));
            }
            let nm = op
                .get("name")
                .and_then(|v| v.as_str())
                .or(default_name)
                .unwrap_or("")
                .to_string();
            if nm.is_empty() {
                return refusal(&format!(
                    "operations[{}] needs a 'name' (the skill it targets).",
                    i
                ));
            }
            if let Some(shape_err) = op_shape_error(act, op) {
                return refusal(&format!(
                    "operations[{}] ({} on '{}'): {}",
                    i, act, nm, shape_err
                ));
            }
            if act == "create" && names.contains(&nm) {
                return refusal(&format!(
                    "operations[{}]: create for '{}' must precede that skill's other ops.",
                    i, nm
                ));
            }
            names.push(nm);
        }

        // ── Clobber guard: a destructive op on a file an earlier op touched would
        // SILENTLY discard its work. Additive patches are always legal. ──
        let mut touched: HashSet<(String, String)> = HashSet::new();
        for (i, op) in operations.iter().enumerate() {
            let act = op.get("action").and_then(|v| v.as_str()).unwrap_or("");
            let nm = &names[i];
            let full_rewrite = act == "patch" && op.get("content").and_then(|v| v.as_str()).is_some();
            let fp = op.get("file_path").and_then(|v| v.as_str()).unwrap_or("").trim();
            let target = if act == "create" || full_rewrite || fp.is_empty() {
                "SKILL.md".to_string()
            } else {
                posix_normpath(fp.trim_start_matches('/'))
            };
            let key = (nm.clone(), target.clone());
            if (act == "create" || act == "write_file" || act == "remove_file" || full_rewrite)
                && touched.contains(&key)
            {
                return refusal(&format!(
                    "operations[{}]: {} on '{}' of skill '{}' — an earlier op in this batch already \
                     touched that file, and this op would silently discard its work. One \
                     destructive op (write_file/remove_file/full rewrite) per file per batch; put \
                     it first, or fold the change in. Patch chains are fine.",
                    i, act, target, nm
                ));
            }
            touched.insert(key);
        }

        // ── Snapshot every touched skill, then apply. Any failure rolls back all. ──
        let unique_names: Vec<String> = {
            let mut seen = HashSet::new();
            names
                .iter()
                .filter(|n| seen.insert((*n).clone()))
                .cloned()
                .collect()
        };

        let _locks: Vec<_> = unique_names
            .iter()
            .map(|n| self.skill_lock(n))
            .collect();
        let _guards: Vec<_> = _locks.iter().map(|l| l.lock()).collect();

        // snapshot: name -> (pre_dir, Some(snapshot_dir))
        let snap_root = std::env::temp_dir().join(format!("skill_batch_{}", uuid::Uuid::new_v4()));
        if let Err(e) = std::fs::create_dir_all(&snap_root) {
            return refusal(&format!("Could not create batch snapshot dir: {}", e));
        }
        let mut snapshots: HashMap<String, (Option<PathBuf>, Option<PathBuf>)> = HashMap::new();
        for nm in &unique_names {
            let pre_dir = self.find_skill(nm);
            let snap = pre_dir.as_ref().map(|_| snap_root.join(nm));
            if let (Some(from), Some(to)) = (&pre_dir, &snap) {
                if let Err(e) = copy_dir(from, to) {
                    let _ = std::fs::remove_dir_all(&snap_root);
                    return refusal(&format!(
                        "Could not snapshot '{}' for atomic batch: {}",
                        nm, e
                    ));
                }
            }
            snapshots.insert(nm.clone(), (pre_dir, snap));
        }

        let mut results: Vec<Value> = Vec::new();
        for (i, op) in operations.iter().enumerate() {
            let act = op.get("action").and_then(|v| v.as_str()).unwrap_or("");
            let nm = names[i].clone();
            let outcome = self.dispatch_one(act, &nm, op, pinned);
            match outcome {
                Ok(v) => {
                    results.push(json!({
                        "name": nm,
                        "action": act,
                        "file_path": op.get("file_path").cloned().unwrap_or(Value::Null),
                        "success": true,
                    }));
                    let _ = v;
                }
                Err(e) => {
                    let (note, rollback_failed) = self.rollback(&snapshots);
                    let err_msg = e
                        .get("error")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown error")
                        .to_string();
                    let mut fail = Map::new();
                    fail.insert("success".into(), json!(false));
                    fail.insert(
                        "error".into(),
                        json!(format!(
                            "operations[{}] ({} on '{}') failed: {} — batch aborted, {}.",
                            i, act, nm, err_msg, note
                        )),
                    );
                    fail.insert("failed_index".into(), json!(i));
                    fail.insert("completed_before_failure".into(), json!(i));
                    if !rollback_failed {
                        let _ = std::fs::remove_dir_all(&snap_root);
                    }
                    return Value::Object(fail);
                }
            }
        }
        let _ = std::fs::remove_dir_all(&snap_root);
        json!({
            "success": true,
            "operations_applied": results.len(),
            "results": results,
        })
    }

    /// Dispatch one op through the flat handlers (used by the batch path).
    fn dispatch_one(
        &self,
        action: &str,
        name: &str,
        op: &Value,
        pinned: &HashSet<String>,
    ) -> Result<Value, Value> {
        let get = |k: &str| op.get(k).and_then(|v| v.as_str());
        match action {
            "create" => self.create(name, get("content").unwrap_or(""), get("category")),
            "edit" => self.edit(name, get("content").unwrap_or(""), pinned),
            "patch" => {
                if let Some(content) = get("content") {
                    return self.edit(name, content, pinned);
                }
                self.patch(
                    name,
                    get("old_string").unwrap_or(""),
                    get("new_string").unwrap_or(""),
                    get("file_path"),
                    op.get("replace_all").and_then(|v| v.as_bool()).unwrap_or(false),
                    pinned,
                )
            }
            "write_file" => self.write_file(
                name,
                get("file_path").unwrap_or(""),
                get("file_content").unwrap_or(""),
                pinned,
            ),
            "remove_file" => self.remove_file(name, get("file_path").unwrap_or(""), pinned),
            other => Err(refusal(&format!("Unknown action '{}'.", other))),
        }
    }

    /// Restore every snapshot. Returns `(note, failed)`.
    fn rollback(
        &self,
        snapshots: &HashMap<String, (Option<PathBuf>, Option<PathBuf>)>,
    ) -> (String, bool) {
        let mut notes: Vec<String> = Vec::new();
        for (nm, (pre_dir, snap)) in snapshots {
            let post = self.find_skill(nm);
            if let Err(e) = restore_snapshot(pre_dir.as_deref(), snap.as_deref(), post.as_deref()) {
                notes.push(format!(
                    "ROLLBACK FAILED for '{}' ({}){}",
                    nm,
                    e,
                    snap
                        .as_ref()
                        .map(|s| format!("; snapshot preserved at '{}'", s.display()))
                        .unwrap_or_default()
                ));
            }
        }
        if notes.is_empty() {
            ("all touched skills rolled back".to_string(), false)
        } else {
            (notes.join("; "), true)
        }
    }

    // ── Staging (write-approval gate) ──────────────────────────────────────

    /// Shared write gate. Returns `Some(response)` when the write must NOT proceed
    /// (blocked or staged), `None` to proceed. Skills are too large to review
    /// inline, so they always stage regardless of origin.
    pub fn run_write_gate(&self, action: &str, _name: &str, gist: &str, payload: Value) -> Option<Value> {
        if gate_bypassed() || !ALL_ACTIONS.contains(&action) {
            return None;
        }
        if !self.staging_enabled() {
            return None; // gate OFF by default: the write proceeds.
        }
        self.stage_write(payload, gist)
    }

    fn stage_write(&self, payload: Value, gist: &str) -> Option<Value> {
        let pending_id = uuid::Uuid::new_v4().to_string();
        self.staged.lock().push(json!({
            "id": pending_id,
            "payload": payload,
            "summary": gist,
        }));
        Some(json!({
            "success": true,
            "staged": true,
            "pending_id": pending_id,
            "gist": gist,
            "message": format!("Staged for approval: {}", gist),
        }))
    }

    // ── Flat dispatch ──────────────────────────────────────────────────────

    /// Tool entry point. Dispatch to the action handler -> JSON string.
    /// `operations` (atomic batch shape) overrides the flat fields.
    #[allow(clippy::too_many_arguments)]
    pub fn skill_manage(
        &self,
        action: Option<&str>,
        name: &str,
        content: Option<&str>,
        category: Option<&str>,
        file_path: Option<&str>,
        file_content: Option<&str>,
        old_string: Option<&str>,
        new_string: Option<&str>,
        replace_all: bool,
        absorbed_into: Option<&str>,
        operations: Option<&Vec<Value>>,
        pinned: &HashSet<String>,
    ) -> String {
        if let Some(ops) = operations {
            return self.apply_batch(ops, Some(name), pinned).to_string();
        }
        let action = action.unwrap_or("");
        if !ALL_ACTIONS.contains(&action) {
            return refusal(&format!(
                "Unknown action '{}'. Use: create, edit, patch, delete, write_file, remove_file",
                action
            ))
            .to_string();
        }
        // Argument-shape errors for patch, before the gate so a malformed op is
        // rejected now rather than at approve time.
        if action == "patch" {
            let args = json!({
                "content": content,
                "old_string": old_string,
                "new_string": new_string,
            });
            if let Some(shape_err) = op_shape_error("patch", &args) {
                return refusal(&shape_err).to_string();
            }
        }
        // Approval gate: always stage (bypassed when replaying an approved write).
        let gist = format!("{}: {}", action, name);
        let payload = json!({
            "action": action,
            "name": name,
            "content": content,
            "category": category,
            "file_path": file_path,
            "file_content": file_content,
            "old_string": old_string,
            "new_string": new_string,
            "replace_all": replace_all,
            "absorbed_into": absorbed_into,
        });
        if let Some(staged) = self.run_write_gate(action, name, &gist, payload) {
            return staged.to_string();
        }
        if let Some(err) = validate_name(if action == "create" || name.is_empty() {
            name
        } else {
            Path::new(name)
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or(name)
        }) {
            return refusal(&err).to_string();
        }
        let _lock = self.skill_lock(name);
        let _guard = _lock.lock();
        let outcome = match action {
            "create" => self.create(name, content.unwrap_or(""), category),
            "edit" => self.edit(name, content.unwrap_or(""), pinned),
            "patch" => {
                if let Some(c) = content {
                    // content alone = full rewrite (the old 'edit').
                    self.edit(name, c, pinned)
                } else {
                    self.patch(
                        name,
                        old_string.unwrap_or(""),
                        new_string.unwrap_or(""),
                        file_path,
                        replace_all,
                        pinned,
                    )
                }
            }
            "delete" => self.delete(name, absorbed_into, pinned),
            "write_file" => self.write_file(
                name,
                file_path.unwrap_or(""),
                file_content.unwrap_or(""),
                pinned,
            ),
            "remove_file" => self.remove_file(name, file_path.unwrap_or(""), pinned),
            _ => Err(refusal(&format!("Unknown action '{}'.", action))),
        };
        match outcome {
            Ok(v) => v.to_string(),
            Err(e) => e.to_string(),
        }
    }
}

// ── Free helpers ───────────────────────────────────────────────────────────

const PATCH_NEEDS_OLD_STRING: &str =
    "old_string is required for 'patch' and must be the EXACT text currently in the file. Read \
     the target file first (read_file on the skill's SKILL.md, or the file named by file_path) \
     and copy the snippet verbatim, then retry 'patch'. Do NOT fall back to action='write_file' \
     — that rewrites the entire file and destroys unrelated content.";

const PATCH_NEEDS_NEW_STRING: &str =
    "new_string is required for 'patch'. Use an empty string to delete matched text.";

const PATCH_EITHER_OR: &str =
    "Pass EITHER content (full SKILL.md rewrite) OR old_string/new_string (targeted replacement), \
     not both.";

/// `action -> (text slots it reads, where misfiled text belongs)`.
fn text_slot_for(action: &str) -> Option<(&'static [&'static str], &'static str)> {
    match action {
        "create" | "edit" => Some((&["content"], "'content'")),
        "patch" => Some((
            &["content", "new_string"],
            "old_string/new_string (targeted) or 'content' (full rewrite, last resort)",
        )),
        "write_file" => Some((&["file_content"], "'file_content'")),
        _ => None,
    }
}

/// Sentence naming the text-slot key(s) this op carries that `action` never reads.
fn misplaced_text_hint(action: &str, op: &Value) -> String {
    let Some((reads, destination)) = text_slot_for(action) else {
        return String::new();
    };
    let mut stray: Vec<(&str, &str)> = Vec::new();
    for (key, owner) in [
        ("content", "create (and a full-rewrite patch)"),
        ("new_string", "a targeted patch (with old_string)"),
        ("file_content", "write_file"),
    ] {
        if !reads.contains(&key) && op.get(key).map(|v| !v.is_null()).unwrap_or(false) {
            stray.push((key, owner));
        }
    }
    if stray.is_empty() {
        return String::new();
    }
    let carried = stray
        .iter()
        .map(|(k, o)| format!("'{}' (that key is for {})", k, o))
        .collect::<Vec<_>>()
        .join(" and ");
    format!(
        " Note: this op carries {} — move that text to {}.",
        carried, destination
    )
}

/// Argument-shape error text for one op, or `None`. Only shape misses carry the
/// misplaced-text hint: a patch whose real problem is an unmatched old_string
/// must not be steered to a full rewrite.
fn op_shape_error(action: &str, op: &Value) -> Option<String> {
    let missing = |k: &str| op.get(k).map(|v| v.is_null() || v.as_str() == Some("")).unwrap_or(true);
    let is_none = |k: &str| op.get(k).map(|v| v.is_null()).unwrap_or(true);
    match action {
        "create" | "edit" => {
            if missing("content") {
                return Some(
                    format!(
                        "content is required for '{}'. Provide the full SKILL.md text \
                         (frontmatter + body).",
                        action
                    ) + &misplaced_text_hint(action, op),
                );
            }
        }
        "write_file" => {
            if missing("file_path") {
                return Some(
                    "file_path is required for 'write_file'. Example: 'references/api-guide.md'"
                        .to_string()
                        + &misplaced_text_hint(action, op),
                );
            }
            if is_none("file_content") {
                return Some(
                    "file_content is required for 'write_file'.".to_string()
                        + &misplaced_text_hint(action, op),
                );
            }
        }
        "remove_file" => {
            if missing("file_path") {
                return Some("file_path is required for 'remove_file'.".to_string());
            }
        }
        "patch" => {
            let has_content = op
                .get("content")
                .and_then(|v| v.as_str())
                .map(|s| !s.is_empty())
                .unwrap_or(false);
            let has_old = op
                .get("old_string")
                .and_then(|v| v.as_str())
                .map(|s| !s.is_empty())
                .unwrap_or(false);
            let new_present = op
                .get("new_string")
                .map(|v| !v.is_null())
                .unwrap_or(false);
            if has_content && (has_old || new_present) {
                return Some(PATCH_EITHER_OR.to_string());
            }
            if !has_old && !has_content {
                return Some(PATCH_NEEDS_OLD_STRING.to_string() + &misplaced_text_hint(action, op));
            }
            if !has_content && !new_present {
                return Some(PATCH_NEEDS_NEW_STRING.to_string());
            }
        }
        _ => {}
    }
    None
}

/// All `SKILL.md` parents under `root`, excluding hidden/internal dirs.
fn iter_skill_dirs(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(ft) = entry.file_type() else { continue };
            if !ft.is_dir() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            // Excluded: hidden dirs, the lock dir, archive, and node_modules.
            if name.starts_with('.') || name == "node_modules" {
                continue;
            }
            if path.join("SKILL.md").is_file() {
                out.push(path.clone());
            }
            stack.push(path);
        }
    }
    out
}

/// Files under `dir`, as paths relative to `base`.
fn collect_files(dir: &Path, base: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if let Ok(rel) = path.strip_prefix(base) {
                out.push(rel.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    out.sort();
    out
}

fn rmdir_if_empty(parent: &Path, stop: &Path) {
    if parent == stop || !parent.exists() {
        return;
    }
    let empty = std::fs::read_dir(parent)
        .map(|mut it| it.next().is_none())
        .unwrap_or(false);
    if empty {
        let _ = std::fs::remove_dir(parent);
    }
}

fn clip(text: &str, n: usize, ellipsis: &str) -> String {
    let count = text.chars().count();
    let head: String = text.chars().take(n).collect();
    if count > n {
        format!("{}{}", head, ellipsis)
    } else {
        head
    }
}

/// Recursively copy `from` into `to`.
fn copy_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let src = entry.path();
        let dst = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&src, &dst)?;
        } else {
            std::fs::copy(&src, &dst)?;
        }
    }
    Ok(())
}

/// Restore one snapshot. Moves the broken state aside and deletes it only after
/// the snapshot is back, so a failed copy cannot mean total loss.
fn restore_snapshot(
    pre_dir: Option<&Path>,
    snap: Option<&Path>,
    post_dir: Option<&Path>,
) -> std::io::Result<()> {
    let post_exists = post_dir.map(|p| p.is_dir()).unwrap_or(false);
    match snap {
        None => {
            if let (Some(post), true) = (post_dir, post_exists) {
                std::fs::remove_dir_all(post)?;
            }
            Ok(())
        }
        Some(snap) => {
            let pre = pre_dir.ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::Other, "missing pre-dir for snapshot")
            })?;
            if !post_exists {
                return copy_dir(snap, pre);
            }
            let post = post_dir.unwrap();
            let aside = post.with_file_name(format!(
                "{}.rollback-broken",
                post.file_name().unwrap_or_default().to_string_lossy()
            ));
            let _ = std::fs::remove_dir_all(&aside);
            std::fs::rename(post, &aside)?;
            match copy_dir(snap, pre) {
                Ok(()) => {
                    let _ = std::fs::remove_dir_all(&aside);
                    Ok(())
                }
                Err(e) => {
                    // Restore failed: put the half-applied state back rather than nothing.
                    let _ = std::fs::remove_dir_all(pre);
                    let _ = std::fs::rename(&aside, pre);
                    Err(e)
                }
            }
        }
    }
}

/// Tool definition for `skill_manage`.
pub fn get_definition(skills_dir_display: &str) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": "skill_manage",
            "description": format!(
                "Create, update, or delete skills — your procedural memory for recurring task \
                 types. The call is an operations array (a single edit is a list of one); it \
                 applies atomically — any failure rolls every touched skill back. Ops: create \
                 (full SKILL.md; lands in {}; must precede that skill's other ops), patch \
                 (targeted old_string/new_string fix — preferred; content alone REPLACES the \
                 whole file, read it via skill_view() first), write_file/remove_file (supporting \
                 files), delete (sole op only). Existing skills are modified wherever they live. \
                 Keep the description's first 57 chars a self-contained trigger: 'Use when \
                 <trigger>. <one-line behavior>.' Write lessons, not logs: imperative rule + why, \
                 no PR numbers/dates/incident narration, one rule per lesson, references/ named \
                 by topic (extend before adding). skill_view() shows format conventions.",
                skills_dir_display
            ),
            "parameters": {
                "type": "object",
                "properties": {
                    "operations": {
                        "type": "array",
                        "description": "Ordered ops; each names its target skill (lowercase, hyphens/underscores, max 64 chars). Each action is its own shape; another action's text slot is invalid.",
                        "items": {
                            "anyOf": [
                                {
                                    "type": "object",
                                    "additionalProperties": false,
                                    "properties": {
                                        "name": { "type": "string" },
                                        "action": { "type": "string", "enum": ["create"] },
                                        "content": { "type": "string", "description": "Full SKILL.md text (YAML frontmatter + markdown body)." },
                                        "category": { "type": "string", "description": "Optional category subdir (e.g. 'devops')." }
                                    },
                                    "required": ["name", "action", "content"]
                                },
                                {
                                    "type": "object",
                                    "additionalProperties": false,
                                    "properties": {
                                        "name": { "type": "string" },
                                        "action": { "type": "string", "enum": ["patch"] },
                                        "old_string": { "type": "string", "description": "Text to find (same matching semantics as the patch tool)." },
                                        "new_string": { "type": "string", "description": "Replacement; empty string deletes the match." },
                                        "replace_all": { "type": "boolean", "description": "Replace all occurrences (default false)." },
                                        "file_path": { "type": "string", "description": "Optional supporting file (write_file's shape); default SKILL.md." }
                                    },
                                    "required": ["name", "action", "old_string", "new_string"]
                                },
                                {
                                    "type": "object",
                                    "additionalProperties": false,
                                    "properties": {
                                        "name": { "type": "string" },
                                        "action": { "type": "string", "enum": ["patch"] },
                                        "content": { "type": "string", "description": "Full SKILL.md rewrite (REPLACES the whole file; last resort)." }
                                    },
                                    "required": ["name", "action", "content"]
                                },
                                {
                                    "type": "object",
                                    "additionalProperties": false,
                                    "properties": {
                                        "name": { "type": "string" },
                                        "action": { "type": "string", "enum": ["write_file"] },
                                        "file_path": { "type": "string", "description": "Path RELATIVE to the skill's own directory, e.g. 'references/api.md' — no leading slash, never absolute; first segment references/, templates/, scripts/, or assets/." },
                                        "file_content": { "type": "string", "description": "Full text of the supporting file." }
                                    },
                                    "required": ["name", "action", "file_path", "file_content"]
                                },
                                {
                                    "type": "object",
                                    "additionalProperties": false,
                                    "properties": {
                                        "name": { "type": "string" },
                                        "action": { "type": "string", "enum": ["remove_file"] },
                                        "file_path": { "type": "string", "description": "Supporting file (write_file's shape)." }
                                    },
                                    "required": ["name", "action", "file_path"]
                                },
                                {
                                    "type": "object",
                                    "additionalProperties": false,
                                    "properties": {
                                        "name": { "type": "string" },
                                        "action": { "type": "string", "enum": ["delete"] },
                                        "absorbed_into": { "type": "string", "description": "Curator consolidation only: umbrella skill that absorbed this one (must exist)." }
                                    },
                                    "required": ["name", "action"]
                                }
                            ]
                        }
                    }
                },
                "required": ["operations"]
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &str = "---\nname: demo-skill\ndescription: Use when demo. Does the demo.\n---\n\n# Body\n\nStep one.\n";

    fn temp_manager() -> (SkillManager, PathBuf) {
        let dir = std::env::temp_dir().join(format!("hermes_skills_{}", uuid::Uuid::new_v4()));
        let root = dir.join("skills");
        std::fs::create_dir_all(&root).unwrap();
        (SkillManager::new(root), dir)
    }

    fn no_pins() -> HashSet<String> {
        HashSet::new()
    }

    // ── Validation ────────────────────────────────────────────────────────
    #[test]
    fn validate_name_accepts_and_rejects() {
        assert!(validate_name("demo-skill").is_none());
        assert!(validate_name("mlops.axolotl_v2").is_none());
        assert!(validate_name("").is_some());
        assert!(validate_name("Demo").is_some()); // uppercase
        assert!(validate_name("-lead").is_some()); // must start alnum
        assert!(validate_name(&"a".repeat(MAX_NAME_LENGTH + 1)).is_some());
    }

    #[test]
    fn validate_category_accepts_absent_and_rejects_paths() {
        assert!(validate_category(None).is_none());
        assert!(validate_category(Some("  ")).is_none());
        assert!(validate_category(Some("devops")).is_none());
        assert!(validate_category(Some("a/b")).is_some());
        assert!(validate_category(Some("a\\b")).is_some());
    }

    #[test]
    fn validate_frontmatter_required_fields_and_body() {
        assert!(validate_frontmatter(VALID, false).is_none());
        assert!(validate_frontmatter("", false).is_some());
        assert!(validate_frontmatter("no frontmatter", false).is_some());
        // Missing description.
        let no_desc = "---\nname: demo-skill\n---\n\nBody\n";
        assert!(validate_frontmatter(no_desc, false).is_some());
        // Unclosed frontmatter.
        let unclosed = "---\nname: demo-skill\ndescription: x\n\nBody\n";
        assert!(validate_frontmatter(unclosed, false).is_some());
        // No body after frontmatter.
        let no_body = "---\nname: demo-skill\ndescription: x\n---\n\n";
        assert!(validate_frontmatter(no_body, false).is_some());
    }

    #[test]
    fn new_skill_enforces_prompt_desc_limit_but_edit_does_not() {
        let long = "x".repeat(SKILL_PROMPT_DESC_LIMIT + 10);
        let content = format!("---\nname: demo-skill\ndescription: {}\n---\n\nBody\n", long);
        assert!(validate_frontmatter(&content, true).is_some());
        assert!(validate_frontmatter(&content, false).is_none());
    }

    #[test]
    fn validate_file_path_rules() {
        assert!(validate_file_path("references/api.md").is_none());
        assert!(validate_file_path("SKILL.md").is_none());
        assert!(validate_file_path("demo-skill/SKILL.md").is_none());
        assert!(validate_file_path("").is_some());
        assert!(validate_file_path("../escape.md").is_some());
        assert!(validate_file_path("references/../escape.md").is_some());
        assert!(validate_file_path("secrets/key.md").is_some());
        assert!(validate_file_path("references").is_some()); // dir only
    }

    #[test]
    fn posix_normpath_matches_python_shape() {
        assert_eq!(posix_normpath("/references/api.md"), "references/api.md");
        assert_eq!(posix_normpath("references/./api.md"), "references/api.md");
        assert_eq!(posix_normpath("./references/api.md"), "references/api.md");
    }

    // ── Actions ───────────────────────────────────────────────────────────
    #[test]
    fn create_then_find() {
        let (mgr, _d) = temp_manager();
        let out = mgr.create("demo-skill", VALID, None).unwrap();
        assert_eq!(out["success"], json!(true));
        assert!(out["message"].as_str().unwrap().contains("demo-skill"));
        let found = mgr.find_skill("demo-skill").unwrap();
        assert!(found.join("SKILL.md").exists());
    }

    #[test]
    fn create_with_category_nests_dir() {
        let (mgr, _d) = temp_manager();
        let out = mgr.create("demo-skill", VALID, Some("devops")).unwrap();
        assert_eq!(out["category"], json!("devops"));
        let found = mgr.find_skill("demo-skill").unwrap();
        assert!(found.ends_with("devops/demo-skill"));
        // Categorized relative lookup works too.
        assert!(mgr.find_skill("devops/demo-skill").is_some());
    }

    #[test]
    fn create_duplicate_is_refused() {
        let (mgr, _d) = temp_manager();
        mgr.create("demo-skill", VALID, None).unwrap();
        let err = mgr.create("demo-skill", VALID, None).unwrap_err();
        assert!(err["error"].as_str().unwrap().contains("already exists"));
    }

    #[test]
    fn create_rejects_bad_frontmatter() {
        let (mgr, _d) = temp_manager();
        let err = mgr.create("demo-skill", "not frontmatter", None).unwrap_err();
        assert_eq!(err["success"], json!(false));
        assert!(!mgr.skills_dir().join("demo-skill").exists());
    }

    #[test]
    fn edit_full_rewrite() {
        let (mgr, _d) = temp_manager();
        mgr.create("demo-skill", VALID, None).unwrap();
        let updated = VALID.replace("Step one.", "Step two.");
        let out = mgr.edit("demo-skill", &updated, &no_pins()).unwrap();
        assert!(out["message"].as_str().unwrap().contains("full rewrite"));
        let on_disk = std::fs::read_to_string(mgr.find_skill("demo-skill").unwrap().join("SKILL.md")).unwrap();
        assert!(on_disk.contains("Step two."));
    }

    #[test]
    fn patch_targeted_replacement() {
        let (mgr, _d) = temp_manager();
        mgr.create("demo-skill", VALID, None).unwrap();
        let out = mgr
            .patch("demo-skill", "Step one.", "Step one, revised.", None, false, &no_pins())
            .unwrap();
        assert!(out["message"].as_str().unwrap().contains("1 replacement"));
        let on_disk = std::fs::read_to_string(mgr.find_skill("demo-skill").unwrap().join("SKILL.md")).unwrap();
        assert!(on_disk.contains("Step one, revised."));
    }

    #[test]
    fn patch_missing_old_string_teaches_recovery() {
        let (mgr, _d) = temp_manager();
        mgr.create("demo-skill", VALID, None).unwrap();
        let err = mgr
            .patch("demo-skill", "not in the file", "x", None, false, &no_pins())
            .unwrap_err();
        assert_eq!(err["success"], json!(false));
        assert!(err["file_preview"].is_string());
    }

    #[test]
    fn patch_empty_old_string_refused() {
        let (mgr, _d) = temp_manager();
        mgr.create("demo-skill", VALID, None).unwrap();
        let err = mgr.patch("demo-skill", "", "x", None, false, &no_pins()).unwrap_err();
        assert!(err["error"].as_str().unwrap().contains("old_string is required"));
    }

    #[test]
    fn write_file_then_remove_file() {
        let (mgr, _d) = temp_manager();
        mgr.create("demo-skill", VALID, None).unwrap();
        let out = mgr
            .write_file("demo-skill", "references/api.md", "# API\n", &no_pins())
            .unwrap();
        assert!(out["message"].as_str().unwrap().contains("references/api.md"));
        let dir = mgr.find_skill("demo-skill").unwrap();
        assert!(dir.join("references/api.md").exists());

        let out = mgr.remove_file("demo-skill", "references/api.md", &no_pins()).unwrap();
        assert!(out["message"].as_str().unwrap().contains("removed"));
        assert!(!dir.join("references/api.md").exists());
        // Empty subdir is cleaned up.
        assert!(!dir.join("references").exists());
    }

    #[test]
    fn write_file_rejects_traversal() {
        let (mgr, _d) = temp_manager();
        mgr.create("demo-skill", VALID, None).unwrap();
        let err = mgr
            .write_file("demo-skill", "../escape.md", "x", &no_pins())
            .unwrap_err();
        assert!(err["error"].as_str().unwrap().contains("Path traversal"));
    }

    #[test]
    fn remove_file_missing_lists_available() {
        let (mgr, _d) = temp_manager();
        mgr.create("demo-skill", VALID, None).unwrap();
        mgr.write_file("demo-skill", "references/one.md", "1", &no_pins()).unwrap();
        let err = mgr
            .remove_file("demo-skill", "references/two.md", &no_pins())
            .unwrap_err();
        let avail = err["available_files"].as_array().unwrap();
        assert!(avail.iter().any(|v| v == "references/one.md"));
    }

    #[test]
    fn delete_removes_skill_and_empty_category() {
        let (mgr, _d) = temp_manager();
        mgr.create("demo-skill", VALID, Some("devops")).unwrap();
        let out = mgr.delete("demo-skill", None, &no_pins()).unwrap();
        assert!(out["message"].as_str().unwrap().contains("deleted"));
        assert!(mgr.find_skill("demo-skill").is_none());
        assert!(!mgr.skills_dir().join("devops").exists());
    }

    #[test]
    fn delete_unknown_skill_is_refused() {
        let (mgr, _d) = temp_manager();
        let err = mgr.delete("nope", None, &no_pins()).unwrap_err();
        assert!(err["error"].as_str().unwrap().contains("not found"));
    }

    #[test]
    fn delete_absorbed_into_must_exist() {
        let (mgr, _d) = temp_manager();
        mgr.create("demo-skill", VALID, None).unwrap();
        let err = mgr.delete("demo-skill", Some("umbrella"), &no_pins()).unwrap_err();
        assert!(err["error"].as_str().unwrap().contains("does not exist"));
        // Self-reference refused.
        let err = mgr.delete("demo-skill", Some("demo-skill"), &no_pins()).unwrap_err();
        assert!(err["error"].as_str().unwrap().contains("cannot equal"));
    }

    // ── Delete guards ─────────────────────────────────────────────────────
    #[test]
    fn delete_pinned_skill_blocked_but_patch_allowed() {
        let (mgr, _d) = temp_manager();
        mgr.create("demo-skill", VALID, None).unwrap();
        let mut pinned = HashSet::new();
        pinned.insert("demo-skill".to_string());
        let err = mgr.delete("demo-skill", None, &pinned).unwrap_err();
        assert!(err["error"].as_str().unwrap().contains("pinned"));
        // Patches stay allowed on pinned skills.
        assert!(mgr.patch("demo-skill", "Step one.", "Step 1.", None, false, &pinned).is_ok());
    }

    #[test]
    fn delete_essential_skill_blocked() {
        let (mgr, _d) = temp_manager();
        let mgr = mgr.with_essential_skills(["demo-skill".to_string()]);
        mgr.create("demo-skill", VALID, None).unwrap();
        let err = mgr.delete("demo-skill", None, &no_pins()).unwrap_err();
        assert!(err["error"].as_str().unwrap().contains("essential"));
    }

    #[cfg(unix)]
    #[test]
    fn delete_symlinked_skill_dir_is_refused() {
        let (mgr, d) = temp_manager();
        // A poisoned tree can hand the delete path a symlinked directory; the
        // guard must refuse so a recursive delete cannot follow the link out.
        let outside = d.join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("keep.txt"), "do not touch").unwrap();
        let link = mgr.skills_dir().join("demo-skill");
        std::os::unix::fs::symlink(&outside, &link).unwrap();

        let err = validate_delete_target(&link, &mgr.skills_roots()).unwrap();
        assert!(err.contains("symlink"), "{}", err);
        // Nothing was removed.
        assert!(outside.join("keep.txt").exists());
    }

    #[test]
    fn delete_outside_any_skills_root_is_refused() {
        let (mgr, d) = temp_manager();
        let stray = d.join("stray-skill");
        std::fs::create_dir_all(&stray).unwrap();
        std::fs::write(stray.join("SKILL.md"), VALID).unwrap();
        let err = validate_delete_target(&stray, &mgr.skills_roots()).unwrap();
        assert!(err.contains("does not resolve inside"), "{}", err);
        assert!(stray.join("SKILL.md").exists());
    }

    #[test]
    fn validate_delete_target_rejects_root_itself() {
        let (mgr, _d) = temp_manager();
        let err = validate_delete_target(mgr.skills_dir(), &mgr.skills_roots()).unwrap();
        assert!(err.contains("skills root itself"));
    }

    // ── Drift/staging gates ───────────────────────────────────────────────
    #[test]
    fn write_gate_is_off_by_default_and_stages_when_on() {
        let (mgr, _d) = temp_manager();
        assert!(!mgr.staging_enabled());
        // Off: the write applies.
        let out = mgr.create("demo-skill", VALID, None).unwrap();
        assert_eq!(out["success"], json!(true));

        // On: the write is staged, not applied.
        mgr.set_stage_writes(true);
        let payload = json!({"action": "delete", "name": "demo-skill"});
        let staged = mgr.run_write_gate("delete", "demo-skill", "delete: demo-skill", payload).unwrap();
        assert_eq!(staged["staged"], json!(true));
        assert!(staged["pending_id"].is_string());
        assert_eq!(mgr.staged_writes().len(), 1);
    }

    #[test]
    fn background_review_blocks_unread_write() {
        let (mgr, _d) = temp_manager();
        mgr.create("demo-skill", VALID, None).unwrap();
        set_background_review(true);
        reset_background_review_read_marks();
        let err = mgr
            .patch("demo-skill", "Step one.", "x", None, false, &no_pins())
            .unwrap_err();
        set_background_review(false);
        assert_eq!(err["_read_before_write_required"], json!(true));
        assert!(err["error"].as_str().unwrap().contains("has not been loaded"));
    }

    #[test]
    fn background_review_allows_after_read_mark() {
        let (mgr, _d) = temp_manager();
        mgr.create("demo-skill", VALID, None).unwrap();
        let target = mgr.find_skill("demo-skill").unwrap().join("SKILL.md");
        set_background_review(true);
        reset_background_review_read_marks();
        mark_background_review_skill_read(&target);
        let ok = mgr.patch("demo-skill", "Step one.", "Step 1.", None, false, &no_pins());
        set_background_review(false);
        assert!(ok.is_ok(), "{:?}", ok.err());
    }

    #[test]
    fn background_delete_requires_absorbed_into() {
        let (mgr, _d) = temp_manager();
        mgr.create("demo-skill", VALID, None).unwrap();
        set_background_review(true);
        // Bare delete during the consolidation pass: fail closed.
        let guard = curator_consolidation_delete_guard("demo-skill", None).unwrap();
        set_background_review(false);
        assert_eq!(guard["_fail_closed"], json!(true));
        // Declared consolidation passes the guard.
        assert!(curator_consolidation_delete_guard("demo-skill", Some("umbrella")).is_none());
    }

    // ── Batch semantics ───────────────────────────────────────────────────
    #[test]
    fn batch_create_then_patch_atomic() {
        let (mgr, _d) = temp_manager();
        let ops = vec![
            json!({"action": "create", "name": "demo-skill", "content": VALID}),
            json!({"action": "patch", "name": "demo-skill", "old_string": "Step one.", "new_string": "Step 1."}),
        ];
        let out = mgr.apply_batch(&ops, None, &no_pins());
        assert_eq!(out["success"], json!(true));
        assert_eq!(out["operations_applied"], json!(2));
        let on_disk = std::fs::read_to_string(mgr.find_skill("demo-skill").unwrap().join("SKILL.md")).unwrap();
        assert!(on_disk.contains("Step 1."));
    }

    #[test]
    fn batch_rolls_back_on_failure() {
        let (mgr, _d) = temp_manager();
        mgr.create("demo-skill", VALID, None).unwrap();
        // Second op fails (old_string not present) -> the first op must be undone.
        let ops = vec![
            json!({"action": "patch", "name": "demo-skill", "old_string": "Step one.", "new_string": "Step 1."}),
            json!({"action": "patch", "name": "demo-skill", "old_string": "absent text", "new_string": "x"}),
        ];
        let out = mgr.apply_batch(&ops, None, &no_pins());
        assert_eq!(out["success"], json!(false));
        assert_eq!(out["failed_index"], json!(1));
        assert!(out["error"].as_str().unwrap().contains("batch aborted"));
        // Rolled back: the first patch's change is gone.
        let on_disk = std::fs::read_to_string(mgr.find_skill("demo-skill").unwrap().join("SKILL.md")).unwrap();
        assert!(on_disk.contains("Step one."));
        assert!(!on_disk.contains("Step 1."));
    }

    #[test]
    fn batch_delete_must_be_sole_op() {
        let (mgr, _d) = temp_manager();
        mgr.create("demo-skill", VALID, None).unwrap();
        let ops = vec![
            json!({"action": "delete", "name": "demo-skill"}),
            json!({"action": "create", "name": "other", "content": VALID}),
        ];
        let out = mgr.apply_batch(&ops, None, &no_pins());
        assert_eq!(out["success"], json!(false));
        assert!(out["error"].as_str().unwrap().contains("SOLE op"));
    }

    #[test]
    fn batch_clobber_guard_refuses_second_destructive_op() {
        let (mgr, _d) = temp_manager();
        mgr.create("demo-skill", VALID, None).unwrap();
        let ops = vec![
            json!({"action": "write_file", "name": "demo-skill", "file_path": "references/a.md", "file_content": "1"}),
            json!({"action": "write_file", "name": "demo-skill", "file_path": "references/a.md", "file_content": "2"}),
        ];
        let out = mgr.apply_batch(&ops, None, &no_pins());
        assert_eq!(out["success"], json!(false));
        assert!(out["error"].as_str().unwrap().contains("already touched"));
    }

    #[test]
    fn batch_caps_op_count() {
        let (mgr, _d) = temp_manager();
        let ops: Vec<Value> = (0..=BATCH_MAX_OPS)
            .map(|i| json!({"action": "create", "name": format!("s{}", i), "content": VALID}))
            .collect();
        let out = mgr.apply_batch(&ops, None, &no_pins());
        assert_eq!(out["success"], json!(false));
        assert!(out["error"].as_str().unwrap().contains("capped"));
    }

    #[test]
    fn batch_empty_refused() {
        let (mgr, _d) = temp_manager();
        let out = mgr.apply_batch(&[], None, &no_pins());
        assert_eq!(out["success"], json!(false));
        assert!(out["error"].as_str().unwrap().contains("non-empty"));
    }

    #[test]
    fn batch_create_must_precede_other_ops() {
        let (mgr, _d) = temp_manager();
        let ops = vec![
            json!({"action": "write_file", "name": "demo-skill", "file_path": "references/a.md", "file_content": "1"}),
            json!({"action": "create", "name": "demo-skill", "content": VALID}),
        ];
        // remove_file/write_file on a non-existent skill fails shape validation first,
        // so use two creates to isolate the precedence rule.
        let dup = vec![
            json!({"action": "create", "name": "demo-skill", "content": VALID}),
            json!({"action": "create", "name": "demo-skill", "content": VALID}),
        ];
        let out = mgr.apply_batch(&dup, None, &no_pins());
        assert_eq!(out["success"], json!(false));
        assert!(out["error"].as_str().unwrap().contains("must precede"));
        let _ = ops;
    }

    #[test]
    fn batch_shape_error_names_misfiled_text_slot() {
        let (mgr, _d) = temp_manager();
        // `content` on a write_file op is the classic misfiled text slot.
        let ops = vec![json!({"action": "write_file", "name": "demo-skill", "content": "oops"})];
        let out = mgr.apply_batch(&ops, None, &no_pins());
        assert_eq!(out["success"], json!(false));
        let msg = out["error"].as_str().unwrap();
        assert!(msg.contains("file_path is required"));
        assert!(msg.contains("that key is for"));
    }

    #[test]
    fn flat_patch_shape_error_before_any_effect() {
        let (mgr, _d) = temp_manager();
        let out = mgr.skill_manage(
            Some("patch"), "demo-skill", Some("full rewrite"), None, None, None,
            Some("old"), None, false, None, None, &no_pins(),
        );
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["success"], json!(false));
        assert!(v["error"].as_str().unwrap().contains("EITHER content"));
    }

    #[test]
    fn flat_unknown_action_refused() {
        let (mgr, _d) = temp_manager();
        let out = mgr.skill_manage(
            Some("frobnicate"), "demo-skill", None, None, None, None, None, None,
            false, None, None, &no_pins(),
        );
        let v: Value = serde_json::from_str(&out).unwrap();
        assert!(v["error"].as_str().unwrap().contains("Unknown action"));
    }
}
