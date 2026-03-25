use anyhow::{Result, bail};

use crate::config::{self, Archetype, Config, Project};

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

    register_on_project(project, archetype, claude, codex);

    let entry = project.archetypes.get(archetype).expect("archetype just inserted");
    let prompt_diff = entry.prompt_diff.clone();
    let prompt_document = entry.prompt_document.clone();

    config::save(config)?;
    println!("registered session(s) for archetype '{archetype}'");

    for (label, path) in [("diff", &prompt_diff), ("document", &prompt_document)] {
        if !config::expand_path(path).exists() {
            eprintln!("warning: {label} prompt file does not exist: {path}");
        }
    }

    Ok(())
}

fn register_on_project(
    project: &mut Project,
    archetype: &str,
    claude: Option<String>,
    codex: Option<String>,
) {
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
}

pub fn deregister(
    config: &mut Config,
    archetype: &str,
    claude_only: bool,
    codex_only: bool,
) -> Result<()> {
    let (_name, project) = config::resolve_project_mut(config)?;

    deregister_on_project(project, archetype, claude_only, codex_only)?;

    config::save(config)?;
    println!("deregistered '{archetype}'");
    Ok(())
}

fn deregister_on_project(
    project: &mut Project,
    archetype: &str,
    claude_only: bool,
    codex_only: bool,
) -> Result<()> {
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
        let entry = project.archetypes.get_mut(archetype).expect("archetype existence checked above");
        if claude_only {
            entry.claude = None;
        }
        if codex_only {
            entry.codex = None;
        }
        // If both sessions are now gone, remove the archetype entirely
        if !entry.has_sessions() {
            project.archetypes.remove(archetype);
        }
    } else {
        project.archetypes.remove(archetype);
    }

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

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn test_project() -> Project {
        let mut archetypes = BTreeMap::new();
        archetypes.insert(
            "security".to_string(),
            Archetype {
                claude: Some("claude-123".into()),
                codex: Some("codex-456".into()),
                prompt_diff: "d".into(),
                prompt_document: "d".into(),
            },
        );
        Project {
            path: "/test".to_string(),
            archetypes,
        }
    }

    #[test]
    fn deregister_no_flags_removes_archetype() {
        let mut project = test_project();
        deregister_on_project(&mut project, "security", false, false).unwrap();
        assert!(!project.archetypes.contains_key("security"));
    }

    #[test]
    fn deregister_claude_only_keeps_codex() {
        let mut project = test_project();
        deregister_on_project(&mut project, "security", true, false).unwrap();
        let entry = project.archetypes.get("security").unwrap();
        assert!(entry.claude.is_none());
        assert!(entry.codex.is_some());
    }

    #[test]
    fn deregister_codex_only_keeps_claude() {
        let mut project = test_project();
        deregister_on_project(&mut project, "security", false, true).unwrap();
        let entry = project.archetypes.get("security").unwrap();
        assert!(entry.claude.is_some());
        assert!(entry.codex.is_none());
    }

    #[test]
    fn deregister_both_flags_removes_archetype() {
        let mut project = test_project();
        deregister_on_project(&mut project, "security", true, true).unwrap();
        assert!(
            !project.archetypes.contains_key("security"),
            "archetype should be removed when all sessions are cleared"
        );
    }

    #[test]
    fn deregister_unknown_archetype_errors() {
        let mut project = test_project();
        let result = deregister_on_project(&mut project, "nonexistent", false, false);
        assert!(result.is_err());
    }

    #[test]
    fn register_creates_new_archetype() {
        let mut project = Project {
            path: "/test".to_string(),
            archetypes: BTreeMap::new(),
        };
        register_on_project(&mut project, "perf", Some("c-1".into()), None);
        let entry = project.archetypes.get("perf").unwrap();
        assert_eq!(entry.claude.as_deref(), Some("c-1"));
        assert!(entry.codex.is_none());
    }

    #[test]
    fn register_updates_existing_archetype() {
        let mut project = test_project();
        register_on_project(&mut project, "security", None, Some("new-codex".into()));
        let entry = project.archetypes.get("security").unwrap();
        assert_eq!(entry.claude.as_deref(), Some("claude-123"));
        assert_eq!(entry.codex.as_deref(), Some("new-codex"));
    }
}
