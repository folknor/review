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
    std::fs::write(&path, contents).context("failed to write config")?;
    Ok(())
}

pub fn expand_path(p: &str) -> PathBuf {
    PathBuf::from(shellexpand::tilde(p).into_owned())
}

/// Resolve the current directory to a registered project. Returns (project_name, project).
pub fn resolve_project(config: &Config) -> Result<(String, &Project)> {
    let cwd = std::env::current_dir().context("failed to get current directory")?;
    resolve_project_from(config, &cwd)
}

pub fn resolve_project_from<'a>(config: &'a Config, cwd: &Path) -> Result<(String, &'a Project)> {
    for (name, project) in &config.projects {
        let project_path = expand_path(&project.path);
        if cwd.starts_with(&project_path) {
            return Ok((name.clone(), project));
        }
    }
    bail!(
        "current directory is not a registered project\n  cwd: {}\n  hint: add a [projects.<name>] entry in ~/.config/review/config.toml",
        cwd.display()
    );
}

/// Resolve project mutably for config updates.
pub fn resolve_project_mut(config: &mut Config) -> Result<(String, &mut Project)> {
    let cwd = std::env::current_dir().context("failed to get current directory")?;
    for (name, project) in &config.projects {
        let project_path = expand_path(&project.path);
        if cwd.starts_with(&project_path) {
            let name = name.clone();
            let project = config.projects.get_mut(&name).unwrap();
            return Ok((name, project));
        }
    }
    bail!(
        "current directory is not a registered project\n  cwd: {}\n  hint: add a [projects.<name>] entry in ~/.config/review/config.toml",
        cwd.display()
    );
}
