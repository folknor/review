//! End-to-end tests for the codex and claude runners, driven by stub providers.
//!
//! An integration test rather than a unit test because every provider runs
//! under a keeper, and the keeper is the built `review` binary: Cargo hands an
//! integration test its path (`CARGO_BIN_EXE_review`) and gives a unit test
//! nothing of the kind, so only here do the runners run under exactly the
//! keeper that ships.
//!
//! # Why a stub rather than mocks
//!
//! The behaviour under test is a *process* behaviour: codex finishing its work
//! and then refusing to exit, or wedging mid-turn, or leaving a descendant that
//! holds the stdout pipe open and ignores `SIGTERM`. None of that can be
//! exercised by faking a return value - it needs a real child process, a real
//! pipe, a real process group and a real kill. So these tests run the actual
//! `run_codex_json` against a shell script that reproduces each hang shape
//! deterministically.
//!
//! This matters because the watchdog *kills a live codex*. Before this file the
//! kill path had never run outside hand-written parse fixtures.
//!
//! # How it stays fast
//!
//! `ProviderRuntime` makes the binary, the watchdog timings, the drain grace and
//! the keeper's teardown graces injectable, so a hang that takes 3 minutes to
//! detect and 10 seconds to escalate in production resolves in under a second
//! here. Nothing about the code path itself changes.
//!
//! # Isolation
//!
//! Scratch files live under `target/test-scratch/<uuid>/` (per the project rule
//! that data lives in the repo rather than `/tmp`) and are removed on success.
//!
//! `ProviderRuntime::data_root` is also redirected there, and that is load-bearing
//! rather than tidiness: `CODEX_HOME` only redirects the *child*, so `review`'s
//! own data paths still resolved from the test process's real
//! `HOME`/`XDG_DATA_HOME`. Before the override existed, running this suite
//! deposited stub incident bundles in the operator's real
//! `~/.local/share/review/incidents`.
//!
//! Any process a stub deliberately leaves running records its pid to
//! `$LINGER_PID`, so the test can assert on it and clean it up rather than
//! leaking it into the rest of the suite.
//!
//! The generated stub is passed as data to `/bin/sh`; it is never exec'd
//! directly. In a parallel test process, another thread can fork while
//! `write_stub` still has a write descriptor open, and that child inherits the
//! descriptor until its own exec. Directly exec'ing the freshly-written stub in
//! that window fails with `ETXTBSY`. Exec'ing the system shell avoids the race
//! by construction while preserving the real child/process-group behaviour.
//! Suspicious runs consequently probe `/bin/sh --version`, not the script;
//! `contained::version_probe` must keep its stdin set to null so a shell that
//! handles an unknown option by reading stdin cannot hang the bundle write.

use anyhow::Result;
use review::provider::{
    Digest, ProviderRuntime, RunOutput, new_output_file, run_claude, run_codex_json,
};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// A stub session id. Fixed rather than random so the stub script and the
/// assertions can both name the rollout file.
const STUB_SESSION: &str = "01920000-0000-7000-8000-000000000001";

/// Per-test scratch directory, removed when the test finishes.
///
/// `Drop` also SIGKILLs any process a stub deliberately left running, unless the
/// test disarmed it by cleaning up itself. That belt-and-braces matters because
/// these fixtures leave processes alive for 30-300 seconds: if `run_stub` or any
/// assertion before the cleanup line panics, an explicit kill at the end of the
/// test body never runs, and the leftover process leaks into the rest of the
/// suite. `Drop` runs on unwind, so it covers every failure path.
struct Scratch {
    root: PathBuf,
    /// Set once a test has cleaned up its own lingering process. Suppresses the
    /// kill in `Drop`, which would otherwise be a signal aimed at a pid that has
    /// already been reaped - and could therefore hit an unrelated process that
    /// reused the number.
    linger_disarmed: std::cell::Cell<bool>,
}

impl Scratch {
    fn new() -> Self {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target/test-scratch")
            .join(review::config::generate_uuid());
        std::fs::create_dir_all(root.join("codex-home")).expect("create scratch dirs");
        std::fs::create_dir_all(root.join("project")).expect("create project dir");
        Self {
            root,
            linger_disarmed: std::cell::Cell::new(false),
        }
    }

    /// Take responsibility for the lingering process: `Drop` will not signal it.
    /// Call only after confirming it is gone.
    fn disarm_linger(&self) {
        self.linger_disarmed.set(true);
    }

    fn codex_home(&self) -> PathBuf {
        self.root.join("codex-home")
    }

    fn project(&self) -> PathBuf {
        self.root.join("project")
    }

    /// Isolated XDG data root for in-flight markers and incident bundles.
    ///
    /// Redirecting `CODEX_HOME` only redirects the *child process*; `review`'s
    /// own data paths resolve from this process's real `HOME`/`XDG_DATA_HOME`.
    /// Without this override the suite wrote stub incident bundles straight into
    /// the operator's real `~/.local/share/review/incidents`.
    fn data_root(&self) -> PathBuf {
        self.root.join("data")
    }

    /// Pid a stub recorded for a process it deliberately left running, so the
    /// test can assert on it and clean it up.
    fn linger_pid(&self) -> Option<i32> {
        std::fs::read_to_string(self.root.join("linger.pid"))
            .ok()?
            .trim()
            .parse()
            .ok()
    }

    /// Incident bundles written by a run under this scratch root.
    fn incident_dirs(&self) -> Vec<PathBuf> {
        let base = self.data_root().join("review").join("incidents");
        match std::fs::read_dir(base) {
            Ok(rd) => rd.flatten().map(|e| e.path()).collect(),
            Err(_) => Vec::new(),
        }
    }

    /// Write a stub shell script and return its path.
    ///
    /// The harness passes this path to `/bin/sh` as data; it must not exec the
    /// freshly-written file directly (see the module-level `ETXTBSY` note). The
    /// executable mode is retained only so a failed fixture is convenient to
    /// invoke by hand while debugging.
    fn write_stub(&self, body: &str) -> String {
        let path = self.root.join("codex-stub.sh");
        let script = format!("#!/bin/sh\n{body}\n");
        std::fs::write(&path, script).expect("write stub");
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("chmod stub");
        path.to_string_lossy().into_owned()
    }

    /// Path the stub is expected to write its rollout to, matching the real
    /// codex layout (`sessions/YYYY/MM/DD/rollout-<ts>-<id>.jsonl`) that
    /// `transcript::find_transcript_path` walks.
    fn rollout_path(&self) -> PathBuf {
        self.codex_home()
            .join("sessions/2026/01/01")
            .join(format!("rollout-2026-01-01T00-00-00-{STUB_SESSION}.jsonl"))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        // Reap a lingering fixture process the test did not get to clean up
        // (a panic before its cleanup line, including inside `run_stub`).
        if !self.linger_disarmed.get()
            && let Some(pid) = self.linger_pid()
        {
            // SAFETY: a pid this fixture's stub spawned and recorded. Only
            // reached when the test did *not* disarm, so it has not been reaped
            // by us and the number cannot have been recycled behind our back.
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Shell fragment: emit `thread.started` on stdout and create the rollout with
/// a `task_started`. Shared by every stub.
fn stub_preamble() -> String {
    format!(
        r#"
# Record our own pid so a test that deliberately leaves this process unreaped
# can kill its process group afterwards.
printf '%s\n' "$$" > "$CODEX_HOME/pid"
# Drain the prompt so review's stdin write completes rather than seeing EPIPE.
cat > /dev/null
ROLL_DIR="$CODEX_HOME/sessions/2026/01/01"
mkdir -p "$ROLL_DIR"
ROLL="$ROLL_DIR/rollout-2026-01-01T00-00-00-{STUB_SESSION}.jsonl"
printf '%s\n' '{{"type":"thread.started","thread_id":"{STUB_SESSION}"}}'
printf '%s\n' '{{"type":"event_msg","payload":{{"type":"task_started"}}}}' >> "$ROLL"
"#
    )
}

/// Fast timings: the whole watchdog cycle completes in well under a second,
/// while preserving the ordering the production values encode (poll interval
/// comfortably shorter than the quiet grace).
fn fast_runtime(scratch: &Scratch) -> ProviderRuntime {
    ProviderRuntime {
        // Execute the stable system shell, not the fixture file another test
        // thread may still have open for writing. The script path is argv[0]
        // from the shell's perspective; see `run_stub_with`.
        codex_binary: "/bin/sh".to_string(),
        // Replaced by `claude_runtime` for the claude tests.
        claude_command: vec!["claude-not-stubbed".to_string()],
        timings: review::watchdog::Timings {
            poll_interval: std::time::Duration::from_millis(50),
            quiet_grace: std::time::Duration::from_millis(250),
            // Mirrors the production ordering (3 min vs 15 min) for realism.
            // It is *not* required for correctness: the two verdicts are
            // mutually exclusive on whether an answer exists, so a shorter
            // stall grace cannot steal an answered run - see
            // `an_answered_run_is_stranded_even_with_a_shorter_stall_grace`.
            stall_grace: Some(std::time::Duration::from_millis(600)),
        },
        drain_grace: std::time::Duration::from_millis(300),
        data_root: Some(scratch.data_root()),
        // Never flips; a test of the signal path passes its own.
        stop: tokio::sync::watch::channel(false).1,
        teardown: fast_teardown(),
        orphan_log: Some(scratch.data_root().join("review/teardowns.jsonl")),
    }
}

/// The real keeper, paced for tests: graces short enough to keep the suite
/// fast, long enough that a cooperative process still gets a real chance to
/// honour SIGTERM first.
fn fast_teardown() -> review::contained::Settings {
    review::contained::Settings {
        keeper: PathBuf::from(env!("CARGO_BIN_EXE_review")),
        leftover_grace: std::time::Duration::from_millis(200),
        terminate_grace: std::time::Duration::from_millis(400),
        kill_phase: std::time::Duration::from_millis(600),
        beat: std::time::Duration::from_millis(20),
        startup: std::time::Duration::from_secs(10),
        flush: std::time::Duration::from_secs(2),
    }
}

/// Run the stub through the real codex path as a fresh (oneshot) run, with the
/// default fast timings.
async fn run_stub(scratch: &Scratch, script: String) -> Result<RunOutput> {
    let runtime = fast_runtime(scratch);
    run_stub_with(scratch, &script, &runtime).await
}

/// As `run_stub`, but with an explicit runtime - for tests that vary the
/// timings themselves.
async fn run_stub_with(
    scratch: &Scratch,
    script: &str,
    runtime: &ProviderRuntime,
) -> Result<RunOutput> {
    let out_file = new_output_file().expect("create -o file");
    let mut env = BTreeMap::new();
    env.insert(
        "CODEX_HOME".to_string(),
        scratch.codex_home().to_string_lossy().into_owned(),
    );
    // The real codex learns its `-o` path from argv; the stub takes it from the
    // environment so it does not have to parse arguments.
    env.insert("LAST_MSG".to_string(), out_file.clone());
    // Where a stub records the pid of a process it intentionally leaves behind.
    env.insert(
        "LINGER_PID".to_string(),
        scratch
            .root
            .join("linger.pid")
            .to_string_lossy()
            .into_owned(),
    );
    let project = scratch.project();
    run_codex_json(
        vec![
            script.to_string(),
            "exec".to_string(),
            "--json".to_string(),
            "-o".to_string(),
            out_file.clone(),
        ],
        &out_file,
        // Fresh run: the session id must be learned from the stream, which also
        // exercises the stdout scanner that feeds the watchdog.
        None,
        Some(&env),
        "review this",
        &project,
        None,
        runtime,
    )
    .await
}

/// Is `pid` in the zombie state - i.e. dead, but not yet reaped by its parent?
///
/// `/proc/<pid>/stat` is `pid (comm) state ...`, and `comm` may itself contain
/// spaces and parentheses, so the state is the first token *after the last*
/// `)`, not the third whitespace-separated field.
fn is_zombie(pid: i32) -> bool {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    let Some(after_comm) = stat.rsplit_once(')') else {
        return false;
    };
    after_comm.1.split_whitespace().next() == Some("Z")
}

/// Is `pid` still running? Used to assert that group escalation actually reaped
/// a deliberately stubborn descendant, rather than trusting that it did.
///
/// A zombie counts as terminated. `kill(pid, 0)` succeeds against zombies, and
/// these fixtures create them by design: killing the stub makes its stubborn
/// child an orphan, and whether that orphan is reaped promptly depends on
/// whether the environment's PID 1 does so. In a container that does not, the
/// SIGKILL would have worked perfectly and the test would still have failed.
fn pid_alive(pid: i32) -> bool {
    // SAFETY: signal 0 delivers nothing; it only reports existence.
    let exists = unsafe { libc::kill(pid, 0) == 0 };
    exists && !is_zombie(pid)
}

/// 1. Stranded completion: codex writes a real final answer, then hangs
///    forever. The watchdog must kill the group, recovery must surface the
///    answer, and the digest must record that *we* ended the run.
#[tokio::test]
async fn stranded_completion_is_killed_and_recovered() {
    let scratch = Scratch::new();
    let stub = scratch.write_stub(&format!(
        r#"{preamble}
printf '%s\n' '{{"type":"event_msg","payload":{{"type":"agent_message","message":"STRANDED ANSWER","phase":"final_answer"}}}}' >> "$ROLL"
printf '%s\n' '{{"type":"event_msg","payload":{{"type":"task_complete"}}}}' >> "$ROLL"
# Finished the work, now refuse to exit - the 0.147.0 hang.
sleep 3600
"#,
        preamble = stub_preamble()
    ));

    let run = run_stub(&scratch, stub).await.expect("run completes");

    assert_eq!(
        run.text, "STRANDED ANSWER",
        "the answer on disk must be recovered and reported"
    );
    let digest = run.digest.expect("codex runs carry a digest");
    assert!(
        digest.recovered_from_transcript,
        "answer came from the rollout, not the -o backstop"
    );
    assert!(
        digest
            .terminated_by_review
            .as_deref()
            .is_some_and(|why| why.contains("watchdog")),
        "digest must record the watchdog kill, got {:?}",
        digest.terminated_by_review
    );
    assert_eq!(
        run.session_id.as_deref(),
        Some(STUB_SESSION),
        "session id must survive the kill"
    );
}

/// The two verdicts are mutually exclusive on whether an answer exists, so
/// `stall_grace` shorter than `quiet_grace` is not a misconfiguration: an
/// answered run matches neither branch until `quiet_grace` and is then
/// `Stranded`, with its answer recovered. Pinned because the alternative -
/// the stall branch stealing an answered run and reporting a recovered result
/// as a failure - is silent and plausible-looking, the worst kind of wrong.
#[tokio::test]
async fn an_answered_run_is_stranded_even_with_a_shorter_stall_grace() {
    let scratch = Scratch::new();
    let stub = scratch.write_stub(&format!(
        r#"{preamble}
printf '%s\n' '{{"type":"event_msg","payload":{{"type":"agent_message","message":"ANSWER","phase":"final_answer"}}}}' >> "$ROLL"
sleep 3600
"#,
        preamble = stub_preamble()
    ));

    let mut runtime = fast_runtime(&scratch);
    // Inverted relative to production: the stall grace elapses first.
    runtime.timings.quiet_grace = std::time::Duration::from_millis(500);
    runtime.timings.stall_grace = Some(std::time::Duration::from_millis(100));

    let run = run_stub_with(&scratch, &stub, &runtime)
        .await
        .expect("run completes");

    let digest = run.digest.expect("digest");
    let why = digest.terminated_by_review.unwrap_or_default();
    assert!(
        why.contains("stranded"),
        "an answered run must be stranded, not stalled; got {why:?}"
    );
    assert!(
        !why.contains("stall timeout"),
        "the stall branch must not claim a run that produced an answer"
    );
    assert_eq!(run.text, "ANSWER", "the answer must still be recovered");
}

/// 2. Mid-turn wedge: codex goes silent having written only interim commentary,
///    never a final answer. The stall timeout must kill it, leave an incident
///    bundle, record the silence duration and last event, and - critically -
///    recover *no* answer, so the run reads as the failure it is.
#[tokio::test]
async fn a_mid_turn_wedge_trips_the_stall_timeout() {
    let scratch = Scratch::new();
    let stub = scratch.write_stub(&format!(
        r#"{preamble}
# An interim note only. codex tags these "commentary"; treating one as a result
# would report "I am now doing X" as the review.
printf '%s\n' '{{"type":"event_msg","payload":{{"type":"agent_message","message":"I am now regenerating the pilot.","phase":"commentary"}}}}' >> "$ROLL"
printf '%s\n' '{{"type":"response_item","payload":{{"type":"custom_tool_call","name":"exec","call_id":"c1","input":"{{}}"}}}}' >> "$ROLL"
# Wedged mid-tool-call: silent from here, exactly like the field rollout that
# stopped dead with a background cell still running.
sleep 3600
"#,
        preamble = stub_preamble()
    ));

    let run = run_stub(&scratch, stub).await.expect("run completes");

    let digest = run.digest.expect("digest");
    assert!(
        digest
            .terminated_by_review
            .as_deref()
            .is_some_and(|why| why.contains("stall timeout")),
        "the stall timeout must be what ended this run, got {:?}",
        digest.terminated_by_review
    );
    // The whole point of the answer gate: nothing was produced, so nothing may
    // be reported as a result.
    assert!(
        !digest.captured && !digest.recovered_from_transcript,
        "a wedged run must not report an answer (captured={}, recovered={})",
        digest.captured,
        digest.recovered_from_transcript
    );
    assert!(
        !run.text.contains("regenerating the pilot"),
        "interim commentary must not be surfaced as the result, got {:?}",
        run.text
    );
    // Forensics that make a misfire on a healthy run diagnosable.
    assert!(
        digest.quiet_secs.is_some(),
        "the observed silence duration must be recorded"
    );
    assert_eq!(
        digest.last_rollout_event.as_deref(),
        Some("response_item/custom_tool_call"),
        "the last thing codex was doing must be recorded"
    );
    assert_eq!(
        scratch.incident_dirs().len(),
        1,
        "a stalled run must leave exactly one forensic bundle"
    );
}

/// The stall timeout is disableable, because it rests on an empirical codex
/// property rather than a documented contract. With it off, the same wedge must
/// hang rather than be killed - proving the switch actually reaches the poller.
#[tokio::test]
async fn a_disabled_stall_timeout_never_fires() {
    let scratch = Scratch::new();
    let stub = scratch.write_stub(&format!(
        r#"{preamble}
printf '%s\n' '{{"type":"event_msg","payload":{{"type":"agent_message","message":"working","phase":"commentary"}}}}' >> "$ROLL"
sleep 3600
"#,
        preamble = stub_preamble()
    ));

    let mut runtime = fast_runtime(&scratch);
    runtime.timings.stall_grace = None;

    // Give the poller many times the stall grace it *would* have used. The run
    // must still be going: with the branch disabled, nothing can end it.
    let outcome = tokio::time::timeout(
        std::time::Duration::from_millis(2_000),
        run_stub_with(&scratch, &stub, &runtime),
    )
    .await;
    assert!(
        outcome.is_err(),
        "with the stall timeout disabled, a wedged run must not be killed"
    );

    // Nothing reaped the stub, so clean it up via its own process group. The
    // pid is the group leader (`process_group(0)` at spawn).
    let stub_pid = std::fs::read_to_string(scratch.codex_home().join("pid"))
        .ok()
        .and_then(|s| s.trim().parse::<i32>().ok());
    if let Some(pid) = stub_pid {
        // SAFETY: negative pid addresses the group this test's stub leads.
        unsafe { libc::kill(-pid, libc::SIGKILL) };
    }
}

/// 3. A run that keeps writing to its rollout must never be touched, however
///    long it goes on.
///
///    The stub writes its final answer *early* and then keeps working for well
///    past the quiet grace. That is the important shape: the answer gate alone
///    would happily kill this run, so the only thing protecting it is the
///    advancement check. It also mirrors real codex, which appends trailing
///    micro-turns after the substantive answer.
#[tokio::test]
async fn an_advancing_rollout_is_never_killed() {
    let scratch = Scratch::new();
    let stub = scratch.write_stub(&format!(
        r#"{preamble}
# The answer exists from here on, so the answer gate is satisfied for the whole
# remainder of the run.
printf '%s\n' '{{"type":"event_msg","payload":{{"type":"agent_message","message":"REAL ANSWER","phase":"final_answer"}}}}' >> "$ROLL"
# Now keep working, well past quiet_grace, exactly as a live codex does (it
# cannot stay silent for long). Killing here would be a false positive.
i=0
while [ $i -lt 12 ]; do
  printf '%s\n' '{{"type":"event_msg","payload":{{"type":"token_count","info":{{}}}}}}' >> "$ROLL"
  sleep 0.1
  i=$((i+1))
done
printf '%s\n' '{{"type":"event_msg","payload":{{"type":"task_complete"}}}}' >> "$ROLL"
printf 'REAL ANSWER' > "$LAST_MSG"
exit 0
"#,
        preamble = stub_preamble()
    ));

    let run = run_stub(&scratch, stub).await.expect("run completes");

    let digest = run.digest.expect("digest");
    assert!(
        digest.terminated_by_review.is_none(),
        "an active run must never be killed even once its answer exists, got {:?}",
        digest.terminated_by_review
    );
    assert_eq!(
        digest.exit_code,
        Some(0),
        "stub must have exited on its own, not been killed"
    );
    assert!(
        digest.signal.is_none(),
        "no signal: nothing killed this run, got {:?}",
        digest.signal
    );
    assert!(
        digest.captured,
        "the stub wrote the -o backstop, so this is a clean capture"
    );
    assert_eq!(run.text, "REAL ANSWER");
}

/// 4a. Pipe retention *only*: a descendant outlives its parent and holds the
///     stdout pipe open, so EOF never arrives. The parent exits normally, so
///     nothing is killed - this isolates "reaping is authoritative, and output
///     captured before the drain grace expired is preserved" from any question
///     of signal handling.
///
///     The descendant is killed explicitly at the end rather than left to time
///     out, so the test does not leak a process into the rest of the suite.
#[tokio::test]
async fn a_pipe_retaining_child_does_not_block_reaping_or_lose_output() {
    let scratch = Scratch::new();
    let stub = scratch.write_stub(&format!(
        r#"{preamble}
printf '%s\n' '{{"type":"event_msg","payload":{{"type":"agent_message","message":"PARTIAL","phase":"final_answer"}}}}' >> "$ROLL"
# A non-JSON stdout line, which the digest surfaces as a log line. It is written
# *before* the pipe is stranded, so it is the evidence that output captured
# ahead of the drain-grace expiry is preserved rather than discarded.
printf '%s\n' 'ERROR: codex log line before the hang'
# A grandchild that keeps stdout open well past the drain grace. It inherits
# stdout, so EOF cannot arrive while it lives. Its pid is recorded so the test
# can clean it up instead of leaking it into the rest of the suite.
sleep 30 &
echo $! > "$LINGER_PID"
# Parent exits at once: review reaps it while the pipe is still held open.
exit 0
"#,
        preamble = stub_preamble()
    ));

    let started = std::time::Instant::now();
    let run = run_stub(&scratch, stub).await.expect("run completes");
    let elapsed = started.elapsed();

    // The run must not block on the grandchild's 30s sleep.
    assert!(
        elapsed < std::time::Duration::from_secs(10),
        "reaping must not wait for pipe EOF; took {elapsed:?}"
    );
    let digest = run.digest.expect("digest");
    assert_eq!(digest.exit_code, Some(0), "the stub itself exited cleanly");
    // The heart of this test: stdout never reached EOF, so the reader was
    // aborted on the grace expiry. What it had already buffered must survive.
    // An earlier version returned an empty buffer here, silently discarding the
    // NDJSON stream, the log lines and the forensic `stdout.jsonl` - in exactly
    // the scenario the code exists to diagnose.
    assert!(
        digest
            .log_lines
            .iter()
            .any(|l| l.contains("codex log line before the hang")),
        "output captured before the drain grace expired must be preserved, got {:?}",
        digest.log_lines
    );
    // The answer was written to the rollout before the hang, so recovery still
    // works even though the stream was cut short.
    assert_eq!(run.text, "PARTIAL");
    assert!(
        scratch.rollout_path().exists(),
        "sanity: the stub wrote its rollout"
    );

    // The pipe-holder is a leftover: the keeper ended it before the run
    // returned, and it is recorded, not reported.
    let lingering = scratch
        .linger_pid()
        .expect("stub recorded its background pid");
    let gone = !pid_alive(lingering);
    if gone {
        scratch.disarm_linger();
    }
    assert!(
        gone,
        "the keeper must end the pipe-holder before the run returns"
    );
    assert!(run.leftovers.is_clean(), "{:?}", run.leftovers);
    assert!(
        run.leftovers
            .killed
            .iter()
            // By pid: its recorded command line is whatever it was when last
            // seen alive, and a keeper that catches it between fork and exec
            // ends it there, still a copy of the stub shell (or `[sleep]`
            // mid-exec).
            .any(|p| u32::try_from(lingering).is_ok_and(|l| l == p.pid)),
        "the killed leftover is named in the record: {:?}",
        run.leftovers
    );
}

/// 4b. Escalation: a descendant that ignores `SIGTERM` while its parent hangs
///     after producing an answer. The watchdog fires, the keeper SIGTERMs the
///     run (which the descendant ignores) and escalates to `SIGKILL`. Asserts
///     the descendant is actually gone when the run returns, rather than
///     assuming the signal landed.
///
///     Split from 4a deliberately: that test exercises a leftover that dies on
///     its first SIGTERM after the provider exited on its own, this one a
///     terminate that has to escalate.
#[tokio::test]
async fn group_escalation_kills_a_sigterm_ignoring_descendant() {
    let scratch = Scratch::new();
    let stub = scratch.write_stub(&format!(
        r#"{preamble}
printf '%s\n' '{{"type":"event_msg","payload":{{"type":"agent_message","message":"ANSWER","phase":"final_answer"}}}}' >> "$ROLL"
# A descendant that refuses SIGTERM outright. Only the keeper's SIGKILL
# escalation can reap it.
sh -c 'trap "" TERM; echo $$ > "$LINGER_PID"; sleep 300' &
# Parent hangs too, so the watchdog is what ends this run.
sleep 300
"#,
        preamble = stub_preamble()
    ));

    let run = run_stub(&scratch, stub).await.expect("run completes");

    let digest = run.digest.expect("digest");
    assert!(
        digest
            .terminated_by_review
            .as_deref()
            .is_some_and(|why| why.contains("watchdog")),
        "the watchdog must be what ended this run, got {:?}",
        digest.terminated_by_review
    );

    // The keeper is reaped only after its teardown, so the descendant is gone
    // by the time the run returns.
    let stubborn = scratch
        .linger_pid()
        .expect("stub recorded its background pid");
    let reaped = !pid_alive(stubborn);
    // Disarm before asserting: if the escalation did *not* work, this process is
    // still alive with a 300-second sleep ahead of it, and the assertion below
    // would unwind past any cleanup written after it. `Scratch::drop` kills it
    // on the way out - which is exactly the case the drop guard exists for, so
    // the guard must stay armed here.
    if reaped {
        scratch.disarm_linger();
    }
    assert!(
        reaped,
        "a SIGTERM-ignoring descendant must be reaped by the SIGKILL escalation"
    );
    assert!(run.leftovers.is_clean(), "{:?}", run.leftovers);

    // A killed run is suspicious, so it must leave a forensic bundle - in the
    // scratch data root, never the operator's real one.
    assert_eq!(
        scratch.incident_dirs().len(),
        1,
        "a watchdog kill must write exactly one incident bundle"
    );
}

/// 5. A refused turn: codex states why it stopped and exits non-zero, having
///    produced only interim commentary. The real shape, from a run whose
///    recursion-depth hardening tripped the upstream cybersecurity classifier -
///    two `agent_message`s of working commentary, then `error` + `turn.failed`
///    carrying the same message, then exit 1 with no `final_answer` on disk.
///
///    Before `turn_error`, both events were dropped on the floor: the run was
///    reported as "died without a final answer", filed as a mystery death, and
///    auto-resumed straight into the identical refusal - which is why every
///    occurrence in the field left *two* incident bundles seconds apart.
#[tokio::test]
async fn a_stated_turn_failure_is_surfaced_not_reported_as_a_mystery_death() {
    let scratch = Scratch::new();
    const REFUSAL: &str = "This content was flagged for possible cybersecurity risk. \
If this seems wrong, try rephrasing your request.";
    let stub = scratch.write_stub(&format!(
        r#"{preamble}
# Interim commentary only - no final_answer is ever written to the rollout,
# which is what makes this indistinguishable from a mid-turn death without the
# error events.
printf '%s\n' '{{"type":"item.completed","item":{{"type":"agent_message","text":"I am now mapping the parser tests."}}}}'
printf '%s\n' '{{"type":"event_msg","payload":{{"type":"agent_message","message":"I am now mapping the parser tests.","phase":"commentary"}}}}' >> "$ROLL"
# Codex says why it is stopping, as a pair of events carrying the same message.
printf '%s\n' '{{"type":"error","message":"{refusal}"}}'
printf '%s\n' '{{"type":"turn.failed","error":{{"message":"{refusal}"}}}}'
exit 1
"#,
        preamble = stub_preamble(),
        refusal = REFUSAL
    ));

    let run = run_stub(&scratch, stub).await.expect("run completes");
    let digest = run.digest.expect("digest");

    // The point of the fix: codex's stated reason is carried, not discarded.
    assert_eq!(
        digest.turn_error.as_deref(),
        Some(REFUSAL),
        "the turn.failed / error message must be captured"
    );
    // The surrounding failure signature is unchanged - this still *is* a run
    // that produced no answer, and must keep reporting as one.
    assert_eq!(digest.exit_code, Some(1));
    assert!(!digest.captured, "no -o file was written");
    assert!(
        !digest.recovered_from_transcript,
        "there is no final_answer to recover - a refusal is not a truncation"
    );
    // It is persisted, so refusals stay greppable in the sidecar and audit logs.
    assert_eq!(digest.summary().turn_error.as_deref(), Some(REFUSAL));

    // The interim note is still returned (it is all there is), but the reported
    // text must not be the bare "died without a final answer" fabrication.
    assert!(
        run.text.contains("mapping the parser tests"),
        "the interim commentary is still surfaced, got {:?}",
        run.text
    );

    // A refusal is still suspicious enough to bundle - the evidence is worth
    // keeping - but the bundle now carries the reason on its face, so
    // `review incidents` can say what happened instead of guessing "died".
    let dirs = scratch.incident_dirs();
    assert_eq!(dirs.len(), 1, "a refused run still writes one bundle");
    let meta = std::fs::read_to_string(dirs[0].join("meta.json")).expect("meta.json");
    assert!(
        meta.contains("This content was flagged"),
        "meta.json must record the stated reason, got {meta}"
    );
}

/// A stub that answers `SIGINT` the way `codex exec` does: it aborts the turn
/// and exits 1. `before` runs first - e.g. writing a final answer.
fn interruptible_stub(scratch: &Scratch, before: &str) -> String {
    scratch.write_stub(&format!(
        r#"{preamble}
printf '%s\n' '{{"type":"event_msg","payload":{{"type":"agent_message","message":"I am now working.","phase":"commentary"}}}}' >> "$ROLL"
{before}
trap 'printf "%s\n" "{{\"type\":\"event_msg\",\"payload\":{{\"type\":\"turn_aborted\",\"reason\":\"interrupted\"}}}}" >> "$ROLL"; exit 1' INT
# Short sleeps, so the trap runs promptly: sh defers it until the foreground
# command returns.
while :; do sleep 0.05; done
"#,
        preamble = stub_preamble()
    ))
}

/// Run `stub` as the owner and `interrupt_cmd::interrupt` against it, the way
/// `review interrupt` would from another process. The owner's sidecar row -
/// which `run_codex_json` does not write itself - is appended when its run
/// returns, as the fan-out task does.
async fn interrupt_stub(
    scratch: &Scratch,
    stub: &str,
) -> (RunOutput, Result<review::interrupt_cmd::Outcome>) {
    let mut runtime = fast_runtime(scratch);
    // The stub is silent while it waits to be interrupted; the stall timeout
    // must not be what ends it.
    runtime.timings.stall_grace = None;
    let data_root = scratch.data_root();
    let rows = std::sync::Mutex::new(Vec::<review::sessions::SessionRecord>::new());

    let owner = async {
        let run = run_stub_with(scratch, stub, &runtime)
            .await
            .expect("run completes");
        let row = serde_json::json!({
            "timestamp": "t", "epoch_secs": 1, "project": "/p", "hostname": "h",
            "audit_id": "a", "provider": "codex", "archetype": "bare",
            "session_id": run.session_id.clone().expect("session id"),
            "operator_prompt": "", "assembled_prompt": "", "review_version": "0",
            "digest": run.digest.as_ref().map(Digest::summary),
        });
        rows.lock()
            .expect("rows")
            .push(serde_json::from_value(row).expect("row parses"));
        run
    };
    let interrupter = async {
        // `review interrupt` refuses a run whose pid is not offered yet, so
        // wait for it the way an operator would retry.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while review::inflight::live_marker(STUB_SESSION, Some(&data_root))
            .and_then(|m| m.child_pid)
            .is_none()
        {
            assert!(
                std::time::Instant::now() < deadline,
                "codex's pid never offered"
            );
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        review::interrupt_cmd::interrupt(STUB_SESSION, Some(&data_root), || {
            rows.lock().expect("rows").clone()
        })
        .await
    };
    tokio::join!(owner, interrupter)
}

/// `review interrupt` end to end, against a run that has no answer yet. From
/// the exit status alone this is a mid-turn death; the request is what must
/// turn it into an interrupt: flagged on the digest (so the fan-out does not
/// auto-resume past it), no incident bundle, the request consumed, and the verb
/// reporting an interrupted session.
#[tokio::test]
async fn an_operator_interrupt_is_not_reported_as_a_death() {
    let scratch = Scratch::new();
    let stub = interruptible_stub(&scratch, "");
    let (run, outcome) = interrupt_stub(&scratch, &stub).await;
    let outcome = outcome.expect("the verb succeeds");
    assert!(outcome.interrupted, "the verb reports the interrupt");
    let digest = run.digest.expect("digest");

    assert!(digest.interrupted, "the digest must record the interrupt");
    assert!(digest.summary().interrupted, "and persist it");
    assert!(
        digest
            .terminated_by_review
            .as_deref()
            .is_some_and(|why| why.contains("operator interrupt")),
        "got {:?}",
        digest.terminated_by_review
    );
    assert_eq!(digest.exit_code, Some(1));
    assert_eq!(run.session_id.as_deref(), Some(STUB_SESSION));
    assert!(
        scratch.incident_dirs().is_empty(),
        "an interrupt is explained, so it is not an incident"
    );
    assert!(
        !review::inflight::take_interrupt_request(STUB_SESSION, Some(&scratch.data_root())),
        "the run consumed the request"
    );
}

/// A signalled `review` (an orchestrator stopping its background task) ends the
/// turn the way `review interrupt` does instead of killing codex: the run comes
/// back interrupted, with its session id, so it can be recorded and continued.
#[tokio::test]
async fn a_stop_signal_ends_the_turn_as_an_interrupt() {
    let scratch = Scratch::new();
    let stub = interruptible_stub(&scratch, "");
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let mut runtime = fast_runtime(&scratch);
    runtime.timings.stall_grace = None;
    runtime.stop = stop_rx;
    let data_root = scratch.data_root();

    let stopper = async {
        // As in production, the SIGINT path is only taken once codex has
        // spoken; before that the group is killed instead.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while review::inflight::live_marker(STUB_SESSION, Some(&data_root))
            .and_then(|m| m.child_pid)
            .is_none()
        {
            assert!(
                std::time::Instant::now() < deadline,
                "codex's pid never offered"
            );
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        stop_tx.send_replace(true);
    };
    let (run, ()) = tokio::join!(run_stub_with(&scratch, &stub, &runtime), stopper);
    let run = run.expect("run completes");
    let digest = run.digest.expect("digest");

    assert!(digest.interrupted, "the digest must record the interrupt");
    assert_eq!(
        digest.exit_code,
        Some(1),
        "codex answered SIGINT, not a kill"
    );
    assert!(
        digest
            .terminated_by_review
            .as_deref()
            .is_some_and(|why| why.contains("signalled")),
        "got {:?}",
        digest.terminated_by_review
    );
    assert_eq!(run.session_id.as_deref(), Some(STUB_SESSION));
    assert!(scratch.incident_dirs().is_empty());

    // Once stopped, nothing new launches.
    let refused = run_stub_with(&scratch, &stub, &runtime).await;
    assert!(
        refused.is_err_and(|e| e.to_string().contains("signalled to stop")),
        "a launch after the stop must be refused"
    );
}

/// A request is not an interrupt when the run already has its answer - codex
/// finished before the signal landed, or answered and then took the signal.
/// Calling that interrupted would report a completed run as cut short.
#[tokio::test]
async fn a_run_that_already_answered_is_not_marked_interrupted() {
    let scratch = Scratch::new();
    let stub = interruptible_stub(&scratch, r#"printf '%s' 'THE ANSWER' > "$LAST_MSG""#);
    let (run, outcome) = interrupt_stub(&scratch, &stub).await;

    let digest = run.digest.expect("digest");
    assert!(digest.captured, "the answer was written before the signal");
    assert!(!digest.interrupted, "so the run is not an interrupt");
    assert!(digest.terminated_by_review.is_none());
    assert_eq!(run.text, "THE ANSWER");
    assert!(
        !outcome.expect("the verb succeeds").interrupted,
        "and the verb says the run ended first"
    );
}

// ---------------------------------------------------------------------------
// claude runner, against a stub `claude`
//
// The same process behaviours as codex - signals, process groups, the in-flight
// marker, interrupt requests - driven through the real `run_claude`. The stub
// speaks claude's `stream-json`: an `init` event, then a `result`.
// ---------------------------------------------------------------------------

/// Shell fragment every claude stub starts with. Records its pid, environment
/// and arguments under `$STUB_DIR`, learns the session id from its argv as
/// claude does (`--session-id` fresh, `--resume` otherwise), and drains the
/// prompt. Prints nothing: each stub decides when claude "starts".
const CLAUDE_STUB_PREAMBLE: &str = r#"
printf '%s\n' "$$" > "$STUB_DIR/pid"
env > "$STUB_DIR/env"
printf '%s\n' "$*" > "$STUB_DIR/args"
SID=""
prev=""
for a in "$@"; do
  if [ "$prev" = "--session-id" ] || [ "$prev" = "--resume" ]; then SID="$a"; fi
  prev="$a"
done
cat > /dev/null
"#;

/// Shell line printing claude's first event, which carries the session id.
const CLAUDE_STUB_INIT: &str =
    r#"printf '%s\n' "{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"$SID\"}""#;

/// Shell line printing a result event for this session.
fn claude_stub_result(is_error: bool, extra: &str) -> String {
    format!(
        r#"printf '%s\n' "{{\"type\":\"result\",\"is_error\":{is_error},\"session_id\":\"$SID\",\"num_turns\":1{extra}}}""#
    )
}

/// Shell fragment: on SIGINT, end the turn as claude does - print an aborted
/// result and exit 0 - then wait for it.
fn claude_stub_await_sigint() -> String {
    format!(
        r#"trap '{result}; exit 0' INT
while :; do sleep 0.05; done"#,
        result = claude_stub_result(
            true,
            r#",\"subtype\":\"error_during_execution\",\"terminal_reason\":\"aborted_tools\""#
        )
    )
}

/// Run `body` as claude through the real `run_claude`: a fresh run, or a resume
/// of `resume`.
async fn run_claude_stub(
    scratch: &Scratch,
    body: &str,
    runtime: &ProviderRuntime,
    resume: Option<&str>,
) -> Result<RunOutput> {
    let script = scratch.write_stub(&format!("{CLAUDE_STUB_PREAMBLE}\n{body}"));
    let mut runtime = runtime.clone();
    runtime.claude_command = vec!["/bin/sh".to_string(), script];
    let mut env = BTreeMap::new();
    env.insert(
        "STUB_DIR".to_string(),
        scratch.root.to_string_lossy().into_owned(),
    );
    run_claude(
        resume.unwrap_or(""),
        None,
        None,
        Some(&env),
        "review this",
        &scratch.project(),
        resume.is_none(),
        None,
        &runtime,
    )
    .await
}

/// The session id of the one claude run in flight under `scratch`, once its
/// pid is offered for signalling - which is what `review interrupt` waits for.
async fn claude_signalable(scratch: &Scratch) -> String {
    let data_root = scratch.data_root();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if let Some(m) = review::inflight::read_live(Some(&data_root))
            .into_iter()
            .find(|m| m.provider == "claude" && m.child_pid.is_some())
        {
            return m.session_id;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "claude's pid never offered"
        );
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
}

/// Run `body` as the owner and `review interrupt` against it from "another
/// process", appending the owner's sidecar row when its run returns, as the
/// fan-out task does. The interrupt is sent once claude's pid is offered.
async fn claude_interrupt_stub(
    scratch: &Scratch,
    body: &str,
) -> (RunOutput, Result<review::interrupt_cmd::Outcome>) {
    claude_interrupt_stub_at(scratch, body, true).await
}

/// As `claude_interrupt_stub`, but `after_pid: false` sends the interrupt as
/// soon as the run is marked in flight - before claude's first event, as a
/// second `review message` does when its launch lock hands over.
async fn claude_interrupt_stub_at(
    scratch: &Scratch,
    body: &str,
    after_pid: bool,
) -> (RunOutput, Result<review::interrupt_cmd::Outcome>) {
    let runtime = fast_runtime(scratch);
    let data_root = scratch.data_root();
    let rows = std::sync::Mutex::new(Vec::<review::sessions::SessionRecord>::new());
    let owner = async {
        let run = run_claude_stub(scratch, body, &runtime, None)
            .await
            .expect("run completes");
        let row = serde_json::json!({
            "timestamp": "t", "epoch_secs": 1, "project": "/p", "hostname": "h",
            "audit_id": "a", "provider": "claude", "archetype": "bare",
            "session_id": run.session_id.clone().expect("session id"),
            "operator_prompt": "", "assembled_prompt": "", "review_version": "0",
            "digest": run.digest.as_ref().map(Digest::summary),
        });
        rows.lock()
            .expect("rows")
            .push(serde_json::from_value(row).expect("row parses"));
        run
    };
    let interrupter = async {
        let sid = if after_pid {
            claude_signalable(scratch).await
        } else {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            loop {
                if let Some(m) = review::inflight::read_live(Some(&data_root)).pop() {
                    assert!(m.child_pid.is_none(), "the stub has not spoken yet");
                    break m.session_id;
                }
                assert!(std::time::Instant::now() < deadline, "no marker");
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        };
        review::interrupt_cmd::interrupt(&sid, Some(&data_root), || {
            rows.lock().expect("rows").clone()
        })
        .await
    };
    tokio::join!(owner, interrupter)
}

#[tokio::test]
async fn a_claude_run_launches_with_background_off_and_reports_its_answer() {
    let scratch = Scratch::new();
    let body = format!(
        "{CLAUDE_STUB_INIT}\n{}",
        claude_stub_result(false, r#",\"result\":\"THE ANSWER\""#)
    );
    let run = run_claude_stub(&scratch, &body, &fast_runtime(&scratch), None)
        .await
        .expect("run completes");
    assert_eq!(run.text, "THE ANSWER");
    assert!(run.digest.is_none(), "a clean answer carries no digest");
    let args = std::fs::read_to_string(scratch.root.join("args")).expect("args");
    let sid = run.session_id.expect("a fresh run reports its session");
    assert!(
        args.contains(&format!("--session-id {sid}")),
        "the id recorded is the one claude was given: {args}"
    );
    assert!(
        args.contains("--output-format stream-json --verbose"),
        "{args}"
    );
    let env = std::fs::read_to_string(scratch.root.join("env")).expect("env");
    for var in [
        "CLAUDE_CODE_DISABLE_BACKGROUND_TASKS=1",
        "BASH_DEFAULT_TIMEOUT_MS=1200000",
        "BASH_MAX_TIMEOUT_MS=1200000",
    ] {
        assert!(env.lines().any(|l| l == var), "{var} must reach claude");
    }
    assert!(
        review::inflight::read_live(Some(&scratch.data_root())).is_empty(),
        "the in-flight marker goes when the run does"
    );
}

#[tokio::test]
async fn review_interrupt_ends_a_claude_turn_and_keeps_its_session() {
    let scratch = Scratch::new();
    let body = format!("{CLAUDE_STUB_INIT}\n{}", claude_stub_await_sigint());
    let (run, outcome) = claude_interrupt_stub(&scratch, &body).await;
    let outcome = outcome.expect("the verb succeeds");
    assert!(outcome.interrupted, "the verb reports the interrupt");
    assert_eq!(outcome.provider, "claude");
    let digest = run.digest.expect("digest");
    assert!(digest.interrupted);
    assert!(
        digest.turn_error.is_none(),
        "claude's complaint about the abort is not a stated failure"
    );
    assert_eq!(digest.exit_code, Some(0), "claude answered SIGINT itself");
    assert!(
        digest
            .terminated_by_review
            .as_deref()
            .is_some_and(|why| why.contains("operator interrupt")),
        "got {:?}",
        digest.terminated_by_review
    );
    assert!(run.text.contains("interrupted"), "got {:?}", run.text);
    let sid = run.session_id.expect("the session survives the interrupt");
    assert!(
        !review::inflight::take_interrupt_request(&sid, Some(&scratch.data_root())),
        "the run consumed the request"
    );
}

#[tokio::test]
async fn review_interrupt_waits_for_a_claude_that_has_not_spoken_yet() {
    // A second `review message` meets a run in the second after it launched,
    // before claude's first event offers its pid. Refusing then lost the
    // message; the verb waits for the pid instead.
    let scratch = Scratch::new();
    let body = format!(
        "sleep 1\n{CLAUDE_STUB_INIT}\n{}",
        claude_stub_await_sigint()
    );
    let (run, outcome) = claude_interrupt_stub_at(&scratch, &body, false).await;
    assert!(
        outcome
            .expect("the verb waits rather than refusing")
            .interrupted,
        "and then interrupts"
    );
    assert!(run.digest.expect("digest").interrupted);
}

#[tokio::test]
async fn a_claude_answer_that_beats_the_interrupt_stands() {
    let scratch = Scratch::new();
    let body = format!(
        "{CLAUDE_STUB_INIT}\n{}\ntrap 'exit 0' INT\nwhile :; do sleep 0.05; done",
        claude_stub_result(false, r#",\"result\":\"THE ANSWER\""#)
    );
    let (run, outcome) = claude_interrupt_stub(&scratch, &body).await;
    assert_eq!(run.text, "THE ANSWER");
    assert!(run.digest.is_none_or(|d| !d.interrupted));
    assert!(
        !outcome.expect("the verb succeeds").interrupted,
        "the verb says the run ended first"
    );
}

#[tokio::test]
async fn a_stop_signal_ends_a_started_claude_turn_as_an_interrupt() {
    let scratch = Scratch::new();
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let mut runtime = fast_runtime(&scratch);
    runtime.stop = stop_rx;
    let body = format!("{CLAUDE_STUB_INIT}\n{}", claude_stub_await_sigint());
    let stopper = async {
        claude_signalable(&scratch).await;
        stop_tx.send_replace(true);
    };
    let (run, ()) = tokio::join!(run_claude_stub(&scratch, &body, &runtime, None), stopper);
    let run = run.expect("run completes");
    let digest = run.digest.expect("digest");
    assert!(digest.interrupted);
    assert_eq!(digest.exit_code, Some(0), "SIGINT, not a group kill");
    assert!(
        digest
            .terminated_by_review
            .as_deref()
            .is_some_and(|why| why.contains("signalled"))
    );
    assert!(run.session_id.is_some());

    // Once stopped, nothing new launches - a claude run waiting on its
    // stagger or the lock must not start only to be killed.
    let refused = run_claude_stub(&scratch, &body, &runtime, None).await;
    assert!(
        refused.is_err_and(|e| e.to_string().contains("not launched")),
        "a launch after the stop must be refused"
    );
}

#[tokio::test]
async fn a_claude_stopped_before_its_first_event_names_no_session() {
    // Killed before claude could have written a session: recording one would
    // print a resume command for a session that does not exist.
    let scratch = Scratch::new();
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let mut runtime = fast_runtime(&scratch);
    runtime.stop = stop_rx;
    let data_root = scratch.data_root();
    let stopper = async {
        // The marker goes up at spawn; claude never speaks, so no pid is offered.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while review::inflight::read_live(Some(&data_root)).is_empty() {
            assert!(std::time::Instant::now() < deadline, "no marker");
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        stop_tx.send_replace(true);
    };
    let body = "while :; do sleep 0.05; done";
    let (run, ()) = tokio::join!(run_claude_stub(&scratch, body, &runtime, None), stopper);
    let Err(err) = run else {
        panic!("a run stopped before it started has no session to report");
    };
    assert!(err.to_string().contains("not started"), "{err}");
    assert!(review::inflight::read_live(Some(&data_root)).is_empty());
}

#[tokio::test]
async fn a_claude_that_dies_after_starting_keeps_its_session() {
    // Killed from outside or crashed after its first event: no result, but the
    // session exists and is the turn most worth resuming.
    let scratch = Scratch::new();
    let body = format!("{CLAUDE_STUB_INIT}\nexit 3");
    let run = run_claude_stub(&scratch, &body, &fast_runtime(&scratch), None)
        .await
        .expect("a death after starting is not a launch failure");
    let digest = run.digest.expect("digest");
    assert!(!digest.captured);
    assert!(!digest.interrupted);
    assert!(
        digest.turn_error.is_none(),
        "claude stated no reason, so this is a death, not a stated failure"
    );
    assert_eq!(digest.exit_code, Some(3));
    assert!(run.session_id.is_some(), "the session id is kept");
    assert!(run.text.contains("without a result"), "got {:?}", run.text);
}

#[tokio::test]
async fn output_before_init_is_not_a_claude_start() {
    // A stray byte or diagnostic, then a crash, before claude's `init` event:
    // claude never reached the point of having a session, so none is kept.
    let scratch = Scratch::new();
    let body = "printf 'booting'\nexit 1";
    let run = run_claude_stub(&scratch, body, &fast_runtime(&scratch), None).await;
    assert!(
        run.is_err(),
        "no init event, no session: a launch failure, not a death"
    );
}

#[tokio::test]
async fn a_stale_interrupt_request_does_not_mark_a_claude_resume_interrupted() {
    // Left by a `review` killed before it consumed it. A resume that then fails
    // for its own reason must report that reason, not an interrupt.
    let scratch = Scratch::new();
    let sid = "0c1a0000-0000-4000-8000-000000000001";
    std::fs::create_dir_all(scratch.data_root().join("review/inflight")).expect("dir");
    review::inflight::request_interrupt(sid, Some(&scratch.data_root())).expect("request");
    let body = format!(
        "{CLAUDE_STUB_INIT}\n{}\nexit 1",
        claude_stub_result(true, r#",\"result\":\"rate limited\""#)
    );
    let run = run_claude_stub(&scratch, &body, &fast_runtime(&scratch), Some(sid))
        .await
        .expect("run completes");
    let digest = run.digest.expect("digest");
    assert!(!digest.interrupted, "the request was not aimed at this run");
    assert!(
        digest
            .turn_error
            .as_deref()
            .is_some_and(|e| e.contains("rate limited")),
        "claude's reason is kept: {:?}",
        digest.turn_error
    );
}

/// Whether `session_id`'s marker is readable once this task sees `launched`
/// fire. `review message` releases its session launch lock on that signal, so
/// a marker written any later leaves a window in which a second message sees
/// no turn in flight and launches beside this one.
///
/// What this can and cannot catch: it runs when the test task is next
/// scheduled after the signal, not at the signal itself. It reliably catches
/// a marker written from a task spawned after launch - the shape the runners
/// had, which failed it 5 times in 5 - but not one written synchronously just
/// after the signal, before the runner yields. That the marker is written
/// before the spawn is stated where each runner writes it; this test guards
/// against the realistic regression, not every conceivable one.
async fn marked_at_launch(
    launched: tokio::sync::oneshot::Receiver<()>,
    session_id: &str,
    data_root: &std::path::Path,
) -> bool {
    launched.await.expect("the run signals its launch");
    review::inflight::live_marker(session_id, Some(data_root)).is_some()
}

#[tokio::test]
async fn a_claude_resume_is_marked_in_flight_before_it_launches() {
    let scratch = Scratch::new();
    let sid = "0c1a0000-0000-4000-8000-000000000002";
    let body = format!(
        "{CLAUDE_STUB_PREAMBLE}\n{CLAUDE_STUB_INIT}\n{}",
        claude_stub_result(false, r#",\"result\":\"ok\""#)
    );
    let mut runtime = fast_runtime(&scratch);
    runtime.claude_command = vec!["/bin/sh".to_string(), scratch.write_stub(&body)];
    let mut env = BTreeMap::new();
    env.insert(
        "STUB_DIR".to_string(),
        scratch.root.to_string_lossy().into_owned(),
    );
    let (tx, rx) = tokio::sync::oneshot::channel();
    let data_root = scratch.data_root();
    let project = scratch.project();
    let (run, marked) = tokio::join!(
        run_claude(
            sid,
            None,
            None,
            Some(&env),
            "more",
            &project,
            false,
            Some(tx),
            &runtime
        ),
        marked_at_launch(rx, sid, &data_root)
    );
    assert!(run.is_ok_and(|r| r.text == "ok"));
    assert!(marked, "the marker must exist when the launch is signalled");
}

#[tokio::test]
async fn a_codex_resume_is_marked_in_flight_before_it_launches() {
    let scratch = Scratch::new();
    let stub = scratch.write_stub(&format!(
        "{}\nprintf '%s' 'ok' > \"$LAST_MSG\"",
        stub_preamble()
    ));
    let runtime = fast_runtime(&scratch);
    let out_file = new_output_file().expect("create -o file");
    let mut env = BTreeMap::new();
    env.insert(
        "CODEX_HOME".to_string(),
        scratch.codex_home().to_string_lossy().into_owned(),
    );
    env.insert("LAST_MSG".to_string(), out_file.clone());
    let (tx, rx) = tokio::sync::oneshot::channel();
    let data_root = scratch.data_root();
    let project = scratch.project();
    let (run, marked) = tokio::join!(
        run_codex_json(
            vec![stub, "exec".to_string(), "resume".to_string()],
            &out_file,
            Some(STUB_SESSION.to_string()),
            Some(&env),
            "more",
            &project,
            Some(tx),
            &runtime,
        ),
        marked_at_launch(rx, STUB_SESSION, &data_root)
    );
    assert!(run.is_ok());
    assert!(marked, "the marker must exist when the launch is signalled");
}

/// The ticket's case: the agent started a hung script in a session of its own
/// (codex's sandbox runs bwrap with `--new-session`), and codex finished. The
/// script escaped every process group `review` knew about and used to run for
/// hours after `review` reported the run over. The keeper adopts it as a
/// subreaper and ends it - SIGKILL, since it ignores SIGTERM - before the run
/// returns, and the record names it.
#[tokio::test]
async fn a_hung_process_in_its_own_session_is_ended_when_the_run_ends() {
    let scratch = Scratch::new();
    let stub = scratch.write_stub(&format!(
        r#"{preamble}
setsid sh -c 'trap "" TERM; echo $$ > "$LINGER_PID"; while :; do sleep 1; done' &
# Wait until it has recorded itself, so the test can find it.
while [ ! -s "$LINGER_PID" ]; do sleep 0.01; done
printf '%s\n' '{{"type":"event_msg","payload":{{"type":"agent_message","message":"DONE","phase":"final_answer"}}}}' >> "$ROLL"
printf '%s' 'DONE' > "$LAST_MSG"
exit 0
"#,
        preamble = stub_preamble()
    ));
    let run = run_stub(&scratch, stub).await.expect("run completes");
    assert_eq!(run.text, "DONE");
    // The teardown takes longer than the stub's quiet grace (the script needs
    // the SIGKILL rounds), and must not read as codex refusing to exit.
    let digest = run.digest.as_ref().expect("codex digest");
    assert_eq!(digest.terminated_by_review, None);
    assert!(
        scratch.incident_dirs().is_empty(),
        "a clean run is no incident"
    );
    let hung = scratch.linger_pid().expect("stub recorded the hung script");
    let gone = !pid_alive(hung);
    if gone {
        scratch.disarm_linger();
    }
    assert!(gone, "the run must not return while the hung script lives");
    assert!(run.leftovers.is_clean(), "{:?}", run.leftovers);
    assert!(
        run.leftovers
            .killed
            .iter()
            .any(|p| u32::try_from(hung).is_ok_and(|h| h == p.pid)),
        "the killed script is recorded: {:?}",
        run.leftovers
    );
}

/// A daemonising double fork reparents to the nearest subreaper, which is the
/// keeper, so it is torn down like any other leftover.
#[tokio::test]
async fn a_double_forked_daemon_is_ended_when_the_run_ends() {
    let scratch = Scratch::new();
    let stub = scratch.write_stub(&format!(
        r#"{preamble}
( setsid sleep 300 & echo $! > "$LINGER_PID" )
while [ ! -s "$LINGER_PID" ]; do sleep 0.01; done
printf '%s' 'DONE' > "$LAST_MSG"
exit 0
"#,
        preamble = stub_preamble()
    ));
    let run = run_stub(&scratch, stub).await.expect("run completes");
    let daemon = scratch.linger_pid().expect("stub recorded the daemon");
    let gone = !pid_alive(daemon);
    if gone {
        scratch.disarm_linger();
    }
    assert!(gone, "the daemon must be ended before the run returns");
    assert!(run.leftovers.is_clean(), "{:?}", run.leftovers);
}

/// A keeper that dies cannot vouch for anything: the run's teardown outcome is
/// unknown, which goes in the ledger (failing the command) and never reads as
/// clean. The run itself is still `Ok`, so its session survives to be
/// inspected or continued, as for any death.
#[tokio::test]
async fn a_keeper_that_dies_leaves_the_outcome_unknown() {
    let scratch = Scratch::new();
    let stub = scratch.write_stub(&format!(
        r#"{preamble}
echo $$ > "$LINGER_PID"
# This shell's parent is its keeper.
kill -9 $PPID
sleep 1
"#,
        preamble = stub_preamble()
    ));
    let run = run_stub(&scratch, stub).await.expect("the run is still Ok");
    assert_eq!(
        run.session_id.as_deref(),
        Some(STUB_SESSION),
        "the session survives"
    );
    assert!(run.leftovers.unknown.is_some(), "{:?}", run.leftovers);
    let digest = run.digest.as_ref().expect("digest");
    assert_eq!(
        digest.exit_code, None,
        "an unreported status is not invented"
    );
    assert!(
        review::contained::unresolved()
            .iter()
            .any(|u| u.leftovers.unknown.is_some() && u.label.contains("codex")),
        "the ledger records the unknown outcome"
    );
}

/// A provider that cannot be executed fails the launch, with the reason.
#[tokio::test]
async fn a_provider_that_cannot_exec_fails_the_launch() {
    let scratch = Scratch::new();
    let mut runtime = fast_runtime(&scratch);
    runtime.codex_binary = scratch
        .root
        .join("no-such-codex")
        .to_string_lossy()
        .into_owned();
    let err = match run_stub_with(&scratch, "unused", &runtime).await {
        Ok(_) => panic!("a missing binary must fail the launch"),
        Err(e) => e.to_string(),
    };
    assert!(err.contains("failed to launch"), "{err}");
}
