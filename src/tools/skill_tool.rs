/// SkillTool — port of skill.ts
/// Looks up a skill by name from the skills registry and executes it.
/// Skills are the built-in set plus the `<name>/SKILL.md` and flat `.md`
/// skills `crate::skills::load_skills_in` finds — the same set `/name` runs.
use super::{Tool, ToolContext, ToolOutput, async_trait};
use crate::permissions::ReadDeny;
use crate::skills::Skill;
use anyhow::Result;
use serde::Deserialize;
use serde_json::json;
use std::collections::HashMap;

pub struct SkillTool;

#[derive(Deserialize)]
struct Input {
    /// Skill name, as DiscoverSkills lists it
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
        explain, fix, test) plus Agent Skills (<name>/SKILL.md) and markdown prompt \
        templates in the project's .agents/skills/, .oxideclaw/skills/ or .claude/skills/, \
        the config dir's skills/ or ~/.claude/skills/. Returns the skill's instructions; \
        follow them. Use DiscoverSkills to list available skills."
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

        if input.skill.is_empty() || input.skill.contains(['/', '\\']) || input.skill.contains("..")
        {
            return Ok(ToolOutput::error(
                "skill must be a bare name, as DiscoverSkills lists it, not a path",
            ));
        }

        // The tool returns the file's text with no Read check of its own,
        // so the user's Read deny rules apply to skill files here.
        let deny = ctx
            .permission_gate
            .as_ref()
            .map(|g| g.read_deny("Read"))
            .unwrap_or_default();
        let skills = crate::skills::load_skills_in(&ctx.cwd, &deny).await.skills;
        Ok(invoke(&skills, &input.skill, input.args.as_deref(), &deny))
    }
}

/// Expands the skill exactly as `/name` would, so `{{ARGS}}` and declared
/// params are filled in rather than reaching the model as literal
/// placeholders, and a `SKILL.md` body is read only now. The loader already
/// refuses skill files that link to key material or a denied file.
fn invoke(
    skills: &HashMap<String, Skill>,
    name: &str,
    args: Option<&str>,
    deny: &ReadDeny,
) -> ToolOutput {
    let Some(skill) = skills.get(name) else {
        return ToolOutput::error(format!(
            "Skill '{name}' not found in the built-in skills, .agents/skills/, \
            .oxideclaw/skills/, .claude/skills/, {} or ~/.claude/skills/.\n\
            Use DiscoverSkills to see available skills.",
            crate::config::Config::config_dir().join("skills").display()
        ));
    };
    match skill.invoke(args.unwrap_or(""), deny) {
        // The caller (run_api_task) sends this on as a user message.
        Ok(prompt) => ToolOutput::success(format!("[SKILL_PROMPT]\n{prompt}")),
        Err(why) => ToolOutput::error(format!("Skill '{name}' could not be loaded: {why}")),
    }
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

    fn no_deny() -> ReadDeny {
        ReadDeny::default()
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
        let skills =
            crate::skills::load_skills_at(&dir.path().join("proj"), &global, None, &no_deny())
                .await
                .skills;

        let out = invoke(&skills, "deploy", None, &no_deny());
        assert!(!out.is_error);
        assert!(text(&out).contains("Deploy to staging."), "{}", text(&out));
        assert!(
            text(&invoke(&skills, "deploy", Some("env=prod"), &no_deny()))
                .contains("Deploy to prod.")
        );

        let out = text(&invoke(&skills, "legacy", Some("the tests"), &no_deny()));
        assert!(out.contains("Run the tests now"), "{out}");
        assert!(
            !out.contains("Legacy") && !out.contains("{{ARGS}}"),
            "{out}"
        );

        // No slot for args: they are appended instead of dropped.
        let out = text(&invoke(&skills, "plain", Some("in src/"), &no_deny()));
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
            crate::skills::load_skills_at(dir.path(), &dir.path().join("g"), None, &no_deny())
                .await
                .skills;
        let out = invoke(&skills, "commit", Some("--amend"), &no_deny());
        assert!(!out.is_error);
        assert!(text(&out).contains("git commit") && text(&out).contains("--amend"));
        assert!(invoke(&skills, "nope", None, &no_deny()).is_error);
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
        let loaded = crate::skills::load_skills_at(
            &dir.path().join("proj"),
            &dir.path().join("global"),
            None,
            &no_deny(),
        )
        .await;
        assert_eq!(loaded.invalid.len(), 1, "the refused skill is reported");
        let skills = loaded.skills;

        let out = invoke(&skills, "setup", None, &no_deny());
        assert!(out.is_error);
        assert!(!text(&out).contains("KEYBODY"));

        let out = invoke(&skills, "ok", None, &no_deny());
        assert!(!out.is_error && text(&out).contains("do the thing"));
    }

    /// A repo's `.claude/skills/notes.md -> ../.env` was read with no check
    /// but the private-key names, so `Read(./.env)` did not stop the Skill
    /// tool returning the file or DiscoverSkills listing its first line.
    #[cfg(unix)]
    #[tokio::test]
    async fn skill_linked_to_a_read_denied_file_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let proj = dir.path().join("proj");
        let skills_dir = proj.join(".claude/skills");
        std::fs::create_dir_all(&skills_dir).unwrap();
        std::fs::write(proj.join(".env"), "OPENAI_API_KEY=sk-SECRET\n").unwrap();
        std::os::unix::fs::symlink("../../.env", skills_dir.join("notes.md")).unwrap();
        // A directory skill that becomes such a link after it was loaded.
        std::fs::create_dir_all(skills_dir.join("later")).unwrap();
        std::fs::write(
            skills_dir.join("later/SKILL.md"),
            "---\nname: later\ndescription: d\n---\nbody",
        )
        .unwrap();
        let deny = crate::permissions::PermissionState::new(false, &[], &["Read(./.env)".into()])
            .with_cwd(&proj)
            .read_deny("Read");

        let loaded =
            crate::skills::load_skills_at(&proj, &dir.path().join("cfg"), None, &deny).await;
        assert!(!loaded.skills.contains_key("notes"));
        let why = loaded.warning().unwrap();
        assert!(why.contains("permissions.deny"), "{why}");
        let listing = super::super::discover_skills::render(&loaded);
        assert!(!listing.contains("SECRET"), "{listing}");
        let out = invoke(&loaded.skills, "notes", None, &deny);
        assert!(out.is_error && !text(&out).contains("SECRET"));

        std::fs::remove_file(skills_dir.join("later/SKILL.md")).unwrap();
        std::os::unix::fs::symlink("../../../.env", skills_dir.join("later/SKILL.md")).unwrap();
        let out = invoke(&loaded.skills, "later", None, &deny);
        assert!(out.is_error, "{}", text(&out));
        assert!(!text(&out).contains("SECRET"), "{}", text(&out));

        // Without the rule the same link loads, so the rule is what refused it.
        let loaded =
            crate::skills::load_skills_at(&proj, &dir.path().join("cfg"), None, &no_deny()).await;
        assert!(loaded.skills.contains_key("notes"));
    }
}
