//! Token bookkeeping, in the spirit of the house: every LLM call is entered
//! in .scrooge/accounts.log — header line plus the user prompt and the
//! model's text answer (the system prompt, tool calls, and their results
//! are left out to keep the ledger readable) — and running per-agent totals are kept
//! in .scrooge/ledger.json. Best-effort by design — bookkeeping must never
//! fail a chat call.

use serde_json::{Value, json};
use std::fmt::Write as _;
use std::io::Write;
use std::path::Path;

/// Just the user prompt(s): the system prompt, assistant turns, and
/// tool-result messages are all dropped, leaving only what the user asked.
fn prompt_text(request: &Value) -> String {
    let mut out = String::new();
    for msg in request["messages"].as_array().into_iter().flatten() {
        if msg["role"].as_str() != Some("user") {
            continue;
        }
        if let Some(content) = msg["content"].as_str()
            && !content.is_empty()
        {
            let _ = writeln!(out, "{content}");
        }
    }
    out
}

/// The model's text output, ignoring any tool calls it requested.
fn output_text(response: &Value) -> String {
    response["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or("")
        .to_string()
}

#[allow(clippy::too_many_arguments)]
pub fn record(
    root: &Path,
    agent: &str,
    model: &str,
    prompt: u64,
    completion: u64,
    cost_usd: f64,
    request: &Value,
    response: &Value,
) {
    let dir = root.join(".scrooge");
    let _ = std::fs::create_dir_all(&dir);

    let ts = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("accounts.log"))
    {
        let _ = writeln!(
            f,
            "=== {ts} {agent} {model} prompt={prompt} completion={completion} cost=${cost_usd:.6} ===\n\
             >>> prompt\n{}\n<<< output\n{}",
            prompt_text(request),
            output_text(response)
        );
    }

    let path = dir.join("ledger.json");
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
    entry["cost_usd"] = json!(entry["cost_usd"].as_f64().unwrap_or(0.0) + cost_usd);
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
        let req = serde_json::json!({"messages": [{"role": "user", "content": "fix the bug"}]});
        let resp = serde_json::json!({"choices": [{"message": {"content": "done"}}]});
        super::record(&dir, "cratchit", "m", 100, 10, 0.001, &req, &resp);
        super::record(&dir, "cratchit", "m", 50, 5, 0.002, &req, &resp);
        super::record(&dir, "scrooge", "m", 7, 3, 0.05, &req, &resp);
        let scrooge_dir = dir.join(".scrooge");
        let ledger: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(scrooge_dir.join("ledger.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(ledger["cratchit"]["prompt_tokens"], 150);
        assert_eq!(ledger["cratchit"]["completion_tokens"], 15);
        assert_eq!(ledger["scrooge"]["prompt_tokens"], 7);
        assert!((ledger["cratchit"]["cost_usd"].as_f64().unwrap() - 0.003).abs() < 1e-9);
        assert!((ledger["scrooge"]["cost_usd"].as_f64().unwrap() - 0.05).abs() < 1e-9);
        let log = std::fs::read_to_string(scrooge_dir.join("accounts.log")).unwrap();
        assert_eq!(log.matches("=== ").count(), 3, "one header per call");
        assert!(log.contains("fix the bug"), "full request text logged");
        assert!(log.contains("done"), "full response text logged");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
