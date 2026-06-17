//! Project overview: a short prose file saying what the codebase *is* and
//! how it hangs together — the things a symbol map cannot show. Written by
//! Cratchit once (from the kickoff task on a new project, or by exploring an
//! existing one), stored in .scrooge/overview.md, freely editable by the
//! user, and injected verbatim into every Scrooge and Cratchit briefing.

use anyhow::Context;
use std::path::{Path, PathBuf};

pub fn path(root: &Path) -> PathBuf {
    root.join(".scrooge").join("overview.md")
}

pub fn load(root: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path(root)).ok()?;
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    Some(text.to_string())
}

pub fn save(root: &Path, text: &str) -> anyhow::Result<()> {
    let path = path(root);
    std::fs::create_dir_all(path.parent().context("overview path has no parent")?)?;
    std::fs::write(&path, text.trim())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn load_roundtrips_and_ignores_empty() {
        let dir = std::env::temp_dir().join(format!("scrooge-ov-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(super::load(&dir), None);
        super::save(&dir, "A duck website.\n").unwrap();
        assert_eq!(super::load(&dir).as_deref(), Some("A duck website."));
        super::save(&dir, "  \n").unwrap();
        assert_eq!(super::load(&dir), None, "blank file counts as missing");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
