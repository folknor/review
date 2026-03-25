use anyhow::{Result, bail};
use serde::Deserialize;
use std::collections::BTreeMap;
use yaml_front_matter::YamlFrontMatter;

const CONFIG_FILENAME: &str = ".review.md";

pub const BUILTIN_ARCHETYPES: &[&str] = &["security", "bugs", "perf", "arch"];

#[derive(Debug, Deserialize)]
pub struct Frontmatter {
    #[serde(flatten)]
    pub archetypes: BTreeMap<String, ArchetypeConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ArchetypeConfig {
    pub claude: Option<String>,
    pub codex: Option<String>,
}

impl ArchetypeConfig {
    pub fn has_sessions(&self) -> bool {
        self.claude.is_some() || self.codex.is_some()
    }
}

#[derive(Debug)]
pub struct ReviewConfig {
    pub frontmatter: Frontmatter,
    pub archetype_prompts: BTreeMap<String, String>,
}

pub fn load() -> Result<(ReviewConfig, std::path::PathBuf)> {
    let path = find_config()?;
    let project_root = path.parent().expect("config file has parent dir").to_path_buf();
    let raw = std::fs::read_to_string(&path)
        .map_err(|e| anyhow::anyhow!("failed to read {}: {e}", path.display()))?;
    let config = parse(&raw)?;
    Ok((config, project_root))
}

fn find_config() -> Result<std::path::PathBuf> {
    let mut dir = std::env::current_dir()
        .map_err(|e| anyhow::anyhow!("failed to get current directory: {e}"))?;

    loop {
        let candidate = dir.join(CONFIG_FILENAME);
        if candidate.exists() {
            return Ok(candidate);
        }
        // Stop at git root — don't walk above the repository
        if dir.join(".git").exists() {
            bail!(
                "no {CONFIG_FILENAME} found (searched up to git root: {})\n\n\
                 Run `review init` to create one.",
                dir.display()
            );
        }
        if !dir.pop() {
            bail!(
                "no {CONFIG_FILENAME} found in current or parent directories\n\n\
                 Run `review init` to create one."
            );
        }
    }
}

pub fn parse(raw: &str) -> Result<ReviewConfig> {
    let document = YamlFrontMatter::parse::<Frontmatter>(raw).map_err(|e| {
        anyhow::anyhow!(
            "failed to parse {CONFIG_FILENAME}: {e}\n  \
             frontmatter keys must be archetype names ({}) with claude/codex sub-keys",
            BUILTIN_ARCHETYPES.join(", ")
        )
    })?;

    // Validate archetype names
    for name in document.metadata.archetypes.keys() {
        if !BUILTIN_ARCHETYPES.contains(&name.as_str()) {
            bail!(
                "unknown archetype '{name}' in frontmatter\n  supported: {}",
                BUILTIN_ARCHETYPES.join(", ")
            );
        }
    }

    let archetype_prompts = parse_archetype_sections(&document.content);

    Ok(ReviewConfig {
        frontmatter: document.metadata,
        archetype_prompts,
    })
}

fn parse_archetype_sections(body: &str) -> BTreeMap<String, String> {
    let mut sections = BTreeMap::new();
    let mut current_name: Option<String> = None;
    let mut current_content = String::new();

    for line in body.lines() {
        if let Some(heading) = line.strip_prefix("## ") {
            if let Some(name) = current_name.take() {
                let trimmed = current_content.trim().to_string();
                if !trimmed.is_empty() {
                    sections.insert(name, trimmed);
                }
            }
            current_name = Some(heading.trim().to_lowercase());
            current_content = String::new();
        } else if current_name.is_some() {
            current_content.push_str(line);
            current_content.push('\n');
        }
    }

    if let Some(name) = current_name {
        let trimmed = current_content.trim().to_string();
        if !trimmed.is_empty() {
            sections.insert(name, trimmed);
        }
    }

    sections
}

const INIT_TEMPLATE: &str = "\
---
# Add your provider session IDs below.
# Supported archetypes: security, bugs, perf, arch
#
# security:
#   claude: \"your-claude-session-id\"
#   codex: \"your-codex-session-id\"
# bugs:
#   claude: \"your-claude-session-id\"
---

## security

## bugs

## perf

## arch
";

pub fn init() -> Result<()> {
    let path = std::env::current_dir()
        .map_err(|e| anyhow::anyhow!("failed to get current directory: {e}"))?
        .join(CONFIG_FILENAME);

    if path.exists() {
        bail!("{CONFIG_FILENAME} already exists in current directory");
    }

    // Warn if a parent directory already has one
    if let Ok(existing) = find_config() {
        bail!(
            "{CONFIG_FILENAME} already exists at {}\n  \
             Creating another here would shadow it.",
            existing.display()
        );
    }

    std::fs::write(&path, INIT_TEMPLATE)
        .map_err(|e| anyhow::anyhow!("failed to write {CONFIG_FILENAME}: {e}"))?;

    println!("Created {CONFIG_FILENAME}");
    println!();
    println!("Next steps:");
    println!("  1. Add your session IDs to the frontmatter");
    println!("  2. Optionally add review instructions under each ## heading");
    println!("  3. Run: echo \"check for issues\" | review security --staged");
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn parses_frontmatter_and_sections() {
        let raw = "\
---
security:
  claude: \"sess-1\"
bugs:
  claude: \"sess-2\"
  codex: \"sess-3\"
---

## security

Check for auth issues and injection vectors.

## bugs

Look for logic errors and edge cases.
";
        let cfg = parse(raw).unwrap();
        assert_eq!(cfg.frontmatter.archetypes.len(), 2);
        assert_eq!(
            cfg.frontmatter.archetypes["security"].claude.as_deref(),
            Some("sess-1")
        );
        assert!(cfg.frontmatter.archetypes["security"].codex.is_none());
        assert!(cfg.frontmatter.archetypes["bugs"].codex.is_some());

        assert_eq!(cfg.archetype_prompts.len(), 2);
        assert!(cfg.archetype_prompts["security"].contains("auth issues"));
        assert!(cfg.archetype_prompts["bugs"].contains("logic errors"));
    }

    #[test]
    fn rejects_unknown_archetype() {
        let raw = "\
---
foobar:
  claude: \"sess-1\"
---
";
        let result = parse(raw);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("unknown archetype 'foobar'"));
    }

    #[test]
    fn missing_frontmatter_errors() {
        let raw = "# security\n\nSome content\n";
        let result = parse(raw);
        assert!(result.is_err());
    }

    #[test]
    fn empty_sections_not_included() {
        let raw = "\
---
security:
  claude: \"sess-1\"
---

## security

## bugs
";
        let cfg = parse(raw).unwrap();
        assert!(cfg.archetype_prompts.is_empty());
    }

    #[test]
    fn has_sessions_both() {
        let cfg = ArchetypeConfig {
            claude: Some("c".into()),
            codex: Some("x".into()),
        };
        assert!(cfg.has_sessions());
    }

    #[test]
    fn has_sessions_none() {
        let cfg = ArchetypeConfig {
            claude: None,
            codex: None,
        };
        assert!(!cfg.has_sessions());
    }
}
