//! Browser automation via Chrome DevTools Protocol.
pub mod actions;
pub mod approval_gate;
pub mod browse_loop;
pub mod cdp;
pub mod element;
pub mod loop_detector;
pub mod middleware;
pub mod snapshot;
pub mod yolo_ack;

use anyhow::{Result, bail};
use cdp::CdpClient;
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use tempfile::TempDir;
use tokio::process::Child;
use tokio::sync::Mutex as AsyncMutex;
use tokio::task::JoinHandle;

/// Bounded ring buffer cap for captured console messages.
const CONSOLE_BUF_CAP: usize = 500;

/// Active browser session — owns the CDP connection, Chrome child process, and page state.
#[derive(Default)]
pub struct BrowserSession {
    client: Option<CdpClient>,
    /// Chrome child process — kept alive for the session; killed on close/drop.
    child: Option<Child>,
    /// Temp user-data-dir — kept alive so Chrome's profile directory is not deleted.
    _user_data: Option<TempDir>,
    /// Policy proxy every connection of a launched Chrome goes through; lives
    /// as long as that Chrome.
    _proxy: Option<crate::net_policy::PolicyProxy>,
    /// Element ref map: @e1 -> backend DOM node ID
    refs: HashMap<String, i64>,
    /// Element label map: @e1 -> accessible name (for approval gate pattern matching).
    /// Parallel to `refs`; may omit entries when a node has no usable accessible name.
    ref_names: HashMap<String, String>,
    /// Current page URL
    pub current_url: String,
    /// Text of the last snapshot / get_text, for the approval gate's
    /// visible-price signal.
    pub last_page_text: String,
    /// Current page title
    pub current_title: String,
    /// Captured console messages (Runtime.consoleAPICalled + Runtime.exceptionThrown).
    /// Bounded at `CONSOLE_BUF_CAP`; oldest dropped on overflow.
    console_buf: Arc<AsyncMutex<VecDeque<String>>>,
    /// Background task that drains CDP events into `console_buf`. Aborted on close.
    console_task: Option<JoinHandle<()>>,
}

impl BrowserSession {
    pub fn is_connected(&self) -> bool {
        self.client.is_some()
    }

    /// Expose the ref map for inspection (debugging, REPL, future /browser diagnostics).
    /// Not currently used by any tool — kept for parity with snapshot.rs, which
    /// mutates this same map via `set_refs`.
    #[allow(dead_code)]
    pub fn ref_map(&self) -> &HashMap<String, i64> {
        &self.refs
    }

    pub fn client(&self) -> Result<&CdpClient> {
        self.client.as_ref().ok_or_else(|| {
            anyhow::anyhow!("Browser not connected. Use /browser or browser_navigate first.")
        })
    }

    /// Launch Chrome and connect via CDP.
    pub async fn launch(&mut self, headless: bool, chrome_path: Option<&str>) -> Result<()> {
        if self.client.is_some() {
            return Ok(());
        }

        let chrome = match chrome_path {
            Some(p) => PathBuf::from(p),
            None => find_chrome().ok_or_else(|| anyhow::anyhow!(
                "Chrome/Chromium not found. Install Chrome or set browserChromePath in settings.json"
            ))?,
        };

        let user_data = tempfile::tempdir()?;
        let port = find_free_port().await?;
        // Chrome follows redirects, link clicks, meta refresh and script
        // navigation and resolves DNS on its own, so the preflight on
        // browser_navigate's URL left the metadata service one 302 away.
        // Every connection it makes is resolved, checked and pinned by this
        // proxy instead. LOCAL_OK: dev servers on loopback and the LAN stay
        // reachable; link-local never is.
        let proxy =
            crate::net_policy::spawn_policy_proxy(crate::net_policy::NetPolicy::LOCAL_OK).await?;

        let args = launch_args(port, user_data.path(), proxy.addr, headless, runs_as_root());

        let mut child = tokio::process::Command::new(&chrome)
            .args(&args)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| anyhow::anyhow!("Failed to launch Chrome at {}: {e}", chrome.display()))?;
        // Chrome explains a failed start only on stderr. Drained for the
        // whole session so a chatty Chrome never blocks on a full pipe.
        let stderr_tail = Arc::new(std::sync::Mutex::new(Vec::new()));
        let drain = child
            .stderr
            .take()
            .map(|pipe| tokio::spawn(keep_tail(pipe, stderr_tail.clone())));

        // Cleanup guard: if poll or connect fail, kill the child before returning.
        let ws_url = match poll_cdp_endpoint(port, &mut child).await {
            Ok(u) => u,
            Err(e) => {
                let _ = child.kill().await;
                // Chrome's helpers can hold the pipe open after it exits.
                if let Some(d) = drain {
                    let _ = tokio::time::timeout(std::time::Duration::from_secs(1), d).await;
                }
                let tail = stderr_tail.lock().unwrap_or_else(|e| e.into_inner());
                let tail = String::from_utf8_lossy(&tail);
                let tail = tail.trim();
                if tail.is_empty() {
                    return Err(e);
                }
                return Err(anyhow::anyhow!("{e}\nChrome stderr:\n{tail}"));
            }
        };
        let client = match CdpClient::connect(&ws_url).await {
            Ok(c) => c,
            Err(e) => {
                let _ = child.kill().await;
                return Err(e);
            }
        };

        self.console_task = Some(spawn_console_listener(&client, self.console_buf.clone()));
        self.client = Some(client);
        self.child = Some(child);
        self._user_data = Some(user_data);
        self._proxy = Some(proxy);
        Ok(())
    }

    /// Connect to an existing CDP endpoint.
    pub async fn connect(&mut self, endpoint: &str) -> Result<()> {
        let client = CdpClient::connect(endpoint).await?;
        self.console_task = Some(spawn_console_listener(&client, self.console_buf.clone()));
        self.client = Some(client);
        Ok(())
    }

    /// Close the browser session — kills Chrome and frees the user-data-dir.
    pub async fn close(&mut self) {
        // Stop the console listener before tearing down the CDP client so it
        // doesn't observe a half-dead connection.
        if let Some(task) = self.console_task.take() {
            task.abort();
        }
        self.console_buf.lock().await.clear();
        // Drop the CDP client first to close the WebSocket.
        self.client = None;
        // Kill the Chrome process if we launched it (connect() doesn't set child).
        if let Some(mut child) = self.child.take() {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
        // Drop the TempDir now so the profile directory is removed.
        self._user_data = None;
        self._proxy = None;
        self.refs.clear();
        self.ref_names.clear();
        self.current_url.clear();
        self.current_title.clear();
    }

    /// Drain and return all console messages captured since the last call.
    /// Includes both `Runtime.consoleAPICalled` events (formatted as
    /// `[level] text`), `Runtime.exceptionThrown` events (formatted as
    /// `[exception] text`) and auto-handled dialogs not yet drained by
    /// `take_dialog_messages`. The buffer is cleared on each call.
    pub async fn take_console_messages(&self) -> Vec<String> {
        let mut buf = self.console_buf.lock().await;
        buf.drain(..).collect()
    }

    /// Drain only the auto-handled dialog lines, leaving console output for
    /// `browser_console`. Action tools append these to their result: a
    /// dismissed confirm() means the click did not do what the model
    /// intended, and it would otherwise never find out.
    pub async fn take_dialog_messages(&self) -> Vec<String> {
        let mut buf = self.console_buf.lock().await;
        let mut dialogs = Vec::new();
        buf.retain(|line| {
            if line.starts_with(DIALOG_LINE_PREFIX) {
                dialogs.push(line.clone());
                false
            } else {
                true
            }
        });
        dialogs
    }

    /// Update ref map (called after each snapshot). Names are optional — pass
    /// an empty map to preserve the old behavior.
    #[allow(dead_code)]
    pub fn set_refs(&mut self, refs: HashMap<String, i64>) {
        self.refs = refs;
    }

    /// Update both the ref map and the parallel name map (called after each snapshot).
    pub fn set_refs_with_names(
        &mut self,
        refs: HashMap<String, i64>,
        names: HashMap<String, String>,
    ) {
        self.refs = refs;
        self.ref_names = names;
    }

    /// Resolve an @eN ref to a backend node ID.
    pub fn resolve_ref(&self, r: &str) -> Result<i64> {
        let key = normalize_ref(r);
        self.refs.get(&key).copied().ok_or_else(|| {
            anyhow::anyhow!("Element ref '{key}' not found. Run browser_snapshot first.")
        })
    }

    /// Resolve an @eN ref to its accessible name, if one was captured.
    pub fn resolve_ref_name(&self, r: &str) -> Option<&str> {
        let key = normalize_ref(r);
        self.ref_names.get(&key).map(|s| s.as_str())
    }
}

/// Canonical `@eN` spelling of an element ref. The tools accept a bare `eN`
/// too, so every check keyed on the ref (approval gate, loop detector) must
/// normalize the same way or the bare spelling slips past it.
pub fn normalize_ref(r: &str) -> String {
    if r.starts_with('@') {
        r.to_string()
    } else {
        format!("@{r}")
    }
}

/// Bytes of Chrome's stderr kept for a launch error.
const STDERR_TAIL: usize = 2048;

/// Read `pipe` to EOF, keeping only its last `STDERR_TAIL` bytes in `tail`.
async fn keep_tail(mut pipe: tokio::process::ChildStderr, tail: Arc<std::sync::Mutex<Vec<u8>>>) {
    use tokio::io::AsyncReadExt;
    let mut buf = [0u8; 4096];
    while let Ok(n) = pipe.read(&mut buf).await {
        if n == 0 {
            break;
        }
        let mut t = tail.lock().unwrap_or_else(|e| e.into_inner());
        t.extend_from_slice(&buf[..n]);
        let excess = t.len().saturating_sub(STDERR_TAIL);
        t.drain(..excess);
    }
}

/// Chrome on Linux refuses to start as root unless its sandbox is off
/// ("Running as root without --no-sandbox is not supported"), and root is
/// the default user in Docker and most CI runners.
pub(crate) fn runs_as_root() -> bool {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: geteuid has no preconditions and cannot fail.
        unsafe { libc::geteuid() == 0 }
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

/// Chrome's command line for a launched session. Every connection goes
/// through the policy proxy at `proxy`.
fn launch_args(
    port: u16,
    user_data: &std::path::Path,
    proxy: std::net::SocketAddr,
    headless: bool,
    no_sandbox: bool,
) -> Vec<String> {
    let mut args = vec![
        format!("--remote-debugging-port={port}"),
        format!("--user-data-dir={}", user_data.display()),
        "--no-first-run".to_string(),
        "--no-default-browser-check".to_string(),
        "--disable-background-networking".to_string(),
        "--disable-extensions".to_string(),
        // Container/sandbox robustness: prevent /dev/shm crashes, GPU hangs in headless
        "--disable-dev-shm-usage".to_string(),
    ];
    args.extend(crate::net_policy::chromium_proxy_args(proxy));
    if no_sandbox {
        args.push("--no-sandbox".to_string());
    }
    if headless {
        args.push("--headless=new".to_string());
        args.push("--disable-gpu".to_string());
    }
    args.push("about:blank".to_string());
    args
}

/// Find an installed Chromium-based browser: Chrome, Chromium, Brave or
/// Edge, on PATH or in its usual install location.
pub fn find_chrome() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("PATH").and_then(|path| find_on_path(&path)) {
        return Some(p);
    }
    let mut known: Vec<PathBuf> = [
        // Linux
        "/usr/bin/google-chrome-stable",
        "/usr/bin/google-chrome",
        "/usr/bin/chromium",
        "/usr/bin/chromium-browser",
        "/snap/bin/chromium",
        "/usr/bin/brave-browser",
        "/usr/bin/microsoft-edge-stable",
        "/usr/bin/microsoft-edge",
        // macOS
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        "/Applications/Chromium.app/Contents/MacOS/Chromium",
        "/Applications/Brave Browser.app/Contents/MacOS/Brave Browser",
        "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
    ]
    .iter()
    .map(PathBuf::from)
    .collect();
    // Windows installers rarely put the browser on PATH.
    if cfg!(windows) {
        for root in ["PROGRAMFILES", "PROGRAMFILES(X86)", "LOCALAPPDATA"] {
            let Some(root) = std::env::var_os(root) else {
                continue;
            };
            for rel in [
                r"Google\Chrome\Application\chrome.exe",
                r"Microsoft\Edge\Application\msedge.exe",
                r"BraveSoftware\Brave-Browser\Application\brave.exe",
                r"Chromium\Application\chrome.exe",
            ] {
                known.push(PathBuf::from(&root).join(rel));
            }
        }
    }
    known.into_iter().find(|p| p.is_file())
}

/// First browser executable found in the directories of `path` (a PATH
/// value). Walked here rather than through `which`, which stock Windows
/// does not have.
fn find_on_path(path: &std::ffi::OsStr) -> Option<PathBuf> {
    const NAMES: [&str; 10] = [
        "google-chrome-stable",
        "google-chrome",
        "chromium-browser",
        "chromium",
        "chrome",
        "brave-browser",
        "brave",
        "microsoft-edge-stable",
        "microsoft-edge",
        "msedge",
    ];
    let dirs: Vec<PathBuf> = std::env::split_paths(path).collect();
    NAMES.iter().find_map(|name| {
        let file = if cfg!(windows) {
            format!("{name}.exe")
        } else {
            name.to_string()
        };
        dirs.iter()
            .map(|d| d.join(&file))
            .find(|p| is_executable(p))
    })
}

fn is_executable(p: &std::path::Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        p.is_file()
    }
}

/// Spawn a background task that subscribes to CDP events and pushes
/// console + exception messages into `buf`. Bounded at `CONSOLE_BUF_CAP`;
/// oldest entries are dropped on overflow. The returned handle is aborted
/// on session close.
///
/// It also answers JavaScript dialogs. With the Page domain enabled Chrome
/// hands alert/confirm/prompt/beforeunload to the CDP client and blocks the
/// renderer until `Page.handleJavaScriptDialog` arrives; headless Chrome has
/// no UI to close them, so an unanswered dialog stalls every later command
/// on the page until it times out.
fn spawn_console_listener(
    client: &CdpClient,
    buf: Arc<AsyncMutex<VecDeque<String>>>,
) -> JoinHandle<()> {
    let mut rx = client.subscribe();
    let client = client.clone();
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(event) => {
                    let mut dialog_accept = None;
                    let formatted = match event.method.as_str() {
                        "Runtime.consoleAPICalled" => format_console_event(&event.params),
                        "Runtime.exceptionThrown" => format_exception_event(&event.params),
                        "Page.javascriptDialogOpening" => {
                            let (accept, line) = dialog_response(&event.params);
                            dialog_accept = Some(accept);
                            Some(line)
                        }
                        _ => continue,
                    };
                    if let Some(line) = formatted {
                        let mut guard = buf.lock().await;
                        if guard.len() >= CONSOLE_BUF_CAP {
                            guard.pop_front();
                        }
                        guard.push_back(line);
                    }
                    // Logged before answering: the blocked click/navigate only
                    // returns once the dialog closes, and its tool result
                    // drains the dialog line, so it must already be there.
                    if let Some(accept) = dialog_accept {
                        let c = client.clone();
                        tokio::spawn(async move {
                            let _ = c
                                .send(
                                    "Page.handleJavaScriptDialog",
                                    serde_json::json!({ "accept": accept }),
                                )
                                .await;
                        });
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    // High event rate — drop the lag and keep listening.
                    continue;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    })
}

/// Prefix of the console-buffer lines that record an auto-handled dialog.
const DIALOG_LINE_PREFIX: &str = "[dialog:";

/// Decide how to answer a `Page.javascriptDialogOpening` event and describe
/// it for the model. beforeunload is accepted because the agent itself asked
/// to leave the page; alert/confirm/prompt are dismissed so no confirmation
/// ("Delete this repo?") is ever silently granted.
fn dialog_response(params: &serde_json::Value) -> (bool, String) {
    let ty = params
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("alert");
    let message = params.get("message").and_then(|v| v.as_str()).unwrap_or("");
    let accept = ty == "beforeunload";
    let outcome = if accept {
        "auto-accepted"
    } else {
        "auto-dismissed"
    };
    (
        accept,
        format!("{DIALOG_LINE_PREFIX}{ty}] {message} ({outcome})"),
    )
}

fn format_console_event(params: &serde_json::Value) -> Option<String> {
    let level = params.get("type").and_then(|v| v.as_str()).unwrap_or("log");
    let args = params.get("args").and_then(|v| v.as_array())?;
    let text: Vec<String> = args
        .iter()
        .map(|a| {
            a.get("value")
                .map(stringify_arg)
                .or_else(|| {
                    a.get("description")
                        .and_then(|v| v.as_str())
                        .map(String::from)
                })
                .unwrap_or_else(|| "<unprintable>".into())
        })
        .collect();
    Some(format!("[{level}] {}", text.join(" ")))
}

fn format_exception_event(params: &serde_json::Value) -> Option<String> {
    let details = params.get("exceptionDetails")?;
    let text = details.get("text").and_then(|v| v.as_str()).unwrap_or("");
    let exception_text = details
        .get("exception")
        .and_then(|e| e.get("description").and_then(|d| d.as_str()))
        .or_else(|| {
            details
                .get("exception")
                .and_then(|e| e.get("value").and_then(|v| v.as_str()))
        })
        .unwrap_or("");
    let combined = if exception_text.is_empty() {
        text.to_string()
    } else {
        format!("{text} {exception_text}").trim().to_string()
    };
    Some(format!("[exception] {combined}"))
}

fn stringify_arg(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Find a free TCP port for Chrome's debugging port.
async fn find_free_port() -> Result<u16> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| anyhow::anyhow!("Failed to bind loopback socket: {e}"))?;
    Ok(listener.local_addr()?.port())
}

/// Poll Chrome's /json/list endpoint and return the WebSocket URL for the first
/// page-type target. Page-level commands like `Page.enable` only work on a
/// page target; the browser-level WebSocket from /json/version returns
/// `'Page.enable' wasn't found`. If no page target exists yet, fall back to
/// creating one via PUT /json/new.
async fn poll_cdp_endpoint(port: u16, child: &mut Child) -> Result<String> {
    let list_url = format!("http://127.0.0.1:{port}/json/list");
    let new_url = format!("http://127.0.0.1:{port}/json/new?about:blank");
    let client = reqwest::Client::new();

    for attempt in 0..30 {
        // A Chrome that refused to start used to cost the full 6 s and
        // surface only as "no page target".
        if let Ok(Some(status)) = child.try_wait() {
            bail!("Chrome exited during startup ({status})");
        }
        if let Ok(resp) = client.get(&list_url).send().await
            && let Ok(targets) = resp.json::<serde_json::Value>().await
            && let Some(arr) = targets.as_array()
        {
            if let Some(ws) = arr
                .iter()
                .find(|t| t["type"].as_str() == Some("page"))
                .and_then(|t| t["webSocketDebuggerUrl"].as_str())
            {
                return Ok(ws.to_string());
            }
            // Chrome is up (list responded) but has no page target — create one.
            // Do this once, on attempt 2+ so we don't race the about:blank launcher arg.
            if attempt >= 2
                && let Ok(resp) = client.put(&new_url).send().await
                && let Ok(target) = resp.json::<serde_json::Value>().await
                && let Some(ws) = target["webSocketDebuggerUrl"].as_str()
            {
                return Ok(ws.to_string());
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    bail!("Chrome did not expose a page target within 6 seconds on port {port}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A launched Chrome had no proxy, so it followed a 302 (or a click, or a
    /// script) to 169.254.169.254 unchecked. All of its traffic, loopback
    /// included, must go through the policy proxy.
    #[test]
    fn launched_chrome_is_pinned_to_the_policy_proxy() {
        let proxy: std::net::SocketAddr = "127.0.0.1:4242".parse().unwrap();
        for headless in [true, false] {
            let args = launch_args(9222, std::path::Path::new("/p"), proxy, headless, false);
            assert!(args.contains(&"--proxy-server=http://127.0.0.1:4242".to_string()));
            assert!(args.contains(&"--proxy-bypass-list=<-loopback>".to_string()));
            assert!(
                args.contains(&"--force-webrtc-ip-handling-policy=disable_non_proxied_udp".into())
            );
            assert_eq!(args.last().unwrap(), "about:blank");
        }
    }

    /// Only Chrome and Chromium were looked for, through `which`, so a
    /// machine with just Brave or Edge (and any Windows machine) had none.
    #[cfg(unix)]
    #[test]
    fn brave_or_edge_on_path_is_found() {
        use std::os::unix::fs::PermissionsExt;
        for name in ["brave-browser", "microsoft-edge"] {
            let empty = tempfile::tempdir().unwrap();
            let dir = tempfile::tempdir().unwrap();
            let exe = dir.path().join(name);
            std::fs::write(&exe, "").unwrap();
            let path = std::env::join_paths([empty.path(), dir.path()]).unwrap();
            assert_eq!(find_on_path(&path), None, "not executable yet");
            std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
            assert_eq!(find_on_path(&path), Some(exe));
        }
    }

    #[test]
    fn the_sandbox_is_off_only_when_asked() {
        let proxy: std::net::SocketAddr = "127.0.0.1:4242".parse().unwrap();
        let p = std::path::Path::new("/p");
        assert!(launch_args(9222, p, proxy, true, true).contains(&"--no-sandbox".to_string()));
        assert!(!launch_args(9222, p, proxy, true, false).contains(&"--no-sandbox".to_string()));
    }

    /// Chrome refusing to start (as root without --no-sandbox, a missing
    /// library) was reported after 6 s as "did not expose a page target",
    /// with its stderr thrown away.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_chrome_that_exits_at_startup_reports_its_stderr_at_once() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("chrome");
        std::fs::write(
            &fake,
            "#!/bin/sh\necho 'Running as root without --no-sandbox is not supported.' >&2\nexit 1\n",
        )
        .unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();

        let started = std::time::Instant::now();
        let err = BrowserSession::default()
            .launch(true, Some(fake.to_str().unwrap()))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("exited during startup"), "{err}");
        assert!(
            err.contains("without --no-sandbox is not supported"),
            "{err}"
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(4),
            "{err}"
        );
    }

    #[test]
    fn dialogs_are_dismissed_except_beforeunload() {
        let (accept, line) =
            dialog_response(&json!({"type": "confirm", "message": "Delete repo?"}));
        assert!(!accept);
        assert_eq!(line, "[dialog:confirm] Delete repo? (auto-dismissed)");
        assert!(!dialog_response(&json!({"type": "prompt", "message": "Name?"})).0);
        assert!(!dialog_response(&json!({"type": "alert", "message": "hi"})).0);
        let (accept, line) = dialog_response(&json!({"type": "beforeunload", "message": ""}));
        assert!(
            accept,
            "agent-requested navigation must not hang on beforeunload"
        );
        assert!(line.ends_with("(auto-accepted)"));
    }

    #[test]
    fn console_event_formats_string_args() {
        let params = json!({
            "type": "log",
            "args": [
                {"type": "string", "value": "hello"},
                {"type": "number", "value": 42},
            ]
        });
        assert_eq!(
            format_console_event(&params).as_deref(),
            Some("[log] hello 42")
        );
    }

    #[test]
    fn console_event_falls_back_to_description_for_objects() {
        let params = json!({
            "type": "error",
            "args": [
                {"type": "object", "description": "Error: oops"},
            ]
        });
        assert_eq!(
            format_console_event(&params).as_deref(),
            Some("[error] Error: oops")
        );
    }

    #[test]
    fn console_event_defaults_level_to_log() {
        let params = json!({
            "args": [{"type": "string", "value": "no-level"}]
        });
        assert_eq!(
            format_console_event(&params).as_deref(),
            Some("[log] no-level")
        );
    }

    #[test]
    fn console_event_returns_none_without_args() {
        let params = json!({"type": "log"});
        assert!(format_console_event(&params).is_none());
    }

    #[test]
    fn exception_event_formats_text_and_description() {
        let params = json!({
            "exceptionDetails": {
                "text": "Uncaught",
                "exception": {"description": "TypeError: x is undefined"}
            }
        });
        assert_eq!(
            format_exception_event(&params).as_deref(),
            Some("[exception] Uncaught TypeError: x is undefined")
        );
    }

    #[test]
    fn exception_event_handles_missing_exception() {
        let params = json!({
            "exceptionDetails": {"text": "Uncaught"}
        });
        assert_eq!(
            format_exception_event(&params).as_deref(),
            Some("[exception] Uncaught")
        );
    }

    #[tokio::test]
    async fn take_console_messages_drains_and_clears() {
        let session = BrowserSession::default();
        {
            let mut buf = session.console_buf.lock().await;
            buf.push_back("[log] one".into());
            buf.push_back("[error] two".into());
        }
        let drained = session.take_console_messages().await;
        assert_eq!(drained, vec!["[log] one", "[error] two"]);
        assert!(session.take_console_messages().await.is_empty());
    }

    #[tokio::test]
    async fn console_buf_drops_oldest_on_overflow() {
        let buf: Arc<AsyncMutex<VecDeque<String>>> = Arc::new(AsyncMutex::new(VecDeque::new()));
        // Simulate the listener push path with a small synthetic cap.
        for i in 0..(CONSOLE_BUF_CAP + 5) {
            let mut g = buf.lock().await;
            if g.len() >= CONSOLE_BUF_CAP {
                g.pop_front();
            }
            g.push_back(format!("msg {i}"));
        }
        let g = buf.lock().await;
        assert_eq!(g.len(), CONSOLE_BUF_CAP);
        assert_eq!(g.front().unwrap(), &format!("msg {}", 5));
        assert_eq!(g.back().unwrap(), &format!("msg {}", CONSOLE_BUF_CAP + 4));
    }
}
