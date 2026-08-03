//! Phase 2: the filesystem tools must not become a way around the sensitive-path
//! deny-list.
//!
//! Two bypasses were live before these tests existed:
//!
//!   1. `check_sensitive_path` matches on the *file name* of the path it is
//!      handed, so a symlink with an innocuous name defeated every protection it
//!      offers. A link `notes.md` -> `~/.ssh/id_rsa` returned the private key,
//!      and `config.json` -> `~/.aws/credentials` overwrote them, while the same
//!      operations on the real names were correctly refused. A repository can
//!      simply ship such a link — "read the README" is enough to exfiltrate.
//!
//!   2. Grep never consulted the deny-list at all, so searching for a string
//!      inside a private key returned the key material verbatim — a read
//!      primitive that walked straight past the guard on FileRead.
//!
//! Unix-only: symlink semantics.

#![cfg(unix)]

use rustyclaw::tools::{
    Tool, ToolContext, file_read::FileReadTool, file_write::FileWriteTool, grep::GrepTool,
};
use serde_json::json;
use std::path::PathBuf;
use tempfile::TempDir;

fn text(o: &rustyclaw::tools::ToolOutput) -> String {
    o.content
        .iter()
        .map(|c| match c {
            rustyclaw::api::types::ToolResultContent::Text { text } => text.as_str(),
        })
        .collect::<Vec<_>>()
        .join("")
}

/// Secrets outside the project, a project dir, and links pointing out of it.
fn fixture() -> (TempDir, PathBuf) {
    let td = TempDir::new().unwrap();
    let ssh = td.path().join(".ssh");
    std::fs::create_dir_all(&ssh).unwrap();
    std::fs::write(ssh.join("id_rsa"), "PRIVATE-KEY-MATERIAL\n").unwrap();

    let aws = td.path().join(".aws");
    std::fs::create_dir_all(&aws).unwrap();
    std::fs::write(aws.join("credentials"), "[default]\nreal=secret\n").unwrap();

    let proj = td.path().join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    std::os::unix::fs::symlink(ssh.join("id_rsa"), proj.join("notes.md")).unwrap();
    std::os::unix::fs::symlink(aws.join("credentials"), proj.join("config.json")).unwrap();
    (td, proj)
}

#[tokio::test]
async fn symlink_cannot_be_used_to_read_a_private_key() {
    let (_td, proj) = fixture();
    let ctx = ToolContext::new(proj);
    let out = FileReadTool
        .execute(json!({"file_path": "notes.md"}), &ctx)
        .await
        .unwrap();
    assert!(out.is_error, "reading a link to a private key must be refused");
    assert!(
        !text(&out).contains("PRIVATE-KEY-MATERIAL"),
        "key material must not appear in the result"
    );
}

#[tokio::test]
async fn symlink_cannot_be_used_to_overwrite_credentials() {
    let (td, proj) = fixture();
    let creds = td.path().join(".aws").join("credentials");
    let before = std::fs::read_to_string(&creds).unwrap();

    let ctx = ToolContext::new(proj);
    let out = FileWriteTool
        .execute(json!({"file_path": "config.json", "content": "CLOBBERED\n"}), &ctx)
        .await
        .unwrap();

    assert!(out.is_error, "writing through a link to credentials must be refused");
    assert_eq!(
        std::fs::read_to_string(&creds).unwrap(),
        before,
        "the target file must be byte-identical"
    );
}

/// Ordinary files must keep working — a guard that blocks real work gets
/// disabled, which is worse than no guard.
#[tokio::test]
async fn normal_files_are_unaffected() {
    let (_td, proj) = fixture();
    let ctx = ToolContext::new(proj.clone());

    let w = FileWriteTool
        .execute(json!({"file_path": "src.rs", "content": "fn main() {}\n"}), &ctx)
        .await
        .unwrap();
    assert!(!w.is_error, "writing an ordinary file must work: {}", text(&w));

    let r = FileReadTool
        .execute(json!({"file_path": "src.rs"}), &ctx)
        .await
        .unwrap();
    assert!(!r.is_error && text(&r).contains("fn main"), "{}", text(&r));

    // A symlink to a harmless file is still fine.
    std::os::unix::fs::symlink(proj.join("src.rs"), proj.join("alias.rs")).unwrap();
    let a = FileReadTool
        .execute(json!({"file_path": "alias.rs"}), &ctx)
        .await
        .unwrap();
    assert!(!a.is_error, "benign symlinks must not be blocked: {}", text(&a));
}

/// Grep returns matching lines verbatim, so it must honour the same read
/// deny-list as FileRead. Both backends are exercised: the `rg` subprocess when
/// ripgrep is installed, and the pure-Rust fallback when it is not — a fix
/// applied to only one of them leaves the other leaking.
#[tokio::test]
async fn grep_does_not_return_private_key_contents() {
    let (_td, proj) = fixture();
    std::fs::write(
        proj.join("server.pem"),
        "-----BEGIN PRIVATE KEY-----\nPEMSECRET\n",
    )
    .unwrap();
    std::fs::write(proj.join("app.rs"), "// PEMSECRET appears here legitimately\n").unwrap();

    let ctx = ToolContext::new(proj);
    let out = GrepTool
        .execute(json!({"pattern": "PEMSECRET", "output_mode": "content"}), &ctx)
        .await
        .unwrap();
    let body = text(&out);

    assert!(
        !body.contains("server.pem"),
        "key-material file must not be searched: {body}"
    );
    assert!(
        body.contains("app.rs"),
        "ordinary files must still be searched: {body}"
    );
}

/// The same, forced down the non-ripgrep path.
#[tokio::test]
async fn grep_fallback_backend_also_honours_the_deny_list() {
    let (_td, proj) = fixture();
    std::fs::write(proj.join("key.pem"), "FALLBACKSECRET\n").unwrap();
    std::fs::write(proj.join("ok.txt"), "FALLBACKSECRET\n").unwrap();

    // An empty PATH removes `rg`, so the pure-Rust backend runs.
    let ctx = ToolContext::new(proj);
    let prev = std::env::var_os("PATH");
    // SAFETY: restored below; this test does not run concurrently with other
    // PATH users in this binary.
    unsafe { std::env::set_var("PATH", "") };
    let out = GrepTool
        .execute(json!({"pattern": "FALLBACKSECRET", "output_mode": "content"}), &ctx)
        .await
        .unwrap();
    unsafe {
        match prev {
            Some(v) => std::env::set_var("PATH", v),
            None => std::env::remove_var("PATH"),
        }
    }

    let body = text(&out);
    assert!(!body.contains("key.pem"), "fallback leaked key material: {body}");
    assert!(body.contains("ok.txt"), "fallback must still search normal files: {body}");
}
