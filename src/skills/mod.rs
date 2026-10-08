//! Skills system — markdown files that expand into prompts.
//!
//! Three formats supported:
//! 1. Agent Skills: a `<name>/SKILL.md` directory with YAML frontmatter
//!    (`name`, `description`; other fields tolerated) and any supporting
//!    files. As in Claude Code every field is optional: the name defaults to
//!    the folder's and the description to the body's first paragraph. Only
//!    the name and description stay loaded; the body is read when the skill
//!    runs.
//! 2. Legacy flat `<name>.md`: `# Title\nDescription\n---\nPrompt with {{ARGS}}`
//! 3. Flat `<name>.md` with YAML frontmatter (`name`, `description`,
//!    `category`, `params`)
//!
//! A skill is only a prompt. Frontmatter such as `allowed-tools` is accepted
//! but grants nothing: what the prompt makes the model do goes through the
//! same permission gate, sandbox and `disableSkillShellExecution` as any
//! other turn, so a cloned repository's skills get no more than its prompt.
use crate::permissions::ReadDeny;
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
    /// Claude Code's `disable-model-invocation: true`: only the user may run
    /// it (`/name`). The flag marks side-effecting workflows (deploy,
    /// release), so DiscoverSkills hides the skill and the Skill tool
    /// refuses it.
    pub disable_model_invocation: bool,
}

impl Skill {
    /// The prompt `/name args` and the Skill tool send. A directory skill's
    /// body is read from its `SKILL.md` now, and the prompt names the skill's
    /// directory so the model can read the files the body refers to. Args
    /// with no placeholder or param to fill are appended, not dropped.
    /// `deny` is the session's Read deny rules: the file is read again here,
    /// and it may have become a link to a denied file since it was loaded.
    pub fn invoke(&self, args: &str, deny: &ReadDeny) -> std::result::Result<String, String> {
        let args = args.trim();
        let loaded;
        let skill = match &self.skill_file {
            None => self,
            Some(file) => {
                let dir_name = file
                    .parent()
                    .and_then(Path::file_name)
                    .and_then(|n| n.to_str())
                    .unwrap_or("");
                loaded = parse_skill_md(&read_skill_file(file, deny)?, dir_name)?;
                &loaded
            }
        };
        let skill_dir = self.skill_file.as_deref().and_then(Path::parent);
        let (template, dollar_slot) = expand_claude_args(&skill.prompt_template, args, skill_dir);
        let skill = Skill {
            prompt_template: template,
            ..skill.clone()
        };
        let mut prompt = skill.expand_named(args);
        let has_slot = dollar_slot
            || skill.prompt_template.contains("{{ARGS}}")
            || skill.prompt_template.contains("{{args}}");
        if !args.is_empty() && !has_slot && skill.params.is_empty() {
            prompt = format!("{prompt}\n\n{args}");
        }
        if let Some(dir) = skill_dir {
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

/// Fill Claude Code's placeholders, so its skills work as they are:
/// `$ARGUMENTS` is the raw args, `$ARGUMENTS[N]` and `$N` the Nth word
/// (from 0), `${CLAUDE_SKILL_DIR}` the skill's folder. Returns whether the
/// template had an argument slot, so the args are not appended as well.
/// A bare `$N` past the last word is left alone: it is as likely an `awk`
/// or shell `$1` in the skill's own instructions as a placeholder.
fn expand_claude_args(template: &str, args: &str, dir: Option<&Path>) -> (String, bool) {
    if !template.contains('$') {
        return (template.to_string(), false);
    }
    let words = shell_words(args);
    let mut out = String::with_capacity(template.len());
    let mut slot = false;
    let mut rest = template;
    while let Some(i) = rest.find('$') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        if let Some(r) = after.strip_prefix("{CLAUDE_SKILL_DIR}")
            && let Some(dir) = dir
        {
            out.push_str(&dir.display().to_string());
            rest = r;
            continue;
        }
        if let Some(r) = after.strip_prefix("ARGUMENTS") {
            if let Some(index) = r.strip_prefix('[')
                && let Some(close) = index.find(']')
                && let Ok(n) = index[..close].trim().parse::<usize>()
            {
                slot = true;
                out.push_str(words.get(n).map_or("", String::as_str));
                rest = &index[close + 1..];
                continue;
            }
            if !r.starts_with(|c: char| c.is_alphanumeric() || c == '_') {
                slot = true;
                out.push_str(args);
                rest = r;
                continue;
            }
        }
        let digits = after.len() - after.trim_start_matches(|c: char| c.is_ascii_digit()).len();
        if digits > 0
            && let Ok(n) = after[..digits].parse::<usize>()
            && let Some(word) = words.get(n)
        {
            slot = true;
            out.push_str(word);
            rest = &after[digits..];
            continue;
        }
        out.push('$');
        rest = after;
    }
    out.push_str(rest);
    (out, slot)
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
    // A quoted `"true"` must not silently leave the skill open to the model.
    let disable_model_invocation = match &yaml["disable-model-invocation"] {
        serde_yaml::Value::Bool(b) => *b,
        serde_yaml::Value::String(v) => v.trim().eq_ignore_ascii_case("true"),
        _ => false,
    };

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
        disable_model_invocation,
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
        disable_model_invocation: false,
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
    /// Skills named like a built-in command, with their file: `/name` runs
    /// the built-in, so only the Skill tool can run them.
    pub shadowed: Vec<(PathBuf, String)>,
}

impl LoadedSkills {
    /// One notice naming every skipped skill file and every skill a built-in
    /// command hides, or `None` when there is neither.
    pub fn warning(&self) -> Option<String> {
        let mut parts = Vec::new();
        if !self.invalid.is_empty() {
            let lines: Vec<String> = self
                .invalid
                .iter()
                .map(|(path, why)| format!("  {} — {why}", path.display()))
                .collect();
            parts.push(format!(
                "Skipped {} invalid skill(s):\n{}",
                self.invalid.len(),
                lines.join("\n")
            ));
        }
        if !self.shadowed.is_empty() {
            let lines: Vec<String> = self
                .shadowed
                .iter()
                .map(|(path, name)| format!("  /{name} — {}", path.display()))
                .collect();
            parts.push(format!(
                "These skills share a name with a built-in command, so /name runs the built-in; rename the skill to run it as a command:\n{}",
                lines.join("\n")
            ));
        }
        (!parts.is_empty()).then(|| parts.join("\n\n"))
    }
}

/// Whether a built-in slash command owns `name`: commands are matched before
/// skills, so `/name` never reaches a skill called that.
pub fn is_builtin_command(name: &str) -> bool {
    crate::commands::SLASH_COMMANDS.contains(&name)
}

/// Load the skills for a project rooted at `cwd`: the bundled set plus every
/// skill directory [`skill_dirs`] names. `/name`, the Skill tool and
/// DiscoverSkills all go through this, so the model sees exactly the skills
/// `/name` runs. Skill files `deny` (the Read deny rules) covers are
/// skipped as invalid.
pub async fn load_skills_in(cwd: &Path, deny: &ReadDeny) -> LoadedSkills {
    load_skills_at(
        cwd,
        &crate::config::Config::config_dir(),
        dirs::home_dir().filter(|h| h.is_absolute()).as_deref(),
        deny,
    )
    .await
}

/// Where skills are looked up, highest priority first, each with whether it
/// also holds legacy flat `<name>.md` skills. `.agents/skills` is the
/// cross-agent standard directory, where only `<name>/SKILL.md` is a skill
/// (a README.md there is not one). `.claude/skills` and `~/.claude/skills`
/// are Claude Code's; OxideClaw only reads them. The project directories are
/// read at every level from `cwd` up to the repo root, nearest first.
fn skill_dirs(cwd: &Path, config_dir: &Path, home: Option<&Path>) -> Vec<(PathBuf, bool)> {
    let mut candidates = Vec::new();
    for level in project_levels(cwd, home) {
        candidates.push((level.join(".agents").join("skills"), false));
        candidates.push((level.join(".oxideclaw").join("skills"), true));
        candidates.push((level.join(".claude").join("skills"), true));
    }
    candidates.push((config_dir.join("skills"), true));
    if let Some(home) = home {
        candidates.push((home.join(".claude").join("skills"), true));
    }
    // Run from $HOME with the default config dir and `.claude/skills`, the
    // config dir's and `~/.claude/skills` are one directory. Read it once, at
    // its highest priority, or each invalid skill in it is reported twice.
    let mut dirs: Vec<(PathBuf, bool)> = Vec::new();
    for (dir, flat) in candidates {
        if !dirs.iter().any(|(d, _)| *d == dir) {
            dirs.push((dir, flat));
        }
    }
    dirs
}

/// `cwd` and its parents up to the git work-tree root (a `.git` file counts,
/// for worktrees), so a monorepo's root skills load in `packages/web` too.
/// With no `.git` below `home` or the filesystem root only `cwd` is used,
/// so a stray folder's skills above an unversioned directory never load.
fn project_levels(cwd: &Path, home: Option<&Path>) -> Vec<PathBuf> {
    let mut levels = Vec::new();
    for dir in cwd.ancestors() {
        if Some(dir) == home && dir != cwd {
            break;
        }
        levels.push(dir.to_path_buf());
        if std::fs::symlink_metadata(dir.join(".git")).is_ok() {
            return levels;
        }
        if Some(dir) == home {
            break;
        }
    }
    vec![cwd.to_path_buf()]
}

/// [`load_skills_in`] with the config and home directories passed in. On a
/// name collision the earlier directory wins, then the earlier path within
/// one directory; bundled skills only fill names nothing else uses. The
/// bundled set has no `commit` or `review`: those are built-in commands.
pub(crate) async fn load_skills_at(
    cwd: &Path,
    config_dir: &Path,
    home: Option<&Path>,
    deny: &ReadDeny,
) -> LoadedSkills {
    let mut skills = HashMap::new();
    let mut invalid = Vec::new();
    let mut shadowed = Vec::new();
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
                let dir_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                let skill = read_skill_file(&file, deny)
                    .and_then(|c| parse_skill_md(&c, dir_name))
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
                let skill = read_skill_file(&path, deny).and_then(|c| {
                    parse_skill_from_content(&c, &fallback)
                        .map_err(|e| format!("malformed frontmatter: {e}"))
                });
                (path, skill)
            } else {
                continue;
            };
            match skill {
                Ok(skill) => {
                    if !skills.contains_key(&skill.name) {
                        if is_builtin_command(&skill.name) {
                            shadowed.push((file, skill.name.clone()));
                        }
                        skills.insert(skill.name.clone(), skill);
                    }
                }
                Err(why) => invalid.push((file, why)),
            }
        }
    }
    for s in bundled_skills() {
        skills.entry(s.name.clone()).or_insert(s);
    }
    LoadedSkills {
        skills,
        invalid,
        shadowed,
    }
}

/// Read a skill file through Read's deny-list and the user's Read deny
/// rules: a skill that is or links to key material or a denied file (a
/// repo's `notes.md -> ../.env`) must not become a prompt, since the Skill
/// tool hands it to the model with no permission check of its own.
fn read_skill_file(path: &Path, deny: &ReadDeny) -> std::result::Result<String, String> {
    if deny.denies(path) {
        return Err("refused: blocked by a permissions.deny Read rule".into());
    }
    if crate::tools::check_sensitive_path_resolved(path, crate::tools::SensitiveOp::Read).is_some()
    {
        return Err("refused: it is or links to a file on the sensitive-file deny-list".into());
    }
    // Repo-controlled and read before any /trust: a FIFO would hang startup
    // and a link to /dev/zero read until out of memory. Regular files only,
    // capped in size.
    match crate::settings::read_config_file(path) {
        Ok(Some(text)) => Ok(text),
        Ok(None) => Err("unreadable: not found".into()),
        Err(e) => Err(format!("unreadable: {e}")),
    }
}

/// Parse an Agent Skills `SKILL.md` in the folder `dir_name`. Claude Code
/// makes every frontmatter field optional, and its skills must load as they
/// are: a missing `name` is the folder's, a missing `description` the first
/// paragraph of the body, and a file with no frontmatter is all body. The
/// name must still work as a `/name` command and something must describe
/// the skill.
fn parse_skill_md(content: &str, dir_name: &str) -> std::result::Result<Skill, String> {
    let content = content.trim_start_matches('\u{feff}').trim_start();
    let mut skill = if content.starts_with("---\n") || content.starts_with("---\r\n") {
        parse_yaml_skill(content, dir_name).map_err(|e| format!("malformed frontmatter: {e}"))?
    } else {
        Skill {
            name: dir_name.to_lowercase().replace(' ', "-"),
            description: String::new(),
            prompt_template: content.trim().to_string(),
            category: None,
            params: Vec::new(),
            skill_file: None,
            disable_model_invocation: false,
        }
    };
    if skill.name.is_empty() {
        return Err("no `name` in the frontmatter".into());
    }
    if skill.description.trim().is_empty() {
        skill.description = first_paragraph(&skill.prompt_template);
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
        return Err("no `description` in the frontmatter and an empty body".into());
    }
    Ok(skill)
}

/// The first paragraph of a skill body as one line, heading marks dropped
/// and capped so a body that is one long paragraph does not become the
/// listing.
fn first_paragraph(body: &str) -> String {
    let para: Vec<&str> = body
        .lines()
        .map(str::trim)
        .skip_while(|l| l.is_empty())
        .take_while(|l| !l.is_empty())
        .map(|l| l.trim_start_matches('#').trim())
        .collect();
    let text = para.join(" ");
    if text.chars().count() > 250 {
        let cut: String = text.chars().take(247).collect();
        format!("{cut}…")
    } else {
        text
    }
}

fn bundled_skills() -> Vec<Skill> {
    let code = Some("code".to_string());
    vec![
        Skill {
            name: "explain".into(),
            description: "Explain how a piece of code works".into(),
            prompt_template: "Please explain how the following code works, including its \
                purpose, key logic, and any non-obvious design decisions. {{ARGS}}"
                .into(),
            category: code.clone(),
            params: vec![],
            skill_file: None,
            disable_model_invocation: false,
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
            disable_model_invocation: false,
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
            disable_model_invocation: false,
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
    use crate::permissions::ReadDeny;
    use std::path::{Path, PathBuf};

    fn write(path: &Path, content: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    fn skill_md(name: &str, description: &str, body: &str) -> String {
        format!("---\nname: {name}\ndescription: {description}\n---\n{body}")
    }

    /// `base` joined with each `/`-separated part of `rel`, so the path uses
    /// the platform separator like the loader's own paths do. A literal
    /// `join("a/b")` keeps the `/` and never string-matches on Windows.
    fn at(base: &Path, rel: &str) -> PathBuf {
        rel.split('/')
            .fold(base.to_path_buf(), |p, part| p.join(part))
    }

    /// The five skill directories, highest priority first, under one temp dir:
    /// project, config dir and home are siblings.
    fn locations(root: &Path) -> [PathBuf; 5] {
        [
            at(root, "proj/.agents/skills"),
            at(root, "proj/.oxideclaw/skills"),
            at(root, "proj/.claude/skills"),
            at(root, "xdg/oxideclaw/skills"),
            at(root, "home/.claude/skills"),
        ]
    }

    async fn load(root: &Path) -> super::LoadedSkills {
        load_skills_at(
            &root.join("proj"),
            &at(root, "xdg/oxideclaw"),
            Some(&root.join("home")),
            &crate::permissions::ReadDeny::default(),
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
        assert!(skills.contains_key("explain"), "bundled skills are kept");
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
        let prompt = skill.invoke("1.2.3", &ReadDeny::default()).unwrap();
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
        let prompt = skill.invoke("for staging", &ReadDeny::default()).unwrap();
        assert!(prompt.ends_with("No slot.\n\nfor staging"), "{prompt}");

        std::fs::remove_file(skill_dir.join("SKILL.md")).unwrap();
        assert!(
            skill
                .invoke("", &ReadDeny::default())
                .unwrap_err()
                .contains("unreadable")
        );
    }

    /// Claude Code makes every SKILL.md field optional, but a SKILL.md with
    /// no `name`, no `description` or no frontmatter at all was skipped, so
    /// `~/.claude/skills/deploy/SKILL.md` with only `description:` never
    /// became /deploy.
    #[tokio::test]
    async fn claude_code_skills_without_name_or_frontmatter_load() {
        let dir = tempfile::tempdir().unwrap();
        let [agents, _, claude, _, home] = locations(dir.path());
        write(
            &at(&home, "deploy/SKILL.md"),
            "---\ndescription: Deploy the app\n---\nRun deploy.sh",
        );
        write(
            &at(&claude, "Fix Issue/SKILL.md"),
            "# Fix an issue\nfrom the tracker\n\nRead the issue, then fix it.",
        );
        write(
            &at(&agents, "notes/SKILL.md"),
            "---\nname: notes\n---\n\nKeep notes tidy.\n\nMore.",
        );

        let loaded = load(dir.path()).await;
        assert!(loaded.invalid.is_empty(), "{:?}", loaded.invalid);
        let deploy = &loaded.skills["deploy"];
        assert_eq!(deploy.description, "Deploy the app");
        let fix = &loaded.skills["fix-issue"];
        assert_eq!(fix.description, "Fix an issue from the tracker");
        assert_eq!(loaded.skills["notes"].description, "Keep notes tidy.");

        // Invoking parses the file again and must agree on name and body.
        let none = ReadDeny::default();
        assert!(
            deploy.invoke("", &none).unwrap().ends_with("Run deploy.sh"),
            "{:?}",
            deploy.invoke("", &none)
        );
        let prompt = fix.invoke("#42", &none).unwrap();
        assert!(
            prompt.ends_with("Read the issue, then fix it.\n\n#42"),
            "{prompt}"
        );
    }

    /// Claude Code's `$ARGUMENTS`, `$N` and `${CLAUDE_SKILL_DIR}` reached
    /// the model verbatim, with the args appended at the bottom instead.
    #[tokio::test]
    async fn claude_code_argument_placeholders_are_filled() {
        let dir = tempfile::tempdir().unwrap();
        let [_, _, claude, ..] = locations(dir.path());
        let skill_dir = at(&claude, "fix-issue");
        write(
            &skill_dir.join("SKILL.md"),
            "---\ndescription: d\n---\nFix issue $ARGUMENTS following our standards.\n\
             Repo $0, issue $ARGUMENTS[1], label $2; missing [$ARGUMENTS[5]].\n\
             Run ${CLAUDE_SKILL_DIR}/check.sh and awk '{print $7}'. Cost $5 or $ARGUMENTSX.",
        );
        let loaded = load(dir.path()).await;
        let none = ReadDeny::default();
        let prompt = loaded.skills["fix-issue"]
            .invoke("web 123 \"needs review\"", &none)
            .unwrap();
        let body = prompt.split_once("\n\n").unwrap().1;
        assert_eq!(
            body,
            format!(
                "Fix issue web 123 \"needs review\" following our standards.\n\
                 Repo web, issue 123, label needs review; missing [].\n\
                 Run {}/check.sh and awk '{{print $7}}'. Cost $5 or $ARGUMENTSX.",
                skill_dir.display()
            ),
            "the args were not appended again"
        );

        // Flat skills too; with no args the slot empties and nothing is appended.
        write(&claude.join("greet.md"), "Say hi to $ARGUMENTS.");
        let loaded = load(dir.path()).await;
        assert_eq!(
            loaded.skills["greet"].invoke("", &none).unwrap(),
            "Say hi to ."
        );
        assert_eq!(
            loaded.skills["greet"].invoke("Ana", &none).unwrap(),
            "Say hi to Ana."
        );
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
        assert_eq!(
            skills["b"].invoke("now", &ReadDeny::default()).unwrap(),
            "Do B\n\nnow"
        );
    }

    /// A cloned repo's SKILL.md that is a FIFO hung startup forever; one
    /// linked to /dev/zero read until out of memory.
    #[cfg(unix)]
    #[tokio::test]
    async fn special_files_are_skipped_without_blocking() {
        let dir = tempfile::tempdir().unwrap();
        let [agents, ..] = locations(dir.path());
        let zero = at(&agents, "a/SKILL.md");
        std::fs::create_dir_all(zero.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink("/dev/zero", &zero).unwrap();
        let fifo = at(&agents, "b/SKILL.md");
        std::fs::create_dir_all(fifo.parent().unwrap()).unwrap();
        let made = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .is_ok_and(|s| s.success());
        let loaded = tokio::time::timeout(std::time::Duration::from_secs(10), load(dir.path()))
            .await
            .expect("loading skills must not block");
        let invalid: Vec<_> = loaded.invalid.iter().map(|(p, _)| p.clone()).collect();
        assert!(invalid.contains(&zero), "{invalid:?}");
        if made {
            assert!(invalid.contains(&fifo), "{invalid:?}");
        }
    }

    #[tokio::test]
    async fn invalid_skills_are_skipped_with_one_warning_naming_each_path() {
        let dir = tempfile::tempdir().unwrap();
        let [agents, _, claude, _, _] = locations(dir.path());
        write(&at(&agents, "empty/SKILL.md"), "---\nname: empty\n---\n");
        write(
            &at(&agents, "slash/SKILL.md"),
            "---\nname: a/b\ndescription: d\n---\nbody",
        );
        write(
            &at(&agents, "badyaml/SKILL.md"),
            "---\nname: [oops\n---\nbody",
        );
        write(&claude.join("flat.md"), "---\nname: never-closed\n");
        // Not a skill at all: no warning.
        write(&at(&agents, "notes/todo.txt"), "x");
        write(
            &at(&agents, "good/SKILL.md"),
            &skill_md("good", "Fine", "body"),
        );

        let loaded = load(dir.path()).await;
        assert!(loaded.skills.contains_key("good"));
        for name in ["empty", "a/b", "slash", "badyaml", "never-closed", "notes"] {
            assert!(!loaded.skills.contains_key(name), "{name} loaded");
        }
        let warning = loaded.warning().unwrap();
        assert!(
            warning.starts_with("Skipped 4 invalid skill(s)"),
            "{warning}"
        );
        assert_eq!(warning.lines().count(), 5, "{warning}");
        for (path, why) in [
            (at(&agents, "empty/SKILL.md"), "no `description`"),
            (at(&agents, "slash/SKILL.md"), "not a single command word"),
            (at(&agents, "badyaml/SKILL.md"), "malformed frontmatter"),
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

    /// Skills were only looked up in cwd, so `cd packages/web && oxideclaw`
    /// lost a monorepo's root `.agents/skills`.
    #[tokio::test]
    async fn repo_root_skills_load_from_a_subdirectory_and_nearer_ones_win() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        let sub = at(&repo, "packages/web");
        write(
            &at(&repo, ".agents/skills/release/SKILL.md"),
            &skill_md("release", "root release", "b"),
        );
        write(
            &at(&repo, ".claude/skills/shared/SKILL.md"),
            &skill_md("shared", "root", "b"),
        );
        write(
            &at(&sub, ".claude/skills/shared/SKILL.md"),
            &skill_md("shared", "nearest", "b"),
        );
        // Above the repo root: never read.
        write(
            &at(dir.path(), ".agents/skills/outside/SKILL.md"),
            &skill_md("outside", "o", "b"),
        );
        let cfg = dir.path().join("cfg");
        let none = ReadDeny::default();

        let loaded = load_skills_at(&sub, &cfg, Some(dir.path()), &none).await;
        assert_eq!(loaded.skills["release"].description, "root release");
        assert_eq!(loaded.skills["shared"].description, "nearest");
        assert!(!loaded.skills.contains_key("outside"));

        // Not in a repo: only cwd itself.
        let plain = dir.path().join("plain/sub");
        write(
            &at(dir.path(), "plain/.agents/skills/up/SKILL.md"),
            &skill_md("up", "u", "b"),
        );
        std::fs::create_dir_all(&plain).unwrap();
        // Bounded at the tempdir: a TMPDIR inside a git checkout must not
        // turn `plain` into part of that repo.
        let loaded = load_skills_at(&plain, &cfg, Some(dir.path()), &none).await;
        assert!(!loaded.skills.contains_key("up"));
        assert!(!loaded.skills.contains_key("outside"));
    }

    /// `/review` and `/commit` always run the built-in commands, yet a
    /// user's `review` skill and the bundled `review`/`commit` skills were
    /// listed as runnable with no word that /name would never reach them.
    #[tokio::test]
    async fn skills_named_like_built_in_commands_are_reported() {
        let dir = tempfile::tempdir().unwrap();
        let [_, _, claude, ..] = locations(dir.path());
        write(
            &at(&claude, "review/SKILL.md"),
            &skill_md("review", "Our review", "b"),
        );
        write(&claude.join("deploy-check.md"), "Check the deploy");
        let loaded = load(dir.path()).await;
        assert!(loaded.invalid.is_empty(), "{:?}", loaded.invalid);
        assert_eq!(
            loaded.shadowed,
            vec![(at(&claude, "review/SKILL.md"), "review".to_string())]
        );
        let warning = loaded.warning().unwrap();
        assert!(warning.contains("built-in command"), "{warning}");
        assert!(warning.contains("/review"), "{warning}");
        assert!(!warning.contains("deploy-check"), "{warning}");

        let empty = tempfile::tempdir().unwrap();
        let bundled = load(empty.path()).await;
        assert!(bundled.warning().is_none(), "{:?}", bundled.warning());
        for name in bundled.skills.keys() {
            assert!(
                !super::is_builtin_command(name),
                "bundled /{name} is shadowed"
            );
        }
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
        let loaded = load_skills_at(
            &dir.path().join("proj"),
            &home.join(".claude"),
            Some(&home),
            &ReadDeny::default(),
        )
        .await;
        assert_eq!(loaded.skills["x"].description, "X");
    }

    /// Run from $HOME with the default config dir, `.claude/skills` and the
    /// config dir's `skills` are one directory. It was read twice, so every
    /// invalid skill in it was reported twice.
    #[tokio::test]
    async fn a_skill_dir_shared_by_cwd_and_config_dir_is_read_once() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let config = home.join(".claude");
        let shared = at(&config, "skills");
        write(&at(&shared, "broken/SKILL.md"), "---\nname: [oops\n---\nb");
        write(&at(&shared, "ok/SKILL.md"), &skill_md("ok", "OK", "b"));

        let dirs = super::skill_dirs(&home, &config, Some(&home));
        let paths: Vec<_> = dirs.iter().map(|(d, _)| d).collect();
        assert_eq!(paths.len(), 3, "{paths:?}");
        assert_eq!(paths.iter().filter(|d| ***d == shared).count(), 1);

        let loaded = load_skills_at(&home, &config, Some(&home), &ReadDeny::default()).await;
        assert_eq!(loaded.skills["ok"].description, "OK");
        assert_eq!(loaded.invalid.len(), 1, "{:?}", loaded.invalid);
        let warning = loaded.warning().unwrap();
        assert!(
            warning.starts_with("Skipped 1 invalid skill(s)"),
            "{warning}"
        );
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
