//! `review config`: the effective configuration and where each value came from.
//!
//! Orchestrators used to parse `.review.toml` directly to learn which
//! archetypes, providers and profiles exist. With the global layer that file no
//! longer tells the whole story - a profile may come from
//! `~/.config/review/config.toml`, and a stale local one may be hiding it - so
//! this command is the single source of truth. It renders what `config::load`
//! resolved, i.e. exactly what a run would use; it never re-parses TOML, because
//! a second reader is a second opinion that can disagree with the one that
//! actually launches providers.
//!
//! Rendering is pure (`render_text`, `render_json` take the resolved config and
//! an availability map), so it is testable without `PATH` or files; `run` only
//! loads, probes and prints.

use crate::config::{
    self, KNOWN_PROVIDERS, Profile, ProfileDef, ProfileEntry, ReviewConfig, Sourced,
};
use anyhow::Result;
use serde::Serialize;
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;

/// provider name -> whether its binary is on `PATH` on this host.
pub type Availability = BTreeMap<String, bool>;

/// How much of an archetype's first prompt line the human view shows.
const PRIME_PREVIEW_CHARS: usize = 72;

/// Works outside a project too: with no `.review.toml` it shows the global
/// layer and provider availability, which is what an orchestrator needs before
/// it has picked a project.
pub fn run(json: bool) -> Result<()> {
    let (cfg, project_root) = config::load_optional()?;
    let avail = probe_availability();
    if json {
        let value = render_json(&cfg, project_root.as_deref(), &avail);
        println!("{}", serde_json::to_string_pretty(&value)?);
    } else {
        print!("{}", render_text(&cfg, project_root.as_deref(), &avail));
    }
    Ok(())
}

fn probe_availability() -> Availability {
    KNOWN_PROVIDERS
        .iter()
        .map(|p| ((*p).to_string(), crate::provider::is_available(p)))
        .collect()
}

fn installed(avail: &Availability, provider: &str) -> bool {
    avail.get(provider).copied().unwrap_or(false)
}

/// The machine-readable view. Key names are a contract with orchestrators.
/// Values are raw: an unset sandbox is `null`, not the runner's default, so a
/// consumer can tell "the profile says read-only" from "nothing was said".
/// Env *values* never appear - they can carry secrets - only their names.
pub fn render_json(cfg: &ReviewConfig, project_root: Option<&Path>, avail: &Availability) -> Value {
    let available: Map<String, Value> = KNOWN_PROVIDERS
        .iter()
        .map(|p| ((*p).to_string(), Value::Bool(installed(avail, p))))
        .collect();

    let archetypes: Map<String, Value> = cfg
        .archetypes
        .iter()
        .map(|(name, s)| {
            (
                name.clone(),
                json!({"layer": s.layer.as_str(), "prompt": s.value}),
            )
        })
        .collect();

    let groups: Map<String, Value> = cfg
        .groups
        .iter()
        .map(|(name, s)| {
            (
                name.clone(),
                json!({"layer": s.layer.as_str(), "members": s.value}),
            )
        })
        .collect();

    let profiles: Map<String, Value> = cfg
        .profiles
        .iter()
        .map(|(provider, by_name)| {
            let entries: Map<String, Value> = by_name
                .iter()
                .map(|(name, entry)| (name.clone(), profile_json(entry)))
                .collect();
            (provider.clone(), Value::Object(entries))
        })
        .collect();

    json!({
        "project_root": project_root.map(|p| p.display().to_string()),
        "hostname": cfg.hostname,
        "files": {
            "local": cfg.files.local.as_ref().map(|l| l.display().to_string()),
            "global": cfg.files.global.as_ref().map(|g| g.display().to_string()),
            "global_loaded": cfg.files.global_loaded,
        },
        "providers": {
            "default": cfg.providers.as_ref().map(sourced_json),
            "available": available,
        },
        "archetypes": archetypes,
        "groups": groups,
        "profiles": profiles,
        "stall_timeout_secs": cfg.stall_timeout_secs.as_ref().map(sourced_json),
    })
}

fn sourced_json<T: Serialize>(s: &Sourced<T>) -> Value {
    json!({"value": s.value, "layer": s.layer.as_str(), "table": s.table})
}

fn env_keys(p: &Profile) -> Vec<&str> {
    p.env
        .as_ref()
        .map(|e| e.keys().map(String::as_str).collect())
        .unwrap_or_default()
}

fn profile_json(entry: &ProfileEntry) -> Value {
    let def = &entry.effective;
    let p = &def.profile;
    let shadows: Vec<Value> = entry
        .shadowed
        .iter()
        .map(|s| {
            json!({
                "layer": s.layer.as_str(),
                "table": s.table,
                "host": s.host,
                "model": s.profile.model,
                "effort": s.profile.effort,
                "sandbox": s.profile.sandbox,
            })
        })
        .collect();
    json!({
        "layer": def.layer.as_str(),
        "table": def.table,
        "host": def.host,
        "model": p.model,
        "effort": p.effort,
        "sandbox": p.sandbox,
        "writable_roots": p.writable_roots,
        "env_keys": env_keys(p),
        "config": p.config,
        "shadows": shadows,
    })
}

/// The terminal view: compact, no colours, one block per section.
pub fn render_text(
    cfg: &ReviewConfig,
    project_root: Option<&Path>,
    avail: &Availability,
) -> String {
    let mut out = String::new();
    // Writing into a `String` cannot fail; the `fmt::Result` plumbing exists
    // only so the body can use `?` instead of discarding each `writeln!`.
    let _ = write_text(&mut out, cfg, project_root, avail);
    out
}

fn write_text(
    out: &mut String,
    cfg: &ReviewConfig,
    project_root: Option<&Path>,
    avail: &Availability,
) -> std::fmt::Result {
    writeln!(out, "files:")?;
    match cfg.files.local {
        Some(ref l) => writeln!(out, "  local:   {}", l.display())?,
        None => writeln!(out, "  local:   none (not in a project; global layer only)")?,
    }
    match cfg.files.global {
        Some(ref g) if cfg.files.global_loaded => writeln!(out, "  global:  {}", g.display())?,
        Some(ref g) => writeln!(out, "  global:  {} (absent)", g.display())?,
        None => writeln!(
            out,
            "  global:  none (neither XDG_CONFIG_HOME nor HOME is usable)"
        )?,
    }
    writeln!(out, "host:      {}", cfg.hostname)?;
    match project_root {
        Some(root) => writeln!(out, "project:   {}", root.display())?,
        None => writeln!(out, "project:   none")?,
    }

    writeln!(out)?;
    writeln!(out, "providers:")?;
    let defaults = cfg.default_providers().unwrap_or_default();
    match cfg.providers {
        Some(ref s) => writeln!(
            out,
            "  default: {}  ({})",
            if s.value.is_empty() {
                "(empty list)".to_string()
            } else {
                s.value.join(", ")
            },
            source(s.layer.as_str(), &s.table, None)
        )?,
        None => writeln!(out, "  default: none set (a run needs --provider)")?,
    }
    let pw = KNOWN_PROVIDERS.iter().map(|p| p.len()).max().unwrap_or(0);
    let mut missing_defaults = Vec::new();
    for provider in KNOWN_PROVIDERS {
        let is_default = defaults.iter().any(|d| d == provider);
        let ok = installed(avail, provider);
        if is_default && !ok {
            missing_defaults.push(*provider);
        }
        let status = if ok { "installed" } else { "NOT INSTALLED" };
        let mark = if is_default { "  default" } else { "" };
        let line = format!("  {provider:<pw$}  {status:<13}{mark}");
        writeln!(out, "{}", line.trim_end())?;
    }
    for provider in missing_defaults {
        writeln!(
            out,
            "  warning: default provider '{provider}' is not installed on this host - \
             a run without --provider will fail"
        )?;
    }

    writeln!(out)?;
    writeln!(out, "archetypes:")?;
    if cfg.archetypes.is_empty() {
        writeln!(out, "  (none)")?;
    }
    let aw = cfg.archetypes.keys().map(String::len).max().unwrap_or(0);
    for (name, s) in &cfg.archetypes {
        writeln!(
            out,
            "  {name:<aw$}  {:<6}  {}",
            s.layer.as_str(),
            prime_preview(&s.value)
        )?;
    }

    writeln!(out)?;
    writeln!(out, "groups:")?;
    if cfg.groups.is_empty() {
        writeln!(out, "  (none)")?;
    }
    let gw = cfg.groups.keys().map(String::len).max().unwrap_or(0);
    for (name, s) in &cfg.groups {
        writeln!(
            out,
            "  {name:<gw$}  {:<6}  {}",
            s.layer.as_str(),
            s.value.join(", ")
        )?;
    }

    writeln!(out)?;
    writeln!(out, "profiles:")?;
    if cfg.profiles.values().all(BTreeMap::is_empty) {
        writeln!(out, "  (none)")?;
    }
    for (provider, by_name) in &cfg.profiles {
        if by_name.is_empty() {
            continue;
        }
        writeln!(out, "  {provider}:")?;
        for (name, entry) in by_name {
            write_profile(out, provider, name, entry)?;
        }
    }

    writeln!(out)?;
    match cfg.stall_timeout_secs {
        Some(ref s) => writeln!(
            out,
            "stall_timeout_secs: {}  ({})",
            s.value,
            source(s.layer.as_str(), &s.table, None)
        )?,
        None => writeln!(out, "stall_timeout_secs: unset (built-in default)")?,
    }
    Ok(())
}

fn write_profile(
    out: &mut String,
    provider: &str,
    name: &str,
    entry: &ProfileEntry,
) -> std::fmt::Result {
    let def = &entry.effective;
    let p = &def.profile;
    writeln!(out, "    {name}  ({})", def_source(def))?;
    writeln!(out, "      {}", settings(provider, p))?;
    if !p.writable_roots.is_empty() {
        writeln!(out, "      writable_roots: {}", p.writable_roots.join(", "))?;
    }
    let keys = env_keys(p);
    if !keys.is_empty() {
        writeln!(out, "      env: {}", keys.join(", "))?;
    }
    for c in &p.config {
        writeln!(out, "      config: {c}")?;
    }
    for s in &entry.shadowed {
        writeln!(
            out,
            "      shadows {}: {}",
            def_source(s),
            settings(provider, &s.profile)
        )?;
    }
    Ok(())
}

fn def_source(def: &ProfileDef) -> String {
    source(def.layer.as_str(), &def.table, def.host.as_deref())
}

/// `local [codex.deep]`, flagging a legacy host table so an operator migrating
/// to hostless profiles can see which ones are left.
fn source(layer: &str, table: &str, host: Option<&str>) -> String {
    if host.is_some() {
        format!("{layer} {table}, legacy host table")
    } else {
        format!("{layer} {table}")
    }
}

fn settings(provider: &str, p: &Profile) -> String {
    format!(
        "model {}, effort {}, sandbox {}",
        p.model.as_deref().unwrap_or("unset"),
        p.effort.as_deref().unwrap_or("unset"),
        sandbox_text(provider, p.sandbox.as_deref())
    )
}

/// The human view says what an unset sandbox *means* for the provider, because
/// "unset" alone invites the wrong guess: codex and grok runners default to
/// read-only, while claude has no filesystem sandbox on this axis at all.
fn sandbox_text(provider: &str, sandbox: Option<&str>) -> String {
    match (provider, sandbox) {
        ("claude", Some(s)) => format!("{s} (ignored by claude)"),
        ("claude", None) => "n/a (claude ignores sandbox)".to_string(),
        (_, Some(s)) => s.to_string(),
        (_, None) => "unset (runner default: read-only)".to_string(),
    }
}

/// First non-blank line of a prime, truncated; `...` marks anything cut,
/// including further lines.
fn prime_preview(prime: &str) -> String {
    let mut lines = prime.lines().map(str::trim).filter(|l| !l.is_empty());
    let Some(first) = lines.next() else {
        return "(empty)".to_string();
    };
    let mut shown: String = first.chars().take(PRIME_PREVIEW_CHARS).collect();
    if lines.next().is_some() || first.chars().count() > PRIME_PREVIEW_CHARS {
        shown.push_str("...");
    }
    shown
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::config::{Layer, parse_file, resolve};

    const LOCAL: &str = "\
[archetypes]
security = \"You are a security expert.\\nRead the codebase.\"
goal = \"\"

[_defaults]
providers = [\"codex\"]

[_groups]
sweep = [\"security\", \"bugs\"]

[host.codex.deep]
model = \"stale-model\"
effort = \"low\"
env = { SECRET_TOKEN = \"hunter2\" }
";

    const GLOBAL: &str = "\
[archetypes]
bugs = \"You hunt for edge cases.\"
security = \"global security prime\"

[_defaults]
providers = [\"claude\"]
stall_timeout_secs = 600

[codex.deep]
model = \"fresh-model\"
effort = \"high\"
sandbox = \"workspace-write\"
writable_roots = [\"/srv/data\"]
config = ['model_provider=\"x\"']

[claude.opus]
model = \"opus\"
";

    fn cfg() -> ReviewConfig {
        let local = parse_file(LOCAL, "local", Layer::Local).unwrap();
        let global = parse_file(GLOBAL, "global", Layer::Global).unwrap();
        resolve(local, Some(global), "host").unwrap()
    }

    fn avail(codex: bool) -> Availability {
        [("claude", true), ("codex", codex), ("grok", false)]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect()
    }

    #[test]
    fn json_reports_layers_and_first_definition_wins() {
        let v = render_json(&cfg(), Some(Path::new("/proj")), &avail(true));
        assert_eq!(v["project_root"], "/proj");
        assert_eq!(v["hostname"], "host");
        assert_eq!(v["archetypes"]["security"]["layer"], "local");
        assert_eq!(v["archetypes"]["bugs"]["layer"], "global");
        assert_eq!(v["groups"]["sweep"]["layer"], "local");
        assert_eq!(v["groups"]["sweep"]["members"], json!(["security", "bugs"]));
        assert_eq!(
            v["providers"]["default"],
            json!({"value": ["codex"], "layer": "local", "table": "[_defaults]"})
        );
        assert_eq!(
            v["stall_timeout_secs"],
            json!({"value": 600, "layer": "global", "table": "[_defaults]"})
        );
    }

    #[test]
    fn json_shows_what_a_stale_local_profile_shadows() {
        let v = render_json(&cfg(), Some(Path::new("/proj")), &avail(true));
        let deep = &v["profiles"]["codex"]["deep"];
        assert_eq!(deep["layer"], "local");
        assert_eq!(deep["table"], "[host.codex.deep]");
        assert_eq!(deep["host"], "host");
        assert_eq!(deep["model"], "stale-model");
        // No invented default: the winning profile set no sandbox.
        assert_eq!(deep["sandbox"], Value::Null);
        assert_eq!(deep["writable_roots"], json!([]));

        let shadows = deep["shadows"].as_array().unwrap();
        assert_eq!(shadows.len(), 1);
        assert_eq!(shadows[0]["layer"], "global");
        assert_eq!(shadows[0]["table"], "[codex.deep]");
        assert_eq!(shadows[0]["host"], Value::Null);
        assert_eq!(shadows[0]["model"], "fresh-model");
        assert_eq!(shadows[0]["sandbox"], "workspace-write");

        let opus = &v["profiles"]["claude"]["opus"];
        assert_eq!(opus["layer"], "global");
        assert_eq!(opus["shadows"], json!([]));
    }

    #[test]
    fn env_names_appear_but_values_never_do() {
        let c = cfg();
        let v = render_json(&c, Some(Path::new("/proj")), &avail(true));
        assert_eq!(
            v["profiles"]["codex"]["deep"]["env_keys"],
            json!(["SECRET_TOKEN"])
        );
        let json_text = v.to_string();
        assert!(!json_text.contains("hunter2"), "{json_text}");

        let text = render_text(&c, Some(Path::new("/proj")), &avail(true));
        assert!(text.contains("SECRET_TOKEN"), "{text}");
        assert!(!text.contains("hunter2"), "{text}");
    }

    #[test]
    fn availability_is_reported_and_a_missing_default_stands_out() {
        let c = cfg();
        let v = render_json(&c, Some(Path::new("/proj")), &avail(false));
        assert_eq!(
            v["providers"]["available"],
            json!({"claude": true, "codex": false, "grok": false})
        );

        let text = render_text(&c, Some(Path::new("/proj")), &avail(false));
        assert!(text.contains("NOT INSTALLED  default"), "{text}");
        assert!(
            text.contains("default provider 'codex' is not installed"),
            "{text}"
        );

        // Installed default: no warning. grok is not installed but is not a
        // default, so it is listed without alarm.
        let text = render_text(&c, Some(Path::new("/proj")), &avail(true));
        assert!(!text.contains("warning"), "{text}");
    }

    #[test]
    fn text_view_previews_primes_and_explains_unset_sandbox() {
        let text = render_text(&cfg(), Some(Path::new("/proj")), &avail(true));
        // Multi-line prime: first line, marked as cut.
        assert!(text.contains("You are a security expert...."), "{text}");
        assert!(!text.contains("Read the codebase"), "{text}");
        assert!(text.contains("(empty)"), "{text}");
        assert!(
            text.contains("[host.codex.deep], legacy host table"),
            "{text}"
        );
        assert!(
            text.contains("sandbox unset (runner default: read-only)"),
            "{text}"
        );
        assert!(
            text.contains("shadows global [codex.deep]: model fresh-model"),
            "{text}"
        );
        assert!(text.contains("n/a (claude ignores sandbox)"), "{text}");
        assert!(
            text.contains("stall_timeout_secs: 600  (global [_defaults])"),
            "{text}"
        );
    }

    #[test]
    fn outside_a_project_the_global_layer_is_still_shown() {
        // An orchestrator asks before it has picked a project; the global
        // profiles and provider availability are the answer it needs.
        let global = parse_file(GLOBAL, "global", Layer::Global).unwrap();
        let c = resolve(config::ConfigFile::default(), Some(global), "host").unwrap();
        let v = render_json(&c, None, &avail(true));
        assert_eq!(v["project_root"], Value::Null);
        assert_eq!(v["files"]["local"], Value::Null);
        assert_eq!(v["profiles"]["codex"]["deep"]["model"], "fresh-model");

        let text = render_text(&c, None, &avail(true));
        assert!(text.contains("not in a project"), "{text}");
    }

    #[test]
    fn prime_preview_truncates_long_lines() {
        let long = "x".repeat(PRIME_PREVIEW_CHARS + 10);
        let shown = prime_preview(&long);
        assert_eq!(shown.chars().count(), PRIME_PREVIEW_CHARS + 3);
        assert!(shown.ends_with("..."));
        assert_eq!(prime_preview("  \n \n"), "(empty)");
        assert_eq!(prime_preview("short"), "short");
    }
}
