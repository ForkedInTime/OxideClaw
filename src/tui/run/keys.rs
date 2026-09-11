//! Key handling: `KeyCtx` and `handle_key` (overlays, editing, submit).
//! Split out of `tui/run.rs` mechanically — no behaviour change.

use super::*;

/// Bundle of references `handle_key` needs to mutate app/session state and react
/// to the current key event. Exists purely to keep the argument count below the
/// clippy::too_many_arguments threshold; the body destructures it on entry and
/// uses the fields directly.
pub(super) struct KeyCtx<'a> {
    pub(super) key: crossterm::event::KeyEvent,
    pub(super) app: &'a mut App,
    pub(super) messages: &'a mut Vec<Message>,
    pub(super) client: &'a mut ApiBackend,
    pub(super) tools: &'a [DynTool],
    pub(super) config: &'a mut Config,
    pub(super) perm_state: &'a PermissionState,
    pub(super) skills: &'a std::collections::HashMap<String, crate::skills::Skill>,
    pub(super) system_prompt: &'a mut String,
    pub(super) tx: &'a mpsc::UnboundedSender<AppEvent>,
    pub(super) todo_state: &'a TodoState,
    pub(super) session: &'a mut Session,
    pub(super) saved_count: &'a mut usize,
    pub(super) mcp_statuses: &'a [crate::mcp::types::McpServerStatus],
    pub(super) turn_counter: &'a mut usize,
    pub(super) spawn_registry: &'a crate::spawn::SpawnRegistry,
}

pub(super) async fn handle_key(ctx: KeyCtx<'_>) -> Result<()> {
    let KeyCtx {
        key,
        app,
        messages,
        client,
        tools,
        config,
        perm_state,
        skills,
        system_prompt,
        tx,
        todo_state,
        session,
        saved_count,
        mcp_statuses,
        turn_counter,
        spawn_registry,
    } = ctx;
    use KeyCode::*;

    // Overlay dismissal takes second priority (after permission dialog)
    if app.overlay.is_some() {
        let is_interactive = app.overlay.as_ref().is_some_and(|o| o.is_interactive());
        match key.code {
            KeyCode::Esc | KeyCode::Char('q')
                if app
                    .overlay
                    .as_ref()
                    .is_some_and(|o| o.title == "help-commands") =>
            {
                // Back to category picker instead of closing entirely
                let cats = crate::commands::HELP_CATEGORIES;
                let mut lines = vec![format!("Help — pick a category ({})\n", cats.len())];
                let mut ids = Vec::new();
                for (i, (name, desc, _)) in cats.iter().enumerate() {
                    lines.push(format!("  {}. {} — {}", i + 1, name, desc));
                    ids.push(i.to_string());
                }
                lines.push(String::new());
                lines.push("  ↑↓ select · Enter open · 1-9 quick pick · Esc close".into());
                app.overlay = Some(Overlay::with_items("help", lines.join("\n"), ids));
            }
            KeyCode::Esc | KeyCode::Char('q') => {
                app.overlay = None;
                app.pending_undo_positions = None;
                app.pending_redo_positions = None;
            }
            KeyCode::Enter if is_interactive => {
                let title = app
                    .overlay
                    .as_ref()
                    .map(|o| o.title.clone())
                    .unwrap_or_default();
                let selected_index = app.overlay.as_ref().map(|o| o.selected).unwrap_or(0);
                let selected_val = app
                    .overlay
                    .as_ref()
                    .and_then(|o| o.selectable_ids.get(o.selected).cloned());
                app.overlay = None;
                if title == "undo" {
                    let positions = app.pending_undo_positions.take();
                    let target_pos = positions
                        .as_ref()
                        .and_then(|p| p.get(selected_index))
                        .copied();
                    if let Some(target_pos) =
                        target_pos.filter(|&p| p != session.meta.undo_position)
                    {
                        match oxideclaw::autocommit::restore_to(
                            &config.cwd,
                            &session.meta.auto_commits,
                            target_pos,
                        ) {
                            Ok(report) => {
                                session.meta.undo_position = target_pos;
                                if let Err(e) = session.save_meta().await {
                                    tracing::warn!("[undo] failed to save meta: {e}");
                                }
                                let label = if target_pos == 0 {
                                    "session base".to_string()
                                } else {
                                    format!("turn {target_pos}")
                                };
                                app.entries.push(ChatEntry::system(format!(
                                    "[undo] rewound to {label} ({} files restored)",
                                    report.files_restored
                                )));
                            }
                            Err(e) => {
                                app.entries
                                    .push(ChatEntry::system(format!("[undo] restore failed: {e}")));
                            }
                        }
                    }
                } else if title == "redo" {
                    let positions = app.pending_redo_positions.take();
                    let target_pos = positions
                        .as_ref()
                        .and_then(|p| p.get(selected_index))
                        .copied();
                    if let Some(target_pos) =
                        target_pos.filter(|&p| p != session.meta.undo_position)
                    {
                        match oxideclaw::autocommit::restore_to(
                            &config.cwd,
                            &session.meta.auto_commits,
                            target_pos,
                        ) {
                            Ok(report) => {
                                session.meta.undo_position = target_pos;
                                if let Err(e) = session.save_meta().await {
                                    tracing::warn!("[redo] failed to save meta: {e}");
                                }
                                app.entries.push(ChatEntry::system(format!(
                                    "[redo] advanced to turn {target_pos} ({} files restored)",
                                    report.files_restored
                                )));
                            }
                            Err(e) => {
                                app.entries
                                    .push(ChatEntry::system(format!("[redo] restore failed: {e}")));
                            }
                        }
                    }
                } else if let Some(val) = selected_val {
                    if title == "models" {
                        app.pending_model = Some(val);
                    } else if title == "help" {
                        if let Ok(idx) = val.parse::<usize>() {
                            app.pending_help_category = Some(idx);
                        }
                    } else if title == "help-commands" {
                        app.pending_help_command = Some(val);
                    } else if title == "voices" {
                        app.pending_voice_model = Some(val);
                    } else {
                        app.pending_resume = Some(val);
                    }
                }
            }
            KeyCode::Enter => {
                app.overlay = None;
            }
            KeyCode::Char(c @ '1'..='9') if is_interactive => {
                let idx = (c as usize) - ('1' as usize);
                let title = app
                    .overlay
                    .as_ref()
                    .map(|o| o.title.clone())
                    .unwrap_or_default();
                let selected_val = app
                    .overlay
                    .as_ref()
                    .and_then(|o| o.selectable_ids.get(idx).cloned());
                app.overlay = None;
                if let Some(val) = selected_val {
                    if title == "models" {
                        app.pending_model = Some(val);
                    } else if title == "help" {
                        if let Ok(cat_idx) = val.parse::<usize>() {
                            app.pending_help_category = Some(cat_idx);
                        }
                    } else if title == "help-commands" {
                        app.pending_help_command = Some(val);
                    } else if title == "voices" {
                        app.pending_voice_model = Some(val);
                    } else {
                        app.pending_resume = Some(val);
                    }
                }
            }
            KeyCode::Char('d') | KeyCode::Delete if is_interactive => {
                // Delete the selected session
                let selected_id = app
                    .overlay
                    .as_ref()
                    .and_then(|o| o.selectable_ids.get(o.selected).cloned());
                if let Some(id) = selected_id {
                    // Don't allow deleting the current session
                    if id == session.id {
                        app.entries.push(ChatEntry::system(
                            "Cannot delete the current session.".to_string(),
                        ));
                    } else {
                        app.pending_delete = Some(id);
                    }
                }
            }
            KeyCode::Up if is_interactive => {
                if let Some(o) = &mut app.overlay {
                    o.select_up();
                }
            }
            KeyCode::Down if is_interactive => {
                if let Some(o) = &mut app.overlay {
                    o.select_down();
                }
            }
            KeyCode::Up | KeyCode::PageUp => {
                if let Some(o) = &mut app.overlay {
                    o.scroll_up();
                }
            }
            KeyCode::Down | KeyCode::PageDown => {
                if let Some(o) = &mut app.overlay {
                    o.scroll += 5;
                }
            }
            _ => {}
        }
        return Ok(());
    }

    // Permission dialog takes priority
    if app.pending_permission.is_some() {
        match key.code {
            Char('y') | Char('Y') => {
                if let Some(p) = app.pending_permission.take() {
                    let _ = p.reply.send(PermissionDecision::Allow);
                }
            }
            Char('a') | Char('A') => {
                if let Some(p) = app.pending_permission.take() {
                    perm_state.record_always_allow(&p.tool_name);
                    let _ = p.reply.send(PermissionDecision::AlwaysAllow);
                }
            }
            Char('n') | Char('N') | Esc => {
                if let Some(p) = app.pending_permission.take() {
                    let _ = p.reply.send(PermissionDecision::Deny);
                }
            }
            _ => {}
        }
        return Ok(());
    }

    // Browse approval dialog takes priority after permission dialog
    if app.browse_approval.is_some() {
        match key.code {
            Char('a') | Char('A') => {
                if let Some(prompt) = app.browse_approval.take() {
                    let _ = prompt.reply.send(true);
                    app.entries.push(ChatEntry::system("  ✓ Approved"));
                    app.scroll_to_bottom();
                }
            }
            Char('d') | Char('D') => {
                if let Some(prompt) = app.browse_approval.take() {
                    let _ = prompt.reply.send(false);
                    app.entries.push(ChatEntry::system("  ✗ Denied"));
                    app.scroll_to_bottom();
                }
            }
            KeyCode::Esc => {
                if let Some(prompt) = app.browse_approval.take() {
                    let _ = prompt.reply.send(false);
                    app.entries.push(ChatEntry::system("  ✗ Cancelled"));
                    app.scroll_to_bottom();
                }
            }
            _ => {} // ignore other keys while prompt is active
        }
        return Ok(());
    }

    // AskUser dialog takes priority after permission dialog
    if let Some(ref mut q) = app.pending_user_question {
        match key.code {
            Enter => {
                let answer: String = q.input.iter().collect();
                if let Some(pq) = app.pending_user_question.take() {
                    let _ = pq.reply.send(answer);
                }
            }
            Backspace => {
                if let Some(ref mut q) = app.pending_user_question
                    && q.cursor > 0
                {
                    q.cursor -= 1;
                    q.input.remove(q.cursor);
                }
            }
            Left => {
                if let Some(ref mut q) = app.pending_user_question
                    && q.cursor > 0
                {
                    q.cursor -= 1;
                }
            }
            Right => {
                if let Some(ref mut q) = app.pending_user_question
                    && q.cursor < q.input.len()
                {
                    q.cursor += 1;
                }
            }
            Esc => {
                // Cancel the question — send empty string
                if let Some(pq) = app.pending_user_question.take() {
                    let _ = pq.reply.send(String::new());
                }
            }
            Char(c) => {
                if let Some(ref mut q) = app.pending_user_question {
                    q.input.insert(q.cursor, c);
                    q.cursor += 1;
                }
            }
            _ => {}
        }
        return Ok(());
    }

    // Block input while loading (except Ctrl+C and Esc)
    if app.is_loading && key.code != Char('c') && key.code != Esc {
        return Ok(());
    }

    // ── Vim mode routing ───────────────────────────────────────────────────────
    if app.vim_enabled {
        if app.vim_normal {
            handle_vim_normal(key, app);
            return Ok(());
        }
        // Insert mode: Esc → normal mode
        if key.code == KeyCode::Esc {
            app.vim_enter_normal();
            return Ok(());
        }
    }

    match (key.code, key.modifiers) {
        (Char('c'), KeyModifiers::CONTROL) => {
            app.should_quit = true;
        }

        // Ctrl+R — toggle voice recording (only when voice mode is enabled)
        (Char('r'), KeyModifiers::CONTROL) if config.voice_enabled => {
            if app.voice_recording {
                // Stop recording gracefully via oneshot signal
                if let Some(stop_tx) = app.voice_stop_tx.take() {
                    let _ = stop_tx.send(());
                }
                app.voice_task = None;
                app.voice_recording = false;

                if let Some(tier) = app.pending_clone_tier.take() {
                    // Voice clone mode — save the recording as voice clone sample
                    app.entries
                        .push(ChatEntry::system("Saving voice clone…".to_string()));
                    app.scroll_to_bottom();
                    let tx2 = tx.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
                        match crate::voice::save_voice_clone(tier).await {
                            Ok(msg) => {
                                let _ = tx2.send(AppEvent::SystemMessage(msg));
                            }
                            Err(e) => {
                                let _ = tx2.send(AppEvent::SystemMessage(format!(
                                    "Voice clone failed: {e:#}"
                                )));
                            }
                        }
                    });
                } else {
                    // Normal mode — transcribe the recording
                    app.entries
                        .push(ChatEntry::system("Transcribing…".to_string()));
                    app.scroll_to_bottom();
                    let tx2 = tx.clone();
                    let api_url = config.voice_api_url.clone();
                    let api_key = crate::voice::voice_api_key();
                    tokio::spawn(async move {
                        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
                        match crate::voice::transcribe(api_url.as_deref(), api_key.as_deref()).await
                        {
                            Ok(text) if text.is_empty() => {
                                let _ = tx2.send(AppEvent::SystemMessage(
                                    "Transcription was empty.".into(),
                                ));
                            }
                            Ok(text) => {
                                if crate::voice::voice_routes_to_browse(&text) {
                                    let goal = crate::voice::strip_browse_prefix(&text);
                                    let _ = tx2.send(AppEvent::VoiceBrowse(goal));
                                } else {
                                    let _ = tx2.send(AppEvent::VoiceTranscription(text));
                                }
                            }
                            Err(e) => {
                                let _ =
                                    tx2.send(AppEvent::Error(format!("Transcription failed: {e}")));
                            }
                        }
                    });
                }
            } else {
                // Start recording
                match crate::voice::find_recorder() {
                    None => {
                        app.entries.push(ChatEntry::system(
                            "No recorder found. Install arecord (alsa-utils), sox, or ffmpeg."
                                .to_string(),
                        ));
                    }
                    Some(backend) => {
                        app.voice_recording = true;
                        app.entries.push(ChatEntry::system(
                            "Recording… press Ctrl+R again to stop.".to_string(),
                        ));
                        app.scroll_to_bottom();
                        let tx2 = tx.clone();
                        let (stop_tx, stop_rx) = oneshot::channel::<()>();
                        app.voice_stop_tx = Some(stop_tx);
                        let handle = tokio::spawn(async move {
                            match crate::voice::start_recording(&backend).await {
                                Ok(mut child) => {
                                    tokio::select! {
                                        _ = stop_rx => {
                                            // Graceful stop: send SIGINT so ffmpeg finalizes the WAV
                                            if let Some(pid) = child.id() {
                                                let _ = tokio::process::Command::new("kill")
                                                    .args(["-2", &pid.to_string()])
                                                    .status()
                                                    .await;
                                            }
                                            let _ = child.wait().await;
                                        }
                                        _ = child.wait() => {}
                                    }
                                }
                                Err(e) => {
                                    let _ =
                                        tx2.send(AppEvent::Error(format!("Recording failed: {e}")));
                                }
                            }
                        });
                        app.voice_task = Some(handle.abort_handle());
                    }
                }
            }
        }

        // Ctrl+S — stop TTS playback without cancelling generation
        (Char('s'), KeyModifiers::CONTROL) => {
            if let Some(stop_tx) = app.tts_stop_tx.take() {
                let _ = stop_tx.send(());
                app.entries
                    .push(ChatEntry::system("TTS stopped.".to_string()));
                app.scroll_to_bottom();
            }
        }

        // Escape — stop TTS if playing, or cancel API request if loading
        (Esc, _) if app.tts_stop_tx.is_some() && !app.is_loading => {
            if let Some(stop_tx) = app.tts_stop_tx.take() {
                let _ = stop_tx.send(());
            }
            app.entries
                .push(ChatEntry::system("TTS stopped.".to_string()));
            app.scroll_to_bottom();
        }
        (Esc, _) if app.is_loading => {
            // Stop any active TTS first
            if let Some(stop_tx) = app.tts_stop_tx.take() {
                let _ = stop_tx.send(());
            }
            if let Some(handle) = app.api_task.take() {
                handle.abort();
            }
            app.is_loading = false;
            app.turn_start = None; // cancelled — no completion message
            app.flush_streaming();
            app.entries
                .push(ChatEntry::system("Request cancelled.".to_string()));
            app.scroll_to_bottom();
        }

        // Shift+Enter inserts a newline in the input box (multi-line mode)
        (Enter, KeyModifiers::SHIFT) => {
            app.insert_newline();
        }

        (Enter, _) => {
            let raw = app.take_input();
            let input = raw.trim().to_string();
            if input.is_empty() {
                return Ok(());
            }

            // Stop any active TTS when the user sends a new message
            if let Some(stop_tx) = app.tts_stop_tx.take() {
                let _ = stop_tx.send(());
            }

            // Slash command dispatch (disabled when --disable-slash-commands)
            if input.starts_with('/') && !config.disable_slash_commands {
                return dispatch::run_slash_command(
                    input,
                    KeyCtx {
                        key,
                        app,
                        messages,
                        client,
                        tools,
                        config,
                        perm_state,
                        skills,
                        system_prompt,
                        tx,
                        todo_state,
                        session,
                        saved_count,
                        mcp_statuses,
                        turn_counter,
                        spawn_registry,
                    },
                )
                .await;
            }

            // Regular user message → send to Claude
            app.show_welcome = false;
            app.entries.push(ChatEntry::user(input.clone()));
            app.scroll_to_bottom();
            app.start_loading();

            // Build message content — text + optional image attachment
            let mut user_content: Vec<ContentBlock> = Vec::new();
            if let Some(image_path) = app.pending_image.take() {
                match attach_image(&image_path) {
                    Ok(image_block) => {
                        user_content.push(image_block);
                    }
                    Err(e) => {
                        app.entries
                            .push(ChatEntry::error(format!("Image attach failed: {e}")));
                    }
                }
            }
            // Prepend btw_note if set
            let final_text = if let Some(note) = app.btw_note.take() {
                format!("(btw: {note})\n\n{input}")
            } else {
                input
            };
            // UserPromptSubmit hooks — can inject additional context
            let final_text = if let Some(hook_cfg) = &config.hooks {
                if !config.disable_all_hooks {
                    if let Some(extra_ctx) = hooks::run_user_prompt_hooks(
                        hook_cfg,
                        &final_text,
                        &session.id,
                        &config.cwd,
                    )
                    .await
                    {
                        format!(
                            "{final_text}\n\n<additional_context>{extra_ctx}</additional_context>"
                        )
                    } else {
                        final_text
                    }
                } else {
                    final_text
                }
            } else {
                final_text
            };
            user_content.push(ContentBlock::Text {
                text: final_text.clone(),
            });

            messages.push(Message {
                role: Role::User,
                content: user_content,
            });

            // Set snapshot directory for this turn (file history checkpointing)
            *turn_counter += 1;
            config.file_snapshot_dir = Some(
                crate::config::Config::sessions_dir()
                    .join(&session.id)
                    .join("snapshots")
                    .join(format!("turn-{}", *turn_counter)),
            );

            // Background incremental re-index: pick up any files changed since last index.
            // Fire-and-forget — doesn't block the user's message from being sent.
            {
                let cwd = config.cwd.clone();
                tokio::spawn(async move {
                    let _ = tokio::task::spawn_blocking(move || {
                        if let Ok(db) = crate::rag::RagDb::open(&cwd) {
                            let _ = crate::rag::indexer::index_project(&db, &cwd, false);
                        }
                    })
                    .await;
                });
            }

            let c2 = client.clone();
            let tvec = tools.to_vec();
            let msgs = messages.clone();
            let mut cfg = config.clone();

            // Model routing: phase routing takes priority over complexity routing.
            // 1. Phase router (if enabled) — research/plan/edit/review → specific model
            // 2. Complexity router (if enabled) — low/medium/high/super-high → model tier
            // 3. Fallback: config.model unchanged
            if config.phase_router.enabled
                && !crate::api::is_ollama_model(&config.model)
                && !crate::api::is_openai_compat_model(&config.model)
            {
                let phase = crate::router::detect_phase(&final_text);
                if phase != crate::router::Phase::Default {
                    let routed_model = config.phase_router.model_for(phase).to_string();
                    if routed_model != config.model {
                        app.entries.push(ChatEntry::system(format!(
                            "[phase: {} → {}]",
                            phase,
                            crate::tui::app::pretty_model_name(&routed_model)
                        )));
                        cfg.model = routed_model;
                    }
                }
            } else if app.router.enabled
                && !crate::api::is_ollama_model(&config.model)
                && !crate::api::is_openai_compat_model(&config.model)
            {
                let complexity = crate::router::detect_complexity(&final_text);
                let routed_model = app.router.model_for(complexity).to_string();
                if routed_model != config.model {
                    app.entries.push(ChatEntry::system(format!(
                        "Router: {complexity} complexity → {}",
                        crate::tui::app::pretty_model_name(&routed_model)
                    )));
                    cfg.model = routed_model;
                }
            }

            let tx2 = tx.clone();
            // Inject brief mode instruction into system prompt if enabled
            let sp = if app.brief_mode {
                format!(
                    "{}\n\nIMPORTANT: The user has enabled brief mode. \
                     Keep all responses concise and to the point. \
                     Lead with the answer, skip preamble and filler.",
                    system_prompt
                )
            } else {
                system_prompt.clone()
            };
            let ps = perm_state.clone();
            let pm = app.plan_mode;

            let sid2 = session.id.clone();
            let handle = tokio::spawn(async move {
                run_api_task(ApiTask {
                    client: c2,
                    tools: tvec,
                    messages: msgs,
                    config: cfg,
                    perm_state: ps,
                    system_prompt: sp,
                    tx: tx2,
                    plan_mode: pm,
                    session_id: sid2,
                })
                .await;
            });
            app.api_task = Some(handle.abort_handle());
        }

        // '?' shows keybindings as an overlay when input is empty — otherwise insert normally
        (Char('?'), _) if !app.is_loading && app.input.is_empty() => {
            let last_assistant = app
                .entries
                .iter()
                .rev()
                .find(|e| matches!(e.kind, crate::tui::app::EntryKind::Assistant))
                .map(|e| e.text.as_str());
            let ctx = CommandContext {
                config,
                tokens_in: app.tokens_in,
                tokens_out: app.tokens_out,
                cache_read_tokens: app.cache_read_tokens,
                cache_write_tokens: app.cache_write_tokens,
                vim_mode: app.vim_enabled,
                skills,
                todo_state,
                last_assistant,
                session_id: &session.id,
                session_name: &session.meta.name,
                claudemd: &config.claudemd,
                mcp_statuses,
                brief_mode: app.brief_mode,
                btw_note: app.btw_note.as_deref(),
            };
            if let CommandAction::Message(text) = dispatch("/keybindings", &ctx) {
                app.overlay = Some(Overlay::new("keybindings", text));
            }
        }

        (Backspace, _) => app.backspace(),
        (Delete, _) => {
            app.cursor_right();
            app.backspace();
        }

        // ── Readline-style editing shortcuts ───────────────────────────────────
        (Char('a'), KeyModifiers::CONTROL) => app.cursor_home(),
        (Char('e'), KeyModifiers::CONTROL) => app.cursor_end(),
        (Char('w'), KeyModifiers::CONTROL) => app.delete_word_back(),
        (Char('k'), KeyModifiers::CONTROL) => app.delete_to_end(),
        // Alt+B / Alt+F word navigation
        (Char('b'), KeyModifiers::ALT) => app.word_back_readline(),
        (Char('f'), KeyModifiers::ALT) => app.word_forward_readline(),
        // Alt+D delete word forward
        (Char('d'), KeyModifiers::ALT) => app.delete_word_forward(),
        // Ctrl+Left / Ctrl+Right word navigation (some terminals)
        (Left, KeyModifiers::CONTROL) => app.word_back_readline(),
        (Right, KeyModifiers::CONTROL) => app.word_forward_readline(),

        // ── Tab: history autosuggestion, then slash-command/model completion ────
        (Tab, _) => {
            let raw: String = app.input.iter().collect();

            // If input is non-empty and not a slash command, try history suggestion first
            if !raw.is_empty() && !raw.starts_with('/') && app.history_suggestion().is_some() {
                app.accept_suggestion();
                return Ok(());
            }

            // "/model ollama:<prefix>" → complete from installed Ollama models
            if let Some(after) = raw.strip_prefix("/model ") {
                let query = after.trim();
                // Get installed models from Ollama
                let ollama_host = &config.ollama_host;
                let models = crate::api::list_ollama_models(ollama_host).await;
                // Strip "ollama:" prefix for display, filter by what's typed
                let typed_bare = query.strip_prefix("ollama:").unwrap_or(query);
                let matches: Vec<String> = models
                    .iter()
                    .filter(|m| {
                        let bare = crate::api::strip_ollama_prefix(m);
                        bare.contains(typed_bare)
                    })
                    .cloned()
                    .collect();
                match matches.len() {
                    0 => {}
                    1 => {
                        app.input = format!("/model {}", matches[0]).chars().collect();
                        app.cursor = app.input.len();
                    }
                    _ => {
                        let mut lines = vec!["Installed Ollama models:\n".to_string()];
                        let ids: Vec<String> = matches.clone();
                        for (i, m) in matches.iter().enumerate() {
                            lines.push(format!("  {}. {}", i + 1, m));
                        }
                        lines.push(String::new());
                        lines
                            .push("  ↑↓ select · Enter switch · 1-9 quick pick · Esc close".into());
                        app.overlay = Some(Overlay::with_items("models", lines.join("\n"), ids));
                    }
                }
            // "/cmd" with no space → complete slash command names + plugin:command
            } else if raw.starts_with('/') && !raw.contains(' ') {
                let prefix = raw.trim_start_matches('/');

                // Static slash commands
                let mut all_completions: Vec<String> = crate::commands::SLASH_COMMANDS
                    .iter()
                    .filter(|&&cmd| cmd.starts_with(prefix))
                    .map(|&s| s.to_string())
                    .collect();

                // Dynamic plugin:command entries from connected MCP tools.
                // MCP tool names look like "mcp__context_mode__ctx_doctor" →
                // we surface them as "context-mode:ctx-doctor".
                for tool in tools.iter() {
                    let name = tool.name();
                    if let Some(rest) = name.strip_prefix("mcp__")
                        && let Some(sep) = rest.find("__")
                    {
                        let server = &rest[..sep];
                        let cmd = &rest[sep + 2..];
                        // Convert underscores back to hyphens for the slash form
                        let entry =
                            format!("{}:{}", server.replace('_', "-"), cmd.replace('_', "-"));
                        if entry.starts_with(prefix) {
                            all_completions.push(entry);
                        }
                    }
                }

                match all_completions.len() {
                    0 => {}
                    1 => {
                        app.input = format!("/{}", all_completions[0]).chars().collect();
                        app.cursor = app.input.len();
                    }
                    _ => {
                        all_completions.sort();
                        let list = all_completions
                            .iter()
                            .map(|s| format!("/{s}"))
                            .collect::<Vec<_>>()
                            .join("   ");
                        app.overlay = Some(Overlay::new("tab", format!("Completions:\n\n{list}")));
                    }
                }
            }
        }

        (Left, _) => app.cursor_left(),
        (Right, _) => app.cursor_right(),
        // Ctrl+Home → jump to very top of chat; Ctrl+End → jump to bottom
        (Home, KeyModifiers::CONTROL) => {
            app.follow_bottom = false;
            app.scroll = 0;
        }
        (End, KeyModifiers::CONTROL) => {
            app.follow_bottom = true;
        }
        (Home, _) => app.cursor_home(),
        (End, _) => app.cursor_end(),
        (Up, _) => {
            // In multi-line input, Up on non-first line moves cursor up a line;
            // otherwise fall through to input history.
            if app.input_line_count() > 1 {
                let before: String = app.input[..app.cursor].iter().collect();
                let line_idx = before.split('\n').count().saturating_sub(1);
                if line_idx > 0 {
                    app.move_cursor_up_one_line();
                } else {
                    app.history_up();
                }
            } else {
                app.history_up();
            }
        }
        (Down, _) => {
            if app.input_line_count() > 1 {
                let before: String = app.input[..app.cursor].iter().collect();
                let line_idx = before.split('\n').count().saturating_sub(1);
                let total_lines = app.input_line_count();
                if line_idx + 1 < total_lines {
                    app.move_cursor_down_one_line();
                } else {
                    app.history_down();
                }
            } else {
                app.history_down();
            }
        }
        (PageUp, _) => app.scroll_up(),
        (PageDown, _) => {
            app.follow_bottom = true;
        }
        (Char('u'), KeyModifiers::CONTROL) => app.clear_line(),
        (Char(c), _) => app.insert_char(c),
        _ => {}
    }
    Ok(())
}

// ── Vim normal-mode key handler ───────────────────────────────────────────────
