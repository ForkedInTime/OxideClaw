//! Slash-command dispatch — the `CommandAction` match carved out of
//! `handle_key` mechanically. No behaviour change.

use super::*;

/// Tools that run shell commands, directly or through a sub-agent that has
/// its own Bash. `disableSkillShellExecution` removes them from skill turns.
const SKILL_SHELL_TOOLS: &[&str] = &["Bash", "PowerShell", "Agent", "TeamCreate", "SendMessage"];

/// The tool list for a `/skill` turn. A prompt note alone left Bash both
/// advertised and executable, so with `Bash(*)` allowed or in bypass mode a
/// skill still ran shell commands with the flag on.
fn skill_turn_tools(tools: &[DynTool], disable_shell: bool) -> Vec<DynTool> {
    tools
        .iter()
        .filter(|t| !(disable_shell && SKILL_SHELL_TOOLS.contains(&t.name())))
        .cloned()
        .collect()
}

/// Run one `/command` line: build the `CommandContext`, dispatch, and
/// apply the resulting `CommandAction` to app/session state.
pub(super) async fn run_slash_command(input: String, k: KeyCtx<'_>) -> Result<()> {
    let KeyCtx {
        key: _,
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
        spawn_registry,
    } = k;
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
        brief_mode: app.brief_mode,
        btw_note: app.btw_note.as_deref(),
    };

    let action = dispatch(&input, &ctx);
    if starts_model_turn(&action, &input, skills, mcp_statuses) && budget_blocks(app, &input) {
        return Ok(());
    }
    match action {
        CommandAction::Quit => {
            app.should_quit = true;
        }
        CommandAction::Clear => {
            messages.clear();
            messages.shrink_to_fit();
            // New turns go to a fresh session; the cleared one stays
            // resumable. Keeping the old session with a stale saved_count
            // left new turns unsaved until history outgrew it, then skipped
            // the first N of them.
            if !config.no_session_persistence
                && let Ok(fresh) = Session::new().await
            {
                *session = fresh;
            } else {
                // Same session, new conversation: /undo and /redo must not
                // reach the cleared turns.
                session.meta.timeline.clear();
                session.meta.redo.clear();
                let _ = session.save_redo(false).await;
            }
            *saved_count = 0;
            app.clear();
            app.pending_screen_clear = true;
            // Refresh recent sessions so the welcome banner is up-to-date
            if let Ok(list) = Session::list().await {
                app.recent_sessions = list
                    .into_iter()
                    .filter(|m| m.id != session.id)
                    .take(5)
                    .map(|m| {
                        let id_short = if m.id.len() >= 8 {
                            short_id(&m.id, 8).to_string()
                        } else {
                            m.id.clone()
                        };
                        (m.name, id_short, m.preview)
                    })
                    .collect();
            }
        }
        CommandAction::Compact => {
            if messages.is_empty() {
                app.entries
                    .push(ChatEntry::system("Nothing to compact yet."));
            } else if app.compacting {
                app.entries.push(ChatEntry::system(
                    "Already compacting — wait for it to finish.",
                ));
            } else {
                app.entries
                    .push(ChatEntry::system("Compacting conversation…"));
                // Same hook contract as auto-compact: preCompact/postCompact
                // fire for every compact cycle, not just the automatic one.
                if let Some(hook_cfg) = &config.hooks
                    && !config.disable_all_hooks
                {
                    hooks::run_pre_compact_hooks(hook_cfg, &session.id, &config.cwd).await;
                }
                app.compacting = true;
                let c2 = client.clone();
                // Snip only what is summarised: a failed summary must leave
                // the live history as it was.
                let base = messages.clone();
                let mut msgs = base.clone();
                snip_compact(&mut msgs, &config.model);
                let cfg = config.clone();
                let tx2 = tx.clone();
                let sid = session.id.clone();
                tokio::spawn(async move {
                    let bill = |u: &Usage| {
                        let _ = tx2.send(AppEvent::usage(&cfg.model, u));
                    };
                    match summarize_compact(&c2, &msgs, &cfg, bill).await {
                        Ok(r) => {
                            let summary_len = r
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
                            if let Some(hook_cfg) = &cfg.hooks
                                && !cfg.disable_all_hooks
                            {
                                hooks::run_post_compact_hooks(hook_cfg, &sid, &cfg.cwd).await;
                            }
                            let _ = tx2.send(AppEvent::Compacted {
                                replacement: r,
                                summary_len,
                                base: Some((sid, base)),
                            });
                        }
                        Err(e) => {
                            let _ =
                                tx2.send(AppEvent::CompactFailed(format!("Compact failed: {e}")));
                        }
                    }
                });
            }
        }
        CommandAction::ToggleVim => {
            app.vim_enabled = !app.vim_enabled;
            if !app.vim_enabled {
                // Leaving vim: ensure we're in "insert" state so input works normally
                app.vim_normal = false;
                app.vim_pending = None;
            }
            let msg = if app.vim_enabled {
                "Vim mode ON\n\nPress Esc to enter normal mode, i/a/A/I to re-enter insert.\nNormal: h/l move, w/b/e word, 0/$ line, x delete char, dd clear line, j/k scroll."
            } else {
                "Vim mode OFF — standard (readline) bindings."
            };
            app.overlay = Some(Overlay::new("vim", msg));
        }
        CommandAction::SetModel(model) => {
            match switch_model(model, config, app, client, system_prompt) {
                Ok(msg) => app.overlay = Some(Overlay::new("model", msg)),
                Err(e) => app
                    .entries
                    .push(ChatEntry::error(format!("Backend error: {e}"))),
            }
        }
        CommandAction::ListModels => {
            // Build combined Anthropic + Ollama interactive model picker
            let ollama_models = crate::api::list_ollama_models(&config.ollama_host).await;
            let mut lines = Vec::new();
            let mut ids = Vec::new();
            let total = crate::commands::KNOWN_MODELS.len() + ollama_models.len();
            lines.push(format!("Models ({})\n", total));
            lines.push(format!("Current: {}\n", config.model));
            // Anthropic models
            lines.push("── Anthropic ──".to_string());
            for (i, (id, desc)) in crate::commands::KNOWN_MODELS.iter().enumerate() {
                let marker = if *id == config.model { " ▶" } else { "" };
                lines.push(format!("  {}. {} — {}{}", i + 1, id, desc, marker));
                ids.push(id.to_string());
            }
            // Ollama models (if any)
            if !ollama_models.is_empty() {
                lines.push(String::new());
                lines.push("── Ollama (local) ──".to_string());
                let offset = crate::commands::KNOWN_MODELS.len();
                for (i, m) in ollama_models.iter().enumerate() {
                    let marker = if *m == config.model { " ▶" } else { "" };
                    lines.push(format!("  {}. {}{}", offset + i + 1, m, marker));
                    ids.push(m.clone());
                }
            }
            lines.push(String::new());
            lines.push("  ↑↓ select · Enter switch · 1-9 quick pick · Esc close".into());
            app.overlay = Some(Overlay::with_items("models", lines.join("\n"), ids));
        }
        CommandAction::ListHelp => {
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
        CommandAction::ShowHelpCategory(idx) => {
            let cats = crate::commands::HELP_CATEGORIES;
            if let Some((name, _, commands)) = cats.get(idx) {
                let mut lines = vec![format!("{}\n", name)];
                let mut ids = Vec::new();
                for (i, (cmd, desc)) in commands.iter().enumerate() {
                    lines.push(format!("  {}. {:16} {}", i + 1, cmd, desc));
                    ids.push(crate::commands::help_picker_input(cmd));
                }
                lines.push(String::new());
                lines.push("  ↑↓ select · Enter run · 1-9 quick pick · Esc close".into());
                app.overlay = Some(Overlay::with_items("help-commands", lines.join("\n"), ids));
            }
        }
        CommandAction::Message(text) => {
            // Readable command output — color-coded, not dim/italic
            app.entries.push(ChatEntry::command_output(text));
            app.follow_bottom = true;
        }
        CommandAction::SendPrompt(prompt) => {
            // Command prompts (/review, /init, /commit, ...) are real turns:
            // Done hands back the full history with the prompt in it.
            app.entries.push(ChatEntry::user(input.clone()));
            app.scroll_to_bottom();
            app.start_loading();
            begin_agent_turn(session, config).await;
            push_prompt_turn(messages, vec![ContentBlock::Text { text: prompt }], session).await;
            let snapshot = messages.clone();
            let c2 = client.clone();
            let tvec = tools.to_vec();
            let cfg = config.clone();
            let tx2 = tx.clone();
            let sp = system_prompt.clone();
            let ps = perm_state.clone();
            let pm = app.plan_mode;
            let budget_left = app.cost_tracker.remaining();
            let sid3 = session.id.clone();
            let turn_history = TurnHistory::default();
            app.turn_history = Some(turn_history.clone());
            let handle = tokio::spawn(async move {
                run_api_task(ApiTask {
                    client: c2,
                    tools: tvec,
                    messages: snapshot,
                    config: cfg,
                    perm_state: ps,
                    system_prompt: sp,
                    tx: tx2,
                    plan_mode: pm,
                    skill_no_shell: false,
                    budget_remaining_usd: budget_left,
                    session_id: sid3,
                    history: turn_history,
                })
                .await;
            });
            app.api_task = Some(handle.abort_handle());
        }
        CommandAction::Rewind(None) => {
            if app.is_loading {
                app.entries.push(ChatEntry::system(
                    "[undo] cannot undo while an assistant turn is running",
                ));
            } else {
                timeline::open_rewind_picker(app, messages);
            }
        }
        CommandAction::Rewind(Some(n)) | CommandAction::Undo(n) => {
            timeline::undo(app, messages, session, saved_count, config, n).await;
        }
        CommandAction::Redo(n) => {
            timeline::redo(app, messages, session, saved_count, config, n).await;
        }
        CommandAction::ResumeSession(id_or_prefix) => {
            let full_id = match Session::resolve(&id_or_prefix).await {
                Ok(id) => Some(id),
                Err(e) => {
                    app.overlay = Some(Overlay::new("error", format!("{e}. Try /session list")));
                    None
                }
            };

            if let Some(id) = full_id {
                match Session::resume(&id).await {
                    Ok((new_session, loaded_messages)) => {
                        let display = entries_from_messages(&loaded_messages);
                        *saved_count = loaded_messages.len();
                        *messages = loaded_messages;
                        app.entries = display;
                        app.streaming = String::new();
                        app.show_welcome = false;
                        app.scroll_to_bottom();
                        let resume_name = new_session.meta.name.clone();
                        let resume_count = *saved_count;
                        app.session_name = resume_name.clone();
                        *session = new_session;
                        app.overlay = Some(Overlay::new(
                            "resume",
                            format!(
                                "Resumed session '{}'\n{} messages loaded.",
                                resume_name, resume_count
                            ),
                        ));
                    }
                    Err(e) => {
                        app.overlay = Some(Overlay::new(
                            "error",
                            format!("Could not resume session: {e}"),
                        ));
                    }
                }
            }
        }
        CommandAction::ListSessions => match Session::list().await {
            Err(e) => {
                app.overlay = Some(Overlay::new(
                    "sessions",
                    format!("Error listing sessions: {e}"),
                ));
            }
            Ok(list) if list.is_empty() => {
                app.overlay = Some(Overlay::new(
                    "sessions",
                    format!(
                        "Sessions\n\nNo saved sessions yet.\nCurrent: {} ({})\n\nSessions are saved automatically.",
                        session.meta.name,
                        short_id(&session.id, 8)
                    ),
                ));
            }
            Ok(list) => {
                let mut lines = vec![
                    format!("Sessions ({})\n", list.len()),
                    format!(
                        "Current: {} ({})\n",
                        session.meta.name,
                        short_id(&session.id, 8)
                    ),
                ];
                let mut ids = Vec::new();
                for (i, meta) in list.iter().enumerate() {
                    let current = if meta.id == session.id { " ◀" } else { "" };
                    let preview = if meta.preview.is_empty() {
                        "(empty)"
                    } else {
                        &meta.preview
                    };
                    lines.push(format!(
                        "  {}. [{}] {} — {}{}",
                        i + 1,
                        short_id(&meta.id, 8),
                        meta.name,
                        preview,
                        current
                    ));
                    ids.push(meta.id.clone());
                }
                lines.push(String::new());
                lines.push(
                    "  ↑↓ select · Enter resume · d delete · 1-9 quick pick · Esc close".into(),
                );
                app.overlay = Some(Overlay::with_items("sessions", lines.join("\n"), ids));
            }
        },
        CommandAction::ClearAllSessions => {
            match Session::list().await {
                Ok(list) => {
                    let current_id = session.id.clone();
                    let mut deleted = 0usize;
                    for meta in &list {
                        if meta.id != current_id && Session::delete(&meta.id).await.is_ok() {
                            deleted += 1;
                        }
                    }
                    app.entries.push(ChatEntry::system(format!(
                        "Cleared {deleted} session(s). Current session kept."
                    )));
                    // Refresh recent sessions in banner
                    app.recent_sessions.clear();
                }
                Err(e) => {
                    app.entries
                        .push(ChatEntry::error(format!("Failed to list sessions: {e}")));
                }
            }
        }
        CommandAction::ExportCurrentSession => {
            let id = session.id.clone();
            let name = session.meta.name.clone();
            let dest = config.cwd.join(format!("session-{}.md", short_id(&id, 8)));
            match Session::export(&id, &dest).await {
                Ok(path) => {
                    app.overlay = Some(Overlay::new(
                        "export",
                        format!("Session '{}' exported to\n{}", name, path.display()),
                    ));
                }
                Err(e) => {
                    app.overlay = Some(Overlay::new("error", format!("Export failed: {e}")));
                }
            }
        }
        CommandAction::RenameSession(name) => match session.rename(&name).await {
            Ok(()) => {
                app.session_name = name.clone();
                app.overlay = Some(Overlay::new(
                    "rename",
                    format!("Session renamed to '{name}'"),
                ));
            }
            Err(e) => {
                app.overlay = Some(Overlay::new("error", format!("Rename failed: {e}")));
            }
        },
        CommandAction::TogglePlanMode => {
            app.plan_mode = !app.plan_mode;
            config.plan_mode = app.plan_mode;
            let msg = if app.plan_mode {
                "Plan mode ON — destructive tools (Bash, Write, Edit) are blocked."
            } else {
                "Plan mode OFF — all tools are available."
            };
            app.entries.push(ChatEntry::system(msg.to_string()));
            app.scroll_to_bottom();
        }
        CommandAction::AttachImage(path) => {
            app.pending_image = Some(path.clone());
            app.overlay = Some(Overlay::new(
                "image",
                format!("Image attached: {path}\nIt will be included in your next message."),
            ));
        }
        CommandAction::ToggleBriefMode => {
            app.brief_mode = !app.brief_mode;
            let msg = if app.brief_mode {
                "Brief mode ON — responses will be concise."
            } else {
                "Brief mode OFF — normal response length."
            };
            app.entries.push(ChatEntry::system(msg.to_string()));
            app.scroll_to_bottom();
        }
        CommandAction::SetBtwNote(note) => {
            app.btw_note = Some(note.clone());
            app.entries.push(ChatEntry::system(format!(
                "btw note set — will be prepended to your next message: \"{note}\""
            )));
            app.scroll_to_bottom();
        }
        CommandAction::SetOutputStyle(name) => {
            if name.eq_ignore_ascii_case("default") {
                config.output_style = None;
                config.output_style_prompt = None;
                *system_prompt = config.build_system_prompt();
                let _ = crate::config::Config::save_user_setting(
                    "outputStyle",
                    serde_json::Value::String("default".into()),
                );
                app.entries.push(ChatEntry::system(
                    "Output style cleared — using default Claude responses.".to_string(),
                ));
            } else {
                let styles = crate::config::Config::load_output_styles(&config.cwd);
                if let Some(def) = styles.iter().find(|s| s.name.eq_ignore_ascii_case(&name)) {
                    config.output_style = Some(def.name.clone());
                    config.output_style_prompt = Some(def.prompt.clone());
                    *system_prompt = config.build_system_prompt();
                    let _ = crate::config::Config::save_user_setting(
                        "outputStyle",
                        serde_json::Value::String(def.name.clone()),
                    );
                    app.entries.push(ChatEntry::system(format!(
                        "Output style set to '{}' — {}",
                        def.name, def.description
                    )));
                } else {
                    app.entries.push(ChatEntry::system(format!(
                        "Unknown style '{}'. Type /output-style to list available styles.",
                        name
                    )));
                }
            }
            app.scroll_to_bottom();
        }
        CommandAction::SetEffort(level) => {
            config.effort = level.clone();
            app.effort = level.clone();
            let _ = crate::config::Config::save_user_setting(
                "effort",
                match &level {
                    Some(l) => serde_json::Value::String(l.clone()),
                    None => serde_json::Value::Null,
                },
            );
            let note = match &level {
                None => "Effort cleared — the API default applies.".to_string(),
                Some(l) if crate::api::thinking::supports_effort(&config.model) => {
                    format!("Effort set to '{l}' (sent as output_config.effort).")
                }
                Some(l)
                    if crate::api::openai_compat::responses_reasoning_model(
                        &config.model,
                        config.openai_api,
                    ) =>
                {
                    format!("Effort set to '{l}' (sent as reasoning.effort).")
                }
                Some(l) => format!(
                    "Effort set to '{l}'. {} has no effort parameter, so it is applied as a prompt nudge.",
                    config.model
                ),
            };
            app.entries.push(ChatEntry::system(note));
        }
        CommandAction::SetTheme(name) => {
            config.theme = Some(name.clone());
            app.theme = name.clone();
            let _ = crate::config::Config::save_user_setting(
                "theme",
                serde_json::Value::String(name.clone()),
            );
            app.entries
                .push(ChatEntry::system(format!("Theme set to '{name}'.")));
            app.scroll_to_bottom();
        }
        CommandAction::SetVoiceEnabled(enabled) => {
            config.voice_enabled = enabled;
            if !enabled {
                app.pending_clone_tier = None;
            }
            let _ = crate::config::Config::save_user_setting(
                "voiceEnabled",
                serde_json::Value::Bool(enabled),
            );
            let msg = crate::voice::voice_status(enabled, config.tts_enabled);
            app.overlay = Some(Overlay::new("voice", msg));
        }
        CommandAction::SetTtsEnabled(enabled) => {
            // Only the state that took effect is kept: saving a refused "on"
            // made every later turn (and launch) fail with "TTS failed".
            let mut effective = enabled;
            if enabled {
                let has_xtts = crate::voice::xtts_available();
                let has_player = crate::voice::audio_player_available();
                effective = has_xtts && has_player;

                if !has_xtts {
                    app.entries.push(ChatEntry::system(format!(
                        "Cannot enable TTS — XTTS v2 not found.\n\n\
                         Install:\n\
                           uv tool install TTS --python 3.11 \\\n\
                             --with 'transformers<4.46' --with 'torch<2.6' --with 'torchaudio<2.6'\n\n{}",
                        crate::voice::XTTS_FIRST_RUN_HINT
                    )));
                } else if !has_player {
                    app.entries.push(ChatEntry::system(
                        "Cannot enable TTS — no audio player.\n\
                         Install: sudo pacman -S mpv   # or alsa-utils"
                            .to_string(),
                    ));
                } else {
                    start_xtts_server_in_background(tx);
                    app.entries.push(ChatEntry::system(
                        "TTS on — starting XTTS v2 server (loading model, ~10s)...".to_string(),
                    ));
                }
                app.follow_bottom = true;
            } else {
                let msg = if crate::voice::stop_xtts_server() {
                    "TTS off. XTTS v2 server stopped."
                } else {
                    "TTS off."
                };
                app.entries.push(ChatEntry::system(msg.to_string()));
                app.follow_bottom = true;
            }
            config.tts_enabled = effective;
            let _ = crate::config::Config::save_user_setting(
                "ttsEnabled",
                serde_json::Value::Bool(effective),
            );
        }
        CommandAction::ListVoiceModels => {
            let voices = crate::voice::find_all_voices();
            if voices.is_empty() {
                app.overlay = Some(Overlay::new(
                    "voices",
                    "No voice models found\n\n\
                     Install XTTS v2:\n\
                       uv tool install TTS --python 3.11 \\\n\
                         --with 'transformers<4.46' --with 'torch<2.6' --with 'torchaudio<2.6'\n\n\
                     Then record a custom voice:\n\
                       /voice clone"
                        .to_string(),
                ));
            } else {
                let current = crate::voice::active_voice_id(config.tts_voice_model.as_deref());
                let mut lines = vec![format!("Voice models ({})\n", voices.len())];
                let mut ids = Vec::new();
                for (i, (name, path)) in voices.iter().enumerate() {
                    let marker = if current == *path { " ▶" } else { "" };
                    lines.push(format!("  {}. {}{}", i + 1, name, marker));
                    ids.push(path.clone());
                }
                lines.push(String::new());
                lines.push("  ↑↓ select · Enter preview & set · 1-9 quick pick · Esc close".into());
                app.overlay = Some(Overlay::with_items("voices", lines.join("\n"), ids));
            }
        }
        CommandAction::SetSandboxEnabled { enabled, mode } => {
            config.sandbox_enabled = enabled;
            if enabled && !mode.is_empty() {
                config.sandbox_mode = mode.clone();
                let _ = crate::config::Config::save_user_setting(
                    "sandboxMode",
                    serde_json::Value::String(mode),
                );
            }
            let _ = crate::config::Config::save_user_setting(
                "sandboxEnabled",
                serde_json::Value::Bool(enabled),
            );
            let mut msg = String::new();
            if let Some(why) = config.fall_back_from_full_auto() {
                msg.push_str(&why);
                msg.push_str("\n\n");
            }
            // Enabling is the moment the user forms a belief about how
            // protected they are. If the active mode cannot enforce
            // anything, say so here rather than burying it in status.
            if enabled
                && let Some(warning) = crate::sandbox::weak_mode_warning(&config.sandbox_mode)
            {
                msg.push_str("⚠ ");
                msg.push_str(&warning);
                msg.push_str("\n\n");
            }
            msg.push_str(&crate::sandbox::sandbox_status(
                enabled,
                &config.sandbox_mode,
            ));
            app.overlay = Some(Overlay::new("sandbox", msg));
        }
        CommandAction::ShowThinkback => {
            let msg_count = messages.len();
            let user_count = messages.iter().filter(|m| m.role == Role::User).count();
            let asst_count = messages
                .iter()
                .filter(|m| m.role == Role::Assistant)
                .count();
            let tool_calls = app
                .entries
                .iter()
                .filter(|e| matches!(e.kind, crate::tui::app::EntryKind::ToolCall))
                .count();
            let total_in: u64 = app.turn_costs.iter().map(|(i, _)| i).sum();
            let total_out: u64 = app.turn_costs.iter().map(|(_, o)| o).sum();

            // Build ASCII bar chart (tokens_in per turn)
            let chart = if app.turn_costs.is_empty() {
                "  (no turns yet)".to_string()
            } else {
                let max_tokens = app
                    .turn_costs
                    .iter()
                    .map(|(i, _)| *i)
                    .max()
                    .unwrap_or(1)
                    .max(1);
                let bar_width = 20usize;
                app.turn_costs
                    .iter()
                    .enumerate()
                    .take(12) // cap at 12 turns to fit overlay
                    .map(|(i, (tin, tout))| {
                        let bars = (((*tin as f64 / max_tokens as f64) * bar_width as f64).round()
                            as usize)
                            .max(1);
                        format!(
                            "  Turn {:>2}: {:░<width$} {}in {}out",
                            i + 1,
                            "█".repeat(bars),
                            tin,
                            tout,
                            width = bar_width - bars
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            };

            let text = format!(
                "Session Statistics\n\n\
                 Messages:    {msg_count} ({user_count} user, {asst_count} assistant)\n\
                 Tool calls:  {tool_calls}\n\
                 Turns:       {}\n\
                 Session:     {}\n\
                 Model:       {}\n\n\
                 Tokens in:   {} total\n\
                 Tokens out:  {} total\n\n\
                 Per-turn tokens (in):\n{}",
                messages.iter().filter(|m| is_prompt(m)).count(),
                &session.id[..8.min(session.id.len())],
                app.model,
                total_in,
                total_out,
                chart,
            );
            app.overlay = Some(Overlay::new("thinkback", text));
        }
        CommandAction::TeleportExport => {
            let teleport_path = crate::config::Config::config_dir().join("teleport.json");
            let export_data = serde_json::json!({
                "session_id": session.id,
                "session_name": session.meta.name,
                "model": config.model,
                "messages": messages,
                "turn_count": messages.iter().filter(|m| is_prompt(m)).count(),
            });
            match serde_json::to_string_pretty(&export_data) {
                Ok(json_str) => match std::fs::write(&teleport_path, &json_str) {
                    Ok(()) => {
                        app.overlay = Some(Overlay::new(
                            "teleport",
                            format!(
                                "Teleport export saved\n\n\
                             File: {}\n\
                             Messages: {}  Model: {}",
                                teleport_path.display(),
                                messages.len(),
                                config.model
                            ),
                        ));
                    }
                    Err(e) => {
                        app.overlay = Some(Overlay::new(
                            "error",
                            format!("Teleport export failed: {e}"),
                        ));
                    }
                },
                Err(e) => {
                    app.overlay = Some(Overlay::new(
                        "error",
                        format!("Teleport serialisation failed: {e}"),
                    ));
                }
            }
        }
        CommandAction::TeleportImport => {
            let teleport_path = crate::config::Config::config_dir().join("teleport.json");
            if !teleport_path.exists() {
                app.overlay = Some(Overlay::new(
                    "teleport",
                    "No teleport file found.\nUse /teleport export first.".to_string(),
                ));
            } else {
                let result = std::fs::read_to_string(&teleport_path)
                    .ok()
                    .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
                    .and_then(|data| {
                        let msgs =
                            serde_json::from_value::<Vec<Message>>(data.get("messages")?.clone())
                                .ok()?;
                        let model = data
                            .get("model")
                            .and_then(|v| v.as_str())
                            .unwrap_or(&config.model)
                            .to_string();
                        Some((msgs, model))
                    });
                match result {
                    None => {
                        app.overlay = Some(Overlay::new(
                            "error",
                            "Teleport file is invalid or has no messages.".to_string(),
                        ));
                    }
                    Some((msgs, src_model)) => {
                        let count = msgs.len();
                        let display = entries_from_messages(&msgs);
                        *messages = msgs;
                        // The import is its own conversation: appending it to
                        // the current session's file interleaved the two.
                        // A fresh session (as /clear) keeps the old one
                        // resumable.
                        if !config.no_session_persistence
                            && let Ok(fresh) = Session::new().await
                        {
                            *session = fresh;
                        } else {
                            session.meta.timeline.clear();
                            session.meta.redo.clear();
                            let _ = session.save_redo(false).await;
                        }
                        *saved_count = 0;
                        app.entries = display;
                        app.show_welcome = false;
                        app.scroll_to_bottom();
                        app.overlay = Some(Overlay::new(
                            "teleport",
                            format!(
                                "Teleport imported\n\n{count} messages loaded\nOriginal model: {src_model}"
                            ),
                        ));
                    }
                }
            }
        }
        CommandAction::ShareSession => {
            let id = session.id.clone();
            let name = session.meta.name.clone();
            let safe_name = name.replace(|c: char| !c.is_alphanumeric() && c != '-', "-");
            let dest =
                config
                    .cwd
                    .join(format!("share-{}-{}.md", safe_name, &id[..8.min(id.len())]));
            match Session::export(&id, &dest).await {
                Ok(path) => {
                    app.overlay = Some(Overlay::new(
                        "share",
                        format!(
                            "Session shared\n\nFile: {}\n\nShare this markdown file with your team.",
                            path.display()
                        ),
                    ));
                }
                Err(e) => {
                    app.overlay = Some(Overlay::new("error", format!("Share failed: {e}")));
                }
            }
        }
        CommandAction::ShareClipboard => {
            let id = session.id.clone();
            let tx2 = tx.clone();
            tokio::spawn(async move {
                match Session::export_to_string(&id).await {
                    Err(e) => {
                        let _ = tx2.send(AppEvent::SystemMessage(format!(
                            "Clipboard export failed: {e}"
                        )));
                    }
                    Ok(content) => {
                        let copied = tokio::task::spawn_blocking(move || {
                            crate::commands::copy_to_clipboard(&content)
                        })
                        .await
                        .unwrap_or(false);
                        let msg = if copied {
                            "Session copied to clipboard.".to_string()
                        } else {
                            format!(
                                "Could not copy to clipboard.\n{}",
                                crate::commands::CLIPBOARD_INSTALL_HINT
                            )
                        };
                        let _ = tx2.send(AppEvent::SystemMessage(msg));
                    }
                }
            });
        }
        CommandAction::SetNotificationsEnabled(enabled) => {
            config.notifications_enabled = enabled;
            let _ = crate::config::Config::save_user_setting(
                "notificationsEnabled",
                serde_json::Value::Bool(enabled),
            );
            let msg = if enabled {
                "Notifications enabled — terminal bell + notify-send on task completion."
            } else {
                "Notifications disabled."
            };
            app.entries.push(ChatEntry::system(msg.to_string()));
            app.scroll_to_bottom();
        }
        CommandAction::SetSandboxNetwork(allow) => {
            config.sandbox_allow_network = allow;
            let _ = crate::config::Config::save_user_setting(
                "sandboxAllowNetwork",
                serde_json::Value::Bool(allow),
            );
            let msg = if allow {
                "Sandbox network: allowed (bwrap will not use --unshare-net)."
            } else {
                "Sandbox network: blocked (bwrap will use --unshare-net)."
            };
            app.entries.push(ChatEntry::system(msg.to_string()));
            app.scroll_to_bottom();
        }
        CommandAction::EditClaudeMd => {
            let claude_md = crate::config::Config::config_dir().join("CLAUDE.md");
            // Create the file if it doesn't exist. It replaces Claude Code's
            // ~/.claude/CLAUDE.md as the global file, so start from a copy.
            let created_seed: Option<String> = if !claude_md.exists() {
                if let Some(parent) = claude_md.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                let seed = crate::config::Config::claude_code_dir()
                    .and_then(|d| std::fs::read_to_string(d.join("CLAUDE.md")).ok())
                    .unwrap_or_else(|| "# CLAUDE.md\n\n".to_string());
                std::fs::write(&claude_md, &seed).ok().map(|()| seed)
            } else {
                None
            };
            // The editor never ran or failed: an untouched seed would shadow
            // ~/.claude/CLAUDE.md from now on, so remove it.
            let discard_seed = || discard_untouched_seed(&claude_md, created_seed.as_deref());
            let editor = editor_command(std::env::var("VISUAL").ok(), std::env::var("EDITOR").ok());
            // Suspend raw mode, run editor, restore
            suspend_tty();
            let status = editor_process(&editor, &claude_md).status().await;
            resume_tty();
            // The child (sh's "not found", an editor failing before its alt
            // screen) may have written to the TTY; ratatui only repaints
            // changed cells, so clear and recreate before the next draw.
            app.pending_screen_clear = true;
            match status {
                Ok(s) if s.success() => {}
                // sh's "command not found": the editor never ran.
                Ok(s) if cfg!(unix) && s.code() == Some(127) => {
                    discard_seed();
                    app.entries.push(ChatEntry::error(format!(
                        "Could not launch editor '{editor}': command not found. \
                         Set $VISUAL or $EDITOR."
                    )));
                    app.scroll_to_bottom();
                    return Ok(());
                }
                Ok(s) => {
                    discard_seed();
                    app.entries.push(ChatEntry::error(format!(
                        "Editor '{editor}' exited with {s}; CLAUDE.md not reloaded."
                    )));
                    app.scroll_to_bottom();
                    return Ok(());
                }
                Err(e) => {
                    discard_seed();
                    app.entries.push(ChatEntry::error(format!(
                        "Could not launch editor '{editor}': {e}. Set $VISUAL or $EDITOR."
                    )));
                    app.scroll_to_bottom();
                    return Ok(());
                }
            }
            // Reload CLAUDE.md into config; --bare never loads it.
            if !config.bare_mode {
                config.claudemd = crate::config::Config::load_claude_md(&config.cwd);
            }
            *system_prompt = config.build_system_prompt();
            app.entries.push(ChatEntry::system(format!(
                "CLAUDE.md reloaded ({} chars).",
                config.claudemd.len()
            )));
            app.scroll_to_bottom();
        }
        CommandAction::SearchSessions(query) => match Session::list().await {
            Err(e) => {
                app.overlay = Some(Overlay::new(
                    "search",
                    format!("Error listing sessions: {e}"),
                ));
            }
            Ok(list) => {
                let q = query.to_lowercase();
                let matches: Vec<_> = list
                    .iter()
                    .filter(|m| {
                        m.name.to_lowercase().contains(&q)
                            || m.preview.to_lowercase().contains(&q)
                            || m.id.starts_with(&q)
                    })
                    .collect();
                if matches.is_empty() {
                    app.overlay = Some(Overlay::new(
                        "search",
                        format!("No sessions matching '{query}'."),
                    ));
                } else {
                    let mut lines = vec![format!(
                        "Sessions matching '{}' ({}):\n",
                        query,
                        matches.len()
                    )];
                    for m in matches.iter().take(20) {
                        let preview = if m.preview.is_empty() {
                            "(empty)"
                        } else {
                            &m.preview
                        };
                        lines.push(format!(
                            "  [{}] {} — {}",
                            short_id(&m.id, 8),
                            m.name,
                            preview
                        ));
                    }
                    lines.push(String::new());
                    lines.push("  /resume <id-prefix>  — resume a session".into());
                    app.overlay = Some(Overlay::new("search", lines.join("\n")));
                }
            }
        },
        CommandAction::PluginInstall(spec) => {
            app.entries.push(ChatEntry::system(format!(
                "Installing plugin: {} …",
                spec.trim_start_matches("marketplace:")
            )));
            app.start_loading();
            app.scroll_to_bottom();
            let tx2 = tx.clone();
            app.side_task = Some(tokio::spawn(plugin_install_task(spec, tx2)).abort_handle());
        }
        CommandAction::ReloadSettings => {
            // Hot-reload settings.json without restarting
            let settings = config.load_settings();
            let mut reloaded = Vec::new();
            // Trust may have changed since startup, and it gates the
            // project's autoFixLoop block; rebuild both together.
            config.project_trusted = settings.project_trusted;
            config.apply_auto_fix_settings(settings.auto_fix.as_ref());
            if settings.auto_fix.is_some() {
                reloaded.push("autoFixLoop");
            }

            // OXIDECLAW_OPENAI_API, when set, still wins over the file.
            let openai_api = settings
                .openai_api
                .as_deref()
                .and_then(crate::api::OpenAiApi::parse)
                .unwrap_or_default();
            let openai_api_changed =
                crate::config::app_env("OPENAI_API").is_none() && openai_api != config.openai_api;
            if openai_api_changed {
                config.openai_api = openai_api;
                reloaded.push("openaiApi");
            }
            if let Some(model) = reloaded_model(
                settings.model.as_deref(),
                &mut config.settings_model,
                &config.model,
            ) {
                // The backend family (Anthropic / Ollama / OpenAI-compat) is
                // fixed when the client is built; build first so a model the
                // client cannot serve leaves the current one in place.
                match backend_for_model(config, &model) {
                    Ok(new_client) => {
                        *client = new_client;
                        config.model = model.clone();
                        app.set_model(model);
                        reloaded.push("model");
                    }
                    Err(e) => app.entries.push(ChatEntry::error(format!(
                        "Backend error: {e}\n\nModel unchanged: {}",
                        config.model
                    ))),
                }
            }
            // The API is fixed when the client is built.
            if openai_api_changed
                && !reloaded.contains(&"model")
                && matches!(client, ApiBackend::OpenAiCompat(_))
            {
                match backend_for_model(config, &config.model) {
                    Ok(new_client) => *client = new_client,
                    Err(e) => app
                        .entries
                        .push(ChatEntry::error(format!("Backend error: {e}"))),
                }
            }
            if let Some(ref theme) = settings.theme {
                config.theme = Some(theme.clone());
                app.theme = theme.clone();
                reloaded.push("theme");
            }
            if let Some(effort) = settings.effort {
                config.effort = Some(effort.clone());
                app.effort = Some(effort);
                reloaded.push("effort");
            }
            if let Some(style) = settings.spinner_style {
                config.spinner_style = style.clone();
                app.spinner_style = style;
                reloaded.push("spinnerStyle");
            }
            config.show_thinking_summaries = settings
                .show_thinking_summaries
                .unwrap_or(config.show_thinking_summaries);
            if let Some(tbt) = settings.thinking_budget_tokens {
                config.thinking_budget_tokens = Some(tbt);
                reloaded.push("thinkingBudgetTokens");
            }
            if let Some(v) = settings.verbose {
                config.verbose = v;
                reloaded.push("verbose");
            }
            if let Some(ac) = settings.auto_compact {
                config.auto_compact_enabled = ac;
                reloaded.push("autoCompact");
            }
            if let Some(se) = settings.sandbox_enabled {
                config.sandbox_enabled = se;
                reloaded.push("sandboxEnabled");
            }
            if let Some(mode) = settings.sandbox_mode {
                config.sandbox_mode = mode;
                reloaded.push("sandboxMode");
            }
            let autonomy_fallback = config.fall_back_from_full_auto();

            // Reload CLAUDE.md + AGENTS.md + GEMINI.md; --bare never loads them.
            if !config.bare_mode {
                config.claudemd = crate::config::Config::load_claude_md(&config.cwd);
                config.agentsmd = crate::config::Config::load_agents_md(&config.cwd);
                config.geminimd = crate::config::Config::load_gemini_md(&config.cwd);
            }
            // Turns send this string, not config; without the rebuild the
            // refreshed files never reached the model until a restart.
            *system_prompt = config.build_system_prompt();

            let mut msg = if reloaded.is_empty() {
                "Settings reloaded (no changes detected). CLAUDE.md + AGENTS.md + GEMINI.md refreshed."
                    .to_string()
            } else {
                format!(
                    "Settings reloaded: {}. CLAUDE.md + AGENTS.md + GEMINI.md refreshed.",
                    reloaded.join(", ")
                )
            };
            if let Some(why) = autonomy_fallback {
                msg.push('\n');
                msg.push_str(&why);
            }
            // Otherwise a typo reads as "no changes detected".
            if !settings.load_errors.is_empty() {
                msg.push('\n');
                msg.push_str(&crate::settings::load_errors_notice(&settings.load_errors));
            }
            config.settings_load_errors = settings.load_errors;
            app.entries.push(ChatEntry::system(msg));
            app.scroll_to_bottom();
        }
        CommandAction::ReloadPlugins => {
            // Re-read settings.json mcpServers section
            let count = config.load_settings().mcp_servers.len();
            app.entries.push(ChatEntry::system(format!(
                "Plugin/MCP config reloaded — {} server(s) defined in settings.json.\n\
                 \n\
                 Note: MCP server connections are established at startup.\n\
                 Newly installed plugins require a full restart of oxideclaw to activate.",
                count
            )));
            app.scroll_to_bottom();
        }
        CommandAction::CheckUpgrade => {
            app.entries
                .push(ChatEntry::system("Checking for updates …".to_string()));
            app.start_loading();
            app.scroll_to_bottom();
            let tx2 = tx.clone();
            app.side_task = Some(tokio::spawn(upgrade_check_task(tx2)).abort_handle());
        }
        CommandAction::RunInstall(cmd) => {
            // Signal run_loop to handle this — it needs terminal/cols/rows access.
            app.entries
                .push(ChatEntry::system(format!("Running: {cmd}")));
            app.follow_bottom = true;
            app.pending_install = Some(cmd);
        }

        CommandAction::OpenBrowser(url) => {
            // Try platform-specific openers
            let opened = std::process::Command::new("xdg-open")
                .arg(&url)
                .spawn()
                .is_ok()
                || std::process::Command::new("open").arg(&url).spawn().is_ok()
                || std::process::Command::new("cmd.exe")
                    .args(["/C", "start", &url])
                    .spawn()
                    .is_ok();
            let msg = if opened {
                format!("Opened in browser: {}", url)
            } else {
                format!(
                    "Could not open browser. URL: {}\n(Install xdg-utils on Linux or copy the URL manually.)",
                    url
                )
            };
            app.entries.push(ChatEntry::system(msg));
            app.scroll_to_bottom();
        }
        CommandAction::PluginList => {
            let plugins_path = crate::config::Config::config_dir().join("plugins.json");
            let obj = std::fs::read_to_string(&plugins_path)
                .ok()
                .and_then(|c| serde_json::from_str::<serde_json::Value>(&c).ok())
                .and_then(|v| v.as_object().cloned())
                .unwrap_or_default();
            let text = if obj.is_empty() {
                "No plugins installed.\n\n\
                 Install a plugin:\n\
                 /plugin marketplace add <user/repo>\n\
                 /plugin install <npm-package>"
                    .to_string()
            } else {
                let mut lines = vec![format!("Installed plugins ({})\n", obj.len())];
                for (name, info) in &obj {
                    let from_marketplace = info["marketplace"].as_bool().unwrap_or(false);
                    let spec = info["spec"].as_str().unwrap_or(name);
                    if from_marketplace {
                        lines.push(format!("  {} (from {})", name, spec));
                    } else {
                        lines.push(format!("  {}", name));
                    }
                }
                lines.push(String::new());
                lines.push("/plugin remove <name>  — uninstall a plugin".to_string());
                lines.join("\n")
            };
            app.overlay = Some(Overlay::new("plugins", text));
        }
        CommandAction::PluginRemove(name) => {
            // Where /plugin install registered it: the loader's config dir.
            let settings_path = crate::config::Config::config_dir().join("settings.json");
            let mut removed = false;
            if let Ok(content) = std::fs::read_to_string(&settings_path)
                && let Ok(mut json) = serde_json::from_str::<serde_json::Value>(&content)
            {
                // get_mut, not IndexMut: a `[]` root would panic (and abort).
                if let Some(obj) = json.get_mut("mcpServers").and_then(|v| v.as_object_mut()) {
                    removed = obj.remove(&name).is_some();
                }
                if removed {
                    let _ = crate::config::write_json_atomic(
                        &settings_path,
                        &serde_json::to_string_pretty(&json).unwrap_or_default(),
                    );
                }
            }
            // Remove from plugins.json
            let plugins_path = crate::config::Config::config_dir().join("plugins.json");
            if let Ok(content) = std::fs::read_to_string(&plugins_path)
                && let Ok(mut plugins) = serde_json::from_str::<serde_json::Value>(&content)
                && let Some(obj) = plugins.as_object_mut()
                && obj.remove(&name).is_some()
            {
                let _ = crate::config::write_json_atomic(
                    &plugins_path,
                    &serde_json::to_string_pretty(&plugins).unwrap_or_default(),
                );
            }
            let msg = if removed {
                format!(
                    "Plugin '{}' removed. Restart oxideclaw to deactivate.",
                    name
                )
            } else {
                format!("Plugin '{}' not found in installed plugins.", name)
            };
            app.entries.push(ChatEntry::system(msg));
            app.scroll_to_bottom();
        }
        CommandAction::PluginCommand {
            plugin,
            command,
            args,
        } => {
            // Plugin slash commands → invoke as a prompt so Claude calls
            // the matching MCP tool (e.g. ctx_doctor → mcp__…__ctx_doctor).
            if let Some(server) = plugin_server(mcp_statuses, &plugin) {
                let prompt = plugin_command_prompt(&server.name, &command, &args);
                app.entries.push(ChatEntry::user(input.clone()));
                app.scroll_to_bottom();
                app.start_loading();
                begin_agent_turn(session, config).await;
                push_prompt_turn(messages, vec![ContentBlock::Text { text: prompt }], session)
                    .await;
                let snapshot = messages.clone();
                let c2 = client.clone();
                let tvec = tools.to_vec();
                let cfg = config.clone();
                let tx2 = tx.clone();
                let sp = system_prompt.clone();
                let ps = perm_state.clone();
                let pm = app.plan_mode;
                let budget_left = app.cost_tracker.remaining();
                let sid3 = session.id.clone();
                let turn_history = TurnHistory::default();
                app.turn_history = Some(turn_history.clone());
                let handle = tokio::spawn(async move {
                    run_api_task(ApiTask {
                        client: c2,
                        tools: tvec,
                        messages: snapshot,
                        config: cfg,
                        perm_state: ps,
                        system_prompt: sp,
                        tx: tx2,
                        plan_mode: pm,
                        skill_no_shell: false,
                        budget_remaining_usd: budget_left,
                        session_id: sid3,
                        history: turn_history,
                    })
                    .await;
                });
                app.api_task = Some(handle.abort_handle());
            } else {
                app.entries.push(ChatEntry::system(format!(
                    "/{plugin}:{command} — plugin '{plugin}' is not active.\n\
                     Install it with: /plugin marketplace add <user/{plugin}>\n\
                     Then restart oxideclaw to activate it."
                )));
                app.scroll_to_bottom();
            }
        }
        CommandAction::IndexProject { force } => {
            let target = match crate::rag::IndexTarget::for_cwd(&config.cwd, false) {
                Ok(t) => t,
                Err(why) => {
                    app.entries.push(ChatEntry::system(format!(
                        "Not indexing {}: {why}.",
                        config.cwd.display()
                    )));
                    app.scroll_to_bottom();
                    return Ok(());
                }
            };
            let label = if force {
                "Full re-index"
            } else {
                "Incremental index"
            };
            app.entries
                .push(ChatEntry::system(format!("{label} — indexing codebase …")));
            // No spinner: indexing runs in the background and reports back
            // with a SystemMessage, which never cleared `is_loading`, so the
            // spinner ran forever and swallowed typing.
            app.scroll_to_bottom();
            let tx2 = tx.clone();
            tokio::spawn(async move {
                let result = tokio::task::spawn_blocking(move || {
                    let db = target.open()?;
                    target.index(&db, force)
                })
                .await
                .unwrap_or_else(|e| Err(anyhow::anyhow!("Index task panicked: {e}")));
                match result {
                    Ok(r) => {
                        let msg = format!(
                            "Index complete — {} files scanned, {} indexed, {} skipped, {} chunks. {:.0}ms",
                            r.files_scanned,
                            r.files_indexed,
                            r.files_skipped,
                            r.chunks_added,
                            r.elapsed_ms
                        );
                        let _ = tx2.send(crate::tui::events::AppEvent::SystemMessage(msg));
                    }
                    Err(e) => {
                        let _ = tx2.send(crate::tui::events::AppEvent::SystemMessage(format!(
                            "Index error: {e}"
                        )));
                    }
                }
            });
        }
        CommandAction::RagSearch(query) => {
            match existing_index(&config.cwd) {
                Ok((target, db)) => {
                    match crate::rag::search::search(&db, &query, 10) {
                        Ok(results) if results.is_empty() => {
                            app.entries.push(ChatEntry::system(format!(
                                "No results for '{query}'. Run /index to refresh the index."
                            )));
                        }
                        Ok(mut results) => {
                            target.localize(&mut results);
                            let mut lines = vec![format!(
                                "RAG search: '{}' — {} results\n",
                                query,
                                results.len()
                            )];
                            for r in &results {
                                lines.push(format!(
                                    "  {}:{}-{} ({} `{}`, {})",
                                    r.file_path,
                                    r.start_line,
                                    r.end_line,
                                    r.symbol_kind,
                                    r.symbol_name,
                                    r.language,
                                ));
                            }
                            // Show full context of first 3 results
                            let ctx = crate::rag::search::build_context(
                                &results[..results.len().min(3)],
                                8000,
                            );
                            if !ctx.is_empty() {
                                lines.push(String::new());
                                lines.push(ctx);
                            }
                            app.entries.push(ChatEntry::system(lines.join("\n")));
                        }
                        Err(e) => {
                            app.entries
                                .push(ChatEntry::system(format!("RAG search error: {e}")));
                        }
                    }
                }
                Err(msg) => app.entries.push(ChatEntry::system(msg)),
            }
            app.scroll_to_bottom();
        }
        CommandAction::RagStatus => {
            match existing_index(&config.cwd) {
                Ok((target, db)) => {
                    let chunks = db.chunk_count().unwrap_or(0);
                    let files = db.file_count().unwrap_or(0);
                    let size_bytes = db.db_size();
                    let size = if size_bytes > 1_048_576 {
                        format!("{:.1} MB", size_bytes as f64 / 1_048_576.0)
                    } else {
                        format!("{:.0} KB", size_bytes as f64 / 1024.0)
                    };
                    // Get language breakdown
                    let lang_breakdown = db.conn.prepare(
                        "SELECT language, COUNT(*) FROM code_chunks GROUP BY language ORDER BY COUNT(*) DESC"
                    ).ok().map(|mut stmt| {
                        stmt.query_map([], |row| {
                            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
                        }).ok().map(|rows| {
                            rows.filter_map(|r| r.ok())
                                .map(|(lang, count)| format!("  {lang}: {count} chunks"))
                                .collect::<Vec<_>>()
                                .join("\n")
                        }).unwrap_or_default()
                    }).unwrap_or_default();

                    let mut text = format!(
                        "RAG Index Status\n\
                         ─────────────────\n\
                         Files indexed: {files}\n\
                         Code chunks:   {chunks}\n\
                         Database size: {size}\n\
                         Project:       {}\n\
                         DB path:       {}",
                        target.root.display(),
                        db.db_path.display()
                    );
                    if !lang_breakdown.is_empty() {
                        text.push_str(&format!("\n\nLanguages:\n{lang_breakdown}"));
                    }
                    app.entries.push(ChatEntry::system(text));
                }
                Err(msg) => app.entries.push(ChatEntry::system(msg)),
            }
            app.scroll_to_bottom();
        }
        CommandAction::RagClear => {
            match existing_index(&config.cwd) {
                Ok((_, db)) => {
                    let old_chunks = db.chunk_count().unwrap_or(0);
                    match db.clear() {
                        Ok(()) => {
                            app.entries.push(ChatEntry::system(
                                format!("RAG index cleared — {old_chunks} chunks deleted.\nRun /index or /rag rebuild to re-index.")
                            ));
                        }
                        Err(e) => {
                            app.entries
                                .push(ChatEntry::system(format!("Failed to clear RAG index: {e}")));
                        }
                    }
                }
                Err(msg) => app.entries.push(ChatEntry::system(msg)),
            }
            app.scroll_to_bottom();
        }
        // ── Persistent memory commands ───────────────────────────────────
        CommandAction::MemoryAdd(text) => {
            let cwd = config.cwd.clone();
            match crate::memory::MemoryStore::open(&cwd) {
                Ok(store) => {
                    let cat = crate::memory::auto_categorize(&text);
                    match store.add_auto(&text, "user") {
                        Ok(true) => app
                            .entries
                            .push(ChatEntry::system(format!("Memory stored [{cat}]: {text}"))),
                        Ok(false) => app.entries.push(ChatEntry::system(
                            "Memory skipped — near-duplicate already exists.".to_string(),
                        )),
                        Err(e) => app
                            .entries
                            .push(ChatEntry::system(format!("Memory store error: {e}"))),
                    }
                }
                Err(e) => app
                    .entries
                    .push(ChatEntry::system(format!("Memory store error: {e}"))),
            }
            app.scroll_to_bottom();
        }
        CommandAction::MemoryForget(query) => {
            let cwd = config.cwd.clone();
            match crate::memory::MemoryStore::open(&cwd) {
                Ok(store) => match store.forget_matching(&query) {
                    Ok(removed) if removed.is_empty() => {
                        let mut msg = format!("No memories matched every word of '{query}'.");
                        // Near misses, so the user can forget one by its key.
                        if let Ok(near) = store.search(&query, 5)
                            && !near.is_empty()
                        {
                            msg.push_str(" Closest (/forget <key>):");
                            for m in near {
                                msg.push_str(&format!(
                                    "\n  [{}] {} ({})",
                                    m.category, m.value, m.key
                                ));
                            }
                        }
                        app.entries.push(ChatEntry::system(msg));
                    }
                    Ok(removed) => {
                        let mut msg = format!(
                            "Forgot {} memor{}:",
                            removed.len(),
                            if removed.len() == 1 { "y" } else { "ies" }
                        );
                        for m in removed {
                            msg.push_str(&format!("\n  [{}] {} ({})", m.category, m.value, m.key));
                        }
                        app.entries.push(ChatEntry::system(msg));
                    }
                    Err(e) => app
                        .entries
                        .push(ChatEntry::system(format!("Memory forget error: {e}"))),
                },
                Err(e) => app
                    .entries
                    .push(ChatEntry::system(format!("Memory store error: {e}"))),
            }
            app.scroll_to_bottom();
        }
        CommandAction::MemorySearch(query) => {
            let cwd = config.cwd.clone();
            match crate::memory::MemoryStore::open(&cwd) {
                Ok(store) => match store.search(&query, 10) {
                    Ok(results) if results.is_empty() => {
                        app.entries.push(ChatEntry::system(format!(
                            "No memories found for '{query}'."
                        )));
                    }
                    Ok(results) => {
                        let mut lines = vec![format!(
                            "Memory search: '{}' — {} results\n",
                            query,
                            results.len()
                        )];
                        for r in &results {
                            lines.push(format!("  [{}] {} ({})", r.category, r.value, r.key));
                        }
                        app.entries.push(ChatEntry::system(lines.join("\n")));
                    }
                    Err(e) => app
                        .entries
                        .push(ChatEntry::system(format!("Memory search error: {e}"))),
                },
                Err(e) => app
                    .entries
                    .push(ChatEntry::system(format!("Memory store error: {e}"))),
            }
            app.scroll_to_bottom();
        }
        CommandAction::MemoryList => {
            let cwd = config.cwd.clone();
            match crate::memory::MemoryStore::open(&cwd) {
                Ok(store) => match store.list(None) {
                    Ok(memories) if memories.is_empty() => {
                        app.entries.push(ChatEntry::system(
                            "No memories stored yet. Use /remember <text> to add one.".to_string(),
                        ));
                    }
                    Ok(memories) => {
                        let mut lines = vec![format!("Memories ({} total)\n", memories.len())];
                        let mut last_cat = String::new();
                        for m in &memories {
                            let cat = m.category.as_str().to_string();
                            if cat != last_cat {
                                lines.push(format!("\n[{cat}]"));
                                last_cat = cat;
                            }
                            lines.push(format!("  • {}", m.value));
                        }
                        app.entries.push(ChatEntry::system(lines.join("\n")));
                    }
                    Err(e) => app
                        .entries
                        .push(ChatEntry::system(format!("Memory list error: {e}"))),
                },
                Err(e) => app
                    .entries
                    .push(ChatEntry::system(format!("Memory store error: {e}"))),
            }
            app.scroll_to_bottom();
        }
        CommandAction::MemoryClear => {
            let cwd = config.cwd.clone();
            match crate::memory::MemoryStore::open(&cwd) {
                Ok(store) => match store.clear_all() {
                    Ok(()) => app
                        .entries
                        .push(ChatEntry::system("All memories cleared.".to_string())),
                    Err(e) => app
                        .entries
                        .push(ChatEntry::system(format!("Memory clear error: {e}"))),
                },
                Err(e) => app
                    .entries
                    .push(ChatEntry::system(format!("Memory store error: {e}"))),
            }
            app.scroll_to_bottom();
        }
        CommandAction::MemoryAutoToggle(on) => {
            config.memory_auto_capture = on;
            app.entries.push(ChatEntry::system(format!(
                "Memory auto-capture {}.",
                if on {
                    "enabled — notable decisions will be stored automatically"
                } else {
                    "disabled"
                }
            )));
            app.scroll_to_bottom();
        }
        CommandAction::MemoryInject => {
            let cwd = config.cwd.clone();
            match crate::memory::MemoryStore::open(&cwd) {
                Ok(store) => match store.build_context(10) {
                    Ok(ctx_text) if ctx_text.is_empty() => {
                        app.entries.push(ChatEntry::system(
                            "No memories to inject — store is empty.".to_string(),
                        ));
                    }
                    Ok(ctx_text) => {
                        app.entries.push(ChatEntry::system(format!(
                            "Current memory context:\n\n{ctx_text}"
                        )));
                    }
                    Err(e) => app
                        .entries
                        .push(ChatEntry::system(format!("Memory context error: {e}"))),
                },
                Err(e) => app
                    .entries
                    .push(ChatEntry::system(format!("Memory store error: {e}"))),
            }
            app.scroll_to_bottom();
        }
        CommandAction::SetBudget(amount) => {
            match amount {
                None => {
                    // Show current budget
                    let text = app.cost_tracker.summary();
                    app.entries.push(ChatEntry::system(text));
                }
                Some(v) if v < 0.0 => {
                    // Clear budget
                    app.cost_tracker.clear_budget();
                    app.entries
                        .push(ChatEntry::system("Budget limit removed.".to_string()));
                }
                Some(v) => {
                    app.cost_tracker.set_budget(v);
                    app.entries.push(ChatEntry::system(format!(
                        "Budget set to ${:.2}. Use /budget to check status.",
                        v
                    )));
                }
            }
            app.scroll_to_bottom();
        }
        CommandAction::RouterSet(enabled) => {
            app.router.enabled = enabled;
            let state = if app.router.enabled { "ON" } else { "OFF" };
            app.entries.push(ChatEntry::system(format!(
                "Smart model router: {state}\n\
                         Low → {}\n\
                         Medium → {}\n\
                         High → {}\n\
                         Super-High → {}",
                app.router.low_model,
                app.router.medium_model,
                app.router.high_model,
                app.router.super_high_model,
            )));
            app.scroll_to_bottom();
        }
        CommandAction::RouterStatus => {
            let state = if app.router.enabled { "ON" } else { "OFF" };
            let savings = app.cost_tracker.routing_savings(&app.router.high_model);
            let mut text = format!(
                "Smart Model Router: {state}\n\n\
                 Tier assignments:\n  \
                 Low complexity       → {}\n  \
                 Medium complexity    → {}\n  \
                 High complexity      → {}\n  \
                 Super-High (1M ctx)  → {}\n",
                app.router.low_model,
                app.router.medium_model,
                app.router.high_model,
                app.router.super_high_model,
            );
            if savings > 0.001 {
                text.push_str(&format!(
                    "\nEstimated savings from routing: ${:.4}",
                    savings
                ));
            }
            text.push_str(&format!("\n\n{}", app.cost_tracker.summary()));
            app.entries.push(ChatEntry::system(text));
            app.scroll_to_bottom();
        }
        CommandAction::RouterSetTier { tier, model } => {
            match tier.as_str() {
                "low" => app.router.low_model = model.clone(),
                "medium" => app.router.medium_model = model.clone(),
                "high" => app.router.high_model = model.clone(),
                "super-high" => app.router.super_high_model = model.clone(),
                _ => {}
            }
            app.entries.push(ChatEntry::system(format!(
                "Router {tier} tier set to: {model}"
            )));
            app.scroll_to_bottom();
        }
        CommandAction::SetAutonomy(mode) => {
            use crate::permissions::{Autonomy, autonomy::full_auto_blocker};
            let blocker = (mode == Autonomy::FullAuto)
                .then(|| full_auto_blocker(config.sandbox_enabled, &config.sandbox_mode))
                .flatten();
            let msg = match blocker {
                Some(why) => format!("{why}\nAutonomy stays {}.", config.autonomy),
                None => {
                    config.autonomy = mode;
                    match crate::permissions::autonomy::home_notice(mode, &config.cwd) {
                        Some(why) => format!("Autonomy set to: {mode}\n{why}"),
                        None => format!("Autonomy set to: {mode}"),
                    }
                }
            };
            app.entries.push(ChatEntry::system(msg));
            app.scroll_to_bottom();
        }
        CommandAction::GitCheckpoint(msg) => {
            let cwd = config.cwd.clone();
            let result =
                tokio::task::spawn_blocking(move || git_checkpoint(&cwd, msg.as_deref())).await;
            match result {
                Ok(Ok(summary)) => {
                    app.entries.push(ChatEntry::system(summary));
                }
                Ok(Err(e)) => {
                    app.entries
                        .push(ChatEntry::system(format!("Checkpoint failed: {e}")));
                }
                Err(e) => {
                    app.entries
                        .push(ChatEntry::system(format!("Checkpoint task error: {e}")));
                }
            }
            app.scroll_to_bottom();
        }
        CommandAction::SpawnAgent(task) => {
            if app.cost_tracker.over_budget() {
                app.entries.push(ChatEntry::system(format!(
                    "Budget exceeded (${:.4}) — not spawning. Use /budget to raise or clear the limit.",
                    app.cost_tracker.total_cost_usd
                )));
                app.scroll_to_bottom();
                return Ok(());
            }
            let tx2 = tx.clone();
            let cfg = config.clone();
            let reg = spawn_registry.clone();
            let task2 = task.clone();
            let budget_left = app.cost_tracker.remaining();
            tokio::spawn(async move {
                match crate::spawn::spawn_agent(task2.clone(), &cfg, &reg, tx2.clone(), budget_left)
                    .await
                {
                    Ok(id) => {
                        let _ = tx2.send(AppEvent::SystemMessage(
                            format!("Spawned agent [{id}]: {task2}\nRuns in the background with no approval prompts (settings deny rules still apply). Use /spawn list to check status."),
                        ));
                    }
                    Err(e) => {
                        let _ = tx2.send(AppEvent::SystemMessage(format!(
                            "Failed to spawn agent: {e:#}"
                        )));
                    }
                }
            });
            app.entries.push(ChatEntry::system(format!(
                "Spawning background agent: {task}"
            )));
        }
        CommandAction::ListSpawns => {
            let text = crate::spawn::list_agents(spawn_registry);
            app.entries.push(ChatEntry::system(text));
        }
        CommandAction::ReviewSpawn(id) => match crate::spawn::review_agent(spawn_registry, &id) {
            Ok(text) => app.entries.push(ChatEntry::system(text)),
            Err(e) => app.entries.push(ChatEntry::error(format!("{e:#}"))),
        },
        CommandAction::MergeSpawn(id) => {
            let reg = spawn_registry.clone();
            let cwd = config.cwd.clone();
            let tx2 = tx.clone();
            let id2 = id.clone();
            tokio::spawn(async move {
                match crate::spawn::merge_agent(&reg, &id2, &cwd).await {
                    Ok(msg) => {
                        let _ = tx2.send(AppEvent::SystemMessage(msg));
                    }
                    Err(e) => {
                        let _ = tx2.send(AppEvent::SystemMessage(format!("Merge failed: {e:#}")));
                    }
                }
            });
            app.entries
                .push(ChatEntry::system(format!("Merging agent [{id}]...")));
        }
        CommandAction::KillSpawn(id) => match crate::spawn::kill_agent(spawn_registry, &id) {
            Ok(msg) => app.entries.push(ChatEntry::system(msg)),
            Err(e) => app.entries.push(ChatEntry::error(format!("{e:#}"))),
        },
        CommandAction::VoiceClone(tier_arg) => {
            if tier_arg.is_empty() {
                // Show tier picker
                let text = format!(
                    "Voice Clone — choose a quality tier:\n\n\
                     1. quick        — {}\n\
                     2. recommended  — {}\n\
                     3. premium      — {}\n\n\
                     Usage: /voice clone <tier>\n\
                     Example: /voice clone recommended",
                    crate::voice::CloneTier::Quick.description(),
                    crate::voice::CloneTier::Recommended.description(),
                    crate::voice::CloneTier::Premium.description(),
                );
                app.entries.push(ChatEntry::system(text));
            } else if let Some(tier) = crate::voice::CloneTier::parse(&tier_arg) {
                // Show recording instructions for the chosen tier
                let instructions = crate::voice::recording_instructions(tier);
                app.entries.push(ChatEntry::system(format!(
                    "{instructions}\n\nUntil then, Ctrl+R records the clone sample, not \
                     dictation. Esc cancels clone mode."
                )));
                app.pending_clone_tier = Some(tier);
                // Enable voice mode so Ctrl+R works
                config.voice_enabled = true;
            } else {
                app.entries.push(ChatEntry::error(format!(
                    "Unknown tier '{tier_arg}'. Use: quick, recommended, or premium"
                )));
            }
        }
        CommandAction::VoiceCloneSave(tier_arg) => {
            let tier = crate::voice::CloneTier::parse(&tier_arg)
                .or(app.pending_clone_tier)
                .unwrap_or(crate::voice::CloneTier::Recommended);
            let tx2 = tx.clone();
            tokio::spawn(async move {
                match crate::voice::save_voice_clone(tier).await {
                    Ok(msg) => {
                        let _ = tx2.send(AppEvent::SystemMessage(msg));
                    }
                    Err(e) => {
                        let _ = tx2.send(AppEvent::SystemMessage(format!(
                            "Voice clone save failed: {e:#}"
                        )));
                    }
                }
            });
            app.entries.push(ChatEntry::system("Saving voice clone..."));
            app.pending_clone_tier = None;
        }
        CommandAction::VoiceTest => {
            // Cancel any existing TTS
            if let Some(prev) = app.tts_stop_tx.take() {
                let _ = prev.send(());
            }
            let (stop_tx, stop_rx) = oneshot::channel::<()>();
            app.tts_stop_tx = Some(stop_tx);
            let tx2 = tx.clone();
            let xtts_ok = crate::voice::xtts_available();
            let server_up = crate::voice::xtts_server_running();
            let label = format!("XTTS v2 default ({})", crate::voice::XTTS_DEFAULT_SPEAKER);
            let time_hint = if server_up {
                "~1 second"
            } else if xtts_ok {
                "starting server ~10s, then ~1s"
            } else {
                "a few seconds"
            };
            let has_clone = crate::voice::voice_clone_sample_path().is_some_and(|p| p.exists());
            let clone_hint = if has_clone {
                " Custom voice active for responses. /voice clone to change it."
            } else {
                " /voice clone to record any voice — yours, a friend, anyone."
            };
            app.entries.push(ChatEntry::system(format!(
                "Synthesizing with {label}… ({time_hint}, Esc to cancel){clone_hint}"
            )));
            tokio::spawn(async move {
                // Auto-start server if XTTS v2 is available but server not running
                if crate::voice::xtts_available() && !crate::voice::xtts_server_running() {
                    let _ = crate::voice::ensure_xtts_server().await;
                }
                let test_text = "Hello, I am OxideClaw, your personal coding \
                    assistant. I can help you write, debug, and ship code faster \
                    than ever before.";
                match crate::voice::speak_default_only(test_text, stop_rx).await {
                    Ok(_) => {
                        let _ = tx2.send(AppEvent::SystemMessage("Voice test complete.".into()));
                    }
                    Err(e) => {
                        let _ =
                            tx2.send(AppEvent::SystemMessage(format!("Voice test failed: {e}")));
                    }
                }
            });
        }
        CommandAction::VoiceCloneRemove => {
            app.pending_clone_tier = None;
            let tx2 = tx.clone();
            tokio::spawn(async move {
                match crate::voice::remove_voice_clone().await {
                    Ok(msg) => {
                        let _ = tx2.send(AppEvent::SystemMessage(msg));
                    }
                    Err(e) => {
                        let _ = tx2.send(AppEvent::SystemMessage(format!("Failed: {e:#}")));
                    }
                }
            });
        }
        CommandAction::DiscardSpawn(id) => {
            let reg = spawn_registry.clone();
            let cwd = config.cwd.clone();
            let tx2 = tx.clone();
            let id2 = id.clone();
            tokio::spawn(async move {
                match crate::spawn::discard_agent(&reg, &id2, &cwd).await {
                    Ok(msg) => {
                        let _ = tx2.send(AppEvent::SystemMessage(msg));
                    }
                    Err(e) => {
                        let _ = tx2.send(AppEvent::SystemMessage(format!("Discard failed: {e:#}")));
                    }
                }
            });
            app.entries
                .push(ChatEntry::system(format!("Discarding agent [{id}]...")));
        }
        CommandAction::TrustProject { mode } => {
            use crate::commands::TrustMode;
            let global = crate::settings::Settings::load_global();
            let trusted = crate::settings::Settings::is_trusted(&global, &config.cwd);
            let canonical = config
                .cwd
                .canonicalize()
                .unwrap_or_else(|_| config.cwd.clone())
                .to_string_lossy()
                .into_owned();
            // An unparsable global file reads as an empty trust list: saying
            // "NOT trusted", or saving [this project] over the user's list
            // (a wrong-typed value still parses as JSON), would both be wrong.
            let save = |list: Vec<String>| {
                crate::config::Config::save_user_setting("trustedProjects", serde_json::json!(list))
            };
            let msg = match mode {
                _ if !global.load_errors.is_empty() => format!(
                    "Cannot check or change trust for {canonical}.\n{}",
                    crate::settings::load_errors_notice(&global.load_errors)
                ),
                TrustMode::Revoke => {
                    let mut list = global.trusted_projects.unwrap_or_default();
                    if !crate::settings::Settings::remove_trusted(&mut list, &config.cwd) {
                        format!("Project {canonical} is not trusted.")
                    } else {
                        match save(list) {
                            Ok(()) => format!(
                                "Revoked trust for {canonical}. Its settings hooks, \
                                 apiKeyHelper and MCP servers, and OLLAMA_HOST / \
                                 ANTHROPIC_MODEL from its .env, will be ignored — restart \
                                 oxideclaw to apply. Auto-fix stops running its lint and \
                                 test commands now."
                            ),
                            Err(e) => format!("Could not save trust: {e}"),
                        }
                    }
                }
                TrustMode::Grant if !trusted => {
                    let mut list = global.trusted_projects.unwrap_or_default();
                    list.push(canonical.clone());
                    match save(list) {
                        Ok(()) => format!(
                            "Trusted {canonical}. Its settings hooks, apiKeyHelper and MCP \
                             servers will be honoured, and so will OLLAMA_HOST / ANTHROPIC_MODEL \
                             from its .env — restart oxideclaw to apply. Auto-fix runs its \
                             lint and test commands from the next edit."
                        ),
                        Err(e) => format!("Could not save trust: {e}"),
                    }
                }
                TrustMode::Grant | TrustMode::Status => format!(
                    "Project {canonical} is {}.{}",
                    if trusted { "trusted" } else { "NOT trusted" },
                    if config.untrusted_project_config.is_empty() {
                        String::new()
                    } else {
                        format!(
                            "\nIgnored from its settings: {}. Run /trust to enable.",
                            config.untrusted_project_config.join(", ")
                        )
                    }
                ),
            };
            app.entries.push(ChatEntry::system(msg));
            // Auto-fix reads trust and its settings per edit, so a change
            // applies at once, including the project's own autoFixLoop block.
            config.refresh_trust();
        }
        CommandAction::AutoCommitStatus => {
            let cwd_ok = oxideclaw::autocommit::is_git_repo(&config.cwd);
            let msg = format!(
                "Auto-commit status:\n  \
                 Enabled:        {}\n  \
                 Repo:           {}\n  \
                 Session ID:     {}\n  \
                 Turns recorded: {}\n  \
                 Undo position:  {} / {}\n  \
                 Retention:      keep {} most recent sessions\n  \
                 Message prefix: \"{}\"",
                if config.auto_commit.enabled {
                    "yes"
                } else {
                    "no"
                },
                if cwd_ok {
                    "git detected (cwd)"
                } else {
                    "not a git repo"
                },
                session.id,
                session.meta.auto_commits.len(),
                session.meta.undo_position,
                session.meta.auto_commits.len(),
                config.auto_commit.keep_sessions,
                config.auto_commit.message_prefix,
            );
            app.entries.push(ChatEntry::system(msg));
        }
        CommandAction::Browse {
            goal,
            policy,
            max_steps,
        } => {
            // Pattern is the parser's "no flag given" sentinel — substitute
            // the user's configured default unless they explicitly chose a policy.
            let policy = if matches!(policy, crate::browser::browse_loop::BrowsePolicy::Pattern) {
                crate::browser::browse_loop::BrowsePolicy::from_settings_str(
                    &config.browse_default_policy,
                )
            } else {
                policy
            };
            let max = max_steps.unwrap_or(config.browse_max_steps);
            app.entries.push(ChatEntry::system(format!(
                "🌐 /browse started — goal: {goal} (max {max} steps, policy: {policy:?})"
            )));
            app.scroll_to_bottom();
            app.start_loading();
            begin_agent_turn(session, config).await;

            // Create channels for progress events and approval prompts
            let (progress_tx, progress_rx) = tokio::sync::mpsc::channel(64);
            let (approval_tx, approval_rx) = tokio::sync::mpsc::channel(4);
            app.browse_progress_rx = Some(progress_rx);
            app.browse_approval_rx = Some(approval_rx);

            // Shared current-URL state
            let current_url = std::sync::Arc::new(tokio::sync::Mutex::new(String::new()));

            let mut cfg = config.clone();
            if let Some(left) = app.cost_tracker.remaining() {
                cfg.max_budget_usd = Some(left);
            }
            let all_tools = tools.to_vec();
            let browser_session = app.browser_session.clone();
            let usage_sink = Some(crate::tui::events::forward_usage(tx.clone()));
            let err_tx = tx.clone();

            let browse_req = crate::browser::browse_loop::BrowseRequest {
                goal,
                policy,
                max_steps: max,
                voice: false,
            };
            let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            app.browse_cancel = Some(cancel.clone());
            tokio::spawn(async move {
                let channels = crate::browser::browse_loop::BrowseChannels {
                    progress_tx,
                    approval_tx,
                    cancel,
                    usage_sink,
                };
                let result = crate::browser::browse_loop::run_browse(
                    browse_req,
                    &cfg,
                    all_tools,
                    current_url,
                    browser_session,
                    channels,
                )
                .await;
                // stderr would be drawn over the inline viewport.
                if let Err(e) = result {
                    let _ = err_tx.send(AppEvent::SystemMessage(format!("⚠ /browse error: {e:#}")));
                }
            });
        }
        CommandAction::BrowseUrl(url) => {
            // Ship a prompt to the agent loop — it will invoke the shared
            // browser_navigate / browser_snapshot tools (which drive the same
            // Chrome instance as /browser and /screenshot).
            let prompt = if url == "about:blank" {
                "Launch the browser (use the browser_navigate tool with url=about:blank) and take an accessibility snapshot. Tell me it's ready.".to_string()
            } else {
                format!(
                    "Navigate the browser to {url} and take an accessibility snapshot. Describe what you see on the page."
                )
            };
            app.entries.push(ChatEntry::user(input.clone()));
            app.scroll_to_bottom();
            app.start_loading();
            begin_agent_turn(session, config).await;
            push_prompt_turn(messages, vec![ContentBlock::Text { text: prompt }], session).await;
            let snapshot = messages.clone();
            let c2 = client.clone();
            let tvec = tools.to_vec();
            let cfg = config.clone();
            let tx2 = tx.clone();
            let sp = system_prompt.clone();
            let ps = perm_state.clone();
            let pm = app.plan_mode;
            let budget_left = app.cost_tracker.remaining();
            let sid3 = session.id.clone();
            let turn_history = TurnHistory::default();
            app.turn_history = Some(turn_history.clone());
            let handle = tokio::spawn(async move {
                run_api_task(ApiTask {
                    client: c2,
                    tools: tvec,
                    messages: snapshot,
                    config: cfg,
                    perm_state: ps,
                    system_prompt: sp,
                    tx: tx2,
                    plan_mode: pm,
                    skill_no_shell: false,
                    budget_remaining_usd: budget_left,
                    session_id: sid3,
                    history: turn_history,
                })
                .await;
            });
            app.api_task = Some(handle.abort_handle());
        }
        CommandAction::BrowserScreenshot => {
            let prompt = "Take a screenshot of the current browser page (use the browser_screenshot tool) and tell me what is visible.".to_string();
            app.entries.push(ChatEntry::user(input.clone()));
            app.scroll_to_bottom();
            app.start_loading();
            begin_agent_turn(session, config).await;
            push_prompt_turn(messages, vec![ContentBlock::Text { text: prompt }], session).await;
            let snapshot = messages.clone();
            let c2 = client.clone();
            let tvec = tools.to_vec();
            let cfg = config.clone();
            let tx2 = tx.clone();
            let sp = system_prompt.clone();
            let ps = perm_state.clone();
            let pm = app.plan_mode;
            let budget_left = app.cost_tracker.remaining();
            let sid3 = session.id.clone();
            let turn_history = TurnHistory::default();
            app.turn_history = Some(turn_history.clone());
            let handle = tokio::spawn(async move {
                run_api_task(ApiTask {
                    client: c2,
                    tools: tvec,
                    messages: snapshot,
                    config: cfg,
                    perm_state: ps,
                    system_prompt: sp,
                    tx: tx2,
                    plan_mode: pm,
                    skill_no_shell: false,
                    budget_remaining_usd: budget_left,
                    session_id: sid3,
                    history: turn_history,
                })
                .await;
            });
            app.api_task = Some(handle.abort_handle());
        }
        CommandAction::BrowserClose => {
            if let Some(arc) = &app.browser_session {
                let mut session = arc.lock().await;
                session.close().await;
                app.entries
                    .push(ChatEntry::system("Browser session closed.".to_string()));
            } else {
                app.entries.push(ChatEntry::system(
                    "Browser not enabled — set browserEnabled=true in settings.json.".to_string(),
                ));
            }
            app.scroll_to_bottom();
        }
        CommandAction::Watch(arg) => {
            match arg.as_deref() {
                Some("off") | Some("stop") => {
                    if app.watcher.take().is_some() {
                        app.entries.push(ChatEntry::system("Watch mode stopped."));
                    } else {
                        app.entries
                            .push(ChatEntry::system("Watch mode was not active."));
                    }
                }
                Some("status") => {
                    let msg = if app.watcher.is_some() {
                        "Watch mode: active"
                    } else {
                        "Watch mode: inactive"
                    };
                    app.entries.push(ChatEntry::system(msg));
                }
                arg_opt => {
                    // Already running? Tell the user instead of double-starting.
                    if app.watcher.is_some() {
                        app.entries.push(ChatEntry::system(
                            "Watch mode already active. Use `/watch stop` first.",
                        ));
                    } else {
                        // Containment check: a user-provided path must resolve
                        // inside cwd. Prevents `/watch /etc` or
                        // `/watch ../../..` from spinning up a watcher over
                        // arbitrary filesystem regions. Canonicalize both
                        // sides so symlink trickery can't escape cwd either.
                        let watch_path_result: Result<std::path::PathBuf, String> = match arg_opt {
                            None => Ok(config.cwd.clone()),
                            Some(p) => {
                                let candidate = std::path::PathBuf::from(p);
                                let absolute = if candidate.is_absolute() {
                                    candidate
                                } else {
                                    config.cwd.join(candidate)
                                };
                                match (absolute.canonicalize(), config.cwd.canonicalize()) {
                                    (Ok(canon), Ok(cwd_canon)) if canon.starts_with(&cwd_canon) => {
                                        Ok(canon)
                                    }
                                    (Ok(canon), Ok(cwd_canon)) => Err(format!(
                                        "Watch path {} is outside cwd {}.",
                                        canon.display(),
                                        cwd_canon.display()
                                    )),
                                    _ => Err(format!(
                                        "Watch path not accessible: {}",
                                        absolute.display()
                                    )),
                                }
                            }
                        };
                        let watch_path = match watch_path_result {
                            Ok(p) => p,
                            Err(msg) => {
                                app.entries.push(ChatEntry::error(msg));
                                app.scroll_to_bottom();
                                return Ok(());
                            }
                        };
                        let cfg = crate::watch::WatchConfig {
                            paths: vec![watch_path.clone()],
                            patterns: vec![
                                "*.rs".into(),
                                "*.py".into(),
                                "*.ts".into(),
                                "*.js".into(),
                            ],
                            markers: config.watch_markers.clone(),
                            debounce_ms: config.watch_debounce_ms,
                            rate_limit_ms: config.watch_rate_limit_ms,
                        };
                        let (wtx, mut wrx) = mpsc::unbounded_channel();
                        match crate::watch::start_watcher(cfg, wtx) {
                            Ok(watcher) => {
                                // Forwarder: every watch event → SystemMessage in the TUI.
                                let tx2 = tx.clone();
                                let handle = tokio::spawn(async move {
                                    while let Some(ev) = wrx.recv().await {
                                        let msg = match ev {
                                            crate::watch::WatchEvent::FileChanged { path } => {
                                                format!("[watch] changed: {}", path.display())
                                            }
                                            crate::watch::WatchEvent::MarkerFound { marker } => {
                                                format!(
                                                    "[watch] {} at {}:{} — {}",
                                                    marker.kind,
                                                    marker.file.display(),
                                                    marker.line,
                                                    marker.text,
                                                )
                                            }
                                        };
                                        let _ = tx2.send(AppEvent::SystemMessage(msg));
                                    }
                                });
                                app.watcher = Some(crate::tui::app::WatcherHandle {
                                    forwarder: handle.abort_handle(),
                                    watcher,
                                });
                                app.entries.push(ChatEntry::system(format!(
                                    "Watching {} for {} markers.",
                                    watch_path.display(),
                                    config.watch_markers.join(", "),
                                )));
                            }
                            Err(e) => {
                                app.entries
                                    .push(ChatEntry::error(format!("Watch failed: {e}")));
                            }
                        }
                    }
                }
            }
            app.scroll_to_bottom();
        }
        CommandAction::ShowDiff(path) => {
            match uncommitted_diff(&config.cwd, path.as_deref()).await {
                Ok((diff_text, untracked)) => {
                    if diff_text.trim().is_empty() && untracked.is_empty() {
                        app.entries
                            .push(ChatEntry::system("No uncommitted changes."));
                    } else {
                        let files = crate::tui::diff::parse_unified_diff(&diff_text);
                        let summary: String = files
                            .iter()
                            .map(|f| format!("  {} (+{} -{})", f.path, f.additions, f.deletions))
                            .chain(untracked.iter().map(|f| format!("  {f} (new, untracked)")))
                            .collect::<Vec<_>>()
                            .join("\n");
                        app.overlay = Some(Overlay::new(
                            "diff",
                            format!("Diff Review\n\n{summary}\n\n{diff_text}"),
                        ));
                    }
                }
                Err(e) => {
                    app.entries
                        .push(ChatEntry::error(format!("git diff failed: {e}")));
                }
            }
            app.scroll_to_bottom();
        }
        CommandAction::Unknown(name) => {
            // Try skill expansion before giving up
            if let Some((skill_name, args)) = parse_skill_invocation(&input)
                && let Some(skill) = skills.get(skill_name)
            {
                let mut prompt = match skill.invoke(args) {
                    Ok(prompt) => prompt,
                    Err(why) => {
                        app.entries.push(ChatEntry::error(format!(
                            "Skill '{}' could not be loaded: {why}",
                            skill.name
                        )));
                        app.scroll_to_bottom();
                        return Ok(());
                    }
                };
                if config.disable_skill_shell_execution {
                    prompt.push_str("\n\nNote: shell command execution (Bash tool) is disabled for skill invocations.");
                }
                app.entries.push(ChatEntry::system(format!(
                    "Skill: {} — {}",
                    skill.name, skill.description
                )));
                app.entries.push(ChatEntry::user(input));
                app.scroll_to_bottom();
                app.start_loading();
                begin_agent_turn(session, config).await;
                push_prompt_turn(messages, vec![ContentBlock::Text { text: prompt }], session)
                    .await;
                let snapshot = messages.clone();
                let c2 = client.clone();
                let tvec = skill_turn_tools(tools, config.disable_skill_shell_execution);
                let cfg = config.clone();
                let tx2 = tx.clone();
                let sp = system_prompt.clone();
                let ps = perm_state.clone();
                let pm = app.plan_mode;
                let no_shell = config.disable_skill_shell_execution;
                let budget_left = app.cost_tracker.remaining();
                let sid4 = session.id.clone();
                let turn_history = TurnHistory::default();
                app.turn_history = Some(turn_history.clone());
                let handle = tokio::spawn(async move {
                    run_api_task(ApiTask {
                        client: c2,
                        tools: tvec,
                        messages: snapshot,
                        config: cfg,
                        perm_state: ps,
                        system_prompt: sp,
                        tx: tx2,
                        plan_mode: pm,
                        skill_no_shell: no_shell,
                        budget_remaining_usd: budget_left,
                        session_id: sid4,
                        history: turn_history,
                    })
                    .await;
                });
                app.api_task = Some(handle.abort_handle());
                return Ok(());
            }
            app.entries.push(ChatEntry::system(format!(
                "Unknown command '/{name}'. Type /help for available commands."
            )));
        }
    }
    Ok(())
}

/// /diff: everything a commit would pick up, not just unstaged edits. Bare
/// `git diff` hid staged changes and every file the agent created with
/// Write, then reported "No uncommitted changes." Returns the diff against
/// HEAD (the index, before the first commit) and the untracked files.
async fn uncommitted_diff(
    cwd: &std::path::Path,
    path: Option<&str>,
) -> anyhow::Result<(String, Vec<String>)> {
    // Separate args, never `sh -c` with a formatted path (shell injection).
    let git = |args: &[&str]| {
        let mut cmd = tokio::process::Command::new("git");
        cmd.args(args).current_dir(cwd);
        if let Some(p) = path {
            cmd.arg("--").arg(p);
        }
        cmd.output()
    };
    // --no-ext-diff: /diff wants a parseable unified diff, not whatever a
    // configured diff.external tool prints.
    let mut out = git(&["diff", "--no-ext-diff", "HEAD"]).await?;
    if !out.status.success() {
        // No HEAD yet: everything is staged against the empty tree.
        let cached = git(&["diff", "--no-ext-diff", "--cached"]).await?;
        if !cached.status.success() {
            anyhow::bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
        }
        out = cached;
    }
    let others = git(&[
        "ls-files",
        "-z",
        "--full-name",
        "--others",
        "--exclude-standard",
    ])
    .await?;
    let untracked = String::from_utf8_lossy(&others.stdout)
        .split('\0')
        .filter(|f| !f.is_empty())
        .map(str::to_string)
        .collect();
    Ok((String::from_utf8_lossy(&out.stdout).into_owned(), untracked))
}

/// The user turn for `/<plugin>:<command> [args]`. The model only sees this
/// prompt (the raw slash input stays local), so the user's arguments must be
/// carried in it or the tool gets called with guessed or empty parameters.
fn plugin_command_prompt(plugin: &str, command: &str, args: &str) -> String {
    let tool_name = command.replace('-', "_");
    let mut prompt = format!(
        "Run the `{plugin}` MCP tool `{tool_name}`. \
         If the exact name doesn't match, look for the closest \
         tool starting with `mcp__` that contains `{tool_name}`."
    );
    if !args.is_empty() {
        prompt.push_str(&format!(
            "\n\nCall it with these arguments from the user: {args}"
        ));
    }
    prompt
}

/// The editor for /edit-claude-md: the first non-blank $VISUAL or $EDITOR.
/// `VISUAL=""` used to win over a set $EDITOR and fail to launch.
fn editor_command(visual: Option<String>, editor: Option<String>) -> String {
    [visual, editor]
        .into_iter()
        .flatten()
        .map(|e| e.trim().to_string())
        .find(|e| !e.is_empty())
        .unwrap_or_else(|| if cfg!(windows) { "notepad" } else { "nano" }.to_string())
}

/// Remove `path` when this /memory run created it from `seed` and it still
/// holds exactly that: the editor never saved, and a frozen copy of
/// `~/.claude/CLAUDE.md` would shadow the original from then on. Edits a
/// user saved before the editor failed are kept.
fn discard_untouched_seed(path: &std::path::Path, seed: Option<&str>) {
    if let Some(seed) = seed
        && std::fs::read_to_string(path).ok().as_deref() == Some(seed)
    {
        let _ = std::fs::remove_file(path);
    }
}

/// Run `editor` on `path`. The variable is a command line, not a program
/// name (`code --wait`, `emacsclient -t`): spawned verbatim it failed with
/// ENOENT. Unix goes through the shell like git does; Windows splits on
/// whitespace unless the whole value names an existing file.
fn editor_process(editor: &str, path: &std::path::Path) -> tokio::process::Command {
    if cfg!(windows) {
        let (program, args): (&str, Vec<&str>) = if std::path::Path::new(editor).is_file() {
            (editor, vec![])
        } else {
            let mut words = editor.split_whitespace();
            (words.next().unwrap_or(editor), words.collect())
        };
        let mut cmd = tokio::process::Command::new(program);
        cmd.args(args).arg(path);
        cmd
    } else {
        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c")
            .arg(format!("{editor} \"$1\""))
            .arg("sh")
            .arg(path);
        cmd
    }
}

/// The connected server a `/plugin:command` names. Tab completion builds
/// the slug from the sanitized tool prefix with `_` turned into `-`, so a
/// server called `brave_search` (or `my.server`) is offered as
/// `brave-search:` and never matched its real name exactly.
fn plugin_server<'a>(
    statuses: &'a [crate::mcp::types::McpServerStatus],
    plugin: &str,
) -> Option<&'a crate::mcp::types::McpServerStatus> {
    let norm = |s: &str| -> String {
        s.chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect()
    };
    let want = norm(plugin);
    statuses
        .iter()
        .find(|s| s.name == plugin)
        .or_else(|| statuses.iter().find(|s| norm(&s.name) == want))
}

/// Commands that send a prompt to the model, mirroring the arms below that
/// spawn `run_api_task` or the browse loop. /budget is checked against these
/// before they run, as it is for typed messages.
fn starts_model_turn(
    action: &CommandAction,
    input: &str,
    skills: &std::collections::HashMap<String, crate::skills::Skill>,
    mcp_statuses: &[crate::mcp::types::McpServerStatus],
) -> bool {
    match action {
        CommandAction::SendPrompt(_)
        | CommandAction::BrowseUrl(_)
        | CommandAction::BrowserScreenshot
        | CommandAction::Browse { .. } => true,
        CommandAction::PluginCommand { plugin, .. } => {
            plugin_server(mcp_statuses, plugin).is_some()
        }
        CommandAction::Unknown(_) => {
            parse_skill_invocation(input).is_some_and(|(name, _)| skills.contains_key(name))
        }
        _ => false,
    }
}

/// The model /reload should switch to, if any. Only a settings.json model
/// that changed since it was last read counts: `--model` and
/// `ANTHROPIC_MODEL` outrank settings at startup, and a reload made to pick
/// up a CLAUDE.md edit used to revert them silently.
fn reloaded_model(
    settings_model: Option<&str>,
    last_seen: &mut Option<String>,
    current: &str,
) -> Option<String> {
    let resolved = settings_model.map(crate::commands::resolve_model_alias);
    if resolved == *last_seen {
        return None;
    }
    last_seen.clone_from(&resolved);
    resolved.filter(|m| m != current)
}

/// The code index `/rag search`, `/rag status` and `/rag clear` read, opened
/// only if it exists: none of them may create one (in `$HOME` or `/` they
/// would leave an empty database behind). `Err` is the message to show.
fn existing_index(
    cwd: &std::path::Path,
) -> std::result::Result<(crate::rag::IndexTarget, crate::rag::RagDb), String> {
    let target = crate::rag::IndexTarget::for_cwd(cwd, false)
        .map_err(|why| format!("No code index for {}: {why}.", cwd.display()))?;
    match target.open_existing() {
        Ok(Some(db)) => Ok((target, db)),
        Ok(None) => Err(target.missing_message()),
        Err(e) => Err(format!("RAG database error: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabling_skill_shell_execution_removes_shell_capable_tools() {
        let all = crate::tools::all_tools_with_state(&crate::config::Config::default()).0;
        let names = |v: &[DynTool]| v.iter().map(|t| t.name().to_string()).collect::<Vec<_>>();

        let open = names(&skill_turn_tools(&all, false));
        assert_eq!(open, names(&all), "flag off must not change the list");
        assert!(open.iter().any(|n| n == "Bash"));

        let closed = names(&skill_turn_tools(&all, true));
        for n in ["Bash", "PowerShell", "Agent"] {
            assert!(
                !closed.iter().any(|c| c == n),
                "{n} still offered: {closed:?}"
            );
        }
        assert!(closed.iter().any(|n| n == "Read"), "non-shell tools stay");
    }
}

#[cfg(test)]
mod editor_tests {
    use super::*;

    #[test]
    fn an_untouched_seed_is_removed_and_an_edited_one_kept() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("CLAUDE.md");
        std::fs::write(&path, "seed").unwrap();
        discard_untouched_seed(&path, Some("seed"));
        assert!(
            !path.exists(),
            "an untouched seed must not shadow ~/.claude"
        );

        std::fs::write(&path, "seed plus the user's edit").unwrap();
        discard_untouched_seed(&path, Some("seed"));
        assert!(path.exists(), "saved edits are kept");

        // A file that existed before this run is never removed.
        std::fs::write(&path, "seed").unwrap();
        discard_untouched_seed(&path, None);
        assert!(path.exists());
    }

    #[test]
    fn blank_visual_falls_through_to_editor() {
        let some = |s: &str| Some(s.to_string());
        assert_eq!(editor_command(some(""), some("vim")), "vim");
        assert_eq!(editor_command(some("  "), None), editor_command(None, None));
        assert_eq!(
            editor_command(some("code --wait"), some("vim")),
            "code --wait"
        );
        assert_eq!(editor_command(None, some(" hx ")), "hx");
    }

    /// `EDITOR="code --wait"` was spawned as a program named `code --wait`.
    #[cfg(unix)]
    #[tokio::test]
    async fn editor_value_with_arguments_runs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("my CLAUDE.md");
        std::fs::write(&path, "old").unwrap();
        let status = editor_process("printf '%s' edited >", &path)
            .status()
            .await
            .unwrap();
        assert!(status.success());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "edited");

        let missing = editor_process("definitely-not-an-editor-xyz", &path)
            .stderr(std::process::Stdio::null())
            .status()
            .await
            .unwrap();
        assert_eq!(missing.code(), Some(127));
    }
}

#[cfg(test)]
mod reload_tests {
    use super::reloaded_model;

    /// Started with `--model ollama:qwen` while settings say a Claude model:
    /// a reload that leaves settings.json's model alone keeps the CLI model
    /// (switching it also left the Ollama client serving a Claude model).
    #[test]
    fn unchanged_settings_model_keeps_a_cli_or_env_model() {
        let mut seen = Some("claude-sonnet-4-6".to_string());
        assert_eq!(
            reloaded_model(Some("claude-sonnet-4-6"), &mut seen, "ollama:qwen"),
            None
        );
        assert_eq!(reloaded_model(None, &mut None, "ollama:qwen"), None);
    }

    /// Editing settings.json's model is what /reload applies, once.
    #[test]
    fn edited_settings_model_is_applied() {
        let mut seen = Some("claude-sonnet-4-6".to_string());
        assert_eq!(
            reloaded_model(Some("ollama:llama3"), &mut seen, "claude-sonnet-4-6"),
            Some("ollama:llama3".to_string())
        );
        assert_eq!(seen.as_deref(), Some("ollama:llama3"));
        assert_eq!(
            reloaded_model(Some("ollama:llama3"), &mut seen, "ollama:llama3"),
            None
        );
    }
}

#[cfg(test)]
mod budget_tests {
    use super::*;

    /// /summary, skills, plugin commands and the browser commands went
    /// straight to the model after /budget was spent; only typed messages
    /// were refused.
    #[test]
    fn spent_budget_blocks_prompt_sending_slash_commands() {
        let mut skills = std::collections::HashMap::new();
        skills.insert(
            "deploy".to_string(),
            crate::skills::Skill {
                name: "deploy".into(),
                description: String::new(),
                prompt_template: "deploy {{ARGS}}".into(),
                category: None,
                params: Vec::new(),
                skill_file: None,
            },
        );
        let statuses = [crate::mcp::types::McpServerStatus {
            name: "ctx".into(),
            transport: "stdio",
            protocol: "2026-07-28".into(),
            tool_count: 1,
        }];
        let turn = |a: CommandAction, input: &str| starts_model_turn(&a, input, &skills, &statuses);

        assert!(turn(dispatch_action("/summary"), "/summary"));
        assert!(turn(CommandAction::BrowserScreenshot, "/screenshot"));
        assert!(turn(CommandAction::Unknown("deploy".into()), "/deploy now"));
        assert!(turn(
            CommandAction::PluginCommand {
                plugin: "ctx".into(),
                command: "doctor".into(),
                args: String::new(),
            },
            "/ctx:doctor"
        ));
        assert!(!turn(
            CommandAction::PluginCommand {
                plugin: "gone".into(),
                command: "doctor".into(),
                args: String::new(),
            },
            "/gone:doctor"
        ));
        assert!(!turn(CommandAction::Unknown("nope".into()), "/nope"));
        assert!(!turn(CommandAction::ShowDiff(None), "/diff"));

        let mut app = App::new("claude-sonnet-4-6", std::path::Path::new("/tmp"));
        assert!(!budget_blocks(&mut app, "/summary"));
        app.cost_tracker.set_budget(1.0);
        app.cost_tracker.total_cost_usd = 1.5;
        assert!(budget_blocks(&mut app, "/summary"));
        assert_eq!(app.input.iter().collect::<String>(), "/summary");
        assert!(app.entries.last().unwrap().text.contains("not sending"));
    }

    /// Tab completion offers `brave_search` as `/brave-search:...`, which
    /// then reported the connected server as "not active".
    #[test]
    fn completed_plugin_slug_finds_server_with_underscores() {
        let status = |name: &str| crate::mcp::types::McpServerStatus {
            name: name.into(),
            transport: "stdio",
            protocol: "2026-07-28".into(),
            tool_count: 1,
        };
        let statuses = [status("brave_search"), status("my.server"), status("ctx")];
        let found = |p: &str| plugin_server(&statuses, p).map(|s| s.name.as_str());
        assert_eq!(found("brave-search"), Some("brave_search"));
        assert_eq!(found("brave_search"), Some("brave_search"));
        assert_eq!(found("my-server"), Some("my.server"));
        assert_eq!(found("ctx"), Some("ctx"));
        assert_eq!(found("gone"), None);

        let skills = std::collections::HashMap::new();
        let action = dispatch_action("/brave-search:brave-web-search rust");
        assert!(starts_model_turn(
            &action,
            "/brave-search:brave-web-search rust",
            &skills,
            &statuses
        ));
    }

    /// `/github:search_issues label:bug is:open` used to drop everything
    /// after the command name; the model never saw the filter.
    #[test]
    fn plugin_command_arguments_reach_the_model() {
        match dispatch_action("/github:search-issues label:bug is:open") {
            CommandAction::PluginCommand {
                plugin,
                command,
                args,
            } => {
                assert_eq!(plugin, "github");
                assert_eq!(command, "search-issues");
                assert_eq!(args, "label:bug is:open");
                let prompt = plugin_command_prompt(&plugin, &command, &args);
                assert!(prompt.contains("`search_issues`"), "{prompt}");
                assert!(prompt.contains("label:bug is:open"), "{prompt}");
            }
            _ => panic!("expected PluginCommand"),
        }
        let bare = plugin_command_prompt("ctx", "doctor", "");
        assert!(!bare.contains("arguments"), "{bare}");
    }

    fn dispatch_action(input: &str) -> CommandAction {
        let config = crate::config::Config::default();
        let skills = std::collections::HashMap::new();
        let todo = TodoState::default();
        let ctx = CommandContext {
            config: &config,
            tokens_in: 0,
            context_window: 0,
            tokens_out: 0,
            cache_read_tokens: 0,
            cost_summary: String::new(),
            cost_recorded: false,
            cache_write_tokens: 0,
            vim_mode: false,
            skills: &skills,
            todo_state: &todo,
            last_assistant: None,
            session_id: "s",
            session_name: "",
            claudemd: "",
            mcp_statuses: &[],
            brief_mode: false,
            btw_note: None,
        };
        dispatch(input, &ctx)
    }
}

#[cfg(test)]
mod diff_tests {
    use super::uncommitted_diff;

    fn git(dir: &std::path::Path, args: &[&str]) {
        let ok = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap()
            .status
            .success();
        assert!(ok, "git {args:?}");
    }

    /// Staged edits and new files used to give "No uncommitted changes."
    #[tokio::test]
    async fn diff_includes_staged_and_untracked_files() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        git(d, &["init", "-q"]);
        // The helper's own git calls lack the env isolation above.
        git(d, &["config", "core.fsmonitor", "false"]);
        // Repo config overrides a global core.excludesFile (and the XDG
        // default ignore file) that might match the untracked file.
        git(d, &["config", "core.excludesFile", "/dev/null"]);
        // An external diff tool must not replace the unified diff.
        git(d, &["config", "diff.external", "false"]);
        std::fs::write(d.join("staged.txt"), "one\n").unwrap();
        git(d, &["add", "staged.txt"]);

        // Before the first commit there is no HEAD to diff against.
        let (diff, untracked) = uncommitted_diff(d, None).await.unwrap();
        assert!(diff.contains("+one"), "{diff}");
        assert!(untracked.is_empty());

        git(
            d,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-q",
                "-m",
                "init",
            ],
        );
        std::fs::write(d.join("staged.txt"), "two\n").unwrap();
        git(d, &["add", "staged.txt"]);
        std::fs::write(d.join("new-untracked.oxideclaw-test"), "new\n").unwrap();

        let (diff, untracked) = uncommitted_diff(d, None).await.unwrap();
        assert!(diff.contains("+two"), "{diff}");
        assert_eq!(untracked, ["new-untracked.oxideclaw-test"]);

        let (diff, untracked) = uncommitted_diff(d, Some("new-untracked.oxideclaw-test"))
            .await
            .unwrap();
        assert!(diff.is_empty(), "{diff}");
        assert_eq!(untracked, ["new-untracked.oxideclaw-test"]);
    }
}
