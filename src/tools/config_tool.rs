/// ConfigTool — port of config.ts (slash command) exposed as a tool
/// Reads the current configuration and returns it to Claude.
use super::{Tool, ToolContext, ToolOutput, async_trait};
use anyhow::Result;
use serde_json::json;

pub struct ConfigTool {
    pub config: crate::config::Config,
}

#[async_trait]
impl Tool for ConfigTool {
    fn name(&self) -> &str {
        "Config"
    }

    fn description(&self) -> &str {
        "Read the current assistant configuration (model, settings, enabled features). \
        Use this to check what model is active or whether features like prompt caching, \
        extended thinking, or plan mode are enabled."
    }

    fn input_schema(&self) -> serde_json::Value {
        json!({ "type": "object", "properties": {} })
    }

    async fn execute(&self, _input: serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let cfg = &self.config;
        // The registry snapshot goes stale after `/model`; the executor
        // publishes the live choice on the context.
        let model = ctx.live_model.as_deref().unwrap_or(&cfg.model);
        let plan_mode = ctx.live_plan_mode.unwrap_or(cfg.plan_mode);
        let thinking_budget = ctx
            .live_thinking_budget
            .unwrap_or(cfg.thinking_budget_tokens);

        let mut lines = vec![
            format!("model: {model}"),
            format!("max_tokens: {}", cfg.max_tokens_for(model)),
            format!("prompt_cache: {}", cfg.prompt_cache),
            format!("plan_mode: {plan_mode}"),
        ];

        if let Some(budget) = thinking_budget {
            lines.push(format!("thinking_budget_tokens: {budget}"));
        } else {
            lines.push("thinking_budget_tokens: disabled".to_string());
        }

        if let Some(hooks) = &cfg.hooks {
            lines.push(format!(
                "hooks: pre_tool_use={}, post_tool_use={}",
                hooks.pre_tool_use.len(),
                hooks.post_tool_use.len()
            ));
        } else {
            lines.push("hooks: none".to_string());
        }

        Ok(ToolOutput::success(lines.join("\n")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `/model` changes the live provider; the tool's snapshot must not
    /// keep reporting the model from startup.
    #[tokio::test]
    async fn reports_the_live_model_over_the_startup_snapshot() {
        let cfg = crate::config::Config {
            model: "claude-sonnet-5".into(),
            ..crate::config::Config::default()
        };
        let tool = ConfigTool { config: cfg };
        let mut ctx = ToolContext::new(std::env::temp_dir());
        ctx.live_model = Some("claude-opus-5".into());
        let out = tool.execute(json!({}), &ctx).await.unwrap();
        let text = format!("{:?}", out.content);
        assert!(text.contains("model: claude-opus-5"), "{text}");
    }

    /// `/plan`, EnterPlanMode and `/reload` never reach the registry's
    /// snapshot, so the tool said `plan_mode: false` while plan mode was on.
    #[tokio::test]
    async fn reports_live_plan_mode_and_thinking_budget() {
        let cfg = crate::config::Config {
            plan_mode: false,
            thinking_budget_tokens: Some(4096),
            ..crate::config::Config::default()
        };
        let tool = ConfigTool { config: cfg };
        let mut ctx = ToolContext::new(std::env::temp_dir());
        ctx.live_plan_mode = Some(true);
        ctx.live_thinking_budget = Some(None);
        let out = tool.execute(json!({}), &ctx).await.unwrap();
        let text = format!("{:?}", out.content);
        assert!(text.contains("plan_mode: true"), "{text}");
        assert!(text.contains("thinking_budget_tokens: disabled"), "{text}");

        ctx.live_thinking_budget = Some(Some(16000));
        let out = tool.execute(json!({}), &ctx).await.unwrap();
        let text = format!("{:?}", out.content);
        assert!(text.contains("thinking_budget_tokens: 16000"), "{text}");
    }
}
