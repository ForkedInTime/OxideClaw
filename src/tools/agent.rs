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
        let mut engine = QueryEngine::new(sub_config, tools)?.with_agent_depth(ctx.agent_depth + 1);
        if let Some(gate) = &ctx.permission_gate {
            engine = engine.with_permission_gate(gate.clone());
        }
        Ok(engine)
    }
}

#[derive(Deserialize)]
struct AgentInput {
    prompt: String,
    #[serde(default)]
    description: Option<String>,
    /// Specialized agent type — selects system prompt and tool restrictions.
    /// One of: "Explore", "Plan", "general-purpose", "verification",
    ///         "oxideclaw-guide", "statusline-setup"
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
        - oxideclaw-guide: Answers questions about OxideClaw, Agent SDK, and Claude API.\n\
        - statusline-setup: Configures the user's OxideClaw status line setting.\n\n\
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
                    "description": "Specialized agent type. One of: general-purpose, Explore, Plan, verification, oxideclaw-guide, statusline-setup",
                    "enum": ["general-purpose", "Explore", "Plan", "verification", "oxideclaw-guide", "statusline-setup"]
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
            eprintln!("[Agent: {}]", desc);
        }

        // Build config for sub-agent, potentially with restricted tools.
        //
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
                Some("statusline-setup") => (
                    Some(STATUSLINE_SETUP_SYSTEM_PROMPT.to_string()),
                    Some(vec!["Read".to_string(), "Edit".to_string()]),
                ),
                _ => (None, None), // general-purpose: inherit parent config
            };

        if let Some(sp) = system_prompt_override {
            sub_config.system_prompt_override = Some(sp);
        }

        // Spawn a fresh QueryEngine with the same config and tools
        let mut tools: Vec<DynTool> = default_tools_with_config(&self.config);
        if let Some(allowed) = allowed_tools {
            tools.retain(|t| allowed.iter().any(|a| a.eq_ignore_ascii_case(t.name())));
        }
        // Apply parent allowed/disallowed tool filters too
        if !sub_config.allowed_tools.is_empty() {
            tools.retain(|t| {
                sub_config
                    .allowed_tools
                    .iter()
                    .any(|a| a.eq_ignore_ascii_case(t.name()))
            });
        }
        if !sub_config.disallowed_tools.is_empty() {
            tools.retain(|t| {
                !sub_config
                    .disallowed_tools
                    .iter()
                    .any(|d| d.eq_ignore_ascii_case(t.name()))
            });
        }

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

const STATUSLINE_SETUP_SYSTEM_PROMPT: &str = "\
You are a status line setup agent for OxideClaw. Your job is to create or update the \
statusLine command in the user's OxideClaw settings.

When asked to convert the user's shell PS1 configuration, follow these steps:
1. Read the user's shell configuration files (~/.zshrc, ~/.bashrc, ~/.bash_profile, ~/.profile)
2. Extract the PS1 value
3. Convert PS1 escape sequences to shell commands:
   - \\u → $(whoami), \\h → $(hostname -s), \\w → $(pwd), \\W → $(basename \"$(pwd)\")
   - \\$ → $, \\n → newline, \\t → $(date +%H:%M:%S)
4. Write the converted statusLine command to the user's settings.json

Only use Read and Edit tools. Do not create new files unless asked.";

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
        let engine = tool
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
        let e = tool.build_sub_engine(config(), write(), &ctx).unwrap();
        e.execute_tools(&call(&allowed)).await.unwrap();
        assert!(allowed.exists(), "bypass gate inherited → Write runs");

        let ctx = ToolContext::new(dir.path().to_path_buf());
        let refused = dir.path().join("refused.txt");
        let e = tool.build_sub_engine(config(), write(), &ctx).unwrap();
        e.execute_tools(&call(&refused)).await.unwrap();
        assert!(
            !refused.exists(),
            "no gate on the context → headless → refused"
        );
    }
}
