//! Skills system — markdown files that expand into prompts.
//!
//! Three formats supported:
//! 1. Agent Skills: a `<name>/SKILL.md` directory with YAML frontmatter
//!    (`name` and `description` required, other fields tolerated) and any
//!    supporting files. Only the frontmatter is read at load; the body is
//!    read when the skill runs.
//! 2. Legacy flat `<name>.md`: `# Title\nDescription\n---\nPrompt with {{ARGS}}`
//! 3. Flat `<name>.md` with YAML frontmatter (`name`, `description`,
//!    `category`, `params`)
//!
//! A skill is only a prompt. Frontmatter such as `allowed-tools` is accepted
//! but grants nothing: what the prompt makes the model do goes through the
//! same permission gate, sandbox and `disableSkillShellExecution` as any
//! other turn, so a cloned repository's skills get no more than its prompt.
use anyhow::Result;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tokio::fs;

/// Parsed YAML frontmatter parameter. `description` and `enum_values` are
/// loaded from the skill file but not yet displayed in `/skills` listings
/// or validated on invocation — retained so the parser stays lossless and
/// the listing UI can pick them up without a schema change.
#[derive(Debug, Clone)]
pub struct SkillParam {
    pub name: String,
    pub required: bool,
    pub default: Option<String>,
    #[allow(dead_code)]
    pub description: String,
    #[allow(dead_code)]
    pub enum_values: Option<Vec<String>>,
}

/// `category` is parsed from frontmatter but not yet grouped in any UI —
/// kept for the planned categorised `/skills` picker.
#[derive(Debug, Clone)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub prompt_template: String,
    #[allow(dead_code)]
    pub category: Option<String>,
    pub params: Vec<SkillParam>,
    /// The `SKILL.md` of a directory skill. Its body is read only by
    /// [`Skill::invoke`]; `prompt_template` is left empty.
    pub skill_file: Option<PathBuf>,
}

impl Skill {
    /// The prompt `/name args` and the Skill tool send. A directory skill's
    /// body is read from its `SKILL.md` now, and the prompt names the skill's
    /// directory so the model can read the files the body refers to. Args
    /// with no placeholder or param to fill are appended, not dropped.
    pub fn invoke(&self, args: &str) -> std::result::Result<String, String> {
        let args = args.trim();
        let loaded;
        let skill = match &self.skill_file {
            None => self,
            Some(file) => {
                loaded = parse_skill_md(&read_skill_file(file)?)?;
                &loaded
            }
        };
        let mut prompt = skill.expand_named(args);
        let has_slot = skill.prompt_template.contains("{{ARGS}}")
            || skill.prompt_template.contains("{{args}}");
        if !args.is_empty() && !has_slot && skill.params.is_empty() {
            prompt = format!("{prompt}\n\n{args}");
        }
        if let Some(dir) = self.skill_file.as_deref().and_then(Path::parent) {
            prompt = format!(
                "Base directory for this skill: {}\n\
                 Paths in the skill are relative to it; read those files only when you need them.\n\n{prompt}",
                dir.display()
            );
        }
        Ok(prompt)
    }

    /// Legacy expand: {{ARGS}} = raw argument string.
    pub fn expand(&self, args: &str) -> String {
        self.prompt_template
            .replace("{{ARGS}}", args)
            .replace("{{args}}", args)
    }

    /// Named-parameter expand: parses `key=value` tokens, applies defaults,
    /// replaces `{{key}}` in the template. Falls back to `expand()` when the
    /// skill has no declared params (full legacy compatibility).
    pub fn expand_named(&self, args: &str) -> String {
        if self.params.is_empty() {
            return self.expand(args);
        }
        let mut values: HashMap<String, String> = HashMap::new();
        let mut positional: Vec<String> = Vec::new();
        for token in shell_words(args) {
            if let Some(eq) = token.find('=') {
                values.insert(token[..eq].to_string(), token[eq + 1..].to_string());
            } else {
                positional.push(token);
            }
        }
        // Bind positional args to declared-required params in declaration order.
        let required_params: Vec<&SkillParam> = self.params.iter().filter(|p| p.required).collect();
        for (i, p) in required_params.iter().enumerate() {
            if !values.contains_key(&p.name)
                && let Some(v) = positional.get(i)
            {
                values.insert(p.name.clone(), v.clone());
            }
        }
        // Apply defaults for params still missing.
        for p in &self.params {
            if !values.contains_key(&p.name)
                && let Some(d) = &p.default
            {
                values.insert(p.name.clone(), d.clone());
            }
        }
        let mut out = self.prompt_template.clone();
        for (k, v) in &values {
            out = out.replace(&format!("{{{{{k}}}}}"), v);
        }
        // Legacy {{ARGS}} still gets the raw blob.
        out.replace("{{ARGS}}", args).replace("{{args}}", args)
    }
}

/// Split `args` on whitespace, honoring single and double quotes.
fn shell_words(s: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut in_quote = false;
    let mut q = '"';
    for ch in s.chars() {
        if in_quote {
            if ch == q {
                in_quote = false;
            } else {
                cur.push(ch);
            }
        } else if ch == '"' || ch == '\'' {
            in_quote = true;
            q = ch;
        } else if ch.is_whitespace() {
            if !cur.is_empty() {
                words.push(std::mem::take(&mut cur));
            }
        } else {
            cur.push(ch);
        }
    }
    if !cur.is_empty() {
        words.push(cur);
    }
    words
}

/// Parse a skill from content. Dispatches on `---` frontmatter header.
pub fn parse_skill_from_content(content: &str, fallback_name: &str) -> Result<Skill> {
    let trimmed = content.trim_start();
    if trimmed.starts_with("---\n") || trimmed.starts_with("---\r\n") {
        parse_yaml_skill(trimmed, fallback_name)
    } else {
        parse_legacy_skill(content, fallback_name)
    }
}

fn parse_yaml_skill(content: &str, fallback_name: &str) -> Result<Skill> {
    // Strip opening `---\n` or `---\r\n`.
    let after_first = content
        .strip_prefix("---\r\n")
        .or_else(|| content.strip_prefix("---\n"))
        .ok_or_else(|| anyhow::anyhow!("Expected YAML frontmatter delimiter"))?;
    // An empty frontmatter block (`---` immediately followed by `---`) has
    // its closing delimiter at offset 0, with no preceding newline.
    let (end, prompt_start) = if after_first.starts_with("---\r\n") {
        (0, 5)
    } else if after_first.starts_with("---\n") {
        (0, 4)
    } else {
        let end = after_first
            .find("\n---\n")
            .or_else(|| after_first.find("\n---\r\n"))
            .ok_or_else(|| anyhow::anyhow!("No closing --- in YAML frontmatter"))?;
        // Skip past `\n---\n` or `\n---\r\n`.
        let skip = if after_first[end..].starts_with("\n---\r\n") {
            6
        } else {
            5
        };
        (end, end + skip)
    };
    let yaml_str = &after_first[..end];
    let prompt = after_first[prompt_start..].trim().to_string();

    let yaml: serde_yaml::Value = serde_yaml::from_str(yaml_str)?;

    let name = yaml["name"]
        .as_str()
        .unwrap_or(fallback_name)
        .to_lowercase()
        .replace(' ', "-");
    let description = yaml["description"].as_str().unwrap_or("").to_string();
    let category = yaml["category"].as_str().map(|s| s.to_string());

    let mut params = Vec::new();
    if let Some(map) = yaml["params"].as_mapping() {
        for (k, v) in map {
            let name = k.as_str().unwrap_or("").to_string();
            if name.is_empty() {
                continue;
            }
            let required = v["required"].as_bool().unwrap_or(false);
            // Accept string / number / bool as default, stringify.
            let default = match &v["default"] {
                serde_yaml::Value::String(s) => Some(s.clone()),
                serde_yaml::Value::Number(n) => Some(n.to_string()),
                serde_yaml::Value::Bool(b) => Some(b.to_string()),
                _ => None,
            };
            let desc = v["description"].as_str().unwrap_or("").to_string();
            let enum_values = v["enum"].as_sequence().map(|seq| {
                seq.iter()
                    .filter_map(|x| x.as_str().map(|s| s.to_string()))
                    .collect()
            });
            params.push(SkillParam {
                name,
                required,
                default,
                description: desc,
                enum_values,
            });
        }
    }

    Ok(Skill {
        name,
        description,
        prompt_template: prompt,
        category,
        params,
        skill_file: None,
    })
}

fn parse_legacy_skill(content: &str, fallback_name: &str) -> Result<Skill> {
    let name = fallback_name.to_lowercase().replace(' ', "-");
    let (description, prompt_template) = if let Some(sep) = content.find("\n---\n") {
        let desc = content[..sep]
            .trim()
            .lines()
            .filter(|l| !l.starts_with('#'))
            .collect::<Vec<_>>()
            .join("\n")
            .trim()
            .to_string();
        (desc, content[sep + 5..].trim().to_string())
    } else if let Some(sep) = content.find("\n---\r\n") {
        let desc = content[..sep]
            .trim()
            .lines()
            .filter(|l| !l.starts_with('#'))
            .collect::<Vec<_>>()
            .join("\n")
            .trim()
            .to_string();
        (desc, content[sep + 6..].trim().to_string())
    } else {
        (name.clone(), content.trim().to_string())
    };
    Ok(Skill {
        name,
        description,
        prompt_template,
        category: None,
        params: Vec::new(),
        skill_file: None,
    })
}

/// Check if an input is a skill invocation `/name [args]`.
pub fn parse_skill_invocation(input: &str) -> Option<(&str, &str)> {
    let input = input.trim();
    if !input.starts_with('/') {
        return None;
    }
    let rest = &input[1..];
    // Any whitespace, as commands::dispatch splits the name: Shift+Enter
    // puts a newline right after it, and splitting on ' ' alone made the
    // key "fix\nthe" and reported a real skill as an unknown command.
    if let Some(sp) = rest.find(char::is_whitespace) {
        Some((&rest[..sp], rest[sp..].trim()))
    } else {
        Some((rest, ""))
    }
}

/// The skills one project can run, and the skill files skipped as invalid.
pub struct LoadedSkills {
    pub skills: HashMap<String, Skill>,
    /// Each skipped skill file with the reason, in load order.
    pub invalid: Vec<(PathBuf, String)>,
}

impl LoadedSkills {
    /// One notice naming every skipped skill file, or `None` when all loaded.
    pub fn warning(&self) -> Option<String> {
        if self.invalid.is_empty() {
            return None;
        }
        let lines: Vec<String> = self
            .invalid
            .iter()
            .map(|(path, why)| format!("  {} — {why}", path.display()))
            .collect();
        Some(format!(
            "Skipped {} invalid skill(s); a SKILL.md needs YAML frontmatter with `name` and `description`:\n{}",
            self.invalid.len(),
            lines.join("\n")
        ))
    }
}

/// Load the skills for a project rooted at `cwd`: the bundled set plus every
/// skill directory [`skill_dirs`] names. `/name`, the Skill tool and
/// DiscoverSkills all go through this, so the model sees exactly the skills
/// `/name` runs.
pub async fn load_skills_in(cwd: &Path) -> LoadedSkills {
    load_skills_at(
        cwd,
        &crate::config::Config::config_dir(),
        dirs::home_dir().filter(|h| h.is_absolute()).as_deref(),
    )
    .await
}

/// Where skills are looked up, highest priority first, each with whether it
/// also holds legacy flat `<name>.md` skills. `.agents/skills` is the
/// cross-agent standard directory, where only `<name>/SKILL.md` is a skill
/// (a README.md there is not one). `.claude/skills` and `~/.claude/skills`
/// are Claude Code's; OxideClaw only reads them.
fn skill_dirs(cwd: &Path, config_dir: &Path, home: Option<&Path>) -> Vec<(PathBuf, bool)> {
    let mut dirs = vec![
        (cwd.join(".agents").join("skills"), false),
        (cwd.join(".oxideclaw").join("skills"), true),
        (cwd.join(".claude").join("skills"), true),
        (config_dir.join("skills"), true),
    ];
    if let Some(home) = home {
        let claude = home.join(".claude").join("skills");
        if !dirs.iter().any(|(d, _)| *d == claude) {
            dirs.push((claude, true));
        }
    }
    dirs
}

/// [`load_skills_in`] with the config and home directories passed in. On a
/// name collision the earlier directory wins, then the earlier path within
/// one directory; bundled skills only fill names nothing else uses.
pub(crate) async fn load_skills_at(
    cwd: &Path,
    config_dir: &Path,
    home: Option<&Path>,
) -> LoadedSkills {
    let mut skills = HashMap::new();
    let mut invalid = Vec::new();
    for (dir, flat) in skill_dirs(cwd, config_dir, home) {
        let Ok(mut entries) = fs::read_dir(&dir).await else {
            continue;
        };
        let mut paths = Vec::new();
        while let Ok(Some(entry)) = entries.next_entry().await {
            paths.push(entry.path());
        }
        paths.sort();
        for path in paths {
            let (file, skill) = if fs::metadata(&path).await.is_ok_and(|m| m.is_dir()) {
                // A folder without SKILL.md is not a skill (and says nothing
                // worth a warning); one with it must be valid.
                let file = path.join("SKILL.md");
                if fs::symlink_metadata(&file).await.is_err() {
                    continue;
                }
                let skill = read_skill_file(&file)
                    .and_then(|c| parse_skill_md(&c))
                    .map(|mut s| {
                        // Progressive disclosure: only name and description
                        // stay loaded; `invoke` reads the body.
                        s.prompt_template.clear();
                        s.skill_file = Some(file.clone());
                        s
                    });
                (file, skill)
            } else if flat && path.extension().and_then(|e| e.to_str()) == Some("md") {
                let fallback = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("unnamed")
                    .to_string();
                let skill = read_skill_file(&path).and_then(|c| {
                    parse_skill_from_content(&c, &fallback)
                        .map_err(|e| format!("malformed frontmatter: {e}"))
                });
                (path, skill)
            } else {
                continue;
            };
            match skill {
                Ok(skill) => {
                    skills.entry(skill.name.clone()).or_insert(skill);
                }
                Err(why) => invalid.push((file, why)),
            }
        }
    }
    for s in bundled_skills() {
        skills.entry(s.name.clone()).or_insert(s);
    }
    LoadedSkills { skills, invalid }
}

/// Read a skill file through Read's deny-list: a skill that links to key
/// material must not become a prompt.
fn read_skill_file(path: &Path) -> std::result::Result<String, String> {
    if crate::tools::check_sensitive_path_resolved(path, crate::tools::SensitiveOp::Read).is_some()
    {
        return Err("refused: it is or links to a file on the sensitive-file deny-list".into());
    }
    std::fs::read_to_string(path).map_err(|e| format!("unreadable: {e}"))
}

/// Parse an Agent Skills `SKILL.md`: frontmatter with a `name` usable as a
/// `/name` command and a non-empty `description` is required.
fn parse_skill_md(content: &str) -> std::result::Result<Skill, String> {
    let content = content.trim_start_matches('\u{feff}').trim_start();
    if !(content.starts_with("---\n") || content.starts_with("---\r\n")) {
        return Err("no YAML frontmatter".into());
    }
    let skill = parse_yaml_skill(content, "").map_err(|e| format!("malformed frontmatter: {e}"))?;
    if skill.name.is_empty() {
        return Err("frontmatter has no `name`".into());
    }
    if skill
        .name
        .contains(|c: char| c.is_whitespace() || c == '/' || c == '\\')
    {
        return Err(format!(
            "`name: {}` is not a single command word",
            skill.name
        ));
    }
    if skill.description.trim().is_empty() {
        return Err("frontmatter has no `description`".into());
    }
    Ok(skill)
}

fn bundled_skills() -> Vec<Skill> {
    let code = Some("code".to_string());
    vec![
        Skill {
            name: "commit".into(),
            description: "Create a git commit with a well-formatted message".into(),
            prompt_template: "Please create a git commit for the current staged changes. \
                Follow conventional commit format. Run git diff --staged first to see the changes, \
                then write a commit message and run git commit. {{ARGS}}"
                .into(),
            category: code.clone(),
            params: vec![],
            skill_file: None,
        },
        Skill {
            name: "review".into(),
            description: "Review code changes for quality and correctness".into(),
            prompt_template: "Please review the following code/changes for correctness, \
                quality, potential bugs, and style issues. Be specific about any problems found. \
                {{ARGS}}"
                .into(),
            category: code.clone(),
            params: vec![],
            skill_file: None,
        },
        Skill {
            name: "explain".into(),
            description: "Explain how a piece of code works".into(),
            prompt_template: "Please explain how the following code works, including its \
                purpose, key logic, and any non-obvious design decisions. {{ARGS}}"
                .into(),
            category: code.clone(),
            params: vec![],
            skill_file: None,
        },
        Skill {
            name: "fix".into(),
            description: "Find and fix a bug or error".into(),
            prompt_template: "Please investigate and fix the following issue. Read relevant \
                files first, diagnose the root cause, then make the minimal change to fix it. \
                {{ARGS}}"
                .into(),
            category: code.clone(),
            params: vec![],
            skill_file: None,
        },
        Skill {
            name: "test".into(),
            description: "Write tests for a function or module".into(),
            prompt_template: "Please write comprehensive tests for the following. Include \
                unit tests for edge cases and happy paths. {{ARGS}}"
                .into(),
            category: code,
            params: vec![],
            skill_file: None,
        },
    ]
}

#[cfg(test)]
mod frontmatter_tests {
    use super::parse_skill_from_content;

    #[test]
    fn malformed_frontmatter_is_an_error_not_a_panic() {
        assert!(parse_skill_from_content("---\nname: [oops\n---\nbody", "x").is_err());
        assert!(parse_skill_from_content("---\nname: never-closed\n", "x").is_err());
        assert!(
            parse_skill_from_content("---\n---\n", "x").is_ok(),
            "empty frontmatter is fine"
        );
    }

    #[test]
    fn crlf_frontmatter_parses() {
        let s =
            parse_skill_from_content("---\r\nname: Win Skill\r\n---\r\nbody here", "x").unwrap();
        assert_eq!(s.name, "win-skill");
        assert_eq!(s.prompt_template, "body here");
    }

    #[test]
    fn params_that_are_not_a_mapping_are_ignored() {
        let s = parse_skill_from_content("---\nname: p\nparams: [a, b]\n---\nbody", "x").unwrap();
        assert!(s.params.is_empty());
    }

    #[test]
    fn unicode_next_to_the_delimiters_does_not_break_slicing() {
        let s = parse_skill_from_content("---\nname: é\ndescription: ü\n---\n日本語 {{ARGS}}", "x")
            .unwrap();
        assert_eq!(s.name, "é");
        assert!(s.expand("x").starts_with("日本語"));
    }
}

#[cfg(test)]
mod tests {
    use super::{load_skills_at, parse_skill_invocation};
    use std::path::Path;

    fn write(path: &Path, content: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    fn skill_md(name: &str, description: &str, body: &str) -> String {
        format!("---\nname: {name}\ndescription: {description}\n---\n{body}")
    }

    /// The five skill directories, highest priority first, under one temp dir:
    /// project, config dir and home are siblings.
    fn locations(root: &Path) -> [std::path::PathBuf; 5] {
        [
            root.join("proj/.agents/skills"),
            root.join("proj/.oxideclaw/skills"),
            root.join("proj/.claude/skills"),
            root.join("xdg/oxideclaw/skills"),
            root.join("home/.claude/skills"),
        ]
    }

    async fn load(root: &Path) -> super::LoadedSkills {
        load_skills_at(
            &root.join("proj"),
            &root.join("xdg/oxideclaw"),
            Some(&root.join("home")),
        )
        .await
    }

    #[tokio::test]
    async fn project_skills_override_global_and_bundled_ones() {
        let dir = tempfile::tempdir().unwrap();
        let [_, _, local, global, claude_code] = locations(dir.path());
        write(&global.join("deploy.md"), "global deploy");
        write(&global.join("both.md"), "global copy");
        write(&local.join("both.md"), "project copy");
        write(&local.join("commit.md"), "project commit");
        // Claude Code's ~/.claude/skills is read too, below ours.
        write(&claude_code.join("deploy.md"), "claude code deploy");
        write(&claude_code.join("lint.md"), "claude code lint");

        let skills = load(dir.path()).await.skills;
        assert_eq!(skills["lint"].prompt_template, "claude code lint");
        assert_eq!(skills["deploy"].prompt_template, "global deploy");
        assert_eq!(skills["both"].prompt_template, "project copy");
        assert_eq!(skills["commit"].prompt_template, "project commit");
        assert!(skills.contains_key("review"), "bundled skills are kept");
    }

    /// Only flat `*.md` files were read, so a `<name>/SKILL.md` skill in any
    /// directory was dropped without a word.
    #[tokio::test]
    async fn a_skill_md_skill_loads_from_every_location() {
        let dir = tempfile::tempdir().unwrap();
        for (i, loc) in locations(dir.path()).iter().enumerate() {
            write(
                &loc.join(format!("s{i}/SKILL.md")),
                &skill_md(&format!("s{i}"), &format!("Skill {i}"), "body"),
            );
        }
        let loaded = load(dir.path()).await;
        assert!(loaded.invalid.is_empty(), "{:?}", loaded.invalid);
        for i in 0..5 {
            let skill = &loaded.skills[&format!("s{i}")];
            assert_eq!(skill.description, format!("Skill {i}"));
        }
    }

    /// Progressive disclosure: loading keeps only name and description; the
    /// body (read fresh) and the skill's directory arrive on invocation.
    #[tokio::test]
    async fn a_skill_md_body_is_read_only_when_invoked() {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = locations(dir.path())[0].join("release");
        write(
            &skill_dir.join("SKILL.md"),
            "---\nname: release\ndescription: Cut a release\nlicense: MIT\nallowed-tools: Bash\n---\n\
             Follow scripts/release.sh. {{ARGS}}",
        );
        write(&skill_dir.join("scripts/release.sh"), "echo SCRIPT-BODY");

        let skills = load(dir.path()).await.skills;
        let skill = &skills["release"];
        assert_eq!(skill.description, "Cut a release");
        assert!(skill.prompt_template.is_empty(), "body loaded eagerly");

        write(
            &skill_dir.join("SKILL.md"),
            "---\nname: release\ndescription: Cut a release\n---\nFollow scripts/release.sh v2. {{ARGS}}",
        );
        let prompt = skill.invoke("1.2.3").unwrap();
        assert!(
            prompt.contains("Follow scripts/release.sh v2. 1.2.3"),
            "{prompt}"
        );
        assert!(
            prompt.contains(&format!(
                "Base directory for this skill: {}",
                skill_dir.display()
            )),
            "{prompt}"
        );
        assert!(
            !prompt.contains("SCRIPT-BODY"),
            "supporting files are read on demand"
        );

        // Args the body has no slot for are appended, not dropped.
        write(
            &skill_dir.join("SKILL.md"),
            &skill_md("release", "d", "No slot."),
        );
        let prompt = skill.invoke("for staging").unwrap();
        assert!(prompt.ends_with("No slot.\n\nfor staging"), "{prompt}");

        std::fs::remove_file(skill_dir.join("SKILL.md")).unwrap();
        assert!(skill.invoke("").unwrap_err().contains("unreadable"));
    }

    #[tokio::test]
    async fn legacy_flat_skills_still_load_but_not_from_agents_skills() {
        let dir = tempfile::tempdir().unwrap();
        let [agents, oxide, claude, config, home] = locations(dir.path());
        write(&agents.join("README.md"), "About these skills");
        write(&oxide.join("a.md"), "# A\nDo A\n---\nRun A {{ARGS}}");
        write(&claude.join("b.md"), "Do B");
        write(&config.join("c.md"), &skill_md("c", "Do C", "Run C"));
        write(&home.join("d.md"), "Do D");

        let loaded = load(dir.path()).await;
        assert!(loaded.invalid.is_empty(), "{:?}", loaded.invalid);
        let skills = loaded.skills;
        assert!(!skills.contains_key("readme"));
        assert_eq!(skills["a"].description, "Do A");
        assert_eq!(skills["a"].expand("x"), "Run A x");
        assert_eq!(skills["b"].prompt_template, "Do B");
        assert_eq!(skills["c"].prompt_template, "Run C");
        assert_eq!(skills["d"].prompt_template, "Do D");
        assert_eq!(skills["b"].invoke("now").unwrap(), "Do B\n\nnow");
    }

    #[tokio::test]
    async fn invalid_skills_are_skipped_with_one_warning_naming_each_path() {
        let dir = tempfile::tempdir().unwrap();
        let [agents, _, claude, _, _] = locations(dir.path());
        write(&agents.join("plain/SKILL.md"), "Just a body");
        write(
            &agents.join("noname/SKILL.md"),
            "---\ndescription: d\n---\nbody",
        );
        write(
            &agents.join("nodesc/SKILL.md"),
            "---\nname: nodesc\n---\nbody",
        );
        write(
            &agents.join("badyaml/SKILL.md"),
            "---\nname: [oops\n---\nbody",
        );
        write(&claude.join("flat.md"), "---\nname: never-closed\n");
        // Not a skill at all: no warning.
        write(&agents.join("notes/todo.txt"), "x");
        write(
            &agents.join("good/SKILL.md"),
            &skill_md("good", "Fine", "body"),
        );

        let loaded = load(dir.path()).await;
        assert!(loaded.skills.contains_key("good"));
        for name in [
            "plain",
            "noname",
            "nodesc",
            "badyaml",
            "never-closed",
            "notes",
        ] {
            assert!(!loaded.skills.contains_key(name), "{name} loaded");
        }
        let warning = loaded.warning().unwrap();
        assert!(
            warning.starts_with("Skipped 5 invalid skill(s)"),
            "{warning}"
        );
        assert_eq!(warning.lines().count(), 6, "{warning}");
        for (path, why) in [
            (agents.join("plain/SKILL.md"), "no YAML frontmatter"),
            (agents.join("noname/SKILL.md"), "no `name`"),
            (agents.join("nodesc/SKILL.md"), "no `description`"),
            (agents.join("badyaml/SKILL.md"), "malformed frontmatter"),
            (claude.join("flat.md"), "malformed frontmatter"),
        ] {
            let line = warning
                .lines()
                .find(|l| l.contains(&path.display().to_string()))
                .unwrap_or_else(|| panic!("{} missing from {warning}", path.display()));
            assert!(line.contains(why), "{line}");
        }
        assert!(!warning.contains("notes"), "{warning}");
    }

    /// The frontmatter `name`, not the folder, is the command; the higher
    /// priority directory wins, and bundled skills come last.
    #[tokio::test]
    async fn the_first_location_wins_a_name_collision() {
        let dir = tempfile::tempdir().unwrap();
        let locs = locations(dir.path());
        for (i, loc) in locs.iter().enumerate() {
            // Every location defines `shared`; `from{j}` is defined in
            // location j and every lower-priority one, so j must win it.
            write(
                &loc.join(format!("folder{i}/SKILL.md")),
                &skill_md("shared", &format!("from location {i}"), "b"),
            );
            for j in 0..=i {
                write(
                    &loc.join(format!("f{j}/SKILL.md")),
                    &skill_md(&format!("from{j}"), &format!("location {i}"), "b"),
                );
            }
        }
        write(
            &locs[4].join("commit/SKILL.md"),
            &skill_md("commit", "home commit", "b"),
        );
        // Within one directory the earlier path wins.
        write(&locs[1].join("aa.md"), &skill_md("dup", "aa", "b"));
        write(&locs[1].join("bb/SKILL.md"), &skill_md("dup", "bb", "b"));

        let skills = load(dir.path()).await.skills;
        assert_eq!(skills["shared"].description, "from location 0");
        for j in 0..5 {
            assert_eq!(
                skills[&format!("from{j}")].description,
                format!("location {j}")
            );
        }
        assert_eq!(skills["commit"].description, "home commit");
        assert_eq!(skills["dup"].description, "aa");
    }

    #[tokio::test]
    async fn home_claude_skills_is_read_once_when_it_is_the_config_dir() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        write(
            &home.join(".claude/skills/x/SKILL.md"),
            &skill_md("x", "X", "b"),
        );
        let dirs = super::skill_dirs(&dir.path().join("proj"), &home.join(".claude"), Some(&home));
        assert_eq!(dirs.len(), 4, "{dirs:?}");
        let loaded =
            load_skills_at(&dir.path().join("proj"), &home.join(".claude"), Some(&home)).await;
        assert_eq!(loaded.skills["x"].description, "X");
    }

    #[test]
    fn skill_name_ends_at_any_whitespace() {
        assert_eq!(
            parse_skill_invocation("/fix\nthe login button\ndoes nothing"),
            Some(("fix", "the login button\ndoes nothing"))
        );
        assert_eq!(parse_skill_invocation("/fix\targs"), Some(("fix", "args")));
        assert_eq!(parse_skill_invocation("/fix  a b"), Some(("fix", "a b")));
        assert_eq!(parse_skill_invocation("/fix\u{3000}x"), Some(("fix", "x")));
        assert_eq!(parse_skill_invocation("/fix"), Some(("fix", "")));
        assert_eq!(parse_skill_invocation("fix"), None);
    }
}
