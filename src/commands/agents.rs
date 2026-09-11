//! `/` command handlers — split out of `commands/mod.rs` mechanically.

use super::*;

pub(super) fn cmd_skills(ctx: &CommandContext) -> CommandAction {
    if ctx.skills.is_empty() {
        return CommandAction::Message(
            "No skills loaded.\n\
             \n\
             Create skills by adding .md files to ~/.claude/skills/\n\
             Each file becomes a /skill-name command.\n\
             \n\
             Example: ~/.claude/skills/review.md\n\
             Then type /review [args] to expand it."
                .into(),
        );
    }

    let mut lines = vec![format!("Loaded skills ({})\n", ctx.skills.len())];
    let mut names: Vec<_> = ctx.skills.keys().collect();
    names.sort();
    for name in names {
        if let Some(skill) = ctx.skills.get(name) {
            // v2.1.91: cap skill descriptions at 250 chars so the listing
            // doesn't blow out the terminal when a skill has a long preamble.
            let desc = if skill.description.chars().count() > 250 {
                let truncated: String = skill.description.chars().take(247).collect();
                format!("{truncated}…")
            } else {
                skill.description.clone()
            };
            lines.push(format!("  /{name} — {desc}"));
        }
    }
    lines.push(String::new());
    lines.push("Usage: /skill-name [args]".into());
    CommandAction::Message(lines.join("\n"))
}

pub(super) fn cmd_tasks(ctx: &CommandContext) -> CommandAction {
    let state = ctx.todo_state.lock().unwrap_or_else(|e| e.into_inner());
    if state.is_empty() {
        return CommandAction::Message(
            "No tasks. Claude will create tasks automatically for complex multi-step work.".into(),
        );
    }

    let mut lines = vec![format!("Tasks ({})\n", state.len())];
    for item in state.iter() {
        let icon = match item.status {
            TodoStatus::Completed => "✓",
            TodoStatus::InProgress => "▶",
            TodoStatus::Pending => "○",
        };
        let pri = match item.priority {
            TodoPriority::High => " [high]",
            TodoPriority::Medium => "",
            TodoPriority::Low => " [low]",
        };
        lines.push(format!("  {icon} {}{pri}", item.content));
    }
    CommandAction::Message(lines.join("\n"))
}

pub(super) fn cmd_brief(_ctx: &CommandContext) -> CommandAction {
    CommandAction::ToggleBriefMode
}

pub(super) fn cmd_btw(args: &str) -> CommandAction {
    let note = args.trim();
    if note.is_empty() {
        CommandAction::Message(
            "Usage: /btw <note>\n\n\
             Prepends a note to your next message. Useful for providing context \
             without making it the main focus of your prompt.\n\n\
             Example: /btw I'm using Python 3.11 on macOS\n\
             Then your next message will include that context automatically."
                .into(),
        )
    } else {
        CommandAction::SetBtwNote(note.to_string())
    }
}

pub(super) fn cmd_agents(ctx: &CommandContext) -> CommandAction {
    // List agents defined in .claude/agents/ directories
    let mut search_dirs = vec![ctx.config.cwd.join(".claude").join("agents")];
    if let Some(home) = dirs::home_dir() {
        search_dirs.push(home.join(".claude").join("agents"));
    }

    let mut lines = vec!["Agents\n".to_string()];
    let mut total = 0usize;

    for dir in &search_dirs {
        if !dir.exists() {
            continue;
        }
        let label = if dir.starts_with(&ctx.config.cwd) {
            "Project agents"
        } else {
            "Global agents"
        };
        lines.push(format!("{label}:"));

        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => continue,
        };

        let mut found = false;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let name = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("")
                    .to_string();
                // Read description from AGENT.md if present
                let agent_md = path.join("AGENT.md");
                let description = if agent_md.exists() {
                    std::fs::read_to_string(&agent_md)
                        .ok()
                        .and_then(|content| {
                            content
                                .lines()
                                .find(|l| {
                                    !l.trim().is_empty()
                                        && !l.starts_with("---")
                                        && !l.starts_with('#')
                                })
                                .map(|l| l.trim().to_string())
                        })
                        .unwrap_or_default()
                } else {
                    String::new()
                };
                if description.is_empty() {
                    lines.push(format!("  {name}"));
                } else {
                    lines.push(format!("  {name} — {description}"));
                }
                total += 1;
                found = true;
            } else if path.extension().and_then(|e| e.to_str()) == Some("md") {
                let name = path
                    .file_stem()
                    .and_then(|n| n.to_str())
                    .unwrap_or("")
                    .to_string();
                lines.push(format!("  {name}"));
                total += 1;
                found = true;
            }
        }
        if !found {
            lines.push("  (none)".into());
        }
        lines.push(String::new());
    }

    if total == 0 {
        lines.push(
            "No agents found. Create agent definitions in .claude/agents/ or ~/.claude/agents/."
                .into(),
        );
    } else {
        lines.insert(1, format!("{total} agent(s) found\n"));
    }

    CommandAction::Message(lines.join("\n"))
}

pub(super) fn cmd_spawn(args: &str) -> CommandAction {
    let args = args.trim();
    if args.is_empty() {
        return CommandAction::Message(
            "Usage:\n\
             /spawn <task>           — spawn a background agent to work on <task>\n\
             /spawn list             — list all spawned agents\n\
             /spawn review <id>      — review a completed agent's changes\n\
             /spawn merge <id>       — merge an agent's changes into current branch\n\
             /spawn kill <id>        — cancel a running agent\n\
             /spawn discard <id>     — discard an agent's worktree\n\n\
             Example: /spawn refactor the auth module to use JWT tokens"
                .into(),
        );
    }

    let (sub, rest) = split_first_word(args);
    match sub {
        "list" | "ls" => CommandAction::ListSpawns,
        "review" | "diff" => {
            if rest.is_empty() {
                CommandAction::Message("Usage: /spawn review <agent-id>".into())
            } else {
                CommandAction::ReviewSpawn(rest.to_string())
            }
        }
        "merge" => {
            if rest.is_empty() {
                CommandAction::Message("Usage: /spawn merge <agent-id>".into())
            } else {
                CommandAction::MergeSpawn(rest.to_string())
            }
        }
        "kill" | "cancel" => {
            if rest.is_empty() {
                CommandAction::Message("Usage: /spawn kill <agent-id>".into())
            } else {
                CommandAction::KillSpawn(rest.to_string())
            }
        }
        "discard" | "drop" => {
            if rest.is_empty() {
                CommandAction::Message("Usage: /spawn discard <agent-id>".into())
            } else {
                CommandAction::DiscardSpawn(rest.to_string())
            }
        }
        // Everything else is the task description
        _ => CommandAction::SpawnAgent(args.to_string()),
    }
}

pub(super) fn cmd_ultraplan(args: &str) -> CommandAction {
    let depth = args.trim();
    let prompt = if depth == "deep" || depth == "max" {
        "Enter ULTRAPLAN mode. You are a senior engineering lead.\n\
         Perform the deepest possible analysis of this codebase and conversation context:\n\
         1. Map every file and module — purpose, dependencies, interfaces\n\
         2. Identify ALL technical debt, bugs, and inconsistencies\n\
         3. Draw the full data flow and control flow diagrams (in text)\n\
         4. Produce a prioritised implementation plan with concrete steps\n\
         5. Flag every assumption and risk\n\
         Use extended thinking. Be exhaustive. Do not summarise — detail everything."
    } else {
        "Enter ULTRAPLAN mode. Produce a comprehensive implementation plan:\n\
         1. Analyse the current codebase structure and relevant context\n\
         2. Break the work into concrete, ordered tasks with clear acceptance criteria\n\
         3. Identify dependencies, blockers, and risks for each task\n\
         4. Estimate complexity (S/M/L/XL) for each task\n\
         5. Recommend the optimal implementation sequence\n\
         Be specific and actionable. Use TodoWrite to record the plan."
    };
    CommandAction::SendPrompt(prompt.into())
}
