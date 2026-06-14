---
description: Hunt down every humbug — run scrooge's full check suite (format, tests, lint).
---

Run scrooge's full check suite on the current project and report every problem.

Invoke the staged binary directly:

```sh
"${CLAUDE_PLUGIN_ROOT}/bin/scrooge" -r "${CLAUDE_PROJECT_DIR}" humbugs
```

It autoformats, runs the tests, applies mechanical lint autofixes, then reports
whatever's left (per-language commands come from `.scrooge/checks.toml`). Exit
code 0 means clean, 1 means test/build failures, 2 means lint warnings remain.

Relay the verdict concisely: "clean" if nothing remains, otherwise the
failures/warnings per language. Offer to dispatch any leftovers to Cratchit.
