/// rustyclaw — Rust-native AI coding CLI
/// Entry point
mod api;
mod autofix;
mod browser;
mod commands;
mod compact;
mod config;
mod cost;
mod deeplink;
mod distro;
mod hooks;
mod mcp;
mod memory;
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
mod voice;
mod watch;

use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};
use config::Config;
use query_engine::QueryEngine;
use tools::all_tools;

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
    name = "rustyclaw",
    version = VERSION,
    about = "RustyClaw — Rust-native AI coding CLI",
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

    /// Model to use (default: claude-sonnet-4-6)
    #[arg(long)]
    model: Option<String>,

    /// Resume the most recent session
    #[arg(short = 'r', long)]
    resume: bool,

    /// Continue the most recent session (alias for --resume)
    #[arg(short = 'c', long = "continue")]
    continue_session: bool,

    /// Resume a specific session by ID prefix
    #[arg(long)]
    session: Option<String>,

    /// Session display name
    #[arg(short = 'n', long)]
    name: Option<String>,

    /// Output format for --print mode: text (default), json, stream-json
    #[arg(long, value_enum, default_value = "text")]
    output_format: OutputFormat,

    /// Max agentic turns before stopping (0 = unlimited)
    #[arg(long, default_value = "0")]
    max_turns: u32,

    /// Tools to allow (comma-separated or repeated flag). Restricts to this set.
    #[arg(long, value_delimiter = ',')]
    allowed_tools: Vec<String>,

    /// Tools to block (comma-separated or repeated flag).
    #[arg(long, value_delimiter = ',')]
    disallowed_tools: Vec<String>,

    /// System prompt override (replaces built-in system prompt)
    #[arg(long)]
    system_prompt: Option<String>,

    /// Append text to the system prompt
    #[arg(long)]
    append_system_prompt: Option<String>,

    /// Load additional MCP server configs (JSON: {"name":{"command":"...","args":[...]},...})
    #[arg(long, value_delimiter = ' ')]
    mcp_config: Vec<String>,

    /// Permission mode: default, auto, bypass
    #[arg(long, value_enum)]
    permission_mode: Option<PermissionMode>,

    /// Extended thinking mode: enabled, disabled, auto
    #[arg(long, value_enum)]
    thinking: Option<ThinkingMode>,

    /// Max thinking tokens (overrides settings.json thinkingBudgetTokens)
    #[arg(long)]
    max_thinking_tokens: Option<u32>,

    /// Extra directories to grant tool access to
    #[arg(long, value_delimiter = ',')]
    add_dir: Vec<String>,

    /// Effort level: low, medium, high, max
    #[arg(long)]
    effort: Option<String>,

    /// Beta headers to include in API requests (API key users only)
    #[arg(long, num_args = 1..)]
    betas: Vec<String>,

    /// Disable session persistence — sessions will not be saved to disk
    #[arg(long)]
    no_session_persistence: bool,

    /// Use a specific session ID for the conversation (must be a valid UUID)
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

    /// Minimal mode: skip hooks, CLAUDE.md discovery, and LSP
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

    /// Allow --dangerously-skip-permissions to be used without enabling it by default
    #[arg(long)]
    allow_dangerously_skip_permissions: bool,

    /// Maximum USD to spend on API calls (--print mode only)
    #[arg(long)]
    max_budget_usd: Option<f64>,

    /// Fallback model to use when primary model is overloaded (HTTP 529)
    #[arg(long)]
    fallback_model: Option<String>,

    /// Create a git worktree at startup (optional name)
    #[arg(long)]
    worktree: Option<Option<String>>,

    /// Create a tmux pane for the worktree (requires --worktree)
    #[arg(long)]
    tmux: bool,

    /// Handle a deep link URI (called by the OS when a registered URL scheme is activated)
    #[arg(long, value_name = "URI")]
    handle_uri: Option<String>,

    /// Register the deep link protocol handler (creates .desktop file, runs xdg-mime)
    #[arg(long)]
    register_protocol: bool,

    /// Custom agent definitions JSON
    #[arg(long)]
    agents: Option<String>,

    /// Disable all slash commands
    #[arg(long)]
    disable_slash_commands: bool,

    /// Comma-separated list of setting sources to load (user, project, local)
    #[arg(long, value_delimiter = ',')]
    setting_sources: Vec<String>,

    /// Tools to make available: "" = none, "default" = all, or specific names
    #[arg(long, value_delimiter = ',')]
    tools: Vec<String>,

    /// Prompt to send (used with --print)
    #[arg(trailing_var_arg = true)]
    prompt: Vec<String>,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
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
    /// Self-update to the latest release from GitHub
    Update,
    /// Run the autonomous browser agent
    Browse {
        /// Goal for the browser agent
        goal: Vec<String>,
        /// Skip all approval prompts (requires prior acknowledgment)
        #[arg(long)]
        yolo: bool,
        /// Prompt for approval on every destructive action
        #[arg(long)]
        ask: bool,
        /// Maximum number of steps (default: 50)
        #[arg(long, default_value = "50")]
        max_steps: u32,
    },
}

#[derive(Subcommand)]
enum McpSubcommand {
    /// List configured MCP servers
    List,
    /// Add an MCP server (stdio or HTTP)
    Add {
        /// Server name
        name: String,
        /// Command to run (for stdio transport)
        command: String,
        /// Arguments for the command
        args: Vec<String>,
        /// Configuration scope: user, project, or local (default: local)
        #[arg(short = 's', long, default_value = "local")]
        scope: String,
        /// Transport type: stdio (default) or http
        #[arg(short = 't', long, default_value = "stdio")]
        transport: String,
        /// Environment variables (KEY=VALUE)
        #[arg(short = 'e', long)]
        env: Vec<String>,
    },
    /// Add an MCP server from a JSON string
    AddJson {
        /// Server name
        name: String,
        /// JSON configuration string
        json: String,
        /// Configuration scope: user, project, or local (default: local)
        #[arg(short = 's', long, default_value = "local")]
        scope: String,
    },
    /// Import MCP servers from Claude Desktop configuration
    AddFromClaudeDesktop {
        /// Configuration scope: user, project, or local (default: local)
        #[arg(short = 's', long, default_value = "local")]
        scope: String,
    },
    /// Remove an MCP server
    Remove {
        /// Server name to remove
        name: String,
        /// Configuration scope (if omitted, removes from whichever scope it exists in)
        #[arg(short = 's', long)]
        scope: Option<String>,
    },
    /// Show details about an MCP server
    Get {
        /// Server name
        name: String,
    },
    /// Reset approved/rejected project-scoped (.mcp.json) server choices
    ResetProjectChoices,
}

/// Safe allowlist of env vars that rustyclaw may load from .env files.
///
/// Project `.env` files are **untrusted data** — a malicious repo could ship a
/// `.env` that sets `PATH`, `LD_PRELOAD`, or `RUSTYCLAW_*_COMMAND` to pivot
/// code execution the moment the user opens the folder. We therefore load only
/// a narrow allowlist of our own API-key and model vars, and specifically NEVER
/// load anything that could:
///   - Bypass permission prompts (`CLAUDE_DANGEROUSLY_SKIP_PERMISSIONS`)
///   - Redirect config / settings / hook resolution (`CLAUDE_CONFIG_DIR`,
///     `XDG_CONFIG_HOME`, `HOME`)
///   - Alter any process-spawn path (`PATH`, `LD_PRELOAD`, `LD_LIBRARY_PATH`,
///     `DYLD_*`, `RUSTYCLAW_*_COMMAND`, sandbox binaries, voice binaries,
///     MCP server argv)
///
/// If a user legitimately needs one of the blocked vars set, they can export
/// it in their shell — project `.env` is not the right place.
const SAFE_ENV_KEYS: &[&str] = &[
    // Anthropic
    "ANTHROPIC_API_KEY",
    "RUSTYCLAW_API_KEY_FILE_DESCRIPTOR",
    "ANTHROPIC_MODEL",
    // Verbose logging toggle — no exec side-effects
    "RUSTYCLAW_VERBOSE",
    // Ollama host — read-only redirect risk, but legitimate common use case
    "OLLAMA_HOST",
    // OpenAI-compat provider keys
    "OPENAI_API_KEY",
    "GROQ_API_KEY",
    "DEEPSEEK_API_KEY",
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
    "CLAUDE_DANGEROUSLY_SKIP_PERMISSIONS",
    "RUSTYCLAW_SANDBOX_COMMAND",
    "RUSTYCLAW_VOICE_COMMAND",
    "GEMINI_CLI_IDE_SERVER_STDIO_COMMAND",
];

/// Load KEY=VALUE pairs from a .env file into the process environment.
/// Only sets vars from SAFE_ENV_KEYS that are NOT already set.
/// Skips blank lines and lines starting with #.
fn load_dotenv(path: &std::path::Path) {
    let Ok(content) = std::fs::read_to_string(path) else {
        return;
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
            if !key.is_empty() && SAFE_ENV_KEYS.contains(&key) && std::env::var(key).is_err() {
                // SAFETY: single-threaded at this point — called before tokio runtime starts
                unsafe {
                    std::env::set_var(key, val);
                }
            }
        }
    }
}

/// Search common locations for .env files and load them in priority order.
/// Later sources do NOT override earlier ones (env already set always wins).
fn load_dotenv_auto() {
    // 1. CWD/.env  — project-local keys (highest priority, filtered to safe keys only)
    if let Ok(cwd) = std::env::current_dir() {
        let env_path = cwd.join(".env");
        if env_path.exists() {
            load_dotenv(&env_path);
            // Warn if project .env exists — it won't leak into tool subprocesses
            eprintln!(
                "Note: .env detected in project root. Only rustyclaw-specific keys \
                 (ANTHROPIC_API_KEY, OLLAMA_HOST, etc.) are loaded. \
                 Project vars are NOT injected into tool execution."
            );
        }
    }
    // 2. ~/.env  — user-global keys
    if let Some(home) = dirs::home_dir() {
        load_dotenv(&home.join(".env"));
        // 3. ~/.config/rustyclaw/.env  — app-specific config
        load_dotenv(&home.join(".config").join("rustyclaw").join(".env"));
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // Suppress broken-pipe errors — these happen when stdout is piped to `head`
    // or any consumer that exits early. Without this, Rust panics with
    // "failed to write ... Broken pipe" instead of exiting silently.
    #[cfg(unix)]
    {
        // SAFETY: single-threaded before tokio runtime; no signal handlers yet.
        unsafe {
            libc::signal(libc::SIGPIPE, libc::SIG_DFL);
        }
    }

    // Load .env files before anything else so API keys are available
    // to Config::load() and all downstream code.
    load_dotenv_auto();

    let cli = Cli::parse();

    // Initialize tracing — write to a log file in TUI mode so logs don't corrupt the screen
    let filter = if cli.verbose { "debug" } else { "warn" };
    let log_path = std::env::temp_dir().join("rustyclaw.log");
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .unwrap_or_else(|_| std::fs::File::create(&log_path).expect("Cannot create log file"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::sync::Mutex::new(log_file))
        .init();

    // --register-protocol: install the deep link handler and exit
    if cli.register_protocol {
        return deeplink::register_protocol();
    }

    // --handle-uri: parse and launch the TUI with the given prompt
    if let Some(ref uri) = cli.handle_uri {
        match deeplink::parse_deep_link(uri) {
            None => {
                eprintln!("Invalid or unrecognised deep link URI: {uri}");
                std::process::exit(1);
            }
            Some(params) => {
                let mut config = Config::load()?;
                if let Some(ref dir) = params.cwd {
                    let p = std::path::PathBuf::from(dir);
                    if p.is_dir() {
                        config.cwd = p;
                    }
                }
                // Run in --print mode with the extracted query so it works headlessly too
                config.verbose = false;
                let tools = all_tools(&config);
                let mut engine = QueryEngine::new(config, tools)?;
                engine.query(params.query).await?;
                return Ok(());
            }
        }
    }

    // Handle subcommands
    if let Some(cmd) = &cli.command {
        match cmd {
            Commands::Version => {
                println!("rustyclaw {VERSION}");
                return Ok(());
            }
            Commands::Mcp { subcommand } => {
                return handle_mcp_subcommand(subcommand).await;
            }
            Commands::Completions { shell } => {
                let mut cmd = <Cli as clap::CommandFactory>::command();
                clap_complete::generate(*shell, &mut cmd, "rustyclaw", &mut std::io::stdout());
                return Ok(());
            }
            Commands::Doctor => {
                println!("rustyclaw doctor — checking installation health…\n");
                // API key
                if let Ok(key) = std::env::var("ANTHROPIC_API_KEY") {
                    if key.len() >= 4 {
                        println!("  \u{2713} ANTHROPIC_API_KEY set ({}…)", &key[..4]);
                    }
                } else {
                    println!("  \u{2717} ANTHROPIC_API_KEY not set");
                }
                // Config dir
                let config_dir = config::Config::claude_dir();
                println!("  \u{2713} Config dir: {}", config_dir.display());
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
                println!("\n  rustyclaw v{VERSION}");
                return Ok(());
            }
            Commands::Update => {
                return self_update().await;
            }
            Commands::Browse { goal, yolo, ask, max_steps } => {
                use crate::browser::browse_loop::{BrowsePolicy, BrowseRequest, BrowseProgress, run_browse};
                use tokio::sync::mpsc;

                let goal_str = goal.join(" ");
                if goal_str.trim().is_empty() {
                    eprintln!("Error: browse requires a goal argument");
                    std::process::exit(1);
                }

                let config = Config::load()?;

                // Determine policy: --yolo > --ask > settings.browseDefaultPolicy > Pattern.
                let policy = if *yolo {
                    // First-time --yolo: write acknowledgment file if not yet present
                    if !crate::browser::yolo_ack::is_acknowledged() {
                        eprintln!(
                            "Warning: --yolo disables all approval prompts. \
                             The browser agent will execute destructive actions without confirmation.\n\
                             To proceed, this acknowledgment is recorded in your XDG state directory."
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
                    eprintln!("Error: ANTHROPIC_API_KEY not set for model: {}", config.model);
                    std::process::exit(1);
                }
                let (tools, shared_state) = crate::tools::all_tools_with_state(&config);
                let current_url = std::sync::Arc::new(tokio::sync::Mutex::new(String::new()));
                let browser_session = shared_state.browser_session.clone();

                let (progress_tx, mut progress_rx) = mpsc::channel::<BrowseProgress>(64);
                // Approval channel: in CLI mode auto-deny (user must use --yolo or --ask interactively)
                let (approval_tx, mut approval_rx) = mpsc::channel::<crate::browser::approval_gate::ApprovalPrompt>(8);

                // Spawn task to handle approval prompts: prompt on stderr, read from stdin
                let _approval_task = tokio::spawn(async move {
                    use std::io::Write;
                    while let Some(prompt) = approval_rx.recv().await {
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
                        let _ = prompt.reply.send(allowed);
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
                let channels = crate::browser::browse_loop::BrowseChannels { progress_tx, approval_tx, cancel };
                let result = run_browse(req, &config, tools, current_url, browser_session, channels).await?;
                progress_task.await.ok();

                // Print final result as JSON
                println!("{}", serde_json::to_string_pretty(&result)?);
                return Ok(());
            }
        }
    }

    // Load config
    let mut config = Config::load()?;

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
    if !cli.allowed_tools.is_empty() {
        config.allowed_tools = cli.allowed_tools.clone();
    }
    if !cli.disallowed_tools.is_empty() {
        config.disallowed_tools = cli.disallowed_tools.clone();
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
        config.bare_mode = true;
        config.disable_all_hooks = true;
        config.claudemd = String::new();
    }
    if cli.disable_slash_commands {
        config.disable_slash_commands = true;
    }
    if cli.allow_dangerously_skip_permissions {
        // Does not enable bypass by default — just allows it to be toggled
        // (stored for future permission prompt support)
    }
    if let Some(fb) = cli.fallback_model {
        config.fallback_model = Some(fb);
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
    if !cli.setting_sources.is_empty() {
        config.setting_sources = Some(cli.setting_sources.clone());
    }
    // --settings: load extra settings from a file path or JSON string
    if let Some(ref settings_arg) = cli.settings {
        let extra_settings: Option<crate::settings::Settings> =
            if std::path::Path::new(settings_arg).exists() {
                std::fs::read_to_string(settings_arg)
                    .ok()
                    .and_then(|s| serde_json::from_str(&s).ok())
            } else {
                serde_json::from_str(settings_arg).ok()
            };
        if let Some(extra) = extra_settings {
            // Merge: extra wins over already-loaded settings
            if let Some(m) = extra.model {
                config.model = crate::commands::resolve_model_alias(&m);
            }
            if let Some(mt) = extra.max_tokens {
                config.max_tokens = mt;
            }
            if let Some(ah) = extra.api_key_helper {
                config.api_key_helper = Some(ah);
            }
            for (k, v) in extra.mcp_servers {
                config.extra_mcp_servers.insert(k, v);
            }
        }
    }
    // --tools: override tool set ("" = none, "default" = all, or specific names)
    if !cli.tools.is_empty() {
        let raw = cli.tools.join(",");
        if raw.is_empty() {
            // "" = disable all tools
            config.allowed_tools = vec!["__none__".to_string()];
        } else if raw.eq_ignore_ascii_case("default") {
            // "default" = use all tools (already the default, clear any restrictions)
            config.allowed_tools.clear();
        } else {
            config.allowed_tools = cli.tools.clone();
        }
    }

    // Inline MCP configs from --mcp-config (each is a JSON object merged into settings)
    if !cli.mcp_config.is_empty() {
        for json_str in &cli.mcp_config {
            if let Ok(serde_json::Value::Object(map)) = serde_json::from_str(json_str) {
                for (name, val) in map {
                    if let Ok(cfg) =
                        serde_json::from_value::<crate::mcp::types::McpServerConfig>(val)
                    {
                        config.extra_mcp_servers.insert(name, cfg);
                    }
                }
            }
        }
    }

    // --headless mode: long-running SDK server
    if cli.headless {
        let transport = crate::sdk::transport::stdio::StdioTransport::new();
        crate::sdk::SdkServer::run(config, transport).await?;
        return Ok(());
    }

    // --print mode: non-interactive, no TUI
    if cli.print {
        // --input-format=stream-json: read prompt from JSON-line events on stdin
        let prompt = if config.input_format.as_deref() == Some("stream-json") {
            use std::io::BufRead;
            let mut result = String::new();
            let stdin = std::io::stdin();
            for line in stdin.lock().lines() {
                let line = line.unwrap_or_default();
                if line.is_empty() {
                    continue;
                }
                if let Ok(event) = serde_json::from_str::<serde_json::Value>(&line)
                    && event.get("type").and_then(|v| v.as_str()) == Some("user")
                    && let Some(text) = event
                        .get("message")
                        .and_then(|m| m.get("content"))
                        .and_then(|c| c.as_array())
                        .and_then(|arr| {
                            arr.iter()
                                .find(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
                        })
                        .and_then(|b| b.get("text"))
                        .and_then(|t| t.as_str())
                {
                    result = text.to_string();
                }
            }
            if result.is_empty() && !cli.prompt.is_empty() {
                cli.prompt.join(" ")
            } else {
                result
            }
        } else {
            if cli.prompt.is_empty() {
                eprintln!("Error: --print requires a prompt argument");
                std::process::exit(1);
            }
            cli.prompt.join(" ")
        };

        let mut tools = all_tools(&config);
        if !config.allowed_tools.is_empty() {
            // "__none__" sentinel means no tools allowed
            if config.allowed_tools.iter().any(|a| a == "__none__") {
                tools.clear();
            } else {
                tools.retain(|t| {
                    config
                        .allowed_tools
                        .iter()
                        .any(|a| a.eq_ignore_ascii_case(t.name()))
                });
            }
        }
        if !config.disallowed_tools.is_empty() {
            tools.retain(|t| {
                !config
                    .disallowed_tools
                    .iter()
                    .any(|d| d.eq_ignore_ascii_case(t.name()))
            });
        }

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
        engine.query(prompt).await?;
        return Ok(());
    }

    // Resolve resume session ID (--session-id takes priority as an explicit UUID)
    let resume_id = if let Some(id) = cli.session_id {
        Some(id)
    } else if let Some(id) = cli.session {
        Some(id)
    } else if cli.resume || cli.continue_session {
        // Find most recent session ID
        match session::Session::list().await {
            Ok(list) if !list.is_empty() => Some(list[0].id.clone()),
            _ => None,
        }
    } else {
        None
    };

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
    tui::run_tui(config, resume_id).await
}

/// Self-update: download the latest release from GitHub and replace the running binary.
async fn self_update() -> Result<()> {
    println!("Checking for updates…");

    // Map from Rust target triple to our release artifact name
    let target = self_update_target();
    println!("Platform: {target}");

    let status = self_update::backends::github::Update::configure()
        .repo_owner("ForkedInTime")
        .repo_name("RustyClaw")
        .bin_name("rustyclaw")
        .current_version(VERSION)
        .target(&target)
        .identifier(&target) // match artifact name pattern
        .show_download_progress(true)
        .no_confirm(false)
        .build()?
        .update()?;

    if status.updated() {
        println!("Updated to {}!", status.version());
    } else {
        println!("Already on latest version ({VERSION}).");
    }

    Ok(())
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

async fn handle_mcp_subcommand(subcommand: &Option<McpSubcommand>) -> Result<()> {
    let config = Config::load()?;
    match subcommand {
        None | Some(McpSubcommand::List) => {
            let settings = crate::settings::Settings::load(&config.cwd);
            if settings.mcp_servers.is_empty() {
                println!("No MCP servers configured.");
                println!("Add servers to ~/.claude/settings.json or .claude/settings.json:");
                println!("  {{");
                println!("    \"mcpServers\": {{");
                println!(
                    "      \"my-server\": {{\"command\": \"npx\", \"args\": [\"-y\", \"@my/mcp-server\"]}}"
                );
                println!("    }}");
                println!("  }}");
            } else {
                println!("Configured MCP servers ({}):", settings.mcp_servers.len());
                for (name, cfg) in &settings.mcp_servers {
                    let kind = match cfg {
                        crate::mcp::types::McpServerConfig::Stdio(s) => {
                            format!("stdio: {}", s.command)
                        }
                        crate::mcp::types::McpServerConfig::Http(h) => format!("http: {}", h.url),
                    };
                    println!("  {name}  ({kind})");
                }
            }
        }
        Some(McpSubcommand::Get { name }) => {
            let settings = crate::settings::Settings::load(&config.cwd);
            match settings.mcp_servers.get(name) {
                None => {
                    eprintln!("Server '{name}' not found.");
                    std::process::exit(1);
                }
                Some(cfg) => {
                    println!("MCP server: {name}");
                    match cfg {
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
                }
            }
        }
        Some(McpSubcommand::Add {
            name,
            command,
            args,
            scope,
            transport: _,
            env,
        }) => {
            let env_map: std::collections::HashMap<String, String> = env
                .iter()
                .filter_map(|kv| {
                    let mut parts = kv.splitn(2, '=');
                    let k = parts.next()?.to_string();
                    let v = parts.next()?.to_string();
                    Some((k, v))
                })
                .collect();
            let cfg =
                crate::mcp::types::McpServerConfig::Stdio(crate::mcp::types::StdioServerConfig {
                    command: command.clone(),
                    args: args.clone(),
                    env: env_map,
                });
            mcp_write_server(name, cfg, scope, &config)?;
            println!("Added MCP server '{name}' (scope: {scope})");
        }
        Some(McpSubcommand::AddJson { name, json, scope }) => {
            let cfg: crate::mcp::types::McpServerConfig =
                serde_json::from_str(json).map_err(|e| anyhow::anyhow!("Invalid JSON: {e}"))?;
            mcp_write_server(name, cfg, scope, &config)?;
            println!("Added MCP server '{name}' (scope: {scope})");
        }
        Some(McpSubcommand::AddFromClaudeDesktop { scope }) => {
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
                    for (name, val) in &servers {
                        match serde_json::from_value::<crate::mcp::types::McpServerConfig>(
                            val.clone(),
                        ) {
                            Ok(cfg) => {
                                mcp_write_server(name, cfg, scope, &config)?;
                                println!("  Imported: {name}");
                                imported += 1;
                            }
                            Err(e) => {
                                eprintln!("  Skipped '{name}': {e}");
                            }
                        }
                    }
                    println!("Imported {imported} server(s) from Claude Desktop.");
                }
            }
        }
        Some(McpSubcommand::Remove { name, scope }) => {
            let removed = mcp_remove_server(name, scope.as_deref(), &config)?;
            if removed {
                println!("Removed MCP server '{name}'.");
            } else {
                eprintln!("Server '{name}' not found in any settings file.");
                std::process::exit(1);
            }
        }
        Some(McpSubcommand::ResetProjectChoices) => {
            // Reset approvedMcpjsonServers / rejectedMcpjsonServers in project settings
            let project_path = config.cwd.join(".claude").join("settings.json");
            if project_path.exists() {
                let content = std::fs::read_to_string(&project_path)?;
                let mut json: serde_json::Value =
                    serde_json::from_str(&content).unwrap_or(serde_json::json!({}));
                if let Some(m) = json.as_object_mut() {
                    m.remove("approvedMcpjsonServers");
                    m.remove("rejectedMcpjsonServers");
                }
                std::fs::write(&project_path, serde_json::to_string_pretty(&json)?)?;
                println!("Reset project MCP choices in {}", project_path.display());
            } else {
                println!("No project settings file found.");
            }
        }
    }
    Ok(())
}

/// Write an MCP server config to the appropriate settings file for the given scope.
fn mcp_write_server(
    name: &str,
    cfg: crate::mcp::types::McpServerConfig,
    scope: &str,
    config: &Config,
) -> Result<()> {
    let path = mcp_scope_path(scope, config);
    let content = if path.exists() {
        std::fs::read_to_string(&path)?
    } else {
        "{}".to_string()
    };
    let mut json: serde_json::Value =
        serde_json::from_str(&content).unwrap_or(serde_json::json!({}));
    if json.get("mcpServers").is_none() {
        json["mcpServers"] = serde_json::json!({});
    }
    json["mcpServers"][name] = serde_json::to_value(&cfg)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, serde_json::to_string_pretty(&json)?)?;
    Ok(())
}

/// Remove an MCP server from the specified scope (or all scopes if scope is None).
fn mcp_remove_server(name: &str, scope: Option<&str>, config: &Config) -> Result<bool> {
    let paths: Vec<std::path::PathBuf> = if let Some(s) = scope {
        vec![mcp_scope_path(s, config)]
    } else {
        vec![
            mcp_scope_path("user", config),
            mcp_scope_path("project", config),
            mcp_scope_path("local", config),
        ]
    };
    let mut removed = false;
    for path in &paths {
        if !path.exists() {
            continue;
        }
        let content = std::fs::read_to_string(path)?;
        let mut json: serde_json::Value =
            serde_json::from_str(&content).unwrap_or(serde_json::json!({}));
        if let Some(servers) = json.get_mut("mcpServers").and_then(|v| v.as_object_mut())
            && servers.remove(name).is_some()
        {
            std::fs::write(path, serde_json::to_string_pretty(&json)?)?;
            removed = true;
        }
    }
    Ok(removed)
}

/// Resolve the settings file path for an mcp scope.
fn mcp_scope_path(scope: &str, config: &Config) -> std::path::PathBuf {
    match scope {
        "user" => Config::claude_dir().join("settings.json"),
        "project" => config.cwd.join(".claude").join("settings.json"),
        _ => config.cwd.join(".mcp.json"), // "local" scope = .mcp.json
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
mod dotenv_allowlist_tests {
    use super::{FORBIDDEN_ENV_KEYS, SAFE_ENV_KEYS, load_dotenv};
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

    /// A malicious .env that sets dangerous vars must not leak into the
    /// process environment when we run `load_dotenv` against it.
    ///
    /// This is a live, end-to-end test of the loader against a real file.
    /// We use `RUSTYCLAW_VERBOSE` as the "safe var loaded" probe rather than
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
        let path = std::env::temp_dir().join(format!("rustyclaw-dotenv-test-{suffix}.env"));
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "PATH=/evil/bin").unwrap();
        writeln!(f, "LD_PRELOAD=/evil/libhack.so").unwrap();
        writeln!(f, "CLAUDE_DANGEROUSLY_SKIP_PERMISSIONS=1").unwrap();
        writeln!(f, "CLAUDE_CONFIG_DIR=/tmp/attacker/config").unwrap();
        writeln!(f, "XDG_CONFIG_HOME=/tmp/attacker/xdg").unwrap();
        writeln!(f, "RUSTYCLAW_SANDBOX_COMMAND=/evil/bwrap").unwrap();
        writeln!(f, "RUSTYCLAW_VERBOSE=1").unwrap();
        drop(f);

        // Snapshot-restore anything we might touch so we don't perturb the
        // test harness's own environment.
        let snap_ck = std::env::var("CLAUDE_DANGEROUSLY_SKIP_PERMISSIONS").ok();
        let snap_cd = std::env::var("CLAUDE_CONFIG_DIR").ok();
        let snap_xdg = std::env::var("XDG_CONFIG_HOME").ok();
        let snap_sbox = std::env::var("RUSTYCLAW_SANDBOX_COMMAND").ok();
        let snap_verb = std::env::var("RUSTYCLAW_VERBOSE").ok();
        let original_path = std::env::var("PATH").unwrap_or_default();

        unsafe {
            std::env::remove_var("CLAUDE_DANGEROUSLY_SKIP_PERMISSIONS");
            std::env::remove_var("CLAUDE_CONFIG_DIR");
            std::env::remove_var("XDG_CONFIG_HOME");
            std::env::remove_var("RUSTYCLAW_SANDBOX_COMMAND");
            std::env::remove_var("RUSTYCLAW_VERBOSE");
        }

        load_dotenv(&path);

        // Safe var loaded.
        assert_eq!(
            std::env::var("RUSTYCLAW_VERBOSE").ok().as_deref(),
            Some("1"),
            "safe var RUSTYCLAW_VERBOSE should have been loaded"
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
            std::env::var("RUSTYCLAW_SANDBOX_COMMAND").is_err(),
            "RUSTYCLAW_SANDBOX_COMMAND must NEVER be loaded from .env"
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
            std::env::remove_var("RUSTYCLAW_VERBOSE");
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
                std::env::set_var("RUSTYCLAW_SANDBOX_COMMAND", v);
            }
            if let Some(v) = snap_verb {
                std::env::set_var("RUSTYCLAW_VERBOSE", v);
            }
        }
    }
}
