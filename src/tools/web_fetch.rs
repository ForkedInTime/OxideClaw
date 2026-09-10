/// WebFetchTool — port of tools/WebFetchTool/WebFetchTool.ts
/// Fetches a URL, converts HTML to readable text, returns content.
use super::{Tool, ToolContext, ToolOutput, async_trait};
use anyhow::Result;
use serde::Deserialize;
use serde_json::json;

const MAX_CONTENT_BYTES: usize = 200_000; // ~50K tokens worth

pub struct WebFetchTool;

#[derive(Deserialize)]
struct WebFetchInput {
    url: String,
    prompt: String,
}

#[async_trait]
impl Tool for WebFetchTool {
    fn name(&self) -> &str {
        "WebFetch"
    }

    fn description(&self) -> &str {
        "Fetch content from a URL and extract relevant information. \
        Converts HTML to readable text. Provide a prompt describing what \
        information you want to extract from the page."
    }

    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "url": {
                    "type": "string",
                    "description": "The URL to fetch"
                },
                "prompt": {
                    "type": "string",
                    "description": "What information to extract from the page"
                }
            },
            "required": ["url", "prompt"]
        })
    }

    async fn execute(&self, input: serde_json::Value, _ctx: &ToolContext) -> Result<ToolOutput> {
        let input: WebFetchInput = serde_json::from_value(input)?;

        // Validate URL
        let parsed =
            url::Url::parse(&input.url).map_err(|e| anyhow::anyhow!("Invalid URL: {e}"))?;
        if parsed.scheme() != "http" && parsed.scheme() != "https" {
            return Ok(ToolOutput::error("Only http/https URLs are supported"));
        }

        // Fetch
        let client = reqwest::Client::builder()
            .user_agent(concat!(
                "Mozilla/5.0 (compatible; rustyclaw/",
                env!("CARGO_PKG_VERSION"),
                ")"
            ))
            .timeout(std::time::Duration::from_secs(30))
            .build()?;

        let response = match client.get(&input.url).send().await {
            Ok(r) => r,
            Err(e) => return Ok(ToolOutput::error(format!("Fetch failed: {e}"))),
        };

        let status = response.status();
        if !status.is_success() {
            return Ok(ToolOutput::error(format!("HTTP {status}: {}", input.url)));
        }

        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();

        let bytes = response.bytes().await?;

        // Convert to readable text
        let text = if content_type.contains("text/html") || content_type.is_empty() {
            let html = String::from_utf8_lossy(&bytes);
            html_to_text(&html)
        } else if content_type.contains("text/") || content_type.contains("json") {
            String::from_utf8_lossy(&bytes).into_owned()
        } else {
            return Ok(ToolOutput::error(format!(
                "Unsupported content type: {content_type}"
            )));
        };

        let mut text = text;
        if text.len() > MAX_CONTENT_BYTES {
            text.truncate(MAX_CONTENT_BYTES);
            text.push_str("\n... (content truncated)");
        }

        // Return the content with the prompt as context header
        let output = format!(
            "Content from: {}\nPrompt: {}\n\n---\n\n{}",
            input.url, input.prompt, text
        );

        Ok(ToolOutput::success(output))
    }
}

/// Convert HTML to plain readable text using html2text
fn html_to_text(html: &str) -> String {
    html2text::from_read(html.as_bytes(), 100)
}
