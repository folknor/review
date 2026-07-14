use anyhow::{Context, Result};
use std::path::Path;
use std::process::Stdio;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

/// Check whether a provider binary is available on PATH.
pub fn is_available(provider: &str) -> bool {
    which::which(provider).is_ok()
}

fn temp_path(archetype: &str, provider: &str) -> String {
    let pid = std::process::id();
    let safe_name: String = archetype
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!("/tmp/review-{safe_name}-{provider}-{pid}.txt")
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
    /// `false` means we fell back to the last streamed message, or none at all.
    pub captured: bool,
    pub turns: u32,
    pub usage: Usage,
    /// Non-JSON stdout lines (codex ERROR/WARN, apply_patch dumps). The harness
    /// can halt NDJSON emission on these, so we keep them visible.
    pub log_lines: Vec<String>,
}

pub struct ProviderResult {
    pub provider: String,
    pub output: Result<String>,
    /// Session ID associated with this invocation, when one is known to the
    /// caller (a fresh run captures the freshly-created session).
    pub session_id: Option<String>,
    /// Structured run summary (codex only, today).
    pub digest: Option<Digest>,
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
    archetype: &str,
    prompt: &str,
    project_root: &Path,
    oneshot: bool,
) -> ProviderResult {
    let result = match provider {
        "claude" => {
            run_claude(
                session_id,
                model,
                effort,
                sandbox,
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
                archetype,
                prompt,
                project_root,
                oneshot,
            )
            .await
        }
        other => Err(anyhow::anyhow!("unknown provider: {other}")),
    };
    match result {
        Ok(run) => ProviderResult {
            provider: provider.to_string(),
            output: Ok(run.text),
            session_id: run.session_id,
            digest: run.digest,
        },
        Err(e) => ProviderResult {
            provider: provider.to_string(),
            output: Err(e),
            session_id: None,
            digest: None,
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
    // TODO: map the profile `sandbox` value (read-only / workspace-write) onto
    // claude's `--permission-mode` (acceptEdits/bypassPermissions for writes).
    // Deferred; claude currently always runs read-only via `--permission-mode
    // dontAsk`.
    _sandbox: Option<&str>,
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
    archetype: &str,
    prompt: &str,
    project_root: &Path,
    oneshot: bool,
) -> Result<RunOutput> {
    if oneshot {
        return run_codex_oneshot(model, effort, sandbox, env, prompt, project_root).await;
    }

    let output_path = temp_path(archetype, "codex");

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
    args.extend([
        "resume".to_string(),
        session_id.to_string(),
        "-o".to_string(),
        output_path.clone(),
    ]);

    let mut cmd = Command::new("codex");
    cmd.args(&args)
        .current_dir(project_root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(vars) = env {
        cmd.envs(vars);
    }
    let mut child = cmd.spawn().context("failed to spawn codex")?;

    let stdin = child.stdin.take().context("failed to open codex stdin")?;
    let write_result = write_stdin(stdin, prompt.as_bytes().to_vec());
    let output = child.wait_with_output();

    let (write_res, output) = tokio::join!(write_result, output);
    let output = output.context("failed to wait for codex")?;

    let cleanup = || async {
        let _ = tokio::fs::remove_file(&output_path).await;
    };

    if !output.status.success() {
        cleanup().await;
        if let Err(e) = write_res {
            anyhow::bail!("failed to write prompt: {e}");
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("codex exited with error: {}", stderr.trim());
    }

    let result = tokio::fs::read_to_string(&output_path).await;
    cleanup().await;

    let text = result.with_context(|| format!("failed to read codex output from {output_path}"))?;
    Ok(RunOutput {
        text,
        session_id: None,
        digest: None,
    })
}

/// Oneshot codex: `--json` to capture the freshly-created thread_id and stream
/// events, plus `-o` as the authoritative final-message backstop. Distills a
/// `Digest` (usage, turns, captured, exit/signal, log lines) so the run's
/// outcome is legible even when the stream halts or the process errors.
async fn run_codex_oneshot(
    model: Option<&str>,
    effort: Option<&str>,
    sandbox: Option<&str>,
    env: Option<&std::collections::BTreeMap<String, String>>,
    prompt: &str,
    project_root: &Path,
) -> Result<RunOutput> {
    let last_msg_path = temp_path("last", "codex");
    let mut args: Vec<String> = vec![
        "exec".to_string(),
        "--sandbox".to_string(),
        sandbox.unwrap_or("read-only").to_string(),
        "--json".to_string(),
        "-o".to_string(),
        last_msg_path.clone(),
    ];
    if let Some(m) = model {
        args.push("-m".to_string());
        args.push(m.to_string());
    }
    if let Some(e) = effort {
        args.push("-c".to_string());
        args.push(format!("model_reasoning_effort=\"{e}\""));
    }

    let mut cmd = Command::new("codex");
    cmd.args(&args)
        .current_dir(project_root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(vars) = env {
        cmd.envs(vars);
    }
    let mut child = cmd.spawn().context("failed to spawn codex")?;

    let stdin = child.stdin.take().context("failed to open codex stdin")?;
    let write_result = write_stdin(stdin, prompt.as_bytes().to_vec());
    let output = child.wait_with_output();

    let (_write_res, output) = tokio::join!(write_result, output);
    let output = output.context("failed to wait for codex")?;

    // Parse the NDJSON stream: session id, streamed final message, turn count,
    // summed usage, and any non-JSON log lines. We do NOT bail on a non-zero
    // exit here - the whole point of the digest is that a halted or errored run
    // still yields whatever it produced, with the exit status recorded.
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut session_id: Option<String> = None;
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
                session_id = val
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
    let _ = tokio::fs::remove_file(&last_msg_path).await;

    let captured = final_from_file.is_some();
    let final_message = final_from_file.or(stream_message);

    let digest = Digest {
        exit_code: output.status.code(),
        signal: signal_name(&output.status),
        captured,
        turns,
        usage,
        log_lines,
    };

    match final_message {
        Some(text) => Ok(RunOutput {
            text,
            session_id,
            digest: Some(digest),
        }),
        None => {
            // Nothing usable came back at all. Surface stderr as the error.
            let stderr = String::from_utf8_lossy(&output.stderr);
            let detail = stderr.trim();
            if detail.is_empty() {
                anyhow::bail!(
                    "codex produced no final message (exit {:?})",
                    output.status.code()
                );
            }
            anyhow::bail!("codex produced no final message: {detail}");
        }
    }
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
}
