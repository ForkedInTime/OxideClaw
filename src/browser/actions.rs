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

    // Wait for load event
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
    loop {
        match tokio::time::timeout_at(deadline, events.recv()).await {
            Ok(Ok(ev)) if ev.method == "Page.loadEventFired" => break,
            Ok(Ok(_)) => continue,
            Ok(Err(_)) => break, // Channel lagged, page likely loaded
            Err(_) => anyhow::bail!("Page load timed out after {timeout_ms}ms"),
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
    }
    let result = client.send("Page.captureScreenshot", params).await?;
    let data = result["data"].as_str().unwrap_or("").to_string();
    Ok(data)
}

/// Press a key (e.g. "Enter", "Tab", "Escape", "a").
pub async fn press_key(client: &CdpClient, key: &str) -> Result<String> {
    let key_lower = key.to_lowercase();
    let (key_code, text) = match key_lower.as_str() {
        "enter" | "return" => ("Enter", "\r"),
        "tab" => ("Tab", "\t"),
        "escape" | "esc" => ("Escape", ""),
        "backspace" => ("Backspace", ""),
        "space" => (" ", " "),
        _ => (key, key),
    };

    client
        .send(
            "Input.dispatchKeyEvent",
            json!({
                "type": "keyDown",
                "key": key_code,
                "text": text,
            }),
        )
        .await?;
    client
        .send(
            "Input.dispatchKeyEvent",
            json!({
                "type": "keyUp",
                "key": key_code,
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
