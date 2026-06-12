//! Post-task verification pass: autoformat, run tests, autofix + report lint
//! warnings. Per-language commands default from what the repo contains and
//! live in .scrooge/checks.toml so the user (or the agents) can edit them.
//! Driven by the plugin's Stop hook: test failures are fed back to Scrooge,
//! leftover warnings are delegated to Cratchit.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Serialize, Deserialize, Clone, Default)]
pub struct LangChecks {
    /// Applied silently before anything else; never blocks.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
    /// Build + tests. Non-zero exit = errors, fed back to Scrooge.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub test: Option<String>,
    /// Mechanical warning autofix, run before `lint`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lint_fix: Option<String>,
    /// Non-zero exit = warnings remain, delegated to Cratchit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lint: Option<String>,
}

pub struct Report {
    /// Test/build failures per language.
    pub errors: Vec<(String, String)>,
    /// Lint warnings per language (after autofix).
    pub warnings: Vec<(String, String)>,
}

const MAX_OUTPUT_CHARS: usize = 3000;

pub fn config_path(root: &Path) -> PathBuf {
    root.join(".scrooge").join("checks.toml")
}

/// Built-in defaults for whatever languages the repo actually contains.
fn defaults(root: &Path) -> BTreeMap<String, LangChecks> {
    let mut map = BTreeMap::new();
    if root.join("Cargo.toml").exists() {
        map.insert(
            "rust".into(),
            LangChecks {
                format: Some("cargo fmt".into()),
                test: Some("cargo test --quiet".into()),
                lint_fix: Some(
                    "cargo clippy --fix --allow-dirty --allow-staged --quiet 2>/dev/null".into(),
                ),
                lint: Some("cargo clippy --quiet -- -D warnings".into()),
            },
        );
    }
    if root.join("pyproject.toml").exists()
        || root.join("setup.py").exists()
        || root.join("requirements.txt").exists()
    {
        map.insert(
            "python".into(),
            LangChecks {
                format: Some("ruff format .".into()),
                test: Some("pytest -q".into()),
                lint_fix: Some("ruff check --fix --quiet .".into()),
                lint: Some("ruff check .".into()),
            },
        );
    }
    if root.join("package.json").exists() {
        map.insert(
            "javascript".into(),
            LangChecks {
                format: Some("npx --no-install prettier --log-level warn --write .".into()),
                test: Some("npm test --silent".into()),
                lint_fix: Some("npx --no-install eslint --fix --quiet . || true".into()),
                lint: Some("npx --no-install eslint .".into()),
            },
        );
    }
    map
}

/// Load .scrooge/checks.toml, writing it from the built-in defaults first if
/// it doesn't exist yet (so there is always a file to edit).
pub fn load(root: &Path) -> Result<BTreeMap<String, LangChecks>> {
    let path = config_path(root);
    if !path.exists() {
        let defaults = defaults(root);
        std::fs::create_dir_all(path.parent().unwrap())?;
        let header = "# Per-language check commands run by `scrooge check` after each task.\n\
                      # Edit freely (agents may too). Remove a key to skip that stage.\n\n";
        std::fs::write(
            &path,
            format!("{header}{}", toml::to_string_pretty(&defaults)?),
        )?;
        return Ok(defaults);
    }
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

fn run_cmd(root: &Path, cmd: &str) -> (bool, String) {
    let out = Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .current_dir(root)
        .output();
    match out {
        Ok(o) => {
            let mut text = String::from_utf8_lossy(&o.stdout).into_owned();
            text.push_str(&String::from_utf8_lossy(&o.stderr));
            (o.status.success(), text)
        }
        Err(e) => (false, format!("failed to spawn `{cmd}`: {e}")),
    }
}

/// Keep the tail (where test failures and error summaries end up).
fn truncate(s: &str) -> String {
    let s = s.trim();
    if s.len() <= MAX_OUTPUT_CHARS {
        return s.to_string();
    }
    // Back off to a char boundary: tool output is full of multibyte
    // punctuation (rustc’s quotes, arrows) and a mid-char slice panics.
    let mut start = s.len() - MAX_OUTPUT_CHARS;
    while !s.is_char_boundary(start) {
        start -= 1;
    }
    let start = s[start..].find('\n').map_or(start, |i| start + i + 1);
    format!("[... truncated ...]\n{}", &s[start..])
}

/// Format everything, then run tests; only if all tests pass, autofix and
/// report lint warnings (warnings are pointless noise while the build is red).
pub fn run(root: &Path) -> Result<Report> {
    let cfg = load(root)?;
    let mut report = Report {
        errors: Vec::new(),
        warnings: Vec::new(),
    };
    for (lang, checks) in &cfg {
        if let Some(cmd) = &checks.format {
            run_cmd(root, cmd);
        }
        if let Some(cmd) = &checks.test {
            let (ok, out) = run_cmd(root, cmd);
            if !ok {
                report.errors.push((lang.clone(), truncate(&out)));
            }
        }
    }
    if !report.errors.is_empty() {
        return Ok(report);
    }
    for (lang, checks) in &cfg {
        if let Some(cmd) = &checks.lint_fix {
            run_cmd(root, cmd);
        }
        if let Some(cmd) = &checks.lint {
            let (ok, out) = run_cmd(root, cmd);
            if !ok {
                report.warnings.push((lang.clone(), truncate(&out)));
            }
        }
    }
    Ok(report)
}

pub fn render(report: &Report) -> String {
    let mut s = String::new();
    for (lang, out) in &report.errors {
        s.push_str(&format!("== {lang}: tests/build FAILED ==\n{out}\n"));
    }
    for (lang, out) in &report.warnings {
        s.push_str(&format!(
            "== {lang}: warnings remain after autofix ==\n{out}\n"
        ));
    }
    if s.is_empty() {
        s.push_str("all checks clean\n");
    }
    s
}
