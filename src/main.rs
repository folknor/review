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
    let cfg = config::load()?;

    let context = input::context_line(&cli.input)?;
    let stdin_instructions = input::read_stdin()?;

    let archetype_name = &cli.archetype;

    let archetypes_to_run: Vec<&str> = if archetype_name == "all" {
        config::BUILTIN_ARCHETYPES.to_vec()
    } else if config::BUILTIN_ARCHETYPES.contains(&archetype_name.as_str()) {
        vec![archetype_name.as_str()]
    } else {
        bail!(
            "unknown archetype '{archetype_name}'\n  available: {}, all",
            config::BUILTIN_ARCHETYPES.join(", ")
        );
    };

    // Filter to archetypes that have sessions configured
    let runnable: Vec<&str> = archetypes_to_run
        .iter()
        .filter(|name| {
            if let Some(arch) = cfg.frontmatter.archetypes.get(**name)
                && arch.has_sessions()
            {
                return true;
            }
            eprintln!("warning: skipping archetype '{name}' (no sessions in .review.md)");
            false
        })
        .copied()
        .collect();

    if runnable.is_empty() {
        bail!("no archetypes have sessions configured in .review.md");
    }

    // Assemble prompts and spawn all providers in parallel
    let mut handles: Vec<(String, tokio::task::JoinHandle<provider::ProviderResult>)> = Vec::new();

    for arch_name in &runnable {
        let assembled = prompt::assemble(&cfg, arch_name, &context, &stdin_instructions);
        let arch_cfg = cfg.frontmatter.archetypes.get(*arch_name).expect("filtered above");

        if let Some(ref session_id) = arch_cfg.claude {
            let sid = session_id.clone();
            let prompt = assembled.clone();
            handles.push((
                (*arch_name).to_string(),
                tokio::spawn(async move { provider::invoke_claude(&sid, &prompt).await }),
            ));
        }

        if let Some(ref session_id) = arch_cfg.codex {
            let sid = session_id.clone();
            let aname = (*arch_name).to_string();
            let prompt = assembled.clone();
            handles.push((
                (*arch_name).to_string(),
                tokio::spawn(async move { provider::invoke_codex(&sid, &aname, &prompt).await }),
            ));
        }
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

    Ok(())
}
