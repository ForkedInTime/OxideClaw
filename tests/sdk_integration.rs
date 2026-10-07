// tests/sdk_integration.rs
//! Integration test: spawn oxideclaw --headless, send health/check, verify response.

use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

#[tokio::test]
async fn test_health_check_via_headless() {
    // Cargo builds the binary for integration tests and exposes its path;
    // shelling out to `cargo run` deadlocks on the build lock under `cargo test`.
    // A temp home: never read or migrate the real ~/.claude or config dir.
    let home = tempfile::tempdir().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_oxideclaw"))
        .arg("--headless")
        .env("HOME", home.path())
        .env("XDG_CONFIG_HOME", home.path().join(".config"))
        .env("XDG_DATA_HOME", home.path().join(".local/share"))
        .env("XDG_CACHE_HOME", home.path().join(".cache"))
        .env_remove("OXIDECLAW_CONFIG_DIR")
        .env_remove("RUSTYCLAW_CONFIG_DIR")
        .env_remove("CLAUDE_CONFIG_DIR")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("Failed to spawn oxideclaw --headless");

    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout).lines();

    // Send health/check request
    let req = r#"{"id":"test-1","type":"health/check"}"#;
    stdin.write_all(req.as_bytes()).await.unwrap();
    stdin.write_all(b"\n").await.unwrap();
    stdin.flush().await.unwrap();

    // Read response with timeout
    let line = tokio::time::timeout(std::time::Duration::from_secs(10), reader.next_line())
        .await
        .expect("Timeout waiting for response")
        .expect("IO error")
        .expect("No line received");

    let resp: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(resp["type"], "health/check");
    assert_eq!(resp["id"], "test-1");
    assert_eq!(resp["status"], "ok");
    assert!(resp["version"].is_string());
    assert!(resp["active_sessions"].is_number());
    assert!(resp["uptime_seconds"].is_number());

    // Close stdin to shut down the server
    drop(stdin);
    let status = tokio::time::timeout(std::time::Duration::from_secs(5), child.wait())
        .await
        .expect("Timeout waiting for process exit")
        .expect("Process wait error");

    // Server should exit cleanly on EOF
    assert!(status.success() || status.code() == Some(0));
}
