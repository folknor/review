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

Fixed archetypes: security, bugs, perf, arch. Per-project config via `.review.md` (YAML frontmatter for session IDs, markdown headings for archetype prompts).

## Build and run

```
cargo build
cargo install --path .
review init
echo "review for issues" | review security --staged
```

Single binary crate, no workspace.

## Architecture

- `src/cli.rs` — Clap CLI. Archetypes are subcommands (security, bugs, perf, arch, all). `init` creates a starter `.review.md`.
- `src/config.rs` — Parses `.review.md` in cwd. YAML frontmatter for sessions, markdown `# headings` for archetype prompts. Uses `yaml-front-matter` crate.
- `src/input.rs` — Builds context line from flags (e.g. "You are reviewing staged changes."). Reads stdin instructions (required, 20KB limit).
- `src/prompt.rs` — Assembles: compiled prefix + archetype prompt (from .review.md or built-in) + context line + stdin instructions. Built-in prompts for security, bugs, perf, arch.
- `src/provider.rs` — Async provider invocation. Prompts piped via stdin. Claude uses `--permission-mode plan`, Codex uses `--sandbox read-only`.
- `src/main.rs` — Wires CLI to config, prompt assembly, and provider dispatch.
- `prompts/` — Default prompt templates compiled into the binary via `include_str!`.

## Design decisions

- The tool is a **prompt builder**, not a content fetcher. Flags like `--staged` add context hints; agents fetch the actual code themselves.
- Providers get prompts via **stdin pipe**, not CLI args, to avoid shell argument length limits.
- Claude runs in `plan` mode (read-only). Codex runs with `--sandbox read-only`.
- All prompt templates are compiled into the binary. `.review.md` headings override built-in archetype prompts.
- No global config — `.review.md` lives in the project root.

## Config format

```markdown
---
security:
  claude: "session-id"
  codex: "session-id"
bugs:
  claude: "session-id"
---

# security

Custom security review instructions here.

# bugs

Custom bugs review instructions here.
```
