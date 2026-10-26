# Why a run failed

A run exits `1` when no provider produced an answer, and `3` when something it
started was left running (see the section below), whatever the answers were. Most failures still print
a session ID and a digest, because the session exists and can be inspected or
continued. Read the digest top down:

- `turn failed: <reason>` - usually the provider **stated** why it ended the
  turn. For grok it can also be `review`'s own verdict: background results it
  could not verify, or the wake limit (see the end of this page). The reason
  says which.
- `terminated by review` - `review` ended it: an interrupt, a signal, or the
  codex watchdog. Not always a failure: the watchdog also ends a codex process
  that finished its answer and then hung, and that answer is recovered.
- `recovered` - codex never reported its answer but wrote one to its
  transcript, and that answer was restored. This is a success even though
  `captured` reads `false`.
- `captured: false` with none of the above - the run **died** mid-turn without
  saying why.
- `captured: true` - there is an answer; a non-zero exit after it is reported
  but does not make the run a failure.

`review sessions <ID>` shows the same digest later (see `review man sessions`).

## A stated failure

The provider refused or could not run the request: content flagged, a rate
limit, an authentication failure, an unknown model. The reason is printed
first, because the next move follows from it. A stated failure is **never
retried** - resending the same request buys the same refusal. Fix the cause
(model name, credentials, quota, prompt) and start a fresh run, or message the
session if the cause was transient.

## A death

The turn was cut off with no final answer and no reason given. On codex,
`review` first tries to recover: if codex wrote a final answer to its
transcript but exited before reporting it, the answer is restored and the
digest says `recovered`. If there was no answer, the run is auto-resumed once
with a nudge to finish (see `review man run`), and an incident bundle is filed
(see `review man sessions`).

A claude run killed or crashed after it started is a death too, and keeps its
session ID. A claude run that never started has no session.

Grok and claude classify their own turns, so neither is auto-resumed: a turn
that ends without an answer is reported with the reason they gave.

## A stall

A codex run whose transcript stays silent for `stall_timeout_secs` (in
`[_defaults]`; `0` disables) without a final answer is killed as stalled. Codex
normally writes every few minutes even while it waits on a long command, so
`review` treats a long silence as a wedge - an empirical rule, which a change in
codex's habits could make misfire. It only applies when `review` can find and
read the run's transcript and knew, at launch, where in it this run's part
begins; without both, nothing bounds a hung codex run. The digest says `terminated by review` with
how long it was quiet and what it was last doing, and an incident bundle is
filed. The session can be messaged to continue.

## An interrupt

`review message`, `review interrupt`, or a signal to `review` ended the turn.
The digest is flagged interrupted, the run exits `1`, and it is never
auto-resumed or filed as an incident. If the provider finished before the
interrupt landed, the answer stands and the run succeeds.

## Processes left running

Whatever a run's agent started that is still running when the run ends -
background commands, daemons, shells in sessions of their own - is ended before
`review` reports, however the run ended. That is recorded in the run's session
record (`leftovers`) and never printed.

What could not be ended is printed above the `runtime:` line, and the command
exits `3` even if every run answered:

- `left running: N processes <run> started survived teardown`, then one line per
  process with its pid and command line. `at least N` means more remain that
  could not be identified.
- `teardown outcome unknown: <run> (keeper pid N) - <reason>`: `review` could
  not confirm the teardown, so processes the run started may still be running;
  any it knew to have survived are listed under it.

Either way the run's processes may still be changing the tree. Check them
before starting more work there. A codex death in this state is not
auto-resumed.

This also ends daemons an agent started and left behind (a build cache server,
a build tool's daemon), which the next run that needs them starts again.

When `review` is signalled, the same report goes to stderr before it exits. If
`review` is killed outright, what its runs started is still torn down, but
nothing is printed; the outcome is appended to `teardowns.jsonl` (or
`teardowns-private.jsonl`) beside the session logs, best-effort.

## Before anything launched

These fail the whole run with no session: no stdin, a config that does not
parse, an unknown archetype, a profile missing for a launched provider, an
unknown or uninstalled provider, a project that `-C` cannot resolve. The error
says which, and nothing was sent.

## A run that cannot write

See `review man sandbox`. A grok write run cancelled at its first edit, with a
hint about folder trust in the reason, ran in a folder grok does not trust.

## A finished run with no report

Grok can end its turn while one of its commands is still running in the
background, and never see the result. `review` waits for such work and resumes
the session with the results it detects, printing each wake, until a turn ends
with nothing unread. The run **fails** instead if grok still has unread results
after ten wakes, or if grok's own record of the session cannot be read or
parsed. It does not wake after a stated failure. One gap is known: a background
subagent that finishes while the turn is still running can go undetected.

Claude runs keep shell commands in the foreground for the same reason, so the
model waits for them, with a 20-minute ceiling per command. Both are defaults
`review` sets through environment variables, so a profile's `env` can change
them, and a model can still give a command a shorter timeout.
