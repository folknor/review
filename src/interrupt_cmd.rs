//! `review interrupt <ID>`: stop a codex run mid-turn and hand back its session.
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
//!
//! # How
//!
//! 1. The run's in-flight marker names codex's pid, the leader of the process
//!    group `review` spawned it into - offered only once codex has produced
//!    output, so it is never a node wrapper too young to forward the signal.
//! 2. An interrupt request is written beside the marker *before* the signal, so
//!    the owning `review` can tell this apart from a death once codex exits.
//! 3. `SIGINT` goes to that pid alone - on an npm install the node wrapper,
//!    which forwards it to the native binary once. Signalling the whole group
//!    would hand the native binary a second copy. codex answers `SIGINT` with a
//!    `turn/interrupt`, so the turn ends cleanly and the session lock is freed.
//! 4. We wait for the owner to consume the request (codex has been reaped),
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
        println!("{}", crate::provider::resume_hint("codex", session_id));
    } else {
        // codex finished before the signal took effect; its answer is in the
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
pub(crate) struct Outcome {
    pub interrupted: bool,
    pub project: String,
}

/// The verb, minus printing. `data_root` and `rows` (every sidecar row, oldest
/// first) are injectable so the whole sequence can run against a stub codex.
pub(crate) async fn interrupt(
    session_id: &str,
    data_root: Option<&Path>,
    rows: impl Fn() -> Vec<SessionRecord>,
) -> Result<Outcome> {
    let Some(marker) = crate::inflight::live_marker(session_id, data_root) else {
        bail!(
            "no run of session {session_id} is in flight on this host\n  \
             `review sessions --all` lists the ones that are"
        );
    };
    if marker.provider != "codex" {
        bail!(
            "session {session_id} is a {} run; only codex runs can be interrupted",
            marker.provider
        );
    }
    let Some(child_pid) = marker.child_pid else {
        bail!(
            "session {session_id} cannot be interrupted yet: codex has produced no output, \
             or the run was launched by an older `review` (pid {}) that does not record \
             codex's pid",
            marker.pid
        );
    };
    let Ok(pid) = i32::try_from(child_pid) else {
        bail!("codex pid {child_pid} is out of range");
    };
    // codex may have exited since the marker was read, and its pid been reused.
    // What `review` spawned leads its own process group and is the owner's
    // child; anything else at that pid is not ours to signal.
    // SAFETY: `getpgid` only reads the process table.
    let leads_group = unsafe { libc::getpgid(pid) } == pid;
    if !leads_group || parent_pid(pid) != Some(marker.pid) {
        bail!("codex (pid {child_pid}) has already exited; the run is finishing on its own");
    }

    let count = |rows: &[SessionRecord]| rows.iter().filter(|r| r.session_id == session_id).count();
    let rows_before = count(&rows());
    crate::inflight::request_interrupt(session_id, data_root)?;
    // SAFETY: a pid verified above to be the owner's group-leading child.
    if unsafe { libc::kill(pid, libc::SIGINT) } != 0 {
        let err = std::io::Error::last_os_error();
        // Gone already, and the owner may have consumed the request on its way
        // out; then there is a run to report on after all.
        if crate::inflight::interrupt_pending(session_id, data_root) {
            let _ = crate::inflight::take_interrupt_request(session_id, data_root);
            bail!("failed to signal codex (pid {child_pid}): {err}");
        }
    } else {
        eprintln!("interrupt sent to codex (pid {child_pid}); waiting for the run to wind down");
    }

    // The owner consumes the request once codex is reaped, so its absence says
    // the turn is over - without guessing from sidecar timestamps, which are
    // whole seconds and can tie with the session's previous row.
    while crate::inflight::interrupt_pending(session_id, data_root) {
        if !crate::inflight::pid_alive(marker.pid) {
            // Nobody is left to consume it; withdraw it so it cannot mark a
            // later resume of this session as interrupted.
            let _ = crate::inflight::take_interrupt_request(session_id, data_root);
            bail!(
                "the owning review (pid {}) exited before codex did; session {session_id} \
                 was not recorded, so it cannot be resumed through review",
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
            });
        }
        if let Some(row) = new.last() {
            if looked_again {
                return Ok(Outcome {
                    interrupted: false,
                    project: row.project.clone(),
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

/// `pid`'s parent, from `/proc/<pid>/stat`. `comm` may contain spaces and
/// parentheses, so the fields are read from after the *last* `)`: state, then
/// ppid.
fn parent_pid(pid: i32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (_, fields) = stat.rsplit_once(')')?;
    fields.split_whitespace().nth(1)?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_process_knows_its_parent() {
        let me = i32::try_from(std::process::id()).expect("pid fits");
        // SAFETY: getppid has no failure mode.
        let parent = u32::try_from(unsafe { libc::getppid() }).expect("pid fits");
        assert_eq!(parent_pid(me), Some(parent));
    }
}
