//! The per-turn API task: streaming, tool loop, auto-fix, checkpoints.
//! Split out of `tui/run.rs` mechanically — no behaviour change.

use super::*;

/// Maximum number of tool-use→response cycles per user turn.
/// Prevents runaway loops where Claude repeatedly calls tools without finishing.
pub(super) const MAX_TOOL_ITERATIONS: u32 = 50;

/// Destructive tools blocked when plan mode is active. PowerShell runs
/// commands like Bash; the browser actions click and type on live sites;
/// MCP tools can do anything their server does and do not say whether they
/// write; ExitWorktree deletes the worktree directory. Agent is not listed:
/// sub-agents inherit this list through the permission gate, so Explore and
/// Plan helpers can still research while every write they try is refused.
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
    /// The model router, when it routes this turn: the task picks the tier
    /// from the last user message and moves up a tier on failure.
    pub(super) router: Option<crate::router::RouterConfig>,
    /// The session router for a turn it does not route (/review, skills,
    /// plugin commands): an overflowing history is summarised on its
    /// largest tier, since routed turns may have grown it past this model.
    pub(super) compact_router: Option<crate::router::RouterConfig>,
}

/// Show `client`'s retry backoff in the transcript. Without this a
/// rate-limited turn sits on a spinner for up to a minute with no
/// explanation and reads as a freeze.
fn notify_retries(client: &mut ApiBackend, tx: &mpsc::UnboundedSender<AppEvent>) {
    let notice_tx = tx.clone();
    client.set_retry_notifier(std::sync::Arc::new(
        move |n: &crate::api::retry::RetryNotice| {
            let _ = notice_tx.send(AppEvent::SystemMessage(n.message()));
        },
    ));
}

/// Continue the turn one tier up after `trigger`, once per turn. True when
/// it moved: `client` and `config.model` now serve the new tier.
async fn escalate(
    routing: &mut Option<crate::router::TurnRoute>,
    trigger: crate::router::Trigger,
    client: &mut ApiBackend,
    config: &mut Config,
    context_tokens: u64,
    budget_left: Option<f64>,
    tx: &mpsc::UnboundedSender<AppEvent>,
) -> bool {
    let Some(route) = routing.as_mut() else {
        return false;
    };
    let mut notices = Vec::new();
    let next = route
        .escalate(
            config,
            client,
            context_tokens,
            budget_left,
            trigger,
            &mut notices,
        )
        .await;
    for n in notices {
        let _ = tx.send(AppEvent::SystemMessage(n));
    }
    match next {
        crate::router::Escalation::To(r) => {
            let line = r.line();
            *client = r.client;
            notify_retries(client, tx);
            config.model = r.model.clone();
            let _ = tx.send(AppEvent::Routed {
                model: r.model,
                line,
            });
            true
        }
        crate::router::Escalation::OverBudget(line) => {
            let _ = tx.send(AppEvent::SystemMessage(line));
            false
        }
        crate::router::Escalation::None => false,
    }
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

/// The files an edit tool call names, before it runs.
fn edit_targets(name: &str, input: &serde_json::Value) -> Vec<std::path::PathBuf> {
    let as_path = |v: &serde_json::Value| {
        v.get("file_path")
            .and_then(|p| p.as_str())
            .map(std::path::PathBuf::from)
    };
    match name {
        "Write" | "Edit" => as_path(input).into_iter().collect(),
        "MultiEdit" => input
            .get("edits")
            .and_then(|e| e.as_array())
            .map(|edits| edits.iter().filter_map(as_path).collect())
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

/// The files an edit tool call wrote, for the auto-fix check. MultiEdit
/// has no top-level `file_path` (each edit names its own), so turns that
/// edited only through it never ran lint or tests. It also commits per
/// file: a reported error can still leave other files written, which its
/// "✓" lines show.
fn edited_paths(
    name: &str,
    input: &serde_json::Value,
    output: &crate::tools::ToolOutput,
) -> Vec<std::path::PathBuf> {
    let applied = !output.is_error
        || name == "MultiEdit"
            && output.content.iter().any(|c| {
                let ToolResultContent::Text { text } = c;
                text.contains('✓')
            });
    if applied {
        edit_targets(name, input)
    } else {
        Vec::new()
    }
}

/// `path` as the edit tools resolve it from `cwd`.
fn absolute_in(cwd: &std::path::Path, path: &std::path::Path) -> std::path::PathBuf {
    crate::tools::file_read::resolve_path(&path.to_string_lossy(), cwd)
        .unwrap_or_else(|_| cwd.join(path))
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
        mut config,
        perm_state,
        system_prompt,
        tx,
        plan_mode,
        skill_no_shell,
        session_id,
        budget_remaining_usd,
        history,
        router,
        compact_router,
    } = task;
    let session_id = session_id.as_str();
    notify_retries(&mut client, &tx);
    // This task's own spend, so a sub-agent is capped at what is left of
    // the budget rather than given all of it again.
    let mut task_cost = crate::cost::CostTracker::new();
    if let Some(left) = budget_remaining_usd {
        task_cost.set_budget(left);
    }
    // Plain prompts are refused over budget before they get here, but slash
    // commands that start a turn (/review, skills, plugin commands) were not:
    // each sent one more full-history request before /budget stopped it.
    if budget_remaining_usd.is_some_and(|left| left <= 0.0) {
        let _ = tx.send(AppEvent::TurnFailed(
            "Budget reached — not sending. Use /budget to raise or clear it.".into(),
        ));
        return;
    }

    // Pick the tier here rather than in the key handler: the model
    // classifier is a network call, and the UI keeps drawing meanwhile.
    let mut routing: Option<crate::router::TurnRoute> = None;
    if let Some(router) = router {
        let prompt = crate::router::last_prompt(&messages);
        let context_tokens = crate::router::estimate_context_tokens(&system_prompt, &messages);
        let outcome = router
            .route(&config, &client, &prompt, context_tokens)
            .await;
        for n in outcome.notices {
            let _ = tx.send(AppEvent::SystemMessage(n));
        }
        if let Some((model, u)) = &outcome.classifier_usage {
            task_cost.record_with_cache(
                model,
                u.input_tokens,
                u.output_tokens,
                u.cache_read_input_tokens,
                u.cache_creation_input_tokens,
            );
            let _ = tx.send(AppEvent::usage(model, u));
        }
        match outcome.route {
            Some(route) => {
                let line = route.line();
                client = route.client;
                notify_retries(&mut client, &tx);
                config.model = route.model.clone();
                let _ = tx.send(AppEvent::Routed {
                    model: route.model,
                    line,
                });
                routing = Some(crate::router::TurnRoute::new(router, route.tier));
            }
            None => {
                let _ = tx.send(AppEvent::Routed {
                    model: config.model.clone(),
                    line: format!(
                        "Router: no tier is usable; this turn runs on {}.",
                        config.model
                    ),
                });
            }
        }
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
    // The last request overflowed and was answered by compacting. A second
    // overflow straight after means the fixed part of the request (system
    // prompt, tool definitions, maxTokens) leaves no room: compacting again
    // only summarises the summary, billing a call each time.
    let mut overflow_compacted = false;
    // Retries consumed by the auto-fix loop within the current user turn.
    // Reset to 0 on every user prompt; the retry helper enforces the cap.
    let mut auto_fix_retries: u32 = 0;
    // Each edited file as it was before this turn's first edit to it, so
    // the language-server step reports only the errors the turn added.
    let lsp_pool = crate::tools::lsp_pool(&tools);
    let mut lsp_baselines: std::collections::HashMap<
        std::path::PathBuf,
        crate::autofix::LspBaseline,
    > = std::collections::HashMap::new();
    'turn: loop {
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

        // Effort, thinking and betas; the effort nudge lands in system_text.
        let mut system_text = system_prompt.clone();
        let (thinking_cfg, output_config, betas) = crate::api::thinking::request_knobs(
            &config,
            &config.model,
            max_tokens,
            &mut system_text,
        );

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
            explicit_max_tokens: config.explicit_max_tokens_for(&config.model).is_some(),
            cache_history: config.prompt_cache,
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

                    // Over the context window: signal outer loop to compact and retry
                    if crate::api::is_context_overflow(&err_str) {
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
                    // The connection cases cover a stream that dropped before
                    // any text: next_sse_event names a mid-body drop, and the
                    // stall timeout says the connection was likely dropped.
                    let overloaded = !matches!(client, crate::api::ApiBackend::Anthropic(_))
                        && crate::api::retry::is_overloaded(&e);
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

                    // The cheap tier failed the request itself: try the
                    // next tier up before giving up. Not after text has
                    // streamed, for the same reason as the retries above.
                    if !streamed
                        && crate::router::escalates_on(&err_str)
                        && escalate(
                            &mut routing,
                            crate::router::Trigger::ApiError,
                            &mut client,
                            &mut config,
                            crate::router::estimate_context_tokens(&system_prompt, &messages),
                            task_cost.remaining(),
                            &tx,
                        )
                        .await
                    {
                        continue 'turn;
                    }
                    let _ = tx.send(AppEvent::TurnFailed(err_str));
                    return;
                }
            }
        };

        // Handle prompt_too_long: a routed turn first moves to a tier with
        // a larger window; otherwise auto-compact and retry the outer loop.
        if should_compact_retry
            && escalate(
                &mut routing,
                crate::router::Trigger::ContextOverflow,
                &mut client,
                &mut config,
                crate::router::estimate_context_tokens(&system_prompt, &messages),
                task_cost.remaining(),
                &tx,
            )
            .await
        {
            continue;
        }
        if should_compact_retry && overflow_compacted {
            let _ = tx.send(AppEvent::TurnFailed(
                "Prompt still exceeds the model's context window after compacting: the system \
                 prompt, tool definitions or maxTokens leave no room. Use a larger-window model, \
                 lower maxTokens or disable MCP servers or tools."
                    .into(),
            ));
            return;
        }
        if should_compact_retry {
            let _ = tx.send(AppEvent::SystemMessage(
                "Prompt too long — auto-compacting context…".into(),
            ));
            // The history already overflows the window as sent; summarise a
            // snipped copy so the summary request has a chance to fit, on
            // the router's largest tier when this model's window is smaller.
            let (sum_client, sum_config) = compaction_backend(
                &config,
                &client,
                routing
                    .as_ref()
                    .map(|r| &r.router)
                    .or(compact_router.as_ref()),
            );
            let mut snipped = messages.clone();
            crate::compact::snip_compact(&mut snipped, &sum_config.model);
            if let Some(hook_cfg) = &config.hooks
                && !config.disable_all_hooks
            {
                hooks::run_pre_compact_hooks(hook_cfg, session_id, &config.cwd).await;
            }
            let bill = |u: &Usage| {
                task_cost.record_with_cache(
                    &sum_config.model,
                    u.input_tokens,
                    u.output_tokens,
                    u.cache_read_input_tokens,
                    u.cache_creation_input_tokens,
                );
                let _ = tx.send(AppEvent::usage(&sum_config.model, u));
            };
            match crate::compact::summarize_compact(&sum_client, &snipped, &sum_config, bill).await
            {
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
                    if let Some(hook_cfg) = &config.hooks
                        && !config.disable_all_hooks
                    {
                        hooks::run_post_compact_hooks(hook_cfg, session_id, &config.cwd).await;
                    }
                    let _ = tx.send(AppEvent::Compacted {
                        replacement: replacement.clone(),
                        summary_len,
                        base: None,
                    });
                    messages = replacement;
                    publish_history(&history, &messages, &[]);
                    // The summary dropped the bodies of this turn's reads;
                    // a re-read must return the file, not "unchanged".
                    read_cache.lock().unwrap_or_else(|e| e.into_inner()).clear();
                    overflow_compacted = true;
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

        // The compacted request fit; a later overflow in a long turn may
        // compact again.
        overflow_compacted = false;
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
            context: true,
        });

        // One-time notice when an Ollama model is detected as not supporting tools
        if client.take_tools_notice() {
            let _ = tx.send(AppEvent::SystemMessage(
                "Note: this model doesn't support tools — running in text-only mode.".into(),
            ));
        }
        if client.take_summary_notice() {
            let _ = tx.send(AppEvent::SystemMessage(
                "Note: OpenAI refused reasoning summaries (they need a verified organization), \
                 so showThinkingSummaries is ignored for this session."
                    .into(),
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

        // The tier is fixed for the rest of this prompt and Ollama truncates
        // an overflow silently instead of failing, so measure each response
        // against the turn's window; the auto-compact after the turn only
        // measures against the largest tier's.
        if response.stop_reason == Some(StopReason::ToolUse) {
            use crate::compact::CompactNeeded;
            let window = crate::compact::turn_window(&config, routing.as_ref().map(|r| &r.router));
            let context_tokens = response.usage.context_tokens();
            let need = crate::compact::compact_needed(context_tokens, window);
            let moved = need == CompactNeeded::Summarise
                && escalate(
                    &mut routing,
                    crate::router::Trigger::ContextOverflow,
                    &mut client,
                    &mut config,
                    context_tokens,
                    task_cost.remaining(),
                    &tx,
                )
                .await;
            if !moved
                && matches!(need, CompactNeeded::Snip | CompactNeeded::Summarise)
                && config.auto_compact_enabled
                && crate::compact::snip_compact(&mut messages, &config.model)
            {
                let _ = tx.send(AppEvent::SystemMessage(format!(
                    "Context is {}% of {}'s window: stripped old tool results (snipCompact).",
                    context_tokens * 100 / window.max(1),
                    config.model
                )));
                publish_history(&history, &messages, &[]);
                // A re-read must return the file, not "unchanged".
                read_cache.lock().unwrap_or_else(|e| e.into_inner()).clear();
            }
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
                // Malformed calls twice in a row: the model cannot drive the
                // tools. The calls still get their (error) results below;
                // the next request goes one tier up.
                if let Some(route) = routing.as_mut() {
                    let defs: Vec<ToolDefinition> = tools.iter().map(|t| t.definition()).collect();
                    if route.malformed_twice(&response.content, &defs) {
                        let context_tokens =
                            crate::router::estimate_context_tokens(&system_prompt, &messages);
                        escalate(
                            &mut routing,
                            crate::router::Trigger::MalformedToolCalls,
                            &mut client,
                            &mut config,
                            context_tokens,
                            task_cost.remaining(),
                            &tx,
                        )
                        .await;
                    }
                }
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
                if config.sandbox_enabled {
                    ctx.sandbox_mode = Some(config.sandbox_mode.clone());
                }
                ctx.sandbox_allow_network = config.sandbox_allow_network;
                ctx.project_trusted = config.project_trusted;
                ctx.read_cache = Some(read_cache.clone());
                // Publish live provider snapshot so AgentTool / spawned
                // sub-agents inherit `/model` changes made mid-session.
                ctx.live_model = Some(config.model.clone());
                ctx.live_api_key = Some(config.api_key.clone());
                ctx.live_ollama_host = Some(config.ollama_host.clone());
                ctx.live_thinking_budget = Some(config.thinking_budget_tokens);
                ctx.usage_sink = Some(child_usage_tx.clone());
                ctx.budget_remaining_usd = task_cost.remaining();
                // Drain any pending plan_mode changes before building the gate
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
                ctx.live_plan_mode = Some(effective_plan_mode);

                // One gate per response (autonomy can change between turns
                // via /autonomy), rebuilt when plan mode flips mid-response.
                // Published on the context so `Agent` children prompt
                // through the same user and inherit the plan-mode blocks.
                let bash_shell = ctx.command_shell();
                let build_gate = |plan: bool, skill_shell: bool| {
                    let gate = PermissionGate::new(
                        perm_state.clone(),
                        config.effective_autonomy(),
                        Some(std::sync::Arc::new(TuiAsker { tx: tx.clone() })),
                    )
                    .with_bash_shell(&bash_shell)
                    .with_blocked_tools(if plan {
                        PLAN_MODE_BLOCKED_TOOLS
                    } else {
                        &[]
                    });
                    if skill_shell {
                        gate.with_skill_shell_blocked()
                    } else {
                        gate
                    }
                };
                let mut gate = build_gate(effective_plan_mode, skill_shell_blocked);
                ctx.permission_gate = Some(gate.clone());
                let mut results: Vec<ContentBlock> = Vec::new();

                // Auto-fix loop: accumulate file paths touched by Write/Edit/MultiEdit
                // in this assistant turn. After the tool-use loop we run the detected
                // lint + test commands and, on failure, feed the output back to the
                // model as a synthetic user turn via `continue`, up to
                // `config.auto_fix.max_retries` times.
                let mut auto_fix_touched: Vec<std::path::PathBuf> = Vec::new();
                // A call that tripped the loop guard: (tool, streak). Its
                // result is already in `results`.
                let mut loop_hit: Option<(String, usize)> = None;

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
                                loop_hit = Some((name.clone(), streak));
                                break;
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
                                    loop_hit = Some((name.clone(), streak));
                                    break;
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
                        // Judged where the tool will write: an entered
                        // worktree, not the launch project.
                        let work_cwd = crate::tools::session_cwd(&tools, &config.cwd);
                        if let GateOutcome::Denied(reason) =
                            gate.decide_in(name, input, &work_cwd).await
                        {
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
                                loop_hit = Some((name.clone(), streak));
                                break;
                            }
                            continue;
                        }

                        let tool = tools.iter().find(|t| t.name() == name);
                        ctx.cwd = crate::tools::session_cwd(&tools, &config.cwd);
                        if let Some(pool) = &lsp_pool
                            && config.project_trusted
                            && config.auto_fix.lsp.enabled
                            && crate::autofix::should_trigger(&config.auto_fix, config.autonomy)
                        {
                            for path in edit_targets(name, input) {
                                let path = absolute_in(&ctx.cwd, &path);
                                if let std::collections::hash_map::Entry::Vacant(e) =
                                    lsp_baselines.entry(path)
                                {
                                    let baseline = crate::autofix::capture_lsp_baseline(
                                        pool,
                                        &ctx.cwd,
                                        e.key(),
                                        std::env::var_os("PATH").as_deref(),
                                    )
                                    .await;
                                    e.insert(baseline);
                                }
                            }
                        }
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
                            let _ = tx.send(AppEvent::usage(&model, &u));
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
                        let was_plan_mode = effective_plan_mode;
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
                        // An Agent later in this response must get the new
                        // blocks, not the gate built before the switch.
                        if effective_plan_mode != was_plan_mode {
                            gate = build_gate(effective_plan_mode, skill_shell_blocked);
                            ctx.permission_gate = Some(gate.clone());
                            ctx.live_plan_mode = Some(effective_plan_mode);
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
                            loop_hit = Some((name.clone(), same_call_streak));
                            break;
                        }

                        // Files this call wrote trigger the auto-fix check.
                        for path in edited_paths(name, input, &output) {
                            if !auto_fix_touched.contains(&path) {
                                auto_fix_touched.push(path);
                            }
                        }

                        // Stored cut, as the request copy is: compaction
                        // renders the stored history, not what was sent.
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
                }

                if let Some((name, streak)) = loop_hit {
                    // A routed turn gets one more try a tier up, with the
                    // repeated results in front of it.
                    let context_tokens =
                        crate::router::estimate_context_tokens(&system_prompt, &messages);
                    if escalate(
                        &mut routing,
                        crate::router::Trigger::Loop,
                        &mut client,
                        &mut config,
                        context_tokens,
                        task_cost.remaining(),
                        &tx,
                    )
                    .await
                    {
                        messages.push(Message {
                            role: Role::User,
                            content: std::mem::take(&mut results),
                        });
                        close_dangling_tool_uses(&mut messages);
                        publish_history(&history, &messages, &[]);
                        loop_guard = LoopGuard::default();
                        continue 'turn;
                    }
                    end_turn_on_loop(
                        &tx,
                        &mut messages,
                        &mut results,
                        &response.usage,
                        &config.model,
                        &name,
                        streak,
                    );
                    return;
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
                    // Lint and tests can run for minutes; keep them off the
                    // async worker that also drives the UI channel.
                    let (auto_fix, autonomy) = (config.auto_fix.clone(), config.autonomy);
                    // Trust is the launch project's: a worktree it entered
                    // holds the same repo. The sandbox is the Bash tool's.
                    let containment = crate::autofix::Containment {
                        trusted: config.project_trusted,
                        sandbox_mode: config.sandbox_enabled.then(|| config.sandbox_mode.clone()),
                        sandbox_allow_network: config.sandbox_allow_network,
                    };
                    // Esc aborts this task at the await below, which leaves
                    // the blocking check running; dropping the guard there
                    // tells it to kill its lint/test processes.
                    struct CancelOnDrop(std::sync::Arc<std::sync::atomic::AtomicBool>);
                    impl Drop for CancelOnDrop {
                        fn drop(&mut self) {
                            self.0.store(true, std::sync::atomic::Ordering::SeqCst);
                        }
                    }
                    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                    let _cancel_on_abort = CancelOnDrop(cancel.clone());
                    // Language servers for the edited files, the LSP tool's.
                    let lsp = lsp_pool.clone().map(|pool| {
                        let files = auto_fix_touched
                            .iter()
                            .map(|p| {
                                let p = absolute_in(&work_cwd, p);
                                let baseline = lsp_baselines.get(&p).cloned();
                                (p, baseline)
                            })
                            .collect();
                        crate::autofix::LspDiagnostics {
                            pool,
                            root: work_cwd.clone(),
                            files,
                            search_path: std::env::var_os("PATH"),
                            runtime: tokio::runtime::Handle::current(),
                        }
                    });
                    let action = match tokio::task::spawn_blocking(move || {
                        crate::autofix::run_auto_fix_check(
                            &work_cwd,
                            &auto_fix,
                            autonomy,
                            auto_fix_retries,
                            &containment,
                            &cancel,
                            lsp.as_ref(),
                        )
                    })
                    .await
                    {
                        Ok(action) => action,
                        Err(e) => crate::autofix::AutoFixAction::Continue {
                            status: Some(format!("Auto-fix check failed: {e}")),
                        },
                    };
                    auto_fix_touched.clear();

                    match action {
                        crate::autofix::AutoFixAction::Continue { status } => {
                            if let Some(msg) = status {
                                let _ = tx.send(AppEvent::SystemMessage(msg));
                            }
                        }
                        // The app shows the notice once per session.
                        crate::autofix::AutoFixAction::Untrusted => {
                            let _ = tx.send(AppEvent::AutoFixUntrusted);
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
    // Hooks and fsmonitor off: the sandbox can write `.git/`, and this runs
    // on the host.
    let git = || oxideclaw::autocommit::git_cmd(cwd);

    // Check if we're in a git repo
    let in_repo = git()
        .args(["rev-parse", "--is-inside-work-tree"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !in_repo {
        anyhow::bail!("Not inside a git repository");
    }

    // Staging runs clean filters on the host, so only trusted ones.
    oxideclaw::autocommit::check_no_untrusted_filters(cwd, cwd)?;

    // Check for changes
    let status = git().args(["status", "--porcelain"]).output()?;
    let status_text = String::from_utf8_lossy(&status.stdout);
    if status_text.trim().is_empty() {
        return Ok("No changes to checkpoint.".into());
    }

    // Stage all changes
    git().args(["add", "-A"]).output()?;

    // Create commit
    let ts = chrono_free_timestamp();
    let msg = message.unwrap_or("oxideclaw checkpoint");
    let full_msg = format!("[checkpoint] {msg} ({ts})");

    let commit = git()
        .args(["commit", "-m", &full_msg, "--no-verify"])
        .output()?;

    if !commit.status.success() {
        let err = String::from_utf8_lossy(&commit.stderr);
        anyhow::bail!("git commit failed: {err}");
    }

    // Get the short hash
    let hash = git().args(["rev-parse", "--short", "HEAD"]).output()?;
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
            router: None,
            compact_router: None,
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

    fn task(
        url: String,
        dir: &std::path::Path,
        budget: Option<f64>,
    ) -> (ApiTask, mpsc::UnboundedReceiver<AppEvent>) {
        let config = Config {
            model: "claude-sonnet-5".into(),
            api_key: "sk-ant-test".into(),
            cwd: dir.to_path_buf(),
            ..Config::default()
        };
        let mut c = crate::api::ClaudeClient::new("sk-ant-test").unwrap();
        c.set_base_url_for_test(url);
        let (tx, rx) = mpsc::unbounded_channel();
        let task = ApiTask {
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
            plan_mode: false,
            skill_no_shell: false,
            session_id: "s".into(),
            budget_remaining_usd: budget,
            history: TurnHistory::default(),
            router: None,
            compact_router: None,
        };
        (task, rx)
    }

    /// Only Anthropic's "prompt is too long" took the compact-and-retry
    /// path; an OpenAI-style overflow failed the turn, and the oversized
    /// history it left behind failed every later prompt too.
    #[tokio::test]
    async fn an_openai_style_overflow_compacts_and_retries() {
        use crate::query_engine::scripted_api_tests::{serve, sse};
        let body = r#"{"error":{"message":"This model's maximum context length is 65536 tokens. However, you requested 70321 tokens.","type":"invalid_request_error","code":"context_length_exceeded"}}"#;
        let overflow = format!(
            "HTTP/1.1 400 Bad Request\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        let text = |t: &str| sse(&[serde_json::json!({"type":"text","text":t})], "end_turn");
        let (url, seen) = serve(vec![overflow, text("summary"), text("answer")]).await;
        let dir = tempfile::tempdir().unwrap();
        let (t, mut rx) = task(url, dir.path(), None);
        run_api_task(t).await;

        assert_eq!(seen.lock().unwrap().len(), 3);
        let (mut compacted, mut done, mut failed) = (false, false, None);
        while let Ok(ev) = rx.try_recv() {
            match ev {
                AppEvent::Compacted { .. } => compacted = true,
                AppEvent::Done { .. } => done = true,
                AppEvent::TurnFailed(e) => failed = Some(e),
                _ => {}
            }
        }
        assert_eq!(failed, None);
        assert!(compacted && done);
    }

    /// When the system prompt and tools alone exceed the window, every
    /// compacted retry overflowed again while the small summary request
    /// kept succeeding, so the turn summarised its own summary up to 50
    /// times. A second overflow straight after compacting ends the turn.
    #[tokio::test]
    async fn an_overflow_that_compacting_cannot_fix_fails_the_turn() {
        use crate::query_engine::scripted_api_tests::{serve, sse};
        let body = r#"{"error":{"message":"This model's maximum context length is 8192 tokens. However, you requested 9000 tokens.","type":"invalid_request_error","code":"context_length_exceeded"}}"#;
        let overflow = format!(
            "HTTP/1.1 400 Bad Request\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        let text = |t: &str| sse(&[serde_json::json!({"type":"text","text":t})], "end_turn");
        let (url, seen) = serve(vec![
            overflow.clone(),
            text("summary"),
            overflow.clone(),
            text("summary of the summary"),
            overflow,
            text("answer"),
        ])
        .await;
        let dir = tempfile::tempdir().unwrap();
        let (t, mut rx) = task(url, dir.path(), None);
        run_api_task(t).await;

        assert_eq!(seen.lock().unwrap().len(), 3, "compacted more than once");
        let (mut compactions, mut failed) = (0, None);
        while let Ok(ev) = rx.try_recv() {
            match ev {
                AppEvent::Compacted { .. } => compactions += 1,
                AppEvent::TurnFailed(e) => failed = Some(e),
                _ => {}
            }
        }
        assert_eq!(compactions, 1);
        assert!(
            failed
                .as_deref()
                .is_some_and(|e| e.contains("after compacting")),
            "{failed:?}"
        );
    }

    /// The mid-turn compact never ran the documented preCompact /
    /// postCompact hooks; only the between-turns auto-compact did.
    #[tokio::test]
    async fn prompt_too_long_compact_runs_compact_hooks() {
        use crate::query_engine::scripted_api_tests::{serve, sse};
        let body = r#"{"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long: 210000 tokens > 200000 maximum"}}"#;
        let overflow = format!(
            "HTTP/1.1 400 Bad Request\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        let text = |t: &str| sse(&[serde_json::json!({"type":"text","text":t})], "end_turn");
        let (url, _seen) = serve(vec![overflow, text("summary"), text("answer")]).await;
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("hooks.log");
        let hook = |tag: &str| crate::settings::HookEntry {
            matcher: String::new(),
            command: format!("echo {tag} >> '{}'", log.display()),
        };
        let (mut t, mut rx) = task(url, dir.path(), None);
        t.config.hooks = Some(crate::settings::HooksConfig {
            pre_compact: vec![hook("pre")],
            post_compact: vec![hook("post")],
            ..Default::default()
        });
        run_api_task(t).await;

        let mut compacted = false;
        while let Ok(ev) = rx.try_recv() {
            if let AppEvent::Compacted { .. } = ev {
                compacted = true;
            }
        }
        assert!(compacted);
        assert_eq!(std::fs::read_to_string(&log).unwrap(), "pre\npost\n");
    }

    /// A slash command that starts a turn (/review, a skill, a plugin
    /// command) over the /budget cap sent one more request before the
    /// Usage handler stopped it.
    #[tokio::test]
    async fn a_turn_started_over_budget_sends_nothing() {
        use crate::query_engine::scripted_api_tests::{serve, sse};
        let (url, seen) = serve(vec![sse(
            &[serde_json::json!({"type":"text","text":"spent"})],
            "end_turn",
        )])
        .await;
        let dir = tempfile::tempdir().unwrap();
        let (t, mut rx) = task(url, dir.path(), Some(0.0));
        run_api_task(t).await;

        assert!(seen.lock().unwrap().is_empty(), "request sent over budget");
        let mut failed = None;
        while let Ok(ev) = rx.try_recv() {
            if let AppEvent::TurnFailed(e) = ev {
                failed = Some(e);
            }
        }
        assert!(failed.is_some_and(|e| e.contains("Budget")));
    }

    /// Stands in for the Agent tool: records whether the gate it would
    /// hand its sub-agent refuses Bash. An unblocked gate would prompt the
    /// user, which nobody answers here, hence the timeout.
    struct AgentProbe(std::sync::Arc<std::sync::Mutex<Vec<bool>>>);
    #[async_trait::async_trait]
    impl crate::tools::Tool for AgentProbe {
        fn name(&self) -> &str {
            "Agent"
        }
        fn description(&self) -> &str {
            "test"
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        async fn execute(&self, _: serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput> {
            let gate = ctx.permission_gate.clone().unwrap();
            let decided = tokio::time::timeout(
                Duration::from_millis(200),
                gate.decide("Bash", &serde_json::json!({"command": "touch x"})),
            )
            .await;
            let denied = matches!(decided, Ok(GateOutcome::Denied(_)));
            self.0.lock().unwrap().push(denied);
            Ok(ToolOutput::success("ok"))
        }
    }

    /// EnterPlanMode without the approval round-trip.
    struct Planner;
    #[async_trait::async_trait]
    impl crate::tools::Tool for Planner {
        fn name(&self) -> &str {
            "Planner"
        }
        fn description(&self) -> &str {
            "test"
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        async fn execute(&self, _: serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput> {
            let _ = ctx.plan_mode_tx.as_ref().unwrap().send(true);
            Ok(ToolOutput::success("planning"))
        }
    }

    async fn run_with_probe(plan_mode: bool, calls: &[&str]) -> (Vec<bool>, Vec<String>) {
        use crate::query_engine::scripted_api_tests::{serve, sse};
        let blocks: Vec<_> = calls
            .iter()
            .enumerate()
            .map(|(i, n)| serde_json::json!({"type":"tool_use","id":format!("t{i}"),"name":n,"input":{}}))
            .collect();
        let (url, _) = serve(vec![
            sse(&blocks, "tool_use"),
            sse(
                &[serde_json::json!({"type":"text","text":"done"})],
                "end_turn",
            ),
        ])
        .await;
        let dir = tempfile::tempdir().unwrap();
        let (mut t, mut rx) = task(url, dir.path(), None);
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        t.tools = vec![
            std::sync::Arc::new(AgentProbe(seen.clone())),
            std::sync::Arc::new(Planner),
        ];
        t.plan_mode = plan_mode;
        t.perm_state = PermissionState::new(false, &["Agent".into(), "Planner".into()], &[]);
        run_api_task(t).await;
        let mut errors = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            if let AppEvent::ToolResult {
                is_error: true,
                text,
            } = ev
            {
                errors.push(text);
            }
        }
        let seen = seen.lock().unwrap().clone();
        (seen, errors)
    }

    /// Plan mode refused the Agent tool outright, so the read-only Explore
    /// and Plan helpers could not research a plan; the sub-agent inherits
    /// the plan-mode blocks through the gate instead.
    #[tokio::test]
    async fn plan_mode_runs_agents_under_its_blocks() {
        let (seen, errors) = run_with_probe(true, &["Agent"]).await;
        assert_eq!(errors, Vec::<String>::new());
        assert_eq!(seen, vec![true]);
    }

    /// The gate was built once per response, so an Agent launched after
    /// EnterPlanMode in the same response got an unblocked one.
    #[tokio::test]
    async fn an_agent_after_entering_plan_mode_inherits_the_blocks() {
        let (seen, errors) = run_with_probe(false, &["Planner", "Agent"]).await;
        assert_eq!(errors, Vec::<String>::new());
        assert_eq!(seen, vec![true]);
    }

    /// Config read plan mode from the registry's startup snapshot, so it
    /// said `plan_mode: false` after EnterPlanMode (or `/plan`).
    #[tokio::test]
    async fn config_tool_reports_plan_mode_entered_mid_response() {
        use crate::query_engine::scripted_api_tests::{serve, sse};
        let blocks: Vec<_> = ["Config", "Planner", "Config"]
            .iter()
            .enumerate()
            .map(|(i, n)| serde_json::json!({"type":"tool_use","id":format!("t{i}"),"name":n,"input":{}}))
            .collect();
        let (url, _) = serve(vec![
            sse(&blocks, "tool_use"),
            sse(
                &[serde_json::json!({"type":"text","text":"done"})],
                "end_turn",
            ),
        ])
        .await;
        let dir = tempfile::tempdir().unwrap();
        let (mut t, mut rx) = task(url, dir.path(), None);
        t.tools = vec![
            std::sync::Arc::new(crate::tools::config_tool::ConfigTool {
                config: t.config.clone(),
            }),
            std::sync::Arc::new(Planner),
        ];
        t.perm_state = PermissionState::new(false, &["Config".into(), "Planner".into()], &[]);
        run_api_task(t).await;
        let mut reports = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            if let AppEvent::ToolResult { text, .. } = ev
                && let Some(line) = text.lines().find(|l| l.starts_with("plan_mode:"))
            {
                reports.push(line.to_string());
            }
        }
        assert_eq!(reports, vec!["plan_mode: false", "plan_mode: true"]);
    }

    /// Stands in for Write: succeeds without touching the disk.
    struct FakeWrite;
    #[async_trait::async_trait]
    impl crate::tools::Tool for FakeWrite {
        fn name(&self) -> &str {
            "Write"
        }
        fn description(&self) -> &str {
            "test"
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        async fn execute(&self, _: serde_json::Value, _: &ToolContext) -> Result<ToolOutput> {
            Ok(ToolOutput::success("written"))
        }
    }

    /// The model's reply: one Write of `a.txt`, which starts the auto-fix
    /// check once it has run.
    fn write_a_txt_response() -> String {
        let input = serde_json::json!({"file_path": "a.txt", "content": "x"}).to_string();
        let events = [
            serde_json::json!({"type":"message_start","message":{"id":"m","type":"message","role":"assistant","content":[],"model":"x","stop_reason":null,"usage":{"input_tokens":1,"output_tokens":0}}}),
            serde_json::json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"w1","name":"Write","input":{}}}),
            serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":input}}),
            serde_json::json!({"type":"content_block_stop","index":0}),
            serde_json::json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":5}}),
            serde_json::json!({"type":"message_stop"}),
        ];
        let body: String = events.iter().map(|e| format!("data: {e}\n\n")).collect();
        format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    /// Esc aborts the task while the auto-fix check runs; the lint/test
    /// process it started ran on, unseen, until it finished or timed out.
    #[cfg(unix)]
    #[tokio::test]
    async fn aborting_the_turn_kills_the_running_auto_fix_check() {
        use crate::query_engine::scripted_api_tests::serve;
        let write = write_a_txt_response();
        let (url, _) = serve(vec![write]).await;
        let dir = tempfile::tempdir().unwrap();
        let (mut t, mut rx) = task(url, dir.path(), None);
        t.tools = vec![std::sync::Arc::new(FakeWrite)];
        t.perm_state = PermissionState::new(false, &["Write".into()], &[]);
        t.config.autonomy = crate::permissions::Autonomy::AutoEdit;
        t.config.project_trusted = true;
        t.config.auto_fix = crate::autofix::AutoFixConfig {
            trigger: crate::autofix::AutoFixTrigger::Always,
            test_command: Some("echo $$ > check.pid; exec sleep 30".into()),
            timeout_secs: 0,
            ..Default::default()
        };
        let pid_file = dir.path().join("check.pid");
        let handle = tokio::spawn(run_api_task(t));
        let pid = loop {
            if let Ok(p) = std::fs::read_to_string(&pid_file)
                && let Ok(p) = p.trim().parse::<i32>()
            {
                break p;
            }
            assert!(!handle.is_finished(), "the check never started");
            tokio::time::sleep(Duration::from_millis(50)).await;
        };
        handle.abort();
        let _ = handle.await;

        let alive = || unsafe { libc::kill(pid, 0) } == 0;
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while alive() && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let still_running = alive();
        if still_running {
            unsafe { libc::kill(-pid, libc::SIGKILL) };
        }
        assert!(!still_running, "the check outlived the cancelled turn");
        while let Ok(ev) = rx.try_recv() {
            assert!(
                !matches!(ev, AppEvent::Done { .. }),
                "a cancelled turn sent Done"
            );
        }
    }

    /// Auto-fix ran the project's lint and test commands after every edit,
    /// trusted folder or not.
    #[tokio::test]
    async fn an_untrusted_project_edit_runs_no_auto_fix_command() {
        use crate::query_engine::scripted_api_tests::{serve, sse};
        let done = sse(
            &[serde_json::json!({"type":"text","text":"done"})],
            "end_turn",
        );
        let (url, _) = serve(vec![write_a_txt_response(), done]).await;
        let dir = tempfile::tempdir().unwrap();
        let (mut t, mut rx) = task(url, dir.path(), None);
        t.tools = vec![std::sync::Arc::new(FakeWrite)];
        t.perm_state = PermissionState::new(false, &["Write".into()], &[]);
        t.config.autonomy = crate::permissions::Autonomy::AutoEdit;
        assert!(!t.config.project_trusted);
        t.config.auto_fix = crate::autofix::AutoFixConfig {
            lint_command: Some("touch lint.marker".into()),
            test_command: Some("touch test.marker".into()),
            ..Default::default()
        };
        run_api_task(t).await;

        assert!(!dir.path().join("lint.marker").exists());
        assert!(!dir.path().join("test.marker").exists());
        let (mut untrusted, mut done) = (0, false);
        while let Ok(ev) = rx.try_recv() {
            match ev {
                AppEvent::AutoFixUntrusted => untrusted += 1,
                AppEvent::Done { .. } => done = true,
                AppEvent::TurnFailed(e) => panic!("turn failed: {e}"),
                _ => {}
            }
        }
        assert_eq!(untrusted, 1);
        assert!(done);
    }

    /// `/autonomy auto-edit` was stored and never read: the TUI prompted
    /// for an in-project Write in every mode. Now auto-edit runs it without
    /// a prompt and the default `ask` still prompts.
    #[tokio::test]
    async fn the_tui_gate_applies_the_autonomy_mode() {
        use crate::permissions::Autonomy;
        use crate::query_engine::scripted_api_tests::{serve, sse};
        for (mode, prompted) in [(Autonomy::AutoEdit, false), (Autonomy::Ask, true)] {
            let done = sse(
                &[serde_json::json!({"type":"text","text":"done"})],
                "end_turn",
            );
            let (url, _) = serve(vec![write_a_txt_response(), done]).await;
            let dir = tempfile::tempdir().unwrap();
            let (mut t, mut rx) = task(url, dir.path(), None);
            t.tools = vec![std::sync::Arc::new(FakeWrite)];
            t.perm_state = PermissionState::new(false, &[], &[]).with_cwd(dir.path());
            t.config.autonomy = mode;
            let handle = tokio::spawn(run_api_task(t));
            let (mut asked, mut result) = (0, None);
            while let Ok(Some(ev)) = tokio::time::timeout(Duration::from_secs(10), rx.recv()).await
            {
                match ev {
                    // Dropping the reply denies the call.
                    AppEvent::PermissionRequest { tool_name, .. } => {
                        assert_eq!(tool_name, "Write");
                        asked += 1;
                    }
                    AppEvent::ToolResult { text, .. } => result = Some(text),
                    AppEvent::Done { .. } => break,
                    AppEvent::TurnFailed(e) => panic!("turn failed: {e}"),
                    _ => {}
                }
            }
            handle.await.unwrap();
            assert_eq!(asked, usize::from(prompted), "{mode}");
            let result = result.expect("the Write got a result");
            assert_eq!(result.contains("written"), !prompted, "{mode}: {result}");
        }
    }

    /// A connection that dies after the headers but before any text is
    /// safe to re-send; its error text never matched the retry check, so
    /// the turn failed on the first network blip.
    #[tokio::test]
    async fn a_connection_dropped_before_any_text_is_retried() {
        use crate::query_engine::scripted_api_tests::{serve, sse};
        let start = r#"data: {"type":"message_start","message":{"id":"m","type":"message","role":"assistant","content":[],"model":"x","stop_reason":null,"usage":{"input_tokens":1,"output_tokens":0}}}"#;
        // The body promises more than it delivers before the socket closes.
        let cut = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: 100000\r\n\r\n{start}\n\n"
        );
        let (url, seen) = serve(vec![
            cut,
            sse(
                &[serde_json::json!({"type":"text","text":"answer"})],
                "end_turn",
            ),
        ])
        .await;
        let dir = tempfile::tempdir().unwrap();
        let (t, mut rx) = task(url, dir.path(), None);
        run_api_task(t).await;

        assert_eq!(seen.lock().unwrap().len(), 2);
        let (mut done, mut failed) = (false, None);
        while let Ok(ev) = rx.try_recv() {
            match ev {
                AppEvent::Done { .. } => done = true,
                AppEvent::TurnFailed(e) => failed = Some(e),
                _ => {}
            }
        }
        assert_eq!(failed, None);
        assert!(done);
    }

    /// A turn that edited only through MultiEdit never ran lint or tests:
    /// its file paths are inside `edits`, not at the top level.
    #[test]
    fn multiedit_paths_trigger_the_auto_fix_check() {
        use crate::tools::ToolOutput;
        let input = serde_json::json!({"edits": [
            {"file_path": "/w/a.rs", "old_string": "x", "new_string": "y"},
            {"file_path": "/w/b.rs", "old_string": "x", "new_string": "y"},
        ]});
        let ok = ToolOutput::success("[1/2] /w/a.rs ✓ Edit applied\n[2/2] /w/b.rs ✓ Edit applied");
        assert_eq!(
            edited_paths("MultiEdit", &input, &ok),
            vec![
                std::path::PathBuf::from("/w/a.rs"),
                std::path::PathBuf::from("/w/b.rs")
            ]
        );
        // One file failed, the other was still written.
        let partial =
            ToolOutput::error("[1/2] /w/a.rs ✓ Edit applied\n[2/2] /w/b.rs ✗ old_string not found");
        assert_eq!(edited_paths("MultiEdit", &input, &partial).len(), 2);
        // Nothing applied (denied, or every edit failed): no check.
        let none =
            ToolOutput::error("[1/2] /w/a.rs ✗ File not found\n[2/2] /w/b.rs ✗ File not found");
        assert!(edited_paths("MultiEdit", &input, &none).is_empty());

        let edit =
            serde_json::json!({"file_path": "/w/c.rs", "old_string": "x", "new_string": "y"});
        assert_eq!(
            edited_paths("Edit", &edit, &ToolOutput::success("ok")),
            vec![std::path::PathBuf::from("/w/c.rs")]
        );
        assert!(edited_paths("Edit", &edit, &ToolOutput::error("no")).is_empty());
        assert!(edited_paths("Read", &edit, &ToolOutput::success("ok")).is_empty());
    }
}

#[cfg(test)]
mod router_tests {
    use super::*;
    use crate::router::fake_chat::{self, Reply};

    /// A routed turn whose tiers all live on one fake Ollama host: "yes"
    /// is a low-tier prompt.
    async fn routed_task(
        reply: impl Fn(&str, usize) -> Reply + Send + Sync + 'static,
        dir: &std::path::Path,
    ) -> (
        ApiTask,
        mpsc::UnboundedReceiver<AppEvent>,
        std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    ) {
        let (host, seen) = fake_chat::start(reply).await;
        let config = Config {
            model: "ollama:big".into(),
            ollama_host: host,
            cwd: dir.to_path_buf(),
            ..Config::default()
        };
        let mut router = crate::router::RouterConfig::new(&config.model);
        router.enabled = true;
        router.low_model = "ollama:small".into();
        router.medium_model = "ollama:mid".into();
        let (tx, rx) = mpsc::unbounded_channel();
        let task = ApiTask {
            client: config.backend_for(&config.model).unwrap(),
            tools: Vec::new(),
            messages: vec![Message {
                role: Role::User,
                content: vec![ContentBlock::Text { text: "yes".into() }],
            }],
            config,
            perm_state: PermissionState::new(false, &[], &[]),
            system_prompt: String::new(),
            tx,
            plan_mode: false,
            skill_no_shell: false,
            session_id: "s".into(),
            budget_remaining_usd: None,
            history: TurnHistory::default(),
            router: Some(router),
            compact_router: None,
        };
        (task, rx, seen)
    }

    /// The classifier's usage is billed but is not the session's context:
    /// taken for it, the status bar's ctx % fell to near 0% until the
    /// turn's first response.
    #[tokio::test]
    async fn the_classifier_call_is_billed_without_moving_the_ctx_gauge() {
        let dir = tempfile::tempdir().unwrap();
        let (mut task, mut rx, seen) = routed_task(
            |_, n| {
                if n == 0 {
                    Reply::Text("low")
                } else {
                    Reply::Text("done")
                }
            },
            dir.path(),
        )
        .await;
        task.router.as_mut().unwrap().classifier = crate::router::Classifier::Model;
        run_api_task(task).await;

        assert_eq!(*seen.lock().unwrap(), vec!["small", "small"]);
        let mut usage = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            if let AppEvent::Usage { model, context, .. } = ev {
                usage.push((model, context));
            }
        }
        assert_eq!(
            usage,
            vec![
                ("ollama:small".to_string(), false),
                ("ollama:small".to_string(), true)
            ]
        );
    }

    /// The low tier fails; the turn finishes one tier up, the status line
    /// learns both models, and the escalation is one line.
    #[tokio::test]
    async fn a_failed_low_tier_turn_finishes_one_tier_up() {
        let dir = tempfile::tempdir().unwrap();
        let (task, mut rx, seen) = routed_task(
            |model, _| match model {
                "small" => Reply::Status(400, r#"{"error":"unsupported tool format"}"#),
                _ => Reply::Text("fixed"),
            },
            dir.path(),
        )
        .await;
        run_api_task(task).await;

        assert_eq!(*seen.lock().unwrap(), vec!["small", "mid"]);
        let (mut routed, mut done, mut failed) = (Vec::new(), None, None);
        while let Ok(ev) = rx.try_recv() {
            match ev {
                AppEvent::Routed { model, line } => routed.push((model, line)),
                AppEvent::Done { model_used, .. } => done = Some(model_used),
                AppEvent::TurnFailed(e) => failed = Some(e),
                _ => {}
            }
        }
        assert_eq!(failed, None);
        assert_eq!(done.as_deref(), Some("ollama:mid"));
        assert_eq!(routed.len(), 2, "{routed:?}");
        assert_eq!(routed[0].0, "ollama:small");
        assert!(routed[0].1.contains("heuristic"), "{}", routed[0].1);
        assert_eq!(routed[1].0, "ollama:mid");
        assert!(
            routed[1].1.contains("API error on ollama:small"),
            "{}",
            routed[1].1
        );
    }

    /// Inside a turn the TUI never measured the context against the tier's
    /// window, so an Ollama tier truncated the history silently. A response
    /// at 90% of the low tier's 32k window goes one tier up (128k) for the
    /// rest of the turn.
    #[tokio::test]
    async fn a_low_tier_filling_its_window_mid_turn_goes_up() {
        let dir = tempfile::tempdir().unwrap();
        let (mut task, mut rx, seen) = routed_task(
            |model, _| match model {
                "gemma3:1b" => Reply::ToolAt("Glob", r#"{"pattern":"*.rs"}"#, 30_000),
                _ => Reply::Text("done"),
            },
            dir.path(),
        )
        .await;
        let router = task.router.as_mut().unwrap();
        router.low_model = "ollama:gemma3:1b".into();
        router.medium_model = "ollama:mid".into();
        task.tools = vec![std::sync::Arc::new(crate::tools::glob::GlobTool) as DynTool];
        run_api_task(task).await;

        assert_eq!(*seen.lock().unwrap(), vec!["gemma3:1b", "mid"]);
        let mut done = None;
        while let Ok(ev) = rx.try_recv() {
            if let AppEvent::Done { model_used, .. } = ev {
                done = Some(model_used);
            }
        }
        assert_eq!(done.as_deref(), Some("ollama:mid"));
    }

    /// With no larger tier to go to, old tool results are stripped before
    /// the next request instead of letting Ollama cut the history.
    #[tokio::test]
    async fn a_full_window_mid_turn_snips_without_a_larger_tier() {
        let dir = tempfile::tempdir().unwrap();
        let (mut task, mut rx, seen) = routed_task(
            |model, n| match (model, n) {
                ("gemma3:1b", 0) => Reply::ToolAt("Glob", r#"{"pattern":"*.rs"}"#, 28_000),
                _ => Reply::Text("done"),
            },
            dir.path(),
        )
        .await;
        task.config.model = "ollama:gemma3:1b".into();
        task.client = task.config.backend_for(&task.config.model).unwrap();
        task.router = None;
        let mut history = Vec::new();
        for i in 0..12 {
            history.push(Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: format!("old{i}"),
                    name: "Read".into(),
                    input: serde_json::json!({"file_path": "a.rs"}),
                }],
            });
            history.push(Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: format!("old{i}"),
                    content: vec![ToolResultContent::Text {
                        text: "OLD FILE BODY".into(),
                    }],
                    is_error: None,
                }],
            });
        }
        history.append(&mut task.messages);
        task.messages = history;
        task.tools = vec![std::sync::Arc::new(crate::tools::glob::GlobTool) as DynTool];
        run_api_task(task).await;

        assert_eq!(*seen.lock().unwrap(), vec!["gemma3:1b", "gemma3:1b"]);
        let mut done = None;
        while let Ok(ev) = rx.try_recv() {
            if let AppEvent::Done { messages, .. } = ev {
                done = Some(messages);
            }
        }
        let messages = done.expect("turn finished");
        let oldest = serde_json::to_string(&messages[1]).unwrap();
        assert!(!oldest.contains("OLD FILE BODY"), "old tool result kept");
        let newest = serde_json::to_string(&messages[23]).unwrap();
        assert!(newest.contains("OLD FILE BODY"), "recent results stay");
    }

    /// A turn the router does not route (/review, a skill) on a model whose
    /// window the history outgrew: the summary went to that same model and
    /// failed. It goes to the router's largest tier.
    #[tokio::test]
    async fn an_unrouted_overflow_is_summarised_on_the_largest_tier() {
        let dir = tempfile::tempdir().unwrap();
        let (mut task, mut rx, seen) = routed_task(
            |model, n| match (model, n) {
                ("gemma3:1b", 0) => Reply::Status(400, r#"{"error":"context_length_exceeded"}"#),
                ("mid", _) => Reply::Text("summary"),
                _ => Reply::Text("answer"),
            },
            dir.path(),
        )
        .await;
        task.config.model = "ollama:gemma3:1b".into();
        task.client = task.config.backend_for(&task.config.model).unwrap();
        let mut router = task.router.take().unwrap();
        router.low_model = "ollama:gemma3:1b".into();
        router.high_model = "ollama:gemma3:1b".into();
        router.super_high_model = "ollama:gemma3:1b".into();
        task.compact_router = Some(router);
        run_api_task(task).await;

        assert_eq!(*seen.lock().unwrap(), vec!["gemma3:1b", "mid", "gemma3:1b"]);
        let (mut compacted, mut done, mut failed) = (false, None, None);
        while let Ok(ev) = rx.try_recv() {
            match ev {
                AppEvent::Compacted { .. } => compacted = true,
                AppEvent::Done { model_used, .. } => done = Some(model_used),
                AppEvent::TurnFailed(e) => failed = Some(e),
                _ => {}
            }
        }
        assert_eq!(failed, None);
        assert!(compacted);
        assert_eq!(done.as_deref(), Some("ollama:gemma3:1b"));
    }

    /// The loop detector on the cheap tier: the turn continues one tier up
    /// with the repeated results in its history instead of pausing.
    #[tokio::test]
    async fn a_loop_on_the_low_tier_escalates_once() {
        let dir = tempfile::tempdir().unwrap();
        let (mut task, mut rx, seen) = routed_task(
            |model, _| match model {
                "small" => Reply::Tool("Read", r#"{"file_path":"missing.rs"}"#),
                _ => Reply::Text("the file does not exist"),
            },
            dir.path(),
        )
        .await;
        task.tools = vec![std::sync::Arc::new(crate::tools::file_read::FileReadTool) as DynTool];
        run_api_task(task).await;

        assert_eq!(
            *seen.lock().unwrap(),
            vec!["small", "small", "small", "mid"]
        );
        let mut done = None;
        let mut loop_paused = false;
        while let Ok(ev) = rx.try_recv() {
            match ev {
                AppEvent::Done { messages, .. } => done = Some(messages),
                AppEvent::SystemMessage(m) if m.contains("Loop detected") => loop_paused = true,
                _ => {}
            }
        }
        assert!(!loop_paused);
        let messages = done.expect("turn finished");
        // Every tool_use answered, the last word is the mid tier's.
        assert_eq!(messages.last().unwrap().role, Role::Assistant);
        assert_eq!(messages.len(), 8);
    }
}
