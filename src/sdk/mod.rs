//! OxideClaw SDK — headless NDJSON server for embedding.
//!
//! `SdkServer::run()` is the main loop: reads requests from a `Transport`,
//! dispatches to `SdkSession` instances, and forwards notifications/approvals
//! back to the host over stdout.

pub mod approval;
pub mod protocol;
pub mod session;
pub mod transport;

pub use protocol::*;

use crate::browser::browse_loop::{
    BrowsePolicy, BrowseProgress, BrowseReason, BrowseRequest, BrowseResult, run_browse,
};
use crate::config::Config;
use anyhow::Result;
use session::SdkSession;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;
use tokio::sync::{mpsc, oneshot};
use transport::Transport;

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The approval gate a browse run is blocked on, keyed by browse session id:
/// `(step, reply)`. A run waits on at most one prompt at a time.
/// Per browse session: the waiting prompt's `approval_id` and its reply.
type PendingBrowse = Arc<std::sync::Mutex<HashMap<String, (u64, oneshot::Sender<bool>)>>>;

/// Headless SDK server — reads NDJSON requests, writes NDJSON responses.
pub struct SdkServer;

/// A client-supplied working directory must exist; otherwise every tool in
/// the session fails one call at a time with a confusing path error.
pub(crate) fn validate_session_cwd(dir: Option<String>) -> Result<Option<PathBuf>, String> {
    match dir {
        None => Ok(None),
        Some(d) => {
            let p = PathBuf::from(&d);
            if p.is_dir() {
                Ok(Some(p))
            } else {
                Err(format!("cwd {d:?} is not a directory"))
            }
        }
    }
}

impl SdkServer {
    /// Run the server loop until the transport closes (EOF on stdin).
    pub async fn run(config: Config, mut transport: impl Transport) -> Result<()> {
        let start_time = Instant::now();

        // Shared notification channel: sessions → server → stdout
        let (notif_tx, mut notif_rx) = mpsc::unbounded_channel::<SdkNotification>();

        // Shared approval-out channel: sessions → server → stdout
        let (approval_out_tx, mut approval_out_rx) = mpsc::unbounded_channel::<SdkNotification>();

        // Per-session approval-in channels: server → session
        let mut approval_ins: HashMap<String, mpsc::UnboundedSender<(String, Option<String>)>> =
            HashMap::new();

        // Track active session count (shared with spawned tasks)
        let active_sessions = Arc::new(AtomicUsize::new(0));

        let pending_browse: PendingBrowse = Arc::default();

        loop {
            // Biased so an approval request never overtakes the deltas and
            // tool events a session queued before it.
            tokio::select! {
                biased;
                req = transport.read_request() => {
                    match req {
                        Ok(Some(request)) => {
                            Self::handle_request(
                                request,
                                &config,
                                &mut transport,
                                &notif_tx,
                                &approval_out_tx,
                                &mut approval_ins,
                                &pending_browse,
                                &active_sessions,
                                start_time,
                            ).await?;
                        }
                        Ok(None) => break, // EOF — host closed stdin
                        Err(e) => {
                            eprintln!("[sdk] Request parse error: {e:#}");
                            // Continue — don't crash on malformed input
                        }
                    }
                }
                Some(notif) = notif_rx.recv() => {
                    transport.send_notification(notif).await?;
                }
                Some(approval_notif) = approval_out_rx.recv() => {
                    transport.send_notification(approval_notif).await?;
                }
            }
        }

        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn handle_request(
        request: SdkRequest,
        config: &Config,
        transport: &mut impl Transport,
        notif_tx: &mpsc::UnboundedSender<SdkNotification>,
        approval_out_tx: &mpsc::UnboundedSender<SdkNotification>,
        approval_ins: &mut HashMap<String, mpsc::UnboundedSender<(String, Option<String>)>>,
        pending_browse: &PendingBrowse,
        active_sessions: &Arc<AtomicUsize>,
        start_time: Instant,
    ) -> Result<()> {
        match request {
            // ── Health Check ────────────────────────────────────────
            SdkRequest::HealthCheck { id } => {
                transport
                    .send_response(SdkResponse::HealthCheck {
                        id,
                        status: "ok".into(),
                        version: VERSION.into(),
                        protocol_version: crate::sdk::protocol::PROTOCOL_VERSION,
                        active_sessions: active_sessions.load(Ordering::Relaxed),
                        uptime_seconds: start_time.elapsed().as_secs(),
                    })
                    .await?;
            }

            // ── Session Start ───────────────────────────────────────
            SdkRequest::SessionStart {
                id,
                prompt,
                cwd,
                model,
                max_turns,
                max_budget_usd,
                policy,
                capabilities,
                ..
            } => {
                // Clone and override config
                let mut cfg = config.clone();
                match validate_session_cwd(cwd) {
                    Ok(Some(dir)) => cfg.retarget_cwd(dir),
                    Ok(None) => {}
                    Err(message) => {
                        transport
                            .send_response(SdkResponse::Error {
                                id,
                                code: "invalid_cwd".into(),
                                message,
                            })
                            .await?;
                        return Ok(());
                    }
                }
                if let Some(m) = model {
                    cfg.model = m;
                }
                if let Some(turns) = max_turns {
                    cfg.max_turns = turns;
                }
                if let Some(budget) = max_budget_usd {
                    cfg.max_budget_usd = Some(budget);
                }

                let tools = crate::mcp::tools_for_config(&cfg).await;
                let session_policy = policy.unwrap_or_default();
                let session_caps = capabilities.unwrap_or_default();

                // Per-session approval channel
                let (approval_in_tx, approval_in_rx) =
                    mpsc::unbounded_channel::<(String, Option<String>)>();

                // Clone notif_tx for error reporting in the spawned task
                let spawn_notif_tx = notif_tx.clone();

                let session = match SdkSession::new(
                    cfg,
                    tools,
                    session_policy,
                    session_caps,
                    notif_tx.clone(),
                    approval_out_tx.clone(),
                    approval_in_rx,
                ) {
                    Ok(s) => s,
                    Err(e) => {
                        transport
                            .send_response(SdkResponse::Error {
                                id,
                                code: "session_create_failed".into(),
                                message: format!("{e:#}"),
                            })
                            .await?;
                        return Ok(());
                    }
                };

                let session_id = session.session_id.clone();
                let model_name = session.config_model().to_string();

                // Register approval channel
                approval_ins.insert(session_id.clone(), approval_in_tx);
                active_sessions.fetch_add(1, Ordering::Relaxed);

                // Respond immediately
                transport
                    .send_response(SdkResponse::SessionStarted {
                        id,
                        session_id: session_id.clone(),
                        model: model_name,
                        router_decision: None,
                    })
                    .await?;

                // Spawn turn execution in background
                let spawn_session_id = session_id;
                let session_counter = Arc::clone(active_sessions);
                tokio::spawn(async move {
                    let mut session = session;
                    if let Err(e) = session.execute_turn(prompt).await {
                        let _ = spawn_notif_tx.send(SdkNotification::Error {
                            session_id: spawn_session_id,
                            code: "turn_error".into(),
                            message: format!("{e:#}"),
                        });
                    }
                    session_counter.fetch_sub(1, Ordering::Relaxed);
                });
            }

            // ── Session List ────────────────────────────────────────
            SdkRequest::SessionList { id, limit } => {
                let sessions = match list_sessions(limit).await {
                    Ok(s) => s,
                    Err(e) => {
                        transport
                            .send_response(SdkResponse::Error {
                                id,
                                code: "session_list_failed".into(),
                                message: format!("{e:#}"),
                            })
                            .await?;
                        return Ok(());
                    }
                };

                transport
                    .send_response(SdkResponse::SessionList { id, sessions })
                    .await?;
            }

            // ── RAG Search ──────────────────────────────────────────
            SdkRequest::RagSearch { id, query, limit } => {
                let results = match rag_search(&config.cwd, &query, limit.unwrap_or(20)) {
                    Ok(r) => r,
                    Err(e) => {
                        transport
                            .send_response(SdkResponse::Error {
                                id,
                                code: "rag_search_failed".into(),
                                message: format!("{e:#}"),
                            })
                            .await?;
                        return Ok(());
                    }
                };

                transport
                    .send_response(SdkResponse::RagSearchResult { id, results })
                    .await?;
            }

            // ── Tool Approve ────────────────────────────────────────
            SdkRequest::ToolApprove { id, approval_id } => {
                // Phase A: broadcast to all sessions
                let mut delivered = false;
                for tx in approval_ins.values() {
                    if tx.send((approval_id.clone(), None)).is_ok() {
                        delivered = true;
                    }
                }
                if !delivered {
                    transport
                        .send_response(SdkResponse::Error {
                            id,
                            code: "no_session".into(),
                            message: "No active session to receive approval.".into(),
                        })
                        .await?;
                }
            }

            // ── Tool Deny ───────────────────────────────────────────
            SdkRequest::ToolDeny {
                id,
                approval_id,
                reason,
            } => {
                let deny_reason = reason.unwrap_or_else(|| "Denied by host.".into());
                let mut delivered = false;
                for tx in approval_ins.values() {
                    if tx
                        .send((approval_id.clone(), Some(deny_reason.clone())))
                        .is_ok()
                    {
                        delivered = true;
                    }
                }
                if !delivered {
                    transport
                        .send_response(SdkResponse::Error {
                            id,
                            code: "no_session".into(),
                            message: "No active session to receive denial.".into(),
                        })
                        .await?;
                }
            }

            // ── Browse Start ────────────────────────────────────────
            SdkRequest::BrowseStart {
                id,
                goal,
                policy,
                max_steps,
                yolo_ack,
            } => {
                // Validate yolo_ack requirement
                if policy == BrowsePolicy::Yolo && !yolo_ack {
                    transport
                        .send_response(SdkResponse::Error {
                            id,
                            code: "yolo_ack_required".into(),
                            message: "browse/start with policy=yolo requires yolo_ack=true".into(),
                        })
                        .await?;
                    return Ok(());
                }

                // Generate a session ID for this browse run
                let session_id = format!(
                    "browse-{}",
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis()
                );

                // Clone config and build tools
                let cfg = config.clone();
                let (mut all_tools_list, shared_state) = crate::tools::all_tools_with_state(&cfg);
                crate::tools::apply_tool_filters(&mut all_tools_list, &cfg);
                let browser_session = shared_state.browser_session.clone();

                // Channels: progress events from browse loop → notif forwarding task
                let (progress_tx, mut progress_rx) =
                    tokio::sync::mpsc::channel::<BrowseProgress>(64);
                // Channel: approval prompts from browse loop → host
                let (approval_tx, mut approval_rx) =
                    tokio::sync::mpsc::channel::<crate::browser::approval_gate::ApprovalPrompt>(4);

                let current_url = Arc::new(tokio::sync::Mutex::new(String::new()));

                // Respond immediately with session_id
                transport
                    .send_response(SdkResponse::BrowseStarted {
                        id,
                        session_id: session_id.clone(),
                    })
                    .await?;

                active_sessions.fetch_add(1, Ordering::Relaxed);

                // Forward BrowseProgress events as SdkNotification NDJSON
                let fwd_notif_tx = notif_tx.clone();
                let fwd_sid = session_id.clone();
                tokio::spawn(async move {
                    while let Some(event) = progress_rx.recv().await {
                        let notif = match event {
                            BrowseProgress::Step { n, action, target } => {
                                SdkNotification::BrowseProgress {
                                    session_id: fwd_sid.clone(),
                                    step: n,
                                    action,
                                    target,
                                }
                            }
                            BrowseProgress::Completed(result) => SdkNotification::BrowseCompleted {
                                session_id: fwd_sid.clone(),
                                result,
                            },
                            BrowseProgress::Nudge { .. } | BrowseProgress::Started { .. } => {
                                // Not surfaced as SDK notifications
                                continue;
                            }
                            BrowseProgress::ApprovalNeeded { .. } => {
                                // Handled via approval_rx below
                                continue;
                            }
                        };
                        let _ = fwd_notif_tx.send(notif);
                    }
                });

                // Forward ApprovalPrompt events as BrowseApprovalNeeded notifications
                let appr_notif_tx = approval_out_tx.clone();
                let appr_sid = session_id.clone();
                let appr_pending = Arc::clone(pending_browse);
                tokio::spawn(async move {
                    while let Some(prompt) = approval_rx.recv().await {
                        // Park the reply before the host can see the prompt, so
                        // an instant browse/approval_reply always finds it. A
                        // stale entry (the gate timed out) is simply replaced.
                        appr_pending
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .insert(appr_sid.clone(), (prompt.id, prompt.reply));
                        let notif = SdkNotification::BrowseApprovalNeeded {
                            session_id: appr_sid.clone(),
                            approval_id: prompt.id,
                            step: prompt.step,
                            tool_name: prompt.tool_name,
                            target_text: prompt.target_text,
                            url: prompt.url,
                            reason: prompt.reason,
                        };
                        let _ = appr_notif_tx.send(notif);
                    }
                });

                // Spawn the browse run
                let browse_req = BrowseRequest {
                    goal,
                    policy,
                    max_steps: max_steps.unwrap_or(cfg.browse_max_steps),
                    voice: false,
                };
                let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
                let session_counter = Arc::clone(active_sessions);
                let run_notif_tx = notif_tx.clone();
                let run_pending = Arc::clone(pending_browse);
                tokio::spawn(async move {
                    let channels = crate::browser::browse_loop::BrowseChannels {
                        progress_tx,
                        approval_tx,
                        cancel,
                    };
                    let outcome = run_browse(
                        browse_req,
                        &cfg,
                        all_tools_list,
                        current_url,
                        browser_session,
                        channels,
                    )
                    .await;
                    // run_browse only reports Completed itself on the Ok path;
                    // without this a setup failure (e.g. no credential) left
                    // the host waiting forever after browse/started.
                    if let Err(e) = outcome {
                        let _ = run_notif_tx.send(SdkNotification::BrowseCompleted {
                            session_id: session_id.clone(),
                            result: BrowseResult {
                                achieved: false,
                                summary: format!("Browse agent error: {e:#}"),
                                reason: BrowseReason::Bailed,
                                steps_used: 0,
                                final_url: None,
                            },
                        });
                    }
                    run_pending
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(&session_id);
                    session_counter.fetch_sub(1, Ordering::Relaxed);
                });
            }

            // ── Not yet implemented (Phase A stubs) ─────────────────
            SdkRequest::TurnStart { id, .. }
            | SdkRequest::TurnInterrupt { id, .. }
            | SdkRequest::SessionResume { id, .. }
            | SdkRequest::SessionExport { id, .. }
            | SdkRequest::CostReport { id, .. } => {
                transport
                    .send_response(SdkResponse::Error {
                        id,
                        code: "not_implemented".into(),
                        message: "This request type is not yet implemented in Phase A.".into(),
                    })
                    .await?;
            }

            // BrowseApprovalReply has no request id — it's a fire-and-forget
            // host reply, so a mismatch can only be logged, not answered.
            // Matched on `approval_id`, not `step`: the step number repeats
            // after a denied or expired prompt, so a late approval of one
            // action would grant the next.
            SdkRequest::BrowseApprovalReply {
                session_id,
                approval_id,
                step,
                approved,
            } => {
                let mut pending = pending_browse.lock().unwrap_or_else(|e| e.into_inner());
                match pending.remove(&session_id) {
                    Some((want, reply)) if want == approval_id => {
                        if reply.send(approved).is_err() {
                            eprintln!(
                                "[sdk] browse/approval_reply for {session_id} approval {approval_id} (step {step}) arrived after the gate timed out"
                            );
                        }
                    }
                    Some((want, reply)) => {
                        eprintln!(
                            "[sdk] browse/approval_reply for {session_id} names approval {approval_id}, but approval {want} is waiting; ignored"
                        );
                        pending.insert(session_id, (want, reply));
                    }
                    None => eprintln!(
                        "[sdk] browse/approval_reply for {session_id}: no approval is pending"
                    ),
                }
            }
        }

        Ok(())
    }
}

// ── Session listing (self-contained, no TUI dependency) ─────────────────────

/// List saved sessions by reading `.meta` files from the sessions directory.
/// This avoids importing `crate::session` which has TUI dependencies not
/// available in the library crate.
async fn list_sessions(limit: Option<usize>) -> Result<Vec<SessionInfo>> {
    list_sessions_in(&Config::sessions_dir(), limit).await
}

async fn list_sessions_in(dir: &std::path::Path, limit: Option<usize>) -> Result<Vec<SessionInfo>> {
    if !dir.exists() {
        return Ok(Vec::new());
    }

    let mut entries = tokio::fs::read_dir(dir).await?;
    let mut sessions: Vec<(u64, SessionInfo)> = Vec::new();

    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("meta") {
            continue;
        }

        let id = match path.file_stem().and_then(|s| s.to_str()) {
            Some(s) if !s.is_empty() => s.to_string(),
            _ => continue,
        };

        let content = match tokio::fs::read_to_string(&path).await {
            Ok(c) => c,
            Err(_) => continue,
        };

        // Inline deserialization matching SessionMeta format
        #[derive(serde::Deserialize)]
        struct RawMeta {
            #[allow(dead_code)]
            id: String,
            name: String,
            created_at: u64,
            #[serde(default)]
            preview: String,
        }

        let meta: RawMeta = match serde_json::from_str(&content) {
            Ok(m) => m,
            Err(_) => continue,
        };

        // Same order as the TUI's Session::list: last activity, not
        // creation, so a session worked on today is not buried.
        let modified = tokio::fs::metadata(dir.join(format!("{id}.jsonl")))
            .await
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_secs());
        sessions.push((
            meta.created_at.max(modified),
            SessionInfo {
                id,
                name: meta.name,
                created_at: format!("{}", meta.created_at),
                preview: meta.preview,
            },
        ));
    }

    // Most recently active first
    sessions.sort_by_key(|e| std::cmp::Reverse(e.0));

    let limit = limit.unwrap_or(50);
    let infos: Vec<SessionInfo> = sessions.into_iter().take(limit).map(|(_, s)| s).collect();
    Ok(infos)
}

// ── RAG search helper ───────────────────────────────────────────────────────

/// Search the local RAG index and map results to SDK protocol format.
fn rag_search(cwd: &std::path::Path, query: &str, limit: usize) -> Result<Vec<RagResult>> {
    let db = crate::rag::RagDb::open(cwd)?;
    let results = crate::rag::search::search(&db, query, limit as i64)?;

    Ok(results
        .into_iter()
        .map(|r| RagResult {
            file: r.file_path,
            line: r.start_line as u32,
            symbol: r.symbol_name,
            kind: r.symbol_kind,
            snippet: r.content,
        })
        .collect())
}

#[cfg(test)]
mod cwd_tests {
    use super::validate_session_cwd;

    #[test]
    fn a_missing_directory_is_rejected_up_front() {
        assert!(validate_session_cwd(None).unwrap().is_none());
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            validate_session_cwd(Some(dir.path().to_string_lossy().into_owned())).unwrap(),
            Some(dir.path().to_path_buf())
        );
        let err = validate_session_cwd(Some("/definitely/not/here".into())).unwrap_err();
        assert!(err.contains("not a directory"), "{err}");
    }
}

#[cfg(test)]
mod browse_tests {
    use super::*;
    use async_trait::async_trait;

    /// Records responses; notifications go through the server's channels.
    #[derive(Default)]
    struct Recorder(std::sync::Mutex<Vec<SdkResponse>>);

    #[async_trait]
    impl Transport for Recorder {
        async fn read_request(&mut self) -> Result<Option<SdkRequest>> {
            Ok(None)
        }
        async fn send_response(&self, response: SdkResponse) -> Result<()> {
            self.0.lock().unwrap().push(response);
            Ok(())
        }
        async fn send_notification(&self, _: SdkNotification) -> Result<()> {
            Ok(())
        }
    }

    struct Harness {
        transport: Recorder,
        notif_tx: mpsc::UnboundedSender<SdkNotification>,
        notif_rx: mpsc::UnboundedReceiver<SdkNotification>,
        approval_out_tx: mpsc::UnboundedSender<SdkNotification>,
        approval_ins: HashMap<String, mpsc::UnboundedSender<(String, Option<String>)>>,
        pending: PendingBrowse,
        active: Arc<AtomicUsize>,
    }

    impl Harness {
        fn new() -> Self {
            let (notif_tx, notif_rx) = mpsc::unbounded_channel();
            let (approval_out_tx, _) = mpsc::unbounded_channel();
            Self {
                transport: Recorder::default(),
                notif_tx,
                notif_rx,
                approval_out_tx,
                approval_ins: HashMap::new(),
                pending: Arc::default(),
                active: Arc::new(AtomicUsize::new(0)),
            }
        }

        async fn handle(&mut self, cfg: &Config, req: SdkRequest) {
            SdkServer::handle_request(
                req,
                cfg,
                &mut self.transport,
                &self.notif_tx,
                &self.approval_out_tx,
                &mut self.approval_ins,
                &self.pending,
                &self.active,
                Instant::now(),
            )
            .await
            .unwrap();
        }
    }

    fn reply(session_id: &str, approval_id: u64, approved: bool) -> SdkRequest {
        SdkRequest::BrowseApprovalReply {
            session_id: session_id.into(),
            approval_id,
            step: 3,
            approved,
        }
    }

    /// The reply used to be dropped on the floor, so every gated browse
    /// action timed out into a deny no matter what the host answered.
    #[tokio::test]
    async fn approval_reply_reaches_the_waiting_gate_only_for_its_prompt() {
        let mut h = Harness::new();
        let cfg = Config::default();
        let (tx, mut rx) = oneshot::channel();
        h.pending.lock().unwrap().insert("browse-1".into(), (3, tx));

        // A late reply to an expired prompt at the same step (approval 2),
        // and an unknown session: ignored, prompt still waiting.
        h.handle(&cfg, reply("browse-1", 2, true)).await;
        h.handle(&cfg, reply("browse-9", 3, true)).await;
        assert!(rx.try_recv().is_err());
        assert!(h.pending.lock().unwrap().contains_key("browse-1"));

        h.handle(&cfg, reply("browse-1", 3, true)).await;
        assert_eq!(rx.await, Ok(true));
        assert!(h.pending.lock().unwrap().is_empty());
        assert!(
            h.transport.0.lock().unwrap().is_empty(),
            "replies have no id"
        );
    }

    /// A run that fails before the loop starts (here: no credential) must
    /// still end with browse/completed instead of leaving the host hanging.
    #[tokio::test]
    async fn a_browse_setup_error_is_reported_as_completed() {
        let mut h = Harness::new();
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config {
            api_key: String::new(),
            model: "claude-sonnet-4-5".into(),
            cwd: dir.path().to_path_buf(),
            ..Default::default()
        };
        h.handle(
            &cfg,
            SdkRequest::BrowseStart {
                id: "1".into(),
                goal: "open example.com".into(),
                policy: BrowsePolicy::Pattern,
                max_steps: Some(3),
                yolo_ack: false,
            },
        )
        .await;
        let sid = match h.transport.0.lock().unwrap().first() {
            Some(SdkResponse::BrowseStarted { session_id, .. }) => session_id.clone(),
            other => panic!("expected browse/started, got {other:?}"),
        };
        let notif = tokio::time::timeout(std::time::Duration::from_secs(10), h.notif_rx.recv())
            .await
            .expect("no browse/completed")
            .unwrap();
        match notif {
            SdkNotification::BrowseCompleted { session_id, result } => {
                assert_eq!(session_id, sid);
                assert!(!result.achieved);
                assert_eq!(result.reason, BrowseReason::Bailed);
                assert!(result.summary.contains("credential"), "{}", result.summary);
            }
            other => panic!("expected browse/completed, got {other:?}"),
        }
    }
}

#[cfg(test)]
mod list_tests {
    use super::list_sessions_in;
    use std::time::{Duration, UNIX_EPOCH};

    #[tokio::test]
    async fn sessions_are_listed_by_last_activity() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        for (id, created) in [("old-but-active", 1_000u64), ("newer", 2_000)] {
            std::fs::write(
                d.join(format!("{id}.meta")),
                serde_json::json!({"id": id, "name": id, "created_at": created}).to_string(),
            )
            .unwrap();
        }
        let jsonl = d.join("old-but-active.jsonl");
        std::fs::write(&jsonl, "").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&jsonl)
            .unwrap()
            .set_modified(UNIX_EPOCH + Duration::from_secs(3_000))
            .unwrap();

        let ids: Vec<String> = list_sessions_in(d, None)
            .await
            .unwrap()
            .into_iter()
            .map(|s| s.id)
            .collect();
        assert_eq!(ids, ["old-but-active", "newer"]);
    }
}
