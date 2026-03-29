use anyhow::{Result, bail};
use std::path::Path;

use crate::config;

/// Add provider session entries to .review.toml for a given archetype and hostname.
/// Creates a new section if needed, or appends to an existing one.
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

    if content.contains(&section_header) {
        // Section exists — check for conflicts and append new providers
        for (provider, session_id) in sessions {
            // Check if this provider already has an entry
            if has_provider_in_section(&content, &section_header, provider) {
                bail!(
                    "provider '{provider}' already configured in {section_header}\n  \
                     Remove it first or edit .review.toml manually."
                );
            }

            // Find the end of the section (next section header or EOF)
            let insert_pos = find_section_end(&content, &section_header);
            let entry = format!("{provider} = \"{session_id}\"\n");
            content.insert_str(insert_pos, &entry);
        }
    } else {
        // New section
        if !content.ends_with('\n') && !content.is_empty() {
            content.push('\n');
        }
        content.push('\n');
        content.push_str(&section_header);
        content.push('\n');

        for (provider, session_id) in sessions {
            content.push_str(&format!("{provider} = \"{session_id}\"\n"));
        }
    }

    std::fs::write(config_path, content)?;
    Ok(())
}

fn has_provider_in_section(content: &str, section_header: &str, provider: &str) -> bool {
    let Some(section_start) = content.find(section_header) else {
        return false;
    };
    let after_header = section_start + section_header.len();
    let section_body = &content[after_header..];

    // Find end of section (next [...] or EOF)
    let section_end = section_body
        .find("\n[")
        .unwrap_or(section_body.len());

    let section_text = &section_body[..section_end];
    section_text.lines().any(|line| {
        let trimmed = line.trim();
        trimmed.starts_with(provider) && trimmed[provider.len()..].trim_start().starts_with('=')
    })
}

fn find_section_end(content: &str, section_header: &str) -> usize {
    let section_start = content.find(section_header).expect("section must exist");
    let after_header = section_start + section_header.len();
    let section_body = &content[after_header..];

    // Find end of section (next [...] or EOF)
    match section_body.find("\n[") {
        Some(pos) => after_header + pos + 1, // insert before the next section's [
        None => content.len(), // append at EOF
    }
}
