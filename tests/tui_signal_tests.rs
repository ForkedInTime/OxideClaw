//! Regression: closing the terminal (SIGHUP) or `kill <pid>` (SIGTERM)
//! while the TUI runs a Bash tool must kill that tool's process group. Tools
//! run in their own session, so the terminal's hangup never reaches them,
//! and a TUI that died on the default action left them running as orphans.
//!
//! Linux-only: the TUI runs behind a real pseudo-terminal.

#![cfg(target_os = "linux")]

use std::io::{Read, Write};
use std::net::TcpListener;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn pid_alive(pid: i32) -> bool {
    // SAFETY: signal 0 only checks that the process exists.
    if unsafe { libc::kill(pid, 0) } != 0 {
        return false;
    }
    // A zombie left under a container init that never reaps is dead.
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

enum Stop {
    /// Close the pty master, as a terminal emulator does when its tab
    /// closes: the tty hangs up and the kernel sends SIGHUP.
    CloseTerminal,
    Signal(i32),
}

fn stopped_tui_kills_the_tool(stop: Stop) {
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

    let (mut master, mut slave) = (-1, -1);
    let ws = libc::winsize {
        ws_row: 24,
        ws_col: 80,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: openpty writes two fds into the out-params.
    let rc = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null(),
            &ws,
        )
    };
    assert_eq!(rc, 0, "openpty failed");
    // Neither end may leak into oxideclaw or its tools: an inherited master
    // would keep the terminal open after this test closes it.
    for fd in [master, slave] {
        // SAFETY: FD_CLOEXEC on fds we own.
        unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
    }
    // SAFETY: both fds were just opened and are owned here.
    let (master, slave) = unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_oxideclaw"));
    cmd.args([
        "--dangerously-skip-permissions",
        "--model",
        "openai-compat:test",
        "go",
    ])
    .current_dir(&project)
    .env_clear()
    .env("PATH", std::env::var_os("PATH").unwrap_or_default())
    .env("TERM", "xterm-256color")
    .env("HOME", &home)
    .env("XDG_CONFIG_HOME", home.join(".config"))
    .env("XDG_DATA_HOME", home.join(".local/share"))
    .env("XDG_CACHE_HOME", home.join(".cache"))
    .env("OPENAI_BASE_URL", format!("http://127.0.0.1:{port}/v1"))
    .env("OPENAI_API_KEY", "test")
    .stdin(Stdio::from(slave.try_clone().unwrap()))
    .stdout(Stdio::from(slave.try_clone().unwrap()))
    .stderr(Stdio::from(slave.try_clone().unwrap()));
    // SAFETY: setsid and ioctl are async-signal-safe.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn().unwrap();
    drop(slave);

    // Drain the screen so the TUI never blocks on a full pty buffer, and
    // keep it for the failure message. The thread owns the only master fd,
    // so its close (on `hang_up`) is the terminal going away.
    let screen = Arc::new(Mutex::new(Vec::new()));
    let hang_up = Arc::new(AtomicBool::new(false));
    // SAFETY: O_NONBLOCK on an fd we own.
    unsafe {
        let flags = libc::fcntl(master.as_raw_fd(), libc::F_GETFL);
        libc::fcntl(master.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK);
    }
    let reader = {
        let mut master = std::fs::File::from(master);
        let (screen, hang_up) = (screen.clone(), hang_up.clone());
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while !hang_up.load(Ordering::SeqCst) {
                match master.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => screen.lock().unwrap().extend_from_slice(&buf[..n]),
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
        })
    };

    let started = wait_for(Duration::from_secs(60), || {
        std::fs::read_to_string(&pid_file).is_ok_and(|s| s.trim().parse::<i32>().is_ok())
    });
    if !started {
        let _ = child.kill();
        let out = String::from_utf8_lossy(&screen.lock().unwrap()).into_owned();
        panic!("Bash tool never ran; screen:\n{out}");
    }
    let grandchild: i32 = std::fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(pid_alive(grandchild));

    match stop {
        Stop::CloseTerminal => {
            hang_up.store(true, Ordering::SeqCst);
            reader.join().unwrap();
        }
        // SAFETY: plain kill(2) on the child we spawned.
        Stop::Signal(sig) => unsafe {
            libc::kill(child.id() as i32, sig);
        },
    }
    let mut status = None;
    wait_for(Duration::from_secs(15), || {
        status = child.try_wait().unwrap();
        status.is_some()
    });
    if status.is_none() {
        let _ = child.kill();
        // SAFETY: clean up the orphan so it does not outlive the test.
        unsafe { libc::kill(grandchild, libc::SIGKILL) };
        panic!("oxideclaw did not exit after it was stopped");
    }
    let died = wait_for(Duration::from_secs(5), || !pid_alive(grandchild));
    if !died {
        // SAFETY: clean up the orphan so it does not outlive the test.
        unsafe { libc::kill(grandchild, libc::SIGKILL) };
    }
    hang_up.store(true, Ordering::SeqCst);
    assert!(died, "the Bash tool's background process outlived the TUI");
}

#[test]
fn sigterm_during_tui_turn_kills_the_running_tool() {
    stopped_tui_kills_the_tool(Stop::Signal(libc::SIGTERM));
}

#[test]
fn closed_terminal_during_tui_turn_kills_the_running_tool() {
    stopped_tui_kills_the_tool(Stop::CloseTerminal);
}
