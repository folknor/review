# review

A Rust CLI that fans out code reviews to persistent AI sessions across multiple providers (Claude Code, Codex), each cultivated with a specific reviewer perspective.

## How it works

You configure **archetypes** -- named reviewer perspectives like `security`, `bugs`, `perf`, `arch` -- each backed by long-lived sessions in one or more AI providers. When you run a review, the tool assembles a prompt (grounding prefix + archetype-specific instructions + your diff or document), sends it to all providers for that archetype in parallel, and prints the labeled results.

Sessions carry project familiarity from previous interactions. A grounding prefix on every invocation tells the reviewer not to trust its memory of the codebase, only what's explicitly provided.

## Install

```
cargo install --path .
```

Requires Rust 1.92+.

## Quick start

### 1. Create the config

```toml
# ~/.config/review/config.toml

[global]
prefix = "~/.config/review/prompts/prefix.md"

[projects.myproject]
path = "/home/you/myproject"
```

### 2. Create prompt files

```
~/.config/review/prompts/
  prefix.md                  # grounding prompt (sent every time)
  security/
    diff.md                  # security review instructions for diffs
    document.md              # security review instructions for documents
  bugs/
    diff.md
    document.md
```

### 3. Register sessions

```
cd /home/you/myproject
review register security --claude <session-id>
review register security --codex <session-id>
review register bugs --claude <session-id>
```

### 4. Run reviews

```
review security --staged
review bugs --branch
review all --unstaged
```

## Usage

```
review <archetype> <input-source>
```

### Input sources

| Flag | Description |
|------|-------------|
| `--unstaged` | Working tree changes (`git diff`) |
| `--staged` | Staged changes (`git diff --cached`) |
| `--commit <hash>` | Diff of a specific commit |
| `--range <a..b>` | Diff across a commit range |
| `--branch` | Full branch diff against default branch |
| `--document <path>` | A file reviewed as-is, not as a diff |
| `--stdin` | Read from stdin (treated as diff) |
| `--stdin --as-document` | Read from stdin (treated as document) |

Use `all` as the archetype to fan out to every configured archetype.

### Session management

```
review register <archetype> --claude <session-id>
review register <archetype> --codex <session-id>
review deregister <archetype>               # remove entirely
review deregister <archetype> --claude      # remove just claude
review list                                 # current project
review list --all                           # all projects
```

### Output format

```
--- claude ---
<review content>

--- codex ---
<review content>
```

When using `all`, archetype headers are added:

```
=== security ===

--- claude ---
<review content>

=== bugs ===

--- claude ---
<review content>
```

## Configuration

Single config file at `~/.config/review/config.toml`.

```toml
[global]
prefix = "~/.config/review/prompts/prefix.md"

[projects.myproject]
path = "/home/you/myproject"

[projects.myproject.archetypes.security]
claude = "session-abc123"
codex = "session-def456"
prompt_diff = "~/.config/review/prompts/security/diff.md"
prompt_document = "~/.config/review/prompts/security/document.md"
```

Project resolution is prefix-based -- running `review` from any subdirectory of a registered project path matches that project. Nested project paths resolve to the most specific match.

## Providers

### Claude Code

Uses `claude --resume <session-id> --print` in non-interactive mode. Prompt piped via stdin.

### Codex

Uses `codex exec resume <session-id> -o <file>`. Prompt piped via stdin, output captured from the `-o` file.

Both providers run in parallel. If one fails, the other's results are still shown.
