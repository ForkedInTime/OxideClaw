//! The per-turn API task: streaming, tool loop, auto-fix, checkpoints.
//! Split out of `tui/run.rs` mechanically — no behaviour change.

use super::*;

/// Maximum number of tool-use→response cycles per user turn.
/// Prevents runaway loops where Claude repeatedly calls tools without finishing.
pub(super) const MAX_TOOL_ITERATIONS: u32 = 50;

/// Destructive tools blocked when plan mode is active.
pub(super) const PLAN_MODE_BLOCKED_TOOLS: &[&str] = &[
    "Bash",
    "Write",
    "Edit",
    "MultiEdit",
    "NotebookEdit",
    "EnterWorktree",
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
    pub(super) session_id: String,
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
        session_id,
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
    // Set up AskUserQuestion channel: tool → TUI dialog
    let (ask_tx, mut ask_rx) =
        tokio::sync::mpsc::unbounded_channel::<(String, oneshot::Sender<String>)>();
    let tx_ask = tx.clone();
    tokio::spawn(async move {
        while let Some((question, reply)) = ask_rx.recv().await {
            let _ = tx_ask.send(AppEvent::AskUser {
                question,
                reply,
                secret: false,
            });
        }
    });

    // Set up plan mode channel: tool → inline drain per-iteration
    let (plan_tx, mut plan_rx) = tokio::sync::mpsc::unbounded_channel::<bool>();

    // Runtime plan mode state (may be toggled mid-turn by EnterPlanMode/ExitPlanMode tools)
    let mut effective_plan_mode = plan_mode;

    // max_turns from CLI overrides the built-in safety cap (0 = use default cap)
    let turn_limit = if config.max_turns > 0 {
        config.max_turns
    } else {
        MAX_TOOL_ITERATIONS
    };

    // Shared Read-tool cache so repeat reads of unchanged files emit a
    // compact notice instead of the full body (v2.1.86).
    let read_cache = crate::tools::new_read_cache();

    // Loop detection: track recent (tool_name, args_hash) to catch repeated failures.
    // If the same call appears 3+ times in the last 6 entries, pause and ask.
    let mut recent_calls: std::collections::VecDeque<(String, u64)> =
        std::collections::VecDeque::new();
    const LOOP_WINDOW: usize = 6;
    const LOOP_THRESHOLD: usize = 3;

    let mut iterations: u32 = 0;
    // Retries consumed by the auto-fix loop within the current user turn.
    // Reset to 0 on every user prompt; the retry helper enforces the cap.
    let mut auto_fix_retries: u32 = 0;
    loop {
        iterations += 1;
        if iterations > turn_limit {
            let _ = tx.send(AppEvent::Error(format!(
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
        const TOOL_RESULT_MAX_CHARS: usize = 100_000;
        let mut budgeted_messages = messages.clone();
        for msg in budgeted_messages.iter_mut() {
            for block in msg.content.iter_mut() {
                if let ContentBlock::ToolResult { content, .. } = block {
                    for item in content.iter_mut() {
                        let ToolResultContent::Text { text } = item;
                        if text.len() > TOOL_RESULT_MAX_CHARS {
                            let truncated: String =
                                text.chars().take(TOOL_RESULT_MAX_CHARS).collect();
                            *text = format!(
                                "{truncated}\n\n[... output truncated to {TOOL_RESULT_MAX_CHARS} characters]"
                            );
                        }
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

        // Transient *pre-stream* failures (429, 5xx, connection refused) are
        // retried inside the API client, which honours `retry-after` and backs
        // off exponentially. What is left here is recovery the client cannot
        // do: compacting an over-long prompt, and switching models on 529.
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
                    // server's `retry-after`. 529 stays because the client
                    // leaves it alone for the model-switch path, and the
                    // connection cases stay for a drop on the very first event.
                    let is_retryable = err_str.contains("529")
                        || err_str.contains("overloaded")
                        || err_str.contains("Overloaded")
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

                    let _ = tx.send(AppEvent::Error(err_str));
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
                    });
                    messages = replacement;
                    continue; // retry outer loop with compacted history
                }
                Err(compact_err) => {
                    let _ = tx.send(AppEvent::Error(format!(
                        "Prompt too long and auto-compact failed: {compact_err}"
                    )));
                    return;
                }
            }
        }

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

        messages.push(Message {
            role: Role::Assistant,
            content: response.content.clone(),
        });

        match &response.stop_reason {
            Some(StopReason::EndTurn) | None | Some(StopReason::StopSequence) => {
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
            Some(StopReason::MaxTokens) => {
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
                ctx.live_auth = Some(config.auth.clone());
                ctx.live_ollama_host = Some(config.ollama_host.clone());
                // One gate per turn (autonomy can change between turns via
                // /autonomy). Published on the context so `Agent` children
                // prompt through the same user.
                let gate = PermissionGate::new(
                    perm_state.clone(),
                    config.autonomy == "suggest",
                    Some(std::sync::Arc::new(TuiAsker { tx: tx.clone() })),
                )
                .with_blocked_tools(if effective_plan_mode {
                    PLAN_MODE_BLOCKED_TOOLS
                } else {
                    &[]
                });
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
                        let args = serde_json::to_string(input).unwrap_or_default();
                        let _ = tx.send(AppEvent::ToolCall {
                            name: name.clone(),
                            args: args.clone(),
                        });

                        // ── Loop detection ──────────────────────────────────
                        {
                            use std::hash::{Hash, Hasher};
                            let mut hasher = std::collections::hash_map::DefaultHasher::new();
                            args.hash(&mut hasher);
                            let sig = (name.clone(), hasher.finish());
                            recent_calls.push_back(sig.clone());
                            while recent_calls.len() > LOOP_WINDOW {
                                recent_calls.pop_front();
                            }
                            let repeats = recent_calls.iter().filter(|c| **c == sig).count();
                            if repeats >= LOOP_THRESHOLD {
                                let _ = tx.send(AppEvent::Error(format!(
                                    "Loop detected: tool '{name}' called {} times with identical arguments in the last {} calls. \
                                     Pausing to prevent infinite loop. Send a new message to continue.",
                                    repeats, LOOP_WINDOW,
                                )));
                                return;
                            }
                        }

                        // Plan mode: block destructive tools
                        if effective_plan_mode && PLAN_MODE_BLOCKED_TOOLS.contains(&name.as_str()) {
                            let msg = format!(
                                "Tool '{}' is blocked in plan mode. \
                                 Use /plan to exit plan mode first.",
                                name
                            );
                            let _ = tx.send(AppEvent::ToolResult {
                                is_error: true,
                                text: msg.clone(),
                            });
                            results.push(ContentBlock::ToolResult {
                                tool_use_id: id.clone(),
                                content: vec![ToolResultContent::text(msg)],
                                is_error: Some(true),
                            });
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
                                results.push(ContentBlock::ToolResult {
                                    tool_use_id: id.clone(),
                                    content: vec![ToolResultContent::text(msg)],
                                    is_error: Some(true),
                                });
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
                        let decision = match gate.decide(name, input).await {
                            GateOutcome::Allowed => PermissionDecision::Allow,
                            GateOutcome::Denied(_) => PermissionDecision::Deny,
                        };

                        if decision == PermissionDecision::Deny {
                            let _ = tx.send(AppEvent::ToolResult {
                                is_error: true,
                                text: format!("Permission denied: {name}"),
                            });
                            results.push(ContentBlock::ToolResult {
                                tool_use_id: id.clone(),
                                content: vec![ToolResultContent::text(format!(
                                    "Permission denied: {name}"
                                ))],
                                is_error: Some(true),
                            });
                            continue;
                        }

                        let tool = tools.iter().find(|t| t.name() == name);
                        let output: ToolOutput = match tool {
                            Some(t) => t
                                .execute(input.clone(), &ctx)
                                .await
                                .unwrap_or_else(|e| ToolOutput::error(e.to_string())),
                            None => ToolOutput::error(format!("Unknown tool: {name}")),
                        };

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

                        let _ = tx.send(AppEvent::ToolResult {
                            is_error: output.is_error,
                            text: result_text,
                        });

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
                    let action = crate::autofix::run_auto_fix_check(
                        &config.cwd,
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
                            // Append the synthetic user turn so the model sees
                            // the lint/test failure on the next API round.
                            messages.push(Message {
                                role: Role::User,
                                content: vec![ContentBlock::Text { text: feedback }],
                            });
                            auto_fix_retries += 1;
                            // Re-enter the outer loop to call the model again
                            // with the injected feedback in history.
                            continue;
                        }
                        crate::autofix::AutoFixAction::GiveUp { status } => {
                            let _ = tx.send(AppEvent::SystemMessage(status));
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
