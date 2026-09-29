# Claude

How `review` drives Claude Code: its invocation and environment, how a run's
outcome is read, and how a run is stopped. The cross-provider pieces are in
[sandbox.md](sandbox.md).

## Invocation

A fresh run is `claude --session-id <generated UUID> --print --permission-mode
dontAsk --output-format stream-json --verbose`; a `review resume` is the same
with `--resume <id>` in place of `--session-id`. Profile settings: `model` as
`--model`, `effort` as `--effort`, `env`. The prompt is piped via stdin. The
session ID is generated up front, so it is known before the run starts, and the
run's in-flight marker goes up at launch.

`--permission-mode dontAsk` uses pre-approved permissions and rejects
interactive prompts, which a headless run could never answer.

`stream-json` (which needs `--verbose` under `--print`) rather than `json` for
one reason: its first event, an `init` within half a second of launch, is the
sign that claude can be signalled safely (see [Stopping a run](#stopping-a-run)).
`json` prints nothing until the end. Both end in the same result object, which
is all `review` reads.

## Turns classify themselves

The stream ends in one result object per run, whatever the outcome, and its
`is_error` says whether `result` is an answer (a run in which claude woke the
model for a background subagent carries one per turn; the last is the answer).
That is the grok arrangement ([grok.md](grok.md#turns-classify-themselves)), and
`run_claude` handles it the same way: `is_error: false` is the answer; anything
else is `Ok` carrying a digest whose `turn_error` names what claude reported,
and keeps the session ID. A result object missing `is_error` counts as a
failure, so a claude that stopped reporting it fails loudly rather than passing
every run off as answered.

No result object at all splits on whether claude printed its first event, the
`init` that carries the session ID. Before it, nothing ran and there is no
session: an `Err`. After it, the session exists - its transcript is on disk -
so a claude killed from outside or crashed mid-turn is `Ok` with a death digest
(no `turn_error`, since claude stated no reason) and keeps the ID, as codex's
deaths do. The error text for a missing result is claude's stderr, or else the
stream's last line, truncated: the whole stream is every event of the run.

The stream is buffered up to 64 MiB from the start and, past that, the last
8 MiB or more, because the result is the *last* line: keeping only the head
would lose the answer of any run that outgrew the cap.

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
second turn, whose result object is the last in the stream.

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
[sandbox.md](sandbox.md#what-a-resume-inherits).

## Stopping a run

**A claude turn ends cleanly on `SIGINT`, so `review interrupt`, `review
message` and a signalled `review` treat it as they treat codex's.** Probed on
Claude Code 2.1.284 with a command running mid-turn: `SIGINT` to the claude
process ended the turn at once, killed the command, exited 0, and still printed
a result object - `is_error: true`, `subtype: "error_during_execution"`,
`terminal_reason: "aborted_tools"`, the session ID - and the session resumed
normally, the model seeing its command as rejected. `SIGTERM` also left a
resumable session but printed nothing (exit 143), so nothing could be recorded
from it; that is why the graceful path sends `SIGINT`.

The mechanics are codex's ([codex.md](codex.md#interrupting-a-run)), minus the
node wrapper: claude runs in its own process group, registered with the signal
supervisor; its in-flight marker names claude's pid once claude has printed its
first event (a `SIGINT` before its handler is installed would kill it with
nothing printed); `review interrupt` leaves a request beside the marker and
signals that pid alone; and a signalled `review` sends it `SIGINT` too, waiting
for the run to record its session before exiting. A run that had produced no
first event is killed with its group instead, and reported as never started,
with no session ID: claude cannot have written one, and recording it would
print a resume command that fails. A run that has not spawned yet when `review`
is told to stop - still waiting on the lock or its stagger - is not launched at
all. An interrupted run keeps its session ID even if claude printed no result,
is flagged `interrupted` rather than carrying claude's complaint about the abort
as a `turn_error`, and is not retried. If claude answers before the signal
lands, the answer stands. Before a resume launches, any interrupt request left
for its session by an earlier `review` that died before consuming it is
cleared, so it cannot mark this run interrupted.

Claude has no session lock of its own, so two `review message` calls to an idle
session used to both launch, running two turns on one session at once. `review
message` now holds a per-session launch lock from before it checks for a turn in
flight until its run has launched, and a run's marker is written before its
spawn, so the second message waits, then finds the first in flight and
interrupts it. Should two runs of one session still meet (the fan-out's own
auto-resume is not serialised this way), a run removes or updates the shared
marker only while it is still the one it wrote.

`src/provider_tests.rs` drives the real `run_claude` against a stub claude
(`ProviderRuntime::claude_command`) for each of these: the interrupt, an answer
beating it, a stop after and before the first event, a refused launch after a
stop, a death after starting, and a stale request before a resume.

Verified end to end through `review`: a `SIGTERM` to `review` mid-command ended
the turn in 0.2s, recorded it, printed the `review message` command and left
nothing running; `review interrupt <ID>` from a second process did the same;
and `review message <ID>` interrupted the turn and resumed it with the new
message, which the model answered.
