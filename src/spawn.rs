/// Spawn — background parallel agents in git worktrees.
///
/// `/spawn "refactor auth"` creates a git worktree, launches a background
/// QueryEngine in it, and reports back when done.  The user keeps working
/// in the main TUI while the spawned agent operates in isolation.
use crate::config::Config;
use crate::query_engine::QueryEngine;
use crate::tools::default_tools;
use crate::tui::events::AppEvent;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::process::Command;
use tokio::sync::mpsc;
use uuid::Uuid;

// ── Types ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SpawnStatus {
    Running,
    Completed,
    Failed,
    Cancelled,
}

/// Note: not Clone because of cancel_tx (oneshot::Sender is not Clone).
#[derive(Debug)]
#[allow(dead_code)] // fields populated at spawn time, read by merge/review handlers
pub struct SpawnedAgent {
    pub id: String,
    pub description: String,
    pub status: SpawnStatus,
    pub worktree_path: PathBuf,
    pub branch: String,
    pub original_cwd: PathBuf,
    /// Final summary text returned by the agent (set on completion).
    pub summary: Option<String>,
    /// The diff between the worktree branch and HEAD at spawn time.
    pub diff: Option<String>,
    /// Error message if failed.
    pub error: Option<String>,
    /// Cancel signal — take and send () to request cancellation.
    pub cancel_tx: Option<tokio::sync::oneshot::Sender<()>>,
}

pub type SpawnRegistry = Arc<Mutex<HashMap<String, SpawnedAgent>>>;

pub fn new_registry() -> SpawnRegistry {
    Arc::new(Mutex::new(HashMap::new()))
}

// ── Spawn ────────────────────────────────────────────────────────────────────

/// Create a git worktree, launch a background agent in it, return the agent id.
///
/// The agent runs with a default tool set (Bash, Read, Write, Edit, Glob, Grep,
/// WebFetch) — no MCP, no recursive Agent spawning, no worktree tools.
pub async fn spawn_agent(
    description: String,
    config: &Config,
    registry: &SpawnRegistry,
    event_tx: mpsc::UnboundedSender<AppEvent>,
) -> Result<String> {
    let cwd = config.cwd.clone();

    // Validate git repo
    let git_check = Command::new("git")
        .args(["rev-parse", "--git-dir"])
        .current_dir(&cwd)
        .output()
        .await?;
    if !git_check.status.success() {
        anyhow::bail!("Not inside a git repository — cannot spawn a worktree agent.");
    }

    // Derive a slug from the description
    let slug: String = description
        .chars()
        .take(30)
        .map(|c| {
            if c.is_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_string();
    let slug = if slug.is_empty() {
        format!("spawn-{}", &Uuid::new_v4().to_string()[..8])
    } else {
        format!("spawn-{slug}")
    };

    // Find git root
    let git_root = String::from_utf8_lossy(
        &Command::new("git")
            .args(["rev-parse", "--show-toplevel"])
            .current_dir(&cwd)
            .output()
            .await?
            .stdout,
    )
    .trim()
    .to_string();

    let repo_name = git_root.split('/').next_back().unwrap_or("repo");
    let worktree_path = PathBuf::from(&git_root)
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(std::env::temp_dir)
        .join(format!("{repo_name}-{slug}"));

    // Create worktree + branch
    let output = Command::new("git")
        .args([
            "worktree",
            "add",
            "-b",
            &slug,
            worktree_path.to_str().unwrap_or("/tmp/spawn-wt"),
            "HEAD",
        ])
        .current_dir(&cwd)
        .output()
        .await?;

    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git worktree add failed: {err}");
    }

    // Capture the base commit for diffing later
    let base_sha = String::from_utf8_lossy(
        &Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&worktree_path)
            .output()
            .await?
            .stdout,
    )
    .trim()
    .to_string();

    let id = Uuid::new_v4().to_string()[..8].to_string();
    let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel::<()>();

    let agent = SpawnedAgent {
        id: id.clone(),
        description: description.clone(),
        status: SpawnStatus::Running,
        worktree_path: worktree_path.clone(),
        branch: slug.clone(),
        original_cwd: cwd.clone(),
        summary: None,
        diff: None,
        error: None,
        cancel_tx: Some(cancel_tx),
    };

    registry.lock().unwrap_or_else(|e| e.into_inner()).insert(id.clone(), agent);

    // Build config for the spawned agent
    let mut agent_config = config.clone();
    agent_config.cwd = worktree_path.clone();

    // System prompt addition for the spawned agent
    let spawn_context = format!(
        "\n\n# Spawned Agent Context\n\
        You are a background agent running in an isolated git worktree.\n\
        - Worktree: {}\n\
        - Branch: {}\n\
        - Task: {}\n\n\
        Complete the task thoroughly. When done, provide a brief summary of what you changed.\n\
        You are working in an isolated copy — make all changes you need without hesitation.\n\
        Do NOT push, create PRs, or interact with remotes. Just make the changes locally.",
        worktree_path.display(),
        slug,
        description,
    );
    agent_config.append_system_prompt =
        Some(agent_config.append_system_prompt.unwrap_or_default() + &spawn_context);

    // Launch background task
    let reg = registry.clone();
    let agent_id = id.clone();
    let desc = description.clone();
    let wt_path = worktree_path.clone();
    let orig_cwd = cwd.clone();

    tokio::spawn(async move {
        let result = run_spawned_agent(agent_config, &desc, cancel_rx).await;

        // Collect the diff (committed + uncommitted changes since base)
        let diff = Command::new("git")
            .args(["diff", &base_sha])
            .current_dir(&wt_path)
            .output()
            .await
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
            .unwrap_or_default();

        // Also get a stat summary
        let stat = Command::new("git")
            .args(["diff", "--stat", &base_sha])
            .current_dir(&wt_path)
            .output()
            .await
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
            .unwrap_or_default();

        match result {
            Ok(summary) => {
                if let Ok(mut reg) = reg.lock()
                    && let Some(agent) = reg.get_mut(&agent_id)
                {
                    agent.status = SpawnStatus::Completed;
                    agent.summary = Some(summary.clone());
                    agent.diff = Some(diff);
                }
                let _ = event_tx.send(AppEvent::SystemMessage(format!(
                    "🏁 Agent [{agent_id}] completed: {desc}\n{stat}\nUse /review {agent_id} to inspect changes, /merge {agent_id} to apply them.",
                )));
            }
            Err(e) => {
                let err_msg = format!("{e:#}");
                if let Ok(mut reg) = reg.lock()
                    && let Some(agent) = reg.get_mut(&agent_id)
                {
                    agent.status = SpawnStatus::Failed;
                    agent.error = Some(err_msg.clone());
                }
                let _ = event_tx.send(AppEvent::SystemMessage(format!(
                    "❌ Agent [{agent_id}] failed: {desc}\n{err_msg}",
                )));
            }
        }

        // Clean up worktree on failure/cancel (keep on success for review)
        let status = reg
            .lock()
            .ok()
            .and_then(|r| r.get(&agent_id).map(|a| a.status.clone()));
        if matches!(status, Some(SpawnStatus::Failed | SpawnStatus::Cancelled)) {
            let _ = Command::new("git")
                .args([
                    "worktree",
                    "remove",
                    "--force",
                    wt_path.to_str().unwrap_or(""),
                ])
                .current_dir(&orig_cwd)
                .output()
                .await;
        }
    });

    Ok(id)
}

/// Run the actual agent loop. Returns the final summary text.
async fn run_spawned_agent(
    config: Config,
    task: &str,
    mut cancel_rx: tokio::sync::oneshot::Receiver<()>,
) -> Result<String> {
    let tools = default_tools();
    let mut engine = QueryEngine::new(config, tools)?;

    // Race the agent against the cancel signal
    tokio::select! {
        result = engine.query_and_collect(task) => {
            match result {
                Ok(output) => {
                    // Extract text from ToolOutput content
                    let text = output.content.iter()
                        .map(|c| {
                            let crate::api::types::ToolResultContent::Text { text } = c;
                            text.as_str()
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    Ok(text)
                }
                Err(e) => Err(e),
            }
        }
        _ = &mut cancel_rx => {
            anyhow::bail!("Agent cancelled by user.");
        }
    }
}

// ── Agent management helpers ─────────────────────────────────────────────────

/// List all agents with their status.
pub fn list_agents(registry: &SpawnRegistry) -> String {
    let reg = registry.lock().unwrap_or_else(|e| e.into_inner());
    if reg.is_empty() {
        return "No spawned agents.".to_string();
    }

    let mut lines = Vec::new();
    let mut agents: Vec<&SpawnedAgent> = reg.values().collect();
    agents.sort_by_key(|a| &a.id);

    for a in agents {
        let status = match &a.status {
            SpawnStatus::Running => "⚡ running",
            SpawnStatus::Completed => "✅ done",
            SpawnStatus::Failed => "❌ failed",
            SpawnStatus::Cancelled => "⛔ cancelled",
        };
        lines.push(format!(
            "[{}] {} — {} (branch: {})",
            a.id, status, a.description, a.branch,
        ));
    }
    lines.join("\n")
}

/// Get the diff for a completed agent.
pub fn review_agent(registry: &SpawnRegistry, id: &str) -> Result<String> {
    let reg = registry.lock().unwrap_or_else(|e| e.into_inner());
    let agent = find_agent(&reg, id)?;

    match &agent.status {
        SpawnStatus::Running => anyhow::bail!("Agent [{}] is still running.", agent.id),
        SpawnStatus::Cancelled => anyhow::bail!("Agent [{}] was cancelled.", agent.id),
        _ => {}
    }

    let mut output = format!("# Agent [{}]: {}\n\n", agent.id, agent.description);

    if let Some(ref summary) = agent.summary {
        output.push_str("## Summary\n");
        output.push_str(summary);
        output.push_str("\n\n");
    }

    if let Some(ref err) = agent.error {
        output.push_str("## Error\n");
        output.push_str(err);
        output.push_str("\n\n");
    }

    if let Some(ref diff) = agent.diff {
        if !diff.is_empty() {
            output.push_str("## Diff\n```diff\n");
            // Limit diff to first 200 lines to avoid flooding
            let truncated: String = diff.lines().take(200).collect::<Vec<_>>().join("\n");
            output.push_str(&truncated);
            if diff.lines().count() > 200 {
                output.push_str("\n... (truncated, use `git diff` in worktree for full diff)");
            }
            output.push_str("\n```\n");
        } else {
            output.push_str("No changes made.\n");
        }
    }

    Ok(output)
}

/// Cancel a running agent.
pub fn kill_agent(registry: &SpawnRegistry, id: &str) -> Result<String> {
    let mut reg = registry.lock().unwrap_or_else(|e| e.into_inner());
    let agent = find_agent_mut(&mut reg, id)?;

    if agent.status != SpawnStatus::Running {
        anyhow::bail!(
            "Agent [{}] is not running (status: {:?}).",
            agent.id,
            agent.status
        );
    }

    // Send cancel signal
    if let Some(tx) = agent.cancel_tx.take() {
        let _ = tx.send(());
    }
    agent.status = SpawnStatus::Cancelled;

    Ok(format!(
        "Agent [{}] cancelled: {}",
        agent.id, agent.description
    ))
}

/// Merge a completed agent's worktree changes into the current branch.
pub async fn merge_agent(registry: &SpawnRegistry, id: &str, main_cwd: &PathBuf) -> Result<String> {
    let (branch, wt_path) = {
        let reg = registry.lock().unwrap_or_else(|e| e.into_inner());
        let agent = find_agent(&reg, id)?;
        if agent.status != SpawnStatus::Completed {
            anyhow::bail!(
                "Agent [{}] is not completed (status: {:?}). Only completed agents can be merged.",
                agent.id,
                agent.status
            );
        }
        (agent.branch.clone(), agent.worktree_path.clone())
    };

    // Commit all changes in the worktree first
    let status = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(&wt_path)
        .output()
        .await?;
    let has_changes = !status.stdout.is_empty();

    if has_changes {
        let _ = Command::new("git")
            .args(["add", "-A"])
            .current_dir(&wt_path)
            .output()
            .await?;

        let commit_msg = format!("spawn: {}", {
            let reg = registry.lock().unwrap_or_else(|e| e.into_inner());
            reg.get(id)
                .map(|a| a.description.clone())
                .unwrap_or_default()
        });

        let commit = Command::new("git")
            .args(["commit", "-m", &commit_msg])
            .current_dir(&wt_path)
            .output()
            .await?;

        if !commit.status.success() {
            let err = String::from_utf8_lossy(&commit.stderr);
            anyhow::bail!("Failed to commit agent changes: {err}");
        }
    }

    // Merge the worktree branch into the current branch
    let merge = Command::new("git")
        .args(["merge", &branch, "--no-edit"])
        .current_dir(main_cwd)
        .output()
        .await?;

    // Clean up worktree
    let _ = Command::new("git")
        .args([
            "worktree",
            "remove",
            "--force",
            wt_path.to_str().unwrap_or(""),
        ])
        .current_dir(main_cwd)
        .output()
        .await;

    // Delete the branch
    let _ = Command::new("git")
        .args(["branch", "-d", &branch])
        .current_dir(main_cwd)
        .output()
        .await;

    // Update registry
    if let Ok(mut reg) = registry.lock() {
        reg.remove(id);
    }

    if merge.status.success() {
        Ok(format!(
            "Merged branch '{branch}' into current branch. Worktree cleaned up."
        ))
    } else {
        let err = String::from_utf8_lossy(&merge.stderr);
        anyhow::bail!("Merge failed (resolve manually): {err}")
    }
}

/// Discard a completed/failed agent's worktree without merging.
pub async fn discard_agent(
    registry: &SpawnRegistry,
    id: &str,
    main_cwd: &PathBuf,
) -> Result<String> {
    let (branch, wt_path, desc) = {
        let reg = registry.lock().unwrap_or_else(|e| e.into_inner());
        let agent = find_agent(&reg, id)?;
        if agent.status == SpawnStatus::Running {
            anyhow::bail!(
                "Agent [{}] is still running. Use /kill {} first.",
                agent.id,
                agent.id
            );
        }
        (
            agent.branch.clone(),
            agent.worktree_path.clone(),
            agent.description.clone(),
        )
    };

    // Remove worktree
    let _ = Command::new("git")
        .args([
            "worktree",
            "remove",
            "--force",
            wt_path.to_str().unwrap_or(""),
        ])
        .current_dir(main_cwd)
        .output()
        .await;

    // Delete the branch
    let _ = Command::new("git")
        .args(["branch", "-D", &branch])
        .current_dir(main_cwd)
        .output()
        .await;

    // Remove from registry
    if let Ok(mut reg) = registry.lock() {
        reg.remove(id);
    }

    Ok(format!(
        "Discarded agent [{id}]: {desc}. Worktree and branch removed."
    ))
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn find_agent<'a>(reg: &'a HashMap<String, SpawnedAgent>, id: &str) -> Result<&'a SpawnedAgent> {
    // Support prefix matching
    let matches: Vec<&SpawnedAgent> = reg.values().filter(|a| a.id.starts_with(id)).collect();

    match matches.len() {
        0 => anyhow::bail!("No agent found matching '{id}'. Use /agents to list."),
        1 => Ok(matches[0]),
        _ => anyhow::bail!(
            "Ambiguous id '{id}' — matches {} agents. Be more specific.",
            matches.len()
        ),
    }
}

fn find_agent_mut<'a>(
    reg: &'a mut HashMap<String, SpawnedAgent>,
    id: &str,
) -> Result<&'a mut SpawnedAgent> {
    let matching_ids: Vec<String> = reg.keys().filter(|k| k.starts_with(id)).cloned().collect();

    match matching_ids.len() {
        0 => anyhow::bail!("No agent found matching '{id}'. Use /agents to list."),
        1 => {
            // The key came from reg.keys() a moment ago and the registry is
            // held under the caller's lock, so a get_mut miss is unreachable
            // in practice — but surface it as an error instead of panicking
            // to avoid crashing /agents on a concurrent-modification bug.
            let only = &matching_ids[0];
            reg.get_mut(only).ok_or_else(|| {
                anyhow::anyhow!("agent '{only}' vanished from registry between lookup and get")
            })
        }
        _ => anyhow::bail!(
            "Ambiguous id '{id}' — matches {} agents. Be more specific.",
            matching_ids.len()
        ),
    }
}
