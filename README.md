# review

A Rust CLI that fans out code reviews to persistent AI sessions across multiple providers (Claude Code, Codex), each cultivated with a specific reviewer perspective.

## How it works

You configure **archetypes** -- named reviewer perspectives like `security`, `bugs`, `perf`, `arch` -- each backed by long-lived sessions in one or more AI providers. When you run a review, you pipe your instructions via stdin and specify what to review with a flag. The tool assembles a prompt (grounding prefix + archetype prompt + your instructions + content), sends it to all providers for that archetype in parallel, and prints the labeled results.

Sessions carry project familiarity from previous interactions. A grounding prefix on every invocation tells the reviewer not to trust its memory of the codebase, only what's explicitly provided.

## Install

```
cargo install --path .
```

Requires Rust 1.92+.

## Quick start

### 1. Create the config

```toml
# ~/.config/review/config.toml

[projects.myproject]
path = "/home/you/myproject"
```

### 2. Register sessions

```
cd /home/you/myproject
review register security --claude <session-id>
review register security --codex <session-id>
review register bugs --claude <session-id>
```

### 3. Run reviews

```
echo "look for auth boundary violations" | review security --staged
echo "check for edge cases" | review bugs --branch
echo "review this spec for gaps" | review arch --document spec.md
echo "full review" | review all --unstaged
```

## Usage

```
echo "<instructions>" | review <archetype> <input-source>
```

Instructions are piped via stdin (required, 20KB limit). An input source flag is always required.

### Built-in archetypes

| Archetype | Focus |
|-----------|-------|
| `security` | Auth boundaries, injection, secrets, trust assumptions |
| `bugs` | Logic errors, edge cases, error handling, crashes |
| `perf` | Allocations, complexity, hot paths, async blocking |
| `arch` | Coupling, abstractions, API design, consistency |

Custom archetypes are also supported -- any name works. Built-in archetypes include tailored prompts; custom ones use a generic fallback. All prompts are overridable in config.

Use `all` to fan out to every configured archetype.

### Input sources

| Flag | Description |
|------|-------------|
| `--unstaged` | Working tree changes (`git diff`) |
| `--staged` | Staged changes (`git diff --cached`) |
| `--commit <hash>` | Diff of a specific commit |
| `--range <a..b>` | Diff across a commit range |
| `--branch` | Full branch diff against default branch |
| `--document <path>` | A file reviewed as-is |

### Session management

```
review register <archetype> --claude <session-id>
review register <archetype> --codex <session-id>
review deregister <archetype>               # remove entirely
review deregister <archetype> --claude      # remove just claude
review list                                 # current project
review list --all                           # all projects
```

### Output format

```
--- claude ---
<review content>

--- codex ---
<review content>
```

When using `all`, archetype headers are added:

```
=== security ===

--- claude ---
<review content>

=== bugs ===

--- claude ---
<review content>
```

## Configuration

Single config file at `~/.config/review/config.toml`.

```toml
[projects.myproject]
path = "/home/you/myproject"

[projects.myproject.archetypes.security]
claude = "session-abc123"
codex = "session-def456"
# prompt = "~/custom/security-prompt.md"    # optional override

[global]
# prefix = "~/custom/prefix.md"            # optional override
```

Project resolution is prefix-based -- running `review` from any subdirectory of a registered project path matches that project. Nested project paths resolve to the most specific match.

## Providers

### Claude Code

Uses `claude --resume <session-id> --print` in non-interactive mode. Prompt piped via stdin.

### Codex

Uses `codex exec resume <session-id> -o <file>`. Prompt piped via stdin, output captured from the `-o` file.

Both providers run in parallel. If one fails, the other's results are still shown.
