# review

A Rust CLI that fans out code reviews to persistent AI sessions across multiple providers (Claude Code, Codex), each cultivated with a specific reviewer perspective.

## How it works

You configure **archetypes** -- reviewer perspectives like `security`, `bugs`, `perf`, `arch`, or any custom name -- each backed by long-lived sessions in one or more AI providers. When you run a review, you pipe your instructions via stdin and tell the tool what to review with a flag. The tool builds a prompt and sends it to all providers for that archetype in parallel. The agents have project familiarity from their persistent sessions and fetch the code themselves.

## Install

```
cargo install --path .
```

Requires Rust 1.92+.

## Quick start

### 1. Initialize

```
cd /path/to/your/project
review init
```

### 2. Add session IDs

Edit `.review.md` and add your provider session IDs under your hostname:

```markdown
---
security:
  myhostname:
    claude: "your-claude-session-id"
    codex: "your-codex-session-id"
bugs:
  myhostname:
    claude: "your-claude-session-id"
---
```

### 3. Run reviews

```
echo "look for auth boundary violations" | review security --staged
echo "check for edge cases" | review bugs --commit abc123
echo "review this spec for gaps" | review arch --document spec.md
echo "full review" | review all --general
echo "check logging patterns" | review logging --general
```

## Usage

```
echo "<instructions>" | review <archetype> <flags>
```

Instructions are piped via stdin (required, 20KB limit). A flag telling the agent what to review is always required.

### Archetypes

Built-in archetypes have tailored prompts:

| Archetype | Focus |
|-----------|-------|
| `security` | Auth boundaries, injection, secrets, trust assumptions |
| `bugs` | Logic errors, edge cases, error handling, crashes |
| `perf` | Allocations, complexity, hot paths, async blocking |
| `arch` | Coupling, abstractions, API design, consistency |

Custom archetype names are also supported — any name works. Custom archetypes use a generic fallback prompt unless overridden in `.review.md`.

Use `all` to fan out to every configured archetype, or define **groups** to fan out to a named subset.

### Flags

| Flag | Context sent to reviewer |
|------|------------------------|
| `--unstaged` | "You are reviewing unstaged changes." |
| `--staged` | "You are reviewing staged changes." |
| `--commit <hash>` | "You are reviewing commit \<hash\>." |
| `--range <a..b>` | "You are reviewing commits \<a..b\>." |
| `--document <path>` | "You are reviewing the file \<path\>." |
| `--general` | "You are reviewing the entire codebase." |
| `--raw` | No prefix, archetype prompt, or context line — stdin only |
| `--dry-run` | Print the assembled prompt instead of sending it |

The agents fetch the actual content themselves using their project context.

Use `--raw` for follow-up questions or when you want full control over the prompt:

```
echo "what did you mean by finding #3?" | review bugs --raw
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

Per-project `.review.md` in the project root (discovered by walking up to the git root). Run `review init` to create a starter.

```markdown
---
security:
  myhostname:
    claude: "session-abc123"
    codex: "session-def456"
bugs:
  myhostname:
    claude: "session-ghi789"
tilemaker:
  myhostname:
    claude: "session-jkl012"
tippecanoe:
  myhostname:
    claude: "session-mno345"

_groups:
  sweep: [security, bugs]
  competitors: [tilemaker, tippecanoe]
---

## security

Custom security review instructions here.
Overrides the built-in security prompt.

## bugs

Custom bugs review instructions here.
```

Session IDs are scoped by hostname, so the same `.review.md` works across machines with different sessions. Markdown `## headings` optionally override the built-in archetype prompts.

### Groups

Groups fan out to multiple archetypes with a single command:

```
echo "how to handle clipping?" | review competitors --general
echo "full sweep" | review sweep --staged
```

Define groups in the `_groups` key of the frontmatter. Group names must not conflict with archetype names. `all` is reserved and runs every configured archetype.

## Providers

### Claude Code

```
claude --resume <session-id> --print --permission-mode plan
```

Runs in `plan` mode (read-only). Prompt piped via stdin, output captured from stdout.

### Codex

```
codex exec --sandbox read-only resume <session-id> -o <file>
```

Runs in read-only sandbox. Prompt piped via stdin, output captured from the `-o` file.

Both providers run in parallel. If one fails, the other's results are still shown. Providers whose binaries aren't installed are silently skipped.
