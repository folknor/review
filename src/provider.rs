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
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' })
        .collect();
    format!("/tmp/review-{safe_name}-{provider}-{pid}.txt")
}

pub struct ProviderResult {
    pub provider: String,
    pub output: Result<String>,
}

pub async fn invoke(
    provider: &str,
    session_id: &str,
    model: Option<&str>,
    archetype: &str,
    prompt: &str,
    project_root: &Path,
) -> ProviderResult {
    let result = match provider {
        "claude" => run_claude(session_id, model, prompt, project_root).await,
        "codex" => run_codex(session_id, model, archetype, prompt, project_root).await,
        "kilo" => run_stdout_provider("kilo", session_id, model, prompt, project_root).await,
        "opencode" => run_stdout_provider("opencode", session_id, model, prompt, project_root).await,
        other => Err(anyhow::anyhow!("unknown provider: {other}")),
    };
    ProviderResult {
        provider: provider.to_string(),
        output: result,
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

/// Run a provider that outputs to stdout (claude, kilo, opencode).
/// Shared logic for stdin pipe → stdout capture.
async fn run_with_stdout(mut child: tokio::process::Child, prompt: &str, provider: &str) -> Result<String> {
    let stdin = child.stdin.take().with_context(|| format!("failed to open {provider} stdin"))?;
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

async fn run_claude(session_id: &str, model: Option<&str>, prompt: &str, project_root: &Path) -> Result<String> {
    let mut args = vec!["--resume", session_id, "--print", "--permission-mode", "dontAsk"];
    let model_owned;
    if let Some(m) = model {
        model_owned = m.to_string();
        args.push("--model");
        args.push(&model_owned);
    }

    let child = Command::new("claude")
        .args(&args)
        .current_dir(project_root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to spawn claude")?;

    run_with_stdout(child, prompt, "claude").await
}

async fn run_codex(
    session_id: &str,
    model: Option<&str>,
    archetype: &str,
    prompt: &str,
    project_root: &Path,
) -> Result<String> {
    let output_path = temp_path(archetype, "codex");

    let mut args = vec!["exec".to_string(), "--sandbox".to_string(), "read-only".to_string()];
    if let Some(m) = model {
        args.push("-m".to_string());
        args.push(m.to_string());
    }
    args.extend(["resume".to_string(), session_id.to_string(), "-o".to_string(), output_path.clone()]);

    let mut child = Command::new("codex")
        .args(&args)
        .current_dir(project_root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to spawn codex")?;

    let stdin = child.stdin.take().context("failed to open codex stdin")?;
    let write_result = write_stdin(stdin, prompt.as_bytes().to_vec());
    let output = child.wait_with_output();

    let (write_res, output) = tokio::join!(write_result, output);
    let output = output.context("failed to wait for codex")?;

    let cleanup = || async { let _ = tokio::fs::remove_file(&output_path).await; };

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

    result.with_context(|| format!("failed to read codex output from {output_path}"))
}

/// Run kilo or opencode — both use `<binary> run -s <id> --dir <path>` and output to stdout.
async fn run_stdout_provider(
    provider: &str,
    session_id: &str,
    model: Option<&str>,
    prompt: &str,
    project_root: &Path,
) -> Result<String> {
    let mut args = vec!["run".to_string(), "-s".to_string(), session_id.to_string()];
    if let Some(m) = model {
        args.push("-m".to_string());
        args.push(m.to_string());
    }
    let dir = project_root.to_string_lossy().to_string();
    args.push("--dir".to_string());
    args.push(dir);

    let child = Command::new(provider)
        .args(&args)
        .current_dir(project_root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to spawn {provider}"))?;

    run_with_stdout(child, prompt, provider).await
}

pub fn print_result(result: &ProviderResult) {
    match &result.output {
        Ok(text) => {
            println!("--- {} ---", result.provider);
            println!("{text}");
        }
        Err(err) => {
            eprintln!("--- {} ---", result.provider);
            eprintln!("error: {err}");
        }
    }
}
