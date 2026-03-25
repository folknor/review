# TODO

## Remaining review findings

- **Stale temp file from PID reuse** (LOW) — killed process + recycled PID = silent wrong codex results. Use `tempfile` crate or `create_new(true)`.
