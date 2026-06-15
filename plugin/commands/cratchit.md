---
description: Hand a coding task straight to Cratchit — no Scrooge planning, the cheap executor does it directly.
---

Hand this task directly to Cratchit, skipping the Scrooge planning step.

Task: $ARGUMENTS

Do NOT plan it yourself or read source files. Dispatch the whole task in one go:

- If the environment variable `CRATCHIT_BACKEND` is `subagent`, send the task to the `cratchit` subagent.
- Otherwise (default), call the `give_cratchit_task` MCP tool with the task as both the task and the instructions.

When Cratchit reports back, relay a one-line summary plus the CHECKS verdict and the token bill.
