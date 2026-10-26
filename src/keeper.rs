//! The keeper: one per provider invocation, between `review` and the provider,
//! so that when a run ends nothing it started is left running.
//!
//! # Why a process of its own
//!
//! A provider's descendants do not stay in the process group `review` gives it.
//! codex's Linux sandbox runs bwrap with `--new-session` and unified-exec
//! shells `setsid()`, so a shell the agent started is in a session of its own,
//! and when codex exits it is reparented to init. That is how two hung codemods
//! from a finished run kept a machine busy for five hours while `review` had
//! reported the run over. Killing the group cannot reach such a process, and
//! making `review` itself a child subreaper cannot attribute one: with several
//! runs in one `review`, an adopted orphan may belong to a run that is still
//! going, and killing it destroys live work. So each invocation gets its own
//! subreaper. Everything the provider starts, whatever it does to its session
//! or group, stays a descendant of this process, and this process belongs to
//! exactly one run.
//!
//! # The proof of emptiness
//!
//! As a subreaper the keeper is the nearest living ancestor of every process in
//! its tree, so while any of them lives, the keeper has at least one child.
//! `waitpid` returning `ECHILD` therefore proves the tree is empty, and nothing
//! else is accepted as that proof. Discovery (walking
//! `/proc/<pid>/task/<tid>/children`) only finds signal targets: the kernel
//! documents that walk as unreliable while siblings exit, so an empty walk
//! proves nothing.
//!
//! # Identity
//!
//! Signals go only through pidfds, never to a bare pid. A direct child is
//! admitted by pid (only the keeper can reap it, so its pid cannot be reused
//! under us). A process `c` found under an admitted `p` is admitted when, after
//! reading `c`'s parent from `/proc/c/stat`, both `c`'s and `p`'s pidfds still
//! show them alive: a process alive at the end of the check was alive
//! throughout and held its pid, so the parent read named `p` while `p` owned
//! that number, and `c` is in the tree.
//!
//! # Protocol
//!
//! `review` spawns `review __keep <spec-json>` with SIGINT, SIGTERM, SIGCHLD
//! and SIGPIPE blocked, so no signal can take its default action before the
//! keeper reads them from a signalfd. Two pipes beyond stdio, whose numbers are
//! in the spec: control (review to keeper: `g` go, `i` interrupt, `t`
//! terminate; EOF means `review` is gone) and events (keeper to review, one
//! JSON `Event` per line). The keeper launches nothing before `g`, so `review`
//! can record the keeper's identity first. Its own stdio is the provider's and
//! it never writes to it.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, VecDeque};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// The hidden first argument that turns a `review` process into a keeper.
pub const ARG: &str = "__keep";

/// Longest command line kept for a process, in bytes.
const CMDLINE_MAX: usize = 512;

/// How many cleanly killed processes an outcome names. Survivors are never
/// capped: the ticket's requirement is that every one is named.
pub const KILLED_MAX: usize = 64;

/// What `review` asks the keeper to run, and how patiently to tear it down.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Spec {
    pub program: String,
    pub args: Vec<String>,
    pub control_fd: RawFd,
    pub events_fd: RawFd,
    /// SIGTERM grace for what a provider left behind after exiting on its own.
    pub leftover_grace_ms: u64,
    /// SIGTERM grace when told to terminate a provider that is still running.
    pub terminate_grace_ms: u64,
    /// After the SIGTERM grace: how long SIGKILL rounds may take, leaves first.
    pub kill_phase_ms: u64,
    /// Pause between teardown rounds, which lets a supervising ancestor reap a
    /// killed child and run its own cleanup.
    pub beat_ms: u64,
    /// How long the provider may take to exec before the launch is failed.
    pub startup_ms: u64,
    /// How long the final events may take to drain before the keeper exits.
    pub flush_ms: u64,
    /// Appended to (one JSON line) when `review` is gone and the outcome has
    /// nobody to go to.
    pub orphan_log: Option<PathBuf>,
    /// What this run is, for that log line.
    pub label: String,
}

/// A process the teardown dealt with.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Proc {
    pub pid: u32,
    pub cmdline: String,
    /// The command line was cut at `CMDLINE_MAX`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
}

/// What the teardown established.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Outcome {
    /// The provider's raw wait status; `None` when it never ran or could not
    /// be reaped.
    pub status: Option<i32>,
    /// `ECHILD`: nothing the provider started is alive.
    pub clean: bool,
    /// Processes left behind that the teardown ended, up to `KILLED_MAX`.
    pub killed: Vec<Proc>,
    /// Further killed processes not named in `killed`.
    #[serde(default)]
    pub killed_dropped: usize,
    /// Processes still alive that the keeper could name; each was sent as a
    /// `Survivor` event before this outcome.
    pub survivors: usize,
    /// Processes remain that discovery could not name, so `survivors` is a
    /// lower bound.
    pub incomplete: bool,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    Spawned {
        pid: u32,
    },
    ExecFailed {
        reason: String,
    },
    /// The provider itself has exited and been reaped; the teardown of what it
    /// left follows. Lets `review` stop watching the provider (the watchdog,
    /// interrupts) without waiting out the teardown.
    Exited {
        status: i32,
    },
    Survivor(Proc),
    Done(Outcome),
}

/// The keeper process. Returns its exit code: 0 once it has run to the end of
/// its protocol, whether or not its events could all be delivered (a reader
/// that stopped draining, or a `review` that is gone; `review` judges the
/// report by what reached it, never by this code alone), 1 for an unusable
/// spec or setup, 101 for a panic.
pub fn main() -> i32 {
    // stdio belongs to the provider, so a panic must print nothing; an aborted
    // keeper is reported by `review` as a keeper failure.
    std::panic::set_hook(Box::new(|_| {}));
    match std::panic::catch_unwind(run_from_args) {
        Ok(Ok(())) => 0,
        Ok(Err(_)) => 1,
        Err(_) => 101,
    }
}

fn run_from_args() -> std::io::Result<()> {
    let spec = std::env::args()
        .nth(2)
        .and_then(|s| serde_json::from_str::<Spec>(&s).ok())
        .ok_or_else(|| std::io::Error::other("bad keeper spec"))?;
    Keeper::new(spec)?.run()
}

fn last_error() -> std::io::Error {
    std::io::Error::last_os_error()
}

fn check(ret: libc::c_int) -> std::io::Result<libc::c_int> {
    if ret < 0 { Err(last_error()) } else { Ok(ret) }
}

/// The signals the keeper handles through its signalfd.
fn handled_signals() -> libc::sigset_t {
    // SAFETY: sigemptyset/sigaddset only write the set we own.
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        for sig in [libc::SIGINT, libc::SIGTERM, libc::SIGCHLD, libc::SIGPIPE] {
            libc::sigaddset(&mut set, sig);
        }
        set
    }
}

/// Block the handled signals in the calling thread. `review` calls this in the
/// keeper's pre-exec hook, so they are blocked from the keeper's first
/// instruction; the keeper repeats it, which is harmless.
pub fn block_handled_signals() -> std::io::Result<()> {
    let set = handled_signals();
    // SAFETY: async-signal-safe; only changes the calling thread's mask.
    check(unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut()) })?;
    Ok(())
}

fn set_cloexec(fd: RawFd) -> std::io::Result<()> {
    // SAFETY: fcntl on a descriptor we own.
    unsafe {
        let flags = check(libc::fcntl(fd, libc::F_GETFD))?;
        check(libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC))?;
    }
    Ok(())
}

fn set_nonblocking(fd: RawFd) -> std::io::Result<()> {
    // SAFETY: fcntl on a descriptor we own.
    unsafe {
        let flags = check(libc::fcntl(fd, libc::F_GETFL))?;
        check(libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK))?;
    }
    Ok(())
}

pub fn pidfd_open(pid: u32) -> std::io::Result<OwnedFd> {
    let pid = libc::pid_t::try_from(pid).map_err(std::io::Error::other)?;
    // SAFETY: plain syscall; the returned descriptor is ours to own.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    if fd < 0 {
        return Err(last_error());
    }
    let fd = RawFd::try_from(fd).map_err(std::io::Error::other)?;
    // SAFETY: a fresh descriptor nobody else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// Whether the process behind `pidfd` has exited (its pidfd is readable). For
/// a multithreaded process that is when its last thread has exited, unlike its
/// leader's `/proc` state.
pub fn pidfd_exited(pidfd: &OwnedFd) -> bool {
    let mut pfd = libc::pollfd {
        fd: pidfd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: one pollfd we own, no timeout.
    let ret = unsafe { libc::poll(&mut pfd, 1, 0) };
    ret > 0 && pfd.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0
}

pub fn pidfd_signal(pidfd: &OwnedFd, sig: libc::c_int) -> std::io::Result<()> {
    // SAFETY: plain syscall on a pidfd we own; no siginfo.
    let ret = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            pidfd.as_raw_fd(),
            sig,
            std::ptr::null::<libc::siginfo_t>(),
            0,
        )
    };
    if ret < 0 { Err(last_error()) } else { Ok(()) }
}

/// `pid`'s parent and start time, from `/proc/<pid>/stat`. `comm` may hold
/// spaces and parentheses, so fields are read after the last `)`: state is
/// field 3, ppid 4, starttime 22.
pub fn stat_ppid_start(pid: u32) -> Option<(u32, u64)> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (_, rest) = stat.rsplit_once(')')?;
    let fields: Vec<&str> = rest.split_whitespace().collect();
    let ppid = fields.get(1)?.parse().ok()?;
    let start = fields.get(19)?.parse().ok()?;
    Some((ppid, start))
}

/// `pid`'s command line, and whether it is only the `comm` fallback (an empty
/// command line: mid-`execve`, a zombie, or a process that cleared its argv).
fn read_cmdline(pid: u32) -> (Proc, bool) {
    let raw = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
    let truncated = raw.len() > CMDLINE_MAX;
    let mut text: String = String::from_utf8_lossy(&raw[..raw.len().min(CMDLINE_MAX)])
        .chars()
        .map(|c| if c == '\0' || c.is_control() { ' ' } else { c })
        .collect();
    text.truncate(text.trim_end().len());
    let fallback = text.is_empty();
    if fallback {
        // Its comm still says what it is.
        text = std::fs::read_to_string(format!("/proc/{pid}/comm"))
            .map(|c| format!("[{}]", c.trim()))
            .unwrap_or_else(|_| "?".to_string());
    }
    (
        Proc {
            pid,
            cmdline: text,
            truncated,
        },
        fallback,
    )
}

fn past(deadline: Instant) -> std::io::Result<()> {
    if Instant::now() >= deadline {
        Err(std::io::Error::other("teardown deadline"))
    } else {
        Ok(())
    }
}

/// Children of `pid` across all its threads. `Err` when they cannot be read,
/// or `deadline` passes first, which leaves discovery incomplete rather than
/// empty.
fn children_of(pid: u32, deadline: Instant) -> std::io::Result<Vec<u32>> {
    let mut out = Vec::new();
    let tasks = std::fs::read_dir(format!("/proc/{pid}/task"))?;
    let mut any = false;
    for task in tasks {
        past(deadline)?;
        let task = task?;
        let content = match std::fs::read_to_string(task.path().join("children")) {
            Ok(c) => c,
            // Without CONFIG_PROC_CHILDREN the file does not exist; scan /proc
            // for this parent instead.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return children_by_scan(pid, deadline);
            }
            Err(e) => return Err(e),
        };
        any = true;
        out.extend(
            content
                .split_whitespace()
                .filter_map(|p| p.parse::<u32>().ok()),
        );
    }
    if !any {
        // The thread list is gone (the leader exited while other threads
        // live, or the process is a zombie): its children cannot be named.
        return Err(std::io::Error::other("no readable tasks"));
    }
    Ok(out)
}

fn children_by_scan(parent: u32, deadline: Instant) -> std::io::Result<Vec<u32>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir("/proc")?.flatten() {
        past(deadline)?;
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        if stat_ppid_start(pid).is_some_and(|(ppid, _)| ppid == parent) {
            out.push(pid);
        }
    }
    Ok(out)
}

struct Member {
    pidfd: OwnedFd,
    /// The last command line read while the process was alive. Read again
    /// every round: a process found between its fork and its exec still shows
    /// its parent's command line, and one mid-`execve` shows none.
    proc_: Proc,
    termed: bool,
}

#[derive(PartialEq)]
enum Phase {
    /// Waiting for `g`.
    Waiting,
    /// The provider has been forked; waiting for its exec.
    Starting,
    Running,
    /// Tearing down; never restarted.
    TearingDown,
}

struct Keeper {
    spec: Spec,
    me: u32,
    signals: OwnedFd,
    control: Option<OwnedFd>,
    events: Option<OwnedFd>,
    pending: VecDeque<u8>,
    owner_gone: bool,
    phase: Phase,
    leader: Option<(u32, OwnedFd)>,
    leader_status: Option<i32>,
    exec_status: Option<OwnedFd>,
    interrupted: bool,
    terminate: bool,
    members: BTreeMap<u32, Member>,
    killed: Vec<Proc>,
    killed_dropped: usize,
}

impl Keeper {
    fn new(spec: Spec) -> std::io::Result<Self> {
        block_handled_signals()?;
        let set = handled_signals();
        // SAFETY: creates a descriptor for the blocked set; we own it.
        let sfd =
            check(unsafe { libc::signalfd(-1, &set, libc::SFD_NONBLOCK | libc::SFD_CLOEXEC) })?;
        // SAFETY: fresh descriptor.
        let signals = unsafe { OwnedFd::from_raw_fd(sfd) };
        // SAFETY: plain prctl on ourselves.
        check(unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) })?;
        // Private from here on: the provider inherits neither.
        for fd in [spec.control_fd, spec.events_fd] {
            set_cloexec(fd)?;
            set_nonblocking(fd)?;
        }
        // SAFETY: review handed us these descriptors, ours alone from here.
        let control = Some(unsafe { OwnedFd::from_raw_fd(spec.control_fd) });
        let events = Some(unsafe { OwnedFd::from_raw_fd(spec.events_fd) });
        Ok(Self {
            spec,
            me: std::process::id(),
            signals,
            control,
            events,
            pending: VecDeque::new(),
            owner_gone: false,
            phase: Phase::Waiting,
            leader: None,
            leader_status: None,
            exec_status: None,
            interrupted: false,
            terminate: false,
            members: BTreeMap::new(),
            killed: Vec::new(),
            killed_dropped: 0,
        })
    }

    fn run(mut self) -> std::io::Result<()> {
        // Nothing launches before `g`: an owner already gone launches nothing.
        while self.phase == Phase::Waiting {
            if self.owner_gone {
                return Ok(());
            }
            if self.terminate {
                // Shutdown reached this keeper before its `g`: say so, with a
                // clean outcome (nothing was started), rather than leave the
                // run's teardown looking unconfirmed.
                self.queue(&Event::ExecFailed {
                    reason: "terminated before the provider started".to_string(),
                });
                self.finish(&Outcome {
                    clean: true,
                    ..Outcome::default()
                });
                return Ok(());
            }
            self.service(None);
        }
        let started = Instant::now();
        let startup = Duration::from_millis(self.spec.startup_ms);
        while self.phase == Phase::Starting {
            if self.terminate || started.elapsed() >= startup {
                let reason = if self.terminate {
                    "terminated before the provider started".to_string()
                } else {
                    format!("the provider did not start within {}s", startup.as_secs())
                };
                if let Some((_, fd)) = &self.leader {
                    let _ = pidfd_signal(fd, libc::SIGKILL);
                }
                self.queue(&Event::ExecFailed { reason });
                // The killed child closes the exec-status pipe by dying, which
                // would otherwise read as a successful exec mid-teardown.
                self.exec_status = None;
                break;
            }
            self.service(Some(startup.saturating_sub(started.elapsed())));
        }
        while self.phase == Phase::Running {
            if self.terminate || self.leader_status.is_some() {
                break;
            }
            self.service(None);
        }
        let grace = if self.leader_status.is_some() || self.leader.is_none() {
            self.spec.leftover_grace_ms
        } else {
            self.spec.terminate_grace_ms
        };
        self.phase = Phase::TearingDown;
        let outcome = self.teardown(Duration::from_millis(grace));
        self.finish(&outcome);
        Ok(())
    }

    /// Wait for at most `timeout` (forever if `None`) for something to happen,
    /// and handle it: signals, control bytes, the provider's exec status, and
    /// draining queued events.
    fn service(&mut self, timeout: Option<Duration>) {
        let mut fds = vec![libc::pollfd {
            fd: self.signals.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        }];
        let control_at = self.control.as_ref().map(|c| {
            fds.push(libc::pollfd {
                fd: c.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            });
            fds.len() - 1
        });
        let exec_at = self.exec_status.as_ref().map(|e| {
            fds.push(libc::pollfd {
                fd: e.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            });
            fds.len() - 1
        });
        let events_at = match (&self.events, self.pending.is_empty()) {
            (Some(ev), false) => {
                fds.push(libc::pollfd {
                    fd: ev.as_raw_fd(),
                    events: libc::POLLOUT,
                    revents: 0,
                });
                Some(fds.len() - 1)
            }
            _ => None,
        };
        let ms = timeout.map_or(-1, |t| i32::try_from(t.as_millis()).unwrap_or(i32::MAX));
        let nfds = libc::nfds_t::try_from(fds.len()).unwrap_or(0);
        // SAFETY: `fds` is a live array of `nfds` pollfds.
        let ret = unsafe { libc::poll(fds.as_mut_ptr(), nfds, ms) };
        if ret <= 0 {
            return;
        }
        if fds[0].revents != 0 {
            self.read_signals();
        }
        if let Some(i) = control_at
            && fds[i].revents != 0
        {
            self.read_control();
        }
        if let Some(i) = exec_at
            && fds[i].revents != 0
        {
            self.read_exec_status();
        }
        if let Some(i) = events_at
            && fds[i].revents != 0
        {
            self.flush();
        }
    }

    fn read_signals(&mut self) {
        let mut info: libc::signalfd_siginfo =
            // SAFETY: plain-old-data, fully overwritten by read.
            unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::signalfd_siginfo>();
        loop {
            // SAFETY: reads at most `size` bytes into `info`.
            let n = unsafe {
                libc::read(
                    self.signals.as_raw_fd(),
                    (&raw mut info).cast::<libc::c_void>(),
                    size,
                )
            };
            if usize::try_from(n).ok() != Some(size) {
                break;
            }
            match libc::c_int::try_from(info.ssi_signo).unwrap_or(0) {
                libc::SIGCHLD => {}
                libc::SIGINT => self.interrupt(),
                libc::SIGTERM => self.terminate = true,
                _ => {}
            }
        }
        self.reap();
    }

    fn read_control(&mut self) {
        let Some(control) = &self.control else {
            return;
        };
        let mut buf = [0u8; 64];
        // SAFETY: reads into a buffer we own.
        let n = unsafe {
            libc::read(
                control.as_raw_fd(),
                buf.as_mut_ptr().cast::<libc::c_void>(),
                buf.len(),
            )
        };
        if n == 0 {
            self.owner_gone();
            return;
        }
        let Ok(n) = usize::try_from(n) else {
            if last_error().kind() != std::io::ErrorKind::WouldBlock {
                self.owner_gone();
            }
            return;
        };
        for &b in &buf[..n] {
            match b {
                // Never once termination has begun: a `t` (or SIGTERM) ahead
                // of the `g` means shutdown closed admission first.
                b'g' if self.phase == Phase::Waiting && !self.terminate => self.launch(),
                b'i' => self.interrupt(),
                b't' => self.terminate = true,
                _ => {}
            }
        }
    }

    /// `review` is gone: tear down, and keep the outcome for the orphan log.
    fn owner_gone(&mut self) {
        self.owner_gone = true;
        self.control = None;
        self.terminate = true;
    }

    /// Forward the first interrupt to the provider, once, while it runs.
    fn interrupt(&mut self) {
        if self.phase != Phase::Running || self.interrupted || self.leader_status.is_some() {
            return;
        }
        if let Some((_, fd)) = &self.leader {
            self.interrupted = true;
            let _ = pidfd_signal(fd, libc::SIGINT);
        }
    }

    fn launch(&mut self) {
        match self.fork_provider() {
            Ok(()) => self.phase = Phase::Starting,
            Err(reason) => {
                self.queue(&Event::ExecFailed { reason });
                self.phase = Phase::TearingDown;
            }
        }
    }

    fn fork_provider(&mut self) -> Result<(), String> {
        let program = &self.spec.program;
        let path = if program.contains('/') {
            PathBuf::from(program)
        } else {
            which::which(program).map_err(|e| format!("{program}: {e}"))?
        };
        let to_c = |s: &str| std::ffi::CString::new(s).map_err(|e| e.to_string());
        let path_c = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())
            .map_err(|e| e.to_string())?;
        let mut argv_c = vec![to_c(program)?];
        for a in &self.spec.args {
            argv_c.push(to_c(a)?);
        }
        let mut argv: Vec<*const libc::c_char> = argv_c.iter().map(|c| c.as_ptr()).collect();
        argv.push(std::ptr::null());
        let mut status_fds = [0 as RawFd; 2];
        // SAFETY: fills two descriptors we then own.
        check(unsafe { libc::pipe2(status_fds.as_mut_ptr(), libc::O_CLOEXEC) })
            .map_err(|e| format!("exec status pipe: {e}"))?;
        // SAFETY: fresh descriptors.
        let (status_r, status_w) = unsafe {
            (
                OwnedFd::from_raw_fd(status_fds[0]),
                OwnedFd::from_raw_fd(status_fds[1]),
            )
        };
        // SAFETY: this process is single-threaded, so the child may run
        // anything; it still does only what is needed before exec.
        let pid = unsafe { libc::fork() };
        if pid < 0 {
            return Err(format!("fork: {}", last_error()));
        }
        if pid == 0 {
            // SAFETY: the child, about to exec or exit.
            unsafe {
                libc::setpgid(0, 0);
                let mut empty: libc::sigset_t = std::mem::zeroed();
                libc::sigemptyset(&mut empty);
                libc::pthread_sigmask(libc::SIG_SETMASK, &empty, std::ptr::null_mut());
                // The Rust runtime ignores SIGPIPE; a provider gets the
                // default, as it would from `std::process::Command`.
                libc::signal(libc::SIGPIPE, libc::SIG_DFL);
                libc::execv(path_c.as_ptr(), argv.as_ptr());
                let errno = *libc::__errno_location();
                let bytes = errno.to_ne_bytes();
                libc::write(
                    status_w.as_raw_fd(),
                    bytes.as_ptr().cast::<libc::c_void>(),
                    bytes.len(),
                );
                libc::_exit(127);
            }
        }
        drop(status_w);
        // The provider has its stdio; the keeper's copies would only hold its
        // pipes open past its exit, delaying `review`'s EOF and its EPIPE on a
        // prompt nobody reads.
        if let Ok(null) = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/null")
        {
            for fd in 0..3 {
                // SAFETY: replaces our own stdio with /dev/null.
                unsafe { libc::dup2(null.as_raw_fd(), fd) };
            }
        }
        let raw_pid = pid;
        let pid = u32::try_from(pid).map_err(|e| e.to_string())?;
        let pidfd = match pidfd_open(pid) {
            Ok(fd) => fd,
            Err(e) => {
                // Without a pidfd the teardown could never signal it, so it
                // must not run unsupervised. Our own unreaped child, so its
                // pid cannot have been reused: killing it by pid is safe.
                // SAFETY: plain kill of our own child; reaped by `reap`.
                unsafe { libc::kill(raw_pid, libc::SIGKILL) };
                return Err(format!("pidfd_open: {e}"));
            }
        };
        self.leader = Some((pid, pidfd));
        self.exec_status = Some(status_r);
        Ok(())
    }

    fn read_exec_status(&mut self) {
        let Some(fd) = self.exec_status.take() else {
            return;
        };
        let mut buf = [0u8; 4];
        // SAFETY: reads into a buffer we own.
        let n = unsafe { libc::read(fd.as_raw_fd(), buf.as_mut_ptr().cast::<libc::c_void>(), 4) };
        if n == 0 {
            // Closed by a successful exec.
            if let Some((pid, _)) = &self.leader {
                let pid = *pid;
                self.queue(&Event::Spawned { pid });
            }
            self.phase = Phase::Running;
        } else if n == 4 {
            let err = std::io::Error::from_raw_os_error(i32::from_ne_bytes(buf));
            self.queue(&Event::ExecFailed {
                reason: format!("{}: {err}", self.spec.program),
            });
            self.phase = Phase::TearingDown;
        } else {
            // A short or failed read: not proof of an exec either way.
            self.queue(&Event::ExecFailed {
                reason: "could not read the provider's exec status".to_string(),
            });
            self.phase = Phase::TearingDown;
        }
    }

    /// Reap every child that has exited, recording the provider's status.
    /// Returns true on `ECHILD`: no children at all.
    fn reap(&mut self) -> bool {
        loop {
            let mut status: libc::c_int = 0;
            // SAFETY: plain waitpid on our own children.
            let pid = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };
            if pid > 0 {
                let pid = u32::try_from(pid).unwrap_or(0);
                if self.leader.as_ref().is_some_and(|(l, _)| *l == pid) {
                    self.leader_status = Some(status);
                    self.queue(&Event::Exited { status });
                }
                continue;
            }
            if pid == 0 {
                return false;
            }
            return last_error().raw_os_error() == Some(libc::ECHILD);
        }
    }

    /// Walk the tree from the keeper down, admitting what can be proven ours.
    /// Returns whether every process visited had readable children. Stops at
    /// `deadline`, incomplete, so a large or growing tree cannot hold a round
    /// past the teardown's bound.
    fn discover(&mut self, deadline: Instant) -> bool {
        // Members that died since the last look are recorded, not dropped:
        // a process that dies between two rounds was still ended by us.
        self.account_dead();
        self.refresh_cmdlines(deadline);
        let mut complete = true;
        let mut stack = vec![self.me];
        let mut seen = std::collections::BTreeSet::new();
        while let Some(parent) = stack.pop() {
            if Instant::now() >= deadline {
                return false;
            }
            if !seen.insert(parent) {
                continue;
            }
            let children = match children_of(parent, deadline) {
                Ok(c) => c,
                Err(_) => {
                    complete = false;
                    continue;
                }
            };
            for child in children {
                if Instant::now() >= deadline {
                    return false;
                }
                if !self.members.contains_key(&child) && !self.admit(child, parent) {
                    continue;
                }
                stack.push(child);
            }
        }
        complete
    }

    fn admit(&mut self, child: u32, parent: u32) -> bool {
        let Ok(pidfd) = pidfd_open(child) else {
            return false;
        };
        let Some((ppid, _)) = stat_ppid_start(child) else {
            return false;
        };
        if ppid != parent || pidfd_exited(&pidfd) {
            return false;
        }
        // The keeper is alive by definition; any other parent must still be
        // the admitted process, alive after the read.
        if parent != self.me
            && !self
                .members
                .get(&parent)
                .is_some_and(|p| !pidfd_exited(&p.pidfd))
        {
            return false;
        }
        let (proc_, _) = read_cmdline(child);
        self.members.insert(
            child,
            Member {
                pidfd,
                proc_,
                termed: false,
            },
        );
        true
    }

    /// Re-read every member's command line, keeping a new one only if it is
    /// a real command line and the process was still alive after the read (so
    /// the read was this process's).
    fn refresh_cmdlines(&mut self, deadline: Instant) {
        for (pid, m) in &mut self.members {
            if Instant::now() >= deadline {
                return;
            }
            let (fresh, fallback) = read_cmdline(*pid);
            if (!fallback || m.proc_.cmdline.starts_with('[')) && !pidfd_exited(&m.pidfd) {
                m.proc_ = fresh;
            }
        }
    }

    fn leader_pid(&self) -> Option<u32> {
        self.leader.as_ref().map(|(p, _)| *p)
    }

    /// Note members that have died since the last look as killed.
    fn account_dead(&mut self) {
        let leader = self.leader_pid();
        let dead: Vec<u32> = self
            .members
            .iter()
            .filter(|(_, m)| pidfd_exited(&m.pidfd))
            .map(|(pid, _)| *pid)
            .collect();
        for pid in dead {
            if let Some(m) = self.members.remove(&pid)
                && Some(pid) != leader
            {
                if self.killed.len() < KILLED_MAX {
                    self.killed.push(m.proc_);
                } else {
                    self.killed_dropped += 1;
                }
            }
        }
    }

    /// Wait for the next event, at most one beat (and never past `until`),
    /// servicing signals, control and events. Used while SIGTERMing, where
    /// waking early only means finding the newcomers sooner.
    fn beat(&mut self, until: Instant) {
        let left = until.saturating_duration_since(Instant::now());
        let beat = Duration::from_millis(self.spec.beat_ms).min(left);
        if !beat.is_zero() {
            self.service(Some(beat));
        }
    }

    /// Pause a whole beat (never past `until`), servicing and reaping all the
    /// while. Between kill rounds: the SIGCHLD from a killed leaf must not cut
    /// short the moment its parent gets to reap it and clean up. Returns true
    /// once the tree is empty.
    fn full_beat(&mut self, until: Instant) -> bool {
        let end = (Instant::now() + Duration::from_millis(self.spec.beat_ms)).min(until);
        loop {
            if self.reap() {
                return true;
            }
            let left = end.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            self.service(Some(left));
        }
    }

    fn teardown(&mut self, term_grace: Duration) -> Outcome {
        let term_until = Instant::now() + term_grace;
        let final_at = term_until + Duration::from_millis(self.spec.kill_phase_ms);
        let mut complete;
        // SIGTERM (and SIGCONT, so a stopped process can act on it) to every
        // member as it is found, until the grace runs out.
        loop {
            if self.reap() {
                self.account_dead();
                return self.outcome(true, true);
            }
            // Whether this walk was complete matters only for the final
            // verdict, which the kill rounds below decide.
            let _ = self.discover(term_until);
            for m in self.members.values_mut().filter(|m| !m.termed) {
                let _ = pidfd_signal(&m.pidfd, libc::SIGTERM);
                let _ = pidfd_signal(&m.pidfd, libc::SIGCONT);
                m.termed = true;
            }
            self.account_dead();
            if Instant::now() >= term_until {
                break;
            }
            self.beat(term_until);
        }
        // SIGKILL leaves first for the first half of the kill phase, a full
        // beat between rounds, so a supervising ancestor (codex-linux-sandbox
        // removing the placeholder files it created in the workspace) can
        // reap and clean up before its turn. Only for half: a parent that
        // forks a new child each time its last one dies is never a leaf, and
        // leaves-first alone would kill its children forever. From then on,
        // every known member each round.
        let beat = Duration::from_millis(self.spec.beat_ms);
        let leaves_until = term_until + Duration::from_millis(self.spec.kill_phase_ms) / 2;
        loop {
            if self.reap() {
                self.account_dead();
                return self.outcome(true, true);
            }
            // Discovery stops short of the deadline, so what it found can
            // still be killed in the time left.
            complete = self.discover(final_at.checked_sub(beat * 2).unwrap_or(final_at));
            self.account_dead();
            let now = Instant::now();
            if now >= final_at {
                break;
            }
            let mut targets: Vec<u32> = if now >= leaves_until {
                Vec::new()
            } else {
                let parents: std::collections::BTreeSet<u32> = self
                    .members
                    .keys()
                    .filter_map(|pid| stat_ppid_start(*pid).map(|(ppid, _)| ppid))
                    .collect();
                self.members
                    .keys()
                    .copied()
                    .filter(|pid| !parents.contains(pid))
                    .collect()
            };
            if targets.is_empty() {
                targets = self.members.keys().copied().collect();
            }
            for pid in targets {
                if let Some(m) = self.members.get(&pid) {
                    let _ = pidfd_signal(&m.pidfd, libc::SIGKILL);
                }
            }
            if self.full_beat(final_at) {
                self.account_dead();
                return self.outcome(true, true);
            }
        }
        // At the deadline: SIGKILL everything known, then keep finding and
        // killing what appears (a child forked just before its parent died
        // is now the keeper's) for two more beats, so nothing killed in the
        // last round, or orphaned by it, is reported as surviving.
        let settle_end = final_at + beat * 2;
        loop {
            for m in self.members.values() {
                let _ = pidfd_signal(&m.pidfd, libc::SIGKILL);
            }
            if self.full_beat(settle_end) {
                self.account_dead();
                return self.outcome(true, true);
            }
            if Instant::now() >= settle_end {
                break;
            }
            complete = self.discover(settle_end);
        }
        let empty = self.reap();
        self.account_dead();
        self.outcome(empty, complete)
    }

    fn outcome(&mut self, empty: bool, complete: bool) -> Outcome {
        let survivors: Vec<Proc> = if empty {
            Vec::new()
        } else {
            self.members.values().map(|m| m.proc_.clone()).collect()
        };
        for s in &survivors {
            self.queue(&Event::Survivor(s.clone()));
        }
        Outcome {
            status: self.leader_status,
            clean: empty,
            killed: std::mem::take(&mut self.killed),
            killed_dropped: self.killed_dropped,
            survivors: survivors.len(),
            incomplete: !empty && (!complete || survivors.is_empty()),
        }
    }

    fn finish(&mut self, outcome: &Outcome) {
        self.queue(&Event::Done(outcome.clone()));
        let until = Instant::now() + Duration::from_millis(self.spec.flush_ms);
        while !self.pending.is_empty() && self.events.is_some() && Instant::now() < until {
            self.flush();
            if !self.pending.is_empty() {
                self.service(Some(until.saturating_duration_since(Instant::now())));
            }
        }
        if self.owner_gone || self.events.is_none() {
            self.log_orphaned(outcome);
        }
    }

    fn queue(&mut self, event: &Event) {
        if let Ok(mut line) = serde_json::to_vec(event) {
            line.push(b'\n');
            self.pending.extend(line);
        }
        self.flush();
    }

    /// Write what the events pipe will take now. Backpressure leaves the rest
    /// queued; only a closed reader ends delivery.
    fn flush(&mut self) {
        let Some(events) = &self.events else {
            self.pending.clear();
            return;
        };
        while !self.pending.is_empty() {
            let (head, _) = self.pending.as_slices();
            // SAFETY: writes from a slice we own.
            let n = unsafe {
                libc::write(
                    events.as_raw_fd(),
                    head.as_ptr().cast::<libc::c_void>(),
                    head.len(),
                )
            };
            match usize::try_from(n) {
                Ok(n) => {
                    self.pending.drain(..n);
                }
                Err(_) => {
                    if last_error().kind() != std::io::ErrorKind::WouldBlock {
                        // EPIPE: nobody is reading, so `review` is gone.
                        self.events = None;
                        self.pending.clear();
                        self.owner_gone();
                    }
                    return;
                }
            }
        }
    }

    /// `review` is gone, so the outcome goes to the orphan log: one line, one
    /// write call. Best-effort - nothing reads it back.
    fn log_orphaned(&self, outcome: &Outcome) {
        let Some(path) = &self.spec.orphan_log else {
            return;
        };
        let survivors: Vec<Proc> = if outcome.clean {
            Vec::new()
        } else {
            self.members.values().map(|m| m.proc_.clone()).collect()
        };
        let line = serde_json::json!({
            "epoch_secs": crate::provider::now_epoch_secs(),
            "label": self.spec.label,
            "keeper_pid": self.me,
            "outcome": outcome,
            "survivor_list": survivors,
        });
        let Ok(mut bytes) = serde_json::to_vec(&line) else {
            return;
        };
        bytes.push(b'\n');
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            use std::io::Write;
            let _ = (&file).write(&bytes);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_process_knows_its_parent_and_start_time() {
        // SAFETY: getppid has no failure mode.
        let parent = u32::try_from(unsafe { libc::getppid() }).expect("pid fits");
        let (ppid, start) = stat_ppid_start(std::process::id()).expect("own stat");
        assert_eq!(ppid, parent);
        assert!(start > 0);
    }

    #[test]
    fn a_live_process_has_an_unexited_pidfd() {
        let fd = pidfd_open(std::process::id()).expect("own pidfd");
        assert!(!pidfd_exited(&fd));
    }
}
