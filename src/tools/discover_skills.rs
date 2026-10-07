/// DiscoverSkillsTool — port of discoverSkills.ts
/// Lists available skills from the global skills dir and .claude/skills/
use super::{Tool, ToolContext, ToolOutput, async_trait};
use anyhow::Result;
use serde_json::json;
use std::path::PathBuf;

pub struct DiscoverSkillsTool;

#[async_trait]
impl Tool for DiscoverSkillsTool {
    fn name(&self) -> &str {
        "DiscoverSkills"
    }

    fn description(&self) -> &str {
        "List available skills (slash commands) from the global skills dir \
        (~/.claude/skills/ by default) and .claude/skills/. \
        Returns a list of skill names and their first-line descriptions."
    }

    fn input_schema(&self) -> serde_json::Value {
        json!({ "type": "object", "properties": {} })
    }

    async fn execute(&self, _input: serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput> {
        // Same global dir `/name` loads from, so CLAUDE_CONFIG_DIR / XDG
        // skills are listed too.
        let global = crate::config::Config::claude_dir().join("skills");
        let skills = list_skills(&[ctx.cwd.join(".claude").join("skills"), global.clone()]).await?;

        if skills.is_empty() {
            return Ok(ToolOutput::success(format!(
                "No skills found. Place .md files in {} or .claude/skills/.",
                global.display()
            )));
        }

        let lines: Vec<String> = skills
            .iter()
            .map(|(name, desc)| {
                if desc.is_empty() {
                    format!("/{name}")
                } else {
                    format!("/{name} — {desc}")
                }
            })
            .collect();

        Ok(ToolOutput::success(lines.join("\n")))
    }
}

/// Skills in `dirs`, sorted by name; on a name clash the earlier dir wins.
async fn list_skills(dirs: &[PathBuf]) -> Result<Vec<(String, String)>> {
    let mut skills: Vec<(String, String)> = Vec::new();
    for dir in dirs {
        if !dir.is_dir() {
            continue;
        }
        let mut entries = tokio::fs::read_dir(dir).await?;
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("md") {
                let name = path
                    .file_stem()
                    .and_then(|n| n.to_str())
                    .unwrap_or("")
                    .to_string();
                if name.is_empty() {
                    continue;
                }

                // Read first non-empty, non-frontmatter line as description
                // (never from a link to key material).
                let desc = if super::check_sensitive_path_resolved(&path, super::SensitiveOp::Read)
                    .is_none()
                    && let Ok(content) = tokio::fs::read_to_string(&path).await
                {
                    extract_description(&content)
                } else {
                    String::new()
                };

                // Avoid duplicates (local overrides global)
                if !skills.iter().any(|(n, _)| n == &name) {
                    skills.push((name, desc));
                }
            }
        }
    }

    skills.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(skills)
}

fn extract_description(content: &str) -> String {
    let mut in_frontmatter = false;
    let mut frontmatter_done = false;
    let mut fence_count = 0;

    for line in content.lines() {
        let trimmed = line.trim();

        // Handle YAML frontmatter delimited by ---
        if trimmed == "---" {
            fence_count += 1;
            if fence_count == 1 {
                in_frontmatter = true;
                continue;
            } else if fence_count == 2 {
                in_frontmatter = false;
                frontmatter_done = true;
                continue;
            }
        }

        if in_frontmatter {
            continue;
        }

        // Skip blank lines and heading markers at the start
        if trimmed.is_empty() {
            continue;
        }
        if !frontmatter_done && trimmed.starts_with('#') {
            continue;
        }

        // Strip leading '#' characters (headings)
        let clean = trimmed.trim_start_matches('#').trim();
        if !clean.is_empty() {
            return clean.chars().take(120).collect();
        }
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The global dir was hard-coded to ~/.claude/skills, so skills under
    /// CLAUDE_CONFIG_DIR or XDG config were missing from the list, and a
    /// global copy shadowed the project's description.
    #[tokio::test]
    async fn lists_the_given_global_dir_and_project_wins_clashes() {
        let dir = tempfile::tempdir().unwrap();
        let global = dir.path().join("xdg/oxideclaw/skills");
        let local = dir.path().join("proj/.claude/skills");
        std::fs::create_dir_all(&global).unwrap();
        std::fs::create_dir_all(&local).unwrap();
        std::fs::write(global.join("deploy.md"), "Ship it").unwrap();
        std::fs::write(global.join("both.md"), "global copy").unwrap();
        std::fs::write(local.join("both.md"), "project copy").unwrap();

        let skills = list_skills(&[local, global]).await.unwrap();
        assert_eq!(
            skills,
            vec![
                ("both".to_string(), "project copy".to_string()),
                ("deploy".to_string(), "Ship it".to_string()),
            ]
        );
    }
}
