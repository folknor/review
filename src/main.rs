mod cli;
mod config;
mod input;
mod lock;
mod prompt;
mod provider;

use anyhow::{Result, bail};
use clap::Parser;

use cli::Cli;

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    if matches!(cli.command, Some(cli::Command::Init)) {
        return config::init();
    }


    let archetype_name = match cli.archetype.as_deref() {
        Some(name) => name,
        None => {
            Cli::print_help();
            std::process::exit(2);
        }
    };

    let (cfg, project_root) = config::load()?;
    let stdin_instructions = input::read_stdin()?;

    let hostname = config::hostname();

    if cli.dry_run {
        eprintln!("config: {}", project_root.join(".review.toml").display());
        eprintln!("hostname: {hostname}");
    }

    let archetypes_to_run: Vec<&str> = if archetype_name == "all" {
        cfg
            .archetypes
            .keys()
            .map(String::as_str)
            .collect()
    } else if let Some(group) = cfg.groups.get(archetype_name) {
        group.iter().map(String::as_str).collect()
    } else if cfg.archetypes.contains_key(archetype_name) {
        vec![archetype_name]
    } else {
        let mut available: Vec<&str> = cfg.archetypes.keys().map(String::as_str).collect();
        let groups: Vec<&str> = cfg.groups.keys().map(String::as_str).collect();
        available.extend(groups);
        bail!(
            "'{archetype_name}' not found in .review.toml\n  \
             configured: {}",
            if available.is_empty() {
                "(none)".to_string()
            } else {
                available.join(", ")
            }
        );
    };

    // Filter to archetypes that have sessions configured for this host
    let mut skipped: Vec<&str> = Vec::new();
    let runnable: Vec<&str> = archetypes_to_run
        .iter()
        .filter(|name| {
            if let Some(arch) = cfg.archetypes.get(**name)
                && arch.has_sessions_for_host(&hostname)
            {
                return true;
            }
            skipped.push(name);
            false
        })
        .copied()
        .collect();

    if runnable.is_empty() {
        let host_key = config::toml_key(&hostname);
        if skipped.is_empty() {
            bail!(
                "no archetypes configured in .review.toml\n\n\
                 Add session IDs to your .review.toml, e.g.:\n\n\
                 [security.{host_key}]\n\
                 claude = \"your-session-id\""
            );
        }
        let example = skipped[0];
        bail!(
            "no sessions configured for host '{hostname}': {}\n\n\
             Add session IDs to your .review.toml, e.g.:\n\n\
             [{example}.{host_key}]\n\
             claude = \"your-session-id\"",
            skipped.join(", ")
        );
    }

    for name in &skipped {
        eprintln!("warning: skipping '{name}' (no sessions for host '{hostname}' in .review.toml)");
    }

    // Dry run: print what would be sent and exit
    if cli.dry_run {
        for arch_name in &runnable {
            let prompt = if cli.anchor {
                prompt::assemble(&stdin_instructions)
            } else {
                stdin_instructions.clone()
            };
            if runnable.len() > 1 {
                println!("=== {arch_name} ===\n");
            }
            println!("{prompt}");
            if runnable.len() > 1 {
                println!();
            }
        }
        return Ok(());
    }

    // Global lock — one review invocation at a time across all projects.
    // Uses flock(2) advisory lock; released automatically when lock_file is dropped.
    let lock_path = std::env::temp_dir().join("review.lock");
    let lock_file = std::fs::File::create(&lock_path)
        .map_err(|e| anyhow::anyhow!("failed to create lock file: {e}"))?;
    lock::acquire_blocking(&lock_file)?;

    // Check which providers are needed and available
    let needs_claude = runnable.iter().any(|name| {
        cfg
            .archetypes
            .get(*name)
            .and_then(|a| a.resolve_host(&hostname))
            .is_some_and(|h| h.claude.is_some())
    });
    let needs_codex = runnable.iter().any(|name| {
        cfg
            .archetypes
            .get(*name)
            .and_then(|a| a.resolve_host(&hostname))
            .is_some_and(|h| h.codex.is_some())
    });

    let claude_available = !needs_claude || provider::is_available("claude");
    let codex_available = !needs_codex || provider::is_available("codex");

    if needs_claude && !claude_available {
        eprintln!("warning: 'claude' not found on PATH, skipping claude sessions");
    }
    if needs_codex && !codex_available {
        eprintln!("warning: 'codex' not found on PATH, skipping codex sessions");
    }

    // Spawn all providers in parallel
    let mut handles: Vec<(String, tokio::task::JoinHandle<provider::ProviderResult>)> = Vec::new();

    for arch_name in &runnable {
        let assembled = if cli.anchor {
            prompt::assemble(&stdin_instructions)
        } else {
            stdin_instructions.clone()
        };
        let arch_cfg = cfg.archetypes.get(*arch_name).expect("filtered above");
        let host_cfg = arch_cfg.resolve_host(&hostname).expect("filtered above");

        if claude_available
            && let Some(ref session_id) = host_cfg.claude
        {
            let sid = session_id.clone();
            let prompt = assembled.clone();
            let root = project_root.clone();
            handles.push((
                (*arch_name).to_string(),
                tokio::spawn(async move { provider::invoke_claude(&sid, &prompt, &root).await }),
            ));
        }

        if codex_available
            && let Some(ref session_id) = host_cfg.codex
        {
            let sid = session_id.clone();
            let aname = (*arch_name).to_string();
            let prompt = assembled.clone();
            let root = project_root.clone();
            handles.push((
                (*arch_name).to_string(),
                tokio::spawn(async move { provider::invoke_codex(&sid, &aname, &prompt, &root).await }),
            ));
        }
    }

    if handles.is_empty() {
        let mut missing = Vec::new();
        if needs_claude && !claude_available {
            missing.push("claude");
        }
        if needs_codex && !codex_available {
            missing.push("codex");
        }
        bail!(
            "no providers available to run\n\n\
             Required but not found on PATH: {}\n\
             Install the missing provider(s) to proceed.",
            missing.join(", ")
        );
    }

    // Collect results
    let mut grouped: Vec<(String, provider::ProviderResult)> = Vec::new();
    for (arch_name, handle) in handles {
        let result = match handle.await {
            Ok(r) => r,
            Err(err) => provider::ProviderResult {
                provider: "unknown".into(),
                output: Err(anyhow::anyhow!("task panicked: {err}")),
            },
        };
        grouped.push((arch_name, result));
    }

    // Print results
    let multi = runnable.len() > 1;
    let mut current_arch = "";
    for (arch_name, result) in &grouped {
        if multi && arch_name.as_str() != current_arch {
            if !current_arch.is_empty() {
                println!();
            }
            println!("=== {arch_name} ===\n");
            current_arch = arch_name;
        }
        provider::print_result(result);
    }

    let all_failed = grouped.iter().all(|(_, r)| r.output.is_err());
    if all_failed {
        std::process::exit(1);
    }

    Ok(())
}
