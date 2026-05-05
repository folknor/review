# CLAUDE.md

## Rules

### Bash
- Never use sed, find, awk, or complex bash commands
- Never chain commands with &&
- Never chain commands with ;
- Never pipe commands with |
- Never read or write from /tmp. All data lives in the project.

### Subagents
- Always launch subagents in the foreground (never use `run_in_background: true`) so the user can approve tool requests.
- Do NOT use worktree isolation for parallel agents. Worktrees create merge conflicts that silently drop agent work. Instead, launch agents in the same tree with strict file ownership — zero overlap.

### Commits
- Don't commit pure markdown changes on their own. Bundle them with the code change they relate to, or skip them. Unless the markdown update is substantive.
- Has Cargo.lock changed? Commit it.

## What this project is

A Rust CLI (`review`) that fans out code reviews to persistent AI sessions across multiple providers (Claude Code, Codex, Kilo, OpenCode). It's a prompt builder that knows about sessions — the agents fetch code themselves.

Per-project config via `.review.toml` (host-scoped session IDs, optional model overrides). Custom archetypes and groups also supported. Comma-separated archetypes/groups can be mixed freely, with deduplication.

## Build and run

```
cargo build
cargo install --path .
review init
echo "review for issues" | review security
```

Single binary crate, no workspace.

## Architecture

- `src/cli.rs` — Clap CLI. Archetype is a positional arg, `init` and `prime` are subcommands.
- `src/config.rs` — Parses `.review.toml` in cwd. TOML config for host-scoped sessions (archetype → hostname → provider), `_groups` for named archetype sets, `_defaults.providers` for the prime-only-archetype fallback (see Design decisions). Uses `toml` and `gethostname` crates.
- `src/input.rs` — Reads stdin instructions (required, 20KB limit).
- `src/prompt.rs` — Assembles: compiled prefix + stdin (`--anchor`), or prefix + `[_prime]` prompt + stdin (`--oneshot`).
- `src/provider.rs` — Async provider invocation. Prompts piped via stdin. Claude uses `--permission-mode dontAsk`, Codex uses `--sandbox read-only`. In oneshot mode each provider runs a fresh persistable session (claude `--session-id <generated UUID>`, codex `--json` to capture `thread_id`, kilo `--auto`, opencode plain). Claude/codex emit the new session ID via `ProviderResult.session_id` so the operator can follow up via `--session`.
- `src/sessions.rs` — Append-only sidecar log at `~/.local/share/review/sessions.jsonl` (or `sessions-private.jsonl` if `audit.private`). One row per `--oneshot` that captured a session ID (`kind = "oneshot"`), one per `--session` resume (`kind = "session"`), and one per `review prime` invocation that successfully created a session (`kind = "prime"`). Rows carry timestamp + epoch_secs, project, hostname, audit_id, provider, archetype, session_id, model, env var *names* (not values — those can carry secrets), operator prompt, assembled prompt, response or error, and review version. Read helpers (`read_all`, `latest_for_session`, `age_secs`, `format_age`) drive the cache-age advisory in `--session` mode and the `review sessions` subcommand.
- `src/prime.rs` — Session creation for `review prime`. Claude uses `--session-id`, Codex uses `--json` to capture `thread_id`.
- `src/config_write.rs` — Appends session entries to `.review.toml`.
- `src/main.rs` — Wires CLI to config, prompt assembly, and provider dispatch.
- `prompts/` — Grounding prefix compiled into the binary via `include_str!`.

## Design decisions

- Stdin goes directly to provider sessions by default. `--anchor` prepends a grounding prefix.
- `--oneshot` skips session resume to avoid reprocessing accumulated session prefixes on cold-cache daily wakes; prepends `[_prime].<archetype>` instead. Existing `[archetype.host]` config still drives provider selection and overrides; only the session ID is ignored. The fresh session is persistable and its ID is printed above the response so the operator can follow up via `--session <id>` while the cache is warm. Implies `--anchor`.
- Prime-only archetypes: an archetype defined only in `[_prime]` (no `[archetype.host]` block) is valid for `--oneshot` — there's no session to persist, so no host block is needed. Provider list resolves from `--provider` first, then `[_defaults].providers`. Without either, the run errors with a message naming both. Non-oneshot still requires a host session (nothing to send to without one).
- `--session <id>` resumes a specific provider session and sends raw stdin — bypasses `.review.toml`, no PREFIX, no prime, no anchor. Requires a single `--provider`; mutually exclusive with `--oneshot` and `--anchor`. Validation of the session ID is delegated to the provider. Before invoking, `review` looks up the sidecar log to print the time since the last touch and warn when it's > ~55 min (past the realistic prompt-cache TTL).
- `review sessions` lists recent sessions for the current project (or `--all`), grouped by session ID, sorted by most recent touch. Output is block-formatted for terminal reading; ad-hoc queries beyond that go through `jq` on the JSONL directly.
- Providers get prompts via **stdin pipe**, not CLI args, to avoid shell argument length limits.
- Claude runs with `--permission-mode dontAsk` (uses pre-approved permissions, rejects interactive prompts). Codex runs with `--sandbox read-only`.
- No global config — `.review.toml` lives in the project root.

## Config format

```toml
[security.myhostname]
claude = "session-id"
codex = "session-id"

[bugs.myhostname]
claude = "session-id"
codex = { session = "session-id", model = "o3" }
kilo = { session = "session-id", model = "anthropic/claude-sonnet-4.6" }
opencode = { session = "session-id", model = "openai/gpt-5" }
claude = { session = "session-id", env = { ANTHROPIC_BASE_URL = "http://localhost:8787" } }

[_groups]
sweep = ["security", "bugs"]

[_defaults]
providers = ["claude"]
```
