/// Slash command dispatch — port of commands/ directory.
///
/// Each `/command [args]` typed by the user is matched here and returns a
/// `CommandAction` telling the run-loop what to do.  The run-loop owns all
/// mutable state (App, Config, messages) so commands that need to mutate
/// state return a variant instead of doing it directly.
use crate::config::Config;
use crate::mcp::types::McpServerStatus;
use crate::skills::Skill;
use crate::tools::todo::{TodoPriority, TodoState, TodoStatus};
use std::collections::HashMap;

// ── Slash command list (used for Tab completion) ──────────────────────────────

pub const SLASH_COMMANDS: &[&str] = &[
    "add-dir",
    "advisor",
    "agents",
    "banner",
    "branch",
    "brief",
    "browse",
    "browser",
    "btw",
    "budget",
    "clear",
    "compact",
    "commit",
    "commit-push-pr",
    "config",
    "context",
    "copy",
    "cost",
    "ctx-viz",
    "diff",
    "doctor",
    "edit-claude-md",
    "effort",
    "env",
    "exit",
    "export",
    "install-missing",
    "fast",
    "feedback",
    "files",
    "help",
    "hooks",
    "ide",
    "image",
    "index",
    "init",
    "init-verifiers",
    "insights",
    "keybindings",
    "login",
    "logout",
    "mcp",
    "memory",
    "model",
    "notifications",
    "output-style",
    "permissions",
    "plan",
    "plugin",
    "pr_comments",
    "quit",
    "release-notes",
    "reload-plugins",
    "rename",
    "resume",
    "rewind",
    "review",
    "sandbox",
    "screenshot",
    "security-review",
    "session",
    "share",
    "skills",
    "stats",
    "status",
    "statusline",
    "summary",
    "tasks",
    "teleport",
    "terminal-setup",
    "theme",
    "thinkback",
    "ultraplan",
    "upgrade",
    "rag",
    "remember",
    "router",
    "usage",
    "version",
    "vim",
    "watch",
    "voice",
    "autofix-pr",
    "issue",
    "color",
    "forget",
    "powerup",
    "undo",
    "redo",
    "autocommit",
];

// ── Model catalogue ───────────────────────────────────────────────────────────

pub const KNOWN_MODELS: &[(&str, &str)] = &[
    ("claude-opus-4-6", "Most capable — best for complex tasks"),
    ("claude-sonnet-4-6", "Smart & fast — recommended (default)"),
    (
        "claude-haiku-4-5-20251001",
        "Fastest & cheapest — great for simple tasks",
    ),
    ("claude-opus-4-5", "Previous Opus generation"),
    ("claude-sonnet-4-5", "Previous Sonnet generation"),
];

// Cost per million tokens (input, output) in USD
fn model_pricing(model: &str) -> (f64, f64) {
    if crate::api::is_ollama_model(model) {
        (0.0, 0.0) // Local — free
    } else if model.contains("opus-4") {
        (15.0, 75.0)
    } else if model.contains("sonnet-4") {
        (3.0, 15.0)
    } else if model.contains("haiku") {
        (0.25, 1.25)
    } else {
        (3.0, 15.0) // fallback to sonnet pricing
    }
}

// ── Command action ────────────────────────────────────────────────────────────

/// What the run-loop should do after a slash command is dispatched.
#[allow(dead_code)] // some variants are built but not yet wired to commands
pub enum CommandAction {
    /// Display a system message in the chat panel
    Message(String),
    /// Quit the TUI
    Quit,
    /// Clear conversation history and chat panel
    Clear,
    /// Change the active model
    SetModel(String),
    /// Toggle vim editing mode
    ToggleVim,
    /// Trigger a compact cycle
    Compact,
    /// Send this text as a user prompt to Claude
    SendPrompt(String),
    /// Remove last N user+assistant exchange pairs from history
    Rewind(usize),
    /// Resume a session by ID
    ResumeSession(String),
    /// Rename current session
    RenameSession(String),
    /// List sessions async (run_loop does the I/O and shows overlay)
    ListSessions,
    /// Delete all sessions except the current one
    ClearAllSessions,
    /// Export current session to markdown (run_loop does the I/O)
    ExportCurrentSession,
    /// Toggle plan mode (read-only: destructive tools blocked)
    TogglePlanMode,
    /// Attach an image to the next user message
    AttachImage(String),
    /// Toggle brief/concise response mode
    ToggleBriefMode,
    /// Set a "by the way" note to prepend to the next user message
    SetBtwNote(String),
    /// Set the active output style by name ("default" to clear)
    SetOutputStyle(String),
    /// Set the active UI theme ("dark", "light", "solarized")
    SetTheme(String),
    /// Enable or disable voice input mode
    SetVoiceEnabled(bool),
    /// Enable or disable TTS (XTTS v2 voice output)
    SetTtsEnabled(bool),
    /// Run a system package-manager install command interactively (drops raw mode)
    RunInstall(String),
    /// Enable or disable sandbox mode with the given mode string
    SetSandboxEnabled { enabled: bool, mode: String },
    /// Show session statistics overlay (thinkback)
    ShowThinkback,
    /// Export session context to teleport file
    TeleportExport,
    /// Import session context from teleport file
    TeleportImport,
    /// Share session as a detailed markdown export
    ShareSession,
    /// Share session by copying markdown to clipboard (xclip/wl-copy)
    ShareClipboard,
    /// Enable or disable desktop notifications + terminal bell
    SetNotificationsEnabled(bool),
    /// Toggle sandbox network access
    SetSandboxNetwork(bool),
    /// Open CLAUDE.md in $EDITOR
    EditClaudeMd,
    /// Search sessions by query string
    SearchSessions(String),
    /// Persist the current model to settings.json
    PersistModel,
    /// Install a plugin. Spec is "marketplace:<user/repo>" or a direct npm package spec.
    PluginInstall(String),
    /// Remove a plugin by name
    PluginRemove(String),
    /// List installed plugins
    PluginList,
    /// Reload settings.json (model, theme, effort, spinner, etc.) without restart
    ReloadSettings,
    /// Reload all plugin MCP servers (re-reads settings.json)
    ReloadPlugins,
    /// Execute a plugin slash command (`<plugin>:<command>`)
    PluginCommand { plugin: String, command: String },
    /// Show interactive model picker (async — needs Ollama query)
    ListModels,
    /// Show interactive voice model picker
    ListVoiceModels,
    /// Set the voice model and play a preview
    PreviewVoiceModel(String),
    /// Show interactive help category picker
    ListHelp,
    /// Show help for a specific category by index
    ShowHelpCategory(usize),
    /// Async GitHub version check
    CheckUpgrade,
    /// Open a URL in the default browser
    OpenBrowser(String),
    /// Trigger RAG codebase indexing (force = re-index everything)
    IndexProject { force: bool },
    /// Search the RAG index and display results
    RagSearch(String),
    /// Show RAG index statistics
    RagStatus,
    /// Clear the RAG index
    RagClear,
    /// Set session budget limit in USD
    SetBudget(Option<f64>),
    /// Toggle smart model router on/off, or configure tiers
    RouterToggle,
    /// Show router config and status
    RouterStatus,
    /// Set a specific router tier model
    RouterSetTier { tier: String, model: String },
    /// Create a git checkpoint commit (auto-stash of working changes)
    GitCheckpoint(Option<String>),
    /// Set autonomy level: "suggest", "auto-edit", "full-auto"
    SetAutonomy(String),
    /// Show cost dashboard
    ShowCostDashboard,
    /// Spawn a background agent in a git worktree
    SpawnAgent(String),
    /// List all spawned background agents
    ListSpawns,
    /// Review a completed spawn's changes
    ReviewSpawn(String),
    /// Merge a completed spawn into the current branch
    MergeSpawn(String),
    /// Cancel a running spawn
    KillSpawn(String),
    /// Discard a spawn's worktree without merging
    DiscardSpawn(String),
    /// Start voice clone recording flow (tier selection)
    VoiceClone(String),
    /// Save the just-recorded voice clone sample
    VoiceCloneSave(String),
    /// Remove the voice clone sample
    VoiceCloneRemove,
    /// Play a test phrase with current TTS (XTTS v2)
    VoiceTest,
    /// Add a text snippet to persistent memory
    MemoryAdd(String),
    /// Remove memories matching a query string
    MemoryForget(String),
    /// Search persistent memory
    MemorySearch(String),
    /// List all persistent memories grouped by category
    MemoryList,
    /// Clear all persistent memories
    MemoryClear,
    /// Toggle auto-capture of decisions/preferences from assistant responses
    MemoryAutoToggle(bool),
    /// Show the current memory context (top 10 entries)
    MemoryInject,
    /// `/undo [N]` — restore working tree to an earlier auto-commit.
    /// `n == None` opens a picker; `n == Some(k)` rewinds k turns.
    Undo { n: Option<u32> },
    /// `/redo [N]` — restore working tree to a later auto-commit in the redo stack.
    /// `n == None` opens a picker; `n == Some(k)` advances k turns.
    Redo { n: Option<u32> },
    /// `/autocommit [status]` — print auto-commit state to the chat. v1 only supports `status`.
    AutoCommitStatus,
    /// Start an autonomous browser run.
    Browse {
        goal: String,
        policy: crate::browser::browse_loop::BrowsePolicy,
        max_steps: Option<u32>,
    },
    /// Launch browser and optionally navigate to URL
    BrowseUrl(String),
    /// Take a screenshot of the current browser page
    BrowserScreenshot,
    /// Close the browser session
    BrowserClose,
    /// Start / stop / check file watcher.
    /// `None` = start watching cwd with default settings.
    /// `Some("off")` or `Some("stop")` = stop the watcher.
    /// `Some("status")` = report current watch state.
    /// `Some(path)` = start watching a specific path.
    Watch(Option<String>),
    /// Show a diff review overlay.
    /// `None` = show the full `git diff`.
    /// `Some(path)` = show `git diff -- <path>`.
    ShowDiff(Option<String>),
    /// Command not recognised — show error
    Unknown(String),
}

// ── Context passed to every command ──────────────────────────────────────────

#[allow(dead_code)] // fields prepared for future commands that will use them
pub struct CommandContext<'a> {
    pub config: &'a Config,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub vim_mode: bool,
    pub skills: &'a HashMap<String, Skill>,
    pub todo_state: &'a TodoState,
    /// Last assistant message text (for /copy)
    pub last_assistant: Option<&'a str>,
    /// Current session ID
    pub session_id: &'a str,
    /// Current session name
    pub session_name: &'a str,
    /// Merged CLAUDE.md content (empty if none loaded)
    pub claudemd: &'a str,
    /// MCP server statuses (for /mcp command)
    pub mcp_statuses: &'a [McpServerStatus],
    /// Whether brief/concise mode is currently active
    pub brief_mode: bool,
    /// Current btw note (if any)
    pub btw_note: Option<&'a str>,
}

// ── Dispatcher ────────────────────────────────────────────────────────────────

/// Parse `input` (which starts with '/') and return the action to perform.
pub fn dispatch(input: &str, ctx: &CommandContext) -> CommandAction {
    let input = input.trim_start_matches('/');
    let (name, args) = split_first_word(input);

    match name {
        // ── Already handled in run.rs for special reasons, but also here ──
        "exit" | "quit" => CommandAction::Quit,
        "clear" => CommandAction::Clear,
        "compact" => CommandAction::Compact,

        // ── New commands ──────────────────────────────────────────────────
        "banner" => cmd_banner(args),
        "version" => cmd_version(),
        "status" => cmd_status(ctx),
        "cost" => cmd_cost(ctx),
        "context" => cmd_context(ctx),
        "config" => cmd_config(ctx),
        "files" => cmd_files(ctx),
        "model" => cmd_model(args, ctx),
        "vim" => CommandAction::ToggleVim,
        "keybindings" => cmd_keybindings(),
        "doctor" => cmd_doctor(ctx),
        "install-missing" => cmd_install_missing(),
        "init" => cmd_init(ctx),
        "watch" => {
            let a = args.trim();
            CommandAction::Watch(if a.is_empty() { None } else { Some(a.to_string()) })
        }
        "diff" => {
            let a = args.trim();
            CommandAction::ShowDiff(if a.is_empty() { None } else { Some(a.to_string()) })
        }
        "permissions" => cmd_permissions(ctx),
        "skills" => cmd_skills(ctx),
        "review" => cmd_review(args),
        "tasks" => cmd_tasks(ctx),
        "copy" => cmd_copy(ctx),
        "rewind" => CommandAction::Rewind(args.parse::<usize>().unwrap_or(1)),
        "undo" => cmd_undo(args),
        "redo" => cmd_redo(args),
        "autocommit" => cmd_autocommit(args),
        "browser" => {
            let url = args.trim().to_string();
            if url == "close" {
                CommandAction::BrowserClose
            } else if url.is_empty() {
                CommandAction::BrowseUrl("about:blank".to_string())
            } else {
                CommandAction::BrowseUrl(url)
            }
        }
        "browse" => parse_browse_command(args.trim()),
        "screenshot" => CommandAction::BrowserScreenshot,
        "branch" => cmd_branch(ctx),
        "summary" => CommandAction::SendPrompt(
            "Please give a brief summary of our conversation so far — what we've discussed, \
             decisions made, and current state of any work in progress. Keep it concise."
                .into(),
        ),
        "checkpoint" => CommandAction::GitCheckpoint(if args.is_empty() {
            None
        } else {
            Some(args.to_string())
        }),
        "lint" => cmd_lint(ctx),
        "autonomy" => cmd_autonomy(args),
        "add-dir" => cmd_add_dir(args, ctx),
        "pr_comments" => cmd_pr_comments(args, ctx),
        "usage" => cmd_usage(ctx),
        "help" => cmd_help(args),

        // ── RAG codebase indexing ────────────────────────────────────────
        "index" => cmd_index(args),
        "rag" => cmd_rag(args),

        // ── Smart model router + cost ───────────────────────────────────
        "budget" => cmd_budget(args),
        "router" => cmd_router(args),

        // ── Session commands ──────────────────────────────────────────────
        "session" => cmd_session(args, ctx),
        "sessions" => cmd_session(args, ctx),
        "resume" => cmd_resume(args),
        "rename" => {
            if args.is_empty() {
                CommandAction::Message("Usage: /rename <new-name>".into())
            } else {
                CommandAction::RenameSession(args.to_string())
            }
        }
        "export" => cmd_export(ctx),
        "mcp" => cmd_mcp(args, ctx),
        "login" | "logout" => CommandAction::Message(
            "Auth management not needed — API key is read from ANTHROPIC_API_KEY.".into(),
        ),
        "theme" => cmd_theme(args, ctx),
        "fast" => CommandAction::Message(
            "Fast mode is not applicable in rustyclaw (streaming is always on).".into(),
        ),
        "plan" => CommandAction::TogglePlanMode,
        "hooks" => cmd_hooks(ctx),
        "image" => cmd_image(args),
        "memory" => cmd_memory_dispatch(args, ctx),
        "remember" => {
            if args.is_empty() {
                CommandAction::Message("Usage: /remember <text to remember>".into())
            } else {
                CommandAction::MemoryAdd(args.to_string())
            }
        }
        "forget" => {
            if args.is_empty() {
                CommandAction::Message("Usage: /forget <query matching memories to remove>".into())
            } else {
                CommandAction::MemoryForget(args.to_string())
            }
        }

        // ── New commands (gap fill) ───────────────────────────────────────
        "commit" => cmd_commit(args, ctx),
        "commit-push-pr" => cmd_commit_push_pr(args, ctx),
        "effort" => cmd_effort(args, ctx),
        "insights" => cmd_insights(ctx),
        "security-review" => cmd_security_review(),
        "ide" => cmd_ide(ctx),
        "env" => cmd_env(ctx),
        "output-style" => cmd_output_style(args, ctx),
        "upgrade" => cmd_upgrade(),
        "advisor" => cmd_advisor(args),
        "brief" => cmd_brief(ctx),
        "btw" => cmd_btw(args),
        "ctx-viz" | "ctx_viz" => cmd_ctx_viz(ctx),
        "init-verifiers" => cmd_init_verifiers(),
        "agents" => cmd_agents(ctx),
        "stats" => cmd_stats(ctx),
        "statusline" => cmd_statusline(args),

        // ── New features (voice, sandbox, thinkback, teleport, share, etc.) ─────
        "voice" => cmd_voice(args, ctx),
        "sandbox" => cmd_sandbox(args, ctx),
        "thinkback" => CommandAction::ShowThinkback,
        "teleport" => cmd_teleport(args),
        "share" => cmd_share(args),
        "notifications" => cmd_notifications(args, ctx),
        "feedback" => cmd_feedback(),
        "terminal-setup" => cmd_terminal_setup(),
        "release-notes" => cmd_release_notes(args),
        "edit-claude-md" => CommandAction::EditClaudeMd,
        "plugin" => cmd_plugin(args),
        "reload" => CommandAction::ReloadSettings,
        "reload-settings" => CommandAction::ReloadSettings,
        "reload-plugins" => CommandAction::ReloadPlugins,
        "spawn" => cmd_spawn(args),
        "ultraplan" => cmd_ultraplan(args),
        "autofix-pr" => cmd_autofix_pr(args),
        "issue" => cmd_issue(args),
        "color" => cmd_color(args),
        "powerup" => cmd_powerup(args),

        other => {
            // Plugin slash commands: /context-mode:ctx-doctor etc.
            if let Some(colon) = other.find(':') {
                CommandAction::PluginCommand {
                    plugin: other[..colon].to_string(),
                    command: other[colon + 1..].to_string(),
                }
            } else {
                CommandAction::Unknown(other.to_string())
            }
        }
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn split_first_word(s: &str) -> (&str, &str) {
    let s = s.trim();
    match s.find(char::is_whitespace) {
        Some(i) => (&s[..i], s[i..].trim()),
        None => (s, ""),
    }
}

/// Parse `/browse` arguments into a `CommandAction::Browse` variant.
///
/// Supported flags (all optional, may appear in any order before the goal):
/// - `--yolo`             → `BrowsePolicy::Yolo`
/// - `--ask`              → `BrowsePolicy::Ask`
/// - `--max-steps <N>`    → `max_steps = Some(N)`
///
/// Remaining tokens after flag removal form the `goal` string.
pub fn parse_browse_command(input: &str) -> CommandAction {
    use crate::browser::browse_loop::BrowsePolicy;
    let mut policy = BrowsePolicy::Pattern;
    let mut max_steps: Option<u32> = None;
    let mut tokens: Vec<&str> = input.split_whitespace().collect();
    let mut i = 0;
    while i < tokens.len() {
        match tokens[i] {
            "--yolo" => {
                policy = BrowsePolicy::Yolo;
                tokens.remove(i);
            }
            "--ask" => {
                policy = BrowsePolicy::Ask;
                tokens.remove(i);
            }
            "--max-steps" if i + 1 < tokens.len() => {
                if let Ok(n) = tokens[i + 1].parse() {
                    max_steps = Some(n);
                }
                tokens.drain(i..=i + 1);
            }
            _ => {
                i += 1;
            }
        }
    }
    let goal = tokens.join(" ").trim().to_string();
    CommandAction::Browse { goal, policy, max_steps }
}

// ── Individual commands ───────────────────────────────────────────────────────

fn cmd_banner(args: &str) -> CommandAction {
    use crate::config::Config;
    let arg = args.trim();

    if arg.is_empty() {
        // Show current setting
        let current = Config::get_banner_label();
        let msg = match &current {
            None => "Banner label: none\n\
                 Usage: /banner <text>   — show custom text (e.g. \"Penguin Corp\")\n\
                 /banner none            — hide extra label (default)"
                .to_string(),
            Some(v) => format!(
                "Banner label: {v}\n\
                 Use /banner none to clear, or /banner <text> to change it."
            ),
        };
        return CommandAction::Message(msg);
    }

    let value = if arg == "none" || arg == "default" {
        "none"
    } else {
        arg
    };

    match Config::set_banner_label(value) {
        Ok(()) => {
            if value == "none" {
                CommandAction::Message("Banner label cleared. Restart to see the change.".into())
            } else {
                CommandAction::Message(format!(
                    "Banner label set to: {value}\nRestart to see the change."
                ))
            }
        }
        Err(e) => CommandAction::Message(format!("Failed to save banner label: {e}")),
    }
}

fn cmd_version() -> CommandAction {
    CommandAction::Message(format!("RustyClaw v{}", env!("CARGO_PKG_VERSION")))
}

fn cmd_status(ctx: &CommandContext) -> CommandAction {
    let cwd = ctx.config.cwd.display().to_string();

    let git_branch = std::process::Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(&ctx.config.cwd)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "not a git repo".into());

    let api_key_status = if ctx.config.api_key.len() >= 4 {
        format!("set ({}...)", &ctx.config.api_key[..4])
    } else if !ctx.config.api_key.is_empty() {
        "set".into()
    } else {
        "not set".into()
    };

    let text = format!(
        "RustyClaw v{ver}\n\
         \n\
         Model:      {model}\n\
         Max tokens: {max_tok}\n\
         CWD:        {cwd}\n\
         Git branch: {branch}\n\
         API key:    {api}\n\
         Vim mode:   {vim}\n\
         Auto-compact: {compact}",
        ver = env!("CARGO_PKG_VERSION"),
        model = ctx.config.model,
        max_tok = ctx.config.max_tokens,
        branch = git_branch,
        api = api_key_status,
        vim = if ctx.vim_mode { "on" } else { "off" },
        compact = if ctx.config.auto_compact_enabled {
            "on"
        } else {
            "off"
        },
    );
    CommandAction::Message(text)
}

fn cmd_cost(ctx: &CommandContext) -> CommandAction {
    if ctx.tokens_in == 0 && ctx.tokens_out == 0 {
        return CommandAction::Message("No tokens used in this session yet.".into());
    }
    let (price_in, price_out) = model_pricing(&ctx.config.model);
    let cost_in = ctx.tokens_in as f64 * price_in / 1_000_000.0;
    let cost_out = ctx.tokens_out as f64 * price_out / 1_000_000.0;
    let total = cost_in + cost_out;

    // Cache pricing: read = 10% of input price, write = 125% of input price (Anthropic prompt cache)
    let cache_read_cost = ctx.cache_read_tokens as f64 * (price_in * 0.10) / 1_000_000.0;
    let cache_write_cost = ctx.cache_write_tokens as f64 * (price_in * 1.25) / 1_000_000.0;
    let cache_hit_pct = if ctx.tokens_in > 0 {
        ctx.cache_read_tokens as f64 * 100.0 / ctx.tokens_in as f64
    } else {
        0.0
    };

    let cache_section = if ctx.cache_read_tokens > 0 || ctx.cache_write_tokens > 0 {
        format!(
            "\n\nPrompt cache\n\
             Cache read:   {} tokens (${:.4}, {:.1}% hit rate)\n\
             Cache write:  {} tokens (${:.4})\n\
             Cache savings: ${:.4}",
            ctx.cache_read_tokens,
            cache_read_cost,
            cache_hit_pct,
            ctx.cache_write_tokens,
            cache_write_cost,
            // savings = what it would have cost at full price minus what was actually charged
            ctx.cache_read_tokens as f64 * price_in / 1_000_000.0 - cache_read_cost,
        )
    } else {
        String::new()
    };

    CommandAction::Message(format!(
        "Session cost estimate\n\
         \n\
         Model:        {model}\n\
         Input tokens: {tin} (${cin:.4})\n\
         Output tokens:{tout} (${cout:.4})\n\
         Total:        ${total:.4}\n\
         \n\
         Prices: ${pin}/1M input, ${pout}/1M output{cache}",
        model = ctx.config.model,
        tin = ctx.tokens_in,
        cin = cost_in,
        tout = ctx.tokens_out,
        cout = cost_out,
        pin = price_in,
        pout = price_out,
        cache = cache_section,
    ))
}

fn cmd_context(ctx: &CommandContext) -> CommandAction {
    let limit: u64 = 200_000;
    let used = ctx.tokens_in;
    let pct = if limit > 0 { used * 100 / limit } else { 0 };
    let bar_len = 30usize;
    let filled = (pct as usize * bar_len / 100).min(bar_len);
    let bar: String = "█".repeat(filled) + &"░".repeat(bar_len - filled);

    CommandAction::Message(format!(
        "Context window usage\n\
         \n\
         [{bar}] {pct}%\n\
         Used:      {used} tokens\n\
         Remaining: {rem} tokens\n\
         Limit:     {limit} tokens\n\
         \n\
         Run /compact to free context space.",
        rem = limit.saturating_sub(used),
    ))
}

fn cmd_config(ctx: &CommandContext) -> CommandAction {
    let settings_paths = crate::settings::Settings::loaded_paths(&ctx.config.cwd);
    let settings_line = if settings_paths.is_empty() {
        "  (none found)".to_string()
    } else {
        settings_paths
            .iter()
            .map(|p| format!("  {p}"))
            .collect::<Vec<_>>()
            .join("\n")
    };

    let allow_line = if ctx.config.permissions_allow.is_empty() {
        "(none)".to_string()
    } else {
        ctx.config.permissions_allow.join(", ")
    };
    let deny_line = if ctx.config.permissions_deny.is_empty() {
        "(none)".to_string()
    } else {
        ctx.config.permissions_deny.join(", ")
    };

    CommandAction::Message(format!(
        "Current configuration\n\
         \n\
         model:                   {model}\n\
         max_tokens:              {max_tok}\n\
         cwd:                     {cwd}\n\
         auto_compact_enabled:    {compact}\n\
         dangerously_skip_perms:  {skip_perms}\n\
         verbose:                 {verbose}\n\
         vim_mode:                {vim}\n\
         \n\
         Settings files loaded:\n\
         {settings_line}\n\
         \n\
         permissions.allow:  {allow}\n\
         permissions.deny:   {deny}",
        model = ctx.config.model,
        max_tok = ctx.config.max_tokens,
        cwd = ctx.config.cwd.display(),
        compact = ctx.config.auto_compact_enabled,
        skip_perms = ctx.config.dangerously_skip_permissions,
        verbose = ctx.config.verbose,
        vim = if ctx.vim_mode { "on" } else { "off" },
        allow = allow_line,
        deny = deny_line,
    ))
}

fn cmd_files(ctx: &CommandContext) -> CommandAction {
    let Ok(rd) = std::fs::read_dir(&ctx.config.cwd) else {
        return CommandAction::Message("Could not read current directory.".into());
    };
    let mut entries: Vec<String> = rd
        .filter_map(|e| e.ok())
        .map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
            if is_dir { format!("{name}/") } else { name }
        })
        .filter(|n| !n.starts_with('.'))
        .collect();
    entries.sort();

    if entries.is_empty() {
        return CommandAction::Message("No files in current directory.".into());
    }
    CommandAction::Message(format!(
        "Files in {}\n\n{}",
        ctx.config.cwd.display(),
        entries.join("\n")
    ))
}

fn cmd_model(args: &str, _ctx: &CommandContext) -> CommandAction {
    if args.is_empty() {
        CommandAction::ListModels
    } else {
        // Normalize "provider name" (space) → "provider:name" (colon)
        let raw = args.trim();
        let model = if raw == "default" {
            crate::api::default_model().to_string()
        } else if let Some(rest) = raw.strip_prefix("ollama ") {
            format!("ollama:{}", rest.trim())
        } else if let Some(space_pos) = raw.find(' ') {
            let prefix = &raw[..space_pos];
            // Check if the word before the space is a known provider prefix
            if crate::api::PROVIDERS.iter().any(|p| p.prefix == prefix) {
                format!("{prefix}:{}", raw[space_pos + 1..].trim())
            } else {
                raw.to_string()
            }
        } else {
            raw.to_string()
        };

        // Ollama models — accept any "ollama:<name>" without checking KNOWN_MODELS
        if crate::api::is_ollama_model(&model) {
            return CommandAction::SetModel(model);
        }

        // OpenAI-compatible provider models — accept any "prefix:<name>"
        if crate::api::is_openai_compat_model(&model) {
            return CommandAction::SetModel(model);
        }

        // Resolve common shorthands before validation
        let model = resolve_model_alias(&model);

        // Anthropic models — validate against known list
        let known = KNOWN_MODELS.iter().any(|(n, _)| *n == model);
        if !known {
            let names: Vec<_> = KNOWN_MODELS.iter().map(|(n, _)| *n).collect();
            let providers: Vec<_> = crate::api::PROVIDERS
                .iter()
                .map(|p| format!("  /model {}:<model-name>  ({})", p.prefix, p.name))
                .collect();
            return CommandAction::Message(format!(
                "Unknown model '{model}'.\n\
                 Known Anthropic models:\n  {}\n\
                 \n\
                 Shorthands: opus, sonnet, haiku\n\
                 \n\
                 Local models:\n  /model ollama:<name>\n\
                 \n\
                 OpenAI-compatible providers:\n{}",
                names.join("\n  "),
                providers.join("\n"),
            ));
        }
        CommandAction::SetModel(model)
    }
}

/// Resolve common model shorthands to full Anthropic model IDs.
/// e.g. "opus" → "claude-opus-4-6", "sonnet" → "claude-sonnet-4-6"
pub fn resolve_model_alias(model: &str) -> String {
    match model {
        "opus" | "Opus" => "claude-opus-4-6".into(),
        "sonnet" | "Sonnet" => "claude-sonnet-4-6".into(),
        "haiku" | "Haiku" => "claude-haiku-4-5-20251001".into(),
        "opus-4-5" | "opus4.5" => "claude-opus-4-5".into(),
        "sonnet-4-5" | "sonnet4.5" => "claude-sonnet-4-5".into(),
        other => other.to_string(),
    }
}

fn cmd_keybindings() -> CommandAction {
    CommandAction::Message(
        concat!(
            "Keyboard shortcuts\n",
            "\n",
            "Input editing\n",
            "  Enter         Send message\n",
            "  Shift+Enter   Insert newline (multi-line prompt)\n",
            "  Backspace     Delete char before cursor\n",
            "  Delete        Delete char after cursor\n",
            "  ←/→           Move cursor left/right\n",
            "  Home/End      Move to start/end of line\n",
            "  Tab           Complete /command or plugin:command\n",
            "  ↑/↓           Input history\n",
            "  Esc           Cancel current request\n",
            "\n",
            "Readline shortcuts\n",
            "  Ctrl+A        Move to start of line\n",
            "  Ctrl+E        Move to end of line\n",
            "  Ctrl+U        Clear entire input line\n",
            "  Ctrl+W        Delete word before cursor\n",
            "  Ctrl+K        Delete from cursor to end\n",
            "  Alt+B         Move word left\n",
            "  Alt+F         Move word right\n",
            "  Alt+D         Delete word forward\n",
            "\n",
            "Vim mode (/vim to toggle)\n",
            "  Esc           Enter normal mode\n",
            "  i / a / A / I Insert/append modes\n",
            "  h / l         Move left / right\n",
            "  w / b / e     Word forward / back / end\n",
            "  0 / $         Start / end of line\n",
            "  x             Delete char under cursor\n",
            "  dd            Clear entire line\n",
            "  j / k         Scroll chat down / up\n",
            "  G             Scroll to bottom\n",
            "\n",
            "Chat & scrolling\n",
            "  PageUp/Down   Scroll chat\n",
            "  Mouse scroll  Scroll chat\n",
            "  Ctrl+R        Start/stop voice recording\n",
            "\n",
            "Session picker (/session)\n",
            "  ↑/↓           Select session\n",
            "  Enter         Resume selected session\n",
            "  1-9           Quick pick by number\n",
            "  d / Delete    Delete selected session\n",
            "  Esc / q       Close\n",
            "\n",
            "Permission dialog\n",
            "  y             Allow once\n",
            "  a             Always allow\n",
            "  n / Esc       Deny\n",
            "\n",
            "Text selection & copy\n",
            "  Shift+click   Select text (bypass TUI mouse capture)\n",
            "  Ctrl+Shift+C  Copy selected text\n",
            "  Ctrl+Shift+V  Paste\n",
            "\n",
            "Application\n",
            "  Ctrl+C        Quit\n",
            "  ?             Show this help (when input is empty)\n",
            "  /help         Show all commands",
        )
        .into(),
    )
}

// ─── RAG codebase indexing ────────────────────────────────────────────────────

fn cmd_index(args: &str) -> CommandAction {
    let args = args.trim();
    match args {
        "force" | "--force" | "-f" => CommandAction::IndexProject { force: true },
        "stats" | "status" => CommandAction::RagStatus,
        "" => CommandAction::IndexProject { force: false },
        _ => CommandAction::Message(
            "Usage: /index [force|stats]\n\
             \n  /index        — incremental re-index (only changed files)\
             \n  /index force  — full re-index (clear + rebuild)\
             \n  /index stats  — show index statistics"
                .into(),
        ),
    }
}

fn cmd_rag(args: &str) -> CommandAction {
    let query = args.trim();
    match query {
        "" => CommandAction::Message(
            "Usage: /rag <query|status|rebuild|clear>\n\
             \n  /rag <query>   — search the codebase index\
             \n  /rag status    — show index statistics\
             \n  /rag rebuild   — force full re-index\
             \n  /rag clear     — delete the index\
             \n\n  Example: /rag authentication middleware\
             \n  Example: /rag how does the streaming API work"
                .into(),
        ),
        "status" | "stats" | "info" => CommandAction::RagStatus,
        "rebuild" | "reindex" | "force" => CommandAction::IndexProject { force: true },
        "clear" | "reset" | "delete" => CommandAction::RagClear,
        _ => CommandAction::RagSearch(query.to_string()),
    }
}

fn cmd_budget(args: &str) -> CommandAction {
    let args = args.trim();
    if args.is_empty() || args == "status" {
        return CommandAction::SetBudget(None); // show current budget
    }
    if args == "off" || args == "clear" || args == "none" {
        return CommandAction::SetBudget(Some(-1.0)); // sentinel: clear budget
    }
    // Parse dollar amount: "$5", "5", "5.00", "$10.50"
    let cleaned = args.trim_start_matches('$');
    match cleaned.parse::<f64>() {
        Ok(v) if v > 0.0 => CommandAction::SetBudget(Some(v)),
        _ => CommandAction::Message(
            "Usage: /budget <amount>\n\
             \n  Set a session spend limit.\
             \n  Examples: /budget $5   /budget 10.50\
             \n  /budget off — remove budget limit\
             \n  /budget — show current budget"
                .into(),
        ),
    }
}

fn cmd_router(args: &str) -> CommandAction {
    let args = args.trim();
    match args {
        "" | "status" => CommandAction::RouterStatus,
        "on" | "enable" => CommandAction::RouterToggle,
        "off" | "disable" => CommandAction::RouterToggle,
        _ if args.starts_with("low ") => {
            let model = args["low ".len()..].trim().to_string();
            CommandAction::RouterSetTier {
                tier: "low".into(),
                model,
            }
        }
        _ if args.starts_with("medium ") || args.starts_with("mid ") => {
            let model = args
                .split_whitespace()
                .skip(1)
                .collect::<Vec<_>>()
                .join(" ");
            CommandAction::RouterSetTier {
                tier: "medium".into(),
                model,
            }
        }
        _ if args.starts_with("high ") => {
            let model = args["high ".len()..].trim().to_string();
            CommandAction::RouterSetTier {
                tier: "high".into(),
                model,
            }
        }
        _ if args.starts_with("super-high ") || args.starts_with("superhigh ") => {
            let model = args
                .split_whitespace()
                .skip(1)
                .collect::<Vec<_>>()
                .join(" ");
            CommandAction::RouterSetTier {
                tier: "super-high".into(),
                model,
            }
        }
        _ => CommandAction::Message(
            "Usage: /router [on|off|status]\n\
             \n  Smart model router — auto-routes tasks by complexity.\
             \n  /router on      — enable auto-routing\
             \n  /router off     — disable (use configured model for all)\
             \n  /router status  — show current config\
             \n  /router low <model>         — set low-complexity model\
             \n  /router medium <model>      — set medium-complexity model\
             \n  /router high <model>        — set high-complexity model\
             \n  /router super-high <model>  — set 1M-context model\
             \n\n  Example: /router low ollama:llama3"
                .into(),
        ),
    }
}

fn cmd_doctor(ctx: &CommandContext) -> CommandAction {
    use crate::distro::{Distro, build_install_command, find_missing};

    let distro = Distro::detect();
    let mut checks: Vec<String> = Vec::new();

    checks.push(format!("System: {}", distro.name()));
    checks.push(format!("RustyClaw v{}", env!("CARGO_PKG_VERSION")));
    checks.push(String::new());

    // XDG / config directory
    let config_dir = crate::config::Config::claude_dir();
    let data_dir = crate::config::Config::data_dir();
    checks.push(format!("✓ Config dir: {}", config_dir.display()));
    if data_dir != config_dir {
        checks.push(format!("✓ Data dir: {}", data_dir.display()));
    }

    // API key
    if ctx.config.api_key.len() >= 4 {
        checks.push(format!(
            "✓ ANTHROPIC_API_KEY set ({}...)",
            &ctx.config.api_key[..4]
        ));
    } else {
        checks.push("✗ ANTHROPIC_API_KEY not set — run: export ANTHROPIC_API_KEY=sk-...".into());
    }

    // cwd / git / config
    if ctx.config.cwd.exists() {
        checks.push(format!("✓ Working directory: {}", ctx.config.cwd.display()));
    } else {
        checks.push(format!(
            "✗ Working directory missing: {}",
            ctx.config.cwd.display()
        ));
    }
    let is_git = std::process::Command::new("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .current_dir(&ctx.config.cwd)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if is_git {
        checks.push("✓ Git repository".into());
    }

    if ctx.config.cwd.join("CLAUDE.md").exists() {
        checks.push("✓ CLAUDE.md present".into());
    } else {
        checks.push("  No CLAUDE.md — run /init to create one".into());
    }
    if ctx.config.cwd.join("AGENTS.md").exists() {
        checks.push("✓ AGENTS.md present".into());
    }
    checks.push(format!("✓ Model: {}", ctx.config.model));

    // .env
    let env_paths = [
        ctx.config.cwd.join(".env"),
        dirs::home_dir().unwrap_or_default().join(".env"),
        dirs::home_dir()
            .unwrap_or_default()
            .join(".config")
            .join("rustyclaw")
            .join(".env"),
    ];
    for p in env_paths.iter().filter(|p| p.exists()) {
        checks.push(format!("✓ .env loaded: {}", p.display()));
    }

    // Node.js / plugins
    let node_ok = std::process::Command::new("node")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if node_ok {
        let v = std::process::Command::new("node")
            .arg("--version")
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .unwrap_or_default();
        checks.push(format!("✓ Node.js {} (plugins supported)", v.trim()));
    } else {
        checks.push("  ✗ Node.js not found — plugins need Node.js".to_string());
        checks.push(format!("      {}", install_one_liner(&distro, "nodejs")));
    }

    // MCP
    if !ctx.mcp_statuses.is_empty() {
        let tc: usize = ctx.mcp_statuses.iter().map(|s| s.tool_count).sum();
        checks.push(format!(
            "✓ {} MCP server(s) ({} tools)",
            ctx.mcp_statuses.len(),
            tc
        ));
    }

    // Whisper
    let whisper_ok = std::process::Command::new("whisper")
        .args(["--help"])
        .output()
        .map(|o| o.status.success() || o.status.code() == Some(1))
        .unwrap_or(false);
    let openai_key = std::env::var("OPENAI_API_KEY")
        .or_else(|_| std::env::var("WHISPER_API_KEY"))
        .is_ok();
    if whisper_ok {
        checks.push("✓ whisper (offline STT transcription)".into());
    } else if openai_key {
        checks.push("✓ OPENAI_API_KEY set (cloud Whisper transcription)".into());
    } else {
        checks.push("  ✗ No speech-to-text — for /voice input:".into());
        match distro {
            crate::distro::Distro::Arch => {
                checks.push("      Offline: pipx install openai-whisper  (NOT bare pip)".into());
            }
            _ => {
                checks.push("      Offline: pipx install openai-whisper".into());
            }
        }
        checks.push("      Cloud:   add OPENAI_API_KEY=sk-... to ~/.env".into());
    }

    // TTS engine — XTTS v2
    if crate::voice::xtts_available() {
        checks.push("✓ XTTS v2 — natural neural TTS (primary engine)".into());
        if let Some(clone_path) = crate::voice::voice_clone_sample_path() {
            if clone_path.exists() {
                checks.push(format!("✓ Voice clone: {}", clone_path.display()));
            } else {
                checks.push(format!(
                    "  Using default speaker ({}) — /voice clone to set a custom voice",
                    crate::voice::XTTS_DEFAULT_SPEAKER
                ));
            }
        }
    } else {
        checks.push("  ✗ XTTS v2 not found — required for TTS".into());
        checks.push("      uv tool install TTS --python 3.11 \\".into());
        checks.push(
            "        --with 'transformers<4.46' --with 'torch<2.6' --with 'torchaudio<2.6'".into(),
        );
    }

    // System tool check
    checks.push(String::new());
    checks.push("── System tools ──────────────────────────────".into());

    let missing = find_missing(&distro);
    if missing.is_empty() {
        checks.push("✓ All system tools present".into());
    } else {
        for m in &missing {
            if let Some(pkg) = m.package {
                checks.push(format!(
                    "  ✗ {} — {}",
                    m.tool.binary(),
                    m.tool.description()
                ));
                checks.push(format!("      {}", install_one_liner(&distro, pkg)));
            } else if let Some(note) = &m.manual_note {
                checks.push(format!(
                    "  ✗ {} — {}",
                    m.tool.binary(),
                    m.tool.description()
                ));
                checks.push(format!("      {note}"));
            }
        }

        // Consolidated fix command
        if let Some(cmd) = build_install_command(&missing, &distro) {
            checks.push(String::new());
            checks.push("── Install all missing system packages ────────".into());
            checks.push(format!("  {cmd}"));
            checks.push(String::new());
            checks.push("  Or run /install-missing to install automatically.".into());
        }
    }

    CommandAction::Message(format!("Diagnostics\n\n{}", checks.join("\n")))
}

fn install_one_liner(distro: &crate::distro::Distro, pkg: &str) -> String {
    format!("{} {pkg}", crate::distro::install_prefix(distro))
}

fn cmd_install_missing() -> CommandAction {
    use crate::distro::{Distro, build_install_command, find_missing};

    let distro = Distro::detect();
    let missing = find_missing(&distro);

    if missing.is_empty() {
        return CommandAction::Message(
            "✓ All system tools are already installed. Nothing to do.".into(),
        );
    }

    match build_install_command(&missing, &distro) {
        Some(cmd) => CommandAction::RunInstall(cmd),
        None => {
            // Only pip/manual installs missing — no package manager command to run
            let notes: Vec<String> = missing
                .iter()
                .filter_map(|m| m.manual_note.as_ref().map(|n| format!("  • {n}")))
                .collect();
            CommandAction::Message(format!(
                "Missing tools require manual installation:\n\n{}\n\nNo package manager command needed.",
                notes.join("\n")
            ))
        }
    }
}

fn cmd_init(ctx: &CommandContext) -> CommandAction {
    let path = ctx.config.cwd.join("CLAUDE.md");
    let exists = path.exists();

    // Send a prompt to Claude to analyze the codebase and generate CLAUDE.md
    // This mirrors the real source's behavior: Claude explores the codebase
    // and writes a minimal, accurate CLAUDE.md rather than a generic template.
    let action_desc = if exists {
        format!(
            "CLAUDE.md already exists at {}. Suggest improvements to it.",
            path.display()
        )
    } else {
        format!("Create a new CLAUDE.md at {}.", path.display())
    };

    CommandAction::SendPrompt(format!(
        "Please analyze this codebase and {}

CLAUDE.md is loaded into every RustyClaw session. It must be concise — only include \
what Claude would get wrong without it.

## What to analyze

Read these key files if they exist:
- manifest files: package.json, Cargo.toml, pyproject.toml, go.mod, pom.xml, etc.
- README.md, Makefile, CI config (.github/workflows/, .circleci/, etc.)
- Existing CLAUDE.md (if any)
- .cursor/rules, .cursorrules, .github/copilot-instructions.md, AGENTS.md

Detect:
- Build, test, and lint commands (especially non-standard ones)
- Languages, frameworks, and package manager
- Project structure
- Code style rules that differ from language defaults
- Non-obvious gotchas or required environment variables

## What to include in CLAUDE.md

Only include lines that pass this test: \"Would removing this cause Claude to make mistakes?\"

Good candidates:
1. **Commands**: build, test, lint, format commands — especially non-standard ones
2. **Architecture**: High-level structure that requires reading multiple files to understand
3. **Gotchas**: Workflow quirks, required env vars, non-obvious conventions

Do NOT include:
- Generic practices like \"write unit tests\", \"provide helpful errors\", \"don't expose secrets\"
- Things that can be easily discovered by reading the code
- Repetition of what's in README unless it's critical to know

## Format

Start the file with:
```
# CLAUDE.md

This file provides guidance to RustyClaw when working with code in this repository.
```

Then add only the sections that have real content. Use terse, actionable language.",
        action_desc
    ))
}

fn cmd_permissions(ctx: &CommandContext) -> CommandAction {
    let _ = ctx; // permissions state lives in PermissionState, not config
    CommandAction::Message(
        concat!(
            "Permission system\n",
            "\n",
            "When Claude wants to run a sensitive tool, a permission dialog appears:\n",
            "  y — allow this call once\n",
            "  a — always allow this tool (no more prompts for this tool)\n",
            "  n — deny this call\n",
            "\n",
            "Tools that always require permission:\n",
            "  Bash, Write, Edit\n",
            "\n",
            "Tools that never require permission:\n",
            "  Read, Glob, Grep, WebFetch, WebSearch",
        )
        .into(),
    )
}

fn cmd_skills(ctx: &CommandContext) -> CommandAction {
    if ctx.skills.is_empty() {
        return CommandAction::Message(
            "No skills loaded.\n\
             \n\
             Create skills by adding .md files to ~/.claude/skills/\n\
             Each file becomes a /skill-name command.\n\
             \n\
             Example: ~/.claude/skills/review.md\n\
             Then type /review [args] to expand it."
                .into(),
        );
    }

    let mut lines = vec![format!("Loaded skills ({})\n", ctx.skills.len())];
    let mut names: Vec<_> = ctx.skills.keys().collect();
    names.sort();
    for name in names {
        if let Some(skill) = ctx.skills.get(name) {
            // v2.1.91: cap skill descriptions at 250 chars so the listing
            // doesn't blow out the terminal when a skill has a long preamble.
            let desc = if skill.description.chars().count() > 250 {
                let truncated: String = skill.description.chars().take(247).collect();
                format!("{truncated}…")
            } else {
                skill.description.clone()
            };
            lines.push(format!("  /{name} — {desc}"));
        }
    }
    lines.push(String::new());
    lines.push("Usage: /skill-name [args]".into());
    CommandAction::Message(lines.join("\n"))
}

fn cmd_review(args: &str) -> CommandAction {
    let pr_ref = args.trim();
    CommandAction::SendPrompt(format!(
        "You are an expert code reviewer. Follow these steps:\n\n\
         1. {}
         2. Run `gh pr diff <number>` to get the diff\n\
         3. Analyze the changes and provide a thorough code review that includes:\n\
            - Overview of what the PR does\n\
            - Analysis of code quality and style\n\
            - Specific suggestions for improvements\n\
            - Any potential issues or risks\n\n\
         Keep your review concise but thorough. Focus on:\n\
         - Code correctness and logic\n\
         - Project conventions\n\
         - Performance implications\n\
         - Test coverage\n\
         - Security considerations\n\n\
         Format your review with clear sections and bullet points.",
        if pr_ref.is_empty() {
            "Run `gh pr list` to show open PRs, then ask the user which PR to review".to_string()
        } else {
            format!("Run `gh pr view {pr_ref}` to get PR details. PR: {pr_ref}")
        }
    ))
}

fn cmd_autonomy(args: &str) -> CommandAction {
    let level = args.trim().to_lowercase();
    match level.as_str() {
        "suggest" | "auto-edit" | "full-auto" => CommandAction::SetAutonomy(level),
        "" => CommandAction::Message(
            "Autonomy levels:\n  suggest   — show diff + ask before every Write/Edit\n  \
             auto-edit — auto-apply edits, ask for new files (default)\n  \
             full-auto — apply all changes without asking\n\n\
             Usage: /autonomy <level>"
                .into(),
        ),
        _ => CommandAction::Message(format!(
            "Unknown autonomy level: '{level}'. Use: suggest, auto-edit, or full-auto"
        )),
    }
}

fn cmd_lint(ctx: &CommandContext) -> CommandAction {
    // Detect project type from the working directory
    let cwd = &ctx.config.cwd;
    let mut checks = Vec::new();

    if cwd.join("Cargo.toml").exists() {
        checks.push("cargo clippy --all-targets -- -D warnings");
        checks.push("cargo test");
    }
    if cwd.join("package.json").exists() {
        checks.push("npm run lint 2>/dev/null || npx eslint . 2>/dev/null || true");
        checks.push("npm test 2>/dev/null || true");
    }
    if cwd.join("pyproject.toml").exists() || cwd.join("setup.py").exists() {
        checks.push("ruff check . 2>/dev/null || python -m flake8 . 2>/dev/null || true");
        checks.push("python -m pytest 2>/dev/null || true");
    }
    if cwd.join("go.mod").exists() {
        checks.push("go vet ./...");
        checks.push("go test ./...");
    }

    if checks.is_empty() {
        return CommandAction::Message(
            "No recognized project type found (Cargo.toml, package.json, pyproject.toml, go.mod)."
                .into(),
        );
    }

    let cmds = checks.join("\n  ");
    CommandAction::SendPrompt(format!(
        "Run the following lint/test commands for this project and fix any errors or warnings. \
         Keep running them in a loop until they all pass cleanly:\n\n  {cmds}\n\n\
         For each failure:\n\
         1. Read the error output carefully\n\
         2. Fix the root cause in the source code\n\
         3. Re-run the failing command to verify the fix\n\
         4. Repeat until all commands pass with zero errors/warnings\n\n\
         Report what you fixed when done."
    ))
}

fn cmd_mcp(args: &str, ctx: &CommandContext) -> CommandAction {
    let sub = args.trim();
    let (subcmd, rest) = split_first_word(sub);

    match subcmd {
        "" | "list" | "status" => {
            if ctx.mcp_statuses.is_empty() {
                return CommandAction::Message(
                    "No MCP servers connected.\n\n\
                    Add one: /mcp add <name> <command> [args...]\n\
                    Or for HTTP:  /mcp add <name> <url>\n\n\
                    Example: /mcp add github npx -y @modelcontextprotocol/server-github\n\
                    Restart rustyclaw after adding servers."
                        .into(),
                );
            }
            let total_tools: usize = ctx.mcp_statuses.iter().map(|s| s.tool_count).sum();
            let mut lines = vec![format!(
                "MCP servers ({} connected, {} tools total)\n",
                ctx.mcp_statuses.len(),
                total_tools
            )];

            for s in ctx.mcp_statuses {
                lines.push(format!(
                    "  {:20} [{}]  {} tool{}",
                    s.name,
                    s.transport,
                    s.tool_count,
                    if s.tool_count == 1 { "" } else { "s" }
                ));
            }

            lines.push(String::new());
            lines.push("Tool names are prefixed with mcp__<server>__ to avoid conflicts.".into());
            lines.push("Use /mcp add <name> <cmd> to add, /mcp remove <name> to remove.".into());

            CommandAction::Message(lines.join("\n"))
        }
        "tools" => {
            // List all tools per server
            let mut lines = vec!["MCP tools by server\n".to_string()];
            for s in ctx.mcp_statuses {
                if s.tool_count == 0 {
                    lines.push(format!("  {} — no tools", s.name));
                } else {
                    lines.push(format!("  {} ({} tools)", s.name, s.tool_count));
                }
            }
            if ctx.mcp_statuses.is_empty() {
                lines.push("  No MCP servers connected.".into());
            }
            CommandAction::Message(lines.join("\n"))
        }
        "add" => mcp_add_server(rest),
        "remove" | "rm" | "delete" => mcp_remove_server(rest),
        "enable" => mcp_set_disabled(rest, false),
        "disable" => mcp_set_disabled(rest, true),
        "reconnect" => CommandAction::Message(
            "MCP reconnection requires restarting rustyclaw.\n\
                 Exit and relaunch to reconnect all MCP servers."
                .into(),
        ),
        "get" => {
            let name = rest.trim();
            if name.is_empty() {
                return CommandAction::Message("Usage: /mcp get <name>".into());
            }
            let settings_path = Config::claude_dir().join("settings.json");
            let raw = std::fs::read_to_string(&settings_path).unwrap_or_default();
            let val: serde_json::Value =
                serde_json::from_str(&raw).unwrap_or(serde_json::json!({}));
            if let Some(srv) = val.get("mcpServers").and_then(|m| m.get(name)) {
                CommandAction::Message(format!(
                    "MCP server '{}'\n{}",
                    name,
                    serde_json::to_string_pretty(srv).unwrap_or_default()
                ))
            } else {
                CommandAction::Message(format!("MCP server '{}' not found in settings.json", name))
            }
        }
        _ => CommandAction::Message(
            "MCP commands:\n  /mcp list              — show connected servers\n  \
             /mcp tools             — show tools per server\n  \
             /mcp add <n> <cmd>     — add stdio server\n  \
             /mcp add <n> <url>     — add HTTP server\n  \
             /mcp remove <n>        — remove server\n  \
             /mcp enable <n>        — enable disabled server\n  \
             /mcp disable <n>       — disable server\n  \
             /mcp get <n>           — show server config\n  \
             /mcp reconnect         — restart all (requires app restart)"
                .into(),
        ),
    }
}

/// Write a new MCP server entry to ~/.claude/settings.json.
/// Detects HTTP servers by URL prefix; everything else is stdio.
fn mcp_add_server(args: &str) -> CommandAction {
    let args = args.trim();
    let (name, rest) = split_first_word(args);
    if name.is_empty() {
        return CommandAction::Message(
            "Usage: /mcp add <name> <command|url> [args...]\n\
             Examples:\n  /mcp add github npx -y @modelcontextprotocol/server-github\n  \
             /mcp add remote http://localhost:3000/mcp"
                .into(),
        );
    }
    let rest = rest.trim();
    if rest.is_empty() {
        return CommandAction::Message(format!("Usage: /mcp add {name} <command|url> [args...]"));
    }

    let settings_path = Config::claude_dir().join("settings.json");
    let raw = std::fs::read_to_string(&settings_path).unwrap_or_else(|_| "{}".to_string());
    let mut val: serde_json::Value = serde_json::from_str(&raw).unwrap_or(serde_json::json!({}));

    // Check if already exists
    if val.get("mcpServers").and_then(|m| m.get(name)).is_some() {
        return CommandAction::Message(format!(
            "MCP server '{}' already exists. Remove it first with /mcp remove {}",
            name, name
        ));
    }

    // Build config object
    let server_cfg = if rest.starts_with("http://") || rest.starts_with("https://") {
        serde_json::json!({ "url": rest })
    } else {
        // Parse: first token = command, rest = args array
        let parts: Vec<&str> = rest.split_whitespace().collect();
        match parts.split_first() {
            None => return CommandAction::Message("Invalid: empty command string".into()),
            Some((cmd, cmd_args)) => {
                if cmd_args.is_empty() {
                    serde_json::json!({ "command": cmd })
                } else {
                    serde_json::json!({ "command": cmd, "args": cmd_args })
                }
            }
        }
    };

    // Upsert into mcpServers
    if !val.is_object() {
        val = serde_json::json!({});
    }
    let root = match val.as_object_mut() {
        Some(o) => o,
        None => return CommandAction::Message("settings.json is not a JSON object".into()),
    };
    let servers = root.entry("mcpServers").or_insert(serde_json::json!({}));
    match servers.as_object_mut() {
        Some(m) => {
            m.insert(name.to_string(), server_cfg);
        }
        None => {
            return CommandAction::Message(
                "mcpServers is not a JSON object in settings.json".into(),
            );
        }
    }

    match serde_json::to_string_pretty(&val) {
        Ok(s) => {
            if let Some(parent) = settings_path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            match std::fs::write(&settings_path, s) {
                Ok(_) => CommandAction::Message(format!(
                    "MCP server '{}' added to {}\nRestart rustyclaw to connect.",
                    name,
                    settings_path.display()
                )),
                Err(e) => CommandAction::Message(format!("Failed to write settings: {e}")),
            }
        }
        Err(e) => CommandAction::Message(format!("Serialization error: {e}")),
    }
}

/// Remove an MCP server from ~/.claude/settings.json.
fn mcp_remove_server(args: &str) -> CommandAction {
    let name = args.trim();
    if name.is_empty() {
        return CommandAction::Message("Usage: /mcp remove <name>".into());
    }

    let settings_path = Config::claude_dir().join("settings.json");
    let raw = std::fs::read_to_string(&settings_path).unwrap_or_else(|_| "{}".to_string());
    let mut val: serde_json::Value = serde_json::from_str(&raw).unwrap_or(serde_json::json!({}));

    let removed = val
        .get_mut("mcpServers")
        .and_then(|m| m.as_object_mut())
        .and_then(|m| m.remove(name));

    if removed.is_none() {
        return CommandAction::Message(format!("MCP server '{}' not found in settings.json", name));
    }

    match serde_json::to_string_pretty(&val) {
        Ok(s) => match std::fs::write(&settings_path, s) {
            Ok(_) => CommandAction::Message(format!(
                "MCP server '{}' removed from settings.json\nRestart rustyclaw to disconnect.",
                name
            )),
            Err(e) => CommandAction::Message(format!("Failed to write settings: {e}")),
        },
        Err(e) => CommandAction::Message(format!("Serialization error: {e}")),
    }
}

/// Enable or disable an MCP server in settings.json via a `disabled` flag.
fn mcp_set_disabled(args: &str, disabled: bool) -> CommandAction {
    let name = args.trim();
    if name.is_empty() {
        return CommandAction::Message(if disabled {
            "Usage: /mcp disable <name>".into()
        } else {
            "Usage: /mcp enable <name>".into()
        });
    }

    let settings_path = Config::claude_dir().join("settings.json");
    let raw = std::fs::read_to_string(&settings_path).unwrap_or_else(|_| "{}".to_string());
    let mut val: serde_json::Value = serde_json::from_str(&raw).unwrap_or(serde_json::json!({}));

    let server = val
        .get_mut("mcpServers")
        .and_then(|m| m.as_object_mut())
        .and_then(|m| m.get_mut(name));

    match server {
        None => CommandAction::Message(format!("MCP server '{}' not found in settings.json", name)),
        Some(s) => {
            if let Some(obj) = s.as_object_mut() {
                if disabled {
                    obj.insert("disabled".to_string(), serde_json::json!(true));
                } else {
                    obj.remove("disabled");
                }
            }
            match serde_json::to_string_pretty(&val) {
                Ok(text) => match std::fs::write(&settings_path, text) {
                    Ok(_) => CommandAction::Message(format!(
                        "MCP server '{}' {}. Restart rustyclaw to apply.",
                        name,
                        if disabled { "disabled" } else { "enabled" }
                    )),
                    Err(e) => CommandAction::Message(format!("Failed to write settings: {e}")),
                },
                Err(e) => CommandAction::Message(format!("Serialization error: {e}")),
            }
        }
    }
}

/// Dispatch the `/memory` command with subcommands for the persistent MemoryStore.
fn cmd_memory_dispatch(args: &str, ctx: &CommandContext) -> CommandAction {
    let (sub, rest) = split_first_word(args);
    match sub {
        "list" | "" => CommandAction::MemoryList,
        "search" => {
            if rest.is_empty() {
                CommandAction::Message("Usage: /memory search <query>".into())
            } else {
                CommandAction::MemorySearch(rest.to_string())
            }
        }
        "clear" => CommandAction::MemoryClear,
        "inject" => CommandAction::MemoryInject,
        "auto" => match rest {
            "on" => CommandAction::MemoryAutoToggle(true),
            "off" => CommandAction::MemoryAutoToggle(false),
            _ => CommandAction::Message(format!(
                "Memory auto-capture is currently {}.\n\
                         Usage: /memory auto on|off",
                if ctx.config.memory_auto_capture {
                    "ON"
                } else {
                    "OFF"
                }
            )),
        },
        "add" => {
            if rest.is_empty() {
                CommandAction::Message("Usage: /memory add <text>".into())
            } else {
                CommandAction::MemoryAdd(rest.to_string())
            }
        }
        "forget" => {
            if rest.is_empty() {
                CommandAction::Message("Usage: /memory forget <query>".into())
            } else {
                CommandAction::MemoryForget(rest.to_string())
            }
        }
        _ => CommandAction::Message(format!(
            "Unknown /memory subcommand '{sub}'.\n\
             Available: list, search <q>, add <text>, forget <q>, clear, inject, auto on|off"
        )),
    }
}

fn cmd_tasks(ctx: &CommandContext) -> CommandAction {
    let state = ctx.todo_state.lock().unwrap_or_else(|e| e.into_inner());
    if state.is_empty() {
        return CommandAction::Message(
            "No tasks. Claude will create tasks automatically for complex multi-step work.".into(),
        );
    }

    let mut lines = vec![format!("Tasks ({})\n", state.len())];
    for item in state.iter() {
        let icon = match item.status {
            TodoStatus::Completed => "✓",
            TodoStatus::InProgress => "▶",
            TodoStatus::Pending => "○",
        };
        let pri = match item.priority {
            TodoPriority::High => " [high]",
            TodoPriority::Medium => "",
            TodoPriority::Low => " [low]",
        };
        lines.push(format!("  {icon} {}{pri}", item.content));
    }
    CommandAction::Message(lines.join("\n"))
}

fn cmd_copy(ctx: &CommandContext) -> CommandAction {
    let Some(text) = ctx.last_assistant else {
        return CommandAction::Message("No assistant message to copy yet.".into());
    };
    clipboard_write(text)
}

/// Write text to the system clipboard. Tries all known clipboard tools across platforms.
pub fn clipboard_write(text: &str) -> CommandAction {
    use std::io::Write;

    // Helper: pipe text into a child process
    let pipe_to = |cmd: &str, args: &[&str]| -> std::io::Result<std::process::ExitStatus> {
        let mut child = std::process::Command::new(cmd)
            .args(args)
            .stdin(std::process::Stdio::piped())
            .spawn()?;
        child
            .stdin
            .as_mut()
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "stdin not available"))?
            .write_all(text.as_bytes())?;
        child.wait()
    };

    // Wayland
    let ok = std::process::Command::new("wl-copy").arg(text).status()
        .map(|s| s.success()).unwrap_or(false)
    // X11 xclip
    || pipe_to("xclip", &["-selection", "clipboard"]).map(|s| s.success()).unwrap_or(false)
    // X11 xsel
    || pipe_to("xsel", &["--clipboard", "--input"]).map(|s| s.success()).unwrap_or(false)
    // macOS
    || pipe_to("pbcopy", &[]).map(|s| s.success()).unwrap_or(false)
    // WSL / Windows
    || pipe_to("clip.exe", &[]).map(|s| s.success()).unwrap_or(false);

    if ok {
        CommandAction::Message("Copied to clipboard.".into())
    } else {
        CommandAction::Message(
            "Could not copy to clipboard.\n\
             Install one of: wl-clipboard (Wayland), xclip or xsel (X11), pbcopy (macOS), clip.exe (WSL)."
            .into()
        )
    }
}

fn cmd_branch(ctx: &CommandContext) -> CommandAction {
    let run = |args: &[&str]| -> Option<String> {
        std::process::Command::new("git")
            .args(args)
            .current_dir(&ctx.config.cwd)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .filter(|s| !s.is_empty())
    };

    let current =
        run(&["rev-parse", "--abbrev-ref", "HEAD"]).unwrap_or_else(|| "not a git repo".into());
    if current == "not a git repo" {
        return CommandAction::Message("Not a git repository.".into());
    }

    let mut lines = vec![format!("Current branch: {current}\n")];

    if let Some(log) = run(&["log", "--oneline", "-5"]) {
        lines.push("Recent commits:".into());
        for l in log.lines() {
            lines.push(format!("  {l}"));
        }
        lines.push(String::new());
    }

    if let Some(status) = run(&["status", "--short"]) {
        if !status.is_empty() {
            lines.push("Working tree changes:".into());
            for l in status.lines().take(15) {
                lines.push(format!("  {l}"));
            }
        } else {
            lines.push("Working tree: clean".into());
        }
    }

    if let Some(branches) = run(&["branch", "-a", "--format=%(refname:short)"]) {
        let all: Vec<_> = branches
            .lines()
            .filter(|b| !b.contains("HEAD"))
            .take(10)
            .collect();
        if !all.is_empty() {
            lines.push(String::new());
            lines.push("Branches:".into());
            for b in all {
                let marker = if b == current { "▶ " } else { "  " };
                lines.push(format!("{marker}{b}"));
            }
        }
    }

    CommandAction::Message(lines.join("\n"))
}

fn cmd_add_dir(args: &str, ctx: &CommandContext) -> CommandAction {
    let dir = args.trim();
    if dir.is_empty() {
        return CommandAction::Message(
            "Usage: /add-dir <path>\nAdds a directory to your context for Claude to reference."
                .into(),
        );
    }
    let path = std::path::Path::new(dir);
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        ctx.config.cwd.join(path)
    };
    if !path.exists() {
        return CommandAction::Message(format!("Directory not found: {}", path.display()));
    }
    if !path.is_dir() {
        return CommandAction::Message(format!("Not a directory: {}", path.display()));
    }
    // We communicate the added dir by sending a system message to Claude
    CommandAction::SendPrompt(format!(
        "<context>\nThe user has added the directory {} to the conversation context. \
         When working with files, also consider files in this directory.\n</context>",
        path.display()
    ))
}

fn cmd_pr_comments(args: &str, ctx: &CommandContext) -> CommandAction {
    let pr = args.trim();
    let mut cmd = std::process::Command::new("gh");
    if pr.is_empty() {
        cmd.args(["pr", "view", "--json", "comments,reviews,number,title"]);
    } else {
        cmd.args(["pr", "view", pr, "--json", "comments,reviews,number,title"]);
    }
    cmd.current_dir(&ctx.config.cwd);

    match cmd.output() {
        Err(_) => CommandAction::Message(
            "gh CLI not found. Install the GitHub CLI (gh) to use /pr_comments.".into(),
        ),
        Ok(o) if !o.status.success() => {
            let err = String::from_utf8_lossy(&o.stderr).trim().to_string();
            CommandAction::Message(format!("gh pr view failed: {err}"))
        }
        Ok(o) => {
            let out = String::from_utf8_lossy(&o.stdout).to_string();
            // Parse minimally — just show raw JSON summary or pass to Claude
            CommandAction::SendPrompt(format!(
                "The following is the GitHub PR data. Please summarize the comments and reviews:\n\n{out}"
            ))
        }
    }
}

fn cmd_usage(ctx: &CommandContext) -> CommandAction {
    // Usage = cost information for this session
    cmd_cost(ctx)
}

fn cmd_session(args: &str, _ctx: &CommandContext) -> CommandAction {
    let (sub, sub_args) = split_first_word(args);
    match sub {
        "list" | "" => CommandAction::ListSessions,
        "clear-all" | "clearall" | "clear" => CommandAction::ClearAllSessions,
        "search" => {
            if sub_args.is_empty() {
                CommandAction::Message("Usage: /session search <query>".into())
            } else {
                CommandAction::SearchSessions(sub_args.to_string())
            }
        }
        "delete" => {
            if sub_args.is_empty() {
                CommandAction::Message("Usage: /session delete <id-prefix>".into())
            } else {
                CommandAction::Message(format!(
                    "To delete session, run:\n  rm ~/.claude/sessions/{sub_args}*.jsonl ~/.claude/sessions/{sub_args}*.meta\n\nUse /session list to confirm the ID prefix."
                ))
            }
        }
        // Treat anything else as a session ID prefix to resume
        _ => CommandAction::ResumeSession(sub.to_string()),
    }
}

fn cmd_resume(args: &str) -> CommandAction {
    if args.is_empty() {
        // Show session list — run_loop handles async I/O
        CommandAction::ListSessions
    } else {
        // Pass the prefix to run_loop for async matching
        CommandAction::ResumeSession(args.trim().to_string())
    }
}

fn cmd_export(_ctx: &CommandContext) -> CommandAction {
    CommandAction::ExportCurrentSession
}

fn cmd_hooks(ctx: &CommandContext) -> CommandAction {
    let hooks =
        match &ctx.config.hooks {
            None => {
                return CommandAction::Message(concat!(
                "Hooks\n\nNo hooks configured.\n\n",
                "Add hooks to ~/.claude/settings.json or .claude/settings.json:\n\n",
                "{\n  \"hooks\": {\n",
                "    \"preToolUse\": [\n",
                "      {\"matcher\": \"Bash\", \"command\": \"echo Running: $TOOL_NAME\"}\n",
                "    ],\n",
                "    \"postToolUse\": [\n",
                "      {\"matcher\": \"\", \"command\": \"echo Done: $TOOL_NAME\"}\n",
                "    ]\n",
                "  }\n}\n\n",
                "Env vars: TOOL_NAME, TOOL_INPUT (pre), TOOL_RESULT (post).\n",
                "Empty matcher or \"*\" matches all tools."
            ).into());
            }
            Some(h) => h.clone(),
        };

    let mut lines = vec!["Hooks\n".to_string()];

    if hooks.pre_tool_use.is_empty() && hooks.post_tool_use.is_empty() {
        lines.push("  No hooks defined.".into());
    }

    if !hooks.pre_tool_use.is_empty() {
        lines.push("Pre-tool-use:".into());
        for h in &hooks.pre_tool_use {
            let matcher = if h.matcher.is_empty() || h.matcher == "*" {
                "*all*".to_string()
            } else {
                h.matcher.clone()
            };
            lines.push(format!("  [{matcher}]  {}", h.command));
        }
        lines.push(String::new());
    }

    if !hooks.post_tool_use.is_empty() {
        lines.push("Post-tool-use:".into());
        for h in &hooks.post_tool_use {
            let matcher = if h.matcher.is_empty() || h.matcher == "*" {
                "*all*".to_string()
            } else {
                h.matcher.clone()
            };
            lines.push(format!("  [{matcher}]  {}", h.command));
        }
    }

    lines.push(String::new());
    lines.push("Env vars: TOOL_NAME, TOOL_INPUT (pre), TOOL_RESULT (post).".into());

    let _ = ctx; // suppress unused warning
    CommandAction::Message(lines.join("\n"))
}

fn cmd_image(args: &str) -> CommandAction {
    let path = args.trim();
    if path.is_empty() {
        return CommandAction::Message(
            "Usage: /image <path>\n\nAttaches an image to your next message.\n\
             Supported formats: PNG, JPEG, GIF, WebP\n\n\
             Example: /image /home/user/screenshot.png"
                .into(),
        );
    }
    let expanded = if path.starts_with('~') {
        if let Some(home) = dirs::home_dir() {
            home.join(path.trim_start_matches("~/"))
                .to_string_lossy()
                .into_owned()
        } else {
            path.to_string()
        }
    } else {
        path.to_string()
    };
    if !std::path::Path::new(&expanded).exists() {
        return CommandAction::Message(format!("File not found: {expanded}"));
    }
    CommandAction::AttachImage(expanded)
}

/// A single help command entry: (slash_command, description).
pub type HelpCommand = (&'static str, &'static str);

/// Help categories — each entry is (category_name, short_description, commands).
/// Used by both the interactive picker and `/help <category>`.
pub const HELP_CATEGORIES: &[(&str, &str, &[HelpCommand])] = &[
    (
        "General",
        "Basic session commands",
        &[
            ("/help", "show this help"),
            ("/status", "show session status"),
            ("/cost", "show token usage & costs"),
            ("/stats", "detailed session statistics"),
            ("/clear", "clear chat history"),
            ("/compact", "summarize & compress context"),
            ("/exit", "quit rustyclaw"),
        ],
    ),
    (
        "Model & behavior",
        "Switch models, effort, output style",
        &[
            ("/model", "interactive model picker"),
            ("/effort", "set thinking effort (low, medium, high)"),
            ("/brief", "toggle concise responses"),
            ("/output-style", "set output style preference"),
            ("/plan", "toggle plan mode (read-only)"),
            ("/advisor", "ask Claude for strategic advice"),
        ],
    ),
    (
        "Session",
        "Save, resume, manage sessions",
        &[
            ("/session", "interactive session picker (↑↓ Enter d)"),
            ("/resume", "resume a saved session"),
            ("/rename", "rename current session"),
            ("/export", "export session to markdown"),
            ("/rewind", "undo last n exchanges (default 1)"),
            ("/summary", "summarize conversation so far"),
            ("/copy", "copy last response to clipboard"),
            (
                "/undo",
                "Rewind working tree to an earlier auto-commit turn ([N] or picker)",
            ),
            (
                "/redo",
                "Advance working tree to a later auto-commit turn ([N] or picker)",
            ),
            (
                "/autocommit",
                "Show auto-commit status (enabled, session ID, turns recorded)",
            ),
        ],
    ),
    (
        "Code & git",
        "Commits, PRs, reviews, diffs",
        &[
            ("/diff", "git diff"),
            ("/branch", "show/switch branches"),
            ("/commit", "generate & run a git commit"),
            ("/commit-push-pr", "commit, push, and create PR"),
            ("/review", "code review"),
            ("/security-review", "security audit"),
            (
                "/checkpoint",
                "git checkpoint commit (snapshot current changes)",
            ),
            (
                "/lint",
                "auto-detect and run lint + tests, fix errors in loop",
            ),
            ("/pr_comments", "show PR comments"),
            ("/issue", "work on a GitHub issue"),
            ("/autofix-pr", "auto-fix PR review comments"),
        ],
    ),
    (
        "Project",
        "Config, context, environment",
        &[
            ("/init", "create CLAUDE.md for this project"),
            ("/doctor", "check system health"),
            ("/config", "show configuration"),
            ("/context", "show context window usage"),
            ("/files", "list project files"),
            ("/add-dir", "add directory to context"),
            ("/permissions", "show permission settings"),
            ("/env", "show environment info"),
            ("/hooks", "show configured hooks"),
            ("/memory", "show saved memories"),
            ("/tasks", "show todo items"),
            ("/skills", "list available skills"),
            ("/insights", "show codebase insights"),
        ],
    ),
    (
        "Spawn agents",
        "Background parallel agents in git worktrees",
        &[
            ("/spawn <task>", "spawn a background agent"),
            ("/spawn list", "list spawned agents"),
            ("/spawn review <id>", "review agent's changes"),
            ("/spawn merge <id>", "merge agent's changes"),
            ("/spawn kill <id>", "cancel a running agent"),
            ("/spawn discard <id>", "discard agent's worktree"),
        ],
    ),
    (
        "Browser",
        "Autonomous browser agent",
        &[
            ("/browse <goal>", "run autonomous browser agent towards a goal"),
            ("/browse --yolo <goal>", "yolo mode: no approval prompts"),
            ("/browse --ask <goal>", "ask before every action"),
            ("/browse --max-steps N <goal>", "cap the run at N steps"),
            ("/browser <url>", "open URL in managed browser session"),
            ("/screenshot", "take a screenshot of the current page"),
        ],
    ),
    (
        "Plugins & tools",
        "MCP servers, plugins, viz",
        &[
            ("/mcp", "show MCP server status"),
            ("/plugin", "install/manage plugins"),
            ("/reload", "hot-reload settings.json + CLAUDE.md"),
            ("/reload-plugins", "reload all MCP plugins"),
            ("/ctx-viz", "context usage visualization"),
        ],
    ),
    (
        "Other",
        "Voice, vim, themes, upgrades",
        &[
            ("/image", "attach an image to next message"),
            ("/voice", "voice input/output status"),
            ("/voice clone", "record a custom voice for TTS"),
            ("/vim", "toggle vim editing mode"),
            (
                "/autonomy",
                "set file change oversight (suggest/auto-edit/full-auto)",
            ),
            ("/theme", "switch theme (dark, light, solarized)"),
            ("/btw", "prepend a note to next message"),
            ("/upgrade", "check for updates"),
            ("/release-notes", "open upstream release notes"),
            ("/keybindings", "show keyboard shortcuts"),
            ("/ide", "show IDE integration info"),
        ],
    ),
];

fn cmd_help(args: &str) -> CommandAction {
    let q = args.trim().to_lowercase();
    if q.is_empty() {
        return CommandAction::ListHelp;
    }
    // Match category by name prefix or number
    if let Ok(n) = q.parse::<usize>()
        && n >= 1
        && n <= HELP_CATEGORIES.len()
    {
        return CommandAction::ShowHelpCategory(n - 1);
    }
    for (i, (name, _, _)) in HELP_CATEGORIES.iter().enumerate() {
        if name.to_lowercase().starts_with(&q) {
            return CommandAction::ShowHelpCategory(i);
        }
    }
    // No match — show the picker anyway
    CommandAction::ListHelp
}

// ── New commands (gap fill) ───────────────────────────────────────────────────

fn cmd_commit(args: &str, _ctx: &CommandContext) -> CommandAction {
    let msg = args.trim();
    if msg.is_empty() {
        // Ask Claude to generate a commit
        CommandAction::SendPrompt(
            "Please review the git diff and staged changes, then create an appropriate git commit \
             with a clear, concise commit message. Use conventional commit format if applicable. \
             Run `git add -A && git commit -m <message>` to commit."
                .into(),
        )
    } else {
        // User provided the message — just run it
        CommandAction::SendPrompt(format!(
            "Please run: git add -A && git commit -m {msg:?}\n\
             Then confirm the commit was successful."
        ))
    }
}

fn cmd_commit_push_pr(args: &str, ctx: &CommandContext) -> CommandAction {
    let branch = args.trim();
    let branch_clause = if branch.is_empty() {
        String::new()
    } else {
        format!(" to branch '{branch}'")
    };
    let _ = ctx;
    CommandAction::SendPrompt(format!(
        "Please: \
         1. Review the git diff for any staged/unstaged changes. \
         2. Create a git commit with an appropriate message. \
         3. Push the commit{branch_clause}. \
         4. Create a GitHub pull request using `gh pr create` with a clear title and description. \
         Show me the PR URL when done."
    ))
}

fn cmd_effort(args: &str, ctx: &CommandContext) -> CommandAction {
    let level = args.trim().to_lowercase();
    let _ = ctx;
    match level.as_str() {
        "1" | "low" | "quick" => CommandAction::SendPrompt(
            "For this conversation, keep responses brief and fast. \
             Prefer quick solutions over thorough analysis. Skip detailed explanations unless asked."
                .into()
        ),
        "2" | "medium" | "normal" | "" => CommandAction::SendPrompt(
            "Use normal effort for this conversation — balance thoroughness with efficiency."
                .into()
        ),
        "3" | "high" | "thorough" | "max" => CommandAction::SendPrompt(
            "For this conversation, use maximum effort. Be thorough, check edge cases, \
             write comprehensive tests, and explain your reasoning in detail."
                .into()
        ),
        _ => CommandAction::Message(format!(
            "Usage: /effort [1|2|3] or [low|medium|high]\n\n\
             1 / low    — quick, brief responses\n\
             2 / medium — balanced (default)\n\
             3 / high   — thorough, detailed responses\n\n\
             Got: '{}'", args.trim()
        )),
    }
}

fn cmd_insights(ctx: &CommandContext) -> CommandAction {
    let _ = ctx;
    CommandAction::SendPrompt(
        "Please analyze this codebase and provide insights on: \
         1. Architecture and design patterns used \
         2. Code quality and potential improvements \
         3. Security considerations \
         4. Performance bottlenecks or opportunities \
         5. Test coverage gaps \
         6. Technical debt items \
         Be specific with file paths and line numbers where relevant."
            .into(),
    )
}

fn cmd_security_review() -> CommandAction {
    // Mirrors the real security-review.ts prompt methodology:
    // git diff to get changes, 3-phase analysis, false-positive filtering
    CommandAction::SendPrompt(concat!(
        "You are a senior security engineer conducting a focused security review of the changes on this branch.\n\n",
        "First, run these commands to gather context:\n",
        "- git status\n",
        "- git diff --name-only origin/HEAD...\n",
        "- git log --no-decorate origin/HEAD...\n",
        "- git diff origin/HEAD...\n\n",
        "OBJECTIVE:\n",
        "Identify HIGH-CONFIDENCE security vulnerabilities with real exploitation potential. ",
        "Focus ONLY on security issues newly introduced by the current changes. ",
        "Do not comment on pre-existing concerns.\n\n",
        "CRITICAL INSTRUCTIONS:\n",
        "1. MINIMIZE FALSE POSITIVES: Only flag issues where >80% confidence of actual exploitability\n",
        "2. EXCLUSIONS — do NOT report:\n",
        "   - Denial of Service vulnerabilities\n",
        "   - Secrets stored on disk (handled elsewhere)\n",
        "   - Rate limiting/resource exhaustion\n",
        "   - UUIDs (assumed unguessable)\n",
        "   - Environment variable / CLI flag injection (trusted values)\n",
        "   - Memory/file descriptor leaks\n",
        "   - Tabnabbing, XS-Leaks, open redirects (unless very high confidence)\n",
        "   - XSS in React/Angular unless using dangerouslySetInnerHTML or similar\n",
        "   - Missing auth checks in client-side code (server is responsible)\n\n",
        "SECURITY CATEGORIES TO EXAMINE:\n",
        "- SQL/command/XXE/template/NoSQL injection\n",
        "- Authentication bypass, privilege escalation, session flaws\n",
        "- Hardcoded secrets, weak crypto, improper key storage\n",
        "- Path traversal, insecure deserialization, RCE\n",
        "- SSRF, CORS misconfigurations, insecure direct object references\n\n",
        "ANALYSIS PROCESS (3 phases):\n",
        "1. Use a sub-task (Task tool) to identify all candidate vulnerabilities\n",
        "2. For each candidate, launch a parallel sub-task to filter false positives\n",
        "3. Only include findings with confidence >= 8/10\n\n",
        "FINAL REPORT FORMAT (markdown):\n",
        "## Security Review\n",
        "### [CRITICAL|HIGH|MEDIUM] Finding Title\n",
        "- **File**: path/to/file.rs:line\n",
        "- **Description**: What the vulnerability is\n",
        "- **Attack path**: Concrete exploitation steps\n",
        "- **Fix**: Recommended remediation\n",
        "- **Confidence**: N/10\n\n",
        "If no issues found, state: 'No high-confidence security vulnerabilities identified in these changes.'"
    ).into())
}

fn cmd_ide(ctx: &CommandContext) -> CommandAction {
    // Show IDE integration information
    let _ = ctx;
    CommandAction::Message(concat!(
        "IDE Integration\n\n",
        "rustyclaw runs as a standalone TUI — no IDE plugin required.\n\n",
        "For VS Code integration:\n",
        "  1. Run rustyclaw in the VS Code integrated terminal\n",
        "  2. Full tool access — Bash, Read, Write, Edit, Grep, etc.\n\n",
        "For JetBrains IDEs:\n",
        "  1. Run rustyclaw in the built-in terminal\n",
        "  2. Or run rustyclaw in the built-in terminal\n\n",
        "The LSP tool provides code intelligence directly in the chat:\n",
        "  Use the LSP tool to query language servers for definitions, references, hover docs, etc."
    ).into())
}

fn cmd_env(ctx: &CommandContext) -> CommandAction {
    let mut lines = vec!["Environment\n".to_string()];

    lines.push(format!("CWD:     {}", ctx.config.cwd.display()));
    lines.push(format!("Model:   {}", ctx.config.model));
    lines.push(format!(
        "Shell:   {}",
        std::env::var("SHELL").unwrap_or_else(|_| "unknown".into())
    ));
    lines.push(format!(
        "User:    {}",
        std::env::var("USER")
            .or_else(|_| std::env::var("USERNAME"))
            .unwrap_or_else(|_| "unknown".into())
    ));
    lines.push(format!(
        "HOME:    {}",
        std::env::var("HOME").unwrap_or_else(|_| "unknown".into())
    ));

    if let Ok(path) = std::env::var("PATH") {
        let entries: Vec<&str> = path.split(':').take(5).collect();
        lines.push(format!("PATH:    {} ...", entries.join(":")));
    }

    lines.push(String::new());
    lines.push(format!(
        "ANTHROPIC_API_KEY: {}",
        if ctx.config.api_key.is_empty() {
            "not set".to_string()
        } else {
            format!(
                "{}...",
                &ctx.config.api_key[..4.min(ctx.config.api_key.len())]
            )
        }
    ));

    CommandAction::Message(lines.join("\n"))
}

fn cmd_output_style(args: &str, ctx: &CommandContext) -> CommandAction {
    // Note: in the upstream source this command is deprecated in favour of /config.
    // We keep it functional here since output styles are a real feature.
    let style_name = args.trim();

    // No args — list available styles
    if style_name.is_empty() {
        let styles = crate::config::Config::load_output_styles(&ctx.config.cwd);
        let active = ctx.config.output_style.as_deref().unwrap_or("default");
        let mut lines = vec![
            "Output Style\n".into(),
            format!("Active: {}\n", active),
            "Available styles:\n".into(),
            "  default         — normal Claude responses (no style override)\n".into(),
        ];
        for s in &styles {
            let marker = if active.eq_ignore_ascii_case(&s.name) {
                " ◀"
            } else {
                ""
            };
            lines.push(format!(
                "  {:<16} — {} [{}]{}\n",
                s.name, s.description, s.source, marker
            ));
        }
        lines.push("\nUsage: /output-style <name>  (e.g. /output-style Explanatory)\n".into());
        lines.push("       /output-style default  — clear active style\n".into());
        return CommandAction::Message(lines.join(""));
    }

    CommandAction::SetOutputStyle(style_name.to_string())
}

fn cmd_theme(args: &str, ctx: &CommandContext) -> CommandAction {
    let theme = args.trim().to_lowercase();
    let valid = ["dark", "light", "solarized"];
    if theme.is_empty() {
        let active = ctx.config.theme.as_deref().unwrap_or("dark");
        return CommandAction::Message(format!(
            "Theme\n\nActive: {}\nAvailable: dark, light, solarized\n\nUsage: /theme <name>",
            active
        ));
    }
    if !valid.contains(&theme.as_str()) {
        return CommandAction::Message(format!(
            "Unknown theme '{}'. Available: dark, light, solarized",
            theme
        ));
    }
    CommandAction::SetTheme(theme)
}

fn cmd_upgrade() -> CommandAction {
    CommandAction::CheckUpgrade
}

fn cmd_advisor(args: &str) -> CommandAction {
    let topic = args.trim();
    if topic.is_empty() {
        CommandAction::SendPrompt(
            "Act as a senior software architect advisor. Review this codebase and conversation, \
             then provide strategic recommendations on: architecture improvements, technology choices, \
             scalability considerations, and best practices. Be specific and actionable."
                .into()
        )
    } else {
        CommandAction::SendPrompt(format!(
            "Act as a senior software architect advisor and provide expert advice on: {topic}. \
             Be specific, actionable, and consider trade-offs."
        ))
    }
}

fn cmd_brief(_ctx: &CommandContext) -> CommandAction {
    CommandAction::ToggleBriefMode
}

fn cmd_btw(args: &str) -> CommandAction {
    let note = args.trim();
    if note.is_empty() {
        CommandAction::Message(
            "Usage: /btw <note>\n\n\
             Prepends a note to your next message. Useful for providing context \
             without making it the main focus of your prompt.\n\n\
             Example: /btw I'm using Python 3.11 on macOS\n\
             Then your next message will include that context automatically."
                .into(),
        )
    } else {
        CommandAction::SetBtwNote(note.to_string())
    }
}

fn cmd_ctx_viz(ctx: &CommandContext) -> CommandAction {
    let limit: u64 = 200_000;
    let used = ctx.tokens_in;
    let pct = if limit > 0 { used * 100 / limit } else { 0 };

    // Build a visual histogram of context usage
    let bar_width = 40usize;
    let filled = (pct as usize * bar_width / 100).min(bar_width);

    let color_bar = if pct >= 90 {
        format!("{}{}", "▓".repeat(filled), "░".repeat(bar_width - filled))
    } else if pct >= 70 {
        format!("{}{}", "▒".repeat(filled), "░".repeat(bar_width - filled))
    } else {
        format!("{}{}", "█".repeat(filled), "░".repeat(bar_width - filled))
    };

    let thresholds = [
        (crate::compact::COMPACT_WARN_TOKENS, "warn"),
        (crate::compact::COMPACT_SNIP_TOKENS, "snip"),
        (crate::compact::COMPACT_SUMMARISE_TOKENS, "summarise"),
    ];

    let mut lines = vec![
        "Context Visualizer\n".to_string(),
        format!("[{color_bar}] {pct}%"),
        format!("{used} / {limit} tokens used"),
        String::new(),
        "Thresholds:".to_string(),
    ];

    for (threshold, label) in &thresholds {
        let t_pct = threshold * 100 / limit;
        let marker_pos = (t_pct as usize * bar_width / 100).min(bar_width);
        lines.push(format!(
            "  {threshold:>7} ({t_pct}%) — auto-{label}  {}",
            " ".repeat(marker_pos) + "^"
        ));
    }

    lines.push(String::new());
    if pct >= 90 {
        lines.push("⚠ Context nearly full — consider /compact".to_string());
    } else if pct >= 70 {
        lines.push("Context usage is elevated — monitor closely.".to_string());
    } else {
        lines.push("Context usage is healthy.".to_string());
    }

    CommandAction::Message(lines.join("\n"))
}

fn cmd_init_verifiers() -> CommandAction {
    CommandAction::SendPrompt(
        r#"Use the TodoWrite tool to track your progress through this multi-step task.

## Goal

Create one or more verifier skills that can be used by the Verify agent to automatically verify code changes in this project or folder. You may create multiple verifiers if the project has different verification needs (e.g., both web UI and API endpoints).

**Do NOT create verifiers for unit tests or typechecking.** Those are already handled by the standard build/test workflow and don't need dedicated verifier skills. Focus on functional verification: web UI (Playwright), CLI (Tmux), and API (HTTP) verifiers.

## Phase 1: Auto-Detection

Analyze the project to detect what's in different subdirectories. The project may contain multiple sub-projects or areas that need different verification approaches (e.g., a web frontend, an API backend, and shared libraries all in one repo).

1. **Scan top-level directories** to identify distinct project areas:
   - Look for separate package.json, Cargo.toml, pyproject.toml, go.mod in subdirectories
   - Identify distinct application types in different folders

2. **For each area, detect:**

   a. **Project type and stack** — Primary language(s), frameworks, package managers

   b. **Application type**
      - Web app (React, Next.js, Vue, etc.) -> suggest Playwright-based verifier
      - CLI tool -> suggest Tmux-based verifier
      - API service (Express, FastAPI, etc.) -> suggest HTTP-based verifier

   c. **Existing verification tools** — Test frameworks, E2E tools, dev server scripts

   d. **Dev server configuration** — How to start, URL, ready signal

3. **Installed verification packages** (for web apps)
   - Check if Playwright is installed (look in package.json dependencies/devDependencies)
   - Check MCP configuration (.mcp.json) for browser automation tools

## Phase 2: Verification Tool Setup

Based on what was detected in Phase 1, help the user set up appropriate verification tools.

### For Web Applications

1. **If browser automation tools are already installed/configured**, ask the user which one they want to use via AskUserQuestion.

2. **If NO browser automation tools are detected**, ask if they want to install/configure one:
   - Options: Playwright (Recommended), Chrome DevTools MCP, Claude Chrome Extension, None

3. **If user chooses to install Playwright**, run the appropriate command based on package manager.

4. **If user chooses Chrome DevTools MCP or Claude Chrome Extension**, configure .mcp.json accordingly.

### For CLI Tools

1. Check if asciinema is available (run `which asciinema`).
2. Tmux is typically system-installed, just verify it's available.

### For API Services

1. Check if HTTP testing tools are available (curl, httpie).

## Phase 3: Interactive Q&A

For each distinct area, use AskUserQuestion to confirm:

1. **Verifier name** — suggest based on detection:
   - Single area: "verifier-playwright", "verifier-cli", "verifier-api"
   - Multiple areas: "verifier-<project>-<type>" (e.g., "verifier-frontend-playwright")
   - MUST include "verifier" in the name for auto-discovery

2. **Project-specific questions** based on type (dev server command, URL, ready signal, etc.)

3. **Authentication & Login** — ask if the app requires authentication to access pages/endpoints being verified.

## Phase 4: Generate Verifier Skill

Write the skill file to `.claude/skills/<verifier-name>/SKILL.md`.

Use this template:

```
---
name: <verifier-name>
description: <description based on type>
allowed-tools:
  # Tools appropriate for the verifier type
---

# <Verifier Title>

You are a verification executor. You receive a verification plan and execute it EXACTLY as written.

## Project Context
<Project-specific details from detection>

## Setup Instructions
<How to start any required services>

## Authentication
<If auth is required, include step-by-step login instructions here>
<If no auth needed, omit this section>

## Reporting

Report PASS or FAIL for each step using the format specified in the verification plan.

## Cleanup

After verification:
1. Stop any dev servers started
2. Close any browser sessions
3. Report final summary

## Self-Update

If verification fails because this skill's instructions are outdated (not because the feature under test is broken), use AskUserQuestion to confirm and then Edit this SKILL.md with a minimal targeted fix.
```

Allowed tools by type:
- verifier-playwright: Bash(npm:*), Bash(yarn:*), Bash(pnpm:*), Bash(bun:*), mcp__playwright__*, Read, Glob, Grep
- verifier-cli: Tmux, Bash(asciinema:*), Read, Glob, Grep
- verifier-api: Bash(curl:*), Bash(http:*), Bash(npm:*), Bash(yarn:*), Read, Glob, Grep

## Phase 5: Confirm Creation

After writing the skill file(s), inform the user:
1. Where each skill was created (always in `.claude/skills/`)
2. How the Verify agent will discover them (folder name must contain "verifier")
3. That they can edit the skills to customize them
4. That they can run /init-verifiers again to add more verifiers for other areas"#.into()
    )
}

fn cmd_agents(ctx: &CommandContext) -> CommandAction {
    // List agents defined in .claude/agents/ directories
    let mut search_dirs = vec![ctx.config.cwd.join(".claude").join("agents")];
    if let Some(home) = dirs::home_dir() {
        search_dirs.push(home.join(".claude").join("agents"));
    }

    let mut lines = vec!["Agents\n".to_string()];
    let mut total = 0usize;

    for dir in &search_dirs {
        if !dir.exists() {
            continue;
        }
        let label = if dir.starts_with(&ctx.config.cwd) {
            "Project agents"
        } else {
            "Global agents"
        };
        lines.push(format!("{label}:"));

        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => continue,
        };

        let mut found = false;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let name = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("")
                    .to_string();
                // Read description from AGENT.md if present
                let agent_md = path.join("AGENT.md");
                let description = if agent_md.exists() {
                    std::fs::read_to_string(&agent_md)
                        .ok()
                        .and_then(|content| {
                            content
                                .lines()
                                .find(|l| {
                                    !l.trim().is_empty()
                                        && !l.starts_with("---")
                                        && !l.starts_with('#')
                                })
                                .map(|l| l.trim().to_string())
                        })
                        .unwrap_or_default()
                } else {
                    String::new()
                };
                if description.is_empty() {
                    lines.push(format!("  {name}"));
                } else {
                    lines.push(format!("  {name} — {description}"));
                }
                total += 1;
                found = true;
            } else if path.extension().and_then(|e| e.to_str()) == Some("md") {
                let name = path
                    .file_stem()
                    .and_then(|n| n.to_str())
                    .unwrap_or("")
                    .to_string();
                lines.push(format!("  {name}"));
                total += 1;
                found = true;
            }
        }
        if !found {
            lines.push("  (none)".into());
        }
        lines.push(String::new());
    }

    if total == 0 {
        lines.push(
            "No agents found. Create agent definitions in .claude/agents/ or ~/.claude/agents/."
                .into(),
        );
    } else {
        lines.insert(1, format!("{total} agent(s) found\n"));
    }

    CommandAction::Message(lines.join("\n"))
}

fn cmd_stats(ctx: &CommandContext) -> CommandAction {
    // Show usage statistics from session history
    let sessions_dir = dirs::home_dir()
        .map(|h| h.join(".claude").join("sessions"))
        .unwrap_or_else(|| std::path::PathBuf::from(".claude/sessions"));

    let mut total_sessions = 0usize;
    let mut total_tokens_in: u64 = 0;
    let mut total_tokens_out: u64 = 0;

    if sessions_dir.exists()
        && let Ok(entries) = std::fs::read_dir(&sessions_dir)
    {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("meta") {
                total_sessions += 1;
                if let Ok(data) = std::fs::read_to_string(&path)
                    && let Ok(json) = serde_json::from_str::<serde_json::Value>(&data)
                {
                    total_tokens_in += json["tokens_in"].as_u64().unwrap_or(0);
                    total_tokens_out += json["tokens_out"].as_u64().unwrap_or(0);
                }
            }
        }
    }

    let (price_in, price_out) = {
        let m = &ctx.config.model;
        if m.contains("opus-4") {
            (15.0f64, 75.0f64)
        } else if m.contains("sonnet-4") {
            (3.0, 15.0)
        } else if m.contains("haiku") {
            (0.25, 1.25)
        } else {
            (3.0, 15.0)
        }
    };
    let cost = (total_tokens_in as f64 / 1_000_000.0 * price_in)
        + (total_tokens_out as f64 / 1_000_000.0 * price_out);

    let session_in = ctx.tokens_in;
    let session_out = ctx.tokens_out;
    let session_cost = (session_in as f64 / 1_000_000.0 * price_in)
        + (session_out as f64 / 1_000_000.0 * price_out);

    let lines = vec![
        "Usage Statistics\n".to_string(),
        "Current session:".to_string(),
        format!("  Tokens in:  {session_in}"),
        format!("  Tokens out: {session_out}"),
        format!("  Est. cost:  ${session_cost:.4}"),
        String::new(),
        format!("All sessions ({total_sessions} total):"),
        format!("  Tokens in:  {total_tokens_in}"),
        format!("  Tokens out: {total_tokens_out}"),
        format!("  Est. cost:  ${cost:.4}"),
        String::new(),
        format!("Model: {}", ctx.config.model),
        format!("Pricing: ${price_in}/M in, ${price_out}/M out"),
    ];
    CommandAction::Message(lines.join("\n"))
}

fn cmd_statusline(args: &str) -> CommandAction {
    let prompt = if args.trim().is_empty() {
        "Configure my statusLine from my shell PS1 configuration".to_string()
    } else {
        args.trim().to_string()
    };
    CommandAction::SendPrompt(format!(
        "Create an Agent with subagent_type \"statusline-setup\" and the prompt \"{prompt}\""
    ))
}

// ── Voice ──────────────────────────────────────────────────────────────────────

fn cmd_voice(args: &str, ctx: &CommandContext) -> CommandAction {
    let trimmed = args.trim();
    match trimmed {
        "enable" => CommandAction::SetVoiceEnabled(true),
        "disable" => CommandAction::SetVoiceEnabled(false),
        "speak on" => CommandAction::SetTtsEnabled(true),
        "speak off" => CommandAction::SetTtsEnabled(false),
        "model" => CommandAction::ListVoiceModels,
        "test" => CommandAction::VoiceTest,
        "clone remove" => CommandAction::VoiceCloneRemove,
        _ if trimmed.starts_with("clone save") => {
            let tier = trimmed.strip_prefix("clone save").unwrap_or("").trim();
            CommandAction::VoiceCloneSave(tier.to_string())
        }
        _ if trimmed.starts_with("clone") => {
            let tier_arg = trimmed.strip_prefix("clone").unwrap_or("").trim();
            if tier_arg.is_empty() {
                // Show tier picker
                CommandAction::VoiceClone(String::new())
            } else {
                CommandAction::VoiceClone(tier_arg.to_string())
            }
        }
        _ => CommandAction::Message(crate::voice::voice_status(
            ctx.config.voice_enabled,
            ctx.config.tts_enabled,
        )),
    }
}

// ── Sandbox ────────────────────────────────────────────────────────────────────

fn cmd_sandbox(args: &str, ctx: &CommandContext) -> CommandAction {
    let (sub, rest) = split_first_word(args);
    match sub {
        "enable" => {
            let mode = if rest.is_empty() {
                crate::sandbox::best_available_mode().to_string()
            } else {
                match rest {
                    "strict" | "bwrap" | "firejail" => rest.to_string(),
                    other => {
                        return CommandAction::Message(format!(
                            "Unknown sandbox mode '{other}'. Valid: strict, bwrap, firejail"
                        ));
                    }
                }
            };
            CommandAction::SetSandboxEnabled {
                enabled: true,
                mode,
            }
        }
        "disable" => CommandAction::SetSandboxEnabled {
            enabled: false,
            mode: String::new(),
        },
        "network" => match rest {
            "on" | "allow" => CommandAction::SetSandboxNetwork(true),
            "off" | "block" => CommandAction::SetSandboxNetwork(false),
            _ => CommandAction::Message(format!(
                "Network is currently: {}\n\n\
                 /sandbox network on   — allow outbound network (bwrap mode)\n\
                 /sandbox network off  — block outbound network (bwrap mode)",
                if ctx.config.sandbox_allow_network {
                    "on (allowed)"
                } else {
                    "off (blocked)"
                }
            )),
        },
        _ => CommandAction::Message(crate::sandbox::sandbox_status(
            ctx.config.sandbox_enabled,
            &ctx.config.sandbox_mode,
        )),
    }
}

// ── Teleport ───────────────────────────────────────────────────────────────────

fn cmd_teleport(args: &str) -> CommandAction {
    match args.trim() {
        "export" => CommandAction::TeleportExport,
        "import" => CommandAction::TeleportImport,
        _ => CommandAction::Message(
            "Teleport — session context transfer\n\n\
             /teleport export   — save session context to ~/.claude/teleport.json\n\
             /teleport import   — load session context from ~/.claude/teleport.json\n\n\
             Use this to transfer a conversation to another terminal or instance."
                .into(),
        ),
    }
}

// ── Feedback ───────────────────────────────────────────────────────────────────

fn cmd_feedback() -> CommandAction {
    CommandAction::OpenBrowser("https://github.com/ForkedInTime/RustyClaw/issues".into())
}

// ── Terminal Setup ─────────────────────────────────────────────────────────────

fn cmd_terminal_setup() -> CommandAction {
    let term = std::env::var("TERM").unwrap_or_else(|_| "unknown".into());
    let colorterm = std::env::var("COLORTERM").unwrap_or_default();
    let term_prog = std::env::var("TERM_PROGRAM").unwrap_or_default();
    let term_ver = std::env::var("TERM_PROGRAM_VERSION").unwrap_or_default();

    // Detect color depth
    let color_support = if colorterm == "truecolor" || colorterm == "24bit" {
        "✓ Truecolor (24-bit) — optimal"
    } else if colorterm == "256color" || term.contains("256color") {
        "  256-color — themes will work, not optimal"
    } else {
        "  Basic color — consider a modern terminal for best experience"
    };

    // Identify terminal
    let terminal_name = if !term_prog.is_empty() {
        if term_ver.is_empty() {
            term_prog.clone()
        } else {
            format!("{} {}", term_prog, term_ver)
        }
    } else if term.contains("kitty") {
        "kitty".into()
    } else if term.contains("alacritty") {
        "alacritty".into()
    } else if term.contains("xterm") {
        "xterm".into()
    } else {
        term.clone()
    };

    // Unicode check (basic — if LANG includes UTF-8 we're probably fine)
    let lang = std::env::var("LANG").unwrap_or_default();
    let unicode_ok = lang.to_lowercase().contains("utf") || lang.to_lowercase().contains("utf-8");
    let unicode_status = if unicode_ok {
        "✓ UTF-8 locale detected"
    } else {
        "  LANG not UTF-8 — set LANG=en_US.UTF-8 for best display"
    };

    let msg = format!(
        "Terminal Setup\n\n\
         Detected:\n\
           Terminal:  {terminal_name}\n\
           TERM:      {term}\n\
           Color:     {color_support}\n\
           Unicode:   {unicode_status}\n\n\
         Recommended terminals (truecolor + Unicode):\n\
           kitty, alacritty, wezterm, iTerm2, ghostty\n\n\
         Themes:\n\
           /theme dark        — default dark theme\n\
           /theme light       — light theme\n\
           /theme solarized   — solarized theme\n\n\
         Voice input:\n\
           /voice             — show voice setup status\n\
           /voice enable      — enable Ctrl+R recording\n\
           Install: sudo apt install ffmpeg           (recorder)\n\
                    pip install openai-whisper        (offline transcription)\n\n\
         Sandbox:\n\
           /sandbox           — show sandbox status\n\
           Install: sudo apt install bubblewrap       (bwrap mode)\n\
                    sudo apt install firejail         (firejail mode)"
    );
    CommandAction::Message(msg)
}

// ── Share ─────────────────────────────────────────────────────────────────────

fn cmd_share(args: &str) -> CommandAction {
    match args.trim() {
        "clip" | "clipboard" => CommandAction::ShareClipboard,
        "" => CommandAction::ShareSession,
        _ => CommandAction::Message(
            "Share session\n\n\
             /share          — export session to a markdown file in the current directory\n\
             /share clip     — copy session markdown to clipboard (requires xclip or wl-copy)"
                .into(),
        ),
    }
}

// ── Notifications ──────────────────────────────────────────────────────────────

fn cmd_notifications(args: &str, ctx: &CommandContext) -> CommandAction {
    match args.trim() {
        "enable" | "on" => CommandAction::SetNotificationsEnabled(true),
        "disable" | "off" => CommandAction::SetNotificationsEnabled(false),
        _ => CommandAction::Message(format!(
            "Notifications  {}\n\n\
             /notifications enable   — fire terminal bell + notify-send on task completion\n\
             /notifications disable  — disable notifications\n\n\
             Requires: notify-send (sudo apt install libnotify-bin)",
            if ctx.config.notifications_enabled {
                "ENABLED"
            } else {
                "DISABLED"
            }
        )),
    }
}

// ── Release Notes ──────────────────────────────────────────────────────────────

fn cmd_release_notes(_args: &str) -> CommandAction {
    CommandAction::Message(
        "Release Notes — RustyClaw v0.1.0\n\n\
         Features:\n\
           • Voice input (/voice) — audio capture via arecord/sox/ffmpeg\n\
             + transcription via local whisper CLI or OpenAI-compatible API\n\
           • XTTS v2 TTS — voice cloning with custom voice models\n\
           • Codebase RAG (/rag) — tree-sitter AST parsing, 8 languages, FTS5 search\n\
           • Smart model router — auto-route by task complexity\n\
           • Cost dashboard (/cost, /budget) — real-time token/cost tracking\n\
           • Parallel agents (/spawn) — background agents in git worktrees\n\
           • OpenAI-compatible providers — Groq, OpenRouter, DeepSeek, LM Studio, etc.\n\
           • SDK mode (--headless) — NDJSON stdio server for editor/CI embedding\n\
           • Sandbox (/sandbox) — bwrap / firejail execution isolation\n\
           • Session management (/session) — save, resume, search, export\n\
           • Output styles (/output-style) — Explanatory, Learning, custom .md files\n\
           • Themes (/theme) — dark, light, solarized\n\n\
         See CHANGELOG.md for full history.\n\
         See https://github.com/ForkedInTime/RustyClaw/releases for downloads."
            .into(),
    )
}

fn cmd_plugin(args: &str) -> CommandAction {
    let (sub, rest) = split_first_word(args);
    match sub {
        "marketplace" | "market" => {
            let (action, target) = split_first_word(rest);
            match action {
                "add" if !target.is_empty() => {
                    CommandAction::PluginInstall(format!("marketplace:{}", target))
                }
                "remove" | "rm" if !target.is_empty() => {
                    // Remove from plugins.json mcpServers entry
                    CommandAction::PluginRemove(target.to_string())
                }
                "update" if !target.is_empty() => {
                    // Re-install to update
                    CommandAction::PluginInstall(format!("marketplace:{}", target))
                }
                "list" | "" => plugin_marketplace_list(),
                _ => CommandAction::Message(
                    "Usage: /plugin marketplace <add|remove|update|list> [target]\n\
                     Example: /plugin marketplace add mksglu/context-mode"
                        .into(),
                ),
            }
        }
        "install" | "i" if !rest.is_empty() => CommandAction::PluginInstall(rest.to_string()),
        "install" | "i" => CommandAction::Message(
            "Usage: /plugin install <package[@version]>\n\
                 Example: /plugin install @modelcontextprotocol/server-github"
                .into(),
        ),
        "remove" | "uninstall" | "rm" if !rest.is_empty() => {
            CommandAction::PluginRemove(rest.to_string())
        }
        "remove" | "uninstall" | "rm" => {
            CommandAction::Message("Usage: /plugin remove <name>".into())
        }
        "enable" if !rest.is_empty() => plugin_set_enabled(rest, true),
        "disable" if !rest.is_empty() => plugin_set_enabled(rest, false),
        "enable" | "disable" => CommandAction::Message(format!("Usage: /plugin {sub} <name>")),
        "validate" => {
            let path = rest.trim();
            if path.is_empty() {
                CommandAction::Message(
                    "Usage: /plugin validate [path]\n\
                     Validates a plugin's package.json and MCP server configuration.\n\
                     Without a path, validates all installed plugins."
                        .into(),
                )
            } else {
                plugin_validate(path)
            }
        }
        "manage" => {
            // Show installed plugins with enable/disable options
            plugin_manage_list()
        }
        "" | "list" | "status" => CommandAction::PluginList,
        "help" | "--help" | "-h" => CommandAction::Message(
            concat!(
                "Plugin management\n\n",
                "  /plugin list                         — list installed plugins\n",
                "  /plugin install <pkg>                — install npm package as MCP server\n",
                "  /plugin remove <name>                — uninstall plugin\n",
                "  /plugin enable <name>                — enable a disabled plugin\n",
                "  /plugin disable <name>               — disable a plugin\n",
                "  /plugin validate [path]              — validate plugin configuration\n",
                "  /plugin manage                       — show all plugins with status\n",
                "  /plugin marketplace add <repo>       — install from GitHub marketplace\n",
                "  /plugin marketplace remove <name>    — remove marketplace plugin\n",
                "  /plugin marketplace list             — list marketplace sources\n\n",
                "Examples:\n",
                "  /plugin install @modelcontextprotocol/server-github\n",
                "  /plugin marketplace add mksglu/context-mode\n",
                "  /plugin disable context-mode"
            )
            .into(),
        ),
        _ => CommandAction::Message(format!(
            "Unknown plugin subcommand '{sub}'. Try /plugin help."
        )),
    }
}

fn plugin_marketplace_list() -> CommandAction {
    let plugins_path = Config::claude_dir().join("plugins.json");
    let content = std::fs::read_to_string(&plugins_path).unwrap_or_default();
    let plugins: serde_json::Value =
        serde_json::from_str(&content).unwrap_or(serde_json::json!({}));

    if let Some(obj) = plugins.as_object() {
        let marketplace_plugins: Vec<_> = obj
            .iter()
            .filter(|(_, v)| {
                v.get("source")
                    .and_then(|s| s.as_str())
                    .is_some_and(|s| s.contains("marketplace") || s.contains("github"))
            })
            .collect();
        if marketplace_plugins.is_empty() {
            CommandAction::Message(
                "No marketplace plugins installed.\n\
                 Install one: /plugin marketplace add <github-user/repo>"
                    .into(),
            )
        } else {
            let lines: Vec<String> = marketplace_plugins
                .iter()
                .map(|(name, v)| {
                    let src = v
                        .get("source")
                        .and_then(|s| s.as_str())
                        .unwrap_or("unknown");
                    format!("  {name}  (from {src})")
                })
                .collect();
            CommandAction::Message(format!("Marketplace plugins\n\n{}", lines.join("\n")))
        }
    } else {
        CommandAction::Message("No plugins installed.".into())
    }
}

fn plugin_set_enabled(name: &str, enabled: bool) -> CommandAction {
    // Toggle disabled flag in settings.json mcpServers entry
    let settings_path = Config::claude_dir().join("settings.json");
    let content = std::fs::read_to_string(&settings_path).unwrap_or_else(|_| "{}".to_string());
    let mut val: serde_json::Value =
        serde_json::from_str(&content).unwrap_or(serde_json::json!({}));

    let server = val
        .get_mut("mcpServers")
        .and_then(|m| m.as_object_mut())
        .and_then(|m| m.get_mut(name));

    match server {
        None => CommandAction::Message(format!(
            "Plugin/MCP server '{}' not found in settings.json.\n\
             List installed plugins: /plugin list",
            name
        )),
        Some(s) => {
            if let Some(obj) = s.as_object_mut() {
                if enabled {
                    obj.remove("disabled");
                } else {
                    obj.insert("disabled".to_string(), serde_json::json!(true));
                }
            }
            match serde_json::to_string_pretty(&val)
                .ok()
                .and_then(|s| std::fs::write(&settings_path, s).ok())
            {
                Some(_) => CommandAction::Message(format!(
                    "Plugin '{}' {}. Restart rustyclaw to apply.",
                    name,
                    if enabled { "enabled" } else { "disabled" }
                )),
                None => CommandAction::Message("Failed to write settings.json".into()),
            }
        }
    }
}

fn plugin_validate(path: &str) -> CommandAction {
    use std::path::Path;
    let p = Path::new(path);
    let pkg_json = if p.is_dir() {
        p.join("package.json")
    } else {
        p.to_path_buf()
    };

    let content = match std::fs::read_to_string(&pkg_json) {
        Ok(c) => c,
        Err(e) => {
            return CommandAction::Message(format!(
                "Cannot read {}: {e}\nMake sure the path points to a plugin directory or package.json",
                pkg_json.display()
            ));
        }
    };

    let pkg: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(e) => return CommandAction::Message(format!("Invalid package.json: {e}")),
    };

    let mut issues: Vec<String> = Vec::new();
    let mut ok: Vec<String> = Vec::new();

    if pkg.get("name").and_then(|v| v.as_str()).is_some() {
        ok.push("name: present".into());
    } else {
        issues.push("MISSING: 'name' field in package.json".into());
    }

    if pkg.get("version").and_then(|v| v.as_str()).is_some() {
        ok.push("version: present".into());
    } else {
        issues.push("MISSING: 'version' field in package.json".into());
    }

    let has_bin = pkg.get("bin").is_some();
    let has_main = pkg.get("main").is_some();
    if has_bin || has_main {
        ok.push(
            if has_bin {
                "bin: present (MCP entry point)"
            } else {
                "main: present"
            }
            .into(),
        );
    } else {
        issues.push(
            "WARNING: No 'bin' or 'main' in package.json — may not work as MCP server".into(),
        );
    }

    let result = if issues.is_empty() {
        format!(
            "Plugin validation OK\n{}\n\n{}",
            pkg_json.display(),
            ok.join("\n")
        )
    } else {
        format!(
            "Plugin validation issues in {}\n\nIssues:\n{}\n\nOK:\n{}",
            pkg_json.display(),
            issues.join("\n"),
            ok.join("\n")
        )
    };
    CommandAction::Message(result)
}

fn plugin_manage_list() -> CommandAction {
    let settings_path = Config::claude_dir().join("settings.json");
    let content = std::fs::read_to_string(&settings_path).unwrap_or_default();
    let val: serde_json::Value = serde_json::from_str(&content).unwrap_or(serde_json::json!({}));

    let plugins_path = Config::claude_dir().join("plugins.json");
    let plugins_content = std::fs::read_to_string(&plugins_path).unwrap_or_default();
    let plugins: serde_json::Value =
        serde_json::from_str(&plugins_content).unwrap_or(serde_json::json!({}));

    let mcp_servers = val.get("mcpServers").and_then(|m| m.as_object());

    if mcp_servers.is_none_or(|m| m.is_empty()) && plugins.as_object().is_none_or(|m| m.is_empty())
    {
        return CommandAction::Message(
            "No plugins installed.\n\
             Install one: /plugin install <package>"
                .into(),
        );
    }

    let mut lines = vec!["Installed plugins\n".to_string()];

    if let Some(servers) = mcp_servers {
        for (name, cfg) in servers {
            let disabled = cfg
                .get("disabled")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let transport = if cfg.get("url").is_some() {
                "HTTP"
            } else {
                "stdio"
            };
            let status = if disabled { "DISABLED" } else { "enabled " };
            lines.push(format!("  [{status}] {name}  ({transport})"));
        }
    }

    lines.push(String::new());
    lines.push("  /plugin enable <name>   /plugin disable <name>   /plugin remove <name>".into());

    CommandAction::Message(lines.join("\n"))
}

// ── New commands from TS source ───────────────────────────────────────────────

fn cmd_spawn(args: &str) -> CommandAction {
    let args = args.trim();
    if args.is_empty() {
        return CommandAction::Message(
            "Usage:\n\
             /spawn <task>           — spawn a background agent to work on <task>\n\
             /spawn list             — list all spawned agents\n\
             /spawn review <id>      — review a completed agent's changes\n\
             /spawn merge <id>       — merge an agent's changes into current branch\n\
             /spawn kill <id>        — cancel a running agent\n\
             /spawn discard <id>     — discard an agent's worktree\n\n\
             Example: /spawn refactor the auth module to use JWT tokens"
                .into(),
        );
    }

    let (sub, rest) = split_first_word(args);
    match sub {
        "list" | "ls" => CommandAction::ListSpawns,
        "review" | "diff" => {
            if rest.is_empty() {
                CommandAction::Message("Usage: /spawn review <agent-id>".into())
            } else {
                CommandAction::ReviewSpawn(rest.to_string())
            }
        }
        "merge" => {
            if rest.is_empty() {
                CommandAction::Message("Usage: /spawn merge <agent-id>".into())
            } else {
                CommandAction::MergeSpawn(rest.to_string())
            }
        }
        "kill" | "cancel" => {
            if rest.is_empty() {
                CommandAction::Message("Usage: /spawn kill <agent-id>".into())
            } else {
                CommandAction::KillSpawn(rest.to_string())
            }
        }
        "discard" | "drop" => {
            if rest.is_empty() {
                CommandAction::Message("Usage: /spawn discard <agent-id>".into())
            } else {
                CommandAction::DiscardSpawn(rest.to_string())
            }
        }
        // Everything else is the task description
        _ => CommandAction::SpawnAgent(args.to_string()),
    }
}

fn cmd_ultraplan(args: &str) -> CommandAction {
    let depth = args.trim();
    let prompt = if depth == "deep" || depth == "max" {
        "Enter ULTRAPLAN mode. You are a senior engineering lead.\n\
         Perform the deepest possible analysis of this codebase and conversation context:\n\
         1. Map every file and module — purpose, dependencies, interfaces\n\
         2. Identify ALL technical debt, bugs, and inconsistencies\n\
         3. Draw the full data flow and control flow diagrams (in text)\n\
         4. Produce a prioritised implementation plan with concrete steps\n\
         5. Flag every assumption and risk\n\
         Use extended thinking. Be exhaustive. Do not summarise — detail everything."
    } else {
        "Enter ULTRAPLAN mode. Produce a comprehensive implementation plan:\n\
         1. Analyse the current codebase structure and relevant context\n\
         2. Break the work into concrete, ordered tasks with clear acceptance criteria\n\
         3. Identify dependencies, blockers, and risks for each task\n\
         4. Estimate complexity (S/M/L/XL) for each task\n\
         5. Recommend the optimal implementation sequence\n\
         Be specific and actionable. Use TodoWrite to record the plan."
    };
    CommandAction::SendPrompt(prompt.into())
}

fn cmd_autofix_pr(args: &str) -> CommandAction {
    let pr_ref = args.trim();
    if pr_ref.is_empty() {
        return CommandAction::Message(
            "Auto-fix PR review comments.\n\n\
             Usage: /autofix-pr [<pr-number-or-url>]\n\
             Example: /autofix-pr 42\n\n\
             Without an argument, fixes comments on the current branch's open PR.\n\
             Requires: gh CLI installed and authenticated."
                .into(),
        );
    }
    CommandAction::SendPrompt(format!(
        "Using the gh CLI, read all review comments on PR {pr_ref}.\n\
         For each actionable comment:\n\
         1. Understand what change is needed\n\
         2. Make the change in the relevant file\n\
         3. Note what was changed\n\
         After all fixes, run any relevant tests and commit with a summary message."
    ))
}

fn cmd_autocommit(_args: &str) -> CommandAction {
    // v1 only supports `status`. Any arg (or none) shows status.
    CommandAction::AutoCommitStatus
}

fn cmd_redo(args: &str) -> CommandAction {
    let n = args.trim().parse::<u32>().ok().filter(|n| *n > 0);
    CommandAction::Redo { n }
}

fn cmd_undo(args: &str) -> CommandAction {
    let n = args.trim().parse::<u32>().ok().filter(|n| *n > 0);
    CommandAction::Undo { n }
}

fn cmd_issue(args: &str) -> CommandAction {
    let desc = args.trim();
    if desc.is_empty() {
        return CommandAction::Message(
            "Create a GitHub issue.\n\n\
             Usage: /issue <description>\n\
             Example: /issue login fails when username contains spaces\n\n\
             Requires: gh CLI installed and authenticated (gh auth login)"
                .into(),
        );
    }
    CommandAction::SendPrompt(format!(
        "Create a GitHub issue for: {desc}\n\n\
         Steps:\n\
         1. Analyse the codebase and conversation for relevant context\n\
         2. Draft a structured issue: title, description, steps to reproduce, \
            expected vs actual behaviour, environment\n\
         3. Use the gh CLI: gh issue create --title '...' --body '...'\n\
         4. Return the issue URL."
    ))
}

fn cmd_color(args: &str) -> CommandAction {
    match args.trim() {
        "" => {
            let colorterm = std::env::var("COLORTERM").unwrap_or_default();
            let term = std::env::var("TERM").unwrap_or_else(|_| "unknown".into());
            CommandAction::Message(format!(
                "Color Settings\n\n\
                 TERM={term}\n\
                 COLORTERM={colorterm}\n\n\
                 To enable truecolor: export COLORTERM=truecolor\n\
                 To change theme:     /theme dark | light | solarized\n\
                 Full diagnostics:    /terminal-setup"
            ))
        }
        other => CommandAction::Message(format!(
            "Unknown option '{other}'.\nUsage: /color  — show color settings\nUse /theme to change the UI theme."
        )),
    }
}

fn cmd_powerup(args: &str) -> CommandAction {
    let lessons: &[(&str, &str, &str)] = &[
        (
            "lesson 1 — navigating the TUI",
            "Basic navigation",
            "Welcome to rustyclaw! Here are the essentials:\n\
             \n\
             **Sending messages**\n\
             - Type your message and press Enter\n\
             - Shift+Enter inserts a newline (multi-line input)\n\
             - Escape cancels the current request mid-stream\n\
             \n\
             **Scrolling**\n\
             - Page Up / Page Down scrolls the chat\n\
             - End (or any new message) snaps back to the bottom\n\
             \n\
             **History**\n\
             - Up/Down arrow recalls previous inputs\n\
             - Tab auto-completes the input from history\n\
             \n\
             **Vim mode**\n\
             - /vim toggles vim keybindings (normal/insert mode)\n\
             \n\
             Type /powerup 2 for the next lesson.",
        ),
        (
            "lesson 2 — slash commands",
            "Slash commands",
            "Slash commands control rustyclaw's behaviour:\n\
             \n\
             - /help         — full command list\n\
             - /model        — switch AI model\n\
             - /cost         — token usage + cache breakdown\n\
             - /context      — how full the context window is\n\
             - /compact       — summarise + compress history\n\
             - /clear        — start a fresh conversation\n\
             - /session      — list/resume previous sessions\n\
             - /doctor       — system health check\n\
             - /voice        — toggle voice input (whisper)\n\
             - /plan         — read-only mode (no destructive tools)\n\
             - /effort       — set thinking depth (low/medium/high/max)\n\
             \n\
             Type /powerup 3 for the next lesson.",
        ),
        (
            "lesson 3 — skills",
            "Skills (prompt templates)",
            "Skills are reusable prompt templates stored in ~/.claude/skills/\n\
             \n\
             **Built-in skills**\n\
             - /commit   — write a conventional git commit\n\
             - /review   — code review\n\
             - /fix      — diagnose and fix a bug\n\
             - /explain  — explain a piece of code\n\
             - /test     — write tests\n\
             \n\
             **Create your own**\n\
             Make a .md file in ~/.claude/skills/:\n\
             ```\n\
             # My Skill\n\
             What it does\n\
             ---\n\
             Prompt template. Use {{ARGS}} for user arguments.\n\
             ```\n\
             Then invoke it with /my-skill some arguments\n\
             \n\
             Type /powerup 4 for the next lesson.",
        ),
        (
            "lesson 4 — context & sessions",
            "Context management",
            "Every conversation has a context window (~200K tokens):\n\
             \n\
             - /context     — visual usage bar\n\
             - /compact      — when near-full, summarises and resets\n\
             - autoCompact  — set in settings.json to run automatically\n\
             \n\
             **Sessions are saved automatically**\n\
             ~/.claude/sessions/<uuid>.jsonl\n\
             \n\
             - /session list     — see all saved sessions\n\
             - /resume <id>      — continue an old session\n\
             - rustyclaw -r      — resume most recent on launch\n\
             - /export           — save as markdown\n\
             \n\
             Type /powerup 5 for the next lesson.",
        ),
        (
            "lesson 5 — MCP servers",
            "MCP (Model Context Protocol) servers",
            "MCP servers extend rustyclaw with additional tools:\n\
             \n\
             **Configure in ~/.claude/settings.json:**\n\
             ```json\n\
             {\n\
               \"mcpServers\": {\n\
                 \"github\": {\n\
                   \"command\": \"npx\",\n\
                   \"args\": [\"-y\", \"@modelcontextprotocol/server-github\"],\n\
                   \"env\": {\"GITHUB_TOKEN\": \"ghp_...\"}\n\
                 }\n\
               }\n\
             }\n\
             ```\n\
             \n\
             - /mcp         — list connected servers and tools\n\
             - rustyclaw mcp list   — from the terminal\n\
             \n\
             Type /powerup 6 for the next lesson.",
        ),
        (
            "lesson 6 — voice & TTS",
            "Voice input & text-to-speech",
            "Voice features (requires XTTS v2 + whisper/openai):\n\
             \n\
             **Voice input** — transcribes your speech to text\n\
             - /voice on    — enable (requires a mic + whisper)\n\
             - Hold Ctrl+Space to record while voice is on\n\
             \n\
             **Text-to-speech** — speaks replies in a custom voice (record anyone)\n\
             - /voice tts on   — enable TTS (requires XTTS v2)\n\
             - /voice clone    — record any voice as the TTS speaker\n\
             - /doctor         — check if XTTS v2 is configured\n\
             \n\
             Install XTTS v2:\n\
               uv tool install TTS --python 3.11 \\\n\
                 --with 'transformers<4.46' --with 'torch<2.6' --with 'torchaudio<2.6'\n\
             \n\
             You have completed the rustyclaw power-up course!\n\
             Type /help any time for the full command reference.",
        ),
    ];

    let num: usize = args.trim().parse().unwrap_or(1);
    let idx = (num.saturating_sub(1)).min(lessons.len() - 1);
    let (_, title, content) = lessons[idx];

    let header = format!(
        "**rustyclaw /powerup — {} of {}** — {}\n\n",
        idx + 1,
        lessons.len(),
        title
    );

    CommandAction::Message(format!("{header}{content}"))
}
