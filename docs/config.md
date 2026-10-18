# Configuration

Settings resolve from the command line, then the project's `.review.toml`,
then the operator's global config: `$XDG_CONFIG_HOME/review/config.toml`, else
`~/.config/review/config.toml`. Both files share one format. Nothing is built
in.

**`review config` is the authoritative view.** It runs the same resolver a run
uses and shows every effective value with the file and table it came from, and
what each one shadows. Reading `.review.toml` alone gives an incomplete and
often wrong answer, because profiles and archetypes may live in the global
file. Outside a project it shows the global layer alone.

## Finding the project file

`review` walks up from the current directory to the first `.review.toml`,
stopping at the git root. A run requires one, because its `[_audit]` block
identifies the project; a file holding only `[_audit]` is a complete config.
`review init` creates a starter file in the current directory (or in the
selected one with `-C`), refusing if one already exists there or above it.
`-C` picks another project (see `review man projects`).

A config that exists but does not parse is always an error, never treated as
absent.

## Sections

```toml
[archetypes]
security = "You are a security expert. Read the codebase."
bugs = "You hunt for edge cases and correctness bugs."

[_groups]
sweep = ["security", "bugs"]

[_defaults]
providers = ["claude", "codex"]
# Seconds of silence after which a codex run counts as stalled. 0 disables;
# omitted uses the built-in default. Codex only.
stall_timeout_secs = 900

# Profiles: see `review man profiles`.
[codex.deep]
model = "gpt-5.6-terra"
effort = "high"
sandbox = "read-only"

# Project file only.
[_audit]
id = "43fd"
private = false

# Global file only: see `review man projects`.
[_projects.myhostname]
bz = "~/Programs/bz"
```

- `[archetypes]`: name to priming prompt. The prompt owns the session's role
  and whether it should modify files; `review` adds nothing of its own.
- `[_groups]`: name to a non-empty list of archetypes, without duplicates.
- `[_defaults]`: `providers` (the list used without `--provider`) and
  `stall_timeout_secs`.
- `[_audit]`: `id` names the project's audit trail and is generated on the
  first run if missing. `private = true` files the project's sessions in a
  separate private log.
- `[_projects.<host>]`: the `-C` registry.
- Any other top-level table is a profile table, `[<provider>.<profile>]`, or a
  legacy host-scoped one, `[<host>.<provider>.<profile>]`.

## How the two files combine

- **Archetypes and groups** are a union; for a name both define, the project
  wins. That holds even between a group and an archetype of the same name, so a
  group added to the global file cannot break a project with an archetype of
  that name. Within one file such a clash is an error.
- A **project group** may name a global archetype. A **global group** may name
  only global archetypes, since the global file is read in every project.
- **`providers` and `stall_timeout_secs`** come from the first layer that sets
  them, each on its own. An explicit empty list counts as set.
- **Profiles** resolve to their first definition and win whole, never merged
  field by field (see `review man profiles`).
- **`[_audit]`** is the project's alone and is an error in the global file.
  **`[_projects]`** is the global file's alone and is an error in a project.

## Reserved words

An archetype or group may not be named any of these:

| Reserved word | Why |
|---|---|
| `all` | the `-a` keyword for every configured archetype |
| `message` | a subcommand |
| `resume` | a subcommand (hidden alias of `message`) |
| `interrupt` | a subcommand |
| `config` | a subcommand |
| `sessions` | a subcommand |
| `incidents` | a subcommand |
| `init` | a subcommand |
| `man` | a subcommand |
| `help` | clap's generated subcommand |

Subcommand names are reserved because the legacy positional archetype form
(`review sessions`) would run the subcommand instead, silently.

`bare` is a special case: the legacy `review bare` means "no archetype", so an
archetype named `bare` may only have an empty prompt (`bare = ""`), and no group
may be named `bare`.
