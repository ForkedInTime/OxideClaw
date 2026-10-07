//! Browser automation tools implementing the Tool trait.
//!
//! Each struct wraps a browser action from `src/browser/` and exposes it
//! as a tool the AI model can call. Tools share a single `BrowserSession`
//! so one `browser_navigate` launches the browser and subsequent clicks/fills
//! operate on the same page.
//!
//! ## Locking model
//!
//! The shared `BrowserSession` lives behind an `Arc<Mutex<_>>`. Short-lived
//! operations (resolving an `@eN` ref, updating `current_url`) hold the lock
//! directly. Long-lived CDP operations (page load, `wait_for` polling) take
//! a brief lock to *clone the `CdpClient` handle out*, drop the session
//! guard, then call the action on the cloned client. `CdpClient` is
//! `#[derive(Clone)]` and thread-safe internally. This keeps concurrent
//! browser tool calls (e.g. a `/browser` close while a `wait_for` is in
//! flight) from serializing on the session mutex for up to the full
//! timeout.

use crate::browser::{self, BrowserSession};
use crate::tools::{Tool, ToolContext, ToolOutput};
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;
use tokio::sync::Mutex;

pub type SharedSession = Arc<Mutex<BrowserSession>>;

/// Extract a required string field from a tool input object.
/// Returns a clear error instead of silently substituting a default — the
/// model can then self-correct on the next turn instead of acting on
/// garbage (clicking @e1 when it meant @e42, navigating to about:blank
/// when it meant the actual URL, etc.).
fn required_str<'a>(input: &'a serde_json::Value, field: &str) -> Result<&'a str> {
    input[field]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("missing required field: {field}"))
}

/// Ensure the browser is connected. If `cdp_endpoint` is set (from
/// `browserCdpEndpoint` in settings.json), connect to that existing CDP
/// WebSocket — useful for attaching to a pre-launched Chrome (e.g. one
/// running in a container, or a user's own Chrome opened with
/// `--remote-debugging-port`). Otherwise, launch a fresh headless Chrome
/// using the provided headless + chrome_path settings.
///
/// Each navigator-style tool holds its own copy of these settings
/// (captured at construction from the Config), so no ToolContext fields
/// are needed.
async fn ensure_launched(
    session: &SharedSession,
    headless: bool,
    chrome_path: Option<&str>,
    cdp_endpoint: Option<&str>,
) -> Result<()> {
    let mut s = session.lock().await;
    // A dead socket (Chrome crashed, or the connection dropped) leaves the
    // client in place, so `is_connected` would keep reporting true and every
    // tool would fail with "CDP connection is closed" until a manual
    // /browser close. Tear it down so the branch below starts fresh; close()
    // also reaps the old Chrome child and console task.
    if s.client().is_ok_and(|c| !c.is_alive()) {
        s.close().await;
    }
    if !s.is_connected() {
        if let Some(endpoint) = cdp_endpoint {
            s.connect(endpoint).await?;
        } else {
            s.launch(headless, chrome_path).await?;
        }
    }
    Ok(())
}

/// Clone the `CdpClient` out from under the session lock, or fail with the
/// "Browser not connected" error. Callers use this when they need to run a
/// long CDP operation without holding the session mutex.
async fn clone_client(session: &SharedSession) -> Result<browser::cdp::CdpClient> {
    let s = session.lock().await;
    Ok(s.client()?.clone())
}

/// Result trailer listing the JavaScript dialogs the action just raised (the
/// console listener answers them automatically), or "" when there were none.
async fn dialog_trailer(session: &SharedSession) -> String {
    let lines = session.lock().await.take_dialog_messages().await;
    if lines.is_empty() {
        String::new()
    } else {
        format!("\n\nJavaScript dialogs:\n{}", lines.join("\n"))
    }
}

// ── browser_navigate ────────────────────────────────────────────────────────

pub struct BrowserNavigateTool {
    pub session: SharedSession,
    pub headless: bool,
    pub chrome_path: Option<String>,
    /// If set, attach to an existing CDP endpoint instead of launching Chrome.
    pub cdp_endpoint: Option<String>,
    /// Page-load timeout (ms) applied to `actions::navigate`.
    pub timeout_ms: u64,
}

#[async_trait]
impl Tool for BrowserNavigateTool {
    fn name(&self) -> &str {
        "browser_navigate"
    }
    fn description(&self) -> &str {
        "Navigate the browser to a URL. Launches the browser if not already running. \
         Returns the page title and an accessibility snapshot with the page's text, \
         fenced as untrusted page content."
    }
    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "url": { "type": "string", "description": "URL to navigate to" }
            },
            "required": ["url"]
        })
    }
    async fn execute(&self, input: serde_json::Value, _ctx: &ToolContext) -> Result<ToolOutput> {
        let url = required_str(&input, "url")?;
        ensure_launched(
            &self.session,
            self.headless,
            self.chrome_path.as_deref(),
            self.cdp_endpoint.as_deref(),
        )
        .await?;

        // Clone the CdpClient out so Page.navigate + load-event wait
        // (up to timeout_ms) does not hold the session lock.
        let client = clone_client(&self.session).await?;
        let (title, status) = browser::actions::navigate(&client, url, self.timeout_ms).await?;
        // Chrome follows redirects itself; record where the page actually
        // landed, which the approval gate's URL patterns match against.
        let final_url = browser::actions::current_url(&client)
            .await
            .unwrap_or_else(|| url.to_string());
        let dialogs = dialog_trailer(&self.session).await;

        // Snapshot afterwards. This also uses the client, not the session,
        // so still no session lock held.
        let tree = match browser::snapshot::take_snapshot(&client).await {
            Ok(snap) => {
                // Brief re-lock to publish the post-navigation state.
                let mut session = self.session.lock().await;
                session.current_url = final_url.clone();
                session.current_title = title.clone();
                session.set_refs_with_names(snap.refs, snap.names);
                session.last_page_text = snap.page_text;
                snap.tree
            }
            Err(e) => {
                // Snapshot failed (e.g. page still settling). Still publish
                // the navigation result so the model sees we made progress.
                let mut session = self.session.lock().await;
                session.current_url = final_url.clone();
                session.current_title = title.clone();
                session.last_page_text.clear();
                format!("(snapshot unavailable: {e})")
            }
        };

        let status = match status {
            Some(code @ 400..) => format!("\nStatus: {code} (error page)"),
            Some(code) => format!("\nStatus: {code}"),
            None => String::new(),
        };
        // The title is the page's too, so it goes inside the fence.
        let page = browser::snapshot::wrap_untrusted(&format!(
            "Title: {}\n\nAccessibility snapshot:\n{tree}",
            title.split_whitespace().collect::<Vec<_>>().join(" ")
        ));
        Ok(ToolOutput::success(format!(
            "Navigated to: {final_url}{status}{dialogs}\n\n{page}"
        )))
    }
}

// ── browser_snapshot ────────────────────────────────────────────────────────

pub struct BrowserSnapshotTool {
    pub session: SharedSession,
}

#[async_trait]
impl Tool for BrowserSnapshotTool {
    fn name(&self) -> &str {
        "browser_snapshot"
    }
    fn description(&self) -> &str {
        "Capture a fresh accessibility snapshot of the current page. \
         Returns a text tree with @eN refs you can pass to browser_click/browser_fill/etc., \
         plus the page's text, fenced as untrusted page content."
    }
    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {},
            "required": []
        })
    }
    async fn execute(&self, _input: serde_json::Value, _ctx: &ToolContext) -> Result<ToolOutput> {
        let client = clone_client(&self.session).await?;
        browser::actions::ensure_page_allowed(&client).await?;
        let snap = browser::snapshot::take_snapshot(&client).await?;
        // A redirect or JS navigation may have moved the page since the last
        // tool recorded its URL.
        let live_url = browser::actions::current_url(&client).await;
        {
            let mut s = self.session.lock().await;
            s.set_refs_with_names(snap.refs, snap.names);
            s.last_page_text = snap.page_text;
            if let Some(u) = live_url {
                s.current_url = u;
            }
        }
        Ok(ToolOutput::success(browser::snapshot::wrap_untrusted(
            &snap.tree,
        )))
    }
}

// ── browser_click ───────────────────────────────────────────────────────────

pub struct BrowserClickTool {
    pub session: SharedSession,
}

#[async_trait]
impl Tool for BrowserClickTool {
    fn name(&self) -> &str {
        "browser_click"
    }
    fn description(&self) -> &str {
        "Click an element identified by an @eN ref from the latest accessibility snapshot. \
         Auto-snapshots afterwards so the updated page is visible."
    }
    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "ref": {
                    "type": "string",
                    "description": "Element ref from the last snapshot, e.g. \"@e3\""
                }
            },
            "required": ["ref"]
        })
    }
    async fn execute(&self, input: serde_json::Value, _ctx: &ToolContext) -> Result<ToolOutput> {
        let element_ref = required_str(&input, "ref")?;

        // click needs the session (ref map + resolve_ref), so hold the lock.
        // This is fast — no long awaits inside actions::click.
        let (result, client) = {
            let mut session = self.session.lock().await;
            let result = browser::actions::click(&mut session, element_ref).await?;
            let client = session.client()?.clone();
            (result, client)
        };
        let dialogs = dialog_trailer(&self.session).await;
        // The click may have navigated somewhere blocked; check before the
        // auto-snapshot reads the new page.
        browser::actions::ensure_page_allowed(&client).await?;

        // Auto-snapshot uses only the client — no session lock held during
        // the CDP round-trip. If the snapshot fails (e.g. page navigated
        // away mid-click), report that as a soft warning rather than
        // losing the successful click result.
        let trailer = match browser::snapshot::take_snapshot(&client).await {
            Ok(snap) => {
                {
                    let mut s = self.session.lock().await;
                    s.set_refs_with_names(snap.refs, snap.names);
                    s.last_page_text = snap.page_text;
                }
                format!(
                    "\n\nUpdated snapshot:\n{}",
                    browser::snapshot::wrap_untrusted(&snap.tree)
                )
            }
            Err(e) => format!("\n\n(snapshot unavailable: {e})"),
        };

        // Refresh current_url — clicks can trigger navigation.
        if let Some(new_url) = browser::actions::current_url(&client).await {
            self.session.lock().await.current_url = new_url;
        }

        Ok(ToolOutput::success(format!("{result}{dialogs}{trailer}")))
    }
}

// ── browser_fill ────────────────────────────────────────────────────────────

pub struct BrowserFillTool {
    pub session: SharedSession,
}

#[async_trait]
impl Tool for BrowserFillTool {
    fn name(&self) -> &str {
        "browser_fill"
    }
    fn description(&self) -> &str {
        "Fill a text input / textarea / contenteditable element identified by an @eN ref. \
         Clears the existing value first. On a <select> (combobox/listbox), chooses the \
         option whose value or visible text matches `value`."
    }
    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "ref":   { "type": "string", "description": "Element ref from the last snapshot" },
                "value": { "type": "string", "description": "Text to type into the element, or the option to choose in a <select>" }
            },
            "required": ["ref", "value"]
        })
    }
    async fn execute(&self, input: serde_json::Value, _ctx: &ToolContext) -> Result<ToolOutput> {
        let element_ref = required_str(&input, "ref")?;
        // `value` is required by schema but may legitimately be the empty
        // string (to clear a field). Accept "" here rather than erroring.
        let value = input["value"]
            .as_str()
            .ok_or_else(|| anyhow!("missing required field: value"))?;
        let (result, client) = {
            let mut session = self.session.lock().await;
            let result = browser::actions::fill(&mut session, element_ref, value).await?;
            let client = session.client()?.clone();
            (result, client)
        };
        let dialogs = dialog_trailer(&self.session).await;
        // Fill can trigger navigation on some SPAs (e.g. submit-on-change forms).
        if let Some(new_url) = browser::actions::current_url(&client).await {
            self.session.lock().await.current_url = new_url;
        }
        Ok(ToolOutput::success(format!("{result}{dialogs}")))
    }
}

// ── browser_screenshot ──────────────────────────────────────────────────────

pub struct BrowserScreenshotTool {
    pub session: SharedSession,
}

#[async_trait]
impl Tool for BrowserScreenshotTool {
    fn name(&self) -> &str {
        "browser_screenshot"
    }
    fn description(&self) -> &str {
        "Capture a PNG screenshot of the current page. Saves to a temp file and returns the path. \
         Pass full_page=true for a full-document capture."
    }
    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "full_page": {
                    "type": "boolean",
                    "description": "Capture the entire scroll height instead of just the viewport",
                    "default": false
                }
            },
            "required": []
        })
    }
    async fn execute(&self, input: serde_json::Value, _ctx: &ToolContext) -> Result<ToolOutput> {
        let full_page = input["full_page"].as_bool().unwrap_or(false);
        let client = clone_client(&self.session).await?;
        browser::actions::ensure_page_allowed(&client).await?;
        let b64 = browser::actions::screenshot(&client, full_page).await?;

        use base64::Engine;
        let bytes = base64::engine::general_purpose::STANDARD.decode(&b64)?;

        // Unique filename per screenshot — process-wide timestamp in nanos.
        // Prevents collisions across parallel agents sharing $TMPDIR and
        // preserves screenshot history within a session.
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let path = std::env::temp_dir().join(format!("oxideclaw_screenshot_{ts}.png"));
        tokio::fs::write(&path, &bytes).await?;

        Ok(ToolOutput::success(format!(
            "Screenshot saved to {}\n({} bytes, base64 length: {})",
            path.display(),
            bytes.len(),
            b64.len()
        )))
    }
}

// ── browser_get_text ────────────────────────────────────────────────────────

pub struct BrowserGetTextTool {
    pub session: SharedSession,
}

#[async_trait]
impl Tool for BrowserGetTextTool {
    fn name(&self) -> &str {
        "browser_get_text"
    }
    fn description(&self) -> &str {
        "Read the inner text / textContent of an element identified by an @eN ref. \
         The text is fenced as untrusted page content."
    }
    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "ref": { "type": "string", "description": "Element ref from the last snapshot" }
            },
            "required": ["ref"]
        })
    }
    async fn execute(&self, input: serde_json::Value, _ctx: &ToolContext) -> Result<ToolOutput> {
        let element_ref = required_str(&input, "ref")?;
        let client = clone_client(&self.session).await?;
        browser::actions::ensure_page_allowed(&client).await?;
        let mut session = self.session.lock().await;
        let text = browser::actions::get_text(&mut session, element_ref).await?;
        Ok(ToolOutput::success(browser::snapshot::wrap_untrusted(
            &text,
        )))
    }
}

// ── browser_press_key ───────────────────────────────────────────────────────

pub struct BrowserPressKeyTool {
    pub session: SharedSession,
}

#[async_trait]
impl Tool for BrowserPressKeyTool {
    fn name(&self) -> &str {
        "browser_press_key"
    }
    fn description(&self) -> &str {
        "Send a single key press to the focused element (a named key or a single character)."
    }
    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "key": {
                    "type": "string",
                    "description": "A single character, or one of \"Enter\", \"Tab\", \"Escape\", \"Backspace\", \"Delete\", \"Space\", \"ArrowLeft\", \"ArrowUp\", \"ArrowRight\", \"ArrowDown\", \"Home\", \"End\", \"PageUp\", \"PageDown\""
                }
            },
            "required": ["key"]
        })
    }
    async fn execute(&self, input: serde_json::Value, _ctx: &ToolContext) -> Result<ToolOutput> {
        let key = required_str(&input, "key")?;
        let client = clone_client(&self.session).await?;
        let result = browser::actions::press_key(&client, key).await?;
        let dialogs = dialog_trailer(&self.session).await;
        // Enter/Tab can submit a form and trigger navigation.
        if let Some(new_url) = browser::actions::current_url(&client).await {
            self.session.lock().await.current_url = new_url;
        }
        Ok(ToolOutput::success(format!("{result}{dialogs}")))
    }
}

// ── browser_wait ────────────────────────────────────────────────────────────

pub struct BrowserWaitTool {
    pub session: SharedSession,
    /// Fallback timeout (ms) when the tool input omits `timeout_ms`.
    /// Sourced from `browserTimeoutMs` in settings.json.
    pub default_timeout_ms: u64,
}

#[async_trait]
impl Tool for BrowserWaitTool {
    fn name(&self) -> &str {
        "browser_wait"
    }
    fn description(&self) -> &str {
        "Wait until a CSS selector matches an element on the page, or until the timeout elapses."
    }
    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "selector":   { "type": "string", "description": "CSS selector to wait for" },
                "timeout_ms": {
                    "type": "integer",
                    "description": "Timeout in milliseconds (falls back to browserTimeoutMs from settings)"
                }
            },
            "required": ["selector"]
        })
    }
    async fn execute(&self, input: serde_json::Value, _ctx: &ToolContext) -> Result<ToolOutput> {
        let selector = required_str(&input, "selector")?;
        let timeout_ms = input["timeout_ms"]
            .as_u64()
            .unwrap_or(self.default_timeout_ms);

        // Clone the client so the polling loop does NOT hold the session
        // lock for the full timeout window.
        let client = clone_client(&self.session).await?;
        let result = browser::actions::wait_for(&client, selector, timeout_ms).await?;
        // Waiting is how the model rides out a redirect chain; record where
        // it ended up.
        if let Some(u) = browser::actions::current_url(&client).await {
            self.session.lock().await.current_url = u;
        }
        Ok(ToolOutput::success(result))
    }
}

// ── browser_console ─────────────────────────────────────────────────────────

pub struct BrowserConsoleTool {
    pub session: SharedSession,
}

#[async_trait]
impl Tool for BrowserConsoleTool {
    fn name(&self) -> &str {
        "browser_console"
    }
    fn description(&self) -> &str {
        "Drain and return console messages and uncaught exceptions captured \
         since the last call. Each entry is prefixed with [level], \
         [exception] or [dialog:type]. The buffer is cleared after each call."
    }
    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {},
            "required": []
        })
    }
    async fn execute(&self, _input: serde_json::Value, _ctx: &ToolContext) -> Result<ToolOutput> {
        let session = self.session.lock().await;
        let _ = session.client()?;
        let messages = session.take_console_messages().await;
        if messages.is_empty() {
            Ok(ToolOutput::success("(no console messages)".to_string()))
        } else {
            Ok(ToolOutput::success(messages.join("\n")))
        }
    }
}

// ── browse_done ──────────────────────────────────────────────────────────────

/// Tool the model calls to end an autonomous `/browse` run. The browse engine
/// stops after a successful call and reports the call's own `achieved` and
/// `summary` arguments; the returned text is only an acknowledgement.
pub struct BrowseDoneTool;

impl BrowseDoneTool {
    pub fn new() -> Self {
        Self
    }
}

impl Default for BrowseDoneTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for BrowseDoneTool {
    fn name(&self) -> &str {
        "browse_done"
    }
    fn description(&self) -> &str {
        "Signal that the browser goal is achieved or that you're stuck."
    }
    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "summary":  { "type": "string", "description": "Short summary of what was found or done during browsing" },
                "achieved": { "type": "boolean", "description": "True if the browsing goal was accomplished, false if it could not be" }
            },
            "required": ["summary", "achieved"]
        })
    }
    async fn execute(&self, input: serde_json::Value, _ctx: &ToolContext) -> Result<ToolOutput> {
        let summary = input["summary"]
            .as_str()
            .ok_or_else(|| anyhow!("missing required field: summary"))?;
        let achieved = input["achieved"]
            .as_bool()
            .ok_or_else(|| anyhow!("missing required field: achieved"))?;
        Ok(ToolOutput::success(format!(
            "BROWSE_DONE achieved={achieved} summary={summary}"
        )))
    }
}
