# review

A Rust CLI that fans out code reviews to persistent AI sessions across multiple providers (Claude Code, Codex), each cultivated with a specific reviewer perspective.

## How it works

You configure **archetypes** -- reviewer perspectives like `security`, `bugs`, `perf`, `arch` -- each backed by long-lived sessions in one or more AI providers. When you run a review, you pipe your instructions via stdin and tell the tool what to review with a flag. The tool builds a prompt and sends it to all providers for that archetype in parallel. The agents have project familiarity from their persistent sessions and fetch the code themselves.

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

Edit `.review.md` and add your provider session IDs:

```markdown
---
security:
  claude: "your-claude-session-id"
  codex: "your-codex-session-id"
bugs:
  claude: "your-claude-session-id"
---
```

### 3. Run reviews

```
echo "look for auth boundary violations" | review security --staged
echo "check for edge cases" | review bugs --commit abc123
echo "review this spec for gaps" | review arch --document spec.md
echo "full review" | review all --unstaged
```

## Usage

```
echo "<instructions>" | review <archetype> <flags>
```

Instructions are piped via stdin (required, 20KB limit). A flag telling the agent what to review is always required.

### Archetypes

| Command | Focus |
|---------|-------|
| `review security` | Auth boundaries, injection, secrets, trust assumptions |
| `review bugs` | Logic errors, edge cases, error handling, crashes |
| `review perf` | Allocations, complexity, hot paths, async blocking |
| `review arch` | Coupling, abstractions, API design, consistency |
| `review all` | Fan out to all configured archetypes |

### Flags

| Flag | Context sent to reviewer |
|------|------------------------|
| `--unstaged` | "You are reviewing unstaged changes." |
| `--staged` | "You are reviewing staged changes." |
| `--commit <hash>` | "You are reviewing commit \<hash\>." |
| `--range <a..b>` | "You are reviewing commits \<a..b\>." |
| `--document <path>` | "You are reviewing the file \<path\>." |

The agents fetch the actual content themselves using their project context.

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

Per-project `.review.md` in the project root. Run `review init` to create a starter.

```markdown
---
security:
  claude: "session-abc123"
  codex: "session-def456"
bugs:
  claude: "session-ghi789"
---

# security

Custom security review instructions here.
Overrides the built-in security prompt.

# bugs

Custom bugs review instructions here.
```

The YAML frontmatter maps archetypes to provider session IDs. Markdown `# headings` optionally override the built-in archetype prompts.

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

Both providers run in parallel. If one fails, the other's results are still shown.
