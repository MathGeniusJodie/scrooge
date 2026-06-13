//! Minimal OpenRouter chat client with OpenAI-style tool calling.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;

use crate::accounting;

pub const DEV_MODEL_CHEAP: &str = "deepseek/deepseek-v4-flash"; // cratchit
pub const DEV_MODEL_SOTA: &str = "deepseek/deepseek-v4-flash"; // scrooge (swap before real use)

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Message {
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl Message {
    pub fn text(role: &str, content: impl Into<String>) -> Self {
        Message {
            role: role.into(),
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: None,
        }
    }
    pub fn tool_result(id: &str, content: impl Into<String>) -> Self {
        Message {
            role: "tool".into(),
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: Some(id.into()),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub function: FunctionCall,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct FunctionCall {
    pub name: String,
    pub arguments: String, // JSON string per OpenAI convention
}

#[derive(Debug, Default)]
pub struct Usage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    /// Cumulative cost in USD, as reported by OpenRouter (`usage.cost`).
    pub cost_usd: f64,
}

pub struct Client {
    http: reqwest::Client,
    api_key: String,
    root: PathBuf,
    pub usage: Usage,
    /// finish_reason of the most recent completion ("stop", "length", ...),
    /// so callers can detect a truncated response deterministically.
    pub last_finish_reason: Option<String>,
}

impl Client {
    pub fn new(root: PathBuf) -> Result<Self> {
        let api_key =
            std::env::var("OPENROUTER_API_KEY").context("OPENROUTER_API_KEY is not set")?;
        Ok(Client {
            http: reqwest::Client::new(),
            api_key,
            root,
            usage: Usage::default(),
            last_finish_reason: None,
        })
    }

    /// One chat completion. `agent` ("scrooge"/"cratchit") attributes the
    /// tokens in the ledger. `tools` is an OpenAI-format tool list or empty.
    /// `max_tokens` hard-caps the completion (used to keep Scrooge terse).
    pub async fn chat(
        &mut self,
        agent: &str,
        model: &str,
        messages: &[Message],
        tools: &[Value],
        max_tokens: Option<u32>,
    ) -> Result<Message> {
        let mut body = serde_json::json!({
            "model": model,
            "messages": messages,
            // Ask OpenRouter to report the actual dollar cost in `usage.cost`.
            "usage": { "include": true },
        });
        if !tools.is_empty() {
            body["tools"] = Value::Array(tools.to_vec());
        }
        if let Some(cap) = max_tokens {
            body["max_tokens"] = serde_json::json!(cap);
        }
        // Retry transient failures (429 / 5xx / transport errors) with
        // backoff: one network blip must not discard a whole task's tokens.
        const RETRIES: u32 = 3;
        let mut attempt = 0;
        let v: Value = loop {
            attempt += 1;
            let resp = self
                .http
                .post("https://openrouter.ai/api/v1/chat/completions")
                .bearer_auth(&self.api_key)
                .header("HTTP-Referer", "https://github.com/scrooge-agent")
                .header("X-Title", "scrooge")
                .json(&body)
                .send()
                .await;
            match resp {
                Ok(r) => {
                    let status = r.status();
                    // Read text before parsing so a non-JSON error page (e.g.
                    // gateway HTML on a 502) still reports its status code.
                    let text = r.text().await.unwrap_or_default();
                    if status.is_success() {
                        break serde_json::from_str(&text)
                            .context("decoding openrouter response")?;
                    }
                    let transient = status.as_u16() == 429 || status.is_server_error();
                    if !transient || attempt > RETRIES {
                        bail!("openrouter error {status}: {text}");
                    }
                    eprintln!("[openrouter {status}; retry {attempt}/{RETRIES}]");
                }
                Err(e) if attempt <= RETRIES => {
                    eprintln!("[openrouter transport error: {e}; retry {attempt}/{RETRIES}]");
                }
                Err(e) => return Err(e).context("openrouter request failed"),
            }
            tokio::time::sleep(std::time::Duration::from_secs(1 << attempt)).await;
        };
        if let Some(u) = v.get("usage") {
            let (p, c) = (
                u["prompt_tokens"].as_u64().unwrap_or(0),
                u["completion_tokens"].as_u64().unwrap_or(0),
            );
            let cost = u["cost"].as_f64().unwrap_or(0.0);
            self.usage.prompt_tokens += p;
            self.usage.completion_tokens += c;
            self.usage.cost_usd += cost;
            accounting::record(&self.root, agent, model, p, c, cost, &body, &v);
        }
        self.last_finish_reason = v["choices"][0]["finish_reason"]
            .as_str()
            .map(str::to_string);
        let msg = v["choices"][0]["message"].clone();
        if msg.is_null() {
            bail!("no choices in response: {v}");
        }
        Ok(serde_json::from_value(msg)?)
    }
}
