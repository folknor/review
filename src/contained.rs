//! `review`'s side of the keeper (see `keeper`): every provider process
//! `review` starts is launched through `spawn`, which puts a keeper between the
//! two, and a run is not over until that keeper has torn down whatever the
//! provider left behind and been reaped.
//!
//! Three process-wide records hang off it:
//!
//! - the ledger: one verdict per invocation, whichever answer is reported. The
//!   exit status reads it (exit 3), so a run whose teardown failed fails the command even
//!   when an auto-resume, a grok wake or another provider in the fan-out
//!   produced the answer;
//! - the registry of live keepers, which the signal supervisor terminates and
//!   waits for on its way out;
//! - the admission gate: once shutdown has begun no keeper is spawned, checked
//!   under the same lock the registry is updated under, so a launch can never
//!   slip between the supervisor's snapshot and its exit.

use crate::keeper::{self, Event, Outcome, Proc};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::AsyncBufReadExt;
use tokio::process::{Child, Command};

/// How a teardown is paced, and which binary is the keeper. Injectable so the
/// integration tests can run the real keeper in milliseconds.
#[derive(Clone, Debug)]
pub struct Settings {
    pub keeper: PathBuf,
    pub leftover_grace: Duration,
    pub terminate_grace: Duration,
    pub kill_phase: Duration,
    pub beat: Duration,
    pub startup: Duration,
    pub flush: Duration,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            // The running `review` is its own keeper; `main` dispatches on
            // `keeper::ARG` before anything else. `/proc/self/exe` rather than
            // the path it was started from: exec'd in the forked child, it is
            // the image this `review` is running, so a binary replaced
            // mid-run (an install while runs are in flight) cannot hand later
            // launches a keeper speaking another version of the protocol.
            keeper: PathBuf::from("/proc/self/exe"),
            leftover_grace: crate::timings::LEFTOVER_GRACE,
            terminate_grace: crate::timings::SIGKILL_ESCALATION,
            kill_phase: crate::timings::KILL_PHASE,
            beat: crate::timings::TEARDOWN_BEAT,
            startup: crate::timings::KEEPER_STARTUP,
            flush: crate::timings::KEEPER_FLUSH,
        }
    }
}

impl Settings {
    /// The longest a keeper takes from `terminate` to its last event: the
    /// bound the signal supervisor waits for on its way out.
    pub fn terminate_bound(&self) -> Duration {
        self.terminate_grace + self.kill_phase + self.flush + self.beat * 4
    }
}

/// What one invocation left behind, as recorded in its audit and sidecar rows.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Leftovers {
    /// Ended by the teardown. Recorded, never printed: nobody can act on it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub killed: Vec<Proc>,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub killed_dropped: usize,
    /// Still alive after the teardown. Printed, and the command fails.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub survivors: Vec<Proc>,
    /// Processes remained that could not be named; `survivors` is a lower bound.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub incomplete: bool,
    /// The teardown's outcome never arrived intact (keeper died, events
    /// truncated or malformed, run abandoned before it finished).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unknown: Option<String>,
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

impl Leftovers {
    /// Nothing of the invocation is alive, as established by its keeper.
    pub fn is_clean(&self) -> bool {
        self.survivors.is_empty() && !self.incomplete && self.unknown.is_none()
    }

    /// Nothing worth recording at all.
    pub fn is_empty(&self) -> bool {
        self.is_clean() && self.killed.is_empty() && self.killed_dropped == 0
    }

    /// Fold another invocation's leftovers into this record (grok wakes, the
    /// version probe), never losing a non-clean verdict.
    pub fn merge(&mut self, other: Leftovers) {
        self.killed.extend(other.killed);
        self.killed_dropped += other.killed_dropped;
        self.survivors.extend(other.survivors);
        self.incomplete |= other.incomplete;
        if self.unknown.is_none() {
            self.unknown = other.unknown;
        }
    }
}

/// One invocation whose teardown did not establish that nothing is alive.
#[derive(Clone, Debug)]
pub struct Unresolved {
    pub label: String,
    pub keeper_pid: u32,
    pub leftovers: Leftovers,
}

static LEDGER: Mutex<Vec<Unresolved>> = Mutex::new(Vec::new());

fn record(label: &str, keeper_pid: u32, leftovers: &Leftovers) {
    if leftovers.is_clean() {
        return;
    }
    if let Ok(mut ledger) = LEDGER.lock() {
        ledger.push(Unresolved {
            label: label.to_string(),
            keeper_pid,
            leftovers: leftovers.clone(),
        });
    }
}

/// Every invocation so far whose teardown was not clean.
pub fn unresolved() -> Vec<Unresolved> {
    LEDGER.lock().map(|l| l.clone()).unwrap_or_default()
}

/// The closing report: what is still running, or may be. Empty when every
/// teardown was clean. Plain lines: printed to stdout above the `runtime:`
/// line, or to stderr by the signal supervisor on its way out.
pub fn report_lines(entries: &[Unresolved]) -> Vec<String> {
    let mut out = Vec::new();
    for e in entries {
        let l = &e.leftovers;
        if let Some(why) = &l.unknown {
            out.push(format!(
                "teardown outcome unknown: {} (keeper pid {}) - {why}; processes it started \
                 may still be running",
                e.label, e.keeper_pid
            ));
            if !l.survivors.is_empty() {
                out.push("  known to have survived:".to_string());
            }
            for p in &l.survivors {
                out.push(format!(
                    "  pid {}  {}{}",
                    p.pid,
                    p.cmdline,
                    if p.truncated { " ..." } else { "" }
                ));
            }
            continue;
        }
        let n = l.survivors.len();
        out.push(format!(
            "left running: {}{} process{} {} started survived teardown",
            if l.incomplete { "at least " } else { "" },
            n.max(1),
            if n.max(1) == 1 { "" } else { "es" },
            e.label
        ));
        for p in &l.survivors {
            out.push(format!(
                "  pid {}  {}{}",
                p.pid,
                p.cmdline,
                if p.truncated { " ..." } else { "" }
            ));
        }
        if l.incomplete {
            out.push("  (more remain that could not be identified)".to_string());
        }
    }
    out
}

/// The control pipe's write end, shared by a run and the registry.
struct Control(OwnedFd);

impl Control {
    fn send(&self, byte: u8) -> std::io::Result<()> {
        // SAFETY: one byte from a local into a descriptor we own; the pipe is
        // non-blocking, and the keeper drains it promptly.
        let n = unsafe {
            libc::write(
                self.0.as_raw_fd(),
                (&raw const byte).cast::<libc::c_void>(),
                1,
            )
        };
        if n == 1 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }
}

#[derive(Default)]
struct Registry {
    closed: bool,
    live: std::collections::HashMap<u32, Arc<Control>>,
}

static REGISTRY: Mutex<Option<Registry>> = Mutex::new(None);

fn with_registry<T>(f: impl FnOnce(&mut Registry) -> T) -> Option<T> {
    REGISTRY
        .lock()
        .ok()
        .map(|mut g| f(g.get_or_insert_with(Default::default)))
}

/// Begin shutdown: no keeper spawns from here on, and every live one is told
/// to terminate. Returns how many were told.
pub fn terminate_all() -> usize {
    with_registry(|r| {
        r.closed = true;
        for control in r.live.values() {
            let _ = control.send(b't');
        }
        r.live.len()
    })
    .unwrap_or(0)
}

/// Keeper pids still running, for the shutdown report.
pub fn live_keepers() -> Vec<u32> {
    with_registry(|r| r.live.keys().copied().collect()).unwrap_or_default()
}

/// A pipe pair with both ends close-on-exec and numbered at least 3, so the
/// stdio set-up of a spawn can never overwrite one.
fn pipe_pair() -> std::io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0 as RawFd; 2];
    // SAFETY: fills two descriptors we then own.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: fresh descriptors.
    let (r, w) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    Ok((above_stdio(r)?, above_stdio(w)?))
}

fn above_stdio(fd: OwnedFd) -> std::io::Result<OwnedFd> {
    if fd.as_raw_fd() > 2 {
        return Ok(fd);
    }
    // SAFETY: duplicates a descriptor we own; the original closes on drop.
    let dup = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 3) };
    if dup < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: fresh descriptor.
    Ok(unsafe { OwnedFd::from_raw_fd(dup) })
}

/// What the events reader collected.
#[derive(Default)]
struct Events {
    survivors: Vec<Proc>,
    done: Option<Outcome>,
    malformed: Option<String>,
}

/// Longest events line accepted; a survivor's command line is capped well
/// under this by the keeper.
const EVENT_LINE_MAX: usize = 64 << 10;

/// Read the keeper's events into `collected` as they arrive, so whatever was
/// read survives the reader being aborted.
async fn read_events(
    fd: OwnedFd,
    spawned: tokio::sync::oneshot::Sender<std::result::Result<u32, String>>,
    exited: tokio::sync::watch::Sender<bool>,
    collected: Arc<Mutex<Events>>,
) {
    let record = |f: &mut dyn FnMut(&mut Events)| {
        if let Ok(mut e) = collected.lock() {
            f(&mut e);
        }
    };
    let mut spawned = Some(spawned);
    let Ok(rx) = tokio::net::unix::pipe::Receiver::from_owned_fd(fd) else {
        record(&mut |e| e.malformed = Some("could not read the keeper's events".to_string()));
        return;
    };
    let mut reader = tokio::io::BufReader::new(rx);
    let mut line = Vec::new();
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        // Past the cap the line is still drained, never parsed.
        if line.len() > EVENT_LINE_MAX || line.last() != Some(&b'\n') {
            record(&mut |e| {
                e.malformed
                    .get_or_insert_with(|| "a truncated keeper event".to_string());
            });
            continue;
        }
        match serde_json::from_slice::<Event>(&line) {
            Ok(Event::Spawned { pid }) => {
                if let Some(tx) = spawned.take() {
                    let _ = tx.send(Ok(pid));
                }
            }
            Ok(Event::ExecFailed { reason }) => {
                if let Some(tx) = spawned.take() {
                    let _ = tx.send(Err(reason));
                }
            }
            Ok(Event::Exited { .. }) => {
                exited.send_replace(true);
            }
            Ok(Event::Survivor(p)) => record(&mut |e| e.survivors.push(p.clone())),
            Ok(Event::Done(outcome)) => record(&mut |e| e.done = Some(outcome.clone())),
            Err(err) => record(&mut |e| {
                e.malformed
                    .get_or_insert_with(|| format!("a malformed keeper event: {err}"));
            }),
        }
    }
}

/// A provider running under its keeper. Use it as the provider's `Child`:
/// take its pipes from `child`, wait with `wait`, then `finish`.
pub struct Contained {
    /// The keeper process. Its stdio is the provider's.
    pub child: Child,
    keeper_pid: u32,
    control: Arc<Control>,
    events: Option<tokio::task::JoinHandle<()>>,
    collected: Arc<Mutex<Events>>,
    /// How long `finish` waits for the events pipe to close after the keeper
    /// has exited.
    flush_bound: Duration,
    spawned: Option<tokio::sync::oneshot::Receiver<std::result::Result<u32, String>>>,
    exited: tokio::sync::watch::Receiver<bool>,
    label: String,
    finished: bool,
}

/// Launch `provider` under a keeper. The keeper starts nothing until `go`, so
/// the caller can record its identity first. Provider stdout and stderr are
/// piped; stdin is the caller's choice.
///
/// Refused once shutdown has begun, checked under the registry lock that
/// shutdown closes, and registered under it before the spawn returns.
pub fn spawn(
    provider: &Command,
    stdin: std::process::Stdio,
    settings: &Settings,
    label: &str,
    orphan_log: Option<PathBuf>,
) -> Result<Contained> {
    let std_cmd = provider.as_std();
    let (control_r, control_w) = pipe_pair().context("keeper control pipe")?;
    let (events_r, events_w) = pipe_pair().context("keeper events pipe")?;
    let ms = |d: Duration| u64::try_from(d.as_millis()).unwrap_or(u64::MAX);
    let spec = keeper::Spec {
        program: std_cmd.get_program().to_string_lossy().into_owned(),
        args: std_cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect(),
        control_fd: control_r.as_raw_fd(),
        events_fd: events_w.as_raw_fd(),
        leftover_grace_ms: ms(settings.leftover_grace),
        terminate_grace_ms: ms(settings.terminate_grace),
        kill_phase_ms: ms(settings.kill_phase),
        beat_ms: ms(settings.beat),
        startup_ms: ms(settings.startup),
        flush_ms: ms(settings.flush),
        orphan_log,
        label: label.to_string(),
    };
    let mut cmd = Command::new(&settings.keeper);
    cmd.arg(keeper::ARG)
        .arg(serde_json::to_string(&spec)?)
        // Outside the terminal's group and every provider's: nothing reaches
        // it but what `review` sends.
        .process_group(0)
        .stdin(stdin)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    if let Some(dir) = std_cmd.get_current_dir() {
        cmd.current_dir(dir);
    }
    for (key, value) in std_cmd.get_envs() {
        match value {
            Some(v) => cmd.env(key, v),
            None => cmd.env_remove(key),
        };
    }
    let keep = [control_r.as_raw_fd(), events_w.as_raw_fd()];
    // SAFETY: the hook only calls async-signal-safe functions (sigprocmask,
    // fcntl) on descriptors that exist in the child.
    unsafe {
        cmd.pre_exec(move || {
            keeper::block_handled_signals()?;
            for fd in keep {
                let flags = libc::fcntl(fd, libc::F_GETFD);
                if flags < 0 || libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
    let control = Arc::new(Control(control_w));
    let child = with_registry(|r| -> Result<Child> {
        if r.closed || crate::provider::stop_requested() {
            anyhow::bail!("not launched: review was signalled to stop");
        }
        let child = cmd.spawn().context("failed to spawn the keeper")?;
        if let Some(pid) = child.id() {
            r.live.insert(pid, Arc::clone(&control));
        }
        Ok(child)
    })
    .unwrap_or_else(|| Err(anyhow::anyhow!("keeper registry unavailable")))?;
    // The keeper has its own copies now.
    drop(control_r);
    drop(events_w);
    let keeper_pid = child.id().unwrap_or(0);
    let (tx, rx) = tokio::sync::oneshot::channel();
    // SAFETY: fcntl on a descriptor we own.
    unsafe {
        let flags = libc::fcntl(events_r.as_raw_fd(), libc::F_GETFL);
        libc::fcntl(
            events_r.as_raw_fd(),
            libc::F_SETFL,
            flags | libc::O_NONBLOCK,
        );
        let flags = libc::fcntl(control.0.as_raw_fd(), libc::F_GETFL);
        libc::fcntl(
            control.0.as_raw_fd(),
            libc::F_SETFL,
            flags | libc::O_NONBLOCK,
        );
    }
    let (exited_tx, exited_rx) = tokio::sync::watch::channel(false);
    let collected = Arc::new(Mutex::new(Events::default()));
    let events = tokio::spawn(read_events(events_r, tx, exited_tx, Arc::clone(&collected)));
    Ok(Contained {
        child,
        keeper_pid,
        control,
        events: Some(events),
        collected,
        flush_bound: settings.flush + settings.beat * 4,
        spawned: Some(rx),
        exited: exited_rx,
        label: label.to_string(),
        finished: false,
    })
}

/// How a contained invocation ended.
pub struct Finished {
    /// The provider's own exit status. `None` when it never ran, or its
    /// keeper's report did not arrive.
    pub status: Option<std::process::ExitStatus>,
    pub leftovers: Leftovers,
}

impl Contained {
    /// The keeper's pid: what the in-flight marker records and `review
    /// interrupt` signals.
    pub fn pid(&self) -> u32 {
        self.keeper_pid
    }

    /// Name the run in the ledger and the report, once its session is known.
    pub fn set_label(&mut self, label: String) {
        self.label = label;
    }

    /// Let the keeper start the provider, then wait until it has (or failed
    /// to). Returns the provider's pid.
    pub async fn go(&mut self) -> Result<u32> {
        self.control
            .send(b'g')
            .context("could not tell the keeper to launch")?;
        let rx = self
            .spawned
            .take()
            .context("the provider was already launched")?;
        match rx.await {
            Ok(Ok(pid)) => Ok(pid),
            Ok(Err(reason)) => anyhow::bail!("failed to launch: {reason}"),
            Err(_) => anyhow::bail!("the keeper exited before launching the provider"),
        }
    }

    /// `go`, keeping the teardown's verdict when the launch fails: the keeper
    /// still reports, and the ledger gets what it found rather than an
    /// "unknown" from an abandoned handle.
    pub async fn launch(mut self) -> Result<Self> {
        match self.go().await {
            Ok(_) => Ok(self),
            Err(e) => {
                let status = self.wait().await;
                self.finish(&status).await;
                Err(e)
            }
        }
    }

    /// Forward one SIGINT to the provider (the keeper ignores repeats).
    pub fn interrupt(&self) {
        let _ = self.control.send(b'i');
    }

    /// Tear the provider and everything it started down now.
    pub fn terminate(&self) {
        let _ = self.control.send(b't');
    }

    /// Wait for the provider itself to exit (or the keeper, should it die
    /// first). The run's teardown has not finished yet: follow with `wait`.
    /// Cancel-safe, so it can sit in a `select!` beside a watchdog, which must
    /// judge the provider and not the teardown of what it left.
    pub async fn wait_provider(&mut self) {
        let mut exited = self.exited.clone();
        tokio::select! {
            _ = exited.wait_for(|e| *e) => {}
            _ = self.child.wait() => {}
        }
    }

    /// Wait for the keeper to exit, which it does only after the teardown.
    /// Cancel-safe.
    pub async fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
        self.child.wait().await
    }

    /// Collect the keeper's report once `wait` has returned, and record the
    /// verdict. An incomplete report is never read as a clean one.
    pub async fn finish(
        mut self,
        keeper_status: &std::io::Result<std::process::ExitStatus>,
    ) -> Finished {
        // `finished` is set only once the verdict is recorded: cancelled
        // before that, `Drop` records the run as unknown.
        //
        // Bounded: the keeper has exited, but a provider stuck before its
        // exec still holds the inherited events pipe, and EOF would never
        // come. What was read by then is the report.
        if let Some(mut handle) = self.events.take()
            && tokio::time::timeout(self.flush_bound, &mut handle)
                .await
                .is_err()
        {
            handle.abort();
            let _ = handle.await;
            if let Ok(mut e) = self.collected.lock() {
                e.malformed
                    .get_or_insert_with(|| "the keeper's report was cut off".to_string());
            }
        }
        let events = self
            .collected
            .lock()
            .map(|mut e| std::mem::take(&mut *e))
            .unwrap_or_default();
        let keeper_ok = matches!(keeper_status, Ok(s) if s.success());
        let finished = match (&events.done, &events.malformed, keeper_ok) {
            (Some(outcome), None, true) if outcome.survivors == events.survivors.len() => {
                Finished {
                    status: outcome.status.map(std::process::ExitStatus::from_raw),
                    leftovers: Leftovers {
                        killed: outcome.killed.clone(),
                        killed_dropped: outcome.killed_dropped,
                        survivors: if outcome.clean {
                            Vec::new()
                        } else {
                            events.survivors
                        },
                        incomplete: !outcome.clean && outcome.incomplete,
                        unknown: None,
                    },
                }
            }
            (done, malformed, _) => {
                let why = match (done, malformed, keeper_status) {
                    (_, Some(m), _) => m.clone(),
                    (_, _, Err(e)) => format!("could not wait for the keeper: {e}"),
                    (_, _, Ok(s)) if !s.success() => format!("the keeper exited abnormally ({s})"),
                    (None, _, _) => "the keeper's report never arrived".to_string(),
                    (Some(_), _, _) => "the keeper's report was incomplete".to_string(),
                };
                // Survivors already named are kept beside the unknown: what
                // the keeper knew is still true, it is just not all of it.
                Finished {
                    status: done
                        .as_ref()
                        .and_then(|o| o.status)
                        .map(std::process::ExitStatus::from_raw),
                    leftovers: Leftovers {
                        killed: done.as_ref().map(|o| o.killed.clone()).unwrap_or_default(),
                        killed_dropped: done.as_ref().map_or(0, |o| o.killed_dropped),
                        survivors: events.survivors.clone(),
                        incomplete: false,
                        unknown: Some(why),
                    },
                }
            }
        };
        record(&self.label, self.keeper_pid, &finished.leftovers);
        let pid = self.keeper_pid;
        with_registry(|r| r.live.remove(&pid));
        self.finished = true;
        finished
    }
}

impl Drop for Contained {
    /// A run abandoned before `finish` (an early error return, a cancelled
    /// task) still owes its teardown: the keeper is told to terminate, and the
    /// ledger records that nobody saw it finish.
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        self.terminate();
        record(
            &self.label,
            self.keeper_pid,
            &Leftovers {
                unknown: Some("the run ended before its teardown was confirmed".to_string()),
                ..Leftovers::default()
            },
        );
        let pid = self.keeper_pid;
        with_registry(|r| r.live.remove(&pid));
    }
}

/// `binary --version` under a keeper, for incident bundles: a wrapper or a
/// stub can fork too. Its leftovers join the run's.
pub async fn version_probe(binary: &str, settings: &Settings) -> (Option<String>, Leftovers) {
    let mut cmd = Command::new(binary);
    cmd.arg("--version");
    let label = format!("{binary} --version probe");
    // Never inherit our stdin: a binary that reads stdin before parsing its
    // arguments would block forever, taking the incident write with it.
    let mut contained = match spawn(&cmd, std::process::Stdio::null(), settings, &label, None) {
        Ok(c) => c,
        Err(_) => return (None, Leftovers::default()),
    };
    let stdout = contained.child.stdout.take();
    let stderr = contained.child.stderr.take();
    // Both pipes drained to EOF, keeping only a version's worth: a probe left
    // blocked on a full pipe would never exit, and its keeper never start the
    // teardown.
    async fn drain<R: tokio::io::AsyncRead + Unpin>(pipe: Option<R>) -> Vec<u8> {
        use tokio::io::AsyncReadExt;
        let mut kept = Vec::new();
        let Some(mut pipe) = pipe else {
            return kept;
        };
        let mut chunk = [0u8; 4096];
        while let Ok(n) = pipe.read(&mut chunk).await {
            if n == 0 {
                break;
            }
            let room = (64usize << 10).saturating_sub(kept.len()).min(n);
            kept.extend_from_slice(&chunk[..room]);
        }
        kept
    }
    let run = async {
        let launched = contained.go().await.is_ok();
        let (buf, _, status) = tokio::join!(drain(stdout), drain(stderr), contained.wait());
        (launched, buf, status)
    };
    // Bounded end to end, launch included: a probe that has not finished
    // within the startup bound is terminated, and given the keeper's own
    // teardown bound. Past that it is abandoned, which `Drop` records as an
    // unknown outcome; an incident bundle is not worth a hung run.
    let outcome = tokio::time::timeout(settings.startup, run).await;
    let (launched, buf, status) = match outcome {
        Ok(done) => done,
        Err(_) => {
            contained.terminate();
            match tokio::time::timeout(settings.terminate_bound(), contained.wait()).await {
                Ok(status) => (false, Vec::new(), status),
                Err(_) => {
                    drop(contained);
                    return (
                        None,
                        Leftovers {
                            unknown: Some(
                                "the version probe's keeper did not finish in time".to_string(),
                            ),
                            ..Leftovers::default()
                        },
                    );
                }
            }
        }
    };
    let finished = contained.finish(&status).await;
    let text = String::from_utf8_lossy(&buf).trim().to_string();
    let version = (launched && !text.is_empty()).then_some(text);
    (version, finished.leftovers)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proc_(pid: u32, cmdline: &str) -> Proc {
        Proc {
            pid,
            cmdline: cmdline.to_string(),
            truncated: false,
        }
    }

    fn entry(leftovers: Leftovers) -> Unresolved {
        Unresolved {
            label: "the codex run of session s".to_string(),
            keeper_pid: 7,
            leftovers,
        }
    }

    #[test]
    fn cleanly_killed_leftovers_report_nothing() {
        let killed = Leftovers {
            killed: vec![proc_(1, "sleep 600")],
            ..Leftovers::default()
        };
        assert!(killed.is_clean());
        assert!(!killed.is_empty(), "but they are recorded");
    }

    #[test]
    fn survivors_are_named_one_per_line() {
        let lines = report_lines(&[entry(Leftovers {
            survivors: vec![proc_(41233, "python3 a.py"), proc_(41240, "python3 b.py")],
            ..Leftovers::default()
        })]);
        assert_eq!(
            lines,
            vec![
                "left running: 2 processes the codex run of session s started survived teardown",
                "  pid 41233  python3 a.py",
                "  pid 41240  python3 b.py",
            ]
        );
    }

    #[test]
    fn an_incomplete_enumeration_is_a_lower_bound_not_a_count() {
        let lines = report_lines(&[entry(Leftovers {
            incomplete: true,
            ..Leftovers::default()
        })]);
        assert!(
            lines[0].starts_with("left running: at least 1 process "),
            "{lines:?}"
        );
        assert!(lines[1].contains("could not be identified"), "{lines:?}");
    }

    #[test]
    fn an_unknown_outcome_never_reads_as_clean() {
        let unknown = Leftovers {
            unknown: Some("the keeper exited abnormally".to_string()),
            ..Leftovers::default()
        };
        assert!(!unknown.is_clean());
        let lines = report_lines(&[entry(unknown)]);
        assert!(
            lines[0].starts_with("teardown outcome unknown:"),
            "{lines:?}"
        );
    }

    #[test]
    fn an_unknown_outcome_still_names_the_survivors_it_knew() {
        let lines = report_lines(&[entry(Leftovers {
            survivors: vec![proc_(9, "python3 hung.py")],
            unknown: Some("the keeper's report was cut off".to_string()),
            ..Leftovers::default()
        })]);
        assert!(
            lines[0].starts_with("teardown outcome unknown:"),
            "{lines:?}"
        );
        assert!(
            lines.iter().any(|l| l.contains("pid 9  python3 hung.py")),
            "{lines:?}"
        );
    }

    #[test]
    fn merging_keeps_a_non_clean_verdict() {
        let mut run = Leftovers {
            killed: vec![proc_(1, "a")],
            ..Leftovers::default()
        };
        run.merge(Leftovers {
            survivors: vec![proc_(2, "b")],
            ..Leftovers::default()
        });
        run.merge(Leftovers::default());
        assert!(!run.is_clean());
        assert_eq!(run.killed.len(), 1);
        assert_eq!(run.survivors.len(), 1);
    }
}
