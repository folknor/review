mod audit;
mod cli;
mod config;
mod config_write;
mod input;
mod lock;
mod prime;
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

    if let Some(cli::Command::Prime { archetype, provider }) = &cli.command {
        return run_prime(archetype, provider).await;
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

    // Provider filter from --provider flag
    let provider_filter: Option<Vec<&str>> = cli.provider.as_ref().map(|v| {
        v.iter().map(String::as_str).collect()
    });

    if cli.dry_run {
        eprintln!("config: {}", project_root.join(".review.toml").display());
        eprintln!("hostname: {hostname}");
    }

    // Resolve archetype(s) — supports "all", groups, comma-separated, or single names
    let names: Vec<&str> = archetype_name.split(',').collect();
    let mut archetypes_to_run: Vec<&str> = Vec::new();

    for name in &names {
        if *name == "all" {
            archetypes_to_run.extend(cfg.archetypes.keys().map(String::as_str));
        } else if let Some(group) = cfg.groups.get(*name) {
            archetypes_to_run.extend(group.iter().map(String::as_str));
        } else if cfg.archetypes.contains_key(*name) {
            archetypes_to_run.push(name);
        } else {
            let mut available: Vec<&str> = cfg.archetypes.keys().map(String::as_str).collect();
            let groups: Vec<&str> = cfg.groups.keys().map(String::as_str).collect();
            available.extend(groups);
            bail!(
                "'{name}' not found in .review.toml\n  \
                 configured: {}",
                if available.is_empty() {
                    "(none)".to_string()
                } else {
                    available.join(", ")
                }
            );
        }
    }

    // Deduplicate (e.g. "all" + a specific archetype, or overlapping groups)
    let mut seen = std::collections::HashSet::new();
    archetypes_to_run.retain(|name| seen.insert(*name));

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

    // Global lock
    let lock_path = std::env::temp_dir().join("review.lock");
    let lock_file = std::fs::File::create(&lock_path)
        .map_err(|e| anyhow::anyhow!("failed to create lock file: {e}"))?;
    lock::acquire_blocking(&lock_file)?;

    // Spawn all providers with staggered launches to avoid rate limits
    let stagger = std::time::Duration::from_secs(cli.stagger);
    struct PendingResult {
        archetype: String,
        session: String,
        prompt: String,
        handle: tokio::task::JoinHandle<provider::ProviderResult>,
    }
    let mut pending: Vec<PendingResult> = Vec::new();
    let mut warned_unavailable = std::collections::HashSet::new();
    let mut launch_count = 0u32;

    for arch_name in &runnable {
        let assembled = if cli.anchor {
            prompt::assemble(&stdin_instructions)
        } else {
            stdin_instructions.clone()
        };
        let arch_cfg = cfg.archetypes.get(*arch_name).expect("filtered above");
        let host_cfg = arch_cfg.resolve_host(&hostname).expect("filtered above");

        for (prov_name, entry) in &host_cfg.providers {
            // Skip if --provider filter is set and this provider isn't in it
            if let Some(ref filter) = provider_filter
                && !filter.contains(&prov_name.as_str())
            {
                continue;
            }

            if !provider::is_available(prov_name) {
                if warned_unavailable.insert(prov_name.clone()) {
                    eprintln!("warning: '{prov_name}' not found on PATH, skipping");
                }
                continue;
            }

            let prov = prov_name.clone();
            let sid = entry.session().to_string();
            let model = entry.model().map(String::from);
            let aname = (*arch_name).to_string();
            let prompt = assembled.clone();
            let root = project_root.clone();
            let delay = stagger * launch_count;

            let session_for_audit = sid.clone();
            let prompt_for_audit = prompt.clone();
            pending.push(PendingResult {
                archetype: (*arch_name).to_string(),
                session: session_for_audit,
                prompt: prompt_for_audit,
                handle: tokio::spawn(async move {
                    if !delay.is_zero() {
                        tokio::time::sleep(delay).await;
                    }
                    provider::invoke(&prov, &sid, model.as_deref(), &aname, &prompt, &root).await
                }),
            });
            launch_count += 1;
        }
    }

    // Wait one final stagger interval so the next invocation's first launch
    // doesn't collide with our last launch, then release the global lock.
    if launch_count > 0 && !stagger.is_zero() {
        tokio::time::sleep(stagger).await;
    }
    drop(lock_file);

    if pending.is_empty() {
        let msg = if let Some(ref filter) = provider_filter {
            format!(
                "no providers matched --provider {}\n  \
                 Check spelling and that the provider binary is on PATH.",
                filter.join(",")
            )
        } else {
            "no providers available to run\n  \
             Check that provider binaries are on PATH.".to_string()
        };
        bail!("{msg}");
    }

    // Collect results
    let mut results: Vec<(String, provider::ProviderResult)> = Vec::new();
    for p in pending {
        let result = match p.handle.await {
            Ok(r) => r,
            Err(err) => provider::ProviderResult {
                provider: "unknown".into(),
                output: Err(anyhow::anyhow!("task panicked: {err}")),
            },
        };

        audit::log_result(
            &project_root,
            cfg.audit.private,
            &p.archetype,
            &result.provider,
            &p.session,
            &p.prompt,
            &result.output,
        );

        results.push((p.archetype, result));
    }

    // Print results
    let multi = runnable.len() > 1;
    let mut current_arch = "";
    for (arch_name, result) in &results {
        if multi && arch_name.as_str() != current_arch {
            if !current_arch.is_empty() {
                println!();
            }
            println!("=== {arch_name} ===\n");
            current_arch = arch_name;
        }
        provider::print_result(result);
    }

    let all_failed = results.iter().all(|(_, r)| r.output.is_err());
    if all_failed {
        std::process::exit(1);
    }

    Ok(())
}

async fn run_prime(archetype: &str, providers: &[String]) -> Result<()> {
    // Validate provider names
    for p in providers {
        if !config::KNOWN_PROVIDERS.contains(&p.as_str()) {
            bail!(
                "unknown provider '{p}'\n  supported: {}",
                config::KNOWN_PROVIDERS.join(", ")
            );
        }
    }

    // Find .review.toml (or note that we'll create entries anyway)
    let (_, project_root) = config::load().or_else(|_| {
        // Config might not exist yet — that's fine for prime
        let cwd = std::env::current_dir()?;
        Ok::<_, anyhow::Error>((
            config::ReviewConfig {
                archetypes: std::collections::BTreeMap::new(),
                groups: std::collections::BTreeMap::new(),
                audit: config::AuditConfig::default(),
            },
            cwd,
        ))
    })?;

    let config_path = project_root.join(".review.toml");
    let stdin_prompt = input::read_stdin()?;
    let hostname = config::hostname();

    eprintln!("Priming archetype '{archetype}' for host '{hostname}'");
    eprintln!();

    let mut sessions: Vec<(String, String)> = Vec::new();

    for provider in providers {
        let result = prime::prime_provider(provider, &stdin_prompt, &project_root).await;
        match result {
            Ok(primed) => {
                eprintln!();
                sessions.push((primed.provider, primed.session_id));
            }
            Err(e) => {
                eprintln!("error priming {provider}: {e}");
            }
        }
    }

    if sessions.is_empty() {
        bail!("no sessions were created");
    }

    // Write to .review.toml
    config_write::append_sessions(&config_path, archetype, &hostname, &sessions)?;

    eprintln!();
    eprintln!("Added to .review.toml:");
    let host_key = config::toml_key(&hostname);
    eprintln!("  [{archetype}.{host_key}]");
    for (prov, sid) in &sessions {
        eprintln!("  {prov} = \"{sid}\"");
    }

    Ok(())
}
