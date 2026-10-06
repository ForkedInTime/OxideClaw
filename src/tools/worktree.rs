/// EnterWorktreeTool / ExitWorktreeTool — port of tools/EnterWorktreeTool + ExitWorktreeTool
/// Creates and removes git worktrees for isolated development sessions.
use super::{Tool, ToolContext, ToolOutput, async_trait};
use anyhow::Result;
use serde::Deserialize;
use serde_json::json;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::process::Command;
use uuid::Uuid;

/// Shared worktree session state (current worktree path if inside one)
pub type WorktreeState = Arc<Mutex<Option<WorktreeSession>>>;

#[derive(Debug, Clone)]
pub struct WorktreeSession {
    pub path: PathBuf,
    pub branch: String,
    pub original_cwd: PathBuf,
}

#[allow(dead_code)] // factory used when worktree tools are registered
pub fn new_worktree_state() -> WorktreeState {
    Arc::new(Mutex::new(None))
}

// ── EnterWorktree ─────────────────────────────────────────────────────────────

pub struct EnterWorktreeTool {
    pub state: WorktreeState,
}

#[derive(Deserialize)]
struct EnterInput {
    #[serde(default)]
    name: Option<String>,
}

#[async_trait]
impl Tool for EnterWorktreeTool {
    fn name(&self) -> &str {
        "EnterWorktree"
    }

    fn description(&self) -> &str {
        "Create and enter a git worktree for isolated work. Creates a new branch \
        and worktree directory so changes don't affect the main working tree. \
        Use ExitWorktree when done."
    }

    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Optional name for the worktree branch (alphanumeric, dashes, underscores). Auto-generated if omitted."
                }
            }
        })
    }

    async fn execute(&self, input: serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let input: EnterInput = serde_json::from_value(input)?;

        // Must not already be in a worktree
        if self
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
        {
            return Ok(ToolOutput::error(
                "Already in a worktree session. Use ExitWorktree first.",
            ));
        }

        // Validate we're in a git repo
        let git_check = Command::new("git")
            .args(["rev-parse", "--git-dir"])
            .current_dir(&ctx.cwd)
            .output()
            .await?;

        if !git_check.status.success() {
            return Ok(ToolOutput::error(
                "Not inside a git repository — cannot create a worktree.",
            ));
        }

        // Build branch/worktree name
        let slug = input
            .name
            .unwrap_or_else(|| format!("wt-{}", &Uuid::new_v4().to_string()[..8]));

        // Validate slug: only alphanumeric, dash, underscore, dot
        if !slug
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.'))
        {
            return Ok(ToolOutput::error(
                "Worktree name may only contain letters, digits, dashes, underscores, and dots.",
            ));
        }

        // Find git root
        let git_root = String::from_utf8_lossy(
            &Command::new("git")
                .args(["rev-parse", "--show-toplevel"])
                .current_dir(&ctx.cwd)
                .output()
                .await?
                .stdout,
        )
        .trim()
        .to_string();

        let worktree_path = PathBuf::from(&git_root)
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(std::env::temp_dir)
            .join(format!(
                "{}-{}",
                git_root.split('/').next_back().unwrap_or("repo"),
                slug
            ));

        // Create worktree + branch
        let output = Command::new("git")
            .args([
                "worktree",
                "add",
                "-b",
                &slug,
                worktree_path.to_str().unwrap_or("/tmp/wt"),
                "HEAD",
            ])
            .current_dir(&ctx.cwd)
            .output()
            .await?;

        if !output.status.success() {
            let err = String::from_utf8_lossy(&output.stderr);
            return Ok(ToolOutput::error(format!("git worktree add failed: {err}")));
        }

        let session = WorktreeSession {
            path: worktree_path.clone(),
            branch: slug.clone(),
            original_cwd: ctx.cwd.clone(),
        };
        *self.state.lock().unwrap_or_else(|e| e.into_inner()) = Some(session);

        Ok(ToolOutput::success(
            json!({
                "worktreePath": worktree_path.to_string_lossy(),
                "worktreeBranch": slug,
                "message": format!("Entered worktree '{}' at {}", slug, worktree_path.display())
            })
            .to_string(),
        ))
    }
}

// ── ExitWorktree ──────────────────────────────────────────────────────────────

pub struct ExitWorktreeTool {
    pub state: WorktreeState,
}

#[derive(Deserialize)]
struct ExitInput {
    #[serde(default)]
    discard_changes: bool,
}

#[async_trait]
impl Tool for ExitWorktreeTool {
    fn name(&self) -> &str {
        "ExitWorktree"
    }

    fn description(&self) -> &str {
        "Exit the current git worktree and return to the original working directory. \
        Removes the worktree directory and keeps its branch. Uncommitted changes block \
        removal unless discard_changes is true; commit them on the branch first to keep them."
    }

    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "discard_changes": {
                    "type": "boolean",
                    "description": "Delete the worktree even if it has uncommitted or untracked changes. Those changes are lost. Default false."
                }
            }
        })
    }

    async fn execute(&self, input: serde_json::Value, _ctx: &ToolContext) -> Result<ToolOutput> {
        let input: ExitInput = serde_json::from_value(input)?;
        // Cloned, not taken: if git refuses, the session must stay open so
        // the model can commit and retry.
        let session = self.state.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let Some(s) = session else {
            return Ok(ToolOutput::error("Not currently in a worktree session."));
        };

        // Without --force git refuses to delete modified or untracked files,
        // which are unrecoverable once gone: the branch still points at the
        // commit the worktree was created from.
        let mut args = vec!["worktree", "remove"];
        if input.discard_changes {
            args.push("--force");
        }
        args.push(s.path.to_str().unwrap_or(""));
        let output = Command::new("git")
            .args(&args)
            .current_dir(&s.original_cwd)
            .output()
            .await?;

        if !output.status.success() {
            let err = String::from_utf8_lossy(&output.stderr);
            return Ok(ToolOutput::error(format!(
                "git worktree remove failed: {}\nThe worktree is still open. To keep its changes, \
                 commit them on branch '{}' first (git -C '{}' add -A && git -C '{}' commit -m ...), \
                 then call ExitWorktree again; or call ExitWorktree with discard_changes=true to \
                 delete them.",
                err.trim(),
                s.branch,
                s.path.display(),
                s.path.display(),
            )));
        }

        *self.state.lock().unwrap_or_else(|e| e.into_inner()) = None;
        Ok(ToolOutput::success(format!(
            "Exited worktree '{}'. Returned to {}. Branch '{}' preserved.",
            s.path.display(),
            s.original_cwd.display(),
            s.branch,
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(dir: &std::path::Path, args: &[&str]) {
        let ok = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap()
            .status
            .success();
        assert!(ok, "git {args:?}");
    }

    /// A repo inside its own temp dir, so the sibling worktree EnterWorktree
    /// creates next to it is cleaned up with it.
    fn repo() -> (tempfile::TempDir, PathBuf) {
        let outer = tempfile::tempdir().unwrap();
        let root = outer.path().join("repo");
        std::fs::create_dir(&root).unwrap();
        git(&root, &["init", "-q"]);
        git(
            &root,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "init",
            ],
        );
        (outer, root)
    }

    async fn enter(state: &WorktreeState, root: &std::path::Path) -> PathBuf {
        let out = EnterWorktreeTool {
            state: state.clone(),
        }
        .execute(json!({"name": "wt"}), &ToolContext::new(root.to_path_buf()))
        .await
        .unwrap();
        assert!(!out.is_error);
        state.lock().unwrap().as_ref().unwrap().path.clone()
    }

    /// ExitWorktree ran `git worktree remove --force`, deleting uncommitted
    /// work and reporting success even when removal failed.
    #[tokio::test]
    async fn exit_keeps_uncommitted_work_unless_told_to_discard_it() {
        let (_outer, root) = repo();
        let state = new_worktree_state();
        let wt = enter(&state, &root).await;
        std::fs::write(wt.join("work.rs"), "fn main() {}").unwrap();
        let exit = ExitWorktreeTool {
            state: state.clone(),
        };
        let ctx = ToolContext::new(root.clone());

        let out = exit.execute(json!({}), &ctx).await.unwrap();
        assert!(out.is_error, "a dirty worktree must not be removed");
        assert!(wt.join("work.rs").exists(), "uncommitted work was deleted");
        assert!(state.lock().unwrap().is_some(), "session must stay open");

        let out = exit
            .execute(json!({"discard_changes": true}), &ctx)
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(!wt.exists());
        assert!(state.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn a_clean_worktree_exits_without_force() {
        let (_outer, root) = repo();
        let state = new_worktree_state();
        let wt = enter(&state, &root).await;
        let out = ExitWorktreeTool {
            state: state.clone(),
        }
        .execute(json!({}), &ToolContext::new(root))
        .await
        .unwrap();
        assert!(!out.is_error);
        assert!(!wt.exists());
        assert!(state.lock().unwrap().is_none());
    }
}
