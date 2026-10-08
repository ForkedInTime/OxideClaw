//! `/` command handlers — split out of `commands/mod.rs` mechanically.

use super::*;

pub(super) fn cmd_skills(ctx: &CommandContext) -> CommandAction {
    if ctx.skills.is_empty() {
        return CommandAction::Message(format!(
            "No skills loaded.\n\
             \n\
             Create a skill as .agents/skills/<name>/SKILL.md in your project\n\
             (or {skills}/<name>/SKILL.md for every project), with YAML\n\
             frontmatter giving its `name` and `description`.\n\
             Each skill becomes a /<name> command.\n\
             \n\
             Example: .agents/skills/deploy-check/SKILL.md\n\
             Then type /deploy-check [args] to run it.",
            skills = crate::config::Config::config_dir().join("skills").display()
        ));
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
            // Built-in commands are matched first, so /name never
            // reaches this skill; say so rather than list it as runnable.
            if crate::skills::is_builtin_command(name) {
                lines.push(format!(
                    "  /{name} — {desc} (hidden by the built-in /{name}; rename the skill to use it)"
                ));
            } else {
                lines.push(format!("  /{name} — {desc}"));
            }
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
    let mut search_dirs = vec![
        ctx.config.cwd.join(".claude").join("agents"),
        crate::config::Config::config_dir().join("agents"),
    ];
    search_dirs.extend(crate::config::Config::claude_code_dir().map(|d| d.join("agents")));

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
                let description = agent_description(&path.join("AGENT.md"));
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

    // Nothing reads these files yet: the Agent tool only knows its
    // built-in types, so do not invite users to write agents that never run.
    if total == 0 {
        lines.push(
            "No agent definitions found in .claude/agents/, the config dir's agents/ or \
             ~/.claude/agents/."
                .into(),
        );
    } else {
        lines.insert(1, format!("{total} agent definition(s) found\n"));
    }
    lines.push(
        "Note: OxideClaw lists these files but does not load them yet. The Agent tool \
         supports only its built-in types: general-purpose, Explore, Plan, verification, \
         oxideclaw-guide."
            .into(),
    );

    CommandAction::Message(lines.join("\n"))
}

/// The first plain line of an agent's `AGENT.md`, or empty. A cloned repo
/// controls this file and /agents reads it on the event loop: a link to
/// /dev/zero read until out of memory and one to /dev/tty froze the TUI,
/// so only a regular file under the config-file size cap is read.
fn agent_description(agent_md: &std::path::Path) -> String {
    crate::settings::read_config_file(agent_md)
        .ok()
        .flatten()
        .and_then(|content| {
            content
                .lines()
                .find(|l| !l.trim().is_empty() && !l.starts_with("---") && !l.starts_with('#'))
                .map(|l| l.trim().to_string())
        })
        .unwrap_or_default()
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
    // Decided by word count, not by the shape of the id: one bare token
    // after a subcommand word is an id (a typo or unknown one is a cheap
    // "No agent found"), a multi-word remainder is a task. Checking for hex
    // made `/spawn kill a1b2c3dz` launch a paid bypass-permissions agent.
    let one_token = !rest.is_empty() && !rest.contains(char::is_whitespace);
    // A pasted branch name `spawn-<slug>-<id>` names its trailing id.
    let id = rest.rsplit('-').next().unwrap_or(rest).to_string();
    match sub {
        "list" | "ls" if rest.is_empty() || one_token => CommandAction::ListSpawns,
        "review" | "diff" if rest.is_empty() => {
            CommandAction::Message("Usage: /spawn review <agent-id>".into())
        }
        "review" | "diff" if one_token => CommandAction::ReviewSpawn(id),
        "merge" if rest.is_empty() => {
            CommandAction::Message("Usage: /spawn merge <agent-id>".into())
        }
        "merge" if one_token => CommandAction::MergeSpawn(id),
        "kill" | "cancel" if rest.is_empty() => {
            CommandAction::Message("Usage: /spawn kill <agent-id>".into())
        }
        "kill" | "cancel" if one_token => CommandAction::KillSpawn(id),
        "discard" | "drop" if rest.is_empty() => {
            CommandAction::Message("Usage: /spawn discard <agent-id>".into())
        }
        "discard" | "drop" if one_token => CommandAction::DiscardSpawn(id),
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

#[cfg(test)]
mod spawn_parse_tests {
    use super::{CommandAction, cmd_spawn};

    /// Tasks that start with a subcommand word used to be parsed as that
    /// subcommand with the rest of the sentence as an agent id.
    #[test]
    fn tasks_starting_with_a_subcommand_word_are_spawned() {
        for task in [
            "review the auth module",
            "list all TODOs in src",
            "merge the two config parsers",
            "kill dead code in utils",
            "discard unused imports",
            "drop the legacy API",
        ] {
            match cmd_spawn(task) {
                CommandAction::SpawnAgent(t) => assert_eq!(t, task),
                _ => panic!("{task:?} was not spawned as a task"),
            }
        }
    }

    #[test]
    fn subcommands_with_an_id_still_dispatch() {
        assert!(matches!(cmd_spawn("list"), CommandAction::ListSpawns));
        assert!(matches!(cmd_spawn("ls"), CommandAction::ListSpawns));
        assert!(
            matches!(cmd_spawn("review 3fa85f64"), CommandAction::ReviewSpawn(id) if id == "3fa85f64")
        );
        assert!(matches!(cmd_spawn("diff 3fa8"), CommandAction::ReviewSpawn(id) if id == "3fa8"));
        assert!(matches!(
            cmd_spawn("merge 3fa85f64"),
            CommandAction::MergeSpawn(_)
        ));
        assert!(matches!(
            cmd_spawn("kill 3fa85f64"),
            CommandAction::KillSpawn(_)
        ));
        assert!(matches!(
            cmd_spawn("discard 3fa85f64"),
            CommandAction::DiscardSpawn(_)
        ));
        assert!(matches!(cmd_spawn("merge"), CommandAction::Message(_)));
    }

    /// A mistyped or non-hex id fell through to SpawnAgent and launched a
    /// paid background agent instead of reporting "No agent found".
    #[test]
    fn a_single_token_after_a_subcommand_is_always_an_id() {
        assert!(
            matches!(cmd_spawn("kill a1b2c3dz"), CommandAction::KillSpawn(id) if id == "a1b2c3dz")
        );
        assert!(
            matches!(cmd_spawn("review a1b2c3d4e"), CommandAction::ReviewSpawn(id) if id == "a1b2c3d4e")
        );
        assert!(matches!(
            cmd_spawn("merge spawn-refactor-auth-a1b2c3d4"),
            CommandAction::MergeSpawn(id) if id == "a1b2c3d4"
        ));
        assert!(matches!(
            cmd_spawn("list running"),
            CommandAction::ListSpawns
        ));
        assert!(matches!(
            cmd_spawn("discard dead"),
            CommandAction::DiscardSpawn(_)
        ));
    }
}

#[cfg(test)]
mod agent_description_tests {
    use super::agent_description;

    #[test]
    fn reads_the_first_plain_line() {
        let dir = tempfile::tempdir().unwrap();
        let md = dir.path().join("AGENT.md");
        std::fs::write(&md, "---\n# Reviewer\n\n  Reviews diffs  \nmore").unwrap();
        assert_eq!(agent_description(&md), "Reviews diffs");
        assert_eq!(agent_description(&dir.path().join("missing.md")), "");
    }

    /// A repo's AGENT.md linked to /dev/zero was read until out of memory.
    #[cfg(unix)]
    #[test]
    fn a_special_file_is_not_read() {
        let dir = tempfile::tempdir().unwrap();
        let md = dir.path().join("AGENT.md");
        std::os::unix::fs::symlink("/dev/zero", &md).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || tx.send(agent_description(&md)).unwrap());
        let got = rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("reading AGENT.md must not block or run away");
        assert_eq!(got, "");
    }
}
