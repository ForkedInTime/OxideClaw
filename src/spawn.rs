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

/// Running agents at once. Each is a full engine billing tokens in parallel.
pub const MAX_CONCURRENT_SPAWNS: usize = 8;

/// Refuse a new spawn while `MAX_CONCURRENT_SPAWNS` are still running.
fn check_capacity(registry: &SpawnRegistry) -> Result<()> {
    let running = registry
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .values()
        .filter(|a| a.status == SpawnStatus::Running)
        .count();
    if running >= MAX_CONCURRENT_SPAWNS {
        anyhow::bail!(
            "{running} agents are already running (limit {MAX_CONCURRENT_SPAWNS}). \
             Wait for one to finish or /kill one first."
        );
    }
    Ok(())
}

/// Branch/worktree name: `spawn-<slug>-<id>`. The id keeps two spawns with
/// the same description (or a re-run after a kept worktree) from colliding
/// on `git worktree add -b`.
fn spawn_slug(description: &str, id: &str) -> String {
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
    if slug.is_empty() {
        format!("spawn-{id}")
    } else {
        format!("spawn-{slug}-{id}")
    }
}

/// Record the task's outcome. A `Cancelled` status set by `/kill` is kept —
/// the task's own error ("cancelled by user") must not relabel it `Failed`.
/// Returns the status the agent ended in.
fn finalize(
    registry: &SpawnRegistry,
    id: &str,
    result: &Result<String>,
    diff: String,
) -> SpawnStatus {
    let mut reg = registry.lock().unwrap_or_else(|e| e.into_inner());
    let Some(agent) = reg.get_mut(id) else {
        return SpawnStatus::Failed;
    };
    if agent.status == SpawnStatus::Cancelled {
        return SpawnStatus::Cancelled;
    }
    match result {
        Ok(summary) => {
            agent.status = SpawnStatus::Completed;
            agent.summary = Some(summary.clone());
            agent.diff = Some(diff);
        }
        Err(e) => {
            agent.status = SpawnStatus::Failed;
            agent.error = Some(format!("{e:#}"));
        }
    }
    agent.status.clone()
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
    check_capacity(registry)?;

    // Validate git repo
    let git_check = Command::new("git")
        .args(["rev-parse", "--git-dir"])
        .current_dir(&cwd)
        .output()
        .await?;
    if !git_check.status.success() {
        anyhow::bail!("Not inside a git repository — cannot spawn a worktree agent.");
    }

    let id = Uuid::new_v4().to_string()[..8].to_string();
    let slug = spawn_slug(&description, &id);

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
        .args(["worktree", "add", "-b", &slug])
        .arg(&worktree_path)
        .arg("HEAD")
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

    registry
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(id.clone(), agent);

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

        let status = finalize(&reg, &agent_id, &result, diff);
        match (&status, &result) {
            (SpawnStatus::Completed, _) => {
                let _ = event_tx.send(AppEvent::SystemMessage(format!(
                    "🏁 Agent [{agent_id}] completed: {desc}\n{stat}\nUse /review {agent_id} to inspect changes, /merge {agent_id} to apply them.",
                )));
            }
            (SpawnStatus::Cancelled, _) => {
                let _ = event_tx.send(AppEvent::SystemMessage(format!(
                    "⛔ Agent [{agent_id}] cancelled: {desc}"
                )));
            }
            (_, Err(e)) => {
                let _ = event_tx.send(AppEvent::SystemMessage(format!(
                    "❌ Agent [{agent_id}] failed: {desc}\n{e:#}",
                )));
            }
            (_, Ok(_)) => {}
        }

        // Clean up worktree on failure/cancel (keep on success for review)
        if matches!(status, SpawnStatus::Failed | SpawnStatus::Cancelled) {
            let _ = Command::new("git")
                .args(["worktree", "remove", "--force"])
                .arg(&wt_path)
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
    let tools = default_tools(crate::net_policy::NetPolicy::from_config(&config));
    // The user asked for an autonomous background agent: no prompts. Settings
    // deny rules still hold (PermissionState checks them before bypass).
    let mut engine = QueryEngine::new(config, tools)?
        .with_permission_gate(crate::permissions::PermissionGate::bypass());

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

/// TUI shutdown: cancel every running agent and remove its worktree and
/// branch — an in-flight agent's half-done tree is worthless once the
/// registry that tracks it is gone. Completed (unmerged) work is kept and
/// listed so the user can merge it by hand. Returns the message to show.
pub async fn cleanup_on_exit(registry: &SpawnRegistry, main_cwd: &PathBuf) -> Option<String> {
    // Take what we need out of the lock; the git calls below must not hold it.
    let (to_remove, kept) = {
        let mut reg = registry.lock().unwrap_or_else(|e| e.into_inner());
        let mut to_remove = Vec::new();
        let mut kept = Vec::new();
        for agent in reg.values_mut() {
            match agent.status {
                SpawnStatus::Completed => {
                    kept.push((agent.branch.clone(), agent.worktree_path.clone()));
                }
                SpawnStatus::Running | SpawnStatus::Cancelled | SpawnStatus::Failed => {
                    if let Some(tx) = agent.cancel_tx.take() {
                        let _ = tx.send(());
                    }
                    agent.status = SpawnStatus::Cancelled;
                    to_remove.push((agent.branch.clone(), agent.worktree_path.clone()));
                }
            }
        }
        (to_remove, kept)
    };
    if to_remove.is_empty() && kept.is_empty() {
        return None;
    }

    for (branch, path) in &to_remove {
        let _ = Command::new("git")
            .args(["worktree", "remove", "--force"])
            .arg(path)
            .current_dir(main_cwd)
            .output()
            .await;
        let _ = Command::new("git")
            .args(["branch", "-D", branch])
            .current_dir(main_cwd)
            .output()
            .await;
    }

    let mut lines = Vec::new();
    if !to_remove.is_empty() {
        lines.push(format!(
            "Cancelled {} running spawn agent(s) and removed their worktrees.",
            to_remove.len()
        ));
    }
    if !kept.is_empty() {
        lines.push("Completed spawn work kept for manual merge:".to_string());
        for (branch, path) in &kept {
            lines.push(format!(
                "  {branch}  →  {}  (git merge {branch})",
                path.display()
            ));
        }
    }
    Some(lines.join("\n"))
}

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

    if !merge.status.success() {
        // Leave the user's checkout as it was, and keep the worktree, branch
        // and registry entry so the work can be reviewed and merged by hand.
        let _ = Command::new("git")
            .args(["merge", "--abort"])
            .current_dir(main_cwd)
            .output()
            .await;
        let err = String::from_utf8_lossy(&merge.stderr);
        let out = String::from_utf8_lossy(&merge.stdout);
        anyhow::bail!(
            "Merge failed and was aborted; the worktree and branch '{branch}' are kept. \
             Resolve by hand (git merge {branch}) or /discard {id}.\n{out}{err}"
        );
    }

    // Clean up worktree
    let _ = Command::new("git")
        .args(["worktree", "remove", "--force"])
        .arg(&wt_path)
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

    Ok(format!(
        "Merged branch '{branch}' into current branch. Worktree cleaned up."
    ))
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
        .args(["worktree", "remove", "--force"])
        .arg(&wt_path)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, status: SpawnStatus) -> SpawnedAgent {
        SpawnedAgent {
            id: id.into(),
            description: "d".into(),
            status,
            worktree_path: PathBuf::from("/nonexistent"),
            branch: format!("spawn-{id}"),
            original_cwd: PathBuf::from("/nonexistent"),
            summary: None,
            diff: None,
            error: None,
            cancel_tx: None,
        }
    }

    fn registry_with(entries: Vec<SpawnedAgent>) -> SpawnRegistry {
        let reg = new_registry();
        let mut r = reg.lock().unwrap();
        for e in entries {
            r.insert(e.id.clone(), e);
        }
        drop(r);
        reg
    }

    #[test]
    fn slug_carries_the_id_so_repeat_descriptions_do_not_collide() {
        assert_ne!(
            spawn_slug("refactor auth", "a1b2"),
            spawn_slug("refactor auth", "c3d4")
        );
        assert!(spawn_slug("Refactor Auth!", "a1b2").starts_with("spawn-refactor-auth"));
        assert_eq!(spawn_slug("", "a1b2"), "spawn-a1b2");
        assert_eq!(spawn_slug("!!!", "a1b2"), "spawn-a1b2");
    }

    #[test]
    fn capacity_counts_only_running_agents() {
        let running: Vec<_> = (0..MAX_CONCURRENT_SPAWNS)
            .map(|i| entry(&format!("r{i}"), SpawnStatus::Running))
            .collect();
        assert!(check_capacity(&registry_with(running)).is_err());

        let mut mixed: Vec<_> = (0..MAX_CONCURRENT_SPAWNS - 1)
            .map(|i| entry(&format!("r{i}"), SpawnStatus::Running))
            .collect();
        mixed.push(entry("done", SpawnStatus::Completed));
        mixed.push(entry("dead", SpawnStatus::Failed));
        assert!(check_capacity(&registry_with(mixed)).is_ok());
    }

    #[test]
    fn a_cancelled_agent_stays_cancelled_when_its_task_errors_out() {
        let reg = registry_with(vec![entry("x", SpawnStatus::Cancelled)]);
        let r: Result<String> = Err(anyhow::anyhow!("Agent cancelled by user."));
        assert_eq!(
            finalize(&reg, "x", &r, String::new()),
            SpawnStatus::Cancelled
        );
        assert_eq!(reg.lock().unwrap()["x"].status, SpawnStatus::Cancelled);
    }

    #[test]
    fn running_agents_end_completed_or_failed() {
        let reg = registry_with(vec![
            entry("ok", SpawnStatus::Running),
            entry("bad", SpawnStatus::Running),
        ]);
        let good: Result<String> = Ok("did it".into());
        assert_eq!(
            finalize(&reg, "ok", &good, "+line".into()),
            SpawnStatus::Completed
        );
        let bad: Result<String> = Err(anyhow::anyhow!("boom"));
        assert_eq!(
            finalize(&reg, "bad", &bad, String::new()),
            SpawnStatus::Failed
        );
        let r = reg.lock().unwrap();
        assert_eq!(r["ok"].summary.as_deref(), Some("did it"));
        assert_eq!(r["ok"].diff.as_deref(), Some("+line"));
        assert!(r["bad"].error.as_deref().unwrap().contains("boom"));
    }

    async fn git(dir: &std::path::Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .await
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// Running agents are torn down with their worktrees; completed work
    /// survives and is named in the message.
    #[tokio::test]
    async fn exit_cleanup_removes_running_worktrees_and_keeps_completed_ones() {
        let tmp = tempfile::tempdir().unwrap();
        let main = tmp.path().join("repo");
        std::fs::create_dir(&main).unwrap();
        git(&main, &["init", "-q"]).await;
        git(&main, &["config", "user.email", "t@t"]).await;
        git(&main, &["config", "user.name", "t"]).await;
        git(&main, &["config", "commit.gpgsign", "false"]).await;
        // Windows CI: autocrlf would rewrite "user\n" as "user\r\n" on restore.
        git(&main, &["config", "core.autocrlf", "false"]).await;
        std::fs::write(main.join("a.txt"), "base\n").unwrap();
        git(&main, &["add", "-A"]).await;
        git(&main, &["commit", "-q", "-m", "base"]).await;

        let wt_run = tmp.path().join("repo-spawn-run");
        let wt_done = tmp.path().join("repo-spawn-done");
        for (wt, br) in [(&wt_run, "spawn-run"), (&wt_done, "spawn-done")] {
            git(
                &main,
                &[
                    "worktree",
                    "add",
                    "-q",
                    "-b",
                    br,
                    wt.to_str().unwrap(),
                    "HEAD",
                ],
            )
            .await;
        }

        let (cancel_tx, mut cancel_rx) = tokio::sync::oneshot::channel::<()>();
        let mut running = entry("run", SpawnStatus::Running);
        running.branch = "spawn-run".into();
        running.worktree_path = wt_run.clone();
        running.cancel_tx = Some(cancel_tx);
        let mut done = entry("done", SpawnStatus::Completed);
        done.branch = "spawn-done".into();
        done.worktree_path = wt_done.clone();
        let reg = registry_with(vec![running, done]);

        let msg = cleanup_on_exit(&reg, &main)
            .await
            .expect("something to report");

        assert!(
            cancel_rx.try_recv().is_ok(),
            "running agent must be cancelled"
        );
        assert!(!wt_run.exists(), "running agent's worktree must be removed");
        assert!(
            git(&main, &["branch", "--list", "spawn-run"])
                .await
                .is_empty()
        );
        assert!(wt_done.exists(), "completed work must survive");
        assert!(
            !git(&main, &["branch", "--list", "spawn-done"])
                .await
                .is_empty()
        );
        assert!(msg.contains("spawn-done"), "{msg}");
        assert!(msg.contains(wt_done.to_str().unwrap()), "{msg}");
    }

    #[tokio::test]
    async fn exit_cleanup_is_silent_with_no_agents() {
        let reg = new_registry();
        assert!(cleanup_on_exit(&reg, &PathBuf::from(".")).await.is_none());
    }

    /// A conflicting merge must not leave the user's checkout mid-merge, and
    /// must not throw away the worktree, branch and registry entry the user
    /// would need to resolve it by hand.
    #[tokio::test]
    async fn a_failed_merge_aborts_cleanly_and_keeps_the_agents_work() {
        let tmp = tempfile::tempdir().unwrap();
        let main = tmp.path().join("repo");
        std::fs::create_dir(&main).unwrap();
        git(&main, &["init", "-q"]).await;
        git(&main, &["config", "user.email", "t@t"]).await;
        git(&main, &["config", "user.name", "t"]).await;
        git(&main, &["config", "commit.gpgsign", "false"]).await;
        // Windows CI: autocrlf would rewrite "user\n" as "user\r\n" on restore.
        git(&main, &["config", "core.autocrlf", "false"]).await;
        std::fs::write(main.join("a.txt"), "base\n").unwrap();
        git(&main, &["add", "-A"]).await;
        git(&main, &["commit", "-q", "-m", "base"]).await;

        let wt = tmp.path().join("repo-spawn-x");
        git(
            &main,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "spawn-x",
                wt.to_str().unwrap(),
                "HEAD",
            ],
        )
        .await;
        std::fs::write(wt.join("a.txt"), "agent\n").unwrap();
        std::fs::write(main.join("a.txt"), "user\n").unwrap();
        git(&main, &["commit", "-q", "-am", "user edit"]).await;

        let mut agent = entry("x", SpawnStatus::Completed);
        agent.branch = "spawn-x".into();
        agent.worktree_path = wt.clone();
        agent.original_cwd = main.clone();
        let reg = registry_with(vec![agent]);

        let err = merge_agent(&reg, "x", &main).await.unwrap_err();
        assert!(err.to_string().contains("Merge failed"), "{err}");

        let unmerged = git(&main, &["diff", "--name-only", "--diff-filter=U"]).await;
        assert!(unmerged.is_empty(), "main is still mid-merge: {unmerged}");
        assert!(
            !main.join(".git/MERGE_HEAD").exists(),
            "merge was not aborted"
        );
        assert_eq!(
            std::fs::read_to_string(main.join("a.txt")).unwrap(),
            "user\n"
        );
        assert!(wt.exists(), "worktree was removed");
        assert!(
            !git(&main, &["branch", "--list", "spawn-x"])
                .await
                .is_empty()
        );
        assert!(
            reg.lock().unwrap().contains_key("x"),
            "registry entry dropped"
        );
    }
}
