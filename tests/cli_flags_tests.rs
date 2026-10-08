//! Regressions for top-level CLI flags that parsed but did not do what they
//! said: `browse` dropped --model/--settings, `--input-format stream-json`
//! kept only the last message, `-p` exited 0 at the turn cap, and
//! `mcp reset-project-choices` left the project trusted.
//!
//! Each test runs the real binary with a cleared environment and temp dirs
//! for HOME, XDG and the config dir, so nothing touches the user's setup.

#![cfg(unix)]

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex};

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
        .env("OXIDECLAW_CONFIG_DIR", &env.config_dir)
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

/// OpenAI-compatible endpoint that answers every request with `reply` and
/// records each request body.
fn serve(reply: serde_json::Value) -> (u16, Arc<Mutex<Vec<String>>>) {
    serve_seq(vec![sse_response(&reply)])
}

fn sse_response(reply: &serde_json::Value) -> String {
    let body = format!("data: {reply}\n\ndata: [DONE]\n\n");
    format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
}

/// Like `serve`, answering the n-th request with the n-th raw HTTP response
/// and every later one with the last.
fn serve_seq(responses: Vec<String>) -> (u16, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let bodies = Arc::new(Mutex::new(Vec::new()));
    let seen = bodies.clone();
    std::thread::spawn(move || {
        for sock in listener.incoming() {
            let Ok(mut sock) = sock else { return };
            let mut raw = Vec::new();
            let mut buf = [0u8; 8192];
            let request_body = loop {
                let n = sock.read(&mut buf).unwrap_or(0);
                if n == 0 {
                    break None;
                }
                raw.extend_from_slice(&buf[..n]);
                let text = String::from_utf8_lossy(&raw).into_owned();
                if let Some(split) = text.find("\r\n\r\n") {
                    let len = text[..split]
                        .lines()
                        .find_map(|l| {
                            let (k, v) = l.split_once(':')?;
                            k.eq_ignore_ascii_case("content-length")
                                .then(|| v.trim().parse::<usize>().ok())?
                        })
                        .unwrap_or(0);
                    if raw.len() >= split + 4 + len {
                        break Some(text[split + 4..].to_string());
                    }
                }
            };
            let Some(request_body) = request_body else {
                continue;
            };
            let n = {
                let mut seen = seen.lock().unwrap();
                seen.push(request_body);
                seen.len()
            };
            let response = &responses[(n - 1).min(responses.len() - 1)];
            let _ = sock.write_all(response.as_bytes());
        }
    });
    (port, bodies)
}

fn text_reply(text: &str) -> serde_json::Value {
    serde_json::json!({
        "id": "c", "object": "chat.completion.chunk", "model": "m",
        "choices": [{
            "index": 0,
            "delta": { "role": "assistant", "content": text },
            "finish_reason": "stop"
        }]
    })
}

fn bash_call_reply(command: &str) -> serde_json::Value {
    let args = serde_json::json!({ "command": command }).to_string();
    serde_json::json!({
        "id": "c", "object": "chat.completion.chunk", "model": "m",
        "choices": [{
            "index": 0,
            "delta": { "role": "assistant", "tool_calls": [{
                "index": 0, "id": "call_1", "type": "function",
                "function": { "name": "Bash", "arguments": args }
            }]},
            "finish_reason": "tool_calls"
        }]
    })
}

fn openai_env(port: u16) -> Vec<(&'static str, String)> {
    vec![
        ("OPENAI_BASE_URL", format!("http://127.0.0.1:{port}/v1")),
        ("OPENAI_API_KEY", "test".to_string()),
    ]
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

/// Only the last user event was sent, and string `content` was skipped.
#[test]
fn stream_json_input_sends_every_user_message() {
    let e = env();
    let (port, bodies) = serve(text_reply("ok"));
    let stdin = concat!(
        r#"{"type":"user","message":{"role":"user","content":"first question"}}"#,
        "\n",
        r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"second question"}]}}"#,
        "\n",
    );
    let out = run(
        &e,
        &[
            "-p",
            "--input-format",
            "stream-json",
            "--model",
            "openai-compat:test",
        ],
        &openai_env(port),
        stdin,
    );
    assert!(out.status.success(), "{}", stderr(&out));
    let bodies = bodies.lock().unwrap();
    assert_eq!(bodies.len(), 2, "{bodies:?}");
    assert!(bodies[0].contains("first question"), "{}", bodies[0]);
    assert!(!bodies[0].contains("second question"));
    assert!(bodies[1].contains("first question") && bodies[1].contains("second question"));

    let out = run(
        &e,
        &[
            "-p",
            "--input-format",
            "stream-json",
            "--model",
            "openai-compat:test",
        ],
        &openai_env(port),
        "{\"type\":\"system\"}\n",
    );
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("no user message"), "{}", stderr(&out));
}

/// Hitting --max-turns exited 0 with nothing on stdout to mark the run as
/// cut off.
#[test]
fn print_mode_reports_the_turn_cap() {
    let e = env();
    let (port, bodies) = serve(bash_call_reply("echo hi"));
    let out = run(
        &e,
        &[
            "-p",
            "--dangerously-skip-permissions",
            "--max-turns",
            "1",
            "--output-format",
            "json",
            "--model",
            "openai-compat:test",
            "go",
        ],
        &openai_env(port),
        "",
    );
    assert_eq!(bodies.lock().unwrap().len(), 1);
    assert!(!out.status.success(), "exited 0 at the turn cap");
    assert!(stderr(&out).contains("--max-turns"), "{}", stderr(&out));
    let stdout = String::from_utf8_lossy(&out.stdout);
    let last: serde_json::Value = serde_json::from_str(stdout.lines().last().unwrap()).unwrap();
    assert_eq!(last["subtype"], "error_max_turns");
    assert_eq!(last["is_error"], true);
}

fn trusted(config_dir: &Path) -> Vec<String> {
    let s: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(config_dir.join("settings.json")).unwrap())
            .unwrap();
    serde_json::from_value(s["trustedProjects"].clone()).unwrap()
}

/// The command cleared keys nothing reads; project MCP servers are gated by
/// the global trustedProjects list, which it left alone.
#[test]
fn reset_project_choices_revokes_trust() {
    let e = env();
    let project = e.project.canonicalize().unwrap();
    let project = project.to_str().unwrap();
    std::fs::write(
        e.config_dir.join("settings.json"),
        serde_json::json!({ "trustedProjects": [project, "/elsewhere"] }).to_string(),
    )
    .unwrap();
    let out = run(&e, &["mcp", "reset-project-choices"], &[], "");
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(trusted(&e.config_dir), vec!["/elsewhere"]);

    let out = run(&e, &["mcp", "reset-project-choices"], &[], "");
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(String::from_utf8_lossy(&out.stdout).contains("not trusted"));
    assert_eq!(trusted(&e.config_dir), vec!["/elsewhere"]);
}

/// The tool names a recorded chat-completions request offered the model.
fn offered_tools(body: &str) -> Vec<String> {
    let v: serde_json::Value = serde_json::from_str(body).unwrap();
    v["tools"]
        .as_array()
        .map(|tools| {
            tools
                .iter()
                .filter_map(|t| t["function"]["name"].as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

/// Runs `-p` against a model that asks for `touch marker` on every turn;
/// returns whether the marker was made and what the second request carried
/// back as the tool result.
fn touch_marker_with(flags: &[&str]) -> (bool, Vec<String>, String) {
    let e = env();
    let (port, bodies) = serve(bash_call_reply("touch marker"));
    // The prompt goes first: the tool flags take every argument after them.
    let mut args = vec![
        "-p",
        "go",
        "--max-turns",
        "2",
        "--model",
        "openai-compat:test",
    ];
    args.extend_from_slice(flags);
    let out = run(&e, &args, &openai_env(port), "");
    let bodies = bodies.lock().unwrap();
    assert!(bodies.len() >= 2, "{}", stderr(&out));
    (
        e.project.join("marker").exists(),
        offered_tools(&bodies[0]),
        bodies[1].clone(),
    )
}

/// `--allowed-tools 'Bash(git status:*)'` was taken as a tool name: it
/// matched no tool, so the model got none and nothing was pre-approved.
/// A rule now keeps every tool and approves only the calls it matches.
#[test]
fn allowed_tools_rules_approve_only_matching_commands() {
    let (made, tools, _) = touch_marker_with(&["--allowed-tools", "Bash(touch marker)"]);
    assert!(
        made,
        "a matching rule must run the command without a prompt"
    );
    for t in ["Bash", "Edit", "Read"] {
        assert!(tools.iter().any(|n| n == t), "{t} missing: {tools:?}");
    }

    let (made, _, result) = touch_marker_with(&["--allowed-tools", "Bash(git status:*)"]);
    assert!(!made, "a rule for git status approved touch");
    assert!(result.contains("Permission denied"), "{result}");
}

/// Bare names still restrict the tool list; a rule beside them keeps its tool.
#[test]
fn allowed_tools_bare_names_filter_the_tool_list() {
    let (made, mut tools, result) = touch_marker_with(&[
        "--allowed-tools",
        "Read,Grep",
        "--allowed-tools",
        "Bash(ls:*)",
    ]);
    tools.sort();
    assert_eq!(tools, ["Bash", "Grep", "Read"]);
    assert!(!made && result.contains("Permission denied"), "{result}");
}

/// A `--disallowed-tools` rule refuses matching calls even under
/// --dangerously-skip-permissions, and leaves the tool for other calls.
#[test]
fn disallowed_tools_rules_block_matching_commands() {
    let (made, tools, result) = touch_marker_with(&[
        "--dangerously-skip-permissions",
        "--disallowed-tools",
        "Bash(touch:*) WebFetch",
    ]);
    assert!(!made, "the deny rule did not hold");
    assert!(result.contains("Permission denied"), "{result}");
    assert!(tools.iter().any(|n| n == "Bash"), "{tools:?}");
    assert!(!tools.iter().any(|n| n == "WebFetch"), "{tools:?}");
}

/// Claude Code's form passes each tool as its own argument. With one value
/// per flag, `WebFetch` became prompt text and stayed available.
#[test]
fn tool_flags_take_separate_arguments() {
    let (_, tools, _) = touch_marker_with(&["--disallowed-tools", "Edit", "WebFetch"]);
    assert!(tools.iter().any(|n| n == "Bash"), "{tools:?}");
    for t in ["Edit", "WebFetch"] {
        assert!(
            !tools.iter().any(|n| n == t),
            "{t} still offered: {tools:?}"
        );
    }
}

/// A prompt after the flag is read as tool names and fails loudly, rather
/// than dropping a restriction.
#[test]
fn a_prompt_after_a_tool_flag_is_a_startup_error() {
    let e = env();
    let out = run(&e, &["-p", "--disallowed-tools", "Bash", "fix it"], &[], "");
    assert_eq!(out.status.code(), Some(1));
    let err = stderr(&out);
    assert!(err.contains("unknown tool `fix`"), "{err}");
    assert!(err.contains("put the prompt first"), "{err}");
}

/// An entry that does not parse is a startup error, never dropped.
#[test]
fn malformed_tool_flags_exit_non_zero_with_the_reason() {
    let e = env();
    for (flag, value, needle) in [
        (
            "--allowed-tools",
            "Bash(git status:*",
            "Error: --allowed-tools: `Bash(git status:*` has an unclosed `(`",
        ),
        (
            "--disallowed-tools",
            "Bassh",
            "Error: --disallowed-tools: unknown tool `Bassh`",
        ),
        (
            "--allowed-tools",
            "Agent(explore)",
            "`Agent(explore)` is not a permission rule",
        ),
    ] {
        let out = run(&e, &["-p", "go", flag, value], &[], "");
        assert_eq!(out.status.code(), Some(1), "{flag} {value}");
        assert!(stderr(&out).contains(needle), "{}", stderr(&out));
    }
}

/// `-p --session-id` saved nothing when a request failed: the next run
/// with the same id had no record of the tool calls the failed run made.
#[test]
fn a_failed_print_run_still_saves_its_session() {
    let e = env();
    let body = r#"{"error":{"message":"bad request","type":"invalid_request_error"}}"#;
    let bad = format!(
        "HTTP/1.1 400 Bad Request\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    let (port, _) = serve_seq(vec![
        sse_response(&bash_call_reply("echo saved-marker")),
        bad,
    ]);
    let id = "6f1c2a7e-3b4d-4e5f-8a9b-0c1d2e3f4a5b";
    let args = [
        "-p",
        "--dangerously-skip-permissions",
        "--session-id",
        id,
        "--model",
        "openai-compat:test",
    ];
    let out = run(&e, &[&args[..], &["first"]].concat(), &openai_env(port), "");
    assert!(!out.status.success(), "the second request failed");

    let (port, bodies) = serve(text_reply("ok"));
    let out = run(&e, &[&args[..], &["second"]].concat(), &openai_env(port), "");
    assert!(out.status.success(), "{}", stderr(&out));
    let bodies = bodies.lock().unwrap();
    assert!(
        bodies[0].contains("first") && bodies[0].contains("saved-marker"),
        "{}",
        bodies[0]
    );
}
