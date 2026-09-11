//! `/` command handlers — split out of `commands/mod.rs` mechanically.

use super::*;

pub(super) fn cmd_upgrade() -> CommandAction {
    CommandAction::CheckUpgrade
}

pub(super) fn cmd_plugin(args: &str) -> CommandAction {
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

pub(super) fn plugin_marketplace_list() -> CommandAction {
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

pub(super) fn plugin_set_enabled(name: &str, enabled: bool) -> CommandAction {
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
                    "Plugin '{}' {}. Restart oxideclaw to apply.",
                    name,
                    if enabled { "enabled" } else { "disabled" }
                )),
                None => CommandAction::Message("Failed to write settings.json".into()),
            }
        }
    }
}

pub(super) fn plugin_validate(path: &str) -> CommandAction {
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

pub(super) fn plugin_manage_list() -> CommandAction {
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

pub(super) fn cmd_powerup(args: &str) -> CommandAction {
    let lessons: &[(&str, &str, &str)] = &[
        (
            "lesson 1 — navigating the TUI",
            "Basic navigation",
            "Welcome to oxideclaw! Here are the essentials:\n\
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
            "Slash commands control oxideclaw's behaviour:\n\
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
             - oxideclaw -r      — resume most recent on launch\n\
             - /export           — save as markdown\n\
             \n\
             Type /powerup 5 for the next lesson.",
        ),
        (
            "lesson 5 — MCP servers",
            "MCP (Model Context Protocol) servers",
            "MCP servers extend oxideclaw with additional tools:\n\
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
             - oxideclaw mcp list   — from the terminal\n\
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
             You have completed the oxideclaw power-up course!\n\
             Type /help any time for the full command reference.",
        ),
    ];

    let num: usize = args.trim().parse().unwrap_or(1);
    let idx = (num.saturating_sub(1)).min(lessons.len() - 1);
    let (_, title, content) = lessons[idx];

    let header = format!(
        "**oxideclaw /powerup — {} of {}** — {}\n\n",
        idx + 1,
        lessons.len(),
        title
    );

    CommandAction::Message(format!("{header}{content}"))
}
