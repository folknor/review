# Claude

How `review` drives Claude Code. Claude needs the least provider-specific
handling of the three; the cross-provider pieces are in
[sandbox.md](sandbox.md).

## Invocation

A fresh run is `claude --session-id <generated UUID> --print --permission-mode
dontAsk --output-format json`; a `review resume` is the same with `--resume <id>`
in place of `--session-id`. Profile settings: `model` as `--model`, `effort` as
`--effort`, `env`. The prompt is piped via stdin. The session ID is generated up
front, so it is known before the run starts.

`--permission-mode dontAsk` uses pre-approved permissions and rejects
interactive prompts, which a headless run could never answer.

claude runs in its own process group, registered with the signal supervisor
like codex's, so a `review` that is signalled takes claude and every shell it
started with it instead of leaving them running detached. Unlike codex, a claude
run gets no graceful stop: the group is killed, and no sidecar row is written
for a fresh run that had not finished.

## Turns classify themselves

`--output-format json` prints one result object per run, whatever the outcome,
and its `is_error` says whether `result` is an answer. That is the grok
arrangement ([grok.md](grok.md#turns-classify-themselves)), and `run_claude`
handles it the same way: no result object at all is an `Err` (nothing ran);
`is_error: false` is the answer; anything else is `Ok` carrying a digest whose
`turn_error` names what claude reported, and keeps the session ID. A result
object missing `is_error` counts as a failure, so a claude that stopped
reporting it fails loudly rather than passing every run off as answered.

Two details from Claude Code 2.1.284 decide the shape. `subtype` is no guide:
an unknown `--model` came back `subtype: "success"`, `is_error: true`,
`terminal_reason: "api_error"`, `api_error_status: 404`, exit 1 - so `is_error`
decides, and the other three are only named in the reason. And that failed
launch still has a session: the result carries its ID and the transcript is on
disk. Text output, which `review` used before, gave only the non-zero exit, and
the ID was discarded with it (`invoke` sets none on an `Err`).

The result object's `modelUsage` keys and `total_cost_usd` are recorded as the
served model and cost (`result_served`, shared with grok), and its usage fills
the digest (`claude_usage`). A clean run has no digest, as for grok. Claude has
no auto-resume: that is codex's workaround for a death, and a claude turn that
fails says why.

## Shell commands stay in the foreground

**`--print` kills a background Bash command when the model ends its turn.**
Probed on Claude Code 2.1.284 (`scripts/claude_background_probe.py`): asked only
to run a 60-second command in the background and report its output, the model
replied "I'll report exactly what it prints when it finishes" and ended its
turn. About five seconds later claude marked the command `killed` and exited 0.
The result object says nothing about it - `is_error: false`,
`terminal_reason: "completed"` - so to `review` it was a clean run with no
report: the loss grok's wake loop exists for
([grok.md](grok.md#background-commands)). A background *subagent* is handled
differently: claude waits for it and wakes the model with its result in a
second turn, and the single json result is that last turn's.

Claude, unlike grok, has a switch that removes the failure rather than
detecting it: `CLAUDE_CODE_DISABLE_BACKGROUND_TASKS=1` takes the
`run_in_background` parameter off the Bash tool altogether. Under it the same
prompt ran the command in the foreground and reported its output. `run_claude`
sets it on every run, so no wake loop is needed.

That exposes Bash's timeout, which kills a foreground command at 2 minutes by
default. The model may pass a longer `timeout`, but under `dontAsk` a permission
allowlist naming commands (`Bash(python3 *)`) denied every call carrying one -
the probe's model tried 200s and was refused - so the default is the limit that
actually binds. `run_claude` sets `BASH_DEFAULT_TIMEOUT_MS` and
`BASH_MAX_TIMEOUT_MS` to 20 minutes (`CLAUDE_COMMAND_WAIT_SECS`, grok's
allowance too); a 150-second command then ran to completion. All three are set
before the profile's `env`, so a profile can restate any of them.

## `sandbox` is ignored

A profile's `sandbox` has no effect on claude. Claude's `--permission-mode` is a
tool-approval policy - a different axis from a filesystem sandbox - with no
honest mapping from `read-only`/`workspace-write`, so `invoke` drops the value
rather than translating it, and a claude run records no sandbox level in the
sidecar. What a claude run may touch is decided by the permissions Claude Code
itself is configured with.

## Transcript, no watchdog

Claude writes each session's transcript to
`~/.claude/projects/<cwd with / as ->/<session-id>.jsonl`, but `review` reads
nothing from it: the result object already says whether the turn answered,
which is what codex's rollout forensics reconstruct. No stall watchdog either -
it rests on codex's habit of writing to its rollout every few minutes (see
[codex.md](codex.md#the-stall-timeout)), which nothing has established for
claude.

## Resume

A `review resume` carries the model and effort recorded for the session
(`--model`/`--effort`), as every provider's does - see
[sandbox.md](sandbox.md#what-a-resume-inherits). `review interrupt` and
`review message`'s interrupt are codex-only.
