/// Main TUI loop — ratatui alternate screen, event-driven (no fixed tick).
use crate::api::types::*;
use crate::api::{ApiBackend, MessagesRequest};
use crate::commands::{CommandAction, CommandContext, dispatch};
use crate::compact::{CompactNeeded, compact_needed, snip_compact, summarize_compact};
use crate::config::Config;
use crate::hooks;
use crate::mcp::{McpManager, mcp_dyn_tools};
use crate::permissions::{
    GateOutcome, PermissionAsker, PermissionDecision, PermissionGate, PermissionState,
};

// ── Submodules (mechanical split of the former 5.9k-line file) ─────────────
mod api_task;
mod dispatch;
mod input_helpers;
mod keys;
mod plugins;
use api_task::*;
use input_helpers::*;
use keys::*;
use plugins::*;

/// First `n` characters of an id for display. Session ids are UUIDs, but a
/// hand-edited or foreign `.meta` file can carry anything; a byte slice
/// panics on a short or non-ASCII id and takes the picker down with it.
fn short_id(id: &str, n: usize) -> &str {
    match id.char_indices().nth(n) {
        Some((i, _)) => &id[..i],
        None => id,
    }
}

/// Puts a permission prompt in front of the user through the TUI event
/// loop. A dropped reply (TUI shutdown, panic, SIGHUP) is `None`, which the
/// gate treats as Deny — the "close terminal = auto-approve" class.
struct TuiAsker {
    tx: mpsc::UnboundedSender<AppEvent>,
}

#[async_trait::async_trait]
impl PermissionAsker for TuiAsker {
    async fn ask(&self, tool_name: &str, description: &str) -> Option<PermissionDecision> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(AppEvent::PermissionRequest {
                tool_name: tool_name.to_string(),
                description: description.to_string(),
                reply: reply_tx,
            })
            .ok()?;
        reply_rx.await.ok()
    }
}
use crate::session::{Session, entries_from_messages};
use crate::skills::{load_skills, parse_skill_invocation};
use crate::tools::todo::TodoState;
use crate::tools::{DynTool, ToolContext, ToolOutput, all_tools_with_state_and_mcp};
use crate::tui::app::{App, ChatEntry, Overlay};
use crate::tui::events::AppEvent;
use crate::tui::render::draw;
use anyhow::Result as AResult;

use anyhow::Result;
use crossterm::{
    event::{
        DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, EventStream, KeyCode, KeyModifiers, MouseEventKind,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode},
};
use futures_util::StreamExt as _;
use ratatui::{Terminal, TerminalOptions, Viewport, backend::CrosstermBackend};
use std::io;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

pub async fn run_tui(
    config: Config,
    resume_id: Option<String>,
    initial_input: Option<String>,
) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    // Clear the visible screen and anchor cursor at top-left so the compact
    // inline viewport always starts at the top of the terminal window —
    // matching the TS oxideclaw/Ink behaviour where the banner appears right
    // below the launch command regardless of where the cursor was.
    // Previous content remains in the scrollback buffer (not deleted).
    execute!(
        stdout,
        crossterm::terminal::Clear(crossterm::terminal::ClearType::All),
        crossterm::cursor::MoveTo(0, 0),
        EnableBracketedPaste,
        EnableMouseCapture,
    )?;
    let result = run_loop(config, resume_id, initial_input).await;
    disable_raw_mode()?;
    let mut cleanup = io::stdout();
    execute!(
        cleanup,
        DisableBracketedPaste,
        DisableMouseCapture,
        crossterm::cursor::Show
    )?;
    result
}

/// Compute the inline viewport height for this frame.
/// On the welcome screen: exactly banner + input + status so the prompt sits
/// right below the box (matching TS oxideclaw/Ink compact behaviour).
/// During chat: full terminal height to maximise scroll room.
fn viewport_height(app: &App, term_cols: u16, term_rows: u16) -> u16 {
    let show_banner = app.show_welcome && app.entries.is_empty() && app.streaming.is_empty();
    let status_h = 1u16;

    let usable_w = term_cols.saturating_sub(2) as usize;
    let full_input: String = app.input.iter().collect();
    let input_h: u16 = full_input
        .split('\n')
        .map(|line| {
            let n = line.chars().count();
            (n.saturating_add(usable_w).saturating_sub(1) / usable_w.max(1)).max(1) as u16
        })
        .sum::<u16>()
        .clamp(1, 8);

    if show_banner {
        // Must mirror the banner_h formula in render::draw() exactly.
        const LOGO_H: u16 = 6; // LOGO.len() in render.rs
        let left_h = LOGO_H + 7; // welcome + blank + logo + blank + model + cwd + blank + tagline
        let sess_h = (app.recent_sessions.len() as u16).min(4) * 2;
        let right_h = 6 + sess_h;
        let banner_h = left_h.max(right_h) + 2;
        (banner_h + input_h + status_h).min(term_rows)
    } else {
        term_rows
    }
}

/// Create a new inline terminal with the given viewport height.
/// Viewport::Inline(n) stores n immutably and resize() has no effect on it,
/// so when the height needs to change we drop the old terminal and create a
/// fresh one.  io::stdout() is a handle to fd 1 — multiple instances are fine.
fn make_terminal(vp_h: u16) -> Result<Terminal<CrosstermBackend<io::Stdout>>> {
    let backend = CrosstermBackend::new(io::stdout());
    match Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: Viewport::Inline(vp_h),
        },
    ) {
        Ok(t) => Ok(t),
        Err(_) => {
            // Fallback: fullscreen viewport (works in all terminals)
            let backend = CrosstermBackend::new(io::stdout());
            Ok(Terminal::with_options(
                backend,
                TerminalOptions {
                    viewport: Viewport::Fullscreen,
                },
            )?)
        }
    }
}

// ── Plugin install async task ─────────────────────────────────────────────────

async fn run_loop(
    mut config: Config,
    resume_id: Option<String>,
    initial_input: Option<String>,
) -> Result<()> {
    // Compute welcome-screen height and create the first terminal.
    let (init_cols, init_rows) = crossterm::terminal::size().unwrap_or((80, 24));
    let init_h = {
        const LOGO_H: u16 = 6;
        let left_h = LOGO_H + 7; // welcome + blank + logo + blank + model + cwd + blank + tagline
        let right_h = 6u16; // no recent sessions yet (sessions loaded below)
        (left_h.max(right_h) + 2 + 1 + 1).min(init_rows)
    };
    let mut terminal = make_terminal(init_h)?;
    let mut current_vp_h = init_h;
    let mut last_term_cols = init_cols;
    let mut last_term_rows = init_rows; // cached — updated only on Resize events
    let mut system_prompt = config.build_system_prompt();
    // Validate Anthropic API key only when the initial model is Anthropic.
    // Ollama + OpenAI-compat providers manage their own credentials elsewhere.
    let is_non_anthropic = crate::api::is_ollama_model(&config.model)
        || crate::api::is_openai_compat_model(&config.model);
    if !is_non_anthropic && config.api_key.is_empty() {
        return Err(anyhow::anyhow!(
            "No Anthropic credential found.\n\
                 OxideClaw checks, in order:\n\
                   1. ANTHROPIC_API_KEY      export ANTHROPIC_API_KEY=sk-ant-...\n\
                   2. ANTHROPIC_AUTH_TOKEN   an OAuth access token\n\
                   3. apiKeyHelper / OXIDECLAW_API_KEY_FILE_DESCRIPTOR\n\
                   4. ant auth login         shared with Claude Code and the official SDKs\n\
                 To use a local model instead: --model ollama:<name>\n\
                 Or a cloud OpenAI-compatible model: --model groq:<name>, --model openrouter:<name>, ..."
        ));
    }
    let mut client: ApiBackend = ApiBackend::new_with_auth(
        &config.model,
        &config.api_key,
        config.auth_is_oauth,
        &config.ollama_host,
    )?;

    // Start MCP servers (failures are logged and skipped — never fatal)
    let settings = crate::settings::Settings::load(&config.cwd);
    // --strict-mcp-config: only use CLI --mcp-config servers, ignore settings.json
    let mcp_manager = if config.strict_mcp_config {
        McpManager::start_with_extra(
            &crate::settings::Settings::default(),
            &config.extra_mcp_servers,
        )
        .await
    } else {
        McpManager::start_with_extra(&settings, &config.extra_mcp_servers).await
    };
    let mcp_tools = mcp_dyn_tools(&mcp_manager);
    let mcp_statuses = mcp_manager.statuses();

    let mcp_clients = mcp_manager.clients.clone();
    let (mut tools, shared_state) = all_tools_with_state_and_mcp(&config, mcp_tools, mcp_clients);
    let todo_state: TodoState = shared_state.todo;
    // Keep a handle to the shared browser session so /browser, /browse and /screenshot
    // drive the SAME Chrome instance as the `browser_*` tools. `None` if browser disabled.
    let browser_session_for_app = shared_state.browser_session.clone();
    let spawn_registry = crate::spawn::new_registry();

    // Apply --allowed-tools / --disallowed-tools CLI filters
    if !config.allowed_tools.is_empty() {
        tools.retain(|t| {
            config
                .allowed_tools
                .iter()
                .any(|a| a.eq_ignore_ascii_case(t.name()))
        });
    }
    if !config.disallowed_tools.is_empty() {
        tools.retain(|t| {
            !config
                .disallowed_tools
                .iter()
                .any(|d| d.eq_ignore_ascii_case(t.name()))
        });
    }
    let perm_state = PermissionState::new(
        config.dangerously_skip_permissions,
        &config.permissions_allow,
        &config.permissions_deny,
    );
    let skills = load_skills().await;

    let mut app = App::new(&config.model, &config.cwd);
    app.browser_session = browser_session_for_app;
    // A deep link's prompt lands in the input box for the user to read and
    // send (or not) — it is never submitted on their behalf.
    if let Some(text) = initial_input {
        app.input = text.chars().collect();
        app.cursor = app.input.len();
        app.entries.push(ChatEntry::system(
            "Deep link received — the prompt is in the input box. Review it, then press Enter to send.",
        ));
    }
    // Spawn worktrees a crash left behind (a clean exit removes running
    // ones; a crash has no exit path).
    let leftovers = crate::spawn::leftover_spawn_worktrees(&config.cwd).await;
    if !leftovers.is_empty() {
        let list: Vec<String> = leftovers
            .iter()
            .map(|(b, p)| format!("  {b}  →  {}", p.display()))
            .collect();
        app.entries.push(ChatEntry::system(format!(
            "Leftover spawn worktrees from a previous session:\n{}\nMerge with `git merge <branch>` or remove with `git worktree remove <path>`.",
            list.join("\n")
        )));
    }
    if !config.untrusted_project_config.is_empty() {
        app.entries.push(ChatEntry::system(format!(
            "This project's settings define {} — ignored because the project is not trusted. \
             A cloned repository must not run commands on your machine by itself. \
             Run /trust to enable them for this folder.",
            config.untrusted_project_config.join(", ")
        )));
    }
    if let Some(ref e) = config.effort {
        app.effort = Some(e.clone());
    }
    app.spinner_style = config.spinner_style.clone();
    // Apply theme from config (loaded from settings.json)
    if let Some(ref theme) = config.theme {
        app.theme = theme.clone();
    }
    // Apply router settings from config (loaded from settings.json)
    if config.router_enabled {
        app.router.enabled = true;
    }
    if let Some(budget) = config.router_budget {
        app.cost_tracker.set_budget(budget);
    }
    if let Some(ref m) = config.router_low_model {
        app.router.low_model = m.clone();
    }
    if let Some(ref m) = config.router_medium_model {
        app.router.medium_model = m.clone();
    }
    if let Some(ref m) = config.router_high_model {
        app.router.high_model = m.clone();
    }
    if let Some(ref m) = config.router_super_high_model {
        app.router.super_high_model = m.clone();
    }
    let mut messages: Vec<Message> = Vec::new();
    let mut last_tokens_in: u64 = 0;
    let mut consecutive_compact_count: u32 = 0;
    let mut saved_count: usize = 0;
    // Turn counter for file history snapshots (increments on each user prompt sent to API)
    let mut turn_counter: usize = 0;

    // Session cleanup — delete sessions older than cleanupPeriodDays
    if let Some(days) = config.cleanup_period_days
        && days > 0
        && let Ok(list) = crate::session::Session::list().await
    {
        let cutoff_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
            .saturating_sub(days as u64 * 86400);
        for meta in &list {
            if meta.created_at < cutoff_secs {
                let _ = crate::session::Session::delete(&meta.id).await;
            }
        }
    }

    // Session — create new or resume existing
    let mut session = match resume_id.clone() {
        Some(ref id) => {
            match Session::resume(id).await {
                Ok((mut s, loaded_messages)) => {
                    // --fork-session: assign a new UUID so we don't overwrite the original
                    if config.fork_session {
                        s.id = uuid::Uuid::new_v4().to_string();
                        s.meta.name = format!("fork-of-{}", &id[..8.min(id.len())]);
                    }
                    // Restore chat entries for display
                    let display = entries_from_messages(&loaded_messages);
                    app.entries.extend(display);
                    app.show_welcome = false;
                    saved_count = loaded_messages.len();
                    messages = loaded_messages;
                    let label = if config.fork_session {
                        "Forked"
                    } else {
                        "Resumed"
                    };
                    app.entries.push(ChatEntry::system(format!(
                        "{} session '{}' ({} messages)",
                        label, s.meta.name, saved_count
                    )));
                    app.session_name = s.meta.name.clone();
                    app.scroll_to_bottom();
                    s
                }
                Err(e) => {
                    app.entries
                        .push(ChatEntry::error(format!("Could not resume session: {e}")));
                    let s = Session::new().await?;
                    app.session_name = s.meta.name.clone();
                    s
                }
            }
        }
        None => {
            let s = Session::new().await?;
            app.session_name = s.meta.name.clone();
            s
        }
    };

    // Apply session name from CLI --name if provided
    if let Some(ref name) = config.session_name {
        session.meta.name = name.clone();
        app.session_name = name.clone();
    }

    // SessionStart hooks
    if let Some(hook_cfg) = &config.hooks
        && !config.disable_all_hooks
    {
        hooks::run_session_start_hooks(hook_cfg, &session.id, &config.cwd).await;
    }

    // Move pre-rename shadow refs first so /undo history survives, then prune
    // (keeps the configured number of newest sessions).
    if let Err(e) = oxideclaw::autocommit::migrate_legacy_refs(&config.cwd) {
        tracing::warn!("autoCommit ref migration failed: {e}");
    }
    if let Err(e) =
        oxideclaw::autocommit::prune_old_refs(&config.cwd, config.auto_commit.keep_sessions)
    {
        tracing::warn!("autoCommit startup prune failed: {e}");
    }

    // Pre-load recent sessions for the welcome screen — exclude the current session
    // so it doesn't appear in the list AND in the title bar at the same time.
    if let Ok(list) = Session::list().await {
        let current_id = &session.id;
        app.recent_sessions = list
            .into_iter()
            .filter(|m| &m.id != current_id)
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

    let (tx, mut rx) = mpsc::unbounded_channel::<AppEvent>();
    let mut term_events = EventStream::new();

    // ── Background RAG indexing (incremental, non-blocking) ────────────────
    {
        let cwd = config.cwd.clone();
        let tx2 = tx.clone();
        tokio::spawn(async move {
            let result = tokio::task::spawn_blocking(move || {
                let db = crate::rag::RagDb::open(&cwd)?;
                crate::rag::indexer::index_project(&db, &cwd, false)
            })
            .await
            .unwrap_or_else(|e| Err(anyhow::anyhow!("RAG index panicked: {e}")));
            match result {
                Ok(r) if r.files_indexed > 0 => {
                    let _ = tx2.send(crate::tui::events::AppEvent::SystemMessage(format!(
                        "Codebase indexed — {} files, {} chunks ({:.0}ms)",
                        r.files_indexed, r.chunks_added, r.elapsed_ms
                    )));
                }
                Ok(_) => {} // nothing new to index — stay silent
                Err(e) => {
                    tracing::warn!("Background RAG indexing failed: {e}");
                }
            }
        });
    }

    loop {
        // Recreate the terminal if the needed viewport height or width changed.
        // If /clear was issued, scroll old content off screen before redrawing.
        // With Viewport::Inline the terminal doesn't own the full screen, so we
        // print blank lines equal to the terminal height to push history upward.
        // /install-missing — drop raw mode so sudo can prompt for password
        if let Some(cmd) = app.pending_install.take() {
            drop(terminal);
            let _ = crossterm::terminal::disable_raw_mode();
            let _ = execute!(io::stdout(), crossterm::cursor::Show);
            println!(); // blank line before package manager output

            let parts: Vec<&str> = cmd.split_whitespace().collect();
            let success = if let Some((bin, args)) = parts.split_first() {
                std::process::Command::new(bin)
                    .args(args)
                    .status()
                    .map(|s| s.success())
                    .unwrap_or(false)
            } else {
                false
            };

            println!(); // blank line after package manager output
            let _ = crossterm::terminal::enable_raw_mode();
            let _ = execute!(
                io::stdout(),
                crossterm::terminal::Clear(crossterm::terminal::ClearType::All),
                crossterm::cursor::MoveTo(0, 0),
            );
            let needed = viewport_height(&app, last_term_cols, last_term_rows);
            terminal = make_terminal(needed)?;
            current_vp_h = needed;

            let msg = if success {
                "Install complete — run /doctor to verify.".to_string()
            } else {
                "Install exited with an error. Check the output above.".to_string()
            };
            app.entries.push(ChatEntry::system(msg));
            app.scroll_to_bottom();
        }

        if app.pending_screen_clear {
            app.pending_screen_clear = false;
            drop(terminal);
            execute!(
                io::stdout(),
                crossterm::terminal::Clear(crossterm::terminal::ClearType::All),
                crossterm::cursor::MoveTo(0, 0),
            )?;
            let needed = viewport_height(&app, last_term_cols, last_term_rows);
            terminal = make_terminal(needed)?;
            current_vp_h = needed;
        }

        // Handle pending session delete from interactive session picker
        if let Some(id) = app.pending_delete.take() {
            match Session::delete(&id).await {
                Ok(()) => {
                    app.entries.push(ChatEntry::system(format!(
                        "Deleted session {}.",
                        short_id(&id, 8)
                    )));
                    // Refresh the session list overlay + welcome banner
                    if let Ok(list) = Session::list().await {
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
                            "  ↑↓ select · Enter resume · 1-9 quick pick · d delete · Esc close"
                                .into(),
                        );
                        let prev_selected = app.overlay.as_ref().map(|o| o.selected).unwrap_or(0);
                        let mut new_overlay =
                            Overlay::with_items("sessions", lines.join("\n"), ids.clone());
                        new_overlay.selected = prev_selected.min(ids.len().saturating_sub(1));
                        app.overlay = Some(new_overlay);
                        // Also refresh the welcome banner's recent sessions
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
                Err(e) => {
                    app.entries
                        .push(ChatEntry::error(format!("Failed to delete session: {e}")));
                }
            }
        }

        // Handle pending session resume from interactive session picker
        if let Some(id) = app.pending_resume.take() {
            match Session::resume(&id).await {
                Ok((new_session, loaded_messages)) => {
                    let display = entries_from_messages(&loaded_messages);
                    saved_count = loaded_messages.len();
                    messages = loaded_messages;
                    app.entries = display;
                    app.streaming = String::new();
                    app.show_welcome = false;
                    app.scroll_to_bottom();
                    let resume_name = new_session.meta.name.clone();
                    let resume_count = saved_count;
                    app.session_name = resume_name.clone();
                    session = new_session;
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

        // Handle pending model switch from interactive model picker
        if let Some(model) = app.pending_model.take() {
            let msg = format!("Model changed\n\n  {} → {}", config.model, model);
            config.model = model.clone();
            app.set_model(model.clone());
            let _ =
                crate::config::Config::save_user_setting("model", serde_json::Value::String(model));
            system_prompt.clear();
            system_prompt.push_str(&config.build_system_prompt());
            match ApiBackend::new_with_auth(
                &config.model,
                &config.api_key,
                config.auth_is_oauth,
                &config.ollama_host,
            ) {
                Ok(new_client) => {
                    client = new_client;
                }
                Err(e) => {
                    app.entries
                        .push(ChatEntry::error(format!("Backend error: {e}")));
                }
            }
            app.entries.push(ChatEntry::system(msg));
            app.scroll_to_bottom();
        }

        // Handle pending help category selection from interactive help picker
        if let Some(idx) = app.pending_help_category.take() {
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

        // Handle pending help command — populate input line with the selected command
        if let Some(cmd) = app.pending_help_command.take() {
            app.input = cmd.chars().collect();
            app.cursor = app.input.len();
        }

        // Handle pending voice model selection — set model, save, play preview
        if let Some(model_path) = app.pending_voice_model.take() {
            let display = std::path::Path::new(&model_path)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("unknown")
                .to_string();
            config.tts_voice_model = Some(model_path.clone());
            let _ = crate::config::Config::save_user_setting(
                "ttsVoiceModel",
                serde_json::Value::String(model_path.clone()),
            );
            app.entries.push(ChatEntry::system(format!(
                "Voice model set to: {display}\nPlaying preview… (Esc or Ctrl+S to stop)"
            )));
            app.scroll_to_bottom();
            // Stop any existing TTS before starting preview
            if let Some(prev) = app.tts_stop_tx.take() {
                let _ = prev.send(());
            }
            let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
            app.tts_stop_tx = Some(stop_tx);
            let model = model_path.clone();
            let tx2 = tx.clone();
            tokio::spawn(async move {
                let preview_text = "Hi there. This is OxideClaw. Ready whenever you are.";
                match crate::voice::speak(preview_text, Some(&model), stop_rx).await {
                    Ok(_) => {}
                    Err(e) => {
                        let _ = tx2.send(AppEvent::Error(format!("Voice preview failed: {e}")));
                    }
                }
            });
        }

        // ── Poll browse progress events ──────────────────────────────────────
        if let Some(mut rx) = app.browse_progress_rx.take() {
            let mut done = false;
            while let Ok(event) = rx.try_recv() {
                use crate::browser::browse_loop::BrowseProgress;
                match event {
                    BrowseProgress::Started { .. } => {
                        // Already shown at dispatch time
                    }
                    BrowseProgress::Step { n, action, target } => {
                        app.entries
                            .push(ChatEntry::system(format!("  Step {n}: {action} {target}")));
                        app.scroll_to_bottom();
                    }
                    BrowseProgress::Nudge { level, text } => {
                        app.entries
                            .push(ChatEntry::system(format!("  ⚠ Nudge L{level}: {text}")));
                        app.scroll_to_bottom();
                    }
                    BrowseProgress::ApprovalNeeded { .. } => {
                        // Handled via approval_rx below
                    }
                    BrowseProgress::Completed(result) => {
                        let icon = if result.achieved { "✅" } else { "⚠" };
                        app.entries.push(ChatEntry::system(format!(
                            "{icon} /browse done ({:?}): {}",
                            result.reason, result.summary
                        )));
                        app.scroll_to_bottom();
                        app.finish_loading();
                        // Clean up approval channel too
                        app.browse_approval_rx = None;
                        done = true;
                        break;
                    }
                }
            }
            if !done {
                // Put the receiver back — run is still in progress
                app.browse_progress_rx = Some(rx);
            }
        }

        // Poll browse approval prompts
        if let Some(mut rx) = app.browse_approval_rx.take() {
            if let Ok(prompt) = rx.try_recv() {
                app.browse_approval = Some(prompt);
            }
            // Put back if browse is still running
            if app.browse_progress_rx.is_some() || app.browse_approval.is_some() {
                app.browse_approval_rx = Some(rx);
            }
        }

        // Uses cached term size — no syscall per frame; updated on Resize events.
        // Bound scrollback before measuring/drawing. Every path that appends to
        // `app.entries` reaches the renderer through here, so this single call is
        // sufficient — no need to police ~40 individual push sites.
        app.trim_entries();

        {
            let needed = viewport_height(&app, last_term_cols, last_term_rows);
            if needed != current_vp_h {
                drop(terminal);
                terminal = make_terminal(needed)?;
                current_vp_h = needed;
            }
        }
        terminal.draw(|f| draw(f, &mut app))?;

        // ── Wait for next activity: API event, keyboard, or 50 ms heartbeat ──
        tokio::select! {
            biased; // prioritise API events so streaming renders without delay

            // API / background task events
            Some(event) = rx.recv() => {
                // Handle the first event, then drain any that arrived simultaneously
                let mut ev = event;
                loop {
                    match ev {
                        AppEvent::Done { tokens_in, tokens_out, cache_read, cache_write, messages: new_messages, model_used } => {
                            last_tokens_in = tokens_in;
                            messages = new_messages.clone();
                            if !config.no_session_persistence && new_messages.len() > saved_count {
                                let to_save = new_messages[saved_count..].to_vec();
                                saved_count = new_messages.len();
                                let _ = session.append(&to_save).await;
                            }
                            // Per-turn cost tracking
                            app.turn_costs.push((tokens_in, tokens_out));
                            // Record in cost tracker (per-model breakdown)
                            app.cost_tracker.record(&model_used, tokens_in, tokens_out);
                            // Budget check
                            if app.cost_tracker.budget_warning()
                                && let Some(remaining) = app.cost_tracker.remaining() {
                                    app.entries.push(ChatEntry::system(
                                        format!("Budget warning: ${:.4} remaining", remaining)
                                    ));
                                }
                            if app.cost_tracker.over_budget() {
                                app.entries.push(ChatEntry::system(
                                    "Budget exceeded! Use /budget to adjust or remove the limit.".to_string()
                                ));
                            }

                            // Notifications + terminal bell on task completion
                            if config.notifications_enabled {
                                use std::io::Write;
                                print!("\x07"); // terminal bell
                                let _ = std::io::stdout().flush();
                                tokio::spawn(async {
                                    let _ = tokio::process::Command::new("notify-send")
                                        .args(["oxideclaw", "Task complete"])
                                        .spawn();
                                });
                            }

                            // TTS: speak the last assistant response
                            if config.tts_enabled {
                                let tts_text: String = new_messages.iter()
                                    .rfind(|m| m.role == Role::Assistant)
                                    .map(|m| m.content.iter()
                                        .filter_map(|b| if let ContentBlock::Text { text } = b {
                                            Some(text.as_str())
                                        } else { None })
                                        .collect::<Vec<_>>()
                                        .join(" "))
                                    .unwrap_or_default();
                                if !tts_text.is_empty() {
                                    // Cancel any previous TTS still playing
                                    if let Some(prev) = app.tts_stop_tx.take() {
                                        let _ = prev.send(());
                                    }
                                    let (stop_tx, stop_rx) = oneshot::channel::<()>();
                                    app.tts_stop_tx = Some(stop_tx);
                                    let voice_model = config.tts_voice_model.clone();
                                    let tts_tx = tx.clone();
                                    tokio::spawn(async move {
                                        match crate::voice::speak(&tts_text, voice_model.as_deref(), stop_rx).await {
                                            Ok(true) => {
                                                let _ = tts_tx.send(AppEvent::SystemMessage(
                                                    format!("TTS: response trimmed to {} words — use Ctrl+S to stop early, /voice speak off to disable.", crate::voice::TTS_WORD_LIMIT)
                                                ));
                                            }
                                            Err(e) => {
                                                let _ = tts_tx.send(AppEvent::SystemMessage(format!(
                                                    "TTS failed: {e}"
                                                )));
                                            }
                                            Ok(false) => {}
                                        }
                                    });
                                }
                            }
                            // Auto-capture: scan assistant response for notable decisions
                            if config.memory_auto_capture {
                                let response_text: String = new_messages.iter()
                                    .rfind(|m| m.role == Role::Assistant)
                                    .map(|m| m.content.iter()
                                        .filter_map(|b| if let ContentBlock::Text { text } = b {
                                            Some(text.as_str())
                                        } else { None })
                                        .collect::<Vec<_>>()
                                        .join(" "))
                                    .unwrap_or_default();
                                if !response_text.is_empty() {
                                    let cwd = config.cwd.clone();
                                    tokio::spawn(async move {
                                        if let Ok(store) = crate::memory::MemoryStore::open(&cwd) {
                                            for candidate in crate::memory::auto_capture_memories(&response_text) {
                                                let _ = store.add_auto(&candidate, "auto");
                                            }
                                        }
                                    });
                                }
                            }
                            app.apply(AppEvent::Done { tokens_in, tokens_out, cache_read, cache_write, messages: new_messages, model_used });

                            // Auto-commit: snapshot the working tree for this turn.
                            if config.auto_commit.enabled {
                                // snapshot_turn truncates the redo stack to undo_position before
                                // appending, so after /undo + new work, auto_commits is about to
                                // shrink. Use the post-truncation index so the tracing log line
                                // reflects the real new turn number.
                                let turn_index = (session.meta.undo_position as u32) + 1;
                                let prompt = app
                                    .entries
                                    .iter()
                                    .rev()
                                    .find_map(|e| {
                                        if matches!(e.kind, crate::tui::app::EntryKind::User) {
                                            Some(e.text.clone())
                                        } else {
                                            None
                                        }
                                    })
                                    .unwrap_or_default();
                                match oxideclaw::autocommit::snapshot_turn_raw(
                                    &config.cwd,
                                    &config.auto_commit.message_prefix,
                                    &session.id,
                                    &prompt,
                                    turn_index,
                                    &mut session.meta.auto_commits,
                                    &mut session.meta.undo_position,
                                ) {
                                    Ok(oxideclaw::autocommit::SnapshotOutcome::Committed { sha, files }) => {
                                        tracing::info!(
                                            "autoCommit: turn {turn_index} committed ({files} files, sha={})",
                                            &sha[..7.min(sha.len())]
                                        );
                                        if let Err(e) = session.save_meta().await {
                                            tracing::warn!("autoCommit: failed to save meta after snapshot: {e}");
                                        }
                                    }
                                    Ok(oxideclaw::autocommit::SnapshotOutcome::NoChanges) => {
                                        tracing::debug!("autoCommit: turn {turn_index} had no file changes");
                                    }
                                    Ok(oxideclaw::autocommit::SnapshotOutcome::Disabled { reason }) => {
                                        tracing::debug!("autoCommit: disabled ({reason})");
                                    }
                                    Ok(oxideclaw::autocommit::SnapshotOutcome::Conflict { reason }) => {
                                        // Must be visible, not just logged: this turn is absent
                                        // from the undo history, so /undo will silently skip it
                                        // if the user is never told.
                                        tracing::warn!("autoCommit: {reason}");
                                        app.entries.push(crate::tui::app::ChatEntry::error(
                                            format!("⚠ Auto-commit conflict — this turn was not added to /undo history.\n{reason}"),
                                        ));
                                    }
                                    Err(e) => {
                                        tracing::warn!("autoCommit: snapshot failed: {e}");
                                    }
                                }
                            }
                        }
                        AppEvent::Compacted { ref replacement, summary_len } => {
                            consecutive_compact_count = 0; // successful compact resets thrash counter
                            messages = replacement.clone();
                            if !config.no_session_persistence {
                                let to_save = replacement.clone();
                                saved_count = to_save.len();
                                let _ = session.overwrite(&to_save).await;
                            }
                            app.apply(AppEvent::Compacted {
                                replacement: replacement.clone(),
                                summary_len,
                            });
                        }
                        AppEvent::VoiceBrowse(ref goal) => {
                            // Voice always uses Pattern policy — never Yolo (too easy
                            // to mis-transcribe destructive commands).
                            let goal_str = goal.clone();
                            app.apply(ev);
                            let max = config.browse_max_steps;
                            app.entries.push(ChatEntry::system(format!(
                                "🌐 /browse (voice) — goal: {goal_str} (max {max} steps, policy: Pattern)"
                            )));
                            app.scroll_to_bottom();
                            app.start_loading();
                            let (progress_tx, progress_rx) = tokio::sync::mpsc::channel(64);
                            let (approval_tx, approval_rx) = tokio::sync::mpsc::channel(4);
                            app.browse_progress_rx = Some(progress_rx);
                            app.browse_approval_rx = Some(approval_rx);
                            let current_url = std::sync::Arc::new(tokio::sync::Mutex::new(String::new()));
                            let cfg = config.clone();
                            let all_tools = tools.to_vec();
                            let browser_session = app.browser_session.clone();
                            let browse_req = crate::browser::browse_loop::BrowseRequest {
                                goal: goal_str,
                                policy: crate::browser::browse_loop::BrowsePolicy::Pattern,
                                max_steps: max,
                                voice: true,
                            };
                            let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                            tokio::spawn(async move {
                                let channels = crate::browser::browse_loop::BrowseChannels { progress_tx, approval_tx, cancel };
                                let result = crate::browser::browse_loop::run_browse(
                                    browse_req, &cfg, all_tools, current_url, browser_session, channels,
                                ).await;
                                if let Err(e) = result {
                                    eprintln!("Voice browse error: {e}");
                                }
                            });
                        }
                        other => app.apply(other),
                    }
                    match rx.try_recv() {
                        Ok(next) => ev = next,
                        Err(_) => break,
                    }
                }
            }

            // Terminal keyboard / mouse / paste events
            Some(Ok(term_ev)) = term_events.next() => {
                match term_ev {
                    Event::Key(key) => {
                        handle_key(KeyCtx {
                            key,
                            app: &mut app,
                            messages: &mut messages,
                            client: &mut client,
                            tools: &tools,
                            config: &mut config,
                            perm_state: &perm_state,
                            skills: &skills,
                            system_prompt: &mut system_prompt,
                            tx: &tx,
                            todo_state: &todo_state,
                            session: &mut session,
                            saved_count: &mut saved_count,
                            mcp_statuses: &mcp_statuses,
                            turn_counter: &mut turn_counter,
                            spawn_registry: &spawn_registry,
                        }).await?;
                    }
                    Event::Mouse(mouse) => {
                        match mouse.kind {
                            MouseEventKind::ScrollUp => {
                                if app.overlay.is_some() {
                                    if let Some(o) = &mut app.overlay { o.scroll_up(); }
                                } else {
                                    app.follow_bottom = false;
                                    app.scroll = app.scroll.saturating_sub(3);
                                }
                            }
                            MouseEventKind::ScrollDown => {
                                if app.overlay.is_some() {
                                    if let Some(o) = &mut app.overlay { o.scroll += 3; }
                                } else {
                                    app.scroll += 3;
                                    // re-enable follow if we've scrolled to bottom
                                    // (render.rs will clamp and set follow_bottom automatically)
                                }
                            }
                            _ => {}
                        }
                    }
                    Event::Paste(text) => {
                        for ch in text.chars() {
                            app.insert_char(ch);
                        }
                    }
                    Event::Resize(cols, rows) => {
                        last_term_cols = cols;
                        last_term_rows = rows;
                        // Always clear old content on resize — otherwise stale lines
                        // from the previous viewport size leave ghost artifacts.
                        drop(terminal);
                        execute!(
                            io::stdout(),
                            crossterm::terminal::Clear(crossterm::terminal::ClearType::All),
                            crossterm::cursor::MoveTo(0, 0),
                        )?;
                        let needed = viewport_height(&app, cols, rows);
                        terminal = make_terminal(needed)?;
                        current_vp_h = needed;
                    }
                    _ => {}
                }
            }

            // Heartbeat — ensures periodic redraws for cursor blink / animations
            _ = tokio::time::sleep(Duration::from_millis(50)) => {}
        }

        // Auto-compact after API turn completes
        if !app.is_loading && last_tokens_in > 0 {
            match compact_needed(last_tokens_in) {
                CompactNeeded::None => {}
                CompactNeeded::Warn => {
                    let pct = last_tokens_in * 100 / 200_000;
                    app.entries.push(ChatEntry::system(format!(
                        "Context ~{pct}% full ({last_tokens_in} tokens). Run /compact.",
                    )));
                    last_tokens_in = 0;
                }
                CompactNeeded::Snip => {
                    if config.auto_compact_enabled {
                        snip_compact(&mut messages);
                        app.entries.push(ChatEntry::system(
                            "Auto-compacted (snip): stripped old tool results.",
                        ));
                    } else {
                        app.entries
                            .push(ChatEntry::system("Context near limit. Run /compact."));
                    }
                    last_tokens_in = 0;
                }
                CompactNeeded::Summarise => {
                    if config.auto_compact_enabled {
                        consecutive_compact_count += 1;
                        if consecutive_compact_count >= 3 {
                            app.entries.push(ChatEntry::error(
                                "Autocompact thrash detected: context refilled to the limit \
                                 3 times in a row. The conversation may be too large to compact \
                                 effectively. Start a new session (/clear) or run /compact manually."
                                .to_string()
                            ));
                            consecutive_compact_count = 0;
                        } else {
                            snip_compact(&mut messages); // immediate safety snip
                            app.entries
                                .push(ChatEntry::system("Auto-compacting (summarise)…"));
                            // PreCompact hooks
                            if let Some(hook_cfg) = &config.hooks
                                && !config.disable_all_hooks
                            {
                                hooks::run_pre_compact_hooks(hook_cfg, &session.id, &config.cwd)
                                    .await;
                            }
                            let c2 = client.clone();
                            let msgs = messages.clone();
                            let cfg = config.clone();
                            let tx2 = tx.clone();
                            let sid = session.id.clone();
                            let cwd = config.cwd.clone();
                            let hook_cfg_clone = config.hooks.clone();
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
                                        // PostCompact hooks
                                        if let Some(hook_cfg) = &hook_cfg_clone
                                            && !cfg.disable_all_hooks
                                        {
                                            hooks::run_post_compact_hooks(hook_cfg, &sid, &cwd)
                                                .await;
                                        }
                                        let _ = tx2.send(AppEvent::Compacted {
                                            replacement: r,
                                            summary_len,
                                        });
                                    }
                                    Err(e) => {
                                        let _ = tx2
                                            .send(AppEvent::Error(format!("Compact failed: {e}")));
                                    }
                                }
                            });
                        } // end thrash-check else
                    } else {
                        app.entries
                            .push(ChatEntry::system("Context critically full! Run /compact."));
                    }
                    last_tokens_in = 0;
                }
            }
        }

        if app.should_quit {
            // ── Security: fail any pending tool-approval to Deny ────────────
            // If a tool permission prompt is still on screen when we start
            // shutting down, we MUST drop its oneshot::Sender so the awaiting
            // tool-executor task resolves to Deny (via the Err(_) branch in
            // the PermissionRequest handler above). This prevents the
            // terminal-close = auto-approve bug class (see [redacted] #17276).
            //
            // Dropping the PendingPermission is equivalent to "Deny" because
            // the executor side already maps Err(_) on reply_rx to Deny.
            // We do the same for PendingUserQuestion so AskUser prompts
            // don't deadlock the shutdown path.
            if app.pending_permission.take().is_some() {
                app.entries
                    .push(ChatEntry::system("Shutdown: pending tool approval denied."));
            }
            if app.pending_user_question.take().is_some() {
                app.entries
                    .push(ChatEntry::system("Shutdown: pending question cancelled."));
            }

            // Stop hooks — fire before exiting
            if let Some(hook_cfg) = &config.hooks
                && !config.disable_all_hooks
            {
                hooks::run_stop_hooks(hook_cfg, &session.id, &config.cwd).await;
            }

            // Background spawn agents: cancel what is running and remove its
            // worktree (the registry that tracks it dies with us); tell the
            // user where completed, unmerged work is.
            if let Some(msg) = crate::spawn::cleanup_on_exit(&spawn_registry, &config.cwd).await {
                eprintln!("{msg}");
            }
            break;
        }
    }
    Ok(())
}

// ── Key handler ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tui_asker_tests {
    use super::*;

    /// The TUI answers → the gate gets the decision.
    #[tokio::test]
    async fn forwards_the_users_decision() {
        let (tx, mut rx) = mpsc::unbounded_channel::<AppEvent>();
        let asker = TuiAsker { tx };
        let ui = tokio::spawn(async move {
            match rx.recv().await {
                Some(AppEvent::PermissionRequest {
                    tool_name,
                    description,
                    reply,
                }) => {
                    assert_eq!(tool_name, "Bash");
                    assert!(description.contains("ls"));
                    reply.send(PermissionDecision::AlwaysAllow).unwrap();
                }
                _ => panic!("expected a PermissionRequest"),
            }
        });
        let got = asker.ask("Bash", "Bash: ls").await;
        assert_eq!(got, Some(PermissionDecision::AlwaysAllow));
        ui.await.unwrap();
    }

    /// TUI gone (receiver dropped) → `None`, which the gate turns into Deny.
    #[tokio::test]
    async fn a_dead_ui_yields_no_decision() {
        let (tx, rx) = mpsc::unbounded_channel::<AppEvent>();
        drop(rx);
        let asker = TuiAsker { tx };
        assert_eq!(asker.ask("Bash", "Bash: ls").await, None);
    }

    /// TUI received the request but dropped the reply without answering.
    #[tokio::test]
    async fn an_unanswered_request_yields_no_decision() {
        let (tx, mut rx) = mpsc::unbounded_channel::<AppEvent>();
        let asker = TuiAsker { tx };
        let ui = tokio::spawn(async move {
            let ev = rx.recv().await;
            drop(ev);
        });
        assert_eq!(asker.ask("Bash", "Bash: ls").await, None);
        ui.await.unwrap();
    }
}

#[cfg(test)]
mod short_id_tests {
    use super::short_id;

    #[test]
    fn short_ids_and_multibyte_ids_do_not_panic() {
        assert_eq!(short_id("ab", 8), "ab");
        assert_eq!(short_id("0123456789abcdef", 8), "01234567");
        assert_eq!(
            short_id("日本語のセッション", 8),
            "日本語のセッション"[..24].to_string()
        );
        assert_eq!(short_id("", 8), "");
    }
}
