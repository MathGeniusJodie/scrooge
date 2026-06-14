//! Orchestration. Scrooge (expensive SOTA model) only ever sees compact
//! briefs and terse reports — he plans, reviews, and decides. Cratchit
//! (cheap model) does all the token-heavy tool work and must compress
//! everything he sends upstairs.
//!
//! Everything that can be decided deterministically is: the code map and
//! relevant guidance are injected rather than fetched by the model; every
//! execution ends with machine-generated CHANGED (git diffstat) and CHECKS
//! lines appended to the report; mechanical check failures loop straight
//! back to Cratchit without burning a Scrooge round; and DONE is only
//! accepted while checks are green.

use anyhow::Result;
use serde_json::Value;
use std::fmt::Write;
use std::path::{Path, PathBuf};

use crate::checks;
use crate::codemap;
use crate::helpers::Helper;
use crate::openrouter::{Client, DEV_MODEL_CHEAP, DEV_MODEL_SOTA, Message};
use crate::practices;
use crate::tools::{self, Toolbox};

const SCROOGE_SYSTEM: &str = "\
You are Scrooge, a senior software architect. Your time is extremely valuable, \
so you receive only compressed briefs and you produce only terse, high-leverage output. \
You never read full files, never write code, and never use tools. You direct Cratchit, \
a junior agent with full tool access (files, shell, python, wolfram, docs, call graph).\n\
When given a task and a codebase brief, reply with a numbered plan of concrete steps \
for Cratchit. Each step: one line, imperative, naming exact files/symbols where known. \
Each numbered step is dispatched to a FRESH Cratchit in order, so every step must be \
independently executable and verifiable; group work that belongs together into one step. \
Every Cratchit report ends with machine-generated CHANGED (git diffstat) and CHECKS \
(format/test/lint verdict) lines — trust those over Cratchit's own claims. When given \
a progress report, reply either with corrections/next steps (same format) or the \
single word DONE if the task is complete and CHECKS is clean. No preamble, no prose.";

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

/// Per-round briefing for Scrooge. Placeholders: {TASK} {OVERVIEW} {BRIEF}
/// {GUIDANCE}.
const SCROOGE_BRIEF: &str = "\
TASK: {TASK}\n\nPROJECT OVERVIEW:\n{OVERVIEW}\n\n\
CODEBASE BRIEF:\n{BRIEF}\nKEY GUIDANCE:\n{GUIDANCE}";

/// Briefing for a fresh Cratchit. Placeholders: {ROOT} {TASK} {OVERVIEW}
/// {PLAN} {PREV} {MAP} {GUIDANCE}.
const CRATCHIT_BRIEF: &str = "\
PROJECT ROOT: {ROOT}\nTASK: {TASK}\n{OVERVIEW}\n\
SCROOGE'S INSTRUCTIONS:\n{PLAN}\n{PREV}\n\
CODE MAP (files mentioned in the instructions shown in full):\n{MAP}\n\
GUIDANCE:\n{GUIDANCE}";

/// Wrapper handed to a per-step Cratchit so it sees the whole plan but only
/// executes its own step. Placeholders: {PLAN} {N} {STEP}.
const STEP_INSTRUCTIONS: &str = "\
FULL PLAN (context only — do NOT execute other steps):\n{PLAN}\n\n\
Execute ONLY step {N}:\n{STEP}";

/// Corrective step synthesized (no Scrooge round) when DONE is proposed while
/// checks are red. Placeholder: {FAILURES}.
const CHECK_FAILING_PLAN: &str = "\
1. The deterministic check suite is failing. Fix the failures below, then \
verify.\n{FAILURES}";

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

/// Completion cap for Scrooge: plans are numbered one-liners, so anything
/// past this is waste on the expensive model.
const SCROOGE_MAX_TOKENS: u32 = 700;

/// How many times a failing check report is routed straight back to Cratchit
/// before the failure is escalated to Scrooge.
const CHECK_RETRIES: usize = 2;

/// Hard caps enforcing rule 7 (the ≤6-line report) in code: Cratchit's final
/// message is clamped before anyone expensive reads it.
const MAX_REPORT_LINES: usize = 12;
const MAX_REPORT_CHARS: usize = 1200;

/// How much of a check-failure dump Scrooge sees. Cratchit already got the
/// full output during the retry loop; Scrooge only needs the gist.
const MAX_FAIL_CHARS: usize = 600;

/// Full briefs larger than this are sliced to task-relevant files on review
/// rounds (round 1 always gets the full brief — Scrooge is planning then).
const SLICE_BRIEF_OVER: usize = 4000;

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

/// Split Scrooge's numbered plan into steps ("1. ..." / "2) ..."); unnumbered
/// continuation lines stick to their step. A plan with no numbering is one
/// step. Each step gets its own fresh Cratchit.
fn plan_steps(plan: &str) -> Vec<String> {
    let mut steps: Vec<String> = Vec::new();
    for line in plan.lines() {
        let t = line.trim_start();
        let digits = t.chars().take_while(char::is_ascii_digit).count();
        let is_new = digits > 0
            && t[digits..]
                .chars()
                .next()
                .is_some_and(|c| c == '.' || c == ')');
        if is_new {
            steps.push(line.trim().to_string());
        } else if let Some(cur) = steps.last_mut() {
            cur.push('\n');
            cur.push_str(line);
        }
    }
    if steps.is_empty() {
        vec![plan.trim().to_string()]
    } else {
        steps
    }
}

/// Compress a multi-step report for Scrooge: completed intermediate steps
/// shrink to their first lines plus the CHECKS verdict; the final step and
/// any failing step stay full (a failing step may not be last — a "[steps
/// skipped]" stub follows it).
fn digest_steps(reports: &[String]) -> String {
    let mut out = Vec::new();
    for (i, r) in reports.iter().enumerate() {
        let lines: Vec<&str> = r.lines().collect();
        if i + 1 == reports.len() || lines.len() <= 6 || r.contains("CHECKS: FAILING") {
            out.push(r.clone());
        } else {
            out.push(format!(
                "{}\n[...]\n{}",
                lines[..4].join("\n"),
                lines.last().unwrap_or(&"")
            ));
        }
    }
    out.join("\n")
}

/// Tool results older than this many assistant turns are evicted from
/// Cratchit's log — without this, every old file dump is re-paid on each of
/// up to 40 loop iterations (quadratic growth).
const EVICT_AFTER_TURNS: usize = 8;
const EVICT_MIN_CHARS: usize = 400;

/// Replace large tool results older than `EVICT_AFTER_TURNS` assistant turns
/// with a stub. The model can always re-run a tool it still needs.
fn evict_old_tool_results(log: &mut [Message]) {
    let assistants: Vec<usize> = log
        .iter()
        .enumerate()
        .filter(|(_, m)| m.role == "assistant")
        .map(|(i, _)| i)
        .collect();
    if assistants.len() <= EVICT_AFTER_TURNS {
        return;
    }
    let cutoff = assistants[assistants.len() - EVICT_AFTER_TURNS];
    for m in &mut log[..cutoff] {
        if m.role == "tool"
            && m.content
                .as_deref()
                .is_some_and(|c| c.len() > EVICT_MIN_CHARS)
        {
            m.content = Some(
                "[old result evicted to save tokens — re-run the tool if still needed]".into(),
            );
        }
    }
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

pub struct Orchestrator {
    client: Client,
    toolbox: Toolbox,
    cheap_model: String,
    sota_model: String,
    max_rounds: usize,
}

impl Orchestrator {
    pub fn new(root: PathBuf) -> Result<Self> {
        Ok(Self {
            client: Client::new(root.clone())?,
            toolbox: Toolbox::new(root),
            cheap_model: std::env::var("CRATCHIT_MODEL").unwrap_or_else(|_| DEV_MODEL_CHEAP.into()),
            sota_model: std::env::var("SCROOGE_MODEL").unwrap_or_else(|_| DEV_MODEL_SOTA.into()),
            max_rounds: 5,
        })
    }

    /// Full task loop: brief -> Scrooge plan -> Cratchit executes -> checks
    /// run -> report -> Scrooge reviews -> ... until DONE or round cap.
    ///
    /// Scrooge's context is rebuilt every round: system + brief (sliced to
    /// task-relevant files on review rounds when large) + one-line digests of
    /// earlier rounds + only the latest plan/report, so his prompt does not
    /// grow with superseded history.
    // TODO(scrooge): spec a refactor splitting the plan/execute/review phases
    // into helpers; allowed for now so the lint stays enforced elsewhere.
    #[allow(clippy::too_many_lines)]
    pub async fn run_task(&mut self, task: &str) -> Result<String> {
        // Deterministic, zero-token context gathering. The overview is the
        // one piece Cratchit writes first if missing — what the project *is*
        // can't be derived from the symbol map.
        let overview = self.ensure_overview(task).await?;
        let map = codemap::build_cached(&self.toolbox.root)?;
        let full_brief = map.brief();
        let guidance = practices::summary(task);

        let mut digests: Vec<String> = Vec::new();
        let mut last_plan: Option<String> = None;
        let mut last_report: Option<String> = None;
        let mut checks_clean: Option<bool> = None;

        for round in 1..=self.max_rounds {
            let brief = if round == 1 || full_brief.len() <= SLICE_BRIEF_OVER {
                full_brief.clone()
            } else {
                map.brief_for(&format!(
                    "{task} {} {}",
                    last_plan.as_deref().unwrap_or(""),
                    last_report.as_deref().unwrap_or("")
                ))
            };
            let mut log = vec![
                Message::text("system", SCROOGE_SYSTEM),
                Message::text(
                    "user",
                    SCROOGE_BRIEF
                        .replace("{TASK}", task)
                        .replace("{OVERVIEW}", &overview)
                        .replace("{BRIEF}", &brief)
                        .replace("{GUIDANCE}", &guidance),
                ),
            ];
            if !digests.is_empty() {
                log.push(Message::text(
                    "user",
                    format!("EARLIER ROUNDS (digest):\n{}", digests.join("\n")),
                ));
            }
            if let (Some(plan), Some(report)) = (&last_plan, &last_report) {
                log.push(Message::text("assistant", plan.clone()));
                log.push(Message::text("user", format!("CRATCHIT REPORT:\n{report}")));
            }

            let plan_msg = self
                .client
                .chat(
                    "scrooge",
                    &self.sota_model,
                    &log,
                    &[],
                    Some(SCROOGE_MAX_TOKENS),
                )
                .await?;
            let mut plan = plan_msg.content.unwrap_or_default();
            if self.client.last_finish_reason.as_deref() == Some("length") {
                // The plan was cut mid-step; drop the partial final line
                // rather than handing Cratchit half an instruction.
                if let Some(i) = plan.rfind('\n') {
                    plan.truncate(i);
                }
                eprintln!("[scrooge hit the completion cap; dropped partial final step]");
            }
            eprintln!("--- scrooge (round {round}) ---\n{plan}\n");

            // DONE is accepted only while checks are green. With red checks
            // the corrective step is fully determined, so synthesize it and
            // fall through to the normal execution path instead of burning a
            // Scrooge round; the DONE ruling is honored once checks pass.
            let done_pending = plan.trim().starts_with("DONE");
            if done_pending {
                if checks_clean != Some(false) {
                    // A verdict only exists when files changed, so this
                    // gates the overview review to tasks that wrote code.
                    if checks_clean == Some(true) {
                        self.refresh_overview(task).await;
                    }
                    return Ok(self.bill(round));
                }
                let failures = last_report
                    .as_deref()
                    .and_then(|r| r.split("CHECKS: FAILING").nth(1))
                    .unwrap_or("");
                plan = CHECK_FAILING_PLAN.replace("{FAILURES}", failures);
            }

            // One fresh Cratchit per plan step, in order. Each step is
            // verified before the next starts; a red step aborts the rest of
            // the plan (no point building on a broken base).
            let steps = plan_steps(&plan);
            let prev_round = last_report.clone();
            let mut step_reports: Vec<String> = Vec::new();
            for (i, step) in steps.iter().enumerate() {
                let n = i + 1;
                let mut ctx = prev_round.clone().unwrap_or_default();
                if !step_reports.is_empty() {
                    ctx.push_str("\nEARLIER STEPS THIS ROUND:\n");
                    ctx.push_str(&step_reports.join("\n"));
                }
                let instructions = if steps.len() > 1 {
                    STEP_INSTRUCTIONS
                        .replace("{PLAN}", &plan)
                        .replace("{N}", &n.to_string())
                        .replace("{STEP}", step)
                } else {
                    plan.clone()
                };
                eprintln!("--- cratchit step {n}/{} ---", steps.len());
                let (rep, verdict) = self
                    .execute_and_verify(
                        task,
                        &instructions,
                        (!ctx.is_empty()).then_some(ctx.as_str()),
                    )
                    .await?;
                if let Some(c) = verdict {
                    checks_clean = Some(c);
                }
                step_reports.push(format!("STEP {n}/{}:\n{rep}", steps.len()));
                if verdict == Some(false) {
                    if n < steps.len() {
                        step_reports
                            .push(format!("[steps {} onward skipped: checks failing]", n + 1));
                    }
                    break;
                }
            }
            if done_pending && checks_clean == Some(true) {
                // Scrooge already ruled DONE pending green checks.
                self.refresh_overview(task).await;
                return Ok(self.bill(round));
            }
            let report = digest_steps(&step_reports);
            eprintln!("--- cratchit report ---\n{report}\n");

            // Include the CHECKS verdict so Scrooge sees round-level
            // pass/fail history at negligible cost.
            let verdict = report
                .lines()
                .rev()
                .find(|l| l.starts_with("CHECKS:"))
                .unwrap_or("");
            digests.push(format!(
                "round {round}: {} -> {} [{}]",
                plan.lines().next().unwrap_or("").trim(),
                report.lines().next().unwrap_or("").trim(),
                verdict
            ));
            last_plan = Some(plan);
            last_report = Some(report);
        }
        Ok("round limit reached without DONE; review output above".into())
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
        let text = self
            .cratchit_execute_with(task, instructions, None, Self::overview_tools())
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
                if text.is_empty() || text.eq_ignore_ascii_case("UNCHANGED") {
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

    /// Completion banner with the cumulative token bill.
    fn bill(&self, round: usize) -> String {
        let u = &self.client.usage;
        format!(
            "task complete in {round} round(s). tokens: {} in / {} out (${:.4})",
            u.prompt_tokens, u.completion_tokens, u.cost_usd
        )
    }

    /// Execute instructions via Cratchit, then verify deterministically:
    /// clamp the report, loop mechanical check failures straight back to
    /// Cratchit, and append machine-generated CHANGED and CHECKS lines.
    /// The verdict is None when nothing changed on disk (checks skipped).
    async fn execute_and_verify(
        &mut self,
        task: &str,
        instructions: &str,
        prev_report: Option<&str>,
    ) -> Result<(String, Option<bool>)> {
        let before = worktree_changes(&self.toolbox.root);
        let mut report = clamp_report(
            &self
                .cratchit_execute(task, instructions, prev_report)
                .await?,
        );

        let after = worktree_changes(&self.toolbox.root);
        if let (Some(b), Some(a)) = (&before, &after)
            && b == a
        {
            // Investigation-only call: nothing to verify, say so in code.
            report.push_str("\nCHANGED: nothing (no file modifications)");
            return Ok((report, None));
        }

        let mut clean = false;
        let mut failures = String::new();
        for attempt in 0..=CHECK_RETRIES {
            let check = self.run_checks().await?;
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
            let prev = report.clone();
            report = clamp_report(
                &self
                    .cratchit_execute(
                        task,
                        &CHECK_FAILURE_INSTRUCTIONS.replace("{FAILURES}", &rendered),
                        Some(&prev),
                    )
                    .await?,
            );
        }

        // CHANGED reflects the final worktree, including retry fixes.
        if let Some(a) = worktree_changes(&self.toolbox.root) {
            let shown = if a.is_empty() {
                "worktree clean".to_string()
            } else {
                a
            };
            write!(report, "\nCHANGED:\n{shown}").unwrap();
        }
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

    async fn run_checks(&self) -> Result<checks::Report> {
        let root = self.toolbox.root.clone();
        tokio::task::spawn_blocking(move || checks::run(&root)).await?
    }

    /// Dispatch a pre-planned task to Cratchit (used by MCP mode, where the
    /// Claude Code conversation plays Scrooge). The report carries the same
    /// machine-generated CHANGED/CHECKS lines as the native loop, plus the
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
        let (report, _) = self.execute_and_verify(task, instructions, None).await?;
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
    /// calls (evicting stale results as the log grows) until the model
    /// replies with text, or return None when `max_iters` is exhausted.
    async fn tool_loop(
        &mut self,
        log: &mut Vec<Message>,
        defs: &[Value],
        max_iters: usize,
    ) -> Result<Option<String>> {
        for _ in 0..max_iters {
            evict_old_tool_results(log);
            let msg = self
                .client
                .chat("cratchit", &self.cheap_model, log, defs, None)
                .await?;
            log.push(msg.clone());
            let Some(calls) = msg.tool_calls.filter(|c| !c.is_empty()) else {
                return Ok(Some(msg.content.unwrap_or_default()));
            };
            for call in calls {
                let args: Value =
                    serde_json::from_str(&call.function.arguments).unwrap_or(Value::Null);
                eprintln!(
                    "  [cratchit] {}({})",
                    call.function.name,
                    arg_preview(&call.function.arguments)
                );
                let out = self.toolbox.call(&call.function.name, &args).await;
                log.push(Message::tool_result(&call.id, out));
            }
        }
        Ok(None)
    }

    async fn cratchit_execute(
        &mut self,
        task: &str,
        plan: &str,
        prev_report: Option<&str>,
    ) -> Result<String> {
        self.cratchit_execute_with(task, plan, prev_report, tools::definitions())
            .await
    }

    /// As `cratchit_execute`, but with an explicit tool set. Used by
    /// `ensure_overview` to withhold the file-writing tools: the overview is
    /// captured from Cratchit's final message and saved by the orchestrator,
    /// so a Cratchit that writes overview.md itself is only a mistake.
    async fn cratchit_execute_with(
        &mut self,
        task: &str,
        plan: &str,
        prev_report: Option<&str>,
        defs: Vec<Value>,
    ) -> Result<String> {
        // Inject context deterministically instead of having the model fetch
        // it: the code map sliced to what the plan mentions, the full
        // best-practice sections relevant to task + plan, and the previous
        // round's report so already-paid-for findings aren't re-investigated.
        let context = format!("{task} {plan}");
        let map = codemap::build_cached(&self.toolbox.root)?.brief_for(&context);
        let guidance = practices::relevant_sections(&context);
        // Loaded, never generated, here — ensure_overview() itself runs
        // through cratchit_execute, so generating would recurse.
        let overview = crate::overview::load(&self.toolbox.root)
            .map(|o| format!("\nPROJECT OVERVIEW:\n{o}\n"))
            .unwrap_or_default();
        let prev = prev_report
            .map(|r| format!("\nPREVIOUS ROUND REPORT (already verified facts):\n{r}\n"))
            .unwrap_or_default();
        let mut log = vec![
            Message::text("system", CRATCHIT_SYSTEM),
            Message::text(
                "user",
                CRATCHIT_BRIEF
                    .replace("{ROOT}", &self.toolbox.root.display().to_string())
                    .replace("{TASK}", task)
                    .replace("{OVERVIEW}", &overview)
                    .replace("{PLAN}", plan)
                    .replace("{PREV}", &prev)
                    .replace("{MAP}", &map)
                    .replace("{GUIDANCE}", &guidance),
            ),
        ];
        // Tool loop, capped to keep the cheap model from wandering.
        if let Some(text) = self.tool_loop(&mut log, &defs, 40).await? {
            return Ok(text);
        }
        // Tool budget exhausted: force a final report deterministically so
        // Scrooge never pays a round to read "hit the limit".
        log.push(Message::text("user", CRATCHIT_STOP));
        let msg = self
            .client
            .chat("cratchit", &self.cheap_model, &log, &[], None)
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
    fn plan_steps_splits_numbered_plans() {
        let plan = "1. Edit foo.rs: rename bar to baz\n2) Update callers in main.rs\n   and mcp.rs\n3. Run tests";
        let steps = plan_steps(plan);
        assert_eq!(steps.len(), 3);
        assert!(steps[0].contains("foo.rs"));
        assert!(steps[1].contains("mcp.rs"), "continuation line attached");
        // unnumbered plan = single step
        assert_eq!(plan_steps("just fix the bug").len(), 1);
    }

    #[test]
    fn eviction_stubs_only_old_large_tool_results() {
        let big = "x".repeat(EVICT_MIN_CHARS + 1);
        let mut log = vec![Message::text("system", "s"), Message::text("user", "u")];
        for _ in 0..EVICT_AFTER_TURNS + 2 {
            log.push(Message::text("assistant", "call"));
            log.push(Message::tool_result("id", big.clone()));
        }
        evict_old_tool_results(&mut log);
        let stubs = log
            .iter()
            .filter(|m| {
                m.role == "tool" && m.content.as_deref().is_some_and(|c| c.starts_with("[old"))
            })
            .count();
        assert_eq!(stubs, 2, "only results older than the keep window evicted");
        // recent results untouched
        assert!(
            log.last().unwrap().content.as_deref() == Some(big.as_str()),
            "most recent tool result must survive"
        );
    }
}
