# Sessions and incidents

`review` records every run that produced a session ID, on this host only, in
`sessions.jsonl` under `$XDG_DATA_HOME/review`, else `~/.local/share/review`
(`sessions-private.jsonl` for a project with `[_audit] private = true`). That record is what `review message` reads to
find a session's provider, project and settings.

## Listing sessions

`review sessions` lists the current project's sessions, most recently touched
first (`--limit`, default 20; `--all` for every project). `-C` lists another
project's. Each entry shows its age, provider, archetype, how many times it was
touched, and the first line of the prompt that opened it.

Two kinds of entry come before the list:

- `[in flight] <provider> / turn in flight since <age>`: a run is happening
  right now. Its record is only written when it returns, so without this a
  running turn would look like an idle session showing its previous answer.
- `[queued] message waiting to launch`: a `review message` that holds the
  session's launch lock but has not launched yet, usually because it is waiting
  behind the host-wide launch lock. A further message waiting behind that one
  adds no entry of its own (see `review man message`).

A run whose `review` process has died is not shown in flight.

## One session

`review sessions <ID>` shows a session's last record wherever it was started:
provider, archetype, project, model, effort, sandbox and writable roots, the
digest, and the response. A deleted project still shows. With `-C`, the
session must belong to the selected project.

For codex it also reads the session's transcript, under the `CODEX_HOME` of
the `review sessions` command itself: its path, whether the turn completed,
the last event and any tool call still in flight. When the transcript holds a
final answer, that answer is shown in place of the recorded response, marked
`(recovered final answer from transcript)` when the two differ. A completed
transcript with no final answer is flagged as having produced no conclusion;
the recorded response is still shown below it. For a run whose profile set its
own `CODEX_HOME`, run `review sessions <ID>` with that `CODEX_HOME` set.

## Incidents

`review incidents` lists the forensic bundles filed for suspicious codex runs,
newest first (`--limit`): deaths, stalls and stated failures. Each line shows
the time, session, exit status and signal, a verdict, and the bundle directory.

The verdicts:

- `turn failed: <reason>` - codex stated why; not a hang.
- `recovered from transcript` - the answer was restored.
- `no final answer (died)` - a genuine mid-turn death.
- `completed (stream/-o truncated)` - codex finished but its output was cut.
- `suspicious` - none of the above could be established.

A bundle holds codex's stderr, the raw event stream, the digest and the
transcript excerpt, so the run can be diagnosed without reproducing it.
Interrupted runs are never bundled.

## Querying further

The session log is one JSON object per line. Fields include `session_id`,
`provider`, `project`, `archetype`, `model`, `effort`, `sandbox`,
`writable_roots`, `env_keys` (env var names only), the prompts, the response or error, and a
flat `digest`, so `jq` answers what the listing does not (with the default
data directory):

```
jq 'select(.digest.captured == false)' ~/.local/share/review/sessions.jsonl
jq 'select(.sandbox == "workspace-write")' ~/.local/share/review/sessions.jsonl
```
