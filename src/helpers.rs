//! Generic-utility extraction. Finds helper functions in the repo and in all
//! its dependencies (cargo registry sources, python site-packages,
//! `node_modules`) using heuristics — cross-file fan-in, utility-shaped names,
//! small bodies, public visibility — so agents can reuse instead of reinvent.
//! Candidates can be validated/annotated by Cratchit and are cached on disk.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write;
use std::path::{Path, PathBuf};

use crate::codemap::{self, CodeMap, SymbolKind};

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Helper {
    /// `"repo"` or the dependency name (e.g. `"serde_json"`, `"lodash"`).
    pub origin: String,
    pub name: String,
    pub signature: String,
    /// Absolute for dependencies, repo-relative for the repo itself.
    pub file: String,
    pub line: usize,
    /// Distinct files that call it (within its own codebase).
    pub fan_in: usize,
    pub score: u32,
    /// One-line purpose, filled in by Cratchit validation.
    pub purpose: Option<String>,
}

/// Name fragments that smell like a generic utility.
const UTIL_NAMES: &[&str] = &[
    "is_",
    "has_",
    "to_",
    "from_",
    "parse",
    "format",
    "norm",
    "escape",
    "unescape",
    "trim",
    "split",
    "join_",
    "merge",
    "convert",
    "encode",
    "decode",
    "clamp",
    "strip",
    "slug",
    "camel",
    "snake",
    "kebab",
    "dedup",
    "flatten",
    "chunk",
    "retry",
    "memo",
    "truncat",
    "sanitize",
    "valid",
    "util",
    "helper",
    "wrap_",
    "uniq",
    "deep_",
    "pluck",
    "pad_",
    "capitalize",
    "pluralize",
    "slugify",
];

const MAX_HELPER_LINES: usize = 100;
const MAX_FILES_PER_DEP: usize = 300;
const MAX_HELPERS_PER_DEP: usize = 25;

fn name_score(name: &str) -> u32 {
    let lower = name.to_lowercase();
    let bare = lower.rsplit('.').next().unwrap_or(&lower);
    u32::from(UTIL_NAMES.iter().any(|p| bare.contains(p))) * 2
}

/// Score every function in a map; `public_only` is used for dependencies,
/// where private items can't be reused anyway.
fn score_map(origin: &str, map: &CodeMap, base: &Path, public_only: bool) -> Vec<Helper> {
    // callee name -> distinct caller files
    let file_of: BTreeMap<&str, &Path> = map
        .symbols
        .iter()
        .map(|s| (s.name.as_str(), s.file.as_path()))
        .collect();
    let mut fan_in: BTreeMap<&str, BTreeSet<&Path>> = BTreeMap::new();
    for (caller, callees) in &map.calls {
        let Some(cf) = file_of.get(caller.as_str()) else {
            continue;
        };
        for callee in callees {
            fan_in.entry(callee).or_default().insert(cf);
        }
    }

    let mut out = Vec::new();
    for s in &map.symbols {
        if !matches!(s.kind, SymbolKind::Function) {
            continue; // methods need an instance — not drop-in reusable
        }
        let bare_name = s.name.rsplit('.').next().unwrap_or(&s.name);
        if bare_name.starts_with('_') || bare_name == "main" || bare_name.starts_with("test") {
            continue;
        }
        if s.end_line.saturating_sub(s.line) > MAX_HELPER_LINES {
            continue; // helpers are small
        }
        // Visibility: rust needs `pub`; python/js use the underscore check above.
        let is_rust = s.file.extension().is_some_and(|e| e == "rs");
        if public_only && is_rust && !s.signature.starts_with("pub ") {
            continue;
        }
        let files = fan_in.get(s.name.as_str()).map_or(0, BTreeSet::len);
        // Single-file usage is not evidence of genericity — only cross-file is.
        let cross = if files >= 2 {
            u32::try_from(files.min(5)).unwrap_or(u32::MAX) * 2
        } else {
            0
        };
        let score = cross + name_score(&s.name);
        // Keep if used across files OR clearly utility-named.
        if score < 2 {
            continue;
        }
        out.push(Helper {
            origin: origin.to_string(),
            name: s.name.clone(),
            signature: s.signature.clone(),
            file: base.join(&s.file).display().to_string(),
            line: s.line,
            fan_in: files,
            score,
            purpose: None,
        });
    }
    out.sort_by(|a, b| b.score.cmp(&a.score));
    out
}

pub fn repo_helpers(root: &Path) -> Result<Vec<Helper>> {
    let map = codemap::build(root)?;
    Ok(score_map("repo", &map, Path::new(""), false))
}

pub fn dep_helpers(root: &Path) -> Vec<Helper> {
    let mut deps: Vec<(String, PathBuf)> = Vec::new();
    deps.extend(rust_deps(root).unwrap_or_default());
    deps.extend(python_deps(root));
    deps.extend(js_deps(root));

    let mut out = Vec::new();
    for (name, dir) in deps {
        let Ok(map) = codemap::build_limited(&dir, MAX_FILES_PER_DEP) else {
            continue;
        };
        let mut helpers = score_map(&name, &map, &dir, true);
        helpers.truncate(MAX_HELPERS_PER_DEP);
        out.extend(helpers);
    }
    out
}

/// (name, path) for each immediate subdirectory of `dir`, skipping entries
/// whose name isn't valid UTF-8. Empty if `dir` can't be read.
fn dir_subdirs(dir: &Path) -> Vec<(String, PathBuf)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|e| {
            let p = e.path();
            let name = p.file_name()?.to_str()?.to_string();
            p.is_dir().then_some((name, p))
        })
        .collect()
}

/// Cargo dependencies via `cargo metadata`: every non-workspace package's
/// source directory in the registry checkout.
fn rust_deps(root: &Path) -> Result<Vec<(String, PathBuf)>> {
    if !root.join("Cargo.toml").exists() {
        return Ok(vec![]);
    }
    let out = std::process::Command::new("cargo")
        .args(["metadata", "--format-version", "1"])
        .current_dir(root)
        .output()
        .context("running cargo metadata")?;
    let v: serde_json::Value = serde_json::from_slice(&out.stdout)?;
    let members: BTreeSet<&str> = v["workspace_members"]
        .as_array()
        .map(|a| a.iter().filter_map(|m| m.as_str()).collect())
        .unwrap_or_default();
    let mut deps = Vec::new();
    for pkg in v["packages"].as_array().unwrap_or(&vec![]) {
        let id = pkg["id"].as_str().unwrap_or("");
        if members.contains(id) {
            continue;
        }
        let (Some(name), Some(manifest)) = (pkg["name"].as_str(), pkg["manifest_path"].as_str())
        else {
            continue;
        };
        let src = Path::new(manifest)
            .parent()
            .unwrap_or_else(|| Path::new("/"))
            .join("src");
        if src.is_dir() {
            deps.push((name.to_string(), src));
        }
    }
    Ok(deps)
}

/// Python dependencies: site-packages of the project venv if present,
/// otherwise the interpreter's site-packages.
fn python_deps(root: &Path) -> Vec<(String, PathBuf)> {
    let mut site_dirs: Vec<PathBuf> = Vec::new();
    for venv in ["venv", ".venv"] {
        if let Ok(entries) = std::fs::read_dir(root.join(venv).join("lib")) {
            for e in entries.flatten() {
                let sp = e.path().join("site-packages");
                if sp.is_dir() {
                    site_dirs.push(sp);
                }
            }
        }
    }
    // Only fall back to the global interpreter if the project actually uses python.
    let uses_python = root.join("requirements.txt").exists()
        || root.join("pyproject.toml").exists()
        || root.join("setup.py").exists();
    if site_dirs.is_empty()
        && uses_python
        && let Ok(out) = std::process::Command::new("python3")
            .args([
                "-c",
                "import site, json; print(json.dumps(site.getsitepackages()))",
            ])
            .output()
        && let Ok(v) = serde_json::from_slice::<Vec<String>>(&out.stdout)
    {
        site_dirs.extend(v.into_iter().map(PathBuf::from));
    }
    let skip = [
        "pip",
        "setuptools",
        "wheel",
        "pkg_resources",
        "_distutils_hack",
    ];
    let mut deps = Vec::new();
    for sp in site_dirs {
        for (name, p) in dir_subdirs(&sp) {
            if name.contains("dist-info") || name.starts_with('_') || skip.contains(&name.as_str())
            {
                continue;
            }
            deps.push((name, p));
        }
    }
    deps
}

/// JS dependencies: top-level packages in `node_modules` (including @scopes).
fn js_deps(root: &Path) -> Vec<(String, PathBuf)> {
    let nm = root.join("node_modules");
    let mut deps = Vec::new();
    for (name, p) in dir_subdirs(&nm) {
        if name == ".bin" {
            continue;
        }
        if let Some(scope) = name.strip_prefix('@') {
            for (sub, sp) in dir_subdirs(&p) {
                deps.push((format!("@{scope}/{sub}"), sp));
            }
        } else {
            deps.push((name, p));
        }
    }
    deps
}

// --- cache ---

pub fn cache_path(root: &Path) -> PathBuf {
    root.join(".scrooge").join("helpers.json")
}

pub fn load_cache(root: &Path) -> Option<Vec<Helper>> {
    let data = std::fs::read_to_string(cache_path(root)).ok()?;
    serde_json::from_str(&data).ok()
}

pub fn save_cache(root: &Path, helpers: &[Helper]) -> Result<()> {
    let mut seen = BTreeSet::new();
    let deduped: Vec<&Helper> = helpers
        .iter()
        .filter(|h| seen.insert((h.origin.clone(), h.name.clone(), h.file.clone())))
        .collect();
    let path = cache_path(root);
    std::fs::create_dir_all(path.parent().unwrap())?;
    std::fs::write(&path, serde_json::to_string_pretty(&deduped)?)?;
    Ok(())
}

/// The `helpers` tool body, shared by the toolbox and the MCP server:
/// validated cache entries (full scans run via `scrooge helpers --deps`)
/// topped up with a fresh repo scan — so helpers written after the cache was
/// built still show up — narrowed by an optional substring filter.
pub fn filtered_listing(root: &Path, filter: &str) -> Result<String> {
    let mut list = load_cache(root).unwrap_or_default();
    let known: BTreeSet<(String, String)> = list
        .iter()
        .map(|h| (h.name.clone(), h.file.clone()))
        .collect();
    list.extend(
        repo_helpers(root)?
            .into_iter()
            .filter(|h| !known.contains(&(h.name.clone(), h.file.clone()))),
    );
    let filter = filter.to_lowercase();
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
    if filtered.is_empty() {
        return Ok("no matching helpers found".into());
    }
    Ok(render(&filtered))
}

/// Compact one-line-per-helper rendering for LLM consumption.
pub fn render(helpers: &[Helper]) -> String {
    let mut out = String::new();
    for h in helpers {
        write!(
            out,
            "[{}] {} — {} ({}:{}, used in {} files)",
            h.origin, h.name, h.signature, h.file, h.line, h.fan_in
        )
        .unwrap();
        if let Some(p) = &h.purpose {
            write!(out, " :: {p}").unwrap();
        }
        out.push('\n');
    }
    out
}
