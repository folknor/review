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
//! While codex runs, poll its rollout file. When *both* hold:
//!
//! - a real final answer for **this** run's turn exists on disk, and
//! - the rollout has stopped growing for `QUIET_GRACE`,
//!
//! conclude that codex has produced everything we came for and is now stuck in
//! teardown, and tell the caller to kill it. The recovered `final_answer` is
//! then reported through the normal transcript-recovery path, so the operator
//! gets the real report instead of an indefinite hang.
//!
//! # What it deliberately is *not*
//!
//! This is **not** a timeout on the provider. It never fires while codex is
//! working, however long that takes, and it never fires on a run that has not
//! already produced its answer. The trigger is "we demonstrably have the
//! result and the process has gone quiet", not elapsed wall-clock time. A
//! genuine mid-turn wedge (failure 2 above) is *not* covered - that needs a
//! stall detector, which is a real timeout and is a separate decision.

use std::time::{Duration, Instant};

/// How often to stat/read the rollout. Cheap (a stat, plus a read only when the
/// file grew), and slow enough to be invisible next to a multi-minute turn.
const POLL_INTERVAL: Duration = Duration::from_secs(15);

/// How long the rollout must stay byte-for-byte unchanged, *after* a final
/// answer exists, before we call it stranded. Generous on purpose: codex writes
/// token_count events throughout a turn, so a genuinely-working run touches the
/// file far more often than this. The cost of firing early is killing a run
/// that was about to continue; the cost of firing late is a few idle minutes.
const QUIET_GRACE: Duration = Duration::from_secs(180);

/// Verdict handed back to the runner when the watchdog fires.
pub struct Stranded {
    /// Operator-facing explanation, printed before we kill the child.
    pub reason: String,
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
) -> Stranded {
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
        tokio::time::sleep(POLL_INTERVAL).await;

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

        // The file has not grown since the previous poll. Only now is it worth
        // reading and parsing - a working run never reaches this branch.
        if last_change.elapsed() < QUIET_GRACE {
            continue;
        }
        let Ok(bytes) = tokio::fs::read(&path).await else {
            continue;
        };
        if !has_final_answer(scan_from_baseline(&bytes, baseline)) {
            // Quiet, but with nothing to show for it. That is the mid-turn
            // wedge (or a legitimately long-thinking model), and we refuse to
            // guess: killing here could discard real in-progress work. Keep
            // waiting.
            //
            // TODO(stall-detector): this is the hook for the optional
            // "rollout has not advanced in N minutes -> incident bundle and
            // exit nonzero" behaviour. That one *is* a timeout, so it stays
            // opt-in and off by default until explicitly enabled.
            continue;
        }

        return Stranded {
            reason: format!(
                "codex has written a final answer to its rollout but has not exited, \
                 and the rollout has not advanced in {}s.\n  \
                 Treating the run as complete and terminating codex.\n  \
                 transcript: {}",
                QUIET_GRACE.as_secs(),
                path.display()
            ),
        };
    }
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
