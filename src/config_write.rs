use anyhow::{Result, bail};
use std::path::Path;

use crate::config;

/// Append provider session entries to .review.toml for a given archetype and hostname.
pub fn append_sessions(
    config_path: &Path,
    archetype: &str,
    hostname: &str,
    sessions: &[(String, String)], // (provider, session_id)
) -> Result<()> {
    if sessions.is_empty() {
        return Ok(());
    }

    let mut content = std::fs::read_to_string(config_path)
        .unwrap_or_default();

    let host_key = config::toml_key(hostname);
    let section_header = format!("[{archetype}.{host_key}]");

    // Check if the section already exists
    if content.contains(&section_header) {
        bail!(
            "section {section_header} already exists in .review.toml\n  \
             Remove it first or edit manually."
        );
    }

    // Append the new section
    if !content.ends_with('\n') && !content.is_empty() {
        content.push('\n');
    }
    content.push('\n');
    content.push_str(&section_header);
    content.push('\n');

    for (provider, session_id) in sessions {
        content.push_str(&format!("{provider} = \"{session_id}\"\n"));
    }

    std::fs::write(config_path, content)?;
    Ok(())
}
