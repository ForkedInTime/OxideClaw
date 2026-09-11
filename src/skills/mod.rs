//! Skills system — markdown files that expand into prompts.
//!
//! Two formats supported:
//! 1. Legacy: `# Title\nDescription\n---\nPrompt with {{ARGS}}`
//! 2. Enhanced: YAML frontmatter with `name`, `description`, `category`, `params`
use anyhow::Result;
use std::collections::HashMap;
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
}

impl Skill {
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
    })
}

/// Check if an input is a skill invocation `/name [args]`.
pub fn parse_skill_invocation(input: &str) -> Option<(&str, &str)> {
    let input = input.trim();
    if !input.starts_with('/') {
        return None;
    }
    let rest = &input[1..];
    if let Some(sp) = rest.find(' ') {
        Some((&rest[..sp], rest[sp + 1..].trim()))
    } else {
        Some((rest, ""))
    }
}

/// Load all skills from bundled set + ~/.claude/skills/ + ./.claude/skills/.
pub async fn load_skills() -> HashMap<String, Skill> {
    let mut skills = HashMap::new();
    for s in bundled_skills() {
        skills.insert(s.name.clone(), s);
    }
    let global_dir = crate::config::Config::claude_dir().join("skills");
    let local_dir = std::path::Path::new(".claude").join("skills");
    for dir in [global_dir, local_dir] {
        if let Ok(mut entries) = fs::read_dir(&dir).await {
            while let Ok(Some(entry)) = entries.next_entry().await {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) == Some("md")
                    && let Ok(content) = fs::read_to_string(&path).await
                {
                    let fallback = path
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or("unnamed");
                    if let Ok(skill) = parse_skill_from_content(&content, fallback) {
                        skills.insert(skill.name.clone(), skill);
                    }
                }
            }
        }
    }
    skills
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
        },
        Skill {
            name: "explain".into(),
            description: "Explain how a piece of code works".into(),
            prompt_template: "Please explain how the following code works, including its \
                purpose, key logic, and any non-obvious design decisions. {{ARGS}}"
                .into(),
            category: code.clone(),
            params: vec![],
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
        },
        Skill {
            name: "test".into(),
            description: "Write tests for a function or module".into(),
            prompt_template: "Please write comprehensive tests for the following. Include \
                unit tests for edge cases and happy paths. {{ARGS}}"
                .into(),
            category: code,
            params: vec![],
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
