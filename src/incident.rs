//! Forensic capture for suspicious provider runs.
//!
//! When a codex run looks wrong (not captured / non-zero exit / signal) the
//! live digest + transcript summary are not enough to explain *why* it died -
//! the investigation that motivated this module kept stalling on information
//! `review` had thrown away: codex's stderr, the raw NDJSON stream, whether the
//! prompt even finished writing, and (absent `RUST_BACKTRACE`) any panic trace.
//!
//! `write_bundle` dumps all of it to `~/.local/share/review/incidents/<dir>/`
//! so the *next* death is over-instrumented instead of a mystery. It is
//! best-effort: every failure warns and returns, never derailing the run. Only
//! suspicious runs write a bundle, so clean runs stay uncluttered.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Everything known about a finished provider run, handed to the bundle writer.
pub struct Incident<'a> {
    pub provider: &'a str,
    /// The binary invoked (e.g. "codex") - argv[0] of the replayable command.
    pub binary: &'a str,
    pub session_id: Option<&'a str>,
    /// The args passed to the binary (not including argv[0]).
    pub argv: &'a [String],
    /// The prompt piped on the child's stdin - part of the invocation, so a
    /// replay is impossible without it.
    pub prompt: &'a str,
    pub cwd: &'a Path,
    /// Profile env (names always recorded; values redacted when the name looks
    /// secret, so the bundle never leaks tokens).
    pub env: Option<&'a std::collections::BTreeMap<String, String>>,
    pub exit_code: Option<i32>,
    pub signal: Option<&'a str>,
    /// `Some` if writing the prompt to the child's stdin failed (e.g. a broken
    /// pipe because the child exited before consuming it).
    pub stdin_write_error: Option<&'a str>,
    pub stdout: &'a [u8],
    pub stderr: &'a [u8],
    pub transcript_path: Option<&'a str>,
    pub duration_ms: u128,
    /// Overrides the XDG data root the bundle is written under. `None` in
    /// production; set by tests so a stub run cannot write into the operator's
    /// real incident directory.
    pub data_root: Option<&'a Path>,
    pub captured: bool,
    pub recovered_from_transcript: bool,
    pub turns: u32,
    /// Why `review` killed the run, when it did (watchdog verdict or operator
    /// signal). `None` means codex ended on its own.
    pub terminated_by_review: Option<&'a str>,
    /// How long the rollout had been unchanged when the watchdog acted, and the
    /// last event it saw. The pair that makes a stall-timeout misfire on a
    /// healthy run diagnosable after the fact.
    pub quiet_secs: Option<u64>,
    pub last_rollout_event: Option<&'a str>,
    /// Codex's own stated reason for the turn ending (stream `error` /
    /// `turn.failed`) - typically an upstream refusal. Recorded so a bundle that
    /// has an explanation says so on its face, rather than being filed alongside
    /// the genuinely unexplained deaths this directory exists for.
    pub turn_error: Option<&'a str>,
}

/// Keep the last MiB of the raw NDJSON stream - the death is at the end, and a
/// long agentic run's stream can be tens of MiB.
const STDOUT_TAIL_BYTES: usize = 1 << 20;
/// Raw transcript lines to keep in the tail (the full file stays on disk).
const TRANSCRIPT_TAIL_LINES: usize = 80;

#[derive(Serialize)]
struct Meta {
    schema: u32,
    timestamp: String,
    provider: String,
    session_id: Option<String>,
    review_version: &'static str,
    codex_version: Option<String>,
    /// Full argv including argv[0] (the binary).
    argv: Vec<String>,
    /// A copy-pasteable shell command that reproduces the run: the injected env
    /// prefix, the quoted argv, and `< prompt.txt` (the prompt is piped on
    /// stdin, so it's written alongside as `prompt.txt`). Secret env values are
    /// shown as `<redacted>` - fill them back in to actually run it.
    command: String,
    cwd: String,
    prompt_bytes: usize,
    exit_code: Option<i32>,
    signal: Option<String>,
    stdin_write_error: Option<String>,
    duration_ms: u128,
    captured: bool,
    recovered_from_transcript: bool,
    turns: u32,
    terminated_by_review: Option<String>,
    quiet_secs: Option<u64>,
    last_rollout_event: Option<String>,
    /// Codex's stated reason for the turn ending, when it gave one.
    turn_error: Option<String>,
    stdout_bytes: usize,
    stderr_bytes: usize,
    stdout_truncated: bool,
    transcript_path: Option<String>,
    /// codex records this on `task_complete`; `Some(false)` means the turn ended
    /// with no final answer (a death signature), `Some(true)` means it did.
    final_answer_present: Option<bool>,
    /// Rate-limit snapshot from the last `token_count` event (raw JSON), so a
    /// limit-driven stop is visible without re-reading the transcript.
    rate_limits: Option<serde_json::Value>,
    /// Env var names configured for the run; values only when non-secret.
    env: Vec<EnvVar>,
}

#[derive(Serialize)]
struct EnvVar {
    name: String,
    value: Option<String>,
}

/// Where bundles are written. `data_root` overrides the real XDG location and
/// exists so the test harness cannot deposit stub bundles in the operator's
/// actual `~/.local/share/review/incidents` - it did exactly that before this
/// was injectable, because redirecting `CODEX_HOME` only redirects the *child*.
fn incidents_dir(data_root: Option<&Path>) -> Option<PathBuf> {
    let data_dir = match data_root {
        Some(root) => root.to_path_buf(),
        None => std::env::var("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|_| std::env::var("HOME").map(|h| PathBuf::from(h).join(".local/share")))
            .ok()?,
    };
    Some(data_dir.join("review").join("incidents"))
}

/// A name that shouldn't have its value recorded (heuristic, errs toward hiding).
fn looks_secret(name: &str) -> bool {
    let n = name.to_ascii_uppercase();
    [
        "KEY",
        "TOKEN",
        "SECRET",
        "PASSWORD",
        "PASSWD",
        "AUTH",
        "COOKIE",
        "CREDENTIAL",
    ]
    .iter()
    .any(|needle| n.contains(needle))
}

/// Write a forensic bundle for a suspicious run. Returns the bundle directory on
/// success. Best-effort: warns and returns `None` on any IO failure.
pub fn write_bundle(inc: &Incident) -> Option<PathBuf> {
    let base = incidents_dir(inc.data_root)?;
    let stamp = crate::audit::chrono_now().replace(':', "-");
    let sid = inc.session_id.unwrap_or("no-session");
    // A short random suffix keeps two same-second failures (e.g. two
    // `--stagger 0` deaths before `thread.started`, both "no-session") from
    // colliding on the same dir and overwriting each other's artifacts.
    let nonce = crate::config::generate_short_id();
    let dir = base.join(format!("{stamp}-{}-{sid}-{nonce}", inc.provider));
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!(
            "warning: failed to create incident dir {}: {e}",
            dir.display()
        );
        return None;
    }

    // Full stderr - the highest-value channel we used to discard. With
    // RUST_BACKTRACE set on the child, a panic lands here with its trace.
    write_file(&dir, "stderr.txt", inc.stderr);

    // The prompt piped on stdin, and the runnable command that consumes it.
    write_file(&dir, "prompt.txt", inc.prompt.as_bytes());
    let mut full_argv = vec![inc.binary.to_string()];
    full_argv.extend(inc.argv.iter().cloned());
    let command = replay_command(&full_argv, inc.env, inc.cwd, &dir.join("prompt.txt"));

    // Raw NDJSON stream, tail-capped. The interesting part is where it stops.
    let (stdout_slice, stdout_truncated) = tail_bytes(inc.stdout, STDOUT_TAIL_BYTES);
    write_file(&dir, "stdout.jsonl", stdout_slice);

    // Transcript tail (the full rollout stays at transcript_path) plus the two
    // scalars worth surfacing without a re-parse.
    let mut final_answer_present = None;
    let mut rate_limits = None;
    if let Some(path) = inc.transcript_path
        && let Ok(content) = std::fs::read_to_string(path)
    {
        let tail = tail_lines(&content, TRANSCRIPT_TAIL_LINES);
        write_file(&dir, "transcript.tail.jsonl", tail.as_bytes());
        (final_answer_present, rate_limits) = scan_transcript(&content);
    }

    let env = inc
        .env
        .map(|m| {
            m.iter()
                .map(|(k, v)| EnvVar {
                    name: k.clone(),
                    value: (!looks_secret(k)).then(|| v.clone()),
                })
                .collect()
        })
        .unwrap_or_default();

    let meta = Meta {
        schema: 1,
        timestamp: crate::audit::chrono_now(),
        provider: inc.provider.to_string(),
        session_id: inc.session_id.map(str::to_string),
        review_version: env!("CARGO_PKG_VERSION"),
        codex_version: probe_codex_version(inc.binary),
        argv: full_argv,
        command,
        cwd: inc.cwd.to_string_lossy().into_owned(),
        prompt_bytes: inc.prompt.len(),
        exit_code: inc.exit_code,
        signal: inc.signal.map(str::to_string),
        stdin_write_error: inc.stdin_write_error.map(str::to_string),
        duration_ms: inc.duration_ms,
        captured: inc.captured,
        recovered_from_transcript: inc.recovered_from_transcript,
        turns: inc.turns,
        terminated_by_review: inc.terminated_by_review.map(str::to_string),
        quiet_secs: inc.quiet_secs,
        last_rollout_event: inc.last_rollout_event.map(str::to_string),
        turn_error: inc.turn_error.map(str::to_string),
        stdout_bytes: inc.stdout.len(),
        stderr_bytes: inc.stderr.len(),
        stdout_truncated,
        transcript_path: inc.transcript_path.map(str::to_string),
        final_answer_present,
        rate_limits,
        env,
    };
    // meta.json is the index `review incidents` reads and the file that makes
    // the bundle self-describing. If it can't be written (quota, unwritable
    // dir), don't advertise a path that points at a broken bundle.
    let meta_ok = match serde_json::to_vec_pretty(&meta) {
        Ok(bytes) => write_file(&dir, "meta.json", &bytes),
        Err(e) => {
            eprintln!("warning: failed to serialize incident meta: {e}");
            false
        }
    };
    if !meta_ok {
        return None;
    }

    Some(dir)
}

/// Write `bytes` to `dir/name`; returns whether it succeeded.
fn write_file(dir: &Path, name: &str, bytes: &[u8]) -> bool {
    let path = dir.join(name);
    match std::fs::write(&path, bytes) {
        Ok(()) => true,
        Err(e) => {
            eprintln!("warning: failed to write {}: {e}", path.display());
            false
        }
    }
}

/// Last `max` bytes of `data`, plus whether it was truncated.
fn tail_bytes(data: &[u8], max: usize) -> (&[u8], bool) {
    if data.len() <= max {
        (data, false)
    } else {
        (&data[data.len() - max..], true)
    }
}

/// Last `max` non-empty lines, rejoined.
fn tail_lines(content: &str, max: usize) -> String {
    let lines: Vec<&str> = content.lines().filter(|l| !l.trim().is_empty()).collect();
    let start = lines.len().saturating_sub(max);
    lines[start..].join("\n")
}

/// Extract the two scalars worth promoting into meta: whether the final
/// `task_complete` carried a non-null `last_agent_message`, and the last
/// `token_count` rate-limit snapshot.
fn scan_transcript(content: &str) -> (Option<bool>, Option<serde_json::Value>) {
    let mut final_answer_present = None;
    let mut rate_limits = None;
    for line in content.lines() {
        let val: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let payload = val.get("payload");
        match payload.and_then(|p| p.get("type")).and_then(|t| t.as_str()) {
            // Reset per turn so a resumed turn that freezes before task_complete
            // reports "unknown", not the prior turn's completion (which would
            // mislabel the death as "completed" in `review incidents`).
            Some("task_started") => final_answer_present = None,
            Some("task_complete") => {
                let msg = payload.and_then(|p| p.get("last_agent_message"));
                final_answer_present = Some(msg.is_some_and(|m| !m.is_null()));
            }
            Some("token_count") => {
                if let Some(rl) = payload.and_then(|p| p.get("rate_limits"))
                    && !rl.is_null()
                {
                    rate_limits = Some(rl.clone());
                }
            }
            _ => {}
        }
    }
    (final_answer_present, rate_limits)
}

/// Build a copy-pasteable shell command that reproduces the run: `cd` to the
/// recorded cwd (codex resolves rules/sandbox root from it), the injected
/// `RUST_BACKTRACE=1` plus profile env, the quoted argv, and stdin redirected
/// from the bundle's absolute `prompt.txt`. Secret env values become
/// `<redacted>` so the string is safe to keep. Self-contained: runnable from
/// anywhere, and it can't pick up the wrong repo or the wrong prompt.
fn replay_command(
    full_argv: &[String],
    env: Option<&std::collections::BTreeMap<String, String>>,
    cwd: &Path,
    prompt_path: &Path,
) -> String {
    let mut parts = vec!["RUST_BACKTRACE=1".to_string()];
    if let Some(m) = env {
        for (k, v) in m {
            let val = if looks_secret(k) {
                "<redacted>"
            } else {
                v.as_str()
            };
            parts.push(format!("{k}={}", shell_quote(val)));
        }
    }
    parts.extend(full_argv.iter().map(|a| shell_quote(a)));
    format!(
        "cd {} && {} < {}",
        shell_quote(&cwd.to_string_lossy()),
        parts.join(" "),
        shell_quote(&prompt_path.to_string_lossy()),
    )
}

/// Minimal POSIX shell quoting: bare when safe, single-quoted otherwise.
fn shell_quote(s: &str) -> String {
    let safe = !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_./=:,@".contains(&b));
    if safe {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

/// A subset of `meta.json` read back for `review incidents`. All fields default
/// so a partial or older bundle still lists.
#[derive(Deserialize, Default)]
pub struct IncidentSummary {
    #[serde(default)]
    pub timestamp: String,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub provider: String,
    #[serde(default)]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub signal: Option<String>,
    #[serde(default)]
    pub recovered_from_transcript: bool,
    #[serde(default)]
    pub final_answer_present: Option<bool>,
    #[serde(default)]
    pub codex_version: Option<String>,
    /// Codex's stated reason for the turn ending. When present it *is* the
    /// verdict - the bundle is explained, not a mystery death.
    #[serde(default)]
    pub turn_error: Option<String>,
}

pub struct ListedIncident {
    pub dir: PathBuf,
    pub meta: IncidentSummary,
}

/// Recent incident bundles, newest first. Bundle dir names are UTC-timestamped,
/// so a reverse lexical sort is chronological. A dir whose `meta.json` is
/// missing or unreadable still lists (with defaults) so nothing is hidden.
pub fn list_recent(limit: usize) -> Vec<ListedIncident> {
    // Always the real location: `review incidents` is an operator-facing view.
    let base = match incidents_dir(None) {
        Some(b) => b,
        None => return Vec::new(),
    };
    let mut dirs: Vec<PathBuf> = match std::fs::read_dir(&base) {
        Ok(rd) => rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect(),
        Err(_) => return Vec::new(),
    };
    dirs.sort_by(|a, b| b.file_name().cmp(&a.file_name()));
    dirs.truncate(limit);
    dirs.into_iter()
        .map(|dir| {
            let meta = std::fs::read_to_string(dir.join("meta.json"))
                .ok()
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or_default();
            ListedIncident { dir, meta }
        })
        .collect()
}

/// Best-effort `<binary> --version`; deaths may be version-specific.
///
/// Probes the binary that was actually executed, not the provider name. Those
/// differ whenever the binary is overridden (the test harness runs a stub), and
/// probing the *name* meant a stub run recorded the host's real `codex
/// --version` - metadata describing a process that never ran.
fn probe_codex_version(binary: &str) -> Option<String> {
    let out = std::process::Command::new(binary)
        .arg("--version")
        // Never inherit our stdin. `output()` would otherwise hand the probe
        // whatever `review` is reading from, and a binary that reads stdin
        // before parsing its arguments then blocks forever - taking the incident
        // write, and the run, with it. A version probe has no business reading
        // input, so close it.
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_flags_null_final_answer_as_death() {
        // task_complete with last_agent_message=null == no final answer produced.
        let died = concat!(
            r#"{"payload":{"type":"token_count","rate_limits":{"primary":{"used_percent":31.0}}}}"#,
            "\n",
            r#"{"payload":{"type":"task_complete","last_agent_message":null}}"#,
        );
        let (final_answer_present, rate_limits) = scan_transcript(died);
        assert_eq!(final_answer_present, Some(false));
        assert!(rate_limits.is_some(), "rate_limits should be captured");
    }

    #[test]
    fn scan_flags_real_answer_as_present() {
        let ok = r#"{"payload":{"type":"task_complete","last_agent_message":"All done."}}"#;
        let (final_answer_present, _) = scan_transcript(ok);
        assert_eq!(final_answer_present, Some(true));
    }

    #[test]
    fn scan_resets_final_answer_on_new_turn() {
        // Turn 1 completed with an answer; turn 2 started and froze (no complete).
        // The frozen turn must read as "unknown", not inherit turn 1's success.
        let rollout = concat!(
            r#"{"payload":{"type":"task_started"}}"#,
            "\n",
            r#"{"payload":{"type":"task_complete","last_agent_message":"Turn one answer."}}"#,
            "\n",
            r#"{"payload":{"type":"task_started"}}"#,
        );
        let (final_answer_present, _) = scan_transcript(rollout);
        assert_eq!(final_answer_present, None);
    }

    #[test]
    fn secret_env_names_are_redacted() {
        assert!(looks_secret("ANTHROPIC_API_KEY"));
        assert!(looks_secret("OPENAI_TOKEN"));
        assert!(looks_secret("DB_PASSWORD"));
        assert!(!looks_secret("CODEX_HOME"));
        assert!(!looks_secret("CARGO_TARGET_DIR"));
    }

    #[test]
    fn replay_command_is_runnable_and_redacts_secrets() {
        let argv = vec![
            "codex".to_string(),
            "exec".to_string(),
            "--sandbox".to_string(),
            "read-only".to_string(),
        ];
        let mut env = std::collections::BTreeMap::new();
        env.insert("CODEX_HOME".to_string(), "/home/x/.codex".to_string());
        env.insert(
            "ANTHROPIC_API_KEY".to_string(),
            "sk-super-secret".to_string(),
        );
        let cmd = replay_command(
            &argv,
            Some(&env),
            Path::new("/home/x/proj"),
            Path::new("/data/incidents/x/prompt.txt"),
        );
        assert!(cmd.starts_with("cd /home/x/proj && RUST_BACKTRACE=1 "));
        assert!(cmd.contains("CODEX_HOME=/home/x/.codex"));
        assert!(cmd.contains("ANTHROPIC_API_KEY='<redacted>'"));
        assert!(!cmd.contains("sk-super-secret"), "secret leaked: {cmd}");
        assert!(
            cmd.ends_with("codex exec --sandbox read-only < /data/incidents/x/prompt.txt"),
            "cmd was: {cmd}"
        );
    }

    #[test]
    fn shell_quote_handles_spaces_and_quotes() {
        assert_eq!(shell_quote("plain-value"), "plain-value");
        assert_eq!(shell_quote("has space"), "'has space'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
    }

    #[test]
    fn tails_keep_the_end() {
        let (slice, truncated) = tail_bytes(b"abcdef", 3);
        assert_eq!(slice, b"def");
        assert!(truncated);
        let (slice, truncated) = tail_bytes(b"ab", 3);
        assert_eq!(slice, b"ab");
        assert!(!truncated);

        assert_eq!(tail_lines("a\n\nb\nc\n", 2), "b\nc");
    }
}
