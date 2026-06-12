#!/usr/bin/env python3
"""Stop hook: when source files changed this session (.scrooge/dirty, set by
the PostToolUse hook), run `scrooge check`. Test/build failures block the stop
and are handed back to Scrooge to fix; leftover lint warnings block once and
are delegated to Cratchit. A retry cap stops infinite fix loops."""
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
            f"scrooge check: still failing after {MAX_ATTEMPTS} fix attempts, giving up"}))
    sys.exit(0)


if attempts() >= MAX_ATTEMPTS:
    finish(clean=False)

binary = os.path.join(os.environ.get("CLAUDE_PLUGIN_ROOT", ""), "bin", "scrooge")
try:
    result = subprocess.run(
        [binary, "-r", cwd, "check"],
        capture_output=True, text=True, timeout=600,
    )
except Exception:
    sys.exit(0)  # never trap the user in a broken hook

if result.returncode == 0:
    finish(clean=True)

next_attempt = attempts() + 1
with open(attempts_file, "w") as f:
    f.write(str(next_attempt))

report = result.stdout.strip()
if not report or result.returncode not in (1, 2):
    sys.exit(0)  # binary mismatch or internal error, not a check verdict
if result.returncode == 1:
    reason = (
        "Post-task checks failed: the build/tests are broken after your changes. "
        "Fix these errors yourself (you are Scrooge — correctness is planner work), "
        "then stop again to re-run the checks.\n\n" + report
    )
else:
    reason = (
        "Tests pass, but lint warnings remain after mechanical autofix. Delegate "
        "fixing them to Cratchit via give_cratchit_task (or the cratchit agent) — "
        "do not burn Scrooge tokens on warning cleanup. If a warning is a false "
        "positive, you may instead adjust .scrooge/checks.toml or add a suppression.\n\n"
        + report
    )

print(json.dumps({"decision": "block", "reason": reason}))
