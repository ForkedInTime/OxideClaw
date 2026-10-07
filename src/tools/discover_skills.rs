/// DiscoverSkillsTool — port of discoverSkills.ts
/// Lists the skills `/name` and the Skill tool can run: the built-in set plus
/// the global skills dir and .claude/skills/.
use super::{Tool, ToolContext, ToolOutput, async_trait};
use crate::skills::Skill;
use anyhow::Result;
use serde_json::json;
use std::collections::HashMap;

pub struct DiscoverSkillsTool;

#[async_trait]
impl Tool for DiscoverSkillsTool {
    fn name(&self) -> &str {
        "DiscoverSkills"
    }

    fn description(&self) -> &str {
        "List available skills (slash commands): the built-in skills plus those in the \
        global skills dir (skills/ under the config dir, ~/.config/oxideclaw/skills/ by \
        default), ~/.claude/skills/ and .claude/skills/. Returns a list of skill names and their descriptions."
    }

    fn input_schema(&self) -> serde_json::Value {
        json!({ "type": "object", "properties": {} })
    }

    async fn execute(&self, _input: serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let skills = crate::skills::load_skills_in(&ctx.cwd).await;
        Ok(ToolOutput::success(list_skills(&skills)))
    }
}

/// One `/name — description` line per skill, sorted by name.
fn list_skills(skills: &HashMap<String, Skill>) -> String {
    let mut names: Vec<&String> = skills.keys().collect();
    names.sort();
    names
        .into_iter()
        .map(|name| {
            let desc = summary(&skills[name]);
            if desc.is_empty() {
                format!("/{name}")
            } else {
                format!("/{name} — {desc}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// A plain markdown skill has no description (the parser falls back to its
/// name), so describe it by the first line of its prompt instead.
fn summary(skill: &Skill) -> String {
    let source = if skill.description.is_empty() || skill.description == skill.name {
        &skill.prompt_template
    } else {
        &skill.description
    };
    source
        .lines()
        .map(|l| l.trim().trim_start_matches('#').trim())
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .chars()
        .take(120)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tool scanned the directories itself, so built-in skills were
    /// missing and descriptions disagreed with `/skills`.
    #[tokio::test]
    async fn lists_bundled_global_and_project_skills_with_project_winning() {
        let dir = tempfile::tempdir().unwrap();
        let global = dir.path().join("xdg/oxideclaw/skills");
        let local = dir.path().join("proj/.claude/skills");
        std::fs::create_dir_all(&global).unwrap();
        std::fs::create_dir_all(&local).unwrap();
        std::fs::write(global.join("deploy.md"), "# Deploy\nShip it").unwrap();
        std::fs::write(global.join("both.md"), "global copy").unwrap();
        std::fs::write(local.join("both.md"), "project copy").unwrap();
        std::fs::write(
            local.join("lint.md"),
            "---\nname: lint\ndescription: Run the linters\n---\nRun {{ARGS}}",
        )
        .unwrap();

        let skills = crate::skills::load_skills_from(&[global, local]).await;
        let out = list_skills(&skills);
        let lines: Vec<&str> = out.lines().collect();
        assert!(lines.contains(&"/both — project copy"), "{out}");
        assert!(lines.contains(&"/deploy — Deploy"), "{out}");
        assert!(lines.contains(&"/lint — Run the linters"), "{out}");
        assert!(lines.iter().any(|l| l.starts_with("/commit — ")), "{out}");
        let mut sorted = lines.clone();
        sorted.sort();
        assert_eq!(lines, sorted);
    }
}
