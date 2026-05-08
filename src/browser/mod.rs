//! Browser automation via Chrome DevTools Protocol.
pub mod actions;
pub mod browse_loop;
pub mod approval_gate;
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
    /// Element ref map: @e1 -> backend DOM node ID
    refs: HashMap<String, i64>,
    /// Element label map: @e1 -> accessible name (for approval gate pattern matching).
    /// Parallel to `refs`; may omit entries when a node has no usable accessible name.
    ref_names: HashMap<String, String>,
    /// Current page URL
    pub current_url: String,
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
        self.client.as_ref().ok_or_else(|| anyhow::anyhow!("Browser not connected. Use /browser or browser_navigate first."))
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

        let mut args = vec![
            format!("--remote-debugging-port={port}"),
            format!("--user-data-dir={}", user_data.path().display()),
            "--no-first-run".to_string(),
            "--no-default-browser-check".to_string(),
            "--disable-background-networking".to_string(),
            "--disable-extensions".to_string(),
            // Container/sandbox robustness: prevent /dev/shm crashes, GPU hangs in headless
            "--disable-dev-shm-usage".to_string(),
        ];
        if headless {
            args.push("--headless=new".to_string());
            args.push("--disable-gpu".to_string());
        }
        args.push("about:blank".to_string());

        let mut child = tokio::process::Command::new(&chrome)
            .args(&args)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| anyhow::anyhow!("Failed to launch Chrome at {}: {e}", chrome.display()))?;

        // Cleanup guard: if poll or connect fail, kill the child before returning.
        let ws_url = match poll_cdp_endpoint(port).await {
            Ok(u) => u,
            Err(e) => {
                let _ = child.kill().await;
                return Err(e);
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
        self.refs.clear();
        self.ref_names.clear();
        self.current_url.clear();
        self.current_title.clear();
    }

    /// Drain and return all console messages captured since the last call.
    /// Includes both `Runtime.consoleAPICalled` events (formatted as
    /// `[level] text`) and `Runtime.exceptionThrown` events (formatted as
    /// `[exception] text`). The buffer is cleared on each call.
    pub async fn take_console_messages(&self) -> Vec<String> {
        let mut buf = self.console_buf.lock().await;
        buf.drain(..).collect()
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
        let key = if r.starts_with('@') { r.to_string() } else { format!("@{r}") };
        self.refs.get(&key)
            .copied()
            .ok_or_else(|| anyhow::anyhow!("Element ref '{key}' not found. Run browser_snapshot first."))
    }

    /// Resolve an @eN ref to its accessible name, if one was captured.
    pub fn resolve_ref_name(&self, r: &str) -> Option<&str> {
        let key = if r.starts_with('@') { r.to_string() } else { format!("@{r}") };
        self.ref_names.get(&key).map(|s| s.as_str())
    }
}

/// Search common Chrome/Chromium binary locations (Linux + macOS).
pub fn find_chrome() -> Option<PathBuf> {
    // PATH lookup via `which` (Linux/macOS)
    let candidates = [
        "google-chrome-stable",
        "google-chrome",
        "chromium-browser",
        "chromium",
        "chrome",
    ];
    for name in &candidates {
        if let Ok(output) = std::process::Command::new("which")
            .arg(name)
            .output()
            && output.status.success() {
                let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if !path.is_empty() {
                    return Some(PathBuf::from(path));
                }
            }
    }
    // Well-known paths — Linux + macOS
    let known: &[&str] = &[
        // Linux
        "/usr/bin/google-chrome-stable",
        "/usr/bin/chromium",
        "/usr/bin/google-chrome",
        "/snap/bin/chromium",
        // macOS
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        "/Applications/Chromium.app/Contents/MacOS/Chromium",
        "/Applications/Brave Browser.app/Contents/MacOS/Brave Browser",
        "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
    ];
    for path in known {
        let p = PathBuf::from(path);
        if p.exists() {
            return Some(p);
        }
    }
    None
}

/// Spawn a background task that subscribes to CDP events and pushes
/// console + exception messages into `buf`. Bounded at `CONSOLE_BUF_CAP`;
/// oldest entries are dropped on overflow. The returned handle is aborted
/// on session close.
fn spawn_console_listener(
    client: &CdpClient,
    buf: Arc<AsyncMutex<VecDeque<String>>>,
) -> JoinHandle<()> {
    let mut rx = client.subscribe();
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(event) => {
                    let formatted = match event.method.as_str() {
                        "Runtime.consoleAPICalled" => format_console_event(&event.params),
                        "Runtime.exceptionThrown" => format_exception_event(&event.params),
                        _ => continue,
                    };
                    if let Some(line) = formatted {
                        let mut guard = buf.lock().await;
                        if guard.len() >= CONSOLE_BUF_CAP {
                            guard.pop_front();
                        }
                        guard.push_back(line);
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

fn format_console_event(params: &serde_json::Value) -> Option<String> {
    let level = params.get("type").and_then(|v| v.as_str()).unwrap_or("log");
    let args = params.get("args").and_then(|v| v.as_array())?;
    let text: Vec<String> = args
        .iter()
        .map(|a| {
            a.get("value")
                .map(stringify_arg)
                .or_else(|| a.get("description").and_then(|v| v.as_str()).map(String::from))
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
        .or_else(|| details.get("exception").and_then(|e| e.get("value").and_then(|v| v.as_str())))
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
async fn poll_cdp_endpoint(port: u16) -> Result<String> {
    let list_url = format!("http://127.0.0.1:{port}/json/list");
    let new_url = format!("http://127.0.0.1:{port}/json/new?about:blank");
    let client = reqwest::Client::new();

    for attempt in 0..30 {
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
