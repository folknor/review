mod audit;
mod cli;
mod config;
mod config_write;
mod input;
mod lock;
mod prime;
mod prompt;
mod provider;
mod sessions;

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

    if let Some(ref session_id) = cli.session {
        if cli.oneshot {
            bail!("--session and --oneshot are mutually exclusive");
        }
        if cli.anchor {
            bail!("--session and --anchor are mutually exclusive (resumed sessions already have grounding)");
        }
        let providers = cli.provider.as_ref().map(Vec::as_slice).unwrap_or(&[]);
        let provider_name = match providers {
            [one] => one.as_str(),
            [] => bail!("--session requires --provider <name> (single provider)"),
            _ => bail!("--session requires exactly one --provider, got {}", providers.len()),
        };
        return run_session_resume(archetype_name, provider_name, session_id, cli.dry_run).await;
    }

    let (mut cfg, project_root) = config::load()?;
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
            let prompt = if cli.oneshot {
                let prime = cfg.prime.get(*arch_name).map(String::as_str);
                prompt::assemble_oneshot(prime, &stdin_instructions)
            } else if cli.anchor {
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

    // Ensure audit ID exists — generate and persist if missing (after lock to prevent races)
    let audit_id = match cfg.audit.id {
        Some(ref id) => id.clone(),
        None => {
            let id = config::generate_short_id();
            let config_path = project_root.join(".review.toml");
            if let Err(e) = config_write::append_audit_id(&config_path, &id) {
                eprintln!("warning: failed to write audit id to .review.toml: {e}");
            }
            cfg.audit.id = Some(id.clone());
            id
        }
    };

    // Spawn all providers with staggered launches to avoid rate limits
    let stagger = std::time::Duration::from_secs(cli.stagger);
    struct PendingResult {
        archetype: String,
        session: String,
        prompt: String,
        operator_prompt: String,
        model: Option<String>,
        env_keys: Vec<String>,
        oneshot: bool,
        handle: tokio::task::JoinHandle<provider::ProviderResult>,
    }
    let mut pending: Vec<PendingResult> = Vec::new();
    let mut warned_unavailable = std::collections::HashSet::new();
    let mut launch_count = 0u32;

    for arch_name in &runnable {
        let assembled = if cli.oneshot {
            let prime = cfg.prime.get(*arch_name).map(String::as_str);
            prompt::assemble_oneshot(prime, &stdin_instructions)
        } else if cli.anchor {
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
            let sid = if cli.oneshot {
                String::new()
            } else {
                entry.session().to_string()
            };
            let model = entry.model().map(String::from);
            let env = entry.env().cloned();
            let env_keys: Vec<String> = env
                .as_ref()
                .map(|m| m.keys().cloned().collect())
                .unwrap_or_default();
            let aname = (*arch_name).to_string();
            let prompt = assembled.clone();
            let root = project_root.clone();
            let delay = stagger * launch_count;
            let oneshot = cli.oneshot;

            let session_for_audit = sid.clone();
            let prompt_for_audit = prompt.clone();
            let model_for_pending = model.clone();
            let operator_prompt = stdin_instructions.clone();
            pending.push(PendingResult {
                archetype: (*arch_name).to_string(),
                session: session_for_audit,
                prompt: prompt_for_audit,
                operator_prompt,
                model: model_for_pending,
                env_keys,
                oneshot,
                handle: tokio::spawn(async move {
                    if !delay.is_zero() {
                        tokio::time::sleep(delay).await;
                    }
                    provider::invoke(&prov, &sid, model.as_deref(), env.as_ref(), &aname, &prompt, &root, oneshot).await
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
                session_id: None,
            },
        };

        // Prefer the captured session ID (from --oneshot) over the placeholder
        // empty string we stored at launch time.
        let session_for_log = result.session_id.as_deref().unwrap_or(&p.session);
        audit::log_result(
            &project_root,
            cfg.audit.private,
            &audit_id,
            &p.archetype,
            &result.provider,
            session_for_log,
            &p.prompt,
            &result.output,
        );

        // Sidecar: record fresh sessions created by --oneshot. We rely on a
        // captured session ID, which only claude/codex provide today.
        if p.oneshot
            && let Some(ref sid) = result.session_id
        {
            sessions::record(
                &project_root,
                cfg.audit.private,
                &audit_id,
                &p.archetype,
                &result.provider,
                sid,
                "oneshot",
                p.model.as_deref(),
                p.env_keys.clone(),
                &p.operator_prompt,
                &p.prompt,
                &result.output,
            );
        }

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

/// `--session <id>` mode: resume a specific provider session and send raw
/// stdin. No `.review.toml` lookup, no PREFIX, no prime, no anchor — the
/// session already has its grounding from the original interaction. Single
/// provider, single archetype (the archetype is cosmetic context for audit).
async fn run_session_resume(
    archetype: &str,
    provider_name: &str,
    session_id: &str,
    dry_run: bool,
) -> Result<()> {
    if !config::KNOWN_PROVIDERS.contains(&provider_name) {
        bail!(
            "unknown provider '{provider_name}'\n  supported: {}",
            config::KNOWN_PROVIDERS.join(", ")
        );
    }

    let stdin_instructions = input::read_stdin()?;
    let project_root = config::load()
        .map(|(_, root)| root)
        .or_else(|_| std::env::current_dir().map_err(anyhow::Error::from))?;

    if dry_run {
        eprintln!("session: {session_id}");
        eprintln!("provider: {provider_name}");
        eprintln!("archetype: {archetype}");
        println!("{stdin_instructions}");
        return Ok(());
    }

    if !provider::is_available(provider_name) {
        bail!("'{provider_name}' not found on PATH");
    }

    // Global lock: serialize against other `review` invocations.
    let lock_path = std::env::temp_dir().join("review.lock");
    let lock_file = std::fs::File::create(&lock_path)
        .map_err(|e| anyhow::anyhow!("failed to create lock file: {e}"))?;
    lock::acquire_blocking(&lock_file)?;

    let result = provider::invoke(
        provider_name,
        session_id,
        None,
        None,
        archetype,
        &stdin_instructions,
        &project_root,
        false,
    )
    .await;

    drop(lock_file);

    // Audit log: load config opportunistically for audit_id/private settings;
    // tolerate the case where .review.toml is absent.
    if let Ok((mut cfg, root)) = config::load() {
        let audit_id = match cfg.audit.id {
            Some(ref id) => id.clone(),
            None => {
                let id = config::generate_short_id();
                let config_path = root.join(".review.toml");
                let _ = config_write::append_audit_id(&config_path, &id);
                cfg.audit.id = Some(id.clone());
                id
            }
        };
        audit::log_result(
            &root,
            cfg.audit.private,
            &audit_id,
            archetype,
            &result.provider,
            session_id,
            &stdin_instructions,
            &result.output,
        );
        sessions::record(
            &root,
            cfg.audit.private,
            &audit_id,
            archetype,
            &result.provider,
            session_id,
            "session",
            None,
            Vec::new(),
            &stdin_instructions,
            &stdin_instructions,
            &result.output,
        );
    }

    provider::print_result(&result);

    if result.output.is_err() {
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
    let (cfg, project_root) = config::load().or_else(|_| {
        // Config might not exist yet — that's fine for prime
        let cwd = std::env::current_dir()?;
        Ok::<_, anyhow::Error>((
            config::ReviewConfig {
                archetypes: std::collections::BTreeMap::new(),
                groups: std::collections::BTreeMap::new(),
                audit: config::AuditConfig::default(),
                prime: std::collections::BTreeMap::new(),
            },
            cwd,
        ))
    })?;

    let config_path = project_root.join(".review.toml");

    // Read stdin before taking the lock (can block on a slow pipe).
    let piped = input::read_stdin_optional()?;

    // Global lock: serialize against other `review` invocations so config
    // reads and writes stay consistent. Held through provider priming.
    let lock_path = std::env::temp_dir().join("review.lock");
    let lock_file = std::fs::File::create(&lock_path)
        .map_err(|e| anyhow::anyhow!("failed to create lock file: {e}"))?;
    lock::acquire_blocking(&lock_file)?;

    // Re-load config under the lock so we see any concurrent writer's changes.
    let cfg = match config::load() {
        Ok((c, _)) => c,
        Err(_) => cfg,
    };

    let stored = cfg.prime.get(archetype).cloned();
    if let Some(ref s) = stored
        && s.len() > input::MAX_STDIN_BYTES
    {
        bail!(
            "stored prime prompt for '{archetype}' exceeds {} byte limit (found {})",
            input::MAX_STDIN_BYTES,
            s.len()
        );
    }

    let (stdin_prompt, save_prompt) = match (piped, stored) {
        (Some(_), Some(_)) => bail!(
            "a prime prompt for '{archetype}' is already stored in .review.toml\n  \
             Remove [_prime].{archetype} manually if you want to replace it,\n  \
             or omit stdin to reuse the stored prompt."
        ),
        (Some(s), None) => (s, true),
        (None, Some(s)) => {
            eprintln!("Using stored prime prompt from .review.toml");
            (s, false)
        }
        (None, None) => bail!(
            "no instructions provided on stdin and no stored prompt for '{archetype}'\n\n\
             Pipe instructions via stdin, e.g.:\n  \
             echo \"you are a bugs expert\" | review prime {archetype} --provider claude"
        ),
    };

    let hostname = config::hostname();

    eprintln!("Priming archetype '{archetype}' for host '{hostname}'");
    eprintln!();

    let mut sessions: Vec<(String, String)> = Vec::new();

    let send_prompt = prompt::assemble_prime(&stdin_prompt);

    for provider in providers {
        let result = prime::prime_provider(provider, &send_prompt, &project_root).await;
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

    // Atomic single-pass write: sessions and (optionally) the stored prompt
    // land together, or not at all.
    config_write::write_prime_result(
        &config_path,
        archetype,
        &hostname,
        &sessions,
        if save_prompt { Some(stdin_prompt.as_str()) } else { None },
    )?;

    drop(lock_file);

    eprintln!();
    eprintln!("Added to .review.toml:");
    let host_key = config::toml_key(&hostname);
    eprintln!("  [{archetype}.{host_key}]");
    for (prov, sid) in &sessions {
        eprintln!("  {prov} = \"{sid}\"");
    }

    Ok(())
}
