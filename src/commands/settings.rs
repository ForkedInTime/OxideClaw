//! `/` command handlers — split out of `commands/mod.rs` mechanically.

use super::*;

pub(super) fn cmd_config(ctx: &CommandContext) -> CommandAction {
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

pub(super) fn cmd_index(args: &str) -> CommandAction {
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

pub(super) fn cmd_rag(args: &str) -> CommandAction {
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

pub(super) fn cmd_budget(args: &str) -> CommandAction {
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

pub(super) fn cmd_router(args: &str) -> CommandAction {
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

pub(super) fn cmd_permissions(ctx: &CommandContext) -> CommandAction {
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

pub(super) fn cmd_autonomy(args: &str) -> CommandAction {
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

/// Dispatch the `/memory` command with subcommands for the persistent MemoryStore.
pub(super) fn cmd_memory_dispatch(args: &str, ctx: &CommandContext) -> CommandAction {
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

pub(super) fn cmd_hooks(ctx: &CommandContext) -> CommandAction {
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

pub(super) fn cmd_ide(ctx: &CommandContext) -> CommandAction {
    // Show IDE integration information
    let _ = ctx;
    CommandAction::Message(concat!(
        "IDE Integration\n\n",
        "oxideclaw runs as a standalone TUI — no IDE plugin required.\n\n",
        "For VS Code integration:\n",
        "  1. Run oxideclaw in the VS Code integrated terminal\n",
        "  2. Full tool access — Bash, Read, Write, Edit, Grep, etc.\n\n",
        "For JetBrains IDEs:\n",
        "  1. Run oxideclaw in the built-in terminal\n",
        "  2. Or run oxideclaw in the built-in terminal\n\n",
        "The LSP tool provides code intelligence directly in the chat:\n",
        "  Use the LSP tool to query language servers for definitions, references, hover docs, etc."
    ).into())
}

pub(super) fn cmd_advisor(args: &str) -> CommandAction {
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

pub(super) fn cmd_voice(args: &str, ctx: &CommandContext) -> CommandAction {
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

pub(super) fn cmd_sandbox(args: &str, ctx: &CommandContext) -> CommandAction {
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
