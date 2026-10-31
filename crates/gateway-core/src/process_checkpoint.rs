//! Running-process checkpoint persistence and PID-safe recovery.
//!
//! Layer 3 of the Rust port of `tools/process_registry_checkpoint.py`. A crash-recovery
//! JSON file records every live, host-scoped process so the gateway can adopt it after a
//! restart as a `detached` session (status + kill work; output history is gone).
//!
//! Two hazards drive the design, both preserved from Python:
//! - **PID recycling.** Across a restart the kernel may reuse a stored PID for an
//!   unrelated process. Adoption requires the recorded kernel start time to still match;
//!   a bare liveness check would let a later kill tree-kill a stranger (a browser).
//! - **Credentials in the command string.** The checkpoint lives in the profile home, so
//!   the command is redacted before it lands on disk. Recovery only displays it; adoption
//!   re-validates the PID and never re-runs the command, so masking is lossless.
//!
//! File I/O sits behind [`CheckpointStore`] so the whole flow is testable in memory.

use std::sync::Arc;

use serde_json::{json, Value};

use crate::process::{
    PidScope, ProcessSession, SessionHandle, WATCHER_ROUTE_KEYS,
};

/// Session fields persisted verbatim in the checkpoint (plus `session_id`; `command` is
/// redacted and `owner_task_id` defaulted on write).
pub const CHECKPOINT_FIELDS: [&str; 27] = [
    "command",
    "pid",
    "pid_scope",
    "host_start_time",
    "systemd_unit",
    "cwd",
    "started_at",
    "task_id",
    "owner_task_id",
    "session_key",
    "watcher_platform",
    "watcher_chat_id",
    "watcher_user_id",
    "watcher_user_name",
    "watcher_thread_id",
    "watcher_message_id",
    "watcher_interval",
    "parent_session_id",
    "notify_on_complete",
    "completion_output_chars",
    "watch_patterns",
    "heartbeat_seconds",
    // The remaining fields are written for fidelity even though recovery ignores them.
    "total_output_chars",
    "detached",
    "exited",
    "completion_reason",
    "termination_source",
];

// ── seams ──────────────────────────────────────────────────────────────────────

/// Reads/writes the checkpoint file. The production impl writes atomically (temp file +
/// fsync + rename) so a crash mid-write cannot corrupt the previous checkpoint.
pub trait CheckpointStore: Send + Sync {
    /// Read the raw checkpoint contents, or `None` when absent/unreadable.
    fn read(&self) -> Option<String>;
    /// Write the checkpoint atomically. Errors are the caller's to log, not to raise.
    fn write(&self, contents: &str) -> Result<(), String>;
    /// Path for diagnostics.
    fn path(&self) -> String;
}

/// In-memory checkpoint store for tests.
#[derive(Debug, Default)]
pub struct MemoryCheckpointStore {
    contents: parking_lot::Mutex<Option<String>>,
    pub writes: parking_lot::Mutex<u32>,
}

impl MemoryCheckpointStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Seed an initial checkpoint payload.
    pub fn with_contents(contents: impl Into<String>) -> Self {
        Self {
            contents: parking_lot::Mutex::new(Some(contents.into())),
            writes: parking_lot::Mutex::new(0),
        }
    }

    pub fn contents(&self) -> Option<String> {
        self.contents.lock().clone()
    }
}

impl CheckpointStore for MemoryCheckpointStore {
    fn read(&self) -> Option<String> {
        self.contents.lock().clone()
    }
    fn write(&self, contents: &str) -> Result<(), String> {
        *self.contents.lock() = Some(contents.to_string());
        *self.writes.lock() += 1;
        Ok(())
    }
    fn path(&self) -> String {
        ":memory:".to_string()
    }
}

/// Atomic JSON file store. Mirrors `utils.atomic_json_write`: a temp file in the same
/// directory, fsync, then rename over the target.
#[derive(Debug)]
pub struct FileCheckpointStore {
    path: std::path::PathBuf,
}

impl FileCheckpointStore {
    pub fn new(path: impl Into<std::path::PathBuf>) -> Self {
        Self { path: path.into() }
    }
}

impl CheckpointStore for FileCheckpointStore {
    fn read(&self) -> Option<String> {
        std::fs::read_to_string(&self.path).ok()
    }

    fn write(&self, contents: &str) -> Result<(), String> {
        use std::io::Write;
        let dir = self
            .path
            .parent()
            .ok_or_else(|| "checkpoint path has no parent directory".to_string())?;
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        let tmp = dir.join(format!(
            ".{}.{}.tmp",
            self.path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("processes.json"),
            std::process::id()
        ));
        {
            let mut f = std::fs::File::create(&tmp).map_err(|e| e.to_string())?;
            f.write_all(contents.as_bytes()).map_err(|e| e.to_string())?;
            f.sync_all().map_err(|e| e.to_string())?;
        }
        std::fs::rename(&tmp, &self.path).map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            e.to_string()
        })
    }

    fn path(&self) -> String {
        self.path.display().to_string()
    }
}

/// A command redactor applied before persisting. The production hook masks inline
/// credentials; the default is pass-through.
pub trait CommandRedactor: Send + Sync {
    fn redact(&self, command: &str) -> String;
}

/// Pass-through redactor (no `agent.redact` equivalent wired).
#[derive(Debug, Default)]
pub struct NoRedact;

impl CommandRedactor for NoRedact {
    fn redact(&self, command: &str) -> String {
        command.to_string()
    }
}

/// Kernel start ticks for a PID, or `None` when unavailable. The production impl reads
/// `/proc/<pid>/stat` field 22.
pub trait PidProbe: Send + Sync {
    fn is_alive(&self, pid: i32) -> bool;
    fn start_time(&self, pid: i32) -> Option<i64>;
}

/// Production PID probe: liveness via signal 0, start time via `/proc`.
#[derive(Debug, Default)]
pub struct SystemPidProbe;

impl PidProbe for SystemPidProbe {
    fn is_alive(&self, pid: i32) -> bool {
        crate::process::pid_is_alive(pid)
    }
    fn start_time(&self, pid: i32) -> Option<i64> {
        host_start_time(pid)
    }
}

/// Kernel start ticks for a PID from `/proc/<pid>/stat`, or `None` off-Linux.
pub fn host_start_time(pid: i32) -> Option<i64> {
    #[cfg(target_os = "linux")]
    {
        // Field 22 of /proc/<pid>/stat. The comm field can contain spaces and
        // parentheses, so split after the LAST ')' and count from there.
        let stat = std::fs::read_to_string(format!("/proc/{}/stat", pid)).ok()?;
        let after_comm = stat.rsplit_once(')')?.1;
        after_comm.split_whitespace().nth(19)?.parse::<i64>().ok()
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        None
    }
}

/// The two checkpoint operations the registry needs.
pub trait CheckpointStoreOps {
    fn write_checkpoint(&self, extra_entries: Vec<Value>);
    fn recover_from_checkpoint(&self) -> usize;
}

// ── build an entry from a live session ─────────────────────────────────────────

/// Build one checkpoint entry for a live session. Returns `None` for an exited one.
///
/// `start_time_lookup` backfills `host_start_time` for sessions spawned before the field
/// existed, so recovery can still detect PID recycling.
pub fn session_to_entry<F>(session: &ProcessSession, mut start_time_lookup: F) -> Option<Value>
where
    F: FnMut(i32) -> Option<i64>,
{
    if session.exited {
        return None;
    }
    // Backfill the start time so recovery can detect PID recycling even for sessions
    // spawned before this field existed.
    let mut host_start_time = session.host_start_time;
    if host_start_time.is_none() && session.pid_scope == PidScope::Host {
        if let Some(pid) = session.pid {
            host_start_time = start_time_lookup(pid);
        }
    }

    let mut entry = json!({
        "session_id": session.id,
        "command": session.command,
        "pid": session.pid,
        "pid_scope": session.pid_scope.as_str(),
        "host_start_time": host_start_time,
        "systemd_unit": session.systemd_unit,
        "cwd": session.cwd,
        "started_at": session.started_at,
        "task_id": session.task_id,
        "owner_task_id": if session.owner_task_id.is_empty() {
            session.task_id.clone()
        } else {
            session.owner_task_id.clone()
        },
        "session_key": session.session_key,
        "watcher_interval": session.watcher_interval,
        "parent_session_id": session.parent_session_id,
        "notify_on_complete": session.notify_on_complete,
        "completion_output_chars": session.completion_output_chars,
        "watch_patterns": session.watch_patterns,
        "heartbeat_seconds": session.heartbeat_seconds,
        "total_output_chars": session.total_output_chars,
        "detached": session.detached,
        "exited": session.exited,
        "completion_reason": session.completion_reason,
        "termination_source": session.termination_source,
    });
    for key in WATCHER_ROUTE_KEYS {
        entry[format!("watcher_{}", key)] = json!(session.watcher.get(key));
    }
    Some(entry)
}

/// Serialise the given sessions into the checkpoint payload.
///
/// `extra_entries` (unresolved scope cleanups from a previous recovery) are appended for
/// any session id not already tracked, so a failed scope reap is retried next startup.
pub fn build_checkpoint_payload<F>(
    sessions: &[Arc<SessionHandle>],
    extra_entries: &[Value],
    start_time_lookup: F,
    redact: &dyn CommandRedactor,
) -> Vec<Value>
where
    F: FnMut(i32) -> Option<i64>,
{
    let mut lookup = start_time_lookup;
    let mut entries: Vec<Value> = Vec::new();
    for handle in sessions {
        let snapshot = handle.snapshot();
        if let Some(mut entry) = session_to_entry(&snapshot, &mut lookup) {
            // Redact inline credentials before persisting: the checkpoint lives in the
            // profile home and recovery only displays the command.
            entry["command"] = json!(redact.redact(&snapshot.command));
            entries.push(entry);
        }
    }
    if !extra_entries.is_empty() {
        let tracked: std::collections::HashSet<String> = entries
            .iter()
            .filter_map(|e| e.get("session_id").and_then(|v| v.as_str()).map(String::from))
            .collect();
        for item in extra_entries {
            let id = item
                .get("session_id")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            if !tracked.contains(id) {
                entries.push(item.clone());
            }
        }
    }
    entries
}

// ── recovery ───────────────────────────────────────────────────────────────────

/// What followed from probing one checkpoint entry.
#[derive(Debug, PartialEq, Eq)]
pub enum RecoverOutcome {
    /// Adopted as a detached session.
    Recovered,
    /// Skipped: no PID recorded.
    NoPid,
    /// Skipped: an in-sandbox PID means nothing once the env handle is gone.
    NonHostScope,
    /// Skipped: already tracked (the registry is process-global, so a multiplexer
    /// recovering several homes must adopt each session once).
    AlreadyTracked,
    /// Skipped: the PID is gone, or alive but recycled onto a stranger.
    NotOurs,
    /// Skipped, and the persisted scope could not be reaped — the entry is retained so
    /// the next startup retries it.
    ScopeUnresolved,
}

/// One recovery decision, kept pure so the whole matrix is testable.
///
/// `is_alive`/`start_time` probe the live kernel; `stop_unit` reaps a persisted systemd
/// scope and reports whether it succeeded.
pub fn plan_recovery(
    entry: &Value,
    already_tracked: bool,
    pid_is_ours: bool,
    pid_alive: bool,
    mut stop_unit: impl FnMut(&str) -> bool,
) -> RecoverOutcome {
    let pid = entry.get("pid").and_then(|v| v.as_i64());
    if pid.is_none() {
        return RecoverOutcome::NoPid;
    }
    if already_tracked {
        return RecoverOutcome::AlreadyTracked;
    }
    let scope = entry
        .get("pid_scope")
        .and_then(|v| v.as_str())
        .unwrap_or("host");
    if scope != "host" {
        // In-sandbox PIDs mean nothing once the env handle is gone.
        return RecoverOutcome::NonHostScope;
    }
    if pid_is_ours {
        return RecoverOutcome::Recovered;
    }
    let systemd_unit = entry
        .get("systemd_unit")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if !systemd_unit.is_empty() && !stop_unit(systemd_unit) {
        return RecoverOutcome::ScopeUnresolved;
    }
    let _ = pid_alive;
    RecoverOutcome::NotOurs
}

/// A watcher re-enqueue request, produced when a recovered session had one configured so
/// the gateway can resume notifications.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingWatcher {
    pub session_id: String,
    pub check_interval: u64,
    pub session_key: String,
    pub platform: String,
    pub chat_id: String,
    pub user_id: String,
    pub user_name: String,
    pub thread_id: String,
    pub message_id: String,
    pub notify_on_complete: bool,
    pub parent_session_id: String,
}

impl PendingWatcher {
    pub fn to_value(&self) -> Value {
        json!({
            "session_id": self.session_id,
            "check_interval": self.check_interval,
            "session_key": self.session_key,
            "platform": self.platform,
            "chat_id": self.chat_id,
            "user_id": self.user_id,
            "user_name": self.user_name,
            "thread_id": self.thread_id,
            "message_id": self.message_id,
            "notify_on_complete": self.notify_on_complete,
            "parent_session_id": self.parent_session_id,
        })
    }
}

/// Apply defaults for a missing checkpoint field. `watch_patterns` defaults to an empty
/// array, every other listed field to `null`.
pub fn checkpoint_default(field: &str) -> Value {
    if field == "watch_patterns" {
        json!([])
    } else {
        Value::Null
    }
}

/// Rebuild a [`ProcessSession`] from a checkpoint entry as a detached session.
pub fn entry_to_session(entry: &Value, now: f64) -> ProcessSession {
    let mut s = ProcessSession::new(
        entry
            .get("session_id")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown"),
        entry
            .get("command")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown"),
        entry
            .get("started_at")
            .and_then(|v| v.as_f64())
            .unwrap_or(now),
    );
    s.detached = true; // can't read output, but can report status + kill
    s.pid = entry.get("pid").and_then(|v| v.as_i64()).map(|v| v as i32);
    s.pid_scope = match entry.get("pid_scope").and_then(|v| v.as_str()) {
        Some("sandbox") => PidScope::Sandbox,
        _ => PidScope::Host,
    };
    s.host_start_time = entry.get("host_start_time").and_then(|v| v.as_i64());
    s.systemd_unit = str_field(entry, "systemd_unit");
    s.cwd = entry
        .get("cwd")
        .and_then(|v| v.as_str())
        .map(String::from);
    s.task_id = str_field(entry, "task_id");
    s.owner_task_id = {
        let owner = str_field(entry, "owner_task_id");
        if owner.is_empty() {
            s.task_id.clone()
        } else {
            owner
        }
    };
    s.session_key = str_field(entry, "session_key");
    s.watcher_interval = entry
        .get("watcher_interval")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    s.parent_session_id = str_field(entry, "parent_session_id");
    s.notify_on_complete = entry
        .get("notify_on_complete")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    s.completion_output_chars = entry
        .get("completion_output_chars")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as usize;
    s.watch_patterns = entry
        .get("watch_patterns")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    s.heartbeat_seconds = entry
        .get("heartbeat_seconds")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    s.total_output_chars = entry
        .get("total_output_chars")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    for key in WATCHER_ROUTE_KEYS {
        let value = str_field(entry, &format!("watcher_{}", key));
        match key {
            "platform" => s.watcher.platform = value,
            "chat_id" => s.watcher.chat_id = value,
            "user_id" => s.watcher.user_id = value,
            "user_name" => s.watcher.user_name = value,
            "thread_id" => s.watcher.thread_id = value,
            "message_id" => s.watcher.message_id = value,
            _ => {}
        }
    }
    s
}

fn str_field(entry: &Value, key: &str) -> String {
    entry
        .get(key)
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string()
}

/// Build the watcher re-enqueue request for a recovered session, if it had one.
pub fn pending_watcher_for(session: &ProcessSession) -> Option<PendingWatcher> {
    if session.watcher_interval == 0 {
        return None;
    }
    Some(PendingWatcher {
        session_id: session.id.clone(),
        check_interval: session.watcher_interval,
        session_key: session.session_key.clone(),
        platform: session.watcher.platform.clone(),
        chat_id: session.watcher.chat_id.clone(),
        user_id: session.watcher.user_id.clone(),
        user_name: session.watcher.user_name.clone(),
        thread_id: session.watcher.thread_id.clone(),
        message_id: session.watcher.message_id.clone(),
        notify_on_complete: session.notify_on_complete,
        parent_session_id: session.parent_session_id.clone(),
    })
}

/// Parse a checkpoint payload into entries. A malformed file yields no entries, matching
/// Python's silent `return 0`.
pub fn parse_checkpoint(contents: &str) -> Vec<Value> {
    match serde_json::from_str::<Value>(contents) {
        Ok(Value::Array(items)) => items,
        _ => Vec::new(),
    }
}

/// Serialise entries to the checkpoint payload text.
pub fn serialize_checkpoint(entries: &[Value]) -> String {
    serde_json::to_string(entries).unwrap_or_else(|_| "[]".to_string())
}

// ── tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn session(id: &str, pid: Option<i32>) -> ProcessSession {
        let mut s = ProcessSession::new(id, "sleep 999", 1_000.0);
        s.pid = pid;
        s
    }

    // ── entry building ──
    #[test]
    fn exited_sessions_are_not_checkpointed() {
        let mut s = session("proc_a", Some(42));
        s.mark_exited(Some(0), "exited", "");
        assert!(session_to_entry(&s, |_| Some(7)).is_none());
    }

    #[test]
    fn entry_carries_the_core_fields() {
        let mut s = session("proc_a", Some(42));
        s.task_id = "t1".to_string();
        s.session_key = "sk".to_string();
        s.systemd_unit = "hermes-worker-1.scope".to_string();
        s.cwd = Some("/tmp".to_string());
        s.host_start_time = Some(12345);
        let e = session_to_entry(&s, |_| None).unwrap();
        assert_eq!(e["session_id"], "proc_a");
        assert_eq!(e["pid"], 42);
        assert_eq!(e["pid_scope"], "host");
        assert_eq!(e["host_start_time"], 12345);
        assert_eq!(e["systemd_unit"], "hermes-worker-1.scope");
        assert_eq!(e["cwd"], "/tmp");
        assert_eq!(e["task_id"], "t1");
        // owner falls back to task_id when unset.
        assert_eq!(e["owner_task_id"], "t1");
    }

    #[test]
    fn entry_backfills_host_start_time_for_host_pids() {
        let s = session("proc_a", Some(42)); // no host_start_time set
        let e = session_to_entry(&s, |pid| {
            assert_eq!(pid, 42);
            Some(999_111)
        })
        .unwrap();
        assert_eq!(e["host_start_time"], 999_111);
    }

    #[test]
    fn entry_does_not_backfill_for_sandbox_pids() {
        let mut s = session("proc_a", Some(42));
        s.pid_scope = PidScope::Sandbox;
        let e = session_to_entry(&s, |_| panic!("must not probe a sandbox pid")).unwrap();
        assert_eq!(e["host_start_time"], Value::Null);
        assert_eq!(e["pid_scope"], "sandbox");
    }

    #[test]
    fn entry_keeps_an_existing_host_start_time() {
        let mut s = session("proc_a", Some(42));
        s.host_start_time = Some(5);
        let e = session_to_entry(&s, |_| panic!("must not re-probe")).unwrap();
        assert_eq!(e["host_start_time"], 5);
    }

    #[test]
    fn entry_carries_watcher_routing() {
        let mut s = session("proc_a", Some(1));
        s.watcher.platform = "telegram".to_string();
        s.watcher.chat_id = "-100".to_string();
        s.watcher.message_id = "55".to_string();
        s.watcher_interval = 30;
        let e = session_to_entry(&s, |_| None).unwrap();
        assert_eq!(e["watcher_platform"], "telegram");
        assert_eq!(e["watcher_chat_id"], "-100");
        assert_eq!(e["watcher_message_id"], "55");
        assert_eq!(e["watcher_interval"], 30);
    }

    // ── redaction ──
    #[test]
    fn checkpoint_redacts_the_command() {
        struct Masker;
        impl CommandRedactor for Masker {
            fn redact(&self, c: &str) -> String {
                c.replace("secret123", "***")
            }
        }
        let handle = SessionHandle::new(session("proc_a", Some(1)));
        {
            handle.session.lock().command = "curl -H 'X-Key: secret123'".to_string();
        }
        let payload = build_checkpoint_payload(&[handle], &[], |_| None, &Masker);
        assert_eq!(payload[0]["command"], "curl -H 'X-Key: ***'");
    }

    #[test]
    fn default_redactor_is_passthrough() {
        let handle = SessionHandle::new(session("proc_a", Some(1)));
        let payload = build_checkpoint_payload(&[handle], &[], |_| None, &NoRedact);
        assert_eq!(payload[0]["command"], "sleep 999");
    }

    // ── extra entries ──
    #[test]
    fn extra_unresolved_entries_are_appended() {
        let handle = SessionHandle::new(session("proc_a", Some(1)));
        let extras = vec![json!({"session_id": "proc_old", "pid": 9})];
        let payload = build_checkpoint_payload(&[handle], &extras, |_| None, &NoRedact);
        assert_eq!(payload.len(), 2);
        assert!(payload.iter().any(|e| e["session_id"] == "proc_old"));
    }

    #[test]
    fn extra_entries_do_not_duplicate_a_tracked_session() {
        let handle = SessionHandle::new(session("proc_a", Some(1)));
        let extras = vec![json!({"session_id": "proc_a", "pid": 999})];
        let payload = build_checkpoint_payload(&[handle], &extras, |_| None, &NoRedact);
        assert_eq!(payload.len(), 1);
        // The live entry wins, not the stale extra.
        assert_eq!(payload[0]["pid"], 1);
    }

    // ── recovery matrix ──
    fn entry(pid: Option<i64>, scope: &str, unit: &str) -> Value {
        json!({
            "session_id": "proc_r",
            "pid": pid,
            "pid_scope": scope,
            "systemd_unit": unit,
            "host_start_time": 4242,
        })
    }

    #[test]
    fn recovery_skips_entries_without_a_pid() {
        let out = plan_recovery(&entry(None, "host", ""), false, false, false, |_| true);
        assert_eq!(out, RecoverOutcome::NoPid);
    }

    #[test]
    fn recovery_skips_already_tracked_sessions() {
        let out = plan_recovery(&entry(Some(1), "host", ""), true, true, true, |_| true);
        assert_eq!(out, RecoverOutcome::AlreadyTracked);
    }

    #[test]
    fn recovery_skips_non_host_scope() {
        let out = plan_recovery(&entry(Some(1), "sandbox", ""), false, true, true, |_| true);
        assert_eq!(out, RecoverOutcome::NonHostScope);
    }

    #[test]
    fn recovery_adopts_a_matching_live_pid() {
        let out = plan_recovery(&entry(Some(1), "host", ""), false, true, true, |_| true);
        assert_eq!(out, RecoverOutcome::Recovered);
    }

    #[test]
    fn recovery_refuses_a_recycled_pid() {
        // Alive, but the start time no longer matches: the PID was recycled.
        let out = plan_recovery(&entry(Some(1), "host", ""), false, false, true, |_| true);
        assert_eq!(out, RecoverOutcome::NotOurs);
    }

    #[test]
    fn recovery_refuses_a_dead_pid() {
        let out = plan_recovery(&entry(Some(1), "host", ""), false, false, false, |_| true);
        assert_eq!(out, RecoverOutcome::NotOurs);
    }

    #[test]
    fn recovery_retains_the_entry_when_scope_reap_fails() {
        let mut calls = 0;
        let out = plan_recovery(
            &entry(Some(1), "host", "hermes-worker-1.scope"),
            false,
            false,
            true,
            |unit| {
                calls += 1;
                assert_eq!(unit, "hermes-worker-1.scope");
                false // reap failed
            },
        );
        assert_eq!(out, RecoverOutcome::ScopeUnresolved);
        assert_eq!(calls, 1);
    }

    #[test]
    fn recovery_proceeds_when_scope_reap_succeeds() {
        let out = plan_recovery(
            &entry(Some(1), "host", "hermes-worker-1.scope"),
            false,
            false,
            false,
            |_| true,
        );
        assert_eq!(out, RecoverOutcome::NotOurs);
    }

    #[test]
    fn recovery_skips_scope_reap_for_a_matching_pid() {
        // A live, matching process is adopted; its scope must NOT be stopped.
        let out = plan_recovery(
            &entry(Some(1), "host", "hermes-worker-1.scope"),
            false,
            true,
            true,
            |_| panic!("must not stop the scope of a live process we own"),
        );
        assert_eq!(out, RecoverOutcome::Recovered);
    }

    // ── entry -> session ──
    #[test]
    fn entry_to_session_marks_it_detached() {
        let e = entry(Some(77), "host", "");
        let s = entry_to_session(&e, 5_000.0);
        assert!(s.detached);
        assert_eq!(s.id, "proc_r");
        assert_eq!(s.pid, Some(77));
        assert_eq!(s.host_start_time, Some(4242));
        assert_eq!(s.pid_scope, PidScope::Host);
    }

    #[test]
    fn entry_to_session_applies_defaults_for_missing_fields() {
        let e = json!({"session_id": "proc_min", "pid": 3});
        let s = entry_to_session(&e, 7_000.0);
        assert_eq!(s.command, "unknown");
        assert_eq!(s.started_at, 7_000.0);
        assert_eq!(s.task_id, "");
        assert_eq!(s.session_key, "");
        assert_eq!(s.watcher_interval, 0);
        assert!(!s.notify_on_complete);
        assert!(s.watch_patterns.is_empty());
        assert_eq!(s.heartbeat_seconds, 0);
        assert_eq!(s.completion_output_chars, 0);
    }

    #[test]
    fn entry_to_session_restores_watch_patterns_and_watcher() {
        let e = json!({
            "session_id": "proc_w", "pid": 3,
            "watch_patterns": ["ERROR", "READY"],
            "watcher_interval": 30,
            "watcher_platform": "telegram",
            "watcher_thread_id": "21787",
            "notify_on_complete": true,
            "parent_session_id": "sess-1",
        });
        let s = entry_to_session(&e, 1.0);
        assert_eq!(s.watch_patterns, vec!["ERROR", "READY"]);
        assert_eq!(s.watcher_interval, 30);
        assert_eq!(s.watcher.platform, "telegram");
        assert_eq!(s.watcher.thread_id, "21787");
        assert!(s.notify_on_complete);
        assert_eq!(s.parent_session_id, "sess-1");
    }

    #[test]
    fn entry_to_session_defaults_owner_to_task_id() {
        let e = json!({"session_id": "proc_o", "pid": 3, "task_id": "t7"});
        let s = entry_to_session(&e, 1.0);
        assert_eq!(s.owner_task_id, "t7");
    }

    #[test]
    fn entry_to_session_keeps_a_distinct_owner() {
        let e = json!({"session_id": "proc_o", "pid": 3, "task_id": "t7", "owner_task_id": "sa-1"});
        let s = entry_to_session(&e, 1.0);
        assert_eq!(s.owner_task_id, "sa-1");
    }

    #[test]
    fn entry_to_session_restores_sandbox_scope() {
        let e = json!({"session_id": "proc_s", "pid": 3, "pid_scope": "sandbox"});
        assert_eq!(entry_to_session(&e, 1.0).pid_scope, PidScope::Sandbox);
    }

    // ── watcher re-enqueue ──
    #[test]
    fn pending_watcher_only_when_an_interval_is_set() {
        let mut s = session("proc_w", Some(1));
        assert!(pending_watcher_for(&s).is_none());
        s.watcher_interval = 30;
        s.session_key = "sk".to_string();
        s.watcher.platform = "telegram".to_string();
        let p = pending_watcher_for(&s).unwrap();
        assert_eq!(p.session_id, "proc_w");
        assert_eq!(p.check_interval, 30);
        assert_eq!(p.session_key, "sk");
        assert_eq!(p.platform, "telegram");
        assert_eq!(p.to_value()["check_interval"], 30);
    }

    // ── parse / serialize ──
    #[test]
    fn parse_round_trips() {
        let entries = vec![json!({"session_id": "proc_a", "pid": 1})];
        let text = serialize_checkpoint(&entries);
        let back = parse_checkpoint(&text);
        assert_eq!(back.len(), 1);
        assert_eq!(back[0]["session_id"], "proc_a");
    }

    #[test]
    fn parse_tolerates_malformed_and_wrong_shapes() {
        assert!(parse_checkpoint("not json").is_empty());
        assert!(parse_checkpoint("{}").is_empty()); // object, not an array
        assert!(parse_checkpoint("").is_empty());
        assert!(parse_checkpoint("[1,2]").len() == 2); // entries are validated downstream
    }

    // ── file store ──
    #[test]
    fn file_store_round_trips() {
        let dir = std::env::temp_dir().join(format!("hermes-ckpt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("processes.json");
        let store = FileCheckpointStore::new(&path);
        assert!(store.read().is_none());
        store.write("[{\"session_id\":\"proc_a\"}]").unwrap();
        assert_eq!(store.read().unwrap(), "[{\"session_id\":\"proc_a\"}]");
        // No stray temp file left behind.
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temp file must be renamed away");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn file_store_replaces_previous_contents_atomically() {
        let dir = std::env::temp_dir().join(format!("hermes-ckpt2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("processes.json");
        let store = FileCheckpointStore::new(&path);
        store.write("[1]").unwrap();
        store.write("[2]").unwrap();
        assert_eq!(store.read().unwrap(), "[2]");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn memory_store_counts_writes() {
        let store = MemoryCheckpointStore::new();
        assert!(store.read().is_none());
        store.write("[]").unwrap();
        assert_eq!(store.read().unwrap(), "[]");
        assert_eq!(*store.writes.lock(), 1);
    }

    #[test]
    fn checkpoint_defaults() {
        assert_eq!(checkpoint_default("watch_patterns"), json!([]));
        assert_eq!(checkpoint_default("pid"), Value::Null);
    }

    #[test]
    fn checkpoint_fields_cover_the_python_set() {
        // Every field Python persists must be represented, or recovery silently drops it.
        for required in [
            "command", "pid", "pid_scope", "host_start_time", "systemd_unit", "cwd",
            "started_at", "task_id", "owner_task_id", "session_key", "watcher_interval",
            "parent_session_id", "notify_on_complete", "completion_output_chars",
            "watch_patterns", "heartbeat_seconds",
        ] {
            assert!(
                CHECKPOINT_FIELDS.contains(&required),
                "CHECKPOINT_FIELDS is missing {required}"
            );
        }
        for key in WATCHER_ROUTE_KEYS {
            let f: &'static str = CHECKPOINT_FIELDS
                .iter()
                .find(|f| f.ends_with(key))
                .copied()
                .unwrap_or_else(|| panic!("missing watcher field for {key}"));
            assert_eq!(f, format!("watcher_{}", key));
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn host_start_time_reads_this_process() {
        let pid = std::process::id() as i32;
        let st = host_start_time(pid);
        assert!(st.is_some(), "own /proc start time must be readable");
        // Stable across calls in the same process.
        assert_eq!(st, host_start_time(pid));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn host_start_time_is_none_for_a_dead_pid() {
        assert!(host_start_time(999_999_999).is_none());
    }
}
