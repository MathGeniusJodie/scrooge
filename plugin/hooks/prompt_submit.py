#!/usr/bin/env python3
"""UserPromptSubmit hook: inject only the best-practice sections whose
keywords match the prompt — never the whole document."""
import json
import os
import subprocess
import sys

data = json.load(sys.stdin)
prompt = data.get("prompt", "")
if not prompt.strip():
    sys.exit(0)
binary = os.path.join(os.environ.get("CLAUDE_PLUGIN_ROOT", ""), "bin", "scrooge")
try:
    out = subprocess.run(
        [binary, "-r", data.get("cwd", "."), "practices", prompt],
        capture_output=True, text=True, timeout=15,
    ).stdout.strip()
except Exception:
    sys.exit(0)
# The matcher returns a fallback sentence when nothing matches; skip it.
if not out or out.startswith("no specific guidance"):
    sys.exit(0)

print(json.dumps({
    "hookSpecificOutput": {
        "hookEventName": "UserPromptSubmit",
        "additionalContext": "Relevant best practices (scrooge):\n" + out,
    }
}))
