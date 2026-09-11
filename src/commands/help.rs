//! `/` command handlers — split out of `commands/mod.rs` mechanically.

use super::*;

pub(super) fn cmd_banner(args: &str) -> CommandAction {
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

pub(super) fn cmd_version() -> CommandAction {
    CommandAction::Message(format!("RustyClaw v{}", env!("CARGO_PKG_VERSION")))
}

pub(super) fn cmd_keybindings() -> CommandAction {
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
            (
                "/effort",
                "set effort (low, medium, high, max, off) — output_config.effort on Claude 4.6+/5",
            ),
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
            (
                "/trust",
                "Trust this project: honour its settings hooks, apiKeyHelper and MCP servers",
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
            (
                "/browse <goal>",
                "run autonomous browser agent towards a goal",
            ),
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

pub(super) fn cmd_help(args: &str) -> CommandAction {
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

pub(super) fn cmd_output_style(args: &str, ctx: &CommandContext) -> CommandAction {
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

pub(super) fn cmd_theme(args: &str, ctx: &CommandContext) -> CommandAction {
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

pub(super) fn cmd_statusline(args: &str) -> CommandAction {
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

pub(super) fn cmd_feedback() -> CommandAction {
    CommandAction::OpenBrowser("https://github.com/ForkedInTime/RustyClaw/issues".into())
}

// ── Terminal Setup ─────────────────────────────────────────────────────────────

pub(super) fn cmd_terminal_setup() -> CommandAction {
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

pub(super) fn cmd_notifications(args: &str, ctx: &CommandContext) -> CommandAction {
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

pub(super) fn cmd_release_notes(_args: &str) -> CommandAction {
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

pub(super) fn cmd_color(args: &str) -> CommandAction {
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
