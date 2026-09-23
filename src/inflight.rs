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
use std::path::{Path, PathBuf};

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
    /// pid of the provider process `review` spawned - the leader of its process
    /// group - which `review interrupt` signals. Absent in markers written before
    /// the field existed; such a run cannot be interrupted.
    #[serde(default)]
    pub child_pid: Option<u32>,
}

/// Where markers live. `data_root` overrides the real XDG location and exists so
/// the test harness cannot leave stub markers in the operator's actual
/// `~/.local/share/review/inflight` - redirecting `CODEX_HOME` only redirects
/// the *child*, not the paths `review` itself resolves.
fn dir(data_root: Option<&Path>) -> Option<PathBuf> {
    let data_dir = match data_root {
        Some(root) => root.to_path_buf(),
        None => std::env::var("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|_| std::env::var("HOME").map(|h| PathBuf::from(h).join(".local/share")))
            .ok()?,
    };
    Some(data_dir.join("review").join("inflight"))
}

/// Is `id` safe to use as a filename component?
///
/// The session id reaches here straight from `review resume <id>` on the command
/// line, and `review` deliberately delegates session-id validation to the
/// provider - so by the time we see it, it is arbitrary operator input. Using it
/// unchecked as a path component let `review resume ../foo` escape the marker
/// directory and write (and then, via `Guard::drop`, *delete*) a file elsewhere
/// under the data dir.
///
/// Provider session ids are UUIDs, so an allowlist of hex, dashes and
/// underscores is comfortably permissive while excluding `/`, `.` and anything
/// else that could traverse. An allowlist rather than a "reject `..`" denylist
/// because only the former is safe by construction.
fn is_safe_filename_component(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Is `pid` a live process? Signal 0 performs permission and existence checks
/// without delivering anything. `ESRCH` means gone; `EPERM` means it exists but
/// belongs to someone else, which still counts as alive.
pub fn pid_alive(pid: u32) -> bool {
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
pub struct Guard(Option<(PathBuf, Marker)>);

impl Drop for Guard {
    fn drop(&mut self) {
        if let Some((path, _)) = self.0.take() {
            let _ = std::fs::remove_file(path);
        }
    }
}

impl Guard {
    /// Offer the provider's pid to `review interrupt`, once it is safe to
    /// signal (see `run_codex_json`). Best-effort like the rest of the marker.
    pub fn record_child_pid(&mut self, child_pid: Option<u32>) {
        if let Some((path, marker)) = self.0.as_mut() {
            marker.child_pid = child_pid;
            if let Err(e) = write_marker(path, marker) {
                eprintln!("warning: failed to update inflight marker: {e}");
            }
        }
    }
}

/// Write a marker via a rename, so a concurrent reader never sees it
/// half-written - a marker that briefly failed to parse would read as "no run
/// in flight" to `review interrupt`. The temporary file's extension keeps it out
/// of `read_live`.
fn write_marker(path: &Path, marker: &Marker) -> std::io::Result<()> {
    let json = serde_json::to_string(marker).map_err(std::io::Error::other)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, path)
}

/// Record that `session_id` is running now. Returns a guard that removes the
/// marker when dropped; a `Guard(None)` on any failure, so a broken data dir
/// silently degrades to the old no-visibility behaviour instead of failing the
/// run.
pub fn mark(session_id: &str, provider: &str, project: &str, data_root: Option<&Path>) -> Guard {
    // Refuse to build a path out of anything that is not plainly a session id.
    // Skipping the marker only costs liveness reporting for that run; letting it
    // through would let a crafted resume id write and delete an arbitrary
    // file.
    if !is_safe_filename_component(session_id) {
        eprintln!("warning: session id is not a safe filename; skipping in-flight marker");
        return Guard(None);
    }
    let Some(dir) = dir(data_root) else {
        return Guard(None);
    };
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!("warning: failed to create inflight dir: {e}");
        return Guard(None);
    }
    // Validated above as a bare filename component, so this cannot escape `dir`.
    let path = dir.join(format!("{session_id}.json"));
    let marker = Marker {
        session_id: session_id.to_string(),
        provider: provider.to_string(),
        project: project.to_string(),
        started_epoch: crate::provider::now_epoch_secs(),
        pid: std::process::id(),
        // Offered later, once it is safe to signal - see `Guard::record_child_pid`.
        child_pid: None,
    };
    if let Err(e) = write_marker(&path, &marker) {
        eprintln!("warning: failed to write inflight marker: {e}");
        return Guard(None);
    }
    Guard(Some((path, marker)))
}

/// Every currently-live in-flight marker. Markers whose owning `review` process
/// is gone are stale (killed mid-run) and are both skipped and cleaned up here,
/// so the directory cannot accumulate lies over time - and so is an interrupt
/// request with no live marker beside it, which no run is left to consume.
///
/// `data_root` is `None` everywhere but tests: `review sessions` and `review
/// interrupt` are operator-facing and read the real location.
pub fn read_live(data_root: Option<&Path>) -> Vec<Marker> {
    let Some(dir) = dir(data_root) else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut requests = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        match path.extension().and_then(|e| e.to_str()) {
            Some("json") => {}
            Some("interrupt") => {
                requests.push(path);
                continue;
            }
            _ => continue,
        }
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
    // A run consumes its request before removing its marker, so a request whose
    // marker is gone is one nobody will ever read.
    for request in requests {
        let owned = request
            .file_stem()
            .and_then(|s| s.to_str())
            .is_some_and(|sid| out.iter().any(|m| m.session_id == sid));
        if !owned {
            let _ = std::fs::remove_file(&request);
        }
    }
    out
}

/// The live marker for `session_id`, if a run of it is in flight on this host.
pub fn live_marker(session_id: &str, data_root: Option<&Path>) -> Option<Marker> {
    read_live(data_root)
        .into_iter()
        .find(|m| m.session_id == session_id)
}

/// Is `session_id`'s interrupt request still waiting for its run to consume it?
pub fn interrupt_pending(session_id: &str, data_root: Option<&Path>) -> bool {
    interrupt_path(session_id, data_root).is_some_and(|p| p.exists())
}

/// Path of `session_id`'s interrupt request, beside its marker.
fn interrupt_path(session_id: &str, data_root: Option<&Path>) -> Option<PathBuf> {
    if !is_safe_filename_component(session_id) {
        return None;
    }
    Some(dir(data_root)?.join(format!("{session_id}.interrupt")))
}

/// Record that the operator asked for `session_id`'s run to be interrupted.
///
/// Written *before* the signal is sent, so the owning `review` - which checks
/// for it once codex has exited - can tell an interrupt it must honour from a
/// mid-turn death it should auto-resume past. The two look identical from the
/// exit status alone.
pub fn request_interrupt(session_id: &str, data_root: Option<&Path>) -> std::io::Result<()> {
    let path = interrupt_path(session_id, data_root).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "session id is not a safe filename",
        )
    })?;
    // Its existence is the whole message.
    std::fs::write(path, "")
}

/// Consume `session_id`'s interrupt request: whether one was pending. Called by
/// the owning run after codex exits, so a request never outlives the run it was
/// aimed at and cannot mark a later resume of the same session as interrupted.
/// Also how a request is withdrawn, with the result ignored.
pub fn take_interrupt_request(session_id: &str, data_root: Option<&Path>) -> bool {
    interrupt_path(session_id, data_root).is_some_and(|p| std::fs::remove_file(p).is_ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target/test-scratch")
            .join(crate::config::generate_uuid())
    }

    #[test]
    fn an_interrupt_request_is_consumed_exactly_once() {
        let root = scratch_root();
        std::fs::create_dir_all(root.join("review/inflight")).expect("scratch dir");
        let sid = "019fefb0-227c-7c83-a398-380011b8e66a";

        assert!(
            !take_interrupt_request(sid, Some(&root)),
            "nothing pending yet"
        );
        request_interrupt(sid, Some(&root)).expect("request");
        assert!(
            take_interrupt_request(sid, Some(&root)),
            "the request is seen"
        );
        assert!(
            !take_interrupt_request(sid, Some(&root)),
            "and consumed, so a later run of the session is not marked interrupted"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_interrupt_request_refuses_an_unsafe_id() {
        assert!(request_interrupt("../escape", Some(Path::new("unused"))).is_err());
    }

    #[test]
    fn a_marker_from_before_child_pid_still_parses() {
        let old =
            r#"{"session_id":"s","provider":"codex","project":"/p","started_epoch":1,"pid":2}"#;
        let marker: Marker = serde_json::from_str(old).expect("old marker parses");
        assert_eq!(marker.child_pid, None);
    }

    #[test]
    fn accepts_provider_session_ids() {
        assert!(is_safe_filename_component(
            "019fefb0-227c-7c83-a398-380011b8e66a"
        ));
        assert!(is_safe_filename_component("abc_123-DEF"));
    }

    #[test]
    fn rejects_path_traversal() {
        // `review resume ../sessions` would otherwise write and then delete a file
        // outside the marker directory.
        assert!(!is_safe_filename_component("../sessions"));
        assert!(!is_safe_filename_component("../../etc/passwd"));
        assert!(!is_safe_filename_component("a/b"));
        assert!(!is_safe_filename_component("."));
        assert!(!is_safe_filename_component(".."));
        assert!(!is_safe_filename_component("with.dot"));
        assert!(!is_safe_filename_component(""));
    }

    #[test]
    fn rejects_absurdly_long_ids() {
        assert!(!is_safe_filename_component(&"a".repeat(129)));
    }
}
