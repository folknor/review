//! Codex on-disk transcript forensics.
//!
//! Codex writes a rollout transcript per session under `$CODEX_HOME/sessions`
//! (default `~/.codex/sessions`), named `rollout-<ts>-<session_id>.jsonl`. It
//! carries the same turn events as the `--json` stream plus tool calls, and it
//! survives a frozen/halted stream. When a fresh codex run ends without a clean
//! capture, we read this to classify *why* it stopped: a `stream_error`, or a
//! terminal `function_call` with no matching output (the tool that was running
//! when it died).

use std::path::{Path, PathBuf};

pub struct TranscriptSummary {
    pub path: String,
    /// A `task_complete` event was seen (codex reported the turn done).
    pub task_complete: bool,
    /// A `stream_error` event was seen (internal-tool error froze the stream).
    pub stream_error: bool,
    /// `type/payload.type` of the final transcript event.
    pub last_event: Option<String>,
    /// The last tool call with no matching output: `(name, arguments)`.
    pub last_in_flight_tool: Option<(String, String)>,
}

fn codex_home() -> PathBuf {
    if let Ok(dir) = std::env::var("CODEX_HOME") {
        return PathBuf::from(dir);
    }
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join(".codex")
}

/// Locate the transcript file for a session id by matching the `-<id>.jsonl`
/// filename suffix (the id is a UUID, so this is unambiguous). Bounded
/// recursive walk of the date-nested `sessions/` tree.
fn find_transcript(session_id: &str) -> Option<PathBuf> {
    let sessions = codex_home().join("sessions");
    let needle = format!("-{session_id}.jsonl");
    let mut stack = vec![(sessions, 0u32)];
    while let Some((dir, depth)) = stack.pop() {
        let entries = std::fs::read_dir(&dir).ok()?;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                // sessions/YYYY/MM/DD/file.jsonl -> cap the descent.
                if depth < 5 {
                    stack.push((path, depth + 1));
                }
            } else if path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with(&needle))
            {
                return Some(path);
            }
        }
    }
    None
}

fn summarize(path: &Path) -> Option<TranscriptSummary> {
    let content = std::fs::read_to_string(path).ok()?;
    let mut summary = parse(&content);
    summary.path = path.to_string_lossy().into_owned();
    Some(summary)
}

/// Parse transcript NDJSON content into a summary. Split from file IO so the
/// event handling can be unit-tested directly.
fn parse(content: &str) -> TranscriptSummary {
    let mut task_complete = false;
    let mut stream_error = false;
    let mut last_event: Option<String> = None;
    // call_id -> (name, arguments), preserving insertion order via a Vec.
    let mut in_flight: Vec<(String, (String, String))> = Vec::new();

    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let event: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let top = event.get("type").and_then(|t| t.as_str());
        let payload = event.get("payload");
        let ptype = payload.and_then(|p| p.get("type")).and_then(|t| t.as_str());

        last_event = Some(format!("{}/{}", top.unwrap_or("?"), ptype.unwrap_or("?")));

        match ptype {
            Some("task_complete") => task_complete = true,
            Some("stream_error") => stream_error = true,
            Some(pt) if pt.ends_with("_call") => {
                if let Some(p) = payload {
                    let call_id = p
                        .get("call_id")
                        .and_then(|c| c.as_str())
                        .unwrap_or("")
                        .to_string();
                    let name = p
                        .get("name")
                        .and_then(|n| n.as_str())
                        .unwrap_or(pt)
                        .to_string();
                    let args = p
                        .get("arguments")
                        .or_else(|| p.get("input"))
                        .map(ToString::to_string)
                        .unwrap_or_default();
                    in_flight.push((call_id, (name, args)));
                }
            }
            Some(pt) if pt.ends_with("_call_output") => {
                if let Some(call_id) = payload
                    .and_then(|p| p.get("call_id"))
                    .and_then(|c| c.as_str())
                {
                    in_flight.retain(|(id, _)| id != call_id);
                }
            }
            _ => {}
        }
    }

    let last_in_flight_tool = in_flight.pop().map(|(_, nt)| nt);

    TranscriptSummary {
        path: String::new(),
        task_complete,
        stream_error,
        last_event,
        last_in_flight_tool,
    }
}

/// Find and summarize the transcript for a codex session id, if one exists.
pub fn summarize_session(session_id: &str) -> Option<TranscriptSummary> {
    let path = find_transcript(session_id)?;
    summarize(&path)
}

#[cfg(test)]
mod tests {
    use super::*;

    // A run that ran one tool to completion, started a second, then hit a
    // stream_error and never reached task_complete - the frozen-stream case.
    const FROZEN: &str = r#"
{"type":"session_meta","payload":{"originator":"codex_exec","session_id":"abc"}}
{"type":"event_msg","payload":{"type":"task_started"}}
{"type":"response_item","payload":{"type":"function_call","name":"exec_command","arguments":"{\"cmd\":\"ls\"}","call_id":"call_1"}}
{"type":"response_item","payload":{"type":"function_call_output","call_id":"call_1","output":"ok"}}
{"type":"response_item","payload":{"type":"function_call","name":"exec_command","arguments":"{\"cmd\":\"sleep 999\"}","call_id":"call_2"}}
{"type":"event_msg","payload":{"type":"stream_error"}}
"#;

    #[test]
    fn frozen_run_surfaces_last_in_flight_tool() {
        let s = parse(FROZEN);
        assert!(!s.task_complete);
        assert!(s.stream_error);
        assert_eq!(s.last_event.as_deref(), Some("event_msg/stream_error"));
        let (name, args) = s.last_in_flight_tool.expect("in-flight tool");
        assert_eq!(name, "exec_command");
        assert!(args.contains("sleep 999"), "args were: {args}");
    }

    #[test]
    fn clean_run_has_no_in_flight_tool() {
        let clean = concat!(
            r#"{"type":"event_msg","payload":{"type":"task_started"}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"type":"function_call","name":"exec_command","arguments":"{}","call_id":"c1"}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"type":"function_call_output","call_id":"c1","output":"ok"}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"task_complete"}}"#,
        );
        let s = parse(clean);
        assert!(s.task_complete);
        assert!(!s.stream_error);
        assert!(s.last_in_flight_tool.is_none());
    }
}
