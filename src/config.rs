use anyhow::{Result, bail};
use serde::Deserialize;
use std::collections::BTreeMap;
use yaml_front_matter::YamlFrontMatter;

const CONFIG_FILENAME: &str = ".review.md";

#[derive(Debug, Deserialize)]
pub struct Frontmatter {
    #[serde(flatten)]
    pub archetypes: BTreeMap<String, ArchetypeConfig>,
}

/// Per-archetype config: maps hostname → provider sessions.
#[derive(Debug, Clone, Deserialize)]
pub struct ArchetypeConfig {
    #[serde(flatten)]
    pub hosts: BTreeMap<String, HostConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct HostConfig {
    pub claude: Option<String>,
    pub codex: Option<String>,
}

impl ArchetypeConfig {
    /// Resolve config for the current hostname. Returns None if this host has no entry.
    pub fn resolve_host(&self, hostname: &str) -> Option<&HostConfig> {
        self.hosts.get(hostname)
    }

    pub fn has_sessions_for_host(&self, hostname: &str) -> bool {
        self.resolve_host(hostname)
            .map(|h| h.claude.is_some() || h.codex.is_some())
            .unwrap_or(false)
    }
}

pub fn hostname() -> String {
    gethostname::gethostname()
        .to_string_lossy()
        .to_string()
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
             frontmatter keys must be archetype names with hostname/provider sub-keys"
        )
    })?;

    // "all" is reserved
    if document.metadata.archetypes.contains_key("all") {
        bail!("'all' is a reserved name and cannot be used as an archetype in {CONFIG_FILENAME}");
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

const INIT_TEMPLATE_PREFIX: &str = "\
---
# Session IDs are scoped by hostname.
# Supported archetypes: security, bugs, perf, arch
#
# security:
#   myhostname:
#     claude: \"your-claude-session-id\"
#     codex: \"your-codex-session-id\"
# bugs:
#   myhostname:
#     claude: \"your-claude-session-id\"
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

    let host = hostname();
    let content = INIT_TEMPLATE_PREFIX.replace("myhostname", &host);
    std::fs::write(&path, content)
        .map_err(|e| anyhow::anyhow!("failed to write {CONFIG_FILENAME}: {e}"))?;

    println!("Created {CONFIG_FILENAME}");
    println!();
    println!("Next steps:");
    println!("  1. Add your session IDs under your hostname ({host}) in the frontmatter");
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
  myhost:
    claude: \"sess-1\"
bugs:
  myhost:
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

        let sec_host = cfg.frontmatter.archetypes["security"].resolve_host("myhost").unwrap();
        assert_eq!(sec_host.claude.as_deref(), Some("sess-1"));
        assert!(sec_host.codex.is_none());

        let bugs_host = cfg.frontmatter.archetypes["bugs"].resolve_host("myhost").unwrap();
        assert!(bugs_host.codex.is_some());

        assert_eq!(cfg.archetype_prompts.len(), 2);
        assert!(cfg.archetype_prompts["security"].contains("auth issues"));
        assert!(cfg.archetype_prompts["bugs"].contains("logic errors"));
    }

    #[test]
    fn allows_custom_archetype() {
        let raw = "\
---
foobar:
  myhost:
    claude: \"sess-1\"
---
";
        let cfg = parse(raw).unwrap();
        assert!(cfg.frontmatter.archetypes.contains_key("foobar"));
        assert!(cfg.frontmatter.archetypes["foobar"].has_sessions_for_host("myhost"));
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
  myhost:
    claude: \"sess-1\"
---

## security

## bugs
";
        let cfg = parse(raw).unwrap();
        assert!(cfg.archetype_prompts.is_empty());
    }

    #[test]
    fn has_sessions_for_matching_host() {
        let mut hosts = BTreeMap::new();
        hosts.insert("myhost".to_string(), HostConfig {
            claude: Some("c".into()),
            codex: Some("x".into()),
        });
        let cfg = ArchetypeConfig { hosts };
        assert!(cfg.has_sessions_for_host("myhost"));
        assert!(!cfg.has_sessions_for_host("otherhost"));
    }

    #[test]
    fn no_sessions_for_any_host() {
        let cfg = ArchetypeConfig { hosts: BTreeMap::new() };
        assert!(!cfg.has_sessions_for_host("myhost"));
    }

    #[test]
    fn multiple_hosts() {
        let raw = "\
---
security:
  host-a:
    claude: \"sess-a\"
  host-b:
    codex: \"sess-b\"
---
";
        let cfg = parse(raw).unwrap();
        let sec = &cfg.frontmatter.archetypes["security"];
        assert!(sec.has_sessions_for_host("host-a"));
        assert!(sec.has_sessions_for_host("host-b"));
        assert!(!sec.has_sessions_for_host("host-c"));
        assert_eq!(sec.resolve_host("host-a").unwrap().claude.as_deref(), Some("sess-a"));
        assert_eq!(sec.resolve_host("host-b").unwrap().codex.as_deref(), Some("sess-b"));
    }
}
