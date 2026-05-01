use anyhow::Result;
use serde::Serialize;
use std::path::Path;

#[derive(Serialize)]
struct SessionEntry {
    timestamp: String,
    project: String,
    hostname: String,
    audit_id: String,
    provider: String,
    archetype: String,
    session_id: String,
    /// "oneshot" for --oneshot creation events, "session" for --session resumes.
    kind: &'static str,
    model: Option<String>,
    /// Environment variable *names* configured for this provider entry; values
    /// are deliberately not recorded to avoid leaking secrets in the sidecar.
    env_keys: Vec<String>,
    operator_prompt: String,
    assembled_prompt: String,
    response: Option<String>,
    error: Option<String>,
    review_version: String,
}

fn sessions_path(private: bool) -> Option<std::path::PathBuf> {
    let data_dir = std::env::var("XDG_DATA_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|_| {
            std::env::var("HOME").map(|h| std::path::PathBuf::from(h).join(".local/share"))
        })
        .ok()?;
    let base = data_dir.join("review");
    let name = if private { "sessions-private.jsonl" } else { "sessions.jsonl" };
    Some(base.join(name))
}

#[allow(clippy::too_many_arguments)]
pub fn record(
    project_root: &Path,
    private: bool,
    audit_id: &str,
    archetype: &str,
    provider: &str,
    session_id: &str,
    kind: &'static str,
    model: Option<&str>,
    env_keys: Vec<String>,
    operator_prompt: &str,
    assembled_prompt: &str,
    result: &Result<String>,
) {
    let path = match sessions_path(private) {
        Some(p) => p,
        None => {
            eprintln!("warning: could not determine sessions log path (HOME not set)");
            return;
        }
    };
    if let Some(parent) = path.parent()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        eprintln!("warning: failed to create sessions log dir: {e}");
        return;
    }

    let entry = SessionEntry {
        timestamp: crate::audit::chrono_now(),
        project: project_root.to_string_lossy().to_string(),
        hostname: crate::config::hostname(),
        audit_id: audit_id.to_string(),
        provider: provider.to_string(),
        archetype: archetype.to_string(),
        session_id: session_id.to_string(),
        kind,
        model: model.map(String::from),
        env_keys,
        operator_prompt: operator_prompt.to_string(),
        assembled_prompt: assembled_prompt.to_string(),
        response: result.as_ref().ok().cloned(),
        error: result.as_ref().err().map(ToString::to_string),
        review_version: env!("CARGO_PKG_VERSION").to_string(),
    };

    let line = match serde_json::to_string(&entry) {
        Ok(json) => json,
        Err(e) => {
            eprintln!("warning: failed to serialize session entry: {e}");
            return;
        }
    };

    let mut file = match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        Ok(f) => f,
        Err(e) => {
            eprintln!("warning: failed to open sessions log: {e}");
            return;
        }
    };

    use std::io::Write;
    if let Err(e) = writeln!(file, "{line}") {
        eprintln!("warning: failed to write session entry: {e}");
    }
}
