//! The public docs claim only what the code does. Each retracted phrase
//! below was once in README.md or FEATURES.md and was false or unprovable;
//! the checks keep it from creeping back in an unrelated edit.

const README: &str = include_str!("../README.md");
const FEATURES: &str = include_str!("../FEATURES.md");

/// (phrase, why it must not be claimed)
const RETRACTED: &[(&str, &str)] = &[
    (
        "XDG Base Directory compliant",
        "config defaults to ~/.claude, not $XDG_CONFIG_HOME",
    ),
    (
        "~/.config/oxideclaw/settings.json",
        "settings.json is read from Config::claude_dir(), ~/.claude by default",
    ),
    (
        "~/.local/share/oxideclaw/sessions",
        "sessions default to <config dir>/sessions without $XDG_DATA_HOME",
    ),
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
];

#[test]
fn public_docs_make_no_retracted_claims() {
    for (doc, text) in [("README.md", README), ("FEATURES.md", FEATURES)] {
        for (phrase, why) in RETRACTED {
            assert!(!text.contains(phrase), "{doc} says {phrase:?}, but {why}");
        }
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

#[test]
fn router_is_documented_as_off_by_default_and_is() {
    assert!(!oxideclaw::router::RouterConfig::default().enabled);
    assert!(README.contains("The smart router is optional and off by default."));
    assert!(FEATURES.contains("## Smart Model Router\n\nOptional and off by default."));
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
