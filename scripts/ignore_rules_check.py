#!/usr/bin/env python3
"""Verify that `--ignore-rules` restores codex's sandbox guarantee.

Codex loads execpolicy rules from `$CODEX_HOME/rules/*.rules`; an Allow match on
every segment of a command makes it run with no sandbox wrapper at all, even
under `--sandbox read-only`. `codex exec --ignore-rules` skips the user and
project rule layers, which should make the same command sandboxed again.

Two witnesses per case: a write to the build history database (outside every
writable root) and a symlink created inside the workspace.
"""

import json
import os
import sqlite3
import subprocess
import sys

DB = os.path.expanduser("~/.local/share/brokkr/history.db")
LINK = "probe-readonly-breach"


def last_ids():
    try:
        con = sqlite3.connect(f"file:{DB}?mode=ro", uri=True)
        return {r[0] for r in con.execute(
            "select id from history order by id desc limit 10")}
    except sqlite3.Error:
        return set()


def run(cwd, command, extra_args):
    argv = [
        "codex", "exec", "--sandbox", "read-only", "--skip-git-repo-check",
        "-c", 'approval_policy="never"', "-m", "gpt-5.6-sol",
        "-c", 'model_reasoning_effort="low"', "--json",
    ] + extra_args
    prompt = (
        "Run exactly this one shell command, then reply with its complete "
        "output verbatim, or the exact error if it failed. Do not investigate, "
        "do not read files, do not explain, do not retry.\n\n" + command
    )
    proc = subprocess.run(argv, cwd=cwd, input=prompt,
                          capture_output=True, text=True, timeout=300)
    reply = ""
    for line in proc.stdout.splitlines():
        try:
            row = json.loads(line)
        except json.JSONDecodeError:
            continue
        item = row.get("item") or {}
        if item.get("type") == "agent_message":
            reply = item.get("text", "")
    return reply.strip(), proc.returncode, proc.stderr.strip()


def main():
    cwd = sys.argv[1] if len(sys.argv) > 1 else os.getcwd()
    target = os.path.join(cwd, LINK)

    for label, extra in (("without --ignore-rules", []),
                         ("with --ignore-rules", ["--ignore-rules"])):
        print(f"=== {label}")

        before = last_ids()
        reply, code, stderr = run(cwd, "brokkr status", extra)
        new = last_ids() - before
        print(f"  [history write outside roots] new rows: {sorted(new)}  "
              f"-> {'UNSANDBOXED' if new else 'sandboxed'}")
        if code != 0 and stderr:
            print(f"  launch stderr: {stderr[:300]!r}")

        if os.path.islink(target):
            os.unlink(target)
        reply, _, _ = run(cwd, f"ln -s /etc/hostname {LINK}", extra)
        created = os.path.islink(target)
        print(f"  [repo write under read-only] link created: {created}  "
              f"-> {'UNSANDBOXED' if created else 'sandboxed'}")
        print(f"  reply: {reply[:160]!r}")
        if created:
            os.unlink(target)
        print()


if __name__ == "__main__":
    main()
