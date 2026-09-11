/// MCP (Model Context Protocol) integration.
///
/// Provides:
///   - McpManager  — connects to all servers from settings.json at startup
///   - McpDynamicTool — wraps an MCP tool as a built-in Tool (`Arc<dyn Tool>`)
///
/// Tool names use the format `mcp__<server>__<tool>` to avoid conflicts with
/// built-in tools and to let Claude identify which server provides each tool.
pub mod client;
pub mod manager;
pub mod types;

pub use client::McpClient;
pub use manager::McpManager;
// McpServerStatus is imported directly by consumers from types::

use crate::api::types::ToolDefinition;
use crate::tools::{DynTool, Tool, ToolContext, ToolOutput, async_trait};
use anyhow::Result;
use std::sync::Arc;

// ── McpDynamicTool ────────────────────────────────────────────────────────────

/// Wraps an MCP tool definition + client so it looks identical to a built-in
/// Tool.  The tool name sent to the API is `mcp__<server>__<original_name>`.
pub struct McpDynamicTool {
    /// Composed name: `mcp__<server>__<original>`
    pub composed_name: String,
    /// Original tool name as defined by the MCP server (used in tools/call)
    pub original_name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
    pub client: Arc<McpClient>,
}

#[async_trait]
impl Tool for McpDynamicTool {
    fn name(&self) -> &str {
        &self.composed_name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn input_schema(&self) -> serde_json::Value {
        self.input_schema.clone()
    }

    async fn execute(&self, input: serde_json::Value, _ctx: &ToolContext) -> Result<ToolOutput> {
        match self.client.call_tool(&self.original_name, input).await {
            Ok(text) => Ok(ToolOutput::success(text)),
            Err(e) => Ok(ToolOutput::error(e.to_string())),
        }
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.composed_name.clone(),
            description: self.description.clone(),
            input_schema: self.input_schema.clone(),
            cache_control: None,
        }
    }
}

// ── Helper: build DynTool list from an McpManager ────────────────────────────

/// Convert all connected MCP servers' tools into `Arc<dyn Tool>` entries.
pub fn mcp_dyn_tools(manager: &McpManager) -> Vec<DynTool> {
    let mut tools: Vec<DynTool> = Vec::new();

    for client in &manager.clients {
        let server = sanitize_name(&client.server_name);

        for tool_def in &client.tools {
            let original = tool_def.name.clone();
            let composed = format!("mcp__{}__{}", server, sanitize_name(&original));

            let description = describe(&client.server_name, &original, &tool_def.description);

            tools.push(Arc::new(McpDynamicTool {
                composed_name: composed,
                original_name: original,
                description,
                input_schema: tool_def.input_schema.clone(),
                client: Arc::clone(client),
            }));
        }
    }

    tools
}

/// Replace non-alphanumeric characters with underscores for safe tool names.
/// Tool descriptions come from the server and go straight into the model's
/// prompt — attacker-controlled text across a trust boundary. The prefix
/// keeps provenance visible and the cap bounds the payload.
pub(crate) const MAX_DESCRIPTION_CHARS: usize = 2_000;

fn describe(server: &str, original: &str, description: &str) -> String {
    let body = if description.is_empty() {
        original
    } else {
        description
    };
    let mut d = format!("[MCP: {server}] ");
    if body.chars().count() > MAX_DESCRIPTION_CHARS {
        d.extend(body.chars().take(MAX_DESCRIPTION_CHARS));
        d.push_str(" [description truncated…]");
    } else {
        d.push_str(body);
    }
    d
}

fn sanitize_name(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod description_tests {
    use super::*;

    #[test]
    fn descriptions_are_prefixed_with_the_server_and_capped() {
        let d = describe("srv", "tool", "does things");
        assert_eq!(d, "[MCP: srv] does things");
        assert_eq!(describe("srv", "tool", ""), "[MCP: srv] tool");

        let long = "y".repeat(50_000);
        let d = describe("srv", "tool", &long);
        assert!(
            d.chars().count() <= MAX_DESCRIPTION_CHARS + 64,
            "{}",
            d.len()
        );
        assert!(
            d.ends_with("…]"),
            "must mark the cut: {}",
            &d[d.len() - 20..]
        );
    }
}
