#!/usr/bin/env python3
"""Dump the shell commands (and user messages) from a codex rollout.

Reasoning blobs in a rollout are encrypted and their summaries are usually
empty, so the command sequence is the only readable record of what an agent
actually did. Usage:

    python3 scripts/rollout_commands.py <rollout.jsonl> [substring-filter]
"""

import json
import sys


def text_of(item):
    parts = []
    for chunk in item.get("content") or []:
        if isinstance(chunk, dict) and "text" in chunk:
            parts.append(chunk["text"])
    return "".join(parts)


def command_of(item):
    """Pull a command string out of a call item, whatever shape it uses."""
    args = item.get("arguments") or item.get("input")
    if isinstance(args, str):
        try:
            args = json.loads(args)
        except json.JSONDecodeError:
            return args
    if isinstance(args, dict):
        for key in ("command", "cmd", "input", "script"):
            value = args.get(key)
            if isinstance(value, list):
                return " ".join(str(v) for v in value)
            if isinstance(value, str):
                return value
    return None


def main():
    path = sys.argv[1]
    needle = sys.argv[2] if len(sys.argv) > 2 else None

    with open(path, encoding="utf-8") as handle:
        for lineno, line in enumerate(handle, 1):
            try:
                row = json.loads(line)
            except json.JSONDecodeError:
                continue
            payload = row.get("payload") or {}
            kind = payload.get("type")
            stamp = row.get("timestamp", "")

            label = None
            body = None
            if kind == "message" and payload.get("role") == "user":
                label, body = "USER", text_of(payload)
            elif kind in ("custom_tool_call", "function_call", "local_shell_call"):
                label, body = "CALL", command_of(payload)
            elif kind in ("custom_tool_call_output", "function_call_output"):
                label = "OUT"
                out = payload.get("output")
                body = out if isinstance(out, str) else json.dumps(out)

            if label is None or not body:
                continue
            if needle and needle not in body:
                continue
            body = body.strip()
            if len(body) > 1500:
                body = body[:1500] + " …[truncated]"
            print(f"--- {lineno} {stamp} {label}\n{body}\n")


if __name__ == "__main__":
    main()
