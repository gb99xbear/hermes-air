// Code Execution Engine - Programmatic Tool Calling (PTC)
// Manages Python execution sessions, hermes_tools RPC helper injection, limits, and output truncation/spill.


use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::process::Command;
use tokio::sync::Mutex;
use tracing::debug;

use crate::tools::{ToolCall, ToolRegistry};

pub const DEFAULT_TIMEOUT_SECS: u64 = 300;
pub const MAX_STDOUT_BYTES: usize = 50_000; // 50 KB
pub const MAX_SPILLED_STDOUT_BYTES: usize = 5_000_000; // 5 MB ceiling
pub const HEAD_RATIO: f64 = 0.4;
pub const SPILL_DIR_PATH: &str = "/tmp/hermes_exec_spill";

/// Truncation and spill metadata
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TruncationMetadata {
    pub stdout_truncated: bool,
    pub stdout_bytes_captured: usize,
    pub stdout_bytes_total: usize,
    pub stdout_bytes_omitted: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stdout_spill_path: Option<String>,
}

/// Head / tail byte budget split
pub fn head_tail_split(budget: usize) -> (usize, usize) {
    let head = (budget as f64 * HEAD_RATIO) as usize;
    (head, budget.saturating_sub(head))
}

/// Formats the truncation notice matching Python Hermes parity
pub fn truncation_notice(omitted: usize, total: usize, label: &str, unit: &str) -> String {
    format!("\n\n... [{} TRUNCATED - {} {} omitted out of {} total] ...\n\n", label, omitted, unit, total)
}

/// Computes SHA256 hex digest for byte spill filename
pub fn sha256_digest_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

/// Spills full stdout text to /tmp/hermes_exec_spill/stdout-<hash>.txt
pub async fn spill_full_stdout(stdout_text: &str) -> Option<String> {
    let mut text_to_spill = stdout_text.to_string();
    if text_to_spill.len() > MAX_SPILLED_STDOUT_BYTES {
        text_to_spill.truncate(MAX_SPILLED_STDOUT_BYTES);
        text_to_spill.push_str(&format!("\n\n[... spill capped at {} bytes ...]", MAX_SPILLED_STDOUT_BYTES));
    }

    let dir = Path::new(SPILL_DIR_PATH);
    if let Err(e) = tokio::fs::create_dir_all(dir).await {
        debug!("Failed to create spill dir {}: {}", SPILL_DIR_PATH, e);
        return None;
    }

    let digest_full = sha256_digest_hex(text_to_spill.as_bytes());
    let digest_12 = &digest_full[..12.min(digest_full.len())];
    let file_path = dir.join(format!("stdout-{}.txt", digest_12));

    if let Err(e) = tokio::fs::write(&file_path, text_to_spill.as_bytes()).await {
        debug!("Failed to write spill stdout: {}", e);
        return None;
    }

    Some(file_path.to_string_lossy().to_string())
}

/// Truncate stdout text (40% head / 60% tail) and produce metadata
pub async fn truncate_stdout_text(stdout_text: &str) -> (String, TruncationMetadata) {
    let raw_bytes = stdout_text.as_bytes();
    let total = raw_bytes.len();
    let captured = total.min(MAX_STDOUT_BYTES);

    let mut metadata = TruncationMetadata {
        stdout_truncated: total > captured,
        stdout_bytes_captured: captured,
        stdout_bytes_total: total,
        stdout_bytes_omitted: total.saturating_sub(captured),
        warning: None,
        stdout_spill_path: None,
    };

    if total <= MAX_STDOUT_BYTES {
        return (stdout_text.to_string(), metadata);
    }

    let (head_bytes, tail_bytes) = head_tail_split(MAX_STDOUT_BYTES);

    // Ensure valid UTF-8 boundary slices
    let safe_head = match std::str::from_utf8(&raw_bytes[..head_bytes]) {
        Ok(s) => s.to_string(),
        Err(_) => String::from_utf8_lossy(&raw_bytes[..head_bytes]).into_owned(),
    };

    let tail_start = total.saturating_sub(tail_bytes);
    let safe_tail = match std::str::from_utf8(&raw_bytes[tail_start..]) {
        Ok(s) => s.to_string(),
        Err(_) => String::from_utf8_lossy(&raw_bytes[tail_start..]).into_owned(),
    };

    let notice = truncation_notice(total.saturating_sub(captured), total, "OUTPUT", "bytes");
    let text = format!("{}{}{}", safe_head, notice, safe_tail);

    let spill_path = spill_full_stdout(stdout_text).await;
    if let Some(ref path) = spill_path {
        metadata.stdout_spill_path = Some(path.clone());
        metadata.warning = Some(format!(
            "execute_code stdout was truncated (head/tail shown); the script did run. FULL output saved to {} \u{2014} page it with read_file(path=\"{}\", offset=...) instead of re-running.",
            path, path
        ));
    } else {
        metadata.warning = Some(
            "execute_code stdout was truncated; the script did run, but only the captured head/tail output is included. Re-run only with narrower output if the omitted data is required.".to_string()
        );
    }

    (text, metadata)
}

/// Generates the hermes_tools.py stub module matching Python Hermes parity
pub fn generate_hermes_tools_module() -> String {
    r#"# Auto-generated hermes_tools RPC module
import json
import os
import socket
import shlex
import threading
import time

_sock = None
_call_lock = threading.Lock()

def json_parse(text: str):
    """Parse JSON tolerant of control characters and UTF-8 BOM."""
    if isinstance(text, str) and text.startswith("\ufeff"):
        text = text[1:]
    return json.loads(text, strict=False)

def shell_quote(s: str) -> str:
    """Shell-escape a string for safe interpolation into commands."""
    return shlex.quote(s)

def retry(fn, max_attempts=3, delay=2):
    """Retry a function up to max_attempts times with exponential backoff."""
    last_err = None
    for attempt in range(max_attempts):
        try:
            return fn()
        except Exception as e:
            last_err = e
            if attempt < max_attempts - 1:
                time.sleep(delay * (2 ** attempt))
    raise last_err

def _connect():
    global _sock
    if _sock is None:
        port = int(os.environ.get("HERMES_RPC_PORT", "0"))
        _sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        _sock.connect(("127.0.0.1", port))
        _sock.settimeout(300)
    return _sock

def _call(tool_name, args):
    req_obj = {
        "tool": tool_name,
        "args": args,
        "token": os.environ.get("HERMES_RPC_TOKEN", "")
    }
    request = json.dumps(req_obj) + "\n"
    with _call_lock:
        conn = _connect()
        conn.sendall(request.encode("utf-8"))
        buf = b""
        while True:
            chunk = conn.recv(65536)
            if not chunk:
                raise RuntimeError("Hermes agent process disconnected")
            buf += chunk
            if buf.endswith(b"\n"):
                break
    raw = buf.decode("utf-8").strip()
    result = json.loads(raw)
    if isinstance(result, str):
        try:
            return json.loads(result)
        except Exception:
            return result
    return result

def web_search(query: str, limit: int = 5):
    """Search the web. Returns dict with data.web list of {url, title, description}."""
    return _call("web_search", {"query": query, "limit": limit})

def web_extract(urls: list, char_limit: int = None):
    """Extract content from URLs. Returns dict with results list of {url, title, content, error}."""
    return _call("web_extract", {"urls": urls, "char_limit": char_limit})

def read_file(path: str, offset: int = 1, limit: int = 2000):
    """Read a file (1-indexed lines). Returns dict with 'content' and 'total_lines'."""
    return _call("read_file", {"path": path, "offset": offset, "limit": limit})

def write_file(path: str, content: str):
    """Write content to a file (always overwrites). Returns dict with status."""
    return _call("write_file", {"path": path, "content": content})

def search_files(pattern: str, target: str = "content", path: str = ".", file_glob: str = None, limit: int = 50, offset: int = 0, output_mode: str = "content", context: int = 0, order: str = "discovery"):
    """Search file contents or find files by name. Returns dict with matches."""
    return _call("search_files", {
        "pattern": pattern, "target": target, "path": path, "file_glob": file_glob,
        "limit": limit, "offset": offset, "output_mode": output_mode, "context": context, "order": order
    })

def patch(path: str = None, old_string: str = None, new_string: str = None, replace_all: bool = False):
    """Targeted find-and-replace in file. Returns dict with status."""
    return _call("patch", {
        "path": path, "old_string": old_string, "new_string": new_string, "replace_all": replace_all
    })

def terminal(command: str, timeout: int = None, workdir: str = None):
    """Run a shell command (foreground only). Returns dict with output and exit_code."""
    return _call("terminal", {"command": command, "timeout": timeout, "workdir": workdir})
"#.to_string()
}

/// Represents an active or cached session state for code execution
#[derive(Debug, Clone)]
pub struct SessionKernelState {
    pub session_id: String,
    pub session_dir: PathBuf,
    pub execution_count: usize,
    pub variables_state_file: PathBuf,
}

/// Global Code Execution Engine
pub struct CodeExecutionEngine {
    sessions: DashMap<String, Arc<Mutex<SessionKernelState>>>,
    base_dir: PathBuf,
}

impl Default for CodeExecutionEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl CodeExecutionEngine {
    pub fn new() -> Self {
        let base_dir = std::env::temp_dir().join("hermes_code_exec");
        let _ = std::fs::create_dir_all(&base_dir);
        Self {
            sessions: DashMap::new(),
            base_dir,
        }
    }

    pub fn with_base_dir(base_dir: PathBuf) -> Self {
        let _ = std::fs::create_dir_all(&base_dir);
        Self {
            sessions: DashMap::new(),
            base_dir,
        }
    }

    /// Reset an existing session kernel state
    pub async fn reset_session(&self, session_id: &str) {
        if let Some((_, state_lock)) = self.sessions.remove(session_id) {
            let state = state_lock.lock().await;
            if state.session_dir.exists() {
                let _ = tokio::fs::remove_dir_all(&state.session_dir).await;
            }
            debug!("Reset session kernel state for session_id: {}", session_id);
        }
    }

    /// Get or create session directory & kernel state
    async fn get_or_create_session(&self, session_id: &str) -> Arc<Mutex<SessionKernelState>> {
        if let Some(entry) = self.sessions.get(session_id) {
            return entry.value().clone();
        }

        let session_dir = self.base_dir.join(format!("session_{}", session_id));
        let _ = tokio::fs::create_dir_all(&session_dir).await;
        let variables_state_file = session_dir.join("kernel_vars.pickle");

        let state = SessionKernelState {
            session_id: session_id.to_string(),
            session_dir,
            execution_count: 0,
            variables_state_file,
        };

        let arc_state = Arc::new(Mutex::new(state));
        self.sessions.insert(session_id.to_string(), arc_state.clone());
        arc_state
    }

    /// Executes code within the engine
    pub async fn execute(
        &self,
        session_id: &str,
        code: &str,
        reset: bool,
        timeout_secs: Option<u64>,
        tool_registry: Option<Arc<ToolRegistry>>,
    ) -> Result<Value, String> {
        let start_time = Instant::now();

        if reset {
            self.reset_session(session_id).await;
        }

        if code.trim().is_empty() {
            return Err("No code provided. execute_code requires a non-empty 'code' parameter containing Python source.".to_string());
        }

        let session_arc = self.get_or_create_session(session_id).await;
        let (session_dir, execution_count, variables_state_file) = {
            let mut session = session_arc.lock().await;
            session.execution_count += 1;
            (session.session_dir.clone(), session.execution_count, session.variables_state_file.clone())
        };

        // Ensure session directory exists
        tokio::fs::create_dir_all(&session_dir)
            .await
            .map_err(|e| format!("Failed to create session dir: {}", e))?;

        // Stage hermes_tools.py in session dir
        let hermes_tools_py = session_dir.join("hermes_tools.py");
        tokio::fs::write(&hermes_tools_py, generate_hermes_tools_module().as_bytes())
            .await
            .map_err(|e| format!("Failed to write hermes_tools.py: {}", e))?;

        // Set up RPC server if ToolRegistry is provided
        let (rpc_port, rpc_token, stop_tx) = if let Some(ref reg) = tool_registry {
            let listener = TcpListener::bind("127.0.0.1:0")
                .await
                .map_err(|e| format!("Failed to bind RPC TCP listener: {}", e))?;
            let port = listener
                .local_addr()
                .map_err(|e| format!("Failed to get local port: {}", e))?
                .port();
            let token = uuid::Uuid::new_v4().to_string();

            let (stop_sender, mut stop_receiver) = tokio::sync::oneshot::channel::<()>();
            let reg_clone = reg.clone();
            let token_clone = token.clone();

            tokio::spawn(async move {
                loop {
                    tokio::select! {
                        _ = &mut stop_receiver => {
                            break;
                        }
                        accept_res = listener.accept() => {
                            match accept_res {
                                Ok((mut socket, _)) => {
                                    let reg_inner = reg_clone.clone();
                                    let expected_token = token_clone.clone();
                                    tokio::spawn(async move {
                                        let (reader, mut writer) = socket.split();
                                        let mut buf_reader = BufReader::new(reader);
                                        let mut line = String::new();
                                        while let Ok(bytes) = buf_reader.read_line(&mut line).await {
                                            if bytes == 0 {
                                                break;
                                            }
                                            let trimmed = line.trim();
                                            if !trimmed.is_empty() {
                                                let response = Self::handle_rpc_request(trimmed, &reg_inner, &expected_token).await;
                                                let resp_str = serde_json::to_string(&response).unwrap_or_else(|_| "{}".to_string()) + "\n";
                                                let _ = writer.write_all(resp_str.as_bytes()).await;
                                            }
                                            line.clear();
                                        }
                                    });
                                }
                                Err(_) => break,
                            }
                        }
                    }
                }
            });

            (Some(port), Some(token), Some(stop_sender))
        } else {
            (None, None, None)
        };

        // Construct runner script to exec code and preserve state across executions
        let state_file_str = variables_state_file.to_string_lossy().to_string();
        let runner_script = format!(
            r#"# Persistent runner wrapper
import os, sys, pickle

# Configure sys.path so hermes_tools is importable
current_dir = os.path.abspath(os.path.dirname(__file__))
if current_dir not in sys.path:
    sys.path.insert(0, current_dir)

state_file = {state_file:?}
g = {{'__name__': '__main__', '__builtins__': __builtins__}}

if os.path.exists(state_file):
    try:
        with open(state_file, 'rb') as f:
            saved_vars = pickle.load(f)
            g.update(saved_vars)
    except Exception:
        pass

exec_code = compile({user_code:?}, '<cell>', 'exec')
exec(exec_code, g)

# Save serializable globals for session persistence
to_save = {{}}
for k, v in g.items():
    if k.startswith('__'):
        continue
    # Exclude non-pickleable types gracefully
    try:
        pickle.dumps(v)
        to_save[k] = v
    except Exception:
        pass

try:
    with open(state_file, 'wb') as f:
        pickle.dump(to_save, f)
except Exception:
    pass
"#,
            state_file = state_file_str,
            user_code = code,
        );

        let runner_file = session_dir.join(format!("run_{}.py", execution_count));
        tokio::fs::write(&runner_file, runner_script.as_bytes())
            .await
            .map_err(|e| format!("Failed to write runner script: {}", e))?;

        let timeout_duration = Duration::from_secs(timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS));

        let mut cmd = Command::new("python3");
        cmd.arg(&runner_file);
        cmd.current_dir(std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

        // Pass environment
        if let Some(port) = rpc_port {
            cmd.env("HERMES_RPC_PORT", port.to_string());
        }
        if let Some(tok) = rpc_token {
            cmd.env("HERMES_RPC_TOKEN", tok);
        }
        cmd.env("PYTHONUNBUFFERED", "1");
        cmd.env("PYTHONDONTWRITEBYTECODE", "1");

        let child = cmd
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| format!("Failed to spawn python3: {}", e))?;

        let exec_result = tokio::time::timeout(timeout_duration, child.wait_with_output()).await;

        if let Some(tx) = stop_tx {
            let _ = tx.send(());
        }

        let duration_secs = start_time.elapsed().as_secs_f64();

        match exec_result {
            Ok(Ok(output)) => {
                let raw_stdout = String::from_utf8_lossy(&output.stdout).to_string();
                let raw_stderr = String::from_utf8_lossy(&output.stderr).to_string();

                let mut combined_output = raw_stdout.clone();
                if !raw_stderr.trim().is_empty() {
                    if !combined_output.is_empty() {
                        combined_output.push_str("\n--- stderr ---\n");
                    }
                    combined_output.push_str(&raw_stderr);
                }

                let (truncated_out, meta) = truncate_stdout_text(&combined_output).await;

                let status = if output.status.success() {
                    "success"
                } else {
                    "error"
                };

                let mut res_obj = json!({
                    "status": status,
                    "output": truncated_out,
                    "exit_code": output.status.code().unwrap_or(-1),
                    "duration_seconds": (duration_secs * 100.0).round() / 100.0,
                    "stdout_truncated": meta.stdout_truncated,
                    "stdout_bytes_captured": meta.stdout_bytes_captured,
                    "stdout_bytes_total": meta.stdout_bytes_total,
                    "stdout_bytes_omitted": meta.stdout_bytes_omitted,
                });

                if let Some(ref w) = meta.warning {
                    res_obj["warning"] = json!(w);
                }
                if let Some(ref p) = meta.stdout_spill_path {
                    res_obj["stdout_spill_path"] = json!(p);
                }
                if !output.status.success() {
                    res_obj["error"] = json!(format!("Script exited with code {:?}", output.status.code()));
                }

                Ok(res_obj)
            }
            Ok(Err(e)) => Err(format!("Execution process failure: {}", e)),
            Err(_) => {
                // Timeout occurred
                let timeout_msg = format!("Script timed out after {}s and was killed.", timeout_duration.as_secs());
                let (truncated_out, meta) = truncate_stdout_text(&format!("⏰ {}", timeout_msg)).await;
                let res_obj = json!({
                    "status": "timeout",
                    "output": truncated_out,
                    "error": timeout_msg,
                    "exit_code": 124,
                    "duration_seconds": (duration_secs * 100.0).round() / 100.0,
                    "stdout_truncated": meta.stdout_truncated,
                    "stdout_bytes_captured": meta.stdout_bytes_captured,
                    "stdout_bytes_total": meta.stdout_bytes_total,
                    "stdout_bytes_omitted": meta.stdout_bytes_omitted,
                });
                Ok(res_obj)
            }
        }
    }

    /// Handles RPC requests sent from Python hermes_tools
    pub async fn handle_rpc_request(req_str: &str, tool_registry: &ToolRegistry, expected_token: &str) -> Value {
        let req_json: Value = match serde_json::from_str(req_str) {
            Ok(v) => v,
            Err(e) => {
                return json!({ "error": format!("Invalid JSON request: {}", e) });
            }
        };

        let token = req_json.get("token").and_then(|v| v.as_str()).unwrap_or("");
        if token != expected_token {
            return json!({ "error": "Unauthorized RPC token" });
        }

        let tool_name = match req_json.get("tool").and_then(|v| v.as_str()) {
            Some(t) => t,
            None => return json!({ "error": "Missing tool field in RPC request" }),
        };

        let args = req_json.get("args").cloned().unwrap_or(json!({}));

        let call = ToolCall {
            id: format!("rpc_{}_{}", tool_name, uuid::Uuid::new_v4()),
            name: tool_name.to_string(),
            arguments: args,
        };

        let result = match tool_name {
            "terminal" => tool_registry.execute_terminal(&call).await,
            "read_file" => tool_registry.execute_read_file(&call).await,
            "write_file" => tool_registry.execute_write_file(&call).await,
            "patch" => tool_registry.execute_patch(&call).await,
            "search_files" => tool_registry.execute_search_files(&call).await,
            "web_search" => tool_registry.execute_web_search(&call).await,
            "web_extract" => tool_registry.execute_web_extract(&call).await,
            "skills_list" => tool_registry.execute_skills_list(&call).await,
            "skill_view" => tool_registry.execute_skill_view(&call).await,
            _ => tool_registry.execute_terminal(&call).await,
        };

        if let Ok(parsed) = serde_json::from_str::<Value>(&result.output) {
            parsed
        } else {
            json!({
                "output": result.output,
                "success": result.success
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_head_tail_split_ratio() {
        let (head, tail) = head_tail_split(50_000);
        assert_eq!(head, 20_000); // 40% of 50,000
        assert_eq!(tail, 30_000); // 60% of 50,000
        assert_eq!(head + tail, 50_000);
    }

    #[tokio::test]
    async fn test_stdout_truncation_under_50kb() {
        let small_out = "Hello world from Rust PTC\n".repeat(10);
        let (out, meta) = truncate_stdout_text(&small_out).await;
        assert_eq!(out, small_out);
        assert!(!meta.stdout_truncated);
        assert_eq!(meta.stdout_bytes_omitted, 0);
        assert!(meta.warning.is_none());
        assert!(meta.stdout_spill_path.is_none());
    }

    #[tokio::test]
    async fn test_stdout_truncation_over_50kb_and_spill_file() {
        let large_out = "0123456789abcdefghijklmnopqrstuvwxyz\n".repeat(2000); // ~74 KB
        assert!(large_out.len() > MAX_STDOUT_BYTES);

        let (truncated, meta) = truncate_stdout_text(&large_out).await;
        assert!(meta.stdout_truncated);
        assert_eq!(meta.stdout_bytes_captured, MAX_STDOUT_BYTES);
        assert_eq!(meta.stdout_bytes_total, large_out.len());
        assert_eq!(meta.stdout_bytes_omitted, large_out.len() - MAX_STDOUT_BYTES);

        assert!(truncated.contains("... [OUTPUT TRUNCATED -"));
        assert!(truncated.contains("omitted out of"));
        assert!(meta.warning.is_some());
        assert!(meta.stdout_spill_path.is_some());

        let spill_path = meta.stdout_spill_path.unwrap();
        assert!(Path::new(&spill_path).exists());
        let spilled_content = tokio::fs::read_to_string(&spill_path).await.unwrap();
        assert_eq!(spilled_content.len(), large_out.len());
        assert_eq!(spilled_content, large_out);

        let _ = tokio::fs::remove_file(spill_path).await;
    }

    #[tokio::test]
    async fn test_code_execution_engine_basic_python_run() {
        let engine = CodeExecutionEngine::new();
        let session_id = "test_sess_basic";

        let code = r#"
a = 10
b = 20
print(f"SUM={a+b}")
"#;

        let res = engine.execute(session_id, code, true, Some(10), None).await.expect("execute should succeed");
        assert_eq!(res.get("status").and_then(|v| v.as_str()), Some("success"));
        assert_eq!(res.get("exit_code").and_then(|v| v.as_i64()), Some(0));
        let output = res.get("output").and_then(|v| v.as_str()).unwrap();
        assert!(output.contains("SUM=30"));
    }

    #[tokio::test]
    async fn test_code_execution_engine_persistence_across_runs() {
        let engine = CodeExecutionEngine::new();
        let session_id = "test_sess_persistent";

        // Step 1: define variable
        let code1 = "x = 42\nprint('Initialized x')";
        let res1 = engine.execute(session_id, code1, true, Some(10), None).await.unwrap();
        assert_eq!(res1.get("status").and_then(|v| v.as_str()), Some("success"));

        // Step 2: use variable from previous execution
        let code2 = "print(f'VALUE_OF_X={x}')";
        let res2 = engine.execute(session_id, code2, false, Some(10), None).await.unwrap();
        assert_eq!(res2.get("status").and_then(|v| v.as_str()), Some("success"));
        let output2 = res2.get("output").and_then(|v| v.as_str()).unwrap();
        assert!(output2.contains("VALUE_OF_X=42"));

        // Step 3: execute with reset=true, variable should be gone
        let code3 = "print('x in globals:', 'x' in globals() or 'x' in locals())";
        let res3 = engine.execute(session_id, code3, true, Some(10), None).await.unwrap();
        assert_eq!(res3.get("status").and_then(|v| v.as_str()), Some("success"));
        let output3 = res3.get("output").and_then(|v| v.as_str()).unwrap();
        assert!(output3.contains("x in globals: False"));
    }

    #[tokio::test]
    async fn test_code_execution_hermes_tools_rpc_integration() {
        let engine = CodeExecutionEngine::new();
        let tool_registry = Arc::new(ToolRegistry::new());
        let session_id = "test_sess_rpc";

        let code = r#"
from hermes_tools import terminal, json_parse, shell_quote

test_str = "hello 'world'"
escaped = shell_quote(test_str)
cmd = f"echo {escaped}"
res = terminal(command=cmd)
print(f"TERMINAL_STATUS={res.get('success')}")
print(f"TERMINAL_OUT={res.get('output').strip()}")
"#;

        let res = engine
            .execute(session_id, code, true, Some(30), Some(tool_registry))
            .await
            .expect("RPC code execution should succeed");

        assert_eq!(res.get("status").and_then(|v| v.as_str()), Some("success"));
        let output = res.get("output").and_then(|v| v.as_str()).unwrap();
        assert!(output.contains("TERMINAL_STATUS=True"));
        assert!(output.contains("TERMINAL_OUT=hello 'world'"));
    }
}
