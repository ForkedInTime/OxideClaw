/// FileWriteTool — port of tools/FileWriteTool/FileWriteTool.ts
use super::{Tool, ToolContext, ToolOutput, async_trait, snapshot_file};
use crate::tools::file_read::resolve_path;
use anyhow::Result;
use serde::Deserialize;
use serde_json::json;
use tokio::fs;

pub struct FileWriteTool;

#[derive(Deserialize)]
struct FileWriteInput {
    file_path: String,
    content: String,
}

#[async_trait]
impl Tool for FileWriteTool {
    fn name(&self) -> &str {
        "Write"
    }

    fn description(&self) -> &str {
        "Write content to a file, creating it or overwriting it completely. \
        Always provide the complete file content — this tool does not append, \
        it replaces. Use Edit for partial modifications."
    }

    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "file_path": {
                    "type": "string",
                    "description": "Absolute path to the file to write"
                },
                "content": {
                    "type": "string",
                    "description": "The complete content to write to the file"
                }
            },
            "required": ["file_path", "content"]
        })
    }

    async fn execute(&self, input: serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let input: FileWriteInput = serde_json::from_value(input)?;
        let path = match resolve_path(&input.file_path, &ctx.cwd) {
            Ok(p) => p,
            Err(e) => return Ok(ToolOutput::error(e.to_string())),
        };

        if let Some(err) = super::check_protected_path(&path) {
            return Ok(err);
        }
        if let Some(err) = super::check_sensitive_path_resolved(&path, super::SensitiveOp::Write) {
            return Ok(err);
        }

        // Snapshot the original before overwriting
        snapshot_file(ctx, &path).await;

        // Create parent directories if needed
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).await.map_err(|e| {
                anyhow::anyhow!("Failed to create directories for {}: {}", path.display(), e)
            })?;
        }

        super::atomic_write(&path, &input.content)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to write {}: {}", path.display(), e))?;

        Ok(ToolOutput::success(format!(
            "File written successfully: {}",
            path.display()
        )))
    }
}
