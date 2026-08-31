#!/usr/bin/env python3
"""Measure whether a codex `read-only` run can use sockets, and how to grant it.

`sandbox_workspace_write.network_access` has no `read-only` counterpart: the
legacy `sandbox_mode` pipeline maps `SandboxMode::ReadOnly` to
`PermissionProfile::read_only()` with network hardcoded to `Restricted`. The
only surface that can express "read-only filesystem, network enabled" is
codex's named-permissions pipeline - and `resolve_permission_config_syntax`
returns `Legacy` (making `[permissions]` profiles inert) whenever a
`sandbox_mode` override is present, so selecting a profile means *not* passing
`--sandbox`.

This script measures three configurations against the same witness program:

  legacy-read-only   `--sandbox read-only`, what review passes today
  profile-restricted the permissions profile with network left restricted,
                     which must reproduce legacy-read-only exactly - the
                     control that proves the profile is not silently widening
                     the filesystem
  profile-network    the same profile with `network.enabled = true`

What must hold for the change to be safe: `write_cwd` stays denied in all
three, `read_etc_hostname` stays allowed in all three, and only the socket
lines move.

Costs real codex turns. Run it deliberately after a codex upgrade.
"""

import json
import os
import socket
import subprocess
import sys
import threading

PROFILE = "review-read-only"

COMMON = [
    "exec",
    "--skip-git-repo-check",
    "--ignore-rules",
    "--ignore-user-config",
    "-c", 'approval_policy="never"',
    "-c", 'model_reasoning_effort="low"',
    "--json",
]

PROFILE_BASE = [
    "-c", f'permissions.{PROFILE}.extends=":read-only"',
    "-c", f'default_permissions="{PROFILE}"',
]

CONFIGS = [
    ("legacy-read-only", ["--sandbox", "read-only"]),
    ("profile-restricted",
     PROFILE_BASE + ["-c", f"permissions.{PROFILE}.network.enabled=false"]),
    ("profile-network",
     PROFILE_BASE + ["-c", f"permissions.{PROFILE}.network.enabled=true"]),
]


def serve(path, stop):
    """Listen on an AF_UNIX socket so the probe has something to connect to."""
    srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    srv.bind(path)
    srv.listen(8)
    srv.settimeout(0.5)
    while not stop.is_set():
        try:
            conn, _ = srv.accept()
        except socket.timeout:
            continue
        except OSError:
            break
        conn.recv(64)
        conn.close()
    srv.close()


def run(cwd, extra, unix_path, model):
    argv = ["codex"] + COMMON + ["-m", model] + extra
    prompt = (
        "Run exactly this one shell command, then reply with its complete "
        "stdout and stderr verbatim and nothing else. Do not investigate, do "
        "not read files, do not explain, do not retry, do not fix anything.\n\n"
        f"python3 scripts/sandbox_probe.py {unix_path}"
    )
    proc = subprocess.run(argv, cwd=cwd, input=prompt,
                          capture_output=True, text=True, timeout=600)
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


def witnesses(reply):
    """Parse `NAME: result` lines between the probe's begin/end markers.

    The witness names contain colons (`tcp_connect_1.1.1.1:443`), so split on
    the *last* colon rather than the first.
    """
    out = {}
    seen_begin = False
    for line in reply.splitlines():
        line = line.strip().strip("`")
        if line == "PROBE-BEGIN":
            seen_begin = True
            continue
        if line == "PROBE-END":
            break
        if not seen_begin or ":" not in line:
            continue
        name, _, result = line.rpartition(":")
        if name.strip() and result.strip():
            out[name.strip()] = result.strip()
    return out


def verdict(result):
    """Collapse a witness result to a short token so the table lines up."""
    return result.split()[0] if result else "-"


def main():
    cwd = sys.argv[1] if len(sys.argv) > 1 else os.getcwd()
    model = os.environ.get("PROBE_MODEL", "gpt-5.6-sol")
    unix_path = os.path.join(cwd, "probe.sock")
    if os.path.exists(unix_path):
        os.unlink(unix_path)

    stop = threading.Event()
    thread = threading.Thread(target=serve, args=(unix_path, stop), daemon=True)
    thread.start()

    results = {}
    try:
        for label, extra in CONFIGS:
            print(f"=== {label}")
            print(f"    codex {' '.join(COMMON + ['-m', model] + extra)}")
            reply, code, stderr = run(cwd, extra, unix_path, model)
            if code != 0 and stderr:
                print(f"    launch stderr: {stderr[:400]!r}")
            got = witnesses(reply)
            results[label] = got
            if not got:
                print(f"    no witnesses parsed; raw reply: {reply[:400]!r}")
            for name, value in got.items():
                print(f"    {name:28} {value}")
            print()
    finally:
        stop.set()
        thread.join(timeout=2)
        if os.path.exists(unix_path):
            os.unlink(unix_path)

    names = sorted({n for got in results.values() for n in got})
    if names:
        print("=== summary")
        width = max(len(n) for n in names)
        labels = [label for label, _ in CONFIGS]
        col = max(len(label) for label in labels)
        print(f"{'':{width}} " + " ".join(f"{label:>{col}}" for label in labels))
        for name in names:
            row = " ".join(
                f"{verdict(results.get(label, {}).get(name, '')):>{col}}"
                for label in labels)
            print(f"{name:{width}} {row}")


if __name__ == "__main__":
    main()
