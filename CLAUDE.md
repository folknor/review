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

A Rust CLI (`review`) that fans out code reviews to persistent AI sessions across multiple providers (Claude Code, Codex). It's a prompt builder that knows about sessions — the agents fetch code themselves.

Fixed archetypes: security, bugs, perf, arch. Per-project config via `.review.toml` (YAML frontmatter for host-scoped session IDs, markdown `## headings` for archetype prompts). Custom archetypes and groups also supported.

## Build and run

```
cargo build
cargo install --path .
review init
echo "review for issues" | review security
```

Single binary crate, no workspace.

## Architecture

- `src/cli.rs` — Clap CLI. Archetype is a positional arg, `init` is the only subcommand.
- `src/config.rs` — Parses `.review.toml` in cwd. YAML frontmatter for host-scoped sessions (archetype → hostname → provider), `_groups` for named archetype sets. Uses `yaml-front-matter` and `gethostname` crates.
- `src/input.rs` — Reads stdin instructions (required, 20KB limit).
- `src/prompt.rs` — Assembles: compiled prefix + archetype prompt + stdin instructions. Built-in prompts for security, bugs, perf, arch.
- `src/provider.rs` — Async provider invocation. Prompts piped via stdin. Claude uses `--permission-mode dontAsk`, Codex uses `--sandbox read-only`.
- `src/main.rs` — Wires CLI to config, prompt assembly, and provider dispatch.
- `prompts/` — Default prompt templates compiled into the binary via `include_str!`.

## Design decisions

- The tool is a **prompt builder**, not a content fetcher. Flags like `--staged` add context hints; agents fetch the actual code themselves.
- Providers get prompts via **stdin pipe**, not CLI args, to avoid shell argument length limits.
- Claude runs with `--permission-mode dontAsk` (uses pre-approved permissions, rejects interactive prompts). Codex runs with `--sandbox read-only`.
- All prompt templates are compiled into the binary. `.review.md` headings override built-in archetype prompts.
- No global config — `.review.toml` lives in the project root.

## Config format

```toml
[security.myhostname]
claude = "session-id"
codex = "session-id"

[bugs.myhostname]
claude = "session-id"

[_groups]
sweep = ["security", "bugs"]
```
