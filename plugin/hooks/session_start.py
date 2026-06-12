#!/usr/bin/env python3
"""SessionStart hook: inject the free code-map brief so Claude starts every
session already knowing the codebase shape."""
import json
import os
import subprocess
import sys

data = json.load(sys.stdin)
binary = os.path.join(os.environ.get("CLAUDE_PLUGIN_ROOT", ""), "bin", "scrooge")
try:
    brief = subprocess.run(
        [binary, "-r", data.get("cwd", "."), "map"],
        capture_output=True, text=True, timeout=30,
    ).stdout.strip()
except Exception:
    sys.exit(0)
if not brief:
    sys.exit(0)

print(json.dumps({
    "hookSpecificOutput": {
        "hookEventName": "SessionStart",
        "additionalContext": (
            "Codebase brief (from scrooge, free — prefer the scrooge MCP tools "
            "symbol_info/callers/callees/helpers and give_cratchit_task over "
            "reading files yourself):\n" + brief
        ),
    }
}))
