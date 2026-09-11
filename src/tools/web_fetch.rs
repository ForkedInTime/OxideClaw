/// WebFetchTool — port of tools/WebFetchTool/WebFetchTool.ts
/// Fetches a URL, converts HTML to readable text, returns content.
use super::{Tool, ToolContext, ToolOutput, async_trait};
use crate::net_policy::NetPolicy;
use anyhow::Result;
use serde::Deserialize;
use serde_json::json;

const MAX_CONTENT_BYTES: usize = 200_000; // ~50K tokens worth
/// Raw response cap. HTML-to-text is ~10:1, so this comfortably covers
/// `MAX_CONTENT_BYTES` of output without letting a hostile server stream
/// gigabytes into memory.
const MAX_RESPONSE_BYTES: usize = 5 * 1024 * 1024;
const FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

pub struct WebFetchTool {
    /// Which destinations this tool may reach. See `net_policy`.
    pub policy: NetPolicy,
}

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

        // Scheme, host, DNS and redirect hops are all checked by the
        // policy; the body is refused past MAX_RESPONSE_BYTES.
        let fetched = match crate::net_policy::fetch(
            &input.url,
            &self.policy,
            MAX_RESPONSE_BYTES,
            FETCH_TIMEOUT,
        )
        .await
        {
            Ok(f) => f,
            Err(e) => return Ok(ToolOutput::error(format!("Fetch failed: {e}"))),
        };

        let status = fetched.status;
        if !status.is_success() {
            return Ok(ToolOutput::error(format!("HTTP {status}: {}", input.url)));
        }
        let content_type = fetched.content_type;
        let bytes = fetched.body;
        // Report where the content actually came from (after redirects).
        let final_url = fetched.final_url;

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
            "Content from: {final_url}\nPrompt: {}\n\n---\n\n{}",
            input.prompt, text
        );

        Ok(ToolOutput::success(output))
    }
}

/// Convert HTML to plain readable text using html2text
fn html_to_text(html: &str) -> String {
    html2text::from_read(html.as_bytes(), 100)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::types::ToolResultContent;
    use crate::net_policy::test_support::{ok_with, scripted_server};
    use std::sync::atomic::Ordering;

    async fn run(policy: NetPolicy, url: &str) -> ToolOutput {
        let tool = WebFetchTool { policy };
        let ctx = ToolContext::new(std::env::temp_dir());
        tool.execute(json!({"url": url, "prompt": "x"}), &ctx)
            .await
            .expect("policy refusals are tool errors, not Err")
    }

    fn text(o: &ToolOutput) -> String {
        o.content
            .iter()
            .map(|c| match c {
                ToolResultContent::Text { text } => text.as_str(),
            })
            .collect()
    }

    /// The default policy must refuse loopback *before connecting*, and
    /// report it as a tool error the model can read.
    #[tokio::test]
    async fn strict_policy_refuses_loopback_without_connecting() {
        let (base, hits) = scripted_server(vec![ok_with("text/html", "<p>secret</p>")]).await;
        let out = run(NetPolicy::STRICT, &base).await;
        assert!(out.is_error);
        assert!(text(&out).contains("private"), "{}", text(&out));
        assert_eq!(hits.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn local_ok_policy_fetches_and_converts_html() {
        let (base, _) =
            scripted_server(vec![ok_with("text/html", "<p>Hello <b>there</b></p>")]).await;
        let out = run(NetPolicy::LOCAL_OK, &base).await;
        assert!(!out.is_error, "{}", text(&out));
        assert!(text(&out).contains("Hello there"), "{}", text(&out));
    }

    #[tokio::test]
    async fn oversized_response_is_refused_as_a_tool_error() {
        let resp = "HTTP/1.1 200 OK\r\ncontent-type: text/html\r\n\
                    content-length: 99999999\r\nconnection: close\r\n\r\nx";
        let (base, _) = scripted_server(vec![resp.to_string()]).await;
        let out = run(NetPolicy::LOCAL_OK, &base).await;
        assert!(out.is_error);
        assert!(text(&out).contains("too large"), "{}", text(&out));
    }
}
