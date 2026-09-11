/// Memory tools — read and write persistent memory across sessions.
///
/// Memory is stored in ~/.claude/memory.md (global) and optionally in
/// ./.claude/memory.md (project-specific).  These files persist across
/// all oxideclaw sessions so Claude can remember things long-term.
use crate::tools::{Tool, ToolContext, ToolOutput, async_trait};
use anyhow::Result;
use std::path::PathBuf;

fn global_memory_path() -> PathBuf {
    crate::config::Config::claude_dir().join("memory.md")
}

fn project_memory_path(cwd: &std::path::Path) -> PathBuf {
    cwd.join(".claude").join("memory.md")
}

// ── MemoryRead ────────────────────────────────────────────────────────────────

pub struct MemoryReadTool;

#[async_trait]
impl Tool for MemoryReadTool {
    fn name(&self) -> &str {
        "MemoryRead"
    }

    fn description(&self) -> &str {
        "Read persistent memory. Returns the contents of the global memory file \
         (~/.claude/memory.md) and the project memory file (.claude/memory.md in cwd), \
         if they exist. Use this to recall information saved in previous sessions."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object", "properties": {} })
    }

    async fn execute(&self, _input: serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let mut parts: Vec<String> = Vec::new();

        let global = global_memory_path();
        if global.exists() {
            match std::fs::read_to_string(&global) {
                Ok(content) if !content.trim().is_empty() => {
                    parts.push(format!(
                        "## Global memory ({})\n\n{}",
                        global.display(),
                        content.trim()
                    ));
                }
                _ => {}
            }
        }

        let project = project_memory_path(&ctx.cwd);
        if project.exists() {
            match std::fs::read_to_string(&project) {
                Ok(content) if !content.trim().is_empty() => {
                    parts.push(format!(
                        "## Project memory ({})\n\n{}",
                        project.display(),
                        content.trim()
                    ));
                }
                _ => {}
            }
        }

        if parts.is_empty() {
            Ok(ToolOutput::success(
                "No memory found. Use MemoryWrite to save information.",
            ))
        } else {
            Ok(ToolOutput::success(parts.join("\n\n---\n\n")))
        }
    }
}

// ── MemoryWrite ───────────────────────────────────────────────────────────────

pub struct MemoryWriteTool;

#[async_trait]
impl Tool for MemoryWriteTool {
    fn name(&self) -> &str {
        "MemoryWrite"
    }

    fn description(&self) -> &str {
        "Write or update persistent memory. Content is appended to the memory file \
         (or replaces it if replace=true). Use `scope` to write to global \
         (~/.claude/memory.md) or project (.claude/memory.md) memory."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "content": {
                    "type": "string",
                    "description": "The content to write to memory"
                },
                "scope": {
                    "type": "string",
                    "enum": ["global", "project"],
                    "description": "Where to write the memory (default: global)"
                },
                "replace": {
                    "type": "boolean",
                    "description": "If true, replace entire memory file; if false (default), append"
                }
            },
            "required": ["content"]
        })
    }

    async fn execute(&self, input: serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let content = match input.get("content").and_then(|v| v.as_str()) {
            Some(c) => c.to_string(),
            None => return Ok(ToolOutput::error("Missing required field: content")),
        };
        let scope = input
            .get("scope")
            .and_then(|v| v.as_str())
            .unwrap_or("global");
        let replace = input
            .get("replace")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let path = if scope == "project" {
            let p = project_memory_path(&ctx.cwd);
            // Ensure parent directory exists
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent)?;
            }
            p
        } else {
            let p = global_memory_path();
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent)?;
            }
            p
        };

        if replace {
            std::fs::write(&path, &content)?;
        } else {
            // Append with a separator if file already has content
            let existing = if path.exists() {
                std::fs::read_to_string(&path).unwrap_or_default()
            } else {
                String::new()
            };
            let new_content = if existing.trim().is_empty() {
                content.clone()
            } else {
                format!("{}\n\n{}", existing.trim_end(), content)
            };
            std::fs::write(&path, new_content)?;
        }

        Ok(ToolOutput::success(format!(
            "Memory written to {} ({})",
            path.display(),
            if replace { "replaced" } else { "appended" }
        )))
    }
}
