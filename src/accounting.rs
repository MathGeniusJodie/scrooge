//! Token bookkeeping, in the spirit of the house: every LLM call is entered
//! in .scrooge/accounts.log — header line plus the user prompt and the
//! model's text answer (the system prompt, tool calls, and their results
//! are left out to keep the ledger readable) — and running per-agent totals are kept
//! in .scrooge/ledger.json. Best-effort by design — bookkeeping must never
//! fail a chat call.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::fmt::Write as _;
use std::io::Write;
use std::path::Path;

/// Scrooge-model token prices, in USD per million tokens, used to value the
/// `shillings_saved` each Cratchit call earns. Lives in .scrooge/rates.toml
/// so the user (or the agents) can edit it, just like checks.toml.
#[derive(Serialize, Deserialize, Clone)]
pub struct Rates {
    /// USD per million prompt (input) tokens on the Scrooge model.
    pub scrooge_usd_per_mtok_in: f64,
    /// USD per million completion (output) tokens on the Scrooge model.
    pub scrooge_usd_per_mtok_out: f64,
}

impl Default for Rates {
    fn default() -> Self {
        Self {
            scrooge_usd_per_mtok_in: 3.0,
            scrooge_usd_per_mtok_out: 15.0,
        }
    }
}

/// Load .scrooge/rates.toml, writing it from the defaults first if it doesn't
/// exist yet (so there is always a file to edit). Best-effort: a malformed or
/// unreadable file falls back to the built-in defaults rather than failing.
fn load_rates(dir: &Path) -> Rates {
    let path = dir.join("rates.toml");
    if let Ok(text) = std::fs::read_to_string(&path)
        && let Ok(rates) = toml::from_str(&text)
    {
        return rates;
    }
    let rates = Rates::default();
    if let Ok(body) = toml::to_string_pretty(&rates) {
        let header = "# Scrooge-model token prices (USD per million tokens) used to compute\n\
                      # `shillings_saved` in ledger.json. Edit freely (agents may too).\n\n";
        let _ = std::fs::write(&path, format!("{header}{body}"));
    }
    rates
}

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

/// What `prompt`/`completion` tokens would have cost on the Scrooge model at
/// the given rates. The single source of truth for the "shillings saved" math,
/// shared by `record` (cumulative totals) and `shillings_saved` (per-request).
#[allow(clippy::cast_precision_loss)]
fn scrooge_cost(rates: &Rates, prompt: f64, completion: f64) -> f64 {
    prompt * rates.scrooge_usd_per_mtok_in / 1e6 + completion * rates.scrooge_usd_per_mtok_out / 1e6
}

/// The model's text output, ignoring any tool calls it requested.
fn output_text(response: &Value) -> String {
    response["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or("")
        .to_string()
}

#[allow(clippy::too_many_arguments)]
#[allow(clippy::cast_precision_loss)]
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
    // Shillings saved is the thrift of running Cratchit instead of Scrooge:
    // what these tokens would have cost on the Scrooge model (rates from
    // .scrooge/rates.toml) less what they actually cost. Only meaningful for
    // Cratchit — for Scrooge it would value the model against itself, so the
    // field is left off his ledger entry entirely.
    if agent == "cratchit" {
        let rates = load_rates(&dir);
        let p = entry["prompt_tokens"].as_u64().unwrap_or(0) as f64;
        let c = entry["completion_tokens"].as_u64().unwrap_or(0) as f64;
        let saved = scrooge_cost(&rates, p, c) - entry["cost_usd"].as_f64().unwrap_or(0.0);
        entry["shillings_saved"] = json!(saved);
    }
    if let Ok(s) = serde_json::to_string_pretty(&ledger) {
        let _ = std::fs::write(&path, s);
    }
}

/// What `prompt`/`completion` tokens would have cost on the Scrooge model
/// (rates from .scrooge/rates.toml) less the `cost_usd` actually paid — the
/// thrift of delegating to Cratchit, in plain USD. `root` is the project root
/// (the rates file lives under its .scrooge/ directory).
#[allow(clippy::cast_precision_loss)]
pub fn shillings_saved(root: &Path, prompt: u64, completion: u64, cost_usd: f64) -> f64 {
    let rates = load_rates(&root.join(".scrooge"));
    scrooge_cost(&rates, prompt as f64, completion as f64) - cost_usd
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
        // 150*$3/M + 15*$15/M - $0.003 actual = 0.000675 - 0.003
        assert!(
            (ledger["cratchit"]["shillings_saved"].as_f64().unwrap() - (0.000675 - 0.003)).abs()
                < 1e-9
        );
        // Scrooge's entry carries no shillings_saved — valuing the model
        // against itself is meaningless.
        assert!(ledger["scrooge"]["shillings_saved"].is_null());
        let log = std::fs::read_to_string(scrooge_dir.join("accounts.log")).unwrap();
        assert_eq!(log.matches("=== ").count(), 3, "one header per call");
        assert!(log.contains("fix the bug"), "full request text logged");
        assert!(log.contains("done"), "full response text logged");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
