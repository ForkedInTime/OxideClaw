//! `oxideclaw mcp add/list/get/remove` against a fake home: the default
//! scope stays private to the user and project, the shared project scope
//! refuses literal secrets, and every listing names the scope.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Env {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    project: PathBuf,
}

impl Env {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let project = tmp.path().join("project");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&project).unwrap();
        Env {
            _tmp: tmp,
            home,
            project,
        }
    }

    fn config(&self) -> PathBuf {
        self.home.join(".config/oxideclaw")
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_oxideclaw"))
            .args(args)
            .current_dir(&self.project)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", &self.home)
            .output()
            .unwrap()
    }

    fn ok(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(out.status.success(), "{args:?}: {out:?}");
        String::from_utf8_lossy(&out.stdout).into_owned()
    }
}

fn files_under(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).unwrap().flatten() {
            if e.file_type().unwrap().is_dir() {
                stack.push(e.path());
            } else {
                out.push(e.path());
            }
        }
    }
    out
}

#[test]
fn mcp_add_defaults_to_local_and_project_refuses_secrets() {
    let e = Env::new();

    let out = e.ok(&[
        "mcp",
        "add",
        "gh",
        "npx",
        "srv",
        "-e",
        "GITHUB_TOKEN=ghp_secret",
    ]);
    assert!(out.contains("local scope"), "{out}");
    // Only the per-project file in the user's config; nothing in the repo,
    // nothing in settings.json (read by every project).
    assert!(files_under(&e.project).is_empty());
    assert!(!e.config().join("settings.json").exists());
    let written = files_under(&e.config().join("local-mcp"));
    assert_eq!(written.len(), 1, "{written:?}");
    assert!(
        std::fs::read_to_string(&written[0])
            .unwrap()
            .contains("ghp_secret")
    );
    // Keyed by the project: another project does not see it.
    let other = e.project.with_file_name("other");
    std::fs::create_dir(&other).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_oxideclaw"))
        .args(["mcp", "list"])
        .current_dir(&other)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", &e.home)
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&out.stdout).contains("No MCP servers configured"));

    // Shared scope: refused with the reason, and nothing written.
    let out = e.run(&[
        "mcp",
        "add",
        "--scope",
        "project",
        "team",
        "npx",
        "-e",
        "TOKEN=abc",
    ]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("shared") && err.contains("--force"), "{err}");
    assert!(!e.project.join(".mcp.json").exists());

    let out = e.ok(&[
        "mcp",
        "add",
        "--scope",
        "project",
        "team",
        "npx",
        "-e",
        "TOKEN=abc",
        "--force",
    ]);
    assert!(out.contains("/trust"), "{out}");
    assert!(e.project.join(".mcp.json").is_file());
    e.ok(&[
        "mcp",
        "add",
        "--scope",
        "user",
        "web",
        "-t",
        "http",
        "https://x.test/mcp",
    ]);

    let list = e.ok(&["mcp", "list"]);
    let line = |name: &str| {
        list.lines()
            .find(|l| l.trim_start().starts_with(name))
            .unwrap_or_else(|| panic!("{name} missing: {list}"))
            .to_string()
    };
    assert!(line("gh").contains("local"), "{list}");
    assert!(line("team").contains("project") && line("team").contains("needs /trust"));
    assert!(line("web").contains("user"), "{list}");

    let get = e.ok(&["mcp", "get", "team"]);
    assert!(get.contains("scope:     project"), "{get}");
    assert!(get.contains("/trust"), "{get}");

    let out = e.ok(&["mcp", "remove", "gh"]);
    assert!(out.contains("local scope"), "{out}");
    assert!(!e.ok(&["mcp", "list"]).contains("gh "));
}
