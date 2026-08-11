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
    /// The last `final_answer`-phase agent message this turn, if any. codex
    /// tags interim progress notes `phase="commentary"` and the real report
    /// `phase="final_answer"`. When a shutdown-exit truncates the `--json`
    /// stream and `-o` file (captured=false) but the rollout still reached the
    /// answer, this is the authoritative response to recover. `None` means the
    /// turn ended without emitting a final answer (e.g. it stopped on a tool).
    pub final_answer: Option<String>,
}

/// Codex home directory. `override_home` is the effective `CODEX_HOME` for the
/// run (e.g. a profile `env` override), which wins over the parent process's
/// environment - otherwise a run pointed at a custom codex home would have its
/// transcript written somewhere we never look.
fn codex_home(override_home: Option<&str>) -> PathBuf {
    if let Some(dir) = override_home {
        return PathBuf::from(dir);
    }
    if let Ok(dir) = std::env::var("CODEX_HOME") {
        return PathBuf::from(dir);
    }
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join(".codex")
}

/// Locate the transcript file for a session id by matching the `-<id>.jsonl`
/// filename suffix (the id is a UUID, so this is unambiguous). Bounded
/// recursive walk of the date-nested `sessions/` tree.
pub fn find_transcript_path(session_id: &str, override_home: Option<&str>) -> Option<PathBuf> {
    let sessions = codex_home(override_home).join("sessions");
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

fn summarize(path: &Path, from_offset: Option<u64>) -> Option<TranscriptSummary> {
    let bytes = std::fs::read(path).ok()?;
    // Lossy is safe: an offset landing mid-line leaves one unparseable fragment,
    // which is skipped like any other malformed line.
    let content = String::from_utf8_lossy(slice_from_offset(&bytes, from_offset));
    let mut summary = parse(&content);
    summary.path = path.to_string_lossy().into_owned();
    Some(summary)
}

/// Slice a rollout's raw bytes to the region a run appended, given the file's
/// size at that run's launch. `None` means "the whole file".
///
/// This is how a run's own events are delimited, in preference to comparing
/// timestamps. Rollout events carry millisecond stamps while `review`'s clock
/// string is second-resolution, so a previous turn that finished in the same
/// second as a resume's launch would slip past a time filter - and its stale
/// `final_answer` would then be recovered as the new run's result, suppressing
/// auto-resume and writing a wrong answer into the sidecar. A byte offset has no
/// such race.
///
/// A rollout *shorter* than the offset was rotated or replaced under us;
/// scanning it whole could attribute another run's turn to this one, so it
/// yields nothing.
pub fn slice_from_offset(bytes: &[u8], from_offset: Option<u64>) -> &[u8] {
    let Some(offset) = from_offset else {
        return bytes;
    };
    match usize::try_from(offset) {
        Ok(offset) if offset <= bytes.len() => &bytes[offset..],
        _ => &[],
    }
}

/// Parse transcript NDJSON content into a summary. Split from file IO so the
/// event handling can be unit-tested directly. Scoping to a single run's events
/// is the caller's job, via `slice_from_offset`.
fn parse(content: &str) -> TranscriptSummary {
    // Per-turn state. A resumed session appends to the same rollout, so this is
    // snapshotted and reset at each turn boundary (see the `task_started` arm).
    #[derive(Default, Clone)]
    struct TurnState {
        task_complete: bool,
        stream_error: bool,
        final_answer: Option<String>,
        /// call_id -> (name, arguments), preserving insertion order via a Vec.
        in_flight: Vec<(String, (String, String))>,
    }

    let mut cur = TurnState::default();
    let mut last_event: Option<String> = None;
    // The turn we were in before the most recent `task_started`. Kept so an
    // empty turn that immediately aborts can be rolled back (see below).
    let mut prev: Option<TurnState> = None;
    // Did the current turn emit anything at all? Distinguishes a real turn from
    // codex's shutdown-time phantom turn.
    let mut turn_produced_content = false;

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
            // A resumed session appends new turns to the same rollout file. Reset
            // per-turn state on each turn boundary so the summary reflects the
            // current (last) turn, not a completed earlier one. The outgoing turn
            // is stashed in `prev` in case this new turn turns out to be a
            // phantom (see the `turn_aborted` arm).
            Some("task_started") => {
                prev = Some(std::mem::take(&mut cur));
                turn_produced_content = false;
            }
            // A turn that aborts without ever carrying a user request or model
            // output is codex's shutdown-time phantom, not a real turn: as of
            // codex-cli 0.147.0, `codex exec` opens a turn during teardown and
            // aborts it within milliseconds. Observed shape, in order:
            // `task_started`, a `turn_context`, a synthetic `role: "developer"`
            // message whose text is `<turn_aborted>`, then `turn_aborted` with
            // `reason: "interrupted"` - 46 ms end to end.
            //
            // Note it is *not* an empty event sequence, so "zero items" cannot
            // be the test. What distinguishes it is the absence of both a
            // `user_message` (every real turn is opened by one) and any model
            // output. `turn_produced_content` is therefore keyed on a positive
            // list of real-content events rather than on "any event at all";
            // see the arms below for what counts.
            //
            // Attributing this phantom as the session's final state made clean,
            // fully-completed sessions report `task_complete=false` - which is
            // how the "resume never completes" investigation was sent chasing a
            // non-existent truncated-turn condition. Roll back to the real turn
            // instead. A turn that *did* carry content and then aborted is a
            // genuine interruption and is kept as-is.
            Some("turn_aborted") if !turn_produced_content => {
                if let Some(p) = prev.take() {
                    cur = p;
                }
            }
            // The operator's request. Its presence alone proves a real turn:
            // the phantom is synthesised at teardown and never carries one.
            Some("user_message") => turn_produced_content = true,
            // Model output short of an agent message. A turn cancelled after
            // only reasoning is a genuine early cancellation, not a phantom,
            // and must not be rolled back.
            Some("reasoning") => turn_produced_content = true,
            // A conversation message. Excluding `role: "developer"` is the
            // point: that is the synthetic `<turn_aborted>` note codex injects
            // into the phantom itself, so counting it would defeat the check.
            Some("message") => {
                let role = payload.and_then(|p| p.get("role")).and_then(|r| r.as_str());
                if role != Some("developer") {
                    turn_produced_content = true;
                }
            }
            Some("task_complete") => {
                turn_produced_content = true;
                cur.task_complete = true;
            }
            Some("stream_error") => {
                turn_produced_content = true;
                cur.stream_error = true;
            }
            // codex phase-tags agent messages: interim notes are "commentary",
            // the real report is "final_answer". Keep only the latter.
            Some("agent_message") => {
                turn_produced_content = true;
                let is_final = payload
                    .and_then(|p| p.get("phase"))
                    .and_then(|p| p.as_str())
                    == Some("final_answer");
                if is_final {
                    cur.final_answer = payload
                        .and_then(|p| p.get("message"))
                        .and_then(|m| m.as_str())
                        .map(str::to_string);
                }
            }
            Some(pt) if pt.ends_with("_call") => {
                turn_produced_content = true;
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
                    cur.in_flight.push((call_id, (name, args)));
                }
            }
            Some(pt) if pt.ends_with("_call_output") => {
                turn_produced_content = true;
                if let Some(call_id) = payload
                    .and_then(|p| p.get("call_id"))
                    .and_then(|c| c.as_str())
                {
                    cur.in_flight.retain(|(id, _)| id != call_id);
                }
            }
            _ => {}
        }
    }

    let last_in_flight_tool = cur.in_flight.pop().map(|(_, nt)| nt);

    TranscriptSummary {
        path: String::new(),
        task_complete: cur.task_complete,
        stream_error: cur.stream_error,
        last_event,
        last_in_flight_tool,
        final_answer: cur.final_answer,
    }
}

/// Find and summarize the transcript for a codex session id, if one exists.
/// `override_home` is the run's effective `CODEX_HOME` (a profile `env`
/// override), if any.
/// `from_offset` is the rollout's size at the run's launch, restricting the
/// summary to that run's own events - used by recovery so a prior turn's
/// `final_answer` can never be reported as this run's result. Pass `None` to
/// summarize the whole rollout (e.g. `review sessions <id>`).
pub fn summarize_session(
    session_id: &str,
    override_home: Option<&str>,
    from_offset: Option<u64>,
) -> Option<TranscriptSummary> {
    let path = find_transcript_path(session_id, override_home)?;
    summarize(&path, from_offset)
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

    // A truncated run: the model emitted an interim commentary message, ran a
    // final tool, then a real final_answer, and reached task_complete - but the
    // exec stream/-o were cut off. The final_answer must be recoverable.
    #[test]
    fn recovers_final_answer_from_transcript() {
        let run = concat!(
            r#"{"type":"event_msg","payload":{"type":"task_started"}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"agent_message","message":"I am now running the checks.","phase":"commentary"}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"type":"function_call","name":"exec_command","arguments":"{}","call_id":"c1"}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"type":"function_call_output","call_id":"c1","output":"ok"}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"agent_message","message":"All checks passed. No files changed.","phase":"final_answer"}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"task_complete"}}"#,
        );
        let s = parse(run);
        assert!(s.task_complete);
        assert_eq!(
            s.final_answer.as_deref(),
            Some("All checks passed. No files changed.")
        );
    }

    // A run that ended on a tool with only commentary - no final answer exists.
    #[test]
    fn no_final_answer_when_only_commentary() {
        let run = concat!(
            r#"{"type":"event_msg","payload":{"type":"task_started"}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"agent_message","message":"I am moving through the checks now.","phase":"commentary"}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"type":"function_call","name":"exec_command","arguments":"{}","call_id":"c1"}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"type":"function_call_output","call_id":"c1","output":"ok"}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"task_complete"}}"#,
        );
        let s = parse(run);
        assert!(s.task_complete);
        assert!(s.final_answer.is_none());
    }

    // A resumed rollout: the first turn completed, then a second turn started
    // (resume), ran a tool, and hit a stream_error without completing. The
    // summary must reflect the current turn, not the completed earlier one.
    #[test]
    fn resumed_turn_does_not_inherit_prior_completion() {
        let resumed = concat!(
            r#"{"type":"event_msg","payload":{"type":"task_started"}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"task_complete"}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"task_started"}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"type":"function_call","name":"exec_command","arguments":"{\"cmd\":\"sleep 999\"}","call_id":"c9"}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"stream_error"}}"#,
        );
        let s = parse(resumed);
        assert!(!s.task_complete, "second turn must not report complete");
        assert!(s.stream_error);
        let (name, _) = s.last_in_flight_tool.expect("in-flight tool from turn 2");
        assert_eq!(name, "exec_command");
    }

    // codex-cli 0.147.0 opens a turn at `codex exec` teardown and aborts it
    // milliseconds later. Transcribed from the field (session 019fefb0), which
    // is why it includes the `turn_context` and the synthetic developer message
    // - the phantom is NOT an empty event sequence:
    //   07:23:23.220  event_msg     task_started   turn_id 67a4fabf-...
    //   07:23:23.228  turn_context  (no payload.type)
    //   07:23:23.265  response_item message        role "developer", <turn_aborted>
    //   07:23:23.266  event_msg     turn_aborted   reason "interrupted"
    // The summary must still describe the real (completed) turn underneath.
    #[test]
    fn phantom_shutdown_turn_does_not_mask_real_completion() {
        let rollout = concat!(
            r#"{"type":"event_msg","payload":{"type":"task_started"}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"user_message","message":"do the thing"}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"agent_message","message":"The real report.","phase":"final_answer"}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"task_complete"}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"task_started"}}"#,
            "\n",
            r#"{"type":"turn_context","payload":{"cwd":"/home/folk/Programs/mogwai"}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"type":"message","role":"developer","content":[{"type":"input_text","text":"<turn_aborted>"}]}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"turn_aborted","reason":"interrupted"}}"#,
        );
        let s = parse(rollout);
        assert!(s.task_complete, "phantom turn must not clear task_complete");
        assert_eq!(s.final_answer.as_deref(), Some("The real report."));
    }

    // A real turn cancelled before it emitted any agent message or tool call is
    // a genuine early cancellation. It must NOT be rolled back into the previous
    // turn's completion - doing so would report a cancelled turn as finished,
    // with a stale answer attached.
    #[test]
    fn early_cancelled_turn_with_a_user_message_is_not_a_phantom() {
        let rollout = concat!(
            r#"{"type":"event_msg","payload":{"type":"task_started"}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"agent_message","message":"Old answer.","phase":"final_answer"}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"task_complete"}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"task_started"}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"user_message","message":"now do the next thing"}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"turn_aborted","reason":"interrupted"}}"#,
        );
        let s = parse(rollout);
        assert!(
            !s.task_complete,
            "a cancelled real turn must not inherit the prior turn's completion"
        );
        assert!(
            s.final_answer.is_none(),
            "a cancelled real turn must not surface the prior turn's answer"
        );
    }

    // Same, for a turn cancelled after only reasoning events - no user_message
    // in this rollout at all, so the reasoning is what proves it was real.
    #[test]
    fn turn_cancelled_after_only_reasoning_is_not_a_phantom() {
        let rollout = concat!(
            r#"{"type":"event_msg","payload":{"type":"task_started"}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"task_complete"}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"task_started"}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"type":"reasoning","summary":[]}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"turn_aborted","reason":"interrupted"}}"#,
        );
        let s = parse(rollout);
        assert!(
            !s.task_complete,
            "reasoning proves the turn was real, not a teardown phantom"
        );
    }

    // The rollback must not swallow a *genuine* interruption: a turn that ran
    // tools and was then aborted really did fail, and must report as such.
    #[test]
    fn aborted_turn_with_content_is_not_rolled_back() {
        let rollout = concat!(
            r#"{"type":"event_msg","payload":{"type":"task_started"}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"task_complete"}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"task_started"}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"type":"function_call","name":"exec_command","arguments":"{}","call_id":"c1"}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"turn_aborted","reason":"interrupted"}}"#,
        );
        let s = parse(rollout);
        assert!(
            !s.task_complete,
            "a real aborted turn must not inherit the prior turn's completion"
        );
        let (name, _) = s.last_in_flight_tool.expect("in-flight tool from turn 2");
        assert_eq!(name, "exec_command");
    }

    // A prior turn completed with a real final_answer; a later resume died
    // before emitting any new event. Scoped to the bytes this run appended,
    // recovery must NOT surface the stale final_answer as its result - doing so
    // would report someone else's answer, suppress auto-resume, and write the
    // wrong response into the sidecar.
    fn parse_from(rollout: &str, from_offset: Option<u64>) -> TranscriptSummary {
        let bytes = rollout.as_bytes();
        parse(&String::from_utf8_lossy(slice_from_offset(
            bytes,
            from_offset,
        )))
    }

    #[test]
    fn offset_excludes_a_prior_turns_final_answer() {
        let prior = concat!(
            r#"{"timestamp":"2026-07-17T10:00:00.000Z","type":"event_msg","payload":{"type":"task_started"}}"#,
            "\n",
            r#"{"timestamp":"2026-07-17T10:00:05.000Z","type":"event_msg","payload":{"type":"agent_message","message":"Old answer from turn one.","phase":"final_answer"}}"#,
            "\n",
            r#"{"timestamp":"2026-07-17T10:00:06.000Z","type":"event_msg","payload":{"type":"task_complete"}}"#,
            "\n",
        );
        let offset = u64::try_from(prior.len()).expect("test fixture fits in u64");
        // The resume died before writing anything of its own.
        let rollout = prior.to_string();

        // Whole-file view (e.g. `review sessions <id>`): the old answer shows.
        assert_eq!(
            parse_from(&rollout, None).final_answer.as_deref(),
            Some("Old answer from turn one.")
        );
        // Scoped to this run's bytes: nothing of ours, so nothing to recover.
        let s = parse_from(&rollout, Some(offset));
        assert!(
            s.final_answer.is_none(),
            "stale final_answer must be gated out"
        );
        assert!(!s.task_complete, "prior task_complete must be gated out");
    }

    // The offset is immune to clock resolution, which a timestamp filter was
    // not: a prior answer written in the *same second* as the resume's launch
    // is still excluded.
    #[test]
    fn offset_excludes_a_prior_answer_from_the_same_second() {
        let prior = concat!(
            r#"{"timestamp":"2026-07-17T10:00:05.100Z","type":"event_msg","payload":{"type":"agent_message","message":"Old answer.","phase":"final_answer"}}"#,
            "\n",
        );
        let offset = u64::try_from(prior.len()).expect("test fixture fits in u64");
        let rollout = format!(
            "{prior}{}\n",
            r#"{"timestamp":"2026-07-17T10:00:05.900Z","type":"event_msg","payload":{"type":"task_started"}}"#
        );
        let s = parse_from(&rollout, Some(offset));
        assert!(s.final_answer.is_none());
    }

    // A rollout shorter than the offset was rotated or replaced under us;
    // attributing its contents to this run would be a lie.
    #[test]
    fn offset_past_the_end_yields_nothing() {
        let rollout = concat!(
            r#"{"type":"event_msg","payload":{"type":"agent_message","message":"Answer.","phase":"final_answer"}}"#,
            "\n",
        );
        let offset = u64::try_from(rollout.len() + 1000).expect("fits");
        assert!(parse_from(rollout, Some(offset)).final_answer.is_none());
    }
}
