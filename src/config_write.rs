use anyhow::{Context, Result};
use std::path::Path;
use toml_edit::{DocumentMut, Item, Table, Value, value};

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

/// Upsert a provider → session_id entry into a host table.
/// Preserves sibling fields (model, env) when the existing entry is a table.
fn upsert_session(host_table: &mut Table, provider: &str, session_id: &str) {
    match host_table.get_mut(provider) {
        Some(Item::Value(Value::InlineTable(t))) => {
            t.insert("session", session_id.into());
        }
        Some(Item::Table(t)) => {
            t.insert("session", value(session_id));
        }
        _ => {
            host_table.insert(provider, value(session_id));
        }
    }
}

/// Write the outcome of a prime run: session IDs plus (optionally) the prime prompt.
/// Single atomic load-mutate-save — if the write fails, neither sessions nor prompt land.
pub fn write_prime_result(
    config_path: &Path,
    archetype: &str,
    hostname: &str,
    sessions: &[(String, String)],
    store_prompt: Option<&str>,
) -> Result<()> {
    if sessions.is_empty() && store_prompt.is_none() {
        return Ok(());
    }

    let mut doc = load_doc(config_path)?;

    if !sessions.is_empty() {
        let arch_table = doc
            .entry(archetype)
            .or_insert_with(|| Item::Table(Table::new()))
            .as_table_mut()
            .ok_or_else(|| anyhow::anyhow!("[{archetype}] is not a table in .review.toml"))?;

        let host_table = arch_table
            .entry(hostname)
            .or_insert_with(|| Item::Table(Table::new()))
            .as_table_mut()
            .ok_or_else(|| {
                anyhow::anyhow!("[{archetype}.{hostname}] is not a table in .review.toml")
            })?;

        for (provider, session_id) in sessions {
            upsert_session(host_table, provider, session_id);
        }
    }

    if let Some(prompt) = store_prompt {
        let prime_table = doc
            .entry("_prime")
            .or_insert_with(|| Item::Table(Table::new()))
            .as_table_mut()
            .ok_or_else(|| anyhow::anyhow!("[_prime] is not a table in .review.toml"))?;
        prime_table.insert(archetype, value(prompt));
    }

    save_doc(config_path, &doc)
}

/// Append an [_audit] section with the given id to .review.toml.
pub fn append_audit_id(config_path: &Path, id: &str) -> Result<()> {
    let mut doc = load_doc(config_path)?;

    let audit_table = doc
        .entry("_audit")
        .or_insert_with(|| Item::Table(Table::new()))
        .as_table_mut()
        .ok_or_else(|| anyhow::anyhow!("[_audit] is not a table in .review.toml"))?;
    audit_table.insert("id", value(id));

    save_doc(config_path, &doc)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::config;

    fn scratch_file(content: &str) -> std::path::PathBuf {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/test-scratch");
        std::fs::create_dir_all(&dir).unwrap();
        let name = format!(
            "cfg-{}-{:?}.toml",
            std::process::id(),
            std::thread::current().id()
        );
        let path = dir.join(name);
        std::fs::write(&path, content).unwrap();
        path
    }

    fn round_trip_prompt(initial: &str, archetype: &str, prompt: &str) -> (String, String) {
        let path = scratch_file(initial);
        write_prime_result(&path, archetype, "myhost", &[], Some(prompt)).unwrap();
        let serialized = std::fs::read_to_string(&path).unwrap();
        let parsed = config::parse(&serialized).unwrap();
        let stored = parsed.prime.get(archetype).cloned().unwrap();
        let _ = std::fs::remove_file(&path);
        (serialized, stored)
    }

    #[test]
    fn round_trips_plain_prompt() {
        let (_, stored) = round_trip_prompt("", "bugs", "you are a bugs expert");
        assert_eq!(stored, "you are a bugs expert");
    }

    #[test]
    fn round_trips_multiline_prompt() {
        let prompt = "line one\nline two\n\nline four";
        let (_, stored) = round_trip_prompt("", "bugs", prompt);
        assert_eq!(stored, prompt);
    }

    #[test]
    fn round_trips_quotes_and_backslashes() {
        let prompt = r#"she said "hello" and \n was literal and ''' too"#;
        let (_, stored) = round_trip_prompt("", "bugs", prompt);
        assert_eq!(stored, prompt);
    }

    #[test]
    fn round_trips_unicode() {
        let prompt = "café ☃ 日本語 🔥";
        let (_, stored) = round_trip_prompt("", "bugs", prompt);
        assert_eq!(stored, prompt);
    }

    #[test]
    fn round_trips_toml_injection_attempt() {
        let prompt = "\"\"\"\n[evil]\nx = 1\n\"\"\"";
        let (serialized, stored) = round_trip_prompt("", "bugs", prompt);
        assert_eq!(stored, prompt);
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
        let (serialized, _) = round_trip_prompt(initial, "bugs", "prompt");
        assert!(serialized.contains("# top comment"));
        assert!(serialized.contains("[security.myhost]"));
        assert!(serialized.contains("id = \"abcd\""));
    }

    #[test]
    fn session_upserts_bare_string() {
        let path = scratch_file("[bugs.myhost]\nclaude = \"old-sess\"\n");
        write_prime_result(
            &path,
            "bugs",
            "myhost",
            &[("claude".to_string(), "new-sess".to_string())],
            None,
        )
        .unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        let cfg = config::parse(&raw).unwrap();
        let host = cfg.archetypes["bugs"].resolve_host("myhost").unwrap();
        assert_eq!(host.providers["claude"].session(), "new-sess");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn session_preserves_model_and_env_on_inline_upsert() {
        let initial = "\
[bugs.myhost]
claude = { session = \"old-sess\", model = \"opus\", env = { FOO = \"bar\" } }
";
        let path = scratch_file(initial);
        write_prime_result(
            &path,
            "bugs",
            "myhost",
            &[("claude".to_string(), "new-sess".to_string())],
            None,
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

    #[test]
    fn session_preserves_model_on_standard_table_upsert() {
        // User hand-wrote the provider entry as a standard sub-table.
        let initial = "\
[bugs.myhost.claude]
session = \"old-sess\"
model = \"opus\"
";
        let path = scratch_file(initial);
        write_prime_result(
            &path,
            "bugs",
            "myhost",
            &[("claude".to_string(), "new-sess".to_string())],
            None,
        )
        .unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        let cfg = config::parse(&raw).unwrap();
        let entry = &cfg.archetypes["bugs"].resolve_host("myhost").unwrap().providers["claude"];
        assert_eq!(entry.session(), "new-sess");
        assert_eq!(entry.model(), Some("opus"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn session_adds_alongside_existing_provider() {
        let path = scratch_file("[bugs.myhost]\nclaude = \"existing\"\n");
        write_prime_result(
            &path,
            "bugs",
            "myhost",
            &[("codex".to_string(), "new-codex".to_string())],
            None,
        )
        .unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        let cfg = config::parse(&raw).unwrap();
        let host = cfg.archetypes["bugs"].resolve_host("myhost").unwrap();
        assert_eq!(host.providers["claude"].session(), "existing");
        assert_eq!(host.providers["codex"].session(), "new-codex");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn write_prime_result_atomically_writes_sessions_and_prompt() {
        let path = scratch_file("");
        write_prime_result(
            &path,
            "bugs",
            "myhost",
            &[("claude".to_string(), "sess-1".to_string())],
            Some("be a bugs expert"),
        )
        .unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        let cfg = config::parse(&raw).unwrap();
        assert_eq!(
            cfg.archetypes["bugs"].resolve_host("myhost").unwrap().providers["claude"].session(),
            "sess-1"
        );
        assert_eq!(cfg.prime["bugs"], "be a bugs expert");
        let _ = std::fs::remove_file(&path);
    }
}
