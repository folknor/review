use anyhow::{Result, bail};
use serde::Deserialize;
use std::collections::BTreeMap;

const CONFIG_FILENAME: &str = ".review.toml";

#[derive(Debug, Deserialize)]
pub struct RawConfig {
    #[serde(default, rename = "_groups")]
    pub groups: BTreeMap<String, Vec<String>>,
    #[serde(flatten)]
    pub archetypes: BTreeMap<String, ArchetypeConfig>,
}

#[derive(Debug)]
pub struct ReviewConfig {
    pub archetypes: BTreeMap<String, ArchetypeConfig>,
    pub groups: BTreeMap<String, Vec<String>>,
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
    let raw_cfg: RawConfig = toml::from_str(raw)
        .map_err(|e| anyhow::anyhow!("failed to parse {CONFIG_FILENAME}: {e}"))?;

    // Reserved names
    for reserved in ["all", "init"] {
        if raw_cfg.archetypes.contains_key(reserved) {
            bail!("'{reserved}' is a reserved name and cannot be used as an archetype in {CONFIG_FILENAME}");
        }
    }

    // Validate group names
    for name in raw_cfg.groups.keys() {
        for reserved in ["all", "init"] {
            if name == reserved {
                bail!("'{reserved}' is a reserved name and cannot be used as a group in {CONFIG_FILENAME}");
            }
        }
        if raw_cfg.archetypes.contains_key(name) {
            bail!("group '{name}' conflicts with an archetype of the same name in {CONFIG_FILENAME}");
        }
    }

    // Validate group members: must exist, no duplicates, not empty
    for (group_name, members) in &raw_cfg.groups {
        if members.is_empty() {
            bail!("group '{group_name}' is empty in {CONFIG_FILENAME}");
        }
        let mut seen = std::collections::HashSet::new();
        for member in members {
            if !raw_cfg.archetypes.contains_key(member) {
                bail!(
                    "group '{group_name}' references unknown archetype '{member}' in {CONFIG_FILENAME}"
                );
            }
            if !seen.insert(member) {
                bail!(
                    "group '{group_name}' contains duplicate archetype '{member}' in {CONFIG_FILENAME}"
                );
            }
        }
    }

    Ok(ReviewConfig {
        archetypes: raw_cfg.archetypes,
        groups: raw_cfg.groups,
    })
}

const INIT_TEMPLATE_PREFIX: &str = "\
# Session IDs are scoped by hostname.
# Archetypes with built-in prompts: security, bugs, perf, arch
# Custom archetype names are also supported.
#
# [security.myhostname]
# claude = \"your-claude-session-id\"
# codex = \"your-codex-session-id\"
#
# [bugs.myhostname]
# claude = \"your-claude-session-id\"
#
# Groups fan out to multiple archetypes:
# [_groups]
# sweep = [\"security\", \"bugs\", \"perf\"]
";

pub fn init() -> Result<()> {
    let path = std::env::current_dir()
        .map_err(|e| anyhow::anyhow!("failed to get current directory: {e}"))?
        .join(CONFIG_FILENAME);

    if path.exists() {
        bail!("{CONFIG_FILENAME} already exists in current directory");
    }

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
    println!("  1. Add your session IDs under [archetype.{host}] tables");
    println!("  2. Run: echo \"check for issues\" | review security");
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn parses_config() {
        let raw = "\
[security.myhost]
claude = \"sess-1\"

[bugs.myhost]
claude = \"sess-2\"
codex = \"sess-3\"
";
        let cfg = parse(raw).unwrap();
        assert_eq!(cfg.archetypes.len(), 2);

        let sec_host = cfg.archetypes["security"].resolve_host("myhost").unwrap();
        assert_eq!(sec_host.claude.as_deref(), Some("sess-1"));
        assert!(sec_host.codex.is_none());

        let bugs_host = cfg.archetypes["bugs"].resolve_host("myhost").unwrap();
        assert!(bugs_host.codex.is_some());
    }

    #[test]
    fn allows_custom_archetype() {
        let raw = "\
[foobar.myhost]
claude = \"sess-1\"
";
        let cfg = parse(raw).unwrap();
        assert!(cfg.archetypes.contains_key("foobar"));
        assert!(cfg.archetypes["foobar"].has_sessions_for_host("myhost"));
    }

    #[test]
    fn empty_config_parses() {
        let raw = "";
        let cfg = parse(raw).unwrap();
        assert!(cfg.archetypes.is_empty());
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
[security.host-a]
claude = \"sess-a\"

[security.host-b]
codex = \"sess-b\"
";
        let cfg = parse(raw).unwrap();
        let sec = &cfg.archetypes["security"];
        assert!(sec.has_sessions_for_host("host-a"));
        assert!(sec.has_sessions_for_host("host-b"));
        assert!(!sec.has_sessions_for_host("host-c"));
        assert_eq!(sec.resolve_host("host-a").unwrap().claude.as_deref(), Some("sess-a"));
        assert_eq!(sec.resolve_host("host-b").unwrap().codex.as_deref(), Some("sess-b"));
    }

    #[test]
    fn parses_groups() {
        let raw = "\
[security.myhost]
claude = \"sess-1\"

[bugs.myhost]
claude = \"sess-2\"

[perf.myhost]
claude = \"sess-3\"

[_groups]
sweep = [\"security\", \"bugs\", \"perf\"]
";
        let cfg = parse(raw).unwrap();
        assert_eq!(cfg.groups.len(), 1);
        assert_eq!(cfg.groups["sweep"], vec!["security", "bugs", "perf"]);
        assert!(!cfg.archetypes.contains_key("_groups"));
    }

    #[test]
    fn group_with_unknown_member_errors() {
        let raw = "\
[security.myhost]
claude = \"sess-1\"

[_groups]
sweep = [\"security\", \"nonexistent\"]
";
        let result = parse(raw);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("nonexistent"));
    }

    #[test]
    fn group_name_conflicts_with_archetype() {
        let raw = "\
[security.myhost]
claude = \"sess-1\"

[_groups]
security = [\"security\"]
";
        let result = parse(raw);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("conflicts"));
    }
}
