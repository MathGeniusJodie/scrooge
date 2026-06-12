mod agents;
mod codemap;
mod mcp;
mod helpers;
mod openrouter;
mod practices;
mod tools;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

/// Scrooge: a token-miserly coding agent. Scrooge (SOTA model) plans from
/// compact briefs; Cratchit (cheap model) does the legwork with tools.
#[derive(Parser)]
#[command(name = "scrooge", version)]
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
    /// Ask a one-shot question (Cratchit only, with tools).
    Ask { question: String },
    /// Print the compact codebase brief (no LLM, free).
    Map,
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
    /// Find generic utility/helper functions in the repo (and dependencies
    /// with --deps), so agents reuse instead of reinventing. Results are
    /// cached in .scrooge/helpers.json and served by the `helpers` tool.
    Helpers {
        /// Also scan all dependencies (cargo registry, site-packages, node_modules).
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
    match cli.cmd {
        Cmd::Map => print!("{}", codemap::build(&root)?.brief()),
        Cmd::Sym { name } => print!("{}", codemap::build(&root)?.detail(&name)),
        Cmd::Callers { name } => println!("{}", codemap::build(&root)?.callers_of(&name).join("\n")),
        Cmd::Callees { name } => println!("{}", codemap::build(&root)?.callees_of(&name).join("\n")),
        Cmd::Run { task } => {
            let mut orch = agents::Orchestrator::new(root)?;
            println!("{}", orch.run_task(&task).await?);
        }
        Cmd::McpServe => mcp::Server::new(root).run().await?,
        Cmd::Practices { text } => print!("{}", practices::relevant_sections(&text)),
        Cmd::Helpers { deps, validate } => {
            let mut list = helpers::repo_helpers(&root)?;
            if deps {
                eprintln!("scanning dependencies (cargo registry / site-packages / node_modules)...");
                list.extend(helpers::dep_helpers(&root)?);
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
