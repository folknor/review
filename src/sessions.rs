use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Serialize)]
struct SessionEntry<'a> {
    timestamp: String,
    /// Seconds since UNIX epoch — duplicates `timestamp` but lets readers do
    /// age math without parsing the ISO string back.
    epoch_secs: u64,
    project: &'a str,
    hostname: String,
    audit_id: &'a str,
    provider: &'a str,
    archetype: &'a str,
    session_id: &'a str,
    /// "oneshot" for --oneshot creation events, "session" for --session resumes.
    kind: &'static str,
    model: Option<&'a str>,
    /// Environment variable *names* configured for this provider entry; values
    /// are deliberately not recorded to avoid leaking secrets in the sidecar.
    env_keys: Vec<String>,
    operator_prompt: &'a str,
    assembled_prompt: &'a str,
    response: Option<String>,
    error: Option<String>,
    review_version: &'static str,
}

#[derive(Deserialize, Clone)]
pub struct SessionRecord {
    #[allow(dead_code)]
    pub timestamp: String,
    /// Older records may not have this field; default to 0 and treat as
    /// "age unknown" if so.
    #[serde(default)]
    pub epoch_secs: u64,
    pub project: String,
    #[allow(dead_code)]
    pub hostname: String,
    #[allow(dead_code)]
    pub audit_id: String,
    pub provider: String,
    pub archetype: String,
    pub session_id: String,
    /// "oneshot", "session", or "prime"; "" for entries written before this
    /// field existed (none in practice — included for forward compat).
    #[serde(default)]
    pub kind: String,
    #[allow(dead_code)]
    pub model: Option<String>,
    #[allow(dead_code)]
    #[serde(default)]
    pub env_keys: Vec<String>,
    pub operator_prompt: String,
    #[allow(dead_code)]
    pub assembled_prompt: String,
    #[allow(dead_code)]
    pub response: Option<String>,
    #[allow(dead_code)]
    pub error: Option<String>,
    #[allow(dead_code)]
    pub review_version: String,
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

fn now_epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Read every well-formed entry from both the public and private sidecar logs.
/// Malformed lines are skipped with a warning; missing files are treated as empty.
pub fn read_all() -> Vec<SessionRecord> {
    let mut out = Vec::new();
    for private in [false, true] {
        let path = match sessions_path(private) {
            Some(p) => p,
            None => continue,
        };
        if !path.exists() {
            continue;
        }
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("warning: failed to read {}: {e}", path.display());
                continue;
            }
        };
        for line in content.lines() {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<SessionRecord>(line) {
                Ok(r) => out.push(r),
                Err(e) => eprintln!("warning: skipping malformed sessions log line: {e}"),
            }
        }
    }
    out
}

/// Most recent record matching the given session ID, if any.
pub fn latest_for_session(session_id: &str) -> Option<SessionRecord> {
    read_all()
        .into_iter()
        .filter(|r| r.session_id == session_id)
        .max_by(|a, b| a.epoch_secs.cmp(&b.epoch_secs))
}

pub fn age_secs(record: &SessionRecord) -> Option<u64> {
    if record.epoch_secs == 0 {
        return None;
    }
    Some(now_epoch_secs().saturating_sub(record.epoch_secs))
}

pub fn format_age(secs: u64) -> String {
    if secs < 60 {
        return "now".to_string();
    }
    let mins = secs / 60;
    if mins < 60 {
        return format!("{mins}m");
    }
    let hours = mins / 60;
    let rem_mins = mins % 60;
    if hours < 24 {
        if rem_mins == 0 {
            return format!("{hours}h");
        }
        return format!("{hours}h{rem_mins}m");
    }
    let days = hours / 24;
    format!("{days}d")
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

    let project_str = project_root.to_string_lossy();
    let entry = SessionEntry {
        timestamp: crate::audit::chrono_now(),
        epoch_secs: now_epoch_secs(),
        project: project_str.as_ref(),
        hostname: crate::config::hostname(),
        audit_id,
        provider,
        archetype,
        session_id,
        kind,
        model,
        env_keys,
        operator_prompt,
        assembled_prompt,
        response: result.as_ref().ok().cloned(),
        error: result.as_ref().err().map(ToString::to_string),
        review_version: env!("CARGO_PKG_VERSION"),
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
