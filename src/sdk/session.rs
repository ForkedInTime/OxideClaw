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
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::mpsc;
use tracing::debug;

/// Maximum context window tokens (200K for Claude).  Used for health estimates.
const MAX_CONTEXT_TOKENS: u64 = 200_000;

/// Why a turn stopped. Maps onto ACP stop reasons and SDK notifications.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnEnd {
    EndTurn,
    MaxTokens,
    MaxTurns,
    BudgetExceeded,
    Cancelled,
}

/// A cancellation flag that can also wake a waiting API stream.
#[derive(Debug, Default)]
pub struct CancelSignal {
    flag: std::sync::atomic::AtomicBool,
    notify: tokio::sync::Notify,
}

impl CancelSignal {
    pub fn cancel(&self) {
        self.flag.store(true, std::sync::atomic::Ordering::SeqCst);
        // `notify_one` stores a permit when nobody is waiting yet, so a
        // cancel that lands between a flag check and `notified()` still wakes.
        self.notify.notify_one();
    }

    pub fn is_cancelled(&self) -> bool {
        self.flag.load(std::sync::atomic::Ordering::SeqCst)
    }

    pub fn reset(&self) {
        self.flag.store(false, std::sync::atomic::Ordering::SeqCst);
    }

    /// Resolves once `cancel` has been called (immediately if it already was).
    pub async fn cancelled(&self) {
        loop {
            if self.is_cancelled() {
                return;
            }
            self.notify.notified().await;
        }
    }
}

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
    /// Set by `session/cancel`; checked between model calls and tools.
    cancel: Arc<CancelSignal>,
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
        let client = ApiBackend::from_config(&config).context("Failed to create API client")?;
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
            cancel: Arc::new(CancelSignal::default()),
        })
    }

    /// The model this session is configured to use.
    pub fn config_model(&self) -> &str {
        &self.config.model
    }

    /// Execute a full agentic turn: prompt → stream → tool loop → complete.
    /// Handle used to cancel a running turn from another task.
    pub fn cancel_signal(&self) -> Arc<CancelSignal> {
        Arc::clone(&self.cancel)
    }

    pub async fn execute_turn(&mut self, prompt: String) -> Result<TurnEnd> {
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
        let mut end = TurnEnd::EndTurn;

        loop {
            loop_turn += 1;
            if self.cancel.is_cancelled() {
                end = TurnEnd::Cancelled;
                break;
            }
            if loop_turn > max_turns {
                self.send_notif(SdkNotification::Error {
                    session_id: self.session_id.clone(),
                    code: "max_turns".into(),
                    message: format!("Stopped after {max_turns} agentic turns."),
                });
                end = TurnEnd::MaxTurns;
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
                output_config: None,
                betas: self.config.extra_betas.clone(),
                session_id: Some(self.session_id.clone()),
            };

            // Stream the response — callback sends MessageDelta notifications
            let notif_tx = self.notif_tx.clone();
            let sid = self.session_id.clone();
            let mut turn_text = String::new();

            let cancel = Arc::clone(&self.cancel);
            let response = {
                let call = self.client.messages_stream(request, |chunk| {
                    turn_text.push_str(chunk);
                    let _ = notif_tx.send(SdkNotification::MessageDelta {
                        session_id: sid.clone(),
                        content: chunk.to_string(),
                    });
                });
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => {
                        end = TurnEnd::Cancelled;
                        break;
                    }
                    r = call => r.context("API stream call failed")?,
                }
            };

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
                end = TurnEnd::BudgetExceeded;
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
                    end = TurnEnd::MaxTokens;
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

        Ok(end)
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
        ctx.live_auth = Some(self.config.auth.clone());
        ctx.live_ollama_host = Some(self.config.ollama_host.clone());

        let mut results = Vec::new();

        for block in content {
            let (id, name, input) = match block {
                ContentBlock::ToolUse { id, name, input } => (id, name, input),
                _ => continue,
            };

            if self.cancel.is_cancelled() {
                results.push(ContentBlock::ToolResult {
                    tool_use_id: id.clone(),
                    content: vec![ToolResultContent::text(
                        "Cancelled by the client before this tool ran.",
                    )],
                    is_error: Some(true),
                });
                continue;
            }

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

                    // Wait for the matching reply; stale replies to earlier
                    // prompts are skipped rather than treated as this answer.
                    let timeout_secs = self.policy_engine.timeout_seconds();
                    let outcome = await_approval(
                        &mut self.approval_rx,
                        &approval_id,
                        std::time::Duration::from_secs(timeout_secs),
                    )
                    .await;
                    let deny_text = match outcome {
                        ApprovalOutcome::Approved => None,
                        ApprovalOutcome::Denied(reason) => Some(if reason.is_empty() {
                            "Denied by host.".to_string()
                        } else {
                            reason
                        }),
                        ApprovalOutcome::Closed => Some("Approval channel closed.".to_string()),
                        ApprovalOutcome::TimedOut => Some(format!(
                            "Tool '{}' approval timed out after {}s.",
                            name, timeout_secs
                        )),
                    };
                    if let Some(text) = deny_text {
                        results.push(ContentBlock::ToolResult {
                            tool_use_id: id.clone(),
                            content: vec![ToolResultContent::text(text)],
                            is_error: Some(true),
                        });
                        continue;
                    }
                    // Approved — fall through to execute
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

/// Outcome of waiting for the host's answer to one approval request.
#[derive(Debug, PartialEq)]
pub(crate) enum ApprovalOutcome {
    Approved,
    Denied(String),
    /// Host went away.
    Closed,
    TimedOut,
}

/// Wait for the reply matching `approval_id`. Replies for *other* ids
/// (late answers to earlier prompts) are discarded, not treated as the
/// answer to this one — previously one stale reply denied the current
/// tool and left the real answer queued for the next prompt, cascading.
pub(crate) async fn await_approval(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<(String, Option<String>)>,
    approval_id: &str,
    timeout: std::time::Duration,
) -> ApprovalOutcome {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        match tokio::time::timeout_at(deadline, rx.recv()).await {
            Ok(Some((received_id, reason))) => {
                if received_id != approval_id {
                    tracing::debug!("ignoring stale approval reply for {received_id}");
                    continue;
                }
                return match reason {
                    Some(r) => ApprovalOutcome::Denied(r),
                    None => ApprovalOutcome::Approved,
                };
            }
            Ok(None) => return ApprovalOutcome::Closed,
            Err(_) => return ApprovalOutcome::TimedOut,
        }
    }
}

#[cfg(test)]
mod approval_wait_tests {
    use super::{ApprovalOutcome, await_approval};
    use std::time::Duration;

    #[tokio::test]
    async fn a_stale_reply_is_skipped_and_the_matching_one_wins() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        tx.send(("old-id".into(), None)).unwrap();
        tx.send(("this-id".into(), None)).unwrap();
        let out = await_approval(&mut rx, "this-id", Duration::from_secs(2)).await;
        assert_eq!(out, ApprovalOutcome::Approved);
    }

    #[tokio::test]
    async fn a_denial_carries_the_hosts_reason() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        tx.send(("this-id".into(), Some("nope".into()))).unwrap();
        let out = await_approval(&mut rx, "this-id", Duration::from_secs(2)).await;
        assert_eq!(out, ApprovalOutcome::Denied("nope".into()));
    }

    #[tokio::test]
    async fn only_stale_replies_still_time_out() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        tx.send(("old-id".into(), None)).unwrap();
        let out = await_approval(&mut rx, "this-id", Duration::from_millis(200)).await;
        assert_eq!(out, ApprovalOutcome::TimedOut);
    }

    #[tokio::test]
    async fn a_dropped_host_is_reported_as_closed() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(String, Option<String>)>();
        drop(tx);
        let out = await_approval(&mut rx, "this-id", Duration::from_secs(2)).await;
        assert_eq!(out, ApprovalOutcome::Closed);
    }
}

#[cfg(test)]
mod cancel_tests {
    use super::*;
    use std::time::Duration;

    fn offline_session() -> (SdkSession, mpsc::UnboundedReceiver<SdkNotification>) {
        let cfg = crate::config::Config {
            api_key: "sk-ant-test".into(),
            cwd: std::env::temp_dir(),
            ..Default::default()
        };
        let (ntx, nrx) = mpsc::unbounded_channel();
        let (atx, _arx) = mpsc::unbounded_channel();
        let (_itx, irx) = mpsc::unbounded_channel();
        let s = SdkSession::new(
            cfg,
            vec![],
            Policy::default(),
            Capabilities::default(),
            ntx,
            atx,
            irx,
        )
        .unwrap();
        (s, nrx)
    }

    /// A cancel that lands before the first model call must end the turn
    /// as `Cancelled` without touching the network (the fake key would
    /// otherwise surface as an API error, not `Ok`).
    #[tokio::test]
    async fn a_pre_cancelled_turn_ends_cancelled_before_calling_the_api() {
        let (mut s, _rx) = offline_session();
        s.cancel_signal().cancel();
        let end = tokio::time::timeout(Duration::from_secs(2), s.execute_turn("hi".into()))
            .await
            .expect("must not hang")
            .expect("must not error");
        assert_eq!(end, TurnEnd::Cancelled);
    }

    #[tokio::test]
    async fn cancel_signal_wakes_a_waiter_and_resets() {
        let sig = Arc::new(CancelSignal::default());
        assert!(!sig.is_cancelled());
        let waiter = {
            let sig = Arc::clone(&sig);
            tokio::spawn(async move { sig.cancelled().await })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        sig.cancel();
        tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .expect("waiter woke")
            .unwrap();
        assert!(sig.is_cancelled());
        tokio::time::timeout(Duration::from_millis(100), sig.cancelled())
            .await
            .expect("immediate");
        sig.reset();
        assert!(!sig.is_cancelled());
    }
}
