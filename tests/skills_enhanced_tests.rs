#[test]
fn parse_yaml_frontmatter_skill() {
    use rustyclaw::skills::parse_skill_from_content;
    let content = r#"---
name: scrape-prices
description: Extract product prices
category: browser
params:
  url: { required: true, description: "Target URL" }
  max_pages: { default: "3", description: "Max pages" }
---
Navigate to {{url}} and extract prices. Max pages: {{max_pages}}."#;

    let skill = parse_skill_from_content(content, "scrape-prices").unwrap();
    assert_eq!(skill.name, "scrape-prices");
    assert_eq!(skill.category.as_deref(), Some("browser"));
    assert_eq!(skill.params.len(), 2);
    // Map order from serde_yaml is insertion-order; assertion is loose for robustness
    let by_name: std::collections::HashMap<&str, &rustyclaw::skills::SkillParam> =
        skill.params.iter().map(|p| (p.name.as_str(), p)).collect();
    assert!(by_name["url"].required);
    assert_eq!(by_name["max_pages"].default.as_deref(), Some("3"));
}

#[test]
fn expand_named_params() {
    use rustyclaw::skills::parse_skill_from_content;
    let content = r#"---
name: test-skill
description: A test
params:
  url: { required: true }
  count: { default: "5" }
---
Fetch {{url}} and get {{count}} items."#;

    let skill = parse_skill_from_content(content, "test-skill").unwrap();
    let expanded = skill.expand_named("url=https://example.com count=10");
    assert!(expanded.contains("https://example.com"));
    assert!(expanded.contains("10"));
}

#[test]
fn expand_named_params_uses_defaults() {
    use rustyclaw::skills::parse_skill_from_content;
    let content = r#"---
name: test-skill
description: A test
params:
  url: { required: true }
  count: { default: "5" }
---
Fetch {{url}} and get {{count}} items."#;

    let skill = parse_skill_from_content(content, "test-skill").unwrap();
    let expanded = skill.expand_named("url=https://example.com");
    assert!(expanded.contains("https://example.com"));
    assert!(expanded.contains("5")); // default
}

#[test]
fn backward_compat_args_blob() {
    use rustyclaw::skills::parse_skill_from_content;
    let content = "# Old Skill\nDoes a thing.\n---\nPlease do {{ARGS}}.";
    let skill = parse_skill_from_content(content, "old-skill").unwrap();
    assert!(skill.params.is_empty());
    assert!(skill.category.is_none());
    let expanded = skill.expand("something cool");
    assert!(expanded.contains("something cool"));
}

#[test]
fn filter_skills_by_category() {
    use rustyclaw::skills::{Skill, SkillParam};
    let skills = [
        Skill {
            name: "a".into(),
            description: String::new(),
            prompt_template: String::new(),
            category: Some("browser".into()),
            params: vec![],
        },
        Skill {
            name: "b".into(),
            description: String::new(),
            prompt_template: String::new(),
            category: Some("code".into()),
            params: vec![],
        },
        Skill {
            name: "c".into(),
            description: String::new(),
            prompt_template: String::new(),
            category: None,
            params: vec![],
        },
    ];
    let _ = SkillParam {
        name: "x".into(),
        required: true,
        default: None,
        description: String::new(),
        enum_values: None,
    };
    let browser: Vec<_> = skills
        .iter()
        .filter(|s| s.category.as_deref() == Some("browser"))
        .collect();
    assert_eq!(browser.len(), 1);
    assert_eq!(browser[0].name, "a");
}
