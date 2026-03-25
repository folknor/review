# review

A Rust CLI that fans out code reviews to persistent AI sessions across multiple providers, each cultivated with a specific reviewer perspective.

## Core concept

Each project registers **archetypes** - named reviewer perspectives (security, bugs, perf, architecture, etc.) backed by persistent sessions in one or more providers (Claude Code, Codex). Each archetype has its own prompt templates. The tool assembles a review request (prefix + archetype prompt + content), sends it to the archetype's sessions in parallel, and collects the results.

Sessions are long-lived and carry project familiarity, but may be stale. A generic prefix prompt grounds every invocation, telling the reviewer not to trust its memory of the codebase.

## CLI interface

```
review <archetype> <input-source>
```

### Archetypes (first positional arg)

The archetype selects which reviewer sessions to invoke. Defined per-project in config.

```
review security ...
review bugs ...
review perf ...
review arch ...
review all ...          # fan out to every archetype
```

### Input sources (second positional arg or flag)

Exactly one required. No auto-detection.

```
review security --unstaged          # working tree changes (git diff)
review security --staged            # staged changes (git diff --cached)
review security --commit abc123     # diff of a specific commit
review security --range abc..def    # diff across a commit range
review security --branch            # full branch diff against main
review security --document path.md  # a file as-is, not a diff
echo "..." | review security        # stdin pipe, raw input
```

### Output

Responses from all providers for the archetype are printed to stdout, labeled by provider. Designed to be readable by both humans and a calling AI session.

```
--- claude ---
<review content>

--- codex ---
<review content>
```

## Prompt assembly

Every invocation assembles the message sent to each session as:

```
[prefix] + [archetype prompt] + [content]
```

### 1. Prefix (global per project)

Grounding prompt. Sent on every invocation, every archetype. Purpose: counteract stale session context.

```
You are a {archetype} reviewer for {project}.

The codebase may have changed significantly since your last review.
Do NOT assume any function, file, module, or structure still exists
or works the way you last saw it. Only reason about what is explicitly
provided below. Your accumulated project knowledge is background
context for orientation, not ground truth.

Do not execute commands, modify files, or use any tools.
Your response is text analysis only. Read what is provided
below and respond with your review.
```

### 2. Archetype prompt

Perspective-specific instructions. Varies by archetype AND by input type (diff vs document).

Example: `security/diff.md`
```
Focus on:
- Authentication and authorization boundary violations
- Input validation gaps and injection vectors
- Trust assumptions between components
- Secrets or credentials in code
- Race conditions with security implications
```

Example: `security/document.md`
```
Focus on:
- Underspecified auth/trust requirements
- Missing threat model considerations
- Assumptions about transport or storage security
- Gaps where an attacker could exploit ambiguity in the spec
```

### 3. Content

The actual diff, document, or piped input. Passed as-is after the prompts.

## Configuration

Single global config file at `~/.config/review/config.toml`.

```toml
[global]
prefix = "~/.config/review/prompts/prefix.md"

[projects.ratatoskr]
path = "/home/folk/Programs/ratatoskr"

[projects.ratatoskr.archetypes.security]
claude = "session-abc123"
codex = "session-def456"
prompt_diff = "~/.config/review/prompts/security/diff.md"
prompt_document = "~/.config/review/prompts/security/document.md"

[projects.ratatoskr.archetypes.bugs]
claude = "session-mno345"
prompt_diff = "~/.config/review/prompts/bugs/diff.md"
prompt_document = "~/.config/review/prompts/bugs/document.md"

[projects.ratatoskr.archetypes.perf]
claude = "session-ghi789"
codex = "session-jkl012"
prompt_diff = "~/.config/review/prompts/perf/diff.md"
prompt_document = "~/.config/review/prompts/perf/document.md"

[projects.todo]
path = "/home/folk/Programs/todo"
# ...
```

### Project resolution

On invocation, the tool resolves `cwd` against all `projects.*.path` entries. If no match, exit with error:

```
error: current directory is not a registered project
  cwd: /home/folk/Programs/unknown
  hint: add a [projects.<name>] entry in ~/.config/review/config.toml
```

Matching is prefix-based - subdirectories of a project path match that project.

## Session management

### Registering sessions

```
review register <archetype> --claude <session-id>
review register <archetype> --codex <session-id>
```

Updates the config file for the current project. Errors if not in a known project.

### Listing

```
review list                     # all archetypes for current project
review list --all               # all projects, all archetypes
```

### Deregistering

```
review deregister <archetype>               # remove archetype entirely
review deregister <archetype> --claude      # remove just the claude session
```

## Provider invocation

Both providers receive the assembled prompt via stdin to avoid shell argument length limits. Both run in parallel; the tool collects output files and prints them labeled to stdout.

### Claude Code

```
echo "<assembled prompt>" | claude --resume <session-id> --print > /tmp/review-<archetype>-claude.txt
```

Uses `--print` for non-interactive single-response mode. Output is captured to a temp file and read back. The session context provides project familiarity; the prompt provides the specific ask.

### Codex

```
echo "<assembled prompt>" | codex exec resume <session-id> -o /tmp/review-<archetype>-codex.txt
```

- `codex exec` is the non-interactive mode (equivalent of `claude --print`)
- `codex exec resume <session-id>` resumes a session non-interactively
- `-o <file>` captures final output to a file
- Stdin piping works for long prompts
- `--json` gives JSONL event streaming (future use for structured output)
- We do NOT use `codex review` — we control the prompts ourselves

## Error handling

- Unknown project: error with hint to register
- Unknown archetype: error listing available archetypes for the project
- No sessions registered for archetype: error with hint to register
- Provider invocation fails: report which provider failed, still show results from others
- No input source specified: error showing usage

## Future considerations

- `review init <archetype>` - create fresh sessions with an initial project briefing prompt
- Configurable timeout per provider
- `--json` output for programmatic consumption
- Review history / delta tracking
- Custom prefix per archetype (override global)
- Weighting or priority between providers
