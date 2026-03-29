use anyhow::Result;
use serde::Serialize;
use std::path::Path;

#[derive(Serialize)]
struct AuditEntry {
    timestamp: String,
    project: String,
    archetype: String,
    provider: String,
    session: String,
    prompt: String,
    response: Option<String>,
    error: Option<String>,
}

fn audit_dir(project_root: &Path, private: bool) -> Option<std::path::PathBuf> {
    let data_dir = std::env::var("XDG_DATA_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|_| {
            std::env::var("HOME").map(|h| std::path::PathBuf::from(h).join(".local/share"))
        })
        .ok()?;

    // Use a sanitized version of the full path to avoid collisions
    // between projects with the same directory name
    let project_key = project_root
        .to_string_lossy()
        .replace('/', "-")
        .trim_start_matches('-')
        .to_string();

    let base = if private {
        data_dir.join("review").join("private")
    } else {
        data_dir.join("review")
    };

    Some(base.join(project_key))
}

pub fn log_result(
    project_root: &Path,
    private: bool,
    archetype: &str,
    provider: &str,
    session: &str,
    prompt: &str,
    result: &Result<String>,
) {
    let dir = match audit_dir(project_root, private) {
        Some(d) => d,
        None => {
            eprintln!("warning: could not determine audit directory (HOME not set)");
            return;
        }
    };
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!("warning: failed to create audit dir: {e}");
        return;
    }

    let path = dir.join("audit.jsonl");

    let entry = AuditEntry {
        timestamp: chrono_now(),
        project: project_root.to_string_lossy().to_string(),
        archetype: archetype.to_string(),
        provider: provider.to_string(),
        session: session.to_string(),
        prompt: prompt.to_string(),
        response: result.as_ref().ok().cloned(),
        error: result.as_ref().err().map(ToString::to_string),
    };

    let line = match serde_json::to_string(&entry) {
        Ok(json) => json,
        Err(e) => {
            eprintln!("warning: failed to serialize audit entry: {e}");
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
            eprintln!("warning: failed to open audit log: {e}");
            return;
        }
    };

    use std::io::Write;
    if let Err(e) = writeln!(file, "{line}") {
        eprintln!("warning: failed to write audit entry: {e}");
    }
}

fn chrono_now() -> String {
    // UTC timestamp without pulling in the chrono crate
    let duration = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = duration.as_secs();

    // Convert to date-time components
    let days = secs / 86400;
    let time_secs = secs % 86400;
    let hours = time_secs / 3600;
    let minutes = (time_secs % 3600) / 60;
    let seconds = time_secs % 60;

    // Days since epoch to Y-M-D (simplified, good enough for logging)
    let mut y = 1970i64;
    let mut remaining = days.cast_signed();
    loop {
        let days_in_year = if y % 4 == 0 && (y % 100 != 0 || y % 400 == 0) { 366 } else { 365 };
        if remaining < days_in_year {
            break;
        }
        remaining -= days_in_year;
        y += 1;
    }
    let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
    let month_days = [31, if leap { 29 } else { 28 }, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let mut m = 0usize;
    for md in &month_days {
        if remaining < *md as i64 {
            break;
        }
        remaining -= *md as i64;
        m += 1;
    }

    format!("{y:04}-{:02}-{:02}T{hours:02}:{minutes:02}:{seconds:02}Z", m + 1, remaining + 1)
}
