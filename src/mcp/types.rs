/// MCP protocol types — JSON-RPC 2.0 + Model Context Protocol.
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// ── Server configuration (from settings.json mcpServers) ─────────────────────

/// One entry in the mcpServers map.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum McpServerConfig {
    Stdio(StdioServerConfig),
    Http(HttpServerConfig),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StdioServerConfig {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// Set by `/mcp disable` and `/plugin disable`; the server is not started.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub disabled: bool,
    /// Values arrived resolved (an ACP host's `mcpServers`), so `${VAR}`
    /// placeholders are not expanded against this process's environment.
    /// Never read from or written to a config file.
    #[serde(skip)]
    pub literal: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpServerConfig {
    pub url: String,
    #[serde(default)]
    pub headers: HashMap<String, String>,
    /// Set by `/mcp disable` and `/plugin disable`; the server is not started.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub disabled: bool,
    /// Speak MCP's legacy HTTP+SSE transport instead of Streamable HTTP.
    /// Only an ACP host's `type: "sse"` servers set it.
    #[serde(skip)]
    pub sse: bool,
    /// Values arrived resolved (an ACP host's `mcpServers`), so `${VAR}`
    /// placeholders are not expanded against this process's environment.
    /// Never read from or written to a config file.
    #[serde(skip)]
    pub literal: bool,
}

impl McpServerConfig {
    pub fn is_disabled(&self) -> bool {
        match self {
            Self::Stdio(s) => s.disabled,
            Self::Http(h) => h.disabled,
        }
    }
}

// ── JSON-RPC 2.0 ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<u64>,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
}

impl JsonRpcRequest {
    pub fn new(id: u64, method: impl Into<String>, params: serde_json::Value) -> Self {
        Self {
            jsonrpc: "2.0",
            id: Some(id),
            method: method.into(),
            params: Some(params),
        }
    }

    pub fn notification(method: impl Into<String>) -> Self {
        Self {
            jsonrpc: "2.0",
            id: None,
            method: method.into(),
            params: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)] // fields populated by JSON-RPC deserialization
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    pub id: Option<serde_json::Value>,
    pub result: Option<serde_json::Value>,
    pub error: Option<JsonRpcError>,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)] // fields populated by JSON-RPC deserialization
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
    pub data: Option<serde_json::Value>,
}

// ── MCP tool definition (from tools/list) ────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpToolDef {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(rename = "inputSchema", default = "default_schema")]
    pub input_schema: serde_json::Value,
    /// Server-provided metadata — may contain "anthropic/maxResultSizeChars"
    #[serde(rename = "_meta", default)]
    pub meta: Option<serde_json::Value>,
}

impl McpToolDef {
    /// Returns the server-declared max result size in chars, if any.
    /// Falls back to a conservative default of 25_000.
    pub fn max_result_chars(&self) -> usize {
        self.meta
            .as_ref()
            .and_then(|m| m.get("anthropic/maxResultSizeChars"))
            .and_then(|v| v.as_u64())
            .map(|n| n as usize)
            .unwrap_or(25_000)
    }
}

fn default_schema() -> serde_json::Value {
    serde_json::json!({ "type": "object", "properties": {} })
}

// ── Tool call result ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct McpCallResult {
    /// Raw content items (text, resource, resource_link, image, audio, ...);
    /// kept as JSON so one odd item cannot fail the whole result.
    #[serde(default)]
    pub content: Vec<serde_json::Value>,
    #[serde(rename = "isError", default)]
    pub is_error: bool,
}

// ── MCP resource (from resources/list) ───────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpResource {
    pub uri: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(rename = "mimeType", default)]
    pub mime_type: String,
}

// ── Status (for /mcp command) ─────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct McpServerStatus {
    pub name: String,
    pub transport: &'static str, // "stdio" | "http" | "sse"
    /// Negotiated MCP revision, e.g. "2026-07-28" or "2024-11-05".
    pub protocol: String,
    pub tool_count: usize,
}
