use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use tokio::sync::mpsc;

use crate::browser::BrowserSession;
use crate::browser::approval_gate::{ApprovalGate, ApprovalGateMiddleware, ApprovalPrompt};
use crate::browser::loop_detector::LoopDetectorMiddleware;
use crate::browser::middleware::{MiddlewareVerdict, ToolMiddleware};
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
    /// Parse `browseDefaultPolicy` ("pattern" / "ask"). Unknown or empty
    /// strings fall back to `Pattern`.
    ///
    /// "yolo" is deliberately not honored: settings.json can come from a
    /// cloned, untrusted repo, and yolo switches off the approval gate for
    /// purchases, submits and OAuth grants. It must be asked for per run
    /// (--yolo, /browse --yolo, or SDK yolo with yolo_ack).
    pub fn from_settings_str(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "ask" => Self::Ask,
            "yolo" => {
                tracing::warn!(
                    "browseDefaultPolicy=yolo is ignored; use --yolo / /browse --yolo per run"
                );
                Self::Pattern
            }
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
    Started {
        goal: String,
        max_steps: u32,
    },
    Step {
        n: u32,
        action: String,
        target: String,
    },
    Nudge {
        level: u8,
        text: String,
    },
    ApprovalNeeded {
        step: u32,
        action: String,
        target_text: String,
        url: String,
        reason: String,
    },
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
         - If approval is denied, do not retry that action another way (keyboard, another ref); try a different approach or call browse_done(achieved=false)."
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

/// (achieved, summary) from the typed arguments of a `browse_done` call.
/// Read from the call, never from text: a page showing "BROWSE_DONE
/// achieved=true" came back in a browser_get_text result and was taken as
/// the verdict.
fn browse_done_args(input: &serde_json::Value) -> Option<(bool, String)> {
    Some((
        input["achieved"].as_bool()?,
        input["summary"].as_str()?.to_string(),
    ))
}

/// Extract a human-readable "target" from tool input for Step events.
/// Prefers `url` → `ref` → `selector` → `key` → empty string.
fn extract_target(input: &serde_json::Value) -> String {
    for key in ["url", "ref", "selector", "key"] {
        if let Some(s) = input.get(key).and_then(|v| v.as_str())
            && !s.is_empty()
        {
            return s.to_string();
        }
    }
    String::new()
}

/// Channels and cancellation state the caller must plumb into a browse run.
pub struct BrowseChannels {
    pub progress_tx: mpsc::Sender<BrowseProgress>,
    pub approval_tx: mpsc::Sender<ApprovalPrompt>,
    pub cancel: Arc<AtomicBool>,
    /// Where each API call's usage goes, so a session's /cost and /budget
    /// include the run. None where the run is the whole process.
    pub usage_sink: Option<crate::tools::UsageSink>,
}

/// Puts the browser tools' own questions to the user (may the browser reach
/// this loopback service?) through the run's approval channel, the one
/// prompt every browse host (TUI, SDK, `oxideclaw browse`) answers. The
/// run's engine is otherwise headless, so without this a local dev server
/// could never be approved from a /browse run.
struct ApprovalChannelAsker {
    approval_tx: mpsc::Sender<ApprovalPrompt>,
    step_counter: Arc<AtomicU32>,
}

#[async_trait]
impl crate::permissions::PermissionAsker for ApprovalChannelAsker {
    async fn ask(
        &self,
        tool_name: &str,
        description: &str,
        input: &serde_json::Value,
    ) -> Option<crate::permissions::PermissionDecision> {
        use crate::permissions::PermissionDecision;
        let (reply, rx) = tokio::sync::oneshot::channel();
        let prompt = ApprovalPrompt {
            id: crate::browser::approval_gate::next_prompt_id(),
            // The step emitter counted this call before the tool ran.
            step: self.step_counter.load(Ordering::Relaxed),
            tool_name: tool_name.to_string(),
            target_text: input["target"].as_str().unwrap_or("").to_string(),
            url: input["url"].as_str().unwrap_or("").to_string(),
            // One line: browse hosts show the reason inline.
            reason: description.split_whitespace().collect::<Vec<_>>().join(" "),
            reply,
        };
        self.approval_tx.send(prompt).await.ok()?;
        match tokio::time::timeout(crate::browser::approval_gate::APPROVAL_WINDOW, rx).await {
            Ok(Ok(true)) => Some(PermissionDecision::Allow),
            Ok(Ok(false)) => Some(PermissionDecision::Deny),
            _ => None,
        }
    }
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

    async fn after_tool(&self, _tool_name: &str, _output: &str) -> Option<String> {
        let url = self.session.lock().await.current_url.clone();
        *self.shared_url.lock().await = url;
        None
    }
}

/// Enforces the step cap per browser action. The engine's turn cap alone
/// let one turn with several tool calls run past it. Placed first in the
/// chain so a call over the cap never reaches the approval prompt.
struct StepCapMiddleware {
    counter: Arc<AtomicU32>,
    max_steps: u32,
    stopped: AtomicBool,
}

#[async_trait]
impl ToolMiddleware for StepCapMiddleware {
    async fn before_tool(&self, tool_name: &str, _input: &serde_json::Value) -> MiddlewareVerdict {
        // The model must still be able to report after the last step.
        if tool_name == "browse_done" || self.counter.load(Ordering::Relaxed) < self.max_steps {
            return MiddlewareVerdict::Allow;
        }
        // It was told to finish and acted instead: end the run after this
        // turn rather than spend more turns on denied calls.
        self.stopped.store(true, Ordering::SeqCst);
        MiddlewareVerdict::Deny {
            reason: format!(
                "step cap of {} reached; call browse_done now",
                self.max_steps
            ),
        }
    }

    async fn after_tool(&self, _tool_name: &str, _output: &str) -> Option<String> {
        None
    }

    fn should_stop(&self) -> bool {
        self.stopped.load(Ordering::SeqCst)
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
        // Finishing is not a browser action, so it does not use up a step.
        if tool_name == "browse_done" {
            return MiddlewareVerdict::Allow;
        }
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

    async fn after_tool(&self, _tool_name: &str, _output: &str) -> Option<String> {
        None
    }
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
    let BrowseChannels {
        progress_tx,
        approval_tx,
        cancel,
        usage_sink,
    } = channels;
    // A zero cap (settings.json, the SDK) used to fall through to the
    // engine's default of 50 turns.
    let req = BrowseRequest {
        max_steps: req.max_steps.max(1),
        ..req
    };
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
            crate::voice::speak_browse_milestone(crate::voice::BrowseMilestone::Start, &goal).await;
        });
    }

    // 2. Shared step counter for the approval prompt.
    let step_counter = Arc::new(AtomicU32::new(0));

    // 3. Build the approval gate from config patterns.
    // The gate sends prompts to an internal channel; a bridge task mirrors each
    // prompt as BrowseProgress::ApprovalNeeded, then forwards it to the caller.
    let gate = ApprovalGate::with_user_patterns(config.browse_approval_patterns.clone());
    let (internal_approval_tx, mut internal_approval_rx) = mpsc::channel::<ApprovalPrompt>(16);
    let consent_asker = Arc::new(ApprovalChannelAsker {
        approval_tx: internal_approval_tx.clone(),
        step_counter: step_counter.clone(),
    });
    let gate_mw = Arc::new(
        ApprovalGateMiddleware::new(
            gate,
            req.policy,
            current_url.clone(),
            internal_approval_tx,
            step_counter.clone(),
            req.voice,
        )
        .with_voice_api_url(config.voice_api_url.clone())
        .with_browser_session(browser_session.clone()),
    );

    // Bridge: internal approvals → progress event + external approval channel.
    // The gate-trip announcement is spoken by the gate itself, before it
    // starts listening: spoken here, it played into the open mic.
    let approval_bridge_progress_tx = progress_tx.clone();
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
            if approval_tx.send(prompt).await.is_err() {
                // Caller dropped the approval channel — stop bridging.
                break;
            }
        }
    });

    // 4. Build the loop detector middleware.
    let (nudge_tx, mut nudge_rx) = mpsc::channel::<String>(16);
    let loop_mw = Arc::new(LoopDetectorMiddleware::new(nudge_tx));

    // 5. Build the step-cap middleware (runs first) and the step-emitter
    // middleware (runs last — only fires for allowed calls).
    let cap_mw = Arc::new(StepCapMiddleware {
        counter: step_counter.clone(),
        max_steps: req.max_steps,
        stopped: AtomicBool::new(false),
    });
    let step_emitter = Arc::new(StepEmitterMiddleware {
        progress_tx: progress_tx.clone(),
        counter: step_counter.clone(),
    });

    // 6. Assemble middleware chain (keep Arc refs for post-run inspection).
    // Order matters: url_sync runs first so after_tool fires BEFORE any later
    // middleware reads the updated URL on the next iteration's before_tool.
    let mut middlewares: crate::browser::middleware::MiddlewareChain = Vec::new();
    middlewares.push(cap_mw.clone() as Arc<dyn ToolMiddleware>);
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

    // 8. The step cap is enforced per action by StepCapMiddleware; every
    // tool turn spends a step or ends the run, so the turn cap is only a
    // backstop, with one turn to spare for browse_done after the last step.
    let mut browse_config = config.clone();
    browse_config.max_turns = req.max_steps.saturating_add(1);

    // 9. Create the browse-mode query engine.
    // A setup failure (no credential, a /model whose client cannot be
    // built) ends the run like any other error: frontends wait for
    // Completed after Started, and the TUI spinner ran on until Esc.
    // The headless engine's rules, plus someone to ask: browser tools never
    // need a rule prompt, so only their own questions (a loopback service
    // to open) reach the user, as approval prompts.
    let permission_gate =
        crate::permissions::PermissionGate::headless(&browse_config).with_asker(consent_asker);
    let mut engine =
        match QueryEngine::new_for_browse(browse_config, browser_tools, system_prompt, middlewares)
        {
            Ok(engine) => engine
                .with_usage_sink(usage_sink)
                .with_permission_gate(permission_gate),
            Err(e) => {
                let result = BrowseResult {
                    achieved: false,
                    summary: format!("Browse agent error: {e:#}"),
                    reason: BrowseReason::Bailed,
                    steps_used: 0,
                    final_url: None,
                };
                let _ = progress_tx
                    .send(BrowseProgress::Completed(result.clone()))
                    .await;
                return Ok(result);
            }
        };

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
            if url.is_empty() {
                None
            } else {
                Some(url.clone())
            }
        };
        let result = BrowseResult {
            achieved: false,
            summary: "Browse cancelled before starting".to_string(),
            reason: BrowseReason::Cancelled,
            steps_used: 0,
            final_url,
        };
        let _ = progress_tx
            .send(BrowseProgress::Completed(result.clone()))
            .await;
        return Ok(result);
    }

    // 12. Run the agentic loop, racing the cancel flag: it used to be read
    // only before and after the loop, so Ctrl-C / Esc could not stop a
    // running agent. Dropping the query future aborts the in-flight call.
    let cancel_watch = {
        let cancel = cancel.clone();
        async move {
            while !cancel.load(Ordering::SeqCst) {
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
        }
    };
    let query_result = tokio::select! {
        r = engine.query(&req.goal) => r,
        _ = cancel_watch => Err(anyhow::anyhow!("cancelled by user")),
    };

    // 13. Determine the result.
    let steps_used = step_counter.load(Ordering::Relaxed).min(req.max_steps);
    let final_url = {
        let url = current_url.lock().await;
        if url.is_empty() {
            None
        } else {
            Some(url.clone())
        }
    };

    // Check cancellation flag — if set during run, override the result.
    let cancelled = cancel.load(Ordering::SeqCst);

    // Check middleware termination flags first — they override the browse_done verdict.
    let middleware_reason = if cancelled {
        Some(BrowseReason::Cancelled)
    } else if loop_mw.is_stopped() {
        Some(BrowseReason::Stagnation)
    } else if gate_mw.is_user_denied() {
        Some(BrowseReason::UserDenied)
    } else if cap_mw.should_stop() {
        Some(BrowseReason::StepCap)
    } else {
        None
    };

    let result = match query_result {
        Ok(()) => {
            let done = engine
                .last_successful_call("browse_done")
                .and_then(browse_done_args);

            if let Some((achieved, summary)) = done {
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
                // Middleware stopped the loop before browse_done.
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
                        BrowseReason::StepCap => {
                            format!("Agent terminated: step cap of {} reached", req.max_steps)
                        }
                        _ => "Agent terminated by middleware".to_string(),
                    },
                    reason,
                    steps_used,
                    final_url,
                }
            } else if let Some(text) = engine.last_assistant_text() {
                // No browse_done, no middleware stop — engine stopped for other reasons.
                let reason = if steps_used >= req.max_steps
                    || engine.turns_used() > req.max_steps.saturating_add(1)
                {
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
                    let crash_keywords = [
                        "browser",
                        "CDP",
                        "Chrome",
                        "WebSocket",
                        "connection",
                        "disconnected",
                        "tungstenite",
                    ];
                    if crash_keywords
                        .iter()
                        .any(|kw| msg.to_lowercase().contains(&kw.to_lowercase()))
                    {
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
            crate::voice::speak_browse_milestone(crate::voice::BrowseMilestone::End, &phrase).await;
        });
    }

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A /browse run's engine had no one to ask, so a local dev server could
    /// never be approved there. The question now arrives as an approval
    /// prompt the host answers.
    #[tokio::test]
    async fn loopback_questions_reach_the_browse_host_as_approval_prompts() {
        use crate::permissions::PermissionAsker;
        for (answer, expected) in [
            (
                Some(true),
                Some(crate::permissions::PermissionDecision::Allow),
            ),
            (
                Some(false),
                Some(crate::permissions::PermissionDecision::Deny),
            ),
            (None, None),
        ] {
            let (tx, mut rx) = mpsc::channel::<ApprovalPrompt>(1);
            let asker = ApprovalChannelAsker {
                approval_tx: tx,
                step_counter: Arc::new(AtomicU32::new(4)),
            };
            let host = tokio::spawn(async move {
                let p = rx.recv().await.unwrap();
                let seen = (
                    p.step,
                    p.tool_name.clone(),
                    p.target_text.clone(),
                    p.url.clone(),
                );
                match answer {
                    Some(a) => {
                        let _ = p.reply.send(a);
                    }
                    None => drop(p.reply),
                }
                seen
            });
            let got = asker
                .ask(
                    crate::tools::browser_tools::LOOPBACK_QUESTION,
                    "Let the browser reach the local service at 127.0.0.1:3000?",
                    &serde_json::json!({"url": "http://127.0.0.1:3000/", "target": "127.0.0.1:3000"}),
                )
                .await;
            assert_eq!(got, expected);
            let (step, tool, target, url) = host.await.unwrap();
            assert_eq!(step, 4);
            assert_eq!(tool, "browser_loopback");
            assert_eq!(target, "127.0.0.1:3000");
            assert_eq!(url, "http://127.0.0.1:3000/");
        }
    }

    /// The cap was enforced per model turn, so a turn with several browser
    /// calls ran past it; browse_done must still get through at the cap.
    #[tokio::test]
    async fn the_step_cap_counts_browser_actions_not_turns() {
        let counter = Arc::new(AtomicU32::new(0));
        let cap = StepCapMiddleware {
            counter: counter.clone(),
            max_steps: 2,
            stopped: AtomicBool::new(false),
        };
        let (progress_tx, _progress_rx) = mpsc::channel(16);
        let emitter = StepEmitterMiddleware {
            progress_tx,
            counter: counter.clone(),
        };
        let click = serde_json::json!({"selector": "e1"});
        // One model turn with three parallel clicks: only two may run.
        let mut allowed = 0;
        for _ in 0..3 {
            if let MiddlewareVerdict::Allow = cap.before_tool("browser_click", &click).await {
                emitter.before_tool("browser_click", &click).await;
                allowed += 1;
            }
        }
        assert_eq!(allowed, 2);
        assert_eq!(counter.load(Ordering::Relaxed), 2);
        assert!(cap.should_stop(), "acting past the cap ends the run");

        let done = serde_json::json!({"summary": "x", "achieved": true});
        assert!(matches!(
            cap.before_tool("browse_done", &done).await,
            MiddlewareVerdict::Allow
        ));
        emitter.before_tool("browse_done", &done).await;
        assert_eq!(
            counter.load(Ordering::Relaxed),
            2,
            "browse_done is not a step"
        );
    }

    /// After Started, a setup failure sent nothing more: the TUI spinner ran
    /// until Esc and voice /browse stayed refused for the session.
    #[tokio::test]
    async fn a_setup_failure_still_completes_the_run() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config {
            model: "claude-sonnet-4-5".into(),
            api_key: String::new(),
            cwd: dir.path().to_path_buf(),
            ..Config::default()
        };
        let (progress_tx, mut progress_rx) = mpsc::channel(16);
        let (approval_tx, _approval_rx) = mpsc::channel(4);
        let channels = BrowseChannels {
            progress_tx,
            approval_tx,
            cancel: Arc::new(AtomicBool::new(false)),
            usage_sink: None,
        };
        let req = BrowseRequest {
            goal: "open example.com".into(),
            policy: BrowsePolicy::Pattern,
            max_steps: 5,
            voice: false,
        };
        let current_url = Arc::new(tokio::sync::Mutex::new(String::new()));
        let result = run_browse(req, &config, Vec::new(), current_url, None, channels)
            .await
            .unwrap();
        assert!(!result.achieved);
        assert_eq!(result.reason, BrowseReason::Bailed);

        let mut events = Vec::new();
        while let Some(ev) = progress_rx.recv().await {
            events.push(ev);
        }
        assert!(matches!(
            events.first(),
            Some(BrowseProgress::Started { .. })
        ));
        match events.last() {
            Some(BrowseProgress::Completed(r)) => {
                assert!(r.summary.starts_with("Browse agent error"), "{}", r.summary)
            }
            other => panic!("expected Completed last, got {other:?}"),
        }
    }
}
