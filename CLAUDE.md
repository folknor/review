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

A Rust CLI (`review`) that fans out code reviews to persistent AI sessions across multiple providers (Claude Code, Codex). Each project configures named reviewer archetypes (security, bugs, perf, etc.) backed by long-lived sessions. The tool assembles a prompt (prefix + archetype prompt + content), sends it to all providers for the archetype in parallel, and prints labeled results.

The spec is the source of truth: `docs/spec.md`.

## Build and run

```
cargo build
cargo run -- <archetype> --staged   # example
```

No tests yet. No workspace, single binary crate.

## Architecture

- `src/cli.rs` — Clap CLI. The review action is the **default** (archetype is a top-level positional arg). Management commands (`register`, `deregister`, `list`) are subcommands.
- `src/config.rs` — TOML config at `~/.config/review/config.toml`. Project resolution is longest-prefix match against cwd.
- `src/input.rs` — Resolves input sources (git diff variants, document, stdin). Explicit flags always take priority over stdin.
- `src/prompt.rs` — Assembles: prefix template + archetype prompt (diff or document variant) + content.
- `src/provider.rs` — Async provider invocation. Prompts are piped via stdin. Output files are PID-scoped in `/tmp`. Errors print to stdout within the labeled block.
- `src/session.rs` — Register/deregister/list commands that mutate the config file.
- `src/main.rs` — Wires CLI parsing to the appropriate handler.

## Design decisions

- Providers get prompts via **stdin pipe**, not CLI args, to avoid shell argument length limits.
- Temp output files include PID (`/tmp/review-<archetype>-<provider>-<pid>.txt`) to prevent races between concurrent invocations.
- `git show --format=` is used for `--commit` to handle root commits.
- The CLI uses `Option<ManagementCommand>` so that no subcommand = review mode, matching the spec's `review <archetype> <input-source>` interface.

## Config format

```toml
[global]
prefix = "~/.config/review/prompts/prefix.md"

[projects.myproject]
path = "/home/user/myproject"

[projects.myproject.archetypes.security]
claude = "session-id"
codex = "session-id"
prompt_diff = "~/.config/review/prompts/security/diff.md"
prompt_document = "~/.config/review/prompts/security/document.md"
```
