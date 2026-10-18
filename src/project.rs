//! `-C <project>`: which project a command acts on, when that is not the one
//! the invoking directory is in.
//!
//! An orchestrating agent working in one checkout asks for a review of
//! another. The process never changes directory: providers and writable roots
//! already take the project root explicitly, so only config discovery has to
//! start somewhere else, and keeping cwd still leaves every relative path the
//! operator's environment holds (`XDG_DATA_HOME`, `TMPDIR`, `PATH`) meaning what
//! it meant before. Hence a `config::Selection` rather than a `chdir`.
//!
//! A selector is a **path** when it is spelled like one - `/...`, `./...`,
//! `../...`, `.`, `..`, `~` or `~/...` - and a **registered name** otherwise.
//! There is no fallback from one to the other: if a name that did not resolve
//! fell back to a directory of that name, registering a project would silently
//! retarget every command that had been spelling that directory, and a
//! registered checkout that went missing would quietly resolve to whatever sat
//! in the cwd under the same name.
//!
//! Names live in the global config, keyed by host because paths differ between
//! machines:
//!
//! ```toml
//! [_projects.bygg]
//! bz = "~/Programs/bz"
//! "folknor/piners" = "$HOME/src/piners"
//! ```
//!
//! A name matches a key ignoring ASCII case, then - the bz convention for
//! `owner/name` - the part after a key's last `/`; an ambiguous short name is an
//! error. A registered path is the project root exactly (`Selection::Exact`); a
//! path selector is discovered from, as the cwd always has been.

use anyhow::{Result, bail};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::config::Selection;

/// Whether `selector` is spelled as a path rather than a registered name.
fn is_path(selector: &str) -> bool {
    matches!(selector, "." | ".." | "~")
        || ["/", "./", "../", "~/"]
            .iter()
            .any(|p| selector.starts_with(p))
}

/// A registry key must be a name: anything spelled as a path could never be
/// selected by it, so it is refused where it is written.
pub fn check_name(name: &str) -> std::result::Result<(), String> {
    if name.trim().is_empty() {
        return Err("a project name cannot be empty".to_string());
    }
    if name.starts_with('~') || is_path(name) {
        return Err(format!(
            "'{name}' is spelled like a path, so `-C {name}` would never look it up"
        ));
    }
    if name.ends_with('/') || name.chars().any(char::is_whitespace) {
        return Err(format!(
            "'{name}' has a trailing slash or whitespace, which no selector can match"
        ));
    }
    Ok(())
}

/// Resolve `-C`. `None` is the invoking directory, as without the flag.
pub fn select(selector: Option<&str>) -> Result<Selection> {
    let Some(selector) = selector else {
        return Selection::cwd();
    };
    if is_path(selector) {
        let expanded =
            expand(selector, &env_var).map_err(|e| anyhow::anyhow!("-C {selector}: {e}"))?;
        let path = PathBuf::from(&expanded);
        let path = if path.is_absolute() {
            path
        } else {
            std::env::current_dir()
                .map_err(|e| anyhow::anyhow!("failed to get current directory: {e}"))?
                .join(path)
        };
        if !path.is_dir() {
            bail!("-C {selector}: {} is not a directory", path.display());
        }
        return Ok(Selection::Discover(canonical(&path)));
    }
    if selector.starts_with('~') {
        bail!("-C {selector}: only `~` and `~/...` expand; spell the path out");
    }
    let (registry, global) = crate::config::global_projects()?;
    let path = lookup(selector, &registry, &env_var).map_err(|why| {
        let file = global
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "the global config".to_string());
        anyhow::anyhow!(
            "-C {selector}: {why}\n  \
             Projects are registered in {file} under [_projects.{}]; a path needs a \
             path spelling (./{selector}).",
            crate::config::toml_key(&crate::config::hostname())
        )
    })?;
    if !path.is_dir() {
        bail!(
            "-C {selector}: registered as {}, which is not a directory on this host",
            path.display()
        );
    }
    Ok(Selection::Exact(canonical(&path)))
}

/// The registered path for `name`, expanded and checked to be absolute. Pure
/// over the registry and environment so every rule is tested.
fn lookup(
    name: &str,
    registry: &BTreeMap<String, String>,
    var: &dyn Fn(&str) -> Option<String>,
) -> std::result::Result<PathBuf, String> {
    let exact: Vec<&String> = registry
        .keys()
        .filter(|k| k.eq_ignore_ascii_case(name))
        .collect();
    let key = match exact.as_slice() {
        [one] => *one,
        _ => {
            let short: Vec<&String> = registry
                .keys()
                .filter(|k| {
                    k.rsplit_once('/')
                        .is_some_and(|(_, tail)| tail.eq_ignore_ascii_case(name))
                })
                .collect();
            match short.as_slice() {
                [one] => *one,
                [] => {
                    let known: Vec<&str> = registry.keys().map(String::as_str).collect();
                    return Err(format!(
                        "no project registered under that name on this host (registered: {})",
                        if known.is_empty() {
                            "none".to_string()
                        } else {
                            known.join(", ")
                        }
                    ));
                }
                many => {
                    let names: Vec<&str> = many.iter().map(|s| s.as_str()).collect();
                    return Err(format!(
                        "ambiguous: matches {}; use the full name",
                        names.join(", ")
                    ));
                }
            }
        }
    };
    let raw = &registry[key];
    let expanded = expand(raw, var).map_err(|e| format!("registered path {raw}: {e}"))?;
    let path = PathBuf::from(&expanded);
    if !path.is_absolute() {
        return Err(format!(
            "registered path {raw} is not absolute; a registry entry cannot depend on \
             the directory review was started from"
        ));
    }
    Ok(path)
}

/// Expand `~`, `~/`, `$VAR` and `${VAR}`. Every failure is an error: unlike a
/// writable root, which can be dropped with a warning, a project target that
/// cannot be expanded leaves nothing to run the command against.
fn expand(raw: &str, var: &dyn Fn(&str) -> Option<String>) -> Result<String> {
    let home = || var("HOME").ok_or_else(|| anyhow::anyhow!("HOME is not set"));
    let raw = if raw == "~" {
        home()?
    } else if let Some(rest) = raw.strip_prefix("~/") {
        format!("{}/{rest}", home()?.trim_end_matches('/'))
    } else if raw.starts_with('~') {
        bail!("only `~` and `~/...` expand");
    } else {
        raw.to_string()
    };
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw.as_str();
    while let Some(idx) = rest.find('$') {
        out.push_str(&rest[..idx]);
        let after = &rest[idx + 1..];
        let (name, tail) = if let Some(braced) = after.strip_prefix('{') {
            match braced.find('}') {
                Some(end) => (&braced[..end], &braced[end + 1..]),
                None => bail!("unclosed '${{'"),
            }
        } else {
            let end = after
                .find(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                .unwrap_or(after.len());
            (&after[..end], &after[end..])
        };
        if name.is_empty() {
            bail!("empty variable name");
        }
        match var(name) {
            Some(value) => out.push_str(&value),
            None => bail!("${name} is not set"),
        }
        rest = tail;
    }
    out.push_str(rest);
    Ok(out)
}

fn env_var(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Whether a recorded project string names `root`. New rows record canonical
/// roots; older ones may carry a symlinked spelling, so a recorded path that
/// still exists is compared canonicalized, and one that is gone by its raw
/// string - its rows stay listed under the identity they were filed with.
pub fn same_project(recorded: &str, root: &Path) -> bool {
    let rec = Path::new(recorded);
    rec == root || canonical(rec) == canonical(root)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn reg(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    fn env(k: &str) -> Option<String> {
        match k {
            "HOME" => Some("/home/u".to_string()),
            "SRC" => Some("/srv/src".to_string()),
            _ => None,
        }
    }

    #[test]
    fn path_spellings_bypass_the_registry() {
        for s in [".", "..", "~", "/abs", "./rel", "../up", "~/x"] {
            assert!(is_path(s), "{s}");
        }
        for s in ["bz", "folknor/piners", "a.b", ".hidden"] {
            assert!(!is_path(s), "{s}");
        }
    }

    #[test]
    fn names_match_exactly_then_by_short_name_ignoring_case() {
        let r = reg(&[("folknor/piners", "/p"), ("bz", "/b")]);
        assert_eq!(lookup("BZ", &r, &env).unwrap(), PathBuf::from("/b"));
        assert_eq!(
            lookup("Folknor/Piners", &r, &env).unwrap(),
            PathBuf::from("/p")
        );
        assert_eq!(lookup("piners", &r, &env).unwrap(), PathBuf::from("/p"));
    }

    #[test]
    fn an_ambiguous_short_name_is_an_error() {
        let r = reg(&[("a/tool", "/a"), ("b/tool", "/b")]);
        let e = lookup("tool", &r, &env).unwrap_err();
        assert!(e.contains("ambiguous"), "{e}");
        assert_eq!(lookup("a/tool", &r, &env).unwrap(), PathBuf::from("/a"));
    }

    #[test]
    fn an_unknown_name_lists_what_is_registered() {
        let r = reg(&[("bz", "/b")]);
        let e = lookup("nope", &r, &env).unwrap_err();
        assert!(e.contains("registered: bz"), "{e}");
    }

    #[test]
    fn registered_paths_expand_strictly() {
        let r = reg(&[
            ("a", "~/Programs/a"),
            ("b", "${SRC}/b"),
            ("c", "$UNSET/c"),
            ("d", "relative/d"),
            ("e", "~other/e"),
        ]);
        assert_eq!(
            lookup("a", &r, &env).unwrap(),
            PathBuf::from("/home/u/Programs/a")
        );
        assert_eq!(lookup("b", &r, &env).unwrap(), PathBuf::from("/srv/src/b"));
        assert!(lookup("c", &r, &env).unwrap_err().contains("$UNSET"));
        assert!(lookup("d", &r, &env).unwrap_err().contains("not absolute"));
        assert!(lookup("e", &r, &env).unwrap_err().contains("only `~`"));
    }

    #[test]
    fn registry_keys_must_be_names() {
        for bad in ["", "./x", "/x", "~x", "~", "x/", "a b"] {
            assert!(check_name(bad).is_err(), "{bad:?}");
        }
        for good in ["bz", "folknor/piners", ".hidden"] {
            assert!(check_name(good).is_ok(), "{good:?}");
        }
    }

    #[test]
    fn a_vanished_recorded_project_keeps_its_raw_identity() {
        assert!(same_project("/no/such/dir", Path::new("/no/such/dir")));
        assert!(!same_project("/no/such/dir", Path::new("/no/such/other")));
    }
}
