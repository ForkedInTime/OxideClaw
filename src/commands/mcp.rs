//! `/` command handlers — split out of `commands/mod.rs` mechanically.

use super::*;

pub(super) fn cmd_mcp(args: &str, ctx: &CommandContext) -> CommandAction {
    cmd_mcp_in(args, ctx, &Config::config_dir())
}

fn cmd_mcp_in(args: &str, ctx: &CommandContext, config_dir: &std::path::Path) -> CommandAction {
    let sub = args.trim();
    let (subcmd, rest) = split_first_word(sub);
    let cwd = &ctx.config.cwd;

    match subcmd {
        "" | "list" | "status" => {
            let configured = crate::mcp::scope::list(cwd, config_dir);
            let waiting: Vec<&str> = configured
                .iter()
                .filter(|s| s.needs_trust && s.overridden_by.is_none())
                .map(|s| s.name.as_str())
                .collect();
            let trust_note = (!waiting.is_empty()).then(|| {
                format!(
                    "Not started until you /trust this project (.mcp.json): {}",
                    waiting.join(", ")
                )
            });
            let failed_note =
                (!ctx.mcp_failed.is_empty()).then(|| crate::mcp::failed_notice(ctx.mcp_failed));
            if ctx.mcp_statuses.is_empty() {
                // The add hint is for a first server, not one that broke.
                if let Some(note) = failed_note {
                    let mut text = format!("No MCP servers connected.\n\n{note}");
                    if let Some(trust) = trust_note {
                        text.push_str("\n\n");
                        text.push_str(&trust);
                    }
                    return CommandAction::Message(text);
                }
                let mut text = String::from(
                    "No MCP servers connected.\n\n\
                    Add one: /mcp add <name> <command> [args...]\n\
                    Or for HTTP:  /mcp add <name> <url>\n\n\
                    Example: /mcp add github npx -y @modelcontextprotocol/server-github\n\
                    It is private to you and this project; /mcp add --scope user for every \
                    project. Restart oxideclaw after adding servers.",
                );
                if let Some(note) = trust_note {
                    text.push_str("\n\n");
                    text.push_str(&note);
                }
                return CommandAction::Message(text);
            }
            let total_tools: usize = ctx.mcp_statuses.iter().map(|s| s.tool_count).sum();
            let mut lines = vec![format!(
                "MCP servers ({} connected, {} tools total)\n",
                ctx.mcp_statuses.len(),
                total_tools
            )];

            for s in ctx.mcp_statuses {
                // --mcp-config servers are in no scope.
                let scope = configured
                    .iter()
                    .find(|c| c.name == s.name && c.is_effective())
                    .map_or("", |c| c.scope.as_str());
                lines.push(format!(
                    "  {:20} {:7} [{}]  {} tool{}  MCP {}",
                    s.name,
                    scope,
                    s.transport,
                    s.tool_count,
                    if s.tool_count == 1 { "" } else { "s" },
                    s.protocol
                ));
            }

            lines.push(String::new());
            if let Some(note) = failed_note {
                lines.push(note);
            }
            if let Some(note) = trust_note {
                lines.push(note);
            }
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
        "add" => mcp_add_server(rest, cwd, config_dir),
        "remove" | "rm" | "delete" => mcp_remove_server(rest, cwd, config_dir),
        "enable" => mcp_set_disabled(rest, false, cwd, config_dir),
        "disable" => mcp_set_disabled(rest, true, cwd, config_dir),
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
            let entries: Vec<_> = crate::mcp::scope::list(cwd, config_dir)
                .into_iter()
                .filter(|s| s.name == name)
                .collect();
            if entries.is_empty() {
                return CommandAction::Message(format!("MCP server '{name}' not found."));
            }
            let mut out = format!("MCP server '{name}'");
            for s in &entries {
                let state = if s.needs_trust {
                    " — not started until you /trust this project"
                } else if let Some(by) = s.overridden_by {
                    &format!(" — overridden by the {by} entry")
                } else {
                    ""
                };
                out.push_str(&format!(
                    "\n\n{} scope ({}){state}\n{}",
                    s.scope,
                    s.path.display(),
                    serde_json::to_string_pretty(&s.config).unwrap_or_default()
                ));
            }
            CommandAction::Message(out)
        }
        _ => CommandAction::Message(
            "MCP commands:\n  /mcp list              — show connected servers and protocol revisions\n  \
             /mcp tools             — show tools per server\n  \
             /mcp add <n> <cmd>     — add stdio server (private to you, this project)\n  \
             /mcp add <n> <url>     — add HTTP server\n  \
             /mcp add --scope user|project <n> ...  — every project / shared .mcp.json\n  \
             /mcp remove <n>        — remove server (--scope to pick one)\n  \
             /mcp enable <n>        — enable disabled server\n  \
             /mcp disable <n>       — disable server\n  \
             /mcp get <n>           — show server config and scope\n  \
             /mcp reconnect         — restart all (requires app restart)"
                .into(),
        ),
    }
}

/// A leading `--scope <s>` / `-s <s>` / `--scope=<s>`, and what follows it.
fn take_scope(args: &str) -> Result<(Option<crate::mcp::scope::Scope>, &str), String> {
    let (first, rest) = split_first_word(args);
    let (value, rest) = match first.strip_prefix("--scope=") {
        Some(v) => (v, rest),
        None if first == "--scope" || first == "-s" => split_first_word(rest),
        None => return Ok((None, args.trim())),
    };
    crate::mcp::scope::Scope::parse(value)
        .map(|s| (Some(s), rest))
        .map_err(|e| e.to_string())
}

/// `/mcp add [--scope local|project|user] <name> <command|url> [args...]`.
/// Local (private to you, this project) by default, like `oxideclaw mcp add`.
/// Detects HTTP servers by URL prefix; everything else is stdio.
pub(super) fn mcp_add_server(
    args: &str,
    cwd: &std::path::Path,
    config_dir: &std::path::Path,
) -> CommandAction {
    use crate::mcp::scope::Scope;
    use crate::mcp::types::{HttpServerConfig, McpServerConfig, StdioServerConfig};
    // `--force` before or after `--scope`, as on the command line.
    fn take_force(args: &str) -> (bool, &str) {
        match split_first_word(args) {
            ("--force", rest) => (true, rest),
            _ => (false, args),
        }
    }
    let (force, args) = take_force(args);
    let (scope, args) = match take_scope(args) {
        Ok(v) => v,
        Err(e) => return CommandAction::Message(e),
    };
    let (force_after, args) = take_force(args);
    let force = force || force_after;
    let scope = scope.unwrap_or(Scope::Local);
    let (name, rest) = split_first_word(args);
    if name.is_empty() {
        return CommandAction::Message(
            "Usage: /mcp add [--scope local|project|user] [--force] <name> <command|url> \
             [args...]\n\
             Examples:\n  /mcp add github npx -y @modelcontextprotocol/server-github\n  \
             /mcp add remote http://localhost:3000/mcp\n\
             local (default): only you, this project. user: only you, every project. \
             project: the repo's .mcp.json, shared; starts after /trust, and \
             --force writes a literal secret there."
                .into(),
        );
    }
    let rest = rest.trim();
    if rest.is_empty() {
        return CommandAction::Message(format!("Usage: /mcp add {name} <command|url> [args...]"));
    }

    let path = scope.write_path(cwd, config_dir);
    match crate::config::read_json_object(&path) {
        Err(e) => return CommandAction::Message(e.to_string()),
        Ok(v) if v.get("mcpServers").and_then(|m| m.get(name)).is_some() => {
            return CommandAction::Message(format!(
                "MCP server '{name}' already exists in the {scope} scope. Remove it first \
                 with /mcp remove --scope {scope} {name}"
            ));
        }
        Ok(_) => {}
    }

    let server_cfg = if rest.starts_with("http://") || rest.starts_with("https://") {
        McpServerConfig::Http(HttpServerConfig {
            url: rest.to_string(),
            headers: Default::default(),
            disabled: false,
            sse: false,
            literal: false,
        })
    } else {
        let mut parts = rest.split_whitespace().map(str::to_string);
        let Some(command) = parts.next() else {
            return CommandAction::Message("Invalid: empty command string".into());
        };
        McpServerConfig::Stdio(StdioServerConfig {
            command,
            args: parts.collect(),
            env: Default::default(),
            disabled: false,
            literal: false,
        })
    };

    match crate::mcp::scope::add(name, server_cfg, scope, cwd, config_dir, force) {
        Ok(path) => {
            let mut msg = format!(
                "MCP server '{name}' added to the {scope} scope ({}).\nRestart oxideclaw to connect.",
                path.display()
            );
            if scope == Scope::Project
                && !crate::settings::Settings::is_trusted(
                    &crate::settings::Settings::load_file(&config_dir.join("settings.json")),
                    cwd,
                )
            {
                msg.push_str(
                    "\nThis project is not trusted: run /trust to start .mcp.json servers.",
                );
            }
            CommandAction::Message(msg)
        }
        Err(e) => CommandAction::Message(format!("Failed to add '{name}': {e}")),
    }
}

/// `/mcp remove [--scope s] <name>`.
pub(super) fn mcp_remove_server(
    args: &str,
    cwd: &std::path::Path,
    config_dir: &std::path::Path,
) -> CommandAction {
    let (scope, name) = match take_scope(args) {
        Ok(v) => v,
        Err(e) => return CommandAction::Message(e),
    };
    if name.is_empty() {
        return CommandAction::Message(
            "Usage: /mcp remove [--scope local|project|user] <name>".into(),
        );
    }
    match crate::mcp::scope::remove(name, scope, cwd, config_dir) {
        Ok(Some(from)) => CommandAction::Message(format!(
            "MCP server '{name}' removed from the {from} scope.\nRestart oxideclaw to disconnect."
        )),
        Ok(None) => CommandAction::Message(match scope {
            Some(s) => format!("MCP server '{name}' not found in the {s} scope."),
            None => format!("MCP server '{name}' not found."),
        }),
        Err(e) => CommandAction::Message(e.to_string()),
    }
}

/// Enable or disable an MCP server via a `disabled` flag, in the scope whose
/// entry starts.
pub(super) fn mcp_set_disabled(
    args: &str,
    disabled: bool,
    cwd: &std::path::Path,
    config_dir: &std::path::Path,
) -> CommandAction {
    let name = args.trim();
    if name.is_empty() {
        return CommandAction::Message(if disabled {
            "Usage: /mcp disable <name>".into()
        } else {
            "Usage: /mcp enable <name>".into()
        });
    }
    match crate::mcp::scope::set_disabled(name, disabled, cwd, config_dir) {
        Ok(Some((scope, _))) => CommandAction::Message(format!(
            "MCP server '{name}' {} in the {scope} scope. Restart oxideclaw to apply.",
            if disabled { "disabled" } else { "enabled" }
        )),
        Ok(None) => CommandAction::Message(format!("MCP server '{name}' not found.")),
        Err(e) => CommandAction::Message(format!("{e:#}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(a: CommandAction) -> String {
        match a {
            CommandAction::Message(m) => m,
            _ => panic!("expected a message"),
        }
    }

    /// `/mcp add` wrote every server into settings.json, which every project
    /// reads; like `oxideclaw mcp add` it now defaults to the local scope.
    #[test]
    fn slash_add_defaults_to_local_and_list_get_remove_show_the_scope() {
        let home = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let (r, h) = (repo.path(), home.path());

        let out = msg(mcp_add_server("gh npx -y srv", r, h));
        assert!(out.contains("local scope"), "{out}");
        assert!(!h.join("settings.json").exists());
        assert!(!r.join(".mcp.json").exists());
        let out = msg(mcp_add_server("--scope project team ./srv", r, h));
        assert!(out.contains("/trust"), "{out}");
        assert!(r.join(".mcp.json").is_file());
        msg(mcp_add_server("-s user gh npx other", r, h));
        assert!(msg(mcp_add_server("--scope=nope x y", r, h)).contains("unknown scope"));
        // A literal key in a shared URL is refused, and --force (which the
        // refusal suggests) writes it.
        let keyed = "https://h.test/mcp?api_key=sk-1";
        let out = msg(mcp_add_server(&format!("-s project k {keyed}"), r, h));
        assert!(out.contains("url query api_key"), "{out}");
        let out = msg(mcp_add_server(
            &format!("--force -s project k {keyed}"),
            r,
            h,
        ));
        assert!(out.contains("project scope"), "{out}");
        let out = msg(mcp_add_server(
            &format!("-s project --force k2 {keyed}"),
            r,
            h,
        ));
        assert!(out.contains("project scope"), "{out}");
        msg(mcp_remove_server("--scope project k", r, h));
        msg(mcp_remove_server("--scope project k2", r, h));

        let config = Config {
            cwd: r.to_path_buf(),
            ..Config::default()
        };
        let skills = HashMap::new();
        let todo = TodoState::default();
        let statuses = [crate::mcp::types::McpServerStatus {
            name: "gh".into(),
            transport: "stdio",
            protocol: "2026-07-28".into(),
            tool_count: 2,
        }];
        let ctx = CommandContext {
            config: &config,
            tokens_in: 0,
            context_window: 0,
            tokens_out: 0,
            cache_read_tokens: 0,
            cost_summary: String::new(),
            cost_recorded: false,
            cache_write_tokens: 0,
            vim_mode: false,
            skills: &skills,
            todo_state: &todo,
            last_assistant: None,
            session_id: "s",
            session_name: "",
            claudemd: "",
            mcp_statuses: &statuses,
            mcp_failed: &[],
            brief_mode: false,
            btw_note: None,
        };
        let list = msg(cmd_mcp_in("list", &ctx, h));
        assert!(
            list.lines()
                .any(|l| l.contains("gh") && l.contains("local") && l.contains("MCP 2026-07-28")),
            "{list}"
        );
        assert!(
            list.contains("/trust this project (.mcp.json): team"),
            "{list}"
        );

        let get = msg(cmd_mcp_in("get gh", &ctx, h));
        assert!(get.contains("local scope"), "{get}");
        assert!(
            get.contains("user scope") && get.contains("overridden by the local"),
            "{get}"
        );

        let out = msg(mcp_set_disabled("gh", true, r, h));
        assert!(out.contains("local scope"), "{out}");

        assert!(msg(mcp_remove_server("gh", r, h)).contains("--scope"));
        assert!(msg(mcp_remove_server("--scope user gh", r, h)).contains("user scope"));
        assert!(msg(mcp_remove_server("gh", r, h)).contains("local scope"));
        assert!(msg(mcp_remove_server("gh", r, h)).contains("not found"));
    }

    /// A configured server that failed at startup did not appear at all:
    /// `/mcp` said none were connected and suggested adding one.
    #[test]
    fn list_names_servers_that_failed_to_start() {
        let home = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let config = Config {
            cwd: repo.path().to_path_buf(),
            ..Config::default()
        };
        let skills = HashMap::new();
        let todo = TodoState::default();
        let failed = ["gh".to_string()];
        let statuses = [crate::mcp::types::McpServerStatus {
            name: "fs".into(),
            transport: "stdio",
            protocol: "2026-07-28".into(),
            tool_count: 1,
        }];
        let ctx = |statuses| CommandContext {
            config: &config,
            tokens_in: 0,
            context_window: 0,
            tokens_out: 0,
            cache_read_tokens: 0,
            cost_summary: String::new(),
            cost_recorded: false,
            cache_write_tokens: 0,
            vim_mode: false,
            skills: &skills,
            todo_state: &todo,
            last_assistant: None,
            session_id: "s",
            session_name: "",
            claudemd: "",
            mcp_statuses: statuses,
            mcp_failed: &failed,
            brief_mode: false,
            btw_note: None,
        };

        let only_failed = msg(cmd_mcp_in("", &ctx(&[]), home.path()));
        assert!(
            only_failed.contains("failed to start") && only_failed.contains(": gh."),
            "{only_failed}"
        );
        assert!(!only_failed.contains("Add one"), "{only_failed}");

        let mixed = msg(cmd_mcp_in("list", &ctx(&statuses), home.path()));
        assert!(mixed.contains("1 connected"), "{mixed}");
        assert!(
            mixed.contains("failed to start") && mixed.contains(": gh."),
            "{mixed}"
        );
    }
}
