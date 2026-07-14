mod audit;
mod cli;
mod config;
mod config_write;
mod input;
mod lock;
mod prompt;
mod provider;
mod sessions;
mod transcript;

use anyhow::{Result, bail};
use clap::Parser;

use cli::Cli;

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    if matches!(cli.command, Some(cli::Command::Init)) {
        return config::init();
    }

    if let Some(cli::Command::Sessions { all, limit }) = &cli.command {
        return run_sessions(*all, *limit);
    }

    let archetype_name = match cli.archetype.as_deref() {
        Some(name) => name,
        None => {
            Cli::print_help();
            std::process::exit(2);
        }
    };

    if let Some(ref session_id) = cli.session {
        let providers = cli.provider.as_deref().unwrap_or(&[]);
        let provider_name = match providers {
            [one] => one.as_str(),
            [] => bail!("--session requires --provider <name> (single provider)"),
            _ => bail!(
                "--session requires exactly one --provider, got {}",
                providers.len()
            ),
        };
        return run_session_resume(archetype_name, provider_name, session_id, cli.dry_run).await;
    }

    let (mut cfg, project_root) = config::load()?;
    let stdin_instructions = input::read_stdin()?;

    let hostname = config::hostname();

    if cli.dry_run {
        eprintln!("config: {}", project_root.join(".review.toml").display());
        eprintln!("hostname: {hostname}");
    }

    // Resolve archetype(s) - supports "all", groups, comma-separated, or single names
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
            available.extend(cfg.groups.keys().map(String::as_str));
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

    if archetypes_to_run.is_empty() {
        bail!(
            "no archetypes configured in .review.toml\n\n\
             Add archetypes, e.g.:\n\n\
             [archetypes]\n\
             security = \"You are a security expert. Read the codebase.\""
        );
    }

    // Provider list: --provider wins, otherwise [_defaults].providers.
    let providers_to_run: Vec<String> = match cli.provider.as_ref().filter(|v| !v.is_empty()) {
        Some(v) => v.clone(),
        None => {
            if cfg.defaults.providers.is_empty() {
                bail!(
                    "no providers to run: pass --provider <name> or set [_defaults].providers in .review.toml"
                );
            }
            cfg.defaults.providers.clone()
        }
    };

    // If a profile was requested, every launched provider must define it under
    // [<host>.<provider>.<profile>]. Validate up front so we fail before spawning.
    if let Some(ref profile) = cli.profile {
        for prov in &providers_to_run {
            if cfg.resolve_profile(&hostname, prov, profile).is_none() {
                bail!(
                    "profile '{profile}' not defined for provider '{prov}' on host '{hostname}'\n  \
                     add [{}.{prov}.{profile}] to .review.toml",
                    config::toml_key(&hostname)
                );
            }
        }
    }

    // Dry run: print what would be sent and exit
    if cli.dry_run {
        for arch_name in &archetypes_to_run {
            let prime = &cfg.archetypes[*arch_name];
            let prompt = prompt::assemble(prime, &stdin_instructions);
            if archetypes_to_run.len() > 1 {
                println!("=== {arch_name} ===\n");
            }
            println!("{prompt}");
            if archetypes_to_run.len() > 1 {
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

    // Ensure audit ID exists - generate and persist if missing (after lock to prevent races)
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
        prompt: String,
        operator_prompt: String,
        model: Option<String>,
        env_keys: Vec<String>,
        handle: tokio::task::JoinHandle<provider::ProviderResult>,
    }
    let mut pending: Vec<PendingResult> = Vec::new();
    let mut warned_unavailable = std::collections::HashSet::new();
    let mut launch_count = 0u32;

    for arch_name in &archetypes_to_run {
        let prime = &cfg.archetypes[*arch_name];
        let assembled = prompt::assemble(prime, &stdin_instructions);

        for prov_name in &providers_to_run {
            if !provider::is_available(prov_name) {
                if warned_unavailable.insert(prov_name.clone()) {
                    eprintln!("warning: '{prov_name}' not found on PATH, skipping");
                }
                continue;
            }

            // Profile overrides (validated above to exist when --profile is set).
            let profile = cli
                .profile
                .as_ref()
                .and_then(|name| cfg.resolve_profile(&hostname, prov_name, name));
            let model = profile.and_then(|p| p.model.clone());
            let effort = profile.and_then(|p| p.effort.clone());
            let sandbox = profile.and_then(|p| p.sandbox.clone());
            let env = profile.and_then(|p| p.env.clone());
            let env_keys: Vec<String> = env
                .as_ref()
                .map(|m| m.keys().cloned().collect())
                .unwrap_or_default();

            let prov = prov_name.clone();
            let aname = (*arch_name).to_string();
            let prompt = assembled.clone();
            let root = project_root.clone();
            let delay = stagger * launch_count;

            let prompt_for_audit = prompt.clone();
            let model_for_pending = model.clone();
            let operator_prompt = stdin_instructions.clone();
            pending.push(PendingResult {
                archetype: (*arch_name).to_string(),
                prompt: prompt_for_audit,
                operator_prompt,
                model: model_for_pending,
                env_keys,
                handle: tokio::spawn(async move {
                    if !delay.is_zero() {
                        tokio::time::sleep(delay).await;
                    }
                    provider::invoke(
                        &prov,
                        "",
                        model.as_deref(),
                        effort.as_deref(),
                        sandbox.as_deref(),
                        env.as_ref(),
                        &aname,
                        &prompt,
                        &root,
                        true,
                    )
                    .await
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
        bail!(
            "no providers available to run\n  \
             Check that provider binaries are on PATH."
        );
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
                digest: None,
            },
        };

        // Audit log always records the run; the session_id is present for
        // claude/codex (both capture it).
        let session_for_log = result.session_id.as_deref().unwrap_or("");
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

        // Sidecar: record the fresh session so it can be followed up via
        // --session. Only claude/codex capture a session ID today.
        if let Some(ref sid) = result.session_id {
            sessions::record(
                &project_root,
                cfg.audit.private,
                &audit_id,
                &p.archetype,
                &result.provider,
                sid,
                "run",
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
    let multi = archetypes_to_run.len() > 1;
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
/// stdin. No `.review.toml` lookup, no prime - the session already has its
/// grounding from the original interaction. Single provider, single archetype
/// (the archetype is cosmetic context for audit).
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

    // Cache-age advisory. The sidecar lets us tell the operator how long it's
    // been since the session was last touched; >55 minutes is roughly the cap
    // on Anthropic's prompt cache TTL (5 min default, ~1h with the right env
    // vars), so anything older is almost certainly a cold-cache resume.
    if let Some(record) = sessions::latest_for_session(session_id) {
        if let Some(age) = sessions::age_secs(&record) {
            if age < 60 {
                eprintln!("session last touched just now");
            } else {
                eprintln!("session last touched {} ago", sessions::format_age(age));
            }
            if age > 55 * 60 {
                eprintln!(
                    "  cache is likely cold - a fresh run with restated context may be cheaper"
                );
            }
        }
    } else {
        eprintln!("note: no sidecar record for this session");
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

/// `review sessions` - aggregate sidecar records by session_id and print
/// recent sessions with their age, provider, archetype, touch count, and the
/// prompt that opened them. Filtered to the current project unless `--all`.
fn run_sessions(all: bool, limit: usize) -> Result<()> {
    let project_filter: Option<String> = if all {
        None
    } else {
        let root = config::load()
            .map(|(_, root)| root)
            .or_else(|_| std::env::current_dir().map_err(anyhow::Error::from))?;
        Some(root.to_string_lossy().into_owned())
    };

    let mut records = sessions::read_all();
    if let Some(ref proj) = project_filter {
        records.retain(|r| &r.project == proj);
    }

    if records.is_empty() {
        if project_filter.is_some() {
            eprintln!("no sessions recorded for this project (try --all)");
        } else {
            eprintln!("no sessions recorded");
        }
        return Ok(());
    }

    // Group by session_id.
    let mut groups: std::collections::HashMap<String, Vec<sessions::SessionRecord>> =
        std::collections::HashMap::new();
    for rec in records {
        groups.entry(rec.session_id.clone()).or_default().push(rec);
    }

    struct Row {
        latest_secs: u64,
        opener: sessions::SessionRecord,
        latest: sessions::SessionRecord,
        touches: usize,
    }

    let mut rows: Vec<Row> = groups
        .into_values()
        .map(|mut entries| {
            entries.sort_by_key(|r| r.epoch_secs);
            let touches = entries.len();
            // Chronologically first entry is the opener (the fresh run that
            // created the session); a session touch in pathological cases where
            // the creation row is missing.
            let opener = entries[0].clone();
            let latest = entries.last().cloned().expect("non-empty group");
            Row {
                latest_secs: latest.epoch_secs,
                opener,
                latest,
                touches,
            }
        })
        .collect();

    rows.sort_by_key(|r| std::cmp::Reverse(r.latest_secs));
    rows.truncate(limit);

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    for row in &rows {
        let age = if row.latest_secs == 0 {
            "?".to_string()
        } else {
            sessions::format_age(now.saturating_sub(row.latest_secs))
        };
        let touches_label = if row.touches == 1 { "touch" } else { "touches" };
        println!(
            "[{age}] {} / {} ({}) / {} {touches_label}",
            row.opener.provider, row.opener.archetype, row.opener.kind, row.touches
        );
        println!("       session: {}", row.latest.session_id);
        let opened = first_line_truncated(&row.opener.operator_prompt, 80);
        println!("       opened:  {opened}");
        if all {
            println!("       project: {}", row.opener.project);
        }
        println!();
    }

    Ok(())
}

fn first_line_truncated(s: &str, max_chars: usize) -> String {
    let line = s.lines().next().unwrap_or("");
    let count = line.chars().count();
    if count <= max_chars {
        return line.to_string();
    }
    let mut out: String = line.chars().take(max_chars).collect();
    out.push('…');
    out
}
