# Sandbox levels

A profile's `sandbox` says what a run may modify. It defaults to `read-only`,
so a run with no profile cannot change files. **Claude ignores the setting**:
what a claude run can touch is decided by Claude Code's own permission
configuration, and `review` records no level for it.

The levels are `review`'s own names, translated per provider:

| `sandbox` | codex | grok | claude |
|---|---|---|---|
| `read-only` | `read-only` | `read-only` | ignored |
| `workspace-write` | `workspace-write` | `workspace` | ignored |
| `danger-full-access` | `danger-full-access` | `none` | ignored |

Any other value is passed to the provider unchanged, for a provider that
defines its own named profiles; the provider rejects a name it does not know
before running a turn. These three names always mean the levels above.

## read-only

The run can read the filesystem and cannot write to the project or anywhere
else the operator owns. Grok's read-only level still lets it write its own
home and temporary directories. On codex this holds no matter what the
operator's own codex configuration says: `review` pins the
settings that could otherwise widen it. A read-only codex run can still make
network connections, including to sockets of services already running.

The one way to widen it is deliberate: a profile's codex `config` overrides
come after `review`'s own settings, so one that restates the approval policy,
the permission settings or network access replaces them. The guarantees on this
page assume a profile that does not.

## workspace-write

The run can write inside the project directory, plus the roots below, plus
whatever the provider itself always grants: temporary directories on both, and
on grok its own home. Codex also keeps some paths inside the project read-only
on its own account, such as `.git`. On codex the run has network access too,
which is what lets build and test suites use unix sockets.

On grok, a write run first marks the project as trusted in grok's folder-trust
store, because grok cancels every edit in an untrusted folder. That grant is
**permanent and host-wide**: from then on grok also runs that repo's own
MCP and LSP configuration without asking, in every grok session. The run that
grants it says so. A folder explicitly marked untrusted is never overridden;
the run is refused instead.

## Writable roots

A build needs to write outside the project, so a `workspace-write` run on codex
or grok is also granted these, when they lie outside the project:

- brokkr's build lock and worktree directory, `$HOME/.brokkr`;
- the cargo home, `$CARGO_HOME`, else `$HOME/.cargo`;
- brokkr's command history, under `$XDG_DATA_HOME`, else `$HOME/.local/share`;
- `$CARGO_TARGET_DIR`, and the target of a `./target` symlink;
- the `.git/lfs` store of a sibling checkout that holds a path dependency, when
  that store exists.

The first three are granted whether or not they exist yet, since a build may
create them.

Every grant is printed at launch with its reason, and recorded with the run.

A profile's `writable_roots` adds to these, never replaces them. Each entry,
after expanding `~`, `$VAR` and `${VAR}`, must be an absolute path. An entry
that exists is resolved first - symlinks followed, `..` collapsed - and the
filters apply to where it actually leads, so an alias cannot reach what its
spelling would not; one that does not exist yet is checked, and granted, as
written. Either way it is refused if it is `/`, still contains `..`, or is the
project itself or one of its parents. A sibling of the project is allowed. An
entry naming an unset variable is dropped with a warning rather than expanded
to nothing.

The exception: a codex profile whose `config` sets
`sandbox_workspace_write.writable_roots` **replaces** the whole set - derived
roots and `writable_roots` alike - with exactly what it lists, and those paths
pass through none of the filters above. The run records what it actually got.

## What a resumed session gets

`review message` keeps the session's own level, from the last run recorded for
it: a `workspace-write` session stays writable.

- **Codex**: the recorded roots are passed again as profile roots, so they go
  back through the filters, and the build roots are derived afresh for this
  host. A root that came from a `config` override is recorded like any other,
  so it carries over if it passes the filters; what is lost is the override
  itself - its replacing the whole set, and its exemption from the filters.
- **Grok**: a session started under one of `review`'s write profiles resumes
  under exactly that profile and its recorded roots, because grok fixes a
  session's sandbox for its lifetime. An older session recorded as plain
  `workspace` resumes that way, with no extra roots; one with no recorded level
  resumes under whatever grok saved for it.

## When a write fails

A run that reports a read-only filesystem was launched `read-only`, is writing
outside its roots, or is writing a path the provider protects (such as `.git`
on codex). `review sessions <ID>` shows the level and roots it
launched with. Then:

- launched `read-only`: use a profile with `workspace-write`;
- writing outside its roots: add the path to the profile's `writable_roots`;
- writing a path the provider protects: `writable_roots` does not lift that
  protection. `danger-full-access` or a deliberate codex permission override
  would, at the cost of the whole sandbox; usually do that step outside the
  agent instead.

Never work around it by moving `HOME` or `CARGO_TARGET_DIR` inside the project:
that silently bypasses the host's shared build lock.
