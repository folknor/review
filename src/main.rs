mod cli;
mod config;
mod input;
mod prompt;
mod provider;
mod session;

use anyhow::{Result, bail};
use clap::Parser;

use cli::{Cli, Command};

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::Review { archetype, input } => run_review(&archetype, &input).await,
        Command::Register {
            archetype,
            claude,
            codex,
        } => {
            let mut cfg = config::load()?;
            session::register(&mut cfg, &archetype, claude, codex)
        }
        Command::Deregister {
            archetype,
            claude,
            codex,
        } => {
            let mut cfg = config::load()?;
            session::deregister(&mut cfg, &archetype, claude, codex)
        }
        Command::List { all } => {
            let cfg = config::load()?;
            session::list(&cfg, all)
        }
    }
}

async fn run_review(archetype_name: &str, input_source: &cli::InputSource) -> Result<()> {
    let cfg = config::load()?;
    let (project_name, project) = config::resolve_project(&cfg)?;

    let resolved_input = input::resolve(input_source)?;

    let owned_name;
    let archetypes_to_run: Vec<(&String, &config::Archetype)> = if archetype_name == "all" {
        project.archetypes.iter().collect()
    } else {
        let arch = project.archetypes.get(archetype_name).ok_or_else(|| {
            let available: Vec<_> = project.archetypes.keys().map(|s| s.as_str()).collect();
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

    for (arch_name, arch) in &archetypes_to_run {
        if !arch.has_sessions() {
            bail!(
                "no sessions registered for archetype '{arch_name}'\n  hint: review register {arch_name} --claude <session-id>"
            );
        }
    }

    let mut all_results = Vec::new();

    for (arch_name, arch) in &archetypes_to_run {
        let assembled = prompt::assemble(
            &cfg.global.prefix,
            arch_name,
            &project_name,
            arch,
            &resolved_input.content_type,
            &resolved_input.content,
        )?;

        let mut handles = Vec::new();

        if let Some(ref session_id) = arch.claude {
            let sid = session_id.clone();
            let aname = arch_name.to_string();
            let prompt = assembled.clone();
            handles.push(tokio::spawn(
                async move { provider::invoke_claude(&sid, &aname, &prompt).await },
            ));
        }

        if let Some(ref session_id) = arch.codex {
            let sid = session_id.clone();
            let aname = arch_name.to_string();
            let prompt = assembled.clone();
            handles.push(tokio::spawn(
                async move { provider::invoke_codex(&sid, &aname, &prompt).await },
            ));
        }

        let mut results = Vec::new();
        for handle in handles {
            results.push(handle.await?);
        }

        if archetypes_to_run.len() > 1 {
            println!("=== {arch_name} ===\n");
        }
        provider::print_results(&results);
        all_results.extend(results);

        if archetypes_to_run.len() > 1 {
            println!();
        }
    }

    Ok(())
}
