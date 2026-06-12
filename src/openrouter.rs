//! Minimal OpenRouter chat client with OpenAI-style tool calling.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

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
}

pub struct Client {
    http: reqwest::Client,
    api_key: String,
    pub usage: Usage,
}

impl Client {
    pub fn new() -> Result<Self> {
        let api_key =
            std::env::var("OPENROUTER_API_KEY").context("OPENROUTER_API_KEY is not set")?;
        Ok(Client {
            http: reqwest::Client::new(),
            api_key,
            usage: Usage::default(),
        })
    }

    /// One chat completion. `tools` is an OpenAI-format tool list or empty.
    pub async fn chat(
        &mut self,
        model: &str,
        messages: &[Message],
        tools: &[Value],
    ) -> Result<Message> {
        let mut body = serde_json::json!({
            "model": model,
            "messages": messages,
        });
        if !tools.is_empty() {
            body["tools"] = Value::Array(tools.to_vec());
        }
        let resp = self
            .http
            .post("https://openrouter.ai/api/v1/chat/completions")
            .bearer_auth(&self.api_key)
            .header("HTTP-Referer", "https://github.com/scrooge-agent")
            .header("X-Title", "scrooge")
            .json(&body)
            .send()
            .await?;
        let status = resp.status();
        let v: Value = resp.json().await.context("decoding openrouter response")?;
        if !status.is_success() {
            bail!("openrouter error {status}: {v}");
        }
        if let Some(u) = v.get("usage") {
            self.usage.prompt_tokens += u["prompt_tokens"].as_u64().unwrap_or(0);
            self.usage.completion_tokens += u["completion_tokens"].as_u64().unwrap_or(0);
        }
        let msg = v["choices"][0]["message"].clone();
        if msg.is_null() {
            bail!("no choices in response: {v}");
        }
        Ok(serde_json::from_value(msg)?)
    }
}
