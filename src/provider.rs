#[cfg(test)]
#[path = "provider_tests.rs"]
mod provider_tests;

use anyhow::{Context, Result};
use std::path::Path;
use std::process::Stdio;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

/// Check whether a provider binary is available on PATH.
pub fn is_available(provider: &str) -> bool {
    which::which(provider).is_ok()
}

/// Create a fresh, exclusively-owned temp file for codex's `-o` output and
/// return its path. A random unguessable name (UUID) plus `O_EXCL` creation
/// (`create_new`) makes collisions between concurrent runs impossible and
/// defeats symlink pre-planting on what used to be a predictable pid+archetype
/// path. The file is created empty; codex overwrites it with the final message.
fn new_output_file() -> Result<String> {
    new_temp_file("codex")
}

/// The same exclusive-creation dance for any provider temp file. `tag` only
/// distinguishes the files to a human reading the temp dir; the UUID is what
/// makes the name unique and unguessable.
fn new_temp_file(tag: &str) -> Result<String> {
    let dir = std::env::temp_dir();
    for _ in 0..8 {
        let path = dir.join(format!(
            "review-{tag}-{}.txt",
            crate::config::generate_uuid()
        ));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(_) => return Ok(path.to_string_lossy().into_owned()),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => {
                return Err(anyhow::anyhow!("failed to create {tag} temp file: {e}"));
            }
        }
    }
    anyhow::bail!("could not create a unique {tag} temp file after 8 tries")
}

/// Caps on how much provider output we buffer in memory. Generous - real reviews
/// are far under these; the cap only stops a runaway stream from OOMing review.
const STDOUT_CAPTURE_CAP: usize = 64 << 20; // 64 MiB
const STDERR_CAPTURE_CAP: usize = 8 << 20; // 8 MiB

/// Root sets already announced this process, so an identical set is printed once
/// while a *different* one always prints.
///
/// A fan-out today runs one profile in one directory, so every invocation
/// derives the same set and a plain "have we announced?" flag would do. Keying
/// on the set instead costs nothing and removes the trap: the moment roots can
/// differ between invocations - per-archetype profiles being the obvious next
/// config change - a flag would leave the *wider* run unannounced while the
/// operator had seen only the narrower block, which is precisely the invisible
/// permission this announcement exists to prevent.
static ROOTS_ANNOUNCED: std::sync::Mutex<Option<std::collections::HashSet<Vec<String>>>> =
    std::sync::Mutex::new(None);

/// Whether this exact root set still needs announcing.
fn claim_roots_announcement(paths: &[String]) -> bool {
    match ROOTS_ANNOUNCED.lock() {
        Ok(mut guard) => guard
            .get_or_insert_with(Default::default)
            .insert(paths.to_vec()),
        // A poisoned lock means another thread panicked mid-announcement.
        // Announcing twice is noise; staying silent about a widening is not.
        Err(_) => true,
    }
}

/// Buffer shared between a reader task and the runner.
///
/// The reader appends as bytes arrive rather than returning at EOF, so the
/// runner can take whatever has been captured *so far* even if the reader never
/// finishes. That matters because the drain grace can expire with real output
/// already in hand - the NDJSON stream, and with it the log lines and the
/// forensic `stdout.jsonl` - and throwing it away would blind exactly the
/// failure this code exists to diagnose.
type SharedBuf = std::sync::Arc<std::sync::Mutex<Vec<u8>>>;

fn new_shared_buf() -> SharedBuf {
    std::sync::Arc::new(std::sync::Mutex::new(Vec::new()))
}

/// Append `chunk` to a shared buffer, respecting `cap`. The lock is taken and
/// released inside this call and never held across an await.
fn push_capped(buf: &SharedBuf, chunk: &[u8], cap: usize) {
    if let Ok(mut guard) = buf.lock()
        && guard.len() < cap
    {
        let take = (cap - guard.len()).min(chunk.len());
        guard.extend_from_slice(&chunk[..take]);
    }
}

/// Read `r` to EOF into `out`, buffering at most `cap` bytes but continuing to
/// drain the rest (so the child never blocks on a full pipe).
async fn read_capped<R>(mut r: R, cap: usize, out: SharedBuf)
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt;
    let mut chunk = [0u8; 16384];
    loop {
        match r.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => push_capped(&out, &chunk[..n], cap),
        }
    }
}

/// Take a reader's output once the child is reaped, without waiting on it
/// forever.
///
/// Pipe EOF needs *every* holder of the write end to close it, so a reader can
/// outlive the child. We give it `grace` to finish, then abort it - an aborted
/// task stops reading, whereas simply dropping the `JoinHandle` would detach it
/// and leave it running - and take whatever it has buffered either way.
async fn collect_reader(
    mut handle: tokio::task::JoinHandle<()>,
    buf: &SharedBuf,
    grace: std::time::Duration,
    what: &str,
) -> Vec<u8> {
    if tokio::time::timeout(grace, &mut handle).await.is_err() {
        handle.abort();
        eprintln!(
            "warning: codex {what} was still open {}s after the process was reaped \
             (something inherited the pipe); using the {} bytes captured so far",
            grace.as_secs(),
            buf.lock().map(|b| b.len()).unwrap_or(0)
        );
    }
    buf.lock().map(|b| b.clone()).unwrap_or_default()
}

/// Read codex's stdout to EOF exactly like `read_capped`, but additionally scan
/// the NDJSON for the `thread.started` event and publish the session id on
/// `sid_tx` the moment it appears.
///
/// The watchdog (`crate::watchdog`) needs the session id to locate the rollout
/// transcript. On a resume we already know it, but on a fresh run codex only
/// reveals it mid-stream - and the old code could not see it until the whole
/// stream had hit EOF, which is precisely the thing that never happens on a
/// wedged run. Scanning stops as soon as the id is found, so the steady-state
/// cost is the same byte-copy loop as before.
///
/// The published id also survives a reader that never reaches EOF: it goes out
/// on the watch channel as soon as it is seen, so a truncated `stdout_buf` can
/// no longer cost us the session id (and with it the transcript, the recovery
/// path and the sidecar row).
async fn read_capped_stdout<R>(
    mut r: R,
    cap: usize,
    out: SharedBuf,
    sid_tx: tokio::sync::watch::Sender<Option<String>>,
    first_output: tokio::sync::watch::Sender<bool>,
    mut scanning: bool,
) where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt;
    let mut chunk = [0u8; 16384];
    // Carry for a line split across two reads; only maintained while scanning.
    let mut partial: Vec<u8> = Vec::new();
    loop {
        match r.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                // `send_if_modified` so only the first chunk wakes the watcher.
                first_output.send_if_modified(|seen| !std::mem::replace(seen, true));
                if scanning {
                    partial.extend_from_slice(&chunk[..n]);
                    // Process whole lines; keep the trailing fragment.
                    while let Some(nl) = partial.iter().position(|b| *b == b'\n') {
                        let line: Vec<u8> = partial.drain(..=nl).collect();
                        if let Ok(val) = serde_json::from_slice::<serde_json::Value>(&line)
                            && val.get("type").and_then(|t| t.as_str()) == Some("thread.started")
                            && let Some(id) = val.get("thread_id").and_then(|t| t.as_str())
                        {
                            // A closed receiver just means nobody is watching.
                            let _ = sid_tx.send(Some(id.to_string()));
                            scanning = false;
                            partial = Vec::new();
                            break;
                        }
                    }
                    // Guard against a pathological unterminated first line
                    // growing without bound while we wait for a newline.
                    if partial.len() > 1 << 20 {
                        partial.clear();
                    }
                }
                // Past the cap this keeps draining but stops buffering, so the
                // child never blocks on a full pipe.
                push_capped(&out, &chunk[..n], cap);
            }
        }
    }
}

/// Process groups of codex children currently running under this `review`.
///
/// Needed because codex is spawned into its *own* process group (see
/// `run_codex_json`), so a terminal `SIGINT` no longer reaches it implicitly and
/// a `SIGTERM` aimed at `review` never did. Without explicit forwarding, killing
/// `review` leaves codex running detached - observed in the field, where the
/// abandoned codex kept working and editing the tree for hours after its
/// operator thought it had been stopped.
static CODEX_GROUPS: std::sync::Mutex<Option<std::collections::HashSet<u32>>> =
    std::sync::Mutex::new(None);

fn register_group(pid: u32) {
    if let Ok(mut guard) = CODEX_GROUPS.lock() {
        guard.get_or_insert_with(Default::default).insert(pid);
    }
}

fn unregister_group(pid: u32) {
    if let Ok(mut guard) = CODEX_GROUPS.lock()
        && let Some(set) = guard.as_mut()
    {
        set.remove(&pid);
    }
}

/// Install the one process-wide handler for `SIGINT`/`SIGTERM`: kill every live
/// codex process group, then exit.
///
/// This is deliberately a single global supervisor rather than a `select!` arm
/// inside each run. Tokio's signal registration is process-wide and permanent -
/// once a `SignalKind::terminate()` stream has been created, the default
/// "terminate the process" behaviour is gone for the rest of the run, even after
/// the stream is dropped. Installing it per-run would therefore have left
/// `review` quietly ignoring every `SIGTERM` after its first codex invocation,
/// i.e. *harder* to kill than before - the opposite of the intent.
///
/// Exiting here means we forfeit the digest and incident bundle for the
/// interrupted run. That is the right trade: the operator asked us to stop, and
/// the rollout transcript is on disk regardless, so `review sessions <id>` can
/// still show what codex produced.
pub fn install_signal_supervisor() {
    let mut sigterm =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok();
    tokio::spawn(async move {
        let name = match sigterm.as_mut() {
            Some(term) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => "SIGINT",
                    _ = term.recv() => "SIGTERM",
                }
            }
            // SIGTERM handler unavailable: still forward Ctrl-C, and leave
            // SIGTERM on its OS default (which kills us, orphaning codex - the
            // old behaviour, and better than not handling anything).
            None => {
                let _ = tokio::signal::ctrl_c().await;
                "SIGINT"
            }
        };
        // Snapshot and release the lock before signalling; never hold it across
        // the kills.
        let pids: Vec<u32> = CODEX_GROUPS
            .lock()
            .ok()
            .and_then(|g| g.as_ref().map(|s| s.iter().copied().collect()))
            .unwrap_or_default();
        if !pids.is_empty() {
            eprintln!(
                "\n{name}: terminating {} running codex process group(s)",
                pids.len()
            );
        }
        let pids_to_kill = pids.clone();
        for pid in pids {
            terminate_group(pid, crate::timings::SIGKILL_ESCALATION);
        }
        // Give SIGTERM a brief moment to land before we go; the escalation to
        // SIGKILL inside `terminate_group` dies with us, so this is the only
        // window codex gets to shut down cleanly.
        tokio::time::sleep(crate::timings::SIGTERM_WINDOW).await;
        // SIGKILL the *original* snapshot, deliberately not a fresh read of the
        // registry. If codex exits promptly on SIGTERM its waiter unregisters
        // the group, so a re-read would omit precisely the case that matters: a
        // descendant that ignored SIGTERM and is still alive in that group.
        // `process::exit` below cancels `terminate_group`'s deferred escalation,
        // making this the last chance to reap them.
        for pid in pids_to_kill {
            if let Ok(pid) = i32::try_from(pid) {
                // SAFETY: negative pid addresses the process group; codex was
                // spawned with `process_group(0)` so this cannot reach `review`.
                unsafe {
                    libc::kill(-pid, libc::SIGKILL);
                }
            }
        }
        // 128 + signal number, the conventional shell encoding.
        std::process::exit(if name == "SIGINT" { 130 } else { 143 });
    });
}

/// Terminate a codex child *and everything it spawned* by signalling its whole
/// process group, escalating to `SIGKILL` after a grace period.
///
/// The group, not the pid: codex spawns exec-server processes, unified-exec
/// background "cells", network proxies and MCP servers, and a wedged codex is
/// exactly the case where those are still alive. Killing the pid alone would
/// orphan them. `run_codex_json` puts codex in its own group so `pgid` equals
/// the child pid and this cannot reach back and signal `review` itself.
fn terminate_group(pid: u32, escalate_after: std::time::Duration) {
    let Ok(pid) = i32::try_from(pid) else {
        return;
    };
    // SAFETY: a negative pid addresses the process group with that id. codex was
    // spawned with `process_group(0)`, so this group contains codex and its
    // descendants and nothing else.
    unsafe {
        libc::kill(-pid, libc::SIGTERM);
    }
    // Escalate if SIGTERM is ignored.
    tokio::spawn(async move {
        tokio::time::sleep(escalate_after).await;
        // SAFETY: as above. Harmless if the group is already gone (ESRCH).
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
    });
}

/// Removes a path on drop, so the codex `-o` temp file is cleaned up on every
/// exit path of the runner - including error returns before the normal read.
struct RemoveOnDrop(String);

impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Token accounting summed across a run's `turn.completed` events (codex).
#[derive(Default, Clone)]
pub struct Usage {
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_output_tokens: u64,
}

/// Structured summary of a codex run, distilled from its NDJSON stream plus the
/// `-o`/`--output-last-message` backstop. Absent for providers that don't emit
/// a machine-readable stream (claude `--print`).
pub struct Digest {
    /// Process exit code (`None` if terminated by a signal).
    pub exit_code: Option<i32>,
    /// Signal name when the process was killed by one, else `None`.
    pub signal: Option<String>,
    /// Whether the final message came from the authoritative `-o` file (which
    /// is written only on a real final message and survives a frozen stream).
    /// `false` means we fell back to the transcript or the last streamed
    /// message, or none at all.
    pub captured: bool,
    /// The `-o` file was empty but the real `final_answer` was salvaged from the
    /// on-disk rollout transcript (codex finished the turn but truncated its
    /// stream/`-o` on a shutdown-exit). The reported text is authentic, not an
    /// interim note.
    pub recovered_from_transcript: bool,
    pub turns: u32,
    pub usage: Usage,
    /// Non-JSON stdout lines (codex ERROR/WARN, apply_patch dumps). The harness
    /// can halt NDJSON emission on these, so we keep them visible.
    pub log_lines: Vec<String>,
    /// On-disk transcript forensics, read only when the run looks wrong
    /// (not captured, non-zero exit, or killed by a signal).
    pub transcript: Option<crate::transcript::TranscriptSummary>,
    /// Directory of the forensic bundle written for a suspicious run (stderr,
    /// raw stream, transcript tail, argv, codex version). `None` on clean runs.
    pub incident_path: Option<String>,
    /// Set when `review` killed codex rather than waiting for it to exit on its
    /// own: either the rollout watchdog fired (codex had written its final
    /// answer but would not exit) or the operator signalled us. The exit
    /// code/signal below will therefore describe *our* kill, not codex's own
    /// fate - without this field a watchdog kill would be indistinguishable
    /// from codex being killed by something else.
    pub terminated_by_review: Option<String>,
    /// How long the rollout had been unchanged when the watchdog decided to act.
    /// Recorded so a stall-timeout misfire on a healthy run shows how close the
    /// call was, rather than only that it happened.
    pub quiet_secs: Option<u64>,
    /// `type/payload.type` of the last rollout event - what codex was doing when
    /// it went silent.
    pub last_rollout_event: Option<String>,
    /// Message from a stream `error` / `turn.failed` event: codex itself
    /// explaining why the turn ended, most commonly an upstream refusal (content
    /// flagged, rate limit, auth). Distinct from every other failure mode here in
    /// that the cause is *known and stated*, so it must not be reported as a
    /// mystery death or retried - see `print_digest` and the auto-resume gate.
    pub turn_error: Option<String>,
    /// The operator ended this run with `review interrupt`. Its missing answer
    /// is therefore deliberate: no auto-resume, no incident bundle, and the
    /// output says how to continue the session instead of reporting a death.
    pub interrupted: bool,
}

/// Flat, serializable projection of a `Digest` for the sidecar and audit logs.
/// Flattened (no nested transcript) so failures are greppable after the fact -
/// e.g. `jq 'select(.digest.captured==false)'`. Without this, a codex run that
/// exits non-zero mid-shutdown is filed as a clean short `response` and the exit
/// status is lost. Absent for providers without a machine-readable stream.
#[derive(serde::Serialize, serde::Deserialize, Clone)]
pub struct DigestSummary {
    #[serde(default)]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal: Option<String>,
    pub captured: bool,
    #[serde(default)]
    pub recovered_from_transcript: bool,
    pub turns: u32,
    /// From the rollout transcript (codex): the turn reached `task_complete`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_complete: Option<bool>,
    /// From the rollout transcript (codex): a `stream_error` froze the stream.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_error: Option<bool>,
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_output_tokens: u64,
    /// Forensic-bundle directory for a suspicious run (see `incident.rs`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub incident_path: Option<String>,
    /// Why `review` killed codex, when it did. Persisted so the hang class is
    /// greppable after the fact:
    /// `jq 'select(.digest.terminated_by_review != null)'`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminated_by_review: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quiet_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_rollout_event: Option<String>,
    /// Codex's own stated reason for the turn ending (stream `error` /
    /// `turn.failed`). Persisted so refusals are greppable and never confused
    /// with the hang class: `jq 'select(.digest.turn_error != null)'`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_error: Option<String>,
    /// Ended by `review interrupt`: `jq 'select(.digest.interrupted)'`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub interrupted: bool,
}

impl Digest {
    pub fn summary(&self) -> DigestSummary {
        DigestSummary {
            exit_code: self.exit_code,
            signal: self.signal.clone(),
            captured: self.captured,
            recovered_from_transcript: self.recovered_from_transcript,
            turns: self.turns,
            task_complete: self.transcript.as_ref().map(|t| t.task_complete),
            stream_error: self.transcript.as_ref().map(|t| t.stream_error),
            input_tokens: self.usage.input_tokens,
            cached_input_tokens: self.usage.cached_input_tokens,
            output_tokens: self.usage.output_tokens,
            reasoning_output_tokens: self.usage.reasoning_output_tokens,
            incident_path: self.incident_path.clone(),
            terminated_by_review: self.terminated_by_review.clone(),
            quiet_secs: self.quiet_secs,
            last_rollout_event: self.last_rollout_event.clone(),
            turn_error: self.turn_error.clone(),
            interrupted: self.interrupted,
        }
    }
}

pub struct ProviderResult {
    pub provider: String,
    pub output: Result<String>,
    /// Session ID associated with this invocation, when one is known to the
    /// caller (a fresh run captures the freshly-created session).
    pub session_id: Option<String>,
    /// Structured run summary (codex only, today).
    pub digest: Option<Digest>,
    /// UNIX seconds when this run actually finished. Stamped here rather than at
    /// sidecar-record time so the cold-cache clock reflects completion, not the
    /// (possibly much later) moment results are collected in launch order.
    pub completed_epoch: u64,
    /// The filesystem permissions this run actually launched with, in the
    /// provider's own vocabulary. Recorded because the guarantee a profile
    /// advertises is worth nothing if the permissions a run *received* are only
    /// visible in the launching terminal: `jq 'select(.sandbox=="read-only")'`
    /// has to be able to answer "which runs could not write?" after the fact.
    pub sandbox: Option<String>,
    /// Writable roots derived for this run beyond the provider's defaults
    /// (`src/writable_roots.rs`). Empty for every run that widened nothing,
    /// which is every read-only run.
    pub writable_roots: Vec<String>,
    /// The model and reasoning effort this run launched with, recorded for the
    /// same reason `sandbox` is: they are properties of the *run*, and a
    /// `review resume` that cannot see them silently falls back to whatever
    /// the provider defaults to. `None` means "we passed nothing and the
    /// provider chose", which since `--ignore-user-config` means codex's
    /// built-in default rather than the operator's configured one.
    ///
    /// A profile `config` entry restating `model` or `model_reasoning_effort`
    /// is deliberately *not* parsed back out the way `writable_roots` is: which
    /// of `-m` and `-c model=` codex resolves last is not established here, and
    /// a confident wrong record is worse than an incomplete one.
    pub model: Option<String>,
    pub effort: Option<String>,
    /// What the provider reports actually served the run. Record-only.
    pub served: Served,
    /// The folder grok was asked to trust permanently (`--trust`) for this run,
    /// when it had no trust decision yet. Recorded because the grant outlives
    /// the run and widens every later grok session there (the repo's `.grok`
    /// MCP/LSP config runs without a prompt) - a change to the operator's
    /// config that must be findable after the terminal is gone.
    pub grok_trust: Option<String>,
}

/// What a provider reports, after the fact, about what served a run - as
/// opposed to `ProviderResult::model`, which is what we asked for.
///
/// The two are different names, not two spellings of one: grok's `-m grok-4.7`
/// is served as `grok-4.7-build`, and `-m grok-4.7-build` is refused as an
/// unknown model id. So this is recorded but **never** inherited by a
/// `review resume` - feeding it back to `-m` would fail the launch. Only grok
/// reports it today; empty for every other provider.
#[derive(Default, Clone, Debug, PartialEq)]
pub struct Served {
    /// Every model that served a call in the run, sorted and comma-joined (one
    /// in practice; a subagent on another model would add a second).
    pub model: Option<String>,
    pub cost_usd: Option<f64>,
}

pub fn now_epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Internal result of one provider run before it's wrapped into a
/// `ProviderResult`. Lets the run helpers return the session ID and digest
/// alongside the text without a widening tuple.
struct RunOutput {
    text: String,
    session_id: Option<String>,
    digest: Option<Digest>,
    served: Served,
}

/// Knobs on how a codex run is executed, injectable so tests can drive the real
/// `run_codex_json` path against a stub binary in milliseconds rather than
/// minutes. Production always uses `Default`.
#[derive(Clone)]
pub struct CodexRuntime {
    /// The binary to exec. Overridden in tests by a stub that reproduces the
    /// hang shapes (answer-then-hang, hang-with-no-answer, pipe-retaining
    /// descendant) deterministically.
    pub binary: String,
    /// Watchdog polling/patience.
    pub timings: crate::watchdog::Timings,
    /// How long a pipe reader gets to finish after the child is reaped before
    /// it is aborted and we take whatever it buffered.
    pub drain_grace: std::time::Duration,
    /// How long a process group gets to honour `SIGTERM` before `SIGKILL`.
    pub sigkill_escalation: std::time::Duration,
    /// Overrides the XDG data root used for in-flight markers and incident
    /// bundles. `None` in production. Tests set it because redirecting
    /// `CODEX_HOME` only redirects the *child* - `review`'s own paths are
    /// resolved from the test process's real `HOME`/`XDG_DATA_HOME`, so without
    /// this the suite wrote stub incident bundles into the operator's real
    /// `~/.local/share/review/incidents`.
    pub data_root: Option<std::path::PathBuf>,
}

impl CodexRuntime {
    /// Build from project config. `stall_timeout_secs` comes from
    /// `[_defaults].stall_timeout_secs`: `None` keeps the built-in default,
    /// `Some(0)` disables the stall branch, anything else sets it.
    pub fn from_config(stall_timeout_secs: Option<u64>) -> Self {
        let mut rt = Self::default();
        if let Some(secs) = stall_timeout_secs {
            rt.timings.stall_grace = (secs > 0).then(|| std::time::Duration::from_secs(secs));
        }
        rt
    }
}

impl Default for CodexRuntime {
    fn default() -> Self {
        Self {
            binary: "codex".to_string(),
            timings: crate::watchdog::Timings::default(),
            drain_grace: crate::timings::DRAIN_GRACE,
            sigkill_escalation: crate::timings::SIGKILL_ESCALATION,
            data_root: None,
        }
    }
}

/// Signals the caller that the provider process has actually been spawned.
///
/// The global lock exists to space out provider *launches*, so it has to be held
/// until the spawn has happened - releasing it beforehand leaves a critical
/// section that protects nothing, and lets every queued invocation through to
/// launch simultaneously. Sending on this (or dropping it, on a failed spawn)
/// is what tells the caller it is safe to release.
pub type LaunchSignal = tokio::sync::oneshot::Sender<()>;

#[allow(clippy::too_many_arguments)]
pub async fn invoke(
    provider: &str,
    session_id: &str,
    model: Option<&str>,
    effort: Option<&str>,
    sandbox: Option<&str>,
    profile_roots: &[String],
    env: Option<&std::collections::BTreeMap<String, String>>,
    config: &[String],
    prompt: &str,
    project_root: &Path,
    oneshot: bool,
    launched: Option<LaunchSignal>,
    runtime: &CodexRuntime,
) -> ProviderResult {
    // Profiles carry `review`'s own sandbox vocabulary; each provider spells the
    // levels differently and grok hard-errors on a name it cannot resolve, so
    // the translation happens once, here, and the runners below only ever see
    // their own native value.
    let sandbox = sandbox.map(|s| crate::config::sandbox_for(provider, s));
    let sandbox = sandbox.as_deref();
    let grok_write = if provider == "grok" {
        grok_write_launch(oneshot, sandbox)
    } else {
        None
    };
    // Roots are *derived* for a codex write run and a fresh grok write run. A
    // resumed grok session under a `review-ws-*` profile instead takes the roots
    // recorded with it, verbatim: the profile name is fixed for the session's
    // life (grok refuses to switch it) and names exactly one root set, so
    // re-deriving on another host would describe a different profile.
    let derives = grok_write == Some(GrokWrite::Fresh)
        || (provider == "codex" && sandbox == Some("workspace-write"));
    let resumes_profile = matches!(grok_write, Some(GrokWrite::Resume(_)));

    // Derived here rather than inside the runners so the same values that
    // widen the run are the ones recorded on the result: a grant that is applied
    // but not logged is exactly the invisible permission this is meant to avoid.
    let granted_roots: Vec<crate::writable_roots::GrantedRoot> = if resumes_profile {
        Vec::new()
    } else if derives {
        // Against `project_root`, not the process cwd: every runner spawns
        // the provider with `.current_dir(project_root)`, and `.review.toml`
        // discovery deliberately allows launching from a descendant of it.
        // Deriving from the process cwd inspected `<subdir>/target` instead
        // of the workspace's, so a run started one directory down silently
        // failed to grant the root its build needed.
        crate::writable_roots::derive_with(
            project_root,
            &crate::writable_roots::RealHost,
            profile_roots,
        )
    } else {
        // Silence here would be indistinguishable from the paths having been
        // granted: an operator who put `writable_roots` on the wrong profile
        // gets a build that fails for reasons the config appears to rule out.
        if !profile_roots.is_empty() {
            eprintln!(
                "warning: profile writable_roots ignored - they apply only to \
                     a codex or grok workspace-write profile (this run: {} {})",
                provider,
                sandbox.unwrap_or("read-only")
            );
        }
        Vec::new()
    };
    let mut root_paths: Vec<String> = if resumes_profile {
        profile_roots.to_vec()
    } else {
        granted_roots.iter().map(|g| g.path.clone()).collect()
    };
    // Grok's profile name is a hash of the canonical set, and the recorded
    // roots must hash back to it on resume - so record them canonically too.
    if grok_write.is_some() {
        root_paths = crate::grok_home::canonical_roots(&root_paths);
    }
    // A profile `config` entry restating the key wins, deliberately - but the
    // record must follow the run, not the derivation, or the sidecar makes a
    // confident false claim: a profile overriding to `["/secret"]` would be
    // logged with the derived roots, and one overriding to `[]` would be logged
    // as widened when it was not.
    // `config` is codex `-c` only; grok never sees it.
    if provider == "codex"
        && let Some(overridden) = config_writable_roots_override(config)
    {
        root_paths = overridden;
    }

    // A grok write run needs grok's own config ready before it launches, or it
    // fails in ways that cost a whole run to discover: trust (else the first
    // edit cancels the turn) and its roots profile (else every build dies at
    // the brokkr lock). A setup failure is a launch failure - running anyway
    // would reproduce those failures more expensively. Blocking file I/O and a
    // `flock` another `review` may hold, so off the async runtime.
    let mut trust_grant: Option<String> = None;
    let mut grok_profile: Option<String> = None;
    let setup: Result<()> = match grok_write.clone() {
        None => Ok(()),
        Some(kind) => {
            let root = project_root.to_path_buf();
            let roots = root_paths.clone();
            let env = env.cloned();
            let prepared = tokio::task::spawn_blocking(move || {
                prepare_grok_write(&kind, &root, &roots, env.as_ref())
            })
            .await
            .map_err(|e| anyhow::anyhow!("grok setup task failed: {e}"))
            .and_then(|r| r);
            prepared.map(|p| {
                grok_profile = p.profile;
                trust_grant = p.trust_grant;
            })
        }
    };
    // Announced once per distinct root set, not once per invocation: a fan-out
    // launches the same profile against several archetypes and would otherwise
    // repeat an identical block per run.
    //
    // What is announced is the *effective* set, not the derived one. Keying
    // either the guard or the loop on `granted_roots` broke the "every grant is
    // printed" fence three ways: a profile `config` override that replaced the
    // roots printed the derived paths instead of the ones the run actually got,
    // one that *added* roots to an empty derivation printed nothing at all, and
    // an override to `[]` printed grants the run did not receive. The whole
    // justification for deriving from ambient environment is that the result is
    // visible rather than inferred, so the line must follow the run.
    if !root_paths.is_empty() && claim_roots_announcement(&root_paths) {
        let other = if resumes_profile {
            "recorded with the session"
        } else {
            "profile config override"
        };
        for line in announcement_lines(provider, other, &granted_roots, &root_paths) {
            eprintln!("{line}");
        }
    }
    // A workspace-write run that ends up with nothing outside the workspace is
    // usually a stripped environment rather than a workspace that needs nothing
    // (see `writable_roots::derive`, which has a legitimate zero case). It is
    // therefore a policy warning, not a correctness diagnostic - but silence
    // here is what let 40 consecutive runs record no roots unnoticed while every
    // build inside them died at a lock outside the sandbox.
    if derives && root_paths.is_empty() {
        eprintln!(
            "warning: workspace-write run with no writable roots outside the \
             workspace - a build needing one (the build lock, the cargo home, \
             or a shared target directory) will fail read-only. Declare it with \
             `writable_roots` on this profile if so."
        );
    }

    let sandbox = grok_profile.as_deref().or(sandbox);

    let result = match (provider, setup) {
        (_, Err(e)) => Err(e),
        (provider, Ok(())) => match provider {
            // `sandbox` is an OS filesystem sandbox, which codex and grok both have
            // and claude does not: claude's `--permission-mode` is a tool-approval
            // policy on a different axis with no honest mapping, so claude ignores
            // it. `config` (codex `-c`) stays codex-only.
            "claude" => {
                run_claude(
                    session_id,
                    model,
                    effort,
                    env,
                    prompt,
                    project_root,
                    oneshot,
                    launched,
                )
                .await
            }
            "grok" => {
                run_grok(
                    session_id,
                    model,
                    effort,
                    sandbox,
                    trust_grant.is_some(),
                    env,
                    prompt,
                    project_root,
                    oneshot,
                    launched,
                )
                .await
            }
            "codex" => {
                run_codex(
                    session_id,
                    model,
                    effort,
                    sandbox,
                    &granted_roots,
                    env,
                    config,
                    prompt,
                    project_root,
                    oneshot,
                    launched,
                    runtime,
                )
                .await
            }
            other => Err(anyhow::anyhow!("unknown provider: {other}")),
        },
    };
    // Record a trust grant only if it actually landed. Grok reports a refused or
    // process-local grant only on stderr (surfaced by `run_grok`), and a
    // process-local one leaves the next run untrusted - so "we passed --trust"
    // is not "the folder is now permanently trusted", and the sidecar field
    // claims the latter. Re-read after the run; small file, no lock.
    if trust_grant.is_some() {
        let landed = crate::grok_home::Grok::resolve(env).is_ok_and(|g| {
            matches!(
                g.check_trust(project_root),
                Ok(crate::grok_home::Trust::Trusted)
            )
        });
        if !landed {
            eprintln!(
                "warning: grok --trust did not durably trust {} - the next write run \
                 there will try again",
                trust_grant.as_deref().unwrap_or_default()
            );
            trust_grant = None;
        }
    }
    let completed_epoch = now_epoch_secs();
    match result {
        Ok(run) => ProviderResult {
            provider: provider.to_string(),
            output: Ok(run.text),
            session_id: run.session_id,
            digest: run.digest,
            completed_epoch,
            sandbox: effective_sandbox(provider, sandbox),
            writable_roots: root_paths,
            model: model.map(str::to_string),
            effort: effort.map(str::to_string),
            served: run.served,
            grok_trust: trust_grant,
        },
        Err(e) => ProviderResult {
            provider: provider.to_string(),
            output: Err(e),
            session_id: None,
            digest: None,
            completed_epoch,
            // A launch that failed still launched with these permissions
            // requested, and a row that omitted them would read as a run with
            // none.
            sandbox: effective_sandbox(provider, sandbox),
            writable_roots: root_paths,
            model: model.map(str::to_string),
            effort: effort.map(str::to_string),
            served: Served::default(),
            // Requested, as `sandbox` is: if grok got as far as running, the
            // grant happened before whatever failed.
            grok_trust: trust_grant,
        },
    }
}

/// The writable roots a profile `config` entry sets, when one does.
///
/// Profile overrides are passed after the derived `-c`, so codex takes the last
/// one and a profile restating this key replaces the derivation entirely. The
/// last entry wins here for the same reason. Returns `None` when no entry
/// touches the key, and `Some(vec![])` for an explicit empty override - which is
/// a real, recordable state, not the absence of one.
/// The `writable root` lines for a launch: one per **effective** root, carrying
/// the derivation's reason where the path came from the derivation and saying so
/// plainly where it did not.
///
/// Split out from `invoke` because the defect this replaced was invisible in
/// review: the announcement read from the derived set while the run used the
/// effective one, so the two could disagree with nothing failing. A pure
/// function over both sets is the only shape that can be tested.
fn announcement_lines(
    provider: &str,
    other: &str,
    granted_roots: &[crate::writable_roots::GrantedRoot],
    root_paths: &[String],
) -> Vec<String> {
    root_paths
        .iter()
        .map(|path| {
            // A derived root can state why a build needs it; one that arrived
            // another way - a codex `config` override, or a grok profile root
            // another project added - cannot, and must not borrow a reason from
            // a derivation it did not come from.
            match granted_roots.iter().find(|g| &g.path == path) {
                Some(grant) => format!("{provider}: writable root {} ({})", grant.path, grant.why),
                None => format!("{provider}: writable root {path} ({other})"),
            }
        })
        .collect()
}

/// How a grok run that may write is launched.
#[derive(Debug, Clone, PartialEq, Eq)]
enum GrokWrite {
    /// A fresh write run: roots derived, launched under the `review-ws-*`
    /// profile named for them.
    Fresh,
    /// A resume of a session created under a `review-ws-*` profile: the same
    /// profile, with the roots recorded with the session. Grok fixes a
    /// session's sandbox profile for its lifetime and refuses a different
    /// `--sandbox` on resume (`resolve_startup_sandbox` → `Conflict`).
    Resume(String),
    /// A resume of a session created under grok's built-in `workspace`, before
    /// `review` made profiles. It can only continue as it began: trusted, but
    /// without the extra roots.
    BuiltinWorkspace,
}

/// Whether, and how, a grok run writes. A fresh `workspace` run takes a new
/// profile; a resume passes the recorded name through unchanged, because grok
/// refuses any other.
fn grok_write_launch(oneshot: bool, sandbox: Option<&str>) -> Option<GrokWrite> {
    match sandbox {
        Some(name) if name.starts_with(crate::grok_home::PROFILE_PREFIX) => {
            Some(GrokWrite::Resume(name.to_string()))
        }
        Some("workspace") if oneshot => Some(GrokWrite::Fresh),
        Some("workspace") => Some(GrokWrite::BuiltinWorkspace),
        _ => None,
    }
}

/// What `prepare_grok_write` decided.
#[derive(Debug, Default)]
struct GrokPrepared {
    /// The `review-ws-*` profile to launch under, when the run uses one.
    profile: Option<String>,
    /// The trust key `--trust` will grant, when the project had no decision
    /// yet. `None` when it was already trusted: no flag, no grant, nothing to
    /// announce.
    trust_grant: Option<String>,
}

/// Get grok's own config ready for a write run.
///
/// Trust is granted by grok (`--trust`, grok's own key and lock), and only when
/// the project has no decision yet - a grant is a **permanent**, host-wide
/// decision in `trusted_folders.toml`, and it means more than "may edit": grok's
/// folder trust gates whether the repo's own `.grok` MCP/LSP config may spawn
/// servers (`trust.rs` module docs), so from then on every grok session there,
/// interactive ones included, runs that config without asking. It is therefore
/// announced on the run that makes it, and recorded in the sidecar.
/// `check_trust` refuses the cases where the grant would be wrong.
///
/// Creating a profile is printed, because it edits the operator's file and an
/// edit nobody saw is the ambient widening the rest of this file refuses; an
/// existing profile is silent - it was announced when it was made.
fn prepare_grok_write(
    kind: &GrokWrite,
    project_root: &Path,
    roots: &[String],
    env: Option<&std::collections::BTreeMap<String, String>>,
) -> Result<GrokPrepared> {
    let grok = crate::grok_home::Grok::resolve(env)?;
    let mut prepared = GrokPrepared::default();
    if grok.check_trust(project_root)? == crate::grok_home::Trust::Undecided {
        let key = grok.workspace_key(project_root);
        eprintln!(
            "grok: trusting {} permanently in {} - a write run's edits are cancelled in \
             an untrusted folder; grok will also run this repo's .grok MCP/LSP config \
             without asking from now on",
            key.display(),
            grok.home.join("trusted_folders.toml").display()
        );
        prepared.trust_grant = Some(key.to_string_lossy().into_owned());
    }
    let name = match kind {
        GrokWrite::BuiltinWorkspace => return Ok(prepared),
        GrokWrite::Fresh => crate::grok_home::profile_name(roots),
        GrokWrite::Resume(name) => {
            // The recorded roots must be the set the session's profile is named
            // for; anything else is a row edited by hand or a record from a
            // different run, and launching would grant a set nobody recorded.
            if crate::grok_home::profile_name(roots) != *name {
                anyhow::bail!(
                    "the roots recorded with this session do not match its grok \
                     sandbox profile `{name}` - start a fresh run"
                );
            }
            name.clone()
        }
    };
    // Recreated on resume if someone deleted it: the name pins the exact set,
    // so recreating it grants nothing the session did not already have.
    if grok.ensure_profile(&name, roots)? {
        eprintln!(
            "grok: created sandbox profile `{name}` in {} (writable roots: {})",
            grok.home.join("sandbox.toml").display(),
            if roots.is_empty() {
                "none beyond grok's workspace".to_string()
            } else {
                roots.join(", ")
            }
        );
    }
    prepared.profile = Some(name);
    Ok(prepared)
}

fn config_writable_roots_override(config: &[String]) -> Option<Vec<String>> {
    const KEY: &str = "sandbox_workspace_write.writable_roots";
    let entry = config
        .iter()
        .rev()
        .find(|c| c.trim_start().starts_with(KEY))?;
    // Parse rather than string-scrape: the value is TOML, and a hand-written
    // profile can spell an array in ways a regex would get wrong.
    let parsed = entry.parse::<toml::Table>().ok()?;
    let roots = parsed
        .get("sandbox_workspace_write")?
        .get("writable_roots")?
        .as_array()?;
    Some(
        roots
            .iter()
            .filter_map(|v| v.as_str().map(ToString::to_string))
            .collect(),
    )
}

/// The sandbox level a run will actually be launched under, as opposed to the
/// one the caller asked for.
///
/// The two differ wherever the caller passes no level: the runners still pass
/// `--sandbox read-only`, so the run genuinely is read-only, and recording the
/// caller's `None` left such rows invisible to the exact query the sidecar
/// fields exist to answer, `jq 'select(.sandbox=="read-only")'`. On the
/// `review resume` path the caller is `main::inherited_settings`, which
/// supplies the level the session was created with; `None` there means the
/// session's record carries no level to inherit (a row written before levels
/// were recorded). Claude has no filesystem sandbox on this axis, so it records
/// none.
fn effective_sandbox(provider: &str, sandbox: Option<&str>) -> Option<String> {
    match provider {
        "codex" | "grok" => Some(sandbox.unwrap_or("read-only").to_string()),
        _ => None,
    }
}

/// Write prompt to stdin on a spawned task. Returns an error if the write fails.
async fn write_stdin(
    stdin: tokio::process::ChildStdin,
    prompt_bytes: Vec<u8>,
) -> Result<(), anyhow::Error> {
    let handle = tokio::spawn(async move {
        let mut stdin = stdin;
        let result = stdin.write_all(&prompt_bytes).await;
        drop(stdin);
        result
    });

    match handle.await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(anyhow::anyhow!("failed to write prompt to stdin: {e}")),
        Err(e) => Err(anyhow::anyhow!("stdin write task panicked: {e}")),
    }
}

/// Run a provider that outputs to stdout (claude).
/// Shared logic for stdin pipe → stdout capture.
async fn run_with_stdout(
    mut child: tokio::process::Child,
    prompt: &str,
    provider: &str,
) -> Result<String> {
    let stdin = child
        .stdin
        .take()
        .with_context(|| format!("failed to open {provider} stdin"))?;
    let write_result = write_stdin(stdin, prompt.as_bytes().to_vec());
    let output = child.wait_with_output();

    let (write_res, output) = tokio::join!(write_result, output);
    let output = output.with_context(|| format!("failed to wait for {provider}"))?;

    if !output.status.success() {
        if let Err(e) = write_res {
            anyhow::bail!("failed to write prompt: {e}");
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("{provider} exited with error: {}", stderr.trim());
    }

    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[allow(clippy::too_many_arguments)]
async fn run_claude(
    session_id: &str,
    model: Option<&str>,
    effort: Option<&str>,
    env: Option<&std::collections::BTreeMap<String, String>>,
    prompt: &str,
    project_root: &Path,
    oneshot: bool,
    launched: Option<LaunchSignal>,
) -> Result<RunOutput> {
    // In oneshot mode, generate a UUID up front and pass it via --session-id
    // so the fresh session is persistable and the operator can follow up via
    // `review resume <id>`. (Previously used --no-session-persistence, which made
    // the session unreachable.)
    let oneshot_id = if oneshot {
        Some(crate::config::generate_uuid())
    } else {
        None
    };

    let mut args: Vec<&str> = if let Some(ref id) = oneshot_id {
        vec![
            "--session-id",
            id,
            "--print",
            "--permission-mode",
            "dontAsk",
        ]
    } else {
        vec![
            "--resume",
            session_id,
            "--print",
            "--permission-mode",
            "dontAsk",
        ]
    };
    let model_owned;
    if let Some(m) = model {
        model_owned = m.to_string();
        args.push("--model");
        args.push(&model_owned);
    }
    let effort_owned;
    if let Some(e) = effort {
        effort_owned = e.to_string();
        args.push("--effort");
        args.push(&effort_owned);
    }

    let mut cmd = Command::new("claude");
    cmd.args(&args)
        .current_dir(project_root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(vars) = env {
        cmd.envs(vars);
    }
    let child = cmd.spawn().context("failed to spawn claude")?;
    // Launch has happened: the caller may release the global lock. Dropping the
    // sender instead (on the `?` above) tells it the same thing.
    if let Some(signal) = launched {
        let _ = signal.send(());
    }

    let text = run_with_stdout(child, prompt, "claude").await?;
    Ok(RunOutput {
        text,
        session_id: oneshot_id,
        digest: None,
        served: Served::default(),
    })
}

/// Grok's headless result object (`--output-format json`). One JSON object on
/// stdout per run, whatever the outcome; the fields we don't use (usage,
/// requestId, thought) are ignored rather than modelled. The served model and
/// cost are read separately, by `grok_served`.
#[derive(serde::Deserialize)]
struct GrokResult {
    #[serde(default)]
    text: String,
    #[serde(rename = "stopReason", default)]
    stop_reason: Option<String>,
    #[serde(rename = "sessionId", default)]
    session_id: Option<String>,
    /// Grok also prints failures that happen *before* a turn as a typed line on
    /// stdout (`{"type":"error","message":...}`). Every field above is optional,
    /// so such a line parses cleanly as an all-default result - these two carry
    /// the reason that would otherwise be silently discarded.
    #[serde(rename = "type", default)]
    kind: Option<String>,
    #[serde(default)]
    message: Option<String>,
    /// Not read for its value - only as evidence that grok got as far as
    /// running a turn. See `ran_a_turn`.
    #[serde(rename = "numTurns", alias = "num_turns", default)]
    num_turns: Option<u32>,
}

impl GrokResult {
    /// Whether this object is evidence that a turn actually ran.
    ///
    /// Every field is optional, so *any* JSON object deserializes into an
    /// all-default `GrokResult`. Treating that as "a turn ran but did not
    /// answer" infers the fact from successful parsing rather than from
    /// evidence, and the inference is load-bearing: `NoAnswer` returns `Ok`,
    /// which tells `review resume` the prompt cache was warmed and refreshes the
    /// staleness clock. An unrecognised pre-turn failure would therefore mark a
    /// cold session warm. Requiring one of grok's own result fields keeps that
    /// claim tied to something grok actually said.
    fn ran_a_turn(&self) -> bool {
        self.stop_reason.is_some() || self.session_id.is_some() || self.num_turns.is_some()
    }
}

/// How long `review` lets a grok shell command run: 20 minutes. Three grok
/// limits are set from it:
/// - the auto-background budget (`GROK_FOREGROUND_BLOCK_BUDGET_MS`, default
///   15s): a command holds the foreground for `min(model's timeout, this)` -
///   the model's timeout may be up to 10h, since production grok always sends
///   `max_timeout_secs` = 10h (`tools/config.rs` `PRODUCTION_MAX_TIMEOUT_SECS`),
///   and is 120s when the model sets none;
/// - the kill cap for a command that *cannot* be backgrounded
///   (`GROK_MAX_FOREGROUND_BLOCK_MS`, default 5 minutes) - not the normal path,
///   set so that path gets the same allowance rather than a shorter one;
/// - how long headless grok waits for a backgrounded command after the model's
///   turn ends before killing it (`--background-wait-timeout`, default 600s).
///
/// Long enough for a full gate; short enough that a wedged command cannot hold a
/// run for hours.
const GROK_COMMAND_WAIT_SECS: u64 = 20 * 60;

/// The one `stopReason` that means `text` is an answer. Anything else
/// (`cancelled`, `max_tokens`, ...) means `text` is whatever the model happened
/// to have said when it was cut off - interim commentary, not a result.
const GROK_OK_STOP: &str = "end_turn";

/// How many times `run_grok` will wake a model for a background result it did
/// not read. A model that reads the result and finishes needs one; the cap only
/// stops a model that keeps backgrounding work from looping forever, and
/// hitting it fails the run rather than passing it off as done.
const GROK_MAX_WAKES: u32 = 10;

/// Run grok, and keep waking it until it has read every background result.
///
/// Headless grok ends when the model ends its turn, even if a command the model
/// started is still running in the background: it waits for the command to
/// finish, queues a wake-up message for the model, and exits without running
/// that turn (`headless.rs` has no wake handling). Whether the model waited for
/// its own command first is its choice, and a wrong choice is silent - a run
/// that looks finished, with the build's result never read and no report. So
/// the guarantee is enforced here instead of hoped for: after every turn,
/// grok's own update stream is checked for results the model never saw
/// (`grok_home::unread_background_results`), and if there are any the session
/// is resumed, same sandbox and model, with a prompt that **contains** them
/// (`grok_home::wake_prompt`: each command's exit status and output tail, or a
/// plain "did NOT finish"). Carrying the result rather than pointing at it is
/// what makes delivery independent of the model: a prompt that only told it to
/// go and read the output could be ignored, and the next turn boundary would
/// then look clean. The run returns once a turn ends with nothing unread.
///
/// It fails rather than returns clean whenever the results cannot be vouched
/// for: grok's home or update log cannot be read or parsed (fail closed), or
/// `GROK_MAX_WAKES` is reached (each wake delivers what it names, so the cap
/// only stops a model that keeps starting new background work). It does not
/// wake after a stated failure - grok said why the turn ended, and resending
/// buys the same answer (`main::stated_failure`); that digest already fails the
/// run. A wake turn that fails to launch keeps the first turn's output and
/// session id rather than dropping both.
#[allow(clippy::too_many_arguments)]
async fn run_grok(
    session_id: &str,
    model: Option<&str>,
    effort: Option<&str>,
    sandbox: Option<&str>,
    trust: bool,
    env: Option<&std::collections::BTreeMap<String, String>>,
    prompt: &str,
    project_root: &Path,
    oneshot: bool,
    launched: Option<LaunchSignal>,
) -> Result<RunOutput> {
    let (mut first, first_turns, first_usage) = run_grok_turn(
        session_id,
        model,
        effort,
        sandbox,
        trust,
        env,
        prompt,
        project_root,
        oneshot,
        launched,
    )
    .await?;
    // A fresh run reports its new id (grok's echo, else the one we minted); a
    // resume was given it. So this is always known.
    let sid = first
        .session_id
        .clone()
        .unwrap_or_else(|| session_id.to_string());
    // Fail closed: without grok's home the results cannot be checked, and an
    // unchecked run must not pass as one whose results were seen.
    let grok = match crate::grok_home::Grok::resolve(env) {
        Ok(g) => g,
        Err(e) => {
            let reason = format!("could not verify grok's background results: {e:#}");
            eprintln!("grok: {reason}");
            first.digest = Some(wake_failure_digest(reason, first.digest.as_ref()));
            return Ok(first);
        }
    };
    let mut out = first;
    let mut cost = out.served.cost_usd;
    let mut turns = first_turns;
    let mut usage = first_usage;
    let mut wakes = 0;
    loop {
        // Fail closed: a run whose results cannot be checked is not a run
        // whose results were seen.
        let pending = match crate::grok_home::session_updates(&grok.home, &sid) {
            None => Err(format!("no update log for grok session {sid}")),
            Some(log) => crate::grok_home::unread_background_results(&log),
        };
        let pending = match pending {
            Ok(p) => p,
            Err(e) => {
                let reason = format!("could not verify grok's background results: {e}");
                eprintln!("grok: {reason}");
                out.digest = Some(wake_failure_digest(reason, out.digest.as_ref()));
                break;
            }
        };
        if pending.is_empty() {
            break;
        }
        // A stated failure (grok said why the turn ended) is not fixed by
        // waking: resending buys the same answer. The digest already fails it.
        if out
            .digest
            .as_ref()
            .is_some_and(|d| d.turn_error.is_some() && !d.captured)
        {
            break;
        }
        let names: Vec<String> = pending.iter().map(crate::grok_home::Unread::id).collect();
        if wakes == GROK_MAX_WAKES {
            let reason = format!(
                "grok still had unread background results after {GROK_MAX_WAKES} wake-ups: {}",
                names.join(", ")
            );
            eprintln!("grok: {reason} - giving up on session {sid}");
            out.digest = Some(wake_failure_digest(reason, out.digest.as_ref()));
            break;
        }
        wakes += 1;
        eprintln!(
            "grok: session {sid} ended its turn with unread background results ({}) - \
             resuming it with them (wake {wakes}/{GROK_MAX_WAKES})",
            names.join(", ")
        );
        // Same sandbox (grok refuses a different one on resume) and model; no
        // `--trust` (granted on the first turn if it was needed); no launch
        // signal (the lock was released when the first turn spawned).
        let prompt = crate::grok_home::wake_prompt(&pending);
        let (next, next_turns, next_usage) = match run_grok_turn(
            &sid,
            model,
            effort,
            sandbox,
            false,
            env,
            &prompt,
            project_root,
            false,
            None,
        )
        .await
        {
            Ok(next) => next,
            Err(e) => {
                // Keep what the run already has: `invoke` drops the session id
                // on `Err`, which would cost the sidecar row and any resume.
                let reason = format!("wake-up turn for session {sid} failed: {e:#}");
                eprintln!("grok: {reason}");
                out.digest = Some(wake_failure_digest(reason, out.digest.as_ref()));
                break;
            }
        };
        cost = match (cost, next.served.cost_usd) {
            (Some(a), Some(b)) => Some(a + b),
            (a, b) => a.or(b),
        };
        turns += next_turns;
        usage.input_tokens += next_usage.input_tokens;
        usage.cached_input_tokens += next_usage.cached_input_tokens;
        usage.output_tokens += next_usage.output_tokens;
        usage.reasoning_output_tokens += next_usage.reasoning_output_tokens;
        out = RunOutput {
            // The wake turn's report is the run's answer; the id stays the
            // first turn's (a resume does not report one).
            session_id: out.session_id,
            ..next
        };
    }
    out.served.cost_usd = cost;
    // Whatever digest the run ends with describes the whole run, not its last
    // grok process.
    if let Some(d) = out.digest.as_mut() {
        d.turns = turns;
        d.usage = usage;
    }
    if wakes > 0 {
        out.text = format!(
            "(grok woken {wakes} time(s) to read background results it had not waited for)\n{}",
            out.text
        );
    }
    Ok(out)
}

/// The digest for a grok run the wake loop gave up on: a failure with the
/// stated reason, keeping the turn and token counts the last turn reported
/// rather than zeroing them. Exit 1 and no signal are stated outright - the
/// failure is `review`'s verdict, not an observed process status.
fn wake_failure_digest(reason: String, last: Option<&Digest>) -> Digest {
    let (turns, usage) = last.map_or((0, Usage::default()), |d| (d.turns, d.usage.clone()));
    Digest {
        exit_code: Some(1),
        signal: None,
        turns,
        usage,
        ..grok_no_answer_digest(reason, &std::process::ExitStatus::default())
    }
}

/// One grok process: a single headless turn.
#[allow(clippy::too_many_arguments)]
async fn run_grok_turn(
    session_id: &str,
    model: Option<&str>,
    effort: Option<&str>,
    sandbox: Option<&str>,
    trust: bool,
    env: Option<&std::collections::BTreeMap<String, String>>,
    prompt: &str,
    project_root: &Path,
    oneshot: bool,
    launched: Option<LaunchSignal>,
) -> Result<(RunOutput, u32, Usage)> {
    // Grok takes no prompt on stdin - `-p` requires a value and `-p -` is read
    // literally as a one-character prompt. `--prompt-file` is the equivalent
    // escape from shell argument length limits, so the prompt goes via a temp
    // file instead of a pipe.
    let prompt_path = new_temp_file("grok-prompt")?;
    std::fs::write(&prompt_path, prompt)
        .with_context(|| format!("failed to write grok prompt file {prompt_path}"))?;
    let _prompt_file = RemoveOnDrop(prompt_path.clone());

    let oneshot_id = if oneshot {
        Some(crate::config::generate_uuid())
    } else {
        None
    };

    let mut args: Vec<String> = if let Some(ref id) = oneshot_id {
        vec!["--session-id".to_string(), id.clone()]
    } else {
        vec!["--resume".to_string(), session_id.to_string()]
    };
    args.push("--prompt-file".to_string());
    args.push(prompt_path.clone());
    args.push("--output-format".to_string());
    args.push("json".to_string());
    args.push("--permission-mode".to_string());
    args.push("dontAsk".to_string());
    // Unlike claude, grok has a real filesystem sandbox on the same axis as
    // codex's, so the profile field maps honestly and the default still holds:
    // a run with no profile cannot modify files.
    //
    // Except on a resume with no recorded level (a session whose sidecar row
    // predates sandbox recording): grok fixes a session's profile
    // for its lifetime and refuses an explicit `--sandbox` that differs from the
    // saved one (`resolve_startup_sandbox`: explicit + saved, different =>
    // `Conflict`), while no flag applies the saved one. Forcing `read-only` there
    // made every such write session unresumable. Omitting the flag defers to
    // what the session already had - no trust or root setup happens, because
    // there is nothing recorded to say the session writes.
    match (sandbox, oneshot) {
        (Some(level), _) => {
            args.push("--sandbox".to_string());
            args.push(level.to_string());
        }
        (None, true) => {
            args.push("--sandbox".to_string());
            args.push("read-only".to_string());
        }
        (None, false) => {}
    }
    // Grok's own (hidden, headless-honoured) grant: records the project in
    // `trusted_folders.toml` under grok's key and lock before the turn starts.
    // Without it every edit in an untrusted folder cancels the turn under
    // `dontAsk`. Passed only when the project has no decision yet, and
    // announced by `invoke`, which has also refused the cases where the grant
    // would be wrong (see `prepare_grok_write`).
    if trust {
        args.push("--trust".to_string());
    }
    if let Some(m) = model {
        args.push("-m".to_string());
        args.push(m.to_string());
    }
    if let Some(e) = effort {
        args.push("--reasoning-effort".to_string());
        args.push(e.to_string());
    }
    // When the model ends its turn with a command still in the background,
    // headless grok waits for it - by default only 600s, then kills it
    // (`reap_pending_background_tasks`). Raised to the same wait as a foreground
    // command, so a build the model walked away from still finishes and leaves
    // its output for the wake-up turn `run_grok` then gives the model. Hidden
    // but headless-honoured (`cli.rs` `background_wait_timeout_secs`).
    args.push("--background-wait-timeout".to_string());
    args.push(GROK_COMMAND_WAIT_SECS.to_string());

    let mut cmd = Command::new("grok");
    cmd.args(&args)
        .current_dir(project_root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Keep shell commands in the foreground until they finish. Grok moves any
    // command that blocks longer than 15s to the background, and headless `-p`
    // then only waits for it to *finish* before exiting - it never gives the
    // model another turn to read the result (`headless.rs`: exits once
    // `pending_bg` is empty). So every build over 15s ended a fixer's turn
    // without a report: the second wave lost two that way, and a live probe
    // watched `brokkr test` pass 13s after grok had already stopped listening.
    // The switch itself (`toolset.bash.auto_background_on_timeout`) is
    // config.toml-only and not on the `GROK_CONFIG` allowlist, and turning it
    // off would *kill* commands at a 5-minute cap instead, so the budgets are
    // raised per run through grok's own env overrides: the auto-background
    // budget (`GROK_FOREGROUND_BLOCK_BUDGET_MS`, terminal.rs) and the
    // non-backgroundable cap (`GROK_MAX_FOREGROUND_BLOCK_MS`, bash/mod.rs), both
    // to `GROK_COMMAND_WAIT_SECS`. A command then blocks until it exits or hits
    // the timeout the model asked for (120s when it asks for none). That keeps
    // most commands in the foreground but cannot guarantee it - the model
    // chooses the timeout - which is why `run_grok` also wakes the model for any
    // result it missed. Set before the profile env so a profile can restate
    // either.
    let wait_ms = (GROK_COMMAND_WAIT_SECS * 1000).to_string();
    cmd.env("GROK_FOREGROUND_BLOCK_BUDGET_MS", &wait_ms)
        .env("GROK_MAX_FOREGROUND_BLOCK_MS", &wait_ms);
    if let Some(vars) = env {
        cmd.envs(vars);
    }
    let grok = crate::grok_home::Grok::resolve(env).ok();
    // Re-checked immediately before spawn. `check_trust` in `invoke` and grok's
    // own grant are two separate reads of the store, and grok's grant overwrites
    // a `trusted = false` - so a "no" recorded between them (by the operator, or
    // another grok) would be overturned. This cannot close that race from
    // outside grok, which offers no "grant unless declined" operation; it
    // shrinks the window from the whole setup to the time grok takes to start.
    if trust && let Some(ref g) = grok {
        g.check_trust(project_root)?;
    }
    let child = cmd.spawn().context("failed to spawn grok")?;
    if let Some(signal) = launched {
        let _ = signal.send(());
    }

    let output = child
        .wait_with_output()
        .await
        .context("failed to wait for grok")?;
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    // A failed `--trust` grant is printed only to stderr, which is otherwise
    // read only when interpreting an error - so it would stay invisible on any
    // turn that happened not to edit, and the next run would be untrusted.
    if trust {
        for line in crate::grok_home::trust_failures(&stderr) {
            eprintln!("warning: grok --trust: {line}");
        }
    }
    // Only a *fresh* run reports a session id: it is new information, and it
    // must survive a turn that produced no answer so that turn can still be
    // resumed. A resume reports `None` because the caller passed the id in and
    // already records it (`main` logs the `review resume` id, not this field);
    // claude does the same. Preferring grok's echoed id over the UUID we
    // generated means a future grok that reassigns it cannot orphan the session.
    // Where to look for the event log if grok does not echo an id.
    let lookup_id = oneshot_id.clone().unwrap_or_else(|| session_id.to_string());
    let keep_id = |echoed: Option<String>| if oneshot { echoed.or(oneshot_id) } else { None };
    let served = grok_served(&stdout);
    let (turns, usage) = grok_usage(&stdout);
    let with_usage = |digest: Digest| Digest {
        turns,
        usage: usage.clone(),
        ..digest
    };

    let run = match interpret_grok_output(&stdout, &stderr)? {
        GrokOutcome::Answered(answer) => RunOutput {
            text: answer.text,
            session_id: keep_id(answer.session_id),
            served,
            // A real answer from a process that nonetheless exited non-zero is
            // still a real answer, but the status is worth surfacing rather
            // than discarding - it is the only sign that something went wrong
            // after the turn finished. `captured: true` keeps it out of
            // `died_without_answer`, so the run does not fail on it.
            digest: (!output.status.success()).then(|| {
                with_usage(Digest {
                    captured: true,
                    ..grok_no_answer_digest(
                        "grok exited non-zero after producing an answer".to_string(),
                        &output.status,
                    )
                })
            }),
        },
        GrokOutcome::NoAnswer {
            reason,
            text,
            session_id,
        } => {
            // The result object says only `cancelled`; grok's own event log
            // says why, and folder trust is the cause worth naming outright.
            let cause = grok.as_ref().and_then(|g| {
                let sid = session_id.as_deref().unwrap_or(&lookup_id);
                crate::grok_home::session_events(&g.home, sid)
                    .as_deref()
                    .and_then(crate::grok_home::cancel_cause)
            });
            let untrusted = grok.as_ref().and_then(|g| g.untrusted_hint(project_root));
            let reason = explain_grok_no_answer(reason, cause.as_deref(), untrusted.as_deref());
            RunOutput {
                text,
                session_id: keep_id(session_id),
                digest: Some(with_usage(grok_no_answer_digest(reason, &output.status))),
                served,
            }
        }
    };
    // Turn and token counts go back separately: a clean answer carries no
    // digest, and `run_grok` needs every turn's counts to describe the run.
    Ok((run, turns, usage))
}

/// Extend grok's bare stop reason with what its event log and folder trust
/// say. Pure so the wording is tested without a grok home.
fn explain_grok_no_answer(reason: String, cause: Option<&str>, untrusted: Option<&str>) -> String {
    let mut out = reason;
    if let Some(cause) = cause {
        out.push_str(&format!(" - {cause}"));
    }
    if let Some(untrusted) = untrusted {
        out.push_str(&format!(" - {untrusted}"));
    }
    out
}

/// Turn count and token usage from grok's result object, mapped onto the
/// codex-shaped `Usage`: codex's `input_tokens` includes the cached part, so
/// grok's separately-reported cache reads are added back in. Record-only, like
/// `grok_served`: a result without these fields records zeros.
fn grok_usage(stdout: &str) -> (u32, Usage) {
    #[derive(serde::Deserialize, Default)]
    #[serde(default)]
    struct RawUsage {
        input_tokens: u64,
        cache_read_input_tokens: u64,
        output_tokens: u64,
        reasoning_tokens: u64,
    }
    #[derive(serde::Deserialize)]
    struct Raw {
        #[serde(default)]
        usage: RawUsage,
        #[serde(rename = "numTurns", alias = "num_turns", default)]
        num_turns: u32,
    }
    let Ok(raw) = serde_json::from_str::<Raw>(stdout.trim()) else {
        return (0, Usage::default());
    };
    let u = raw.usage;
    (
        raw.num_turns,
        Usage {
            input_tokens: u.input_tokens + u.cache_read_input_tokens,
            cached_input_tokens: u.cache_read_input_tokens,
            output_tokens: u.output_tokens,
            reasoning_output_tokens: u.reasoning_tokens,
        },
    )
}

/// Pull what served the run out of grok's result object. Parsed separately
/// from `interpret_grok_output` because it is record-only: nothing about
/// whether the run answered depends on it, and a result lacking these fields
/// is still a valid result - it just records nothing here.
fn grok_served(stdout: &str) -> Served {
    #[derive(serde::Deserialize)]
    struct Raw {
        #[serde(rename = "modelUsage", default)]
        model_usage: serde_json::Map<String, serde_json::Value>,
        #[serde(default)]
        total_cost_usd: Option<f64>,
    }
    let Ok(raw) = serde_json::from_str::<Raw>(stdout.trim()) else {
        return Served::default();
    };
    let names: Vec<&str> = raw.model_usage.keys().map(String::as_str).collect();
    Served {
        model: (!names.is_empty()).then(|| names.join(",")),
        cost_usd: raw.total_cost_usd,
    }
}

/// A grok run that produced a real answer.
#[derive(Debug)]
struct GrokAnswer {
    text: String,
    session_id: Option<String>,
}

/// A grok turn that ran but produced no answer, expressed the same way codex's
/// deaths are: `Ok` carrying a digest, never `Err`.
///
/// The distinction that matters is between *never reaching the provider* (a
/// spawn failure, an unknown model, a session that does not exist - nothing ran,
/// no session exists to resume, no prompt cache was warmed) and *running a turn
/// that produced no answer*. Only the first is an `Err`. Collapsing the second
/// into `Err` too costs two things that are invisible until you need them:
/// `invoke` forces `session_id: None` on the error path, so the id grok already
/// minted is dropped and the cut-off turn cannot be resumed - exactly the turn
/// most worth resuming; and `review resume` refreshes its cold-cache clock on
/// `output.is_ok()`, so a resume that ran and warmed the cache would leave the
/// clock stale and get the *next* resume refused. Codex avoids both by returning
/// `Ok` with a death digest, and grok has no reason to differ.
///
/// `turn_error` is the honest field for it: grok stated why the turn ended, so
/// this is a stated failure in exactly the sense codex means, and it prints
/// first, persists flat, and never invites a retry. The remaining fields
/// describe a run with no captured answer, which is what happened; the
/// codex-specific forensics stay `None` because grok has no rollout.
fn grok_no_answer_digest(reason: String, status: &std::process::ExitStatus) -> Digest {
    Digest {
        // The *observed* status, not an assumed one. Hardcoding `1` here made
        // the digest assert something it had never looked at - and the exit
        // code is the field an operator reads to tell "grok declined" from
        // "grok was killed".
        exit_code: status.code(),
        signal: signal_name(status),
        captured: false,
        recovered_from_transcript: false,
        turns: 0,
        usage: Usage::default(),
        log_lines: Vec::new(),
        transcript: None,
        incident_path: None,
        terminated_by_review: None,
        quiet_secs: None,
        last_rollout_event: None,
        turn_error: Some(reason),
        interrupted: false,
    }
}

/// Decide whether a finished grok run produced an answer, from its streams
/// alone.
///
/// Two distinct failures have to stay distinguishable, because the operator's
/// next move differs. A run that never reached a turn at all (unknown model,
/// missing session, auth failure) prints a bare message and no result object -
/// nothing ran, so nothing was spent. A run that reached a turn and was cut off
/// emits a full result object whose `text` is whatever the model had said at
/// that moment: interim commentary, not a result. That second shape is exactly
/// the one that cost so much effort on codex, where it is indistinguishable
/// from a real answer without going to the rollout; grok labels it itself via
/// `stopReason`, so this takes it at face value rather than re-deriving it. The
/// commentary is carried in the error text - it is often the only clue about
/// what the run was doing - but it never becomes the response.
fn interpret_grok_output(stdout: &str, stderr: &str) -> Result<GrokOutcome> {
    let Ok(result) = serde_json::from_str::<GrokResult>(stdout.trim()) else {
        let detail = if stderr.trim().is_empty() {
            stdout.trim()
        } else {
            stderr.trim()
        };
        anyhow::bail!("grok produced no result object: {detail}");
    };

    // A typed error line is well-formed JSON, so it survives the parse above as
    // an empty result. Check it before `stopReason`, or the reason grok gave is
    // replaced by the generic "no stop reason" complaint.
    if result.kind.as_deref() == Some("error") {
        let detail = result.message.as_deref().unwrap_or(stdout.trim());
        anyhow::bail!("grok reported an error: {detail}");
    }

    // Nothing here identifies this as a result object, so it is not evidence a
    // turn ran - treat it like any other pre-turn failure.
    if !result.ran_a_turn() {
        let detail = if stderr.trim().is_empty() {
            stdout.trim()
        } else {
            stderr.trim()
        };
        anyhow::bail!("grok produced no recognisable result: {detail}");
    }

    let stop = result.stop_reason.as_deref().unwrap_or("missing");
    if stop != GROK_OK_STOP {
        return Ok(GrokOutcome::NoAnswer {
            reason: format!("grok ended the turn without an answer (stopReason: {stop})"),
            // Whatever the model had said when it was cut off. Kept because it
            // is all there is and often the only clue about what the run was
            // doing - the digest, not this text, is what says it is not an
            // answer.
            text: result.text,
            session_id: result.session_id,
        });
    }

    Ok(GrokOutcome::Answered(GrokAnswer {
        text: result.text,
        session_id: result.session_id,
    }))
}

/// What a finished grok run amounts to. An `Err` alongside these two means the
/// run never got as far as a turn at all.
#[derive(Debug)]
enum GrokOutcome {
    Answered(GrokAnswer),
    /// A turn ran and grok said why it produced nothing.
    NoAnswer {
        reason: String,
        text: String,
        session_id: Option<String>,
    },
}

/// Name of the codex permissions profile `review` synthesises for `read-only`.
///
/// Never read from the operator's config - `--ignore-user-config` drops that
/// file, and the profile is defined entirely by the `-c` overrides below - so
/// the name only has to avoid colliding with a built-in (those all start `:`).
const CODEX_READ_ONLY_PROFILE: &str = "review-read-only";

/// The codex arguments that select a sandbox level.
///
/// Every level but `read-only` is `--sandbox <level>`. `read-only` is not,
/// and the reason is worth the paragraph.
///
/// A `read-only` run needs sockets for the same reason a `workspace-write` one
/// does: the seccomp filter codex installs when network access is off denies
/// `bind`, `connect`, `listen`, `accept`, `getsockname`, `getsockopt`,
/// `setsockopt` and `sendto` outright, and permits `socket()` only for
/// AF_UNIX - so a unix socket can be created and then used for nothing. That
/// filter is not scoped to a sandbox level; it is installed whenever the
/// network policy is `Restricted`, which `read-only` always was.
///
/// There is no `sandbox_read_only.network_access` to turn it off.
/// `SandboxPolicy::ReadOnly` does carry a `network_access` field, but nothing
/// in the legacy `sandbox_mode` pipeline ever sets it: `derive_permission_
/// profile` maps `SandboxMode::ReadOnly` to `PermissionProfile::read_only()`,
/// whose network policy is hardcoded `Restricted`. Only the workspace-write
/// arm consults config, which is why the sibling override below exists and has
/// no read-only counterpart.
///
/// The one surface that *can* express "read-only filesystem, network enabled"
/// is codex's named-permissions pipeline - a `[permissions.<id>]` profile
/// extending the built-in `:read-only` with `network.enabled = true`. The
/// catch is that `resolve_permission_config_syntax` returns `Legacy` the
/// instant a `sandbox_mode` override is present, and `Legacy` makes
/// `[permissions]` profiles inert. So selecting the profile means **not**
/// passing `--sandbox` at all; the two mechanisms cannot be combined.
///
/// Giving up `--sandbox` on the path that matters most deserved evidence
/// rather than a reading of the source, so `scripts/readonly_network_check.py`
/// measures three configurations against one witness program. Verified on
/// codex-cli 0.151.0:
///
/// ```text
///                      --sandbox read-only   profile, restricted   profile, enabled
///   write_cwd          EROFS                 EROFS                 EROFS
///   read_etc_hostname  ok                    ok                    ok
///   unix_connect       EPERM                 EPERM                 ok
///   unix_sockopt       EPERM                 EPERM                 ok
///   inet_socket_create EPERM                 EPERM                 ok
///   inet_bind_listen   EPERM                 EPERM                 ok
///   tcp_connect        EPERM                 EPERM                 ok
/// ```
///
/// The middle column is the load-bearing one. It is the same profile with
/// network left restricted, and it reproduces `--sandbox read-only` exactly -
/// which is what establishes that dropping the flag does not widen the
/// filesystem, separately from the network change. `write_cwd` stays `EROFS`
/// in all three: the guarantee `.review.toml` advertises for `read-only` is
/// intact, and only the socket lines move.
///
/// Note the witnesses are `connect()` and socket options, not `bind()` on a
/// unix path. Under `read-only` nothing is writable, so binding an AF_UNIX
/// socket cannot succeed on filesystem grounds however the filter is set; what
/// a read-only run gains is the ability to *reach* something already
/// listening, plus AF_INET egress.
///
/// Enabling network is as blunt here as it is for `workspace-write` - the
/// filter is all-or-nothing and codex exposes no AF_UNIX-only knob on Linux
/// (`NetworkToml` has `unix_sockets` and `allow_local_binding`, but only
/// `seatbelt.rs` reads them; the Linux path is `landlock.rs`, where network
/// `Enabled` with no proxy means no filter is installed at all). Deliberately
/// *not* configuring a proxy also keeps us out of `NetworkSeccompMode::
/// ProxyRouted`, which would be worse than the status quo: it denies AF_UNIX
/// `socket()` entirely.
///
/// An older codex without the permissions pipeline fails soft rather than
/// silently wide: it ignores `default_permissions` and falls through to
/// `SandboxMode::default()`, which is `ReadOnly`.
fn codex_sandbox_args(level: &str) -> Vec<String> {
    if level != "read-only" {
        return vec!["--sandbox".to_string(), level.to_string()];
    }
    vec![
        "-c".to_string(),
        format!(r#"permissions.{CODEX_READ_ONLY_PROFILE}.extends=":read-only""#),
        "-c".to_string(),
        format!("permissions.{CODEX_READ_ONLY_PROFILE}.network.enabled=true"),
        "-c".to_string(),
        format!(r#"default_permissions="{CODEX_READ_ONLY_PROFILE}""#),
    ]
}

#[allow(clippy::too_many_arguments)]
async fn run_codex(
    session_id: &str,
    model: Option<&str>,
    effort: Option<&str>,
    sandbox: Option<&str>,
    granted_roots: &[crate::writable_roots::GrantedRoot],
    env: Option<&std::collections::BTreeMap<String, String>>,
    config: &[String],
    prompt: &str,
    project_root: &Path,
    oneshot: bool,
    launched: Option<LaunchSignal>,
    runtime: &CodexRuntime,
) -> Result<RunOutput> {
    let last_msg_path = new_output_file()?;

    // Exec-level options shared by both paths.
    let mut args: Vec<String> = vec![
        "exec".to_string(),
        // `review` is routinely launched from a directory that is not itself a
        // git repo (an umbrella dir whose repos live in subdirectories). Codex
        // refuses to start there - "Not inside a trusted directory and
        // --skip-git-repo-check was not specified" - so pass it always. The
        // filesystem guarantee we rely on is `--sandbox`, which is unaffected:
        // this flag only waives codex's own "am I in a repo?" precondition.
        "--skip-git-repo-check".to_string(),
        // `--sandbox` alone stopped being a filesystem guarantee in codex
        // 0.150.x. `codex exec` sets `approval_policy = "never"` for headless
        // runs, which is what made a read-only sandbox absolute: the model's
        // `request_permissions` escalation is auto-denied under `never`. But
        // `build_exec_config` now *drops* that headless default whenever the
        // resolved `approvals_reviewer` is `auto_review` (settable globally in
        // `~/.codex/config.toml`), falling back to `on-request`. Escalations
        // then go to codex's own auto-approving guardian, which grants them,
        // and additional write permissions are layered onto the read-only
        // profile for the turn. Verified on 0.150.1: a bare
        // `codex exec --sandbox read-only` wrote a file; with this override the
        // same prompt is rejected ("writing is blocked by read-only sandbox").
        // Pinning the policy here restores the invariant regardless of the
        // operator's global codex config. Passed before profile `config`
        // overrides so a profile can still opt back in deliberately.
        "-c".to_string(),
        "approval_policy=\"never\"".to_string(),
        // The second, independent hole in the same guarantee - and the one that
        // actually fires in the field. Codex's execpolicy `.rules` files are not
        // only an approval allowlist: a `prefix_rule(..., decision="allow")`
        // whose pattern matches *every* parsed segment of a command makes
        // `exec_policy.rs` return `Skip { bypass_sandbox: true }`, which the
        // orchestrator resolves to `SandboxType::None`. On Linux the sandbox is
        // purely an argv wrapper, so that is not a weakened sandbox - it is no
        // sandbox at all. `approval_policy="never"` does not gate this branch,
        // and `read-only` is exposed exactly as much as `workspace-write`
        // (`unsandboxed_execution_allowed` only tests for deny-*read* entries,
        // which neither level has). Verified on 0.151.0 against an operator
        // `~/.codex/rules/default.rules` carrying `["brokkr","status"]` and
        // `["ln","-s"]`: under `--sandbox read-only`, `ln -s /etc/hostname x`
        // created the link, and `brokkr status` wrote a row into
        // `~/.local/share/brokkr/history.db` - both outside every writable root.
        // Prefixing one non-allowlisted segment (`echo probe; ...`) made the
        // identical command sandboxed, which is why this looked like flaky,
        // per-run behaviour for months: enforcement tracked the shape of the
        // command the model happened to type. `--ignore-rules` drops the user
        // and project rule layers for this invocation only, leaving the
        // operator's interactive codex untouched. An older codex without the
        // flag fails loudly on an unknown argument rather than silently running
        // unsandboxed, which is the right way round.
        "--ignore-rules".to_string(),
        // And the general form of the same lesson: a `review` run must be a
        // function of `.review.toml` plus argv, never of a file the operator
        // edited for unrelated reasons. Twice now the guarantee a profile
        // advertised was silently voided by ambient global codex config - once
        // by `approvals_reviewer = "auto_review"` rewriting the approval
        // policy, once by the execpolicy rules above. Pinning each hole as it
        // is found leaves the next one live, so drop `$CODEX_HOME/config.toml`
        // wholesale. Auth is unaffected: it resolves from `CODEX_HOME`
        // independently of this flag (verified - a run with it authenticates
        // and completes normally). Two consequences, both deliberate: a run
        // whose profile sets no `model` now takes codex's built-in default
        // rather than the operator's configured one (identical on this host
        // today, but no longer a silent coupling), and a profile whose `config`
        // overrides name a custom `model_provider` defined in the operator's
        // config.toml must now define it in the profile too.
        "--ignore-user-config".to_string(),
        // Rollout reasoning items carry `encrypted_content` - sealed server-side
        // and undecryptable here - and on the default `auto` summary setting
        // their `summary` arrays come back empty, so a rollout records *what* a
        // run did and never *why*. That cost real time: reconstructing why an
        // agent relocated a build meant inferring intent from the command
        // sequence alone, because the transcript held no prose at any point.
        // `detailed` fills the summaries, which `src/transcript.rs` forensics
        // and `review sessions <id>` can then surface. Passed before profile
        // `config` overrides, so a profile can still set `none` where the
        // summaries are not worth their tokens.
        "-c".to_string(),
        "model_reasoning_summary=\"detailed\"".to_string(),
    ];
    args.extend(codex_sandbox_args(sandbox.unwrap_or("read-only")));
    if let Some(m) = model {
        args.push("-m".to_string());
        args.push(m.to_string());
    }
    if let Some(e) = effort {
        args.push("-c".to_string());
        args.push(format!("model_reasoning_effort=\"{e}\""));
    }
    // Writable roots the host needs for a build, derived by the caller (see
    // `invoke`, and `src/writable_roots.rs` for why they are derived at all).
    // Placed before profile `config` so a profile can restate
    // `sandbox_workspace_write.writable_roots` and win.
    if let Some(override_arg) = crate::writable_roots::config_override(granted_roots) {
        args.push("-c".to_string());
        args.push(override_arg);
    }
    // A `workspace-write` run gets network access, because without it codex
    // cannot use **unix domain sockets** - which has nothing to do with the
    // network and everything to do with how the filter is written.
    //
    // The seccomp filter codex installs when network access is off
    // (`linux-sandbox/src/landlock.rs`) unconditionally denies `bind`,
    // `connect`, `listen` and `accept`, returning `EPERM`. It carves out
    // AF_UNIX for `socket()` and `socketpair()` only, so an AF_UNIX socket can
    // be created and then used for nothing that needs a pathname. A daemon /
    // worker test suite that binds a socket under its own target directory
    // fails with `Operation not permitted` on a path that is demonstrably
    // writable - verified by binding in the *cwd*, which codex grants
    // unconditionally, and getting the same errno. So no `writable_roots`
    // entry can fix it; there is no path for which it works.
    //
    // Enabling network access is a blunt instrument - the filter is
    // all-or-nothing, so this restores AF_INET egress too, and codex offers no
    // AF_UNIX-only knob. It is nevertheless not a widening of what these runs
    // have actually had: until `--ignore-rules` landed in a commit earlier that
    // day, an operator execpolicy allowlist made build commands bypass the
    // sandbox wrapper entirely, so no seccomp filter was installed and every
    // `workspace-write` run in this tool's history ran with full network
    // access. This makes that explicit and, unlike the bypass, it applies to
    // every command rather than the ones whose argv happened to match a rule.
    //
    // Scoped to `workspace-write` because this key only exists for that level.
    // `read-only` needs the same grant - the filter denies `connect` there too -
    // and gets it from the permissions profile in `codex_sandbox_args`, there
    // being no `sandbox_read_only.network_access`. Passed before profile
    // `config`, so a profile can restate the key and win.
    if sandbox == Some("workspace-write") {
        args.push("-c".to_string());
        args.push("sandbox_workspace_write.network_access=true".to_string());
    }

    // Profile `config` overrides, each a verbatim `-c key=value`. Placed after
    // effort so a profile could even override reasoning effort if it wanted.
    for c in config {
        args.push("-c".to_string());
        args.push(c.clone());
    }

    // A fresh run captures the new session id from the stream; the `resume`
    // subcommand carries the id we already know. Both stream `--json` and take
    // `-o` (placed at the subcommand level for resume, exec level for fresh).
    let known_session_id = if oneshot {
        args.push("--json".to_string());
        args.push("-o".to_string());
        args.push(last_msg_path.clone());
        None
    } else {
        args.push("resume".to_string());
        args.push(session_id.to_string());
        args.push("--json".to_string());
        args.push("-o".to_string());
        args.push(last_msg_path.clone());
        Some(session_id.to_string())
    };

    run_codex_json(
        args,
        &last_msg_path,
        known_session_id,
        env,
        prompt,
        project_root,
        launched,
        runtime,
    )
    .await
}

/// Shared codex runner. `args` must include `--json` and `-o <last_msg_path>`.
/// Pipes the prompt on stdin and distills a `Digest` (usage, turns, captured,
/// exit/signal, log lines, transcript) from the NDJSON stream plus the `-o`
/// backstop. `known_session_id` is `Some` on resume (we already know it) and
/// `None` on a fresh run (parsed from `thread.started`). Never bails on a
/// non-zero exit: a halted/errored run still reports what it produced.
// Every argument here is an independent axis of one invocation (argv, the `-o`
// path, the known session id, env, prompt, cwd, the launch handshake, the
// injectable runtime). Bundling them into a struct would add a type whose only
// purpose is to be destructured immediately, so the lint is waived rather than
// worked around.
#[allow(clippy::too_many_arguments)]
async fn run_codex_json(
    args: Vec<String>,
    last_msg_path: &str,
    known_session_id: Option<String>,
    env: Option<&std::collections::BTreeMap<String, String>>,
    prompt: &str,
    project_root: &Path,
    launched: Option<LaunchSignal>,
    runtime: &CodexRuntime,
) -> Result<RunOutput> {
    let mut cmd = Command::new(&runtime.binary);
    cmd.args(&args)
        .current_dir(project_root)
        // Make a codex panic legible: without this it exits 1 with no trace,
        // which is exactly the "no recorded reason" we kept hitting. Set before
        // the profile env so an explicit override still wins.
        .env("RUST_BACKTRACE", "1")
        // Put codex in its own process group so we can signal it *and every
        // process it spawned* (exec-server, unified-exec background cells, MCP
        // servers) as a unit, without the signal reaching `review` itself. This
        // is what makes the watchdog's kill effective: a wedged codex typically
        // still has live children, and killing the pid alone would orphan them.
        //
        // The cost is that a terminal SIGINT no longer reaches codex
        // implicitly, since it is no longer in the foreground process group -
        // so `run_codex_json` installs its own handler and forwards it. Without
        // that forwarding this change would *cause* the orphaning it exists to
        // prevent.
        .process_group(0)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(vars) = env {
        cmd.envs(vars);
    }
    // Size of the rollout transcript *before* this run appends to it. A resume
    // writes into the same file as every earlier turn, so the watchdog needs to
    // know where this run's events begin - otherwise a previous turn's
    // `final_answer` would look like proof that this run had finished, and a
    // quiet mid-turn wedge would be killed as a "stranded completion".
    //
    // A byte offset rather than a timestamp on purpose: rollout timestamps carry
    // milliseconds but our own clock string is second-resolution, so a turn that
    // completed in the same second as this run's launch would slip through a
    // time-based filter. The offset has no such race - it is exact. Zero for a
    // fresh run, whose rollout does not exist yet.
    // `None` means "no trustworthy baseline", which disables both the watchdog
    // and transcript recovery for this run. It is emphatically not the same as
    // `Some(0)`: falling back to zero for a *resume* whose rollout could not be
    // read would scope the scan to the entire session history, and an old
    // `final_answer` from a previous turn could then authorise killing a genuine
    // mid-turn wedge, or be recovered as this run's answer.
    let codex_home_for_baseline = env.and_then(|e| e.get("CODEX_HOME")).map(String::as_str);
    let rollout_baseline: Option<u64> = match known_session_id.as_deref() {
        // Fresh run: a new session id, so its rollout does not exist yet and
        // every byte in it will be ours. Zero is exact here, not a fallback.
        None => Some(0),
        // Resume: the rollout already holds earlier turns, so we must know
        // exactly where they end. If we cannot stat it, we do not guess.
        Some(sid) => crate::transcript::find_transcript_path(sid, codex_home_for_baseline)
            .and_then(|p| std::fs::metadata(p).ok())
            .map(|m| m.len()),
    };
    if rollout_baseline.is_none() {
        eprintln!(
            "warning: could not read the rollout for session {} before launch; \
             completion watchdog and transcript recovery are disabled for this run",
            known_session_id.as_deref().unwrap_or("?")
        );
    }

    // A request left over from an earlier run of this session (its `review`
    // killed before consuming it) is not aimed at this one. A fresh run needs
    // no such check: its session id is new.
    if let Some(sid) = known_session_id.as_deref() {
        let _ = crate::inflight::take_interrupt_request(sid, runtime.data_root.as_deref());
    }

    let start = std::time::Instant::now();
    // (Gating transcript recovery to this run's turn is done by byte offset -
    // `rollout_baseline` above - not by a wall-clock stamp, which was too coarse
    // to separate a resume from an answer written in the same second.)
    let mut child = cmd.spawn().context("failed to spawn codex")?;
    // Launch has happened: the caller may release the global lock. Dropping the
    // sender instead (on the `?` above) tells it the same thing.
    if let Some(signal) = launched {
        let _ = signal.send(());
    }
    // Clean up the `-o` temp file on every exit path - spawn succeeded so it
    // exists, and an error before the read below would otherwise leak it.
    let _output_cleanup = RemoveOnDrop(last_msg_path.to_string());

    let stdin = child.stdin.take().context("failed to open codex stdin")?;
    let stdout_pipe = child.stdout.take().context("failed to open codex stdout")?;
    let stderr_pipe = child.stderr.take().context("failed to open codex stderr")?;

    // Publishes the session id as soon as it is known: immediately on a resume,
    // or when `thread.started` arrives on stdout for a fresh run. The watchdog
    // needs it to find the rollout transcript.
    let (sid_tx, sid_rx) = tokio::sync::watch::channel(known_session_id.clone());
    let scanning_for_session_id = known_session_id.is_none();
    // Kept for after the wait, so the id survives a stdout reader that never
    // reached EOF (see where `session_id` is resolved).
    let sid_rx_final = sid_rx.clone();
    // The group leader: what the watchdog and signal supervisor kill, and what
    // the in-flight marker records for `review interrupt` to signal.
    let child_pid = child.id();
    // Flips on codex's first stdout. Until then the pid may be a node wrapper
    // that has not yet installed the handler forwarding SIGINT to the native
    // binary, so interrupting it could kill the wrapper and orphan codex -
    // still holding the session's writer lock, which would make the resume
    // `review interrupt` hands back fail. Output from codex proves the wrapper
    // got that far, so the marker only offers the pid from then on.
    let (first_output_tx, mut first_output_rx) = tokio::sync::watch::channel(false);

    // Mark the run as in flight so `review sessions` can report "turn in flight
    // since <time>" instead of showing the *previous* turn's response while this
    // one runs - the exact ambiguity that made a 10-hour hang look identical to
    // an idle session.
    //
    // This lives in its own task because a fresh run does not learn its session
    // id until `thread.started` arrives mid-stream. The task parks forever
    // holding the marker guard; aborting it after the run drops the guard, which
    // deletes the marker. That covers every normal exit path, and `read_live`
    // discards markers whose owning pid is gone for the ones it cannot.
    let marker_task = tokio::spawn({
        let mut rx = sid_rx.clone();
        let project = project_root.to_string_lossy().into_owned();
        let data_root = runtime.data_root.clone();
        async move {
            let sid = loop {
                if let Some(sid) = rx.borrow_and_update().clone() {
                    break sid;
                }
                if rx.changed().await.is_err() {
                    return;
                }
            };
            let mut guard = crate::inflight::mark(&sid, "codex", &project, data_root.as_deref());
            if first_output_rx.wait_for(|seen| *seen).await.is_ok() {
                guard.record_child_pid(child_pid);
            }
            // Hold the marker until this task is aborted.
            std::future::pending::<()>().await;
        }
    });

    // Bound in-memory buffering: a runaway/noisy provider could otherwise OOM
    // `review` long before the 1 MiB forensic tail is applied. Buffer up to a
    // generous cap and keep draining past it so codex never blocks on a full
    // pipe. Real reviews are far under the cap.
    //
    // These are spawned rather than joined inline because the wait below has to
    // be interruptible, and because pipe EOF is *not* a reliable completion
    // signal - see the reaping comment further down.
    let stdout_shared = new_shared_buf();
    let stderr_shared = new_shared_buf();
    let stdout_task = tokio::spawn(read_capped_stdout(
        stdout_pipe,
        STDOUT_CAPTURE_CAP,
        std::sync::Arc::clone(&stdout_shared),
        sid_tx,
        first_output_tx,
        scanning_for_session_id,
    ));
    let stderr_task = tokio::spawn(read_capped(
        stderr_pipe,
        STDERR_CAPTURE_CAP,
        std::sync::Arc::clone(&stderr_shared),
    ));
    let write_task = tokio::spawn(write_stdin(stdin, prompt.as_bytes().to_vec()));

    // Make this run's process group visible to the signal supervisor, so an
    // operator killing `review` takes codex with it instead of orphaning it.
    if let Some(pid) = child_pid {
        register_group(pid);
    }
    // Records that *we* ended the run, and why. See `Digest::terminated_by_review`.
    let mut terminated_by_review: Option<String> = None;
    // Forensics from the watchdog verdict, carried onto the digest and into the
    // incident bundle.
    let mut quiet_secs: Option<u64> = None;
    let mut last_rollout_event: Option<String> = None;

    // Wait for codex, but stay interruptible.
    //
    // Before this loop the code was a plain `join!` on the child plus both
    // pipes, which had two failure modes, both observed:
    //   - codex finishes its turn (final answer on disk, `task_complete`
    //     recorded) and then never exits. `review` blocked for 10+ hours with
    //     the answer sitting in the rollout the whole time, and produced no
    //     digest, no incident bundle and no sidecar row, because all of that is
    //     computed below this point.
    //   - the operator kills `review`, and codex - a separate process - keeps
    //     running detached, still modifying the tree.
    // The watchdog arm below fixes the first; the second is handled process-wide
    // by `install_signal_supervisor`.
    let status = {
        let watchdog = crate::watchdog::wait_for_stranded_completion(
            sid_rx,
            env.and_then(|e| e.get("CODEX_HOME")).cloned(),
            rollout_baseline,
            runtime.timings,
        );
        tokio::pin!(watchdog);
        loop {
            tokio::select! {
                // Normal path: codex exited on its own.
                status = child.wait() => break status,

                // codex has demonstrably produced its answer but will not exit.
                // Kill the group; the loop then reaps it on the next iteration
                // and the recovered `final_answer` is reported as usual. The
                // guard disables this arm once it has fired, so the completed
                // future is never polled again.
                verdict = &mut watchdog, if terminated_by_review.is_none() => {
                    eprintln!("watchdog: {}", verdict.reason);
                    terminated_by_review = Some(match verdict.kind {
                        crate::watchdog::Kind::Stranded => {
                            "watchdog: stranded completion".to_string()
                        }
                        // Named a stall *timeout* on purpose: it is one, and
                        // saying so keeps a future codex cadence change
                        // diagnosable instead of mysterious.
                        crate::watchdog::Kind::Stalled => {
                            format!("watchdog: stall timeout ({}s silent)", verdict.quiet_secs)
                        }
                    });
                    quiet_secs = Some(verdict.quiet_secs);
                    last_rollout_event = verdict.last_event.clone();
                    if let Some(pid) = child_pid {
                        terminate_group(pid, runtime.sigkill_escalation);
                    }
                }
            }
        }
    };
    let watched_session_id = sid_rx_final.borrow().clone();
    // Consumed now that codex is reaped, whatever became of it: a request left
    // behind would mark the session's *next* run as interrupted. Before the
    // marker goes, because `read_live` discards a request with no marker beside
    // it. Whether it *counts* waits until we know if an answer was produced.
    let interrupt_requested = watched_session_id.as_deref().is_some_and(|sid| {
        crate::inflight::take_interrupt_request(sid, runtime.data_root.as_deref())
    });
    // Run is over: drop the in-flight marker (aborting the task drops its guard)
    // and stop advertising the group to the signal supervisor. Both happen
    // before the `?` below so a failed wait cannot leak either.
    marker_task.abort();
    if let Some(pid) = child_pid {
        unregister_group(pid);
    }
    let status = status.context("failed to wait for codex")?;

    // The child is reaped, so the run is over regardless of what the pipes do.
    //
    // Waiting on pipe EOF here would reintroduce a hang of its own: EOF needs
    // *every* holder of the write end to close it, which includes any process
    // codex spawned that inherited the descriptors. Reaping is the authoritative
    // signal; give the readers a moment to drain what is already buffered, then
    // take what they have. (Investigation of codex 0.147.0 found no production
    // `Stdio::inherit` on the exec path - `codex-rs/core/src/spawn.rs` gives
    // children null/piped stdio - so this is defence in depth rather than a
    // known live path.)
    let stdout_buf =
        collect_reader(stdout_task, &stdout_shared, runtime.drain_grace, "stdout").await;
    let stderr_buf =
        collect_reader(stderr_task, &stderr_shared, runtime.drain_grace, "stderr").await;
    let write_res = match write_task.await {
        Ok(r) => r,
        Err(e) => Err(anyhow::anyhow!("stdin write task panicked: {e}")),
    };
    let duration_ms = start.elapsed().as_millis();
    // A failed prompt write (e.g. broken pipe because codex exited first) is a
    // forensic signal, not a fatal error - the digest still reports what ran.
    let stdin_write_error = write_res.err().map(|e| e.to_string());

    // Parse the NDJSON stream: session id, streamed final message, turn count,
    // summed usage, and any non-JSON log lines. We do NOT bail on a non-zero
    // exit here - the whole point of the digest is that a halted or errored run
    // still yields whatever it produced, with the exit status recorded.
    let stdout = String::from_utf8_lossy(&stdout_buf);
    let mut parsed_session_id: Option<String> = None;
    let mut stream_message: Option<String> = None;
    let mut turns: u32 = 0;
    let mut usage = Usage::default();
    let mut log_lines: Vec<String> = Vec::new();
    let mut turn_error: Option<String> = None;
    for line in stdout.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let val: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => {
                // Plain-text log line (codex ERROR/WARN, apply_patch dump). The
                // harness can halt NDJSON emission here, so keep it visible.
                log_lines.push(line.to_string());
                continue;
            }
        };
        match val.get("type").and_then(|t| t.as_str()) {
            Some("thread.started") => {
                parsed_session_id = val
                    .get("thread_id")
                    .and_then(|t| t.as_str())
                    .map(String::from);
            }
            Some("item.completed") => {
                let item = val.get("item");
                // Only agent_message items carry the reportable final text;
                // reasoning / command items also arrive as item.completed.
                if item.and_then(|i| i.get("type")).and_then(|t| t.as_str())
                    == Some("agent_message")
                {
                    stream_message = item
                        .and_then(|i| i.get("text"))
                        .and_then(|t| t.as_str())
                        .map(String::from);
                }
            }
            // Codex stating why the turn ended: an upstream refusal (content
            // flagged, rate limit, auth) or a harness error. Dropping these was
            // what made a policy refusal - fully explained, at the end of the
            // stream - surface as an unexplained death with a bare interim note,
            // then get pointlessly auto-resumed into the identical refusal.
            // `turn.failed` nests the message; the bare `error` event carries it
            // at the top level. They arrive as a pair saying the same thing, so
            // last-writer-wins is fine; a genuine second failure is more
            // informative than the first anyway.
            Some("error") => {
                if let Some(m) = val.get("message").and_then(|m| m.as_str()) {
                    turn_error = Some(m.to_string());
                }
            }
            Some("turn.failed") => {
                if let Some(m) = val
                    .get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(|m| m.as_str())
                {
                    turn_error = Some(m.to_string());
                }
            }
            Some("turn.completed") => {
                turns += 1;
                if let Some(u) = val.get("usage") {
                    let get = |k: &str| u.get(k).and_then(serde_json::Value::as_u64).unwrap_or(0);
                    usage.input_tokens += get("input_tokens");
                    usage.cached_input_tokens += get("cached_input_tokens");
                    usage.output_tokens += get("output_tokens");
                    usage.reasoning_output_tokens += get("reasoning_output_tokens");
                }
            }
            _ => {}
        }
    }

    // The -o file is written via a path separate from the NDJSON stream, only
    // on a real final message. A non-empty file is thus the authoritative
    // completed signal - it survives a frozen stream. Its absence means the run
    // ended without a final report (crashed, killed, or yielded out).
    let final_from_file = match tokio::fs::read_to_string(&last_msg_path).await {
        Ok(s) => {
            let s = s.trim().to_string();
            if s.is_empty() { None } else { Some(s) }
        }
        Err(_) => None,
    };
    // (the `-o` temp file is removed by `_output_cleanup` on scope exit)

    let captured = final_from_file.is_some();

    // On resume we already know the session id; on a fresh run it comes from
    // the stream. `watched_session_id` is the same id as published live by the
    // stdout reader, and is the fallback for when that reader was aborted before
    // EOF: `parsed_session_id` comes from re-parsing `stdout_buf`, which a
    // truncated capture can leave without the `thread.started` line. Losing the
    // id would cost us the transcript, the recovery path and the sidecar row.
    let session_id = known_session_id
        .or(parsed_session_id)
        .or(watched_session_id);

    let exit_code = status.code();
    let signal = signal_name(&status);
    // Only pay for transcript forensics when the run looks wrong; a clean
    // captured run needs no post-mortem. Look under the run's effective
    // CODEX_HOME (a profile env override) so a custom codex home is searched.
    // Read before choosing the final message: codex routinely finishes the turn
    // on disk (task_complete) yet exits non-zero and truncates both the `--json`
    // stream and the `-o` file, so the rollout is the authoritative record.
    let suspicious = !captured || exit_code != Some(0) || signal.is_some();
    let codex_home = env.and_then(|e| e.get("CODEX_HOME")).map(String::as_str);
    // An untrustworthy baseline must not degrade to "scan everything": scope to
    // a point past the end of the file so nothing is attributed to this run.
    // `u64::MAX` is exact for that purpose - `slice_from_offset` yields an empty
    // slice for any offset beyond the file.
    let recovery_scope = Some(rollout_baseline.unwrap_or(u64::MAX));
    let transcript = if suspicious {
        // Scoped to the bytes this run appended (see `rollout_baseline`), so a
        // previous turn's `final_answer` can never be recovered as ours. When
        // the baseline is untrustworthy this yields nothing rather than the
        // whole history, which is the safe direction: no recovery beats a wrong
        // answer that also suppresses auto-resume.
        session_id
            .as_deref()
            .and_then(|sid| crate::transcript::summarize_session(sid, codex_home, recovery_scope))
    } else {
        None
    };

    // Final-message priority:
    //   1. the `-o` backstop - authoritative when codex managed to write it.
    //   2. the transcript's recovered `final_answer` - the real report salvaged
    //      when the stream/`-o` were truncated but the rollout reached the end.
    //   3. the last streamed message - interim commentary, a last resort.
    let recovered = transcript.as_ref().and_then(|t| t.final_answer.clone());
    let recovered_from_transcript = final_from_file.is_none() && recovered.is_some();
    let final_message = final_from_file.or(recovered).or(stream_message);

    // A request only makes this an interrupt if the run has no answer. codex
    // can finish on its own between `review interrupt` writing the request and
    // its signal landing, or answer before the signal is read; calling either
    // interrupted would report a completed run as cut short.
    let interrupted = interrupt_requested && !captured && !recovered_from_transcript;
    if interrupted && terminated_by_review.is_none() {
        terminated_by_review = Some("operator interrupt (`review interrupt`)".to_string());
    }

    // Dump a full forensic bundle for the same suspicious runs we post-mortem -
    // stderr (with backtraces), the raw stream, the transcript tail, the exact
    // argv, and codex's version - so the next death is over-instrumented.
    // An operator interrupt is not an incident: the missing answer is explained.
    let incident_path = if suspicious && !interrupted {
        crate::incident::write_bundle(&crate::incident::Incident {
            provider: "codex",
            // The binary actually executed, which is not always "codex": a
            // stub overrides it. `command` in meta.json has to replay the
            // process that really ran.
            binary: &runtime.binary,
            session_id: session_id.as_deref(),
            argv: &args,
            prompt,
            cwd: project_root,
            env,
            exit_code,
            signal: signal.as_deref(),
            stdin_write_error: stdin_write_error.as_deref(),
            stdout: &stdout_buf,
            stderr: &stderr_buf,
            transcript_path: transcript.as_ref().map(|t| t.path.as_str()),
            duration_ms,
            data_root: runtime.data_root.as_deref(),
            captured,
            recovered_from_transcript,
            turns,
            terminated_by_review: terminated_by_review.as_deref(),
            quiet_secs,
            last_rollout_event: last_rollout_event.as_deref(),
            turn_error: turn_error.as_deref(),
        })
        .map(|p| p.to_string_lossy().into_owned())
    } else {
        None
    };

    let digest = Digest {
        exit_code,
        signal,
        captured,
        recovered_from_transcript,
        turns,
        usage,
        log_lines,
        transcript,
        incident_path,
        terminated_by_review,
        quiet_secs,
        last_rollout_event,
        turn_error,
        interrupted,
    };

    // Even when no message came back (a hard freeze before any agent_message,
    // the very case forensics exist to explain), return the digest rather than a
    // bare error - otherwise `invoke` would drop the session id and transcript.
    let text = final_message.unwrap_or_else(|| {
        // No final answer at all - not even an interim message. task_complete
        // is not a success signal (it fires on aborted turns with a null final
        // answer), so a non-zero exit/signal here means the run died mid-turn.
        // A stated reason outranks our inference: when codex told us why the turn
        // ended, say that instead of guessing from the exit status.
        let stated = digest
            .turn_error
            .as_ref()
            .map(|m| format!("(codex ended the turn: {m})"));
        let base = stated.unwrap_or_else(|| {
            if digest.interrupted {
                "(interrupted by the operator before a final answer)".to_string()
            } else if digest.exit_code == Some(0) && digest.signal.is_none() {
                "(codex produced no final message)".to_string()
            } else {
                "(codex died without a final answer)".to_string()
            }
        });
        let stderr = String::from_utf8_lossy(&stderr_buf);
        let detail = stderr.trim();
        if detail.is_empty() {
            base
        } else {
            format!("{base}\n{detail}")
        }
    });
    Ok(RunOutput {
        text,
        session_id,
        digest: Some(digest),
        served: Served::default(),
    })
}

/// Signal name when a process was terminated by a signal, else `None`.
fn signal_name(status: &std::process::ExitStatus) -> Option<String> {
    use std::os::unix::process::ExitStatusExt;
    status.signal().map(|sig| match sig {
        2 => "SIGINT".to_string(),
        9 => "SIGKILL".to_string(),
        15 => "SIGTERM".to_string(),
        other => format!("signal {other}"),
    })
}

pub fn print_result(result: &ProviderResult) {
    match &result.output {
        Ok(text) => {
            println!("--- {} ---", result.provider);
            if let Some(ref sid) = result.session_id {
                println!("session: {sid}");
            }
            if let Some(ref d) = result.digest {
                print_digest(d);
            }
            println!("{text}");
            if let (Some(sid), Some(true)) = (
                result.session_id.as_deref(),
                result.digest.as_ref().map(|d| d.interrupted),
            ) {
                println!("\n{}", resume_hint(&result.provider, sid));
            }
        }
        Err(err) => {
            eprintln!("--- {} ---", result.provider);
            eprintln!("error: {err}");
        }
    }
}

/// How to continue an interrupted session, and by when - shared by the run's
/// own output and `review interrupt`, so both say the same thing.
pub fn resume_hint(provider: &str, session_id: &str) -> String {
    let cutoff = crate::timings::stale_session(provider).as_secs() / 60;
    format!(
        "interrupted - continue within {cutoff}m with:\n  \
         echo \"<message>\" | review resume {session_id}"
    )
}

fn print_digest(d: &Digest) {
    match d.exit_code {
        Some(code) => println!("exit: {code}"),
        None => println!("exit: -"),
    }
    if let Some(ref sig) = d.signal {
        println!("signal: {sig}");
    }
    println!("captured: {}", d.captured);
    // Ahead of everything else: codex stated why the turn ended, which reframes
    // the exit code and makes the notes below unnecessary. Most often an upstream
    // refusal, and the operator's next move (rephrase, get authorised, wait out a
    // limit) follows from the message, not from the exit status.
    if let Some(ref msg) = d.turn_error {
        println!("turn failed: {msg}");
    }
    // Print this before the notes below: it reframes the exit code/signal, which
    // otherwise read as codex dying on its own when in fact we killed it.
    if let Some(ref why) = d.terminated_by_review {
        println!("terminated by review: {why}");
        if let Some(secs) = d.quiet_secs {
            println!("  rollout silent for: {secs}s");
        }
        if let Some(ref ev) = d.last_rollout_event {
            println!("  last rollout event: {ev}");
        }
    }
    if d.recovered_from_transcript {
        println!("recovered: final answer restored from transcript (stream/-o truncated)");
    } else if !d.captured {
        // No -o and nothing to recover: no final answer was produced. Note that
        // task_complete is NOT a success signal - it fires on aborted turns too
        // (task_complete.last_agent_message=null), so the exit status sets tone.
        // A kill by us is excluded: "died" would be a lie about our own doing.
        if d.turn_error.is_some() {
            // Already explained by `turn failed:` above. Saying "died without a
            // final answer" underneath a stated cause is the bug this branch
            // exists to avoid: it reframes an answered question as a mystery.
            println!("note: no conclusion was produced; the text below is interim");
        } else if d.terminated_by_review.is_some() {
            println!(
                "note: review terminated the run; the text below is whatever codex \
                 had produced by then"
            );
        } else if d.exit_code != Some(0) || d.signal.is_some() {
            println!(
                "note: run died without a final answer; the text below is the last \
                 interim note, not a conclusion"
            );
        } else if d.transcript.as_ref().is_some_and(|t| t.task_complete) {
            println!("note: turn ended without a final answer (text below is interim)");
        }
    }
    println!("turns: {}", d.turns);
    let u = &d.usage;
    println!(
        "usage: input={} cached={} output={} reasoning={}",
        u.input_tokens, u.cached_input_tokens, u.output_tokens, u.reasoning_output_tokens
    );
    if !d.log_lines.is_empty() {
        println!("--- codex log lines ({}) ---", d.log_lines.len());
        for line in &d.log_lines {
            println!("{line}");
        }
        println!("--- end log lines ---");
    }
    if let Some(ref t) = d.transcript {
        println!("transcript: {}", t.path);
        println!(
            "  task_complete={} stream_error={}",
            t.task_complete, t.stream_error
        );
        if let Some(ref last) = t.last_event {
            println!("  last_event: {last}");
        }
        if let Some((ref name, ref args)) = t.last_in_flight_tool {
            println!("  last_in_flight_tool: {name} {}", truncate(args, 200));
        }
    }
    if let Some(ref path) = d.incident_path {
        println!("incident: {path}");
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push_str("...");
    out
}
