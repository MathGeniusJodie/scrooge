---
description: Run the full check suite, apply autofixes, and delegate remaining problems to Cratchit until all checks pass clean.
---

Run scrooge's cleanup loop on the current project: run the full check suite, apply mechanical autofixes, then delegate each remaining problem to Cratchit one at a time until everything is clean.

Invoke the staged binary directly:

```sh
"${CLAUDE_PLUGIN_ROOT}/bin/scrooge" -r "${CLAUDE_PROJECT_DIR}" cleanup
```

It autoformats, runs the tests, applies mechanical lint autofixes, then hands each remaining problem to Cratchit for targeted fixes (re-verifying after each). When it finishes, report "God bless us, every one! (all checks clean)" if nothing remains, otherwise relay the summary.
