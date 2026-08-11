# TODO

## `--session` and `--provider` shouldn't clash when they agree

`--session` currently requires exactly one `--provider` and errors on more, but it should not treat a `--provider` list that resolves to the *matching* provider as a conflict. If the named provider matches the session's provider, accept it instead of erroring.

## `review --help` should print the configured profiles

`--profile <name>` is only discoverable by reading `.review.toml`. Have `--help` list the profiles available for the current host, so an operator can see what `--profile` accepts without opening the config.

## Stall detector for the mid-turn wedge

The rollout-completion watchdog (`src/watchdog.rs`) covers a codex run that finished its answer and then hung. It does *not* cover a run that wedges mid-turn with no answer written - the rollout simply stops advancing. Detecting that means "no rollout growth in N minutes -> incident bundle + nonzero exit", which is a genuine timeout rather than a completion signal, so it stays opt-in and off by default. Hook is marked `TODO(stall-detector)` in `wait_for_stranded_completion`.

## `review add`

Add a command that creates an archetype from a priming prompt (writes `[archetypes].<name>` in `.review.toml`), so archetypes don't have to be hand-edited. Takes the prompt on stdin.

## Subsume the pbfhogg spec-loop scripts

`review` is absorbing the per-project python scripts (`pbfhogg/scripts/codex_common.py`, `codex-review.py`, `codex-implement.py`) so the workflow stops living as copied scripts in each project. Landed so far: fresh-session-per-run, host-scoped profiles (model/effort/env), `sandbox` as a profile field (codex `--sandbox`; default `read-only`), the rich codex digest + `-o`/`--output-last-message` backstop (token usage, turn count, captured-vs-interrupted; run no longer bails on non-zero exit; both fresh and `--session` resume runs share the digest via `run_codex_json`), and transcript forensics (`src/transcript.rs`: on suspicious runs, read `$CODEX_HOME/sessions/**/*-<id>.jsonl` for task_complete / stream_error / last in-flight tool). The absorption is complete; a codex self-review (dogfood) then surfaced a batch of bugs in the new code, all since fixed (unique `-o` temp file, digest preserved on no-message runs, per-turn transcript reset, completion-time sidecar stamping, `--session`/`--profile` guard, reserved subcommand names, resume-recording conditions, profile `CODEX_HOME` in forensics).

Non-goals (considered and dropped):
- **Usage in the sidecar.** Token usage/turns are in the printed digest; persisting them only helps after-the-fact spend aggregation, which we don't need. Skipped.

Note: `goal` needs no code - an archetype whose prompt is `/goal` covers it.
Note: `sandbox` is codex-only by design. Codex's filesystem sandbox and claude's
`--permission-mode` (acceptEdits/auto/bypassPermissions/manual/dontAsk/plan) are
different axes with no honest mapping, so claude ignores `sandbox`.

Sources:
- [Codex session-id feature request](https://github.com/openai/codex/issues/13242)

## Codex WebSocket transport deaths + workaround

codex `exec` runs intermittently die mid-turn: process exits 1, `-o` never written, the `--json` stream stops before `turn.completed` (0 turns), yet the rollout reaches `task_complete` with `last_agent_message: null` and **no** `stream_error`/abort/limit event. Reproduced by forcing an auth failure - stderr showed repeated `401 Unauthorized` on `wss://api.openai.com/v1/responses`.

Root: codex streams each turn over a **WebSocket**; on a WS failure a `426` falls back to HTTP but a `401` (or anything else) is a hard error with **no** HTTP fallback (`core/src/client.rs:1616`), surfacing as the silent null-completion + exit 1. There is no env/flag/global-config to disable websockets, and the built-in `openai` provider (which hardcodes `supports_websockets = true`) cannot be overridden (reserved id).

### Workaround (Lever A - not yet applied): force HTTP/SSE via a custom provider

Define a duplicate OpenAI provider under a **non-reserved** key with `supports_websockets` left at its `false` default, and select it. Routes every turn over HTTP/SSE, bypassing the websocket death, reusing existing auth:

```toml
model_provider = "openai-http"

[model_providers.openai-http]
name = "OpenAI"                      # keeps is_openai() true (matches on name, not key)
base_url = "https://api.openai.com/v1"
wire_api = "responses"
requires_openai_auth = true          # reuses auth.json / ChatGPT login
# supports_websockets omitted -> defaults false -> HTTP/SSE only
```

Or inline via two `-c` overrides (`-c model_provider="openai-http" -c model_providers.openai-http='{ ... }'`). Tradeoff: loses websocket latency optimizations. Weaker levers (retry/timeout knobs; failure-surfacing) don't help a 401.

**Applied in `review` via a profile `config` passthrough** (each string -> `codex exec -c ...`, scoped per-profile in `.review.toml`). The passthrough works and is committed.

### Outcome (2026-07-17): HTTP workaround blocked by account scopes - reverted

Forcing HTTP on this account (plantasjen) does switch transport - the `wss://.../responses` 401 disappears - but it hits a **hard `403 Forbidden` on `GET /v1/models`**: *"insufficient permissions ... Missing scopes: api.model.read ... restricted API key"*, and the run dies the same way (task_complete null, exit 1). Blanking `OPENAI_API_KEY` in the profile env did **not** change it - the restricted credential appears to live in `~/.codex/auth.json` (an api-key login, 4 KB), not the env - so codex uses it regardless. The websocket transport doesn't call `/v1/models` that way, which is why it works.

Conclusion: the underlying issue is **auth scope**, not transport (websocket 401 <-> HTTP 403 are two faces of it). The HTTP override is a net regression here (intermittent 401 -> hard 403), so it's reverted from the profile; the passthrough mechanism stays for future use. Websocket is the working transport; the intermittent 401 deaths are handled (not prevented) by recover + auto-resume + incident bundles.

To actually *prevent* the deaths, the auth must be fixed outside `review`: re-login with ChatGPT OAuth (`codex login`) or grant the API key the missing scopes (`api.model.read` + responses). (Evidence: `core/src/client.rs:930-938,1786-1823`; `model-provider-info/src/lib.rs:139-141,362,498`; `config/src/config_toml.rs:61-66,160,898-918`; `model-provider/src/provider.rs:286-320`.)

## Self-review findings (dogfood, 2026-07-17)

A codex self-review of the digest/recovery/incident/auto-resume work surfaced these. **All fixed** (with regression tests for the transcript/incident logic):

- [x] **[P1] Stale-answer recovery.** A resume that dies before a new `task_started` leaves the prior turn's `final_answer` in the transcript; recovery returns it as the new answer. `transcript.rs`, `provider.rs:509,534`
- [x] **[P1] Dead run exits 0.** `run_codex_json` returns `Ok` on exit-1/no-answer, and `main` treats only `output.is_err()` as failure, so an all-dead run (incl. failed auto-resume) exits 0. `provider.rs:578`, `main.rs`
- [x] **[P1] Global lock released too early.** Parent sleeps one `stagger` before dropping the lock, but tasks launch at `stagger * i`; with 3+ launches the lock drops before later launches. `main.rs`
- [x] **[P1] `review.lock` opened unsafely.** `File::create` truncates + follows symlinks (clobber risk on shared /tmp) and `0644` blocks a second user from opening for write. `main.rs`
- [x] **[P2] Failed manual resume refreshes warm-cache clock.** `run_session_resume` records a touch on `output.is_ok()`, but codex failures are `Ok`-with-digest. `main.rs`
- [x] **[P2] Auto-resume loses one invocation's forensics.** Only the returned result is persisted; the other (initial death or failed retry) digest/incident/nudge is dropped. `main.rs`
- [x] **[P2] Incident scan inherits prior turn's completion.** `scan_transcript` doesn't reset `final_answer_present` on `task_started`, mislabeling a frozen resumed turn as "completed". `incident.rs`
- [x] **[P2] Incident bundles collide.** Second-resolution timestamp + `no-session` + `create_dir_all` reuse = two concurrent failures overwrite each other. `incident.rs`
- [x] **[P2] Replay command can't reproduce prompt + cwd.** Uses relative `< prompt.txt` and never `cd`s to the recorded cwd. `incident.rs`
- [x] **[P2] Incident writer reports success after write failures.** `write_file` only warns; `write_bundle` always returns `Some(dir)`. `incident.rs`
- [x] **[P2] Forensic cap doesn't bound memory.** `wait_with_output` buffers all stdout/stderr before the 1 MiB tail is applied. `provider.rs`
- [x] **[P3] Codex `-o` temp files leak on early errors.** Created before spawn, removed only after a successful wait. `provider.rs`

## Audit log phase 2: git sync

Phase 1 (done) writes audit entries to `~/.local/share/review/<project>/audit.jsonl`. Phase 2 adds optional git sync to a central audit repository.

### Design

A global config at `~/.local/share/review/config.toml` with:

```toml
[audit]
repo = "folknor/review-audit"
```

When configured and `gh` is authenticated:

1. On first use, clone the repo to `~/.local/share/review/audit-repo/`
2. After each review invocation, copy the updated `audit.jsonl` into the repo under `<project>/audit.jsonl`
3. Commit and push with an automated message like "audit: <project> <archetype> <timestamp>"

### Requirements

- `gh` must be on PATH and authenticated (`gh auth status`)
- Repo must exist on GitHub (could offer to create it via `gh repo create --private`)
- Push failures should warn, not block the review
- Consider batching: don't push on every invocation, push on a timer or after N entries

### Open questions

- Should the audit repo be private by default? (yes, probably)
- Should `review init` offer to set up the audit repo?
- Should there be a `review audit` subcommand to inspect/manage the log?
- How to handle multiple machines pushing to the same repo - just append and let git merge, or use per-host branches?
