/// Tool trait and registry — port of Tool.ts and tools.ts
pub mod agent;
pub mod ask_user;
pub mod bash;
pub mod brief_tool;
pub mod browser_tools;
pub mod config_tool;
pub mod cron;
pub mod discover_skills;
pub mod file_edit;
pub mod file_read;
pub mod file_write;
pub mod glob;
pub mod grep;
pub mod lsp;
pub mod mcp_resources;
pub mod memory;
pub mod multi_edit;
pub mod notebook;
pub mod plan_mode;
pub mod powershell;
pub mod send_message;
pub mod skill_tool;
pub mod sleep;
pub mod tasks;
pub mod team_tools;
pub mod todo;
pub mod tool_search;
pub mod web_browser;
pub mod web_fetch;
pub mod web_search;
pub mod workflow;
pub mod worktree;

use crate::api::types::{ToolDefinition, ToolResultContent};
use anyhow::Result;
pub use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::oneshot;

/// Cache of (path → content-hash) populated by the Read tool so it can
/// emit a compact "unchanged since last read" notice instead of re-sending
/// the full file body on repeat reads. v2.1.86 token-overhead fix.
pub type ReadCache = Arc<Mutex<HashMap<std::path::PathBuf, u64>>>;

pub fn new_read_cache() -> ReadCache {
    Arc::new(Mutex::new(HashMap::new()))
}

/// Permission mode for the session — mirrors PermissionMode in types/permissions.ts
#[derive(Debug, Clone, PartialEq)]
#[allow(dead_code)] // variants wired to CLI flags, not all exposed yet
pub enum PermissionMode {
    Default,
    AutoEdit,
    BypassPermissions,
}

/// Minimal tool execution context — mirrors ToolUseContext in Tool.ts
#[derive(Clone)]
pub struct ToolContext {
    pub cwd: std::path::PathBuf,
    pub permission_mode: PermissionMode,
    pub verbose: bool,
    /// Optional channel for streaming live output lines to the TUI during tool execution.
    pub stream_tx: Option<tokio::sync::mpsc::UnboundedSender<String>>,
    /// Optional channel for AskUserQuestion tool to request user input.
    /// Tuple: (question_text, reply_sender).
    pub ask_user_tx: Option<tokio::sync::mpsc::UnboundedSender<(String, oneshot::Sender<String>)>>,
    /// Optional channel for plan mode tools to toggle plan mode.
    pub plan_mode_tx: Option<tokio::sync::mpsc::UnboundedSender<bool>>,
    /// Default shell for the Bash tool ("bash", "powershell", etc.).
    /// None = use $SHELL env var or "bash" as fallback.
    pub default_shell: Option<String>,

    /// If set, Write/Edit tools snapshot the original file here before modifying it.
    /// Set to `~/.claude/sessions/<sid>/snapshots/turn-<n>/` by the run loop.
    pub snapshot_dir: Option<std::path::PathBuf>,

    /// Active sandbox mode for the Bash tool ("strict", "bwrap", "firejail").
    /// None means sandbox is disabled.
    pub sandbox_mode: Option<String>,

    /// Whether bwrap sandbox allows outbound network (passed to bwrap_wrap).
    pub sandbox_allow_network: bool,

    /// Shared Read-tool cache: path → content hash. Lets the Read tool skip
    /// re-emitting a file body that hasn't changed since the last read.
    pub read_cache: Option<ReadCache>,

    /// Live provider snapshot taken at turn start. Tools that spawn a new
    /// `QueryEngine` (AgentTool, background spawn) must read these instead
    /// of their own cached `config` snapshot, which goes stale the moment
    /// the user runs `/model foo` mid-session. When `None`, tools fall back
    /// to their internal snapshot.
    pub live_model: Option<String>,
    pub live_api_key: Option<String>,
    pub live_ollama_host: Option<String>,

    /// Middleware chain: pre/post hooks around every tool call.
    /// Default empty = no-op (existing behavior unchanged).
    pub middlewares: crate::browser::middleware::MiddlewareChain,
}

impl std::fmt::Debug for ToolContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolContext")
            .field("cwd", &self.cwd)
            .field("permission_mode", &self.permission_mode)
            .field("verbose", &self.verbose)
            .finish()
    }
}

impl ToolContext {
    pub fn new(cwd: std::path::PathBuf) -> Self {
        Self {
            cwd,
            permission_mode: PermissionMode::Default,
            verbose: false,
            stream_tx: None,
            ask_user_tx: None,
            plan_mode_tx: None,
            default_shell: None,
            snapshot_dir: None,
            sandbox_mode: None,
            sandbox_allow_network: true,
            read_cache: None,
            live_model: None,
            live_api_key: None,
            live_ollama_host: None,
            middlewares: Vec::new(),
        }
    }

    /// Send a live output line to the TUI (if a stream channel is set).
    pub fn stream_line(&self, line: &str) {
        if let Some(tx) = &self.stream_tx {
            let _ = tx.send(line.to_string());
        }
    }
}

/// Snapshot a file to the ctx.snapshot_dir before it is modified.
/// The snapshot preserves the file at its current state so /rewind can restore it.
/// Silently skips if snapshot_dir is None or the file doesn't exist yet (new file).
pub async fn snapshot_file(ctx: &ToolContext, path: &std::path::Path) {
    let Some(ref snap_dir) = ctx.snapshot_dir else {
        return;
    };
    if !path.exists() {
        return;
    } // new file — nothing to snapshot

    // Compute a relative-ish path for the snapshot filename
    // e.g. /home/user/project/src/main.rs → "home_user_project_src_main.rs"
    let flat_name = path
        .display()
        .to_string()
        .replace('/', "_")
        .trim_start_matches('_')
        .to_string();

    if tokio::fs::create_dir_all(snap_dir).await.is_err() {
        return;
    }
    let dest = snap_dir.join(&flat_name);
    let _ = tokio::fs::copy(path, &dest).await;
}

/// Result of a tool execution
#[derive(Debug)]
pub struct ToolOutput {
    pub content: Vec<ToolResultContent>,
    pub is_error: bool,
}

impl ToolOutput {
    pub fn success(text: impl Into<String>) -> Self {
        Self {
            content: vec![ToolResultContent::text(text)],
            is_error: false,
        }
    }

    pub fn error(text: impl Into<String>) -> Self {
        Self {
            content: vec![ToolResultContent::text(text)],
            is_error: true,
        }
    }
}

/// Directories that Write/Edit tools must never modify.
/// Protected directories that Write/Edit tools must never modify.
pub const PROTECTED_DIRS: &[&str] = &[".git", ".husky"];

/// Returns Some(error ToolOutput) if `path` is inside a protected directory.
pub fn check_protected_path(path: &std::path::Path) -> Option<ToolOutput> {
    for &protected in PROTECTED_DIRS {
        if path.components().any(|c| c.as_os_str() == protected) {
            return Some(ToolOutput::error(format!(
                "Modifying files inside '{}' directories is not allowed.",
                protected
            )));
        }
    }
    None
}

/// Which tool operation is asking the sensitive-path guard.
/// `Read` is permissive: only private-key material is blocked so normal
/// project work can still read `.env.example` and inspect config.
/// `Write` is strict: blocks every class of secret to stop accidental
/// overwrites of `.ssh/`, `.aws/credentials`, `.env`, etc.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SensitiveOp {
    Read,
    Write,
}

/// File names / suffixes that represent private-key material.
/// Blocked for BOTH read and write.
const PRIVATE_KEY_NAMES: &[&str] = &["id_rsa", "id_ed25519", "id_ecdsa", "id_dsa", "identity"];
const PRIVATE_KEY_SUFFIXES: &[&str] = &[".pem", ".key", ".p12", ".pfx", ".jks", ".keystore"];

/// File names that are secret-material for write-only.
/// These are commonly read by tooling (e.g. `.env` for config inspection)
/// but must never be clobbered by the agent.
const SECRET_WRITE_BLOCK_NAMES: &[&str] = &[
    "credentials",
    "credentials.json",
    ".netrc",
    ".pgpass",
    "kubeconfig",
];

/// Directory components that signal a secrets tree.
/// Any path containing one of these components is blocked on write.
const SECRET_DIR_COMPONENTS: &[&str] = &[".ssh", ".aws", ".gnupg", ".kube"];

/// Does `file_name` look like a dotenv variant (`.env`, `.env.local`,
/// `.env.production`, etc.) — but NOT `.env.example` / `.env.sample`
/// which are safe templates?
fn is_dotenv(file_name: &str) -> bool {
    if !file_name.starts_with(".env") {
        return false;
    }
    let rest = &file_name[4..];
    if rest.is_empty()
        || rest == ".local"
        || rest == ".production"
        || rest == ".development"
        || rest == ".test"
        || rest == ".staging"
    {
        return true;
    }
    // Skip known-safe templates.
    if rest == ".example" || rest == ".sample" || rest == ".template" || rest == ".dist" {
        return false;
    }
    // Other `.env.*` variants — treat as sensitive by default.
    rest.starts_with('.')
}

/// Returns Some(error ToolOutput) if the path should be blocked for `op`.
/// Read mode blocks private keys only. Write mode additionally blocks
/// dotenv files, credential stores, and secrets directories.
pub fn check_sensitive_path(path: &std::path::Path, op: SensitiveOp) -> Option<ToolOutput> {
    let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");

    // Private-key material — blocked in both modes.
    if PRIVATE_KEY_NAMES.contains(&file_name) {
        return Some(ToolOutput::error(format!(
            "Refusing to {} private-key file '{}'. This path is on the hard deny-list.",
            match op {
                SensitiveOp::Read => "read",
                SensitiveOp::Write => "modify",
            },
            file_name
        )));
    }
    if PRIVATE_KEY_SUFFIXES
        .iter()
        .any(|s| file_name.to_ascii_lowercase().ends_with(s))
    {
        return Some(ToolOutput::error(format!(
            "Refusing to {} key-material file '{}'. This path is on the hard deny-list.",
            match op {
                SensitiveOp::Read => "read",
                SensitiveOp::Write => "modify",
            },
            file_name
        )));
    }

    // For read operations, allow everything else (agent can legitimately
    // inspect `.env`, `credentials`, etc. with user permission prompts).
    if op == SensitiveOp::Read {
        return None;
    }

    // Write-only blocks below.

    // Dotenv files — never let the agent overwrite these.
    if is_dotenv(file_name) {
        return Some(ToolOutput::error(format!(
            "Refusing to modify dotenv file '{}'. The agent must not overwrite environment secrets.",
            file_name
        )));
    }

    // Well-known credential files.
    if SECRET_WRITE_BLOCK_NAMES.contains(&file_name) {
        return Some(ToolOutput::error(format!(
            "Refusing to modify credential file '{}'. This path is on the hard deny-list.",
            file_name
        )));
    }

    // Paths inside secrets directories anywhere in the ancestor chain.
    for comp in path.components() {
        let s = comp.as_os_str();
        if SECRET_DIR_COMPONENTS.iter().any(|d| s == *d) {
            return Some(ToolOutput::error(format!(
                "Refusing to modify files inside '{}'. This directory is on the hard deny-list.",
                s.to_string_lossy()
            )));
        }
    }

    None
}

/// The core Tool trait — mirrors the Tool interface in Tool.ts
#[async_trait]
pub trait Tool: Send + Sync {
    /// Unique tool name sent to the API
    fn name(&self) -> &str;

    /// Human-readable description sent to the API
    fn description(&self) -> &str;

    /// JSON Schema for the tool's input parameters
    fn input_schema(&self) -> serde_json::Value;

    /// Execute the tool with the given JSON input
    async fn execute(&self, input: serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput>;

    /// Build the ToolDefinition to include in API requests
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: self.description().to_string(),
            input_schema: self.input_schema(),
            cache_control: None,
        }
    }
}

pub type DynTool = Arc<dyn Tool>;

/// Build the default tool set.
pub fn default_tools() -> Vec<DynTool> {
    vec![
        Arc::new(bash::BashTool),
        Arc::new(file_read::FileReadTool),
        Arc::new(file_write::FileWriteTool),
        Arc::new(file_edit::FileEditTool),
        Arc::new(multi_edit::MultiEditTool),
        Arc::new(glob::GlobTool),
        Arc::new(grep::GrepTool),
        Arc::new(web_fetch::WebFetchTool),
    ]
}

/// All shared state needed by tools that must also be readable by slash commands.
pub struct SharedToolState {
    pub todo: todo::TodoState,
    /// Shared across the 8 browser tools and the /browser slash commands.
    /// None when config.browser_enabled == false.
    pub browser_session:
        Option<std::sync::Arc<tokio::sync::Mutex<crate::browser::BrowserSession>>>,
}

/// Build the full tool set. Returns tools + shared state so slash commands can read it.
pub fn all_tools_with_state(config: &crate::config::Config) -> (Vec<DynTool>, SharedToolState) {
    let mut tools = default_tools();

    tools.push(Arc::new(web_search::WebSearchTool {
        api_key: config.api_key.clone(),
        model: config.model.clone(),
    }));
    tools.push(Arc::new(agent::AgentTool {
        config: config.clone(),
    }));

    // Task management tools (shared registry)
    let registry = tasks::new_registry();
    tools.push(Arc::new(tasks::TaskCreateTool {
        registry: registry.clone(),
    }));
    tools.push(Arc::new(tasks::TaskGetTool {
        registry: registry.clone(),
    }));
    tools.push(Arc::new(tasks::TaskListTool {
        registry: registry.clone(),
    }));
    tools.push(Arc::new(tasks::TaskUpdateTool {
        registry: registry.clone(),
    }));
    tools.push(Arc::new(tasks::TaskStopTool {
        registry: registry.clone(),
    }));
    tools.push(Arc::new(tasks::TaskOutputTool {
        registry: registry.clone(),
    }));

    // Worktree tools (shared state)
    let wt_state = worktree::WorktreeState::default();
    tools.push(Arc::new(worktree::EnterWorktreeTool {
        state: wt_state.clone(),
    }));
    tools.push(Arc::new(worktree::ExitWorktreeTool { state: wt_state }));

    // TodoWrite — shared state readable by /tasks command
    let todo_state = todo::new_todo_state();
    tools.push(Arc::new(todo::TodoWriteTool {
        state: todo_state.clone(),
    }));

    // Interactive / meta tools
    tools.push(Arc::new(ask_user::AskUserQuestionTool));
    tools.push(Arc::new(plan_mode::EnterPlanModeTool));
    tools.push(Arc::new(plan_mode::ExitPlanModeTool));
    tools.push(Arc::new(memory::MemoryReadTool));
    tools.push(Arc::new(memory::MemoryWriteTool));

    // BriefTool — always-on in Rust (no build-time KAIROS flag system)
    tools.push(Arc::new(brief_tool::BriefTool));

    // Agent swarm tools — enabled when RUSTYCLAW_EXPERIMENTAL_AGENT_TEAMS=1
    if send_message::is_agent_swarms_enabled() {
        tools.push(Arc::new(send_message::SendMessageTool));
        tools.push(Arc::new(team_tools::TeamCreateTool));
        tools.push(Arc::new(team_tools::TeamDeleteTool));
    }

    // Simple utilities
    tools.push(Arc::new(sleep::SleepTool));
    tools.push(Arc::new(powershell::PowerShellTool));
    tools.push(Arc::new(web_browser::WebBrowserTool));

    // Browser automation tools (shared session across all browser_* tools AND /browser commands).
    let browser_session_shared = if config.browser_enabled {
        let browser_session = std::sync::Arc::new(tokio::sync::Mutex::new(
            crate::browser::BrowserSession::default(),
        ));
        tools.push(Arc::new(browser_tools::BrowserNavigateTool {
            session: browser_session.clone(),
            headless: config.browser_headless,
            chrome_path: config.browser_chrome_path.clone(),
            cdp_endpoint: config.browser_cdp_endpoint.clone(),
            timeout_ms: config.browser_timeout_ms,
        }));
        tools.push(Arc::new(browser_tools::BrowserSnapshotTool {
            session: browser_session.clone(),
        }));
        tools.push(Arc::new(browser_tools::BrowserClickTool {
            session: browser_session.clone(),
        }));
        tools.push(Arc::new(browser_tools::BrowserFillTool {
            session: browser_session.clone(),
        }));
        tools.push(Arc::new(browser_tools::BrowserScreenshotTool {
            session: browser_session.clone(),
        }));
        tools.push(Arc::new(browser_tools::BrowserGetTextTool {
            session: browser_session.clone(),
        }));
        tools.push(Arc::new(browser_tools::BrowserPressKeyTool {
            session: browser_session.clone(),
        }));
        tools.push(Arc::new(browser_tools::BrowserWaitTool {
            session: browser_session.clone(),
            default_timeout_ms: config.browser_timeout_ms,
        }));
        Some(browser_session)
    } else {
        None
    };

    tools.push(Arc::new(lsp::LSPTool));
    tools.push(Arc::new(discover_skills::DiscoverSkillsTool));
    tools.push(Arc::new(skill_tool::SkillTool));
    tools.push(Arc::new(workflow::WorkflowTool));

    // Notebook tools
    tools.push(Arc::new(notebook::NotebookReadTool));
    tools.push(Arc::new(notebook::NotebookEditTool));

    // Config tool
    tools.push(Arc::new(config_tool::ConfigTool {
        config: config.clone(),
    }));

    // Cron tools (shared store)
    let cron_store: cron::CronStore = Arc::new(std::sync::Mutex::new(cron::load_jobs()));
    tools.push(Arc::new(cron::CronCreateTool {
        store: cron_store.clone(),
    }));
    tools.push(Arc::new(cron::CronDeleteTool {
        store: cron_store.clone(),
    }));
    tools.push(Arc::new(cron::CronListTool { store: cron_store }));

    // ToolSearch — built last so it can include all tool names+descriptions
    let snapshot: Vec<(String, String)> = tools
        .iter()
        .map(|t| (t.name().to_string(), t.description().to_string()))
        .collect();
    tools.push(Arc::new(tool_search::ToolSearchTool {
        tools_snapshot: snapshot,
    }));

    let shared = SharedToolState {
        todo: todo_state,
        browser_session: browser_session_shared,
    };
    (tools, shared)
}

/// Convenience wrapper for callers that don't need the shared state.
pub fn all_tools(config: &crate::config::Config) -> Vec<DynTool> {
    all_tools_with_state(config).0
}

/// Build tools + shared state, then append MCP dynamic tools and resource tools.
pub fn all_tools_with_state_and_mcp(
    config: &crate::config::Config,
    mcp_tools: Vec<DynTool>,
    mcp_clients: Vec<Arc<crate::mcp::client::McpClient>>,
) -> (Vec<DynTool>, SharedToolState) {
    let (mut tools, shared) = all_tools_with_state(config);
    tools.extend(mcp_tools);

    // MCP resource tools (need the client list)
    if !mcp_clients.is_empty() {
        tools.push(Arc::new(mcp_resources::ListMcpResourcesTool {
            clients: mcp_clients.clone(),
        }));
        tools.push(Arc::new(mcp_resources::ReadMcpResourceTool {
            clients: mcp_clients,
        }));
    }

    (tools, shared)
}

#[cfg(test)]
mod sensitive_path_tests {
    use super::*;
    use std::path::PathBuf;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn write_blocks_dotenv() {
        assert!(check_sensitive_path(&p("/proj/.env"), SensitiveOp::Write).is_some());
        assert!(check_sensitive_path(&p("/proj/.env.local"), SensitiveOp::Write).is_some());
        assert!(check_sensitive_path(&p("/proj/.env.production"), SensitiveOp::Write).is_some());
    }

    #[test]
    fn write_allows_dotenv_templates() {
        assert!(check_sensitive_path(&p("/proj/.env.example"), SensitiveOp::Write).is_none());
        assert!(check_sensitive_path(&p("/proj/.env.sample"), SensitiveOp::Write).is_none());
        assert!(check_sensitive_path(&p("/proj/.env.template"), SensitiveOp::Write).is_none());
    }

    #[test]
    fn write_blocks_ssh_dir() {
        assert!(
            check_sensitive_path(&p("/home/u/.ssh/authorized_keys"), SensitiveOp::Write).is_some()
        );
        assert!(check_sensitive_path(&p("/home/u/.ssh/config"), SensitiveOp::Write).is_some());
    }

    #[test]
    fn write_blocks_aws_creds() {
        assert!(check_sensitive_path(&p("/home/u/.aws/credentials"), SensitiveOp::Write).is_some());
        assert!(check_sensitive_path(&p("/home/u/.aws/config"), SensitiveOp::Write).is_some());
    }

    #[test]
    fn write_blocks_credential_files() {
        assert!(check_sensitive_path(&p("/proj/credentials.json"), SensitiveOp::Write).is_some());
        assert!(check_sensitive_path(&p("/home/u/.netrc"), SensitiveOp::Write).is_some());
        assert!(check_sensitive_path(&p("/home/u/.pgpass"), SensitiveOp::Write).is_some());
    }

    #[test]
    fn read_and_write_block_private_keys() {
        for op in [SensitiveOp::Read, SensitiveOp::Write] {
            assert!(check_sensitive_path(&p("/home/u/.ssh/id_rsa"), op).is_some());
            assert!(check_sensitive_path(&p("/proj/server.pem"), op).is_some());
            assert!(check_sensitive_path(&p("/proj/keystore.jks"), op).is_some());
            assert!(check_sensitive_path(&p("/proj/tls.key"), op).is_some());
        }
    }

    #[test]
    fn read_allows_dotenv_and_credentials() {
        // Read is permissive — user can still inspect secrets via prompts.
        assert!(check_sensitive_path(&p("/proj/.env"), SensitiveOp::Read).is_none());
        assert!(check_sensitive_path(&p("/proj/credentials.json"), SensitiveOp::Read).is_none());
        assert!(check_sensitive_path(&p("/home/u/.aws/config"), SensitiveOp::Read).is_none());
    }

    #[test]
    fn normal_files_pass() {
        assert!(check_sensitive_path(&p("/proj/src/main.rs"), SensitiveOp::Read).is_none());
        assert!(check_sensitive_path(&p("/proj/src/main.rs"), SensitiveOp::Write).is_none());
        assert!(check_sensitive_path(&p("/proj/README.md"), SensitiveOp::Write).is_none());
    }

    // ── Glob-semantics regression tests (Sprint #2 HIGH) ────────────────────
    //
    // The deny-list is implemented with literal equality (`PRIVATE_KEY_NAMES`,
    // `SECRET_WRITE_BLOCK_NAMES`, `SECRET_DIR_COMPONENTS`) and case-insensitive
    // `ends_with` (`PRIVATE_KEY_SUFFIXES`). It does NOT interpret glob
    // metacharacters. A known bug in another tool in this space was adding
    // entries like `"*.pem"` thinking they'd be globbed, which then silently
    // failed to match anything because literal-equality saw the asterisk as
    // part of the filename. These tests lock in the invariant that every
    // entry is a plain literal and that each one actually matches a realistic
    // path.

    /// Static-assertion: no deny-list entry contains glob metacharacters.
    /// If this test fails, someone added a pattern assuming globs are
    /// supported — the match is literal, so the "rule" is a no-op.
    #[test]
    fn no_deny_list_entry_contains_glob_metacharacters() {
        const META: &[char] = &['*', '?', '[', ']', '{', '}'];
        let all_entries: Vec<(&str, &str)> = PRIVATE_KEY_NAMES
            .iter()
            .map(|e| ("PRIVATE_KEY_NAMES", *e))
            .chain(
                PRIVATE_KEY_SUFFIXES
                    .iter()
                    .map(|e| ("PRIVATE_KEY_SUFFIXES", *e)),
            )
            .chain(
                SECRET_WRITE_BLOCK_NAMES
                    .iter()
                    .map(|e| ("SECRET_WRITE_BLOCK_NAMES", *e)),
            )
            .chain(
                SECRET_DIR_COMPONENTS
                    .iter()
                    .map(|e| ("SECRET_DIR_COMPONENTS", *e)),
            )
            .collect();

        for (list, entry) in all_entries {
            for ch in META {
                assert!(
                    !entry.contains(*ch),
                    "{list}: entry {entry:?} contains glob metacharacter {ch:?} — \
                     the deny-list uses literal equality, not glob matching. \
                     Either drop the glob chars or convert the check to a \
                     real glob matcher."
                );
            }
        }
    }

    /// Coverage: every literal entry in the deny-lists actually blocks a
    /// representative path that a well-meaning engineer would expect it to.
    /// If someone silently adds an entry that never fires, this catches it.
    #[test]
    fn every_deny_list_entry_blocks_a_representative_path() {
        // PRIVATE_KEY_NAMES — blocked in both modes.
        for name in PRIVATE_KEY_NAMES {
            let path = p(&format!("/home/u/.ssh/{name}"));
            assert!(
                check_sensitive_path(&path, SensitiveOp::Read).is_some(),
                "PRIVATE_KEY_NAMES entry {name:?} did not block a read of {path:?}"
            );
            assert!(
                check_sensitive_path(&path, SensitiveOp::Write).is_some(),
                "PRIVATE_KEY_NAMES entry {name:?} did not block a write of {path:?}"
            );
        }

        // PRIVATE_KEY_SUFFIXES — blocked in both modes via ends_with.
        for suffix in PRIVATE_KEY_SUFFIXES {
            let path = p(&format!("/proj/server{suffix}"));
            assert!(
                check_sensitive_path(&path, SensitiveOp::Read).is_some(),
                "PRIVATE_KEY_SUFFIXES entry {suffix:?} did not block {path:?}"
            );
            assert!(
                check_sensitive_path(&path, SensitiveOp::Write).is_some(),
                "PRIVATE_KEY_SUFFIXES entry {suffix:?} did not block {path:?}"
            );
        }

        // SECRET_WRITE_BLOCK_NAMES — write only.
        for name in SECRET_WRITE_BLOCK_NAMES {
            let path = p(&format!("/proj/{name}"));
            assert!(
                check_sensitive_path(&path, SensitiveOp::Write).is_some(),
                "SECRET_WRITE_BLOCK_NAMES entry {name:?} did not block a write of {path:?}"
            );
        }

        // SECRET_DIR_COMPONENTS — any file beneath should be write-blocked.
        for dir in SECRET_DIR_COMPONENTS {
            let path = p(&format!("/home/u/{dir}/some_file.txt"));
            assert!(
                check_sensitive_path(&path, SensitiveOp::Write).is_some(),
                "SECRET_DIR_COMPONENTS entry {dir:?} did not block a write of {path:?}"
            );
        }
    }

    /// Case-insensitivity on the suffix side: `FOO.PEM` / `server.Key` /
    /// mixed-case suffix variants must still match PRIVATE_KEY_SUFFIXES
    /// because the check lowercases the filename before comparing.
    #[test]
    fn private_key_suffixes_match_case_insensitively() {
        for case in &["SERVER.PEM", "tls.KEY", "Secrets.P12", "foo.JKS"] {
            let path = p(&format!("/proj/{case}"));
            assert!(
                check_sensitive_path(&path, SensitiveOp::Write).is_some(),
                "uppercase/mixed-case suffix {case:?} must still match the deny-list"
            );
            assert!(
                check_sensitive_path(&path, SensitiveOp::Read).is_some(),
                "uppercase/mixed-case suffix {case:?} must still block reads of private-key material"
            );
        }
    }

    /// A file nested arbitrarily deep inside a secret directory must still
    /// be blocked — the check walks every path component, not just the
    /// immediate parent.
    #[test]
    fn secret_dir_blocks_nested_children() {
        assert!(
            check_sensitive_path(
                &p("/home/u/.aws/sub/dir/deep/creds.toml"),
                SensitiveOp::Write
            )
            .is_some(),
            "deeply nested file under .aws/ must be write-blocked"
        );
        assert!(
            check_sensitive_path(
                &p("/srv/secrets/.gnupg/private-keys-v1.d/ABCDEF.key"),
                SensitiveOp::Write
            )
            .is_some(),
            "nested file under .gnupg/ must be write-blocked"
        );
    }

    /// A path using `..` to traverse INTO a secret directory must still be
    /// blocked. We do not canonicalize (symlink traversal is a separate
    /// threat model), but the component walk should still see the `.ssh`
    /// component in the path.
    #[test]
    fn secret_dir_blocked_through_traversal_component() {
        assert!(
            check_sensitive_path(
                &p("/proj/../home/u/.ssh/authorized_keys"),
                SensitiveOp::Write
            )
            .is_some(),
            ".ssh component in a traversal path must still trip the deny-list"
        );
    }

    /// A literal filename that happens to CONTAIN a deny-list entry as a
    /// substring must NOT be blocked. For example, `my_id_rsa_notes.md` is
    /// not `id_rsa`. This locks in that PRIVATE_KEY_NAMES uses equality,
    /// not substring matching.
    #[test]
    fn private_key_names_require_exact_filename_match() {
        // These are NOT id_rsa — they merely contain the substring.
        assert!(
            check_sensitive_path(&p("/proj/my_id_rsa_notes.md"), SensitiveOp::Read).is_none(),
            "substring match must not block unrelated files"
        );
        assert!(
            check_sensitive_path(&p("/proj/not_id_ed25519.txt"), SensitiveOp::Write).is_none(),
            "substring match must not block unrelated files"
        );
    }
}
