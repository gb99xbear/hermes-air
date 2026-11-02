//! Formatting of queued background-process events into an agent-readable notice.
//!
//! The watch pipeline (layer 2) queues events; nobody drains them, so a process
//! that exits, a watch pattern that matches, or a heartbeat that fires is
//! invisible to the conversation. This module turns one event into the
//! `[IMPORTANT: ...]` notice the loop injects, matching the Python gateway's
//! `format_process_notification`.

use serde_json::Value;

/// Seconds as a short human age: `45s`, `3m12s`, `2h5m`.
fn format_age(seconds: f64) -> String {
    let total = seconds.max(0.0).floor() as u64;
    if total < 60 {
        format!("{}s", total)
    } else if total < 3600 {
        format!("{}m{}s", total / 60, total % 60)
    } else {
        format!("{}h{}m", total / 3600, (total % 3600) / 60)
    }
}

/// One-line attribution when a process was handed to this session by a subagent.
fn attribution_line(evt: &Value) -> Option<String> {
    if let Some(note) = evt.get("handoff_note").and_then(|v| v.as_str()) {
        if !note.is_empty() {
            return Some(format!(
                "Handed off to you by a subagent before it finished. Purpose: {}",
                note
            ));
        }
    }
    None
}

/// A synthetic notice for one queued event, or `None` when the event is not one
/// the conversation should see (foreign types the watcher task owns).
pub fn format_process_event(evt: &Value) -> Option<String> {
    let type_ = evt
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("completion");
    let sid = evt
        .get("session_id")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    let cmd = evt
        .get("command")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");

    // Breaker and disabled notices carry their own human-readable summary.
    if matches!(
        type_,
        "watch_disabled" | "watch_overflow_tripped" | "watch_overflow_released"
    ) {
        return Some(format!(
            "[IMPORTANT: {}]",
            evt.get("message").and_then(|v| v.as_str()).unwrap_or("")
        ));
    }

    let attribution = attribution_line(evt)
        .map(|a| format!("{}\n", a))
        .unwrap_or_default();

    match type_ {
        "heartbeat" => {
            let out = evt
                .get("output")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .unwrap_or("(no new output since the last heartbeat)");
            let seq = evt.get("seq").and_then(|v| v.as_u64()).unwrap_or(0);
            let elapsed = evt.get("elapsed").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let interval = evt.get("interval").and_then(|v| v.as_u64()).unwrap_or(0);
            Some(format!(
                "[Background process {} heartbeat #{} - still running after {} \
                 (next in {}s; you will also be told when it exits).\n{}Command: {}\n\
                 Output since last heartbeat:\n{}]",
                sid,
                seq,
                format_age(elapsed),
                interval,
                attribution,
                cmd,
                out
            ))
        }
        "watch_match" => {
            let pattern = evt.get("pattern").and_then(|v| v.as_str()).unwrap_or("?");
            let out = evt.get("output").and_then(|v| v.as_str()).unwrap_or("");
            let suppressed = evt.get("suppressed").and_then(|v| v.as_u64()).unwrap_or(0);
            let mut text = format!(
                "[IMPORTANT: Background process {} matched watch pattern \"{}\".\n{}Command: {}\n\
                 Matched output:\n{}",
                sid, pattern, attribution, cmd, out
            );
            if suppressed > 0 {
                text.push_str(&format!(
                    "\n({} earlier matches were suppressed by rate limit)",
                    suppressed
                ));
            }
            text.push(']');
            Some(text)
        }
        // A completed background process: the case the agent most needs to hear
        // about, because the work it started has finished somewhere it is not
        // looking.
        "completion" | "exited" => {
            let exit = evt
                .get("exit_code")
                .map(|v| v.to_string())
                .unwrap_or_else(|| "\"unknown\"".to_string());
            let out = evt.get("output").and_then(|v| v.as_str()).unwrap_or("");
            Some(format!(
                "[IMPORTANT: Background process {} completed (exit code {}).\n{}Command: {}\n\
                 Output:\n{}]",
                sid, exit, attribution, cmd, out
            ))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_completed_process_is_reported_with_its_exit_code() {
        let evt = json!({
            "type": "completion",
            "session_id": "proc_1",
            "command": "cargo build",
            "exit_code": 0,
            "output": "done"
        });
        let text = format_process_event(&evt).expect("notice");
        assert!(text.contains("[IMPORTANT:"));
        assert!(text.contains("proc_1"));
        assert!(text.contains("exit code 0"));
        assert!(text.contains("cargo build"));
        assert!(text.ends_with(']'));
    }

    #[test]
    fn a_handoff_is_attributed_to_the_subagent() {
        let evt = json!({
            "type": "completion",
            "session_id": "proc_2",
            "command": "long job",
            "exit_code": 0,
            "handoff_note": "backfill the archive"
        });
        let text = format_process_event(&evt).unwrap();
        assert!(text.contains("Handed off to you by a subagent"));
        assert!(text.contains("backfill the archive"));
    }

    #[test]
    fn a_heartbeat_says_it_is_still_running() {
        let evt = json!({
            "type": "heartbeat",
            "session_id": "proc_3",
            "command": "train",
            "seq": 4,
            "elapsed": 185.0,
            "interval": 60,
            "output": "epoch 4"
        });
        let text = format_process_event(&evt).unwrap();
        assert!(text.contains("heartbeat #4"));
        assert!(text.contains("3m5s"));
        assert!(text.contains("epoch 4"));
    }

    #[test]
    fn a_rate_limited_match_names_the_suppressed_count() {
        let evt = json!({
            "type": "watch_match",
            "session_id": "proc_4",
            "command": "serve",
            "pattern": "READY",
            "output": "READY",
            "suppressed": 7
        });
        let text = format_process_event(&evt).unwrap();
        assert!(text.contains("matched watch pattern \"READY\""));
        assert!(text.contains("7 earlier matches were suppressed"));
    }

    #[test]
    fn breaker_notices_pass_their_own_message_through() {
        let evt = json!({
            "type": "watch_overflow_tripped",
            "message": "watch delivery paused for 60s"
        });
        assert_eq!(
            format_process_event(&evt).unwrap(),
            "[IMPORTANT: watch delivery paused for 60s]"
        );
    }

    #[test]
    fn a_foreign_event_is_not_claimed() {
        let evt = json!({"type": "async_delegation", "session_id": "x"});
        assert!(format_process_event(&evt).is_none());
    }
}
