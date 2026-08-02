//! SdkSession — wraps the API + tool execution for SDK headless use.
//!
//! Each session owns an ApiBackend, CostTracker, and PolicyEngine.
//! The turn lifecycle: receive prompt → run agentic loop → stream notifications → complete.

use crate::api::types::*;
use crate::api::{ApiBackend, MessagesRequest};
use crate::config::Config;
use crate::cost::CostTracker;
use crate::rag;
use crate::sdk::approval::{ApprovalDecision, PolicyEngine};
use crate::sdk::protocol::*;
use crate::tools::{DynTool, PermissionMode, ReadCache, ToolContext, ToolOutput, new_read_cache};
use anyhow::{Context, Result};
use std::time::Instant;
use tokio::sync::mpsc;
use tracing::debug;

/// Maximum context window tokens (200K for Claude).  Used for health estimates.
const MAX_CONTEXT_TOKENS: u64 = 200_000;

pub struct SdkSession {
    pub session_id: String,
    config: Config,
    client: ApiBackend,
    system_prompt: String,
    tools: Vec<DynTool>,
    messages: Vec<Message>,
    cost_tracker: CostTracker,
    policy_engine: PolicyEngine,
    capabilities: Capabilities,
    tools_used_this_turn: Vec<String>,
    tools_executed_count: u32,
    read_cache: ReadCache,
    notif_tx: mpsc::UnboundedSender<SdkNotification>,
    /// Channel to send approval-needed notifications to the host.
    approval_tx: mpsc::UnboundedSender<SdkNotification>,
    /// Channel to receive approval/deny decisions from the host.
    approval_rx: mpsc::UnboundedReceiver<(String, Option<String>)>,
}

impl SdkSession {
    pub fn new(
        config: Config,
        tools: Vec<DynTool>,
        policy: Policy,
        capabilities: Capabilities,
        notif_tx: mpsc::UnboundedSender<SdkNotification>,
        approval_tx: mpsc::UnboundedSender<SdkNotification>,
        approval_rx: mpsc::UnboundedReceiver<(String, Option<String>)>,
    ) -> Result<Self> {
        let client = ApiBackend::new_with_auth(&config.model, &config.api_key, config.auth_is_oauth, &config.ollama_host)
            .context("Failed to create API client")?;
        let system_prompt = config.build_system_prompt();
        let session_id = uuid::Uuid::new_v4().to_string();

        let mut cost_tracker = CostTracker::new();
        if let Some(budget) = config.max_budget_usd {
            cost_tracker.set_budget(budget);
        }

        let interactive = capabilities.interactive_approval;

        Ok(Self {
            session_id,
            config,
            client,
            system_prompt,
            tools,
            messages: Vec::new(),
            cost_tracker,
            policy_engine: PolicyEngine::new(policy, interactive),
            capabilities,
            tools_used_this_turn: Vec::new(),
            tools_executed_count: 0,
            read_cache: new_read_cache(),
            notif_tx,
            approval_rx,
            approval_tx,
        })
    }

    /// The model this session is configured to use.
    pub fn config_model(&self) -> &str {
        &self.config.model
    }

    /// Execute a full agentic turn: prompt → stream → tool loop → complete.
    pub async fn execute_turn(&mut self, prompt: String) -> Result<()> {
        let turn_start = Instant::now();
        let mut turn_input_tokens: u64 = 0;
        let mut turn_output_tokens: u64 = 0;
        let turn_cost_start = self.cost_tracker.total_cost_usd;

        // 1. Reset per-turn state
        self.tools_used_this_turn.clear();

        // 2. Retrieve RAG context (silently ignore errors)
        let rag_context = self.retrieve_rag_context(&prompt);

        // 3. Push user message
        self.messages.push(Message {
            role: Role::User,
            content: vec![ContentBlock::Text { text: prompt }],
        });

        // 4. Agentic loop
        const DEFAULT_MAX_TURNS: u32 = 50;
        let max_turns = if self.config.max_turns > 0 {
            self.config.max_turns
        } else {
            DEFAULT_MAX_TURNS
        };

        let mut final_text = String::new();
        let mut loop_turn = 0u32;

        loop {
            loop_turn += 1;
            if loop_turn > max_turns {
                self.send_notif(SdkNotification::Error {
                    session_id: self.session_id.clone(),
                    code: "max_turns".into(),
                    message: format!("Stopped after {max_turns} agentic turns."),
                });
                break;
            }

            // Build tool definitions
            let tool_defs: Vec<ToolDefinition> =
                self.tools.iter().map(|t| t.definition()).collect();

            // Augment system prompt with RAG context on the first turn + capabilities
            let mut effective_system = if loop_turn == 1 && !rag_context.is_empty() {
                format!("{}\n\n{}", self.system_prompt, rag_context)
            } else {
                self.system_prompt.clone()
            };
            self.inject_capabilities(&mut effective_system);

            let request = MessagesRequest {
                model: self.config.model.clone(),
                max_tokens: self.config.max_tokens_for(&self.config.model),
                system: SystemContent::Plain(effective_system),
                messages: self.messages.clone(),
                tools: tool_defs,
                stream: None,
                thinking: None,
                betas: self.config.extra_betas.clone(),
                session_id: Some(self.session_id.clone()),
            };

            // Stream the response — callback sends MessageDelta notifications
            let notif_tx = self.notif_tx.clone();
            let sid = self.session_id.clone();
            let mut turn_text = String::new();

            let response = self
                .client
                .messages_stream(request, |chunk| {
                    turn_text.push_str(chunk);
                    let _ = notif_tx.send(SdkNotification::MessageDelta {
                        session_id: sid.clone(),
                        content: chunk.to_string(),
                    });
                })
                .await
                .context("API stream call failed")?;

            if !turn_text.is_empty() {
                final_text = turn_text;
            }

            // Push assistant message to history
            self.messages.push(Message {
                role: Role::Assistant,
                content: response.content.clone(),
            });

            // Record cost
            let input_tok = response.usage.input_tokens;
            let output_tok = response.usage.output_tokens;
            turn_input_tokens += input_tok;
            turn_output_tokens += output_tok;
            self.cost_tracker
                .record(&self.config.model, input_tok, output_tok);

            let turn_cost = self.cost_tracker.total_cost_usd - turn_cost_start;
            self.send_notif(SdkNotification::CostUpdated {
                session_id: self.session_id.clone(),
                turn_cost_usd: turn_cost,
                session_total_usd: self.cost_tracker.total_cost_usd,
                budget_remaining_usd: self.cost_tracker.remaining(),
                input_tokens: input_tok,
                output_tokens: output_tok,
                model: self.config.model.clone(),
            });

            // Context health — input_tokens represents the full conversation context
            // sent to the model (system + messages + tools), which is the real measure
            // of how full the context window is.
            let used_pct =
                ((input_tok as f64 / MAX_CONTEXT_TOKENS as f64) * 100.0).min(100.0) as u8;
            self.send_notif(SdkNotification::ContextHealth {
                session_id: self.session_id.clone(),
                used_pct,
                tokens_used: input_tok,
                tokens_max: MAX_CONTEXT_TOKENS,
                compaction_imminent: used_pct >= 85,
            });

            // Budget check
            if self.cost_tracker.over_budget() {
                self.send_notif(SdkNotification::Error {
                    session_id: self.session_id.clone(),
                    code: "budget_exceeded".into(),
                    message: format!(
                        "Budget limit reached: ${:.4} spent.",
                        self.cost_tracker.total_cost_usd
                    ),
                });
                break;
            }

            // Check stop reason
            match &response.stop_reason {
                Some(StopReason::ToolUse) => {
                    let tool_results = self.execute_tools_with_approval(&response.content).await?;

                    // Push tool results as user message
                    self.messages.push(Message {
                        role: Role::User,
                        content: tool_results,
                    });

                    // Progress notification
                    self.send_notif(SdkNotification::ProgressUpdated {
                        session_id: self.session_id.clone(),
                        percent: ((loop_turn as f64 / max_turns as f64) * 100.0).min(99.0) as u8,
                        stage: "executing_tools".into(),
                        tools_executed: self.tools_executed_count,
                        tools_remaining_estimate: 0, // unknown
                    });

                    // Continue loop for next API call
                }
                Some(StopReason::EndTurn) | None => break,
                Some(StopReason::MaxTokens) => {
                    self.send_notif(SdkNotification::Error {
                        session_id: self.session_id.clone(),
                        code: "max_tokens".into(),
                        message: "Max tokens reached.".into(),
                    });
                    break;
                }
                Some(StopReason::StopSequence) => break,
            }
        }

        // 5. TurnCompleted notification
        let duration_ms = turn_start.elapsed().as_millis() as u64;
        let turn_cost = self.cost_tracker.total_cost_usd - turn_cost_start;

        self.send_notif(SdkNotification::TurnCompleted {
            session_id: self.session_id.clone(),
            response: final_text,
            structured_output: None,
            cost_usd: turn_cost,
            total_session_cost_usd: self.cost_tracker.total_cost_usd,
            tokens: TokenUsage {
                input: turn_input_tokens,
                output: turn_output_tokens,
            },
            model: self.config.model.clone(),
            tools_used: self.tools_used_this_turn.clone(),
            duration_ms,
        });

        Ok(())
    }

    /// Execute tool calls with policy-based approval.
    async fn execute_tools_with_approval(
        &mut self,
        content: &[ContentBlock],
    ) -> Result<Vec<ContentBlock>> {
        let mut ctx = ToolContext::new(self.config.cwd.clone());
        ctx.permission_mode = PermissionMode::BypassPermissions; // SDK handles approval via PolicyEngine
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

        let mut results = Vec::new();

        for block in content {
            let (id, name, input) = match block {
                ContentBlock::ToolUse { id, name, input } => (id, name, input),
                _ => continue,
            };

            let decision = self.policy_engine.evaluate(name);

            match decision {
                ApprovalDecision::Deny => {
                    results.push(ContentBlock::ToolResult {
                        tool_use_id: id.clone(),
                        content: vec![ToolResultContent::text(format!(
                            "Tool '{}' is denied by policy.",
                            name
                        ))],
                        is_error: Some(true),
                    });
                    continue;
                }

                ApprovalDecision::Ask => {
                    if !self.capabilities.interactive_approval {
                        // No interactive approval available — deny
                        results.push(ContentBlock::ToolResult {
                            tool_use_id: id.clone(),
                            content: vec![ToolResultContent::text(format!(
                                "Tool '{}' requires approval but interactive approval is disabled.",
                                name
                            ))],
                            is_error: Some(true),
                        });
                        continue;
                    }

                    let approval_id = uuid::Uuid::new_v4().to_string();

                    // Send approval request via the approval channel
                    let _ = self.approval_tx.send(SdkNotification::ToolApprovalNeeded {
                        session_id: self.session_id.clone(),
                        approval_id: approval_id.clone(),
                        tool: name.clone(),
                        args: input.clone(),
                        tool_use_id: id.clone(),
                    });

                    // Wait for approval with timeout
                    let timeout_secs = self.policy_engine.timeout_seconds();
                    let approval_result = tokio::time::timeout(
                        std::time::Duration::from_secs(timeout_secs),
                        self.approval_rx.recv(),
                    )
                    .await;

                    match approval_result {
                        Ok(Some((received_id, reason))) => {
                            // Reject mismatched approval IDs — Phase A is single-session,
                            // but this prevents out-of-order approvals from executing wrong tools.
                            if received_id != approval_id {
                                results.push(ContentBlock::ToolResult {
                                    tool_use_id: id.clone(),
                                    content: vec![ToolResultContent::text(format!(
                                        "Approval ID mismatch: expected {}, got {}",
                                        approval_id, received_id
                                    ))],
                                    is_error: Some(true),
                                });
                                continue;
                            }
                            if reason.is_some() {
                                // Denied by host
                                let deny_msg = reason.unwrap_or_else(|| "Denied by host.".into());
                                results.push(ContentBlock::ToolResult {
                                    tool_use_id: id.clone(),
                                    content: vec![ToolResultContent::text(deny_msg)],
                                    is_error: Some(true),
                                });
                                continue;
                            }
                            // Approved — fall through to execute
                        }
                        Ok(None) => {
                            // Channel closed
                            results.push(ContentBlock::ToolResult {
                                tool_use_id: id.clone(),
                                content: vec![ToolResultContent::text(
                                    "Approval channel closed.".to_string(),
                                )],
                                is_error: Some(true),
                            });
                            continue;
                        }
                        Err(_) => {
                            // Timeout
                            results.push(ContentBlock::ToolResult {
                                tool_use_id: id.clone(),
                                content: vec![ToolResultContent::text(format!(
                                    "Tool '{}' approval timed out after {}s.",
                                    name, timeout_secs
                                ))],
                                is_error: Some(true),
                            });
                            continue;
                        }
                    }
                }

                ApprovalDecision::AutoApprove => {
                    // Send ToolStarted notification
                    self.send_notif(SdkNotification::ToolStarted {
                        session_id: self.session_id.clone(),
                        tool: name.clone(),
                        args: input.clone(),
                        tool_use_id: id.clone(),
                    });
                }

                ApprovalDecision::Allow => {
                    // Execute silently — no notification
                }
            }

            // Execute the tool
            let tool_start = Instant::now();
            let tool = self.tools.iter().find(|t| t.name() == name.as_str());

            let output = match tool {
                Some(t) => match t.execute(input.clone(), &ctx).await {
                    Ok(out) => out,
                    Err(e) => ToolOutput::error(format!("Tool error: {e}")),
                },
                None => ToolOutput::error(format!("Unknown tool: {name}")),
            };

            let duration_ms = tool_start.elapsed().as_millis() as u64;
            let success = !output.is_error;

            // Summarize output for notification
            let output_summary = output
                .content
                .iter()
                .map(|c| {
                    let ToolResultContent::Text { text } = c;
                    text.as_str()
                })
                .collect::<Vec<_>>()
                .join("\n");
            let summary_truncated = if output_summary.len() > 500 {
                // Find a safe UTF-8 boundary near 500 bytes
                let mut end = 500;
                while end > 0 && !output_summary.is_char_boundary(end) {
                    end -= 1;
                }
                format!("{}...", &output_summary[..end])
            } else {
                output_summary
            };

            // Send ToolCompleted notification
            self.send_notif(SdkNotification::ToolCompleted {
                session_id: self.session_id.clone(),
                tool: name.clone(),
                tool_use_id: id.clone(),
                success,
                output_summary: summary_truncated,
                duration_ms,
            });

            // Track tool usage
            if !self.tools_used_this_turn.contains(name) {
                self.tools_used_this_turn.push(name.clone());
            }
            self.tools_executed_count += 1;

            // Push result to message history
            results.push(ContentBlock::ToolResult {
                tool_use_id: id.clone(),
                content: output.content,
                is_error: if output.is_error { Some(true) } else { None },
            });
        }

        Ok(results)
    }

    /// Append capability constraints to the system prompt.
    fn inject_capabilities(&self, system: &mut String) {
        let mut constraints: Vec<String> = Vec::new();

        if !self.capabilities.open_browser {
            constraints.push("Do not open a browser or generate browser links.".into());
        }
        if !self.capabilities.play_audio {
            constraints.push("Do not generate audio or attempt to play sounds.".into());
        }
        if !self.capabilities.supports_images {
            constraints.push(
                "The host does not support images. Use text descriptions instead of image output."
                    .into(),
            );
        }
        if let Some(max_size) = self.capabilities.max_file_size_bytes {
            constraints.push(format!("Maximum file size: {} bytes.", max_size));
        }

        if !constraints.is_empty() {
            system.push_str("\n\n<sdk_constraints>\n");
            for c in &constraints {
                system.push_str("- ");
                system.push_str(c);
                system.push('\n');
            }
            system.push_str("</sdk_constraints>");
        }
    }

    /// Retrieve relevant code context from the local RAG index.
    /// Returns a formatted context block, or empty string on any failure.
    fn retrieve_rag_context(&self, user_input: &str) -> String {
        let db = match rag::RagDb::open(&self.config.cwd) {
            Ok(db) => db,
            Err(_) => return String::new(),
        };

        if db.chunk_count().unwrap_or(0) == 0 {
            return String::new();
        }

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

        // Filter by relevance threshold (FTS5 rank is negative; closer to 0 = more relevant)
        let top_rank = results[0].rank;
        let threshold = if top_rank < -5.0 {
            top_rank * 0.3
        } else {
            top_rank * 0.5
        };
        let filtered: Vec<_> = results
            .into_iter()
            .filter(|r| r.rank <= threshold || r.rank <= top_rank * 0.8)
            .take(10)
            .collect();

        if filtered.is_empty() {
            return String::new();
        }

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

    /// Send a notification, ignoring channel errors (host may have disconnected).
    fn send_notif(&self, notif: SdkNotification) {
        let _ = self.notif_tx.send(notif);
    }
}
