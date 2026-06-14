//! Codebase summarizer: extracts symbols and a function-call graph using
//! tree-sitter, then renders ultra-compact briefs for LLM consumption.
//! Deterministic — costs zero tokens to build.

use anyhow::{Context, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::SystemTime;
use tree_sitter::{Language, Node, Parser};
use walkdir::WalkDir;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SymbolKind {
    Function,
    Method,
    Struct,
    Enum,
    Trait,
    Class,
    Impl,
}

impl SymbolKind {
    const fn short(&self) -> &'static str {
        match self {
            Self::Function => "fn",
            Self::Method => "method",
            Self::Struct => "struct",
            Self::Enum => "enum",
            Self::Trait => "trait",
            Self::Class => "class",
            Self::Impl => "impl",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Symbol {
    pub name: String,
    pub kind: SymbolKind,
    /// Signature line, trimmed (no body).
    pub signature: String,
    pub file: PathBuf,
    pub line: usize,
    pub end_line: usize,
}

#[derive(Debug, Default)]
pub struct CodeMap {
    pub symbols: Vec<Symbol>,
    /// caller symbol name -> set of callee names (only names defined in this codebase).
    pub calls: BTreeMap<String, BTreeSet<String>>,
}

enum Lang {
    Rust,
    Python,
    Js,
    Html,
}

fn lang_for(path: &Path) -> Option<Lang> {
    match path.extension()?.to_str()? {
        "rs" => Some(Lang::Rust),
        "py" => Some(Lang::Python),
        "js" | "mjs" | "jsx" => Some(Lang::Js),
        "html" | "htm" => Some(Lang::Html),
        _ => None,
    }
}

const SKIP_DIRS: &[&str] = &[
    ".git",
    "target",
    "node_modules",
    "__pycache__",
    ".venv",
    "venv",
    "dist",
    "build",
    ".scrooge",
    "tests",
    "test",
    "examples",
    "benches",
];

const MAX_FILE_BYTES: u64 = 262_144;

/// All indexable source files under `root`, skipping vendored/build dirs.
/// Shared by `build_limited` and `cache_key` so the cache key and the build
/// always agree on what counts as a source file.
fn source_files(root: &Path) -> impl Iterator<Item = walkdir::DirEntry> {
    WalkDir::new(root)
        .into_iter()
        .filter_entry(|e| {
            !e.file_name()
                .to_str()
                .is_some_and(|n| SKIP_DIRS.contains(&n))
        })
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file() && lang_for(e.path()).is_some())
}

pub fn build(root: &Path) -> Result<CodeMap> {
    build_limited(root, usize::MAX)
}

/// Like `build` but stops after `max_files` source files — used when
/// scanning third-party dependencies, which can be arbitrarily large.
pub fn build_limited(root: &Path, max_files: usize) -> Result<CodeMap> {
    let mut map = CodeMap::default();
    let mut seen = 0usize;
    for entry in source_files(root) {
        let path = entry.path();
        let Some(lang) = lang_for(path) else { continue };
        if entry
            .metadata()
            .map(|m| m.len() > MAX_FILE_BYTES)
            .unwrap_or(true)
        {
            continue;
        }
        seen += 1;
        if seen > max_files {
            break;
        }
        let Ok(src) = std::fs::read_to_string(path) else {
            continue; // non-utf8 etc.
        };
        let rel = path.strip_prefix(root).unwrap_or(path).to_path_buf();
        index_file(&mut map, &rel, &src, &lang)?;
    }
    resolve_calls(&mut map);
    Ok(map)
}

type CacheEntry = (PathBuf, SystemTime, usize, Arc<CodeMap>);
static CACHE: OnceLock<Mutex<Option<CacheEntry>>> = OnceLock::new();

/// Cheap staleness key: newest mtime + count of indexable source files.
/// Walking metadata is far cheaper than re-parsing every file.
fn cache_key(root: &Path) -> (SystemTime, usize) {
    let mut newest = SystemTime::UNIX_EPOCH;
    let mut count = 0usize;
    for entry in source_files(root) {
        count += 1;
        if let Ok(meta) = entry.metadata()
            && let Ok(t) = meta.modified()
            && t > newest
        {
            newest = t;
        }
    }
    (newest, count)
}

/// `build` behind an mtime-keyed process cache, for tool calls that hit the
/// map repeatedly within one session.
pub fn build_cached(root: &Path) -> Result<Arc<CodeMap>> {
    let (mtime, count) = cache_key(root);
    let cache = CACHE.get_or_init(|| Mutex::new(None));
    {
        let mut guard = cache.lock().unwrap();
        if let Some(v) = guard.as_ref()
            && v.0 == root
            && v.1 == mtime
            && v.2 == count
        {
            return Ok(v.3.clone());
        }
        let map = Arc::new(build(root)?);
        *guard = Some((root.to_path_buf(), mtime, count, map.clone()));
        drop(guard);
        Ok(map)
    }
}

/// Line of the first syntax error in `src` per the file's grammar, or None
/// when the file parses (or has no grammar). Lets write/edit tools report a
/// broken edit in the same turn instead of waiting for the test run.
pub fn syntax_error_line(path: &Path, src: &str) -> Option<usize> {
    let lang = lang_for(path)?;
    if matches!(lang, Lang::Html) {
        return None; // html parses almost anything; not worth flagging
    }
    let mut parser = Parser::new();
    parser.set_language(&ts_language(&lang)).ok()?;
    let tree = parser.parse(src, None)?;
    first_error_line(tree.root_node())
}

fn first_error_line(node: Node) -> Option<usize> {
    if node.is_error() || node.is_missing() {
        return Some(node.start_position().row + 1);
    }
    if !node.has_error() {
        return None;
    }
    let mut c = node.walk();
    for child in node.children(&mut c) {
        if let Some(line) = first_error_line(child) {
            return Some(line);
        }
    }
    None
}

fn ts_language(lang: &Lang) -> Language {
    match lang {
        Lang::Rust => tree_sitter_rust::LANGUAGE.into(),
        Lang::Python => tree_sitter_python::LANGUAGE.into(),
        Lang::Js => tree_sitter_javascript::LANGUAGE.into(),
        Lang::Html => tree_sitter_html::LANGUAGE.into(),
    }
}

fn index_file(map: &mut CodeMap, rel: &Path, src: &str, lang: &Lang) -> Result<()> {
    if matches!(lang, Lang::Html) {
        // Parse HTML only to pull out <script> bodies, then index those as JS.
        let mut parser = Parser::new();
        parser
            .set_language(&ts_language(&Lang::Html))
            .context("html grammar")?;
        if let Some(tree) = parser.parse(src, None) {
            collect_scripts(tree.root_node(), src.as_bytes(), &mut |js| {
                let _ = index_file(map, rel, js, &Lang::Js);
            });
        }
        return Ok(());
    }

    let mut parser = Parser::new();
    parser.set_language(&ts_language(lang)).context("grammar")?;
    let Some(tree) = parser.parse(src, None) else {
        return Ok(());
    };
    let bytes = src.as_bytes();
    walk(map, rel, bytes, tree.root_node(), lang, None);
    Ok(())
}

fn collect_scripts(node: Node, bytes: &[u8], f: &mut dyn FnMut(&str)) {
    if node.kind() == "script_element" {
        let mut c = node.walk();
        for child in node.children(&mut c) {
            if child.kind() == "raw_text"
                && let Ok(text) = child.utf8_text(bytes)
            {
                f(text);
            }
        }
    }
    let mut c = node.walk();
    for child in node.children(&mut c) {
        collect_scripts(child, bytes, f);
    }
}

fn text<'a>(node: Node, bytes: &'a [u8]) -> &'a str {
    node.utf8_text(bytes).unwrap_or("")
}

fn first_line(s: &str) -> String {
    let line = s.lines().next().unwrap_or("").trim();
    let line = line.trim_end_matches('{').trim_end_matches(':').trim();
    line.to_string()
}

fn name_of(node: Node, bytes: &[u8]) -> Option<String> {
    node.child_by_field_name("name")
        .map(|n| text(n, bytes).to_string())
}

/// Recursively walk the AST collecting definitions; `parent` is the enclosing
/// class/impl/function used to qualify methods and attribute call edges.
fn walk(
    map: &mut CodeMap,
    rel: &Path,
    bytes: &[u8],
    node: Node,
    lang: &Lang,
    parent: Option<&str>,
) {
    let kind = node.kind();
    let def: Option<(SymbolKind, Option<String>)> = match (lang, kind) {
        (Lang::Rust, "function_item") | (Lang::Python, "function_definition") => Some((
            if parent.is_some() {
                SymbolKind::Method
            } else {
                SymbolKind::Function
            },
            name_of(node, bytes),
        )),
        (Lang::Rust, "struct_item") => Some((SymbolKind::Struct, name_of(node, bytes))),
        (Lang::Rust, "enum_item") => Some((SymbolKind::Enum, name_of(node, bytes))),
        (Lang::Rust, "trait_item") => Some((SymbolKind::Trait, name_of(node, bytes))),
        (Lang::Rust, "impl_item") => Some((
            SymbolKind::Impl,
            node.child_by_field_name("type")
                .map(|n| text(n, bytes).to_string()),
        )),
        (Lang::Python, "class_definition") | (Lang::Js, "class_declaration") => {
            Some((SymbolKind::Class, name_of(node, bytes)))
        }
        (Lang::Js, "function_declaration" | "generator_function_declaration") => {
            Some((SymbolKind::Function, name_of(node, bytes)))
        }
        (Lang::Js, "method_definition") => Some((SymbolKind::Method, name_of(node, bytes))),
        _ => None,
    };

    // JS: const foo = () => {} / function expressions assigned to names.
    let js_assigned_fn = if matches!((lang, kind), (Lang::Js, "variable_declarator")) {
        let is_fn = node
            .child_by_field_name("value")
            .is_some_and(|v| matches!(v.kind(), "arrow_function" | "function_expression"));
        if is_fn { name_of(node, bytes) } else { None }
    } else {
        None
    };

    let mut new_parent: Option<String> = None;
    if let Some((skind, Some(name))) = def {
        let qualified = match (&skind, parent) {
            (SymbolKind::Method, Some(p)) => format!("{p}.{name}"),
            _ => name.clone(),
        };
        let is_container = matches!(
            skind,
            SymbolKind::Class | SymbolKind::Impl | SymbolKind::Struct | SymbolKind::Trait
        );
        let is_callable = matches!(skind, SymbolKind::Function | SymbolKind::Method);
        map.symbols.push(Symbol {
            name: qualified.clone(),
            kind: skind,
            signature: first_line(text(node, bytes)),
            file: rel.to_path_buf(),
            line: node.start_position().row + 1,
            end_line: node.end_position().row + 1,
        });
        if is_container {
            new_parent = Some(name);
        }
        if is_callable {
            // Record call edges out of this body under the qualified name.
            collect_calls(map, bytes, node, lang, &qualified);
            new_parent = Some(qualified); // nested defs still get qualified
        }
    } else if let Some(name) = js_assigned_fn {
        map.symbols.push(Symbol {
            name: name.clone(),
            kind: SymbolKind::Function,
            signature: first_line(text(node, bytes)),
            file: rel.to_path_buf(),
            line: node.start_position().row + 1,
            end_line: node.end_position().row + 1,
        });
        collect_calls(map, bytes, node, lang, &name);
    }

    let p = new_parent.as_deref().or(parent);
    let mut c = node.walk();
    for child in node.children(&mut c) {
        walk(map, rel, bytes, child, lang, p);
    }
}

/// Collect names invoked inside `node`'s body, keyed by `caller`.
fn collect_calls(map: &mut CodeMap, bytes: &[u8], node: Node, lang: &Lang, caller: &str) {
    let mut stack = vec![node];
    while let Some(n) = stack.pop() {
        let callee_name_opt: Option<String> = match (lang, n.kind()) {
            (Lang::Rust | Lang::Js, "call_expression") => n
                .child_by_field_name("function")
                .map(|f| callee_name(f, bytes)),
            (Lang::Python, "call") => n
                .child_by_field_name("function")
                .map(|f| callee_name(f, bytes)),
            _ => None,
        };
        if let Some(name) = callee_name_opt
            && !name.is_empty()
        {
            map.calls
                .entry(caller.to_string())
                .or_default()
                .insert(name);
        }
        let mut c = n.walk();
        for child in n.children(&mut c) {
            stack.push(child);
        }
    }
}

/// Reduce a call target expression to its final identifier:
/// `mod::foo` -> foo, `obj.method` -> method, `foo` -> foo.
fn callee_name(node: Node, bytes: &[u8]) -> String {
    let t = text(node, bytes);
    let last = t.rsplit(['.', ':']).next().unwrap_or(t).trim();
    // Strip generics / call remnants.
    last.split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .next()
        .unwrap_or("")
        .to_string()
}

/// Keep only call edges whose callee matches a symbol defined in the codebase
/// (by bare name); rewrite callees to their qualified names when unambiguous.
fn resolve_calls(map: &mut CodeMap) {
    let mut by_bare: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for s in &map.symbols {
        if matches!(s.kind, SymbolKind::Function | SymbolKind::Method) {
            let bare = s.name.rsplit('.').next().unwrap_or(&s.name);
            by_bare.entry(bare).or_default().push(&s.name);
        }
    }
    let mut resolved: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (caller, callees) in &map.calls {
        let mut out = BTreeSet::new();
        for callee in callees {
            if let Some(matches) = by_bare.get(callee.as_str()) {
                if matches.len() == 1 {
                    out.insert(matches[0].to_string());
                } else {
                    out.insert(callee.clone()); // ambiguous: keep bare name
                }
            }
        }
        if !out.is_empty() {
            resolved.insert(caller.clone(), out);
        }
    }
    map.calls = resolved;
}

/// True when `needle` occurs in `haystack` delimited by non-identifier
/// characters, so "rust" doesn't match "frustrating" and a function named
/// `run` doesn't match "running".
pub fn contains_word(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    let ident = |c: char| c.is_alphanumeric() || c == '_';
    let mut from = 0;
    while let Some(i) = haystack[from..].find(needle) {
        let i = from + i;
        let j = i + needle.len();
        let before_ok = !haystack[..i].chars().next_back().is_some_and(ident);
        let after_ok = !haystack[j..].chars().next().is_some_and(ident);
        if before_ok && after_ok {
            return true;
        }
        from = j;
    }
    false
}

/// `Client.chat` matches queries "chat" and "Client.chat".
fn name_matches(qualified: &str, query: &str) -> bool {
    qualified == query || qualified.ends_with(&format!(".{query}"))
}

impl CodeMap {
    /// Symbols grouped by file, impl blocks dropped (their methods already
    /// carry the impl name as a prefix).
    fn by_file(&self) -> BTreeMap<&Path, Vec<&Symbol>> {
        let mut by_file: BTreeMap<&Path, Vec<&Symbol>> = BTreeMap::new();
        for s in &self.symbols {
            if matches!(s.kind, SymbolKind::Impl) {
                continue;
            }
            by_file.entry(&s.file).or_default().push(s);
        }
        by_file
    }

    fn file_line(file: &Path, syms: &[&Symbol]) -> String {
        let packed = syms
            .iter()
            .map(|s| match s.kind {
                // Functions/methods are the unmarked default; only the rarer
                // kinds pay for a label.
                SymbolKind::Function | SymbolKind::Method => format!("{}@{}", s.name, s.line),
                _ => format!("{} {}@{}", s.kind.short(), s.name, s.line),
            })
            .collect::<Vec<_>>()
            .join(", ");
        format!("{}: {packed}\n", file.display())
    }

    /// Best-practice keyword tags for every language present in the codebase,
    /// derived from file extensions. Used to select language guidance by what
    /// the project *is*, rather than by whether the task text happens to name
    /// the language (it almost never does).
    pub fn languages(&self) -> Vec<&'static str> {
        let mut tags: Vec<&'static str> = Vec::new();
        for s in &self.symbols {
            let tag = match lang_for(&s.file) {
                Some(Lang::Rust) => "rust",
                Some(Lang::Python) => "python",
                Some(Lang::Js | Lang::Html) => "javascript",
                None => continue,
            };
            if !tags.contains(&tag) {
                tags.push(tag);
            }
        }
        tags
    }

    /// Compact brief: one packed line per file.
    /// Designed to be the cheapest faithful overview of a codebase.
    pub fn brief(&self) -> String {
        self.by_file()
            .into_iter()
            .map(|(file, syms)| Self::file_line(file, &syms))
            .collect()
    }

    /// Brief sliced to what `text` (task + plan) mentions: files whose path
    /// or symbols appear in the text get full listings, the rest contribute
    /// only their file name so the overall shape stays visible.
    pub fn brief_for(&self, text: &str) -> String {
        let text = text.to_lowercase();
        let mut out = String::new();
        for (file, syms) in self.by_file() {
            let path = file.display().to_string().to_lowercase();
            let stem = file
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_lowercase();
            let relevant = text.contains(&path)
                || (!stem.is_empty() && text.contains(&stem))
                || syms.iter().any(|s| {
                    let bare = s.name.rsplit('.').next().unwrap_or(&s.name);
                    // Short names (run, new, map, ...) match everything and
                    // would silently inflate the slice back to the full brief.
                    bare.len() >= 4 && contains_word(&text, &bare.to_lowercase())
                });
            if relevant {
                out.push_str(&Self::file_line(file, &syms));
            } else {
                writeln!(out, "{}", file.display()).unwrap();
            }
        }
        out
    }

    /// Full detail for a subset of symbols (signatures + edges).
    pub fn detail(&self, name: &str) -> String {
        let mut out = String::new();
        for s in self
            .symbols
            .iter()
            .filter(|s| s.name == name || s.name.ends_with(&format!(".{name}")))
        {
            write!(
                out,
                "{} {} @ {}:{}\n  {}\n",
                s.kind.short(),
                s.name,
                s.file.display(),
                s.line,
                s.signature
            )
            .unwrap();
            if let Some(callees) = self.calls.get(&s.name) {
                writeln!(
                    out,
                    "  calls: {}",
                    callees.iter().cloned().collect::<Vec<_>>().join(", ")
                )
                .unwrap();
            }
            let callers = self.callers_of(&s.name);
            if !callers.is_empty() {
                writeln!(out, "  called-by: {}", callers.join(", ")).unwrap();
            }
        }
        if out.is_empty() {
            out = format!("no symbol named '{name}'");
        }
        out
    }

    pub fn callers_of(&self, name: &str) -> Vec<String> {
        self.calls
            .iter()
            .filter(|(_, callees)| callees.iter().any(|c| name_matches(c, name)))
            .map(|(caller, _)| caller.clone())
            .collect()
    }

    pub fn callees_of(&self, name: &str) -> Vec<String> {
        self.calls
            .iter()
            .filter(|(caller, _)| name_matches(caller, name))
            .flat_map(|(_, callees)| callees.iter().cloned())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn syntax_error_line_flags_broken_rust() {
        let path = std::path::Path::new("x.rs");
        assert_eq!(
            super::syntax_error_line(path, "fn ok() { let a = 1; }"),
            None
        );
        assert!(super::syntax_error_line(path, "fn broken( { let a = ; }").is_some());
        // no grammar -> no verdict
        assert_eq!(
            super::syntax_error_line(std::path::Path::new("x.txt"), "anything"),
            None
        );
    }

    #[test]
    fn contains_word_respects_identifier_boundaries() {
        assert!(super::contains_word("update the rust tests", "rust"));
        assert!(super::contains_word("rust", "rust"));
        assert!(super::contains_word("fix codemap.rs build", "build"));
        assert!(!super::contains_word("frustrating", "rust"));
        assert!(!super::contains_word("running the suite", "run"));
        assert!(!super::contains_word("my_run_helper", "run"));
        assert!(!super::contains_word("anything", ""));
    }
}
