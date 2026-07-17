mod audit;
mod cli;
mod config;
mod config_write;
mod incident;
mod input;
mod lock;
mod prompt;
mod provider;
mod sessions;
mod transcript;

use anyhow::{Result, bail};
use clap::Parser;

use cli::Cli;

/// Nudge sent when auto-resuming a codex run that died without a final answer.
const RESUME_NUDGE: &str = "The previous turn ended without a final answer. \
Continue from exactly where you left off and produce your complete final response now.";

/// A codex run that ended with no real final answer - no `-o` capture and
/// nothing recovered from the rollout. This is the death worth resuming past;
/// a run that captured or recovered an answer is not retried.
fn died_without_answer(r: &provider::ProviderResult) -> bool {
    match &r.digest {
        Some(d) => !d.captured && !d.recovered_from_transcript,
        None => false,
    }
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
            operator_prompt,
            prompt,
            &result.output,
            digest_summary.as_ref(),
        );
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

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
                    )
                    .await;
                    // The "work around" for codex mid-turn deaths: a run that
                    // ended with no real final answer gets one immediate resume
                    // (cache still warm) with a nudge, reusing the same profile.
                    // A manual resume is what rescued the original Death 2.
                    if prov == "codex"
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

    // Cache-age gate. The sidecar tells us how long it's been since the session
    // last ended; past ~55 minutes (the realistic cap on Anthropic's prompt
    // cache TTL - 5 min default, ~1h with the right env vars) the cache is cold,
    // and resuming means reprocessing the whole session prefix at full cost.
    // `--session` is the *warm* follow-up path, so a cold resume is refused: do
    // a fresh run with restated context instead. When there's no sidecar record
    // we can't determine age, so we proceed rather than block.
    const STALE_SESSION_SECS: u64 = 55 * 60;
    if let Some(record) = sessions::latest_for_session(session_id) {
        if let Some(age) = sessions::age_secs(&record) {
            if age > STALE_SESSION_SECS {
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

    // Global lock: serialize against other `review` invocations.
    let lock_path = std::env::temp_dir().join("review.lock");
    let lock_file = lock::open_lock_file(&lock_path)?;
    lock::acquire_blocking(&lock_file)?;

    let result = provider::invoke(
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
    )
    .await;

    drop(lock_file);

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
    // Only a *successful* resume refreshes the cold-cache clock. codex failures
    // come back as `Ok`-with-a-death-digest, so `output.is_ok()` alone would let
    // a dead resume mark the session warm for another ~55 minutes and defeat the
    // gate. Require that it produced a real final answer (claude has no digest,
    // so `died_without_answer` is false and `output.is_ok()` still governs it).
    let resume_succeeded = result.output.is_ok() && !died_without_answer(&result);
    if resume_succeeded {
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
            &stdin_instructions,
            &stdin_instructions,
            &result.output,
            digest_summary.as_ref(),
        );
    }

    provider::print_result(&result);

    if result.output.is_err() {
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
        let verdict = if m.recovered_from_transcript {
            "recovered from transcript"
        } else if m.final_answer_present == Some(false) {
            "no final answer (died)"
        } else if m.final_answer_present == Some(true) {
            "completed (stream/-o truncated)"
        } else {
            "suspicious"
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
    if d.recovered_from_transcript {
        println!("recovered: final answer restored from transcript");
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
