mod audit;
mod cli;
mod config;
mod config_write;
mod incident;
mod inflight;
mod input;
mod lock;
mod prompt;
mod provider;
mod sessions;
mod timings;
mod transcript;
mod watchdog;
mod writable_roots;

use anyhow::{Result, bail};
use clap::Parser;

use cli::Cli;

/// Nudge sent when auto-resuming a codex run that died without a final answer.
const RESUME_NUDGE: &str = "The previous turn ended without a final answer. \
Continue from exactly where you left off and produce your complete final response now.";

/// Format the wall-clock runtime printed when `review` returns: sub-minute runs
/// get a decimal second (short runs differ by fractions), longer ones roll up to
/// m/s then h/m/s. `sessions::format_age` is deliberately not reused - it floors
/// anything under a minute to "now", which is most of a resume's runtime.
fn format_runtime(d: std::time::Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        return format!("{:.1}s", d.as_secs_f64());
    }
    let (mins, rem_s) = (secs / 60, secs % 60);
    if mins < 60 {
        return format!("{mins}m{rem_s:02}s");
    }
    format!("{}h{:02}m{rem_s:02}s", mins / 60, mins % 60)
}

/// A codex run that ended with no real final answer - no `-o` capture and
/// nothing recovered from the rollout. This is the death worth resuming past;
/// a run that captured or recovered an answer is not retried.
fn died_without_answer(r: &provider::ProviderResult) -> bool {
    match &r.digest {
        Some(d) => !d.captured && !d.recovered_from_transcript,
        None => false,
    }
}

/// Codex's own stated reason for the turn ending (a stream `error`/`turn.failed`
/// event) - an upstream refusal, a rate limit, an auth failure.
///
/// This is the one death class auto-resume must *not* touch. Auto-resume exists
/// for the mid-turn wedge, where the work was real and interrupted, so replaying
/// it can finish the job. A stated failure is a verdict on the request itself:
/// resending it produces the identical failure, at full cost, and files a second
/// incident bundle that looks like a second death. Every observed refusal in this
/// workspace has shown up as exactly that - a pair of bundles seconds apart.
fn stated_failure(r: &provider::ProviderResult) -> Option<&str> {
    r.digest.as_ref()?.turn_error.as_deref()
}

/// A run that produced a real final answer (captured from `-o` or recovered from
/// the rollout). Used to decide whether an auto-resume actually helped.
fn got_final_answer(r: &provider::ProviderResult) -> bool {
    match &r.digest {
        Some(d) => d.captured || d.recovered_from_transcript,
        None => false,
    }
}

/// Write one provider run to the audit log and, if it captured a session, the
/// sidecar. Shared so an auto-resume can persist *both* of its invocations.
#[allow(clippy::too_many_arguments)]
fn record_run(
    project_root: &std::path::Path,
    private: bool,
    audit_id: &str,
    archetype: &str,
    model: Option<&str>,
    env_keys: &[String],
    operator_prompt: &str,
    prompt: &str,
    result: &provider::ProviderResult,
) {
    let session_for_log = result.session_id.as_deref().unwrap_or("");
    let digest_summary = result.digest.as_ref().map(provider::Digest::summary);
    audit::log_result(
        project_root,
        private,
        audit_id,
        archetype,
        &result.provider,
        session_for_log,
        prompt,
        &result.output,
        digest_summary.as_ref(),
    );
    if let Some(ref sid) = result.session_id {
        sessions::record(
            project_root,
            private,
            audit_id,
            archetype,
            &result.provider,
            sid,
            "run",
            result.completed_epoch,
            model,
            env_keys.to_vec(),
            result.sandbox.as_deref(),
            result.writable_roots.clone(),
            operator_prompt,
            prompt,
            &result.output,
            digest_summary.as_ref(),
        );
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // Total wall clock, started before anything else runs. This deliberately
    // includes the stdin read and the global-lock wait: it is what the operator
    // waited, not just what the providers spent.
    let started = std::time::Instant::now();
    let cli = Cli::parse();

    // Forward SIGINT/SIGTERM to any running codex process groups before exiting.
    // codex runs in its own process group (so we can kill it and its children as
    // a unit), which means it no longer receives a terminal Ctrl-C implicitly -
    // without this, killing `review` would leave codex running detached.
    // Installed once, process-wide, and before any provider can be spawned; see
    // `provider::install_signal_supervisor` for why it cannot be per-run.
    provider::install_signal_supervisor();

    if matches!(cli.command, Some(cli::Command::Init)) {
        return config::init();
    }

    if let Some(cli::Command::Sessions { id, all, limit }) = &cli.command {
        return match id {
            Some(sid) => run_session_show(sid),
            None => run_sessions(*all, *limit),
        };
    }

    if let Some(cli::Command::Incidents { limit }) = &cli.command {
        return run_incidents(*limit);
    }

    let archetype_name = match cli.archetype.as_deref() {
        Some(name) => name,
        None => {
            Cli::print_help();
            std::process::exit(2);
        }
    };

    if let Some(ref session_id) = cli.session {
        if cli.profile.is_some() {
            bail!(
                "--session and --profile are mutually exclusive: a resumed session \
                 bypasses .review.toml, so profile model/effort/sandbox/env overrides \
                 can't be applied"
            );
        }
        return run_session_resume(
            archetype_name,
            cli.provider.as_deref().unwrap_or(&[]),
            session_id,
            cli.dry_run,
            started,
        )
        .await;
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
    let lock_file = lock::open_lock_file(&lock_path)?;
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

    // Codex run settings from project config: today just the stall timeout,
    // which is tunable (and disableable) because it rests on an empirical codex
    // property rather than a documented contract.
    let codex_runtime = provider::CodexRuntime::from_config(cfg.defaults.stall_timeout_secs);

    // Spawn all providers with staggered launches to avoid rate limits
    let stagger = std::time::Duration::from_secs(cli.stagger);
    // A task yields its reportable `result` plus, when auto-resume ran, the
    // *other* invocation (`also`): the initial death when the resume rescued it,
    // or the failed resume when it didn't. Both are persisted so no provider
    // work or death is invisible to the audit/sidecar logs.
    struct TaskOutcome {
        result: provider::ProviderResult,
        also: Option<provider::ProviderResult>,
    }
    struct PendingResult {
        archetype: String,
        prompt: String,
        operator_prompt: String,
        model: Option<String>,
        env_keys: Vec<String>,
        handle: tokio::task::JoinHandle<TaskOutcome>,
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
            let config = profile.map(|p| p.config.clone()).unwrap_or_default();
            let env_keys: Vec<String> = env
                .as_ref()
                .map(|m| m.keys().cloned().collect())
                .unwrap_or_default();

            let prov = prov_name.clone();
            let prompt = assembled.clone();
            let root = project_root.clone();
            let runtime = codex_runtime.clone();
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
                    let first = provider::invoke(
                        &prov,
                        "",
                        model.as_deref(),
                        effort.as_deref(),
                        sandbox.as_deref(),
                        env.as_ref(),
                        &config,
                        &prompt,
                        &root,
                        true,
                        // The fan-out path serializes launches with its own
                        // stagger sleep, so it needs no launch handshake.
                        None,
                        &runtime,
                    )
                    .await;
                    // The "work around" for codex mid-turn deaths: a run that
                    // ended with no real final answer gets one immediate resume
                    // (cache still warm) with a nudge, reusing the same profile.
                    // A manual resume is what rescued the original Death 2.
                    // Only a run that produced nothing is a resume candidate at
                    // all; of those, one with a stated reason is skipped, because
                    // resending it just buys the same refusal twice (see
                    // `stated_failure`).
                    if let Some(why) = stated_failure(&first).filter(|_| died_without_answer(&first))
                    {
                        eprintln!("codex ended the turn without a final answer: {why}");
                        eprintln!(
                            "not auto-resuming - codex stated a reason, so a retry would hit it again"
                        );
                    } else if prov == "codex"
                        && died_without_answer(&first)
                        && let Some(sid) = first.session_id.clone()
                    {
                        eprintln!(
                            "codex session {sid} died without a final answer - auto-resuming once"
                        );
                        let second = provider::invoke(
                            &prov,
                            &sid,
                            model.as_deref(),
                            effort.as_deref(),
                            sandbox.as_deref(),
                            env.as_ref(),
                            &config,
                            RESUME_NUDGE,
                            &root,
                            false,
                            // Auto-resume runs inside an already-launched task.
                            None,
                            &runtime,
                        )
                        .await;
                        if got_final_answer(&second) {
                            let mut second = second;
                            if let Ok(text) = second.output.as_mut() {
                                *text = format!(
                                    "(auto-resumed after the initial run died without a final answer)\n\n{text}"
                                );
                            }
                            // Resume rescued it: report it, keep the death's record.
                            return TaskOutcome {
                                result: second,
                                also: Some(first),
                            };
                        }
                        // Resume didn't help: keep the death as the result, but
                        // still persist the resume's digest/incident.
                        return TaskOutcome {
                            result: first,
                            also: Some(second),
                        };
                    }
                    TaskOutcome {
                        result: first,
                        also: None,
                    }
                }),
            });
            launch_count += 1;
        }
    }

    // Hold the lock until every staggered launch has fired, plus one interval,
    // so a queued invocation can't start launching concurrently with our later
    // tasks. Task i launches at `stagger * i` (last at `stagger*(launch_count-1)`),
    // so we wait `stagger * launch_count` to cover the last launch and a trailing
    // gap before the next invocation's first launch.
    if launch_count > 0 && !stagger.is_zero() {
        tokio::time::sleep(stagger * launch_count).await;
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
        let outcome = match p.handle.await {
            Ok(o) => o,
            Err(err) => TaskOutcome {
                result: provider::ProviderResult {
                    provider: "unknown".into(),
                    output: Err(anyhow::anyhow!("task panicked: {err}")),
                    session_id: None,
                    digest: None,
                    completed_epoch: provider::now_epoch_secs(),
                    // A panicked task never reached a provider, so it launched
                    // with nothing; recording a sandbox here would attribute
                    // permissions to a run that never happened.
                    sandbox: None,
                    writable_roots: Vec::new(),
                },
                also: None,
            },
        };
        let TaskOutcome { result, also } = outcome;

        // Record the other invocation first (an auto-resume's initial death or
        // failed retry) so both are in the logs regardless of which we report.
        if let Some(ref also) = also {
            record_run(
                &project_root,
                cfg.audit.private,
                &audit_id,
                &p.archetype,
                p.model.as_deref(),
                &p.env_keys,
                &p.operator_prompt,
                &p.prompt,
                also,
            );
        }
        record_run(
            &project_root,
            cfg.audit.private,
            &audit_id,
            &p.archetype,
            p.model.as_deref(),
            &p.env_keys,
            &p.operator_prompt,
            &p.prompt,
            &result,
        );

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

    println!("\nruntime: {}", format_runtime(started.elapsed()));

    // A codex run that died without a final answer returns Ok (so its session
    // id + digest survive), but it is a failure for exit-code purposes - a dead
    // review that exits 0 lies to scripts and CI. Count it as failed.
    let all_failed = results
        .iter()
        .all(|(_, r)| r.output.is_err() || died_without_answer(r));
    if all_failed {
        std::process::exit(1);
    }

    Ok(())
}

/// Decide which provider a `--session` resume should talk to.
///
/// A session belongs to exactly one provider, and the sidecar already records
/// which. So `--provider` here is a *filter over a known answer*, not a
/// declaration - which means the old rule ("exactly one `--provider`, or
/// error") rejected two cases it had no reason to:
///
/// - **No `--provider` at all.** The answer is on record; asking the operator
///   to restate it is busywork, and getting it wrong is an error we could
///   simply not have.
/// - **A list that contains the right one.** `--provider claude,codex` against
///   a codex session is not ambiguous - only codex can resume it. This matters
///   because the same flags otherwise carry over verbatim from the fresh run
///   that created the session.
///
/// What remains an error is a genuine contradiction (a `--provider` the session
/// does not belong to) or genuine ambiguity (several named providers and no
/// record to choose between them).
fn resolve_session_provider(requested: &[String], recorded: Option<&str>) -> Result<String> {
    // `--provider codex,codex` names one provider, not two.
    let mut wanted: Vec<&str> = Vec::new();
    for p in requested {
        if !wanted.contains(&p.as_str()) {
            wanted.push(p.as_str());
        }
    }

    let chosen: String = match (wanted.as_slice(), recorded) {
        // Nothing requested: take the recorded owner.
        ([], Some(rec)) => rec.to_string(),
        ([], None) => bail!(
            "--session requires --provider <name>\n  \
             There is no sidecar record for this session, so the provider \
             can't be inferred."
        ),
        // One requested, and it disagrees with the record: a real conflict.
        // Resuming under the wrong provider cannot work, so refuse rather than
        // guess which of the two the operator meant.
        ([one], Some(rec)) if *one != rec => bail!(
            "this session belongs to '{rec}', but --provider says '{one}'\n  \
             A session can only be resumed by the provider that created it.\n  \
             Drop --provider to use '{rec}'."
        ),
        ([one], _) => (*one).to_string(),
        // Several requested and the record picks one out: not a conflict.
        (many, Some(rec)) if many.contains(&rec) => rec.to_string(),
        (many, Some(rec)) => bail!(
            "session belongs to '{rec}', which is not among --provider {}\n  \
             A session can only be resumed by the provider that created it.",
            many.join(",")
        ),
        (many, None) => bail!(
            "--session requires a single --provider, got {}\n  \
             There is no sidecar record for this session, so the right one \
             can't be inferred.",
            many.join(",")
        ),
    };

    if !config::KNOWN_PROVIDERS.contains(&chosen.as_str()) {
        bail!(
            "unknown provider '{chosen}'\n  supported: {}",
            config::KNOWN_PROVIDERS.join(", ")
        );
    }
    Ok(chosen)
}

/// `--session <id>` mode: resume a specific provider session and send raw
/// stdin. No `.review.toml` lookup, no prime - the session already has its
/// grounding from the original interaction. Single provider, single archetype
/// (the archetype is cosmetic context for audit).
async fn run_session_resume(
    archetype: &str,
    requested_providers: &[String],
    session_id: &str,
    dry_run: bool,
    started: std::time::Instant,
) -> Result<()> {
    // The sidecar knows which provider owns this session, so `--provider` is a
    // filter rather than a required declaration. Looked up once and reused by
    // the cache-age gate below.
    let record = sessions::latest_for_session(session_id);
    let provider_name = resolve_session_provider(
        requested_providers,
        record.as_ref().map(|r| r.provider.as_str()),
    )?;
    let provider_name = provider_name.as_str();
    if requested_providers.is_empty() {
        eprintln!("provider: {provider_name} (from the session record)");
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

    // `--session` bypasses .review.toml for prompt/profile purposes, but the
    // stall timeout is a safety setting rather than a prompt input, so it is
    // still honoured when a config happens to be present.
    let codex_runtime = provider::CodexRuntime::from_config(
        config::load()
            .ok()
            .and_then(|(cfg, _)| cfg.defaults.stall_timeout_secs),
    );

    // Cache-age gate. The sidecar tells us how long it's been since the session
    // last ended; past ~55 minutes (the realistic cap on Anthropic's prompt
    // cache TTL - 5 min default, ~1h with the right env vars) the cache is cold,
    // and resuming means reprocessing the whole session prefix at full cost.
    // `--session` is the *warm* follow-up path, so a cold resume is refused: do
    // a fresh run with restated context instead. When there's no sidecar record
    // we can't determine age, so we proceed rather than block.
    if let Some(ref record) = record {
        if let Some(age) = sessions::age_secs(record) {
            if age > timings::STALE_SESSION.as_secs() {
                bail!(
                    "session last touched {} ago - its prompt cache is cold.\n  \
                     Resuming would reprocess the whole session prefix at full cost.\n  \
                     Start a fresh run with restated context instead of `--session`.",
                    sessions::format_age(age)
                );
            }
            if age < 60 {
                eprintln!("session last touched just now");
            } else {
                eprintln!("session last touched {} ago", sessions::format_age(age));
            }
        }
    } else {
        eprintln!("note: no sidecar record for this session (age unknown)");
    }

    // Global lock: serialize the *launch* against other `review` invocations,
    // matching what the fan-out path does (it releases the lock once its
    // staggered launches have fired, not when the runs finish).
    //
    // It is deliberately NOT held across the run. The lock exists to space out
    // provider launches, and a resume launches exactly one thing, so there is
    // nothing left to serialize once it is running. Holding it for the turn's
    // duration meant a single wedged resume froze all `review` traffic on the
    // host indefinitely - every later invocation sat printing "Waiting for
    // another review to finish..." behind a process that would never return.
    // A hung run should cost you that run, not the tool.
    //
    // It *is* held across the spawn, via the launch handshake: releasing it
    // before calling `invoke` would leave a critical section guarding nothing,
    // letting every queued resume through to spawn simultaneously - the rate
    // limiting the lock exists to provide.
    let lock_path = std::env::temp_dir().join("review.lock");
    let lock_file = lock::open_lock_file(&lock_path)?;
    lock::acquire_blocking(&lock_file)?;

    let (launched_tx, launched_rx) = tokio::sync::oneshot::channel();
    let invoke = provider::invoke(
        provider_name,
        session_id,
        None,
        None,
        None,
        None,
        &[],
        &stdin_instructions,
        &project_root,
        false,
        Some(launched_tx),
        &codex_runtime,
    );
    tokio::pin!(invoke);

    // Release the lock as soon as the process is spawned. `launched_rx` also
    // resolves (as an error) if the sender is dropped without sending, i.e. the
    // spawn failed - which is equally a reason to stop holding the lock. The
    // `invoke` arm covers a provider that returns before signalling at all.
    let result = tokio::select! {
        result = &mut invoke => {
            drop(lock_file);
            result
        }
        _ = launched_rx => {
            drop(lock_file);
            invoke.await
        }
    };

    // Resolve audit_id/private for the logs. Load config opportunistically (and
    // persist a generated id when a config exists), but fall back so logging
    // works even without a .review.toml - otherwise a successful resume in a
    // config-less dir would never be recorded and would soon be refused as stale.
    let (audit_id, private) = match config::load() {
        Ok((mut cfg, root)) => {
            let id = match cfg.audit.id.take() {
                Some(id) => id,
                None => {
                    let id = config::generate_short_id();
                    let _ = config_write::append_audit_id(&root.join(".review.toml"), &id);
                    id
                }
            };
            (id, cfg.audit.private)
        }
        Err(_) => (config::generate_short_id(), false),
    };

    let digest_summary = result.digest.as_ref().map(provider::Digest::summary);
    audit::log_result(
        &project_root,
        private,
        &audit_id,
        archetype,
        &result.provider,
        session_id,
        &stdin_instructions,
        &result.output,
        digest_summary.as_ref(),
    );
    // Any resume that actually *ran* refreshes the cold-cache clock, because the
    // clock tracks prompt-cache warmth, not answer quality. Reprocessing the
    // session prefix warms the cache regardless of whether the turn produced a
    // real final answer - a codex mid-turn death (which comes back as
    // `Ok`-with-a-death-digest) warmed it just as much as a clean run. Gating on
    // "did we get a good answer" (the old `!died_without_answer` clause) meant a
    // dead intermediate resume left the clock pinned to the last *successful*
    // touch, so the next resume of a genuinely-warm session was wrongly refused
    // as stale. `output.is_ok()` is the honest proxy for "reached the provider
    // and got output back"; a hard launch failure (`Err`, cache never warmed)
    // still doesn't refresh.
    let resume_ran = result.output.is_ok();
    if resume_ran {
        sessions::record(
            &project_root,
            private,
            &audit_id,
            archetype,
            &result.provider,
            session_id,
            "session",
            result.completed_epoch,
            None,
            Vec::new(),
            // A `--session` resume bypasses `.review.toml` entirely and inherits
            // whatever the original run was launched with, so `review` has no
            // sandbox of its own to report here.
            result.sandbox.as_deref(),
            result.writable_roots.clone(),
            &stdin_instructions,
            &stdin_instructions,
            &result.output,
            digest_summary.as_ref(),
        );
    }

    provider::print_result(&result);
    println!("\nruntime: {}", format_runtime(started.elapsed()));

    // A stalled or dead resume comes back as `Ok` (so its session id, digest and
    // incident survive), but it is a failure for exit-code purposes - exiting 0
    // on a wedged run lies to scripts and CI, exactly as it did on the fan-out
    // path before this was fixed there.
    if result.output.is_err() || died_without_answer(&result) {
        std::process::exit(1);
    }
    Ok(())
}

/// `review incidents` - list recent forensic bundles (see `incident.rs`) so a
/// dead run is triageable at a glance instead of buried under the incidents dir.
fn run_incidents(limit: usize) -> Result<()> {
    let incidents = incident::list_recent(limit);
    if incidents.is_empty() {
        eprintln!("no incident bundles recorded (none of your codex runs have looked suspicious)");
        return Ok(());
    }
    for inc in &incidents {
        let m = &inc.meta;
        let exit = match m.exit_code {
            Some(c) => c.to_string(),
            None => "-".to_string(),
        };
        // A stated reason wins over every inferred verdict: these bundles are
        // explained, and listing them as "died" sends the reader hunting for a
        // cause codex already gave. Truncated so one refusal can't wrap the line.
        let stated = m
            .turn_error
            .as_deref()
            .map(|e| format!("turn failed: {}", first_line_truncated(e, 90)));
        let verdict = match &stated {
            Some(s) => s.as_str(),
            None if m.recovered_from_transcript => "recovered from transcript",
            None if m.final_answer_present == Some(false) => "no final answer (died)",
            None if m.final_answer_present == Some(true) => "completed (stream/-o truncated)",
            None => "suspicious",
        };
        let sig = m
            .signal
            .as_deref()
            .map(|s| format!(" signal={s}"))
            .unwrap_or_default();
        let ver = m
            .codex_version
            .as_deref()
            .map(|v| format!("  [{v}]"))
            .unwrap_or_default();
        let sid = m.session_id.as_deref().unwrap_or("no-session");
        println!(
            "{}  {} {sid}  exit={exit}{sig}  {verdict}{ver}",
            m.timestamp, m.provider
        );
        println!("       {}", inc.dir.display());
    }
    Ok(())
}

/// `review sessions <id>` - surface one session's artifacts: its digest, the
/// on-disk codex rollout transcript (the authoritative record), and the final
/// response. This is the on-demand form of the post-mortem that a suspicious
/// run prints inline - reachable for any past session by ID.
fn run_session_show(session_id: &str) -> Result<()> {
    let mut records: Vec<sessions::SessionRecord> = sessions::read_all()
        .into_iter()
        .filter(|r| r.session_id == session_id)
        .collect();
    if records.is_empty() {
        bail!(
            "no sidecar record for session {session_id}\n  \
             (run `review sessions` to list known sessions)"
        );
    }
    records.sort_by_key(|r| r.epoch_secs);
    let latest = records.last().expect("non-empty");

    let now = provider::now_epoch_secs();
    let age = if latest.epoch_secs == 0 {
        "?".to_string()
    } else {
        sessions::format_age(now.saturating_sub(latest.epoch_secs))
    };

    println!("session: {session_id}");
    println!(
        "provider: {} / archetype: {} ({}) / {} touch(es), last {age} ago",
        latest.provider,
        latest.archetype,
        latest.kind,
        records.len()
    );
    println!("project: {}", latest.project);
    if let Some(ref model) = latest.model {
        println!("model: {model}");
    }

    if let Some(ref d) = latest.digest {
        print_digest_summary(d);
    }

    // The on-disk rollout is the authoritative artifact. We recorded only env
    // var *names*, so a run that overrode CODEX_HOME can't be resolved here;
    // fall back to the default home and report a miss plainly.
    let transcript = if latest.provider == "codex" {
        transcript::summarize_session(session_id, None, None)
    } else {
        None
    };
    if latest.provider == "codex" {
        match transcript {
            Some(ref t) => {
                println!("transcript: {}", t.path);
                println!(
                    "  task_complete={} stream_error={}",
                    t.task_complete, t.stream_error
                );
                if let Some(ref last) = t.last_event {
                    println!("  last_event: {last}");
                }
                if let Some((ref name, ref args)) = t.last_in_flight_tool {
                    let shown: String = args.chars().take(200).collect();
                    println!("  last_in_flight_tool: {name} {shown}");
                }
            }
            None => println!("transcript: (not found under default CODEX_HOME)"),
        }
    }

    // Prefer the transcript's authoritative final_answer when it exists: it
    // recovers the real report even for rows recorded before runtime recovery
    // landed (whose sidecar `response` may be a truncated interim note).
    let recovered = transcript.as_ref().and_then(|t| t.final_answer.as_deref());
    // A codex rollout that reached task_complete but carries no final_answer
    // produced no conclusion - the turn died mid-work (task_complete fires on
    // aborted turns too). Flag it even for old rows that predate digest
    // persistence and so have no stored exit code.
    if transcript
        .as_ref()
        .is_some_and(|t| t.final_answer.is_none() && t.task_complete)
    {
        println!(
            "note: rollout has no final answer - the turn produced no conclusion (likely died mid-work)"
        );
    }
    println!("--- response ---");
    match (recovered, &latest.response, &latest.error) {
        (Some(fa), sidecar, _) => {
            if sidecar.as_deref() != Some(fa) {
                println!("(recovered final answer from transcript)");
            }
            println!("{fa}");
        }
        (None, Some(r), _) => println!("{r}"),
        (None, None, Some(e)) => println!("(no response) error: {e}"),
        (None, None, None) => println!("(no response recorded)"),
    }
    Ok(())
}

/// Print a persisted `DigestSummary` (from the sidecar) in the same shape as the
/// inline post-mortem, so `review sessions <id>` reads like a live run.
fn print_digest_summary(d: &provider::DigestSummary) {
    match d.exit_code {
        Some(code) => println!("exit: {code}"),
        None => println!("exit: -"),
    }
    if let Some(ref sig) = d.signal {
        println!("signal: {sig}");
    }
    println!("captured: {}", d.captured);
    if let Some(ref msg) = d.turn_error {
        println!("turn failed: {msg}");
    }
    if d.recovered_from_transcript {
        println!("recovered: final answer restored from transcript");
    } else if d.turn_error.is_some() {
        // Explained above - don't also guess "died mid-turn" at it.
        println!("note: no conclusion was produced");
    } else if !d.captured && (d.exit_code != Some(0) || d.signal.is_some()) {
        // No real final answer obtained and the process failed: a mid-turn
        // death. task_complete alone doesn't refute this - it fires on aborts.
        println!("note: no final answer captured - run likely died mid-turn");
    }
    if let Some(false) = d.task_complete {
        println!("task_complete: false");
    }
    if let Some(true) = d.stream_error {
        println!("stream_error: true");
    }
    println!("turns: {}", d.turns);
    println!(
        "usage: input={} cached={} output={} reasoning={}",
        d.input_tokens, d.cached_input_tokens, d.output_tokens, d.reasoning_output_tokens
    );
    if let Some(ref path) = d.incident_path {
        println!("incident: {path}");
    }
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

    // Runs that are happening *right now*. The sidecar is only written when a
    // run returns, so without this a session with a turn in flight is
    // indistinguishable from an idle one - it shows its previous response and a
    // touch count that has not moved. During the codex hang that motivated the
    // watchdog, that ambiguity is what made a wedged 10-hour run look like
    // nothing was happening at all.
    let mut live = inflight::read_live();
    if let Some(ref proj) = project_filter {
        live.retain(|m| &m.project == proj);
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if !live.is_empty() {
        live.sort_by_key(|m| m.started_epoch);
        for m in &live {
            let since = sessions::format_age(now.saturating_sub(m.started_epoch));
            println!("[in flight] {} / turn in flight since {since}", m.provider);
            println!("       session: {}", m.session_id);
            if all {
                println!("       project: {}", m.project);
            }
            println!();
        }
    }

    let mut records = sessions::read_all();
    if let Some(ref proj) = project_filter {
        records.retain(|r| &r.project == proj);
    }

    if records.is_empty() {
        if live.is_empty() {
            if project_filter.is_some() {
                eprintln!("no sessions recorded for this project (try --all)");
            } else {
                eprintln!("no sessions recorded");
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn req(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn infers_the_provider_from_the_session_record() {
        // The common case: `review goal --session <id>` with no --provider at
        // all. The answer is on record, so requiring it was busywork.
        let got = resolve_session_provider(&[], Some("codex")).expect("inferred");
        assert_eq!(got, "codex");
    }

    #[test]
    fn a_list_containing_the_owner_is_not_a_conflict() {
        // The reason this matters: --provider claude,codex carries over verbatim
        // from the fresh run that created the session. Only codex can resume a
        // codex session, so there is nothing ambiguous to reject.
        let got = resolve_session_provider(&req(&["claude", "codex"]), Some("codex"))
            .expect("resolved to the owner");
        assert_eq!(got, "codex");
    }

    #[test]
    fn a_single_matching_provider_is_accepted() {
        let got = resolve_session_provider(&req(&["codex"]), Some("codex")).expect("accepted");
        assert_eq!(got, "codex");
    }

    #[test]
    fn repeats_collapse_to_one_provider() {
        let got = resolve_session_provider(&req(&["codex", "codex"]), None).expect("accepted");
        assert_eq!(got, "codex");
    }

    #[test]
    fn a_contradicting_provider_is_refused() {
        // Resuming under the wrong provider cannot work, so this stays an error
        // rather than being silently corrected to the recorded owner.
        let err = resolve_session_provider(&req(&["claude"]), Some("codex"))
            .expect_err("must refuse a provider the session does not belong to");
        let msg = err.to_string();
        assert!(msg.contains("codex"), "names the owner: {msg}");
        assert!(msg.contains("claude"), "names what was asked for: {msg}");
    }

    #[test]
    fn a_list_missing_the_owner_is_refused() {
        // Deliberately more than one name, so this exercises the multi-provider
        // arm rather than collapsing into the single-provider contradiction
        // case above.
        let err = resolve_session_provider(&req(&["claude", "opencode"]), Some("codex"))
            .expect_err("must refuse when the owner is absent from the list");
        let msg = err.to_string();
        assert!(msg.contains("codex"), "names the owner: {msg}");
        assert!(msg.contains("claude,opencode"), "names the list: {msg}");
    }

    #[test]
    fn ambiguity_without_a_record_is_refused() {
        // No record to choose between them, so this is genuinely ambiguous.
        let err = resolve_session_provider(&req(&["claude", "codex"]), None)
            .expect_err("must refuse genuine ambiguity");
        assert!(err.to_string().contains("single --provider"));
    }

    #[test]
    fn no_provider_and_no_record_is_refused() {
        let err = resolve_session_provider(&[], None)
            .expect_err("nothing to infer from and nothing requested");
        assert!(err.to_string().contains("--provider"));
    }

    #[test]
    fn an_unknown_provider_is_refused() {
        let err = resolve_session_provider(&req(&["kilo"]), None).expect_err("unknown provider");
        assert!(err.to_string().contains("supported"));
    }

    #[test]
    fn an_unknown_recorded_provider_is_refused() {
        // A sidecar row naming a provider we no longer support (kilo/opencode
        // were removed) must not be inferred into an unrunnable invocation.
        let err = resolve_session_provider(&[], Some("opencode"))
            .expect_err("recorded provider must still be validated");
        assert!(err.to_string().contains("supported"));
    }
}
