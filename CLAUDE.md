# CLAUDE.md

## Rules

### Bash
- Never use sed, find, awk, or complex bash commands
- Never chain commands with &&
- Never chain commands with ;
- Never pipe commands with |
- Never read or write from /tmp. All data lives in the project.

### Memory rules
Do not use your Memory functionality. Update CLAUDE.md instead. This project is developed across several hosts and several users. Memories do not transfer across hosts or users. CLAUDE.md does.

### Bash rules
- Never capture stdout into env vars (`UUID=$(...)`).
- Never run raw cargo, curl, pkill. Use `brokkr`. **This means never, including
  during iteration.** `cargo build` / `cargo test` while working, "just to check
  quickly", is the same violation as shipping with them - `brokkr` is the only
  supported entry point, and reaching past it means running a different build
  configuration from the one that gates the commit. The mapping is total; there
  is no case that needs raw cargo:

  | instead of | run |
  |---|---|
  | `cargo build` / `cargo clippy` / `cargo test` | `brokkr check` (gremlins + clippy + tests, the full gate) |
  | `cargo test <name>` | `brokkr test <name>` |
  | `cargo test <name>` repeatedly (flaky hunt) | `brokkr test <name> -N <count>` |
  | `cargo run -- <args>` | `brokkr run -- <args>` |
  | `cargo fmt` | `brokkr fmt` |
  | `cargo install --path .` | `brokkr install` |

- `brokkr check` is the gate; run it before every commit, not just at the end.
- `brokkr test <NAME>` is a substring filter over the package's unit *and*
  integration tests, release profile by default (`--debug` for dev). It always
  passes `--include-ignored --nocapture --test-threads=1`.
- **There is a 20-second per-test watchdog**, shared by `brokkr check`'s test
  phase and `brokkr test`. A test that runs longer is killed and reported as
  hung. `brokkr test --timeout <SECS>` raises it to at most 280s, and only for a
  name that matches exactly one test. This is why fixture timings in
  `src/provider_tests.rs` are injectable rather than real-world durations - a
  test that waited out the production 10s `SIGKILL` escalation would sit right
  under the ceiling and eventually trip it on a loaded machine.
- `brokkr man` lists bundled docs (`man check`, `man config`, `man clippy`,
  `man run`, `man results`, ...). Read those rather than guessing at flags.
- Waiting on a long command: just run it and stop. It wakes you on exit. Never
  poll it with `sleep`, `until`, or a watch loop.

### git commit rules
- Always run `brokkr fmt` before a commit.
- Never commit markdown changes and/or results.db alone. Bundle them with upcoming code commits.
- When committing other changes: always tag along brokkrs 'results.db' and markdown files if dirty.
- Write substantive engineering-focused commit messages.
- Has `Cargo.lock` changed? Commit it.
- Never `git push` unless the user explicitly asks. Stop after the commit.

### Subagents- Do NOT use worktree isolation for parallel agents. Worktrees create merge conflicts that silently drop agent work. Instead, launch agents in the same tree with strict file ownership - zero overlap.

## What this project is

A Rust CLI (`review`) that fans out code reviews to fresh AI sessions across providers (Claude Code, Codex). It's a prompt builder - the agents fetch code themselves. Each run starts a clean session primed with an archetype's prompt (see Design decisions for why fresh beats long-lived).

Per-project config via `.review.toml`: archetypes (name → priming prompt), groups, default providers, and host-scoped `--profile` overrides. Comma-separated archetypes/groups can be mixed freely, with deduplication.

## Build and run

`brokkr` is the only entry point - never raw `cargo` (see Bash rules for the
full mapping and the 20s per-test watchdog).

```
brokkr check                    # the gate: gremlins + clippy + tests
brokkr test <name>              # one test by substring; -N <n> to repeat
brokkr run -- <args>            # run the binary
brokkr fmt                      # before every commit
brokkr man check                # bundled docs for the pipeline
```

```
brokkr check
review init
echo "review for issues" | review security
```

Single binary crate, no workspace.

## Architecture

- `src/cli.rs` - Clap CLI. Archetype is a positional arg; `init` and `sessions` are subcommands.
- `src/config.rs` - Parses `.review.toml` in cwd. `[archetypes]` maps name → priming prompt; `[_groups]` names archetype sets; `[_defaults].providers` is the provider list when `--provider` is omitted; every other top-level table is a hostname carrying `[<host>.<provider>.<profile>]` profiles (model/effort/sandbox/env/`config`, the last being verbatim codex `-c` overrides). Parsed by peeling reserved sections off a `toml::Table` and treating the rest as hosts (serde `flatten` can't coexist with the sibling `archetypes` field). Also holds `generate_uuid`/`generate_short_id`. Uses `toml` and `gethostname` crates.
- `src/input.rs` - Reads stdin instructions (required, 20KB limit).
- `src/prompt.rs` - `assemble`: archetype prompt + `\n\n` + stdin. No baked-in grounding; the archetype prompt owns role and read/write intent. Special case: a **bare single-line slash-command** prime (e.g. `goal = "/goal "`) inlines the stdin onto the command's line (`/goal <stdin>`) instead of the blank-line separator, because the claude/codex harness only treats same-line text as the slash command's argument - a blank-line gap fires the command argument-less and orphans the operator's goal below.
- `src/provider.rs` - Async provider invocation for claude and codex only. Prompts piped via stdin. Each run (oneshot=true) starts a fresh persistable session (claude `--session-id <generated UUID> --permission-mode dontAsk`, codex `exec --json` to capture `thread_id`); `--session` resume passes oneshot=false. Profile settings applied: `model` (claude `--model`, codex `-m`), `effort` (claude `--effort`, codex `-c model_reasoning_effort=`), `sandbox` (codex-only: `--sandbox`, default `read-only`; claude ignores it - its `--permission-mode` is a different axis with no honest mapping), `env`. Claude/codex emit the new session ID via `ProviderResult.session_id`. Both codex paths (fresh + `--session` resume) share `run_codex_json`, which streams `--json` and takes `-o`, then distills a `Digest` (exit/signal, `captured` from the `-o`/`--output-last-message` backstop, `recovered_from_transcript`, turn count, summed token usage, non-JSON log lines, optional transcript forensics) from the NDJSON (`thread.started`/`item.completed` agent_message/`turn.completed`); it does not bail on a non-zero exit so a halted/errored run still reports what it produced. The final-message pick is a three-tier fallback: the `-o` backstop, else the transcript's recovered `final_answer`, else the last streamed message (see Design decisions for why). `run_codex` just builds the args (fresh vs `resume <id>`); the `-o` temp file is created via `new_output_file` with a random UUID name and `O_EXCL`, so concurrent runs can't collide and the predictable path can't be symlink-clobbered. A run that produces no message still returns the digest (with transcript forensics) rather than a bare error. `print_result` renders the digest above the message. The codex child runs with `RUST_BACKTRACE=1` (so a panic is legible) and its stdin-write result is kept (a broken pipe = codex exited first). `DigestSummary` is the flat, serializable projection of a `Digest` (`exit_code`/`signal`/`captured`/`recovered_from_transcript`/`turns`/`task_complete`/`stream_error`/usage/`incident_path`) persisted into the audit + sidecar logs so failures are greppable after the fact.
- `src/transcript.rs` - Codex on-disk transcript forensics. Locates `$CODEX_HOME/sessions/**/rollout-*-<session_id>.jsonl` by filename (the session ID is captured, so no cwd/mtime heuristic) and parses it for `task_complete`, `stream_error`, the last event, the last in-flight tool call (a `function_call` with no matching `_call_output` = what was running when it stopped), and `final_answer` (the last `agent_message` whose `phase == "final_answer"` - codex tags interim notes `commentary` and the real report `final_answer`). Per-turn state (including `final_answer`) resets on each `task_started` so a resumed rollout reflects the current turn. Scoping a summary to one run's events is the caller's job, via `slice_from_offset(bytes, from_offset)` - the rollout's size at that run's launch, shared with `watchdog` so the two can never disagree about where a run begins. Passing `None` summarizes the whole file (`review sessions <id>`). Read when a run looks wrong (not captured / non-zero exit / signal), so clean runs stay uncluttered.
- `src/incident.rs` - Forensic bundle writer for suspicious codex runs (same `!captured || exit!=0 || signal` gate as the transcript). `write_bundle` dumps `~/.local/share/review/incidents/<utc>-codex-<sid>/` with `stderr.txt` (the channel we used to discard - and with `RUST_BACKTRACE=1` set on the child, a panic lands here with its trace), `stdout.jsonl` (raw NDJSON, last 1 MiB), `transcript.tail.jsonl` (last 80 rollout lines), `prompt.txt` (the exact stdin), and `meta.json` (full argv incl. argv[0], a copy-pasteable `command` string = injected env prefix + quoted argv + `< prompt.txt`, cwd, exit/signal, stdin-write error, durations, `codex --version`, `final_answer_present`/`rate_limits` scanned from the rollout, and profile env - names always, values redacted when the name looks secret). The `command` + `prompt.txt` make the run replayable verbatim (secret env values show as `<redacted>`). `review incidents [--limit N]` lists recent bundles newest-first with a one-line verdict (`no final answer (died)` / `recovered` / `completed (truncated)`) and the bundle path; `list_recent` reads each `meta.json` back via `IncidentSummary`. Best-effort: every failure warns, never derails the run. The bundle dir is surfaced on the digest (`incident:` line) and persisted in the sidecar/audit digest.
- `src/watchdog.rs` - Rollout-completion watchdog for codex runs. Polls the rollout while codex runs; when a real `final_answer` for *this* run's turn exists on disk **and** the rollout has stopped growing for `QUIET_GRACE` (180s), it concludes codex has produced everything it is going to and is stuck in teardown, and the runner kills the process group. "This run's turn" is delimited by a **byte offset** (the rollout's size at spawn), not a timestamp: rollout events carry millisecond stamps while our own clock string is second-resolution, so a prior turn completing in the same second as launch would slip past a time filter and its stale answer would license killing a resume that had produced nothing. The offset is exact and has no such race. The baseline is an `Option<u64>`, and `None` (a resume whose rollout could not be statted before launch) **disables the watchdog entirely** rather than degrading to `Some(0)`: scanning the whole session history would let a previous turn's `final_answer` authorise killing a genuine mid-turn wedge. A fresh run's `Some(0)` is exact, not a fallback - its rollout does not exist yet. Explicitly **not** a timeout: it never fires on a run that has not already produced its answer, however long that run takes. `has_final_answer` is deliberately separate from `transcript::parse` - parse answers "state of the current turn" (resetting at each `task_started`), the watchdog asks "has this run produced an answer at any point since it began", which must survive the trailing micro-turns codex appends after the substantive turn. A genuine mid-turn wedge is *not* covered; that needs a stall detector, which is a real timeout and a separate decision (marked `TODO(stall-detector)`).
- `src/provider_tests.rs` - End-to-end tests of the codex runner against a **stub `codex`** shell script (included into `provider.rs` as a `#[cfg(test)]` module so it can reach private items). The behaviours under test are *process* behaviours - finishing work then refusing to exit, wedging mid-turn, leaving a descendant that holds stdout open or ignores `SIGTERM` - none of which can be faked with a mocked return value; they need a real child, pipe, process group and kill. `CodexRuntime` (binary, watchdog `Timings`, `drain_grace`, `sigkill_escalation`, `data_root`) is the injection point that makes a 3-minute detection and 10-second escalation resolve in under a second. `data_root` is load-bearing, not tidiness: `CODEX_HOME` only redirects the *child*, so `review`'s own paths otherwise resolve from the real `HOME`/`XDG_DATA_HOME` - an earlier version of this suite wrote stub incident bundles into the operator's real `~/.local/share/review/incidents`. Coverage: stranded completion is killed and recovered; an advancing rollout is never killed (answer present *and* still writing - the advancement gate, not the answer gate); a pipe-retaining child blocks neither reaping nor output capture; group escalation actually reaps a `SIGTERM`-ignoring descendant. Scratch lives in `target/test-scratch/<uuid>/`; stubs record any deliberately-left-running pid to `$LINGER_PID` so tests clean up after themselves.
- `src/inflight.rs` - Marker files at `~/.local/share/review/inflight/<session_id>.json` written once a run's session ID is known and removed when it returns, so `review sessions` can print `[in flight] ... turn in flight since <age>`. The sidecar is only written on return, so without this a running (or wedged) turn is indistinguishable from an idle session showing its previous response. Each marker records the owning `review` pid; `read_live` treats a marker whose pid is gone as stale, and deletes it. The session ID is validated as a bare filename component (alphanumerics, `-`, `_`) before being used as a path - it arrives straight from `--session <id>`, whose validation `review` otherwise delegates to the provider, so unchecked it let `--session ../foo` escape the marker directory and write then *delete* a file elsewhere. An allowlist, not a `..` denylist, because only the former is safe by construction. Best-effort throughout - never derails a run.
- `src/sessions.rs` - Append-only sidecar log at `~/.local/share/review/sessions.jsonl` (or `sessions-private.jsonl` if `audit.private`). One row per run that captured a session ID (`kind = "run"`), one per `--session` resume (`kind = "session"`). Rows carry timestamp + epoch_secs, project, hostname, audit_id, provider, archetype, session_id, model, env var *names* (not values - those can carry secrets), operator prompt, assembled prompt, response or error, review version, and (codex) the flat `DigestSummary`. Read helpers (`read_all`, `latest_for_session`, `age_secs`, `format_age`) drive the cache-age gate in `--session` mode and the `review sessions` subcommand.
- `src/config_write.rs` - `append_audit_id` (the only writer left; archetypes/profiles are hand-edited).
- `src/main.rs` - Wires CLI to config, prompt assembly, and provider dispatch. Also prints the trailing `runtime:` line: a wall clock started before `Cli::parse()` and printed after the last result on both the fan-out and `--session` resume paths (`format_runtime`). It deliberately spans stdin read + global-lock wait, so it's what the operator waited, not provider time; the subcommands (`init`/`sessions`/`incidents`) return before it.

## Design decisions

- Every run starts a fresh session - archetype priming prompt + stdin. Reviving a long-lived session on a cold cache reprocesses its ever-growing history; a fresh session costs ~one review's worth of tokens and can't act on stale accumulated context. The session is persistable and its ID is printed so follow-ups can go through `--session` while the cache is warm.
- No baked-in grounding prefix. It was written for long-lived read-only review sessions (anti-staleness + "don't modify files"); fresh-per-run made the anti-staleness lines dead and workspace-write made "don't modify files" wrong. Archetypes now own their own grounding.
- Archetypes are pure: `[archetypes]` name → prompt, no host/session binding. Overrides live in separate host-scoped named profiles (`[<host>.<provider>.<profile>]` carrying model/effort/sandbox/env) selected with `--profile <name>`; `--profile` requires the table to exist for every launched provider or the run errors. `sandbox` defaults to `read-only`, so a bare run can never modify files; a profile opts up to `workspace-write`. Codex-only - claude's `--permission-mode` is a different axis (tool-approval, not a filesystem sandbox) with no honest mapping, so claude ignores `sandbox`.
- Providers resolve from `--provider`, else `[_defaults].providers`; empty → error.
- `--session <id>` resumes a specific provider session and sends raw stdin - bypasses `.review.toml`, no prime, no profile. Requires a single `--provider`. Validation of the session ID is delegated to the provider. Before invoking, `review` looks up the sidecar log for the last-touched time: `--session` is the *warm* path, so if the session last ended > 55 min ago (`STALE_SESSION_SECS`, past the realistic prompt-cache TTL) it **errors out** and tells the operator to do a fresh run instead of paying to reprocess a cold prefix. No sidecar record -> age unknown -> proceed. What refreshes the last-touched clock is *any resume that ran* (`resume_ran = output.is_ok()`), not just one that produced a good answer: the clock tracks prompt-cache warmth, and reprocessing the prefix warms the cache even on a codex mid-turn death (which returns `Ok`-with-a-death-digest). Gating refresh on answer quality would pin the clock to the last *successful* touch and wrongly refuse the next resume of a genuinely-warm session as stale; only a hard launch failure (`Err`, cache never warmed) skips the refresh.
- Codex `exec --json` runs fail two distinct ways, both surfacing as `exit 1 / captured false / turns 0` on stdout. (1) *Truncated-but-completed*: the model produced a real `final_answer` on disk but codex exited non-zero at shutdown, dropping the closing stdout events and the `-o` file. (2) *Genuine mid-turn death*: no `final_answer` was ever produced - the turn was cut off after a tool call. **`task_complete` does not tell these apart** - it fires on aborted turns too; the authoritative signal is `task_complete.last_agent_message` (== the rollout's `final_answer` `agent_message`), which is `null` on a death. So `review` keys recovery on `final_answer`, not `task_complete`: recovery is scoped to the bytes *this run* appended (the same `rollout_baseline` byte offset the watchdog uses, not a wall-clock stamp - a second-resolution stamp let a prior answer written in the same second as a resume's launch be recovered as the new run's result, suppressing auto-resume and writing a wrong response into the sidecar; an untrustworthy baseline scopes to `u64::MAX`, i.e. recovers nothing, because no recovery beats a wrong answer). When it exists (including from a later `--session` resume of the same session) it's restored from the rollout instead of surfacing the last streamed `commentary` (an interim "I am now doing X" note); when it's absent the run genuinely died, `review` keeps the interim text but the digest note says "died without a final answer" (exit status distinguishes a death from a benign no-answer). Why codex exits 1 / dies is codex-side and is *not* recorded in the rollout - no `stream_error`, abort, or rate/context-limit event; recovery salvages output but does not explain the death.
- Auto-resume is the "work around" for a codex mid-turn death: a fresh codex run that ends with no real final answer (`!captured && !recovered_from_transcript`, `died_without_answer`) is resumed **once**, immediately (cache still warm), inside the same spawned task so the profile model/effort/sandbox/env are reused, with a fixed nudge (`RESUME_NUDGE`). If the resume produces a real answer (`got_final_answer`) it replaces the dead result, prefixed `(auto-resumed after the initial run died...)`; otherwise the original dead result stands. A manual resume is exactly what rescued the original Death 2. It calls `provider::invoke` directly, bypassing the `--session` cold-cache gate (that gate guards a *cold* manual resume; this one is warm by construction). Always on.
- A suspicious codex run writes a full forensic bundle (`src/incident.rs`), because the death investigation kept stalling on information `review` discarded: codex's stderr, the raw NDJSON stream, whether the prompt finished writing, and (absent `RUST_BACKTRACE`) any panic trace. The bundle over-instruments the *next* death so it's diagnosable without a repro. It's the "catch" half of the strategy; recovery (final_answer) is the "work around" half. Root-causing codex itself is explicitly out of scope.
- **codex 0.147.0 can finish a turn and never exit.** Two hangs were diagnosed on 0.147.0 against sessions that worked on 0.146.0. (1) *Stranded completion*: every turn reached `task_complete` with a non-null `last_agent_message`, the rollout then stopped advancing, and the process hung 10+ hours with the answer on disk the whole time. (2) *Mid-turn wedge*: the rollout stopped mid-turn with 25 unified-exec background cells running. Leading upstream explanation for (1) is the resume-only `exclude_turns: true` that `codex exec`'s `thread_resume_params_from_config` began sending in 0.147.0 (openai/codex#35621): after `TurnCompleted`, exec issues an unbounded `thread/read` with `include_turns: true` (`maybe_backfill_turn_completed_items`) while its single `select!` loop is not draining notifications, and with turns excluded at resume that read must rebuild the whole thread history from the rollout - which fits "resumes hang, fresh runs don't, small sessions are fine". Secondary candidate: `OtelProvider`'s `Drop` shutdown, bounded for the TUI only by openai/codex#37109 - `codex exec` still drops it synchronously, and a Statsig metrics exporter is on by default in release builds. Neither is proven; root-causing codex is out of scope. `review` catches it rather than explains it.
  - The old `join!` on child-exit **plus both pipes** made this unsurvivable: everything `review` knows about a run (digest, transcript, incident bundle, sidecar row) is computed after that join, so a wedged run produced no output, no error and no incident bundle. The wait is now a `select!` loop that breaks on reap, with the watchdog able to pre-empt it. Pipe EOF is no longer required to finish - reaping is authoritative, and the readers get a 5s drain grace (defence in depth: 0.147.0 has no production `Stdio::inherit` on the exec path, so a descendant holding the pipes is not a known live path). Readers append into a shared buffer instead of returning at EOF, so a grace-period expiry keeps whatever was captured rather than discarding the stream; the reader is `abort()`ed rather than detached by dropping its `JoinHandle`. The session ID is published on a watch channel the instant `thread.started` is seen, so it survives a truncated capture - losing it would cost the transcript, the recovery path and the sidecar row.
  - `Digest.terminated_by_review` records that *we* killed the run and why, because otherwise a watchdog kill is indistinguishable from codex dying on its own - the exit/signal describe our kill, not codex's fate. Persisted flat, so `jq 'select(.digest.terminated_by_review != null)'` finds the hang class.
- Codex is spawned with `process_group(0)` so it and everything it spawns (exec-server, unified-exec cells, MCP servers) can be signalled as a unit - a wedged codex usually still has live children, and killing the pid alone orphans them. The cost is that a terminal SIGINT no longer reaches codex implicitly, so `provider::install_signal_supervisor` installs **one** process-wide SIGINT/SIGTERM handler that kills every live codex group and then exits 130/143. It must be global, not a per-run `select!` arm: tokio's signal registration is process-wide and permanent, so a per-run handler would leave `review` silently ignoring every SIGTERM after its first codex invocation - *harder* to kill than before. The final SIGKILL sweep uses the *original* snapshot of live groups, deliberately not a fresh read: if codex exits promptly on SIGTERM its waiter unregisters the group, so a re-read would omit exactly the case that matters - a descendant that ignored SIGTERM and is still alive - and `process::exit` cancels `terminate_group`'s deferred escalation. Exiting forfeits the interrupted run's digest/bundle, which is the right trade: the operator asked to stop, and the rollout is on disk for `review sessions <id>`.
- `--session` releases the global lock **once the provider process has spawned**, not when the run finishes, matching the fan-out path (which releases once its staggered launches have fired). The lock spaces out launches, and a resume launches one thing, so there is nothing left to serialize once it runs; holding it for the turn meant one wedged resume froze all `review` traffic on the host indefinitely. A hung run should cost you that run, not the tool. Release is driven by a launch handshake (`provider::LaunchSignal`, a oneshot sent right after `cmd.spawn()`) rather than by dropping the lock before calling `invoke` - the latter leaves a critical section guarding nothing, letting every queued resume through to spawn simultaneously. A dropped sender (failed spawn) releases it too.
- A turn that aborts without carrying a user request or model output is codex's shutdown-time phantom, not a real turn: 0.147.0 opens a turn during `codex exec` teardown and aborts it within milliseconds. It is **not** an empty event sequence - the observed shape is `task_started`, `turn_context`, a synthetic `role: "developer"` message whose text is `<turn_aborted>`, then `turn_aborted` - so "zero items" cannot be the test. What distinguishes it is the absence of both a `user_message` (every real turn is opened by one) and any model output, so `transcript::parse` keys the rollback on a positive list of real-content events (`user_message`, `agent_message`, `reasoning`, tool calls/outputs, `task_complete`, `stream_error`, and `message` with a non-`developer` role). Attributing the phantom made fully-completed sessions report `task_complete=false` - which sent the hang investigation chasing a non-existent "truncated turn" condition. A turn cancelled early but carrying real content is a genuine interruption and is kept.
- The digest is persisted (flat `DigestSummary`) into the audit + sidecar logs. Before this, `run_codex_json` returned `Ok` on any mid-run death and the digest was print-only, so a dead run was filed as a clean short `response` with `error: null` - invisible to `jq`. Now `jq 'select(.digest.captured==false)'` (or `.digest.stream_error==true`) finds them. The run still returns `Ok` (never `Err`) so the session ID + digest survive; the digest fields, not the `error` field, carry the failure.
- `review sessions` lists recent sessions for the current project (or `--all`), grouped by session ID, sorted by most recent touch. `review sessions <id>` shows one session's artifacts on demand: its persisted digest, the on-disk codex rollout transcript (path + task_complete/stream_error/last_event/last_in_flight_tool), and the final response - preferring the transcript's `final_answer` over the sidecar `response`, so even rows recorded before runtime recovery landed surface the real answer. Output is block-formatted for terminal reading; ad-hoc queries beyond that go through `jq` on the JSONL directly.
- Providers get prompts via **stdin pipe**, not CLI args, to avoid shell argument length limits.
- Claude runs with `--permission-mode dontAsk` (uses pre-approved permissions, rejects interactive prompts). Codex runs with `--sandbox read-only`.
- No global config - `.review.toml` lives in the project root.
- claude and codex only. kilo/opencode were removed.
- Subsuming the pbfhogg spec-loop python scripts (codex review/implement roles). Landed: sandbox as a profile field (codex-only), the rich codex digest + `-o` backstop, transcript forensics, resume-path digest parity, transcript `final_answer` recovery for truncated runs, and digest persistence into the audit + sidecar logs (+ `review sessions <id>` artifact view). `goal` needs no code - it's just an archetype whose prompt is `/goal`.
- Planned: a `review add` command to create an archetype from a priming prompt (currently hand-edited).

## Config format

```toml
[archetypes]
security = "You are a security expert. Read the codebase."
bugs = "You hunt for edge cases and correctness bugs."

[_defaults]
providers = ["claude", "codex"]

[_groups]
sweep = ["security", "bugs"]

# host . provider . profile
[myhostname.claude.opus]
model = "Opus 4.8"
effort = "medium"
env = { ANTHROPIC_BASE_URL = "http://localhost:8787" }

[myhostname.codex.implement]
model = "gpt-5.6-terra"
effort = "high"
sandbox = "workspace-write"
# Optional: extra codex `-c key=value` overrides, each passed verbatim (codex-only).
config = ['model_provider="openai-http"']
```

## Document folders

The standing layout, across every project. Three live folders plus one retired,
split by durability first, subject second.

| Folder | Contents | Rule |
|---|---|---|
| `reference/` | Durable in-repo reference for anyone working on or with the code - how the thing is built and why: `architecture.md`, `technical-implementation-spec.md`, `performance.md` (the durable record of measured numbers over time), invariants, protocol contracts | Citable from source as a source of truth. What it says must be true. |
| `docs/` | Durable in-repo documentation of how the thing is used - guides, CLI reference, the consumer-facing API surface. Sometimes exposed as a hand-edited VitePress gh-pages site | Same must-be-true rule. |
| `notes/` | Transient - work items (`todo.md`), future plans, hypotheticals, bug reports, research, analysis. Things that will die | No truth guarantee. Nothing durable cites it. |
| `plans/` | Retired | Plan documents are transient: they go in `notes/`. |

`reference/` and `docs/` are both durable and both binding. The difference is
subject, not audience: `reference/` covers how the thing is built and why - what
you need in order to change it safely - while `docs/` covers how it is used. A
developer or library consumer reads both. Where a project publishes a site,
`docs/` is what gets published; the folder means the same thing either way.
`notes/` is neither durable nor binding, which is the whole point of keeping it
separate: a document that may be wrong must not sit where a document that must
be right is expected.

The dependency direction is therefore one-way. `notes/` may cite `docs/` and
`reference/`; nothing durable may cite `notes/` - not a code comment, not
`docs/`, not `reference/`. A code comment must carry its full context, because
it outlives the note.

**Root-level convention files are exempt.** `AGENTS.md`, `CLAUDE.md`,
`README.md`, `LICENSE`, `CHANGELOG.md` and their kin are found by tooling and by
convention at the repository root, and stay there. These folders govern
documents we chose where to put, not files whose location is dictated.

In `notes/`, `docs/` and `reference/` alike, avoid citing source line numbers -
they drift fast.
