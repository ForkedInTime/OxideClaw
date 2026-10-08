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
pub mod scope;
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

/// The log file `main` opened: `$TMP/oxideclaw.log`, or the per-user
/// fallback when another user owns that. Unset when neither opened.
pub static LOG_PATH: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();

/// What the TUI, `/mcp` and `/doctor` say about servers that failed to
/// start. The reason is only in the log: the TUI owns the terminal.
pub fn failed_notice(names: &[String]) -> String {
    let log = LOG_PATH
        .get()
        .map(|p| format!("The reason is in {}; ", p.display()))
        .unwrap_or_default();
    format!(
        "MCP server{} failed to start, so {} tools are unavailable: {}. {}`oxideclaw mcp \
         list` checks them again.",
        if names.len() == 1 { "" } else { "s" },
        if names.len() == 1 { "its" } else { "their" },
        names.join(", "),
        log,
    )
}

// ── Helper: build DynTool list from an McpManager ────────────────────────────

/// Built-in tools plus every configured MCP server's tools, for the entry
/// points without a TUI (print mode, `--headless` SDK, ACP). Those modes have
/// no /mcp panel, so a server that fails to start is reported on stderr.
/// The CLI tool filters are already applied.
pub async fn tools_for_config(cfg: &crate::config::Config) -> Vec<DynTool> {
    let manager = McpManager::start_for_config(cfg).await;
    for name in &manager.failed {
        eprintln!("Warning: MCP server '{name}' failed to start; its tools are unavailable.");
    }
    let mcp_tools = mcp_dyn_tools(&manager);
    // The tools hold their own `Arc<McpClient>`, so the servers outlive
    // `manager` for as long as the session keeps its tools.
    let mut tools =
        crate::tools::all_tools_with_state_and_mcp(cfg, mcp_tools, manager.clients.clone()).0;
    crate::tools::apply_tool_filters(&mut tools, cfg);
    tools
}

/// Convert all connected MCP servers' tools into `Arc<dyn Tool>` entries.
pub fn mcp_dyn_tools(manager: &McpManager) -> Vec<DynTool> {
    let mut tools: Vec<DynTool> = Vec::new();
    let mut used = std::collections::HashSet::new();

    for client in &manager.clients {
        let server = sanitize_name(&client.server_name);

        for tool_def in &client.tools {
            let original = tool_def.name.clone();
            let composed = fit_tool_name(
                format!("mcp__{}__{}", server, sanitize_name(&original)),
                &mut used,
            );

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

/// ASCII only: the API's tool-name pattern is `^[a-zA-Z0-9_-]{1,64}$`.
fn sanitize_name(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// The API rejects the *whole request* over one tool name longer than 64
/// characters or a duplicate name, so every MCP tool must fit and be unique.
/// Long or colliding names keep a readable prefix plus a hash of the original.
fn fit_tool_name(name: String, used: &mut std::collections::HashSet<String>) -> String {
    use sha2::{Digest, Sha256};
    const MAX: usize = 64;
    if name.len() <= MAX && !used.contains(&name) {
        used.insert(name.clone());
        return name;
    }
    // Every collision on the same sanitized name hashes the same input, so
    // the third one would repeat the second's suffix: salt until it is free.
    // Attempt 0 is unsalted so existing names stay put.
    let keep = name.len().min(MAX - 8 - 1);
    let fitted = (0u32..)
        .map(|n| {
            let digest = if n == 0 {
                Sha256::digest(name.as_bytes())
            } else {
                Sha256::digest(format!("{name}#{n}").as_bytes())
            };
            let hash: String = digest.iter().take(4).map(|b| format!("{b:02x}")).collect();
            format!("{}_{hash}", &name[..keep])
        })
        .find(|candidate| !used.contains(candidate))
        .expect("an unused suffix exists");
    used.insert(fitted.clone());
    fitted
}

#[cfg(test)]
mod description_tests {
    /// The notice named `$TMP/oxideclaw.log` even when the log went to the
    /// per-user fallback or nowhere. Only the file `main` opened is named.
    #[test]
    fn the_failure_notice_names_only_the_log_that_was_opened() {
        let names = vec!["db".to_string()];
        let notice = super::failed_notice(&names);
        assert!(notice.contains("failed to start, so its tools"), "{notice}");
        match super::LOG_PATH.get() {
            Some(p) => assert!(notice.contains(&p.display().to_string()), "{notice}"),
            None => assert!(!notice.contains("oxideclaw.log"), "{notice}"),
        }
        assert!(notice.ends_with("`oxideclaw mcp list` checks them again."));
    }

    #[test]
    fn tool_names_fit_the_api_pattern_and_stay_unique() {
        let mut used = std::collections::HashSet::new();
        let long = format!("mcp__srv__{}", "x".repeat(80));
        let a = super::fit_tool_name(long.clone(), &mut used);
        assert!(a.len() <= 64, "{a}");
        let b = super::fit_tool_name("mcp__s__t".into(), &mut used);
        let c = super::fit_tool_name("mcp__s__t".into(), &mut used);
        assert_eq!(b, "mcp__s__t");
        assert_ne!(b, c, "duplicates get a distinct name");
        assert_eq!(super::sanitize_name("café.list"), "caf__list");
    }

    /// `get.user`, `get/user` and `get user` all sanitize to `get_user`; the
    /// second and third got the same hashed name and every request was a 400.
    #[test]
    fn any_number_of_colliding_names_stay_unique() {
        let mut used = std::collections::HashSet::new();
        let long = format!("mcp__srv__{}", "z".repeat(80));
        let mut names = Vec::new();
        for _ in 0..5 {
            names.push(super::fit_tool_name("mcp__s__get_user".into(), &mut used));
            names.push(super::fit_tool_name(long.clone(), &mut used));
        }
        let unique: std::collections::HashSet<_> = names.iter().collect();
        assert_eq!(unique.len(), names.len(), "{names:?}");
        assert!(names.iter().all(|n| n.len() <= 64), "{names:?}");
    }

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
