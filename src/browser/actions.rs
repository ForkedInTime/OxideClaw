//! Browser action functions: navigate, click, fill, screenshot, etc.
//!
//! Functions that only need the CDP connection take `&CdpClient` so callers
//! can clone the client out from under a session lock and drop the lock
//! before a long-running operation (page load, wait_for polling, etc.).
//! Functions that need the ref map (click / fill / get_text) still take
//! `&mut BrowserSession` — those are fast, no long awaits.
use super::BrowserSession;
use super::cdp::CdpClient;
use anyhow::{Result, bail};
use serde_json::json;

/// URL schemes the browser is allowed to navigate to. Anything else
/// (`javascript:`, `data:`, `file:`, `ftp:`, …) is rejected up front so a
/// model can't pivot the session into local-file disclosure or in-page
/// script execution.
const ALLOWED_SCHEMES: &[&str] = &["http", "https", "about"];

/// Reject URLs that aren't plain HTTP(S) or `about:blank` style. Returns the
/// validated URL on success.
fn validate_navigation_url(url: &str) -> Result<()> {
    let trimmed = url.trim();
    if trimmed.is_empty() {
        bail!("navigation URL is empty");
    }
    let lower = trimmed.to_ascii_lowercase();
    let scheme = match lower.split_once(':') {
        Some((s, _))
            if !s.is_empty()
                && s.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '-' || c == '.') =>
        {
            s.to_string()
        }
        // No scheme — treat as relative; reject so callers always pass absolute URLs.
        _ => bail!("navigation URL '{url}' is missing an http(s):// scheme"),
    };
    if !ALLOWED_SCHEMES.contains(&scheme.as_str()) {
        bail!(
            "navigation URL scheme '{scheme}:' is not allowed; only http/https/about are permitted"
        );
    }
    Ok(())
}

/// Everything checked before `Page.navigate` is sent: the scheme allowlist
/// plus the always-denied address tier (link-local / cloud metadata). The
/// user's browser may reach loopback and private networks — testing a local
/// dev server is the primary use of `/browse` — so this uses
/// `NetPolicy::LOCAL_OK`, not the strict fetch policy.
pub async fn preflight_navigation_url(url: &str) -> Result<()> {
    validate_navigation_url(url)?;
    let trimmed = url.trim();
    if trimmed.to_ascii_lowercase().starts_with("about:") {
        return Ok(());
    }
    let parsed = url::Url::parse(trimmed)
        .map_err(|e| anyhow::anyhow!("navigation URL '{url}' is invalid: {e}"))?;
    crate::net_policy::NetPolicy::LOCAL_OK
        .resolve(&parsed)
        .await?;
    Ok(())
}

/// Refuse to read a page that has moved to a blocked destination, and blank
/// it. Chrome follows redirects, link clicks, meta refresh and script
/// navigation without asking, so the preflight on the requested URL says
/// nothing about where the page is now. A launched Chrome cannot even load
/// such a page (its traffic goes through the policy proxy); this is what
/// covers a Chrome attached through `browserCdpEndpoint`, which has no proxy.
pub async fn ensure_page_allowed(client: &CdpClient) -> Result<()> {
    let Some(href) = current_url(client).await else {
        return Ok(());
    };
    if let Err(e) = landed_url_verdict(&href).await {
        let _ = client
            .send("Page.navigate", json!({"url": "about:blank"}))
            .await;
        bail!(
            "the page moved to a blocked destination ({e}); the browser was reset to about:blank"
        );
    }
    Ok(())
}

/// Policy verdict on a page's live location. Only addresses that resolve and
/// fail the policy count: a host this machine cannot resolve (a Chrome in a
/// container sees other DNS) is not evidence of anything, and Chrome's own
/// schemes (about:, chrome-error:, data:) have no destination.
async fn landed_url_verdict(href: &str) -> Result<()> {
    let Ok(url) = url::Url::parse(href) else {
        return Ok(());
    };
    if !matches!(url.scheme(), "http" | "https") {
        return Ok(());
    }
    let Some(port) = url.port_or_known_default() else {
        return Ok(());
    };
    let ips: Vec<std::net::IpAddr> = match url.host() {
        Some(url::Host::Ipv4(ip)) => vec![ip.into()],
        Some(url::Host::Ipv6(ip)) => vec![ip.into()],
        Some(url::Host::Domain(name)) => match tokio::net::lookup_host((name, port)).await {
            Ok(addrs) => addrs.map(|a| a.ip()).collect(),
            Err(_) => Vec::new(),
        },
        None => Vec::new(),
    };
    for ip in ips {
        crate::net_policy::NetPolicy::LOCAL_OK
            .check_ip(ip)
            .map_err(|e| anyhow::anyhow!("{}: {e}", url.host_str().unwrap_or("host")))?;
    }
    Ok(())
}

/// Navigate to a URL. Returns (title, status). Does NOT mutate session state —
/// the caller is responsible for updating `current_url` / `current_title`
/// after this returns, so the session lock can be released while we wait on
/// the page load event (bounded by `timeout_ms`).
pub async fn navigate(client: &CdpClient, url: &str, timeout_ms: u64) -> Result<(String, u16)> {
    preflight_navigation_url(url).await?;
    // Subscribe BEFORE navigating so we don't miss Page.loadEventFired on fast loads.
    let mut events = client.subscribe();

    let result = client.send("Page.navigate", json!({"url": url})).await?;
    if let Some(err) = result["errorText"].as_str()
        && !err.is_empty()
    {
        anyhow::bail!("Navigation failed: {err}");
    }

    // A same-document navigation (only the #fragment changed, as in a
    // hash-routed SPA) has no loaderId and never fires Page.loadEventFired;
    // waiting for it ran out the whole timeout and reported a failure.
    let same_document = result["loaderId"].as_str().is_none_or(str::is_empty);
    if !same_document {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
        loop {
            match tokio::time::timeout_at(deadline, events.recv()).await {
                Ok(Ok(ev)) if ev.method == "Page.loadEventFired" => break,
                Ok(Ok(_)) => continue,
                Ok(Err(_)) => break, // Channel lagged, page likely loaded
                Err(_) => anyhow::bail!("Page load timed out after {timeout_ms}ms"),
            }
        }
    }
    // A redirect may have taken the page somewhere the preflight would refuse.
    ensure_page_allowed(client).await?;

    // Get page title
    let eval = client
        .send(
            "Runtime.evaluate",
            json!({ "expression": "document.title" }),
        )
        .await?;
    let title = eval["result"]["value"].as_str().unwrap_or("").to_string();

    Ok((title, 200))
}

/// Query the current page URL via `document.location.href`. Returns `None`
/// on error or if no URL is available (e.g. the page has no window yet).
pub async fn current_url(client: &CdpClient) -> Option<String> {
    let resp = client
        .send(
            "Runtime.evaluate",
            json!({
                "expression": "document.location.href",
                "returnByValue": true,
            }),
        )
        .await
        .ok()?;
    resp["result"]["value"].as_str().map(|s| s.to_string())
}

/// Label of the control a key press on the focused element would activate,
/// so the approval gate can match it like a click. Enter and Space press the
/// focused button or link; Enter in a text input also submits its form
/// through the form's default button (`implicit_submit`). Without this,
/// "Tab to Delete account, press Enter" or "fill the confirm field, press
/// Enter" got past every button pattern. Returns `None` on error or when the
/// focus is on nothing that activates (Enter in a textarea adds a newline).
pub async fn active_element_label(client: &CdpClient, implicit_submit: bool) -> Option<String> {
    let expression = format!(
        r#"(() => {{
  const implicitSubmit = {implicit_submit};
  let el = document.activeElement;
  // Same-origin frames expose their focus; cross-origin ones stay opaque.
  while (el && (el.tagName === 'IFRAME' || el.tagName === 'FRAME') && el.contentDocument) {{
    el = el.contentDocument.activeElement;
  }}
  if (!el) return '';
  const label = (e) => (e.getAttribute('aria-label') || e.innerText || e.value || e.getAttribute('alt') || '').trim();
  const type = (e) => (e.getAttribute('type') || '').toLowerCase();
  const isSubmit = (e) => (e.tagName === 'BUTTON' && (type(e) === '' || type(e) === 'submit'))
    || (e.tagName === 'INPUT' && (type(e) === 'submit' || type(e) === 'image'));
  if (el.tagName === 'BUTTON' || el.tagName === 'A' || el.getAttribute('role') === 'button'
      || (el.tagName === 'INPUT' && ['submit', 'button', 'image', 'reset'].includes(type(el)))) {{
    return label(el);
  }}
  if (implicitSubmit && el.tagName === 'INPUT' && el.form) {{
    // form.elements covers controls bound with form="id" but skips image inputs.
    const def = Array.from(el.form.elements).find(isSubmit)
      || el.form.querySelector('input[type=image]');
    return def ? label(def) : '';
  }}
  return '';
}})()"#
    );
    let resp = client
        .send(
            "Runtime.evaluate",
            json!({ "expression": expression, "returnByValue": true }),
        )
        .await
        .ok()?;
    resp["result"]["value"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
}

/// Click an element by @ref.
pub async fn click(session: &mut BrowserSession, element_ref: &str) -> Result<String> {
    let node_id = session.resolve_ref(element_ref)?;
    let client = session.client()?;

    // Resolve node to a RemoteObject for interaction
    let resolved = client
        .send("DOM.resolveNode", json!({"backendNodeId": node_id}))
        .await?;
    let object_id = resolved["object"]["objectId"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Could not resolve element {element_ref} to JS object"))?;

    // Scroll into view
    let _ = client
        .send(
            "Runtime.callFunctionOn",
            json!({
                "objectId": object_id,
                "functionDeclaration": "function() { this.scrollIntoViewIfNeeded(); }",
            }),
        )
        .await;

    // Get element center coordinates
    let box_model = client
        .send("DOM.getBoxModel", json!({"backendNodeId": node_id}))
        .await?;
    let content = &box_model["model"]["content"];
    if let Some(coords) = content.as_array()
        && coords.len() >= 4
    {
        let x = (coords[0].as_f64().unwrap_or(0.0) + coords[2].as_f64().unwrap_or(0.0)) / 2.0;
        let y = (coords[1].as_f64().unwrap_or(0.0) + coords[5].as_f64().unwrap_or(0.0)) / 2.0;

        // Mouse click sequence
        for event_type in ["mousePressed", "mouseReleased"] {
            client
                .send(
                    "Input.dispatchMouseEvent",
                    json!({
                        "type": event_type,
                        "x": x,
                        "y": y,
                        "button": "left",
                        "clickCount": 1,
                    }),
                )
                .await?;
        }
        return Ok(format!("Clicked {element_ref} at ({x:.0}, {y:.0})"));
    }

    // Fallback: JS click
    client
        .send(
            "Runtime.callFunctionOn",
            json!({
                "objectId": object_id,
                "functionDeclaration": "function() { this.click(); }",
            }),
        )
        .await?;
    Ok(format!("Clicked {element_ref} (JS fallback)"))
}

/// Fill a text input by @ref.
pub async fn fill(session: &mut BrowserSession, element_ref: &str, value: &str) -> Result<String> {
    let node_id = session.resolve_ref(element_ref)?;
    let client = session.client()?;

    // Focus the element
    client
        .send("DOM.focus", json!({"backendNodeId": node_id}))
        .await?;

    // Clear existing value by calling .value = '' on the resolved element directly
    // (not on document.activeElement, which could be anything after focus changes).
    let resolved = client
        .send("DOM.resolveNode", json!({"backendNodeId": node_id}))
        .await?;
    if let Some(object_id) = resolved["object"]["objectId"].as_str() {
        let _ = client
            .send(
                "Runtime.callFunctionOn",
                json!({
                    "objectId": object_id,
                    "functionDeclaration":
                        "function() { if ('value' in this) this.value = ''; \
                                      else if (this.isContentEditable) this.textContent = ''; }",
                }),
            )
            .await;
    }

    // Type the value (handles input events correctly)
    client
        .send("Input.insertText", json!({"text": value}))
        .await?;

    Ok(fill_summary(element_ref, value))
}

/// What the model is told after a fill. The value itself is **not** echoed:
/// it may be a password or card number, and tool results go into the
/// transcript on disk. (The old `&value[..50]` also panicked on a
/// multi-byte character at byte 50.)
fn fill_summary(element_ref: &str, value: &str) -> String {
    format!("Filled {element_ref} ({} chars)", value.chars().count())
}

/// Take a screenshot. Returns base64-encoded PNG.
pub async fn screenshot(client: &CdpClient, full_page: bool) -> Result<String> {
    let mut params = json!({"format": "png"});
    if full_page {
        let metrics = client.send("Page.getLayoutMetrics", json!({})).await?;
        let width = metrics["cssContentSize"]["width"]
            .as_f64()
            .unwrap_or(1280.0);
        let height = metrics["cssContentSize"]["height"]
            .as_f64()
            .unwrap_or(720.0);
        params["clip"] = json!({
            "x": 0, "y": 0,
            "width": width, "height": height,
            "scale": 1,
        });
        // Without it Chrome rasterizes only the viewport and the clip below
        // the fold comes back blank.
        params["captureBeyondViewport"] = json!(true);
    }
    let result = client.send("Page.captureScreenshot", params).await?;
    let data = result["data"].as_str().unwrap_or("").to_string();
    Ok(data)
}

/// Named keys `press_key` accepts: (name, key, code, Windows virtual key
/// code, text). Chrome's editing commands (Backspace deletes, arrows move
/// the caret) and page handlers reading `e.keyCode` go by the virtual key
/// code; without it Backspace did nothing. The approval gate treats
/// "enter"/"return" as submit and "space"/" " as activating, so no other
/// spelling may map to those keys.
const NAMED_KEYS: &[(&str, &str, &str, i64, &str)] = &[
    ("enter", "Enter", "Enter", 13, "\r"),
    ("return", "Enter", "Enter", 13, "\r"),
    ("tab", "Tab", "Tab", 9, ""),
    ("escape", "Escape", "Escape", 27, ""),
    ("esc", "Escape", "Escape", 27, ""),
    ("backspace", "Backspace", "Backspace", 8, ""),
    ("delete", "Delete", "Delete", 46, ""),
    ("space", " ", "Space", 32, " "),
    (" ", " ", "Space", 32, " "),
    ("arrowleft", "ArrowLeft", "ArrowLeft", 37, ""),
    ("arrowup", "ArrowUp", "ArrowUp", 38, ""),
    ("arrowright", "ArrowRight", "ArrowRight", 39, ""),
    ("arrowdown", "ArrowDown", "ArrowDown", 40, ""),
    ("home", "Home", "Home", 36, ""),
    ("end", "End", "End", 35, ""),
    ("pageup", "PageUp", "PageUp", 33, ""),
    ("pagedown", "PageDown", "PageDown", 34, ""),
];

/// The `Input.dispatchKeyEvent` fields for `key`: (key, code, virtual key
/// code, text). Unknown names are refused: sent as text, Chrome rejected
/// names over 4 characters and typed shorter ones ("Home") literally.
fn key_event(key: &str) -> Result<(String, String, i64, String)> {
    // ASCII folding, exactly as the approval gate folds the key it checks.
    let lower = key.to_ascii_lowercase();
    if let Some(&(_, k, code, vk, text)) = NAMED_KEYS.iter().find(|e| e.0 == lower) {
        return Ok((k.into(), code.into(), vk, text.into()));
    }
    let mut chars = key.chars();
    // Control characters are refused too: "\r" as text submits a form
    // without the approval gate seeing an Enter.
    if let (Some(c), None) = (chars.next(), chars.next())
        && !c.is_control()
    {
        let upper = c.to_ascii_uppercase();
        let (code, vk) = if upper.is_ascii_uppercase() {
            (format!("Key{upper}"), upper as i64)
        } else if c.is_ascii_digit() {
            (format!("Digit{c}"), c as i64)
        } else {
            (String::new(), 0)
        };
        return Ok((c.to_string(), code, vk, c.to_string()));
    }
    bail!(
        "unsupported key '{key}': use a single character or one of Enter, Tab, Escape, \
         Backspace, Delete, Space, ArrowLeft, ArrowUp, ArrowRight, ArrowDown, Home, End, \
         PageUp, PageDown"
    )
}

/// Press a key (e.g. "Enter", "Tab", "Backspace", "ArrowDown", "a").
pub async fn press_key(client: &CdpClient, key: &str) -> Result<String> {
    let (key_name, code, vk, text) = key_event(key)?;
    // Chrome only inserts text for "keyDown"; keys without text are "rawKeyDown".
    let down_type = if text.is_empty() {
        "rawKeyDown"
    } else {
        "keyDown"
    };
    let mut down = json!({
        "type": down_type,
        "key": key_name,
        "code": code,
        "windowsVirtualKeyCode": vk,
        "nativeVirtualKeyCode": vk,
    });
    if !text.is_empty() {
        down["text"] = json!(text);
        down["unmodifiedText"] = json!(text);
    }
    client.send("Input.dispatchKeyEvent", down).await?;
    client
        .send(
            "Input.dispatchKeyEvent",
            json!({
                "type": "keyUp",
                "key": key_name,
                "code": code,
                "windowsVirtualKeyCode": vk,
                "nativeVirtualKeyCode": vk,
            }),
        )
        .await?;

    Ok(format!("Pressed key: {key}"))
}

/// Get text content of an element by @ref.
pub async fn get_text(session: &mut BrowserSession, element_ref: &str) -> Result<String> {
    let node_id = session.resolve_ref(element_ref)?;
    let client = session.client()?;
    let resolved = client
        .send("DOM.resolveNode", json!({"backendNodeId": node_id}))
        .await?;
    let object_id = resolved["object"]["objectId"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Could not resolve {element_ref}"))?;
    let result = client.send("Runtime.callFunctionOn", json!({
        "objectId": object_id,
        "functionDeclaration": "function() { return this.innerText || this.textContent || ''; }",
        "returnByValue": true,
    })).await?;
    Ok(result["result"]["value"].as_str().unwrap_or("").to_string())
}

/// Wait for a CSS selector to appear, or timeout.
pub async fn wait_for(client: &CdpClient, condition: &str, timeout_ms: u64) -> Result<String> {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);

    // Serialize the selector as a proper JS string literal — handles quotes,
    // backslashes, </script>, unicode, everything. Prevents injection of
    // arbitrary JS via a malicious selector.
    let selector_js = serde_json::to_string(condition)?;
    let expression = format!("!!document.querySelector({selector_js})");

    loop {
        let result = client
            .send(
                "Runtime.evaluate",
                json!({
                    "expression": expression,
                }),
            )
            .await?;

        if result["result"]["value"].as_bool() == Some(true) {
            return Ok(format!("Condition met: {condition}"));
        }

        if tokio::time::Instant::now() > deadline {
            return Ok(format!(
                "Timeout after {timeout_ms}ms waiting for: {condition}"
            ));
        }

        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn navigation_accepts_http_and_https() {
        assert!(validate_navigation_url("http://example.com").is_ok());
        assert!(validate_navigation_url("https://example.com/path?q=1#x").is_ok());
        assert!(validate_navigation_url("HTTPS://EXAMPLE.COM").is_ok());
        assert!(validate_navigation_url("about:blank").is_ok());
    }

    #[test]
    fn navigation_rejects_dangerous_schemes() {
        for url in [
            "javascript:alert(1)",
            "JavaScript:void(0)",
            "data:text/html,<script>alert(1)</script>",
            "file:///etc/passwd",
            "ftp://example.com",
            "chrome://settings",
            "view-source:http://example.com",
        ] {
            assert!(
                validate_navigation_url(url).is_err(),
                "should have rejected {url}"
            );
        }
    }

    #[test]
    fn navigation_rejects_relative_or_empty() {
        assert!(validate_navigation_url("").is_err());
        assert!(validate_navigation_url("   ").is_err());
        assert!(validate_navigation_url("/foo/bar").is_err());
        assert!(validate_navigation_url("example.com").is_err());
    }
}

#[cfg(test)]
mod preflight_tests {
    use super::preflight_navigation_url;

    #[tokio::test]
    async fn metadata_service_is_refused() {
        let err = preflight_navigation_url("http://169.254.169.254/latest/meta-data/")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("169.254.169.254"), "{err}");
    }

    #[tokio::test]
    async fn local_dev_server_is_allowed() {
        preflight_navigation_url("http://localhost:3000/")
            .await
            .unwrap();
        preflight_navigation_url("http://127.0.0.1:8080/api")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn non_http_schemes_are_still_refused() {
        assert!(
            preflight_navigation_url("file:///etc/passwd")
                .await
                .is_err()
        );
        assert!(
            preflight_navigation_url("javascript:alert(1)")
                .await
                .is_err()
        );
        assert!(preflight_navigation_url("about:blank").await.is_ok());
    }
}

#[cfg(test)]
mod fill_summary_tests {
    use super::fill_summary;

    #[test]
    fn the_typed_value_is_never_echoed() {
        let s = fill_summary("@e3", "hunter2-secret");
        assert!(!s.contains("hunter2"), "{s}");
        assert!(s.contains("@e3"));
        assert!(s.contains("14"), "length is fine to report: {s}");
    }

    #[test]
    fn multibyte_values_do_not_panic() {
        let v = format!("{}🦀", "a".repeat(49));
        let s = fill_summary("@e1", &v);
        assert!(!s.contains('🦀'));
    }
}

#[cfg(test)]
mod landed_url_tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// CDP endpoint that reports `href` as the page location and records the
    /// URL of every Page.navigate it receives.
    async fn fake_cdp(href: &'static str) -> (String, Arc<Mutex<Vec<String>>>) {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message;
        let navigations = Arc::new(Mutex::new(Vec::new()));
        let seen = navigations.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
            while let Some(Ok(Message::Text(t))) = ws.next().await {
                let cmd: serde_json::Value = serde_json::from_str(&t).unwrap();
                if cmd["method"] == "Page.navigate" {
                    let url = cmd["params"]["url"].as_str().unwrap_or("").to_string();
                    seen.lock().unwrap().push(url);
                }
                let result = if cmd["method"] == "Runtime.evaluate" {
                    json!({"result": {"type": "string", "value": href}})
                } else {
                    json!({})
                };
                let reply = json!({"id": cmd["id"], "result": result}).to_string();
                if ws.send(Message::Text(reply.into())).await.is_err() {
                    break;
                }
            }
        });
        (format!("ws://{addr}"), navigations)
    }

    /// Only the URL handed to browser_navigate was checked, so a page that
    /// redirected (or was clicked) to the metadata service was read freely.
    #[tokio::test]
    async fn a_page_that_landed_on_the_metadata_service_is_blanked() {
        let (ws, navs) = fake_cdp("http://169.254.169.254/latest/meta-data/iam/").await;
        let client = CdpClient::connect(&ws).await.unwrap();
        let err = ensure_page_allowed(&client).await.unwrap_err().to_string();
        assert!(err.contains("169.254.169.254"), "{err}");
        assert_eq!(*navs.lock().unwrap(), vec!["about:blank".to_string()]);
    }

    #[tokio::test]
    async fn local_dev_servers_and_chrome_pages_stay_readable() {
        for href in [
            "http://127.0.0.1:3000/",
            "about:blank",
            "chrome-error://chromewebdata/",
        ] {
            let (ws, navs) = fake_cdp(href).await;
            let client = CdpClient::connect(&ws).await.unwrap();
            ensure_page_allowed(&client).await.unwrap();
            assert!(navs.lock().unwrap().is_empty(), "{href}");
        }
    }
}

#[cfg(test)]
mod cdp_request_tests {
    use super::*;
    use serde_json::Value;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    type Log = Arc<Mutex<Vec<(String, Value)>>>;

    /// CDP endpoint that answers every command with `reply(method, params)`
    /// (a reply holding an "error" key is sent as a CDP error) and records
    /// each command it receives. It never emits events.
    async fn scripted_cdp(reply: fn(&str, &Value) -> Value) -> (String, Log) {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message;
        let log: Log = Arc::default();
        let seen = log.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
            while let Some(Ok(Message::Text(t))) = ws.next().await {
                let cmd: Value = serde_json::from_str(&t).unwrap();
                let method = cmd["method"].as_str().unwrap_or("").to_string();
                let r = reply(&method, &cmd["params"]);
                seen.lock().unwrap().push((method, cmd["params"].clone()));
                let msg = match r.get("error") {
                    Some(e) => json!({"id": cmd["id"], "error": e}),
                    None => json!({"id": cmd["id"], "result": r}),
                };
                if ws
                    .send(Message::Text(msg.to_string().into()))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });
        (format!("ws://{addr}"), log)
    }

    fn sent(log: &Log, method: &str) -> Vec<Value> {
        log.lock()
            .unwrap()
            .iter()
            .filter(|(m, _)| m == method)
            .map(|(_, p)| p.clone())
            .collect()
    }

    fn page(method: &str, _: &Value) -> Value {
        match method {
            "Runtime.evaluate" => {
                json!({"result": {"type": "string", "value": "http://127.0.0.1:3000/#/settings"}})
            }
            _ => json!({}),
        }
    }

    /// Chrome answers a fragment-only navigation without a loaderId and
    /// never fires Page.loadEventFired; navigate waited out the timeout.
    #[tokio::test]
    async fn a_same_document_navigation_does_not_wait_for_a_load_event() {
        fn reply(method: &str, params: &Value) -> Value {
            match method {
                "Page.navigate" => json!({"frameId": "F1"}),
                _ => page(method, params),
            }
        }
        let (ws, _) = scripted_cdp(reply).await;
        let client = CdpClient::connect(&ws).await.unwrap();
        let res = tokio::time::timeout(
            Duration::from_millis(2_000),
            navigate(&client, "http://127.0.0.1:3000/#/settings", 5_000),
        )
        .await
        .expect("navigate waited for a load event that never comes");
        res.unwrap();
    }

    #[tokio::test]
    async fn a_new_document_still_waits_for_its_load_event() {
        fn reply(method: &str, params: &Value) -> Value {
            match method {
                "Page.navigate" => json!({"frameId": "F1", "loaderId": "L1"}),
                _ => page(method, params),
            }
        }
        let (ws, _) = scripted_cdp(reply).await;
        let client = CdpClient::connect(&ws).await.unwrap();
        let err = navigate(&client, "http://127.0.0.1:3000/other", 200)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("timed out"), "{err}");
    }

    #[tokio::test]
    async fn full_page_screenshots_capture_beyond_the_viewport() {
        fn reply(method: &str, _: &Value) -> Value {
            match method {
                "Page.getLayoutMetrics" => {
                    json!({"cssContentSize": {"width": 800.0, "height": 3500.0}})
                }
                "Page.captureScreenshot" => json!({"data": "iVBOR"}),
                _ => json!({}),
            }
        }
        let (ws, log) = scripted_cdp(reply).await;
        let client = CdpClient::connect(&ws).await.unwrap();
        assert_eq!(screenshot(&client, true).await.unwrap(), "iVBOR");
        let shot = sent(&log, "Page.captureScreenshot").remove(0);
        assert_eq!(shot["captureBeyondViewport"], true);
        assert_eq!(shot["clip"]["height"], 3500.0);

        screenshot(&client, false).await.unwrap();
        let shot = sent(&log, "Page.captureScreenshot").remove(1);
        assert!(shot.get("clip").is_none() && shot.get("captureBeyondViewport").is_none());
    }

    #[test]
    fn named_keys_carry_virtual_key_codes() {
        let k = |s: &str| key_event(s).unwrap();
        assert_eq!(
            k("Backspace"),
            ("Backspace".into(), "Backspace".into(), 8, "".into())
        );
        assert_eq!(
            k("enter"),
            ("Enter".into(), "Enter".into(), 13, "\r".into())
        );
        assert_eq!(k("ArrowDown").2, 40);
        assert_eq!(k("Home"), ("Home".into(), "Home".into(), 36, "".into()));
        assert_eq!(k("Space"), (" ".into(), "Space".into(), 32, " ".into()));
        assert_eq!(k("a"), ("a".into(), "KeyA".into(), 65, "a".into()));
        assert_eq!(k("7"), ("7".into(), "Digit7".into(), 55, "7".into()));
        assert_eq!(k("é").3, "é");
    }

    /// Unknown names were sent as text: Chrome rejected long ones and typed
    /// short ones; a raw "\r" submitted a form past the gate's Enter check.
    #[test]
    fn unknown_names_and_control_characters_are_refused() {
        for key in ["Foo", "F5", "\r", "\n", "\t", ""] {
            assert!(key_event(key).is_err(), "{key:?}");
        }
    }

    #[tokio::test]
    async fn backspace_is_sent_as_a_raw_key_down_with_its_key_code() {
        let (ws, log) = scripted_cdp(|_, _| json!({})).await;
        let client = CdpClient::connect(&ws).await.unwrap();
        press_key(&client, "Backspace").await.unwrap();
        let events = sent(&log, "Input.dispatchKeyEvent");
        assert_eq!(events.len(), 2);
        assert_eq!(events[0]["type"], "rawKeyDown");
        assert_eq!(events[0]["windowsVirtualKeyCode"], 8);
        assert!(events[0].get("text").is_none());
        assert_eq!(events[1]["type"], "keyUp");
        assert_eq!(events[1]["windowsVirtualKeyCode"], 8);

        press_key(&client, "x").await.unwrap();
        let events = sent(&log, "Input.dispatchKeyEvent");
        assert_eq!(events[2]["type"], "keyDown");
        assert_eq!(events[2]["text"], "x");
        assert_eq!(events[2]["windowsVirtualKeyCode"], 88);
    }
}
