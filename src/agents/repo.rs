//! Git-backed worktree inspection. The agent loop uses these to tell whether a
//! delegated step changed code — and therefore whether checks must run and a
//! CHECKS verdict is owed; `require_git_repo` makes that dependency explicit at
//! the code-changing entry points instead of letting detection silently degrade.

use anyhow::Result;
use std::path::Path;

/// Verify the project root is a git work tree, bailing with actionable guidance
/// if not. The agent loop uses `worktree_changes` to tell whether a step
/// changed code (and therefore whether checks must run / a CHECKS verdict is
/// owed); without git that detection silently degrades, so the code-changing
/// entry points require it up front rather than misbehaving later.
pub(super) fn require_git_repo(root: &Path) -> Result<()> {
    let inside = std::process::Command::new("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .current_dir(root)
        .output()
        .is_ok_and(|o| o.status.success());
    if !inside {
        anyhow::bail!(
            "{} is not a git repository (or git is unavailable). Scrooge needs git to \
             detect what each step changed — run `git init` here first.",
            root.display()
        );
    }
    Ok(())
}

/// Worktree state as git sees it: diffstat of tracked changes plus untracked
/// files. None when the root is not a git repo (or git is unavailable);
/// the code-changing entry points guard against that with `require_git_repo`.
pub(super) fn worktree_changes(root: &Path) -> Option<String> {
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    };
    // Diff against HEAD so staged changes count too (Cratchit sometimes runs
    // `git add`); fall back to the index diff on a repo with no commits yet.
    let diff = git(&["diff", "HEAD", "--stat"]).or_else(|| git(&["diff", "--stat"]))?;
    let untracked = git(&["status", "--short", "--untracked-files"])
        .unwrap_or_default()
        .lines()
        .filter(|l| l.starts_with("??"))
        .collect::<Vec<_>>()
        .join("\n");
    Some(format!("{diff}\n{untracked}").trim().to_string())
}

/// A content fingerprint of the worktree: the git tree OID of every tracked and
/// untracked (non-ignored) file's current bytes, captured through a throwaway
/// index so the real index is untouched. Unlike a `--stat` summary it changes
/// whenever the edited bytes change — two edits that net the same insertion/
/// deletion counts still produce different fingerprints, and a brand-new file
/// that is then modified is caught too — so a real change is never mistaken for
/// a read-only step. None when the root is not a git repo (or git is missing).
pub(super) fn worktree_fingerprint(root: &Path) -> Option<String> {
    use std::sync::atomic::{AtomicU64, Ordering};
    // Unique per call so concurrent orchestrators (e.g. parallel tests) never
    // share a temp index and corrupt each other's snapshot.
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let index = std::env::temp_dir().join(format!(
        "scrooge-index-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_file(&index);
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .current_dir(root)
            .env("GIT_INDEX_FILE", &index)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    };
    // Stage the whole worktree into the scratch index, then hash it to a tree.
    git(&["add", "-A"])?;
    let tree = git(&["write-tree"]);
    let _ = std::fs::remove_file(&index);
    tree
}
