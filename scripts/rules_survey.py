#!/usr/bin/env python3
"""Summarise a codex execpolicy rules file by decision and leading program.

An `allow` prefix rule does not merely skip approval - when every parsed segment
of a command matches one, codex runs that command with no sandbox wrapper at
all, whatever `--sandbox` says. So the set of `allow` rules is the exact set of
command prefixes for which the sandbox guarantee does not hold.
"""

import collections
import re
import sys

DEFAULT = "/home/folk/.codex/rules/default.rules"


def main():
    path = sys.argv[1] if len(sys.argv) > 1 else DEFAULT
    by_decision = collections.Counter()
    programs = collections.Counter()
    with open(path, encoding="utf-8") as handle:
        for line in handle:
            match = re.search(r'pattern=\[(.*?)\],\s*decision="(\w+)"', line)
            if not match:
                continue
            parts = re.findall(r'"((?:[^"\\]|\\.)*)"', match.group(1))
            decision = match.group(2)
            by_decision[decision] += 1
            if decision == "allow" and parts:
                lead = parts[0]
                # `env VAR=... prog` rules really allow `prog`.
                idx = 0
                while idx < len(parts) - 1 and (parts[idx] == "env" or "=" in parts[idx]):
                    idx += 1
                lead = parts[idx]
                programs[lead] += 1

    print(f"{path}\n")
    print("rules by decision:")
    for decision, count in by_decision.most_common():
        print(f"  {decision:8s} {count}")
    print("\nprograms granted unsandboxed execution (allow rules):")
    for program, count in programs.most_common():
        print(f"  {count:4d}  {program}")


if __name__ == "__main__":
    main()
