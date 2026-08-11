//! In-flight run markers.
//!
//! # Why this exists
//!
//! The sidecar log (`sessions.rs`) is written *after* a run returns, so a run
//! that is still going - or one that is wedged and will never return - is
//! completely invisible to `review sessions`. During the 10-hour codex hang that
//! motivated the watchdog, `review sessions` showed the session sitting at one
//! touch the entire time, with the *previous* turn's response, which read
//! exactly like "nothing is happening" while codex was in fact running. The
//! touch count only ever increments on detected completion, so it cannot be used
//! to tell working from wedged.
//!
//! A marker file is written as soon as the session id is known and removed when
//! the run returns, which gives `review sessions` a third state to report:
//! "turn in flight since <time>".
//!
//! # Why a file rather than a sidecar row
//!
//! The row would have to be written from `provider.rs`, which has the session id
//! but none of the audit metadata (`audit_id`, `private`, archetype) that a
//! sidecar row requires - threading all of it down just to mark liveness is a
//! lot of plumbing for a fact that is worthless once the run ends. A marker file
//! is naturally self-cleaning, and its staleness is independently checkable via
//! the recorded pid.
//!
//! # Staleness
//!
//! Removal is best-effort: `Drop` covers normal returns, errors and panics, but
//! not `SIGKILL` of `review` itself. Each marker therefore records the `review`
//! pid that owns it, and readers treat a marker whose pid is gone as stale
//! rather than as a live turn. Every failure in here warns and continues - a
//! liveness hint must never be able to derail an actual run.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Serialize, Deserialize)]
pub struct Marker {
    pub session_id: String,
    pub provider: String,
    pub project: String,
    /// UNIX seconds when the run was launched.
    pub started_epoch: u64,
    /// pid of the owning `review` process, used to detect stale markers left by
    /// a `review` that was killed outright.
    pub pid: u32,
}

fn dir() -> Option<PathBuf> {
    let data_dir = std::env::var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|_| std::env::var("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .ok()?;
    Some(data_dir.join("review").join("inflight"))
}

/// Is `pid` a live process? Signal 0 performs permission and existence checks
/// without delivering anything. `ESRCH` means gone; `EPERM` means it exists but
/// belongs to someone else, which still counts as alive.
fn pid_alive(pid: u32) -> bool {
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    // SAFETY: `kill` with signal 0 delivers nothing; it only reports whether the
    // pid exists. No memory is touched.
    let ret = unsafe { libc::kill(pid, 0) };
    ret == 0 || std::io::Error::last_os_error().kind() == std::io::ErrorKind::PermissionDenied
}

/// Removes its marker file on drop, so the marker's lifetime is exactly the
/// run's - including on early returns and panics.
pub struct Guard(Option<PathBuf>);

impl Drop for Guard {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Record that `session_id` is running now. Returns a guard that removes the
/// marker when dropped; a `Guard(None)` on any failure, so a broken data dir
/// silently degrades to the old no-visibility behaviour instead of failing the
/// run.
pub fn mark(session_id: &str, provider: &str, project: &str) -> Guard {
    let Some(dir) = dir() else {
        return Guard(None);
    };
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!("warning: failed to create inflight dir: {e}");
        return Guard(None);
    }
    // The session id is a UUID, so it is safe as a filename and unique per run.
    let path = dir.join(format!("{session_id}.json"));
    let marker = Marker {
        session_id: session_id.to_string(),
        provider: provider.to_string(),
        project: project.to_string(),
        started_epoch: crate::provider::now_epoch_secs(),
        pid: std::process::id(),
    };
    let json = match serde_json::to_string(&marker) {
        Ok(j) => j,
        Err(e) => {
            eprintln!("warning: failed to serialize inflight marker: {e}");
            return Guard(None);
        }
    };
    if let Err(e) = std::fs::write(&path, json) {
        eprintln!("warning: failed to write inflight marker: {e}");
        return Guard(None);
    }
    Guard(Some(path))
}

/// Every currently-live in-flight marker. Markers whose owning `review` process
/// is gone are stale (killed mid-run) and are both skipped and cleaned up here,
/// so the directory cannot accumulate lies over time.
pub fn read_live() -> Vec<Marker> {
    let Some(dir) = dir() else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(marker) = serde_json::from_str::<Marker>(&content) else {
            continue;
        };
        if pid_alive(marker.pid) {
            out.push(marker);
        } else {
            let _ = std::fs::remove_file(&path);
        }
    }
    out
}
