use anyhow::{Context, Result};

use crate::config::{self, Archetype};
use crate::input::ContentType;

pub fn assemble(
    prefix_path: &str,
    archetype_name: &str,
    project_name: &str,
    archetype: &Archetype,
    content_type: &ContentType,
    content: &str,
) -> Result<String> {
    let prefix_template = std::fs::read_to_string(config::expand_path(prefix_path))
        .context("failed to read prefix prompt")?;

    let prefix = prefix_template
        .replace("{archetype}", archetype_name)
        .replace("{project}", project_name);

    let archetype_prompt_path = match content_type {
        ContentType::Diff => &archetype.prompt_diff,
        ContentType::Document => &archetype.prompt_document,
    };
    let archetype_prompt =
        std::fs::read_to_string(config::expand_path(archetype_prompt_path))
            .with_context(|| format!("failed to read archetype prompt: {archetype_prompt_path}"))?;

    Ok(format!("{prefix}\n\n{archetype_prompt}\n\n{content}"))
}
