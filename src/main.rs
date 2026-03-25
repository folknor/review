mod cli;
mod config;
mod input;
mod prompt;
mod provider;
mod session;

use anyhow::{Result, bail};
use clap::Parser;

use cli::{Cli, ManagementCommand};

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Some(ManagementCommand::Register {
            archetype,
            claude,
            codex,
        }) => {
            let mut cfg = config::load()?;
            session::register(&mut cfg, &archetype, claude, codex)
        }
        Some(ManagementCommand::Deregister {
            archetype,
            claude,
            codex,
        }) => {
            let mut cfg = config::load()?;
            session::deregister(&mut cfg, &archetype, claude, codex)
        }
        Some(ManagementCommand::List { all }) => {
            let cfg = config::load()?;
            session::list(&cfg, all)
        }
        None => {
            let archetype = cli.archetype.expect("archetype required for review");
            run_review(&archetype, &cli.input).await
        }
    }
}

async fn run_review(archetype_name: &str, input_source: &cli::InputSource) -> Result<()> {
    let cfg = config::load()?;
    let (project_name, project) = config::resolve_project(&cfg)?;

    let resolved_input = input::resolve(input_source)?;

    let owned_name;
    let archetypes_to_run: Vec<(&String, &config::Archetype)> = if archetype_name == "all" {
        if project.archetypes.is_empty() {
            bail!(
                "no archetypes configured for project '{project_name}'\n  hint: review register <archetype> --claude <session-id>"
            );
        }
        project.archetypes.iter().collect()
    } else {
        let arch = project.archetypes.get(archetype_name).ok_or_else(|| {
            let available: Vec<_> = project.archetypes.keys().map(String::as_str).collect();
            anyhow::anyhow!(
                "unknown archetype '{archetype_name}'\n  available: {}",
                if available.is_empty() {
                    "(none)".to_string()
                } else {
                    available.join(", ")
                }
            )
        })?;
        owned_name = archetype_name.to_string();
        vec![(&owned_name, arch)]
    };

    // Filter to archetypes that have sessions; warn about the rest
    let runnable: Vec<_> = archetypes_to_run
        .iter()
        .filter(|(name, arch)| {
            if arch.has_sessions() {
                true
            } else {
                eprintln!(
                    "warning: skipping archetype '{name}' (no sessions registered)"
                );
                false
            }
        })
        .collect();

    if runnable.is_empty() {
        bail!("no archetypes have registered sessions\n  hint: review register <archetype> --claude <session-id>");
    }

    // Assemble prompts and spawn all providers across all archetypes in parallel
    let mut handles: Vec<(String, tokio::task::JoinHandle<provider::ProviderResult>)> = Vec::new();

    for (arch_name, arch) in &runnable {
        let assembled = prompt::assemble(
            &cfg.global.prefix,
            arch_name,
            &project_name,
            arch,
            &resolved_input.content_type,
            &resolved_input.content,
        )?;

        if let Some(ref session_id) = arch.claude {
            let sid = session_id.clone();
            let aname = (*arch_name).clone();
            let prompt = assembled.clone();
            handles.push((
                (*arch_name).clone(),
                tokio::spawn(async move { provider::invoke_claude(&sid, &aname, &prompt).await }),
            ));
        }

        if let Some(ref session_id) = arch.codex {
            let sid = session_id.clone();
            let aname = (*arch_name).clone();
            let prompt = assembled.clone();
            handles.push((
                (*arch_name).clone(),
                tokio::spawn(async move { provider::invoke_codex(&sid, &aname, &prompt).await }),
            ));
        }
    }

    // Collect results, grouping by archetype
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

    // Print results, adding archetype headers when multiple archetypes
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

    Ok(())
}
