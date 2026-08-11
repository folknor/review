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
    let dir = std::env::temp_dir();
    for _ in 0..8 {
        let path = dir.join(format!(
            "review-codex-{}.txt",
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
                return Err(anyhow::anyhow!(
                    "failed to create codex output temp file: {e}"
                ));
            }
        }
    }
    anyhow::bail!("could not create a unique codex output temp file after 8 tries")
}

/// Caps on how much provider output we buffer in memory. Generous - real reviews
/// are far under these; the cap only stops a runaway stream from OOMing review.
const STDOUT_CAPTURE_CAP: usize = 64 << 20; // 64 MiB
const STDERR_CAPTURE_CAP: usize = 8 << 20; // 8 MiB

/// Read `r` to EOF, buffering at most `cap` bytes but continuing to drain the
/// rest (so the child never blocks on a full pipe). Returns the buffered prefix.
async fn read_capped<R>(mut r: R, cap: usize) -> Vec<u8>
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 16384];
    loop {
        match r.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if buf.len() < cap {
                    let take = (cap - buf.len()).min(n);
                    buf.extend_from_slice(&chunk[..take]);
                }
                // Past the cap: keep reading (drain) but stop buffering.
            }
        }
    }
    buf
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
async fn read_capped_stdout<R>(
    mut r: R,
    cap: usize,
    sid_tx: tokio::sync::watch::Sender<Option<String>>,
    mut scanning: bool,
) -> Vec<u8>
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 16384];
    // Carry for a line split across two reads; only maintained while scanning.
    let mut partial: Vec<u8> = Vec::new();
    loop {
        match r.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
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
                if buf.len() < cap {
                    let take = (cap - buf.len()).min(n);
                    buf.extend_from_slice(&chunk[..take]);
                }
                // Past the cap: keep reading (drain) but stop buffering.
            }
        }
    }
    buf
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
        for pid in pids {
            terminate_group(pid);
        }
        // Give SIGTERM a brief moment to land before we go; the escalation to
        // SIGKILL inside `terminate_group` dies with us, so this is the only
        // window codex gets to shut down cleanly.
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        for pid in CODEX_GROUPS
            .lock()
            .ok()
            .and_then(|g| g.as_ref().map(|s| s.iter().copied().collect::<Vec<_>>()))
            .unwrap_or_default()
        {
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
fn terminate_group(pid: u32) {
    let Ok(pid) = i32::try_from(pid) else {
        return;
    };
    // SAFETY: a negative pid addresses the process group with that id. codex was
    // spawned with `process_group(0)`, so this group contains codex and its
    // descendants and nothing else.
    unsafe {
        libc::kill(-pid, libc::SIGTERM);
    }
    // Escalate if SIGTERM is ignored - a codex stuck in an unbounded shutdown
    // await may never process it.
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(10)).await;
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
}

#[allow(clippy::too_many_arguments)]
pub async fn invoke(
    provider: &str,
    session_id: &str,
    model: Option<&str>,
    effort: Option<&str>,
    sandbox: Option<&str>,
    env: Option<&std::collections::BTreeMap<String, String>>,
    config: &[String],
    prompt: &str,
    project_root: &Path,
    oneshot: bool,
) -> ProviderResult {
    let result = match provider {
        // `sandbox` is a codex-only concept (an OS filesystem sandbox). Claude's
        // `--permission-mode` is a tool-approval policy on a different axis with
        // no honest mapping, so claude ignores it. `config` (codex `-c`) is
        // likewise codex-only.
        "claude" => {
            run_claude(
                session_id,
                model,
                effort,
                env,
                prompt,
                project_root,
                oneshot,
            )
            .await
        }
        "codex" => {
            run_codex(
                session_id,
                model,
                effort,
                sandbox,
                env,
                config,
                prompt,
                project_root,
                oneshot,
            )
            .await
        }
        other => Err(anyhow::anyhow!("unknown provider: {other}")),
    };
    let completed_epoch = now_epoch_secs();
    match result {
        Ok(run) => ProviderResult {
            provider: provider.to_string(),
            output: Ok(run.text),
            session_id: run.session_id,
            digest: run.digest,
            completed_epoch,
        },
        Err(e) => ProviderResult {
            provider: provider.to_string(),
            output: Err(e),
            session_id: None,
            digest: None,
            completed_epoch,
        },
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
) -> Result<RunOutput> {
    // In oneshot mode, generate a UUID up front and pass it via --session-id
    // so the fresh session is persistable and the operator can follow up via
    // `--session <id>`. (Previously used --no-session-persistence, which made
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

    let text = run_with_stdout(child, prompt, "claude").await?;
    Ok(RunOutput {
        text,
        session_id: oneshot_id,
        digest: None,
    })
}

#[allow(clippy::too_many_arguments)]
async fn run_codex(
    session_id: &str,
    model: Option<&str>,
    effort: Option<&str>,
    sandbox: Option<&str>,
    env: Option<&std::collections::BTreeMap<String, String>>,
    config: &[String],
    prompt: &str,
    project_root: &Path,
    oneshot: bool,
) -> Result<RunOutput> {
    let last_msg_path = new_output_file()?;

    // Exec-level options shared by both paths.
    let mut args: Vec<String> = vec![
        "exec".to_string(),
        "--sandbox".to_string(),
        sandbox.unwrap_or("read-only").to_string(),
    ];
    if let Some(m) = model {
        args.push("-m".to_string());
        args.push(m.to_string());
    }
    if let Some(e) = effort {
        args.push("-c".to_string());
        args.push(format!("model_reasoning_effort=\"{e}\""));
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
    )
    .await
}

/// Shared codex runner. `args` must include `--json` and `-o <last_msg_path>`.
/// Pipes the prompt on stdin and distills a `Digest` (usage, turns, captured,
/// exit/signal, log lines, transcript) from the NDJSON stream plus the `-o`
/// backstop. `known_session_id` is `Some` on resume (we already know it) and
/// `None` on a fresh run (parsed from `thread.started`). Never bails on a
/// non-zero exit: a halted/errored run still reports what it produced.
async fn run_codex_json(
    args: Vec<String>,
    last_msg_path: &str,
    known_session_id: Option<String>,
    env: Option<&std::collections::BTreeMap<String, String>>,
    prompt: &str,
    project_root: &Path,
) -> Result<RunOutput> {
    let mut cmd = Command::new("codex");
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
    let start = std::time::Instant::now();
    // Wall-clock at spawn, used to gate transcript recovery to this run's turn:
    // any final_answer in the rollout stamped before now belongs to an earlier
    // turn (a resume appends to the same file) and must not be recovered.
    let started_at = crate::audit::chrono_utc(now_epoch_secs());
    let mut child = cmd.spawn().context("failed to spawn codex")?;
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
        async move {
            loop {
                let current = rx.borrow_and_update().clone();
                if let Some(sid) = current {
                    let _guard = crate::inflight::mark(&sid, "codex", &project);
                    // Hold the marker until this task is aborted.
                    std::future::pending::<()>().await;
                }
                if rx.changed().await.is_err() {
                    break;
                }
            }
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
    let stdout_task = tokio::spawn(read_capped_stdout(
        stdout_pipe,
        STDOUT_CAPTURE_CAP,
        sid_tx,
        scanning_for_session_id,
    ));
    let stderr_task = tokio::spawn(read_capped(stderr_pipe, STDERR_CAPTURE_CAP));
    let write_task = tokio::spawn(write_stdin(stdin, prompt.as_bytes().to_vec()));

    let child_pid = child.id();
    // Make this run's process group visible to the signal supervisor, so an
    // operator killing `review` takes codex with it instead of orphaning it.
    if let Some(pid) = child_pid {
        register_group(pid);
    }
    // Records that *we* ended the run, and why. See `Digest::terminated_by_review`.
    let mut terminated_by_review: Option<String> = None;

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
            started_at.clone(),
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
                stranded = &mut watchdog, if terminated_by_review.is_none() => {
                    eprintln!("watchdog: {}", stranded.reason);
                    terminated_by_review = Some("watchdog: stranded completion".to_string());
                    if let Some(pid) = child_pid {
                        terminate_group(pid);
                    }
                }
            }
        }
    };
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
    const DRAIN_GRACE: std::time::Duration = std::time::Duration::from_secs(5);
    let stdout_buf = match tokio::time::timeout(DRAIN_GRACE, stdout_task).await {
        Ok(Ok(buf)) => buf,
        Ok(Err(_)) | Err(_) => Vec::new(),
    };
    let stderr_buf = match tokio::time::timeout(DRAIN_GRACE, stderr_task).await {
        Ok(Ok(buf)) => buf,
        Ok(Err(_)) | Err(_) => Vec::new(),
    };
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
    // the stream.
    let session_id = known_session_id.or(parsed_session_id);

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
    let transcript = if suspicious {
        session_id.as_deref().and_then(|sid| {
            crate::transcript::summarize_session(sid, codex_home, Some(&started_at))
        })
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

    // Dump a full forensic bundle for the same suspicious runs we post-mortem -
    // stderr (with backtraces), the raw stream, the transcript tail, the exact
    // argv, and codex's version - so the next death is over-instrumented.
    let incident_path = if suspicious {
        crate::incident::write_bundle(&crate::incident::Incident {
            provider: "codex",
            binary: "codex",
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
            captured,
            recovered_from_transcript,
            turns,
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
    };

    // Even when no message came back (a hard freeze before any agent_message,
    // the very case forensics exist to explain), return the digest rather than a
    // bare error - otherwise `invoke` would drop the session id and transcript.
    let text = final_message.unwrap_or_else(|| {
        // No final answer at all - not even an interim message. task_complete
        // is not a success signal (it fires on aborted turns with a null final
        // answer), so a non-zero exit/signal here means the run died mid-turn.
        let base = if digest.exit_code == Some(0) && digest.signal.is_none() {
            "(codex produced no final message)"
        } else {
            "(codex died without a final answer)"
        };
        let stderr = String::from_utf8_lossy(&stderr_buf);
        let detail = stderr.trim();
        if detail.is_empty() {
            base.to_string()
        } else {
            format!("{base}\n{detail}")
        }
    });
    Ok(RunOutput {
        text,
        session_id,
        digest: Some(digest),
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
        }
        Err(err) => {
            eprintln!("--- {} ---", result.provider);
            eprintln!("error: {err}");
        }
    }
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
    // Print this before the notes below: it reframes the exit code/signal, which
    // otherwise read as codex dying on its own when in fact we killed it.
    if let Some(ref why) = d.terminated_by_review {
        println!("terminated by review: {why}");
    }
    if d.recovered_from_transcript {
        println!("recovered: final answer restored from transcript (stream/-o truncated)");
    } else if !d.captured {
        // No -o and nothing to recover: no final answer was produced. Note that
        // task_complete is NOT a success signal - it fires on aborted turns too
        // (task_complete.last_agent_message=null), so the exit status sets tone.
        // A kill by us is excluded: "died" would be a lie about our own doing.
        if d.terminated_by_review.is_some() {
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
