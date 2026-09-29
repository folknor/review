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
    /// Unique per run, so a run can tell its own marker from another run's of
    /// the same session (see `Guard`). Owner pid and start second together
    /// are not unique: one `review` could run a session twice within a second.
    /// Empty in markers written before the field existed.
    #[serde(default)]
    pub run_id: String,
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
///
/// Only while the file is still this run's. Two `review` processes can run the
/// same session at once - two `review message` calls to an idle claude session
/// both launch, claude having no session lock of its own - and they share one
/// marker path. The second overwrites the first's marker; the first to finish
/// used to delete it regardless, leaving the still-running second invisible to
/// `review sessions` and `review interrupt`.
pub struct Guard(Option<(PathBuf, Marker)>);

impl Drop for Guard {
    fn drop(&mut self) {
        if let Some((path, marker)) = self.0.take()
            && still_ours(&path, &marker)
        {
            let _ = std::fs::remove_file(path);
        }
    }
}

impl Guard {
    /// Offer the provider's pid to `review interrupt`, once it is safe to
    /// signal (see `run_codex_json`). Best-effort like the rest of the marker,
    /// and skipped if another run has since taken the marker over (see `Guard`).
    pub fn record_child_pid(&mut self, child_pid: Option<u32>) {
        if let Some((path, marker)) = self.0.as_mut()
            && still_ours(path, marker)
        {
            marker.child_pid = child_pid;
            if let Err(e) = write_marker(path, marker) {
                eprintln!("warning: failed to update inflight marker: {e}");
            }
        }
    }
}

/// Whether the marker at `path` is the one `ours` describes: the same run. A
/// missing or unreadable file is not ours to touch.
///
/// A check then an action, so a second run could replace the marker between
/// the two. Closing that needs a lock around every marker write; the window is
/// microseconds wide and opens only for two runs of one session at once, which
/// `review message`'s launch lock already keeps apart.
fn still_ours(path: &Path, ours: &Marker) -> bool {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|content| serde_json::from_str::<Marker>(&content).ok())
        .is_some_and(|on_disk| on_disk.run_id == ours.run_id)
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
        run_id: crate::config::generate_uuid(),
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

/// Held by `review message` from before it checks whether the session has a turn
/// in flight until its own run has launched - by which point that run's
/// in-flight marker exists (every runner writes it before spawning when the
/// session id is known). Released by dropping it.
///
/// Without it, two messages to one idle session both saw no turn in flight and
/// both launched a turn on the same session at once. Codex's session lock made
/// the second fail; claude has none, so both ran. And a message waiting behind
/// the global lock was invisible to every other verb: no marker until it
/// launched. With it, a second message waits for the first to launch, then
/// finds its marker and interrupts it, as `review message` promises.
pub struct LaunchLock {
    /// Held only so the lock lives as long as this value: closing it releases
    /// the `flock`.
    _file: std::fs::File,
}

/// How long a launch lock file may sit unused before `lock_session_launch`
/// removes it. "Unused" is judged by modification time, which taking an
/// `flock` does not update, so every open and every acquisition touches the
/// file; and a file whose lock is held right now is never removed, whatever
/// its age. Removing one in use would split the session's messages across two
/// files, each holding a lock on its own.
const LAUNCH_LOCK_MAX_AGE: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);

/// Path of `session_id`'s launch lock, beside its marker.
fn launch_lock_path(session_id: &str, data_root: Option<&Path>) -> Option<PathBuf> {
    if !is_safe_filename_component(session_id) {
        return None;
    }
    Some(dir(data_root)?.join(format!("{session_id}.launch")))
}

/// Take `session_id`'s launch lock, waiting while another message to the same
/// session holds it. Blocking - call off the async runtime. `Ok(None)` when no
/// lock can be made (unsafe id, no data dir): the message then proceeds
/// unserialised, as it did before the lock existed, rather than failing.
pub fn lock_session_launch(
    session_id: &str,
    data_root: Option<&Path>,
) -> std::io::Result<Option<LaunchLock>> {
    use std::os::unix::io::AsRawFd;
    let Some(path) = launch_lock_path(session_id, data_root) else {
        return Ok(None);
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
        remove_stale_launch_locks(parent, &path);
    }
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)?;
    // Touched on open (so a waiter's file is recent) and again on acquiring;
    // see `LAUNCH_LOCK_MAX_AGE`. Best-effort: an untouched file is still a lock.
    let _ = file.set_modified(std::time::SystemTime::now());
    // SAFETY: flock on a descriptor we own; released when the file is closed.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        eprintln!("waiting for another message to session {session_id} to launch...");
        // SAFETY: as above.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        let _ = file.set_modified(std::time::SystemTime::now());
    }
    Ok(Some(LaunchLock { _file: file }))
}

/// Whether the lock file at `path` is held by anyone right now.
fn launch_lock_held(path: &Path) -> bool {
    use std::os::unix::io::AsRawFd;
    let Ok(file) = std::fs::File::open(path) else {
        return false;
    };
    // SAFETY: flock on a descriptor we own. A lock taken here is released when
    // `file` drops at the end of this function.
    let taken = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0;
    !taken
}

/// Session ids whose launch lock is held right now: messages waiting to launch,
/// which have no in-flight marker yet. For `review sessions`.
pub fn pending_launches(data_root: Option<&Path>) -> Vec<String> {
    let Some(dir) = dir(data_root) else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("launch"))
        .filter(|p| launch_lock_held(p))
        .filter_map(|p| p.file_stem().and_then(|s| s.to_str()).map(String::from))
        .collect()
}

/// Whether a message to `session_id` holds its launch lock right now: one is
/// waiting to launch, so the session is about to have a turn in flight.
pub fn launch_pending(session_id: &str, data_root: Option<&Path>) -> bool {
    launch_lock_path(session_id, data_root).is_some_and(|path| launch_lock_held(&path))
}

/// Remove launch lock files nobody has used in `LAUNCH_LOCK_MAX_AGE`, so the
/// directory does not collect one per session ever messaged. Best-effort.
fn remove_stale_launch_locks(dir: &Path, keep: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path == keep || path.extension().and_then(|e| e.to_str()) != Some("launch") {
            continue;
        }
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > LAUNCH_LOCK_MAX_AGE);
        if old && !launch_lock_held(&path) {
            let _ = std::fs::remove_file(path);
        }
    }
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
    fn a_run_does_not_delete_a_marker_another_run_has_taken_over() {
        let root = scratch_root();
        let sid = "019fefb0-227c-7c83-a398-380011b8e66b";
        let first = mark(sid, "claude", "/p", Some(&root));
        // A second run of the same session writes the same path - here from
        // the same `review` in the same second, so only the run id differs.
        let path = dir(Some(&root)).expect("dir").join(format!("{sid}.json"));
        let second = Marker {
            session_id: sid.to_string(),
            provider: "claude".to_string(),
            project: "/p".to_string(),
            started_epoch: crate::provider::now_epoch_secs(),
            pid: std::process::id(),
            child_pid: None,
            run_id: "another-run".to_string(),
        };
        write_marker(&path, &second).expect("second marker");
        drop(first);
        assert!(
            path.exists(),
            "the first run to finish must leave the second's marker in place"
        );
        // A marker that is still ours is removed as before.
        std::fs::remove_file(&path).expect("clear");
        drop(mark(sid, "claude", "/p", Some(&root)));
        assert!(!path.exists(), "our own marker goes when the run does");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_held_launch_lock_is_seen_as_a_pending_message() {
        let root = scratch_root();
        let sid = "019fefb0-227c-7c83-a398-380011b8e66c";
        assert!(!launch_pending(sid, Some(&root)), "nothing waiting yet");
        let lock = lock_session_launch(sid, Some(&root))
            .expect("lock")
            .expect("a safe id gets a lock");
        assert!(
            launch_pending(sid, Some(&root)),
            "a held lock means a message is waiting to launch"
        );
        drop(lock);
        assert!(
            !launch_pending(sid, Some(&root)),
            "released once the message has launched"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn cleanup_never_removes_a_held_launch_lock_however_old() {
        // flock does not touch mtime, so age alone cannot tell a held lock from
        // an abandoned one. Removing a held one would let a later message to
        // that session lock a fresh file and launch beside the holder.
        let root = scratch_root();
        let held_sid = "019fefb0-227c-7c83-a398-380011b8e66e";
        let held = lock_session_launch(held_sid, Some(&root))
            .expect("lock")
            .expect("lock");
        let held_path = launch_lock_path(held_sid, Some(&root)).expect("path");
        let two_days_ago = std::time::SystemTime::now() - 2 * LAUNCH_LOCK_MAX_AGE;
        std::fs::File::options()
            .write(true)
            .open(&held_path)
            .and_then(|f| f.set_modified(two_days_ago))
            .expect("age the file");
        // Another session's message runs the cleanup.
        drop(lock_session_launch(
            "019fefb0-227c-7c83-a398-380011b8e66f",
            Some(&root),
        ));
        assert!(held_path.exists(), "a held lock survives cleanup");
        // Released and old, it goes.
        drop(held);
        drop(lock_session_launch(
            "019fefb0-227c-7c83-a398-380011b8e670",
            Some(&root),
        ));
        assert!(!held_path.exists(), "an abandoned old lock is removed");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_second_launch_waits_for_the_first() {
        let root = scratch_root();
        let sid = "019fefb0-227c-7c83-a398-380011b8e66d";
        let first = lock_session_launch(sid, Some(&root))
            .expect("lock")
            .expect("lock");
        let (tx, rx) = std::sync::mpsc::channel();
        let waiter = {
            let root = root.clone();
            std::thread::spawn(move || {
                let second = lock_session_launch(sid, Some(&root)).expect("lock");
                tx.send(()).expect("send");
                drop(second);
            })
        };
        assert!(
            rx.recv_timeout(std::time::Duration::from_millis(200))
                .is_err(),
            "the second message must not launch while the first holds the lock"
        );
        drop(first);
        rx.recv_timeout(std::time::Duration::from_secs(5))
            .expect("the second proceeds once the first has launched");
        waiter.join().expect("join");
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
