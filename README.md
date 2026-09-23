# review

A Rust CLI that fans out code reviews to fresh AI sessions across multiple providers (Claude Code, Codex, Grok), each primed with a specific reviewer perspective.

Built with LLMs. See [LLM.md](LLM.md).

## How it works

You define **archetypes** -- reviewer perspectives like `security`, `bugs`, `perf`, or any custom name -- as a name mapped to a priming prompt. When you run a review, you pipe your instructions via stdin. The tool starts a **fresh session** on each provider, prepends the archetype's priming prompt, and lets the agent fetch code itself. The archetype prompt carries its own grounding (role, whether it may modify files, "inspect current state") -- the tool bakes in nothing.

Every run is a clean session by design. Reviving a long-lived session on a cold prompt cache means reprocessing its entire accumulated history - which only grows - whereas a fresh session costs roughly one review's worth of tokens each time. All three providers print the new session ID above the response, so you can follow up while the cache is still warm via `--session`.

## Quick start

### 1. Initialize

```
cd /path/to/your/project
review init
```

### 2. Configure

Settings shared by every project - default providers, and the profiles naming
which model serves which tier - go once in the global
`~/.config/review/config.toml`:

```toml
[_defaults]
providers = ["codex"]

[codex.deep]
model = "gpt-6-luna"
effort = "max"
sandbox = "read-only"
```

Project-specific archetypes - a name mapped to a priming prompt - go in the
project's `.review.toml`:

```toml
[archetypes]
security = "You are a security expert for this project. Read the codebase."
bugs = "You hunt for edge cases and correctness bugs."
```

`review config` shows the effective result and which file each value came from.

### 3. Run reviews

```
echo "what does the retry loop guarantee?" | review --profile deep
echo "look for auth boundary violations" | review security
echo "check for edge cases in the parsing module" | review bugs
echo "full review please" | review all
echo "how should we handle polygon clipping?" | review competitors
```

## Usage

```
echo "<instructions>" | review [archetype[,archetype,...]]
```

Instructions are piped via stdin (required, 20KB limit). Without an archetype, stdin is sent unchanged - no priming prompt. With one, the archetype routes to the right sessions. Multiple archetypes and groups can be comma-separated:

```
echo "review please" | review security,bugs,arch
echo "review please" | review bugs,competitors
```

Duplicates are removed automatically (e.g. if a group overlaps with an explicit archetype).

### Archetypes

Archetypes are optional named priming prompts defined under `[archetypes]` (name = priming prompt), in the project's `.review.toml` or the global config; the project wins on a name clash. Any name works except the reserved ones: `all` and the subcommand names (`init`, `config`, `sessions`, `incidents`, `help`).

Use `all` to fan out to every configured archetype, or define **groups** to fan out to a named subset. Groups and individual archetypes can be mixed freely.

### Options

| Flag | Description |
|------|-------------|
| `--profile <name>` | Apply a named profile's `model`/`effort`/`sandbox`/`env` overrides. Resolved per launched provider from `[<provider>.<profile>]`, project config first, then global. |
| `--session <id>` | Resume a specific session. Sends raw stdin (no prime prepended). `--provider` is optional - the owning provider is read from the sidecar. |
| `--dry-run` | Print what would be sent instead of sending it |
| `--provider <list>` | Limit to specific providers (comma-separated) |
| `--stagger <secs>` | Seconds between each provider launch (default: 30, 0 to disable) |

Each run starts a fresh session, prepends the archetype's priming prompt (if any) to your stdin, and lets the agent fetch code itself. Providers come from `--provider`, or `[_defaults].providers` when `--provider` is omitted. A provider that is not installed on this machine fails the run before anything launches - `review config` shows which are installed.

Per-provider launch behavior:

| Provider | Args | Captures session ID? |
|----------|------|----------------------|
| claude | `--session-id <generated> --print --permission-mode dontAsk` | yes (UUID generated up front) |
| codex | `exec --sandbox read-only --json` | yes (parsed from `thread.started`) |
| grok | `--session-id <generated> --prompt-file <tmp> --output-format json --permission-mode dontAsk --sandbox read-only` | yes (echoed in the result object) |

Grok is the one provider that takes no prompt on stdin: `-p` requires a value and `-p -` is read as a one-character prompt, so `review` writes the assembled prompt to a temp file and passes `--prompt-file`. That escapes shell argument length limits the same way the stdin pipe does for the other two.

### Profiles

Profiles carry per-provider `model`, `effort`, `sandbox`, and `env` overrides, applied only when you pass `--profile`. They are `[<provider>.<profile>]` tables, and the natural home for them is the global config, so a new model release is one edit rather than one per project:

```toml
[claude.opus]
model = "Opus 4.8"
effort = "medium"
env = { ANTHROPIC_BASE_URL = "http://localhost:8787" }

[codex.implement]
model = "gpt-5.6-terra"
effort = "high"
sandbox = "workspace-write"
```

```
echo "audit the auth flow" | review security --profile opus
```

`--profile opus` resolves `[<provider>.opus]` for each launched provider: the project's `.review.toml` first, then the global config. A project profile replaces the global one of the same name **entirely** - nothing is merged field by field, so a project that overrides only `model` also drops the global profile's `sandbox`. If no file defines the profile for a launched provider, the run errors naming every file it searched.

The older host-scoped form `[<host>.<provider>.<profile>]` still parses. It applies only on the host it names, and there it beats a hostless table in the same file - which means such a table keeps overriding the global config on that host until you delete it. `review config` shows it alongside the global profile it hides.

`sandbox` takes one of `review`'s three levels -- `read-only`, `workspace-write`, `danger-full-access` -- and defaults to `read-only` when unset, so a bare `review` run can never modify files. **Claude ignores it** -- claude's `--permission-mode` is a tool-approval policy on a different axis with no honest mapping.

The levels are `review`'s own vocabulary, translated per provider at launch, because the providers do not agree on the names and grok *refuses to start* on one it cannot resolve:

| `review` level | codex `--sandbox` | grok `--sandbox` |
|---|---|---|
| `read-only` | `read-only` | `read-only` |
| `workspace-write` | `workspace-write` | `workspace` |
| `danger-full-access` | `danger-full-access` | `none` |

A value `review` does not recognise is passed through to the provider verbatim, so a custom grok profile defined in `~/.grok/sandbox.toml` still works; the provider validates its own vocabulary and fails before running a turn if the name is wrong.

### Follow-up via `--session`

`--session <id>` resumes a specific provider session and sends raw stdin - no prime prepended. The grounding is already in the session's history from the run that created it.

```
echo "what's the worst of those for a single-account user?" | \
  review bugs --provider claude --session 019deabc-0def-7000-8000-abcdef012345
```

Session IDs are provider-scoped, but you rarely need to say which: the sidecar
records who owns each session, so `--provider` is a filter over a known answer
rather than a required declaration.

```
review bugs --session 019deabc-...                          # provider inferred
review bugs --session 019deabc-... --provider claude,codex   # list is fine
```

The second form matters because those flags carry over verbatim from the fresh
run that created the session - and a codex session can only be resumed by
codex, so naming both is not ambiguous.

Two things are still errors: naming a provider the session does not belong to,
and naming several with no sidecar record to choose between them.

```
$ review bugs --session 019deabc-... --provider claude
Error: this session belongs to 'codex', but --provider says 'claude'
  A session can only be resumed by the provider that created it.
  Drop --provider to use 'codex'.
```

Other constraints:

- Bypasses config archetypes and profiles - no prime and no profile overrides are applied. The sandbox level, writable roots, model and effort are inherited from the session's own recorded run instead.
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

A turn that is running right now appears first, so a long run is
distinguishable from an idle session:

```
$ review sessions
[in flight] codex / turn in flight since 6m
       session: 019f5f70-b2ca-7590-8a61-be66d9d7cf07

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

### When codex hangs

Some codex versions can stop making progress without exiting - either after
finishing the work, or partway through it. `review` watches the rollout
transcript while a run is in flight and handles the two cases differently.

**Finished but stuck.** The final answer is already on disk and codex simply
will not exit. `review` recovers the answer from the rollout, terminates codex,
and reports normally:

```
--- codex ---
exit: -
signal: SIGTERM
captured: false
terminated by review: watchdog: stranded completion
  rollout silent for: 183s
  last rollout event: event_msg/task_complete
recovered: final answer restored from transcript (stream/-o truncated)
<the real review content>
```

This is not a timeout. It cannot fire on a run that has not already produced
its answer, however long that run takes, and it cannot fire while the rollout
is still growing.

**Wedged with nothing produced.** No answer was ever written and the rollout
has gone silent. This *is* a timeout, defaulting to 15 minutes:

```
--- codex ---
terminated by review: watchdog: stall timeout (900s silent)
  rollout silent for: 900s
  last rollout event: response_item/custom_tool_call
note: review terminated the run; the text below is whatever codex had produced by then
```

A stalled run writes a forensic bundle (see `review incidents`) and exits
non-zero, so scripts and CI see a failure rather than a silent success.

(Both digests above are abridged - the usual `turns`, `usage`, `transcript` and
`incident` lines still print alongside.)

The 15 minutes rests on codex waking itself every few minutes even while
waiting on a long tool call - not on a documented guarantee. Tune or disable it
if that ever stops holding:

```toml
[_defaults]
stall_timeout_secs = 900   # 0 disables the check entirely
```

Both are codex-only; claude has no rollout to watch, and legitimately goes
quiet while waiting on a backgrounded task.

Killing `review` (Ctrl-C or `SIGTERM`) now takes codex and everything it
spawned with it, rather than leaving it running detached.

### `review incidents`

Any codex run that looks wrong - no capture, non-zero exit, killed by a signal
- writes a forensic bundle to `~/.local/share/review/incidents/`, holding
codex's stderr (with backtraces), the raw NDJSON stream, a transcript tail, the
exact prompt, and a `meta.json` with a copy-pasteable command that replays the
run verbatim. Clean runs write nothing.

```
$ review incidents --limit 2
2026-07-31T10:22:25Z  codex 019fb7b0-cac2-7a02-b121-08af9e2bf626  exit=1  no final answer (died)  [codex-cli 0.146.0]
       /home/folk/.local/share/review/incidents/2026-07-31T10-22-25Z-codex-019fb7b0-…-f008
```

Newest first, `--limit <N>` capping the count (default 20). The verdict is one
of `recovered from transcript`, `no final answer (died)`, `completed (stream/-o
truncated)`, or `suspicious`, and the codex version is recorded because deaths
have been version-specific. The bundle path is also printed on the digest of
the run that produced it.

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

Settings resolve like a standard command-line tool: the command line, then the project's `.review.toml`, then the global config. Nothing is built in.

| File | Where | Required |
|---|---|---|
| Project | `.review.toml` in the project root, discovered by walking up to the git root. `review init` creates a starter. | Yes - it holds `[_audit]`, the project's audit id. A file with only `[_audit]` is enough. |
| Global | `$XDG_CONFIG_HOME/review/config.toml`, else `~/.config/review/config.toml` | No |

Both files take the same format; `[_audit]` is allowed only in the project file.

```toml
[archetypes]
security = "You are a security expert for this project. Read the codebase."
bugs = "You hunt for edge cases and correctness bugs."
tilemaker = "You are a tilemaker maintainer weighing tradeoffs."
tippecanoe = "You are a tippecanoe maintainer weighing tradeoffs."

[_defaults]
providers = ["claude", "codex"]    # used when --provider is omitted
stall_timeout_secs = 900           # codex-only; 0 disables. See "When codex hangs".

[_groups]
sweep = ["security", "bugs"]
competitors = ["tilemaker", "tippecanoe"]

# Named profiles: per-provider model/effort/sandbox/env overrides, applied via
# --profile. Scoped by provider . profile.
[claude.opus]
model = "Opus 4.8"
effort = "medium"
env = { ANTHROPIC_BASE_URL = "http://localhost:8787" }

[codex.high]
model = "gpt-6-luna"
effort = "high"
```

How the layers combine:

- **Archetypes and groups** are the union of both files; the project wins on a name clash, including a project archetype against a global group of the same name. Within one file, a group and an archetype may not share a name. A project group may name a global archetype; a global group may only name global archetypes, since the global file is read in every project.
- **`[_defaults]` keys** come from the first file that sets them. An explicit `providers = []` in the project counts as set.
- **Profiles** come from the first definition found, and win whole. Lookup order: the project's host-scoped table, its hostless table, then the same two in the global file.

### `review config`

Prints the effective configuration for the current directory, computed by the same resolver a run uses: the files consulted, the default providers and whether each provider is installed on this machine, every archetype and group with its source, and every profile with the table it came from and any definitions it shadows. Env vars show by name only.

```
review config          # for reading
review config --json   # for scripts and orchestrators
```

Anything that used to read `.review.toml` to find out what is available should call this instead - the project file no longer tells the whole story. Outside a project it shows the global layer and provider availability on their own.

### Upgrading existing files

Existing `.review.toml` files keep working, including legacy host tables, with two exceptions that now fail to parse:

- A profile containing a key `review` does not know - usually a typo. A project profile replaces the global one whole, so a typo that parsed as an empty profile would silently discard the global settings.
- An archetype or group named `config` or `incidents`, which are now subcommands.

### Providers

| Provider | Binary | Non-interactive | Resume | Model flag |
|----------|--------|----------------|--------|------------|
| claude | `claude` | `--print` | `--resume <id>` | `--model <name>` |
| codex | `codex` | `exec` | `exec resume <id>` | `-m <model>` |
| grok | `grok` | `--prompt-file` | `--resume <id>` | `-m <model>` |

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

A global file lock (`/tmp/review.lock`) serializes provider **launches**, not
whole runs. An invocation holds it until its providers have been spawned (the
fan-out path through its staggered launches, a `--session` resume through the
single spawn), then releases it and lets the runs proceed concurrently.
Additional invocations queue and wait for the launch window only.

That distinction is deliberate. Holding the lock for a run's full duration
meant one wedged provider froze every other `review` on the machine
indefinitely - a hung run should cost you that run, not the tool.

Note: the lock is shared across all users on the machine. On shared dev
machines, one user's launch will briefly block another's.

## License

[MIT](LICENSE)
