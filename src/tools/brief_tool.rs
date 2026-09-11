/// BriefTool (SendUserMessage) — primary output channel for formatted responses.
///
/// In oxideclaw (TS) this is gated behind the KAIROS build-time feature flag,
/// making it dead code in external builds. In Rust there is no build-time DCE
/// system, so the tool is always available.
use crate::tools::{Tool, ToolContext, ToolOutput};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::{Value, json};

pub struct BriefTool;

#[async_trait]
impl Tool for BriefTool {
    fn name(&self) -> &str {
        "SendUserMessage"
    }

    fn description(&self) -> &str {
        "Send a message to the user — your primary visible output channel. \
         Use this to deliver formatted responses, status updates, and proactive \
         notifications. Supports markdown formatting and optional file attachments."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["message", "status"],
            "properties": {
                "message": {
                    "type": "string",
                    "description": "The message for the user. Supports markdown formatting."
                },
                "attachments": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Optional file paths (absolute or relative to cwd) to attach. Use for photos, screenshots, diffs, logs, or any file the user should see alongside your message."
                },
                "status": {
                    "type": "string",
                    "enum": ["normal", "proactive"],
                    "description": "Use 'proactive' when surfacing something the user hasn't asked for (task completion, blockers, unsolicited updates). Use 'normal' when replying to something the user just said."
                }
            }
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let message = input["message"].as_str().unwrap_or("").to_string();
        let status = input["status"].as_str().unwrap_or("normal");
        let attachments = input["attachments"].as_array();

        if message.is_empty() {
            return Ok(ToolOutput::error("message must not be empty"));
        }

        // Process attachments if present
        let mut attachment_results: Vec<String> = Vec::new();
        if let Some(paths) = attachments {
            for path_val in paths {
                let path_str = match path_val.as_str() {
                    Some(s) => s,
                    None => continue,
                };
                let path = if std::path::Path::new(path_str).is_absolute() {
                    std::path::PathBuf::from(path_str)
                } else {
                    ctx.cwd.join(path_str)
                };

                if !path.exists() {
                    attachment_results.push(format!("  [not found: {}]", path_str));
                    continue;
                }

                let size = path.metadata().map(|m| m.len()).unwrap_or(0);
                let ext = path
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("")
                    .to_lowercase();

                let is_image = matches!(
                    ext.as_str(),
                    "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "svg"
                );

                if is_image {
                    attachment_results.push(format!("  [image: {} ({} bytes)]", path_str, size));
                } else {
                    // For text files include a snippet
                    let snippet = std::fs::read_to_string(&path)
                        .ok()
                        .map(|s| {
                            let lines: Vec<&str> = s.lines().take(3).collect();
                            lines.join("\n")
                        })
                        .unwrap_or_else(|| format!("{} bytes", size));
                    attachment_results.push(format!("  [file: {} — {}]", path_str, snippet));
                }
            }
        }

        let n = attachment_results.len();
        let suffix = if n == 0 {
            String::new()
        } else {
            format!(
                " ({} attachment{} included)",
                n,
                if n == 1 { "" } else { "s" }
            )
        };

        // The tool result sent back to the API is the TS equivalent of
        // mapToolResultToToolResultBlockParam: just a delivery confirmation.
        // The actual message display is handled by the TUI via ToolOutputStream.
        let _ = status; // used for routing in Kairos mode; in Rust we just deliver

        // Stream the message content so it's visible in the TUI
        ctx.stream_line(&message);
        for att in &attachment_results {
            ctx.stream_line(att);
        }

        Ok(ToolOutput::success(format!(
            "Message delivered to user.{suffix}"
        )))
    }
}
