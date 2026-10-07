/// MCP client — connects to a single MCP server via stdio or HTTP.
///
/// Implements the Model Context Protocol (MCP) JSON-RPC 2.0 protocol.
/// Stdio transport: spawns the server process and communicates via stdin/stdout.
/// HTTP transport:  POSTs JSON-RPC requests to a URL (streamable HTTP).
use crate::mcp::types::{JsonRpcRequest, JsonRpcResponse, McpCallResult, McpResource, McpToolDef};
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

const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
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
const MCP_PROTOCOL_VERSION: &str = "2024-11-05";

// ── Transport trait ───────────────────────────────────────────────────────────

#[async_trait]
pub(crate) trait McpTransport: Send + Sync {
    /// Send a request and await its response.
    async fn call(&self, id: u64, method: &str, params: Value) -> Result<Value>;

    /// Send a notification (fire-and-forget, no response expected).
    async fn notify(&self, _method: &str) {}
}

// ── Stdio transport ───────────────────────────────────────────────────────────

pub(crate) struct StdioTransport {
    stdin_tx: tokio::sync::mpsc::UnboundedSender<String>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value>>>>>,
    /// Set by the reader, under the `pending` lock, once stdout hits EOF.
    /// A request registered after the reader's final drain would otherwise
    /// wait out the full timeout for a reply that can never come.
    closed: Arc<AtomicBool>,
}

impl StdioTransport {
    pub async fn connect(
        command: &str,
        args: &[String],
        env: &HashMap<String, String>,
        cwd: &std::path::Path,
    ) -> Result<Self> {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
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

        // Reap task: prevent zombie process
        tokio::spawn(async move {
            let _ = child.wait().await;
        });

        // Reader task: child stdout → pending oneshots
        let pending_clone = Arc::clone(&pending);
        let closed = Arc::new(AtomicBool::new(false));
        let closed_clone = Arc::clone(&closed);
        // Weak, so the reader never keeps the child's stdin open after the
        // transport is dropped: stdin EOF is how the server learns to exit.
        let reply_tx = stdin_tx.downgrade();
        tokio::spawn(async move {
            let reader = BufReader::new(stdout);
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                // Ignore malformed / partial lines
                let Ok(msg) = serde_json::from_str::<Value>(trimmed) else {
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
                    Err(anyhow!("MCP error {}: {}", err.code, err.message))
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
        })
    }
}

/// Where a shell would find a bare `command`, walking PATH × PATHEXT.
/// Windows process creation only tries `<name>.exe`, but `npx` (the usual
/// MCP launcher) and most Node and Python shims are `.cmd` files, so
/// `"command": "npx"` failed to spawn. Std quotes arguments safely when it
/// runs a `.cmd`/`.bat` by full path.
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

        match tokio::time::timeout(REQUEST_TIMEOUT, rx).await {
            Ok(reply) => reply.map_err(|_| anyhow!("MCP server disconnected"))?,
            Err(_) => {
                self.pending.lock().await.remove(&id);
                Err(anyhow!("MCP request timed out ({})", method))
            }
        }
    }

    async fn notify(&self, method: &str) {
        let req = JsonRpcRequest::notification(method);
        if let Ok(json) = serde_json::to_string(&req) {
            let _ = self.stdin_tx.send(json);
        }
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
    /// Deadline for a whole exchange, body included. `send()` resolves at
    /// the headers, so a server that then stalls would otherwise hang the
    /// tool call forever; reqwest has no default read timeout.
    timeout: Duration,
}

impl HttpTransport {
    // Auth is static by decision (2026-09-11): headers come from
    // settings.json → mcpServers.*.headers and are sent as-is. An expired
    // bearer token surfaces as a loud "HTTP MCP <method> failed: 401"; the
    // user replaces it and restarts. No OAuth discovery/PKCE/refresh flow is
    // planned — documented in SECURITY.md.
    pub fn new(url: &str, headers: &HashMap<String, String>) -> Result<Self> {
        let mut builder = reqwest::Client::builder();

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

        Ok(Self {
            url: url.to_string(),
            client: builder.build()?,
            session_id: std::sync::Mutex::new(None),
            timeout: REQUEST_TIMEOUT,
        })
    }
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

    async fn post(&self, req: &JsonRpcRequest) -> Result<reqwest::Response> {
        // Streamable-HTTP servers reject (406) a POST that does not accept
        // both; they may answer with plain JSON or an SSE stream.
        let mut builder = self
            .client
            .post(&self.url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream");
        if let Some(sid) = self.session() {
            builder = builder.header("Mcp-Session-Id", sid);
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

/// The JSON-RPC response for `id` carried by one SSE event, if that is
/// what the event holds (not a notification or a server-to-client request).
fn sse_event_response(event: &[u8], id: u64) -> Option<JsonRpcResponse> {
    let text = String::from_utf8_lossy(event);
    let data: Vec<&str> = text
        .split('\n')
        .map(|l| l.trim_end_matches('\r'))
        .filter_map(|l| l.strip_prefix("data:"))
        .map(|d| d.strip_prefix(' ').unwrap_or(d))
        .collect();
    if data.is_empty() {
        return None;
    }
    let v: Value = serde_json::from_str(&data.join("\n")).ok()?;
    let is_response = v.get("result").is_some() || v.get("error").is_some();
    if !is_response || v.get("id").and_then(Value::as_u64) != Some(id) {
        return None;
    }
    serde_json::from_value(v).ok()
}

impl HttpTransport {
    async fn call_inner(&self, id: u64, method: &str, params: Value) -> Result<Value> {
        let req = JsonRpcRequest::new(id, method, params);
        let resp = self.post(&req).await?;

        if !resp.status().is_success() {
            let status = resp.status();
            if status == reqwest::StatusCode::NOT_FOUND && self.session().is_some() {
                return Err(anyhow!(
                    "HTTP MCP {method} failed: session expired — restart oxideclaw to reconnect"
                ));
            }
            let body = Self::bounded_body(resp, method).await.unwrap_or_default();
            let body = String::from_utf8_lossy(&body);
            return Err(anyhow!("HTTP MCP {} failed: {} — {}", method, status, body));
        }

        if let Some(sid) = resp
            .headers()
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
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
            return Err(anyhow!("MCP error {}: {}", err.code, err.message));
        }

        Ok(rpc_resp.result.unwrap_or(Value::Null))
    }
}

#[async_trait]
impl McpTransport for HttpTransport {
    async fn call(&self, id: u64, method: &str, params: Value) -> Result<Value> {
        tokio::time::timeout(self.timeout, self.call_inner(id, method, params))
            .await
            .map_err(|_| anyhow!("HTTP MCP request timed out ({method})"))?
    }

    /// Streamable-HTTP servers expect `notifications/initialized` like any
    /// other transport; a notification has no id and its reply is ignored.
    async fn notify(&self, method: &str) {
        let req = JsonRpcRequest::notification(method);
        let _ = tokio::time::timeout(self.timeout, self.post(&req)).await;
    }
}

// ── McpClient ─────────────────────────────────────────────────────────────────

pub struct McpClient {
    pub server_name: String,
    pub tools: Vec<McpToolDef>,
    pub transport_kind: &'static str, // "stdio" | "http"
    transport: Box<dyn McpTransport>,
    next_id: AtomicU64,
}

impl McpClient {
    // ── Internal helpers ──────────────────────────────────────────────────────

    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.transport.call(id, method, params).await
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

    /// Run the MCP initialize handshake and populate self.tools.
    async fn init(&mut self) -> Result<()> {
        // 1. initialize
        let params = json!({
            "protocolVersion": MCP_PROTOCOL_VERSION,
            // Nothing here answers roots/list or sampling/createMessage; a
            // server told we do waits on them until its own timeout.
            "capabilities": {},
            "clientInfo": {
                "name": "oxideclaw",
                "version": env!("CARGO_PKG_VERSION")
            }
        });
        let init = self
            .request("initialize", params)
            .await
            .map_err(|e| anyhow!("MCP initialize failed for '{}': {}", self.server_name, e))?;

        // 2. Notify server that client is ready (fire-and-forget)
        self.transport.notify("notifications/initialized").await;

        // 3. Fetch tool list — with cursor pagination so servers that return
        //    more than one page worth of tools aren't silently truncated.
        //    A failure leaves the server connected (resource- or prompt-only
        //    servers answer -32601), but say so when it claims tools: a
        //    silent "connected (0 tools)" hid real transport errors.
        let pages = match self.list_paginated("tools/list", "tools").await {
            Ok(pages) => pages,
            Err(e) => {
                if init.pointer("/capabilities/tools").is_some() {
                    tracing::warn!("MCP '{}': tools/list failed: {}", self.server_name, e);
                } else {
                    tracing::debug!("MCP '{}': tools/list failed: {}", self.server_name, e);
                }
                Vec::new()
            }
        };
        self.tools = pages
            .into_iter()
            .filter_map(|v| serde_json::from_value(v).ok())
            .collect();

        Ok(())
    }

    // ── Public API ────────────────────────────────────────────────────────────

    /// Connect to a stdio MCP server.
    pub async fn connect_stdio(
        server_name: String,
        command: &str,
        args: &[String],
        env: &HashMap<String, String>,
        cwd: &std::path::Path,
    ) -> Result<Self> {
        let transport = StdioTransport::connect(command, args, env, cwd).await?;
        let mut client = Self {
            server_name,
            tools: Vec::new(),
            transport_kind: "stdio",
            transport: Box::new(transport),
            next_id: AtomicU64::new(1),
        };
        client.init().await?;
        Ok(client)
    }

    /// Connect to an HTTP MCP server (streamable HTTP transport).
    pub async fn connect_http(
        server_name: String,
        url: &str,
        headers: &HashMap<String, String>,
    ) -> Result<Self> {
        let transport = HttpTransport::new(url, headers)?;
        let mut client = Self {
            server_name,
            tools: Vec::new(),
            transport_kind: "http",
            transport: Box::new(transport),
            next_id: AtomicU64::new(1),
        };
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
        McpClient {
            server_name: "mock".into(),
            tools: Vec::new(),
            transport_kind: "stdio",
            transport: Box::new(mock),
            next_id: AtomicU64::new(1),
        }
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

        let client = McpClient {
            server_name: "mock".into(),
            tools: Vec::new(),
            transport_kind: "stdio",
            transport: Box::new(ArcAdapter(mock)),
            next_id: AtomicU64::new(1),
        };

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

        let client = McpClient {
            server_name: "mock".into(),
            tools: Vec::new(),
            transport_kind: "stdio",
            transport: Box::new(ArcAdapter(Arc::clone(&mock))),
            next_id: AtomicU64::new(1),
        };

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
        McpClient {
            server_name: "mock".into(),
            tools: Vec::new(),
            transport_kind: "stdio",
            transport: Box::new(Canned(resp)),
            next_id: AtomicU64::new(1),
        }
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
    #[tokio::test]
    async fn http_transport_speaks_streamable_http() {
        let init = r#"{"jsonrpc":"2.0","id":1,"result":{"capabilities":{"tools":{}}}}"#;
        let sse = "event: message\r\n\
                   data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/message\",\"params\":{}}\r\n\r\n\
                   data: {\"jsonrpc\":\"2.0\",\"id\":99,\"result\":{}}\n\n\
                   event: message\n\
                   data: {\"jsonrpc\":\"2.0\",\"id\":2,\n\
                   data: \"result\":{\"tools\":[{\"name\":\"echo\"}]}}\n\n";
        let (base, seen) = recording_server(vec![
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

        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 4);
        for req in seen.iter() {
            assert!(
                req.contains("accept: application/json, text/event-stream"),
                "{req}"
            );
        }
        assert!(!seen[0].contains("mcp-session-id"), "{}", seen[0]);
        for req in &seen[1..] {
            assert!(req.contains("mcp-session-id: sess-42"), "{req}");
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
        let err = tokio::time::timeout(Duration::from_secs(10), t.call(1, "tools/call", json!({})))
            .await
            .expect("the transport's own deadline must fire")
            .unwrap_err();
        assert!(err.to_string().contains("timed out"), "{err}");
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
}
