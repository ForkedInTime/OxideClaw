//! Auto-commit loop — per-turn working-tree snapshots on private shadow refs.
//!
//! Uses git plumbing (`write-tree`, `commit-tree`, `update-ref`, `read-tree`,
//! `checkout-index`) under `refs/rustyclaw/sessions/<session-id>`, driven via
//! `std::process::Command` with `GIT_INDEX_FILE` pointed at a temp index so the
//! user's real `.git/index` is never touched.
//!
//! Commits are invisible to normal git tooling (`log`, `status`, `branch`) —
//! only `git for-each-ref refs/rustyclaw/` sees them. They are strictly local
//! (never pushed) and serve as a per-turn undo stack the user can navigate
//! with `/undo` and `/redo`.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub use crate::settings::{AutoCommitConfig, DEFAULT_KEEP_SESSIONS, DEFAULT_MESSAGE_PREFIX};

pub const SHADOW_REF_PREFIX: &str = "refs/rustyclaw/sessions/";

/// Outcome of a single `snapshot_turn` call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SnapshotOutcome {
    /// A new commit was written and `auto_commits` updated.
    Committed { sha: String, files: u32 },
    /// Working tree matches parent tree — nothing to commit.
    NoChanges,
    /// Auto-commit is disabled (config, non-git dir, etc). Human-readable reason.
    Disabled { reason: String },
    /// Another writer moved the session's shadow ref while this turn was being
    /// snapshotted. The turn was NOT recorded, but nothing was destroyed — the
    /// working tree is untouched and the other instance's history is intact.
    Conflict { reason: String },
}

/// Report returned by `restore_to`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoreReport {
    /// Number of files in the target tree (i.e., the count of files that
    /// *should* exist in cwd after the restore). NOT the count of files
    /// that actually changed on disk — files that were already at the
    /// target content are included in this count.
    pub files_restored: u32,
    /// Files in the working tree that do NOT exist in the target tree. They
    /// are left untouched; the caller may mention them in a system message.
    pub orphaned_files: Vec<PathBuf>,
}

// ── Git subprocess helpers ────────────────────────────────────────────────────

/// Build a `std::process::Command` for `git` rooted at `cwd` with a clean,
/// locale-agnostic environment and sourcing no user git config that might
/// break plumbing output parsing.
pub(crate) fn git_cmd(cwd: &Path) -> std::process::Command {
    let mut cmd = std::process::Command::new("git");
    cmd.current_dir(cwd);
    // Deterministic output: no localised messages, no terminal prompting,
    // no optional-locks (git status would take a repo lock otherwise).
    cmd.env("LC_ALL", "C");
    cmd.env("GIT_TERMINAL_PROMPT", "0");
    cmd.env("GIT_OPTIONAL_LOCKS", "0");
    cmd
}

/// Return true if `cwd` is inside a git work tree. Uses
/// `git rev-parse --is-inside-work-tree`, which walks parent dirs.
pub fn is_git_repo(cwd: &Path) -> bool {
    git_cmd(cwd)
        .args(["rev-parse", "--is-inside-work-tree"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .map(|o| o.status.success() && o.stdout.trim_ascii() == b"true")
        .unwrap_or(false)
}

// ── Snapshot pipeline ─────────────────────────────────────────────────────────

fn shadow_ref(session_id: &str) -> String {
    format!("{SHADOW_REF_PREFIX}{session_id}")
}

/// Run a git command, capturing stdout; return the trimmed stdout as a String
/// on success or an error carrying stderr on failure.
fn git_output(cmd: &mut Command) -> anyhow::Result<String> {
    let out = cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).output()?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!("git command failed: {stderr}");
    }
    let s = String::from_utf8(out.stdout)?;
    Ok(s.trim().to_string())
}

/// Resolve HEAD to a SHA, returning None if HEAD is unborn (no commits yet).
fn resolve_head(cwd: &Path) -> Option<String> {
    let out = git_cmd(cwd)
        .args(["rev-parse", "--verify", "HEAD"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?.trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}

/// Look up the tree SHA of a given commit SHA. Returns `None` if the lookup
/// fails, so the call site can distinguish "not found" from a valid tree SHA.
fn tree_of_commit(cwd: &Path, commit: &str) -> Option<String> {
    git_cmd(cwd)
        .args(["rev-parse", &format!("{commit}^{{tree}}")])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
}

/// Count the number of file entries in a tree via `git ls-tree -r --name-only <tree>`.
fn count_tree_files(cwd: &Path, tree: &str) -> u32 {
    let out = git_cmd(cwd)
        .args(["ls-tree", "-r", "--name-only", tree])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output();
    match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
            .lines()
            .filter(|l| !l.is_empty())
            .count() as u32,
        _ => 0,
    }
}

/// Trim a user prompt to a 60-char single-line subject fragment.
fn subject_from_prompt(prompt: &str) -> String {
    let first_line = prompt.lines().next().unwrap_or("").trim();
    if first_line.chars().count() <= 60 {
        first_line.to_string()
    } else {
        let truncated: String = first_line.chars().take(57).collect();
        format!("{truncated}...")
    }
}

/// Take a full-tree snapshot of `cwd` as a commit on the session's shadow ref.
///
/// **Concurrency.** `parent` comes from this process's in-memory `auto_commits`,
/// so two instances sharing a `session_id` (a second pane, a resumed session)
/// each build their own chain and both write the same ref — the later
/// `update-ref` silently orphans the other's history, which is exactly the
/// history `/undo` exists to reach.
///
/// The ref value is therefore read before the snapshot is built and passed to
/// `update-ref` as an expected-old value, making the write a compare-and-swap.
/// Git enforces it atomically under its own ref lock, across processes and
/// without a lockfile of ours to leak. A concurrent write now fails loudly
/// ([`SnapshotOutcome::Conflict`]) instead of destroying data quietly.
pub fn snapshot_turn(
    cwd: &Path,
    config: &AutoCommitConfig,
    session_id: &str,
    user_prompt: &str,
    turn_index: u32,
    auto_commits: &mut Vec<String>,
    undo_position: &mut usize,
) -> anyhow::Result<SnapshotOutcome> {
    if !config.enabled {
        return Ok(SnapshotOutcome::Disabled {
            reason: "auto-commit disabled in settings".to_string(),
        });
    }
    if !is_git_repo(cwd) {
        return Ok(SnapshotOutcome::Disabled {
            reason: "not a git repo".to_string(),
        });
    }

    // 0. Work out where *this process* believes the ref should be, so the final
    //    update-ref can compare-and-swap against it.
    //
    //    Reading the ref here instead would be useless: a competing instance
    //    that wrote between our turns has already landed, and we would happily
    //    CAS against its value and clobber it. The meaningful expectation is our
    //    own chain head — anything else means someone moved the ref since we
    //    last wrote.
    //
    //    The expectation is `auto_commits.last()`, NOT the undo position:
    //    `restore_to` rewrites the working tree but deliberately leaves the ref
    //    alone, so after an /undo the ref still points at the newest commit we
    //    wrote while `undo_position` has moved back. Using the undo position
    //    here manufactures a conflict on the first turn after any undo.
    //
    //    An empty expectation (no commits yet) means the ref must not exist,
    //    which is exactly right for a fresh session.
    let ref_name = shadow_ref(session_id);
    let expected_ref = auto_commits.last().cloned();

    // 1. Temp index file, isolated via GIT_INDEX_FILE.
    let td = tempfile::TempDir::new()?;
    let temp_index = td.path().join("turn.index");

    // 2. Seed the temp index from the parent's tree (if we have one) so `add -A`
    //    only records diffs relative to the parent, which makes empty-turn
    //    detection accurate even when the user's real index has other staging.
    let parent = if *undo_position > 0 {
        auto_commits.get(*undo_position - 1).cloned()
    } else {
        resolve_head(cwd)
    };
    if let Some(p) = &parent
        && let Some(tree) = tree_of_commit(cwd, p)
    {
        let s = git_cmd(cwd)
            .env("GIT_INDEX_FILE", &temp_index)
            .args(["read-tree", &tree])
            .status()?;
        if !s.success() {
            anyhow::bail!("git read-tree {tree} failed");
        }
    }

    // 3. Stage everything under cwd into the temp index.
    let add_status = git_cmd(cwd)
        .env("GIT_INDEX_FILE", &temp_index)
        .args(["add", "-A"])
        .status()?;
    if !add_status.success() {
        anyhow::bail!("git add -A failed");
    }

    // 4. Write the tree.
    let tree_sha = git_output(
        git_cmd(cwd)
            .env("GIT_INDEX_FILE", &temp_index)
            .args(["write-tree"]),
    )?;

    // 5. Empty-turn optimization: compare against parent tree.
    if let Some(p) = &parent
        && let Some(parent_tree) = tree_of_commit(cwd, p)
        && parent_tree == tree_sha
    {
        return Ok(SnapshotOutcome::NoChanges);
    }

    // 6. Build the commit.
    let subject = format!(
        "{} turn {}: {}",
        config.message_prefix,
        turn_index,
        subject_from_prompt(user_prompt),
    );
    let files = count_tree_files(cwd, &tree_sha);
    let body = format!(
        "\nRustyClaw-Session: {session_id}\nRustyClaw-Turn: {turn_index}\nRustyClaw-Files: {files}\n"
    );
    let full_msg = format!("{subject}\n{body}");

    let mut commit_cmd = git_cmd(cwd);
    commit_cmd
        .env("GIT_AUTHOR_NAME", "rustyclaw")
        .env("GIT_AUTHOR_EMAIL", "noreply@rustyclaw.local")
        .env("GIT_COMMITTER_NAME", "rustyclaw")
        .env("GIT_COMMITTER_EMAIL", "noreply@rustyclaw.local")
        .args(["commit-tree", &tree_sha, "-m", &full_msg]);
    if let Some(p) = &parent {
        commit_cmd.args(["-p", p]);
    }
    let commit_sha = git_output(&mut commit_cmd)?;

    // 7. Update the shadow ref, compare-and-swap against the value we started
    //    from. An empty expected-old tells git the ref must not exist yet.
    let expected_old = expected_ref.as_deref().unwrap_or("");
    let update_status = git_cmd(cwd)
        .args(["update-ref", &ref_name, &commit_sha, expected_old])
        .status()?;
    if !update_status.success() {
        // The commit object is already written and reachable by sha, so nothing
        // the user did is lost — we simply refuse to move the ref over someone
        // else's work.
        return Ok(SnapshotOutcome::Conflict {
            reason: format!(
                "another rustyclaw instance wrote to this session's history \
                 while this turn was being snapshotted (session '{session_id}'). \
                 This turn was not recorded; the working tree is untouched. \
                 Use a distinct session per instance — /undo history is per-session."
            ),
        });
    }

    // 8. Discard redo tail if user was in an undone state, then append.
    if *undo_position < auto_commits.len() {
        auto_commits.truncate(*undo_position);
    }
    auto_commits.push(commit_sha.clone());
    *undo_position = auto_commits.len();

    Ok(SnapshotOutcome::Committed {
        sha: commit_sha,
        files,
    })
}

// ── Restore pipeline ──────────────────────────────────────────────────────────

fn list_tree_files(cwd: &Path, tree: &str) -> Vec<String> {
    git_cmd(cwd)
        .args(["ls-tree", "-r", "--name-only", tree])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| {
            s.lines()
                .filter(|l| !l.is_empty())
                .map(|l| l.to_string())
                .collect()
        })
        .unwrap_or_default()
}

/// Lists files in the user's real git index via `git ls-files`. Only used
/// as a fallback in `restore_to` when there are zero shadow commits yet
/// (can't query a snapshot tree that doesn't exist).
fn list_tracked_files(cwd: &Path) -> Vec<String> {
    git_cmd(cwd)
        .args(["ls-files"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| {
            s.lines()
                .filter(|l| !l.is_empty())
                .map(|l| l.to_string())
                .collect()
        })
        .unwrap_or_default()
}

pub fn restore_to(
    cwd: &Path,
    auto_commits: &[String],
    target_position: usize,
) -> anyhow::Result<RestoreReport> {
    if !is_git_repo(cwd) {
        anyhow::bail!("not a git repo");
    }
    if target_position > auto_commits.len() {
        anyhow::bail!(
            "target_position {target_position} out of range (max {})",
            auto_commits.len()
        );
    }

    // Resolve target tree.
    let tree_sha = if target_position == 0 {
        if let Some(first) = auto_commits.first() {
            // Session base = tree of first's parent. If first is a root commit
            // (no parent), fall through to HEAD tree, else canonical empty tree.
            let parent_commit = git_cmd(cwd)
                .args(["rev-parse", &format!("{first}^")])
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .output()
                .ok()
                .filter(|o| o.status.success())
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .map(|s| s.trim().to_string())
                .unwrap_or_default();
            if !parent_commit.is_empty() {
                tree_of_commit(cwd, &parent_commit).unwrap_or_default()
            } else if let Some(head) = resolve_head(cwd) {
                tree_of_commit(cwd, &head).unwrap_or_default()
            } else {
                "4b825dc642cb6eb9a060e54bf8d69288fbee4904".to_string()
            }
        } else if let Some(head) = resolve_head(cwd) {
            tree_of_commit(cwd, &head).unwrap_or_default()
        } else {
            "4b825dc642cb6eb9a060e54bf8d69288fbee4904".to_string()
        }
    } else {
        let commit = &auto_commits[target_position - 1];
        tree_of_commit(cwd, commit).unwrap_or_default()
    };

    if tree_sha.is_empty() {
        anyhow::bail!("could not resolve target tree");
    }

    let target_files: std::collections::HashSet<String> =
        list_tree_files(cwd, &tree_sha).into_iter().collect();
    // Determine "current" files from the latest auto-commit tree (not the real
    // git index, which may lag behind the snapshot chain).
    let current_files: Vec<String> = if let Some(latest) = auto_commits.last() {
        if let Some(latest_tree) = tree_of_commit(cwd, latest) {
            list_tree_files(cwd, &latest_tree)
        } else {
            list_tracked_files(cwd)
        }
    } else {
        list_tracked_files(cwd)
    };
    let orphaned_files: Vec<PathBuf> = current_files
        .iter()
        .filter(|f| !target_files.contains(*f))
        .map(PathBuf::from)
        .collect();

    let td = tempfile::TempDir::new()?;
    let temp_index = td.path().join("restore.index");
    let read_status = git_cmd(cwd)
        .env("GIT_INDEX_FILE", &temp_index)
        .args(["read-tree", &tree_sha])
        .status()?;
    if !read_status.success() {
        anyhow::bail!("git read-tree {tree_sha} failed");
    }

    let prefix = format!("{}/", cwd.display());
    let checkout_status = git_cmd(cwd)
        .env("GIT_INDEX_FILE", &temp_index)
        .args(["checkout-index", "-a", "-f", "--prefix", &prefix])
        .status()?;
    if !checkout_status.success() {
        anyhow::bail!("git checkout-index --prefix={prefix} failed");
    }

    Ok(RestoreReport {
        files_restored: target_files.len() as u32,
        orphaned_files,
    })
}

// ── Prune pipeline ────────────────────────────────────────────────────────────

/// Delete old `refs/rustyclaw/sessions/*` refs, keeping the `keep` newest by
/// committer date. `keep == 0` disables pruning. Non-fatal: any error is
/// logged via `tracing::warn!` and the function returns 0.
/// Choose which shadow refs to delete: keep the `keep` newest, delete the rest.
///
/// Extracted so the *direction* is testable. An inversion here deletes the
/// newest sessions instead of the oldest — silent, unrecoverable data loss —
/// and the integration test cannot catch it, because `%(committerdate:unix)`
/// has one-second granularity and sessions created in a loop all tie.
///
/// `%(committerdate:unix)` has one-second granularity, so sessions created in
/// quick succession tie. Ties previously fell through to git's output order
/// (refname-alphabetical), which is unrelated to recency — so prune could keep
/// an older session and delete a newer one purely because of how the ref was
/// named.
///
/// Ties are now broken by refname *descending*. Shadow ref names embed the
/// session id, and session ids are monotonic within a run, so this is a strictly
/// better proxy for recency than ascending-alphabetical and is at minimum
/// deterministic. It is a heuristic, not a guarantee: two sessions genuinely
/// created in the same second with unordered ids are still arbitrary — but the
/// arbitrariness is now stable rather than accidental.
fn select_refs_to_delete(mut rows: Vec<(i64, String)>, keep: usize) -> Vec<String> {
    rows.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
    rows.into_iter().skip(keep).map(|(_, r)| r).collect()
}

pub fn prune_old_refs(cwd: &Path, keep: u32) -> anyhow::Result<u32> {
    if keep == 0 || !is_git_repo(cwd) {
        return Ok(0);
    }

    let out = git_cmd(cwd)
        .args([
            "for-each-ref",
            "--format=%(committerdate:unix) %(refname)",
            SHADOW_REF_PREFIX.trim_end_matches('/'),
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()?;
    if !out.status.success() {
        return Ok(0);
    }
    let s = String::from_utf8(out.stdout)?;
    let rows: Vec<(i64, String)> = s
        .lines()
        .filter_map(|line| {
            let mut it = line.splitn(2, ' ');
            let ts = it.next()?.parse::<i64>().ok()?;
            let refname = it.next()?.to_string();
            Some((ts, refname))
        })
        .collect();

    if rows.len() <= keep as usize {
        return Ok(0);
    }

    let to_delete = select_refs_to_delete(rows, keep as usize);

    let mut deleted = 0u32;
    for r in &to_delete {
        let status = git_cmd(cwd).args(["update-ref", "-d", r]).status();
        match status {
            Ok(s) if s.success() => deleted += 1,
            Ok(_) => tracing::warn!("autoCommit prune: failed to delete {r}"),
            Err(e) => tracing::warn!("autoCommit prune: error deleting {r}: {e}"),
        }
    }
    Ok(deleted)
}

/// Variant of `snapshot_turn` that takes the message-prefix as a plain string
/// instead of `&AutoCommitConfig`, avoiding cross-crate type-identity issues
/// when the bin crate's TUI event loop calls into the lib.  The caller is
/// responsible for checking `enabled` and `keep_sessions` before dispatching.
pub fn snapshot_turn_raw(
    cwd: &Path,
    message_prefix: &str,
    session_id: &str,
    user_prompt: &str,
    turn_index: u32,
    auto_commits: &mut Vec<String>,
    undo_position: &mut usize,
) -> anyhow::Result<SnapshotOutcome> {
    let config = AutoCommitConfig {
        enabled: true,
        keep_sessions: DEFAULT_KEEP_SESSIONS,
        message_prefix: message_prefix.to_string(),
    };
    snapshot_turn(
        cwd,
        &config,
        session_id,
        user_prompt,
        turn_index,
        auto_commits,
        undo_position,
    )
}

#[cfg(test)]
mod prune_selection_tests {
    use super::select_refs_to_delete;

    fn rows(pairs: &[(i64, &str)]) -> Vec<(i64, String)> {
        pairs.iter().map(|(t, r)| (*t, r.to_string())).collect()
    }

    /// The direction is the whole point: keep the NEWEST, delete the oldest.
    /// Inverting this deletes the sessions the user most likely wants to undo
    /// to — silent, unrecoverable loss. The integration test cannot catch an
    /// inversion because its timestamps all tie at one-second granularity.
    #[test]
    fn keeps_the_newest_and_deletes_the_oldest() {
        let deleted = select_refs_to_delete(
            rows(&[(100, "old"), (300, "newest"), (200, "mid"), (50, "oldest")]),
            2,
        );
        assert_eq!(deleted, vec!["old".to_string(), "oldest".to_string()]);
    }

    #[test]
    fn keeping_more_than_present_deletes_nothing() {
        assert!(select_refs_to_delete(rows(&[(1, "a"), (2, "b")]), 10).is_empty());
    }

    #[test]
    fn keep_zero_deletes_everything() {
        let deleted = select_refs_to_delete(rows(&[(1, "a"), (2, "b")]), 0);
        assert_eq!(deleted.len(), 2);
    }

    /// With one-second timestamp granularity ties are the norm, not the
    /// exception. They must resolve deterministically rather than by git's
    /// output order, which is unrelated to recency.
    #[test]
    fn tied_timestamps_break_deterministically_by_refname_desc() {
        // Input order deliberately shuffled — the result must not depend on it.
        let a = select_refs_to_delete(rows(&[(7, "a"), (7, "c"), (7, "b")]), 1);
        let b = select_refs_to_delete(rows(&[(7, "c"), (7, "b"), (7, "a")]), 1);
        assert_eq!(a, b, "tie-break must be independent of input order");
        assert_eq!(a, vec!["b".to_string(), "a".to_string()]);
    }

    /// Timestamp still dominates — the tie-break only applies within a second.
    #[test]
    fn timestamp_beats_refname() {
        let deleted = select_refs_to_delete(rows(&[(1, "zzz"), (9, "aaa")]), 1);
        assert_eq!(deleted, vec!["zzz".to_string()], "older loses regardless of name");
    }
}

#[cfg(test)]
mod git_detection_tests {
    use super::*;
    use std::process::Command;
    use tempfile::TempDir;

    /// Helper: create a tempdir and `git init` inside it. Returns the tempdir handle.
    pub(super) fn init_test_repo() -> TempDir {
        let td = TempDir::new().unwrap();
        let status = Command::new("git")
            .args(["init", "--quiet", "--initial-branch=main"])
            .current_dir(td.path())
            .status()
            .expect("git init");
        assert!(status.success(), "git init failed");
        // Set required git config so later commits work.
        // core.autocrlf=false prevents Windows git from rewriting "x\n" to
        // "x\r\n" on checkout, which breaks byte-exact fixture assertions.
        for (k, v) in [
            ("user.name", "rustyclaw-test"),
            ("user.email", "noreply@rustyclaw.local"),
            ("commit.gpgsign", "false"),
            ("core.autocrlf", "false"),
            ("core.safecrlf", "false"),
        ] {
            let s = Command::new("git")
                .args(["config", k, v])
                .current_dir(td.path())
                .status()
                .expect("git config");
            assert!(s.success());
        }
        td
    }

    #[test]
    fn detects_fresh_git_repo() {
        let td = init_test_repo();
        assert!(is_git_repo(td.path()));
    }

    #[test]
    fn detects_subdirectory_of_git_repo() {
        let td = init_test_repo();
        let sub = td.path().join("pkg").join("nested");
        std::fs::create_dir_all(&sub).unwrap();
        assert!(is_git_repo(&sub));
    }

    #[test]
    fn rejects_non_git_dir() {
        let td = TempDir::new().unwrap();
        assert!(!is_git_repo(td.path()));
    }
}

#[cfg(test)]
mod snapshot_tests {
    use super::git_detection_tests::init_test_repo;
    use super::*;
    use std::fs;

    pub(super) fn write_file(repo: &Path, rel: &str, body: &str) {
        let p = repo.join(rel);
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(p, body).unwrap();
    }

    fn initial_commit(repo: &Path) {
        write_file(repo, "README.md", "base\n");
        let s = git_cmd(repo).args(["add", "README.md"]).status().unwrap();
        assert!(s.success());
        let s = git_cmd(repo)
            .args(["commit", "-q", "-m", "base"])
            .status()
            .unwrap();
        assert!(s.success());
    }

    #[test]
    fn snapshot_creates_commit_with_head_parent() {
        let td = init_test_repo();
        initial_commit(td.path());
        write_file(td.path(), "src/lib.rs", "fn main() {}\n");

        let cfg = AutoCommitConfig::default();
        let mut commits = Vec::new();
        let mut pos = 0usize;
        let outcome = snapshot_turn(
            td.path(),
            &cfg,
            "test-session-1",
            "add src/lib.rs",
            1,
            &mut commits,
            &mut pos,
        )
        .unwrap();

        match outcome {
            SnapshotOutcome::Committed { sha, files } => {
                assert_eq!(sha.len(), 40, "sha should be full 40-char hex");
                assert_eq!(
                    files, 2,
                    "README.md + src/lib.rs should both be in the tree"
                );
                assert_eq!(commits, vec![sha]);
                assert_eq!(pos, 1);
            }
            other => panic!("expected Committed, got {other:?}"),
        }

        // Shadow ref points at the new commit.
        let out = git_cmd(td.path())
            .args(["rev-parse", "refs/rustyclaw/sessions/test-session-1"])
            .output()
            .unwrap();
        assert!(out.status.success());
        let ref_sha = String::from_utf8(out.stdout).unwrap().trim().to_string();
        assert_eq!(ref_sha, commits[0]);
    }

    #[test]
    fn snapshot_chains_parents_across_turns() {
        let td = init_test_repo();
        initial_commit(td.path());

        let cfg = AutoCommitConfig::default();
        let mut commits = Vec::new();
        let mut pos = 0usize;

        write_file(td.path(), "a.txt", "1\n");
        snapshot_turn(td.path(), &cfg, "s1", "add a", 1, &mut commits, &mut pos).unwrap();

        write_file(td.path(), "b.txt", "2\n");
        snapshot_turn(td.path(), &cfg, "s1", "add b", 2, &mut commits, &mut pos).unwrap();

        write_file(td.path(), "c.txt", "3\n");
        snapshot_turn(td.path(), &cfg, "s1", "add c", 3, &mut commits, &mut pos).unwrap();

        assert_eq!(commits.len(), 3);
        assert_eq!(pos, 3);
        // Each commit's parent is the previous.
        for (i, sha) in commits.iter().enumerate().skip(1) {
            let out = git_cmd(td.path())
                .args(["rev-parse", &format!("{sha}^")])
                .output()
                .unwrap();
            let parent = String::from_utf8(out.stdout).unwrap().trim().to_string();
            assert_eq!(parent, commits[i - 1], "chain broken at index {i}");
        }
    }

    #[test]
    fn snapshot_no_changes_is_noop() {
        let td = init_test_repo();
        initial_commit(td.path());
        let cfg = AutoCommitConfig::default();
        let mut commits = Vec::new();
        let mut pos = 0usize;

        let outcome = snapshot_turn(
            td.path(),
            &cfg,
            "s-empty",
            "pure read turn",
            1,
            &mut commits,
            &mut pos,
        )
        .unwrap();
        assert_eq!(outcome, SnapshotOutcome::NoChanges);
        assert!(commits.is_empty());
        assert_eq!(pos, 0);
    }

    #[test]
    fn snapshot_disabled_when_config_disabled() {
        let td = init_test_repo();
        initial_commit(td.path());
        let cfg = AutoCommitConfig {
            enabled: false,
            ..AutoCommitConfig::default()
        };
        let mut commits = Vec::new();
        let mut pos = 0usize;
        let outcome =
            snapshot_turn(td.path(), &cfg, "s", "msg", 1, &mut commits, &mut pos).unwrap();
        match outcome {
            SnapshotOutcome::Disabled { reason } => {
                assert!(reason.contains("disabled"), "reason: {reason}");
            }
            other => panic!("expected Disabled, got {other:?}"),
        }
        assert!(commits.is_empty());
    }

    #[test]
    fn snapshot_disabled_when_not_git_repo() {
        let td = tempfile::TempDir::new().unwrap();
        let cfg = AutoCommitConfig::default();
        let mut commits = Vec::new();
        let mut pos = 0usize;
        let outcome =
            snapshot_turn(td.path(), &cfg, "s", "msg", 1, &mut commits, &mut pos).unwrap();
        assert!(matches!(outcome, SnapshotOutcome::Disabled { .. }));
    }

    #[test]
    fn snapshot_respects_gitignore() {
        let td = init_test_repo();
        initial_commit(td.path());
        write_file(td.path(), ".gitignore", "secret.txt\n");
        write_file(td.path(), "secret.txt", "password\n");
        write_file(td.path(), "ok.txt", "safe\n");

        let cfg = AutoCommitConfig::default();
        let mut commits = Vec::new();
        let mut pos = 0usize;
        snapshot_turn(td.path(), &cfg, "s", "ignored", 1, &mut commits, &mut pos).unwrap();
        assert_eq!(commits.len(), 1);

        // ls-tree the commit and confirm secret.txt is NOT present.
        let out = git_cmd(td.path())
            .args(["ls-tree", "-r", "--name-only", &commits[0]])
            .output()
            .unwrap();
        let tree = String::from_utf8(out.stdout).unwrap();
        assert!(
            !tree.contains("secret.txt"),
            "secret.txt leaked into tree:\n{tree}"
        );
        assert!(tree.contains("ok.txt"));
    }

    #[test]
    fn snapshot_works_on_unborn_head() {
        // Fresh repo — no commits, HEAD is unborn.
        let td = init_test_repo();
        write_file(td.path(), "a.txt", "hello\n");

        let cfg = AutoCommitConfig::default();
        let mut commits = Vec::new();
        let mut pos = 0usize;
        let outcome = snapshot_turn(
            td.path(),
            &cfg,
            "s-new",
            "first turn",
            1,
            &mut commits,
            &mut pos,
        )
        .unwrap();

        let sha = match outcome {
            SnapshotOutcome::Committed { sha, files } => {
                assert_eq!(files, 1, "only a.txt should be in the tree");
                sha
            }
            other => panic!("expected Committed, got {other:?}"),
        };

        // The commit must have no parent line.
        let out = git_cmd(td.path())
            .args(["cat-file", "-p", &sha])
            .output()
            .unwrap();
        let info = String::from_utf8(out.stdout).unwrap();
        assert!(
            !info.contains("parent "),
            "unborn-HEAD commit should have no parent, got:\n{info}"
        );
    }

    #[test]
    fn snapshot_discards_redo_tail_on_new_work() {
        let td = init_test_repo();
        initial_commit(td.path());
        let cfg = AutoCommitConfig::default();
        let mut commits = Vec::new();
        let mut pos = 0usize;

        write_file(td.path(), "a.txt", "1\n");
        snapshot_turn(td.path(), &cfg, "s", "a", 1, &mut commits, &mut pos).unwrap();
        write_file(td.path(), "b.txt", "2\n");
        snapshot_turn(td.path(), &cfg, "s", "b", 2, &mut commits, &mut pos).unwrap();
        write_file(td.path(), "c.txt", "3\n");
        snapshot_turn(td.path(), &cfg, "s", "c", 3, &mut commits, &mut pos).unwrap();
        assert_eq!(commits.len(), 3);
        assert_eq!(pos, 3);

        // Simulate /undo 2 → pos = 1, then new work — expect redo tail discarded.
        pos = 1;
        write_file(td.path(), "d.txt", "4\n");
        snapshot_turn(td.path(), &cfg, "s", "d", 2, &mut commits, &mut pos).unwrap();
        assert_eq!(
            commits.len(),
            2,
            "turns 2 and 3 should be dropped, new turn 2 appended"
        );
        assert_eq!(pos, 2);
    }
}

#[cfg(test)]
mod restore_tests {
    use super::git_detection_tests::init_test_repo;
    use super::snapshot_tests::write_file;
    use super::*;

    #[test]
    fn restore_overwrites_modified_files() {
        let td = init_test_repo();
        write_file(td.path(), "app.txt", "v1\n");
        git_cmd(td.path())
            .args(["add", "app.txt"])
            .status()
            .unwrap();
        git_cmd(td.path())
            .args(["commit", "-q", "-m", "v1"])
            .status()
            .unwrap();

        let cfg = AutoCommitConfig::default();
        let mut commits = Vec::new();
        let mut pos = 0usize;

        write_file(td.path(), "app.txt", "v2\n");
        snapshot_turn(td.path(), &cfg, "s", "v2", 1, &mut commits, &mut pos).unwrap();
        write_file(td.path(), "app.txt", "v3\n");
        snapshot_turn(td.path(), &cfg, "s", "v3", 2, &mut commits, &mut pos).unwrap();

        let report = restore_to(td.path(), &commits, 1).unwrap();
        assert!(report.files_restored >= 1);
        let contents = std::fs::read_to_string(td.path().join("app.txt")).unwrap();
        assert_eq!(contents, "v2\n");

        let _ = restore_to(td.path(), &commits, 2).unwrap();
        let contents = std::fs::read_to_string(td.path().join("app.txt")).unwrap();
        assert_eq!(contents, "v3\n");
    }

    #[test]
    fn restore_leaves_untracked_files_alone() {
        let td = init_test_repo();
        write_file(td.path(), "tracked.txt", "x\n");
        git_cmd(td.path())
            .args(["add", "tracked.txt"])
            .status()
            .unwrap();
        git_cmd(td.path())
            .args(["commit", "-q", "-m", "base"])
            .status()
            .unwrap();

        let cfg = AutoCommitConfig::default();
        let mut commits = Vec::new();
        let mut pos = 0usize;

        write_file(td.path(), "tracked.txt", "y\n");
        snapshot_turn(td.path(), &cfg, "s", "mod", 1, &mut commits, &mut pos).unwrap();

        write_file(td.path(), "untracked.log", "scratch\n");

        restore_to(td.path(), &commits, 0).unwrap();
        assert_eq!(
            std::fs::read_to_string(td.path().join("tracked.txt")).unwrap(),
            "x\n"
        );
        assert!(
            td.path().join("untracked.log").exists(),
            "untracked file was deleted!"
        );
    }

    #[test]
    fn restore_to_session_base_zero_position() {
        let td = init_test_repo();
        write_file(td.path(), "app.txt", "base\n");
        git_cmd(td.path())
            .args(["add", "app.txt"])
            .status()
            .unwrap();
        git_cmd(td.path())
            .args(["commit", "-q", "-m", "base"])
            .status()
            .unwrap();

        let cfg = AutoCommitConfig::default();
        let mut commits = Vec::new();
        let mut pos = 0usize;

        write_file(td.path(), "app.txt", "modified\n");
        snapshot_turn(td.path(), &cfg, "s", "m", 1, &mut commits, &mut pos).unwrap();

        restore_to(td.path(), &commits, 0).unwrap();
        assert_eq!(
            std::fs::read_to_string(td.path().join("app.txt")).unwrap(),
            "base\n"
        );
    }

    #[test]
    fn restore_reports_orphaned_files() {
        let td = init_test_repo();
        write_file(td.path(), "old.txt", "kept\n");
        git_cmd(td.path())
            .args(["add", "old.txt"])
            .status()
            .unwrap();
        git_cmd(td.path())
            .args(["commit", "-q", "-m", "base"])
            .status()
            .unwrap();

        let cfg = AutoCommitConfig::default();
        let mut commits = Vec::new();
        let mut pos = 0usize;

        write_file(td.path(), "new.txt", "added\n");
        snapshot_turn(td.path(), &cfg, "s", "add new", 1, &mut commits, &mut pos).unwrap();

        let report = restore_to(td.path(), &commits, 0).unwrap();
        assert!(
            report.orphaned_files.iter().any(|p| p.ends_with("new.txt")),
            "expected new.txt to be flagged as orphaned: {:?}",
            report.orphaned_files
        );
    }

    #[test]
    fn restore_to_session_base_with_unborn_head_first_commit() {
        // First auto-commit is a root commit (no parent — fresh repo, unborn HEAD).
        // Restoring to position 0 should land on the empty tree, removing the
        // file that was added in turn 1.
        let td = init_test_repo();
        let cfg = AutoCommitConfig::default();
        let mut commits = Vec::new();
        let mut pos = 0usize;

        write_file(td.path(), "only.txt", "solo\n");
        snapshot_turn(td.path(), &cfg, "s", "first", 1, &mut commits, &mut pos).unwrap();
        assert_eq!(commits.len(), 1);

        // Verify commits[0] has no parent (root commit).
        let out = git_cmd(td.path())
            .args(["cat-file", "-p", &commits[0]])
            .output()
            .unwrap();
        let body = String::from_utf8(out.stdout).unwrap();
        assert!(
            !body.contains("\nparent "),
            "expected root commit, got:\n{body}"
        );

        // Restore to session base → empty tree.
        let report = restore_to(td.path(), &commits, 0).unwrap();
        // only.txt was in turn 1 but not in empty target tree → orphaned.
        assert!(
            report
                .orphaned_files
                .iter()
                .any(|p| p.ends_with("only.txt")),
            "expected only.txt as orphan: {:?}",
            report.orphaned_files
        );
    }
}

#[cfg(test)]
mod prune_tests {
    use super::git_detection_tests::init_test_repo;
    use super::*;

    /// Create a single empty commit so we can point shadow refs at it.
    fn make_empty_commit(cwd: &Path, subject: &str) -> String {
        let td = tempfile::TempDir::new().unwrap();
        let idx = td.path().join("idx");
        let tree = git_cmd(cwd)
            .env("GIT_INDEX_FILE", &idx)
            .args(["write-tree"])
            .output()
            .unwrap();
        assert!(tree.status.success());
        let tree_sha = String::from_utf8(tree.stdout).unwrap().trim().to_string();
        let out = git_cmd(cwd)
            .env("GIT_AUTHOR_NAME", "rustyclaw")
            .env("GIT_AUTHOR_EMAIL", "noreply@rustyclaw.local")
            .env("GIT_COMMITTER_NAME", "rustyclaw")
            .env("GIT_COMMITTER_EMAIL", "noreply@rustyclaw.local")
            .args(["commit-tree", &tree_sha, "-m", subject])
            .output()
            .unwrap();
        assert!(out.status.success());
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    }

    fn make_ref(cwd: &Path, name: &str, sha: &str) {
        let s = git_cmd(cwd)
            .args(["update-ref", name, sha])
            .status()
            .unwrap();
        assert!(s.success());
    }

    #[test]
    fn prune_keeps_newest_n() {
        let td = init_test_repo();
        std::fs::write(td.path().join("x"), "").unwrap();
        git_cmd(td.path()).args(["add", "x"]).status().unwrap();
        git_cmd(td.path())
            .args(["commit", "-q", "-m", "x"])
            .status()
            .unwrap();

        for i in 0..5 {
            let sha = make_empty_commit(td.path(), &format!("session-{i}"));
            make_ref(td.path(), &format!("refs/rustyclaw/sessions/s{i}"), &sha);
        }

        let deleted = prune_old_refs(td.path(), 3).unwrap();
        assert_eq!(deleted, 2, "should delete the 2 oldest of 5 refs");

        let out = git_cmd(td.path())
            .args([
                "for-each-ref",
                "--format=%(refname)",
                "refs/rustyclaw/sessions/",
            ])
            .output()
            .unwrap();
        let remaining = String::from_utf8(out.stdout).unwrap();
        assert_eq!(remaining.lines().count(), 3);
    }

    #[test]
    fn prune_noop_when_unlimited() {
        let td = init_test_repo();
        std::fs::write(td.path().join("x"), "").unwrap();
        git_cmd(td.path()).args(["add", "x"]).status().unwrap();
        git_cmd(td.path())
            .args(["commit", "-q", "-m", "x"])
            .status()
            .unwrap();
        for i in 0..3 {
            let sha = make_empty_commit(td.path(), &format!("s{i}"));
            make_ref(td.path(), &format!("refs/rustyclaw/sessions/s{i}"), &sha);
        }
        let deleted = prune_old_refs(td.path(), 0).unwrap();
        assert_eq!(deleted, 0);
    }

    #[test]
    fn prune_noop_when_below_threshold() {
        let td = init_test_repo();
        std::fs::write(td.path().join("x"), "").unwrap();
        git_cmd(td.path()).args(["add", "x"]).status().unwrap();
        git_cmd(td.path())
            .args(["commit", "-q", "-m", "x"])
            .status()
            .unwrap();
        for i in 0..2 {
            let sha = make_empty_commit(td.path(), &format!("s{i}"));
            make_ref(td.path(), &format!("refs/rustyclaw/sessions/s{i}"), &sha);
        }
        let deleted = prune_old_refs(td.path(), 10).unwrap();
        assert_eq!(deleted, 0);
    }
}
