# Claude

How `review` drives Claude Code. Claude needs the least provider-specific
handling of the three; the cross-provider pieces are in
[sandbox.md](sandbox.md).

## Invocation

A fresh run is `claude --session-id <generated UUID> --print --permission-mode
dontAsk`; `--session` resume is `claude --resume <id> --print --permission-mode
dontAsk`. Profile settings: `model` as `--model`, `effort` as `--effort`, `env`.
The prompt is piped via stdin. The session ID is generated up front, so it is
known before the run starts.

`--permission-mode dontAsk` uses pre-approved permissions and rejects
interactive prompts, which a headless run could never answer.

## `sandbox` is ignored

A profile's `sandbox` has no effect on claude. Claude's `--permission-mode` is a
tool-approval policy - a different axis from a filesystem sandbox - with no
honest mapping from `read-only`/`workspace-write`, so `invoke` drops the value
rather than translating it, and a claude run records no sandbox level in the
sidecar. What a claude run may touch is decided by the permissions Claude Code
itself is configured with.

## No rollout, no watchdog

Claude leaves no on-disk rollout for `review` to read, so none of codex's
transcript forensics, final-answer recovery, stall watchdog or incident bundles
apply. The stall timeout would be wrong for claude anyway: claude legitimately
goes silent while waiting on a backgrounded task, where codex wakes itself every
few minutes (see [codex.md](codex.md#the-stall-timeout)).

## Resume

A `--session` resume carries the model and effort recorded for the session
(`--model`/`--effort`), as every provider's does - see
[sandbox.md](sandbox.md#what-a-resume-inherits).
