use anyhow::{Context, Result, bail};
use std::path::Path;
use toml_edit::{DocumentMut, Item, Table, value};

/// Load .review.toml as an editable document (or an empty doc if missing).
fn load_doc(config_path: &Path) -> Result<DocumentMut> {
    let raw = std::fs::read_to_string(config_path).unwrap_or_default();
    raw.parse::<DocumentMut>()
        .with_context(|| format!("failed to parse {}", config_path.display()))
}

fn save_doc(config_path: &Path, doc: &DocumentMut) -> Result<()> {
    std::fs::write(config_path, doc.to_string())
        .with_context(|| format!("failed to write {}", config_path.display()))
}

/// Add provider session entries to .review.toml for a given archetype and hostname.
pub fn append_sessions(
    config_path: &Path,
    archetype: &str,
    hostname: &str,
    sessions: &[(String, String)],
) -> Result<()> {
    if sessions.is_empty() {
        return Ok(());
    }

    let mut doc = load_doc(config_path)?;

    let arch_item = doc
        .entry(archetype)
        .or_insert_with(|| Item::Table(Table::new()));
    let arch_table = arch_item
        .as_table_mut()
        .ok_or_else(|| anyhow::anyhow!("[{archetype}] is not a table in .review.toml"))?;

    let host_item = arch_table
        .entry(hostname)
        .or_insert_with(|| Item::Table(Table::new()));
    let host_table = host_item.as_table_mut().ok_or_else(|| {
        anyhow::anyhow!("[{archetype}.{hostname}] is not a table in .review.toml")
    })?;

    for (provider, session_id) in sessions {
        // Upsert: re-priming replaces the stale session ID with a fresh one.
        // If the existing entry is an inline table (with model/env overrides),
        // update just the `session` field so those overrides survive.
        match host_table.get_mut(provider) {
            Some(Item::Value(toml_edit::Value::InlineTable(t))) => {
                t.insert("session", session_id.as_str().into());
            }
            _ => {
                host_table.insert(provider, value(session_id.as_str()));
            }
        }
    }

    save_doc(config_path, &doc)
}

/// Append an [_audit] section with the given id to .review.toml.
pub fn append_audit_id(config_path: &Path, id: &str) -> Result<()> {
    let mut doc = load_doc(config_path)?;

    let audit_item = doc
        .entry("_audit")
        .or_insert_with(|| Item::Table(Table::new()));
    let audit_table = audit_item
        .as_table_mut()
        .ok_or_else(|| anyhow::anyhow!("[_audit] is not a table in .review.toml"))?;
    audit_table.insert("id", value(id));

    save_doc(config_path, &doc)
}

/// Store a prime prompt for an archetype under [_prime]. Errors if one already exists.
pub fn insert_prime_prompt(
    config_path: &Path,
    archetype: &str,
    prompt: &str,
) -> Result<()> {
    let mut doc = load_doc(config_path)?;
    insert_prime_into_doc(&mut doc, archetype, prompt)?;
    save_doc(config_path, &doc)
}

fn insert_prime_into_doc(doc: &mut DocumentMut, archetype: &str, prompt: &str) -> Result<()> {
    let prime_item = doc
        .entry("_prime")
        .or_insert_with(|| Item::Table(Table::new()));
    let prime_table = prime_item
        .as_table_mut()
        .ok_or_else(|| anyhow::anyhow!("[_prime] is not a table in .review.toml"))?;

    if prime_table.contains_key(archetype) {
        bail!(
            "prime prompt for '{archetype}' already stored in [_prime]\n  \
             Remove it from .review.toml first if you want to replace it."
        );
    }

    // toml_edit escapes safely for any input; default is a basic string with \n escapes.
    prime_table.insert(archetype, value(prompt));
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::config;

    fn round_trip(initial: &str, archetype: &str, prompt: &str) -> (String, String) {
        let mut doc: DocumentMut = initial.parse().unwrap();
        insert_prime_into_doc(&mut doc, archetype, prompt).unwrap();
        let serialized = doc.to_string();
        let parsed = config::parse(&serialized).unwrap();
        let stored = parsed.prime.get(archetype).cloned().unwrap();
        (serialized, stored)
    }

    #[test]
    fn round_trips_plain_prompt() {
        let (_, stored) = round_trip("", "bugs", "you are a bugs expert");
        assert_eq!(stored, "you are a bugs expert");
    }

    #[test]
    fn round_trips_multiline_prompt() {
        let prompt = "line one\nline two\n\nline four";
        let (_, stored) = round_trip("", "bugs", prompt);
        assert_eq!(stored, prompt);
    }

    #[test]
    fn round_trips_quotes_and_backslashes() {
        let prompt = r#"she said "hello" and \n was literal and ''' too"#;
        let (_, stored) = round_trip("", "bugs", prompt);
        assert_eq!(stored, prompt);
    }

    #[test]
    fn round_trips_control_characters() {
        let prompt = "tab\there\nnull\x00byte\x01ctrl\x7fdel";
        let (_, stored) = round_trip("", "bugs", prompt);
        assert_eq!(stored, prompt);
    }

    #[test]
    fn round_trips_unicode() {
        let prompt = "café ☃ 日本語 🔥";
        let (_, stored) = round_trip("", "bugs", prompt);
        assert_eq!(stored, prompt);
    }

    #[test]
    fn round_trips_toml_injection_attempt() {
        // An attacker-supplied prompt trying to break out of the string
        // and inject a new section should be harmlessly escaped.
        let prompt = "\"\"\"\n[evil]\nx = 1\n\"\"\"";
        let (serialized, stored) = round_trip("", "bugs", prompt);
        assert_eq!(stored, prompt);
        // Make sure no rogue [evil] section was introduced.
        let parsed = config::parse(&serialized).unwrap();
        assert!(!parsed.archetypes.contains_key("evil"));
    }

    #[test]
    fn preserves_existing_content_and_comments() {
        let initial = "\
# top comment
[security.myhost]
claude = \"sess-1\"

[_audit]
id = \"abcd\"
";
        let (serialized, _) = round_trip(initial, "bugs", "prompt");
        assert!(serialized.contains("# top comment"));
        assert!(serialized.contains("[security.myhost]"));
        assert!(serialized.contains("id = \"abcd\""));
    }

    #[test]
    fn append_sessions_upserts_bare_string() {
        let initial = "[bugs.myhost]\nclaude = \"old-sess\"\n";
        let path = scratch_file(initial);
        append_sessions(
            &path,
            "bugs",
            "myhost",
            &[("claude".to_string(), "new-sess".to_string())],
        )
        .unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        let cfg = config::parse(&raw).unwrap();
        let host = cfg.archetypes["bugs"].resolve_host("myhost").unwrap();
        assert_eq!(host.providers["claude"].session(), "new-sess");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn append_sessions_preserves_model_and_env_on_upsert() {
        let initial = "\
[bugs.myhost]
claude = { session = \"old-sess\", model = \"opus\", env = { FOO = \"bar\" } }
";
        let path = scratch_file(initial);
        append_sessions(
            &path,
            "bugs",
            "myhost",
            &[("claude".to_string(), "new-sess".to_string())],
        )
        .unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        let cfg = config::parse(&raw).unwrap();
        let entry = &cfg.archetypes["bugs"].resolve_host("myhost").unwrap().providers["claude"];
        assert_eq!(entry.session(), "new-sess");
        assert_eq!(entry.model(), Some("opus"));
        assert_eq!(entry.env().unwrap()["FOO"], "bar");
        let _ = std::fs::remove_file(&path);
    }

    fn scratch_file(content: &str) -> std::path::PathBuf {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/test-scratch");
        std::fs::create_dir_all(&dir).unwrap();
        let name = format!("cfg-{}-{:?}.toml", std::process::id(), std::thread::current().id());
        let path = dir.join(name);
        std::fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn errors_when_prompt_already_stored() {
        let initial = "[_prime]\nbugs = \"existing\"\n";
        let mut doc: DocumentMut = initial.parse().unwrap();
        let err = insert_prime_into_doc(&mut doc, "bugs", "new").unwrap_err();
        assert!(err.to_string().contains("already stored"));
    }
}
