#!/usr/bin/env python3
"""Which command-line flags actually close the sandbox-bypass hole.

Execpolicy rules live in `$CODEX_HOME/rules/*.rules`, separate from
`config.toml`, so the two isolation flags cover different ground:

  --ignore-user-config  drops $CODEX_HOME/config.toml (auth still resolves)
  --ignore-rules        drops the user and project execpolicy rule layers

Witness: `ln -s` is allowlisted in the operator's rules, so under
`--sandbox read-only` it creates a link if and only if the bypass is active.
"""

import json
import os
import subprocess
import sys

LINK = "probe-flag-matrix"

BASE = ["codex", "exec", "--sandbox", "read-only", "--skip-git-repo-check",
        "--json", "-m", "gpt-5.6-sol", "-c", 'model_reasoning_effort="low"',
        "-c", 'approval_policy="never"']

CASES = [
    ("neither flag", []),
    ("--ignore-user-config only", ["--ignore-user-config"]),
    ("--ignore-rules only", ["--ignore-rules"]),
    ("both", ["--ignore-user-config", "--ignore-rules"]),
]


def run(cwd, extra, command):
    prompt = ("Run exactly this one shell command, then reply with only its "
              "result. Do not investigate, do not read files, do not "
              "explain.\n\n" + command)
    proc = subprocess.run(BASE + extra, cwd=cwd, input=prompt,
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
    for label, extra in CASES:
        if os.path.islink(target):
            os.unlink(target)
        reply, code, stderr = run(cwd, extra, f"ln -s /etc/hostname {LINK}")
        created = os.path.islink(target)
        launched = not (code != 0 and "Error:" in stderr)
        print(f"{label:28s} launched={launched}  "
              f"{'UNSANDBOXED' if created else 'sandboxed'}")
        if not launched:
            print(f"  stderr: {stderr[:300]!r}")
        if created:
            os.unlink(target)


if __name__ == "__main__":
    main()
