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

/// Definitions ride on every model call, so they are kept terse. The
/// `code_map` and `best_practices` bodies are no longer listed: both are
/// injected into Cratchit's briefing deterministically (the handlers remain
/// callable for other entry points).
pub fn definitions() -> Vec<Value> {
    vec![
        tool(
            "read_file",
            "Read a file. Files over 400 lines return an outline instead; pass start_line/end_line (max 250 lines per call).",
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
            "Create or overwrite a file. Result includes a syntax verdict.",
            obj(
                json!({"path": {"type": "string"}, "content": {"type": "string"}}),
                &["path", "content"],
            ),
        ),
        tool(
            "edit_file",
            "Apply one or more find/replace edits to a file in order, all-or-nothing. Each find must match exactly once (whitespace-tolerant fallback) unless its replace_all is true. Returns applied line numbers and a syntax verdict — no need to re-read.",
            obj(
                json!({
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
            "replace_symbol",
            "Replace an entire function/method/struct by its code-map name with new source — no find string needed, the span comes from the parser. Returns a syntax verdict. Optional path narrows when the name is ambiguous.",
            obj(
                json!({
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
            obj(json!({"command": {"type": "string"}}), &["command"]),
        ),
        tool(
            "python",
            "Run Python code; use for ALL math, counting and data transformation — never compute in your head. Prints stdout.",
            obj(json!({"code": {"type": "string"}}), &["code"]),
        ),
        tool(
            "wolfram",
            "WolframScript for symbolic math, calculus, equation solving.",
            obj(json!({"expression": {"type": "string"}}), &["expression"]),
        ),
        tool(
            "symbol_info",
            "Signature, location, callers and callees of a symbol.",
            obj(json!({"name": {"type": "string"}}), &["name"]),
        ),
        tool(
            "callers",
            "Functions that call the named function.",
            obj(json!({"name": {"type": "string"}}), &["name"]),
        ),
        tool(
            "callees",
            "Functions the named function calls.",
            obj(json!({"name": {"type": "string"}}), &["name"]),
        ),
        tool(
            "query_docs",
            "Official docs: python=pydoc, rust=docs.rs, js=MDN. Check before using any API you are not 100% sure about.",
            obj(
                json!({"lang": {"type": "string", "enum": ["python", "rust", "js"]}, "query": {"type": "string", "description": "module/symbol, e.g. 'os.path.join', 'serde_json', 'Array.prototype.map'"}}),
                &["lang", "query"],
            ),
        ),
        tool(
            "helpers",
            "Generic utility functions known in this repo and its dependencies; check before writing a new helper. Optional substring filter.",
            obj(
                json!({"filter": {"type": "string", "description": "substring filter, optional"}}),
                &[],
            ),
        ),
        tool(
            "search_libraries",
            "Web-search for the best external library for a need; call before add_dependency when choosing a library — do not pick from memory.",
            obj(
                json!({"query": {"type": "string", "description": "what you need, e.g. 'rust crate for parsing TOML'"}}),
                &["query"],
            ),
        ),
        tool(
            "add_dependency",
            "Add a dependency at its latest published version (cargo add / pip install -U / npm install @latest). Never write version numbers from memory.",
            obj(
                json!({"lang": {"type": "string", "enum": ["python", "rust", "js"]}, "package": {"type": "string"}, "dev": {"type": "boolean", "description": "dev-dependency, optional"}}),
                &["lang", "package"],
            ),
        ),
    ]
}

const MAX_OUTPUT: usize = 8000;

/// Whole-file reads above this are refused with an outline instead, so the
/// "read line ranges" rule is enforced in code rather than pleaded in prompts.
const MAX_WHOLE_FILE_LINES: usize = 400;

/// Largest line range a single read_file call returns.
const MAX_RANGE_LINES: usize = 250;

/// Whitespace-tolerant match: byte ranges of line windows whose trimmed
/// lines equal the trimmed lines of `find`.
fn fuzzy_match_ranges(content: &str, find: &str) -> Vec<(usize, usize)> {
    let needle: Vec<&str> = find.lines().map(str::trim).collect();
    if needle.is_empty() {
        return vec![];
    }
    let mut starts = vec![0usize];
    for (i, b) in content.bytes().enumerate() {
        if b == b'\n' {
            starts.push(i + 1);
        }
    }
    let lines: Vec<&str> = content.lines().collect();
    let mut out = Vec::new();
    if lines.len() < needle.len() {
        return out;
    }
    for i in 0..=lines.len() - needle.len() {
        if (0..needle.len()).all(|j| lines[i + j].trim() == needle[j]) {
            let last = i + needle.len() - 1;
            out.push((starts[i], starts[last] + lines[last].len()));
        }
    }
    out
}

/// Keep head AND tail when truncating: compiler errors and test failures
/// land at the end of long outputs, which a head-only cut would discard.
fn truncate(s: String) -> String {
    const HEAD: usize = 2000;
    if s.len() <= MAX_OUTPUT {
        return s;
    }
    let mut head_end = HEAD;
    while !s.is_char_boundary(head_end) {
        head_end -= 1;
    }
    let mut tail_start = s.len() - (MAX_OUTPUT - HEAD);
    while !s.is_char_boundary(tail_start) {
        tail_start += 1;
    }
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

/// "syntax OK" or a warning naming the first bad line, per tree-sitter.
fn syntax_verdict(path: &Path, src: &str) -> String {
    match codemap::syntax_error_line(path, src) {
        Some(line) => format!("WARNING: syntax error near line {line}"),
        None => "syntax OK".into(),
    }
}

impl Toolbox {
    pub fn new(root: PathBuf) -> Self {
        Toolbox { root }
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
        let mut probe = path.as_path();
        let canonical = loop {
            match probe.canonicalize() {
                Ok(c) => break c,
                Err(_) => match probe.parent() {
                    Some(parent) => probe = parent,
                    None => anyhow::bail!("cannot resolve {}", path.display()),
                },
            }
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
        Ok(path)
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
                let path = self.resolve(&s("path"));
                let content = std::fs::read_to_string(&path)?;
                let start = args["start_line"].as_u64().map(|n| n as usize);
                let end = args["end_line"].as_u64().map(|n| n as usize);
                Ok(match (start, end) {
                    (Some(a), b) => {
                        // Cap range size too, or 1..999999 would bypass the
                        // whole-file guard below.
                        let wanted = b.unwrap_or(usize::MAX);
                        let capped = wanted.min(a.saturating_add(MAX_RANGE_LINES - 1));
                        let mut out = content
                            .lines()
                            .enumerate()
                            .filter(|(i, _)| *i + 1 >= a && *i < capped)
                            .map(|(i, l)| format!("{}|{l}", i + 1))
                            .collect::<Vec<_>>()
                            .join("\n");
                        if capped < wanted && capped < content.lines().count() {
                            out.push_str(&format!(
                                "\n[range clamped to {MAX_RANGE_LINES} lines; \
                                 request another range to continue]"
                            ));
                        }
                        out
                    }
                    _ => {
                        let total = content.lines().count();
                        if total > MAX_WHOLE_FILE_LINES {
                            self.file_outline(&path, total)?
                        } else {
                            content
                        }
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
            "wolfram" => {
                self.run("wolframscript", &["-code", &s("expression")])
                    .await
            }
            "code_map" => Ok(codemap::build_cached(&self.root)?.brief()),
            "symbol_info" => Ok(codemap::build_cached(&self.root)?.detail(&s("name"))),
            "callers" => {
                let m = codemap::build_cached(&self.root)?;
                Ok(m.callers_of(&s("name")).join("\n"))
            }
            "callees" => {
                let m = codemap::build_cached(&self.root)?;
                Ok(m.callees_of(&s("name")).join("\n"))
            }
            "helpers" => crate::helpers::filtered_listing(&self.root, &s("filter")),
            "query_docs" => self.query_docs(&s("lang"), &s("query")).await,
            "search_libraries" => web_search(&s("query")).await,
            "add_dependency" => {
                let dev = args["dev"].as_bool().unwrap_or(false);
                self.add_dependency(&s("lang"), &s("package"), dev).await
            }
            "best_practices" => Ok(practices::relevant_sections(&s("topic"))),
            _ => anyhow::bail!("unknown tool {name}"),
        }
    }

    /// Batched find/replace: edits apply in order on an in-memory copy and
    /// nothing is written unless every edit succeeds, so a failure report
    /// always means "file untouched".
    async fn edit_file(&self, args: &Value) -> Result<String> {
        let path = self.resolve_write(args["path"].as_str().unwrap_or(""))?;
        let mut content = std::fs::read_to_string(&path)?;

        struct E {
            find: String,
            replace: String,
            all: bool,
        }
        // Array form is canonical; the legacy single find/replace form is
        // still accepted so a confused model isn't hard-stuck.
        let edits: Vec<E> = match args["edits"].as_array() {
            Some(arr) => arr
                .iter()
                .map(|e| E {
                    find: e["find"].as_str().unwrap_or("").to_string(),
                    replace: e["replace"].as_str().unwrap_or("").to_string(),
                    all: e["replace_all"].as_bool().unwrap_or(false),
                })
                .collect(),
            None => vec![E {
                find: args["find"].as_str().unwrap_or("").to_string(),
                replace: args["replace"].as_str().unwrap_or("").to_string(),
                all: args["replace_all"].as_bool().unwrap_or(false),
            }],
        };
        if edits.is_empty() || edits.iter().any(|e| e.find.is_empty()) {
            anyhow::bail!("no edits given (each needs a non-empty `find`)");
        }

        let line_of = |content: &str, at: usize| content[..at].matches('\n').count() + 1;
        let mut notes = Vec::new();
        let mut single_region: Option<(usize, usize)> = None;
        for (i, e) in edits.iter().enumerate() {
            let n = i + 1;
            match content.matches(&e.find).count() {
                0 => match fuzzy_match_ranges(&content, &e.find).as_slice() {
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
                        single_region = Some((a, e.replace.len()));
                    }
                    m => anyhow::bail!(
                        "edit {n}: matches {} places ignoring whitespace — no edits applied",
                        m.len()
                    ),
                },
                1 => {
                    let at = content.find(&e.find).unwrap_or(0);
                    content = content.replacen(&e.find, &e.replace, 1);
                    notes.push(format!("edit {n}: line {}", line_of(&content, at)));
                    single_region = Some((at, e.replace.len()));
                }
                c if e.all => {
                    content = content.replace(&e.find, &e.replace);
                    notes.push(format!("edit {n}: replaced {c} occurrences"));
                    single_region = None;
                }
                c => anyhow::bail!(
                    "edit {n}: string appears {c} times; set replace_all or provide \
                     more context — no edits applied"
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
        if edits.len() == 1
            && let Some((at, len)) = single_region
        {
            out.push('\n');
            out.push_str(&edit_echo(&content, at, len));
        }
        Ok(out)
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
        let candidates: Vec<_> = map
            .symbols
            .iter()
            .filter(|sym| sym.name == name || sym.name.ends_with(&format!(".{name}")))
            .filter(|sym| {
                path_filter.is_empty() || sym.file.display().to_string().contains(path_filter)
            })
            .collect();
        let sym = match candidates.as_slice() {
            [] => anyhow::bail!("no symbol named '{name}' in the code map"),
            [one] => *one,
            many => anyhow::bail!(
                "'{name}' is ambiguous; pass `path` to pick one of: {}",
                many.iter()
                    .map(|s| format!("{}:{}", s.file.display(), s.line))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        };
        let path = self.resolve_write(&sym.file.display().to_string())?;
        let content = std::fs::read_to_string(&path)?;
        let lines: Vec<&str> = content.lines().collect();
        if sym.end_line > lines.len() || sym.line == 0 {
            anyhow::bail!("stale span for '{name}' — file changed; retry");
        }
        let mut new_lines: Vec<&str> = lines[..sym.line - 1].to_vec();
        let body = new_source.trim_end_matches('\n');
        new_lines.extend(body.lines());
        new_lines.extend(&lines[sym.end_line..]);
        let mut new = new_lines.join("\n");
        if content.ends_with('\n') {
            new.push('\n');
        }
        std::fs::write(&path, &new)?;

        let at: usize = new.lines().take(sym.line - 1).map(|l| l.len() + 1).sum();
        Ok(format!(
            "replaced {} ({}:{}-{}); {}\n{}",
            sym.name,
            sym.file.display(),
            sym.line,
            sym.end_line,
            syntax_verdict(&path, &new),
            edit_echo(&new, at, body.len())
        ))
    }

    /// Symbol outline returned instead of an oversized whole-file read.
    fn file_outline(&self, path: &Path, total_lines: usize) -> Result<String> {
        let map = codemap::build_cached(&self.root)?;
        let rel = path.strip_prefix(&self.root).unwrap_or(path);
        let outline: String = map
            .symbols
            .iter()
            .filter(|s| s.file == rel)
            .map(|s| format!("  L{}-{} {}\n", s.line, s.end_line, s.signature))
            .collect();
        Ok(format!(
            "{} is {total_lines} lines — too large to read whole; request a \
             start_line/end_line range instead. Outline:\n{outline}",
            rel.display()
        ))
    }

    async fn run(&self, prog: &str, args: &[&str]) -> Result<String> {
        let mut cmd = Command::new(prog);
        cmd.args(args).current_dir(&self.root);
        let sandbox_root = self.root.clone();
        // Confine the child: read anywhere, write only inside the project
        // root plus /tmp and package-manager caches. Best-effort — on a
        // kernel without Landlock the command still runs.
        unsafe {
            cmd.pre_exec(move || {
                let _ = sandbox::confine(&sandbox_root);
                Ok(())
            });
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
            s.push_str(&format!("\n[exit {}]", out.status.code().unwrap_or(-1)));
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
                let pip = venv.join("bin/python");
                self.run(
                    pip.to_str().unwrap_or("python3"),
                    &["-m", "pip", "install", "--upgrade", package],
                )
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
            "rust" => {
                // docs.rs serves a text-friendly page per crate/item; strip
                // tags, then skip the nav boilerplate before the item itself.
                let krate = query.split("::").next().unwrap_or(query);
                let item = query.replace("::", "/");
                let url = if krate == query {
                    format!("https://docs.rs/{krate}/latest/{krate}/")
                } else {
                    format!("https://docs.rs/{krate}/latest/{item}/")
                };
                let text = fetch_text(&url).await?;
                let needle = query.rsplit("::").next().unwrap_or(query);
                let start = text.find(needle).unwrap_or(0);
                let mut end = (start + 4000).min(text.len());
                while !text.is_char_boundary(end) {
                    end -= 1;
                }
                Ok(text[start..end].to_string())
            }
            "js" => {
                // MDN's search API returns JSON; extract title/summary/url
                // instead of dumping the raw payload on the model.
                let q = query.replace(' ', "+");
                let url = format!("https://developer.mozilla.org/api/v1/search?q={q}&locale=en-US");
                let v: Value = reqwest::Client::new()
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

mod sandbox {
    use landlock::{
        ABI, Access, AccessFs, Ruleset, RulesetAttr, RulesetCreatedAttr, path_beneath_rules,
    };
    use std::path::{Path, PathBuf};

    /// Landlock policy: read the whole filesystem, write only beneath the
    /// project root, /tmp, /dev (null, shm, …) and the package-manager
    /// caches that `cargo`/`npm`/`pip` need to function.
    pub fn confine(root: &Path) -> Result<(), Box<dyn std::error::Error>> {
        let abi = ABI::V2;
        let mut writable: Vec<PathBuf> = vec![root.to_path_buf(), "/tmp".into(), "/dev".into()];
        if let Ok(home) = std::env::var("HOME") {
            for d in [".cargo", ".npm", ".cache"] {
                writable.push(Path::new(&home).join(d));
            }
        }
        writable.retain(|p| p.exists());
        Ruleset::default()
            .handle_access(AccessFs::from_all(abi))?
            .create()?
            .add_rules(path_beneath_rules(["/"], AccessFs::from_read(abi)))?
            .add_rules(path_beneath_rules(&writable, AccessFs::from_all(abi)))?
            .restrict_self()?;
        Ok(())
    }
}

async fn web_search(query: &str) -> Result<String> {
    let key = std::env::var("BRAVE_SEARCH_KEY")
        .map_err(|_| anyhow::anyhow!("BRAVE_SEARCH_KEY is not set"))?;
    let body: Value = reqwest::Client::new()
        .get("https://api.search.brave.com/res/v1/web/search")
        .query(&[("q", query), ("count", "8")])
        .header("X-Subscription-Token", key)
        .header("Accept", "application/json")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let results = body["web"]["results"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    if results.is_empty() {
        return Ok("no results".into());
    }
    Ok(results
        .iter()
        .map(|r| {
            format!(
                "{}\n  {}\n  {}",
                r["title"].as_str().unwrap_or(""),
                r["url"].as_str().unwrap_or(""),
                r["description"].as_str().unwrap_or("")
            )
        })
        .collect::<Vec<_>>()
        .join("\n"))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn toolbox() -> Toolbox {
        let root = std::env::temp_dir().join(format!("scrooge-sbx-{}", std::process::id()));
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
        let target = format!("{}/scrooge-landlock-escape", std::env::var("HOME").unwrap());
        let out = tb
            .call(
                "shell",
                &json!({"command": format!("echo pwned > {target}")}),
            )
            .await;
        assert!(
            !std::path::Path::new(&target).exists(),
            "sandbox escape: wrote {target}"
        );
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

    #[test]
    fn truncate_keeps_head_and_tail() {
        let s = format!("HEAD{}TAIL", "x".repeat(MAX_OUTPUT * 2));
        let t = truncate(s);
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
