use anyhow::{Result, bail};

use crate::config::{self, Archetype, Config};

pub fn register(
    config: &mut Config,
    archetype: &str,
    claude: Option<String>,
    codex: Option<String>,
) -> Result<()> {
    if claude.is_none() && codex.is_none() {
        bail!("provide at least one of --claude or --codex");
    }

    let (_name, project) = config::resolve_project_mut(config)?;

    let entry = project
        .archetypes
        .entry(archetype.to_string())
        .or_insert_with(|| Archetype {
            claude: None,
            codex: None,
            prompt_diff: format!("~/.config/review/prompts/{archetype}/diff.md"),
            prompt_document: format!("~/.config/review/prompts/{archetype}/document.md"),
        });

    if let Some(id) = claude {
        entry.claude = Some(id);
    }
    if let Some(id) = codex {
        entry.codex = Some(id);
    }

    config::save(config)?;
    println!("registered session(s) for archetype '{archetype}'");
    Ok(())
}

pub fn deregister(
    config: &mut Config,
    archetype: &str,
    claude_only: bool,
    codex_only: bool,
) -> Result<()> {
    let (_name, project) = config::resolve_project_mut(config)?;

    if !project.archetypes.contains_key(archetype) {
        let available: Vec<_> = project.archetypes.keys().collect();
        bail!(
            "unknown archetype '{archetype}'\n  available: {}",
            if available.is_empty() {
                "(none)".to_string()
            } else {
                available
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            }
        );
    }

    if claude_only || codex_only {
        let entry = project.archetypes.get_mut(archetype).unwrap();
        if claude_only {
            entry.claude = None;
        }
        if codex_only {
            entry.codex = None;
        }
    } else {
        project.archetypes.remove(archetype);
    }

    config::save(config)?;
    println!("deregistered '{archetype}'");
    Ok(())
}

pub fn list(config: &Config, all: bool) -> Result<()> {
    if all {
        for (name, project) in &config.projects {
            print_project(name, project);
        }
    } else {
        let (name, project) = config::resolve_project(config)?;
        print_project(&name, project);
    }
    Ok(())
}

fn print_project(name: &str, project: &config::Project) {
    println!("{name} ({})", project.path);
    if project.archetypes.is_empty() {
        println!("  (no archetypes)");
        return;
    }
    for (arch_name, arch) in &project.archetypes {
        let providers: Vec<&str> = [
            arch.claude.as_deref().map(|_| "claude"),
            arch.codex.as_deref().map(|_| "codex"),
        ]
        .into_iter()
        .flatten()
        .collect();
        println!("  {arch_name}: {}", providers.join(", "));
    }
}
