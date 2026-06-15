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
use crate::openrouter::{Client, DEV_MODEL_CHEAP, DEV_MODEL_SOTA, Message, ToolCall};
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
- symbol_info / callers / callees: free, instant call-graph lookups (a symbol's \
  signature, who calls it, what it calls). Use them to gauge the blast radius of a change \
  before delegating — they cost nothing, so never spend a delegation just to ask who calls \
  what.\n\
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

/// Instruction to reconsider the overview after a completed task.
/// Placeholder: {CHANGED}.
const OVERVIEW_REFRESH_INSTRUCTIONS: &str = "\
The task above is complete. Review the PROJECT OVERVIEW in your \
briefing against what changed:\n{CHANGED}\n\n\
Decide whether the overview's description of purpose or architecture \
is now stale. Do NOT modify any files. Your final message must be \
EITHER the single word UNCHANGED (if it is still accurate), OR the \
full rewritten overview — first line: what the project is in one \
sentence, then one short prose paragraph on architecture and \
non-obvious invariants, at most 15 lines. Output ONLY that, nothing else.";

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

/// Largest char boundary <= `i`, so byte-budget truncation never panics on
/// multibyte UTF-8 (reports and check output routinely contain `—`, `’`, …).
const fn floor_char_boundary(s: &str, mut i: usize) -> usize {
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Clamp a Cratchit report to the size rule 7 promises.
fn clamp_report(s: &str) -> String {
    let mut out = s
        .lines()
        .take(MAX_REPORT_LINES)
        .collect::<Vec<_>>()
        .join("\n");
    if out.len() > MAX_REPORT_CHARS {
        out.truncate(floor_char_boundary(&out, MAX_REPORT_CHARS));
        out.push_str("\n[report clamped]");
    } else if s.lines().count() > MAX_REPORT_LINES {
        out.push_str("\n[report clamped]");
    }
    out
}

/// True when an overview-review reply means "no rewrite needed". The model is
/// asked for the single word UNCHANGED but often prefaces it with a sentence of
/// reasoning ("The architecture is unchanged. UNCHANGED"), so treat the reply
/// as a verdict if any line, stripped of markdown emphasis/punctuation, is just
/// UNCHANGED — rather than only the exact-string case.
fn is_unchanged_verdict(text: &str) -> bool {
    text.lines().any(|l| {
        let l = l.trim().trim_matches(|c: char| !c.is_alphanumeric());
        l.eq_ignore_ascii_case("UNCHANGED")
    })
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
        &args[..floor_char_boundary(args, MAX)],
        args.len()
    )
}

/// Keep the tail of `s` (failure summaries end up at the bottom).
fn tail(s: &str, max_chars: usize) -> String {
    let s = s.trim();
    if s.len() <= max_chars {
        return s.to_string();
    }
    let cut = floor_char_boundary(s, s.len() - max_chars);
    let cut = s[cut..].find('\n').map_or(cut, |i| cut + i + 1);
    format!("[...]\n{}", &s[cut..])
}

/// Worktree state as git sees it: diffstat of tracked changes plus untracked
/// files. None when the root is not a git repo (or git is unavailable).
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

pub struct Orchestrator {
    client: Client,
    toolbox: Toolbox,
    cheap_model: String,
    sota_model: String,
    max_steps: usize,
}

impl Orchestrator {
    pub fn new(root: PathBuf) -> Result<Self> {
        Ok(Self {
            client: Client::new(root.clone())?,
            toolbox: Toolbox::new(root),
            cheap_model: std::env::var("CRATCHIT_MODEL").unwrap_or_else(|_| DEV_MODEL_CHEAP.into()),
            sota_model: std::env::var("SCROOGE_MODEL").unwrap_or_else(|_| DEV_MODEL_SOTA.into()),
            max_steps: 20,
        })
    }

    /// Full task loop: Scrooge delegates to Cratchit via the
    /// `delegate_to_cratchit` tool, sees each report as a tool result, and
    /// adapts step by step. The initial briefing is built once and never
    /// rebuilt — the provider's KV cache keeps it cheap on every subsequent
    /// Scrooge turn, which also eliminates the stale-context re-injection
    /// problem of the old round-based loop.
    pub async fn run_task(&mut self, task: &str) -> Result<String> {
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

        let mut delegations = 0usize;
        let mut web_calls = 0usize;
        let mut last_report: Option<String> = None;
        let mut checks_clean: Option<bool> = None;
        let mut dirty = false;
        let mut finish_attempts = 0usize;

        loop {
            if delegations >= self.max_steps {
                break;
            }
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
                    .try_finish(
                        task,
                        &mut log,
                        dirty,
                        &mut checks_clean,
                        &mut finish_attempts,
                        delegations,
                        &structure_before,
                    )
                    .await?
                {
                    return Ok(out);
                }
                continue; // checks red — try_finish nudged Scrooge to fix them
            };

            // A turn cut off by the output cap may carry a clipped tool call;
            // only then do we let parse_tool_args attempt a repair.
            let truncated = self.client.last_finish_reason.as_deref() == Some("length");
            // At most one delegation per turn: Cratchit's report (and its CHECKS
            // verdict) must come back before the next step is planned, so a
            // second delegate call in the same turn was planned blind.
            let mut delegated = false;
            for call in calls {
                let result = self
                    .run_scrooge_call(
                        task,
                        &call,
                        truncated,
                        &mut delegated,
                        &mut delegations,
                        &mut web_calls,
                        &mut last_report,
                        &mut checks_clean,
                        &mut dirty,
                    )
                    .await?;
                log.push(Message::tool_result(&call.id, result));
            }
        }
        Ok("step limit reached without DONE; review output above".into())
    }

    /// Run one Scrooge delegation: execute + verify, fold the verdict into the
    /// run's running state, and return the report Scrooge will read.
    async fn run_delegation(
        &mut self,
        task: &str,
        instructions: &str,
        step: usize,
        last_report: &mut Option<String>,
        checks_clean: &mut Option<bool>,
        dirty: &mut bool,
    ) -> Result<String> {
        eprintln!("--- scrooge → cratchit (step {step}) ---\n{instructions}\n");
        let (report, verdict) = self
            .execute_and_verify(task, instructions, last_report.as_deref(), CheckTier::Quick)
            .await?;
        if let Some(c) = verdict {
            *checks_clean = Some(c);
            *dirty = true;
        }
        eprintln!("--- cratchit report ---\n{report}\n");
        *last_report = Some(report.clone());
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
        delegations: &mut usize,
        web_calls: &mut usize,
        last_report: &mut Option<String>,
        checks_clean: &mut Option<bool>,
        dirty: &mut bool,
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
            *delegations += 1;
            return self
                .run_delegation(
                    task,
                    &instructions,
                    *delegations,
                    last_report,
                    checks_clean,
                    dirty,
                )
                .await;
        }
        if name == "web_answer" {
            if *web_calls >= SCROOGE_WEB_LOOKUPS {
                return Ok(
                    "web_answer budget exhausted — decide with what you have and delegate."
                        .to_string(),
                );
            }
            *web_calls += 1;
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
        dirty: bool,
        checks_clean: &mut Option<bool>,
        finish_attempts: &mut usize,
        delegations: usize,
        structure_before: &[String],
    ) -> Result<Option<String>> {
        if !dirty {
            return Ok(Some(Self::bill(delegations))); // nothing changed to verify
        }
        if *checks_clean == Some(false) {
            // Quick checks from the last step are still red.
            if *finish_attempts < MAX_FINISH_ATTEMPTS {
                *finish_attempts += 1;
                log.push(Message::text("user", SCROOGE_DONE_WITH_FAILURES));
                return Ok(None);
            }
            return Ok(Some("checks still failing; review the output above".into()));
        }
        // Quick checks are green; run the full suite once before accepting DONE.
        let full = self.run_checks(CheckTier::Full).await?;
        if !full.errors.is_empty() || !full.warnings.is_empty() {
            if *finish_attempts < MAX_FINISH_ATTEMPTS {
                *finish_attempts += 1;
                *checks_clean = Some(false);
                let failures = tail(&checks::render(&full), MAX_FAIL_CHARS);
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
        // Cratchit pass (#4).
        let structure_after = codemap::build_cached(&self.toolbox.root)?.structure_signature();
        if structure_after == *structure_before {
            eprintln!("--- overview review skipped: no structural change ---");
        } else {
            self.refresh_overview(task).await;
        }
        Ok(Some(Self::bill(delegations)))
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
                if text.is_empty() || is_unchanged_verdict(&text) {
                    eprintln!("--- overview review: unchanged ---");
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
    fn bill(delegations: usize) -> String {
        format!("task complete in {delegations} delegation(s).")
    }

    /// Two-line footer for `scrooge run`/`scrooge cratchit`: what this request
    /// actually cost (Cratchit's wages) and the shillings saved versus running
    /// it all on the pricey Scrooge model. The usage is per-request because the
    /// orchestrator is built fresh for each command invocation.
    pub fn wages_footer(&self) -> String {
        let u = &self.client.usage;
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
        let before = worktree_changes(&self.toolbox.root);
        let defs = tools::definitions();
        let mut log = self.cratchit_brief(task, instructions, prev_report, true)?;
        let mut report = clamp_report(&self.run_cratchit(&mut log, &defs).await?);

        let after = worktree_changes(&self.toolbox.root);
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
                tail(&failures, MAX_FAIL_CHARS)
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
        // Same rule as the native loop: an overview exists before any work
        // is planned against the codebase.
        self.ensure_overview(task).await?;
        let before = (
            self.client.usage.prompt_tokens,
            self.client.usage.completion_tokens,
            self.client.usage.cost_usd,
        );
        let (report, _) = self
            .execute_and_verify(task, instructions, None, CheckTier::Full)
            .await?;
        let u = &self.client.usage;
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
            let truncated = self.client.last_finish_reason.as_deref() == Some("length");
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

    #[test]
    fn unchanged_verdict_tolerates_preamble() {
        assert!(is_unchanged_verdict("UNCHANGED"));
        assert!(is_unchanged_verdict(
            "The architecture still holds.\nUNCHANGED"
        ));
        assert!(is_unchanged_verdict("**UNCHANGED**"));
        assert!(is_unchanged_verdict("unchanged."));
        // A real rewritten overview is not a verdict.
        assert!(!is_unchanged_verdict(
            "Scrooge is a token-miserly coding agent.\nIt splits planning from execution."
        ));
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
}
