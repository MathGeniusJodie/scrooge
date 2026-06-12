//! Cratchit's tool belt. Philosophy: never trust the LLM with anything a
//! deterministic program can do — math goes to python/wolframscript, code
//! questions go to the code map, facts go to documentation.

use anyhow::Result;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use tokio::process::Command;

use crate::codemap;
use crate::practices;

pub struct Toolbox {
    pub root: PathBuf,
}

fn tool(name: &str, desc: &str, params: Value) -> Value {
    json!({
        "type": "function",
        "function": { "name": name, "description": desc, "parameters": params }
    })
}

fn obj(props: Value, required: &[&str]) -> Value {
    json!({ "type": "object", "properties": props, "required": required })
}

pub fn definitions() -> Vec<Value> {
    vec![
        tool(
            "read_file",
            "Read a file. Prefer line ranges over whole files to save tokens.",
            obj(
                json!({
                    "path": {"type": "string"},
                    "start_line": {"type": "integer", "description": "1-based, optional"},
                    "end_line": {"type": "integer", "description": "inclusive, optional"}
                }),
                &["path"],
            ),
        ),
        tool(
            "write_file",
            "Create or overwrite a file with the given content.",
            obj(json!({"path": {"type": "string"}, "content": {"type": "string"}}), &["path", "content"]),
        ),
        tool(
            "edit_file",
            "Replace an exact string in a file once. Fails if not found or ambiguous.",
            obj(
                json!({"path": {"type": "string"}, "find": {"type": "string"}, "replace": {"type": "string"}}),
                &["path", "find", "replace"],
            ),
        ),
        tool(
            "shell",
            "Run a shell command in the project root (tests, builds, grep, etc.). 60s timeout.",
            obj(json!({"command": {"type": "string"}}), &["command"]),
        ),
        tool(
            "python",
            "Evaluate Python code for any math, counting, data transformation, or verification. ALWAYS use this instead of doing arithmetic yourself. Prints stdout.",
            obj(json!({"code": {"type": "string"}}), &["code"]),
        ),
        tool(
            "wolfram",
            "Evaluate a WolframScript expression for symbolic math, calculus, equation solving. ALWAYS use this instead of reasoning through math yourself.",
            obj(json!({"expression": {"type": "string"}}), &["expression"]),
        ),
        tool(
            "code_map",
            "Compact symbol map of the codebase (files, functions, classes, line numbers). Cheap; call before reading files.",
            obj(json!({}), &[]),
        ),
        tool(
            "symbol_info",
            "Signature, location, callers and callees of a symbol from the call graph.",
            obj(json!({"name": {"type": "string"}}), &["name"]),
        ),
        tool(
            "callers",
            "List functions that call the named function.",
            obj(json!({"name": {"type": "string"}}), &["name"]),
        ),
        tool(
            "callees",
            "List functions the named function calls.",
            obj(json!({"name": {"type": "string"}}), &["name"]),
        ),
        tool(
            "query_docs",
            "Query official documentation. lang=python uses pydoc; lang=rust fetches docs.rs; lang=js fetches MDN. Always check docs before using an unfamiliar API.",
            obj(
                json!({"lang": {"type": "string", "enum": ["python", "rust", "js"]}, "query": {"type": "string", "description": "module/symbol, e.g. 'os.path.join', 'serde_json', 'Array.prototype.map'"}}),
                &["lang", "query"],
            ),
        ),
        tool(
            "helpers",
            "List known generic utility functions from this repo AND its dependencies. ALWAYS check here before writing a new helper — do not reinvent the wheel. Optional filter narrows by substring.",
            obj(json!({"filter": {"type": "string", "description": "substring to match against name/purpose, optional"}}), &[]),
        ),
        tool(
            "best_practices",
            "Fetch only the best-practice sections relevant to the given topic keywords.",
            obj(json!({"topic": {"type": "string"}}), &["topic"]),
        ),
    ]
}

const MAX_OUTPUT: usize = 8000;

fn truncate(mut s: String) -> String {
    if s.len() > MAX_OUTPUT {
        s.truncate(MAX_OUTPUT);
        s.push_str("\n[truncated]");
    }
    s
}

impl Toolbox {
    pub fn new(root: PathBuf) -> Self {
        Toolbox { root }
    }

    fn resolve(&self, p: &str) -> PathBuf {
        let path = Path::new(p);
        if path.is_absolute() { path.to_path_buf() } else { self.root.join(path) }
    }

    pub async fn call(&self, name: &str, args: &Value) -> String {
        let result = self.dispatch(name, args).await;
        truncate(match result {
            Ok(s) => s,
            Err(e) => format!("error: {e:#}"),
        })
    }

    async fn dispatch(&self, name: &str, args: &Value) -> Result<String> {
        let s = |k: &str| args[k].as_str().unwrap_or("").to_string();
        match name {
            "read_file" => {
                let content = std::fs::read_to_string(self.resolve(&s("path")))?;
                let start = args["start_line"].as_u64().map(|n| n as usize);
                let end = args["end_line"].as_u64().map(|n| n as usize);
                Ok(match (start, end) {
                    (Some(a), b) => {
                        let b = b.unwrap_or(usize::MAX);
                        content
                            .lines()
                            .enumerate()
                            .filter(|(i, _)| *i + 1 >= a && *i + 1 <= b)
                            .map(|(i, l)| format!("{}|{l}", i + 1))
                            .collect::<Vec<_>>()
                            .join("\n")
                    }
                    _ => content,
                })
            }
            "write_file" => {
                let path = self.resolve(&s("path"));
                if let Some(dir) = path.parent() {
                    std::fs::create_dir_all(dir)?;
                }
                std::fs::write(&path, s("content"))?;
                Ok(format!("wrote {}", path.display()))
            }
            "edit_file" => {
                let path = self.resolve(&s("path"));
                let content = std::fs::read_to_string(&path)?;
                let find = s("find");
                match content.matches(&find).count() {
                    0 => anyhow::bail!("string not found in {}", path.display()),
                    1 => {
                        std::fs::write(&path, content.replacen(&find, &s("replace"), 1))?;
                        Ok("edited".into())
                    }
                    n => anyhow::bail!("string appears {n} times; provide more context"),
                }
            }
            "shell" => self.run("bash", &["-c", &s("command")]).await,
            "python" => self.run("python3", &["-c", &s("code")]).await,
            "wolfram" => self.run("wolframscript", &["-code", &s("expression")]).await,
            "code_map" => Ok(codemap::build(&self.root)?.brief()),
            "symbol_info" => Ok(codemap::build(&self.root)?.detail(&s("name"))),
            "callers" => {
                let m = codemap::build(&self.root)?;
                Ok(m.callers_of(&s("name")).join("\n"))
            }
            "callees" => {
                let m = codemap::build(&self.root)?;
                Ok(m.callees_of(&s("name")).join("\n"))
            }
            "helpers" => {
                // Prefer the validated cache; fall back to a repo-only scan
                // (full dependency scans are done via `scrooge helpers --deps`).
                let list = crate::helpers::load_cache(&self.root)
                    .map(Ok)
                    .unwrap_or_else(|| crate::helpers::repo_helpers(&self.root))?;
                let filter = s("filter").to_lowercase();
                let filtered: Vec<_> = list
                    .into_iter()
                    .filter(|h| {
                        filter.is_empty()
                            || h.name.to_lowercase().contains(&filter)
                            || h.purpose.as_deref().unwrap_or("").to_lowercase().contains(&filter)
                    })
                    .collect();
                Ok(crate::helpers::render(&filtered))
            }
            "query_docs" => self.query_docs(&s("lang"), &s("query")).await,
            "best_practices" => Ok(practices::relevant_sections(&s("topic"))),
            _ => anyhow::bail!("unknown tool {name}"),
        }
    }

    async fn run(&self, prog: &str, args: &[&str]) -> Result<String> {
        let out = tokio::time::timeout(
            std::time::Duration::from_secs(60),
            Command::new(prog).args(args).current_dir(&self.root).output(),
        )
        .await
        .map_err(|_| anyhow::anyhow!("timed out after 60s"))??;
        let mut s = String::from_utf8_lossy(&out.stdout).to_string();
        let err = String::from_utf8_lossy(&out.stderr);
        if !err.trim().is_empty() {
            s.push_str("\n[stderr] ");
            s.push_str(err.trim());
        }
        if !out.status.success() {
            s.push_str(&format!("\n[exit {}]", out.status.code().unwrap_or(-1)));
        }
        Ok(s)
    }

    async fn query_docs(&self, lang: &str, query: &str) -> Result<String> {
        match lang {
            "python" => self.run("python3", &["-m", "pydoc", query]).await,
            "rust" => {
                // docs.rs serves a text-friendly page per crate/item; strip tags crudely.
                let krate = query.split("::").next().unwrap_or(query);
                let item = query.replace("::", "/");
                let url = if krate == query {
                    format!("https://docs.rs/{krate}/latest/{krate}/")
                } else {
                    format!("https://docs.rs/{krate}/latest/{item}/")
                };
                fetch_text(&url).await
            }
            "js" => {
                let q = query.replace(' ', "+");
                fetch_text(&format!(
                    "https://developer.mozilla.org/api/v1/search?q={q}&locale=en-US"
                ))
                .await
            }
            _ => anyhow::bail!("unsupported lang {lang}"),
        }
    }
}

async fn fetch_text(url: &str) -> Result<String> {
    let body = reqwest::Client::new()
        .get(url)
        .header("User-Agent", "scrooge-agent")
        .send()
        .await?
        .text()
        .await?;
    // Crude HTML -> text: drop scripts/styles/tags, collapse whitespace.
    let no_script = regex::Regex::new(r"(?s)<(script|style)[^>]*>.*?</(script|style)>")?
        .replace_all(&body, " ");
    let no_tags = regex::Regex::new(r"<[^>]+>")?.replace_all(&no_script, " ");
    let collapsed = regex::Regex::new(r"\s+")?.replace_all(&no_tags, " ");
    Ok(collapsed.trim().to_string())
}
