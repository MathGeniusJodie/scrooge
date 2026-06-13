# TODO — pipeline review findings (2026-06-12)

## Correctness

- [ ] **Change detection misses same-line re-edits** (`src/agents.rs` `execute_and_verify`, ~L525).
  Detecting "nothing changed" by comparing `git diff HEAD --stat` strings aliases when a later
  round re-edits the same already-dirty line (diffstat identical → checks skipped, false
  `CHANGED: nothing`). Fix: compare a hash of the full `git diff HEAD` output plus untracked
  file contents instead of the diffstat string.

- [ ] **Synthesized check-failure plan gets re-split by `plan_steps`** (`src/agents.rs` ~L346).
  The `"1. Fix the failures below…\n{failures}"` plan built on DONE-with-red-checks flows through
  `plan_steps`; numbered lines in the failure dump (pytest, clippy) become bogus steps each
  dispatched to a fresh Cratchit. Fix: bypass the splitter for the synthesized plan — it is one
  step by construction.

- [ ] **`checks::run` has no timeout** (`src/checks.rs` `run_cmd`).
  A hanging test deadlocks the native orchestrator (shell tool has 60s, Stop hook has 600s, but
  this path is unbounded). Fix: wrap each check command in a timeout (e.g. 600s) and report a
  timeout as an error.

- [ ] **60s shell timeout vs. Cratchit's "verify before reporting" rule** (`src/tools.rs` `run`).
  Cold `cargo test` routinely exceeds 60s, so rule 6 burns tool-budget on doomed verification.
  Fix: either raise the timeout for build/test-shaped commands, or (cheaper) change
  CRATCHIT_SYSTEM rule 6 to say the deterministic suite verifies after him — only compile-check,
  don't run the full test suite.

- [ ] **Code map blind to tests** (`src/codemap.rs` `SKIP_DIRS`).
  `tests`/`test`/`examples`/`benches` are skipped, yet check failures reference those symbols;
  `symbol_info` and `replace_symbol` can't see/edit them. Also bare `build`/`dist` names can skip
  legitimate source dirs. Fix: index test dirs but tag the symbols (exclude from `brief()` only,
  keep in the symbol table / call graph); consider only skipping `build`/`dist` when they look
  like build output.

## Efficiency

- [ ] **Duplicate check runs in plugin mode** (`plugin/hooks/stop.py` + PostToolUse matcher).
  `give_cratchit_task` already runs checks; the Stop hook re-runs them. Fix: skip the Stop-hook
  run when the last delegated report already said `CHECKS: clean` (e.g. have the binary record a
  verdict timestamp in `.scrooge/` that the hook consults against the dirty flag).

- [ ] **`helpers::filtered_listing` re-parses the repo on every tool call**
  (`src/helpers.rs` ~L326). `repo_helpers` uses `codemap::build`; switch to `build_cached`
  (or thread an `Arc<CodeMap>` through `score_map`).

- [ ] **`query_docs` rust window anchors on nav boilerplate** (`src/tools.rs` `query_docs`).
  `text.find(needle)` hits the docs.rs sidebar first. Fix: anchor on a later occurrence or on an
  item-header pattern before taking the 4000-char window.

- [ ] **`DONE` detection too loose** (`src/agents.rs` ~L332).
  `plan.trim().starts_with("DONE")` matches "DONE is wrong because…". Fix: require the first
  line to be exactly `DONE` (allow trailing punctuation).

## Notes / polish

- [ ] **Document the sandbox trust model** (README). Landlock confines writes but the child has
  unrestricted network and read access to all of `$HOME` — note that prompts must be trusted.
  Optionally explore network confinement.

- [ ] **MCP spec conformance** (`src/mcp.rs`). Return `-32601` (method not found) for unsupported
  methods instead of `-32603`; consider handling `notifications/cancelled` so long
  `give_cratchit_task` calls can be interrupted.

- [ ] **Missing `CHANGED` line on non-git roots** (`src/agents.rs` `execute_and_verify`).
  Scrooge is told to trust a line that may be absent. Fix: append `CHANGED: (not a git repo)`
  when `worktree_changes` returns None.

- [ ] **Prompts vs. clamp constants disagree** (`src/agents.rs`). Prompts promise 6-line reports;
  code clamps at 12 lines / 1200 chars. Intentional slack — either document it where the
  constants live (done partially) or align the numbers.
