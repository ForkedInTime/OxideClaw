//! The per-turn API task: streaming, tool loop, auto-fix, checkpoints.
//! Split out of `tui/run.rs` mechanically — no behaviour change.

use super::*;

/// Maximum number of tool-use→response cycles per user turn.
/// Prevents runaway loops where Claude repeatedly calls tools without finishing.
pub(super) const MAX_TOOL_ITERATIONS: u32 = 50;

/// Destructive tools blocked when plan mode is active. PowerShell runs
/// commands like Bash; Agent spawns a sub-agent with its own (unblocked)
/// tools; the browser actions click and type on live sites; MCP tools can
/// do anything their server does and do not say whether they write;
/// ExitWorktree deletes the worktree directory.
pub(super) const PLAN_MODE_BLOCKED_TOOLS: &[&str] = &[
    "Bash",
    "PowerShell",
    "Write",
    "Edit",
    "MultiEdit",
    "NotebookEdit",
    "MemoryWrite",
    "EnterWorktree",
    "ExitWorktree",
    "Agent",
    "browser_click",
    "browser_fill",
    "browser_press_key",
    "mcp__*",
];

/// Owned bundle handed to `run_api_task` when a user turn kicks off a new
/// conversation-streaming task. Exists to stay under the clippy argument cap;
/// the function destructures it immediately.
pub(super) struct ApiTask {
    pub(super) client: ApiBackend,
    pub(super) tools: Vec<DynTool>,
    pub(super) messages: Vec<Message>,
    pub(super) config: Config,
    pub(super) perm_state: PermissionState,
    pub(super) system_prompt: String,
    pub(super) tx: mpsc::UnboundedSender<AppEvent>,
    pub(super) plan_mode: bool,
    /// A `/skill` turn with `disableSkillShellExecution` set: shell tools are
    /// refused from the first call. A `Skill` tool call sets the same block
    /// mid-turn.
    pub(super) skill_no_shell: bool,
    pub(super) session_id: String,
    /// What is left of the `/budget` cap when the turn starts.
    pub(super) budget_remaining_usd: Option<f64>,
    /// Where the task publishes its history as it goes; the spawner keeps
    /// the other end in `App::turn_history`.
    pub(super) history: TurnHistory,
}

/// Loop detection: the same call getting the same result several times in
/// a row is a model stuck, not progress. Repeating a call alone is normal
/// (edit, `cargo test`, edit, `cargo test`), so the result counts too.
#[derive(Default)]
struct LoopGuard {
    last: Option<(String, u64, bool)>,
    streak: usize,
}

const LOOP_THRESHOLD: usize = 3;

impl LoopGuard {
    /// Record a finished call; returns how many times in a row it has now
    /// run with these arguments and this result.
    fn record(&mut self, name: &str, args: &str, result: &str, is_error: bool) -> usize {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        args.hash(&mut hasher);
        result.hash(&mut hasher);
        let sig = (name.to_string(), hasher.finish(), is_error);
        if self.last.as_ref() == Some(&sig) {
            self.streak += 1;
        } else {
            self.last = Some(sig);
            self.streak = 1;
        }
        self.streak
    }
}

/// Publish the turn's history so far: `messages` plus the results of the
/// tool round in progress.
fn publish_history(history: &TurnHistory, messages: &[Message], results: &[ContentBlock]) {
    let mut h = messages.to_vec();
    if !results.is_empty() {
        h.push(Message {
            role: Role::User,
            content: results.to_vec(),
        });
    }
    if let Ok(mut g) = history.lock() {
        *g = h;
    }
}

/// Every tool_use needs a tool_result in the next message or the next
/// request is rejected. A turn cut short mid-round leaves some unanswered:
/// the tools that never ran or never finished.
fn close_dangling_tool_uses(messages: &mut Vec<Message>) {
    let Some(i) = messages.iter().rposition(|m| m.role == Role::Assistant) else {
        return;
    };
    let ids: Vec<String> = messages[i]
        .content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::ToolUse { id, .. } => Some(id.clone()),
            _ => None,
        })
        .collect();
    if ids.is_empty() {
        return;
    }
    if i + 1 == messages.len() {
        messages.push(Message {
            role: Role::User,
            content: Vec::new(),
        });
    }
    let results = &mut messages[i + 1].content;
    for id in ids {
        let answered = results.iter().any(
            |b| matches!(b, ContentBlock::ToolResult { tool_use_id, .. } if *tool_use_id == id),
        );
        if !answered {
            results.push(ContentBlock::ToolResult {
                tool_use_id: id,
                content: vec![ToolResultContent::text(
                    "Not completed: the turn was stopped before this tool finished.",
                )],
                is_error: Some(true),
            });
        }
    }
}

/// End the turn on a detected loop like any other: keep its history and
/// answer every tool_use, so the user can steer and the next request is
/// valid. `results` must already hold the looping call's result.
fn end_turn_on_loop(
    tx: &mpsc::UnboundedSender<AppEvent>,
    messages: &mut Vec<Message>,
    results: &mut Vec<ContentBlock>,
    usage: &crate::api::types::Usage,
    model: &str,
    name: &str,
    streak: usize,
) {
    messages.push(Message {
        role: Role::User,
        content: std::mem::take(results),
    });
    close_dangling_tool_uses(messages);
    let _ = tx.send(AppEvent::SystemMessage(format!(
        "Loop detected: '{name}' returned the same output {streak} times in a row — paused. \
         Send a message to continue."
    )));
    let _ = tx.send(AppEvent::Done {
        tokens_in: usage.input_tokens,
        tokens_out: usage.output_tokens,
        cache_read: usage.cache_read_input_tokens,
        cache_write: usage.cache_creation_input_tokens,
        messages: messages.clone(),
        model_used: model.to_string(),
    });
}

/// The history to continue from after a turn ended without `Done`, with
/// every unanswered tool_use closed, and whether the session file must be
/// rewritten rather than appended to: when the turn compacted and that
/// event is still queued, what is on disk is no prefix of it. `None` when
/// the turn published nothing (it failed on its first request).
fn recover_turn_history(
    history: &TurnHistory,
    current: &[Message],
    saved_count: usize,
) -> Option<(Vec<Message>, bool)> {
    let mut partial = std::mem::take(&mut *history.lock().ok()?);
    if partial.is_empty() {
        return None;
    }
    close_dangling_tool_uses(&mut partial);
    let saved = &current[..saved_count.min(current.len())];
    let rewrite = !partial.starts_with(saved);
    Some((partial, rewrite))
}

/// Take over the history of a turn that ended without `Done` (Esc, /budget,
/// quit, an API error): the tools it ran already changed files, so dropping
/// their calls and results would leave the model and the session file
/// without any record of that work.
pub(super) async fn adopt_turn_history(
    history: &TurnHistory,
    messages: &mut Vec<Message>,
    saved_count: &mut usize,
    session: &mut Session,
    persist: bool,
) {
    let Some((partial, rewrite)) = recover_turn_history(history, messages, *saved_count) else {
        return;
    };
    if persist {
        if rewrite {
            let _ = session.overwrite(&partial).await;
            *saved_count = partial.len();
        } else if session
            .append(&partial[(*saved_count).min(partial.len())..])
            .await
            .is_ok()
        {
            // A failed append leaves the count, so the next turn retries it.
            *saved_count = partial.len();
        }
    }
    *messages = partial;
}

pub(super) async fn run_api_task(task: ApiTask) {
    let ApiTask {
        mut client,
        tools,
        mut messages,
        config,
        perm_state,
        system_prompt,
        tx,
        plan_mode,
        skill_no_shell,
        session_id,
        budget_remaining_usd,
        history,
    } = task;
    let session_id = session_id.as_str();
    // Surface the client's retry backoff in the transcript. Without this a
    // rate-limited turn sits on a spinner for up to a minute with no
    // explanation and reads as a freeze.
    {
        let notice_tx = tx.clone();
        client.set_retry_notifier(std::sync::Arc::new(
            move |n: &crate::api::retry::RetryNotice| {
                let _ = notice_tx.send(AppEvent::SystemMessage(n.message()));
            },
        ));
    }
    // This task's own spend, so a sub-agent is capped at what is left of
    // the budget rather than given all of it again.
    let mut task_cost = crate::cost::CostTracker::new();
    if let Some(left) = budget_remaining_usd {
        task_cost.set_budget(left);
    }
    let (child_usage_tx, mut child_usage_rx) = tokio::sync::mpsc::unbounded_channel();
    // Set up AskUserQuestion channel: tool → TUI dialog
    let (ask_tx, mut ask_rx) =
        tokio::sync::mpsc::unbounded_channel::<(String, oneshot::Sender<String>)>();
    let tx_ask = tx.clone();
    tokio::spawn(async move {
        while let Some((question, reply)) = ask_rx.recv().await {
            let _ = tx_ask.send(AppEvent::AskUser { question, reply });
        }
    });

    // Set up plan mode channel: tool → inline drain per-iteration
    let (plan_tx, mut plan_rx) = tokio::sync::mpsc::unbounded_channel::<bool>();

    // Runtime plan mode state (may be toggled mid-turn by EnterPlanMode/ExitPlanMode tools)
    let mut effective_plan_mode = plan_mode;
    // Sticky for the whole turn: the gate is rebuilt every iteration.
    let mut skill_shell_blocked = skill_no_shell;

    // max_turns from CLI overrides the built-in safety cap (0 = use default cap)
    let turn_limit = if config.max_turns > 0 {
        config.max_turns
    } else {
        MAX_TOOL_ITERATIONS
    };

    // Shared Read-tool cache so repeat reads of unchanged files emit a
    // compact notice instead of the full body (v2.1.86).
    let read_cache = crate::tools::new_read_cache();

    let mut loop_guard = LoopGuard::default();

    let mut iterations: u32 = 0;
    // Retries consumed by the auto-fix loop within the current user turn.
    // Reset to 0 on every user prompt; the retry helper enforces the cap.
    let mut auto_fix_retries: u32 = 0;
    loop {
        iterations += 1;
        if iterations > turn_limit {
            let _ = tx.send(AppEvent::TurnFailed(format!(
                "Stopped after {turn_limit} tool iterations — possible loop detected."
            )));
            return;
        }

        // Build tool definitions, optionally adding prompt cache marker to the last one
        let mut tool_defs: Vec<ToolDefinition> = tools.iter().map(|t| t.definition()).collect();
        if config.prompt_cache
            && !tool_defs.is_empty()
            && let Some(last) = tool_defs.last_mut()
        {
            last.cache_control = Some(crate::api::types::CacheControl::ephemeral());
        }

        let max_tokens = config.max_tokens_for(&config.model);

        // Effort: a real parameter on models that have one, a prompt nudge elsewhere.
        let mut system_text = system_prompt.clone();
        let output_config =
            match crate::api::thinking::effort_for(&config.model, config.effort.as_deref()) {
                Some(crate::api::thinking::EffortWire::Param(oc)) => Some(oc),
                Some(crate::api::thinking::EffortWire::Prompt(nudge)) => {
                    system_text.push_str("\n\n");
                    system_text.push_str(&nudge);
                    None
                }
                None => None,
            };

        // Build system content — wrap in blocks for prompt caching if enabled
        let system_content = if config.prompt_cache {
            crate::api::types::SystemContent::Blocks(vec![crate::api::types::SystemBlock {
                block_type: "text".into(),
                text: system_text,
                cache_control: Some(crate::api::types::CacheControl::ephemeral()),
            }])
        } else {
            crate::api::types::SystemContent::Plain(system_text)
        };

        // Extended thinking: adaptive on Claude 4.6+/5, budget_tokens on older
        // models, nothing on non-Claude backends (see api::thinking).
        let thinking_cfg = crate::api::thinking::thinking_for(
            &config.model,
            config.thinking_budget_tokens,
            max_tokens,
        );
        let mut betas = crate::api::thinking::thinking_betas(thinking_cfg.as_ref());
        // Append extra betas from CLI --betas flag
        for b in &config.extra_betas {
            if !betas.contains(b) {
                betas.push(b.clone());
            }
        }

        // Tool result budgeting — truncate oversized ToolResult payloads
        // to prevent context overflow (mirrors apiMicrocompact truncation).
        let mut budgeted_messages = messages.clone();
        for msg in budgeted_messages.iter_mut() {
            for block in msg.content.iter_mut() {
                if let ContentBlock::ToolResult { content, .. } = block {
                    for item in content.iter_mut() {
                        let ToolResultContent::Text { text } = item;
                        crate::compact::budget_tool_result(text);
                    }
                }
            }
        }

        let request = MessagesRequest {
            model: config.model.clone(),
            max_tokens,
            system: system_content,
            messages: budgeted_messages,
            tools: tool_defs,
            stream: None,
            thinking: thinking_cfg,
            output_config,
            betas,
            session_id: Some(session_id.to_string()),
        };

        // Transient *pre-stream* failures (429, 5xx, Anthropic overloads,
        // connection refused) are retried inside the API client, which
        // honours `retry-after` and backs off exponentially. What is left here
        // is recovery the client cannot do: compacting an over-long prompt.
        //
        // Nothing may be retried once a chunk has reached the transcript.
        // `AppEvent::TextChunk` appends to `app.streaming` and that buffer is
        // never rewound, so re-issuing the call replays the whole response and
        // the user sees a truncated answer followed by a complete one.
        const MAX_RETRIES: u32 = 3;
        let mut attempt = 0u32;
        let mut should_compact_retry = false;
        let response = loop {
            attempt += 1;
            let tx2 = tx.clone();
            let req_clone = request.clone();
            let streamed_any = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let streamed_flag = streamed_any.clone();
            let result = client
                .messages_stream(req_clone, move |chunk| {
                    streamed_flag.store(true, std::sync::atomic::Ordering::Relaxed);
                    let _ = tx2.send(AppEvent::TextChunk(chunk.to_string()));
                })
                .await;

            match result {
                Ok(r) => break r,
                Err(e) => {
                    let err_str = e.to_string();

                    // prompt_too_long: signal outer loop to compact and retry
                    if err_str.contains("prompt is too long") || err_str.contains("prompt_too_long")
                    {
                        should_compact_retry = true;
                        break StreamedResponse {
                            content: vec![],
                            stop_reason: None,
                            usage: crate::api::types::Usage::default(),
                        };
                    }

                    // 429 and 5xx are deliberately absent: the client already
                    // retried those with proper backoff, and repeating them
                    // here would multiply into 15 attempts while ignoring the
                    // server's `retry-after`. The Anthropic client retries an
                    // overload (529 or a pre-text overloaded_error) the same
                    // way, so only other backends' overloads are retried here.
                    // The connection cases stay for a drop on the first event.
                    let overloaded = !matches!(client, crate::api::ApiBackend::Anthropic(_))
                        && (err_str.contains("529")
                            || err_str.contains("overloaded")
                            || err_str.contains("Overloaded"));
                    let is_retryable = overloaded
                        || err_str.contains("connection")
                        || err_str.contains("reset by peer");

                    // The duplication guard. Losing a partial response is bad;
                    // showing it twice is worse and corrupts the saved turn.
                    let streamed = streamed_any.load(std::sync::atomic::Ordering::Relaxed);
                    if is_retryable && streamed {
                        tracing::warn!(
                            "not retrying: output already streamed, a retry would \
                             duplicate the response ({err_str})"
                        );
                    }

                    if crate::api::retry::may_retry_stream(
                        is_retryable,
                        streamed,
                        attempt,
                        MAX_RETRIES,
                    ) {
                        let delay = std::time::Duration::from_millis(1000 * 2u64.pow(attempt - 1));
                        let _ = tx.send(AppEvent::SystemMessage(format!(
                            "API error (attempt {attempt}/{MAX_RETRIES}): {err_str}\nRetrying in {:.0}s…",
                            delay.as_secs_f64(),
                        )));
                        tokio::time::sleep(delay).await;
                        continue;
                    }

                    let _ = tx.send(AppEvent::TurnFailed(err_str));
                    return;
                }
            }
        };

        // Handle prompt_too_long: auto-compact and retry the outer loop
        if should_compact_retry {
            let _ = tx.send(AppEvent::SystemMessage(
                "Prompt too long — auto-compacting context…".into(),
            ));
            match crate::compact::summarize_compact(&client, &messages, &config).await {
                Ok(replacement) => {
                    let summary_len = replacement
                        .first()
                        .and_then(|m| m.content.first())
                        .map(|b| {
                            if let ContentBlock::Text { text } = b {
                                text.len()
                            } else {
                                0
                            }
                        })
                        .unwrap_or(0);
                    let _ = tx.send(AppEvent::Compacted {
                        replacement: replacement.clone(),
                        summary_len,
                        base: None,
                    });
                    messages = replacement;
                    publish_history(&history, &messages, &[]);
                    continue; // retry outer loop with compacted history
                }
                Err(compact_err) => {
                    let _ = tx.send(AppEvent::TurnFailed(format!(
                        "Prompt too long and auto-compact failed: {compact_err}"
                    )));
                    return;
                }
            }
        }

        task_cost.record_with_cache(
            &config.model,
            response.usage.input_tokens,
            response.usage.output_tokens,
            response.usage.cache_read_input_tokens,
            response.usage.cache_creation_input_tokens,
        );
        let _ = tx.send(AppEvent::Usage {
            model: config.model.clone(),
            input: response.usage.input_tokens,
            output: response.usage.output_tokens,
            cache_read: response.usage.cache_read_input_tokens,
            cache_write: response.usage.cache_creation_input_tokens,
        });

        // One-time notice when an Ollama model is detected as not supporting tools
        if client.take_tools_notice() {
            let _ = tx.send(AppEvent::SystemMessage(
                "Note: this model doesn't support tools — running in text-only mode.".into(),
            ));
        }

        // Emit thinking blocks to the TUI (if show_thinking_summaries is enabled)
        if config.show_thinking_summaries {
            for block in &response.content {
                if let ContentBlock::Thinking { thinking, .. } = block
                    && !thinking.trim().is_empty()
                {
                    let _ = tx.send(AppEvent::ThinkingBlock(thinking.clone()));
                }
            }
        }

        // Never store an empty assistant message: the next request would 400.
        if !response.content.is_empty() {
            messages.push(Message {
                role: Role::Assistant,
                content: response.content.clone(),
            });
            publish_history(&history, &messages, &[]);
        }

        if response.stop_reason == Some(StopReason::Refusal) {
            let _ = tx.send(AppEvent::SystemMessage(
                "The model declined this request (stop_reason: refusal).".into(),
            ));
        }
        match &response.stop_reason {
            Some(StopReason::EndTurn)
            | None
            | Some(StopReason::StopSequence)
            | Some(StopReason::Refusal)
            | Some(StopReason::Other) => {
                let _ = tx.send(AppEvent::Done {
                    tokens_in: response.usage.input_tokens,
                    tokens_out: response.usage.output_tokens,
                    cache_read: response.usage.cache_read_input_tokens,
                    cache_write: response.usage.cache_creation_input_tokens,
                    messages: messages.clone(),
                    model_used: config.model.clone(),
                });
                return;
            }
            Some(StopReason::MaxTokens) | Some(StopReason::ModelContextWindowExceeded) => {
                // Response hit the model's max_tokens cap. Preserve the partial
                // assistant content (already pushed to `messages` above) and
                // surface a warning instead of dropping the turn with an error.
                let _ = tx.send(AppEvent::SystemMessage(
                    "Response capped at max_tokens — partial output preserved. \
                     Ask the model to continue if more output is needed."
                        .into(),
                ));
                let _ = tx.send(AppEvent::Done {
                    tokens_in: response.usage.input_tokens,
                    tokens_out: response.usage.output_tokens,
                    cache_read: response.usage.cache_read_input_tokens,
                    cache_write: response.usage.cache_creation_input_tokens,
                    messages: messages.clone(),
                    model_used: config.model.clone(),
                });
                return;
            }
            Some(StopReason::ToolUse) => {
                // Set up a streaming channel so tools like Bash can send live output to the TUI
                let (stream_tx, mut stream_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
                let tx_stream = tx.clone();
                tokio::spawn(async move {
                    while let Some(line) = stream_rx.recv().await {
                        let _ = tx_stream.send(AppEvent::ToolOutputStream(line));
                    }
                });
                let mut ctx = ToolContext::new(config.cwd.clone());
                ctx.stream_tx = Some(stream_tx);
                ctx.ask_user_tx = Some(ask_tx.clone());
                ctx.plan_mode_tx = Some(plan_tx.clone());
                ctx.default_shell = config.default_shell.clone();
                ctx.env = config.env.clone();
                ctx.snapshot_dir = config.file_snapshot_dir.clone();
                if config.sandbox_enabled {
                    ctx.sandbox_mode = Some(config.sandbox_mode.clone());
                }
                ctx.sandbox_allow_network = config.sandbox_allow_network;
                ctx.read_cache = Some(read_cache.clone());
                // Publish live provider snapshot so AgentTool / spawned
                // sub-agents inherit `/model` changes made mid-session.
                ctx.live_model = Some(config.model.clone());
                ctx.live_api_key = Some(config.api_key.clone());
                ctx.live_ollama_host = Some(config.ollama_host.clone());
                ctx.usage_sink = Some(child_usage_tx.clone());
                ctx.budget_remaining_usd = task_cost.remaining();
                // One gate per turn (autonomy can change between turns via
                // /autonomy). Published on the context so `Agent` children
                // prompt through the same user.
                let mut gate = PermissionGate::new(
                    perm_state.clone(),
                    config.autonomy == "suggest",
                    Some(std::sync::Arc::new(TuiAsker { tx: tx.clone() })),
                )
                .with_blocked_tools(if effective_plan_mode {
                    PLAN_MODE_BLOCKED_TOOLS
                } else {
                    &[]
                });
                if skill_shell_blocked {
                    gate = gate.with_skill_shell_blocked();
                }
                ctx.permission_gate = Some(gate.clone());
                let mut results: Vec<ContentBlock> = Vec::new();

                // Auto-fix loop: accumulate file paths touched by Write/Edit/MultiEdit
                // in this assistant turn. After the tool-use loop we run the detected
                // lint + test commands and, on failure, feed the output back to the
                // model as a synthetic user turn via `continue`, up to
                // `config.auto_fix.max_retries` times.
                let mut auto_fix_touched: Vec<std::path::PathBuf> = Vec::new();

                // Drain any pending plan_mode changes before processing tools
                while let Ok(enabled) = plan_rx.try_recv() {
                    effective_plan_mode = enabled;
                    let msg = if enabled {
                        "Plan mode enabled by Claude."
                    } else {
                        "Plan mode disabled by Claude."
                    };
                    let _ = tx.send(AppEvent::SystemMessage(msg.into()));
                    let _ = tx.send(AppEvent::SetPlanMode(enabled));
                }

                for block in &response.content {
                    if let ContentBlock::ToolUse { id, name, input } = block {
                        publish_history(&history, &messages, &results);
                        let args = serde_json::to_string(input).unwrap_or_default();
                        let _ = tx.send(AppEvent::ToolCall {
                            name: name.clone(),
                            args: args.clone(),
                        });

                        // Plan mode: block destructive tools
                        if effective_plan_mode
                            && PLAN_MODE_BLOCKED_TOOLS
                                .iter()
                                .any(|b| crate::permissions::blocked_entry_matches(b, name))
                        {
                            let msg = format!(
                                "Tool '{}' is blocked in plan mode. \
                                 Use /plan to exit plan mode first.",
                                name
                            );
                            let _ = tx.send(AppEvent::ToolResult {
                                is_error: true,
                                text: msg.clone(),
                            });
                            // Refused calls count too: a model retrying one
                            // pays a full request each time.
                            let streak = loop_guard.record(name, &args, &msg, true);
                            results.push(ContentBlock::ToolResult {
                                tool_use_id: id.clone(),
                                content: vec![ToolResultContent::text(msg)],
                                is_error: Some(true),
                            });
                            if streak >= LOOP_THRESHOLD {
                                end_turn_on_loop(
                                    &tx,
                                    &mut messages,
                                    &mut results,
                                    &response.usage,
                                    &config.model,
                                    name,
                                    streak,
                                );
                                return;
                            }
                            continue;
                        }

                        // Pre-tool-use hooks — can block execution
                        if let Some(hook_cfg) = &config.hooks
                            && !config.disable_all_hooks
                        {
                            let hook_result = hooks::run_pre_tool_hooks(
                                hook_cfg,
                                name,
                                &args,
                                session_id,
                                &config.cwd,
                            )
                            .await;
                            if !hook_result.should_continue {
                                let msg = hook_result
                                    .stop_reason
                                    .unwrap_or_else(|| format!("PreToolUse hook blocked: {name}"));
                                if let Some(sys_msg) = hook_result.system_message {
                                    let _ = tx.send(AppEvent::SystemMessage(sys_msg));
                                }
                                let _ = tx.send(AppEvent::ToolResult {
                                    is_error: true,
                                    text: msg.clone(),
                                });
                                let streak = loop_guard.record(name, &args, &msg, true);
                                results.push(ContentBlock::ToolResult {
                                    tool_use_id: id.clone(),
                                    content: vec![ToolResultContent::text(msg)],
                                    is_error: Some(true),
                                });
                                if streak >= LOOP_THRESHOLD {
                                    end_turn_on_loop(
                                        &tx,
                                        &mut messages,
                                        &mut results,
                                        &response.usage,
                                        &config.model,
                                        name,
                                        streak,
                                    );
                                    return;
                                }
                                continue;
                            }
                            if let Some(sys_msg) = hook_result.system_message {
                                let _ = tx.send(AppEvent::SystemMessage(sys_msg));
                            }
                        } // disable_all_hooks guard

                        // Permission check — the gate handles the suggest-mode
                        // override, compound-command splitting, the prompt,
                        // and always-allow recording. Same code path as
                        // sub-agents and headless engines.
                        // The gate's text, so a blocked tool says why.
                        if let GateOutcome::Denied(reason) = gate.decide(name, input).await {
                            let _ = tx.send(AppEvent::ToolResult {
                                is_error: true,
                                text: reason.clone(),
                            });
                            let streak = loop_guard.record(name, &args, &reason, true);
                            results.push(ContentBlock::ToolResult {
                                tool_use_id: id.clone(),
                                content: vec![ToolResultContent::text(reason)],
                                is_error: Some(true),
                            });
                            if streak >= LOOP_THRESHOLD {
                                end_turn_on_loop(
                                    &tx,
                                    &mut messages,
                                    &mut results,
                                    &response.usage,
                                    &config.model,
                                    name,
                                    streak,
                                );
                                return;
                            }
                            continue;
                        }

                        let tool = tools.iter().find(|t| t.name() == name);
                        ctx.cwd = crate::tools::session_cwd(&tools, &config.cwd);
                        let output: ToolOutput = match tool {
                            Some(t) => t
                                .execute(input.clone(), &ctx)
                                .await
                                .unwrap_or_else(|e| ToolOutput::error(e.to_string())),
                            None => ToolOutput::error(format!("Unknown tool: {name}")),
                        };

                        // Sub-agent spend goes to /cost and /budget like the
                        // session's own; over budget, the main loop stops the turn.
                        while let Ok((model, u)) = child_usage_rx.try_recv() {
                            task_cost.record_with_cache(
                                &model,
                                u.input_tokens,
                                u.output_tokens,
                                u.cache_read_input_tokens,
                                u.cache_creation_input_tokens,
                            );
                            let _ = tx.send(AppEvent::Usage {
                                model,
                                input: u.input_tokens,
                                output: u.output_tokens,
                                cache_read: u.cache_read_input_tokens,
                                cache_write: u.cache_creation_input_tokens,
                            });
                        }
                        ctx.budget_remaining_usd = task_cost.remaining();

                        // A skill the model loads itself is as untrusted as a
                        // /skill one; later calls in this response are covered
                        // too, since they go through the updated gate.
                        if name == "Skill"
                            && !output.is_error
                            && config.disable_skill_shell_execution
                            && !skill_shell_blocked
                        {
                            skill_shell_blocked = true;
                            gate = gate.with_skill_shell_blocked();
                            ctx.permission_gate = Some(gate.clone());
                        }

                        let result_text = output
                            .content
                            .iter()
                            .map(|c| {
                                let ToolResultContent::Text { text } = c;
                                text.as_str()
                            })
                            .collect::<Vec<_>>()
                            .join("\n");

                        // Post-tool-use hooks — fire and don't block
                        if let Some(hook_cfg) = &config.hooks
                            && !config.disable_all_hooks
                        {
                            hooks::run_post_tool_hooks(
                                hook_cfg,
                                name,
                                &result_text,
                                session_id,
                                &config.cwd,
                            )
                            .await;
                        }

                        // Check if a plan mode change was emitted by a tool (EnterPlanMode / ExitPlanMode)
                        while let Ok(enabled) = plan_rx.try_recv() {
                            effective_plan_mode = enabled;
                            let msg = if enabled {
                                "Plan mode enabled by Claude."
                            } else {
                                "Plan mode disabled by Claude."
                            };
                            let _ = tx.send(AppEvent::SystemMessage(msg.into()));
                            let _ = tx.send(AppEvent::SetPlanMode(enabled));
                        }

                        let same_call_streak =
                            loop_guard.record(name, &args, &result_text, output.is_error);

                        let _ = tx.send(AppEvent::ToolResult {
                            is_error: output.is_error,
                            text: result_text,
                        });

                        if same_call_streak >= LOOP_THRESHOLD {
                            results.push(ContentBlock::ToolResult {
                                tool_use_id: id.clone(),
                                content: vec![ToolResultContent::text(format!(
                                    "Loop detected: identical '{name}' call returned identical \
                                     output {same_call_streak} times in a row. Change approach \
                                     or ask the user."
                                ))],
                                is_error: Some(true),
                            });
                            end_turn_on_loop(
                                &tx,
                                &mut messages,
                                &mut results,
                                &response.usage,
                                &config.model,
                                name,
                                same_call_streak,
                            );
                            return;
                        }

                        // Track files touched by successful Write/Edit/MultiEdit
                        // calls for the auto-fix post-loop check.
                        if !output.is_error
                            && matches!(name.as_str(), "Write" | "Edit" | "MultiEdit")
                            && let Some(fp) = input.get("file_path").and_then(|v| v.as_str())
                        {
                            let path = std::path::PathBuf::from(fp);
                            if !auto_fix_touched.contains(&path) {
                                auto_fix_touched.push(path);
                            }
                        }

                        results.push(ContentBlock::ToolResult {
                            tool_use_id: id.clone(),
                            content: output.content,
                            is_error: if output.is_error { Some(true) } else { None },
                        });
                    }
                }

                // ── Auto-fix check with retry loop ───────────────────────────
                // After all tool calls complete, run lint + tests. On failure
                // and under cap, append a synthetic user message with the
                // failure output and re-enter the agentic loop. On cap reached,
                // emit a SystemMessage and end the turn preserving partial work.
                if !auto_fix_touched.is_empty() {
                    // After EnterWorktree the edits are in the worktree,
                    // not the main checkout.
                    let work_cwd = crate::tools::session_cwd(&tools, &config.cwd);
                    let action = crate::autofix::run_auto_fix_check(
                        &work_cwd,
                        &config.auto_fix,
                        &config.autonomy,
                        auto_fix_retries,
                    );
                    auto_fix_touched.clear();

                    match action {
                        crate::autofix::AutoFixAction::Continue { status } => {
                            if let Some(msg) = status {
                                let _ = tx.send(AppEvent::SystemMessage(msg));
                            }
                        }
                        crate::autofix::AutoFixAction::Retry { feedback, status } => {
                            let _ = tx.send(AppEvent::SystemMessage(status));
                            // Every tool_use needs its tool_result in the very
                            // next user message, so the lint/test failure rides
                            // along after the results rather than replacing them.
                            let mut content = std::mem::take(&mut results);
                            content.push(ContentBlock::Text { text: feedback });
                            messages.push(Message {
                                role: Role::User,
                                content,
                            });
                            publish_history(&history, &messages, &[]);
                            auto_fix_retries += 1;
                            // Re-enter the outer loop to call the model again
                            // with the injected feedback in history.
                            continue;
                        }
                        crate::autofix::AutoFixAction::GiveUp { status } => {
                            let _ = tx.send(AppEvent::SystemMessage(status));
                            messages.push(Message {
                                role: Role::User,
                                content: std::mem::take(&mut results),
                            });
                            let _ = tx.send(AppEvent::Done {
                                tokens_in: response.usage.input_tokens,
                                tokens_out: response.usage.output_tokens,
                                cache_read: response.usage.cache_read_input_tokens,
                                cache_write: response.usage.cache_creation_input_tokens,
                                messages: messages.clone(),
                                model_used: config.model.clone(),
                            });
                            return;
                        }
                    }
                }

                // If no tool blocks were found (e.g. model returned finish_reason=tool_calls
                // but tools were disabled), treat as EndTurn to avoid an infinite loop.
                if results.is_empty() {
                    let _ = tx.send(AppEvent::Done {
                        tokens_in: response.usage.input_tokens,
                        tokens_out: response.usage.output_tokens,
                        cache_read: response.usage.cache_read_input_tokens,
                        cache_write: response.usage.cache_creation_input_tokens,
                        messages: messages.clone(),
                        model_used: config.model.clone(),
                    });
                    return;
                }

                messages.push(Message {
                    role: Role::User,
                    content: results,
                });
                publish_history(&history, &messages, &[]);
                // Loop → send next request
            }
        }
    }
}

/// Create a git checkpoint commit of all uncommitted changes.
/// Returns a summary string for display, or an error.
pub(super) fn git_checkpoint(
    cwd: &std::path::Path,
    message: Option<&str>,
) -> anyhow::Result<String> {
    use std::process::Command;

    // Check if we're in a git repo
    let in_repo = Command::new("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .current_dir(cwd)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !in_repo {
        anyhow::bail!("Not inside a git repository");
    }

    // Check for changes
    let status = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(cwd)
        .output()?;
    let status_text = String::from_utf8_lossy(&status.stdout);
    if status_text.trim().is_empty() {
        return Ok("No changes to checkpoint.".into());
    }

    // Stage all changes
    Command::new("git")
        .args(["add", "-A"])
        .current_dir(cwd)
        .output()?;

    // Create commit
    let ts = chrono_free_timestamp();
    let msg = message.unwrap_or("oxideclaw checkpoint");
    let full_msg = format!("[checkpoint] {msg} ({ts})");

    let commit = Command::new("git")
        .args(["commit", "-m", &full_msg, "--no-verify"])
        .current_dir(cwd)
        .output()?;

    if !commit.status.success() {
        let err = String::from_utf8_lossy(&commit.stderr);
        anyhow::bail!("git commit failed: {err}");
    }

    // Get the short hash
    let hash = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .current_dir(cwd)
        .output()?;
    let short = String::from_utf8_lossy(&hash.stdout).trim().to_string();

    let changed: usize = status_text.lines().count();
    Ok(format!(
        "Checkpoint created: {short} ({changed} files) — \"{full_msg}\"\nUse `git reset HEAD~1` to undo."
    ))
}

/// Simple timestamp without pulling in chrono: "2026-04-08T15:30:42"
pub(super) fn chrono_free_timestamp() -> String {
    use std::time::SystemTime;
    let secs = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    // Rough UTC breakdown (good enough for checkpoint labels)
    let days = secs / 86400;
    let time_of_day = secs % 86400;
    let h = time_of_day / 3600;
    let m = (time_of_day % 3600) / 60;
    let s = time_of_day % 60;
    // Days since epoch → year/month/day (simplified, ignoring leap seconds)
    let (y, mo, d) = days_to_ymd(days);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}")
}

pub(super) fn days_to_ymd(mut days: u64) -> (u64, u64, u64) {
    let mut y = 1970;
    loop {
        let dy = if is_leap(y) { 366 } else { 365 };
        if days < dy {
            break;
        }
        days -= dy;
        y += 1;
    }
    let leap = is_leap(y);
    let month_days: [u64; 12] = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut mo = 0;
    for (i, &md) in month_days.iter().enumerate() {
        if days < md {
            mo = i;
            break;
        }
        days -= md;
    }
    (y, (mo + 1) as u64, days + 1)
}

pub(super) fn is_leap(y: u64) -> bool {
    (y.is_multiple_of(4) && !y.is_multiple_of(100)) || y.is_multiple_of(400)
}

#[cfg(test)]
mod turn_history_tests {
    use super::*;

    fn text(role: Role, t: &str) -> Message {
        Message {
            role,
            content: vec![ContentBlock::Text { text: t.into() }],
        }
    }

    fn tool_use(id: &str) -> ContentBlock {
        ContentBlock::ToolUse {
            id: id.into(),
            name: "Bash".into(),
            input: serde_json::json!({ "command": "cargo test" }),
        }
    }

    fn tool_result(id: &str, out: &str) -> ContentBlock {
        ContentBlock::ToolResult {
            tool_use_id: id.into(),
            content: vec![ToolResultContent::text(out)],
            is_error: None,
        }
    }

    fn result_ids(m: &Message) -> Vec<(String, Option<bool>)> {
        m.content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::ToolResult {
                    tool_use_id,
                    is_error,
                    ..
                } => Some((tool_use_id.clone(), *is_error)),
                _ => None,
            })
            .collect()
    }

    /// Esc while the second of two tools runs: the first tool's result and
    /// the call itself survive, and the unfinished one is answered so the
    /// next request is not rejected for an orphaned tool_use.
    #[test]
    fn a_cancelled_round_keeps_finished_results_and_closes_the_rest() {
        let history = TurnHistory::default();
        let current = vec![text(Role::User, "q1"), text(Role::Assistant, "a1")];
        let mut messages = current.clone();
        messages.push(text(Role::User, "fix the tests"));
        messages.push(Message {
            role: Role::Assistant,
            content: vec![tool_use("t1"), tool_use("t2")],
        });
        publish_history(&history, &messages, &[tool_result("t1", "ok")]);

        let (got, rewrite) = recover_turn_history(&history, &current, 2).unwrap();
        assert!(!rewrite);
        assert_eq!(got.len(), 5);
        assert_eq!(got[..4], messages[..]);
        assert_eq!(
            result_ids(&got[4]),
            vec![("t1".into(), None), ("t2".into(), Some(true))]
        );
        // Taken, not copied: a second recovery has nothing to adopt.
        assert!(recover_turn_history(&history, &current, 2).is_none());
    }

    /// Esc after the assistant asked for tools but before any ran.
    #[test]
    fn unanswered_tool_uses_get_a_results_message() {
        let mut m = vec![
            text(Role::User, "go"),
            Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Text {
                        text: "running".into(),
                    },
                    tool_use("t1"),
                ],
            },
        ];
        close_dangling_tool_uses(&mut m);
        assert_eq!(m.len(), 3);
        assert_eq!(m[2].role, Role::User);
        assert_eq!(result_ids(&m[2]), vec![("t1".into(), Some(true))]);

        // A complete round is left alone.
        let before = m.clone();
        close_dangling_tool_uses(&mut m);
        assert_eq!(m, before);
    }

    /// A turn that failed on its first request published nothing; the
    /// caller keeps its own history (prompt at the tail).
    #[test]
    fn nothing_published_means_nothing_to_adopt() {
        let history = TurnHistory::default();
        assert!(recover_turn_history(&history, &[text(Role::User, "q")], 0).is_none());
    }

    /// The turn compacted its history (event still queued): the file holds
    /// the old history, so it has to be rewritten, not appended to.
    #[test]
    fn a_compacted_turn_rewrites_the_session_file() {
        let history = TurnHistory::default();
        let current = vec![
            text(Role::User, "q1"),
            text(Role::Assistant, "a1"),
            text(Role::User, "q2"),
        ];
        publish_history(&history, &[text(Role::User, "SUMMARY")], &[]);
        let (got, rewrite) = recover_turn_history(&history, &current, 2).unwrap();
        assert!(rewrite);
        assert_eq!(got, vec![text(Role::User, "SUMMARY")]);
    }
}

#[cfg(test)]
mod loop_guard_tests {
    use super::*;

    /// edit, test, edit, test, edit, test: each test run is the same call,
    /// which the old detector counted as a loop and aborted the turn on.
    #[test]
    fn an_edit_test_cycle_is_not_a_loop() {
        let mut g = LoopGuard::default();
        for i in 0..5 {
            let edit = format!(r#"{{"file_path":"a.rs","new_string":"v{i}"}}"#);
            assert_eq!(g.record("Edit", &edit, "ok", false), 1);
            let out = format!("{} passed; 1 failed", i);
            assert_eq!(
                g.record("Bash", r#"{"command":"cargo test"}"#, &out, false),
                1
            );
        }
    }

    /// The same call answered the same way, back to back, is a model stuck.
    #[test]
    fn identical_call_and_result_in_a_row_trips_the_threshold() {
        let mut g = LoopGuard::default();
        let args = r#"{"file_path":"a.rs","old_string":"x","new_string":"y"}"#;
        assert_eq!(g.record("Edit", args, "old_string not found", true), 1);
        assert_eq!(g.record("Edit", args, "old_string not found", true), 2);
        assert_eq!(
            g.record("Edit", args, "old_string not found", true),
            LOOP_THRESHOLD
        );
        // A different result resets the streak.
        assert_eq!(g.record("Edit", args, "ok", false), 1);
    }

    /// A refused call never reached the old guard's place after execution,
    /// so a model retrying a plan-mode-blocked Bash ran until the 50-request
    /// cap. Three identical refusals end the turn like three identical runs.
    #[tokio::test]
    async fn repeated_refused_calls_end_the_turn() {
        use crate::query_engine::scripted_api_tests::{serve, sse};
        let bash = |id: &str| {
            sse(
                &[serde_json::json!({"type":"tool_use","id":id,"name":"Bash","input":{}})],
                "tool_use",
            )
        };
        let (url, seen) = serve(vec![
            bash("t1"),
            bash("t2"),
            bash("t3"),
            bash("t4"),
            sse(
                &[serde_json::json!({"type":"text","text":"done"})],
                "end_turn",
            ),
        ])
        .await;
        let dir = tempfile::tempdir().unwrap();
        let config = Config {
            model: "claude-sonnet-5".into(),
            api_key: "sk-ant-test".into(),
            cwd: dir.path().to_path_buf(),
            ..Config::default()
        };
        let mut c = crate::api::ClaudeClient::new("sk-ant-test").unwrap();
        c.set_base_url_for_test(url);
        let (tx, mut rx) = mpsc::unbounded_channel();
        run_api_task(ApiTask {
            client: ApiBackend::Anthropic(c),
            tools: Vec::new(),
            messages: vec![Message {
                role: Role::User,
                content: vec![ContentBlock::Text { text: "go".into() }],
            }],
            config,
            perm_state: PermissionState::new(false, &[], &[]),
            system_prompt: String::new(),
            tx,
            plan_mode: true,
            skill_no_shell: false,
            session_id: "s".into(),
            budget_remaining_usd: None,
            history: TurnHistory::default(),
        })
        .await;

        assert_eq!(seen.lock().unwrap().len(), 3, "turn kept retrying");
        let mut done = None;
        let mut loop_notice = false;
        while let Ok(ev) = rx.try_recv() {
            match ev {
                AppEvent::Done { messages, .. } => done = Some(messages),
                AppEvent::SystemMessage(m) if m.contains("Loop detected") => loop_notice = true,
                _ => {}
            }
        }
        assert!(loop_notice);
        let messages = done.expect("turn ended with Done");
        // One result per tool_use, no duplicate for the looping call.
        let last = messages.last().unwrap();
        assert_eq!(last.role, Role::User);
        assert_eq!(last.content.len(), 1);
    }
}
