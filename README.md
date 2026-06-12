# scrooge

A token-miserly coding agent. **Scrooge** (expensive SOTA model) only ever sees
compact briefs and terse reports — he plans and reviews, never reads files or
writes code. **Cratchit** (cheap model) does all the token-heavy work with
tools, and must compress everything he reports upstairs.

Both models are `deepseek/deepseek-v4-flash` during development; override with
`SCROOGE_MODEL` and `CRATCHIT_MODEL` env vars. Requires `OPENROUTER_API_KEY`.

## Usage

```sh
scrooge run "fix the off-by-one in pagination"   # full plan/execute/review loop
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

## Token-efficiency design

- The codebase brief is built deterministically with tree-sitter (Rust,
  Python, JS, and `<script>` blocks in HTML) — symbols plus a
  who-calls-whom graph. Zero LLM tokens to produce.
- Scrooge gets: task + brief + only the best-practice sections whose
  keywords match the task (`best_practices.md`, keyword-tagged headers).
- Cratchit is instructed to read line ranges not files, query the call
  graph before reading anything, never do math in-model (mandatory
  `python`/`wolframscript` tools), always `query_docs` (pydoc / docs.rs /
  MDN) before using unfamiliar APIs, and cap reports to Scrooge at 6 lines.
- **Helper inventory** (`src/helpers.rs`): heuristics (fan-in across ≥2
  distinct files, utility-shaped names, ≤100 lines, public) find generic
  utilities in the repo and in every dependency — cargo registry sources,
  python site-packages, node_modules. `--validate` has Cratchit reject
  domain-specific candidates and annotate keepers with a purpose line.
  Results cache to `.scrooge/helpers.json` and are exposed to the agents
  via the `helpers` tool, so they reuse instead of reinventing the wheel.
- Tool output is truncated at 8 KB; the loop is capped at 5 review rounds
  and 40 tool calls per round. Token usage is printed at the end of a run.

## Layout

- `src/codemap.rs` — tree-sitter symbol extraction + call graph (the core)
- `src/agents.rs` — Scrooge/Cratchit orchestration loop and prompts
- `src/tools.rs` — Cratchit's tools (files, shell, python, wolfram, docs, graph)
- `src/openrouter.rs` — minimal OpenRouter chat client with tool calling
- `src/practices.rs` + `best_practices.md` — keyword-matched guidance sections
