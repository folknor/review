use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Deserialize, Serialize)]
pub struct Config {
    pub global: Global,
    #[serde(default)]
    pub projects: BTreeMap<String, Project>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Global {
    pub prefix: String,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Project {
    pub path: String,
    #[serde(default)]
    pub archetypes: BTreeMap<String, Archetype>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Archetype {
    pub claude: Option<String>,
    pub codex: Option<String>,
    pub prompt_diff: String,
    pub prompt_document: String,
}

impl Archetype {
    pub fn has_sessions(&self) -> bool {
        self.claude.is_some() || self.codex.is_some()
    }
}

fn config_path() -> PathBuf {
    dirs_or_default().join("config.toml")
}

fn dirs_or_default() -> PathBuf {
    let base = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").expect("HOME not set");
            PathBuf::from(home).join(".config")
        });
    base.join("review")
}

pub fn load() -> Result<Config> {
    let path = config_path();
    let contents = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read config at {}", path.display()))?;
    let config: Config =
        toml::from_str(&contents).with_context(|| "failed to parse config.toml")?;
    Ok(config)
}

pub fn save(config: &Config) -> Result<()> {
    let path = config_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let contents = toml::to_string_pretty(config).context("failed to serialize config")?;

    // Atomic write: write to temp file in same directory, then rename
    let tmp_path = path.with_extension("toml.tmp");
    std::fs::write(&tmp_path, &contents).context("failed to write temp config")?;
    std::fs::rename(&tmp_path, &path).context("failed to rename temp config into place")?;
    Ok(())
}

pub fn expand_path(p: &str) -> PathBuf {
    PathBuf::from(shellexpand::tilde(p).into_owned())
}

/// Best-effort canonicalization: resolves symlinks if possible, falls back to the original path.
fn canonicalize_best_effort(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

/// Resolve the current directory to a registered project. Returns (project_name, project).
pub fn resolve_project(config: &Config) -> Result<(String, &Project)> {
    let cwd = std::env::current_dir().context("failed to get current directory")?;
    resolve_project_from(config, &cwd)
}

pub fn resolve_project_from<'a>(config: &'a Config, cwd: &Path) -> Result<(String, &'a Project)> {
    let cwd = canonicalize_best_effort(cwd);
    let mut best: Option<(String, &Project, usize)> = None;
    for (name, project) in &config.projects {
        let project_path = canonicalize_best_effort(&expand_path(&project.path));
        if cwd.starts_with(&project_path) {
            let depth = project_path.components().count();
            if best.as_ref().is_none_or(|(_, _, d)| depth > *d) {
                best = Some((name.clone(), project, depth));
            }
        }
    }
    match best {
        Some((name, project, _)) => Ok((name, project)),
        None => bail!(
            "current directory is not a registered project\n  cwd: {}\n  hint: add a [projects.<name>] entry in ~/.config/review/config.toml",
            cwd.display()
        ),
    }
}

/// Resolve project mutably for config updates.
pub fn resolve_project_mut(config: &mut Config) -> Result<(String, &mut Project)> {
    let cwd = std::env::current_dir().context("failed to get current directory")?;
    let cwd = canonicalize_best_effort(&cwd);
    let mut best: Option<(String, usize)> = None;
    for (name, project) in &config.projects {
        let project_path = canonicalize_best_effort(&expand_path(&project.path));
        if cwd.starts_with(&project_path) {
            let depth = project_path.components().count();
            if best.as_ref().is_none_or(|(_, d)| depth > *d) {
                best = Some((name.clone(), depth));
            }
        }
    }
    match best {
        Some((name, _)) => {
            let project = config.projects.get_mut(&name).expect("project disappeared from map");
            Ok((name, project))
        }
        None => bail!(
            "current directory is not a registered project\n  cwd: {}\n  hint: add a [projects.<name>] entry in ~/.config/review/config.toml",
            cwd.display()
        ),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn test_config() -> Config {
        let mut projects = BTreeMap::new();
        projects.insert(
            "repo".to_string(),
            Project {
                path: "/home/user/repo".to_string(),
                archetypes: BTreeMap::new(),
            },
        );
        projects.insert(
            "subproject".to_string(),
            Project {
                path: "/home/user/repo/subproject".to_string(),
                archetypes: BTreeMap::new(),
            },
        );
        projects.insert(
            "other".to_string(),
            Project {
                path: "/home/user/other".to_string(),
                archetypes: BTreeMap::new(),
            },
        );
        Config {
            global: Global {
                prefix: "~/.config/review/prompts/prefix.md".to_string(),
            },
            projects,
        }
    }

    #[test]
    fn resolves_exact_project_path() {
        let cfg = test_config();
        let (name, _) = resolve_project_from(&cfg, Path::new("/home/user/other")).unwrap();
        assert_eq!(name, "other");
    }

    #[test]
    fn resolves_subdirectory_to_project() {
        let cfg = test_config();
        let (name, _) =
            resolve_project_from(&cfg, Path::new("/home/user/other/src/lib")).unwrap();
        assert_eq!(name, "other");
    }

    #[test]
    fn longest_prefix_wins_for_nested_projects() {
        let cfg = test_config();
        let (name, _) =
            resolve_project_from(&cfg, Path::new("/home/user/repo/subproject/src")).unwrap();
        assert_eq!(name, "subproject");
    }

    #[test]
    fn parent_project_still_matches_outside_subproject() {
        let cfg = test_config();
        let (name, _) =
            resolve_project_from(&cfg, Path::new("/home/user/repo/other_dir")).unwrap();
        assert_eq!(name, "repo");
    }

    #[test]
    fn no_match_returns_error() {
        let cfg = test_config();
        let result = resolve_project_from(&cfg, Path::new("/home/user/unknown"));
        assert!(result.is_err());
    }

    #[test]
    fn has_sessions_both() {
        let arch = Archetype {
            claude: Some("c".into()),
            codex: Some("x".into()),
            prompt_diff: "d".into(),
            prompt_document: "d".into(),
        };
        assert!(arch.has_sessions());
    }

    #[test]
    fn has_sessions_none() {
        let arch = Archetype {
            claude: None,
            codex: None,
            prompt_diff: "d".into(),
            prompt_document: "d".into(),
        };
        assert!(!arch.has_sessions());
    }
}
