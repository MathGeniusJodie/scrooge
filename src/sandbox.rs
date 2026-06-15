//! Landlock confinement for child processes. Shared by Cratchit's `shell`
//! tool (tools.rs) and the deterministic check suite (checks.rs) — both spawn
//! commands that may run freshly written, untrusted code, so both confine it.

use landlock::{
    ABI, Access, AccessFs, Ruleset, RulesetAttr, RulesetCreatedAttr, path_beneath_rules,
};
use std::path::{Path, PathBuf};

/// Landlock policy: read the whole filesystem, write only beneath the project
/// root, /tmp, /dev (null, shm, …) and the package-manager caches that
/// `cargo`/`npm`/`pip` need to function. Best-effort — on a kernel without
/// Landlock the command still runs (the caller ignores the error).
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
