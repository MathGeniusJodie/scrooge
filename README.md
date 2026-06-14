# Scrooge Coding Agent (WIP)

![scrooge logo](https://github.com/MathGeniusJodie/scrooge/blob/master/logo.png?raw=true)

A token-miserly coding agent. **Scrooge** (expensive SOTA model) only ever sees
terse briefs and reports. He plans and reviews, never reads files or
writes code. **Cratchit** (cheap model) does all the work.

override with
`SCROOGE_MODEL` and `CRATCHIT_MODEL` env vars. Requires `OPENROUTER_API_KEY`.

Currently supports rust, python and html/css/js. lua support is planned.

## Usage

```sh
scrooge run "fix the off-by-one in pagination"   # full plan/execute/review loop
scrooge cratchit "rename foo to bar everywhere"  # hand straight to cratchit, no planning
scrooge humbugs                                  # find every humbug: format, tests, lint
scrooge ask "where is auth handled?"             # one-shot, cratchit only
scrooge map                                      # codebase brief (free, no LLM)
scrooge sym <name>                               # signature + callers + callees
scrooge callers <fn> / scrooge callees <fn>      # call-graph queries
scrooge -r path/to/project ...                   # operate on another project
scrooge helpers                                  # generic utilities in the repo (heuristic, free)
scrooge helpers --deps --validate                # + all dependencies, filtered by Cratchit
```

## Claude Code plugin mode

In [plugin/](plugin/), scrooge doubles as a Claude Code plugin where the
Claude conversation itself plays Scrooge (on your subscription) and the
binary serves tools over MCP (`scrooge mcp-serve`): `get_brief`,
`symbol_info`/`callers`/`callees`, `helpers`, `best_practices`, plus
`give_cratchit_task` / `ask_cratchit` which run the OpenRouter Cratchit
loop and return only the terse report. Hooks inject the code-map brief at
SessionStart and keyword-matched best practices at UserPromptSubmit.
`/scrooge:task <task>` runs the full miserly workflow; set
`CRATCHIT_BACKEND=subagent` to use the bundled Haiku subagent (all on
subscription) instead of OpenRouter. Build with `plugin/build.sh`, then
install the plugin directory in Claude Code. Design notes:
[docs/plugin-architecture.md](docs/plugin-architecture.md).