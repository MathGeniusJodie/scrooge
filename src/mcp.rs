//! MCP stdio server: lets a Claude Code plugin use scrooge as a
//! token-efficiency service. Claude (the subscription model) plays Scrooge;
//! these tools give it the deterministic brief/graph/helpers machinery and
//! a way to dispatch legwork to Cratchit without seeing the raw tokens.

use anyhow::Result;
use serde_json::{Value, json};
use std::path::PathBuf;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::agents::Orchestrator;
use crate::{codemap, helpers, practices};

pub struct Server {
    root: PathBuf,
    /// Built on first cratchit call so map/graph tools work without an API key.
    orch: Option<Orchestrator>,
}

fn tool(name: &str, desc: &str, props: &Value, required: &[&str]) -> Value {
    json!({
        "name": name,
        "description": desc,
        "inputSchema": { "type": "object", "properties": props, "required": required }
    })
}

fn tool_list() -> Value {
    json!([
        tool(
            "get_brief",
            "Project overview (what the codebase is and how it hangs together) plus a compact codebase map: every file with its functions/classes/line numbers. Pass `about` (task keywords) to slice the map — only matching files in full, the rest as names. Never read source files directly — that's what Cratchit is for.",
            &json!({"about": {"type": "string", "description": "task keywords to slice the brief by, optional"}}),
            &[]
        ),
        tool(
            "run_checks",
            "Run the deterministic check suite (format, tests, lint autofix) and return the verdict. Zero LLM cost — use this to verify instead of dispatching Cratchit.",
            &json!({}),
            &[]
        ),
        tool(
            "symbol_info",
            "Signature and location of a symbol. Use callers/callees for the call graph.",
            &json!({"name": {"type": "string"}}),
            &["name"]
        ),
        tool(
            "callers",
            "Functions that call the named function.",
            &json!({"name": {"type": "string"}}),
            &["name"]
        ),
        tool(
            "callees",
            "Functions the named function calls.",
            &json!({"name": {"type": "string"}}),
            &["name"]
        ),
        tool(
            "helpers",
            "Known generic utility functions in this repo and its dependencies. Check before writing (or asking Cratchit to write) any new helper.",
            &json!({"filter": {"type": "string", "description": "substring filter, optional"}}),
            &[]
        ),
        tool(
            "best_practices",
            "Project best-practice sections matching the given topic keywords.",
            &json!({"topic": {"type": "string"}}),
            &["topic"]
        ),
        tool(
            "give_cratchit_task",
            "Dispatch a concrete task to Cratchit, a cheap agent with full tool access (files, shell, python, docs, call graph). He executes and returns a short report; a task that changes code ends with a machine-generated CHECKS (format/test/lint) verdict — trust it over his claims; mechanical check failures are already retried automatically. Use this for ALL file reading, editing, and verification instead of doing it yourself.",
            &json!({
                "task": {"type": "string", "description": "overall goal, one line"},
                "instructions": {"type": "string", "description": "numbered concrete steps naming exact files/symbols"}
            }),
            &["task", "instructions"]
        ),
        tool(
            "ask_cratchit",
            "Have Cratchit investigate a question with his tools and return a compressed answer. Use instead of reading code yourself.",
            &json!({"question": {"type": "string"}}),
            &["question"]
        ),
    ])
}

impl Server {
    pub const fn new(root: PathBuf) -> Self {
        Self { root, orch: None }
    }

    pub async fn run(&mut self) -> Result<()> {
        let mut stdin = tokio::io::stdin();
        let mut stdout = tokio::io::stdout();
        // Frame on JSON value boundaries rather than newlines: accumulate bytes
        // and let serde's streaming deserializer pull off as many complete
        // values as are buffered, regardless of how reads or whitespace split
        // them. A value straddling two reads simply waits for the next chunk.
        let mut buf: Vec<u8> = Vec::new();
        let mut chunk = [0u8; 8192];
        loop {
            let n = stdin.read(&mut chunk).await?;
            if n == 0 {
                break; // EOF
            }
            buf.extend_from_slice(&chunk[..n]);
            let mut consumed = 0;
            let mut stream = serde_json::Deserializer::from_slice(&buf).into_iter::<Value>();
            loop {
                match stream.next() {
                    Some(Ok(req)) => {
                        consumed = stream.byte_offset();
                        if let Some(resp) = self.respond(&req).await {
                            stdout.write_all(format!("{resp}\n").as_bytes()).await?;
                            stdout.flush().await?;
                        }
                    }
                    // Incomplete trailing value: keep it buffered for more bytes.
                    Some(Err(e)) if e.is_eof() => break,
                    // Malformed value: drop what's buffered and resync on the
                    // next read rather than spinning on the same bad bytes.
                    Some(Err(_)) => {
                        consumed = buf.len();
                        break;
                    }
                    None => break,
                }
            }
            drop(stream);
            buf.drain(..consumed);
        }
        Ok(())
    }

    /// Build the JSON-RPC response for one request, or `None` for a
    /// notification (no `id`), which gets no reply.
    async fn respond(&mut self, req: &Value) -> Option<Value> {
        let id = req.get("id").cloned()?;
        let method = req["method"].as_str().unwrap_or("");
        Some(match self.handle(method, &req["params"]).await {
            Ok(r) => json!({"jsonrpc": "2.0", "id": id, "result": r}),
            Err(e) => json!({"jsonrpc": "2.0", "id": id,
                "error": {"code": -32603, "message": format!("{e:#}")}}),
        })
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
        let s = |k: &str| crate::util::str_arg(args, k);
        match name {
            "get_brief" => {
                let map = codemap::build_cached(&self.root)?;
                let about = s("about");
                let brief = if about.is_empty() {
                    map.brief()
                } else {
                    map.brief_for(&about)
                };
                // The prose overview rides on top of the symbol map; it is
                // written by Cratchit on the first task if missing.
                Ok(match crate::overview::load(&self.root) {
                    Some(o) => format!("PROJECT OVERVIEW:\n{o}\n\n{brief}"),
                    None => brief,
                })
            }
            "run_checks" => {
                let root = self.root.clone();
                let report =
                    tokio::task::spawn_blocking(move || crate::checks::run(&root)).await??;
                Ok(crate::checks::render(&report))
            }
            "symbol_info" => Ok(codemap::build_cached(&self.root)?.info(&s("name"))),
            "callers" => Ok(crate::tools::or_none(
                codemap::build_cached(&self.root)?
                    .callers_of(&s("name"))
                    .join("\n"),
            )),
            "callees" => Ok(crate::tools::or_none(
                codemap::build_cached(&self.root)?
                    .callees_of(&s("name"))
                    .join("\n"),
            )),
            "best_practices" => Ok(practices::relevant_sections(
                &s("topic"),
                &codemap::build_cached(&self.root)?.languages(),
            )),
            "helpers" => helpers::filtered_listing(&self.root, &s("filter")),
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
