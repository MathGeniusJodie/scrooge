//! MCP stdio server: lets a Claude Code plugin use scrooge as a
//! token-efficiency service. Claude (the subscription model) plays Scrooge;
//! these tools give it the deterministic brief/graph/helpers machinery and
//! a way to dispatch legwork to Cratchit without seeing the raw tokens.

use anyhow::Result;
use serde_json::{Value, json};
use std::path::PathBuf;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::agents::Orchestrator;
use crate::{codemap, helpers, practices};

pub struct Server {
    root: PathBuf,
    /// Built on first cratchit call so map/graph tools work without an API key.
    orch: Option<Orchestrator>,
}

fn tool(name: &str, desc: &str, props: Value, required: &[&str]) -> Value {
    json!({
        "name": name,
        "description": desc,
        "inputSchema": { "type": "object", "properties": props, "required": required }
    })
}

fn tool_list() -> Value {
    json!([
        tool("get_brief",
            "Compact codebase map: every file with its functions/classes/line numbers. Call this before anything else; never read source files directly — that's what Cratchit is for.",
            json!({}), &[]),
        tool("symbol_info",
            "Signature, location, callers and callees of a symbol from the call graph.",
            json!({"name": {"type": "string"}}), &["name"]),
        tool("callers",
            "Functions that call the named function.",
            json!({"name": {"type": "string"}}), &["name"]),
        tool("callees",
            "Functions the named function calls.",
            json!({"name": {"type": "string"}}), &["name"]),
        tool("helpers",
            "Known generic utility functions in this repo and its dependencies. Check before writing (or asking Cratchit to write) any new helper.",
            json!({"filter": {"type": "string", "description": "substring filter, optional"}}), &[]),
        tool("best_practices",
            "Project best-practice sections matching the given topic keywords.",
            json!({"topic": {"type": "string"}}), &["topic"]),
        tool("give_cratchit_task",
            "Dispatch a concrete task to Cratchit, a cheap agent with full tool access (files, shell, python, wolfram, docs, call graph). He executes, verifies, and returns a report of at most 6 lines. Use this for ALL file reading, editing, and verification instead of doing it yourself.",
            json!({
                "task": {"type": "string", "description": "overall goal, one line"},
                "instructions": {"type": "string", "description": "numbered concrete steps naming exact files/symbols"}
            }), &["task", "instructions"]),
        tool("ask_cratchit",
            "Have Cratchit investigate a question with his tools and return a compressed answer. Use instead of reading code yourself.",
            json!({"question": {"type": "string"}}), &["question"]),
    ])
}

impl Server {
    pub fn new(root: PathBuf) -> Self {
        Server { root, orch: None }
    }

    pub async fn run(&mut self) -> Result<()> {
        let stdin = BufReader::new(tokio::io::stdin());
        let mut stdout = tokio::io::stdout();
        let mut lines = stdin.lines();
        while let Some(line) = lines.next_line().await? {
            if line.trim().is_empty() {
                continue;
            }
            let req: Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(_) => continue,
            };
            let id = req.get("id").cloned();
            let method = req["method"].as_str().unwrap_or("");
            // Notifications (no id) get no response.
            let Some(id) = id else { continue };
            let result = self.handle(method, &req["params"]).await;
            let resp = match result {
                Ok(r) => json!({"jsonrpc": "2.0", "id": id, "result": r}),
                Err(e) => json!({"jsonrpc": "2.0", "id": id,
                    "error": {"code": -32603, "message": format!("{e:#}")}}),
            };
            stdout
                .write_all(format!("{resp}\n").as_bytes())
                .await?;
            stdout.flush().await?;
        }
        Ok(())
    }

    async fn handle(&mut self, method: &str, params: &Value) -> Result<Value> {
        match method {
            "initialize" => Ok(json!({
                "protocolVersion": params["protocolVersion"].as_str().unwrap_or("2025-06-18"),
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "scrooge", "version": env!("CARGO_PKG_VERSION") }
            })),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({ "tools": tool_list() })),
            "tools/call" => {
                let name = params["name"].as_str().unwrap_or("");
                let args = &params["arguments"];
                let (text, is_err) = match self.call_tool(name, args).await {
                    Ok(t) => (t, false),
                    Err(e) => (format!("error: {e:#}"), true),
                };
                Ok(json!({
                    "content": [{ "type": "text", "text": text }],
                    "isError": is_err
                }))
            }
            _ => anyhow::bail!("method not supported: {method}"),
        }
    }

    fn orchestrator(&mut self) -> Result<&mut Orchestrator> {
        if self.orch.is_none() {
            self.orch = Some(Orchestrator::new(self.root.clone())?);
        }
        Ok(self.orch.as_mut().unwrap())
    }

    async fn call_tool(&mut self, name: &str, args: &Value) -> Result<String> {
        let s = |k: &str| args[k].as_str().unwrap_or("").to_string();
        match name {
            "get_brief" => Ok(codemap::build(&self.root)?.brief()),
            "symbol_info" => Ok(codemap::build(&self.root)?.detail(&s("name"))),
            "callers" => Ok(codemap::build(&self.root)?.callers_of(&s("name")).join("\n")),
            "callees" => Ok(codemap::build(&self.root)?.callees_of(&s("name")).join("\n")),
            "best_practices" => Ok(practices::relevant_sections(&s("topic"))),
            "helpers" => {
                let list = helpers::load_cache(&self.root)
                    .map(Ok)
                    .unwrap_or_else(|| helpers::repo_helpers(&self.root))?;
                let filter = s("filter").to_lowercase();
                let filtered: Vec<_> = list
                    .into_iter()
                    .filter(|h| {
                        filter.is_empty()
                            || h.name.to_lowercase().contains(&filter)
                            || h.purpose
                                .as_deref()
                                .unwrap_or("")
                                .to_lowercase()
                                .contains(&filter)
                    })
                    .collect();
                Ok(helpers::render(&filtered))
            }
            "give_cratchit_task" => {
                let (task, instructions) = (s("task"), s("instructions"));
                self.orchestrator()?.delegate(&task, &instructions).await
            }
            "ask_cratchit" => {
                let q = s("question");
                self.orchestrator()?.ask(&q).await
            }
            _ => anyhow::bail!("unknown tool {name}"),
        }
    }
}
