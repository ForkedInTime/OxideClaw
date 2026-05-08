//! Browser action functions: navigate, click, fill, screenshot, etc.
//!
//! Functions that only need the CDP connection take `&CdpClient` so callers
//! can clone the client out from under a session lock and drop the lock
//! before a long-running operation (page load, wait_for polling, etc.).
//! Functions that need the ref map (click / fill / get_text) still take
//! `&mut BrowserSession` — those are fast, no long awaits.
use super::cdp::CdpClient;
use super::BrowserSession;
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
        Some((s, _)) if !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '-' || c == '.') => s.to_string(),
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

/// Navigate to a URL. Returns (title, status). Does NOT mutate session state —
/// the caller is responsible for updating `current_url` / `current_title`
/// after this returns, so the session lock can be released while we wait on
/// the page load event (bounded by `timeout_ms`).
pub async fn navigate(client: &CdpClient, url: &str, timeout_ms: u64) -> Result<(String, u16)> {
    validate_navigation_url(url)?;
    // Subscribe BEFORE navigating so we don't miss Page.loadEventFired on fast loads.
    let mut events = client.subscribe();

    let result = client.send("Page.navigate", json!({"url": url})).await?;
    if let Some(err) = result["errorText"].as_str()
        && !err.is_empty() {
            anyhow::bail!("Navigation failed: {err}");
        }

    // Wait for load event
    let deadline =
        tokio::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
    loop {
        match tokio::time::timeout_at(deadline, events.recv()).await {
            Ok(Ok(ev)) if ev.method == "Page.loadEventFired" => break,
            Ok(Ok(_)) => continue,
            Ok(Err(_)) => break, // Channel lagged, page likely loaded
            Err(_) => anyhow::bail!("Page load timed out after {timeout_ms}ms"),
        }
    }

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
    let resolved = client.send("DOM.resolveNode", json!({"backendNodeId": node_id})).await?;
    let object_id = resolved["object"]["objectId"].as_str()
        .ok_or_else(|| anyhow::anyhow!("Could not resolve element {element_ref} to JS object"))?;

    // Scroll into view
    let _ = client.send("Runtime.callFunctionOn", json!({
        "objectId": object_id,
        "functionDeclaration": "function() { this.scrollIntoViewIfNeeded(); }",
    })).await;

    // Get element center coordinates
    let box_model = client.send("DOM.getBoxModel", json!({"backendNodeId": node_id})).await?;
    let content = &box_model["model"]["content"];
    if let Some(coords) = content.as_array()
        && coords.len() >= 4 {
            let x = (coords[0].as_f64().unwrap_or(0.0) + coords[2].as_f64().unwrap_or(0.0)) / 2.0;
            let y = (coords[1].as_f64().unwrap_or(0.0) + coords[5].as_f64().unwrap_or(0.0)) / 2.0;

            // Mouse click sequence
            for event_type in ["mousePressed", "mouseReleased"] {
                client.send("Input.dispatchMouseEvent", json!({
                    "type": event_type,
                    "x": x,
                    "y": y,
                    "button": "left",
                    "clickCount": 1,
                })).await?;
            }
            return Ok(format!("Clicked {element_ref} at ({x:.0}, {y:.0})"));
        }

    // Fallback: JS click
    client.send("Runtime.callFunctionOn", json!({
        "objectId": object_id,
        "functionDeclaration": "function() { this.click(); }",
    })).await?;
    Ok(format!("Clicked {element_ref} (JS fallback)"))
}

/// Fill a text input by @ref.
pub async fn fill(session: &mut BrowserSession, element_ref: &str, value: &str) -> Result<String> {
    let node_id = session.resolve_ref(element_ref)?;
    let client = session.client()?;

    // Focus the element
    client.send("DOM.focus", json!({"backendNodeId": node_id})).await?;

    // Clear existing value by calling .value = '' on the resolved element directly
    // (not on document.activeElement, which could be anything after focus changes).
    let resolved = client.send("DOM.resolveNode", json!({"backendNodeId": node_id})).await?;
    if let Some(object_id) = resolved["object"]["objectId"].as_str() {
        let _ = client.send("Runtime.callFunctionOn", json!({
            "objectId": object_id,
            "functionDeclaration":
                "function() { if ('value' in this) this.value = ''; \
                              else if (this.isContentEditable) this.textContent = ''; }",
        })).await;
    }

    // Type the value (handles input events correctly)
    client.send("Input.insertText", json!({"text": value})).await?;

    Ok(format!("Filled {element_ref} with \"{}\"", if value.len() > 50 {
        format!("{}...", &value[..50])
    } else {
        value.to_string()
    }))
}

/// Take a screenshot. Returns base64-encoded PNG.
pub async fn screenshot(client: &CdpClient, full_page: bool) -> Result<String> {
    let mut params = json!({"format": "png"});
    if full_page {
        let metrics = client.send("Page.getLayoutMetrics", json!({})).await?;
        let width = metrics["cssContentSize"]["width"].as_f64().unwrap_or(1280.0);
        let height = metrics["cssContentSize"]["height"].as_f64().unwrap_or(720.0);
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

    client.send("Input.dispatchKeyEvent", json!({
        "type": "keyDown",
        "key": key_code,
        "text": text,
    })).await?;
    client.send("Input.dispatchKeyEvent", json!({
        "type": "keyUp",
        "key": key_code,
    })).await?;

    Ok(format!("Pressed key: {key}"))
}

/// Get text content of an element by @ref.
pub async fn get_text(session: &mut BrowserSession, element_ref: &str) -> Result<String> {
    let node_id = session.resolve_ref(element_ref)?;
    let client = session.client()?;
    let resolved = client.send("DOM.resolveNode", json!({"backendNodeId": node_id})).await?;
    let object_id = resolved["object"]["objectId"].as_str()
        .ok_or_else(|| anyhow::anyhow!("Could not resolve {element_ref}"))?;
    let result = client.send("Runtime.callFunctionOn", json!({
        "objectId": object_id,
        "functionDeclaration": "function() { return this.innerText || this.textContent || ''; }",
        "returnByValue": true,
    })).await?;
    Ok(result["result"]["value"].as_str().unwrap_or("").to_string())
}

/// Wait for a CSS selector to appear, or timeout.
pub async fn wait_for(
    client: &CdpClient,
    condition: &str,
    timeout_ms: u64,
) -> Result<String> {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);

    // Serialize the selector as a proper JS string literal — handles quotes,
    // backslashes, </script>, unicode, everything. Prevents injection of
    // arbitrary JS via a malicious selector.
    let selector_js = serde_json::to_string(condition)?;
    let expression = format!("!!document.querySelector({selector_js})");

    loop {
        let result = client.send("Runtime.evaluate", json!({
            "expression": expression,
        })).await?;

        if result["result"]["value"].as_bool() == Some(true) {
            return Ok(format!("Condition met: {condition}"));
        }

        if tokio::time::Instant::now() > deadline {
            return Ok(format!("Timeout after {timeout_ms}ms waiting for: {condition}"));
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
