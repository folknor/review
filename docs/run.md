# A fresh run

`echo "<instructions>" | review [-a ARCHETYPES] [-p PROFILE] [--provider LIST]`

Every run starts a **fresh session** on each provider it launches. Nothing
carries over from an earlier run: if the new session needs context, the
instructions have to state it. To continue a session instead, use
`review message <ID>` while its cache is warm (see `review man message`).

## Input

Instructions come on stdin and are required, up to 20 KB. A pipe and a heredoc
are the same thing to `review`:

```
review -a security <<'EOF'
Audit the session handling in src/auth.
EOF
```

Quote the heredoc delimiter (`'EOF'`) so the shell leaves `$`, backticks and
backslashes in the text alone. With no arguments and nothing on stdin, `review`
prints its help and exits 2.

## Archetypes

`-a` names what to prime each session with. An archetype is a name mapped to a
prompt in the config, prepended to stdin with a blank line between.

- no `-a`: stdin is sent unchanged, recorded under the archetype label `bare`.
- `-a security`: one archetype.
- `-a security,bugs`: several, comma-separated.
- `-a sweep`: a group, which expands to its member archetypes.
- `-a all`: every configured archetype.

Names can be mixed (`-a sweep,perf`) and duplicates are dropped. An unknown
name is an error listing what is configured. The old positional form
(`review security`) still works with a warning.

## Fan-out

Each archetype runs on each provider: two archetypes on two providers are four
independent sessions. Providers come from `--provider claude,codex`, else from
`[_defaults].providers` in the config. Every provider is checked before
anything launches: one that is unknown or not installed on this host fails the
whole run, rather than quietly running a narrower fan-out than was asked for.
`review config` shows which are installed.

Launches are spaced by `--stagger` seconds to avoid rate limits; `review --help`
shows the default, and `0` disables it. Runs on the same host also queue behind one host-wide launch lock;
it is released once the launches have fired, not when the runs finish, so a
long run never blocks other `review` calls.

A profile (`-p`) must be defined for every launched provider, or the run errors
before launching anything (see `review man profiles`).

## Dry run

`--dry-run` prints the assembled prompt for each archetype and launches
nothing. It warns, rather than fails, about providers that are not installed.

## Output

Each provider prints a `--- <provider> ---` block: the `session: <ID>` line,
any digest, then the response. `review --help` lists the digest fields. With
several archetypes, results are grouped under `=== <archetype> ===`. The output
ends with a `runtime:` line, which is wall clock including the wait for stdin
and the launch lock.

## Auto-resume

A codex run that dies mid-turn without a final answer is resumed once,
immediately, on the same profile and session, with a short nudge to finish. If
that produces an answer it is reported, prefixed
`(auto-resumed after the initial run died without a final answer)`; if not,
the original death is reported - unless the resume was itself interrupted, in
which case the resume is reported, so its continue command is shown. A run whose provider stated why it failed, and
a run that was interrupted, are never auto-resumed (see `review man failures`).

## Exit status

`0` when at least one run produced an answer and nothing any run started is
still running, and for a `--dry-run`. `1` when no run produced an answer -
including runs that died, stalled, stated a failure or were interrupted - and
on any error before launch. `3` when anything a run started was left running,
or its teardown could not be confirmed, whatever the answers were (see
`review man failures`): it outranks `1`, because those processes may still be
changing the tree and need checking first. `2` for invalid arguments, and
for bare `review` with nothing on stdin. A run that failed still prints its session ID when the
provider created one, so it can be inspected (`review sessions <ID>`) or
continued.

## Long runs

Runs can take hours. A harness that stops background commands after a timeout
signals `review`, which tries to end its codex and claude turns gracefully and
record them within a bounded wait, so the sessions can be continued with
`review message`. A second signal, SIGKILL, or a turn that does not stop in
time loses the record. Launch with the largest
timeout the harness allows, and never stop or kill `review` to steer an agent:
use `review message` (see `review man message`).
