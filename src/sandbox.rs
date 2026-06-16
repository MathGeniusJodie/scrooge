//! Landlock confinement for child processes. Shared by Cratchit's `shell`
//! tool (tools.rs) and the deterministic check suite (checks.rs) — both spawn
//! commands that may run freshly written, untrusted code, so both confine it.
//!
//! Fork-safety: the ruleset (path resolution, allocation, rule-adding — all of
//! which allocate or open files) is built in the *parent*, before fork. The
//! only thing the post-fork `pre_exec` closure does is the
//! `landlock_restrict_self` syscall, which allocates nothing — so it is safe to
//! run between `fork` and `exec` in a multi-threaded program, where a captured
//! malloc lock would otherwise deadlock the child.

use landlock::{
    ABI, Access, AccessFs, CompatLevel, Compatible, Ruleset, RulesetAttr, RulesetCreated,
    RulesetCreatedAttr, path_beneath_rules,
};
use std::path::{Path, PathBuf};

/// Build the Landlock ruleset for `root`: read the system locations and
/// toolchain installs that compilers/package managers need, write only beneath
/// the project root, /tmp, /dev (null, shm, …) and the package-manager caches
/// that `cargo`/`npm`/`pip` need to function. Deliberately does NOT grant read
/// of the home directory or the filesystem at large, so freshly written,
/// untrusted code cannot exfiltrate the user's SSH/cloud credentials, config
/// secrets, browser data, or documents.
///
/// All of this — `env::var`, the `Vec`/`PathBuf` allocations, the `exists()`
/// probes, opening each path, and the `landlock_add_rule` syscalls — runs in
/// the parent. The returned `RulesetCreated`'s only remaining step is
/// `restrict_self()`, which the child performs post-fork (see `confiner`).
fn build_ruleset(root: &Path) -> Result<RulesetCreated, Box<dyn std::error::Error>> {
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
    Ok(Ruleset::default()
        .handle_access(AccessFs::from_all(abi))?
        .create()?
        .add_rules(path_beneath_rules(&readable, AccessFs::from_read(abi)))?
        .add_rules(path_beneath_rules(&writable, AccessFs::from_all(abi)))?
        .add_rules(path_beneath_rules(
            &dev_files,
            AccessFs::WriteFile | AccessFs::ReadFile,
        ))?)
}

/// A `pre_exec` closure that confines the child to `build_ruleset(root)`'s
/// policy. The ruleset is built now, in the parent; the returned closure makes
/// only the allocation-free `restrict_self` syscall, so it is safe to run
/// between fork and exec. Best-effort — on a kernel without Landlock the
/// ruleset build fails and the closure is a no-op (callers gate on `preflight`
/// so an unenforced sandbox can't pass unnoticed).
pub fn confiner(root: &Path) -> impl FnMut() -> std::io::Result<()> + Send + Sync + use<> {
    let mut prepared = build_ruleset(root).ok();
    move || {
        if let Some(rs) = prepared.take() {
            let _ = rs.restrict_self();
        }
        Ok(())
    }
}

/// Whether this kernel actually enforces Landlock at the ABI we rely on. Uses a
/// hard compatibility requirement so a kernel missing Landlock (or below ABI
/// V2) reports `false` instead of silently degrading to a no-op ruleset. Only
/// creates a ruleset fd — never restricts the calling process.
fn enforced() -> bool {
    Ruleset::default()
        .set_compatibility(CompatLevel::HardRequirement)
        .handle_access(AccessFs::from_all(ABI::V2))
        .and_then(Ruleset::create)
        .is_ok()
}

/// Refuse to run untrusted child processes on a kernel that won't enforce the
/// sandbox — the whole promise of `confiner` is that Cratchit's code can't read
/// your secrets, and a silent fail-open would break it without a word. Override
/// with `SCROOGE_ALLOW_UNSANDBOXED=1` to proceed unguarded. Called from the
/// entry points that spawn untrusted commands.
pub fn preflight() {
    if std::env::var_os("SCROOGE_ALLOW_UNSANDBOXED").is_some() || enforced() {
        return;
    }
    // Bold red — a warning Scrooge himself could not ignore.
    eprintln!(
        "\x1b[1;31mBah! Humbug! This kernel will not enforce Landlock!\n\
         Set SCROOGE_ALLOW_UNSANDBOXED=1 to proceed unguarded, and may the three Spirits\n\
         have mercy on your credentials.\x1b[0m"
    );
    std::process::exit(1);
}
