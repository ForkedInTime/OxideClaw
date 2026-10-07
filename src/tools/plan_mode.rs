/// Plan mode tools — let Claude enter/exit read-only plan mode.
///
/// In plan mode, destructive tools (Bash, Write, Edit, MultiEdit, EnterWorktree)
/// are blocked.  Claude uses this to outline a plan before executing changes.
use crate::tools::{Tool, ToolContext, ToolOutput, async_trait};
use anyhow::Result;

pub struct EnterPlanModeTool;
pub struct ExitPlanModeTool;

#[async_trait]
impl Tool for EnterPlanModeTool {
    fn name(&self) -> &str {
        "EnterPlanMode"
    }

    fn description(&self) -> &str {
        "Enter plan mode. In plan mode you can only read files and think; all write/execute \
         tools are blocked. Use this when you want to outline a plan and get user approval \
         before making any changes."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object", "properties": {} })
    }

    async fn execute(&self, _input: serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput> {
        // Only the TUI turn loop wires a plan-mode channel and rebuilds its
        // gate from it. Headless SDK, ACP, `-p` and sub-agents have no plan
        // gate, so claiming tools are blocked there would be a false promise.
        let Some(tx) = &ctx.plan_mode_tx else {
            return Ok(ToolOutput::error(
                "Plan mode is not available in this mode (headless/ACP/print); \
                 destructive tools are NOT blocked. Describe your plan in text and \
                 wait for the user before making changes.",
            ));
        };
        let _ = tx.send(true);
        Ok(ToolOutput::success(
            "Plan mode enabled. Destructive tools are now blocked. \
             Use ExitPlanMode when you are ready to execute the plan.",
        ))
    }
}

#[async_trait]
impl Tool for ExitPlanModeTool {
    fn name(&self) -> &str {
        "ExitPlanMode"
    }

    fn description(&self) -> &str {
        "Exit plan mode and return to normal mode where all tools are available."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object", "properties": {} })
    }

    async fn execute(&self, _input: serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let Some(tx) = &ctx.plan_mode_tx else {
            return Ok(ToolOutput::success(
                "Plan mode was not active; nothing changed.",
            ));
        };
        let _ = tx.send(false);
        Ok(ToolOutput::success(
            "Plan mode disabled. All tools are now available.",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::types::ToolResultContent;

    fn text(out: &ToolOutput) -> &str {
        let ToolResultContent::Text { text } = &out.content[0];
        text
    }

    #[tokio::test]
    async fn enter_without_plan_channel_does_not_claim_tools_are_blocked() {
        let ctx = ToolContext::new(std::env::temp_dir());
        let out = EnterPlanModeTool
            .execute(serde_json::json!({}), &ctx)
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(text(&out).contains("NOT blocked"), "{}", text(&out));

        let out = ExitPlanModeTool
            .execute(serde_json::json!({}), &ctx)
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(text(&out).contains("not active"), "{}", text(&out));
    }

    #[tokio::test]
    async fn enter_and_exit_with_plan_channel_toggle_it() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut ctx = ToolContext::new(std::env::temp_dir());
        ctx.plan_mode_tx = Some(tx);
        let out = EnterPlanModeTool
            .execute(serde_json::json!({}), &ctx)
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(rx.try_recv().unwrap());
        ExitPlanModeTool
            .execute(serde_json::json!({}), &ctx)
            .await
            .unwrap();
        assert!(!rx.try_recv().unwrap());
    }
}
