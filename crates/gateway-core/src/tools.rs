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
    pub code_engine: Arc<crate::code_exec::CodeExecutionEngine>,
    pub todo_store: Arc<crate::todo::TodoStore>,
    pub cron_scheduler: Option<Arc<hermes_cron::CronScheduler>>,
    pub delegation_manager: Option<Arc<crate::delegation::DelegationManager>>,
    pub session_search: Option<Arc<crate::session_search::SessionSearchEngine>>,
    pub memory_store: Arc<crate::memory::MemoryStore>,
    pub skill_manager: Arc<crate::skill_manager::SkillManager>,
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
            code_engine: Arc::new(crate::code_exec::CodeExecutionEngine::default()),
            todo_store: Arc::new(crate::todo::TodoStore::default()),
            cron_scheduler: None,
            delegation_manager: None,
            session_search: None,
            memory_store: Arc::new(crate::memory::MemoryStore::new()),
            skill_manager: Arc::new(crate::skill_manager::SkillManager::new(
                crate::skill_manager::default_skills_dir(),
            )),
        }
    }

    pub fn with_skill_registry(skill_registry: Arc<SkillRegistry>) -> Self {
        Self {
            skill_registry,
            code_engine: Arc::new(crate::code_exec::CodeExecutionEngine::default()),
            todo_store: Arc::new(crate::todo::TodoStore::default()),
            cron_scheduler: None,
            delegation_manager: None,
            session_search: None,
            memory_store: Arc::new(crate::memory::MemoryStore::new()),
            skill_manager: Arc::new(crate::skill_manager::SkillManager::new(
                crate::skill_manager::default_skills_dir(),
            )),
        }
    }

    pub fn with_session_search(mut self, session_search: Arc<crate::session_search::SessionSearchEngine>) -> Self {
        self.session_search = Some(session_search);
        self
    }

    pub fn with_memory_store(mut self, memory_store: Arc<crate::memory::MemoryStore>) -> Self {
        self.memory_store = memory_store;
        self
    }

    pub fn with_skill_manager(mut self, skill_manager: Arc<crate::skill_manager::SkillManager>) -> Self {
        self.skill_manager = skill_manager;
        self
    }

    pub fn with_session_store(mut self, session_store: Arc<Box<dyn hermes_session_store::SessionStore>>) -> Self {
        self.session_search = Some(Arc::new(crate::session_search::SessionSearchEngine::new(session_store)));
        self
    }

    pub fn with_todo_store(mut self, todo_store: Arc<crate::todo::TodoStore>) -> Self {
        self.todo_store = todo_store;
        self
    }

    pub fn with_delegation_manager(mut self, delegation_manager: Arc<crate::delegation::DelegationManager>) -> Self {
        self.delegation_manager = Some(delegation_manager);
        self
    }

    pub fn with_cron_scheduler(mut self, cron_scheduler: Arc<hermes_cron::CronScheduler>) -> Self {
        self.cron_scheduler = Some(cron_scheduler);
        self
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
            serde_json::json!({
                "type": "function",
                "function": {
                    "name": "browser_navigate",
                    "description": "Navigate to a URL using browser automation with SSRF protection. Returns the page title, URL, and target ID.",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "url": {
                                "type": "string",
                                "description": "The URL to navigate to (must be public http/https, private IP ranges and internal hostnames are blocked)"
                            }
                        },
                        "required": ["url"]
                    }
                }
            }),
            serde_json::json!({
                "type": "function",
                "function": {
                    "name": "browser_snapshot",
                    "description": "Get a clean textual accessibility and DOM snapshot of the current browser page, including interactive element references (@e1, @e2...).",
                    "parameters": {
                        "type": "object",
                        "properties": {},
                        "required": []
                    }
                }
            }),
            serde_json::json!({
                "type": "function",
                "function": {
                    "name": "browser_click",
                    "description": "Click an element on the current page by CSS selector or (x, y) coordinates.",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "selector": {
                                "type": "string",
                                "description": "CSS selector of element to click (e.g. 'button.submit', '#login')"
                            },
                            "x": {
                                "type": "number",
                                "description": "X coordinate in viewport pixels"
                            },
                            "y": {
                                "type": "number",
                                "description": "Y coordinate in viewport pixels"
                            }
                        },
                        "required": []
                    }
                }
            }),
            serde_json::json!({
                "type": "function",
                "function": {
                    "name": "browser_type",
                    "description": "Type text into an input or textarea element identified by CSS selector.",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "selector": {
                                "type": "string",
                                "description": "CSS selector of element to type into (e.g. 'input#search')"
                            },
                            "text": {
                                "type": "string",
                                "description": "Text to type into the element"
                            }
                        },
                        "required": ["selector", "text"]
                    }
                }
            }),
            serde_json::json!({
                "type": "function",
                "function": {
                    "name": "browser_take_screenshot",
                    "description": "Take a screenshot of the current page so you can inspect it visually. Returns image file path and MEDIA:<file_path> tag. Format can be png or jpeg.",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "format": {
                                "type": "string",
                                "enum": ["png", "jpeg"],
                                "description": "Image format: png (lossless) or jpeg (compressed). Default: png"
                            },
                            "quality": {
                                "type": "integer",
                                "description": "Compression quality for jpeg format (1-100). Default: 80",
                                "minimum": 1,
                                "maximum": 100
                            }
                        },
                        "required": []
                    }
                }
            }),
            serde_json::json!({
                "type": "function",
                "function": {
                    "name": "browser_exec",
                    "description": "Execute browser automation JavaScript or CDP script commands on the active tab.",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "code": {
                                "type": "string",
                                "description": "JavaScript or CDP script code to evaluate on the page"
                            }
                        },
                        "required": ["code"]
                    }
                }
            }),
            serde_json::json!({
                "type": "function",
                "function": {
                    "name": "todo_list",
                    "description": "Track a task list for multi-step work (3+ steps). Use for complex tasks with 3+ steps or when the user provides multiple tasks. For 'all N items' tasks, enumerate every instance as its own checklist item so none are silently dropped. Call with no parameters to read the current list.\nList order is priority. Only ONE item in_progress at a time. Break large phases into subtasks via parent. Mark an item completed only after the work is verified done, never based on intent. If something fails, cancel it and add a revised item. Always returns the full current list.",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "todos": {
                                "type": "array",
                                "description": "Task items to write.",
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "id": {
                                            "type": "string"
                                        },
                                        "content": {
                                            "type": "string",
                                            "description": "Task description"
                                        },
                                        "status": {
                                            "type": "string",
                                            "enum": [
                                                "pending",
                                                "in_progress",
                                                "completed",
                                                "cancelled"
                                            ]
                                        },
                                        "parent": {
                                            "type": "string",
                                            "description": "Optional id of another item, making this a nested subtask. Omit for top-level."
                                        }
                                    },
                                    "required": [
                                        "id",
                                        "content",
                                        "status"
                                    ]
                                }
                            },
                            "merge": {
                                "type": "boolean",
                                "description": "true: update existing items by id, add new ones. false (default): replace the entire list with a fresh plan.",
                                "default": false
                            }
                        },
                        "required": []
                    }
                }
            }),
            serde_json::json!({
                "type": "function",
                "function": {
                    "name": "cronjob_manage",
                    "description": "Manage scheduled cron jobs: action='create' schedules a job from a prompt and/or skills; 'list' inspects jobs; 'update'/'pause'/'resume'/'remove' manage one by job_id (always list first — never guess job IDs); 'run' fires a job immediately in the BACKGROUND (returns a handle at once, outcome re-enters the conversation when done — do not wait or poll; optional 'prompt' adds transient context for that fire only).\n\nJobs run on the main agent model (whatever `hermes model` is set to when they fire) unless pinned.\n\nJobs run in a fresh session with no current-chat context, so prompts must be self-contained, and the agent's FINAL RESPONSE is what gets delivered — cron runs are autonomous and cannot ask questions. Jobs run on the main agent model (whatever `hermes model` is set to when they fire) unless the user pins one. Prefer updating an existing job over creating near-duplicates.",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "paused": {
                                "type": "boolean",
                                "description": "Create only: persist disabled atomically. Resume to schedule; explicit run remains available. Default false."
                            },
                            "paused_reason": {
                                "type": "string",
                                "description": "Create only: auditable reason; requires paused=true."
                            },
                            "action": {
                                "type": "string",
                                "description": "One of: create, list, update, pause, resume, remove, run. When action=create, the 'schedule' and 'prompt' fields are REQUIRED."
                            },
                            "job_id": {
                                "type": "string",
                                "description": "Required for update/pause/resume/remove/run."
                            },
                            "pinned": {
                                "type": "boolean",
                                "description": "For create/update. ONLY set when the user explicitly asks to pin (or unpin) a job's model. pinned=true locks the CURRENT main agent model (and its provider) onto the job so later `hermes model` / `/model` changes never touch it; pinned=false releases the lock so the job follows the main agent model again. Never set it on your own initiative: by default jobs follow the main model."
                            },
                            "prompt": {
                                "type": "string",
                                "description": "For create: the full self-contained prompt (paired with any skills as the task instruction). For run: optional transient context for that single fire (never persisted)."
                            },
                            "schedule": {
                                "type": "string",
                                "description": "REQUIRED for create. Schedule forms: (1) recurring interval — '30m', 'every 2h', 'every hour' (EVERY 30 minutes / 2 hours / hour, forever by default); (2) explicit one-shot by duration — 'in 30m', 'in 2h' (fires ONCE that far from now; use this for 'remind me in N minutes' — do NOT hand-compute an absolute timestamp); (3) natural day/time — 'every monday 9am', 'weekdays at 9am', 'every day at 9am' (recurring weekly/daily); (4) cron syntax — '0 9 * * *' (daily 9am); (5) absolute one-shot — ISO timestamp '2026-06-01T09:00:00'."
                            },
                            "name": {
                                "type": "string",
                                "description": "Optional human-friendly name"
                            },
                            "repeat": {
                                "type": "integer",
                                "description": "Optional repeat count. Omit for defaults (once for one-shot, forever for recurring)."
                            },
                            "deliver": {
                                "type": "string",
                                "description": "Where the job's output is POSTED as a one-way message (the job itself always runs in a fresh session with no chat context). Omit to address the chat/topic this job was created from. Otherwise: 'local' (save only, no delivery), 'all' (every connected home channel, resolved at fire time), 'bot-chat' or 'bot-chat:<profile>' (inject into a Bot Chat as a real message), or platform:chat_id:thread_id (e.g. 'telegram:-1001234567890:17585'). Comma-combine like 'origin,all'."
                            },
                            "failure_deliver": {
                                "type": "string",
                                "description": "Optional override target for FAILURE notices only (same grammar as deliver). When set, engine failure/interruption notices go here instead of the deliver target; 'local' suppresses them entirely (state still recorded in cron list/run history). Use for jobs delivering into shared channels where failure noise is unwanted. Omit = failures follow deliver (default). On update, '' clears."
                            },
                            "skills": {
                                "type": "array",
                                "items": {
                                    "type": "string"
                                },
                                "description": "Optional ordered skill names loaded before the cron prompt. On update, [] clears."
                            },
                            "script": {
                                "type": "string",
                                "description": "Optional script run each tick; stdout is injected into the agent's prompt as context (with no_agent=True the script IS the job). Relative paths resolve under ~/.hermes/scripts/; .sh/.bash via bash, else Python. On update, '' clears."
                            },
                            "monitor": {
                                "type": "string",
                                "description": "Optional change-detector that gates the agent: an http(s) URL (fetched each tick) or a script path (same rules as `script`, run each tick) — cheap, no LLM. Output identical to the previous tick skips the agent run entirely; changed output wakes the agent with a diff injected into the prompt. First tick always runs (baseline). Output must be deterministic (no timestamps) or every tick looks changed. Incompatible with no_agent. On update, '' clears."
                            },
                            "no_agent": {
                                "type": "boolean",
                                "default": false,
                                "description": "True = no LLM: the scheduler runs `script` (required) on schedule and delivers its stdout verbatim; empty stdout sends nothing (watchdog pattern). Use for script-only pings with fixed output; keep False for anything needing reasoning."
                            },
                            "context_from": {
                                "type": "array",
                                "items": {
                                    "type": "string"
                                },
                                "description": "Optional job ID(s) whose most recent completed output is injected as context each run — chains jobs (A collects, B processes). For a job's OWN previous output prefer `continuity`. On update, [] clears."
                            }
                        },
                        "required": ["action"]
                    }
                }
            }),
            serde_json::json!({
                "type": "function",
                "function": {
                    "name": "delegate_task",
                    "description": "Spawn one or more subagents in isolated contexts, or manage running ones. For action='spawn', runs subagent tasks with fresh context and returns consolidated summaries. For action='list', returns running subagents. For action='steer', injects course-correction text mid-flight. For action='stop', cancels a running subagent.",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "tasks": {
                                "type": "array",
                                "description": "List of tasks to execute as subagents",
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "goal": {
                                            "type": "string",
                                            "description": "What this subagent should accomplish. Be specific and self-contained."
                                        },
                                        "context": {
                                            "type": "string",
                                            "description": "Background this child needs: file paths, error messages, constraints."
                                        },
                                        "output_schema": {
                                            "type": "object",
                                            "description": "Optional JSON Schema this child's final answer must validate against."
                                        },
                                        "images": {
                                            "type": "array",
                                            "items": {"type": "string"},
                                            "description": "Optional image file paths or URLs this child must see."
                                        },
                                        "group": {
                                            "type": "string",
                                            "description": "Optional result-delivery bucket within this call."
                                        }
                                    },
                                    "required": ["goal"]
                                }
                            },
                            "action": {
                                "type": "string",
                                "description": "Default 'spawn'. Options: 'spawn', 'list', 'steer', 'stop'.",
                                "enum": ["spawn", "list", "steer", "stop"]
                            },
                            "subagent_id": {
                                "type": "string",
                                "description": "Target subagent ID for action='steer' or action='stop'."
                            },
                            "message": {
                                "type": "string",
                                "description": "Course-correction instruction for action='steer'."
                            }
                        },
                        "required": []
                    }
                }
            }),
            serde_json::json!({
                "type": "function",
                "function": {
                    "name": "execute_code",
                    "description": "Run Python that calls Hermes tools programmatically. Use when you need 3+ tool calls with logic between them: filtering/reducing large outputs before they enter context, branching, or loops (N pages/files, retry on failure). Use normal tool calls for single calls, results you must reason over in full, or anything needing user interaction.\n\nCalls run in a persistent session kernel: variables, imports, and loaded data survive across execute_code calls, so build on earlier work instead of re-loading it. A timed-out or interrupted call loses that state.\n\nAvailable via `from hermes_tools import ...`:\n\n  web_search(query: str, limit: int = 5) -> dict\n    Returns {\"data\": {\"web\": [{\"url\", \"title\", \"description\"}, ...]}}\n  web_extract(urls: list[str], char_limit: int = None) -> dict\n    Returns {\"results\": [{\"url\", \"title\", \"content\", \"error\"}, ...]} where content is markdown.\n    No LLM summarization. Pages over char_limit (default 15000) are head+tail truncated; full text stored on disk (path in the content footer).\n  read_file(path: str, offset: int = 1, limit: int = 2000) -> dict\n    Lines are 1-indexed. Returns {\"content\": \"...\", \"total_lines\": N}\n  write_file(path: str, content: str) -> dict\n    Always overwrites the entire file.\n  search_files(pattern: str, target=\"content\", path=\".\", file_glob=None, limit=50, order=\"discovery\") -> dict\n    target: \"content\" (search inside files) or \"files\" (find files by name). Returns {\"matches\": [...]}\n  patch(path: str, old_string: str, new_string: str, replace_all: bool = False) -> dict\n    Replaces old_string with new_string in the file.\n  terminal(command: str, timeout=None, workdir=None) -> dict\n    Foreground only (no background/pty). Returns {\"output\": \"...\", \"exit_code\": N}\n\nLimits: 5-minute timeout, max 50 tool calls per call. Stdout over 50KB shows head/tail inline; the FULL text is auto-saved to a file whose path rides in the result.\n\nScripts run in the session's working directory. Interpreter: the project's activated venv/conda python when one is active (VIRTUAL_ENV/CONDA_PREFIX — matches terminal()); otherwise Hermes's own python (the common case — stdlib plus Hermes's deps; check `import x` before relying on project packages).\n\nHelpers require imports: `from hermes_tools import json_parse, shell_quote, retry`. json_parse(text) — tolerant json.loads for terminal() output; shell_quote(s) — shlex.quote for dynamic shell args; retry(fn, max_attempts=3, delay=2) — exponential backoff.",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "code": {
                                "type": "string",
                                "description": "Python code to execute. Import tools with `from hermes_tools import web_search, terminal, ...` and print your final result to stdout."
                            },
                            "reset": {
                                "type": "boolean",
                                "description": "Discard the kernel's persistent state and start fresh before running this code."
                            }
                        },
                        "required": ["code"]
                    }
                }
            }),
            crate::memory::get_definition(),
            crate::session_search::SessionSearchEngine::get_definition(),
            crate::skill_manager::get_definition(&format!(
                "{}/skills/",
                crate::skill_manager::default_skills_dir().display()
            )),
        ]
    }

    pub async fn execute_core(&self, call: &ToolCall) -> ToolResult {
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
            "browser_navigate" => self.execute_browser_navigate(call).await,
            "browser_snapshot" => self.execute_browser_snapshot(call).await,
            "browser_click" => self.execute_browser_click(call).await,
            "browser_type" => self.execute_browser_type(call).await,
            "browser_take_screenshot" => self.execute_browser_take_screenshot(call).await,
            "browser_exec" => self.execute_browser_exec(call).await,
            "todo_list" => self.execute_todo_list(call).await,
            "cronjob_manage" => self.execute_cronjob_manage(call).await,
            "execute_code" => self.execute_code(call).await,
            "session_search" => self.execute_session_search(call).await,
            "memory" => self.execute_memory(call).await,
            "skill_manage" => self.execute_skill_manage(call).await,
            other => ToolResult {
                tool_call_id: call.id.clone(),
                output: format!("Unknown core tool: {}", other),
                success: false,
            },
        }
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
            "browser_navigate" => self.execute_browser_navigate(call).await,
            "browser_snapshot" => self.execute_browser_snapshot(call).await,
            "browser_click" => self.execute_browser_click(call).await,
            "browser_type" => self.execute_browser_type(call).await,
            "browser_take_screenshot" => self.execute_browser_take_screenshot(call).await,
            "browser_exec" => self.execute_browser_exec(call).await,
            "todo_list" => self.execute_todo_list(call).await,
            "cronjob_manage" => self.execute_cronjob_manage(call).await,
            "execute_code" => self.execute_code(call).await,
            "delegate_task" => self.execute_delegate_task(call).await,
            "session_search" => self.execute_session_search(call).await,
            "memory" => self.execute_memory(call).await,
            "skill_manage" => self.execute_skill_manage(call).await,
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

    pub async fn execute_terminal(&self, call: &ToolCall) -> ToolResult {
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

    pub async fn execute_web_search(&self, call: &ToolCall) -> ToolResult {
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

    pub async fn execute_web_extract(&self, call: &ToolCall) -> ToolResult {
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


    pub async fn execute_skills_list(&self, call: &ToolCall) -> ToolResult {
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

    pub async fn execute_skill_view(&self, call: &ToolCall) -> ToolResult {
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

    async fn execute_browser_navigate(&self, call: &ToolCall) -> ToolResult {
        let url = match call.arguments.get("url").and_then(|v| v.as_str()) {
            Some(u) => u,
            None => {
                return ToolResult {
                    tool_call_id: call.id.clone(),
                    output: serde_json::json!({
                        "success": false,
                        "error": "Missing required 'url' parameter for browser_navigate"
                    }).to_string(),
                    success: false,
                };
            }
        };

        match crate::browser::browser_navigate(url).await {
            Ok(res) => ToolResult {
                tool_call_id: call.id.clone(),
                output: serde_json::to_string_pretty(&res).unwrap_or_else(|_| "{}".to_string()),
                success: res.success,
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

    async fn execute_browser_snapshot(&self, call: &ToolCall) -> ToolResult {
        match crate::browser::browser_snapshot().await {
            Ok(res) => ToolResult {
                tool_call_id: call.id.clone(),
                output: serde_json::to_string_pretty(&res).unwrap_or_else(|_| "{}".to_string()),
                success: res.success,
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

    async fn execute_browser_click(&self, call: &ToolCall) -> ToolResult {
        let selector = call.arguments.get("selector").and_then(|v| v.as_str());
        let x = call.arguments.get("x").and_then(|v| v.as_f64());
        let y = call.arguments.get("y").and_then(|v| v.as_f64());

        match crate::browser::browser_click(selector, x, y).await {
            Ok(res) => ToolResult {
                tool_call_id: call.id.clone(),
                output: serde_json::to_string_pretty(&res).unwrap_or_else(|_| "{}".to_string()),
                success: res.success,
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

    async fn execute_browser_type(&self, call: &ToolCall) -> ToolResult {
        let selector = match call.arguments.get("selector").and_then(|v| v.as_str()) {
            Some(s) => s,
            None => {
                return ToolResult {
                    tool_call_id: call.id.clone(),
                    output: serde_json::json!({
                        "success": false,
                        "error": "Missing required 'selector' parameter for browser_type"
                    }).to_string(),
                    success: false,
                };
            }
        };

        let text = match call.arguments.get("text").and_then(|v| v.as_str()) {
            Some(t) => t,
            None => {
                return ToolResult {
                    tool_call_id: call.id.clone(),
                    output: serde_json::json!({
                        "success": false,
                        "error": "Missing required 'text' parameter for browser_type"
                    }).to_string(),
                    success: false,
                };
            }
        };

        match crate::browser::browser_type(selector, text).await {
            Ok(res) => ToolResult {
                tool_call_id: call.id.clone(),
                output: serde_json::to_string_pretty(&res).unwrap_or_else(|_| "{}".to_string()),
                success: res.success,
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

    async fn execute_browser_take_screenshot(&self, call: &ToolCall) -> ToolResult {
        let format = call.arguments.get("format").and_then(|v| v.as_str());
        let quality = call.arguments.get("quality").and_then(|v| v.as_u64()).map(|q| q as u32);

        match crate::browser::browser_take_screenshot(format, quality).await {
            Ok(res) => ToolResult {
                tool_call_id: call.id.clone(),
                output: serde_json::to_string_pretty(&res).unwrap_or_else(|_| "{}".to_string()),
                success: res.success,
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

    async fn execute_browser_exec(&self, call: &ToolCall) -> ToolResult {
        let code = match call.arguments.get("code").and_then(|v| v.as_str()) {
            Some(c) => c,
            None => {
                return ToolResult {
                    tool_call_id: call.id.clone(),
                    output: serde_json::json!({
                        "success": false,
                        "error": "Missing required 'code' parameter for browser_exec"
                    }).to_string(),
                    success: false,
                };
            }
        };

        match crate::browser::browser_exec(code).await {
            Ok(res) => ToolResult {
                tool_call_id: call.id.clone(),
                output: serde_json::to_string_pretty(&res).unwrap_or_else(|_| "{}".to_string()),
                success: res.success,
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

    pub async fn execute_todo_list(&self, call: &ToolCall) -> ToolResult {
        let todos_arg = call.arguments.get("todos");
        let merge = call
            .arguments
            .get("merge")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        match crate::todo::todo_tool(todos_arg, merge, &self.todo_store).await {
            Ok(val) => ToolResult {
                tool_call_id: call.id.clone(),
                output: serde_json::to_string_pretty(&val).unwrap_or_else(|_| val.to_string()),
                success: true,
            },
            Err(e) => ToolResult {
                tool_call_id: call.id.clone(),
                output: serde_json::json!({
                    "error": e
                })
                .to_string(),
                success: false,
            },
        }
    }

    async fn execute_code(&self, call: &ToolCall) -> ToolResult {
        let code = match call.arguments.get("code").and_then(|v| v.as_str()) {
            Some(c) => c,
            None => {
                return ToolResult {
                    tool_call_id: call.id.clone(),
                    output: serde_json::json!({
                        "success": false,
                        "error": "No code provided. execute_code requires a non-empty code parameter containing Python source."
                    }).to_string(),
                    success: false,
                };
            }
        };

        let reset = call.arguments.get("reset").and_then(|v| v.as_bool()).unwrap_or(false);
        let session_id = call.arguments.get("session_id")
            .and_then(|v| v.as_str())
            .unwrap_or("default");

        // ToolRegistry cannot easily pass Arc<Self> without Arc::new or with_self,
        // so we instantiate or pass Arc of self clone if needed, or pass Arc::new(ToolRegistry::new())
        let tool_reg = Arc::new(Self {
            skill_registry: self.skill_registry.clone(),
            code_engine: self.code_engine.clone(),
            todo_store: self.todo_store.clone(),
            cron_scheduler: self.cron_scheduler.clone(),
            delegation_manager: self.delegation_manager.clone(),
            session_search: self.session_search.clone(),
            memory_store: self.memory_store.clone(),
            skill_manager: self.skill_manager.clone(),
        });

        match self.code_engine.execute(session_id, code, reset, None, Some(tool_reg)).await {
            Ok(res_val) => {
                let success = res_val.get("status").and_then(|v| v.as_str()) == Some("success");
                ToolResult {
                    tool_call_id: call.id.clone(),
                    output: serde_json::to_string_pretty(&res_val).unwrap_or_else(|_| "{}".to_string()),
                    success,
                }
            }
            Err(e) => ToolResult {
                tool_call_id: call.id.clone(),
                output: serde_json::json!({
                    "status": "error",
                    "error": e
                }).to_string(),
                success: false,
            },
        }
    }

    pub async fn execute_cronjob_manage(&self, call: &ToolCall) -> ToolResult {
        let action = match call.arguments.get("action").and_then(|v| v.as_str()) {
            Some(a) => a.trim().to_lowercase(),
            None => {
                return ToolResult {
                    tool_call_id: call.id.clone(),
                    output: serde_json::json!({
                        "success": false,
                        "error": "Missing required field 'action'"
                    })
                    .to_string(),
                    success: false,
                };
            }
        };

        let scheduler = match &self.cron_scheduler {
            Some(s) => s.clone(),
            None => {
                return ToolResult {
                    tool_call_id: call.id.clone(),
                    output: serde_json::json!({
                        "success": false,
                        "error": "CronScheduler is not initialized or configured"
                    })
                    .to_string(),
                    success: false,
                };
            }
        };

        match action.as_str() {
            "create" => {
                let schedule = match call.arguments.get("schedule").and_then(|v| v.as_str()) {
                    Some(s) if !s.trim().is_empty() => s.to_string(),
                    _ => {
                        return ToolResult {
                            tool_call_id: call.id.clone(),
                            output: serde_json::json!({
                                "success": false,
                                "error": "schedule is required for create"
                            })
                            .to_string(),
                            success: false,
                        };
                    }
                };

                let prompt = call.arguments.get("prompt").and_then(|v| v.as_str()).map(|s| s.to_string());
                let skills = call.arguments.get("skills").and_then(|v| {
                    v.as_array().map(|arr| {
                        arr.iter().filter_map(|s| s.as_str().map(|str_val| str_val.to_string())).collect()
                    })
                });

                let no_agent = call.arguments.get("no_agent").and_then(|v| v.as_bool()).unwrap_or(false);
                let script = call.arguments.get("script").and_then(|v| v.as_str()).map(|s| s.to_string());

                if no_agent {
                    if script.is_none() {
                        return ToolResult {
                            tool_call_id: call.id.clone(),
                            output: serde_json::json!({
                                "success": false,
                                "error": "create with no_agent=True requires a script — the script is the job."
                            })
                            .to_string(),
                            success: false,
                        };
                    }
                } else if prompt.is_none() && skills.as_ref().map(|s: &Vec<String>| s.is_empty()).unwrap_or(true) {
                    return ToolResult {
                        tool_call_id: call.id.clone(),
                        output: serde_json::json!({
                            "success": false,
                            "error": "create requires either prompt or at least one skill"
                        })
                        .to_string(),
                        success: false,
                    };
                }

                let params = hermes_cron::CreateJobParams {
                    schedule,
                    prompt,
                    name: call.arguments.get("name").and_then(|v| v.as_str()).map(|s| s.to_string()),
                    session_id: call.arguments.get("session_id").and_then(|v| v.as_str()).map(|s| s.to_string()),
                    deliver: call.arguments.get("deliver").and_then(|v| v.as_str()).map(|s| s.to_string()),
                    failure_deliver: call.arguments.get("failure_deliver").and_then(|v| v.as_str()).map(|s| s.to_string()),
                    skills,
                    paused: call.arguments.get("paused").and_then(|v| v.as_bool()),
                    paused_reason: call.arguments.get("paused_reason").and_then(|v| v.as_str()).map(|s| s.to_string()),
                    pinned: call.arguments.get("pinned").and_then(|v| v.as_bool()),
                    model: call.arguments.get("model").and_then(|v| v.as_str()).map(|s| s.to_string()),
                    provider: call.arguments.get("provider").and_then(|v| v.as_str()).map(|s| s.to_string()),
                    base_url: call.arguments.get("base_url").and_then(|v| v.as_str()).map(|s| s.to_string()),
                    script,
                    monitor: call.arguments.get("monitor").and_then(|v| v.as_str()).map(|s| s.to_string()),
                    no_agent: Some(no_agent),
                    repeat: call.arguments.get("repeat").and_then(|v| {
                        if let Some(i) = v.as_i64() {
                            Some(i.to_string())
                        } else {
                            v.as_str().map(|s| s.to_string())
                        }
                    }),
                };

                match scheduler.create_job(params).await {
                    Ok(job) => {
                        let prompt_preview = if job.prompt.len() > 100 {
                            format!("{}...", &job.prompt[..100])
                        } else {
                            job.prompt.clone()
                        };

                        let job_payload = serde_json::json!({
                            "job_id": job.id,
                            "name": job.name,
                            "skill": job.skill,
                            "skills": job.skills,
                            "prompt_preview": prompt_preview,
                            "model": job.model,
                            "provider": job.provider,
                            "pinned": job.pinned,
                            "base_url": job.base_url,
                            "schedule": job.schedule_display,
                            "repeat": job.repeat,
                            "deliver": job.deliver,
                            "next_run_at": job.next_run_at,
                            "last_run_at": job.last_run_at,
                            "last_status": job.last_status,
                            "last_delivery_error": null,
                            "last_delivery_unverified": null,
                            "last_fire_error": null,
                            "last_error": job.last_error,
                            "enabled": job.enabled,
                            "state": job.state,
                            "paused_at": job.paused_at,
                            "paused_reason": job.paused_reason,
                        });

                        let res = serde_json::json!({
                            "success": true,
                            "job_id": job.id,
                            "name": job.name,
                            "skill": job.skill,
                            "skills": job.skills,
                            "schedule": job.schedule_display,
                            "repeat": job.repeat,
                            "deliver": job.deliver,
                            "next_run_at": job.next_run_at,
                            "job": job_payload,
                            "message": format!("Cron job '{}' created.", job.name),
                            "gateway_running": true
                        });
                        ToolResult {
                            tool_call_id: call.id.clone(),
                            output: serde_json::to_string_pretty(&res).unwrap_or_else(|_| res.to_string()),
                            success: true,
                        }
                    }
                    Err(e) => ToolResult {
                        tool_call_id: call.id.clone(),
                        output: serde_json::json!({
                            "success": false,
                            "error": e.to_string()
                        })
                        .to_string(),
                        success: false,
                    },
                }
            }
            "list" => {
                let jobs = scheduler.list_jobs().await;
                let formatted_jobs: Vec<serde_json::Value> = jobs.iter().map(|job| {
                    let prompt_preview = if job.prompt.len() > 100 {
                        format!("{}...", &job.prompt[..100])
                    } else {
                        job.prompt.clone()
                    };
                    serde_json::json!({
                        "job_id": job.id,
                        "name": job.name,
                        "skill": job.skill,
                        "skills": job.skills,
                        "prompt_preview": prompt_preview,
                        "model": job.model,
                        "provider": job.provider,
                        "pinned": job.pinned,
                        "base_url": job.base_url,
                        "schedule": job.schedule_display,
                        "repeat": job.repeat,
                        "deliver": job.deliver,
                        "next_run_at": job.next_run_at,
                        "last_run_at": job.last_run_at,
                        "last_status": job.last_status,
                        "last_delivery_error": null,
                        "last_delivery_unverified": null,
                        "last_fire_error": null,
                        "last_error": job.last_error,
                        "enabled": job.enabled,
                        "state": job.state,
                        "paused_at": job.paused_at,
                        "paused_reason": job.paused_reason,
                    })
                }).collect();

                let res = serde_json::json!({
                    "success": true,
                    "count": formatted_jobs.len(),
                    "jobs": formatted_jobs,
                    "gateway_running": true
                });

                ToolResult {
                    tool_call_id: call.id.clone(),
                    output: serde_json::to_string_pretty(&res).unwrap_or_else(|_| res.to_string()),
                    success: true,
                }
            }
            "update" => {
                let job_id = match call.arguments.get("job_id").and_then(|v| v.as_str()) {
                    Some(id) if !id.trim().is_empty() => id.trim().to_string(),
                    _ => {
                        return ToolResult {
                            tool_call_id: call.id.clone(),
                            output: serde_json::json!({
                                "success": false,
                                "error": "job_id is required for action 'update'"
                            })
                            .to_string(),
                            success: false,
                        };
                    }
                };

                let params = hermes_cron::UpdateJobParams {
                    job_id: job_id.clone(),
                    prompt: call.arguments.get("prompt").and_then(|v| v.as_str()).map(|s| s.to_string()),
                    name: call.arguments.get("name").and_then(|v| v.as_str()).map(|s| s.to_string()),
                    schedule: call.arguments.get("schedule").and_then(|v| v.as_str()).map(|s| s.to_string()),
                    deliver: call.arguments.get("deliver").and_then(|v| v.as_str()).map(|s| s.to_string()),
                    failure_deliver: call.arguments.get("failure_deliver").and_then(|v| v.as_str()).map(|s| s.to_string()),
                    skills: call.arguments.get("skills").and_then(|v| {
                        v.as_array().map(|arr| {
                            arr.iter().filter_map(|s| s.as_str().map(|str_val| str_val.to_string())).collect()
                        })
                    }),
                    paused: call.arguments.get("paused").and_then(|v| v.as_bool()),
                    pinned: call.arguments.get("pinned").and_then(|v| v.as_bool()),
                    model: call.arguments.get("model").and_then(|v| v.as_str()).map(|s| s.to_string()),
                    provider: call.arguments.get("provider").and_then(|v| v.as_str()).map(|s| s.to_string()),
                    base_url: call.arguments.get("base_url").and_then(|v| v.as_str()).map(|s| s.to_string()),
                    script: call.arguments.get("script").and_then(|v| v.as_str()).map(|s| s.to_string()),
                    monitor: call.arguments.get("monitor").and_then(|v| v.as_str()).map(|s| s.to_string()),
                    no_agent: call.arguments.get("no_agent").and_then(|v| v.as_bool()),
                    repeat: call.arguments.get("repeat").and_then(|v| {
                        if let Some(i) = v.as_i64() {
                            Some(i.to_string())
                        } else {
                            v.as_str().map(|s| s.to_string())
                        }
                    }),
                };

                match scheduler.update_job(params).await {
                    Ok(job) => {
                        let prompt_preview = if job.prompt.len() > 100 {
                            format!("{}...", &job.prompt[..100])
                        } else {
                            job.prompt.clone()
                        };
                        let job_payload = serde_json::json!({
                            "job_id": job.id,
                            "name": job.name,
                            "skill": job.skill,
                            "skills": job.skills,
                            "prompt_preview": prompt_preview,
                            "model": job.model,
                            "provider": job.provider,
                            "pinned": job.pinned,
                            "base_url": job.base_url,
                            "schedule": job.schedule_display,
                            "repeat": job.repeat,
                            "deliver": job.deliver,
                            "next_run_at": job.next_run_at,
                            "last_run_at": job.last_run_at,
                            "last_status": job.last_status,
                            "last_delivery_error": null,
                            "last_delivery_unverified": null,
                            "last_fire_error": null,
                            "last_error": job.last_error,
                            "enabled": job.enabled,
                            "state": job.state,
                            "paused_at": job.paused_at,
                            "paused_reason": job.paused_reason,
                        });
                        let res = serde_json::json!({
                            "success": true,
                            "job": job_payload
                        });
                        ToolResult {
                            tool_call_id: call.id.clone(),
                            output: serde_json::to_string_pretty(&res).unwrap_or_else(|_| res.to_string()),
                            success: true,
                        }
                    }
                    Err(e) => ToolResult {
                        tool_call_id: call.id.clone(),
                        output: serde_json::json!({
                            "success": false,
                            "error": e.to_string()
                        })
                        .to_string(),
                        success: false,
                    },
                }
            }
            "pause" => {
                let job_id = match call.arguments.get("job_id").and_then(|v| v.as_str()) {
                    Some(id) if !id.trim().is_empty() => id.trim(),
                    _ => {
                        return ToolResult {
                            tool_call_id: call.id.clone(),
                            output: serde_json::json!({
                                "success": false,
                                "error": "job_id is required for action 'pause'"
                            })
                            .to_string(),
                            success: false,
                        };
                    }
                };

                let reason = call.arguments.get("reason").or_else(|| call.arguments.get("paused_reason")).and_then(|v| v.as_str()).map(|s| s.to_string());

                match scheduler.pause_job(job_id, reason).await {
                    Ok(job) => {
                        let prompt_preview = if job.prompt.len() > 100 {
                            format!("{}...", &job.prompt[..100])
                        } else {
                            job.prompt.clone()
                        };
                        let job_payload = serde_json::json!({
                            "job_id": job.id,
                            "name": job.name,
                            "skill": job.skill,
                            "skills": job.skills,
                            "prompt_preview": prompt_preview,
                            "model": job.model,
                            "provider": job.provider,
                            "pinned": job.pinned,
                            "base_url": job.base_url,
                            "schedule": job.schedule_display,
                            "repeat": job.repeat,
                            "deliver": job.deliver,
                            "next_run_at": job.next_run_at,
                            "last_run_at": job.last_run_at,
                            "last_status": job.last_status,
                            "last_delivery_error": null,
                            "last_delivery_unverified": null,
                            "last_fire_error": null,
                            "last_error": job.last_error,
                            "enabled": job.enabled,
                            "state": job.state,
                            "paused_at": job.paused_at,
                            "paused_reason": job.paused_reason,
                        });
                        let res = serde_json::json!({
                            "success": true,
                            "job": job_payload
                        });
                        ToolResult {
                            tool_call_id: call.id.clone(),
                            output: serde_json::to_string_pretty(&res).unwrap_or_else(|_| res.to_string()),
                            success: true,
                        }
                    }
                    Err(e) => ToolResult {
                        tool_call_id: call.id.clone(),
                        output: serde_json::json!({
                            "success": false,
                            "error": e.to_string()
                        })
                        .to_string(),
                        success: false,
                    },
                }
            }
            "resume" => {
                let job_id = match call.arguments.get("job_id").and_then(|v| v.as_str()) {
                    Some(id) if !id.trim().is_empty() => id.trim(),
                    _ => {
                        return ToolResult {
                            tool_call_id: call.id.clone(),
                            output: serde_json::json!({
                                "success": false,
                                "error": "job_id is required for action 'resume'"
                            })
                            .to_string(),
                            success: false,
                        };
                    }
                };

                match scheduler.resume_job(job_id).await {
                    Ok(job) => {
                        let prompt_preview = if job.prompt.len() > 100 {
                            format!("{}...", &job.prompt[..100])
                        } else {
                            job.prompt.clone()
                        };
                        let job_payload = serde_json::json!({
                            "job_id": job.id,
                            "name": job.name,
                            "skill": job.skill,
                            "skills": job.skills,
                            "prompt_preview": prompt_preview,
                            "model": job.model,
                            "provider": job.provider,
                            "pinned": job.pinned,
                            "base_url": job.base_url,
                            "schedule": job.schedule_display,
                            "repeat": job.repeat,
                            "deliver": job.deliver,
                            "next_run_at": job.next_run_at,
                            "last_run_at": job.last_run_at,
                            "last_status": job.last_status,
                            "last_delivery_error": null,
                            "last_delivery_unverified": null,
                            "last_fire_error": null,
                            "last_error": job.last_error,
                            "enabled": job.enabled,
                            "state": job.state,
                            "paused_at": job.paused_at,
                            "paused_reason": job.paused_reason,
                        });
                        let res = serde_json::json!({
                            "success": true,
                            "job": job_payload
                        });
                        ToolResult {
                            tool_call_id: call.id.clone(),
                            output: serde_json::to_string_pretty(&res).unwrap_or_else(|_| res.to_string()),
                            success: true,
                        }
                    }
                    Err(e) => ToolResult {
                        tool_call_id: call.id.clone(),
                        output: serde_json::json!({
                            "success": false,
                            "error": e.to_string()
                        })
                        .to_string(),
                        success: false,
                    },
                }
            }
            "remove" => {
                let job_id = match call.arguments.get("job_id").and_then(|v| v.as_str()) {
                    Some(id) if !id.trim().is_empty() => id.trim(),
                    _ => {
                        return ToolResult {
                            tool_call_id: call.id.clone(),
                            output: serde_json::json!({
                                "success": false,
                                "error": "job_id is required for action 'remove'"
                            })
                            .to_string(),
                            success: false,
                        };
                    }
                };

                match scheduler.remove_job(job_id).await {
                    Ok(job) => {
                        let res = serde_json::json!({
                            "success": true,
                            "message": format!("Cron job '{}' removed.", job.name),
                            "removed_job": {
                                "id": job.id,
                                "name": job.name,
                                "schedule": job.schedule_display
                            }
                        });
                        ToolResult {
                            tool_call_id: call.id.clone(),
                            output: serde_json::to_string_pretty(&res).unwrap_or_else(|_| res.to_string()),
                            success: true,
                        }
                    }
                    Err(e) => ToolResult {
                        tool_call_id: call.id.clone(),
                        output: serde_json::json!({
                            "success": false,
                            "error": e.to_string()
                        })
                        .to_string(),
                        success: false,
                    },
                }
            }
            "run" | "run_now" | "trigger" => {
                let job_id = match call.arguments.get("job_id").and_then(|v| v.as_str()) {
                    Some(id) if !id.trim().is_empty() => id.trim(),
                    _ => {
                        return ToolResult {
                            tool_call_id: call.id.clone(),
                            output: serde_json::json!({
                                "success": false,
                                "error": "job_id is required for action 'run'"
                            })
                            .to_string(),
                            success: false,
                        };
                    }
                };

                let extra_prompt = call.arguments.get("prompt").and_then(|v| v.as_str()).map(|s| s.to_string());

                match scheduler.run_job_now(job_id, extra_prompt).await {
                    Ok(job) => {
                        let prompt_preview = if job.prompt.len() > 100 {
                            format!("{}...", &job.prompt[..100])
                        } else {
                            job.prompt.clone()
                        };
                        let job_payload = serde_json::json!({
                            "job_id": job.id,
                            "name": job.name,
                            "skill": job.skill,
                            "skills": job.skills,
                            "prompt_preview": prompt_preview,
                            "model": job.model,
                            "provider": job.provider,
                            "pinned": job.pinned,
                            "base_url": job.base_url,
                            "schedule": job.schedule_display,
                            "repeat": job.repeat,
                            "deliver": job.deliver,
                            "next_run_at": job.next_run_at,
                            "last_run_at": job.last_run_at,
                            "last_status": job.last_status,
                            "last_delivery_error": null,
                            "last_delivery_unverified": null,
                            "last_fire_error": null,
                            "last_error": job.last_error,
                            "enabled": job.enabled,
                            "state": job.state,
                            "paused_at": job.paused_at,
                            "paused_reason": job.paused_reason,
                            "executed": true,
                            "execution_mode": "background",
                            "delegation_id": format!("deleg_{}", &job.id[..std::cmp::min(8, job.id.len())])
                        });

                        let res = serde_json::json!({
                            "success": true,
                            "job": job_payload,
                            "note": "The job is running in the background. You and the user can keep working; its outcome re-enters the conversation as a new message when it finishes. Do not wait or poll — just continue."
                        });

                        ToolResult {
                            tool_call_id: call.id.clone(),
                            output: serde_json::to_string_pretty(&res).unwrap_or_else(|_| res.to_string()),
                            success: true,
                        }
                    }
                    Err(e) => ToolResult {
                        tool_call_id: call.id.clone(),
                        output: serde_json::json!({
                            "success": false,
                            "error": e.to_string()
                        })
                        .to_string(),
                        success: false,
                    },
                }
            }
            unknown => ToolResult {
                tool_call_id: call.id.clone(),
                output: serde_json::json!({
                    "success": false,
                    "error": format!("Unknown cron action '{}'", unknown)
                })
                .to_string(),
                success: false,
            },
        }
    }

    pub async fn execute_delegate_task(&self, call: &ToolCall) -> ToolResult {
        let manager = match &self.delegation_manager {
            Some(m) => m.clone(),
            None => {
                return ToolResult {
                    tool_call_id: call.id.clone(),
                    output: serde_json::json!({
                        "success": false,
                        "error": "DelegationManager is not initialized or configured on this registry."
                    }).to_string(),
                    success: false,
                };
            }
        };

        let action_str = call.arguments.get("action")
            .and_then(|v| v.as_str())
            .unwrap_or("spawn")
            .trim()
            .to_lowercase();

        match action_str.as_str() {
            "list" => {
                let res = manager.list_active().await;
                ToolResult {
                    tool_call_id: call.id.clone(),
                    output: serde_json::to_string_pretty(&res).unwrap_or_else(|_| res.to_string()),
                    success: true,
                }
            }
            "steer" => {
                let subagent_id = call.arguments.get("subagent_id").and_then(|v| v.as_str()).unwrap_or("");
                let message = call.arguments.get("message").and_then(|v| v.as_str()).unwrap_or("");
                let res = manager.steer(subagent_id, message).await;
                let success = res.get("status").and_then(|v| v.as_str()) == Some("queued");
                ToolResult {
                    tool_call_id: call.id.clone(),
                    output: serde_json::to_string_pretty(&res).unwrap_or_else(|_| res.to_string()),
                    success,
                }
            }
            "stop" => {
                let subagent_id = call.arguments.get("subagent_id").and_then(|v| v.as_str()).unwrap_or("");
                let res = manager.stop(subagent_id).await;
                let success = res.get("status").and_then(|v| v.as_str()) == Some("interrupt_requested");
                ToolResult {
                    tool_call_id: call.id.clone(),
                    output: serde_json::to_string_pretty(&res).unwrap_or_else(|_| res.to_string()),
                    success,
                }
            }
            "spawn" => {
                let mut tasks = Vec::new();

                // Accept either 'tasks' array or legacy single 'goal'/'context'
                if let Some(tasks_arr) = call.arguments.get("tasks").and_then(|v| v.as_array()) {
                    for t in tasks_arr {
                        if let Some(goal) = t.get("goal").and_then(|v| v.as_str()) {
                            let context = t.get("context").and_then(|v| v.as_str()).map(|s| s.to_string());
                            let output_schema = t.get("output_schema").cloned();
                            let images = t.get("images").and_then(|v| v.as_array()).map(|arr| {
                                arr.iter().filter_map(|s| s.as_str().map(|str_val| str_val.to_string())).collect()
                            });
                            let group = t.get("group").and_then(|v| v.as_str()).map(|s| s.to_string());
                            tasks.push(crate::delegation::TaskSpec {
                                goal: goal.to_string(),
                                context,
                                output_schema,
                                images,
                                group,
                            });
                        }
                    }
                } else if let Some(goal) = call.arguments.get("goal").and_then(|v| v.as_str()) {
                    let context = call.arguments.get("context").and_then(|v| v.as_str()).map(|s| s.to_string());
                    let output_schema = call.arguments.get("output_schema").cloned();
                    let images = call.arguments.get("images").and_then(|v| v.as_array()).map(|arr| {
                        arr.iter().filter_map(|s| s.as_str().map(|str_val| str_val.to_string())).collect()
                    });
                    tasks.push(crate::delegation::TaskSpec {
                        goal: goal.to_string(),
                        context,
                        output_schema,
                        images,
                        group: None,
                    });
                }

                if tasks.is_empty() {
                    return ToolResult {
                        tool_call_id: call.id.clone(),
                        output: serde_json::json!({
                            "success": false,
                            "error": "action='spawn' requires 'tasks' array or 'goal' argument."
                        }).to_string(),
                        success: false,
                    };
                }

                let parent_session_id = uuid::Uuid::new_v4();
                let llm_client = Arc::new(crate::llm::LlmClient::new(None, None));
                let self_registry = Arc::new(Self {
                    skill_registry: self.skill_registry.clone(),
                    code_engine: self.code_engine.clone(),
                    todo_store: self.todo_store.clone(),
                    cron_scheduler: self.cron_scheduler.clone(),
                    delegation_manager: self.delegation_manager.clone(),
                    session_search: self.session_search.clone(),
                    memory_store: self.memory_store.clone(),
                    skill_manager: self.skill_manager.clone(),
                });

                let res = manager.spawn_tasks(tasks, parent_session_id, llm_client, self_registry).await;
                ToolResult {
                    tool_call_id: call.id.clone(),
                    output: serde_json::to_string_pretty(&res).unwrap_or_else(|_| res.to_string()),
                    success: true,
                }
            }
            unknown => ToolResult {
                tool_call_id: call.id.clone(),
                output: serde_json::json!({
                    "success": false,
                    "error": format!("Unknown delegation action '{}'. Expected 'spawn', 'list', 'steer', or 'stop'.", unknown)
                }).to_string(),
                success: false,
            },
        }
    }


    pub async fn execute_session_search(&self, call: &ToolCall) -> ToolResult {
        if let Some(engine) = &self.session_search {
            match engine.execute(&call.arguments).await {
                Ok(output) => ToolResult {
                    tool_call_id: call.id.clone(),
                    output,
                    success: true,
                },
                Err(e) => ToolResult {
                    tool_call_id: call.id.clone(),
                    output: serde_json::json!({
                        "success": false,
                        "error": e
                    }).to_string(),
                    success: false,
                },
            }
        } else {
            ToolResult {
                tool_call_id: call.id.clone(),
                output: serde_json::json!({
                    "success": false,
                    "error": "SessionSearchEngine is not configured on this ToolRegistry"
                }).to_string(),
                success: false,
            }
        }
    }

    pub async fn execute_memory(&self, call: &ToolCall) -> ToolResult {
        use serde_json::Value;
        let args = &call.arguments;
        let get_str = |k: &str| -> Option<String> {
            args.get(k).and_then(|v| v.as_str()).map(|s| s.to_string())
        };
        let operations: Option<Vec<Value>> = args
            .get("operations")
            .and_then(|v| v.as_array())
            .cloned();

        let output = crate::memory::memory_tool(
            &self.memory_store,
            get_str("action").as_deref(),
            get_str("target").as_deref(),
            get_str("content").as_deref(),
            get_str("old_text").as_deref(),
            get_str("new_text").as_deref(),
            operations.as_ref(),
        );

        let success = serde_json::from_str::<Value>(&output)
            .ok()
            .and_then(|v| v.get("success").and_then(|s| s.as_bool()))
            .unwrap_or(false);

        ToolResult {
            tool_call_id: call.id.clone(),
            output,
            success,
        }
    }

    pub async fn execute_skill_manage(&self, call: &ToolCall) -> ToolResult {
        use serde_json::Value;
        let args = &call.arguments;
        let get_str = |k: &str| -> Option<String> {
            args.get(k).and_then(|v| v.as_str()).map(|s| s.to_string())
        };
        let operations: Option<Vec<Value>> = args
            .get("operations")
            .and_then(|v| v.as_array())
            .cloned();
        let replace_all = args
            .get("replace_all")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let output = self.skill_manager.skill_manage(
            get_str("action").as_deref(),
            get_str("name").as_deref().unwrap_or(""),
            get_str("content").as_deref(),
            get_str("category").as_deref(),
            get_str("file_path").as_deref(),
            get_str("file_content").as_deref(),
            get_str("old_string").as_deref(),
            get_str("new_string").as_deref(),
            replace_all,
            get_str("absorbed_into").as_deref(),
            operations.as_ref(),
            self.skill_manager.pinned(),
        );

        let success = serde_json::from_str::<Value>(&output)
            .ok()
            .and_then(|v| v.get("success").and_then(|s| s.as_bool()))
            .unwrap_or(false);

        ToolResult {
            tool_call_id: call.id.clone(),
            output,
            success,
        }
    }

}
