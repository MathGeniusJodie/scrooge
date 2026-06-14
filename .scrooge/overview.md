The current overview claims the changes are all cosmetic/ergonomic. Let me verify what actually changed by examining the diff closely.

The diff shows:
1. **`gather_context`** — new async method: Cratchit pre-reads files and hands facts to Scrooge before planning. This is a new architectural component in the delegation pipeline.
2. **`{CONTEXT}` in `SCROOGE_BRIEF`** — Scrooge's briefing now includes Cratchit-gathered context, changing first-round information flow.
3. **`CodeMap::languages()`** — new method deriving language tags from file extensions.
4. **`practices::relevant()`** — new function matching sections by both topic *and* project languages.
5. **`practices::summary()`** — substantially rewritten: now shows multiple rules per section with keyword-matching rules pulled to front. Behavioral change.
6. **`relevant_sections()` and `summary()` signatures** — gained `langs` parameter, changing how guidance is selected everywhere.
7. **`best_practices.md`** — file-extension keywords added.

The overview calling these "cosmetic/ergonomic output additions" and saying "no new architectural components, no changed delegation model, no altered invariants" is stale.

**Scrooge** is a Rust CLI agent orchestrator where a cheap "Scrooge" model writes execution plans that a capable "Cratchit" model carries out, with deterministic tool dispatch and filesystem confinement.

Architecture: The `Orchestrator` delegates to Cratchit (OpenRouter API), Scrooge plans via `scrooge_plan`, and `Toolbox` dispatches file/shell/API calls deterministically within a git worktree. A `CodeMap` (tree-sitter) indexes the codebase for symbol navigation; an `overview.md` memoizes project context. A pre-planning `gather_context` pass lets Cratchit pre-filter file-level facts for Scrooge's first brief so it isn't blind on round one. Best-practice guidance (`practices.rs`) selects sections by task keywords *and* project languages (derived from `CodeMap::languages()`), with `summary()` pulling keyword-matching rules to the front per section. The MCP server and CLI subcommands both route through `Orchestrator`.

Non-obvious invariants: tool output is always clamp-truncated before injection; plans are checked mechanically against output (changed/checks lines); `edit_file` batches are all-or-nothing; `replace_symbol` swaps whole definitions; `confine` keeps paths inside the worktree root; the cheap model never touches files or shell directly.