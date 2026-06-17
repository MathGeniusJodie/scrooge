mod accounting;
mod agents;
mod checks;
mod cleanup;
mod codemap;
mod complexity;
mod helpers;
mod mcp;
mod openrouter;
mod overview;
mod practices;
mod sandbox;
mod tools;
mod util;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

/// Scrooge: a token-miserly coding agent. Scrooge (SOTA model) plans from
/// compact briefs; Cratchit (cheap model) does the legwork with tools.
#[derive(Parser)]
#[command(
    name = "scrooge",
    version,
    long_about = "Scrooge: a token-miserly coding agent.\n\n\
        \"I'll not spend a farthing more than I must.\"\n\n\
        Scrooge (SOTA model) plans from compact briefs; Cratchit (cheap model) \
        does the legwork with tools."
)]
struct Cli {
    /// Project root to operate on.
    #[arg(short, long, default_value = ".")]
    root: PathBuf,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run a coding task through the Scrooge/Cratchit loop.
    Run { task: String },
    /// Hand a task straight to Cratchit (cheap model, full tools), skipping
    /// Scrooge's planning step. The task doubles as the instructions.
    Cratchit { task: String },
    /// Ask a one-shot question (Cratchit only, with tools).
    Ask { question: String },
    /// Print the compact codebase brief (no LLM, free).
    Map,
    /// Rank every Rust function by cognitive complexity, hottest first (no
    /// LLM, free). Shows the top 10 by default.
    Complexity {
        /// How many to show; 0 shows all.
        #[arg(long, default_value_t = 10)]
        top: usize,
    },
    /// Show signature, callers and callees of a symbol (no LLM, free).
    Sym { name: String },
    /// List callers of a function (no LLM, free).
    Callers { name: String },
    /// List callees of a function (no LLM, free).
    Callees { name: String },
    /// Serve scrooge's tools over MCP stdio (for the Claude Code plugin,
    /// where the Claude conversation plays Scrooge).
    McpServe,
    /// Print best-practice sections matching the given text (for hooks).
    Practices { text: String },
    /// Have Cratchit review .scrooge/overview.md against the current diff
    /// and rewrite it if stale (written from scratch if missing). Run by the
    /// plugin's Stop hook after a session that changed code.
    RefreshOverview {
        /// Task context for the review, optional.
        #[arg(default_value = "the changes made in this session")]
        task: String,
    },
    /// Run the post-task verification pass: autoformat, tests, lint autofix.
    /// Commands come from .scrooge/checks.toml (created with per-language
    /// defaults on first run). Exit code: 0 clean, 1 errors, 2 warnings.
    Check,
    /// Hunt down every humbug: run the full check suite (format, tests, lint).
    /// Alias for `check`. Exit code: 0 clean, 1 errors, 2 warnings.
    Humbugs,
    /// Run the checks, apply the mechanical autofixes, then hand each remaining
    /// problem to Cratchit to fix one at a time, re-verifying after each.
    Cleanup,
    /// Find generic utility/helper functions in the repo (and dependencies
    /// with --deps), so agents reuse instead of reinventing. Results are
    /// cached in .scrooge/helpers.json and served by the `helpers` tool.
    Helpers {
        /// Also scan all dependencies (cargo registry, site-packages, `node_modules`).
        #[arg(long)]
        deps: bool,
        /// Have Cratchit (cheap LLM) filter heuristic candidates and annotate purposes.
        #[arg(long)]
        validate: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let root = cli.root.canonicalize()?;
    // Every command that spawns untrusted code (Cratchit's shell/python, the
    // check suite) is gated on an enforced sandbox; the read-only map/graph
    // commands are not. One guard here keeps the list in a single place instead
    // of sprinkling the call across arms.
    if matches!(
        cli.cmd,
        Cmd::Run { .. }
            | Cmd::Cratchit { .. }
            | Cmd::Ask { .. }
            | Cmd::McpServe
            | Cmd::RefreshOverview { .. }
            | Cmd::Check
            | Cmd::Humbugs
            | Cmd::Cleanup
            | Cmd::Helpers { validate: true, .. }
    ) {
        sandbox::preflight();
    }
    match cli.cmd {
        Cmd::Map => print!("{}", codemap::build(&root)?.brief()),
        Cmd::Complexity { top } => {
            let funcs = complexity::report(&root);
            let limit = if top == 0 { funcs.len() } else { top };
            print!("{}", complexity::render(&funcs, limit));
        }
        Cmd::Sym { name } => print!("{}", codemap::build(&root)?.detail(&name)),
        Cmd::Callers { name } => {
            println!("{}", codemap::build(&root)?.callers_of(&name).join("\n"));
        }
        Cmd::Callees { name } => {
            println!("{}", codemap::build(&root)?.callees_of(&name).join("\n"));
        }
        Cmd::Run { task } => {
            if !cleanup::ensure_clean_or_prompt(&root).await? {
                return Ok(());
            }
            let mut orch = agents::Orchestrator::new(root)?;
            let out = orch.run_task(&task).await?;
            println!("{out}{}", orch.wages_footer());
        }
        Cmd::Cratchit { task } => {
            if !cleanup::ensure_clean_or_prompt(&root).await? {
                return Ok(());
            }
            let mut orch = agents::Orchestrator::new(root)?;
            let out = orch.delegate(&task, &task).await?;
            println!("{out}{}", orch.wages_footer());
        }
        Cmd::McpServe => mcp::Server::new(root).run().await?,
        Cmd::Practices { text } => print!(
            "{}",
            practices::relevant_sections(&text, &codemap::build_cached(&root)?.languages())
        ),
        Cmd::RefreshOverview { task } => {
            let mut orch = agents::Orchestrator::new(root.clone())?;
            if overview::load(&root).is_some() {
                orch.refresh_overview(&task).await;
            } else {
                orch.ensure_overview(&task).await?;
            }
        }
        Cmd::Check | Cmd::Humbugs => {
            let report = checks::run(&root)?;
            print!("{}", checks::render(&report));
            if !report.errors.is_empty() {
                std::process::exit(1);
            }
            if !report.warnings.is_empty() {
                std::process::exit(2);
            }
        }
        Cmd::Cleanup => cleanup::run(&root).await?,
        Cmd::Helpers { deps, validate } => {
            let mut list = helpers::repo_helpers(&root)?;
            if deps {
                eprintln!(
                    "scanning dependencies (cargo registry / site-packages / node_modules)..."
                );
                list.extend(helpers::dep_helpers(&root));
            }
            eprintln!("{} heuristic candidates", list.len());
            if validate {
                let mut orch = agents::Orchestrator::new(root.clone())?;
                list = orch.validate_helpers(list).await?;
                eprintln!("{} kept after cratchit validation", list.len());
            }
            helpers::save_cache(&root, &list)?;
            print!("{}", helpers::render(&list));
            eprintln!("cached to {}", helpers::cache_path(&root).display());
        }
        Cmd::Ask { question } => {
            let mut orch = agents::Orchestrator::new(root)?;
            println!("{}", orch.ask(&question).await?);
        }
    }
    Ok(())
}
