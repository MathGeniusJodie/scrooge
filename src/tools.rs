//! Cratchit's tool belt. Philosophy: never trust the LLM with anything a
//! deterministic program can do — math goes to python/wolframscript, code
//! questions go to the code map, facts go to documentation.

use anyhow::Result;
use serde_json::{Value, json};
use std::fmt::Write;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use tokio::process::Command;

use crate::codemap;

/// Process-wide `reqwest` client for the web/doc tools. Built once and reused so
/// every lookup shares one connection pool instead of standing up a fresh client
/// (and pool) per call.
fn http() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(reqwest::Client::new)
}

pub struct Toolbox {
    pub root: PathBuf,
}

fn tool(name: &str, desc: &str, params: &Value) -> Value {
    json!({
        "type": "function",
        "function": { "name": name, "description": desc, "parameters": params }
    })
}

fn obj(props: &Value, required: &[&str]) -> Value {
    json!({ "type": "object", "properties": props, "required": required })
}

/// Definitions ride on every model call, so they are kept terse. `code_map`
/// and `best_practices` are not listed (and have no dispatch handler): both are
/// injected into Cratchit's briefing deterministically.
// A flat data table of tool schemas — long by nature, not a refactor target.
#[allow(clippy::too_many_lines)]
pub fn definitions() -> Vec<Value> {
    vec![
        tool(
            "read_file",
            "Read a file. Every line is returned tagged `LINE#HASH:content` (e.g. `12#MQ:    let x = 1;`); pass those LINE#HASH anchors to edit_file. Files over 2000 lines return an outline instead; pass start_line/end_line (max 2000 lines per call). Prefer narrow ranges — reading whole large files burns context.",
            &obj(
                &json!({
                    "path": {"type": "string"},
                    "start_line": {"type": "integer", "description": "1-based, optional"},
                    "end_line": {"type": "integer", "description": "inclusive, optional"}
                }),
                &["path"],
            ),
        ),
        tool(
            "write_file",
            "Create or overwrite a file. Result includes a syntax verdict.",
            &obj(
                &json!({"path": {"type": "string"}, "content": {"type": "string"}}),
                &["path", "content"],
            ),
        ),
        tool(
            "edit_file",
            "Apply hash-anchored edits to a file, all-or-nothing — batch related edits into ONE call. `pos`/`end` are `LINE#HASH` anchors from read_file output that locate (and staleness-check) the target lines; `lines` is the literal new content that goes there (no `LINE#HASH:` prefixes). A stale anchor (the file changed since you read it) is rejected with fresh anchors to retry — never guess a hash. Returns fresh anchors for the changed region and a syntax verdict — no need to re-read. Ops: replace (one line at pos, or the pos..end range; `lines` is the replacement, empty to delete), append (lines after pos, or at EOF if pos omitted), prepend (lines before pos, or at BOF if omitted).",
            &obj(
                &json!({
                    "path": {"type": "string"},
                    "edits": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "op": {"type": "string", "enum": ["replace", "append", "prepend"]},
                                "pos": {"type": "string", "description": "anchor LINE#HASH, e.g. '12#MQ'"},
                                "end": {"type": "string", "description": "range end anchor for replace, optional"},
                                "lines": {"type": "array", "items": {"type": "string"}, "description": "literal new lines to write at the anchor"}
                            },
                            "required": ["op", "lines"]
                        }
                    }
                }),
                &["path", "edits"],
            ),
        ),
        tool(
            "read_symbol",
            "Read a whole function/method/struct's source by its code-map name — the parser supplies the exact span, so no line guessing. Prefer this over a read_file range when you want one definition. Optional path narrows when the name is ambiguous.",
            &obj(
                &json!({
                    "name": {"type": "string", "description": "symbol name from the code map, e.g. 'parse' or 'Client.chat'"},
                    "path": {"type": "string", "description": "file filter when ambiguous, optional"}
                }),
                &["name"],
            ),
        ),
        tool(
            "replace_symbol",
            "Replace an entire function/method/struct by its code-map name with new source — prefer this over edit_file when rewriting a whole definition; no find string needed, the span comes from the parser. Returns a syntax verdict. Optional path narrows when the name is ambiguous.",
            &obj(
                &json!({
                    "name": {"type": "string", "description": "symbol name from the code map, e.g. 'parse' or 'Client.chat'"},
                    "new_source": {"type": "string", "description": "full replacement definition"},
                    "path": {"type": "string", "description": "file filter when ambiguous, optional"}
                }),
                &["name", "new_source"],
            ),
        ),
        tool(
            "shell",
            "Run a shell command in the project root (tests, builds, grep). 60s timeout.",
            &obj(&json!({"command": {"type": "string"}}), &["command"]),
        ),
        tool(
            "python",
            "Run Python code; use for ALL math, counting, logic and data transformation — never work a result out in your head. Use sympy for symbolic math, calculus and equation solving. Prints stdout.",
            &obj(&json!({"code": {"type": "string"}}), &["code"]),
        ),
        tool(
            "symbol_info",
            "Signature and location of a symbol. Use callers/callees for the call graph.",
            &obj(&json!({"name": {"type": "string"}}), &["name"]),
        ),
        tool(
            "callers",
            "Functions that call the named function — check before changing its signature to see the change's blast radius.",
            &obj(&json!({"name": {"type": "string"}}), &["name"]),
        ),
        tool(
            "callees",
            "Functions the named function calls.",
            &obj(&json!({"name": {"type": "string"}}), &["name"]),
        ),
        tool(
            "library_docs",
            "Fetch upstream docs for a third-party library/dependency (NOT the language or its stdlib — you already know those). If the library is a project dependency the docs are pinned to the installed version; otherwise the latest published version is fetched, so you can also evaluate a crate before adding it. rust=docs.rs, js=unpkg README, python=PyPI.",
            &obj(
                &json!({"lang": {"type": "string", "enum": ["python", "rust", "js"]}, "library": {"type": "string", "description": "package/crate name, e.g. 'tokio', 'axum', 'requests'"}, "item": {"type": "string", "description": "optional path/symbol to narrow within the library, e.g. 'fs::read_to_string', 'Router::route'"}}),
                &["lang", "library"],
            ),
        ),
        tool(
            "helpers",
            "Generic utility functions known in this repo and its dependencies; check before writing a new helper. Optional substring filter.",
            &obj(
                &json!({"filter": {"type": "string", "description": "substring filter, optional"}}),
                &[],
            ),
        ),
        web_answer_tool(),
        tool(
            "add_dependency",
            "Add a dependency at its latest published version (cargo add / pip install -U / npm install @latest). Never write version numbers from memory.",
            &obj(
                &json!({"lang": {"type": "string", "enum": ["python", "rust", "js"]}, "package": {"type": "string"}, "dev": {"type": "boolean", "description": "dev-dependency, optional"}}),
                &["lang", "package"],
            ),
        ),
    ]
}

/// Scrooge's tools. `delegate_to_cratchit` is the workhorse; `symbol_info` /
/// `callers` / `callees` are free, deterministic call-graph lookups answered
/// locally (no Cratchit round); `web_answer` is rate-limited to
/// `SCROOGE_WEB_LOOKUPS` uses. The list is stable across a task so the
/// provider's KV cache survives every turn.
pub fn scrooge_definitions() -> Vec<Value> {
    vec![
        tool(
            "delegate_to_cratchit",
            "Dispatch one step to Cratchit for execution. Cratchit has full tool access \
             (files, shell, python, docs, call graph); when you need file-level \
             facts before planning a \
             change, spend one call purely to investigate — tell him to read the relevant \
             files and report, changing nothing. Instructions must be standalone and \
             imperative, naming exact files/symbols. A step that changes code returns a \
             report ending with a CHECKS verdict (a compile check). Call ONCE per turn.",
            &obj(
                &json!({"instructions": {"type": "string", "description": "standalone imperative step for Cratchit to execute and verify"}}),
                &["instructions"],
            ),
        ),
        tool(
            "symbol_info",
            "signature and location of a symbol. Use callers/callees for the call graph.",
            &obj(&json!({"name": {"type": "string"}}), &["name"]),
        ),
        tool(
            "callers",
            "the functions that call the named function — its blast radius before a signature change.",
            &obj(&json!({"name": {"type": "string"}}), &["name"]),
        ),
        tool(
            "callees",
            "the functions the named function calls.",
            &obj(&json!({"name": {"type": "string"}}), &["name"]),
        ),
        web_answer_tool(),
    ]
}

/// The single web tool, shared by Cratchit and Scrooge: one AI-summarized
/// answer (Brave summarizer), used both to pick an external library and to
/// settle a specific API/version detail. Identical in both tool lists so the
/// provider's KV cache survives, and so there is one web affordance to reason
/// about rather than a search-vs-answer split.
fn web_answer_tool() -> Value {
    tool(
        "web_answer",
        "Get one concise AI-summarized answer from the web (not a link list). Use SPARINGLY — to choose an external library for a need, or settle a specific API/version/implementation detail you are unsure of; most tasks need zero calls. Not for facts about code in this repo. Call before add_dependency when picking a library.",
        &obj(
            &json!({"query": {"type": "string", "description": "a focused question, e.g. 'best maintained rust crate for TOML parsing 2026' or 'does tokio::fs::read_to_string exist'"}}),
            &["query"],
        ),
    )
}

const MAX_OUTPUT: usize = 8000;

/// `read_file` can legitimately return far more than a shell command (a whole
/// file up to `MAX_WHOLE_FILE_LINES`), so it gets a roomier truncation budget;
/// everything else stays capped at `MAX_OUTPUT`.
const READ_FILE_MAX_CHARS: usize = 120_000;

/// Whole-file reads above this are refused with an outline instead, so the
/// "read line ranges" rule is enforced in code rather than pleaded in prompts.
/// Matches the file-length lint threshold in `checks.rs` — files kept under it
/// are readable whole; anything larger should be split.
const MAX_WHOLE_FILE_LINES: usize = 2000;

/// Largest line range a single `read_file` call returns.
const MAX_RANGE_LINES: usize = 2000;

/// Keep head AND tail when truncating: compiler errors and test failures
/// land at the end of long outputs, which a head-only cut would discard.
/// `max` is the per-tool budget (`read_file` gets a bigger one than shell).
fn truncate(s: String, max: usize) -> String {
    if s.len() <= max {
        return s;
    }
    // Keep the head of the output (where errors usually surface) larger than
    // the tail; a quarter/three-quarters split reads well after truncation.
    const HEAD_DIVISOR: usize = 4;
    let head = max / HEAD_DIVISOR;
    let head_end = crate::util::floor_char_boundary(&s, head);
    let tail_start = crate::util::ceil_char_boundary(&s, s.len() - (max - head));
    format!(
        "{}\n[... {} chars truncated ...]\n{}",
        &s[..head_end],
        tail_start - head_end,
        &s[tail_start..]
    )
}

/// Empty tool results read as silence to a cheap model (it retries or
/// hallucinates); say "none found" explicitly instead.
pub fn or_none(s: String) -> String {
    if s.trim().is_empty() {
        "none found".into()
    } else {
        s
    }
}

/// "syntax OK" or a warning naming the first bad line, per tree-sitter.
fn syntax_verdict(path: &Path, src: &str) -> String {
    codemap::syntax_error_line(path, src).map_or_else(
        || "syntax OK".into(),
        |line| format!("WARNING: syntax error near line {line}"),
    )
}

impl Toolbox {
    pub const fn new(root: PathBuf) -> Self {
        Self { root }
    }

    fn resolve(&self, p: &str) -> PathBuf {
        let path = Path::new(p);
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.root.join(path)
        }
    }

    /// Resolve a path for writing; refuse anything outside the project root.
    /// Canonicalizes the nearest existing ancestor so `..` and symlinks
    /// can't escape.
    fn resolve_write(&self, p: &str) -> Result<PathBuf> {
        let path = self.resolve(p);
        // Canonicalize the nearest existing ancestor (so `..` and symlinks can't
        // escape), tracking the not-yet-existing suffix so the returned path is
        // fully canonical: the path we validate is exactly the path we write.
        let mut probe = path.as_path();
        let mut suffix = PathBuf::new();
        let ancestor = loop {
            match probe.canonicalize() {
                Ok(c) => break c,
                Err(_) => match (probe.file_name(), probe.parent()) {
                    (Some(name), Some(parent)) => {
                        // Guard the empty case: `Path::join("")` would append a
                        // trailing separator and turn a file target into a dir.
                        suffix = if suffix.as_os_str().is_empty() {
                            PathBuf::from(name)
                        } else {
                            Path::new(name).join(&suffix)
                        };
                        probe = parent;
                    }
                    _ => anyhow::bail!("cannot resolve {}", path.display()),
                },
            }
        };
        let canonical = if suffix.as_os_str().is_empty() {
            ancestor
        } else {
            ancestor.join(&suffix)
        };
        let root = self.root.canonicalize()?;
        if !canonical.starts_with(&root) {
            anyhow::bail!(
                "denied: {} is outside the project root {}. Writes outside the \
                 project require user confirmation — report this as a blocker \
                 instead of working around it.",
                path.display(),
                root.display()
            );
        }
        Ok(canonical)
    }

    pub async fn call(&self, name: &str, args: &Value) -> String {
        let result = self.dispatch(name, args).await;
        let max = if name == "read_file" {
            READ_FILE_MAX_CHARS
        } else {
            MAX_OUTPUT
        };
        truncate(
            match result {
                Ok(s) => s,
                Err(e) => format!("error: {e:#}"),
            },
            max,
        )
    }

    fn read_file(&self, args: &Value) -> Result<String> {
        let s = |k: &str| crate::util::str_arg(args, k);
        let path = self.resolve(&s("path"));
        let content = std::fs::read_to_string(&path)?;
        let lines: Vec<&str> = content.lines().collect();
        let total = lines.len();
        let end = args["end_line"].as_u64().map(|n| n as usize);
        // A bare end_line (no start) reads from line 1 rather than
        // silently falling through to a whole-file read.
        let start = args["start_line"]
            .as_u64()
            .map(|n| n as usize)
            .or_else(|| end.map(|_| 1));
        Ok(if let (Some(a), b) = (start, end) {
            // Cap range size too, or 1..999999 would bypass the
            // whole-file guard below.
            let wanted = b.unwrap_or(usize::MAX);
            let capped = wanted.min(a.saturating_add(MAX_RANGE_LINES - 1));
            let window = &lines[(a.saturating_sub(1)).min(total)..capped.min(total)];
            if window.is_empty() {
                format!(
                    "no lines in range {a}-{} — file has {total} lines",
                    if wanted == usize::MAX { total } else { wanted }
                )
            } else {
                // Tagged with LINE#HASH anchors so the model can edit without
                // re-reading; line numbers start at the window's first line.
                let mut out = crate::hashline::format_region(window, a);
                if capped < wanted && capped < total {
                    write!(
                        out,
                        "\n[range clamped to {MAX_RANGE_LINES} lines; \
                         request another range to continue]"
                    )
                    .unwrap();
                }
                out
            }
        } else if total > MAX_WHOLE_FILE_LINES {
            self.file_outline(&path, total)?
        } else {
            crate::hashline::format_region(&lines, 1)
        })
    }

    async fn dispatch(&self, name: &str, args: &Value) -> Result<String> {
        let s = |k: &str| crate::util::str_arg(args, k);
        match name {
            "read_file" => self.read_file(args),
            "write_file" => {
                let path = self.resolve_write(&s("path"))?;
                if let Some(dir) = path.parent() {
                    std::fs::create_dir_all(dir)?;
                }
                let content = s("content");
                std::fs::write(&path, &content)?;
                Ok(format!(
                    "wrote {} ({})",
                    path.display(),
                    syntax_verdict(&path, &content)
                ))
            }
            "edit_file" => self.edit_file(args).await,
            "replace_symbol" => {
                self.replace_symbol(&s("name"), &s("new_source"), &s("path"))
                    .await
            }
            "shell" => self.run("bash", &["-c", &s("command")]).await,
            "python" => self.run("python3", &["-c", &s("code")]).await,
            "read_symbol" => self.read_symbol(&s("name"), &s("path")),
            "symbol_info" => Ok(codemap::build_cached(&self.root)?.info(&s("name"))),
            "callers" => {
                let m = codemap::build_cached(&self.root)?;
                Ok(or_none(m.callers_of(&s("name")).join("\n")))
            }
            "callees" => {
                let m = codemap::build_cached(&self.root)?;
                Ok(or_none(m.callees_of(&s("name")).join("\n")))
            }
            "helpers" => crate::helpers::filtered_listing(&self.root, &s("filter")),
            "library_docs" => {
                self.library_docs(&s("lang"), &s("library"), &s("item"))
                    .await
            }
            "web_answer" => web_answer(&s("query")).await,
            "add_dependency" => {
                let dev = args["dev"].as_bool().unwrap_or(false);
                self.add_dependency(&s("lang"), &s("package"), dev).await
            }
            _ => anyhow::bail!("unknown tool {name}"),
        }
    }

    /// Hash-anchored editing: edits validate against the file's current
    /// `LINE#HASH` anchors and apply on an in-memory copy, all-or-nothing, so a
    /// failure report always means "file untouched". A stale anchor is rejected
    /// with a fresh-anchor retry snippet rather than corrupting the file.
    async fn edit_file(&self, args: &Value) -> Result<String> {
        let path = self.resolve_write(args["path"].as_str().unwrap_or(""))?;
        let content = std::fs::read_to_string(&path)?;

        let edits = crate::hashline::parse_edits(&args["edits"])?;
        if edits.is_empty() {
            anyhow::bail!("no edits given");
        }
        let result = crate::hashline::apply(&content, &edits)?;
        std::fs::write(&path, &result.content)?;

        let mut out = format!(
            "applied {} edit(s); {}",
            edits.len(),
            syntax_verdict(&path, &result.content)
        );
        for w in &result.warnings {
            out.push_str("\nwarning: ");
            out.push_str(w);
        }
        // Fresh LINE#HASH anchors for the changed region, ready to chain into
        // the next edit without a re-read.
        if let Some(anchors) = result.fresh_anchors() {
            out.push_str("\n--- anchors ---\n");
            out.push_str(&anchors);
        }
        Ok(out)
    }

    /// Read a whole definition by its code-map name: the parser supplies the
    /// span, so the model gets exactly the symbol's source — numbered, ready to
    /// hand back to `replace_symbol` — without guessing a line range.
    fn read_symbol(&self, name: &str, path_filter: &str) -> Result<String> {
        if name.is_empty() {
            anyhow::bail!("read_symbol needs a `name`");
        }
        let map = codemap::build_cached(&self.root)?;
        let sym = resolve_symbol(&map, name, path_filter)?;
        let path = self.resolve(&sym.file.display().to_string());
        let content = std::fs::read_to_string(&path)?;
        let lines: Vec<&str> = content.lines().collect();
        if sym.end_line > lines.len() || sym.line == 0 {
            anyhow::bail!("stale span for '{name}' — file changed; retry");
        }
        let body = crate::hashline::format_region(&lines[sym.line - 1..sym.end_line], sym.line);
        Ok(format!(
            "{} @ {}:{}-{}\n{body}",
            sym.name,
            sym.file.display(),
            sym.line,
            sym.end_line
        ))
    }

    /// Replace a whole definition by its code-map name: the parser supplies
    /// the byte span, so no find string is needed and no match can be
    /// ambiguous (an ambiguous *name* is reported with candidate locations).
    async fn replace_symbol(
        &self,
        name: &str,
        new_source: &str,
        path_filter: &str,
    ) -> Result<String> {
        if name.is_empty() || new_source.trim().is_empty() {
            anyhow::bail!("replace_symbol needs `name` and non-empty `new_source`");
        }
        let map = codemap::build_cached(&self.root)?;
        let sym = resolve_symbol(&map, name, path_filter)?;
        let path = self.resolve_write(&sym.file.display().to_string())?;
        let content = std::fs::read_to_string(&path)?;
        let lines: Vec<&str> = content.lines().collect();
        if sym.end_line > lines.len() || sym.line == 0 {
            anyhow::bail!("stale span for '{name}' — file changed; retry");
        }
        // `lines()` strips \r, so join with the file's own separator to keep
        // CRLF files CRLF (and the byte offsets below honest).
        let sep = if content.contains("\r\n") {
            "\r\n"
        } else {
            "\n"
        };
        let mut new_lines: Vec<&str> = lines[..sym.line - 1].to_vec();
        let body = new_source.trim_end_matches(['\n', '\r']);
        let body_lines: Vec<&str> = body.lines().collect();
        new_lines.extend(&body_lines);
        new_lines.extend(&lines[sym.end_line..]);
        let mut new = new_lines.join(sep);
        if content.ends_with('\n') {
            new.push_str(sep);
        }
        std::fs::write(&path, &new)?;

        // Echo the new definition with fresh LINE#HASH anchors, ready to chain
        // into a follow-up edit_file without a re-read.
        let new_lines: Vec<&str> = new.lines().collect();
        let echo_end = (sym.line - 1 + body_lines.len()).min(new_lines.len());
        let echo = crate::hashline::format_region(&new_lines[sym.line - 1..echo_end], sym.line);
        Ok(format!(
            "replaced {} ({}:{}-{}); {}\n{}",
            sym.name,
            sym.file.display(),
            sym.line,
            sym.end_line,
            syntax_verdict(&path, &new),
            echo
        ))
    }

    /// Symbol outline returned instead of an oversized whole-file read.
    fn file_outline(&self, path: &Path, total_lines: usize) -> Result<String> {
        let map = codemap::build_cached(&self.root)?;
        let rel = path.strip_prefix(&self.root).unwrap_or(path);
        let mut outline = String::new();
        for s in map.symbols.iter().filter(|s| s.file == rel) {
            use std::fmt::Write;
            let _ = writeln!(outline, "  L{}-{} {}", s.line, s.end_line, s.signature);
        }
        Ok(format!(
            "{} is {total_lines} lines — too large to read whole; request a \
             start_line/end_line range instead. Outline:\n{outline}",
            rel.display()
        ))
    }

    async fn run(&self, prog: &str, args: &[&str]) -> Result<String> {
        let mut cmd = Command::new(prog);
        cmd.args(args).current_dir(&self.root);
        // Confine the child: write only inside the project root plus /tmp and
        // package-manager caches. The ruleset is built here, in the parent; the
        // pre_exec closure only makes the allocation-free restrict_self syscall.
        unsafe {
            cmd.pre_exec(crate::sandbox::confiner(&self.root));
        }
        const TIMEOUT_SECS: u64 = 60;
        let out = tokio::time::timeout(std::time::Duration::from_secs(TIMEOUT_SECS), cmd.output())
            .await
            .map_err(|_| anyhow::anyhow!("timed out after {TIMEOUT_SECS}s"))??;
        let mut s = String::from_utf8_lossy(&out.stdout).to_string();
        let err = String::from_utf8_lossy(&out.stderr);
        if !err.trim().is_empty() {
            s.push_str("\n[stderr] ");
            s.push_str(err.trim());
        }
        if !out.status.success() {
            use std::fmt::Write;
            let _ = write!(s, "\n[exit {}]", out.status.code().unwrap_or(-1));
        }
        Ok(s)
    }

    async fn add_dependency(&self, lang: &str, package: &str, dev: bool) -> Result<String> {
        match lang {
            "rust" => {
                // `cargo add` without a version resolves the latest from crates.io
                // and pins it in Cargo.toml; its stderr names the chosen version.
                let mut args = vec!["add", package];
                if dev {
                    args.push("--dev");
                }
                self.run("cargo", &args).await
            }
            "python" => {
                // Install into a project-local venv so the package lands
                // inside the working directory, not user site-packages.
                let venv = self.root.join(".venv");
                if !venv.exists() {
                    self.run("python3", &["-m", "venv", ".venv"]).await?;
                }
                let python = venv.join("bin/python");
                let python = python.to_str().ok_or_else(|| {
                    anyhow::anyhow!("venv python path is not valid UTF-8: {}", python.display())
                })?;
                self.run(python, &["-m", "pip", "install", "--upgrade", package])
                    .await
            }
            "js" => {
                let spec = format!("{package}@latest");
                let mut args = vec!["install", spec.as_str()];
                if dev {
                    args.push("--save-dev");
                }
                self.run("npm", &args).await
            }
            _ => anyhow::bail!("unsupported lang {lang}"),
        }
    }

    /// Upstream docs for a third-party library. Pins to the version installed in
    /// the project when the library is a dependency (the version actually
    /// compiled against), otherwise fetches the latest published version so the
    /// model can evaluate a library before adding it. The version label tells the
    /// model which of the two it got.
    async fn library_docs(&self, lang: &str, library: &str, item: &str) -> Result<String> {
        let version = pinned_version(&self.root, lang, library);
        let label = version.as_ref().map_or_else(
            || format!("{library} latest (not a project dependency)"),
            |v| format!("{library} {v} (pinned from project)"),
        );
        // Docs are immutable per version, so cache them under `.scrooge/docs`
        // and serve a hit without touching the network.
        let cache = self.docs_cache_path(lang, library, version.as_deref(), item);
        if let Some(body) = cache.as_ref().and_then(|p| std::fs::read_to_string(p).ok()) {
            return Ok(format!("{label} [cached]\n\n{body}"));
        }
        let body = match lang {
            "rust" => rust_doc(library, item, version.as_deref()).await?,
            "js" => js_doc(library, version.as_deref()).await?,
            "python" => py_doc(library, version.as_deref()).await?,
            _ => anyhow::bail!("unsupported lang {lang}"),
        };
        if let Some(path) = cache {
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let _ = std::fs::write(&path, &body);
        }
        Ok(format!("{label}\n\n{body}"))
    }

    /// Filesystem path for a library-docs cache entry, laid out as
    /// `.scrooge/docs/{lang}/{library}@{version}/{item}.md`. Returns `None` when
    /// the version is unknown (an unpinned `latest` lookup), since a `latest`
    /// fetch can drift between runs and shouldn't be cached under a stable key.
    fn docs_cache_path(
        &self,
        lang: &str,
        library: &str,
        version: Option<&str>,
        item: &str,
    ) -> Option<PathBuf> {
        let slug = |s: &str| {
            s.chars()
                .map(|c| if c.is_alphanumeric() { c } else { '_' })
                .collect::<String>()
        };
        let version = version?;
        let leaf = if item.is_empty() {
            "index".to_string()
        } else {
            slug(item)
        };
        Some(
            self.root
                .join(".scrooge/docs")
                .join(lang)
                .join(format!("{}@{}", slug(library), slug(version)))
                .join(format!("{leaf}.md")),
        )
    }
}

/// Resolve the version of `library` actually installed in the project, or `None`
/// if it is not a dependency (in which case the doc fetchers fall back to the
/// latest published version). Best-effort and silent on failure: a missing or
/// unparseable manifest just means "no pin", never an error.
fn pinned_version(root: &Path, lang: &str, library: &str) -> Option<String> {
    match lang {
        "rust" => {
            let lock = std::fs::read_to_string(root.join("Cargo.lock")).ok()?;
            let doc: toml::Value = toml::from_str(&lock).ok()?;
            doc.get("package")?
                .as_array()?
                .iter()
                .find(|p| p.get("name").and_then(toml::Value::as_str) == Some(library))
                .and_then(|p| p.get("version"))
                .and_then(toml::Value::as_str)
                .map(str::to_string)
        }
        "js" => {
            let manifest = root.join("node_modules").join(library).join("package.json");
            let v: Value = serde_json::from_str(&std::fs::read_to_string(manifest).ok()?).ok()?;
            v["version"].as_str().map(str::to_string)
        }
        "python" => python_installed_version(root, library),
        _ => None,
    }
}

/// Read an installed package's version from its `*.dist-info` directory name in
/// the project venv. `PyPI` normalizes names case-insensitively and treats `-`/`_`
/// as equivalent, so the match does too.
fn python_installed_version(root: &Path, pkg: &str) -> Option<String> {
    let norm = |s: &str| s.replace('_', "-").to_ascii_lowercase();
    let want = norm(pkg);
    // .venv/lib/python3.X/site-packages/{name}-{version}.dist-info
    for entry in std::fs::read_dir(root.join(".venv/lib")).ok()?.flatten() {
        let Ok(rd) = std::fs::read_dir(entry.path().join("site-packages")) else {
            continue;
        };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if let Some(rest) = name.strip_suffix(".dist-info")
                && let Some((n, v)) = rest.rsplit_once('-')
                && norm(n) == want
            {
                return Some(v.to_string());
            }
        }
    }
    None
}

/// Resolve a code-map name (optionally narrowed by a path substring) to exactly
/// one symbol, or bail with a not-found / ambiguous message listing candidate
/// locations. Shared by `read_symbol` and `replace_symbol` so the two
/// symbol-by-name tools agree on what "found" and "ambiguous" mean.
fn resolve_symbol<'a>(
    map: &'a codemap::CodeMap,
    name: &str,
    path_filter: &str,
) -> Result<&'a codemap::Symbol> {
    let candidates: Vec<_> = map
        .symbols
        .iter()
        .filter(|sym| sym.name == name || sym.name.ends_with(&format!(".{name}")))
        .filter(|sym| {
            path_filter.is_empty() || sym.file.display().to_string().contains(path_filter)
        })
        .collect();
    match candidates.as_slice() {
        [] => anyhow::bail!("no symbol named '{name}' in the code map"),
        [one] => Ok(one),
        many => anyhow::bail!(
            "'{name}' is ambiguous; pass `path` to pick one of: {}",
            many.iter()
                .map(|s| format!("{}:{}", s.file.display(), s.line))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// Brave's "Answers" product — one AI-generated answer backed by real-time
/// web search, rather than a list of links. It is an OpenAI-compatible
/// chat/completions endpoint (model `brave`), so a single request returns the
/// answer text. Far cheaper on Scrooge's tokens than raw search results, which
/// is why it is the only web tool he gets.
async fn web_answer(query: &str) -> Result<String> {
    let key = std::env::var("BRAVE_ANSWERS_KEY")
        .map_err(|_| anyhow::anyhow!("BRAVE_ANSWERS_KEY is not set"))?;
    let body: Value = http()
        .post("https://api.search.brave.com/res/v1/chat/completions")
        .bearer_auth(&key)
        .json(&json!({
            "model": "brave",
            "stream": false,
            "messages": [{"role": "user", "content": query}],
        }))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let answer = body["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or("")
        .trim();
    if answer.is_empty() {
        return Ok("no web answer available for that query".into());
    }
    Ok(answer.to_string())
}

/// Crude HTML -> text: drop scripts/styles/tags, collapse whitespace.
fn html_to_text(body: &str) -> Result<String> {
    let no_script =
        regex::Regex::new(r"(?s)<(script|style)[^>]*>.*?</(script|style)>")?.replace_all(body, " ");
    let no_tags = regex::Regex::new(r"<[^>]+>")?.replace_all(&no_script, " ");
    let collapsed = regex::Regex::new(r"\s+")?.replace_all(&no_tags, " ");
    Ok(collapsed.trim().to_string())
}

/// GET a page and return its de-tagged text, or None on a non-2xx status
/// (e.g. a 404), so callers can fall through to the next candidate URL.
async fn fetch_doc(url: &str) -> Result<Option<String>> {
    let resp = http()
        .get(url)
        .header("User-Agent", "scrooge-agent")
        .send()
        .await?;
    if !resp.status().is_success() {
        return Ok(None);
    }
    Ok(Some(html_to_text(&resp.text().await?)?))
}

/// docs.rs lookup that resolves an item to its real page instead of guessing a
/// single URL. A bare crate name fetches the crate root; for `crate::Item` we
/// try each rustdoc item-kind filename (`struct.Item.html`, `enum.Item.html`,
/// …) under the item's module path, plus a module page, and use the first that
/// exists. The old single-URL scrape 404'd on essentially every non-trivial
/// path because rustdoc filenames carry a kind prefix it omitted.
/// rustdoc item-kind filename prefixes, tried in turn against a module path.
const RUST_DOC_KINDS: &[&str] = &[
    "struct",
    "enum",
    "trait",
    "fn",
    "type",
    "macro",
    "constant",
    "derive",
    "union",
    "primitive",
    "static",
];

async fn rust_doc(krate: &str, item: &str, version: Option<&str>) -> Result<String> {
    let ver = version.unwrap_or("latest");
    let root = format!("https://docs.rs/{krate}/{ver}/{krate}/");
    let item = item.trim_start_matches("::");
    let text = if item.is_empty() {
        fetch_doc(&format!("{root}index.html")).await?
    } else {
        let mut segs: Vec<&str> = item.split("::").filter(|s| !s.is_empty()).collect();
        let leaf = segs.pop().unwrap_or(item);
        let modpath = segs.join("/");
        let base = if modpath.is_empty() {
            root.clone()
        } else {
            format!("{root}{modpath}/")
        };
        let mut found = None;
        for kind in RUST_DOC_KINDS {
            if let Some(t) = fetch_doc(&format!("{base}{kind}.{leaf}.html")).await? {
                found = Some(t);
                break;
            }
        }
        // Fall back to treating the final segment as a module.
        if found.is_none() {
            found = fetch_doc(&format!("{base}{leaf}/index.html")).await?;
        }
        found
    };
    let Some(text) = text else {
        let path = if item.is_empty() {
            krate.to_string()
        } else {
            format!("{krate}::{item}")
        };
        return Ok(format!(
            "no docs.rs page found for `{path}` — check the crate/item path"
        ));
    };
    // Skip the nav boilerplate: start at the item (or crate) name, then window.
    let needle = item
        .rsplit("::")
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(krate);
    let start = text.find(needle).unwrap_or(0);
    Ok(doc_window(&text[start..]))
}

/// Fetch a JS package's README from unpkg, which serves published package files
/// at a version. README markdown is returned mostly as-is (it is already terse
/// text), just windowed.
async fn js_doc(pkg: &str, version: Option<&str>) -> Result<String> {
    let ver = version.unwrap_or("latest");
    for file in ["README.md", "readme.md", "README.markdown"] {
        let url = format!("https://unpkg.com/{pkg}@{ver}/{file}");
        if let Some(text) = fetch_raw(&url).await? {
            return Ok(doc_window(&text));
        }
    }
    Ok(format!("no README found on unpkg for `{pkg}@{ver}`"))
}

/// Fetch a Python package's summary + long description from the `PyPI` JSON API,
/// pinned to a version when known.
async fn py_doc(pkg: &str, version: Option<&str>) -> Result<String> {
    let url = version.map_or_else(
        || format!("https://pypi.org/pypi/{pkg}/json"),
        |v| format!("https://pypi.org/pypi/{pkg}/{v}/json"),
    );
    let resp = http()
        .get(&url)
        .header("User-Agent", "scrooge-agent")
        .send()
        .await?;
    if !resp.status().is_success() {
        return Ok(format!("no PyPI entry for `{pkg}`"));
    }
    let v: Value = resp.json().await?;
    let summary = v["info"]["summary"].as_str().unwrap_or("").trim();
    let desc = v["info"]["description"].as_str().unwrap_or("").trim();
    Ok(doc_window(format!("{summary}\n\n{desc}").trim()))
}

/// GET a page and return its raw text (no HTML stripping), or None on a non-2xx
/// status. For endpoints that already serve plain text/markdown.
async fn fetch_raw(url: &str) -> Result<Option<String>> {
    let resp = http()
        .get(url)
        .header("User-Agent", "scrooge-agent")
        .send()
        .await?;
    if !resp.status().is_success() {
        return Ok(None);
    }
    Ok(Some(resp.text().await?))
}

/// Trim doc text to a fixed character budget on a char boundary — keeps Scrooge
/// miserly regardless of how much the upstream page dumps.
fn doc_window(text: &str) -> String {
    const DOC_WINDOW: usize = 4000;
    let end = crate::util::floor_char_boundary(text, DOC_WINDOW.min(text.len()));
    text[..end].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toolbox() -> Toolbox {
        // A fresh root per call: tests run concurrently on the multi-threaded
        // runtime and share the process-global code-map cache (keyed by root),
        // so a shared directory would let one test's fixtures perturb another's
        // cache key mid-build. A unique root isolates them completely.
        let root = crate::util::unique_temp_path("scrooge-sbx");
        std::fs::create_dir_all(&root).unwrap();
        Toolbox::new(root)
    }

    #[tokio::test]
    async fn shell_write_inside_root_allowed() {
        let tb = toolbox();
        let out = tb
            .call(
                "shell",
                &json!({"command": "echo hi > inside.txt && cat inside.txt"}),
            )
            .await;
        assert!(out.contains("hi"), "unexpected: {out}");
    }

    #[tokio::test]
    async fn shell_write_outside_root_denied() {
        let tb = toolbox();
        // A path outside the sandbox root and outside the writable allowlist
        // (/tmp, /dev, ~/.cargo, ...): the repo's own target/ dir. It is
        // normally user-writable, so this genuinely exercises Landlock, but it
        // is gitignored and we remove it, so a non-enforcing kernel leaves
        // nothing behind in the developer's home.
        let target = format!(
            "{}/target/scrooge-landlock-escape-{}",
            env!("CARGO_MANIFEST_DIR"),
            std::process::id()
        );
        let _ = std::fs::remove_file(&target);
        let out = tb
            .call(
                "shell",
                &json!({"command": format!("echo pwned > {target}")}),
            )
            .await;
        if std::path::Path::new(&target).exists() {
            // Landlock is unsupported / not enforced on this kernel; clean up
            // and skip rather than fail.
            let _ = std::fs::remove_file(&target);
            eprintln!("landlock not enforced here; skipping sandbox-escape assertion");
            return;
        }
        assert!(out.contains("[exit"), "expected failure, got: {out}");
    }

    /// `LINE#HASH` anchor for the 1-based `line` of `content`.
    fn anchor(content: &str, line: usize) -> String {
        let l = content.lines().nth(line - 1).unwrap();
        format!("{line}#{}", crate::hashline::compute_line_hash(line, l))
    }

    #[tokio::test]
    async fn edit_file_applies_hash_anchored_edits() {
        let tb = toolbox();
        let p = tb.root.join("batch.rs");
        let src = "fn a() {}\nfn b() {}\nfn c() {}\n";
        std::fs::write(&p, src).unwrap();
        let out = tb
            .call(
                "edit_file",
                &json!({"path": "batch.rs", "edits": [
                    {"op": "replace", "pos": anchor(src, 1), "lines": ["fn alpha() {}"]},
                    {"op": "append", "pos": anchor(src, 3), "lines": ["fn d() {}"]}
                ]}),
            )
            .await;
        assert!(out.contains("applied 2 edit(s)"), "unexpected: {out}");
        assert!(
            out.contains("--- anchors ---"),
            "fresh anchors echoed: {out}"
        );
        let now = std::fs::read_to_string(&p).unwrap();
        assert_eq!(now, "fn alpha() {}\nfn b() {}\nfn c() {}\nfn d() {}\n");
    }

    #[tokio::test]
    async fn edit_file_rejects_stale_anchor_untouched() {
        let tb = toolbox();
        let p = tb.root.join("stale.rs");
        let src = "fn a() {}\nfn b() {}\n";
        std::fs::write(&p, src).unwrap();
        // A wrong hash on line 1 -> stale, nothing applied.
        let out = tb
            .call(
                "edit_file",
                &json!({"path": "stale.rs", "edits": [
                    {"op": "replace", "pos": "1#ZZ", "lines": ["fn alpha() {}"]}
                ]}),
            )
            .await;
        assert!(out.contains("E_STALE_ANCHOR"), "unexpected: {out}");
        assert_eq!(std::fs::read_to_string(&p).unwrap(), src, "file untouched");
    }

    #[tokio::test]
    async fn replace_symbol_swaps_the_whole_definition() {
        let tb = toolbox();
        std::fs::write(
            tb.root.join("sym.rs"),
            "fn keep() {\n    1;\n}\n\nfn target() {\n    old();\n    old();\n}\n",
        )
        .unwrap();
        let out = tb
            .call(
                "replace_symbol",
                &json!({"name": "target", "new_source": "fn target() {\n    new();\n}"}),
            )
            .await;
        assert!(out.contains("replaced target"), "unexpected: {out}");
        assert!(out.contains("syntax OK"), "unexpected: {out}");
        let now = std::fs::read_to_string(tb.root.join("sym.rs")).unwrap();
        assert!(now.contains("fn keep()"), "untouched neighbor");
        assert!(now.contains("new()"));
        assert!(!now.contains("old()"));
    }

    #[tokio::test]
    async fn read_symbol_returns_just_the_definition() {
        let tb = toolbox();
        std::fs::write(
            tb.root.join("rd.rs"),
            "fn rd_keep() {\n    1;\n}\n\nfn rd_target() {\n    work();\n}\n",
        )
        .unwrap();
        let out = tb.call("read_symbol", &json!({"name": "rd_target"})).await;
        assert!(out.contains("fn rd_target()"), "unexpected: {out}");
        assert!(out.contains("work()"), "unexpected: {out}");
        assert!(
            !out.contains("fn rd_keep()"),
            "should not bleed into neighbor: {out}"
        );
        // Tagged with a LINE#HASH anchor starting at the symbol's real line (5).
        assert!(
            out.contains(&format!(
                "5#{}",
                crate::hashline::compute_line_hash(5, "fn rd_target() {")
            )),
            "unexpected: {out}"
        );
    }

    #[test]
    fn truncate_keeps_head_and_tail() {
        let s = format!("HEAD{}TAIL", "x".repeat(MAX_OUTPUT * 2));
        let t = truncate(s, MAX_OUTPUT);
        assert!(t.starts_with("HEAD"));
        assert!(t.ends_with("TAIL"));
        assert!(t.contains("truncated"));
        assert!(t.len() < MAX_OUTPUT + 100);
    }

    #[tokio::test]
    async fn read_file_tags_lines_with_anchors() {
        let tb = toolbox();
        std::fs::write(tb.root.join("r.txt"), "alpha\nbeta\n").unwrap();
        let out = tb.call("read_file", &json!({"path": "r.txt"})).await;
        assert!(
            out.contains(&format!(
                "1#{}:alpha",
                crate::hashline::compute_line_hash(1, "alpha")
            )),
            "unexpected: {out}"
        );
    }

    #[tokio::test]
    async fn write_file_outside_root_denied() {
        let tb = toolbox();
        let out = tb
            .call(
                "write_file",
                &json!({"path": "../escape.txt", "content": "x"}),
            )
            .await;
        assert!(out.contains("denied"), "unexpected: {out}");
    }
}
