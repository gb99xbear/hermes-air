//! Watch patterns, the notification rate limiter, the global breaker, and periodic
//! heartbeats for background processes.
//!
//! Layer 2 of the Rust port of `tools/process_registry.py` (the `_check_watch_patterns`
//! / `_heartbeat_*` / `_global_watch_admit` family). The decision logic is pure and
//! clock-injected so every rate-limit path is testable without sleeping.
//!
//! Semantics, preserved exactly:
//! - One match per cooldown window is DELIVERED; further matches inside the window are
//!   dropped and counted.
//! - A strike is a WINDOW with drops, not a drop. `WATCH_STRIKE_LIMIT` consecutive
//!   strike windows permanently disable watching and promote to `notify_on_complete`.
//! - A clean window resets the consecutive-strike counter.
//! - `WATCH_LIFETIME_MAX_HITS` delivered matches also disable watching, independent of
//!   strikes: a pattern recurring just above the cooldown never strikes yet still forces
//!   a full-context agent turn each time.
//! - A global breaker across all sessions caps the fleet-wide rate so concurrent
//!   siblings cannot collectively flood the user.

use parking_lot::Mutex;
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::sync::Arc;

use crate::process::{
    Clock, ProcessSession, SessionHandle, HEARTBEAT_MIN_SECONDS, HEARTBEAT_OUTPUT_CHARS,
    WATCHER_ROUTE_KEYS, WATCH_GLOBAL_COOLDOWN_SECONDS, WATCH_GLOBAL_MAX_PER_WINDOW,
    WATCH_GLOBAL_WINDOW_SECONDS, WATCH_LIFETIME_MAX_HITS, WATCH_MIN_INTERVAL_SECONDS,
    WATCH_STRIKE_LIMIT,
};

/// Cap on the lines a watch_match event carries.
pub const MAX_WATCH_LINES: usize = 20;
/// Cap on the characters a watch_match event carries.
pub const MAX_WATCH_OUTPUT_CHARS: usize = 2_000;

// ── event payloads ─────────────────────────────────────────────────────────────

/// Session identity + watcher routing fields shared by every watch event.
pub fn watch_event_base(session: &ProcessSession) -> Value {
    let mut v = json!({
        "session_id": session.id,
        "session_key": session.session_key,
        "task_id": session.task_id,
        "owner_task_id": if session.owner_task_id.is_empty() {
            session.task_id.clone()
        } else {
            session.owner_task_id.clone()
        },
        "command": session.command,
    });
    for key in WATCHER_ROUTE_KEYS {
        v[key] = json!(session.watcher.get(key));
    }
    v
}

/// Unaddressed (all-sessions) watch breaker event.
pub fn global_watch_event(type_: &str, message: &str, extra: Value) -> Value {
    let mut v = json!({
        "session_id": "",
        "session_key": "",
        "command": "",
        "type": type_,
        "message": message,
        "platform": "",
        "chat_id": "",
        "user_id": "",
        "user_name": "",
        "thread_id": "",
    });
    if let Some(obj) = extra.as_object() {
        for (k, val) in obj {
            v[k] = val.clone();
        }
    }
    v
}

/// The one-shot summary emitted when watching is disabled (strike limit or lifetime cap).
pub fn watch_disabled_event(session: &ProcessSession, suppressed: u32, why: &str) -> Value {
    let mut v = watch_event_base(session);
    v["type"] = json!("watch_disabled");
    v["suppressed"] = json!(suppressed);
    v["message"] = json!(format!(
        "Watch patterns disabled for process {} — {}Falling back to notify_on_complete semantics; you'll get exactly one notification when the process exits.",
        session.id, why
    ));
    v
}

/// A `watch_match` event payload.
pub fn watch_match_event(
    session: &ProcessSession,
    pattern: &str,
    lines: &[String],
    suppressed: u32,
) -> Value {
    let mut output = lines
        .iter()
        .take(MAX_WATCH_LINES)
        .cloned()
        .collect::<Vec<_>>()
        .join("\n");
    if output.chars().count() > MAX_WATCH_OUTPUT_CHARS {
        output = format!(
            "{}\n...(truncated)",
            crate::process::head_chars(&output, MAX_WATCH_OUTPUT_CHARS)
        );
    }
    let mut v = watch_event_base(session);
    v["type"] = json!("watch_match");
    v["pattern"] = json!(pattern);
    v["output"] = json!(output);
    v["suppressed"] = json!(suppressed);
    v
}

/// A `heartbeat` event payload.
pub fn heartbeat_event(session: &ProcessSession, seq: u64, now: f64, output: String) -> Value {
    let mut v = watch_event_base(session);
    v["type"] = json!("heartbeat");
    v["seq"] = json!(seq);
    v["interval"] = json!(session.heartbeat_seconds);
    v["elapsed"] = json!(if session.started_at > 0.0 {
        (now - session.started_at).floor() as i64
    } else {
        0
    });
    v["output"] = json!(output);
    v["started_at"] = json!(session.started_at);
    v
}

// ── global breaker ─────────────────────────────────────────────────────────────

/// Fleet-wide watch-match rate limiter. Caps total deliveries per rolling window; on
/// breach it trips for a cooldown, emitting ONE tripped summary and ONE released
/// summary (with the suppressed count) when the cooldown ends.
#[derive(Debug)]
pub struct GlobalWatchBreaker {
    window_start: f64,
    window_hits: u32,
    tripped_until: f64,
    suppressed_during_trip: u32,
}

impl Default for GlobalWatchBreaker {
    fn default() -> Self {
        Self::new()
    }
}

impl GlobalWatchBreaker {
    pub fn new() -> Self {
        Self {
            window_start: 0.0,
            window_hits: 0,
            tripped_until: 0.0,
            suppressed_during_trip: 0,
        }
    }

    /// True if this `watch_match` may pass. Returns any summary events to queue,
    /// outside the lock.
    pub fn admit(&mut self, now: f64) -> (bool, Vec<Value>) {
        let mut events: Vec<Value> = Vec::new();

        // Handle cooldown expiry first so the release summary can be emitted.
        if self.tripped_until > 0.0 && now >= self.tripped_until {
            let suppressed = self.suppressed_during_trip;
            self.tripped_until = 0.0;
            self.suppressed_during_trip = 0;
            self.window_start = now;
            self.window_hits = 0;
            if suppressed > 0 {
                events.push(global_watch_event(
                    "watch_overflow_released",
                    &format!(
                        "Watch-pattern notifications resumed. {} match event(s) were suppressed during the flood.",
                        suppressed
                    ),
                    json!({ "suppressed": suppressed }),
                ));
            }
        }

        if self.tripped_until > 0.0 && now < self.tripped_until {
            // Still in cooldown: drop and count.
            self.suppressed_during_trip += 1;
            return (false, events);
        }

        if now - self.window_start >= WATCH_GLOBAL_WINDOW_SECONDS {
            self.window_start = now;
            self.window_hits = 0;
        }
        if self.window_hits < WATCH_GLOBAL_MAX_PER_WINDOW {
            self.window_hits += 1;
            (true, events)
        } else {
            self.tripped_until = now + WATCH_GLOBAL_COOLDOWN_SECONDS;
            self.suppressed_during_trip += 1;
            events.push(global_watch_event(
                "watch_overflow_tripped",
                &format!(
                    "Watch-pattern overflow: >{} notifications in {}s across all processes. Suppressing further watch_match events for {}s.",
                    WATCH_GLOBAL_MAX_PER_WINDOW, WATCH_GLOBAL_WINDOW_SECONDS, WATCH_GLOBAL_COOLDOWN_SECONDS
                ),
                json!({}),
            ));
            (false, events)
        }
    }
}

// ── per-session watch limiter ──────────────────────────────────────────────────

/// Find the watch-pattern matches in one freshly-read chunk.
///
/// Matching is plain substring containment (`p in line`), NOT regex — the field is
/// called "patterns" but Python matches substrings. Returns `(first_pattern, line)`
/// per matching line, first match wins per line.
pub fn match_watch_lines(patterns: &[String], new_text: &str) -> Vec<(String, String)> {
    let mut hits = Vec::new();
    for line in new_text.split('\n') {
        if let Some(p) = patterns.iter().find(|p| line.contains(p.as_str())) {
            hits.push((p.clone(), line.trim_end().to_string()));
        }
    }
    hits
}

/// Scan a freshly-read chunk for watch patterns and return the events to queue.
///
/// `admit` is the global breaker; wiring it as a closure keeps this function pure and
/// lets tests observe the breaker independently.
pub fn check_watch_patterns(
    session: &mut ProcessSession,
    new_text: &str,
    now: f64,
    admit: &mut dyn FnMut(f64) -> bool,
) -> Vec<Value> {
    let mut events: Vec<Value> = Vec::new();
    if session.watch_patterns.is_empty() || session.watch_disabled {
        return events;
    }
    // Late chunks after the reader declared exit are post-exit noise; dropping them
    // avoids stale notifications minutes after the process ended.
    if session.exited {
        return events;
    }

    let hits = match_watch_lines(&session.watch_patterns, new_text);
    if hits.is_empty() {
        return events;
    }
    let matched_pattern = hits[0].0.clone();
    let matched_lines: Vec<String> = hits.iter().map(|(_, l)| l.clone()).collect();

    let suppressed;

    if session.watch_cooldown_until > 0.0 && now < session.watch_cooldown_until {
        // Inside the cooldown: drop, count one strike per window, disable + promote
        // once the strike limit is hit.
        session.watch_suppressed += matched_lines.len() as u32;
        if session.watch_strike_candidate {
            return events;
        }
        session.watch_strike_candidate = true;
        session.watch_consecutive_strikes += 1;
        if session.watch_consecutive_strikes < WATCH_STRIKE_LIMIT {
            return events;
        }
        session.watch_disabled = true;
        // Promote so the agent still gets exactly one notification on exit, plus
        // exactly one summary so it sees why things went quiet.
        session.notify_on_complete = true;
        events.push(watch_disabled_event(
            session,
            session.watch_suppressed,
            &format!(
                "{} consecutive rate-limit windows triggered (min spacing {}s). ",
                WATCH_STRIKE_LIMIT, WATCH_MIN_INTERVAL_SECONDS
            ),
        ));
        return events;
    }

    // Cooldown expired. A prior window with no drops resets the consecutive-strike
    // counter (healthy cadence again).
    if session.watch_cooldown_until > 0.0 && !session.watch_strike_candidate {
        session.watch_consecutive_strikes = 0;
    }
    session.watch_strike_candidate = false;
    // Emit and start a new cooldown window.
    session.watch_cooldown_until = now + WATCH_MIN_INTERVAL_SECONDS;
    session.watch_hits += 1;
    suppressed = session.watch_suppressed;
    session.watch_suppressed = 0;
    // Lifetime cap: this match is still delivered, but no further ones.
    let lifetime_exhausted = session.watch_hits >= WATCH_LIFETIME_MAX_HITS;
    if lifetime_exhausted {
        session.watch_disabled = true;
        session.notify_on_complete = true;
    }

    if admit(now) {
        events.push(watch_match_event(
            session,
            &matched_pattern,
            &matched_lines,
            suppressed,
        ));
    }
    // Even when the breaker drops the final match, still explain the silence.
    if lifetime_exhausted {
        events.push(watch_disabled_event(
            session,
            0,
            &format!(
                "reached the lifetime cap of {} delivered matches. ",
                WATCH_LIFETIME_MAX_HITS
            ),
        ));
    }
    events
}

// ── heartbeat ──────────────────────────────────────────────────────────────────

/// Enable periodic heartbeat events for a session; returns the effective interval.
pub fn arm_heartbeat(session: &mut ProcessSession, seconds: u64, now: f64) -> u64 {
    let effective = seconds.max(HEARTBEAT_MIN_SECONDS);
    session.heartbeat_seconds = effective;
    session.heartbeat_last = now;
    session.heartbeat_total_at_last = session.total_output_chars;
    effective
}

/// Whether a heartbeat is due for this session.
pub fn heartbeat_due(session: &ProcessSession, now: f64) -> bool {
    session.heartbeat_seconds > 0
        && !session.exited
        && now - session.heartbeat_last >= session.heartbeat_seconds as f64
}

/// Build the heartbeat event for a session, advancing its bookkeeping.
///
/// The output slice is what arrived since the previous heartbeat, capped. A heartbeat
/// is time-driven, so it is bounded by construction (<= 3600/HEARTBEAT_MIN_SECONDS
/// events per hour per process) and needs no strike/lifetime breaker.
pub fn emit_heartbeat(session: &mut ProcessSession, now: f64) -> Value {
    let delta = session
        .total_output_chars
        .saturating_sub(session.heartbeat_total_at_last);
    let mut output = if delta > 0 {
        crate::process::tail_chars(&session.output_buffer, delta as usize)
    } else {
        String::new()
    };
    session.heartbeat_total_at_last = session.total_output_chars;
    if output.chars().count() > HEARTBEAT_OUTPUT_CHARS {
        let cut = output.chars().count() - HEARTBEAT_OUTPUT_CHARS;
        output = format!(
            "...({} earlier characters omitted)\n{}",
            cut,
            crate::process::tail_chars(&output, HEARTBEAT_OUTPUT_CHARS)
        );
    }
    session.heartbeat_last = now;
    session.heartbeat_seq += 1;
    heartbeat_event(session, session.heartbeat_seq, now, output)
}

// ── event queue ────────────────────────────────────────────────────────────────

/// Unified queue for all background events (distinguished by `type`). The CLI
/// process loop and the gateway drain it after each agent turn to trigger new turns.
#[derive(Debug, Default)]
pub struct EventQueue {
    inner: Mutex<VecDeque<Value>>,
}

impl EventQueue {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&self, event: Value) {
        self.inner.lock().push_back(event);
    }

    pub fn push_all(&self, events: Vec<Value>) {
        let mut q = self.inner.lock();
        for e in events {
            q.push_back(e);
        }
    }

    /// Take every queued event, in order.
    pub fn drain(&self) -> Vec<Value> {
        let mut q = self.inner.lock();
        q.drain(..).collect()
    }

    /// Look at the queued events without removing them.
    pub fn snapshot(&self) -> Vec<Value> {
        self.inner.lock().iter().cloned().collect()
    }

    pub fn len(&self) -> usize {
        self.inner.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.lock().is_empty()
    }
}

/// Live-output sink: `on_output(session, chunk)` streams output from reader threads
/// (the desktop gateway's live terminal). Called from reader threads; never raises.
pub type OutputSink = Arc<dyn Fn(&ProcessSession, &str) + Send + Sync>;

// ── the pipeline ───────────────────────────────────────────────────────────────

/// Wires output ingestion to watch checks, the global breaker, live streaming, and the
/// event queue. Held by the registry; the reader hook closes over an `Arc` of it.
pub struct WatchPipeline {
    pub events: Arc<EventQueue>,
    pub breaker: Arc<Mutex<GlobalWatchBreaker>>,
    pub on_output: Arc<Mutex<Option<OutputSink>>>,
    clock: Arc<dyn Clock>,
}

impl WatchPipeline {
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Self {
            events: Arc::new(EventQueue::new()),
            breaker: Arc::new(Mutex::new(GlobalWatchBreaker::new())),
            on_output: Arc::new(Mutex::new(None)),
            clock,
        }
    }

    /// Install (or clear) the live-output sink.
    pub fn set_output_sink(&self, sink: Option<OutputSink>) {
        *self.on_output.lock() = sink;
    }

    /// Buffer a freshly-read chunk, then scan watch patterns and stream it live.
    /// Mirrors `_ingest_output`.
    pub fn ingest_text(&self, handle: &Arc<SessionHandle>, text: &str) {
        let now = self.clock.now_secs();
        let events = {
            let mut session = handle.session.lock();
            session.append_output(text);
            let mut admit = |t: f64| {
                let (ok, evs) = self.breaker.lock().admit(t);
                self.events.push_all(evs);
                ok
            };
            check_watch_patterns(&mut session, text, now, &mut admit)
        };
        self.events.push_all(events);

        // Live stream to the driver-installed sink (never raises).
        let sink = self.on_output.lock().clone();
        if let Some(sink) = sink {
            if !text.is_empty() {
                let snapshot = handle.snapshot();
                sink(&snapshot, text);
            }
        }
    }

    /// Enable heartbeats on a session; returns the effective interval.
    pub fn arm_heartbeat(&self, handle: &Arc<SessionHandle>, seconds: u64) -> u64 {
        let now = self.clock.now_secs();
        arm_heartbeat(&mut handle.session.lock(), seconds, now)
    }

    /// One pass of the heartbeat loop: emit for every session whose interval elapsed.
    /// Returns the events emitted, and also queues them.
    pub fn heartbeat_tick<'a, I>(&self, sessions: I) -> Vec<Value>
    where
        I: IntoIterator<Item = &'a Arc<SessionHandle>>,
    {
        let now = self.clock.now_secs();
        let mut out = Vec::new();
        for handle in sessions {
            let mut s = handle.session.lock();
            if heartbeat_due(&s, now) {
                out.push(emit_heartbeat(&mut s, now));
            }
        }
        self.events.push_all(out.clone());
        out
    }

    /// Take every queued event.
    pub fn drain(&self) -> Vec<Value> {
        self.events.drain()
    }
}

// ── tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Default)]
    struct FakeClock {
        t: Mutex<f64>,
    }
    impl FakeClock {
        fn new(t: f64) -> Arc<Self> {
            Arc::new(Self { t: Mutex::new(t) })
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

    fn session_with(patterns: &[&str]) -> ProcessSession {
        let mut s = ProcessSession::new("proc_watch1", "tail -f app.log", 1_000.0);
        s.watch_patterns = patterns.iter().map(|p| p.to_string()).collect();
        s
    }

    // ── match_watch_lines ──
    #[test]
    fn matches_are_substrings_not_regex() {
        let pats = vec!["ERROR".to_string()];
        // A regex would treat "." specially; Python uses plain containment.
        let hits = match_watch_lines(&pats, "x.x ERROR happened\nok");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].1, "x.x ERROR happened");
    }

    #[test]
    fn first_matching_pattern_wins_per_line() {
        let pats = vec!["WARN".to_string(), "WARNING".to_string()];
        let hits = match_watch_lines(&pats, "WARNING: disk");
        assert_eq!(hits[0].0, "WARN");
    }

    #[test]
    fn one_hit_per_line_even_with_repeats() {
        let pats = vec!["ERROR".to_string()];
        let hits = match_watch_lines(&pats, "ERROR ERROR ERROR");
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn no_match_gives_no_hits() {
        let pats = vec!["ERROR".to_string()];
        assert!(match_watch_lines(&pats, "all good\nfine").is_empty());
    }

    #[test]
    fn matched_line_is_right_trimmed() {
        let pats = vec!["ERROR".to_string()];
        let hits = match_watch_lines(&pats, "ERROR here   \n");
        assert_eq!(hits[0].1, "ERROR here");
    }

    // ── check_watch_patterns: delivery + cooldown ──
    #[test]
    fn delivers_first_match_and_starts_cooldown() {
        let mut s = session_with(&["ERROR"]);
        let mut admit = |_t| true;
        let evs = check_watch_patterns(&mut s, "ERROR boom", 1_000.0, &mut admit);
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0]["type"], "watch_match");
        assert_eq!(evs[0]["pattern"], "ERROR");
        assert_eq!(evs[0]["output"], "ERROR boom");
        assert_eq!(evs[0]["suppressed"], 0);
        assert_eq!(s.watch_hits, 1);
        assert_eq!(s.watch_cooldown_until, 1_000.0 + WATCH_MIN_INTERVAL_SECONDS);
    }

    #[test]
    fn drops_inside_cooldown_and_counts_suppressed() {
        let mut s = session_with(&["ERROR"]);
        let mut admit = |_t| true;
        check_watch_patterns(&mut s, "ERROR one", 1_000.0, &mut admit);
        let evs = check_watch_patterns(&mut s, "ERROR two", 1_001.0, &mut admit);
        assert!(evs.is_empty());
        assert_eq!(s.watch_suppressed, 1);
        assert_eq!(s.watch_hits, 1);
    }

    #[test]
    fn second_match_in_same_window_counts_one_strike_not_two() {
        let mut s = session_with(&["ERROR"]);
        let mut admit = |_t| true;
        check_watch_patterns(&mut s, "ERROR a", 1_000.0, &mut admit);
        check_watch_patterns(&mut s, "ERROR b", 1_001.0, &mut admit);
        check_watch_patterns(&mut s, "ERROR c", 1_002.0, &mut admit);
        // Two drops in ONE window is one strike.
        assert_eq!(s.watch_consecutive_strikes, 1);
        assert_eq!(s.watch_suppressed, 2);
    }

    #[test]
    fn clean_window_resets_consecutive_strikes() {
        let mut s = session_with(&["ERROR"]);
        let mut admit = |_t| true;
        check_watch_patterns(&mut s, "ERROR a", 1_000.0, &mut admit); // window 1 delivered
        check_watch_patterns(&mut s, "ERROR b", 1_001.0, &mut admit); // strike window
        assert_eq!(s.watch_consecutive_strikes, 1);
        // The reset only happens when a DELIVERED window had no drops: the strike
        // candidate from that window must be clear. A window that delivers while the
        // previous window dropped keeps the strike standing.
        let evs = check_watch_patterns(&mut s, "ERROR c", 1_020.0, &mut admit);
        assert_eq!(evs.len(), 1, "this window still delivers");
        assert_eq!(
            s.watch_consecutive_strikes, 1,
            "strike stands until a clean window"
        );
        // Now a genuinely clean delivered window (no drops in the window before it).
        let evs2 = check_watch_patterns(&mut s, "ERROR d", 1_040.0, &mut admit);
        assert_eq!(evs2.len(), 1);
        assert_eq!(s.watch_consecutive_strikes, 0, "clean window resets");
    }

    #[test]
    fn strike_limit_disables_watching_and_promotes() {
        let mut s = session_with(&["ERROR"]);
        let mut admit = |_t| true;
        // A "strike window" is one cooldown window that contained at least one drop.
        // Build them explicitly: deliver, then drop inside the same window, then wait
        // out the cooldown before the next window starts.
        let step = WATCH_MIN_INTERVAL_SECONDS + 1.0;
        let mut t = 1_000.0;
        let mut saw_disabled = false;
        for _ in 0..WATCH_STRIKE_LIMIT {
            // Start a fresh window with a delivered match.
            check_watch_patterns(&mut s, "ERROR open", t, &mut admit);
            // Drop inside the window -> this window is a strike.
            let evs = check_watch_patterns(&mut s, "ERROR extra", t + 0.5, &mut admit);
            if evs.iter().any(|e| e["type"] == "watch_disabled") {
                saw_disabled = true;
                break;
            }
            t += step;
        }
        assert!(
            s.watch_disabled,
            "watching should be disabled at the strike limit"
        );
        assert!(s.notify_on_complete, "must promote to notify_on_complete");
        assert_eq!(s.watch_consecutive_strikes, WATCH_STRIKE_LIMIT);
        assert!(saw_disabled, "a watch_disabled summary must be emitted");
    }

    #[test]
    fn disabled_session_stops_matching() {
        let mut s = session_with(&["ERROR"]);
        s.watch_disabled = true;
        let mut admit = |_t| true;
        let evs = check_watch_patterns(&mut s, "ERROR x", 1_000.0, &mut admit);
        assert!(evs.is_empty());
        assert_eq!(s.watch_hits, 0);
    }

    #[test]
    fn no_patterns_no_events() {
        let mut s = session_with(&[]);
        let mut admit = |_t| true;
        assert!(check_watch_patterns(&mut s, "ERROR x", 1_000.0, &mut admit).is_empty());
    }

    #[test]
    fn exited_session_ignores_late_chunks() {
        let mut s = session_with(&["ERROR"]);
        s.exited = true;
        let mut admit = |_t| true;
        assert!(check_watch_patterns(&mut s, "ERROR after exit", 1_000.0, &mut admit).is_empty());
    }

    #[test]
    fn lifetime_cap_delivers_then_disables() {
        let mut s = session_with(&["TICK"]);
        let mut admit = |_t| true;
        // Deliver WATCH_LIFETIME_MAX_HITS times, each in its own window.
        for i in 0..WATCH_LIFETIME_MAX_HITS {
            let t = 1_000.0 + (i as f64) * (WATCH_MIN_INTERVAL_SECONDS + 1.0);
            let evs = check_watch_patterns(&mut s, "TICK", t, &mut admit);
            let has_match = evs.iter().any(|e| e["type"] == "watch_match");
            assert!(has_match, "match {} should still be delivered", i);
            if i + 1 == WATCH_LIFETIME_MAX_HITS {
                assert!(
                    evs.iter().any(|e| e["type"] == "watch_disabled"),
                    "the final match must also explain the coming silence"
                );
                assert!(
                    evs[0]["type"] == "watch_match",
                    "match first, then the notice"
                );
            }
        }
        assert!(s.watch_disabled);
        assert!(s.notify_on_complete);
    }

    #[test]
    fn breaker_refusal_still_explains_lifetime_cap() {
        let mut s = session_with(&["TICK"]);
        let mut admit = |_t| false; // breaker always drops
        let mut last = Vec::new();
        for i in 0..WATCH_LIFETIME_MAX_HITS {
            let t = 1_000.0 + (i as f64) * (WATCH_MIN_INTERVAL_SECONDS + 1.0);
            last = check_watch_patterns(&mut s, "TICK", t, &mut admit);
        }
        assert!(
            last.iter().any(|e| e["type"] == "watch_disabled"),
            "even a dropped final match must explain the silence"
        );
        assert!(!last.iter().any(|e| e["type"] == "watch_match"));
    }

    // ── watch_match payload ──
    #[test]
    fn watch_match_caps_lines_and_chars() {
        let s = session_with(&["E"]);
        let lines: Vec<String> = (0..40).map(|i| format!("line {}", i)).collect();
        let ev = watch_match_event(&s, "E", &lines, 0);
        assert_eq!(
            ev["output"].as_str().unwrap().lines().count(),
            MAX_WATCH_LINES
        );

        let long: Vec<String> = vec!["x".repeat(5_000)];
        let ev2 = watch_match_event(&s, "E", &long, 0);
        let out = ev2["output"].as_str().unwrap();
        assert!(out.ends_with("...(truncated)"));
    }

    #[test]
    fn watch_match_carries_watcher_routing() {
        let mut s = session_with(&["E"]);
        s.watcher.platform = "telegram".to_string();
        s.watcher.chat_id = "-100".to_string();
        s.session_key = "sk".to_string();
        s.task_id = "t1".to_string();
        let ev = watch_match_event(&s, "E", &["E".to_string()], 3);
        assert_eq!(ev["platform"], "telegram");
        assert_eq!(ev["chat_id"], "-100");
        assert_eq!(ev["session_key"], "sk");
        assert_eq!(ev["owner_task_id"], "t1");
        assert_eq!(ev["suppressed"], 3);
    }

    // ── global breaker ──
    #[test]
    fn breaker_admits_up_to_the_cap() {
        let mut b = GlobalWatchBreaker::new();
        for i in 0..WATCH_GLOBAL_MAX_PER_WINDOW {
            let (ok, evs) = b.admit(1_000.0 + (i as f64) * 0.01);
            assert!(ok, "hit {} within the window should pass", i);
            assert!(evs.is_empty());
        }
    }

    #[test]
    fn breaker_trips_over_the_cap_and_emits_tripped() {
        let mut b = GlobalWatchBreaker::new();
        for i in 0..WATCH_GLOBAL_MAX_PER_WINDOW {
            b.admit(1_000.0 + (i as f64) * 0.01);
        }
        let (ok, evs) = b.admit(1_000.5);
        assert!(!ok, "the over-cap hit must be dropped");
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0]["type"], "watch_overflow_tripped");
        assert_eq!(evs[0]["session_id"], "");
        assert!(evs[0]["message"].as_str().unwrap().contains("overflow"));
    }

    #[test]
    fn breaker_drops_during_cooldown_and_emits_released_once() {
        let mut b = GlobalWatchBreaker::new();
        for i in 0..WATCH_GLOBAL_MAX_PER_WINDOW {
            b.admit(1_000.0 + (i as f64) * 0.01);
        }
        b.admit(1_000.5); // trips
                          // Two more hits inside the cooldown: dropped, no events.
        let (ok1, e1) = b.admit(1_001.0);
        let (ok2, e2) = b.admit(1_002.0);
        assert!(!ok1 && !ok2);
        assert!(e1.is_empty() && e2.is_empty());
        // Cooldown expires: one released summary carrying the suppressed count.
        let (ok3, e3) = b.admit(1_000.5 + WATCH_GLOBAL_COOLDOWN_SECONDS + 0.1);
        assert!(ok3);
        assert_eq!(e3.len(), 1);
        assert_eq!(e3[0]["type"], "watch_overflow_released");
        // 1 from the trip itself + 2 during cooldown.
        assert_eq!(e3[0]["suppressed"], 3);
    }

    #[test]
    fn breaker_window_slides_after_the_window_elapses() {
        let mut b = GlobalWatchBreaker::new();
        for i in 0..WATCH_GLOBAL_MAX_PER_WINDOW {
            b.admit(1_000.0 + (i as f64) * 0.01);
        }
        // A fresh window well past the window length: allowed again.
        let (ok, evs) = b.admit(1_000.0 + WATCH_GLOBAL_WINDOW_SECONDS + 1.0);
        assert!(ok);
        assert!(evs.is_empty());
    }

    // ── heartbeat ──
    #[test]
    fn arm_heartbeat_raises_to_the_floor() {
        let mut s = session_with(&[]);
        let effective = arm_heartbeat(&mut s, 5, 1_000.0);
        assert_eq!(effective, HEARTBEAT_MIN_SECONDS);
        assert_eq!(s.heartbeat_seconds, HEARTBEAT_MIN_SECONDS);
        assert_eq!(s.heartbeat_last, 1_000.0);
    }

    #[test]
    fn arm_heartbeat_keeps_a_larger_interval() {
        let mut s = session_with(&[]);
        assert_eq!(arm_heartbeat(&mut s, 120, 1_000.0), 120);
    }

    #[test]
    fn heartbeat_not_due_before_the_interval() {
        let mut s = session_with(&[]);
        arm_heartbeat(&mut s, 60, 1_000.0);
        assert!(!heartbeat_due(&s, 1_030.0));
    }

    #[test]
    fn heartbeat_due_at_the_interval() {
        let mut s = session_with(&[]);
        arm_heartbeat(&mut s, 60, 1_000.0);
        assert!(heartbeat_due(&s, 1_060.0));
    }

    #[test]
    fn heartbeat_never_due_when_off_or_exited() {
        let mut s = session_with(&[]);
        assert!(!heartbeat_due(&s, 9_999.0)); // 0 = off
        arm_heartbeat(&mut s, 60, 1_000.0);
        s.exited = true;
        assert!(!heartbeat_due(&s, 2_000.0));
    }

    #[test]
    fn emit_heartbeat_reports_only_new_output() {
        let mut s = session_with(&[]);
        arm_heartbeat(&mut s, 60, 1_000.0);
        s.append_output("first chunk");
        let ev = emit_heartbeat(&mut s, 1_060.0);
        assert_eq!(ev["type"], "heartbeat");
        assert_eq!(ev["seq"], 1);
        assert_eq!(ev["interval"], 60);
        assert_eq!(ev["elapsed"], 60);
        assert_eq!(ev["output"], "first chunk");

        // A second heartbeat with no new output carries an empty slice.
        let ev2 = emit_heartbeat(&mut s, 1_120.0);
        assert_eq!(ev2["seq"], 2);
        assert_eq!(ev2["output"], "");
        assert_eq!(ev2["elapsed"], 120);
    }

    #[test]
    fn emit_heartbeat_caps_the_slice() {
        let mut s = session_with(&[]);
        arm_heartbeat(&mut s, 60, 1_000.0);
        s.append_output(&"z".repeat(HEARTBEAT_OUTPUT_CHARS + 500));
        let ev = emit_heartbeat(&mut s, 1_060.0);
        let out = ev["output"].as_str().unwrap();
        assert!(out.starts_with("...(500 earlier characters omitted)"));
    }

    #[test]
    fn heartbeat_output_survives_a_rolling_buffer_truncation() {
        let mut s = session_with(&[]);
        s.max_output_chars = 10;
        arm_heartbeat(&mut s, 60, 1_000.0);
        s.append_output("0123456789ABCDEF"); // 16 chars in, buffer keeps the last 10
        assert_eq!(s.total_output_chars, 16);
        assert_eq!(s.output_buffer, "6789ABCDEF");
        let ev = emit_heartbeat(&mut s, 1_060.0);
        // The delta (16) asks for more than the rolling tail retains, so the beat
        // carries exactly what is left — never a panic, never stale bytes.
        assert_eq!(ev["output"], "6789ABCDEF");
    }

    // ── event queue ──
    #[test]
    fn queue_preserves_order_and_drains() {
        let q = EventQueue::new();
        assert!(q.is_empty());
        q.push(json!({"type": "a"}));
        q.push(json!({"type": "b"}));
        assert_eq!(q.len(), 2);
        let drained = q.drain();
        assert_eq!(drained[0]["type"], "a");
        assert_eq!(drained[1]["type"], "b");
        assert!(q.is_empty());
    }

    #[test]
    fn queue_snapshot_does_not_consume() {
        let q = EventQueue::new();
        q.push(json!({"type": "a"}));
        assert_eq!(q.snapshot().len(), 1);
        assert_eq!(q.len(), 1);
    }

    // ── pipeline ──
    fn pipeline(clock: Arc<dyn Clock>) -> WatchPipeline {
        WatchPipeline::new(clock)
    }

    fn handle_for(session: ProcessSession) -> Arc<SessionHandle> {
        SessionHandle::new(session)
    }

    #[test]
    fn ingest_appends_output_and_queues_a_watch_match() {
        let clock = FakeClock::new(1_000.0);
        let p = pipeline(clock);
        let h = handle_for(session_with(&["ERROR"]));
        p.ingest_text(&h, "ERROR boom");
        assert_eq!(h.snapshot().output_buffer, "ERROR boom");
        let evs = p.drain();
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0]["type"], "watch_match");
    }

    #[test]
    fn ingest_without_patterns_queues_nothing() {
        let p = pipeline(FakeClock::new(1_000.0));
        let h = handle_for(session_with(&[]));
        p.ingest_text(&h, "hello");
        assert!(p.drain().is_empty());
        assert_eq!(h.snapshot().output_buffer, "hello");
    }

    #[test]
    fn ingest_streams_to_the_installed_sink() {
        let p = pipeline(FakeClock::new(1_000.0));
        let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let seen2 = Arc::clone(&seen);
        p.set_output_sink(Some(Arc::new(move |_s, chunk| {
            seen2.lock().push(chunk.to_string());
        })));
        let h = handle_for(session_with(&[]));
        p.ingest_text(&h, "chunk one");
        p.ingest_text(&h, "chunk two");
        assert_eq!(
            &*seen.lock(),
            &vec!["chunk one".to_string(), "chunk two".to_string()]
        );
    }

    #[test]
    fn ingest_with_no_sink_is_silent() {
        let p = pipeline(FakeClock::new(1_000.0));
        let h = handle_for(session_with(&[]));
        p.ingest_text(&h, "nothing to stream to");
        assert!(p.drain().is_empty());
    }

    #[test]
    fn pipeline_arm_heartbeat_and_tick() {
        let clock = FakeClock::new(1_000.0);
        let p = pipeline(clock.clone());
        let h = handle_for(session_with(&[]));
        assert_eq!(p.arm_heartbeat(&h, 60), 60);
        // Not due yet.
        assert!(p.heartbeat_tick([&h]).is_empty());
        clock.advance(61.0);
        p.ingest_text(&h, "progress line");
        let evs = p.heartbeat_tick([&h]);
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0]["type"], "heartbeat");
        assert_eq!(evs[0]["output"], "progress line");
        // Also queued for the drain loop.
        let queued = p.drain();
        assert!(queued.iter().any(|e| e["type"] == "heartbeat"));
    }

    #[test]
    fn pipeline_heartbeat_tick_skips_sessions_without_heartbeat() {
        let clock = FakeClock::new(1_000.0);
        let p = pipeline(clock.clone());
        let h = handle_for(session_with(&[])); // heartbeat_seconds = 0
        clock.advance(10_000.0);
        assert!(p.heartbeat_tick([&h]).is_empty());
    }

    #[test]
    fn pipeline_breaker_is_shared_across_sessions() {
        let clock = FakeClock::new(1_000.0);
        let p = pipeline(clock.clone());
        // Many sessions matching in the SAME window: the fleet-wide cap must trip even
        // though each session is individually well inside its own cooldown.
        let n = (WATCH_GLOBAL_MAX_PER_WINDOW as usize) + 4;
        let handles: Vec<Arc<SessionHandle>> = (0..n)
            .map(|i| {
                let mut s = session_with(&["T"]);
                s.id = format!("proc_b{}", i);
                handle_for(s)
            })
            .collect();
        for (i, h) in handles.iter().enumerate() {
            // All within the global window, spread by microseconds so the window holds.
            *clock.t.lock() = 1_000.0 + (i as f64) * 0.001;
            p.ingest_text(h, "T");
        }
        let evs = p.drain();
        assert!(
            evs.iter().any(|e| e["type"] == "watch_overflow_tripped"),
            "the fleet-wide breaker must trip when many sessions match at once; got {:?}",
            evs.iter().map(|e| e["type"].clone()).collect::<Vec<_>>()
        );
        // And the sessions past the cap must NOT have been delivered.
        let delivered = evs.iter().filter(|e| e["type"] == "watch_match").count();
        assert_eq!(delivered, WATCH_GLOBAL_MAX_PER_WINDOW as usize);
    }

    #[test]
    fn global_watch_event_shape() {
        let ev = global_watch_event("watch_overflow_tripped", "msg", json!({"suppressed": 2}));
        assert_eq!(ev["session_id"], "");
        assert_eq!(ev["platform"], "");
        assert_eq!(ev["type"], "watch_overflow_tripped");
        assert_eq!(ev["suppressed"], 2);
        assert_eq!(ev["thread_id"], "");
    }

    #[test]
    fn watch_disabled_event_message_shape() {
        let s = session_with(&["E"]);
        let ev = watch_disabled_event(&s, 7, "because. ");
        assert_eq!(ev["type"], "watch_disabled");
        assert_eq!(ev["suppressed"], 7);
        let msg = ev["message"].as_str().unwrap();
        assert!(msg.contains("proc_watch1"));
        assert!(msg.contains("because."));
        assert!(msg.contains("exactly one notification"));
    }
}
