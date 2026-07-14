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
- Never run raw cargo, curl, pkill. Use `brokkr`.

### git commit rules
- Always run `brokkr fmt` before a commit.
- Never commit markdown changes and/or results.db alone. Bundle them with upcoming code commits.
- When committing other changes: always tag along brokkrs 'results.db' and markdown files if dirty.
- Write substantive engineering-focused commit messages.
- Has `Cargo.lock` changed? Commit it.
- Never `git push` unless the user explicitly asks. Stop after the commit.

### Subagents
- Always launch subagents in the foreground (never use `run_in_background: true`) so the user can approve tool requests.
- Do NOT use worktree isolation for parallel agents. Worktrees create merge conflicts that silently drop agent work. Instead, launch agents in the same tree with strict file ownership - zero overlap.

## What this project is

A Rust CLI (`review`) that fans out code reviews to fresh AI sessions across providers (Claude Code, Codex). It's a prompt builder - the agents fetch code themselves. Each run starts a clean session primed with an archetype's prompt (see Design decisions for why fresh beats long-lived).

Per-project config via `.review.toml`: archetypes (name → priming prompt), groups, default providers, and host-scoped `--profile` overrides. Comma-separated archetypes/groups can be mixed freely, with deduplication.

## Build and run

Use `brokkr` for build/test/clippy (`brokkr check`) and running (`brokkr run -- ...`).

```
brokkr check
review init
echo "review for issues" | review security
```

Single binary crate, no workspace.

## Architecture

- `src/cli.rs` - Clap CLI. Archetype is a positional arg; `init` and `sessions` are subcommands.
- `src/config.rs` - Parses `.review.toml` in cwd. `[archetypes]` maps name → priming prompt; `[_groups]` names archetype sets; `[_defaults].providers` is the provider list when `--provider` is omitted; every other top-level table is a hostname carrying `[<host>.<provider>.<profile>]` profiles (model/effort/env). Parsed by peeling reserved sections off a `toml::Table` and treating the rest as hosts (serde `flatten` can't coexist with the sibling `archetypes` field). Also holds `generate_uuid`/`generate_short_id`. Uses `toml` and `gethostname` crates.
- `src/input.rs` - Reads stdin instructions (required, 20KB limit).
- `src/prompt.rs` - `assemble`: archetype prompt + stdin. No baked-in grounding; the archetype prompt owns role and read/write intent.
- `src/provider.rs` - Async provider invocation for claude and codex only. Prompts piped via stdin. Each run (oneshot=true) starts a fresh persistable session (claude `--session-id <generated UUID> --permission-mode dontAsk`, codex `exec --json` to capture `thread_id`); `--session` resume passes oneshot=false. Profile settings applied: `model` (claude `--model`, codex `-m`), `effort` (claude `--effort`, codex `-c model_reasoning_effort=`), `sandbox` (codex-only: `--sandbox`, default `read-only`; claude ignores it - its `--permission-mode` is a different axis with no honest mapping), `env`. Claude/codex emit the new session ID via `ProviderResult.session_id`. Both codex paths (fresh + `--session` resume) share `run_codex_json`, which streams `--json` and takes `-o`, then distills a `Digest` (exit/signal, `captured` from the `-o`/`--output-last-message` backstop, turn count, summed token usage, non-JSON log lines, optional transcript forensics) from the NDJSON (`thread.started`/`item.completed` agent_message/`turn.completed`); it does not bail on a non-zero exit so a halted/errored run still reports what it produced. `run_codex` just builds the args (fresh vs `resume <id>`); the `-o` temp file is keyed by archetype so concurrent codex archetypes don't collide. `print_result` renders the digest above the message.
- `src/transcript.rs` - Codex on-disk transcript forensics. Locates `$CODEX_HOME/sessions/**/rollout-*-<session_id>.jsonl` by filename (the session ID is captured, so no cwd/mtime heuristic) and parses it for `task_complete`, `stream_error`, the last event, and the last in-flight tool call (a `function_call` with no matching `_call_output` = what was running when it stopped). Read only when a run looks wrong (not captured / non-zero exit / signal), so clean runs stay uncluttered.
- `src/sessions.rs` - Append-only sidecar log at `~/.local/share/review/sessions.jsonl` (or `sessions-private.jsonl` if `audit.private`). One row per run that captured a session ID (`kind = "run"`), one per `--session` resume (`kind = "session"`). Rows carry timestamp + epoch_secs, project, hostname, audit_id, provider, archetype, session_id, model, env var *names* (not values - those can carry secrets), operator prompt, assembled prompt, response or error, and review version. Read helpers (`read_all`, `latest_for_session`, `age_secs`, `format_age`) drive the cache-age gate in `--session` mode and the `review sessions` subcommand.
- `src/config_write.rs` - `append_audit_id` (the only writer left; archetypes/profiles are hand-edited).
- `src/main.rs` - Wires CLI to config, prompt assembly, and provider dispatch.

## Design decisions

- Every run starts a fresh session - archetype priming prompt + stdin. Reviving a long-lived session on a cold cache reprocesses its ever-growing history; a fresh session costs ~one review's worth of tokens and can't act on stale accumulated context. The session is persistable and its ID is printed so follow-ups can go through `--session` while the cache is warm.
- No baked-in grounding prefix. It was written for long-lived read-only review sessions (anti-staleness + "don't modify files"); fresh-per-run made the anti-staleness lines dead and workspace-write made "don't modify files" wrong. Archetypes now own their own grounding.
- Archetypes are pure: `[archetypes]` name → prompt, no host/session binding. Overrides live in separate host-scoped named profiles (`[<host>.<provider>.<profile>]` carrying model/effort/sandbox/env) selected with `--profile <name>`; `--profile` requires the table to exist for every launched provider or the run errors. `sandbox` defaults to `read-only`, so a bare run can never modify files; a profile opts up to `workspace-write`. Codex-only - claude's `--permission-mode` is a different axis (tool-approval, not a filesystem sandbox) with no honest mapping, so claude ignores `sandbox`.
- Providers resolve from `--provider`, else `[_defaults].providers`; empty → error.
- `--session <id>` resumes a specific provider session and sends raw stdin - bypasses `.review.toml`, no prime, no profile. Requires a single `--provider`. Validation of the session ID is delegated to the provider. Before invoking, `review` looks up the sidecar log for the last-touched time: `--session` is the *warm* path, so if the session last ended > 55 min ago (`STALE_SESSION_SECS`, past the realistic prompt-cache TTL) it **errors out** and tells the operator to do a fresh run instead of paying to reprocess a cold prefix. No sidecar record -> age unknown -> proceed.
- `review sessions` lists recent sessions for the current project (or `--all`), grouped by session ID, sorted by most recent touch. Output is block-formatted for terminal reading; ad-hoc queries beyond that go through `jq` on the JSONL directly.
- Providers get prompts via **stdin pipe**, not CLI args, to avoid shell argument length limits.
- Claude runs with `--permission-mode dontAsk` (uses pre-approved permissions, rejects interactive prompts). Codex runs with `--sandbox read-only`.
- No global config - `.review.toml` lives in the project root.
- claude and codex only. kilo/opencode were removed.
- Subsuming the pbfhogg spec-loop python scripts (codex review/implement roles). Landed: sandbox as a profile field (codex-only), the rich codex digest + `-o` backstop, transcript forensics, and resume-path digest parity. `goal` needs no code - it's just an archetype whose prompt is `/goal`. Remaining: persist digest usage into the sidecar.
- Planned: a `review add` command to create an archetype from a priming prompt (currently hand-edited).

## Config format

```toml
[archetypes]
security = "You are a security expert. Read the codebase."
bugs = "You hunt for edge cases and correctness bugs."

[_defaults]
providers = ["claude", "codex"]

[_groups]
sweep = ["security", "bugs"]

# host . provider . profile
[myhostname.claude.opus]
model = "Opus 4.8"
effort = "medium"
env = { ANTHROPIC_BASE_URL = "http://localhost:8787" }

[myhostname.codex.implement]
model = "gpt-5.6-terra"
effort = "high"
sandbox = "workspace-write"
```
