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

    if let Some(section_start) = find_uncommented_section(&content, &section_header) {
        // Section exists — check for conflicts and append new providers
        for (provider, session_id) in sessions {
            if has_provider_in_section(&content, section_start, &section_header, provider) {
                bail!(
                    "provider '{provider}' already configured in {section_header}\n  \
                     Remove it first or edit .review.toml manually."
                );
            }

            let insert_pos = find_section_end(&content, section_start, &section_header);
            let mut entry = format!("{provider} = \"{session_id}\"\n");
            // Ensure we're on a new line
            if insert_pos > 0 && !content[..insert_pos].ends_with('\n') {
                entry.insert(0, '\n');
            }
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

/// Find a section header that is NOT inside a comment.
/// Returns the byte offset of the `[` if found.
fn find_uncommented_section(content: &str, section_header: &str) -> Option<usize> {
    let mut search_from = 0;
    while let Some(pos) = content[search_from..].find(section_header) {
        let abs_pos = search_from + pos;

        // Check if this is at the start of a line (not inside a comment)
        let line_start = content[..abs_pos].rfind('\n').map_or(0, |p| p + 1);
        let before_bracket = content[line_start..abs_pos].trim();

        if before_bracket.is_empty() {
            // The `[` is at the start of the line (possibly after whitespace) — real section
            return Some(abs_pos);
        }

        // This match is inside a comment or other content — skip it
        search_from = abs_pos + section_header.len();
    }
    None
}

fn has_provider_in_section(
    content: &str,
    section_start: usize,
    section_header: &str,
    provider: &str,
) -> bool {
    let after_header = section_start + section_header.len();
    let section_body = &content[after_header..];

    let section_end = find_next_section(section_body);
    let section_text = &section_body[..section_end];

    section_text.lines().any(|line| {
        let trimmed = line.trim();
        trimmed.starts_with(provider) && trimmed[provider.len()..].trim_start().starts_with('=')
    })
}

fn find_section_end(content: &str, section_start: usize, section_header: &str) -> usize {
    let after_header = section_start + section_header.len();
    let section_body = &content[after_header..];

    let end_offset = find_next_section(section_body);
    after_header + end_offset
}

/// Find the start of the next uncommented section header, or return the length of the input.
fn find_next_section(body: &str) -> usize {
    for (i, line) in body.split('\n').scan(0usize, |offset, line| {
        let start = *offset;
        *offset += line.len() + 1; // +1 for the \n
        Some((start, line))
    }) {
        // Skip the first line (it's the rest of the current section header line)
        if i == 0 {
            continue;
        }
        let trimmed = line.trim();
        if trimmed.starts_with('[') && !trimmed.starts_with('#') {
            return i;
        }
    }
    body.len()
}
