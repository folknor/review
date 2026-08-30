use anyhow::{Result, bail};
use serde::Deserialize;
use std::collections::BTreeMap;

const CONFIG_FILENAME: &str = ".review.toml";
pub const KNOWN_PROVIDERS: &[&str] = &["claude", "codex", "grok"];

/// Translate `review`'s sandbox vocabulary into a provider's own.
///
/// `review`'s levels, in increasing order of access, are `read-only`,
/// `workspace-write` and `danger-full-access`. The names are codex's, because
/// they were `review`'s documented config surface before any translation
/// existed - adopting a fresh set would have silently changed the meaning of
/// every profile already written.
///
/// The problem this solves is that the names are not interchangeable: codex's
/// writable profile is `workspace-write`, grok's is `workspace`, and grok
/// *hard-errors* on a name it cannot resolve rather than falling back. So a
/// profile written for one provider broke the moment it was pointed at the
/// other, at launch time, for a reason that reads as a missing config file.
///
/// An unrecognised value is passed through verbatim rather than rejected. Grok
/// resolves sandbox names against user-defined profiles in `~/.grok/sandbox.toml`
/// (and codex has its own config surface), so a name `review` does not know may
/// still be one the operator legitimately defined. Rejecting it here would make
/// `review` the reason a valid provider config could not be used; passing it on
/// leaves the provider to validate its own vocabulary, which it does with a
/// better error than we could write. The cost is that a typo reaches the
/// provider - acceptable, because it fails before any turn runs and quotes the
/// offending name.
///
/// The three level names are consequently **reserved**: they always mean
/// `review`'s level, so a custom grok profile named `workspace-write` or
/// `danger-full-access` cannot be selected through a `.review.toml` profile.
/// That is the deliberate trade - the whole point is that these three names mean
/// the same thing whichever provider a profile is pointed at, which fails the
/// moment one provider can reinterpret them. Custom profiles have the entire
/// rest of the namespace, and `read-only` is unaffected either way because every
/// provider already spells it that way.
pub fn sandbox_for(provider: &str, sandbox: &str) -> String {
    match (provider, sandbox) {
        // Grok's built-ins are `read-only`, `workspace` and `none`.
        ("grok", "workspace-write") => "workspace".to_string(),
        ("grok", "danger-full-access") => "none".to_string(),
        // `read-only` happens to be spelled the same everywhere, which is why
        // the default path was portable across providers by luck rather than
        // design.
        _ => sandbox.to_string(),
    }
}

/// Names that can't be archetypes or groups because the CLI routes them
/// elsewhere: `all` (fan-out keyword) and the clap subcommands (`init`,
/// `sessions`, plus the auto-generated `help`). Without this guard such a
/// config entry parses fine but is unreachable - clap intercepts the name.
pub const RESERVED_NAMES: &[&str] = &["all", "init", "sessions", "help"];

#[derive(Debug, Default, Deserialize)]
pub struct AuditConfig {
    #[serde(default)]
    pub private: bool,
    pub id: Option<String>,
}

/// Project-wide defaults under [_defaults]. `providers` is the provider list
/// used when --provider is omitted.
#[derive(Debug, Default, Deserialize)]
pub struct DefaultsConfig {
    #[serde(default)]
    pub providers: Vec<String>,
    /// Seconds of rollout silence, with no final answer written, after which a
    /// codex run is treated as stalled: killed, bundled, and reported as a
    /// failure. `0` disables the check; omitted uses the built-in default
    /// (see `watchdog::Timings`).
    ///
    /// This is a genuine timeout resting on an *empirical* property of codex -
    /// that it wakes itself every few minutes and cannot stay silent - so it is
    /// deliberately tunable per project, and switchable off, in case a future
    /// codex changes cadence. codex-only; claude has no rollout to watch.
    #[serde(default)]
    pub stall_timeout_secs: Option<u64>,
}

#[derive(Debug)]
pub struct ReviewConfig {
    pub archetypes: BTreeMap<String, String>,
    pub groups: BTreeMap<String, Vec<String>>,
    pub audit: AuditConfig,
    pub defaults: DefaultsConfig,
    pub hosts: BTreeMap<String, HostConfig>,
}

/// Per-host config: maps provider name → its named profiles.
#[derive(Debug, Clone, Deserialize)]
pub struct HostConfig {
    #[serde(flatten)]
    pub providers: BTreeMap<String, ProviderProfiles>,
}

/// Per-provider config: maps profile name → profile settings.
#[derive(Debug, Clone, Deserialize)]
pub struct ProviderProfiles {
    #[serde(flatten)]
    pub profiles: BTreeMap<String, Profile>,
}

/// A named settings profile: optional model, effort, sandbox, and env overrides
/// applied to a provider invocation when selected via `--profile`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Profile {
    pub model: Option<String>,
    pub effort: Option<String>,
    /// Sandbox / write-access level, passed as `--sandbox` by both codex (e.g.
    /// `read-only`, `workspace-write`) and grok (which resolves the name against
    /// its own built-in and `sandbox.toml` profiles). Defaults to `read-only`
    /// when unset. Ignored by claude, which has no sandbox on this axis.
    pub sandbox: Option<String>,
    pub env: Option<BTreeMap<String, String>>,
    /// Extra codex `-c key=value` overrides, each passed verbatim as its own
    /// `-c`. Codex-only. Lets a profile force config the CLI doesn't expose - in
    /// particular a custom HTTP-transport provider to dodge the websocket death
    /// path (`model_provider` + `model_providers.<name>`).
    #[serde(default)]
    pub config: Vec<String>,
    /// Extra writable roots for a `workspace-write` run, **added to** the ones
    /// derived from the host (`src/writable_roots.rs`) rather than replacing
    /// them: the derived set covers what any build needs on this machine, and
    /// this covers what a particular project needs beyond that - a data
    /// directory, a sibling checkout, a generated-asset cache.
    ///
    /// Per-profile, which is already per-host, so a path that exists on one
    /// machine cannot leak into a run on another. Ignored unless the profile's
    /// `sandbox` is `workspace-write`, because widening a `read-only` profile
    /// would contradict the only thing that profile promises.
    #[serde(default)]
    pub writable_roots: Vec<String>,
}

impl ReviewConfig {
    /// Resolve a `[host.provider.profile]` settings block, if present.
    pub fn resolve_profile(
        &self,
        hostname: &str,
        provider: &str,
        profile: &str,
    ) -> Option<&Profile> {
        self.hosts
            .get(hostname)?
            .providers
            .get(provider)?
            .profiles
            .get(profile)
    }
}

pub fn hostname() -> String {
    gethostname::gethostname().to_string_lossy().to_string()
}

/// Format a hostname as a TOML key, quoting only if it contains dots.
pub fn toml_key(key: &str) -> String {
    if key.contains('.') {
        format!("\"{key}\"")
    } else {
        key.to_string()
    }
}

/// Generate a short 4-character hex ID for audit directory naming.
pub fn generate_short_id() -> String {
    let bytes = std::fs::File::open("/dev/urandom")
        .and_then(|mut f| {
            use std::io::Read;
            let mut buf = [0u8; 2];
            f.read_exact(&mut buf)?;
            Ok(buf)
        })
        .unwrap_or([0x42, 0x42]);
    format!("{:02x}{:02x}", bytes[0], bytes[1])
}

/// Generate a v4 UUID for provisioning a fresh, persistable session ID.
pub fn generate_uuid() -> String {
    // Read from /proc/sys/kernel/random/uuid (Linux)
    std::fs::read_to_string("/proc/sys/kernel/random/uuid")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| {
            // Fallback: generate a v4 UUID from random bytes
            let mut buf = [0u8; 16];
            if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
                use std::io::Read;
                let _ = f.read_exact(&mut buf);
            }
            buf[6] = (buf[6] & 0x0f) | 0x40; // version 4
            buf[8] = (buf[8] & 0x3f) | 0x80; // variant 1
            format!(
                "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
                buf[0], buf[1], buf[2], buf[3],
                buf[4], buf[5],
                buf[6], buf[7],
                buf[8], buf[9],
                buf[10], buf[11], buf[12], buf[13], buf[14], buf[15]
            )
        })
}

pub fn load() -> Result<(ReviewConfig, std::path::PathBuf)> {
    let path = find_config()?;
    let project_root = path
        .parent()
        .expect("config file has parent dir")
        .to_path_buf();
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
    // Parse to a table first, then peel off the reserved sections by name.
    // Everything left over is a hostname table. This avoids serde `flatten`,
    // which does not coexist with a sibling named field (`archetypes`).
    let mut table: toml::Table = toml::from_str(raw)
        .map_err(|e| anyhow::anyhow!("failed to parse {CONFIG_FILENAME}: {e}"))?;

    let groups: BTreeMap<String, Vec<String>> = match table.remove("_groups") {
        Some(v) => v
            .try_into()
            .map_err(|e| anyhow::anyhow!("[_groups] in {CONFIG_FILENAME}: {e}"))?,
        None => BTreeMap::new(),
    };
    let audit: AuditConfig = match table.remove("_audit") {
        Some(v) => v
            .try_into()
            .map_err(|e| anyhow::anyhow!("[_audit] in {CONFIG_FILENAME}: {e}"))?,
        None => AuditConfig::default(),
    };
    let defaults: DefaultsConfig = match table.remove("_defaults") {
        Some(v) => v
            .try_into()
            .map_err(|e| anyhow::anyhow!("[_defaults] in {CONFIG_FILENAME}: {e}"))?,
        None => DefaultsConfig::default(),
    };
    let archetypes: BTreeMap<String, String> = match table.remove("archetypes") {
        Some(v) => v
            .try_into()
            .map_err(|e| anyhow::anyhow!("[archetypes] in {CONFIG_FILENAME}: {e}"))?,
        None => BTreeMap::new(),
    };

    // Remaining top-level tables are hostname configs.
    let mut hosts: BTreeMap<String, HostConfig> = BTreeMap::new();
    for (key, val) in table {
        let host: HostConfig = val
            .try_into()
            .map_err(|e| anyhow::anyhow!("[{key}.*] in {CONFIG_FILENAME}: {e}"))?;
        hosts.insert(key, host);
    }

    // Reserved names
    for &reserved in RESERVED_NAMES {
        if archetypes.contains_key(reserved) {
            bail!(
                "'{reserved}' is a reserved name and cannot be used as an archetype in {CONFIG_FILENAME}"
            );
        }
    }

    // Validate group names
    for name in groups.keys() {
        for &reserved in RESERVED_NAMES {
            if name == reserved {
                bail!(
                    "'{reserved}' is a reserved name and cannot be used as a group in {CONFIG_FILENAME}"
                );
            }
        }
        if archetypes.contains_key(name) {
            bail!(
                "group '{name}' conflicts with an archetype of the same name in {CONFIG_FILENAME}"
            );
        }
    }

    // Validate group members: must exist, no duplicates, not empty
    for (group_name, members) in &groups {
        if members.is_empty() {
            bail!("group '{group_name}' is empty in {CONFIG_FILENAME}");
        }
        let mut seen = std::collections::HashSet::new();
        for member in members {
            if !archetypes.contains_key(member) {
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

    // Validate provider names in host profile tables.
    for (host, host_cfg) in &hosts {
        for prov_name in host_cfg.providers.keys() {
            if !KNOWN_PROVIDERS.contains(&prov_name.as_str()) {
                bail!(
                    "unknown provider '{prov_name}' in [{host}.{prov_name}.*]\n  \
                     supported: {}",
                    KNOWN_PROVIDERS.join(", ")
                );
            }
        }
    }
    for prov_name in &defaults.providers {
        if !KNOWN_PROVIDERS.contains(&prov_name.as_str()) {
            bail!(
                "unknown provider '{prov_name}' in [_defaults].providers\n  \
                 supported: {}",
                KNOWN_PROVIDERS.join(", ")
            );
        }
    }

    Ok(ReviewConfig {
        archetypes,
        groups,
        audit,
        defaults,
        hosts,
    })
}

const INIT_TEMPLATE_PREFIX: &str = "\
# Archetypes are reviewer personas: a name mapped to a priming prompt.
# Any name works.
#
# [archetypes]
# security = \"You are a security expert for this project. Read the codebase.\"
# bugs = \"You hunt for edge cases and correctness bugs.\"
#
# Providers to fan out to when --provider is omitted:
# [_defaults]
# providers = [\"claude\", \"codex\"]
#
# Groups fan out to multiple archetypes:
# [_groups]
# sweep = [\"security\", \"bugs\"]
#
# Named profiles carry per-provider model/effort/env overrides, selected with
# --profile. Scoped by host . provider . profile:
# [myhostname.claude.opus]
# model = \"Opus 4.8\"
# effort = \"medium\"
# env = { ANTHROPIC_BASE_URL = \"http://localhost:8787\" }
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

    let audit_id = generate_short_id();
    let mut content = INIT_TEMPLATE_PREFIX.replace("myhostname", &toml_key(&hostname()));
    content.push_str(&format!("\n[_audit]\nid = \"{audit_id}\"\n"));
    std::fs::write(&path, content)
        .map_err(|e| anyhow::anyhow!("failed to write {CONFIG_FILENAME}: {e}"))?;

    println!("Created {CONFIG_FILENAME}");
    println!();
    println!("Next steps:");
    println!("  1. Define archetypes under [archetypes] and providers under [_defaults]");
    println!("  2. Run: echo \"check for issues\" | review security");
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn sandbox_levels_translate_per_provider() {
        // The default has to stay portable: it is what every bare run gets and
        // it is the invariant that a run cannot write. Claude is deliberately
        // not in this list - it has no filesystem sandbox to translate to, and
        // `invoke` drops the value rather than mapping it, so asserting a level
        // survives translation for claude would imply an enforcement claude does
        // not make.
        for provider in ["codex", "grok"] {
            assert_eq!(
                sandbox_for(provider, "read-only"),
                "read-only",
                "{provider} must understand the default level"
            );
        }

        // The levels that actually differ. Passing codex's spelling to grok is
        // not a soft failure - grok cannot resolve the profile and refuses to
        // start - so this mapping is what makes one profile vocabulary usable
        // across providers.
        assert_eq!(sandbox_for("codex", "workspace-write"), "workspace-write");
        assert_eq!(sandbox_for("grok", "workspace-write"), "workspace");
        assert_eq!(
            sandbox_for("codex", "danger-full-access"),
            "danger-full-access"
        );
        assert_eq!(sandbox_for("grok", "danger-full-access"), "none");
    }

    #[test]
    fn unknown_sandbox_names_reach_the_provider_unchanged() {
        // Grok resolves names against user-defined profiles in
        // `~/.grok/sandbox.toml`, so a name `review` does not recognise may be
        // perfectly valid. Rewriting or rejecting it would make `review` the
        // reason a working provider config could not be used.
        assert_eq!(
            sandbox_for("grok", "my-custom-profile"),
            "my-custom-profile"
        );
        assert_eq!(
            sandbox_for("codex", "my-custom-profile"),
            "my-custom-profile"
        );
    }

    /// `review`'s documented sandbox vocabulary. Kept here rather than in the
    /// module so it cannot drift into looking like a runtime allowlist - the
    /// translation deliberately passes unknown names through.
    const SANDBOX_LEVELS: &[&str] = &["read-only", "workspace-write", "danger-full-access"];

    #[test]
    fn every_documented_level_maps_somewhere() {
        // Guards the pairing between the documented vocabulary and the match
        // arms: adding a level here without teaching grok about it would
        // otherwise pass it through verbatim and fail at launch.
        for level in SANDBOX_LEVELS {
            let mapped = sandbox_for("grok", level);
            assert!(
                ["read-only", "workspace", "none"].contains(&mapped.as_str()),
                "{level} maps to {mapped}, which is not one of grok's built-in profiles"
            );
        }
    }

    #[test]
    fn parses_archetypes() {
        let raw = "\
[archetypes]
security = \"be a security expert\"
bugs = \"find edge cases\"
";
        let cfg = parse(raw).unwrap();
        assert_eq!(cfg.archetypes.len(), 2);
        assert_eq!(cfg.archetypes["security"], "be a security expert");
        assert_eq!(cfg.archetypes["bugs"], "find edge cases");
    }

    #[test]
    fn parses_profiles() {
        let raw = "\
[archetypes]
bugs = \"find edge cases\"

[myhost.claude.opus]
model = \"Opus 4.8\"
effort = \"medium\"
env = { ANTHROPIC_BASE_URL = \"http://localhost:8787\" }

[myhost.codex.implement]
model = \"gpt-5.6-terra\"
effort = \"high\"
sandbox = \"workspace-write\"
";
        let cfg = parse(raw).unwrap();

        let opus = cfg.resolve_profile("myhost", "claude", "opus").unwrap();
        assert_eq!(opus.model.as_deref(), Some("Opus 4.8"));
        assert_eq!(opus.effort.as_deref(), Some("medium"));
        assert_eq!(opus.sandbox, None);
        assert_eq!(
            opus.env.as_ref().unwrap()["ANTHROPIC_BASE_URL"],
            "http://localhost:8787"
        );

        let implement = cfg.resolve_profile("myhost", "codex", "implement").unwrap();
        assert_eq!(implement.model.as_deref(), Some("gpt-5.6-terra"));
        assert_eq!(implement.effort.as_deref(), Some("high"));
        assert_eq!(implement.sandbox.as_deref(), Some("workspace-write"));

        assert!(cfg.resolve_profile("myhost", "claude", "nope").is_none());
        assert!(cfg.resolve_profile("otherhost", "claude", "opus").is_none());
    }

    #[test]
    fn empty_config_parses() {
        let raw = "";
        let cfg = parse(raw).unwrap();
        assert!(cfg.archetypes.is_empty());
        assert!(cfg.hosts.is_empty());
    }

    #[test]
    fn unknown_provider_in_profile_errors() {
        let raw = "\
[archetypes]
bugs = \"x\"

[myhost.gpt.fast]
model = \"whatever\"
";
        let result = parse(raw);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("unknown provider 'gpt'")
        );
    }

    #[test]
    fn parses_defaults_providers() {
        let raw = "\
[archetypes]
bugs = \"x\"

[_defaults]
providers = [\"claude\", \"codex\"]
";
        let cfg = parse(raw).unwrap();
        assert_eq!(cfg.defaults.providers, vec!["claude", "codex"]);
    }

    #[test]
    fn parses_groups() {
        let raw = "\
[archetypes]
security = \"a\"
bugs = \"b\"

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
[archetypes]
security = \"a\"

[_groups]
sweep = [\"security\", \"nonexistent\"]
";
        let result = parse(raw);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("nonexistent"));
    }

    #[test]
    fn group_name_conflicts_with_archetype() {
        let raw = "\
[archetypes]
security = \"a\"

[_groups]
security = [\"security\"]
";
        let result = parse(raw);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("conflicts"));
    }

    #[test]
    fn reserved_archetype_name_errors() {
        let raw = "\
[archetypes]
all = \"a\"
";
        let result = parse(raw);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("reserved"));
    }

    #[test]
    fn subcommand_names_reserved_as_archetype() {
        for name in ["sessions", "help", "init"] {
            let raw = format!("[archetypes]\n{name} = \"x\"\n");
            let result = parse(&raw);
            assert!(result.is_err(), "'{name}' should be reserved");
            assert!(result.unwrap_err().to_string().contains("reserved"));
        }
    }

    #[test]
    fn subcommand_name_reserved_as_group() {
        let raw = "\
[archetypes]
bugs = \"x\"

[_groups]
sessions = [\"bugs\"]
";
        let result = parse(raw);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("reserved"));
    }
}
