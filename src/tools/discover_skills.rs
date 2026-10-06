/// DiscoverSkillsTool — port of discoverSkills.ts
/// Lists available skills from ~/.claude/skills/ and .claude/skills/
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
        "List available skills (slash commands) from ~/.claude/skills/ and .claude/skills/. \
        Returns a list of skill names and their first-line descriptions."
    }

    fn input_schema(&self) -> serde_json::Value {
        json!({ "type": "object", "properties": {} })
    }

    async fn execute(&self, _input: serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let mut skills: Vec<(String, String)> = Vec::new();

        let dirs: Vec<PathBuf> = {
            let mut d = Vec::new();
            if let Some(home) = dirs::home_dir() {
                d.push(home.join(".claude").join("skills"));
            }
            d.push(ctx.cwd.join(".claude").join("skills"));
            d
        };

        for dir in &dirs {
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
                    let desc =
                        if super::check_sensitive_path_resolved(&path, super::SensitiveOp::Read)
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

        if skills.is_empty() {
            return Ok(ToolOutput::success(
                "No skills found. Place .md files in ~/.claude/skills/ or .claude/skills/.",
            ));
        }

        skills.sort_by(|a, b| a.0.cmp(&b.0));
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
