/// oxideclaw — Rust-native AI coding CLI
/// Entry point
mod acp;
mod api;
mod auth;
mod autofix;
mod browser;
mod claude_import;
mod commands;
mod compact;
mod config;
mod cost;
mod deeplink;
mod distro;
mod hooks;
mod mcp;
mod memory;
mod net_policy;
mod permissions;
mod query_engine;
mod rag;
mod router;
mod sandbox;
mod sdk;
mod session;
mod settings;
mod skills;
mod spawn;
mod tools;
mod tui;
mod update_check;
mod voice;
mod watch;

use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};
use config::Config;
use mcp::scope::Scope as McpScope;
use query_engine::QueryEngine;

#[allow(unused_imports)]
use colored::*;

/// SyntheticOutputTool — injected when --json-schema is provided.
/// Claude must call this tool with the structured output; we extract it.
struct SyntheticOutputTool {
    schema: serde_json::Value,
}

#[async_trait::async_trait]
impl tools::Tool for SyntheticOutputTool {
    fn name(&self) -> &str {
        "result"
    }
    fn description(&self) -> &str {
        "Use this tool to provide the final structured output that matches the required JSON schema. \
        You MUST call this tool to return your result."
    }
    fn input_schema(&self) -> serde_json::Value {
        self.schema.clone()
    }
    async fn execute(
        &self,
        input: serde_json::Value,
        _ctx: &tools::ToolContext,
    ) -> anyhow::Result<tools::ToolOutput> {
        // Emit the structured result as JSON
        println!("{}", serde_json::to_string(&input).unwrap_or_default());
        Ok(tools::ToolOutput::success(
            serde_json::to_string(&input).unwrap_or_default(),
        ))
    }
}

const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Clone, ValueEnum)]
enum OutputFormat {
    Text,
    Json,
    StreamJson,
}

#[derive(Debug, Clone, ValueEnum)]
enum PermissionMode {
    Default,
    Auto,
    Bypass,
}

#[derive(Debug, Clone, ValueEnum)]
enum ThinkingMode {
    Enabled,
    Disabled,
    Auto,
}

#[derive(Parser)]
#[command(
    name = "oxideclaw",
    version = VERSION,
    about = "OxideClaw — Rust-native AI coding CLI",
    long_about = None,
)]
struct Cli {
    /// Print response and exit (non-interactive / pipe mode)
    #[arg(short = 'p', long)]
    print: bool,

    /// Run as headless SDK server (NDJSON stdio, long-running)
    #[arg(long)]
    headless: bool,

    /// Enable verbose/debug output
    #[arg(long)]
    verbose: bool,

    /// Bypass all permission checks (sandboxes only)
    #[arg(long)]
    dangerously_skip_permissions: bool,

    /// Model to use (default: claude-sonnet-5)
    #[arg(long)]
    model: Option<String>,

    /// Resume the most recent session
    #[arg(short = 'r', long)]
    resume: bool,

    /// Continue the most recent session (alias for --resume)
    #[arg(short = 'c', long = "continue")]
    continue_session: bool,

    /// Resume a specific session by ID, unique ID prefix, or name
    #[arg(long)]
    session: Option<String>,

    /// Session display name
    #[arg(short = 'n', long)]
    name: Option<String>,

    /// Output format for --print mode: text (default), json, stream-json
    #[arg(long, value_enum, default_value = "text")]
    output_format: OutputFormat,

    /// Max agentic turns before stopping (0 = the default cap of 50)
    #[arg(long, default_value = "0")]
    max_turns: u32,

    /// Tools to allow, separated by commas or spaces, as separate arguments
    /// or by repeating the flag. A bare name (Read) restricts the tool list
    /// to the names given; a rule (Bash(git status:*)) runs matching calls
    /// without a prompt and keeps its tool available. Every argument up to
    /// the next flag is read as a tool: put the prompt first, end the list
    /// with `--`, or write --allowed-tools=<list>.
    // No value_delimiter: clap would split `Bash(npm run a,b)` inside the
    // parentheses. permissions::parse_tool_flag splits outside them.
    // Variadic like Claude Code's `--allowedTools A B`: with one value per
    // flag, `B` went into the prompt and the restriction silently shrank.
    #[arg(long, num_args = 1..)]
    allowed_tools: Vec<String>,

    /// Tools to block, separated by commas or spaces, as separate arguments
    /// or by repeating the flag. A bare name (Bash) removes the tool; a rule
    /// (Bash(git push:*)) refuses matching calls and keeps the tool for the
    /// rest. Every argument up to the next flag is read as a tool: put the
    /// prompt first, end the list with `--`, or write
    /// --disallowed-tools=<list>.
    #[arg(long, num_args = 1..)]
    disallowed_tools: Vec<String>,

    /// System prompt override (replaces built-in system prompt)
    #[arg(long)]
    system_prompt: Option<String>,

    /// Append text to the system prompt
    #[arg(long)]
    append_system_prompt: Option<String>,

    /// Load additional MCP server configs from a JSON file path or JSON string:
    /// {"name":{"command":"...","args":[...]},...} or {"mcpServers":{...}}.
    /// Repeat the flag to load several.
    // No value_delimiter: splitting on spaces cut any JSON with a space in
    // it into fragments that were then dropped without a word.
    #[arg(long)]
    mcp_config: Vec<String>,

    /// Permission mode: default or bypass (auto is accepted and runs as default)
    #[arg(long, value_enum)]
    permission_mode: Option<PermissionMode>,

    /// Extended thinking mode: enabled, disabled, auto
    #[arg(long, value_enum)]
    thinking: Option<ThinkingMode>,

    /// Max thinking tokens (overrides settings.json thinkingBudgetTokens)
    #[arg(long)]
    max_thinking_tokens: Option<u32>,

    // Accepted but not implemented; see `ignored_flag_warnings`.
    #[arg(long, value_delimiter = ',', hide = true)]
    add_dir: Vec<String>,

    /// Effort level: low, medium, high, xhigh, max
    #[arg(long)]
    effort: Option<String>,

    /// Beta headers to include in API requests (API key users only)
    #[arg(long, num_args = 1..)]
    betas: Vec<String>,

    /// Disable session persistence — sessions will not be saved to disk
    #[arg(long)]
    no_session_persistence: bool,

    /// Use a specific session ID for the conversation (must be a valid UUID):
    /// resumes it if it exists, otherwise starts a new session with that ID
    #[arg(long)]
    session_id: Option<String>,

    /// Only use MCP servers from --mcp-config, ignoring settings.json mcpServers
    #[arg(long)]
    strict_mcp_config: bool,

    /// Include partial message chunks as they arrive (only with --output-format=stream-json)
    #[arg(long)]
    include_partial_messages: bool,

    /// Include hook lifecycle events in the output stream (only with --output-format=stream-json)
    #[arg(long)]
    include_hook_events: bool,

    /// Minimal mode: skip hooks, CLAUDE.md/AGENTS.md/GEMINI.md discovery, and LSP
    #[arg(long)]
    bare: bool,

    /// Load additional settings from a JSON file path or JSON string
    #[arg(long)]
    settings: Option<String>,

    /// Input format for --print mode: text (default) or stream-json
    #[arg(long)]
    input_format: Option<String>,

    /// JSON schema for structured output (adds result tool with this schema)
    #[arg(long)]
    json_schema: Option<String>,

    /// Re-emit user messages on stdout in stream-json mode
    #[arg(long)]
    replay_user_messages: bool,

    /// When resuming, assign a new session UUID instead of reusing the original
    #[arg(long)]
    fork_session: bool,

    /// Read system prompt from a file (overrides --system-prompt)
    #[arg(long)]
    system_prompt_file: Option<String>,

    /// Read text from file and append to the system prompt
    #[arg(long)]
    append_system_prompt_file: Option<String>,

    // Accepted but not implemented; see `ignored_flag_warnings`.
    #[arg(long, hide = true)]
    allow_dangerously_skip_permissions: bool,

    /// Maximum USD to spend on API calls (--print mode only)
    #[arg(long)]
    max_budget_usd: Option<f64>,

    /// Fallback model to use when primary model is overloaded (HTTP 529;
    /// --print mode only)
    #[arg(long)]
    fallback_model: Option<String>,

    // Accepted for Claude Code command-line compatibility but not
    // implemented; `ignored_flag_warnings` says so instead of silently running
    // in the main tree. `oxideclaw spawn` is the worktree path.
    #[arg(long, hide = true)]
    worktree: Option<Option<String>>,

    #[arg(long, hide = true)]
    tmux: bool,

    /// Handle a deep link URI (called by the OS when a registered URL scheme is activated)
    #[arg(long, value_name = "URI")]
    handle_uri: Option<String>,

    /// Register the deep link protocol handler (creates .desktop file, runs xdg-mime)
    #[arg(long)]
    register_protocol: bool,

    // Accepted but not implemented; see `ignored_flag_warnings`.
    #[arg(long, hide = true)]
    agents: Option<String>,

    /// Disable all slash commands
    #[arg(long)]
    disable_slash_commands: bool,

    // Accepted but not implemented; see `ignored_flag_warnings`.
    #[arg(long, value_delimiter = ',', hide = true)]
    setting_sources: Vec<String>,

    /// Tools to make available: "" = none, "default" = all, or specific names
    #[arg(long, value_delimiter = ',')]
    tools: Vec<String>,

    /// Prompt to send: with --print, answer it and exit; without, open the
    /// TUI and send it as the first message. Flags may come before or after
    /// it; quote the prompt or put it after `--` if it contains words that
    /// start with `-`.
    // Not trailing_var_arg: that swallowed every flag after the first prompt
    // word (`-p "fix" --disallowed-tools Bash`) into the prompt text and
    // silently dropped the restriction.
    prompt: Vec<String>,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Run as an Agent Client Protocol agent over stdio (Zed, JetBrains, any ACP client)
    Acp,
    /// Show version information
    Version,
    /// Manage MCP servers
    Mcp {
        #[command(subcommand)]
        subcommand: Option<McpSubcommand>,
    },
    /// Generate shell completions
    Completions {
        /// Shell to generate completions for (bash, zsh, fish, elvish, powershell)
        shell: clap_complete::Shell,
    },
    /// Check installation health
    Doctor,
    /// Manage OxideClaw's configuration
    Config {
        #[command(subcommand)]
        subcommand: ConfigSubcommand,
    },
    /// Self-update to the latest release from GitHub
    Update,
    /// Run the autonomous browser agent
    Browse {
        /// Goal for the browser agent
        goal: Vec<String>,
        /// Skip all approval prompts (warns once, on first use)
        #[arg(long)]
        yolo: bool,
        /// Prompt for approval on every destructive action
        #[arg(long)]
        ask: bool,
        /// Maximum number of steps (default: 50)
        #[arg(long, default_value = "50", value_parser = clap::value_parser!(u32).range(1..))]
        max_steps: u32,
    },
}

#[derive(Subcommand)]
enum ConfigSubcommand {
    /// Copy hooks, permission rules, apiKeyHelper or MCP servers from Claude
    /// Code's ~/.claude/settings.json into OxideClaw's settings. With no
    /// options, lists what is there and changes nothing. ~/.claude is only read.
    ImportClaude {
        /// Import hooks (Claude Code's format is converted)
        #[arg(long)]
        hooks: bool,
        /// Import permissions.allow and permissions.deny rules
        #[arg(long)]
        permissions: bool,
        /// Import apiKeyHelper (kept as is when OxideClaw already has one)
        #[arg(long)]
        api_key_helper: bool,
        /// Import MCP servers OxideClaw does not have yet
        #[arg(long)]
        mcp: bool,
    },
}

#[derive(Subcommand)]
enum McpSubcommand {
    /// List configured MCP servers and the scope each comes from
    List,
    /// Add an MCP server (stdio or HTTP)
    Add {
        /// Server name
        name: String,
        /// Command to run (stdio) or server URL (http)
        command: String,
        /// Arguments for the command (stdio only)
        args: Vec<String>,
        /// Where to keep it: local (default; private to you, this project
        /// only), project (.mcp.json: shared, usually committed, starts only
        /// after /trust), or user (all your projects)
        #[arg(short = 's', long, default_value = "local", value_parser = parse_mcp_scope)]
        scope: McpScope,
        /// Transport type: stdio (default) or http
        #[arg(short = 't', long, default_value = "stdio")]
        transport: String,
        /// Environment variables (KEY=VALUE, stdio only). The project scope
        /// refuses literal values (use KEY='${KEY}') unless --force
        #[arg(short = 'e', long)]
        env: Vec<String>,
        /// With --scope project, write literal env values anyway
        #[arg(long)]
        force: bool,
    },
    /// Add an MCP server from a JSON string
    AddJson {
        /// Server name
        name: String,
        /// JSON configuration string
        json: String,
        /// Where to keep it: local (default; private to you, this project
        /// only), project (.mcp.json: shared, usually committed, starts only
        /// after /trust), or user (all your projects)
        #[arg(short = 's', long, default_value = "local", value_parser = parse_mcp_scope)]
        scope: McpScope,
        /// With --scope project, write literal env values or headers anyway
        #[arg(long)]
        force: bool,
    },
    /// Import MCP servers from Claude Desktop configuration
    AddFromClaudeDesktop {
        /// Where to keep them: local (default; private to you, this project
        /// only), project (.mcp.json: shared, usually committed, starts only
        /// after /trust), or user (all your projects)
        #[arg(short = 's', long, default_value = "local", value_parser = parse_mcp_scope)]
        scope: McpScope,
        /// With --scope project, write literal env values or headers anyway
        #[arg(long)]
        force: bool,
    },
    /// Remove an MCP server
    Remove {
        /// Server name to remove
        name: String,
        /// Scope to remove it from (local, project or user); may be omitted
        /// when only one scope defines the name
        #[arg(short = 's', long, value_parser = parse_mcp_scope)]
        scope: Option<McpScope>,
    },
    /// Show details about an MCP server, including its scope
    Get {
        /// Server name
        name: String,
    },
    /// Stop this project's .mcp.json and project-settings MCP servers from
    /// starting by revoking its trust (same as /trust revoke)
    ResetProjectChoices,
}

fn parse_mcp_scope(s: &str) -> std::result::Result<McpScope, String> {
    McpScope::parse(s).map_err(|e| e.to_string())
}

/// Safe allowlist of env vars that oxideclaw may load from .env files.
///
/// Project `.env` files are **untrusted data** — a malicious repo could ship a
/// `.env` that sets `PATH`, `LD_PRELOAD`, or `OXIDECLAW_*_COMMAND` to pivot
/// code execution the moment the user opens the folder. We therefore load only
/// a narrow allowlist of our own API-key and model vars, and specifically NEVER
/// load anything that could:
///   - Bypass permission prompts (`CLAUDE_DANGEROUSLY_SKIP_PERMISSIONS`)
///   - Redirect config / settings / hook resolution (`OXIDECLAW_CONFIG_DIR`,
///     `CLAUDE_CONFIG_DIR`, `XDG_CONFIG_HOME`, `HOME`)
///   - Alter any process-spawn path (`PATH`, `LD_PRELOAD`, `LD_LIBRARY_PATH`,
///     `DYLD_*`, `OXIDECLAW_*_COMMAND`, sandbox binaries, voice binaries,
///     MCP server argv)
///
/// If a user legitimately needs one of the blocked vars set, they can export
/// it in their shell — project `.env` is not the right place.
const SAFE_ENV_KEYS: &[&str] = &[
    // Anthropic credentials. The whole documented resolution chain must be
    // settable from .env, not just the API key — otherwise a project that
    // authenticates with an OAuth token silently falls back to whatever key
    // happens to be in the ambient environment.
    //
    // ANTHROPIC_BASE_URL is deliberately NOT here: it redirects every API call,
    // so a hostile .env could point credentials at an attacker-controlled host.
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_PROFILE",
    "OXIDECLAW_API_KEY_FILE_DESCRIPTOR",
    "RUSTYCLAW_API_KEY_FILE_DESCRIPTOR", // pre-rename name, still honoured
    "ANTHROPIC_MODEL",
    // Verbose logging toggle — no exec side-effects
    "OXIDECLAW_VERBOSE",
    "RUSTYCLAW_VERBOSE",
    // Ollama host: whoever runs it is the model, so it sees every prompt and
    // drives tool calls. A project .env may set it (and ANTHROPIC_MODEL) only
    // once the folder is trusted; see PROJECT_UNTRUSTED_ENV_KEYS.
    "OLLAMA_HOST",
    // OpenAI-compat provider keys
    "OPENAI_API_KEY",
    "GROQ_API_KEY",
    "DEEPSEEK_API_KEY",
    "GEMINI_API_KEY",
    "GOOGLE_API_KEY",
    "MISTRAL_API_KEY",
    "OPENROUTER_API_KEY",
    "TOGETHER_API_KEY",
    "XAI_API_KEY",
    "VENICE_API_KEY",
];

/// Env vars that MUST NEVER be loaded from `.env` files because doing so
/// would allow a malicious repo to bypass security controls or redirect
/// process execution. This is a belt-and-braces check on top of the
/// allowlist in [`SAFE_ENV_KEYS`].
///
/// Kept as a separate constant so the intent (and the threat model) stay
/// explicit in code reviews.
#[cfg(test)]
const FORBIDDEN_ENV_KEYS: &[&str] = &[
    "PATH",
    "LD_PRELOAD",
    "LD_LIBRARY_PATH",
    "DYLD_LIBRARY_PATH",
    "DYLD_INSERT_LIBRARIES",
    "HOME",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_CACHE_HOME",
    "CLAUDE_CONFIG_DIR",
    "OXIDECLAW_CONFIG_DIR",
    "RUSTYCLAW_CONFIG_DIR",
    "CLAUDE_DANGEROUSLY_SKIP_PERMISSIONS",
    "OXIDECLAW_SANDBOX_COMMAND",
    "OXIDECLAW_VOICE_COMMAND",
    "RUSTYCLAW_SANDBOX_COMMAND",
    "RUSTYCLAW_VOICE_COMMAND",
    "GEMINI_CLI_IDE_SERVER_STDIO_COMMAND",
];

/// Keys an untrusted project's `.env` may not set. Each picks where prompts
/// (system prompt, CLAUDE.md, file contents read by tools) are sent, or the
/// account that receives them: a cloned repo could otherwise point
/// `OLLAMA_HOST` at its own server, pick an `ANTHROPIC_MODEL` on a provider
/// it controls, or supply its own API key or token (the project `.env` loads
/// first and wins over the user's own) and read every turn in that account's
/// logs. This mirrors the `ollamaHost` drop for untrusted project
/// `settings.json`; `/trust` lifts both.
const PROJECT_UNTRUSTED_ENV_KEYS: &[&str] = &[
    "OLLAMA_HOST",
    "ANTHROPIC_MODEL",
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_PROFILE",
    "OXIDECLAW_API_KEY_FILE_DESCRIPTOR",
    "RUSTYCLAW_API_KEY_FILE_DESCRIPTOR",
    "OPENAI_API_KEY",
    "GROQ_API_KEY",
    "DEEPSEEK_API_KEY",
    "GEMINI_API_KEY",
    "GOOGLE_API_KEY",
    "MISTRAL_API_KEY",
    "OPENROUTER_API_KEY",
    "TOGETHER_API_KEY",
    "XAI_API_KEY",
    "VENICE_API_KEY",
];

/// Deny list for the project `.env` in `cwd`, given the global settings.
fn project_dotenv_deny(
    global: &settings::Settings,
    cwd: &std::path::Path,
) -> &'static [&'static str] {
    if settings::Settings::is_trusted(global, cwd) {
        &[]
    } else {
        PROJECT_UNTRUSTED_ENV_KEYS
    }
}

/// Load KEY=VALUE pairs from a .env file into the process environment.
/// Only sets vars from SAFE_ENV_KEYS that are NOT already set and not in
/// `deny`. Skips blank lines and lines starting with #. Returns the `deny`
/// keys the file tried to set, so the caller can say why they were ignored.
fn load_dotenv(path: &std::path::Path, deny: &[&'static str]) -> Vec<&'static str> {
    let mut skipped = Vec::new();
    let content = match settings::read_config_file(path) {
        Ok(Some(content)) => content,
        Ok(None) => return skipped,
        Err(e) => {
            eprintln!("Warning: {}: {e} — ignored", path.display());
            return skipped;
        }
    };
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        if let Some((key, val)) = line.split_once('=') {
            let key = key.trim();
            let val = val.trim().trim_matches('"').trim_matches('\'');
            if let Some(denied) = deny.iter().find(|d| **d == key) {
                if !skipped.contains(denied) {
                    skipped.push(*denied);
                }
                continue;
            }
            // A blank value would claim the key and keep a later .env (say
            // ~/.env behind a project's `ANTHROPIC_API_KEY=`) from filling it.
            if !key.is_empty()
                && !val.is_empty()
                && SAFE_ENV_KEYS.contains(&key)
                && std::env::var(key).is_err()
            {
                // SAFETY: single-threaded at this point — called before tokio runtime starts
                unsafe {
                    std::env::set_var(key, val);
                }
            }
        }
    }
    skipped
}

/// Search common locations for .env files and load them in priority order.
/// Later sources do NOT override earlier ones (env already set always wins).
fn load_dotenv_auto() {
    // 1. CWD/.env  — project-local keys (highest priority, filtered to safe keys only)
    if let Ok(cwd) = std::env::current_dir() {
        let env_path = cwd.join(".env");
        if env_path.exists() {
            // Safe this early: config_dir() depends only on OXIDECLAW_CONFIG_DIR /
            // CLAUDE_CONFIG_DIR / XDG_CONFIG_HOME / HOME, none of which a .env may set.
            let deny = project_dotenv_deny(&settings::Settings::load_global(), &cwd);
            let skipped = load_dotenv(&env_path, deny);
            // Warn if project .env exists — it won't leak into tool subprocesses
            eprintln!(
                "Note: .env detected in project root. Only oxideclaw-specific keys \
                 (ANTHROPIC_API_KEY, OPENAI_API_KEY, etc.; API keys only in trusted \
                 folders) are loaded. Project vars are NOT injected into tool execution."
            );
            if !skipped.is_empty() {
                eprintln!(
                    "Note: ignored {} from the project .env because this project is not \
                     trusted (they choose where prompts are sent and which account receives \
                     them). Run /trust in this folder, \
                     then restart, to allow them.",
                    skipped.join(", ")
                );
            }
        }
    }
    // 2. ~/.env  — user-global keys
    if let Some(home) = dirs::home_dir() {
        load_dotenv(&home.join(".env"), &[]);
    }
    // 3. <config dir>/.env (~/.config/oxideclaw/.env by default) — app-specific
    for path in Config::user_dotenv_paths() {
        load_dotenv(&path, &[]);
    }
}

/// Says which config dir a deprecated `$CLAUDE_CONFIG_DIR` picked, and on
/// the first run of a version with its own config dir, copies OxideClaw's
/// state out of Claude Code's `~/.claude` (which it never changes). Runs
/// before anything reads or writes the config dir.
fn prepare_config_dirs() {
    let choice = Config::config_dir_choice();
    if let Some(notice) = choice.notice() {
        eprintln!("{notice}");
    }
    // A directory the user named is theirs to fill.
    if !choice.source.is_explicit() {
        let data = Config::data_dir();
        if let Some(claude) = Config::claude_code_dir().filter(|d| d.is_dir())
            && claude_import::needs_migration(&choice.dir)
        {
            for line in claude_import::migrate(&claude, &choice.dir, &data) {
                eprintln!("{line}");
            }
        }
        if let Some(line) = claude_import::move_sessions_to_data_dir(&choice.dir, &data) {
            eprintln!("{line}");
        }
    }
    // After the import, which runs only into an empty dir. A named dir
    // ($CLAUDE_CONFIG_DIR) is where older versions kept their settings.
    if let Some(line) = config::migrate_legacy_autonomy(&choice.dir) {
        eprintln!("{line}");
    }
}

/// Exit status owed to a SIGINT/SIGTERM that ended -p, --headless or acp.
static SIGNAL_EXIT: std::sync::OnceLock<i32> = std::sync::OnceLock::new();

/// Runs `fut` to completion, or returns None once SIGINT/SIGTERM arrives.
/// Bash/PowerShell tools and hooks run in their own process groups, so the
/// terminal's Ctrl-C never reaches them, and dying on the default signal
/// action skips the destructors that kill them: they ran on as orphans.
/// Catching the signal lets `main` drop every task before exiting.
async fn until_signal<F: std::future::Future>(fut: F) -> Option<F::Output> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        // Registered before `fut` is first polled, so no tool can start
        // while the default action is still in place.
        let (Ok(mut int), Ok(mut term)) = (
            signal(SignalKind::interrupt()),
            signal(SignalKind::terminate()),
        ) else {
            return Some(fut.await);
        };
        let code = tokio::select! {
            out = fut => return Some(out),
            _ = int.recv() => 130,
            _ = term.recv() => 143,
        };
        let _ = SIGNAL_EXIT.set(code);
        None
    }
    #[cfg(not(unix))]
    {
        tokio::select! {
            out = fut => Some(out),
            _ = tokio::signal::ctrl_c() => {
                let _ = SIGNAL_EXIT.set(130);
                None
            }
        }
    }
}

fn main() -> Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let result = rt.block_on(run());
    if let Some(&code) = SIGNAL_EXIT.get() {
        // Shutdown drops every task (SDK/ACP sessions included), so the
        // guards on tool and hook processes kill them. The timeout covers a
        // blocking stdin read that would otherwise hold the exit until input.
        rt.shutdown_timeout(std::time::Duration::from_secs(1));
        std::process::exit(code);
    }
    result
}

async fn run() -> Result<()> {
    // Respect NO_COLOR (https://no-color.org/) and dumb terminals so piped
    // output / CI logs / `less` don't get ANSI escape codes.
    if std::env::var_os("NO_COLOR").is_some()
        || std::env::var("TERM").map(|t| t == "dumb").unwrap_or(false)
    {
        colored::control::set_override(false);
    }

    // Parse first: `--help`, `--version` and a mistyped flag exit here,
    // before the first-run import or the settings migration touch anything.
    let cli = Cli::parse();

    // A list-only `config import-claude` promises to change nothing. A
    // flagged import still migrates first: it writes settings.json into the
    // config dir, after which the automatic import would never run.
    let list_only_import = matches!(
        &cli.command,
        Some(Commands::Config {
            subcommand: ConfigSubcommand::ImportClaude {
                hooks: false,
                permissions: false,
                api_key_helper: false,
                mcp: false,
            },
        })
    );
    if !list_only_import {
        prepare_config_dirs();
    }

    // Load .env files before anything else so API keys are available
    // to Config::load() and all downstream code.
    load_dotenv_auto();

    // One-shot commands exit quietly when stdout is piped to `head` instead of
    // panicking on "Broken pipe". Long-lived modes keep Rust's SIG_IGN: they
    // write to MCP/LSP child stdin, and SIG_DFL would kill the whole process
    // (terminal left in raw mode) the moment one of those children died.
    #[cfg(unix)]
    {
        let long_lived = cli.headless
            || matches!(
                cli.command,
                Some(Commands::Acp) | Some(Commands::Browse { .. })
            )
            || (!cli.print && cli.command.is_none());
        if !long_lived {
            // SAFETY: plain disposition change; no handler is installed.
            unsafe {
                libc::signal(libc::SIGPIPE, libc::SIG_DFL);
            }
        }
    }

    // Initialize tracing — write to a log file in TUI mode so logs don't corrupt the screen
    let filter = if cli.verbose { "debug" } else { "warn" };
    let tmp = std::env::temp_dir();
    // /tmp is shared: if another user owns oxideclaw.log, use a per-user
    // file, and never refuse to start over a log.
    let open = |p: std::path::PathBuf| {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(p)
            .ok()
    };
    #[cfg(unix)]
    let per_user = format!("oxideclaw-{}.log", unsafe { libc::getuid() });
    #[cfg(not(unix))]
    let per_user = "oxideclaw-user.log".to_string();
    let log_writer: Box<dyn std::io::Write + Send> =
        match open(tmp.join("oxideclaw.log")).or_else(|| open(tmp.join(per_user))) {
            Some(f) => Box::new(f),
            None => Box::new(std::io::sink()),
        };
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::sync::Mutex::new(log_writer))
        .init();

    // --register-protocol: install the deep link handler and exit
    if cli.register_protocol {
        return deeplink::register_protocol();
    }

    // --handle-uri: a link from a browser or another app. It opens the
    // interactive TUI with the prompt pre-filled and never runs headlessly —
    // that would be an unattended agent session with the user's credentials,
    // triggered by any web page.
    if let Some(ref uri) = cli.handle_uri {
        let Some(params) = deeplink::parse_deep_link(uri) else {
            eprintln!("Invalid or unrecognised deep link URI: {uri}");
            std::process::exit(1);
        };
        use std::io::IsTerminal;
        match deeplink::plan(params, std::io::stdin().is_terminal()) {
            deeplink::DeepLinkAction::Refuse(msg) => {
                eprintln!("{msg}");
                std::process::exit(1);
            }
            deeplink::DeepLinkAction::OpenTui { query, cwd } => {
                // The link's directory decides which CLAUDE.md, permission
                // rules and hooks apply, so it goes in before they are read.
                let mut config = Config::load_with(cwd.map(std::path::PathBuf::from), None, false)?;
                let chosen = model_chosen(false, &config);
                keyless_ollama_start(&mut config, chosen, false).await?;
                return tui::run_tui(config, None, Some(query), None).await;
            }
        }
    }

    // --settings is merged over the settings files inside Config::load, so
    // every key applies, its apiKeyHelper runs with the other credential
    // sources, and the CLI flags below still override it. Parsed only where
    // a config is loaded, so `version` and `completions` never fail on it.
    let flag_settings = || {
        cli.settings.as_deref().map(|arg| {
            parse_settings_arg(arg).unwrap_or_else(|e| {
                eprintln!("Error: --settings: {e}");
                std::process::exit(1);
            })
        })
    };

    // Handle subcommands
    if let Some(cmd) = &cli.command {
        match cmd {
            Commands::Acp => {} // needs the full config; handled below
            Commands::Version => {
                println!("oxideclaw {VERSION}");
                return Ok(());
            }
            Commands::Mcp { subcommand } => {
                return handle_mcp_subcommand(subcommand).await;
            }
            Commands::Completions { shell } => {
                let mut cmd = <Cli as clap::CommandFactory>::command();
                clap_complete::generate(*shell, &mut cmd, "oxideclaw", &mut std::io::stdout());
                return Ok(());
            }
            Commands::Doctor => {
                println!("oxideclaw doctor — checking installation health…\n");
                // API key
                if let Ok(key) = std::env::var("ANTHROPIC_API_KEY") {
                    if key.len() >= 4 {
                        println!("  \u{2713} ANTHROPIC_API_KEY set ({}…)", &key[..4]);
                    }
                } else {
                    println!("  \u{2717} ANTHROPIC_API_KEY not set");
                }
                // Config dir
                let config_dir = config::Config::config_dir();
                println!("  \u{2713} Config dir: {}", config_dir.display());
                println!(
                    "  \u{2713} Data dir: {}",
                    config::Config::data_dir().display()
                );
                if config_dir.exists() {
                    println!("  \u{2713} Config dir exists");
                } else {
                    println!("  \u{2717} Config dir missing");
                }
                // Git
                let git_ok = std::process::Command::new("git")
                    .args(["rev-parse", "--is-inside-work-tree"])
                    .output()
                    .map(|o| o.status.success())
                    .unwrap_or(false);
                if git_ok {
                    println!("  \u{2713} Git repository");
                }
                // Ollama
                let ollama_ok = std::process::Command::new("ollama")
                    .arg("--version")
                    .output()
                    .is_ok();
                if ollama_ok {
                    println!("  \u{2713} Ollama available");
                } else {
                    println!("  - Ollama not found (optional)");
                }
                // XTTS v2
                let tts_ok = std::process::Command::new("tts")
                    .arg("--help")
                    .output()
                    .is_ok();
                if tts_ok {
                    println!("  \u{2713} XTTS v2 (Coqui TTS) available");
                } else {
                    println!("  - XTTS v2 not found (optional — needed for TTS)");
                }
                println!("\n  oxideclaw v{VERSION}");
                return Ok(());
            }
            Commands::Update => {
                return self_update().await;
            }
            Commands::Config {
                subcommand:
                    ConfigSubcommand::ImportClaude {
                        hooks,
                        permissions,
                        api_key_helper,
                        mcp,
                    },
            } => {
                let Some(claude) = Config::claude_code_dir() else {
                    anyhow::bail!("no home directory, so no ~/.claude to import from");
                };
                let opts = claude_import::ImportOptions {
                    hooks: *hooks,
                    permissions: *permissions,
                    api_key_helper: *api_key_helper,
                    mcp: *mcp,
                };
                for line in claude_import::import_claude(&claude, &Config::config_dir(), opts)? {
                    println!("{line}");
                }
                return Ok(());
            }
            // Needs the full config so --model, --settings and the other
            // global flags apply; handled below.
            Commands::Browse { .. } => {}
        }
    }

    for w in ignored_flag_warnings(&cli) {
        eprintln!("warning: {w}");
    }

    let mut config = Config::load_with(None, flag_settings(), cli.bare)?;
    let chosen = model_chosen(cli.model.is_some(), &config);

    // Apply CLI overrides (highest priority)
    if cli.verbose {
        config.verbose = true;
    }
    if cli.dangerously_skip_permissions {
        config.dangerously_skip_permissions = true;
    }
    if let Some(model) = cli.model {
        config.model = crate::commands::resolve_model_alias(&model);
    }
    if let Some(name) = cli.name {
        config.session_name = Some(name);
    } else if cli.tmux {
        // When launching with --tmux, use a hostname-prefixed adjective-animal name
        // so the tmux pane/window has a recognisable, stable title.
        config.session_name = Some(session::generate_tmux_session_name());
    }
    if cli.max_turns > 0 {
        config.max_turns = cli.max_turns;
    }
    // --tools first: --allowed-tools rules add their tool to its list.
    config.apply_tools_flag(&cli.tools);
    if let Err(e) = config.apply_tool_flags(&cli.allowed_tools, &cli.disallowed_tools) {
        eprintln!("Error: {e}");
        std::process::exit(1);
    }
    if let Some(sp) = cli.system_prompt {
        config.system_prompt_override = Some(sp);
    }
    if let Some(append) = cli.append_system_prompt {
        config.append_system_prompt = Some(append);
    }
    if let Some(effort) = cli.effort {
        config.effort = Some(effort);
    }
    if let Some(mt) = cli.max_thinking_tokens {
        config.thinking_budget_tokens = Some(mt);
    }
    if let Some(tm) = cli.thinking {
        match tm {
            ThinkingMode::Disabled => config.thinking_budget_tokens = Some(0),
            ThinkingMode::Enabled => {
                if config.thinking_budget_tokens.unwrap_or(0) == 0 {
                    config.thinking_budget_tokens = Some(10_000);
                }
            }
            ThinkingMode::Auto => {} // keep settings value
        }
    }
    if let Some(pm) = cli.permission_mode {
        match pm {
            PermissionMode::Bypass => config.dangerously_skip_permissions = true,
            PermissionMode::Auto => {} // auto-permission via classifier (not yet implemented)
            PermissionMode::Default => {}
        }
    }
    if !cli.betas.is_empty() {
        config.extra_betas = cli.betas.clone();
    }
    if cli.no_session_persistence {
        config.no_session_persistence = true;
    }
    if cli.strict_mcp_config {
        config.strict_mcp_config = true;
    }
    for dir in &cli.add_dir {
        let path = std::path::PathBuf::from(dir);
        let abs = if path.is_absolute() {
            path
        } else {
            config.cwd.join(dir)
        };
        config.extra_dirs.push(abs);
    }
    if cli.bare {
        config.disable_all_hooks = true;
    }
    if cli.disable_slash_commands {
        config.disable_slash_commands = true;
    }
    if cli.allow_dangerously_skip_permissions {
        // Does not enable bypass by default — just allows it to be toggled
        // (stored for future permission prompt support)
    }
    if let Some(fb) = cli.fallback_model.as_deref() {
        config.fallback_model = Some(crate::commands::resolve_model_alias(fb));
    }
    if let Some(budget) = cli.max_budget_usd {
        config.max_budget_usd = Some(budget);
    }
    if let Some(fmt) = cli.input_format {
        config.input_format = Some(fmt);
    }
    if let Some(schema) = cli.json_schema {
        config.json_schema = Some(schema);
    }
    if cli.replay_user_messages {
        config.replay_user_messages = true;
    }
    if cli.fork_session {
        config.fork_session = true;
    }
    if let Some(file) = cli.system_prompt_file {
        match std::fs::read_to_string(&file) {
            Ok(content) => config.system_prompt_override = Some(content.trim().to_string()),
            Err(e) => {
                eprintln!("Error reading --system-prompt-file '{}': {}", file, e);
                std::process::exit(1);
            }
        }
    }
    if let Some(file) = cli.append_system_prompt_file {
        match std::fs::read_to_string(&file) {
            Ok(content) => {
                let trimmed = content.trim().to_string();
                config.append_system_prompt = Some(match config.append_system_prompt {
                    Some(existing) => format!("{}\n\n{}", existing, trimmed),
                    None => trimmed,
                });
            }
            Err(e) => {
                eprintln!(
                    "Error reading --append-system-prompt-file '{}': {}",
                    file, e
                );
                std::process::exit(1);
            }
        }
    }
    if let Some(agents_json) = cli.agents {
        match serde_json::from_str::<serde_json::Value>(&agents_json) {
            Ok(v) => config.custom_agents = Some(v),
            Err(e) => {
                eprintln!("Error parsing --agents JSON: {}", e);
                std::process::exit(1);
            }
        }
    }
    for arg in &cli.mcp_config {
        match parse_mcp_config_arg(arg) {
            Ok(servers) => config.extra_mcp_servers.extend(servers),
            Err(e) => {
                eprintln!("Error: --mcp-config: {e}");
                std::process::exit(1);
            }
        }
    }

    // The TUI shows these in the transcript; the other modes only have stderr.
    if cli.print
        || cli.headless
        || matches!(
            cli.command,
            Some(Commands::Acp) | Some(Commands::Browse { .. })
        )
    {
        warn_settings_load_errors(&config);
    }

    // `oxideclaw acp`: Agent Client Protocol over stdio
    if matches!(cli.command, Some(Commands::Acp)) {
        let stdin = tokio::io::BufReader::new(tokio::io::stdin());
        if let Some(r) = until_signal(crate::acp::AcpServer::run(
            config,
            stdin,
            tokio::io::stdout(),
        ))
        .await
        {
            r?;
        }
        return Ok(());
    }

    // `oxideclaw browse`: the browser agent, once and exit
    if let Some(Commands::Browse {
        goal,
        yolo,
        ask,
        max_steps,
    }) = &cli.command
    {
        use crate::browser::browse_loop::{
            BrowsePolicy, BrowseProgress, BrowseRequest, run_browse,
        };
        use tokio::sync::mpsc;

        let goal_str = goal.join(" ");
        if goal_str.trim().is_empty() {
            eprintln!("Error: browse requires a goal argument");
            std::process::exit(1);
        }

        // Determine policy: --yolo > --ask > settings.browseDefaultPolicy > Pattern.
        let policy = if *yolo {
            // First-time --yolo: write acknowledgment file if not yet present
            if !crate::browser::yolo_ack::is_acknowledged() {
                eprintln!(
                    "Warning: --yolo disables all approval prompts. \
                     The browser agent will execute destructive actions without confirmation.\n\
                     This warning is shown once; the acknowledgment is recorded in your XDG \
                     state directory."
                );
                if let Err(e) = crate::browser::yolo_ack::acknowledge() {
                    eprintln!("Warning: could not write yolo-ack file: {e}");
                }
            }
            BrowsePolicy::Yolo
        } else if *ask {
            BrowsePolicy::Ask
        } else {
            BrowsePolicy::from_settings_str(&config.browse_default_policy)
        };

        let req = BrowseRequest {
            goal: goal_str.clone(),
            policy,
            max_steps: *max_steps,
            voice: false,
        };

        let is_non_anthropic = crate::api::is_ollama_model(&config.model)
            || crate::api::is_openai_compat_model(&config.model);
        if !is_non_anthropic && config.api_key.is_empty() {
            eprintln!(
                "Error: ANTHROPIC_API_KEY not set for model: {}",
                config.model
            );
            std::process::exit(1);
        }
        let (mut tools, shared_state) = crate::tools::all_tools_with_state(&config);
        crate::tools::apply_tool_filters(&mut tools, &config);
        let current_url = std::sync::Arc::new(tokio::sync::Mutex::new(String::new()));
        let browser_session = shared_state.browser_session.clone();

        let (progress_tx, mut progress_rx) = mpsc::channel::<BrowseProgress>(64);
        // Approval channel: in CLI mode auto-deny (user must use --yolo or --ask interactively)
        let (approval_tx, mut approval_rx) =
            mpsc::channel::<crate::browser::approval_gate::ApprovalPrompt>(8);

        // Prompt on stderr, read the answer from stdin. A plain OS
        // thread, not a tokio task: a read left blocked by Ctrl-C or
        // the gate's timeout would keep the runtime from shutting
        // down, and the process would hang at exit until Enter.
        // Returning from main ends this thread.
        std::thread::spawn(move || {
            use std::io::Write;
            while let Some(prompt) = approval_rx.blocking_recv() {
                eprint!(
                    "Approval needed [step {}]: {} on '{}' at {}\n  Reason: {}\nAllow? [y/N] ",
                    prompt.step, prompt.tool_name, prompt.target_text, prompt.url, prompt.reason
                );
                let _ = std::io::stderr().flush();
                let mut line = String::new();
                let allowed = if std::io::stdin().read_line(&mut line).is_ok() {
                    matches!(line.trim().to_lowercase().as_str(), "y" | "yes")
                } else {
                    false
                };
                // The gate stopped waiting while we read: say so,
                // rather than let the answer look like it counted.
                if prompt.reply.send(allowed).is_err() {
                    eprintln!("Approval prompt had already expired; answer ignored.");
                }
            }
        });

        // Spawn task to print progress as NDJSON
        let progress_task = tokio::spawn(async move {
            while let Some(event) = progress_rx.recv().await {
                if let Ok(json) = serde_json::to_string(&event) {
                    println!("{json}");
                }
            }
        });

        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let cancel_clone = cancel.clone();
        tokio::spawn(async move {
            let _ = tokio::signal::ctrl_c().await;
            cancel_clone.store(true, std::sync::atomic::Ordering::SeqCst);
        });
        let channels = crate::browser::browse_loop::BrowseChannels {
            progress_tx,
            approval_tx,
            cancel,
            usage_sink: None,
        };
        let result =
            run_browse(req, &config, tools, current_url, browser_session, channels).await?;
        progress_task.await.ok();

        // Print final result as JSON. A goal not reached (a setup
        // failure such as a missing key included) is a non-zero
        // exit for scripts and CI.
        println!("{}", serde_json::to_string_pretty(&result)?);
        if !result.achieved {
            std::process::exit(1);
        }
        return Ok(());
    }

    // --headless mode: long-running SDK server
    if cli.headless {
        let transport = crate::sdk::transport::stdio::StdioTransport::new();
        if let Some(r) = until_signal(crate::sdk::SdkServer::run(config, transport)).await {
            r?;
        }
        return Ok(());
    }

    // SDK and ACP hosts pick their own models, so only -p and the TUI get
    // here.
    keyless_ollama_start(&mut config, chosen, cli.print).await?;

    // --session-id takes priority: resume that exact session if it exists,
    // otherwise start a new one under that ID. --session also takes the short
    // IDs and names the picker shows; an unknown one is an error rather than a
    // silently fresh session.
    let resume_id = if let Some(raw) = cli.session_id {
        let id = uuid::Uuid::parse_str(raw.trim())
            .map_err(|e| anyhow::anyhow!("--session-id must be a valid UUID: {e}"))?
            .to_string();
        if session::Session::exists(&id) {
            Some(id)
        } else {
            config.new_session_id = Some(id);
            None
        }
    } else if let Some(q) = cli.session {
        Some(
            session::Session::resolve(&q)
                .await
                .map_err(|e| anyhow::anyhow!("--session: {e}"))?,
        )
    } else if cli.resume || cli.continue_session {
        session::Session::most_recent().await
    } else {
        None
    };

    // --print mode: non-interactive, no TUI
    if cli.print {
        // --input-format=stream-json: every user event on stdin is a turn
        let prompts = if config.input_format.as_deref() == Some("stream-json") {
            let mut msgs = stream_json_user_messages(std::io::stdin().lock());
            if msgs.is_empty() && !cli.prompt.is_empty() {
                msgs.push(cli.prompt.join(" "));
            }
            if msgs.is_empty() {
                eprintln!("Error: --input-format stream-json: no user message on stdin");
                std::process::exit(1);
            }
            msgs
        } else {
            if cli.prompt.is_empty() {
                eprintln!("Error: --print requires a prompt argument");
                std::process::exit(1);
            }
            vec![cli.prompt.join(" ")]
        };

        let mut tools = crate::mcp::tools_for_config(&config).await;

        // --json-schema: add a SyntheticOutputTool named "result" with the user's schema
        let json_schema_str = config.json_schema.clone();
        if let Some(ref schema_str) = json_schema_str {
            if let Ok(schema) = serde_json::from_str::<serde_json::Value>(schema_str) {
                use std::sync::Arc;
                tools.push(Arc::new(SyntheticOutputTool { schema }));
            } else {
                eprintln!("Warning: --json-schema is not valid JSON — ignoring");
            }
        }

        let mut engine = QueryEngine::new(config.clone(), tools)?;
        // -p used to ignore the resume flags and run a fresh conversation.
        let mut resumed = None;
        if let Some(id) = &resume_id {
            let (mut s, history) = session::Session::resume(id).await?;
            if config.fork_session && !config.no_session_persistence {
                s.fork(&history).await?;
            }
            engine.resume_history(s.id.clone(), history);
            resumed = Some(s);
        } else if let Some(id) = config.new_session_id.clone()
            && !config.no_session_persistence
        {
            // --session-id naming a session that does not exist yet starts
            // it, so the next `-p --session-id <same>` continues it.
            let s = session::Session::new_with_id(id).await?;
            engine.resume_history(s.id.clone(), Vec::new());
            resumed = Some(s);
        } else if cli.resume || cli.continue_session {
            anyhow::bail!("No previous session to continue.");
        }
        match cli.output_format {
            OutputFormat::Json => engine.set_json_output(true),
            OutputFormat::StreamJson => engine.set_stream_json_output(true),
            OutputFormat::Text => {}
        }
        if cli.include_partial_messages {
            engine.set_include_partial_messages(true);
        }
        if cli.include_hook_events {
            engine.set_include_hook_events(true);
        }
        // A hook blocking a later message still leaves the earlier turns to save.
        let mut outcome = Ok(());
        for prompt in prompts {
            let prompt = match &config.hooks {
                Some(h) if !config.disable_all_hooks => {
                    let r =
                        crate::hooks::run_user_prompt_hooks(h, &prompt, "print-mode", &config.cwd)
                            .await;
                    if !r.should_continue {
                        outcome = Err(anyhow::anyhow!(
                            "Prompt not sent — blocked by a userPromptSubmit hook: {}",
                            r.stop_reason.unwrap_or_default()
                        ));
                        break;
                    }
                    match r.additional_context {
                        Some(extra) => {
                            format!("{prompt}\n\n<additional_context>{extra}</additional_context>")
                        }
                        None => prompt,
                    }
                }
                _ => prompt,
            };
            match until_signal(engine.query(prompt)).await {
                Some(r) => r?,
                None => return Ok(()),
            }
            // A script must be able to tell a cut-off run from a finished one.
            if engine.hit_turn_cap() {
                outcome = Err(anyhow::anyhow!(
                    "stopped at the turn limit before the task finished; raise it with \
                     --max-turns"
                ));
                break;
            }
        }
        // Overwrite, not append: compaction may have rewritten the history.
        if let Some(s) = resumed
            && !config.no_session_persistence
        {
            s.overwrite(engine.history()).await?;
        }
        return outcome;
    }

    // --fork-session: generate a new UUID instead of reusing the original
    let resume_id = if config.fork_session && resume_id.is_some() {
        // We still need to know WHICH session to resume (to load its messages),
        // but we store it as `resume_id` and the TUI will create a new session.
        // The tui/run.rs uses fork_session flag from config to handle this.
        resume_id
    } else {
        resume_id
    };

    // Interactive TUI mode
    tui::run_tui(config, resume_id, None, interactive_prompt(&cli.prompt)).await
}

/// Whether the user picked the model: `--model`, a non-blank
/// `ANTHROPIC_MODEL` (a blank one selects nothing), or `model` in a
/// settings file or `--settings`.
fn model_chosen(cli_model: bool, config: &Config) -> bool {
    cli_model
        || config.settings_model.is_some()
        || std::env::var("ANTHROPIC_MODEL").is_ok_and(|m| !m.trim().is_empty())
}

/// No Anthropic credential and no model chosen: start on a local Ollama
/// model and say why — on stderr for -p (stdout carries the answer), on
/// the TUI's first screen otherwise.
async fn keyless_ollama_start(config: &mut Config, chosen: bool, print: bool) -> Result<()> {
    let Some(name) = config.fall_back_to_local_ollama(chosen).await? else {
        return Ok(());
    };
    let change = if print { "pass --model" } else { "run /model" };
    let notice = format!(
        "No Anthropic key found; using local Ollama model {name}. \
         Set ANTHROPIC_API_KEY or {change} to change."
    );
    if print {
        eprintln!("{notice}");
        // warn_settings_load_errors leaves these to the missing-credential
        // error, which no longer comes.
        for why in &config.api_key_helper_rejected {
            eprintln!("Warning: {why}");
        }
    } else {
        config.startup_notice = Some(notice);
    }
    Ok(())
}

/// Positional words without `-p` start the TUI with that prompt already
/// sent, so `oxideclaw /init` runs /init. They used to be dropped silently.
fn interactive_prompt(words: &[String]) -> Option<String> {
    let text = words.join(" ");
    (!text.trim().is_empty()).then_some(text)
}

/// The GitHub releases `oxideclaw update` installs from. The TUI's daily
/// update notice reads the same source.
fn release_updater() -> self_update::backends::github::UpdateBuilder {
    let mut builder = self_update::backends::github::Update::configure();
    builder
        .repo_owner("ForkedInTime")
        .repo_name("OxideClaw")
        .bin_name("oxideclaw")
        .current_version(VERSION);
    builder
}

/// The newest published release's version (`0.4.1`, no `v`). Blocking; the
/// whole request, DNS included, gives up after `timeout`. Proxies come from
/// `HTTPS_PROXY` / `ALL_PROXY` / `NO_PROXY`, as for `oxideclaw update`.
fn latest_release_version(timeout: std::time::Duration) -> Result<String> {
    let releases = release_updater()
        .timeout(timeout)
        .build()?
        .get_latest_release()?;
    let latest = releases
        .latest()
        .ok_or_else(|| anyhow::anyhow!("no release published"))?;
    Ok(latest.version().to_string())
}

/// Self-update: download the latest release from GitHub and replace the running binary.
async fn self_update() -> Result<()> {
    println!("Checking for updates…");

    // Map from Rust target triple to our release artifact name
    let target = self_update_target();
    println!("Platform: {target}");

    let status = release_updater()
        .target(&target)
        .asset_matcher(move |assets| pick_release_asset(assets, &target))
        .show_download_progress(true)
        .no_confirm(false)
        .build()?
        .update()?;

    if status.is_updated() {
        println!("Updated to {}!", status.version());
    } else {
        println!("Already on latest version ({VERSION}).");
    }

    Ok(())
}

/// Select the release asset for `target` by **exact** name.
///
/// The library's default heuristic is substring matching, and our asset names
/// overlap: `linux-x64` is a substring of `oxideclaw-linux-x64-musl` and of
/// every `.sha256` sidecar. Whichever the API listed first would have been
/// installed as the binary — possibly a checksum text file.
fn pick_release_asset(
    assets: &[self_update::update::ReleaseAsset],
    target: &str,
) -> Option<self_update::update::ReleaseAsset> {
    let want = format!("oxideclaw-{target}");
    assets.iter().find(|a| a.name() == want).cloned()
}

/// Map the current platform to our GitHub release artifact suffix.
fn self_update_target() -> String {
    let os = if cfg!(target_os = "linux") {
        "linux"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "windows") {
        "windows"
    } else {
        "unknown"
    };

    let arch = if cfg!(target_arch = "x86_64") {
        "x64"
    } else if cfg!(target_arch = "aarch64") {
        "arm64"
    } else {
        "unknown"
    };

    // Detect musl on Linux
    let suffix = if cfg!(target_os = "linux") && cfg!(target_env = "musl") {
        "-musl"
    } else {
        ""
    };

    if cfg!(target_os = "windows") {
        format!("{os}-{arch}.exe")
    } else {
        format!("{os}-{arch}{suffix}")
    }
}

/// The `--settings` value: a settings file path or inline JSON. Unlike the
/// settings files, a bad value is an error: it was asked for explicitly.
/// A file gets the same writable-file check on its apiKeyHelper as the
/// settings files: on a shared mount another user could rewrite it.
fn parse_settings_arg(arg: &str) -> std::result::Result<settings::Settings, String> {
    let path = std::path::Path::new(arg);
    let is_file = path.is_file();
    let text = if is_file {
        std::fs::read_to_string(arg).map_err(|e| format!("{arg}: {e}"))?
    } else if arg.trim_start().starts_with('{') {
        arg.to_string()
    } else {
        return Err(format!("{arg}: not a file or a JSON object"));
    };
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
    let parsed: settings::Settings =
        serde_json::from_str(text).map_err(|e| format!("invalid settings JSON: {e}"))?;
    Ok(if is_file {
        settings::Settings::sanitize_unsafe_helper(parsed, path)
    } else {
        parsed
    })
}

/// One `--mcp-config` value: a file path or inline JSON, holding server
/// entries at the top level or under `mcpServers` (the .mcp.json shape).
/// Any entry that is not a valid server is an error: a server the user
/// asked for on the command line must not silently fail to start.
fn parse_mcp_config_arg(
    arg: &str,
) -> std::result::Result<Vec<(String, crate::mcp::types::McpServerConfig)>, String> {
    let text = if std::path::Path::new(arg).is_file() {
        std::fs::read_to_string(arg).map_err(|e| format!("{arg}: {e}"))?
    } else {
        arg.to_string()
    };
    let json: serde_json::Value = serde_json::from_str(&text).map_err(|e| {
        if text.trim_start().starts_with('{') {
            format!("invalid JSON: {e}")
        } else {
            format!("{arg}: not a file or a JSON object")
        }
    })?;
    let json = match json.get("mcpServers") {
        Some(inner) => inner.clone(),
        None => json,
    };
    let serde_json::Value::Object(map) = json else {
        return Err(
            "expected a JSON object of servers, e.g. {\"name\":{\"command\":\"...\"}}".into(),
        );
    };
    map.into_iter()
        .map(|(name, val)| {
            serde_json::from_value(val)
                .map(|cfg| (name.clone(), cfg))
                .map_err(|e| format!("server '{name}': {e}"))
        })
        .collect()
}

/// stderr notice for `Config::settings_load_errors`, `settings_notices`
/// (and an ignored apiKeyHelper) outside the TUI.
fn warn_settings_load_errors(config: &Config) {
    if !config.settings_load_errors.is_empty() {
        eprintln!(
            "Warning: {}",
            settings::load_errors_notice(&config.settings_load_errors)
        );
    }
    for line in &config.settings_notices {
        eprintln!("Warning: {line}");
    }
    // With no key at all the missing-credential error already says why.
    if !config.api_key.is_empty() {
        for why in &config.api_key_helper_rejected {
            eprintln!("Warning: {why}");
        }
    }
}

async fn handle_mcp_subcommand(subcommand: &Option<McpSubcommand>) -> Result<()> {
    let config = Config::load()?;
    let config_dir = Config::config_dir();
    // Otherwise a typo in settings.json reads as "No MCP servers configured".
    warn_settings_load_errors(&config);
    match subcommand {
        None | Some(McpSubcommand::List) => {
            let servers = crate::mcp::scope::list(&config.cwd, &config_dir);
            if servers.is_empty() {
                println!("No MCP servers configured. Add one:");
                println!(
                    "  oxideclaw mcp add <name> <command> [args...]   (or a URL with -t http)"
                );
                println!();
                print_mcp_scopes();
            } else {
                println!("Configured MCP servers ({}):", servers.len());
                let width = servers.iter().map(|s| s.name.len()).max().unwrap_or(0);
                for s in &servers {
                    let kind = match &s.config {
                        crate::mcp::types::McpServerConfig::Stdio(c) => {
                            format!("stdio: {}", c.command)
                        }
                        crate::mcp::types::McpServerConfig::Http(h) => format!("http: {}", h.url),
                    };
                    println!(
                        "  {:width$}  {:7}  ({kind}){}",
                        s.name,
                        s.scope.as_str(),
                        mcp_state(s)
                    );
                }
                println!();
                print_mcp_scopes();
            }
        }
        Some(McpSubcommand::Get { name }) => {
            let entries: Vec<_> = crate::mcp::scope::list(&config.cwd, &config_dir)
                .into_iter()
                .filter(|s| &s.name == name)
                .collect();
            let Some(main) = entries
                .iter()
                .find(|s| s.is_effective())
                .or_else(|| entries.first())
            else {
                eprintln!("Server '{name}' not found.");
                std::process::exit(1);
            };
            println!("MCP server: {name}");
            println!("  scope:     {} ({})", main.scope, main.path.display());
            if main.needs_trust {
                println!("  status:    not started until you /trust this project");
            } else if main.config.is_disabled() {
                println!("  status:    disabled");
            }
            match &main.config {
                crate::mcp::types::McpServerConfig::Stdio(s) => {
                    println!("  transport: stdio");
                    println!("  command:   {}", s.command);
                    if !s.args.is_empty() {
                        println!("  args:      {:?}", s.args);
                    }
                    if !s.env.is_empty() {
                        println!("  env:       {:?}", s.env);
                    }
                }
                crate::mcp::types::McpServerConfig::Http(h) => {
                    println!("  transport: http");
                    println!("  url:       {}", h.url);
                    if !h.headers.is_empty() {
                        println!("  headers:   {:?}", h.headers);
                    }
                }
            }
            for other in entries.iter().filter(|s| !std::ptr::eq(*s, main)) {
                println!(
                    "  also in:   {} ({}){}",
                    other.scope,
                    other.path.display(),
                    mcp_state(other)
                );
            }
        }
        Some(McpSubcommand::Add {
            name,
            command,
            args,
            scope,
            transport,
            env,
            force,
        }) => {
            let cfg = mcp_add_config(transport, command, args, env)?;
            let path = crate::mcp::scope::add(name, cfg, *scope, &config.cwd, &config_dir, *force)?;
            mcp_added(name, *scope, &path, &config);
        }
        Some(McpSubcommand::AddJson {
            name,
            json,
            scope,
            force,
        }) => {
            let cfg: crate::mcp::types::McpServerConfig =
                serde_json::from_str(json).map_err(|e| anyhow::anyhow!("Invalid JSON: {e}"))?;
            let path = crate::mcp::scope::add(name, cfg, *scope, &config.cwd, &config_dir, *force)?;
            mcp_added(name, *scope, &path, &config);
        }
        Some(McpSubcommand::AddFromClaudeDesktop { scope, force }) => {
            let desktop_config = find_claude_desktop_config();
            match desktop_config {
                None => {
                    eprintln!("Claude Desktop config not found. Expected locations:");
                    eprintln!(
                        "  macOS: ~/Library/Application Support/Claude/claude_desktop_config.json"
                    );
                    eprintln!(
                        "  WSL:   /mnt/c/Users/<user>/AppData/Roaming/Claude/claude_desktop_config.json"
                    );
                    std::process::exit(1);
                }
                Some(path) => {
                    let content = std::fs::read_to_string(&path)?;
                    let json: serde_json::Value = serde_json::from_str(&content)?;
                    let servers = json
                        .get("mcpServers")
                        .and_then(|v| v.as_object())
                        .cloned()
                        .unwrap_or_default();
                    if servers.is_empty() {
                        println!("No MCP servers found in Claude Desktop config.");
                        return Ok(());
                    }
                    let mut imported = 0usize;
                    let mut written = None;
                    for (name, val) in &servers {
                        let added = serde_json::from_value::<crate::mcp::types::McpServerConfig>(
                            val.clone(),
                        )
                        .map_err(anyhow::Error::from)
                        .and_then(|cfg| {
                            crate::mcp::scope::add(
                                name,
                                cfg,
                                *scope,
                                &config.cwd,
                                &config_dir,
                                *force,
                            )
                        });
                        match added {
                            Ok(path) => {
                                println!("  Imported: {name}");
                                imported += 1;
                                written = Some(path);
                            }
                            Err(e) => eprintln!("  Skipped '{name}': {e}"),
                        }
                    }
                    match written {
                        Some(path) => println!(
                            "Imported {imported} server(s) from Claude Desktop into the {scope} \
                             scope ({}).",
                            path.display()
                        ),
                        None => println!("Imported no servers from Claude Desktop."),
                    }
                }
            }
        }
        Some(McpSubcommand::Remove { name, scope }) => {
            match crate::mcp::scope::remove(name, *scope, &config.cwd, &config_dir)? {
                Some(from) => println!("Removed MCP server '{name}' from the {from} scope."),
                None => {
                    match scope {
                        Some(s) => eprintln!("Server '{name}' not found in the {s} scope."),
                        None => eprintln!("Server '{name}' not found in any scope."),
                    }
                    std::process::exit(1);
                }
            }
        }
        Some(McpSubcommand::ResetProjectChoices) => {
            // Project MCP servers (.mcp.json, .claude/settings.json) start
            // only while the project is in the global trustedProjects list,
            // so revoking that is the reset, as `/trust revoke` does.
            let global = crate::settings::Settings::load_global();
            if !global.load_errors.is_empty() {
                anyhow::bail!(
                    "{}",
                    crate::settings::load_errors_notice(&global.load_errors)
                );
            }
            let shown = config
                .cwd
                .canonicalize()
                .unwrap_or_else(|_| config.cwd.clone());
            let mut list = global.trusted_projects.unwrap_or_default();
            if crate::settings::Settings::remove_trusted(&mut list, &config.cwd) {
                Config::save_user_setting("trustedProjects", serde_json::json!(list))?;
                println!(
                    "Revoked trust for {}. Its .mcp.json and project MCP servers, settings \
                     hooks and apiKeyHelper will be ignored from the next start; run /trust \
                     to approve them again.",
                    shown.display()
                );
            } else {
                println!(
                    "{} is not trusted, so its .mcp.json and project MCP servers are \
                     already ignored.",
                    shown.display()
                );
            }
        }
    }
    Ok(())
}

/// The server `mcp add` writes. `target` is the command for stdio and the
/// URL for http; an http server written as stdio would try to run its URL.
fn mcp_add_config(
    transport: &str,
    target: &str,
    args: &[String],
    env: &[String],
) -> Result<crate::mcp::types::McpServerConfig> {
    use crate::mcp::types::{HttpServerConfig, McpServerConfig, StdioServerConfig};
    match transport {
        "stdio" => {
            let env = env
                .iter()
                .filter_map(|kv| {
                    let (k, v) = kv.split_once('=')?;
                    Some((k.to_string(), v.to_string()))
                })
                .collect();
            Ok(McpServerConfig::Stdio(StdioServerConfig {
                command: target.to_string(),
                args: args.to_vec(),
                env,
                disabled: false,
                literal: false,
            }))
        }
        "http" => {
            if !target.starts_with("http://") && !target.starts_with("https://") {
                anyhow::bail!("http transport needs an http:// or https:// URL, got '{target}'");
            }
            if !args.is_empty() || !env.is_empty() {
                anyhow::bail!(
                    "arguments and --env apply to stdio servers only; \
                     for HTTP headers use `oxideclaw mcp add-json`"
                );
            }
            Ok(McpServerConfig::Http(HttpServerConfig {
                url: target.to_string(),
                headers: std::collections::HashMap::new(),
                disabled: false,
                literal: false,
                sse: false,
            }))
        }
        other => anyhow::bail!("unknown transport '{other}' (expected stdio or http)"),
    }
}

/// `[needs /trust]`, `[overridden by local]`, `[disabled]` for `mcp list`/`get`.
fn mcp_state(s: &crate::mcp::scope::ScopedServer) -> String {
    let mut state = String::new();
    if s.needs_trust {
        state.push_str("  [needs /trust]");
    }
    if let Some(by) = s.overridden_by {
        state.push_str(&format!("  [overridden by {by}]"));
    }
    if s.config.is_disabled() {
        state.push_str("  [disabled]");
    }
    state
}

fn print_mcp_scopes() {
    println!("Scopes (--scope):");
    println!("  local    only you, only this project (default; kept in your config dir)");
    println!("  project  .mcp.json in the repo: shared, starts only after /trust");
    println!("  user     only you, every project (your settings.json)");
}

fn mcp_added(name: &str, scope: McpScope, path: &std::path::Path, config: &Config) {
    println!(
        "Added MCP server '{name}' to the {scope} scope ({}).",
        path.display()
    );
    if scope == McpScope::Project && !config.project_trusted {
        println!(
            "{} is not trusted, so its .mcp.json servers do not start until you run /trust.",
            config.cwd.display()
        );
    }
}

/// Try to locate Claude Desktop's config file on macOS or WSL.
fn find_claude_desktop_config() -> Option<std::path::PathBuf> {
    // macOS
    if let Some(home) = dirs::home_dir() {
        let mac = home.join("Library/Application Support/Claude/claude_desktop_config.json");
        if mac.exists() {
            return Some(mac);
        }
    }
    // WSL: try /mnt/c/Users/<user>/AppData/Roaming/Claude/
    if let Ok(entries) = std::fs::read_dir("/mnt/c/Users") {
        for entry in entries.flatten() {
            let p = entry
                .path()
                .join("AppData/Roaming/Claude/claude_desktop_config.json");
            if p.exists() {
                return Some(p);
            }
        }
    }
    None
}

#[cfg(test)]
mod self_update_tests {
    use self_update::update::ReleaseAsset;

    fn assets(names: &[&str]) -> Vec<ReleaseAsset> {
        names
            .iter()
            .map(|n| ReleaseAsset::new(*n, format!("https://host/{n}")))
            .collect()
    }

    /// `linux-x64` is a substring of the musl binary and of both `.sha256`
    /// sidecars. Whatever the API lists first must not win — only the exact
    /// name may.
    #[test]
    fn picks_exact_asset_even_when_substring_matches_come_first() {
        let a = assets(&[
            "manifest.json",
            "oxideclaw-linux-x64.sha256",
            "oxideclaw-linux-x64-musl",
            "oxideclaw-linux-x64-musl.sha256",
            "oxideclaw-linux-x64",
        ]);
        let got = super::pick_release_asset(&a, "linux-x64").expect("asset");
        assert_eq!(got.name(), "oxideclaw-linux-x64");

        let got = super::pick_release_asset(&a, "linux-x64-musl").expect("asset");
        assert_eq!(got.name(), "oxideclaw-linux-x64-musl");
    }

    #[test]
    fn windows_target_carries_exe_suffix() {
        let a = assets(&[
            "oxideclaw-windows-x64.exe",
            "oxideclaw-windows-x64.exe.sha256",
        ]);
        let got = super::pick_release_asset(&a, "windows-x64.exe").expect("asset");
        assert_eq!(got.name(), "oxideclaw-windows-x64.exe");
    }

    #[test]
    fn missing_asset_is_none_not_a_near_match() {
        let a = assets(&["oxideclaw-linux-x64.sha256", "oxideclaw-linux-x64-musl"]);
        assert!(super::pick_release_asset(&a, "linux-x64").is_none());
    }
}

/// The text of every `{"type":"user"}` event in `--input-format stream-json`
/// input, in order. `content` may be a plain string or an array of blocks,
/// as in the Messages API; an array's text blocks are joined.
fn stream_json_user_messages(input: impl std::io::BufRead) -> Vec<String> {
    let mut out = Vec::new();
    for line in input.lines() {
        let Ok(line) = line else { break };
        let Ok(event) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if event.get("type").and_then(|v| v.as_str()) != Some("user") {
            continue;
        }
        let text = match event.get("message").and_then(|m| m.get("content")) {
            Some(serde_json::Value::String(s)) => s.clone(),
            Some(serde_json::Value::Array(blocks)) => blocks
                .iter()
                .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
                .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join("\n"),
            _ => continue,
        };
        // An empty text block is a 400 from the API.
        if !text.trim().is_empty() {
            out.push(text);
        }
    }
    out
}

/// Flags kept so Claude Code command lines still parse, but which OxideClaw
/// does not implement. Running anyway is right; doing it silently is not, since
/// the user believes they are in a worktree or that project settings are off.
fn ignored_flag_warnings(cli: &Cli) -> Vec<&'static str> {
    let mut out = Vec::new();
    if cli.worktree.is_some() || cli.tmux {
        out.push(
            "--worktree and --tmux are not implemented and were ignored; this session runs \
             in the current directory. Use `oxideclaw spawn \"<task>\"` for an agent in its \
             own git worktree.",
        );
    }
    // Only the -p engine switches models on an overload; the TUI, --headless
    // and acp loops retry the primary model.
    if cli.fallback_model.is_some() && !cli.print {
        out.push(
            "--fallback-model only applies with --print and was ignored; this session \
             retries the primary model when it is overloaded.",
        );
    }
    if !cli.setting_sources.is_empty() {
        out.push(
            "--setting-sources is not implemented and was ignored; user and project \
             settings both load. Use --settings <file> to add settings for one run.",
        );
    }
    if !cli.add_dir.is_empty() {
        out.push(
            "--add-dir is not implemented and was ignored; tools are not confined to the \
             project directory, except under the sandbox, which only binds the project \
             directory.",
        );
    }
    if cli.agents.is_some() {
        out.push(
            "--agents is not implemented and was ignored; custom agent definitions are \
             not supported yet.",
        );
    }
    if matches!(cli.permission_mode, Some(PermissionMode::Auto)) {
        out.push(
            "--permission-mode auto is not implemented; running with the default \
             permission prompts.",
        );
    }
    if cli.allow_dangerously_skip_permissions {
        out.push(
            "--allow-dangerously-skip-permissions has no effect; pass \
             --dangerously-skip-permissions to skip permission prompts.",
        );
    }
    out
}

#[cfg(test)]
mod cli_parse_tests {
    use super::Cli;
    use clap::Parser;

    /// Flags after the prompt must still be parsed as flags, not appended to
    /// the prompt text (where restrictions like --disallowed-tools vanish).
    #[test]
    fn flags_after_the_prompt_are_parsed() {
        let cli = Cli::try_parse_from([
            "oxideclaw",
            "-p",
            "refactor auth",
            "--disallowed-tools",
            "Bash",
            "--max-budget-usd",
            "1",
        ])
        .unwrap();
        assert!(cli.print);
        assert_eq!(cli.prompt, vec!["refactor auth"]);
        assert_eq!(cli.disallowed_tools, vec!["Bash"]);
        assert_eq!(cli.max_budget_usd, Some(1.0));
    }

    /// Claude Code's `--disallowedTools A B` form: with one value per flag,
    /// `WebFetch` went into the prompt and stayed available.
    #[test]
    fn tool_flags_take_every_following_argument() {
        let cli = Cli::try_parse_from([
            "oxideclaw",
            "-p",
            "x",
            "--disallowed-tools",
            "Bash",
            "WebFetch",
            "--allowed-tools",
            "Read",
            "Bash(git status:*)",
        ])
        .unwrap();
        assert_eq!(cli.prompt, vec!["x"]);
        assert_eq!(cli.disallowed_tools, vec!["Bash", "WebFetch"]);
        assert_eq!(cli.allowed_tools, vec!["Read", "Bash(git status:*)"]);

        // `--` or `=` ends the list, so the prompt and subcommands still work.
        let cli =
            Cli::try_parse_from(["oxideclaw", "--disallowed-tools", "Bash", "--", "fix", "it"])
                .unwrap();
        assert_eq!(cli.disallowed_tools, vec!["Bash"]);
        assert_eq!(cli.prompt, vec!["fix", "it"]);
        let cli = Cli::try_parse_from([
            "oxideclaw",
            "--disallowed-tools=browser_fill",
            "browse",
            "find docs",
        ])
        .unwrap();
        assert_eq!(cli.disallowed_tools, vec!["browser_fill"]);
        assert!(matches!(cli.command, Some(super::Commands::Browse { .. })));
    }

    #[test]
    fn unquoted_prompt_words_and_double_dash_still_collect() {
        let cli =
            Cli::try_parse_from(["oxideclaw", "-p", "fix", "the", "doctor", "--verbose"]).unwrap();
        assert_eq!(cli.prompt, vec!["fix", "the", "doctor"]);
        assert!(cli.verbose);
        assert!(cli.command.is_none());

        let cli = Cli::try_parse_from(["oxideclaw", "-p", "--", "fix", "-x", "flag"]).unwrap();
        assert_eq!(cli.prompt, vec!["fix", "-x", "flag"]);
    }

    #[test]
    fn positional_words_become_the_interactive_prompt() {
        let cli = Cli::try_parse_from(["oxideclaw", "/init"]).unwrap();
        assert!(!cli.print);
        assert_eq!(
            super::interactive_prompt(&cli.prompt).as_deref(),
            Some("/init")
        );
        let cli = Cli::try_parse_from(["oxideclaw", "explain", "main.rs", "--verbose"]).unwrap();
        assert_eq!(
            super::interactive_prompt(&cli.prompt).as_deref(),
            Some("explain main.rs")
        );
        let cli = Cli::try_parse_from(["oxideclaw"]).unwrap();
        assert_eq!(super::interactive_prompt(&cli.prompt), None);
    }

    /// These flags parsed and then did nothing, so `--worktree foo` edited the
    /// main tree with no hint that it had.
    #[test]
    fn unimplemented_flags_warn_instead_of_vanishing() {
        let warns = |args: &[&str]| {
            let mut argv = vec!["oxideclaw"];
            argv.extend_from_slice(args);
            super::ignored_flag_warnings(&Cli::try_parse_from(argv).unwrap())
        };
        assert!(warns(&[]).is_empty());
        assert!(warns(&["--worktree"])[0].contains("--worktree"));
        assert!(warns(&["--worktree", "feat"])[0].contains("--worktree"));
        assert!(warns(&["--tmux"])[0].contains("--tmux"));
        assert!(warns(&["--setting-sources", "user,project"])[0].contains("--setting-sources"));
        assert!(warns(&["--add-dir", "../lib"])[0].contains("--add-dir"));
        assert!(warns(&["--agents", "{}"])[0].contains("--agents"));
        assert!(warns(&["--permission-mode", "auto"])[0].contains("auto"));
        assert!(warns(&["--permission-mode", "bypass"]).is_empty());
        assert!(
            warns(&["--allow-dangerously-skip-permissions"])[0]
                .contains("--allow-dangerously-skip-permissions")
        );
        // Only the -p engine switches to the fallback on an overload.
        assert!(warns(&["--fallback-model", "haiku"])[0].contains("--fallback-model"));
        assert!(warns(&["-p", "hi", "--fallback-model", "haiku"]).is_empty());
    }

    /// Only the last user event was kept, and string `content` (valid in the
    /// Messages API) was skipped, leaving an empty prompt.
    #[test]
    fn stream_json_input_keeps_every_user_message() {
        let input = concat!(
            r#"{"type":"user","message":{"role":"user","content":"first"}}"#,
            "\n\n",
            r#"{"type":"system","subtype":"init"}"#,
            "\nnot json\n",
            r#"{"type":"user","message":{"content":[{"type":"text","text":"a"},{"type":"image"},{"type":"text","text":"b"}]}}"#,
            "\n",
            r#"{"type":"user","message":{"content":""}}"#,
            "\n",
        );
        assert_eq!(
            super::stream_json_user_messages(input.as_bytes()),
            vec!["first", "a\nb"]
        );
        assert!(super::stream_json_user_messages(&b""[..]).is_empty());
    }

    /// The flag split values on spaces, so ordinary pretty JSON became
    /// fragments that failed to parse and were dropped without a warning.
    #[test]
    fn mcp_config_json_with_spaces_is_one_value() {
        let json = r#"{"fs": {"command": "npx", "args": ["-y", "server fs"]}}"#;
        let cli =
            Cli::try_parse_from(["oxideclaw", "--mcp-config", json, "--mcp-config", "{}"]).unwrap();
        assert_eq!(cli.mcp_config, vec![json, "{}"]);
        let servers = super::parse_mcp_config_arg(json).unwrap();
        assert_eq!(servers.len(), 1);
        let (name, crate::mcp::types::McpServerConfig::Stdio(s)) = &servers[0] else {
            panic!("expected a stdio server: {servers:?}");
        };
        assert_eq!(name, "fs");
        assert_eq!(s.args, vec!["-y", "server fs"]);
    }

    #[test]
    fn mcp_config_reads_files_and_reports_bad_entries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp.json");
        std::fs::write(
            &path,
            r#"{"mcpServers": {"web": {"url": "https://x.test/mcp"}}}"#,
        )
        .unwrap();
        let servers = super::parse_mcp_config_arg(path.to_str().unwrap()).unwrap();
        assert_eq!(servers[0].0, "web");

        let err = super::parse_mcp_config_arg(r#"{"bad": {"cmd": "x"}}"#).unwrap_err();
        assert!(err.contains("'bad'"), "{err}");
        assert!(super::parse_mcp_config_arg("{not json").is_err());
        assert!(super::parse_mcp_config_arg("/no/such/mcp.json").is_err());
        assert!(super::parse_mcp_config_arg("[1]").is_err());
    }
}

#[cfg(test)]
mod settings_arg_tests {
    use super::parse_settings_arg;

    /// A missing file or bad JSON was dropped silently, and the run went
    /// ahead without the settings (deny rules included) it was asked for.
    #[test]
    fn bad_settings_values_are_errors() {
        assert!(parse_settings_arg("/no/such/settings.json").is_err());
        assert!(parse_settings_arg(r#"{"model": "#).is_err());
        // Wrong type: the whole value is rejected, not just that key.
        assert!(parse_settings_arg(r#"{"maxTokens": "8000"}"#).is_err());
    }

    #[test]
    fn settings_come_from_a_file_or_inline_json_with_every_key() {
        let s = parse_settings_arg(r#"{"permissions": {"deny": ["Bash(rm:*)"]}, "effort": "low"}"#)
            .unwrap();
        assert_eq!(s.permissions.deny, vec!["Bash(rm:*)"]);
        assert_eq!(s.effort.as_deref(), Some("low"));

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.json");
        std::fs::write(&path, "\u{feff}{\"model\": \"haiku\"}").unwrap();
        let s = parse_settings_arg(path.to_str().unwrap()).unwrap();
        assert_eq!(s.model.as_deref(), Some("haiku"));
    }

    /// A world-writable --settings file's apiKeyHelper ran through `sh -c`,
    /// while the same file as ~/.claude/settings.json was refused.
    #[cfg(unix)]
    #[test]
    fn a_writable_settings_file_does_not_run_its_helper() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("team.json");
        std::fs::write(&path, r#"{"apiKeyHelper": "echo key"}"#).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666)).unwrap();
        let s = parse_settings_arg(path.to_str().unwrap()).unwrap();
        assert_eq!(s.api_key_helper, None);
        assert!(!s.helper_rejected.is_empty());

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let s = parse_settings_arg(path.to_str().unwrap()).unwrap();
        assert_eq!(s.api_key_helper.as_deref(), Some("echo key"));
        // Inline JSON is the caller's own argument.
        let s = parse_settings_arg(r#"{"apiKeyHelper": "echo key"}"#).unwrap();
        assert_eq!(s.api_key_helper.as_deref(), Some("echo key"));
    }
}

#[cfg(test)]
mod mcp_add_tests {
    use super::mcp_add_config;
    use crate::mcp::types::McpServerConfig;

    /// `-t http` was ignored: the URL was saved as a stdio command, and
    /// startup then tried to run it as a program.
    #[test]
    fn http_transport_writes_an_http_server() {
        let cfg = mcp_add_config("http", "https://mcp.example.test/mcp", &[], &[]).unwrap();
        let McpServerConfig::Http(h) = &cfg else {
            panic!("expected http: {cfg:?}");
        };
        assert_eq!(h.url, "https://mcp.example.test/mcp");
        // It must also come back as http from settings.json.
        let json = serde_json::to_value(&cfg).unwrap();
        assert!(matches!(
            serde_json::from_value(json).unwrap(),
            McpServerConfig::Http(_)
        ));
    }

    #[test]
    fn stdio_is_unchanged_and_bad_input_is_refused() {
        let args = vec!["-y".to_string(), "pkg".to_string()];
        let env = vec!["TOKEN=a=b".to_string()];
        let McpServerConfig::Stdio(s) = mcp_add_config("stdio", "npx", &args, &env).unwrap() else {
            panic!("expected stdio");
        };
        assert_eq!(s.command, "npx");
        assert_eq!(s.args, args);
        assert_eq!(s.env["TOKEN"], "a=b");

        assert!(mcp_add_config("sse", "https://x.test", &[], &[]).is_err());
        assert!(mcp_add_config("http", "npx", &[], &[]).is_err());
        assert!(mcp_add_config("http", "https://x.test", &args, &[]).is_err());
    }
}

#[cfg(test)]
mod dotenv_allowlist_tests {
    use super::{
        FORBIDDEN_ENV_KEYS, PROJECT_UNTRUSTED_ENV_KEYS, SAFE_ENV_KEYS, load_dotenv,
        project_dotenv_deny, settings,
    };
    use std::io::Write;

    /// Every var the threat model says must be blocked must NOT appear in
    /// the allowlist — otherwise a malicious project `.env` can pivot
    /// permission prompts, config resolution, or process exec.
    #[test]
    fn forbidden_keys_are_not_in_allowlist() {
        for k in FORBIDDEN_ENV_KEYS {
            assert!(
                !SAFE_ENV_KEYS.contains(k),
                "{k} is in SAFE_ENV_KEYS but must be forbidden — see threat model in SAFE_ENV_KEYS doc"
            );
        }
    }

    /// Every source in the documented credential chain must be settable from
    /// .env. ANTHROPIC_AUTH_TOKEN was missing when OAuth support landed, so a
    /// project authenticating with a token silently fell back to whatever key
    /// was in the ambient environment.
    #[test]
    fn dotenv_allowlist_covers_the_whole_credential_chain() {
        for key in [
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_AUTH_TOKEN",
            "ANTHROPIC_PROFILE",
            "OXIDECLAW_API_KEY_FILE_DESCRIPTOR",
        ] {
            assert!(
                SAFE_ENV_KEYS.contains(&key),
                "{key} must be loadable from .env — it is part of credential resolution"
            );
        }
    }

    /// Every provider key, Gemini's GOOGLE_API_KEY fallback included, loads
    /// from the user's .env, and a cloned repo's .env cannot set it.
    #[test]
    fn every_provider_key_is_loadable_and_project_gated() {
        for p in crate::api::PROVIDERS {
            for key in crate::api::openai_compat::provider_key_envs(p) {
                assert!(SAFE_ENV_KEYS.contains(&key), "{key} not loadable from .env");
                assert!(
                    PROJECT_UNTRUSTED_ENV_KEYS.contains(&key),
                    "{key} settable by an untrusted project .env"
                );
            }
        }
    }

    /// Redirecting the API base URL from a project .env would let a hostile
    /// repo point real credentials at an attacker-controlled host.
    #[test]
    fn dotenv_allowlist_excludes_base_url_redirect() {
        assert!(
            !SAFE_ENV_KEYS.contains(&"ANTHROPIC_BASE_URL"),
            "ANTHROPIC_BASE_URL must never be settable from .env"
        );
    }

    /// A cloned repo's .env must not choose where prompts go until the user
    /// trusts the project; once trusted, nothing is denied.
    #[test]
    fn project_dotenv_deny_depends_on_trust() {
        let dir = tempfile::tempdir().unwrap();
        let untrusted = settings::Settings::default();
        assert_eq!(
            project_dotenv_deny(&untrusted, dir.path()),
            PROJECT_UNTRUSTED_ENV_KEYS
        );
        let trusted = settings::Settings {
            trusted_projects: Some(vec![
                dir.path()
                    .canonicalize()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
            ]),
            ..Default::default()
        };
        assert!(project_dotenv_deny(&trusted, dir.path()).is_empty());
    }

    /// Denied keys are reported and never reach the process environment.
    #[test]
    fn load_dotenv_skips_denied_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".env");
        std::fs::write(
            &path,
            "OLLAMA_HOST=http://attacker.invalid:11434\n\
             export ANTHROPIC_MODEL=\"ollama:attacker-model\"\n\
             OLLAMA_HOST=http://attacker.invalid:2\n\
             OPENROUTER_API_KEY=sk-or-attacker\n",
        )
        .unwrap();
        let skipped = load_dotenv(&path, PROJECT_UNTRUSTED_ENV_KEYS);
        assert_eq!(
            skipped,
            vec!["OLLAMA_HOST", "ANTHROPIC_MODEL", "OPENROUTER_API_KEY"]
        );
        assert_ne!(
            std::env::var("OPENROUTER_API_KEY").ok().as_deref(),
            Some("sk-or-attacker")
        );
        assert!(
            !std::env::var("OLLAMA_HOST")
                .unwrap_or_default()
                .contains("attacker.invalid")
        );
        assert!(
            !std::env::var("ANTHROPIC_MODEL")
                .unwrap_or_default()
                .contains("attacker-model")
        );
    }

    /// `Out-File -Encoding utf8` in Windows PowerShell 5.1 writes a BOM; the
    /// first key must still be recognised, not read as "\u{feff}KEY".
    #[test]
    fn load_dotenv_ignores_a_utf8_bom() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".env");
        std::fs::write(&path, "\u{feff}OLLAMA_HOST=http://bom.invalid:1\r\n").unwrap();
        assert_eq!(
            load_dotenv(&path, PROJECT_UNTRUSTED_ENV_KEYS),
            vec!["OLLAMA_HOST"]
        );
    }

    /// A blank `KEY=` in the first .env set the key to "", so the real
    /// value in a later .env (e.g. ~/.env) was skipped and auth found none.
    #[test]
    fn load_dotenv_blank_value_does_not_shadow_a_later_file() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project.env");
        let home = dir.path().join("home.env");
        std::fs::write(&project, "XAI_API_KEY=\nXAI_API_KEY=\"\"\n").unwrap();
        std::fs::write(&home, "XAI_API_KEY=xai-real\n").unwrap();
        let snap = std::env::var("XAI_API_KEY").ok();
        unsafe { std::env::remove_var("XAI_API_KEY") };

        load_dotenv(&project, &[]);
        load_dotenv(&home, &[]);
        let got = std::env::var("XAI_API_KEY").ok();

        unsafe {
            match snap {
                Some(v) => std::env::set_var("XAI_API_KEY", v),
                None => std::env::remove_var("XAI_API_KEY"),
            }
        }
        assert_eq!(got.as_deref(), Some("xai-real"));
    }

    /// Windows PowerShell 5.1 writes a BOM; the first key used to carry it
    /// and fail the allowlist without a word.
    #[test]
    fn load_dotenv_strips_a_utf8_bom() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".env");
        std::fs::write(&path, "\u{feff}VENICE_API_KEY=venice-key\r\n").unwrap();
        let snap = std::env::var("VENICE_API_KEY").ok();
        unsafe { std::env::remove_var("VENICE_API_KEY") };

        load_dotenv(&path, &[]);
        let got = std::env::var("VENICE_API_KEY").ok();

        unsafe {
            match snap {
                Some(v) => std::env::set_var("VENICE_API_KEY", v),
                None => std::env::remove_var("VENICE_API_KEY"),
            }
        }
        assert_eq!(got.as_deref(), Some("venice-key"));
    }

    /// A credential from an untrusted repo's `.env` picks the account that
    /// receives every prompt; each one the loader accepts must be gated.
    #[test]
    fn every_credential_key_needs_trust() {
        for k in SAFE_ENV_KEYS {
            if k.ends_with("_API_KEY") || k.ends_with("_TOKEN") || k.contains("KEY_FILE") {
                assert!(PROJECT_UNTRUSTED_ENV_KEYS.contains(k), "{k}");
            }
        }
    }

    /// A repo's `.env -> /dev/zero` used to exhaust memory before --help.
    #[cfg(unix)]
    #[test]
    fn load_dotenv_refuses_a_device_link() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".env");
        std::os::unix::fs::symlink("/dev/zero", &path).unwrap();
        assert!(load_dotenv(&path, PROJECT_UNTRUSTED_ENV_KEYS).is_empty());
    }

    /// A malicious .env that sets dangerous vars must not leak into the
    /// process environment when we run `load_dotenv` against it.
    ///
    /// This is a live, end-to-end test of the loader against a real file.
    /// We use `OXIDECLAW_VERBOSE` as the "safe var loaded" probe rather than
    /// `ANTHROPIC_API_KEY` so we don't clobber a real credential.
    #[test]
    fn load_dotenv_blocks_dangerous_vars() {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let suffix = format!(
            "{}_{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        );
        let path = std::env::temp_dir().join(format!("oxideclaw-dotenv-test-{suffix}.env"));
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "PATH=/evil/bin").unwrap();
        writeln!(f, "LD_PRELOAD=/evil/libhack.so").unwrap();
        writeln!(f, "CLAUDE_DANGEROUSLY_SKIP_PERMISSIONS=1").unwrap();
        writeln!(f, "CLAUDE_CONFIG_DIR=/tmp/attacker/config").unwrap();
        writeln!(f, "XDG_CONFIG_HOME=/tmp/attacker/xdg").unwrap();
        writeln!(f, "OXIDECLAW_SANDBOX_COMMAND=/evil/bwrap").unwrap();
        writeln!(f, "OXIDECLAW_VERBOSE=1").unwrap();
        drop(f);

        // Snapshot-restore anything we might touch so we don't perturb the
        // test harness's own environment.
        let snap_ck = std::env::var("CLAUDE_DANGEROUSLY_SKIP_PERMISSIONS").ok();
        let snap_cd = std::env::var("CLAUDE_CONFIG_DIR").ok();
        let snap_xdg = std::env::var("XDG_CONFIG_HOME").ok();
        let snap_sbox = std::env::var("OXIDECLAW_SANDBOX_COMMAND").ok();
        let snap_verb = std::env::var("OXIDECLAW_VERBOSE").ok();
        let original_path = std::env::var("PATH").unwrap_or_default();

        unsafe {
            std::env::remove_var("CLAUDE_DANGEROUSLY_SKIP_PERMISSIONS");
            std::env::remove_var("CLAUDE_CONFIG_DIR");
            std::env::remove_var("XDG_CONFIG_HOME");
            std::env::remove_var("OXIDECLAW_SANDBOX_COMMAND");
            std::env::remove_var("OXIDECLAW_VERBOSE");
        }

        load_dotenv(&path, &[]);

        // Safe var loaded.
        assert_eq!(
            std::env::var("OXIDECLAW_VERBOSE").ok().as_deref(),
            Some("1"),
            "safe var OXIDECLAW_VERBOSE should have been loaded"
        );
        // Dangerous vars NOT loaded.
        assert!(
            std::env::var("CLAUDE_DANGEROUSLY_SKIP_PERMISSIONS").is_err(),
            "CLAUDE_DANGEROUSLY_SKIP_PERMISSIONS must NEVER be loaded from .env"
        );
        assert!(
            std::env::var("CLAUDE_CONFIG_DIR").is_err(),
            "CLAUDE_CONFIG_DIR must NEVER be loaded from .env"
        );
        assert!(
            std::env::var("XDG_CONFIG_HOME").is_err(),
            "XDG_CONFIG_HOME must NEVER be loaded from .env"
        );
        assert!(
            std::env::var("OXIDECLAW_SANDBOX_COMMAND").is_err(),
            "OXIDECLAW_SANDBOX_COMMAND must NEVER be loaded from .env"
        );
        // PATH must be untouched (loader only sets keys that are NOT already set,
        // but even if PATH were unset we still block it via the allowlist).
        assert_eq!(
            std::env::var("PATH").unwrap_or_default(),
            original_path,
            "PATH must NEVER be overwritten from .env"
        );
        assert!(
            std::env::var("LD_PRELOAD").ok().as_deref() != Some("/evil/libhack.so"),
            "LD_PRELOAD must NEVER be loaded from .env"
        );

        // Cleanup
        let _ = std::fs::remove_file(&path);
        unsafe {
            std::env::remove_var("OXIDECLAW_VERBOSE");
            if let Some(v) = snap_ck {
                std::env::set_var("CLAUDE_DANGEROUSLY_SKIP_PERMISSIONS", v);
            }
            if let Some(v) = snap_cd {
                std::env::set_var("CLAUDE_CONFIG_DIR", v);
            }
            if let Some(v) = snap_xdg {
                std::env::set_var("XDG_CONFIG_HOME", v);
            }
            if let Some(v) = snap_sbox {
                std::env::set_var("OXIDECLAW_SANDBOX_COMMAND", v);
            }
            if let Some(v) = snap_verb {
                std::env::set_var("OXIDECLAW_VERBOSE", v);
            }
        }
    }
}

#[cfg(test)]
mod mcp_scope_tests {
    use crate::mcp::scope::{Scope, add, remove};
    use crate::mcp::types::{McpServerConfig, StdioServerConfig};

    fn server_with_token() -> McpServerConfig {
        McpServerConfig::Stdio(StdioServerConfig {
            command: "npx".into(),
            args: vec!["srv".into()],
            env: [("GITHUB_TOKEN".to_string(), "ghp_x".to_string())].into(),
            disabled: false,
            literal: false,
        })
    }

    /// `mcp add -e TOKEN=...` with the default scope wrote the token into the
    /// repo's shared `.mcp.json`, which an untrusted project then ignored.
    #[test]
    fn default_local_scope_is_private_and_loaded_in_an_untrusted_project() {
        let home = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let path = add(
            "gh",
            server_with_token(),
            Scope::Local,
            repo.path(),
            home.path(),
            false,
        )
        .unwrap();

        assert!(path.starts_with(home.path()), "{}", path.display());
        assert!(!repo.path().join(".mcp.json").exists());
        assert!(!repo.path().join(".claude").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }

        let settings = crate::settings::Settings::load_in(home.path(), repo.path());
        assert!(settings.mcp_servers.contains_key("gh"));
        assert!(settings.untrusted_project_config.is_empty());

        assert_eq!(
            remove("gh", None, repo.path(), home.path()).unwrap(),
            Some(Scope::Local)
        );
        let settings = crate::settings::Settings::load_in(home.path(), repo.path());
        assert!(!settings.mcp_servers.contains_key("gh"));
    }

    #[test]
    fn an_unknown_scope_is_a_cli_error() {
        use clap::Parser;
        let parse = |scope: &str| {
            super::Cli::try_parse_from(["oxideclaw", "mcp", "add", "-s", scope, "gh", "npx"])
        };
        assert!(parse("loacl").is_err());
        assert!(parse("project").is_ok());
        // The default is local.
        let cli = super::Cli::try_parse_from(["oxideclaw", "mcp", "add", "gh", "npx"]).unwrap();
        let Some(super::Commands::Mcp {
            subcommand: Some(super::McpSubcommand::Add { scope, force, .. }),
        }) = cli.command
        else {
            panic!("expected mcp add");
        };
        assert_eq!(scope, Scope::Local);
        assert!(!force);
    }

    /// Entries older versions wrote to `.mcp.json` with `--scope local` are
    /// the project scope now and can still be removed.
    #[test]
    fn remove_without_scope_also_sweeps_legacy_mcp_json() {
        let home = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let legacy = repo.path().join(".mcp.json");
        std::fs::write(&legacy, r#"{"mcpServers":{"gh":{"command":"npx"}}}"#).unwrap();
        assert_eq!(
            remove("gh", None, repo.path(), home.path()).unwrap(),
            Some(Scope::Project)
        );
        let left = std::fs::read_to_string(&legacy).unwrap();
        assert!(!left.contains("\"gh\""), "{left}");
    }
}
