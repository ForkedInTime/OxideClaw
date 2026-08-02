//! Regression: BashTool must bound how much command output it buffers.
//!
//! `BufReader::lines()` accumulates bytes until it sees a newline, so a command
//! that produces a large newline-free stream (`yes | tr -d '\n'`,
//! `cat /dev/urandom | tr -d '\n'`) buffered the entire stream into a single
//! String. The MAX_OUTPUT_BYTES check ran *per line* and so never got a chance
//! to trip — memory grew until the 2-minute timeout fired, or the process died.
//!
//! The fix caps each pipe at the source with `AsyncReadExt::take`, which makes
//! the limit real regardless of whether the output contains newlines.
//!
//! Unix-only: uses `head`/`tr` and /dev/zero.

#![cfg(unix)]

use rustyclaw::tools::{Tool, ToolContext, bash::BashTool};
use serde_json::json;
use std::path::PathBuf;
use tempfile::TempDir;

/// BashTool's internal cap. Output may exceed it slightly (the final line plus
/// the truncation notice), so assertions allow generous headroom while still
/// being far below the unbounded size.
const MAX_OUTPUT_BYTES: usize = 1_000_000;

fn ctx(dir: &TempDir) -> ToolContext {
    ToolContext::new(PathBuf::from(dir.path()))
}

/// Flatten a ToolOutput's content blocks into one string.
fn text(out: &rustyclaw::tools::ToolOutput) -> String {
    out.content
        .iter()
        .map(|c| match c {
            rustyclaw::api::types::ToolResultContent::Text { text } => text.as_str(),
        })
        .collect::<Vec<_>>()
        .join("")
}

/// 8 MB of output with **no newline at all** — the pathological shape.
/// Before the fix this buffered all 8 MB into one String.
#[tokio::test]
async fn newline_free_output_is_bounded() {
    let dir = TempDir::new().unwrap();
    let out = BashTool
        .execute(
            json!({
                "command": "head -c 8000000 /dev/zero | tr '\\0' 'a'",
                "timeout": 60000
            }),
            &ctx(&dir),
        )
        .await
        .expect("tool should return a result, not hang or error out");

    let len = text(&out).len();
    assert!(
        len < MAX_OUTPUT_BYTES * 3,
        "newline-free output must be bounded, got {len} bytes (input was 8 MB)"
    );
}

/// The same volume split across many lines — the case the per-line check
/// already handled. Kept so a future refactor can't regress one shape while
/// fixing the other.
#[tokio::test]
async fn line_delimited_output_is_bounded() {
    let dir = TempDir::new().unwrap();
    let out = BashTool
        .execute(
            json!({
                "command": "for i in $(seq 1 200000); do echo aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa; done",
                "timeout": 60000
            }),
            &ctx(&dir),
        )
        .await
        .expect("tool should return a result");

    let len = text(&out).len();
    assert!(
        len < MAX_OUTPUT_BYTES * 3,
        "line-delimited output must be bounded, got {len} bytes"
    );
}

/// Bounding the reader must not truncate ordinary small output.
#[tokio::test]
async fn small_output_is_unaffected() {
    let dir = TempDir::new().unwrap();
    let out = BashTool
        .execute(json!({ "command": "echo hello world" }), &ctx(&dir))
        .await
        .expect("tool should return a result");

    let body = text(&out);
    assert!(body.contains("hello world"), "got: {body}");
}

/// stdin is redirected from /dev/null, so a command that reads stdin gets EOF
/// immediately instead of competing with the TUI for the user's keystrokes.
#[tokio::test]
async fn stdin_is_not_inherited() {
    let dir = TempDir::new().unwrap();
    let out = BashTool
        .execute(
            json!({ "command": "cat; echo EOF_REACHED", "timeout": 10000 }),
            &ctx(&dir),
        )
        .await
        .expect("reading stdin must not hang — it should hit EOF immediately");

    let body = text(&out);
    assert!(
        body.contains("EOF_REACHED"),
        "command should complete on stdin EOF, got: {body}"
    );
}
