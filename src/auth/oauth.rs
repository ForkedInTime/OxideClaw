//! Console OAuth for Anthropic: PKCE login, code exchange, refresh.
//! Parameters mirror the open-source `ant` CLI (`pkg/cmd/cmd_auth.go`).

#![allow(dead_code)] // Task 4 and lib consumers will use these items

use super::profile::ProfileCredentials;
use anyhow::{Context, Result, anyhow};
use serde::Deserialize;

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
