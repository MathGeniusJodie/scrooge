//! Landlock confinement for child processes. Shared by Cratchit's `shell`
//! tool (tools.rs) and the deterministic check suite (checks.rs) — both spawn
//! commands that may run freshly written, untrusted code, so both confine it.

use landlock::{
    ABI, Access, AccessFs, Ruleset, RulesetAttr, RulesetCreatedAttr, path_beneath_rules,
};
use std::path::{Path, PathBuf};

/// Landlock policy: read the system locations and toolchain installs that
/// compilers/package managers need, write only beneath the project root,
/// /tmp, /dev (null, shm, …) and the package-manager caches that
/// `cargo`/`npm`/`pip` need to function. Deliberately does NOT grant read of
/// the home directory or the filesystem at large, so freshly written,
/// untrusted code cannot exfiltrate the user's SSH/cloud credentials, config
/// secrets, browser data, or documents. Best-effort — on a kernel without
/// Landlock the command still runs (the caller ignores the error).
pub fn confine(root: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let abi = ABI::V2;
    // Writable directory subtrees: the project, scratch space, the POSIX shared
    // memory dir, and the package-manager caches the toolchains need.
    let mut writable: Vec<PathBuf> = vec![root.to_path_buf(), "/tmp".into(), "/dev/shm".into()];
    if let Ok(home) = std::env::var("HOME") {
        for d in [".cargo", ".npm", ".cache"] {
            writable.push(Path::new(&home).join(d));
        }
    }
    writable.retain(|p| p.exists());
    // Read-only subtrees: system binaries, libraries, certificates and the
    // proc/sys interfaces that toolchains and build scripts read, plus the
    // per-user toolchain installs (rustup toolchains/std sources, language
    // version managers, ~/.local). The writable subtrees above are readable
    // too (from_all includes read). The home directory itself is NOT listed,
    // so dotfiles like ~/.ssh, ~/.aws and ~/.config stay unreadable.
    let mut readable: Vec<PathBuf> = [
        "/usr", "/bin", "/sbin", "/lib", "/lib32", "/lib64", "/libx32", "/etc", "/opt", "/proc",
        "/sys", "/run",
    ]
    .into_iter()
    .map(PathBuf::from)
    .collect();
    if let Ok(home) = std::env::var("HOME") {
        for d in [
            ".rustup", ".pyenv", ".rbenv", ".nvm", ".volta", ".local", ".deno", ".bun",
        ] {
            readable.push(Path::new(&home).join(d));
        }
    }
    readable.retain(|p| p.exists());
    // Individual device *files* the toolchains open, rather than all of /dev:
    // granting write to the whole tree would expose every device the user can
    // open to freshly written, untrusted code. These are not directories, so
    // they get the file-only access rights (not the directory bits in from_all).
    let dev_files: Vec<PathBuf> = [
        "/dev/null",
        "/dev/zero",
        "/dev/full",
        "/dev/random",
        "/dev/urandom",
        "/dev/tty",
    ]
    .into_iter()
    .map(PathBuf::from)
    .filter(|p| p.exists())
    .collect();
    Ruleset::default()
        .handle_access(AccessFs::from_all(abi))?
        .create()?
        .add_rules(path_beneath_rules(&readable, AccessFs::from_read(abi)))?
        .add_rules(path_beneath_rules(&writable, AccessFs::from_all(abi)))?
        .add_rules(path_beneath_rules(
            &dev_files,
            AccessFs::WriteFile | AccessFs::ReadFile,
        ))?
        .restrict_self()?;
    Ok(())
}
