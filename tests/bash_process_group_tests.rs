//! Regression: BashTool must kill the entire child process group when its
//! execution future is dropped (Esc mid-run) or the tool times out.
//!
//! Before the fix, tokio::process::Child dropped on abort did NOT kill the
//! running shell process — and even if it had, any grandchildren spawned by
//! `sh -c '... & ...'` would be reparented to init and continue running as
//! orphans. This test arms a long-lived grandchild, aborts the tool, and
//! verifies the grandchild is dead shortly after.
//!
//! Unix-only: process-group semantics are POSIX.

#![cfg(unix)]

use oxideclaw::tools::{Tool, ToolContext, bash::BashTool};
use serde_json::json;
use std::path::PathBuf;
use std::time::Duration;
use tempfile::TempDir;

/// Returns true if a process with `pid` is alive (kill(pid, 0) == 0).
fn pid_alive(pid: i32) -> bool {
    // SAFETY: kill(pid, 0) with sig=0 only performs permission/existence
    // checking and does not deliver a signal.
    unsafe { libc::kill(pid, 0) == 0 }
}

/// Wait up to `total` for the pid to die; polls every 50ms.
async fn wait_for_death(pid: i32, total: Duration) -> bool {
    let deadline = std::time::Instant::now() + total;
    while std::time::Instant::now() < deadline {
        if !pid_alive(pid) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    !pid_alive(pid)
}

#[tokio::test]
async fn bash_tool_timeout_kills_grandchild_in_process_group() {
    let tmp = TempDir::new().unwrap();
    let pid_file = tmp.path().join("grandchild.pid");

    // Background a long-lived sleep, record its PID, then have the shell
    // itself sleep long enough to trip the tool timeout. The fix must kill
    // BOTH the shell AND the backgrounded sleep (the whole process group).
    let cmd = format!("sleep 30 & echo $! > {}; sleep 30", pid_file.display());

    let mut ctx = ToolContext::new(PathBuf::from("/tmp"));
    ctx.default_shell = Some("bash".into());

    let tool = BashTool;
    let input = json!({
        "command": cmd,
        "timeout": 600u64, // 0.6s — enough time to arm the pid file
    });

    // Race: tool should time out after ~600ms and return an error.
    let result = tool.execute(input, &ctx).await.expect("tool returned Err");
    assert!(result.is_error, "expected timeout error");

    // Read the grandchild pid the shell wrote.
    // Small grace window so the shell's redirection lands before we read.
    let mut pid: Option<i32> = None;
    for _ in 0..20 {
        if let Ok(s) = std::fs::read_to_string(&pid_file)
            && let Ok(p) = s.trim().parse::<i32>()
        {
            pid = Some(p);
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let pid = pid.expect("grandchild pid file never appeared");

    // Give the kill signal ~1.5s to propagate through the process group.
    let dead = wait_for_death(pid, Duration::from_millis(1500)).await;
    if !dead {
        // Best-effort cleanup so we don't leak sleeps on failure.
        unsafe {
            libc::kill(pid, libc::SIGKILL);
        }
        panic!(
            "grandchild pid {pid} still alive after BashTool timeout — process group NOT killed"
        );
    }
}

#[tokio::test]
async fn bash_tool_abort_kills_grandchild_in_process_group() {
    let tmp = TempDir::new().unwrap();
    let pid_file = tmp.path().join("grandchild2.pid");

    let cmd = format!("sleep 30 & echo $! > {}; sleep 30", pid_file.display());

    let ctx = {
        let mut c = ToolContext::new(PathBuf::from("/tmp"));
        c.default_shell = Some("bash".into());
        c
    };
    let input = json!({
        "command": cmd,
        "timeout": 30_000u64, // long — we'll abort manually
    });

    // Spawn the tool execution on its own task so we can abort it, simulating
    // a user pressing Esc mid-run.
    let handle = tokio::spawn(async move { BashTool.execute(input, &ctx).await });

    // Wait for the pid file to be written by the shell.
    let mut pid: Option<i32> = None;
    for _ in 0..40 {
        if let Ok(s) = std::fs::read_to_string(&pid_file)
            && let Ok(p) = s.trim().parse::<i32>()
        {
            pid = Some(p);
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let pid = pid.expect("grandchild pid file never appeared");
    assert!(pid_alive(pid), "grandchild should be alive before abort");

    // Abort — this drops the tool's future, which must kill the process group.
    handle.abort();
    let _ = handle.await; // should be JoinError::Cancelled

    let dead = wait_for_death(pid, Duration::from_millis(1500)).await;
    if !dead {
        unsafe {
            libc::kill(pid, libc::SIGKILL);
        }
        panic!("grandchild pid {pid} still alive after BashTool abort — process group NOT killed");
    }
}
