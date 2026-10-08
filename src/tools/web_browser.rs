/// WebBrowserTool — port of webBrowser.ts
/// Fetches a URL using headless Chromium (if available) or falls back to reqwest.
/// Returns rendered DOM text content, stripped of scripts and styles.
use super::{Tool, ToolContext, ToolOutput, async_trait};
use crate::net_policy::NetPolicy;
use anyhow::Result;
use serde::Deserialize;
use serde_json::json;

pub struct WebBrowserTool {
    /// Which destinations this tool may reach. See `net_policy`.
    pub policy: NetPolicy,
    /// `browserChromePath`, tried before the auto-detected browser.
    pub chrome_path: Option<String>,
}

#[derive(Deserialize)]
struct Input {
    url: String,
    /// Max characters to return (default 50 000)
    #[serde(default = "default_max_chars")]
    max_chars: usize,
}

fn default_max_chars() -> usize {
    50_000
}

/// Raw response cap for the plain-HTTP fallback.
const MAX_RESPONSE_BYTES: usize = 5 * 1024 * 1024;
const FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

#[async_trait]
impl Tool for WebBrowserTool {
    fn name(&self) -> &str {
        "WebBrowser"
    }

    fn description(&self) -> &str {
        "Open a URL in a headless browser and return the rendered page text. \
        Uses Chromium if installed; falls back to a plain HTTP fetch. \
        Better than WebFetch for JavaScript-heavy pages."
    }

    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "url": {
                    "type": "string",
                    "description": "The URL to open"
                },
                "max_chars": {
                    "type": "integer",
                    "description": "Maximum characters of content to return (default 50 000)",
                    "default": 50000,
                    "minimum": 1000,
                    "maximum": 200000
                }
            },
            "required": ["url"]
        })
    }

    async fn execute(&self, input: serde_json::Value, _ctx: &ToolContext) -> Result<ToolOutput> {
        let input: Input = serde_json::from_value(input)?;
        let max_chars = input.max_chars;

        // Parse + policy check *before* anything touches the URL: chromium
        // would print `file:///etc/passwd` on request, and a flag-shaped
        // string would be parsed as a switch.
        let url = match url::Url::parse(&input.url) {
            Ok(u) => u,
            Err(e) => return Ok(ToolOutput::error(format!("Invalid URL: {e}"))),
        };
        // The same check browser_navigate makes, so a name only HTTP(S)_PROXY
        // can resolve passes here as it does in the proxy and the fallback
        // fetch. This tool has no prompt to grant loopback, so any loopback
        // address the policy needs a grant for is refused.
        match self
            .policy
            .check_browser_url(&url, &crate::net_policy::LoopbackGrants::default())
            .await
        {
            Err(e) => return Ok(ToolOutput::error(format!("WebBrowser refused: {e}"))),
            Ok(need) if !need.is_empty() => {
                return Ok(ToolOutput::error(format!(
                    "WebBrowser refused: destination {} is a private or loopback address; \
                     set allowPrivateNetworkFetch: true to permit it",
                    need[0].ip()
                )));
            }
            Ok(_) => {}
        }

        // Try headless Chromium first. It follows redirects, meta refresh and
        // JS navigation on its own, so every connection it makes goes through
        // a proxy that re-applies the policy.
        if let Some(text) = try_chromium(
            url.as_str(),
            &self.policy,
            self.chrome_path.as_deref(),
            max_chars,
        )
        .await
        {
            return Ok(ToolOutput::success(text));
        }

        // Fallback: guarded plain fetch (every hop checked, body capped).
        match fetch_plain(url.as_str(), &self.policy, max_chars).await {
            Ok(text) => Ok(ToolOutput::success(text)),
            Err(e) => Ok(ToolOutput::error(format!("WebBrowser fetch failed: {e}"))),
        }
    }
}

/// Try to fetch via `chromium --headless --dump-dom`, with all of its
/// traffic forced through a policy-enforcing proxy.
async fn try_chromium(
    url: &str,
    policy: &NetPolicy,
    chrome_path: Option<&str>,
    max_chars: usize,
) -> Option<String> {
    use tokio::process::Command;
    use tokio::time::{Duration, timeout};

    // Without the proxy Chromium would reach whatever a redirect names, so
    // no proxy means no Chromium; the guarded plain fetch takes over.
    // Both live until this returns, timeout included.
    // As root Chromium only starts with its sandbox off, and these pages are
    // model-chosen (often from prompt-injected content): never run them
    // unsandboxed as root unless the user opted in. The plain fetch, which
    // runs no page JavaScript, takes over.
    let no_sandbox = crate::browser::chrome_no_sandbox();
    if crate::browser::runs_as_root() && !no_sandbox {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            tracing::debug!(
                "WebBrowser JS rendering disabled as root (set {}=1 to allow)",
                crate::browser::NO_SANDBOX_ENV
            )
        });
        return None;
    }
    let proxy = crate::net_policy::spawn_policy_proxy(*policy).await.ok()?;
    let profile = tempfile::tempdir().ok()?;
    let args = chromium_args(proxy.addr, profile.path(), url, no_sandbox);

    for exe in chromium_candidates(chrome_path, crate::browser::find_chrome()) {
        let result = timeout(
            Duration::from_secs(20),
            Command::new(exe)
                .args(&args)
                // A timeout drops this future; without this the browser
                // would outlive the tool call as an orphan.
                .kill_on_drop(true)
                .output(),
        )
        .await;

        match result {
            Ok(Ok(output)) if output.status.success() => {
                let raw = String::from_utf8_lossy(&output.stdout);
                // A refused or failed navigation still exits 0 and dumps
                // Chromium's own error page; that is no answer, and every
                // installed browser would hit the same wall. The guarded
                // fetch reports the real reason instead.
                if is_chromium_error_page(&raw) {
                    return None;
                }
                let stripped = strip_html(raw.as_ref(), max_chars);
                if !stripped.trim().is_empty() {
                    return Some(stripped);
                }
            }
            _ => continue,
        }
    }
    None
}

/// Browsers to try, in order: the configured one, the one /browse would
/// find (macOS app bundles and Windows install dirs are not on PATH), then
/// the common Linux names.
fn chromium_candidates(
    configured: Option<&str>,
    found: Option<std::path::PathBuf>,
) -> Vec<std::path::PathBuf> {
    let mut out: Vec<std::path::PathBuf> = configured.map(Into::into).into_iter().collect();
    out.extend(found);
    for name in [
        "chromium",
        "chromium-browser",
        "google-chrome",
        "google-chrome-stable",
    ] {
        let name = std::path::PathBuf::from(name);
        if !out.contains(&name) {
            out.push(name);
        }
    }
    out
}

/// Chromium's net-error interstitial (`chrome-error://chromewebdata/`).
fn is_chromium_error_page(dom: &str) -> bool {
    dom.contains("id=\"main-frame-error\"") && dom.contains("neterror")
}

fn chromium_args(
    proxy: std::net::SocketAddr,
    profile: &std::path::Path,
    url: &str,
    no_sandbox: bool,
) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "--headless".into(),
        "--disable-gpu".into(),
        "--no-first-run".into(),
        "--disable-background-networking".into(),
        "--disable-component-update".into(),
        "--disable-sync".into(),
        format!("--user-data-dir={}", profile.display()),
    ];
    args.extend(crate::net_policy::chromium_proxy_args(proxy));
    // Only as root with OXIDECLAW_BROWSER_NO_SANDBOX=1 (see try_chromium).
    if no_sandbox {
        args.push("--no-sandbox".into());
    }
    args.extend(["--dump-dom".into(), url.into()]);
    args
}

/// Plain HTTP fetch as fallback.
async fn fetch_plain(url: &str, policy: &NetPolicy, max_chars: usize) -> Result<String> {
    // WebBrowser has no domain rules to re-check, and Chromium follows
    // redirects itself on the main path.
    let fetched =
        crate::net_policy::fetch(url, policy, MAX_RESPONSE_BYTES, FETCH_TIMEOUT, true).await?;
    if !fetched.status.is_success() {
        anyhow::bail!("HTTP {}", fetched.status);
    }
    Ok(strip_html(
        &crate::net_policy::decode_body(&fetched.content_type, &fetched.body),
        max_chars,
    ))
}

/// Very lightweight HTML → plain-text extractor.
fn strip_html(html: &str, max_chars: usize) -> String {
    let mut out = String::with_capacity(html.len().min(max_chars + 1024));
    let mut in_tag = false;
    let mut in_script = false;
    let mut in_style = false;
    let mut tag_buf = String::new();
    let mut prev_space = false;

    for c in html.chars() {
        if in_tag {
            tag_buf.push(c);
            if c == '>' {
                in_tag = false;
                let tag_lower = tag_buf.to_lowercase();
                if tag_lower.contains("<script") {
                    in_script = true;
                } else if tag_lower.contains("</script") {
                    in_script = false;
                } else if tag_lower.contains("<style") {
                    in_style = true;
                } else if tag_lower.contains("</style") {
                    in_style = false;
                }
                // Add space after block-level tags
                let is_block = [
                    "</p", "<br", "</div", "</h1", "</h2", "</h3", "</h4", "</h5", "</h6", "<li",
                    "</li",
                ]
                .iter()
                .any(|t| tag_lower.starts_with(t));
                if is_block && !prev_space {
                    out.push('\n');
                    prev_space = true;
                }
                tag_buf.clear();
            }
        } else if c == '<' {
            in_tag = true;
            tag_buf.push(c);
        } else if !in_script && !in_style {
            if c.is_whitespace() {
                if !prev_space {
                    out.push(' ');
                    prev_space = true;
                }
            } else {
                out.push(c);
                prev_space = false;
            }
        }

        if out.len() >= max_chars {
            out.push_str("\n[content truncated]");
            break;
        }
    }

    // Decode common HTML entities
    out.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::types::ToolResultContent;
    use crate::net_policy::test_support::{ok_with, scripted_server};
    use std::sync::atomic::Ordering;

    async fn run(policy: NetPolicy, url: &str) -> ToolOutput {
        let tool = WebBrowserTool {
            policy,
            chrome_path: None,
        };
        let ctx = ToolContext::new(std::env::temp_dir());
        tool.execute(json!({"url": url}), &ctx)
            .await
            .expect("refusals are tool errors, not Err")
    }

    fn text(o: &ToolOutput) -> String {
        o.content
            .iter()
            .map(|c| match c {
                ToolResultContent::Text { text } => text.as_str(),
            })
            .collect()
    }

    /// Only the four Linux names were tried, so Chrome in a macOS app bundle
    /// or Windows' Program Files (and `browserChromePath`) went unused and
    /// pages were fetched without JavaScript.
    #[test]
    fn chromium_candidates_put_the_configured_and_found_browser_first() {
        let mac = std::path::PathBuf::from(
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        );
        let got = chromium_candidates(Some("/opt/chrome/chrome"), Some(mac.clone()));
        assert_eq!(got[0], std::path::PathBuf::from("/opt/chrome/chrome"));
        assert_eq!(got[1], mac);
        assert!(got.contains(&std::path::PathBuf::from("chromium")));
        let got = chromium_candidates(None, None);
        assert_eq!(got.len(), 4, "{got:?}");
    }

    /// Chromium used to run with no proxy, so a redirect it followed was
    /// never policy-checked. Every connection must go through the proxy,
    /// loopback included (Chromium bypasses proxies for localhost by default).
    #[test]
    fn chromium_is_pinned_to_the_policy_proxy() {
        let proxy: std::net::SocketAddr = "127.0.0.1:4242".parse().unwrap();
        let args = chromium_args(
            proxy,
            std::path::Path::new("/p"),
            "https://e.example/",
            false,
        );
        assert!(!args.contains(&"--no-sandbox".to_string()));
        assert!(args.contains(&"--proxy-server=http://127.0.0.1:4242".to_string()));
        assert!(args.contains(&"--proxy-bypass-list=<-loopback>".to_string()));
        assert!(args.contains(&"--user-data-dir=/p".to_string()));
        assert_eq!(args.last().unwrap(), "https://e.example/");
    }

    /// A navigation the proxy refused used to come back as success, with
    /// "This site can't be reached" as the page text.
    #[test]
    fn chromium_error_pages_are_not_page_content() {
        let interstitial = r#"<html dir="ltr" lang="en"><body class="neterror" id="t">
            <div id="main-frame-error" class="interstitial-wrapper"><h1>This site can't be reached</h1>
            <div class="error-code">ERR_TUNNEL_CONNECTION_FAILED</div></div></body></html>"#;
        assert!(is_chromium_error_page(interstitial));
        assert!(!is_chromium_error_page(
            "<html><body><p>neterror is a word here</p></body></html>"
        ));
    }

    /// `chromium --dump-dom file:///etc/passwd` would happily print the
    /// file. The URL must be refused before any process is spawned.
    #[tokio::test]
    async fn file_url_is_refused() {
        let out = run(NetPolicy::LOCAL_OK, "file:///etc/passwd").await;
        assert!(out.is_error);
        assert!(text(&out).contains("http/https"), "{}", text(&out));
    }

    /// A flag-shaped "URL" would be parsed by chromium as a switch.
    #[tokio::test]
    async fn flag_shaped_url_is_refused() {
        let out = run(NetPolicy::LOCAL_OK, "--remote-debugging-port=9222").await;
        assert!(out.is_error);
        assert!(
            text(&out).to_lowercase().contains("invalid url"),
            "{}",
            text(&out)
        );
    }

    #[tokio::test]
    async fn strict_policy_refuses_loopback_without_connecting() {
        let (base, hits) = scripted_server(vec![ok_with("text/html", "<p>secret</p>")]).await;
        let out = run(NetPolicy::STRICT, &base).await;
        assert!(out.is_error);
        assert!(text(&out).contains("private"), "{}", text(&out));
        assert_eq!(hits.load(Ordering::SeqCst), 0);
    }

    /// The plain-HTTP fallback goes through the same guarded fetch.
    #[tokio::test]
    async fn fallback_fetch_is_capped() {
        let resp = "HTTP/1.1 200 OK\r\ncontent-type: text/html\r\n\
                    content-length: 99999999\r\nconnection: close\r\n\r\nx";
        let (base, _) = scripted_server(vec![resp.to_string()]).await;
        let err = fetch_plain(&base, &NetPolicy::LOCAL_OK, 1000)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("too large"), "{err}");
    }

    #[tokio::test]
    async fn fallback_fetch_strips_html() {
        let (base, _) = scripted_server(vec![ok_with(
            "text/html",
            "<h1>Hi</h1><script>x()</script>",
        )])
        .await;
        let got = fetch_plain(&base, &NetPolicy::LOCAL_OK, 1000)
            .await
            .unwrap();
        assert!(got.contains("Hi"), "{got}");
        assert!(!got.contains("x()"), "{got}");
    }

    #[test]
    fn strip_html_does_not_scan_past_the_cap() {
        let html = format!("<p>{}</p>", "a".repeat(100_000));
        let out = strip_html(&html, 10);
        assert!(out.len() < 100, "{}", out.len());
        assert!(out.contains("[content truncated]"));
    }
}
