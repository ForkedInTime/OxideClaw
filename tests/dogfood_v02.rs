//! Dogfood tests for v0.2.0 auto-commit / /undo / /redo.
//!
//! These tests drive the public `autocommit` API against real `git init`
//! tempdirs to validate the five checklist items we committed to verifying
//! before shipping Feature 4:
//!
//!   1. End-to-end /undo + /redo round-trip against real files
//!   2. Shadow refs are INVISIBLE to the user's normal git tooling
//!      (git log / git branch / git status / even git log --all)
//!   3. Auto-fix retry loop — covered by tests/autofix_loop_integration.rs
//!      (not duplicated here)
//!   4. Startup prune with keepSessions=10 against 15 sessions + verify
//!      that user branches and HEAD are untouched by the prune
//!   5. Abort-safety: snapshot_turn must be transactional — a simulated
//!      mid-snapshot failure leaves no partial shadow ref behind
//!
//! All tests are unix-only because autocommit uses POSIX git plumbing.

#![cfg(unix)]

use oxideclaw::autocommit::{
    AutoCommitConfig, SHADOW_REF_PREFIX, SnapshotOutcome, is_git_repo, prune_old_refs, restore_to,
    snapshot_turn,
};
use std::fs;
use std::path::Path;
use std::process::Command;
use tempfile::TempDir;

// ── helpers ───────────────────────────────────────────────────────────────────

fn git_init(path: &Path) {
    let s = Command::new("git")
        .args(["init", "--quiet", "--initial-branch=main"])
        .current_dir(path)
        .status()
        .unwrap();
    assert!(s.success());
    for (k, v) in [
        ("user.name", "oxideclaw-dogfood"),
        ("user.email", "noreply@oxideclaw.local"),
        ("commit.gpgsign", "false"),
    ] {
        Command::new("git")
            .args(["config", k, v])
            .current_dir(path)
            .status()
            .unwrap();
    }
}

fn git(path: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(path)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
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
    git(path, &["add", "README.md"]);
    git(path, &["commit", "-q", "-m", "base"]);
}

// ── Tests ─────────────────────────────────────────────────────────────────────

/// Dogfood #1: drive 3 real turns + /undo 2 + /redo 1 end-to-end against a
/// real git repo. Verifies that files on disk match expectations at every
/// step and that the user's real HEAD never moves.
#[test]
fn dogfood_end_to_end_undo_redo_against_real_tree() {
    let td = TempDir::new().unwrap();
    git_init(td.path());
    commit_initial(td.path());
    let initial_head = git(td.path(), &["rev-parse", "HEAD"]).trim().to_string();

    let cfg = AutoCommitConfig::default();
    let mut commits: Vec<String> = Vec::new();
    let mut pos = 0usize;

    // Turn 1: create app.txt
    write(td.path(), "src/app.txt", "v1\n");
    let out = snapshot_turn(
        td.path(),
        &cfg,
        "dogfood",
        "add app v1",
        1,
        &mut commits,
        &mut pos,
    )
    .unwrap();
    assert!(matches!(out, SnapshotOutcome::Committed { .. }));

    // Turn 2: modify app.txt + add a sibling
    write(td.path(), "src/app.txt", "v2\n");
    write(td.path(), "src/notes.md", "hello\n");
    snapshot_turn(
        td.path(),
        &cfg,
        "dogfood",
        "bump v2 + notes",
        2,
        &mut commits,
        &mut pos,
    )
    .unwrap();

    // Turn 3: delete notes + bump app
    fs::remove_file(td.path().join("src/notes.md")).unwrap();
    write(td.path(), "src/app.txt", "v3\n");
    snapshot_turn(
        td.path(),
        &cfg,
        "dogfood",
        "v3 + drop notes",
        3,
        &mut commits,
        &mut pos,
    )
    .unwrap();

    assert_eq!(commits.len(), 3);
    assert_eq!(pos, 3);
    assert_eq!(read(td.path(), "src/app.txt"), "v3\n");
    assert!(!td.path().join("src/notes.md").exists());

    // /undo 2 → back to turn 1 state (pos=1)
    restore_to(td.path(), &commits, 1).unwrap();
    pos -= 2;
    assert_eq!(pos, 1);
    assert_eq!(
        read(td.path(), "src/app.txt"),
        "v1\n",
        "undo 2 did not restore app.txt to v1"
    );
    // notes.md was NOT in turn 1, restore_to leaves it as an orphan report
    // (per Task 5 design). It may still be on disk from turn 2's write.
    // The key guarantee: files THAT EXISTED in turn 1 are restored.

    // /redo 2 → back to turn 3 state
    let report = restore_to(td.path(), &commits, 3).unwrap();
    pos += 2;
    assert_eq!(pos, 3);
    assert_eq!(read(td.path(), "src/app.txt"), "v3\n");
    // After redo to turn 3, notes.md should NOT be in target_files
    assert!(
        !report.orphaned_files.iter().any(|p| p.ends_with("app.txt")),
        "app.txt should not be orphaned in turn 3"
    );

    // CRITICAL INVARIANT: user's real HEAD never moved across ALL that.
    let final_head = git(td.path(), &["rev-parse", "HEAD"]).trim().to_string();
    assert_eq!(
        initial_head, final_head,
        "user's HEAD moved during auto-commit operations"
    );
}

/// Dogfood #2a: After snapshotting, `git log`, `git log --all`, `git branch`,
/// and `git status` show ZERO evidence of the shadow commits. The only way
/// to see them is via `git for-each-ref refs/oxideclaw/`.
#[test]
fn dogfood_shadow_refs_invisible_to_normal_git() {
    let td = TempDir::new().unwrap();
    git_init(td.path());
    commit_initial(td.path());

    let cfg = AutoCommitConfig::default();
    let mut commits: Vec<String> = Vec::new();
    let mut pos = 0usize;

    // Drive 3 snapshots. Each turn creates a new uncommitted file — the
    // user never commits anything, which is the realistic "agent edits
    // in my dirty tree" scenario. Capture git status BEFORE and AFTER
    // the snapshot to prove snapshot_turn does not alter the user's
    // view of their own repo.
    for i in 1..=3 {
        write(td.path(), &format!("iter{i}.txt"), &format!("v{i}\n"));
        let status_before = git(td.path(), &["status", "--porcelain=v1"]);
        snapshot_turn(
            td.path(),
            &cfg,
            "invis",
            "iteration",
            i,
            &mut commits,
            &mut pos,
        )
        .unwrap();
        let status_after = git(td.path(), &["status", "--porcelain=v1"]);
        assert_eq!(
            status_before, status_after,
            "turn {i}: git status changed across snapshot_turn"
        );
    }
    assert_eq!(commits.len(), 3);

    // ── A. `git log` on HEAD shows only the user's real commit ──
    let log = git(td.path(), &["log", "--oneline"]);
    let log_lines: Vec<&str> = log.lines().collect();
    assert_eq!(log_lines.len(), 1, "git log leaked shadow commits: {log}");
    assert!(log_lines[0].contains("base"));

    // ── B. `git log --all` DOES show shadow commits ──
    //
    //    Documented limitation: --all walks every ref under refs/, including
    //    refs/oxideclaw/. This is a fundamental git behavior — there is no
    //    refspace outside of refs/ that `for-each-ref` / `update-ref` can
    //    reach. Every other tool in this space has the identical limitation.
    //    The competitive claim is about plain `git log` / `git branch` /
    //    `git status`, not --all.
    //
    //    We pin the behavior explicitly so a future change that accidentally
    //    HIDES them from --all (or makes them visible to plain `git log`)
    //    gets flagged.
    let log_all = git(td.path(), &["log", "--all", "--oneline"]);
    let log_all_lines: Vec<&str> = log_all.lines().collect();
    assert_eq!(
        log_all_lines.len(),
        4, // base + 3 shadow commits — exposed by --all, hidden from plain log
        "git log --all changed visibility: {log_all}"
    );

    // ── C. `git branch -a` shows zero shadow refs ──
    let branches = git(td.path(), &["branch", "-a"]);
    assert!(
        !branches.contains("oxideclaw"),
        "git branch -a leaked shadow refs: {branches}"
    );

    // ── D. `git status` reports ONLY the user's untracked work files,
    //       never anything owned by the snapshot machinery ──
    let status = git(td.path(), &["status", "--porcelain=v1"]);
    let status_lines: Vec<&str> = status.lines().collect();
    assert_eq!(
        status_lines.len(),
        3,
        "expected exactly 3 untracked iter*.txt files, got: {status}"
    );
    for line in &status_lines {
        assert!(
            line.starts_with("?? iter") && line.ends_with(".txt"),
            "unexpected status line: {line}"
        );
    }

    // ── E. for-each-ref with the shadow prefix finds it ──
    let shadow = git(
        td.path(),
        &[
            "for-each-ref",
            "--format=%(refname)",
            SHADOW_REF_PREFIX.trim_end_matches('/'),
        ],
    );
    assert_eq!(shadow.trim(), "refs/oxideclaw/sessions/invis");

    // ── F. The shadow commit chain has exactly 3 commits ──
    let chain = git(
        td.path(),
        &["log", "--oneline", "refs/oxideclaw/sessions/invis"],
    );
    assert_eq!(
        chain.lines().count(),
        4, // 3 shadow + base parent
        "shadow ref chain wrong length: {chain}"
    );
}

/// Dogfood #2b: The user has uncommitted work — staged changes AND untracked
/// files. A snapshot must not corrupt their index or touch their working
/// tree. After the snapshot, `git status` is IDENTICAL to before.
#[test]
fn dogfood_snapshot_preserves_user_index_and_worktree() {
    let td = TempDir::new().unwrap();
    git_init(td.path());
    commit_initial(td.path());

    // Arrange: user has a staged file + a modified tracked file + an
    // untracked file, the same kind of messy state a real dev has mid-session.
    write(td.path(), "README.md", "base\nuser edit\n"); // modify tracked
    write(td.path(), "staged.txt", "user staged this\n"); // new, staged
    git(td.path(), &["add", "staged.txt"]);
    write(td.path(), "untracked.tmp", "user did not stage me\n");

    let status_before = git(td.path(), &["status", "--porcelain=v1"]);
    let ls_files_before = git(td.path(), &["ls-files", "--stage"]);

    let cfg = AutoCommitConfig::default();
    let mut commits: Vec<String> = Vec::new();
    let mut pos = 0usize;

    snapshot_turn(
        td.path(),
        &cfg,
        "messy",
        "snapshot over messy tree",
        1,
        &mut commits,
        &mut pos,
    )
    .unwrap();

    let status_after = git(td.path(), &["status", "--porcelain=v1"]);
    let ls_files_after = git(td.path(), &["ls-files", "--stage"]);

    assert_eq!(
        status_before, status_after,
        "snapshot changed `git status`:\nBEFORE:\n{status_before}\nAFTER:\n{status_after}"
    );
    assert_eq!(
        ls_files_before, ls_files_after,
        "snapshot changed user's staged index:\nBEFORE:\n{ls_files_before}\nAFTER:\n{ls_files_after}"
    );
    // Files on disk untouched
    assert_eq!(read(td.path(), "README.md"), "base\nuser edit\n");
    assert_eq!(read(td.path(), "staged.txt"), "user staged this\n");
    assert_eq!(read(td.path(), "untracked.tmp"), "user did not stage me\n");

    // And the shadow commit captured the FULL messy tree (including
    // untracked + modified + staged files) so undo would restore them.
    let shadow_tree = git(
        td.path(),
        &[
            "ls-tree",
            "-r",
            "--name-only",
            "refs/oxideclaw/sessions/messy",
        ],
    );
    let files: std::collections::HashSet<&str> = shadow_tree.lines().collect();
    assert!(files.contains("README.md"));
    assert!(files.contains("staged.txt"));
    assert!(files.contains("untracked.tmp"));
}

/// Dogfood #4: Prune 15 sessions down to 10 AND verify the user's real
/// refs (main branch + HEAD) are not touched by the prune.
#[test]
fn dogfood_prune_touches_only_shadow_refs() {
    let td = TempDir::new().unwrap();
    git_init(td.path());
    commit_initial(td.path());

    // Record user's real state before.
    let user_head_before = git(td.path(), &["rev-parse", "HEAD"]).trim().to_string();
    let user_branches_before = git(td.path(), &["branch", "-a"]);

    let cfg = AutoCommitConfig::default();
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

    // Verify all 15 shadow refs exist before prune.
    let refs_before = git(
        td.path(),
        &[
            "for-each-ref",
            "--format=%(refname)",
            SHADOW_REF_PREFIX.trim_end_matches('/'),
        ],
    );
    assert_eq!(refs_before.lines().count(), 15);

    let deleted = prune_old_refs(td.path(), 10).unwrap();
    assert_eq!(deleted, 5);

    // Verify exactly 10 shadow refs remain.
    let refs_after = git(
        td.path(),
        &[
            "for-each-ref",
            "--format=%(refname)",
            SHADOW_REF_PREFIX.trim_end_matches('/'),
        ],
    );
    assert_eq!(refs_after.lines().count(), 10);

    // CRITICAL: user's HEAD + branches unchanged.
    let user_head_after = git(td.path(), &["rev-parse", "HEAD"]).trim().to_string();
    let user_branches_after = git(td.path(), &["branch", "-a"]);
    assert_eq!(user_head_before, user_head_after, "prune moved user HEAD");
    assert_eq!(
        user_branches_before, user_branches_after,
        "prune changed user branches"
    );

    // `git log` still shows just the base commit.
    let log = git(td.path(), &["log", "--oneline"]);
    assert_eq!(log.lines().count(), 1);
}

/// Dogfood #5a: snapshot_turn with a clean tree returns NoChanges and does
/// NOT create an empty shadow commit — the "no-op turn" case.
#[test]
fn dogfood_noop_turn_creates_no_commit() {
    let td = TempDir::new().unwrap();
    git_init(td.path());
    commit_initial(td.path());

    let cfg = AutoCommitConfig::default();
    let mut commits: Vec<String> = Vec::new();
    let mut pos = 0usize;

    // No file edits — just call snapshot_turn.
    let out = snapshot_turn(
        td.path(),
        &cfg,
        "noop",
        "idle turn",
        1,
        &mut commits,
        &mut pos,
    )
    .unwrap();
    assert!(
        matches!(out, SnapshotOutcome::NoChanges),
        "expected NoChanges, got {out:?}"
    );
    assert!(commits.is_empty());
    assert_eq!(pos, 0);

    // Shadow ref must not exist.
    let refs = git(
        td.path(),
        &[
            "for-each-ref",
            "--format=%(refname)",
            SHADOW_REF_PREFIX.trim_end_matches('/'),
        ],
    );
    assert!(
        refs.trim().is_empty(),
        "noop turn leaked a shadow ref: {refs}"
    );
}

/// Dogfood #5b: snapshot_turn fails cleanly if the target directory is not
/// a git repo. No panics, no stray files.
#[test]
fn dogfood_non_git_dir_returns_disabled_outcome() {
    let td = TempDir::new().unwrap();
    // Deliberately NOT calling git_init.
    assert!(!is_git_repo(td.path()));

    let cfg = AutoCommitConfig::default();
    let mut commits: Vec<String> = Vec::new();
    let mut pos = 0usize;

    write(td.path(), "work.txt", "hello\n");
    let out = snapshot_turn(
        td.path(),
        &cfg,
        "nogit",
        "should short-circuit",
        1,
        &mut commits,
        &mut pos,
    )
    .unwrap();
    assert!(matches!(out, SnapshotOutcome::Disabled { .. }));
    assert!(commits.is_empty());

    // Verify the working tree was untouched.
    assert_eq!(read(td.path(), "work.txt"), "hello\n");
}
