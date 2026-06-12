//! Codebase summarizer: extracts symbols and a function-call graph using
//! tree-sitter, then renders ultra-compact briefs for LLM consumption.
//! Deterministic — costs zero tokens to build.

use anyhow::{Context, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
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
    fn short(&self) -> &'static str {
        match self {
            SymbolKind::Function => "fn",
            SymbolKind::Method => "method",
            SymbolKind::Struct => "struct",
            SymbolKind::Enum => "enum",
            SymbolKind::Trait => "trait",
            SymbolKind::Class => "class",
            SymbolKind::Impl => "impl",
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
    /// Enclosing class/impl name, if any.
    pub parent: Option<String>,
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

pub fn build(root: &Path) -> Result<CodeMap> {
    build_limited(root, usize::MAX)
}

/// Like `build` but stops after `max_files` source files — used when
/// scanning third-party dependencies, which can be arbitrarily large.
pub fn build_limited(root: &Path, max_files: usize) -> Result<CodeMap> {
    let mut map = CodeMap::default();
    let mut seen = 0usize;
    for entry in WalkDir::new(root)
        .into_iter()
        .filter_entry(|e| {
            !e.file_name()
                .to_str()
                .map(|n| SKIP_DIRS.contains(&n))
                .unwrap_or(false)
        })
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
    {
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
        let src = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(_) => continue, // non-utf8 etc.
        };
        let rel = path.strip_prefix(root).unwrap_or(path).to_path_buf();
        index_file(&mut map, &rel, &src, lang)?;
    }
    resolve_calls(&mut map);
    Ok(map)
}

fn ts_language(lang: &Lang) -> Language {
    match lang {
        Lang::Rust => tree_sitter_rust::LANGUAGE.into(),
        Lang::Python => tree_sitter_python::LANGUAGE.into(),
        Lang::Js => tree_sitter_javascript::LANGUAGE.into(),
        Lang::Html => tree_sitter_html::LANGUAGE.into(),
    }
}

fn index_file(map: &mut CodeMap, rel: &Path, src: &str, lang: Lang) -> Result<()> {
    if let Lang::Html = lang {
        // Parse HTML only to pull out <script> bodies, then index those as JS.
        let mut parser = Parser::new();
        parser
            .set_language(&ts_language(&Lang::Html))
            .context("html grammar")?;
        if let Some(tree) = parser.parse(src, None) {
            collect_scripts(tree.root_node(), src.as_bytes(), &mut |js| {
                let _ = index_file(map, rel, js, Lang::Js);
            });
        }
        return Ok(());
    }

    let mut parser = Parser::new();
    parser
        .set_language(&ts_language(&lang))
        .context("grammar")?;
    let Some(tree) = parser.parse(src, None) else {
        return Ok(());
    };
    let bytes = src.as_bytes();
    walk(map, rel, bytes, tree.root_node(), &lang, None);
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
        (Lang::Rust, "function_item") => Some((
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
        (Lang::Python, "function_definition") => Some((
            if parent.is_some() {
                SymbolKind::Method
            } else {
                SymbolKind::Function
            },
            name_of(node, bytes),
        )),
        (Lang::Python, "class_definition") => Some((SymbolKind::Class, name_of(node, bytes))),
        (Lang::Js, "function_declaration") | (Lang::Js, "generator_function_declaration") => {
            Some((SymbolKind::Function, name_of(node, bytes)))
        }
        (Lang::Js, "method_definition") => Some((SymbolKind::Method, name_of(node, bytes))),
        (Lang::Js, "class_declaration") => Some((SymbolKind::Class, name_of(node, bytes))),
        _ => None,
    };

    // JS: const foo = () => {} / function expressions assigned to names.
    let js_assigned_fn = if let (Lang::Js, "variable_declarator") = (lang, kind) {
        let is_fn = node
            .child_by_field_name("value")
            .map(|v| matches!(v.kind(), "arrow_function" | "function_expression"))
            .unwrap_or(false);
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
            parent: parent.map(|s| s.to_string()),
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
            parent: parent.map(|s| s.to_string()),
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
        let callee: Option<String> = match (lang, n.kind()) {
            (Lang::Rust, "call_expression") | (Lang::Js, "call_expression") => n
                .child_by_field_name("function")
                .map(|f| callee_name(f, bytes)),
            (Lang::Python, "call") => n
                .child_by_field_name("function")
                .map(|f| callee_name(f, bytes)),
            _ => None,
        };
        if let Some(name) = callee
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

/// `Client.chat` matches queries "chat" and "Client.chat".
fn name_matches(qualified: &str, query: &str) -> bool {
    qualified == query || qualified.ends_with(&format!(".{query}"))
}

impl CodeMap {
    /// Compact brief: one line per file, then symbols grouped by file.
    /// Designed to be the cheapest faithful overview of a codebase.
    pub fn brief(&self) -> String {
        let mut by_file: BTreeMap<&Path, Vec<&Symbol>> = BTreeMap::new();
        for s in &self.symbols {
            by_file.entry(&s.file).or_default().push(s);
        }
        let mut out = String::new();
        for (file, syms) in by_file {
            out.push_str(&format!("{}\n", file.display()));
            for s in syms {
                out.push_str(&format!("  {} {} L{}\n", s.kind.short(), s.name, s.line));
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
            out.push_str(&format!(
                "{} {} @ {}:{}\n  {}\n",
                s.kind.short(),
                s.name,
                s.file.display(),
                s.line,
                s.signature
            ));
            if let Some(callees) = self.calls.get(&s.name) {
                out.push_str(&format!(
                    "  calls: {}\n",
                    callees.iter().cloned().collect::<Vec<_>>().join(", ")
                ));
            }
            let callers = self.callers_of(&s.name);
            if !callers.is_empty() {
                out.push_str(&format!("  called-by: {}\n", callers.join(", ")));
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
