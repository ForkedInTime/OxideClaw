//! Agent Client Protocol (ACP) agent — JSON-RPC 2.0 over stdio.
//!
//! `oxideclaw acp` lets ACP-capable editors (Zed, JetBrains, and any client
//! that speaks <https://agentclientprotocol.com>) drive OxideClaw as their
//! coding agent. The agent loop is the SDK sidecar's `SdkSession`; this
//! module only translates between ACP messages and SDK notifications.
//!
//! Supported: `initialize`, `authenticate` (no-op, no auth needed),
//! `session/new` and `session/load` (with stdio and `http` MCP servers),
//! `session/prompt`, `session/cancel`, `session/update` streaming, and
//! `session/request_permission` for tools the policy marks as ask. Sessions
//! are saved after each turn in the sessions directory the TUI uses, so
//! `session/load` also opens TUI sessions. Not supported (advertised as
//! such): image/audio prompts and the deprecated `sse` MCP transport.

pub mod rpc;
pub mod server;

pub use server::AcpServer;

/// ACP protocol version this agent speaks.
pub const PROTOCOL_VERSION: u64 = 1;
