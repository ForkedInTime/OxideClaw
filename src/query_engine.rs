/// QueryEngine — port of QueryEngine.ts + query.ts
/// The core agentic loop: send messages → receive tool calls → execute tools → repeat.
use crate::api::types::*;
use crate::api::{ApiBackend, MessagesRequest};
use crate::browser::middleware::MiddlewareVerdict;
use crate::compact::{
    CompactNeeded, compact_needed, compaction_window, snip_compact, summarize_compact,
};
use crate::config::Config;
use crate::rag;
use crate::tools::{DynTool, ToolContext};
use anyhow::{Context, Result};
use colored::Colorize;
use tracing::debug;

// System prompt is built dynamically from Config::build_system_prompt().

pub struct QueryEngine {
    client: ApiBackend,
    system_prompt: String,
    config: Config,
    tools: Vec<DynTool>,
    messages: Vec<Message>,
    json_output: bool,
    stream_json_output: bool,
    /// Print nothing to stdout. Embedded runs (browse under the SDK, the
    /// TUI, or `oxideclaw browse`'s NDJSON) own stdout; human-readable
    /// "Claude:" text there corrupted the stream.
    quiet: bool,
    include_partial_messages: bool,
    include_hook_events: bool,
    cumulative_cost_usd: f64,
    /// UUID for the session ID header sent to the Anthropic API.
    session_id: Option<String>,
    /// Shared Read-tool cache for deduplicating unchanged re-reads (v2.1.86).
    read_cache: crate::tools::ReadCache,
    /// In-process tool middleware chain (empty by default).
    middlewares: crate::browser::middleware::MiddlewareChain,
    /// Turn counter.
    turns: u32,
    /// Every tool call goes through this. Defaults to the headless gate
    /// (settings/CLI rules apply; anything needing a prompt is refused).
    gate: crate::permissions::PermissionGate,
    /// Nesting level for `Agent` launches; published to tools via ToolContext.
    agent_depth: u8,
    /// Tool whose successful call ends `query()` once that turn's results
    /// are recorded (`browse_done` for browse runs). None everywhere else.
    stop_after_tool: Option<&'static str>,
}

impl QueryEngine {
    pub fn new(config: Config, tools: Vec<DynTool>) -> Result<Self> {
        // Validate that an API key is present when using Anthropic models.
        // Ollama and OpenAI-compat providers fetch their own credentials
        // (Ollama needs none; each OpenAI-compat provider has its own env var
        // like GROQ_API_KEY, and local ones like lmstudio/openai-compat are
        // key-less). So config.api_key is only required for Anthropic.
        let is_non_anthropic = crate::api::is_ollama_model(&config.model)
            || crate::api::is_openai_compat_model(&config.model);
        if !is_non_anthropic && config.api_key.is_empty() {
            return Err(anyhow::anyhow!(
                "No Anthropic credential found.\n\
                 OxideClaw checks, in order:\n\
                   1. ANTHROPIC_API_KEY      export ANTHROPIC_API_KEY=sk-ant-...\n\
                   2. ANTHROPIC_AUTH_TOKEN   an OAuth access token\n\
                   3. apiKeyHelper / OXIDECLAW_API_KEY_FILE_DESCRIPTOR\n\
                   4. ant auth login         shared with Claude Code and the official SDKs\n\
                 To use a local model instead: --model ollama:<name>\n\
                 Or a cloud OpenAI-compatible model: --model groq:<name>, --model openrouter:<name>, ..."
            ));
        }

        let mut client = ApiBackend::new_with_auth(
            &config.model,
            &config.api_key,
            config.auth_is_oauth,
            &config.ollama_host,
        )
        .context("Failed to create API client")?;
        // Headless: retry notices go to stderr so they never contaminate
        // stdout, which carries the machine-readable result in --json modes.
        client.set_retry_notifier(std::sync::Arc::new(|n: &crate::api::retry::RetryNotice| {
            eprintln!("{}", n.message().yellow());
        }));
        let system_prompt = config.build_system_prompt();
        let gate = crate::permissions::PermissionGate::headless(&config);
        Ok(Self {
            client,
            system_prompt,
            config,
            tools,
            messages: Vec::new(),
            json_output: false,
            stream_json_output: false,
            quiet: false,
            include_partial_messages: false,
            include_hook_events: false,
            cumulative_cost_usd: 0.0,
            session_id: Some(uuid::Uuid::new_v4().to_string()),
            read_cache: crate::tools::new_read_cache(),
            middlewares: Vec::new(),
            turns: 0,
            gate,
            agent_depth: 0,
            stop_after_tool: None,
        })
    }

    /// Replace the headless default with the parent executor's gate, so a
    /// sub-agent's Bash/Write/Edit prompt the same human as the session.
    pub fn with_permission_gate(mut self, gate: crate::permissions::PermissionGate) -> Self {
        self.gate = gate;
        self
    }

    /// Record how deep this engine sits in the `Agent` launch chain.
    pub fn with_agent_depth(mut self, depth: u8) -> Self {
        self.agent_depth = depth;
        self
    }

    /// Output responses as JSON objects (one per turn).
    pub fn set_json_output(&mut self, enabled: bool) {
        self.json_output = enabled;
    }

    /// Output a stream of JSON events as they arrive.
    pub fn set_stream_json_output(&mut self, enabled: bool) {
        self.stream_json_output = enabled;
    }

    /// Include partial text chunks as individual stream-json events.
    pub fn set_include_partial_messages(&mut self, enabled: bool) {
        self.include_partial_messages = enabled;
    }

    /// Include hook lifecycle events in stream-json output.
    pub fn set_include_hook_events(&mut self, enabled: bool) {
        self.include_hook_events = enabled;
    }

    /// Add a user message and run the agentic loop until stop_reason == EndTurn.
    /// Mirrors the main query() function in query.ts.
    pub async fn query(&mut self, user_input: impl Into<String>) -> Result<()> {
        let user_input = user_input.into();
        self.turns = 0;

        // --replay-user-messages: echo user message in stream-json output
        if self.replay_user_messages() && self.stream_json_output {
            let event = serde_json::json!({"type":"user","message":{"role":"user","content":[{"type":"text","text":user_input}]}});
            println!("{}", event);
        }

        // RAG context rides in the user turn, after the prompt. Putting it in
        // `system` for the first request only changed `system` mid-
        // conversation, which invalidates the signed thinking blocks replayed
        // on the next request (a 400 on Opus 5.5 / Fable 5.1 / Sonnet 5.5).
        let rag_context = self.retrieve_rag_context(&user_input);
        let mut content = vec![ContentBlock::Text { text: user_input }];
        if !rag_context.is_empty() {
            content.push(ContentBlock::Text { text: rag_context });
        }
        self.messages.push(Message {
            role: Role::User,
            content,
        });

        const DEFAULT_MAX_TURNS: u32 = 50;
        let max_turns = if self.config.max_turns > 0 {
            self.config.max_turns
        } else {
            DEFAULT_MAX_TURNS
        };
        let mut turn = 0u32;

        loop {
            turn += 1;
            self.turns = turn;
            if turn > max_turns {
                eprintln!("{}", format!("Stopped after {max_turns} turns.").yellow());
                break;
            }
            // Build tool definitions for this turn
            let tool_defs: Vec<ToolDefinition> =
                self.tools.iter().map(|t| t.definition()).collect();

            let request = MessagesRequest {
                model: self.config.model.clone(),
                max_tokens: self.config.max_tokens_for(&self.config.model),
                system: crate::api::types::SystemContent::Plain(self.system_prompt.clone()),
                messages: self.messages.clone(),
                tools: tool_defs,
                stream: None,
                thinking: None,
                output_config: None,
                betas: self.config.extra_betas.clone(),
                session_id: self.session_id.clone(),
            };

            // Call the API with streaming, printing text as it arrives
            let mut full_text = String::new();
            let human = !self.json_output && !self.stream_json_output && !self.quiet;
            if human {
                print!("\n{} ", "Claude:".cyan().bold());
            }
            let include_partial = self.include_partial_messages && self.stream_json_output;
            // Try the call; on 529 with fallback_model, retry once with fallback
            let response = {
                let model = request.model.clone();
                let res = self
                    .client
                    .messages_stream(request.clone(), |chunk| {
                        if self.stream_json_output {
                            if include_partial {
                                let event =
                                    serde_json::json!({"type":"partial_text","text": chunk});
                                println!("{}", event);
                            }
                            full_text.push_str(chunk);
                        } else if human {
                            print!("{chunk}");
                        } else {
                            full_text.push_str(chunk);
                        }
                    })
                    .await;
                match res {
                    Err(ref e) if is_overloaded_error(e) => {
                        if let Some(ref fb) = self.config.fallback_model.clone() {
                            if fb != &model {
                                eprintln!(
                                    "{}",
                                    format!("Model overloaded — retrying with {fb}").yellow()
                                );
                                full_text.clear();
                                let mut fb_req = request.clone();
                                fb_req.model = fb.clone();
                                self.client.messages_stream(fb_req, |chunk| {
                                    if self.stream_json_output {
                                        if include_partial {
                                            let event = serde_json::json!({"type":"partial_text","text": chunk});
                                            println!("{}", event);
                                        }
                                        full_text.push_str(chunk);
                                    } else if human {
                                        print!("{chunk}");
                                    } else {
                                        full_text.push_str(chunk);
                                    }
                                }).await?
                            } else {
                                res?
                            }
                        } else {
                            res?
                        }
                    }
                    Err(e) => return Err(e),
                    Ok(r) => r,
                }
            };
            if human {
                println!(); // newline after streamed text
            }
            // JSON / stream-json mode: emit result object at end of turn
            if (self.json_output || self.stream_json_output) && !full_text.is_empty() {
                let result = serde_json::json!({
                    "type": "result",
                    "text": full_text,
                    "tokens_in": response.usage.input_tokens,
                    "tokens_out": response.usage.output_tokens,
                });
                println!("{}", result);
                full_text.clear();
            }

            // Collect assistant content into message history. An empty
            // assistant message (whitespace-only reply, refusal, a dropped
            // truncated tool call) is a 400 on the next request.
            if !response.content.is_empty() {
                self.messages.push(Message {
                    role: Role::Assistant,
                    content: response.content.clone(),
                });
            }

            // Log token usage in verbose mode
            if self.config.verbose {
                eprintln!(
                    "[tokens] in={} out={} cache_read={} cache_create={}",
                    response.usage.input_tokens,
                    response.usage.output_tokens,
                    response.usage.cache_read_input_tokens,
                    response.usage.cache_creation_input_tokens,
                );
            }

            // Track cost and check budget
            let turn_cost = estimate_cost_usd(&self.config.model, &response.usage);
            self.cumulative_cost_usd += turn_cost;
            if let Some(budget) = self.config.max_budget_usd
                && self.cumulative_cost_usd >= budget
            {
                eprintln!(
                    "{}",
                    format!(
                        "Budget limit reached: ${:.4} / ${:.4} — stopping.",
                        self.cumulative_cost_usd, budget
                    )
                    .yellow()
                );
                break;
            }

            // Context compaction check
            let window = compaction_window(&self.config, None);
            match compact_needed(response.usage.input_tokens, window) {
                CompactNeeded::None => {}
                CompactNeeded::Warn => {
                    eprintln!(
                        "{}",
                        format!(
                            "Warning: context is {:.0}% full ({} / {} tokens). \
                             Use /compact or enable auto_compact.",
                            response.usage.input_tokens as f64 * 100.0 / window as f64,
                            response.usage.input_tokens,
                            window
                        )
                        .yellow()
                    );
                }
                CompactNeeded::Snip => {
                    if self.config.auto_compact_enabled {
                        eprintln!(
                            "{}",
                            "Auto-compacting: stripping old tool results (snipCompact)…".yellow()
                        );
                        snip_compact(&mut self.messages);
                    } else {
                        eprintln!(
                            "{}",
                            "Context near limit. Enable auto_compact or run /compact.".yellow()
                        );
                    }
                }
                // Summarising now would replace the assistant tool_use that the
                // results appended below answer, orphaning them (a 400). Snip
                // this round; summarise once the model stops calling tools.
                CompactNeeded::Summarise if response.stop_reason == Some(StopReason::ToolUse) => {
                    if self.config.auto_compact_enabled {
                        snip_compact(&mut self.messages);
                    }
                }
                CompactNeeded::Summarise => {
                    if self.config.auto_compact_enabled {
                        eprintln!(
                            "{}",
                            "Auto-compacting: summarising conversation (summarizeCompact)…"
                                .yellow()
                        );
                        match summarize_compact(&self.client, &self.messages, &self.config).await {
                            Ok(replacement) => {
                                self.messages = replacement;
                                eprintln!("{}", "Compaction complete. Conversation history replaced with summary.".green());
                            }
                            Err(e) => {
                                eprintln!(
                                    "{}",
                                    format!("Compact failed: {e}. Falling back to snip.").red()
                                );
                                snip_compact(&mut self.messages);
                            }
                        }
                    } else {
                        eprintln!(
                            "{}",
                            "Context critically full. Enable auto_compact or run /compact now."
                                .red()
                        );
                    }
                }
            }

            // Check stop reason
            match &response.stop_reason {
                Some(StopReason::EndTurn) | Some(StopReason::Other) | None => break,
                Some(StopReason::MaxTokens) | Some(StopReason::ModelContextWindowExceeded) => {
                    eprintln!("{}", "Warning: max tokens reached".yellow());
                    break;
                }
                Some(StopReason::Refusal) => {
                    eprintln!("{}", "The model declined this request.".yellow());
                    break;
                }
                Some(StopReason::ToolUse) => {
                    // Execute all tool calls in this response
                    let tool_results = self.execute_tools(&response.content).await?;
                    let stop = self
                        .stop_after_tool
                        .is_some_and(|name| ran_ok(name, &response.content, &tool_results));

                    // Append tool results as a user message
                    self.messages.push(Message {
                        role: Role::User,
                        content: tool_results,
                    });
                    // Stop only after the results are in, so every tool_use
                    // in the history keeps its tool_result.
                    if stop {
                        break;
                    }
                    // Continue the loop to get Claude's next response
                }
                Some(StopReason::StopSequence) => break,
            }
        }

        Ok(())
    }

    /// Execute all tool_use blocks in the response content.
    /// Returns a vec of tool_result ContentBlocks to send back.
    pub(crate) async fn execute_tools(
        &self,
        content: &[ContentBlock],
    ) -> Result<Vec<ContentBlock>> {
        let mut ctx = ToolContext::new(self.config.cwd.clone());
        ctx.default_shell = self.config.default_shell.clone();
        ctx.snapshot_dir = self.config.file_snapshot_dir.clone();
        if self.config.sandbox_enabled {
            ctx.sandbox_mode = Some(self.config.sandbox_mode.clone());
        }
        ctx.sandbox_allow_network = self.config.sandbox_allow_network;
        ctx.read_cache = Some(self.read_cache.clone());
        // Publish live provider snapshot for AgentTool / spawn sub-agents.
        ctx.live_model = Some(self.config.model.clone());
        ctx.live_api_key = Some(self.config.api_key.clone());
        ctx.live_ollama_host = Some(self.config.ollama_host.clone());
        ctx.middlewares = self.middlewares.clone();
        ctx.permission_gate = Some(self.gate.clone());
        ctx.agent_depth = self.agent_depth;
        let mut results = Vec::new();

        for block in content {
            if let ContentBlock::ToolUse { id, name, input } = block {
                // Emit tool_use event for stream-json + hook-events mode
                if self.include_hook_events && self.stream_json_output {
                    let event = serde_json::json!({
                        "type": "tool_use",
                        "name": name,
                        "input": input,
                    });
                    println!("{}", event);
                } else if !self.json_output && !self.stream_json_output && !self.quiet {
                    println!(
                        "\n{} {}({})",
                        "Tool:".yellow().bold(),
                        name.green(),
                        truncate_json(input, 120)
                    );
                }

                // Pre-tool-use hooks always run: they are the user's guards,
                // and `-p`, sub-agents and /spawn skipped them unless hook
                // events were being streamed. Only the event is optional.
                if let Some(hook_cfg) = &self.config.hooks
                    && !self.config.disable_all_hooks
                {
                    let args = serde_json::to_string(input).unwrap_or_default();
                    let hook_result = crate::hooks::run_pre_tool_hooks(
                        hook_cfg,
                        name,
                        &args,
                        "print-mode",
                        &self.config.cwd,
                    )
                    .await;
                    if self.include_hook_events && self.stream_json_output {
                        let event = serde_json::json!({
                            "type": "hook_event",
                            "hook": "preToolUse",
                            "tool": name,
                            "continue": hook_result.should_continue,
                        });
                        println!("{}", event);
                    }
                    if !hook_result.should_continue {
                        let msg = hook_result
                            .stop_reason
                            .unwrap_or_else(|| format!("PreToolUse hook blocked: {name}"));
                        results.push(ContentBlock::ToolResult {
                            tool_use_id: id.clone(),
                            content: vec![ToolResultContent::text(msg)],
                            is_error: Some(true),
                        });
                        continue;
                    }
                }

                // ── Middleware before_tool check ──────────────────────
                let mut middleware_denied = false;
                for mw in &ctx.middlewares {
                    match mw.before_tool(name, input).await {
                        MiddlewareVerdict::Allow => {}
                        MiddlewareVerdict::Deny { reason } => {
                            results.push(ContentBlock::ToolResult {
                                tool_use_id: id.clone(),
                                content: vec![ToolResultContent::text(format!(
                                    "Middleware denied: {reason}"
                                ))],
                                is_error: Some(true),
                            });
                            middleware_denied = true;
                            break;
                        }
                    }
                }
                if middleware_denied {
                    continue;
                }

                // ── Permission gate ───────────────────────────────────
                // Same decision the TUI makes; headless engines fail closed.
                if let crate::permissions::GateOutcome::Denied(reason) =
                    self.gate.decide(name, input).await
                {
                    results.push(ContentBlock::ToolResult {
                        tool_use_id: id.clone(),
                        content: vec![ToolResultContent::text(reason)],
                        is_error: Some(true),
                    });
                    continue;
                }

                let tool = self.tools.iter().find(|t| t.name() == name);

                let output = match tool {
                    Some(t) => match t.execute(input.clone(), &ctx).await {
                        Ok(out) => out,
                        Err(e) => crate::tools::ToolOutput::error(format!("Tool error: {e}")),
                    },
                    None => crate::tools::ToolOutput::error(format!("Unknown tool: {name}")),
                };

                // ── Middleware after_tool ─────────────────────────────
                let output_text: String = output
                    .content
                    .iter()
                    .map(|c| {
                        let ToolResultContent::Text { text } = c;
                        text.as_str()
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                for mw in &ctx.middlewares {
                    mw.after_tool(name, &output_text).await;
                }
                if let Some(hook_cfg) = &self.config.hooks
                    && !self.config.disable_all_hooks
                {
                    crate::hooks::run_post_tool_hooks(
                        hook_cfg,
                        name,
                        &output_text,
                        "print-mode",
                        &self.config.cwd,
                    )
                    .await;
                }

                if output.is_error && !self.stream_json_output && !self.quiet {
                    eprintln!(
                        "{} {}",
                        "Error:".red().bold(),
                        output
                            .content
                            .iter()
                            .map(|c| {
                                let ToolResultContent::Text { text } = c;
                                text.as_str()
                            })
                            .collect::<Vec<_>>()
                            .join(" ")
                    );
                }

                // Emit tool_result event for stream-json + hook-events mode
                if self.include_hook_events && self.stream_json_output {
                    let event = serde_json::json!({
                        "type": "tool_result",
                        "name": name,
                        "is_error": output.is_error,
                        "content": output_text,
                    });
                    println!("{}", event);
                }

                results.push(ContentBlock::ToolResult {
                    tool_use_id: id.clone(),
                    content: output.content,
                    is_error: if output.is_error { Some(true) } else { None },
                });
            }
        }

        Ok(results)
    }

    /// Run a query and collect all text output as a single String.
    /// Used by AgentTool to capture sub-agent output without printing.
    pub async fn query_and_collect(
        &mut self,
        user_input: &str,
    ) -> Result<crate::tools::ToolOutput> {
        // Both callers (Agent tool, /spawn) run inside a frontend that owns
        // stdout/stderr: the TUI's raw-mode viewport, SDK NDJSON, ACP
        // JSON-RPC or `-p --output-format json`. Any print here corrupts it.
        self.quiet = true;
        self.client.set_retry_notifier(std::sync::Arc::new(
            |n: &crate::api::retry::RetryNotice| {
                tracing::warn!("{}", n.message());
            },
        ));
        self.messages.push(Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: user_input.to_string(),
            }],
        });

        // Sub-agents (Agent tool, /spawn) run unattended with a bypass gate,
        // so they need the same turn cap and budget as the headless loop.
        const DEFAULT_MAX_TURNS: u32 = 50;
        let max_turns = if self.config.max_turns > 0 {
            self.config.max_turns
        } else {
            DEFAULT_MAX_TURNS
        };
        // The agent's answer is its last message, not all its narration.
        let mut final_text = String::new();
        let mut turns = 0u32;

        loop {
            turns += 1;
            if turns > max_turns {
                final_text.push_str(&format!("\n\n[Stopped after {max_turns} turns.]"));
                break;
            }
            let tool_defs: Vec<ToolDefinition> =
                self.tools.iter().map(|t| t.definition()).collect();

            let request = MessagesRequest {
                model: self.config.model.clone(),
                max_tokens: self.config.max_tokens_for(&self.config.model),
                system: crate::api::types::SystemContent::Plain(self.system_prompt.clone()),
                messages: self.messages.clone(),
                tools: tool_defs,
                stream: None,
                thinking: None,
                output_config: None,
                betas: vec![],
                session_id: self.session_id.clone(),
            };

            let mut turn_text = String::new();
            let response = self
                .client
                .messages_stream(request, |chunk| {
                    turn_text.push_str(chunk);
                })
                .await?;
            if !turn_text.trim().is_empty() {
                final_text = turn_text;
            }

            self.cumulative_cost_usd += estimate_cost_usd(&self.config.model, &response.usage);
            if let Some(budget) = self.config.max_budget_usd
                && self.cumulative_cost_usd >= budget
            {
                final_text.push_str(&format!("\n\n[Stopped: budget of ${budget:.2} reached.]"));
                break;
            }

            if !response.content.is_empty() {
                self.messages.push(Message {
                    role: Role::Assistant,
                    content: response.content.clone(),
                });
            }

            match &response.stop_reason {
                Some(StopReason::EndTurn)
                | Some(StopReason::Other)
                | Some(StopReason::Refusal)
                | None => break,
                Some(StopReason::MaxTokens) | Some(StopReason::ModelContextWindowExceeded) => break,
                Some(StopReason::ToolUse) => {
                    let tool_results = self.execute_tools(&response.content).await?;
                    self.messages.push(Message {
                        role: Role::User,
                        content: tool_results,
                    });
                }
                Some(StopReason::StopSequence) => break,
            }
        }

        Ok(crate::tools::ToolOutput::success(final_text))
    }

    /// Clear conversation history (equivalent to /clear)
    #[allow(dead_code)] // public API for SDK/headless mode
    pub fn clear(&mut self) {
        self.messages.clear();
    }

    /// Return number of messages in history
    #[allow(dead_code)] // public API for SDK/headless mode
    pub fn history_len(&self) -> usize {
        self.messages.len()
    }
}

fn truncate_json(v: &serde_json::Value, max_len: usize) -> String {
    let s = v.to_string();
    if s.len() <= max_len {
        s
    } else {
        let mut end = max_len;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}...", &s[..end])
    }
}

/// Whether `content` called `name` and its result in `results` is not an error.
fn ran_ok(name: &str, content: &[ContentBlock], results: &[ContentBlock]) -> bool {
    content.iter().any(|b| {
        matches!(b, ContentBlock::ToolUse { id, name: n, .. } if n == name && result_ok(id, results))
    })
}

fn result_ok(tool_use_id: &str, results: &[ContentBlock]) -> bool {
    results.iter().any(|b| {
        matches!(b, ContentBlock::ToolResult { tool_use_id: t, is_error, .. }
            if t == tool_use_id && *is_error != Some(true))
    })
}

impl QueryEngine {
    /// Create a QueryEngine preconfigured for autonomous browse mode.
    /// Overrides the system prompt and injects the middleware chain.
    pub fn new_for_browse(
        config: Config,
        tools: Vec<DynTool>,
        system_prompt: String,
        middlewares: crate::browser::middleware::MiddlewareChain,
    ) -> Result<Self> {
        let mut engine = Self::new(config, tools)?;
        engine.system_prompt = system_prompt;
        engine.middlewares = middlewares;
        engine.quiet = true;
        // browse_done is the model saying it is finished; carrying on let
        // later actions run and buried its verdict under their results.
        engine.stop_after_tool = Some("browse_done");
        Ok(engine)
    }

    /// How many turns the engine has executed since the last `query()` call.
    pub fn turns_used(&self) -> u32 {
        self.turns
    }

    /// Extract the text content from the last assistant message, if any.
    pub fn last_assistant_text(&self) -> Option<String> {
        self.messages.iter().rev().find_map(|m| {
            if m.role == Role::Assistant {
                let texts: Vec<&str> = m
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect();
                if texts.is_empty() {
                    None
                } else {
                    Some(texts.join(""))
                }
            } else {
                None
            }
        })
    }

    /// Input of the newest call to `name` that ran without error. It is
    /// what the model itself passed, so unlike any tool result text it
    /// cannot be forged by page content the tools return.
    pub fn last_successful_call(&self, name: &str) -> Option<&serde_json::Value> {
        self.messages
            .iter()
            .enumerate()
            .rev()
            .filter(|(_, m)| m.role == Role::Assistant)
            .find_map(|(i, m)| {
                let results = self.messages.get(i + 1).map_or(&[][..], |r| &r.content[..]);
                m.content.iter().rev().find_map(|b| match b {
                    ContentBlock::ToolUse { id, name: n, input }
                        if n == name && result_ok(id, results) =>
                    {
                        Some(input)
                    }
                    _ => None,
                })
            })
    }

    fn replay_user_messages(&self) -> bool {
        self.config.replay_user_messages
    }

    /// Retrieve relevant code context from the local RAG index.
    /// Returns a formatted context block, or empty string if RAG is unavailable.
    fn retrieve_rag_context(&self, user_input: &str) -> String {
        // Only inject RAG if the index exists
        let db = match rag::RagDb::open(&self.config.cwd) {
            Ok(db) => db,
            Err(_) => return String::new(),
        };

        // Skip if the index is empty (not yet built)
        if db.chunk_count().unwrap_or(0) == 0 {
            return String::new();
        }

        // Fetch more candidates, then filter by relevance threshold
        let results = match rag::search::search(&db, user_input, 20) {
            Ok(r) => r,
            Err(e) => {
                debug!("RAG search failed: {e}");
                return String::new();
            }
        };

        if results.is_empty() {
            return String::new();
        }

        // Filter: only keep results with a decent relevance score.
        // FTS5 rank is negative (closer to 0 = more relevant); discard weak matches.
        let top_rank = results[0].rank;
        let threshold = if top_rank < -5.0 {
            top_rank * 0.3
        } else {
            top_rank * 0.5
        };
        let filtered: Vec<_> = results
            .into_iter()
            .filter(|r| r.rank <= threshold || r.rank <= top_rank * 0.8)
            .take(10) // cap at 10 injected chunks
            .collect();

        if filtered.is_empty() {
            return String::new();
        }

        // Context budget: ~12KB for rich models, keeps well within token limits
        let context = rag::search::build_context(&filtered, 12288);
        if !context.is_empty() {
            debug!(
                "RAG injected {} results ({} chars)",
                filtered.len(),
                context.len()
            );
        }
        context
    }
}

/// Returns true if the error is an HTTP 529 (overloaded) response.
fn is_overloaded_error(e: &anyhow::Error) -> bool {
    let msg = e.to_string();
    msg.contains("529") || msg.contains("overloaded") || msg.contains("Overloaded")
}

/// Per-call cost in USD, from the same price table as `/cost`.
fn estimate_cost_usd(model: &str, usage: &crate::api::types::Usage) -> f64 {
    crate::cost::model_price(model).cost(
        usage.input_tokens,
        usage.output_tokens,
        usage.cache_read_input_tokens,
        usage.cache_creation_input_tokens,
    )
}

#[cfg(test)]
mod rag_placement_tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn sse(blocks: &[serde_json::Value], stop_reason: &str) -> String {
        let mut events = vec![
            r#"{"type":"message_start","message":{"id":"m","type":"message","role":"assistant","content":[],"model":"x","stop_reason":null,"usage":{"input_tokens":1,"output_tokens":0}}}"#.to_string(),
        ];
        for (i, b) in blocks.iter().enumerate() {
            events.push(
                serde_json::json!({"type":"content_block_start","index":i,"content_block":b})
                    .to_string(),
            );
            if b["type"] == "tool_use" {
                events.push(serde_json::json!({"type":"content_block_delta","index":i,"delta":{"type":"input_json_delta","partial_json":"{}"}}).to_string());
            }
            events.push(serde_json::json!({"type":"content_block_stop","index":i}).to_string());
        }
        events.push(serde_json::json!({"type":"message_delta","delta":{"stop_reason":stop_reason},"usage":{"output_tokens":5}}).to_string());
        events.push(r#"{"type":"message_stop"}"#.to_string());
        let body: String = events.iter().map(|e| format!("data: {e}\n\n")).collect();
        format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    /// Anthropic stand-in: answers each connection with the next scripted
    /// response and records every request body.
    async fn serve(responses: Vec<String>) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        tokio::spawn(async move {
            for response in responses {
                let (mut sock, _) = listener.accept().await.unwrap();
                let mut raw = Vec::new();
                let mut buf = [0u8; 8192];
                loop {
                    let n = sock.read(&mut buf).await.unwrap();
                    raw.extend_from_slice(&buf[..n]);
                    let text = String::from_utf8_lossy(&raw).to_string();
                    if let Some(split) = text.find("\r\n\r\n") {
                        let len = text[..split]
                            .lines()
                            .find_map(|l| {
                                l.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|v| v.trim().parse::<usize>().unwrap())
                            })
                            .unwrap_or(0);
                        if raw.len() >= split + 4 + len || n == 0 {
                            sink.lock().unwrap().push(text[split + 4..].to_string());
                            break;
                        }
                    }
                    if n == 0 {
                        break;
                    }
                }
                let _ = sock.write_all(response.as_bytes()).await;
                let _ = sock.shutdown().await;
            }
        });
        (format!("http://{addr}"), seen)
    }

    /// RAG text went into `system` on the first request of a prompt only,
    /// so the request after a tool call carried a different `system` and
    /// the replayed thinking signatures were rejected. `system` must be
    /// identical on every request, with the context in the user turn.
    #[tokio::test]
    async fn rag_context_keeps_the_system_prompt_stable_across_tool_turns() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("auth.rs"),
            "/// Validate the session token expiry.\nfn validate_session_token() -> bool { true }\n",
        )
        .unwrap();
        let db = rag::RagDb::open(dir.path()).unwrap();
        rag::indexer::index_project(&db, dir.path(), true).unwrap();
        drop(db);

        let (url, seen) = serve(vec![
            sse(
                &[serde_json::json!({"type":"tool_use","id":"t1","name":"Nope","input":{}})],
                "tool_use",
            ),
            sse(
                &[serde_json::json!({"type":"text","text":"done"})],
                "end_turn",
            ),
        ])
        .await;
        let config = Config {
            model: "claude-sonnet-5".into(),
            api_key: "sk-ant-test".into(),
            cwd: dir.path().to_path_buf(),
            ..Config::default()
        };
        let mut e = QueryEngine::new(config, Vec::new()).unwrap();
        e.quiet = true;
        let mut c = crate::api::ClaudeClient::new("sk-ant-test").unwrap();
        c.set_base_url_for_test(url);
        e.client = ApiBackend::Anthropic(c);

        e.query("validate session token").await.unwrap();

        let bodies: Vec<serde_json::Value> = seen
            .lock()
            .unwrap()
            .iter()
            .map(|b| serde_json::from_str(b).unwrap())
            .collect();
        assert_eq!(bodies.len(), 2);
        assert_eq!(bodies[0]["system"], bodies[1]["system"]);
        assert!(!bodies[0]["system"].to_string().contains("codebase_context"));
        let user = &bodies[1]["messages"][0]["content"];
        assert_eq!(user[0]["text"], "validate session token");
        assert!(
            user[1]["text"]
                .as_str()
                .unwrap()
                .contains("validate_session_token"),
            "{user}"
        );
    }
}

#[cfg(test)]
mod permission_wiring_tests {
    use super::*;
    use crate::permissions::{
        PermissionAsker, PermissionDecision, PermissionGate, PermissionState,
    };
    use crate::tools::{Tool, ToolContext, ToolOutput};
    use serde_json::json;
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    fn engine(dir: &Path, tools: Vec<DynTool>) -> QueryEngine {
        // Ollama needs no credential, and nothing is contacted until a query runs.
        let c = Config {
            model: "ollama:test-model".into(),
            cwd: dir.to_path_buf(),
            ..Config::default()
        };
        QueryEngine::new(c, tools).unwrap()
    }

    fn write_call(path: &Path) -> Vec<ContentBlock> {
        vec![ContentBlock::ToolUse {
            id: "t1".into(),
            name: "Write".into(),
            input: json!({"file_path": path.to_string_lossy(), "content": "x"}),
        }]
    }

    fn result(blocks: &[ContentBlock]) -> (bool, String) {
        match &blocks[0] {
            ContentBlock::ToolResult {
                content, is_error, ..
            } => {
                let text = content
                    .iter()
                    .map(|c| {
                        let ToolResultContent::Text { text } = c;
                        text.clone()
                    })
                    .collect::<Vec<_>>()
                    .join("");
                (is_error.unwrap_or(false), text)
            }
            other => panic!("expected a tool result, got {other:?}"),
        }
    }

    struct AlwaysDeny;
    #[async_trait::async_trait]
    impl PermissionAsker for AlwaysDeny {
        async fn ask(&self, _: &str, _: &str) -> Option<PermissionDecision> {
            Some(PermissionDecision::Deny)
        }
    }

    /// The bug this guards: sub-agents and `-p` sessions ran Write/Edit/Bash
    /// with no check at all.
    #[tokio::test]
    async fn default_engine_refuses_a_sensitive_tool_with_no_human_attached() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("marker.txt");
        let e = engine(
            dir.path(),
            vec![Arc::new(crate::tools::file_write::FileWriteTool)],
        );
        let out = e.execute_tools(&write_call(&marker)).await.unwrap();
        let (is_error, text) = result(&out);
        assert!(is_error, "{text}");
        assert!(text.contains("no interactive session"), "{text}");
        assert!(!marker.exists(), "the tool must not have run");
    }

    #[tokio::test]
    async fn a_human_deny_means_the_tool_never_runs() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("marker.txt");
        let gate = PermissionGate::new(
            PermissionState::new(false, &[], &[]),
            false,
            Some(Arc::new(AlwaysDeny)),
        );
        let e = engine(
            dir.path(),
            vec![Arc::new(crate::tools::file_write::FileWriteTool)],
        )
        .with_permission_gate(gate);
        let out = e.execute_tools(&write_call(&marker)).await.unwrap();
        let (is_error, text) = result(&out);
        assert!(is_error && text.contains("Permission denied"), "{text}");
        assert!(!marker.exists());
    }

    #[tokio::test]
    async fn a_bypass_gate_lets_the_tool_run() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("marker.txt");
        let e = engine(
            dir.path(),
            vec![Arc::new(crate::tools::file_write::FileWriteTool)],
        )
        .with_permission_gate(PermissionGate::bypass());
        let out = e.execute_tools(&write_call(&marker)).await.unwrap();
        let (is_error, text) = result(&out);
        assert!(!is_error, "{text}");
        assert!(marker.exists());
    }

    /// Records what the executor published to it.
    struct Probe(Mutex<Option<(u8, bool)>>);
    #[async_trait::async_trait]
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

    /// `Agent` reads these off its context to build the child engine; if
    /// the executor stopped publishing them, nesting would silently lose
    /// both the gate and the depth cap.
    #[tokio::test]
    async fn tools_see_the_gate_and_depth_of_their_executor() {
        let dir = tempfile::tempdir().unwrap();
        let probe = Arc::new(Probe(Mutex::new(None)));
        let e = engine(dir.path(), vec![probe.clone()]).with_agent_depth(2);
        let call = vec![ContentBlock::ToolUse {
            id: "t1".into(),
            name: "Probe".into(),
            input: json!({}),
        }];
        e.execute_tools(&call).await.unwrap();
        assert_eq!(*probe.0.lock().unwrap(), Some((2, true)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Sub-agents used to println! "Tool: ..." for every child tool call
    /// straight into the TUI / SDK / ACP stdout stream.
    #[tokio::test]
    async fn query_and_collect_silences_the_engine() {
        let config = Config {
            model: "ollama:test-model".into(),
            // Nothing listens here: the request fails fast, no retries.
            ollama_host: "http://127.0.0.1:1".into(),
            ..Config::default()
        };
        let mut engine = QueryEngine::new(config, Vec::new()).unwrap();
        assert!(!engine.quiet);
        let _ = engine.query_and_collect("hi").await;
        assert!(engine.quiet, "collected runs must never print");
    }

    /// Model endpoint that answers every request with one `browse_done`
    /// tool call, counting the requests.
    async fn browse_done_model() -> (String, std::sync::Arc<std::sync::atomic::AtomicU32>) {
        use std::sync::atomic::{AtomicU32, Ordering};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = std::sync::Arc::new(AtomicU32::new(0));
        let counter = hits.clone();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                counter.fetch_add(1, Ordering::SeqCst);
                // Read the headers and the Content-Length body, then answer.
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                loop {
                    let n = sock.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    let text = String::from_utf8_lossy(&buf);
                    if let Some(end) = text.find("\r\n\r\n") {
                        let len = text[..end]
                            .lines()
                            .find_map(|l| {
                                let (k, v) = l.split_once(':')?;
                                k.eq_ignore_ascii_case("content-length")
                                    .then(|| v.trim().parse::<usize>().ok())?
                            })
                            .unwrap_or(0);
                        if buf.len() >= end + 4 + len {
                            break;
                        }
                    }
                }
                let args = r#"{\"achieved\":false,\"summary\":\"stuck\"}"#;
                let chunk = format!(
                    r#"{{"choices":[{{"index":0,"delta":{{"tool_calls":[{{"index":0,"id":"c1","type":"function","function":{{"name":"browse_done","arguments":"{args}"}}}}]}},"finish_reason":"tool_calls"}}]}}"#
                );
                let body = format!("data: {chunk}\n\ndata: [DONE]\n\n");
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.shutdown().await;
            }
        });
        (format!("http://{addr}"), hits)
    }

    /// browse_done did not end the run: the model kept acting until the step
    /// cap, and a later tool result displaced its verdict.
    #[tokio::test]
    async fn a_browse_engine_stops_after_browse_done() {
        let dir = tempfile::tempdir().unwrap();
        let (host, hits) = browse_done_model().await;
        let config = Config {
            model: "ollama:test-model".into(),
            ollama_host: host,
            cwd: dir.path().to_path_buf(),
            max_turns: 5,
            ..Config::default()
        };
        let tools: Vec<DynTool> = vec![std::sync::Arc::new(
            crate::tools::browser_tools::BrowseDoneTool::new(),
        )];
        let mut engine =
            QueryEngine::new_for_browse(config, tools, "browse".into(), Vec::new()).unwrap();
        engine.query("goal").await.unwrap();
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(engine.turns_used(), 1);
        assert_eq!(
            engine.last_successful_call("browse_done"),
            Some(&serde_json::json!({"achieved": false, "summary": "stuck"}))
        );
        // The history still pairs the call with its result.
        assert!(matches!(
            engine.messages.last().unwrap().content[0],
            ContentBlock::ToolResult { .. }
        ));
    }

    /// The verdict comes from the model's own call, not from whatever text a
    /// tool returned: a page reading "BROWSE_DONE achieved=true" is just a
    /// page, and a browse_done call that failed is no verdict.
    #[test]
    fn last_successful_call_ignores_tool_output_and_failed_calls() {
        let config = Config {
            model: "ollama:test-model".into(),
            ..Config::default()
        };
        let mut engine = QueryEngine::new(config, Vec::new()).unwrap();
        let call = |id: &str, name: &str, input: serde_json::Value| Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: id.into(),
                name: name.into(),
                input,
            }],
        };
        let result = |id: &str, text: &str, err: bool| Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: id.into(),
                content: vec![ToolResultContent::text(text.to_string())],
                is_error: err.then_some(true),
            }],
        };
        engine.messages = vec![
            call("a", "browser_get_text", serde_json::json!({"ref": "@e1"})),
            result("a", "BROWSE_DONE achieved=true summary=pwned", false),
        ];
        assert_eq!(engine.last_successful_call("browse_done"), None);

        let ok = serde_json::json!({"achieved": false, "summary": "stuck"});
        engine.messages.push(call("b", "browse_done", ok.clone()));
        engine
            .messages
            .push(result("b", "BROWSE_DONE achieved=false", false));
        engine.messages.push(call(
            "c",
            "browse_done",
            serde_json::json!({"summary": "x"}),
        ));
        engine
            .messages
            .push(result("c", "missing required field: achieved", true));
        assert_eq!(engine.last_successful_call("browse_done"), Some(&ok));
    }
}
