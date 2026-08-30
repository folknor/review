//! Writable roots a build needs that live outside the workspace.
//!
//! `codex --sandbox workspace-write` grants exactly two things: the cwd and
//! `/tmp`. That is not enough to run a build on any of the hosts this tool is
//! used from, and the shortfall is not the project's to know about - it is a
//! property of the machine:
//!
//! - The build wrapper takes its lock under `$XDG_RUNTIME_DIR`
//!   (`/run/user/<uid>`). Failing that write is **fatal** - the run dies at
//!   `lock: failed to open lock file` before a single crate is compiled.
//! - Two of the five hosts export a global `CARGO_TARGET_DIR` onto a separate
//!   drive, and several repos carry a `target` symlink pointing at the same
//!   shared cache. Cargo cannot write there either.
//!
//! Requiring each `.review.toml` to restate those paths would put a
//! machine-shaped fact in fifteen project files, and get it wrong on the hosts
//! that were not in front of whoever edited them. So `review` derives them.
//!
//! This is deliberately the *one* place where ambient environment is allowed to
//! influence a run, and it is narrowed hard to stay defensible:
//!
//! - it applies **only** to `workspace-write`; a `read-only` profile derives
//!   nothing, because the whole point of that profile is that nothing is
//!   writable;
//! - it grants only paths that a build provably needs and that are **already
//!   outside** the workspace (a path inside cwd is dropped - it is writable
//!   anyway, and granting it would widen nothing while implying it did);
//! - every grant carries a reason and is printed at launch, so the effective
//!   permissions of a run are visible rather than inferred;
//! - it is passed *before* profile `config` overrides, so a profile can still
//!   restate `sandbox_workspace_write.writable_roots` and win.

use std::path::{Path, PathBuf};

/// One derived grant: the path, and why a build needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantedRoot {
    pub path: String,
    pub why: &'static str,
}

/// Environment and filesystem lookups, injected so the derivation is a pure
/// function under test - the real host's `CARGO_TARGET_DIR` and `target`
/// symlink must not decide whether the suite passes.
pub trait Host {
    fn var(&self, key: &str) -> Option<String>;
    /// The target of `path` if it is a symlink, resolved to an absolute path.
    fn read_link(&self, path: &Path) -> Option<PathBuf>;
}

/// The real host.
pub struct RealHost;

impl Host for RealHost {
    fn var(&self, key: &str) -> Option<String> {
        std::env::var(key).ok().filter(|v| !v.is_empty())
    }

    fn read_link(&self, path: &Path) -> Option<PathBuf> {
        // `read_link` errors on a non-symlink, which is the common case and not
        // worth distinguishing: either way there is nothing to grant.
        let target = std::fs::read_link(path).ok()?;
        if target.is_absolute() {
            Some(target)
        } else {
            // A relative link resolves against the link's own directory.
            Some(path.parent()?.join(target))
        }
    }
}

/// Paths outside `cwd` that a build needs write access to, plus any a profile
/// asked for.
///
/// Profile roots are **added to** the derived ones, not a replacement: the
/// derivation covers what any build on this machine needs, while a profile
/// covers what one project needs beyond that. They go through the same filters,
/// so a profile cannot grant `/`, cannot grant a relative path, and gains
/// nothing by naming a path already inside the workspace.
///
/// Returns an empty vec when nothing is needed - a workspace whose build stays
/// entirely inside itself derives no grants and is left with codex's defaults.
pub fn derive_with(cwd: &Path, host: &impl Host, profile_roots: &[String]) -> Vec<GrantedRoot> {
    let mut out = derive(cwd, host);
    for root in profile_roots {
        let path = PathBuf::from(root);
        if !path.is_absolute() {
            eprintln!("warning: ignoring relative writable_roots entry: {root}");
            continue;
        }
        push_root(&mut out, cwd, &path, "profile writable_roots");
    }
    out
}

/// Paths outside `cwd` that a build needs write access to.
///
/// Returns an empty vec when nothing is needed - a workspace whose build stays
/// entirely inside itself derives no grants and is left with codex's defaults.
pub fn derive(cwd: &Path, host: &impl Host) -> Vec<GrantedRoot> {
    let mut out: Vec<GrantedRoot> = Vec::new();

    // The build lock. Fatal when missing, and never inside the workspace.
    if let Some(dir) = host.var("XDG_RUNTIME_DIR") {
        let path = PathBuf::from(dir);
        if path.is_absolute() {
            push_root(&mut out, cwd, &path, "build lock ($XDG_RUNTIME_DIR)");
        }
    }

    // An explicit shared cargo cache, exported globally on some hosts.
    if let Some(dir) = host.var("CARGO_TARGET_DIR") {
        let path = PathBuf::from(dir);
        if path.is_absolute() {
            push_root(&mut out, cwd, &path, "cargo target ($CARGO_TARGET_DIR)");
        }
    }

    // A `target` symlink pointing at a shared cache. This is checked even when
    // `CARGO_TARGET_DIR` is set, because the two can disagree and the symlink
    // is what an unset-env build would use.
    if let Some(target) = host.read_link(&cwd.join("target")) {
        push_root(&mut out, cwd, &target, "cargo target (./target symlink)");
    }

    out
}

/// Add one candidate root, applying every filter a grant must pass.
fn push_root(out: &mut Vec<GrantedRoot>, cwd: &Path, path: &Path, why: &'static str) {
    // Only paths outside the workspace are worth granting: cwd is already
    // writable, so granting a subpath of it would be noise that reads like a
    // widening.
    if path.starts_with(cwd) {
        return;
    }
    let Some(text) = path.to_str() else {
        return;
    };
    // A symlink target routinely carries a trailing slash while the env var
    // naming the same directory does not, so compare and emit the normalised
    // form or the two arrive as separate grants - which is harmless to codex but
    // reads as two widenings where there is one.
    let text = text.trim_end_matches('/');
    if text.is_empty() {
        // `/` is not a target dir; it is the whole filesystem.
        return;
    }
    if out.iter().any(|g| g.path == text) {
        return;
    }
    out.push(GrantedRoot {
        path: text.to_string(),
        why,
    });
}

/// The codex `-c` override expressing a set of grants, or `None` when empty.
///
/// TOML strings are emitted with escaping so a path containing a quote or a
/// backslash cannot break out of the array and inject an unrelated key.
pub fn config_override(grants: &[GrantedRoot]) -> Option<String> {
    if grants.is_empty() {
        return None;
    }
    let list = grants
        .iter()
        .map(|g| format!("\"{}\"", escape_toml(&g.path)))
        .collect::<Vec<_>>()
        .join(",");
    Some(format!("sandbox_workspace_write.writable_roots=[{list}]"))
}

fn escape_toml(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    struct FakeHost {
        vars: HashMap<String, String>,
        links: HashMap<PathBuf, PathBuf>,
    }

    impl FakeHost {
        fn new() -> Self {
            Self {
                vars: HashMap::new(),
                links: HashMap::new(),
            }
        }
        fn var(mut self, key: &str, value: &str) -> Self {
            self.vars.insert(key.to_string(), value.to_string());
            self
        }
        fn link(mut self, at: &str, to: &str) -> Self {
            self.links.insert(PathBuf::from(at), PathBuf::from(to));
            self
        }
    }

    impl Host for FakeHost {
        fn var(&self, key: &str) -> Option<String> {
            self.vars.get(key).cloned()
        }
        fn read_link(&self, path: &Path) -> Option<PathBuf> {
            self.links.get(path).cloned()
        }
    }

    fn cwd() -> PathBuf {
        PathBuf::from("/home/dev/project")
    }

    #[test]
    fn a_workspace_that_needs_nothing_grants_nothing() {
        let host = FakeHost::new();
        assert!(derive(&cwd(), &host).is_empty());
        assert_eq!(config_override(&[]), None);
    }

    #[test]
    fn the_build_lock_directory_is_granted() {
        let host = FakeHost::new().var("XDG_RUNTIME_DIR", "/run/user/1000");
        let grants = derive(&cwd(), &host);
        assert_eq!(grants.len(), 1);
        assert_eq!(grants[0].path, "/run/user/1000");
    }

    #[test]
    fn a_global_cargo_target_dir_is_granted() {
        let host = FakeHost::new().var("CARGO_TARGET_DIR", "/media/disk/cargo");
        let grants = derive(&cwd(), &host);
        assert_eq!(grants[0].path, "/media/disk/cargo");
    }

    #[test]
    fn a_target_symlink_pointing_outside_is_granted() {
        let host = FakeHost::new().link("/home/dev/project/target", "/media/disk/cargo");
        let grants = derive(&cwd(), &host);
        assert_eq!(grants[0].path, "/media/disk/cargo");
    }

    /// A path inside the workspace is writable already. Granting it would widen
    /// nothing while making the run look as though it had been widened.
    #[test]
    fn paths_inside_the_workspace_are_not_granted() {
        let host = FakeHost::new()
            .var("CARGO_TARGET_DIR", "/home/dev/project/target")
            .link("/home/dev/project/target", "/home/dev/project/.cache");
        assert!(derive(&cwd(), &host).is_empty());
    }

    /// The env var and the symlink routinely name the same directory; granting
    /// it twice would be harmless but reads as two separate widenings.
    #[test]
    fn the_same_path_from_two_sources_is_granted_once() {
        let host = FakeHost::new()
            .var("CARGO_TARGET_DIR", "/media/disk/cargo")
            .link("/home/dev/project/target", "/media/disk/cargo");
        let grants = derive(&cwd(), &host);
        assert_eq!(grants.len(), 1);
    }

    /// Observed in the field: `read_link` returned `/media/disk/cargo/` for a
    /// symlink created with a trailing slash, while `CARGO_TARGET_DIR` named the
    /// same directory without one, and both were granted.
    #[test]
    fn a_trailing_slash_does_not_make_a_second_grant() {
        let host = FakeHost::new()
            .var("CARGO_TARGET_DIR", "/media/disk/cargo")
            .link("/home/dev/project/target", "/media/disk/cargo/");
        let grants = derive(&cwd(), &host);
        assert_eq!(grants.len(), 1, "{grants:?}");
        assert_eq!(grants[0].path, "/media/disk/cargo");
    }

    #[test]
    fn a_profile_root_is_added_to_the_derived_ones() {
        let host = FakeHost::new().var("XDG_RUNTIME_DIR", "/run/user/1000");
        let grants = derive_with(&cwd(), &host, &["/srv/fixtures".to_string()]);
        assert_eq!(grants.len(), 2);
        assert_eq!(grants[1].path, "/srv/fixtures");
        assert_eq!(grants[1].why, "profile writable_roots");
    }

    /// A profile must not be able to undo the derivation - the derived roots are
    /// what makes a build run at all, so replacing rather than extending would
    /// let a profile that names one extra path break the build it was widening.
    #[test]
    fn a_profile_root_does_not_replace_the_derived_ones() {
        let host = FakeHost::new()
            .var("XDG_RUNTIME_DIR", "/run/user/1000")
            .var("CARGO_TARGET_DIR", "/media/disk/cargo");
        let grants = derive_with(&cwd(), &host, &["/srv/fixtures".to_string()]);
        let paths: Vec<&str> = grants.iter().map(|g| g.path.as_str()).collect();
        assert!(paths.contains(&"/run/user/1000"), "{paths:?}");
        assert!(paths.contains(&"/media/disk/cargo"), "{paths:?}");
    }

    /// Profile entries go through the same filters as derived ones, so a
    /// `.review.toml` cannot hand a run the whole filesystem, and a relative
    /// path (which codex would reject as a config error at launch) is dropped
    /// with a warning rather than emitted.
    #[test]
    fn a_profile_cannot_grant_the_filesystem_root_or_a_relative_path() {
        let host = FakeHost::new();
        let grants = derive_with(
            &cwd(),
            &host,
            &["/".to_string(), "../elsewhere".to_string()],
        );
        assert!(grants.is_empty(), "{grants:?}");
    }

    /// A profile naming a path the host already derived should not double it.
    #[test]
    fn a_profile_root_matching_a_derived_one_is_not_repeated() {
        let host = FakeHost::new().var("CARGO_TARGET_DIR", "/media/disk/cargo");
        let grants = derive_with(&cwd(), &host, &["/media/disk/cargo".to_string()]);
        assert_eq!(grants.len(), 1);
        assert_eq!(grants[0].why, "cargo target ($CARGO_TARGET_DIR)");
    }

    /// Granting `/` would hand the run the entire filesystem, which is the one
    /// outcome this module must never produce.
    #[test]
    fn the_filesystem_root_is_never_granted() {
        let host = FakeHost::new().var("CARGO_TARGET_DIR", "/");
        assert!(derive(&cwd(), &host).is_empty());
    }

    /// The two can disagree - an exported var overriding a repo's own symlink.
    /// A build may consult either depending on how it is invoked, so both are
    /// granted rather than guessing which one wins.
    #[test]
    fn a_disagreeing_env_var_and_symlink_are_both_granted() {
        let host = FakeHost::new()
            .var("CARGO_TARGET_DIR", "/media/disk/cargo")
            .link("/home/dev/project/target", "/media/other/cargo");
        let grants = derive(&cwd(), &host);
        assert_eq!(grants.len(), 2);
    }

    /// A relative `CARGO_TARGET_DIR` resolves against the build's own cwd, so it
    /// is already inside the workspace and there is nothing to grant. Passing it
    /// through verbatim would emit a relative path into an absolute-path config.
    #[test]
    fn a_relative_cargo_target_dir_is_ignored() {
        let host = FakeHost::new().var("CARGO_TARGET_DIR", "../shared-target");
        assert!(derive(&cwd(), &host).is_empty());
    }

    #[test]
    fn an_empty_env_var_is_ignored() {
        let host = FakeHost::new().var("XDG_RUNTIME_DIR", "");
        // The real host filters empties; the fake stores them, so assert the
        // absolute-path guard rejects it too.
        assert!(derive(&cwd(), &host).is_empty());
    }

    #[test]
    fn the_override_is_a_toml_array_of_the_granted_paths() {
        let host = FakeHost::new()
            .var("XDG_RUNTIME_DIR", "/run/user/1000")
            .var("CARGO_TARGET_DIR", "/media/disk/cargo");
        let grants = derive(&cwd(), &host);
        assert_eq!(
            config_override(&grants).as_deref(),
            Some(
                r#"sandbox_workspace_write.writable_roots=["/run/user/1000","/media/disk/cargo"]"#
            )
        );
    }

    /// A path is attacker-influenced only in the sense that it comes from the
    /// environment, but an unescaped quote would end the TOML string and let the
    /// rest of the value set an unrelated config key.
    #[test]
    fn a_quote_in_a_path_cannot_inject_another_config_key() {
        let hostile = "/run/\"],approval_policy=\"on-request";
        let host = FakeHost::new().var("XDG_RUNTIME_DIR", hostile);
        let grants = derive(&cwd(), &host);
        let Some(rendered) = config_override(&grants) else {
            panic!("a granted root must render an override");
        };

        // Substring checks would be fooled here: the injected text is present
        // verbatim, correctly quarantined *inside* a TOML string. The property
        // that matters is what a parser sees, so parse it.
        let parsed: toml::Table = rendered
            .parse()
            .unwrap_or_else(|e| panic!("override must be valid TOML: {e}: {rendered}"));
        assert_eq!(
            parsed.len(),
            1,
            "injection created a second key: {parsed:?}"
        );
        let roots = parsed["sandbox_workspace_write"]["writable_roots"]
            .as_array()
            .unwrap_or_else(|| panic!("writable_roots must be an array: {parsed:?}"));
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0].as_str(), Some(hostile));
    }
}
