//! Reads from grok's own state directory (`~/.grok`) that explain a run which
//! grok itself reports only as `stopReason: cancelled`.
//!
//! Two facts live there that `review` cannot see any other way. Folder trust
//! (`trusted_folders.toml`): in a folder grok has not been told to trust, every
//! file edit raises a permission prompt, and under `--permission-mode dontAsk`
//! that prompt is resolved `cancelled` - which ends the whole turn rather than
//! handing a denial back to the model. And the per-session event log
//! (`sessions/<encoded cwd>/<id>/events.jsonl`), which records *why* a turn was
//! cancelled (`cancellation_category`) and which tool's permission was refused.
//! The result object carries neither. Observed on grok 1.0.40: six write runs
//! in an untrusted checkout each diagnosed their task and were cancelled at
//! their first edit, and the only thing `review` could say was "cancelled".
//!
//! Everything here is best-effort: a missing or unparseable file means "no
//! extra explanation", never a failed run.

use std::path::{Path, PathBuf};

/// `~/.grok`, or `None` when `HOME` is unset.
pub fn default_home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".grok"))
}

/// Whether `dir` is trusted according to a `trusted_folders.toml` body.
///
/// Trust is inherited by subdirectories: a run under `review/target/...` was
/// allowed to edit because `review` is trusted, while `piners/target/...` was
/// not. Matching is component-wise (`Path::starts_with`), so trusting
/// `/a/review` does not trust `/a/review-old`.
pub fn is_trusted(toml_text: &str, dir: &Path) -> bool {
    let Ok(table) = toml_text.parse::<toml::Table>() else {
        return false;
    };
    let Some(folders) = table.get("folders").and_then(|f| f.as_table()) else {
        return false;
    };
    folders.iter().any(|(path, entry)| {
        let trusted = entry
            .get("trusted")
            .and_then(toml::Value::as_bool)
            .unwrap_or(false);
        trusted && dir.starts_with(Path::new(path))
    })
}

/// Why `dir` would have grok cancel its edits, or `None` when it is trusted or
/// trust cannot be determined.
///
/// An unreadable or missing file is treated as "cannot tell" rather than
/// "untrusted": a grok that stores trust elsewhere must not get a false
/// warning on every run.
pub fn untrusted_warning(grok_home: &Path, dir: &Path) -> Option<String> {
    let text = std::fs::read_to_string(grok_home.join("trusted_folders.toml")).ok()?;
    if is_trusted(&text, dir) {
        return None;
    }
    Some(format!(
        "{} is not in grok's trusted folders ({}): grok will cancel the turn at the \
         first file edit. Run `grok` there once and accept the trust prompt.",
        dir.display(),
        grok_home.join("trusted_folders.toml").display()
    ))
}

/// The cause of a cancelled turn, from a session's `events.jsonl` body.
///
/// Takes the *last* `turn_ended` (a resumed session holds several turns), and
/// the last permission refusal before it, which is what names the tool.
pub fn cancel_cause(events: &str) -> Option<String> {
    let mut refused_tool: Option<String> = None;
    let mut cause: Option<String> = None;
    for line in events.lines() {
        let Ok(ev) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        match ev.get("type").and_then(|t| t.as_str()) {
            Some("turn_started") => {
                refused_tool = None;
                cause = None;
            }
            Some("permission_resolved") => {
                let decision = ev.get("decision").and_then(|d| d.as_str());
                if decision.is_some_and(|d| d != "allow") {
                    refused_tool = ev
                        .get("tool_name")
                        .and_then(|t| t.as_str())
                        .map(str::to_string);
                }
            }
            Some("turn_ended") => {
                cause = ev
                    .get("cancellation_category")
                    .and_then(|c| c.as_str())
                    .map(str::to_string);
            }
            _ => {}
        }
    }
    let cause = cause?;
    Some(match refused_tool {
        Some(tool) => format!("{cause}: permission for `{tool}` was refused"),
        None => cause,
    })
}

/// The `events.jsonl` body for a session, found by id under any cwd directory.
///
/// Located by scanning `sessions/*/<id>/` rather than by re-encoding the cwd,
/// so this does not depend on grok's path-encoding scheme.
pub fn session_events(grok_home: &Path, session_id: &str) -> Option<String> {
    if session_id.is_empty()
        || !session_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-')
    {
        return None;
    }
    let sessions = std::fs::read_dir(grok_home.join("sessions")).ok()?;
    sessions.flatten().find_map(|cwd_dir| {
        std::fs::read_to_string(cwd_dir.path().join(session_id).join("events.jsonl")).ok()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const TRUST: &str = r#"
[folders."/home/u/Programs/review"]
trusted = true
decided_at = 1786636049

[folders."/home/u/Programs/distrusted"]
trusted = false
"#;

    #[test]
    fn trust_is_inherited_by_subdirectories_component_wise() {
        assert!(is_trusted(TRUST, Path::new("/home/u/Programs/review")));
        assert!(is_trusted(
            TRUST,
            Path::new("/home/u/Programs/review/target/scratch")
        ));
        assert!(
            !is_trusted(TRUST, Path::new("/home/u/Programs/review-old")),
            "a string prefix is not an ancestor"
        );
        assert!(!is_trusted(TRUST, Path::new("/home/u/Programs/piners")));
        assert!(
            !is_trusted(TRUST, Path::new("/home/u/Programs/distrusted")),
            "an explicit `trusted = false` is not trust"
        );
        assert!(!is_trusted(
            "not toml [",
            Path::new("/home/u/Programs/review")
        ));
    }

    #[test]
    fn warning_only_when_the_file_exists_and_omits_the_dir() {
        let home = PathBuf::from("target/test-scratch").join(crate::config::generate_uuid());
        std::fs::create_dir_all(&home).expect("create scratch grok home");
        assert_eq!(
            untrusted_warning(&home, Path::new("/home/u/Programs/piners")),
            None,
            "no trust file = cannot tell, so no warning"
        );
        std::fs::write(home.join("trusted_folders.toml"), TRUST).expect("write trust file");
        assert_eq!(
            untrusted_warning(&home, Path::new("/home/u/Programs/review")),
            None
        );
        let w = untrusted_warning(&home, Path::new("/home/u/Programs/piners"))
            .expect("untrusted dir warns");
        assert!(w.contains("/home/u/Programs/piners"), "{w}");
        let _ = std::fs::remove_dir_all(&home);
    }

    /// Trimmed from the tail of a real grok 1.0.40 session that was cancelled
    /// at its first edit in an untrusted folder.
    const CANCELLED: &str = r#"{"ts":"2026-09-22T08:43:27.728Z","type":"phase_changed","phase":"streaming_text"}
{"ts":"2026-09-22T08:43:52.896Z","type":"tool_started","tool_name":"search_replace"}
{"ts":"2026-09-22T08:43:52.896Z","type":"permission_requested","tool_name":"search_replace"}
{"ts":"2026-09-22T08:43:52.896Z","type":"permission_resolved","tool_name":"search_replace","decision":"cancelled","wait_ms":0}
{"ts":"2026-09-22T08:43:52.946Z","type":"turn_ended","outcome":"cancelled","cancellation_category":"permission_cancelled"}"#;

    #[test]
    fn cancel_cause_names_the_category_and_the_refused_tool() {
        assert_eq!(
            cancel_cause(CANCELLED).as_deref(),
            Some("permission_cancelled: permission for `search_replace` was refused")
        );
    }

    #[test]
    fn allowed_permissions_are_not_blamed() {
        let events = r#"{"type":"permission_resolved","tool_name":"read_file","decision":"allow"}
{"type":"turn_ended","outcome":"cancelled","cancellation_category":"max_turns"}"#;
        assert_eq!(cancel_cause(events).as_deref(), Some("max_turns"));
        assert_eq!(
            cancel_cause(r#"{"type":"turn_ended","outcome":"end_turn"}"#),
            None
        );
        assert_eq!(cancel_cause(""), None);
    }

    #[test]
    fn session_events_are_found_under_any_cwd_dir_and_ids_are_validated() {
        let home = PathBuf::from("target/test-scratch").join(crate::config::generate_uuid());
        let sid = "7ada244b-4d05-4fc4-b2c3-3b7594ff0577";
        let dir = home.join("sessions/%2Fsome%2Fcwd").join(sid);
        std::fs::create_dir_all(&dir).expect("create session dir");
        std::fs::write(dir.join("events.jsonl"), CANCELLED).expect("write events");
        assert_eq!(session_events(&home, sid).as_deref(), Some(CANCELLED));
        assert_eq!(session_events(&home, "../etc"), None);
        assert_eq!(session_events(&home, "missing"), None);
        let _ = std::fs::remove_dir_all(&home);
    }
}
