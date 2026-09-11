//! Slash-command dispatch — the `CommandAction` match carved out of
//! `handle_key` mechanically. No behaviour change.

use super::*;

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
        turn_counter,
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

    match dispatch(&input, &ctx) {
        CommandAction::Quit => {
            crate::voice::stop_xtts_server();
            app.should_quit = true;
        }
        CommandAction::Clear => {
            messages.clear();
            messages.shrink_to_fit();
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
            } else {
                app.entries
                    .push(ChatEntry::system("Compacting conversation…"));
                snip_compact(messages);
                let c2 = client.clone();
                let msgs = messages.clone();
                let cfg = config.clone();
                let tx2 = tx.clone();
                tokio::spawn(async move {
                    match summarize_compact(&c2, &msgs, &cfg).await {
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
                            let _ = tx2.send(AppEvent::Compacted {
                                replacement: r,
                                summary_len,
                            });
                        }
                        Err(e) => {
                            let _ = tx2.send(AppEvent::Error(format!("Compact failed: {e}")));
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
            let msg = format!("Model changed\n\n  {} → {}", config.model, model);
            config.model = model.clone();
            app.set_model(model.clone());
            let _ =
                crate::config::Config::save_user_setting("model", serde_json::Value::String(model));
            *system_prompt = config.build_system_prompt();
            // Re-create backend when switching between Anthropic ↔ Ollama
            match ApiBackend::new_with_auth(
                &config.model,
                &config.api_key,
                config.auth_is_oauth,
                &config.ollama_host,
            ) {
                Ok(new_client) => {
                    *client = new_client;
                }
                Err(e) => {
                    app.entries
                        .push(ChatEntry::error(format!("Backend error: {e}")));
                }
            }
            app.overlay = Some(Overlay::new("model", msg));
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
                    ids.push(cmd.to_string());
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
            // Ephemeral prompt (e.g. /summary, /review) — send to Claude but
            // do NOT add to the persistent messages history so it doesn't bleed
            // into future turns.
            app.entries.push(ChatEntry::user(input.clone()));
            app.scroll_to_bottom();
            app.start_loading();
            // Snapshot: existing history + ephemeral prompt, but don't mutate messages
            let mut snapshot = messages.clone();
            snapshot.push(Message {
                role: Role::User,
                content: vec![ContentBlock::Text { text: prompt }],
            });
            let c2 = client.clone();
            let tvec = tools.to_vec();
            let cfg = config.clone();
            let tx2 = tx.clone();
            let sp = system_prompt.clone();
            let ps = perm_state.clone();
            let pm = app.plan_mode;
            let sid3 = session.id.clone();
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
                    session_id: sid3,
                })
                .await;
            });
            app.api_task = Some(handle.abort_handle());
        }
        CommandAction::Rewind(n) => {
            // Remove last n user+assistant pairs from messages and entries
            let pairs_to_remove = n * 2; // each exchange = user + assistant message
            let len = messages.len();
            if len == 0 {
                app.entries.push(ChatEntry::system("Nothing to rewind."));
            } else {
                let remove = pairs_to_remove.min(len);
                messages.truncate(len - remove);
                messages.shrink_to_fit();
                // Also trim display entries — remove last n*2 non-system entries
                let mut removed = 0;
                while removed < pairs_to_remove {
                    if let Some(pos) = app.entries.iter().rposition(|e| {
                        matches!(
                            e.kind,
                            crate::tui::app::EntryKind::User
                                | crate::tui::app::EntryKind::Assistant
                        )
                    }) {
                        app.entries.remove(pos);
                        removed += 1;
                    } else {
                        break;
                    }
                }

                // Restore file snapshots for rewound turns
                let restore_start = (*turn_counter).saturating_sub(n) + 1;
                let snap_base = crate::config::Config::sessions_dir()
                    .join(&session.id)
                    .join("snapshots");
                let mut restored_files: Vec<String> = Vec::new();
                for turn in (restore_start..=*turn_counter).rev() {
                    let snap_dir = snap_base.join(format!("turn-{}", turn));
                    if let Ok(entries) = std::fs::read_dir(&snap_dir) {
                        for entry in entries.flatten() {
                            let src = entry.path();
                            // Convert flat snapshot name back to absolute path
                            // e.g. "home_user_project_src_main.rs" → "/home/user/project/src/main.rs"
                            let flat = src
                                .file_name()
                                .and_then(|n| n.to_str())
                                .unwrap_or("")
                                .to_string();
                            let real_path =
                                std::path::PathBuf::from(format!("/{}", flat.replace('_', "/")));
                            if !restored_files.contains(&flat)
                                && std::fs::copy(&src, &real_path).is_ok()
                            {
                                restored_files.push(flat);
                            }
                        }
                    }
                    let _ = std::fs::remove_dir_all(&snap_dir);
                }
                *turn_counter = (*turn_counter).saturating_sub(n);

                let file_note = if restored_files.is_empty() {
                    String::new()
                } else {
                    format!(" {} file(s) restored.", restored_files.len())
                };
                app.entries.push(ChatEntry::system(format!(
                    "Rewound {} exchange{}.{}",
                    n,
                    if n == 1 { "" } else { "s" },
                    file_note
                )));
            }
        }
        CommandAction::ResumeSession(id_or_prefix) => {
            // Resolve prefix to full ID if necessary
            let full_id = if id_or_prefix.len() == 36 {
                // Looks like a full UUID — use directly
                Some(id_or_prefix.clone())
            } else {
                // Prefix match
                match Session::list().await {
                    Ok(list) => {
                        let matched: Vec<_> = list
                            .iter()
                            .filter(|m| m.id.starts_with(&id_or_prefix) || m.name == id_or_prefix)
                            .collect();
                        match matched.len() {
                            0 => {
                                app.overlay = Some(Overlay::new(
                                    "error",
                                    format!(
                                        "No session matching '{id_or_prefix}'. Try /session list"
                                    ),
                                ));
                                None
                            }
                            1 => Some(matched[0].id.clone()),
                            _ => {
                                let ids: Vec<_> = matched
                                    .iter()
                                    .map(|m| format!("  {} — {}", short_id(&m.id, 12), m.name))
                                    .collect();
                                app.overlay = Some(Overlay::new(
                                    "resume",
                                    format!(
                                        "Multiple sessions match '{id_or_prefix}':\n{}\nBe more specific.",
                                        ids.join("\n")
                                    ),
                                ));
                                None
                            }
                        }
                    }
                    Err(e) => {
                        app.overlay = Some(Overlay::new(
                            "error",
                            format!("Could not list sessions: {e}"),
                        ));
                        None
                    }
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
                        if meta.id != current_id {
                            let _ = Session::delete(&meta.id).await;
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
            let _ = crate::config::Config::save_user_setting(
                "voiceEnabled",
                serde_json::Value::Bool(enabled),
            );
            let msg = crate::voice::voice_status(enabled, config.tts_enabled);
            app.overlay = Some(Overlay::new("voice", msg));
        }
        CommandAction::SetTtsEnabled(enabled) => {
            if enabled {
                let has_xtts = crate::voice::xtts_available();
                let has_player = crate::voice::audio_player_available();

                if !has_xtts {
                    app.entries.push(ChatEntry::system(
                        "Cannot enable TTS — XTTS v2 not found.\n\n\
                         Install:\n\
                           uv tool install TTS --python 3.11 \\\n\
                             --with 'transformers<4.46' --with 'torch<2.6' --with 'torchaudio<2.6'"
                            .to_string(),
                    ));
                } else if !has_player {
                    app.entries.push(ChatEntry::system(
                        "Cannot enable TTS — no audio player.\n\
                         Install: sudo pacman -S mpv   # or alsa-utils"
                            .to_string(),
                    ));
                } else {
                    // Auto-start XTTS v2 server for fast synthesis
                    let tx2 = tx.clone();
                    tokio::spawn(async move {
                        match crate::voice::ensure_xtts_server().await {
                            Ok(_port) => {
                                let gpu = if crate::voice::cuda_available() {
                                    " (GPU)"
                                } else {
                                    " (CPU)"
                                };
                                let _ = tx2.send(AppEvent::SystemMessage(format!(
                                    "XTTS v2 server ready{gpu} — responses will be spoken."
                                )));
                            }
                            Err(e) => {
                                let _ = tx2.send(AppEvent::SystemMessage(format!(
                                    "XTTS v2 server failed: {e}\nFalling back to CLI mode (slower)."
                                )));
                            }
                        }
                    });
                    app.entries.push(ChatEntry::system(
                        "TTS on — starting XTTS v2 server (loading model, ~10s)...".to_string(),
                    ));
                }
                app.follow_bottom = true;
            } else {
                crate::voice::stop_xtts_server();
                app.entries.push(ChatEntry::system(
                    "TTS off. XTTS v2 server stopped.".to_string(),
                ));
                app.follow_bottom = true;
            }
            config.tts_enabled = enabled;
            let _ = crate::config::Config::save_user_setting(
                "ttsEnabled",
                serde_json::Value::Bool(enabled),
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
                let current = config
                    .tts_voice_model
                    .clone()
                    .unwrap_or_else(|| crate::voice::XTTS_DEFAULT_SPEAKER.to_string());
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
                turn_counter,
                &session.id[..8.min(session.id.len())],
                app.model,
                total_in,
                total_out,
                chart,
            );
            app.overlay = Some(Overlay::new("thinkback", text));
        }
        CommandAction::TeleportExport => {
            let teleport_path = crate::config::Config::claude_dir().join("teleport.json");
            let export_data = serde_json::json!({
                "session_id": session.id,
                "session_name": session.meta.name,
                "model": config.model,
                "messages": messages,
                "turn_count": turn_counter,
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
            let teleport_path = crate::config::Config::claude_dir().join("teleport.json");
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
                        // Try wl-copy (Wayland) then xclip (X11)
                        let copied = try_clipboard_write(&content).await;
                        let msg = if copied {
                            "Session copied to clipboard.".to_string()
                        } else {
                            "Clipboard tools not found. Install wl-copy or xclip.".to_string()
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
            let claude_md = crate::config::Config::claude_dir().join("CLAUDE.md");
            // Create the file if it doesn't exist
            if !claude_md.exists() {
                if let Some(parent) = claude_md.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                let _ = std::fs::write(&claude_md, "# CLAUDE.md\n\n");
            }
            let editor = std::env::var("VISUAL")
                .or_else(|_| std::env::var("EDITOR"))
                .unwrap_or_else(|_| "nano".to_string());
            // Suspend raw mode, run editor, restore
            let _ = crossterm::terminal::disable_raw_mode();
            let _ = tokio::process::Command::new(&editor)
                .arg(&claude_md)
                .status()
                .await;
            let _ = crossterm::terminal::enable_raw_mode();
            // Reload CLAUDE.md into config
            config.claudemd = crate::config::Config::load_claude_md(&config.cwd);
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
            tokio::spawn(plugin_install_task(spec, tx2));
        }
        CommandAction::ReloadSettings => {
            // Hot-reload settings.json without restarting
            let settings = crate::settings::Settings::load(&config.cwd);
            let mut reloaded = Vec::new();

            if let Some(model) = settings.model {
                let resolved = crate::commands::resolve_model_alias(&model);
                config.model = resolved.clone();
                app.set_model(resolved);
                reloaded.push("model");
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

            // Reload CLAUDE.md + AGENTS.md
            config.claudemd = crate::config::Config::load_claude_md(&config.cwd);
            config.agentsmd = crate::config::Config::load_agents_md(&config.cwd);

            let msg = if reloaded.is_empty() {
                "Settings reloaded (no changes detected). CLAUDE.md + AGENTS.md refreshed."
                    .to_string()
            } else {
                format!(
                    "Settings reloaded: {}. CLAUDE.md + AGENTS.md refreshed.",
                    reloaded.join(", ")
                )
            };
            app.entries.push(ChatEntry::system(msg));
            app.scroll_to_bottom();
        }
        CommandAction::ReloadPlugins => {
            // Re-read settings.json mcpServers section
            let count = crate::settings::Settings::load(&config.cwd)
                .mcp_servers
                .len();
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
            tokio::spawn(upgrade_check_task(tx2));
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
            let plugins_path = dirs::home_dir()
                .unwrap_or_default()
                .join(".claude")
                .join("plugins.json");
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
            let settings_path = dirs::home_dir()
                .unwrap_or_default()
                .join(".claude")
                .join("settings.json");
            let mut removed = false;
            if let Ok(content) = std::fs::read_to_string(&settings_path)
                && let Ok(mut json) = serde_json::from_str::<serde_json::Value>(&content)
            {
                if let Some(obj) = json["mcpServers"].as_object_mut() {
                    removed = obj.remove(&name).is_some();
                }
                if removed {
                    let _ = std::fs::write(
                        &settings_path,
                        serde_json::to_string_pretty(&json).unwrap_or_default(),
                    );
                }
            }
            // Remove from plugins.json
            let plugins_path = dirs::home_dir()
                .unwrap_or_default()
                .join(".claude")
                .join("plugins.json");
            if let Ok(content) = std::fs::read_to_string(&plugins_path)
                && let Ok(mut plugins) = serde_json::from_str::<serde_json::Value>(&content)
            {
                if let Some(obj) = plugins.as_object_mut() {
                    obj.remove(&name);
                }
                let _ = std::fs::write(
                    &plugins_path,
                    serde_json::to_string_pretty(&plugins).unwrap_or_default(),
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
        CommandAction::PluginCommand { plugin, command } => {
            // Plugin slash commands → invoke as a prompt so Claude calls
            // the matching MCP tool (e.g. ctx_doctor → mcp__…__ctx_doctor).
            let is_connected = mcp_statuses.iter().any(|s| s.name == plugin);
            if is_connected {
                let tool_name = command.replace('-', "_");
                let prompt = format!(
                    "Run the `{plugin}` MCP tool `{tool_name}`. \
                     If the exact name doesn't match, look for the closest \
                     tool starting with `mcp__` that contains `{tool_name}`."
                );
                app.entries.push(ChatEntry::user(input.clone()));
                app.scroll_to_bottom();
                app.start_loading();
                let mut snapshot = messages.clone();
                snapshot.push(Message {
                    role: Role::User,
                    content: vec![ContentBlock::Text { text: prompt }],
                });
                let c2 = client.clone();
                let tvec = tools.to_vec();
                let cfg = config.clone();
                let tx2 = tx.clone();
                let sp = system_prompt.clone();
                let ps = perm_state.clone();
                let pm = app.plan_mode;
                let sid3 = session.id.clone();
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
                        session_id: sid3,
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
            let cwd = config.cwd.clone();
            let label = if force {
                "Full re-index"
            } else {
                "Incremental index"
            };
            app.entries
                .push(ChatEntry::system(format!("{label} — indexing codebase …")));
            app.start_loading();
            app.scroll_to_bottom();
            let tx2 = tx.clone();
            tokio::spawn(async move {
                let result = tokio::task::spawn_blocking(move || {
                    let db = crate::rag::RagDb::open(&cwd)?;
                    crate::rag::indexer::index_project(&db, &cwd, force)
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
            let cwd = config.cwd.clone();
            match crate::rag::RagDb::open(&cwd) {
                Ok(db) => {
                    match crate::rag::search::search(&db, &query, 10) {
                        Ok(results) if results.is_empty() => {
                            app.entries.push(ChatEntry::system(format!(
                                "No results for '{query}'. Run /index first to build the index."
                            )));
                        }
                        Ok(results) => {
                            let mut lines = vec![format!(
                                "RAG search: '{}' — {} results\n",
                                query,
                                results.len()
                            )];
                            for r in &results {
                                lines.push(format!(
                                    "  {} {}:{}-{} ({} `{}`)",
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
                Err(e) => {
                    app.entries
                        .push(ChatEntry::system(format!("RAG database error: {e}")));
                }
            }
            app.scroll_to_bottom();
        }
        CommandAction::RagStatus => {
            let cwd = config.cwd.clone();
            match crate::rag::RagDb::open(&cwd) {
                Ok(db) => {
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
                         DB path:       {}",
                        db.db_path.display()
                    );
                    if !lang_breakdown.is_empty() {
                        text.push_str(&format!("\n\nLanguages:\n{lang_breakdown}"));
                    }
                    app.entries.push(ChatEntry::system(text));
                }
                Err(e) => {
                    app.entries
                        .push(ChatEntry::system(format!("RAG database error: {e}")));
                }
            }
            app.scroll_to_bottom();
        }
        CommandAction::RagClear => {
            let cwd = config.cwd.clone();
            match crate::rag::RagDb::open(&cwd) {
                Ok(db) => {
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
                Err(e) => {
                    app.entries
                        .push(ChatEntry::system(format!("RAG database error: {e}")));
                }
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
                Ok(store) => match store.search(&query, 20) {
                    Ok(matches) if matches.is_empty() => {
                        app.entries
                            .push(ChatEntry::system(format!("No memories matched '{query}'.")));
                    }
                    Ok(matches) => {
                        let count = matches.len();
                        let mut errs = 0usize;
                        for m in &matches {
                            if store.forget(&m.key).is_err() {
                                errs += 1;
                            }
                        }
                        let ok = count - errs;
                        app.entries.push(ChatEntry::system(format!(
                            "Forgot {ok} memory entries matching '{query}'."
                        )));
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
        CommandAction::RouterToggle => {
            app.router.enabled = !app.router.enabled;
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
        CommandAction::SetAutonomy(level) => {
            config.autonomy = level.clone();
            app.entries
                .push(ChatEntry::system(format!("Autonomy set to: {level}")));
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
            let tx2 = tx.clone();
            let cfg = config.clone();
            let reg = spawn_registry.clone();
            let task2 = task.clone();
            tokio::spawn(async move {
                match crate::spawn::spawn_agent(task2.clone(), &cfg, &reg, tx2.clone()).await {
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
                app.entries.push(ChatEntry::system(instructions));
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
        CommandAction::Undo { n } => {
            if app.is_loading {
                app.entries.push(ChatEntry::system(
                    "[undo] cannot undo while an assistant turn is running",
                ));
            } else if !config.auto_commit.enabled {
                app.entries.push(ChatEntry::system(
                    "[undo] auto-commit is disabled in settings",
                ));
            } else if !oxideclaw::autocommit::is_git_repo(&config.cwd) {
                app.entries.push(ChatEntry::system(
                    "[undo] auto-commit disabled — not a git repo",
                ));
            } else if session.meta.auto_commits.is_empty() {
                app.entries.push(ChatEntry::system(
                    "[undo] nothing to undo (session has no auto-commits)",
                ));
            } else if session.meta.undo_position == 0 {
                app.entries.push(ChatEntry::system(
                    "[undo] at session start, nothing more to undo",
                ));
            } else {
                match n {
                    Some(k) => {
                        let new_pos = session.meta.undo_position.saturating_sub(k as usize);
                        match oxideclaw::autocommit::restore_to(
                            &config.cwd,
                            &session.meta.auto_commits,
                            new_pos,
                        ) {
                            Ok(report) => {
                                session.meta.undo_position = new_pos;
                                if let Err(e) = session.save_meta().await {
                                    tracing::warn!("[undo] failed to save meta: {e}");
                                }
                                let label = if new_pos == 0 {
                                    "session base".to_string()
                                } else {
                                    format!("turn {new_pos}")
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
                    None => {
                        let mut labels: Vec<String> = Vec::new();
                        let mut positions: Vec<usize> = Vec::new();
                        let cur = session.meta.undo_position;
                        for i in (0..cur).rev() {
                            let target_pos = i + 1;
                            let marker = if target_pos == cur {
                                " ← current"
                            } else {
                                ""
                            };
                            labels.push(format!(
                                "turn {target_pos}  ·  {}{marker}",
                                session.meta.auto_commits[i]
                                    .chars()
                                    .take(7)
                                    .collect::<String>()
                            ));
                            positions.push(target_pos);
                        }
                        labels.push("session base (pre-OxideClaw)".to_string());
                        positions.push(0);
                        app.overlay = Some(crate::tui::app::Overlay::with_items(
                            "undo".to_string(),
                            String::new(),
                            labels,
                        ));
                        app.pending_undo_positions = Some(positions);
                    }
                }
            }
        }
        CommandAction::Redo { n } => {
            if app.is_loading {
                app.entries.push(ChatEntry::system(
                    "[redo] cannot redo while an assistant turn is running",
                ));
            } else if !config.auto_commit.enabled {
                app.entries.push(ChatEntry::system(
                    "[redo] auto-commit is disabled in settings",
                ));
            } else if !oxideclaw::autocommit::is_git_repo(&config.cwd) {
                app.entries.push(ChatEntry::system(
                    "[redo] auto-commit disabled — not a git repo",
                ));
            } else if session.meta.undo_position == session.meta.auto_commits.len() {
                app.entries
                    .push(ChatEntry::system("[redo] nothing to redo (at latest turn)"));
            } else {
                match n {
                    Some(k) => {
                        let new_pos = (session.meta.undo_position + k as usize)
                            .min(session.meta.auto_commits.len());
                        match oxideclaw::autocommit::restore_to(
                            &config.cwd,
                            &session.meta.auto_commits,
                            new_pos,
                        ) {
                            Ok(report) => {
                                session.meta.undo_position = new_pos;
                                if let Err(e) = session.save_meta().await {
                                    tracing::warn!("[redo] failed to save meta: {e}");
                                }
                                app.entries.push(ChatEntry::system(format!(
                                    "[redo] advanced to turn {new_pos} ({} files restored)",
                                    report.files_restored
                                )));
                            }
                            Err(e) => {
                                app.entries
                                    .push(ChatEntry::system(format!("[redo] restore failed: {e}")));
                            }
                        }
                    }
                    None => {
                        let mut labels: Vec<String> = Vec::new();
                        let mut positions: Vec<usize> = Vec::new();
                        let cur = session.meta.undo_position;
                        let cur_label = if cur == 0 {
                            "session base (pre-OxideClaw) ← current".to_string()
                        } else {
                            format!(
                                "turn {cur}  ·  {} ← current",
                                session.meta.auto_commits[cur - 1]
                                    .chars()
                                    .take(7)
                                    .collect::<String>()
                            )
                        };
                        labels.push(cur_label);
                        positions.push(cur);
                        for i in cur..session.meta.auto_commits.len() {
                            labels.push(format!(
                                "turn {}  ·  {}",
                                i + 1,
                                session.meta.auto_commits[i]
                                    .chars()
                                    .take(7)
                                    .collect::<String>()
                            ));
                            positions.push(i + 1);
                        }
                        app.overlay = Some(crate::tui::app::Overlay::with_items(
                            "redo".to_string(),
                            String::new(),
                            labels,
                        ));
                        app.pending_redo_positions = Some(positions);
                    }
                }
            }
        }
        CommandAction::TrustProject { status_only } => {
            let global = crate::settings::Settings::load_global();
            let trusted = crate::settings::Settings::is_trusted(&global, &config.cwd);
            let canonical = config
                .cwd
                .canonicalize()
                .unwrap_or_else(|_| config.cwd.clone())
                .to_string_lossy()
                .into_owned();
            let msg = if status_only || trusted {
                format!(
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
                )
            } else {
                let mut list = global.trusted_projects.unwrap_or_default();
                list.push(canonical.clone());
                match crate::config::Config::save_user_setting(
                    "trustedProjects",
                    serde_json::json!(list),
                ) {
                    Ok(()) => format!(
                        "Trusted {canonical}. Its settings hooks, apiKeyHelper and MCP \
                         servers will be honoured — run /reload (or restart) to apply."
                    ),
                    Err(e) => format!("Could not save trust: {e}"),
                }
            };
            app.entries.push(ChatEntry::system(msg));
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

            // Create channels for progress events and approval prompts
            let (progress_tx, progress_rx) = tokio::sync::mpsc::channel(64);
            let (approval_tx, approval_rx) = tokio::sync::mpsc::channel(4);
            app.browse_progress_rx = Some(progress_rx);
            app.browse_approval_rx = Some(approval_rx);

            // Shared current-URL state
            let current_url = std::sync::Arc::new(tokio::sync::Mutex::new(String::new()));

            let cfg = config.clone();
            let all_tools = tools.to_vec();
            let browser_session = app.browser_session.clone();

            let browse_req = crate::browser::browse_loop::BrowseRequest {
                goal,
                policy,
                max_steps: max,
                voice: false,
            };
            let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            tokio::spawn(async move {
                let channels = crate::browser::browse_loop::BrowseChannels {
                    progress_tx,
                    approval_tx,
                    cancel,
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
                if let Err(e) = result {
                    eprintln!("Browse error: {e}");
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
            let mut snapshot = messages.clone();
            snapshot.push(Message {
                role: Role::User,
                content: vec![ContentBlock::Text { text: prompt }],
            });
            let c2 = client.clone();
            let tvec = tools.to_vec();
            let cfg = config.clone();
            let tx2 = tx.clone();
            let sp = system_prompt.clone();
            let ps = perm_state.clone();
            let pm = app.plan_mode;
            let sid3 = session.id.clone();
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
                    session_id: sid3,
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
            let mut snapshot = messages.clone();
            snapshot.push(Message {
                role: Role::User,
                content: vec![ContentBlock::Text { text: prompt }],
            });
            let c2 = client.clone();
            let tvec = tools.to_vec();
            let cfg = config.clone();
            let tx2 = tx.clone();
            let sp = system_prompt.clone();
            let ps = perm_state.clone();
            let pm = app.plan_mode;
            let sid3 = session.id.clone();
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
                    session_id: sid3,
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
            // Use tokio::process::Command with separate args — NEVER sh -c
            // with a formatted path (user-controlled → shell injection).
            let mut cmd = tokio::process::Command::new("git");
            cmd.arg("diff").current_dir(&config.cwd);
            if let Some(p) = &path {
                cmd.arg("--").arg(p);
            }
            match cmd.output().await {
                Ok(output) if output.status.success() => {
                    let diff_text = String::from_utf8_lossy(&output.stdout).into_owned();
                    if diff_text.trim().is_empty() {
                        app.entries
                            .push(ChatEntry::system("No uncommitted changes."));
                    } else {
                        let files = crate::tui::diff::parse_unified_diff(&diff_text);
                        let summary: String = files
                            .iter()
                            .map(|f| format!("  {} (+{} -{})", f.path, f.additions, f.deletions))
                            .collect::<Vec<_>>()
                            .join("\n");
                        app.overlay = Some(Overlay::new(
                            "diff",
                            format!("Diff Review\n\n{summary}\n\n{diff_text}"),
                        ));
                    }
                }
                Ok(output) => {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    app.entries
                        .push(ChatEntry::error(format!("git diff failed: {stderr}")));
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
                let mut prompt = skill.expand_named(args);
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
                // Skills are also ephemeral — don't contaminate history
                let mut snapshot = messages.clone();
                snapshot.push(Message {
                    role: Role::User,
                    content: vec![ContentBlock::Text { text: prompt }],
                });
                let c2 = client.clone();
                let tvec = tools.to_vec();
                let cfg = config.clone();
                let tx2 = tx.clone();
                let sp = system_prompt.clone();
                let ps = perm_state.clone();
                let pm = app.plan_mode;
                let sid4 = session.id.clone();
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
                        session_id: sid4,
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
