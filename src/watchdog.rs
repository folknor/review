//! Rollout-completion watchdog for codex runs.
//!
//! # Why this exists
//!
//! `codex exec` can finish a turn completely - final answer written to the
//! rollout transcript, `task_complete` recorded - and then never exit. When that
//! happens `review` blocks forever inside `run_codex_json`, because every piece
//! of its reporting machinery (digest, transcript forensics, incident bundle,
//! sidecar row) runs *after* the child is reaped. The operator sees no output,
//! no error, no incident bundle, and - before the lock change in `main.rs` - a
//! globally wedged `review` because the flock was held across the whole await.
//!
//! Two real hangs were diagnosed on codex-cli 0.147.0 (the same sessions worked
//! on 0.146.0):
//!
//! 1. *Stranded completion.* Every turn reached `task_complete` with a non-null
//!    `last_agent_message`; the rollout then stopped advancing and the process
//!    hung for 10+ hours. The answer existed on disk the entire time.
//! 2. *Mid-turn wedge.* The turn stopped emitting to the rollout partway
//!    through, while 25 unified-exec background cells were running.
//!
//! The leading upstream explanation for (1) is the resume-only
//! `exclude_turns: true` that `codex exec`'s `thread_resume_params_from_config`
//! began sending in 0.147.0 (openai/codex#35621, `codex-rs/exec/src/lib.rs`).
//! After `TurnCompleted`, exec issues an unbounded `thread/read` with
//! `include_turns: true` (`maybe_backfill_turn_completed_items`) while its
//! single `tokio::select!` loop is not draining notifications; with turns
//! excluded at resume, that read has to reconstruct the entire thread history
//! from the rollout. Secondary candidate: `OtelProvider`'s `Drop` shutdown,
//! which openai/codex#37109 bounded for the TUI only - `codex exec` still drops
//! the provider synchronously with no timeout. Neither is proven, and root-
//! causing codex is explicitly out of scope (see CLAUDE.md); this module is the
//! "catch" that makes the failure survivable from our side.
//!
//! # What it does
//!
//! One poller over the rollout file, with two verdicts:
//!
//! - **`Stranded`** - a real final answer for *this* run exists on disk and the
//!   rollout has stopped growing for `quiet_grace`. codex has produced
//!   everything we came for and is stuck in teardown, so kill it; the
//!   `final_answer` is then reported through the normal transcript-recovery
//!   path and the operator gets the real report instead of an indefinite hang.
//! - **`Stalled`** - no answer was ever written and the rollout has been silent
//!   for `stall_grace`. The run is lost: kill it, write the incident bundle,
//!   and report failure.
//!
//! `Stranded` is not a timeout. It cannot fire on a run that has not already
//! produced its answer, and it cannot fire while the rollout is still growing,
//! however long the run takes. The trigger is "we demonstrably have the result
//! and the process has gone quiet".
//!
//! # `Stalled` *is* a timeout, and is named like one
//!
//! It rests on an empirical property of codex rather than a documented
//! contract: **codex cannot stay silent.** It wakes itself every 2-5 minutes
//! even while babysitting a long-running task - the field rollout that motivated
//! this module shows `wait` calls with `yield_time_ms: 30000`, and events
//! landing every 5-15 seconds throughout the working portion of the turn. The
//! wedge, by contrast, was silent for hours. Two to three orders of magnitude
//! separate them, which is what makes a 15-minute default safe.
//!
//! That asymmetry is codex-specific and does not generalise. Claude legitimately
//! goes silent while waiting on a backgrounded task, so none of this applies to
//! it - and nothing here runs for claude anyway, which has no rollout.
//!
//! Because the invariant is empirical, a future codex could change its cadence
//! and start tripping this on healthy runs. Hence: the honest name (a rename to
//! something softer would make that day harder to diagnose), the configurable
//! and disableable threshold (`[_defaults].stall_timeout_secs`, `0` to turn it
//! off), and `quiet_secs` + `last_event` recorded on every verdict so a misfire
//! shows exactly how close the call was and what codex was last doing.

use std::time::{Duration, Instant};

/// Polling and patience settings, injectable so tests can drive the whole
/// watchdog in milliseconds instead of minutes.
#[derive(Clone, Copy)]
pub struct Timings {
    /// How often to stat/read the rollout. Cheap (a stat, plus a read only when
    /// the file grew), and slow enough to be invisible next to a long turn.
    pub poll_interval: Duration,
    /// How long the rollout must stay byte-for-byte unchanged, *after* a final
    /// answer exists, before we call it stranded. The cost of firing early is
    /// killing a run that was about to continue; the cost of firing late is a
    /// few idle minutes.
    pub quiet_grace: Duration,
    /// How long the rollout must stay unchanged with *no* answer written before
    /// we call the run stalled. `None` disables the stall branch entirely.
    ///
    /// This one is a timeout, and is named as such deliberately - see the module
    /// docs for why the honest name matters.
    pub stall_grace: Option<Duration>,
}

impl Default for Timings {
    fn default() -> Self {
        Self {
            poll_interval: crate::timings::POLL_INTERVAL,
            quiet_grace: crate::timings::QUIET_GRACE,
            stall_grace: Some(crate::timings::STALL_GRACE),
        }
    }
}

/// Which of the two failure shapes the poller detected.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// The answer is on disk and codex will not exit. Recoverable: killing it
    /// costs nothing, because everything we came for has already been written.
    Stranded,
    /// No answer was ever written and the rollout has gone silent far longer
    /// than codex's own wake cadence. The run is lost; kill it and fail.
    Stalled,
}

/// Verdict handed back to the runner when the watchdog fires.
pub struct Verdict {
    pub kind: Kind,
    /// Operator-facing explanation, printed before we kill the child.
    pub reason: String,
    /// How long the rollout had been unchanged when we decided. Recorded on the
    /// digest and in the incident bundle: if the stall branch ever misfires on a
    /// healthy run, this is the number that shows how close the call was.
    pub quiet_secs: u64,
    /// `type/payload.type` of the last event in the rollout - what codex was
    /// doing when it went quiet.
    pub last_event: Option<String>,
}

/// Does the region of the rollout written by *this* run contain a real final
/// answer?
///
/// `bytes` must already be sliced to start at the run's baseline offset (see
/// `scan_from_baseline`), which is how "this run" is delimited. A byte offset is
/// used rather than a timestamp comparison because rollout events carry
/// millisecond stamps while our own clock string is second-resolution: a turn
/// that completed within the same second as this run's launch would slip past a
/// time-based filter, and its stale answer would then license killing a resume
/// that had produced nothing.
///
/// Deliberately *not* `transcript::parse`: that function answers "what is the
/// state of the current turn", resetting at every `task_started`. Here the
/// question is the different one of "has this run produced an answer at any
/// point since it began", which must survive the trailing micro-turns codex
/// sometimes appends after the substantive turn (observed in the field: a
/// 64-item resume turn followed by 2-item and 1-item turns, each with its own
/// `task_complete`).
///
/// Requires the answer to be phase-tagged `final_answer`; codex tags interim
/// progress notes `commentary`, and killing a live run on the strength of an
/// "I am now doing X" note would be exactly wrong.
fn has_final_answer(bytes: &[u8]) -> bool {
    // Lossy is safe here: a baseline that lands mid-line leaves one unparseable
    // fragment, which is skipped like any other malformed line.
    let content = String::from_utf8_lossy(bytes);
    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(event) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let payload = event.get("payload");
        if payload.and_then(|p| p.get("type")).and_then(|t| t.as_str()) != Some("agent_message") {
            continue;
        }
        if payload
            .and_then(|p| p.get("phase"))
            .and_then(|p| p.as_str())
            == Some("final_answer")
        {
            return true;
        }
    }
    false
}

/// Slice a rollout's raw bytes to the region this run appended. Shares
/// `transcript`'s helper so the watchdog and transcript recovery can never
/// disagree about where a run's events begin.
fn scan_from_baseline(bytes: &[u8], baseline: u64) -> &[u8] {
    crate::transcript::slice_from_offset(bytes, Some(baseline))
}

/// Watch `session_id`'s rollout until it looks stranded, then resolve.
///
/// Resolves *only* on the stranded verdict - on any other condition (no session
/// id yet, no rollout file, file still advancing) it simply keeps polling
/// forever, so the caller can hold it in a `select!` against `child.wait()`
/// without it ever pre-empting a healthy run.
///
/// `session_rx` is a watch channel because a fresh run does not know its session
/// id until `thread.started` arrives on the NDJSON stream; on a resume it is
/// populated from the start. `baseline` is the rollout's size at spawn, which
/// scopes the answer scan to this run's own events.
pub async fn wait_for_stranded_completion(
    mut session_rx: tokio::sync::watch::Receiver<Option<String>>,
    codex_home: Option<String>,
    baseline: Option<u64>,
    timings: Timings,
) -> Verdict {
    // No trustworthy baseline means we cannot tell this run's events from an
    // earlier turn's, so we cannot safely judge anything: never fire. Scanning
    // the whole history instead would let a previous turn's `final_answer`
    // authorise killing a run that has produced nothing.
    let baseline: u64 = match baseline {
        Some(b) => b,
        None => std::future::pending().await,
    };
    // Fingerprint of the rollout at the last observed change, plus when that
    // change was seen. Length alone is enough: the rollout is append-only.
    let mut last_len: u64 = 0;
    let mut last_change = Instant::now();

    loop {
        tokio::time::sleep(timings.poll_interval).await;

        // Still waiting to learn which session this is (fresh run, pre-
        // `thread.started`). Nothing to watch yet.
        let Some(session_id) = session_rx.borrow_and_update().clone() else {
            continue;
        };

        let Some(path) =
            crate::transcript::find_transcript_path(&session_id, codex_home.as_deref())
        else {
            // No rollout yet, or a custom CODEX_HOME we cannot see into. Not an
            // error - just nothing to judge.
            continue;
        };

        let Ok(meta) = tokio::fs::metadata(&path).await else {
            continue;
        };
        let len = meta.len();
        if len != last_len {
            last_len = len;
            last_change = Instant::now();
            continue;
        }

        // The file has not grown since the previous poll. How long it has been
        // quiet decides which (if either) branch applies.
        let quiet = last_change.elapsed();
        let stall_due = timings.stall_grace.is_some_and(|grace| quiet >= grace);
        if quiet < timings.quiet_grace && !stall_due {
            continue;
        }

        // Only now is it worth reading and parsing - a working run never gets
        // this far, because its rollout keeps growing.
        let Ok(bytes) = tokio::fs::read(&path).await else {
            continue;
        };
        // Both helpers see only what *this* run appended. Scoping the answer
        // check but not the last-event probe would make the forensic field lie
        // precisely when it matters: a resume that stalls before writing any
        // parseable event would report the *previous* turn's last event as its
        // own activity. `None` here honestly means "this run wrote nothing".
        let ours = scan_from_baseline(&bytes, baseline);
        let answered = has_final_answer(ours);
        let quiet_secs = quiet.as_secs();
        let last_event = last_event(ours);

        if answered && quiet >= timings.quiet_grace {
            return Verdict {
                kind: Kind::Stranded,
                reason: format!(
                    "codex has written a final answer to its rollout but has not exited, \
                     and the rollout has not advanced in {quiet_secs}s.\n  \
                     Treating the run as complete and terminating codex.\n  \
                     transcript: {}",
                    path.display()
                ),
                quiet_secs,
                last_event,
            };
        }

        if !answered && stall_due {
            return Verdict {
                kind: Kind::Stalled,
                reason: format!(
                    "stall timeout: codex has written nothing to its rollout in \
                     {quiet_secs}s and has produced no final answer.\n  \
                     codex wakes itself every few minutes even while waiting on a \
                     long tool call, so silence this long means the run is wedged, \
                     not working.\n  \
                     Terminating it and reporting failure.\n  \
                     last rollout event: {}\n  \
                     transcript: {}",
                    last_event.as_deref().unwrap_or("(none)"),
                    path.display()
                ),
                quiet_secs,
                last_event,
            };
        }

        // Quiet, but not yet conclusive: either an answered run inside its
        // grace, or an unanswered one that has not reached the stall timeout
        // (or has it disabled). Keep waiting - killing an unanswered run early
        // discards real in-progress work.
        continue;
    }
}

/// `type/payload.type` of the last parseable event. Recorded with a verdict so a
/// kill says *what codex was doing* when it went silent, rather than only that
/// it did.
///
/// `bytes` must already be sliced to this run's region (`scan_from_baseline`),
/// exactly like `has_final_answer`. Scanning the whole rollout would make a
/// resume that stalled before writing anything report the *previous* turn's
/// last event as its own - the one case where this field is most needed and
/// would be most misleading. `None` honestly means "this run wrote nothing".
fn last_event(bytes: &[u8]) -> Option<String> {
    let content = String::from_utf8_lossy(bytes);
    content.lines().rev().find_map(|line| {
        let event: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
        let top = event.get("type").and_then(|t| t.as_str()).unwrap_or("?");
        let ptype = event
            .get("payload")
            .and_then(|p| p.get("type"))
            .and_then(|t| t.as_str())
            .unwrap_or("?");
        Some(format!("{top}/{ptype}"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const TASK_STARTED: &str = r#"{"timestamp":"2026-08-11T07:25:08.000Z","type":"event_msg","payload":{"type":"task_started"}}"#;
    const FINAL_ANSWER: &str = r#"{"timestamp":"2026-08-11T07:42:20.000Z","type":"event_msg","payload":{"type":"agent_message","message":"Done.","phase":"final_answer"}}"#;
    const COMMENTARY: &str = r#"{"timestamp":"2026-08-11T07:30:00.000Z","type":"event_msg","payload":{"type":"agent_message","message":"I am now regenerating the pilot.","phase":"commentary"}}"#;

    fn scan(rollout: &str, baseline: u64) -> bool {
        has_final_answer(scan_from_baseline(rollout.as_bytes(), baseline))
    }

    #[test]
    fn detects_a_final_answer_from_this_run() {
        let rollout = format!("{TASK_STARTED}\n{FINAL_ANSWER}\n");
        assert!(scan(&rollout, 0));
    }

    #[test]
    fn commentary_alone_is_not_an_answer() {
        // The mid-turn wedge case: interim notes only. Killing here would throw
        // away a run that never reported.
        let rollout = format!("{TASK_STARTED}\n{COMMENTARY}\n");
        assert!(!scan(&rollout, 0));
    }

    #[test]
    fn a_previous_turns_answer_does_not_count() {
        // A resume appends to the same rollout. The prior turn's answer must not
        // make us kill a resume that has produced nothing yet.
        let prior = format!("{TASK_STARTED}\n{FINAL_ANSWER}\n");
        let baseline = u64::try_from(prior.len()).expect("test fixture fits in u64");
        let rollout = format!("{prior}{TASK_STARTED}\n{COMMENTARY}\n");
        assert!(!scan(&rollout, baseline));
        // Sanity: without the baseline the stale answer would have counted,
        // which is exactly the misfire the offset prevents.
        assert!(scan(&rollout, 0));
    }

    // The offset is immune to clock resolution: an earlier turn that completed
    // in the *same second* as this run's launch is still excluded, where a
    // second-truncated timestamp comparison would have let it through and
    // licensed killing a resume that had produced nothing.
    #[test]
    fn a_prior_answer_in_the_same_second_does_not_count() {
        let same_second_answer = r#"{"timestamp":"2026-08-11T07:25:08.100Z","type":"event_msg","payload":{"type":"agent_message","message":"Old answer.","phase":"final_answer"}}"#;
        let prior = format!("{same_second_answer}\n");
        let baseline = u64::try_from(prior.len()).expect("test fixture fits in u64");
        let launched_same_second = r#"{"timestamp":"2026-08-11T07:25:08.900Z","type":"event_msg","payload":{"type":"task_started"}}"#;
        let rollout = format!("{prior}{launched_same_second}\n");
        assert!(!scan(&rollout, baseline));
    }

    // The last-event probe must be scoped to this run's bytes, like the answer
    // check. A resume that stalls before writing anything parseable must report
    // no event of its own rather than inheriting the previous turn's - that
    // field's whole purpose is saying what *this* run was doing when it froze.
    #[test]
    fn last_event_is_scoped_to_this_run() {
        let prior = format!("{TASK_STARTED}\n{FINAL_ANSWER}\n");
        let baseline = u64::try_from(prior.len()).expect("test fixture fits in u64");

        // A resume that has written nothing of its own yet.
        let bytes = prior.clone().into_bytes();
        assert_eq!(
            last_event(scan_from_baseline(&bytes, baseline)),
            None,
            "a run that wrote nothing must not inherit the prior turn's event"
        );
        // Unscoped, it would have reported the previous turn's answer event -
        // the misreport this scoping prevents.
        assert_eq!(
            last_event(&bytes).as_deref(),
            Some("event_msg/agent_message")
        );

        // Once the run does write, its own last event is reported.
        let tool_call =
            r#"{"type":"response_item","payload":{"type":"custom_tool_call","name":"exec"}}"#;
        let resumed = format!("{prior}{TASK_STARTED}\n{tool_call}\n");
        let bytes = resumed.into_bytes();
        assert_eq!(
            last_event(scan_from_baseline(&bytes, baseline)).as_deref(),
            Some("response_item/custom_tool_call")
        );
    }

    #[test]
    fn malformed_lines_are_skipped() {
        let rollout = format!("not json at all\n{FINAL_ANSWER}\n");
        assert!(scan(&rollout, 0));
    }

    // A baseline landing mid-line leaves an unparseable fragment, which must be
    // skipped without hiding the answer that follows it.
    #[test]
    fn a_baseline_inside_a_line_skips_only_that_fragment() {
        let rollout = format!("{TASK_STARTED}\n{FINAL_ANSWER}\n");
        let baseline = u64::try_from(TASK_STARTED.len() / 2).expect("fits");
        assert!(scan(&rollout, baseline));
    }

    // A rollout shorter than the baseline was rotated or replaced; scanning it
    // whole could read another run's turn, so it must yield nothing.
    #[test]
    fn a_truncated_rollout_yields_nothing() {
        let rollout = format!("{FINAL_ANSWER}\n");
        let baseline = u64::try_from(rollout.len() + 1000).expect("fits");
        assert!(!scan(&rollout, baseline));
    }
}
