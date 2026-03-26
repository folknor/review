mod cli;
mod config;
mod input;
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

    // #1/#2: No archetype → print help
    let archetype_name = match cli.archetype.as_deref() {
        Some(name) => name,
        None => {
            Cli::print_help();
            std::process::exit(0);
        }
    };

    if cli.raw && cli.input.is_specified() {
        bail!(
            "--raw cannot be combined with review scope flags\n\n\
             With --raw you need to provide the entire context yourself;\n\
             pointers to documents, git hashes, whatever."
        );
    }

    // #3: "review scope" not "input source"
    if !cli.raw && !cli.input.is_specified() {
        bail!(
            "no review scope specified\n\n\
             Tell the agent what to look at:\n  \
             --unstaged, --staged, --commit, --range, --document, --general\n\
             Or use --raw to send only piped stdin"
        );
    }

    let (cfg, project_root) = config::load()?;
    let stdin_instructions = input::read_stdin()?;

    let hostname = config::hostname();

    // #9/#10: Show config path and hostname in dry-run
    if cli.dry_run {
        eprintln!("config: {}", project_root.join(".review.md").display());
        eprintln!("hostname: {hostname}");
    }

    let archetypes_to_run: Vec<&str> = if archetype_name == "all" {
        cfg.frontmatter
            .archetypes
            .keys()
            .map(String::as_str)
            .collect()
    } else if let Some(group) = cfg.frontmatter.groups.get(archetype_name) {
        group.iter().map(String::as_str).collect()
    } else if cfg.frontmatter.archetypes.contains_key(archetype_name) {
        vec![archetype_name]
    } else {
        let mut available: Vec<&str> = cfg.frontmatter.archetypes.keys().map(String::as_str).collect();
        let groups: Vec<&str> = cfg.frontmatter.groups.keys().map(String::as_str).collect();
        available.extend(groups);
        bail!(
            "'{archetype_name}' not found in .review.md\n  \
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
            if let Some(arch) = cfg.frontmatter.archetypes.get(**name)
                && arch.has_sessions_for_host(&hostname)
            {
                return true;
            }
            skipped.push(name);
            false
        })
        .copied()
        .collect();

    // #6: Better error for empty results
    if runnable.is_empty() {
        if skipped.is_empty() {
            bail!(
                "no archetypes configured in .review.md\n\n\
                 Run `review init` to create a starter config."
            );
        }
        let example = skipped[0];
        bail!(
            "no sessions configured for host '{hostname}': {}\n\n\
             Add session IDs to your .review.md frontmatter, e.g.:\n\
             ---\n\
             {example}:\n  \
               {hostname}:\n    \
                 claude: \"your-session-id\"\n\
             ---",
            skipped.join(", ")
        );
    }

    for name in &skipped {
        eprintln!("warning: skipping '{name}' (no sessions for host '{hostname}' in .review.md)");
    }

    // Dry run: print assembled prompts and exit
    if cli.dry_run {
        for arch_name in &runnable {
            let assembled = if cli.raw {
                stdin_instructions.clone()
            } else {
                let context = input::context_line(&cli.input);
                prompt::assemble(arch_name, &context, &stdin_instructions)
            };
            if runnable.len() > 1 {
                println!("=== {arch_name} ===\n");
            }
            println!("{assembled}");
            if runnable.len() > 1 {
                println!();
            }
        }
        return Ok(());
    }

    // Check which providers are needed and available
    let needs_claude = runnable.iter().any(|name| {
        cfg.frontmatter
            .archetypes
            .get(*name)
            .and_then(|a| a.resolve_host(&hostname))
            .is_some_and(|h| h.claude.is_some())
    });
    let needs_codex = runnable.iter().any(|name| {
        cfg.frontmatter
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

    // Assemble prompts and spawn all providers in parallel
    let mut handles: Vec<(String, tokio::task::JoinHandle<provider::ProviderResult>)> = Vec::new();

    for arch_name in &runnable {
        let assembled = if cli.raw {
            stdin_instructions.clone()
        } else {
            let context = input::context_line(&cli.input);
            prompt::assemble(arch_name, &context, &stdin_instructions)
        };
        let arch_cfg = cfg.frontmatter.archetypes.get(*arch_name).expect("filtered above");
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

    // Print results (#11: errors to stderr)
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

    // #5: Non-zero exit when all providers failed
    let all_failed = grouped.iter().all(|(_, r)| r.output.is_err());
    if all_failed {
        std::process::exit(1);
    }

    Ok(())
}
