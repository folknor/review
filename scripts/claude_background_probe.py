#!/usr/bin/env python3
"""Probe how `claude --print` handles background work, timeouts and failures.

The evidence behind `run_claude`'s environment (`reference/claude.md`, "Shell
commands stay in the foreground"). Re-run after a Claude Code upgrade: each
probe costs a real turn, and the long ones take a minute or more.

Each probe launches claude the way `review` does (fresh --session-id, --print,
--permission-mode dontAsk, prompt on stdin, cwd = this repo) but, by default,
with --output-format stream-json so every event is visible with its arrival
time. The background commands are `python3 -c ...` rather than `sleep` so that
an allowlist naming `Bash(python3 *)` lets a dontAsk run execute them.

Output lands in target/claude-probes/<probe>[-<tag>]/:

  events.jsonl      each stdout line, prefixed by seconds since launch
  stderr.txt        stderr
  summary.txt       exit code, wall time, leftover marker processes, transcript
  transcript.jsonl  a copy of claude's on-disk session transcript, if found

Usage: claude_background_probe.py <probe> [--format=text|json|stream-json]
           [--tag=T] [KEY=VALUE ...] [extra claude args...]

--format picks claude's --output-format (default stream-json, which adds
--verbose). KEY=VALUE pairs are set in claude's environment - `review` sets
CLAUDE_CODE_DISABLE_BACKGROUND_TASKS=1 and BASH_DEFAULT_TIMEOUT_MS /
BASH_MAX_TIMEOUT_MS, so pass those to reproduce a `review` run and leave them
out to see claude's own defaults. --tag names the output directory, so variants
of one probe do not collide.

What each probe showed on Claude Code 2.1.284, defaults unless stated:

  bg-bash-natural     the model backgrounded the command, said it would report
                      when it finished, ended its turn; claude killed the
                      command and exited 0. With background disabled: ran in
                      the foreground and reported the marker.
  bg-bash-walkaway    same kill. With background disabled the Bash tool has no
                      run_in_background parameter at all.
  bg-agent-walkaway   waited for the background subagent, woke the model with
                      its result in a second turn; json/text output carry only
                      that last turn's result.
  long-foreground     background disabled: killed at Bash's 2-minute default
                      timeout (the model's own `timeout` was denied by the
                      allowlist); with BASH_DEFAULT_TIMEOUT_MS raised, it ran to
                      completion.
  bad-model           (pass --model no-such-model-xyz) exit 1 with a result
                      object: is_error true, subtype "success", the session id,
                      and a transcript on disk.
"""

import glob
import json
import os
import shutil
import subprocess
import sys
import time
import uuid

REPO = os.path.abspath(os.path.join(os.path.dirname(os.path.abspath(__file__)), ".."))

PY_SLEEP = "python3 -c \"import time; time.sleep({secs}); print('{marker}')\""

PREAMBLE = "This is a harness probe, not a review. Do not edit any file.\n\n"

PROBES = {
    # The natural shape: nothing tells the model to walk away. What does an
    # unsteered model do with a long command it is asked to background?
    "bg-bash-natural": (
        PREAMBLE
        + "Run this command in the background, and when it finishes, report the "
        "exact text it printed:\n\n"
        + PY_SLEEP.format(secs=60, marker="PROBE-C-DONE")
    ),
    # The model is told to background a command and end its turn at once. Does
    # claude --print exit, wait for the task, or wake the model with its result?
    "bg-bash-walkaway": (
        PREAMBLE
        + "Use the Bash tool with run_in_background set to true to start exactly "
        "this command:\n\n"
        + PY_SLEEP.format(secs=60, marker="PROBE-A-DONE")
        + "\n\nThen immediately end your turn, replying with only the word "
        "LAUNCHED. Do not wait for the command, and do not check its output. "
        "If you are later given its result, reply with the exact text it printed."
    ),
    # Same, with a background subagent.
    "bg-agent-walkaway": (
        PREAMBLE
        + "Use the Agent tool with run_in_background set to true to launch one "
        "general-purpose subagent with this task: \"Run exactly this command "
        "with the Bash tool in the foreground and report the exact text it "
        "printed: "
        + PY_SLEEP.format(secs=45, marker="PROBE-B-DONE")
        + "\"\n\nThen immediately end your turn, replying with only the word "
        "LAUNCHED. Do not wait for the subagent. If you are later given its "
        "result, reply with the exact text it reported."
    ),
    # Longer than Bash's default 2-minute timeout, with no mention of background
    # or timeouts.
    "long-foreground": (
        PREAMBLE
        + "Run this command and report the exact text it printed:\n\n"
        + PY_SLEEP.format(secs=150, marker="PROBE-D-DONE")
    ),
    # A launch failure, given an unknown --model: exit code, and does the output
    # still name a session?
    "bad-model": "Reply with exactly: HELLO",
}


def find_transcript(session_id):
    pattern = os.path.expanduser(f"~/.claude/projects/*/{session_id}.jsonl")
    hits = glob.glob(pattern)
    return hits[0] if hits else None


def leftover_markers():
    out = subprocess.run(["ps", "-eo", "pid,args"], capture_output=True, text=True).stdout
    return [line.strip() for line in out.splitlines() if "PROBE-" in line and "ps -eo" not in line]


def main():
    if len(sys.argv) < 2 or sys.argv[1] not in PROBES:
        sys.exit(f"usage: {sys.argv[0]} <{'|'.join(PROBES)}> [options]")
    name = sys.argv[1]
    fmt = "stream-json"
    tag = None
    env = dict(os.environ)
    overrides = []
    extra = []
    for arg in sys.argv[2:]:
        if arg.startswith("--format="):
            fmt = arg.split("=", 1)[1]
        elif arg.startswith("--tag="):
            tag = arg.split("=", 1)[1]
        elif "=" in arg and not arg.startswith("-"):
            key, value = arg.split("=", 1)
            env[key] = value
            overrides.append(arg)
        else:
            extra.append(arg)
    out_dir = os.path.join(REPO, "target", "claude-probes", f"{name}-{tag}" if tag else name)
    os.makedirs(out_dir, exist_ok=True)

    session_id = str(uuid.uuid4())
    args = [
        "claude",
        "--session-id", session_id,
        "--print",
        "--permission-mode", "dontAsk",
        "--output-format", fmt,
    ]
    if fmt == "stream-json":
        args.append("--verbose")
    args += extra

    start = time.monotonic()
    proc = subprocess.Popen(
        args,
        cwd=REPO,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        env=env,
    )
    proc.stdin.write(PROBES[name])
    proc.stdin.close()

    with open(os.path.join(out_dir, "events.jsonl"), "w") as events:
        for line in proc.stdout:
            events.write(f"{time.monotonic() - start:8.2f} {line}")
            events.flush()
    stderr = proc.stderr.read()
    code = proc.wait()
    wall = time.monotonic() - start

    with open(os.path.join(out_dir, "stderr.txt"), "w") as f:
        f.write(stderr)

    # Marker processes still running right after claude exited = work it
    # abandoned. Probes run concurrently show up in each other's lists; tell
    # them apart by marker letter.
    at_exit = leftover_markers()

    transcript = find_transcript(session_id)
    if transcript:
        shutil.copy(transcript, os.path.join(out_dir, "transcript.jsonl"))

    summary = [
        f"session_id: {session_id}",
        f"args: {json.dumps(args)}",
        f"env overrides: {overrides}",
        f"exit_code: {code}",
        f"wall_secs: {wall:.1f}",
        f"marker processes alive at exit: {at_exit or 'none'}",
        f"transcript: {transcript or 'not found'}",
    ]
    with open(os.path.join(out_dir, "summary.txt"), "w") as f:
        f.write("\n".join(summary) + "\n")
    print("\n".join(summary))


if __name__ == "__main__":
    main()
