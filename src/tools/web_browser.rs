/// WebBrowserTool — port of webBrowser.ts
/// Fetches a URL using headless Chromium (if available) or falls back to reqwest.
/// Returns rendered DOM text content, stripped of scripts and styles.
use super::{Tool, ToolContext, ToolOutput, async_trait};
use crate::net_policy::NetPolicy;
use anyhow::Result;
use serde::Deserialize;
use serde_json::json;

pub struct WebBrowserTool {
    /// Which destinations this tool may reach. See `net_policy`.
    pub policy: NetPolicy,
}

#[derive(Deserialize)]
struct Input {
    url: String,
    /// Max characters to return (default 50 000)
    #[serde(default = "default_max_chars")]
    max_chars: usize,
}

fn default_max_chars() -> usize {
    50_000
}

/// Raw response cap for the plain-HTTP fallback.
const MAX_RESPONSE_BYTES: usize = 5 * 1024 * 1024;
const FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

#[async_trait]
impl Tool for WebBrowserTool {
    fn name(&self) -> &str {
        "WebBrowser"
    }

    fn description(&self) -> &str {
        "Open a URL in a headless browser and return the rendered page text. \
        Uses Chromium if installed; falls back to a plain HTTP fetch. \
        Better than WebFetch for JavaScript-heavy pages."
    }

    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "url": {
                    "type": "string",
                    "description": "The URL to open"
                },
                "max_chars": {
                    "type": "integer",
                    "description": "Maximum characters of content to return (default 50 000)",
                    "default": 50000,
                    "minimum": 1000,
                    "maximum": 200000
                }
            },
            "required": ["url"]
        })
    }

    async fn execute(&self, input: serde_json::Value, _ctx: &ToolContext) -> Result<ToolOutput> {
        let input: Input = serde_json::from_value(input)?;
        let max_chars = input.max_chars;

        // Parse + policy check *before* anything touches the URL: chromium
        // would print `file:///etc/passwd` on request, and a flag-shaped
        // string would be parsed as a switch.
        let url = match url::Url::parse(&input.url) {
            Ok(u) => u,
            Err(e) => return Ok(ToolOutput::error(format!("Invalid URL: {e}"))),
        };
        if let Err(e) = self.policy.resolve(&url).await {
            return Ok(ToolOutput::error(format!("WebBrowser refused: {e}")));
        }

        // Try headless Chromium first. Residual: chromium follows redirects
        // itself, so only the initial URL is policy-checked on this path.
        if let Some(text) = try_chromium(url.as_str(), max_chars).await {
            return Ok(ToolOutput::success(text));
        }

        // Fallback: guarded plain fetch (every hop checked, body capped).
        match fetch_plain(url.as_str(), &self.policy, max_chars).await {
            Ok(text) => Ok(ToolOutput::success(text)),
            Err(e) => Ok(ToolOutput::error(format!("WebBrowser fetch failed: {e}"))),
        }
    }
}

/// Try to fetch via `chromium --headless --dump-dom`.
async fn try_chromium(url: &str, max_chars: usize) -> Option<String> {
    use tokio::process::Command;
    use tokio::time::{Duration, timeout};

    // Try several common chromium executable names
    for exe in &[
        "chromium",
        "chromium-browser",
        "google-chrome",
        "google-chrome-stable",
    ] {
        let result = timeout(
            Duration::from_secs(20),
            Command::new(exe)
                .args(["--headless", "--disable-gpu", "--dump-dom", url])
                // A timeout drops this future; without this the browser
                // would outlive the tool call as an orphan.
                .kill_on_drop(true)
                .output(),
        )
        .await;

        match result {
            Ok(Ok(output)) if output.status.success() => {
                let raw = String::from_utf8_lossy(&output.stdout);
                let stripped = strip_html(raw.as_ref(), max_chars);
                if !stripped.trim().is_empty() {
                    return Some(stripped);
                }
            }
            _ => continue,
        }
    }
    None
}

/// Plain HTTP fetch as fallback.
async fn fetch_plain(url: &str, policy: &NetPolicy, max_chars: usize) -> Result<String> {
    let fetched = crate::net_policy::fetch(url, policy, MAX_RESPONSE_BYTES, FETCH_TIMEOUT).await?;
    if !fetched.status.is_success() {
        anyhow::bail!("HTTP {}", fetched.status);
    }
    Ok(strip_html(
        &String::from_utf8_lossy(&fetched.body),
        max_chars,
    ))
}

/// Very lightweight HTML → plain-text extractor.
fn strip_html(html: &str, max_chars: usize) -> String {
    let mut out = String::with_capacity(html.len().min(max_chars + 1024));
    let mut in_tag = false;
    let mut in_script = false;
    let mut in_style = false;
    let mut tag_buf = String::new();
    let mut prev_space = false;

    for c in html.chars() {
        if in_tag {
            tag_buf.push(c);
            if c == '>' {
                in_tag = false;
                let tag_lower = tag_buf.to_lowercase();
                if tag_lower.contains("<script") {
                    in_script = true;
                } else if tag_lower.contains("</script") {
                    in_script = false;
                } else if tag_lower.contains("<style") {
                    in_style = true;
                } else if tag_lower.contains("</style") {
                    in_style = false;
                }
                // Add space after block-level tags
                let is_block = [
                    "</p", "<br", "</div", "</h1", "</h2", "</h3", "</h4", "</h5", "</h6", "<li",
                    "</li",
                ]
                .iter()
                .any(|t| tag_lower.starts_with(t));
                if is_block && !prev_space {
                    out.push('\n');
                    prev_space = true;
                }
                tag_buf.clear();
            }
        } else if c == '<' {
            in_tag = true;
            tag_buf.push(c);
        } else if !in_script && !in_style {
            if c.is_whitespace() {
                if !prev_space {
                    out.push(' ');
                    prev_space = true;
                }
            } else {
                out.push(c);
                prev_space = false;
            }
        }

        if out.len() >= max_chars {
            out.push_str("\n[content truncated]");
            break;
        }
    }

    // Decode common HTML entities
    out.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::types::ToolResultContent;
    use crate::net_policy::test_support::{ok_with, scripted_server};
    use std::sync::atomic::Ordering;

    async fn run(policy: NetPolicy, url: &str) -> ToolOutput {
        let tool = WebBrowserTool { policy };
        let ctx = ToolContext::new(std::env::temp_dir());
        tool.execute(json!({"url": url}), &ctx)
            .await
            .expect("refusals are tool errors, not Err")
    }

    fn text(o: &ToolOutput) -> String {
        o.content
            .iter()
            .map(|c| match c {
                ToolResultContent::Text { text } => text.as_str(),
            })
            .collect()
    }

    /// `chromium --dump-dom file:///etc/passwd` would happily print the
    /// file. The URL must be refused before any process is spawned.
    #[tokio::test]
    async fn file_url_is_refused() {
        let out = run(NetPolicy::LOCAL_OK, "file:///etc/passwd").await;
        assert!(out.is_error);
        assert!(text(&out).contains("http/https"), "{}", text(&out));
    }

    /// A flag-shaped "URL" would be parsed by chromium as a switch.
    #[tokio::test]
    async fn flag_shaped_url_is_refused() {
        let out = run(NetPolicy::LOCAL_OK, "--remote-debugging-port=9222").await;
        assert!(out.is_error);
        assert!(
            text(&out).to_lowercase().contains("invalid url"),
            "{}",
            text(&out)
        );
    }

    #[tokio::test]
    async fn strict_policy_refuses_loopback_without_connecting() {
        let (base, hits) = scripted_server(vec![ok_with("text/html", "<p>secret</p>")]).await;
        let out = run(NetPolicy::STRICT, &base).await;
        assert!(out.is_error);
        assert!(text(&out).contains("private"), "{}", text(&out));
        assert_eq!(hits.load(Ordering::SeqCst), 0);
    }

    /// The plain-HTTP fallback goes through the same guarded fetch.
    #[tokio::test]
    async fn fallback_fetch_is_capped() {
        let resp = "HTTP/1.1 200 OK\r\ncontent-type: text/html\r\n\
                    content-length: 99999999\r\nconnection: close\r\n\r\nx";
        let (base, _) = scripted_server(vec![resp.to_string()]).await;
        let err = fetch_plain(&base, &NetPolicy::LOCAL_OK, 1000)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("too large"), "{err}");
    }

    #[tokio::test]
    async fn fallback_fetch_strips_html() {
        let (base, _) = scripted_server(vec![ok_with(
            "text/html",
            "<h1>Hi</h1><script>x()</script>",
        )])
        .await;
        let got = fetch_plain(&base, &NetPolicy::LOCAL_OK, 1000)
            .await
            .unwrap();
        assert!(got.contains("Hi"), "{got}");
        assert!(!got.contains("x()"), "{got}");
    }

    #[test]
    fn strip_html_does_not_scan_past_the_cap() {
        let html = format!("<p>{}</p>", "a".repeat(100_000));
        let out = strip_html(&html, 10);
        assert!(out.len() < 100, "{}", out.len());
        assert!(out.contains("[content truncated]"));
    }
}
