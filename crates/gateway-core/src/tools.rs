use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::Duration;
use tokio::process::Command;
use tracing::{error, info, warn};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    pub tool_call_id: String,
    pub output: String,
    pub success: bool,
}

pub struct ToolRegistry {
    // Can hold extra configuration or dynamic tools in the future
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self {}
    }

    pub fn get_definitions(&self) -> Vec<serde_json::Value> {
        vec![
            serde_json::json!({
                "type": "function",
                "function": {
                    "name": "terminal",
                    "description": "Execute a shell command using 'sh -c' with timeout and output capture.",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "command": {
                                "type": "string",
                                "description": "The shell command to execute"
                            }
                        },
                        "required": ["command"]
                    }
                }
            }),
            serde_json::json!({
                "type": "function",
                "function": {
                    "name": "read_file",
                    "description": "Read the text contents of a file at the specified path.",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "path": {
                                "type": "string",
                                "description": "The file path to read"
                            }
                        },
                        "required": ["path"]
                    }
                }
            }),
            serde_json::json!({
                "type": "function",
                "function": {
                    "name": "write_file",
                    "description": "Write or overwrite content to a file at the specified path.",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "path": {
                                "type": "string",
                                "description": "The file path to write to"
                            },
                            "content": {
                                "type": "string",
                                "description": "The text content to write"
                            }
                        },
                        "required": ["path", "content"]
                    }
                }
            }),
        ]
    }

    pub async fn execute(&self, call: &ToolCall) -> ToolResult {
        info!("Executing tool: {} with id: {}", call.name, call.id);
        match call.name.as_str() {
            "terminal" => self.execute_terminal(call).await,
            "read_file" => self.execute_read_file(call).await,
            "write_file" => self.execute_write_file(call).await,
            other => ToolResult {
                tool_call_id: call.id.clone(),
                output: format!("Unknown tool: {}", other),
                success: false,
            },
        }
    }

    async fn execute_terminal(&self, call: &ToolCall) -> ToolResult {
        let command_str = match call.arguments.get("command").and_then(|v| v.as_str()) {
            Some(cmd) => cmd,
            None => {
                return ToolResult {
                    tool_call_id: call.id.clone(),
                    output: "Missing 'command' argument or argument is not a string".to_string(),
                    success: false,
                };
            }
        };

        let timeout_duration = Duration::from_secs(60);
        let command_fut = Command::new("sh").arg("-c").arg(command_str).output();

        match tokio::time::timeout(timeout_duration, command_fut).await {
            Ok(Ok(output)) => {
                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);
                let exit_status = output.status;

                let mut combined = String::new();
                if !stdout.is_empty() {
                    combined.push_str(&stdout);
                }
                if !stderr.is_empty() {
                    if !combined.is_empty() {
                        combined.push('\n');
                    }
                    combined.push_str(&stderr);
                }
                if combined.is_empty() {
                    combined = format!("(command exited with code: {:?})", exit_status.code());
                }

                ToolResult {
                    tool_call_id: call.id.clone(),
                    output: combined,
                    success: exit_status.success(),
                }
            }
            Ok(Err(e)) => {
                error!("Failed to execute terminal command: {}", e);
                ToolResult {
                    tool_call_id: call.id.clone(),
                    output: format!("Command execution error: {}", e),
                    success: false,
                }
            }
            Err(_) => {
                warn!("Terminal command timed out after 60s");
                ToolResult {
                    tool_call_id: call.id.clone(),
                    output: "Command timed out after 60 seconds".to_string(),
                    success: false,
                }
            }
        }
    }

    async fn execute_read_file(&self, call: &ToolCall) -> ToolResult {
        let path_str = match call.arguments.get("path").and_then(|v| v.as_str()) {
            Some(p) => p,
            None => {
                return ToolResult {
                    tool_call_id: call.id.clone(),
                    output: "Missing 'path' argument or argument is not a string".to_string(),
                    success: false,
                };
            }
        };

        match tokio::fs::read_to_string(path_str).await {
            Ok(content) => ToolResult {
                tool_call_id: call.id.clone(),
                output: content,
                success: true,
            },
            Err(e) => ToolResult {
                tool_call_id: call.id.clone(),
                output: format!("Failed to read file '{}': {}", path_str, e),
                success: false,
            },
        }
    }

    async fn execute_write_file(&self, call: &ToolCall) -> ToolResult {
        let path_str = match call.arguments.get("path").and_then(|v| v.as_str()) {
            Some(p) => p,
            None => {
                return ToolResult {
                    tool_call_id: call.id.clone(),
                    output: "Missing 'path' argument or argument is not a string".to_string(),
                    success: false,
                };
            }
        };

        let content = match call.arguments.get("content").and_then(|v| v.as_str()) {
            Some(c) => c,
            None => {
                return ToolResult {
                    tool_call_id: call.id.clone(),
                    output: "Missing 'content' argument or argument is not a string".to_string(),
                    success: false,
                };
            }
        };

        if let Some(parent) = Path::new(path_str).parent() {
            if !parent.as_os_str().is_empty() {
                if let Err(e) = tokio::fs::create_dir_all(parent).await {
                    return ToolResult {
                        tool_call_id: call.id.clone(),
                        output: format!("Failed to create parent directories for '{}': {}", path_str, e),
                        success: false,
                    };
                }
            }
        }

        match tokio::fs::write(path_str, content).await {
            Ok(_) => ToolResult {
                tool_call_id: call.id.clone(),
                output: format!("Successfully wrote {} bytes to '{}'", content.len(), path_str),
                success: true,
            },
            Err(e) => ToolResult {
                tool_call_id: call.id.clone(),
                output: format!("Failed to write file '{}': {}", path_str, e),
                success: false,
            },
        }
    }
}
