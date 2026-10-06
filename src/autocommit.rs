//! Auto-commit loop — per-turn working-tree snapshots on private shadow refs.
//!
//! Uses git plumbing (`write-tree`, `commit-tree`, `update-ref`, `read-tree`,
//! `checkout-index`) under `refs/oxideclaw/sessions/<session-id>`, driven via
//! `std::process::Command` with `GIT_INDEX_FILE` pointed at a temp index so the
//! user's real `.git/index` is never touched.
//!
//! Commits are invisible to normal git tooling (`log`, `status`, `branch`) —
//! only `git for-each-ref refs/oxideclaw/` sees them. They are strictly local
//! (never pushed) and serve as a per-turn undo stack the user can navigate
//! with `/undo` and `/redo`.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub use crate::settings::{AutoCommitConfig, DEFAULT_KEEP_SESSIONS, DEFAULT_MESSAGE_PREFIX};

pub const SHADOW_REF_PREFIX: &str = "refs/oxideclaw/sessions/";
/// Prefix used before the rename; refs found there are moved on startup.
pub const LEGACY_SHADOW_REF_PREFIX: &str = "refs/rustyclaw/sessions/";
/// Working-tree states `restore_to` was about to overwrite without any
/// snapshot holding them, per session: `refs/oxideclaw/recovery/<session>`
/// (see [`recovery_ref`]). Each recovery commit keeps the previous one as a
/// second parent, so all of them stay reachable until `prune_old_refs`
/// deletes the session, and its recovery ref with it.
pub const RECOVERY_REF_PREFIX: &str = "refs/oxideclaw/recovery/";
/// The single, unbounded recovery ref used before it became per-session.
const LEGACY_RECOVERY_REF: &str = "refs/oxideclaw/recovery";

/// The recovery ref of one session.
pub fn recovery_ref(session_id: &str) -> String {
    format!("{RECOVERY_REF_PREFIX}{session_id}")
}
/// Canonical empty tree: the session base of a repo with no commits.
const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

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
    /// Commit holding the working tree as it was before the restore, when no
    /// snapshot had it (edits made after the last turn). Reachable from
    /// `recovery_ref`.
    pub saved_edits: Option<String>,
    /// The session's [`recovery_ref`].
    pub recovery_ref: String,
}

impl RestoreReport {
    /// What to tell the user about `saved_edits`, or "" when nothing was saved.
    pub fn saved_edits_note(&self) -> String {
        match &self.saved_edits {
            Some(sha) => format!(
                "\nYour edits since the last snapshot were saved as {} \
                 ({}); `git checkout {sha} -- .` brings them back.",
                &sha[..7.min(sha.len())],
                self.recovery_ref
            ),
            None => String::new(),
        }
    }
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
    // A sandboxed Bash call can write `.git/` when cwd is the repo root, and
    // these commands run on the host: a planted fsmonitor or hook (e.g.
    // `reference-transaction` on our update-ref) would run outside the
    // sandbox. Plumbing needs neither, so neither is ever honoured.
    cmd.args([
        "-c",
        "core.fsmonitor=false",
        "-c",
        "core.hooksPath=/dev/null",
    ]);
    cmd
}

/// What snapshots and /undo trust about a repository, recorded at startup
/// by [`pin_filters`]: where its git dir and work tree are, and the
/// repo-local config that runs commands or moves the work tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoPin {
    git_dir: Option<String>,
    toplevel: Option<String>,
    config: String,
}

/// Pins per (canonical) session directory. Never filled lazily: a repository
/// first seen mid-session may have been made by a sandboxed command.
static PINS: std::sync::Mutex<Option<std::collections::HashMap<PathBuf, RepoPin>>> =
    std::sync::Mutex::new(None);

fn pin_key(cwd: &Path) -> PathBuf {
    std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf())
}

/// Repo-local config a sandboxed command could write that would act on the
/// host: `filter.*` drivers (run by `add -A` and `checkout-index`) and
/// `core.worktree`/`core.bare` (which move what is staged and restored, e.g.
/// to `$HOME`). Read per scope rather than with `--show-scope` (git 2.26+):
/// `--local`, `--includes` and `--worktree` work on every supported git.
fn local_sensitive_config(cwd: &Path, git_dir: &str) -> anyhow::Result<String> {
    let read = |scope: &str| -> anyhow::Result<String> {
        let out = git_cmd(cwd)
            .args([
                "config",
                scope,
                "--includes",
                "--get-regexp",
                r"^(filter\.|core\.worktree$|core\.bare$)",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()?;
        match out.status.code() {
            Some(0) => Ok(String::from_utf8_lossy(&out.stdout).into_owned()),
            Some(1) => Ok(String::new()), // no such keys
            _ => anyhow::bail!("git config {scope} failed; not running repo filters"),
        }
    };
    let mut local = read("--local")?;
    if Path::new(git_dir).join("config.worktree").exists() {
        local.push_str(&read("--worktree")?);
    }
    Ok(local)
}

fn repo_state(cwd: &Path) -> anyhow::Result<RepoPin> {
    let Ok(git_dir) = git_output(git_cmd(cwd).args(["rev-parse", "--absolute-git-dir"])) else {
        return Ok(RepoPin {
            git_dir: None,
            toplevel: None,
            config: String::new(),
        });
    };
    let toplevel = git_output(git_cmd(cwd).args(["rev-parse", "--show-toplevel"])).ok();
    let config = local_sensitive_config(cwd, &git_dir)?;
    Ok(RepoPin {
        git_dir: Some(git_dir),
        toplevel,
        config,
    })
}

/// Record the repository at `cwd` as trusted: call at startup, before any
/// (possibly sandboxed) tool runs. Snapshots and /undo then refuse to run if
/// its git dir, work tree or filter/work-tree config differ from this.
/// Filters are not simply disabled: that would store LFS/git-crypt files raw
/// and /undo would write pointers or ciphertext over them.
pub fn pin_filters(cwd: &Path) -> anyhow::Result<()> {
    let pin = repo_state(cwd)?;
    PINS.lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_or_insert_with(Default::default)
        .insert(pin_key(cwd), pin);
    Ok(())
}

/// Refuse to run the repo's clean/smudge filters, or to stage and restore
/// files, unless the repository is the one [`pin_filters`] recorded. `add -A`
/// and `checkout-index` run filter commands on the host, and `core.worktree`
/// points them at any directory, so a sandboxed command that rewrote
/// `.git/config`, or replaced `.git` with a `gitdir:` file pointing at a git
/// dir of its own, would otherwise escape on the next snapshot or /undo.
/// Global and system config are outside any sandbox's reach and not checked.
/// Returns the pin, so callers use its work tree rather than re-reading it.
pub fn check_filters_unchanged(cwd: &Path) -> anyhow::Result<RepoPin> {
    let now = repo_state(cwd)?;
    let guard = PINS.lock().unwrap_or_else(|e| e.into_inner());
    let Some(pinned) = guard.as_ref().and_then(|m| m.get(&pin_key(cwd))) else {
        anyhow::bail!(
            "this repository was not pinned when OxideClaw started, so its git \
             filters and work tree are not trusted. Restart OxideClaw in this folder \
             to enable auto-commit and /undo."
        );
    };
    if *pinned != now {
        anyhow::bail!(
            "the repository's git filter/core.worktree configuration (or its git \
             directory) changed during this session, so OxideClaw will not run it (it \
             could have been written from inside the sandbox). Review .git and the \
             filter.* and core.worktree entries in .git/config, then restart OxideClaw \
             to resume auto-commit and /undo."
        );
    }
    Ok(now)
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

/// `git` on the temp index at `temp_index`. It may start as a copy of the
/// user's index (see [`seed_index`]), whose split-index, untracked-cache and
/// sparse-index state must not be reused or written back next to `.git/index`.
fn temp_index_cmd(cwd: &Path, temp_index: &Path) -> Command {
    let mut cmd = git_cmd(cwd);
    cmd.env("GIT_INDEX_FILE", temp_index).args([
        "-c",
        "core.splitIndex=false",
        "-c",
        "core.untrackedCache=false",
        "-c",
        "index.sparse=false",
    ]);
    cmd
}

/// Fill `temp_index` with `tree`, keeping the stat data of the user's real
/// index for every entry whose blob already matches (`read-tree -m` with one
/// tree does exactly that). A plain `read-tree` zeroes stat data, so `add -A`
/// re-read and re-hashed (through any clean filter) every tracked file on
/// every turn, freezing the TUI for seconds in large repos.
fn seed_index(cwd: &Path, tree: &str, temp_index: &Path) -> anyhow::Result<()> {
    let warm = git_output(git_cmd(cwd).args(["rev-parse", "--git-path", "index"]))
        .is_ok_and(|real| std::fs::copy(cwd.join(real), temp_index).is_ok())
        && temp_index_cmd(cwd, temp_index)
            .args(["read-tree", "-m", tree])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
    if warm {
        return Ok(());
    }
    // No index yet, or one mid-merge (`read-tree -m` refuses unmerged entries).
    let _ = std::fs::remove_file(temp_index);
    let s = temp_index_cmd(cwd, temp_index)
        .args(["read-tree", tree])
        .status()?;
    if !s.success() {
        anyhow::bail!("git read-tree {tree} failed");
    }
    Ok(())
}

/// Stage the whole working tree into the temp index at `temp_index`, seeded
/// from `seed_tree`, and return the resulting tree SHA. The user's real index
/// is never touched.
fn stage_worktree(
    cwd: &Path,
    seed_tree: Option<&str>,
    temp_index: &Path,
) -> anyhow::Result<String> {
    check_filters_unchanged(cwd)?;
    // Seed from the parent's tree so `add -A` only records diffs relative to
    // it, which makes empty-turn detection accurate even when the user's real
    // index has other staging.
    if let Some(tree) = seed_tree {
        seed_index(cwd, tree, temp_index)?;
    }
    let add_status = temp_index_cmd(cwd, temp_index)
        // Whole tree from any subdirectory, minus OxideClaw's own SQLite
        // index/memory store: snapshotting it stored a binary blob per turn,
        // and /undo overwrote the live database (rolling back memories).
        .args([
            "add",
            "-A",
            "--",
            ":(top)",
            ":(top,exclude,glob)**/.claude/rag.db*",
        ])
        .status()?;
    if !add_status.success() {
        anyhow::bail!("git add -A failed");
    }
    git_output(temp_index_cmd(cwd, temp_index).args(["write-tree"]))
}

/// `git commit-tree` with OxideClaw as author, so shadow commits never carry
/// the user's identity or depend on it being configured.
fn commit_tree(cwd: &Path, tree: &str, parents: &[&str], msg: &str) -> anyhow::Result<String> {
    let mut cmd = git_cmd(cwd);
    cmd.env("GIT_AUTHOR_NAME", "oxideclaw")
        .env("GIT_AUTHOR_EMAIL", "noreply@oxideclaw.local")
        .env("GIT_COMMITTER_NAME", "oxideclaw")
        .env("GIT_COMMITTER_EMAIL", "noreply@oxideclaw.local")
        .args(["commit-tree", "--no-gpg-sign", tree, "-m", msg]);
    for p in parents {
        cmd.args(["-p", p]);
    }
    git_output(&mut cmd)
}

/// Snapshot the working tree as it is before the session's first recorded
/// turn, so `/undo` to the session base returns the user's uncommitted work
/// instead of HEAD. Returns `None` when the tree equals HEAD's (HEAD already
/// is the base) or `cwd` is not in a git repo. The commit is not put on any
/// ref: the first turn's snapshot takes it as parent, which keeps it alive.
pub fn snapshot_base(cwd: &Path) -> anyhow::Result<Option<String>> {
    if !is_git_repo(cwd) {
        return Ok(None);
    }
    let head = resolve_head(cwd);
    let head_tree = match &head {
        Some(h) => tree_of_commit(cwd, h),
        None => Some(EMPTY_TREE.to_string()),
    };
    let td = tempfile::TempDir::new()?;
    let tree = stage_worktree(cwd, head_tree.as_deref(), &td.path().join("base.index"))?;
    if head_tree.as_deref() == Some(tree.as_str()) {
        return Ok(None);
    }
    let parents: Vec<&str> = head.as_deref().into_iter().collect();
    commit_tree(
        cwd,
        &tree,
        &parents,
        "oxideclaw: session base (uncommitted work before the first turn)",
    )
    .map(Some)
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
///
/// `base_commit` (from [`snapshot_base`]) is the parent at position 0; without
/// it HEAD is, and `/undo` to the session base would drop whatever was
/// uncommitted when the session started.
#[allow(clippy::too_many_arguments)]
pub fn snapshot_turn(
    cwd: &Path,
    config: &AutoCommitConfig,
    session_id: &str,
    user_prompt: &str,
    turn_index: u32,
    auto_commits: &mut Vec<String>,
    undo_position: &mut usize,
    base_commit: Option<&str>,
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

    // 1. Stage the whole tree into a temp index (GIT_INDEX_FILE) seeded from
    //    the parent's tree, and write it.
    let parent = if *undo_position > 0 {
        auto_commits.get(*undo_position - 1).cloned()
    } else {
        base_commit
            .map(str::to_string)
            .or_else(|| resolve_head(cwd))
    };
    let parent_tree = parent.as_deref().and_then(|p| tree_of_commit(cwd, p));
    let td = tempfile::TempDir::new()?;
    let tree_sha = stage_worktree(cwd, parent_tree.as_deref(), &td.path().join("turn.index"))?;

    // 2. Empty-turn optimization: compare against parent tree.
    if parent_tree.as_deref() == Some(tree_sha.as_str()) {
        return Ok(SnapshotOutcome::NoChanges);
    }

    // 3. Build the commit.
    let subject = format!(
        "{} turn {}: {}",
        config.message_prefix,
        turn_index,
        subject_from_prompt(user_prompt),
    );
    let files = count_tree_files(cwd, &tree_sha);
    let body = format!(
        "\nOxideClaw-Session: {session_id}\nOxideClaw-Turn: {turn_index}\nOxideClaw-Files: {files}\n"
    );
    let full_msg = format!("{subject}\n{body}");

    let parents: Vec<&str> = parent.as_deref().into_iter().collect();
    let commit_sha = commit_tree(cwd, &tree_sha, &parents, &full_msg)?;

    // 4. Update the shadow ref, compare-and-swap against the value we started
    //    from. An empty expected-old tells git the ref must not exist yet.
    //    A ref that is gone while we hold a chain was deleted (startup prune
    //    of a session later resumed, a fork's fresh id, by hand), not written
    //    by a concurrent instance, which would have created it. Re-create it;
    //    this commit's parents still reach the earlier snapshots. Treating it
    //    as a conflict failed every later turn of the session.
    let ref_exists =
        git_output(git_cmd(cwd).args(["rev-parse", "--verify", "-q", &ref_name])).is_ok();
    let expected_old = match &expected_ref {
        Some(sha) if ref_exists => sha.as_str(),
        _ => "",
    };
    let update_status = git_cmd(cwd)
        .args(["update-ref", &ref_name, &commit_sha, expected_old])
        .status()?;
    if !update_status.success() {
        // The commit object is already written and reachable by sha, so nothing
        // the user did is lost — we simply refuse to move the ref over someone
        // else's work.
        return Ok(SnapshotOutcome::Conflict {
            reason: format!(
                "another oxideclaw instance wrote to this session's history \
                 while this turn was being snapshotted (session '{session_id}'). \
                 This turn was not recorded; the working tree is untouched. \
                 Use a distinct session per instance — /undo history is per-session."
            ),
        });
    }

    // 5. Discard redo tail if user was in an undone state, then append.
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
    session_id: &str,
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
                EMPTY_TREE.to_string()
            }
        } else if let Some(head) = resolve_head(cwd) {
            tree_of_commit(cwd, &head).unwrap_or_default()
        } else {
            EMPTY_TREE.to_string()
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

    let recovery = recovery_ref(session_id);
    let (saved_edits, live_tree) = save_unrecorded_worktree(cwd, &recovery, auto_commits)?;

    // Write only what differs from the live tree. `checkout-index -a` on a
    // fresh index rewrote every file (bumping every mtime, so builds redid
    // everything) and wrote out paths a sparse checkout had left out. Paths
    // absent from the target are skipped (`d`): orphans stay on disk.
    let changed = git_cmd(cwd)
        .args([
            "diff-tree",
            "-r",
            "-z",
            "--name-only",
            "--no-renames",
            "--diff-filter=d",
            &live_tree,
            &tree_sha,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()?;
    if !changed.status.success() {
        anyhow::bail!(
            "git diff-tree failed: {}",
            String::from_utf8_lossy(&changed.stderr)
        );
    }

    let td = tempfile::TempDir::new()?;
    let temp_index = td.path().join("restore.index");
    let read_status = git_cmd(cwd)
        .env("GIT_INDEX_FILE", &temp_index)
        .args(["read-tree", &tree_sha])
        .status()?;
    if !read_status.success() {
        anyhow::bail!("git read-tree {tree_sha} failed");
    }

    // Index paths are repo-relative, and checkout-index run from a
    // subdirectory skips files outside it, so restore from the top level:
    // `--prefix <cwd>/` from `repo/pkg` wrote `repo/pkg/pkg/x` and left the
    // real files untouched.
    // The pinned work tree, not a fresh query: repo config cannot move it.
    let pin = check_filters_unchanged(cwd)?;
    let toplevel = pin
        .toplevel
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("git rev-parse --show-toplevel failed"))?;
    let prefix = format!("{}/", toplevel.display());
    if !changed.stdout.is_empty() {
        use std::io::Write;
        let mut child = git_cmd(&toplevel)
            .env("GIT_INDEX_FILE", &temp_index)
            .args(["checkout-index", "-f", "-z", "--stdin", "--prefix", &prefix])
            .stdin(Stdio::piped())
            .spawn()?;
        // Dropping stdin closes it, so checkout-index sees EOF.
        let written = child
            .stdin
            .take()
            .map(|mut w| w.write_all(&changed.stdout))
            .transpose();
        let checkout_status = child.wait()?;
        written?;
        if !checkout_status.success() {
            anyhow::bail!("git checkout-index --prefix={prefix} failed");
        }
    }

    Ok(RestoreReport {
        files_restored: target_files.len() as u32,
        orphaned_files,
        saved_edits,
        recovery_ref: recovery,
    })
}

/// [`restore_to`] on a blocking thread. It stages the whole work tree through
/// git subprocesses, which must not stall the async runtime the TUI runs on.
pub async fn restore_to_blocking(
    cwd: PathBuf,
    session_id: String,
    auto_commits: Vec<String>,
    target_position: usize,
) -> anyhow::Result<RestoreReport> {
    tokio::task::spawn_blocking(move || {
        restore_to(&cwd, &session_id, &auto_commits, target_position)
    })
    .await
    .map_err(|e| anyhow::anyhow!("restore task failed: {e}"))?
}

/// Before `restore_to` overwrites the working tree, commit it under the
/// session's `recovery` ref unless some snapshot already holds exactly this tree.
/// Edits made after the last turn (or uncommitted work a legacy session base
/// never captured) were otherwise overwritten with no way back. Errors abort
/// the restore: better no undo than an undo that destroys work.
/// Also returns the live tree it staged, which `restore_to` diffs against.
fn save_unrecorded_worktree(
    cwd: &Path,
    recovery: &str,
    auto_commits: &[String],
) -> anyhow::Result<(Option<String>, String)> {
    let head = resolve_head(cwd);
    let latest = auto_commits.last().cloned().or_else(|| head.clone());
    let latest_tree = latest.as_deref().and_then(|c| tree_of_commit(cwd, c));
    let td = tempfile::TempDir::new()?;
    let live_tree = stage_worktree(cwd, latest_tree.as_deref(), &td.path().join("live.index"))?;

    // Every state /undo and /redo can reach: each turn and the session base.
    let mut revs: Vec<String> = auto_commits
        .iter()
        .map(|c| format!("{c}^{{tree}}"))
        .collect();
    if let Some(first) = auto_commits.first() {
        revs.push(format!("{first}^^{{tree}}"));
    }
    if let Some(h) = &head {
        revs.push(format!("{h}^{{tree}}"));
    }
    let mut known: Vec<String> = revs
        .iter()
        .filter_map(|r| git_output(git_cmd(cwd).args(["rev-parse", "--verify", "-q", r])).ok())
        .collect();
    if head.is_none() {
        known.push(EMPTY_TREE.to_string());
    }
    if known.contains(&live_tree) {
        return Ok((None, live_tree));
    }

    let previous = git_output(git_cmd(cwd).args(["rev-parse", "--verify", "-q", recovery])).ok();
    let mut parents: Vec<&str> = latest.as_deref().into_iter().collect();
    if let Some(prev) = &previous {
        parents.push(prev);
    }
    let sha = commit_tree(
        cwd,
        &live_tree,
        &parents,
        "oxideclaw: working tree saved before /undo or /redo",
    )?;
    let status = git_cmd(cwd)
        .args([
            "update-ref",
            recovery,
            &sha,
            previous.as_deref().unwrap_or(""),
        ])
        .status()?;
    if !status.success() {
        anyhow::bail!("could not save un-snapshotted edits to {recovery}; nothing was restored");
    }
    Ok((Some(sha), live_tree))
}

// ── Prune pipeline ────────────────────────────────────────────────────────────

/// Delete old `refs/oxideclaw/sessions/*` refs, keeping the `keep` newest by
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

/// Move `refs/rustyclaw/sessions/*` to `refs/oxideclaw/sessions/*` so undo
/// history survives the rename. Returns how many refs moved.
pub fn migrate_legacy_refs(cwd: &Path) -> anyhow::Result<u32> {
    if !is_git_repo(cwd) {
        return Ok(0);
    }
    let out = git_cmd(cwd)
        .args([
            "for-each-ref",
            "--format=%(objectname) %(refname)",
            LEGACY_SHADOW_REF_PREFIX.trim_end_matches('/'),
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()?;
    if !out.status.success() {
        return Ok(0);
    }
    // The old single recovery ref would block every per-session one
    // (`recovery` cannot be both a ref and a directory of refs).
    if let Ok(sha) =
        git_output(git_cmd(cwd).args(["rev-parse", "--verify", "-q", LEGACY_RECOVERY_REF]))
    {
        let deleted = git_cmd(cwd)
            .args(["update-ref", "-d", LEGACY_RECOVERY_REF, &sha])
            .status();
        if matches!(deleted, Ok(s) if s.success()) {
            let _ = git_cmd(cwd)
                .args(["update-ref", &recovery_ref("legacy"), &sha])
                .status();
        }
    }
    let mut moved = 0u32;
    for line in String::from_utf8(out.stdout)?.lines() {
        let Some((sha, old)) = line.split_once(' ') else {
            continue;
        };
        let Some(rest) = old.strip_prefix(LEGACY_SHADOW_REF_PREFIX) else {
            continue;
        };
        let new = format!("{SHADOW_REF_PREFIX}{rest}");
        let created = git_cmd(cwd).args(["update-ref", &new, sha]).status();
        if !matches!(created, Ok(s) if s.success()) {
            tracing::warn!("autoCommit migrate: could not create {new}");
            continue;
        }
        match git_cmd(cwd).args(["update-ref", "-d", old]).status() {
            Ok(s) if s.success() => moved += 1,
            _ => tracing::warn!("autoCommit migrate: could not delete {old}"),
        }
    }
    Ok(moved)
}

/// `current_session` is never deleted: a resumed old session would otherwise
/// lose its ref at startup.
pub fn prune_old_refs(cwd: &Path, keep: u32, current_session: Option<&str>) -> anyhow::Result<u32> {
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

    let current = current_session.map(shadow_ref);
    let mut to_delete = select_refs_to_delete(rows, keep as usize);
    to_delete.retain(|r| Some(r) != current.as_ref());

    let mut deleted = 0u32;
    for r in &to_delete {
        let status = git_cmd(cwd).args(["update-ref", "-d", r]).status();
        match status {
            Ok(s) if s.success() => {
                deleted += 1;
                // The session's saved edits go with it, or they would keep
                // every pruned snapshot alive through their parents.
                if let Some(id) = r.strip_prefix(SHADOW_REF_PREFIX) {
                    let rec = recovery_ref(id);
                    if let Ok(sha) =
                        git_output(git_cmd(cwd).args(["rev-parse", "--verify", "-q", &rec]))
                    {
                        let _ = git_cmd(cwd).args(["update-ref", "-d", &rec, &sha]).status();
                    }
                }
            }
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
#[allow(clippy::too_many_arguments)]
pub fn snapshot_turn_raw(
    cwd: &Path,
    message_prefix: &str,
    session_id: &str,
    user_prompt: &str,
    turn_index: u32,
    auto_commits: &mut Vec<String>,
    undo_position: &mut usize,
    base_commit: Option<&str>,
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
        base_commit,
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
        assert_eq!(
            deleted,
            vec!["zzz".to_string()],
            "older loses regardless of name"
        );
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
            ("user.name", "oxideclaw-test"),
            ("user.email", "noreply@oxideclaw.local"),
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
        // What OxideClaw does at startup: trust the repo as it is now.
        pin_filters(td.path()).unwrap();
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
            None,
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
            .args(["rev-parse", "refs/oxideclaw/sessions/test-session-1"])
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
        snapshot_turn(
            td.path(),
            &cfg,
            "s1",
            "add a",
            1,
            &mut commits,
            &mut pos,
            None,
        )
        .unwrap();

        write_file(td.path(), "b.txt", "2\n");
        snapshot_turn(
            td.path(),
            &cfg,
            "s1",
            "add b",
            2,
            &mut commits,
            &mut pos,
            None,
        )
        .unwrap();

        write_file(td.path(), "c.txt", "3\n");
        snapshot_turn(
            td.path(),
            &cfg,
            "s1",
            "add c",
            3,
            &mut commits,
            &mut pos,
            None,
        )
        .unwrap();

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
            None,
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
            snapshot_turn(td.path(), &cfg, "s", "msg", 1, &mut commits, &mut pos, None).unwrap();
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
            snapshot_turn(td.path(), &cfg, "s", "msg", 1, &mut commits, &mut pos, None).unwrap();
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
        snapshot_turn(
            td.path(),
            &cfg,
            "s",
            "ignored",
            1,
            &mut commits,
            &mut pos,
            None,
        )
        .unwrap();
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
            None,
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
        snapshot_turn(td.path(), &cfg, "s", "a", 1, &mut commits, &mut pos, None).unwrap();
        write_file(td.path(), "b.txt", "2\n");
        snapshot_turn(td.path(), &cfg, "s", "b", 2, &mut commits, &mut pos, None).unwrap();
        write_file(td.path(), "c.txt", "3\n");
        snapshot_turn(td.path(), &cfg, "s", "c", 3, &mut commits, &mut pos, None).unwrap();
        assert_eq!(commits.len(), 3);
        assert_eq!(pos, 3);

        // Simulate /undo 2 → pos = 1, then new work — expect redo tail discarded.
        pos = 1;
        write_file(td.path(), "d.txt", "4\n");
        snapshot_turn(td.path(), &cfg, "s", "d", 2, &mut commits, &mut pos, None).unwrap();
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
        snapshot_turn(td.path(), &cfg, "s", "v2", 1, &mut commits, &mut pos, None).unwrap();
        write_file(td.path(), "app.txt", "v3\n");
        snapshot_turn(td.path(), &cfg, "s", "v3", 2, &mut commits, &mut pos, None).unwrap();

        let report = restore_to(td.path(), "test", &commits, 1).unwrap();
        assert!(report.files_restored >= 1);
        let contents = std::fs::read_to_string(td.path().join("app.txt")).unwrap();
        assert_eq!(contents, "v2\n");

        let _ = restore_to(td.path(), "test", &commits, 2).unwrap();
        let contents = std::fs::read_to_string(td.path().join("app.txt")).unwrap();
        assert_eq!(contents, "v3\n");
    }

    #[test]
    fn snapshots_leave_out_the_rag_database() {
        let td = init_test_repo();
        write_file(td.path(), "app.txt", "v1\n");
        write_file(td.path(), ".claude/rag.db", "sqlite");
        write_file(td.path(), ".claude/rag.db-wal", "wal");
        let mut commits = Vec::new();
        let mut pos = 0usize;
        snapshot_turn(
            td.path(),
            &AutoCommitConfig::default(),
            "s",
            "t",
            1,
            &mut commits,
            &mut pos,
            None,
        )
        .unwrap();
        let tree = tree_of_commit(td.path(), &commits[0]).unwrap();
        let files = list_tree_files(td.path(), &tree);
        assert!(files.iter().any(|f| f == "app.txt"), "{files:?}");
        assert!(!files.iter().any(|f| f.contains("rag.db")), "{files:?}");
    }

    /// Launched from `repo/pkg`, /undo must restore `repo/pkg/x` in place,
    /// not write `repo/pkg/pkg/x`, and must reach files outside `pkg/`.
    #[test]
    fn restore_from_a_subdirectory_restores_in_place() {
        let td = init_test_repo();
        write_file(td.path(), "pkg/x.txt", "v1\n");
        write_file(td.path(), "top.txt", "v1\n");
        git_cmd(td.path()).args(["add", "-A"]).status().unwrap();
        git_cmd(td.path())
            .args(["commit", "-q", "-m", "v1"])
            .status()
            .unwrap();
        let sub = td.path().join("pkg");
        // OxideClaw launched from the subdirectory pins it at startup.
        pin_filters(&sub).unwrap();

        let cfg = AutoCommitConfig::default();
        let mut commits = Vec::new();
        let mut pos = 0usize;
        write_file(td.path(), "pkg/x.txt", "v2\n");
        write_file(td.path(), "top.txt", "v2\n");
        snapshot_turn(&sub, &cfg, "s", "v2", 1, &mut commits, &mut pos, None).unwrap();
        write_file(td.path(), "pkg/x.txt", "v3\n");
        write_file(td.path(), "top.txt", "v3\n");
        snapshot_turn(&sub, &cfg, "s", "v3", 2, &mut commits, &mut pos, None).unwrap();

        restore_to(&sub, "test", &commits, 1).unwrap();
        let read = |p: &str| std::fs::read_to_string(td.path().join(p)).unwrap();
        assert_eq!(read("pkg/x.txt"), "v2\n");
        assert_eq!(read("top.txt"), "v2\n");
        assert!(!td.path().join("pkg/pkg").exists(), "no nested copy");
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
        snapshot_turn(td.path(), &cfg, "s", "mod", 1, &mut commits, &mut pos, None).unwrap();

        write_file(td.path(), "untracked.log", "scratch\n");

        restore_to(td.path(), "test", &commits, 0).unwrap();
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
        snapshot_turn(td.path(), &cfg, "s", "m", 1, &mut commits, &mut pos, None).unwrap();

        restore_to(td.path(), "test", &commits, 0).unwrap();
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
        snapshot_turn(
            td.path(),
            &cfg,
            "s",
            "add new",
            1,
            &mut commits,
            &mut pos,
            None,
        )
        .unwrap();

        let report = restore_to(td.path(), "test", &commits, 0).unwrap();
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
        snapshot_turn(
            td.path(),
            &cfg,
            "s",
            "first",
            1,
            &mut commits,
            &mut pos,
            None,
        )
        .unwrap();
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
        let report = restore_to(td.path(), "test", &commits, 0).unwrap();
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

#[cfg(all(test, unix))]
mod sandbox_escape_tests {
    use super::git_detection_tests::init_test_repo;
    use super::snapshot_tests::write_file;
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// A script that leaves `marker` behind; as a filter it passes data
    /// through, otherwise it exits without reading stdin (an fsmonitor hook
    /// that waits on stdin hangs git).
    fn script(path: &Path, marker: &Path, filter: bool) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let tail = if filter { "cat\n" } else { "" };
        std::fs::write(
            path,
            format!("#!/bin/sh\ntouch '{}'\n{tail}", marker.display()),
        )
        .unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn git_config(repo: &Path, k: &str, v: &str) {
        let s = Command::new("git")
            .args(["config", k, v])
            .current_dir(repo)
            .status()
            .unwrap();
        assert!(s.success());
    }

    fn turn(
        repo: &Path,
        commits: &mut Vec<String>,
        pos: &mut usize,
        n: u32,
    ) -> anyhow::Result<SnapshotOutcome> {
        write_file(repo, "f.txt", &format!("turn {n}\n"));
        snapshot_turn(
            repo,
            &AutoCommitConfig::default(),
            "escape",
            "p",
            n,
            commits,
            pos,
            None,
        )
    }

    /// What a sandboxed command can plant in a writable `.git/` without any
    /// filter: a hook that our update-ref fires, and an fsmonitor that our
    /// `add -A` runs. Neither may execute on the host.
    #[test]
    fn planted_hooks_and_fsmonitor_never_run() {
        let td = init_test_repo();
        let marker = td.path().join("pwned");
        script(
            &td.path().join(".git/hooks/reference-transaction"),
            &marker,
            false,
        );
        let mon = td.path().join(".git/mon.sh");
        script(&mon, &marker, false);
        git_config(td.path(), "core.fsmonitor", &mon.display().to_string());

        let (mut commits, mut pos) = (Vec::new(), 0);
        let out = turn(td.path(), &mut commits, &mut pos, 1).unwrap();
        assert!(matches!(out, SnapshotOutcome::Committed { .. }), "{out:?}");
        restore_to(td.path(), "test", &commits, 0).unwrap();
        assert!(
            !marker.exists(),
            "a planted hook or fsmonitor ran on the host"
        );
    }

    /// A filter driver added mid-session (as from inside the sandbox) is not
    /// run by the next snapshot or /undo; one present from the start is
    /// trusted, so LFS/git-crypt repos keep working.
    #[test]
    fn a_filter_added_mid_session_is_refused_but_a_preexisting_one_runs() {
        let td = init_test_repo();
        let trusted = td.path().join("trusted-ran");
        script(&td.path().join(".git/ok.sh"), &trusted, true);
        git_config(
            td.path(),
            "filter.ok.clean",
            &td.path().join(".git/ok.sh").display().to_string(),
        );
        write_file(
            td.path(),
            ".gitattributes",
            "*.txt filter=ok\n*.bin filter=evil\n",
        );
        // The session starts with the `ok` filter already configured.
        pin_filters(td.path()).unwrap();

        let (mut commits, mut pos) = (Vec::new(), 0);
        let out = turn(td.path(), &mut commits, &mut pos, 1).unwrap();
        assert!(matches!(out, SnapshotOutcome::Committed { .. }), "{out:?}");
        assert!(
            trusted.exists(),
            "a filter configured before the session must run"
        );

        let marker = td.path().join("pwned");
        let evil = td.path().join(".git/evil.sh");
        script(&evil, &marker, true);
        git_config(td.path(), "filter.evil.clean", &evil.display().to_string());
        git_config(td.path(), "filter.evil.smudge", &evil.display().to_string());
        write_file(td.path(), "x.bin", "data\n");

        let err = turn(td.path(), &mut commits, &mut pos, 2).unwrap_err();
        assert!(
            err.to_string().contains("changed during this session"),
            "{err}"
        );
        assert!(restore_to(td.path(), "test", &commits, 0).is_err());
        assert!(!marker.exists(), "a planted filter ran on the host");
    }

    /// `mv .git evil; echo 'gitdir: evil' > .git` gave a git dir never seen
    /// before, so its filters were pinned as trusted on first sight.
    #[test]
    fn a_swapped_in_git_dir_is_not_trusted() {
        let td = init_test_repo();
        let (mut commits, mut pos) = (Vec::new(), 0);
        turn(td.path(), &mut commits, &mut pos, 1).unwrap();

        std::fs::rename(td.path().join(".git"), td.path().join("evil")).unwrap();
        std::fs::write(td.path().join(".git"), "gitdir: evil\n").unwrap();
        let marker = td.path().join("pwned");
        let evil = td.path().join("evil/evil.sh");
        script(&evil, &marker, true);
        git_config(td.path(), "filter.x.clean", &evil.display().to_string());
        git_config(td.path(), "filter.x.smudge", &evil.display().to_string());
        write_file(td.path(), ".gitattributes", "* filter=x\n");

        let err = turn(td.path(), &mut commits, &mut pos, 2).unwrap_err();
        assert!(
            err.to_string().contains("changed during this session"),
            "{err}"
        );
        assert!(restore_to(td.path(), "test", &commits, 0).is_err());
        assert!(!marker.exists(), "the swapped-in filter ran on the host");
    }

    /// A repository that appears after startup (`git init` from a sandboxed
    /// command) was pinned lazily, with whatever filters it was made with.
    #[test]
    fn a_repo_that_was_not_pinned_at_startup_is_refused() {
        let td = tempfile::TempDir::new().unwrap();
        pin_filters(td.path()).unwrap(); // not a repo yet
        let s = Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(td.path())
            .status()
            .unwrap();
        assert!(s.success());
        assert!(check_filters_unchanged(td.path()).is_err());

        let other = tempfile::TempDir::new().unwrap();
        let s = Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(other.path())
            .status()
            .unwrap();
        assert!(s.success());
        let err = check_filters_unchanged(other.path()).unwrap_err();
        assert!(err.to_string().contains("not pinned"), "{err}");
    }

    /// `git config core.worktree $HOME` made `add -A` stage files from
    /// outside the project into `.git/objects`, readable from the sandbox.
    #[test]
    fn a_moved_work_tree_is_refused() {
        // `home/` stands in for $HOME, `home/proj` for the project in it.
        let home = tempfile::TempDir::new().unwrap();
        let proj = home.path().join("proj");
        std::fs::create_dir(&proj).unwrap();
        let s = Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(&proj)
            .status()
            .unwrap();
        assert!(s.success());
        pin_filters(&proj).unwrap();
        write_file(home.path(), "secret/id_rsa", "KEY\n");
        git_config(&proj, "core.worktree", &home.path().display().to_string());

        let (mut commits, mut pos) = (Vec::new(), 0);
        let err = turn(&proj, &mut commits, &mut pos, 1).unwrap_err();
        assert!(err.to_string().contains("core.worktree"), "{err}");
        let objects = Command::new("git")
            .args(["count-objects"])
            .current_dir(&proj)
            .output()
            .unwrap();
        assert!(
            String::from_utf8_lossy(&objects.stdout).starts_with("0 objects"),
            "files from outside the project were staged"
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
            .env("GIT_AUTHOR_NAME", "oxideclaw")
            .env("GIT_AUTHOR_EMAIL", "noreply@oxideclaw.local")
            .env("GIT_COMMITTER_NAME", "oxideclaw")
            .env("GIT_COMMITTER_EMAIL", "noreply@oxideclaw.local")
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
    fn legacy_refs_are_moved_under_the_new_prefix() {
        let td = init_test_repo();
        std::fs::write(td.path().join("x"), "").unwrap();
        git_cmd(td.path()).args(["add", "x"]).status().unwrap();
        git_cmd(td.path())
            .args(["commit", "-q", "-m", "c"])
            .status()
            .unwrap();
        let sha = String::from_utf8(
            git_cmd(td.path())
                .args(["rev-parse", "HEAD"])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_string();
        make_ref(td.path(), &format!("{LEGACY_SHADOW_REF_PREFIX}s1/1"), &sha);
        make_ref(td.path(), &format!("{LEGACY_SHADOW_REF_PREFIX}s2/1"), &sha);
        make_ref(td.path(), LEGACY_RECOVERY_REF, &sha);
        assert_eq!(migrate_legacy_refs(td.path()).unwrap(), 2);
        // The single recovery ref moves aside, so per-session ones can exist.
        make_ref(td.path(), &recovery_ref("s1"), &sha);
        let refs = String::from_utf8(
            git_cmd(td.path())
                .args(["for-each-ref", "--format=%(refname)"])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap();
        assert!(refs.contains(&format!("{SHADOW_REF_PREFIX}s1/1")), "{refs}");
        assert!(refs.contains(&format!("{SHADOW_REF_PREFIX}s2/1")), "{refs}");
        assert!(!refs.contains("refs/rustyclaw/"), "{refs}");
        // Idempotent.
        assert_eq!(migrate_legacy_refs(td.path()).unwrap(), 0);
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
            make_ref(td.path(), &format!("refs/oxideclaw/sessions/s{i}"), &sha);
        }
        // Saved edits of a pruned session (s0) and a kept one (s4).
        let rec = make_empty_commit(td.path(), "recovery");
        make_ref(td.path(), &recovery_ref("s0"), &rec);
        make_ref(td.path(), &recovery_ref("s4"), &rec);

        let deleted = prune_old_refs(td.path(), 3, None).unwrap();
        assert_eq!(deleted, 2, "should delete the 2 oldest of 5 refs");
        // The recovery ref of a pruned session would keep all of its
        // snapshots alive through their parents.
        let has = |r: &str| {
            git_cmd(td.path())
                .args(["rev-parse", "--verify", "-q", r])
                .output()
                .unwrap()
                .status
                .success()
        };
        assert!(!has(&recovery_ref("s0")));
        assert!(has(&recovery_ref("s4")));

        let out = git_cmd(td.path())
            .args([
                "for-each-ref",
                "--format=%(refname)",
                "refs/oxideclaw/sessions/",
            ])
            .output()
            .unwrap();
        let remaining = String::from_utf8(out.stdout).unwrap();
        assert_eq!(remaining.lines().count(), 3);
    }

    /// A resumed session older than the newest `keep` lost its ref at
    /// startup, and every later turn then reported a false conflict.
    #[test]
    fn prune_never_deletes_the_current_session() {
        let td = init_test_repo();
        for i in 0..5 {
            let sha = make_empty_commit(td.path(), &format!("session-{i}"));
            make_ref(td.path(), &format!("refs/oxideclaw/sessions/s{i}"), &sha);
        }
        // Created in order, so s0 and s1 are the two oldest.
        assert_eq!(prune_old_refs(td.path(), 3, Some("s0")).unwrap(), 1);
        let has = |r: &str| {
            git_cmd(td.path())
                .args(["rev-parse", "--verify", "-q", r])
                .output()
                .unwrap()
                .status
                .success()
        };
        assert!(has("refs/oxideclaw/sessions/s0"), "current session pruned");
        assert!(!has("refs/oxideclaw/sessions/s1"));
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
            make_ref(td.path(), &format!("refs/oxideclaw/sessions/s{i}"), &sha);
        }
        let deleted = prune_old_refs(td.path(), 0, None).unwrap();
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
            make_ref(td.path(), &format!("refs/oxideclaw/sessions/s{i}"), &sha);
        }
        let deleted = prune_old_refs(td.path(), 10, None).unwrap();
        assert_eq!(deleted, 0);
    }
}

#[cfg(test)]
mod resume_and_restore_tests {
    use super::git_detection_tests::init_test_repo;
    use super::snapshot_tests::write_file;
    use super::*;
    use std::time::{Duration, SystemTime};

    fn git(repo: &Path, args: &[&str]) {
        let s = git_cmd(repo).args(args).status().unwrap();
        assert!(s.success(), "git {args:?}");
    }

    fn age(repo: &Path, rel: &str) -> SystemTime {
        let when = SystemTime::now() - Duration::from_secs(3600);
        std::fs::File::options()
            .write(true)
            .open(repo.join(rel))
            .unwrap()
            .set_modified(when)
            .unwrap();
        std::fs::metadata(repo.join(rel))
            .unwrap()
            .modified()
            .unwrap()
    }

    fn turn(repo: &Path, commits: &mut Vec<String>, pos: &mut usize, n: u32) -> SnapshotOutcome {
        snapshot_turn(
            repo,
            &AutoCommitConfig::default(),
            "s",
            "p",
            n,
            commits,
            pos,
            None,
        )
        .unwrap()
    }

    /// The ref was pruned at startup (or the session forked to a new id)
    /// while `auto_commits` still held the chain: no other writer exists, so
    /// the turn must be recorded, not reported as a conflict forever.
    #[test]
    fn a_deleted_session_ref_is_recreated_not_a_conflict() {
        let td = init_test_repo();
        let (mut commits, mut pos) = (Vec::new(), 0);
        write_file(td.path(), "f.txt", "1\n");
        assert!(matches!(
            turn(td.path(), &mut commits, &mut pos, 1),
            SnapshotOutcome::Committed { .. }
        ));
        git(td.path(), &["update-ref", "-d", &shadow_ref("s")]);

        write_file(td.path(), "f.txt", "2\n");
        let out = turn(td.path(), &mut commits, &mut pos, 2);
        assert!(matches!(out, SnapshotOutcome::Committed { .. }), "{out:?}");
        let head = git_output(git_cmd(td.path()).args(["rev-parse", &shadow_ref("s")])).unwrap();
        assert_eq!(head, commits[1]);
        let parent =
            git_output(git_cmd(td.path()).args(["rev-parse", &format!("{}^", commits[1])]))
                .unwrap();
        assert_eq!(parent, commits[0], "the chain must stay linked");
    }

    /// /undo rewrote every tracked file, so every mtime moved and build
    /// tools rebuilt the whole project.
    #[test]
    fn restore_leaves_unchanged_files_untouched() {
        let td = init_test_repo();
        write_file(td.path(), "a.txt", "v1\n");
        write_file(td.path(), "b.txt", "same\n");
        git(td.path(), &["add", "-A"]);
        git(td.path(), &["commit", "-q", "-m", "base"]);
        let (mut commits, mut pos) = (Vec::new(), 0);
        write_file(td.path(), "a.txt", "v2\n");
        turn(td.path(), &mut commits, &mut pos, 1);
        let before = age(td.path(), "b.txt");

        restore_to(td.path(), "s", &commits, 0).unwrap();
        assert_eq!(
            std::fs::read_to_string(td.path().join("a.txt")).unwrap(),
            "v1\n"
        );
        let after = std::fs::metadata(td.path().join("b.txt"))
            .unwrap()
            .modified()
            .unwrap();
        assert_eq!(before, after, "an unchanged file was rewritten");
    }

    /// Paths a sparse checkout leaves out stay out after /undo.
    #[test]
    fn restore_does_not_materialize_sparse_excluded_paths() {
        let td = init_test_repo();
        write_file(td.path(), "src/s1", "v1\n");
        write_file(td.path(), "other/f1", "out of cone\n");
        git(td.path(), &["add", "-A"]);
        git(td.path(), &["commit", "-q", "-m", "base"]);
        git(td.path(), &["sparse-checkout", "set", "src"]);
        assert!(!td.path().join("other").exists());
        pin_filters(td.path()).unwrap();

        let (mut commits, mut pos) = (Vec::new(), 0);
        write_file(td.path(), "src/s1", "v2\n");
        turn(td.path(), &mut commits, &mut pos, 1);
        restore_to(td.path(), "s", &commits, 0).unwrap();
        assert_eq!(
            std::fs::read_to_string(td.path().join("src/s1")).unwrap(),
            "v1\n"
        );
        assert!(
            !td.path().join("other/f1").exists(),
            "out-of-cone path written"
        );
        // The snapshot still holds it, so nothing is lost.
        let files = list_tree_files(td.path(), &tree_of_commit(td.path(), &commits[0]).unwrap());
        assert!(files.contains(&"other/f1".to_string()), "{files:?}");
    }

    /// A snapshot re-hashed every tracked file through the clean filter
    /// (seconds per turn in large or LFS repos); only changed files should be.
    #[cfg(unix)]
    #[test]
    fn snapshots_only_rehash_changed_files() {
        use std::os::unix::fs::PermissionsExt;
        let td = init_test_repo();
        let logdir = tempfile::TempDir::new().unwrap();
        let log = logdir.path().join("cleaned");
        let filter = logdir.path().join("count.sh");
        std::fs::write(
            &filter,
            format!("#!/bin/sh\necho x >> '{}'\ncat\n", log.display()),
        )
        .unwrap();
        std::fs::set_permissions(&filter, std::fs::Permissions::from_mode(0o755)).unwrap();
        git(
            td.path(),
            &[
                "config",
                "filter.count.clean",
                &filter.display().to_string(),
            ],
        );
        write_file(td.path(), ".gitattributes", "*.txt filter=count\n");
        for i in 0..20 {
            let f = format!("f{i}.txt");
            write_file(td.path(), &f, &format!("{i}\n"));
            // Older than the index git writes next, so no entry is racy.
            age(td.path(), &f);
        }
        git(td.path(), &["add", "-A"]);
        git(td.path(), &["commit", "-q", "-m", "base"]);
        pin_filters(td.path()).unwrap();
        let _ = std::fs::remove_file(&log);

        write_file(td.path(), "f0.txt", "changed\n");
        let (mut commits, mut pos) = (Vec::new(), 0);
        let out = turn(td.path(), &mut commits, &mut pos, 1);
        assert!(matches!(out, SnapshotOutcome::Committed { .. }), "{out:?}");
        let runs = std::fs::read_to_string(&log)
            .unwrap_or_default()
            .lines()
            .count();
        assert!(
            runs <= 2,
            "clean filter ran {runs} times for one changed file"
        );
    }
}
