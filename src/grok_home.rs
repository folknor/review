//! `review`'s dealings with grok's own state directory (`$GROK_HOME`, else
//! `~/.grok`): what a `workspace-write` run needs set up there, and the event
//! log that explains a run grok reports only as `stopReason: cancelled`.
//!
//! **Folder trust** (`trusted_folders.toml`). In a folder grok does not trust,
//! every file edit raises a permission prompt, and under
//! `--permission-mode dontAsk` that prompt resolves `cancelled` - which ends the
//! whole turn rather than handing a denial back to the model. Observed on grok
//! 1.0.40: six write runs in an untrusted checkout each diagnosed their task and
//! were cancelled at their first edit. A write run therefore launches with
//! grok's own (hidden) `--trust` flag, which records the grant under grok's own
//! key and lock - `review` does not write this file. What `review` does here is
//! read it: to refuse a folder the operator explicitly declined (grok's grant
//! would overwrite that), to refuse a store grok cannot parse (grok then trusts
//! nothing, fail-closed), and to explain a cancelled turn.
//!
//! **Writable roots** (`sandbox.toml`). Grok's `workspace` profile writes only
//! to the workspace, the grok home and temp; the brokkr lock and the cargo
//! home are outside it, so every build died before compiling. The only surface
//! grok offers for more is a named profile, so `review` creates one per
//! distinct root set, named for its content (`profile_name`, `ensure_profile`)
//! and never modified after. That write copies grok's own discipline for its
//! trust store (`xai-grok-workspace/src/trust.rs`): an exclusive `flock` on
//! `<file>.lock` across the read-modify-write, `toml_edit` so other content
//! survives, a 0600 temp file renamed over the original.
//!
//! **Why a turn was cancelled** (`sessions/<encoded cwd>/<id>/events.jsonl`):
//! `cancellation_category` and the tool whose permission was refused. The
//! result object carries neither.
//!
//! The schema mirrors below (`TrustDocument`, `SandboxConfig`) copy grok's serde
//! definitions field for field, because grok deserializes each file **whole**:
//! one malformed entry anywhere and grok drops the entire file. For the trust
//! store that means nothing is trusted; for the sandbox file it means the global
//! definition of `review`'s profile vanishes and a checked-out repo's
//! `.grok/sandbox.toml` may define the name instead. Validating only our own
//! table would miss both.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

/// Grok's environment as the *child* will see it. A profile `env` redirects
/// only the child, so reading `review`'s own environment would set up - and
/// judge trust against - the wrong home: the `CODEX_HOME`/`data_root` trap.
#[derive(Debug, Clone)]
pub struct Grok {
    /// `GROK_HOME` (profile env, then process), else `<HOME>/.grok` - grok's own
    /// resolution order (`xai-dirs`).
    pub home: PathBuf,
    /// The user's home directory the child sees, canonical; grok never trusts it
    /// (`is_unsafe_trust_root`), and falls back from a git root equal to it.
    user_home: Option<PathBuf>,
}

impl Grok {
    /// Resolve from a profile `env` and the process environment. The grok home
    /// must be absolute: grok's trust store refuses a relative one and would
    /// trust nothing. (A symlink anywhere on the path to its files is refused
    /// where they are used - `check_trust`, `locked_update` - because grok
    /// refuses to load its trust-boundary files through one.)
    pub fn resolve(env: Option<&BTreeMap<String, String>>) -> anyhow::Result<Self> {
        let var = |name: &str| {
            env.and_then(|e| e.get(name))
                .map(PathBuf::from)
                .or_else(|| std::env::var_os(name).map(PathBuf::from))
                .filter(|p| !p.as_os_str().is_empty())
        };
        let user_home = var("HOME");
        let home = match var("GROK_HOME") {
            Some(p) => p,
            None => user_home.as_ref().map(|h| h.join(".grok")).ok_or_else(|| {
                anyhow::anyhow!("cannot locate grok's home (HOME and GROK_HOME unset)")
            })?,
        };
        if !home.is_absolute() {
            anyhow::bail!(
                "GROK_HOME {} is relative - grok refuses a relative home",
                home.display()
            );
        }
        let user_home = user_home.map(|h| std::fs::canonicalize(&h).unwrap_or(h));
        Ok(Self { home, user_home })
    }

    /// Grok's `is_unsafe_trust_root`: relative, the filesystem root, or the
    /// user's home (compared canonically, as grok's `is_home_dir` does).
    fn unsafe_root(&self, key: &Path) -> bool {
        !key.is_absolute()
            || key.parent().is_none()
            || self.user_home.as_deref().is_some_and(|h| h == key)
    }

    /// Grok's trust key for a directory, modelled on `trust.rs` `workspace_key`:
    /// the canonical git root containing it; for a linked worktree, the main
    /// checkout it belongs to; and the canonical directory itself when there is
    /// no repo or the repo root is an unsafe root (a dotfiles repo at `$HOME`).
    ///
    /// Used only to *read* the store. Where it cannot model grok - a grok-managed
    /// `-w` worktree, resolved through grok's sqlite registry - `check_trust`
    /// refuses rather than guess, because a wrong key here could let `--trust`
    /// overwrite a declined folder.
    pub fn workspace_key(&self, dir: &Path) -> PathBuf {
        let dir = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
        let Some(root) = dir.ancestors().find(|a| a.join(".git").exists()) else {
            return dir;
        };
        let key = main_checkout_of_worktree(root).unwrap_or_else(|| root.to_path_buf());
        let key = std::fs::canonicalize(&key).unwrap_or(key);
        if self.unsafe_root(&key) { dir } else { key }
    }
}

/// `path` or any of its ancestors is a symlink. Grok's own test
/// (`path_has_symlink_component`) for refusing trust-boundary files.
fn has_symlink_component(path: &Path) -> bool {
    path.ancestors()
        .any(|p| std::fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_symlink()))
}

/// For a linked worktree (`.git` is a file reading `gitdir: <main>/.git/worktrees/<name>`),
/// the main checkout's directory.
fn main_checkout_of_worktree(root: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(root.join(".git")).ok()?;
    let gitdir = PathBuf::from(text.strip_prefix("gitdir:")?.trim());
    let gitdir = if gitdir.is_absolute() {
        gitdir
    } else {
        root.join(gitdir)
    };
    let worktrees = gitdir.parent()?;
    (worktrees.file_name()? == "worktrees")
        .then(|| worktrees.parent()?.parent().map(Path::to_path_buf))
        .flatten()
}

/// What a `trusted_folders.toml` body says about one trust key.
#[derive(Debug, PartialEq, Eq)]
pub enum Trust {
    Trusted,
    /// No decision recorded: grok would prompt, and `dontAsk` cancels.
    Undecided,
    /// An explicit `trusted = false` - the operator said no.
    Declined,
}

/// Mirror of grok's `FolderTrust` (`trust.rs`).
#[derive(serde::Deserialize)]
#[allow(dead_code)] // fields exist to be type-checked, as grok checks them
struct FolderTrust {
    trusted: bool,
    #[serde(default)]
    decided_at: Option<i64>,
}

/// Mirror of grok's `TrustDocument` (`trust.rs`).
#[derive(serde::Deserialize)]
struct TrustDocument {
    #[serde(default)]
    folders: BTreeMap<String, FolderTrust>,
}

/// Parse a trust store the way grok does, whole. `Err` is a store grok would
/// treat as unreadable - and grok then trusts nothing at all.
fn parse_trust(text: &str) -> anyhow::Result<TrustDocument> {
    if text.trim().is_empty() {
        return Ok(TrustDocument {
            folders: BTreeMap::new(),
        });
    }
    toml::from_str(text).map_err(|e| anyhow::anyhow!("grok cannot parse trusted_folders.toml: {e}"))
}

impl Grok {
    /// The decision grok would reach for a run in `dir`. Grok's gate queries
    /// the store with the *workspace key*, not the directory
    /// (`folder_trust::decide_inputs` → `is_trusted_this_process(key)`), and
    /// `trust.rs` `is_trusted` only counts an entry the key starts with *and*
    /// that shares the key's workspace id. An ancestor of the key belongs to a
    /// different workspace, and an entry below it does not prefix it, so the
    /// only entry that can decide is the one for the key itself. (A
    /// `trusted = false` on a subdirectory therefore does not block a run keyed
    /// on the repo root - grok never consults it.)
    fn decision(&self, doc: &TrustDocument, dir: &Path) -> Trust {
        let key = self.workspace_key(dir);
        if self.unsafe_root(&key) {
            return Trust::Undecided;
        }
        match key.to_str().and_then(|k| doc.folders.get(k)) {
            Some(f) if f.trusted => Trust::Trusted,
            Some(_) => Trust::Declined,
            None => Trust::Undecided,
        }
    }

    /// Pre-launch trust check for a write run, returning the decision grok
    /// would reach. `Err` where grok's `--trust` would do the wrong thing or
    /// could not work:
    /// - an explicit `trusted = false` covering the project (grok's grant would
    ///   overwrite it - automating the yes must not overturn a recorded no);
    /// - a store grok cannot parse (it would trust nothing and cancel at the
    ///   first edit), or a symlinked one (grok refuses to load it);
    /// - a grok-managed `-w` worktree, whose key grok resolves through its own
    ///   registry, which `review` cannot read - guessing wrong could let the
    ///   grant land on a declined source repo.
    ///
    /// A missing store is `Undecided`: `--trust` creates it.
    pub fn check_trust(&self, dir: &Path) -> anyhow::Result<Trust> {
        let path = self.home.join("trusted_folders.toml");
        let canonical = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
        if canonical.starts_with(self.home.join("worktrees")) {
            anyhow::bail!(
                "{} is a grok-managed worktree; review cannot model its trust key - \
                 trust it from grok",
                dir.display()
            );
        }
        if has_symlink_component(&path) {
            anyhow::bail!(
                "{} is reached through a symlink - grok refuses to load it",
                path.display()
            );
        }
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Trust::Undecided),
            Err(e) => anyhow::bail!("cannot read {}: {e}", path.display()),
        };
        let doc = parse_trust(&text).map_err(|e| {
            anyhow::anyhow!(
                "{e} ({}) - grok would trust nothing; fix the file",
                path.display()
            )
        })?;
        let decision = self.decision(&doc, dir);
        if decision == Trust::Declined {
            anyhow::bail!(
                "{} is explicitly untrusted in {} - review will not override that; \
                 change it there if you mean to",
                self.workspace_key(dir).display(),
                path.display()
            );
        }
        Ok(decision)
    }

    /// A hint for a cancelled turn when the project was not trusted, or `None`
    /// when it was or the store cannot be read (no false blame).
    pub fn untrusted_hint(&self, dir: &Path) -> Option<String> {
        let path = self.home.join("trusted_folders.toml");
        let doc = parse_trust(&std::fs::read_to_string(&path).ok()?).ok()?;
        (self.decision(&doc, dir) != Trust::Trusted).then(|| {
            format!(
                "{} is not trusted in {}",
                self.workspace_key(dir).display(),
                path.display()
            )
        })
    }

    /// Make sure profile `name` exists in `<home>/sandbox.toml` granting exactly
    /// `roots`, returning whether this call created it. Locked because a
    /// fan-out launches several grok runs at once and two creating the same
    /// profile unlocked would race; renamed into place so a grok starting
    /// concurrently never reads a half-written file.
    pub fn ensure_profile(&self, name: &str, roots: &[String]) -> anyhow::Result<bool> {
        let mut created = false;
        locked_update(&self.home, "sandbox.toml", |text| {
            let (new_text, c) = merge_profile(text, name, roots)?;
            created = c;
            Ok(new_text)
        })?;
        Ok(created)
    }
}

/// Lines of grok's stderr that report a failed `--trust` grant. Grok prints
/// only failures (`folder_trust::report_cli_trust_grant` is silent on success),
/// and every one of them - `Couldn't save folder trust...`, a refusal reason -
/// names "folder trust". A failed grant is otherwise invisible whenever the
/// turn happens not to edit, and a process-local one means the *next* run is
/// untrusted again.
pub fn trust_failures(stderr: &str) -> Vec<String> {
    stderr
        .lines()
        .filter(|l| l.contains("folder trust"))
        .map(|l| l.trim().to_string())
        .collect()
}

/// Locked read-modify-write of `<grok_home>/<name>`: an exclusive `flock` on
/// `<name>.lock` for the whole cycle, then a unique 0600 temp file renamed over
/// the original. `update` returns the new body, or `None` to leave the file
/// untouched.
///
/// The discipline is copied from grok's writer for `trusted_folders.toml`
/// (`trust.rs` `record_decision_strict`/`persist_doc`). Grok has no writer for
/// `sandbox.toml` at all, so for that file the lock serializes `review`
/// processes against each other - a fan-out launching several grok runs at
/// once - not against grok. The rename is what protects grok: a grok starting
/// mid-write reads either the old file or the new one.
///
/// A path through a symlink is refused: grok refuses to load its
/// trust-boundary files (`sandbox.toml` among them) through one
/// (`resolve_trust_boundary_sources`), so a profile written there could never
/// be used, and following the link would drop lock and temp files into
/// wherever it points.
fn locked_update(
    grok_home: &Path,
    name: &str,
    update: impl FnOnce(&str) -> anyhow::Result<Option<String>>,
) -> anyhow::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::io::AsRawFd;

    let path = grok_home.join(name);
    if has_symlink_component(&path) {
        anyhow::bail!(
            "{} is reached through a symlink - grok refuses to load it, so review will not write it",
            path.display()
        );
    }
    std::fs::create_dir_all(grok_home)?;
    let dir = grok_home.to_path_buf();
    let file_name = name.to_string();

    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(dir.join(format!("{file_name}.lock")))?;
    // SAFETY: a valid fd owned by `lock`, which outlives the lock; the kernel
    // releases the flock when the fd closes.
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
        anyhow::bail!(
            "cannot lock {}: {}",
            path.display(),
            std::io::Error::last_os_error()
        );
    }

    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e.into()),
    };
    let Some(new_text) = update(&text).map_err(|e| anyhow::anyhow!("{e} ({})", path.display()))?
    else {
        return Ok(());
    };
    let tmp = dir.join(format!(
        "{file_name}.review-{}",
        crate::config::generate_uuid()
    ));
    let mut f = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&tmp)?;
    f.write_all(new_text.as_bytes())?;
    f.sync_all()?;
    drop(f);
    if let Err(e) = std::fs::rename(&tmp, &path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.into());
    }
    Ok(())
}

/// The cause of a cancelled turn, from a session's `events.jsonl` body.
///
/// Takes the *last* `turn_ended` (a resumed session holds several turns), and
/// the last permission refusal before it, which is what names the tool.
pub fn cancel_cause(events: &str) -> Option<String> {
    let mut refused_tool: Option<String> = None;
    let mut cause: Option<String> = None;
    for line in events.lines() {
        let Ok(ev) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        match ev.get("type").and_then(|t| t.as_str()) {
            Some("turn_started") => {
                refused_tool = None;
                cause = None;
            }
            Some("permission_resolved") => {
                let decision = ev.get("decision").and_then(|d| d.as_str());
                if decision.is_some_and(|d| d != "allow") {
                    refused_tool = ev
                        .get("tool_name")
                        .and_then(|t| t.as_str())
                        .map(str::to_string);
                }
            }
            Some("turn_ended") => {
                cause = ev
                    .get("cancellation_category")
                    .and_then(|c| c.as_str())
                    .map(str::to_string);
            }
            _ => {}
        }
    }
    let cause = cause?;
    Some(match refused_tool {
        Some(tool) => format!("{cause}: permission for `{tool}` was refused"),
        None => cause,
    })
}

/// The `events.jsonl` body for a session, found by id under any cwd directory.
///
/// Located by scanning `sessions/*/<id>/` rather than by re-encoding the cwd,
/// so this does not depend on grok's path-encoding scheme.
pub fn session_events(grok_home: &Path, session_id: &str) -> Option<String> {
    if session_id.is_empty()
        || !session_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-')
    {
        return None;
    }
    let sessions = std::fs::read_dir(grok_home.join("sessions")).ok()?;
    sessions.flatten().find_map(|cwd_dir| {
        std::fs::read_to_string(cwd_dir.path().join(session_id).join("events.jsonl")).ok()
    })
}

/// Prefix of the grok sandbox profiles `review` creates for write runs.
///
/// Grok's built-in `workspace` profile grants writes to the workspace, the grok
/// home and the temp dirs only, and grok offers no CLI flag or environment
/// variable for more: the one surface that can add a writable path is a named
/// profile in `~/.grok/sandbox.toml` (or a project `.grok/sandbox.toml`, which
/// would put a host-specific generated file into every repo). So the roots
/// `writable_roots` derives - the brokkr lock, the cargo home, a shared target -
/// reach grok through a profile `review` creates in the operator's global
/// sandbox file. Without it every grok build died at `lock: failed to open lock
/// file` before compiling, and fixers worked around it by moving `HOME` under
/// `target/`, which silently bypassed the host's build-contention lock.
///
/// **One profile per distinct root set, named for its content**
/// (`profile_name`), never one shared profile. A shared profile has to be
/// union-only (rewriting it to one run's roots would strip a concurrent run's
/// between the write and that grok starting), and union-only means a root one
/// project's `.review.toml` adds becomes writable to every later grok write run
/// on the host - broader than codex, where each run gets exactly its own roots.
/// Content-addressed profiles give grok the codex property: a run gets exactly
/// its set, a profile is created once and never modified, and concurrent runs
/// with different sets touch different entries.
pub const PROFILE_PREFIX: &str = "review-ws-";

const PROFILE_BANNER: &str = "\n# Created by `review` for grok write runs whose writable roots are exactly\n\
# the list below (derived per host - brokkr lock, cargo home, shared target -\n\
# plus any profile extras). The name is a hash of the list: never edit it;\n\
# delete the profile and review recreates it. Other profiles are left alone.\n";

/// The root set in canonical form: sorted, deduplicated. The profile name and
/// the recorded roots both use it, so a resume can check one against the other.
pub fn canonical_roots(roots: &[String]) -> Vec<String> {
    let mut v = roots.to_vec();
    v.sort();
    v.dedup();
    v
}

/// The profile name for a root set: `PROFILE_PREFIX` + 16 hex digits of
/// FNV-1a over the canonical set. FNV rather than `DefaultHasher`, whose output
/// is not stable across Rust releases - a name that changed under a toolchain
/// bump would orphan every recorded session's profile. Not a security hash and
/// not meant as one: the name only has to be stable and collision-unlikely
/// across one host's handful of sets, and `merge_profile` refuses a same-named
/// profile whose contents differ, so a collision fails loudly.
pub fn profile_name(roots: &[String]) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for root in canonical_roots(roots) {
        for b in root.bytes().chain(std::iter::once(0)) {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
    }
    format!("{PROFILE_PREFIX}{h:016x}")
}

/// Mirror of grok's `ProfileConfig` (`xai-grok-sandbox/src/profiles.rs`).
#[derive(serde::Deserialize)]
#[allow(dead_code)] // fields exist to be type-checked, as grok checks them
struct ProfileConfig {
    #[serde(default)]
    extends: Option<String>,
    #[serde(default)]
    restrict_network: Option<bool>,
    #[serde(default)]
    read_only: Vec<String>,
    #[serde(default)]
    read_write: Vec<String>,
    #[serde(default)]
    deny: Vec<String>,
}

/// Mirror of grok's `SandboxConfig`.
#[derive(serde::Deserialize)]
struct SandboxConfig {
    #[serde(default)]
    profiles: HashMap<String, ProfileConfig>,
}

/// Ensure profile `name` (`extends = "workspace"`, `read_write` = `roots`)
/// exists in a `sandbox.toml` body. Returns the new body when it had to be
/// created, and whether it was.
///
/// Create-only: an existing profile of that name is checked, never modified.
/// Its roots must be exactly `roots` and its base `workspace` (or absent, which
/// grok resolves from `workspace` - `profiles.rs` `resolve`, the `Custom` arm);
/// anything else means someone edited a content-addressed profile, or two sets
/// collided, and silently "fixing" it would rewrite the operator's config under
/// a name that no longer describes it. The result is also refused if grok
/// itself could not deserialize the whole file - see the module docs.
pub fn merge_profile(
    text: &str,
    name: &str,
    roots: &[String],
) -> anyhow::Result<(Option<String>, bool)> {
    let roots = canonical_roots(roots);
    let existing: SandboxConfig = if text.trim().is_empty() {
        SandboxConfig {
            profiles: HashMap::new(),
        }
    } else {
        toml::from_str(text).map_err(|e| {
            anyhow::anyhow!(
                "grok could not load sandbox.toml (it would ignore the whole file): {e}"
            )
        })?
    };
    if let Some(p) = existing.profiles.get(name) {
        let base_ok = matches!(p.extends.as_deref(), None | Some("workspace"));
        if !base_ok || canonical_roots(&p.read_write) != roots {
            anyhow::bail!(
                "grok sandbox profile `{name}` does not match the roots it is named for - \
                 it was edited; delete it and review will recreate it"
            );
        }
        return Ok((None, false));
    }

    let mut doc: toml_edit::DocumentMut = text
        .parse()
        .map_err(|e| anyhow::anyhow!("cannot parse grok sandbox.toml: {e}"))?;
    let profiles_item = doc.entry("profiles").or_insert_with(|| {
        let mut t = toml_edit::Table::new();
        t.set_implicit(true);
        toml_edit::Item::Table(t)
    });
    let inline = profiles_item.is_inline_table();
    let profiles = profiles_item
        .as_table_like_mut()
        .ok_or_else(|| anyhow::anyhow!("grok sandbox.toml: `profiles` is not a table"))?;
    let mut table = toml_edit::Table::new();
    table.insert("extends", toml_edit::value("workspace"));
    let list: toml_edit::Array = roots.iter().map(String::as_str).collect();
    table.insert("read_write", toml_edit::value(list));
    // A standard table gets the banner; `profiles = { ... }` written inline can
    // only take an inline entry.
    let item = if inline {
        toml_edit::value(table.into_inline_table())
    } else {
        table.decor_mut().set_prefix(PROFILE_BANNER);
        toml_edit::Item::Table(table)
    };
    profiles.insert(name, item);
    let body = doc.to_string();
    // The gate that matters: grok deserializes the whole file and drops all of
    // it on any error, taking every profile with it.
    let _: SandboxConfig = toml::from_str(&body).map_err(|e| {
        anyhow::anyhow!("grok could not load sandbox.toml after adding `{name}`: {e}")
    })?;
    Ok((Some(body), true))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch() -> PathBuf {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target/test-scratch")
            .join(crate::config::generate_uuid());
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        dir
    }

    fn roots(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn profile_names_are_stable_and_content_addressed() {
        let a = roots(&["/home/u/.brokkr", "/home/u/.cargo"]);
        // Order and duplicates do not change the set, so not the name.
        let a_shuffled = roots(&["/home/u/.cargo", "/home/u/.brokkr", "/home/u/.cargo"]);
        assert_eq!(profile_name(&a), profile_name(&a_shuffled));
        assert!(profile_name(&a).starts_with(PROFILE_PREFIX));
        // A different set is a different profile - no widening across sets.
        let b = roots(&["/home/u/.brokkr", "/home/u/.cargo", "/srv/fixtures"]);
        assert_ne!(profile_name(&a), profile_name(&b));
        // Pinned: the name must survive toolchain upgrades, or every recorded
        // session's profile would be orphaned. FNV-1a, not `DefaultHasher`.
        assert_eq!(profile_name(&[]), "review-ws-cbf29ce484222325");
    }

    #[test]
    fn profile_is_created_once_with_exact_roots_and_never_modified() {
        let set = roots(&["/home/u/.cargo", "/home/u/.brokkr"]);
        let name = profile_name(&set);
        let (text, created) = merge_profile("", &name, &set).expect("empty file");
        let text = text.expect("created");
        assert!(created);
        assert!(text.contains("Created by `review`"), "{text}");
        let parsed: toml::Table = text.parse().expect("valid toml");
        let p = &parsed["profiles"][name.as_str()];
        assert_eq!(p["extends"].as_str(), Some("workspace"));
        assert_eq!(
            p["read_write"].as_array().map(Vec::len),
            Some(2),
            "exactly this set"
        );

        // Already there with the same set: no write.
        assert_eq!(
            merge_profile(&text, &name, &set).expect("again"),
            (None, false)
        );

        // A second set gets its own profile; the first is untouched.
        let other = roots(&["/srv/fixtures"]);
        let (grown, _) = merge_profile(&text, &profile_name(&other), &other).expect("second");
        let grown: toml::Table = grown.expect("changed").parse().expect("valid");
        assert_eq!(
            grown["profiles"][name.as_str()]["read_write"]
                .as_array()
                .map(Vec::len),
            Some(2)
        );
    }

    #[test]
    fn edited_profiles_and_unloadable_files_are_refused() {
        let theirs =
            "# my comment\n[profiles.devbox2]\nextends = \"devbox\"\nread_write = [\"/x\"]\n";
        let set = roots(&["/a"]);
        let name = profile_name(&set);
        let (text, _) = merge_profile(theirs, &name, &set).expect("merge");
        assert!(
            text.expect("changed")
                .starts_with("# my comment\n[profiles.devbox2]"),
            "operator content survives"
        );

        // A content-addressed profile whose contents no longer match its name.
        let edited = format!("[profiles.{name}]\nextends = \"workspace\"\nread_write = [\"/b\"]\n");
        let err = merge_profile(&edited, &name, &set).expect_err("refused");
        assert!(err.to_string().contains("was edited"), "{err}");
        let rebased = format!("[profiles.{name}]\nextends = \"strict\"\nread_write = [\"/a\"]\n");
        assert!(merge_profile(&rebased, &name, &set).is_err());

        // Someone else's profile grok cannot deserialize: grok would drop the
        // whole file, ours included, so writing ours is pointless.
        let broken = "[profiles.theirs]\nread_write = \"/x\"\n";
        let err = merge_profile(broken, &name, &set).expect_err("refused");
        assert!(err.to_string().contains("ignore the whole file"), "{err}");
    }

    /// A `Grok` rooted at a scratch home, with a user home that no test path
    /// lives under.
    fn grok_at(home: &Path) -> Grok {
        Grok {
            home: home.to_path_buf(),
            user_home: Some(PathBuf::from("/home/nobody-here")),
        }
    }

    #[test]
    fn inline_layouts_and_a_missing_extends_are_accepted() {
        let set = roots(&["/a"]);
        let name = profile_name(&set);
        // `profiles = { ... }` inline: the new entry must be inline too.
        let inline = "profiles = { theirs = { extends = \"strict\" } }\n";
        let (text, created) = merge_profile(inline, &name, &set).expect("inline");
        assert!(created);
        let parsed: toml::Table = text.expect("changed").parse().expect("valid");
        assert_eq!(
            parsed["profiles"][name.as_str()]["extends"].as_str(),
            Some("workspace")
        );
        // Grok resolves a custom profile with no `extends` from `workspace`,
        // so one without it still matches.
        let bare = format!("[profiles.{name}]\nread_write = [\"/a\"]\n");
        assert_eq!(
            merge_profile(&bare, &name, &set).expect("no extends"),
            (None, false)
        );
    }

    #[test]
    fn ensure_profile_writes_0600_and_refuses_a_symlink() {
        use std::os::unix::fs::PermissionsExt;
        let home = scratch();
        let grok = grok_at(&home);
        let set = roots(&["/a"]);
        let name = profile_name(&set);
        assert!(grok.ensure_profile(&name, &set).expect("write"));
        let mode = std::fs::metadata(home.join("sandbox.toml"))
            .expect("written")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        assert!(!grok.ensure_profile(&name, &set).expect("noop"));

        // Grok refuses to load a symlinked trust-boundary file, so a profile
        // written through one could never be used.
        let real = home.join("dotfiles");
        std::fs::create_dir_all(&real).expect("dotfiles dir");
        std::fs::rename(home.join("sandbox.toml"), real.join("sandbox.toml")).expect("move");
        std::os::unix::fs::symlink(real.join("sandbox.toml"), home.join("sandbox.toml"))
            .expect("link");
        let other = roots(&["/b"]);
        let err = grok
            .ensure_profile(&profile_name(&other), &other)
            .expect_err("refused");
        assert!(err.to_string().contains("symlink"), "{err}");
        let _ = std::fs::remove_dir_all(&home);
    }

    /// A parsed trust store holding exactly these entries.
    fn store(entries: &[(&Path, bool)]) -> TrustDocument {
        TrustDocument {
            folders: entries
                .iter()
                .map(|(p, t)| {
                    (
                        p.to_string_lossy().into_owned(),
                        FolderTrust {
                            trusted: *t,
                            decided_at: None,
                        },
                    )
                })
                .collect(),
        }
    }

    #[test]
    fn decision_is_grok_lookup_on_the_workspace_key() {
        let grok = grok_at(Path::new("/unused"));
        let root = std::fs::canonicalize(env!("CARGO_MANIFEST_DIR")).expect("manifest dir");
        let src = root.join("src");
        let parent = root.parent().expect("parent").to_path_buf();

        assert_eq!(
            grok.decision(&store(&[(&root, true)]), &src),
            Trust::Trusted
        );
        assert_eq!(grok.decision(&store(&[]), &root), Trust::Undecided);
        // An ancestor above the repo is a different workspace: it does not count.
        assert_eq!(
            grok.decision(&store(&[(&parent, true)]), &root),
            Trust::Undecided
        );
        // Grok queries with the workspace key, so an entry below it is never
        // consulted - not even a `false` on the very directory the run is in.
        assert_eq!(
            grok.decision(&store(&[(&root, true), (&src, false)]), &src),
            Trust::Trusted
        );
        assert_eq!(
            grok.decision(&store(&[(&root, false)]), &src),
            Trust::Declined
        );
    }

    #[test]
    fn workspace_key_follows_grok() {
        let grok = grok_at(Path::new("/unused"));
        // A subdirectory of a repo keys on the repo root.
        let root = std::fs::canonicalize(env!("CARGO_MANIFEST_DIR")).expect("manifest dir");
        assert_eq!(grok.workspace_key(&root.join("src")), root);

        let base = std::fs::canonicalize(scratch()).expect("scratch");
        // A linked worktree keys on its main checkout.
        let main = base.join("main");
        std::fs::create_dir_all(main.join(".git/worktrees/wt")).expect("main repo");
        let wt = base.join("wt");
        std::fs::create_dir_all(wt.join("src")).expect("worktree");
        std::fs::write(
            wt.join(".git"),
            format!("gitdir: {}\n", main.join(".git/worktrees/wt").display()),
        )
        .expect("gitdir file");
        assert_eq!(grok.workspace_key(&wt.join("src")), main);

        // A repo root that is the user's home (dotfiles) is never the key; grok
        // falls back to the directory itself.
        let dotfiles_home = base.join("home");
        std::fs::create_dir_all(dotfiles_home.join(".git")).expect("home repo");
        std::fs::create_dir_all(dotfiles_home.join("project")).expect("project");
        let at_home = Grok {
            home: PathBuf::from("/unused"),
            user_home: Some(dotfiles_home.clone()),
        };
        assert_eq!(
            at_home.workspace_key(&dotfiles_home.join("project")),
            dotfiles_home.join("project")
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn check_trust_refuses_a_no_an_unreadable_store_and_a_grok_worktree() {
        let home = scratch();
        let grok = grok_at(&home);
        let dir = std::fs::canonicalize(scratch()).expect("project dir");
        assert_eq!(
            grok.check_trust(&dir).expect("no store yet"),
            Trust::Undecided,
            "--trust creates the store"
        );
        // The scratch dir lives inside this repo, so grok's key for it is the
        // repo root - the entries below have to be keyed the same way.
        let key = grok.workspace_key(&dir);

        let declined = format!("[folders.\"{}\"]\ntrusted = false\n", key.display());
        std::fs::write(home.join("trusted_folders.toml"), declined).expect("seed");
        let err = grok
            .check_trust(&dir)
            .expect_err("an explicit no is not overridden");
        assert!(err.to_string().contains("explicitly untrusted"), "{err}");
        assert!(grok.untrusted_hint(&dir).is_some());

        std::fs::write(
            home.join("trusted_folders.toml"),
            "[folders.\"/x\"]\ntrusted = 1\n",
        )
        .expect("seed");
        let err = grok
            .check_trust(&dir)
            .expect_err("grok would trust nothing");
        assert!(err.to_string().contains("trust nothing"), "{err}");

        let trusted = format!("[folders.\"{}\"]\ntrusted = true\n", key.display());
        std::fs::write(home.join("trusted_folders.toml"), trusted).expect("seed");
        assert_eq!(grok.check_trust(&dir).expect("trusted"), Trust::Trusted);
        assert_eq!(grok.untrusted_hint(&dir), None);

        // A grok-managed `-w` worktree: its key lives in grok's own registry.
        let managed = home.join("worktrees/abc");
        std::fs::create_dir_all(&managed).expect("managed worktree");
        let err = grok.check_trust(&managed).expect_err("cannot be modelled");
        assert!(err.to_string().contains("grok-managed worktree"), "{err}");
        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn grok_env_is_the_childs_and_the_home_must_be_absolute() {
        let env = BTreeMap::from([
            ("GROK_HOME".to_string(), "/srv/grok".to_string()),
            ("HOME".to_string(), "/srv/someone".to_string()),
        ]);
        let grok = Grok::resolve(Some(&env)).expect("profile env");
        assert_eq!(grok.home, PathBuf::from("/srv/grok"));
        assert_eq!(grok.user_home, Some(PathBuf::from("/srv/someone")));
        let relative = BTreeMap::from([("GROK_HOME".to_string(), "grok".to_string())]);
        assert!(Grok::resolve(Some(&relative)).is_err());
    }

    #[test]
    fn only_trust_failures_are_surfaced_from_stderr() {
        let stderr = "some log line\nCouldn't save folder trust: the folder path changed. Start Grok again.\n";
        assert_eq!(trust_failures(stderr).len(), 1);
        assert!(trust_failures("all good\n").is_empty());
    }

    /// Trimmed from the tail of a real grok 1.0.40 session that was cancelled
    /// at its first edit in an untrusted folder.
    const CANCELLED: &str = r#"{"ts":"2026-09-22T08:43:27.728Z","type":"phase_changed","phase":"streaming_text"}
{"ts":"2026-09-22T08:43:52.896Z","type":"tool_started","tool_name":"search_replace"}
{"ts":"2026-09-22T08:43:52.896Z","type":"permission_requested","tool_name":"search_replace"}
{"ts":"2026-09-22T08:43:52.896Z","type":"permission_resolved","tool_name":"search_replace","decision":"cancelled","wait_ms":0}
{"ts":"2026-09-22T08:43:52.946Z","type":"turn_ended","outcome":"cancelled","cancellation_category":"permission_cancelled"}"#;

    #[test]
    fn cancel_cause_names_the_category_and_the_refused_tool() {
        assert_eq!(
            cancel_cause(CANCELLED).as_deref(),
            Some("permission_cancelled: permission for `search_replace` was refused")
        );
    }

    #[test]
    fn allowed_permissions_are_not_blamed() {
        let events = r#"{"type":"permission_resolved","tool_name":"read_file","decision":"allow"}
{"type":"turn_ended","outcome":"cancelled","cancellation_category":"max_turns"}"#;
        assert_eq!(cancel_cause(events).as_deref(), Some("max_turns"));
        assert_eq!(
            cancel_cause(r#"{"type":"turn_ended","outcome":"end_turn"}"#),
            None
        );
        assert_eq!(cancel_cause(""), None);
    }

    #[test]
    fn session_events_are_found_under_any_cwd_dir_and_ids_are_validated() {
        let home = scratch();
        let sid = "7ada244b-4d05-4fc4-b2c3-3b7594ff0577";
        let dir = home.join("sessions/%2Fsome%2Fcwd").join(sid);
        std::fs::create_dir_all(&dir).expect("create session dir");
        std::fs::write(dir.join("events.jsonl"), CANCELLED).expect("write events");
        assert_eq!(session_events(&home, sid).as_deref(), Some(CANCELLED));
        assert_eq!(session_events(&home, "../etc"), None);
        assert_eq!(session_events(&home, "missing"), None);
        let _ = std::fs::remove_dir_all(&home);
    }
}
