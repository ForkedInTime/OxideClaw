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
        if self.state.lock().unwrap_or_else(|e| e.into_inner()).is_some() {
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
                &slug
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

#[async_trait]
impl Tool for ExitWorktreeTool {
    fn name(&self) -> &str {
        "ExitWorktree"
    }

    fn description(&self) -> &str {
        "Exit the current git worktree and return to the original working directory. \
        The worktree branch is preserved; the worktree directory is removed."
    }

    fn input_schema(&self) -> serde_json::Value {
        json!({ "type": "object", "properties": {} })
    }

    async fn execute(&self, _input: serde_json::Value, _ctx: &ToolContext) -> Result<ToolOutput> {
        let session = self.state.lock().unwrap_or_else(|e| e.into_inner()).take();

        match session {
            None => Ok(ToolOutput::error("Not currently in a worktree session.")),
            Some(s) => {
                // Remove the worktree
                let output = Command::new("git")
                    .args([
                        "worktree",
                        "remove",
                        "--force",
                        s.path.to_str().unwrap_or(""),
                    ])
                    .current_dir(&s.original_cwd)
                    .output()
                    .await?;

                let msg = if output.status.success() {
                    format!(
                        "Exited worktree '{}'. Returned to {}. Branch '{}' preserved.",
                        s.path.display(),
                        s.original_cwd.display(),
                        s.branch,
                    )
                } else {
                    let err = String::from_utf8_lossy(&output.stderr);
                    format!("Worktree removed (with warnings): {err}")
                };

                Ok(ToolOutput::success(msg))
            }
        }
    }
}
