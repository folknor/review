//! End-to-end tests for the codex runner, driven by a stub `codex` binary.
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
//! `CodexRuntime` makes the binary, the watchdog timings, the drain grace and
//! the `SIGKILL` escalation delay injectable, so a hang that takes 3 minutes to
//! detect and 10 seconds to escalate in production resolves in under a second
//! here. Nothing about the code path itself changes.
//!
//! # Isolation
//!
//! Scratch files live under `target/test-scratch/<uuid>/` (per the project rule
//! that data lives in the repo rather than `/tmp`) and are removed on success.
//!
//! `CodexRuntime::data_root` is also redirected there, and that is load-bearing
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
//! `incident::probe_codex_version` must keep its stdin set to null so a shell
//! that handles an unknown option by reading stdin cannot hang the bundle write.

use super::*;
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
            .join(crate::config::generate_uuid());
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
fn fast_runtime(scratch: &Scratch) -> CodexRuntime {
    CodexRuntime {
        // Execute the stable system shell, not the fixture file another test
        // thread may still have open for writing. The script path is argv[0]
        // from the shell's perspective; see `run_stub_with`.
        binary: "/bin/sh".to_string(),
        timings: crate::watchdog::Timings {
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
        // Short enough to keep the escalation test fast, long enough that a
        // cooperative child still gets a real chance to honour SIGTERM first.
        sigkill_escalation: std::time::Duration::from_millis(400),
        data_root: Some(scratch.data_root()),
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
    runtime: &CodexRuntime,
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

/// Wait for `pid` to disappear, up to `limit`. Returns whether it did.
async fn wait_for_exit(pid: i32, limit: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + limit;
    while std::time::Instant::now() < deadline {
        if !pid_alive(pid) {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    !pid_alive(pid)
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

    // Clean up the lingering pipe-holder rather than leaving it to its own
    // 30-second timeout. `Scratch::drop` is the backstop if anything above this
    // line panicked; disarm it only once the process is confirmed gone.
    let lingering = scratch
        .linger_pid()
        .expect("stub recorded its background pid");
    // SAFETY: a pid this test's stub spawned and recorded.
    unsafe { libc::kill(lingering, libc::SIGKILL) };
    assert!(
        wait_for_exit(lingering, std::time::Duration::from_secs(5)).await,
        "the lingering pipe-holder must be cleaned up by the test"
    );
    scratch.disarm_linger();
}

/// 4b. Group escalation: a descendant that ignores `SIGTERM` while its parent
///     hangs after producing an answer. The watchdog fires, `terminate_group`
///     SIGTERMs the group (which the descendant ignores) and escalates to
///     `SIGKILL`. Asserts the descendant is actually gone, rather than assuming
///     the signal landed.
///
///     Split from 4a deliberately: that test exercises pipe retention with
///     nothing being killed, this one exercises killing. Combining them meant
///     neither was really tested - the parent exited on its own, so no
///     `terminate_group` call ever happened.
#[tokio::test]
async fn group_escalation_kills_a_sigterm_ignoring_descendant() {
    let scratch = Scratch::new();
    let stub = scratch.write_stub(&format!(
        r#"{preamble}
printf '%s\n' '{{"type":"event_msg","payload":{{"type":"agent_message","message":"ANSWER","phase":"final_answer"}}}}' >> "$ROLL"
# A descendant that refuses SIGTERM outright. Only the SIGKILL escalation in
# terminate_group can reap it.
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

    // terminate_group escalates to SIGKILL after `sigkill_escalation` (shortened
    // for the test), so allow for that plus scheduling slack.
    let stubborn = scratch
        .linger_pid()
        .expect("stub recorded its background pid");
    let reaped = wait_for_exit(stubborn, std::time::Duration::from_secs(10)).await;
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

// ---------------------------------------------------------------------------
// grok output classification
//
// These need no child process: grok reports the outcome of a turn in its own
// result object, so the whole decision is a pure function of the two streams.
// That is the substantive difference from codex, where the equivalent question
// ("is this text an answer?") can only be settled by reading the rollout, and
// where the tests consequently have to drive a real process.
// ---------------------------------------------------------------------------

/// The shape of a healthy run, trimmed to the fields we read.
const GROK_OK: &str = r#"{
  "text": "OK",
  "stopReason": "end_turn",
  "sessionId": "3f1c9a2e-7b40-4d51-9c68-2a5e10d3b7f4",
  "usage": { "total_tokens": 22628 },
  "num_turns": 1
}"#;

#[test]
fn grok_end_turn_is_an_answer() {
    let super::GrokOutcome::Answered(answer) =
        super::interpret_grok_output(GROK_OK, "").expect("end_turn is an answer")
    else {
        panic!("end_turn must classify as an answer");
    };
    assert_eq!(answer.text, "OK");
    assert_eq!(
        answer.session_id.as_deref(),
        Some("3f1c9a2e-7b40-4d51-9c68-2a5e10d3b7f4"),
        "the echoed session id is what we record, not the one we asked for"
    );
}

#[test]
fn grok_cancelled_turn_is_not_an_answer() {
    // Observed by running a tool-using prompt under `--max-turns 1`: a full
    // result object, exit 1, and `text` holding the model's opening remark.
    // Persisting that as the response is the exact failure this gate prevents.
    let cut_off = r#"{
      "text": "I'll list everything in `src/` first, then read each file.",
      "stopReason": "cancelled",
      "sessionId": "019ffbc5-2b48-7611-a7f2-48db1fa2b0ed",
      "num_turns": 1
    }"#;
    let outcome = super::interpret_grok_output(cut_off, "Error: max turns reached")
        .expect("a cancelled turn still ran - it is not a launch failure");
    let super::GrokOutcome::NoAnswer {
        reason,
        text,
        session_id,
    } = outcome
    else {
        panic!("a cancelled turn is not an answer");
    };
    assert!(
        reason.contains("cancelled"),
        "names the stop reason: {reason}"
    );
    assert!(
        text.contains("list everything"),
        "the interim commentary is kept as a clue: {text}"
    );
    // The session id is the point of returning this rather than an error: the
    // turn that was cut off is the one most worth resuming, and `invoke` drops
    // the id on every error path.
    assert_eq!(
        session_id.as_deref(),
        Some("019ffbc5-2b48-7611-a7f2-48db1fa2b0ed"),
        "a cut-off turn keeps its session id"
    );

    // It must still read as a failure everywhere that matters: exit code,
    // greppable digest, and no auto-resume (a stated reason is not a wedge).
    let digest = super::grok_no_answer_digest(reason);
    assert_eq!(digest.exit_code, Some(1), "a run with no answer exits 1");
    assert!(!digest.captured, "nothing was captured");
    assert!(
        !digest.recovered_from_transcript,
        "grok has no rollout to recover from"
    );
    assert!(
        digest.summary().turn_error.is_some(),
        "the reason persists flat, so these stay greppable"
    );
}

#[test]
fn grok_missing_stop_reason_is_not_an_answer() {
    // A future grok that drops or renames the field must not be read as
    // success: the whole gate rests on that field being present and known.
    let outcome = super::interpret_grok_output(r#"{"text": "half a thought"}"#, "")
        .expect("an unlabelled turn still ran");
    let super::GrokOutcome::NoAnswer { reason, .. } = outcome else {
        panic!("an unlabelled turn is not a proven answer");
    };
    assert!(
        reason.contains("missing"),
        "says the reason was absent: {reason}"
    );
}

#[test]
fn grok_without_a_result_object_reports_the_failure() {
    // Unknown model / unknown session / auth failure: grok never reaches a
    // turn, so there is no object to parse and the message is all we have.
    let err = super::interpret_grok_output("", "Error: Couldn't set model 'nope-9000'")
        .expect_err("no result object is a failure");
    let msg = err.to_string();
    assert!(msg.contains("no result object"), "distinct class: {msg}");
    assert!(msg.contains("nope-9000"), "quotes grok's reason: {msg}");
}

#[test]
fn grok_typed_error_line_keeps_its_message() {
    // Every field of a result object is optional, so this line parses as a
    // perfectly valid empty result. Read as one it reports only "no stop
    // reason" and throws away the sentence that says what went wrong - which is
    // the whole content of a pre-turn failure.
    let err = super::interpret_grok_output(r#"{"type":"error","message":"boom"}"#, "")
        .expect_err("a typed error line is not an answer");
    assert!(
        err.to_string().contains("boom"),
        "an error printed only on stdout keeps its message: {err}"
    );
}
