use serde::{Deserialize, Serialize};
use std::net::{IpAddr, Ipv4Addr};
use std::time::Duration;
use url::Url;

pub const DEFAULT_CHAR_LIMIT: usize = 15000;
pub const MAX_CHAR_LIMIT: usize = 500_000;
pub const MIN_CHAR_LIMIT: usize = 2000;

#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("SSRF security violation: {0}")]
    SsrfViolation(String),
    #[error("Invalid URL: {0}")]
    InvalidUrl(String),
    #[error("Request error: {0}")]
    RequestError(String),
    #[error("Parse error: {0}")]
    ParseError(String),
    #[error("Execution error: {0}")]
    ExecutionError(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WebSearchResultItem {
    pub title: String,
    pub url: String,
    pub description: String,
    pub position: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebSearchResponseData {
    pub web: Vec<WebSearchResultItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebSearchResponse {
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<WebSearchResponseData>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WebExtractItem {
    pub url: String,
    pub title: String,
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebExtractResponse {
    pub success: bool,
    pub results: Vec<WebExtractItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// SSRF protection: reject private/loopback/cloud metadata/internal IPs
pub fn is_private_or_blocked_ip(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(ipv4) => {
            // Check loopback: 127.0.0.0/8
            if ipv4.is_loopback() {
                return true;
            }
            // Check unspecified: 0.0.0.0/8
            if ipv4.is_unspecified() || ipv4.octets()[0] == 0 {
                return true;
            }
            // Check private RFC 1918:
            // 10.0.0.0/8
            // 172.16.0.0/12
            // 192.168.0.0/16
            if ipv4.is_private() {
                return true;
            }
            // Check link local: 169.254.0.0/16 (AWS / GCP / Cloud metadata)
            if ipv4.is_link_local() {
                return true;
            }
            // Broadcast
            if ipv4.is_broadcast() {
                return true;
            }
            // CGNAT: 100.64.0.0/10 (100.64.0.0 - 100.127.255.255)
            let octets = ipv4.octets();
            if octets[0] == 100 && (octets[1] >= 64 && octets[1] <= 127) {
                return true;
            }
            // Alibaba metadata: 100.100.100.200 (also covered by CGNAT range)
            if *ipv4 == Ipv4Addr::new(100, 100, 100, 200) {
                return true;
            }
            false
        }
        IpAddr::V6(ipv6) => {
            // Check loopback ::1
            if ipv6.is_loopback() {
                return true;
            }
            // Check unspecified ::
            if ipv6.is_unspecified() {
                return true;
            }
            // Check IPv4-mapped IPv6 ::ffff:a.b.c.d
            if let Some(mapped_v4) = ipv6.to_ipv4() {
                return is_private_or_blocked_ip(&IpAddr::V4(mapped_v4));
            }
            // Check Unique Local Addresses (ULA): fc00::/7 (fc00... and fd00...)
            let seg0 = ipv6.segments()[0];
            if (seg0 & 0xfe00) == 0xfc00 {
                return true;
            }
            // Check IPv6 Link Local: fe80::/10
            if (seg0 & 0xffc0) == 0xfe80 {
                return true;
            }
            false
        }
    }
}

/// Validate URL against SSRF attacks
pub fn validate_url_safety(url_str: &str) -> Result<Url, ToolError> {
    let parsed = Url::parse(url_str).map_err(|e| ToolError::InvalidUrl(e.to_string()))?;
    
    let scheme = parsed.scheme().to_lowercase();
    if scheme != "http" && scheme != "https" {
        return Err(ToolError::InvalidUrl(format!("Unsupported scheme: '{}'. Only http/https supported", scheme)));
    }

    let host_str = match parsed.host_str() {
        Some(h) => h.to_lowercase(),
        None => return Err(ToolError::InvalidUrl("Missing host in URL".to_string())),
    };

    // Check literal blocked hostnames
    if host_str == "localhost" || host_str.ends_with(".localhost") || host_str.ends_with(".local") || host_str.ends_with(".internal") || host_str == "metadata.google.internal" || host_str == "metadata.goog" {
        return Err(ToolError::SsrfViolation(format!("Blocked internal hostname: {}", host_str)));
    }

    // Check if host parses directly as IP
    if let Ok(ip) = host_str.parse::<IpAddr>() {
        if is_private_or_blocked_ip(&ip) {
            return Err(ToolError::SsrfViolation(format!("Blocked private/loopback IP address: {}", ip)));
        }
    }

    Ok(parsed)
}

/// Resolves DNS and validates all IP addresses for a given host
pub async fn validate_dns_safety(host: &str) -> Result<(), ToolError> {
    // If it is already an IP, it was checked by validate_url_safety
    if let Ok(ip) = host.parse::<IpAddr>() {
        if is_private_or_blocked_ip(&ip) {
            return Err(ToolError::SsrfViolation(format!("Blocked private/loopback IP address: {}", ip)));
        }
        return Ok(());
    }

    let host_with_port = format!("{}:80", host);
    let resolved = tokio::net::lookup_host(&host_with_port)
        .await
        .map_err(|e| ToolError::RequestError(format!("DNS resolution failed for {}: {}", host, e)))?;

    for socket_addr in resolved {
        let ip = socket_addr.ip();
        if is_private_or_blocked_ip(&ip) {
            return Err(ToolError::SsrfViolation(format!(
                "Host '{}' resolved to private/loopback address: {}",
                host, ip
            )));
        }
    }

    Ok(())
}

/// Strip HTML tags and convert basic structures to clean text/markdown
pub fn html_to_markdown(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    let mut in_script_or_style = false;
    let mut current_tag = String::new();

    let bytes = html.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        let c = bytes[i] as char;

        if c == '<' {
            in_tag = true;
            current_tag.clear();
            i += 1;
            while i < bytes.len() && (bytes[i] as char) != '>' && !(bytes[i] as char).is_whitespace() {
                current_tag.push((bytes[i] as char).to_ascii_lowercase());
                i += 1;
            }
            if current_tag == "script" || current_tag == "style" || current_tag == "noscript" {
                in_script_or_style = true;
            } else if current_tag == "/script" || current_tag == "/style" || current_tag == "/noscript" {
                in_script_or_style = false;
            }
            while i < bytes.len() && (bytes[i] as char) != '>' {
                i += 1;
            }
            if i < bytes.len() && (bytes[i] as char) == '>' {
                in_tag = false;
                if !in_script_or_style {
                    match current_tag.as_str() {
                        "p" | "/p" | "div" | "/div" | "br" | "h1" | "/h1" | "h2" | "/h2" | "h3" | "/h3" | "h4" | "/h4" | "li" | "/li" | "tr" | "/tr" => {
                            if !out.ends_with('\n') {
                                out.push('\n');
                            }
                        }
                        _ => {}
                    }
                }
            }
            i += 1;
            continue;
        }

        if !in_tag && !in_script_or_style {
            out.push(c);
        }
        i += 1;
    }

    // Decode basic HTML entities
    let decoded = out
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'");

    // Clean multiple blank lines and whitespace
    let mut cleaned = String::new();
    let mut consecutive_newlines = 0;

    for line in decoded.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            consecutive_newlines += 1;
            if consecutive_newlines <= 2 {
                cleaned.push('\n');
            }
        } else {
            consecutive_newlines = 0;
            cleaned.push_str(trimmed);
            cleaned.push('\n');
        }
    }

    cleaned.trim().to_string()
}

/// Truncate text according to head+tail budget with footer pointing to omitted middle
pub fn truncate_content(content: &str, url: &str, char_limit: Option<usize>) -> (String, bool) {
    let limit = char_limit
        .unwrap_or(DEFAULT_CHAR_LIMIT)
        .clamp(MIN_CHAR_LIMIT, MAX_CHAR_LIMIT);

    if content.len() <= limit {
        return (content.to_string(), false);
    }

    let head_budget = (limit as f64 * 0.75) as usize;
    let tail_budget = limit - head_budget;

    let head_raw = &content[..head_budget];
    let tail_raw = &content[content.len() - tail_budget..];

    let head = if let Some(pos) = head_raw.rfind('\n') {
        if pos > head_budget / 2 {
            &head_raw[..pos]
        } else {
            head_raw
        }
    } else {
        head_raw
    };

    let tail = if let Some(pos) = tail_raw.find('\n') {
        if pos < tail_budget / 2 {
            &tail_raw[pos + 1..]
        } else {
            tail_raw
        }
    } else {
        tail_raw
    };

    let footer = format!(
        "\n\n──────── [TRUNCATED] ────────\nShowing {} chars (head) + {} chars (tail) of {} total clean characters.\nSource URL: {}\n─────────────────────────────",
        head.len(),
        tail.len(),
        content.len(),
        url
    );

    let result = format!("{}\n\n[... middle omitted — see footer ...]\n\n{}{}", head, tail, footer);
    (result, true)
}

/// Execute a web search across DuckDuckGo HTML / Instant Answers with resilient fallback
pub async fn web_search(query: &str, limit: usize) -> Result<WebSearchResponse, ToolError> {
    let query_trim = query.trim();
    if query_trim.is_empty() {
        return Ok(WebSearchResponse {
            success: true,
            data: Some(WebSearchResponseData { web: vec![] }),
            error: None,
        });
    }

    let target_limit = if limit == 0 { 5 } else { limit };
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(12))
        .build()
        .map_err(|e| ToolError::RequestError(e.to_string()))?;

    // 1. Try DuckDuckGo HTML search
    let encoded_q = urlencoding::encode(query_trim);
    let html_url = format!("https://html.duckduckgo.com/html/?q={}", encoded_q);

    let html_resp = client
        .get(&html_url)
        .header("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36")
        .header("Accept", "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8")
        .send()
        .await;

    if let Ok(resp) = html_resp {
        if resp.status().is_success() {
            if let Ok(body) = resp.text().await {
                let parsed_results = parse_duckduckgo_html(&body, target_limit);
                if !parsed_results.is_empty() {
                    return Ok(WebSearchResponse {
                        success: true,
                        data: Some(WebSearchResponseData {
                            web: parsed_results,
                        }),
                        error: None,
                    });
                }
            }
        }
    }

    // 2. Fallback to DuckDuckGo Instant Answer API
    let api_url = format!(
        "https://api.duckduckgo.com/?q={}&format=json&no_html=1&skip_disambig=1",
        encoded_q
    );
    let api_resp = client
        .get(&api_url)
        .header("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36")
        .send()
        .await;

    if let Ok(resp) = api_resp {
        if resp.status().is_success() {
            if let Ok(val) = resp.json::<serde_json::Value>().await {
                let mut results = Vec::new();
                let mut pos = 1;

                if let Some(abstract_text) = val.get("AbstractText").and_then(|t| t.as_str()) {
                    if !abstract_text.trim().is_empty() {
                        let abstract_url = val.get("AbstractURL").and_then(|u| u.as_str()).unwrap_or("");
                        let heading = val.get("Heading").and_then(|h| h.as_str()).unwrap_or(query_trim);
                        results.push(WebSearchResultItem {
                            title: heading.to_string(),
                            url: abstract_url.to_string(),
                            description: abstract_text.to_string(),
                            position: pos,
                        });
                        pos += 1;
                    }
                }

                if let Some(related) = val.get("RelatedTopics").and_then(|r| r.as_array()) {
                    for item in related {
                        if pos > target_limit {
                            break;
                        }
                        if let (Some(text), Some(first_url)) = (
                            item.get("Text").and_then(|t| t.as_str()),
                            item.get("FirstURL").and_then(|u| u.as_str()),
                        ) {
                            if !text.trim().is_empty() && !first_url.trim().is_empty() {
                                results.push(WebSearchResultItem {
                                    title: text.chars().take(80).collect::<String>(),
                                    url: first_url.to_string(),
                                    description: text.to_string(),
                                    position: pos,
                                });
                                pos += 1;
                            }
                        }
                    }
                }

                if !results.is_empty() {
                    return Ok(WebSearchResponse {
                        success: true,
                        data: Some(WebSearchResponseData { web: results }),
                        error: None,
                    });
                }
            }
        }
    }

    Ok(WebSearchResponse {
        success: true,
        data: Some(WebSearchResponseData { web: vec![] }),
        error: None,
    })
}

/// Helper to parse DuckDuckGo HTML results
fn parse_duckduckgo_html(html: &str, limit: usize) -> Vec<WebSearchResultItem> {
    let mut items = Vec::new();

    // Look for results in standard ddg html structure
    let blocks: Vec<&str> = html.split("class=\"result ").collect();

    for block in blocks.into_iter().skip(1) {
        if items.len() >= limit {
            break;
        }

        let (raw_url, title) = if let Some(title_pos) = block.find("result__title") {
            let sub = &block[title_pos..];
            extract_tag_href_and_text(sub)
        } else {
            (None, None)
        };

        let snippet = if let Some(snip_pos) = block.find("result__snippet") {
            let sub = &block[snip_pos..];
            extract_tag_text(sub)
        } else {
            None
        };

        if let (Some(url_raw), Some(title_text)) = (raw_url, title) {
            let clean_url = unwrap_ddg_redirect(&url_raw);
            if clean_url.starts_with("http://") || clean_url.starts_with("https://") {
                if clean_url.contains("duckduckgo.com/y.js") || clean_url.contains("bing.com/aclick") {
                    continue;
                }
                items.push(WebSearchResultItem {
                    title: clean_html_entities(&title_text),
                    url: clean_url,
                    description: clean_html_entities(&snippet.unwrap_or_default()),
                    position: items.len() + 1,
                });
            }
        }
    }

    items
}

fn unwrap_ddg_redirect(raw: &str) -> String {
    if let Some(pos) = raw.find("uddg=") {
        let after = &raw[pos + 5..];
        let end = after.find('&').unwrap_or(after.len());
        if let Ok(unq) = urlencoding::decode(&after[..end]) {
            return unq.to_string();
        }
    }
    raw.to_string()
}

fn extract_tag_href_and_text(sub: &str) -> (Option<String>, Option<String>) {
    let href_start = match sub.find("href=\"") {
        Some(pos) => pos,
        None => return (None, None),
    };
    let href_val_start = href_start + 6;
    let href_end = match sub[href_val_start..].find('\"') {
        Some(pos) => pos + href_val_start,
        None => return (None, None),
    };
    let href = &sub[href_val_start..href_end];

    let tag_close = match sub[href_end..].find('>') {
        Some(pos) => pos + href_end + 1,
        None => return (None, None),
    };
    let a_close = match sub[tag_close..].find("</a>") {
        Some(pos) => pos + tag_close,
        None => return (None, None),
    };
    let text = &sub[tag_close..a_close];

    (Some(href.to_string()), Some(strip_all_tags(text)))
}

fn extract_tag_text(sub: &str) -> Option<String> {
    let tag_close = sub.find('>')? + 1;
    let tag_end = sub[tag_close..].find("</a>").or_else(|| sub[tag_close..].find("</div>"))? + tag_close;
    let text = &sub[tag_close..tag_end];
    Some(strip_all_tags(text))
}

fn strip_all_tags(s: &str) -> String {
    let mut out = String::new();
    let mut inside = false;
    for c in s.chars() {
        if c == '<' {
            inside = true;
        } else if c == '>' {
            inside = false;
        } else if !inside {
            out.push(c);
        }
    }
    out.trim().to_string()
}

fn clean_html_entities(s: &str) -> String {
    s.replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("<b>", "")
        .replace("</b>", "")
        .trim()
        .to_string()
}

/// Extract clean content from URLs with SSRF protection and character budget truncation
pub async fn web_extract(
    urls: &[String],
    char_limit: Option<usize>,
) -> Result<WebExtractResponse, ToolError> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .redirect(reqwest::redirect::Policy::limited(5))
        .build()
        .map_err(|e| ToolError::RequestError(e.to_string()))?;

    let mut results = Vec::new();

    for url_str in urls {
        let trimmed_url = url_str.trim();
        if trimmed_url.is_empty() {
            continue;
        }

        // 1. URL syntax and SSRF check
        let parsed_url = match validate_url_safety(trimmed_url) {
            Ok(u) => u,
            Err(e) => {
                results.push(WebExtractItem {
                    url: trimmed_url.to_string(),
                    title: String::new(),
                    content: String::new(),
                    error: Some(e.to_string()),
                });
                continue;
            }
        };

        // 2. DNS check against SSRF rebind
        if let Some(host) = parsed_url.host_str() {
            if let Err(e) = validate_dns_safety(host).await {
                results.push(WebExtractItem {
                    url: trimmed_url.to_string(),
                    title: String::new(),
                    content: String::new(),
                    error: Some(e.to_string()),
                });
                continue;
            }
        }

        // 3. Fetch URL content
        match client
            .get(parsed_url.as_str())
            .header("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36")
            .header("Accept", "text/html,application/xhtml+xml,application/xml;q=0.9,text/plain;q=0.8,*/*;q=0.7")
            .send()
            .await
        {
            Ok(resp) => {
                let status = resp.status();
                if !status.is_success() {
                    results.push(WebExtractItem {
                        url: trimmed_url.to_string(),
                        title: String::new(),
                        content: String::new(),
                        error: Some(format!("HTTP error status: {}", status)),
                    });
                    continue;
                }

                // Verify redirect target did not land on internal/loopback address
                let final_url = resp.url();
                if let Err(e) = validate_url_safety(final_url.as_str()) {
                    results.push(WebExtractItem {
                        url: trimmed_url.to_string(),
                        title: String::new(),
                        content: String::new(),
                        error: Some(format!("Redirect target violation: {}", e)),
                    });
                    continue;
                }

                match resp.text().await {
                    Ok(body) => {
                        let title = extract_html_title(&body).unwrap_or_else(|| trimmed_url.to_string());
                        let markdown = html_to_markdown(&body);
                        let (content, _was_truncated) = truncate_content(&markdown, trimmed_url, char_limit);

                        results.push(WebExtractItem {
                            url: trimmed_url.to_string(),
                            title,
                            content,
                            error: None,
                        });
                    }
                    Err(e) => {
                        results.push(WebExtractItem {
                            url: trimmed_url.to_string(),
                            title: String::new(),
                            content: String::new(),
                            error: Some(format!("Failed reading response body: {}", e)),
                        });
                    }
                }
            }
            Err(e) => {
                results.push(WebExtractItem {
                    url: trimmed_url.to_string(),
                    title: String::new(),
                    content: String::new(),
                    error: Some(format!("HTTP request failed: {}", e)),
                });
            }
        }
    }

    Ok(WebExtractResponse {
        success: true,
        results,
        error: None,
    })
}

fn extract_html_title(html: &str) -> Option<String> {
    let lower = html.to_lowercase();
    let start = lower.find("<title")?;
    let tag_close = lower[start..].find('>')? + start + 1;
    let end = lower[tag_close..].find("</title>")? + tag_close;
    let title_raw = &html[tag_close..end];
    Some(clean_html_entities(title_raw))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_private_or_blocked_ip() {
        // Loopback
        assert!(is_private_or_blocked_ip(&"127.0.0.1".parse().unwrap()));
        assert!(is_private_or_blocked_ip(&"127.0.1.1".parse().unwrap()));
        assert!(is_private_or_blocked_ip(&"::1".parse().unwrap()));

        // RFC 1918
        assert!(is_private_or_blocked_ip(&"10.0.0.1".parse().unwrap()));
        assert!(is_private_or_blocked_ip(&"10.254.254.254".parse().unwrap()));
        assert!(is_private_or_blocked_ip(&"172.16.0.1".parse().unwrap()));
        assert!(is_private_or_blocked_ip(&"172.31.255.255".parse().unwrap()));
        assert!(is_private_or_blocked_ip(&"192.168.1.1".parse().unwrap()));

        // Non-private
        assert!(!is_private_or_blocked_ip(&"172.32.0.1".parse().unwrap()));
        assert!(!is_private_or_blocked_ip(&"8.8.8.8".parse().unwrap()));
        assert!(!is_private_or_blocked_ip(&"1.1.1.1".parse().unwrap()));

        // Cloud metadata & Link-local
        assert!(is_private_or_blocked_ip(&"169.254.169.254".parse().unwrap()));
        assert!(is_private_or_blocked_ip(&"169.254.170.2".parse().unwrap()));

        // CGNAT
        assert!(is_private_or_blocked_ip(&"100.64.0.1".parse().unwrap()));
        assert!(is_private_or_blocked_ip(&"100.100.100.200".parse().unwrap()));
        assert!(is_private_or_blocked_ip(&"100.127.255.255".parse().unwrap()));
        assert!(!is_private_or_blocked_ip(&"100.128.0.1".parse().unwrap()));

        // IPv6 Unique Local
        assert!(is_private_or_blocked_ip(&"fc00::1".parse().unwrap()));
        assert!(is_private_or_blocked_ip(&"fd12:3456:789a::1".parse().unwrap()));
    }

    #[test]
    fn test_validate_url_safety() {
        assert!(validate_url_safety("http://127.0.0.1/admin").is_err());
        assert!(validate_url_safety("http://localhost:8080").is_err());
        assert!(validate_url_safety("http://192.168.1.100").is_err());
        assert!(validate_url_safety("http://10.1.2.3/secret").is_err());
        assert!(validate_url_safety("http://169.254.169.254/latest/meta-data").is_err());
        assert!(validate_url_safety("ftp://example.com/file").is_err());
        assert!(validate_url_safety("file:///etc/passwd").is_err());

        assert!(validate_url_safety("https://example.com/foo/bar").is_ok());
        assert!(validate_url_safety("http://8.8.8.8:8080/dns-query").is_ok());
    }

    #[test]
    fn test_html_to_markdown() {
        let html = r#"
            <html>
                <head><title>Test Page</title><style>.test{color:red;}</style></head>
                <body>
                    <h1>Main Header</h1>
                    <p>First paragraph with <a href="https://example.com">link</a> and <b>bold</b> text.</p>
                    <script>console.log("ignore me");</script>
                    <div>Second block &amp; &lt;special&gt;</div>
                </body>
            </html>
        "#;
        let md = html_to_markdown(html);
        assert!(md.contains("Main Header"));
        assert!(md.contains("First paragraph with link and bold text."));
        assert!(md.contains("Second block & <special>"));
        assert!(!md.contains("console.log"));
        assert!(!md.contains("color:red"));
    }

    #[test]
    fn test_truncate_content() {
        let text = "Line 1\nLine 2\nLine 3\nLine 4\nLine 5\n";
        let (res, truncated) = truncate_content(text, "http://example.com", Some(100));
        assert!(!truncated);
        assert_eq!(res, text);

        let long_text = "A".repeat(5000);
        let (res_trunc, truncated) = truncate_content(&long_text, "http://example.com", Some(2000));
        assert!(truncated);
        assert!(res_trunc.contains("[... middle omitted — see footer ...]"));
        assert!(res_trunc.contains("──────── [TRUNCATED] ────────"));
    }
}
