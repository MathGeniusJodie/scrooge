# Scrooge as a Claude Code plugin — architecture draft

## The big idea

In plugin mode, **Claude (your subscription) becomes Scrooge**. The main
Claude Code conversation is the expensive SOTA planner; the scrooge binary
stops being an orchestrator and becomes a **token-efficiency service** that
Claude calls. Cratchit and all the deterministic machinery stay in the Rust
binary, unchanged.

```
Claude Code conversation  =  Scrooge (planner, on your subscription)
        │  MCP (stdio)
        ▼
scrooge binary (mcp-serve mode)
  ├── deterministic tools: code map, call graph, helpers, best practices
  └── give_cratchit_task: runs the existing Cratchit loop on OpenRouter,
      returns only the ≤6-line report
```

The binary keeps its standalone CLI (`scrooge run/ask/map/helpers`) — the
plugin is just a second front door. One codebase, two modes.

## Plugin layout

```
scrooge-plugin/
├── .claude-plugin/plugin.json     # name: scrooge
├── .mcp.json                      # command: ${CLAUDE_PLUGIN_ROOT}/bin/scrooge, args: [mcp-serve]
├── bin/scrooge                    # the Rust binary (or build step in install docs)
├── hooks/hooks.json               # SessionStart + UserPromptSubmit (below)
├── skills/
│   └── task/SKILL.md              # /scrooge:task — the full miserly workflow
└── agents/
    └── cratchit.md                # optional: subscription-Cratchit (model: haiku)
```

## MCP tools (new `scrooge mcp-serve` subcommand)

All thin wrappers over existing modules — stdio JSON-RPC, no new logic:

| tool | maps to | notes |
|---|---|---|
| `get_brief` | `codemap::build().brief()` | the compact codebase map |
| `symbol_info` / `callers` / `callees` | call-graph queries | |
| `helpers` | validated helper cache | "check before writing a helper" |
| `best_practices` | keyword-matched sections | |
| `give_cratchit_task` | `cratchit_execute()` | args: task, instructions; returns terse report + token cost |
| `ask_cratchit` | `Orchestrator::ask()` | cheap one-shot investigation |

Claude never reads files through scrooge — if it wants detail, it sends
Cratchit to fetch and compress it. That's the whole token thesis, enforced
by tool shape rather than prompting.

## Hooks (the part plugins do that the CLI can't)

- **SessionStart** → run `scrooge map`, return it as
  `hookSpecificOutput.additionalContext`. Claude starts every session
  already knowing the codebase shape, for the price of the brief.
- **UserPromptSubmit** → run the prompt through the best-practices keyword
  matcher (`scrooge practices "<prompt>"`, tiny new subcommand) and inject
  only matching sections. Same trick as the CLI, now automatic.
- **PreToolUse on Read/Grep** (optional, aggressive): deny with reason
  "use scrooge symbol_info / give_cratchit_task instead" when Claude tries
  to read a large file directly. Start without this; add it if Claude
  ignores the cheap path. A softer variant: allow but append a reminder
  via PostToolUse `updatedToolOutput`.

## Skill: `/scrooge:task <task>`

A SKILL.md that encodes the workflow so Claude behaves like Scrooge:
"You are the planner; your context is expensive. Call `get_brief` and
`helpers` first, plan in numbered steps, dispatch each step via
`give_cratchit_task`, judge reports, never read files yourself unless a
report is ambiguous." Skills can't *force* tool choice, but combined with
the SessionStart brief and (optionally) the PreToolUse hook, the cheap
path is also the easy path.

## Where does Cratchit's model come from? Two options

1. **OpenRouter (current code, default)** — `give_cratchit_task` uses
   `CRATCHIT_MODEL` exactly as today. Pro: any cheap model, costs are
   metered separately from your subscription. Con: needs the API key.
2. **Subscription Cratchit** — ship `agents/cratchit.md` with
   `model: haiku` and the Cratchit system prompt; the skill dispatches to
   the subagent instead of the MCP tool. Pro: zero API keys, everything on
   the subscription. Con: burns subscription quota, and the subagent uses
   Claude Code's generic tools instead of scrooge's tuned toolbox (mitigable:
   restrict its `tools:` to the scrooge MCP tools + Edit/Bash).

Shipping both and letting the skill pick via an env var is cheap to do.

## What changes in the Rust code

1. New `mcp-serve` subcommand: stdio MCP server (initialize, tools/list,
   tools/call — small enough to hand-roll, or use the `rmcp` crate).
2. Extract `cratchit_execute` so it runs without a Scrooge planning loop
   (it already nearly does — `ask()` is this).
3. Tiny `practices <text>` subcommand for the UserPromptSubmit hook.
4. Keep CLI mode untouched.
```
