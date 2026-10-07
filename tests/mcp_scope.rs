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
        self.run_in(&self.project, args)
    }

    /// `mcp list` starts the servers it lists, so everything the binary
    /// could touch points into the temp dir.
    fn run_in(&self, dir: &Path, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_oxideclaw"))
            .args(args)
            .current_dir(dir)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", &self.home)
            .env("XDG_CONFIG_HOME", self.home.join(".config"))
            .env("XDG_DATA_HOME", self.home.join(".local/share"))
            .env("XDG_CACHE_HOME", self.home.join(".cache"))
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

    // `mcp list` starts what it lists: a command that cannot exist and a
    // refused port keep this test off the network.
    let out = e.ok(&[
        "mcp",
        "add",
        "gh",
        "oxideclaw-test-no-such-server",
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
    let out = e.run_in(&other, &["mcp", "list"]);
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
        "http://127.0.0.1:9/mcp",
    ]);

    let list = e.ok(&["mcp", "list"]);
    let line = |name: &str| {
        list.lines()
            .find(|l| l.trim_start().starts_with(name))
            .unwrap_or_else(|| panic!("{name} missing: {list}"))
            .to_string()
    };
    assert!(line("gh").contains("local"), "{list}");
    assert!(line("gh").contains("failed to connect"), "{list}");
    assert!(line("team").contains("project") && line("team").contains("needs /trust"));
    assert!(
        !line("team").contains("connect"),
        "an untrusted server must not start: {list}"
    );
    assert!(line("web").contains("user"), "{list}");

    let get = e.ok(&["mcp", "get", "team"]);
    assert!(get.contains("scope:     project"), "{get}");
    assert!(get.contains("/trust"), "{get}");

    let out = e.ok(&["mcp", "remove", "gh"]);
    assert!(out.contains("local scope"), "{out}");
    assert!(!e.ok(&["mcp", "list"]).contains("gh "));
}

/// `oxideclaw mcp list` connects to each server a session would start and
/// names the protocol revision it negotiated: the stateless 2026-07-28 one,
/// or the version a handshake-era server answered to `initialize`.
#[test]
fn mcp_list_shows_the_negotiated_protocol_revision() {
    let e = Env::new();
    // One JSON-RPC message per line; `$id` is the request id.
    let read = r#"while IFS= read -r l; do
id=$(printf '%s\n' "$l" | sed -n 's/^{"jsonrpc":"2.0","id":\([0-9]*\),.*/\1/p')
[ -z "$id" ] && continue
reply() { printf '{"jsonrpc":"2.0","id":%s,%s}\n' "$id" "$1"; }
"#;
    let modern = format!(
        r#"{read}case "$l" in
*'"method":"server/discover"'*) reply '"result":{{"resultType":"complete","supportedVersions":["2026-07-28"],"capabilities":{{}},"ttlMs":0,"cacheScope":"private"}}';;
*'"method":"tools/list"'*) reply '"result":{{"resultType":"complete","tools":[],"ttlMs":0,"cacheScope":"private"}}';;
*) reply '"error":{{"code":-32601,"message":"no"}}';;
esac
done"#
    );
    let legacy = format!(
        r#"{read}case "$l" in
*'"method":"initialize"'*) reply '"result":{{"protocolVersion":"2024-11-05","capabilities":{{}}}}';;
*'"method":"tools/list"'*) reply '"result":{{"tools":[]}}';;
*) reply '"error":{{"code":-32601,"message":"no"}}';;
esac
done"#
    );
    for (name, script) in [("fresh", modern), ("vintage", legacy)] {
        let json = serde_json::json!({"command": "sh", "args": ["-c", script]}).to_string();
        e.ok(&["mcp", "add-json", name, &json]);
    }
    e.ok(&[
        "mcp",
        "add-json",
        "off",
        r#"{"command":"sh","disabled":true}"#,
    ]);

    let list = e.ok(&["mcp", "list"]);
    let line = |name: &str| {
        list.lines()
            .find(|l| l.trim_start().starts_with(name))
            .unwrap_or_else(|| panic!("{name} missing: {list}"))
            .to_string()
    };
    assert!(
        line("fresh").contains("connected, MCP 2026-07-28"),
        "{list}"
    );
    assert!(
        line("vintage").contains("connected, MCP 2024-11-05"),
        "{list}"
    );
    assert!(line("off").contains("[disabled]"), "{list}");
    assert!(!line("off").contains("connect"), "{list}");
}
