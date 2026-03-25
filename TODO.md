# TODO

## Codex invocation mode

Codex refuses to read the codebase during review — it responds with "no code or diff was included in this turn" instead of using its tools to go look. Claude works fine with `--permission-mode plan`.

Current codex invocation:
```
codex exec --sandbox read-only resume <session-id> -o <path>
```

Need to experiment with:
- Removing `--sandbox read-only` — might be preventing file reads
- Using `codex exec` without `resume` — fresh context might behave differently
- Using `--full-auto` instead of explicit sandbox flags
- Checking if `codex exec resume` inherits tool access from the original session
- Testing interactive `codex resume` to see if it reads files there (isolate whether it's an exec-mode issue or a prompt issue)

## Review findings to address

From Claude's reviews:

- **Pipe deadlock on large prompts** (HIGH) — stdin write blocks while stdout fills. Need concurrent read/write, e.g. `tokio::join!` or spawned write task.
- **Orphaned child process on stdin write failure** (MEDIUM) — early return on write error leaves provider running. Need kill-on-drop guard.
- **No provider timeout** (MEDIUM) — hanging provider blocks forever. Add `tokio::time::timeout`.
- **Invalid YAML in error message** (LOW) — `review all` with no sessions shows comma-joined archetypes as a single YAML key. Show first archetype only or one block per archetype.
- **Stale temp file from PID reuse** (LOW) — killed process + recycled PID = silent wrong results. Use `tempfile` crate or `create_new(true)`.
