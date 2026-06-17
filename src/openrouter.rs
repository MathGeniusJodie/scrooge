//! Minimal `OpenRouter` chat client with OpenAI-style tool calling.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;

use crate::accounting;

pub const DEV_MODEL_CHEAP: &str = "@preset/cratchit"; // cratchit
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
        Self {
            role: role.into(),
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: None,
        }
    }
    pub fn tool_result(id: &str, content: impl Into<String>) -> Self {
        Self {
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
    /// Cumulative cost in USD, as reported by `OpenRouter` (`usage.cost`).
    pub cost_usd: f64,
}

pub struct Client {
    http: reqwest::Client,
    api_key: String,
    root: PathBuf,
    pub usage: Usage,
    /// `finish_reason` of the most recent completion ("stop", "length", ...),
    /// so callers can detect a truncated response deterministically.
    pub last_finish_reason: Option<String>,
}

impl Client {
    pub fn new(root: PathBuf) -> Result<Self> {
        let api_key =
            std::env::var("OPENROUTER_API_KEY").context("OPENROUTER_API_KEY is not set")?;
        Ok(Self {
            http: reqwest::Client::new(),
            api_key,
            root,
            usage: Usage::default(),
            last_finish_reason: None,
        })
    }
}

/// The chat backend the orchestrator talks to. Abstracted behind a trait so the
/// agent loop can be driven by a scripted fake in tests — no live API calls —
/// while production uses `Client` over `OpenRouter`. `-> impl Future + Send`
/// (rather than `async fn`) keeps the returned future `Send` for the
/// multi-threaded runtime without pulling in `async_trait`.
pub trait Chat {
    fn chat(
        &mut self,
        agent: &str,
        model: &str,
        messages: &[Message],
        tools: &[Value],
        max_tokens: Option<u32>,
    ) -> impl std::future::Future<Output = Result<Message>> + Send;

    /// Cumulative token/cost usage across this backend's lifetime.
    fn usage(&self) -> &Usage;

    /// `finish_reason` of the most recent completion, if any.
    fn last_finish_reason(&self) -> Option<&str>;
}

impl Chat for Client {
    fn usage(&self) -> &Usage {
        &self.usage
    }

    fn last_finish_reason(&self) -> Option<&str> {
        self.last_finish_reason.as_deref()
    }

    /// One chat completion. `agent` ("scrooge"/"cratchit") attributes the
    /// tokens in the ledger. `tools` is an OpenAI-format tool list or empty.
    /// `max_tokens` hard-caps the completion (used to keep Scrooge terse).
    async fn chat(
        &mut self,
        agent: &str,
        model: &str,
        messages: &[Message],
        tools: &[Value],
        max_tokens: Option<u32>,
    ) -> Result<Message> {
        // Retry transient failures (429 / 5xx / transport errors) with
        // backoff: one network blip must not discard a whole task's tokens.
        const RETRIES: u32 = 3;
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
                    const TOO_MANY_REQUESTS: u16 = 429;
                    let transient =
                        status.as_u16() == TOO_MANY_REQUESTS || status.is_server_error();
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
            let turn = accounting::Turn {
                agent,
                model,
                prompt_tokens: u["prompt_tokens"].as_u64().unwrap_or(0),
                completion_tokens: u["completion_tokens"].as_u64().unwrap_or(0),
                cost_usd: u["cost"].as_f64().unwrap_or(0.0),
                request: &body,
                response: &v,
            };
            self.usage.prompt_tokens += turn.prompt_tokens;
            self.usage.completion_tokens += turn.completion_tokens;
            self.usage.cost_usd += turn.cost_usd;
            accounting::record(&self.root, &turn);
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
