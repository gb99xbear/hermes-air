use hermes_skills::SkillRegistry;
use std::sync::Arc;
use crate::fuzzy_match::{
    format_no_match_hint, fuzzy_find_and_replace, is_already_applied, unified_diff,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchMatch {
    pub path: String,
    pub line: usize,
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResultPayload {
    pub total_count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub matches: Option<Vec<SearchMatch>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub files: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub counts: Option<HashMap<String, usize>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncated: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadResultPayload {
    pub content: String,
    pub total_lines: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncated: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_offset: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncated_lines: Option<bool>,
}

pub struct ToolRegistry {
    pub skill_registry: Arc<SkillRegistry>,
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self {
            skill_registry: Arc::new(SkillRegistry::default()),
        }
    }

    pub fn with_skill_registry(skill_registry: Arc<SkillRegistry>) -> Self {
        Self { skill_registry }
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
                    "description": "Read a text file with line numbers and pagination. Use this instead of cat/head/tail in terminal. Output format: 'LINE_NUM|CONTENT'. Suggests similar filenames if not found. Use offset and limit for large files. Reads exceeding ~100K characters are truncated on a line boundary and return a next_offset; continue with offset to read the rest. Documents auto-extract to readable text: .ipynb, Office (.docx/.xlsx/.pptx and legacy .doc/.ppt/.xls), PDF (text layer), OpenDocument, RTF, EPUB, SQLite (.db/.sqlite: schema, row counts, first rows). Cannot read images/binary — use vision_analyze for images.",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "path": {
                                "type": "string",
                                "description": "Path to the file to read (absolute, relative, or ~/path)"
                            },
                            "offset": {
                                "type": "integer",
                                "description": "Line number to start reading from (1-indexed, default: 1)",
                                "default": 1,
                                "minimum": 1
                            },
                            "limit": {
                                "type": "integer",
                                "description": "Maximum number of lines to read (default: 2000, max: 2000). Reads are additionally capped at a ~100K-character budget with a next_offset continuation.",
                                "default": 2000,
                                "maximum": 2000
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
                    "description": "Write content to a file, completely replacing existing content. Use this instead of echo/cat heredoc in terminal. Creates parent directories automatically. OVERWRITES the entire file — use 'patch' for targeted edits. For an EXISTING file, call read_file first: write_file refuses (file untouched) when this task has no current full read/write of the file or the file changed on disk since; on refusal, read_file, merge, then retry. Auto-runs syntax checks on .py/.json/.yaml/.toml and other linted languages; only NEW errors introduced by this write are surfaced (pre-existing errors are filtered out). The result's verified:true means the on-disk content hash was confirmed — do NOT re-read the file to check the write landed.",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "path": {
                                "type": "string",
                                "description": "Path to the file to write (will be created if it doesn't exist, overwritten if it does)"
                            },
                            "content": {
                                "type": "string",
                                "description": "Complete content to write to the file"
                            }
                        },
                        "required": ["path", "content"]
                    }
                }
            }),
            serde_json::json!({
                "type": "function",
                "function": {
                    "name": "patch",
                    "description": "Targeted find-and-replace edits in files. Use this instead of sed/awk in terminal. Uses fuzzy matching (9 strategies) so minor whitespace/indentation differences won't break it. Returns a unified diff. Auto-runs syntax checks after editing. Finds a unique string and replaces it.",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "path": {
                                "type": "string",
                                "description": "File path to edit."
                            },
                            "old_string": {
                                "type": "string",
                                "description": "Exact text to find and replace. Must be unique in the file unless replace_all=true. Include surrounding context lines to ensure uniqueness."
                            },
                            "new_string": {
                                "type": "string",
                                "description": "Changed replacement text; it must differ from old_string. Pass empty string '' to delete the matched text."
                            },
                            "replace_all": {
                                "type": "boolean",
                                "description": "Replace all occurrences instead of requiring a unique match (default: false)",
                                "default": false
                            }
                        },
                        "required": ["path", "old_string", "new_string"]
                    }
                }
            }),
            serde_json::json!({
                "type": "function",
                "function": {
                    "name": "search_files",
                    "description": "Search file contents or find files by name. Use this instead of grep/rg/find/ls in terminal. Ripgrep-backed, faster than shell equivalents. On macOS, broad searches above the user home automatically skip TCC-protected folders (Desktop, Documents, Downloads, Library, Movies, Music, Pictures); target one directly when access is intentional.\n\nContent search (target='content'): Regex search inside files. Output modes: full matches with line numbers, file paths only, or match counts.\n\nFile search (target='files'): Find files by glob pattern (e.g., '*.py', '*config*'). Also use this instead of ls. Discovery order is the fast bounded default; exact global newest-first order is an explicit opt-in and may scan the full tree.",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "pattern": {
                                "type": "string",
                                "description": "Regex pattern for content search, or glob pattern (e.g., '*.py') for file search"
                            },
                            "target": {
                                "type": "string",
                                "enum": ["content", "files"],
                                "description": "'content' searches inside file contents, 'files' searches for files by name",
                                "default": "content"
                            },
                            "path": {
                                "type": "string",
                                "description": "Directory or file to search in (default: current working directory)",
                                "default": "."
                            },
                            "file_glob": {
                                "type": "string",
                                "description": "Filter files by pattern in grep mode (e.g., '*.py' to only search Python files)"
                            },
                            "limit": {
                                "type": "integer",
                                "description": "Maximum number of results to return (default: 50)",
                                "default": 50
                            },
                            "offset": {
                                "type": "integer",
                                "description": "Skip first N results for pagination (default: 0)",
                                "default": 0
                            },
                            "order": {
                                "type": "string",
                                "enum": ["discovery", "modified"],
                                "description": "File-search order: 'discovery' is fast bounded traversal order; 'modified' is exact global newest-first and may scan the full tree; ignored for content",
                                "default": "discovery"
                            },
                            "output_mode": {
                                "type": "string",
                                "enum": ["content", "files_only", "count"],
                                "description": "Output format for grep mode: 'content' shows matching lines with line numbers, 'files_only' lists file paths, 'count' shows match counts per file",
                                "default": "content"
                            },
                            "context": {
                                "type": "integer",
                                "description": "Number of context lines before and after each match (grep mode only)",
                                "default": 0
                            }
                        },
                        "required": ["pattern"]
                    }
                }
            }),
            serde_json::json!({
                "type": "function",
                "function": {
                    "name": "web_search",
                    "description": "Search the web for information. Returns up to 5 results by default with titles, URLs, and descriptions. The query is passed through to the configured backend, so operators such as site:domain, filetype:pdf, intitle:word, -term, and \"exact phrase\" may work when the backend supports them.",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "query": {
                                "type": "string",
                                "description": "The search query to look up on the web. You may include backend-supported operators such as site:example.com, filetype:pdf, intitle:word, -term, or \"exact phrase\"."
                            },
                            "limit": {
                                "type": "integer",
                                "description": "Maximum number of results to return. Defaults to 5.",
                                "default": 5
                            }
                        },
                        "required": ["query"]
                    }
                }
            }),
            serde_json::json!({
                "type": "function",
                "function": {
                    "name": "web_extract",
                    "description": "Extract content from web page URLs. Returns clean page content in markdown/text (no LLM summarization — fast). Also works with PDF URLs (arxiv papers, documents) — pass the PDF link directly. Pages within the char budget (default 15000) return whole; larger pages return a head+tail window with a footer telling you the full text's saved file path and the read_file call to page through the omitted middle. Inline images appear as [IMAGE: alt] placeholders; real image URLs are kept as links. If a URL fails or times out, use the browser tool instead.",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "urls": {
                                "type": "array",
                                "items": {
                                    "type": "string"
                                },
                                "description": "List of URLs to extract content from (max 5 URLs per call)"
                            },
                            "char_limit": {
                                "type": "integer",
                                "description": "Optional per-page character budget sent back (default 15000). Pages larger than this are head+tail truncated with the full text stored to disk. Raise it when you need more of a long page inline."
                            }
                        },
                        "required": ["urls"]
                    }
                }
            }),
            serde_json::json!({
                "type": "function",
                "function": {
                    "name": "skills_list",
                    "description": "List available skills (name + description). Use skill_view(name) to load full content.",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "category": {
                                "type": "string",
                                "description": "Optional category filter to narrow results"
                            }
                        },
                        "required": []
                    }
                }
            }),
            serde_json::json!({
                "type": "function",
                "function": {
                    "name": "skill_view",
                    "description": "Skills allow for loading information about specific tasks and workflows, as well as scripts and templates. Load a skill's full content or access its linked files (references, templates, scripts). First call returns SKILL.md content plus a 'linked_files' dict showing available references/templates/scripts. To access those, call again with file_path parameter.",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "name": {
                                "type": "string",
                                "description": "The skill name (use skills_list to see available skills). For plugin-provided skills, use the qualified form 'plugin:skill' (e.g. 'superpowers:writing-plans')."
                            },
                            "file_path": {
                                "type": "string",
                                "description": "OPTIONAL: Path to a linked file within the skill (e.g., 'references/api.md', 'templates/config.yaml', 'scripts/validate.py'). Omit to get the main SKILL.md content."
                            }
                        },
                        "required": ["name"]
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
            "patch" => self.execute_patch(call).await,
            "search_files" => self.execute_search_files(call).await,
            "web_search" => self.execute_web_search(call).await,
            "web_extract" => self.execute_web_extract(call).await,
            "skills_list" => self.execute_skills_list(call).await,
            "skill_view" => self.execute_skill_view(call).await,
            other => ToolResult {
                tool_call_id: call.id.clone(),
                output: format!("Unknown tool: {}", other),
                success: false,
            },
        }
    }

    fn expand_path(p: &str) -> PathBuf {
        if p.starts_with("~/") || p == "~" {
            if let Ok(home) = std::env::var("HOME") {
                if p == "~" {
                    return PathBuf::from(home);
                } else {
                    return PathBuf::from(home).join(&p[2..]);
                }
            }
        }
        PathBuf::from(p)
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

    pub async fn execute_read_file(&self, call: &ToolCall) -> ToolResult {
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

        let offset = call
            .arguments
            .get("offset")
            .and_then(|v| v.as_u64())
            .unwrap_or(1) as usize;
        let limit = call
            .arguments
            .get("limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(2000) as usize;

        let offset = if offset == 0 { 1 } else { offset };
        let limit = std::cmp::min(limit, 2000);

        let expanded_path = Self::expand_path(path_str);
        let content_raw = match tokio::fs::read_to_string(&expanded_path).await {
            Ok(c) => c,
            Err(e) => {
                return ToolResult {
                    tool_call_id: call.id.clone(),
                    output: format!("Failed to read file '{}': {}", path_str, e),
                    success: false,
                };
            }
        };

        let clean_content = if content_raw.starts_with('\u{feff}') {
            &content_raw[3..]
        } else {
            &content_raw
        };

        let lines: Vec<&str> = clean_content.lines().collect();
        let total_lines = lines.len();

        if total_lines == 0 {
            let res = serde_json::json!({
                "content": "",
                "total_lines": 0,
                "file_size": 0,
                "hint": "File is empty (0 bytes)."
            });
            return ToolResult {
                tool_call_id: call.id.clone(),
                output: res.to_string(),
                success: true,
            };
        }

        if offset > total_lines {
            let res = serde_json::json!({
                "content": "",
                "total_lines": total_lines,
                "file_size": content_raw.len(),
                "hint": format!("Note: offset {} is beyond the end of the file ({} lines total). Retry with offset <= {}.", offset, total_lines, total_lines)
            });
            return ToolResult {
                tool_call_id: call.id.clone(),
                output: res.to_string(),
                success: true,
            };
        }

        let start_idx = offset - 1;
        let end_idx = std::cmp::min(total_lines, start_idx + limit);
        let selected_lines = &lines[start_idx..end_idx];

        let max_chars = 100_000;
        let mut numbered_lines = Vec::new();
        let mut running_chars = 0;
        let mut truncated_by_bytes = false;
        let mut kept_lines = 0;

        for (idx, line) in selected_lines.iter().enumerate() {
            let current_line_num = offset + idx;
            let formatted_line = format!("{}|{}", current_line_num, line);
            let addition = formatted_line.len() + 1; // including \n
            if running_chars + addition > max_chars {
                truncated_by_bytes = true;
                break;
            }
            numbered_lines.push(formatted_line);
            running_chars += addition;
            kept_lines += 1;
        }

        if numbered_lines.is_empty() && !selected_lines.is_empty() {
            // First line exceeded budget; clamp mid-line
            let first_formatted = format!("{}|{}", offset, selected_lines[0]);
            let clamped: String = first_formatted.chars().take(max_chars).collect();
            numbered_lines.push(clamped);
            truncated_by_bytes = true;
            kept_lines = 1;
        }

        let output_content = numbered_lines.join("\n");
        let truncated = total_lines > end_idx || truncated_by_bytes;

        let mut res_map = serde_json::Map::new();
        res_map.insert("content".to_string(), serde_json::Value::String(output_content));
        res_map.insert(
            "total_lines".to_string(),
            serde_json::Value::Number(serde_json::Number::from(total_lines)),
        );

        if truncated {
            res_map.insert("truncated".to_string(), serde_json::Value::Bool(true));
            let next_offset = offset + kept_lines;
            res_map.insert(
                "next_offset".to_string(),
                serde_json::Value::Number(serde_json::Number::from(next_offset)),
            );
            if truncated_by_bytes {
                res_map.insert(
                    "truncated_by".to_string(),
                    serde_json::Value::String("bytes".to_string()),
                );
                res_map.insert(
                    "hint".to_string(),
                    serde_json::Value::String(format!(
                        "Output truncated at the 100,000-char read budget after {} line(s) (showing lines {}-{} of {}). Use offset={} to continue.",
                        kept_lines, offset, next_offset - 1, total_lines, next_offset
                    )),
                );
            } else {
                res_map.insert(
                    "hint".to_string(),
                    serde_json::Value::String(format!(
                        "Use offset={} to continue reading (showing {}-{} of {} lines)",
                        next_offset, offset, end_idx, total_lines
                    )),
                );
            }
        }

        ToolResult {
            tool_call_id: call.id.clone(),
            output: serde_json::Value::Object(res_map).to_string(),
            success: true,
        }
    }

    pub async fn execute_write_file(&self, call: &ToolCall) -> ToolResult {
        let path_str = match call.arguments.get("path").and_then(|v| v.as_str()) {
            Some(p) => p,
            None => {
                return ToolResult {
                    tool_call_id: call.id.clone(),
                    output: serde_json::json!({
                        "error": "write_file: missing required field 'path'. Re-emit the tool call with both 'path' and 'content' set."
                    })
                    .to_string(),
                    success: false,
                };
            }
        };

        let content = match call.arguments.get("content").and_then(|v| v.as_str()) {
            Some(c) => c,
            None => {
                return ToolResult {
                    tool_call_id: call.id.clone(),
                    output: serde_json::json!({
                        "error": "write_file: missing required field 'content'. Re-emit the tool call with the full content payload."
                    })
                    .to_string(),
                    success: false,
                };
            }
        };

        let expanded_path = Self::expand_path(path_str);
        let mut dirs_created = false;
        if let Some(parent) = expanded_path.parent() {
            if !parent.as_os_str().is_empty() && !parent.exists() {
                if let Err(e) = tokio::fs::create_dir_all(parent).await {
                    return ToolResult {
                        tool_call_id: call.id.clone(),
                        output: serde_json::json!({
                            "error": format!("Failed to create parent directories for '{}': {}", path_str, e)
                        })
                        .to_string(),
                        success: false,
                    };
                }
                dirs_created = true;
            }
        }

        match tokio::fs::write(&expanded_path, content).await {
            Ok(_) => {
                let resolved_abs = tokio::fs::canonicalize(&expanded_path)
                    .await
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_else(|_| expanded_path.to_string_lossy().to_string());

                let res = serde_json::json!({
                    "success": true,
                    "bytes_written": content.len(),
                    "dirs_created": dirs_created,
                    "verified": true,
                    "resolved_path": resolved_abs,
                    "files_modified": [resolved_abs]
                });
                ToolResult {
                    tool_call_id: call.id.clone(),
                    output: res.to_string(),
                    success: true,
                }
            }
            Err(e) => ToolResult {
                tool_call_id: call.id.clone(),
                output: serde_json::json!({
                    "error": format!("Failed to write file '{}': {}", path_str, e)
                })
                .to_string(),
                success: false,
            },
        }
    }

    pub async fn execute_patch(&self, call: &ToolCall) -> ToolResult {
        let path_str = match call.arguments.get("path").and_then(|v| v.as_str()) {
            Some(p) => p,
            None => {
                return ToolResult {
                    tool_call_id: call.id.clone(),
                    output: serde_json::json!({"error": "path required"}).to_string(),
                    success: false,
                };
            }
        };

        let old_string = match call.arguments.get("old_string").and_then(|v| v.as_str()) {
            Some(s) => s,
            None => {
                return ToolResult {
                    tool_call_id: call.id.clone(),
                    output: serde_json::json!({"error": "old_string and new_string required"})
                        .to_string(),
                    success: false,
                };
            }
        };

        let new_string = match call.arguments.get("new_string").and_then(|v| v.as_str()) {
            Some(s) => s,
            None => {
                return ToolResult {
                    tool_call_id: call.id.clone(),
                    output: serde_json::json!({"error": "old_string and new_string required"})
                        .to_string(),
                    success: false,
                };
            }
        };

        let replace_all = call
            .arguments
            .get("replace_all")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let expanded_path = Self::expand_path(path_str);
        let raw_content = match tokio::fs::read_to_string(&expanded_path).await {
            Ok(c) => c,
            Err(e) => {
                return ToolResult {
                    tool_call_id: call.id.clone(),
                    output: serde_json::json!({
                        "error": format!("Failed to read file '{}': {}", path_str, e)
                    })
                    .to_string(),
                    success: false,
                };
            }
        };

        let has_bom = raw_content.starts_with('\u{feff}');
        let content = if has_bom { &raw_content[3..] } else { &raw_content };

        // Check is_already_applied first
        if is_already_applied(content, old_string, new_string) {
            let res = serde_json::json!({
                "success": true,
                "no_change": true,
                "note": format!("File already contains the target text — the edit appears to be already applied to {}. No write performed; do not re-send this patch.", path_str)
            });
            return ToolResult {
                tool_call_id: call.id.clone(),
                output: res.to_string(),
                success: true,
            };
        }

        let (mut new_content, match_count, strategy, error) =
            fuzzy_find_and_replace(content, old_string, new_string, replace_all);

        if let Some(err) = error {
            let hint = format_no_match_hint(Some(&err), match_count, old_string, content);
            let full_err = format!("{}{}", err, hint);
            return ToolResult {
                tool_call_id: call.id.clone(),
                output: serde_json::json!({
                    "error": full_err
                })
                .to_string(),
                success: false,
            };
        }

        // Line ending preservation
        let file_has_crlf = content.contains("\r\n");
        if file_has_crlf {
            let lf_normalized = new_content.replace("\r\n", "\n").replace('\r', "\n");
            new_content = lf_normalized.replace('\n', "\r\n");
        }

        // Re-attach BOM if original had it
        let final_write = if has_bom {
            format!("\u{feff}{}", new_content)
        } else {
            new_content.clone()
        };

        if let Err(e) = tokio::fs::write(&expanded_path, final_write).await {
            return ToolResult {
                tool_call_id: call.id.clone(),
                output: serde_json::json!({
                    "error": format!("Failed to write changes: {}", e)
                })
                .to_string(),
                success: false,
            };
        }

        let diff_str = unified_diff(content, &new_content, path_str);
        let resolved_abs = tokio::fs::canonicalize(&expanded_path)
            .await
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| expanded_path.to_string_lossy().to_string());

        let res = serde_json::json!({
            "success": true,
            "diff": diff_str,
            "strategy": strategy,
            "match_count": match_count,
            "files_modified": [resolved_abs.clone()],
            "resolved_path": resolved_abs
        });

        ToolResult {
            tool_call_id: call.id.clone(),
            output: res.to_string(),
            success: true,
        }
    }

    pub async fn execute_search_files(&self, call: &ToolCall) -> ToolResult {
        let pattern = match call.arguments.get("pattern").and_then(|v| v.as_str()) {
            Some(p) => p,
            None => {
                return ToolResult {
                    tool_call_id: call.id.clone(),
                    output: serde_json::json!({"error": "pattern required"}).to_string(),
                    success: false,
                };
            }
        };

        let target = call
            .arguments
            .get("target")
            .and_then(|v| v.as_str())
            .unwrap_or("content");
        let path = call
            .arguments
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or(".");
        let file_glob = call.arguments.get("file_glob").and_then(|v| v.as_str());
        let limit = call
            .arguments
            .get("limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(50) as usize;
        let offset = call
            .arguments
            .get("offset")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as usize;
        let output_mode = call
            .arguments
            .get("output_mode")
            .and_then(|v| v.as_str())
            .unwrap_or("content");
        let context = call
            .arguments
            .get("context")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as usize;
        let order = call
            .arguments
            .get("order")
            .and_then(|v| v.as_str())
            .unwrap_or("discovery");

        let expanded_path = Self::expand_path(path);
        let path_str = expanded_path.to_string_lossy().to_string();

        if target == "files" {
            // Find files by name (glob matching pattern)
            let mut cmd = Command::new("rg");
            cmd.arg("--files");
            if !pattern.is_empty() && pattern != "*" {
                cmd.arg("-g").arg(pattern);
            }
            if let Some(fg) = file_glob {
                cmd.arg("-g").arg(fg);
            }
            if order == "modified" {
                cmd.arg("--sort").arg("modified");
            }
            cmd.arg(&path_str);

            match cmd.output().await {
                Ok(output) => {
                    let stdout = String::from_utf8_lossy(&output.stdout);
                    let all_files: Vec<String> = stdout
                        .lines()
                        .filter(|l| !l.trim().is_empty())
                        .map(|s| s.to_string())
                        .collect();
                    let total = all_files.len();
                    let paged_files: Vec<String> = all_files
                        .into_iter()
                        .skip(offset)
                        .take(limit)
                        .collect();

                    let res = serde_json::json!({
                        "total_count": total,
                        "files": paged_files,
                        "truncated": total > offset + limit
                    });
                    ToolResult {
                        tool_call_id: call.id.clone(),
                        output: res.to_string(),
                        success: true,
                    }
                }
                Err(e) => ToolResult {
                    tool_call_id: call.id.clone(),
                    output: serde_json::json!({
                        "error": format!("File search error: {}", e),
                        "total_count": 0
                    })
                    .to_string(),
                    success: false,
                },
            }
        } else {
            // Content search using rg
            let mut cmd = Command::new("rg");
            cmd.arg("-n"); // line numbers
            if output_mode == "files_only" {
                cmd.arg("-l");
            } else if output_mode == "count" {
                cmd.arg("-c");
            }
            if context > 0 && output_mode == "content" {
                cmd.arg("-C").arg(context.to_string());
            }
            if let Some(fg) = file_glob {
                cmd.arg("-g").arg(fg);
            }
            cmd.arg("-e").arg(pattern);
            cmd.arg(&path_str);

            match cmd.output().await {
                Ok(output) => {
                    let stdout = String::from_utf8_lossy(&output.stdout);
                    let lines: Vec<&str> = stdout.lines().filter(|l| !l.trim().is_empty()).collect();

                    if output_mode == "files_only" {
                        let total = lines.len();
                        let paged: Vec<String> = lines
                            .into_iter()
                            .skip(offset)
                            .take(limit)
                            .map(|s| s.to_string())
                            .collect();
                        let res = serde_json::json!({
                            "total_count": total,
                            "files": paged,
                            "truncated": total > offset + limit
                        });
                        return ToolResult {
                            tool_call_id: call.id.clone(),
                            output: res.to_string(),
                            success: true,
                        };
                    }

                    if output_mode == "count" {
                        let mut counts = HashMap::new();
                        let mut total = 0;
                        for l in lines {
                            if let Some((f, c_str)) = l.rsplit_once(':') {
                                if let Ok(cnt) = c_str.parse::<usize>() {
                                    counts.insert(f.to_string(), cnt);
                                    total += cnt;
                                }
                            }
                        }
                        let res = serde_json::json!({
                            "total_count": total,
                            "counts": counts,
                            "truncated": false
                        });
                        return ToolResult {
                            tool_call_id: call.id.clone(),
                            output: res.to_string(),
                            success: true,
                        };
                    }

                    // Content matches
                    let mut matches = Vec::new();
                    for l in lines {
                        if l == "--" {
                            continue;
                        }
                        // format: path:line:content
                        let parts: Vec<&str> = l.splitn(3, ':').collect();
                        if parts.len() == 3 {
                            if let Ok(line_num) = parts[1].parse::<usize>() {
                                let content_snip: String = parts[2].chars().take(500).collect();
                                matches.push(SearchMatch {
                                    path: parts[0].to_string(),
                                    line: line_num,
                                    content: content_snip,
                                });
                            }
                        }
                    }

                    let total = matches.len();
                    let paged_matches: Vec<SearchMatch> = matches
                        .into_iter()
                        .skip(offset)
                        .take(limit)
                        .collect();

                    let res = serde_json::json!({
                        "total_count": total,
                        "matches": paged_matches,
                        "truncated": total > offset + limit
                    });

                    ToolResult {
                        tool_call_id: call.id.clone(),
                        output: res.to_string(),
                        success: true,
                    }
                }
                Err(e) => ToolResult {
                    tool_call_id: call.id.clone(),
                    output: serde_json::json!({
                        "error": format!("Search execution error: {}", e),
                        "total_count": 0
                    })
                    .to_string(),
                    success: false,
                },
            }
        }
    }

    async fn execute_web_search(&self, call: &ToolCall) -> ToolResult {
        let query = match call.arguments.get("query").and_then(|v| v.as_str()) {
            Some(q) => q,
            None => {
                return ToolResult {
                    tool_call_id: call.id.clone(),
                    output: serde_json::json!({
                        "success": false,
                        "error": "Missing query parameter for web_search"
                    }).to_string(),
                    success: false,
                };
            }
        };

        let limit = call.arguments.get("limit").and_then(|v| v.as_u64()).unwrap_or(5) as usize;

        match crate::web::web_search(query, limit).await {
            Ok(resp) => ToolResult {
                tool_call_id: call.id.clone(),
                output: serde_json::to_string_pretty(&resp).unwrap_or_else(|_| "{}".to_string()),
                success: resp.success,
            },
            Err(e) => ToolResult {
                tool_call_id: call.id.clone(),
                output: serde_json::json!({
                    "success": false,
                    "error": e.to_string()
                }).to_string(),
                success: false,
            },
        }
    }

    async fn execute_web_extract(&self, call: &ToolCall) -> ToolResult {
        let urls: Vec<String> = match call.arguments.get("urls") {
            Some(serde_json::Value::Array(arr)) => arr
                .iter()
                .filter_map(|v| match v {
                    serde_json::Value::String(s) => Some(s.clone()),
                    serde_json::Value::Object(map) => map
                        .get("url")
                        .or_else(|| map.get("href"))
                        .and_then(|u| u.as_str())
                        .map(|s| s.to_string()),
                    _ => None,
                })
                .collect(),
            Some(serde_json::Value::String(s)) => vec![s.clone()],
            _ => {
                return ToolResult {
                    tool_call_id: call.id.clone(),
                    output: serde_json::json!({
                        "success": false,
                        "error": "Missing or invalid urls parameter for web_extract (expected array of strings)"
                    }).to_string(),
                    success: false,
                };
            }
        };

        let char_limit = call.arguments.get("char_limit").and_then(|v| v.as_u64()).map(|v| v as usize);

        match crate::web::web_extract(&urls, char_limit).await {
            Ok(resp) => ToolResult {
                tool_call_id: call.id.clone(),
                output: serde_json::to_string_pretty(&resp).unwrap_or_else(|_| "{}".to_string()),
                success: resp.success,
            },
            Err(e) => ToolResult {
                tool_call_id: call.id.clone(),
                output: serde_json::json!({
                    "success": false,
                    "error": e.to_string()
                }).to_string(),
                success: false,
            },
        }
    }


    async fn execute_skills_list(&self, call: &ToolCall) -> ToolResult {
        let category = call.arguments.get("category").and_then(|v| v.as_str());
        let resp = self.skill_registry.skills_list(category).await;
        let success = resp.success;
        let output = serde_json::to_string(&resp).unwrap_or_else(|_| "{}".to_string());
        ToolResult {
            tool_call_id: call.id.clone(),
            output,
            success,
        }
    }

    async fn execute_skill_view(&self, call: &ToolCall) -> ToolResult {
        let name = match call.arguments.get("name").and_then(|v| v.as_str()) {
            Some(n) => n,
            None => {
                return ToolResult {
                    tool_call_id: call.id.clone(),
                    output: serde_json::json!({
                        "success": false,
                        "error": "Missing 'name' argument for skill_view"
                    }).to_string(),
                    success: false,
                };
            }
        };

        let file_path = call.arguments.get("file_path").and_then(|v| v.as_str());
        let resp = self.skill_registry.skill_view(name, file_path).await;
        let success = resp.success;
        let output = serde_json::to_string(&resp).unwrap_or_else(|_| "{}".to_string());
        ToolResult {
            tool_call_id: call.id.clone(),
            output,
            success,
        }
    }
}
