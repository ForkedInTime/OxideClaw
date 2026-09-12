//! Console OAuth for Anthropic: PKCE login, code exchange, refresh.
//! Parameters mirror the open-source `ant` CLI (`pkg/cmd/cmd_auth.go`).

#![allow(dead_code)] // Task 4 and lib consumers will use these items

use super::profile;
use super::profile::ProfileCredentials;
use anyhow::{Context, Result, anyhow};
use base64::Engine as _;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
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

/// The Console's manual code page renders `<code>#<state>`, so the pasted
/// value carries the CSRF state back to us (this mirrors `readManualCode` in
/// the `ant` CLI). Split it, verify the state, and return the code half. A
/// bare code with no `#` is accepted as-is.
pub fn split_manual_code(pasted: &str, expected_state: &str) -> Result<String> {
    let pasted = pasted.trim();
    if pasted.is_empty() {
        return Err(anyhow!("login cancelled"));
    }
    let Some((code, state)) = pasted.split_once('#') else {
        return Ok(pasted.to_string());
    };
    if state != expected_state {
        return Err(anyhow!(
            "pasted code is from a different login attempt — start /login again"
        ));
    }
    let code = code.trim();
    if code.is_empty() {
        return Err(anyhow!("login cancelled"));
    }
    Ok(code.to_string())
}

pub const CALLBACK_TIMEOUT: Duration = Duration::from_secs(300);
/// How long one accepted connection may stay silent before we drop it and go
/// back to accepting. Bounds each peer, never the overall wait.
pub const PER_CONNECTION_READ_TIMEOUT: Duration = Duration::from_secs(10);

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
        // Bound each connection separately: a peer that connects and then says
        // nothing (a port scanner, a browser pre-connect) must not hold the
        // whole five-minute wait hostage. Drop it and keep listening.
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let read_budget = remaining.min(PER_CONNECTION_READ_TIMEOUT);
        let n = match tokio::time::timeout(read_budget, sock.read(&mut buf)).await {
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

/// Redeem an authorization code. Form-encoded, as `ant` does.
pub async fn exchange_code(
    base_url: &str,
    client_id: &str,
    code: &str,
    verifier: &str,
    redirect_uri: &str,
    state: &str,
) -> Result<TokenResponse> {
    let resp = http()?
        .post(format!("{}/v1/oauth/token", base_url.trim_end_matches('/')))
        .header("anthropic-beta", OAUTH_BETA)
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("code_verifier", verifier),
            ("client_id", client_id),
            ("redirect_uri", redirect_uri),
            ("state", state),
        ])
        .send()
        .await
        .context("token exchange request")?;
    parse_token_response(resp, "code exchange").await
}

#[derive(Debug, Clone)]
pub struct LoginRequest {
    pub dir: PathBuf,
    pub profile: String,
    /// Write `active_config` after success.
    pub activate: bool,
    pub base_url: String,
    pub console_url: String,
    pub client_id: String,
    pub workspace_id: Option<String>,
}

impl LoginRequest {
    /// `name` given → that profile, activated. Otherwise the profile that
    /// would be used today (`ANTHROPIC_PROFILE` → active → default), and it
    /// becomes active only when nothing is active yet — mirrors `ant`.
    pub fn for_profile(name: Option<&str>) -> Result<Self> {
        let dir = profile::config_dir()
            .ok_or_else(|| anyhow!("cannot determine the Anthropic config directory"))?;
        let env_profile = std::env::var("ANTHROPIC_PROFILE").ok();
        Self::for_profile_in(dir, name, env_profile.as_deref())
    }

    /// Pure form of [`Self::for_profile`], parameterized on the config
    /// directory and the `ANTHROPIC_PROFILE` value, for testing without
    /// touching the real environment.
    pub fn for_profile_in(
        dir: PathBuf,
        name: Option<&str>,
        env_profile: Option<&str>,
    ) -> Result<Self> {
        let active_exists = dir.join("active_config").is_file();
        let (profile, activate) = match name.map(str::trim).filter(|n| !n.is_empty()) {
            Some(n) => (n.to_string(), true),
            None => (
                profile::resolve_profile_name(&dir, env_profile),
                !active_exists,
            ),
        };
        if !profile
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
        {
            return Err(anyhow!(
                "profile name may contain only letters, digits, '-', '_' and '.'"
            ));
        }
        let existing = profile::load_config(&dir, &profile).ok().flatten();
        Ok(Self {
            dir,
            profile,
            activate,
            base_url: existing
                .as_ref()
                .and_then(|c| c.base_url.clone())
                .unwrap_or_else(|| API_BASE.to_string()),
            console_url: existing
                .as_ref()
                .and_then(|c| c.authentication.console_url.clone())
                .unwrap_or_else(|| CONSOLE_URL.to_string()),
            client_id: existing
                .as_ref()
                .and_then(|c| c.authentication.client_id.clone())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(client_id),
            workspace_id: existing.and_then(|c| c.workspace_id),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginOutcome {
    pub profile: String,
    pub organization: Option<String>,
    pub email: Option<String>,
    pub workspace: Option<String>,
    pub expires_at: Option<i64>,
}

/// Write the profile config (merged over any existing one) and credentials,
/// then activate if requested.
pub fn persist(req: &LoginRequest, tok: TokenResponse) -> Result<LoginOutcome> {
    let creds = tok.into_credentials(now_unix());
    let mut cfg = profile::load_config(&req.dir, &req.profile)
        .ok()
        .flatten()
        .unwrap_or_else(|| profile::ProfileConfig::user_oauth(&req.client_id, None, None));
    cfg.authentication.kind = profile::AUTH_TYPE_USER_OAUTH.into();
    cfg.authentication.client_id = Some(req.client_id.clone());
    if req.console_url != CONSOLE_URL {
        cfg.authentication.console_url = Some(req.console_url.clone());
    }
    if req.base_url != API_BASE {
        cfg.base_url = Some(req.base_url.clone());
    }
    if creds.organization_uuid.is_some() {
        cfg.organization_id = creds.organization_uuid.clone();
    }
    if creds.workspace_id.is_some() {
        cfg.workspace_id = creds.workspace_id.clone();
    }
    if cfg.version.is_empty() {
        cfg.version = profile::CONFIG_FILE_VERSION.into();
    }
    profile::save_config(&req.dir, &req.profile, &cfg)?;
    profile::save_credentials(&req.dir, &req.profile, &creds)?;
    if req.activate {
        profile::set_active_profile(&req.dir, &req.profile)?;
    }
    Ok(LoginOutcome {
        profile: req.profile.clone(),
        organization: creds.organization_name,
        email: creds.account_email,
        workspace: creds.workspace_name,
        expires_at: creds.expires_at,
    })
}

fn org_hint(req: &LoginRequest) -> Option<String> {
    profile::load_config(&req.dir, &req.profile)
        .ok()
        .flatten()
        .and_then(|c| c.organization_id)
}

/// Browser flow: loopback listener, authorize page in the browser, wait for
/// the callback, exchange, persist. `open_url` returns whether a browser was
/// launched; when it fails the URL is reported through `progress` and the
/// listener keeps waiting so the user can open it by hand.
pub async fn login_browser(
    req: &LoginRequest,
    open_url: impl Fn(&str) -> bool,
    progress: impl Fn(String),
) -> Result<LoginOutcome> {
    let verifier = pkce_verifier();
    let challenge = pkce_challenge_s256(&verifier);
    let state = random_state();
    let (listener, redirect_uri) = bind_loopback().await?;
    let org = org_hint(req);
    let authorize_url = build_authorize_url(&AuthorizeParams {
        console_url: &req.console_url,
        client_id: &req.client_id,
        redirect_uri: &redirect_uri,
        scope: SCOPE,
        state: &state,
        code_challenge: &challenge,
        org_uuid: org.as_deref(),
        workspace_id: req.workspace_id.as_deref(),
    });
    if open_url(&authorize_url) {
        progress(format!(
            "Opened the browser to sign in. Waiting for the callback on {redirect_uri} (up to 5 minutes)…\nIf nothing opened, visit:\n  {authorize_url}"
        ));
    } else {
        progress(format!(
            "Could not open a browser. Open this URL on this machine:\n  {authorize_url}\nWaiting for the callback on {redirect_uri} (up to 5 minutes)…"
        ));
    }
    let code = wait_for_code(listener, &state, CALLBACK_TIMEOUT).await?;
    progress("Callback received — exchanging the code for a token…".into());
    let tok = exchange_code(
        &req.base_url,
        &req.client_id,
        &code,
        &verifier,
        &redirect_uri,
        &state,
    )
    .await?;
    persist(req, tok)
}

/// Manual flow for hosts without a usable localhost: the Console shows the
/// code on a page and the user pastes it into `prompt`, which receives the
/// authorize URL to display and returns the pasted code (or `None` to cancel).
pub async fn login_manual(
    req: &LoginRequest,
    prompt: impl FnOnce(String) -> Pin<Box<dyn Future<Output = Option<String>> + Send>>,
) -> Result<LoginOutcome> {
    let verifier = pkce_verifier();
    let challenge = pkce_challenge_s256(&verifier);
    let state = random_state();
    let redirect_uri = manual_redirect_uri(&req.console_url);
    let org = org_hint(req);
    let authorize_url = build_authorize_url(&AuthorizeParams {
        console_url: &req.console_url,
        client_id: &req.client_id,
        redirect_uri: &redirect_uri,
        scope: SCOPE,
        state: &state,
        code_challenge: &challenge,
        org_uuid: org.as_deref(),
        workspace_id: req.workspace_id.as_deref(),
    });
    let pasted = prompt(authorize_url)
        .await
        .ok_or_else(|| anyhow!("login cancelled"))?;
    let code = split_manual_code(&pasted, &state)?;
    let tok = exchange_code(
        &req.base_url,
        &req.client_id,
        &code,
        &verifier,
        &redirect_uri,
        &state,
    )
    .await?;
    persist(req, tok)
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
    fn manual_code_splits_on_the_state_fragment() {
        // Bare code: accepted, trimmed.
        assert_eq!(split_manual_code("  abc  ", "st").unwrap(), "abc");
        // `<code>#<state>` with the matching state: only the code half.
        assert_eq!(split_manual_code("abc#st", "st").unwrap(), "abc");
        assert_eq!(split_manual_code(" abc#st \n", "st").unwrap(), "abc");
        // A code carrying someone else's state is refused.
        let err = split_manual_code("abc#other", "st")
            .unwrap_err()
            .to_string();
        assert!(err.contains("different login attempt"), "{err}");
        // Empty input is a cancellation, not an exchange.
        for empty in ["", "   ", "\n", "#st"] {
            let err = split_manual_code(empty, "st").unwrap_err().to_string();
            assert!(err.contains("cancelled"), "{empty:?}: {err}");
        }
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

    /// A peer that connects and never speaks — and never hangs up — used to
    /// pin the read to the whole remaining five minutes, so the real callback
    /// behind it in the accept backlog was never served. Each connection now
    /// gets its own short budget.
    #[tokio::test]
    async fn a_silent_connection_that_stays_open_does_not_block_the_callback() {
        assert!(
            PER_CONNECTION_READ_TIMEOUT < std::time::Duration::from_secs(15),
            "this test waits out one per-connection budget in real time"
        );
        let (listener, redirect) = bind_loopback().await.unwrap();
        let port = redirect
            .split(':')
            .nth(2)
            .and_then(|p| p.split('/').next())
            .and_then(|p| p.parse::<u16>().ok())
            .unwrap();
        // Overall deadline far beyond the per-connection budget: if the read
        // were still bounded by `remaining`, this test would hang, not fail.
        let waiter = tokio::spawn(async move {
            wait_for_code(listener, "st", std::time::Duration::from_secs(240)).await
        });

        // Held for the whole test: the socket is never dropped, never written to.
        let _silent = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap();
        // Give the waiter time to accept the silent peer first.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let (status, _body) = hit(&redirect, "code=abc&state=st").await;
        assert_eq!(status, 200);
        assert_eq!(waiter.await.unwrap().unwrap(), "abc");
    }
}

#[cfg(test)]
mod login_flow_tests {
    use super::*;
    use crate::auth::profile::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Token endpoint stub that records the last form body.
    async fn token_server(body: &'static str) -> (String, Arc<tokio::sync::Mutex<String>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = Arc::new(tokio::sync::Mutex::new(String::new()));
        let seen2 = seen.clone();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = vec![0u8; 8192];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                *seen2.lock().await = String::from_utf8_lossy(&buf[..n]).to_string();
                let resp = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.shutdown().await;
            }
        });
        (format!("http://{addr}"), seen)
    }

    const TOKEN_JSON: &str = r#"{"access_token":"at","refresh_token":"rt","expires_in":3600,
        "scope":"user:inference","organization":{"uuid":"org-1","name":"Kubereva"},
        "account":{"uuid":"acc","email_address":"a@example.com"},
        "workspace":{"id":"wrkspc_01","name":"default"}}"#;

    fn req(dir: &std::path::Path, base: &str) -> LoginRequest {
        LoginRequest {
            dir: dir.to_path_buf(),
            profile: "default".into(),
            activate: true,
            base_url: base.into(),
            console_url: "https://console.test".into(),
            client_id: "cid".into(),
            workspace_id: None,
        }
    }

    #[test]
    fn for_profile_in_uses_the_explicit_name_and_activates() {
        let d = tempfile::tempdir().unwrap();
        let r = LoginRequest::for_profile_in(d.path().to_path_buf(), Some("work"), None).unwrap();
        assert_eq!(r.profile, "work");
        assert!(r.activate);
    }

    #[test]
    fn for_profile_in_defaults_to_default_and_activates_when_nothing_active() {
        let d = tempfile::tempdir().unwrap();
        let r = LoginRequest::for_profile_in(d.path().to_path_buf(), None, None).unwrap();
        assert_eq!(r.profile, "default");
        assert!(r.activate);
    }

    #[test]
    fn for_profile_in_follows_the_active_pointer_and_does_not_steal_it() {
        let d = tempfile::tempdir().unwrap();
        set_active_profile(d.path(), "team").unwrap();
        let r = LoginRequest::for_profile_in(d.path().to_path_buf(), None, None).unwrap();
        assert_eq!(r.profile, "team");
        assert!(!r.activate);
    }

    #[test]
    fn for_profile_in_env_beats_the_active_pointer() {
        let d = tempfile::tempdir().unwrap();
        // No active_config: env-selected profile should still activate.
        let r =
            LoginRequest::for_profile_in(d.path().to_path_buf(), None, Some("envprof")).unwrap();
        assert_eq!(r.profile, "envprof");
        assert!(r.activate);

        // With an active_config present, env still wins on name, but
        // activation reflects whether something was already active.
        set_active_profile(d.path(), "team").unwrap();
        let r =
            LoginRequest::for_profile_in(d.path().to_path_buf(), None, Some("envprof")).unwrap();
        assert_eq!(r.profile, "envprof");
        assert!(!r.activate);
    }

    #[test]
    fn for_profile_in_rejects_invalid_characters() {
        let d = tempfile::tempdir().unwrap();
        let err = LoginRequest::for_profile_in(d.path().to_path_buf(), Some("bad name!"), None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("letters, digits, '-', '_' and '.'"), "{err}");
    }

    #[test]
    fn for_profile_in_seeds_from_an_existing_config_or_falls_back_to_defaults() {
        let d = tempfile::tempdir().unwrap();
        let mut cfg = ProfileConfig::user_oauth("cid-x", None, Some("wrkspc_9".into()));
        cfg.base_url = Some("https://api.test".into());
        cfg.authentication.console_url = Some("https://console.test".into());
        save_config(d.path(), "work", &cfg).unwrap();

        let r = LoginRequest::for_profile_in(d.path().to_path_buf(), Some("work"), None).unwrap();
        assert_eq!(r.base_url, "https://api.test");
        assert_eq!(r.console_url, "https://console.test");
        assert_eq!(r.client_id, "cid-x");
        assert_eq!(r.workspace_id.as_deref(), Some("wrkspc_9"));

        let r2 = LoginRequest::for_profile_in(d.path().to_path_buf(), Some("nonexistent"), None)
            .unwrap();
        assert_eq!(r2.base_url, API_BASE);
        assert_eq!(r2.console_url, CONSOLE_URL);
        assert_eq!(r2.client_id, client_id());
        assert_eq!(r2.workspace_id, None);
    }

    #[tokio::test]
    async fn exchange_posts_the_form_grant() {
        let (base, seen) = token_server(TOKEN_JSON).await;
        let t = exchange_code(
            &base,
            "cid",
            "code1",
            "ver",
            "http://localhost:1/callback",
            "st",
        )
        .await
        .unwrap();
        assert_eq!(t.access_token, "at");
        let r = seen.lock().await.clone();
        assert!(
            r.to_lowercase()
                .contains("content-type: application/x-www-form-urlencoded"),
            "{r}"
        );
        assert!(
            r.to_lowercase()
                .contains("anthropic-beta: oauth-2025-04-20"),
            "{r}"
        );
        let body = r.split("\r\n\r\n").nth(1).unwrap();
        let q: std::collections::HashMap<String, String> =
            url::form_urlencoded::parse(body.as_bytes())
                .into_owned()
                .collect();
        assert_eq!(q["grant_type"], "authorization_code");
        assert_eq!(q["code"], "code1");
        assert_eq!(q["code_verifier"], "ver");
        assert_eq!(q["client_id"], "cid");
        assert_eq!(q["redirect_uri"], "http://localhost:1/callback");
        assert_eq!(q["state"], "st");
    }

    #[test]
    fn persist_writes_both_files_and_activates() {
        let d = tempfile::tempdir().unwrap();
        let tok: TokenResponse = serde_json::from_str(TOKEN_JSON).unwrap();
        let out = persist(&req(d.path(), "http://unused"), tok).unwrap();
        assert_eq!(out.organization.as_deref(), Some("Kubereva"));
        assert_eq!(out.email.as_deref(), Some("a@example.com"));
        let cfg = load_config(d.path(), "default").unwrap().unwrap();
        assert_eq!(cfg.authentication.kind, "user_oauth");
        assert_eq!(cfg.authentication.client_id.as_deref(), Some("cid"));
        assert_eq!(cfg.organization_id.as_deref(), Some("org-1"));
        assert_eq!(cfg.workspace_id.as_deref(), Some("wrkspc_01"));
        let creds = load_credentials(d.path(), "default").unwrap().unwrap();
        assert_eq!(creds.access_token, "at");
        assert!(creds.expires_at.unwrap() > now_unix() + 3000);
        assert_eq!(resolve_profile_name(d.path(), None), "default");
    }

    #[test]
    fn persist_does_not_steal_the_active_pointer_unless_asked() {
        let d = tempfile::tempdir().unwrap();
        set_active_profile(d.path(), "work").unwrap();
        let tok: TokenResponse = serde_json::from_str(TOKEN_JSON).unwrap();
        let mut r = req(d.path(), "http://unused");
        r.activate = false;
        persist(&r, tok).unwrap();
        assert_eq!(resolve_profile_name(d.path(), None), "work");
    }

    #[tokio::test]
    async fn browser_flow_end_to_end_with_a_simulated_browser() {
        let (base, _) = token_server(TOKEN_JSON).await;
        let d = tempfile::tempdir().unwrap();
        let opened = Arc::new(AtomicUsize::new(0));
        let opened2 = opened.clone();
        // "Browser": parse the authorize URL and immediately hit the redirect
        // with a code and the same state, like Console would.
        let open = move |authorize_url: &str| -> bool {
            opened2.fetch_add(1, Ordering::SeqCst);
            let u = url::Url::parse(authorize_url).unwrap();
            let q: std::collections::HashMap<_, _> = u.query_pairs().into_owned().collect();
            let target = format!("{}?code=c1&state={}", q["redirect_uri"], q["state"]);
            tokio::spawn(async move {
                let _ = reqwest::get(target).await;
            });
            true
        };
        let msgs = Arc::new(std::sync::Mutex::new(Vec::new()));
        let m2 = msgs.clone();
        let out = login_browser(&req(d.path(), &base), open, move |s| {
            m2.lock().unwrap().push(s)
        })
        .await
        .unwrap();
        assert_eq!(opened.load(Ordering::SeqCst), 1);
        assert_eq!(out.email.as_deref(), Some("a@example.com"));
        assert!(profile_exists(d.path(), "default"));
        assert!(
            msgs.lock().unwrap().iter().any(|m| m.contains("Waiting")),
            "{msgs:?}"
        );
    }

    #[tokio::test]
    async fn manual_flow_uses_the_console_code_page_and_the_prompt() {
        let (base, seen) = token_server(TOKEN_JSON).await;
        let d = tempfile::tempdir().unwrap();
        let out = login_manual(&req(d.path(), &base), |authorize_url| {
            Box::pin(async move {
                assert!(
                    authorize_url.contains("oauth%2Fcode%2Fcallback%3Fapp%3Danthropic-cli"),
                    "{authorize_url}"
                );
                // The Console's code page shows `<code>#<state>`; paste both.
                let state = url::Url::parse(&authorize_url)
                    .unwrap()
                    .query_pairs()
                    .find(|(k, _)| k == "state")
                    .map(|(_, v)| v.into_owned())
                    .expect("authorize URL carries a state");
                Some(format!("pasted-code#{state}"))
            })
        })
        .await
        .unwrap();
        assert_eq!(out.profile, "default");
        let body = seen.lock().await.clone();
        // Only the code half is exchanged — the `#state` suffix is stripped.
        assert!(body.contains("code=pasted-code&"), "{body}");
        assert!(!body.contains("%23"), "state fragment leaked into {body}");
        assert!(body.contains("redirect_uri=https%3A%2F%2Fconsole.test%2Foauth%2Fcode%2Fcallback%3Fapp%3Danthropic-cli"), "{body}");
    }

    #[tokio::test]
    async fn manual_flow_cancelled_at_the_prompt_is_an_error_not_a_hang() {
        let d = tempfile::tempdir().unwrap();
        let err = login_manual(&req(d.path(), "http://unused"), |_| {
            Box::pin(async { None })
        })
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("cancelled"), "{err}");
        assert!(!profile_exists(d.path(), "default"));
    }
}
