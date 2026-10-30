//! Browser Automation Suite for Hermes Gateway.
//!
//! Provides CDP (Chrome DevTools Protocol) HTTP/JSON based control:
//! - browser_navigate(url): Navigates to URL with SSRF checks, returns title and status.
//! - browser_snapshot(): Returns textual snapshot of current page's accessibility / DOM tree.
//! - browser_click(selector, x, y): Clicks element by CSS selector or coordinates.
//! - browser_type(selector, text): Types text into input field.
//! - browser_exec(code): Runs browser automation script commands.

use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::env;
use std::time::Duration;
// use tracing::{error, info, warn};
use url::Url;

use crate::web::{validate_dns_safety, validate_url_safety, ToolError};

/// Default CDP endpoint (override with BROWSER_CDP_URL)
pub const DEFAULT_CDP_URL: &str = "http://127.0.0.1:9222";

/// Default solver endpoint (override with BROWSER_SOLVER_URL)
pub const DEFAULT_SOLVER_URL: &str = "http://127.0.0.1:8095";

/// Get configured CDP base URL from environment or default
pub fn get_cdp_base_url() -> String {
    env::var("BROWSER_CDP_URL")
        .unwrap_or_else(|_| DEFAULT_CDP_URL.to_string())
        .trim_end_matches('/')
        .to_string()
}

/// Target descriptor from /json/list
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CdpTarget {
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(rename = "type")]
    pub target_type: String,
    #[serde(default)]
    pub url: String,
    #[serde(rename = "webSocketDebuggerUrl", default)]
    pub websocket_debugger_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NavigateResult {
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotResult {
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionResult {
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScreenshotResult {
    pub success: bool,
    pub file_path: String,
    pub format: String,
    pub media_tag: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecResult {
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

pub struct BrowserClient {
    client: Client,
    cdp_base_url: String,
}

impl Default for BrowserClient {
    fn default() -> Self {
        Self::new(None)
    }
}

impl BrowserClient {
    pub fn new(cdp_base_url: Option<String>) -> Self {
        let base_url = cdp_base_url.unwrap_or_else(get_cdp_base_url);
        let client = Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .unwrap_or_default();

        Self {
            client,
            cdp_base_url: base_url,
        }
    }

    /// List active browser targets
    pub async fn list_targets(&self) -> Result<Vec<CdpTarget>, ToolError> {
        let url = format!("{}/json/list", self.cdp_base_url);
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| ToolError::RequestError(format!("Failed to connect to CDP at {}: {}", url, e)))?;

        let targets: Vec<CdpTarget> = resp
            .json()
            .await
            .map_err(|e| ToolError::ParseError(format!("Failed to parse CDP targets: {}", e)))?;

        Ok(targets)
    }

    /// Find the primary active page target, or create one if none exist
    pub async fn get_or_create_page_target(&self) -> Result<CdpTarget, ToolError> {
        let targets = self.list_targets().await?;
        for t in targets {
            if t.target_type == "page" {
                return Ok(t);
            }
        }

        // None found, open a new blank page
        self.new_tab("about:blank").await
    }

    /// Open a new tab
    pub async fn new_tab(&self, target_url: &str) -> Result<CdpTarget, ToolError> {
        let encoded_url = urlencoding::encode(target_url);
        let url = format!("{}/json/new?{}", self.cdp_base_url, encoded_url);
        let resp = self
            .client
            .put(&url)
            .send()
            .await
            .map_err(|e| ToolError::RequestError(format!("Failed to create new CDP tab: {}", e)))?;

        let target: CdpTarget = resp
            .json()
            .await
            .map_err(|e| ToolError::ParseError(format!("Failed to parse created target: {}", e)))?;

        Ok(target)
    }

    /// Close a target by ID
    pub async fn close_target(&self, target_id: &str) -> Result<(), ToolError> {
        let url = format!("{}/json/close/{}", self.cdp_base_url, target_id);
        let _ = self.client.get(&url).send().await;
        Ok(())
    }

    /// Execute JavaScript expression on the given page target via CDP session
    /// Send an arbitrary CDP method call over WebSocket and wait for result
    pub async fn send_cdp_command(&self, target: &CdpTarget, method: &str, params: Value) -> Result<Value, ToolError> {
        use futures::{SinkExt, StreamExt};
        use tokio_tungstenite::connect_async;
        use tungstenite::Message;

        let ws_url_str = match &target.websocket_debugger_url {
            Some(u) => u.clone(),
            None => {
                let parsed = Url::parse(&self.cdp_base_url)
                    .map_err(|e| ToolError::InvalidUrl(e.to_string()))?;
                let host = parsed.host_str().unwrap_or("127.0.0.1");
                let port = parsed.port().unwrap_or(9222);
                format!("ws://{}:{}/devtools/page/{}", host, port, target.id)
            }
        };

        let (mut ws_stream, _) = tokio::time::timeout(
            Duration::from_secs(5),
            connect_async(&ws_url_str),
        )
        .await
        .map_err(|_| ToolError::RequestError("WebSocket connect timed out".to_string()))?
        .map_err(|e| ToolError::RequestError(format!("WebSocket connect failed to {}: {}", ws_url_str, e)))?;

        let req_id = 1;
        let cdp_msg = serde_json::json!({
            "id": req_id,
            "method": method,
            "params": params
        });

        ws_stream
            .send(Message::Text(cdp_msg.to_string()))
            .await
            .map_err(|e| ToolError::RequestError(format!("Failed to send CDP message: {}", e)))?;

        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        while tokio::time::Instant::now() < deadline {
            if let Ok(Some(msg)) = tokio::time::timeout(Duration::from_millis(2000), ws_stream.next()).await {
                if let Ok(Message::Text(text)) = msg {
                    if let Ok(val) = serde_json::from_str::<Value>(&text) {
                        if val.get("id").and_then(|i| i.as_i64()) == Some(req_id) {
                            if let Some(res) = val.get("result") {
                                let _ = ws_stream.close(None).await;
                                return Ok(res.clone());
                            }
                            if let Some(err) = val.get("error") {
                                let _ = ws_stream.close(None).await;
                                return Err(ToolError::ExecutionError(err.to_string()));
                            }
                            let _ = ws_stream.close(None).await;
                            return Ok(val);
                        }
                    }
                }
            } else {
                break;
            }
        }

        let _ = ws_stream.close(None).await;
        Err(ToolError::ExecutionError(format!("CDP command {} timed out waiting for response", method)))
    }

    pub async fn eval_js(&self, target: &CdpTarget, script: &str) -> Result<Value, ToolError> {
        self.eval_via_websocket(target, script).await
    }

    async fn eval_via_websocket(&self, target: &CdpTarget, script: &str) -> Result<Value, ToolError> {
        use futures::{SinkExt, StreamExt};
        use tokio_tungstenite::connect_async;
        use tungstenite::Message;

        let ws_url_str = match &target.websocket_debugger_url {
            Some(u) => u.clone(),
            None => {
                let parsed = Url::parse(&self.cdp_base_url)
                    .map_err(|e| ToolError::InvalidUrl(e.to_string()))?;
                let host = parsed.host_str().unwrap_or("127.0.0.1");
                let port = parsed.port().unwrap_or(9222);
                format!("ws://{}:{}/devtools/page/{}", host, port, target.id)
            }
        };

        let (mut ws_stream, _) = tokio::time::timeout(
            Duration::from_secs(5),
            connect_async(&ws_url_str),
        )
        .await
        .map_err(|_| ToolError::RequestError("WebSocket connect timed out".to_string()))?
        .map_err(|e| ToolError::RequestError(format!("WebSocket connect failed to {}: {}", ws_url_str, e)))?;

        let req_id = 1;
        let cdp_msg = serde_json::json!({
            "id": req_id,
            "method": "Runtime.evaluate",
            "params": {
                "expression": script,
                "returnByValue": true,
                "awaitPromise": true
            }
        });

        ws_stream
            .send(Message::Text(cdp_msg.to_string()))
            .await
            .map_err(|e| ToolError::RequestError(format!("Failed to send CDP message: {}", e)))?;

        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while tokio::time::Instant::now() < deadline {
            if let Ok(Some(msg)) = tokio::time::timeout(Duration::from_millis(1500), ws_stream.next()).await {
                if let Ok(Message::Text(text)) = msg {
                    if let Ok(val) = serde_json::from_str::<Value>(&text) {
                        if val.get("id").and_then(|i| i.as_i64()) == Some(req_id) {
                            if let Some(res) = val.get("result").and_then(|r| r.get("result")) {
                                if let Some(v) = res.get("value") {
                                    let _ = ws_stream.close(None).await;
                                    return Ok(v.clone());
                                }
                                let _ = ws_stream.close(None).await;
                                return Ok(res.clone());
                            }
                            if let Some(err) = val.get("error") {
                                let _ = ws_stream.close(None).await;
                                return Err(ToolError::ExecutionError(err.to_string()));
                            }
                            let _ = ws_stream.close(None).await;
                            return Ok(val);
                        }
                    }
                }
            } else {
                break;
            }
        }

        let _ = ws_stream.close(None).await;
        Ok(Value::Null)
    }

    /// Navigate to URL with SSRF protection
    pub async fn navigate(&self, target_url: &str) -> Result<NavigateResult, ToolError> {
        if target_url != "about:blank" {
            let parsed = validate_url_safety(target_url)?;
            if let Some(host) = parsed.host_str() {
                validate_dns_safety(host).await?;
            }
        }

        let target = self.get_or_create_page_target().await?;
        let script = format!("window.location.href = {};", serde_json::to_string(target_url).unwrap_or_default());
        let _ = self.eval_js(&target, &script).await;

        tokio::time::sleep(Duration::from_millis(600)).await;

        let title_val = self.eval_js(&target, "document.title").await.unwrap_or(Value::Null);
        let title = title_val.as_str().map(|s| s.to_string()).unwrap_or_default();

        Ok(NavigateResult {
            success: true,
            url: Some(target_url.to_string()),
            title: Some(title),
            target_id: Some(target.id),
            error: None,
        })
    }

    /// Snapshot page accessibility/DOM tree
    pub async fn snapshot(&self) -> Result<SnapshotResult, ToolError> {
        let target = self.get_or_create_page_target().await?;

        let script = r#"
(() => {
    const lines = [];
    lines.push('URL: ' + window.location.href);
    lines.push('Title: ' + document.title);
    lines.push('--- Interactive & Key Elements ---');

    let idx = 1;
    const elements = document.querySelectorAll('button, a, input, textarea, select, [role="button"], h1, h2, h3, p');
    for (const el of elements) {
        const tag = el.tagName.toLowerCase();
        const text = (el.innerText || el.value || el.placeholder || el.getAttribute('aria-label') || '').trim().replace(/\s+/g, ' ');
        if (!text && tag !== 'input') continue;
        
        let desc = `@e${idx++} [${tag}]`;
        if (el.id) desc += ` #${el.id}`;
        if (el.className && typeof el.className === 'string') {
            const cls = el.className.trim().split(/\s+/).slice(0, 2).join('.');
            if (cls) desc += ` .${cls}`;
        }
        if (text) {
            desc += `: "${text.length > 80 ? text.slice(0, 77) + '...' : text}"`;
        }
        lines.push(desc);
        if (lines.length > 100) {
            lines.push('... [truncated remainder of page]');
            break;
        }
    }
    return lines.join('\n');
})()
"#;

        let snap_val = self.eval_js(&target, script).await?;
        let snap_str = snap_val.as_str().unwrap_or("").to_string();

        Ok(SnapshotResult {
            success: true,
            url: Some(target.url),
            title: Some(target.title),
            snapshot: Some(snap_str),
            error: None,
        })
    }

    /// Click element by CSS selector or coordinates
    pub async fn click(&self, selector: Option<&str>, x: Option<f64>, y: Option<f64>) -> Result<ActionResult, ToolError> {
        let target = self.get_or_create_page_target().await?;

        if let Some(sel) = selector {
            let script = format!(
                r#"
(() => {{
    const el = document.querySelector({});
    if (!el) return {{ found: false, error: 'Element not found: ' + {} }};
    el.scrollIntoView({{ behavior: 'instant', block: 'center' }});
    el.focus();
    el.click();
    return {{ found: true }};
}})()
"#,
                serde_json::to_string(sel).unwrap_or_default(),
                serde_json::to_string(sel).unwrap_or_default()
            );

            let res = self.eval_js(&target, &script).await?;
            if let Some(false) = res.get("found").and_then(|v| v.as_bool()) {
                let err_msg = res.get("error").and_then(|e| e.as_str()).unwrap_or("Element not found");
                return Ok(ActionResult {
                    success: false,
                    message: None,
                    error: Some(err_msg.to_string()),
                });
            }

            Ok(ActionResult {
                success: true,
                message: Some(format!("Clicked element matching '{}'", sel)),
                error: None,
            })
        } else if let (Some(x_pos), Some(y_pos)) = (x, y) {
            let script = format!(
                r#"
(() => {{
    const el = document.elementFromPoint({}, {});
    if (el) {{
        el.click();
        return {{ found: true, tag: el.tagName.toLowerCase() }};
    }}
    return {{ found: false }};
}})()
"#,
                x_pos, y_pos
            );
            let res = self.eval_js(&target, &script).await?;
            let tag = res.get("tag").and_then(|t| t.as_str()).unwrap_or("point");

            Ok(ActionResult {
                success: true,
                message: Some(format!("Clicked ({}, {}) on <{}>", x_pos, y_pos, tag)),
                error: None,
            })
        } else {
            Err(ToolError::ExecutionError("Either selector or (x, y) coordinates must be provided".to_string()))
        }
    }

    /// Type text into input field matching selector
    pub async fn type_text(&self, selector: &str, text: &str) -> Result<ActionResult, ToolError> {
        let target = self.get_or_create_page_target().await?;

        let script = format!(
            r#"
(() => {{
    const el = document.querySelector({});
    if (!el) return {{ found: false, error: 'Element not found: ' + {} }};
    el.scrollIntoView({{ behavior: 'instant', block: 'center' }});
    el.focus();
    if ('value' in el) {{
        el.value = {};
        el.dispatchEvent(new Event('input', {{ bubbles: true }}));
        el.dispatchEvent(new Event('change', {{ bubbles: true }}));
    }} else {{
        el.textContent = {};
    }}
    return {{ found: true }};
}})()
"#,
            serde_json::to_string(selector).unwrap_or_default(),
            serde_json::to_string(selector).unwrap_or_default(),
            serde_json::to_string(text).unwrap_or_default(),
            serde_json::to_string(text).unwrap_or_default()
        );

        let res = self.eval_js(&target, &script).await?;
        if let Some(false) = res.get("found").and_then(|v| v.as_bool()) {
            let err_msg = res.get("error").and_then(|e| e.as_str()).unwrap_or("Element not found");
            return Ok(ActionResult {
                success: false,
                message: None,
                error: Some(err_msg.to_string()),
            });
        }

        Ok(ActionResult {
            success: true,
            message: Some(format!("Typed text into '{}'", selector)),
            error: None,
        })
    }

    /// Execute automated script against browser CDP
    pub async fn exec_script(&self, code: &str) -> Result<ExecResult, ToolError> {
        let target = self.get_or_create_page_target().await?;
        let res = self.eval_js(&target, code).await?;
        
        let output_str = if let Some(s) = res.as_str() {
            s.to_string()
        } else {
            serde_json::to_string_pretty(&res).unwrap_or_else(|_| res.to_string())
        };

        Ok(ExecResult {
            success: true,
            output: Some(output_str),
            error: None,
        })
    }

    /// Capture screenshot via CDP Page.captureScreenshot and save to disk
    pub async fn take_screenshot(&self, format: Option<&str>, quality: Option<u32>) -> Result<ScreenshotResult, ToolError> {
        use base64::Engine;
        use std::time::SystemTime;

        let target = self.get_or_create_page_target().await?;
        let fmt = match format.unwrap_or("png").to_lowercase().as_str() {
            "jpeg" | "jpg" => "jpeg",
            _ => "png",
        };

        let mut params = serde_json::json!({
            "format": fmt,
            "fromSurface": true
        });

        if fmt == "jpeg" {
            let q = quality.unwrap_or(80).min(100);
            params["quality"] = serde_json::json!(q);
        }

        let res = self.send_cdp_command(&target, "Page.captureScreenshot", params).await?;
        let b64_data = res.get("data").and_then(|d| d.as_str()).ok_or_else(|| {
            ToolError::ExecutionError("CDP Page.captureScreenshot returned no data".to_string())
        })?;

        let bytes = base64::engine::general_purpose::STANDARD
            .decode(b64_data)
            .map_err(|e| ToolError::ParseError(format!("Failed to decode screenshot base64 data: {}", e)))?;

        let screenshots_dir = std::path::PathBuf::from("/tmp/hermes_browser_screenshots");
        tokio::fs::create_dir_all(&screenshots_dir)
            .await
            .map_err(|e| ToolError::ExecutionError(format!("Failed to create screenshots directory: {}", e)))?;

        let timestamp = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);

        let ext = if fmt == "jpeg" { "jpg" } else { "png" };
        let file_path = screenshots_dir.join(format!("screenshot-{}.{}", timestamp, ext));
        let file_path_str = file_path.to_string_lossy().to_string();

        tokio::fs::write(&file_path, bytes)
            .await
            .map_err(|e| ToolError::ExecutionError(format!("Failed to save screenshot file: {}", e)))?;

        let media_tag = format!("MEDIA:{}", file_path_str);

        Ok(ScreenshotResult {
            success: true,
            file_path: file_path_str,
            format: fmt.to_string(),
            media_tag,
            error: None,
        })
    }

}

// Global convenience functions matching Python Hermes signatures

pub async fn browser_navigate(url: &str) -> Result<NavigateResult, ToolError> {
    let client = BrowserClient::default();
    client.navigate(url).await
}

pub async fn browser_snapshot() -> Result<SnapshotResult, ToolError> {
    let client = BrowserClient::default();
    client.snapshot().await
}

pub async fn browser_click(selector: Option<&str>, x: Option<f64>, y: Option<f64>) -> Result<ActionResult, ToolError> {
    let client = BrowserClient::default();
    client.click(selector, x, y).await
}

pub async fn browser_type(selector: &str, text: &str) -> Result<ActionResult, ToolError> {
    let client = BrowserClient::default();
    client.type_text(selector, text).await
}

pub async fn browser_exec(code: &str) -> Result<ExecResult, ToolError> {
    let client = BrowserClient::default();
    client.exec_script(code).await
}

pub async fn browser_take_screenshot(format: Option<&str>, quality: Option<u32>) -> Result<ScreenshotResult, ToolError> {
    let client = BrowserClient::default();
    client.take_screenshot(format, quality).await
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cdp_base_url_default_and_env() {
        env::remove_var("BROWSER_CDP_URL");
        assert_eq!(get_cdp_base_url(), "http://127.0.0.1:9222");

        env::set_var("BROWSER_CDP_URL", "http://localhost:9999/");
        assert_eq!(get_cdp_base_url(), "http://localhost:9999");
        env::remove_var("BROWSER_CDP_URL");
    }

    #[tokio::test]
    async fn test_browser_navigate_ssrf_rejection() {
        let res = browser_navigate("http://192.168.1.10/admin").await;
        assert!(res.is_err());
        match res.err().unwrap() {
            ToolError::SsrfViolation(_) => {}
            other => panic!("Expected SsrfViolation, got {:?}", other),
        }

        let res2 = browser_navigate("http://127.0.0.1:8080/secret").await;
        assert!(res2.is_err());
    }

    #[tokio::test]
    async fn test_browser_actions_parameter_validation() {
        let client = BrowserClient::default();
        let res = client.click(None, None, None).await;
        assert!(res.is_err());
    }

    #[test]
    fn test_screenshot_result_serialization() {
        let res = ScreenshotResult {
            success: true,
            file_path: "/tmp/hermes_browser_screenshots/screenshot-123.png".to_string(),
            format: "png".to_string(),
            media_tag: "MEDIA:/tmp/hermes_browser_screenshots/screenshot-123.png".to_string(),
            error: None,
        };
        let json = serde_json::to_string(&res).expect("serialize screenshot");
        assert!(json.contains("MEDIA:/tmp/hermes_browser_screenshots/screenshot-123.png"));
        assert!(json.contains("\"success\":true"));
    }
}
