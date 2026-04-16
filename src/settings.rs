/// Settings — load and merge ~/.claude/settings.json and ./.claude/settings.json.
///
/// Priority (lowest → highest):
///   compiled defaults
///   → ~/.claude/settings.json  (global)
///   → <cwd>/.claude/settings.json  (project)
///   → environment variables
///   → CLI flags
///
/// All fields are Option so a missing key means "inherit from lower priority."
use crate::mcp::types::McpServerConfig;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

// ── Hook configuration ────────────────────────────────────────────────────────

/// A single hook entry: runs `command` when the tool name matches `matcher`.
/// If matcher is empty or "*", the hook runs for all tools.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookEntry {
    /// Tool name to match (empty or "*" = all tools)
    #[serde(default)]
    pub matcher: String,
    /// Shell command to execute (passed to sh -c)
    pub command: String,
}

impl HookEntry {
    pub fn matches(&self, tool_name: &str) -> bool {
        self.matcher.is_empty() || self.matcher == "*" || self.matcher == tool_name
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct HooksConfig {
    /// Before a tool executes. Env: TOOL_NAME, TOOL_INPUT. Exit 2 = block.
    #[serde(rename = "preToolUse", default)]
    pub pre_tool_use: Vec<HookEntry>,

    /// After a tool completes. Env: TOOL_NAME, TOOL_RESULT.
    #[serde(rename = "postToolUse", default)]
    pub post_tool_use: Vec<HookEntry>,

    /// When the user submits a message. Env: CLAUDE_MESSAGE.
    #[serde(rename = "userPromptSubmit", default)]
    pub user_prompt_submit: Vec<HookEntry>,

    /// When Claude sends a text notification/response. Env: CLAUDE_MESSAGE.
    #[serde(rename = "notification", default)]
    pub notification: Vec<HookEntry>,

    /// When the session ends (exit or /exit). Env: CLAUDE_SESSION_ID.
    #[serde(rename = "stop", default)]
    pub stop: Vec<HookEntry>,

    /// When a session begins. Env: CLAUDE_SESSION_ID.
    #[serde(rename = "sessionStart", default)]
    pub session_start: Vec<HookEntry>,

    /// Before a compact/summarize cycle.
    #[serde(rename = "preCompact", default)]
    pub pre_compact: Vec<HookEntry>,

    /// After a compact/summarize cycle completes.
    #[serde(rename = "postCompact", default)]
    pub post_compact: Vec<HookEntry>,
}

// ── Settings struct ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    /// Model ID override (prefix "ollama:" for local Ollama models)
    pub model: Option<String>,

    /// Max tokens per response (global fallback)
    pub max_tokens: Option<u32>,

    /// Per-model max_tokens overrides. Keys may be alias or canonical
    /// model IDs; values are the max output token budget for that model.
    /// Example:
    /// ```json
    /// "maxTokensByModel": { "haiku": 4096, "opus": 16000 }
    /// ```
    #[serde(rename = "maxTokensByModel")]
    pub max_tokens_by_model: Option<std::collections::HashMap<String, u32>>,

    /// Enable auto-compact when context fills
    pub auto_compact: Option<bool>,

    /// Verbose/debug output
    pub verbose: Option<bool>,

    /// Ollama server base URL (default: http://localhost:11434)
    pub ollama_host: Option<String>,

    /// Extended thinking budget in tokens (enables interleaved thinking).
    /// Example: 10000
    pub thinking_budget_tokens: Option<u32>,

    /// Show Claude's thinking blocks in the chat UI (default: false).
    /// Set to true in settings.json to display thinking summaries.
    #[serde(rename = "showThinkingSummaries")]
    pub show_thinking_summaries: Option<bool>,

    /// Enable Anthropic prompt caching (saves costs on repeated context).
    pub prompt_cache: Option<bool>,

    /// Hooks run before/after tool calls.
    pub hooks: Option<HooksConfig>,

    /// Permission rules
    #[serde(default)]
    pub permissions: PermissionsConfig,

    /// Effort level: low/medium/high/max (influences thinking budget and compactness).
    pub effort: Option<String>,

    /// Environment variables to inject into tool calls (e.g. for Bash tool).
    /// Example: { "MY_VAR": "value" }
    #[serde(default)]
    pub env: HashMap<String, String>,

    /// MCP server definitions — keyed by server name.
    ///
    /// Example (settings.json):
    /// ```json
    /// "mcpServers": {
    ///   "github": { "command": "npx", "args": ["-y", "@modelcontextprotocol/server-github"], "env": {"GITHUB_TOKEN": "..."} },
    ///   "remote": { "url": "http://localhost:3000/mcp" }
    /// }
    /// ```
    #[serde(rename = "mcpServers", default)]
    pub mcp_servers: HashMap<String, McpServerConfig>,

    /// Path to a shell script/command that prints an Anthropic API key on stdout.
    /// Used for secrets-manager integrations. Result is cached for 5 minutes.
    /// Example: "apiKeyHelper": "aws secretsmanager get-secret-value --query SecretString --output text --secret-id my-api-key"
    pub api_key_helper: Option<String>,

    /// Disable all hooks and statusLine execution globally.
    #[serde(rename = "disableAllHooks")]
    pub disable_all_hooks: Option<bool>,

    /// Auto-delete sessions older than this many days (0 = never).
    #[serde(rename = "cleanupPeriodDays")]
    pub cleanup_period_days: Option<u32>,

    /// Default shell for the Bash tool. "bash" (default) or "powershell".
    #[serde(rename = "defaultShell")]
    pub default_shell: Option<String>,

    /// Whether to add a Co-Authored-By trailer to git commits and PRs.
    /// Defaults to true.
    #[serde(rename = "includeCoAuthoredBy")]
    pub include_co_authored_by: Option<bool>,

    /// Active output style name ("default", "Explanatory", "Learning", or custom).
    #[serde(rename = "outputStyle")]
    pub output_style: Option<String>,

    /// Active theme ("dark", "light", "solarized").
    pub theme: Option<String>,

    /// Whether sandbox mode is enabled for Bash tool execution.
    #[serde(rename = "sandboxEnabled")]
    pub sandbox_enabled: Option<bool>,

    /// Active sandbox mode ("strict", "bwrap", "firejail").
    #[serde(rename = "sandboxMode")]
    pub sandbox_mode: Option<String>,

    /// Whether voice input mode is enabled.
    #[serde(rename = "voiceEnabled")]
    pub voice_enabled: Option<bool>,

    /// Custom Whisper API URL (optional, defaults to OpenAI endpoint).
    #[serde(rename = "voiceApiUrl")]
    pub voice_api_url: Option<String>,

    /// Whether TTS (text-to-speech) output is enabled.
    #[serde(rename = "ttsEnabled")]
    pub tts_enabled: Option<bool>,

    /// Path to the TTS voice model file or clone sample.
    #[serde(rename = "ttsVoiceModel")]
    pub tts_voice_model: Option<String>,

    /// Whether desktop notifications + terminal bell fire on task completion.
    #[serde(rename = "notificationsEnabled")]
    pub notifications_enabled: Option<bool>,

    /// Spinner style: "themed" (default), "minimal", or "silent".
    /// "themed" = fun verbs (gaming/medicine/cycling), "minimal" = just "Working…", "silent" = no spinner text.
    #[serde(rename = "spinnerStyle")]
    pub spinner_style: Option<String>,

    /// Whether bwrap sandbox allows outbound network (default true).
    #[serde(rename = "sandboxAllowNetwork")]
    pub sandbox_allow_network: Option<bool>,

    /// When true, skills cannot execute shell commands (Bash tool blocked during skill turns).
    #[serde(rename = "disableSkillShellExecution")]
    pub disable_skill_shell_execution: Option<bool>,

    /// Enable smart model router — auto-routes tasks by complexity to different models.
    #[serde(rename = "routerEnabled")]
    pub router_enabled: Option<bool>,

    /// Session budget limit in USD (router cost tracking).
    #[serde(rename = "routerBudget")]
    pub router_budget: Option<f64>,

    /// Model for low-complexity tasks (default: claude-haiku-4-5-20251001).
    #[serde(rename = "routerLowModel")]
    pub router_low_model: Option<String>,

    /// Model for medium-complexity tasks (default: claude-sonnet-4-6-20250514).
    #[serde(rename = "routerMediumModel")]
    pub router_medium_model: Option<String>,

    /// Model for high-complexity tasks (default: user's configured model).
    #[serde(rename = "routerHighModel")]
    pub router_high_model: Option<String>,

    /// Model for super-high-complexity tasks needing 1M context (default: claude-opus-4-6).
    #[serde(rename = "routerSuperHighModel")]
    pub router_super_high_model: Option<String>,

    /// Autonomy level for file modifications: "suggest", "auto-edit", "full-auto".
    /// - "suggest": show diff preview + ask before applying any Write/Edit
    /// - "auto-edit": auto-apply edits to existing files, ask for new files (default)
    /// - "full-auto": apply all changes without asking
    pub autonomy: Option<String>,

    /// Auto-capture notable decisions/preferences from assistant responses into persistent memory.
    #[serde(rename = "memoryAutoCapture")]
    pub memory_auto_capture: Option<bool>,

    /// Phase-declarative model routing — route research/plan/edit/review to different models.
    #[serde(rename = "phaseRouter")]
    pub phase_router: Option<PhaseRouterSettings>,

    /// Auto-fix loop: run lint + tests after Write/Edit and re-prompt on failure.
    /// The JSON key `autoFixLoop` is preferred; `autoRollback` remains as a
    /// silent alias so existing user configs keep working.
    #[serde(rename = "autoFixLoop", alias = "autoRollback")]
    pub auto_fix: Option<AutoFixSettings>,

    /// Auto-commit loop: per-turn working-tree snapshots on private shadow refs
    /// (`refs/rustyclaw/sessions/<id>`) navigable via `/undo` and `/redo`.
    #[serde(rename = "autoCommit")]
    pub auto_commit: Option<AutoCommitSettings>,

    #[serde(rename = "browseMaxSteps")]
    pub browse_max_steps: Option<u32>,

    #[serde(rename = "browseApprovalPatterns")]
    pub browse_approval_patterns: Option<Vec<String>>,

    #[serde(rename = "browseDefaultPolicy")]
    pub browse_default_policy: Option<String>,
}

/// Settings for phase-declarative model routing.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PhaseRouterSettings {
    /// Whether phase routing is active (default: false).
    pub enabled: Option<bool>,
    /// Map of phase name → model ID. Keys: "research", "plan", "edit", "review", "default".
    pub phases: Option<std::collections::HashMap<String, String>>,
}

/// Settings for the auto-fix loop (lint + tests + retry feedback).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct AutoFixSettings {
    /// Enable the auto-fix loop (default: true).
    pub enabled: Option<bool>,
    /// When to run: `"autonomous"` (default), `"always"`, `"off"`. Parsed case-insensitively.
    pub trigger: Option<String>,
    /// Lint command override; `null` → auto-detect from project files.
    pub lint_command: Option<String>,
    /// Test command override; `null` → auto-detect from project files.
    pub test_command: Option<String>,
    /// Max consecutive retries before giving up (reserved for future multi-turn loop).
    pub max_retries: Option<u32>,
    /// Max wall-clock seconds the test command may run before being killed.
    /// Defaults to 60 when unset. Set to 0 for no timeout.
    pub timeout_secs: Option<u64>,
}

// ── Auto-commit runtime config ────────────────────────────────────────────────

/// Default number of session shadow-ref sets to retain on startup prune.
pub const DEFAULT_KEEP_SESSIONS: u32 = 10;
/// Default subject prefix for auto-commit messages.
pub const DEFAULT_MESSAGE_PREFIX: &str = "rustyclaw";

/// Runtime config for the auto-commit loop. Built from `AutoCommitSettings`
/// in `Config::load` with out-of-range `keep_sessions` clamped to the default.
#[derive(Debug, Clone)]
pub struct AutoCommitConfig {
    pub enabled: bool,
    pub keep_sessions: u32,
    pub message_prefix: String,
}

impl Default for AutoCommitConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            keep_sessions: DEFAULT_KEEP_SESSIONS,
            message_prefix: DEFAULT_MESSAGE_PREFIX.to_string(),
        }
    }
}

/// Settings for the auto-commit loop (per-turn shadow-ref snapshots + /undo).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct AutoCommitSettings {
    /// Enable the auto-commit loop (default: true).
    pub enabled: Option<bool>,
    /// How many session refs to keep on startup prune (default: 10, 0 = unlimited).
    pub keep_sessions: Option<u32>,
    /// Commit subject prefix (default: "rustyclaw").
    pub message_prefix: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PermissionsConfig {
    /// Tools to always allow without asking (e.g. ["Bash", "Edit"])
    #[serde(default)]
    pub allow: Vec<String>,

    /// Tools to always deny without asking
    #[serde(default)]
    pub deny: Vec<String>,
}

impl Settings {
    /// Load and merge global + project settings + .mcp.json.
    /// Priority: global → project → .mcp.json (for MCP servers only).
    pub fn load(cwd: &Path) -> Self {
        let global_path = crate::config::Config::claude_dir().join("settings.json");
        let project_path = cwd.join(".claude").join("settings.json");
        let mcp_json_path = cwd.join(".mcp.json");

        let global = Self::from_file(&global_path);
        let project = Self::from_file(&project_path);
        let merged = global.merge(project);

        // Auto-load .mcp.json from project root — merges its mcpServers on top
        if mcp_json_path.exists() {
            let mcp_extra = Self::load_mcp_json(&mcp_json_path);
            merged.merge(mcp_extra)
        } else {
            merged
        }
    }

    /// Load only the mcpServers block from a .mcp.json file.
    /// Returns a Settings with only mcp_servers populated.
    fn load_mcp_json(path: &Path) -> Self {
        let mut s = Self::default();
        if let Ok(content) = std::fs::read_to_string(path)
            && let Ok(json) = serde_json::from_str::<serde_json::Value>(&content)
            && let Some(obj) = json.get("mcpServers").and_then(|v| v.as_object())
        {
            for (name, val) in obj {
                if let Ok(cfg) = serde_json::from_value::<McpServerConfig>(val.clone()) {
                    s.mcp_servers.insert(name.clone(), cfg);
                }
            }
        }
        s
    }

    /// Try to read and parse a settings file; silently return defaults on any error.
    ///
    /// Security hardening: `apiKeyHelper` is stripped from the parsed settings
    /// if the source file is world- or group-writable, since the helper is
    /// executed via `sh -c` and a writable settings file is an obvious
    /// shell-injection vector on shared hosts. The warning is emitted via
    /// `tracing::warn` so it shows up in the log file without corrupting TUI.
    fn from_file(path: &Path) -> Self {
        if !path.exists() {
            return Self::default();
        }
        let parsed: Self = match std::fs::read_to_string(path) {
            Err(_) => return Self::default(),
            Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
        };
        Self::sanitize_unsafe_helper(parsed, path)
    }

    /// If `parsed.api_key_helper` is Some and the source file has unsafe
    /// permissions (world- or group-writable on unix), strip the helper and
    /// warn. Windows has no POSIX permission bits so this is a no-op there;
    /// the threat model (multi-user shared settings) is primarily a unix
    /// concern anyway.
    fn sanitize_unsafe_helper(mut parsed: Self, path: &Path) -> Self {
        if parsed.api_key_helper.is_none() {
            return parsed;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if let Ok(md) = std::fs::metadata(path) {
                let mode = md.mode() & 0o777;
                if mode & 0o022 != 0 {
                    tracing::warn!(
                        "ignoring apiKeyHelper from {}: file is world/group-writable (mode {:o}); \
                         refusing to execute it as a shell command. Run `chmod 600 {}` to enable.",
                        path.display(),
                        mode,
                        path.display()
                    );
                    parsed.api_key_helper = None;
                }
            }
        }
        let _ = path; // silence unused on non-unix
        parsed
    }

    /// Merge `other` on top of `self` — `other` wins for any Some field.
    fn merge(self, other: Self) -> Self {
        // MCP servers: project entries override global entries of the same name;
        // entries that only exist in global are preserved.
        let mut mcp_servers = self.mcp_servers;
        for (name, cfg) in other.mcp_servers {
            mcp_servers.insert(name, cfg);
        }

        // Env: merge both maps (project wins on same key)
        let mut env = self.env;
        for (k, v) in other.env {
            env.insert(k, v);
        }

        Self {
            model: other.model.or(self.model),
            max_tokens: other.max_tokens.or(self.max_tokens),
            max_tokens_by_model: match (self.max_tokens_by_model, other.max_tokens_by_model) {
                (Some(mut a), Some(b)) => {
                    for (k, v) in b {
                        a.insert(k, v);
                    }
                    Some(a)
                }
                (a, b) => b.or(a),
            },
            auto_compact: other.auto_compact.or(self.auto_compact),
            verbose: other.verbose.or(self.verbose),
            ollama_host: other.ollama_host.or(self.ollama_host),
            thinking_budget_tokens: other.thinking_budget_tokens.or(self.thinking_budget_tokens),
            show_thinking_summaries: other
                .show_thinking_summaries
                .or(self.show_thinking_summaries),
            prompt_cache: other.prompt_cache.or(self.prompt_cache),
            hooks: other.hooks.or(self.hooks),
            effort: other.effort.or(self.effort),
            env,
            mcp_servers,
            api_key_helper: other.api_key_helper.or(self.api_key_helper),
            disable_all_hooks: other.disable_all_hooks.or(self.disable_all_hooks),
            cleanup_period_days: other.cleanup_period_days.or(self.cleanup_period_days),
            default_shell: other.default_shell.or(self.default_shell),
            include_co_authored_by: other.include_co_authored_by.or(self.include_co_authored_by),
            output_style: other.output_style.or(self.output_style),
            theme: other.theme.or(self.theme),
            sandbox_enabled: other.sandbox_enabled.or(self.sandbox_enabled),
            sandbox_mode: other.sandbox_mode.or(self.sandbox_mode),
            voice_enabled: other.voice_enabled.or(self.voice_enabled),
            voice_api_url: other.voice_api_url.or(self.voice_api_url),
            tts_enabled: other.tts_enabled.or(self.tts_enabled),
            tts_voice_model: other.tts_voice_model.or(self.tts_voice_model),
            notifications_enabled: other.notifications_enabled.or(self.notifications_enabled),
            spinner_style: other.spinner_style.or(self.spinner_style),
            sandbox_allow_network: other.sandbox_allow_network.or(self.sandbox_allow_network),
            disable_skill_shell_execution: other
                .disable_skill_shell_execution
                .or(self.disable_skill_shell_execution),
            router_enabled: other.router_enabled.or(self.router_enabled),
            router_budget: other.router_budget.or(self.router_budget),
            router_low_model: other.router_low_model.or(self.router_low_model),
            router_medium_model: other.router_medium_model.or(self.router_medium_model),
            router_high_model: other.router_high_model.or(self.router_high_model),
            router_super_high_model: other
                .router_super_high_model
                .or(self.router_super_high_model),
            autonomy: other.autonomy.or(self.autonomy),
            memory_auto_capture: other.memory_auto_capture.or(self.memory_auto_capture),
            phase_router: other.phase_router.or(self.phase_router),
            auto_fix: other.auto_fix.or(self.auto_fix),
            auto_commit: other.auto_commit.or(self.auto_commit),
            browse_max_steps: other.browse_max_steps.or(self.browse_max_steps),
            browse_approval_patterns: other
                .browse_approval_patterns
                .or(self.browse_approval_patterns),
            browse_default_policy: other.browse_default_policy.or(self.browse_default_policy),
            permissions: PermissionsConfig {
                // Union both lists — project additions stack on top of global
                allow: {
                    let mut v = self.permissions.allow;
                    for item in other.permissions.allow {
                        if !v.contains(&item) {
                            v.push(item);
                        }
                    }
                    v
                },
                deny: {
                    let mut v = self.permissions.deny;
                    for item in other.permissions.deny {
                        if !v.contains(&item) {
                            v.push(item);
                        }
                    }
                    v
                },
            },
        }
    }

    /// Return the path(s) that were actually loaded, for diagnostics.
    pub fn loaded_paths(cwd: &Path) -> Vec<String> {
        let mut paths = Vec::new();
        let global = crate::config::Config::claude_dir().join("settings.json");
        let project = cwd.join(".claude").join("settings.json");
        let mcp_json = cwd.join(".mcp.json");
        if global.exists() {
            paths.push(global.display().to_string());
        }
        if project.exists() {
            paths.push(project.display().to_string());
        }
        if mcp_json.exists() {
            paths.push(mcp_json.display().to_string());
        }
        paths
    }
}

#[cfg(test)]
mod auto_fix_key_tests {
    use super::*;

    #[test]
    fn parses_new_autofixloop_key() {
        let json = r#"{
            "autoFixLoop": {
                "enabled": true,
                "trigger": "always",
                "lintCommand": "cargo clippy",
                "testCommand": "cargo test",
                "maxRetries": 5,
                "timeoutSecs": 30
            }
        }"#;
        let s: Settings = serde_json::from_str(json).unwrap();
        let af = s.auto_fix.expect("auto_fix should deserialise");
        assert_eq!(af.enabled, Some(true));
        assert_eq!(af.trigger.as_deref(), Some("always"));
        assert_eq!(af.lint_command.as_deref(), Some("cargo clippy"));
        assert_eq!(af.max_retries, Some(5));
    }

    #[test]
    fn parses_legacy_autorollback_key() {
        let json = r#"{
            "autoRollback": {
                "enabled": true,
                "testCommand": "pytest"
            }
        }"#;
        let s: Settings = serde_json::from_str(json).unwrap();
        let af = s
            .auto_fix
            .expect("legacy autoRollback should deserialise into auto_fix");
        assert_eq!(af.test_command.as_deref(), Some("pytest"));
    }

    #[test]
    fn serialises_with_new_key() {
        let s = Settings {
            auto_fix: Some(AutoFixSettings {
                enabled: Some(true),
                trigger: None,
                lint_command: None,
                test_command: Some("cargo test".to_string()),
                max_retries: Some(3),
                timeout_secs: None,
            }),
            ..Default::default()
        };
        let out = serde_json::to_string(&s).unwrap();
        assert!(
            out.contains("autoFixLoop"),
            "serialised form should use the new key: {out}"
        );
        assert!(
            !out.contains("autoRollback"),
            "serialised form should not use the legacy key: {out}"
        );
    }
}

#[cfg(test)]
mod auto_commit_key_tests {
    use super::*;

    #[test]
    fn parses_full_autocommit_block() {
        let json = r#"{
            "autoCommit": {
                "enabled": false,
                "keepSessions": 25,
                "messagePrefix": "claw"
            }
        }"#;
        let s: Settings = serde_json::from_str(json).unwrap();
        let ac = s.auto_commit.expect("auto_commit should deserialise");
        assert_eq!(ac.enabled, Some(false));
        assert_eq!(ac.keep_sessions, Some(25));
        assert_eq!(ac.message_prefix.as_deref(), Some("claw"));
    }

    #[test]
    fn defaults_when_autocommit_missing() {
        let s: Settings = serde_json::from_str("{}").unwrap();
        assert!(s.auto_commit.is_none());
    }

    #[test]
    fn serialises_with_camelcase_key() {
        let s = Settings {
            auto_commit: Some(AutoCommitSettings {
                enabled: Some(true),
                keep_sessions: Some(10),
                message_prefix: Some("rustyclaw".to_string()),
            }),
            ..Default::default()
        };
        let out = serde_json::to_string(&s).unwrap();
        assert!(out.contains("autoCommit"), "expected autoCommit key: {out}");
        assert!(
            out.contains("keepSessions"),
            "expected keepSessions key: {out}"
        );
    }
}
