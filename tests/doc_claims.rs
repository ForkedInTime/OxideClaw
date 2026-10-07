//! The public docs claim only what the code does. Each retracted phrase
//! below was once in README.md or FEATURES.md and was false or unprovable;
//! the checks keep it from creeping back in an unrelated edit.

const README: &str = include_str!("../README.md");
const FEATURES: &str = include_str!("../FEATURES.md");

/// (phrase, why it must not be claimed)
const RETRACTED: &[(&str, &str)] = &[
    ("No Python", "the voice add-on needs Python + Coqui"),
    (
        "cannot talk to Ollama",
        "Claude Code reaches Ollama through its Anthropic-compatible API",
    ),
    (
        "prompt-injected JSON",
        "a model without tools drops to text-only chat",
    ),
    ("semantic search", "FTS5 is lexical BM25 search"),
    ("github/stars/", "no stars badge"),
    (
        "3 ms cold start",
        "3 ms is the `--version` time, not first frame",
    ),
    (
        "cheapest capable model",
        "the router is opt-in and off by default",
    ),
    (
        "No other coding agent speaks",
        "Hermes speaks every reply; claim only the record-your-voice flow",
    ),
    (
        "pollute your history",
        "Cline uses the same private refs; others use a shadow gitdir",
    ),
    (
        "Never pushed",
        "`git push --mirror` pushes refs/oxideclaw/*",
    ),
    (
        "6-second sample",
        "the shortest /voice clone tier records 10 s",
    ),
    ("Piper", "there is no Piper TTS backend"),
    (
        "Haiku / Ollama",
        "the router's default tiers are Claude models, none of them Ollama",
    ),
    (
        "shown in `/cost`",
        "routing savings are shown by `/router`, not `/cost`",
    ),
    (
        "Single 19 MB static binary",
        "only the musl build is static; the gnu builds need glibc 2.28+",
    ),
    (
        "No dependencies.",
        "the gnu builds need glibc and the voice add-on needs Python",
    ),
    (
        "runs even in projects you have not",
        "auto-fix runs the repo's lint and test commands only in /trust-ed projects",
    ),
];

#[test]
fn public_docs_make_no_retracted_claims() {
    for (doc, text) in [("README.md", README), ("FEATURES.md", FEATURES)] {
        for (phrase, why) in RETRACTED {
            assert!(!text.contains(phrase), "{doc} says {phrase:?}, but {why}");
        }
    }
}

/// The config namespace change made these true; they must stay in sync
/// with `Config::config_dir` / `data_dir` (unit-tested in src/config.rs).
#[test]
fn docs_name_oxideclaws_own_xdg_dirs() {
    for (doc, text) in [("README.md", README), ("FEATURES.md", FEATURES)] {
        assert!(text.contains("XDG Base Directory compliant"), "{doc}");
        assert!(text.contains("`~/.config/oxideclaw/"), "{doc}");
        assert!(text.contains("`~/.local/share/oxideclaw/sessions"), "{doc}");
        assert!(text.contains("oxideclaw config import-claude"), "{doc}");
        assert!(
            !text.contains("Settings live in `~/.claude/settings.json`"),
            "{doc} still says OxideClaw's settings are in ~/.claude"
        );
    }
}

#[test]
fn voice_docs_disclose_python_and_the_cpml_license() {
    let disclosure = "Voice is an optional add-on that needs Python + Coqui. \
                      XTTS v2 weights are licensed under CPML (non-commercial use only).";
    for (doc, text) in [("README.md", README), ("FEATURES.md", FEATURES)] {
        assert!(
            text.contains(disclosure),
            "{doc} lacks the voice disclosure"
        );
    }
    assert!(
        oxideclaw::voice::XTTS_FIRST_RUN_HINT.contains("CPML"),
        "the first-run hint and the docs must name the same license"
    );
}

/// Off unless two tiers are configured or it is switched on, and the
/// docs say exactly that: heuristic by default, classifier opt-in.
#[test]
fn router_is_documented_as_off_by_default_and_is() {
    use oxideclaw::router::{Classifier, RouterConfig, starts_enabled};
    assert!(!RouterConfig::default().enabled);
    assert_eq!(RouterConfig::default().classifier, Classifier::Heuristic);
    assert!(!starts_enabled(None, 0) && !starts_enabled(None, 1));
    assert!(starts_enabled(None, 2) && !starts_enabled(Some(false), 2));
    assert!(README.contains("The model router is optional. Give two or more tiers a model"));
    assert!(FEATURES.contains("## Smart Model Router\n\nOptional."));
    assert!(FEATURES.contains(
        "The router starts on once two or more tiers are set. `\"enabled\": false` keeps it off"
    ));
    assert!(FEATURES.contains("By default a keyword and length heuristic scores the prompt"));
    assert!(FEATURES.contains("With `\"classifier\": \"model\"` the low tier is asked"));
}

/// The comparison is against the agents people actually run, and claims as
/// OxideClaw's alone only the scoped index, `/budget` and voice rows.
#[test]
fn comparison_table_names_the_major_agents_and_three_unique_rows() {
    assert!(README.contains(
        "| | Claude Code | Codex CLI | Copilot CLI | OpenCode | Aider | **OxideClaw** |"
    ));
    let table: Vec<&str> = README
        .lines()
        .skip_while(|l| !l.starts_with("| | Claude Code"))
        .take_while(|l| l.starts_with('|'))
        .collect();
    let unique: Vec<&str> = table
        .iter()
        .skip(2)
        .filter(|row| {
            let cells: Vec<&str> = row.split('|').map(str::trim).collect();
            // cells: "", label, 5 competitors, OxideClaw, ""
            cells[2..7].iter().all(|c| !c.starts_with('✅')) && cells[7].starts_with("**✅")
        })
        .map(|row| row.split('|').nth(1).unwrap().trim())
        .collect();
    assert_eq!(
        unique,
        [
            "Code index built in, on by default, local, no embeddings",
            "`/budget` hard stop you can set mid-session",
            "Replies spoken locally in a voice you record",
        ]
    );
    assert!(README.contains("Only the first three rows are OxideClaw's alone"));
}

/// Auto-fix runs the repo's own lint and test commands only in `/trust`-ed
/// projects: with the shipped defaults an untrusted project runs nothing,
/// not even the runner probe, and the README says so.
#[test]
fn autofix_is_gated_on_trust_and_readme_says_so() {
    let config = oxideclaw::config::Config::default();
    assert_eq!(config.autonomy, oxideclaw::permissions::Autonomy::Ask);
    assert!(oxideclaw::autofix::should_trigger(
        &config.auto_fix,
        config.autonomy
    ));
    assert!(!config.project_trusted);
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("Cargo.toml"), "[package]\n").unwrap();
    let untrusted = oxideclaw::autofix::Containment {
        trusted: config.project_trusted,
        ..Default::default()
    };
    let action = oxideclaw::autofix::run_auto_fix_check(
        dir.path(),
        &config.auto_fix,
        config.autonomy,
        0,
        &untrusted,
        &std::sync::atomic::AtomicBool::new(false),
    );
    assert!(
        matches!(action, oxideclaw::autofix::AutoFixAction::Untrusted),
        "{action:?}"
    );
    assert!(README.contains(
        "In trusted projects, every edit triggers a lint and test cycle. Untrusted projects skip it until you run /trust."
    ));
    assert!(README.contains("**✅ runners detected with zero config; trusted projects only**"));
}

/// The router table lists the real default tiers, once, and nothing else.
#[test]
fn features_router_table_matches_router_defaults() {
    let router = oxideclaw::router::RouterConfig::default();
    let section: &str = FEATURES
        .split("## Smart Model Router")
        .nth(1)
        .and_then(|s| s.split("\n---\n").next())
        .expect("FEATURES.md has a Smart Model Router section");
    let rows: Vec<&str> = section.lines().filter(|l| l.starts_with('|')).collect();
    assert_eq!(rows.len(), 6, "header, separator and four tiers: {rows:#?}");
    for (tier, model) in [
        ("Low", router.low_model.as_str()),
        ("Medium", router.medium_model.as_str()),
        ("Super-high", router.super_high_model.as_str()),
    ] {
        let prefix = format!("| {tier} | `{model}` |");
        assert!(
            rows.iter().any(|r| r.starts_with(&prefix)),
            "FEATURES.md router table lacks {prefix}"
        );
    }
    assert!(
        router.high_model.is_empty(),
        "high tier is the current model"
    );
    assert!(
        rows.iter()
            .any(|r| r.starts_with("| High | your current model |"))
    );
}
