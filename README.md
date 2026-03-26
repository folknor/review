# review

A Rust CLI that fans out code reviews to persistent AI sessions across multiple providers (Claude Code, Codex), each cultivated with a specific reviewer perspective.

## How it works

You configure **archetypes** -- reviewer perspectives like `security`, `bugs`, `perf`, `arch`, or any custom name -- each backed by long-lived sessions in one or more AI providers. When you run a review, you pipe your instructions via stdin. The tool sends them to all providers for that archetype in parallel. Sessions are persistent — the agents already have project context from previous interactions.

## Install

```
cargo install --path .
```

Requires Rust 1.92+.

## Quick start

### 1. Initialize

```
cd /path/to/your/project
review init
```

### 2. Add session IDs

Edit `.review.toml` and add your provider session IDs:

```toml
[security.myhostname]
claude = "your-claude-session-id"
codex = "your-codex-session-id"

[bugs.myhostname]
claude = "your-claude-session-id"
```

### 3. Run reviews

```
echo "look for auth boundary violations" | review security
echo "check for edge cases in the parsing module" | review bugs
echo "full review please" | review all
echo "how should we handle polygon clipping?" | review competitors
```

## Usage

```
echo "<instructions>" | review <archetype>
```

Instructions are piped via stdin (required, 20KB limit). The archetype routes to the right sessions.

### Archetypes

Built-in archetypes have tailored prompts (used with `--anchor`):

| Archetype | Focus |
|-----------|-------|
| `security` | Auth boundaries, injection, secrets, trust assumptions |
| `bugs` | Logic errors, edge cases, error handling, crashes |
| `perf` | Allocations, complexity, hot paths, async blocking |
| `arch` | Coupling, abstractions, API design, consistency |

Custom archetype names are also supported — any name works.

Use `all` to fan out to every configured archetype, or define **groups** to fan out to a named subset.

### Options

| Flag | Description |
|------|-------------|
| `--anchor` | Prepend grounding prefix and archetype prompt to stdin |
| `--dry-run` | Print what would be sent instead of sending it |

By default, stdin goes directly to the provider sessions. Use `--anchor` for the first review in a session or to re-anchor a stale session.

### Output format

```
--- claude ---
<review content>

--- codex ---
<review content>
```

When using `all` or groups, archetype headers are added:

```
=== security ===

--- claude ---
<review content>

=== bugs ===

--- claude ---
<review content>
```

## Configuration

Per-project `.review.toml` in the project root (discovered by walking up to the git root). Run `review init` to create a starter.

```toml
[security.myhostname]
claude = "session-abc123"
codex = "session-def456"

[bugs.myhostname]
claude = "session-ghi789"

[tilemaker.myhostname]
claude = "session-jkl012"

[tippecanoe.myhostname]
claude = "session-mno345"

[_groups]
sweep = ["security", "bugs"]
competitors = ["tilemaker", "tippecanoe"]
```

Session IDs are scoped by hostname, so the same `.review.toml` works across machines with different sessions.

### Groups

Groups fan out to multiple archetypes with a single command:

```
echo "how to handle clipping?" | review competitors
echo "full sweep" | review sweep
```

Define groups in the `[_groups]` table. Group names must not conflict with archetype names. `all` is reserved and runs every configured archetype.

## Providers

### Claude Code

```
claude --resume <session-id> --print --permission-mode dontAsk
```

Runs in `dontAsk` mode (uses pre-approved permissions, rejects interactive prompts). Prompt piped via stdin, output captured from stdout.

### Codex

```
codex exec --sandbox read-only resume <session-id> -o <file>
```

Runs in read-only sandbox. Prompt piped via stdin, output captured from the `-o` file.

Both providers run in parallel. If one fails, the other's results are still shown. Providers whose binaries aren't installed are skipped with a warning.
