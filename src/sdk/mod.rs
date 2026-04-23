//! RustyClaw SDK — headless NDJSON server for embedding.
//!
//! `SdkServer::run()` is the main loop: reads requests from a `Transport`,
//! dispatches to `SdkSession` instances, and forwards notifications/approvals
//! back to the host over stdout.

pub mod approval;
pub mod protocol;
pub mod session;
pub mod transport;

pub use protocol::*;

use crate::browser::browse_loop::{BrowsePolicy, BrowseProgress, BrowseRequest, run_browse};
use crate::config::Config;
use crate::tools::all_tools;
use anyhow::Result;
use session::SdkSession;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;
use tokio::sync::mpsc;
use transport::Transport;

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Headless SDK server — reads NDJSON requests, writes NDJSON responses.
pub struct SdkServer;

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

        loop {
            tokio::select! {
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
                if let Some(dir) = cwd {
                    cfg.cwd = PathBuf::from(dir);
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

                let tools = all_tools(&cfg);
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
                let (all_tools_list, shared_state) = crate::tools::all_tools_with_state(&cfg);
                let browser_session = shared_state.browser_session.clone();

                // Channels: progress events from browse loop → notif forwarding task
                let (progress_tx, mut progress_rx) = tokio::sync::mpsc::channel::<BrowseProgress>(64);
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
                            BrowseProgress::Completed(result) => {
                                SdkNotification::BrowseCompleted {
                                    session_id: fwd_sid.clone(),
                                    result,
                                }
                            }
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
                tokio::spawn(async move {
                    while let Some(prompt) = approval_rx.recv().await {
                        let notif = SdkNotification::BrowseApprovalNeeded {
                            session_id: appr_sid.clone(),
                            step: prompt.step,
                            tool_name: prompt.tool_name,
                            target_text: prompt.target_text,
                            url: prompt.url,
                            reason: prompt.reason,
                        };
                        let _ = appr_notif_tx.send(notif);
                        // Note: prompt.reply is dropped here — the gate will treat
                        // an unreceived reply as a deny in Phase A. Phase B will
                        // wire BrowseApprovalReply to fulfill this oneshot.
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
                tokio::spawn(async move {
                    let channels = crate::browser::browse_loop::BrowseChannels { progress_tx, approval_tx, cancel };
                    let _ = run_browse(
                        browse_req,
                        &cfg,
                        all_tools_list,
                        current_url,
                        browser_session,
                        channels,
                    )
                    .await;
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

            // BrowseApprovalReply has no request id — it's a fire-and-forget host reply.
            SdkRequest::BrowseApprovalReply { .. } => {
                // Phase B: route to the waiting browse session's approval channel.
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
    let dir = Config::sessions_dir();
    if !dir.exists() {
        return Ok(Vec::new());
    }

    let mut entries = tokio::fs::read_dir(&dir).await?;
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

        sessions.push((
            meta.created_at,
            SessionInfo {
                id,
                name: meta.name,
                created_at: format!("{}", meta.created_at),
                preview: meta.preview,
            },
        ));
    }

    // Sort by created_at descending (most recent first)
    sessions.sort_by(|a, b| b.0.cmp(&a.0));

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
