/// SkillTool — port of skill.ts
/// Looks up a skill by name from the skills registry and executes it.
/// Skills are the built-in set plus .md files in the global config dir's skills/
/// or .claude/skills/ — the same set `/name` runs.
use super::{Tool, ToolContext, ToolOutput, async_trait};
use crate::skills::Skill;
use anyhow::Result;
use serde::Deserialize;
use serde_json::json;
use std::collections::HashMap;

pub struct SkillTool;

#[derive(Deserialize)]
struct Input {
    /// Skill name (filename without extension)
    skill: String,
    /// Optional arguments to append to the skill prompt
    #[serde(default)]
    args: Option<String>,
}

#[async_trait]
impl Tool for SkillTool {
    fn name(&self) -> &str {
        "Skill"
    }

    fn description(&self) -> &str {
        "Execute a skill by name. Skills are the built-in skills (commit, review, \
        explain, fix, test) plus markdown prompt templates in the global skills dir \
        (skills/ under the config dir, ~/.claude/skills/ by default) or .claude/skills/. \
        Use DiscoverSkills to list available skills."
    }

    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "skill": {
                    "type": "string",
                    "description": "Skill name, as listed by DiscoverSkills"
                },
                "args": {
                    "type": "string",
                    "description": "Optional arguments or context to pass to the skill"
                }
            },
            "required": ["skill"]
        })
    }

    async fn execute(&self, input: serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let input: Input = serde_json::from_value(input)?;

        // The name becomes `<skills dir>/<name>.md`; keep it a bare file stem.
        if input.skill.is_empty() || input.skill.contains(['/', '\\']) || input.skill.contains("..")
        {
            return Ok(ToolOutput::error(
                "skill must be a bare name (the file stem under .claude/skills), not a path",
            ));
        }

        let skills = crate::skills::load_skills_in(&ctx.cwd).await;
        Ok(invoke(&skills, &input.skill, input.args.as_deref()))
    }
}

/// Expands the skill exactly as `/name` would, so `{{ARGS}}` and declared
/// params are filled in rather than reaching the model as literal
/// placeholders. The loader already refuses skill files that link to key
/// material.
fn invoke(skills: &HashMap<String, Skill>, name: &str, args: Option<&str>) -> ToolOutput {
    let Some(skill) = skills.get(name) else {
        return ToolOutput::error(format!(
            "Skill '{name}' not found in .claude/skills/, {} or the built-in skills.\n\
            Use DiscoverSkills to see available skills.",
            crate::config::Config::claude_dir().join("skills").display()
        ));
    };
    let args = args.unwrap_or("").trim();
    let mut prompt = skill.expand_named(args);
    // A template with nowhere to put free-form args would silently drop the
    // caller's context.
    let has_slot =
        skill.prompt_template.contains("{{ARGS}}") || skill.prompt_template.contains("{{args}}");
    if !args.is_empty() && !has_slot && skill.params.is_empty() {
        prompt = format!("{prompt}\n\n{args}");
    }
    // The caller (run_api_task) sends this on as a user message.
    ToolOutput::success(format!("[SKILL_PROMPT]\n{prompt}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The name became `<skills dir>/<name>.md` unchecked, so `../../x`
    /// read markdown from anywhere.
    #[tokio::test]
    async fn skill_names_with_path_separators_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("secret.md");
        std::fs::write(&outside, "leaked").unwrap();
        let cwd = dir.path().join("proj");
        std::fs::create_dir_all(cwd.join(".claude/skills")).unwrap();
        let abs = outside.with_extension("").to_string_lossy().into_owned();
        for name in ["../../../secret", "..\\..\\..\\secret", abs.as_str(), "a/b"] {
            let out = SkillTool
                .execute(json!({"skill": name}), &ToolContext::new(cwd.clone()))
                .await;
            let text = match out {
                Ok(o) => format!("{:?}", o.content),
                Err(e) => e.to_string(),
            };
            assert!(
                !text.contains("leaked"),
                "{name:?} read outside the skills dir"
            );
        }
    }

    fn text(out: &ToolOutput) -> String {
        format!("{:?}", out.content)
    }

    /// The tool re-read the file and only stripped frontmatter, so the model
    /// got `{{ARGS}}` / `{{param}}` verbatim and no param defaults.
    #[tokio::test]
    async fn placeholders_and_param_defaults_are_expanded_like_slash_skills() {
        let dir = tempfile::tempdir().unwrap();
        let global = dir.path().join("global");
        let local = dir.path().join("proj/.claude/skills");
        std::fs::create_dir_all(&global).unwrap();
        std::fs::create_dir_all(&local).unwrap();
        std::fs::write(
            local.join("deploy.md"),
            "---\nname: deploy\nparams:\n  env:\n    default: staging\n---\nDeploy to {{env}}.",
        )
        .unwrap();
        std::fs::write(
            local.join("legacy.md"),
            "# Legacy\nDesc\n---\nRun {{ARGS}} now",
        )
        .unwrap();
        std::fs::write(local.join("plain.md"), "Just do it").unwrap();
        let skills = crate::skills::load_skills_from(&global, &local).await;

        let out = invoke(&skills, "deploy", None);
        assert!(!out.is_error);
        assert!(text(&out).contains("Deploy to staging."), "{}", text(&out));
        assert!(text(&invoke(&skills, "deploy", Some("env=prod"))).contains("Deploy to prod."));

        let out = text(&invoke(&skills, "legacy", Some("the tests")));
        assert!(out.contains("Run the tests now"), "{out}");
        assert!(
            !out.contains("Legacy") && !out.contains("{{ARGS}}"),
            "{out}"
        );

        // No slot for args: they are appended instead of dropped.
        let out = text(&invoke(&skills, "plain", Some("in src/")));
        assert!(
            out.contains("Just do it") && out.contains("in src/"),
            "{out}"
        );
    }

    /// Bundled skills (/commit, /review, ...) were "not found" by the tool.
    #[tokio::test]
    async fn bundled_skills_are_found_and_unknown_names_are_errors() {
        let dir = tempfile::tempdir().unwrap();
        let skills =
            crate::skills::load_skills_from(&dir.path().join("g"), &dir.path().join("l")).await;
        let out = invoke(&skills, "commit", Some("--amend"));
        assert!(!out.is_error);
        assert!(text(&out).contains("git commit") && text(&out).contains("--amend"));
        assert!(invoke(&skills, "nope", None).is_error);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn skill_symlinked_to_a_private_key_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let key = dir.path().join("id_ed25519");
        std::fs::write(&key, "-----BEGIN OPENSSH PRIVATE KEY-----\nKEYBODY\n").unwrap();
        let skills = dir.path().join("proj/.claude/skills");
        std::fs::create_dir_all(&skills).unwrap();
        std::os::unix::fs::symlink(&key, skills.join("setup.md")).unwrap();
        std::fs::write(skills.join("ok.md"), "do the thing").unwrap();
        let skills = crate::skills::load_skills_from(&dir.path().join("global"), &skills).await;

        let out = invoke(&skills, "setup", None);
        assert!(out.is_error);
        assert!(!text(&out).contains("KEYBODY"));

        let out = invoke(&skills, "ok", None);
        assert!(!out.is_error && text(&out).contains("do the thing"));
    }
}
