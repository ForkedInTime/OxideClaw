//! The ACP agent loop: JSON-RPC in, `session/update` out, one `SdkSession`
//! per ACP session. See the module docs in `acp/mod.rs` for scope.

use super::rpc::{self, Incoming, RpcError};
use crate::api::types::{ContentBlock, Message, Role, ToolResultContent};
use crate::config::Config;
use crate::mcp::types::{HttpServerConfig, McpServerConfig, StdioServerConfig};
use crate::sdk::protocol::{Capabilities, Policy, SdkNotification};
use crate::sdk::session::{CancelSignal, SdkSession, TurnEnd};
use crate::sdk::transport::request_id_from_prefix;
use crate::sdk::transport::stdio::{LineRead, spawn_line_reader};
use crate::sdk::validate_session_cwd;
use crate::session::Session;
use anyhow::Result;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncBufRead, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;

const VERSION: &str = env!("CARGO_PKG_VERSION");
/// Longest inbound line we will buffer (embedded resources can be large).
const MAX_LINE_BYTES: usize = 4 * 1024 * 1024;
/// Idle sessions kept live (each holds its history and MCP server
/// processes); older ones are dropped and reloaded from disk on use.
const MAX_LIVE_SESSIONS: usize = 8;
const ALLOW_ONCE: &str = "allow-once";
const REJECT_ONCE: &str = "reject-once";

/// What the ACP server owns per live session.
struct SessionHandle {
    turn_tx: mpsc::UnboundedSender<String>,
    approval_in: mpsc::UnboundedSender<(String, Option<String>)>,
    cancel: Arc<CancelSignal>,
    /// JSON-RPC id of the in-flight `session/prompt`, if any.
    prompt_id: Option<Value>,
    cancel_requested: bool,
    last_used: std::time::Instant,
    /// The `session/new` or `session/load` params (`cwd`, `mcpServers`),
    /// to start the session again after an eviction.
    params: Value,
}

/// A `session/request_permission` we sent and are waiting on.
struct PendingPermission {
    session_id: String,
    approval_id: String,
    tool_use_id: String,
}

type TurnDone = (String, Result<TurnEnd, String>);

pub struct AcpServer;

struct State {
    config: Config,
    /// Where sessions are saved after each turn and `session/load` reads them.
    sessions_dir: PathBuf,
    initialized: bool,
    sessions: HashMap<String, SessionHandle>,
    /// Sessions dropped to bound memory and MCP processes, with their start
    /// params: a prompt for one reloads it from disk.
    evicted: HashMap<String, Value>,
    max_live: usize,
    pending: HashMap<String, PendingPermission>,
    next_id: i64,
    notif_tx: mpsc::UnboundedSender<SdkNotification>,
    done_tx: mpsc::UnboundedSender<TurnDone>,
}

impl AcpServer {
    /// Serve ACP over `reader`/`writer` until the reader hits EOF. Sessions
    /// live in the normal sessions directory, shared with the TUI's /resume.
    pub async fn run<R, W>(config: Config, reader: R, writer: W) -> Result<()>
    where
        R: AsyncBufRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin,
    {
        Self::serve(config, Config::sessions_dir(), reader, writer).await
    }

    /// `run`, saving and loading sessions in `sessions_dir`.
    pub async fn serve<R, W>(
        config: Config,
        sessions_dir: PathBuf,
        reader: R,
        mut writer: W,
    ) -> Result<()>
    where
        R: AsyncBufRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin,
    {
        let (notif_tx, mut notif_rx) = mpsc::unbounded_channel::<SdkNotification>();
        let (done_tx, mut done_rx) = mpsc::unbounded_channel::<TurnDone>();
        let mut st = State {
            config,
            sessions_dir,
            initialized: false,
            sessions: HashMap::new(),
            evicted: HashMap::new(),
            max_live: MAX_LIVE_SESSIONS,
            pending: HashMap::new(),
            next_id: 1,
            notif_tx,
            done_tx,
        };
        // A dedicated reader task: racing `read_line` itself in the select
        // loses a partly-read line whenever a notification wins.
        let mut lines = spawn_line_reader(reader, MAX_LINE_BYTES);
        loop {
            // Biased so done_rx is polled only once notif_rx is empty: a
            // session task queues a turn's last updates before its TurnDone,
            // and the prompt response must follow every one of them.
            let frames = tokio::select! {
                biased;
                read = lines.recv() => {
                    let Some(read) = read else { break };
                    match read? {
                        (LineRead::Eof, _) => break,
                        // The id is recovered where the line's start holds
                        // one: with null, the client cannot match the error
                        // to its request, and a session/prompt spins forever.
                        (LineRead::TooLong, start) => vec![rpc::error(
                            &request_id_from_prefix(&start).unwrap_or(Value::Null),
                            rpc::PARSE_ERROR,
                            format!("line exceeds {} MB", MAX_LINE_BYTES / (1024 * 1024)),
                        )],
                        (LineRead::InvalidUtf8, lossy) => vec![rpc::error(
                            &request_id_from_prefix(&lossy).unwrap_or(Value::Null),
                            rpc::PARSE_ERROR,
                            "line is not valid UTF-8",
                        )],
                        (LineRead::Line, line) => {
                            let line = line.trim();
                            if line.is_empty() {
                                continue;
                            }
                            st.handle_line(line).await
                        }
                    }
                }
                Some(n) = notif_rx.recv() => st.handle_sdk_notification(n),
                Some(d) = done_rx.recv() => st.handle_turn_done(d),
            };
            for f in frames {
                send(&mut writer, &f).await?;
            }
        }
        Ok(())
    }
}

async fn send<W: AsyncWrite + Unpin>(w: &mut W, frame: &Value) -> Result<()> {
    let mut line = serde_json::to_string(frame)?;
    line.push('\n');
    w.write_all(line.as_bytes()).await?;
    w.flush().await?;
    Ok(())
}

impl State {
    async fn handle_line(&mut self, line: &str) -> Vec<Value> {
        match rpc::parse(line) {
            Err(e) => vec![rpc::error(&Value::Null, e.code, e.message)],
            Ok(Incoming::Request { id, method, params }) => {
                match self.handle_request(&id, &method, &params).await {
                    Ok(frames) => frames,
                    Err(e) => vec![rpc::error(&id, e.code, e.message)],
                }
            }
            Ok(Incoming::Notification { method, params }) => match method.as_str() {
                "session/cancel" => self.handle_cancel(&params),
                other => {
                    tracing::debug!("acp: ignoring notification {other}");
                    vec![]
                }
            },
            Ok(Incoming::Response { id, result, error }) => {
                self.handle_client_response(&id, result, error)
            }
        }
    }

    async fn handle_request(
        &mut self,
        id: &Value,
        method: &str,
        params: &Value,
    ) -> Result<Vec<Value>, RpcError> {
        match method {
            "initialize" => {
                self.initialized = true;
                let load = !self.config.no_session_persistence;
                Ok(vec![rpc::response(id, initialize_result(load))])
            }
            "authenticate" => Ok(vec![rpc::response(id, json!({}))]),
            "session/new" => {
                self.require_initialized(method)?;
                let (sid, _) = self.start_session(params, None, None).await?;
                Ok(vec![rpc::response(id, json!({"sessionId": sid}))])
            }
            // Advertised as unsupported: nothing is saved to load.
            "session/load" if self.config.no_session_persistence => Err(RpcError::new(
                rpc::METHOD_NOT_FOUND,
                "session/load is off: sessions are not saved (--no-session-persistence)",
            )),
            "session/load" => {
                self.require_initialized(method)?;
                self.load_session(id, params).await
            }
            "session/prompt" => {
                self.start_prompt(id, params).await?;
                Ok(vec![]) // answered when the turn ends
            }
            "session/close" => self.close_session(id, params),
            other => Err(RpcError::new(
                rpc::METHOD_NOT_FOUND,
                format!("{other} is not supported by this agent"),
            )),
        }
    }

    fn require_initialized(&self, method: &str) -> Result<(), RpcError> {
        if self.initialized {
            return Ok(());
        }
        Err(RpcError::new(
            rpc::INVALID_REQUEST,
            format!("call initialize before {method}"),
        ))
    }

    /// `session/load`: replay the saved conversation as `session/update`s,
    /// then answer, then take prompts on it like any other session.
    async fn load_session(&mut self, id: &Value, params: &Value) -> Result<Vec<Value>, RpcError> {
        let sid = params
            .get("sessionId")
            .and_then(Value::as_str)
            .ok_or_else(|| RpcError::new(rpc::INVALID_PARAMS, "sessionId is required"))?;
        // Loading replaces a live copy; a turn still running would keep
        // writing the transcript underneath the new one.
        if self
            .sessions
            .get(sid)
            .is_some_and(|h| h.prompt_id.is_some())
        {
            return Err(RpcError::new(
                rpc::BUSY,
                "a prompt is in progress for this session",
            ));
        }
        if !Session::exists_in(&self.sessions_dir, sid) {
            return Err(RpcError::new(
                rpc::RESOURCE_NOT_FOUND,
                format!("no saved session {sid}"),
            ));
        }
        let (saved, history) = Session::resume_in(&self.sessions_dir, sid)
            .await
            .map_err(|e| RpcError::new(rpc::INTERNAL_ERROR, format!("{e:#}")))?;
        let (_, replay) = self
            .start_session(params, Some((saved, history)), None)
            .await?;
        let mut frames: Vec<Value> = replay
            .into_iter()
            .map(|p| rpc::notification("session/update", p))
            .collect();
        frames.push(rpc::response(id, json!({})));
        Ok(frames)
    }

    /// Start a session for `session/new`, or for `session/load` with the
    /// saved transcript and its history. Returns the session id and, for a
    /// load, the `session/update` params that replay the history. `id` keeps
    /// the id of an evicted session that was never saved (never prompted).
    async fn start_session(
        &mut self,
        params: &Value,
        saved: Option<(Session, Vec<Message>)>,
        id: Option<String>,
    ) -> Result<(String, Vec<Value>), RpcError> {
        let cwd = params
            .get("cwd")
            .and_then(Value::as_str)
            .ok_or_else(|| RpcError::new(rpc::INVALID_PARAMS, "cwd is required"))?;
        let dir = validate_session_cwd(Some(cwd.to_string()))
            .map_err(|m| RpcError::new(rpc::INVALID_PARAMS, m))?
            .expect("Some(cwd) validates to Some(dir)");
        let mut cfg = self.config.clone();
        cfg.retarget_cwd(dir);
        // routerBudget caps the session like the SDK, -p and the TUI;
        // SdkSession reads only max_budget_usd.
        cfg.max_budget_usd = cfg.session_budget();
        cfg.extra_mcp_servers.extend(mcp_servers(params));
        // Starting servers here stalls other sessions' updates for up to the
        // per-server startup timeout; acceptable for a once-per-session cost.
        let tools = crate::mcp::tools_for_config(&cfg).await;
        let persist = !cfg.no_session_persistence;
        // The session's own project settings decide, as in a live turn.
        let show_thinking = cfg.show_thinking_summaries;
        let (approval_in_tx, approval_in_rx) = mpsc::unbounded_channel();
        let mut session = SdkSession::new(
            cfg,
            tools,
            acp_policy(),
            Capabilities::default(),
            self.notif_tx.clone(),
            self.notif_tx.clone(),
            approval_in_rx,
        )
        .map_err(|e| RpcError::new(rpc::INTERNAL_ERROR, format!("{e:#}")))?;
        let mut transcript = Transcript {
            dir: self.sessions_dir.clone(),
            file: None,
            saved: 0,
            rewrite: false,
        };
        let mut replay = Vec::new();
        if let Some((file, history)) = saved {
            replay = replay_updates(&file.id, &history, show_thinking);
            transcript.saved = history.len();
            session.resume_history(file.id.clone(), history);
            transcript.file = Some(file);
        } else if let Some(id) = id {
            // The Transcript creates its file on the first save, as for a
            // new session.
            session.resume_history(id, Vec::new());
        }
        let session_id = session.session_id.clone();
        let cancel = session.cancel_signal();
        let (turn_tx, mut turn_rx) = mpsc::unbounded_channel::<String>();
        let done_tx = self.done_tx.clone();
        let sid = session_id.clone();
        tokio::spawn(async move {
            let mut session = session;
            while let Some(prompt) = turn_rx.recv().await {
                let r = session
                    .execute_turn(prompt)
                    .await
                    .map_err(|e| format!("{e:#}"));
                // Before the answer: a session/load sent right after it
                // must find the whole turn on disk.
                if persist {
                    transcript.save(&mut session).await;
                }
                if done_tx.send((sid.clone(), r)).is_err() {
                    break;
                }
            }
        });
        self.evict_idle_sessions(&session_id);
        self.evicted.remove(&session_id);
        // On a load this drops any live copy, whose task then ends.
        self.sessions.insert(
            session_id.clone(),
            SessionHandle {
                turn_tx,
                approval_in: approval_in_tx,
                cancel,
                prompt_id: None,
                cancel_requested: false,
                last_used: std::time::Instant::now(),
                params: json!({"cwd": params.get("cwd"), "mcpServers": params.get("mcpServers")}),
            },
        );
        Ok((session_id, replay))
    }

    /// Make room for one more live session: an editor that keeps the agent
    /// running opens a session per thread, and each kept its history and MCP
    /// server processes until it quit. Only idle sessions go: a later prompt
    /// or `session/load` restores one from disk whole, and one never
    /// prompted (an opened thread panel) starts again under its id. Without
    /// persistence nothing could restore them, so nothing is evicted.
    fn evict_idle_sessions(&mut self, incoming: &str) {
        if self.config.no_session_persistence {
            return;
        }
        let live = self.sessions.len() - usize::from(self.sessions.contains_key(incoming));
        let excess = (live + 1).saturating_sub(self.max_live);
        if excess == 0 {
            return;
        }
        let mut idle: Vec<(std::time::Instant, String)> = self
            .sessions
            .iter()
            .filter(|(sid, h)| *sid != incoming && h.prompt_id.is_none())
            .map(|(sid, h)| (h.last_used, sid.clone()))
            .collect();
        idle.sort();
        for (_, sid) in idle.into_iter().take(excess) {
            // Dropping the handle drops `turn_tx`, so the session task ends
            // and its tools (and their MCP children) go with it.
            if let Some(h) = self.sessions.remove(&sid) {
                self.evicted.insert(sid, h.params);
            }
        }
    }

    /// `session/close`: drop a session the editor is done with, ending its
    /// MCP servers. It stays on disk for a later `session/load`.
    fn close_session(&mut self, id: &Value, params: &Value) -> Result<Vec<Value>, RpcError> {
        let sid = params
            .get("sessionId")
            .and_then(Value::as_str)
            .ok_or_else(|| RpcError::new(rpc::INVALID_PARAMS, "sessionId is required"))?;
        match self.sessions.get(sid) {
            Some(h) if h.prompt_id.is_some() => {
                return Err(RpcError::new(
                    rpc::BUSY,
                    "a prompt is in progress for this session",
                ));
            }
            Some(_) => {
                self.sessions.remove(sid);
            }
            None if self.evicted.remove(sid).is_some() => {}
            None => {
                return Err(RpcError::new(
                    rpc::RESOURCE_NOT_FOUND,
                    format!("no live session {sid}"),
                ));
            }
        }
        Ok(vec![rpc::response(id, json!({}))])
    }

    async fn start_prompt(&mut self, id: &Value, params: &Value) -> Result<(), RpcError> {
        let sid = params
            .get("sessionId")
            .and_then(Value::as_str)
            .ok_or_else(|| RpcError::new(rpc::INVALID_PARAMS, "sessionId is required"))?;
        let text = prompt_text(params.get("prompt").unwrap_or(&Value::Null))?;
        if let Some(start) = self.evicted.get(sid).cloned() {
            // The editor still shows the thread: reload it as it was, with
            // no replay, since the client already has the conversation.
            if Session::exists_in(&self.sessions_dir, sid) {
                let (saved, history) = Session::resume_in(&self.sessions_dir, sid)
                    .await
                    .map_err(|e| RpcError::new(rpc::INTERNAL_ERROR, format!("{e:#}")))?;
                self.start_session(&start, Some((saved, history)), None)
                    .await?;
            } else {
                // Never prompted, so never saved: start it fresh, same id.
                self.start_session(&start, None, Some(sid.to_string()))
                    .await?;
            }
        }
        let h = self
            .sessions
            .get_mut(sid)
            .ok_or_else(|| RpcError::new(rpc::INVALID_PARAMS, format!("unknown session {sid}")))?;
        if h.prompt_id.is_some() {
            return Err(RpcError::new(
                rpc::BUSY,
                "a prompt is already in progress for this session",
            ));
        }
        h.cancel.reset();
        h.cancel_requested = false;
        h.prompt_id = Some(id.clone());
        h.last_used = std::time::Instant::now();
        h.turn_tx
            .send(text)
            .map_err(|_| RpcError::new(rpc::INTERNAL_ERROR, "session task has ended"))?;
        Ok(())
    }

    fn handle_cancel(&mut self, params: &Value) -> Vec<Value> {
        let Some(sid) = params.get("sessionId").and_then(Value::as_str) else {
            return vec![];
        };
        let Some(h) = self.sessions.get_mut(sid) else {
            return vec![];
        };
        if h.prompt_id.is_none() {
            return vec![]; // nothing running: the spec says ignore
        }
        h.cancel_requested = true;
        h.cancel.cancel();
        // Any permission prompt still open for this session is now moot.
        let stale: Vec<String> = self
            .pending
            .iter()
            .filter(|(_, p)| p.session_id == sid)
            .map(|(k, _)| k.clone())
            .collect();
        let mut frames = Vec::new();
        for k in stale {
            if let Some(p) = self.pending.remove(&k) {
                let _ = h
                    .approval_in
                    .send((p.approval_id, Some("Cancelled by the client.".into())));
                frames.push(update(
                    sid,
                    json!({
                        "sessionUpdate": "tool_call_update",
                        "toolCallId": p.tool_use_id,
                        "status": "failed",
                    }),
                ));
            }
        }
        frames
    }

    fn handle_client_response(
        &mut self,
        id: &Value,
        result: Option<Value>,
        error: Option<Value>,
    ) -> Vec<Value> {
        let Some(p) = self.pending.remove(&id.to_string()) else {
            tracing::debug!("acp: response to unknown request id {id}");
            return vec![];
        };
        let deny = match (result, error) {
            (Some(r), _) => permission_outcome(&r),
            (None, Some(e)) => Some(format!("Permission request failed: {e}")),
            (None, None) => Some("Empty permission response.".into()),
        };
        let status = if deny.is_none() {
            "in_progress"
        } else {
            "failed"
        };
        if let Some(h) = self.sessions.get(&p.session_id) {
            let _ = h.approval_in.send((p.approval_id, deny));
        }
        vec![update(
            &p.session_id,
            json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": p.tool_use_id,
                "status": status,
            }),
        )]
    }

    fn handle_sdk_notification(&mut self, n: SdkNotification) -> Vec<Value> {
        if let SdkNotification::ToolApprovalNeeded {
            session_id,
            approval_id,
            tool,
            args,
            tool_use_id,
        } = n
        {
            let call = json!({
                "toolCallId": tool_use_id,
                "title": tool_title(&tool, &args),
                "kind": tool_kind(&tool),
                "status": "pending",
                "rawInput": args,
            });
            let mut announce = call.clone();
            announce["sessionUpdate"] = json!("tool_call");
            // Lines are read ahead of queued notifications, so a request
            // can arrive after its turn was cancelled: answer it here
            // rather than open a dialog the user already dismissed.
            if let Some(h) = self
                .sessions
                .get(&session_id)
                .filter(|h| h.cancel_requested)
            {
                let _ = h
                    .approval_in
                    .send((approval_id, Some("Cancelled by the client.".into())));
                announce["status"] = json!("failed");
                return vec![update(&session_id, announce)];
            }
            let rpc_id = self.next_id;
            self.next_id += 1;
            self.pending.insert(
                Value::from(rpc_id).to_string(),
                PendingPermission {
                    session_id: session_id.clone(),
                    approval_id,
                    tool_use_id: tool_use_id.clone(),
                },
            );
            return vec![
                update(&session_id, announce),
                rpc::request(
                    rpc_id,
                    "session/request_permission",
                    json!({
                        "sessionId": session_id,
                        "toolCall": call,
                        "options": [
                            {"optionId": ALLOW_ONCE, "name": "Allow", "kind": "allow_once"},
                            {"optionId": REJECT_ONCE, "name": "Reject", "kind": "reject_once"},
                        ],
                    }),
                ),
            ];
        }
        if let SdkNotification::ToolCompleted {
            session_id,
            tool_use_id,
            ..
        } = &n
        {
            // The call is over (an approval timeout or a sub-agent's denial
            // ends it here). A late Allow must not revive it as in_progress.
            self.pending
                .retain(|_, p| !(p.session_id == *session_id && p.tool_use_id == *tool_use_id));
        }
        session_updates(&n)
            .into_iter()
            .map(|params| rpc::notification("session/update", params))
            .collect()
    }

    fn handle_turn_done(&mut self, (sid, result): TurnDone) -> Vec<Value> {
        // Nothing awaits an approval once the turn is over.
        self.pending.retain(|_, p| p.session_id != sid);
        let Some(h) = self.sessions.get_mut(&sid) else {
            return vec![];
        };
        let Some(id) = h.prompt_id.take() else {
            return vec![];
        };
        let cancelled = std::mem::take(&mut h.cancel_requested);
        h.cancel.reset();
        h.last_used = std::time::Instant::now();
        if cancelled {
            return vec![rpc::response(&id, json!({"stopReason": "cancelled"}))];
        }
        match result {
            Ok(end) => vec![rpc::response(&id, json!({"stopReason": stop_reason(end)}))],
            Err(msg) => vec![rpc::error(&id, rpc::INTERNAL_ERROR, msg)],
        }
    }
}

/// A `session/update` notification frame.
fn update(session_id: &str, update: Value) -> Value {
    rpc::notification(
        "session/update",
        json!({"sessionId": session_id, "update": update}),
    )
}

/// An ACP session's conversation on disk, in the sessions directory the TUI
/// also uses, so `session/load` (or /resume) can continue it later.
struct Transcript {
    dir: PathBuf,
    /// Created by the first save: a session never prompted leaves no file.
    file: Option<Session>,
    /// How many history messages are on disk.
    saved: usize,
    /// Compaction replaced saved messages: the next save rewrites the file.
    rewrite: bool,
}

impl Transcript {
    /// Save what the last turn added. A failure is logged and retried with
    /// the next turn, as in the TUI.
    async fn save(&mut self, session: &mut SdkSession) {
        self.rewrite |= session.take_history_rewritten();
        let history = session.history();
        if !self.rewrite && history.len() <= self.saved {
            return;
        }
        let file = match &mut self.file {
            Some(f) => f,
            None => match Session::create_in(&self.dir, session.session_id.clone()).await {
                Ok(f) => self.file.insert(f),
                Err(e) => {
                    tracing::warn!("acp: could not save session {}: {e:#}", session.session_id);
                    return;
                }
            },
        };
        let written = if self.rewrite {
            file.overwrite(history).await
        } else {
            file.append(&history[self.saved..]).await
        };
        match written {
            Ok(()) => {
                self.saved = history.len();
                self.rewrite = false;
                // A loaded TUI session: these turns are not on its undo
                // timeline (see `Session::end_timeline`).
                if let Err(e) = file.end_timeline().await {
                    tracing::warn!(
                        "acp: could not end the undo timeline of {}: {e:#}",
                        session.session_id
                    );
                }
            }
            Err(e) => tracing::warn!("acp: could not save session {}: {e:#}", session.session_id),
        }
    }
}

// ── pure translation helpers (unit-tested) ──────────────────────────────────

/// The `initialize` result: what we can and cannot do. `load_session` is
/// false when sessions are not saved (`--no-session-persistence`).
pub(crate) fn initialize_result(load_session: bool) -> Value {
    json!({
        "protocolVersion": super::PROTOCOL_VERSION,
        "agentCapabilities": {
            "loadSession": load_session,
            "promptCapabilities": {"image": false, "audio": false, "embeddedContext": true},
            // `http` is Streamable HTTP; `sse` is MCP's older HTTP+SSE.
            "mcpCapabilities": {"http": true, "sse": true},
        },
        "agentInfo": {"name": "oxideclaw", "title": "OxideClaw", "version": VERSION},
        "authMethods": [],
    })
}

/// The SDK policy of an ACP session: every tool asks, and a permission
/// dialog waits for the user, however long they read the diff. ACP has no
/// way to withdraw a `session/request_permission`, so a timeout left the
/// dialog open after the tool was refused and dropped the user's Allow;
/// `session/cancel` is how the editor gives up on one.
pub(crate) fn acp_policy() -> Policy {
    Policy {
        // `await_approval` caps an unrepresentable deadline at decades.
        approval_timeout_seconds: u64::MAX,
        ..Policy::default()
    }
}

/// ACP `ToolKind` for one of our tool names.
pub(crate) fn tool_kind(tool: &str) -> &'static str {
    if tool.starts_with("browser_") {
        return "fetch";
    }
    match tool {
        "Read" | "NotebookRead" | "ReadMcpResource" | "ListMcpResources" | "LSP" => "read",
        "Edit" | "MultiEdit" | "Write" | "NotebookEdit" => "edit",
        "Glob" | "Grep" | "ToolSearch" | "DiscoverSkills" => "search",
        "Bash" | "PowerShell" => "execute",
        "WebFetch" | "WebSearch" | "WebBrowser" => "fetch",
        "EnterPlanMode" | "ExitPlanMode" | "TodoWrite" | "Agent" | "Workflow" | "TaskCreate"
        | "TaskUpdate" | "TaskList" | "TaskGet" => "think",
        _ => "other",
    }
}

/// One-line human title for a tool call.
pub(crate) fn tool_title(tool: &str, args: &Value) -> String {
    const KEYS: [&str; 9] = [
        "command",
        "file_path",
        "path",
        "notebook_path",
        "pattern",
        "query",
        "url",
        "prompt",
        "description",
    ];
    let target = KEYS
        .iter()
        .find_map(|k| args.get(k).and_then(Value::as_str))
        .map(|v| {
            let one_line: String = v.split_whitespace().collect::<Vec<_>>().join(" ");
            if one_line.chars().count() > 80 {
                let cut: String = one_line.chars().take(77).collect();
                format!("{cut}...")
            } else {
                one_line
            }
        });
    match target {
        Some(t) if !t.is_empty() => format!("{tool}: {t}"),
        _ => tool.to_string(),
    }
}

/// Flatten the `prompt` content blocks into the text we send the model.
pub(crate) fn prompt_text(blocks: &Value) -> Result<String, RpcError> {
    let Some(blocks) = blocks.as_array() else {
        return Err(RpcError::new(
            rpc::INVALID_PARAMS,
            "prompt must be an array of content blocks",
        ));
    };
    let mut parts: Vec<String> = Vec::new();
    for b in blocks {
        match b.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(t) = b.get("text").and_then(Value::as_str) {
                    parts.push(t.to_string());
                }
            }
            Some("resource_link") => {
                let uri = b.get("uri").and_then(Value::as_str).unwrap_or("");
                let name = b.get("name").and_then(Value::as_str).unwrap_or(uri);
                parts.push(format!("[{name}]({uri})"));
            }
            Some("resource") => {
                let r = b.get("resource").cloned().unwrap_or(Value::Null);
                let uri = r.get("uri").and_then(Value::as_str).unwrap_or("resource");
                match r.get("text").and_then(Value::as_str) {
                    Some(text) => parts.push(format!("<file uri=\"{uri}\">\n{text}\n</file>")),
                    None => parts.push(format!("[binary resource {uri} omitted]")),
                }
            }
            Some("image") | Some("audio") => {
                return Err(RpcError::new(
                    rpc::INVALID_PARAMS,
                    "image and audio prompts are not supported (see promptCapabilities)",
                ));
            }
            _ => {}
        }
    }
    let text = parts.join("\n");
    if text.trim().is_empty() {
        return Err(RpcError::new(rpc::INVALID_PARAMS, "prompt is empty"));
    }
    Ok(text)
}

/// The `mcpServers` entries of `session/new` / `session/load`: stdio,
/// `http` (Streamable HTTP) and `sse` (MCP's legacy HTTP+SSE). Anything
/// else is skipped with a warning.
pub(crate) fn mcp_servers(params: &Value) -> Vec<(String, McpServerConfig)> {
    // `env` and `headers` are both arrays of {name, value}.
    let pairs = |v: Option<&Value>| -> HashMap<String, String> {
        v.and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|kv| {
                Some((
                    kv.get("name")?.as_str()?.to_string(),
                    kv.get("value")?.as_str()?.to_string(),
                ))
            })
            .collect()
    };
    let mut out = Vec::new();
    let entries = params.get("mcpServers").and_then(Value::as_array);
    for s in entries.into_iter().flatten() {
        let name = s.get("name").and_then(Value::as_str).unwrap_or("mcp");
        let kind = s.get("type").and_then(Value::as_str);
        let server = match (kind, s.get("command").and_then(Value::as_str)) {
            (Some(t @ ("http" | "sse")), _) => match s.get("url").and_then(Value::as_str) {
                Some(url) => McpServerConfig::Http(HttpServerConfig {
                    url: url.to_string(),
                    headers: pairs(s.get("headers")),
                    disabled: false,
                    literal: true,
                    sse: t == "sse",
                }),
                None => {
                    tracing::warn!("acp: ignoring {t} MCP server {name} without a url");
                    continue;
                }
            },
            (None | Some("stdio"), Some(command)) => McpServerConfig::Stdio(StdioServerConfig {
                command: command.to_string(),
                args: s
                    .get("args")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect(),
                env: pairs(s.get("env")),
                disabled: false,
                literal: true,
            }),
            (kind, _) => {
                tracing::warn!(
                    "acp: ignoring MCP server {name}: transport {} is not supported",
                    kind.unwrap_or("stdio without a command")
                );
                continue;
            }
        };
        out.push((name.to_string(), server));
    }
    out
}

/// `session/update` params that replay a saved conversation, in order:
/// user text, the agent's text and thoughts, and each tool call followed
/// by its result, shaped as a live turn reports them. Thoughts are replayed
/// only with `show_thinking`, as a live turn sends them only with
/// `showThinkingSummaries`.
pub(crate) fn replay_updates(
    session_id: &str,
    history: &[Message],
    show_thinking: bool,
) -> Vec<Value> {
    let params = |u: Value| json!({"sessionId": session_id, "update": u});
    let chunk = |kind: &str, text: &str| {
        params(json!({"sessionUpdate": kind, "content": {"type": "text", "text": text}}))
    };
    let mut out = Vec::new();
    for msg in history {
        for block in &msg.content {
            match (&msg.role, block) {
                // Retrieved code context rides in the user turn; the user
                // never typed it.
                (Role::User, ContentBlock::Text { text })
                    if !text.trim().is_empty() && !text.starts_with("<codebase_context>") =>
                {
                    out.push(chunk("user_message_chunk", text));
                }
                (Role::Assistant, ContentBlock::Text { text }) if !text.trim().is_empty() => {
                    out.push(chunk("agent_message_chunk", text));
                }
                (Role::Assistant, ContentBlock::Thinking { thinking, .. })
                    if show_thinking && !thinking.trim().is_empty() =>
                {
                    out.push(chunk("agent_thought_chunk", thinking));
                }
                (Role::Assistant, ContentBlock::ToolUse { id, name, input }) => {
                    out.push(params(json!({
                        "sessionUpdate": "tool_call",
                        "toolCallId": id,
                        "title": tool_title(name, input),
                        "kind": tool_kind(name),
                        "status": "in_progress",
                        "rawInput": input,
                    })));
                }
                (
                    _,
                    ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        is_error,
                    },
                ) => {
                    let text = content
                        .iter()
                        .map(|ToolResultContent::Text { text }| text.as_str())
                        .collect::<Vec<_>>()
                        .join("\n");
                    out.push(params(json!({
                        "sessionUpdate": "tool_call_update",
                        "toolCallId": tool_use_id,
                        "status": if is_error.unwrap_or(false) { "failed" } else { "completed" },
                        "content": [{"type": "content", "content": {"type": "text", "text": clip(&text, 500)}}],
                    })));
                }
                _ => {}
            }
        }
    }
    out
}

/// At most `max` bytes of `s`, cut on a char boundary and marked, the way
/// a live tool result's summary is.
fn clip(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &s[..end])
}

/// `None` = approved; `Some(reason)` = denied.
pub(crate) fn permission_outcome(result: &Value) -> Option<String> {
    let outcome = result.get("outcome").unwrap_or(&Value::Null);
    match outcome.get("outcome").and_then(Value::as_str) {
        Some("selected") => {
            let allowed = outcome
                .get("optionId")
                .and_then(Value::as_str)
                .is_some_and(|o| o.starts_with("allow"));
            (!allowed).then(|| "Rejected by the user.".to_string())
        }
        Some("cancelled") => Some("Cancelled by the client.".into()),
        _ => Some("Unrecognised permission outcome.".into()),
    }
}

pub(crate) fn stop_reason(end: TurnEnd) -> &'static str {
    match end {
        TurnEnd::EndTurn => "end_turn",
        TurnEnd::MaxTokens => "max_tokens",
        // The `/budget` cap ends a turn like the request cap does; `refusal`
        // is the model declining, which an editor shows (and may roll back)
        // as such.
        TurnEnd::MaxTurns | TurnEnd::BudgetExceeded => "max_turn_requests",
        TurnEnd::Refusal => "refusal",
        TurnEnd::Cancelled => "cancelled",
    }
}

/// `session/update` params for an SDK notification (empty when ACP has no
/// equivalent). Approval requests are handled separately.
pub(crate) fn session_updates(n: &SdkNotification) -> Vec<Value> {
    let params = |sid: &str, u: Value| json!({"sessionId": sid, "update": u});
    match n {
        SdkNotification::MessageDelta {
            session_id,
            content,
        } => vec![params(
            session_id,
            json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": content}}),
        )],
        SdkNotification::ThinkingDelta {
            session_id,
            content,
        } => vec![params(
            session_id,
            json!({"sessionUpdate": "agent_thought_chunk", "content": {"type": "text", "text": content}}),
        )],
        SdkNotification::ToolStarted {
            session_id,
            tool,
            args,
            tool_use_id,
        } => vec![params(
            session_id,
            json!({
                "sessionUpdate": "tool_call",
                "toolCallId": tool_use_id,
                "title": tool_title(tool, args),
                "kind": tool_kind(tool),
                "status": "in_progress",
                "rawInput": args,
            }),
        )],
        SdkNotification::ToolCompleted {
            session_id,
            tool_use_id,
            success,
            output_summary,
            ..
        } => {
            vec![params(
                session_id,
                json!({
                    "sessionUpdate": "tool_call_update",
                    "toolCallId": tool_use_id,
                    "status": if *success { "completed" } else { "failed" },
                    "content": [{"type": "content", "content": {"type": "text", "text": output_summary}}],
                }),
            )]
        }
        SdkNotification::Error {
            session_id,
            code,
            message,
        } => vec![params(
            session_id,
            json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": format!("\n[{code}] {message}\n")}}),
        )],
        _ => vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── pure helpers ─────────────────────────────────────────────────────

    #[test]
    fn initialize_advertises_exactly_what_we_support() {
        let r = initialize_result(true);
        assert_eq!(r["protocolVersion"], json!(1));
        assert_eq!(r["agentCapabilities"]["loadSession"], json!(true));
        assert_eq!(
            r["agentCapabilities"]["promptCapabilities"]["image"],
            json!(false)
        );
        assert_eq!(
            r["agentCapabilities"]["promptCapabilities"]["audio"],
            json!(false)
        );
        assert_eq!(
            r["agentCapabilities"]["promptCapabilities"]["embeddedContext"],
            json!(true)
        );
        assert_eq!(
            r["agentCapabilities"]["mcpCapabilities"],
            json!({"http": true, "sse": true})
        );
        assert_eq!(r["agentInfo"]["name"], json!("oxideclaw"));
        assert_eq!(r["agentInfo"]["version"], json!(VERSION));
        assert_eq!(r["authMethods"], json!([]));
    }

    #[test]
    fn tool_kinds_follow_the_acp_taxonomy() {
        assert_eq!(tool_kind("Read"), "read");
        assert_eq!(tool_kind("Edit"), "edit");
        assert_eq!(tool_kind("Write"), "edit");
        assert_eq!(tool_kind("Grep"), "search");
        assert_eq!(tool_kind("Bash"), "execute");
        assert_eq!(tool_kind("PowerShell"), "execute");
        assert_eq!(tool_kind("WebFetch"), "fetch");
        assert_eq!(tool_kind("browser_click"), "fetch");
        assert_eq!(tool_kind("EnterPlanMode"), "think");
        assert_eq!(tool_kind("SomethingNew"), "other");
    }

    #[test]
    fn tool_titles_name_the_target_and_stay_short() {
        assert_eq!(
            tool_title("Bash", &json!({"command": "cargo test"})),
            "Bash: cargo test"
        );
        assert_eq!(
            tool_title("Read", &json!({"file_path": "/a/b.rs"})),
            "Read: /a/b.rs"
        );
        assert_eq!(tool_title("Glob", &json!({})), "Glob");
        let long = "x".repeat(300);
        let t = tool_title("Bash", &json!({"command": long}));
        assert!(t.chars().count() <= 90, "{}", t.len());
        assert!(!tool_title("Bash", &json!({"command": "a\nb"})).contains('\n'));
    }

    #[test]
    fn prompt_text_flattens_text_links_and_embedded_resources() {
        let blocks = json!([
            {"type": "text", "text": "Fix this"},
            {"type": "resource_link", "uri": "file:///p/a.rs", "name": "a.rs"},
            {"type": "resource", "resource": {"uri": "file:///p/b.rs", "text": "fn b() {}"}},
        ]);
        let t = prompt_text(&blocks).unwrap();
        assert!(t.starts_with("Fix this"), "{t}");
        assert!(t.contains("file:///p/a.rs"), "{t}");
        assert!(t.contains("fn b() {}"), "{t}");
        assert!(t.contains("file:///p/b.rs"), "{t}");
    }

    #[test]
    fn prompt_text_rejects_media_and_empty_prompts() {
        let img = json!([{"type": "image", "data": "...", "mimeType": "image/png"}]);
        assert_eq!(prompt_text(&img).unwrap_err().code, rpc::INVALID_PARAMS);
        assert_eq!(
            prompt_text(&json!([])).unwrap_err().code,
            rpc::INVALID_PARAMS
        );
        assert_eq!(
            prompt_text(&json!("nope")).unwrap_err().code,
            rpc::INVALID_PARAMS
        );
    }

    #[test]
    fn permission_outcomes_map_to_approve_or_a_deny_reason() {
        assert_eq!(
            permission_outcome(
                &json!({"outcome": {"outcome": "selected", "optionId": ALLOW_ONCE}})
            ),
            None
        );
        assert!(
            permission_outcome(
                &json!({"outcome": {"outcome": "selected", "optionId": REJECT_ONCE}})
            )
            .is_some()
        );
        assert!(
            permission_outcome(&json!({"outcome": {"outcome": "cancelled"}}))
                .unwrap()
                .to_lowercase()
                .contains("cancel")
        );
        assert!(permission_outcome(&json!({})).is_some());
    }

    /// A permission dialog the user takes over a minute on still decides
    /// the call: the SDK's 60 s default refused it underneath the dialog.
    #[tokio::test(start_paused = true)]
    async fn an_acp_permission_prompt_never_times_out() {
        let (tx, mut rx) = mpsc::unbounded_channel::<(String, Option<String>)>();
        let timeout = std::time::Duration::from_secs(acp_policy().approval_timeout_seconds);
        let wait = tokio::spawn(async move {
            crate::sdk::session::await_approval(&mut rx, "a1", timeout).await
        });
        // Paused time jumps straight past any finite deadline in this span.
        tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
        assert!(!wait.is_finished(), "the approval timed out");
        tx.send(("a1".into(), None)).unwrap();
        assert!(matches!(
            wait.await.unwrap(),
            crate::sdk::session::ApprovalOutcome::Approved
        ));
    }

    #[test]
    fn stop_reasons_use_the_acp_vocabulary() {
        assert_eq!(stop_reason(TurnEnd::EndTurn), "end_turn");
        assert_eq!(stop_reason(TurnEnd::MaxTokens), "max_tokens");
        assert_eq!(stop_reason(TurnEnd::MaxTurns), "max_turn_requests");
        assert_eq!(stop_reason(TurnEnd::BudgetExceeded), "max_turn_requests");
        assert_eq!(stop_reason(TurnEnd::Refusal), "refusal");
        assert_eq!(stop_reason(TurnEnd::Cancelled), "cancelled");
    }

    #[test]
    fn sdk_notifications_become_session_updates() {
        let sid = "s1".to_string();
        let u = session_updates(&SdkNotification::MessageDelta {
            session_id: sid.clone(),
            content: "hi".into(),
        });
        assert_eq!(u.len(), 1);
        assert_eq!(u[0]["sessionId"], json!("s1"));
        assert_eq!(
            u[0]["update"]["sessionUpdate"],
            json!("agent_message_chunk")
        );
        assert_eq!(
            u[0]["update"]["content"],
            json!({"type": "text", "text": "hi"})
        );

        let u = session_updates(&SdkNotification::ThinkingDelta {
            session_id: sid.clone(),
            content: "hmm".into(),
        });
        assert_eq!(
            u[0]["update"]["sessionUpdate"],
            json!("agent_thought_chunk")
        );

        let u = session_updates(&SdkNotification::ToolStarted {
            session_id: sid.clone(),
            tool: "Bash".into(),
            args: json!({"command": "ls"}),
            tool_use_id: "t1".into(),
        });
        assert_eq!(u[0]["update"]["sessionUpdate"], json!("tool_call"));
        assert_eq!(u[0]["update"]["toolCallId"], json!("t1"));
        assert_eq!(u[0]["update"]["kind"], json!("execute"));
        assert_eq!(u[0]["update"]["status"], json!("in_progress"));
        assert_eq!(u[0]["update"]["title"], json!("Bash: ls"));
        assert_eq!(u[0]["update"]["rawInput"], json!({"command": "ls"}));

        let u = session_updates(&SdkNotification::ToolCompleted {
            session_id: sid.clone(),
            tool: "Bash".into(),
            tool_use_id: "t1".into(),
            success: false,
            output_summary: "boom".into(),
            duration_ms: 3,
        });
        assert_eq!(u[0]["update"]["sessionUpdate"], json!("tool_call_update"));
        assert_eq!(u[0]["update"]["status"], json!("failed"));
        assert_eq!(
            u[0]["update"]["content"][0]["content"]["text"],
            json!("boom")
        );

        // No ACP equivalent: nothing is emitted.
        let u = session_updates(&SdkNotification::ProgressUpdated {
            session_id: sid,
            percent: 1,
            stage: "x".into(),
            tools_executed: 0,
            tools_remaining_estimate: 0,
        });
        assert!(u.is_empty());
    }

    // ── server handshake over an in-memory pipe ──────────────────────────

    /// Sessions go under the test's temp dir, never the user's.
    fn sessions_in(dir: &tempfile::TempDir) -> PathBuf {
        dir.path().join("sessions")
    }

    fn test_config() -> (Config, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config {
            api_key: "sk-ant-test".into(),
            cwd: dir.path().to_path_buf(),
            // session/new starts MCP servers; never the developer's own.
            strict_mcp_config: true,
            ..Default::default()
        };
        (cfg, dir)
    }

    /// Feed `lines`, close stdin, collect every frame the server wrote.
    async fn drive(cfg: Config, lines: &[String]) -> Vec<Value> {
        let mut input = Vec::new();
        for l in lines {
            input.extend_from_slice(l.as_bytes());
            input.push(b'\n');
        }
        drive_raw(cfg, &input).await
    }

    async fn drive_raw(cfg: Config, input: &[u8]) -> Vec<Value> {
        let sessions = tempfile::tempdir().unwrap();
        drive_in(cfg, sessions.path().to_path_buf(), input).await
    }

    async fn drive_in(cfg: Config, sessions: PathBuf, input: &[u8]) -> Vec<Value> {
        use tokio::io::AsyncReadExt;
        let (client, server) = tokio::io::duplex(1 << 20);
        let (srv_r, srv_w) = tokio::io::split(server);
        let (mut cli_r, mut cli_w) = tokio::io::split(client);
        let task = tokio::spawn(AcpServer::serve(
            cfg,
            sessions,
            tokio::io::BufReader::new(srv_r),
            srv_w,
        ));
        cli_w.write_all(input).await.unwrap();
        // A dropped WriteHalf does not close a duplex; shutdown does.
        cli_w.shutdown().await.unwrap();
        drop(cli_w);
        let mut out = String::new();
        cli_r.read_to_string(&mut out).await.unwrap();
        task.await.unwrap().unwrap();
        out.lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("{e}: {l}")))
            .collect()
    }

    fn init_line() -> String {
        json!({"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{}}}).to_string()
    }

    fn new_session_line(id: u64, cwd: &str) -> String {
        json!({"jsonrpc":"2.0","id":id,"method":"session/new","params":{"cwd":cwd,"mcpServers":[]}})
            .to_string()
    }

    #[tokio::test]
    async fn initialize_then_session_new_returns_a_session_id() {
        let (cfg, dir) = test_config();
        let out = drive(
            cfg,
            &[
                init_line(),
                new_session_line(1, &dir.path().to_string_lossy()),
            ],
        )
        .await;
        assert_eq!(out[0]["id"], json!(0));
        assert_eq!(out[0]["result"]["protocolVersion"], json!(1));
        assert_eq!(out[1]["id"], json!(1));
        assert!(out[1]["result"]["sessionId"].is_string(), "{:?}", out[1]);
    }

    #[tokio::test]
    async fn session_new_before_initialize_is_rejected() {
        let (cfg, dir) = test_config();
        let out = drive(cfg, &[new_session_line(1, &dir.path().to_string_lossy())]).await;
        assert_eq!(out[0]["error"]["code"], json!(rpc::INVALID_REQUEST));
    }

    #[tokio::test]
    async fn session_new_with_a_missing_cwd_is_invalid_params() {
        let (cfg, _dir) = test_config();
        let out = drive(
            cfg,
            &[init_line(), new_session_line(1, "/definitely/not/here")],
        )
        .await;
        assert_eq!(out[1]["error"]["code"], json!(rpc::INVALID_PARAMS));
        let (cfg, _dir) = test_config();
        let no_cwd = json!({"jsonrpc":"2.0","id":1,"method":"session/new","params":{}}).to_string();
        let out = drive(cfg, &[init_line(), no_cwd]).await;
        assert_eq!(out[1]["error"]["code"], json!(rpc::INVALID_PARAMS));
    }

    #[tokio::test]
    async fn unsupported_methods_are_method_not_found() {
        let (cfg, _dir) = test_config();
        let mode = json!({"jsonrpc":"2.0","id":1,"method":"session/set_mode","params":{"sessionId":"x","modeId":"m"}}).to_string();
        let weird = json!({"jsonrpc":"2.0","id":2,"method":"does/not/exist"}).to_string();
        let out = drive(cfg, &[init_line(), mode, weird]).await;
        assert_eq!(out[1]["error"]["code"], json!(rpc::METHOD_NOT_FOUND));
        assert_eq!(out[2]["error"]["code"], json!(rpc::METHOD_NOT_FOUND));
    }

    #[tokio::test]
    async fn authenticate_is_a_no_op_success() {
        let (cfg, _dir) = test_config();
        let auth =
            json!({"jsonrpc":"2.0","id":1,"method":"authenticate","params":{"methodId":"none"}})
                .to_string();
        let out = drive(cfg, &[init_line(), auth]).await;
        assert_eq!(out[1]["id"], json!(1));
        assert!(out[1].get("result").is_some(), "{:?}", out[1]);
    }

    #[tokio::test]
    async fn prompt_for_an_unknown_session_and_media_prompts_are_invalid_params() {
        let (cfg, dir) = test_config();
        let bad_session = json!({"jsonrpc":"2.0","id":2,"method":"session/prompt","params":{"sessionId":"nope","prompt":[{"type":"text","text":"hi"}]}}).to_string();
        let out = drive(cfg, &[init_line(), bad_session]).await;
        assert_eq!(out[1]["error"]["code"], json!(rpc::INVALID_PARAMS));

        // Media on a real session: rejected before any model call.
        let (cfg, _keep) = test_config();
        let cwd = dir.path().to_string_lossy().to_string();
        let out = drive(cfg.clone(), &[init_line(), new_session_line(1, &cwd)]).await;
        let sid = out[1]["result"]["sessionId"].as_str().unwrap().to_string();
        let _ = sid; // session ids are per-process; re-create in one run below
        let (cfg, _keep2) = test_config();
        let out = drive(
            cfg,
            &[
                init_line(),
                new_session_line(1, &cwd),
                // We cannot know the id ahead of time, so an unknown-session
                // media prompt must still fail on params, not on the session.
                json!({"jsonrpc":"2.0","id":2,"method":"session/prompt","params":{"sessionId":"nope","prompt":[{"type":"image","data":"","mimeType":"image/png"}]}}).to_string(),
            ],
        )
        .await;
        assert_eq!(out[2]["error"]["code"], json!(rpc::INVALID_PARAMS));
    }

    #[tokio::test]
    async fn garbage_and_cancel_for_an_idle_session_do_not_kill_the_server() {
        let (cfg, _dir) = test_config();
        let cancel =
            json!({"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":"ghost"}})
                .to_string();
        let out = drive(cfg, &["{not json".to_string(), cancel, init_line()]).await;
        assert_eq!(out[0]["error"]["code"], json!(rpc::PARSE_ERROR));
        // The cancel produced nothing; initialize still answered.
        assert_eq!(out[1]["id"], json!(0));
        assert_eq!(out.len(), 2);
    }

    type Seen = Arc<std::sync::Mutex<Vec<String>>>;

    /// A loopback HTTP/1.1 server, one request per connection. `reply` gets
    /// each raw request (head and body) and returns the response's content
    /// type and body, or `None` for an empty 202. Every request is recorded.
    async fn http_stub<F>(reply: F) -> (String, Seen)
    where
        F: Fn(&str) -> Option<(&'static str, String)> + Send + Sync + 'static,
    {
        use tokio::io::AsyncReadExt;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = Seen::default();
        let log = seen.clone();
        let reply = Arc::new(reply);
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let (reply, log) = (reply.clone(), log.clone());
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 4096];
                    loop {
                        let n = sock.read(&mut chunk).await.unwrap_or(0);
                        if n == 0 {
                            break;
                        }
                        buf.extend_from_slice(&chunk[..n]);
                        let text = String::from_utf8_lossy(&buf);
                        if let Some(end) = text.find("\r\n\r\n") {
                            let len = text[..end]
                                .lines()
                                .find_map(|l| {
                                    let (k, v) = l.split_once(':')?;
                                    k.eq_ignore_ascii_case("content-length")
                                        .then(|| v.trim().parse::<usize>().ok())?
                                })
                                .unwrap_or(0);
                            if buf.len() >= end + 4 + len {
                                break;
                            }
                        }
                    }
                    let req = String::from_utf8_lossy(&buf).into_owned();
                    let resp = match reply(&req) {
                        Some((ctype, body)) => format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        ),
                        None => "HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                            .to_string(),
                    };
                    log.lock().unwrap().push(req);
                    let _ = sock.write_all(resp.as_bytes()).await;
                    let _ = sock.shutdown().await;
                });
            }
        });
        (format!("http://{addr}"), seen)
    }

    fn request_body(req: &str) -> &str {
        req.split_once("\r\n\r\n").map_or("", |(_, b)| b)
    }

    /// The chat requests a model stub has seen, oldest first.
    fn chat_requests(seen: &Seen) -> Vec<String> {
        seen.lock()
            .unwrap()
            .iter()
            .filter(|r| r.starts_with("POST /v1/chat/completions"))
            .cloned()
            .collect()
    }

    /// An OpenAI-style SSE stream of `chunks`.
    fn sse(chunks: &[Value]) -> String {
        let mut body: String = chunks.iter().map(|c| format!("data: {c}\n\n")).collect();
        body.push_str("data: [DONE]\n\n");
        body
    }

    /// Ollama stand-in that answers every chat request with `text`.
    async fn text_model(text: &'static str) -> (String, Seen) {
        http_stub(move |_| {
            Some((
                "text/event-stream",
                sse(&[
                    json!({"choices":[{"index":0,"delta":{"content":text},"finish_reason":"stop"}]}),
                ]),
            ))
        })
        .await
    }

    /// Ollama stand-in whose every reply is cut off at the token limit, so
    /// each turn ends with an Error notification right before it returns.
    async fn max_tokens_model() -> String {
        http_stub(|_| {
            Some((
                "text/event-stream",
                sse(&[
                    json!({"choices":[{"index":0,"delta":{"content":"hi"},"finish_reason":"length"}]}),
                ]),
            ))
        })
        .await
        .0
    }

    /// A model that declines answers the prompt with `refusal`, not
    /// `end_turn`: the client could only tell from the `[refusal]` chunk.
    #[tokio::test]
    async fn a_model_refusal_ends_the_prompt_with_stop_reason_refusal() {
        let (model, _) = http_stub(|_| {
            Some((
                "text/event-stream",
                sse(&[
                    json!({"choices":[{"index":0,"delta":{"content":"no"},"finish_reason":"content_filter"}]}),
                ]),
            ))
        })
        .await;
        let (mut cfg, dir) = test_config();
        cfg.model = "ollama:test-model".into();
        cfg.ollama_host = model;
        let mut c = Client::start(cfg, sessions_in(&dir));
        c.init().await;
        let (_, created) = c
            .call(
                1,
                "session/new",
                json!({"cwd": dir.path(), "mcpServers": []}),
            )
            .await;
        let sid = created["result"]["sessionId"].as_str().unwrap().to_string();
        let (_, answer) = c.call(2, "session/prompt", prompt(&sid, "hi")).await;
        assert_eq!(answer["result"]["stopReason"], json!("refusal"), "{answer}");
        c.close().await;
    }

    /// The session task queues a turn's last updates and then its TurnDone
    /// on separate channels; an unbiased select answered `session/prompt`
    /// first about half the time, so the `[max_tokens]` notice arrived after
    /// the turn had already ended.
    #[tokio::test]
    async fn every_turn_update_precedes_the_prompt_response() {
        use tokio::io::AsyncBufReadExt;
        let (mut cfg, dir) = test_config();
        cfg.model = "ollama:test-model".into();
        cfg.ollama_host = max_tokens_model().await;
        let (client, server) = tokio::io::duplex(1 << 20);
        let (srv_r, srv_w) = tokio::io::split(server);
        let (cli_r, mut cli_w) = tokio::io::split(client);
        let task = tokio::spawn(AcpServer::serve(
            cfg,
            sessions_in(&dir),
            tokio::io::BufReader::new(srv_r),
            srv_w,
        ));
        let mut lines = tokio::io::BufReader::new(cli_r).lines();
        let mut next = async || -> Value {
            let l = tokio::time::timeout(std::time::Duration::from_secs(30), lines.next_line())
                .await
                .expect("no timeout")
                .unwrap()
                .expect("server closed");
            serde_json::from_str(&l).unwrap()
        };
        cli_w
            .write_all((init_line() + "\n").as_bytes())
            .await
            .unwrap();
        let _ = next().await;
        cli_w
            .write_all((new_session_line(1, &dir.path().to_string_lossy()) + "\n").as_bytes())
            .await
            .unwrap();
        let created = next().await;
        let sid = created["result"]["sessionId"].as_str().unwrap().to_string();
        for id in 2..22 {
            let prompt = json!({"jsonrpc":"2.0","id":id,"method":"session/prompt","params":{"sessionId":sid,"prompt":[{"type":"text","text":"hi"}]}}).to_string();
            cli_w.write_all((prompt + "\n").as_bytes()).await.unwrap();
            let mut saw_notice = false;
            loop {
                let v = next().await;
                if v["id"] == json!(id) {
                    assert_eq!(v["result"]["stopReason"], json!("max_tokens"), "{v}");
                    assert!(
                        saw_notice,
                        "prompt {id} answered before its [max_tokens] update"
                    );
                    break;
                }
                let text = v["params"]["update"]["content"]["text"]
                    .as_str()
                    .unwrap_or("");
                saw_notice |= text.contains("[max_tokens]");
            }
        }
        cli_w.shutdown().await.unwrap();
        drop(cli_w);
        task.await.unwrap().unwrap();
    }

    /// A permission prompt that timed out stayed in `pending`, so a late
    /// Allow marked a call that never ran as in_progress.
    #[test]
    fn a_late_permission_answer_for_a_finished_call_is_ignored() {
        let (cfg, _dir) = test_config();
        let (notif_tx, _n) = mpsc::unbounded_channel();
        let (done_tx, _d) = mpsc::unbounded_channel();
        let mut st = State {
            config: cfg,
            sessions_dir: PathBuf::from("/nonexistent"),
            initialized: true,
            sessions: HashMap::new(),
            evicted: HashMap::new(),
            max_live: MAX_LIVE_SESSIONS,
            pending: HashMap::new(),
            next_id: 1,
            notif_tx,
            done_tx,
        };
        let frames = st.handle_sdk_notification(SdkNotification::ToolApprovalNeeded {
            session_id: "s1".into(),
            approval_id: "a1".into(),
            tool: "Bash".into(),
            args: json!({"command": "ls"}),
            tool_use_id: "t1".into(),
        });
        let rpc_id = frames[1]["id"].clone();
        let frames = st.handle_sdk_notification(SdkNotification::ToolCompleted {
            session_id: "s1".into(),
            tool: "Bash".into(),
            tool_use_id: "t1".into(),
            success: false,
            output_summary: "Tool 'Bash' approval timed out after 60s.".into(),
            duration_ms: 0,
        });
        assert_eq!(frames[0]["params"]["update"]["status"], json!("failed"));
        let late = st.handle_client_response(
            &rpc_id,
            Some(json!({"outcome": {"outcome": "selected", "optionId": ALLOW_ONCE}})),
            None,
        );
        assert!(late.is_empty(), "{late:?}");
    }

    /// A permission request still queued when `session/cancel` was read
    /// went out as a dialog for a cancelled turn, which then waited for an
    /// answer or the approval timeout.
    #[test]
    fn a_permission_request_after_a_cancel_is_denied_without_asking() {
        let (cfg, _dir) = test_config();
        let (notif_tx, _n) = mpsc::unbounded_channel();
        let (done_tx, _d) = mpsc::unbounded_channel();
        let (turn_tx, _t) = mpsc::unbounded_channel();
        let (approval_in, mut answers) = mpsc::unbounded_channel();
        let mut st = State {
            config: cfg,
            sessions_dir: PathBuf::from("/nonexistent"),
            initialized: true,
            sessions: HashMap::new(),
            evicted: HashMap::new(),
            max_live: MAX_LIVE_SESSIONS,
            pending: HashMap::new(),
            next_id: 1,
            notif_tx,
            done_tx,
        };
        st.sessions.insert(
            "s1".into(),
            SessionHandle {
                turn_tx,
                approval_in,
                cancel: Arc::new(CancelSignal::default()),
                prompt_id: Some(json!(7)),
                cancel_requested: false,
                last_used: std::time::Instant::now(),
                params: Value::Null,
            },
        );
        assert!(st.handle_cancel(&json!({"sessionId": "s1"})).is_empty());

        let frames = st.handle_sdk_notification(SdkNotification::ToolApprovalNeeded {
            session_id: "s1".into(),
            approval_id: "a1".into(),
            tool: "Bash".into(),
            args: json!({"command": "ls"}),
            tool_use_id: "t1".into(),
        });

        assert!(
            frames
                .iter()
                .all(|f| f["method"] != json!("session/request_permission")),
            "{frames:?}"
        );
        assert_eq!(frames[0]["params"]["update"]["status"], json!("failed"));
        assert!(st.pending.is_empty());
        assert_eq!(
            answers.try_recv().unwrap(),
            (
                "a1".to_string(),
                Some("Cancelled by the client.".to_string())
            )
        );
    }

    /// A prompt over 4 MB (a large @-mentioned file, inlined) was answered
    /// with id null, so the editor's prompt never completed. The error now
    /// carries the request's id, read from the start of the line; so does a
    /// non-UTF-8 line's.
    #[tokio::test]
    async fn an_over_long_or_non_utf8_request_gets_an_error_with_its_id() {
        let (cfg, _dir) = test_config();
        let big = format!(
            r#"{{"jsonrpc":"2.0","id":42,"method":"session/prompt","params":{{"sessionId":"s","prompt":[{{"type":"text","text":"{}"}}]}}}}"#,
            "x".repeat(MAX_LINE_BYTES + 10)
        );
        let mut input = big.into_bytes();
        input.push(b'\n');
        input.extend_from_slice(
            b"{\"jsonrpc\":\"2.0\",\"id\":\"u1\",\"method\":\"x\",\"params\":\"\xff\"}\n",
        );
        input.extend_from_slice(init_line().as_bytes());
        input.push(b'\n');
        let out = drive_raw(cfg, &input).await;
        assert_eq!(out[0]["id"], json!(42), "{}", out[0]);
        assert_eq!(out[0]["error"]["code"], json!(rpc::PARSE_ERROR));
        assert!(
            out[0]["error"]["message"]
                .as_str()
                .unwrap()
                .contains("4 MB"),
            "{}",
            out[0]
        );
        assert_eq!(out[1]["id"], json!("u1"), "{}", out[1]);
        assert_eq!(out[2]["id"], json!(0));
    }

    /// One non-UTF-8 line used to end the whole ACP process.
    #[tokio::test]
    async fn a_non_utf8_line_is_a_parse_error_not_a_crash() {
        let (cfg, _dir) = test_config();
        let mut input = b"\xff\xfe\n".to_vec();
        input.extend_from_slice(init_line().as_bytes());
        input.push(b'\n');
        let out = drive_raw(cfg, &input).await;
        assert_eq!(out[0]["error"]["code"], json!(rpc::PARSE_ERROR));
        assert_eq!(out[1]["id"], json!(0));
    }

    /// `session/cancel` mid-turn must answer the prompt with `cancelled`
    /// even though the model call was still streaming.
    #[tokio::test]
    #[ignore]
    async fn live_cancel_mid_turn_answers_the_prompt_with_cancelled() {
        use tokio::io::AsyncBufReadExt;
        let key = std::env::var("ANTHROPIC_API_KEY").expect("ANTHROPIC_API_KEY");
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config {
            api_key: key,
            cwd: dir.path().to_path_buf(),
            ..Default::default()
        };
        let (client, server) = tokio::io::duplex(1 << 20);
        let (srv_r, srv_w) = tokio::io::split(server);
        let (cli_r, mut cli_w) = tokio::io::split(client);
        let task = tokio::spawn(AcpServer::serve(
            cfg,
            sessions_in(&dir),
            tokio::io::BufReader::new(srv_r),
            srv_w,
        ));
        let mut lines = tokio::io::BufReader::new(cli_r).lines();
        cli_w
            .write_all((init_line() + "\n").as_bytes())
            .await
            .unwrap();
        let _ = lines.next_line().await.unwrap().unwrap();
        cli_w
            .write_all((new_session_line(1, &dir.path().to_string_lossy()) + "\n").as_bytes())
            .await
            .unwrap();
        let created: Value =
            serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        let sid = created["result"]["sessionId"].as_str().unwrap().to_string();
        let prompt = json!({"jsonrpc":"2.0","id":2,"method":"session/prompt","params":{"sessionId":sid,"prompt":[{"type":"text","text":"Write a 2000-word essay about rivers."}]}}).to_string();
        cli_w.write_all((prompt + "\n").as_bytes()).await.unwrap();
        // Wait for the first chunk so the cancel lands mid-stream.
        loop {
            let l = tokio::time::timeout(std::time::Duration::from_secs(60), lines.next_line())
                .await
                .expect("no timeout")
                .unwrap()
                .expect("server closed");
            let v: Value = serde_json::from_str(&l).unwrap();
            if v["params"]["update"]["sessionUpdate"] == json!("agent_message_chunk") {
                break;
            }
            assert!(
                v["id"] != json!(2),
                "turn ended before we could cancel: {v}"
            );
        }
        let cancel = json!({"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":sid}})
            .to_string();
        cli_w.write_all((cancel + "\n").as_bytes()).await.unwrap();
        let started = std::time::Instant::now();
        let stop = loop {
            let l = tokio::time::timeout(std::time::Duration::from_secs(30), lines.next_line())
                .await
                .expect("cancel must be answered promptly")
                .unwrap()
                .expect("server closed");
            let v: Value = serde_json::from_str(&l).unwrap();
            if v["id"] == json!(2) {
                break v;
            }
        };
        assert_eq!(stop["result"]["stopReason"], json!("cancelled"), "{stop}");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "{:?}",
            started.elapsed()
        );
        cli_w.shutdown().await.unwrap();
        drop(cli_w);
        task.await.unwrap().unwrap();
    }

    /// Full prompt turn against the real API. Run with
    /// `ANTHROPIC_API_KEY=... cargo test --lib acp::server::tests::live -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn live_prompt_turn_streams_chunks_and_ends_the_turn() {
        use tokio::io::AsyncBufReadExt;
        let key = std::env::var("ANTHROPIC_API_KEY").expect("ANTHROPIC_API_KEY");
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config {
            api_key: key,
            cwd: dir.path().to_path_buf(),
            ..Default::default()
        };
        let (client, server) = tokio::io::duplex(1 << 20);
        let (srv_r, srv_w) = tokio::io::split(server);
        let (cli_r, mut cli_w) = tokio::io::split(client);
        let task = tokio::spawn(AcpServer::serve(
            cfg,
            sessions_in(&dir),
            tokio::io::BufReader::new(srv_r),
            srv_w,
        ));
        let mut lines = tokio::io::BufReader::new(cli_r).lines();
        cli_w
            .write_all((init_line() + "\n").as_bytes())
            .await
            .unwrap();
        let _init = lines.next_line().await.unwrap().unwrap();
        cli_w
            .write_all((new_session_line(1, &dir.path().to_string_lossy()) + "\n").as_bytes())
            .await
            .unwrap();
        let created: Value =
            serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        let sid = created["result"]["sessionId"].as_str().unwrap().to_string();
        let prompt = json!({"jsonrpc":"2.0","id":2,"method":"session/prompt","params":{"sessionId":sid,"prompt":[{"type":"text","text":"Reply with exactly one word: pong"}]}}).to_string();
        cli_w.write_all((prompt + "\n").as_bytes()).await.unwrap();
        let mut saw_chunk = false;
        let stop = loop {
            let l = tokio::time::timeout(std::time::Duration::from_secs(60), lines.next_line())
                .await
                .expect("no timeout")
                .unwrap()
                .expect("server closed");
            let v: Value = serde_json::from_str(&l).unwrap();
            if v["method"] == json!("session/update")
                && v["params"]["update"]["sessionUpdate"] == json!("agent_message_chunk")
            {
                saw_chunk = true;
            }
            if v["id"] == json!(2) {
                break v;
            }
        };
        assert!(saw_chunk);
        assert_eq!(stop["result"]["stopReason"], json!("end_turn"), "{stop}");
        cli_w.shutdown().await.unwrap();
        drop(cli_w);
        task.await.unwrap().unwrap();
    }

    // ── session/load and host-provided MCP servers ───────────────────────

    /// One ACP client talking to a server over an in-memory pipe.
    struct Client {
        lines: tokio::io::Lines<tokio::io::BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>>,
        w: tokio::io::WriteHalf<tokio::io::DuplexStream>,
        task: tokio::task::JoinHandle<Result<()>>,
    }

    impl Client {
        fn start(cfg: Config, sessions: PathBuf) -> Self {
            use tokio::io::AsyncBufReadExt;
            let (client, server) = tokio::io::duplex(1 << 20);
            let (srv_r, srv_w) = tokio::io::split(server);
            let (cli_r, w) = tokio::io::split(client);
            let task = tokio::spawn(AcpServer::serve(
                cfg,
                sessions,
                tokio::io::BufReader::new(srv_r),
                srv_w,
            ));
            Self {
                lines: tokio::io::BufReader::new(cli_r).lines(),
                w,
                task,
            }
        }

        async fn send(&mut self, frame: Value) {
            self.w
                .write_all(format!("{frame}\n").as_bytes())
                .await
                .unwrap();
        }

        async fn recv(&mut self) -> Value {
            let l =
                tokio::time::timeout(std::time::Duration::from_secs(30), self.lines.next_line())
                    .await
                    .expect("no timeout")
                    .unwrap()
                    .expect("server closed");
            serde_json::from_str(&l).unwrap()
        }

        /// Send request `id` and collect every frame up to its answer.
        async fn call(&mut self, id: u64, method: &str, params: Value) -> (Vec<Value>, Value) {
            self.send(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
                .await;
            let mut before = Vec::new();
            loop {
                let v = self.recv().await;
                if v["id"] == json!(id) && v.get("method").is_none() {
                    return (before, v);
                }
                before.push(v);
            }
        }

        async fn init(&mut self) {
            let (_, r) = self
                .call(
                    0,
                    "initialize",
                    json!({"protocolVersion":1,"clientCapabilities":{}}),
                )
                .await;
            assert!(r.get("result").is_some(), "{r}");
        }

        async fn close(mut self) {
            self.w.shutdown().await.unwrap();
            drop(self.w);
            self.task.await.unwrap().unwrap();
        }
    }

    fn prompt(sid: &str, text: &str) -> Value {
        json!({"sessionId": sid, "prompt": [{"type": "text", "text": text}]})
    }

    /// (sessionUpdate kind, text or toolCallId) of each replayed update.
    fn update_summary(frames: &[Value]) -> Vec<(String, String)> {
        frames
            .iter()
            .filter(|f| f["method"] == json!("session/update"))
            .map(|f| {
                let u = &f["params"]["update"];
                let what = u["content"]["text"]
                    .as_str()
                    .or_else(|| u["toolCallId"].as_str())
                    .unwrap_or_default();
                (
                    u["sessionUpdate"].as_str().unwrap().to_string(),
                    what.to_string(),
                )
            })
            .collect()
    }

    fn text(role: Role, t: &str) -> Message {
        Message {
            role,
            content: vec![ContentBlock::Text { text: t.into() }],
        }
    }

    #[test]
    fn host_mcp_servers_cover_stdio_http_and_sse() {
        let params = json!({"mcpServers": [
            {"name": "fs", "command": "/bin/fs-mcp", "args": ["--stdio"], "env": [{"name": "K", "value": "v"}]},
            {"type": "http", "name": "api", "url": "https://mcp.example.com/mcp",
             "headers": [{"name": "Authorization", "value": "Bearer t"}]},
            {"type": "sse", "name": "old", "url": "https://mcp.example.com/sse", "headers": []},
            {"type": "http", "name": "nourl", "headers": []},
            {"type": "websocket", "name": "ws", "url": "wss://mcp.example.com"},
        ]});
        let servers = mcp_servers(&params);
        assert_eq!(servers.len(), 3, "{servers:?}");
        match &servers[0] {
            (name, McpServerConfig::Stdio(s)) => {
                assert_eq!(name, "fs");
                assert_eq!(s.command, "/bin/fs-mcp");
                assert_eq!(s.args, vec!["--stdio".to_string()]);
                assert_eq!(s.env.get("K").map(String::as_str), Some("v"));
                assert!(s.literal, "host values must not be expanded");
            }
            other => panic!("{other:?}"),
        }
        match &servers[1] {
            (name, McpServerConfig::Http(h)) => {
                assert_eq!(name, "api");
                assert_eq!(h.url, "https://mcp.example.com/mcp");
                assert_eq!(
                    h.headers.get("Authorization").map(String::as_str),
                    Some("Bearer t")
                );
                assert!(h.literal, "host values must not be expanded");
                assert!(!h.sse);
            }
            other => panic!("{other:?}"),
        }
        match &servers[2] {
            (name, McpServerConfig::Http(h)) => {
                assert_eq!(name, "old");
                assert_eq!(h.url, "https://mcp.example.com/sse");
                assert!(h.sse && h.literal);
            }
            other => panic!("{other:?}"),
        }
        assert!(mcp_servers(&json!({})).is_empty());
    }

    #[test]
    fn replay_skips_injected_code_context_and_clips_tool_output() {
        let history = vec![
            Message {
                role: Role::User,
                content: vec![
                    ContentBlock::Text {
                        text: "fix it".into(),
                    },
                    ContentBlock::Text {
                        text: "<codebase_context>\nfn a() {}\n</codebase_context>".into(),
                    },
                ],
            },
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: "t1".into(),
                    name: "Bash".into(),
                    input: json!({"command": "make"}),
                }],
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "t1".into(),
                    content: vec![ToolResultContent::text("é".repeat(400))],
                    is_error: Some(true),
                }],
            },
        ];
        let u = replay_updates("s1", &history, false);
        assert_eq!(u.len(), 3, "{u:?}");
        assert_eq!(u[0]["update"]["content"]["text"], json!("fix it"));
        assert_eq!(u[1]["update"]["title"], json!("Bash: make"));
        assert_eq!(u[1]["update"]["kind"], json!("execute"));
        assert_eq!(u[2]["update"]["status"], json!("failed"));
        let out = u[2]["update"]["content"][0]["content"]["text"]
            .as_str()
            .unwrap();
        assert!(out.len() <= 503 && out.ends_with("..."), "{}", out.len());
        assert!(u.iter().all(|p| p["sessionId"] == json!("s1")));
    }

    /// A live turn sends thoughts only with `showThinkingSummaries`; a
    /// replay must not reveal what the live session kept hidden.
    #[test]
    fn replay_shows_thinking_only_when_summaries_are_on() {
        let history = vec![Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Thinking {
                    thinking: "private reasoning".into(),
                    signature: String::new(),
                },
                ContentBlock::Text {
                    text: "answer".into(),
                },
            ],
        }];
        let kinds = |show: bool| -> Vec<Value> {
            replay_updates("s1", &history, show)
                .into_iter()
                .map(|u| u["update"]["sessionUpdate"].clone())
                .collect()
        };
        assert_eq!(kinds(false), vec![json!("agent_message_chunk")]);
        assert_eq!(
            kinds(true),
            vec![json!("agent_thought_chunk"), json!("agent_message_chunk")]
        );
    }

    #[tokio::test]
    async fn session_load_of_an_unknown_session_is_resource_not_found() {
        let (cfg, dir) = test_config();
        let cwd = dir.path().to_string_lossy().to_string();
        let load = |id: u64, sid: &str| {
            json!({"jsonrpc":"2.0","id":id,"method":"session/load","params":{"sessionId":sid,"cwd":cwd,"mcpServers":[]}})
                .to_string()
        };
        // A saved session next to the sessions dir must not be reachable by
        // a relative id; without the id check this one would load.
        let sessions = sessions_in(&dir);
        Session::create_in(&sessions, "real".into()).await.unwrap();
        Session::create_in(&dir.path().join("other"), "elsewhere".into())
            .await
            .unwrap();
        let escape = "../other/elsewhere";
        assert!(sessions.join(format!("{escape}.meta")).exists());
        assert!(!Session::exists_in(&sessions, escape));
        assert!(Session::resume_in(&sessions, escape).await.is_err());
        let no_id = json!({"jsonrpc":"2.0","id":4,"method":"session/load","params":{"cwd":cwd,"mcpServers":[]}}).to_string();
        let out = drive_in(
            cfg,
            sessions,
            format!(
                "{}\n{}\n{}\n{}\n{}\n",
                load(9, "early"),
                init_line(),
                load(2, "no-such-session"),
                load(3, escape),
                no_id
            )
            .as_bytes(),
        )
        .await;
        assert_eq!(out[0]["error"]["code"], json!(rpc::INVALID_REQUEST));
        assert_eq!(out[2]["error"]["code"], json!(rpc::RESOURCE_NOT_FOUND));
        assert_eq!(out[3]["error"]["code"], json!(rpc::RESOURCE_NOT_FOUND));
        assert_eq!(out[4]["error"]["code"], json!(rpc::INVALID_PARAMS));
    }

    #[tokio::test]
    async fn without_session_persistence_load_is_neither_offered_nor_served() {
        let (mut cfg, dir) = test_config();
        cfg.no_session_persistence = true;
        let load = json!({"jsonrpc":"2.0","id":1,"method":"session/load","params":{"sessionId":"x","cwd":dir.path(),"mcpServers":[]}}).to_string();
        let out = drive(cfg, &[init_line(), load]).await;
        assert_eq!(
            out[0]["result"]["agentCapabilities"]["loadSession"],
            json!(false)
        );
        assert_eq!(out[1]["error"]["code"], json!(rpc::METHOD_NOT_FOUND));
    }

    /// A saved conversation is replayed in order (code context left out),
    /// answered with `{}`, and then continues: the model sees the old
    /// history, the new turn is saved, and loading again replays it too.
    #[tokio::test]
    async fn session_load_replays_the_history_in_order_then_takes_prompts() {
        let (mut cfg, dir) = test_config();
        let (model, model_seen) = text_model("Still hello.").await;
        cfg.model = "ollama:test-model".into();
        cfg.ollama_host = model;
        // Thoughts replay only when a live turn would show them.
        cfg.show_thinking_summaries = true;
        let sessions = sessions_in(&dir);
        let sid = "5f0c6d1e-0000-4000-8000-00000000abcd";
        let mut saved = Session::create_in(&sessions, sid.into()).await.unwrap();
        saved
            .append(&[
                Message {
                    role: Role::User,
                    content: vec![
                        ContentBlock::Text {
                            text: "What is in a.txt?".into(),
                        },
                        ContentBlock::Text {
                            text: "<codebase_context>\nfn x() {}\n</codebase_context>".into(),
                        },
                    ],
                },
                Message {
                    role: Role::Assistant,
                    content: vec![
                        ContentBlock::Thinking {
                            thinking: "Read the file.".into(),
                            signature: String::new(),
                        },
                        ContentBlock::Text {
                            text: "Let me look.".into(),
                        },
                        ContentBlock::ToolUse {
                            id: "toolu_1".into(),
                            name: "Read".into(),
                            input: json!({"file_path": "/p/a.txt"}),
                        },
                    ],
                },
                Message {
                    role: Role::User,
                    content: vec![ContentBlock::ToolResult {
                        tool_use_id: "toolu_1".into(),
                        content: vec![ToolResultContent::text("hello")],
                        is_error: None,
                    }],
                },
                text(Role::Assistant, "It says hello."),
            ])
            .await
            .unwrap();

        let cwd = dir.path().to_string_lossy().to_string();
        let load = json!({"sessionId": sid, "cwd": cwd, "mcpServers": []});
        let mut c = Client::start(cfg, sessions.clone());
        c.init().await;
        let (replay, answer) = c.call(1, "session/load", load.clone()).await;
        assert_eq!(answer["result"], json!({}), "{answer}");
        assert!(
            replay
                .iter()
                .all(|f| f["params"]["sessionId"] == json!(sid)),
            "{replay:?}"
        );
        let s = |k: &str, v: &str| (k.to_string(), v.to_string());
        let expected = vec![
            s("user_message_chunk", "What is in a.txt?"),
            s("agent_thought_chunk", "Read the file."),
            s("agent_message_chunk", "Let me look."),
            s("tool_call", "toolu_1"),
            s("tool_call_update", "toolu_1"),
            s("agent_message_chunk", "It says hello."),
        ];
        assert_eq!(update_summary(&replay), expected);
        assert_eq!(
            replay[3]["params"]["update"]["title"],
            json!("Read: /p/a.txt")
        );
        assert_eq!(replay[4]["params"]["update"]["status"], json!("completed"));
        assert_eq!(
            replay[4]["params"]["update"]["content"][0]["content"]["text"],
            json!("hello")
        );

        let (updates, answer) = c.call(2, "session/prompt", prompt(sid, "And now?")).await;
        assert_eq!(
            answer["result"]["stopReason"],
            json!("end_turn"),
            "{answer}"
        );
        assert!(
            update_summary(&updates).contains(&s("agent_message_chunk", "Still hello.")),
            "{updates:?}"
        );
        // The model got the loaded history, not a blank conversation.
        let req = chat_requests(&model_seen).pop().unwrap();
        let body = request_body(&req);
        assert!(body.contains("What is in a.txt?"), "{body}");
        assert!(body.contains("It says hello."), "{body}");
        assert!(body.contains("And now?"), "{body}");

        // The turn was saved before the answer, so loading the (live)
        // session again replays it as well, and it still takes prompts.
        let (replay, answer) = c.call(3, "session/load", load).await;
        assert_eq!(answer["result"], json!({}));
        let got = update_summary(&replay);
        assert_eq!(got.len(), expected.len() + 2, "{got:?}");
        assert_eq!(got[6], s("user_message_chunk", "And now?"));
        assert_eq!(got[7], s("agent_message_chunk", "Still hello."));
        let (_, answer) = c.call(4, "session/prompt", prompt(sid, "Once more")).await;
        assert_eq!(answer["result"]["stopReason"], json!("end_turn"));
        c.close().await;

        let (_, history) = Session::resume_in(&sessions, sid).await.unwrap();
        assert_eq!(history.len(), 8, "{history:?}");
    }

    /// A TUI session continued over ACP gets turns its undo timeline never
    /// saw: the timeline and the /redo turns end, so a later /redo cannot
    /// put an undone turn after them and /undo cannot pair one of them
    /// with an older turn of the same text.
    #[tokio::test]
    async fn session_load_ends_the_tui_undo_timeline() {
        use crate::session::{TurnMark, UndoneTurn, prompt_fingerprint};
        let (mut cfg, dir) = test_config();
        cfg.model = "ollama:test-model".into();
        cfg.ollama_host = text_model("Going.").await.0;
        let sessions = sessions_in(&dir);
        let sid = "5f0c6d1e-0000-4000-8000-00000000beef";
        let go = text(Role::User, "go");
        let mut saved = Session::create_in(&sessions, sid.into()).await.unwrap();
        saved
            .append(&[go.clone(), text(Role::Assistant, "went")])
            .await
            .unwrap();
        let mark = TurnMark {
            prompt: prompt_fingerprint(&go),
            before: 0,
        };
        saved.meta.timeline.push(mark.clone());
        saved.meta.redo.push(UndoneTurn {
            mark,
            after: 2,
            messages: vec![text(Role::User, "fix"), text(Role::Assistant, "fixed")],
        });
        saved.save_meta().await.unwrap();
        saved.save_redo(true).await.unwrap();
        let redo_file = sessions.join(format!("{sid}.redo"));
        assert!(redo_file.exists());

        let cwd = dir.path().to_string_lossy().to_string();
        let mut c = Client::start(cfg, sessions.clone());
        c.init().await;
        let load = json!({"sessionId": sid, "cwd": cwd, "mcpServers": []});
        let (_, answer) = c.call(1, "session/load", load).await;
        assert_eq!(answer["result"], json!({}), "{answer}");
        let (_, answer) = c.call(2, "session/prompt", prompt(sid, "go")).await;
        assert_eq!(answer["result"]["stopReason"], json!("end_turn"));
        c.close().await;

        let (resumed, history) = Session::resume_in(&sessions, sid).await.unwrap();
        assert_eq!(history.len(), 4, "{history:?}");
        assert!(resumed.meta.timeline.is_empty());
        assert!(resumed.meta.redo.is_empty());
        assert!(!redo_file.exists());
    }

    /// Idle sessions past the cap are dropped (their task, and with it
    /// their MCP servers, ends), and a prompt for one reloads it from disk
    /// with its history. Without this every `session/new` lived until the
    /// editor quit.
    #[tokio::test]
    async fn idle_sessions_past_the_cap_are_evicted_and_reload_on_their_next_prompt() {
        let (mut cfg, dir) = test_config();
        cfg.model = "ollama:test-model".into();
        let (host, seen) = text_model("ok").await;
        cfg.ollama_host = host;
        let (notif_tx, _notif_rx) = mpsc::unbounded_channel();
        let (done_tx, mut done_rx) = mpsc::unbounded_channel();
        let mut st = State {
            config: cfg,
            sessions_dir: sessions_in(&dir),
            initialized: true,
            sessions: HashMap::new(),
            evicted: HashMap::new(),
            max_live: 2,
            pending: HashMap::new(),
            next_id: 1,
            notif_tx,
            done_tx,
        };
        let cwd = dir.path().to_string_lossy().to_string();
        let mut ids = Vec::new();
        for (n, word) in ["kiwi", "fig", "plum"].into_iter().enumerate() {
            let n = n as u64 * 10;
            let out = st.handle_line(&new_session_line(n + 1, &cwd)).await;
            let sid = out[0]["result"]["sessionId"].as_str().unwrap().to_string();
            let line = json!({"jsonrpc":"2.0","id":n + 2,"method":"session/prompt","params":prompt(&sid, word)});
            assert!(st.handle_line(&line.to_string()).await.is_empty());
            let out = st.handle_turn_done(done_rx.recv().await.unwrap());
            assert_eq!(out[0]["result"]["stopReason"], json!("end_turn"), "{out:?}");
            ids.push(sid);
        }
        // The first, least recently used, made room for the third.
        assert_eq!(st.sessions.len(), 2);
        assert!(!st.sessions.contains_key(&ids[0]));
        let second_task = st.sessions[&ids[1]].approval_in.clone();

        // Its next prompt reloads it with its history, evicting the second.
        let line = json!({"jsonrpc":"2.0","id":40,"method":"session/prompt","params":prompt(&ids[0], "again")});
        assert!(st.handle_line(&line.to_string()).await.is_empty());
        let out = st.handle_turn_done(done_rx.recv().await.unwrap());
        assert_eq!(out[0]["result"]["stopReason"], json!("end_turn"), "{out:?}");
        let last = chat_requests(&seen).pop().unwrap();
        assert!(request_body(&last).contains("kiwi"), "{last}");
        assert!(!st.sessions.contains_key(&ids[1]));
        // The evicted session's task (which owns its tools) has ended.
        tokio::time::timeout(std::time::Duration::from_secs(5), second_task.closed())
            .await
            .expect("the evicted session's task is still running");

        // session/close drops a live or evicted session; an unknown id is
        // not found.
        for (n, sid) in [(50, &ids[2]), (51, &ids[1])] {
            let close =
                json!({"jsonrpc":"2.0","id":n,"method":"session/close","params":{"sessionId":sid}});
            let out = st.handle_line(&close.to_string()).await;
            assert_eq!(out[0]["result"], json!({}), "{out:?}");
            assert!(!st.sessions.contains_key(sid) && !st.evicted.contains_key(sid));
        }
        let close = json!({"jsonrpc":"2.0","id":52,"method":"session/close","params":{"sessionId":"ghost"}});
        let out = st.handle_line(&close.to_string()).await;
        assert_eq!(out[0]["error"]["code"], json!(rpc::RESOURCE_NOT_FOUND));
    }

    /// A `session/new` never prompted (an opened thread panel) was never on
    /// disk, so it was never evicted: its MCP servers lived until the
    /// editor quit. It is evicted too, and its first prompt starts it again
    /// under the same id.
    #[tokio::test]
    async fn sessions_never_prompted_are_evicted_and_restart_under_their_id() {
        let (mut cfg, dir) = test_config();
        cfg.model = "ollama:test-model".into();
        let (host, _seen) = text_model("ok").await;
        cfg.ollama_host = host;
        let (notif_tx, _notif_rx) = mpsc::unbounded_channel();
        let (done_tx, mut done_rx) = mpsc::unbounded_channel();
        let sessions = sessions_in(&dir);
        let mut st = State {
            config: cfg,
            sessions_dir: sessions.clone(),
            initialized: true,
            sessions: HashMap::new(),
            evicted: HashMap::new(),
            max_live: 2,
            pending: HashMap::new(),
            next_id: 1,
            notif_tx,
            done_tx,
        };
        let cwd = dir.path().to_string_lossy().to_string();
        let mut ids = Vec::new();
        for n in 1..=3 {
            let out = st.handle_line(&new_session_line(n, &cwd)).await;
            ids.push(out[0]["result"]["sessionId"].as_str().unwrap().to_string());
        }
        assert_eq!(st.sessions.len(), 2);
        assert!(st.evicted.contains_key(&ids[0]));
        assert!(!Session::exists_in(&sessions, &ids[0]));

        let line = json!({"jsonrpc":"2.0","id":10,"method":"session/prompt","params":prompt(&ids[0], "hi")});
        assert!(st.handle_line(&line.to_string()).await.is_empty());
        let out = st.handle_turn_done(done_rx.recv().await.unwrap());
        assert_eq!(out[0]["result"]["stopReason"], json!("end_turn"), "{out:?}");
        assert!(st.sessions.contains_key(&ids[0]));
        let (_, history) = Session::resume_in(&sessions, &ids[0]).await.unwrap();
        assert_eq!(history.len(), 2, "{history:?}");
    }

    /// routerBudget capped -p, the TUI and the SDK, but not ACP sessions:
    /// SdkSession reads only max_budget_usd.
    #[tokio::test]
    async fn router_budget_caps_acp_sessions() {
        let (mut cfg, dir) = test_config();
        cfg.model = "ollama:test-model".into();
        let (host, seen) = text_model("ok").await;
        cfg.ollama_host = host;
        cfg.max_budget_usd = None;
        cfg.router_budget = Some(0.0);
        cfg.config_dir_override = Some(dir.path().join("config"));
        std::fs::create_dir_all(dir.path().join("config")).unwrap();
        std::fs::write(
            dir.path().join("config/settings.json"),
            r#"{"routerBudget": 0}"#,
        )
        .unwrap();
        let sessions = sessions_in(&dir);
        let cwd = dir.path().to_string_lossy().to_string();
        let mut c = Client::start(cfg, sessions);
        c.init().await;
        let (_, created) = c
            .call(1, "session/new", json!({"cwd": cwd, "mcpServers": []}))
            .await;
        let sid = created["result"]["sessionId"].as_str().unwrap().to_string();
        let (_, answer) = c.call(2, "session/prompt", prompt(&sid, "hi")).await;
        assert_eq!(
            answer["result"]["stopReason"],
            json!("max_turn_requests"),
            "{answer}"
        );
        assert!(chat_requests(&seen).is_empty(), "the model was called");
        c.close().await;
    }

    /// A session started over ACP is saved as it goes, so another agent
    /// process (a restarted editor) can load it.
    #[tokio::test]
    async fn a_new_session_is_saved_and_loads_in_a_later_process() {
        let (mut cfg, dir) = test_config();
        cfg.model = "ollama:test-model".into();
        cfg.ollama_host = text_model("Noted: kiwi.").await.0;
        let sessions = sessions_in(&dir);
        let cwd = dir.path().to_string_lossy().to_string();

        let mut c = Client::start(cfg.clone(), sessions.clone());
        c.init().await;
        let (_, created) = c
            .call(1, "session/new", json!({"cwd": cwd, "mcpServers": []}))
            .await;
        let sid = created["result"]["sessionId"].as_str().unwrap().to_string();
        let (_, answer) = c
            .call(2, "session/prompt", prompt(&sid, "Remember kiwi"))
            .await;
        assert_eq!(answer["result"]["stopReason"], json!("end_turn"));
        c.close().await;

        let mut c = Client::start(cfg, sessions);
        c.init().await;
        let (replay, answer) = c
            .call(
                1,
                "session/load",
                json!({"sessionId": sid, "cwd": cwd, "mcpServers": []}),
            )
            .await;
        assert_eq!(answer["result"], json!({}), "{answer}");
        let s = |k: &str, v: &str| (k.to_string(), v.to_string());
        assert_eq!(
            update_summary(&replay),
            vec![
                s("user_message_chunk", "Remember kiwi"),
                s("agent_message_chunk", "Noted: kiwi."),
            ]
        );
        c.close().await;
    }

    /// An `http` MCP server passed by the host is started over Streamable
    /// HTTP with the host's headers; its tool reaches the model, asks the
    /// editor for permission like any other tool, and runs.
    #[tokio::test]
    async fn a_host_provided_http_mcp_server_is_started_and_its_tools_run() {
        let (mcp_url, mcp_seen) = http_stub(|req| {
            let msg: Value = serde_json::from_str(request_body(req)).ok()?;
            let id = msg.get("id")?.clone(); // notifications get a 202
            let result = match msg["method"].as_str()? {
                "initialize" => json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "fake", "version": "1"},
                }),
                "tools/list" => json!({"tools": [{
                    "name": "echo",
                    "description": "Echo text back",
                    "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}},
                }]}),
                "tools/call" => json!({"content": [{
                    "type": "text",
                    "text": format!("echoed {}", msg["params"]["arguments"]["text"].as_str().unwrap_or("")),
                }]}),
                _ => {
                    return Some((
                        "application/json",
                        json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"no"}})
                            .to_string(),
                    ));
                }
            };
            Some((
                "application/json",
                json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string(),
            ))
        })
        .await;
        // Calls the MCP tool first, then answers once it has the result.
        let (model, model_seen) = http_stub(|req| {
            let chunk = if request_body(req).contains(r#""role":"tool""#) {
                json!({"choices":[{"index":0,"delta":{"content":"done"},"finish_reason":"stop"}]})
            } else {
                json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function",
                    "function":{"name":"mcp__fake__echo","arguments":"{\"text\":\"hi\"}"}}]},
                    "finish_reason":"tool_calls"}]})
            };
            Some(("text/event-stream", sse(&[chunk])))
        })
        .await;
        let (mut cfg, dir) = test_config();
        cfg.model = "ollama:test-model".into();
        cfg.ollama_host = model;
        let mut c = Client::start(cfg, sessions_in(&dir));
        c.init().await;
        let (_, created) = c
            .call(
                1,
                "session/new",
                json!({"cwd": dir.path(), "mcpServers": [{
                    "type": "http", "name": "fake", "url": mcp_url,
                    "headers": [
                        {"name": "Authorization", "value": "Bearer t0k"},
                        // Sent resolved by the host: never expanded here.
                        {"name": "X-Literal", "value": "a${OXIDECLAW_TEST_SURELY_UNSET_VAR}"},
                    ],
                }]}),
            )
            .await;
        let sid = created["result"]["sessionId"].as_str().unwrap().to_string();

        c.send(json!({"jsonrpc":"2.0","id":2,"method":"session/prompt","params":prompt(&sid, "echo hi")}))
            .await;
        let mut asked = false;
        let mut updates = Vec::new();
        let answer = loop {
            let v = c.recv().await;
            if v["method"] == json!("session/request_permission") {
                assert_eq!(
                    v["params"]["toolCall"]["title"],
                    json!("mcp__fake__echo"),
                    "{v}"
                );
                asked = true;
                c.send(json!({"jsonrpc":"2.0","id":v["id"],"result":{"outcome":{"outcome":"selected","optionId":ALLOW_ONCE}}}))
                    .await;
                continue;
            }
            if v["id"] == json!(2) {
                break v;
            }
            updates.push(v);
        };
        assert_eq!(
            answer["result"]["stopReason"],
            json!("end_turn"),
            "{answer}"
        );
        assert!(asked, "the MCP tool ran without asking the editor");
        let done = updates.iter().find(|u| {
            u["params"]["update"]["sessionUpdate"] == json!("tool_call_update")
                && u["params"]["update"]["status"] == json!("completed")
        });
        assert_eq!(
            done.expect("a completed tool call")["params"]["update"]["content"][0]["content"]["text"],
            json!("echoed hi")
        );
        // The model was offered the server's tool.
        let first = chat_requests(&model_seen).remove(0);
        assert!(request_body(&first).contains("mcp__fake__echo"), "{first}");
        // Every request to the server carried the host's header.
        let seen = mcp_seen.lock().unwrap().clone();
        assert!(
            seen.iter().any(|r| r.contains("\"tools/call\"")),
            "{seen:?}"
        );
        assert!(
            seen.iter()
                .all(|r| r.to_ascii_lowercase().contains("authorization: bearer t0k")),
            "{seen:?}"
        );
        assert!(
            seen.iter().all(|r| r
                .to_ascii_lowercase()
                .contains("x-literal: a${oxideclaw_test_surely_unset_var}")),
            "{seen:?}"
        );
        c.close().await;
    }
}
