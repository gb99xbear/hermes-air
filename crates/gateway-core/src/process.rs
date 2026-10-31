//! Process registry — in-memory tracking for background processes spawned via
//! `terminal(background=true)`: rolling output buffer, poll/log/wait/kill, and the
//! `process_manage` tool (9 actions).
//!
//! Layer 1 of the Rust port of `tools/process_registry.py`. Covers the registry core
//! plus every `process_manage` action. The systemd-scope and checkpoint layers land
//! separately, behind the `ScopeHost` / checkpoint seams defined here.
//!
//! Host interaction sits behind traits (`ProcessHost`, `PtyHost`, `ScopeHost`,
//! `InterruptSource`, `Clock`, `ResultTransformer`) so the registry is exercisable
//! without spawning real processes.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;
use serde_json::{json, Value};
use tracing::warn;

// ── constants ──────────────────────────────────────────────────────────────────

/// Rolling output buffer: only this many trailing chars are retained per process.
pub const MAX_OUTPUT_CHARS: usize = 200_000;
/// Tail of the output a completion notification carries.
pub const COMPLETION_OUTPUT_CHARS: usize = 2_000;
/// Keep finished processes around this long before pruning (30 minutes).
pub const FINISHED_TTL_SECONDS: f64 = 1_800.0;
/// Max tracked processes (LRU pruning).
pub const MAX_PROCESSES: usize = 64;
/// Watch-pattern rate limiting, per session.
pub const WATCH_MIN_INTERVAL_SECONDS: f64 = 15.0;
pub const WATCH_STRIKE_LIMIT: u32 = 3;
/// Lifetime cap on watch deliveries, independent of strikes.
pub const WATCH_LIFETIME_MAX_HITS: u32 = 8;
/// Heartbeat floor so a model cannot turn it into a 5-second poll.
pub const HEARTBEAT_MIN_SECONDS: u64 = 60;
pub const HEARTBEAT_OUTPUT_CHARS: usize = 2_000;
pub const HEARTBEAT_TICK_SECONDS: u64 = 5;
/// Global watch circuit breaker, across all sessions.
pub const WATCH_GLOBAL_MAX_PER_WINDOW: u32 = 15;
pub const WATCH_GLOBAL_WINDOW_SECONDS: f64 = 10.0;
pub const WATCH_GLOBAL_COOLDOWN_SECONDS: f64 = 30.0;

/// Fallback wait ceiling when `TERMINAL_TIMEOUT` is unset.
pub const DEFAULT_TERMINAL_TIMEOUT_SECONDS: u64 = 180;
/// `log` default page size.
pub const DEFAULT_LOG_LIMIT: usize = 200;
/// `poll` preview size.
pub const POLL_PREVIEW_CHARS: usize = 1_000;
/// `wait`/interrupted snapshot size.
pub const WAIT_OUTPUT_CHARS: usize = 1_000;
/// `list` per-entry command/output preview sizes.
pub const LIST_COMMAND_CHARS: usize = 200;
pub const LIST_PREVIEW_CHARS: usize = 200;

/// Shell banner noise stripped from process output (mirrors `_SHELL_NOISE_SUBSTRINGS`).
pub const SHELL_NOISE_SUBSTRINGS: [&str; 3] = [
    "no job control in this shell",
    "cannot set terminal process group",
    "tcsetattr: Inappropriate ioctl for device",
];

/// Cap on handoffs a single subagent may perform.
pub const MAX_HANDOFFS_PER_CHILD: usize = 3;

// ── small helpers ──────────────────────────────────────────────────────────────

/// Strip ANSI escape sequences: CSI (`ESC [ ... final`), OSC (`ESC ] ... BEL|ESC \`)
/// and bare two-char escapes. Mirrors `tools.ansi_strip.strip_ansi`.
pub fn strip_ansi(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        match chars.peek().copied() {
            Some('[') => {
                chars.next();
                while let Some(n) = chars.next() {
                    if ('\u{40}'..='\u{7e}').contains(&n) {
                        break;
                    }
                }
            }
            Some(']') => {
                chars.next();
                while let Some(n) = chars.next() {
                    if n == '\u{7}' {
                        break;
                    }
                    if n == '\u{1b}' {
                        if chars.peek() == Some(&'\\') {
                            chars.next();
                        }
                        break;
                    }
                }
            }
            Some(_) => {
                chars.next();
            }
            None => {}
        }
    }
    out
}

/// Last `n` **characters** (not bytes) of `s`.
pub fn tail_chars(s: &str, n: usize) -> String {
    if n == 0 {
        return String::new();
    }
    let count = s.chars().count();
    if count <= n {
        return s.to_string();
    }
    s.chars().skip(count - n).collect()
}

/// First `n` characters of `s`.
pub fn head_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// `"45s"`, `"5m 12s"`, `"2h 5m"` — mirrors `format_uptime_short`.
pub fn format_uptime_short(seconds: i64) -> String {
    let s = seconds.max(0);
    if s < 60 {
        return format!("{}s", s);
    }
    let mins = s / 60;
    let secs = s % 60;
    if mins < 60 {
        return format!("{}m {}s", mins, secs);
    }
    let hours = mins / 60;
    let mins = mins % 60;
    format!("{}h {}m", hours, mins)
}

/// True when a chunk is recognisable shell banner noise.
pub fn is_shell_noise(text: &str) -> bool {
    SHELL_NOISE_SUBSTRINGS.iter().any(|n| text.contains(n))
}

/// Strip shell startup warnings from the beginning of a chunk.
///
/// Mirrors `ProcessRegistry._clean_shell_noise`: it pops LEADING lines that contain a
/// noise substring and leaves the body untouched. It is applied to the FIRST chunk of a
/// reader only — later chunks cannot be startup banners.
pub fn clean_shell_noise(text: &str) -> String {
    let mut lines: Vec<&str> = text.split('\n').collect();
    while let Some(first) = lines.first() {
        if is_shell_noise(first) {
            lines.remove(0);
        } else {
            break;
        }
    }
    lines.join("\n")
}

/// Line ending appended by `submit`. On a Windows PTY, Enter is CRLF: ConPTY treats
/// `\r` as end-of-line and a bare `\n` is NOT a terminator, so a blocking line read
/// in the child never returns and the process hangs looking healthy. POSIX keeps `\n`.
pub fn submit_line_ending(is_windows: bool, is_pty: bool) -> &'static str {
    if is_windows && is_pty {
        "\r\n"
    } else {
        "\n"
    }
}

/// Clamp a `wait` timeout against the configured ceiling.
///
/// Returns `(effective_timeout, clamp_note)`. A `timeout` of 0 is an error and is
/// reported by the caller before this is reached (0 is falsy and would otherwise fall
/// through to the default wait).
pub fn clamp_wait_timeout(timeout: Option<u64>, max_timeout: u64) -> (u64, Option<String>) {
    match timeout {
        Some(t) if t > max_timeout => (
            max_timeout,
            Some(format!(
                "Requested wait of {}s was clamped to configured limit of {}s",
                t, max_timeout
            )),
        ),
        Some(t) => (t, None),
        None => (max_timeout, None),
    }
}

/// Page a log by lines.
///
/// `offset = None` means "last `limit` lines"; an explicit `offset = 0` means the HEAD.
/// Conflating the two via falsiness silently returned the tail when the head was asked
/// for, so they are kept distinct here.
///
/// Returns `(selected_lines, observed_completion_output)`.
pub fn select_log_lines<'a>(
    lines: &'a [&'a str],
    offset: Option<usize>,
    limit: usize,
) -> (Vec<&'a str>, bool) {
    let total = lines.len();
    match offset {
        None if limit > 0 => {
            let start = total.saturating_sub(limit);
            let selected = lines[start..].to_vec();
            let observed = !selected.is_empty() || total == 0;
            (selected, observed)
        }
        other => {
            let off = other.unwrap_or(0);
            let start = off.min(total);
            let stop = (off + limit).min(total);
            let selected = if start < stop { lines[start..stop].to_vec() } else { Vec::new() };
            let observed = total == 0 || (!selected.is_empty() && stop == total);
            (selected, observed)
        }
    }
}

/// `{"status": "not_found", "error": "..."}` — mirrors `_not_found`.
pub fn not_found(session_id: &str) -> Value {
    json!({"status": "not_found", "error": format!("No process with ID {}", session_id)})
}

/// Tool-level error payload. Mirrors `tools.registry.tool_error`.
pub fn tool_error(msg: &str) -> String {
    json!({"error": msg}).to_string()
}

// ── session state ──────────────────────────────────────────────────────────────

/// Where a PID lives: the host, or inside a sandbox environment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PidScope {
    #[default]
    Host,
    Sandbox,
}

impl PidScope {
    pub fn as_str(self) -> &'static str {
        match self {
            PidScope::Host => "host",
            PidScope::Sandbox => "sandbox",
        }
    }
}

/// Watcher routing, persisted for crash recovery as `watcher_<key>`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WatcherRoute {
    pub platform: String,
    pub chat_id: String,
    pub user_id: String,
    pub user_name: String,
    pub thread_id: String,
    pub message_id: String,
}

/// Keys of [`WatcherRoute`], in event-dict order.
pub const WATCHER_ROUTE_KEYS: [&str; 6] =
    ["platform", "chat_id", "user_id", "user_name", "thread_id", "message_id"];

impl WatcherRoute {
    pub fn get(&self, key: &str) -> &str {
        match key {
            "platform" => &self.platform,
            "chat_id" => &self.chat_id,
            "user_id" => &self.user_id,
            "user_name" => &self.user_name,
            "thread_id" => &self.thread_id,
            "message_id" => &self.message_id,
            _ => "",
        }
    }

    pub fn is_empty(&self) -> bool {
        WATCHER_ROUTE_KEYS.iter().all(|k| self.get(k).is_empty())
    }
}

/// A tracked background process with output buffering.
#[derive(Debug, Clone)]
pub struct ProcessSession {
    /// `"proc_xxxxxxxxxxxx"`.
    pub id: String,
    pub command: String,
    /// Task/sandbox isolation key (the CONTAINER key; may be collapsed).
    pub task_id: String,
    /// RAW spawning task id (`"sa-..."`); ownership checks must use this.
    pub owner_task_id: String,
    /// Gateway session key (reset protection).
    pub session_key: String,
    pub pid: Option<i32>,
    pub pid_scope: PidScope,
    pub cwd: Option<String>,
    /// Seconds since epoch of spawn.
    pub started_at: f64,
    /// Kernel start ticks — PID-reuse guard.
    pub host_start_time: Option<i64>,
    pub exited: bool,
    /// Seconds since epoch of the FIRST move to finished (0 = unknown).
    pub exited_at: f64,
    /// `None` while running.
    pub exit_code: Option<i32>,
    /// exited | killed | lost | failed_start | already_exited
    pub completion_reason: String,
    /// process.kill | kill_all | backend_lost | failed_start
    pub termination_source: String,
    /// Rolling tail (last `max_output_chars`).
    pub output_buffer: String,
    pub max_output_chars: usize,
    /// Recovered from checkpoint (no pipe).
    pub detached: bool,
    /// Spawned under a PTY (`use_pty`). Decides stdin routing (PTY vs pipe) and the
    /// Enter sequence `submit` appends, exactly like `session._pty` in Python.
    pub is_pty: bool,
    /// Transient scope unit name when spawned under `systemd-run`.
    pub systemd_unit: String,
    /// Why a subagent handed this process to its parent.
    pub handoff_note: String,
    pub watcher: WatcherRoute,
    /// 0 = no watcher configured.
    pub watcher_interval: u64,
    /// Session-db id of the spawning conversation.
    pub parent_session_id: String,
    pub notify_on_complete: bool,
    /// Output chars the completion carries; 0 = [`COMPLETION_OUTPUT_CHARS`].
    pub completion_output_chars: usize,
    pub watch_patterns: Vec<String>,
    /// 0 = off; else a `heartbeat` event every N seconds while running.
    pub heartbeat_seconds: u64,
    /// Time of the last heartbeat (or spawn); layer 2 drives this.
    pub heartbeat_last: f64,
    /// `total_output_chars` at the last heartbeat, so a beat carries only new output.
    pub heartbeat_total_at_last: u64,
    pub heartbeat_seq: u64,
    /// Chars ever ingested (the buffer is only a rolling tail).
    pub total_output_chars: u64,
    // ── watch bookkeeping (layer 2 drives these) ──
    pub watch_hits: u32,
    pub watch_suppressed: u32,
    pub watch_disabled: bool,
    /// A strike is a WINDOW with drops, not a drop. See WATCH_*.
    pub watch_cooldown_until: f64,
    pub watch_strike_candidate: bool,
    pub watch_consecutive_strikes: u32,
}

impl ProcessSession {
    pub fn new(id: impl Into<String>, command: impl Into<String>, started_at: f64) -> Self {
        Self {
            id: id.into(),
            command: command.into(),
            task_id: String::new(),
            owner_task_id: String::new(),
            session_key: String::new(),
            pid: None,
            pid_scope: PidScope::Host,
            cwd: None,
            started_at,
            host_start_time: None,
            exited: false,
            exited_at: 0.0,
            exit_code: None,
            completion_reason: "exited".to_string(),
            termination_source: String::new(),
            output_buffer: String::new(),
            max_output_chars: MAX_OUTPUT_CHARS,
            detached: false,
            is_pty: false,
            systemd_unit: String::new(),
            handoff_note: String::new(),
            watcher: WatcherRoute::default(),
            watcher_interval: 0,
            parent_session_id: String::new(),
            notify_on_complete: false,
            completion_output_chars: 0,
            watch_patterns: Vec::new(),
            heartbeat_seconds: 0,
            heartbeat_last: 0.0,
            heartbeat_total_at_last: 0,
            heartbeat_seq: 0,
            total_output_chars: 0,
            watch_hits: 0,
            watch_suppressed: 0,
            watch_disabled: false,
            watch_cooldown_until: 0.0,
            watch_strike_candidate: false,
            watch_consecutive_strikes: 0,
        }
    }

    /// Append to the rolling output buffer, keeping the tail.
    pub fn append_output(&mut self, text: &str) {
        self.output_buffer.push_str(text);
        self.total_output_chars += text.chars().count() as u64;
        if self.output_buffer.chars().count() > self.max_output_chars {
            self.output_buffer = tail_chars(&self.output_buffer, self.max_output_chars);
        }
    }

    /// Record an exit. A kill that raced the observer already recorded its own
    /// `exit_code`/`reason`; never overwrite it.
    pub fn mark_exited(&mut self, exit_code: Option<i32>, reason: &str, source: &str) {
        self.exited = true;
        if self.completion_reason != "killed" {
            self.exit_code = exit_code;
            self.completion_reason = reason.to_string();
            if !source.is_empty() {
                self.termination_source = source.to_string();
            }
        }
    }

    pub fn status_str(&self) -> &'static str {
        if self.exited {
            "exited"
        } else {
            "running"
        }
    }

    /// `{"session_id", "command", "status"}` — mirrors `_status_head`.
    pub fn status_head(&self) -> Value {
        json!({
            "session_id": self.id,
            "command": self.command,
            "status": self.status_str(),
        })
    }

    /// Exit metadata: `status`, `exit_code`, `completion_reason`, `termination_source`.
    pub fn exit_fields(&self) -> Value {
        let mut v = json!({
            "exit_code": self.exit_code,
            "completion_reason": self.completion_reason,
        });
        if !self.termination_source.is_empty() {
            v["termination_source"] = json!(self.termination_source);
        }
        v
    }

    /// Output tail with ANSI stripped.
    pub fn output_tail(&self, n: usize) -> String {
        strip_ansi(&tail_chars(&self.output_buffer, n))
    }

    /// `output` sized by `completion_output_chars`, plus `output_cut` when trimmed.
    /// Shared by the completion notification and the wait/poll/kill snapshots.
    pub fn completion_output(&self) -> Value {
        let limit = if self.completion_output_chars > 0 {
            self.completion_output_chars
        } else {
            COMPLETION_OUTPUT_CHARS
        };
        let total = self.output_buffer.chars().count();
        let cut = total.saturating_sub(limit) as i64;
        let mut v = json!({ "output": self.output_tail(limit) });
        if cut > 0 {
            v["output_cut"] = json!(cut);
        }
        v
    }

    /// `{"status", "command", <exit fields>, <completion output>}`.
    pub fn exit_snapshot(&self, status: &str) -> Value {
        let mut v = json!({ "status": status, "command": self.command });
        if let Some(obj) = self.exit_fields().as_object() {
            for (k, val) in obj {
                v[k] = val.clone();
            }
        }
        if let Some(obj) = self.completion_output().as_object() {
            for (k, val) in obj {
                v[k] = val.clone();
            }
        }
        v
    }
}

// ── seams ──────────────────────────────────────────────────────────────────────

/// Wall-clock and monotonic time. Injectable so tests need no real sleeps.
pub trait Clock: Send + Sync {
    fn now_secs(&self) -> f64;
    fn monotonic_secs(&self) -> f64;
}

/// Production clock.
#[derive(Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_secs(&self) -> f64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0)
    }
    fn monotonic_secs(&self) -> f64 {
        // Monotonic source good enough for deadlines without a process-wide static.
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0)
    }
}

/// Interrupt/yield signals from the agent loop. The no-op default keeps the registry
/// usable outside a turn (tests, cron, one-shot runs).
pub trait InterruptSource: Send + Sync {
    fn is_interrupted(&self) -> bool;
    /// A steer/redirect landed mid-turn: `redirect()` asks tool workers to YIELD so the
    /// user's message is delivered instead of parked behind this wait.
    fn consume_yield(&self, thread_id: Option<u64>) -> bool;
}

/// Never interrupted — the default outside an active turn.
#[derive(Debug, Default)]
pub struct NoInterrupt;

impl InterruptSource for NoInterrupt {
    fn is_interrupted(&self) -> bool {
        false
    }
    fn consume_yield(&self, _thread_id: Option<u64>) -> bool {
        false
    }
}

/// Post-processes an output-bearing result (plugin output transform, then secret
/// redaction). Python runs the transform hook first and redaction after, so a
/// replacement the plugin returns is still masked. The default is pass-through when no
/// plugin is registered.
pub trait ResultTransformer: Send + Sync {
    fn transform(&self, result: &mut Value);
}

/// Pass-through transformer.
#[derive(Debug, Default)]
pub struct NoTransform;

impl ResultTransformer for NoTransform {
    fn transform(&self, _result: &mut Value) {}
}

/// A minimal in-memory process handle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildRef {
    pub id: String,
    pub pid: Option<i32>,
    pub is_pty: bool,
}

/// What to spawn.
pub struct SpawnRequest {
    pub session: Arc<SessionHandle>,
    pub command: String,
    pub cwd: Option<String>,
    pub env: Vec<(String, String)>,
}

/// Spawns and controls real child processes.
pub trait ProcessHost: Send + Sync {
    fn spawn(&self, req: SpawnRequest) -> Result<ChildRef, String>;
    fn kill(&self, child: &ChildRef) -> Result<(), String>;
    fn try_wait(&self, child: &ChildRef) -> Option<Option<i32>>;
    fn is_alive(&self, child: &ChildRef) -> bool;
    fn write_stdin(&self, child: &ChildRef, data: &[u8]) -> Result<usize, String>;
    fn close_stdin(&self, child: &ChildRef) -> Result<(), String>;
}

/// A PTY backend for `use_pty` processes.
pub trait PtyHost: Send + Sync {
    fn available(&self) -> bool;
    fn write(&self, child: &ChildRef, data: &[u8]) -> Result<(), String>;
    fn send_eof(&self, child: &ChildRef) -> Result<(), String>;
}

/// systemd transient-scope control (Linux only). Layer 3 fills the probe.
pub trait ScopeHost: Send + Sync {
    /// Whether `systemd-run --user --scope` is usable right now.
    fn available(&self) -> bool;
    fn build_scope_argv(&self, shell_argv: &[String], unit_suffix: &str) -> Vec<String>;
    fn stop_unit(&self, unit_name: &str) -> bool;
}

/// No scope support: every spawn runs in the caller's cgroup.
#[derive(Debug, Default)]
pub struct NoScope;

impl ScopeHost for NoScope {
    fn available(&self) -> bool {
        false
    }
    fn build_scope_argv(&self, shell_argv: &[String], _unit_suffix: &str) -> Vec<String> {
        shell_argv.to_vec()
    }
    fn stop_unit(&self, _unit_name: &str) -> bool {
        false
    }
}

/// Live-output hook: installed by the registry so reader threads route chunks through
/// the watch pipeline (buffer + pattern scan + live stream) instead of appending blind.
pub type OutputHook = Arc<dyn Fn(&str) + Send + Sync>;

/// Session state plus the exit signal `wait` blocks on.
pub struct SessionHandle {
    pub session: Mutex<ProcessSession>,
    pub exited: tokio::sync::Notify,
    output_hook: Mutex<Option<OutputHook>>,
}

impl std::fmt::Debug for SessionHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionHandle")
            .field("id", &self.id())
            .finish_non_exhaustive()
    }
}

impl SessionHandle {
    pub fn new(session: ProcessSession) -> Arc<Self> {
        Arc::new(Self {
            session: Mutex::new(session),
            exited: tokio::sync::Notify::new(),
            output_hook: Mutex::new(None),
        })
    }

    pub fn snapshot(&self) -> ProcessSession {
        self.session.lock().clone()
    }

    /// Append output and wake nobody (output is polled, not awaited).
    pub fn append_output(&self, text: &str) {
        self.session.lock().append_output(text);
    }

    /// Install the live-output hook (the registry's watch pipeline).
    pub fn set_output_hook(&self, hook: Option<OutputHook>) {
        *self.output_hook.lock() = hook;
    }

    /// Route a freshly-read chunk: through the hook when installed (buffer + watch scan
    /// + live stream), else straight into the buffer.
    pub fn ingest_chunk(&self, text: &str) {
        let hook = self.output_hook.lock().clone();
        match hook {
            Some(hook) => hook(text),
            None => self.session.lock().append_output(text),
        }
    }

    /// Mark exited and wake any waiter.
    pub fn finish(&self, exit_code: Option<i32>, reason: &str, source: &str) {
        let mut s = self.session.lock();
        if !s.exited {
            s.exited_at = 0.0;
        }
        s.mark_exited(exit_code, reason, source);
        drop(s);
        self.exited.notify_waiters();
    }

    pub fn is_exited(&self) -> bool {
        self.session.lock().exited
    }

    pub fn id(&self) -> String {
        self.session.lock().id.clone()
    }
}

// ── local process host (std::process + reader thread) ──────────────────────────

/// Spawns children with `std::process` and pumps their output on a reader thread.
#[derive(Debug, Default)]
pub struct LocalProcessHost {
    children: Mutex<HashMap<String, std::process::Child>>,
}

impl LocalProcessHost {
    pub fn new() -> Self {
        Self::default()
    }
}

/// Resolve the shell used to run a command string.
pub fn resolve_shell() -> String {
    std::env::var("SHELL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "/bin/sh".to_string())
}

impl ProcessHost for LocalProcessHost {
    fn spawn(&self, req: SpawnRequest) -> Result<ChildRef, String> {
        use std::process::{Command, Stdio};

        let shell = resolve_shell();
        let mut cmd = Command::new(&shell);
        cmd.arg("-c").arg(&req.command);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(cwd) = &req.cwd {
            cmd.current_dir(cwd);
        }
        for (k, v) in &req.env {
            cmd.env(k, v);
        }

        let mut child = cmd.spawn().map_err(|e| format!("failed to spawn: {}", e))?;
        let pid = child.id() as i32;

        // Reader threads: stdout and stderr both append to the session buffer.
        if let Some(out) = child.stdout.take() {
            let handle = Arc::clone(&req.session);
            spawn_reader(out, handle, "stdout");
        }
        if let Some(err) = child.stderr.take() {
            let handle = Arc::clone(&req.session);
            spawn_reader(err, handle, "stderr");
        }

        // Waiter thread: reaps the child and signals exit.
        let waiter_session = Arc::clone(&req.session);
        let child_id = format!("child_{}", pid);
        let mut waiter_cmd = Command::new(&shell);
        waiter_cmd.arg("-c").arg("true");
        std::thread::spawn(move || {
            // The child is moved into the reaper. In this layer the registry tracks the
            // PID; the reaper polls liveness so a detached supervisor can still exit it.
            loop {
                if !pid_is_alive(pid) {
                    waiter_session.finish(None, "exited", "");
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
        });

        let id = child_id;
        self.children.lock().insert(id.clone(), child);
        Ok(ChildRef {
            id,
            pid: Some(pid),
            is_pty: false,
        })
    }

    fn kill(&self, child: &ChildRef) -> Result<(), String> {
        let mut children = self.children.lock();
        if let Some(c) = children.get_mut(&child.id) {
            return c.kill().map_err(|e| e.to_string());
        }
        if let Some(pid) = child.pid {
            return signal_term(pid, libc::SIGKILL);
        }
        Err(format!("unknown child {}", child.id))
    }

    fn try_wait(&self, child: &ChildRef) -> Option<Option<i32>> {
        let mut children = self.children.lock();
        let c = children.get_mut(&child.id)?;
        match c.try_wait() {
            Ok(Some(status)) => Some(Some(status.code().unwrap_or(-1))),
            Ok(None) => Some(None),
            Err(_) => Some(Some(-1)),
        }
    }

    fn is_alive(&self, child: &ChildRef) -> bool {
        match child.pid {
            Some(pid) => pid_is_alive(pid),
            None => false,
        }
    }

    fn write_stdin(&self, child: &ChildRef, data: &[u8]) -> Result<usize, String> {
        use std::io::Write;
        let mut children = self.children.lock();
        let c = children.get_mut(&child.id).ok_or("unknown child")?;
        let stdin = c.stdin.as_mut().ok_or("Process stdin not available")?;
        stdin.write_all(data).map_err(|e| e.to_string())?;
        stdin.flush().map_err(|e| e.to_string())?;
        Ok(data.len())
    }

    fn close_stdin(&self, child: &ChildRef) -> Result<(), String> {
        let mut children = self.children.lock();
        let c = children.get_mut(&child.id).ok_or("unknown child")?;
        drop(c.stdin.take());
        Ok(())
    }
}

impl Drop for LocalProcessHost {
    fn drop(&mut self) {
        // Best-effort: never leave stray children behind when the host is dropped.
        let mut children = self.children.lock();
        for (_, c) in children.iter_mut() {
            let _ = c.kill();
        }
    }
}

fn spawn_reader<R>(reader: R, session: Arc<SessionHandle>, _label: &'static str)
where
    R: std::io::Read + Send + 'static,
{
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        let mut reader = reader;
        // Startup banners only ever appear in the first chunk; cleaning later chunks
        // would eat legitimate output that happens to contain the same words.
        let mut first_chunk = true;
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let mut chunk = String::from_utf8_lossy(&buf[..n]).into_owned();
                    if first_chunk {
                        chunk = clean_shell_noise(&chunk);
                        first_chunk = false;
                    }
                    if !chunk.is_empty() {
                        session.ingest_chunk(&chunk);
                    }
                }
                Err(_) => break,
            }
        }
    });
}

/// Signal a PID (POSIX). Returns `Ok` when the signal was delivered or the process is gone.
pub fn signal_term(pid: i32, sig: i32) -> Result<(), String> {
    #[cfg(unix)]
    unsafe {
        if libc::kill(pid, sig) == 0 {
            Ok(())
        } else {
            Err(format!("kill({}, {}) failed", pid, sig))
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (pid, sig);
        Err("signals are not supported on this platform".to_string())
    }
}

/// Whether a host PID is alive.
pub fn pid_is_alive(pid: i32) -> bool {
    #[cfg(unix)]
    unsafe {
        // Signal 0 probes existence without delivering anything.
        libc::kill(pid, 0) == 0
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        false
    }
}

// ── POSIX PTY host ─────────────────────────────────────────────────────────────

/// Real POSIX PTY support via `openpty(3)`. On non-Unix platforms `available()`
/// reports false, mirroring the Python `_IS_WINDOWS` gate (Windows PTY needs ConPTY
/// through pywinpty, a separate backend).
#[derive(Debug, Default)]
pub struct PosixPtyHost {
    ptys: Mutex<HashMap<String, std::fs::File>>,
}

impl PosixPtyHost {
    pub fn new() -> Self {
        Self::default()
    }
}

impl PtyHost for PosixPtyHost {
    fn available(&self) -> bool {
        cfg!(unix)
    }

    fn write(&self, child: &ChildRef, data: &[u8]) -> Result<(), String> {
        use std::io::Write;
        let mut ptys = self.ptys.lock();
        let f = ptys.get_mut(&child.id).ok_or("no pty for this process")?;
        f.write_all(data).map_err(|e| e.to_string())?;
        f.flush().map_err(|e| e.to_string())
    }

    fn send_eof(&self, child: &ChildRef) -> Result<(), String> {
        use std::io::Write;
        let mut ptys = self.ptys.lock();
        let f = ptys.get_mut(&child.id).ok_or("no pty for this process")?;
        f.write_all(&[0x04]).map_err(|e| e.to_string()) // EOT
    }
}

/// Allocate a PTY pair: returns `(master_fd, slave_fd)`.
#[cfg(unix)]
pub fn open_pty() -> Result<(i32, i32), String> {
    let mut master: libc::c_int = -1;
    let mut slave: libc::c_int = -1;
    let rc = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    if rc != 0 {
        return Err("openpty failed".to_string());
    }
    Ok((master, slave))
}

// ── registry ───────────────────────────────────────────────────────────────────

#[derive(Default)]
struct RegistryInner {
    running: HashMap<String, Arc<SessionHandle>>,
    finished: HashMap<String, Arc<SessionHandle>>,
    /// Insertion order of `finished`, for LRU pruning.
    finished_order: VecDeque<String>,
    /// Processes merely *observed* exited via `poll()`. `poll()` is read-only and must
    /// NOT mark consumed, or a status check would suppress the watcher's delivery turn;
    /// the CLI has the result inline, so `drain_notifications()` skips these.
    poll_observed: HashSet<String>,
    /// Completions the agent already consumed via `wait()`/`read_log()`.
    completion_consumed: HashSet<String>,
}

/// In-memory registry of running and finished background processes.
pub struct ProcessRegistry {
    inner: Mutex<RegistryInner>,
    host: Arc<dyn ProcessHost>,
    pty: Arc<dyn PtyHost>,
    scope: Arc<dyn ScopeHost>,
    interrupt: Arc<dyn InterruptSource>,
    clock: Arc<dyn Clock>,
    transformer: Arc<dyn ResultTransformer>,
    /// Layer 2: watch patterns, the fleet-wide breaker, heartbeats, live streaming, and
    /// the unified event queue the CLI/gateway drains after each turn.
    pub watch: Arc<crate::process_watch::WatchPipeline>,
    /// Layer 3: crash-recovery persistence + PID probe + command redaction.
    pub checkpoint: Arc<dyn crate::process_checkpoint::CheckpointStore>,
    pub pid_probe: Arc<dyn crate::process_checkpoint::PidProbe>,
    pub redactor: Arc<dyn crate::process_checkpoint::CommandRedactor>,
    /// Side-channel for check_interval watchers re-enqueued by crash recovery.
    pub pending_watchers: Mutex<Vec<Value>>,
}

impl Default for ProcessRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ProcessRegistry {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(RegistryInner::default()),
            host: Arc::new(LocalProcessHost::new()),
            pty: Arc::new(PosixPtyHost::new()),
            scope: Arc::new(NoScope),
            interrupt: Arc::new(NoInterrupt),
            clock: Arc::new(SystemClock),
            transformer: Arc::new(NoTransform),
            watch: Arc::new(crate::process_watch::WatchPipeline::new(Arc::new(
                SystemClock,
            ))),
            checkpoint: Arc::new(crate::process_checkpoint::FileCheckpointStore::new(
                default_checkpoint_path(),
            )),
            pid_probe: Arc::new(crate::process_checkpoint::SystemPidProbe),
            redactor: Arc::new(crate::process_checkpoint::NoRedact),
            pending_watchers: Mutex::new(Vec::new()),
        }
    }

    /// Fully injectable constructor used by tests.
    pub fn with_parts(
        host: Arc<dyn ProcessHost>,
        pty: Arc<dyn PtyHost>,
        scope: Arc<dyn ScopeHost>,
        interrupt: Arc<dyn InterruptSource>,
        clock: Arc<dyn Clock>,
        transformer: Arc<dyn ResultTransformer>,
    ) -> Self {
        Self {
            inner: Mutex::new(RegistryInner::default()),
            host,
            pty,
            scope,
            interrupt,
            watch: Arc::new(crate::process_watch::WatchPipeline::new(Arc::clone(&clock))),
            checkpoint: Arc::new(crate::process_checkpoint::MemoryCheckpointStore::new()),
            pid_probe: Arc::new(crate::process_checkpoint::SystemPidProbe),
            redactor: Arc::new(crate::process_checkpoint::NoRedact),
            pending_watchers: Mutex::new(Vec::new()),
            clock,
            transformer,
        }
    }

    pub fn clock(&self) -> &Arc<dyn Clock> {
        &self.clock
    }

    pub fn scope(&self) -> &Arc<dyn ScopeHost> {
        &self.scope
    }

    /// Register a session. Callers that spawn a real process use [`Self::spawn_session`].
    pub fn register(&self, session: ProcessSession) -> Arc<SessionHandle> {
        let handle = SessionHandle::new(session);
        let id = handle.id();
        self.wire_ingest(&handle);
        self.inner.lock().running.insert(id, Arc::clone(&handle));
        self.prune_if_needed();
        handle
    }

    /// Route a session's reader output through the watch pipeline.
    fn wire_ingest(&self, handle: &Arc<SessionHandle>) {
        let pipe = Arc::clone(&self.watch);
        let weak = Arc::downgrade(handle);
        handle.set_output_hook(Some(Arc::new(move |text: &str| {
            if let Some(h) = weak.upgrade() {
                pipe.ingest_text(&h, text);
            }
        })));
    }

    /// Spawn and register a background process.
    pub fn spawn_session(
        &self,
        command: &str,
        cwd: Option<String>,
        env: Vec<(String, String)>,
        task_id: Option<String>,
    ) -> Result<Arc<SessionHandle>, String> {
        let id = new_session_id();
        let mut session = ProcessSession::new(id.clone(), command, self.clock().now_secs());
        session.cwd = cwd.clone();
        if let Some(t) = task_id {
            session.task_id = t.clone();
            session.owner_task_id = t;
        }
        let handle = SessionHandle::new(session);
        self.wire_ingest(&handle);
        let child = self.host.spawn(SpawnRequest {
            session: Arc::clone(&handle),
            command: command.to_string(),
            cwd,
            env,
        })?;
        {
            let mut s = handle.session.lock();
            s.pid = child.pid;
        }
        self.inner.lock().running.insert(id, Arc::clone(&handle));
        self.prune_if_needed();
        Ok(handle)
    }

    /// Look up a session by exact id.
    pub fn get(&self, session_id: &str) -> Option<Arc<SessionHandle>> {
        let inner = self.inner.lock();
        inner
            .running
            .get(session_id)
            .or_else(|| inner.finished.get(session_id))
            .cloned()
    }

    /// Resolve a unique id prefix (e.g. `"4dae"` for `proc_4dae56ca81f6`).
    pub fn resolve_prefix(&self, session_id: &str) -> Option<Arc<SessionHandle>> {
        if let Some(h) = self.get(session_id) {
            return Some(h);
        }
        if session_id.is_empty() {
            return None;
        }
        let inner = self.inner.lock();
        let mut matches: Vec<Arc<SessionHandle>> = inner
            .running
            .values()
            .chain(inner.finished.values())
            .filter(|h| h.id().starts_with(session_id))
            .cloned()
            .collect();
        matches.sort_by_key(|h| h.id());
        matches.dedup_by_key(|h| h.id());
        if matches.len() == 1 {
            matches.into_iter().next()
        } else {
            None
        }
    }

    /// O(1) running count for status-bar polling.
    pub fn count_running(&self) -> usize {
        self.inner.lock().running.len()
    }

    pub fn has_any_active(&self) -> bool {
        self.count_running() > 0
    }

    pub fn has_active_processes(&self, task_id: &str) -> bool {
        self.inner
            .lock()
            .running
            .values()
            .any(|h| h.session.lock().task_id == task_id)
    }

    pub fn has_active_for_session(&self, session_key: &str, max_active_age: Option<f64>) -> bool {
        let now = self.clock().now_secs();
        self.inner.lock().running.values().any(|h| {
            let s = h.session.lock();
            if s.session_key != session_key {
                return false;
            }
            match max_active_age {
                Some(age) => now - s.started_at <= age,
                None => true,
            }
        })
    }

    pub fn running_owned_by(&self, owner_task_id: &str) -> Vec<Arc<SessionHandle>> {
        let mut v: Vec<Arc<SessionHandle>> = self
            .inner
            .lock()
            .running
            .values()
            .filter(|h| h.session.lock().owner_task_id == owner_task_id)
            .cloned()
            .collect();
        v.sort_by_key(|h| h.id());
        v
    }

    pub fn snapshot_running_ids(&self, task_id: &str) -> HashSet<String> {
        self.inner
            .lock()
            .running
            .values()
            .filter(|h| h.session.lock().task_id == task_id)
            .map(|h| h.id())
            .collect()
    }

    pub fn is_completion_consumed(&self, session_id: &str) -> bool {
        self.inner.lock().completion_consumed.contains(session_id)
    }

    pub fn is_poll_observed(&self, session_id: &str) -> bool {
        self.inner.lock().poll_observed.contains(session_id)
    }

    /// Mark a completion consumed (wait/log observed the finished process's output).
    pub fn mark_completion_consumed(&self, session_id: &str) {
        self.inner
            .lock()
            .completion_consumed
            .insert(session_id.to_string());
    }

    /// Does NOT mark consumed: a poll is read-only and must not suppress the watcher.
    pub fn mark_poll_observed(&self, session_id: &str) {
        self.inner
            .lock()
            .poll_observed
            .insert(session_id.to_string());
    }

    // ── actions ────────────────────────────────────────────────────────────────

    /// `poll` — status + new output.
    pub fn poll(&self, session_id: &str) -> Value {
        let Some(handle) = self.resolve_prefix(session_id) else {
            return not_found(session_id);
        };
        let s = handle.snapshot();
        let mut result = s.status_head();
        result["pid"] = json!(s.pid);
        result["uptime_seconds"] =
            json!((self.clock().now_secs() - s.started_at).floor() as i64);
        result["output_preview"] = json!(s.output_tail(POLL_PREVIEW_CHARS));
        if s.exited {
            if let Some(obj) = s.exit_fields().as_object() {
                for (k, v) in obj {
                    result[k] = v.clone();
                }
            }
            // Read-only: record in poll_observed (CLI inline dedup) but NOT in
            // completion_consumed, or a status check would suppress the watcher's
            // autonomous delivery turn.
            self.mark_poll_observed(&s.id);
        }
        if s.detached {
            result["detached"] = json!(true);
            result["note"] = json!("Process recovered after restart -- output history unavailable");
        }
        result
    }

    /// `log` — full output, paged by lines.
    pub fn read_log(&self, session_id: &str, offset: Option<usize>, limit: usize) -> Value {
        let Some(handle) = self.resolve_prefix(session_id) else {
            return not_found(session_id);
        };
        let s = handle.snapshot();
        let full = strip_ansi(&s.output_buffer);
        let lines: Vec<&str> = full.split('\n').collect();
        let total_lines = lines.len();
        let (selected, observed) = select_log_lines(&lines, offset, limit);
        let mut result = s.status_head();
        result["output"] = json!(selected.join("\n"));
        result["total_lines"] = json!(total_lines);
        result["showing"] = json!(format!("{} lines", selected.len()));
        if s.exited && observed {
            self.mark_completion_consumed(&s.id);
        }
        result
    }

    /// `wait` — block until exit, timeout, interrupt, or a mid-turn steer.
    pub async fn wait(&self, session_id: &str, timeout: Option<u64>, max_timeout: u64) -> Value {
        if let Some(t) = timeout {
            if t == 0 {
                return json!({"status": "error", "error": "timeout must be positive (got 0)"});
            }
        }
        let (effective_timeout, timeout_note) = clamp_wait_timeout(timeout, max_timeout);
        let Some(handle) = self.resolve_prefix(session_id) else {
            return not_found(session_id);
        };
        // The wait deadline is REAL time, not the injectable Clock: the loop sleeps on
        // the tokio timer, so measuring the deadline with a clock that does not advance
        // alongside it would never expire (a frozen test clock spun here forever).
        // `Clock` stays for wall-clock display only (uptime, started_at).
        let deadline = tokio::time::Instant::now()
            + std::time::Duration::from_secs(effective_timeout);

        // Terminal state already reached?
        if let Some(mut r) = self.wait_ready(&handle, timeout_note.as_deref()) {
            return { r["timeout_note"] = json!(timeout_note); r };
        }

        loop {
            let now = tokio::time::Instant::now();
            if now >= deadline {
                break;
            }
            let remaining = deadline - now;
            let notified = handle.exited.notified();
            // Register interest BEFORE the final re-check so an exit between the check
            // and the await cannot be missed.
            tokio::select! {
                _ = notified => {
                    if let Some(r) = self.wait_ready(&handle, timeout_note.as_deref()) {
                        return r;
                    }
                }
                _ = tokio::time::sleep(remaining) => {
                    if let Some(r) = self.wait_ready(&handle, timeout_note.as_deref()) {
                        return r;
                    }
                }
            }
        }

        let s = handle.snapshot();
        let mut result = json!({
            "status": "timeout",
            "command": s.command,
            "output": s.output_tail(WAIT_OUTPUT_CHARS),
            // Not a failure — models re-issued identical waits after misreading this
            // as an error.
            "process_running": true,
        });
        let mut note = format!(
            "Wait window of {}s elapsed — the process is still running. This is not an error.",
            effective_timeout
        );
        if s.started_at > 0.0 {
            note.push_str(&format!(
                " Uptime: {}s.",
                (self.clock().now_secs() - s.started_at).floor() as i64
            ));
        }
        if s.notify_on_complete {
            note.push_str(
                " notify_on_complete is set: you will be notified on exit — do more work instead of waiting again.",
            );
        } else {
            note.push_str(
                " Poll again later or use terminal(background=true, notify_on_complete=true) next time for automatic notification.",
            );
        }
        result["timeout_note"] = json!(match &timeout_note {
            Some(t) => format!("{}. {}", t, note),
            None => note,
        });
        result
    }

    /// Terminal-state check used by the wait loop. Returns `None` while running.
    fn wait_ready(&self, handle: &Arc<SessionHandle>, timeout_note: Option<&str>) -> Option<Value> {
        let s = handle.snapshot();
        let mut result = if s.exited {
            self.mark_completion_consumed(&s.id);
            Some(s.exit_snapshot("exited"))
        } else if self.interrupt.is_interrupted() {
            Some(json!({
                "status": "interrupted",
                "command": s.command,
                "output": s.output_tail(WAIT_OUTPUT_CHARS),
                "note": "User sent a new message -- wait interrupted",
            }))
        } else if self.interrupt.consume_yield(None) {
            // A steer/redirect landed mid-turn. The process is untouched and still
            // notify-tracked; the model should read the steer text and respond, not
            // re-issue the wait.
            Some(json!({
                "status": "interrupted",
                "command": s.command,
                "output": s.output_tail(WAIT_OUTPUT_CHARS),
                "process_running": true,
                "note": "User sent a new message -- wait released; the process is still running and you will be notified on exit. Respond to the user now.",
            }))
        } else {
            None
        };
        if let Some(v) = result.as_mut() {
            if let Some(t) = timeout_note {
                v["timeout_note"] = json!(t);
            }
        }
        result
    }

    /// `kill` — terminate a process.
    pub fn kill_process(&self, session_id: &str, source: &str, consume_output: bool) -> Value {
        let Some(handle) = self.resolve_prefix(session_id) else {
            return not_found(session_id);
        };
        let s = handle.snapshot();
        if s.exited {
            let mut v = s.exit_snapshot("already_exited");
            v["error"] = json!("Process has already finished");
            return v;
        }
        let child = ChildRef {
            id: format!("child_{}", s.pid.unwrap_or(0)),
            pid: s.pid,
            is_pty: false,
        };
        let kill_result = self.host.kill(&child);
        // Record the kill even when signalling failed: the process is about to be
        // reaped, and `mark_exited` refuses to overwrite a "killed" reason.
        handle.finish(None, "killed", source);
        self.move_to_finished(&handle);
        if consume_output {
            self.mark_completion_consumed(&handle.id());
        }
        let finished = handle.snapshot();
        let mut v = finished.exit_snapshot("killed");
        if let Err(e) = kill_result {
            v["kill_warning"] = json!(e);
        }
        v
    }

    /// `write` — raw bytes to stdin, no newline appended.
    pub fn write_stdin(&self, session_id: &str, data: &str) -> Value {
        let Some(handle) = self.resolve_prefix(session_id) else {
            return not_found(session_id);
        };
        if handle.is_exited() {
            return json!({"status": "already_exited", "error": "Process has already finished"});
        }
        match self.stdin_op(&handle, data) {
            Ok(()) => {
                json!({"status": "ok", "bytes_written": data.as_bytes().len()})
            }
            Err(e) => json!({"status": "error", "error": e}),
        }
    }

    /// `submit` — data + newline, like pressing Enter. The Enter sequence depends on
    /// the session's backend: a Windows PTY needs CRLF, everything else LF.
    pub fn submit_stdin(&self, session_id: &str, data: &str) -> Value {
        let Some(handle) = self.resolve_prefix(session_id) else {
            return not_found(session_id);
        };
        if handle.is_exited() {
            return json!({"status": "already_exited", "error": "Process has already finished"});
        }
        let is_pty = handle.session.lock().is_pty;
        let ending = submit_line_ending(cfg!(windows), is_pty);
        let payload = format!("{}{}", data, ending);
        match self.stdin_op(&handle, &payload) {
            Ok(()) => json!({"status": "ok", "bytes_written": payload.as_bytes().len()}),
            Err(e) => json!({"status": "error", "error": e}),
        }
    }

    /// `close` — close stdin / send EOF without killing the process.
    pub fn close_stdin(&self, session_id: &str) -> Value {
        let Some(handle) = self.resolve_prefix(session_id) else {
            return not_found(session_id);
        };
        if handle.is_exited() {
            return json!({"status": "already_exited", "error": "Process has already finished"});
        }
        let s = handle.snapshot();
        let msg = if s.is_pty { "EOF sent" } else { "stdin closed" };
        let child = ChildRef {
            id: format!("child_{}", s.pid.unwrap_or(0)),
            pid: s.pid,
            is_pty: s.is_pty,
        };
        let result = if s.is_pty {
            self.pty.send_eof(&child)
        } else {
            self.host.close_stdin(&child)
        };
        match result {
            Ok(()) => json!({"status": "ok", "message": msg}),
            Err(e) => json!({"status": "error", "error": e}),
        }
    }

    /// Run a stdin write on the session's backend: the PTY host under `use_pty`, else
    /// the pipe. Mirrors Python's `_stdin_op` dispatch.
    fn stdin_op(&self, handle: &Arc<SessionHandle>, data: &str) -> Result<(), String> {
        let s = handle.snapshot();
        let child = ChildRef {
            id: format!("child_{}", s.pid.unwrap_or(0)),
            pid: s.pid,
            is_pty: s.is_pty,
        };
        if s.is_pty {
            // A PTY is a byte stream; hand the backend bytes.
            return self.pty.write(&child, data.as_bytes());
        }
        if s.pid.is_none() {
            return Err("Process stdin not available (non-local backend or stdin closed)".to_string());
        }
        self.host.write_stdin(&child, data.as_bytes())?;
        Ok(())
    }

    /// `list` — running and recently-finished processes.
    pub fn list_sessions(
        &self,
        task_id: Option<&str>,
        session_key: Option<&str>,
        include_retained: bool,
    ) -> Vec<Value> {
        let now = self.clock().now_secs();
        let mut all: Vec<ProcessSession> = {
            let inner = self.inner.lock();
            inner
                .running
                .values()
                .chain(inner.finished.values())
                .map(|h| h.session.lock().clone())
                .collect()
        };
        if !include_retained {
            all.retain(|s| !s.exited || now - s.exited_at <= FINISHED_TTL_SECONDS);
        }
        if task_id.is_some() || session_key.is_some() {
            all.retain(|s| {
                (task_id.is_some() && Some(s.task_id.as_str()) == task_id)
                    || (session_key.is_some() && Some(s.session_key.as_str()) == session_key)
            });
        }
        all.sort_by(|a, b| a.id.cmp(&b.id));
        all.into_iter()
            .map(|s| {
                let mut entry = json!({
                    "session_id": s.id,
                    "command": head_chars(&s.command, LIST_COMMAND_CHARS),
                    "cwd": s.cwd,
                    "pid": s.pid,
                    "owner_task_id": if s.owner_task_id.is_empty() { s.task_id.clone() } else { s.owner_task_id.clone() },
                    "uptime_seconds": (now - s.started_at).floor() as i64,
                    "status": s.status_str(),
                    "output_preview": tail_chars(&s.output_buffer, LIST_PREVIEW_CHARS),
                });
                // Flag processes surfaced only because they share the gateway session
                // (not the current task) — a forgotten preview server blocking reset.
                if let Some(tid) = task_id {
                    if s.task_id != tid {
                        entry["session_scoped"] = json!(true);
                    }
                }
                entry
            })
            .collect()
    }

    // ── lifecycle ──────────────────────────────────────────────────────────────

    /// Move a session from running to finished, stamping `exited_at`.
    pub fn move_to_finished(&self, handle: &Arc<SessionHandle>) -> bool {
        let id = handle.id();
        let mut inner = self.inner.lock();
        if inner.finished.contains_key(&id) {
            return false;
        }
        inner.running.remove(&id);
        {
            let mut s = handle.session.lock();
            if s.exited_at == 0.0 {
                s.exited_at = self.clock().now_secs();
            }
        }
        inner.finished.insert(id.clone(), Arc::clone(handle));
        inner.finished_order.push_back(id);
        drop(inner);
        self.prune_if_needed();
        true
    }

    /// Kill every running process, optionally filtered.
    pub fn kill_all(&self, task_id: Option<&str>, source: &str) -> usize {
        let targets: Vec<String> = {
            let inner = self.inner.lock();
            inner
                .running
                .values()
                .filter(|h| match task_id {
                    Some(t) => h.session.lock().task_id == t,
                    None => true,
                })
                .map(|h| h.id())
                .collect()
        };
        let mut n = 0;
        for id in targets {
            let v = self.kill_process(&id, source, false);
            if v.get("status").and_then(|s| s.as_str()) == Some("killed") {
                n += 1;
            }
        }
        n
    }

    /// Kill processes started since a baseline snapshot; returns the count killed.
    pub fn kill_started_since(
        &self,
        task_id: &str,
        baseline: &HashSet<String>,
        source: &str,
    ) -> usize {
        let targets: Vec<String> = self
            .snapshot_running_ids(task_id)
            .into_iter()
            .filter(|id| !baseline.contains(id))
            .collect();
        let mut n = 0;
        for id in targets {
            let v = self.kill_process(&id, source, false);
            if v.get("status").and_then(|s| s.as_str()) == Some("killed") {
                n += 1;
            }
        }
        n
    }

    /// Drop expired finished sessions, then enforce `MAX_PROCESSES` (LRU).
    pub fn prune_if_needed(&self) {
        let now = self.clock().now_secs();
        let mut inner = self.inner.lock();
        // TTL expiry.
        let expired: Vec<String> = inner
            .finished_order
            .iter()
            .filter(|id| {
                inner
                    .finished
                    .get(*id)
                    .map(|h| {
                        let s = h.session.lock();
                        s.exited_at > 0.0 && now - s.exited_at > FINISHED_TTL_SECONDS
                    })
                    .unwrap_or(true)
            })
            .cloned()
            .collect();
        for id in expired {
            inner.finished.remove(&id);
            inner.poll_observed.remove(&id);
            inner.completion_consumed.remove(&id);
        }
        let still_present: std::collections::HashSet<String> =
            inner.finished.keys().cloned().collect();
        inner.finished_order.retain(|id| still_present.contains(id));
        // LRU: evict oldest finished entries past the cap.
        while inner.running.len() + inner.finished.len() > MAX_PROCESSES {
            let Some(oldest) = inner.finished_order.pop_front() else {
                break;
            };
            inner.finished.remove(&oldest);
            inner.poll_observed.remove(&oldest);
            inner.completion_consumed.remove(&oldest);
        }
    }

    /// Apply the result transformer to an output-bearing payload.
    pub fn transform_result(&self, mut result: Value) -> Value {
        self.transformer.transform(&mut result);
        result
    }
    // ── layer 3: checkpoint / crash recovery ─────────────────────────────────────

    /// Attach a different checkpoint store (file path or in-memory) and redactor.
    pub fn with_checkpoint(
        mut self,
        store: Arc<dyn crate::process_checkpoint::CheckpointStore>,
        redactor: Arc<dyn crate::process_checkpoint::CommandRedactor>,
        probe: Arc<dyn crate::process_checkpoint::PidProbe>,
    ) -> Self {
        self.checkpoint = store;
        self.redactor = redactor;
        self.pid_probe = probe;
        self
    }

    /// Write running-process metadata to the checkpoint file atomically. Never raises;
    /// a failed write must not take the registry down mid-operation.
    pub fn write_checkpoint(&self, extra_entries: Vec<Value>) {
        use crate::process_checkpoint::{build_checkpoint_payload, serialize_checkpoint};
        let sessions = self.running_handles();
        let probe = Arc::clone(&self.pid_probe);
        let payload = build_checkpoint_payload(
            &sessions,
            &extra_entries,
            |pid| probe.start_time(pid),
            &*self.redactor,
        );
        let text = serialize_checkpoint(&payload);
        if let Err(e) = self.checkpoint.write(&text) {
            warn!("Failed to write checkpoint file {}: {}", self.checkpoint.path(), e);
        }
    }

    /// On gateway startup, probe PIDs from the checkpoint and adopt the survivors as
    /// detached sessions. Returns how many were recovered.
    pub fn recover_from_checkpoint(&self) -> usize {
        use crate::process_checkpoint::{
            entry_to_session, parse_checkpoint, pending_watcher_for, plan_recovery,
            RecoverOutcome,
        };
        let Some(contents) = self.checkpoint.read() else {
            return 0;
        };
        let entries = parse_checkpoint(&contents);
        if entries.is_empty() {
            return 0;
        }
        let now = self.clock.now_secs();
        let mut recovered = 0usize;
        let mut unresolved: Vec<Value> = Vec::new();

        for entry in entries {
            let sid = entry
                .get("session_id")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let already = self
                .inner
                .lock()
                .running
                .contains_key(&sid);
            let pid = entry.get("pid").and_then(|v| v.as_i64()).map(|v| v as i32);
            let expected_start = entry.get("host_start_time").and_then(|v| v.as_i64());
            // Alive AND still the process we spawned: the kernel recycles PIDs, so a
            // stored number can later name a stranger.
            let alive = pid.map(|p| self.pid_probe.is_alive(p)).unwrap_or(false);
            let ours = alive
                && (expected_start.is_none()
                    || self.pid_probe.start_time(pid.unwrap_or(0)) == expected_start);
            let probe = Arc::clone(&self.pid_probe);

            let outcome = plan_recovery(&entry, already, ours, alive, |_unit| {
                // Layer 3 has no systemd-scope host wired; an unresolvable scope is
                // retained so a later layer can reap it.
                let _ = &probe;
                false
            });

            match outcome {
                RecoverOutcome::Recovered => {
                    let session = entry_to_session(&entry, now);
                    if let Some(watcher) = pending_watcher_for(&session) {
                        // Re-enqueue so the gateway can resume notifications after the
                        // session is adopted.
                        self.pending_watchers.lock().push(watcher.to_value());
                    }
                    let handle = self.register(session);
                    // A recovered process has no pipe: it is detached by construction.
                    debug_assert!(handle.snapshot().detached);
                    recovered += 1;
                }
                RecoverOutcome::ScopeUnresolved => {
                    unresolved.push(entry);
                }
                _ => {}
            }
        }
        self.write_checkpoint(unresolved);
        recovered
    }

    /// Take the watcher re-enqueue requests produced by crash recovery.
    pub fn take_pending_watchers(&self) -> Vec<Value> {
        std::mem::take(&mut *self.pending_watchers.lock())
    }

    // ── layer 2: watch / heartbeat / events ──────────────────────────────────────

    /// Buffer a chunk and run the watch scan (used by tests and non-reader backends).
    pub fn ingest_output(&self, handle: &Arc<SessionHandle>, text: &str) {
        self.watch.ingest_text(handle, text);
    }

    /// Every running session handle, for the heartbeat tick.
    pub fn running_handles(&self) -> Vec<Arc<SessionHandle>> {
        self.inner.lock().running.values().cloned().collect()
    }

    /// Enable periodic heartbeats for a session; returns the effective interval, or
    /// `None` when the session does not exist.
    pub fn arm_heartbeat(&self, session_id: &str, seconds: u64) -> Option<u64> {
        let handle = self.resolve_prefix(session_id)?;
        Some(self.watch.arm_heartbeat(&handle, seconds))
    }

    /// One pass of the heartbeat loop over every running session.
    pub fn heartbeat_tick(&self) -> Vec<Value> {
        let handles = self.running_handles();
        self.watch.heartbeat_tick(handles.iter())
    }

    /// Take every queued background event (watch matches, heartbeats, breaker notices).
    pub fn drain_events(&self) -> Vec<Value> {
        self.watch.drain()
    }

    /// Inspect the queued events without consuming them.
    pub fn peek_events(&self) -> Vec<Value> {
        self.watch.events.snapshot()
    }

    /// Install the live-output sink (desktop gateway's streaming terminal).
    pub fn set_output_sink(&self, sink: Option<crate::process_watch::OutputSink>) {
        self.watch.set_output_sink(sink);
    }

}

/// Profile-scoped checkpoint path: `<HERMES_HOME>/processes.json`.
pub fn default_checkpoint_path() -> std::path::PathBuf {
    let home = std::env::var("HERMES_HOME").unwrap_or_else(|_| {
        let base = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
        format!("{}/.hermes", base)
    });
    std::path::PathBuf::from(home).join("processes.json")
}

/// Generate a new session id: `proc_` + 12 hex chars.
pub fn new_session_id() -> String {
    let id = uuid::Uuid::new_v4().simple().to_string();
    format!("proc_{}", &id[..12])
}

// ── tool: schema + handler ─────────────────────────────────────────────────────

/// Bare JSON schema for `process_manage` (mirrors the Python `PROCESS_SCHEMA`).
pub fn schema() -> Value {
    json!({
        "name": "process_manage",
        "description": concat!(
            "Poll, wait on, or kill background terminal processes (from ",
            "terminal(background=true)). ",
            "Completed results remain retrievable by session_id when resuming their owning conversation ",
            "(up to 7 days, newest 64 results per profile; rolling output tail). ",
            "poll: status + new output. log: full output, paged. wait: block ",
            "until exit or timeout (partial output on timeout). write vs ",
            "submit: submit appends Enter — use it to answer prompts; write ",
            "sends raw bytes, no newline. close: EOF stdin. kill: terminate. ",
            "handoff (subagents only): transfer a running process you started to your parent agent, which then ",
            "receives its completion; `data` = one sentence on its purpose. Subagent-owned processes are otherwise ",
            "killed when the subagent finishes and their notifications never reach the parent."
        ),
        "parameters": {
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["list", "poll", "log", "wait", "kill", "write", "submit", "close", "handoff"]
                },
                "session_id": {
                    "type": "string",
                    "description": "From terminal background output; any unique prefix works ('4dae' for proc_4dae56ca81f6). Required except for 'list'."
                },
                "data": {
                    "type": "string",
                    "description": "Stdin text for write/submit; purpose sentence for handoff."
                },
                "timeout": {
                    "type": "integer",
                    "description": "Max seconds for 'wait'.",
                    "minimum": 1
                },
                "offset": {
                    "type": "integer",
                    "description": "Log line offset (default: last 200)."
                },
                "limit": {
                    "type": "integer",
                    "description": "Max log lines.",
                    "minimum": 1
                }
            },
            "required": ["action"]
        }
    })
}

/// Tool definition for `process_manage`, wrapped the way every other core tool is
/// (`{"type": "function", "function": {...}}`).
pub fn definition() -> Value {
    json!({ "type": "function", "function": schema() })
}

/// Actions that need a `session_id`.
pub const SESSION_ACTIONS: [&str; 7] =
    ["poll", "log", "wait", "kill", "write", "submit", "close"];

/// Actions whose result is output-bearing and therefore transformed/redacted first.
pub fn action_is_output_bearing(action: &str) -> bool {
    matches!(action, "poll" | "log" | "wait" | "kill")
}

/// Coerce `session_id` to a string — some models send it as an integer.
pub fn coerce_session_id(args: &Value) -> String {
    match args.get("session_id") {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
    }
}

/// Context for a `process_manage` call.
pub struct ProcessCtx {
    pub registry: Arc<ProcessRegistry>,
    pub task_id: Option<String>,
    /// Wall-clock ceiling for `wait`, from `TERMINAL_TIMEOUT`.
    pub max_wait_timeout: u64,
}

impl ProcessCtx {
    pub fn new(registry: Arc<ProcessRegistry>) -> Self {
        let max_wait_timeout = std::env::var("TERMINAL_TIMEOUT")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(DEFAULT_TERMINAL_TIMEOUT_SECONDS);
        Self {
            registry,
            task_id: None,
            max_wait_timeout,
        }
    }

    pub fn with_task_id(mut self, task_id: Option<String>) -> Self {
        self.task_id = task_id;
        self
    }

    pub fn with_max_wait_timeout(mut self, seconds: u64) -> Self {
        self.max_wait_timeout = seconds;
        self
    }
}

/// Dispatch a `process_manage` call. Returns the JSON text the model receives.
pub async fn handle_process(args: &Value, ctx: &ProcessCtx) -> String {
    let action = args.get("action").and_then(|a| a.as_str()).unwrap_or("");
    let session_id = coerce_session_id(args);
    let reg = &ctx.registry;

    match action {
        "list" => {
            let sessions = reg.list_sessions(ctx.task_id.as_deref(), None, true);
            let processes: Vec<Value> = sessions
                .into_iter()
                .map(|p| reg.transform_result(p))
                .collect();
            json!({ "processes": processes }).to_string()
        }
        "handoff" => {
            if session_id.is_empty() {
                return tool_error("session_id is required for handoff");
            }
            // Ownership transfer lives in the handoff layer; this layer reports the
            // process state so the caller can decide.
            match reg.resolve_prefix(&session_id) {
                None => tool_error(&format!(
                    "cannot hand off {}: not a running process you own (already exited? read its result with poll/log and report it instead).",
                    session_id
                )),
                Some(h) if h.is_exited() => tool_error(&format!(
                    "cannot hand off {}: not a running process you own (already exited? read its result with poll/log and report it instead).",
                    session_id
                )),
                Some(_) => {
                    if args
                        .get("data")
                        .and_then(|d| d.as_str())
                        .map(|d| d.trim())
                        .unwrap_or("")
                        .is_empty()
                    {
                        return tool_error(
                            "handoff requires `data`: one sentence saying what the process is for and what the parent should do with its result.",
                        );
                    }
                    tool_error(
                        "handoff is only available to a running subagent with a live parent; you are not one.",
                    )
                }
            }
        }
        a if SESSION_ACTIONS.contains(&a) => {
            if session_id.is_empty() {
                return tool_error(&format!("session_id is required for {}", a));
            }
            let result = match a {
                "poll" => reg.poll(&session_id),
                "log" => {
                    let offset = args.get("offset").and_then(|v| v.as_u64()).map(|v| v as usize);
                    let limit = args
                        .get("limit")
                        .and_then(|v| v.as_u64())
                        .map(|v| v as usize)
                        .unwrap_or(DEFAULT_LOG_LIMIT);
                    reg.read_log(&session_id, offset, limit)
                }
                "wait" => {
                    let timeout = args.get("timeout").and_then(|v| v.as_u64());
                    reg.wait(&session_id, timeout, ctx.max_wait_timeout).await
                }
                "kill" => reg.kill_process(&session_id, "process.kill", true),
                "write" => reg.write_stdin(&session_id, &data_arg(args)),
                "submit" => reg.submit_stdin(&session_id, &data_arg(args)),
                "close" => reg.close_stdin(&session_id),
                _ => unreachable!("SESSION_ACTIONS covers every arm"),
            };
            if action_is_output_bearing(a) {
                reg.transform_result(result).to_string()
            } else {
                result.to_string()
            }
        }
        _ => tool_error(&format!(
            "Unknown process action: {}. Use: list, poll, log, wait, kill, write, submit, close, handoff",
            action
        )),
    }
}

/// `data` argument as a string (missing → empty).
pub fn data_arg(args: &Value) -> String {
    match args.get("data") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) | None => String::new(),
        Some(other) => other.to_string(),
    }
}

// ── tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── fake clock ──
    #[derive(Debug, Default)]
    struct FakeClock {
        t: Mutex<f64>,
    }
    impl FakeClock {
        fn new(t: f64) -> Arc<Self> {
            Arc::new(Self {
                t: Mutex::new(t),
            })
        }
        fn advance(&self, secs: f64) {
            *self.t.lock() += secs;
        }
    }
    impl Clock for FakeClock {
        fn now_secs(&self) -> f64 {
            *self.t.lock()
        }
        fn monotonic_secs(&self) -> f64 {
            *self.t.lock()
        }
    }

    // ── fake host ──
    #[derive(Debug, Default)]
    struct FakeHost {
        alive: Mutex<HashSet<i32>>,
        killed: Mutex<Vec<i32>>,
        written: Mutex<Vec<u8>>,
        closed: Mutex<u32>,
        fail_spawn: bool,
    }
    impl ProcessHost for FakeHost {
        fn spawn(&self, _req: SpawnRequest) -> Result<ChildRef, String> {
            if self.fail_spawn {
                return Err("spawn refused".to_string());
            }
            let pid = 4242;
            self.alive.lock().insert(pid);
            Ok(ChildRef {
                id: format!("child_{}", pid),
                pid: Some(pid),
                is_pty: false,
            })
        }
        fn kill(&self, child: &ChildRef) -> Result<(), String> {
            if let Some(pid) = child.pid {
                self.killed.lock().push(pid);
                self.alive.lock().remove(&pid);
            }
            Ok(())
        }
        fn try_wait(&self, _child: &ChildRef) -> Option<Option<i32>> {
            Some(None)
        }
        fn is_alive(&self, child: &ChildRef) -> bool {
            child.pid.map(|p| self.alive.lock().contains(&p)).unwrap_or(false)
        }
        fn write_stdin(&self, _child: &ChildRef, data: &[u8]) -> Result<usize, String> {
            self.written.lock().extend_from_slice(data);
            Ok(data.len())
        }
        fn close_stdin(&self, _child: &ChildRef) -> Result<(), String> {
            *self.closed.lock() += 1;
            Ok(())
        }
    }

    // ── fake PTY backend ──
    #[derive(Debug, Default)]
    struct FakePty {
        written: Mutex<Vec<u8>>,
        eofs: Mutex<u32>,
        supports: bool,
    }
    impl PtyHost for FakePty {
        fn available(&self) -> bool {
            self.supports
        }
        fn write(&self, _child: &ChildRef, data: &[u8]) -> Result<(), String> {
            self.written.lock().extend_from_slice(data);
            Ok(())
        }
        fn send_eof(&self, _child: &ChildRef) -> Result<(), String> {
            *self.eofs.lock() += 1;
            Ok(())
        }
    }

    fn registry_with_pty(pty: Arc<FakePty>, clock: Arc<dyn Clock>) -> Arc<ProcessRegistry> {
        Arc::new(ProcessRegistry::with_parts(
            Arc::new(FakeHost::default()),
            pty,
            Arc::new(NoScope),
            Arc::new(NoInterrupt),
            clock,
            Arc::new(NoTransform),
        ))
    }

    fn registry_with(clock: Arc<dyn Clock>) -> Arc<ProcessRegistry> {
        Arc::new(ProcessRegistry::with_parts(
            Arc::new(FakeHost::default()),
            Arc::new(PosixPtyHost::new()),
            Arc::new(NoScope),
            Arc::new(NoInterrupt),
            clock,
            Arc::new(NoTransform),
        ))
    }

    fn session(id: &str) -> ProcessSession {
        ProcessSession::new(id, "echo hi", 1_000.0)
    }

    // ── strip_ansi ──
    #[test]
    fn strip_ansi_removes_csi_sequences() {
        assert_eq!(strip_ansi("\u{1b}[31mred\u{1b}[0m"), "red");
    }

    #[test]
    fn strip_ansi_removes_osc_sequences() {
        assert_eq!(strip_ansi("\u{1b}]0;title\u{7}body"), "body");
        assert_eq!(strip_ansi("\u{1b}]8;;http://x\u{1b}\\link"), "link");
    }

    #[test]
    fn strip_ansi_keeps_plain_text_and_newlines() {
        assert_eq!(strip_ansi("plain\ntext"), "plain\ntext");
    }

    #[test]
    fn strip_ansi_handles_truncated_escape() {
        assert_eq!(strip_ansi("abc\u{1b}"), "abc");
    }

    // ── tail/head ──
    #[test]
    fn tail_chars_counts_characters_not_bytes() {
        assert_eq!(tail_chars("abcdef", 3), "def");
        // Multibyte: "日本語字" is 4 CHARS (12 bytes). Ask for 2 -> last 2 chars.
        // Byte slicing here would panic or cut mid-character; a char count gives "語字".
        assert_eq!(tail_chars("日本語字", 2), "語字");
        assert_eq!(tail_chars("日本語字", 2).chars().count(), 2);
    }

    #[test]
    fn head_chars_limits() {
        assert_eq!(head_chars("abcdef", 2), "ab");
        assert_eq!(head_chars("ab", 5), "ab");
    }

    // ── format_uptime_short ──
    #[test]
    fn uptime_seconds_only() {
        assert_eq!(format_uptime_short(45), "45s");
        assert_eq!(format_uptime_short(0), "0s");
        assert_eq!(format_uptime_short(-5), "0s");
    }

    #[test]
    fn uptime_minutes() {
        assert_eq!(format_uptime_short(60), "1m 0s");
        assert_eq!(format_uptime_short(312), "5m 12s");
    }

    #[test]
    fn uptime_hours() {
        assert_eq!(format_uptime_short(3600), "1h 0m");
        assert_eq!(format_uptime_short(7500), "2h 5m");
    }

    // ── shell noise ──
    #[test]
    fn detects_shell_noise() {
        assert!(is_shell_noise("bash: cannot set terminal process group (-1)"));
        assert!(is_shell_noise("no job control in this shell"));
        assert!(!is_shell_noise("hello"));
    }

    #[test]
    fn clean_shell_noise_pops_leading_noise_lines_only() {
        // Leading banner lines go; the body is untouched.
        assert_eq!(
            clean_shell_noise("no job control in this shell\nreal output\n"),
            "real output\n"
        );
        // A noise line in the BODY is preserved — only the head is cleaned.
        let kept = clean_shell_noise("real output\nno job control in this shell");
        assert!(kept.starts_with("real output"));
        assert!(kept.contains("no job control"));
    }

    #[test]
    fn clean_shell_noise_multiple_leading_lines() {
        let text = "no job control in this shell\ncannot set terminal process group\nBODY";
        assert_eq!(clean_shell_noise(text), "BODY");
    }

    // ── submit line ending (the one real trap) ──
    #[test]
    fn submit_line_endings() {
        assert_eq!(submit_line_ending(false, false), "\n");
        assert_eq!(submit_line_ending(false, true), "\n");
        assert_eq!(submit_line_ending(true, false), "\n");
        // Windows PTY: ConPTY needs CRLF, a bare \n is not a terminator.
        assert_eq!(submit_line_ending(true, true), "\r\n");
    }

    // ── wait timeout clamping ──
    #[test]
    fn clamp_uses_default_when_unset() {
        assert_eq!(clamp_wait_timeout(None, 180), (180, None));
    }

    #[test]
    fn clamp_leaves_small_timeout_alone() {
        assert_eq!(clamp_wait_timeout(Some(30), 180), (30, None));
    }

    #[test]
    fn clamp_reports_ceiling() {
        let (eff, note) = clamp_wait_timeout(Some(600), 180);
        assert_eq!(eff, 180);
        assert!(note.unwrap().contains("clamped to configured limit of 180s"));
    }

    // ── log paging: the falsy-offset trap ──
    #[test]
    fn log_offset_none_returns_tail() {
        let lines = ["a", "b", "c", "d"];
        let (sel, observed) = select_log_lines(&lines, None, 2);
        assert_eq!(sel, vec!["c", "d"]);
        assert!(observed);
    }

    #[test]
    fn log_offset_zero_returns_head_not_tail() {
        let lines = ["a", "b", "c", "d"];
        let (sel, observed) = select_log_lines(&lines, Some(0), 2);
        assert_eq!(sel, vec!["a", "b"]);
        assert!(!observed);
    }

    #[test]
    fn log_offset_zero_with_full_limit_observes_completion() {
        let lines = ["a", "b"];
        let (sel, observed) = select_log_lines(&lines, Some(0), 5);
        assert_eq!(sel, vec!["a", "b"]);
        assert!(observed);
    }

    #[test]
    fn log_empty_buffer_observes_completion() {
        let lines: Vec<&str> = Vec::new();
        let (sel, observed) = select_log_lines(&lines, None, 200);
        assert!(sel.is_empty());
        assert!(observed);
    }

    #[test]
    fn log_offset_past_end_is_empty_and_not_observed() {
        let lines = ["a", "b"];
        let (sel, observed) = select_log_lines(&lines, Some(10), 5);
        assert!(sel.is_empty());
        assert!(!observed);
    }

    // ── session buffer ──
    #[test]
    fn append_output_tracks_total_and_tail() {
        let mut s = session("proc_1");
        s.max_output_chars = 5;
        s.append_output("abc");
        s.append_output("defg");
        assert_eq!(s.total_output_chars, 7);
        assert_eq!(s.output_buffer, "cdefg");
    }

    #[test]
    fn mark_exited_sets_fields() {
        let mut s = session("proc_1");
        s.mark_exited(Some(3), "exited", "process.kill");
        assert!(s.exited);
        assert_eq!(s.exit_code, Some(3));
        assert_eq!(s.completion_reason, "exited");
        assert_eq!(s.termination_source, "process.kill");
    }

    #[test]
    fn mark_exited_never_overwrites_a_kill() {
        let mut s = session("proc_1");
        s.mark_exited(Some(-9), "killed", "process.kill");
        s.mark_exited(Some(0), "exited", "");
        assert_eq!(s.exit_code, Some(-9));
        assert_eq!(s.completion_reason, "killed");
    }

    #[test]
    fn status_str_reflects_exit() {
        let mut s = session("proc_1");
        assert_eq!(s.status_str(), "running");
        s.mark_exited(Some(0), "exited", "");
        assert_eq!(s.status_str(), "exited");
    }

    #[test]
    fn completion_output_adds_cut_marker() {
        let mut s = session("proc_1");
        s.completion_output_chars = 3;
        s.append_output("abcdefgh");
        let out = s.completion_output();
        assert_eq!(out["output"], "fgh");
        assert_eq!(out["output_cut"], 5);
    }

    #[test]
    fn completion_output_no_cut_when_fits() {
        let mut s = session("proc_1");
        s.append_output("ab");
        let out = s.completion_output();
        assert_eq!(out["output"], "ab");
        assert!(out.get("output_cut").is_none());
    }

    #[test]
    fn output_tail_strips_ansi() {
        let mut s = session("proc_1");
        s.append_output("\u{1b}[32mok\u{1b}[0m");
        assert_eq!(s.output_tail(100), "ok");
    }

    // ── registry: register / get / prefix ──
    #[test]
    fn register_and_get() {
        let reg = registry_with(FakeClock::new(1000.0));
        reg.register(session("proc_abc123"));
        assert!(reg.get("proc_abc123").is_some());
        assert!(reg.get("nope").is_none());
        assert_eq!(reg.count_running(), 1);
    }

    #[test]
    fn resolve_prefix_unique_match() {
        let reg = registry_with(FakeClock::new(1000.0));
        reg.register(session("proc_abcd1234"));
        assert!(reg.resolve_prefix("proc_ab").is_some());
        assert!(reg.resolve_prefix("proc_abcd1234").is_some());
    }

    #[test]
    fn resolve_prefix_ambiguous_returns_none() {
        let reg = registry_with(FakeClock::new(1000.0));
        reg.register(session("proc_aa11"));
        reg.register(session("proc_aa22"));
        assert!(reg.resolve_prefix("proc_aa").is_none());
    }

    #[test]
    fn resolve_prefix_empty_returns_none() {
        let reg = registry_with(FakeClock::new(1000.0));
        assert!(reg.resolve_prefix("").is_none());
    }

    // ── poll ──
    #[test]
    fn poll_not_found() {
        let reg = registry_with(FakeClock::new(1000.0));
        let v = reg.poll("proc_missing");
        assert_eq!(v["status"], "not_found");
        assert!(v["error"].as_str().unwrap().contains("No process with ID"));
    }

    #[test]
    fn poll_running_shape() {
        let clock = FakeClock::new(1000.0);
        let reg = registry_with(clock.clone());
        let h = reg.register(session("proc_run1"));
        h.append_output("hello");
        clock.advance(42.0);
        let v = reg.poll("proc_run1");
        assert_eq!(v["status"], "running");
        assert_eq!(v["uptime_seconds"], 42);
        assert_eq!(v["output_preview"], "hello");
        assert_eq!(v["session_id"], "proc_run1");
    }

    #[test]
    fn poll_exited_records_poll_observed_not_consumed() {
        let reg = registry_with(FakeClock::new(1000.0));
        let h = reg.register(session("proc_ex1"));
        h.finish(Some(0), "exited", "");
        let v = reg.poll("proc_ex1");
        assert_eq!(v["status"], "exited");
        assert_eq!(v["exit_code"], 0);
        // A status check must NOT suppress the watcher's delivery turn.
        assert!(reg.is_poll_observed("proc_ex1"));
        assert!(!reg.is_completion_consumed("proc_ex1"));
    }

    #[test]
    fn poll_detached_adds_note() {
        let reg = registry_with(FakeClock::new(1000.0));
        let mut s = session("proc_det1");
        s.detached = true;
        reg.register(s);
        let v = reg.poll("proc_det1");
        assert_eq!(v["detached"], true);
        assert!(v["note"].as_str().unwrap().contains("recovered after restart"));
    }

    // ── read_log ──
    #[test]
    fn read_log_tail_default() {
        let reg = registry_with(FakeClock::new(1000.0));
        let h = reg.register(session("proc_log1"));
        h.append_output("l1\nl2\nl3");
        let v = reg.read_log("proc_log1", None, 2);
        assert_eq!(v["output"], "l2\nl3");
        assert_eq!(v["total_lines"], 3);
        assert_eq!(v["showing"], "2 lines");
    }

    #[test]
    fn read_log_offset_zero_is_head() {
        let reg = registry_with(FakeClock::new(1000.0));
        let h = reg.register(session("proc_log2"));
        h.append_output("l1\nl2\nl3");
        let v = reg.read_log("proc_log2", Some(0), 1);
        assert_eq!(v["output"], "l1");
    }

    #[test]
    fn read_log_exited_full_page_consumes_completion() {
        let reg = registry_with(FakeClock::new(1000.0));
        let h = reg.register(session("proc_log3"));
        h.append_output("only");
        h.finish(Some(0), "exited", "");
        reg.read_log("proc_log3", Some(0), 10);
        assert!(reg.is_completion_consumed("proc_log3"));
    }

    #[test]
    fn read_log_not_found() {
        let reg = registry_with(FakeClock::new(1000.0));
        assert_eq!(reg.read_log("proc_x", None, 10)["status"], "not_found");
    }

    // ── wait ──
    #[tokio::test]
    async fn wait_rejects_zero_timeout() {
        let reg = registry_with(FakeClock::new(1000.0));
        reg.register(session("proc_w0"));
        let v = reg.wait("proc_w0", Some(0), 180).await;
        assert_eq!(v["status"], "error");
        assert!(v["error"].as_str().unwrap().contains("timeout must be positive"));
    }

    #[tokio::test]
    async fn wait_not_found() {
        let reg = registry_with(FakeClock::new(1000.0));
        let v = reg.wait("proc_missing", Some(5), 180).await;
        assert_eq!(v["status"], "not_found");
    }

    #[tokio::test]
    async fn wait_returns_exited_immediately() {
        let reg = registry_with(FakeClock::new(1000.0));
        let h = reg.register(session("proc_wexit"));
        h.append_output("done");
        h.finish(Some(0), "exited", "");
        let v = reg.wait("proc_wexit", Some(5), 180).await;
        assert_eq!(v["status"], "exited");
        assert_eq!(v["output"], "done");
        assert!(reg.is_completion_consumed("proc_wexit"));
    }

    #[tokio::test]
    async fn wait_includes_clamp_note_when_clamped() {
        let reg = registry_with(FakeClock::new(1000.0));
        let h = reg.register(session("proc_wclamp"));
        h.finish(Some(0), "exited", "");
        let v = reg.wait("proc_wclamp", Some(900), 180).await;
        assert!(v["timeout_note"].as_str().unwrap().contains("clamped"));
    }

    #[tokio::test]
    async fn wait_times_out_on_running_process() {
        let reg = registry_with(FakeClock::new(1000.0));
        reg.register(session("proc_wrun"));
        let v = reg.wait("proc_wrun", Some(1), 180).await;
        assert_eq!(v["status"], "timeout");
        assert_eq!(v["process_running"], true);
        let note = v["timeout_note"].as_str().unwrap();
        assert!(note.contains("is not an error"));
        assert!(note.contains("Poll again later"));
    }

    #[tokio::test]
    async fn wait_timeout_mentions_notify_when_set() {
        let reg = registry_with(FakeClock::new(1000.0));
        let mut s = session("proc_wnotify");
        s.notify_on_complete = true;
        reg.register(s);
        let v = reg.wait("proc_wnotify", Some(1), 180).await;
        assert!(v["timeout_note"].as_str().unwrap().contains("notify_on_complete is set"));
    }

    // ── interrupt source ──
    #[derive(Default)]
    struct YesInterrupt;
    impl InterruptSource for YesInterrupt {
        fn is_interrupted(&self) -> bool {
            true
        }
        fn consume_yield(&self, _t: Option<u64>) -> bool {
            false
        }
    }

    #[tokio::test]
    async fn wait_reports_interrupt() {
        let reg = Arc::new(ProcessRegistry::with_parts(
            Arc::new(FakeHost::default()),
            Arc::new(PosixPtyHost::new()),
            Arc::new(NoScope),
            Arc::new(YesInterrupt),
            FakeClock::new(1000.0),
            Arc::new(NoTransform),
        ));
        reg.register(session("proc_int1"));
        let v = reg.wait("proc_int1", Some(5), 180).await;
        assert_eq!(v["status"], "interrupted");
        assert!(v["note"].as_str().unwrap().contains("wait interrupted"));
    }

    // ── kill ──
    #[test]
    fn kill_not_found() {
        let reg = registry_with(FakeClock::new(1000.0));
        assert_eq!(reg.kill_process("proc_x", "process.kill", true)["status"], "not_found");
    }

    #[test]
    fn kill_already_exited() {
        let reg = registry_with(FakeClock::new(1000.0));
        let h = reg.register(session("proc_already"));
        h.finish(Some(0), "exited", "");
        let v = reg.kill_process("proc_already", "process.kill", true);
        assert_eq!(v["status"], "already_exited");
        assert!(v["error"].as_str().unwrap().contains("already finished"));
    }

    #[test]
    fn kill_moves_to_finished_and_records_source() {
        let clock = FakeClock::new(1000.0);
        let reg = registry_with(clock.clone());
        let h = reg.register(session("proc_kill1"));
        {
            h.session.lock().pid = Some(99);
        }
        clock.advance(7.0);
        let v = reg.kill_process("proc_kill1", "process.kill", true);
        assert_eq!(v["status"], "killed");
        assert_eq!(v["completion_reason"], "killed");
        assert_eq!(v["termination_source"], "process.kill");
        assert_eq!(reg.count_running(), 0);
        // finished_at stamped so the TTL clock can start.
        assert!(h.snapshot().exited_at > 0.0);
    }

    // ── write / submit / close ──
    #[test]
    fn write_not_found_and_already_exited() {
        let reg = registry_with(FakeClock::new(1000.0));
        assert_eq!(reg.write_stdin("proc_x", "hi")["status"], "not_found");
        let h = reg.register(session("proc_wx"));
        h.finish(Some(0), "exited", "");
        assert_eq!(reg.write_stdin("proc_wx", "hi")["status"], "already_exited");
    }

    #[test]
    fn write_errors_when_no_pid() {
        let reg = registry_with(FakeClock::new(1000.0));
        reg.register(session("proc_nopid"));
        let v = reg.write_stdin("proc_nopid", "hi");
        assert_eq!(v["status"], "error");
        assert!(v["error"].as_str().unwrap().contains("stdin not available"));
    }

    #[test]
    fn write_sends_raw_bytes() {
        let host = Arc::new(FakeHost::default());
        let reg = Arc::new(ProcessRegistry::with_parts(
            host.clone(),
            Arc::new(PosixPtyHost::new()),
            Arc::new(NoScope),
            Arc::new(NoInterrupt),
            FakeClock::new(1000.0),
            Arc::new(NoTransform),
        ));
        let h = reg.register(session("proc_w1"));
        {
            h.session.lock().pid = Some(7);
        }
        let v = reg.write_stdin("proc_w1", "abc");
        assert_eq!(v["status"], "ok");
        assert_eq!(v["bytes_written"], 3);
        // Raw: no newline appended.
        assert_eq!(&*host.written.lock(), b"abc");
    }

    #[test]
    fn submit_appends_newline() {
        let host = Arc::new(FakeHost::default());
        let reg = Arc::new(ProcessRegistry::with_parts(
            host.clone(),
            Arc::new(PosixPtyHost::new()),
            Arc::new(NoScope),
            Arc::new(NoInterrupt),
            FakeClock::new(1000.0),
            Arc::new(NoTransform),
        ));
        let h = reg.register(session("proc_s1"));
        {
            h.session.lock().pid = Some(7);
        }
        let v = reg.submit_stdin("proc_s1", "y");
        assert_eq!(v["status"], "ok");
        assert_eq!(&*host.written.lock(), b"y\n");
    }

    #[test]
    fn close_not_found_and_ok() {
        let reg = registry_with(FakeClock::new(1000.0));
        assert_eq!(reg.close_stdin("proc_x")["status"], "not_found");
        let h = reg.register(session("proc_c1"));
        {
            h.session.lock().pid = Some(7);
        }
        let v = reg.close_stdin("proc_c1");
        assert_eq!(v["status"], "ok");
        assert_eq!(v["message"], "stdin closed");
    }

    #[test]
    fn close_already_exited() {
        let reg = registry_with(FakeClock::new(1000.0));
        let h = reg.register(session("proc_c2"));
        h.finish(Some(0), "exited", "");
        assert_eq!(reg.close_stdin("proc_c2")["status"], "already_exited");
    }




    // ── layer 3 wiring (registry <-> checkpoint) ──
    #[derive(Debug, Default)]
    struct FakeProbe {
        alive: Mutex<HashSet<i32>>,
        starts: Mutex<std::collections::HashMap<i32, i64>>,
    }
    impl FakeProbe {
        fn with(pid: i32, start: i64) -> Arc<Self> {
            let p = Self::default();
            p.alive.lock().insert(pid);
            p.starts.lock().insert(pid, start);
            Arc::new(p)
        }
    }
    impl crate::process_checkpoint::PidProbe for FakeProbe {
        fn is_alive(&self, pid: i32) -> bool {
            self.alive.lock().contains(&pid)
        }
        fn start_time(&self, pid: i32) -> Option<i64> {
            self.starts.lock().get(&pid).copied()
        }
    }

    fn registry_with_checkpoint(
        probe: Arc<FakeProbe>,
        store: Arc<crate::process_checkpoint::MemoryCheckpointStore>,
    ) -> Arc<ProcessRegistry> {
        Arc::new(
            ProcessRegistry::with_parts(
                Arc::new(FakeHost::default()),
                Arc::new(PosixPtyHost::new()),
                Arc::new(NoScope),
                Arc::new(NoInterrupt),
                FakeClock::new(1_000.0),
                Arc::new(NoTransform),
            )
            .with_checkpoint(
                store,
                Arc::new(crate::process_checkpoint::NoRedact),
                probe,
            ),
        )
    }

    #[test]
    fn registry_write_checkpoint_persists_live_sessions() {
        let probe = Arc::new(FakeProbe::default());
        let store = Arc::new(crate::process_checkpoint::MemoryCheckpointStore::new());
        let reg = registry_with_checkpoint(Arc::clone(&probe), store.clone());
        let mut sess = session("proc_ck1");
        sess.pid = Some(4242);
        sess.host_start_time = Some(777);
        reg.register(sess);

        reg.write_checkpoint(vec![]);
        let text = store.contents().expect("checkpoint written");
        assert!(text.contains("proc_ck1"));
        assert!(text.contains("4242"));
        assert!(text.contains("777"));
    }

    #[test]
    fn registry_checkpoint_omits_exited_sessions() {
        let probe = Arc::new(FakeProbe::default());
        let store = Arc::new(crate::process_checkpoint::MemoryCheckpointStore::new());
        let reg = registry_with_checkpoint(probe, store.clone());
        let h = reg.register(session("proc_ck2"));
        h.finish(Some(0), "exited", "");
        reg.move_to_finished(&h);
        reg.write_checkpoint(vec![]);
        assert_eq!(store.contents().unwrap(), "[]");
    }

    #[test]
    fn registry_recover_adopts_a_matching_live_pid() {
        let probe = FakeProbe::with(4242, 777);
        let store = Arc::new(crate::process_checkpoint::MemoryCheckpointStore::with_contents(
            r#"[{"session_id":"proc_rec1","pid":4242,"pid_scope":"host","host_start_time":777,"command":"sleep 9","started_at":900.0}]"#,
        ));
        let reg = registry_with_checkpoint(probe, store);
        assert_eq!(reg.recover_from_checkpoint(), 1);
        let h = reg.get("proc_rec1").expect("adopted");
        let snap = h.snapshot();
        assert!(snap.detached, "a recovered session has no pipe");
        assert_eq!(snap.pid, Some(4242));
        // It is a live running entry, and poll reports the detached note.
        let v = reg.poll("proc_rec1");
        assert_eq!(v["status"], "running");
        assert_eq!(v["detached"], true);
    }

    #[test]
    fn registry_recover_skips_a_recycled_pid() {
        // Alive, but the kernel start time no longer matches.
        let probe = FakeProbe::with(4242, 999_999);
        let store = Arc::new(crate::process_checkpoint::MemoryCheckpointStore::with_contents(
            r#"[{"session_id":"proc_rec2","pid":4242,"pid_scope":"host","host_start_time":777}]"#,
        ));
        let reg = registry_with_checkpoint(probe, store);
        assert_eq!(reg.recover_from_checkpoint(), 0);
        assert!(reg.get("proc_rec2").is_none());
    }

    #[test]
    fn registry_recover_skips_a_dead_pid() {
        let probe = Arc::new(FakeProbe::default());
        let store = Arc::new(crate::process_checkpoint::MemoryCheckpointStore::with_contents(
            r#"[{"session_id":"proc_rec3","pid":4242,"pid_scope":"host","host_start_time":777}]"#,
        ));
        let reg = registry_with_checkpoint(probe, store);
        assert_eq!(reg.recover_from_checkpoint(), 0);
    }

    #[test]
    fn registry_recover_skips_sandbox_scope() {
        let probe = FakeProbe::with(4242, 777);
        let store = Arc::new(crate::process_checkpoint::MemoryCheckpointStore::with_contents(
            r#"[{"session_id":"proc_rec4","pid":4242,"pid_scope":"sandbox","host_start_time":777}]"#,
        ));
        let reg = registry_with_checkpoint(probe, store);
        assert_eq!(reg.recover_from_checkpoint(), 0);
    }

    #[test]
    fn registry_recover_is_idempotent_for_already_tracked_sessions() {
        let probe = FakeProbe::with(4242, 777);
        let store = Arc::new(crate::process_checkpoint::MemoryCheckpointStore::with_contents(
            r#"[{"session_id":"proc_rec5","pid":4242,"pid_scope":"host","host_start_time":777}]"#,
        ));
        let reg = registry_with_checkpoint(probe, store);
        assert_eq!(reg.recover_from_checkpoint(), 1);
        // A second pass must not adopt the same session twice.
        assert_eq!(reg.recover_from_checkpoint(), 0);
        assert_eq!(reg.count_running(), 1);
    }

    #[test]
    fn registry_recover_reenqueues_pending_watchers() {
        let probe = FakeProbe::with(4242, 777);
        let store = Arc::new(crate::process_checkpoint::MemoryCheckpointStore::with_contents(
            r#"[{"session_id":"proc_rec6","pid":4242,"pid_scope":"host","host_start_time":777,
                 "watcher_interval":30,"session_key":"sk","watcher_platform":"telegram"}]"#,
        ));
        let reg = registry_with_checkpoint(probe, store);
        assert_eq!(reg.recover_from_checkpoint(), 1);
        let watchers = reg.take_pending_watchers();
        assert_eq!(watchers.len(), 1);
        assert_eq!(watchers[0]["session_id"], "proc_rec6");
        assert_eq!(watchers[0]["check_interval"], 30);
        assert_eq!(watchers[0]["platform"], "telegram");
        // Taking them drains the side-channel.
        assert!(reg.take_pending_watchers().is_empty());
    }

    #[test]
    fn registry_recover_without_a_checkpoint_is_a_noop() {
        let probe = Arc::new(FakeProbe::default());
        let store = Arc::new(crate::process_checkpoint::MemoryCheckpointStore::new());
        let reg = registry_with_checkpoint(probe, store);
        assert_eq!(reg.recover_from_checkpoint(), 0);
    }

    #[test]
    fn registry_recover_tolerates_a_malformed_checkpoint() {
        let probe = Arc::new(FakeProbe::default());
        let store = Arc::new(crate::process_checkpoint::MemoryCheckpointStore::with_contents(
            "not json at all",
        ));
        let reg = registry_with_checkpoint(probe, store);
        assert_eq!(reg.recover_from_checkpoint(), 0);
    }

    #[test]
    fn default_checkpoint_path_is_profile_scoped() {
        let p = default_checkpoint_path();
        assert!(p.ends_with("processes.json"), "got {:?}", p);
        // Never a hardcoded ~/.hermes outside the home resolution.
        assert!(p.to_string_lossy().contains(".hermes") || std::env::var("HERMES_HOME").is_ok());
    }

    // ── layer 2 wiring (registry <-> watch pipeline) ──
    #[test]
    fn registry_ingest_routes_a_chunk_through_the_watch_pipeline() {
        let clock = FakeClock::new(1_000.0);
        let reg = registry_with(clock);
        let mut sess = session("proc_w2a");
        sess.watch_patterns = vec!["ERROR".to_string()];
        let h = reg.register(sess);
        reg.ingest_output(&h, "ERROR boom");
        // Buffered by the pipeline...
        assert_eq!(h.snapshot().output_buffer, "ERROR boom");
        // ...and the watch match is queued on the registry.
        let evs = reg.drain_events();
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0]["type"], "watch_match");
        assert!(reg.peek_events().is_empty());
    }

    #[test]
    fn registry_resolves_a_session_prefix_for_arm_heartbeat() {
        let reg = registry_with(FakeClock::new(1_000.0));
        let h = reg.register(session("proc_hb1"));
        assert_eq!(h.id(), "proc_hb1");
        // Any unique prefix works, like the other actions.
        assert_eq!(reg.arm_heartbeat("proc_hb", 5), Some(HEARTBEAT_MIN_SECONDS));
        assert_eq!(reg.arm_heartbeat("proc_missing", 60), None);
    }

    #[test]
    fn registry_heartbeat_tick_emits_for_running_sessions_only() {
        let clock = FakeClock::new(1_000.0);
        let reg = registry_with(clock.clone());
        let h = reg.register(session("proc_hb2"));
        reg.arm_heartbeat("proc_hb2", 60);
        assert!(reg.heartbeat_tick().is_empty(), "not due yet");
        clock.advance(61.0);
        reg.ingest_output(&h, "progress");
        let evs = reg.heartbeat_tick();
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0]["type"], "heartbeat");
        assert_eq!(evs[0]["output"], "progress");
        // A finished session stops beating.
        h.finish(Some(0), "exited", "");
        reg.move_to_finished(&h);
        clock.advance(120.0);
        assert!(reg.heartbeat_tick().is_empty());
    }

    #[test]
    fn registry_output_sink_receives_live_chunks() {
        let reg = registry_with(FakeClock::new(1_000.0));
        let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let seen2 = Arc::clone(&seen);
        reg.set_output_sink(Some(Arc::new(move |_s, chunk| {
            seen2.lock().push(chunk.to_string());
        })));
        let h = reg.register(session("proc_os1"));
        reg.ingest_output(&h, "live line");
        assert_eq!(&*seen.lock(), &vec!["live line".to_string()]);
    }

    #[test]
    fn registered_session_hook_feeds_output_back_into_the_pipeline() {
        let reg = registry_with(FakeClock::new(1_000.0));
        let mut sess = session("proc_hook1");
        sess.watch_patterns = vec!["READY".to_string()];
        let h = reg.register(sess);
        // The reader calls ingest_chunk(); with the hook installed it must reach the
        // registry's watch pipeline, not just the raw buffer.
        h.ingest_chunk("READY to serve");
        let evs = reg.drain_events();
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0]["type"], "watch_match");
        assert_eq!(h.snapshot().output_buffer, "READY to serve");
    }

    #[test]
    fn unregistered_handle_falls_back_to_plain_buffering() {
        // No hook installed: ingest_chunk must still buffer.
        let h = SessionHandle::new(session("proc_nohook"));
        h.ingest_chunk("just text");
        assert_eq!(h.snapshot().output_buffer, "just text");
    }

    // ── PTY routing (the is_pty seam) ──
    #[test]
    fn pty_session_routes_write_to_the_pty_host() {
        let pty = Arc::new(FakePty::default());
        let host = Arc::new(FakeHost::default());
        let reg = Arc::new(ProcessRegistry::with_parts(
            host.clone(),
            pty.clone(),
            Arc::new(NoScope),
            Arc::new(NoInterrupt),
            FakeClock::new(1000.0),
            Arc::new(NoTransform),
        ));
        let h = reg.register(session("proc_pty1"));
        {
            let mut sess = h.session.lock();
            sess.pid = Some(7);
            sess.is_pty = true;
        }
        let v = reg.write_stdin("proc_pty1", "abc");
        assert_eq!(v["status"], "ok");
        // Routed to the PTY, NOT the pipe.
        assert_eq!(&*pty.written.lock(), b"abc");
        assert!(host.written.lock().is_empty());
    }

    #[test]
    fn pty_session_close_sends_eof_and_reports_it() {
        let pty = Arc::new(FakePty::default());
        let reg = registry_with_pty(pty.clone(), FakeClock::new(1000.0));
        let h = reg.register(session("proc_pty2"));
        {
            let mut sess = h.session.lock();
            sess.pid = Some(7);
            sess.is_pty = true;
        }
        let v = reg.close_stdin("proc_pty2");
        assert_eq!(v["status"], "ok");
        assert_eq!(v["message"], "EOF sent");
        assert_eq!(*pty.eofs.lock(), 1);
    }

    #[test]
    fn pipe_session_closes_stdin_not_eof() {
        let pty = Arc::new(FakePty::default());
        let reg = registry_with_pty(pty.clone(), FakeClock::new(1000.0));
        let h = reg.register(session("proc_pipe1"));
        h.session.lock().pid = Some(7);
        let v = reg.close_stdin("proc_pipe1");
        assert_eq!(v["message"], "stdin closed");
        assert_eq!(*pty.eofs.lock(), 0);
    }

    #[test]
    fn submit_on_pty_session_appends_the_posix_newline() {
        let pty = Arc::new(FakePty::default());
        let reg = registry_with_pty(pty.clone(), FakeClock::new(1000.0));
        let h = reg.register(session("proc_pty3"));
        {
            let mut sess = h.session.lock();
            sess.pid = Some(7);
            sess.is_pty = true;
        }
        let v = reg.submit_stdin("proc_pty3", "y");
        assert_eq!(v["status"], "ok");
        // POSIX PTY keeps LF; only a WINDOWS pty switches to CRLF.
        assert_eq!(&*pty.written.lock(), b"y\n");
        assert_eq!(v["bytes_written"], 2);
    }

    #[test]
    fn submit_on_pipe_session_appends_newline() {
        let host = Arc::new(FakeHost::default());
        let reg = Arc::new(ProcessRegistry::with_parts(
            host.clone(),
            Arc::new(PosixPtyHost::new()),
            Arc::new(NoScope),
            Arc::new(NoInterrupt),
            FakeClock::new(1000.0),
            Arc::new(NoTransform),
        ));
        let h = reg.register(session("proc_pipe2"));
        h.session.lock().pid = Some(7);
        let v = reg.submit_stdin("proc_pipe2", "y");
        assert_eq!(v["status"], "ok");
        assert_eq!(&*host.written.lock(), b"y\n");
        assert_eq!(v["bytes_written"], 2);
    }

    #[test]
    fn submit_not_found_and_already_exited() {
        let reg = registry_with(FakeClock::new(1000.0));
        assert_eq!(reg.submit_stdin("proc_nope", "x")["status"], "not_found");
        let h = reg.register(session("proc_subx"));
        h.finish(Some(0), "exited", "");
        assert_eq!(reg.submit_stdin("proc_subx", "x")["status"], "already_exited");
    }

    #[test]
    fn pty_host_defaults_to_unavailable_on_this_platform() {
        let p = PosixPtyHost::new();
        assert_eq!(p.available(), cfg!(unix));
    }

    // ── list ──
    #[test]
    fn list_includes_running_and_finished() {
        let reg = registry_with(FakeClock::new(1000.0));
        let mut a = session("proc_l1");
        a.task_id = "t1".to_string();
        reg.register(a);
        let mut b = session("proc_l2");
        b.task_id = "t1".to_string();
        let hb = reg.register(b);
        hb.finish(Some(0), "exited", "");
        let out = reg.list_sessions(Some("t1"), None, true);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn list_filters_by_task() {
        let reg = registry_with(FakeClock::new(1000.0));
        let mut a = session("proc_f1");
        a.task_id = "t1".to_string();
        reg.register(a);
        let mut b = session("proc_f2");
        b.task_id = "t2".to_string();
        reg.register(b);
        let out = reg.list_sessions(Some("t1"), None, true);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["session_id"], "proc_f1");
    }

    #[test]
    fn list_flags_session_scoped_when_only_session_matches() {
        let reg = registry_with(FakeClock::new(1000.0));
        let mut a = session("proc_sc1");
        a.task_id = "other".to_string();
        a.session_key = "sk1".to_string();
        reg.register(a);
        let out = reg.list_sessions(Some("t1"), Some("sk1"), true);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["session_scoped"], true);
    }

    #[test]
    fn list_previews_are_truncated() {
        let reg = registry_with(FakeClock::new(1000.0));
        let mut a = session("proc_p1");
        a.command = "x".repeat(500);
        a.output_buffer = "y".repeat(500);
        reg.register(a);
        let out = reg.list_sessions(None, None, true);
        assert_eq!(out[0]["command"].as_str().unwrap().chars().count(), LIST_COMMAND_CHARS);
        assert_eq!(out[0]["output_preview"].as_str().unwrap().chars().count(), LIST_PREVIEW_CHARS);
    }

    #[test]
    fn list_owner_task_falls_back_to_task_id() {
        let reg = registry_with(FakeClock::new(1000.0));
        let mut a = session("proc_own1");
        a.task_id = "t9".to_string();
        reg.register(a);
        let out = reg.list_sessions(None, None, true);
        assert_eq!(out[0]["owner_task_id"], "t9");
    }

    // ── lifecycle / pruning ──
    #[test]
    fn move_to_finished_is_idempotent() {
        let clock = FakeClock::new(1000.0);
        let reg = registry_with(clock.clone());
        let h = reg.register(session("proc_m1"));
        h.finish(Some(0), "exited", "");
        assert!(reg.move_to_finished(&h));
        clock.advance(5.0);
        assert!(!reg.move_to_finished(&h));
        // exited_at must not be restamped.
        assert!(h.snapshot().exited_at <= 1001.0);
    }

    #[test]
    fn finished_ttl_expiry_prunes() {
        let clock = FakeClock::new(1000.0);
        let reg = registry_with(clock.clone());
        let h = reg.register(session("proc_ttl"));
        h.finish(Some(0), "exited", "");
        reg.move_to_finished(&h);
        assert!(reg.get("proc_ttl").is_some());
        clock.advance(FINISHED_TTL_SECONDS + 1.0);
        reg.prune_if_needed();
        assert!(reg.get("proc_ttl").is_none());
    }

    #[test]
    fn running_sessions_are_never_pruned_by_ttl() {
        let clock = FakeClock::new(1000.0);
        let reg = registry_with(clock.clone());
        reg.register(session("proc_keep"));
        clock.advance(FINISHED_TTL_SECONDS * 10.0);
        reg.prune_if_needed();
        assert!(reg.get("proc_keep").is_some());
    }

    #[test]
    fn lru_evicts_oldest_finished_past_cap() {
        let clock = FakeClock::new(1000.0);
        let reg = registry_with(clock.clone());
        for i in 0..MAX_PROCESSES {
            let h = reg.register(session(&format!("proc_fin{:03}", i)));
            h.finish(Some(0), "exited", "");
            reg.move_to_finished(&h);
            clock.advance(1.0);
        }
        // One more finished pushes past the cap.
        let h = reg.register(session("proc_fin_last"));
        h.finish(Some(0), "exited", "");
        reg.move_to_finished(&h);
        assert_eq!(reg.get("proc_fin000").is_none(), true);
        assert!(reg.get("proc_fin_last").is_some());
    }

    #[test]
    fn kill_all_filters_by_task() {
        let reg = registry_with(FakeClock::new(1000.0));
        let mut a = session("proc_ka1");
        a.task_id = "t1".to_string();
        let ha = reg.register(a);
        {
            ha.session.lock().pid = Some(11);
        }
        let mut b = session("proc_ka2");
        b.task_id = "t2".to_string();
        let hb = reg.register(b);
        {
            hb.session.lock().pid = Some(12);
        }
        let killed = reg.kill_all(Some("t1"), "kill_all");
        assert_eq!(killed, 1);
        assert_eq!(reg.count_running(), 1);
    }

    #[test]
    fn kill_started_since_uses_baseline() {
        let reg = registry_with(FakeClock::new(1000.0));
        let mut a = session("proc_ks1");
        a.task_id = "t1".to_string();
        let ha = reg.register(a);
        {
            ha.session.lock().pid = Some(21);
        }
        let baseline = reg.snapshot_running_ids("t1");
        let mut b = session("proc_ks2");
        b.task_id = "t1".to_string();
        let hb = reg.register(b);
        {
            hb.session.lock().pid = Some(22);
        }
        let n = reg.kill_started_since("t1", &baseline, "kill_all");
        assert_eq!(n, 1);
        assert!(reg.get("proc_ks1").is_some());
    }

    #[test]
    fn has_active_helpers() {
        let reg = registry_with(FakeClock::new(1000.0));
        let mut a = session("proc_ha1");
        a.task_id = "t1".to_string();
        // Ownership matching is on owner_task_id (the RAW spawning task id), not the
        // collapsed container task_id — the two are set separately by spawn_session.
        a.owner_task_id = "t1".to_string();
        a.session_key = "sk".to_string();
        reg.register(a);
        assert!(reg.has_active_processes("t1"));
        assert!(!reg.has_active_processes("t2"));
        assert!(reg.has_active_for_session("sk", None));
        assert!(!reg.has_active_for_session("other", None));
        assert!(reg.has_any_active());
        assert_eq!(reg.running_owned_by("t1").len(), 1);
        // An entry with only task_id set (no owner) is not owned by anyone.
        let mut b = session("proc_ha2");
        b.task_id = "t9".to_string();
        reg.register(b);
        assert_eq!(reg.running_owned_by("t9").len(), 0);
    }

    // ── new_session_id ──
    #[test]
    fn session_ids_have_proc_prefix_and_length() {
        let id = new_session_id();
        assert!(id.starts_with("proc_"));
        assert_eq!(id.len(), "proc_".len() + 12);
        assert_ne!(new_session_id(), new_session_id());
    }

    // ── schema ──
    #[test]
    fn schema_matches_python_contract() {
        let def = schema();
        assert_eq!(def["name"], "process_manage");
        let actions: Vec<&str> = def["parameters"]["properties"]["action"]["enum"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(
            actions,
            vec!["list", "poll", "log", "wait", "kill", "write", "submit", "close", "handoff"]
        );
        assert_eq!(def["parameters"]["required"][0], "action");
        assert_eq!(def["parameters"]["properties"]["timeout"]["minimum"], 1);
        assert_eq!(def["parameters"]["properties"]["limit"]["minimum"], 1);
        // write-vs-submit is the one real trap and must stay documented.
        let desc = def["description"].as_str().unwrap();
        assert!(desc.contains("submit appends Enter"));
        assert!(desc.contains("write"));
    }

    #[test]
    fn definition_wraps_the_schema_as_a_function() {
        let def = definition();
        assert_eq!(def["type"], "function");
        assert_eq!(def["function"]["name"], "process_manage");
        // Every core tool definition must carry a "function" object; the integration
        // suite asserts this for the whole registry.
        assert!(def["function"].get("parameters").is_some());
        assert_eq!(def["function"], schema());
    }

    #[test]
    fn session_actions_and_output_bearing_flags() {
        assert_eq!(SESSION_ACTIONS.len(), 7);
        assert!(SESSION_ACTIONS.contains(&"poll"));
        assert!(!SESSION_ACTIONS.contains(&"list"));
        assert!(!SESSION_ACTIONS.contains(&"handoff"));
        for a in ["poll", "log", "wait", "kill"] {
            assert!(action_is_output_bearing(a), "{} should be output-bearing", a);
        }
        for a in ["write", "submit", "close"] {
            assert!(!action_is_output_bearing(a), "{} should not be output-bearing", a);
        }
    }

    // ── handler ──
    #[tokio::test]
    async fn handler_unknown_action_lists_options() {
        let reg = registry_with(FakeClock::new(1000.0));
        let ctx = ProcessCtx::new(reg);
        let out = handle_process(&json!({"action": "frobnicate"}), &ctx).await;
        let v: Value = serde_json::from_str(&out).unwrap();
        assert!(v["error"].as_str().unwrap().contains("Unknown process action: frobnicate"));
        assert!(v["error"].as_str().unwrap().contains("list, poll, log, wait, kill, write, submit, close, handoff"));
    }

    #[tokio::test]
    async fn handler_missing_session_id_for_action() {
        let reg = registry_with(FakeClock::new(1000.0));
        let ctx = ProcessCtx::new(reg);
        let out = handle_process(&json!({"action": "poll"}), &ctx).await;
        let v: Value = serde_json::from_str(&out).unwrap();
        assert!(v["error"].as_str().unwrap().contains("session_id is required for poll"));
    }

    #[tokio::test]
    async fn handler_coerces_integer_session_id() {
        let reg = registry_with(FakeClock::new(1000.0));
        reg.register(session("12345"));
        let ctx = ProcessCtx::new(reg);
        let out = handle_process(&json!({"action": "poll", "session_id": 12345}), &ctx).await;
        let v: Value = serde_json::from_str(&out).unwrap();
        // Coerced to "12345", which resolves; status is running rather than not_found.
        assert_eq!(v["status"], "running");
    }

    #[tokio::test]
    async fn handler_list_returns_processes_key() {
        let reg = registry_with(FakeClock::new(1000.0));
        reg.register(session("proc_hl1"));
        let ctx = ProcessCtx::new(reg);
        let out = handle_process(&json!({"action": "list"}), &ctx).await;
        let v: Value = serde_json::from_str(&out).unwrap();
        assert!(v["processes"].is_array());
        assert_eq!(v["processes"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn handler_log_uses_default_limit() {
        let reg = registry_with(FakeClock::new(1000.0));
        let h = reg.register(session("proc_hlog"));
        h.append_output("a\nb");
        let ctx = ProcessCtx::new(reg);
        let out = handle_process(&json!({"action": "log", "session_id": "proc_hlog"}), &ctx).await;
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["output"], "a\nb");
    }

    #[tokio::test]
    async fn handler_handoff_requires_data_then_reports_no_parent() {
        let reg = registry_with(FakeClock::new(1000.0));
        reg.register(session("proc_ho1"));
        let ctx = ProcessCtx::new(reg);
        let out = handle_process(&json!({"action": "handoff", "session_id": "proc_ho1"}), &ctx).await;
        let v: Value = serde_json::from_str(&out).unwrap();
        assert!(v["error"].as_str().unwrap().contains("handoff requires `data`"));

        let out2 = handle_process(
            &json!({"action": "handoff", "session_id": "proc_ho1", "data": "build log"}),
            &ctx,
        )
        .await;
        let v2: Value = serde_json::from_str(&out2).unwrap();
        assert!(v2["error"].as_str().unwrap().contains("only available to a running subagent"));
    }

    #[tokio::test]
    async fn handler_handoff_exited_process_reports_recovery_path() {
        let reg = registry_with(FakeClock::new(1000.0));
        let h = reg.register(session("proc_ho2"));
        h.finish(Some(0), "exited", "");
        let ctx = ProcessCtx::new(reg);
        let out = handle_process(
            &json!({"action": "handoff", "session_id": "proc_ho2", "data": "note"}),
            &ctx,
        )
        .await;
        let v: Value = serde_json::from_str(&out).unwrap();
        assert!(v["error"].as_str().unwrap().contains("read its result with poll/log"));
    }

    #[tokio::test]
    async fn handler_handoff_missing_session_id() {
        let reg = registry_with(FakeClock::new(1000.0));
        let ctx = ProcessCtx::new(reg);
        let out = handle_process(&json!({"action": "handoff"}), &ctx).await;
        let v: Value = serde_json::from_str(&out).unwrap();
        assert!(v["error"].as_str().unwrap().contains("session_id is required for handoff"));
    }

    #[tokio::test]
    async fn handler_kill_returns_killed_payload() {
        let reg = registry_with(FakeClock::new(1000.0));
        let h = reg.register(session("proc_hk"));
        {
            h.session.lock().pid = Some(31);
        }
        let ctx = ProcessCtx::new(reg);
        let out = handle_process(&json!({"action": "kill", "session_id": "proc_hk"}), &ctx).await;
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["status"], "killed");
    }

    #[tokio::test]
    async fn handler_write_is_not_transformed() {
        let reg = registry_with(FakeClock::new(1000.0));
        let h = reg.register(session("proc_hw"));
        {
            h.session.lock().pid = Some(41);
        }
        let ctx = ProcessCtx::new(reg);
        let out = handle_process(
            &json!({"action": "write", "session_id": "proc_hw", "data": "hi"}),
            &ctx,
        )
        .await;
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["status"], "ok");
        assert_eq!(v["bytes_written"], 2);
    }

    // ── transformer seam ──
    struct UpperCaser;
    impl ResultTransformer for UpperCaser {
        fn transform(&self, result: &mut Value) {
            if let Some(o) = result.get_mut("output").and_then(|v| v.as_str()) {
                let up = o.to_uppercase();
                result["output"] = json!(up);
            }
        }
    }

    #[tokio::test]
    async fn transformer_applies_to_output_bearing_actions_only() {
        let reg = Arc::new(ProcessRegistry::with_parts(
            Arc::new(FakeHost::default()),
            Arc::new(PosixPtyHost::new()),
            Arc::new(NoScope),
            Arc::new(NoInterrupt),
            FakeClock::new(1000.0),
            Arc::new(UpperCaser),
        ));
        let h = reg.register(session("proc_tr1"));
        h.append_output("hello");
        let ctx = ProcessCtx::new(reg.clone());
        // `log` is output-bearing and its payload carries the "output" key the
        // transformer rewrites.
        let out =
            handle_process(&json!({"action": "log", "session_id": "proc_tr1"}), &ctx).await;
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["output"], "HELLO");
        // `kill` is output-bearing too, but the process here is running, so read the
        // view the registry itself produces after transformation.
        let direct = reg.transform_result(json!({"output": "hello"}));
        assert_eq!(direct["output"], "HELLO");
    }

    // ── data_arg / coerce ──
    #[test]
    fn data_arg_variants() {
        assert_eq!(data_arg(&json!({"data": "x"})), "x");
        assert_eq!(data_arg(&json!({})), "");
        assert_eq!(data_arg(&json!({"data": null})), "");
        assert_eq!(data_arg(&json!({"data": 5})), "5");
    }

    #[test]
    fn coerce_session_id_variants() {
        assert_eq!(coerce_session_id(&json!({"session_id": "abc"})), "abc");
        assert_eq!(coerce_session_id(&json!({})), "");
        assert_eq!(coerce_session_id(&json!({"session_id": null})), "");
        assert_eq!(coerce_session_id(&json!({"session_id": 7})), "7");
    }

    #[test]
    fn not_found_payload_shape() {
        let v = not_found("proc_zz");
        assert_eq!(v["status"], "not_found");
        assert_eq!(v["error"], "No process with ID proc_zz");
    }

    #[test]
    fn tool_error_payload_shape() {
        let s = tool_error("boom");
        assert_eq!(s, "{\"error\":\"boom\"}");
    }
}
