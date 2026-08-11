//! Every production timing default, in one place.
//!
//! These were previously spread across four files in three different forms:
//! two named constants (one of them function-scoped), several inline
//! `Duration::from_secs` literals inside `Default` impls, one bare
//! `from_millis(250)` mid-function, and a clap `default_value = "30"` string.
//! Nothing related them to each other, so the only way to see the shape of the
//! system's patience was to grep for `Duration`.
//!
//! Collecting them does not change how they are *used*: `watchdog::Timings` and
//! `provider::CodexRuntime` are still the injection points, and tests still
//! override them freely (see `provider_tests`, which drives 3-minute and
//! 10-second behaviours in milliseconds). What lives here is only the default
//! each of those structs starts from.
//!
//! # How they relate
//!
//! `POLL_INTERVAL` must stay well under `QUIET_GRACE`, or the effective grace is
//! quantised to the poll and the watchdog reacts a whole interval late.
//!
//! `QUIET_GRACE` and `STALL_GRACE` are *not* ordering-constrained, despite the
//! production values suggesting otherwise. The two verdicts are mutually
//! exclusive on whether this run wrote an answer (`answered && quiet >= QUIET`
//! vs `!answered && quiet >= STALL`), so a `STALL_GRACE` shorter than
//! `QUIET_GRACE` cannot steal an answered run: it matches neither branch until
//! `QUIET_GRACE`, then strands correctly. That is worth stating because the
//! opposite is easy to assume, and "clamp or reject the config" is a fix for a
//! bug that does not exist. Pinned by
//! `an_answered_run_is_stranded_even_with_a_shorter_stall_grace`.
//!
//! `SIGTERM_WINDOW` is the odd one out: it is not patience with codex but with
//! ourselves, on the way out of a signal handler that is about to `exit`.

use std::time::Duration;

/// How often the watchdog stats the rollout. Cheap (a stat, plus a read only
/// when the file grew), and far below every grace it feeds.
pub const POLL_INTERVAL: Duration = Duration::from_secs(15);

/// Rollout silence, *with* a final answer already written, before a run is
/// called stranded and killed.
///
/// Not a timeout: it cannot fire on a run that has not produced its answer, nor
/// while the rollout is still growing. Firing early costs a run that was about
/// to continue; firing late costs a few idle minutes, so it is set generously.
pub const QUIET_GRACE: Duration = Duration::from_secs(180);

/// Rollout silence, with *no* answer written, before a run is called stalled:
/// killed, bundled, and reported as a failure.
///
/// This one is a genuine timeout. It rests on codex's empirical wake cadence
/// (2-5 minutes even while babysitting a long task) rather than any documented
/// contract, so 15 minutes is 3-7x the worst legitimate silence. Overridable
/// per project via `[_defaults].stall_timeout_secs`, and disableable with `0`,
/// because a future codex could change that cadence. See `watchdog`'s module
/// docs for the full argument.
pub const STALL_GRACE: Duration = Duration::from_secs(900);

/// How long a pipe reader may keep going after the child has been reaped before
/// it is aborted and we take whatever it buffered.
///
/// Reaping is the authoritative end of a run; pipe EOF is not, because any
/// process holding the write end can delay it indefinitely. This bounds that
/// exposure without discarding what was already captured.
pub const DRAIN_GRACE: Duration = Duration::from_secs(5);

/// How long a process group gets to honour `SIGTERM` before `SIGKILL`.
///
/// A codex wedged in an unbounded shutdown await may never process the first
/// signal, and a deliberately stubborn descendant may ignore it outright.
pub const SIGKILL_ESCALATION: Duration = Duration::from_secs(10);

/// The window the signal supervisor allows between `SIGTERM`ing codex's groups
/// and `SIGKILL`ing them on its way to `process::exit`.
///
/// Short by necessity rather than choice: `exit` cancels the deferred
/// `SIGKILL_ESCALATION`, so this is the *only* chance a cooperative codex gets
/// to shut down cleanly once the operator has asked us to stop.
pub const SIGTERM_WINDOW: Duration = Duration::from_millis(250);

/// How long since a session was last touched before `--session` refuses to
/// resume it.
///
/// Past this the provider's prompt cache is cold (5 min default, ~1h with the
/// right env vars, so ~55 min is the realistic cap), and resuming means
/// reprocessing the whole session prefix at full cost. `--session` is the
/// *warm* follow-up path; a cold resume should be a fresh run instead.
pub const STALE_SESSION: Duration = Duration::from_secs(55 * 60);

/// Default seconds between provider launches, to avoid rate limits. Overridable
/// with `--stagger`; `0` disables.
///
/// Clap's `default_value` needs a `&'static str`, so this is declared as a
/// string rather than a `Duration`. It lives here anyway: the point of this
/// module is that no production timing default is declared anywhere else, and a
/// literal buried in a clap attribute is exactly the kind that drifts unnoticed.
pub const STAGGER_SECS_STR: &str = "30";

#[cfg(test)]
mod tests {
    use super::*;

    /// The one ordering that *is* required: a poll interval at or above the
    /// quiet grace would make the watchdog react a full interval late, turning
    /// the grace into a lower bound rather than the threshold it reads as.
    #[test]
    fn poll_interval_is_well_under_the_graces() {
        assert!(
            POLL_INTERVAL * 4 <= QUIET_GRACE,
            "POLL_INTERVAL ({POLL_INTERVAL:?}) must be comfortably under \
             QUIET_GRACE ({QUIET_GRACE:?}), or the grace is quantised to it"
        );
        assert!(POLL_INTERVAL * 4 <= STALL_GRACE);
    }

    /// The supervisor's window must be shorter than the escalation it stands in
    /// for; if it were longer it would simply be the escalation.
    #[test]
    fn the_sigterm_window_is_shorter_than_the_escalation() {
        assert!(SIGTERM_WINDOW < SIGKILL_ESCALATION);
    }
}
