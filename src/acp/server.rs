//! The ACP agent loop: JSON-RPC in, `session/update` out, one `SdkSession`
//! per ACP session. See the module docs in `acp/mod.rs` for scope.

use super::rpc::{self, Incoming, RpcError};
use crate::config::Config;
use crate::sdk::protocol::{Capabilities, Policy, SdkNotification};
use crate::sdk::session::{CancelSignal, SdkSession, TurnEnd};
use crate::sdk::transport::stdio::{LineRead, read_line_bounded};
use crate::sdk::validate_session_cwd;
use crate::tools::all_tools;
use anyhow::Result;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::io::{AsyncBufRead, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;

const VERSION: &str = env!("CARGO_PKG_VERSION");
/// Longest inbound line we will buffer (embedded resources can be large).
const MAX_LINE_BYTES: usize = 4 * 1024 * 1024;
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
    initialized: bool,
    sessions: HashMap<String, SessionHandle>,
    pending: HashMap<String, PendingPermission>,
    next_id: i64,
    notif_tx: mpsc::UnboundedSender<SdkNotification>,
    done_tx: mpsc::UnboundedSender<TurnDone>,
}

impl AcpServer {
    /// Serve ACP over `reader`/`writer` until the reader hits EOF.
    pub async fn run<R, W>(config: Config, mut reader: R, mut writer: W) -> Result<()>
    where
        R: AsyncBufRead + Unpin,
        W: AsyncWrite + Unpin,
    {
        let (notif_tx, mut notif_rx) = mpsc::unbounded_channel::<SdkNotification>();
        let (done_tx, mut done_rx) = mpsc::unbounded_channel::<TurnDone>();
        let mut st = State {
            config,
            initialized: false,
            sessions: HashMap::new(),
            pending: HashMap::new(),
            next_id: 1,
            notif_tx,
            done_tx,
        };
        let mut buf = String::new();
        loop {
            let frames = tokio::select! {
                read = read_line_bounded(&mut reader, &mut buf, MAX_LINE_BYTES) => {
                    match read? {
                        LineRead::Eof => break,
                        LineRead::TooLong => vec![rpc::error(
                            &Value::Null,
                            rpc::PARSE_ERROR,
                            format!("line exceeds {MAX_LINE_BYTES} bytes"),
                        )],
                        LineRead::Line => {
                            let line = buf.trim().to_string();
                            buf.clear();
                            if line.is_empty() {
                                continue;
                            }
                            st.handle_line(&line)
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
    fn handle_line(&mut self, line: &str) -> Vec<Value> {
        match rpc::parse(line) {
            Err(e) => vec![rpc::error(&Value::Null, e.code, e.message)],
            Ok(Incoming::Request { id, method, params }) => {
                match self.handle_request(&id, &method, &params) {
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

    fn handle_request(
        &mut self,
        id: &Value,
        method: &str,
        params: &Value,
    ) -> Result<Vec<Value>, RpcError> {
        match method {
            "initialize" => {
                self.initialized = true;
                Ok(vec![rpc::response(id, initialize_result())])
            }
            "authenticate" => Ok(vec![rpc::response(id, json!({}))]),
            "session/new" => {
                if !self.initialized {
                    return Err(RpcError::new(
                        rpc::INVALID_REQUEST,
                        "call initialize before session/new",
                    ));
                }
                let sid = self.new_session(params)?;
                Ok(vec![rpc::response(id, json!({"sessionId": sid}))])
            }
            "session/prompt" => {
                self.start_prompt(id, params)?;
                Ok(vec![]) // answered when the turn ends
            }
            other => Err(RpcError::new(
                rpc::METHOD_NOT_FOUND,
                format!("{other} is not supported by this agent"),
            )),
        }
    }

    fn new_session(&mut self, params: &Value) -> Result<String, RpcError> {
        let cwd = params
            .get("cwd")
            .and_then(Value::as_str)
            .ok_or_else(|| RpcError::new(rpc::INVALID_PARAMS, "cwd is required"))?;
        let dir = validate_session_cwd(Some(cwd.to_string()))
            .map_err(|m| RpcError::new(rpc::INVALID_PARAMS, m))?
            .expect("Some(cwd) validates to Some(dir)");
        let mut cfg = self.config.clone();
        cfg.cwd = dir;
        if let Some(servers) = params.get("mcpServers").and_then(Value::as_array) {
            for s in servers {
                let name = s.get("name").and_then(Value::as_str).unwrap_or("mcp");
                match s.get("command").and_then(Value::as_str) {
                    Some(command) => {
                        let args = s
                            .get("args")
                            .and_then(Value::as_array)
                            .map(|a| {
                                a.iter()
                                    .filter_map(Value::as_str)
                                    .map(str::to_string)
                                    .collect()
                            })
                            .unwrap_or_default();
                        let env = s
                            .get("env")
                            .and_then(Value::as_array)
                            .map(|e| {
                                e.iter()
                                    .filter_map(|kv| {
                                        Some((
                                            kv.get("name")?.as_str()?.to_string(),
                                            kv.get("value")?.as_str()?.to_string(),
                                        ))
                                    })
                                    .collect()
                            })
                            .unwrap_or_default();
                        cfg.extra_mcp_servers.insert(
                            name.to_string(),
                            crate::mcp::types::McpServerConfig::Stdio(
                                crate::mcp::types::StdioServerConfig {
                                    command: command.to_string(),
                                    args,
                                    env,
                                },
                            ),
                        );
                    }
                    None => tracing::warn!("acp: ignoring non-stdio MCP server {name}"),
                }
            }
        }
        let tools = all_tools(&cfg);
        let (approval_in_tx, approval_in_rx) = mpsc::unbounded_channel();
        let session = SdkSession::new(
            cfg,
            tools,
            Policy::default(),
            Capabilities::default(),
            self.notif_tx.clone(),
            self.notif_tx.clone(),
            approval_in_rx,
        )
        .map_err(|e| RpcError::new(rpc::INTERNAL_ERROR, format!("{e:#}")))?;
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
                if done_tx.send((sid.clone(), r)).is_err() {
                    break;
                }
            }
        });
        self.sessions.insert(
            session_id.clone(),
            SessionHandle {
                turn_tx,
                approval_in: approval_in_tx,
                cancel,
                prompt_id: None,
                cancel_requested: false,
            },
        );
        Ok(session_id)
    }

    fn start_prompt(&mut self, id: &Value, params: &Value) -> Result<(), RpcError> {
        let sid = params
            .get("sessionId")
            .and_then(Value::as_str)
            .ok_or_else(|| RpcError::new(rpc::INVALID_PARAMS, "sessionId is required"))?;
        let text = prompt_text(params.get("prompt").unwrap_or(&Value::Null))?;
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
            let call = json!({
                "toolCallId": tool_use_id,
                "title": tool_title(&tool, &args),
                "kind": tool_kind(&tool),
                "status": "pending",
                "rawInput": args,
            });
            let mut announce = call.clone();
            announce["sessionUpdate"] = json!("tool_call");
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
        session_updates(&n)
            .into_iter()
            .map(|params| rpc::notification("session/update", params))
            .collect()
    }

    fn handle_turn_done(&mut self, (sid, result): TurnDone) -> Vec<Value> {
        let Some(h) = self.sessions.get_mut(&sid) else {
            return vec![];
        };
        let Some(id) = h.prompt_id.take() else {
            return vec![];
        };
        let cancelled = std::mem::take(&mut h.cancel_requested);
        h.cancel.reset();
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

// ── pure translation helpers (unit-tested) ──────────────────────────────────

/// The `initialize` result: what we can and cannot do.
pub(crate) fn initialize_result() -> Value {
    json!({
        "protocolVersion": super::PROTOCOL_VERSION,
        "agentCapabilities": {
            "loadSession": false,
            "promptCapabilities": {"image": false, "audio": false, "embeddedContext": true},
            "mcpCapabilities": {"http": false, "sse": false},
        },
        "agentInfo": {"name": "oxideclaw", "title": "OxideClaw", "version": VERSION},
        "authMethods": [],
    })
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
        TurnEnd::MaxTurns => "max_turn_requests",
        TurnEnd::BudgetExceeded => "refusal",
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
        let r = initialize_result();
        assert_eq!(r["protocolVersion"], json!(1));
        assert_eq!(r["agentCapabilities"]["loadSession"], json!(false));
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
            r["agentCapabilities"]["mcpCapabilities"]["http"],
            json!(false)
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

    #[test]
    fn stop_reasons_use_the_acp_vocabulary() {
        assert_eq!(stop_reason(TurnEnd::EndTurn), "end_turn");
        assert_eq!(stop_reason(TurnEnd::MaxTokens), "max_tokens");
        assert_eq!(stop_reason(TurnEnd::MaxTurns), "max_turn_requests");
        assert_eq!(stop_reason(TurnEnd::BudgetExceeded), "refusal");
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

    fn test_config() -> (Config, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config {
            api_key: "sk-ant-test".into(),
            cwd: dir.path().to_path_buf(),
            ..Default::default()
        };
        (cfg, dir)
    }

    /// Feed `lines`, close stdin, collect every frame the server wrote.
    async fn drive(cfg: Config, lines: &[String]) -> Vec<Value> {
        use tokio::io::AsyncReadExt;
        let (client, server) = tokio::io::duplex(1 << 20);
        let (srv_r, srv_w) = tokio::io::split(server);
        let (mut cli_r, mut cli_w) = tokio::io::split(client);
        let task = tokio::spawn(AcpServer::run(cfg, tokio::io::BufReader::new(srv_r), srv_w));
        for l in lines {
            cli_w.write_all(l.as_bytes()).await.unwrap();
            cli_w.write_all(b"\n").await.unwrap();
        }
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
        let load = json!({"jsonrpc":"2.0","id":1,"method":"session/load","params":{"sessionId":"x","cwd":"/"}}).to_string();
        let weird = json!({"jsonrpc":"2.0","id":2,"method":"does/not/exist"}).to_string();
        let out = drive(cfg, &[init_line(), load, weird]).await;
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
        let task = tokio::spawn(AcpServer::run(cfg, tokio::io::BufReader::new(srv_r), srv_w));
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
        let task = tokio::spawn(AcpServer::run(cfg, tokio::io::BufReader::new(srv_r), srv_w));
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
}
