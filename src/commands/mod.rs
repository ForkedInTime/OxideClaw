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

// ── Handler modules (mechanical split, no behaviour change) ──────────────────
mod agents;
mod catalogue;
mod git;
mod help;
mod mcp;
mod plugins;
mod session;
mod settings;
mod status;
use agents::*;
pub use catalogue::*;
use git::*;
pub use help::*;
use mcp::*;
use plugins::*;
pub use session::*;
use settings::*;
use status::*;

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
    "trust",
    "spawn",
    "lint",
    "checkpoint",
    "autonomy",
    "reload",
    "reload-settings",
    "sessions",
];

// ── Model catalogue ───────────────────────────────────────────────────────────

/// What the run-loop should do after a slash command is dispatched.
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
    /// `/rewind [N]`: `None` opens a picker of turns to go back to;
    /// `Some(n)` is `/undo n`.
    Rewind(Option<usize>),
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
    /// Set the effort level sent as `output_config.effort` ("low" | "medium" | "high" | "xhigh" | "max"); `None` clears it
    SetEffort(Option<String>),
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
    /// Execute a plugin slash command (`<plugin>:<command> [args]`)
    PluginCommand {
        plugin: String,
        command: String,
        args: String,
    },
    /// Show interactive model picker (async — needs Ollama query)
    ListModels,
    /// Show interactive voice model picker
    ListVoiceModels,
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
    /// `/router on|off` — sets, never flips (`/router on` twice stays on).
    RouterSet(bool),
    /// Show router config and status
    RouterStatus,
    /// Set a specific router tier model
    RouterSetTier { tier: String, model: String },
    /// Create a git checkpoint commit (auto-stash of working changes)
    GitCheckpoint(Option<String>),
    /// Switch the session's autonomy mode (full-auto only with its sandbox).
    SetAutonomy(crate::permissions::Autonomy),
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
    /// `/undo [N]` — take the last N turns (default 1) off the conversation
    /// and put their files back as they were before them.
    Undo(usize),
    /// `/redo [N]` — put back the last N turns /undo took off (default 1),
    /// conversation and files.
    Redo(usize),
    /// `/autocommit [status]` — print auto-commit state to the chat. v1 only supports `status`.
    AutoCommitStatus,
    /// `/trust` — add the current project to the global `trustedProjects`
    /// list so its `.claude/settings.json` hooks, `apiKeyHelper` and MCP
    /// servers are honoured. `/trust status` reports without changing
    /// anything; `/trust revoke` removes the project from the list.
    TrustProject { mode: TrustMode },
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
    /// Context window auto-compaction measures against (see
    /// `compact::compaction_window`).
    pub context_window: u64,
    pub tokens_out: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    /// Session cost from the per-call cost tracker (the same numbers
    /// `/budget` and the status bar use).
    pub cost_summary: String,
    /// Whether the cost tracker holds any API call this session. Survives
    /// /clear and aborted turns, unlike the last-turn token fields.
    pub cost_recorded: bool,
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
            CommandAction::Watch(if a.is_empty() {
                None
            } else {
                Some(a.to_string())
            })
        }
        "diff" => {
            let a = args.trim();
            CommandAction::ShowDiff(if a.is_empty() {
                None
            } else {
                Some(a.to_string())
            })
        }
        "permissions" => cmd_permissions(ctx),
        "skills" => cmd_skills(ctx),
        "review" => cmd_review(args),
        "tasks" => cmd_tasks(ctx),
        "copy" => cmd_copy(ctx),
        "rewind" => CommandAction::Rewind(args.trim().parse::<usize>().ok().filter(|n| *n > 0)),
        "undo" => cmd_undo(args),
        "redo" => cmd_redo(args),
        "autocommit" => cmd_autocommit(args),
        "trust" => cmd_trust(args),
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
        "autonomy" => cmd_autonomy(args, ctx.config.autonomy),
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
        "session" => cmd_session(args),
        "sessions" => cmd_session(args),
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
            "Auth is env-driven: set ANTHROPIC_API_KEY for Claude, or set the matching key \
             (GROQ_API_KEY, OPENROUTER_API_KEY, …) and switch with /model <provider>:<name>. \
             No login state to manage."
                .into(),
        ),
        "theme" => cmd_theme(args, ctx),
        "fast" => CommandAction::Message(
            "Streaming is always on. Use /model haiku for the lowest-latency tier, or /router \
             to auto-route easy turns to a cheap model."
                .into(),
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
                CommandAction::Message(
                    "Usage: /forget <key | words every memory to remove contains>".into(),
                )
            } else {
                CommandAction::MemoryForget(args.to_string())
            }
        }

        // ── New commands (gap fill) ───────────────────────────────────────
        "commit" => cmd_commit(args, ctx),
        "commit-push-pr" => cmd_commit_push_pr(args, ctx),
        "effort" => cmd_effort(args),
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
                    args: args.to_string(),
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
/// Remaining tokens after flag removal form the `goal` string. A missing goal
/// or a bad `--max-steps` value yields a usage message instead of a run.
pub fn parse_browse_command(input: &str) -> CommandAction {
    use crate::browser::browse_loop::BrowsePolicy;
    const USAGE: &str = "Usage: /browse [--yolo|--ask] [--max-steps N] <goal>";
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
            "--max-steps" => {
                // Zero would mean no cap at all, so it is as bad as a typo;
                // silently dropping either would start an uncapped run.
                match tokens.get(i + 1).and_then(|v| v.parse::<u32>().ok()) {
                    Some(n) if n > 0 => max_steps = Some(n),
                    _ => return CommandAction::Message(USAGE.into()),
                }
                tokens.drain(i..=i + 1);
            }
            _ => {
                i += 1;
            }
        }
    }
    let goal = tokens.join(" ").trim().to_string();
    // The run loop has no goal guard: an empty goal still starts the agent.
    if goal.is_empty() {
        return CommandAction::Message(USAGE.into());
    }
    CommandAction::Browse {
        goal,
        policy,
        max_steps,
    }
}

// ── Individual commands ───────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustMode {
    Grant,
    Status,
    Revoke,
}

/// Only a bare `/trust` grants trust. Granting lets the project's settings
/// run code, so a typo'd status check (`/trust Status`, `/trust check`) must
/// not do it.
fn cmd_trust(args: &str) -> CommandAction {
    let mode = match args.trim().to_ascii_lowercase().as_str() {
        "" => TrustMode::Grant,
        "status" => TrustMode::Status,
        "revoke" | "untrust" | "off" => TrustMode::Revoke,
        _ => return CommandAction::Message("Usage: /trust [status|revoke]".into()),
    };
    CommandAction::TrustProject { mode }
}

#[cfg(test)]
mod completion_list_tests {
    use super::{HELP_CATEGORIES, SLASH_COMMANDS};

    /// Tab completion only offers SLASH_COMMANDS, so a command advertised in
    /// /help but absent here (e.g. /spawn, /reload) never completed.
    #[test]
    fn every_help_command_tab_completes() {
        for (cmd, _) in HELP_CATEGORIES.iter().flat_map(|(_, _, cmds)| cmds.iter()) {
            let name = cmd
                .split_whitespace()
                .next()
                .unwrap()
                .trim_start_matches('/');
            assert!(
                SLASH_COMMANDS.contains(&name),
                "/{name} is in /help but not tab-completable"
            );
        }
    }
}

#[cfg(test)]
mod removed_command_tests {
    use super::*;

    /// `/statusline` had an agent write a `statusLine` setting that OxideClaw
    /// never reads, so it is gone: not listed, not completed, not dispatched.
    #[test]
    fn statusline_is_an_unknown_command() {
        let listed = HELP_CATEGORIES
            .iter()
            .flat_map(|(_, _, cmds)| cmds.iter())
            .any(|(cmd, _)| cmd.starts_with("/statusline"));
        assert!(!listed, "/statusline is still in /help");
        assert!(!SLASH_COMMANDS.contains(&"statusline"));

        let config = Config::default();
        let skills = HashMap::new();
        let todo = TodoState::default();
        let ctx = CommandContext {
            config: &config,
            tokens_in: 0,
            context_window: 0,
            tokens_out: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            cost_summary: String::new(),
            cost_recorded: false,
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
        for input in ["/statusline", "/statusline show my git branch"] {
            assert!(
                matches!(dispatch(input, &ctx), CommandAction::Unknown(ref n) if n == "statusline"),
                "{input} must fall through to the unknown-command message"
            );
        }
    }
}

#[cfg(test)]
mod trust_command_tests {
    use super::{CommandAction, TrustMode, cmd_trust};

    #[test]
    fn only_a_bare_trust_grants() {
        let mode = |a: &str| match cmd_trust(a) {
            CommandAction::TrustProject { mode } => Some(mode),
            CommandAction::Message(m) => {
                assert!(m.contains("Usage"), "{m}");
                None
            }
            _ => panic!("{a:?}: unexpected action"),
        };
        assert_eq!(mode(""), Some(TrustMode::Grant));
        assert_eq!(mode("  "), Some(TrustMode::Grant));
        assert_eq!(mode("status"), Some(TrustMode::Status));
        assert_eq!(mode("Status"), Some(TrustMode::Status));
        assert_eq!(mode("revoke"), Some(TrustMode::Revoke));
        assert_eq!(mode("untrust"), Some(TrustMode::Revoke));
        assert_eq!(mode("OFF"), Some(TrustMode::Revoke));
        for typo in ["check", "?", "help", "--status", "stauts"] {
            assert_eq!(mode(typo), None, "{typo:?} must not grant trust");
        }
    }
}

#[cfg(test)]
mod model_catalogue_tests {
    use super::{CommandAction, KNOWN_MODELS, cmd_effort, resolve_model_alias};

    /// `/effort high` must set the API effort level, not inject a prompt —
    /// on Claude 5 the model ignores prose about effort but honours the
    /// `output_config.effort` parameter.
    #[test]
    fn effort_command_sets_the_api_level() {
        for (arg, want) in [
            ("low", "low"),
            ("1", "low"),
            ("medium", "medium"),
            ("2", "medium"),
            ("", "medium"),
            ("high", "high"),
            ("3", "high"),
            ("xhigh", "xhigh"),
            ("X-High", "xhigh"),
            ("extra-high", "xhigh"),
            ("MAX", "max"),
            ("4", "max"),
        ] {
            match cmd_effort(arg) {
                CommandAction::SetEffort(Some(level)) => assert_eq!(level, want, "arg {arg:?}"),
                _ => panic!("arg {arg:?}: expected SetEffort(Some({want}))"),
            }
        }
    }

    #[test]
    fn effort_off_clears_the_level() {
        for arg in ["off", "default", "none", "clear"] {
            assert!(
                matches!(cmd_effort(arg), CommandAction::SetEffort(None)),
                "{arg}"
            );
        }
    }

    #[test]
    fn effort_rejects_unknown_levels_with_usage() {
        match cmd_effort("ultra") {
            CommandAction::Message(m) => assert!(m.contains("Usage") && m.contains("max"), "{m}"),
            _ => panic!("expected usage message"),
        }
    }

    /// Bare family names mean the current generation (Claude 5 shipped
    /// 2026); the picker and aliases still pointed at 4.6 and a dated Haiku id.
    #[test]
    fn aliases_resolve_to_the_current_generation() {
        assert_eq!(resolve_model_alias("opus"), "claude-opus-5");
        assert_eq!(resolve_model_alias("sonnet"), "claude-sonnet-5");
        assert_eq!(resolve_model_alias("haiku"), "claude-haiku-4-5");
        assert_eq!(resolve_model_alias("fable"), "claude-fable-5-1");
        assert_eq!(resolve_model_alias("opus-4-6"), "claude-opus-4-6");
        assert_eq!(resolve_model_alias("sonnet-4-6"), "claude-sonnet-4-6");
        assert_eq!(resolve_model_alias("ollama:llama3"), "ollama:llama3");
    }

    /// Claude Code's own model spellings (its settings.json is migrated on
    /// first run) were sent to the API verbatim and rejected.
    #[test]
    fn claude_code_model_spellings_resolve() {
        assert_eq!(resolve_model_alias("opusplan"), "claude-opus-5");
        assert_eq!(resolve_model_alias("sonnet[1m]"), "claude-sonnet-5");
        assert_eq!(resolve_model_alias("OPUS[1M]"), "claude-opus-5");
        assert_eq!(
            resolve_model_alias("claude-sonnet-4-5-20250929[1m]"),
            "claude-sonnet-4-5-20250929"
        );
        assert_eq!(resolve_model_alias("default"), crate::api::default_model());
        // Another provider's name is its own business.
        assert_eq!(resolve_model_alias("ollama:x[1m]"), "ollama:x[1m]");
        assert_eq!(super::settings_model(" default "), None);
        assert_eq!(super::settings_model(""), None);
        assert_eq!(
            super::settings_model("opusplan").as_deref(),
            Some("claude-opus-5")
        );
    }

    #[test]
    fn the_picker_lists_current_models_without_date_suffixes() {
        let ids: Vec<&str> = KNOWN_MODELS.iter().map(|(id, _)| *id).collect();
        for want in [
            "claude-fable-5-1",
            "claude-opus-5",
            "claude-sonnet-5",
            "claude-haiku-4-5",
        ] {
            assert!(ids.contains(&want), "picker is missing {want}");
        }
        for id in &ids {
            assert!(
                !id.ends_with(|c: char| c.is_ascii_digit()) || !id.contains("-2025"),
                "{id} carries a date suffix; model ids are complete without one"
            );
        }
        assert_eq!(super::super::api::default_model(), "claude-sonnet-5");
    }

    /// `/model` rejected ids the rest of the app prices and supports, and
    /// the documented opus-4-5 / sonnet-4-5 shorthands always failed:
    /// cmd_model accepts a Claude id only if its alias target is listed.
    #[test]
    fn model_accepts_every_supported_claude_id_and_shorthand() {
        let ids: Vec<&str> = KNOWN_MODELS.iter().map(|(id, _)| *id).collect();
        for m in [
            "claude-opus-5-5",
            "claude-sonnet-5-5",
            "claude-opus-4-8",
            "claude-opus-4-7",
            "opus",
            "sonnet",
            "haiku",
            "fable",
            "opus-4-6",
            "sonnet4.6",
            "opus-4-5",
            "sonnet4.5",
        ] {
            let id = resolve_model_alias(m);
            assert!(ids.contains(&id.as_str()), "/model {m} ({id}) is rejected");
        }
    }
}
