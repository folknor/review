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

pub async fn invoke_claude(
    session_id: &str,
    prompt: &str,
    project_root: &Path,
) -> ProviderResult {
    let result = run_claude(session_id, prompt, project_root).await;
    ProviderResult {
        provider: "claude".into(),
        output: result,
    }
}

pub async fn invoke_codex(
    session_id: &str,
    archetype: &str,
    prompt: &str,
    project_root: &Path,
) -> ProviderResult {
    let result = run_codex(session_id, archetype, prompt, project_root).await;
    ProviderResult {
        provider: "codex".into(),
        output: result,
    }
}

async fn run_claude(session_id: &str, prompt: &str, project_root: &Path) -> Result<String> {
    let mut child = Command::new("claude")
        .args(["--resume", session_id, "--print", "--permission-mode", "dontAsk"])
        .current_dir(project_root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to spawn claude")?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(prompt.as_bytes())
            .await
            .context("failed to write prompt to claude stdin")?;
    }

    let output = child
        .wait_with_output()
        .await
        .context("failed to wait for claude")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("claude exited with error: {}", stderr.trim());
    }

    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

async fn run_codex(
    session_id: &str,
    archetype: &str,
    prompt: &str,
    project_root: &Path,
) -> Result<String> {
    let output_path = temp_path(archetype, "codex");

    let mut child = Command::new("codex")
        .args(["exec", "--sandbox", "read-only", "resume", session_id, "-o", &output_path])
        .current_dir(project_root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to spawn codex")?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(prompt.as_bytes())
            .await
            .context("failed to write prompt to codex stdin")?;
    }

    let output = child
        .wait_with_output()
        .await
        .context("failed to wait for codex")?;

    // Always clean up the temp file, even on error
    let cleanup = || async { let _ = tokio::fs::remove_file(&output_path).await; };

    if !output.status.success() {
        cleanup().await;
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("codex exited with error: {}", stderr.trim());
    }

    let result = tokio::fs::read_to_string(&output_path).await;
    cleanup().await;

    result.with_context(|| format!("failed to read codex output from {output_path}"))
}

pub fn print_result(result: &ProviderResult) {
    println!("--- {} ---", result.provider);
    match &result.output {
        Ok(text) => println!("{text}"),
        Err(err) => println!("error: {err}"),
    }
}
