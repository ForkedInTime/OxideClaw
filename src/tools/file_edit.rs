/// FileEditTool — port of tools/FileEditTool/FileEditTool.ts
/// Performs exact string replacement in a file (old_string → new_string).
use super::{Tool, ToolContext, ToolOutput, async_trait, snapshot_file};
use crate::tools::file_read::resolve_path;
use anyhow::Result;
use serde::Deserialize;
use serde_json::json;
use tokio::fs;

pub struct FileEditTool;

#[derive(Deserialize)]
struct FileEditInput {
    file_path: String,
    old_string: String,
    new_string: String,
    #[serde(default)]
    replace_all: bool,
}

#[async_trait]
impl Tool for FileEditTool {
    fn name(&self) -> &str {
        "Edit"
    }

    fn description(&self) -> &str {
        "Edit a file by replacing an exact string occurrence with a new string. \
        The old_string must appear exactly once (or use replace_all:true to replace \
        all occurrences). Use the shortest old_string that is still unique — minimal \
        context saves output tokens. Preserves indentation and formatting exactly."
    }

    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "file_path": {
                    "type": "string",
                    "description": "Absolute path to the file to edit"
                },
                "old_string": {
                    "type": "string",
                    "description": "The exact string to replace (must be unique in the file)"
                },
                "new_string": {
                    "type": "string",
                    "description": "The string to replace it with"
                },
                "replace_all": {
                    "type": "boolean",
                    "description": "If true, replace all occurrences instead of requiring uniqueness"
                }
            },
            "required": ["file_path", "old_string", "new_string"]
        })
    }

    async fn execute(&self, input: serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let input: FileEditInput = serde_json::from_value(input)?;
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

        // Snapshot the original before editing
        snapshot_file(ctx, &path).await;

        if !path.exists() {
            return Ok(ToolOutput::error(format!(
                "File not found: {}",
                path.display()
            )));
        }

        let content = fs::read_to_string(&path)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to read {}: {}", path.display(), e))?;

        if input.replace_all {
            let new_content = content.replace(&input.old_string, &input.new_string);
            let count = content.matches(&input.old_string as &str).count();
            if count == 0 {
                return Ok(ToolOutput::error(format!(
                    "old_string not found in {}",
                    path.display()
                )));
            }
            super::atomic_write(&path, &new_content).await?;
            return Ok(ToolOutput::success(format!(
                "Replaced {count} occurrence(s) in {}",
                path.display()
            )));
        }

        // Require exactly one occurrence
        let count = content.matches(&input.old_string as &str).count();
        match count {
            0 => Ok(ToolOutput::error(format!(
                "old_string not found in {}",
                path.display()
            ))),
            1 => {
                let new_content = content.replacen(&input.old_string, &input.new_string, 1);
                super::atomic_write(&path, &new_content).await?;
                Ok(ToolOutput::success(format!(
                    "Edit applied successfully to {}",
                    path.display()
                )))
            }
            n => Ok(ToolOutput::error(format!(
                "old_string found {n} times in {} — provide more context to make it unique, \
                or use replace_all:true",
                path.display()
            ))),
        }
    }
}
