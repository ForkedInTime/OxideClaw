/// MCP client — connects to a single MCP server via stdio or HTTP.
///
/// Implements the Model Context Protocol (MCP) JSON-RPC 2.0 protocol.
/// Stdio transport: spawns the server process and communicates via stdin/stdout.
/// HTTP transport:  POSTs JSON-RPC requests to a URL (streamable HTTP).
/// SSE transport:   the legacy HTTP+SSE pair (an event stream plus POSTs).
///
/// Two protocol eras. The stateless 2026-07-28 revision ("modern") has no
/// handshake: every request carries its version, client identity and
/// capabilities in `_meta`. Earlier revisions ("legacy") open with
/// `initialize`. A server's era is probed once per connection with
/// `server/discover` (see `McpClient::handshake`).
use crate::mcp::types::{
    JsonRpcError, JsonRpcRequest, JsonRpcResponse, McpCallResult, McpResource, McpToolDef,
};
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use tokio::sync::{Mutex, oneshot};
use tokio::time::Duration;

/// Deadline for the probe, the handshake and list calls.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
/// Default deadline for `tools/call` and `resources/read`, which do the
/// server's real work (a test suite, a browser flow, a long query) and so
/// may well outlast `REQUEST_TIMEOUT`. Esc still cancels one sooner.
const DEFAULT_TOOL_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// `MCP_TOOL_TIMEOUT` (milliseconds, as in Claude Code) or the default.
fn tool_timeout() -> Duration {
    static TIMEOUT: std::sync::OnceLock<Duration> = std::sync::OnceLock::new();
    *TIMEOUT.get_or_init(|| parse_tool_timeout(std::env::var("MCP_TOOL_TIMEOUT").ok().as_deref()))
}

fn parse_tool_timeout(value: Option<&str>) -> Duration {
    match value.map(|v| v.trim().parse::<u64>()) {
        Some(Ok(ms)) if ms > 0 => Duration::from_millis(ms),
        Some(_) => {
            tracing::warn!("MCP_TOOL_TIMEOUT is not a positive number of milliseconds; ignored");
            DEFAULT_TOOL_TIMEOUT
        }
        None => DEFAULT_TOOL_TIMEOUT,
    }
}

/// The deadline for one request: `tool` for the calls that run a tool or
/// read a resource, `base` for everything else.
fn deadline(method: &str, base: Duration, tool: Duration) -> Duration {
    if matches!(method, "tools/call" | "resources/read") {
        tool
    } else {
        base
    }
}
/// Default ceiling on what one tool call or resource read hands the model.
const DEFAULT_MAX_RESULT_CHARS: usize = 25_000;

/// Truncate at a char boundary and say so.
fn cap_output(output: String, max_chars: usize) -> String {
    if output.chars().count() <= max_chars {
        return output;
    }
    let truncated: String = output.chars().take(max_chars).collect();
    format!(
        "{}\n\n[Result truncated: {} chars total, limit {}]",
        truncated,
        output.chars().count(),
        max_chars
    )
}
// ── Protocol revisions ────────────────────────────────────────────────────────

/// The stateless revision (docs/specification/2026-07-28/basic/versioning.mdx).
const MODERN_PROTOCOL_VERSION: &str = "2026-07-28";

/// Handshake-era revisions this client implements, newest first; the first
/// is offered in `initialize` and the server's answer is used. 2025-11-25 is
/// not among them: its Streamable HTTP lets a server end a POST's event
/// stream before the response and expects a GET with `Last-Event-ID` to
/// resume it, which this client does not do.
const LEGACY_PROTOCOL_VERSIONS: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];

/// How long the `server/discover` probe waits before treating the server
/// as handshake-era: such a server may never answer a method it does not
/// know (docs/specification/2026-07-28/basic/transports/stdio.mdx,
/// "Backward Compatibility").
const DISCOVER_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// After a probe timeout the client sends `initialize`. A modern server that
/// was only slow to start answers the probe first and then rejects
/// `initialize`; this is how long to look for that late answer.
const LATE_PROBE_GRACE: Duration = Duration::from_secs(1);

/// Error codes the 2026-07-28 revision reserves
/// (docs/specification/2026-07-28/basic/index.mdx, "Error Codes"). Only
/// these mark a server as modern: any other probe error means legacy.
const HEADER_MISMATCH: i64 = -32020;
const MISSING_REQUIRED_CLIENT_CAPABILITY: i64 = -32021;
const UNSUPPORTED_PROTOCOL_VERSION: i64 = -32022;

/// Requests a server may answer with `resultType: "input_required"`
/// (docs/specification/2026-07-28/basic/patterns/mrtr.mdx, "Supported
/// Requests").
const INPUT_REQUIRED_METHODS: [&str; 3] = ["tools/call", "resources/read", "prompts/get"];

/// Bound on multi round-trip retries of one request: a server may ask again
/// and again, and nothing here ever supplies what it wants.
const MAX_INPUT_ROUNDS: usize = 8;

fn client_info() -> Value {
    json!({ "name": "oxideclaw", "version": env!("CARGO_PKG_VERSION") })
}

/// `params` with the per-request fields every modern request must carry
/// (docs/specification/2026-07-28/basic/index.mdx, "Per-request protocol
/// fields"). The capabilities are empty: nothing here answers elicitation,
/// sampling or roots.
fn with_modern_meta(mut params: Value) -> Value {
    if let Some(obj) = params.as_object_mut()
        && let Some(meta) = obj
            .entry("_meta")
            .or_insert_with(|| json!({}))
            .as_object_mut()
    {
        meta.insert(
            "io.modelcontextprotocol/protocolVersion".into(),
            MODERN_PROTOCOL_VERSION.into(),
        );
        meta.insert("io.modelcontextprotocol/clientInfo".into(), client_info());
        meta.insert(
            "io.modelcontextprotocol/clientCapabilities".into(),
            json!({}),
        );
    }
    params
}

/// The protocol version a request's `_meta` declares, if it is a modern one.
fn modern_version(params: Option<&Value>) -> Option<&str> {
    params?
        .pointer("/_meta/io.modelcontextprotocol~1protocolVersion")?
        .as_str()
}

/// The revision a connected server speaks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Protocol {
    /// Not negotiated yet.
    Pending,
    /// 2026-07-28: stateless, `_meta` on every request.
    Modern,
    /// `initialize` handshake; the version the server answered.
    Legacy(String),
}

impl Protocol {
    pub fn revision(&self) -> &str {
        match self {
            Self::Pending => "",
            Self::Modern => MODERN_PROTOCOL_VERSION,
            Self::Legacy(v) => v,
        }
    }
}

/// A JSON-RPC error the server sent, kept typed so era detection can tell
/// the 2026-07-28 errors from everything else.
#[derive(Debug)]
pub(crate) struct RpcError {
    pub code: i64,
    pub message: String,
    pub data: Option<Value>,
    /// "HTTP MCP <method> failed: <status>" when it came with an HTTP error.
    pub context: Option<String>,
}

impl From<JsonRpcError> for RpcError {
    fn from(e: JsonRpcError) -> Self {
        Self {
            code: e.code,
            message: e.message,
            data: e.data,
            context: None,
        }
    }
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(c) = &self.context {
            write!(f, "{c} — ")?;
        }
        write!(f, "MCP error {}: {}", self.code, self.message)
    }
}

impl std::error::Error for RpcError {}

/// The server process exited on the `server/discover` probe. Some
/// handshake-era servers end the session on any request before
/// `initialize`; the client starts the server again and skips the probe.
#[derive(Debug)]
struct ProbeExited;

impl std::fmt::Display for ProbeExited {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MCP server process exited on the server/discover probe")
    }
}

impl std::error::Error for ProbeExited {}

// ── Transport trait ───────────────────────────────────────────────────────────

#[async_trait]
pub(crate) trait McpTransport: Send + Sync {
    /// Send a request and await its response.
    async fn call(&self, id: u64, method: &str, params: Value) -> Result<Value>;

    /// `call` with extra HTTP headers (`Mcp-Param-*`); transports without
    /// a header layer ignore them.
    async fn call_with_headers(
        &self,
        id: u64,
        method: &str,
        params: Value,
        _headers: &[(String, String)],
    ) -> Result<Value> {
        self.call(id, method, params).await
    }

    /// Send a notification (fire-and-forget, no response expected).
    async fn notify(&self, _method: &str) {}

    /// The handshake-era version `initialize` settled on, which HTTP sends
    /// as `MCP-Protocol-Version` on every later request.
    fn set_protocol_version(&self, _version: &str) {}

    /// Whether the server is gone for good (a stdio process that exited).
    fn is_closed(&self) -> bool {
        false
    }
}

// ── Stdio transport ───────────────────────────────────────────────────────────

pub(crate) struct StdioTransport {
    stdin_tx: tokio::sync::mpsc::UnboundedSender<String>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value>>>>>,
    /// Set by the reader, under the `pending` lock, once stdout hits EOF.
    /// A request registered after the reader's final drain would otherwise
    /// wait out the full timeout for a reply that can never come.
    closed: Arc<AtomicBool>,
    timeout: Duration,
    tool_timeout: Duration,
    /// Dropped with the transport, which tells the reap task to kill a
    /// server that does not exit on stdin EOF.
    _kill_on_drop: oneshot::Sender<()>,
}

/// How long a stdio server gets to exit on stdin EOF once its transport is
/// dropped, before it is killed.
const STDIO_EXIT_GRACE: Duration = Duration::from_secs(2);

impl StdioTransport {
    pub async fn connect(
        command: &str,
        args: &[String],
        env: &HashMap<String, String>,
        cwd: &std::path::Path,
    ) -> Result<Self> {
        use tokio::io::{AsyncWriteExt, BufReader};
        use tokio::process::Command;

        #[cfg(windows)]
        let program = {
            let path = env
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("PATH"))
                .map(|(_, v)| std::ffi::OsString::from(v))
                .or_else(|| std::env::var_os("PATH"));
            resolve_on_path(
                command,
                path.as_deref(),
                std::env::var_os("PATHEXT").as_deref(),
            )
            .unwrap_or_else(|| command.into())
        };
        #[cfg(not(windows))]
        let program = command;

        let mut child = Command::new(program)
            .args(args)
            .envs(env)
            .current_dir(cwd)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| anyhow!("Failed to spawn MCP server '{}': {}", command, e))?;

        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("MCP server '{}': could not open stdin", command))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("MCP server '{}': could not open stdout", command))?;

        let (stdin_tx, mut stdin_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value>>>>> =
            Arc::new(Mutex::new(HashMap::new()));

        // Writer task: drain channel → child stdin
        tokio::spawn(async move {
            while let Some(line) = stdin_rx.recv().await {
                let data = format!("{}\n", line);
                if stdin.write_all(data.as_bytes()).await.is_err() {
                    break;
                }
            }
        });

        // Reap task: prevents a zombie. It owns the child until it exits,
        // so `kill_on_drop` only fires at runtime shutdown; a server that is
        // stuck or ignores stdin EOF is killed here once the transport is
        // gone (a startup timeout, a failed handshake, a closed ACP session).
        let (kill_tx, kill_rx) = oneshot::channel::<()>();
        tokio::spawn(async move {
            let dropped = tokio::select! {
                _ = child.wait() => false,
                _ = kill_rx => true,
            };
            if dropped
                && tokio::time::timeout(STDIO_EXIT_GRACE, child.wait())
                    .await
                    .is_err()
            {
                let _ = child.start_kill();
                let _ = child.wait().await;
            }
        });

        // Reader task: child stdout → pending oneshots
        let pending_clone = Arc::clone(&pending);
        let closed = Arc::new(AtomicBool::new(false));
        let closed_clone = Arc::clone(&closed);
        // Weak, so the reader never keeps the child's stdin open after the
        // transport is dropped: stdin EOF is how the server learns to exit.
        let reply_tx = stdin_tx.downgrade();
        tokio::spawn(async move {
            // Raw bytes, not `lines()`: a line that is not UTF-8 is an Err
            // from `next_line`, which would end the loop and disconnect a
            // server that is still running. Only EOF or an I/O error may.
            let mut reader = BufReader::new(stdout);
            let mut buf = Vec::new();
            loop {
                match read_line_capped(&mut reader, &mut buf, MAX_STDIO_LINE_BYTES).await {
                    Ok(StdioLine::Eof) | Err(_) => break,
                    Ok(StdioLine::Line) => {}
                    Ok(StdioLine::TooLong) => {
                        // Almost always the reply to a call in flight, whose
                        // id is lost with the line: fail what is waiting
                        // rather than leave it to its timeout. The server
                        // itself may still be fine, so keep reading.
                        tracing::warn!(
                            "MCP stdio server sent a line over {MAX_STDIO_LINE_BYTES} bytes; dropped"
                        );
                        for (_, tx) in pending_clone.lock().await.drain() {
                            let _ = tx.send(Err(anyhow!(
                                "MCP server sent a message over {MAX_STDIO_LINE_BYTES} bytes"
                            )));
                        }
                        continue;
                    }
                }
                let trimmed = buf.trim_ascii();
                if trimmed.is_empty() {
                    continue;
                }
                // Ignore malformed / partial / undecodable lines
                let Ok(msg) = serde_json::from_slice::<Value>(trimmed) else {
                    continue;
                };
                if let Some(method) = msg.get("method").and_then(Value::as_str) {
                    // A server-to-client request must get an answer, or a
                    // server that sent one (ping, roots/list, sampling)
                    // blocks on it. Its id is from the server's own counter,
                    // so it must never resolve one of our pending calls.
                    if let Some(id) = msg.get("id")
                        && let Some(tx) = reply_tx.upgrade()
                    {
                        let _ = tx.send(server_request_reply(id, method).to_string());
                    }
                    continue; // a notification needs nothing
                }
                let Ok(resp) = serde_json::from_value::<JsonRpcResponse>(msg) else {
                    continue;
                };
                let Some(id) = resp.id.as_ref().and_then(|v| v.as_u64()) else {
                    continue;
                };
                let result = if let Some(err) = resp.error {
                    Err(RpcError::from(err).into())
                } else {
                    Ok(resp.result.unwrap_or(Value::Null))
                };
                let mut pending = pending_clone.lock().await;
                if let Some(tx) = pending.remove(&id) {
                    let _ = tx.send(result);
                }
            }
            // Process exited — fail every pending request, and every later
            // one (`call` checks the flag under this same lock).
            let mut pending = pending_clone.lock().await;
            closed_clone.store(true, Ordering::SeqCst);
            for (_, tx) in pending.drain() {
                let _ = tx.send(Err(anyhow!("MCP server process exited")));
            }
        });

        Ok(Self {
            stdin_tx,
            pending,
            closed,
            timeout: REQUEST_TIMEOUT,
            tool_timeout: tool_timeout(),
            _kill_on_drop: kill_tx,
        })
    }
}

/// Largest stdout line (one JSON-RPC message) buffered from a stdio server,
/// the same bound as an HTTP body.
const MAX_STDIO_LINE_BYTES: usize = MAX_HTTP_BODY_BYTES;

enum StdioLine {
    Eof,
    /// A line, in `buf` with its newline (none on a final unterminated one).
    Line,
    /// A line over the cap, read to its end and dropped.
    TooLong,
}

/// `read_until(b'\n')` that stops buffering past `max` bytes, so a server
/// writing a huge or newline-free blob cannot grow our memory without end.
async fn read_line_capped<R>(
    reader: &mut R,
    buf: &mut Vec<u8>,
    max: usize,
) -> std::io::Result<StdioLine>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    use tokio::io::AsyncBufReadExt;
    buf.clear();
    let mut over = false;
    let mut read_any = false;
    loop {
        let avail = reader.fill_buf().await?;
        if avail.is_empty() {
            return Ok(match (over, read_any) {
                (true, _) => StdioLine::TooLong,
                (false, true) => StdioLine::Line,
                (false, false) => StdioLine::Eof,
            });
        }
        read_any = true;
        let (n, done) = match avail.iter().position(|&b| b == b'\n') {
            Some(i) => (i + 1, true),
            None => (avail.len(), false),
        };
        if !over {
            if buf.len() + n > max {
                over = true;
                buf.clear();
                buf.shrink_to_fit();
            } else {
                buf.extend_from_slice(&avail[..n]);
            }
        }
        reader.consume(n);
        if done {
            return Ok(if over {
                StdioLine::TooLong
            } else {
                StdioLine::Line
            });
        }
    }
}

/// Cancels a stdio request its caller stopped waiting for. stdio has no
/// per-request stream to close, so the server learns of it only from
/// `notifications/cancelled`; without it a single-threaded server keeps
/// working on the abandoned call and queues the next one behind it. A
/// server that honours the cancel never replies, so the pending entry is
/// removed here too.
struct CancelOnDrop<'a> {
    transport: &'a StdioTransport,
    id: u64,
    method: &'a str,
    armed: bool,
}

impl Drop for CancelOnDrop<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        // `initialize` must not be cancelled, and before the probe answers
        // the server may be one that expects `initialize` first.
        if !matches!(self.method, "initialize" | "server/discover") {
            let note = json!({
                "jsonrpc": "2.0",
                "method": "notifications/cancelled",
                "params": { "requestId": self.id, "reason": "client stopped waiting" },
            });
            let _ = self.transport.stdin_tx.send(note.to_string());
        }
        let id = self.id;
        if let Ok(mut pending) = self.transport.pending.try_lock() {
            pending.remove(&id);
            return;
        }
        let pending = Arc::clone(&self.transport.pending);
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            rt.spawn(async move {
                pending.lock().await.remove(&id);
            });
        }
    }
}

/// Where a shell would find a bare `command`, walking PATH × PATHEXT.
/// Windows process creation only tries `<name>.exe`, but `npx` (the usual
/// MCP launcher) and most Node and Python shims are `.cmd` files, so
/// `"command": "npx"` failed to spawn. Std quotes arguments safely when it
/// runs a `.cmd`/`.bat` by full path. The LSP tool spawns the same way.
#[cfg(any(windows, test))]
pub(crate) fn resolve_on_path(
    command: &str,
    path: Option<&std::ffi::OsStr>,
    pathext: Option<&std::ffi::OsStr>,
) -> Option<std::path::PathBuf> {
    if command.contains(['/', '\\', ':']) || std::path::Path::new(command).extension().is_some() {
        return None;
    }
    let exts = pathext
        .and_then(|e| e.to_str())
        .unwrap_or(".COM;.EXE;.BAT;.CMD");
    std::env::split_paths(path?).find_map(|dir| {
        exts.split(';')
            .filter(|e| !e.is_empty())
            .map(|ext| dir.join(format!("{command}{ext}")))
            .find(|candidate| candidate.is_file())
    })
}

#[async_trait]
impl McpTransport for StdioTransport {
    async fn call(&self, id: u64, method: &str, params: Value) -> Result<Value> {
        let req = JsonRpcRequest::new(id, method, params);
        let json = serde_json::to_string(&req)?;

        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.pending.lock().await;
            if self.closed.load(Ordering::SeqCst) {
                return Err(anyhow!("MCP server process exited"));
            }
            pending.insert(id, tx);
        }

        if self.stdin_tx.send(json).is_err() {
            self.pending.lock().await.remove(&id);
            return Err(anyhow!("MCP server stdin closed"));
        }

        // Fires on the timeout below and when the caller drops this future
        // (Esc, a cancelled turn or SDK/ACP request).
        let mut cancel = CancelOnDrop {
            transport: self,
            id,
            method,
            armed: true,
        };
        let limit = deadline(method, self.timeout, self.tool_timeout);
        match tokio::time::timeout(limit, rx).await {
            Ok(reply) => {
                cancel.armed = false;
                reply.map_err(|_| anyhow!("MCP server disconnected"))?
            }
            Err(_) => Err(anyhow!("MCP request timed out ({})", method)),
        }
    }

    async fn notify(&self, method: &str) {
        let req = JsonRpcRequest::notification(method);
        if let Ok(json) = serde_json::to_string(&req) {
            let _ = self.stdin_tx.send(json);
        }
    }

    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }
}

/// Our answer to a server-to-client request. We offer no client
/// capabilities, so only `ping` is ours to serve.
fn server_request_reply(id: &Value, method: &str) -> Value {
    if method == "ping" {
        json!({ "jsonrpc": "2.0", "id": id, "result": {} })
    } else {
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": -32601, "message": format!("Method not found: {method}") }
        })
    }
}

// ── HTTP transport ────────────────────────────────────────────────────────────

pub(crate) struct HttpTransport {
    url: String,
    client: reqwest::Client,
    /// `Mcp-Session-Id` a stateful Streamable-HTTP server assigns on
    /// `initialize`; every later request must echo it or the server
    /// answers 400.
    session_id: std::sync::Mutex<Option<String>>,
    /// Handshake-era version `initialize` settled on, sent as
    /// `MCP-Protocol-Version` on every later legacy request (required since
    /// 2025-06-18). Modern requests carry their own, from `_meta`.
    protocol_version: std::sync::Mutex<Option<String>>,
    /// Deadline for a whole exchange, body included. `send()` resolves at
    /// the headers, so a server that then stalls would otherwise hang the
    /// tool call forever; reqwest has no default read timeout.
    timeout: Duration,
    tool_timeout: Duration,
}

impl HttpTransport {
    // Auth is static by decision (2026-09-11): headers come from
    // settings.json → mcpServers.*.headers and are sent as-is. An expired
    // bearer token surfaces as a loud "HTTP MCP <method> failed: 401"; the
    // user replaces it and restarts. No OAuth discovery/PKCE/refresh flow is
    // planned — documented in SECURITY.md.
    pub fn new(url: &str, headers: &HashMap<String, String>) -> Result<Self> {
        Ok(Self {
            url: url.to_string(),
            client: client_with_headers(headers)?,
            session_id: std::sync::Mutex::new(None),
            protocol_version: std::sync::Mutex::new(None),
            timeout: REQUEST_TIMEOUT,
            tool_timeout: tool_timeout(),
        })
    }
}

/// An HTTP client that sends `headers` (static auth) on every request.
///
/// Redirects are followed only within the server's origin. On a cross-host
/// redirect reqwest strips `Authorization` and cookies but not keys in
/// custom headers (`X-API-Key`), a 307/308 resends the JSON-RPC body, and
/// nothing stops https → http. Anything else surfaces as an error naming
/// the target (`redirect_refused`).
fn client_with_headers(headers: &HashMap<String, String>) -> Result<reqwest::Client> {
    let mut builder =
        reqwest::Client::builder().redirect(reqwest::redirect::Policy::custom(|attempt| {
            let same_origin = attempt
                .previous()
                .first()
                .is_some_and(|first| first.origin() == attempt.url().origin());
            if attempt.previous().len() > 5 {
                attempt.error("too many redirects")
            } else if same_origin {
                attempt.follow()
            } else {
                attempt.stop()
            }
        }));
    if !headers.is_empty() {
        let mut header_map = reqwest::header::HeaderMap::new();
        for (k, v) in headers {
            let name = reqwest::header::HeaderName::from_bytes(k.as_bytes())
                .map_err(|e| anyhow!("Invalid MCP header name '{}': {}", k, e))?;
            let value = reqwest::header::HeaderValue::from_str(v)
                .map_err(|e| anyhow!("Invalid MCP header value: {}", e))?;
            header_map.insert(name, value);
        }
        builder = builder.default_headers(header_map);
    }
    Ok(builder.build()?)
}

/// For a 3xx the client did not follow: where it pointed, and why not.
fn redirect_refused(resp: &reqwest::Response) -> Option<String> {
    if !resp.status().is_redirection() {
        return None;
    }
    let to = resp
        .headers()
        .get(reqwest::header::LOCATION)?
        .to_str()
        .ok()?;
    Some(format!(
        "redirect to {to} not followed: it leaves the server's origin (put that URL in the \
         server's config if you trust it)"
    ))
}

/// Largest HTTP MCP response body we will buffer. Resources and tool
/// results are capped far below this before reaching the model; this is
/// the transport-level guard against a server that streams forever.
const MAX_HTTP_BODY_BYTES: usize = 32 * 1024 * 1024;

impl HttpTransport {
    fn session(&self) -> Option<String> {
        self.session_id
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    async fn post(
        &self,
        req: &JsonRpcRequest,
        extra: &[(String, String)],
    ) -> Result<reqwest::Response> {
        // Streamable-HTTP servers reject (406) a POST that does not accept
        // both; they may answer with plain JSON or an SSE stream.
        let mut builder = self
            .client
            .post(&self.url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream");
        if let Some(version) = modern_version(req.params.as_ref()) {
            // Mirrored from the body, so header and body always agree
            // (docs/specification/2026-07-28/basic/transports/streamable-http.mdx,
            // "Request Metadata"). No session: the revision has none.
            builder = builder
                .header("MCP-Protocol-Version", version)
                .header("Mcp-Method", &req.method);
            if let Some(name) = mcp_name(&req.method, req.params.as_ref()) {
                builder = builder.header("Mcp-Name", encode_header_value(name));
            }
        } else {
            let negotiated = self
                .protocol_version
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            if let Some(version) = negotiated {
                builder = builder.header("MCP-Protocol-Version", version);
            }
            if let Some(sid) = self.session() {
                builder = builder.header("Mcp-Session-Id", sid);
            }
        }
        for (name, value) in extra {
            builder = builder.header(name, value);
        }
        Ok(builder.json(req).send().await?)
    }

    /// Read a response body, refusing it once it exceeds the cap.
    async fn bounded_body(resp: reqwest::Response, method: &str) -> Result<Vec<u8>> {
        use tokio_stream::StreamExt;
        if let Some(len) = resp.content_length()
            && len > MAX_HTTP_BODY_BYTES as u64
        {
            return Err(anyhow!(
                "HTTP MCP {method} response too large: {len} bytes (limit {MAX_HTTP_BODY_BYTES})"
            ));
        }
        let mut body = Vec::new();
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            if body.len() + chunk.len() > MAX_HTTP_BODY_BYTES {
                return Err(anyhow!(
                    "HTTP MCP {method} response too large (limit {MAX_HTTP_BODY_BYTES} bytes)"
                ));
            }
            body.extend_from_slice(&chunk);
        }
        Ok(body)
    }

    /// Read an SSE response stream until the JSON-RPC response for `id`
    /// arrives. Servers may interleave notifications and their own requests
    /// before it, and need not close the stream afterwards.
    async fn sse_response(
        resp: reqwest::Response,
        id: u64,
        method: &str,
    ) -> Result<JsonRpcResponse> {
        use tokio_stream::StreamExt;
        let mut stream = resp.bytes_stream();
        let mut buf: Vec<u8> = Vec::new();
        let mut total = 0usize;
        // Bytes of `buf` already searched for an event end, so a large
        // event arriving in small chunks is not rescanned each time.
        let mut scanned = 0usize;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            total += chunk.len();
            if total > MAX_HTTP_BODY_BYTES {
                return Err(anyhow!(
                    "HTTP MCP {method} response too large (limit {MAX_HTTP_BODY_BYTES} bytes)"
                ));
            }
            buf.extend_from_slice(&chunk);
            while let Some((end, sep)) = sse_event_end(&buf, scanned) {
                let event: Vec<u8> = buf.drain(..end + sep).collect();
                scanned = 0;
                if let Some(r) = sse_event_response(&event[..end], id) {
                    return Ok(r);
                }
            }
            // A terminator may straddle the next chunk boundary.
            scanned = buf.len().saturating_sub(3);
        }
        // A final event may lack the trailing blank line.
        sse_event_response(&buf, id)
            .ok_or_else(|| anyhow!("HTTP MCP {method}: event stream ended without a response"))
    }
}

/// End of the first complete SSE event in `buf` (searching from `from`)
/// and the length of its blank-line terminator.
fn sse_event_end(buf: &[u8], from: usize) -> Option<(usize, usize)> {
    let find = |pat: &[u8]| {
        buf[from..]
            .windows(pat.len())
            .position(|w| w == pat)
            .map(|i| i + from)
    };
    match (find(b"\n\n"), find(b"\r\n\r\n")) {
        (Some(a), Some(b)) if b < a => Some((b, 4)),
        (Some(a), _) => Some((a, 2)),
        (None, Some(b)) => Some((b, 4)),
        (None, None) => None,
    }
}

/// One SSE event's type (`message` when it names none) and its `data`
/// lines joined, or `None` for an event without data (a comment, a retry).
fn sse_event_fields(event: &[u8]) -> Option<(String, String)> {
    let text = String::from_utf8_lossy(event);
    let field = |l: &str, name: &str| {
        l.strip_prefix(name)
            .and_then(|r| r.strip_prefix(':'))
            .map(|v| v.strip_prefix(' ').unwrap_or(v).to_string())
    };
    let mut kind = None;
    let mut data: Vec<String> = Vec::new();
    for l in text.split('\n').map(|l| l.trim_end_matches('\r')) {
        if let Some(d) = field(l, "data") {
            data.push(d);
        } else if let Some(e) = field(l, "event") {
            kind = Some(e);
        }
    }
    if data.is_empty() {
        return None;
    }
    Some((kind.unwrap_or_else(|| "message".into()), data.join("\n")))
}

/// The JSON-RPC response for `id` carried by one SSE event, if that is
/// what the event holds (not a notification or a server-to-client request).
fn sse_event_response(event: &[u8], id: u64) -> Option<JsonRpcResponse> {
    let (_, data) = sse_event_fields(event)?;
    let v: Value = serde_json::from_str(&data).ok()?;
    let is_response = v.get("result").is_some() || v.get("error").is_some();
    if !is_response || v.get("id").and_then(Value::as_u64) != Some(id) {
        return None;
    }
    serde_json::from_value(v).ok()
}

/// The `Mcp-Name` source: `params.name` or `params.uri` of the requests
/// that name a target.
fn mcp_name<'a>(method: &str, params: Option<&'a Value>) -> Option<&'a str> {
    let key = match method {
        "tools/call" | "prompts/get" => "name",
        "resources/read" => "uri",
        _ => return None,
    };
    params?.get(key)?.as_str()
}

/// A value fit for an `Mcp-Name` or `Mcp-Param-*` header: as-is when it is
/// plain visible ASCII, else `=?base64?…?=` of its UTF-8 bytes
/// (docs/specification/2026-07-28/basic/transports/streamable-http.mdx,
/// "Value Encoding").
fn encode_header_value(value: &str) -> String {
    use base64::Engine;
    let plain = value
        .bytes()
        .all(|b| b == b'\t' || (0x20..=0x7e).contains(&b))
        && !value.starts_with([' ', '\t'])
        && !value.ends_with([' ', '\t'])
        && !(value.starts_with("=?base64?") && value.ends_with("?="));
    if plain {
        value.to_string()
    } else {
        format!(
            "=?base64?{}?=",
            base64::engine::general_purpose::STANDARD.encode(value)
        )
    }
}

/// The `x-mcp-header` annotations of a tool's `inputSchema`: each header
/// name and the `properties` path of the argument it mirrors. `Err` names
/// why the definition is invalid, which over HTTP excludes the tool
/// (docs/specification/2026-07-28/basic/transports/streamable-http.mdx,
/// "Schema Extension").
fn x_mcp_headers(schema: &Value) -> std::result::Result<Vec<(String, Vec<String>)>, String> {
    fn walk(
        node: &Value,
        path: Option<&[String]>,
        out: &mut Vec<(String, Vec<String>)>,
    ) -> std::result::Result<(), String> {
        match node {
            Value::Array(items) => items.iter().try_for_each(|v| walk(v, None, out)),
            Value::Object(obj) => {
                if let Some(name) = obj.get("x-mcp-header") {
                    let Some(path) = path.filter(|p| !p.is_empty()) else {
                        return Err("x-mcp-header outside a chain of `properties`".into());
                    };
                    let name = name
                        .as_str()
                        .filter(|n| {
                            !n.is_empty()
                                && n.bytes().all(|b| {
                                    b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)
                                })
                        })
                        .ok_or_else(|| format!("x-mcp-header {name} is not a header token"))?;
                    let primitive = |t: &Value| {
                        matches!(t.as_str(), Some("string" | "integer" | "boolean" | "null"))
                    };
                    let ok_type = match obj.get("type") {
                        Some(Value::String(t)) => t != "null" && primitive(&obj["type"]),
                        Some(Value::Array(ts)) => {
                            ts.iter().all(primitive)
                                && ts.iter().any(|t| t.as_str() != Some("null"))
                        }
                        _ => false,
                    };
                    if !ok_type {
                        return Err(format!(
                            "x-mcp-header {name} is not on a string, integer or boolean"
                        ));
                    }
                    if out.iter().any(|(n, _)| n.eq_ignore_ascii_case(name)) {
                        return Err(format!("x-mcp-header {name} is not unique"));
                    }
                    out.push((name.to_string(), path.to_vec()));
                }
                for (key, value) in obj {
                    match key.as_str() {
                        // Instance data, not schemas.
                        "default" | "examples" | "const" | "enum" | "x-mcp-header" => {}
                        "properties" => {
                            let Some(props) = value.as_object() else {
                                continue;
                            };
                            for (prop, sub) in props {
                                let next = path.map(|p| {
                                    let mut p = p.to_vec();
                                    p.push(prop.clone());
                                    p
                                });
                                walk(sub, next.as_deref(), out)?;
                            }
                        }
                        _ => walk(value, None, out)?,
                    }
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }
    let mut out = Vec::new();
    walk(schema, Some(&[]), &mut out)?;
    Ok(out)
}

impl HttpTransport {
    async fn call_inner(
        &self,
        id: u64,
        method: &str,
        params: Value,
        extra: &[(String, String)],
    ) -> Result<Value> {
        let req = JsonRpcRequest::new(id, method, params);
        let modern = modern_version(req.params.as_ref()).is_some();
        let resp = self.post(&req, extra).await?;

        if !resp.status().is_success() {
            let status = resp.status();
            if status == reqwest::StatusCode::NOT_FOUND && !modern && self.session().is_some() {
                return Err(anyhow!(
                    "HTTP MCP {method} failed: session expired — restart oxideclaw to reconnect"
                ));
            }
            if let Some(why) = redirect_refused(&resp) {
                return Err(anyhow!("HTTP MCP {method} failed: {status} {why}"));
            }
            let body = Self::bounded_body(resp, method).await.unwrap_or_default();
            // Modern servers explain a 4xx with a JSON-RPC error; keep it
            // typed, since its code tells the eras apart.
            if let Ok(JsonRpcResponse {
                error: Some(err), ..
            }) = serde_json::from_slice(&body)
            {
                let mut err = RpcError::from(err);
                err.context = Some(format!("HTTP MCP {method} failed: {status}"));
                return Err(err.into());
            }
            let body = String::from_utf8_lossy(&body);
            return Err(anyhow!("HTTP MCP {} failed: {} — {}", method, status, body));
        }

        if let Some(sid) = resp
            .headers()
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
            .filter(|_| !modern)
        {
            let mut stored = self.session_id.lock().unwrap_or_else(|e| e.into_inner());
            if stored.is_none() {
                *stored = Some(sid.to_string());
            }
        }

        let is_sse = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|ct| {
                ct.trim()
                    .to_ascii_lowercase()
                    .starts_with("text/event-stream")
            });
        let rpc_resp: JsonRpcResponse = if is_sse {
            Self::sse_response(resp, id, method).await?
        } else {
            let body = Self::bounded_body(resp, method).await?;
            serde_json::from_slice(&body)?
        };

        if let Some(err) = rpc_resp.error {
            return Err(RpcError::from(err).into());
        }

        Ok(rpc_resp.result.unwrap_or(Value::Null))
    }
}

#[async_trait]
impl McpTransport for HttpTransport {
    async fn call(&self, id: u64, method: &str, params: Value) -> Result<Value> {
        self.call_with_headers(id, method, params, &[]).await
    }

    async fn call_with_headers(
        &self,
        id: u64,
        method: &str,
        params: Value,
        headers: &[(String, String)],
    ) -> Result<Value> {
        let limit = deadline(method, self.timeout, self.tool_timeout);
        tokio::time::timeout(limit, self.call_inner(id, method, params, headers))
            .await
            .map_err(|_| anyhow!("HTTP MCP request timed out ({method})"))?
    }

    /// Streamable-HTTP servers expect `notifications/initialized` like any
    /// other transport; a notification has no id and its reply is ignored.
    async fn notify(&self, method: &str) {
        let req = JsonRpcRequest::notification(method);
        let _ = tokio::time::timeout(self.timeout, self.post(&req, &[])).await;
    }

    fn set_protocol_version(&self, version: &str) {
        *self
            .protocol_version
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(version.to_string());
    }
}

// ── Legacy HTTP+SSE transport ─────────────────────────────────────────────────

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value>>>>>;

/// MCP's HTTP+SSE transport (protocol 2024-11-05, since replaced by
/// Streamable HTTP), which ACP hosts may still pass as `type: "sse"`. A GET
/// opens an event stream whose first `endpoint` event names the URL to POST
/// messages to; the answers come back on that stream, not in the POST's
/// reply.
pub(crate) struct SseTransport {
    endpoint: reqwest::Url,
    client: reqwest::Client,
    pending: Pending,
    /// Set by the reader, under the `pending` lock, once the stream ends.
    closed: Arc<AtomicBool>,
    reader: tokio::task::JoinHandle<()>,
    timeout: Duration,
    tool_timeout: Duration,
}

impl Drop for SseTransport {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

impl SseTransport {
    pub async fn connect(url: &str, headers: &HashMap<String, String>) -> Result<Self> {
        Self::connect_with_timeout(url, headers, REQUEST_TIMEOUT).await
    }

    async fn connect_with_timeout(
        url: &str,
        headers: &HashMap<String, String>,
        timeout: Duration,
    ) -> Result<Self> {
        use tokio_stream::StreamExt;
        let base = reqwest::Url::parse(url).map_err(|e| anyhow!("bad SSE MCP url {url}: {e}"))?;
        let client = client_with_headers(headers)?;
        let open = async {
            let resp = client
                .get(base.clone())
                .header("Accept", "text/event-stream")
                .send()
                .await?;
            if !resp.status().is_success() {
                let why = redirect_refused(&resp)
                    .map(|why| format!(" {why}"))
                    .unwrap_or_default();
                return Err(anyhow!("SSE MCP stream failed: {}{why}", resp.status()));
            }
            let mut stream = Box::pin(resp.bytes_stream());
            let mut buf: Vec<u8> = Vec::new();
            // As in `sse_response`: a large event arriving in small chunks
            // is not rescanned from its start on every chunk.
            let mut scanned = 0usize;
            loop {
                while let Some((end, sep)) = sse_event_end(&buf, scanned) {
                    let event: Vec<u8> = buf.drain(..end + sep).collect();
                    scanned = 0;
                    if let Some((kind, data)) = sse_event_fields(&event[..end])
                        && kind == "endpoint"
                    {
                        return Ok((data, stream, buf));
                    }
                }
                scanned = buf.len().saturating_sub(3);
                match stream.next().await {
                    Some(chunk) => {
                        buf.extend_from_slice(&chunk?);
                        if buf.len() > MAX_HTTP_BODY_BYTES {
                            return Err(anyhow!("SSE MCP stream sent no endpoint event"));
                        }
                    }
                    None => return Err(anyhow!("SSE MCP stream ended before its endpoint event")),
                }
            }
        };
        let (data, mut stream, mut buf) = tokio::time::timeout(timeout, open)
            .await
            .map_err(|_| anyhow!("SSE MCP server sent no endpoint event"))??;
        let endpoint = base
            .join(data.trim())
            .map_err(|e| anyhow!("SSE MCP endpoint {data:?}: {e}"))?;
        // The headers (often a bearer token) go to every POST: never to a
        // host other than the one the user named.
        if endpoint.origin() != base.origin() {
            return Err(anyhow!(
                "SSE MCP endpoint {endpoint} is not on the server's origin {}",
                base.origin().ascii_serialization()
            ));
        }

        let pending: Pending = Arc::default();
        let closed = Arc::new(AtomicBool::new(false));
        let reader = {
            let (pending, closed) = (pending.clone(), closed.clone());
            let (client, endpoint) = (client.clone(), endpoint.clone());
            tokio::spawn(async move {
                let mut scanned = 0usize;
                loop {
                    while let Some((end, sep)) = sse_event_end(&buf, scanned) {
                        let event: Vec<u8> = buf.drain(..end + sep).collect();
                        scanned = 0;
                        let Some((kind, data)) = sse_event_fields(&event[..end]) else {
                            continue;
                        };
                        if kind != "message" {
                            continue;
                        }
                        let Ok(msg) = serde_json::from_str::<Value>(&data) else {
                            continue;
                        };
                        Self::dispatch(msg, &pending, &client, &endpoint).await;
                    }
                    // A terminator may straddle the next chunk boundary.
                    scanned = buf.len().saturating_sub(3);
                    match stream.next().await {
                        Some(Ok(chunk)) if buf.len() + chunk.len() <= MAX_HTTP_BODY_BYTES => {
                            buf.extend_from_slice(&chunk);
                        }
                        Some(Ok(_)) => {
                            tracing::warn!("SSE MCP: event over {MAX_HTTP_BODY_BYTES} bytes");
                            break;
                        }
                        _ => break,
                    }
                }
                // Dropping the senders fails every waiting call at once.
                let mut p = pending.lock().await;
                closed.store(true, Ordering::SeqCst);
                p.clear();
            })
        };
        Ok(Self {
            endpoint,
            client,
            pending,
            closed,
            reader,
            timeout,
            tool_timeout: tool_timeout(),
        })
    }

    /// Route one message from the stream: a response to its caller, a
    /// server request to our fixed answer; notifications are dropped.
    async fn dispatch(
        msg: Value,
        pending: &Pending,
        client: &reqwest::Client,
        endpoint: &reqwest::Url,
    ) {
        if let Some(method) = msg.get("method").and_then(Value::as_str) {
            if let Some(id) = msg.get("id") {
                let reply = server_request_reply(id, method);
                let (client, endpoint) = (client.clone(), endpoint.clone());
                tokio::spawn(async move {
                    let _ = tokio::time::timeout(
                        REQUEST_TIMEOUT,
                        client.post(endpoint).json(&reply).send(),
                    )
                    .await;
                });
            }
            return;
        }
        let Some(id) = msg.get("id").and_then(Value::as_u64) else {
            return;
        };
        let Some(tx) = pending.lock().await.remove(&id) else {
            return;
        };
        let reply = match serde_json::from_value::<JsonRpcResponse>(msg) {
            Ok(r) => match r.error {
                Some(err) => Err(RpcError::from(err).into()),
                None => Ok(r.result.unwrap_or(Value::Null)),
            },
            Err(e) => Err(anyhow!("malformed MCP response: {e}")),
        };
        let _ = tx.send(reply);
    }

    async fn post(&self, body: &JsonRpcRequest, method: &str) -> Result<()> {
        let resp = self
            .client
            .post(self.endpoint.clone())
            .json(body)
            .send()
            .await?;
        if !resp.status().is_success() {
            let status = resp.status();
            if let Some(why) = redirect_refused(&resp) {
                return Err(anyhow!("SSE MCP {method} failed: {status} {why}"));
            }
            let body = HttpTransport::bounded_body(resp, method)
                .await
                .unwrap_or_default();
            return Err(anyhow!(
                "SSE MCP {method} failed: {status} — {}",
                String::from_utf8_lossy(&body)
            ));
        }
        Ok(())
    }
}

#[async_trait]
impl McpTransport for SseTransport {
    async fn call(&self, id: u64, method: &str, params: Value) -> Result<Value> {
        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.pending.lock().await;
            if self.closed.load(Ordering::SeqCst) {
                return Err(anyhow!("SSE MCP stream closed"));
            }
            pending.insert(id, tx);
        }
        let req = JsonRpcRequest::new(id, method, params);
        let exchange = async {
            self.post(&req, method).await?;
            rx.await
                .map_err(|_| anyhow!("SSE MCP stream closed before the {method} response"))?
        };
        let limit = deadline(method, self.timeout, self.tool_timeout);
        let out = match tokio::time::timeout(limit, exchange).await {
            Ok(r) => r,
            Err(_) => Err(anyhow!("SSE MCP request timed out ({method})")),
        };
        if out.is_err() {
            self.pending.lock().await.remove(&id);
        }
        out
    }

    async fn notify(&self, method: &str) {
        let req = JsonRpcRequest::notification(method);
        let _ = tokio::time::timeout(self.timeout, self.post(&req, method)).await;
    }
}

// ── McpClient ─────────────────────────────────────────────────────────────────

pub struct McpClient {
    pub server_name: String,
    pub tools: Vec<McpToolDef>,
    pub transport_kind: &'static str, // "stdio" | "http" | "sse"
    /// What the handshake settled on; fixed for the life of the connection.
    pub protocol: Protocol,
    transport: Box<dyn McpTransport>,
    next_id: AtomicU64,
    /// `None` skips the `server/discover` probe and goes straight to
    /// `initialize`.
    probe_timeout: Option<Duration>,
}

/// What the `server/discover` probe says about a server.
enum Era {
    /// Modern, with the capabilities its `DiscoverResult` lists.
    Modern(Value),
    /// Handshake-era; the version to offer in `initialize`.
    Legacy(&'static str),
}

impl McpClient {
    fn new(
        server_name: String,
        transport_kind: &'static str,
        transport: Box<dyn McpTransport>,
        probe_timeout: Option<Duration>,
    ) -> Self {
        Self {
            server_name,
            tools: Vec::new(),
            transport_kind,
            protocol: Protocol::Pending,
            transport,
            next_id: AtomicU64::new(1),
            probe_timeout,
        }
    }

    // ── Internal helpers ──────────────────────────────────────────────────────

    async fn send(
        &self,
        method: &str,
        params: Value,
        headers: &[(String, String)],
    ) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.transport
            .call_with_headers(id, method, params, headers)
            .await
    }

    /// One request in the negotiated era. A modern request carries `_meta`,
    /// and an `input_required` answer is retried with the client's answers
    /// (docs/specification/2026-07-28/basic/patterns/mrtr.mdx).
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        if self.protocol != Protocol::Modern {
            return self.send(method, params, &[]).await;
        }
        let headers = self.param_headers(method, &params);
        let mut params = with_modern_meta(params);
        for _ in 0..MAX_INPUT_ROUNDS {
            let result = self.send(method, params.clone(), &headers).await?;
            // An absent `resultType` is "complete" (basic/index.mdx,
            // "ResultType"); any value not defined for this request is
            // invalid.
            match result.get("resultType") {
                None => return Ok(result),
                Some(t) if t == "complete" => return Ok(result),
                Some(t) if t == "input_required" && INPUT_REQUIRED_METHODS.contains(&method) => {
                    self.answer_input_requests(method, &result, &mut params)?;
                }
                Some(t) => {
                    return Err(anyhow!(
                        "MCP server '{}' answered {method} with an invalid resultType {t}",
                        self.server_name
                    ));
                }
            }
        }
        Err(anyhow!(
            "MCP server '{}' still wanted more input for {method} after {MAX_INPUT_ROUNDS} rounds",
            self.server_name
        ))
    }

    /// Turn an `InputRequiredResult` into the retry's params. Nothing here
    /// can ask the user or a model, so an elicitation is declined (the
    /// server then decides what to return); a sampling or roots request
    /// ends the call, which needs no reply to the server
    /// (client/sampling.mdx and client/roots.mdx, "Error Handling").
    fn answer_input_requests(
        &self,
        method: &str,
        result: &Value,
        params: &mut Value,
    ) -> Result<()> {
        let Some(obj) = params.as_object_mut() else {
            return Err(anyhow!("MCP {method}: params are not an object"));
        };
        obj.remove("inputResponses");
        obj.remove("requestState");
        if let Some(requests) = result.get("inputRequests") {
            let requests = requests.as_object().ok_or_else(|| {
                anyhow!("MCP server '{}': malformed inputRequests", self.server_name)
            })?;
            let mut responses = serde_json::Map::new();
            for (key, request) in requests {
                match request.get("method").and_then(Value::as_str) {
                    Some("elicitation/create") => {
                        responses.insert(key.clone(), json!({ "action": "decline" }));
                    }
                    other => {
                        return Err(anyhow!(
                            "MCP server '{}' needs {} to finish {method}, which oxideclaw does not provide",
                            self.server_name,
                            other.unwrap_or("an unnamed request")
                        ));
                    }
                }
            }
            obj.insert("inputResponses".into(), Value::Object(responses));
        }
        // Echoed exactly, and only when sent.
        if let Some(state) = result.get("requestState") {
            obj.insert("requestState".into(), state.clone());
        }
        Ok(())
    }

    /// `Mcp-Param-*` headers a modern Streamable HTTP `tools/call` must carry
    /// for the tool's `x-mcp-header` arguments.
    fn param_headers(&self, method: &str, params: &Value) -> Vec<(String, String)> {
        if self.transport_kind != "http" || method != "tools/call" {
            return Vec::new();
        }
        let Some(tool) = params
            .get("name")
            .and_then(Value::as_str)
            .and_then(|n| self.tools.iter().find(|t| t.name == n))
        else {
            return Vec::new();
        };
        let Ok(annotated) = x_mcp_headers(&tool.input_schema) else {
            return Vec::new();
        };
        let Some(args) = params.get("arguments") else {
            return Vec::new();
        };
        annotated
            .into_iter()
            .filter_map(|(name, path)| {
                let value = path.iter().try_fold(args, |v, k| v.get(k))?;
                let text = match value {
                    Value::String(s) => s.clone(),
                    Value::Number(n) => n.to_string(),
                    Value::Bool(b) => b.to_string(),
                    _ => return None, // null or absent: no header
                };
                Some((format!("Mcp-Param-{name}"), encode_header_value(&text)))
            })
            .collect()
    }

    /// Call a paginated MCP `*/list` endpoint and accumulate every page.
    ///
    /// MCP servers return `{ "<field>": [...], "nextCursor": "..." }`. If
    /// `nextCursor` is present, the client MUST repeat the request with
    /// `params = { "cursor": "<nextCursor>" }` until `nextCursor` is absent
    /// or empty. Previously we only fetched the first page, silently
    /// truncating tool/resource lists for any server with more than one
    /// page's worth of items (spec §Pagination).
    async fn list_paginated(&self, method: &str, field: &str) -> Result<Vec<Value>> {
        let mut out: Vec<Value> = Vec::new();
        let mut cursor: Option<String> = None;
        // Hard cap on pages as a safety rail — a buggy server that always
        // returns the same nextCursor would otherwise loop forever.
        const MAX_PAGES: usize = 256;

        for _ in 0..MAX_PAGES {
            let params = match &cursor {
                Some(c) => json!({ "cursor": c }),
                None => json!({}),
            };
            let result = self.request(method, params).await?;

            if let Some(arr) = result.get(field).and_then(|v| v.as_array()) {
                out.extend(arr.iter().cloned());
            }

            cursor = result
                .get("nextCursor")
                .and_then(|c| c.as_str())
                .filter(|s| !s.is_empty())
                .map(String::from);

            if cursor.is_none() {
                return Ok(out);
            }
        }
        // Hit the page cap — return what we have with a warn but don't fail.
        tracing::warn!(
            "MCP {method} for '{}' exceeded {MAX_PAGES} pages — possible server bug, truncating",
            self.server_name
        );
        Ok(out)
    }

    /// Settle the protocol era, then populate self.tools.
    async fn init(&mut self) -> Result<()> {
        let capabilities = self.handshake().await?;

        // Fetch tool list — with cursor pagination so servers that return
        // more than one page worth of tools aren't silently truncated.
        // A failure leaves the server connected (resource- or prompt-only
        // servers answer -32601), but say so when it claims tools: a
        // silent "connected (0 tools)" hid real transport errors.
        let pages = match self.list_paginated("tools/list", "tools").await {
            Ok(pages) => pages,
            Err(e) => {
                if capabilities.get("tools").is_some() {
                    tracing::warn!("MCP '{}': tools/list failed: {}", self.server_name, e);
                } else {
                    tracing::debug!("MCP '{}': tools/list failed: {}", self.server_name, e);
                }
                Vec::new()
            }
        };
        let modern_http = self.protocol == Protocol::Modern && self.transport_kind == "http";
        self.tools = pages
            .into_iter()
            .filter_map(|v| serde_json::from_value::<McpToolDef>(v).ok())
            // Over Streamable HTTP a tool with a bad `x-mcp-header` must be
            // left out, not fail the whole list (streamable-http.mdx,
            // "Schema Extension").
            .filter(|t| match x_mcp_headers(&t.input_schema) {
                Err(why) if modern_http => {
                    tracing::warn!(
                        "MCP '{}': tool '{}' left out: {why}",
                        self.server_name,
                        t.name
                    );
                    false
                }
                _ => true,
            })
            .collect();

        Ok(())
    }

    /// Find out which era the server speaks and get it ready for requests;
    /// returns the server's capabilities. The answer holds for the life of
    /// the connection (basic/versioning.mdx, "Backward Compatibility with
    /// Initialization-Based Versions").
    async fn handshake(&mut self) -> Result<Value> {
        let (protocol, capabilities) = match self.probe_timeout {
            None => self.initialize(LEGACY_PROTOCOL_VERSIONS[0]).await?,
            Some(wait) => self.probe_then_handshake(wait).await?,
        };
        self.protocol = protocol;
        Ok(capabilities)
    }

    /// Probe with `server/discover` carrying the modern version
    /// (basic/transports/stdio.mdx and streamable-http.mdx, "Backward
    /// Compatibility"): a `DiscoverResult` or a recognized modern error
    /// means modern; any other error, an HTTP 4xx without a modern error
    /// body, or silence means legacy, so `initialize` follows.
    async fn probe_then_handshake(&self, wait: Duration) -> Result<(Protocol, Value)> {
        let probe = self.send("server/discover", with_modern_meta(json!({})), &[]);
        tokio::pin!(probe);
        let offer = match tokio::time::timeout(wait, &mut probe).await {
            Ok(reply) => match self.read_probe(reply)? {
                Era::Modern(capabilities) => return Ok((Protocol::Modern, capabilities)),
                Era::Legacy(offer) => offer,
            },
            Err(_) => {
                let legacy = self.initialize(LEGACY_PROTOCOL_VERSIONS[0]).await;
                let Err(e) = legacy else {
                    return legacy;
                };
                // A recognized modern error to `initialize` identifies a
                // modern server, which must not be treated as legacy
                // (versioning.mdx), even while the probe is still out.
                if let Ok(Era::Modern(capabilities)) = self.read_probe_error(&e) {
                    return Ok((Protocol::Modern, capabilities));
                }
                // A modern server that was slow to start has answered the
                // probe by now, and rejected `initialize`.
                if let Ok(reply) = tokio::time::timeout(LATE_PROBE_GRACE, &mut probe).await
                    && let Ok(Era::Modern(capabilities)) = self.read_probe(reply)
                {
                    return Ok((Protocol::Modern, capabilities));
                }
                // A slow-starting server read the queued probe and quit.
                return Err(if self.transport.is_closed() {
                    ProbeExited.into()
                } else {
                    e
                });
            }
        };
        self.initialize(offer).await.map_err(|e| {
            if self.transport.is_closed() {
                ProbeExited.into()
            } else {
                e
            }
        })
    }

    /// Classify the probe's answer.
    fn read_probe(&self, reply: Result<Value>) -> Result<Era> {
        match reply {
            Ok(result) => match result.get("supportedVersions").and_then(Value::as_array) {
                Some(versions) => self.pick_version(
                    versions,
                    result.get("capabilities").cloned().unwrap_or(json!({})),
                ),
                // Not a DiscoverResult: a server that answers anything.
                None => Ok(Era::Legacy(LEGACY_PROTOCOL_VERSIONS[0])),
            },
            Err(e) => self.read_probe_error(&e),
        }
    }

    /// Classify an error the server sent: one of the revision's own errors
    /// means modern, anything else legacy.
    fn read_probe_error(&self, error: &anyhow::Error) -> Result<Era> {
        match error.downcast_ref::<RpcError>() {
            // Modern, but not this version: use one it lists.
            Some(rpc) if rpc.code == UNSUPPORTED_PROTOCOL_VERSION => {
                let supported = rpc
                    .data
                    .as_ref()
                    .and_then(|d| d.get("supported"))
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                self.pick_version(&supported, json!({}))
            }
            Some(rpc)
                if rpc.code == HEADER_MISMATCH
                    || rpc.code == MISSING_REQUIRED_CLIENT_CAPABILITY =>
            {
                Err(anyhow!(
                    "MCP server '{}' rejected server/discover: {rpc}",
                    self.server_name
                ))
            }
            // The fallback must not key on one code (stdio.mdx).
            _ => Ok(Era::Legacy(LEGACY_PROTOCOL_VERSIONS[0])),
        }
    }

    /// The newest revision both sides speak, from the server's list.
    fn pick_version(&self, supported: &[Value], capabilities: Value) -> Result<Era> {
        let has = |v: &str| supported.iter().any(|s| s.as_str() == Some(v));
        if has(MODERN_PROTOCOL_VERSION) {
            return Ok(Era::Modern(capabilities));
        }
        if let Some(v) = LEGACY_PROTOCOL_VERSIONS.iter().find(|v| has(v)) {
            return Ok(Era::Legacy(v));
        }
        Err(anyhow!(
            "MCP server '{}' speaks protocol versions {}; oxideclaw speaks {MODERN_PROTOCOL_VERSION}, {}",
            self.server_name,
            Value::Array(supported.to_vec()),
            LEGACY_PROTOCOL_VERSIONS.join(", ")
        ))
    }

    /// The handshake-era `initialize` / `notifications/initialized` pair,
    /// offering `offer` and taking the version the server answers.
    async fn initialize(&self, offer: &str) -> Result<(Protocol, Value)> {
        let params = json!({
            "protocolVersion": offer,
            // Nothing here answers roots/list or sampling/createMessage; a
            // server told we do waits on them until its own timeout.
            "capabilities": {},
            "clientInfo": client_info()
        });
        let init = self.send("initialize", params, &[]).await.map_err(|e| {
            // Context, not a new error: the RpcError stays readable for
            // era detection after a probe timeout.
            let msg = format!("MCP initialize failed for '{}': {}", self.server_name, e);
            e.context(msg)
        })?;
        let version = init
            .get("protocolVersion")
            .and_then(Value::as_str)
            .unwrap_or(offer)
            .to_string();
        // A version this client does not know gets no header: the server
        // would only reject it.
        if LEGACY_PROTOCOL_VERSIONS.contains(&version.as_str()) {
            self.transport.set_protocol_version(&version);
        }
        // Notify server that client is ready (fire-and-forget)
        self.transport.notify("notifications/initialized").await;
        let capabilities = init.get("capabilities").cloned().unwrap_or(json!({}));
        Ok((Protocol::Legacy(version), capabilities))
    }

    // ── Public API ────────────────────────────────────────────────────────────

    /// The negotiated protocol revision, for `/mcp` and `mcp list`.
    pub fn protocol_revision(&self) -> &str {
        self.protocol.revision()
    }

    /// Connect to a stdio MCP server.
    pub async fn connect_stdio(
        server_name: String,
        command: &str,
        args: &[String],
        env: &HashMap<String, String>,
        cwd: &std::path::Path,
    ) -> Result<Self> {
        Self::connect_stdio_probing(server_name, command, args, env, cwd, DISCOVER_PROBE_TIMEOUT)
            .await
    }

    async fn connect_stdio_probing(
        server_name: String,
        command: &str,
        args: &[String],
        env: &HashMap<String, String>,
        cwd: &std::path::Path,
        probe_timeout: Duration,
    ) -> Result<Self> {
        let transport = StdioTransport::connect(command, args, env, cwd).await?;
        let mut client = Self::new(
            server_name,
            "stdio",
            Box::new(transport),
            Some(probe_timeout),
        );
        match client.init().await {
            // A handshake-era server that quits on a request before
            // `initialize`: start it again and go straight to the handshake.
            Err(e) if e.is::<ProbeExited>() => {
                client.transport =
                    Box::new(StdioTransport::connect(command, args, env, cwd).await?);
                client.probe_timeout = None;
                client.init().await?;
            }
            other => other?,
        }
        Ok(client)
    }

    /// Connect to an HTTP MCP server (streamable HTTP transport).
    pub async fn connect_http(
        server_name: String,
        url: &str,
        headers: &HashMap<String, String>,
    ) -> Result<Self> {
        let transport = HttpTransport::new(url, headers)?;
        let mut client = Self::new(
            server_name,
            "http",
            Box::new(transport),
            Some(DISCOVER_PROBE_TIMEOUT),
        );
        client.init().await?;
        Ok(client)
    }

    /// Connect to an MCP server over the legacy HTTP+SSE transport. It
    /// predates the stateless revision, so there is nothing to probe.
    pub async fn connect_sse(
        server_name: String,
        url: &str,
        headers: &HashMap<String, String>,
    ) -> Result<Self> {
        let transport = SseTransport::connect(url, headers).await?;
        let mut client = Self::new(server_name, "sse", Box::new(transport), None);
        client.init().await?;
        Ok(client)
    }

    /// List MCP resources exposed by this server.
    pub async fn list_resources(&self) -> Result<Vec<McpResource>> {
        let pages = self.list_paginated("resources/list", "resources").await?;
        let resources: Vec<McpResource> = pages
            .into_iter()
            .filter_map(|v| serde_json::from_value(v).ok())
            .collect();
        Ok(resources)
    }

    /// Read an MCP resource by URI.
    pub async fn read_resource(&self, uri: &str) -> Result<String> {
        let result = self
            .request("resources/read", json!({ "uri": uri }))
            .await?;

        // MCP resources/read returns { contents: [{ uri, text?, blob? }] }
        let text = result
            .get("contents")
            .and_then(|c| c.as_array())
            .map(|arr| {
                arr.iter()
                    .map(render_resource)
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_else(|| serde_json::to_string_pretty(&result).unwrap_or_default());

        // Same ceiling as tool results: one large resource must not flood
        // the context window.
        Ok(cap_output(text, DEFAULT_MAX_RESULT_CHARS))
    }

    /// Execute an MCP tool call and return the text output.
    pub async fn call_tool(&self, tool_name: &str, arguments: Value) -> Result<String> {
        let result = self
            .request(
                "tools/call",
                json!({ "name": tool_name, "arguments": arguments }),
            )
            .await?;

        // Deserialize the result
        let call_result: McpCallResult =
            serde_json::from_value(result.clone()).unwrap_or(McpCallResult {
                content: vec![],
                is_error: false,
            });

        let text = call_result
            .content
            .iter()
            .map(render_content_item)
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("\n");

        // Fall back to raw JSON if nothing in the content array rendered
        let mut output = if text.is_empty() {
            serde_json::to_string_pretty(&result).unwrap_or_default()
        } else {
            text
        };

        // Respect _meta["anthropic/maxResultSizeChars"] from the tool definition.
        // This allows servers to declare a larger limit (up to 500K), otherwise we
        // cap at 25K to avoid flooding the context window.
        let max_chars = self
            .tools
            .iter()
            .find(|t| t.name == tool_name)
            .map(|t| t.max_result_chars())
            .unwrap_or(DEFAULT_MAX_RESULT_CHARS);
        output = cap_output(output, max_chars);

        if call_result.is_error {
            Err(anyhow!("MCP tool '{}' error: {}", tool_name, output))
        } else {
            Ok(output)
        }
    }
}

/// One `resources/read` content, or the `resource` of an embedded-resource
/// item: its text, or a placeholder for a blob. Tool output is text-only, so
/// base64 would only burn context.
fn render_resource(res: &Value) -> String {
    if let Some(text) = res.get("text").and_then(Value::as_str) {
        return text.to_string();
    }
    let field = |k: &str| res.get(k).and_then(Value::as_str);
    format!(
        "[binary resource {} ({}, {} bytes base64 omitted)]",
        field("uri").unwrap_or("?"),
        field("mimeType").unwrap_or("unknown type"),
        field("blob").map_or(0, str::len)
    )
}

/// Text for one tool-result content item. Servers such as github-mcp-server
/// return a file as an embedded `resource`; reading only `text` dropped it.
fn render_content_item(item: &Value) -> String {
    let field = |k: &str| item.get(k).and_then(Value::as_str);
    match field("type") {
        Some("text") => field("text").unwrap_or_default().to_string(),
        Some("resource") => item
            .get("resource")
            .map(render_resource)
            .unwrap_or_default(),
        Some("resource_link") => format!("[resource link: {}]", field("uri").unwrap_or("?")),
        Some(kind @ ("image" | "audio")) => format!(
            "[{kind} {}, {} bytes base64 omitted]",
            field("mimeType").unwrap_or("unknown type"),
            field("data").map_or(0, str::len)
        ),
        _ => item.to_string(),
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    /// Fake transport that returns scripted responses for a given method.
    /// Records every request it sees (for assertion ordering and cursor flow).
    struct MockTransport {
        /// Per-method response queues. Each call to `call()` pops the next
        /// response from the queue for that method.
        responses: StdMutex<HashMap<String, Vec<Value>>>,
        /// Recorded (method, params) tuples in call order.
        calls: StdMutex<Vec<(String, Value)>>,
    }

    impl MockTransport {
        fn new(responses: HashMap<String, Vec<Value>>) -> Self {
            Self {
                responses: StdMutex::new(responses),
                calls: StdMutex::new(Vec::new()),
            }
        }

        fn calls(&self) -> Vec<(String, Value)> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl McpTransport for MockTransport {
        async fn call(&self, _id: u64, method: &str, params: Value) -> Result<Value> {
            self.calls
                .lock()
                .unwrap()
                .push((method.to_string(), params));
            let mut queues = self.responses.lock().unwrap();
            let queue = queues
                .get_mut(method)
                .ok_or_else(|| anyhow!("mock: unexpected method '{}'", method))?;
            if queue.is_empty() {
                return Err(anyhow!("mock: response queue empty for '{}'", method));
            }
            Ok(queue.remove(0))
        }

        async fn notify(&self, _method: &str) {}
    }

    fn client_with_mock(mock: MockTransport) -> McpClient {
        McpClient::new("mock".into(), "stdio", Box::new(mock), None)
    }

    #[tokio::test]
    async fn tools_list_follows_next_cursor_across_pages() {
        // Three pages of tools. Pages 1 and 2 return `nextCursor`; page 3
        // omits it (end of list).
        let page1 = json!({
            "tools": [
                { "name": "alpha", "description": "a", "inputSchema": { "type": "object" } },
                { "name": "beta",  "description": "b", "inputSchema": { "type": "object" } }
            ],
            "nextCursor": "cur-2"
        });
        let page2 = json!({
            "tools": [
                { "name": "gamma", "description": "c", "inputSchema": { "type": "object" } }
            ],
            "nextCursor": "cur-3"
        });
        let page3 = json!({
            "tools": [
                { "name": "delta", "description": "d", "inputSchema": { "type": "object" } }
            ]
            // no nextCursor → end
        });

        let mut responses = HashMap::new();
        responses.insert("tools/list".to_string(), vec![page1, page2, page3]);
        let mock = MockTransport::new(responses);
        let client = client_with_mock(mock);

        let all = client
            .list_paginated("tools/list", "tools")
            .await
            .expect("paginated call succeeds");

        assert_eq!(all.len(), 4, "all 4 tools across 3 pages must be returned");
        let names: Vec<String> = all
            .iter()
            .filter_map(|v| v.get("name").and_then(|n| n.as_str()).map(String::from))
            .collect();
        assert_eq!(names, vec!["alpha", "beta", "gamma", "delta"]);
        // Explicit cursor-param verification lives in the separate
        // `list_paginated_sends_cursor_param_on_each_follow_up` test below,
        // which keeps an Arc<MockTransport> handle so it can inspect calls.
    }

    #[tokio::test]
    async fn list_paginated_sends_cursor_param_on_each_follow_up() {
        // Explicit check: second and third requests must carry
        // `{ "cursor": "<prev-nextCursor>" }`, first carries `{}`.
        let page1 = json!({ "tools": [], "nextCursor": "cur-2" });
        let page2 = json!({ "tools": [], "nextCursor": "cur-3" });
        let page3 = json!({ "tools": [] });

        let mut responses = HashMap::new();
        responses.insert("tools/list".to_string(), vec![page1, page2, page3]);
        let mock = Arc::new(MockTransport::new(responses));
        let mock_handle = Arc::clone(&mock);

        // Build a client that wraps the Arc via a thin adapter.
        struct ArcAdapter(Arc<MockTransport>);
        #[async_trait]
        impl McpTransport for ArcAdapter {
            async fn call(&self, id: u64, method: &str, params: Value) -> Result<Value> {
                self.0.call(id, method, params).await
            }
        }

        let client = McpClient::new("mock".into(), "stdio", Box::new(ArcAdapter(mock)), None);

        let all = client
            .list_paginated("tools/list", "tools")
            .await
            .expect("ok");
        assert!(all.is_empty());

        let calls = mock_handle.calls();
        assert_eq!(calls.len(), 3, "exactly 3 pages fetched");
        assert_eq!(calls[0].0, "tools/list");
        assert_eq!(calls[0].1, json!({}));
        assert_eq!(calls[1].1, json!({ "cursor": "cur-2" }));
        assert_eq!(calls[2].1, json!({ "cursor": "cur-3" }));
    }

    #[tokio::test]
    async fn list_paginated_stops_on_empty_next_cursor_string() {
        // Edge case: server returns `"nextCursor": ""`. Spec-compliant clients
        // treat empty string as "no more pages" — we must not loop.
        let page1 = json!({
            "resources": [ { "uri": "file://a", "name": "a" } ],
            "nextCursor": ""
        });
        let mut responses = HashMap::new();
        responses.insert("resources/list".to_string(), vec![page1]);
        let mock = MockTransport::new(responses);
        let client = client_with_mock(mock);

        let all = client
            .list_paginated("resources/list", "resources")
            .await
            .expect("ok");
        assert_eq!(all.len(), 1, "first page returned, loop terminated");
    }

    #[tokio::test]
    async fn list_paginated_caps_runaway_page_loop() {
        // A buggy server that ALWAYS returns the same nextCursor would loop
        // forever without the MAX_PAGES safety rail. Feed 300 identical pages
        // and verify we stop gracefully instead of hanging (or exhausting the
        // mock queue with an Err).
        let page = json!({ "tools": [], "nextCursor": "stuck" });
        let responses = {
            let mut m = HashMap::new();
            m.insert("tools/list".to_string(), vec![page; 300]);
            m
        };
        let mock = Arc::new(MockTransport::new(responses));

        struct ArcAdapter(Arc<MockTransport>);
        #[async_trait]
        impl McpTransport for ArcAdapter {
            async fn call(&self, id: u64, method: &str, params: Value) -> Result<Value> {
                self.0.call(id, method, params).await
            }
        }

        let client = McpClient::new(
            "mock".into(),
            "stdio",
            Box::new(ArcAdapter(Arc::clone(&mock))),
            None,
        );

        // Should return Ok (graceful cap), not hang or Err.
        let all = client
            .list_paginated("tools/list", "tools")
            .await
            .expect("ok");
        assert!(all.is_empty());
        // Must have stopped at MAX_PAGES (256), not consumed all 300.
        assert_eq!(mock.calls().len(), 256, "must cap at MAX_PAGES");
    }

    /// `initialize` used to claim roots and sampling, which nothing here
    /// serves; servers that believed it waited on requests never answered.
    #[tokio::test]
    async fn initialize_claims_no_client_capabilities() {
        let mock = Arc::new(MockTransport::new(HashMap::from([
            (
                "initialize".to_string(),
                vec![json!({ "capabilities": {} })],
            ),
            ("tools/list".to_string(), vec![json!({ "tools": [] })]),
        ])));
        struct Shared(Arc<MockTransport>);
        #[async_trait]
        impl McpTransport for Shared {
            async fn call(&self, id: u64, method: &str, params: Value) -> Result<Value> {
                self.0.call(id, method, params).await
            }
        }
        let mut c = client_with_mock(MockTransport::new(HashMap::new()));
        c.transport = Box::new(Shared(Arc::clone(&mock)));
        c.init().await.unwrap();
        let calls = mock.calls();
        assert_eq!(calls[0].0, "initialize");
        assert_eq!(calls[0].1["capabilities"], json!({}));
    }

    /// `npx` is `npx.cmd` on Windows; spawning the bare name found nothing.
    #[test]
    fn bare_commands_resolve_through_pathext() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        std::fs::write(b.path().join("npx.CMD"), "").unwrap();
        std::fs::write(b.path().join("uvx.EXE"), "").unwrap();
        std::fs::write(a.path().join("uvx.CMD"), "").unwrap();
        let path = std::env::join_paths([a.path(), b.path()]).unwrap();
        let ext = std::ffi::OsStr::new(".COM;.EXE;.BAT;.CMD");
        let find = |c: &str| resolve_on_path(c, Some(&path), Some(ext));
        assert_eq!(find("npx"), Some(b.path().join("npx.CMD")));
        // PATH order first, then PATHEXT order, as cmd.exe does.
        assert_eq!(find("uvx"), Some(a.path().join("uvx.CMD")));
        assert_eq!(find("missing"), None);
        // Explicit paths and extensions are spawned as written.
        assert_eq!(find("npx.cmd"), None);
        assert_eq!(find("./npx"), None);
        assert_eq!(find(r"C:\tools\npx"), None);
    }
}

#[cfg(test)]
mod hardening_tests {
    use super::*;
    use crate::net_policy::test_support::scripted_server;
    use std::sync::atomic::Ordering as AtomicOrdering;

    struct Canned(Value);
    #[async_trait]
    impl McpTransport for Canned {
        async fn call(&self, _id: u64, _method: &str, _params: Value) -> Result<Value> {
            Ok(self.0.clone())
        }
    }

    fn client(resp: Value) -> McpClient {
        McpClient::new("mock".into(), "stdio", Box::new(Canned(resp)), None)
    }

    /// `tools/call` output is capped at 25K chars; `resources/read` was not,
    /// so one hostile or merely large resource flooded the context window.
    #[tokio::test]
    async fn resource_read_output_is_capped_like_tool_output() {
        let big = "x".repeat(200_000);
        let c = client(json!({"contents": [{"uri": "u", "text": big}]}));
        let out = c.read_resource("u").await.unwrap();
        assert!(out.len() < 30_000, "got {} chars", out.len());
        assert!(out.contains("[Result truncated"), "must say it was cut");
    }

    /// Streamable-HTTP servers must receive `notifications/initialized`
    /// before requests; the HTTP transport's `notify` was a no-op.
    #[tokio::test]
    async fn http_transport_sends_notifications() {
        let (base, hits) = scripted_server(vec![
            "HTTP/1.1 202 Accepted\r\ncontent-length: 0\r\nconnection: close\r\n\r\n".into(),
        ])
        .await;
        let t = HttpTransport::new(&base, &HashMap::new()).unwrap();
        t.notify("notifications/initialized").await;
        assert_eq!(hits.load(AtomicOrdering::SeqCst), 1, "no request was sent");
    }

    /// Serves `script` one connection at a time and records each raw request.
    async fn recording_server(script: Vec<String>) -> (String, Arc<std::sync::Mutex<Vec<String>>>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let log = seen.clone();
        tokio::spawn(async move {
            for reply in script {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let mut req = Vec::new();
                let mut tmp = [0u8; 4096];
                loop {
                    let n = sock.read(&mut tmp).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    req.extend_from_slice(&tmp[..n]);
                    let text = String::from_utf8_lossy(&req).to_ascii_lowercase();
                    if let Some(h) = text.find("\r\n\r\n") {
                        let len = text
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length:"))
                            .and_then(|v| v.trim().parse::<usize>().ok())
                            .unwrap_or(0);
                        if req.len() >= h + 4 + len {
                            break;
                        }
                    }
                }
                log.lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&req).to_ascii_lowercase());
                let _ = sock.write_all(reply.as_bytes()).await;
                let _ = sock.shutdown().await;
            }
        });
        (format!("http://{addr}"), seen)
    }

    /// Stateful Streamable-HTTP servers (the SDK default) need both media
    /// types accepted, the session id from `initialize` echoed on every later
    /// request, and may answer in SSE with notifications before the response.
    /// Such a server refuses the `server/discover` probe the way the
    /// TypeScript SDK does (a 400 without a modern error), and gets the
    /// legacy handshake, then the negotiated `MCP-Protocol-Version`.
    #[tokio::test]
    async fn http_transport_speaks_streamable_http() {
        let refused = r#"{"jsonrpc":"2.0","error":{"code":-32000,"message":"Bad Request: No valid session ID provided"},"id":null}"#;
        let init = r#"{"jsonrpc":"2.0","id":2,"result":{"capabilities":{"tools":{}}}}"#;
        let sse = "event: message\r\n\
                   data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/message\",\"params\":{}}\r\n\r\n\
                   data: {\"jsonrpc\":\"2.0\",\"id\":99,\"result\":{}}\n\n\
                   event: message\n\
                   data: {\"jsonrpc\":\"2.0\",\"id\":3,\n\
                   data: \"result\":{\"tools\":[{\"name\":\"echo\"}]}}\n\n";
        let (base, seen) = recording_server(vec![
            format!(
                "HTTP/1.1 400 Bad Request\r\ncontent-type: application/json\r\n\
                 content-length: {}\r\nconnection: close\r\n\r\n{refused}",
                refused.len()
            ),
            format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nmcp-session-id: sess-42\r\n\
                 content-length: {}\r\nconnection: close\r\n\r\n{init}",
                init.len()
            ),
            "HTTP/1.1 202 Accepted\r\ncontent-length: 0\r\nconnection: close\r\n\r\n".into(),
            format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                 connection: close\r\n\r\n{sse}"
            ),
            "HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n".into(),
        ])
        .await;

        let client = McpClient::connect_http("remote".into(), &base, &HashMap::new())
            .await
            .unwrap();
        assert_eq!(client.tools.len(), 1, "SSE tools/list response was lost");
        assert_eq!(client.tools[0].name, "echo");

        let err = client.call_tool("echo", json!({})).await.unwrap_err();
        assert!(err.to_string().contains("session expired"), "{err}");

        assert_eq!(client.protocol, Protocol::Legacy("2025-06-18".into()));
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 5);
        for req in seen.iter() {
            assert!(
                req.contains("accept: application/json, text/event-stream"),
                "{req}"
            );
        }
        assert!(
            seen[0].contains("mcp-method: server/discover"),
            "{}",
            seen[0]
        );
        assert!(
            seen[0].contains("mcp-protocol-version: 2026-07-28"),
            "{}",
            seen[0]
        );
        assert!(
            seen[1].contains(r#""protocolversion":"2025-06-18""#),
            "{}",
            seen[1]
        );
        for req in &seen[..2] {
            assert!(!req.contains("mcp-session-id"), "{req}");
            assert!(!req.contains("mcp-protocol-version: 2025"), "{req}");
        }
        for req in &seen[2..] {
            assert!(req.contains("mcp-session-id: sess-42"), "{req}");
            assert!(req.contains("mcp-protocol-version: 2025-06-18"), "{req}");
        }
    }

    /// `send()` resolves at the headers; a body that then never finishes
    /// used to hang the tool call forever.
    #[tokio::test]
    async fn http_transport_times_out_a_stalled_body() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut tmp = [0u8; 4096];
            let _ = sock.read(&mut tmp).await;
            let _ = sock
                .write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
                      content-length: 100\r\n\r\n{\"jsonrpc\"",
                )
                .await;
            // Hold the connection open without finishing the body.
            tokio::time::sleep(Duration::from_secs(30)).await;
            drop(sock);
        });
        let mut t = HttpTransport::new(&base, &HashMap::new()).unwrap();
        t.timeout = Duration::from_millis(300);
        t.tool_timeout = Duration::from_millis(300);
        let err = tokio::time::timeout(Duration::from_secs(10), t.call(1, "tools/call", json!({})))
            .await
            .expect("the transport's own deadline must fire")
            .unwrap_err();
        assert!(err.to_string().contains("timed out"), "{err}");
    }

    /// A redirect off the server's origin resent the JSON-RPC body and any
    /// custom auth header (`X-API-Key`) to the new host, even over plain
    /// http. One within the origin (`/mcp` → `/mcp/`) is still followed.
    #[tokio::test]
    async fn http_redirects_are_followed_only_within_the_origin() {
        let ok = "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
                  content-length: 36\r\nconnection: close\r\n\r\n\
                  {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}";
        let (elsewhere, elsewhere_seen) = recording_server(vec![ok.to_string()]).await;
        let to_elsewhere = format!(
            "HTTP/1.1 308 Permanent Redirect\r\nlocation: {elsewhere}/mcp\r\n\
             content-length: 0\r\nconnection: close\r\n\r\n"
        );
        let (base, _) = recording_server(vec![to_elsewhere]).await;
        let headers = HashMap::from([("X-API-Key".to_string(), "k-secret".to_string())]);
        let t = HttpTransport::new(&format!("{base}/mcp"), &headers).unwrap();
        let err = t.call(1, "tools/list", json!({})).await.unwrap_err();
        let err = err.to_string();
        assert!(
            err.contains("308") && err.contains(&format!("{elsewhere}/mcp")),
            "{err}"
        );
        assert!(err.contains("not followed"), "{err}");
        assert!(elsewhere_seen.lock().unwrap().is_empty());

        let slash = "HTTP/1.1 307 Temporary Redirect\r\nlocation: /mcp/\r\n\
                     content-length: 0\r\nconnection: close\r\n\r\n";
        let (base, seen) = recording_server(vec![slash.to_string(), ok.to_string()]).await;
        let t = HttpTransport::new(&format!("{base}/mcp"), &headers).unwrap();
        assert_eq!(t.call(1, "tools/list", json!({})).await.unwrap(), json!({}));
        let seen = seen.lock().unwrap();
        assert!(seen[1].starts_with("post /mcp/ "), "{seen:?}");
        assert!(seen[1].contains("x-api-key: k-secret"), "{seen:?}");
    }

    /// A server answering with a multi-gigabyte body must be refused, not
    /// buffered.
    #[tokio::test]
    async fn http_transport_refuses_oversized_responses() {
        let resp = "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
                    content-length: 999999999\r\nconnection: close\r\n\r\n{";
        let (base, _) = scripted_server(vec![resp.to_string()]).await;
        let t = HttpTransport::new(&base, &HashMap::new()).unwrap();
        let err = t.call(1, "tools/list", json!({})).await.unwrap_err();
        assert!(err.to_string().contains("too large"), "{err}");
    }

    /// github-mcp-server's get_file_contents returns [status text, embedded
    /// resource]; only the status line used to reach the model.
    #[tokio::test]
    async fn embedded_resource_text_reaches_the_model() {
        let github = client(json!({ "content": [
                { "type": "text", "text": "successfully downloaded text file" },
                { "type": "resource", "resource": {
                    "uri": "repo://o/r/contents/src/lib.rs",
                    "mimeType": "text/x-rust",
                    "text": "pub fn answer() -> u32 { 42 }"
                } }
            ] }));
        let out = github
            .call_tool("get_file_contents", json!({}))
            .await
            .unwrap();
        assert!(out.contains("successfully downloaded"), "{out}");
        assert!(out.contains("pub fn answer() -> u32 { 42 }"), "{out}");
    }

    #[tokio::test]
    async fn binary_content_becomes_a_placeholder_not_base64() {
        let b64 = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk";
        let tool = client(json!({ "content": [
                { "type": "image", "mimeType": "image/png", "data": b64 },
                { "type": "resource", "resource": {
                    "uri": "file:///shot.png", "mimeType": "image/png", "blob": b64
                } },
                { "type": "resource_link", "uri": "file:///big.log", "name": "big.log" }
            ] }));
        let out = tool.call_tool("screenshot", json!({})).await.unwrap();
        assert!(!out.contains(b64), "{out}");
        assert!(out.contains("[image image/png"), "{out}");
        assert!(out.contains("[binary resource file:///shot.png"), "{out}");
        assert!(out.contains("[resource link: file:///big.log]"), "{out}");

        let resource = client(json!({ "contents": [
                { "uri": "file:///shot.png", "mimeType": "image/png", "blob": b64 }
            ] }));
        let out = resource.read_resource("file:///shot.png").await.unwrap();
        assert!(
            out.starts_with("[binary resource file:///shot.png (image/png"),
            "{out}"
        );
    }

    #[cfg(unix)]
    fn sh_server(script: &str) -> (String, Vec<String>) {
        ("sh".to_string(), vec!["-c".to_string(), script.to_string()])
    }

    /// A server's own request reuses ids from its counter: it was taken for
    /// the reply to our call with the same id (resolving it with null), and
    /// nothing was ever written back, so the server waited forever.
    #[cfg(unix)]
    #[tokio::test]
    async fn stdio_answers_server_requests_and_keeps_them_off_our_calls() {
        let dir = tempfile::tempdir().unwrap();
        let (cmd, args) = sh_server(
            r#"read l
printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"ping"}'
printf '%s\n' '{"jsonrpc":"2.0","method":"notifications/message","params":{}}'
printf '%s\n' '{"jsonrpc":"2.0","id":"s2","method":"sampling/createMessage","params":{}}'
read r1; read r2
printf '{"jsonrpc":"2.0","id":1,"result":{"r1":%s,"r2":%s}}\n' "$r1" "$r2"
cat >/dev/null"#,
        );
        let t = StdioTransport::connect(&cmd, &args, &HashMap::new(), dir.path())
            .await
            .unwrap();
        let out = tokio::time::timeout(Duration::from_secs(10), t.call(1, "initialize", json!({})))
            .await
            .expect("server never got its answers")
            .unwrap();
        assert_eq!(out["r1"]["id"], json!(1));
        assert_eq!(out["r1"]["result"], json!({}));
        assert_eq!(out["r2"]["id"], json!("s2"));
        assert_eq!(out["r2"]["error"]["code"], json!(-32601));
    }

    /// A call abandoned by its caller (Esc, a cancelled turn) or by the
    /// timeout was never cancelled on the server, which kept working on it,
    /// and its pending entry stayed until a reply that never came.
    #[cfg(unix)]
    #[tokio::test]
    async fn stdio_abandoned_call_is_cancelled_on_the_server() {
        let dir = tempfile::tempdir().unwrap();
        let (cmd, args) = sh_server(
            r#"read call; read cancel; read next
printf '{"jsonrpc":"2.0","id":2,"result":{"cancel":%s}}\n' "$cancel"
cat >/dev/null"#,
        );
        let t = StdioTransport::connect(&cmd, &args, &HashMap::new(), dir.path())
            .await
            .unwrap();
        let abandoned = tokio::time::timeout(
            Duration::from_millis(300),
            t.call(1, "tools/call", json!({ "name": "slow" })),
        )
        .await;
        assert!(abandoned.is_err(), "the server never answers the call");
        assert!(t.pending.lock().await.is_empty());

        let out = tokio::time::timeout(Duration::from_secs(10), t.call(2, "tools/list", json!({})))
            .await
            .expect("no cancel reached the server")
            .unwrap();
        assert_eq!(out["cancel"]["method"], "notifications/cancelled");
        assert_eq!(out["cancel"]["params"]["requestId"], json!(1));
        assert!(out["cancel"].get("id").is_none(), "{out}");
    }

    /// Every request, `tools/call` included, was cut off at the 60 s that
    /// bounds the handshake and list calls, so a tool that runs a test suite
    /// or a browser flow always failed.
    #[cfg(unix)]
    #[tokio::test]
    async fn stdio_tool_calls_outlast_the_request_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let (cmd, args) = sh_server(
            r#"while IFS= read -r l; do
id=$(printf '%s\n' "$l" | sed -n 's/^{"jsonrpc":"2.0","id":\([0-9]*\),.*/\1/p')
sleep 1
printf '{"jsonrpc":"2.0","id":%s,"result":{"done":true}}\n' "$id"
done"#,
        );
        let mut t = StdioTransport::connect(&cmd, &args, &HashMap::new(), dir.path())
            .await
            .unwrap();
        assert_eq!(t.tool_timeout, tool_timeout());
        // Explicit, so a MCP_TOOL_TIMEOUT in the environment cannot matter.
        t.timeout = Duration::from_millis(300);
        t.tool_timeout = Duration::from_secs(20);

        let out = t.call(1, "tools/call", json!({ "name": "slow" })).await;
        assert_eq!(out.unwrap()["done"], json!(true));
        let out = t.call(2, "resources/read", json!({ "uri": "x" })).await;
        assert_eq!(out.unwrap()["done"], json!(true));
        let err = t.call(3, "tools/list", json!({})).await.unwrap_err();
        assert!(err.to_string().contains("timed out (tools/list)"), "{err}");
    }

    #[test]
    fn tool_timeout_comes_from_mcp_tool_timeout_in_milliseconds() {
        assert_eq!(parse_tool_timeout(None), DEFAULT_TOOL_TIMEOUT);
        assert_eq!(parse_tool_timeout(Some("90000")), Duration::from_secs(90));
        for bad in ["", "0", "-5", "1h", "1.5"] {
            assert_eq!(parse_tool_timeout(Some(bad)), DEFAULT_TOOL_TIMEOUT, "{bad}");
        }
        assert_eq!(
            deadline("tools/call", REQUEST_TIMEOUT, Duration::from_secs(7)),
            Duration::from_secs(7)
        );
        assert_eq!(
            deadline("initialize", REQUEST_TIMEOUT, Duration::from_secs(7)),
            REQUEST_TIMEOUT
        );
    }

    /// A server writing a huge or newline-free blob to stdout had it held
    /// in memory whole. Past the cap the line is dropped, read to its end,
    /// and the next line is read normally.
    #[tokio::test]
    async fn stdio_lines_over_the_cap_are_dropped() {
        let input: &[u8] =
            b"{\"a\":1}\nxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\n{\"b\":2}\ntail";
        // A small buffer, so lines span several fills.
        let mut reader = tokio::io::BufReader::with_capacity(4, input);
        let mut buf = Vec::new();
        let mut lines = Vec::new();
        loop {
            match read_line_capped(&mut reader, &mut buf, 16).await.unwrap() {
                StdioLine::Eof => break,
                StdioLine::Line => lines.push(String::from_utf8(buf.clone()).unwrap()),
                StdioLine::TooLong => {
                    assert!(buf.capacity() <= 16, "{}", buf.capacity());
                    lines.push("<dropped>".into());
                }
            }
        }
        assert_eq!(lines, ["{\"a\":1}\n", "<dropped>", "{\"b\":2}\n", "tail"]);
    }

    /// A stdio server that ignores stdin EOF (stuck at startup, waiting on
    /// a login) kept running after its transport was dropped: the reap task
    /// owned the child, so `kill_on_drop` never fired.
    #[cfg(unix)]
    #[tokio::test]
    async fn stdio_server_ignoring_eof_is_killed_when_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("pid");
        let (cmd, args) = sh_server(&format!(
            "echo $$ > '{}'; while :; do sleep 0.1; done",
            pid_file.display()
        ));
        let t = StdioTransport::connect(&cmd, &args, &HashMap::new(), dir.path())
            .await
            .unwrap();
        let pid = loop {
            if let Ok(pid) = std::fs::read_to_string(&pid_file)
                && pid.ends_with('\n')
            {
                break pid.trim().to_string();
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        let alive = || {
            std::process::Command::new("kill")
                .args(["-0", &pid])
                .stderr(std::process::Stdio::null())
                .status()
                .unwrap()
                .success()
        };
        assert!(alive());
        drop(t);
        let deadline = tokio::time::Instant::now() + STDIO_EXIT_GRACE + Duration::from_secs(10);
        while alive() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "server {pid} still running"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// A stray non-UTF-8 line (a print() under a cp1252 locale) ended the
    /// reader as if the server had exited, failing every later call.
    #[cfg(unix)]
    #[tokio::test]
    async fn stdio_skips_a_non_utf8_line() {
        let dir = tempfile::tempdir().unwrap();
        let (cmd, args) = sh_server(
            r#"read l
printf 'caf\351 \377\n'
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{}}'
read l
printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"ok":true}}'
cat >/dev/null"#,
        );
        let t = StdioTransport::connect(&cmd, &args, &HashMap::new(), dir.path())
            .await
            .unwrap();
        let first =
            tokio::time::timeout(Duration::from_secs(10), t.call(1, "initialize", json!({})))
                .await
                .expect("first call hung")
                .unwrap();
        assert_eq!(first, json!({}));
        let second =
            tokio::time::timeout(Duration::from_secs(10), t.call(2, "tools/list", json!({})))
                .await
                .expect("second call hung")
                .unwrap();
        assert_eq!(second, json!({"ok": true}));
    }

    /// A call made after the server died sat in the pending map the reader
    /// had already drained, and hung for the full 60 s request timeout.
    #[cfg(unix)]
    #[tokio::test]
    async fn stdio_call_after_server_exit_fails_fast() {
        let dir = tempfile::tempdir().unwrap();
        let (cmd, args) = sh_server(
            r#"read l
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{}}'"#,
        );
        let t = StdioTransport::connect(&cmd, &args, &HashMap::new(), dir.path())
            .await
            .unwrap();
        t.call(1, "initialize", json!({})).await.unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            while !t.closed.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("reader never saw EOF");
        let err = tokio::time::timeout(Duration::from_secs(5), t.call(2, "tools/call", json!({})))
            .await
            .expect("call after exit hung")
            .unwrap_err();
        assert!(err.to_string().contains("exited"), "{err}");
        assert!(t.pending.lock().await.is_empty());
    }

    /// A legacy HTTP+SSE MCP server: GET opens the stream and announces
    /// `endpoint` (relative, as the reference SDK sends it); each POST there
    /// is answered 202 and its reply goes out on the stream. `before`
    /// events are sent ahead of every reply. Records each request head and
    /// body, lowercased.
    async fn legacy_sse_server(
        endpoint: &'static str,
        before: Vec<String>,
    ) -> (String, Arc<std::sync::Mutex<Vec<String>>>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let log = seen.clone();
        let (ev_tx, ev_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let ev_rx = Arc::new(Mutex::new(Some(ev_rx)));
        let before = Arc::new(before);
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let (log, ev_tx, ev_rx, before) =
                    (log.clone(), ev_tx.clone(), ev_rx.clone(), before.clone());
                tokio::spawn(async move {
                    let mut req = Vec::new();
                    let mut tmp = [0u8; 4096];
                    loop {
                        let n = sock.read(&mut tmp).await.unwrap_or(0);
                        if n == 0 {
                            break;
                        }
                        req.extend_from_slice(&tmp[..n]);
                        let text = String::from_utf8_lossy(&req).to_ascii_lowercase();
                        if let Some(h) = text.find("\r\n\r\n") {
                            let len = text
                                .lines()
                                .find_map(|l| l.strip_prefix("content-length:"))
                                .and_then(|v| v.trim().parse::<usize>().ok())
                                .unwrap_or(0);
                            if req.len() >= h + 4 + len {
                                break;
                            }
                        }
                    }
                    let raw = String::from_utf8_lossy(&req).into_owned();
                    log.lock().unwrap().push(raw.to_ascii_lowercase());
                    if raw.starts_with("GET ") {
                        let Some(mut rx) = ev_rx.lock().await.take() else {
                            return;
                        };
                        let head = format!(
                            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                             cache-control: no-cache\r\n\r\n\
                             : a comment\n\nevent: endpoint\ndata: {endpoint}\n\n"
                        );
                        if sock.write_all(head.as_bytes()).await.is_err() {
                            return;
                        }
                        while let Some(ev) = rx.recv().await {
                            if sock.write_all(ev.as_bytes()).await.is_err() {
                                return;
                            }
                        }
                        return;
                    }
                    let _ = sock
                        .write_all(b"HTTP/1.1 202 Accepted\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
                        .await;
                    let _ = sock.shutdown().await;
                    let body = raw.split_once("\r\n\r\n").map_or("", |(_, b)| b);
                    let Ok(msg) = serde_json::from_str::<Value>(body) else {
                        return;
                    };
                    let (Some(id), Some(method)) = (msg.get("id"), msg["method"].as_str()) else {
                        return; // a notification, or our reply to a server request
                    };
                    let result = match method {
                        "initialize" => {
                            json!({"protocolVersion": "2024-11-05", "capabilities": {"tools": {}}})
                        }
                        "tools/list" => {
                            json!({"tools": [{"name": "echo", "inputSchema": {"type": "object"}}]})
                        }
                        "tools/call" => json!({"content": [{"type": "text",
                            "text": format!("echoed {}", msg["params"]["arguments"]["text"].as_str().unwrap_or(""))}]}),
                        _ => json!({}),
                    };
                    for ev in before.iter() {
                        let _ = ev_tx.send(ev.clone());
                    }
                    let reply = json!({"jsonrpc": "2.0", "id": id, "result": result});
                    let _ = ev_tx.send(format!("event: message\r\ndata: {reply}\r\n\r\n"));
                });
            }
        });
        (format!("http://{addr}/sse"), seen)
    }

    /// The SSE reader resumes its search for an event's end where the last
    /// chunk left off; events, and their `\r\n\r\n` terminators, split
    /// across chunks must still be found.
    #[tokio::test]
    async fn sse_transport_reads_events_split_across_chunks() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        async fn read_request(sock: &mut tokio::net::TcpStream) -> String {
            let mut req = Vec::new();
            let mut tmp = [0u8; 4096];
            loop {
                let n = sock.read(&mut tmp).await.unwrap_or(0);
                if n == 0 {
                    break;
                }
                req.extend_from_slice(&tmp[..n]);
                let text = String::from_utf8_lossy(&req).to_ascii_lowercase();
                if let Some(h) = text.find("\r\n\r\n") {
                    let len = text
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length:"))
                        .and_then(|v| v.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    if req.len() >= h + 4 + len {
                        break;
                    }
                }
            }
            String::from_utf8_lossy(&req).into_owned()
        }
        async fn trickle(sock: &mut tokio::net::TcpStream, parts: &[&str]) {
            for part in parts {
                sock.write_all(part.as_bytes()).await.unwrap();
                sock.flush().await.unwrap();
                tokio::time::sleep(Duration::from_millis(30)).await;
            }
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/sse", listener.local_addr().unwrap());
        let (id_tx, id_rx) = tokio::sync::mpsc::unbounded_channel::<Value>();
        let id_rx = Arc::new(Mutex::new(Some(id_rx)));
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let (id_tx, id_rx) = (id_tx.clone(), id_rx.clone());
                tokio::spawn(async move {
                    let raw = read_request(&mut sock).await;
                    if raw.starts_with("GET ") {
                        let mut rx = id_rx.lock().await.take().unwrap();
                        trickle(
                            &mut sock,
                            &[
                                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\r\n",
                                "event: endpoint\r\ndata: /m\r\n\r",
                                "\n",
                            ],
                        )
                        .await;
                        while let Some(id) = rx.recv().await {
                            let reply = json!({"jsonrpc": "2.0", "id": id, "result": {"ok": true}})
                                .to_string();
                            let (a, b) = reply.split_at(reply.len() / 2);
                            let parts = ["event: message\r\nda", "ta: ", a, b, "\r\n", "\r\n"];
                            trickle(&mut sock, &parts).await;
                        }
                        return;
                    }
                    let _ = sock
                        .write_all(b"HTTP/1.1 202 Accepted\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
                        .await;
                    let body = raw.split_once("\r\n\r\n").map_or("", |(_, b)| b);
                    if let Ok(msg) = serde_json::from_str::<Value>(body) {
                        let _ = id_tx.send(msg["id"].clone());
                    }
                });
            }
        });
        let t = tokio::time::timeout(
            Duration::from_secs(10),
            SseTransport::connect(&url, &HashMap::new()),
        )
        .await
        .expect("endpoint event never found")
        .unwrap();
        assert_eq!(t.endpoint.path(), "/m");
        for id in 1..=2 {
            let out =
                tokio::time::timeout(Duration::from_secs(10), t.call(id, "tools/list", json!({})))
                    .await
                    .expect("response event never found")
                    .unwrap();
            assert_eq!(out, json!({"ok": true}));
        }
    }

    /// ACP hosts may pass `type: "sse"` servers: the client opens the
    /// stream, posts to the announced endpoint with the host's headers, and
    /// reads the answers off the stream past notifications and the
    /// server's own requests (a ping, which gets its reply).
    #[tokio::test]
    async fn sse_transport_speaks_legacy_http_sse() {
        let before = vec![
            "data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/message\",\"params\":{}}\n\n"
                .to_string(),
            "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":\"srv-1\",\"method\":\"ping\"}\n\n"
                .to_string(),
        ];
        let (url, seen) = legacy_sse_server("/messages?sessionId=abc", before).await;
        let headers: HashMap<String, String> =
            [("Authorization".to_string(), "Bearer s3cret".to_string())].into();
        let client = tokio::time::timeout(
            Duration::from_secs(20),
            McpClient::connect_sse("old".into(), &url, &headers),
        )
        .await
        .expect("connect hung")
        .unwrap();
        assert_eq!(client.transport_kind, "sse");
        assert_eq!(client.tools.len(), 1, "{:?}", client.tools);
        assert_eq!(client.tools[0].name, "echo");
        let out = tokio::time::timeout(
            Duration::from_secs(20),
            client.call_tool("echo", json!({"text": "hi"})),
        )
        .await
        .expect("tool call hung")
        .unwrap();
        assert_eq!(out, "echoed hi");

        // The ping reply is posted from a spawned task; give it a moment.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while !seen
            .lock()
            .unwrap()
            .iter()
            .any(|r| r.contains(r#""id":"srv-1""#))
        {
            assert!(
                tokio::time::Instant::now() < deadline,
                "ping went unanswered"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let seen = seen.lock().unwrap().clone();
        assert!(seen[0].starts_with("get /sse "), "{}", seen[0]);
        assert!(seen[0].contains("accept: text/event-stream"), "{}", seen[0]);
        let posts: Vec<&String> = seen.iter().filter(|r| r.starts_with("post ")).collect();
        assert!(posts.len() >= 4, "{seen:?}"); // initialize, initialized, tools/list, tools/call
        for r in &posts {
            assert!(r.starts_with("post /messages?sessionid=abc "), "{r}");
        }
        assert!(
            seen.iter()
                .all(|r| r.contains("authorization: bearer s3cret")),
            "{seen:?}"
        );
        assert!(seen.iter().any(|r| r.contains("notifications/initialized")));
        assert!(
            seen.iter()
                .any(|r| r.contains(r#""id":"srv-1""#) && r.contains(r#""result":{}"#)),
            "{seen:?}"
        );
    }

    /// The host's headers ride on every POST, so an endpoint event naming
    /// another host must not be followed.
    #[tokio::test]
    async fn sse_transport_refuses_an_endpoint_on_another_origin() {
        let (url, seen) = legacy_sse_server("http://127.0.0.2:1/steal", vec![]).await;
        let err = match tokio::time::timeout(
            Duration::from_secs(20),
            SseTransport::connect(&url, &HashMap::new()),
        )
        .await
        .expect("connect hung")
        {
            Ok(_) => panic!("followed a cross-origin endpoint"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("origin"), "{err}");
        assert_eq!(seen.lock().unwrap().len(), 1, "only the GET");
    }

    /// A stream that dies fails the waiting call at once, and later calls
    /// fail fast instead of waiting out the request timeout.
    #[tokio::test]
    async fn sse_transport_fails_calls_when_the_stream_ends() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/sse", listener.local_addr().unwrap());
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut tmp = [0u8; 4096];
                    let n = sock.read(&mut tmp).await.unwrap_or(0);
                    if tmp[..n].starts_with(b"GET ") {
                        let _ = sock
                            .write_all(
                                b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\r\n\
                                  event: endpoint\ndata: /m\n\n",
                            )
                            .await;
                        // Close the stream once the first message is posted.
                        tokio::time::sleep(Duration::from_millis(300)).await;
                    } else {
                        let _ = sock
                            .write_all(b"HTTP/1.1 202 Accepted\r\ncontent-length: 0\r\n\r\n")
                            .await;
                    }
                });
            }
        });
        let t = SseTransport::connect(&url, &HashMap::new()).await.unwrap();
        let err = tokio::time::timeout(Duration::from_secs(10), t.call(1, "initialize", json!({})))
            .await
            .expect("call hung after the stream ended")
            .unwrap_err();
        assert!(err.to_string().contains("closed"), "{err}");
        let err = tokio::time::timeout(Duration::from_secs(5), t.call(2, "tools/list", json!({})))
            .await
            .expect("later call hung")
            .unwrap_err();
        assert!(err.to_string().contains("closed"), "{err}");
        assert!(t.pending.lock().await.is_empty());
    }

    /// `HttpServerConfig::sse` (set for an ACP host's `type: "sse"`) picks
    /// this transport when the session's MCP servers start.
    #[tokio::test]
    async fn sse_servers_start_through_the_manager() {
        let (url, _) = legacy_sse_server("/messages", vec![]).await;
        let dir = tempfile::tempdir().unwrap();
        let server =
            crate::mcp::types::McpServerConfig::Http(crate::mcp::types::HttpServerConfig {
                url,
                headers: HashMap::new(),
                disabled: false,
                literal: true,
                sse: true,
            });
        let cfg = crate::config::Config {
            cwd: dir.path().to_path_buf(),
            strict_mcp_config: true,
            extra_mcp_servers: [("old".to_string(), server)].into_iter().collect(),
            ..Default::default()
        };
        let tools = crate::mcp::tools_for_config(&cfg).await;
        let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        assert!(names.contains(&"mcp__old__echo"), "{names:?}");
    }
}

/// The 2026-07-28 revision and the fallback to the `initialize` handshake,
/// against fake servers of each era over stdio and Streamable HTTP.
#[cfg(test)]
mod protocol_tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    const PROBE: Duration = Duration::from_millis(400);

    /// Reads one JSON-RPC message per line; `$id` is the request id (empty
    /// for a notification) and every line is appended to `seen.log`.
    #[cfg(unix)]
    const READ_LOOP: &str = r#"while IFS= read -r l; do
printf '%s\n' "$l" >> seen.log
id=$(printf '%s\n' "$l" | sed -n 's/^{"jsonrpc":"2.0","id":\([0-9]*\),.*/\1/p')
[ -z "$id" ] && continue
reply() { printf '{"jsonrpc":"2.0","id":%s,%s}\n' "$id" "$1"; }
"#;

    /// A server that speaks only 2026-07-28: every request must carry the
    /// per-request `_meta`, so `initialize` is refused. `echo` echoes its
    /// `text`; `ask` needs an elicitation answer first and `sample` a
    /// sampling one.
    #[cfg(unix)]
    fn modern_stdio_script() -> String {
        format!(
            r#"{READ_LOOP}case "$l" in
*'"io.modelcontextprotocol/protocolVersion":"2026-07-28"'*) ;;
*) reply '"error":{{"code":-32602,"message":"missing _meta"}}'; continue;;
esac
case "$l" in
*'"method":"server/discover"'*) reply '"result":{{"resultType":"complete","supportedVersions":["2026-07-28"],"capabilities":{{"tools":{{}}}},"ttlMs":0,"cacheScope":"private"}}';;
*'"method":"tools/list"'*) reply '"result":{{"resultType":"complete","tools":[{{"name":"echo","inputSchema":{{"type":"object"}}}},{{"name":"ask","inputSchema":{{"type":"object"}}}},{{"name":"sample","inputSchema":{{"type":"object"}}}}],"ttlMs":0,"cacheScope":"private"}}';;
*'"name":"echo"'*) t=$(printf '%s' "$l" | sed -n 's/.*"text":"\([^"]*\)".*/\1/p'); reply "\"result\":{{\"resultType\":\"complete\",\"content\":[{{\"type\":\"text\",\"text\":\"modern echoed $t\"}}]}}";;
*'"inputResponses":{{"login":{{"action":"decline"}}}}'*'"name":"ask"'*'"requestState":"st-1"'*) reply '"result":{{"resultType":"complete","content":[{{"type":"text","text":"login declined"}}]}}';;
*'"name":"ask"'*) reply '"result":{{"resultType":"input_required","inputRequests":{{"login":{{"method":"elicitation/create","params":{{"mode":"form","message":"GitHub user?","requestedSchema":{{"type":"object","properties":{{"name":{{"type":"string"}}}}}}}}}}}},"requestState":"st-1"}}';;
*'"name":"sample"'*) reply '"result":{{"resultType":"input_required","inputRequests":{{"q":{{"method":"sampling/createMessage","params":{{"messages":[],"maxTokens":5}}}}}}}}';;
*) reply '"error":{{"code":-32601,"message":"Method not found"}}';;
esac
done"#
        )
    }

    /// A handshake-era server. `unknown` is what it does with a method it
    /// does not know: answer -32601, or stay silent.
    #[cfg(unix)]
    fn legacy_stdio_script(unknown: &str) -> String {
        format!(
            r#"{READ_LOOP}case "$l" in
*'"method":"initialize"'*) reply '"result":{{"protocolVersion":"2024-11-05","capabilities":{{"tools":{{}}}},"serverInfo":{{"name":"old","version":"1"}}}}';;
*io.modelcontextprotocol*) reply '"error":{{"code":-32602,"message":"unexpected _meta"}}';;
*'"method":"tools/list"'*) reply '"result":{{"tools":[{{"name":"echo","inputSchema":{{"type":"object"}}}}]}}';;
*'"method":"tools/call"'*) t=$(printf '%s' "$l" | sed -n 's/.*"text":"\([^"]*\)".*/\1/p'); reply "\"result\":{{\"content\":[{{\"type\":\"text\",\"text\":\"legacy echoed $t\"}}]}}";;
*) {unknown};;
esac
done"#
        )
    }

    /// `script`, but the process exits when it reads `server/discover`.
    /// (`legacy_stdio_script` alone answers the probe: its `_meta` hits the
    /// "unexpected _meta" arm before the unknown-method one.)
    #[cfg(unix)]
    fn exits_on_discover(script: String) -> String {
        let quit = r#"case "$l" in *'"method":"server/discover"'*) echo exited >> seen.log; exit 1;; esac
"#;
        script.replacen(READ_LOOP, &format!("{READ_LOOP}{quit}"), 1)
    }

    #[cfg(unix)]
    async fn stdio_client(dir: &std::path::Path, script: String) -> Result<McpClient> {
        let args = vec!["-c".to_string(), script];
        tokio::time::timeout(
            Duration::from_secs(20),
            McpClient::connect_stdio_probing(
                "fake".into(),
                "sh",
                &args,
                &HashMap::new(),
                dir,
                PROBE,
            ),
        )
        .await
        .expect("connect hung")
    }

    #[cfg(unix)]
    fn seen(dir: &std::path::Path) -> String {
        std::fs::read_to_string(dir.join("seen.log")).unwrap_or_default()
    }

    /// (a) + (d): a modern-only stdio server is used statelessly: one
    /// `server/discover`, no `initialize`, `_meta` on every request.
    #[cfg(unix)]
    #[tokio::test]
    async fn stdio_modern_server_is_used_without_a_handshake() {
        let dir = tempfile::tempdir().unwrap();
        let client = stdio_client(dir.path(), modern_stdio_script())
            .await
            .unwrap();
        assert_eq!(client.protocol, Protocol::Modern);
        assert_eq!(client.protocol_revision(), "2026-07-28");
        assert_eq!(client.tools.len(), 3, "{:?}", client.tools);
        let out = client
            .call_tool("echo", json!({"text": "hi"}))
            .await
            .unwrap();
        assert_eq!(out, "modern echoed hi");

        let log = seen(dir.path());
        assert!(
            log.lines().next().unwrap().contains("server/discover"),
            "{log}"
        );
        assert!(!log.contains("initialize"), "{log}");
        for line in log.lines() {
            let msg: Value = serde_json::from_str(line).unwrap();
            let meta = &msg["params"]["_meta"];
            assert_eq!(
                meta["io.modelcontextprotocol/protocolVersion"],
                "2026-07-28"
            );
            assert_eq!(
                meta["io.modelcontextprotocol/clientInfo"]["name"],
                "oxideclaw"
            );
            assert_eq!(
                meta["io.modelcontextprotocol/clientCapabilities"],
                json!({})
            );
        }
    }

    /// (b) + (d): a legacy-only server refuses the probe and gets today's
    /// handshake, offered the newest legacy revision; its answer is kept.
    #[cfg(unix)]
    #[tokio::test]
    async fn stdio_legacy_server_gets_the_initialize_handshake() {
        let dir = tempfile::tempdir().unwrap();
        let script =
            legacy_stdio_script(r#"reply '"error":{"code":-32601,"message":"Method not found"}'"#);
        let client = stdio_client(dir.path(), script).await.unwrap();
        assert_eq!(client.protocol, Protocol::Legacy("2024-11-05".into()));
        let out = client
            .call_tool("echo", json!({"text": "yo"}))
            .await
            .unwrap();
        assert_eq!(out, "legacy echoed yo");

        let log = seen(dir.path());
        let methods: Vec<String> = log
            .lines()
            .map(|l| {
                serde_json::from_str::<Value>(l).unwrap()["method"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect();
        assert_eq!(
            methods,
            [
                "server/discover",
                "initialize",
                "notifications/initialized",
                "tools/list",
                "tools/call"
            ]
        );
        let init: Value = serde_json::from_str(log.lines().nth(1).unwrap()).unwrap();
        assert_eq!(init["params"]["protocolVersion"], "2025-06-18");
        assert_eq!(init["params"]["capabilities"], json!({}));
    }

    /// (c): a legacy server that never answers an unknown method costs the
    /// probe timeout, not the connection.
    #[cfg(unix)]
    #[tokio::test]
    async fn stdio_server_silent_on_unknown_methods_falls_back_within_the_probe_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let started = std::time::Instant::now();
        let client = stdio_client(dir.path(), legacy_stdio_script(":"))
            .await
            .unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(8),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(client.protocol, Protocol::Legacy("2024-11-05".into()));
        let out = client
            .call_tool("echo", json!({"text": "x"}))
            .await
            .unwrap();
        assert_eq!(out, "legacy echoed x");
    }

    /// A legacy server that quits on a request before `initialize` is
    /// started again and spoken to without the probe.
    #[cfg(unix)]
    #[tokio::test]
    async fn stdio_server_that_exits_on_the_probe_is_restarted_for_the_handshake() {
        let dir = tempfile::tempdir().unwrap();
        let script = exits_on_discover(legacy_stdio_script(":"));
        let client = stdio_client(dir.path(), script).await.unwrap();
        assert_eq!(client.protocol, Protocol::Legacy("2024-11-05".into()));
        assert_eq!(
            client
                .call_tool("echo", json!({"text": "again"}))
                .await
                .unwrap(),
            "legacy echoed again"
        );
        let log = seen(dir.path());
        assert!(log.contains("exited"), "{log}");
        assert_eq!(log.matches("server/discover").count(), 1, "{log}");
        assert_eq!(log.matches(r#""method":"initialize""#).count(), 1, "{log}");
    }

    /// A modern server slow to start answers the probe only after the
    /// timeout, then refuses `initialize`: it is still recognized.
    #[cfg(unix)]
    #[tokio::test]
    async fn stdio_modern_server_slow_to_start_is_still_recognized() {
        let dir = tempfile::tempdir().unwrap();
        let script = format!("sleep 1\n{}", modern_stdio_script());
        let client = stdio_client(dir.path(), script).await.unwrap();
        assert_eq!(client.protocol, Protocol::Modern);
        assert_eq!(
            client
                .call_tool("echo", json!({"text": "late"}))
                .await
                .unwrap(),
            "modern echoed late"
        );
    }

    /// A legacy server slow to start (an `npx -y` cold start) that quits
    /// on the probe it reads only after the probe timeout is still started
    /// again without the probe.
    #[cfg(unix)]
    #[tokio::test]
    async fn stdio_slow_server_that_exits_on_the_late_probe_is_restarted() {
        let dir = tempfile::tempdir().unwrap();
        let script = format!("sleep 1\n{}", exits_on_discover(legacy_stdio_script(":")));
        let client = stdio_client(dir.path(), script).await.unwrap();
        assert_eq!(client.protocol, Protocol::Legacy("2024-11-05".into()));
        assert_eq!(
            client
                .call_tool("echo", json!({"text": "cold"}))
                .await
                .unwrap(),
            "legacy echoed cold"
        );
        let log = seen(dir.path());
        assert!(log.contains("exited"), "{log}");
        assert_eq!(log.matches("server/discover").count(), 1, "{log}");
        assert_eq!(log.matches(r#""method":"initialize""#).count(), 1, "{log}");
    }

    /// (e): an elicitation in an `input_required` result is declined and
    /// the call retried with the server's `requestState`; a sampling
    /// request, which nothing here can serve, fails the call at once.
    #[cfg(unix)]
    #[tokio::test]
    async fn stdio_input_required_requests_are_declined_not_hung_on() {
        let dir = tempfile::tempdir().unwrap();
        let client = stdio_client(dir.path(), modern_stdio_script())
            .await
            .unwrap();
        let out = tokio::time::timeout(Duration::from_secs(10), client.call_tool("ask", json!({})))
            .await
            .expect("hung on input_required")
            .unwrap();
        assert_eq!(out, "login declined");
        let calls: Vec<Value> = seen(dir.path())
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .filter(|m: &Value| m["params"]["name"] == "ask")
            .collect();
        assert_eq!(calls.len(), 2);
        assert!(calls[0]["params"].get("inputResponses").is_none());
        assert_ne!(calls[0]["id"], calls[1]["id"], "a retry is a new request");
        assert_eq!(
            calls[1]["params"]["inputResponses"],
            json!({"login": {"action": "decline"}})
        );
        assert_eq!(calls[1]["params"]["requestState"], "st-1");

        let err = tokio::time::timeout(
            Duration::from_secs(10),
            client.call_tool("sample", json!({})),
        )
        .await
        .expect("hung on sampling")
        .unwrap_err();
        assert!(err.to_string().contains("sampling/createMessage"), "{err}");
        assert_eq!(seen(dir.path()).matches(r#""name":"sample""#).count(), 1);
    }

    /// The manager reports each server's revision for `/mcp`.
    #[cfg(unix)]
    #[tokio::test]
    async fn statuses_name_the_negotiated_revision() {
        use crate::mcp::types::{McpServerConfig, StdioServerConfig};
        let dir = tempfile::tempdir().unwrap();
        let server = |script: String| {
            McpServerConfig::Stdio(StdioServerConfig {
                command: "sh".into(),
                args: vec!["-c".into(), script],
                env: Default::default(),
                disabled: false,
                literal: false,
            })
        };
        let legacy = legacy_stdio_script(r#"reply '"error":{"code":-32601,"message":"no"}'"#);
        let extra: HashMap<_, _> = [
            ("new".to_string(), server(modern_stdio_script())),
            ("old".to_string(), server(legacy)),
        ]
        .into();
        let m = crate::mcp::McpManager::start_with_extra_timeout(
            &crate::settings::Settings::default(),
            &extra,
            Duration::from_secs(15),
            dir.path(),
        )
        .await;
        let got: Vec<(String, String)> = m
            .statuses()
            .into_iter()
            .map(|s| (s.name, s.protocol))
            .collect();
        assert_eq!(
            got,
            [
                ("new".to_string(), "2026-07-28".to_string()),
                ("old".to_string(), "2024-11-05".to_string())
            ]
        );
    }

    // ── Streamable HTTP ──────────────────────────────────────────────────────

    type Seen = Arc<StdMutex<Vec<(String, Value)>>>;

    /// An HTTP MCP endpoint. `handler` gets the lowercased request head and
    /// the JSON body and returns a status and body, or `None` to hold the
    /// request open without answering. Connections are served concurrently.
    async fn http_server<F>(handler: F) -> (String, Seen)
    where
        F: Fn(&str, &Value) -> Option<(u16, String)> + Send + Sync + 'static,
    {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/mcp", listener.local_addr().unwrap());
        let seen = Seen::default();
        let (log, handler) = (seen.clone(), Arc::new(handler));
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let (log, handler) = (log.clone(), handler.clone());
                tokio::spawn(async move {
                    let mut req = Vec::new();
                    let mut tmp = [0u8; 4096];
                    let (head, body) = loop {
                        let n = sock.read(&mut tmp).await.unwrap_or(0);
                        if n == 0 {
                            return;
                        }
                        req.extend_from_slice(&tmp[..n]);
                        let text = String::from_utf8_lossy(&req).into_owned();
                        let Some(h) = text.find("\r\n\r\n") else {
                            continue;
                        };
                        let head = text[..h].to_ascii_lowercase();
                        let len = head
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length:"))
                            .and_then(|v| v.trim().parse::<usize>().ok())
                            .unwrap_or(0);
                        if req.len() >= h + 4 + len {
                            break (head, text[h + 4..].to_string());
                        }
                    };
                    let body: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
                    log.lock().unwrap().push((head.clone(), body.clone()));
                    let Some((status, out)) = handler(&head, &body) else {
                        tokio::time::sleep(Duration::from_secs(30)).await;
                        return;
                    };
                    let resp = format!(
                        "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\n\
                         content-length: {}\r\nconnection: close\r\n\r\n{out}",
                        out.len()
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                    let _ = sock.shutdown().await;
                });
            }
        });
        (url, seen)
    }

    fn header<'a>(head: &'a str, name: &str) -> Option<&'a str> {
        head.lines()
            .find_map(|l| l.strip_prefix(&format!("{name}:")))
            .map(str::trim)
    }

    fn ok(id: &Value, result: Value) -> Option<(u16, String)> {
        Some((
            200,
            json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string(),
        ))
    }

    fn rpc_err(status: u16, id: &Value, code: i64, data: Value) -> Option<(u16, String)> {
        let e = json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": "no", "data": data}});
        Some((status, e.to_string()))
    }

    async fn http_client(url: &str) -> Result<McpClient> {
        let transport = HttpTransport::new(url, &HashMap::new())?;
        let mut client = McpClient::new("web".into(), "http", Box::new(transport), Some(PROBE));
        tokio::time::timeout(Duration::from_secs(20), client.init())
            .await
            .expect("connect hung")?;
        Ok(client)
    }

    /// A modern Streamable HTTP server that checks what the revision makes
    /// it check: `MCP-Protocol-Version` equal to `_meta`, `Mcp-Method`,
    /// `Mcp-Name`, and `Mcp-Param-*` for `x-mcp-header` arguments.
    fn modern_http(head: &str, body: &Value) -> Option<(u16, String)> {
        let id = &body["id"];
        let meta = body["params"]["_meta"]["io.modelcontextprotocol/protocolVersion"].as_str();
        if meta != Some("2026-07-28") || header(head, "mcp-protocol-version") != meta {
            return rpc_err(400, id, -32602, json!(null));
        }
        let method = body["method"].as_str().unwrap_or_default();
        if header(head, "mcp-method") != Some(method) || head.contains("mcp-session-id") {
            return rpc_err(400, id, -32020, json!(null));
        }
        match method {
            "server/discover" => ok(
                id,
                json!({
                    "resultType": "complete", "supportedVersions": ["2025-11-25", "2026-07-28"],
                    "capabilities": {"tools": {}}, "ttlMs": 60000, "cacheScope": "public"
                }),
            ),
            "tools/list" => ok(
                id,
                json!({"resultType": "complete", "ttlMs": 0, "cacheScope": "private", "tools": [
                    {"name": "execute_sql", "inputSchema": {"type": "object", "properties": {
                        "region": {"type": "string", "x-mcp-header": "Region"},
                        "opts": {"type": "object", "properties": {"dry": {"type": "boolean", "x-mcp-header": "Dry"}}},
                        "query": {"type": "string"}}}},
                    {"name": "bad", "inputSchema": {"type": "object", "properties": {
                        "rows": {"type": "array", "items": {"type": "string", "x-mcp-header": "Row"}}}}},
                    {"name": "ask", "inputSchema": {"type": "object"}}
                ]}),
            ),
            "tools/call" => {
                let name = body["params"]["name"].as_str().unwrap_or_default();
                if header(head, "mcp-name") != Some(name) {
                    return rpc_err(400, id, -32020, json!(null));
                }
                if name == "ask" {
                    return match body["params"]["inputResponses"]["confirm"]["action"].as_str() {
                        Some(action) => ok(
                            id,
                            json!({"resultType": "complete",
                            "content": [{"type": "text", "text": format!("ask {action}")}]}),
                        ),
                        None => ok(
                            id,
                            json!({"resultType": "input_required", "inputRequests": {
                            "confirm": {"method": "elicitation/create", "params": {
                                "mode": "form", "message": "Sure?", "requestedSchema": {"type": "object"}}}}}),
                        ),
                    };
                }
                let args = &body["params"]["arguments"];
                let expect = |h: &str, v: &Value| match v {
                    Value::Null => header(head, h).is_none(),
                    Value::String(s) => header(head, h) == Some(s.to_ascii_lowercase().as_str()),
                    other => header(head, h) == Some(other.to_string().as_str()),
                };
                if !expect("mcp-param-region", &args["region"])
                    || !expect("mcp-param-dry", &args["opts"]["dry"])
                {
                    return rpc_err(400, id, -32020, json!(null));
                }
                ok(
                    id,
                    json!({"resultType": "complete", "content": [{"type": "text",
                    "text": format!("ran {} in {}", args["query"].as_str().unwrap_or(""), args["region"])}]}),
                )
            }
            _ => rpc_err(404, id, -32601, json!(null)),
        }
    }

    /// (a) + (d) + (e) over HTTP: modern headers on every POST, the bad
    /// `x-mcp-header` tool left out, the good one's arguments mirrored, and
    /// an elicitation declined.
    #[tokio::test]
    async fn http_modern_server_gets_stateless_requests_with_mirrored_headers() {
        let (url, seen) = http_server(modern_http).await;
        let client = http_client(&url).await.unwrap();
        assert_eq!(client.protocol, Protocol::Modern);
        let names: Vec<&str> = client.tools.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(
            names,
            ["execute_sql", "ask"],
            "invalid x-mcp-header tool must go"
        );

        let out = client
            .call_tool(
                "execute_sql",
                json!({"region": "us-west1", "opts": {"dry": true}, "query": "SELECT 1"}),
            )
            .await
            .unwrap();
        assert_eq!(out, r#"ran SELECT 1 in "us-west1""#);
        // An absent argument sends no header.
        client
            .call_tool("execute_sql", json!({"query": "SELECT 2"}))
            .await
            .unwrap();
        assert_eq!(
            client.call_tool("ask", json!({})).await.unwrap(),
            "ask decline"
        );

        let seen = seen.lock().unwrap();
        assert!(
            seen.iter().all(|(_, b)| b["method"] != "initialize"),
            "{seen:?}"
        );
        let (head, _) = seen
            .iter()
            .find(|(_, b)| b["params"]["arguments"]["query"] == "SELECT 1")
            .unwrap();
        assert_eq!(header(head, "mcp-param-region"), Some("us-west1"));
        assert_eq!(header(head, "mcp-param-dry"), Some("true"));
        assert_eq!(header(head, "mcp-name"), Some("execute_sql"));
    }

    /// (b) + (d) over HTTP: a legacy server (400 without a modern error to
    /// the probe) gets `initialize`, then its session and its negotiated
    /// version on every later request.
    #[tokio::test]
    async fn http_legacy_server_falls_back_to_the_handshake() {
        let (url, seen) = http_server(|head, body| {
            let id = &body["id"];
            match body["method"].as_str()? {
                "initialize" => Some((200, format!(
                    r#"{{"jsonrpc":"2.0","id":{id},"result":{{"protocolVersion":"2025-03-26","capabilities":{{"tools":{{}}}}}}}}"#
                ))),
                "notifications/initialized" => Some((202, String::new())),
                _ if !head.contains("mcp-protocol-version: 2025-03-26") => {
                    Some((400, r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32000,"message":"Bad Request: Server not initialized"}}"#.into()))
                }
                "tools/list" => ok(id, json!({"tools": [{"name": "echo"}]})),
                "tools/call" => ok(id, json!({"content": [{"type": "text",
                    "text": format!("legacy echoed {}", body["params"]["arguments"]["text"].as_str()?)}]})),
                _ => rpc_err(200, id, -32601, json!(null)),
            }
        })
        .await;
        let client = http_client(&url).await.unwrap();
        assert_eq!(client.protocol, Protocol::Legacy("2025-03-26".into()));
        assert_eq!(
            client
                .call_tool("echo", json!({"text": "hi"}))
                .await
                .unwrap(),
            "legacy echoed hi"
        );
        let seen = seen.lock().unwrap();
        assert_eq!(seen[0].1["method"], "server/discover");
        assert_eq!(seen[1].1["params"]["protocolVersion"], "2025-06-18");
        assert!(
            seen[1..]
                .iter()
                .all(|(_, b)| b["params"].get("_meta").is_none()),
            "{seen:?}"
        );
    }

    /// (c) over HTTP: a server that never answers the probe still gets the
    /// handshake once the probe timeout passes.
    #[tokio::test]
    async fn http_server_silent_on_the_probe_falls_back_within_the_timeout() {
        let (url, _) = http_server(|_, body| {
            let id = &body["id"];
            match body["method"].as_str()? {
                "server/discover" => None,
                "initialize" => ok(
                    id,
                    json!({"protocolVersion": "2024-11-05", "capabilities": {}}),
                ),
                "notifications/initialized" => Some((202, String::new())),
                "tools/list" => ok(id, json!({"tools": [{"name": "echo"}]})),
                "tools/call" => ok(id, json!({"content": [{"type": "text", "text": "pong"}]})),
                _ => None,
            }
        })
        .await;
        let started = std::time::Instant::now();
        let client = http_client(&url).await.unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(8),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(client.protocol, Protocol::Legacy("2024-11-05".into()));
        assert_eq!(client.call_tool("echo", json!({})).await.unwrap(), "pong");
    }

    /// A modern server whose probe is still unanswered after the timeout
    /// (a cold replica) but whose `initialize` gets a modern
    /// `UnsupportedProtocolVersionError` is used as modern, not failed.
    #[tokio::test]
    async fn modern_error_to_initialize_after_a_probe_timeout_means_modern() {
        let (url, seen) = http_server(|head, body| match body["method"].as_str()? {
            "server/discover" => None,
            "initialize" => rpc_err(
                400,
                &body["id"],
                -32022,
                json!({"supported": ["2026-07-28"], "requested": "2025-06-18"}),
            ),
            _ => modern_http(head, body),
        })
        .await;
        let client = http_client(&url).await.unwrap();
        assert_eq!(client.protocol, Protocol::Modern);
        assert_eq!(client.tools.len(), 2, "{:?}", client.tools);
        let seen = seen.lock().unwrap();
        let methods: Vec<&str> = seen
            .iter()
            .map(|(_, b)| b["method"].as_str().unwrap_or_default())
            .collect();
        assert_eq!(methods, ["server/discover", "initialize", "tools/list"]);
    }

    /// `UnsupportedProtocolVersionError` marks a modern server: the client
    /// takes a version from its list, and with none in common it stops
    /// instead of guessing with `initialize`.
    #[tokio::test]
    async fn unsupported_version_error_picks_from_the_servers_list() {
        let (url, seen) = http_server(|_, body| {
            let id = &body["id"];
            match body["method"].as_str()? {
                "server/discover" => rpc_err(400, id, -32022, json!({"supported": ["2099-01-01", "2025-06-18"], "requested": "2026-07-28"})),
                "initialize" => ok(id, json!({"protocolVersion": body["params"]["protocolVersion"], "capabilities": {}})),
                "notifications/initialized" => Some((202, String::new())),
                _ => ok(id, json!({"tools": []})),
            }
        })
        .await;
        let client = http_client(&url).await.unwrap();
        assert_eq!(client.protocol, Protocol::Legacy("2025-06-18".into()));
        assert_eq!(
            seen.lock().unwrap()[1].1["params"]["protocolVersion"],
            "2025-06-18"
        );

        let (url, seen) = http_server(|_, body| {
            rpc_err(
                400,
                &body["id"],
                -32022,
                json!({"supported": ["2099-01-01"], "requested": "2026-07-28"}),
            )
        })
        .await;
        let err = match http_client(&url).await {
            Ok(_) => panic!("connected with no common version"),
            Err(e) => e.to_string(),
        };
        assert!(
            err.contains("2099-01-01") && err.contains("2026-07-28"),
            "{err}"
        );
        assert_eq!(
            seen.lock().unwrap().len(),
            1,
            "no initialize after a modern error"
        );
    }

    #[test]
    fn header_values_are_encoded_as_the_spec_shows() {
        // streamable-http.mdx, "Encoding examples".
        assert_eq!(encode_header_value("us-west1"), "us-west1");
        assert_eq!(
            encode_header_value("Hello, 世界"),
            "=?base64?SGVsbG8sIOS4lueVjA==?="
        );
        assert_eq!(encode_header_value(" padded "), "=?base64?IHBhZGRlZCA=?=");
        assert_eq!(
            encode_header_value("line1\nline2"),
            "=?base64?bGluZTEKbGluZTI=?="
        );
        assert_eq!(
            encode_header_value("=?base64?literal?="),
            "=?base64?PT9iYXNlNjQ/bGl0ZXJhbD89?="
        );
    }

    #[test]
    fn x_mcp_header_annotations_are_validated() {
        let paths = x_mcp_headers(&json!({"type": "object", "properties": {
            "region": {"type": "string", "x-mcp-header": "Region"},
            "n": {"type": ["integer", "null"], "x-mcp-header": "N"},
            "deep": {"type": "object", "properties": {"on": {"type": "boolean", "x-mcp-header": "On"}}},
            "x-mcp-header": {"type": "string"},
            "q": {"type": "string", "default": {"x-mcp-header": "data, not a schema"}}
        }}))
        .unwrap();
        assert_eq!(paths.len(), 3, "{paths:?}");
        assert!(paths.contains(&("On".into(), vec!["deep".into(), "on".into()])));

        for bad in [
            json!({"properties": {"a": {"type": "number", "x-mcp-header": "A"}}}),
            json!({"properties": {"a": {"type": "string", "x-mcp-header": ""}}}),
            json!({"properties": {"a": {"type": "string", "x-mcp-header": "A B"}}}),
            json!({"properties": {"a": {"type": "string", "x-mcp-header": "A"},
                                  "b": {"type": "string", "x-mcp-header": "a"}}}),
            json!({"properties": {"a": {"type": "array", "items": {"type": "string", "x-mcp-header": "A"}}}}),
            json!({"properties": {"a": {"anyOf": [{"type": "string", "x-mcp-header": "A"}]}}}),
            json!({"$defs": {"d": {"type": "string", "x-mcp-header": "A"}}}),
            json!({"type": "string", "x-mcp-header": "Root"}),
        ] {
            assert!(x_mcp_headers(&bad).is_err(), "{bad}");
        }
    }
}
