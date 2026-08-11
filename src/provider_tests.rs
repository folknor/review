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

    /// Write an executable stub `codex` and return its path.
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
# Answer the version probe and exit. The incident writer runs `<binary>
# --version` when a run looks suspicious, and every suspicious run here is a
# stub run - so without this the probe would re-enter the stub body and hang on
# its `sleep`, taking the incident write and the whole test with it. Real codex
# answers --version immediately, so the stub must too.
if [ "$1" = "--version" ]; then
  printf '%s\n' 'codex-stub 0.0.0'
  exit 0
fi
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
fn fast_runtime(scratch: &Scratch, binary: String) -> CodexRuntime {
    CodexRuntime {
        binary,
        timings: crate::watchdog::Timings {
            poll_interval: std::time::Duration::from_millis(50),
            quiet_grace: std::time::Duration::from_millis(250),
            // Comfortably longer than `quiet_grace`, mirroring the production
            // ordering (3 min vs 15 min): an answered run must reach the
            // stranded branch well before the stall branch could apply.
            stall_grace: Some(std::time::Duration::from_millis(600)),
        },
        drain_grace: std::time::Duration::from_millis(300),
        // Short enough to keep the escalation test fast, long enough that a
        // cooperative child still gets a real chance to honour SIGTERM first.
        sigkill_escalation: std::time::Duration::from_millis(400),
        data_root: Some(scratch.data_root()),
    }
}

/// Run the stub through the real codex path as a fresh (oneshot) run.
async fn run_stub(scratch: &Scratch, binary: String) -> Result<RunOutput> {
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
    run_codex_json(
        vec![
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
        &scratch.project(),
        None,
        &fast_runtime(scratch, binary),
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

    let mut runtime = fast_runtime(&scratch, stub);
    runtime.timings.stall_grace = None;

    let out_file = new_output_file().expect("create -o file");
    let mut env = BTreeMap::new();
    env.insert(
        "CODEX_HOME".to_string(),
        scratch.codex_home().to_string_lossy().into_owned(),
    );
    env.insert("LAST_MSG".to_string(), out_file.clone());
    env.insert(
        "LINGER_PID".to_string(),
        scratch
            .root
            .join("linger.pid")
            .to_string_lossy()
            .into_owned(),
    );
    let project = scratch.project();
    let run = run_codex_json(
        vec!["exec".to_string(), "--json".to_string()],
        &out_file,
        None,
        Some(&env),
        "review this",
        &project,
        None,
        &runtime,
    );

    // Give the poller many times the stall grace it *would* have used. The run
    // must still be going: with the branch disabled, nothing can end it.
    let outcome = tokio::time::timeout(std::time::Duration::from_millis(2_000), run).await;
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
