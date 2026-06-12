//! Orchestration. Scrooge (expensive SOTA model) only ever sees compact
//! briefs and terse reports — he plans, reviews, and decides. Cratchit
//! (cheap model) does all the token-heavy tool work and must compress
//! everything he sends upstairs.
//!
//! Everything that can be decided deterministically is: the code map and
//! relevant guidance are injected rather than fetched by the model, checks
//! run after every execution and their verdict is appended to the report,
//! mechanical check failures loop straight back to Cratchit without burning
//! a Scrooge round, and DONE is only accepted while checks are green.

use anyhow::Result;
use serde_json::Value;
use std::path::PathBuf;

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
Every Cratchit report ends with a machine-generated CHECKS line (format/test/lint run \
deterministically) — trust it over Cratchit's own claims. When given a progress report, \
reply either with corrections/next steps (same format) or the single word DONE if the \
task is complete and CHECKS is clean. No preamble, no prose.";

const CRATCHIT_SYSTEM: &str = "\
You are Cratchit, a diligent coding agent executing a plan written by Scrooge, \
your demanding boss whose time is very valuable. A code map of the relevant files \
and the applicable best-practice guidance are already included in your briefing. \
Rules:\n\
1. Use tools for everything. NEVER do arithmetic or logic in your head when the \
python or wolfram tool can do it deterministically.\n\
2. Use the included code map; call symbol_info / callers before changing any \
signature; read line ranges, not whole files.\n\
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

/// Completion cap for Scrooge: plans are numbered one-liners, so anything
/// past this is waste on the expensive model.
const SCROOGE_MAX_TOKENS: u32 = 700;

/// How many times a failing check report is routed straight back to Cratchit
/// before the failure is escalated to Scrooge.
const CHECK_RETRIES: usize = 2;

pub struct Orchestrator {
    client: Client,
    toolbox: Toolbox,
    cheap_model: String,
    sota_model: String,
    max_rounds: usize,
}

impl Orchestrator {
    pub fn new(root: PathBuf) -> Result<Self> {
        Ok(Orchestrator {
            client: Client::new(root.clone())?,
            toolbox: Toolbox::new(root),
            cheap_model: std::env::var("CRATCHIT_MODEL").unwrap_or(DEV_MODEL_CHEAP.into()),
            sota_model: std::env::var("SCROOGE_MODEL").unwrap_or(DEV_MODEL_SOTA.into()),
            max_rounds: 5,
        })
    }

    /// Full task loop: brief -> Scrooge plan -> Cratchit executes -> checks
    /// run -> report -> Scrooge reviews -> ... until DONE or round cap.
    ///
    /// Scrooge's context is rebuilt every round: system + original brief +
    /// one-line digests of earlier rounds + only the latest plan/report, so
    /// his prompt does not grow with superseded history.
    pub async fn run_task(&mut self, task: &str) -> Result<String> {
        // Deterministic, zero-token context gathering.
        let brief = codemap::build(&self.toolbox.root)?.brief();
        let guidance = practices::summary(task);

        let intro = format!(
            "TASK: {task}\n\nCODEBASE BRIEF:\n{brief}\nKEY GUIDANCE:\n{guidance}"
        );
        let mut digests: Vec<String> = Vec::new();
        let mut last_plan: Option<String> = None;
        let mut last_report: Option<String> = None;
        let mut checks_clean: Option<bool> = None;

        for round in 1..=self.max_rounds {
            let mut log = vec![
                Message::text("system", SCROOGE_SYSTEM),
                Message::text("user", intro.clone()),
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
            let plan = plan_msg.content.unwrap_or_default();
            eprintln!("--- scrooge (round {round}) ---\n{plan}\n");

            if plan.trim().starts_with("DONE") {
                if checks_clean == Some(false) {
                    // DONE is not Scrooge's to grant while checks are red.
                    digests.push(format!("round {round}: DONE rejected (checks failing)"));
                    last_plan = Some(plan);
                    last_report = Some(
                        "DONE not accepted: the deterministic checks are still failing \
                         (see previous CHECKS output). Issue corrective steps."
                            .into(),
                    );
                    continue;
                }
                let u = &self.client.usage;
                return Ok(format!(
                    "task complete in {round} round(s). tokens: {} in / {} out",
                    u.prompt_tokens, u.completion_tokens
                ));
            }

            let mut report = self.cratchit_execute(task, &plan).await?;
            // Deterministic verification; mechanical failures loop straight
            // back to Cratchit instead of burning a Scrooge round.
            let mut clean = false;
            for attempt in 0..=CHECK_RETRIES {
                let check = self.run_checks().await?;
                if check.errors.is_empty() && check.warnings.is_empty() {
                    clean = true;
                    break;
                }
                let rendered = checks::render(&check);
                if attempt == CHECK_RETRIES {
                    report.push_str(&format!("\nCHECKS: FAILING\n{rendered}"));
                    break;
                }
                eprintln!("--- checks failed (cratchit retry {}) ---\n{rendered}", attempt + 1);
                report = self
                    .cratchit_execute(
                        task,
                        &format!(
                            "The deterministic check suite failed after your last \
                             changes. Fix the failures below, then verify.\n{rendered}"
                        ),
                    )
                    .await?;
            }
            if clean {
                report.push_str("\nCHECKS: clean");
            }
            checks_clean = Some(clean);
            eprintln!("--- cratchit report ---\n{report}\n");

            digests.push(format!(
                "round {round}: {} -> {}",
                plan.lines().next().unwrap_or("").trim(),
                report.lines().next().unwrap_or("").trim()
            ));
            last_plan = Some(plan);
            last_report = Some(report);
        }
        Ok("round limit reached without DONE; review output above".into())
    }

    async fn run_checks(&self) -> Result<checks::Report> {
        let root = self.toolbox.root.clone();
        tokio::task::spawn_blocking(move || checks::run(&root)).await?
    }

    /// Dispatch a pre-planned task to Cratchit (used by MCP mode, where the
    /// Claude Code conversation plays Scrooge). Appends the token bill for
    /// this call only (usage accumulates across the server's lifetime).
    pub async fn delegate(&mut self, task: &str, instructions: &str) -> Result<String> {
        let before = (
            self.client.usage.prompt_tokens,
            self.client.usage.completion_tokens,
        );
        let report = self.cratchit_execute(task, instructions).await?;
        let u = &self.client.usage;
        Ok(format!(
            "{report}\n[cratchit tokens: {} in / {} out]",
            u.prompt_tokens - before.0,
            u.completion_tokens - before.1
        ))
    }

    /// One-shot question for the cheap model with full tool access.
    pub async fn ask(&mut self, question: &str) -> Result<String> {
        self.cratchit_execute(
            question,
            "Answer the question directly; investigate with tools first.",
        )
        .await
    }

    /// Cratchit reviews heuristic helper candidates and keeps only genuinely
    /// generic, reusable utilities, annotating each with a purpose line.
    /// He may read the source to check a body when the signature is unclear —
    /// read_file is the only tool he gets for this.
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
                Message::text(
                    "system",
                    "You are Cratchit, validating utility-function candidates found by \
                     static heuristics. Keep only genuinely GENERIC, reusable helpers — \
                     things any module might want (string/path/collection/number/format \
                     utilities). Reject anything domain-specific, stateful, or trivial \
                     wrappers. If a signature is ambiguous, read the function body with \
                     read_file (use the given file path and line range) before deciding. \
                     Output one line per keeper, exactly: KEEP <name> | <purpose, max 8 words> \
                     Nothing else. No explanations.",
                ),
                Message::text("user", format!("CANDIDATES:\n{listing}")),
            ];
            for _ in 0..20 {
                let msg = self
                    .client
                    .chat("cratchit", &self.cheap_model, &log, &defs, None)
                    .await?;
                log.push(msg.clone());
                if let Some(calls) = msg.tool_calls.filter(|c| !c.is_empty()) {
                    for call in calls {
                        let args: Value =
                            serde_json::from_str(&call.function.arguments).unwrap_or(Value::Null);
                        let out = self.toolbox.call(&call.function.name, &args).await;
                        log.push(Message::tool_result(&call.id, out));
                    }
                    continue;
                }
                let text = msg.content.unwrap_or_default();
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
                break;
            }
        }
        Ok(kept)
    }

    async fn cratchit_execute(&mut self, task: &str, plan: &str) -> Result<String> {
        let defs = tools::definitions();
        // Inject context deterministically instead of having the model fetch
        // it: the code map sliced to what the plan mentions, plus the full
        // best-practice sections relevant to task + plan.
        let context = format!("{task} {plan}");
        let map = codemap::build(&self.toolbox.root)?.brief_for(&context);
        let guidance = practices::relevant_sections(&context);
        let mut log = vec![
            Message::text("system", CRATCHIT_SYSTEM),
            Message::text(
                "user",
                format!(
                    "PROJECT ROOT: {}\nTASK: {task}\n\nSCROOGE'S INSTRUCTIONS:\n{plan}\n\n\
                     CODE MAP (files mentioned in the instructions shown in full):\n{map}\n\
                     GUIDANCE:\n{guidance}",
                    self.toolbox.root.display()
                ),
            ),
        ];
        // Tool loop, capped to keep the cheap model from wandering.
        for _ in 0..40 {
            let msg = self
                .client
                .chat("cratchit", &self.cheap_model, &log, &defs, None)
                .await?;
            log.push(msg.clone());
            let Some(calls) = msg.tool_calls.filter(|c| !c.is_empty()) else {
                return Ok(msg.content.unwrap_or_default());
            };
            for call in calls {
                let args: Value =
                    serde_json::from_str(&call.function.arguments).unwrap_or(Value::Null);
                eprintln!(
                    "  [cratchit] {}({})",
                    call.function.name, call.function.arguments
                );
                let out = self.toolbox.call(&call.function.name, &args).await;
                log.push(Message::tool_result(&call.id, out));
            }
        }
        Ok("cratchit hit the tool-call limit without finishing".into())
    }
}
