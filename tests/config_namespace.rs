//! OxideClaw owns its config namespace: the real binary, run against a
//! fake home holding Claude Code's `~/.claude`, migrates its own state out
//! once, writes only under `$XDG_CONFIG_HOME/oxideclaw`, and leaves
//! `~/.claude` byte-identical.

#![cfg(unix)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn snapshot(dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut out = BTreeMap::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).unwrap().flatten() {
            if e.file_type().unwrap().is_dir() {
                stack.push(e.path());
            } else {
                out.insert(e.path(), std::fs::read(e.path()).unwrap());
            }
        }
    }
    out
}

struct Home {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    project: PathBuf,
}

impl Home {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let project = tmp.path().join("project");
        let claude = home.join(".claude");
        std::fs::create_dir_all(claude.join("sessions")).unwrap();
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(
            claude.join("settings.json"),
            r#"{"model": "opus", "apiKeyHelper": "vault read",
                "hooks": {"PreToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": "audit"}]}]}}"#,
        )
        .unwrap();
        std::fs::write(claude.join("sessions/abc.meta"), r#"{"id": "abc"}"#).unwrap();
        std::fs::write(claude.join("sessions/abc.jsonl"), "").unwrap();
        Home {
            _tmp: tmp,
            home,
            project,
        }
    }

    fn claude(&self) -> PathBuf {
        self.home.join(".claude")
    }

    fn config(&self) -> PathBuf {
        self.home.join(".config/oxideclaw")
    }

    fn run(&self, args: &[&str], extra: &[(&str, &Path)]) -> Output {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_oxideclaw"));
        cmd.args(args)
            .current_dir(&self.project)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", &self.home);
        for (k, v) in extra {
            cmd.env(k, v);
        }
        let out = cmd.output().unwrap();
        assert!(out.status.success(), "{args:?}: {out:?}");
        out
    }
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn settings(path: &Path) -> serde_json::Value {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

#[test]
fn first_run_migrates_once_and_writes_only_to_the_xdg_dir() {
    let h = Home::new();
    let before = snapshot(&h.claude());

    let out = h.run(&["mcp", "add", "--scope", "user", "srv", "mycmd"], &[]);
    let err = stderr(&out);
    assert!(err.contains("Imported from"), "{err}");
    assert!(err.contains("Not imported: hooks, apiKeyHelper"), "{err}");

    let s = settings(&h.config().join("settings.json"));
    assert_eq!(s["model"], "opus", "safe key imported");
    assert_eq!(
        s["mcpServers"]["srv"]["command"], "mycmd",
        "write landed here"
    );
    assert!(s.get("hooks").is_none() && s.get("apiKeyHelper").is_none());
    assert!(
        h.home
            .join(".local/share/oxideclaw/sessions/abc.meta")
            .is_file()
    );

    // The second run neither migrates nor prints anything about it.
    let out = h.run(&["mcp", "list"], &[]);
    assert!(!stderr(&out).contains("Imported"), "{}", stderr(&out));

    // Opting in copies the hooks, still without touching ~/.claude.
    h.run(&["config", "import-claude", "--hooks"], &[]);
    let s = settings(&h.config().join("settings.json"));
    assert_eq!(s["hooks"]["preToolUse"][0]["command"], "audit");

    assert_eq!(snapshot(&h.claude()), before, "~/.claude must not change");
}

#[test]
fn xdg_config_home_and_the_override_variables_pick_the_dir() {
    let h = Home::new();
    let xdg = h.home.join("xdg");
    h.run(
        &["mcp", "add", "--scope", "user", "a", "x"],
        &[("XDG_CONFIG_HOME", &xdg)],
    );
    assert!(xdg.join("oxideclaw/settings.json").is_file());

    // OXIDECLAW_CONFIG_DIR wins and is not migrated into.
    let ox = h.home.join("ox");
    let out = h.run(
        &["mcp", "add", "--scope", "user", "b", "x"],
        &[
            ("OXIDECLAW_CONFIG_DIR", &ox),
            ("CLAUDE_CONFIG_DIR", &h.home.join("cc")),
        ],
    );
    assert!(!stderr(&out).contains("deprecated"), "{}", stderr(&out));
    let s = settings(&ox.join("settings.json"));
    assert_eq!(s["mcpServers"]["b"]["command"], "x");
    assert!(
        s.get("model").is_none(),
        "an explicit dir is not migrated into"
    );

    // CLAUDE_CONFIG_DIR still works, with a deprecation warning.
    let cc = h.home.join("cc");
    let out = h.run(
        &["mcp", "add", "--scope", "user", "c", "x"],
        &[("CLAUDE_CONFIG_DIR", &cc)],
    );
    assert!(
        stderr(&out).contains("CLAUDE_CONFIG_DIR is deprecated"),
        "{}",
        stderr(&out)
    );
    assert!(cc.join("settings.json").is_file());

    // ...except when it names ~/.claude, which is never written.
    let before = snapshot(&h.claude());
    let out = h.run(
        &["mcp", "add", "--scope", "user", "d", "x"],
        &[("CLAUDE_CONFIG_DIR", &h.claude())],
    );
    assert!(stderr(&out).contains("ignoring it"), "{}", stderr(&out));
    assert_eq!(snapshot(&h.claude()), before);
    let s = settings(&h.config().join("settings.json"));
    assert_eq!(s["mcpServers"]["d"]["command"], "x");
}

/// Older versions kept settings in `$XDG_CONFIG_HOME/oxideclaw`, where
/// `"autonomy": "auto-edit"` still prompted for every edit. The upgrade
/// rewrites it to `ask` once, says so, and leaves a later choice alone.
#[test]
fn a_stored_legacy_autonomy_is_migrated_to_ask_once() {
    let h = Home::new();
    let xdg = h.home.join("xdg");
    let dir = xdg.join("oxideclaw");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("settings.json"),
        r#"{"autonomy": "auto-edit", "model": "m"}"#,
    )
    .unwrap();
    let out = h.run(&["mcp", "list"], &[("XDG_CONFIG_HOME", &xdg)]);
    let err = stderr(&out);
    assert!(
        err.contains("\"auto-edit\"") && err.contains("\"ask\""),
        "{err}"
    );
    let s = settings(&dir.join("settings.json"));
    assert_eq!(s["autonomy"], "ask");
    assert_eq!(s["model"], "m");

    std::fs::write(dir.join("settings.json"), r#"{"autonomy": "auto-edit"}"#).unwrap();
    let out = h.run(&["mcp", "list"], &[("XDG_CONFIG_HOME", &xdg)]);
    assert!(!stderr(&out).contains("Autonomy"), "{}", stderr(&out));
    assert_eq!(
        settings(&dir.join("settings.json"))["autonomy"],
        "auto-edit"
    );
}
