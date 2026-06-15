//! Orchestration. Scrooge (expensive SOTA model) only ever sees compact
//! briefs and terse reports — he plans, reviews, and decides. Cratchit
//! (cheap model) does all the token-heavy tool work and must compress
//! everything he sends upstairs.
//!
//! Everything that can be decided deterministically is: the code map and
//! relevant guidance are injected rather than fetched by the model; every
//! code-changing execution ends with a machine-generated CHECKS line appended
//! to the report (Scrooge never sees a diff, only the verdict); mechanical
//! check failures loop straight back to Cratchit in the same live conversation
//! without burning a Scrooge round; and DONE is only accepted while checks are
//! green.

use anyhow::Result;
use serde_json::Value;
use std::fmt::Write;
use std::path::{Path, PathBuf};

use crate::accounting;
use crate::checks;
use crate::codemap;
use crate::helpers::Helper;
use crate::openrouter::{Chat, Client, DEV_MODEL_CHEAP, DEV_MODEL_SOTA, Message, ToolCall};
use crate::practices;
use crate::tools::{self, Toolbox};

const SCROOGE_SYSTEM: &str = "\
You are Scrooge, a senior software architect. Your time is extremely valuable, \
so you receive only compressed briefs and you produce only terse, high-leverage output. \
You never read full files and never write code. Your tools:\n\
- delegate_to_cratchit: dispatch ONE step to Cratchit, a junior agent with full tool \
  access (files, shell, python, wolfram, docs, call graph). Instructions must be standalone \
  and imperative, naming exact files/symbols where known. You see only this brief, never \
  file contents: when you need file-level detail before you can plan a change, spend one \
  delegate_to_cratchit purely to investigate — tell Cratchit to read the relevant files and \
  report the facts, changing nothing; a read-only step returns just his findings (no CHECKS).\n\
- symbol_info / callers / callees: call-graph lookups (a symbol's \
  signature, who calls it, what it calls).\n\
- web_answer: a concise AI answer from the web. Use SPARINGLY — only when a \
  library/dependency choice or a specific API detail would materially change your next \
  step and you are not sure of it. Not for code in this repo. Most tasks need zero calls.\n\
A delegate_to_cratchit step that changes code ends with a machine-generated CHECKS line (a \
fast per-step compile verdict; the full test+lint suite runs when you finish) — trust it over \
Cratchit's claims. When the task is complete and CHECKS is clean, reply with the single word \
DONE and no tool calls. No preamble, no prose.";

const CRATCHIT_SYSTEM: &str = "\
You are Cratchit, a diligent coding agent executing a plan written by Scrooge, \
your demanding boss whose time is very valuable. A code map of the relevant files \
and the applicable best-practice guidance are already included in your briefing. \
Rules:\n\
1. Use tools for everything. NEVER do arithmetic or logic in your head when the \
python or wolfram tool can do it deterministically.\n\
2. Use the included code map; call symbol_info / callers before changing any \
signature; read line ranges, not whole files. Rewrite whole functions with \
replace_symbol; batch related find/replace pairs into ONE edit_file call.\n\
3. Call query_docs before using any API you are not 100% sure about.\n\
4. Check the helpers tool before writing a new utility function.\n\
5. When a task needs an external library and none was named, call \
search_libraries to find the current best option — never pick one from \
memory. Then add it with add_dependency, which installs the LATEST published \
version. Never write a version number into a manifest from memory — your \
training data is stale.\n\
6. Verify your work (compile, run, test) before reporting; a deterministic \
check suite also runs after you finish.\n\
7. Your final message is a report for Scrooge: maximum 6 lines, only facts he \
needs (what changed, file:line, verification result, blockers). No pleasantries, \
no restating the plan, no code unless a decision depends on it.";

// ── LLM prompt templates ────────────────────────────────────────────────────
// Every string sent to a model lives here as a constant. Templated prompts use
// {UPPERCASE} placeholders filled in with `str::replace` at the call site, so
// the wording stays in one place and the methods only assemble values.

/// One-time briefing for Scrooge — built once, never rebuilt, so the
/// provider's KV cache keeps it cheap on every subsequent turn.
/// Placeholders: {TASK} {OVERVIEW} {BRIEF} {GUIDANCE}.
const SCROOGE_BRIEF: &str = "\
TASK: {TASK}\n\nPROJECT OVERVIEW:\n{OVERVIEW}\n\n\
CODEBASE BRIEF:\n{BRIEF}\nKEY GUIDANCE:\n{GUIDANCE}";

/// Briefing for a fresh Cratchit. Stable-first so the provider's KV cache can
/// reuse the prefix across every delegation in a run: ROOT/TASK/OVERVIEW/MAP/
/// GUIDANCE are identical step to step (MAP and GUIDANCE are sliced on the task
/// alone, not the per-step plan), and only the volatile tail — the step's
/// instructions and the previous report — changes.
/// Placeholders: {ROOT} {TASK} {OVERVIEW} {MAP} {GUIDANCE} {PLAN} {PREV}.
/// {OVERVIEW}/{GUIDANCE}/{PREV} are self-contained blocks (headers included) so
/// they vanish cleanly when withheld.
const CRATCHIT_BRIEF: &str = "\
PROJECT ROOT: {ROOT}\nTASK: {TASK}\n{OVERVIEW}\
CODE MAP (files mentioned in the task shown in full):\n{MAP}\n{GUIDANCE}\
SCROOGE'S INSTRUCTIONS:\n{PLAN}\n{PREV}";

/// Injected as a user message when Scrooge stops calling tools while CHECKS
/// is still FAILING, so it is nudged to delegate a fix rather than finish.
const SCROOGE_DONE_WITH_FAILURES: &str = "\
CHECKS are still FAILING — do not stop. Call delegate_to_cratchit to fix the failures.";

/// Injected when Scrooge tries to finish but the full check suite — tests +
/// lint, run only at completion — turned up failures the per-step quick check
/// didn't. Placeholder: {FAILURES}.
const FINISH_BLOCKED: &str = "\
Not done yet: the full check suite (tests + lint) failed. Delegate a fix to \
Cratchit, then finish.\n{FAILURES}";

/// Sent back to Cratchit inside the verify loop when checks fail.
/// Placeholder: {FAILURES}.
const CHECK_FAILURE_INSTRUCTIONS: &str = "\
The deterministic check suite failed after your last changes. Fix the \
failures below, then verify.\n{FAILURES}";

/// Instruction to write the overview for a brand-new project (no code yet).
/// Placeholder: {TASK}.
const OVERVIEW_FRESH_INSTRUCTIONS: &str = "\
This is a brand-new project being kicked off with this request:\n\
{TASK}\n\n\
Write the project overview that future planning will rely on. \
First line: what the project is, in one sentence. Then one short \
prose paragraph describing the intended architecture. Do not \
modify any files. Your final message must be ONLY the overview \
text, at most 15 lines — it is saved verbatim as overview.md.";

/// Instruction to write the overview for an existing, undocumented codebase.
const OVERVIEW_EXISTING_INSTRUCTIONS: &str = "\
This codebase was built without an overview on file. Investigate it \
(README, manifests, entry points, key modules — read as much as you \
need) and write the project overview. First line: what the project \
is, in one sentence. Then one short prose paragraph describing the \
architecture and the design decisions/invariants that are NOT \
obvious from a symbol listing (data flow, why the pieces are split \
this way, what must stay true). Do not modify any files. Your final \
message must be ONLY the overview text, at most 15 lines — it is \
saved verbatim as overview.md.";

/// Instruction to rewrite the overview after a completed task. The rewrite is
/// mandatory — Cratchit always returns a fresh overview, which the orchestrator
/// saves verbatim (no "UNCHANGED" verdict to second-guess). This pass only runs
/// when the task changed the code's structure, so a rewrite is always warranted.
/// Placeholder: {CHANGED}.
const OVERVIEW_REFRESH_INSTRUCTIONS: &str = "\
The task above is complete. The PROJECT OVERVIEW in your briefing may \
now be stale given what changed:\n{CHANGED}\n\n\
Rewrite the project overview to match the current state of the code. \
Do NOT modify any files. Your final message must be ONLY the overview \
text — first line: what the project is in one sentence, then one short \
prose paragraph on architecture and non-obvious invariants, at most 15 \
lines. Output nothing else.";

/// Framing for the one-shot `ask` entry point.
const ASK_INSTRUCTIONS: &str = "Answer the question directly; investigate with tools first.";

/// Tools withheld while generating or refreshing the overview: Cratchit must
/// return the overview as its final message (the orchestrator saves it), never
/// write the file itself. `shell` is intentionally kept for investigation.
const OVERVIEW_WITHHELD_TOOLS: &[&str] = &["write_file", "edit_file", "replace_symbol"];

/// System prompt for the helper-validation pass.
const HELPER_VALIDATION_SYSTEM: &str = "\
You are Cratchit, validating utility-function candidates found by \
static heuristics. Keep only genuinely GENERIC, reusable helpers — \
things any module might want (string/path/collection/number/format \
utilities). Reject anything domain-specific, stateful, or trivial \
wrappers. If a signature is ambiguous, read the function body with \
read_file (use the given file path and line range) before deciding. \
Output one line per keeper, exactly: KEEP <name> | <purpose, max 8 words> \
Nothing else. No explanations.";

/// Forced-verdict nudge when the helper-validation tool budget runs out.
const HELPER_VALIDATION_STOP: &str = "\
STOP: tool budget exhausted. Output your KEEP lines now, \
nothing else. No tool calls.";

/// Forced final-report nudge when Cratchit's main tool budget runs out.
const CRATCHIT_STOP: &str = "\
STOP: tool budget exhausted. Send your final report for Scrooge now \
— at most 6 lines: state reached, what remains, blockers. No tool calls.";

/// Warning injected when Cratchit is this many iterations from the limit.
const TOOL_BUDGET_WARNING_REMAINING: usize = 5;

/// The warning message itself. Number must match the constant above.
const CRATCHIT_BUDGET_WARNING: &str = "\
WARNING: 5 tool calls remain in your budget. Finish your current action \
and write your final report now — do not start new investigations.";

/// How many `web_answer` calls Scrooge may make across the whole task before
/// the tool is withheld. Kept low to enforce "sparing" use in code.
const SCROOGE_WEB_LOOKUPS: usize = 3;

/// How many times a failing check report is routed straight back to Cratchit
/// before the failure is escalated to Scrooge.
const CHECK_RETRIES: usize = 2;

/// How many times Scrooge may try to finish while checks are still red before
/// the loop gives up rather than spinning.
const MAX_FINISH_ATTEMPTS: usize = 4;

/// Per-completion output cap for Scrooge, applied to every planning turn (not a
/// task-wide budget). Sized to bound a runaway turn while leaving ample room for
/// a terse delegate instruction. In the rare case a turn hits the cap, the
/// truncated tool-call JSON is repaired (`parse_tool_args`) and the clipped
/// instruction is run as-is — we never spend a second turn re-asking.
const SCROOGE_MAX_TOKENS: u32 = 1024;

/// Output cap for a forced final report (the tool budget ran out). Applied only
/// to a no-tools completion, so it can never truncate a tool call; a ≤6-line
/// report fits comfortably.
const REPORT_MAX_TOKENS: u32 = 512;

/// Tool-call budget for one stretch of a Cratchit conversation (the initial
/// work, or a single check-failure retry that continues the same conversation).
const CRATCHIT_MAX_ITERS: usize = 40;

/// Which check suite to run after a delegation. The agent loop runs `Quick`
/// (a fast compile/typecheck) between steps and `Full` (tests + lint) only to
/// gate completion; one-shot `scrooge cratchit` runs `Full` directly.
#[derive(Clone, Copy)]
enum CheckTier {
    Quick,
    Full,
}

/// Hard caps enforcing rule 7 (the ≤6-line report) in code: Cratchit's final
/// message is clamped before anyone expensive reads it.
const MAX_REPORT_LINES: usize = 12;
const MAX_REPORT_CHARS: usize = 1200;

/// How much of a check-failure dump Scrooge sees. Cratchit already got the
/// full output during the retry loop; Scrooge only needs the gist — kept
/// minimal so a clean step costs one line and a red one a short tail.
const MAX_FAIL_CHARS: usize = 400;

/// Clamp a Cratchit report to the size rule 7 promises.
fn clamp_report(s: &str) -> String {
    let mut out = s
        .lines()
        .take(MAX_REPORT_LINES)
        .collect::<Vec<_>>()
        .join("\n");
    if out.len() > MAX_REPORT_CHARS {
        out.truncate(crate::util::floor_char_boundary(&out, MAX_REPORT_CHARS));
        out.push_str("\n[report clamped]");
    } else if s.lines().count() > MAX_REPORT_LINES {
        out.push_str("\n[report clamped]");
    }
    out
}

/// Short stderr preview of tool-call arguments — a full `write_file` body
/// would make the log unreadable.
fn arg_preview(args: &str) -> String {
    const MAX: usize = 200;
    if args.len() <= MAX {
        return args.to_string();
    }
    format!(
        "{}… [{} chars]",
        &args[..crate::util::floor_char_boundary(args, MAX)],
        args.len()
    )
}

/// Verify the project root is a git work tree, bailing with actionable guidance
/// if not. The agent loop uses `worktree_changes` to tell whether a step
/// changed code (and therefore whether checks must run / a CHECKS verdict is
/// owed); without git that detection silently degrades, so the code-changing
/// entry points require it up front rather than misbehaving later.
fn require_git_repo(root: &Path) -> Result<()> {
    let inside = std::process::Command::new("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .current_dir(root)
        .output()
        .is_ok_and(|o| o.status.success());
    if !inside {
        anyhow::bail!(
            "{} is not a git repository (or git is unavailable). Scrooge needs git to \
             detect what each step changed — run `git init` here first.",
            root.display()
        );
    }
    Ok(())
}

/// Worktree state as git sees it: diffstat of tracked changes plus untracked
/// files. None when the root is not a git repo (or git is unavailable);
/// the code-changing entry points guard against that with `require_git_repo`.
fn worktree_changes(root: &Path) -> Option<String> {
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    };
    // Diff against HEAD so staged changes count too (Cratchit sometimes runs
    // `git add`); fall back to the index diff on a repo with no commits yet.
    let diff = git(&["diff", "HEAD", "--stat"]).or_else(|| git(&["diff", "--stat"]))?;
    let untracked = git(&["status", "--short", "--untracked-files"])
        .unwrap_or_default()
        .lines()
        .filter(|l| l.starts_with("??"))
        .collect::<Vec<_>>()
        .join("\n");
    Some(format!("{diff}\n{untracked}").trim().to_string())
}

/// A content fingerprint of the worktree: the git tree OID of every tracked and
/// untracked (non-ignored) file's current bytes, captured through a throwaway
/// index so the real index is untouched. Unlike a `--stat` summary it changes
/// whenever the edited bytes change — two edits that net the same insertion/
/// deletion counts still produce different fingerprints, and a brand-new file
/// that is then modified is caught too — so a real change is never mistaken for
/// a read-only step. None when the root is not a git repo (or git is missing).
fn worktree_fingerprint(root: &Path) -> Option<String> {
    use std::sync::atomic::{AtomicU64, Ordering};
    // Unique per call so concurrent orchestrators (e.g. parallel tests) never
    // share a temp index and corrupt each other's snapshot.
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let index = std::env::temp_dir().join(format!(
        "scrooge-index-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_file(&index);
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .current_dir(root)
            .env("GIT_INDEX_FILE", &index)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    };
    // Stage the whole worktree into the scratch index, then hash it to a tree.
    git(&["add", "-A"])?;
    let tree = git(&["write-tree"]);
    let _ = std::fs::remove_file(&index);
    tree
}

/// Parse tool-call arguments. A well-formed object always parses. When the
/// completion was cut short by an output cap (`finish_reason: length`, passed as
/// `truncated`), the arguments can be clipped mid-value; rather than spend
/// another turn we close whatever was left open and run with the surviving
/// (truncated) instruction. When the response was NOT truncated, a parse failure
/// is genuine malformed JSON, not a clip — we do not guess, returning `Null` so
/// the caller can reject the call instead of acting on a blind repair.
fn parse_tool_args(raw: &str, truncated: bool) -> Value {
    if let Ok(v) = serde_json::from_str(raw) {
        return v;
    }
    if truncated && let Ok(v) = serde_json::from_str(&close_truncated_json(raw)) {
        return v;
    }
    Value::Null
}

/// Best-effort close of a JSON fragment cut off at the end: balance an open
/// string, drop a now-dangling separator, then close every still-open `{`/`[`
/// in reverse. Good enough for the dominant case — a long string value clipped
/// mid-content — which is exactly where an output cap lands.
fn close_truncated_json(raw: &str) -> String {
    let mut stack: Vec<char> = Vec::new();
    let mut in_str = false;
    let mut escaped = false;
    for c in raw.chars() {
        if in_str {
            match (escaped, c) {
                (true, _) => escaped = false,
                (false, '\\') => escaped = true,
                (false, '"') => in_str = false,
                _ => {}
            }
            continue;
        }
        match c {
            '"' => in_str = true,
            '{' => stack.push('}'),
            '[' => stack.push(']'),
            '}' | ']' => {
                stack.pop();
            }
            _ => {}
        }
    }
    let mut out = raw.to_string();
    if escaped {
        out.pop(); // a trailing backslash would escape our closing quote
    }
    if in_str {
        out.push('"');
    } else {
        // Cut between elements: drop a dangling comma so the close is valid.
        while out.ends_with(char::is_whitespace) {
            out.pop();
        }
        if out.ends_with(',') {
            out.pop();
        }
    }
    while let Some(close) = stack.pop() {
        out.push(close);
    }
    out
}

/// Mutable per-task bookkeeping for `run_task`, threaded through the planning
/// loop's helpers. Bundled into one struct rather than passed as a fistful of
/// `&mut` scalars (which previously pushed those helpers past 8–10 arguments).
#[derive(Default)]
struct RunState {
    /// Scrooge planning turns taken — the loop's only budget. Every Scrooge
    /// completion counts (a delegation *and* a free call-graph lookup alike),
    /// so a model that only ever issues free lookups can no longer spin the
    /// loop forever without advancing.
    turns: usize,
    /// `web_answer` lookups Scrooge has spent against `SCROOGE_WEB_LOOKUPS`.
    web_calls: usize,
    /// Cratchit's most recent report, fed into the next briefing.
    last_report: Option<String>,
    /// Latest quick-check verdict: `None` until the first code-changing step.
    checks_clean: Option<bool>,
    /// Whether any step has changed code (so checks must gate completion).
    dirty: bool,
    /// How many times Scrooge has tried to finish while checks were red.
    finish_attempts: usize,
}

pub struct Orchestrator<C = Client> {
    client: C,
    toolbox: Toolbox,
    cheap_model: String,
    sota_model: String,
    max_turns: usize,
}

impl Orchestrator<Client> {
    pub fn new(root: PathBuf) -> Result<Self> {
        Ok(Self {
            client: Client::new(root.clone())?,
            toolbox: Toolbox::new(root),
            cheap_model: std::env::var("CRATCHIT_MODEL").unwrap_or_else(|_| DEV_MODEL_CHEAP.into()),
            sota_model: std::env::var("SCROOGE_MODEL").unwrap_or_else(|_| DEV_MODEL_SOTA.into()),
            max_turns: 20,
        })
    }
}

impl<C: Chat + Send + Sync> Orchestrator<C> {
    /// Full task loop: Scrooge delegates to Cratchit via the
    /// `delegate_to_cratchit` tool, sees each report as a tool result, and
    /// adapts step by step. The initial briefing is built once and never
    /// rebuilt — the provider's KV cache keeps it cheap on every subsequent
    /// Scrooge turn, which also eliminates the stale-context re-injection
    /// problem of the old round-based loop.
    pub async fn run_task(&mut self, task: &str) -> Result<String> {
        require_git_repo(&self.toolbox.root)?;
        let overview = self.ensure_overview(task).await?;
        let map = codemap::build_cached(&self.toolbox.root)?;
        // Snapshot the structure now so completion can tell whether the task
        // touched the architecture and the overview needs re-review (#4).
        let structure_before = map.structure_signature();
        let guidance = practices::summary(task, &map.languages());

        // Built once; becomes the cached prefix for all subsequent turns.
        let mut log = vec![
            Message::text("system", SCROOGE_SYSTEM),
            Message::text(
                "user",
                SCROOGE_BRIEF
                    .replace("{TASK}", task)
                    .replace("{OVERVIEW}", &overview)
                    .replace("{BRIEF}", &map.brief())
                    .replace("{GUIDANCE}", &guidance),
            ),
        ];

        // Stable tool array, built once: the web-lookup budget is enforced in
        // the handler below rather than by swapping the tool list, which would
        // invalidate the provider's cached prefix on every transition.
        let defs = tools::scrooge_definitions();

        let mut st = RunState::default();

        loop {
            if st.turns >= self.max_turns {
                break;
            }
            st.turns += 1;
            let msg = self
                .client
                .chat(
                    "scrooge",
                    &self.sota_model,
                    &log,
                    &defs,
                    Some(SCROOGE_MAX_TOKENS),
                )
                .await?;
            log.push(msg.clone());

            let Some(calls) = msg.tool_calls.filter(|c| !c.is_empty()) else {
                // Scrooge stopped calling tools — he wants to finish.
                if let Some(out) = self
                    .try_finish(task, &mut log, &mut st, &structure_before)
                    .await?
                {
                    return Ok(out);
                }
                continue; // checks red — try_finish nudged Scrooge to fix them
            };

            // A turn cut off by the output cap may carry a clipped tool call;
            // only then do we let parse_tool_args attempt a repair.
            let truncated = self.client.last_finish_reason() == Some("length");
            // At most one delegation per turn: Cratchit's report (and its CHECKS
            // verdict) must come back before the next step is planned, so a
            // second delegate call in the same turn was planned blind.
            let mut delegated = false;
            for call in calls {
                let result = self
                    .run_scrooge_call(task, &call, truncated, &mut delegated, &mut st)
                    .await?;
                log.push(Message::tool_result(&call.id, result));
            }
        }
        Ok("turn limit reached without DONE; review output above".into())
    }

    /// Run one Scrooge delegation: execute + verify, fold the verdict into the
    /// run's running state, and return the report Scrooge will read.
    async fn run_delegation(
        &mut self,
        task: &str,
        instructions: &str,
        st: &mut RunState,
    ) -> Result<String> {
        eprintln!(
            "--- scrooge → cratchit (turn {}) ---\n{instructions}\n",
            st.turns
        );
        let (report, verdict) = self
            .execute_and_verify(
                task,
                instructions,
                st.last_report.as_deref(),
                CheckTier::Quick,
            )
            .await?;
        if let Some(c) = verdict {
            st.checks_clean = Some(c);
            st.dirty = true;
        }
        eprintln!("--- cratchit report ---\n{report}\n");
        st.last_report = Some(report.clone());
        Ok(report)
    }

    /// Handle one tool call Scrooge emitted, returning the string fed back as
    /// its tool result. `delegated` carries the one-delegation-per-turn state
    /// across the turn's calls; the free call-graph lookups and the budgeted
    /// `web_answer` are answered locally without a Cratchit round.
    async fn run_scrooge_call(
        &mut self,
        task: &str,
        call: &ToolCall,
        truncated: bool,
        delegated: &mut bool,
        st: &mut RunState,
    ) -> Result<String> {
        let args: Value = parse_tool_args(&call.function.arguments, truncated);
        let name = call.function.name.clone();
        if name == "delegate_to_cratchit" {
            let instructions = args["instructions"]
                .as_str()
                .unwrap_or("")
                .trim()
                .to_string();
            if instructions.is_empty() {
                // Empty or unrepairable args: bounce it back rather than burn a
                // delegation briefing Cratchit with nothing.
                return Ok(
                    "error: delegate_to_cratchit needs a non-empty `instructions` string; \
                           the call arrived empty or malformed — re-issue the step."
                        .to_string(),
                );
            }
            if *delegated {
                return Ok(
                    "error: only one delegate_to_cratchit per turn — wait for this step's \
                           report before issuing the next."
                        .to_string(),
                );
            }
            *delegated = true;
            return self.run_delegation(task, &instructions, st).await;
        }
        if name == "web_answer" {
            if st.web_calls >= SCROOGE_WEB_LOOKUPS {
                return Ok(
                    "web_answer budget exhausted — decide with what you have and delegate."
                        .to_string(),
                );
            }
            st.web_calls += 1;
        }
        // web_answer (within budget) and the free, deterministic call-graph
        // lookups (symbol_info / callers / callees) are answered locally.
        eprintln!(
            "  [scrooge] {name}({})",
            arg_preview(&call.function.arguments)
        );
        Ok(self.toolbox.call(&name, &args).await)
    }

    /// Handle Scrooge ending his turn without a tool call: decide whether the
    /// task is really done. `Ok(Some(s))` is the final result to return from the
    /// loop; `Ok(None)` means checks are red and Scrooge was nudged to fix them,
    /// so the loop should continue. The full test+lint suite runs here, once, in
    /// place of the per-step quick checks — the expensive pass is paid only at
    /// completion, not after every delegation.
    async fn try_finish(
        &mut self,
        task: &str,
        log: &mut Vec<Message>,
        st: &mut RunState,
        structure_before: &[String],
    ) -> Result<Option<String>> {
        if !st.dirty {
            return Ok(Some(Self::bill(st.turns))); // nothing changed to verify
        }
        if st.checks_clean == Some(false) {
            // Quick checks from the last step are still red.
            if st.finish_attempts < MAX_FINISH_ATTEMPTS {
                st.finish_attempts += 1;
                log.push(Message::text("user", SCROOGE_DONE_WITH_FAILURES));
                return Ok(None);
            }
            return Ok(Some("checks still failing; review the output above".into()));
        }
        // Quick checks are green; run the full suite once before accepting DONE.
        let full = self.run_checks(CheckTier::Full).await?;
        if !full.errors.is_empty() || !full.warnings.is_empty() {
            if st.finish_attempts < MAX_FINISH_ATTEMPTS {
                st.finish_attempts += 1;
                st.checks_clean = Some(false);
                let failures = crate::util::tail(&checks::render(&full), MAX_FAIL_CHARS);
                log.push(Message::text(
                    "user",
                    FINISH_BLOCKED.replace("{FAILURES}", &failures),
                ));
                return Ok(None);
            }
            return Ok(Some(
                "full checks still failing after several attempts; review output".into(),
            ));
        }
        // Only reconsider the overview when the change was structural — a pure
        // internal edit (no added/removed/renamed symbols or changed signatures)
        // can't make the architecture description stale, so skip the extra
        // Cratchit pass (#4). This reads the tree *after* the full suite's
        // autofix (clippy/ruff --fix) has run, i.e. its final formatted shape,
        // which is what we want to compare against the task's starting structure.
        let structure_after = codemap::build_cached(&self.toolbox.root)?.structure_signature();
        if structure_after == *structure_before {
            eprintln!("--- overview review skipped: no structural change ---");
        } else {
            self.refresh_overview(task).await;
        }
        Ok(Some(Self::bill(st.turns)))
    }

    /// Load .scrooge/overview.md, having Cratchit write it first if missing.
    /// On a fresh project the kickoff task itself is the source; on an
    /// existing codebase Cratchit explores with his tools — reading enough
    /// files to characterize a codebase is exactly the token-heavy legwork
    /// that must never land on Scrooge. The file stays user-editable and is
    /// injected verbatim into every briefing from then on.
    pub async fn ensure_overview(&mut self, task: &str) -> Result<String> {
        let root = self.toolbox.root.clone();
        if let Some(text) = crate::overview::load(&root) {
            return Ok(text);
        }
        let fresh_project = codemap::build_cached(&root)?.symbols.is_empty();
        let instructions = if fresh_project {
            OVERVIEW_FRESH_INSTRUCTIONS.replace("{TASK}", task)
        } else {
            OVERVIEW_EXISTING_INSTRUCTIONS.to_string()
        };
        eprintln!("--- no overview on file; cratchit is writing one ---");
        let text = self.cratchit_overview(task, &instructions).await?;
        if text.is_empty() {
            anyhow::bail!("cratchit produced an empty overview");
        }
        crate::overview::save(&root, &text)?;
        eprintln!(
            "wrote {} (edit freely; it is sent with every briefing)",
            crate::overview::path(&root).display()
        );
        Ok(text)
    }

    /// Run Cratchit on an overview instruction and return its trimmed final
    /// message. Shared by `ensure_overview` and `refresh_overview`: both
    /// withhold the file-writing tools (see `overview_tools`) and let the
    /// orchestrator, not Cratchit, persist the result.
    async fn cratchit_overview(&mut self, task: &str, instructions: &str) -> Result<String> {
        // No GUIDANCE: writing prose about purpose/architecture is not coding
        // work, so the best-practice sections are noise here.
        let text = self
            .cratchit_execute_with(task, instructions, None, Self::overview_tools(), false)
            .await?;
        Ok(text.trim().to_string())
    }

    /// Tool set for the overview passes: everything except the file-writing
    /// tools, so Cratchit returns the overview text instead of editing the
    /// file. `shell` stays available for investigation.
    fn overview_tools() -> Vec<Value> {
        tools::definitions()
            .into_iter()
            .filter(|d| {
                !OVERVIEW_WITHHELD_TOOLS.contains(&d["function"]["name"].as_str().unwrap_or(""))
            })
            .collect()
    }

    /// After a fulfilled task that wrote code, have Cratchit reconsider the
    /// overview against the diff. Like `ensure_overview`, Cratchit does NOT
    /// touch the file: he returns the rewritten overview (or "UNCHANGED") and
    /// the orchestrator saves it. Best-effort — a failure here must never
    /// tarnish a task that already completed.
    pub async fn refresh_overview(&mut self, task: &str) {
        let root = self.toolbox.root.clone();
        if crate::overview::load(&root).is_none() {
            return; // nothing on file to go stale
        }
        let changed = worktree_changes(&root)
            .filter(|c| !c.is_empty())
            .unwrap_or_else(|| "(no diff available)".into());
        // The current overview is already injected into the briefing by
        // cratchit_execute, so the instructions only need the diff.
        let instructions = OVERVIEW_REFRESH_INSTRUCTIONS.replace("{CHANGED}", &changed);
        match self.cratchit_overview(task, &instructions).await {
            Ok(text) => {
                if text.is_empty() {
                    // A rewrite is mandatory, but an empty reply has nothing to
                    // save — keep the existing overview rather than blanking it.
                    eprintln!("--- overview review: empty reply, keeping existing ---");
                } else if let Err(e) = crate::overview::save(&root, &text) {
                    eprintln!("[overview save failed (task still complete): {e:#}]");
                } else {
                    eprintln!("--- overview updated ---\n{text}");
                }
            }
            Err(e) => eprintln!("[overview review failed (task still complete): {e:#}]"),
        }
    }

    /// Completion banner. The token/cost accounting lives in `wages_footer`.
    fn bill(turns: usize) -> String {
        format!("task complete in {turns} turn(s).")
    }

    /// Two-line footer for `scrooge run`/`scrooge cratchit`: what this request
    /// actually cost (Cratchit's wages) and the shillings saved versus running
    /// it all on the pricey Scrooge model. The usage is per-request because the
    /// orchestrator is built fresh for each command invocation.
    pub fn wages_footer(&self) -> String {
        let u = self.client.usage();
        let saved = accounting::shillings_saved(
            &self.toolbox.root,
            u.prompt_tokens,
            u.completion_tokens,
            u.cost_usd,
        );
        format!(
            "\nCratchit's Wages: ${:.4}\nShillings saved: ${saved:.4}",
            u.cost_usd
        )
    }

    /// Execute instructions via Cratchit, then verify deterministically. One
    /// live Cratchit conversation spans the whole dispatch: the initial work and
    /// every check-failure retry continue the *same* log, so Cratchit keeps the
    /// edits he just made in context (cheaper and more accurate than re-briefing
    /// him from scratch with only the prior report text). Scrooge gets back just
    /// Cratchit's report plus a minimal CHECKS line — never a diff. The verdict
    /// is None when nothing changed on disk (checks skipped).
    async fn execute_and_verify(
        &mut self,
        task: &str,
        instructions: &str,
        prev_report: Option<&str>,
        tier: CheckTier,
    ) -> Result<(String, Option<bool>)> {
        let before = worktree_fingerprint(&self.toolbox.root);
        let defs = tools::definitions();
        let mut log = self.cratchit_brief(task, instructions, prev_report, true)?;
        let mut report = clamp_report(&self.run_cratchit(&mut log, &defs).await?);

        let after = worktree_fingerprint(&self.toolbox.root);
        if let (Some(b), Some(a)) = (&before, &after)
            && b == a
        {
            // Investigation/context-only dispatch: nothing changed, nothing to
            // verify — Scrooge gets just Cratchit's findings, no CHECKS line.
            return Ok((report, None));
        }

        let mut clean = false;
        let mut failures = String::new();
        for attempt in 0..=CHECK_RETRIES {
            let check = self.run_checks(tier).await?;
            if check.errors.is_empty() && check.warnings.is_empty() {
                clean = true;
                break;
            }
            let rendered = checks::render(&check);
            if attempt == CHECK_RETRIES {
                failures = rendered;
                break;
            }
            eprintln!(
                "--- checks failed (cratchit retry {}) ---\n{rendered}",
                attempt + 1
            );
            // Continue the same conversation rather than re-briefing: Cratchit
            // still has his own edits and tool output in context.
            log.push(Message::text(
                "user",
                CHECK_FAILURE_INSTRUCTIONS.replace("{FAILURES}", &rendered),
            ));
            report = clamp_report(&self.run_cratchit(&mut log, &defs).await?);
        }

        // No diff for Scrooge — just the verdict, minimal on success.
        if clean {
            report.push_str("\nCHECKS: clean");
        } else {
            write!(
                report,
                "\nCHECKS: FAILING\n{}",
                crate::util::tail(&failures, MAX_FAIL_CHARS)
            )
            .unwrap();
        }
        Ok((report, Some(clean)))
    }

    async fn run_checks(&self, tier: CheckTier) -> Result<checks::Report> {
        let root = self.toolbox.root.clone();
        tokio::task::spawn_blocking(move || match tier {
            CheckTier::Quick => checks::run_quick(&root),
            CheckTier::Full => checks::run(&root),
        })
        .await?
    }

    /// Dispatch a pre-planned task to Cratchit (used by MCP mode, where the
    /// Claude Code conversation plays Scrooge). The report carries the same
    /// machine-generated CHECKS line as the native loop, plus the
    /// token bill for this call only (usage accumulates across the server's
    /// lifetime).
    pub async fn delegate(&mut self, task: &str, instructions: &str) -> Result<String> {
        require_git_repo(&self.toolbox.root)?;
        // Same rule as the native loop: an overview exists before any work
        // is planned against the codebase.
        self.ensure_overview(task).await?;
        let before = (
            self.client.usage().prompt_tokens,
            self.client.usage().completion_tokens,
            self.client.usage().cost_usd,
        );
        let (report, _) = self
            .execute_and_verify(task, instructions, None, CheckTier::Full)
            .await?;
        let u = self.client.usage();
        Ok(format!(
            "{report}\n[cratchit tokens: {} in / {} out (${:.4})]",
            u.prompt_tokens - before.0,
            u.completion_tokens - before.1,
            u.cost_usd - before.2
        ))
    }

    /// One-shot question for the cheap model with full tool access.
    pub async fn ask(&mut self, question: &str) -> Result<String> {
        self.cratchit_execute(question, ASK_INSTRUCTIONS, None)
            .await
    }

    /// Cratchit reviews heuristic helper candidates and keeps only genuinely
    /// generic, reusable utilities, annotating each with a purpose line.
    /// He may read the source to check a body when the signature is unclear —
    /// `read_file` is the only tool he gets for this.
    pub async fn validate_helpers(&mut self, candidates: Vec<Helper>) -> Result<Vec<Helper>> {
        const BATCH: usize = 50;
        let defs: Vec<Value> = tools::definitions()
            .into_iter()
            .filter(|d| d["function"]["name"] == "read_file")
            .collect();
        let mut kept = Vec::new();
        for batch in candidates.chunks(BATCH) {
            let listing = crate::helpers::render(batch);
            let mut log = vec![
                Message::text("system", HELPER_VALIDATION_SYSTEM),
                Message::text("user", format!("CANDIDATES:\n{listing}")),
            ];
            let text = if let Some(text) = self.tool_loop(&mut log, &defs, 20).await? {
                text
            } else {
                // Tool budget exhausted mid-batch: force a verdict rather
                // than silently dropping every candidate in the batch.
                log.push(Message::text("user", HELPER_VALIDATION_STOP));
                self.client
                    .chat("cratchit", &self.cheap_model, &log, &[], None)
                    .await?
                    .content
                    .unwrap_or_default()
            };
            for line in text.lines() {
                let Some(rest) = line.trim().strip_prefix("KEEP ") else {
                    continue;
                };
                let (name, purpose) = rest.split_once('|').unwrap_or((rest, ""));
                let name = name.trim();
                if let Some(h) = batch.iter().find(|h| h.name == name) {
                    let mut h = h.clone();
                    h.purpose = Some(purpose.trim().to_string());
                    kept.push(h);
                }
            }
        }
        Ok(kept)
    }

    /// Chat/tool loop shared by every Cratchit entry point: dispatch tool
    /// calls until the model replies with text, or return None when
    /// `max_iters` is exhausted.
    async fn tool_loop(
        &mut self,
        log: &mut Vec<Message>,
        defs: &[Value],
        max_iters: usize,
    ) -> Result<Option<String>> {
        for i in 0..max_iters {
            if max_iters - i == TOOL_BUDGET_WARNING_REMAINING {
                log.push(Message::text("user", CRATCHIT_BUDGET_WARNING));
            }
            let msg = self
                .client
                .chat("cratchit", &self.cheap_model, log, defs, None)
                .await?;
            log.push(msg.clone());
            let Some(calls) = msg.tool_calls.filter(|c| !c.is_empty()) else {
                return Ok(Some(msg.content.unwrap_or_default()));
            };
            let truncated = self.client.last_finish_reason() == Some("length");
            self.dispatch_calls(log, calls, "cratchit", truncated).await;
        }
        Ok(None)
    }

    /// Run each tool call the model emitted, tracing a preview and appending
    /// the result to the log. `who` labels the trace line (scrooge/cratchit).
    async fn dispatch_calls(
        &self,
        log: &mut Vec<Message>,
        calls: Vec<ToolCall>,
        who: &str,
        truncated: bool,
    ) {
        for call in calls {
            let args: Value = parse_tool_args(&call.function.arguments, truncated);
            eprintln!(
                "  [{who}] {}({})",
                call.function.name,
                arg_preview(&call.function.arguments)
            );
            let out = self.toolbox.call(&call.function.name, &args).await;
            log.push(Message::tool_result(&call.id, out));
        }
    }

    async fn cratchit_execute(
        &mut self,
        task: &str,
        plan: &str,
        prev_report: Option<&str>,
    ) -> Result<String> {
        self.cratchit_execute_with(task, plan, prev_report, tools::definitions(), true)
            .await
    }

    /// As `cratchit_execute`, but with an explicit tool set. Used by the
    /// overview passes to withhold the file-writing tools: the overview is
    /// captured from Cratchit's final message and saved by the orchestrator,
    /// so a Cratchit that writes overview.md itself is only a mistake.
    async fn cratchit_execute_with(
        &mut self,
        task: &str,
        plan: &str,
        prev_report: Option<&str>,
        defs: Vec<Value>,
        include_guidance: bool,
    ) -> Result<String> {
        let mut log = self.cratchit_brief(task, plan, prev_report, include_guidance)?;
        self.run_cratchit(&mut log, &defs).await
    }

    /// Build a fresh Cratchit briefing (system prompt + one user message). All
    /// context is injected deterministically rather than fetched by the model:
    /// the code map sliced to what the TASK mentions, the relevant best-practice
    /// sections, the project overview, and the previous round's report. The map
    /// and guidance slice on the task alone (not the per-step plan) so the
    /// briefing's head stays byte-identical across a run's delegations and the
    /// provider's KV cache can reuse it — Cratchit can still read any file the
    /// plan names. See `CRATCHIT_BRIEF` for the stable-first field order.
    fn cratchit_brief(
        &self,
        task: &str,
        plan: &str,
        prev_report: Option<&str>,
        include_guidance: bool,
    ) -> Result<Vec<Message>> {
        let full_map = codemap::build_cached(&self.toolbox.root)?;
        let map = full_map.brief_for(task);
        // Withheld for the overview passes: prose about purpose/architecture is
        // not coding work, so the best-practice sections are just noise there.
        let guidance = if include_guidance {
            format!(
                "GUIDANCE:\n{}\n",
                practices::relevant_sections(task, &full_map.languages())
            )
        } else {
            String::new()
        };
        // Loaded, never generated, here — ensure_overview() itself runs
        // through cratchit_execute, so generating would recurse.
        let overview = crate::overview::load(&self.toolbox.root)
            .map(|o| format!("\nPROJECT OVERVIEW:\n{o}\n"))
            .unwrap_or_default();
        let prev = prev_report
            .map(|r| format!("\nPREVIOUS ROUND REPORT (already verified facts):\n{r}\n"))
            .unwrap_or_default();
        Ok(vec![
            Message::text("system", CRATCHIT_SYSTEM),
            Message::text(
                "user",
                CRATCHIT_BRIEF
                    .replace("{ROOT}", &self.toolbox.root.display().to_string())
                    .replace("{TASK}", task)
                    .replace("{OVERVIEW}", &overview)
                    .replace("{MAP}", &map)
                    .replace("{GUIDANCE}", &guidance)
                    .replace("{PLAN}", plan)
                    .replace("{PREV}", &prev),
            ),
        ])
    }

    /// Drive a Cratchit conversation to a final report: loop tool calls until he
    /// replies with text. If the tool budget is exhausted first, force a final
    /// report with no tools available — so `REPORT_MAX_TOKENS` can cap it
    /// without any risk of truncating a tool call mid-JSON. The `log` is taken by
    /// reference so callers (the check-retry loop) can keep the conversation
    /// alive across several stretches.
    async fn run_cratchit(&mut self, log: &mut Vec<Message>, defs: &[Value]) -> Result<String> {
        if let Some(text) = self.tool_loop(log, defs, CRATCHIT_MAX_ITERS).await? {
            return Ok(text);
        }
        // Tool budget exhausted: force a final report deterministically so
        // Scrooge never pays a round to read "hit the limit".
        log.push(Message::text("user", CRATCHIT_STOP));
        let msg = self
            .client
            .chat(
                "cratchit",
                &self.cheap_model,
                log,
                &[],
                Some(REPORT_MAX_TOKENS),
            )
            .await?;
        Ok(msg
            .content
            .filter(|c| !c.trim().is_empty())
            .unwrap_or_else(|| "cratchit hit the tool-call limit without reporting".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::openrouter::{FunctionCall, Usage};
    use std::collections::VecDeque;

    /// A scripted `Chat` backend: each `chat` call pops the next canned reply.
    /// Lets the agent loop be exercised end-to-end with zero network access —
    /// no request ever leaves the process.
    struct FakeChat {
        replies: VecDeque<Message>,
        usage: Usage,
    }

    impl FakeChat {
        fn new(replies: Vec<Message>) -> Self {
            Self {
                replies: replies.into(),
                usage: Usage::default(),
            }
        }
    }

    impl Chat for FakeChat {
        async fn chat(
            &mut self,
            _agent: &str,
            _model: &str,
            _messages: &[Message],
            _tools: &[Value],
            _max_tokens: Option<u32>,
        ) -> Result<Message> {
            Ok(self
                .replies
                .pop_front()
                .expect("FakeChat ran out of scripted replies"))
        }
        fn usage(&self) -> &Usage {
            &self.usage
        }
        fn last_finish_reason(&self) -> Option<&str> {
            None
        }
    }

    fn tool_call_msg(id: &str, name: &str, arguments: &str) -> Message {
        Message {
            role: "assistant".into(),
            content: None,
            tool_calls: Some(vec![ToolCall {
                id: id.into(),
                kind: "function".into(),
                function: FunctionCall {
                    name: name.into(),
                    arguments: arguments.into(),
                },
            }]),
            tool_call_id: None,
        }
    }

    fn orchestrator(client: FakeChat) -> Orchestrator<FakeChat> {
        let root = std::env::temp_dir().join(format!("scrooge-fake-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        build_orchestrator(root, client)
    }

    fn build_orchestrator(root: PathBuf, client: FakeChat) -> Orchestrator<FakeChat> {
        Orchestrator {
            client,
            toolbox: Toolbox::new(root),
            cheap_model: "fake-cheap".into(),
            sota_model: "fake-sota".into(),
            max_turns: 20,
        }
    }

    /// A clean, per-test temp directory (unique by test tag) with a `.scrooge`
    /// dir ready. Not a git repo — call `git_init` when the flow needs one.
    fn fresh_root(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("scrooge-test-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(".scrooge")).unwrap();
        root
    }

    fn git_init(root: &Path) {
        let ok = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(root)
            .status()
            .is_ok_and(|s| s.success());
        assert!(ok, "git init failed in test setup");
    }

    /// Stage everything and commit, with an inline identity so the test doesn't
    /// depend on the machine's git config.
    fn git_commit_all(root: &Path) {
        let status = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(root)
                .status()
                .is_ok_and(|s| s.success())
        };
        assert!(status(&["add", "-A"]), "git add failed");
        assert!(
            status(&[
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-q",
                "-m",
                "init",
            ]),
            "git commit failed"
        );
    }

    /// Write a `.scrooge/checks.toml` with a single synthetic language whose
    /// commands are plain shell, so checks pass/fail deterministically without a
    /// real toolchain.
    fn seed_checks(root: &Path, quick: &str, test: &str) {
        std::fs::write(
            root.join(".scrooge").join("checks.toml"),
            format!("[synthetic]\nquick = {quick:?}\ntest = {test:?}\n"),
        )
        .unwrap();
    }

    /// Pre-seed the overview so `ensure_overview` loads it instead of spending a
    /// (scripted) model call to write one.
    fn seed_overview(root: &Path) {
        std::fs::write(root.join(".scrooge").join("overview.md"), "A test project.").unwrap();
    }

    fn delegate_call(id: &str, instructions: &str) -> Message {
        tool_call_msg(
            id,
            "delegate_to_cratchit",
            &format!("{{\"instructions\": {}}}", serde_json::json!(instructions)),
        )
    }

    /// The shared Cratchit loop dispatches a scripted tool call against the real
    /// (deterministic, offline) toolbox, then returns the model's final text —
    /// all from spoofed replies, proving the loop never touches the network.
    #[tokio::test]
    async fn tool_loop_dispatches_then_returns_final_text() {
        // Reply 1: ask for a free, offline call-graph lookup. Reply 2: the
        // final report once the tool result is in hand.
        let mut orch = orchestrator(FakeChat::new(vec![
            tool_call_msg("c1", "callers", r#"{"name": "nonexistent_fn"}"#),
            Message::text("assistant", "done: examined the call graph"),
        ]));
        let defs = tools::definitions();
        let mut log = vec![Message::text("user", "investigate")];
        let out = orch.tool_loop(&mut log, &defs, 40).await.unwrap();
        assert_eq!(out.as_deref(), Some("done: examined the call graph"));
        // The tool result for the dispatched call landed back in the log.
        assert!(
            log.iter().any(|m| m.tool_call_id.as_deref() == Some("c1")),
            "expected a tool result for the dispatched call in the log"
        );
    }

    /// When the model never asks for a tool, the loop returns its text on the
    /// first turn without spending any of the iteration budget on tools.
    #[tokio::test]
    async fn tool_loop_returns_immediately_without_tool_calls() {
        let mut orch = orchestrator(FakeChat::new(vec![Message::text(
            "assistant",
            "no tools needed",
        )]));
        let mut log = vec![Message::text("user", "answer directly")];
        let out = orch.tool_loop(&mut log, &[], 40).await.unwrap();
        assert_eq!(out.as_deref(), Some("no tools needed"));
    }

    #[test]
    fn parse_tool_args_recovers_truncated_instruction() {
        // Well-formed args parse regardless of the truncation flag.
        let v = parse_tool_args(r#"{"instructions": "edit foo.rs"}"#, false);
        assert_eq!(v["instructions"], "edit foo.rs");

        // Cut mid-string (the output cap's typical landing spot): with the
        // truncation flag set, the surviving prefix is kept.
        let v = parse_tool_args(
            r#"{"instructions": "edit foo.rs and rename bar to ba"#,
            true,
        );
        assert_eq!(v["instructions"], "edit foo.rs and rename bar to ba");

        // Cut right after an escaped quote inside the string.
        let v = parse_tool_args(r#"{"instructions": "set the flag to \"on"#, true);
        assert_eq!(v["instructions"], "set the flag to \"on");

        // Cut between fields, leaving a dangling comma.
        let v = parse_tool_args(r#"{"instructions": "do it","#, true);
        assert_eq!(v["instructions"], "do it");

        // Unrecoverable garbage falls back to Null rather than panicking.
        assert!(parse_tool_args("{not json at all", true).is_null());

        // Without the truncation signal, broken JSON is NOT repaired — a genuine
        // malformed call returns Null instead of a guessed (partial) value.
        assert!(parse_tool_args(r#"{"instructions": "edit foo.rs and rename"#, false).is_null());
    }

    #[test]
    fn clamp_report_caps_lines_and_marks_truncation() {
        let many = (0..30)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let out = clamp_report(&many);
        assert!(out.contains("[report clamped]"));
        // The capped body keeps MAX_REPORT_LINES, plus the one marker line.
        assert_eq!(out.lines().count(), MAX_REPORT_LINES + 1);
        // A short report passes through untouched.
        assert_eq!(clamp_report("a\nb"), "a\nb");
    }

    /// The full planning loop, driven entirely by scripted replies: Scrooge ends
    /// his turn with no tool call and, since nothing was changed on disk, the run
    /// finishes immediately without a single check ever running.
    #[tokio::test]
    async fn run_task_finishes_when_nothing_changed() {
        let root = fresh_root("noop");
        git_init(&root);
        seed_overview(&root);
        let mut orch = build_orchestrator(
            root,
            FakeChat::new(vec![Message::text("assistant", "DONE")]),
        );
        let out = orch.run_task("do nothing").await.unwrap();
        // One Scrooge turn (the immediate DONE), no work done.
        assert!(out.contains("1 turn"), "unexpected: {out}");
    }

    /// One delegation, then DONE: the delegation is counted, Cratchit's text
    /// report flows back (he changed nothing, so no CHECKS verdict is owed), and
    /// the loop accepts completion.
    #[tokio::test]
    async fn run_task_runs_one_delegation_then_finishes() {
        let root = fresh_root("delegate");
        git_init(&root);
        seed_overview(&root);
        let mut orch = build_orchestrator(
            root,
            FakeChat::new(vec![
                delegate_call("d1", "investigate foo, change nothing"),
                Message::text("assistant", "looked at foo; nothing to change"),
                Message::text("assistant", "DONE"),
            ]),
        );
        let out = orch.run_task("inspect foo").await.unwrap();
        // Two Scrooge turns: the delegation, then the DONE.
        assert!(out.contains("2 turn"), "unexpected: {out}");
    }

    /// The code-changing entry points refuse a non-git root rather than silently
    /// losing their change-detection (and the read-only/CHECKS contract with it).
    #[tokio::test]
    async fn run_task_requires_a_git_repo() {
        let root = fresh_root("nogit");
        let mut orch = build_orchestrator(root, FakeChat::new(vec![]));
        let err = orch.run_task("anything").await.unwrap_err();
        assert!(format!("{err:#}").contains("git"), "unexpected: {err:#}");
    }

    /// Scrooge gets at most one delegation per turn: a second call in the same
    /// turn is rejected and does not advance the step counter.
    #[tokio::test]
    async fn second_delegation_in_one_turn_is_rejected() {
        let root = fresh_root("oneper");
        git_init(&root);
        let mut orch = build_orchestrator(root, FakeChat::new(vec![]));
        let mut st = RunState::default();
        let mut delegated = true; // pretend a delegation already happened this turn
        let call = ToolCall {
            id: "x".into(),
            kind: "function".into(),
            function: FunctionCall {
                name: "delegate_to_cratchit".into(),
                arguments: r#"{"instructions": "do x"}"#.into(),
            },
        };
        let out = orch
            .run_scrooge_call("task", &call, false, &mut delegated, &mut st)
            .await
            .unwrap();
        assert!(out.contains("only one delegate"), "unexpected: {out}");
    }

    /// An empty/unrepairable `instructions` is bounced without burning a
    /// delegation or briefing Cratchit with nothing.
    #[tokio::test]
    async fn empty_instructions_are_bounced() {
        let root = fresh_root("empty");
        git_init(&root);
        let mut orch = build_orchestrator(root, FakeChat::new(vec![]));
        let mut st = RunState::default();
        let mut delegated = false;
        let call = ToolCall {
            id: "x".into(),
            kind: "function".into(),
            function: FunctionCall {
                name: "delegate_to_cratchit".into(),
                arguments: r#"{"instructions": "   "}"#.into(),
            },
        };
        let out = orch
            .run_scrooge_call("task", &call, false, &mut delegated, &mut st)
            .await
            .unwrap();
        assert!(out.contains("non-empty"), "unexpected: {out}");
        assert!(
            !delegated,
            "a bounced call must not consume the turn's delegation"
        );
    }

    /// `web_answer` is refused once the per-task budget is spent — and the refusal
    /// is decided locally, without dispatching to the toolbox (no network).
    #[tokio::test]
    async fn web_answer_budget_is_enforced() {
        let root = fresh_root("web");
        git_init(&root);
        let mut orch = build_orchestrator(root, FakeChat::new(vec![]));
        let mut st = RunState {
            web_calls: SCROOGE_WEB_LOOKUPS,
            ..RunState::default()
        };
        let mut delegated = false;
        let call = ToolCall {
            id: "w".into(),
            kind: "function".into(),
            function: FunctionCall {
                name: "web_answer".into(),
                arguments: r#"{"query": "anything"}"#.into(),
            },
        };
        let out = orch
            .run_scrooge_call("task", &call, false, &mut delegated, &mut st)
            .await
            .unwrap();
        assert!(out.contains("budget exhausted"), "unexpected: {out}");
    }

    fn write_file_call(id: &str, path: &str, content: &str) -> Message {
        tool_call_msg(
            id,
            "write_file",
            &format!(
                "{{\"path\": {}, \"content\": {}}}",
                serde_json::json!(path),
                serde_json::json!(content)
            ),
        )
    }

    /// A code-changing dispatch whose quick check fails: the failure is routed
    /// back to Cratchit `CHECK_RETRIES` times, then reported to Scrooge as a
    /// FAILING verdict rather than spinning forever.
    #[tokio::test]
    async fn execute_and_verify_escalates_after_check_retries() {
        let root = fresh_root("checkretry");
        git_init(&root);
        seed_overview(&root);
        seed_checks(&root, "exit 1", "exit 1");
        // Reply 1+2: Cratchit writes a file then reports. Replies 3+4: the two
        // retries (the quick check fails every time, so it never goes green).
        let mut orch = build_orchestrator(
            root,
            FakeChat::new(vec![
                write_file_call("w", "f.txt", "x"),
                Message::text("assistant", "wrote f.txt"),
                Message::text("assistant", "still failing 1"),
                Message::text("assistant", "still failing 2"),
            ]),
        );
        let (report, verdict) = orch
            .execute_and_verify("t", "create f.txt", None, CheckTier::Quick)
            .await
            .unwrap();
        assert_eq!(verdict, Some(false));
        assert!(report.contains("CHECKS: FAILING"), "unexpected: {report}");
    }

    /// A code-changing dispatch whose quick check passes ends with a clean
    /// verdict on the first attempt — no retry loop.
    #[tokio::test]
    async fn execute_and_verify_reports_clean_when_check_passes() {
        let root = fresh_root("checkclean");
        git_init(&root);
        seed_overview(&root);
        seed_checks(&root, "true", "true");
        let mut orch = build_orchestrator(
            root,
            FakeChat::new(vec![
                write_file_call("w", "f.txt", "x"),
                Message::text("assistant", "wrote f.txt"),
            ]),
        );
        let (report, verdict) = orch
            .execute_and_verify("t", "create f.txt", None, CheckTier::Quick)
            .await
            .unwrap();
        assert_eq!(verdict, Some(true));
        assert!(report.contains("CHECKS: clean"), "unexpected: {report}");
    }

    /// A read-only dispatch (Cratchit changes nothing on disk) returns his
    /// findings with no CHECKS line and a None verdict — the checks are skipped.
    #[tokio::test]
    async fn execute_and_verify_skips_checks_when_nothing_changed() {
        let root = fresh_root("nochange");
        git_init(&root);
        seed_overview(&root);
        seed_checks(&root, "exit 1", "exit 1"); // would fail if it ever ran
        let mut orch = build_orchestrator(
            root,
            FakeChat::new(vec![Message::text(
                "assistant",
                "investigated; nothing to change",
            )]),
        );
        let (report, verdict) = orch
            .execute_and_verify("t", "just look", None, CheckTier::Quick)
            .await
            .unwrap();
        assert_eq!(verdict, None);
        assert!(!report.contains("CHECKS"), "unexpected: {report}");
    }

    /// `try_finish` keeps nudging Scrooge to fix red checks until the attempt
    /// budget is spent, then gives up rather than looping.
    #[tokio::test]
    async fn try_finish_nudges_then_gives_up_on_red_checks() {
        let root = fresh_root("finishred");
        git_init(&root);
        let mut orch = build_orchestrator(root, FakeChat::new(vec![]));
        let structure_before: Vec<String> = vec![];

        // First call (attempts remaining): nudge, don't finish.
        let mut log = Vec::new();
        let mut st = RunState {
            dirty: true,
            checks_clean: Some(false),
            finish_attempts: 0,
            ..RunState::default()
        };
        let out = orch
            .try_finish("t", &mut log, &mut st, &structure_before)
            .await
            .unwrap();
        assert!(out.is_none(), "should not finish while checks are red");
        assert_eq!(st.finish_attempts, 1);
        assert_eq!(
            log.last().and_then(|m| m.content.as_deref()),
            Some(SCROOGE_DONE_WITH_FAILURES)
        );

        // Last call (budget spent): give up with a terminal message.
        st.finish_attempts = MAX_FINISH_ATTEMPTS;
        let out = orch
            .try_finish("t", &mut log, &mut st, &structure_before)
            .await
            .unwrap();
        assert_eq!(
            out.as_deref(),
            Some("checks still failing; review the output above")
        );
    }

    /// With nothing changed, `try_finish` accepts completion immediately without
    /// running any checks.
    #[tokio::test]
    async fn try_finish_accepts_when_not_dirty() {
        let root = fresh_root("finishclean");
        git_init(&root);
        let mut orch = build_orchestrator(root, FakeChat::new(vec![]));
        let mut log = Vec::new();
        let mut st = RunState::default(); // dirty: false
        let out = orch.try_finish("t", &mut log, &mut st, &[]).await.unwrap();
        assert!(
            out.unwrap().contains("turn"),
            "expected a completion banner"
        );
    }

    /// The full suite gates DONE: green quick checks plus a passing full suite
    /// accept completion (structure unchanged here, so no overview pass).
    #[tokio::test]
    async fn try_finish_accepts_when_full_suite_clean() {
        let root = fresh_root("finishfull");
        git_init(&root);
        seed_overview(&root);
        seed_checks(&root, "true", "true");
        let structure_before = codemap::build_cached(&root).unwrap().structure_signature();
        let mut orch = build_orchestrator(root, FakeChat::new(vec![]));
        let mut log = Vec::new();
        let mut st = RunState {
            dirty: true,
            checks_clean: Some(true),
            ..RunState::default()
        };
        let out = orch
            .try_finish("t", &mut log, &mut st, &structure_before)
            .await
            .unwrap();
        assert!(
            out.unwrap().contains("turn"),
            "expected a completion banner"
        );
    }

    /// Two edits with an identical `git diff --stat` (one changed line each)
    /// still yield distinct fingerprints — the bug the hash fixes — and a revert
    /// reproduces the clean fingerprint exactly.
    #[test]
    fn worktree_fingerprint_distinguishes_same_stat_edits() {
        let root = fresh_root("fingerprint");
        git_init(&root);
        std::fs::write(root.join("a.txt"), "a\nb\nc\n").unwrap();
        git_commit_all(&root);
        let clean = worktree_fingerprint(&root);
        assert!(clean.is_some(), "fingerprint needs a git repo");

        std::fs::write(root.join("a.txt"), "X\nb\nc\n").unwrap();
        let fp1 = worktree_fingerprint(&root);
        std::fs::write(root.join("a.txt"), "Y\nb\nc\n").unwrap();
        let fp2 = worktree_fingerprint(&root);

        assert_ne!(clean, fp1, "an edit must change the fingerprint");
        assert_ne!(fp1, fp2, "same-stat edits must still differ");

        std::fs::write(root.join("a.txt"), "a\nb\nc\n").unwrap();
        assert_eq!(worktree_fingerprint(&root), clean, "revert restores it");
    }

    #[test]
    fn close_truncated_json_balances_open_structures() {
        // Open string is closed.
        assert_eq!(close_truncated_json(r#"{"a": "hello"#), r#"{"a": "hello"}"#);
        // Nested array+object are closed in reverse.
        assert_eq!(close_truncated_json(r#"{"a": [1, 2"#), r#"{"a": [1, 2]}"#);
        // A dangling separator between elements is dropped.
        assert_eq!(close_truncated_json(r#"{"a": 1,"#), r#"{"a": 1}"#);
        // A trailing escape can't be allowed to escape our closing quote.
        assert_eq!(close_truncated_json(r#"{"a": "x\"#), r#"{"a": "x"}"#);
        // An already-closed string inside an open object.
        assert_eq!(close_truncated_json(r#"{"a": "x""#), r#"{"a": "x"}"#);
        // The repaired output always parses.
        assert!(serde_json::from_str::<Value>(&close_truncated_json(r#"{"a": [{"b": "c"#)).is_ok());
    }
}
