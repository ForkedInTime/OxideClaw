//! ToolMiddleware — pluggable pre/post hook trait around every tool call.
//!
//! Default empty chain (no middlewares = existing behavior unchanged).
//! The approval gate and loop detector implement this trait.

use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;

/// Result of a middleware's `before_tool` check.
#[derive(Debug, Clone)]
pub enum MiddlewareVerdict {
    /// Allow the tool to proceed.
    Allow,
    /// Block the tool with an error reason.
    Deny { reason: String },
}

/// Extension point invoked before and after every tool execution.
#[async_trait]
pub trait ToolMiddleware: Send + Sync {
    /// Called before a tool runs. Return `Deny` to block.
    async fn before_tool(&self, tool_name: &str, input: &Value) -> MiddlewareVerdict;

    /// Called after a tool runs with its output text.
    async fn after_tool(&self, tool_name: &str, output: &str);
}

/// Ordered list of middlewares applied to every tool call.
pub type MiddlewareChain = Vec<Arc<dyn ToolMiddleware>>;
