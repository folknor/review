# TODO

## Remaining review findings

- **Stale temp file from PID reuse** (LOW) — killed process + recycled PID = silent wrong codex results. Use `tempfile` crate or `create_new(true)`.

## Session scaffolding: `review prime` phase 2

Phase 1 (done) implements `review prime` for claude and codex:
- Claude: generates UUID, passes `--session-id`
- Codex: parses `thread_id` from `--json` JSONL output
- Appends session IDs to `.review.toml` (handles existing sections)

Phase 2: kilo and opencode support.

**Kilo** — `--format json` does not emit a session ID event before task completion (output is buffered). But `kilo session list` shows sessions:
```
ses_2c61344d5ffe61Moxe9A1e3Klk  New session - 2026-03-29T14:08:28.970Z
```
Approach: run `kilo session list` before and after priming, diff to find the new one. Note: ID format is `ses_*`, not UUID.

**OpenCode** — same as Kilo (shared codebase). Same buffered JSON, same `session list` approach.

### Open questions

- Should `review prime` support `--model` for providers that need it (kilo, opencode)?
- Should there be a `review sessions` command to list/manage sessions?

Sources:
- [Codex session-id feature request](https://github.com/openai/codex/issues/13242)
- [Kilo CLI docs](https://kilo.ai/docs/code-with-ai/platforms/cli)

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
- How to handle multiple machines pushing to the same repo — just append and let git merge, or use per-host branches?
