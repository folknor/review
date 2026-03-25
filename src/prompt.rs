use anyhow::{Context, Result};

use crate::config::{self, Archetype};

const DEFAULT_PREFIX: &str = include_str!("../prompts/prefix.md");
const DEFAULT_PROMPT: &str = include_str!("../prompts/default.md");
const SECURITY_PROMPT: &str = include_str!("../prompts/security.md");
const BUGS_PROMPT: &str = include_str!("../prompts/bugs.md");
const PERF_PROMPT: &str = include_str!("../prompts/perf.md");
const ARCH_PROMPT: &str = include_str!("../prompts/arch.md");

fn builtin_prompt(archetype_name: &str) -> &'static str {
    match archetype_name {
        "security" => SECURITY_PROMPT,
        "bugs" => BUGS_PROMPT,
        "perf" => PERF_PROMPT,
        "arch" => ARCH_PROMPT,
        _ => DEFAULT_PROMPT,
    }
}

pub fn assemble(
    prefix_override: &Option<String>,
    archetype_name: &str,
    project_name: &str,
    archetype: &Archetype,
    stdin_instructions: &str,
    content: &str,
) -> Result<String> {
    let prefix_template = match prefix_override {
        Some(path) => std::fs::read_to_string(config::expand_path(path))
            .with_context(|| format!("failed to read prefix prompt: {path}"))?,
        None => DEFAULT_PREFIX.to_string(),
    };

    let prefix = prefix_template
        .replace("{archetype}", archetype_name)
        .replace("{project}", project_name);

    let archetype_prompt = match &archetype.prompt {
        Some(path) => std::fs::read_to_string(config::expand_path(path))
            .with_context(|| format!("failed to read archetype prompt: {path}"))?,
        None => builtin_prompt(archetype_name).to_string(),
    };

    Ok(format!(
        "{prefix}\n\n{archetype_prompt}\n\n{stdin_instructions}\n\n{content}"
    ))
}
