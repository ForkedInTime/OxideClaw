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

    /// Called after a tool runs with its output text. A returned note is
    /// appended to the tool result, so guidance (a loop-detector nudge)
    /// reaches the model instead of only the UI.
    async fn after_tool(&self, tool_name: &str, output: &str) -> Option<String>;

    /// True once this middleware has ended the run. Its `before_tool` then
    /// denies everything, browse_done included, so the engine must stop on
    /// its own rather than keep paying for turns that can only be denied.
    fn should_stop(&self) -> bool {
        false
    }
}

/// Ordered list of middlewares applied to every tool call.
pub type MiddlewareChain = Vec<Arc<dyn ToolMiddleware>>;
