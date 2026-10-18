# Acting on another project

`-C <project>` makes a command act on another project than the one the current
directory is in, for an agent working in one checkout that wants a review of
another. It goes before or after the subcommand:
`echo "check this" | review -C bz -a bugs`, `review config -C bz`.

The current directory never changes. `-C` decides which `.review.toml`, audit
id and project root the command uses; providers run in that project root.

## Names and paths

A selector spelled as a path is a path: it starts with `/`, `./`, `../` or
`~/`, or is `.`, `..` or `~`. It resolves against the current directory and is
searched upward for `.review.toml` like the current directory would be.

Anything else is a **registered name**, looked up in the global config:

```toml
[_projects.myhostname]
bz = "~/Programs/bz"
"folknor/piners" = "$HOME/src/piners"
```

- The table is keyed by host, because a checkout's path is a fact about one
  machine. Other hosts' tables are ignored here.
- A name matches a key ignoring ASCII case, then by the part after a key's last
  `/`, so `piners` finds `folknor/piners`. A short name matching several keys
  is an error; use the full key.
- The path may use `~`, `~/`, `$VAR` and `${VAR}`, and must be absolute after
  expansion. An unset variable, a relative path or a missing directory is an
  error.
- A registered path is the project root itself: its `.review.toml` must sit
  directly in it, never in a parent.
- Registry keys may not be spelled like paths, and two keys may not differ
  only in case.

There is **no fallback** between the two. `-C bz` never means a directory
named `bz`; write `-C ./bz`. A name that does not resolve is an error listing
what this host has registered, so registering a project, or a checkout going
missing, can never silently change what an existing command acts on.
`review config` lists the registered projects for this host.

## Per command

- A run, `review config`, `review init` and `review sessions` act on the
  selected project. `review init -C <name>` creates the file in the
  registered directory.
- `review message <ID>` and `review sessions <ID>` take the project from the
  session's own record; `-C` only asserts it, and a mismatch is an error.
- `review interrupt`, `review incidents`, `review sessions --all` and
  `review man` refuse `-C`, having no project to narrow to.
