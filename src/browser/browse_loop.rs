use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;

use crate::browser::approval_gate::{ApprovalGate, ApprovalGateMiddleware, ApprovalPrompt};
use crate::browser::loop_detector::LoopDetectorMiddleware;
use crate::browser::middleware::{MiddlewareVerdict, ToolMiddleware};
use crate::browser::BrowserSession;
use crate::config::Config;
use crate::query_engine::QueryEngine;
use crate::tools::DynTool;

/// Policy for the approval gate during this run.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BrowsePolicy {
    #[default]
    Pattern,
    Ask,
    Yolo,
}

impl BrowsePolicy {
    /// Parse a settings.json-style string ("pattern" / "ask" / "yolo").
    /// Unknown or empty strings fall back to `Pattern`.
    pub fn from_settings_str(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "ask" => Self::Ask,
            "yolo" => Self::Yolo,
            _ => Self::Pattern,
        }
    }
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

/// Extract a human-readable "target" from tool input for Step events.
/// Prefers `url` → `ref` → `selector` → `key` → empty string.
fn extract_target(input: &serde_json::Value) -> String {
    for key in ["url", "ref", "selector", "key"] {
        if let Some(s) = input.get(key).and_then(|v| v.as_str()) {
            if !s.is_empty() {
                return s.to_string();
            }
        }
    }
    String::new()
}

/// Channels and cancellation state the caller must plumb into a browse run.
pub struct BrowseChannels {
    pub progress_tx: mpsc::Sender<BrowseProgress>,
    pub approval_tx: mpsc::Sender<ApprovalPrompt>,
    pub cancel: Arc<AtomicBool>,
}

/// Middleware that syncs the browser session's `current_url` into the
/// shared `Arc<Mutex<String>>` that the approval gate reads from. Runs
/// in `after_tool` so URL-pattern matching sees the post-navigation URL.
struct UrlSyncMiddleware {
    session: Arc<tokio::sync::Mutex<BrowserSession>>,
    shared_url: Arc<tokio::sync::Mutex<String>>,
}

#[async_trait]
impl ToolMiddleware for UrlSyncMiddleware {
    async fn before_tool(&self, _tool_name: &str, _input: &serde_json::Value) -> MiddlewareVerdict {
        MiddlewareVerdict::Allow
    }

    async fn after_tool(&self, _tool_name: &str, _output: &str) {
        let url = self.session.lock().await.current_url.clone();
        *self.shared_url.lock().await = url;
    }
}

/// Middleware that emits `BrowseProgress::Step` for each allowed tool call.
/// Placed last in the chain so denied calls (by gate or loop detector) are not
/// reported as executed steps.
struct StepEmitterMiddleware {
    progress_tx: mpsc::Sender<BrowseProgress>,
    counter: Arc<AtomicU32>,
}

#[async_trait]
impl ToolMiddleware for StepEmitterMiddleware {
    async fn before_tool(&self, tool_name: &str, input: &serde_json::Value) -> MiddlewareVerdict {
        let n = self.counter.fetch_add(1, Ordering::Relaxed) + 1;
        let _ = self
            .progress_tx
            .send(BrowseProgress::Step {
                n,
                action: tool_name.to_string(),
                target: extract_target(input),
            })
            .await;
        MiddlewareVerdict::Allow
    }

    async fn after_tool(&self, _tool_name: &str, _output: &str) {}
}

/// Orchestrate an autonomous browser agent run.
pub async fn run_browse(
    req: BrowseRequest,
    config: &Config,
    tools: Vec<DynTool>,
    current_url: Arc<tokio::sync::Mutex<String>>,
    browser_session: Option<Arc<tokio::sync::Mutex<BrowserSession>>>,
    channels: BrowseChannels,
) -> Result<BrowseResult> {
    let BrowseChannels { progress_tx, approval_tx, cancel } = channels;
    // 1. Emit Started event + speak the goal if voice is enabled.
    let _ = progress_tx
        .send(BrowseProgress::Started {
            goal: req.goal.clone(),
            max_steps: req.max_steps,
        })
        .await;
    if req.voice {
        let goal = req.goal.clone();
        tokio::spawn(async move {
            crate::voice::speak_browse_milestone(
                crate::voice::BrowseMilestone::Start,
                &goal,
            )
            .await;
        });
    }

    // 2. Shared step counter for the approval prompt.
    let step_counter = Arc::new(AtomicU32::new(0));

    // 3. Build the approval gate from config patterns.
    // The gate sends prompts to an internal channel; a bridge task mirrors each
    // prompt as BrowseProgress::ApprovalNeeded, then forwards it to the caller.
    let gate = ApprovalGate::with_user_patterns(config.browse_approval_patterns.clone());
    let (internal_approval_tx, mut internal_approval_rx) = mpsc::channel::<ApprovalPrompt>(16);
    let gate_mw = Arc::new(
        ApprovalGateMiddleware::new(
            gate,
            req.policy,
            current_url.clone(),
            internal_approval_tx,
            step_counter.clone(),
            req.voice,
        )
        .with_browser_session(browser_session.clone()),
    );

    // Bridge: internal approvals → progress event + external approval channel.
    let approval_bridge_progress_tx = progress_tx.clone();
    let voice_on_gate = req.voice;
    let approval_bridge_handle = tokio::spawn(async move {
        while let Some(prompt) = internal_approval_rx.recv().await {
            let _ = approval_bridge_progress_tx
                .send(BrowseProgress::ApprovalNeeded {
                    step: prompt.step,
                    action: prompt.tool_name.clone(),
                    target_text: prompt.target_text.clone(),
                    url: prompt.url.clone(),
                    reason: prompt.reason.clone(),
                })
                .await;
            if voice_on_gate {
                let phrase = format!(
                    "Approval needed for {} — {}",
                    prompt.tool_name, prompt.reason
                );
                tokio::spawn(async move {
                    crate::voice::speak_browse_milestone(
                        crate::voice::BrowseMilestone::GateTrip,
                        &phrase,
                    )
                    .await;
                });
            }
            if approval_tx.send(prompt).await.is_err() {
                // Caller dropped the approval channel — stop bridging.
                break;
            }
        }
    });

    // 4. Build the loop detector middleware.
    let (nudge_tx, mut nudge_rx) = mpsc::channel::<String>(16);
    let loop_mw = Arc::new(LoopDetectorMiddleware::new(nudge_tx));

    // 5. Build the step-emitter middleware (runs last — only fires for allowed calls).
    let step_emitter = Arc::new(StepEmitterMiddleware {
        progress_tx: progress_tx.clone(),
        counter: step_counter.clone(),
    });

    // 6. Assemble middleware chain (keep Arc refs for post-run inspection).
    // Order matters: url_sync runs first so after_tool fires BEFORE any later
    // middleware reads the updated URL on the next iteration's before_tool.
    let mut middlewares: crate::browser::middleware::MiddlewareChain = Vec::new();
    if let Some(session) = browser_session.as_ref() {
        middlewares.push(Arc::new(UrlSyncMiddleware {
            session: session.clone(),
            shared_url: current_url.clone(),
        }) as Arc<dyn ToolMiddleware>);
    }
    middlewares.push(gate_mw.clone() as Arc<dyn ToolMiddleware>);
    middlewares.push(loop_mw.clone() as Arc<dyn ToolMiddleware>);
    middlewares.push(step_emitter as Arc<dyn ToolMiddleware>);

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
    let nudge_handle = tokio::spawn(async move {
        let mut level: u8 = 0;
        while let Some(text) = nudge_rx.recv().await {
            level = level.saturating_add(1);
            let _ = progress_tx_nudge
                .send(BrowseProgress::Nudge { level, text })
                .await;
        }
    });

    // 11. Check for early cancellation before starting the loop.
    if cancel.load(Ordering::SeqCst) {
        drop(engine); // drop engine (and its middleware chain) to shut down channels
        let _ = nudge_handle.await;
        let _ = approval_bridge_handle.await;
        let final_url = {
            let url = current_url.lock().await;
            if url.is_empty() { None } else { Some(url.clone()) }
        };
        let result = BrowseResult {
            achieved: false,
            summary: "Browse cancelled before starting".to_string(),
            reason: BrowseReason::Cancelled,
            steps_used: 0,
            final_url,
        };
        let _ = progress_tx.send(BrowseProgress::Completed(result.clone())).await;
        return Ok(result);
    }

    // 12. Run the agentic loop.
    let query_result = engine.query(&req.goal).await;

    // 13. Determine the result.
    let steps_used = engine.turns_used();
    let final_url = {
        let url = current_url.lock().await;
        if url.is_empty() { None } else { Some(url.clone()) }
    };

    // Check cancellation flag — if set during run, override the result.
    let cancelled = cancel.load(Ordering::SeqCst);

    // Check middleware termination flags first — they override sentinel parsing.
    let middleware_reason = if cancelled {
        Some(BrowseReason::Cancelled)
    } else if loop_mw.is_stopped() {
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
                        BrowseReason::Cancelled => {
                            "Agent terminated: cancelled by user".to_string()
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
                } else {
                    let crash_keywords = ["browser", "CDP", "Chrome", "WebSocket", "connection", "disconnected", "tungstenite"];
                    if crash_keywords.iter().any(|kw| msg.to_lowercase().contains(&kw.to_lowercase())) {
                        BrowseReason::BrowserCrashed
                    } else {
                        BrowseReason::Bailed
                    }
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

    // 14. Emit Completed event + speak the final summary if voice is enabled.
    let _ = progress_tx
        .send(BrowseProgress::Completed(result.clone()))
        .await;
    if req.voice {
        let phrase = if result.achieved {
            format!("Done. {}", result.summary)
        } else {
            format!("Stopped. {}", result.summary)
        };
        tokio::spawn(async move {
            crate::voice::speak_browse_milestone(
                crate::voice::BrowseMilestone::End,
                &phrase,
            )
            .await;
        });
    }

    Ok(result)
}
