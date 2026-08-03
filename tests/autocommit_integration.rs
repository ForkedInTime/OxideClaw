//! End-to-end tests for the auto-commit loop against real `git init` tempdirs.

#![cfg(unix)]

use rustyclaw::autocommit::{
    AutoCommitConfig, SHADOW_REF_PREFIX, SnapshotOutcome, is_git_repo, prune_old_refs, restore_to,
    snapshot_turn,
};
use std::fs;
use std::path::Path;
use std::process::Command;
use tempfile::TempDir;

fn git_init(path: &Path) {
    let s = Command::new("git")
        .args(["init", "--quiet", "--initial-branch=main"])
        .current_dir(path)
        .status()
        .unwrap();
    assert!(s.success());
    for (k, v) in [
        ("user.name", "rustyclaw-test"),
        ("user.email", "noreply@rustyclaw.local"),
        ("commit.gpgsign", "false"),
    ] {
        Command::new("git")
            .args(["config", k, v])
            .current_dir(path)
            .status()
            .unwrap();
    }
}

fn write(path: &Path, rel: &str, body: &str) {
    let p = path.join(rel);
    if let Some(parent) = p.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(p, body).unwrap();
}

fn read(path: &Path, rel: &str) -> String {
    fs::read_to_string(path.join(rel)).unwrap()
}

fn commit_initial(path: &Path) {
    write(path, "README.md", "base\n");
    Command::new("git")
        .args(["add", "README.md"])
        .current_dir(path)
        .status()
        .unwrap();
    Command::new("git")
        .args(["commit", "-q", "-m", "base"])
        .current_dir(path)
        .status()
        .unwrap();
}

#[test]
fn full_3_turn_sequence_with_undo_redo() {
    let td = TempDir::new().unwrap();
    git_init(td.path());
    commit_initial(td.path());

    let cfg = AutoCommitConfig::default();
    let mut commits: Vec<String> = Vec::new();
    let mut pos = 0usize;

    write(td.path(), "app.txt", "v1\n");
    snapshot_turn(td.path(), &cfg, "s", "turn 1", 1, &mut commits, &mut pos).unwrap();
    write(td.path(), "app.txt", "v2\n");
    snapshot_turn(td.path(), &cfg, "s", "turn 2", 2, &mut commits, &mut pos).unwrap();
    write(td.path(), "app.txt", "v3\n");
    snapshot_turn(td.path(), &cfg, "s", "turn 3", 3, &mut commits, &mut pos).unwrap();

    assert_eq!(commits.len(), 3);
    assert_eq!(pos, 3);
    assert_eq!(read(td.path(), "app.txt"), "v3\n");

    // /undo 1 → turn 2 state
    restore_to(td.path(), &commits, 2).unwrap();
    pos -= 1;
    assert_eq!(pos, 2);
    assert_eq!(read(td.path(), "app.txt"), "v2\n");

    // /redo 1 → turn 3 state
    restore_to(td.path(), &commits, 3).unwrap();
    pos += 1;
    assert_eq!(pos, 3);
    assert_eq!(read(td.path(), "app.txt"), "v3\n");
}

#[test]
fn undo_past_session_start_is_clamped() {
    let td = TempDir::new().unwrap();
    git_init(td.path());
    commit_initial(td.path());

    let cfg = AutoCommitConfig::default();
    let mut commits: Vec<String> = Vec::new();
    let mut pos = 0usize;

    write(td.path(), "a.txt", "1\n");
    snapshot_turn(td.path(), &cfg, "s", "a", 1, &mut commits, &mut pos).unwrap();
    write(td.path(), "a.txt", "2\n");
    snapshot_turn(td.path(), &cfg, "s", "b", 2, &mut commits, &mut pos).unwrap();

    // Simulate `/undo 5` via saturating_sub → target_position = 0 (session base)
    let new_pos = pos.saturating_sub(5);
    assert_eq!(new_pos, 0);
    let report = restore_to(td.path(), &commits, new_pos).unwrap();
    // Session base had the initial "base\n" README. a.txt exists only in the
    // shadow commits, so restoring to position 0 reports it as orphaned (Task 5
    // reports orphans rather than deleting them; the caller decides the policy).
    assert!(
        report.orphaned_files.iter().any(|p| p.ends_with("a.txt")),
        "expected a.txt to be reported as orphaned, got {:?}",
        report.orphaned_files
    );
    assert_eq!(read(td.path(), "README.md"), "base\n");
}

#[test]
fn redo_stack_discarded_on_new_turn() {
    let td = TempDir::new().unwrap();
    git_init(td.path());
    commit_initial(td.path());

    let cfg = AutoCommitConfig::default();
    let mut commits: Vec<String> = Vec::new();
    let mut pos = 0usize;

    write(td.path(), "a.txt", "1\n");
    snapshot_turn(td.path(), &cfg, "s", "a", 1, &mut commits, &mut pos).unwrap();
    write(td.path(), "a.txt", "2\n");
    snapshot_turn(td.path(), &cfg, "s", "b", 2, &mut commits, &mut pos).unwrap();
    write(td.path(), "a.txt", "3\n");
    snapshot_turn(td.path(), &cfg, "s", "c", 3, &mut commits, &mut pos).unwrap();
    assert_eq!(commits.len(), 3);

    // Simulate /undo 2 → pos = 1
    pos = 1;
    write(td.path(), "a.txt", "new2\n");
    snapshot_turn(td.path(), &cfg, "s", "new", 2, &mut commits, &mut pos).unwrap();
    assert_eq!(commits.len(), 2, "old turns 2 + 3 should be discarded");
    assert_eq!(pos, 2);
}

#[test]
fn disabled_config_emits_no_commits() {
    let td = TempDir::new().unwrap();
    git_init(td.path());
    commit_initial(td.path());

    let cfg = AutoCommitConfig {
        enabled: false,
        ..AutoCommitConfig::default()
    };
    let mut commits: Vec<String> = Vec::new();
    let mut pos = 0usize;

    write(td.path(), "x.txt", "1\n");
    let out = snapshot_turn(td.path(), &cfg, "s", "x", 1, &mut commits, &mut pos).unwrap();
    assert!(matches!(out, SnapshotOutcome::Disabled { .. }));
    assert!(commits.is_empty());
    assert_eq!(pos, 0);
}

#[test]
fn non_git_dir_is_disabled() {
    let td = TempDir::new().unwrap(); // no git init
    assert!(!is_git_repo(td.path()));
    let cfg = AutoCommitConfig::default();
    let mut commits: Vec<String> = Vec::new();
    let mut pos = 0usize;
    let out = snapshot_turn(td.path(), &cfg, "s", "x", 1, &mut commits, &mut pos).unwrap();
    assert!(matches!(out, SnapshotOutcome::Disabled { .. }));
}

#[test]
fn prune_integration_15_refs_keeps_10() {
    let td = TempDir::new().unwrap();
    git_init(td.path());
    commit_initial(td.path());

    let cfg = AutoCommitConfig::default();
    // Create 15 sessions with 1 turn each.
    for i in 0..15 {
        let mut commits: Vec<String> = Vec::new();
        let mut pos = 0usize;
        write(td.path(), "app.txt", &format!("v{i}\n"));
        snapshot_turn(
            td.path(),
            &cfg,
            &format!("s{i}"),
            "t",
            1,
            &mut commits,
            &mut pos,
        )
        .unwrap();
    }

    let deleted = prune_old_refs(td.path(), 10).unwrap();
    assert_eq!(deleted, 5);

    let out = Command::new("git")
        .args(["for-each-ref", "--format=%(refname)", SHADOW_REF_PREFIX])
        .current_dir(td.path())
        .output()
        .unwrap();
    let remaining = String::from_utf8(out.stdout).unwrap();
    assert_eq!(remaining.lines().count(), 10);
}

/// Two rustyclaw instances sharing a session id each build their own commit
/// chain in memory and both write the same shadow ref. Before compare-and-swap
/// the later `update-ref` silently orphaned the other's history — losing exactly
/// the turns `/undo` exists to reach.
///
/// Simulated by moving the ref out from under an in-flight session, which is
/// what a concurrent writer looks like from this process's point of view.
#[test]
fn concurrent_ref_write_is_detected_not_clobbered() {
    let td = TempDir::new().unwrap();
    git_init(td.path());
    commit_initial(td.path());

    let cfg = AutoCommitConfig::default();
    let mut commits: Vec<String> = Vec::new();
    let mut pos = 0usize;

    // Our instance records one turn.
    write(td.path(), "a.txt", "one\n");
    snapshot_turn(td.path(), &cfg, "shared", "t1", 1, &mut commits, &mut pos).unwrap();
    let ours = commits.last().cloned().expect("first turn committed");

    // Another instance writes the same ref behind our back.
    let refname = format!("{SHADOW_REF_PREFIX}shared");
    let head = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(td.path())
        .output()
        .unwrap();
    let other = String::from_utf8(head.stdout).unwrap().trim().to_string();
    assert!(
        Command::new("git")
            .args(["update-ref", &refname, &other])
            .current_dir(td.path())
            .status()
            .unwrap()
            .success()
    );

    // Our next turn must refuse rather than overwrite the other writer.
    write(td.path(), "a.txt", "two\n");
    let outcome =
        snapshot_turn(td.path(), &cfg, "shared", "t2", 2, &mut commits, &mut pos).unwrap();
    assert!(
        matches!(outcome, SnapshotOutcome::Conflict { .. }),
        "expected Conflict, got {outcome:?}"
    );

    // The other writer's value survived — we did not clobber it.
    let now = Command::new("git")
        .args(["rev-parse", &refname])
        .current_dir(td.path())
        .output()
        .unwrap();
    let now = String::from_utf8(now.stdout).unwrap().trim().to_string();
    assert_eq!(now, other, "the concurrent writer's ref must be intact");
    assert_ne!(now, ours);
}

/// A conflict must not damage the working tree — the user's files are theirs,
/// and a bookkeeping failure is not a reason to touch them.
#[test]
fn conflict_leaves_the_working_tree_untouched() {
    let td = TempDir::new().unwrap();
    git_init(td.path());
    commit_initial(td.path());

    let cfg = AutoCommitConfig::default();
    let mut commits: Vec<String> = Vec::new();
    let mut pos = 0usize;

    write(td.path(), "w.txt", "v1\n");
    snapshot_turn(td.path(), &cfg, "s", "t1", 1, &mut commits, &mut pos).unwrap();

    let refname = format!("{SHADOW_REF_PREFIX}s");
    let head = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(td.path())
        .output()
        .unwrap();
    let other = String::from_utf8(head.stdout).unwrap().trim().to_string();
    Command::new("git")
        .args(["update-ref", &refname, &other])
        .current_dir(td.path())
        .status()
        .unwrap();

    write(td.path(), "w.txt", "v2-user-edit\n");
    let _ = snapshot_turn(td.path(), &cfg, "s", "t2", 2, &mut commits, &mut pos).unwrap();

    let on_disk = std::fs::read_to_string(td.path().join("w.txt")).unwrap();
    assert_eq!(on_disk, "v2-user-edit\n", "working tree must be untouched");
}
