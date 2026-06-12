//! Token bookkeeping, in the spirit of the house: every LLM call is entered
//! in accounts.log, and running per-agent totals are kept in ledger.json.
//! Best-effort by design — bookkeeping must never fail a chat call.

use serde_json::{Value, json};
use std::io::Write;
use std::path::Path;

pub fn record(root: &Path, agent: &str, model: &str, prompt: u64, completion: u64) {
    let ts = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(root.join("accounts.log"))
    {
        let _ = writeln!(
            f,
            "{ts} {agent} {model} prompt={prompt} completion={completion}"
        );
    }

    let path = root.join("ledger.json");
    let mut ledger: Value = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| json!({}));
    let entry = &mut ledger[agent];
    if entry.is_null() {
        *entry = json!({"prompt_tokens": 0, "completion_tokens": 0});
    }
    for (key, add) in [("prompt_tokens", prompt), ("completion_tokens", completion)] {
        entry[key] = json!(entry[key].as_u64().unwrap_or(0) + add);
    }
    if let Ok(s) = serde_json::to_string_pretty(&ledger) {
        let _ = std::fs::write(&path, s);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn record_accumulates_per_agent() {
        let dir = std::env::temp_dir().join(format!("scrooge-acct-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        super::record(&dir, "cratchit", "m", 100, 10);
        super::record(&dir, "cratchit", "m", 50, 5);
        super::record(&dir, "scrooge", "m", 7, 3);
        let ledger: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("ledger.json")).unwrap())
                .unwrap();
        assert_eq!(ledger["cratchit"]["prompt_tokens"], 150);
        assert_eq!(ledger["cratchit"]["completion_tokens"], 15);
        assert_eq!(ledger["scrooge"]["prompt_tokens"], 7);
        assert_eq!(
            std::fs::read_to_string(dir.join("accounts.log"))
                .unwrap()
                .lines()
                .count(),
            3
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
