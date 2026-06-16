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
            "Read a file. Files over 2000 lines return an outline instead; pass start_line/end_line (max 2000 lines per call). Prefer narrow ranges — reading whole large files burns context.",
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
            "Apply one or more find/replace edits to a file in order, all-or-nothing — batch related edits into ONE call rather than issuing several. Each find must match exactly once (whitespace-tolerant fallback) unless its replace_all is true. Returns applied line numbers and a syntax verdict — no need to re-read.",
            &obj(
                &json!({
                    "path": {"type": "string"},
                    "edits": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "find": {"type": "string"},
                                "replace": {"type": "string"},
                                "replace_all": {"type": "boolean", "description": "replace every occurrence, optional"}
                            },
                            "required": ["find", "replace"]
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
            "query_docs",
            "Official docs: python=pydoc, rust=docs.rs, js=MDN. Check before using any API you are not 100% sure about.",
            &obj(
                &json!({"lang": {"type": "string", "enum": ["python", "rust", "js"]}, "query": {"type": "string", "description": "module/symbol, e.g. 'os.path.join', 'serde_json', 'Array.prototype.map'"}}),
                &["lang", "query"],
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

/// Whitespace-tolerant match: byte ranges of line windows whose trimmed
/// lines equal the trimmed lines of `find`.
fn fuzzy_match_ranges(content: &str, find: &str) -> Vec<(usize, usize)> {
    let needle: Vec<&str> = find.lines().map(str::trim).collect();
    if needle.is_empty() {
        return vec![];
    }
    // (start_byte, content_end_byte) for each line, derived purely from the raw
    // bytes — `str::lines` strips a trailing `\r`, so deriving the end offset
    // from `lines[k].len()` would land one byte early on a CRLF file and splice
    // mid-`\r`. Tracking it here keeps the offsets honest for either ending.
    let bytes = content.as_bytes();
    let mut spans: Vec<(usize, usize)> = Vec::new();
    let mut start = 0usize;
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'\n' {
            let end = if i > start && bytes[i - 1] == b'\r' {
                i - 1
            } else {
                i
            };
            spans.push((start, end));
            start = i + 1;
        }
    }
    // Final line with no trailing newline.
    if start < bytes.len() {
        spans.push((start, bytes.len()));
    }
    // Each span's [start, content_end) slice is the line minus its ending, so
    // it stands in for `content.lines()` without a second pass/allocation.
    let mut out = Vec::new();
    if spans.len() < needle.len() {
        return out;
    }
    for i in 0..=spans.len() - needle.len() {
        if (0..needle.len()).all(|j| {
            let (s, e) = spans[i + j];
            content[s..e].trim() == needle[j]
        }) {
            let last = i + needle.len() - 1;
            out.push((spans[i].0, spans[last].1));
        }
    }
    out
}

/// Keep head AND tail when truncating: compiler errors and test failures
/// land at the end of long outputs, which a head-only cut would discard.
/// `max` is the per-tool budget (`read_file` gets a bigger one than shell).
fn truncate(s: String, max: usize) -> String {
    if s.len() <= max {
        return s;
    }
    let head = max / 4;
    let head_end = crate::util::floor_char_boundary(&s, head);
    let tail_start = crate::util::ceil_char_boundary(&s, s.len() - (max - head));
    format!(
        "{}\n[... {} chars truncated ...]\n{}",
        &s[..head_end],
        tail_start - head_end,
        &s[tail_start..]
    )
}

/// Numbered lines around a replaced byte region, so the model can confirm an
/// edit from the tool result instead of re-reading the file.
fn edit_echo(content: &str, at: usize, len: usize) -> String {
    let first = content[..at].matches('\n').count();
    let last = first + content[at..at + len].matches('\n').count();
    let lo = first.saturating_sub(2);
    content
        .lines()
        .enumerate()
        .skip(lo)
        .take(last + 2 - lo + 1)
        .map(|(i, l)| format!("{}|{l}", i + 1))
        .collect::<Vec<_>>()
        .join("\n")
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

    async fn dispatch(&self, name: &str, args: &Value) -> Result<String> {
        let s = |k: &str| crate::util::str_arg(args, k);
        match name {
            "read_file" => {
                let path = self.resolve(&s("path"));
                let content = std::fs::read_to_string(&path)?;
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
                    // Lines are returned unnumbered, matching a whole-file read.
                    let mut out = content
                        .lines()
                        .enumerate()
                        .filter(|(i, _)| *i + 1 >= a && *i < capped)
                        .map(|(_, l)| l)
                        .collect::<Vec<_>>()
                        .join("\n");
                    let total = content.lines().count();
                    if out.is_empty() {
                        out = format!(
                            "no lines in range {a}-{} — file has {total} lines",
                            if wanted == usize::MAX { total } else { wanted }
                        );
                    } else if capped < wanted && capped < total {
                        write!(
                            out,
                            "\n[range clamped to {MAX_RANGE_LINES} lines; \
                             request another range to continue]"
                        )
                        .unwrap();
                    }
                    out
                } else {
                    let total = content.lines().count();
                    if total > MAX_WHOLE_FILE_LINES {
                        self.file_outline(&path, total)?
                    } else {
                        content
                    }
                })
            }
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
            "query_docs" => self.query_docs(&s("lang"), &s("query")).await,
            "web_answer" => web_answer(&s("query")).await,
            "add_dependency" => {
                let dev = args["dev"].as_bool().unwrap_or(false);
                self.add_dependency(&s("lang"), &s("package"), dev).await
            }
            _ => anyhow::bail!("unknown tool {name}"),
        }
    }

    /// Batched find/replace: edits apply in order on an in-memory copy and
    /// nothing is written unless every edit succeeds, so a failure report
    /// always means "file untouched".
    async fn edit_file(&self, args: &Value) -> Result<String> {
        struct E {
            find: String,
            replace: String,
            all: bool,
        }
        let path = self.resolve_write(args["path"].as_str().unwrap_or(""))?;
        let mut content = std::fs::read_to_string(&path)?;

        // Array form is canonical; the legacy single find/replace form is
        // still accepted so a confused model isn't hard-stuck.
        let edits: Vec<E> = args["edits"].as_array().map_or_else(
            || {
                vec![E {
                    find: args["find"].as_str().unwrap_or("").to_string(),
                    replace: args["replace"].as_str().unwrap_or("").to_string(),
                    all: args["replace_all"].as_bool().unwrap_or(false),
                }]
            },
            |arr| {
                arr.iter()
                    .map(|e| E {
                        find: e["find"].as_str().unwrap_or("").to_string(),
                        replace: e["replace"].as_str().unwrap_or("").to_string(),
                        all: e["replace_all"].as_bool().unwrap_or(false),
                    })
                    .collect()
            },
        );
        if edits.is_empty() || edits.iter().any(|e| e.find.is_empty()) {
            anyhow::bail!("no edits given (each needs a non-empty `find`)");
        }

        let line_of = |content: &str, at: usize| content[..at].matches('\n').count() + 1;
        let mut notes = Vec::new();
        // Region of the most recent single replacement — later edits can't
        // shift it, so it is always valid to echo after the loop.
        let mut last_region: Option<(usize, usize)> = None;
        for (i, e) in edits.iter().enumerate() {
            let n = i + 1;
            let occurrences: Vec<usize> =
                content.match_indices(&e.find).map(|(at, _)| at).collect();
            match occurrences.as_slice() {
                [] => match fuzzy_match_ranges(&content, &e.find).as_slice() {
                    [] => anyhow::bail!(
                        "edit {n}: string not found in {} — no edits applied",
                        path.display()
                    ),
                    [(a, b)] => {
                        let (a, b) = (*a, *b);
                        content = format!("{}{}{}", &content[..a], e.replace, &content[b..]);
                        notes.push(format!(
                            "edit {n}: line {} (whitespace-tolerant)",
                            line_of(&content, a)
                        ));
                        last_region = Some((a, e.replace.len()));
                    }
                    m => anyhow::bail!(
                        "edit {n}: matches {} places ignoring whitespace — no edits applied",
                        m.len()
                    ),
                },
                [at] => {
                    let at = *at;
                    content = format!(
                        "{}{}{}",
                        &content[..at],
                        e.replace,
                        &content[at + e.find.len()..]
                    );
                    notes.push(format!("edit {n}: line {}", line_of(&content, at)));
                    last_region = Some((at, e.replace.len()));
                }
                m if e.all => {
                    content = content.replace(&e.find, &e.replace);
                    notes.push(format!("edit {n}: replaced {} occurrences", m.len()));
                    last_region = None;
                }
                m => anyhow::bail!(
                    "edit {n}: string appears {} times; set replace_all or provide \
                     more context — no edits applied",
                    m.len()
                ),
            }
        }
        std::fs::write(&path, &content)?;

        let mut out = format!(
            "applied {} edit(s) ({}); {}",
            edits.len(),
            notes.join("; "),
            syntax_verdict(&path, &content)
        );
        if let Some((at, len)) = last_region {
            out.push('\n');
            out.push_str(&edit_echo(&content, at, len));
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
        let body = lines[sym.line - 1..sym.end_line]
            .iter()
            .enumerate()
            .map(|(i, l)| format!("{}|{l}", sym.line + i))
            .collect::<Vec<_>>()
            .join("\n");
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
        let body_len = body_lines.iter().map(|l| l.len()).sum::<usize>()
            + body_lines.len().saturating_sub(1) * sep.len();
        new_lines.extend(&body_lines);
        new_lines.extend(&lines[sym.end_line..]);
        let mut new = new_lines.join(sep);
        if content.ends_with('\n') {
            new.push_str(sep);
        }
        std::fs::write(&path, &new)?;

        let at: usize = new
            .lines()
            .take(sym.line - 1)
            .map(|l| l.len() + sep.len())
            .sum();
        Ok(format!(
            "replaced {} ({}:{}-{}); {}\n{}",
            sym.name,
            sym.file.display(),
            sym.line,
            sym.end_line,
            syntax_verdict(&path, &new),
            edit_echo(&new, at, body_len)
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
        let out = tokio::time::timeout(std::time::Duration::from_secs(60), cmd.output())
            .await
            .map_err(|_| anyhow::anyhow!("timed out after 60s"))??;
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

    async fn query_docs(&self, lang: &str, query: &str) -> Result<String> {
        match lang {
            "python" => self.run("python3", &["-m", "pydoc", query]).await,
            "rust" => rust_doc(query).await,
            "js" => {
                // MDN's search API returns JSON; extract title/summary/url
                // instead of dumping the raw payload on the model.
                let q = query.replace(' ', "+");
                let url = format!("https://developer.mozilla.org/api/v1/search?q={q}&locale=en-US");
                let v: Value = http()
                    .get(&url)
                    .header("User-Agent", "scrooge-agent")
                    .send()
                    .await?
                    .json()
                    .await?;
                let docs = v["documents"].as_array().cloned().unwrap_or_default();
                if docs.is_empty() {
                    return Ok("no MDN results".into());
                }
                Ok(docs
                    .iter()
                    .take(5)
                    .map(|d| {
                        format!(
                            "{} — {}\n  https://developer.mozilla.org{}",
                            d["title"].as_str().unwrap_or(""),
                            d["summary"].as_str().unwrap_or("").trim(),
                            d["mdn_url"].as_str().unwrap_or("")
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n"))
            }
            _ => anyhow::bail!("unsupported lang {lang}"),
        }
    }
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

async fn rust_doc(query: &str) -> Result<String> {
    let mut segs: Vec<&str> = query.split("::").filter(|s| !s.is_empty()).collect();
    let Some(krate) = segs.first().copied() else {
        return Ok("empty query".into());
    };
    let root = format!("https://docs.rs/{krate}/latest/{krate}/");
    let text = if segs.len() <= 1 {
        fetch_doc(&format!("{root}index.html")).await?
    } else {
        let item = segs.pop().unwrap();
        let modpath = segs[1..].join("/");
        let base = if modpath.is_empty() {
            root.clone()
        } else {
            format!("{root}{modpath}/")
        };
        let mut found = None;
        for kind in RUST_DOC_KINDS {
            if let Some(t) = fetch_doc(&format!("{base}{kind}.{item}.html")).await? {
                found = Some(t);
                break;
            }
        }
        // Fall back to treating the final segment as a module.
        if found.is_none() {
            found = fetch_doc(&format!("{base}{item}/index.html")).await?;
        }
        found
    };
    let Some(text) = text else {
        return Ok(format!(
            "no docs.rs page found for `{query}` — check the crate/item path"
        ));
    };
    // Skip the nav boilerplate: start at the item name, then take a window.
    let needle = query.rsplit("::").next().unwrap_or(query);
    let start = text.find(needle).unwrap_or(0);
    let end = crate::util::floor_char_boundary(&text, (start + 4000).min(text.len()));
    Ok(text[start..end].to_string())
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

    #[tokio::test]
    async fn edit_file_batch_is_all_or_nothing() {
        let tb = toolbox();
        let p = tb.root.join("batch.rs");
        std::fs::write(&p, "fn a() {}\nfn b() {}\nfn c() {}\n").unwrap();
        // second edit fails -> nothing applied
        let out = tb
            .call(
                "edit_file",
                &json!({"path": "batch.rs", "edits": [
                    {"find": "fn a", "replace": "fn alpha"},
                    {"find": "fn zzz", "replace": "fn z"}
                ]}),
            )
            .await;
        assert!(out.contains("no edits applied"), "unexpected: {out}");
        assert!(std::fs::read_to_string(&p).unwrap().contains("fn a()"));
        // valid batch applies in order, with replace_all
        let out = tb
            .call(
                "edit_file",
                &json!({"path": "batch.rs", "edits": [
                    {"find": "fn a", "replace": "fn alpha"},
                    {"find": "()", "replace": "(x: u8)", "replace_all": true}
                ]}),
            )
            .await;
        assert!(out.contains("applied 2 edit(s)"), "unexpected: {out}");
        assert!(out.contains("3 occurrences"), "unexpected: {out}");
        let now = std::fs::read_to_string(&p).unwrap();
        assert!(now.contains("fn alpha(x: u8)"));
        assert!(now.contains("fn c(x: u8)"));
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
        // Numbered, starting at the symbol's real line (5).
        assert!(out.contains("5|fn rd_target()"), "unexpected: {out}");
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

    #[test]
    fn edit_echo_numbers_the_region() {
        let content = "a\nb\nNEW1\nNEW2\nc\nd\ne\nf\n";
        let at = content.find("NEW1").unwrap();
        let echo = edit_echo(content, at, "NEW1\nNEW2".len());
        assert!(echo.contains("3|NEW1"));
        assert!(echo.contains("4|NEW2"));
        assert!(echo.contains("1|a"), "leading context");
        assert!(echo.contains("6|d"), "trailing context");
        assert!(!echo.contains("7|e"), "bounded context");
    }

    #[test]
    fn fuzzy_match_ignores_indentation() {
        let content = "fn a() {\n    let x = 1;\n    let y = 2;\n}\n";
        let find = "let x = 1;\nlet y = 2;";
        let ranges = fuzzy_match_ranges(content, find);
        assert_eq!(ranges.len(), 1);
        let (a, b) = ranges[0];
        assert_eq!(&content[a..b], "    let x = 1;\n    let y = 2;");
        assert!(fuzzy_match_ranges(content, "let z = 3;").is_empty());
    }

    #[test]
    fn fuzzy_match_offsets_survive_crlf() {
        // CRLF line endings: `str::lines` strips the `\r`, so the byte offsets
        // must come from the raw bytes or they land one byte early per line.
        let content = "fn a() {\r\n    let x = 1;\r\n    let y = 2;\r\n}\r\n";
        let find = "let x = 1;\nlet y = 2;";
        let ranges = fuzzy_match_ranges(content, find);
        assert_eq!(ranges.len(), 1);
        let (a, b) = ranges[0];
        // The matched slice is exact (no stray `\r`), so a replace splices cleanly.
        assert_eq!(&content[a..b], "    let x = 1;\r\n    let y = 2;");
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
