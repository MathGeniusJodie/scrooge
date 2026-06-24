#!/usr/bin/env python3
"""Stop hook: when source files changed this session (.scrooge/dirty, set by
the PostToolUse hook), run `scrooge cleanup`. Cleanup runs the check suite,
applies mechanical autofixes, then delegates each remaining problem to Cratchit
one at a time (re-verifying after each). If it still can't clean up after
MAX_ATTEMPTS, the failure is reported to Scrooge."""
import json
import os
import subprocess
import sys

MAX_ATTEMPTS = 5

data = json.load(sys.stdin)
cwd = data.get("cwd", ".")
scrooge_dir = os.path.join(cwd, ".scrooge")
dirty = os.path.join(scrooge_dir, "dirty")
attempts_file = os.path.join(scrooge_dir, "check_attempts")

if not os.path.exists(dirty):
    sys.exit(0)


def attempts():
    try:
        with open(attempts_file) as f:
            return int(f.read().strip())
    except Exception:
        return 0


def finish(clean):
    for path in (dirty, attempts_file):
        try:
            os.remove(path)
        except FileNotFoundError:
            pass
    if not clean:
        print(json.dumps({"systemMessage":
            f"scrooge cleanup: still failing after {MAX_ATTEMPTS} fix attempts, giving up"}))
    sys.exit(0)


if attempts() >= MAX_ATTEMPTS:
    finish(clean=False)

binary = os.path.join(os.environ.get("CLAUDE_PLUGIN_ROOT", ""), "bin", "scrooge")
try:
    result = subprocess.run(
        [binary, "-r", cwd, "cleanup"],
        capture_output=True, text=True, timeout=900,
    )
except Exception:
    sys.exit(0)  # never trap the user in a broken hook

if result.returncode == 0:
    # Cleanup succeeded (all checks clean). Have Cratchit reconsider
    # .scrooge/overview.md against the diff and rewrite it if stale.
    # Best-effort — never blocks the stop.
    try:
        subprocess.run(
            [binary, "-r", cwd, "refresh-overview"],
            capture_output=True, timeout=300,
        )
    except Exception:
        pass
    finish(clean=True)

next_attempt = attempts() + 1
with open(attempts_file, "w") as f:
    f.write(str(next_attempt))

report = result.stdout.strip()
if not report or result.returncode not in (1, 2):
    sys.exit(0)  # binary mismatch or internal error, not a check verdict
if result.returncode == 1:
    reason = (
        "Post-task cleanup failed: the build/tests are broken. Mechanical failures "
        "are Cratchit's work — dispatch give_cratchit_task with the failure output "
        "below as the instructions. Only fix it yourself if Cratchit has already "
        "failed twice on the same failure. Then stop again to re-run cleanup.\n\n"
        + report
    )
else:
    reason = (
        "Tests pass, but lint warnings remain after cleanup attempted fixes. Delegate "
        "fixing them to Cratchit via give_cratchit_task (or the cratchit agent) — "
        "do not burn Scrooge tokens on warning cleanup. If a warning is a false "
        "positive, you may instead adjust .scrooge/checks.toml or add a suppression.\n\n"
        + report
    )

print(json.dumps({"decision": "block", "reason": reason}))
