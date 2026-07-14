# review

A Rust CLI that fans out code reviews to fresh AI sessions across multiple providers (Claude Code, Codex), each primed with a specific reviewer perspective.

Built with LLMs. See [LLM.md](LLM.md).

## How it works

You define **archetypes** -- reviewer perspectives like `security`, `bugs`, `perf`, or any custom name -- as a name mapped to a priming prompt. When you run a review, you pipe your instructions via stdin. The tool starts a **fresh session** on each provider, prepends the archetype's priming prompt, and lets the agent fetch code itself. The archetype prompt carries its own grounding (role, whether it may modify files, "inspect current state") -- the tool bakes in nothing.

Every run is a clean session by design. Reviving a long-lived session on a cold prompt cache means reprocessing its entire accumulated history - which only grows - whereas a fresh session costs roughly one review's worth of tokens each time. For claude and codex the new session ID is printed above the response, so you can follow up while the cache is still warm via `--session`.

## Quick start

### 1. Initialize

```
cd /path/to/your/project
review init
```

### 2. Define archetypes

Add archetypes to `.review.toml` - a name mapped to a priming prompt - and list the providers to fan out to:

```toml
[archetypes]
security = "You are a security expert for this project. Read the codebase."
bugs = "You hunt for edge cases and correctness bugs."

[_defaults]
providers = ["claude", "codex"]
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
echo "<instructions>" | review <archetype[,archetype,...]>
```

Instructions are piped via stdin (required, 20KB limit). The archetype routes to the right sessions. Multiple archetypes and groups can be comma-separated:

```
echo "review please" | review security,bugs,arch
echo "review please" | review bugs,competitors
```

Duplicates are removed automatically (e.g. if a group overlaps with an explicit archetype).

### Archetypes

Archetypes are named reviewer personas defined under `[archetypes]` in `.review.toml` (name = priming prompt). Any name works - use whatever fits your project.

Use `all` to fan out to every configured archetype, or define **groups** to fan out to a named subset. Groups and individual archetypes can be mixed freely.

### Options

| Flag | Description |
|------|-------------|
| `--profile <name>` | Apply a named profile's `model`/`effort`/`env` overrides. Resolved per launched provider from `[<host>.<provider>.<profile>]`. |
| `--session <id>` | Resume a specific session. Sends raw stdin (no prime prepended). Requires a single `--provider`. |
| `--dry-run` | Print what would be sent instead of sending it |
| `--provider <list>` | Limit to specific providers (comma-separated) |
| `--stagger <secs>` | Seconds between each provider launch (default: 30, 0 to disable) |

Each run starts a fresh session, prepends the archetype's priming prompt to your stdin, and lets the agent fetch code itself. Providers come from `--provider`, or `[_defaults].providers` when `--provider` is omitted.

Per-provider launch behavior:

| Provider | Args | Captures session ID? |
|----------|------|----------------------|
| claude | `--session-id <generated> --print --permission-mode dontAsk` | yes (UUID generated up front) |
| codex | `exec --sandbox read-only --json` | yes (parsed from `thread.started`) |

### Profiles

Profiles carry per-provider `model`, `effort`, `sandbox`, and `env` overrides, applied only when you pass `--profile`. They are scoped by host, provider, and profile name so the same name can mean different settings on different machines (e.g. a local proxy `ANTHROPIC_BASE_URL` that differs per host):

```toml
[myhostname.claude.opus]
model = "Opus 4.8"
effort = "medium"
env = { ANTHROPIC_BASE_URL = "http://localhost:8787" }

[myhostname.codex.implement]
model = "gpt-5.6-terra"
effort = "high"
sandbox = "workspace-write"
```

```
echo "audit the auth flow" | review security --profile opus
```

`--profile opus` resolves `[<host>.<provider>.opus]` for each launched provider and applies its overrides. If any launched provider lacks that profile table, the run errors naming the missing `[host.provider.profile]`.

`sandbox` maps to codex's `--sandbox` (`read-only`, `workspace-write`, `danger-full-access`); it defaults to `read-only` when unset, so a bare `review` run can never modify files. It is **codex-only** -- claude's `--permission-mode` is a tool-approval policy on a different axis with no honest mapping, so claude ignores `sandbox`.

### Follow-up via `--session`

`--session <id>` resumes a specific provider session and sends raw stdin - no prime prepended. The grounding is already in the session's history from the run that created it.

```
echo "what's the worst of those for a single-account user?" | \
  review bugs --provider claude --session 019deabc-0def-7000-8000-abcdef012345
```

Constraints:

- Requires exactly one `--provider`. Session IDs are provider-scoped.
- Bypasses `.review.toml` entirely - no profile overrides are applied.
- Validation of the session ID is delegated to the provider; an unknown ID produces a provider-specific error, not a `review` error.

### Sessions sidecar log

Each run that captures a session ID and each `--session` resume appends a JSONL row to `~/.local/share/review/sessions.jsonl` (or `sessions-private.jsonl` when `audit.private = true`). Rows carry:

- `timestamp` (UTC), `epoch_secs`, `project` (root path), `hostname`
- `audit_id`, `provider`, `archetype`, `session_id`
- `kind` - `"run"` for fresh-session creation events, `"session"` for resume touches
- `model`, `env_keys` (env-var *names* only - values are not recorded so secrets don't leak through the sidecar)
- `operator_prompt` (raw stdin), `assembled_prompt` (what the provider actually saw)
- `response` or `error`
- `review_version`

The sidecar drives two things:

**1. Cache-age gate on `--session`.** When you resume, `review` looks up the last touch and prints how long it's been:

```
$ echo "follow up" | review bugs --provider claude --session 019deabc-...
session last touched 14m ago
--- claude ---
<response>
```

`--session` is the *warm* follow-up path. If the session last ended over 55
minutes ago - past the longest realistic prompt-cache TTL - the cache is cold,
and resuming would reprocess the whole session prefix at full cost. So `review`
**refuses** it and tells you to do a fresh run with restated context instead:

```
$ echo "follow up" | review bugs --provider claude --session 019deabc-...
Error: session last touched 1h17m ago - its prompt cache is cold.
  Resuming would reprocess the whole session prefix at full cost.
  Start a fresh run with restated context instead of `--session`.
```

If there's no sidecar record for the session, the age is unknown and the resume
proceeds.

**2. `review sessions` listing.** Aggregates by `session_id` and shows recent sessions for the current project (or `--all` projects), most recent first:

```
$ review sessions
[14m] claude / bugs (run) / 3 touches
       session: 019deabc-0def-7000-8000-abcdef012345
       opened:  review the new sync code

[1h12m] codex / security (run) / 1 touch
       session: 019d0123-...
       opened:  check OAuth handling on the IMAP path
```

Each block shows the age since the last touch, the provider/archetype/touch count, the session ID (copy-paste into `--session <id>`), and the operator prompt that opened the session. `--limit <N>` caps the row count (default 20).

For ad-hoc queries beyond what `review sessions` exposes, the JSONL works directly with `jq` and `grep`.

### Output format

```
--- claude ---
<review content>

--- codex ---
<review content>
```

Codex runs (both fresh and `--session` follow-ups) also print a digest above
the message, distilled from its `--json` stream plus the
`-o`/`--output-last-message` backstop:

```
--- codex ---
session: 019f5f70-b2ca-7590-8a61-be66d9d7cf07
exit: 0
captured: true
turns: 1
usage: input=12244 cached=10112 output=5 reasoning=0
<review content>
```

`captured: true` means the final message came from the authoritative `-o` file
(which survives a frozen or halted stream); `false` means we fell back to the
last streamed message. Non-JSON log lines codex interleaves (ERROR/WARN,
apply_patch dumps) are printed between the digest and the message.

When a run looks wrong (`captured: false`, non-zero exit, or a signal), the
digest also reads codex's on-disk transcript and appends a post-mortem: whether
the turn reached `task_complete`, whether a `stream_error` occurred, the last
event, and the last in-flight tool call (what was running when it stopped).
Clean runs skip this.

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
[archetypes]
security = "You are a security expert for this project. Read the codebase."
bugs = "You hunt for edge cases and correctness bugs."
tilemaker = "You are a tilemaker maintainer weighing tradeoffs."
tippecanoe = "You are a tippecanoe maintainer weighing tradeoffs."

[_defaults]
providers = ["claude", "codex"]    # used when --provider is omitted

[_groups]
sweep = ["security", "bugs"]
competitors = ["tilemaker", "tippecanoe"]

# Named profiles: per-provider model/effort/env overrides, applied via --profile.
# Scoped by host . provider . profile.
[myhostname.claude.opus]
model = "Opus 4.8"
effort = "medium"
env = { ANTHROPIC_BASE_URL = "http://localhost:8787" }

[myhostname.codex.high]
model = "o3"
effort = "high"
```

An archetype is just a name mapped to a priming prompt - no session, no host binding. Profiles are what's host-scoped, so the same profile name can carry different `model`/`env` on different machines.

### Providers

| Provider | Binary | Non-interactive | Resume | Model flag |
|----------|--------|----------------|--------|------------|
| claude | `claude` | `--print` | `--resume <id>` | `--model <name>` |
| codex | `codex` | `exec` | `exec resume <id>` | `-m <model>` |

Use `--provider` to limit which providers run:

```
echo "just claude" | review bugs --provider claude
echo "claude and codex" | review bugs --provider claude,codex
```

### Groups

Groups fan out to multiple archetypes with a single command:

```
echo "how to handle clipping?" | review competitors
echo "full sweep" | review sweep
```

Define groups in the `[_groups]` table. Group names must not conflict with archetype names. `all` is reserved and runs every configured archetype.

## Rate limits and staggering

Provider APIs enforce rate limits across multiple dimensions - requests per minute (RPM), input tokens per minute (ITPM), and rolling usage quotas. The exact limits are not publicly documented for subscription plans, but in practice, firing multiple provider sessions simultaneously (e.g. a group of 5 claude sessions) will trigger RPM limits.

A single Claude Code invocation generates 8-12 internal API calls through its tool-use architecture. Five concurrent sessions means 40-60 API calls hitting at once - enough to blow past most RPM budgets.

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
