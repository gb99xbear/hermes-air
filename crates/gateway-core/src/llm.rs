use futures::StreamExt;
use anyhow::{Context, Result};
use reqwest::header::{HeaderName, HeaderValue};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::Duration;
use tracing::{error, info};

use crate::providers::ProviderProfile;
use crate::tools::ToolCall;

pub use hermes_protocol::{ChatMessage, ToolCallFunctionRaw, ToolCallRaw};

pub trait ToolCallRawExt {
    fn to_tool_call(&self) -> ToolCall;
}

impl ToolCallRawExt for ToolCallRaw {
    fn to_tool_call(&self) -> ToolCall {
        let args: serde_json::Value = serde_json::from_str(&self.function.arguments)
            .unwrap_or_else(|_| serde_json::Value::Object(serde_json::Map::new()));
        ToolCall {
            id: self.id.clone(),
            name: self.function.name.clone(),
            arguments: args,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ChatCompletionRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<serde_json::Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<String>,
    pub stream: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChatCompletionResponse {
    pub choices: Vec<Choice>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Choice {
    pub message: ChatResponseMessage,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChatResponseMessage {
    #[serde(default)]
    pub role: String,
    pub content: Option<String>,
    #[serde(default)]
    pub tool_calls: Option<Vec<ToolCallRaw>>,
}

#[derive(Debug, Clone)]
pub enum ChatResponse {
    Text(String),
    ToolCalls(Vec<ToolCall>),
}

#[derive(Debug, Clone)]
pub struct LlmClient {
    client: Client,
    pub base_url: String,
    pub api_key: Option<String>,
    pub default_headers: HashMap<String, String>,
}

impl LlmClient {
    pub fn new(base_url: Option<String>, api_key: Option<String>) -> Self {
        let base_url = base_url
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "http://100.75.163.92:20128/v1".to_string());
        
        let client = Client::builder()
            .timeout(Duration::from_secs(120))
            .build()
            .unwrap_or_default();

        Self {
            client,
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key,
            default_headers: HashMap::new(),
        }
    }

    /// Construct LlmClient resolving its base_url, api_key, and default_headers from a ProviderProfile.
    pub fn from_provider_profile(
        profile: &ProviderProfile,
        base_url_override: Option<String>,
        api_key_override: Option<String>,
    ) -> Self {
        let base_url = base_url_override
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| {
                if !profile.base_url.is_empty() {
                    profile.base_url.clone()
                } else {
                    "http://100.75.163.92:20128/v1".to_string()
                }
            });

        let api_key = api_key_override
            .filter(|s| !s.trim().is_empty())
            .or_else(|| profile.resolve_api_key());

        let client = Client::builder()
            .timeout(Duration::from_secs(120))
            .build()
            .unwrap_or_default();

        Self {
            client,
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key,
            default_headers: profile.default_headers.clone(),
        }
    }

    pub async fn chat_completion(
        &self,
        model: &str,
        system_prompt: Option<&str>,
        user_message: &str,
        temperature: f32,
        max_tokens: u32,
    ) -> Result<String> {
        let mut messages = Vec::new();
        if let Some(sys) = system_prompt {
            if !sys.is_empty() {
                messages.push(ChatMessage {
                    role: "system".to_string(),
                    content: Some(sys.to_string()),
                    tool_calls: None,
                    tool_call_id: None,
                });
            }
        }
        messages.push(ChatMessage {
            role: "user".to_string(),
            content: Some(user_message.to_string()),
            tool_calls: None,
            tool_call_id: None,
        });

        let req_body = ChatCompletionRequest {
            model: model.to_string(),
            messages,
            temperature: Some(temperature),
            max_tokens: Some(max_tokens),
            tools: None,
            tool_choice: None,
            stream: false,
        };

        let response = self.send_completion(&req_body).await?;
        let choice = response
            .choices
            .into_iter()
            .next()
            .context("LLM returned no choices in response")?;

        Ok(choice.message.content.unwrap_or_default())
    }

    pub async fn chat_completion_with_tools(
        &self,
        model: &str,
        messages: &[ChatMessage],
        tools: Option<Vec<serde_json::Value>>,
        tool_choice: Option<String>,
        temperature: f32,
        max_tokens: u32,
    ) -> Result<(ChatResponseMessage, ChatResponse)> {
        let req_body = ChatCompletionRequest {
            model: model.to_string(),
            messages: messages.to_vec(),
            temperature: Some(temperature),
            max_tokens: Some(max_tokens),
            tools,
            tool_choice,
            stream: false,
        };

        let response = self.send_completion(&req_body).await?;
        let choice = response
            .choices
            .into_iter()
            .next()
            .context("LLM returned no choices in response")?;

        let msg = choice.message;
        if let Some(ref calls) = msg.tool_calls {
            if !calls.is_empty() {
                let parsed_calls = calls.iter().map(|c| c.to_tool_call()).collect();
                return Ok((msg, ChatResponse::ToolCalls(parsed_calls)));
            }
        }

        let text = msg.content.clone().unwrap_or_default();
        Ok((msg, ChatResponse::Text(text)))
    }

    pub async fn chat_completion_stream(
        &self,
        model: &str,
        messages: &[ChatMessage],
        temperature: f32,
        max_tokens: u32,
    ) -> Result<tokio::sync::mpsc::Receiver<String>> {
        let req_body = ChatCompletionRequest {
            model: model.to_string(),
            messages: messages.to_vec(),
            temperature: Some(temperature),
            max_tokens: Some(max_tokens),
            tools: None,
            tool_choice: None,
            stream: true,
        };

        let url = format!("{}/chat/completions", self.base_url);
        let mut request = self.client.post(&url).json(&req_body);

        for (k, v) in &self.default_headers {
            if let (Ok(h_name), Ok(h_val)) = (HeaderName::from_bytes(k.as_bytes()), HeaderValue::from_str(v)) {
                request = request.header(h_name, h_val);
            }
        }

        if let Some(ref key) = self.api_key {
            if !key.is_empty() {
                request = request.header("Authorization", format!("Bearer {}", key));
            }
        }

        info!("Initiating SSE chat completion stream to {} (model: {})", url, model);
        let resp = request
            .send()
            .await
            .with_context(|| format!("Failed to send streaming request to LLM endpoint: {}", url))?;

        let status = resp.status();
        if !status.is_success() {
            let error_text = resp.text().await.unwrap_or_else(|_| "Unknown error".to_string());
            error!("LLM streaming endpoint returned status {}: {}", status, error_text);
            anyhow::bail!("LLM API stream error ({}): {}", status, error_text);
        }

        let (tx, rx) = tokio::sync::mpsc::channel(100);
        let mut stream = resp.bytes_stream();

        tokio::spawn(async move {
            let mut buffer = String::new();
            while let Some(chunk_res) = stream.next().await {
                let chunk = match chunk_res {
                    Ok(bytes) => bytes,
                    Err(e) => {
                        error!("Error reading chunk from LLM stream: {:?}", e);
                        break;
                    }
                };
                buffer.push_str(&String::from_utf8_lossy(&chunk));
                while let Some(pos) = buffer.find("\n") {
                    let line = buffer[..pos].trim_end_matches("\r").to_string();
                    buffer.drain(..=pos);
                    let trimmed = line.trim();
                    if trimmed.is_empty() { continue; }
                    if let Some(rest) = trimmed.strip_prefix("data: ") {
                        let rest = rest.trim();
                        if rest == "[DONE]" {
                            info!("LLM stream completed with [DONE]");
                            return;
                        }
                        if let Ok(val) = serde_json::from_str::<serde_json::Value>(rest) {
                            if let Some(content) = val.get("choices")
                                .and_then(|c| c.get(0))
                                .and_then(|c| c.get("delta"))
                                .and_then(|d| d.get("content"))
                                .and_then(|s| s.as_str()) {
                                    if !content.is_empty() {
                                        if tx.send(content.to_string()).await.is_err() {
                                            // Receiver dropped, terminate stream
                                            return;
                                        }
                                    }
                            }
                        }
                    }
                }
            }
        });

        Ok(rx)
    }

    async fn send_completion(&self, req_body: &ChatCompletionRequest) -> Result<ChatCompletionResponse> {
        let url = format!("{}/chat/completions", self.base_url);
        let mut request = self.client.post(&url).json(req_body);

        // Apply profile default headers if present
        for (k, v) in &self.default_headers {
            if let (Ok(h_name), Ok(h_val)) = (HeaderName::from_bytes(k.as_bytes()), HeaderValue::from_str(v)) {
                request = request.header(h_name, h_val);
            }
        }

        if let Some(ref key) = self.api_key {
            if !key.is_empty() {
                request = request.header("Authorization", format!("Bearer {}", key));
            }
        }

        info!("Sending chat completion to {} (model: {})", url, req_body.model);
        let resp = request
            .send()
            .await
            .with_context(|| format!("Failed to send request to LLM endpoint: {}", url))?;

        let status = resp.status();
        if !status.is_success() {
            let error_text = resp.text().await.unwrap_or_else(|_| "Unknown error".to_string());
            error!("LLM endpoint returned status {}: {}", status, error_text);
            anyhow::bail!("LLM API error ({}): {}", status, error_text);
        }

        let completion: ChatCompletionResponse = resp
            .json()
            .await
            .context("Failed to deserialize ChatCompletionResponse")?;

        Ok(completion)
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_chat_completion_stream_sse_parsing() {
        let raw_sse = "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"\"}}]}\n\ndata: {\"choices\":[{\"delta\":{\"content\":\"Hello \"}}]}\n\ndata: {\"choices\":[{\"delta\":{\"content\":\"world!\"}}]}\n\ndata: [DONE]\n\n";
        let lines: Vec<&str> = raw_sse.lines().collect();
        let mut collected = Vec::new();
        for line in lines {
            let trimmed = line.trim();
            if trimmed.is_empty() { continue; }
            if let Some(rest) = trimmed.strip_prefix("data: ") {
                let rest = rest.trim();
                if rest == "[DONE]" { break; }
                let val: serde_json::Value = serde_json::from_str(rest).unwrap();
                if let Some(content) = val.get("choices")
                    .and_then(|c| c.get(0))
                    .and_then(|c| c.get("delta"))
                    .and_then(|d| d.get("content"))
                    .and_then(|s| s.as_str()) {
                        if !content.is_empty() {
                            collected.push(content.to_string());
                        }
                }
            }
        }
        assert_eq!(collected.join(""), "Hello world!");
    }

    #[tokio::test]
    async fn test_chat_completion_stream_method_signature() {
        let client = LlmClient::new(None, None);
        let msgs = vec![ChatMessage::user("test")];
        let _rx = client.chat_completion_stream("test-model", &msgs, 0.7, 100).await;
    }
}
