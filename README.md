# review

A Rust CLI that fans out code reviews to persistent AI sessions across multiple providers (Claude Code, Codex, Kilo, OpenCode), each cultivated with a specific reviewer perspective.

Built with LLMs. See [LLM.md](LLM.md).


## How it works

You configure **archetypes** -- reviewer perspectives like `security`, `bugs`, `perf`, or any custom name -- each backed by long-lived sessions in one or more AI providers. When you run a review, you pipe your instructions via stdin. The tool sends them to all providers for that archetype in parallel. Sessions are persistent — the agents already have project context from previous interactions.

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

Archetypes are named reviewer sessions defined in `.review.toml`. Any name works — use whatever fits your project.

Use `all` to fan out to every configured archetype, or define **groups** to fan out to a named subset.

### Options

| Flag | Description |
|------|-------------|
| `--anchor` | Prepend grounding prefix to stdin |
| `--dry-run` | Print what would be sent instead of sending it |
| `--provider <list>` | Limit to specific providers (comma-separated) |

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
codex = { session = "session-jkl012", model = "o3" }
kilo = { session = "session-mno345", model = "anthropic/claude-sonnet-4.6" }
opencode = { session = "session-pqr678", model = "openai/gpt-5" }

[tilemaker.myhostname]
claude = "session-stu901"

[tippecanoe.myhostname]
claude = "session-vwx234"

[_groups]
sweep = ["security", "bugs"]
competitors = ["tilemaker", "tippecanoe"]
```

Session IDs are scoped by hostname, so the same `.review.toml` works across machines with different sessions.

Provider entries can be a simple session ID string or a table with session + model:

```toml
claude = "session-id"                                        # default model
codex = { session = "session-id", model = "o3" }             # explicit model
kilo = { session = "session-id", model = "anthropic/claude-sonnet-4.6" }
```

### Providers

| Provider | Binary | Non-interactive | Resume | Model flag |
|----------|--------|----------------|--------|------------|
| claude | `claude` | `--print` | `--resume <id>` | `--model <name>` |
| codex | `codex` | `exec` | `exec resume <id>` | `-m <model>` |
| kilo | `kilo` | `run` | `run -s <id>` | `-m <provider/model>` |
| opencode | `opencode` | `run` | `run -s <id>` | `-m <provider/model>` |

Use `--provider` to limit which providers run:

```
echo "just claude" | review bugs --provider claude
echo "claude and kilo" | review bugs --provider claude,kilo
```

### Groups

Groups fan out to multiple archetypes with a single command:

```
echo "how to handle clipping?" | review competitors
echo "full sweep" | review sweep
```

Define groups in the `[_groups]` table. Group names must not conflict with archetype names. `all` is reserved and runs every configured archetype.

## Concurrency

A global file lock (`/tmp/review.lock`) ensures only one `review` invocation runs providers at a time. Additional invocations queue and wait automatically. This prevents thrashing when multiple projects or terminals launch reviews simultaneously.

Note: the lock is shared across all users on the machine. On shared dev machines, one user's review will block another's.

## License

[MIT](LICENSE)
