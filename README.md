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

Add session IDs to `.review.toml`. Either edit the file manually:

```toml
[security.myhostname]
claude = "your-claude-session-id"
codex = "your-codex-session-id"

[bugs.myhostname]
claude = "your-claude-session-id"
```

Or use `review prime` to create sessions and register them automatically:

```
echo "You are a security expert for this project. Read the codebase." | review prime security --provider claude,codex
```

`review prime` creates new sessions, sends the priming prompt, and writes the session IDs to `.review.toml` automatically. The prompt is stored under `[_prime]` so if a session later breaks you can re-prime with a fresh session without retyping it:

```
review prime security --provider claude    # stdin omitted; reuses stored prompt
```

Re-priming replaces the stale session ID in place. Manually-added `model` and `env` overrides on a provider entry are preserved.

### 3. Run reviews

```
echo "look for auth boundary violations" | review security
echo "check for edge cases in the parsing module" | review bugs
echo "full review please" | review all
echo "how should we handle polygon clipping?" | review competitors
```

## Usage

```
echo "<instructions>" | review <archetype[,archetype,...]>
```

Instructions are piped via stdin (required, 20KB limit). The archetype routes to the right sessions. Multiple archetypes and groups can be comma-separated:

```
echo "review please" | review security,bugs,arch
echo "review please" | review bugs,competitors
```

Duplicates are removed automatically (e.g. if a group overlaps with an explicit archetype).

### Archetypes

Archetypes are named reviewer sessions defined in `.review.toml`. Any name works — use whatever fits your project.

Use `all` to fan out to every configured archetype, or define **groups** to fan out to a named subset. Groups and individual archetypes can be mixed freely.

### Options

| Flag | Description |
|------|-------------|
| `--anchor` | Prepend grounding prefix to stdin |
| `--oneshot` | Skip session resume; start fresh each call and prepend the stored prime prompt. Implies `--anchor`. |
| `--dry-run` | Print what would be sent instead of sending it |
| `--provider <list>` | Limit to specific providers (comma-separated) |
| `--stagger <secs>` | Seconds between each provider launch (default: 30, 0 to disable) |

By default, stdin goes directly to the provider sessions. Use `--anchor` for the first review in a session or to re-anchor a stale session.

### Oneshot mode

`--oneshot` skips session resume entirely. Each call starts fresh providers, prepends the priming prompt stored under `[_prime].<archetype>`, and lets the agent fetch code itself.

```
echo "check the new auth flow" | review --oneshot security,bugs
```

Use this when reviews happen far enough apart that the prompt cache has expired (default 5min, up to 1h with the right env vars). Resuming a long-lived session means reprocessing the entire accumulated prefix on every wake — expensive in API tokens and corrosive to subscription rate-limit windows for once-a-day usage. Oneshot keeps the prefix small and predictable.

`.review.toml` still drives provider selection and `model`/`env` overrides; the session IDs are simply unused. If no `[_prime]` entry exists for the archetype, the prime block is silently skipped.

Per-provider behavior in oneshot mode:

| Provider | Oneshot args |
|----------|--------------|
| claude | `--print --permission-mode dontAsk --no-session-persistence` |
| codex | `exec --sandbox read-only --ephemeral` |
| kilo | `run --auto` (auto-approve permissions; sessions don't carry pre-approval) |
| opencode | `run` (no auto-approve flag — may prompt; use the regular session flow if it does) |

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

Provider entries can be a simple session ID string or a table with session, model, and env:

```toml
claude = "session-id"                                        # default model
codex = { session = "session-id", model = "o3" }             # explicit model
kilo = { session = "session-id", model = "anthropic/claude-sonnet-4.6" }

# environment variables passed to the provider process
claude = { session = "session-id", env = { ANTHROPIC_BASE_URL = "http://localhost:8787" } }
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

## Rate limits and staggering

Provider APIs enforce rate limits across multiple dimensions — requests per minute (RPM), input tokens per minute (ITPM), and rolling usage quotas. The exact limits are not publicly documented for subscription plans, but in practice, firing multiple provider sessions simultaneously (e.g. a group of 5 claude sessions) will trigger RPM limits.

A single Claude Code invocation generates 8-12 internal API calls through its tool-use architecture. Five concurrent sessions means 40-60 API calls hitting at once — enough to blow past most RPM budgets.

To avoid this, provider launches are staggered by default. The first provider starts immediately; each subsequent one waits 30 seconds. All run concurrently once launched.

```
echo "review" | review sweep                    # 30s stagger (default)
echo "review" | review sweep --stagger 10       # 10s stagger
echo "review" | review sweep --stagger 0        # no stagger (risk rate limits)
```

If you're hitting rate limits, increase the stagger. If you're only running 1-2 providers, `--stagger 0` is fine.

## Concurrency

A global file lock (`/tmp/review.lock`) ensures only one `review` invocation runs providers at a time. Additional invocations queue and wait automatically. This prevents thrashing when multiple projects or terminals launch reviews simultaneously.

Note: the lock is shared across all users on the machine. On shared dev machines, one user's review will block another's.

## License

[MIT](LICENSE)
