# Scrooge best practices

Sections are matched by keywords in brackets; only relevant ones are sent to the agent.

## rust [rust, cargo, crate, borrow, lifetime, .rs]
- Prefer `?` over unwrap; use `anyhow::Result` in binaries, `thiserror` in libraries.
- Don't clone to satisfy the borrow checker until you've tried restructuring.
- Minimize allocations.

## python [python, pip, venv, pytest, .py]
- Use type hints on public functions.
- Run code through `python3 -m py_compile` before declaring done; run pytest if tests exist.
- Prefer stdlib over new dependencies.

## javascript [javascript, html, dom, browser, node, .js, .ts, .html, .css]
- Prefer vanilla DOM APIs over adding libraries.
- Use `const`/`let`, never `var`. Use strict equality.
- Proper ARIA spec compliance

## testing [test, tests, pytest, assert, coverage]
- Write the failing test first when fixing a bug.
- Test behavior, not implementation details.

## editing [edit, refactor, rename, change, modify]
- Check callers of a function (call graph) before changing its signature.
- Don't just make the smallest change that accomplishes the task, refactor as if you were writing the code cleanly and correctly from the start if you feel it's warranted.
- Match the style of surrounding code.

## math [math, calculate, compute, number, formula, equation]
- Never do arithmetic in your head: use the python or wolfram tool, always.
- Verify numeric results with a second method when they matter.

## dependencies [dependency, install, package, library, import]
- Query documentation before using any unfamiliar API; do not guess signatures.
- Prefer what's already in the project over adding new dependencies.
