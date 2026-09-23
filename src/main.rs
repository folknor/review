mod audit;
mod cli;
mod config;
mod config_cmd;
mod config_write;
mod grok_home;
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

/// The archetype label recorded for a run launched without one: stdin is sent
/// unchanged, with no priming prompt.
const BARE: &str = "bare";

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
            // From the result, not from the caller's profile: the recorded
            // model must be the one the run launched with, for the same reason
            // the sandbox level is - a resume inherits this row, and inheriting
            // a request rather than an outcome is what this fix is about.
            result.model.as_deref(),
            result.effort.as_deref(),
            env_keys.to_vec(),
            result.sandbox.as_deref(),
            result.writable_roots.clone(),
            &result.served,
            result.grok_trust.as_deref(),
            operator_prompt,
            prompt,
            &result.output,
            digest_summary.as_ref(),
        );
    }
}

/// The archetype selection: `-a`, or the positional form it replaced.
///
/// The archetype used to be the positional argument, which put archetypes and
/// subcommands in one namespace - every new subcommand silently shadowed any
/// project's archetype of that name. As a flag it cannot collide. The positional
/// form is still accepted, with a warning, so orchestration procedures written
/// against it keep working until they are updated; a positional `bare` (the old
/// way to say "no archetype") means none.
fn resolve_archetype_arg(flag: Option<&str>, legacy: Option<&str>) -> Result<Option<String>> {
    match (flag, legacy) {
        (Some(_), Some(positional)) => {
            bail!("archetype given twice: -a and a positional '{positional}'\n  Use -a only.")
        }
        (Some(a), None) => Ok(Some(a.to_string())),
        (None, Some("bare")) => {
            eprintln!("warning: `review bare` is now plain `review` - no -a means no archetype");
            Ok(None)
        }
        // The name is not echoed back: this runs before the config is loaded,
        // so a typo would be recommended verbatim (`use review -a secruity`).
        // The unknown-archetype error that follows lists what is configured.
        (None, Some(positional)) => {
            eprintln!("warning: the archetype is now a flag - use `review -a <name>`");
            Ok(Some(positional.to_string()))
        }
        (None, None) => Ok(None),
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

    if matches!(cli.command, Some(cli::Command::Config)) {
        return config_cmd::run();
    }

    if let Some(cli::Command::Resume { id, dry_run }) = &cli.command {
        return run_session_resume(id, *dry_run, started).await;
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

    // Migration: `--session <ID>` is the old spelling of `review resume <ID>`.
    // Everything that rode along with it is ignored rather than refused, so a
    // procedure written against the old form keeps working until it is updated.
    if let Some(ref session_id) = cli.session {
        eprintln!("warning: `--session <ID>` is now `review resume <ID>`");
        if cli.profile.is_some() || cli.provider.is_some() {
            eprintln!(
                "warning: --profile/--provider are ignored on a resume - the session \
                 keeps its own provider and settings"
            );
        }
        return run_session_resume(session_id, cli.dry_run, started).await;
    }

    let archetype_arg =
        resolve_archetype_arg(cli.archetype.as_deref(), cli.legacy_archetype.as_deref())?;
    let archetype_arg = archetype_arg.as_deref();

    // A bare `review` - no arguments at all and nothing piped in - gets the help
    // text. Read before the config so this works outside a project too. With any
    // flag given the operator meant to run something, so a missing stdin falls
    // through to `read_stdin`'s error, which says what is missing, rather than
    // answering with help that silently ignores the flags.
    let early_stdin = if archetype_arg.is_none() && std::env::args_os().len() == 1 {
        match input::read_stdin_optional()? {
            Some(s) => Some(s),
            None => {
                Cli::print_help();
                std::process::exit(2);
            }
        }
    } else {
        None
    };

    let (mut cfg, project_root) = config::load()?;
    let stdin_instructions = match early_stdin {
        Some(s) => s,
        None => input::read_stdin()?,
    };

    if cli.dry_run {
        eprintln!("config: {}", cfg.searched());
        eprintln!("hostname: {}", cfg.hostname);
    }

    // Resolve archetype(s) - supports "all", groups, comma-separated, or single
    // names - into (label, prime) pairs. No archetype at all is one bare run.
    let mut archetypes_to_run: Vec<(String, String)> = Vec::new();
    match archetype_arg {
        None => archetypes_to_run.push((BARE.to_string(), String::new())),
        Some(arg) => {
            for name in arg.split(',') {
                if name == "all" {
                    archetypes_to_run.extend(
                        cfg.archetypes
                            .iter()
                            .map(|(n, p)| (n.clone(), p.value.clone())),
                    );
                } else if let Some(group) = cfg.groups.get(name) {
                    for member in &group.value {
                        // Resolution checked every member exists.
                        let prime = cfg.archetype(member).unwrap_or_default();
                        archetypes_to_run.push((member.clone(), prime.to_string()));
                    }
                } else if let Some(prime) = cfg.archetype(name) {
                    archetypes_to_run.push((name.to_string(), prime.to_string()));
                } else {
                    let mut available: Vec<&str> =
                        cfg.archetypes.keys().map(String::as_str).collect();
                    available.extend(cfg.groups.keys().map(String::as_str));
                    bail!(
                        "'{name}' not found in {}\n  \
                         configured: {}",
                        cfg.searched(),
                        if available.is_empty() {
                            "(none)".to_string()
                        } else {
                            available.join(", ")
                        }
                    );
                }
            }
        }
    }

    // Deduplicate (e.g. "all" + a specific archetype, or overlapping groups)
    let mut seen = std::collections::HashSet::new();
    archetypes_to_run.retain(|(name, _)| seen.insert(name.clone()));

    if archetypes_to_run.is_empty() {
        bail!(
            "no archetypes configured in {}\n  \
             Omit the archetype to send stdin unchanged, or add one under [archetypes].",
            cfg.searched()
        );
    }

    // Provider list: --provider wins, otherwise [_defaults].providers from the
    // first layer that sets it.
    let providers_to_run: Vec<String> = match cli.provider.as_ref().filter(|v| !v.is_empty()) {
        Some(v) => v.clone(),
        None => match cfg.default_providers() {
            Some(p) if !p.is_empty() => p.to_vec(),
            _ => bail!(
                "no providers to run: pass --provider <name> or set [_defaults].providers \
                 (looked in {})",
                cfg.searched()
            ),
        },
    };

    for prov in &providers_to_run {
        if !config::KNOWN_PROVIDERS.contains(&prov.as_str()) {
            bail!(
                "unknown provider '{prov}'\n  supported: {}",
                config::KNOWN_PROVIDERS.join(", ")
            );
        }
    }

    // If a profile was requested, every launched provider must have it in some
    // layer. Validate up front so we fail before spawning.
    if let Some(ref profile) = cli.profile {
        for prov in &providers_to_run {
            if cfg.resolve_profile(prov, profile).is_none() {
                bail!(
                    "profile '{profile}' not defined for provider '{prov}'\n  \
                     looked for [{prov}.{profile}] and [{}.{prov}.{profile}] in {}",
                    config::toml_key(&cfg.hostname),
                    cfg.searched()
                );
            }
        }
    }

    let missing: Vec<&str> = providers_to_run
        .iter()
        .map(String::as_str)
        .filter(|p| !provider::is_available(p))
        .collect();

    // Dry run: print what would be sent and exit
    if cli.dry_run {
        if !missing.is_empty() {
            eprintln!(
                "warning: not installed on this host: {} - a real run would fail",
                missing.join(", ")
            );
        }
        for (arch_name, prime) in &archetypes_to_run {
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

    // Every provider must be installed here, checked before anything launches.
    // With host-scoped config gone, one config reaches every machine, including
    // ones missing a harness; skipping it would quietly run a narrower fan-out
    // than was asked for, so it fails instead. A dry run launches nothing, so it
    // only warns.
    if !missing.is_empty() {
        bail!(
            "not installed on this host (not found on PATH): {}\n  \
             Nothing was launched. Install it, or choose providers with --provider.",
            missing.join(", ")
        );
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
    let codex_runtime = provider::CodexRuntime::from_config(cfg.stall_timeout_secs());

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
        env_keys: Vec<String>,
        handle: tokio::task::JoinHandle<TaskOutcome>,
    }
    let mut pending: Vec<PendingResult> = Vec::new();
    let mut launch_count = 0u32;

    for (arch_name, prime) in &archetypes_to_run {
        let assembled = prompt::assemble(prime, &stdin_instructions);

        for prov_name in &providers_to_run {
            // Profile overrides (validated above to exist when --profile is set).
            let profile = cli
                .profile
                .as_ref()
                .and_then(|name| cfg.resolve_profile(prov_name, name));
            let model = profile.and_then(|p| p.model.clone());
            let effort = profile.and_then(|p| p.effort.clone());
            let sandbox = profile.and_then(|p| p.sandbox.clone());
            let env = profile.and_then(|p| p.env.clone());
            let config = profile.map(|p| p.config.clone()).unwrap_or_default();
            let profile_roots = profile
                .map(|p| p.writable_roots.clone())
                .unwrap_or_default();
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
            let operator_prompt = stdin_instructions.clone();
            pending.push(PendingResult {
                archetype: arch_name.clone(),
                prompt: prompt_for_audit,
                operator_prompt,
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
                        &profile_roots,
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
                        eprintln!("{prov} ended the turn without a final answer: {why}");
                        eprintln!(
                            "not auto-resuming - {prov} stated a reason, so a retry would hit it again"
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
                            // An auto-resume reuses the dead run's profile, so
                            // it must reuse its roots too or the retry launches
                            // narrower than the run it is replacing.
                            &profile_roots,
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
                    model: None,
                    effort: None,
                    served: provider::Served::default(),
                    grok_trust: None,
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

/// The provider a `review resume` talks to: the one the sidecar recorded for the
/// session.
///
/// A session belongs to exactly one provider, and the only place an operator
/// gets a session ID from is `review`'s own output, whose run wrote that record.
/// So there is nothing to ask for: no `--provider`, and no scanning provider
/// session stores. No record means the session is not one this host's `review`
/// created - another host's (whose provider store is not here either), one made
/// outside `review`, or a lost log - and that is an error, not a guess.
///
/// A recorded provider is still validated, so a row naming a removed provider
/// (kilo/opencode) cannot turn into an unrunnable invocation.
fn resume_record(
    session_id: &str,
    record: Option<sessions::SessionRecord>,
) -> Result<sessions::SessionRecord> {
    let Some(record) = record else {
        bail!(
            "no record of session {session_id} on this host\n  \
             `review sessions` lists the sessions this host's review runs created."
        );
    };
    if !config::KNOWN_PROVIDERS.contains(&record.provider.as_str()) {
        bail!(
            "session {session_id} was recorded for provider '{}', which review no \
             longer supports\n  supported: {}",
            record.provider,
            config::KNOWN_PROVIDERS.join(", ")
        );
    }
    Ok(record)
}

/// What a `review resume` launches with, taken from the session's own last
/// recorded run: sandbox level, writable roots, model and reasoning effort.
///
/// A resume carries no profile, and the runners default to `read-only`, so a
/// resume used to silently drop the permissions the session was created with: a
/// `workspace-write` run that died mid-turn came back as a read-only resume that
/// could not touch the files it had been editing, and the failure surfaced as
/// the model reporting a read-only filesystem rather than as anything about
/// `review`. The permissions are a property of the *session*, not of the
/// invocation that happens to be driving it.
///
/// The sidecar already records both, and records the **effective** values (what
/// the run received, not what its profile asked for), which is exactly what has
/// to carry forward - the recurring lesson of the codex sandbox work is that
/// those two differ. Roots are passed as profile roots rather than as a verbatim
/// list so they go back through the same filters and are re-derived against
/// *this* host: a host fact like the build lock belongs to the machine the
/// resume runs on, not to the one that recorded the row.
///
/// Recorded levels are in the provider's own vocabulary, which `sandbox_for`
/// maps identically for codex and passes through for grok's already-native
/// names, so a round trip cannot change the level. A row predating these fields
/// inherits nothing and leaves the default in place. (A session with no row at
/// all never gets here - `resume_record` refuses it.) A row from another provider
/// also inherits nothing: impossible today, since the provider is read from the
/// same row, but a grok level fed to codex would be a widening nobody asked for,
/// so the guard stays rather than being assumed upstream.
///
/// Model and effort carry forward for the same reason and were originally left
/// out on the grounds that only `model` was recorded, so a partial restoration
/// would be less predictable than none. Measurement killed that argument: every
/// resume ran on codex's **built-in default model** at its default
/// effort, not the profile's, and since `--ignore-user-config` that default is
/// not even the operator's configured one. A session's first turn on
/// `gpt-5.6-sol`/`low` followed by five resumed turns on a different model is
/// not a neutral fallback - it silently changes the model mid-session, at a
/// different price, with nothing in the output saying so. The fix is to record
/// `effort` too rather than to keep inheriting neither.
///
/// Env and profile `config` are still *not* inherited: env values are
/// deliberately never recorded (they can carry secrets) and `config` is not
/// recorded at all, so there is nothing to restore from. That is an absence of
/// data, not a judgement about predictability.
fn inherited_settings(
    record: Option<&sessions::SessionRecord>,
    provider_name: &str,
) -> InheritedSettings {
    let Some(record) = record.filter(|r| r.provider == provider_name) else {
        return InheritedSettings::default();
    };
    InheritedSettings {
        sandbox: record.sandbox.clone(),
        // For grok these are exactly the roots the session's `review-ws-*`
        // profile is named for, and a resume uses them verbatim rather than
        // re-deriving - see `provider::GrokWrite::Resume`.
        writable_roots: record.writable_roots.clone(),
        model: record.model.clone(),
        effort: record.effort.clone(),
    }
}

#[derive(Default)]
struct InheritedSettings {
    sandbox: Option<String>,
    writable_roots: Vec<String>,
    model: Option<String>,
    effort: Option<String>,
}

/// `review resume <id>`: continue a specific provider session and send raw
/// stdin. No prime and no profile - the session already has its grounding from
/// the run that created it, and its provider, permissions, model and effort
/// come from that run's record. The archetype recorded for the resume is the
/// session's own, so its rows stay grouped under what the session was opened as.
async fn run_session_resume(
    session_id: &str,
    dry_run: bool,
    started: std::time::Instant,
) -> Result<()> {
    // Looked up once and reused by the cache-age gate and inheritance below.
    let record = resume_record(session_id, sessions::latest_for_session(session_id))?;
    let provider_name = record.provider.as_str();
    eprintln!("provider: {provider_name} (from the session record)");
    let archetype = record.archetype.as_str();

    let stdin_instructions = input::read_stdin()?;
    // Loaded once, and a config that exists but does not parse is an error
    // rather than treated as absent: it decides whether this resume's rows go
    // to the private log, and a silent fallback would file a private project's
    // turns in the public one. No `.review.toml` at all still works - a resume
    // needs no prime or profile - with the rows keyed to the cwd.
    let (cfg, found_root) = config::load_optional()?;
    let project_root = match found_root {
        Some(ref root) => root.clone(),
        None => std::env::current_dir()?,
    };

    if dry_run {
        eprintln!("session: {session_id}");
        eprintln!("archetype: {archetype}");
        println!("{stdin_instructions}");
        return Ok(());
    }

    if !provider::is_available(provider_name) {
        bail!("'{provider_name}' not found on PATH");
    }

    // A resume bypasses the config for prompt/profile purposes, but the
    // stall timeout is a safety setting rather than a prompt input, so it is
    // still honoured.
    let codex_runtime = provider::CodexRuntime::from_config(cfg.stall_timeout_secs());

    // Cache-age gate. The sidecar tells us how long it's been since the session
    // last ended; past ~55 minutes (the realistic cap on Anthropic's prompt
    // cache TTL - 5 min default, ~1h with the right env vars) the cache is cold,
    // and resuming means reprocessing the whole session prefix at full cost.
    // Resuming is the *warm* follow-up path, so a cold resume is refused: do a
    // fresh run with restated context instead. A row with no usable timestamp
    // leaves the age unknown, and the resume proceeds rather than blocks.
    if let Some(age) = sessions::age_secs(&record) {
        if age > timings::STALE_SESSION.as_secs() {
            bail!(
                "session last touched {} ago - its prompt cache is cold.\n  \
                 Resuming would reprocess the whole session prefix at full cost.\n  \
                 Start a fresh run with restated context instead.",
                sessions::format_age(age)
            );
        }
        if age < 60 {
            eprintln!("session last touched just now");
        } else {
            eprintln!("session last touched {} ago", sessions::format_age(age));
        }
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

    // Permissions, model and effort are inherited from the session's own last
    // recorded run rather than defaulted (see `inherited_settings`). Each is
    // announced, because the failure this replaced was entirely silent: the
    // resume simply ran on a different model and nothing said so.
    let inherited = inherited_settings(Some(&record), provider_name);
    if let Some(ref level) = inherited.sandbox {
        eprintln!("sandbox: {level} (inherited from the session record)");
    }
    if let Some(ref m) = inherited.model {
        eprintln!("model: {m} (inherited from the session record)");
    }
    if let Some(ref e) = inherited.effort {
        eprintln!("effort: {e} (inherited from the session record)");
    }

    let (launched_tx, launched_rx) = tokio::sync::oneshot::channel();
    let invoke = provider::invoke(
        provider_name,
        session_id,
        inherited.model.as_deref(),
        inherited.effort.as_deref(),
        inherited.sandbox.as_deref(),
        &inherited.writable_roots,
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

    // Resolve audit_id/private for the logs, persisting a generated id when a
    // project config exists. Without one, logging still happens - otherwise a
    // successful resume in a config-less dir would never be recorded and would
    // soon be refused as stale.
    let (audit_id, private) = match found_root {
        Some(ref root) => {
            let id = match cfg.audit.id.clone() {
                Some(id) => id,
                None => {
                    let id = config::generate_short_id();
                    let _ = config_write::append_audit_id(&root.join(".review.toml"), &id);
                    id
                }
            };
            (id, cfg.audit.private)
        }
        None => (config::generate_short_id(), false),
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
            // Recorded, not `None`: a resume now launches with an inherited
            // model and effort, and leaving them out of its own row would break
            // the chain at the first resume - the next one would find a row
            // naming neither and fall back to the provider default, which is
            // the bug this fixes, one link along.
            result.model.as_deref(),
            result.effort.as_deref(),
            Vec::new(),
            // The permissions this resume actually launched with - inherited
            // from the session's last recorded run, so the next resume in the
            // chain inherits from this row in turn.
            result.sandbox.as_deref(),
            result.writable_roots.clone(),
            &result.served,
            result.grok_trust.as_deref(),
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
    // `format_age` says "now" under a minute, which does not take "ago".
    let last = if latest.epoch_secs == 0 {
        "at an unknown time".to_string()
    } else {
        match sessions::format_age(now.saturating_sub(latest.epoch_secs)).as_str() {
            "now" => "just now".to_string(),
            age => format!("{age} ago"),
        }
    };

    println!("session: {session_id}");
    println!(
        "provider: {} / archetype: {} ({}) / {} touch(es), last {last}",
        latest.provider,
        latest.archetype,
        latest.kind,
        records.len()
    );
    println!("project: {}", latest.project);
    if let Some(ref model) = latest.model {
        println!("model: {model}");
    }
    if let Some(ref effort) = latest.effort {
        println!("effort: {effort}");
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
        // Only which project we are in matters here, so nothing is parsed: a
        // broken global config must not stop the operator listing sessions.
        let root = match config::project_root()? {
            Some(root) => root,
            None => std::env::current_dir()?,
        };
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

    fn minimal_row(provider: &str) -> sessions::SessionRecord {
        record(serde_json::json!({
            "timestamp": "2026-01-01T00:00:00Z",
            "epoch_secs": 1_767_225_600u64,
            "project": "/w",
            "hostname": "h",
            "audit_id": "a",
            "provider": provider,
            "archetype": "security",
            "session_id": "s",
            "operator_prompt": "p",
            "assembled_prompt": "p",
            "review_version": "0.0.0",
        }))
    }

    #[test]
    fn a_resume_takes_the_provider_from_the_record() {
        // The session ID came from review's own output, whose run wrote this
        // row, so the provider is on record and is never asked for.
        let rec = resume_record("s", Some(minimal_row("codex"))).expect("resolved");
        assert_eq!(rec.provider, "codex");
        assert_eq!(rec.archetype, "security");
    }

    fn resume_error(record: Option<sessions::SessionRecord>) -> String {
        match resume_record("s", record) {
            Ok(_) => panic!("expected the resume to be refused"),
            Err(e) => e.to_string(),
        }
    }

    #[test]
    fn a_resume_with_no_record_is_refused_and_points_at_sessions() {
        let err = resume_error(None);
        assert!(err.contains("review sessions"), "{err}");
    }

    #[test]
    fn a_recorded_provider_review_no_longer_supports_is_refused() {
        // A row naming a removed provider (kilo/opencode) must not turn into
        // an unrunnable invocation.
        let err = resume_error(Some(minimal_row("opencode")));
        assert!(err.contains("supported"), "{err}");
    }

    #[test]
    fn the_archetype_flag_is_taken_as_given() {
        let got = resolve_archetype_arg(Some("security,bugs"), None).expect("ok");
        assert_eq!(got.as_deref(), Some("security,bugs"));
        assert_eq!(resolve_archetype_arg(None, None).expect("ok"), None);
    }

    #[test]
    fn the_old_positional_archetype_still_works() {
        // Orchestration procedures written against `review security` keep
        // working until they are updated; they get a warning, not a failure.
        let got = resolve_archetype_arg(None, Some("security")).expect("ok");
        assert_eq!(got.as_deref(), Some("security"));
    }

    #[test]
    fn a_positional_bare_means_no_archetype() {
        // `review bare` was the old way to say "no priming".
        assert_eq!(resolve_archetype_arg(None, Some("bare")).expect("ok"), None);
    }

    #[test]
    fn an_archetype_given_both_ways_is_refused() {
        let err = resolve_archetype_arg(Some("a"), Some("b"))
            .expect_err("ambiguous")
            .to_string();
        assert!(err.contains("twice"), "{err}");
    }

    /// A sidecar row, built the way one is actually read back - from JSON, so a
    /// field this test forgets is exercised as a missing field rather than as a
    /// compile error the struct literal would have caught.
    fn record(json: serde_json::Value) -> sessions::SessionRecord {
        serde_json::from_value(json).expect("valid session record")
    }

    #[test]
    fn a_resume_inherits_the_permissions_of_the_run_that_created_the_session() {
        let rec = record(serde_json::json!({
            "timestamp": "2026-01-01T00:00:00Z",
            "epoch_secs": 1_767_225_600u64,
            "project": "/w",
            "hostname": "h",
            "audit_id": "a",
            "provider": "codex",
            "archetype": "implement",
            "session_id": "s",
            "kind": "run",
            "sandbox": "workspace-write",
            "writable_roots": ["/home/u/.brokkr", "/srv/cache"],
            "model": "gpt-5.6-sol",
            "effort": "low",
            "operator_prompt": "p",
            "assembled_prompt": "p",
            "review_version": "0.0.0",
        }));
        let got = inherited_settings(Some(&rec), "codex");
        assert_eq!(got.sandbox.as_deref(), Some("workspace-write"));
        assert_eq!(got.writable_roots, vec!["/home/u/.brokkr", "/srv/cache"]);
        assert_eq!(got.model.as_deref(), Some("gpt-5.6-sol"));
        assert_eq!(got.effort.as_deref(), Some("low"));
    }

    #[test]
    fn a_row_recording_a_model_but_no_effort_inherits_just_the_model() {
        // Rows written before `effort` was recorded carry a model and nothing
        // else. Inheriting the model is still strictly better than falling back
        // to the provider's default model, so the absent effort must not
        // suppress it.
        let rec = record(serde_json::json!({
            "timestamp": "2026-01-01T00:00:00Z",
            "epoch_secs": 1_767_225_600u64,
            "project": "/w",
            "hostname": "h",
            "audit_id": "a",
            "provider": "codex",
            "archetype": "implement",
            "session_id": "s",
            "model": "gpt-5.6-sol",
            "operator_prompt": "p",
            "assembled_prompt": "p",
            "review_version": "0.0.0",
        }));
        let got = inherited_settings(Some(&rec), "codex");
        assert_eq!(got.model.as_deref(), Some("gpt-5.6-sol"));
        assert_eq!(got.effort, None);
    }

    #[test]
    fn a_row_from_before_permissions_were_recorded_inherits_nothing() {
        // The pre-existing default (read-only, no roots) has to survive an old
        // row, since the fields are simply absent there.
        let rec = record(serde_json::json!({
            "timestamp": "2026-01-01T00:00:00Z",
            "epoch_secs": 1_767_225_600u64,
            "project": "/w",
            "hostname": "h",
            "audit_id": "a",
            "provider": "codex",
            "archetype": "implement",
            "session_id": "s",
            "operator_prompt": "p",
            "assembled_prompt": "p",
            "review_version": "0.0.0",
        }));
        let got = inherited_settings(Some(&rec), "codex");
        assert_eq!(got.sandbox, None);
        assert!(got.writable_roots.is_empty());
        assert_eq!(got.model, None);
        assert_eq!(got.effort, None);
    }

    #[test]
    fn permissions_are_not_inherited_across_providers() {
        // `resume_record` takes the provider from the same row, so this cannot
        // happen today, but a grok level fed to codex (or vice versa) is a
        // widening nobody asked for, so the guard is here rather than assumed
        // upstream.
        let rec = record(serde_json::json!({
            "timestamp": "2026-01-01T00:00:00Z",
            "epoch_secs": 1_767_225_600u64,
            "project": "/w",
            "hostname": "h",
            "audit_id": "a",
            "provider": "grok",
            "archetype": "implement",
            "session_id": "s",
            "sandbox": "none",
            "model": "grok-4",
            "operator_prompt": "p",
            "assembled_prompt": "p",
            "review_version": "0.0.0",
        }));
        let got = inherited_settings(Some(&rec), "codex");
        assert_eq!(got.sandbox, None);
        assert!(got.writable_roots.is_empty());
        // The model is provider-scoped too: a grok model name handed to codex
        // is not a fallback, it is an invocation that cannot start.
        assert_eq!(got.model, None);
    }

    #[test]
    fn no_record_inherits_nothing() {
        let got = inherited_settings(None, "codex");
        assert_eq!(got.sandbox, None);
        assert!(got.writable_roots.is_empty());
        assert_eq!(got.model, None);
        assert_eq!(got.effort, None);
    }
}
