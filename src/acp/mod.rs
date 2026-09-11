//! Agent Client Protocol (ACP) agent — JSON-RPC 2.0 over stdio.
//!
//! `rustyclaw acp` lets ACP-capable editors (Zed, JetBrains, and any client
//! that speaks <https://agentclientprotocol.com>) drive RustyClaw as their
//! coding agent. The agent loop is the SDK sidecar's `SdkSession`; this
//! module only translates between ACP messages and SDK notifications.
//!
//! Supported: `initialize`, `authenticate` (no-op, no auth needed),
//! `session/new` (with stdio MCP servers), `session/prompt`, `session/cancel`,
//! `session/update` streaming, and `session/request_permission` for tools
//! the policy marks as ask. Not supported (advertised as such):
//! `session/load`, image/audio prompts, HTTP/SSE MCP transports.

pub mod rpc;
pub mod server;

pub use server::AcpServer;

/// ACP protocol version this agent speaks.
pub const PROTOCOL_VERSION: u64 = 1;
