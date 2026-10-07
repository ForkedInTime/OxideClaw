/// AgentTool — port of tools/AgentTool/AgentTool.ts
/// Spawns a sub-agent (nested QueryEngine) to handle a focused subtask.
/// The sub-agent runs with its own isolated conversation history.
use super::{DynTool, Tool, ToolContext, ToolOutput, async_trait, default_tools};
use crate::config::Config;
use crate::query_engine::QueryEngine;
use anyhow::Result;
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

/// How many `Agent` launches may nest. The session is depth 0; a child it
/// launches runs at depth 1 and may launch grandchildren (depth 2), which
/// may not launch further. Before this cap the recursion was unbounded —
/// every level a full engine billing tokens.
pub const MAX_AGENT_DEPTH: u8 = 2;

pub struct AgentTool {
    pub config: Config,
}

impl AgentTool {
    /// The child engine inherits the executor's permission gate (so its
    /// Bash/Write/Edit prompt the same human, or fail closed the same way)
    /// and sits one level deeper in the launch chain.
    fn build_sub_engine(
        &self,
        sub_config: Config,
        tools: Vec<DynTool>,
        ctx: &ToolContext,
    ) -> Result<QueryEngine> {
        let mut engine = QueryEngine::new(sub_config, tools)?
            .with_agent_depth(ctx.agent_depth + 1)
            .with_usage_sink(ctx.usage_sink.clone());
        if let Some(gate) = &ctx.permission_gate {
            engine = engine.with_permission_gate(gate.clone());
        }
        Ok(engine)
    }

    /// The child's config: our build-time snapshot refreshed with the live
    /// values the executor publishes in `ctx` each turn.
    fn live_config(&self, ctx: &ToolContext) -> Config {
        // Our own `self.config` is a snapshot taken at tool-build time and
        // goes stale the moment the user runs `/model foo` mid-session. The
        // run loop publishes the live provider choice through `ToolContext`
        // each turn — prefer it so sub-agents actually run against the
        // currently-active provider instead of silently falling back to the
        // startup model. (Known regression in multiple competing tools.)
        let mut sub_config = self.config.clone();
        if let Some(ref m) = ctx.live_model {
            sub_config.model = m.clone();
        }
        if let Some(ref k) = ctx.live_api_key {
            sub_config.api_key = k.clone();
        }
        if let Some(ref h) = ctx.live_ollama_host {
            sub_config.ollama_host = h.clone();
        }
        // Same staleness for the sandbox: `/sandbox enable` or `/reload`
        // changes only the live config, and every executor publishes it in
        // `ctx`. Trusting the snapshot ran the child's Bash unsandboxed.
        sub_config.sandbox_enabled = ctx.sandbox_mode.is_some();
        if let Some(m) = &ctx.sandbox_mode {
            sub_config.sandbox_mode = m.clone();
        }
        sub_config.sandbox_allow_network = ctx.sandbox_allow_network;
        if ctx.default_shell.is_some() {
            sub_config.default_shell = ctx.default_shell.clone();
        }
        sub_config.env = ctx.env.clone();
        sub_config
    }
}

#[derive(Deserialize)]
struct AgentInput {
    prompt: String,
    #[serde(default)]
    description: Option<String>,
    /// Specialized agent type — selects system prompt and tool restrictions.
    /// One of: "Explore", "Plan", "general-purpose", "verification",
    ///         "oxideclaw-guide"
    #[serde(default)]
    subagent_type: Option<String>,
}

#[async_trait]
impl Tool for AgentTool {
    fn name(&self) -> &str {
        "Agent"
    }

    fn description(&self) -> &str {
        "Launch a new agent to handle complex, multi-step tasks autonomously.\n\n\
        Available agent types and the tools they have access to:\n\
        - general-purpose: General-purpose agent for researching complex questions, searching for code, and executing multi-step tasks.\n\
        - Explore: Fast agent specialized for exploring codebases. Read-only: no file modifications.\n\
        - Plan: Software architect agent for designing implementation plans. Read-only.\n\
        - verification: Verification specialist — tries to break implementations.\n\
        - oxideclaw-guide: Answers questions about OxideClaw, Agent SDK, and Claude API.\n\n\
        When the agent is done, it will return a single message back to you."
    }

    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "prompt": {
                    "type": "string",
                    "description": "The task for the agent to complete. Be specific and self-contained."
                },
                "description": {
                    "type": "string",
                    "description": "Short description of what this agent will do (shown to user)"
                },
                "subagent_type": {
                    "type": "string",
                    "description": "Specialized agent type. One of: general-purpose, Explore, Plan, verification, oxideclaw-guide",
                    "enum": ["general-purpose", "Explore", "Plan", "verification", "oxideclaw-guide"]
                }
            },
            "required": ["prompt"]
        })
    }

    async fn execute(&self, input: serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let input: AgentInput = serde_json::from_value(input)?;

        if ctx.agent_depth >= MAX_AGENT_DEPTH {
            return Ok(ToolOutput::error(format!(
                "Agent launches may nest at most {MAX_AGENT_DEPTH} deep; this agent is already \
                 at depth {}. Do the work directly instead of delegating further.",
                ctx.agent_depth
            )));
        }

        if let Some(desc) = &input.description {
            // Not eprintln!: the TUI owns the terminal in raw mode.
            tracing::info!("[Agent: {}]", desc);
        }

        let mut sub_config = self.live_config(ctx);
        // Inside an EnterWorktree session the child must work there too.
        sub_config.cwd = ctx.cwd.clone();
        // A child given the whole budget again could spend it on top of
        // what the session already has.
        if let Some(left) = ctx.budget_remaining_usd {
            if left <= 0.0 {
                return Ok(ToolOutput::error(
                    "The session's budget is spent; no sub-agent can be launched.",
                ));
            }
            sub_config.max_budget_usd =
                Some(sub_config.max_budget_usd.map_or(left, |b| b.min(left)));
        }
        // Grandchild Agent tools get these live values too, but not the
        // specialised prompt chosen for this child below.
        let tool_config = sub_config.clone();

        // Apply subagent_type: override system prompt + restrict tools as needed
        let (system_prompt_override, allowed_tools): (Option<String>, Option<Vec<String>>) =
            match input.subagent_type.as_deref() {
                Some("Explore") => (
                    Some(EXPLORE_SYSTEM_PROMPT.to_string()),
                    Some(vec![
                        "Bash".to_string(),
                        "Read".to_string(),
                        "Glob".to_string(),
                        "Grep".to_string(),
                        "WebFetch".to_string(),
                        "WebSearch".to_string(),
                    ]),
                ),
                Some("Plan") => (
                    Some(PLAN_SYSTEM_PROMPT.to_string()),
                    Some(vec![
                        "Bash".to_string(),
                        "Read".to_string(),
                        "Glob".to_string(),
                        "Grep".to_string(),
                    ]),
                ),
                Some("verification") => (
                    Some(VERIFICATION_SYSTEM_PROMPT.to_string()),
                    None, // full tools
                ),
                Some("oxideclaw-guide") => (
                    Some(OXIDECLAW_GUIDE_SYSTEM_PROMPT.to_string()),
                    Some(vec![
                        "Bash".to_string(),
                        "Read".to_string(),
                        "Glob".to_string(),
                        "Grep".to_string(),
                        "WebFetch".to_string(),
                        "WebSearch".to_string(),
                    ]),
                ),
                _ => (None, None), // general-purpose: inherit parent config
            };

        if let Some(sp) = system_prompt_override {
            sub_config.system_prompt_override = Some(sp);
        }

        // Spawn a fresh QueryEngine with the same config and tools
        let mut tools: Vec<DynTool> = default_tools_with_config(&tool_config);
        if let Some(allowed) = allowed_tools {
            tools.retain(|t| allowed.iter().any(|a| a.eq_ignore_ascii_case(t.name())));
        }
        // Apply parent allowed/disallowed tool filters too
        crate::tools::apply_tool_filters(&mut tools, &sub_config);

        let mut sub_engine = self.build_sub_engine(sub_config, tools, ctx)?;
        sub_engine.query_and_collect(&input.prompt).await
    }
}

/// Build tools for a sub-agent (same as default but includes AgentTool recursively).
fn default_tools_with_config(config: &Config) -> Vec<DynTool> {
    let mut tools = default_tools(crate::net_policy::NetPolicy::from_config(config));
    tools.push(Arc::new(AgentTool {
        config: config.clone(),
    }));
    tools
}

// ── Built-in agent system prompts ─────────────────────────────────────────────

const EXPLORE_SYSTEM_PROMPT: &str = "\
You are a file search specialist for OxideClaw, a Rust-native AI coding CLI. \
You excel at thoroughly navigating and exploring codebases.

=== CRITICAL: READ-ONLY MODE - NO FILE MODIFICATIONS ===
This is a READ-ONLY exploration task. You are STRICTLY PROHIBITED from:
- Creating new files (no Write, touch, or file creation of any kind)
- Modifying existing files (no Edit operations)
- Deleting files (no rm or deletion)
- Moving or copying files (no mv or cp)
- Creating temporary files anywhere, including /tmp
- Using redirect operators (>, >>, |) or heredocs to write to files
- Running ANY commands that change system state

Your role is EXCLUSIVELY to search and analyze existing code. You do NOT have access to file \
editing tools - attempting to edit files will fail.

Your strengths:
- Rapidly finding files using glob patterns
- Searching code and text with powerful regex patterns
- Reading and analyzing file contents

Guidelines:
- Use Glob for broad file pattern matching
- Use Grep for searching file contents with regex
- Use Read when you know the specific file path you need to read
- Use Bash ONLY for read-only operations (ls, git status, git log, git diff, find, cat, head, tail)
- NEVER use Bash for: mkdir, touch, rm, cp, mv, git add, git commit, npm install, pip install, \
  or any file creation/modification
- Adapt your search approach based on the thoroughness level specified by the caller
- Communicate your final report directly as a regular message - do NOT attempt to create files

NOTE: You are meant to be a fast agent. Make efficient use of tools and spawn parallel \
tool calls where possible. Complete the user's search request efficiently and report findings clearly.";

const PLAN_SYSTEM_PROMPT: &str = "\
You are a software architect and planning specialist for OxideClaw. \
Your role is to explore the codebase and design implementation plans.

=== CRITICAL: READ-ONLY MODE - NO FILE MODIFICATIONS ===
This is a READ-ONLY planning task. You are STRICTLY PROHIBITED from:
- Creating new files (no Write, touch, or file creation of any kind)
- Modifying existing files (no Edit operations)
- Deleting files (no rm or deletion)
- Moving or copying files (no mv or cp)
- Creating temporary files anywhere, including /tmp

Your role is EXCLUSIVELY to explore the codebase and design implementation plans.

## Your Process

1. **Understand Requirements**: Focus on the requirements provided.
2. **Explore Thoroughly**: Read files, find existing patterns and conventions, understand the \
   current architecture, identify similar features as reference, trace through relevant code paths.
   - Use Bash ONLY for read-only operations (ls, git status, git log, git diff, find, cat)
   - NEVER use Bash for mkdir, touch, rm, cp, mv, git add, git commit, or any modification
3. **Design Solution**: Create implementation approach. Consider trade-offs and architectural decisions.
4. **Detail the Plan**: Provide step-by-step implementation strategy. Identify dependencies and sequencing.

## Required Output
Provide a clear, actionable implementation plan with:
- Overview of the approach
- Step-by-step implementation strategy
- Files to create or modify
- Potential challenges and how to address them";

const VERIFICATION_SYSTEM_PROMPT: &str = "\
You are a verification specialist. Your job is not to confirm the implementation works — \
it is to try to break it.

=== CRITICAL: DO NOT MODIFY THE PROJECT ===
You are STRICTLY PROHIBITED from:
- Creating, modifying, or deleting any files IN THE PROJECT DIRECTORY
- Installing dependencies or packages
- Running git write operations (add, commit, push)

You MAY write ephemeral test scripts to /tmp when needed. Clean up after yourself.

=== VERIFICATION STRATEGY ===
Adapt your strategy based on what was changed. Always:
1. Run the build (if applicable). A broken build is an automatic FAIL.
2. Run the project's test suite (if it has one). Failing tests are an automatic FAIL.
3. Run linters/type-checkers if configured.
4. Check for regressions in related code.
5. Include at least one adversarial probe (boundary values, concurrency, idempotency, orphan ops).

=== RECOGNIZE YOUR OWN RATIONALIZATIONS ===
- 'The code looks correct based on my reading' — reading is not verification. Run it.
- 'The implementer's tests already pass' — verify independently.
- 'This is probably fine' — probably is not verified. Run it.

Your PASS/FAIL report must include actual command outputs, not just narration.";

const OXIDECLAW_GUIDE_SYSTEM_PROMPT: &str = "\
You are the OxideClaw guide agent. Your primary responsibility is helping users understand \
and use OxideClaw, the Claude Agent SDK, and the Claude API effectively.

**Your expertise spans three domains:**

1. **OxideClaw** (the CLI tool): Installation, configuration, hooks, skills, MCP servers, \
   keyboard shortcuts, IDE integrations, settings, and workflows.

2. **Claude Agent SDK**: A framework for building custom AI agents.

3. **Claude API**: The Claude API for direct model interaction, tool use, and integrations.

**Approach:**
1. Determine which domain the user's question falls into.
2. Use WebFetch to fetch the appropriate documentation.
3. Provide clear, actionable guidance based on official documentation.
4. Use WebSearch if docs don't cover the topic.
5. Reference local project files (CLAUDE.md, .claude/ directory) when relevant.

**Guidelines:**
- Always prioritize official documentation over assumptions.
- Provide specific, actionable answers with examples where helpful.
- Keep answers concise and focused on what the user needs.";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::types::{ContentBlock, ToolResultContent};
    use crate::permissions::PermissionGate;
    use serde_json::json;
    use std::sync::Mutex;

    fn config() -> Config {
        Config {
            model: "ollama:test-model".into(),
            ..Config::default()
        }
    }

    fn tool_text(o: &ToolOutput) -> String {
        o.content
            .iter()
            .map(|c| {
                let ToolResultContent::Text { text } = c;
                text.as_str()
            })
            .collect()
    }

    /// `statusline-setup` wrote a `statusLine` setting that nothing reads, so
    /// the Agent tool no longer offers it to the model.
    #[test]
    fn statusline_setup_is_not_an_agent_type() {
        let tool = AgentTool { config: config() };
        assert!(!tool.description().contains("statusline"));
        let schema = tool.input_schema();
        let kind = &schema["properties"]["subagent_type"];
        let offered: Vec<&str> = kind["enum"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(
            offered,
            [
                "general-purpose",
                "Explore",
                "Plan",
                "verification",
                "oxideclaw-guide"
            ]
        );
        assert!(!kind["description"].as_str().unwrap().contains("statusline"));
    }

    #[tokio::test]
    async fn launch_is_refused_at_the_depth_cap() {
        let tool = AgentTool { config: config() };
        let mut ctx = ToolContext::new(std::env::temp_dir());
        ctx.agent_depth = MAX_AGENT_DEPTH;
        let out = tool
            .execute(json!({"prompt": "do a thing"}), &ctx)
            .await
            .expect("a refusal is a tool error, not Err");
        assert!(out.is_error);
        assert!(tool_text(&out).contains("nest"), "{}", tool_text(&out));
    }

    /// Children were given the whole budget again; with none left, no
    /// child may start at all.
    #[tokio::test]
    async fn no_sub_agent_starts_once_the_budget_is_spent() {
        let tool = AgentTool { config: config() };
        let mut ctx = ToolContext::new(std::env::temp_dir());
        ctx.budget_remaining_usd = Some(0.0);
        let out = tool
            .execute(json!({"prompt": "do it"}), &ctx)
            .await
            .expect("a refusal is a tool error, not Err");
        assert!(out.is_error);
        assert!(tool_text(&out).contains("budget"), "{}", tool_text(&out));
    }

    struct Probe(Mutex<Option<(u8, bool)>>);
    #[async_trait]
    impl Tool for Probe {
        fn name(&self) -> &str {
            "Probe"
        }
        fn description(&self) -> &str {
            "test"
        }
        fn input_schema(&self) -> serde_json::Value {
            json!({"type": "object"})
        }
        async fn execute(&self, _: serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput> {
            *self.0.lock().unwrap() = Some((ctx.agent_depth, ctx.permission_gate.is_some()));
            Ok(ToolOutput::success("ok"))
        }
    }

    /// The child must run one level deeper and carry the parent's gate.
    #[tokio::test]
    async fn child_engine_is_one_level_deeper_and_inherits_the_gate() {
        let tool = AgentTool { config: config() };
        let mut ctx = ToolContext::new(std::env::temp_dir());
        ctx.agent_depth = 1;
        ctx.permission_gate = Some(PermissionGate::bypass());
        let probe = Arc::new(Probe(Mutex::new(None)));
        let mut engine = tool
            .build_sub_engine(config(), vec![probe.clone()], &ctx)
            .unwrap();
        let call = vec![ContentBlock::ToolUse {
            id: "t1".into(),
            name: "Probe".into(),
            input: json!({}),
        }];
        engine.execute_tools(&call).await.unwrap();
        assert_eq!(*probe.0.lock().unwrap(), Some((2, true)));
    }

    /// With a bypass gate inherited, a sensitive tool in the child runs;
    /// with the parent's gate absent the child falls back to headless and
    /// refuses. Proves the inherited gate is the one consulted.
    #[tokio::test]
    async fn child_uses_the_inherited_gate_not_a_fresh_one() {
        let dir = tempfile::tempdir().unwrap();
        let tool = AgentTool { config: config() };
        let write = || -> Vec<DynTool> { vec![Arc::new(crate::tools::file_write::FileWriteTool)] };
        let call = |p: &std::path::Path| {
            vec![ContentBlock::ToolUse {
                id: "t1".into(),
                name: "Write".into(),
                input: json!({"file_path": p.to_string_lossy(), "content": "x"}),
            }]
        };

        let mut ctx = ToolContext::new(dir.path().to_path_buf());
        ctx.permission_gate = Some(PermissionGate::bypass());
        let allowed = dir.path().join("allowed.txt");
        let mut e = tool.build_sub_engine(config(), write(), &ctx).unwrap();
        e.execute_tools(&call(&allowed)).await.unwrap();
        assert!(allowed.exists(), "bypass gate inherited → Write runs");

        let ctx = ToolContext::new(dir.path().to_path_buf());
        let refused = dir.path().join("refused.txt");
        let mut e = tool.build_sub_engine(config(), write(), &ctx).unwrap();
        e.execute_tools(&call(&refused)).await.unwrap();
        assert!(
            !refused.exists(),
            "no gate on the context → headless → refused"
        );
    }

    /// `/sandbox enable` mid-session changes only the live config; the
    /// child used to run Bash from the stale build-time snapshot, unsandboxed.
    #[tokio::test]
    async fn child_bash_honours_a_sandbox_enabled_after_the_tool_was_built() {
        let dir = tempfile::tempdir().unwrap();
        let tool = AgentTool { config: config() };
        assert!(!tool.config.sandbox_enabled);
        let mut ctx = ToolContext::new(dir.path().to_path_buf());
        ctx.permission_gate = Some(PermissionGate::bypass());
        ctx.sandbox_mode = Some("strict".into());
        ctx.sandbox_allow_network = false;

        let sub = tool.live_config(&ctx);
        assert!(sub.sandbox_enabled);
        assert_eq!(sub.sandbox_mode, "strict");
        assert!(!sub.sandbox_allow_network);

        // `echo mkfs` is harmless unsandboxed but matches strict's blocklist.
        let bash: Vec<DynTool> = vec![Arc::new(crate::tools::bash::BashTool)];
        let mut e = tool.build_sub_engine(sub, bash, &ctx).unwrap();
        let call = vec![ContentBlock::ToolUse {
            id: "t1".into(),
            name: "Bash".into(),
            input: json!({"command": "echo mkfs"}),
        }];
        let out = e.execute_tools(&call).await.unwrap();
        let ContentBlock::ToolResult {
            is_error, content, ..
        } = &out[0]
        else {
            panic!("expected a tool result");
        };
        assert_eq!(*is_error, Some(true), "{content:?}");
    }
}
