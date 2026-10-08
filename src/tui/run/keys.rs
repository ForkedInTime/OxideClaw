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
    pub(super) mcp_failed: &'a [String],
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
        mcp_failed,
        spawn_registry,
    } = ctx;
    use KeyCode::*;

    // Ahead of every overlay and dialog: they swallowed it (AskUser typed a
    // 'c'), so Ctrl+C did not quit while one was open. The quit path denies
    // a pending approval and cancels a pending question.
    if key.code == Char('c') && key.modifiers == KeyModifiers::CONTROL {
        app.should_quit = true;
        return Ok(());
    }

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
                app.pending_rewind = None;
            }
            // A digit picks row N exactly as Enter picks the highlighted
            // row; the separate digit arm sent picker labels to /resume.
            KeyCode::Enter | KeyCode::Char('1'..='9') if is_interactive => {
                let title = app
                    .overlay
                    .as_ref()
                    .map(|o| o.title.clone())
                    .unwrap_or_default();
                let selected_index = match key.code {
                    KeyCode::Char(c) => (c as usize) - ('1' as usize),
                    _ => app.overlay.as_ref().map(|o| o.selected).unwrap_or(0),
                };
                let selected_val = app
                    .overlay
                    .as_ref()
                    .and_then(|o| o.selectable_ids.get(selected_index).cloned());
                app.overlay = None;
                if title == "rewind" {
                    timeline::pick_rewind(
                        app,
                        messages,
                        session,
                        saved_count,
                        config,
                        selected_index,
                    )
                    .await;
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
                    } else if title == "sessions" {
                        app.pending_resume = Some(val);
                    }
                }
            }
            KeyCode::Enter => {
                app.overlay = None;
            }
            // Only session ids are deletable; elsewhere the id is a model,
            // command or voice path and "Deleted session" was a lie.
            KeyCode::Char('d') | KeyCode::Delete
                if is_interactive
                    && app.overlay.as_ref().is_some_and(|o| o.title == "sessions") =>
            {
                if let Some(id) = selected_session_to_delete(app) {
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
    if let Some(perm) = &mut app.pending_permission {
        // y/a only once render has shown every row of the command; until
        // then the arrows scroll the dialog and n/Esc still deny.
        let shown = perm.fully_shown;
        // Not offered (or drawn) for a question an "always" would overstate.
        let always = crate::permissions::offers_always_allow(&perm.tool_name);
        match key.code {
            KeyCode::Up => perm.scroll = perm.scroll.saturating_sub(1),
            KeyCode::Down => perm.scroll = perm.scroll.saturating_add(1),
            KeyCode::PageUp => perm.scroll = perm.scroll.saturating_sub(10),
            KeyCode::PageDown => perm.scroll = perm.scroll.saturating_add(10),
            Char('y') | Char('Y') if shown => {
                if let Some(p) = app.pending_permission.take() {
                    let _ = p.reply.send(PermissionDecision::Allow);
                }
            }
            Char('a') | Char('A') if shown && always => {
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
            Char('a') | Char('A') => answer_browse_approval(app, true, "  ✓ Approved"),
            Char('d') | Char('D') => answer_browse_approval(app, false, "  ✗ Denied"),
            // Esc cancels everywhere else, so it stops the run too: a
            // denied action alone left the agent driving the browser.
            KeyCode::Esc => {
                answer_browse_approval(app, false, "  ✗ Denied — stopping /browse");
                if let Some(cancel) = app.browse_cancel.take() {
                    cancel.store(true, std::sync::atomic::Ordering::SeqCst);
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

    // Block input while loading, except Esc (Ctrl+C was handled above).
    // Matching the bare code let plain 'c' through to the input.
    if app.is_loading && key.code != Esc {
        return Ok(());
    }

    // ── Vim mode routing ───────────────────────────────────────────────────────
    if vim_routes_key(app, &key) {
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
                    let task = tokio::spawn(async move {
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
                    app.voice_transcribe_task = Some(task.abort_handle());
                } else {
                    // Normal mode — transcribe the recording
                    app.entries
                        .push(ChatEntry::system("Transcribing…".to_string()));
                    app.scroll_to_bottom();
                    let tx2 = tx.clone();
                    let api_url = config.voice_api_url.clone();
                    let api_key = crate::voice::voice_api_key();
                    let task = tokio::spawn(async move {
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
                    app.voice_transcribe_task = Some(task.abort_handle());
                }
            } else if app.transcription_pending() {
                // start_recording deletes and rewrites the WAV it still reads.
                app.entries.push(ChatEntry::system(
                    "Still transcribing the last recording; press Ctrl+R again in a moment."
                        .to_string(),
                ));
                app.scroll_to_bottom();
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
                        // This recording replaces the clone sample instead of
                        // being transcribed; say so, since /voice clone may
                        // have been typed long ago.
                        let msg = match app.pending_clone_tier {
                            Some(tier) => format!(
                                "Recording voice-clone sample ({} tier)… press Ctrl+R again to stop. \
                                 (Esc before recording cancels clone mode.)",
                                tier.label()
                            ),
                            None => "Recording… press Ctrl+R again to stop.".to_string(),
                        };
                        app.entries.push(ChatEntry::system(msg));
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
                                    drop(stop_rx);
                                    let _ = tx2.send(AppEvent::RecordingFailed(format!(
                                        "Recording failed: {e}"
                                    )));
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
            if let Some(stop_tx) = app.tts_stop_tx.take().filter(|t| !t.is_closed()) {
                let _ = stop_tx.send(());
                app.entries
                    .push(ChatEntry::system("TTS stopped.".to_string()));
                app.scroll_to_bottom();
            }
        }

        // Escape — stop TTS if playing, or cancel API request if loading
        (Esc, _) if tts_playing(app) && !app.is_loading => {
            if let Some(stop_tx) = app.tts_stop_tx.take() {
                let _ = stop_tx.send(());
            }
            app.entries
                .push(ChatEntry::system("TTS stopped.".to_string()));
            app.scroll_to_bottom();
        }
        (Esc, _) if esc_disarms_clone(app) => {
            app.pending_clone_tier = None;
            app.entries.push(ChatEntry::system(
                "Voice clone cancelled. Ctrl+R records dictation again.".to_string(),
            ));
            app.scroll_to_bottom();
        }
        (Esc, _) if app.prompt_hooks.is_some() => {
            // Aborting drops the hook runs, which kills their process groups.
            // The prompt is still in the input box.
            if let Some(p) = app.prompt_hooks.take() {
                p.task.abort();
            }
            app.is_loading = false;
            app.turn_start = None;
            app.entries.push(ChatEntry::system(
                "Prompt not sent — userPromptSubmit hooks cancelled.".to_string(),
            ));
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
            if let Some(handle) = app.side_task.take() {
                handle.abort();
            }
            if let Some(history) = app.turn_history.take() {
                adopt_turn_history(
                    &history,
                    messages,
                    saved_count,
                    session,
                    !config.no_session_persistence,
                )
                .await;
                snapshot_after_turn(session, config, tools, app).await;
            }
            if let Some(cancel) = app.browse_cancel.take() {
                cancel.store(true, std::sync::atomic::Ordering::SeqCst);
            }
            app.is_loading = false;
            app.turn_start = None; // cancelled — no completion message
            app.flush_streaming();
            app.entries
                .push(ChatEntry::system("Request cancelled.".to_string()));
            app.scroll_to_bottom();
        }

        (code, mods) if is_newline_key(code, mods) => app.insert_newline(),

        (Enter, _) => {
            let raw = app.take_input();
            return submit_line(
                raw,
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
                    mcp_failed,
                    spawn_registry,
                },
            )
            .await;
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
                context_window: app.context_window,
                tokens_out: app.tokens_out,
                cache_read_tokens: app.cache_read_tokens,
                cost_summary: app.cost_tracker.summary(),
                cost_recorded: !app.cost_tracker.by_model.is_empty(),
                cache_write_tokens: app.cache_write_tokens,
                vim_mode: app.vim_enabled,
                skills,
                todo_state,
                last_assistant,
                session_id: &session.id,
                session_name: &session.meta.name,
                claudemd: &config.claudemd,
                mcp_statuses,
                mcp_failed,
                brief_mode: app.brief_mode,
                btw_note: app.btw_note.as_deref(),
            };
            if let CommandAction::Message(text) = dispatch("/keybindings", &ctx) {
                app.overlay = Some(Overlay::new("keybindings", text));
            }
        }

        (Backspace, _) => app.backspace(),
        (Delete, _) if app.cursor < app.input.len() => {
            // Forward delete; at the end of input there is nothing to delete
            // (cursor_right + backspace removed the previous char there).
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

/// Send a submitted input line: a slash command, or a message for a turn.
/// Also the replay once a prompt's userPromptSubmit hooks have finished.
async fn submit_line(raw: String, ctx: KeyCtx<'_>) -> Result<()> {
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
        mcp_failed,
        spawn_registry,
    } = ctx;
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
                mcp_failed,
                spawn_registry,
            },
        )
        .await;
    }

    // Regular user message → send to Claude, unless the budget is spent.
    if budget_blocks(app, &raw) {
        return Ok(());
    }
    // btw note and image are only taken once the prompt is allowed, so
    // a hook that stops the turn leaves them in place for the retry.
    let final_text = match app.btw_note.as_deref() {
        Some(note) => format!("(btw: {note})\n\n{input}"),
        None => input.clone(),
    };
    let Some((final_text, hook_note)) = user_prompt_gate(
        app,
        config,
        &session.id,
        &raw,
        &final_text,
        final_text.clone(),
        None,
    )
    .await
    else {
        return Ok(());
    };
    app.show_welcome = false;
    app.entries.push(ChatEntry::user(input));
    if let Some(msg) = hook_note {
        app.entries.push(ChatEntry::system(msg));
    }
    app.scroll_to_bottom();
    app.start_loading();
    app.btw_note = None;
    begin_agent_turn(session, config, tools).await;

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
    user_content.push(ContentBlock::Text {
        text: final_text.clone(),
    });

    push_prompt_turn(messages, user_content, session).await;

    // Background incremental re-index: pick up any files changed since last index.
    // Fire-and-forget — doesn't block the user's message from being sent.
    // Off where startup said so (outside a git repo, $HOME, /).
    if let Ok(target) = crate::rag::IndexTarget::for_cwd(&config.cwd, true) {
        tokio::spawn(async move {
            let _ = tokio::task::spawn_blocking(move || {
                if let Ok(db) = target.open() {
                    let _ = target.index(&db, false);
                }
            })
            .await;
        });
    }

    let tvec = tools.to_vec();
    let msgs = messages.clone();
    let mut cfg = config.clone();

    // Model routing: phase routing takes priority over complexity routing.
    // 1. Phase router (if enabled) — research/plan/edit/review → specific model
    // 2. Complexity router (if enabled) — low/medium/high/super-high → model
    //    tier on any backend, picked inside the task (the classifier
    //    may take a model call) and escalated there on failure
    // 3. Fallback: config.model unchanged
    let mut router = None;
    if config.phase_router.enabled
        && !crate::api::is_ollama_model(&config.model)
        && !crate::api::is_openai_compat_model(&config.model)
    {
        // The phase router never sends `Routed`: a model left from
        // an earlier complexity-routed turn would stay on screen.
        app.routed_model = None;
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
    } else if app.router.enabled {
        router = Some(app.router.clone());
    }
    let c2 = match routed_client(config, client, &cfg.model) {
        Ok(c) => c,
        Err(e) => {
            app.entries.push(ChatEntry::error(format!(
                "Router: cannot use {}: {e}\n\nThis turn uses {}.",
                cfg.model, config.model
            )));
            cfg.model = config.model.clone();
            client.clone()
        }
    };

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
    let budget_left = app.cost_tracker.remaining();

    let sid2 = session.id.clone();
    let turn_history = TurnHistory::default();
    app.turn_history = Some(turn_history.clone());
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
            skill_no_shell: false,
            budget_remaining_usd: budget_left,
            session_id: sid2,
            history: turn_history,
            router,
            // A routed turn summarises through its route; with
            // phase routing on, the complexity router never ran.
            compact_router: None,
        })
        .await;
    });
    app.api_task = Some(handle.abort_handle());
    Ok(())
}

/// The session `d`/Delete would remove. Only the sessions picker holds
/// session ids; in the model, help, voice and undo pickers the selected id is
/// a model name, voice path or label that must not reach Session::delete.
fn selected_session_to_delete(app: &App) -> Option<String> {
    app.overlay
        .as_ref()
        .filter(|o| o.title == "sessions")
        .and_then(|o| o.selectable_ids.get(o.selected).cloned())
}

/// Send the user's answer to the pending browse approval. The gate stops
/// listening after its 60 s window (denying the action) or once voice answered,
/// so a failed send means this key changed nothing and must not say otherwise.
fn answer_browse_approval(app: &mut App, approved: bool, label: &str) {
    let Some(prompt) = app.browse_approval.take() else {
        return;
    };
    if prompt.reply.send(approved).is_ok() {
        app.entries.push(ChatEntry::system(label));
    } else {
        app.entries.push(ChatEntry::system(
            "  ⚠ Approval prompt expired (timed out or answered by voice) — your key was not applied",
        ));
    }
    app.scroll_to_bottom();
}

// ── Vim normal-mode key handler ───────────────────────────────────────────────

/// UserPromptSubmit hooks for a prompt about to start a turn: a typed
/// message, or a slash command, skill or plugin command that sends one.
/// `hook_text` is what the hook sees (what the user typed), `prompt` what
/// goes to the model. The hooks run in a task, since each may take a
/// minute and the event loop would freeze meanwhile: the first call starts
/// them and returns None, and `finish_prompt_hooks` submits `raw` (or
/// `voice_goal` as a voice /browse) again, when this returns their result.
/// Exit 2 or `continue: false` stops it: `raw` goes back into the input box
/// and None is returned, so run it before anything of the turn is shown.
/// Otherwise returns `prompt` with the hooks' context and the
/// systemMessage to show.
pub(super) async fn user_prompt_gate(
    app: &mut App,
    config: &Config,
    session_id: &str,
    raw: &str,
    hook_text: &str,
    prompt: String,
    voice_goal: Option<&str>,
) -> Option<(String, Option<String>)> {
    let hook_cfg = match &config.hooks {
        Some(h) if !config.disable_all_hooks && !h.user_prompt_submit.is_empty() => h,
        _ => return Some((prompt, None)),
    };
    let r = match app.prompt_hook_result.take() {
        Some((text, r)) if text == hook_text => r,
        _ => {
            let (hooks, text, sid, cwd) = (
                hook_cfg.clone(),
                hook_text.to_string(),
                session_id.to_string(),
                config.cwd.clone(),
            );
            let task = tokio::spawn(async move {
                hooks::run_user_prompt_hooks(&hooks, &text, &sid, &cwd).await
            });
            app.prompt_hooks = Some(crate::tui::app::PendingPromptHooks {
                task,
                hook_text: hook_text.to_string(),
                raw: raw.to_string(),
                voice_goal: voice_goal.map(str::to_string),
            });
            // Shown while it waits, and left there by Esc to edit.
            if app.input.is_empty() {
                app.input = raw.chars().collect();
                app.cursor = app.input.len();
            }
            app.start_loading();
            if !app.spinner_verb.is_empty() {
                app.spinner_verb = "Running userPromptSubmit hooks".to_string();
            }
            return None;
        }
    };
    if !r.should_continue {
        app.input = raw.chars().collect();
        app.cursor = app.input.len();
        app.entries.push(ChatEntry::error(format!(
            "Prompt not sent — blocked by a userPromptSubmit hook: {}",
            r.stop_reason.unwrap_or_default()
        )));
        app.scroll_to_bottom();
        return None;
    }
    let prompt = match r.additional_context {
        Some(extra) => format!("{prompt}\n\n<additional_context>{extra}</additional_context>"),
        None => prompt,
    };
    Some((prompt, r.system_message))
}

/// The prompt's userPromptSubmit hooks finished (or failed): submit it
/// again, and this time `user_prompt_gate` takes their result. Waits for
/// the task, so the event loop calls it once the task is finished.
pub(super) async fn finish_prompt_hooks(
    pending: crate::tui::app::PendingPromptHooks,
    ctx: KeyCtx<'_>,
) -> Result<()> {
    let crate::tui::app::PendingPromptHooks {
        task,
        hook_text,
        raw,
        voice_goal,
    } = pending;
    // A hook that cannot be evaluated already fails closed inside the task;
    // a task that died must not let the prompt through either.
    let result = task.await.unwrap_or_else(|e| hooks::HookResult {
        should_continue: false,
        stop_reason: Some(format!("the hook task failed: {e}")),
        ..Default::default()
    });
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
        mcp_failed,
        spawn_registry,
    } = ctx;
    app.is_loading = false;
    app.turn_start = None;
    if app.input.iter().copied().eq(raw.chars()) {
        app.input.clear();
        app.cursor = 0;
    }
    app.prompt_hook_result = Some((hook_text, result));
    if let Some(goal) = voice_goal {
        // Its handler in the event loop runs the gate again.
        let _ = tx.send(AppEvent::VoiceBrowse(goal));
        return Ok(());
    }
    let r = submit_line(
        raw,
        KeyCtx {
            key,
            app: &mut *app,
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
            mcp_failed,
            spawn_registry,
        },
    )
    .await;
    // Unused when the replay stopped before the gate (the budget ran out
    // meanwhile); a later send of the same text must run the hooks again.
    app.prompt_hook_result = None;
    r
}

/// Refuse a model call once /budget is spent, putting `unsent` back in the
/// input. The Usage-event abort only fires after the first call is billed,
/// so without this every turn past the cap still cost one full request.
pub(super) fn budget_blocks(app: &mut App, unsent: &str) -> bool {
    if !app.cost_tracker.over_budget() {
        return false;
    }
    app.input = unsent.chars().collect();
    app.cursor = app.input.len();
    app.entries.push(ChatEntry::system(format!(
        "Budget of ${:.2} reached — not sending. Use /budget to raise or clear it.",
        app.cost_tracker.budget_usd.unwrap_or_default()
    )));
    app.scroll_to_bottom();
    true
}

/// Whether Esc disarms a pending /voice clone. Not while a turn runs: that
/// Esc must cancel the turn first (clone mode stays armed for the next Esc).
fn esc_disarms_clone(app: &App) -> bool {
    app.pending_clone_tier.is_some() && !app.voice_recording && !app.is_loading
}

/// The sender outlives playback (nothing clears it when `speak` returns),
/// but its receiver is dropped then, so a closed sender means no TTS.
fn tts_playing(app: &App) -> bool {
    app.tts_stop_tx.as_ref().is_some_and(|t| !t.is_closed())
}

/// Whether vim mode gets this key before the main match. Esc while a turn or
/// TTS is running and Ctrl+S must reach their cancel/stop arms: vim would eat
/// them as a mode switch, and while loading every other key is blocked, so
/// Ctrl+C (quit) was the only way out.
fn vim_routes_key(app: &App, key: &crossterm::event::KeyEvent) -> bool {
    if !app.vim_enabled {
        return false;
    }
    let clone_armed = app.pending_clone_tier.is_some() && !app.voice_recording;
    let cancels_work =
        key.code == KeyCode::Esc && (app.is_loading || tts_playing(app) || clone_armed);
    let stops_tts = key.code == KeyCode::Char('s') && key.modifiers == KeyModifiers::CONTROL;
    !(cancels_work || stops_tts)
}

/// Shift+Enter is only distinguishable from Enter where the terminal took
/// the keyboard enhancement flags (kitty, foot, WezTerm, Ghostty); elsewhere
/// and inside tmux it arrives as a plain Enter and would submit. Alt+Enter
/// (ESC CR) and Ctrl+J (LF, which raw mode reports as Ctrl+J) reach us on
/// every terminal.
fn is_newline_key(code: KeyCode, mods: KeyModifiers) -> bool {
    match code {
        KeyCode::Enter => mods.intersects(KeyModifiers::SHIFT | KeyModifiers::ALT),
        KeyCode::Char('j') => mods == KeyModifiers::CONTROL,
        _ => false,
    }
}

#[cfg(test)]
mod newline_key_tests {
    use super::*;

    /// The events crossterm's legacy parser yields in raw mode: CR is Enter,
    /// ESC CR is Alt+Enter, LF is Ctrl+J; kitty's CSI 13;2u is Shift+Enter.
    /// Only the last one matched before, so on most terminals no key could
    /// insert a newline and Ctrl+J typed a literal 'j'.
    #[test]
    fn alt_enter_ctrl_j_and_shift_enter_insert_a_newline() {
        assert!(is_newline_key(KeyCode::Enter, KeyModifiers::ALT));
        assert!(is_newline_key(KeyCode::Char('j'), KeyModifiers::CONTROL));
        assert!(is_newline_key(KeyCode::Enter, KeyModifiers::SHIFT));
        assert!(is_newline_key(
            KeyCode::Enter,
            KeyModifiers::SHIFT | KeyModifiers::ALT
        ));

        assert!(!is_newline_key(KeyCode::Enter, KeyModifiers::NONE));
        assert!(!is_newline_key(KeyCode::Char('j'), KeyModifiers::NONE));
        assert!(!is_newline_key(
            KeyCode::Char('j'),
            KeyModifiers::CONTROL | KeyModifiers::ALT
        ));
    }
}

#[cfg(test)]
mod vim_routing_tests {
    use super::*;
    use crossterm::event::KeyEvent;

    fn vim_app(normal: bool) -> App {
        let mut app = App::new("claude-sonnet-4-6", std::path::Path::new("/tmp"));
        app.vim_enabled = true;
        app.vim_normal = normal;
        app
    }

    #[test]
    fn esc_reaches_cancel_while_loading_in_both_vim_modes() {
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        for normal in [true, false] {
            let mut app = vim_app(normal);
            assert!(vim_routes_key(&app, &esc), "idle Esc is a vim mode switch");
            app.start_loading();
            assert!(!vim_routes_key(&app, &esc), "Esc could not cancel the turn");
        }
    }

    /// Vim would eat the Esc that disarms /voice clone, leaving the next
    /// dictation to overwrite the clone sample.
    #[test]
    fn esc_reaches_clone_cancel_in_vim_mode() {
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        let mut app = vim_app(true);
        app.pending_clone_tier = Some(crate::voice::CloneTier::Quick);
        assert!(!vim_routes_key(&app, &esc));
        app.voice_recording = true;
        assert!(vim_routes_key(&app, &esc));
    }

    /// The clone-disarm arm came before the cancel arm, so the first Esc
    /// during a turn only disarmed /voice clone and the turn kept running.
    #[test]
    fn esc_cancels_a_running_turn_before_disarming_clone() {
        let mut app = vim_app(false);
        app.pending_clone_tier = Some(crate::voice::CloneTier::Quick);
        assert!(esc_disarms_clone(&app));
        app.start_loading();
        assert!(!esc_disarms_clone(&app));
    }

    #[test]
    fn esc_and_ctrl_s_stop_tts_in_vim_mode() {
        let mut app = vim_app(true);
        let (stop_tx, _stop_rx) = tokio::sync::oneshot::channel::<()>();
        app.tts_stop_tx = Some(stop_tx);
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        let ctrl_s = KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL);
        assert!(!vim_routes_key(&app, &esc));
        assert!(!vim_routes_key(&app, &ctrl_s));
        assert!(vim_routes_key(
            &app,
            &KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE)
        ));
    }

    /// After a reply or `/voice test` finished speaking, the next Esc
    /// printed "TTS stopped." instead of entering vim normal mode.
    #[test]
    fn finished_tts_does_not_take_esc() {
        let mut app = vim_app(true);
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        app.tts_stop_tx = Some(stop_tx);
        drop(stop_rx);
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        assert!(!tts_playing(&app));
        assert!(vim_routes_key(&app, &esc));
    }
}

#[cfg(test)]
mod browse_approval_key_tests {
    use super::*;

    fn prompt() -> (
        crate::browser::approval_gate::ApprovalPrompt,
        tokio::sync::oneshot::Receiver<bool>,
    ) {
        let (reply, rx) = tokio::sync::oneshot::channel();
        let prompt = crate::browser::approval_gate::ApprovalPrompt {
            id: 1,
            step: 1,
            tool_name: "browser_click".into(),
            target_text: "Buy".into(),
            url: "https://example.com".into(),
            reason: "submit".into(),
            reply,
        };
        (prompt, rx)
    }

    fn last_entry(app: &App) -> String {
        app.entries.last().unwrap().text.clone()
    }

    #[test]
    fn approving_a_live_prompt_reports_approved() {
        let mut app = App::new("claude-sonnet-4-6", std::path::Path::new("/tmp"));
        let (p, mut rx) = prompt();
        app.browse_approval = Some(p);
        answer_browse_approval(&mut app, true, "  ✓ Approved");
        assert!(app.browse_approval.is_none());
        assert!(rx.try_recv().unwrap());
        assert!(last_entry(&app).contains("Approved"));
    }

    /// The gate timed out and dropped its receiver: the action was already
    /// denied, so "Approved" would be a lie.
    #[test]
    fn approving_an_expired_prompt_does_not_claim_approval() {
        let mut app = App::new("claude-sonnet-4-6", std::path::Path::new("/tmp"));
        let (p, rx) = prompt();
        drop(rx);
        app.browse_approval = Some(p);
        answer_browse_approval(&mut app, true, "  ✓ Approved");
        assert!(app.browse_approval.is_none());
        let last = last_entry(&app);
        assert!(!last.contains("✓ Approved"), "{last}");
        assert!(last.contains("expired"), "{last}");
    }

    /// Esc said "Cancelled" but only denied the one action; the run went on.
    #[tokio::test]
    async fn esc_denies_the_action_and_stops_the_browse() {
        let mut app = App::new("claude-sonnet-4-6", std::path::Path::new("/tmp"));
        let (p, mut rx) = prompt();
        app.browse_approval = Some(p);
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        app.browse_cancel = Some(cancel.clone());
        ctrl_c_tests::press(
            &mut app,
            crossterm::event::KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
        )
        .await;
        assert!(!rx.try_recv().unwrap(), "action approved");
        assert!(cancel.load(std::sync::atomic::Ordering::SeqCst), "run kept going");
        assert!(last_entry(&app).contains("stopping /browse"));
    }
}

#[cfg(test)]
mod overlay_delete_tests {
    use super::*;

    fn app_with_picker(title: &str) -> App {
        let mut app = App::new("claude-sonnet-4-6", std::path::Path::new("/tmp"));
        app.overlay = Some(Overlay::with_items(
            title,
            "pick one",
            vec!["claude-opus-4-6".into()],
        ));
        app
    }

    #[test]
    fn delete_key_targets_only_the_sessions_picker() {
        for title in ["models", "help", "help-commands", "voices", "rewind"] {
            assert_eq!(
                selected_session_to_delete(&app_with_picker(title)),
                None,
                "{title}"
            );
        }
        assert_eq!(
            selected_session_to_delete(&app_with_picker("sessions")).as_deref(),
            Some("claude-opus-4-6")
        );
    }
}

#[cfg(test)]
mod overlay_key_tests {
    use super::*;
    use crossterm::event::KeyEvent;

    /// Press `code` with `overlay` open; returns the app afterwards.
    async fn press(
        overlay: Overlay,
        code: KeyCode,
        setup: impl FnOnce(&mut App),
    ) -> (App, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let mut app = App::new("claude-sonnet-4-6", dir.path());
        app.overlay = Some(overlay);
        setup(&mut app);
        let mut messages = Vec::new();
        let mut client =
            ApiBackend::Anthropic(crate::api::ClaudeClient::new("sk-ant-test").unwrap());
        let mut config = Config {
            cwd: dir.path().to_path_buf(),
            ..Config::default()
        };
        let perm_state = PermissionState::new(false, &[], &[]);
        let skills = std::collections::HashMap::new();
        let mut system_prompt = String::new();
        let (tx, _rx) = mpsc::unbounded_channel();
        let todo_state = TodoState::default();
        let mut session = Session::at_path("current", dir.path().join("current.jsonl"));
        let spawn_registry = crate::spawn::new_registry();
        handle_key(KeyCtx {
            key: KeyEvent::new(code, KeyModifiers::NONE),
            app: &mut app,
            messages: &mut messages,
            client: &mut client,
            tools: &[],
            config: &mut config,
            perm_state: &perm_state,
            skills: &skills,
            system_prompt: &mut system_prompt,
            tx: &tx,
            todo_state: &todo_state,
            session: &mut session,
            saved_count: &mut 0,
            mcp_statuses: &[],
            mcp_failed: &[],
            spawn_registry: &spawn_registry,
        })
        .await
        .unwrap();
        (app, dir)
    }

    /// `1` in the /undo picker sent the row label to /resume, which then
    /// failed with "Could not resume session". The /rewind picker took over.
    #[tokio::test]
    async fn digit_in_rewind_picker_undoes_instead_of_resuming() {
        let labels = vec!["turn 1  ·  hi  ← current".to_string(), "start".to_string()];
        let (app, _dir) = press(
            Overlay::with_items("rewind", "x", labels),
            KeyCode::Char('2'),
            |app| app.pending_rewind = Some((vec![0, 1], Vec::new())),
        )
        .await;
        assert_eq!(app.pending_resume, None);
        assert!(app.pending_rewind.is_none());
        // The conversation is empty, so the undo itself has nothing to take
        // off, but it was attempted.
        let last = app
            .entries
            .last()
            .map(|e| e.text.clone())
            .unwrap_or_default();
        assert!(last.starts_with("[undo]"), "{last}");
    }

    /// `d` in the model picker queued a "session delete" of a model name.
    #[tokio::test]
    async fn d_only_deletes_in_the_session_picker() {
        let ids = vec!["claude-opus-4-7".to_string()];
        let (app, _dir) = press(
            Overlay::with_items("models", "x", ids.clone()),
            KeyCode::Char('d'),
            |_| {},
        )
        .await;
        assert_eq!(app.pending_delete, None);
        assert!(app.overlay.is_some(), "picker stays open");

        let (app, _dir) = press(
            Overlay::with_items("sessions", "x", vec!["old-session".into()]),
            KeyCode::Char('d'),
            |_| {},
        )
        .await;
        assert_eq!(app.pending_delete.as_deref(), Some("old-session"));
    }
}

#[cfg(test)]
mod ctrl_c_tests {
    use super::*;
    use crate::tui::app::PendingUserQuestion;
    use crossterm::event::KeyEvent;

    pub(super) async fn press(app: &mut App, key: KeyEvent) {
        let dir = tempfile::tempdir().unwrap();
        let mut messages = Vec::new();
        let mut client =
            ApiBackend::Anthropic(crate::api::ClaudeClient::new("sk-ant-test").unwrap());
        let mut config = Config {
            cwd: dir.path().to_path_buf(),
            ..Config::default()
        };
        let perm_state = PermissionState::new(false, &[], &[]);
        let skills = std::collections::HashMap::new();
        let mut system_prompt = String::new();
        let (tx, _rx) = mpsc::unbounded_channel();
        let todo_state = TodoState::default();
        let mut session = Session::at_path("current", dir.path().join("current.jsonl"));
        let spawn_registry = crate::spawn::new_registry();
        handle_key(KeyCtx {
            key,
            app,
            messages: &mut messages,
            client: &mut client,
            tools: &[],
            config: &mut config,
            perm_state: &perm_state,
            skills: &skills,
            system_prompt: &mut system_prompt,
            tx: &tx,
            todo_state: &todo_state,
            session: &mut session,
            saved_count: &mut 0,
            mcp_statuses: &[],
            spawn_registry: &spawn_registry,
        })
        .await
        .unwrap();
    }

    fn ctrl_c() -> KeyEvent {
        KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)
    }

    /// Ctrl+C in the AskUser dialog typed a 'c' into the answer and the app
    /// kept running.
    #[tokio::test]
    async fn ctrl_c_quits_from_the_ask_user_dialog() {
        let mut app = App::new("claude-sonnet-4-6", std::path::Path::new("/tmp"));
        let (reply, _rx) = oneshot::channel();
        app.pending_user_question = Some(PendingUserQuestion {
            question: "which?".into(),
            reply,
            input: Vec::new(),
            cursor: 0,
        });
        press(&mut app, ctrl_c()).await;
        assert!(app.should_quit);
        let q = app.pending_user_question.as_ref().unwrap();
        assert!(q.input.is_empty(), "typed {:?}", q.input);
    }

    /// The overlay and browse-approval branches ignored Ctrl+C.
    #[tokio::test]
    async fn ctrl_c_quits_with_an_overlay_open() {
        let mut app = App::new("claude-sonnet-4-6", std::path::Path::new("/tmp"));
        app.overlay = Some(Overlay::with_items("models", "x", vec!["m".into()]));
        press(&mut app, ctrl_c()).await;
        assert!(app.should_quit);
    }

    /// Typing ahead during a turn left only the 'c's in the input box: the
    /// loading guard let the bare 'c' code through.
    #[tokio::test]
    async fn plain_c_is_blocked_while_loading() {
        let mut app = App::new("claude-sonnet-4-6", std::path::Path::new("/tmp"));
        app.start_loading();
        for mods in [KeyModifiers::NONE, KeyModifiers::ALT] {
            press(&mut app, KeyEvent::new(KeyCode::Char('c'), mods)).await;
        }
        assert!(app.input.is_empty(), "typed {:?}", app.input);
        assert!(!app.should_quit);
        press(&mut app, ctrl_c()).await;
        assert!(app.should_quit);
    }
}

#[cfg(test)]
mod prompt_hook_tests {
    use super::*;
    use crossterm::event::KeyEvent;

    fn hooked(dir: &std::path::Path, command: &str) -> Config {
        Config {
            cwd: dir.to_path_buf(),
            hooks: Some(crate::settings::HooksConfig {
                user_prompt_submit: vec![crate::settings::HookEntry {
                    matcher: String::new(),
                    command: command.into(),
                }],
                ..Default::default()
            }),
            ..Config::default()
        }
    }

    /// Press `code`, then, with `finish`, do what the event loop does once
    /// the prompt hooks are done.
    async fn press(app: &mut App, config: &mut Config, code: KeyCode, finish: bool) {
        let mut messages = Vec::new();
        let mut client =
            ApiBackend::Anthropic(crate::api::ClaudeClient::new("sk-ant-test").unwrap());
        let perm_state = PermissionState::new(false, &[], &[]);
        let skills = std::collections::HashMap::new();
        let mut system_prompt = String::new();
        let (tx, _rx) = mpsc::unbounded_channel();
        let todo_state = TodoState::default();
        let mut session = Session::at_path("current", config.cwd.join("current.jsonl"));
        let spawn_registry = crate::spawn::new_registry();
        let key = KeyEvent::new(code, KeyModifiers::NONE);
        handle_key(KeyCtx {
            key,
            app: &mut *app,
            messages: &mut messages,
            client: &mut client,
            tools: &[],
            config: &mut *config,
            perm_state: &perm_state,
            skills: &skills,
            system_prompt: &mut system_prompt,
            tx: &tx,
            todo_state: &todo_state,
            session: &mut session,
            saved_count: &mut 0,
            mcp_statuses: &[],
            spawn_registry: &spawn_registry,
        })
        .await
        .unwrap();
        if finish && let Some(pending) = app.prompt_hooks.take() {
            finish_prompt_hooks(
                pending,
                KeyCtx {
                    key,
                    app,
                    messages: &mut messages,
                    client: &mut client,
                    tools: &[],
                    config,
                    perm_state: &perm_state,
                    skills: &skills,
                    system_prompt: &mut system_prompt,
                    tx: &tx,
                    todo_state: &todo_state,
                    session: &mut session,
                    saved_count: &mut 0,
                    mcp_statuses: &[],
                    spawn_registry: &spawn_registry,
                },
            )
            .await
            .unwrap();
        }
    }

    /// Enter awaited the hooks inside the key handler, so a slow hook froze
    /// the screen and Esc for up to a minute per hook. Esc now cancels them,
    /// including what the hook started in the background.
    #[tokio::test]
    async fn slow_prompt_hook_leaves_the_ui_running_and_esc_kills_it() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = hooked(dir.path(), "(sleep 1; touch ran.txt) & wait");
        let mut app = App::new("claude-sonnet-4-6", dir.path());
        app.input = "hello".chars().collect();
        app.cursor = app.input.len();

        let started = std::time::Instant::now();
        press(&mut app, &mut config, KeyCode::Enter, false).await;
        assert!(started.elapsed() < std::time::Duration::from_millis(500));
        assert!(app.prompt_hooks.is_some() && app.is_loading);
        assert_eq!(app.input.iter().collect::<String>(), "hello");
        assert!(app.entries.iter().all(|e| e.text != "hello"), "shown early");

        // Let the hook start before cancelling it.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        press(&mut app, &mut config, KeyCode::Esc, false).await;
        assert!(app.prompt_hooks.is_none() && !app.is_loading);
        assert_eq!(app.input.iter().collect::<String>(), "hello");
        assert!(app.entries.last().unwrap().text.contains("hooks cancelled"));

        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        assert!(!dir.path().join("ran.txt").exists(), "hook outlived Esc");
    }

    /// The replay after the hooks finish runs them once and applies a stop.
    #[tokio::test]
    async fn typed_prompt_is_replayed_with_the_hook_result() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = hooked(
            dir.path(),
            r#"printf '%s\n' "$CLAUDE_MESSAGE" >> seen.txt; echo nope; exit 2"#,
        );
        let mut app = App::new("claude-sonnet-4-6", dir.path());
        app.input = "hello".chars().collect();
        app.cursor = app.input.len();
        press(&mut app, &mut config, KeyCode::Enter, true).await;
        assert!(app.prompt_hooks.is_none() && !app.is_loading);
        assert!(app.prompt_hook_result.is_none());
        assert_eq!(app.input.iter().collect::<String>(), "hello");
        let last = &app.entries.last().unwrap().text;
        assert!(
            last.contains("blocked by a userPromptSubmit hook: nope"),
            "{last}"
        );
        let seen = std::fs::read_to_string(dir.path().join("seen.txt")).unwrap();
        assert_eq!(seen, "hello\n");
    }
}
