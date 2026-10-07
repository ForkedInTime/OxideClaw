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
    policy_engine: Arc<PolicyEngine>,
    capabilities: Capabilities,
    tools_used_this_turn: Vec<String>,
    /// A skill loaded this turn with `disableSkillShellExecution` set: shell
    /// tools are refused until the turn ends. On the session because each
    /// tool round builds a new gate.
    skill_shell_blocked: bool,
    tools_executed_count: u32,
    read_cache: ReadCache,
    notif_tx: mpsc::UnboundedSender<SdkNotification>,
    /// Channel to send approval-needed notifications to the host.
    approval_tx: mpsc::UnboundedSender<SdkNotification>,
    /// Channel to receive approval/deny decisions from the host.
    approval_rx: ApprovalReceiver,
    /// Set by `session/cancel`; checked between model calls and tools.
    cancel: Arc<CancelSignal>,
    /// Sub-agents (`Agent`) report each API response they pay for here.
    child_usage_tx: crate::tools::UsageSink,
    child_usage_rx: mpsc::UnboundedReceiver<(String, Usage)>,
    /// Sub-agent (input, output) tokens recorded since the last CostUpdated.
    child_tokens: (u64, u64),
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
        let client = ApiBackend::new_with_auth(
            &config.model,
            &config.api_key,
            config.auth_is_oauth,
            &config.ollama_host,
        )
        .context("Failed to create API client")?;
        let system_prompt = config.build_system_prompt();
        let session_id = uuid::Uuid::new_v4().to_string();

        let mut cost_tracker = CostTracker::new();
        if let Some(budget) = config.max_budget_usd {
            cost_tracker.set_budget(budget);
        }

        let interactive = capabilities.interactive_approval;
        let (child_usage_tx, child_usage_rx) = mpsc::unbounded_channel();

        Ok(Self {
            session_id,
            config,
            client,
            system_prompt,
            tools,
            messages: Vec::new(),
            cost_tracker,
            policy_engine: Arc::new(PolicyEngine::new(policy, interactive)),
            capabilities,
            tools_used_this_turn: Vec::new(),
            skill_shell_blocked: false,
            tools_executed_count: 0,
            read_cache: new_read_cache(),
            notif_tx,
            approval_rx: Arc::new(tokio::sync::Mutex::new(approval_rx)),
            approval_tx,
            cancel: Arc::new(CancelSignal::default()),
            child_usage_tx,
            child_usage_rx,
            child_tokens: (0, 0),
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
        self.skill_shell_blocked = false;

        // 2. Retrieve RAG context (silently ignore errors). It goes in the
        // user turn: `system` must stay byte-identical for the whole
        // conversation or replayed thinking-block signatures are rejected.
        let rag_context = {
            let cwd = self.config.cwd.clone();
            let q = prompt.clone();
            tokio::task::spawn_blocking(move || Self::retrieve_rag_context(&cwd, &q))
                .await
                .unwrap_or_default()
        };

        // UserPromptSubmit hooks may add context or stop the prompt, as in the TUI.
        let prompt = match self.hooks() {
            Some(h) => {
                let r = crate::hooks::run_user_prompt_hooks(
                    h,
                    &prompt,
                    &self.session_id,
                    &self.config.cwd,
                )
                .await;
                if !r.should_continue {
                    anyhow::bail!(
                        "Prompt not sent — blocked by a userPromptSubmit hook: {}",
                        r.stop_reason.unwrap_or_default()
                    );
                }
                match r.additional_context {
                    Some(extra) => {
                        format!("{prompt}\n\n<additional_context>{extra}</additional_context>")
                    }
                    None => prompt,
                }
            }
            None => prompt,
        };

        // 3. Push user message
        let base = self.messages.len();
        let mut content = vec![ContentBlock::Text { text: prompt }];
        if !rag_context.is_empty() {
            content.push(ContentBlock::Text { text: rag_context });
        }
        self.messages.push(Message {
            role: Role::User,
            content,
        });
        let mut system = self.system_prompt.clone();
        self.inject_capabilities(&mut system);

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

            let max_tokens = self.config.max_tokens_for(&self.config.model);
            let mut request_system = system.clone();
            let (thinking, output_config, betas) = crate::api::thinking::request_knobs(
                &self.config,
                &self.config.model,
                max_tokens,
                &mut request_system,
            );
            let request = MessagesRequest {
                model: self.config.model.clone(),
                max_tokens,
                system: SystemContent::Plain(request_system),
                messages: self.messages.clone(),
                tools: tool_defs,
                stream: None,
                thinking,
                output_config,
                betas,
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
                    r = call => r,
                }
            };
            let response = match response {
                Ok(r) => r,
                Err(e) => {
                    // ACP runs every prompt on this session. Keeping a turn
                    // whose request was rejected (say, too large) would resend
                    // it, and fail the same way, on every later prompt.
                    self.messages.truncate(base);
                    return Err(e.context("API stream call failed"));
                }
            };

            if !turn_text.is_empty() {
                final_text = turn_text;
            }

            // Push assistant message to history (never empty: that is a 400
            // on the next request).
            if !response.content.is_empty() {
                self.messages.push(Message {
                    role: Role::Assistant,
                    content: response.content.clone(),
                });
            }

            // Record cost
            let input_tok = response.usage.input_tokens;
            let output_tok = response.usage.output_tokens;
            turn_input_tokens += input_tok;
            turn_output_tokens += output_tok;
            self.cost_tracker.record_with_cache(
                &self.config.model,
                input_tok,
                output_tok,
                response.usage.cache_read_input_tokens,
                response.usage.cache_creation_input_tokens,
            );

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
            let window = crate::compact::compaction_window(&self.config, None, None);
            let used_pct = ((input_tok as f64 / window as f64) * 100.0).min(100.0) as u8;
            self.send_notif(SdkNotification::ContextHealth {
                session_id: self.session_id.clone(),
                used_pct,
                tokens_used: input_tok,
                tokens_max: window,
                compaction_imminent: input_tok >= crate::compact::thresholds(window).1,
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

                    let (child_in, child_out) = std::mem::take(&mut self.child_tokens);
                    if child_in + child_out > 0 {
                        turn_input_tokens += child_in;
                        turn_output_tokens += child_out;
                        self.send_notif(SdkNotification::CostUpdated {
                            session_id: self.session_id.clone(),
                            turn_cost_usd: self.cost_tracker.total_cost_usd - turn_cost_start,
                            session_total_usd: self.cost_tracker.total_cost_usd,
                            budget_remaining_usd: self.cost_tracker.remaining(),
                            input_tokens: child_in,
                            output_tokens: child_out,
                            model: self.config.model.clone(),
                        });
                    }
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
                Some(StopReason::EndTurn) | Some(StopReason::Other) | None => break,
                Some(StopReason::Refusal) => {
                    self.send_notif(SdkNotification::Error {
                        session_id: self.session_id.clone(),
                        code: "refusal".into(),
                        message: "The model declined this request.".into(),
                    });
                    break;
                }
                Some(StopReason::MaxTokens) | Some(StopReason::ModelContextWindowExceeded) => {
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
        ctx.env = self.config.env.clone();
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
        ctx.usage_sink = Some(self.child_usage_tx.clone());
        ctx.budget_remaining_usd = self.cost_tracker.remaining();
        // The host's policy and approval see only tool names, so the user's
        // `permissions.deny` rules are checked here as well.
        let mut gate = crate::permissions::PermissionGate::bypass_with_deny(
            &self.config.permissions_deny,
            &self.config.cwd,
        );
        // `Agent` children go through the same deny rules and host policy,
        // asking the host when the policy says Ask. Not the bypass gate
        // above: that would let a child run anything the host never saw.
        let child_asker = crate::sdk::approval::SdkPolicyAsker {
            policy: Arc::clone(&self.policy_engine),
            session_id: self.session_id.clone(),
            approval_tx: self.approval_tx.clone(),
            approval_rx: Arc::clone(&self.approval_rx),
            cancel: Arc::clone(&self.cancel),
        };
        ctx.permission_gate = Some(
            crate::permissions::PermissionGate::new(
                crate::permissions::PermissionState::new(false, &[], &self.config.permissions_deny)
                    .with_cwd(&self.config.cwd),
                false,
                Some(Arc::new(child_asker)),
            )
            .with_asker_for_all_tools(),
        );
        if self.skill_shell_blocked {
            gate = gate.with_skill_shell_blocked();
            ctx.permission_gate = ctx
                .permission_gate
                .take()
                .map(|g| g.with_skill_shell_blocked());
        }

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

            // PreToolUse guards run before the approval prompt, so the host is
            // never asked about a call the user's own guard refuses, and before
            // tool/started, so no start goes without a completion.
            if let Some(h) = self.hooks() {
                let args = serde_json::to_string(input).unwrap_or_default();
                let r = crate::hooks::run_pre_tool_hooks(
                    h,
                    name,
                    &args,
                    &self.session_id,
                    &self.config.cwd,
                )
                .await;
                if !r.should_continue {
                    results.push(ContentBlock::ToolResult {
                        tool_use_id: id.clone(),
                        content: vec![ToolResultContent::text(
                            r.stop_reason
                                .unwrap_or_else(|| format!("PreToolUse hook blocked: {name}")),
                        )],
                        is_error: Some(true),
                    });
                    continue;
                }
            }
            if let crate::permissions::GateOutcome::Denied(reason) = gate.decide(name, input).await
            {
                results.push(ContentBlock::ToolResult {
                    tool_use_id: id.clone(),
                    content: vec![ToolResultContent::text(reason)],
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
                    // The guard drops before the tool runs, so an Agent
                    // child can take the receiver for its own prompts.
                    let outcome = await_approval(
                        &mut *self.approval_rx.lock().await,
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
                        // The host announced this call with approval_needed;
                        // without a completion it stays pending forever (an
                        // ACP client keeps its permission dialog open).
                        self.send_notif(SdkNotification::ToolCompleted {
                            session_id: self.session_id.clone(),
                            tool: name.clone(),
                            tool_use_id: id.clone(),
                            success: false,
                            output_summary: text.clone(),
                            duration_ms: 0,
                        });
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
            ctx.cwd = crate::tools::session_cwd(&self.tools, &self.config.cwd);

            // Raced against session/cancel so the tool's future is dropped:
            // that kills a Bash process group and stops an Agent child,
            // which otherwise ran to their timeout or to completion.
            let cancel = Arc::clone(&self.cancel);
            let output = match tool {
                Some(t) => tokio::select! {
                    biased;
                    _ = cancel.cancelled() => {
                        ToolOutput::error("Cancelled by the client while the tool was running.")
                    }
                    r = t.execute(input.clone(), &ctx) => match r {
                        Ok(out) => out,
                        Err(e) => ToolOutput::error(format!("Tool error: {e}")),
                    },
                },
                None => ToolOutput::error(format!("Unknown tool: {name}")),
            };

            // A sub-agent's spend is the session's: it counts toward
            // CostUpdated, TurnCompleted and the budget.
            while let Ok((model, u)) = self.child_usage_rx.try_recv() {
                self.cost_tracker.record_with_cache(
                    &model,
                    u.input_tokens,
                    u.output_tokens,
                    u.cache_read_input_tokens,
                    u.cache_creation_input_tokens,
                );
                self.child_tokens.0 += u.input_tokens;
                self.child_tokens.1 += u.output_tokens;
            }
            ctx.budget_remaining_usd = self.cost_tracker.remaining();

            // Same rule as the TUI and `-p`: once a skill is loaded with
            // disableSkillShellExecution set, no shell for the rest of the
            // turn, including later calls in this round and Agent children.
            if name == "Skill"
                && !output.is_error
                && self.config.disable_skill_shell_execution
                && !self.skill_shell_blocked
            {
                self.skill_shell_blocked = true;
                gate = gate.with_skill_shell_blocked();
                ctx.permission_gate = ctx
                    .permission_gate
                    .take()
                    .map(|g| g.with_skill_shell_blocked());
            }

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
            if let Some(h) = self.hooks() {
                crate::hooks::run_post_tool_hooks(
                    h,
                    name,
                    &output_summary,
                    &self.session_id,
                    &self.config.cwd,
                )
                .await;
            }
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

            // Stored cut: an oversized result would be resent, and 400, on
            // every later request of this session.
            let mut content = output.content;
            for c in &mut content {
                let ToolResultContent::Text { text } = c;
                crate::compact::budget_tool_result(text);
            }
            results.push(ContentBlock::ToolResult {
                tool_use_id: id.clone(),
                content,
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
    fn retrieve_rag_context(cwd: &std::path::Path, user_input: &str) -> String {
        let db = match rag::RagDb::open(cwd) {
            Ok(db) => db,
            Err(_) => return String::new(),
        };

        if db.chunk_count().unwrap_or(0) == 0 {
            return String::new();
        }

        // Only the TUI indexes on its own; without this, print/SDK/ACP turns
        // inject whatever a past TUI run stored, including deleted files and
        // code this session already edited. Incremental, so cheap when idle.
        if let Err(e) = rag::indexer::index_project(&db, cwd, false) {
            debug!("RAG refresh failed: {e}");
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
    /// The user's hooks, unless `disableAllHooks` / `--bare` turned them off.
    fn hooks(&self) -> Option<&crate::settings::HooksConfig> {
        self.config
            .hooks
            .as_ref()
            .filter(|_| !self.config.disable_all_hooks)
    }

    fn send_notif(&self, notif: SdkNotification) {
        let _ = self.notif_tx.send(notif);
    }
}

/// The host's approval replies, shared by the top-level loop and the
/// `Agent` sub-agents it runs.
pub(crate) type ApprovalReceiver =
    Arc<tokio::sync::Mutex<mpsc::UnboundedReceiver<(String, Option<String>)>>>;

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
    // The host picks the timeout; a "never" sentinel such as u64::MAX
    // seconds overflows `Instant + Duration`, a panic that aborts the
    // release build and every session in it.
    let now = tokio::time::Instant::now();
    let deadline = now
        .checked_add(timeout)
        .unwrap_or_else(|| now + std::time::Duration::from_secs(86_400 * 365 * 30));
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
    async fn a_never_timeout_does_not_overflow() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        tx.send(("this-id".into(), None)).unwrap();
        let out = await_approval(&mut rx, "this-id", Duration::from_secs(u64::MAX)).await;
        assert_eq!(out, ApprovalOutcome::Approved);
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

    /// SDK/ACP sessions never indexed, so turn 2's context could not see code
    /// turn 1 wrote; the index must be refreshed before each search.
    #[test]
    fn rag_context_sees_files_added_after_the_index_was_built() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("old.rs"), "fn unrelated_helper() {}\n").unwrap();
        let db = rag::RagDb::open(dir.path()).unwrap();
        rag::indexer::index_project(&db, dir.path(), true).unwrap();
        drop(db);

        std::fs::write(
            dir.path().join("billing.rs"),
            "/// Compute the invoice total.\nfn compute_invoice_total() -> u32 { 0 }\n",
        )
        .unwrap();
        let ctx = SdkSession::retrieve_rag_context(dir.path(), "compute invoice total");
        assert!(ctx.contains("compute_invoice_total"), "{ctx}");
    }

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

#[cfg(test)]
mod hook_tests {
    use super::*;
    use crate::settings::{HookEntry, HooksConfig};
    use std::time::Duration;

    fn session_with_hooks(
        dir: &std::path::Path,
        hooks: HooksConfig,
        policy: Policy,
    ) -> (SdkSession, mpsc::UnboundedReceiver<SdkNotification>) {
        let cfg = crate::config::Config {
            api_key: "sk-ant-test".into(),
            cwd: dir.to_path_buf(),
            hooks: Some(hooks),
            ..Default::default()
        };
        let (ntx, _nrx) = mpsc::unbounded_channel();
        let (atx, arx) = mpsc::unbounded_channel();
        let (_itx, irx) = mpsc::unbounded_channel();
        let s =
            SdkSession::new(cfg, vec![], policy, Capabilities::default(), ntx, atx, irx).unwrap();
        (s, arx)
    }

    fn tool_use(name: &str) -> Vec<ContentBlock> {
        vec![ContentBlock::ToolUse {
            id: "t1".into(),
            name: name.into(),
            input: serde_json::json!({"command": "rm -rf ~"}),
        }]
    }

    fn text(block: &ContentBlock) -> String {
        match block {
            ContentBlock::ToolResult { content, .. } => content
                .iter()
                .map(|c| {
                    let ToolResultContent::Text { text } = c;
                    text.clone()
                })
                .collect(),
            other => panic!("not a tool result: {other:?}"),
        }
    }

    /// `--headless` and `acp` ran no hooks at all, so a preToolUse guard the
    /// user relied on was silently bypassed. It must also refuse before the
    /// host is asked to approve.
    #[tokio::test]
    async fn a_pre_tool_guard_blocks_before_the_host_is_asked() {
        let dir = tempfile::tempdir().unwrap();
        let hooks = HooksConfig {
            pre_tool_use: vec![HookEntry {
                matcher: "Bash".into(),
                command: "echo guarded; exit 2".into(),
            }],
            ..Default::default()
        };
        let (mut s, mut approvals) = session_with_hooks(dir.path(), hooks, Policy::default());
        let out = tokio::time::timeout(
            Duration::from_secs(30),
            s.execute_tools_with_approval(&tool_use("Bash")),
        )
        .await
        .expect("waited on host approval instead of running the guard")
        .unwrap();
        assert_eq!(out.len(), 1);
        assert!(matches!(
            out[0],
            ContentBlock::ToolResult {
                is_error: Some(true),
                ..
            }
        ));
        assert!(text(&out[0]).contains("guarded"), "{}", text(&out[0]));
        assert!(approvals.try_recv().is_err(), "host was asked anyway");
    }

    #[tokio::test]
    async fn post_tool_hooks_run_and_disable_all_hooks_skips_them() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("post.log");
        let hooks = HooksConfig {
            post_tool_use: vec![HookEntry {
                matcher: "*".into(),
                command: format!("echo \"$TOOL_NAME\" >> '{}'", log.display()),
            }],
            ..Default::default()
        };
        let policy = Policy {
            allow: vec!["Ghost".into()],
            ..Default::default()
        };
        let (mut s, _a) = session_with_hooks(dir.path(), hooks.clone(), policy.clone());
        s.execute_tools_with_approval(&tool_use("Ghost"))
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(&log).unwrap().trim(), "Ghost");

        std::fs::remove_file(&log).unwrap();
        let (mut s, _a) = session_with_hooks(dir.path(), hooks, policy);
        s.config.disable_all_hooks = true;
        s.execute_tools_with_approval(&tool_use("Ghost"))
            .await
            .unwrap();
        assert!(!log.exists(), "disableAllHooks must skip hooks");
    }
}

#[cfg(test)]
mod guard_tests {
    use super::*;
    use crate::tools::Tool;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Stands in for Bash so a test can see whether it ran without
    /// executing anything.
    struct FakeBash(AtomicUsize);
    #[async_trait]
    impl Tool for FakeBash {
        fn name(&self) -> &str {
            "Bash"
        }
        fn description(&self) -> &str {
            "test"
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        async fn execute(&self, input: serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput> {
            self.0.fetch_add(1, Ordering::SeqCst);
            if input["command"] == "spend" {
                // What an Agent child reports for one response.
                let usage = Usage {
                    input_tokens: 1_000,
                    output_tokens: 1_000_000,
                    ..Default::default()
                };
                let sink = ctx
                    .usage_sink
                    .as_ref()
                    .expect("no usage sink on the context");
                sink.send(("claude-sonnet-5".into(), usage)).unwrap();
                return Ok(ToolOutput::success("spent"));
            }
            if input["command"] == "huge" {
                return Ok(ToolOutput::success("x".repeat(3_000_000)));
            }
            if input["command"] == "hang" {
                std::future::pending::<()>().await;
            }
            Ok(ToolOutput::success("ran"))
        }
    }

    fn session(cfg: Config) -> (SdkSession, Arc<FakeBash>) {
        let bash = Arc::new(FakeBash(AtomicUsize::new(0)));
        let (ntx, _nrx) = mpsc::unbounded_channel();
        let (atx, _arx) = mpsc::unbounded_channel();
        let (_itx, irx) = mpsc::unbounded_channel();
        let policy = Policy {
            allow: vec!["Bash".into()],
            ..Policy::default()
        };
        let s = SdkSession::new(
            cfg,
            vec![bash.clone()],
            policy,
            Capabilities::default(),
            ntx,
            atx,
            irx,
        )
        .unwrap();
        (s, bash)
    }

    fn cfg(dir: &std::path::Path) -> Config {
        Config {
            api_key: "sk-ant-test".into(),
            cwd: dir.to_path_buf(),
            ..Default::default()
        }
    }

    fn call(command: &str) -> Vec<ContentBlock> {
        vec![ContentBlock::ToolUse {
            id: "t1".into(),
            name: "Bash".into(),
            input: serde_json::json!({ "command": command }),
        }]
    }

    fn result_text(r: &[ContentBlock]) -> String {
        match &r[0] {
            ContentBlock::ToolResult { content, .. } => content
                .iter()
                .map(|c| {
                    let ToolResultContent::Text { text } = c;
                    text.as_str()
                })
                .collect(),
            other => panic!("{other:?}"),
        }
    }

    fn is_error(r: &[ContentBlock]) -> bool {
        matches!(
            &r[0],
            ContentBlock::ToolResult {
                is_error: Some(true),
                ..
            }
        )
    }

    struct FakeSkill;
    #[async_trait]
    impl Tool for FakeSkill {
        fn name(&self) -> &str {
            "Skill"
        }
        fn description(&self) -> &str {
            "test"
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        async fn execute(&self, _: serde_json::Value, _: &ToolContext) -> Result<ToolOutput> {
            Ok(ToolOutput::success("skill loaded"))
        }
    }

    /// disableSkillShellExecution was wired into the TUI and `-p` only: a
    /// skill loaded in a --headless or ACP session could still run Bash.
    #[tokio::test]
    async fn a_skill_turn_refuses_shell_in_sdk_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = cfg(dir.path());
        c.disable_skill_shell_execution = true;
        let (mut s, bash) = session(c);
        s.tools.push(Arc::new(FakeSkill));
        s.policy_engine = Arc::new(PolicyEngine::new(
            Policy {
                allow: vec!["Bash".into(), "Skill".into()],
                ..Policy::default()
            },
            false,
        ));
        let calls = vec![
            ContentBlock::ToolUse {
                id: "t1".into(),
                name: "Skill".into(),
                input: serde_json::json!({}),
            },
            ContentBlock::ToolUse {
                id: "t2".into(),
                name: "Bash".into(),
                input: serde_json::json!({"command": "ls"}),
            },
        ];
        let r = s.execute_tools_with_approval(&calls).await.unwrap();
        assert!(is_error(&r[1..]), "{r:?}");
        // Still blocked in a later round of the same turn.
        let r = s.execute_tools_with_approval(&call("ls")).await.unwrap();
        assert!(is_error(&r), "{r:?}");
        assert_eq!(
            bash.0.load(Ordering::SeqCst),
            0,
            "shell ran in a skill turn"
        );
    }

    /// The host's policy allowing Bash must not waive the user's own
    /// `permissions.deny` rules.
    #[tokio::test]
    async fn a_settings_deny_rule_beats_a_host_allow() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = cfg(dir.path());
        c.permissions_deny = vec!["Bash(git push:*)".into()];
        let (mut s, bash) = session(c);

        let r = s
            .execute_tools_with_approval(&call("git push origin main"))
            .await
            .unwrap();
        assert!(is_error(&r), "{r:?}");
        assert_eq!(bash.0.load(Ordering::SeqCst), 0, "denied call must not run");

        let r = s.execute_tools_with_approval(&call("ls")).await.unwrap();
        assert!(!is_error(&r), "{r:?}");
        assert_eq!(bash.0.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn pre_tool_hooks_can_block_and_post_tool_hooks_run() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("post-ran");
        let mut c = cfg(dir.path());
        c.hooks = Some(crate::settings::HooksConfig {
            pre_tool_use: vec![crate::settings::HookEntry {
                matcher: "Bash".into(),
                command: "case \"$TOOL_INPUT\" in *rm*) exit 2;; esac".into(),
            }],
            post_tool_use: vec![crate::settings::HookEntry {
                matcher: "Bash".into(),
                command: format!("touch '{}'", marker.display()),
            }],
            ..Default::default()
        });
        let (mut s, bash) = session(c);

        let r = s
            .execute_tools_with_approval(&call("rm -rf x"))
            .await
            .unwrap();
        assert!(is_error(&r), "{r:?}");
        assert_eq!(bash.0.load(Ordering::SeqCst), 0, "hook-blocked call ran");
        assert!(!marker.exists());

        let r = s.execute_tools_with_approval(&call("ls")).await.unwrap();
        assert!(!is_error(&r), "{r:?}");
        assert!(marker.exists(), "postToolUse hook did not run");
    }

    /// A multi-megabyte Read/Grep result was stored whole, so every later
    /// request in the session carried it and was rejected.
    #[tokio::test]
    async fn oversized_tool_results_are_stored_cut() {
        let dir = tempfile::tempdir().unwrap();
        let (mut s, _) = session(cfg(dir.path()));
        let r = s.execute_tools_with_approval(&call("huge")).await.unwrap();
        let text = result_text(&r);
        assert!(text.len() < crate::compact::TOOL_RESULT_MAX_CHARS + 200);
        assert!(text.contains("output truncated"));
    }

    /// A rejected request left its turn in the history, so in ACP, where
    /// every prompt reuses the session, each later prompt resent it and
    /// failed the same way.
    #[tokio::test]
    async fn a_failed_request_does_not_poison_the_session() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = [0u8; 65536];
                let _ = sock.read(&mut buf).await;
                let body = r#"{"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long"}}"#;
                let resp = format!(
                    "HTTP/1.1 400 Bad Request\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.shutdown().await;
            }
        });
        let dir = tempfile::tempdir().unwrap();
        let (mut s, _) = session(cfg(dir.path()));
        let mut c = crate::api::ClaudeClient::new("sk-ant-test").unwrap();
        c.set_base_url_for_test(format!("http://{addr}"));
        s.client = ApiBackend::Anthropic(c);

        assert!(s.execute_turn("hi".into()).await.is_err());
        assert!(s.messages.is_empty(), "the rejected turn stayed in history");
    }

    /// The SDK sidecar and ACP sent `thinking: None, output_config: None`
    /// whatever --thinking / --effort and settings.json said.
    #[tokio::test]
    async fn requests_carry_the_configured_thinking_and_effort() {
        use crate::query_engine::scripted_api_tests::{serve, sse};
        let (url, seen) = serve(vec![sse(
            &[serde_json::json!({"type":"text","text":"ok"})],
            "end_turn",
        )])
        .await;
        let dir = tempfile::tempdir().unwrap();
        let mut c = cfg(dir.path());
        c.model = "claude-opus-5".into();
        c.effort = Some("high".into());
        c.thinking_budget_tokens = Some(0);
        let (mut s, _) = session(c);
        let mut client = crate::api::ClaudeClient::new("sk-ant-test").unwrap();
        client.set_base_url_for_test(url);
        s.client = ApiBackend::Anthropic(client);

        s.execute_turn("hi".into()).await.unwrap();

        let body: serde_json::Value = serde_json::from_str(&seen.lock().unwrap()[0]).unwrap();
        assert_eq!(body["thinking"], serde_json::json!({"type":"disabled"}));
        assert_eq!(body["output_config"]["effort"], "high");
    }

    /// An approval that timed out sent no notification, so the host's
    /// call (and ACP's permission dialog) stayed pending after the model
    /// had moved on.
    #[tokio::test]
    async fn an_approval_timeout_completes_the_call_as_failed() {
        let dir = tempfile::tempdir().unwrap();
        let bash = Arc::new(FakeBash(AtomicUsize::new(0)));
        let (ntx, mut nrx) = mpsc::unbounded_channel();
        let (atx, mut arx) = mpsc::unbounded_channel();
        let (_itx, irx) = mpsc::unbounded_channel();
        let policy = Policy {
            approval_timeout_seconds: 1,
            ..Policy::default()
        };
        let mut s = SdkSession::new(
            cfg(dir.path()),
            vec![bash.clone()],
            policy,
            Capabilities::default(),
            ntx,
            atx,
            irx,
        )
        .unwrap();
        let r = s.execute_tools_with_approval(&call("ls")).await.unwrap();
        assert!(is_error(&r), "{r:?}");
        assert_eq!(bash.0.load(Ordering::SeqCst), 0);
        assert!(matches!(
            arx.try_recv(),
            Ok(SdkNotification::ToolApprovalNeeded { .. })
        ));
        let mut completed = None;
        while let Ok(n) = nrx.try_recv() {
            if let SdkNotification::ToolCompleted {
                tool_use_id,
                success,
                output_summary,
                ..
            } = n
            {
                completed = Some((tool_use_id, success, output_summary));
            }
        }
        let (tool_use_id, success, summary) = completed.expect("no tool/completed");
        assert_eq!(tool_use_id, "t1");
        assert!(!success);
        assert!(summary.contains("timed out"), "{summary}");
    }

    /// session/cancel was checked only before each tool, so a running Bash
    /// command went on to its timeout and an Agent child to completion.
    #[tokio::test]
    async fn cancel_interrupts_a_running_tool() {
        let dir = tempfile::tempdir().unwrap();
        let (mut s, bash) = session(cfg(dir.path()));
        let cancel = s.cancel_signal();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            cancel.cancel();
        });
        let r = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            s.execute_tools_with_approval(&call("hang")),
        )
        .await
        .expect("the tool kept running after cancel")
        .unwrap();
        assert_eq!(bash.0.load(Ordering::SeqCst), 1, "the tool had started");
        assert!(is_error(&r), "{r:?}");
        assert!(result_text(&r).contains("Cancelled"), "{r:?}");
    }

    /// Sub-agent spend never reached the SDK's tracker, so CostUpdated,
    /// TurnCompleted and the budget all left it out.
    #[tokio::test]
    async fn sub_agent_spend_counts_toward_the_session() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = cfg(dir.path());
        c.max_budget_usd = Some(1.0);
        let (mut s, _) = session(c);
        s.execute_tools_with_approval(&call("spend")).await.unwrap();
        assert!(s.cost_tracker.total_cost_usd > 1.0);
        assert!(s.cost_tracker.over_budget());
        assert_eq!(s.child_tokens, (1_000, 1_000_000));
    }
}

#[cfg(test)]
mod subagent_gate_tests {
    use super::*;
    use crate::permissions::GateOutcome;
    use serde_json::{Value, json};
    use std::sync::Mutex;
    use std::time::Duration;

    /// Stands in for `Agent`: puts each child call through the gate the
    /// session published, as the real sub-engine does.
    struct FakeAgent {
        calls: Vec<(&'static str, Value)>,
        outcomes: Arc<Mutex<Vec<GateOutcome>>>,
    }

    #[async_trait::async_trait]
    impl crate::tools::Tool for FakeAgent {
        fn name(&self) -> &str {
            "Agent"
        }
        fn description(&self) -> &str {
            ""
        }
        fn input_schema(&self) -> Value {
            json!({})
        }
        async fn execute(&self, _: Value, ctx: &ToolContext) -> Result<ToolOutput> {
            let Some(gate) = &ctx.permission_gate else {
                return Ok(ToolOutput::error("no gate published"));
            };
            for (tool, input) in &self.calls {
                let out = gate.decide(tool, input).await;
                self.outcomes.lock().unwrap().push(out);
            }
            Ok(ToolOutput::success("done"))
        }
    }

    type Host = (
        mpsc::UnboundedReceiver<SdkNotification>,
        mpsc::UnboundedSender<(String, Option<String>)>,
    );

    fn session(
        policy: Policy,
        interactive: bool,
        deny: &[&str],
        calls: Vec<(&'static str, Value)>,
    ) -> (SdkSession, Arc<Mutex<Vec<GateOutcome>>>, Host) {
        let dir = std::env::temp_dir();
        let cfg = crate::config::Config {
            api_key: "sk-ant-test".into(),
            cwd: dir,
            permissions_deny: deny.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        };
        let outcomes = Arc::new(Mutex::new(Vec::new()));
        let agent: DynTool = Arc::new(FakeAgent {
            calls,
            outcomes: Arc::clone(&outcomes),
        });
        let (ntx, _nrx) = mpsc::unbounded_channel();
        let (atx, arx) = mpsc::unbounded_channel();
        let (itx, irx) = mpsc::unbounded_channel();
        let caps = Capabilities {
            interactive_approval: interactive,
            ..Default::default()
        };
        let s = SdkSession::new(cfg, vec![agent], policy, caps, ntx, atx, irx).unwrap();
        (s, outcomes, (arx, itx))
    }

    fn agent_call() -> Vec<ContentBlock> {
        vec![ContentBlock::ToolUse {
            id: "toolu_agent".into(),
            name: "Agent".into(),
            input: json!({"prompt": "go"}),
        }]
    }

    /// A child must answer to the host's policy and the user's deny rules,
    /// not to the headless gate (which let Read/WebFetch through unasked
    /// and refused Bash outright whatever the host allowed).
    #[tokio::test]
    async fn agent_children_follow_the_host_policy_and_deny_rules() {
        let policy = Policy {
            allow: vec!["Agent".into(), "Read".into(), "Bash".into()],
            deny: vec!["WebFetch".into()],
            ..Default::default()
        };
        let (mut s, outcomes, _host) = session(
            policy,
            false,
            &["Read(./secret.txt)"],
            vec![
                ("Bash", json!({"command": "echo hi"})),
                ("Read", json!({"file_path": "notes.txt"})),
                ("Read", json!({"file_path": "secret.txt"})),
                ("WebFetch", json!({"url": "https://example.com"})),
                ("Grep", json!({"pattern": "x"})),
            ],
        );
        s.execute_tools_with_approval(&agent_call()).await.unwrap();
        let got = outcomes.lock().unwrap();
        let allowed: Vec<bool> = got.iter().map(|o| *o == GateOutcome::Allowed).collect();
        // Bash and Read are host-allowed; secret.txt is a settings deny;
        // WebFetch is a policy deny; Grep is unlisted with no host to ask.
        assert_eq!(allowed, vec![true, true, false, false, false], "{got:?}");
    }

    /// ACP's default policy asks for everything: a child's Edit must reach
    /// the editor as an approval request, and the parent's held receiver
    /// must not deadlock it.
    #[tokio::test]
    async fn an_agent_child_asks_the_host_when_the_policy_says_ask() {
        let (mut s, outcomes, (mut approvals, replies)) = session(
            Policy::default(),
            true,
            &[],
            vec![
                ("Edit", json!({"file_path": "a.rs"})),
                ("Bash", json!({"command": "rm -rf build"})),
            ],
        );
        let host = tokio::spawn(async move {
            let mut asked = Vec::new();
            let mut sub_ids = Vec::new();
            let mut completed = Vec::new();
            while let Some(n) = approvals.recv().await {
                match n {
                    SdkNotification::ToolApprovalNeeded {
                        approval_id,
                        tool,
                        tool_use_id,
                        ..
                    } => {
                        // Approve the Agent and the Edit, refuse the Bash.
                        let deny = (tool == "Bash").then(|| "no".to_string());
                        if tool_use_id.starts_with("subagent-") {
                            sub_ids.push(tool_use_id);
                        }
                        asked.push(tool);
                        let _ = replies.send((approval_id, deny));
                    }
                    SdkNotification::ToolCompleted {
                        tool_use_id,
                        success,
                        ..
                    } => completed.push((tool_use_id, success)),
                    _ => {}
                }
            }
            (asked, sub_ids, completed)
        });
        tokio::time::timeout(
            Duration::from_secs(10),
            s.execute_tools_with_approval(&agent_call()),
        )
        .await
        .expect("child approval must not deadlock")
        .unwrap();
        drop(s);
        let (asked, sub_ids, completed) = host.await.unwrap();
        assert_eq!(asked, vec!["Agent", "Edit", "Bash"]);
        // Each made-up sub-agent id is closed, or an ACP client shows the
        // call spinning forever.
        assert_eq!(
            completed,
            vec![(sub_ids[0].clone(), true), (sub_ids[1].clone(), false)]
        );
        let got = outcomes.lock().unwrap();
        assert_eq!(got[0], GateOutcome::Allowed);
        assert!(matches!(got[1], GateOutcome::Denied(_)), "{got:?}");
    }
}
