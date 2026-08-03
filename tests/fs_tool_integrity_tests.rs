//! Phase 2, data-integrity half: writes must not corrupt, MultiEdit must be
//! atomic as documented, and result sets must be bounded.

use rustyclaw::tools::{
    Tool, ToolContext, file_write::FileWriteTool, glob::GlobTool, multi_edit::MultiEditTool,
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

/// MultiEdit documented its edits as atomic but wrote inside the loop, so a
/// batch where one edit failed left the others on disk — a half-finished
/// refactor, reported as a single "✗" among several "✓".
#[tokio::test]
async fn multi_edit_writes_nothing_to_a_file_when_any_edit_to_it_fails() {
    let td = TempDir::new().unwrap();
    let ctx = ToolContext::new(PathBuf::from(td.path()));
    let original = "one\ntwo\nthree\n";
    std::fs::write(td.path().join("a.txt"), original).unwrap();

    let out = MultiEditTool
        .execute(
            json!({"edits": [
                {"file_path": "a.txt", "old_string": "one",     "new_string": "ONE"},
                {"file_path": "a.txt", "old_string": "MISSING", "new_string": "X"},
                {"file_path": "a.txt", "old_string": "three",   "new_string": "THREE"}
            ]}),
            &ctx,
        )
        .await
        .unwrap();

    assert!(out.is_error, "a failed edit must surface as an error");
    assert_eq!(
        std::fs::read_to_string(td.path().join("a.txt")).unwrap(),
        original,
        "file must be byte-identical when any edit to it failed"
    );
}

/// A failure in one file must not discard good edits to another — the
/// granularity is per file, not per batch.
#[tokio::test]
async fn multi_edit_failure_in_one_file_does_not_block_another() {
    let td = TempDir::new().unwrap();
    let ctx = ToolContext::new(PathBuf::from(td.path()));
    std::fs::write(td.path().join("good.txt"), "alpha\n").unwrap();
    std::fs::write(td.path().join("bad.txt"), "beta\n").unwrap();

    let out = MultiEditTool
        .execute(
            json!({"edits": [
                {"file_path": "good.txt", "old_string": "alpha",   "new_string": "ALPHA"},
                {"file_path": "bad.txt",  "old_string": "MISSING", "new_string": "X"}
            ]}),
            &ctx,
        )
        .await
        .unwrap();

    assert!(out.is_error, "got: {}", text(&out));
    assert_eq!(
        std::fs::read_to_string(td.path().join("good.txt")).unwrap(),
        "ALPHA\n",
        "the succeeding file must still be written"
    );
    assert_eq!(
        std::fs::read_to_string(td.path().join("bad.txt")).unwrap(),
        "beta\n",
        "the failing file must be untouched"
    );
}

/// Sequential edits to one file must compose — staging must not lose earlier
/// edits now that writes happen once at the end.
#[tokio::test]
async fn multi_edit_applies_all_edits_when_all_succeed() {
    let td = TempDir::new().unwrap();
    let ctx = ToolContext::new(PathBuf::from(td.path()));
    std::fs::write(td.path().join("a.txt"), "one\ntwo\nthree\n").unwrap();

    let out = MultiEditTool
        .execute(
            json!({"edits": [
                {"file_path": "a.txt", "old_string": "one",   "new_string": "ONE"},
                {"file_path": "a.txt", "old_string": "two",   "new_string": "TWO"},
                {"file_path": "a.txt", "old_string": "three", "new_string": "THREE"}
            ]}),
            &ctx,
        )
        .await
        .unwrap();

    assert!(!out.is_error, "got: {}", text(&out));
    assert_eq!(
        std::fs::read_to_string(td.path().join("a.txt")).unwrap(),
        "ONE\nTWO\nTHREE\n",
        "every edit must be present"
    );
}

/// An edit chained onto a previous edit's output must see it.
#[tokio::test]
async fn multi_edit_later_edits_see_earlier_ones() {
    let td = TempDir::new().unwrap();
    let ctx = ToolContext::new(PathBuf::from(td.path()));
    std::fs::write(td.path().join("a.txt"), "aaa\n").unwrap();

    MultiEditTool
        .execute(
            json!({"edits": [
                {"file_path": "a.txt", "old_string": "aaa", "new_string": "bbb"},
                {"file_path": "a.txt", "old_string": "bbb", "new_string": "ccc"}
            ]}),
            &ctx,
        )
        .await
        .unwrap();

    assert_eq!(
        std::fs::read_to_string(td.path().join("a.txt")).unwrap(),
        "ccc\n",
        "the second edit must operate on the first edit's result"
    );
}

/// Renaming replaces the inode, so an atomic write must carry the original
/// mode across — otherwise editing a 0600 file silently republishes it at 0644.
#[cfg(unix)]
#[tokio::test]
async fn atomic_write_preserves_file_permissions() {
    // Imported here rather than at module scope: this is the only user, and the
    // test is unix-only, so a top-level import is dead code on Windows and
    // trips `-D warnings` there.
    use rustyclaw::tools::file_edit::FileEditTool;
    use std::os::unix::fs::PermissionsExt;
    let td = TempDir::new().unwrap();
    let ctx = ToolContext::new(PathBuf::from(td.path()));
    let p = td.path().join("secret.conf");
    std::fs::write(&p, "token=abc\n").unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();

    FileEditTool
        .execute(
            json!({"file_path": "secret.conf", "old_string": "abc", "new_string": "xyz"}),
            &ctx,
        )
        .await
        .unwrap();

    let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "permissions must survive the rename, got {mode:o}");
    assert_eq!(std::fs::read_to_string(&p).unwrap(), "token=xyz\n");
}

/// The temp file used for the atomic rename must never be left behind.
#[tokio::test]
async fn atomic_write_leaves_no_temp_files() {
    let td = TempDir::new().unwrap();
    let ctx = ToolContext::new(PathBuf::from(td.path()));

    FileWriteTool
        .execute(json!({"file_path": "out.txt", "content": "hello\n"}), &ctx)
        .await
        .unwrap();

    let strays: Vec<_> = std::fs::read_dir(td.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains("rustyclaw-") || n.ends_with(".tmp"))
        .collect();
    assert!(strays.is_empty(), "temp files left behind: {strays:?}");
    assert_eq!(std::fs::read_to_string(td.path().join("out.txt")).unwrap(), "hello\n");
}

/// A broad pattern over a large tree must not build an unbounded result.
#[tokio::test]
async fn glob_results_are_bounded() {
    let td = TempDir::new().unwrap();
    for i in 0..1500 {
        std::fs::write(td.path().join(format!("f{i:05}.txt")), "x").unwrap();
    }
    let ctx = ToolContext::new(PathBuf::from(td.path()));
    let out = GlobTool
        .execute(json!({"pattern": "**/*.txt"}), &ctx)
        .await
        .unwrap();
    let body = text(&out);

    let listed = body.lines().filter(|l| l.ends_with(".txt")).count();
    assert!(listed <= 1000, "expected a cap, got {listed} entries");
    assert!(
        body.contains("of 1500 matches shown"),
        "truncation must be stated, not silent: {}",
        &body[body.len().saturating_sub(200)..]
    );
}

/// Ordinary searches must not be truncated or gain a spurious notice.
#[tokio::test]
async fn glob_small_result_sets_are_untouched() {
    let td = TempDir::new().unwrap();
    for i in 0..5 {
        std::fs::write(td.path().join(format!("f{i}.txt")), "x").unwrap();
    }
    let ctx = ToolContext::new(PathBuf::from(td.path()));
    let body = text(&GlobTool.execute(json!({"pattern": "**/*.txt"}), &ctx).await.unwrap());
    assert_eq!(body.lines().filter(|l| l.ends_with(".txt")).count(), 5);
    assert!(!body.contains("matches shown"), "no notice expected: {body}");
}
