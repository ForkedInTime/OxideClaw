use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::sync::atomic::AtomicU32;
use std::sync::Arc;
use tokio::sync::mpsc;

use crate::browser::approval_gate::{ApprovalGate, ApprovalGateMiddleware, ApprovalPrompt};
use crate::browser::loop_detector::LoopDetectorMiddleware;
use crate::config::Config;
use crate::query_engine::QueryEngine;
use crate::tools::DynTool;

/// Policy for the approval gate during this run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BrowsePolicy {
    Pattern,
    Ask,
    Yolo,
}

impl Default for BrowsePolicy {
    fn default() -> Self { Self::Pattern }
}

/// A single browse-run configuration.
#[derive(Debug, Clone)]
pub struct BrowseRequest {
    pub goal: String,
    pub policy: BrowsePolicy,
    pub max_steps: u32,
    pub voice: bool,
}

/// Progress events streamed to the caller during a run.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BrowseProgress {
    Started { goal: String, max_steps: u32 },
    Step { n: u32, action: String, target: String },
    Nudge { level: u8, text: String },
    ApprovalNeeded { step: u32, action: String, target_text: String, url: String, reason: String },
    Completed(BrowseResult),
}

/// Final result of a browse run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrowseResult {
    pub achieved: bool,
    pub summary: String,
    pub reason: BrowseReason,
    pub steps_used: u32,
    pub final_url: Option<String>,
}

/// Why the loop terminated.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BrowseReason {
    Done,
    Bailed,
    StepCap,
    Stagnation,
    Budget,
    BrowserCrashed,
    UserDenied,
    Cancelled,
}

/// Build the browse-agent system prompt.
fn build_browse_system_prompt(goal: &str, max_steps: u32) -> String {
    format!(
        "You are an autonomous browser agent.\n\
         \n\
         Goal: {goal}\n\
         \n\
         Instructions:\n\
         - Use the browser_* tools to navigate, inspect, and act on web pages.\n\
         - Take exactly one action per turn. Observe the result before planning the next step.\n\
         - When you believe the goal is achieved (or you're stuck), call browse_done(summary, achieved).\n\
         - Keep summaries under 2 sentences — they may be spoken aloud.\n\
         - You have {max_steps} steps total.\n\
         - If approval is denied, try a different approach or call browse_done(achieved=false)."
    )
}

/// Filter the tool list down to browser_* + browse_done tools.
fn filter_browser_tools(tools: &[DynTool]) -> Vec<DynTool> {
    tools
        .iter()
        .filter(|t| {
            let name = t.name();
            name.starts_with("browser_") || name == "browse_done"
        })
        .cloned()
        .collect()
}

/// Parse the BROWSE_DONE sentinel from assistant text.
/// Returns (achieved, summary) if the sentinel is found.
fn parse_browse_done(text: &str) -> Option<(bool, String)> {
    if !text.contains("BROWSE_DONE") {
        return None;
    }
    let achieved = text.contains("achieved=true");
    let summary = text
        .find("summary=")
        .map(|i| text[i + 8..].trim().to_string())
        .unwrap_or_default();
    Some((achieved, summary))
}

/// Orchestrate an autonomous browser agent run.
pub async fn run_browse(
    req: BrowseRequest,
    config: &Config,
    tools: Vec<DynTool>,
    current_url: Arc<tokio::sync::Mutex<String>>,
    progress_tx: mpsc::Sender<BrowseProgress>,
    approval_tx: mpsc::Sender<ApprovalPrompt>,
) -> Result<BrowseResult> {
    // 1. Emit Started event.
    let _ = progress_tx
        .send(BrowseProgress::Started {
            goal: req.goal.clone(),
            max_steps: req.max_steps,
        })
        .await;

    // 2. Shared step counter for the approval prompt.
    let step_counter = Arc::new(AtomicU32::new(0));

    // 3. Build the approval gate from config patterns.
    let gate = ApprovalGate::with_user_patterns(config.browse_approval_patterns.clone());
    let gate_mw = Arc::new(ApprovalGateMiddleware::new(
        gate,
        req.policy,
        current_url.clone(),
        approval_tx,
        step_counter.clone(),
    ));

    // 4. Build the loop detector middleware.
    let (nudge_tx, mut nudge_rx) = mpsc::channel::<String>(16);
    let loop_mw = Arc::new(LoopDetectorMiddleware::new(nudge_tx));

    // 5. Assemble middleware chain (keep Arc refs for post-run inspection).
    let middlewares: crate::browser::middleware::MiddlewareChain = vec![
        gate_mw.clone() as Arc<dyn crate::browser::middleware::ToolMiddleware>,
        loop_mw.clone() as Arc<dyn crate::browser::middleware::ToolMiddleware>,
    ];

    // 6. Build browse-specific system prompt.
    let system_prompt = build_browse_system_prompt(&req.goal, req.max_steps);

    // 7. Filter tools to browser_* + browse_done only.
    let browser_tools = filter_browser_tools(&tools);

    // 8. Override config's max_turns to the browse step cap.
    let mut browse_config = config.clone();
    browse_config.max_turns = req.max_steps;

    // 9. Create the browse-mode query engine.
    let mut engine =
        QueryEngine::new_for_browse(browse_config, browser_tools, system_prompt, middlewares)?;

    // 10. Spawn a task to forward nudges as BrowseProgress events.
    let progress_tx_nudge = progress_tx.clone();
    let step_counter_nudge = step_counter.clone();
    let nudge_handle = tokio::spawn(async move {
        let mut level: u8 = 0;
        while let Some(text) = nudge_rx.recv().await {
            level = level.saturating_add(1);
            // Update step counter from the nudge level for progress reporting.
            let _ = step_counter_nudge.load(std::sync::atomic::Ordering::Relaxed);
            let _ = progress_tx_nudge
                .send(BrowseProgress::Nudge { level, text })
                .await;
        }
    });

    // 11. Run the agentic loop.
    let query_result = engine.query(&req.goal).await;

    // 12. Shut down the nudge forwarder.
    nudge_handle.abort();

    // 13. Determine the result.
    let steps_used = engine.turns_used();
    let final_url = {
        let url = current_url.lock().await;
        if url.is_empty() { None } else { Some(url.clone()) }
    };

    // Check middleware termination flags first — they override sentinel parsing.
    let middleware_reason = if loop_mw.is_stopped() {
        Some(BrowseReason::Stagnation)
    } else if gate_mw.is_user_denied() {
        Some(BrowseReason::UserDenied)
    } else {
        None
    };

    let result = match query_result {
        Ok(()) => {
            // Bug 1 fix: BROWSE_DONE sentinel is emitted by BrowseDoneTool::execute()
            // which returns it as a tool result (user-role ContentBlock::ToolResult),
            // NOT as assistant text. Check tool results first, then assistant text as fallback.
            let sentinel_text = engine
                .last_tool_result_text()
                .and_then(|t| parse_browse_done(&t).map(|r| (t, r)))
                .or_else(|| {
                    engine
                        .last_assistant_text()
                        .and_then(|t| parse_browse_done(&t).map(|r| (t, r)))
                });

            if let Some((_raw, (achieved, summary))) = sentinel_text {
                let mw_active = middleware_reason.is_some();
                let reason = middleware_reason.unwrap_or(if achieved {
                    BrowseReason::Done
                } else {
                    BrowseReason::Bailed
                });
                BrowseResult {
                    achieved: achieved && !mw_active,
                    summary,
                    reason,
                    steps_used,
                    final_url,
                }
            } else if let Some(reason) = middleware_reason {
                // Middleware stopped the loop but no sentinel was found.
                BrowseResult {
                    achieved: false,
                    summary: match reason {
                        BrowseReason::Stagnation => {
                            "Agent terminated: repeated same action with no progress".to_string()
                        }
                        BrowseReason::UserDenied => {
                            "Agent terminated: user denied the action twice".to_string()
                        }
                        _ => "Agent terminated by middleware".to_string(),
                    },
                    reason,
                    steps_used,
                    final_url,
                }
            } else if let Some(text) = engine.last_assistant_text() {
                // No sentinel, no middleware stop — engine stopped for other reasons.
                let reason = if steps_used >= req.max_steps {
                    BrowseReason::StepCap
                } else {
                    BrowseReason::Done
                };
                BrowseResult {
                    achieved: false,
                    summary: text.chars().take(200).collect(),
                    reason,
                    steps_used,
                    final_url,
                }
            } else {
                // No assistant messages at all.
                BrowseResult {
                    achieved: false,
                    summary: "No response from browse agent".to_string(),
                    reason: BrowseReason::Bailed,
                    steps_used,
                    final_url,
                }
            }
        }
        Err(e) => {
            let msg = e.to_string();
            let reason = middleware_reason.unwrap_or_else(|| {
                if msg.contains("budget") || msg.contains("Budget") {
                    BrowseReason::Budget
                } else if msg.contains("browser") || msg.contains("CDP") || msg.contains("Chrome")
                {
                    BrowseReason::BrowserCrashed
                } else {
                    BrowseReason::Bailed
                }
            });
            BrowseResult {
                achieved: false,
                summary: format!("Browse agent error: {msg}"),
                reason,
                steps_used,
                final_url,
            }
        }
    };

    // 14. Emit Completed event.
    let _ = progress_tx
        .send(BrowseProgress::Completed(result.clone()))
        .await;

    Ok(result)
}
