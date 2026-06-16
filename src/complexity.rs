//! `scrooge complexity`: cognitive complexity of every Rust function, hottest
//! first. Pure analysis, no LLM, free. Uses the `complexity` crate, which
//! scores syn 1.0 AST nodes, so this covers Rust only (the one language we can
//! parse with syn).

use std::path::Path;

use std::fmt::Write as _;

use complexity::Complexity as _;
use syn::visit::Visit;

/// One scored function: `Type::method` or `function`, its file, and score.
struct Func {
    name: String,
    file: String,
    score: u32,
}

/// Walks a parsed file collecting cognitive-complexity scores for every free
/// function and inherent/trait method, tracking the enclosing `impl` type so
/// methods read as `Type::method`.
struct Collector<'a> {
    file: &'a str,
    ty: Option<String>,
    out: &'a mut Vec<Func>,
}

impl<'ast> Visit<'ast> for Collector<'_> {
    fn visit_item_fn(&mut self, node: &'ast syn::ItemFn) {
        self.out.push(Func {
            name: node.sig.ident.to_string(),
            file: self.file.to_string(),
            score: node.complexity(),
        });
        syn::visit::visit_item_fn(self, node);
    }

    fn visit_item_impl(&mut self, node: &'ast syn::ItemImpl) {
        let prev = self.ty.take();
        self.ty = type_name(&node.self_ty);
        syn::visit::visit_item_impl(self, node);
        self.ty = prev;
    }

    fn visit_impl_item_method(&mut self, node: &'ast syn::ImplItemMethod) {
        let method = node.sig.ident.to_string();
        let name = match &self.ty {
            Some(ty) => format!("{ty}::{method}"),
            None => method,
        };
        self.out.push(Func {
            name,
            file: self.file.to_string(),
            score: node.complexity(),
        });
        syn::visit::visit_impl_item_method(self, node);
    }
}

/// Best-effort rendering of an `impl <Type>` target into a short name.
fn type_name(ty: &syn::Type) -> Option<String> {
    match ty {
        syn::Type::Path(p) => p.path.segments.last().map(|s| s.ident.to_string()),
        _ => None,
    }
}

/// Score every Rust function under `root`, hottest first.
pub fn report(root: &Path) -> Vec<(String, String, u32)> {
    let mut funcs = Vec::new();
    for entry in crate::codemap::source_files(root) {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let Ok(src) = std::fs::read_to_string(path) else {
            continue;
        };
        let Ok(parsed) = syn::parse_file(&src) else {
            // Skip files syn can't parse rather than failing the whole run.
            continue;
        };
        let rel = path
            .strip_prefix(root)
            .unwrap_or(path)
            .display()
            .to_string();
        let mut collector = Collector {
            file: &rel,
            ty: None,
            out: &mut funcs,
        };
        collector.visit_file(&parsed);
    }
    funcs.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.name.cmp(&b.name)));
    funcs
        .into_iter()
        .map(|f| (f.name, f.file, f.score))
        .collect()
}

/// Render the top `limit` functions (or all of them) as an aligned table.
pub fn render(funcs: &[(String, String, u32)], limit: usize) -> String {
    let shown = &funcs[..funcs.len().min(limit)];
    if shown.is_empty() {
        return "no Rust functions found\n".to_string();
    }
    let name_w = shown.iter().map(|(n, ..)| n.len()).max().unwrap_or(0);
    let mut out = String::new();
    for (name, file, score) in shown {
        writeln!(out, "{score:>5}  {name:<name_w$}  {file}").unwrap();
    }
    if funcs.len() > shown.len() {
        write!(
            out,
            "\n... {} more (use --top 0 to show all)\n",
            funcs.len() - shown.len()
        )
        .unwrap();
    }
    out
}
