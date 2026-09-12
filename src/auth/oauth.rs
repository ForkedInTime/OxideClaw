//! Console OAuth for Anthropic: PKCE login, code exchange, refresh.
//! Parameters mirror the open-source `ant` CLI (`pkg/cmd/cmd_auth.go`).

#![allow(dead_code)] // Task 4 and lib consumers will use these items

use super::profile::ProfileCredentials;
use anyhow::{Context, Result, anyhow};
use base64::Engine as _;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub const CLIENT_ID: &str = "41077d10-94b8-4194-be48-d251e9eb21b4";
pub const CONSOLE_URL: &str = "https://platform.claude.com";
pub const API_BASE: &str = "https://api.anthropic.com";
pub const SCOPE: &str = "user:profile user:inference user:developer";
pub use super::OAUTH_BETA;
/// Refresh when fewer than this many seconds remain (matches `ant`).
pub const REFRESH_THRESHOLD_SECS: i64 = 120;

/// `ANTHROPIC_OAUTH_CLIENT_ID` overrides the default, as in the `ant` CLI.
pub fn client_id() -> String {
    client_id_from(std::env::var("ANTHROPIC_OAUTH_CLIENT_ID").ok())
}

pub(crate) fn client_id_from(env: Option<String>) -> String {
    env.map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| CLIENT_ID.to_string())
}

pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[derive(Debug, Clone, Deserialize)]
pub struct TokenOrg {
    #[serde(default)]
    pub uuid: String,
    #[serde(default)]
    pub name: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TokenAccount {
    #[serde(default)]
    pub uuid: String,
    #[serde(default)]
    pub email_address: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TokenWorkspace {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
}

/// `/v1/oauth/token` response for both grants.
#[derive(Debug, Clone, Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub expires_in: i64,
    #[serde(default)]
    pub scope: Option<String>,
    #[serde(default)]
    pub organization: Option<TokenOrg>,
    #[serde(default)]
    pub account: Option<TokenAccount>,
    #[serde(default)]
    pub workspace: Option<TokenWorkspace>,
}

fn non_empty(s: String) -> Option<String> {
    if s.is_empty() { None } else { Some(s) }
}

impl TokenResponse {
    pub fn into_credentials(self, now: i64) -> ProfileCredentials {
        let mut c = ProfileCredentials::new(
            &self.access_token,
            self.refresh_token.filter(|r| !r.is_empty()),
            Some(now + self.expires_in),
        );
        c.scope = self.scope.filter(|s| !s.is_empty());
        if let Some(o) = self.organization {
            c.organization_uuid = non_empty(o.uuid);
            c.organization_name = non_empty(o.name);
        }
        if let Some(a) = self.account {
            c.account_email = non_empty(a.email_address);
        }
        if let Some(w) = self.workspace {
            c.workspace_id = non_empty(w.id);
            c.workspace_name = non_empty(w.name);
        }
        c
    }
}

pub fn needs_refresh(expires_at: Option<i64>, now: i64) -> bool {
    match expires_at {
        None => true,
        Some(exp) => exp - now < REFRESH_THRESHOLD_SECS,
    }
}

fn http() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .context("build OAuth HTTP client")
}

/// Redeem a refresh token. JSON body and the oauth beta header, as `ant` does.
pub async fn refresh_access_token(
    base_url: &str,
    client_id: &str,
    refresh_token: &str,
) -> Result<TokenResponse> {
    let resp = http()?
        .post(format!("{}/v1/oauth/token", base_url.trim_end_matches('/')))
        .header("anthropic-beta", OAUTH_BETA)
        .json(&serde_json::json!({
            "grant_type": "refresh_token",
            "refresh_token": refresh_token,
            "client_id": client_id,
        }))
        .send()
        .await
        .context("token refresh request")?;
    parse_token_response(resp, "refresh").await
}

pub(crate) async fn parse_token_response(
    resp: reqwest::Response,
    what: &str,
) -> Result<TokenResponse> {
    let status = resp.status();
    let request_id = resp
        .headers()
        .get("request-id")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("-")
        .to_string();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(anyhow!(
            "OAuth {what} failed: {status} (request_id={request_id}): {}",
            body.trim()
        ));
    }
    let tok: TokenResponse =
        serde_json::from_str(&body).with_context(|| format!("parse OAuth {what} response"))?;
    if tok.access_token.is_empty() {
        return Err(anyhow!(
            "OAuth {what} returned 200 with an empty access_token (request_id={request_id})"
        ));
    }
    Ok(tok)
}

fn random_url_safe(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    rand::fill(&mut buf[..]);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(buf)
}

/// 32 random bytes, base64url without padding (43 chars) — RFC 7636 §4.1.
pub fn pkce_verifier() -> String {
    random_url_safe(32)
}

/// `BASE64URL(SHA256(verifier))` — RFC 7636 §4.2.
pub fn pkce_challenge_s256(verifier: &str) -> String {
    let digest = Sha256::digest(verifier.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest)
}

pub fn random_state() -> String {
    random_url_safe(32)
}

pub struct AuthorizeParams<'a> {
    pub console_url: &'a str,
    pub client_id: &'a str,
    pub redirect_uri: &'a str,
    pub scope: &'a str,
    pub state: &'a str,
    pub code_challenge: &'a str,
    /// Console auto-selects this org (`?orgUUID=`) when the account belongs to it.
    pub org_uuid: Option<&'a str>,
    /// Omitted → Console shows its workspace picker.
    pub workspace_id: Option<&'a str>,
}

pub fn build_authorize_url(p: &AuthorizeParams) -> String {
    let mut u = url::Url::parse(&format!(
        "{}/oauth/authorize",
        p.console_url.trim_end_matches('/')
    ))
    .expect("static authorize path");
    {
        let mut q = u.query_pairs_mut();
        q.append_pair("client_id", p.client_id)
            .append_pair("redirect_uri", p.redirect_uri)
            .append_pair("response_type", "code")
            .append_pair("scope", p.scope)
            .append_pair("state", p.state)
            .append_pair("code_challenge", p.code_challenge)
            .append_pair("code_challenge_method", "S256");
        if let Some(w) = p.workspace_id.filter(|w| !w.is_empty()) {
            q.append_pair("workspace_id", w);
        }
        if let Some(o) = p.org_uuid.filter(|o| !o.is_empty()) {
            q.append_pair("orgUUID", o);
        }
    }
    u.to_string()
}

/// Console-hosted "copy this code" page for hosts with no usable localhost.
/// The `app=anthropic-cli` query is part of the client's registered redirect.
pub fn manual_redirect_uri(console_url: &str) -> String {
    format!(
        "{}/oauth/code/callback?app=anthropic-cli",
        console_url.trim_end_matches('/')
    )
}

/// Extract the authorization code from a callback query string, enforcing
/// `state` (CSRF guard) and surfacing the server's `error` when present.
pub fn parse_callback_query(query: &str, expected_state: &str) -> Result<String> {
    let q: std::collections::HashMap<String, String> =
        url::form_urlencoded::parse(query.as_bytes())
            .into_owned()
            .collect();
    if let Some(e) = q.get("error") {
        let desc = q.get("error_description").map(String::as_str).unwrap_or("");
        return Err(anyhow!("authorization denied: {e}: {desc}"));
    }
    if q.get("state").map(String::as_str) != Some(expected_state) {
        return Err(anyhow!(
            "state mismatch in OAuth callback (possible CSRF) — do not retry in this browser session"
        ));
    }
    match q.get("code") {
        Some(c) if !c.is_empty() => Ok(c.clone()),
        _ => Err(anyhow!("OAuth callback did not include a code")),
    }
}

pub const CALLBACK_TIMEOUT: Duration = Duration::from_secs(300);

/// Bind `127.0.0.1:0`. The redirect URI uses `localhost`, which is what the
/// OAuth client has registered.
pub async fn bind_loopback() -> Result<(tokio::net::TcpListener, String)> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .context("bind OAuth callback listener")?;
    let port = listener.local_addr()?.port();
    Ok((listener, format!("http://localhost:{port}/callback")))
}

fn html_escape(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '&' => "&amp;".to_string(),
            '<' => "&lt;".to_string(),
            '>' => "&gt;".to_string(),
            '"' => "&quot;".to_string(),
            '\'' => "&#39;".to_string(),
            c => c.to_string(),
        })
        .collect()
}

fn html_page(status: u16, reason: &str, title: &str, detail: &str) -> String {
    let title = html_escape(title);
    let detail = html_escape(detail);
    let body = format!(
        "<!doctype html><meta charset=utf-8><title>OxideClaw</title>\
         <body style=\"font-family:system-ui;margin:3rem\"><h1>{title}</h1><p>{detail}</p></body>"
    );
    format!(
        "HTTP/1.1 {status} {reason}\r\ncontent-type: text/html; charset=utf-8\r\n\
         content-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
}

/// Serve exactly one HTTP request on `listener`, answer the browser, and
/// return the authorization code. Requests for other paths (favicon, etc.)
/// get a 404 and do not consume the wait.
pub async fn wait_for_code(
    listener: tokio::net::TcpListener,
    expected_state: &str,
    timeout: Duration,
) -> Result<String> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err(anyhow!("timed out waiting for the browser callback"));
        }
        let (mut sock, _) = match tokio::time::timeout(remaining, listener.accept()).await {
            Ok(Ok(conn)) => conn,
            Ok(Err(e)) => return Err(anyhow!("callback listener: {e}")),
            Err(_) => return Err(anyhow!("timed out waiting for the browser callback")),
        };
        let mut buf = vec![0u8; 8192];
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let n = match tokio::time::timeout(remaining, sock.read(&mut buf)).await {
            Ok(Ok(n)) => n,
            Ok(Err(_)) | Err(_) => continue,
        };
        let head = String::from_utf8_lossy(&buf[..n]);
        let request_line = head.lines().next().unwrap_or("");
        let target = request_line.split_whitespace().nth(1).unwrap_or("");
        let (path, query) = target.split_once('?').unwrap_or((target, ""));
        if path != "/callback" {
            let response = html_page(404, "Not Found", "Not found", "");
            let _ =
                tokio::time::timeout(Duration::from_secs(5), sock.write_all(response.as_bytes()))
                    .await;
            let _ = sock.shutdown().await;
            continue;
        }
        let outcome = parse_callback_query(query, expected_state);
        let page = match &outcome {
            Ok(_) => html_page(
                200,
                "OK",
                "Signed in to OxideClaw",
                "You can close this tab and return to the terminal.",
            ),
            Err(e) => html_page(400, "Bad Request", "Sign-in failed", &e.to_string()),
        };
        let _ = tokio::time::timeout(Duration::from_secs(5), sock.write_all(page.as_bytes())).await;
        let _ = sock.shutdown().await;
        return outcome;
    }
}

#[cfg(test)]
mod pkce_tests {
    use super::*;

    #[test]
    fn s256_challenge_matches_rfc_7636_appendix_b() {
        assert_eq!(
            pkce_challenge_s256("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn verifier_and_state_are_url_safe_and_long_enough() {
        for s in [pkce_verifier(), random_state()] {
            assert!(s.len() >= 43, "{s}");
            assert!(
                s.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
                "{s}"
            );
        }
        assert_ne!(pkce_verifier(), pkce_verifier());
    }

    #[test]
    fn authorize_url_carries_every_required_parameter() {
        let p = AuthorizeParams {
            console_url: "https://platform.claude.com",
            client_id: "cid",
            redirect_uri: "http://localhost:4242/callback",
            scope: SCOPE,
            state: "st",
            code_challenge: "ch",
            org_uuid: None,
            workspace_id: None,
        };
        let u = url::Url::parse(&build_authorize_url(&p)).unwrap();
        assert_eq!(u.path(), "/oauth/authorize");
        let q: std::collections::HashMap<_, _> = u.query_pairs().into_owned().collect();
        assert_eq!(q["client_id"], "cid");
        assert_eq!(q["redirect_uri"], "http://localhost:4242/callback");
        assert_eq!(q["response_type"], "code");
        assert_eq!(q["scope"], SCOPE);
        assert_eq!(q["state"], "st");
        assert_eq!(q["code_challenge"], "ch");
        assert_eq!(q["code_challenge_method"], "S256");
        assert!(!q.contains_key("orgUUID") && !q.contains_key("workspace_id"));
    }

    #[test]
    fn authorize_url_adds_org_and_workspace_hints_when_known() {
        let p = AuthorizeParams {
            console_url: "https://platform.claude.com/",
            client_id: "cid",
            redirect_uri: "r",
            scope: SCOPE,
            state: "st",
            code_challenge: "ch",
            org_uuid: Some("org-1"),
            workspace_id: Some("wrkspc_01"),
        };
        let u = url::Url::parse(&build_authorize_url(&p)).unwrap();
        let q: std::collections::HashMap<_, _> = u.query_pairs().into_owned().collect();
        assert_eq!(q["orgUUID"], "org-1");
        assert_eq!(q["workspace_id"], "wrkspc_01");
        assert!(!u.as_str().contains("com//oauth"), "trailing slash trimmed");
    }

    #[test]
    fn manual_redirect_is_the_console_code_page() {
        assert_eq!(
            manual_redirect_uri("https://platform.claude.com/"),
            "https://platform.claude.com/oauth/code/callback?app=anthropic-cli"
        );
    }

    #[test]
    fn callback_query_parsing() {
        assert_eq!(
            parse_callback_query("code=abc&state=st", "st").unwrap(),
            "abc"
        );
        assert_eq!(
            parse_callback_query("state=st&code=a%2Bb", "st").unwrap(),
            "a+b"
        );
        let e = parse_callback_query("code=abc&state=other", "st")
            .unwrap_err()
            .to_string();
        assert!(e.contains("state"), "{e}");
        let e = parse_callback_query("state=st", "st")
            .unwrap_err()
            .to_string();
        assert!(e.contains("code"), "{e}");
        let e = parse_callback_query(
            "error=access_denied&error_description=User%20declined&state=st",
            "st",
        )
        .unwrap_err()
        .to_string();
        assert!(
            e.contains("access_denied") && e.contains("User declined"),
            "{e}"
        );
    }
}

#[cfg(test)]
pub(crate) mod refresh_tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// One-shot HTTP server: records the request, replies with `body`.
    pub(crate) async fn capture_server(
        status_line: &'static str,
        body: &'static str,
    ) -> (String, Arc<tokio::sync::Mutex<String>>, Arc<AtomicUsize>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = Arc::new(tokio::sync::Mutex::new(String::new()));
        let hits = Arc::new(AtomicUsize::new(0));
        let (seen2, hits2) = (seen.clone(), hits.clone());
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                hits2.fetch_add(1, Ordering::SeqCst);
                let mut buf = vec![0u8; 8192];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                *seen2.lock().await = String::from_utf8_lossy(&buf[..n]).to_string();
                let resp = format!(
                    "{status_line}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.shutdown().await;
            }
        });
        (format!("http://{addr}"), seen, hits)
    }

    const TOKEN_JSON: &str = r#"{"access_token":"at-2","refresh_token":"rt-2","expires_in":3600,
        "token_type":"bearer","scope":"user:inference",
        "organization":{"uuid":"org-1","name":"Kubereva"},
        "account":{"uuid":"acc-1","email_address":"a@example.com"},
        "workspace":{"id":"wrkspc_01","name":"default"}}"#;

    #[test]
    fn refresh_is_needed_inside_the_threshold_and_when_unknown() {
        assert!(
            needs_refresh(None, 1000),
            "no expiry means we cannot trust it"
        );
        assert!(needs_refresh(Some(1000 + 119), 1000));
        assert!(needs_refresh(Some(900), 1000), "already expired");
        assert!(!needs_refresh(Some(1000 + 121), 1000));
    }

    #[test]
    fn token_response_becomes_credentials_with_absolute_expiry() {
        let t: TokenResponse = serde_json::from_str(TOKEN_JSON).unwrap();
        let c = t.into_credentials(1_000);
        assert_eq!(c.access_token, "at-2");
        assert_eq!(c.expires_at, Some(4_600));
        assert_eq!(c.refresh_token.as_deref(), Some("rt-2"));
        assert_eq!(c.organization_name.as_deref(), Some("Kubereva"));
        assert_eq!(c.account_email.as_deref(), Some("a@example.com"));
        assert_eq!(c.workspace_id.as_deref(), Some("wrkspc_01"));
        assert_eq!(c.kind, "oauth_token");
    }

    #[test]
    fn client_id_env_override() {
        // Cannot mutate env under the parallel harness; test the pure form.
        assert_eq!(client_id_from(Some("custom".into())), "custom");
        assert_eq!(client_id_from(Some("  ".into())), CLIENT_ID);
        assert_eq!(client_id_from(None), CLIENT_ID);
    }

    #[tokio::test]
    async fn refresh_posts_json_with_the_oauth_beta_header() {
        let (base, seen, _) = capture_server("HTTP/1.1 200 OK", TOKEN_JSON).await;
        let t = refresh_access_token(&base, "cid", "rt-1").await.unwrap();
        assert_eq!(t.access_token, "at-2");
        let req = seen.lock().await.clone();
        assert!(req.starts_with("POST /v1/oauth/token HTTP/1.1"), "{req}");
        assert!(
            req.to_lowercase()
                .contains("anthropic-beta: oauth-2025-04-20"),
            "{req}"
        );
        assert!(
            req.to_lowercase()
                .contains("content-type: application/json"),
            "{req}"
        );
        let body = req.split("\r\n\r\n").nth(1).unwrap();
        let v: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(v["grant_type"], "refresh_token");
        assert_eq!(v["refresh_token"], "rt-1");
        assert_eq!(v["client_id"], "cid");
    }

    #[tokio::test]
    async fn refresh_failure_carries_status_and_body() {
        let (base, _, _) =
            capture_server("HTTP/1.1 400 Bad Request", r#"{"error":"invalid_grant"}"#).await;
        let err = refresh_access_token(&base, "cid", "rt-1")
            .await
            .unwrap_err();
        let s = err.to_string();
        assert!(s.contains("400") && s.contains("invalid_grant"), "{s}");
    }
}

#[cfg(test)]
mod callback_tests {
    use super::*;

    async fn hit(redirect_uri: &str, query: &str) -> (u16, String) {
        let resp = reqwest::Client::new()
            .get(format!("{redirect_uri}?{query}"))
            .send()
            .await
            .unwrap();
        (resp.status().as_u16(), resp.text().await.unwrap())
    }

    #[tokio::test]
    async fn a_valid_callback_yields_the_code_and_a_success_page() {
        let (listener, redirect) = bind_loopback().await.unwrap();
        assert!(redirect.starts_with("http://localhost:") && redirect.ends_with("/callback"));
        let waiter = tokio::spawn(async move {
            wait_for_code(listener, "st", std::time::Duration::from_secs(5)).await
        });
        let (status, body) = hit(&redirect, "code=abc&state=st").await;
        assert_eq!(status, 200);
        assert!(body.contains("close this tab"), "{body}");
        assert_eq!(waiter.await.unwrap().unwrap(), "abc");
    }

    #[tokio::test]
    async fn a_state_mismatch_is_rejected_with_a_400_page() {
        let (listener, redirect) = bind_loopback().await.unwrap();
        let waiter = tokio::spawn(async move {
            wait_for_code(listener, "st", std::time::Duration::from_secs(5)).await
        });
        let (status, body) = hit(&redirect, "code=abc&state=nope").await;
        assert_eq!(status, 400);
        assert!(body.contains("state"), "{body}");
        let err = waiter.await.unwrap().unwrap_err().to_string();
        assert!(err.contains("state mismatch"), "{err}");
    }

    #[tokio::test(start_paused = true)]
    async fn waiting_times_out() {
        let (listener, _) = bind_loopback().await.unwrap();
        let err = wait_for_code(listener, "st", std::time::Duration::from_secs(1))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("timed out"), "{err}");
    }

    #[tokio::test]
    async fn other_paths_get_404_and_do_not_consume_the_wait() {
        let (listener, redirect) = bind_loopback().await.unwrap();
        let base = redirect.rsplit_once('/').map(|(b, _)| b).unwrap();
        let waiter = tokio::spawn(async move {
            wait_for_code(listener, "st", std::time::Duration::from_secs(5)).await
        });
        let resp = reqwest::Client::new()
            .get(format!("{base}/favicon.ico"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 404);
        let (status, body) = hit(&redirect, "code=abc&state=st").await;
        assert_eq!(status, 200);
        assert!(body.contains("close this tab"), "{body}");
        assert_eq!(waiter.await.unwrap().unwrap(), "abc");
    }

    #[tokio::test]
    async fn reflected_error_description_is_escaped() {
        let (listener, redirect) = bind_loopback().await.unwrap();
        let waiter = tokio::spawn(async move {
            wait_for_code(listener, "st", std::time::Duration::from_secs(5)).await
        });
        let (status, body) = hit(
            &redirect,
            "error=x&error_description=%3Cscript%3Ealert(1)%3C%2Fscript%3E&state=st",
        )
        .await;
        assert_eq!(status, 400);
        assert!(!body.contains("<script>"), "unescaped script in {body}");
        assert!(
            body.contains("&lt;script&gt;"),
            "escaped script not in {body}"
        );
        let err = waiter.await.unwrap().unwrap_err().to_string();
        assert!(err.contains("authorization denied"), "{err}");
    }

    #[tokio::test]
    async fn a_silent_connection_does_not_block_the_callback() {
        let (listener, redirect) = bind_loopback().await.unwrap();
        let port = redirect
            .split(':')
            .nth(2)
            .and_then(|p| p.split('/').next())
            .and_then(|p| p.parse::<u16>().ok())
            .unwrap();
        let waiter = tokio::spawn(async move {
            wait_for_code(listener, "st", std::time::Duration::from_secs(5)).await
        });
        let silent = tokio::spawn(async move {
            if tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .is_ok()
            {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        });
        let _ = silent.await;
        let (status, _body) = hit(&redirect, "code=abc&state=st").await;
        assert_eq!(status, 200);
        assert_eq!(waiter.await.unwrap().unwrap(), "abc");
    }
}
