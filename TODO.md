# TODO

## Remaining review findings

- **Stale temp file from PID reuse** (LOW) - killed process + recycled PID = silent wrong codex results. Use `tempfile` crate or `create_new(true)`.

## `review add`

Add a command that creates an archetype from a priming prompt (writes `[archetypes].<name>` in `.review.toml`), so archetypes don't have to be hand-edited. Takes the prompt on stdin.

## Subsume the pbfhogg spec-loop scripts

`review` is absorbing the per-project python scripts (`pbfhogg/scripts/codex_common.py`, `codex-review.py`, `codex-implement.py`) so the workflow stops living as copied scripts in each project. Landed so far: fresh-session-per-run, host-scoped profiles (model/effort/env), and `sandbox` as a profile field (codex `--sandbox`; default `read-only`). Remaining:

- **Rich digest.** Parse codex's NDJSON into a summary instead of raw passthrough: token usage (input/cached/output/reasoning), turn count, and a completed-vs-interrupted verdict.
- **`-o` / `--output-last-message` backstop.** Authoritative "did it finish" signal that survives codex halting its NDJSON mid-run. The load-bearing trick in `codex_common.run_codex`.
- **Transcript forensics.** Optionally read codex's on-disk session JSONL (`$CODEX_HOME/sessions`) to diagnose why a run stopped.
- **claude sandbox mapping.** Wire the profile `sandbox` value onto claude's `--permission-mode` (see the `_sandbox` TODO in `provider.rs`).

Note: `goal` needs no code - an archetype whose prompt is `/goal` covers it. With the grounding prefix gone, `/goal` now leads the message, but `assemble` joins it as `/goal\n\n<stdin>` whereas the old scripts used `/goal <text>` (same line). Verify codex accepts the newline-separated form, or special-case the join if not.

Sources:
- [Codex session-id feature request](https://github.com/openai/codex/issues/13242)

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
