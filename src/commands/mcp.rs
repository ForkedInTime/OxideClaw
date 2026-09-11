//! `/` command handlers — split out of `commands/mod.rs` mechanically.

use super::*;

pub(super) fn cmd_mcp(args: &str, ctx: &CommandContext) -> CommandAction {
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
                    Restart oxideclaw after adding servers."
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
            "MCP reconnection requires restarting oxideclaw.\n\
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
pub(super) fn mcp_add_server(args: &str) -> CommandAction {
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
        Ok(s) => match crate::config::write_json_atomic(&settings_path, &s) {
            Ok(_) => CommandAction::Message(format!(
                "MCP server '{}' added to {}\nRestart oxideclaw to connect.",
                name,
                settings_path.display()
            )),
            Err(e) => CommandAction::Message(format!("Failed to write settings: {e}")),
        },
        Err(e) => CommandAction::Message(format!("Serialization error: {e}")),
    }
}

/// Remove an MCP server from ~/.claude/settings.json.
pub(super) fn mcp_remove_server(args: &str) -> CommandAction {
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
        Ok(s) => match crate::config::write_json_atomic(&settings_path, &s) {
            Ok(_) => CommandAction::Message(format!(
                "MCP server '{}' removed from settings.json\nRestart oxideclaw to disconnect.",
                name
            )),
            Err(e) => CommandAction::Message(format!("Failed to write settings: {e}")),
        },
        Err(e) => CommandAction::Message(format!("Serialization error: {e}")),
    }
}

/// Enable or disable an MCP server in settings.json via a `disabled` flag.
pub(super) fn mcp_set_disabled(args: &str, disabled: bool) -> CommandAction {
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
                        "MCP server '{}' {}. Restart oxideclaw to apply.",
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
