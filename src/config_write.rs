use anyhow::{Context, Result};
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

    #[test]
    fn append_audit_id_adds_section() {
        let path = scratch_file("[archetypes]\nbugs = \"x\"\n");
        append_audit_id(&path, "abcd").unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        let cfg = config::parse(&raw).unwrap();
        assert_eq!(cfg.audit.id.as_deref(), Some("abcd"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn append_audit_id_preserves_existing_content() {
        let initial = "\
# top comment
[archetypes]
bugs = \"find edge cases\"
";
        let path = scratch_file(initial);
        append_audit_id(&path, "beef").unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("# top comment"));
        assert!(raw.contains("bugs = \"find edge cases\""));
        assert!(raw.contains("beef"));
        let _ = std::fs::remove_file(&path);
    }
}
