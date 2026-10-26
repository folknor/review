//! `review interrupt <ID>`: stop a codex or claude run mid-turn and hand back
//! its session.
//!
//! # Why a verb
//!
//! A `codex exec` turn cannot be spoken to while it runs - it reads stdin once,
//! listens on nothing, and its session is locked against every other writer
//! (see `reference/codex.md`). The only way to redirect a long run is to end
//! the turn and resume the session with a new message. Doing that by hand meant
//! finding codex's pid, and then losing the race with our own auto-resume,
//! which reads an interrupted turn as a mid-turn death and resumes straight past
//! it. This verb does the whole thing and ends by printing the resume command.
//! A `claude --print` turn is no more reachable, and ends the same way on
//! `SIGINT` (see `reference/claude.md`), so it takes the same path.
//!
//! # How
//!
//! 1. The run's in-flight marker names its keeper's pid (see `keeper`), the
//!    owning `review`'s child and leader of its own process group - offered
//!    only once the provider has produced output, so the provider is never too
//!    young to handle the signal (for codex, a node wrapper that would die on
//!    it).
//! 2. An interrupt request is written beside the marker *before* the signal, so
//!    the owning `review` can tell this apart from a death once codex exits.
//! 3. `SIGINT` goes to the keeper, which forwards one SIGINT to the provider
//!    alone, through a pidfd - on a codex npm install the node wrapper, which
//!    forwards it to the native binary once. Signalling the whole group would
//!    hand the native binary a second copy. codex answers `SIGINT` with a
//!    `turn/interrupt`, so the turn ends cleanly and the session lock is freed;
//!    claude ends the turn, kills its running command and prints its result.
//! 4. We wait for the owner to consume the request (the provider has been reaped),
//!    then for its sidecar row, because that row is what `review resume` needs.
//!    Printing the resume command any earlier would hand the operator a command
//!    that fails.

use anyhow::{Result, bail};
use std::path::Path;

use crate::sessions::SessionRecord;

pub async fn run(session_id: &str) -> Result<()> {
    let outcome = interrupt(session_id, None, crate::sessions::read_all).await?;
    println!("session: {session_id}");
    println!("project: {}", outcome.project);
    if outcome.interrupted {
        println!(
            "{}",
            crate::provider::resume_hint(&outcome.provider, session_id)
        );
    } else {
        // The run finished before the signal took effect; its answer is in the
        // owner's output.
        println!(
            "the run ended before the interrupt took effect - see its output, or \
             `review sessions {session_id}`"
        );
    }
    Ok(())
}

/// What became of an interrupt, read from the rows the owner recorded.
#[derive(Debug)]
pub struct Outcome {
    pub interrupted: bool,
    pub project: String,
    pub provider: String,
}

/// The verb, minus printing. `data_root` and `rows` (every sidecar row, oldest
/// first) are injectable so the whole sequence can run against a stub codex.
pub async fn interrupt(
    session_id: &str,
    data_root: Option<&Path>,
    rows: impl Fn() -> Vec<SessionRecord>,
) -> Result<Outcome> {
    let Some(marker) = crate::inflight::live_marker(session_id, data_root) else {
        // Nothing to signal yet, but not nothing happening: a message is
        // queued behind the global lock and will start a turn shortly.
        if crate::inflight::launch_pending(session_id, data_root) {
            bail!(
                "a message to session {session_id} is waiting to launch, so there is no \
                 turn to interrupt yet\n  retry once it has launched, or send your own \
                 `review message`, which waits for it and then interrupts it"
            );
        }
        bail!(
            "no run of session {session_id} is in flight on this host\n  \
             `review sessions --all` lists the ones that are"
        );
    };
    let provider = marker.provider.clone();
    let provider = provider.as_str();
    if marker.cleanup_pending {
        bail!("{}", cleanup_pending_message(session_id, &marker));
    }
    if !interruptible(provider) {
        bail!(
            "session {session_id} is a {provider} run; only codex and claude runs can be \
             interrupted"
        );
    }
    // The pid is offered once the provider has printed its first output, a
    // second or two after launch. A run caught inside that window - which
    // `review message` does routinely, its launch lock handing over the moment
    // the previous message's run spawns - is waited for rather than refused.
    let marker = if marker.child_pid.is_some() {
        marker
    } else {
        eprintln!("waiting for {provider} to start before interrupting it...");
        let deadline = std::time::Instant::now() + crate::timings::INTERRUPT_PID_WAIT;
        loop {
            tokio::time::sleep(crate::timings::INTERRUPT_PID_POLL).await;
            match crate::inflight::live_marker(session_id, data_root) {
                // It ended before it could be interrupted; there is nothing
                // left to stop.
                None => {
                    return Ok(Outcome {
                        interrupted: false,
                        project: marker.project.clone(),
                        provider: provider.to_string(),
                    });
                }
                Some(m) if m.child_pid.is_some() => break m,
                Some(_) if std::time::Instant::now() >= deadline => bail!(
                    "session {session_id} cannot be interrupted: {provider} has produced no \
                     output in {}s, or the run was launched by an older `review` (pid {}) \
                     that does not record {provider}'s pid",
                    crate::timings::INTERRUPT_PID_WAIT.as_secs(),
                    marker.pid
                ),
                Some(_) => {}
            }
        }
    };
    // Always offered by now; refused rather than defaulted, since pid 0 would
    // signal our own process group.
    let Some(child_pid) = marker.child_pid else {
        bail!("session {session_id}: {provider}'s pid is not recorded");
    };
    let Ok(pid) = i32::try_from(child_pid) else {
        bail!("{provider} pid {child_pid} is out of range");
    };
    // The keeper may have exited since the marker was read, and its pid been
    // reused. A pidfd pins whatever holds the pid now; what `review` spawned
    // leads its own process group, is the owner's child, and has the start
    // time the marker recorded. Checked after the pidfd is open and confirmed
    // still unexited afterwards, so the checks were of the process the signal
    // will reach.
    let gone = || {
        anyhow::anyhow!(
            "the {provider} run's keeper (pid {child_pid}) has already exited; the run is \
             finishing on its own"
        )
    };
    let pidfd = crate::keeper::pidfd_open(child_pid).map_err(|_| gone())?;
    // SAFETY: `getpgid` only reads the process table.
    let leads_group = unsafe { libc::getpgid(pid) } == pid;
    let identity = crate::keeper::stat_ppid_start(child_pid);
    let ours = leads_group
        && identity.is_some_and(|(ppid, start)| {
            ppid == marker.pid && marker.keeper_start.is_none_or(|s| s == start)
        })
        && !crate::keeper::pidfd_exited(&pidfd);
    if !ours {
        return Err(gone());
    }

    let count = |rows: &[SessionRecord]| rows.iter().filter(|r| r.session_id == session_id).count();
    let rows_before = count(&rows());
    crate::inflight::request_interrupt(session_id, data_root)?;
    if let Err(err) = crate::keeper::pidfd_signal(&pidfd, libc::SIGINT) {
        // Gone already, and the owner may have consumed the request on its way
        // out; then there is a run to report on after all.
        if crate::inflight::interrupt_pending(session_id, data_root) {
            let _ = crate::inflight::take_interrupt_request(session_id, data_root);
            bail!("failed to signal the {provider} run's keeper (pid {child_pid}): {err}");
        }
    } else {
        eprintln!(
            "interrupt sent to {provider} through its keeper (pid {child_pid}); waiting for the \
             run to wind down"
        );
    }

    // The owner consumes the request once the provider is reaped, so its absence says
    // the turn is over - without guessing from sidecar timestamps, which are
    // whole seconds and can tie with the session's previous row.
    while crate::inflight::interrupt_pending(session_id, data_root) {
        if !crate::inflight::pid_alive(marker.pid) {
            // Nobody is left to consume it; withdraw it so it cannot mark a
            // later resume of this session as interrupted.
            let _ = crate::inflight::take_interrupt_request(session_id, data_root);
            bail!(
                "the owning review (pid {}) exited before {provider} did; session \
                 {session_id} was not recorded, so it cannot be resumed through review",
                marker.pid
            );
        }
        tokio::time::sleep(crate::timings::INTERRUPT_POLL).await;
    }

    // Now the owner's rows. An interrupted auto-resume records the dead first
    // run and then the resume, back to back, so a lone uninterrupted new row
    // gets one more look before it is taken as the answer.
    let mut looked_again = false;
    loop {
        let all = rows();
        let new: Vec<&SessionRecord> = all
            .iter()
            .filter(|r| r.session_id == session_id)
            .skip(rows_before)
            .collect();
        if let Some(row) = new
            .iter()
            .find(|r| r.digest.as_ref().is_some_and(|d| d.interrupted))
        {
            return Ok(Outcome {
                interrupted: true,
                project: row.project.clone(),
                provider: provider.to_string(),
            });
        }
        if let Some(row) = new.last() {
            if looked_again {
                return Ok(Outcome {
                    interrupted: false,
                    project: row.project.clone(),
                    provider: provider.to_string(),
                });
            }
            looked_again = true;
        } else if !crate::inflight::pid_alive(marker.pid) {
            bail!(
                "the owning review (pid {}) exited without recording session {session_id}, \
                 so it cannot be resumed through review",
                marker.pid
            );
        }
        tokio::time::sleep(crate::timings::INTERRUPT_POLL).await;
    }
}

/// Why nothing may launch on, or interrupt, a session whose owning `review` is
/// gone while its keeper still tears the run down. Shared with `review
/// message`.
pub fn cleanup_pending_message(session_id: &str, marker: &crate::inflight::Marker) -> String {
    format!(
        "session {session_id}: the review that ran it (pid {}) is gone and its processes are \
         still being torn down (keeper pid {}); retry shortly",
        marker.pid,
        marker
            .keeper
            .map_or_else(|| "?".to_string(), |k| k.to_string())
    )
}

/// Whether a run of `provider` can be ended mid-turn and resumed: both codex and
/// claude close the turn on `SIGINT` and leave a resumable session. Grok has no
/// such lever. Shared with `review message`, so the two cannot disagree.
pub(crate) fn interruptible(provider: &str) -> bool {
    matches!(provider, "codex" | "claude")
}
