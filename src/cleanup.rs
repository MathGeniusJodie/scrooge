//! `scrooge cleanup`: hunt down the humbugs and put them right. Runs the full
//! check suite (which already applies the mechanical autofixes), then hands the
//! leftover problems to Cratchit one at a time, re-verifying after each fix.
//!
//! It is also the gate `run`/`cratchit` pass through: a task should not start on
//! top of a red tree, so those entry points refuse to proceed while checks are
//! dirty and offer to run cleanup first.

use anyhow::Result;
use std::io::Write;
use std::path::Path;

use crate::agents::Orchestrator;
use crate::checks;

/// Bold red, the one shout Scrooge himself could not ignore (matches the
/// sandbox preflight warning).
const BOLD_RED: &str = "\x1b[1;31m";
const RESET: &str = "\x1b[0m";

/// Run the full check suite and return the leftover problems (errors first,
/// then warnings) as `(label, detail)` pairs. The suite applies the mechanical
/// autofixes itself, so what comes back here is only what a human/agent must
/// still fix by hand.
fn outstanding(root: &Path) -> Result<Vec<(String, String)>> {
    let report = checks::run(root)?;
    Ok(report.errors.into_iter().chain(report.warnings).collect())
}

/// Split one language's check output into individual diagnostics, so cleanup
/// can hand Cratchit a single problem per delegation. `checks::run` reports a
/// whole tool-output blob per language, and clippy/ruff/biome each pack many
/// separate warnings into that blob — fixing them one at a time, re-verifying
/// between, is the whole point of cleanup. Falls back to the blob unsplit when
/// the format isn't recognised, so a novel tool degrades to the old
/// all-at-once behaviour rather than dropping problems on the floor.
fn split_diagnostics(label: &str, detail: &str) -> Vec<String> {
    let items = match label {
        "rust" => split_clippy(detail),
        "python" => split_ruff(detail),
        "javascript" => split_biome(detail),
        "structure" => split_structure(detail),
        // Test/build failures (a red tree) are a single fix-the-build task, not
        // a list; hand them over whole.
        _ => Vec::new(),
    };
    if items.is_empty() {
        vec![detail.to_string()]
    } else {
        items
    }
}

/// Clippy emits one diagnostic per blank-line-separated block; the trailing
/// per-crate summary (`generated N warnings`, `could not compile`) is dropped.
fn split_clippy(detail: &str) -> Vec<String> {
    detail
        .split("\n\n")
        .map(str::trim)
        .filter(|block| {
            let first = block.lines().next().unwrap_or("");
            (first.starts_with("warning:") || first.starts_with("error:"))
                && !first.contains("generated ")
                && !first.contains("could not compile")
                && !first.contains("aborting due to")
        })
        .map(str::to_string)
        .collect()
}

/// Ruff prints one `path:line:col: CODE message` header per diagnostic, with
/// any context/help lines indented beneath it. Each header starts a new item;
/// the trailing `Found N errors` / `[*] fixable` summary lines are skipped.
fn split_ruff(detail: &str) -> Vec<String> {
    let is_header = |l: &str| {
        let mut parts = l.splitn(4, ':');
        parts.next().is_some_and(|p| !p.is_empty())
            && parts.next().is_some_and(|p| p.parse::<u32>().is_ok())
            && parts.next().is_some_and(|p| p.parse::<u32>().is_ok())
    };
    let mut out: Vec<String> = Vec::new();
    for line in detail.lines() {
        if is_header(line) {
            out.push(line.to_string());
        } else if line.starts_with("Found ") || line.starts_with("[*]") {
            // summary lines: skip
        } else if let Some(last) = out.last_mut() {
            last.push('\n');
            last.push_str(line);
        }
    }
    out
}

/// Biome opens each diagnostic with a `path:line:col category ━━━` rule line;
/// split before every such line, dropping any preamble before the first.
fn split_biome(detail: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in detail.lines() {
        if line.contains('━') && line.contains(':') {
            out.push(String::new());
        }
        if let Some(last) = out.last_mut() {
            if !last.is_empty() {
                last.push('\n');
            }
            last.push_str(line);
        }
    }
    out.into_iter().filter(|s| !s.trim().is_empty()).collect()
}

/// The structure warning is an instruction header followed by one
/// `path: N lines` line per oversized file; make each file its own task,
/// repeating the header so the delegation keeps its context.
fn split_structure(detail: &str) -> Vec<String> {
    let mut lines = detail.lines();
    let header = lines.next().unwrap_or("").trim_end_matches(':');
    lines
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(|l| format!("{header}\n{l}"))
        .collect()
}

/// Re-run the checks and return the next single problem to fix, if any. Each
/// language blob is split into individual diagnostics; we only ever take the
/// first, because the tree changes under us as Cratchit works (a fix may
/// resolve or introduce others), so the list is recomputed every round rather
/// than trusted from a stale snapshot.
fn next_problem(root: &Path) -> Result<Option<(String, String)>> {
    for (label, detail) in outstanding(root)? {
        if let Some(first) = split_diagnostics(&label, &detail).into_iter().next() {
            return Ok(Some((label, first)));
        }
    }
    Ok(None)
}

/// `scrooge cleanup`: autofix via the check suite, then loop — re-checking the
/// tree and handing Cratchit the next remaining problem one at a time — until
/// nothing is left to fix. Accepts a borrowed orchestrator so callers (CLI,
/// MCP) can share one rather than each spinning up their own.
pub async fn cleanup(orch: &mut Orchestrator, root: &Path) -> Result<()> {
    if next_problem(root)?.is_none() {
        println!("God bless us, every one! (all checks clean)");
        return Ok(());
    }
    eprintln!("Bah! Humbug! Problems remain after autofix — handing them to Cratchit one by one.");
    let mut round = 0;
    while let Some((label, detail)) = next_problem(root)? {
        round += 1;
        eprintln!("--- cleanup #{round} ({label}) ---");
        // `detail` is the task (it carries the file paths the code map slices
        // on); the plan is just the directive, so the diagnostic isn't repeated
        // verbatim in both the TASK and SCROOGE'S INSTRUCTIONS sections.
        let plan = format!(
            "A check has flagged the {label} problem shown in the task above. \
             Fix it (and only it), then verify."
        );
        let out = orch.delegate_one(&detail, &plan).await?;
        println!("{out}");
    }
    println!("God bless us, every one! (all checks clean)");
    print!("{}", orch.wages_footer());
    println!();
    Ok(())
}

/// CLI entry point: create an orchestrator and delegate to [`cleanup`].
pub async fn run(root: &Path) -> Result<()> {
    let mut orch = Orchestrator::new(root.to_path_buf())?;
    cleanup(&mut orch, root).await
}

/// Gate for `run`/`cratchit`: if checks are dirty, shout in bold red, then ask
/// whether to run cleanup now. Returns `true` if the caller may proceed (checks
/// were clean, or cleanup ran), `false` if the user declined.
pub async fn ensure_clean_or_prompt(root: &Path) -> Result<bool> {
    if outstanding(root)?.is_empty() {
        return Ok(true);
    }
    eprintln!(
        "{BOLD_RED}Bah! Humbug! The checks are not clean.\n\
         Run `scrooge cleanup` before proceeding.{RESET}"
    );
    if prompt_yes_no("Run `scrooge cleanup` now? [y/N] ")? {
        run(root).await?;
        Ok(true)
    } else {
        Ok(false)
    }
}

/// Read a single y/n answer from the terminal. Anything but `y`/`yes` is a no
/// (including EOF when stdin is not a terminal), so a piped/non-interactive run
/// fails safe rather than hanging.
fn prompt_yes_no(prompt: &str) -> Result<bool> {
    eprint!("{prompt}");
    std::io::stderr().flush()?;
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line)? == 0 {
        return Ok(false);
    }
    let a = line.trim().to_ascii_lowercase();
    Ok(a == "y" || a == "yes")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clippy_blob_splits_per_warning_and_drops_summary() {
        let blob = "warning: unused variable: `x`\n  --> src/a.rs:1:5\n   |\n   = note: foo\n\n\
                    warning: this could be written differently\n  --> src/b.rs:2:9\n\n\
                    warning: `scrooge` (bin \"scrooge\") generated 2 warnings";
        let items = split_diagnostics("rust", blob);
        assert_eq!(items.len(), 2);
        assert!(items[0].starts_with("warning: unused variable"));
        assert!(items[1].contains("written differently"));
    }

    #[test]
    fn ruff_blob_splits_per_diagnostic_and_drops_summary() {
        let blob = "src/a.py:1:1: F401 `os` imported but unused\nsrc/b.py:3:5: E501 line too long\n  help: remove it\nFound 2 errors.\n[*] 1 fixable";
        let items = split_diagnostics("python", blob);
        assert_eq!(items.len(), 2);
        assert!(items[0].contains("F401"));
        assert!(items[1].contains("E501"));
        assert!(items[1].contains("help: remove it"));
    }

    #[test]
    fn structure_blob_splits_per_file_repeating_header() {
        let blob = "files over 2000 lines — split into smaller modules:\nsrc/a.rs: 2500 lines\nsrc/b.rs: 2100 lines";
        let items = split_diagnostics("structure", blob);
        assert_eq!(items.len(), 2);
        assert!(items[0].starts_with("files over 2000 lines"));
        assert!(items[0].contains("src/a.rs: 2500 lines"));
        assert!(items[1].contains("src/b.rs: 2100 lines"));
    }

    #[test]
    fn unrecognised_label_is_handed_over_whole() {
        let blob = "test result: FAILED. 3 passed; 1 failed";
        let items = split_diagnostics("rust-tests", blob);
        assert_eq!(items, vec![blob.to_string()]);
    }
}
