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

A Rust CLI (`review`) that fans out code reviews to persistent AI sessions across multiple providers (Claude Code, Codex). Each project configures named reviewer archetypes (security, bugs, perf, etc.) backed by long-lived sessions. The tool assembles a prompt (prefix + archetype prompt + user instructions from stdin + content), sends it to all providers for the archetype in parallel, and prints labeled results.

## Build and run

```
cargo build
echo "review this for issues" | cargo run -- <archetype> --staged
```

Single binary crate, no workspace.

## Architecture

- `src/cli.rs` — Clap CLI. The review action is the **default** (archetype is a top-level positional arg). Management commands (`register`, `deregister`, `list`) are subcommands.
- `src/config.rs` — TOML config at `~/.config/review/config.toml`. Project resolution is longest-prefix match against cwd, with path canonicalization.
- `src/input.rs` — Resolves input sources (git diff variants, document file). Reads user instructions from stdin (required, 20KB limit).
- `src/prompt.rs` — Assembles: prefix + archetype prompt + stdin instructions + content. Built-in prompts for security, bugs, perf, arch; generic fallback for custom archetypes.
- `src/provider.rs` — Async provider invocation. Prompts piped via stdin to providers. PID-scoped temp files for codex output. Errors print to stdout within the labeled block.
- `src/session.rs` — Register/deregister/list commands that mutate the config file.
- `src/main.rs` — Wires CLI parsing to the appropriate handler.
- `prompts/` — Default prompt templates compiled into the binary via `include_str!`.

## Design decisions

- Providers get prompts via **stdin pipe**, not CLI args, to avoid shell argument length limits.
- Temp output files include PID to prevent races between concurrent invocations.
- `git show --format= --no-notes` is used for `--commit` to handle root commits.
- The CLI uses `Option<ManagementCommand>` so that no subcommand = review mode.
- Config writes are atomic (temp file + rename).
- All prompt templates are compiled into the binary with optional config overrides.
- Stdin is always the user's per-invocation review instructions; flags provide the content to review.

## Config format

```toml
[projects.myproject]
path = "/home/user/myproject"

[projects.myproject.archetypes.security]
claude = "session-id"
codex = "session-id"
# prompt = "~/custom/security.md"   # optional override
```

Global prefix override (optional):
```toml
[global]
prefix = "~/custom/prefix.md"
```
