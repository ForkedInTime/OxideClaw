//! Regression: Ctrl-C/SIGTERM during `oxideclaw -p` must kill the running
//! Bash tool's process group. Tools run in their own group, so the signal
//! never reaches them, and dying on the default action skipped the guard
//! that kills them: the command ran on as an orphan of init.
//!
//! Unix-only: process groups and signals are POSIX.

#![cfg(unix)]

use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn pid_alive(pid: i32) -> bool {
    // SAFETY: signal 0 only checks that the process exists.
    if unsafe { libc::kill(pid, 0) } != 0 {
        return false;
    }
    // An orphan killed under a container init that never reaps stays a
    // zombie, which kill(pid, 0) still reports. It is dead.
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => {
            stat.rsplit(')')
                .next()
                .and_then(|rest| rest.split_whitespace().next())
                != Some("Z")
        }
        Err(_) => true,
    }
}

/// OpenAI-compatible endpoint whose only answer is one Bash tool call.
fn serve_bash_call(command: &str) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let args = serde_json::json!({ "command": command }).to_string();
    let chunk = serde_json::json!({
        "id": "c", "object": "chat.completion.chunk", "model": "m",
        "choices": [{
            "index": 0,
            "delta": { "role": "assistant", "tool_calls": [{
                "index": 0, "id": "call_1", "type": "function",
                "function": { "name": "Bash", "arguments": args }
            }]},
            "finish_reason": "tool_calls"
        }]
    });
    let body = format!("data: {chunk}\n\ndata: [DONE]\n\n");
    std::thread::spawn(move || {
        let Ok((mut sock, _)) = listener.accept() else {
            return;
        };
        // Read the headers and the Content-Length body before answering.
        let mut raw = Vec::new();
        let mut buf = [0u8; 8192];
        loop {
            let n = sock.read(&mut buf).unwrap_or(0);
            if n == 0 {
                return;
            }
            raw.extend_from_slice(&buf[..n]);
            let text = String::from_utf8_lossy(&raw);
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
                    break;
                }
            }
        }
        let _ = write!(
            sock,
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        // Keep the listener alive so a follow-up request would hang, not fail.
        std::thread::sleep(Duration::from_secs(60));
    });
    port
}

fn wait_for(total: Duration, mut done: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + total;
    while Instant::now() < deadline {
        if done() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    done()
}

fn interrupted_print_kills_the_tool(signal: i32, expected_code: i32) {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let project = tmp.path().join("project");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&project).unwrap();
    let pid_file = tmp.path().join("grandchild.pid");
    let port = serve_bash_call(&format!(
        "sleep 30 & echo $! > {}; wait",
        pid_file.display()
    ));

    let mut child = Command::new(env!("CARGO_BIN_EXE_oxideclaw"))
        .args([
            "-p",
            "--dangerously-skip-permissions",
            "--model",
            "openai-compat:test",
            "go",
        ])
        .current_dir(&project)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_DATA_HOME", home.join(".local/share"))
        .env("XDG_CACHE_HOME", home.join(".cache"))
        .env("OPENAI_BASE_URL", format!("http://127.0.0.1:{port}/v1"))
        .env("OPENAI_API_KEY", "test")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let started = wait_for(Duration::from_secs(30), || {
        std::fs::read_to_string(&pid_file).is_ok_and(|s| s.trim().parse::<i32>().is_ok())
    });
    if !started {
        let _ = child.kill();
        let mut err = String::new();
        let _ = child.stderr.take().unwrap().read_to_string(&mut err);
        panic!("Bash tool never ran; stderr:\n{err}");
    }
    let grandchild: i32 = std::fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(pid_alive(grandchild));

    // SAFETY: plain kill(2) on the child we spawned.
    unsafe { libc::kill(child.id() as i32, signal) };
    let mut status = None;
    wait_for(Duration::from_secs(10), || {
        status = child.try_wait().unwrap();
        status.is_some()
    });
    let Some(status) = status else {
        let _ = child.kill();
        // SAFETY: clean up the orphan so it does not outlive the test.
        unsafe { libc::kill(grandchild, libc::SIGKILL) };
        panic!("oxideclaw did not exit after the signal");
    };

    let died = wait_for(Duration::from_secs(5), || !pid_alive(grandchild));
    if !died {
        // SAFETY: clean up the orphan so it does not outlive the test.
        unsafe { libc::kill(grandchild, libc::SIGKILL) };
    }
    assert!(
        died,
        "the Bash tool's background process outlived oxideclaw"
    );
    assert_eq!(status.code(), Some(expected_code));
}

#[test]
fn sigint_during_print_kills_the_running_tool() {
    interrupted_print_kills_the_tool(libc::SIGINT, 130);
}

#[test]
fn sigterm_during_print_kills_the_running_tool() {
    interrupted_print_kills_the_tool(libc::SIGTERM, 143);
}
