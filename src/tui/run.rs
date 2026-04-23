/// Main TUI loop — ratatui alternate screen, event-driven (no fixed tick).
use crate::api::types::*;
use crate::api::{ApiBackend, MessagesRequest};
use crate::commands::{CommandAction, CommandContext, dispatch};
use crate::compact::{CompactNeeded, compact_needed, snip_compact, summarize_compact};
use crate::config::Config;
use crate::hooks;
use crate::mcp::{McpManager, mcp_dyn_tools};
use crate::permissions::{CheckResult, PermissionDecision, PermissionState, describe_tool_call};
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

pub async fn run_tui(config: Config, resume_id: Option<String>) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    // Clear the visible screen and anchor cursor at top-left so the compact
    // inline viewport always starts at the top of the terminal window —
    // matching the TS rustyclaw/Ink behaviour where the banner appears right
    // below the launch command regardless of where the cursor was.
    // Previous content remains in the scrollback buffer (not deleted).
    execute!(
        stdout,
        crossterm::terminal::Clear(crossterm::terminal::ClearType::All),
        crossterm::cursor::MoveTo(0, 0),
        EnableBracketedPaste,
        EnableMouseCapture,
    )?;
    let result = run_loop(config, resume_id).await;
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
/// right below the box (matching TS rustyclaw/Ink compact behaviour).
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

/// Install a plugin from npm and register it as an MCP server.
/// `spec` is either "marketplace:<user/repo>" or a direct npm package spec (e.g. "context-mode@context-mode").
/// Check git log for the current commit hash, then fetch the latest release from GitHub.
async fn upgrade_check_task(tx: tokio::sync::mpsc::UnboundedSender<AppEvent>) {
    let current_version = env!("CARGO_PKG_VERSION");

    // Get current git commit hash (best-effort)
    let git_hash = tokio::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .current_dir(std::env::current_dir().unwrap_or_default())
        .output()
        .await
        .ok()
        .and_then(|o| {
            if o.status.success() {
                String::from_utf8(o.stdout)
                    .ok()
                    .map(|s| s.trim().to_string())
            } else {
                None
            }
        });

    // Fetch latest RustyClaw release from GitHub API
    let latest = async {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(8))
            .user_agent("rustyclaw")
            .build()?;
        let resp: serde_json::Value = client
            .get("https://api.github.com/repos/ForkedInTime/RustyClaw/releases/latest")
            .send()
            .await?
            .json()
            .await?;
        anyhow::Ok(resp["tag_name"].as_str().unwrap_or("unknown").to_string())
    }
    .await;

    let hash_str = git_hash.map(|h| format!(" ({})", h)).unwrap_or_default();
    let msg = match latest {
        Ok(tag) => format!(
            "Upgrade Check\n\n\
             rustyclaw v{current_version}{hash_str}\n\
             Latest release: {tag}\n\n\
             To rebuild from source:\n\
               cd ~/Projects/RustyClaw\n\
               git pull\n\
               cargo build --release"
        ),
        Err(_) => format!(
            "Upgrade Check\n\n\
             rustyclaw v{current_version}{hash_str}\n\
             (Could not reach GitHub — check your connection)\n\n\
             To rebuild from source:\n\
               cd ~/Projects/RustyClaw\n\
               git pull\n\
               cargo build --release"
        ),
    };
    let _ = tx.send(AppEvent::UpgradeCheckDone { message: msg });
}

/// Detect the best available JS package manager: bun > pnpm > npm.
fn detect_package_manager() -> (&'static str, &'static str) {
    // Returns (command, runner) — e.g. ("bun", "bunx"), ("pnpm", "pnpx"), ("npm", "npx")
    fn has(cmd: &str) -> bool {
        std::process::Command::new(cmd)
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
    if has("bun") {
        ("bun", "bunx")
    } else if has("pnpm") {
        ("pnpm", "pnpx")
    } else {
        ("npm", "npx")
    }
}

async fn plugin_install_task(spec: String, tx: tokio::sync::mpsc::UnboundedSender<AppEvent>) {
    let (is_marketplace, raw_spec) = if let Some(repo) = spec.strip_prefix("marketplace:") {
        (true, repo.to_string())
    } else {
        (false, spec.clone())
    };

    let result: anyhow::Result<String> = async {
        let (pm, pm_runner) = detect_package_manager();

        if is_marketplace {
            // ── Marketplace install: git clone + install deps locally ─────────
            let marketplace_dir = dirs::home_dir()
                .ok_or_else(|| anyhow::anyhow!("Cannot find home directory"))?
                .join(".claude")
                .join("plugins")
                .join("marketplaces");
            std::fs::create_dir_all(&marketplace_dir)?;

            let safe_name = raw_spec.replace('/', "-");
            let clone_dir = marketplace_dir.join(&safe_name);

            // Clone (shallow) — if already cloned, remove and re-clone for a
            // clean state.  We strip .git/ after cloning so updates always
            // start fresh rather than accumulating history.
            if clone_dir.exists() {
                let _ = tokio::fs::remove_dir_all(&clone_dir).await;
            }
            let url = format!("https://github.com/{}.git", raw_spec);
            let output = tokio::process::Command::new("git")
                .args(["clone", "--depth", "1", &url, &clone_dir.to_string_lossy()])
                .output()
                .await
                .map_err(|e| anyhow::anyhow!("git clone failed: {e}"))?;
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                anyhow::bail!("git clone failed:\n{}", stderr.trim());
            }
            // Remove .git/ — we don't need history at runtime and it's
            // the bulk of the cloned data.
            let _ = tokio::fs::remove_dir_all(clone_dir.join(".git")).await;

            // Read package.json for the plugin name
            let pkg_path = clone_dir.join("package.json");
            let npm_name = if pkg_path.exists() {
                let pkg: serde_json::Value = serde_json::from_str(
                    &tokio::fs::read_to_string(&pkg_path).await.unwrap_or_default()
                ).unwrap_or_default();
                pkg["name"].as_str()
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| raw_spec.split('/').next_back().unwrap_or(&raw_spec).to_string())
            } else {
                raw_spec.split('/').next_back().unwrap_or(&raw_spec).to_string()
            };

            // Install dependencies in the cloned directory
            let install_output = tokio::process::Command::new(pm)
                .args(["install"])
                .current_dir(&clone_dir)
                .output()
                .await
                .map_err(|e| anyhow::anyhow!("{pm} install failed: {e}"))?;

            if !install_output.status.success() {
                let stderr = String::from_utf8_lossy(&install_output.stderr);
                anyhow::bail!("{pm} install failed:\n{}", stderr.trim());
            }

            // Build if a build script exists
            let has_build = if let Ok(pkg_str) = tokio::fs::read_to_string(&pkg_path).await {
                let pkg: serde_json::Value = serde_json::from_str(&pkg_str).unwrap_or_default();
                pkg["scripts"]["build"].is_string()
            } else { false };
            if has_build {
                let _ = tokio::process::Command::new(pm)
                    .args(["run", "build"])
                    .current_dir(&clone_dir)
                    .output()
                    .await;
            }

            // Find entry point for MCP server registration
            let bin_path = clone_dir.join("node_modules").join(".bin").join(&npm_name);
            let main_path = clone_dir.join("index.js");
            let server_cfg = if bin_path.exists() {
                serde_json::json!({ "command": bin_path.to_string_lossy().as_ref(), "args": [] })
            } else if main_path.exists() {
                serde_json::json!({ "command": "node", "args": [main_path.to_string_lossy().as_ref()] })
            } else {
                serde_json::json!({ "command": pm_runner, "args": ["-y", &npm_name] })
            };

            // Register MCP server in settings.json
            register_mcp_server(&npm_name, server_cfg).await?;

            // Track in plugins.json
            track_plugin(&npm_name, &raw_spec, true).await?;

            Ok(format!(
                "Plugin '{npm_name}' installed successfully (via {pm}).\n\
                 Registered as MCP server '{npm_name}' in ~/.claude/settings.json.\n\
                 \n\
                 Restart rustyclaw for the plugin to take effect.\n\
                 After restart, verify with: /{npm_name}:ctx-doctor"
            ))
        } else {
            // ── npm/bun/pnpm registry install ────────────────────────────────
            let plugins_dir = dirs::home_dir()
                .ok_or_else(|| anyhow::anyhow!("Cannot find home directory"))?
                .join(".claude")
                .join("plugins");
            std::fs::create_dir_all(&plugins_dir)?;

            let npm_name = raw_spec.split('@').next().unwrap_or(&raw_spec).to_string();

            let output = tokio::process::Command::new(pm)
                .args(["install", "--prefix", &plugins_dir.to_string_lossy(), &raw_spec])
                .output()
                .await
                .map_err(|e| anyhow::anyhow!("{pm} not found — is a JS runtime installed?\n{e}"))?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                anyhow::bail!("{pm} install failed:\n{}", stderr.trim());
            }

            let bin_path = plugins_dir.join("node_modules").join(".bin").join(&npm_name);
            let server_cfg = if bin_path.exists() {
                serde_json::json!({ "command": bin_path.to_string_lossy().as_ref(), "args": [] })
            } else {
                serde_json::json!({ "command": pm_runner, "args": ["-y", &npm_name] })
            };

            register_mcp_server(&npm_name, server_cfg).await?;
            track_plugin(&npm_name, &raw_spec, false).await?;

            Ok(format!(
                "Plugin '{npm_name}' installed successfully (via {pm}).\n\
                 Registered as MCP server '{npm_name}' in ~/.claude/settings.json.\n\
                 \n\
                 Restart rustyclaw for the plugin to take effect."
            ))
        }
    }.await;

    let (success, message) = match result {
        Ok(msg) => (true, msg),
        Err(e) => (false, format!("Plugin install failed:\n{}", e)),
    };
    let _ = tx.send(AppEvent::PluginInstallDone { success, message });
}

/// Register an MCP server entry in ~/.claude/settings.json.
async fn register_mcp_server(name: &str, server_cfg: serde_json::Value) -> anyhow::Result<()> {
    let settings_path = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("Cannot determine home directory"))?
        .join(".claude")
        .join("settings.json");
    let mut json: serde_json::Value = if settings_path.exists() {
        let content = tokio::fs::read_to_string(&settings_path)
            .await
            .unwrap_or_default();
        serde_json::from_str(&content).unwrap_or(serde_json::json!({}))
    } else {
        serde_json::json!({})
    };

    if json.get("mcpServers").is_none() || json["mcpServers"].is_null() {
        json["mcpServers"] = serde_json::json!({});
    }
    json["mcpServers"][name] = server_cfg;
    tokio::fs::write(&settings_path, serde_json::to_string_pretty(&json)?).await?;
    Ok(())
}

/// Track a plugin in ~/.claude/plugins.json.
async fn track_plugin(name: &str, spec: &str, marketplace: bool) -> anyhow::Result<()> {
    let plugins_path = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("Cannot determine home directory"))?
        .join(".claude")
        .join("plugins.json");
    let mut plugins: serde_json::Value = if plugins_path.exists() {
        let content = tokio::fs::read_to_string(&plugins_path)
            .await
            .unwrap_or_default();
        serde_json::from_str(&content).unwrap_or(serde_json::json!({}))
    } else {
        serde_json::json!({})
    };
    plugins[name] = serde_json::json!({
        "spec": spec,
        "marketplace": marketplace,
    });
    tokio::fs::write(&plugins_path, serde_json::to_string_pretty(&plugins)?).await?;
    Ok(())
}

async fn run_loop(mut config: Config, resume_id: Option<String>) -> Result<()> {
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
            "ANTHROPIC_API_KEY is not set.\n\
             Set it with: export ANTHROPIC_API_KEY=sk-ant-...\n\
             To use a local model: --model ollama:<name>\n\
             Or a cloud OpenAI-compatible model: --model groq:<name>, --model openrouter:<name>, ..."
        ));
    }
    let mut client: ApiBackend =
        ApiBackend::new(&config.model, &config.api_key, &config.ollama_host)?;

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

    // Prune old auto-commit shadow refs (keeps the configured number of newest sessions).
    if let Err(e) =
        rustyclaw::autocommit::prune_old_refs(&config.cwd, config.auto_commit.keep_sessions)
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
                    m.id[..8].to_string()
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
                    app.entries
                        .push(ChatEntry::system(format!("Deleted session {}.", &id[..8])));
                    // Refresh the session list overlay + welcome banner
                    if let Ok(list) = Session::list().await {
                        let mut lines = vec![
                            format!("Sessions ({})\n", list.len()),
                            format!("Current: {} ({})\n", session.meta.name, &session.id[..8]),
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
                                &meta.id[..8],
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
                                    m.id[..8].to_string()
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
            match ApiBackend::new(&config.model, &config.api_key, &config.ollama_host) {
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
                let preview_text = "Hi there. This is RustyClaw. Ready whenever you are.";
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
                        app.entries.push(ChatEntry::system(format!(
                            "  Step {n}: {action} {target}"
                        )));
                        app.scroll_to_bottom();
                    }
                    BrowseProgress::Nudge { level, text } => {
                        app.entries.push(ChatEntry::system(format!(
                            "  ⚠ Nudge L{level}: {text}"
                        )));
                        app.scroll_to_bottom();
                    }
                    BrowseProgress::ApprovalNeeded { .. } => {
                        // Handled via approval_rx below
                    }
                    BrowseProgress::Completed(result) => {
                        let icon = if result.achieved { "✅" } else { "⚠" };
                        app.entries.push(ChatEntry::system(format!(
                            "{icon} /browse done ({:?}): {}", result.reason, result.summary
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
                                        .args(["rustyclaw", "Task complete"])
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
                                match rustyclaw::autocommit::snapshot_turn_raw(
                                    &config.cwd,
                                    &config.auto_commit.message_prefix,
                                    &session.id,
                                    &prompt,
                                    turn_index,
                                    &mut session.meta.auto_commits,
                                    &mut session.meta.undo_position,
                                ) {
                                    Ok(rustyclaw::autocommit::SnapshotOutcome::Committed { sha, files }) => {
                                        tracing::info!(
                                            "autoCommit: turn {turn_index} committed ({files} files, sha={})",
                                            &sha[..7.min(sha.len())]
                                        );
                                        if let Err(e) = session.save_meta().await {
                                            tracing::warn!("autoCommit: failed to save meta after snapshot: {e}");
                                        }
                                    }
                                    Ok(rustyclaw::autocommit::SnapshotOutcome::NoChanges) => {
                                        tracing::debug!("autoCommit: turn {turn_index} had no file changes");
                                    }
                                    Ok(rustyclaw::autocommit::SnapshotOutcome::Disabled { reason }) => {
                                        tracing::debug!("autoCommit: disabled ({reason})");
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
                            let browse_req = crate::browser::browse_loop::BrowseRequest {
                                goal: goal_str,
                                policy: crate::browser::browse_loop::BrowsePolicy::Pattern,
                                max_steps: max,
                                voice: true,
                            };
                            let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                            tokio::spawn(async move {
                                let result = crate::browser::browse_loop::run_browse(
                                    browse_req, &cfg, all_tools, current_url, progress_tx, approval_tx, cancel,
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
            break;
        }
    }
    Ok(())
}

// ── Key handler ───────────────────────────────────────────────────────────────

/// Bundle of references `handle_key` needs to mutate app/session state and react
/// to the current key event. Exists purely to keep the argument count below the
/// clippy::too_many_arguments threshold; the body destructures it on entry and
/// uses the fields directly.
struct KeyCtx<'a> {
    key: crossterm::event::KeyEvent,
    app: &'a mut App,
    messages: &'a mut Vec<Message>,
    client: &'a mut ApiBackend,
    tools: &'a [DynTool],
    config: &'a mut Config,
    perm_state: &'a PermissionState,
    skills: &'a std::collections::HashMap<String, crate::skills::Skill>,
    system_prompt: &'a mut String,
    tx: &'a mpsc::UnboundedSender<AppEvent>,
    todo_state: &'a TodoState,
    session: &'a mut Session,
    saved_count: &'a mut usize,
    mcp_statuses: &'a [crate::mcp::types::McpServerStatus],
    turn_counter: &'a mut usize,
    spawn_registry: &'a crate::spawn::SpawnRegistry,
}

async fn handle_key(ctx: KeyCtx<'_>) -> Result<()> {
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
                        match rustyclaw::autocommit::restore_to(
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
                        match rustyclaw::autocommit::restore_to(
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
                                        m.id[..8].to_string()
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
                                        let _ = tx2
                                            .send(AppEvent::Error(format!("Compact failed: {e}")));
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
                        let _ = crate::config::Config::save_user_setting(
                            "model",
                            serde_json::Value::String(model),
                        );
                        *system_prompt = config.build_system_prompt();
                        // Re-create backend when switching between Anthropic ↔ Ollama
                        match ApiBackend::new(&config.model, &config.api_key, &config.ollama_host) {
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
                        let ollama_models =
                            crate::api::list_ollama_models(&config.ollama_host).await;
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
                        lines
                            .push("  ↑↓ select · Enter switch · 1-9 quick pick · Esc close".into());
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
                            lines.push(
                                "  ↑↓ select · Enter run · 1-9 quick pick · Esc close".into(),
                            );
                            app.overlay =
                                Some(Overlay::with_items("help-commands", lines.join("\n"), ids));
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
                                        let real_path = std::path::PathBuf::from(format!(
                                            "/{}",
                                            flat.replace('_', "/")
                                        ));
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
                                        .filter(|m| {
                                            m.id.starts_with(&id_or_prefix)
                                                || m.name == id_or_prefix
                                        })
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
                                                .map(|m| format!("  {} — {}", &m.id[..12], m.name))
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
                                    &session.id[..8]
                                ),
                            ));
                        }
                        Ok(list) => {
                            let mut lines = vec![
                                format!("Sessions ({})\n", list.len()),
                                format!("Current: {} ({})\n", session.meta.name, &session.id[..8]),
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
                                    &meta.id[..8],
                                    meta.name,
                                    preview,
                                    current
                                ));
                                ids.push(meta.id.clone());
                            }
                            lines.push(String::new());
                            lines.push("  ↑↓ select · Enter resume · d delete · 1-9 quick pick · Esc close".into());
                            app.overlay =
                                Some(Overlay::with_items("sessions", lines.join("\n"), ids));
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
                                app.entries.push(ChatEntry::error(format!(
                                    "Failed to list sessions: {e}"
                                )));
                            }
                        }
                    }
                    CommandAction::ExportCurrentSession => {
                        let id = session.id.clone();
                        let name = session.meta.name.clone();
                        let dest = config.cwd.join(format!("session-{}.md", &id[..8]));
                        match Session::export(&id, &dest).await {
                            Ok(path) => {
                                app.overlay = Some(Overlay::new(
                                    "export",
                                    format!("Session '{}' exported to\n{}", name, path.display()),
                                ));
                            }
                            Err(e) => {
                                app.overlay =
                                    Some(Overlay::new("error", format!("Export failed: {e}")));
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
                            app.overlay =
                                Some(Overlay::new("error", format!("Rename failed: {e}")));
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
                            format!(
                                "Image attached: {path}\nIt will be included in your next message."
                            ),
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
                                "Output style cleared — using default Claude responses."
                                    .to_string(),
                            ));
                        } else {
                            let styles = crate::config::Config::load_output_styles(&config.cwd);
                            if let Some(def) =
                                styles.iter().find(|s| s.name.eq_ignore_ascii_case(&name))
                            {
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
                                    "Unknown style '{}'. Type /output-style to list available styles.", name
                                )));
                            }
                        }
                        app.scroll_to_bottom();
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
                                         --with 'transformers<4.46' --with 'torch<2.6' --with 'torchaudio<2.6'".to_string()
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
                                            let _ = tx2.send(AppEvent::SystemMessage(
                                                format!("XTTS v2 server ready{gpu} — responses will be spoken.")
                                            ));
                                        }
                                        Err(e) => {
                                            let _ = tx2.send(AppEvent::SystemMessage(
                                                format!("XTTS v2 server failed: {e}\nFalling back to CLI mode (slower).")
                                            ));
                                        }
                                    }
                                });
                                app.entries.push(ChatEntry::system(
                                    "TTS on — starting XTTS v2 server (loading model, ~10s)..."
                                        .to_string(),
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
                            app.overlay = Some(Overlay::new("voices", "No voice models found\n\n\
                                 Install XTTS v2:\n\
                                   uv tool install TTS --python 3.11 \\\n\
                                     --with 'transformers<4.46' --with 'torch<2.6' --with 'torchaudio<2.6'\n\n\
                                 Then record a custom voice:\n\
                                   /voice clone".to_string()));
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
                            lines.push(
                                "  ↑↓ select · Enter preview & set · 1-9 quick pick · Esc close"
                                    .into(),
                            );
                            app.overlay =
                                Some(Overlay::with_items("voices", lines.join("\n"), ids));
                        }
                    }
                    CommandAction::PreviewVoiceModel(path) => {
                        // Direct set without picker (future use)
                        app.pending_voice_model = Some(path);
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
                        let msg = crate::sandbox::sandbox_status(enabled, &config.sandbox_mode);
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
                                    let bars = (((*tin as f64 / max_tokens as f64)
                                        * bar_width as f64)
                                        .round()
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
                        let teleport_path =
                            crate::config::Config::claude_dir().join("teleport.json");
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
                        let teleport_path =
                            crate::config::Config::claude_dir().join("teleport.json");
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
                                    let msgs = serde_json::from_value::<Vec<Message>>(
                                        data.get("messages")?.clone(),
                                    )
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
                        let safe_name =
                            name.replace(|c: char| !c.is_alphanumeric() && c != '-', "-");
                        let dest = config.cwd.join(format!(
                            "share-{}-{}.md",
                            safe_name,
                            &id[..8.min(id.len())]
                        ));
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
                                app.overlay =
                                    Some(Overlay::new("error", format!("Share failed: {e}")));
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
                                        "Clipboard tools not found. Install wl-copy or xclip."
                                            .to_string()
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
                                        &m.id[..8],
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
                    CommandAction::PersistModel => {
                        let _ = crate::config::Config::save_user_setting(
                            "model",
                            serde_json::Value::String(config.model.clone()),
                        );
                        app.entries.push(ChatEntry::system(format!(
                            "Model '{}' saved to settings.json.",
                            config.model
                        )));
                        app.scroll_to_bottom();
                    }
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
                            "Settings reloaded (no changes detected). CLAUDE.md + AGENTS.md refreshed.".to_string()
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
                             Newly installed plugins require a full restart of rustyclaw to activate.",
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
                                let from_marketplace =
                                    info["marketplace"].as_bool().unwrap_or(false);
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
                            && let Ok(mut json) =
                                serde_json::from_str::<serde_json::Value>(&content)
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
                            && let Ok(mut plugins) =
                                serde_json::from_str::<serde_json::Value>(&content)
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
                                "Plugin '{}' removed. Restart rustyclaw to deactivate.",
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
                                 Then restart rustyclaw to activate it."
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
                                    let _ =
                                        tx2.send(crate::tui::events::AppEvent::SystemMessage(msg));
                                }
                                Err(e) => {
                                    let _ = tx2.send(crate::tui::events::AppEvent::SystemMessage(
                                        format!("Index error: {e}"),
                                    ));
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
                                        app.entries.push(ChatEntry::system(
                                            format!("No results for '{query}'. Run /index first to build the index.")
                                        ));
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
                                        app.entries.push(ChatEntry::system(format!(
                                            "RAG search error: {e}"
                                        )));
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
                                        app.entries.push(ChatEntry::system(format!(
                                            "Failed to clear RAG index: {e}"
                                        )));
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
                                    Ok(true) => app.entries.push(ChatEntry::system(format!(
                                        "Memory stored [{cat}]: {text}"
                                    ))),
                                    Ok(false) => app.entries.push(ChatEntry::system(
                                        "Memory skipped — near-duplicate already exists."
                                            .to_string(),
                                    )),
                                    Err(e) => app.entries.push(ChatEntry::system(format!(
                                        "Memory store error: {e}"
                                    ))),
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
                                    app.entries.push(ChatEntry::system(format!(
                                        "No memories matched '{query}'."
                                    )));
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
                                        lines.push(format!(
                                            "  [{}] {} ({})",
                                            r.category, r.value, r.key
                                        ));
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
                                        "No memories stored yet. Use /remember <text> to add one."
                                            .to_string(),
                                    ));
                                }
                                Ok(memories) => {
                                    let mut lines =
                                        vec![format!("Memories ({} total)\n", memories.len())];
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
                        let result = tokio::task::spawn_blocking(move || {
                            git_checkpoint(&cwd, msg.as_deref())
                        })
                        .await;
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
                    CommandAction::ShowCostDashboard => {
                        let text = app.cost_tracker.summary();
                        app.entries.push(ChatEntry::system(text));
                        app.scroll_to_bottom();
                    }
                    CommandAction::SpawnAgent(task) => {
                        let tx2 = tx.clone();
                        let cfg = config.clone();
                        let reg = spawn_registry.clone();
                        let task2 = task.clone();
                        tokio::spawn(async move {
                            match crate::spawn::spawn_agent(task2.clone(), &cfg, &reg, tx2.clone())
                                .await
                            {
                                Ok(id) => {
                                    let _ = tx2.send(AppEvent::SystemMessage(
                                        format!("Spawned agent [{id}]: {task2}\nWorking in background. Use /spawn list to check status."),
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
                    CommandAction::ReviewSpawn(id) => {
                        match crate::spawn::review_agent(spawn_registry, &id) {
                            Ok(text) => app.entries.push(ChatEntry::system(text)),
                            Err(e) => app.entries.push(ChatEntry::error(format!("{e:#}"))),
                        }
                    }
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
                                    let _ = tx2.send(AppEvent::SystemMessage(format!(
                                        "Merge failed: {e:#}"
                                    )));
                                }
                            }
                        });
                        app.entries
                            .push(ChatEntry::system(format!("Merging agent [{id}]...")));
                    }
                    CommandAction::KillSpawn(id) => {
                        match crate::spawn::kill_agent(spawn_registry, &id) {
                            Ok(msg) => app.entries.push(ChatEntry::system(msg)),
                            Err(e) => app.entries.push(ChatEntry::error(format!("{e:#}"))),
                        }
                    }
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
                        let label =
                            format!("XTTS v2 default ({})", crate::voice::XTTS_DEFAULT_SPEAKER);
                        let time_hint = if server_up {
                            "~1 second"
                        } else if xtts_ok {
                            "starting server ~10s, then ~1s"
                        } else {
                            "a few seconds"
                        };
                        let has_clone =
                            crate::voice::voice_clone_sample_path().is_some_and(|p| p.exists());
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
                            if crate::voice::xtts_available()
                                && !crate::voice::xtts_server_running()
                            {
                                let _ = crate::voice::ensure_xtts_server().await;
                            }
                            let test_text = "Hello, I am RustyClaw, your personal coding \
                                assistant. I can help you write, debug, and ship code faster \
                                than ever before.";
                            match crate::voice::speak_default_only(test_text, stop_rx).await {
                                Ok(_) => {
                                    let _ = tx2.send(AppEvent::SystemMessage(
                                        "Voice test complete.".into(),
                                    ));
                                }
                                Err(e) => {
                                    let _ = tx2.send(AppEvent::SystemMessage(format!(
                                        "Voice test failed: {e}"
                                    )));
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
                                    let _ =
                                        tx2.send(AppEvent::SystemMessage(format!("Failed: {e:#}")));
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
                                    let _ = tx2.send(AppEvent::SystemMessage(format!(
                                        "Discard failed: {e:#}"
                                    )));
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
                        } else if !rustyclaw::autocommit::is_git_repo(&config.cwd) {
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
                                    let new_pos =
                                        session.meta.undo_position.saturating_sub(k as usize);
                                    match rustyclaw::autocommit::restore_to(
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
                                            app.entries.push(ChatEntry::system(format!(
                                                "[undo] restore failed: {e}"
                                            )));
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
                                    labels.push("session base (pre-RustyClaw)".to_string());
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
                        } else if !rustyclaw::autocommit::is_git_repo(&config.cwd) {
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
                                    match rustyclaw::autocommit::restore_to(
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
                                            app.entries.push(ChatEntry::system(format!(
                                                "[redo] restore failed: {e}"
                                            )));
                                        }
                                    }
                                }
                                None => {
                                    let mut labels: Vec<String> = Vec::new();
                                    let mut positions: Vec<usize> = Vec::new();
                                    let cur = session.meta.undo_position;
                                    let cur_label = if cur == 0 {
                                        "session base (pre-RustyClaw) ← current".to_string()
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
                    CommandAction::AutoCommitStatus => {
                        let cwd_ok = rustyclaw::autocommit::is_git_repo(&config.cwd);
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
                    CommandAction::Browse { goal, policy, max_steps } => {
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

                        let browse_req = crate::browser::browse_loop::BrowseRequest {
                            goal,
                            policy,
                            max_steps: max,
                            voice: false,
                        };
                        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                        tokio::spawn(async move {
                            let result = crate::browser::browse_loop::run_browse(
                                browse_req,
                                &cfg,
                                all_tools,
                                current_url,
                                progress_tx,
                                approval_tx,
                                cancel,
                            ).await;
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
                                "Browser not enabled — set browserEnabled=true in settings.json."
                                    .to_string(),
                            ));
                        }
                        app.scroll_to_bottom();
                    }
                    CommandAction::Watch(arg) => {
                        match arg.as_deref() {
                            Some("off") | Some("stop") => {
                                if app.watcher.take().is_some() {
                                    app.entries.push(ChatEntry::system(
                                        "Watch mode stopped.",
                                    ));
                                } else {
                                    app.entries.push(ChatEntry::system(
                                        "Watch mode was not active.",
                                    ));
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
                                    let watch_path_result: Result<std::path::PathBuf, String> =
                                        match arg_opt {
                                            None => Ok(config.cwd.clone()),
                                            Some(p) => {
                                                let candidate = std::path::PathBuf::from(p);
                                                let absolute = if candidate.is_absolute() {
                                                    candidate
                                                } else {
                                                    config.cwd.join(candidate)
                                                };
                                                match (
                                                    absolute.canonicalize(),
                                                    config.cwd.canonicalize(),
                                                ) {
                                                    (Ok(canon), Ok(cwd_canon))
                                                        if canon.starts_with(&cwd_canon) =>
                                                    {
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
                                            app.entries.push(ChatEntry::error(format!(
                                                "Watch failed: {e}"
                                            )));
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
                                    app.entries.push(ChatEntry::system(
                                        "No uncommitted changes.",
                                    ));
                                } else {
                                    let files = crate::tui::diff::parse_unified_diff(&diff_text);
                                    let summary: String = files
                                        .iter()
                                        .map(|f| {
                                            format!(
                                                "  {} (+{} -{})",
                                                f.path, f.additions, f.deletions
                                            )
                                        })
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
                                app.entries.push(ChatEntry::error(format!(
                                    "git diff failed: {stderr}"
                                )));
                            }
                            Err(e) => {
                                app.entries.push(ChatEntry::error(format!(
                                    "git diff failed: {e}"
                                )));
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
                return Ok(());
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

fn handle_vim_normal(key: crossterm::event::KeyEvent, app: &mut App) {
    use crossterm::event::KeyCode::*;

    // Ctrl+C always quits
    if key.code == Char('c') && key.modifiers == crossterm::event::KeyModifiers::CONTROL {
        app.should_quit = true;
        return;
    }

    // Handle pending two-char commands (e.g. "dd")
    if let Some(pending) = app.vim_pending.take() {
        // Unknown combinations are silently dropped.
        if let ('d', Char('d')) = (pending, key.code) {
            app.clear_line();
        }
        return;
    }

    match key.code {
        // ── Enter insert mode ──────────────────────────────────────────────────
        Char('i') => app.vim_enter_insert(),
        Char('a') => {
            app.cursor_right();
            app.vim_enter_insert();
        }
        Char('A') => {
            app.cursor_end();
            app.vim_enter_insert();
        }
        Char('I') => {
            app.cursor_home();
            app.vim_enter_insert();
        }

        // ── Horizontal motion ──────────────────────────────────────────────────
        Char('h') | Left => app.cursor_left(),
        Char('l') | Right => app.cursor_right(),
        Char('0') | Home => app.cursor_home(),
        Char('$') | End => {
            // In normal mode $ lands on last char, not past it
            if !app.input.is_empty() {
                app.cursor = app.input.len() - 1;
            }
        }

        // ── Word motion ────────────────────────────────────────────────────────
        Char('w') => app.word_forward(),
        Char('b') => app.word_back(),
        Char('e') => app.word_end(),

        // ── Edit operators ─────────────────────────────────────────────────────
        Char('x') => app.delete_under(),
        // 'd' starts a two-char sequence; 'dd' = clear line
        Char('d') => {
            app.vim_pending = Some('d');
        }

        // ── Chat scrolling (j/k in normal mode scroll chat, not move cursor) ──
        Char('j') | Down => {
            app.follow_bottom = false;
            app.scroll = app.scroll.saturating_add(3);
        }
        Char('k') | Up => {
            app.follow_bottom = false;
            app.scroll = app.scroll.saturating_sub(3);
        }
        Char('G') => app.scroll_to_bottom(),

        // ── Esc in normal mode: clear pending, stay in normal ─────────────────
        Esc => {
            app.vim_pending = None;
        }

        _ => {}
    }
}

// ── API task ──────────────────────────────────────────────────────────────────

// ── Image attachment helper ───────────────────────────────────────────────────

/// Read an image file and return it as a ContentBlock::Image (base64).
/// Try to write `content` to the system clipboard via wl-copy (Wayland) or xclip (X11).
/// Returns true if a clipboard tool was found and succeeded.
async fn try_clipboard_write(content: &str) -> bool {
    use std::process::Stdio;
    // Try wl-copy first (Wayland)
    if let Ok(mut child) = tokio::process::Command::new("wl-copy")
        .stdin(Stdio::piped())
        .spawn()
    {
        if let Some(stdin) = child.stdin.take() {
            use tokio::io::AsyncWriteExt;
            let mut s = stdin;
            let _ = s.write_all(content.as_bytes()).await;
        }
        if child.wait().await.map(|s| s.success()).unwrap_or(false) {
            return true;
        }
    }
    // Fall back to xclip (X11)
    if let Ok(mut child) = tokio::process::Command::new("xclip")
        .args(["-selection", "clipboard"])
        .stdin(Stdio::piped())
        .spawn()
    {
        if let Some(stdin) = child.stdin.take() {
            use tokio::io::AsyncWriteExt;
            let mut s = stdin;
            let _ = s.write_all(content.as_bytes()).await;
        }
        if child.wait().await.map(|s| s.success()).unwrap_or(false) {
            return true;
        }
    }
    false
}

fn attach_image(path: &str) -> AResult<ContentBlock> {
    use std::io::Read;
    let mut f = std::fs::File::open(path)?;
    let mut bytes = Vec::new();
    f.read_to_end(&mut bytes)?;

    let media_type = match std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase())
        .as_deref()
    {
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        _ => "image/png",
    };

    let data = base64_encode(&bytes);
    Ok(ContentBlock::Image {
        source: ImageSource::Base64 {
            media_type: media_type.to_string(),
            data,
        },
    })
}

/// Minimal base64 encoder (avoids pulling in a whole crate for a simple use case).
fn base64_encode(data: &[u8]) -> String {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as usize;
        let b1 = if chunk.len() > 1 {
            chunk[1] as usize
        } else {
            0
        };
        let b2 = if chunk.len() > 2 {
            chunk[2] as usize
        } else {
            0
        };
        out.push(CHARS[b0 >> 2] as char);
        out.push(CHARS[((b0 & 3) << 4) | (b1 >> 4)] as char);
        out.push(if chunk.len() > 1 {
            CHARS[((b1 & 0xf) << 2) | (b2 >> 6)] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            CHARS[b2 & 0x3f] as char
        } else {
            '='
        });
    }
    out
}

// ── API task ──────────────────────────────────────────────────────────────────

/// Maximum number of tool-use→response cycles per user turn.
/// Prevents runaway loops where Claude repeatedly calls tools without finishing.
const MAX_TOOL_ITERATIONS: u32 = 50;

/// Destructive tools blocked when plan mode is active.
const PLAN_MODE_BLOCKED_TOOLS: &[&str] = &["Bash", "Write", "Edit", "MultiEdit", "EnterWorktree"];

/// Owned bundle handed to `run_api_task` when a user turn kicks off a new
/// conversation-streaming task. Exists to stay under the clippy argument cap;
/// the function destructures it immediately.
struct ApiTask {
    client: ApiBackend,
    tools: Vec<DynTool>,
    messages: Vec<Message>,
    config: Config,
    perm_state: PermissionState,
    system_prompt: String,
    tx: mpsc::UnboundedSender<AppEvent>,
    plan_mode: bool,
    session_id: String,
}

async fn run_api_task(task: ApiTask) {
    let ApiTask {
        client,
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

        // Build system content — wrap in blocks for prompt caching if enabled
        let system_content = if config.prompt_cache {
            crate::api::types::SystemContent::Blocks(vec![crate::api::types::SystemBlock {
                block_type: "text".into(),
                text: system_prompt.clone(),
                cache_control: Some(crate::api::types::CacheControl::ephemeral()),
            }])
        } else {
            crate::api::types::SystemContent::Plain(system_prompt.clone())
        };

        // Extended thinking config
        let (thinking_cfg, mut betas) = if let Some(budget) = config.thinking_budget_tokens {
            if !crate::api::is_ollama_model(&config.model) {
                (
                    Some(crate::api::types::ThinkingConfig::enabled(budget)),
                    vec!["interleaved-thinking-2025-05-14".to_string()],
                )
            } else {
                (None, vec![])
            }
        } else {
            (None, vec![])
        };
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
            max_tokens: config.max_tokens_for(&config.model),
            system: system_content,
            messages: budgeted_messages,
            tools: tool_defs,
            stream: None,
            thinking: thinking_cfg,
            betas,
            session_id: Some(session_id.to_string()),
        };

        // API call with retry for transient errors (429/529/connection)
        const MAX_RETRIES: u32 = 3;
        let mut attempt = 0u32;
        let mut should_compact_retry = false;
        let response = loop {
            attempt += 1;
            let tx2 = tx.clone();
            let req_clone = request.clone();
            let result = client
                .messages_stream(req_clone, move |chunk| {
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

                    // Retryable errors: rate limit (429), overloaded (529), connection issues
                    let is_retryable = err_str.contains("429")
                        || err_str.contains("529")
                        || err_str.contains("overloaded")
                        || err_str.contains("rate_limit")
                        || err_str.contains("connection")
                        || err_str.contains("timed out")
                        || err_str.contains("reset by peer");

                    if is_retryable && attempt < MAX_RETRIES {
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
                ctx.live_ollama_host = Some(config.ollama_host.clone());
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

                        // Autonomy check: "suggest" mode forces Ask for Write/Edit
                        let autonomy_override = if config.autonomy == "suggest"
                            && (name == "Write" || name == "Edit")
                        {
                            Some(CheckResult::Ask)
                        } else {
                            None
                        };

                        // Permission check — for Bash, parse compound commands (&&, ||, ;, |)
                        // so each sub-command is checked individually against prefix rules
                        let decision = match autonomy_override.unwrap_or_else(|| {
                            if name == "Bash" {
                                if let Some(cmd) = input.get("command").and_then(|c| c.as_str()) {
                                    crate::permissions::check_compound_bash(&perm_state, cmd)
                                } else {
                                    perm_state.check_with_input(name, Some(input))
                                }
                            } else {
                                perm_state.check_with_input(name, Some(input))
                            }
                        }) {
                            CheckResult::Allow => PermissionDecision::Allow,
                            CheckResult::Deny => PermissionDecision::Deny,
                            CheckResult::Ask => {
                                let desc = describe_tool_call(name, input);
                                let (reply_tx, reply_rx) = oneshot::channel();
                                if tx
                                    .send(AppEvent::PermissionRequest {
                                        tool_name: name.clone(),
                                        description: desc,
                                        reply: reply_tx,
                                    })
                                    .is_err()
                                {
                                    return;
                                }
                                // Security contract: if the oneshot sender is
                                // dropped without a value, we MUST treat this
                                // as Deny. This fires on TUI shutdown, panic,
                                // terminal close (SIGHUP), or runtime tear-down,
                                // preventing the "close terminal = auto-approve"
                                // bug ([redacted] #17276).
                                match reply_rx.await {
                                    Ok(d) => d,
                                    Err(_) => PermissionDecision::Deny,
                                }
                            }
                        };

                        if decision == PermissionDecision::AlwaysAllow {
                            perm_state.record_always_allow(name);
                        }
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
fn git_checkpoint(cwd: &std::path::Path, message: Option<&str>) -> anyhow::Result<String> {
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
    let msg = message.unwrap_or("rustyclaw checkpoint");
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
fn chrono_free_timestamp() -> String {
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

fn days_to_ymd(mut days: u64) -> (u64, u64, u64) {
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

fn is_leap(y: u64) -> bool {
    (y.is_multiple_of(4) && !y.is_multiple_of(100)) || y.is_multiple_of(400)
}
