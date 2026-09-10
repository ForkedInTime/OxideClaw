/// WebBrowserTool — port of webBrowser.ts
/// Fetches a URL using headless Chromium (if available) or falls back to reqwest.
/// Returns rendered DOM text content, stripped of scripts and styles.
use super::{Tool, ToolContext, ToolOutput, async_trait};
use anyhow::Result;
use serde::Deserialize;
use serde_json::json;

pub struct WebBrowserTool;

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

        // Try headless Chromium first
        if let Some(text) = try_chromium(&input.url, max_chars).await {
            return Ok(ToolOutput::success(text));
        }

        // Fallback: plain reqwest fetch
        match fetch_plain(&input.url, max_chars).await {
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
async fn fetch_plain(url: &str, max_chars: usize) -> Result<String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .user_agent(concat!(
            "Mozilla/5.0 (compatible; rustyclaw/",
            env!("CARGO_PKG_VERSION"),
            ")"
        ))
        .build()?;

    let resp = client.get(url).send().await?.text().await?;
    Ok(strip_html(&resp, max_chars))
}

/// Very lightweight HTML → plain-text extractor.
fn strip_html(html: &str, max_chars: usize) -> String {
    let mut out = String::with_capacity(html.len().min(max_chars + 1024));
    let mut in_tag = false;
    let mut in_script = false;
    let mut in_style = false;
    let mut tag_buf = String::new();
    let mut prev_space = false;

    let chars: Vec<char> = html.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        let c = chars[i];

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
        i += 1;
    }

    // Decode common HTML entities
    out.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
}
