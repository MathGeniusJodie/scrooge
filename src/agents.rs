//! Orchestration. Scrooge (expensive SOTA model) only ever sees compact
//! briefs and terse reports — he plans, reviews, and decides. Cratchit
//! (cheap model) does all the token-heavy tool work and must compress
//! everything he sends upstairs.

use anyhow::Result;
use serde_json::Value;
use std::path::PathBuf;

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
When given a progress report, reply either with corrections/next steps (same format) \
or the single word DONE if the task is complete and verified. No preamble, no prose.";

const CRATCHIT_SYSTEM: &str = "\
You are Cratchit, a diligent coding agent executing a plan written by Scrooge, \
your demanding boss whose time is very valuable. Rules:\n\
1. Use tools for everything. NEVER do arithmetic or logic in your head when the \
python or wolfram tool can do it deterministically.\n\
2. Call code_map / symbol_info / callers before reading files; read line ranges, \
not whole files.\n\
3. Call query_docs before using any API you are not 100% sure about.\n\
4. Call best_practices with topic keywords before writing code.\n\
5. Verify your work (compile, run, test) before reporting.\n\
6. Your final message is a report for Scrooge: maximum 6 lines, only facts he \
needs (what changed, file:line, verification result, blockers). No pleasantries, \
no restating the plan, no code unless a decision depends on it.";

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
            client: Client::new()?,
            toolbox: Toolbox::new(root),
            cheap_model: std::env::var("CRATCHIT_MODEL").unwrap_or(DEV_MODEL_CHEAP.into()),
            sota_model: std::env::var("SCROOGE_MODEL").unwrap_or(DEV_MODEL_SOTA.into()),
            max_rounds: 5,
        })
    }

    /// Full task loop: brief -> Scrooge plan -> Cratchit executes -> report ->
    /// Scrooge reviews -> ... until DONE or round cap.
    pub async fn run_task(&mut self, task: &str) -> Result<String> {
        // Deterministic, zero-token context gathering.
        let brief = codemap::build(&self.toolbox.root)?.brief();
        let guidance = practices::relevant_sections(task);

        let mut scrooge_log = vec![
            Message::text("system", SCROOGE_SYSTEM),
            Message::text(
                "user",
                format!("TASK: {task}\n\nCODEBASE BRIEF:\n{brief}\nRELEVANT GUIDANCE:\n{guidance}"),
            ),
        ];

        for round in 1..=self.max_rounds {
            let plan_msg = self
                .client
                .chat(&self.sota_model, &scrooge_log, &[])
                .await?;
            let plan = plan_msg.content.clone().unwrap_or_default();
            scrooge_log.push(plan_msg);
            eprintln!("--- scrooge (round {round}) ---\n{plan}\n");

            if plan.trim() == "DONE" || plan.trim().starts_with("DONE") {
                let u = &self.client.usage;
                return Ok(format!(
                    "task complete in {round} round(s). tokens: {} in / {} out",
                    u.prompt_tokens, u.completion_tokens
                ));
            }

            let report = self.cratchit_execute(task, &plan).await?;
            eprintln!("--- cratchit report ---\n{report}\n");
            scrooge_log.push(Message::text("user", format!("CRATCHIT REPORT:\n{report}")));
        }
        Ok("round limit reached without DONE; review output above".into())
    }

    /// Dispatch a pre-planned task to Cratchit (used by MCP mode, where the
    /// Claude Code conversation plays Scrooge). Appends the token bill.
    pub async fn delegate(&mut self, task: &str, instructions: &str) -> Result<String> {
        let report = self.cratchit_execute(task, instructions).await?;
        let u = &self.client.usage;
        Ok(format!(
            "{report}\n[cratchit tokens: {} in / {} out]",
            u.prompt_tokens, u.completion_tokens
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
    /// He may read the source to check a body when the signature is unclear.
    pub async fn validate_helpers(&mut self, candidates: Vec<Helper>) -> Result<Vec<Helper>> {
        const BATCH: usize = 50;
        let defs = tools::definitions();
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
                let msg = self.client.chat(&self.cheap_model, &log, &defs).await?;
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
        let mut log = vec![
            Message::text("system", CRATCHIT_SYSTEM),
            Message::text(
                "user",
                format!(
                    "PROJECT ROOT: {}\nTASK: {task}\n\nSCROOGE'S INSTRUCTIONS:\n{plan}",
                    self.toolbox.root.display()
                ),
            ),
        ];
        // Tool loop, capped to keep the cheap model from wandering.
        for _ in 0..40 {
            let msg = self.client.chat(&self.cheap_model, &log, &defs).await?;
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
