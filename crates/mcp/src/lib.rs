// hermes-mcp - Model Context Protocol server/client
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tokio::process::Command;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum McpError {
    #[error("Transport error: {0}")]
    Transport(String),
    #[error("Protocol error: {0}")]
    Protocol(String),
    #[error("Server not found: {0}")]
    ServerNotFound(String),
    #[error("Tool not found: {0}")]
    ToolNotFound(String),
    #[error("Resource not found: {0}")]
    ResourceNotFound(String),
    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerConfig {
    pub name: String,
    pub transport: McpTransport,
    pub command: Option<String>,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
    pub auto_start: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpTransport {
    Stdio,
    Http,
    WebSocket,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpTool {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpResource {
    pub uri: String,
    pub name: String,
    pub description: Option<String>,
    pub mime_type: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpPrompt {
    pub name: String,
    pub description: String,
    pub arguments: Vec<McpPromptArgument>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpPromptArgument {
    pub name: String,
    pub description: String,
    pub required: bool,
}

#[async_trait]
pub trait McpClient: Send + Sync {
    async fn initialize(&mut self) -> Result<(), McpError>;
    async fn list_tools(&self) -> Result<Vec<McpTool>, McpError>;
    async fn call_tool(&self, name: &str, arguments: serde_json::Value) -> Result<serde_json::Value, McpError>;
    async fn list_resources(&self) -> Result<Vec<McpResource>, McpError>;
    async fn read_resource(&self, uri: &str) -> Result<String, McpError>;
    async fn list_prompts(&self) -> Result<Vec<McpPrompt>, McpError>;
    async fn get_prompt(&self, name: &str, arguments: serde_json::Value) -> Result<String, McpError>;
    async fn shutdown(&mut self) -> Result<(), McpError>;
}

pub struct StdioMcpClient {
    config: McpServerConfig,
    process: Option<tokio::process::Child>,
    stdin: Option<tokio::process::ChildStdin>,
    stdout: Option<tokio::process::ChildStdout>,
    #[allow(dead_code)]
    request_id: u64,
    #[allow(dead_code)]
    pending: HashMap<u64, tokio::sync::oneshot::Sender<Result<serde_json::Value, McpError>>>,
}

impl StdioMcpClient {
    pub fn new(config: McpServerConfig) -> Self {
        Self {
            config,
            process: None,
            stdin: None,
            stdout: None,
            request_id: 0,
            pending: HashMap::new(),
        }
    }

    async fn start_process(&mut self) -> Result<(), McpError> {
        let mut cmd = Command::new(self.config.command.as_ref().ok_or(McpError::Protocol("No command".into()))?);
        cmd.args(&self.config.args);
        for (k, v) in &self.config.env {
            cmd.env(k, v);
        }
        cmd.stdin(std::process::Stdio::piped());
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());

        let mut child = cmd.spawn().map_err(|e| McpError::Io(e))?;
        
        self.stdin = child.stdin.take();
        self.stdout = child.stdout.take();
        self.process = Some(child);

        Ok(())
    }
}

#[async_trait]
impl McpClient for StdioMcpClient {
    async fn initialize(&mut self) -> Result<(), McpError> {
        self.start_process().await?;
        // Send initialize request
        Ok(())
    }

    async fn list_tools(&self) -> Result<Vec<McpTool>, McpError> {
        Ok(vec![])
    }

    async fn call_tool(&self, _name: &str, _arguments: serde_json::Value) -> Result<serde_json::Value, McpError> {
        Ok(serde_json::json!({}))
    }

    async fn list_resources(&self) -> Result<Vec<McpResource>, McpError> {
        Ok(vec![])
    }

    async fn read_resource(&self, _uri: &str) -> Result<String, McpError> {
        Ok(String::new())
    }

    async fn list_prompts(&self) -> Result<Vec<McpPrompt>, McpError> {
        Ok(vec![])
    }

    async fn get_prompt(&self, _name: &str, _arguments: serde_json::Value) -> Result<String, McpError> {
        Ok(String::new())
    }

    async fn shutdown(&mut self) -> Result<(), McpError> {
        if let Some(mut process) = self.process.take() {
            process.kill().await.map_err(McpError::Io)?;
        }
        Ok(())
    }
}

pub struct McpServer {
    tools: HashMap<String, McpTool>,
    resources: HashMap<String, McpResource>,
    prompts: HashMap<String, McpPrompt>,
    tool_handlers: HashMap<String, Box<dyn Fn(serde_json::Value) -> Result<serde_json::Value, McpError> + Send + Sync>>,
}

impl McpServer {
    pub fn new() -> Self {
        Self {
            tools: HashMap::new(),
            resources: HashMap::new(),
            prompts: HashMap::new(),
            tool_handlers: HashMap::new(),
        }
    }

    pub fn register_tool(&mut self, tool: McpTool, handler: impl Fn(serde_json::Value) -> Result<serde_json::Value, McpError> + Send + Sync + 'static) {
        let tool_name = tool.name.clone();
        self.tools.insert(tool_name.clone(), tool);
        self.tool_handlers.insert(tool_name, Box::new(handler));
    }

    pub fn register_resource(&mut self, resource: McpResource) {
        self.resources.insert(resource.uri.clone(), resource);
    }

    pub fn register_prompt(&mut self, prompt: McpPrompt) {
        self.prompts.insert(prompt.name.clone(), prompt);
    }

    pub async fn run_stdio(&self) -> Result<(), McpError> {
        // Implement stdio server loop
        Ok(())
    }
}

impl Default for McpServer {
    fn default() -> Self {
        Self::new()
    }
}
