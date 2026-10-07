/// DiscoverSkillsTool — port of discoverSkills.ts
/// Lists the skills `/name` and the Skill tool can run: the built-in set plus
/// every skill `crate::skills::load_skills_in` finds. Names and descriptions
/// only; a skill's body reaches the model when the Skill tool runs it.
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
        "List available skills (slash commands): the built-in skills plus Agent Skills \
        (<name>/SKILL.md) and markdown skills in the project's .agents/skills/, \
        .oxideclaw/skills/ or .claude/skills/, the config dir's skills/ or ~/.claude/skills/. \
        Returns each skill's name and description; run one with the Skill tool."
    }

    fn input_schema(&self) -> serde_json::Value {
        json!({ "type": "object", "properties": {} })
    }

    async fn execute(&self, _input: serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let loaded = crate::skills::load_skills_in(&ctx.cwd).await;
        Ok(ToolOutput::success(render(&loaded)))
    }
}

/// The listing, then the skipped-skill notice so the model can say why a
/// skill the user expects is missing.
fn render(loaded: &crate::skills::LoadedSkills) -> String {
    let mut out = list_skills(&loaded.skills);
    if let Some(warning) = loaded.warning() {
        out.push_str("\n\n");
        out.push_str(&warning);
    }
    out
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
        let global = dir.path().join("xdg/oxideclaw");
        let local = dir.path().join("proj/.claude/skills");
        std::fs::create_dir_all(global.join("skills")).unwrap();
        std::fs::create_dir_all(&local).unwrap();
        std::fs::write(global.join("skills/deploy.md"), "# Deploy\nShip it").unwrap();
        std::fs::write(global.join("skills/both.md"), "global copy").unwrap();
        std::fs::write(local.join("both.md"), "project copy").unwrap();
        std::fs::write(
            local.join("lint.md"),
            "---\nname: lint\ndescription: Run the linters\n---\nRun {{ARGS}}",
        )
        .unwrap();

        let skills = crate::skills::load_skills_at(&dir.path().join("proj"), &global, None)
            .await
            .skills;
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

    /// `<name>/SKILL.md` skills were never listed. Now they are, by their
    /// frontmatter description, without their body; a broken one is named.
    #[tokio::test]
    async fn lists_skill_md_skills_without_their_body_and_names_invalid_ones() {
        let dir = tempfile::tempdir().unwrap();
        let proj = dir.path().join("proj");
        let skills = proj.join(".agents/skills");
        std::fs::create_dir_all(skills.join("release")).unwrap();
        std::fs::create_dir_all(skills.join("broken")).unwrap();
        std::fs::write(
            skills.join("release/SKILL.md"),
            "---\nname: release\ndescription: Cut a release\n---\nSECRET-BODY steps",
        )
        .unwrap();
        std::fs::write(skills.join("broken/SKILL.md"), "no frontmatter").unwrap();

        let loaded = crate::skills::load_skills_at(&proj, &dir.path().join("cfg"), None).await;
        let out = render(&loaded);
        assert!(
            out.lines().any(|l| l == "/release — Cut a release"),
            "{out}"
        );
        assert!(!out.contains("SECRET-BODY"), "{out}");
        let broken = std::path::Path::new("broken").join("SKILL.md");
        assert!(
            out.contains(&format!("{} — no YAML frontmatter", broken.display())),
            "{out}"
        );
    }
}
