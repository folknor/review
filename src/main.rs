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

    if cli.is_init() {
        return config::init();
    }

    let archetype_name = cli.archetype_name().ok_or_else(|| {
        anyhow::anyhow!("no archetype specified\n  Use a subcommand (security, bugs, perf, arch, all) or --type <name>")
    })?;

    let input = cli.input_source().expect("not init");
    if !input.is_specified() {
        bail!(
            "no input source specified\n\n\
             Provide one of: --unstaged, --staged, --commit, --range, --document, --general"
        );
    }

    let (cfg, project_root) = config::load()?;
    let context = input::context_line(input);
    let stdin_instructions = input::read_stdin()?;

    let hostname = config::hostname();

    let archetypes_to_run: Vec<&str> = if archetype_name == "all" {
        // "all" runs all archetypes that have sessions, not just built-ins
        cfg.frontmatter
            .archetypes
            .keys()
            .map(String::as_str)
            .collect()
    } else {
        vec![archetype_name]
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

    if runnable.is_empty() {
        let name = if skipped.len() == 1 { skipped[0] } else { "archetype" };
        bail!(
            "no sessions configured for host '{hostname}': {}\n\n\
             Add session IDs to your .review.md frontmatter, e.g.:\n\
             ---\n\
             {name}:\n  \
               {hostname}:\n    \
                 claude: \"your-session-id\"\n\
             ---",
            skipped.join(", ")
        );
    }

    for name in &skipped {
        eprintln!("warning: skipping '{name}' (no sessions for host '{hostname}' in .review.md)");
    }

    // Assemble prompts and spawn all providers in parallel
    let mut handles: Vec<(String, tokio::task::JoinHandle<provider::ProviderResult>)> = Vec::new();

    let claude_available = provider::is_available("claude");
    let codex_available = provider::is_available("codex");

    if !claude_available {
        eprintln!("warning: 'claude' not found on PATH, skipping claude sessions");
    }
    if !codex_available {
        eprintln!("warning: 'codex' not found on PATH, skipping codex sessions");
    }

    for arch_name in &runnable {
        let assembled = prompt::assemble(&cfg, arch_name, &context, &stdin_instructions);
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
        bail!(
            "no providers available to run\n\n\
             Sessions are configured but the provider binaries are not on PATH.\n\
             Install 'claude' and/or 'codex' to proceed."
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

    Ok(())
}
