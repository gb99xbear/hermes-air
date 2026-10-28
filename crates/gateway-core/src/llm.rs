use anyhow::{Context, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tracing::{error, info};

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

    async fn send_completion(&self, req_body: &ChatCompletionRequest) -> Result<ChatCompletionResponse> {
        let url = format!("{}/chat/completions", self.base_url);
        let mut request = self.client.post(&url).json(req_body);

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
