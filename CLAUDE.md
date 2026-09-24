# CLAUDE.md

## Rules

### Bash
- Never use sed, find, awk, or complex bash commands
- Never chain commands with &&
- Never chain commands with ;
- Never pipe commands with |
- Never read or write from /tmp. All data lives in the project.

### Memory rules
Do not use your Memory functionality. Update CLAUDE.md instead. This project is developed across several hosts and several users. Memories do not transfer across hosts or users. CLAUDE.md does.

### Bash rules
- Never capture stdout into env vars (`UUID=$(...)`).
- Never run raw cargo, curl, pkill. Use `brokkr`. **This means never, including
  during iteration.** `cargo build` / `cargo test` while working, "just to check
  quickly", is the same violation as shipping with them - `brokkr` is the only
  supported entry point, and reaching past it means running a different build
  configuration from the one that gates the commit. The mapping is total; there
  is no case that needs raw cargo:

  | instead of | run |
  |---|---|
  | `cargo build` / `cargo clippy` / `cargo test` | `brokkr check` (gremlins + clippy + tests, the full gate) |
  | `cargo test <name>` | `brokkr test <name>` |
  | `cargo test <name>` repeatedly (flaky hunt) | `brokkr test <name> -N <count>` |
  | `cargo run -- <args>` | `brokkr run -- <args>` |
  | `cargo fmt` | `brokkr fmt` |
  | `cargo install --path .` | `brokkr install` |

- `brokkr check` is the gate; run it before every commit, not just at the end.
- `brokkr test <NAME>` is a substring filter over the package's unit *and*
  integration tests, release profile by default (`--debug` for dev). It always
  passes `--include-ignored --nocapture --test-threads=1`.
- **There is a 20-second per-test watchdog**, shared by `brokkr check`'s test
  phase and `brokkr test`. A test that runs longer is killed and reported as
  hung. `brokkr test --timeout <SECS>` raises it to at most 280s, and only for a
  name that matches exactly one test. This is why fixture timings in
  `src/provider_tests.rs` are injectable rather than real-world durations - a
  test that waited out the production 10s `SIGKILL` escalation would sit right
  under the ceiling and eventually trip it on a loaded machine.
- `brokkr man` lists bundled docs (`man check`, `man config`, `man clippy`,
  `man run`, `man results`, ...). Read those rather than guessing at flags.
- Waiting on a long command: just run it and stop. It wakes you on exit. Never
  poll it with `sleep`, `until`, or a watch loop.

### git commit rules
- Always run `brokkr fmt` before a commit.
- Never commit markdown changes and/or results.db alone. Bundle them with upcoming code commits.
- When committing other changes: always tag along brokkrs 'results.db' and markdown files if dirty.
- Write substantive engineering-focused commit messages.
- Has `Cargo.lock` changed? Commit it.
- Never `git push` unless the user explicitly asks. Stop after the commit.

### Subagents
- Do NOT use worktree isolation for parallel agents. Worktrees create merge conflicts that silently drop agent work. Instead, launch agents in the same tree with strict file ownership - zero overlap.

### Where notes go
- **Provider-specific notes go in `reference/<provider>.md`, not here.** How `review` drives codex, grok or claude - flags, failure modes, workarounds, measurements against a provider version - belongs in [reference/codex.md](reference/codex.md), [reference/grok.md](reference/grok.md) or [reference/claude.md](reference/claude.md). Permission machinery shared across providers (sandbox vocabulary, writable roots, recorded permissions, resume inheritance) belongs in [reference/sandbox.md](reference/sandbox.md). This file keeps the rules, the provider-agnostic design, and pointers.

## What this project is

A Rust CLI (`review`) that fans out code reviews to fresh AI sessions across providers (Claude Code, Codex, Grok). It's a prompt builder - the agents fetch code themselves. Each run starts a clean session, optionally primed with an archetype's prompt (see Design decisions for why fresh beats long-lived).

Layered config: the command line, then the project's `.review.toml`, then the operator's global `~/.config/review/config.toml` (same format). It carries archetypes (name → priming prompt, all optional), groups, default providers, and `[<provider>.<profile>]` profiles selected with `--profile`. `review config` prints the effective result with each value's source. Comma-separated archetypes/groups can be mixed freely, with deduplication.

## Build and run

`brokkr` is the only entry point - never raw `cargo` (see Bash rules for the
full mapping and the 20s per-test watchdog).

```
brokkr check                    # the gate: gremlins + clippy + tests
brokkr test <name>              # one test by substring; -N <n> to repeat
brokkr run -- <args>            # run the binary
brokkr fmt                      # before every commit
brokkr man check                # bundled docs for the pipeline
```

```
brokkr check
review init
echo "review for issues" | review security
```

Single binary crate, no workspace.

## Architecture

- `src/cli.rs` - Clap CLI. `review` with no subcommand is the run itself (stdin → providers); the archetype is the `-a` flag (omitted = bare: stdin sent unchanged) and `-p` the profile. Subcommands: `resume`, `interrupt`, `config`, `sessions`, `incidents`, `init`. Two hidden migration aliases, each warning when used: the old positional archetype (`review security`, with `review bare` meaning none) and `--session <ID>` for `review resume <ID>`. Reserved archetype/group names are listed under Reserved words. The help text is what orchestrating agents read in place of these docs, so it states the rules they would otherwise trip on: profile precedence and the sandbox default, the digest fields, exit status, resume's limits and stale cutoffs (the cutoffs are restated from `timings`, and a test keeps them in step), and that `review config` rather than `.review.toml` is the authoritative view. Change the behaviour, change the help.
- `src/config.rs` - Layered config. `parse_file` parses one file (`[archetypes]`, `[_groups]`, `[_defaults]` with `providers`/`stall_timeout_secs`, `[_audit]` - local only, an error in the global file - and profiles); `resolve` merges the local `.review.toml` (found by walking up from cwd, required because it holds the audit id) with the optional global `$XDG_CONFIG_HOME/review/config.toml` (else `~/.config/review/config.toml`) for this host. A top-level table named after a known provider is a hostless `[<provider>.<profile>]` table; any other top-level table is a legacy `[<host>.<provider>.<profile>]` host table, still parsed so existing files need no edit. Profiles carry model/effort/sandbox/env/`writable_roots`/`config` (verbatim codex `-c` overrides). Every resolved value is `Sourced` (layer + table) and each profile keeps the definitions it shadows, for `review config`. Parsed by peeling reserved sections off a `toml::Table` (serde `flatten` can't coexist with the sibling `archetypes` field). Also holds `sandbox_for` (see [reference/sandbox.md](reference/sandbox.md)) and `generate_uuid`/`generate_short_id`. Uses `toml` and `gethostname` crates.
- `src/config_tests.rs` - Layering tests for `config::resolve` (precedence, whole-profile replacement, host-vs-hostless, cross-layer group validation), included into `config.rs` as a `#[cfg(test)]` module.
- `src/config_cmd.rs` - `review config`: the effective configuration from the same resolver a run uses, and outside a project the global layer alone (an orchestrator asks before it has picked a project) - files consulted, default providers and whether each provider is installed here, archetypes, groups, profiles with their source table and whatever they shadow. Env var *names* only. Orchestration procedures should read this rather than parse `.review.toml`, which no longer tells the whole story. Plain text, like all of `review`'s output.
- `src/input.rs` - Reads stdin instructions (required, 20KB limit).
- `src/prompt.rs` - `assemble`: archetype prompt + `\n\n` + stdin; an empty prime (no archetype, or `bare = ""`) sends stdin verbatim. No baked-in grounding; the archetype prompt owns role and read/write intent. No slash-command handling - see Design decisions.
- `src/provider.rs` - Async provider invocation for claude, codex and grok. `invoke` resolves the effective sandbox level and writable roots, dispatches to the provider's runner, and returns a `ProviderResult` (output, session ID, optional `Digest`, and the permissions, model and effort the run launched with). Each run (oneshot=true) starts a fresh persistable session; `review resume` passes oneshot=false. All three emit the new session ID via `ProviderResult.session_id`. `print_result` renders the digest above the message. `DigestSummary` is the flat, serializable projection of a `Digest` persisted into the audit + sidecar logs so failures are greppable after the fact. Per-provider arguments and runners: [reference/claude.md](reference/claude.md), [reference/codex.md](reference/codex.md) (`run_codex`, `run_codex_json`, the three-tier final-message pick), [reference/grok.md](reference/grok.md) (`run_grok`, `run_grok_turn`).
- `src/provider_tests.rs` - End-to-end tests of the codex runner against a stub `codex` shell script - see [reference/codex.md](reference/codex.md#modules).
- `src/transcript.rs`, `src/incident.rs`, `src/watchdog.rs` - Codex rollout forensics, forensic bundles for suspicious runs (`review incidents`), and the rollout watchdog - see [reference/codex.md](reference/codex.md#modules).
- `src/grok_home.rs` - Grok's state dir: folder trust, content-addressed sandbox profiles, the background-result reader - see [reference/grok.md](reference/grok.md).
- `src/writable_roots.rs` - Derives the writable roots a `workspace-write` build needs outside the workspace - see [reference/sandbox.md](reference/sandbox.md#writable-roots).
- `src/inflight.rs` - Marker files at `~/.local/share/review/inflight/<session_id>.json` written once a run's session ID is known and removed when it returns, so `review sessions` can print `[in flight] ... turn in flight since <age>`. The sidecar is only written on return, so without this a running (or wedged) turn is indistinguishable from an idle session showing its previous response. Each marker records the owning `review` pid; `read_live` treats a marker whose pid is gone as stale, and deletes it. It also records the provider's pid (`child_pid`) for `review interrupt`, and holds the `<session_id>.interrupt` request files that verb leaves for the owning run. The session ID is validated as a bare filename component (alphanumerics, `-`, `_`) before being used as a path - it arrives straight from the command line (`review resume <id>`), whose validation `review` otherwise delegates to the provider, so unchecked it let an ID like `../foo` escape the marker directory and write then *delete* a file elsewhere. An allowlist, not a `..` denylist, because only the former is safe by construction. Best-effort throughout - never derails a run.
- `src/interrupt_cmd.rs` - `review interrupt <ID>`: ends a codex run's turn with SIGINT, waits for the owning run to record the session, and prints the `review resume` command. Why that is the only lever, and the mechanics, are in [reference/codex.md](reference/codex.md#interrupting-a-run).
- `src/sessions.rs` - Append-only sidecar log at `~/.local/share/review/sessions.jsonl` (or `sessions-private.jsonl` if `audit.private`). One row per run that captured a session ID (`kind = "run"`), one per `review resume` (`kind = "session"`). Rows carry timestamp + epoch_secs, project, hostname, audit_id, provider, archetype, session_id, model, effort, env var *names* (not values - those can carry secrets), the `sandbox` level the run launched with and any derived `writable_roots`, (grok) the provider-reported `served_model` and `cost_usd`, operator prompt, assembled prompt, response or error, review version, and the flat `DigestSummary` when the provider produced one. Read helpers (`read_all`, `latest_for_session`, `age_secs`, `format_age`) drive the cache-age gate in `review resume` and the `review sessions` subcommand.
- `src/config_write.rs` - `append_audit_id` (the only writer left; archetypes/profiles are hand-edited).
- `src/main.rs` - Wires CLI to config, prompt assembly, and provider dispatch. Also prints the trailing `runtime:` line: a wall clock started before `Cli::parse()` and printed after the last result on both the fan-out and `review resume` paths (`format_runtime`). It deliberately spans stdin read + global-lock wait, so it's what the operator waited, not provider time; the other subcommands (`init`/`config`/`sessions`/`incidents`/`interrupt`) return before it. The fan-out's per-launch task is `run_launch`, whose auto-resume decisions are the pure `after_run` / `after_auto_resume`.

## Design decisions

Provider-agnostic decisions only. Provider-specific ones are in
[reference/codex.md](reference/codex.md), [reference/grok.md](reference/grok.md)
and [reference/claude.md](reference/claude.md); permissions in
[reference/sandbox.md](reference/sandbox.md).

- Every run starts a fresh session - archetype priming prompt (if any) + stdin. Reviving a long-lived session on a cold cache reprocesses its ever-growing history; a fresh session costs ~one review's worth of tokens and can't act on stale accumulated context. The session is persistable and its ID is printed so follow-ups can go through `review resume` while the cache is warm.
- No baked-in grounding prefix. It was written for long-lived read-only review sessions (anti-staleness + "don't modify files"); fresh-per-run made the anti-staleness lines dead and workspace-write made "don't modify files" wrong. Archetypes now own their own grounding.
- **Config is layered - command line, project `.review.toml`, global `~/.config/review/config.toml` - because the model choice outgrew per-project files.** Models turn over every few weeks, and the tier → model mapping had been restated in every project's file, once per host per tier; 18 files drifted out of step with each other and with what was actually current, and keeping them current was a job of its own. Which model serves a tier is the operator's current opinion, not a project fact, so it lives once in the global file; a project keeps what it genuinely owns (domain archetypes, the audit id, a deliberate override). Nothing is built in. Both files share one format. Rules: archetypes and groups are a union with the project winning per name - including a group against an archetype of the same name, so adding a group to the global file cannot break a project that has an archetype by that name (within one file such a clash is still an error, there being no layer to pick by) - and a project group may name a global archetype, while a global group may name only global archetypes, because the global file is read in every project; `providers`/`stall_timeout_secs` come from the first layer that *sets* them (an explicit empty list counts); a profile resolves to its **first** definition over local-host, local-hostless, global-host, global-hostless, and wins **whole** - no field-level merge, so no field of a run comes from a table that did not mention it (the same objection to ambient defaults that makes every profile state its sandbox). `.review.toml` stays required for a run because `[_audit]` identifies the project; a file holding only `[_audit]` is a complete config. Profile keys are `deny_unknown_fields`: a profile wins whole, so a misspelled key that parsed as an empty profile would silently discard the global definition it shadows. Loading is strict everywhere a result decides something - a config that exists but does not parse is an error, never treated as absent, because `review resume` reads `audit.private` from it and a silent fallback filed a private project's resumes in the public log. `config::load_optional` (no `.review.toml` → global layer alone) serves `review resume` and `review config`; `config::project_root` (locate, no parse) serves `review sessions`, which only needs to know the project.
- **Host splits are gone; legacy host tables still parse.** In every file the model choice was identical across hosts - only machine facts (env, paths) are host-bound, and `writable_roots` already expands `~`/`$VAR` for those. A legacy `[<host>.<provider>.<profile>]` applies on the host it names and beats a hostless table in the same file; another host's tables are ignored. Consequence, accepted: an old file's host tables keep shadowing the global config on those hosts until the operator removes them, at their own pace - `review config` shows each one next to what it hides.
- **A provider that is not installed fails the run before anything launches.** Without host scoping one config reaches every machine, including ones missing a harness. It used to be a warning and a skip, which quietly ran a narrower fan-out than was asked for; now it is resolved up front (unknown name or not on `PATH`) and `review config` reports availability, so an orchestrator can learn it before assigning a role. `--dry-run` launches nothing, so there it only warns.
- Archetypes are optional and pure: `[archetypes]` name → prompt, no host/session binding. Without one a run is bare - stdin goes through unchanged, recorded under the archetype label `bare`. Overrides live in named profiles (`[<provider>.<profile>]` carrying model/effort/sandbox/env) selected with `-p <name>`; `-p` requires some layer to define it for every launched provider or the run errors naming every file searched. `sandbox` defaults to `read-only`, so a run with no profile can never modify files; a profile opts up to `workspace-write` (see [reference/sandbox.md](reference/sandbox.md)).
- **No slash-command archetypes.** `goal = "/goal "` was supported by inlining stdin onto the command's line. `/goal` works in an interactive session the operator opens, but does nothing sent headless through `review`, so the special case served nothing and was removed. An existing `goal` archetype is now an ordinary prime.
- Providers resolve from `--provider`, else `[_defaults].providers` from the first layer that sets it; empty → error. The providers are claude, codex and grok; kilo/opencode were removed.
- Providers get prompts via **stdin pipe**, not CLI args, to avoid shell argument length limits. Grok is the exception - it takes no prompt on stdin, so it gets a `--prompt-file` (see [reference/grok.md](reference/grok.md#invocation)).
- **The run is the command itself; everything else is a verb.** `echo … | review` sends stdin to the configured providers, and the archetype - optional since it stopped being the only way to launch - is the `-a` flag. It used to be the positional argument, which put archetypes and subcommands in one namespace: every new subcommand silently shadowed any project's archetype of that name, hence a `RESERVED_NAMES` list that had already fallen out of step (`incidents` was missing). As a flag it cannot collide; the subcommand names stay reserved only because of the legacy positional alias (see Reserved words). `review run` was rejected as ceremony on every call. The positional form and `--session` stay as hidden aliases that warn, so orchestration procedures keep working until they are updated; while the positional alias exists a stray word like `review secruity` is still taken as an archetype and reported as an unknown one, and once it is removed that becomes clap's "unrecognized subcommand". Run flags (`-a`, `-p`, `--provider`, `--dry-run`, `--stagger`) conflict with subcommands (`args_conflicts_with_subcommands`): clap otherwise accepts them in front of a subcommand and silently drops them, and `review --dry-run resume <ID>` sent a real turn.
- **No JSON output, anywhere.** Agents read text as reliably as people do - measured on the operator's own tools, where making JSON the default led Claude agents to pass `--human` to switch it off. `review config` is plain text, and so is every run's output.
- `review resume <id>` continues a specific provider session and sends raw stdin - no prime, no profile (permissions, model and effort are inherited from the session instead - see [reference/sandbox.md](reference/sandbox.md#what-a-resume-inherits)). It is a subcommand rather than a flag because it changes what every other option means. **The provider comes from the session's sidecar record, and there is no `--provider`.** The only place an operator gets a session ID from is `review`'s own output, whose run wrote that record, so there is nothing to ask for; a session with no record (another host's, one made outside `review`, a lost log) is refused rather than guessed at, and `review sessions` lists what this host knows. The recorded provider is still validated against `KNOWN_PROVIDERS`, so a row naming a removed provider (kilo/opencode) can't turn into an unrunnable invocation. (Found the hard way: a resume of an unrecorded ID passed to `codex exec resume` ran a real turn instead of failing.) Validation of the session ID itself is delegated to the provider. Before invoking, `review` checks the record's last-touched time: a resume is the *warm* path, so if the session last ended longer ago than its provider's cutoff (`timings::stale_session`: 27 min for codex, whose prompt cache lasts ~30 min; 55 min otherwise, past Anthropic's realistic prompt-cache TTL) it **errors out** and tells the operator to do a fresh run instead of paying to reprocess a cold prefix. A row with no usable timestamp leaves the age unknown and the resume proceeds. What refreshes the last-touched clock is *any resume that ran* (`resume_ran = output.is_ok()`), not just one that produced a good answer: the clock tracks prompt-cache warmth, and reprocessing the prefix warms the cache even on a codex mid-turn death (which returns `Ok`-with-a-death-digest). Gating refresh on answer quality would pin the clock to the last *successful* touch and wrongly refuse the next resume of a genuinely-warm session as stale; only a hard launch failure (`Err`, cache never warmed) skips the refresh.
- **A running codex turn cannot be messaged; it can be interrupted.** `review interrupt <ID>` ends the turn and hands back the session for `review resume` - the evidence that nothing gentler exists is in [reference/codex.md](reference/codex.md#interrupting-a-run). An interrupted run counts as failed for exit-code purposes, like any run without an answer, but is flagged `interrupted`, never auto-resumed and never bundled as an incident. A request only counts when the run has no answer: codex can finish between the request and the signal. To make the resume usable promptly, each fan-out task writes its own audit and sidecar rows as soon as its run ends, rather than the parent writing them all after the slowest run.
- `review resume` releases the global lock **once the provider process has spawned**, not when the run finishes, matching the fan-out path (which releases once its staggered launches have fired). The lock spaces out launches, and a resume launches one thing, so there is nothing left to serialize once it runs; holding it for the turn meant one wedged resume froze all `review` traffic on the host indefinitely. A hung run should cost you that run, not the tool. Release is driven by a launch handshake (`provider::LaunchSignal`, a oneshot sent right after `cmd.spawn()`) rather than by dropping the lock before calling `invoke` - the latter leaves a critical section guarding nothing, letting every queued resume through to spawn simultaneously. A dropped sender (failed spawn) releases it too.
- A stalled or dead run returns `Ok` (so its session ID, digest and incident survive) but exits **1**, on both the fan-out and `review resume` paths. Exiting 0 on a wedged run lies to scripts and CI. What counts as dead is provider-specific: codex's is reconstructed from its rollout ([reference/codex.md](reference/codex.md#two-ways-a-run-fails)), grok states it ([reference/grok.md](reference/grok.md#turns-classify-themselves)).
- `review sessions` lists recent sessions for the current project (or `--all`), grouped by session ID, sorted by most recent touch. `review sessions <id>` shows one session's artifacts on demand: its persisted digest, the on-disk codex rollout transcript (path + task_complete/stream_error/last_event/last_in_flight_tool), and the final response - preferring the transcript's `final_answer` over the sidecar `response`, so even rows recorded before runtime recovery landed surface the real answer. Output is block-formatted for terminal reading; ad-hoc queries beyond that go through `jq` on the JSONL directly.

## Reserved words

A config naming an archetype or group any of these fails to parse (`config::RESERVED_NAMES`, checked in `config::parse_file`):

| Word | Why |
|---|---|
| `all` | The `-a` keyword for every configured archetype. |
| `resume`, `interrupt`, `config`, `sessions`, `incidents`, `init`, `help` | Subcommand names. `-a` itself cannot collide with them, but while the legacy positional alias exists, `review sessions` runs the subcommand rather than the archetype, silently. |
| `bare` | The legacy positional `review bare` means "no archetype", whatever the config defines under that name. So `bare` may only be an *empty* archetype (`bare = ""`, in most existing configs, stays valid) and never a group. |

Keep this list in step with `cli::Command` and `config::RESERVED_NAMES` when either changes.

## Config format

The global `~/.config/review/config.toml` and a project `.review.toml` share
this format; `[_audit]` is the one section allowed only in the project file.

```toml
[archetypes]
security = "You are a security expert. Read the codebase."
bugs = "You hunt for edge cases and correctness bugs."

[_defaults]
providers = ["claude", "codex"]
# Seconds of rollout silence with no answer before a codex run is treated as
# stalled (killed + incident + failure). 0 disables. Omitted = 900. Codex-only.
stall_timeout_secs = 900

[_groups]
sweep = ["security", "bugs"]

# provider . profile
[claude.opus]
model = "Opus 4.8"
effort = "medium"
env = { ANTHROPIC_BASE_URL = "http://localhost:8787" }

[codex.implement]
model = "gpt-5.6-terra"
effort = "high"
sandbox = "workspace-write"
# Optional: extra writable roots, ADDED to the ones derived from the host (the
# build lock, cargo home and cargo target - see `reference/sandbox.md`), never
# replacing them. Only meaningful under `workspace-write`; a `read-only` profile
# widens nothing. `~`, `$VAR` and `${VAR}` are expanded, which is what lets one
# hostless profile serve machines with different layouts; an entry naming an
# unset variable is dropped with a warning rather than expanded to nothing.
writable_roots = ["/srv/fixtures", "$XDG_CACHE_HOME/assets", "~/corpora"]
# Optional: extra codex `-c key=value` overrides, each passed verbatim (codex-only).
config = ['model_provider="openai-http"']

# Legacy host-scoped form, still accepted: applies only on that host, and beats
# [codex.implement] in the same file there.
[myhostname.codex.implement]
model = "gpt-5.6-sol"

# Project file only.
[_audit]
id = "43fd"
```

## Document folders

The standing layout, across every project. Three live folders plus one retired,
split by durability first, subject second.

| Folder | Contents | Rule |
|---|---|---|
| `reference/` | Durable in-repo reference for anyone working on or with the code - how the thing is built and why: `architecture.md`, `technical-implementation-spec.md`, `performance.md` (the durable record of measured numbers over time), invariants, protocol contracts | Citable from source as a source of truth. What it says must be true. |
| `docs/` | Durable in-repo documentation of how the thing is used - guides, CLI reference, the consumer-facing API surface. Sometimes exposed as a hand-edited VitePress gh-pages site | Same must-be-true rule. |
| `notes/` | Transient - work items (`todo.md`), future plans, hypotheticals, bug reports, research, analysis. Things that will die | No truth guarantee. Nothing durable cites it. |
| `plans/` | Retired | Plan documents are transient: they go in `notes/`. |

`reference/` and `docs/` are both durable and both binding. The difference is
subject, not audience: `reference/` covers how the thing is built and why - what
you need in order to change it safely - while `docs/` covers how it is used. A
developer or library consumer reads both. Where a project publishes a site,
`docs/` is what gets published; the folder means the same thing either way.
`notes/` is neither durable nor binding, which is the whole point of keeping it
separate: a document that may be wrong must not sit where a document that must
be right is expected.

In this repo `reference/` holds one document per provider (`codex.md`,
`grok.md`, `claude.md`) plus `sandbox.md` for the permission machinery they
share.

**`scripts/` is a fourth, small exception: maintained diagnostics, not
transient investigation.** Each one exists because a claim in `reference/`
needs to stay re-testable against a provider that upgrades underneath us; the
current set is codex's, and [reference/codex.md](reference/codex.md#source-checkout-and-diagnostics)
says what each measures. They shell out to the provider and cost real turns, so
they are run deliberately after a provider upgrade, never from `brokkr check`.
A probe that answers one question and will not be asked again belongs in
`notes/`, not here.

**`research/` holds gitignored checkouts of provider source** (`research/codex`,
`research/grok-build`) - not ours, not committed, not guaranteed to be present,
but the authority on provider behaviour when they are. How to use each is in
its provider's reference document.

The dependency direction is therefore one-way. `notes/` may cite `docs/` and
`reference/`; nothing durable may cite `notes/` - not a code comment, not
`docs/`, not `reference/`. A code comment must carry its full context, because
it outlives the note.

**Root-level convention files are exempt.** `AGENTS.md`, `CLAUDE.md`,
`README.md`, `LICENSE`, `CHANGELOG.md` and their kin are found by tooling and by
convention at the repository root, and stay there. These folders govern
documents we chose where to put, not files whose location is dictated.

In `notes/`, `docs/` and `reference/` alike, avoid citing source line numbers -
they drift fast.
