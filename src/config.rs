use anyhow::{Result, bail};
use serde::Deserialize;
use std::collections::BTreeMap;

const CONFIG_FILENAME: &str = ".review.toml";
pub const KNOWN_PROVIDERS: &[&str] = &["claude", "codex", "kilo", "opencode"];

#[derive(Debug, Default, Deserialize)]
pub struct AuditConfig {
    #[serde(default)]
    pub private: bool,
}

#[derive(Debug, Deserialize)]
pub struct RawConfig {
    #[serde(default, rename = "_groups")]
    pub groups: BTreeMap<String, Vec<String>>,
    #[serde(default, rename = "_audit")]
    pub audit: AuditConfig,
    #[serde(flatten)]
    pub archetypes: BTreeMap<String, ArchetypeConfig>,
}

#[derive(Debug)]
pub struct ReviewConfig {
    pub archetypes: BTreeMap<String, ArchetypeConfig>,
    pub groups: BTreeMap<String, Vec<String>>,
    pub audit: AuditConfig,
}

/// Per-archetype config: maps hostname → host config.
#[derive(Debug, Clone, Deserialize)]
pub struct ArchetypeConfig {
    #[serde(flatten)]
    pub hosts: BTreeMap<String, HostConfig>,
}

/// Per-host config: maps provider name → provider entry.
#[derive(Debug, Clone, Deserialize)]
pub struct HostConfig {
    #[serde(flatten)]
    pub providers: BTreeMap<String, ProviderEntry>,
}

/// A provider entry: either just a session ID string, or a table with session + model.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum ProviderEntry {
    SessionOnly(String),
    Full { session: String, model: Option<String> },
}

impl ProviderEntry {
    pub fn session(&self) -> &str {
        match self {
            Self::SessionOnly(s) => s,
            Self::Full { session, .. } => session,
        }
    }

    pub fn model(&self) -> Option<&str> {
        match self {
            Self::SessionOnly(_) => None,
            Self::Full { model, .. } => model.as_deref(),
        }
    }
}

impl ArchetypeConfig {
    pub fn resolve_host(&self, hostname: &str) -> Option<&HostConfig> {
        self.hosts.get(hostname)
    }

    pub fn has_sessions_for_host(&self, hostname: &str) -> bool {
        self.resolve_host(hostname)
            .map(|h| !h.providers.is_empty())
            .unwrap_or(false)
    }
}

pub fn hostname() -> String {
    gethostname::gethostname()
        .to_string_lossy()
        .to_string()
}

/// Format a hostname as a TOML key, quoting only if it contains dots.
pub fn toml_key(key: &str) -> String {
    if key.contains('.') {
        format!("\"{key}\"")
    } else {
        key.to_string()
    }
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

    // Validate provider names
    for (arch_name, arch) in &raw_cfg.archetypes {
        for (hostname, host) in &arch.hosts {
            for prov_name in host.providers.keys() {
                if !KNOWN_PROVIDERS.contains(&prov_name.as_str()) {
                    bail!(
                        "unknown provider '{prov_name}' in [{arch_name}.{hostname}]\n  \
                         supported: {}",
                        KNOWN_PROVIDERS.join(", ")
                    );
                }
            }
        }
    }

    Ok(ReviewConfig {
        archetypes: raw_cfg.archetypes,
        groups: raw_cfg.groups,
        audit: raw_cfg.audit,
    })
}

const INIT_TEMPLATE_PREFIX: &str = "\
# Session IDs are scoped by hostname.
# Any archetype name works. Provider options: claude, codex, kilo, opencode
#
# [security.myhostname]
# claude = \"your-session-id\"
# codex = { session = \"your-session-id\", model = \"o3\" }
# kilo = { session = \"your-session-id\", model = \"anthropic/claude-sonnet-4.6\" }
#
# Groups fan out to multiple archetypes:
# [_groups]
# sweep = [\"security\", \"bugs\"]
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
    let content = INIT_TEMPLATE_PREFIX.replace("myhostname", &toml_key(&host));
    std::fs::write(&path, content)
        .map_err(|e| anyhow::anyhow!("failed to write {CONFIG_FILENAME}: {e}"))?;

    let host_key = toml_key(&host);
    println!("Created {CONFIG_FILENAME}");
    println!();
    println!("Next steps:");
    println!("  1. Add your session IDs under [archetype.{host_key}] tables");
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
        assert_eq!(sec_host.providers["claude"].session(), "sess-1");

        let bugs_host = cfg.archetypes["bugs"].resolve_host("myhost").unwrap();
        assert_eq!(bugs_host.providers["codex"].session(), "sess-3");
    }

    #[test]
    fn parses_full_provider_entry() {
        let raw = "\
[bugs.myhost]
claude = { session = \"sess-1\", model = \"opus\" }
kilo = { session = \"sess-2\", model = \"anthropic/claude-sonnet-4.6\" }
";
        let cfg = parse(raw).unwrap();
        let host = cfg.archetypes["bugs"].resolve_host("myhost").unwrap();

        assert_eq!(host.providers["claude"].session(), "sess-1");
        assert_eq!(host.providers["claude"].model(), Some("opus"));
        assert_eq!(host.providers["kilo"].session(), "sess-2");
        assert_eq!(host.providers["kilo"].model(), Some("anthropic/claude-sonnet-4.6"));
    }

    #[test]
    fn mixed_string_and_table_entries() {
        let raw = "\
[bugs.myhost]
claude = \"sess-1\"
codex = { session = \"sess-2\", model = \"o3\" }
";
        let cfg = parse(raw).unwrap();
        let host = cfg.archetypes["bugs"].resolve_host("myhost").unwrap();

        assert_eq!(host.providers["claude"].session(), "sess-1");
        assert!(host.providers["claude"].model().is_none());
        assert_eq!(host.providers["codex"].session(), "sess-2");
        assert_eq!(host.providers["codex"].model(), Some("o3"));
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
        let raw = "\
[bugs.myhost]
claude = \"sess-1\"
";
        let cfg = parse(raw).unwrap();
        assert!(cfg.archetypes["bugs"].has_sessions_for_host("myhost"));
        assert!(!cfg.archetypes["bugs"].has_sessions_for_host("otherhost"));
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
    }

    #[test]
    fn parses_groups() {
        let raw = "\
[security.myhost]
claude = \"sess-1\"

[bugs.myhost]
claude = \"sess-2\"

[_groups]
sweep = [\"security\", \"bugs\"]
";
        let cfg = parse(raw).unwrap();
        assert_eq!(cfg.groups.len(), 1);
        assert_eq!(cfg.groups["sweep"], vec!["security", "bugs"]);
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
