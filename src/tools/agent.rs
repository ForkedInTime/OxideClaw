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

pub struct AgentTool {
    pub config: Config,
}

#[derive(Deserialize)]
struct AgentInput {
    prompt: String,
    #[serde(default)]
    description: Option<String>,
    /// Specialized agent type — selects system prompt and tool restrictions.
    /// One of: "Explore", "Plan", "general-purpose", "verification",
    ///         "rustyclaw-guide", "statusline-setup"
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
        - rustyclaw-guide: Answers questions about RustyClaw, Agent SDK, and Claude API.\n\
        - statusline-setup: Configures the user's RustyClaw status line setting.\n\n\
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
                    "description": "Specialized agent type. One of: general-purpose, Explore, Plan, verification, rustyclaw-guide, statusline-setup",
                    "enum": ["general-purpose", "Explore", "Plan", "verification", "rustyclaw-guide", "statusline-setup"]
                }
            },
            "required": ["prompt"]
        })
    }

    async fn execute(&self, input: serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let input: AgentInput = serde_json::from_value(input)?;

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
                Some("rustyclaw-guide") => (
                    Some(RUSTYCLAW_GUIDE_SYSTEM_PROMPT.to_string()),
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

        let mut sub_engine = QueryEngine::new(sub_config, tools)?;
        sub_engine.query_and_collect(&input.prompt).await
    }
}

/// Build tools for a sub-agent (same as default but includes AgentTool recursively).
fn default_tools_with_config(config: &Config) -> Vec<DynTool> {
    let mut tools = default_tools();
    tools.push(Arc::new(AgentTool {
        config: config.clone(),
    }));
    tools
}

// ── Built-in agent system prompts ─────────────────────────────────────────────

const EXPLORE_SYSTEM_PROMPT: &str = "\
You are a file search specialist for RustyClaw, a Rust-native AI coding CLI. \
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
You are a software architect and planning specialist for RustyClaw. \
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

const RUSTYCLAW_GUIDE_SYSTEM_PROMPT: &str = "\
You are the RustyClaw guide agent. Your primary responsibility is helping users understand \
and use RustyClaw, the Claude Agent SDK, and the Claude API effectively.

**Your expertise spans three domains:**

1. **RustyClaw** (the CLI tool): Installation, configuration, hooks, skills, MCP servers, \
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
You are a status line setup agent for RustyClaw. Your job is to create or update the \
statusLine command in the user's RustyClaw settings.

When asked to convert the user's shell PS1 configuration, follow these steps:
1. Read the user's shell configuration files (~/.zshrc, ~/.bashrc, ~/.bash_profile, ~/.profile)
2. Extract the PS1 value
3. Convert PS1 escape sequences to shell commands:
   - \\u → $(whoami), \\h → $(hostname -s), \\w → $(pwd), \\W → $(basename \"$(pwd)\")
   - \\$ → $, \\n → newline, \\t → $(date +%H:%M:%S)
4. Write the converted statusLine command to the user's settings.json

Only use Read and Edit tools. Do not create new files unless asked.";
