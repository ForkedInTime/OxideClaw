/// QueryEngine — port of QueryEngine.ts + query.ts
/// The core agentic loop: send messages → receive tool calls → execute tools → repeat.
use crate::api::types::*;
use crate::api::{ApiBackend, MessagesRequest};
use crate::compact::{CompactNeeded, compact_needed, snip_compact, summarize_compact};
use crate::config::Config;
use crate::rag;
use crate::browser::middleware::MiddlewareVerdict;
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
                "ANTHROPIC_API_KEY is not set.\n\
                 Set it with: export ANTHROPIC_API_KEY=sk-ant-...\n\
                 To use a local model instead: --model ollama:<name>\n\
                 Or a cloud OpenAI-compatible model: --model groq:<name>, --model openrouter:<name>, ..."
            ));
        }

        let client = ApiBackend::new(&config.model, &config.api_key, &config.ollama_host)
            .context("Failed to create API client")?;
        let system_prompt = config.build_system_prompt();
        Ok(Self {
            client,
            system_prompt,
            config,
            tools,
            messages: Vec::new(),
            json_output: false,
            stream_json_output: false,
            include_partial_messages: false,
            include_hook_events: false,
            cumulative_cost_usd: 0.0,
            session_id: Some(uuid::Uuid::new_v4().to_string()),
            read_cache: crate::tools::new_read_cache(),
            middlewares: Vec::new(),
            turns: 0,
        })
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

        // --replay-user-messages: echo user message in stream-json output
        if self.replay_user_messages() && self.stream_json_output {
            let event = serde_json::json!({"type":"user","message":{"role":"user","content":[{"type":"text","text":user_input}]}});
            println!("{}", event);
        }

        // RAG context injection: search the local index for relevant code
        let rag_context = self.retrieve_rag_context(&user_input);

        // Append user message
        self.messages.push(Message {
            role: Role::User,
            content: vec![ContentBlock::Text { text: user_input }],
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

            // Augment system prompt with RAG context on the first turn
            let effective_system = if turn == 1 && !rag_context.is_empty() {
                format!("{}\n\n{}", self.system_prompt, rag_context)
            } else {
                self.system_prompt.clone()
            };

            let request = MessagesRequest {
                model: self.config.model.clone(),
                max_tokens: self.config.max_tokens_for(&self.config.model),
                system: crate::api::types::SystemContent::Plain(effective_system),
                messages: self.messages.clone(),
                tools: tool_defs,
                stream: None,
                thinking: None,
                betas: self.config.extra_betas.clone(),
                session_id: self.session_id.clone(),
            };

            // Call the API with streaming, printing text as it arrives
            let mut full_text = String::new();
            if !self.json_output && !self.stream_json_output {
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
                        } else if self.json_output {
                            full_text.push_str(chunk);
                        } else {
                            print!("{chunk}");
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
                                    } else if self.json_output {
                                        full_text.push_str(chunk);
                                    } else {
                                        print!("{chunk}");
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
            if !self.json_output && !self.stream_json_output {
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

            // Collect assistant content into message history
            let assistant_message = Message {
                role: Role::Assistant,
                content: response.content.clone(),
            };
            self.messages.push(assistant_message);

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
            let turn_cost = estimate_cost_usd(
                &self.config.model,
                response.usage.input_tokens,
                response.usage.output_tokens,
            );
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
            match compact_needed(response.usage.input_tokens) {
                CompactNeeded::None => {}
                CompactNeeded::Warn => {
                    eprintln!(
                        "{}",
                        format!(
                            "Warning: context is {:.0}% full ({} / ~200k tokens). \
                             Use /compact or enable auto_compact.",
                            response.usage.input_tokens as f64 / 2000.0,
                            response.usage.input_tokens
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
                Some(StopReason::EndTurn) | None => break,
                Some(StopReason::MaxTokens) => {
                    eprintln!("{}", "Warning: max tokens reached".yellow());
                    break;
                }
                Some(StopReason::ToolUse) => {
                    // Execute all tool calls in this response
                    let tool_results = self.execute_tools(&response.content).await?;

                    // Append tool results as a user message
                    self.messages.push(Message {
                        role: Role::User,
                        content: tool_results,
                    });
                    // Continue the loop to get Claude's next response
                }
                Some(StopReason::StopSequence) => break,
            }
        }

        Ok(())
    }

    /// Execute all tool_use blocks in the response content.
    /// Returns a vec of tool_result ContentBlocks to send back.
    async fn execute_tools(&self, content: &[ContentBlock]) -> Result<Vec<ContentBlock>> {
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
                } else if !self.json_output && !self.stream_json_output {
                    println!(
                        "\n{} {}({})",
                        "Tool:".yellow().bold(),
                        name.green(),
                        truncate_json(input, 120)
                    );
                }

                // Pre-tool-use hooks (emit event if requested)
                if self.include_hook_events
                    && self.stream_json_output
                    && let Some(hook_cfg) = &self.config.hooks
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
                    let event = serde_json::json!({
                        "type": "hook_event",
                        "hook": "preToolUse",
                        "tool": name,
                        "continue": hook_result.should_continue,
                    });
                    println!("{}", event);
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
                        MiddlewareVerdict::RequireConfirmation { reason, .. } => {
                            // Treat as Deny until the approval gate resolves internally.
                            results.push(ContentBlock::ToolResult {
                                tool_use_id: id.clone(),
                                content: vec![ToolResultContent::text(format!(
                                    "Middleware requires confirmation: {reason}"
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

                if output.is_error && !self.stream_json_output {
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
        self.messages.push(Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: user_input.to_string(),
            }],
        });

        let mut final_text = String::new();

        loop {
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
                betas: vec![],
                session_id: self.session_id.clone(),
            };

            let response = self
                .client
                .messages_stream(request, |chunk| {
                    final_text.push_str(chunk);
                })
                .await?;

            self.messages.push(Message {
                role: Role::Assistant,
                content: response.content.clone(),
            });

            match &response.stop_reason {
                Some(StopReason::EndTurn) | None => break,
                Some(StopReason::MaxTokens) => break,
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
        format!("{}...", &s[..max_len])
    }
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

    /// Extract the text content from the last tool result (user-role message
    /// containing `ContentBlock::ToolResult`). Tool results are always sent
    /// in user-role messages per the Anthropic API contract.
    pub fn last_tool_result_text(&self) -> Option<String> {
        for msg in self.messages.iter().rev() {
            if msg.role != Role::User {
                continue;
            }
            for block in msg.content.iter().rev() {
                if let ContentBlock::ToolResult { content, .. } = block {
                    let text: String = content
                        .iter()
                        .map(|c| {
                            let ToolResultContent::Text { text } = c;
                            text.as_str()
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    if !text.is_empty() {
                        return Some(text);
                    }
                }
            }
        }
        None
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

/// Rough per-model cost estimate in USD.
/// Uses publicly documented pricing as of mid-2025.
fn estimate_cost_usd(model: &str, input_tokens: u64, output_tokens: u64) -> f64 {
    // Prices per million tokens (input, output)
    let (price_in, price_out): (f64, f64) = if model.contains("opus") {
        (15.0, 75.0)
    } else if model.contains("haiku") {
        (0.25, 1.25)
    } else {
        // sonnet / default
        (3.0, 15.0)
    };
    (input_tokens as f64 / 1_000_000.0) * price_in
        + (output_tokens as f64 / 1_000_000.0) * price_out
}
