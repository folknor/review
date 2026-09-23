#[cfg(test)]
#[path = "config_tests.rs"]
mod config_tests;

use anyhow::{Result, bail};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::PathBuf;

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

/// Names that can't be archetypes or groups: `all`, the `-a` keyword for every
/// configured archetype, and the subcommand names. `-a` itself cannot collide
/// with a subcommand, but the legacy positional form can - `review sessions`
/// runs the subcommand, silently, rather than the archetype - so they stay
/// reserved while that alias exists. Keep in step with `cli::Command` and the
/// "Reserved words" table in CLAUDE.md.
pub const RESERVED_NAMES: &[&str] = &[
    "all",
    "resume",
    "config",
    "sessions",
    "incidents",
    "init",
    "help",
];

/// The legacy positional `review bare` means "no archetype" regardless of the
/// config, so an archetype named `bare` must mean that too: an empty prime.
/// `bare = ""` is in most existing configs and stays valid; anything else would
/// be silently ignored by `review bare`.
const BARE: &str = "bare";

/// Which file a resolved value came from.
///
/// Resolution is POSIX-tool style: the command line beats the project's
/// `.review.toml`, which beats the operator's global config. Nothing is built
/// in. The split exists because the two files answer different questions: a
/// project owns its domain archetypes, while *which model serves a tier* is the
/// operator's current opinion, which changes with every model release and used
/// to be restated per project, per host, per tier until nobody could keep up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layer {
    Local,
    Global,
}

impl Layer {
    pub fn as_str(self) -> &'static str {
        match self {
            Layer::Local => "local",
            Layer::Global => "global",
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct AuditConfig {
    #[serde(default)]
    pub private: bool,
    pub id: Option<String>,
}

/// Defaults under `[_defaults]`. Each field is `Option` so an absent key falls
/// through to the next layer while a present one - even an empty list - wins.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct DefaultsConfig {
    /// The provider list used when `--provider` is omitted.
    pub providers: Option<Vec<String>>,
    /// Seconds of rollout silence, with no final answer written, after which a
    /// codex run is treated as stalled: killed, bundled, and reported as a
    /// failure. `0` disables the check; omitted uses the built-in default
    /// (see `watchdog::Timings`).
    ///
    /// This is a genuine timeout resting on an *empirical* property of codex -
    /// that it wakes itself every few minutes and cannot stay silent - so it is
    /// deliberately tunable, and switchable off, in case a future codex changes
    /// cadence. codex-only; claude has no rollout to watch.
    pub stall_timeout_secs: Option<u64>,
}

/// A named settings profile: optional model, effort, sandbox, and env overrides
/// applied to a provider invocation when selected via `--profile`.
///
/// Unknown keys are an error. A profile wins whole, so a project profile with a
/// misspelled key (`modle = ...`) would otherwise parse as an empty profile and
/// silently discard the global definition it replaces - model, sandbox and all.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
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
    /// Ignored unless the profile's `sandbox` is `workspace-write`, because
    /// widening a `read-only` profile would contradict the only thing that
    /// profile promises. `~`, `$VAR` and `${VAR}` expand, which is what lets one
    /// hostless profile serve hosts with different layouts.
    #[serde(default)]
    pub writable_roots: Vec<String>,
}

/// provider -> profile name -> profile.
type ProfileTables = BTreeMap<String, BTreeMap<String, Profile>>;

/// One config file, parsed and checked on its own but not yet resolved against
/// the other layer.
#[derive(Debug, Default)]
pub struct ConfigFile {
    pub archetypes: BTreeMap<String, String>,
    pub groups: BTreeMap<String, Vec<String>>,
    pub audit: AuditConfig,
    pub defaults: DefaultsConfig,
    /// `[<provider>.<profile>]`.
    pub profiles: ProfileTables,
    /// Legacy `[<host>.<provider>.<profile>]`, keyed by host. Still parsed so
    /// existing files need no edit; for the host it names, it beats a hostless
    /// profile of the same name in the same file.
    pub hosts: BTreeMap<String, ProfileTables>,
}

/// A resolved value plus the layer and table it came from.
#[derive(Debug, Clone)]
pub struct Sourced<T> {
    pub value: T,
    pub layer: Layer,
    /// The table that supplied it, as written in the file (`[_defaults]`,
    /// `[codex.deep]`, `[plantasjen.codex.deep]`).
    pub table: String,
}

/// One definition of a profile. A legacy host-scoped one carries its host.
#[derive(Debug, Clone)]
pub struct ProfileDef {
    pub profile: Profile,
    pub layer: Layer,
    pub table: String,
    pub host: Option<String>,
}

/// Every definition of one `(provider, profile)` visible from this host, in
/// precedence order. The first wins **whole** - a winning profile replaces the
/// others outright rather than merging field by field, so no field of a run can
/// come from a table that did not mention it. The rest are kept so `review
/// config` can show what a stale override is hiding.
#[derive(Debug, Clone)]
pub struct ProfileEntry {
    pub effective: ProfileDef,
    pub shadowed: Vec<ProfileDef>,
}

/// The files that were read to build a `ReviewConfig`.
#[derive(Debug, Clone, Default)]
pub struct ConfigFiles {
    /// The project's `.review.toml`. `None` only from `load_optional` outside a
    /// project, which `review config` uses to show the global layer anyway.
    pub local: Option<PathBuf>,
    /// Where the global config is looked for. `None` when neither
    /// `XDG_CONFIG_HOME` nor `HOME` names a usable directory.
    pub global: Option<PathBuf>,
    /// Whether that global file existed and was read.
    pub global_loaded: bool,
}

/// The effective configuration: local and global layers resolved, for this
/// host.
#[derive(Debug)]
pub struct ReviewConfig {
    pub archetypes: BTreeMap<String, Sourced<String>>,
    pub groups: BTreeMap<String, Sourced<Vec<String>>>,
    /// Always the project's: an audit id identifies a project.
    pub audit: AuditConfig,
    pub providers: Option<Sourced<Vec<String>>>,
    pub stall_timeout_secs: Option<Sourced<u64>>,
    /// provider -> profile name -> entry.
    pub profiles: BTreeMap<String, BTreeMap<String, ProfileEntry>>,
    pub hostname: String,
    pub files: ConfigFiles,
}

impl ReviewConfig {
    /// The effective profile for `provider`, if any layer defines it.
    pub fn resolve_profile(&self, provider: &str, profile: &str) -> Option<&Profile> {
        self.profiles
            .get(provider)?
            .get(profile)
            .map(|e| &e.effective.profile)
    }

    pub fn archetype(&self, name: &str) -> Option<&str> {
        self.archetypes.get(name).map(|s| s.value.as_str())
    }

    pub fn default_providers(&self) -> Option<&[String]> {
        self.providers.as_ref().map(|s| s.value.as_slice())
    }

    pub fn stall_timeout_secs(&self) -> Option<u64> {
        self.stall_timeout_secs.as_ref().map(|s| s.value)
    }

    /// Every file that was consulted, for error messages about something no
    /// layer defines.
    pub fn searched(&self) -> String {
        let mut parts = vec![match self.files.local {
            Some(ref l) => l.display().to_string(),
            None => format!("(no {CONFIG_FILENAME})"),
        }];
        if let Some(ref g) = self.files.global {
            let absent = if self.files.global_loaded {
                ""
            } else {
                " (absent)"
            };
            parts.push(format!("{}{absent}", g.display()));
        }
        parts.join(", ")
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

/// Where the operator's global config lives: `$XDG_CONFIG_HOME/review/config.toml`,
/// else `$HOME/.config/review/config.toml`.
pub fn global_config_path() -> Option<PathBuf> {
    global_config_path_from(
        std::env::var_os("XDG_CONFIG_HOME"),
        std::env::var_os("HOME"),
    )
}

/// A relative `XDG_CONFIG_HOME` or `HOME` is ignored, as the XDG spec requires
/// for the former - either would otherwise resolve against whatever directory
/// `review` was launched from, making the global config a per-directory one.
fn global_config_path_from(xdg: Option<OsString>, home: Option<OsString>) -> Option<PathBuf> {
    let base = match xdg.map(PathBuf::from).filter(|p| p.is_absolute()) {
        Some(p) => p,
        None => home
            .map(PathBuf::from)
            .filter(|h| h.is_absolute())?
            .join(".config"),
    };
    Some(base.join("review").join("config.toml"))
}

/// Load the project's `.review.toml` (required: it carries the audit id) and
/// the global config (optional), and resolve them for this host.
pub fn load() -> Result<(ReviewConfig, PathBuf)> {
    let local_path = match locate_config()? {
        Located::Found(path) => path,
        Located::Missing(why) => bail!("{why}"),
    };
    let project_root = project_root_of(&local_path)?;
    Ok((load_layers(Some(local_path))?, project_root))
}

/// Like `load`, but a missing `.review.toml` is not an error: the config is the
/// global layer alone and the project root is `None`. A file that exists but
/// does not parse still is - callers use this to decide things like whether a
/// run's logs are private, and silently treating a broken config as an absent
/// one would downgrade a private project to the public log.
pub fn load_optional() -> Result<(ReviewConfig, Option<PathBuf>)> {
    match locate_config()? {
        Located::Found(path) => {
            let root = project_root_of(&path)?;
            Ok((load_layers(Some(path))?, Some(root)))
        }
        Located::Missing(_) => Ok((load_layers(None)?, None)),
    }
}

/// The project root - the directory holding `.review.toml` - without parsing
/// anything, for callers that only need to know which project they are in.
pub fn project_root() -> Result<Option<PathBuf>> {
    match locate_config()? {
        Located::Found(path) => Ok(Some(project_root_of(&path)?)),
        Located::Missing(_) => Ok(None),
    }
}

fn project_root_of(local_path: &std::path::Path) -> Result<PathBuf> {
    local_path
        .parent()
        .map(std::path::Path::to_path_buf)
        .ok_or_else(|| anyhow::anyhow!("{} has no parent directory", local_path.display()))
}

fn load_layers(local_path: Option<PathBuf>) -> Result<ReviewConfig> {
    let local = match local_path {
        Some(ref p) => {
            let raw = std::fs::read_to_string(p)
                .map_err(|e| anyhow::anyhow!("failed to read {}: {e}", p.display()))?;
            parse_file(&raw, &p.display().to_string(), Layer::Local)?
        }
        None => ConfigFile::default(),
    };

    let global_path = global_config_path();
    let global = match global_path {
        Some(ref p) => match std::fs::read_to_string(p) {
            Ok(raw) => Some(parse_file(&raw, &p.display().to_string(), Layer::Global)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => bail!("failed to read {}: {e}", p.display()),
        },
        None => None,
    };
    let global_loaded = global.is_some();

    let mut cfg = resolve(local, global, &hostname())?;
    cfg.files = ConfigFiles {
        local: local_path,
        global: global_path,
        global_loaded,
    };
    Ok(cfg)
}

enum Located {
    Found(PathBuf),
    /// Not found; carries the message saying where the search stopped.
    Missing(String),
}

fn locate_config() -> Result<Located> {
    let mut dir = std::env::current_dir()
        .map_err(|e| anyhow::anyhow!("failed to get current directory: {e}"))?;

    loop {
        let candidate = dir.join(CONFIG_FILENAME);
        if candidate.exists() {
            return Ok(Located::Found(candidate));
        }
        if dir.join(".git").exists() {
            return Ok(Located::Missing(format!(
                "no {CONFIG_FILENAME} found (searched up to git root: {})\n\n\
                 Run `review init` to create one.",
                dir.display()
            )));
        }
        if !dir.pop() {
            return Ok(Located::Missing(format!(
                "no {CONFIG_FILENAME} found in current or parent directories\n\n\
                 Run `review init` to create one."
            )));
        }
    }
}

fn unknown_provider_hint() -> String {
    format!("supported: {}", KNOWN_PROVIDERS.join(", "))
}

/// Parse one config file. `origin` names it in errors. Checks everything that
/// can be checked within the file alone; cross-file checks (group members,
/// group/archetype name clashes) happen in `resolve`, because a project's group
/// may name a global archetype.
pub fn parse_file(raw: &str, origin: &str, layer: Layer) -> Result<ConfigFile> {
    // Parse to a table first, then peel off the reserved sections by name.
    // Everything left over is a provider table or a legacy host table. This
    // avoids serde `flatten`, which does not coexist with a sibling named field
    // (`archetypes`).
    let mut table: toml::Table =
        toml::from_str(raw).map_err(|e| anyhow::anyhow!("failed to parse {origin}: {e}"))?;

    let groups: BTreeMap<String, Vec<String>> = match table.remove("_groups") {
        Some(v) => v
            .try_into()
            .map_err(|e| anyhow::anyhow!("[_groups] in {origin}: {e}"))?,
        None => BTreeMap::new(),
    };
    let audit: AuditConfig = match table.remove("_audit") {
        Some(_) if layer == Layer::Global => bail!(
            "[_audit] in {origin}: the audit block identifies a project, so it belongs \
             in that project's {CONFIG_FILENAME}, not in the global config"
        ),
        Some(v) => v
            .try_into()
            .map_err(|e| anyhow::anyhow!("[_audit] in {origin}: {e}"))?,
        None => AuditConfig::default(),
    };
    let defaults: DefaultsConfig = match table.remove("_defaults") {
        Some(v) => v
            .try_into()
            .map_err(|e| anyhow::anyhow!("[_defaults] in {origin}: {e}"))?,
        None => DefaultsConfig::default(),
    };
    let archetypes: BTreeMap<String, String> = match table.remove("archetypes") {
        Some(v) => v
            .try_into()
            .map_err(|e| anyhow::anyhow!("[archetypes] in {origin}: {e}"))?,
        None => BTreeMap::new(),
    };

    // Remaining top-level tables: a known provider name is a hostless
    // `[<provider>.<profile>]` table; anything else is a legacy host.
    let mut profiles: ProfileTables = BTreeMap::new();
    let mut hosts: BTreeMap<String, ProfileTables> = BTreeMap::new();
    for (key, val) in table {
        let toml::Value::Table(inner) = val else {
            bail!("unexpected top-level key '{key}' in {origin}: expected a table");
        };
        if KNOWN_PROVIDERS.contains(&key.as_str()) {
            let parsed = parse_profiles(inner, &key, origin)?;
            profiles.insert(key, parsed);
            continue;
        }
        let mut host_profiles: ProfileTables = BTreeMap::new();
        for (prov, pv) in inner {
            if !KNOWN_PROVIDERS.contains(&prov.as_str()) {
                bail!(
                    "unknown provider '{prov}' in [{key}.{prov}.*] in {origin}\n  \
                     {}\n  \
                     A top-level table is either a provider ([<provider>.<profile>]) \
                     or a legacy host ([<host>.<provider>.<profile>]).",
                    unknown_provider_hint()
                );
            }
            let toml::Value::Table(pt) = pv else {
                bail!("[{key}.{prov}] in {origin}: expected a table of profiles");
            };
            let prefix = format!("{}.{prov}", toml_key(&key));
            let parsed = parse_profiles(pt, &prefix, origin)?;
            host_profiles.insert(prov, parsed);
        }
        hosts.insert(key, host_profiles);
    }

    for (name, prime) in &archetypes {
        if RESERVED_NAMES.contains(&name.as_str()) {
            bail!("'{name}' is a reserved name and cannot be used as an archetype in {origin}");
        }
        if name == BARE && !prime.trim().is_empty() {
            bail!(
                "archetype 'bare' in {origin} has a prompt, but `bare` means no archetype - \
                 `review bare` ignores it\n  Give it an empty prompt (bare = \"\") or rename it."
            );
        }
    }
    for (name, members) in &groups {
        if RESERVED_NAMES.contains(&name.as_str()) || name == BARE {
            bail!("'{name}' is a reserved name and cannot be used as a group in {origin}");
        }
        if members.is_empty() {
            bail!("group '{name}' is empty in {origin}");
        }
        let mut seen = std::collections::HashSet::new();
        for member in members {
            if !seen.insert(member) {
                bail!("group '{name}' contains duplicate archetype '{member}' in {origin}");
            }
            // A global group may only name global archetypes. The global file
            // is read in every project, so a group leaning on one project's
            // archetype would be an error everywhere else - and a global group
            // shadowed by a project group is checked here or nowhere.
            if layer == Layer::Global && !archetypes.contains_key(member) {
                bail!(
                    "group '{name}' references archetype '{member}', which {origin} does not \
                     define\n  A global group may only name global archetypes."
                );
            }
        }
    }
    if let Some(ref providers) = defaults.providers {
        for prov in providers {
            if !KNOWN_PROVIDERS.contains(&prov.as_str()) {
                bail!(
                    "unknown provider '{prov}' in [_defaults].providers in {origin}\n  {}",
                    unknown_provider_hint()
                );
            }
        }
    }

    Ok(ConfigFile {
        archetypes,
        groups,
        audit,
        defaults,
        profiles,
        hosts,
    })
}

fn parse_profiles(t: toml::Table, prefix: &str, origin: &str) -> Result<BTreeMap<String, Profile>> {
    let mut out = BTreeMap::new();
    for (name, v) in t {
        let profile: Profile = v
            .try_into()
            .map_err(|e| anyhow::anyhow!("[{prefix}.{name}] in {origin}: {e}"))?;
        out.insert(name, profile);
    }
    Ok(out)
}

/// Resolve the local and (optional) global layers for `hostname`.
///
/// Everything is first-definition-wins over the precedence order local, then
/// global; within one file a legacy host table for this host beats a hostless
/// table. Profiles and lists win whole - nothing is merged field by field or
/// element by element.
pub fn resolve(
    local: ConfigFile,
    global: Option<ConfigFile>,
    hostname: &str,
) -> Result<ReviewConfig> {
    let audit = local.audit.clone();
    let mut layers: Vec<(Layer, ConfigFile)> = vec![(Layer::Local, local)];
    if let Some(g) = global {
        layers.push((Layer::Global, g));
    }

    let mut archetypes: BTreeMap<String, Sourced<String>> = BTreeMap::new();
    let mut groups: BTreeMap<String, Sourced<Vec<String>>> = BTreeMap::new();
    let mut providers: Option<Sourced<Vec<String>>> = None;
    let mut stall_timeout_secs: Option<Sourced<u64>> = None;
    let mut defs: BTreeMap<String, BTreeMap<String, Vec<ProfileDef>>> = BTreeMap::new();

    for (layer, file) in layers {
        for (name, prime) in file.archetypes {
            archetypes.entry(name).or_insert_with(|| Sourced {
                value: prime,
                layer,
                table: "[archetypes]".into(),
            });
        }
        for (name, members) in file.groups {
            groups.entry(name).or_insert_with(|| Sourced {
                value: members,
                layer,
                table: "[_groups]".into(),
            });
        }
        if providers.is_none()
            && let Some(p) = file.defaults.providers
        {
            providers = Some(Sourced {
                value: p,
                layer,
                table: "[_defaults]".into(),
            });
        }
        if stall_timeout_secs.is_none()
            && let Some(s) = file.defaults.stall_timeout_secs
        {
            stall_timeout_secs = Some(Sourced {
                value: s,
                layer,
                table: "[_defaults]".into(),
            });
        }

        // Host tables first: pushing in precedence order makes the first
        // definition of each profile the effective one.
        let mut hosts = file.hosts;
        if let Some(host_tables) = hosts.remove(hostname) {
            for (prov, profiles) in host_tables {
                for (name, profile) in profiles {
                    let table = format!("[{}.{prov}.{name}]", toml_key(hostname));
                    defs.entry(prov.clone())
                        .or_default()
                        .entry(name)
                        .or_default()
                        .push(ProfileDef {
                            profile,
                            layer,
                            table,
                            host: Some(hostname.to_string()),
                        });
                }
            }
        }
        for (prov, profiles) in file.profiles {
            for (name, profile) in profiles {
                let table = format!("[{prov}.{name}]");
                defs.entry(prov.clone())
                    .or_default()
                    .entry(name)
                    .or_default()
                    .push(ProfileDef {
                        profile,
                        layer,
                        table,
                        host: None,
                    });
            }
        }
    }

    // A group and an archetype share the one positional name the CLI resolves.
    // Across files the project wins, as it does for every other name: adding a
    // group to the global file must not break a project that happens to have an
    // archetype of that name. Within one file there is no winner to pick.
    let clashes: Vec<String> = groups
        .keys()
        .filter(|n| archetypes.contains_key(*n))
        .cloned()
        .collect();
    let mut hidden = std::collections::BTreeSet::new();
    for name in clashes {
        let (g, a) = (groups[&name].layer, archetypes[&name].layer);
        if g == a {
            bail!(
                "group '{name}' conflicts with an archetype of the same name ({} config)",
                g.as_str()
            );
        }
        if g == Layer::Global {
            groups.remove(&name);
        } else {
            archetypes.remove(&name);
        }
        hidden.insert(name);
    }

    for (name, group) in &groups {
        for member in &group.value {
            if !archetypes.contains_key(member) {
                let why = if hidden.contains(member) {
                    " (a project group of that name hides the global archetype)"
                } else {
                    ""
                };
                bail!(
                    "group '{name}' ({} config) references unknown archetype '{member}'{why}",
                    group.layer.as_str()
                );
            }
        }
    }

    let profiles = defs
        .into_iter()
        .map(|(prov, by_name)| {
            let entries = by_name
                .into_iter()
                .filter_map(|(name, list)| {
                    let mut defs = list.into_iter();
                    let effective = defs.next()?;
                    Some((
                        name,
                        ProfileEntry {
                            effective,
                            shadowed: defs.collect(),
                        },
                    ))
                })
                .collect();
            (prov, entries)
        })
        .collect();

    Ok(ReviewConfig {
        archetypes,
        groups,
        audit,
        providers,
        stall_timeout_secs,
        profiles,
        hostname: hostname.to_string(),
        files: ConfigFiles::default(),
    })
}

const INIT_TEMPLATE_PREFIX: &str = "\
# Project config for `review`. Everything here is optional except [_audit].
# Settings resolve command line, then this file, then the operator's global
# config (~/.config/review/config.toml, same format). Run `review config` to see
# the effective result and where each value came from.
#
# Archetypes are priming prompts: a name mapped to text prepended to stdin.
# A run without an archetype sends stdin unchanged.
#
# [archetypes]
# security = \"You are a security expert for this project. Read the codebase.\"
#
# Providers to fan out to when --provider is omitted:
# [_defaults]
# providers = [\"codex\"]
#
# Groups fan out to multiple archetypes:
# [_groups]
# sweep = [\"security\", \"bugs\"]
#
# Named profiles, selected with --profile, as [<provider>.<profile>]. A profile
# here replaces a global profile of the same name entirely.
# [codex.deep]
# model = \"gpt-6-luna\"
# effort = \"high\"
# sandbox = \"read-only\"
";

pub fn init() -> Result<()> {
    let path = std::env::current_dir()
        .map_err(|e| anyhow::anyhow!("failed to get current directory: {e}"))?
        .join(CONFIG_FILENAME);

    if path.exists() {
        bail!("{CONFIG_FILENAME} already exists in current directory");
    }

    if let Located::Found(existing) = locate_config()? {
        bail!(
            "{CONFIG_FILENAME} already exists at {}\n  \
             Creating another here would shadow it.",
            existing.display()
        );
    }

    let audit_id = generate_short_id();
    let mut content = INIT_TEMPLATE_PREFIX.to_string();
    content.push_str(&format!("\n[_audit]\nid = \"{audit_id}\"\n"));
    std::fs::write(&path, content)
        .map_err(|e| anyhow::anyhow!("failed to write {CONFIG_FILENAME}: {e}"))?;

    println!("Created {CONFIG_FILENAME}");
    println!();
    println!("Next steps:");
    println!("  1. Run `review config` to see what the global config already provides");
    println!("  2. Run: echo \"check for issues\" | review --provider codex");
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

    /// Parse a single local file and resolve it alone, as a project with no
    /// global config would be.
    fn local_only(raw: &str, host: &str) -> Result<ReviewConfig> {
        resolve(parse_file(raw, "local", Layer::Local)?, None, host)
    }

    #[test]
    fn parses_archetypes() {
        let raw = "\
[archetypes]
security = \"be a security expert\"
bugs = \"find edge cases\"
";
        let cfg = local_only(raw, "h").unwrap();
        assert_eq!(cfg.archetypes.len(), 2);
        assert_eq!(cfg.archetype("security"), Some("be a security expert"));
        assert_eq!(cfg.archetype("bugs"), Some("find edge cases"));
    }

    #[test]
    fn parses_legacy_host_profiles() {
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
        let cfg = local_only(raw, "myhost").unwrap();

        let opus = cfg.resolve_profile("claude", "opus").unwrap();
        assert_eq!(opus.model.as_deref(), Some("Opus 4.8"));
        assert_eq!(opus.effort.as_deref(), Some("medium"));
        assert_eq!(opus.sandbox, None);
        assert_eq!(
            opus.env.as_ref().unwrap()["ANTHROPIC_BASE_URL"],
            "http://localhost:8787"
        );

        let implement = cfg.resolve_profile("codex", "implement").unwrap();
        assert_eq!(implement.model.as_deref(), Some("gpt-5.6-terra"));
        assert_eq!(implement.effort.as_deref(), Some("high"));
        assert_eq!(implement.sandbox.as_deref(), Some("workspace-write"));

        assert!(cfg.resolve_profile("claude", "nope").is_none());

        // Another host's tables do not apply here.
        let other = local_only(raw, "otherhost").unwrap();
        assert!(other.resolve_profile("claude", "opus").is_none());
    }

    #[test]
    fn parses_hostless_profiles() {
        let raw = "\
[codex.deep]
model = \"gpt-6-luna\"
sandbox = \"read-only\"
";
        for host in ["a", "b"] {
            let cfg = local_only(raw, host).unwrap();
            let deep = cfg.resolve_profile("codex", "deep").unwrap();
            assert_eq!(deep.model.as_deref(), Some("gpt-6-luna"));
        }
    }

    #[test]
    fn empty_config_parses() {
        let cfg = local_only("", "h").unwrap();
        assert!(cfg.archetypes.is_empty());
        assert!(cfg.profiles.is_empty());
        assert!(cfg.providers.is_none());
    }

    #[test]
    fn unknown_provider_in_profile_errors() {
        let raw = "\
[archetypes]
bugs = \"x\"

[myhost.gpt.fast]
model = \"whatever\"
";
        let err = local_only(raw, "myhost").unwrap_err().to_string();
        assert!(err.contains("unknown provider 'gpt'"), "{err}");
    }

    #[test]
    fn parses_defaults_providers() {
        let raw = "\
[_defaults]
providers = [\"claude\", \"codex\"]
";
        let cfg = local_only(raw, "h").unwrap();
        assert_eq!(
            cfg.default_providers().unwrap(),
            &["claude".to_string(), "codex".to_string()]
        );
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
        let cfg = local_only(raw, "h").unwrap();
        assert_eq!(cfg.groups.len(), 1);
        assert_eq!(cfg.groups["sweep"].value, vec!["security", "bugs"]);
    }

    #[test]
    fn group_with_unknown_member_errors() {
        let raw = "\
[archetypes]
security = \"a\"

[_groups]
sweep = [\"security\", \"nonexistent\"]
";
        let err = local_only(raw, "h").unwrap_err().to_string();
        assert!(err.contains("nonexistent"), "{err}");
    }

    #[test]
    fn group_name_conflicts_with_archetype() {
        let raw = "\
[archetypes]
security = \"a\"

[_groups]
security = [\"security\"]
";
        let err = local_only(raw, "h").unwrap_err().to_string();
        assert!(err.contains("conflicts"), "{err}");
    }

    #[test]
    fn reserved_archetype_name_errors() {
        let err = local_only("[archetypes]\nall = \"a\"\n", "h")
            .unwrap_err()
            .to_string();
        assert!(err.contains("reserved"), "{err}");
    }

    #[test]
    fn subcommand_names_are_reserved_as_archetype_and_group_names() {
        // `-a` cannot collide with a subcommand, but the legacy positional form
        // can: `review sessions` runs the subcommand, silently, instead of the
        // archetype.
        for name in ["sessions", "help", "init", "incidents", "config", "resume"] {
            let as_archetype = format!("[archetypes]\n{name} = \"x\"\n");
            let err = local_only(&as_archetype, "h").unwrap_err().to_string();
            assert!(err.contains("reserved"), "{name} as archetype: {err}");
            let as_group = format!("[archetypes]\nx = \"x\"\n[_groups]\n{name} = [\"x\"]\n");
            let err = local_only(&as_group, "h").unwrap_err().to_string();
            assert!(err.contains("reserved"), "{name} as group: {err}");
        }
    }

    #[test]
    fn bare_may_only_be_an_empty_archetype() {
        // `bare = ""` is in most existing configs and means what `review bare`
        // means. A prompt under that name would be silently ignored by it.
        assert!(local_only("[archetypes]\nbare = \"\"\n", "h").is_ok());
        let err = local_only("[archetypes]\nbare = \"be terse\"\n", "h")
            .unwrap_err()
            .to_string();
        assert!(err.contains("bare"), "{err}");
        let err = local_only("[archetypes]\nx = \"x\"\n[_groups]\nbare = [\"x\"]\n", "h")
            .unwrap_err()
            .to_string();
        assert!(err.contains("reserved"), "{err}");
    }

    #[test]
    fn global_config_path_prefers_absolute_xdg() {
        let p = global_config_path_from(Some("/x/cfg".into()), Some("/home/u".into()));
        assert_eq!(p, Some(PathBuf::from("/x/cfg/review/config.toml")));
        // Relative XDG_CONFIG_HOME is ignored per the XDG spec.
        let p = global_config_path_from(Some("rel".into()), Some("/home/u".into()));
        assert_eq!(p, Some(PathBuf::from("/home/u/.config/review/config.toml")));
        assert_eq!(global_config_path_from(None, None), None);
        assert_eq!(global_config_path_from(None, Some("".into())), None);
        // A relative HOME would make the global config depend on the launch
        // directory, the same reason a relative XDG_CONFIG_HOME is ignored.
        assert_eq!(global_config_path_from(None, Some("rel".into())), None);
    }
}
