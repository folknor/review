use anyhow::{Context, Result, bail};
use std::path::Path;
use std::process::Stdio;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

pub struct PrimedSession {
    pub provider: String,
    pub session_id: String,
}

pub async fn prime_provider(
    provider: &str,
    prompt: &str,
    project_root: &Path,
) -> Result<PrimedSession> {
    match provider {
        "claude" => prime_claude(prompt, project_root).await,
        "codex" => prime_codex(prompt, project_root).await,
        other => bail!("prime not yet supported for provider '{other}'"),
    }
}

async fn prime_claude(prompt: &str, project_root: &Path) -> Result<PrimedSession> {
    let session_id = generate_uuid();

    eprintln!("priming claude session {session_id}...");

    let mut child = Command::new("claude")
        .args([
            "--session-id",
            &session_id,
            "--print",
            "--permission-mode",
            "dontAsk",
        ])
        .current_dir(project_root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to spawn claude")?;

    // Write prompt concurrently with stdout read
    let stdin = child.stdin.take().context("failed to open claude stdin")?;
    let prompt_bytes = prompt.as_bytes().to_vec();
    let write_handle = tokio::spawn(async move {
        let mut stdin = stdin;
        let _ = stdin.write_all(&prompt_bytes).await;
        drop(stdin);
    });

    let output = child.wait_with_output().await;
    let _ = write_handle.await;
    let output = output.context("failed to wait for claude")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("claude exited with error: {}", stderr.trim());
    }

    let response = String::from_utf8_lossy(&output.stdout);
    eprintln!("claude primed successfully");
    eprintln!("---");
    eprintln!("{}", response.trim());
    eprintln!("---");

    Ok(PrimedSession {
        provider: "claude".to_string(),
        session_id,
    })
}

async fn prime_codex(prompt: &str, project_root: &Path) -> Result<PrimedSession> {
    eprintln!("priming codex session...");

    let mut child = Command::new("codex")
        .args(["exec", "--json"])
        .current_dir(project_root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to spawn codex")?;

    let stdin = child.stdin.take().context("failed to open codex stdin")?;
    let prompt_bytes = prompt.as_bytes().to_vec();
    let write_handle = tokio::spawn(async move {
        let mut stdin = stdin;
        let _ = stdin.write_all(&prompt_bytes).await;
        drop(stdin);
    });

    let output = child.wait_with_output().await;
    let _ = write_handle.await;
    let output = output.context("failed to wait for codex")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("codex exited with error: {}", stderr.trim());
    }

    // Parse session ID from the JSONL output
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut session_id = None;
    let mut response_text = None;

    for line in stdout.lines() {
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(line) {
            if val.get("type").and_then(|t| t.as_str()) == Some("thread.started") {
                session_id = val.get("thread_id").and_then(|t| t.as_str()).map(String::from);
            }
            if val.get("type").and_then(|t| t.as_str()) == Some("item.completed") {
                response_text = val
                    .get("item")
                    .and_then(|i| i.get("text"))
                    .and_then(|t| t.as_str())
                    .map(String::from);
            }
        }
    }

    let session_id = session_id.ok_or_else(|| {
        anyhow::anyhow!("could not find thread_id in codex output")
    })?;

    eprintln!("codex primed successfully (session: {session_id})");
    if let Some(text) = response_text {
        eprintln!("---");
        eprintln!("{}", text.trim());
        eprintln!("---");
    }

    Ok(PrimedSession {
        provider: "codex".to_string(),
        session_id,
    })
}

fn generate_uuid() -> String {
    // Read from /proc/sys/kernel/random/uuid (Linux)
    std::fs::read_to_string("/proc/sys/kernel/random/uuid")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| {
            // Fallback: generate a v4 UUID from random bytes
            let mut buf = [0u8; 16];
            if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
                use std::io::Read;
                let _ = f.read_exact(&mut buf);
            }
            buf[6] = (buf[6] & 0x0f) | 0x40; // version 4
            buf[8] = (buf[8] & 0x3f) | 0x80; // variant 1
            format!(
                "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
                buf[0], buf[1], buf[2], buf[3],
                buf[4], buf[5],
                buf[6], buf[7],
                buf[8], buf[9],
                buf[10], buf[11], buf[12], buf[13], buf[14], buf[15]
            )
        })
}
