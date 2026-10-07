//! Regressions for top-level CLI flags that parsed but did not do what they
//! said: `browse` dropped --model/--settings, `--input-format stream-json`
//! kept only the last message, `-p` exited 0 at the turn cap, and
//! `mcp reset-project-choices` left the project trusted.
//!
//! Each test runs the real binary with a cleared environment and temp dirs
//! for HOME, XDG and the config dir, so nothing touches the user's setup.

#![cfg(unix)]

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

struct Env {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    config_dir: PathBuf,
    project: PathBuf,
}

fn env() -> Env {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let config_dir = tmp.path().join("config");
    let project = tmp.path().join("project");
    for d in [&home, &config_dir, &project] {
        std::fs::create_dir_all(d).unwrap();
    }
    Env {
        _tmp: tmp,
        home,
        config_dir,
        project,
    }
}

fn run(env: &Env, args: &[&str], extra_env: &[(&str, String)], stdin: &str) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_oxideclaw"));
    cmd.args(args)
        .current_dir(&env.project)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", &env.home)
        .env("CLAUDE_CONFIG_DIR", &env.config_dir)
        .env("XDG_CONFIG_HOME", env.home.join(".config"))
        .env("XDG_DATA_HOME", env.home.join(".local/share"))
        .env("XDG_CACHE_HOME", env.home.join(".cache"))
        .env("XDG_STATE_HOME", env.home.join(".local/state"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

/// `oxideclaw --model X browse` used a bare Config::load and returned
/// before the CLI overrides, so it ran (and billed) the settings model.
/// Without an Anthropic key the run stops at the key check, which names the
/// model it would have used.
#[test]
fn browse_uses_top_level_model_and_settings() {
    let e = env();
    let out = run(
        &e,
        &["--model", "haiku", "browse", "find the docs"],
        &[],
        "",
    );
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("for model: claude-haiku-4-5"),
        "{}",
        stderr(&out)
    );

    let out = run(
        &e,
        &[
            "--settings",
            r#"{"model":"claude-fable-5-1"}"#,
            "browse",
            "find the docs",
        ],
        &[],
        "",
    );
    assert!(
        stderr(&out).contains("for model: claude-fable-5-1"),
        "{}",
        stderr(&out)
    );
}
