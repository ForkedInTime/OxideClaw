# `/login` Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Sign in to Anthropic from inside the TUI with Console OAuth (no `ant` CLI), keep the token fresh mid-session, add and remove OpenAI-compatible provider keys through a masked dialog, and show one `/login` status board.

**Architecture:** `src/auth.rs` becomes a directory: profile files (`profile.rs`), the OAuth flow and refresh (`oauth.rs`), and a `.env`-backed provider keystore (`keystore.rs`). An `AuthHandle` shared through `Config` gives the Anthropic client a per-request credential that refreshes itself. The TUI drives login flows in spawned tasks and learns the outcome through a `CredentialChanged` event.

**Tech Stack:** Rust 2024 edition, tokio (add the `net` feature), reqwest 0.12 (rustls), serde/serde_json, sha2, base64, rand 0.9, url, dirs, tempfile (dev). No new crates.

**Spec:** `docs/superpowers/specs/2026-09-11-login-auth-design.md`

## Global Constraints

- Client ID `41077d10-94b8-4194-be48-d251e9eb21b4`, overridable by `ANTHROPIC_OAUTH_CLIENT_ID`.
- Console URL `https://platform.claude.com`; token endpoint `https://api.anthropic.com/v1/oauth/token`.
- Scope `user:profile user:inference user:developer`; beta header `oauth-2025-04-20` on exchange, refresh, and every OAuth request.
- Profile root: `$ANTHROPIC_CONFIG_DIR`, else `~/.config/anthropic` (Unix) or `%APPDATA%\Anthropic` (Windows). Files `active_config`, `configs/<p>.json`, `credentials/<p>.json`; credential files 0600 in 0700 dirs; atomic writes.
- Refresh when fewer than 120 seconds remain. Callback timeout 300 seconds. Listener bound to `127.0.0.1` only.
- Provider keys live only in `~/.config/oxideclaw/.env` (via `crate::config::app_dir`), 0600 in 0700. Never write under the project directory. Never read another tool's config or credential files.
- Never log or display a secret; display uses `Credential::redacted()` (first 8 chars).
- Commit trailer on every commit: `Co-Authored-By: Arch Linux <noreply@archlinux.org>`. No other trailer, ever.
- Run `cargo test` (both lib and bin targets) and `cargo clippy --all-targets` before each commit; both must be clean.

## File Structure

| File | Responsibility |
|------|----------------|
| `src/auth/mod.rs` | `Credential`, `CredentialSource`, `Resolved`, `AuthEnv`, resolution order, `AuthHandle` (moved from `src/auth.rs`) |
| `src/auth/profile.rs` | Profile root dir, active profile, `ProfileConfig` / `ProfileCredentials` serde types, atomic load/save/delete |
| `src/auth/oauth.rs` | Constants, PKCE, authorize URL, loopback callback, code exchange, refresh, `login_browser` / `login_manual` |
| `src/auth/keystore.rs` | `SAFE_ENV_KEYS` (moved from `main.rs`), `.env` loading with source attribution, `Keystore`, line editing, save/remove |
| `src/api/mod.rs` | `ClaudeClient` authenticates per request through `AuthHandle`; `ApiBackend::from_config` |
| `src/api/openai_compat.rs` | `ProviderDef.key_url`, `from_model_with(lookup)`, `validate_key` |
| `src/config.rs` | `Config.auth: AuthHandle`, `Config.keystore: Keystore`, `resolve_anthropic_auth()` |
| `src/commands/login.rs` | `/login` and `/logout` parsing, board rows |
| `src/tui/events.rs` | `AskUser.secret`, `CredentialChanged` |
| `src/tui/run/dispatch.rs`, `src/tui/run.rs`, `src/tui/run/keys.rs`, `src/tui/render.rs`, `src/tui/app.rs` | Flow orchestration, event handling, masked dialog, board overlay |
| `src/commands/status.rs`, `src/commands/help.rs`, `README.md`, `FEATURES.md` | Doctor, help, docs |

---

## Increment 1: native profile reading and refresh

### Task 1: Move `auth.rs` into a directory and enable tokio `net`

**Files:**
- Move: `src/auth.rs` → `src/auth/mod.rs`
- Modify: `Cargo.toml:30`

**Interfaces:**
- Produces: unchanged public API of `crate::auth`; `tokio::net` available to the crate.

- [ ] **Step 1: Move the file**

```bash
git mv src/auth.rs src/auth/mod.rs
```

- [ ] **Step 2: Add the `net` feature to tokio**

In `Cargo.toml` line 30 change the tokio features list to include `"net"`:

```toml
tokio = { version = "1", features = ["rt-multi-thread", "macros", "fs", "process", "sync", "time", "io-util", "io-std", "signal", "net"] }
```

- [ ] **Step 3: Build and test**

Run: `cargo test --lib auth 2>&1 | tail -3 && cargo clippy --all-targets 2>&1 | grep -c "^warning\|^error"`
Expected: all auth tests pass; count is `0`.

- [ ] **Step 4: Commit**

```bash
git add Cargo.toml Cargo.lock src/auth/mod.rs
git commit -m "auth: move auth.rs to auth/mod.rs; enable tokio net

Preparation for splitting profile files, the OAuth flow, and the
provider keystore into their own modules.

Co-Authored-By: Arch Linux <noreply@archlinux.org>"
```

### Task 2: Profile files (`auth/profile.rs`)

**Files:**
- Create: `src/auth/profile.rs`
- Modify: `src/auth/mod.rs` (add `pub mod profile;`, move `anthropic_config_dir` there)

**Interfaces:**
- Produces:
  - `pub fn config_dir() -> Option<PathBuf>`
  - `pub fn resolve_profile_name(dir: &Path, env_profile: Option<&str>) -> String`
  - `pub struct ProfileConfig { version, authentication: ProfileAuth, organization_id, workspace_id, base_url }`
  - `pub struct ProfileAuth { kind, client_id, scope, console_url }` (serialised as `type`, `client_id`, `scope`, `console_url`)
  - `pub struct ProfileCredentials { version, kind, access_token, expires_at: Option<i64>, refresh_token, scope, organization_uuid, organization_name, account_email, workspace_id, workspace_name }`
  - `pub fn load_config(dir, profile) -> Result<Option<ProfileConfig>>`
  - `pub fn load_credentials(dir, profile) -> Result<Option<ProfileCredentials>>`
  - `pub fn save_config(dir, profile, &ProfileConfig) -> Result<()>`
  - `pub fn save_credentials(dir, profile, &ProfileCredentials) -> Result<()>`
  - `pub fn set_active_profile(dir, profile) -> Result<()>`
  - `pub fn delete_profile(dir, profile) -> Result<bool>`
  - `pub fn profile_exists(dir, profile) -> bool`

- [ ] **Step 1: Write the failing tests**

Create `src/auth/profile.rs` with only the test module first:

```rust
//! On-disk profile files shared with the `ant` CLI, the official SDKs, and
//! Claude Code. Wire shapes copied from anthropic-sdk-go `config/writers.go`
//! and `config/config.go`; unknown fields are ignored on read.

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn credentials_round_trip_matches_the_go_wire_shape() {
        let fixture = r#"{
  "version": "1.0",
  "type": "oauth_token",
  "access_token": "at-1",
  "expires_at": 1757600000,
  "refresh_token": "rt-1",
  "scope": "user:profile user:inference",
  "organization_uuid": "org-uuid",
  "organization_name": "Kubereva",
  "account_email": "a@example.com",
  "workspace_id": "wrkspc_01",
  "workspace_name": "default",
  "unknown_future_field": true
}"#;
        let c: ProfileCredentials = serde_json::from_str(fixture).unwrap();
        assert_eq!(c.access_token, "at-1");
        assert_eq!(c.expires_at, Some(1757600000));
        assert_eq!(c.refresh_token.as_deref(), Some("rt-1"));
        assert_eq!(c.organization_name.as_deref(), Some("Kubereva"));
        let out: serde_json::Value = serde_json::to_value(&c).unwrap();
        assert_eq!(out["type"], "oauth_token");
        assert_eq!(out["version"], "1.0");
        assert_eq!(out["expires_at"], 1757600000);
        assert!(out.get("unknown_future_field").is_none());
    }

    #[test]
    fn empty_optional_fields_are_omitted_on_write() {
        let c = ProfileCredentials::new("at", None, None);
        let s = serde_json::to_string(&c).unwrap();
        assert!(!s.contains("refresh_token"), "{s}");
        assert!(!s.contains("workspace_id"), "{s}");
        assert!(s.contains("\"type\":\"oauth_token\""), "{s}");
    }

    #[test]
    fn a_wrong_credentials_type_fails_loud() {
        let bad = r#"{"type":"api_key","access_token":"x"}"#;
        let d = tmp();
        std::fs::create_dir_all(d.path().join("credentials")).unwrap();
        std::fs::write(d.path().join("credentials/default.json"), bad).unwrap();
        let err = load_credentials(d.path(), "default").unwrap_err();
        assert!(err.to_string().contains("api_key"), "{err}");
    }

    #[test]
    fn config_round_trip_matches_the_go_wire_shape() {
        let fixture = r#"{
  "version": "1.0",
  "authentication": { "type": "user_oauth", "client_id": "cid", "scope": "s" },
  "organization_id": "org-uuid",
  "workspace_id": "wrkspc_01"
}"#;
        let c: ProfileConfig = serde_json::from_str(fixture).unwrap();
        assert_eq!(c.authentication.kind, "user_oauth");
        assert_eq!(c.authentication.client_id.as_deref(), Some("cid"));
        assert_eq!(c.organization_id.as_deref(), Some("org-uuid"));
        let out: serde_json::Value = serde_json::to_value(&c).unwrap();
        assert_eq!(out["authentication"]["type"], "user_oauth");
        assert!(out.get("base_url").is_none());
    }

    #[test]
    fn save_then_load_and_permissions() {
        let d = tmp();
        let creds = ProfileCredentials::new("at", Some("rt".into()), Some(42));
        save_credentials(d.path(), "work", &creds).unwrap();
        let cfg = ProfileConfig::user_oauth("cid", Some("org".into()), None);
        save_config(d.path(), "work", &cfg).unwrap();

        assert_eq!(load_credentials(d.path(), "work").unwrap().unwrap(), creds);
        assert_eq!(load_config(d.path(), "work").unwrap().unwrap(), cfg);
        assert!(load_credentials(d.path(), "nope").unwrap().is_none());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let f = std::fs::metadata(d.path().join("credentials/work.json")).unwrap();
            assert_eq!(f.permissions().mode() & 0o777, 0o600);
            let dir = std::fs::metadata(d.path().join("credentials")).unwrap();
            assert_eq!(dir.permissions().mode() & 0o777, 0o700);
        }
    }

    #[test]
    fn active_profile_resolution_order() {
        let d = tmp();
        assert_eq!(resolve_profile_name(d.path(), None), "default");
        set_active_profile(d.path(), "work").unwrap();
        assert_eq!(
            std::fs::read_to_string(d.path().join("active_config")).unwrap(),
            "work\n"
        );
        assert_eq!(resolve_profile_name(d.path(), None), "work");
        assert_eq!(resolve_profile_name(d.path(), Some("other")), "other");
        assert_eq!(resolve_profile_name(d.path(), Some("  ")), "work", "blank env is unset");
    }

    #[test]
    fn delete_removes_both_files_and_clears_the_pointer() {
        let d = tmp();
        save_credentials(d.path(), "work", &ProfileCredentials::new("at", None, None)).unwrap();
        save_config(d.path(), "work", &ProfileConfig::user_oauth("cid", None, None)).unwrap();
        set_active_profile(d.path(), "work").unwrap();
        assert!(profile_exists(d.path(), "work"));
        assert!(delete_profile(d.path(), "work").unwrap());
        assert!(!profile_exists(d.path(), "work"));
        assert!(!d.path().join("active_config").exists());
        assert!(!delete_profile(d.path(), "work").unwrap(), "second delete is a no-op");
    }
}
```

Add to `src/auth/mod.rs` near the top: `pub mod profile;`

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib auth::profile 2>&1 | grep -E "^error" | head -3`
Expected: compile errors (`ProfileCredentials` not found).

- [ ] **Step 3: Implement**

Prepend the implementation to `src/auth/profile.rs` (above the test module):

```rust
use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const CONFIG_FILE_VERSION: &str = "1.0";
pub const CREDENTIALS_FILE_VERSION: &str = "1.0";
pub const AUTH_TYPE_USER_OAUTH: &str = "user_oauth";
pub const CREDENTIALS_TYPE_OAUTH_TOKEN: &str = "oauth_token";

/// `$ANTHROPIC_CONFIG_DIR`, else `~/.config/anthropic` on Unix and
/// `%APPDATA%\Anthropic` on Windows.
pub fn config_dir() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("ANTHROPIC_CONFIG_DIR")
        && !dir.trim().is_empty()
    {
        return Some(PathBuf::from(dir));
    }
    #[cfg(windows)]
    {
        std::env::var("APPDATA")
            .ok()
            .map(|d| PathBuf::from(d).join("Anthropic"))
    }
    #[cfg(not(windows))]
    {
        dirs::home_dir().map(|h| h.join(".config").join("anthropic"))
    }
}

/// `ANTHROPIC_PROFILE` → `active_config` file → `"default"`.
pub fn resolve_profile_name(dir: &Path, env_profile: Option<&str>) -> String {
    if let Some(p) = env_profile.map(str::trim).filter(|p| !p.is_empty()) {
        return p.to_string();
    }
    if let Ok(s) = std::fs::read_to_string(dir.join("active_config")) {
        let s = s.trim();
        if !s.is_empty() {
            return s.to_string();
        }
    }
    "default".to_string()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileAuth {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub console_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileConfig {
    #[serde(default)]
    pub version: String,
    pub authentication: ProfileAuth,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub organization_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
}

impl ProfileConfig {
    pub fn user_oauth(
        client_id: &str,
        organization_id: Option<String>,
        workspace_id: Option<String>,
    ) -> Self {
        Self {
            version: CONFIG_FILE_VERSION.into(),
            authentication: ProfileAuth {
                kind: AUTH_TYPE_USER_OAUTH.into(),
                client_id: Some(client_id.into()),
                scope: None,
                console_url: None,
            },
            organization_id,
            workspace_id,
            base_url: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileCredentials {
    #[serde(default)]
    pub version: String,
    #[serde(rename = "type", default)]
    pub kind: String,
    pub access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub organization_uuid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub organization_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_name: Option<String>,
}

impl ProfileCredentials {
    pub fn new(access_token: &str, refresh_token: Option<String>, expires_at: Option<i64>) -> Self {
        Self {
            version: CREDENTIALS_FILE_VERSION.into(),
            kind: CREDENTIALS_TYPE_OAUTH_TOKEN.into(),
            access_token: access_token.into(),
            expires_at,
            refresh_token,
            scope: None,
            organization_uuid: None,
            organization_name: None,
            account_email: None,
            workspace_id: None,
            workspace_name: None,
        }
    }
}

fn config_path(dir: &Path, profile: &str) -> PathBuf {
    dir.join("configs").join(format!("{profile}.json"))
}

fn credentials_path(dir: &Path, profile: &str) -> PathBuf {
    dir.join("credentials").join(format!("{profile}.json"))
}

pub fn profile_exists(dir: &Path, profile: &str) -> bool {
    credentials_path(dir, profile).is_file() || config_path(dir, profile).is_file()
}

pub fn load_config(dir: &Path, profile: &str) -> Result<Option<ProfileConfig>> {
    let path = config_path(dir, profile);
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Ok(None);
    };
    serde_json::from_str(&text)
        .map(Some)
        .with_context(|| format!("parse {}", path.display()))
}

pub fn load_credentials(dir: &Path, profile: &str) -> Result<Option<ProfileCredentials>> {
    let path = credentials_path(dir, profile);
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Ok(None);
    };
    let c: ProfileCredentials =
        serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
    if !c.kind.is_empty() && c.kind != CREDENTIALS_TYPE_OAUTH_TOKEN {
        return Err(anyhow!(
            "{}: unknown credentials type {:?}",
            path.display(),
            c.kind
        ));
    }
    Ok(Some(c))
}

pub fn save_config(dir: &Path, profile: &str, cfg: &ProfileConfig) -> Result<()> {
    write_secret_file(&config_path(dir, profile), serde_json::to_vec_pretty(cfg)?)
}

pub fn save_credentials(dir: &Path, profile: &str, creds: &ProfileCredentials) -> Result<()> {
    write_secret_file(&credentials_path(dir, profile), serde_json::to_vec_pretty(creds)?)
}

pub fn set_active_profile(dir: &Path, profile: &str) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    write_atomic(&dir.join("active_config"), format!("{profile}\n").into_bytes(), 0o644)
}

/// Remove both files; clear `active_config` if it named this profile.
/// Returns whether anything was removed.
pub fn delete_profile(dir: &Path, profile: &str) -> Result<bool> {
    let mut removed = false;
    for p in [config_path(dir, profile), credentials_path(dir, profile)] {
        match std::fs::remove_file(&p) {
            Ok(()) => removed = true,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("remove {}", p.display())),
        }
    }
    let pointer = dir.join("active_config");
    if std::fs::read_to_string(&pointer).map(|s| s.trim() == profile).unwrap_or(false) {
        let _ = std::fs::remove_file(&pointer);
    }
    Ok(removed)
}

fn write_secret_file(path: &Path, bytes: Vec<u8>) -> Result<()> {
    let parent = path.parent().ok_or_else(|| anyhow!("no parent for {}", path.display()))?;
    std::fs::create_dir_all(parent)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
    }
    write_atomic(path, bytes, 0o600)
}

/// Temp file in the same directory, chmod, rename — never a torn read.
fn write_atomic(path: &Path, bytes: Vec<u8>, mode: u32) -> Result<()> {
    let parent = path.parent().ok_or_else(|| anyhow!("no parent for {}", path.display()))?;
    let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
    std::io::Write::write_all(&mut tmp, &bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(mode))?;
    }
    #[cfg(not(unix))]
    let _ = mode;
    tmp.persist(path)
        .map_err(|e| anyhow!("persist {}: {}", path.display(), e.error))?;
    Ok(())
}
```

`tempfile` is currently a dev-dependency only (`Cargo.toml:103`). Move it to `[dependencies]` (same version) since `write_atomic` is production code.

In `src/auth/mod.rs`, delete the private `anthropic_config_dir()` and replace its two call sites with `profile::config_dir()`.

- [ ] **Step 4: Run tests**

Run: `cargo test --lib auth::profile 2>&1 | tail -3`
Expected: 7 passed.

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml Cargo.lock src/auth/mod.rs src/auth/profile.rs
git commit -m "auth: profile files compatible with the ant CLI and SDKs

configs/<p>.json, credentials/<p>.json, and active_config, with the wire
shapes from anthropic-sdk-go. 0600 files in 0700 dirs, atomic writes,
unknown fields ignored, wrong credential type fails loud.

Co-Authored-By: Arch Linux <noreply@archlinux.org>"
```

### Task 3: Token refresh (`auth/oauth.rs`, part A)

**Files:**
- Create: `src/auth/oauth.rs`
- Modify: `src/auth/mod.rs` (add `pub mod oauth;`)

**Interfaces:**
- Consumes: `profile::ProfileCredentials`
- Produces:
  - constants `CLIENT_ID`, `CONSOLE_URL`, `API_BASE`, `SCOPE`, `OAUTH_BETA` (re-export of `super::OAUTH_BETA`), `REFRESH_THRESHOLD_SECS: i64 = 120`
  - `pub fn client_id() -> String` (env override)
  - `pub struct TokenResponse { access_token, refresh_token: Option<String>, expires_in: i64, scope: Option<String>, organization: Option<TokenOrg>, account: Option<TokenAccount>, workspace: Option<TokenWorkspace> }`
  - `impl TokenResponse { pub fn into_credentials(self, now: i64) -> ProfileCredentials }`
  - `pub fn needs_refresh(expires_at: Option<i64>, now: i64) -> bool`
  - `pub async fn refresh_access_token(base_url: &str, client_id: &str, refresh_token: &str) -> Result<TokenResponse>`
  - `pub fn now_unix() -> i64`

- [ ] **Step 1: Write the failing tests**

Create `src/auth/oauth.rs` with the test module:

```rust
//! Console OAuth for Anthropic: PKCE login, code exchange, refresh.
//! Parameters mirror the open-source `ant` CLI (`pkg/cmd/cmd_auth.go`).

#[cfg(test)]
mod refresh_tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// One-shot HTTP server: records the request, replies with `body`.
    pub(super) async fn capture_server(
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
        assert!(needs_refresh(None, 1000), "no expiry means we cannot trust it");
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
        assert!(req.to_lowercase().contains("anthropic-beta: oauth-2025-04-20"), "{req}");
        assert!(req.to_lowercase().contains("content-type: application/json"), "{req}");
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
        let err = refresh_access_token(&base, "cid", "rt-1").await.unwrap_err();
        let s = err.to_string();
        assert!(s.contains("400") && s.contains("invalid_grant"), "{s}");
    }
}
```

Add `pub mod oauth;` to `src/auth/mod.rs`.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib auth::oauth 2>&1 | grep -E "^error" | head -3`
Expected: compile errors (`needs_refresh` not found).

- [ ] **Step 3: Implement**

Prepend to `src/auth/oauth.rs`:

```rust
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
```

- [ ] **Step 4: Run tests**

Run: `cargo test --lib auth::oauth 2>&1 | tail -3`
Expected: 5 passed.

- [ ] **Step 5: Commit**

```bash
git add src/auth/mod.rs src/auth/oauth.rs
git commit -m "auth: OAuth token refresh against /v1/oauth/token

Refresh grant with the oauth-2025-04-20 beta, 120s advisory threshold,
token response mapped to profile credentials.

Co-Authored-By: Arch Linux <noreply@archlinux.org>"
```

### Task 4: `AuthHandle` and native profile resolution

**Files:**
- Modify: `src/auth/mod.rs` (replace the `ant` shell-out; add `AuthHandle`)

**Interfaces:**
- Consumes: `profile::*`, `oauth::{needs_refresh, refresh_access_token, now_unix, client_id, API_BASE}`
- Produces:
  - `CredentialSource::Profile(String)` replaces `AntProfile(Option<String>)`; `describe()` → `"OAuth profile '<name>'"`
  - `AuthEnv::profile_access_token() -> Option<String>` and `profile_present()` replace the `ant_*` methods
  - `#[derive(Clone)] pub struct AuthHandle` with:
    - `pub fn none() -> Self`, `pub fn static_credential(c: Credential) -> Self`, `pub fn profile(dir: PathBuf, name: String, cfg: Option<ProfileConfig>, creds: ProfileCredentials) -> Self`
    - `pub fn is_none(&self) -> bool`, `pub fn is_oauth(&self) -> bool`, `pub fn is_profile(&self) -> bool`
    - `pub fn snapshot(&self) -> Option<Credential>` (no refresh, no I/O)
    - `pub async fn credential(&self) -> Result<Credential>` (refresh when needed, persist)
    - `pub async fn force_refresh(&self) -> Result<Credential>`
    - `pub fn profile_info(&self) -> Option<ProfileInfo>` where `pub struct ProfileInfo { pub name: String, pub organization: Option<String>, pub email: Option<String>, pub expires_at: Option<i64> }`
  - `pub fn load_profile_handle() -> Option<AuthHandle>` (real env + disk; `None` when no profile)
  - `impl std::fmt::Debug for AuthHandle` prints kind only, never the secret.

- [ ] **Step 1: Update the existing tests and add new ones**

In the `tests` module of `src/auth/mod.rs`, rename in `FakeEnv`: field `ant_token` → `profile_token`, method `with_ant` → `with_profile`, trait methods `ant_access_token` → `profile_access_token`, `ant_profile_present` → `profile_present`. Update every test that references `CredentialSource::AntProfile(..)`:

```rust
    #[test]
    fn ant_profile_is_third() {
        let r = resolve_with(&FakeEnv::default().with_profile("tok")).unwrap();
        assert_eq!(r.credential, Credential::OAuth("tok".into()));
        assert_eq!(r.source, CredentialSource::Profile("default".into()));
    }

    #[test]
    fn profile_name_is_recorded_when_set() {
        let r = resolve_with(
            &FakeEnv::default()
                .with("ANTHROPIC_PROFILE", "work")
                .with_profile("tok"),
        )
        .unwrap();
        assert_eq!(r.source, CredentialSource::Profile("work".into()));
        assert_eq!(r.source.describe(), "OAuth profile 'work'");
    }
```

Add a new test module at the end of `src/auth/mod.rs`:

```rust
#[cfg(test)]
mod handle_tests {
    use super::*;
    use crate::auth::profile::*;

    #[test]
    fn static_handle_never_refreshes_and_snapshots_itself() {
        let h = AuthHandle::static_credential(Credential::ApiKey("sk-ant-1".into()));
        assert!(!h.is_oauth() && !h.is_profile() && !h.is_none());
        assert_eq!(h.snapshot(), Some(Credential::ApiKey("sk-ant-1".into())));
        let h = AuthHandle::static_credential(Credential::OAuth("tok".into()));
        assert!(h.is_oauth());
        assert!(AuthHandle::none().is_none());
        assert_eq!(AuthHandle::none().snapshot(), None);
    }

    #[test]
    fn profile_handle_exposes_info_without_the_secret() {
        let d = tempfile::tempdir().unwrap();
        let mut creds = ProfileCredentials::new("at", Some("rt".into()), Some(9_999_999_999));
        creds.organization_name = Some("Kubereva".into());
        creds.account_email = Some("a@example.com".into());
        let h = AuthHandle::profile(d.path().to_path_buf(), "default".into(), None, creds);
        assert!(h.is_oauth() && h.is_profile());
        let info = h.profile_info().unwrap();
        assert_eq!(info.name, "default");
        assert_eq!(info.organization.as_deref(), Some("Kubereva"));
        assert_eq!(info.email.as_deref(), Some("a@example.com"));
        assert_eq!(info.expires_at, Some(9_999_999_999));
        assert!(!format!("{h:?}").contains("at"), "debug must not leak the token");
        assert_eq!(h.snapshot(), Some(Credential::OAuth("at".into())));
    }

    #[tokio::test]
    async fn profile_handle_refreshes_and_rewrites_the_file_when_stale() {
        let (base, _, hits) = crate::auth::oauth::refresh_tests::capture_server(
            "HTTP/1.1 200 OK",
            r#"{"access_token":"at-new","refresh_token":"rt-new","expires_in":3600}"#,
        )
        .await;
        let d = tempfile::tempdir().unwrap();
        let creds = ProfileCredentials::new("at-old", Some("rt-old".into()), Some(1)); // long expired
        save_credentials(d.path(), "default", &creds).unwrap();
        let h = AuthHandle::profile(d.path().to_path_buf(), "default".into(), None, creds)
            .with_base_url(base);
        let c = h.credential().await.unwrap();
        assert_eq!(c, Credential::OAuth("at-new".into()));
        let on_disk = load_credentials(d.path(), "default").unwrap().unwrap();
        assert_eq!(on_disk.access_token, "at-new");
        assert_eq!(on_disk.refresh_token.as_deref(), Some("rt-new"));
        // A second call is served from memory: no second HTTP hit.
        let _ = h.credential().await.unwrap();
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn profile_without_refresh_token_says_to_login() {
        let d = tempfile::tempdir().unwrap();
        let creds = ProfileCredentials::new("at-old", None, Some(1));
        let h = AuthHandle::profile(d.path().to_path_buf(), "default".into(), None, creds);
        let err = h.credential().await.unwrap_err().to_string();
        assert!(err.contains("/login"), "{err}");
    }
}
```

Make `refresh_tests` and `capture_server` reachable: in `oauth.rs` change `mod refresh_tests` to `pub(crate) mod refresh_tests` (still `#[cfg(test)]`).

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib auth:: 2>&1 | grep -E "^error" | head -3`
Expected: compile errors (`AuthHandle`, `with_profile` not found).

- [ ] **Step 3: Implement**

In `src/auth/mod.rs`:

1. Replace the module doc's "Why shell out to `ant`" paragraph with:

```rust
//! **Profiles are read natively.** `ant auth login` (and OxideClaw's own
//! `/login`) store `credentials/<profile>.json` under the Anthropic config
//! dir. We read that file directly and refresh the short-lived token
//! ourselves (see [`oauth`]), so the `ant` binary is never required. This
//! also sidesteps the Apache Ant name collision on PATH.
```

2. Replace the `CredentialSource` variant and `describe`:

```rust
pub enum CredentialSource {
    ApiKeyEnv,
    AuthTokenEnv,
    /// `credentials/<profile>.json` under the Anthropic config dir.
    Profile(String),
}

impl CredentialSource {
    pub fn describe(&self) -> String {
        match self {
            CredentialSource::ApiKeyEnv => "ANTHROPIC_API_KEY".into(),
            CredentialSource::AuthTokenEnv => "ANTHROPIC_AUTH_TOKEN".into(),
            CredentialSource::Profile(p) => format!("OAuth profile '{p}'"),
        }
    }
}
```

3. Replace the `AuthEnv` trait methods:

```rust
pub trait AuthEnv {
    fn var(&self, key: &str) -> Option<String>;
    /// Access token from the active profile's credentials file (no refresh).
    fn profile_access_token(&self) -> Option<String>;
    /// Whether any profile exists — used only for the shadowing warning.
    fn profile_present(&self) -> bool {
        false
    }
}
```

In `resolve_stage`, the shadow warning text becomes `"ANTHROPIC_API_KEY is shadowing your OAuth profile — requests will use the key's org/workspace, not the profile's. Unset the variable to use the profile."` and the profile branch becomes:

```rust
    if allow_profile && let Some(token) = non_empty(env.profile_access_token()) {
        let name = profile.unwrap_or_else(|| "default".to_string());
        return Some(Resolved {
            credential: Credential::OAuth(token),
            source: CredentialSource::Profile(name),
            warnings,
        });
    }
```

4. Replace `resolve_profile`, `ProcessAuthEnv`'s impl, `ANT_TIMEOUT`, `ant_profile_dir_exists`, `profile_dir_exists_at`, and `run_ant` with:

```rust
/// The active profile as a refreshing handle, or `None` when no profile exists.
pub fn load_profile_handle() -> Option<AuthHandle> {
    let dir = profile::config_dir()?;
    let name = profile::resolve_profile_name(dir.as_path(), std::env::var("ANTHROPIC_PROFILE").ok().as_deref());
    let creds = match profile::load_credentials(&dir, &name) {
        Ok(Some(c)) => c,
        Ok(None) => return None,
        Err(e) => {
            tracing::warn!("ignoring unreadable profile '{name}': {e}");
            return None;
        }
    };
    let cfg = profile::load_config(&dir, &name).ok().flatten();
    Some(AuthHandle::profile(dir, name, cfg, creds))
}

/// The real environment: process env vars plus profile files on disk.
pub struct ProcessAuthEnv;

impl AuthEnv for ProcessAuthEnv {
    fn var(&self, key: &str) -> Option<String> {
        std::env::var(key).ok()
    }
    fn profile_access_token(&self) -> Option<String> {
        load_profile_handle().and_then(|h| h.snapshot()).map(|c| c.secret().to_string())
    }
    fn profile_present(&self) -> bool {
        profile::config_dir().is_some_and(|d| d.join("credentials").is_dir())
    }
}

/// Profile stage, as a handle (so refresh works), against the real environment.
pub fn resolve_profile() -> Option<(Resolved, AuthHandle)> {
    let h = load_profile_handle()?;
    let info = h.profile_info()?;
    let token = h.snapshot()?;
    Some((
        Resolved {
            credential: token,
            source: CredentialSource::Profile(info.name),
            warnings: Vec::new(),
        },
        h,
    ))
}
```

5. Add `AuthHandle` after `Resolved`:

```rust
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// Details for `/doctor` and the login board. Never contains a secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileInfo {
    pub name: String,
    pub organization: Option<String>,
    pub email: Option<String>,
    pub expires_at: Option<i64>,
}

struct ProfileState {
    dir: PathBuf,
    name: String,
    client_id: String,
    base_url: String,
    /// In-memory copy; std mutex so `snapshot()` is sync and cheap.
    creds: Mutex<profile::ProfileCredentials>,
    /// Serialises refreshes so concurrent requests share one round trip.
    refresh_gate: tokio::sync::Mutex<()>,
}

enum AuthInner {
    None,
    Static(Credential),
    Profile(ProfileState),
}

/// A cloneable credential source consulted per request. A static key is
/// returned as-is; a profile refreshes itself when under the threshold.
#[derive(Clone)]
pub struct AuthHandle {
    inner: Arc<AuthInner>,
}

impl Default for AuthHandle {
    fn default() -> Self {
        Self::none()
    }
}

impl std::fmt::Debug for AuthHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &*self.inner {
            AuthInner::None => write!(f, "AuthHandle(none)"),
            AuthInner::Static(Credential::ApiKey(_)) => write!(f, "AuthHandle(api-key)"),
            AuthInner::Static(Credential::OAuth(_)) => write!(f, "AuthHandle(oauth-token)"),
            AuthInner::Profile(p) => write!(f, "AuthHandle(profile '{}')", p.name),
        }
    }
}

impl AuthHandle {
    pub fn none() -> Self {
        Self { inner: Arc::new(AuthInner::None) }
    }

    pub fn static_credential(c: Credential) -> Self {
        Self { inner: Arc::new(AuthInner::Static(c)) }
    }

    pub fn profile(
        dir: PathBuf,
        name: String,
        cfg: Option<profile::ProfileConfig>,
        creds: profile::ProfileCredentials,
    ) -> Self {
        let client_id = cfg
            .as_ref()
            .and_then(|c| c.authentication.client_id.clone())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(oauth::client_id);
        let base_url = cfg
            .as_ref()
            .and_then(|c| c.base_url.clone())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| oauth::API_BASE.to_string());
        Self {
            inner: Arc::new(AuthInner::Profile(ProfileState {
                dir,
                name,
                client_id,
                base_url,
                creds: Mutex::new(creds),
                refresh_gate: tokio::sync::Mutex::new(()),
            })),
        }
    }

    /// Test seam: point the refresh at a local server.
    #[cfg(test)]
    pub(crate) fn with_base_url(self, base_url: String) -> Self {
        match Arc::try_unwrap(self.inner) {
            Ok(AuthInner::Profile(mut p)) => {
                p.base_url = base_url;
                Self { inner: Arc::new(AuthInner::Profile(p)) }
            }
            Ok(other) => Self { inner: Arc::new(other) },
            Err(arc) => Self { inner: arc },
        }
    }

    pub fn is_none(&self) -> bool {
        matches!(&*self.inner, AuthInner::None)
    }

    pub fn is_profile(&self) -> bool {
        matches!(&*self.inner, AuthInner::Profile(_))
    }

    pub fn is_oauth(&self) -> bool {
        match &*self.inner {
            AuthInner::None => false,
            AuthInner::Static(c) => c.is_oauth(),
            AuthInner::Profile(_) => true,
        }
    }

    /// Current credential without refreshing. Sync and lock-cheap.
    pub fn snapshot(&self) -> Option<Credential> {
        match &*self.inner {
            AuthInner::None => None,
            AuthInner::Static(c) => Some(c.clone()),
            AuthInner::Profile(p) => {
                let creds = p.creds.lock().unwrap_or_else(|e| e.into_inner());
                Some(Credential::OAuth(creds.access_token.clone()))
            }
        }
    }

    pub fn profile_info(&self) -> Option<ProfileInfo> {
        let AuthInner::Profile(p) = &*self.inner else {
            return None;
        };
        let c = p.creds.lock().unwrap_or_else(|e| e.into_inner());
        Some(ProfileInfo {
            name: p.name.clone(),
            organization: c.organization_name.clone(),
            email: c.account_email.clone(),
            expires_at: c.expires_at,
        })
    }

    /// Credential for the next request, refreshing a stale profile first.
    pub async fn credential(&self) -> anyhow::Result<Credential> {
        match &*self.inner {
            AuthInner::None => Err(anyhow::anyhow!(
                "No Anthropic credential. Run /login, or set ANTHROPIC_API_KEY."
            )),
            AuthInner::Static(c) => Ok(c.clone()),
            AuthInner::Profile(p) => {
                let stale = {
                    let c = p.creds.lock().unwrap_or_else(|e| e.into_inner());
                    oauth::needs_refresh(c.expires_at, oauth::now_unix())
                };
                if stale {
                    self.refresh_profile(p, false).await?;
                }
                Ok(self.snapshot().expect("profile always has a token"))
            }
        }
    }

    /// Refresh regardless of expiry (after a 401). Static handles are a no-op.
    pub async fn force_refresh(&self) -> anyhow::Result<Credential> {
        if let AuthInner::Profile(p) = &*self.inner {
            self.refresh_profile(p, true).await?;
        }
        self.credential().await
    }

    async fn refresh_profile(&self, p: &ProfileState, force: bool) -> anyhow::Result<()> {
        let _gate = p.refresh_gate.lock().await;
        // Another request may have refreshed while we waited for the gate.
        let (refresh_token, still_stale) = {
            let c = p.creds.lock().unwrap_or_else(|e| e.into_inner());
            (
                c.refresh_token.clone(),
                force || oauth::needs_refresh(c.expires_at, oauth::now_unix()),
            )
        };
        if !still_stale {
            return Ok(());
        }
        let Some(rt) = refresh_token.filter(|r| !r.is_empty()) else {
            return Err(anyhow::anyhow!(
                "OAuth profile '{}' has expired and stores no refresh token. Run /login.",
                p.name
            ));
        };
        let tok = oauth::refresh_access_token(&p.base_url, &p.client_id, &rt)
            .await
            .map_err(|e| anyhow::anyhow!("{e}. Run /login to sign in again."))?;
        let mut fresh = tok.into_credentials(oauth::now_unix());
        {
            let mut c = p.creds.lock().unwrap_or_else(|e| e.into_inner());
            // Server may omit metadata on refresh; keep what we had.
            if fresh.refresh_token.is_none() {
                fresh.refresh_token = c.refresh_token.clone();
            }
            if fresh.organization_name.is_none() {
                fresh.organization_name = c.organization_name.clone();
                fresh.organization_uuid = c.organization_uuid.clone();
            }
            if fresh.account_email.is_none() {
                fresh.account_email = c.account_email.clone();
            }
            if fresh.workspace_id.is_none() {
                fresh.workspace_id = c.workspace_id.clone();
                fresh.workspace_name = c.workspace_name.clone();
            }
            *c = fresh.clone();
        }
        if let Err(e) = profile::save_credentials(&p.dir, &p.name, &fresh) {
            tracing::warn!("refreshed token could not be saved to disk: {e}");
        }
        Ok(())
    }
}
```

6. Delete the `use std::time::{Duration, Instant};` import if nothing else uses it, and the `#[allow(dead_code)] pub fn resolve()` stays as-is.

- [ ] **Step 4: Run tests**

Run: `cargo test --lib auth:: 2>&1 | tail -3 && cargo build 2>&1 | grep -E "^error" | head`
Expected: all auth tests pass. The build will fail in `config.rs` and `status.rs` on `resolve_profile()`'s new return type and `CredentialSource::AntProfile`; fix them minimally now:

In `src/config.rs` lines 771-782 replace the block with:

```rust
        // ── Last resort: the active OAuth profile on disk (written by
        //    `/login` or `ant auth login`). Read natively; refresh is handled
        //    by the AuthHandle at request time.
        if cfg.api_key.is_empty()
            && let Some((resolved, _handle)) = crate::auth::resolve_profile()
        {
            cfg.auth_is_oauth = resolved.credential.is_oauth();
            cfg.auth_source = Some(resolved.source.describe());
            cfg.api_key = resolved.credential.secret().to_string();
        }
```

In `src/commands/status.rs:193` change the message to `"✗ No Anthropic credential — run /login, or set ANTHROPIC_API_KEY"`.

Run: `cargo test 2>&1 | grep -E "^test result|FAILED" && cargo clippy --all-targets 2>&1 | grep -cE "^(warning|error)"`
Expected: every `test result: ok`; clippy count `0`.

- [ ] **Step 5: Commit**

```bash
git add src/auth/mod.rs src/auth/oauth.rs src/config.rs src/commands/status.rs
git commit -m "auth: read OAuth profiles natively; AuthHandle with self-refresh

Replaces the shell-out to ant auth print-credentials. AuthHandle hands
out the current token per request, refreshing under the 120s threshold
and rewriting credentials/<profile>.json.

Co-Authored-By: Arch Linux <noreply@archlinux.org>"
```

### Task 5: `ClaudeClient` authenticates per request through `AuthHandle`

**Files:**
- Modify: `src/api/mod.rs:65-130` (struct, constructors), `:170-260` (`messages`, `messages_stream`), tests at `:516-552`

**Interfaces:**
- Consumes: `crate::auth::{AuthHandle, Credential, OAUTH_BETA}`
- Produces:
  - `ClaudeClient::with_auth(auth: AuthHandle) -> Result<Self>` (new primary constructor)
  - `ClaudeClient::new(key)` and `with_credential(&Credential)` remain, delegating to `with_auth`
  - private `async fn post_messages(&self, url: &str, request: &MessagesRequest, context: &str) -> Result<reqwest::Response>` used by both public methods; performs the 401 → refresh → single retry for profile handles.
  - `beta_header()` unchanged in behaviour (oauth beta added when `auth.is_oauth()`).

- [ ] **Step 1: Write the failing tests**

Replace the existing test module `mod tests` in `src/api/mod.rs` (lines 516-552) body's three tests with these, keeping the module name:

```rust
#[cfg(test)]
mod auth_header_tests {
    use super::*;
    use crate::auth::{AuthHandle, Credential, OAUTH_BETA};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn oauth_credential_adds_the_required_beta() {
        let c = ClaudeClient::with_credential(&Credential::OAuth("tok".into())).unwrap();
        assert_eq!(c.beta_header(&[]).as_deref(), Some(OAUTH_BETA));
    }

    #[test]
    fn api_key_credential_adds_no_beta() {
        let c = ClaudeClient::new("sk-ant-test").unwrap();
        assert_eq!(c.beta_header(&[]), None);
    }

    #[test]
    fn oauth_beta_merges_with_request_betas_without_duplicates() {
        let c = ClaudeClient::with_credential(&Credential::OAuth("tok".into())).unwrap();
        let merged = c.beta_header(&["compact-2026-01-12".into()]).unwrap();
        assert!(merged.contains(OAUTH_BETA), "{merged}");
        assert!(merged.contains("compact-2026-01-12"), "{merged}");
        assert_eq!(merged.matches(OAUTH_BETA).count(), 1, "{merged}");
        let merged = c.beta_header(&[OAUTH_BETA.into()]).unwrap();
        assert_eq!(merged, OAUTH_BETA, "duplicate must collapse: {merged}");
    }

    /// Scripted server: each accepted connection gets the next response and
    /// the raw request is stored for inspection.
    async fn scripted(
        script: Vec<&'static str>,
    ) -> (String, Arc<tokio::sync::Mutex<Vec<String>>>, Arc<AtomicUsize>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let hits = Arc::new(AtomicUsize::new(0));
        let (seen2, hits2) = (seen.clone(), hits.clone());
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let i = hits2.fetch_add(1, Ordering::SeqCst);
                let body = *script.get(i).unwrap_or_else(|| script.last().unwrap());
                let seen3 = seen2.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 16384];
                    let n = sock.read(&mut buf).await.unwrap_or(0);
                    seen3.lock().await.push(String::from_utf8_lossy(&buf[..n]).to_string());
                    let _ = sock.write_all(body.as_bytes()).await;
                    let _ = sock.shutdown().await;
                });
            }
        });
        (format!("http://{addr}"), seen, hits)
    }

    const UNAUTHORIZED: &str =
        "HTTP/1.1 401 Unauthorized\r\ncontent-type: application/json\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}";
    const TOKEN_OK: &str = "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 66\r\nconnection: close\r\n\r\n{\"access_token\":\"at-new\",\"refresh_token\":\"rt-new\",\"expires_in\":3600}";
    const MSG_OK: &str = "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 121\r\nconnection: close\r\n\r\n{\"id\":\"m\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"x\",\"content\":[],\"stop_reason\":\"end_turn\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}";

    fn req() -> MessagesRequest {
        MessagesRequest {
            model: "claude-opus-5".into(),
            max_tokens: 8,
            messages: vec![],
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn api_key_goes_in_x_api_key_on_every_request() {
        let (base, seen, _) = scripted(vec![MSG_OK]).await;
        let mut c = ClaudeClient::new("sk-ant-test").unwrap();
        c.set_base_url_for_test(base);
        c.messages(req()).await.unwrap();
        let r = seen.lock().await[0].to_lowercase();
        assert!(r.contains("x-api-key: sk-ant-test"), "{r}");
        assert!(!r.contains("authorization:"), "{r}");
    }

    #[tokio::test]
    async fn a_401_on_a_profile_forces_one_refresh_and_one_retry() {
        // Same server answers both /v1/messages and /v1/oauth/token, in order:
        // messages → 401, token → 200, messages → 200.
        let (base, seen, hits) = scripted(vec![UNAUTHORIZED, TOKEN_OK, MSG_OK]).await;
        let d = tempfile::tempdir().unwrap();
        let creds = crate::auth::profile::ProfileCredentials::new(
            "at-old",
            Some("rt-old".into()),
            Some(9_999_999_999),
        );
        let handle = AuthHandle::profile(d.path().to_path_buf(), "default".into(), None, creds)
            .with_base_url(base.clone());
        let mut c = ClaudeClient::with_auth(handle).unwrap();
        c.set_base_url_for_test(base);
        c.messages(req()).await.unwrap();
        assert_eq!(hits.load(Ordering::SeqCst), 3);
        let seen = seen.lock().await;
        assert!(seen[0].to_lowercase().contains("authorization: bearer at-old"), "{}", seen[0]);
        assert!(seen[1].starts_with("POST /v1/oauth/token"), "{}", seen[1]);
        assert!(seen[2].to_lowercase().contains("authorization: bearer at-new"), "{}", seen[2]);
        assert!(seen[2].to_lowercase().contains("anthropic-beta: oauth-2025-04-20"), "{}", seen[2]);
    }

    #[tokio::test]
    async fn a_401_on_a_static_key_is_not_retried() {
        let (base, _, hits) = scripted(vec![UNAUTHORIZED]).await;
        let mut c = ClaudeClient::new("sk-ant-test").unwrap();
        c.set_base_url_for_test(base);
        let err = c.messages(req()).await.unwrap_err().to_string();
        assert!(err.contains("401"), "{err}");
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }
}
```

If `MessagesRequest` does not implement `Default`, add `#[derive(Default)]` to it in `src/api/types.rs` (every field is `Option`, `Vec`, `String`, or a number). Check with `grep -n "pub struct MessagesRequest" -B3 src/api/types.rs`.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib api::auth_header_tests 2>&1 | grep -E "^error" | head -3`
Expected: compile error (`with_auth` not found).

- [ ] **Step 3: Implement**

In `src/api/mod.rs`:

1. Struct fields: replace `api_key: String` and `credential_betas: Vec<String>` with `auth: crate::auth::AuthHandle`. Remove the `#[allow(dead_code)]` above the struct.

2. Constructors:

```rust
impl ClaudeClient {
    /// Construct from a static API key.
    pub fn new(api_key: impl Into<String>) -> Result<Self> {
        Self::with_credential(&crate::auth::Credential::ApiKey(api_key.into()))
    }

    /// Construct from a resolved static credential.
    pub fn with_credential(cred: &crate::auth::Credential) -> Result<Self> {
        Self::with_auth(crate::auth::AuthHandle::static_credential(cred.clone()))
    }

    /// Construct from a credential handle. Authentication headers are set per
    /// request (never as client defaults) so a refreshed profile token is
    /// picked up without rebuilding the client.
    pub fn with_auth(auth: crate::auth::AuthHandle) -> Result<Self> {
        let mut headers = header::HeaderMap::new();
        headers.insert("anthropic-version", ANTHROPIC_VERSION.parse()?);
        headers.insert(header::CONTENT_TYPE, "application/json".parse()?);
        let client = Client::builder()
            .default_headers(headers)
            .connect_timeout(std::time::Duration::from_secs(10))
            .build()
            .context("Failed to build HTTP client")?;
        Ok(Self {
            client,
            auth,
            base_url: ANTHROPIC_API_BASE.to_string(),
            retry_notifier: None,
        })
    }
```

Keep the existing comment about the connect timeout above `.connect_timeout`.

3. `beta_header`: replace `self.credential_betas.iter()` with a local:

```rust
    fn beta_header(&self, request_betas: &[String]) -> Option<String> {
        let credential_betas: &[&str] = if self.auth.is_oauth() {
            &[crate::auth::OAUTH_BETA]
        } else {
            &[]
        };
        if request_betas.is_empty() && credential_betas.is_empty() {
            return None;
        }
        let mut all: Vec<&str> = Vec::new();
        for b in request_betas
            .iter()
            .map(String::as_str)
            .chain(credential_betas.iter().copied())
        {
            if !b.is_empty() && !all.contains(&b) {
                all.push(b);
            }
        }
        if all.is_empty() { None } else { Some(all.join(",")) }
    }
```

4. Add the shared sender and use it from both public methods:

```rust
    fn apply_auth(
        builder: reqwest::RequestBuilder,
        cred: &crate::auth::Credential,
    ) -> reqwest::RequestBuilder {
        match cred {
            crate::auth::Credential::ApiKey(k) => builder.header("x-api-key", k.as_str()),
            crate::auth::Credential::OAuth(t) => builder.bearer_auth(t),
        }
    }

    /// POST `/v1/messages` with the current credential. A 401 on a profile
    /// credential forces one refresh and one retry; anything else is returned
    /// to the caller as-is.
    async fn post_messages(
        &self,
        url: &str,
        request: &MessagesRequest,
        context: &str,
    ) -> Result<reqwest::Response> {
        let betas = self.beta_header(&request.betas);
        let mut cred = self.auth.credential().await?;
        for attempt in 0..2 {
            let resp = retry::send_with_retry(
                || {
                    let mut b = Self::apply_auth(self.client.post(url).json(request), &cred);
                    if let Some(ref b2) = betas {
                        b = b.header("anthropic-beta", b2.as_str());
                    }
                    if let Some(ref sid) = request.session_id {
                        b = b.header("X-Claude-Code-Session-Id", sid.as_str());
                    }
                    b
                },
                self.retry_notifier.as_ref(),
                context,
            )
            .await?;
            if resp.status() == reqwest::StatusCode::UNAUTHORIZED
                && self.auth.is_profile()
                && attempt == 0
            {
                debug!("401 on profile credential — refreshing and retrying once");
                cred = self.auth.force_refresh().await?;
                continue;
            }
            return Ok(resp);
        }
        unreachable!("loop returns on the second attempt")
    }
```

`messages` becomes:

```rust
    pub async fn messages(&self, request: MessagesRequest) -> Result<MessagesResponse> {
        let url = format!("{}/v1/messages", self.base_url);
        debug!("POST {url} model={}", request.model);
        let resp = self.post_messages(&url, &request, "API request failed").await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!("API error {status}: {body}"));
        }
        resp.json::<MessagesResponse>()
            .await
            .context("Failed to parse API response")
    }
```

`messages_stream`: replace its `let betas = …; let resp = retry::send_with_retry(...).await?;` block with `let resp = self.post_messages(&url, &request, "Streaming API request failed").await?;` (after `request.stream = Some(true);`). The rest of the function is unchanged.

Search the file for any other use of `self.api_key` or `credential_betas` and remove it (`grep -n "api_key\|credential_betas" src/api/mod.rs`).

- [ ] **Step 4: Run tests**

Run: `cargo test --lib api:: 2>&1 | grep -E "^test result|FAILED|panicked"`
Expected: all ok, including the three new async tests.

- [ ] **Step 5: Commit**

```bash
git add src/api/mod.rs src/api/types.rs
git commit -m "api: authenticate per request via AuthHandle; 401 refreshes a profile once

Auth headers move off the reqwest defaults so a refreshed OAuth token is
used without rebuilding the client. Static keys are unchanged.

Co-Authored-By: Arch Linux <noreply@archlinux.org>"
```

### Task 6: `Config.auth`, `ApiBackend::from_config`, doctor profile line

**Files:**
- Modify: `src/config.rs:120-142` (fields), `:405-412` (Default), `:705-782` (resolution)
- Modify: `src/api/mod.rs:393-422` (`ApiBackend`)
- Modify: `src/tui/run.rs:204`, `:624`; `src/tui/run/dispatch.rs:138`; `src/query_engine.rs:64`; `src/sdk/session.rs:99`
- Modify: `src/commands/status.rs:170-195`

**Interfaces:**
- Produces:
  - `Config.auth: crate::auth::AuthHandle` (serde-skipped, default none). `Config.api_key` / `auth_is_oauth` / `auth_source` stay as a startup snapshot for tools and status.
  - `Config::resolve_anthropic_auth(&mut self)` — re-runs the whole chain (env → fd → helper → profile) and sets `auth`, `api_key`, `auth_is_oauth`, `auth_source`, `auth_warnings`. Called by `load()` and later by the `CredentialChanged` handler.
  - `ApiBackend::from_config(config: &Config) -> Result<Self>`.

- [ ] **Step 1: Write the failing test**

Append to the `tests` module in `src/config.rs` (find it with `grep -n "^mod tests" src/config.rs`; create `#[cfg(test)] mod auth_tests` at the end of the file if there is none):

```rust
#[cfg(test)]
mod auth_handle_tests {
    use super::*;

    #[test]
    fn default_config_has_no_auth_handle() {
        let c = Config::default();
        assert!(c.auth.is_none());
        assert!(c.api_key.is_empty());
    }

    #[test]
    fn from_config_builds_an_anthropic_backend_from_the_handle() {
        let mut c = Config::default();
        c.model = "claude-opus-5".into();
        c.auth = crate::auth::AuthHandle::static_credential(crate::auth::Credential::OAuth(
            "tok".into(),
        ));
        let b = crate::api::ApiBackend::from_config(&c).unwrap();
        assert!(matches!(b, crate::api::ApiBackend::Anthropic(_)));
    }

    #[test]
    fn from_config_builds_ollama_without_any_credential() {
        let mut c = Config::default();
        c.model = "ollama:llama3".into();
        let b = crate::api::ApiBackend::from_config(&c).unwrap();
        assert!(matches!(b, crate::api::ApiBackend::Ollama(_)));
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib config::auth_handle_tests 2>&1 | grep -E "^error" | head -3`
Expected: compile errors (`auth` field, `from_config`).

- [ ] **Step 3: Implement**

`src/config.rs`:

1. After the `auth_warnings` field add:

```rust
    /// Live credential source for the Anthropic client. A profile handle
    /// refreshes itself; `api_key` above is only the startup snapshot.
    #[serde(skip)]
    pub auth: crate::auth::AuthHandle,
```

and in `impl Default for Config` add `auth: crate::auth::AuthHandle::none(),`.

2. Extract the credential section of `load()` (from the `// ── Credential from the environment` comment through the profile fallback block) into a method, and call it from `load()` at the same spot with `cfg.resolve_anthropic_auth();`:

```rust
    /// Resolve the Anthropic credential chain into `auth` and the snapshot
    /// fields. Safe to call again mid-session (after /login or /logout).
    ///
    ///   ANTHROPIC_API_KEY → ANTHROPIC_AUTH_TOKEN → key fd → apiKeyHelper → OAuth profile
    pub fn resolve_anthropic_auth(&mut self) {
        self.api_key.clear();
        self.auth_is_oauth = false;
        self.auth_source = None;
        self.auth_warnings.clear();
        self.auth = crate::auth::AuthHandle::none();

        if let Some(resolved) = crate::auth::resolve_env() {
            self.auth_is_oauth = resolved.credential.is_oauth();
            self.auth_source = Some(resolved.source.describe());
            self.auth_warnings = resolved.warnings;
            self.api_key = resolved.credential.secret().to_string();
            self.auth = crate::auth::AuthHandle::static_credential(resolved.credential);
        }

        // (existing OXIDECLAW_API_KEY_FILE_DESCRIPTOR block, unchanged, but
        //  after `self.api_key = …` add:)
        //      self.auth = crate::auth::AuthHandle::static_credential(
        //          crate::auth::Credential::ApiKey(self.api_key.clone()));
        // (existing apiKeyHelper block, unchanged, same addition)

        if self.api_key.is_empty()
            && let Some((resolved, handle)) = crate::auth::resolve_profile()
        {
            self.auth_is_oauth = true;
            self.auth_source = Some(resolved.source.describe());
            self.api_key = resolved.credential.secret().to_string();
            self.auth = handle;
        }
    }
```

Move the two existing blocks (fd and helper) verbatim into the method body between the env and profile stages, replacing `cfg.` with `self.`, and add the `self.auth = …static_credential(…)` line inside each after the key is set.

`src/api/mod.rs`, in `impl ApiBackend`:

```rust
    /// The right backend for `config.model`, using the live credential handle
    /// for Anthropic and the process environment for other providers.
    pub fn from_config(config: &crate::config::Config) -> Result<Self> {
        let model = config.model.as_str();
        if is_ollama_model(model) {
            Ok(Self::Ollama(OllamaClient::new(&config.ollama_host)?))
        } else if is_openai_compat_model(model) {
            Ok(Self::OpenAiCompat(OpenAiCompatClient::from_model(model)?))
        } else {
            Ok(Self::Anthropic(ClaudeClient::with_auth(config.auth.clone())?))
        }
    }
```

Keep `new_with_auth` and `new` unchanged (tests and the SDK use them).

Replace the five `ApiBackend::new_with_auth(&config.model, &config.api_key, config.auth_is_oauth, &config.ollama_host)` call sites with `ApiBackend::from_config(&config)` (in `query_engine.rs` and `sdk/session.rs` the variable is `config`; in `run.rs` and `dispatch.rs` it is `config` too). Use `grep -rn "new_with_auth(" src --include=*.rs | grep -v "src/api/mod.rs"` to find them all.

`src/commands/status.rs`, after the `✓ Anthropic credential:` push, add:

```rust
        if let Some(info) = ctx.config.auth.profile_info() {
            let who = match (&info.email, &info.organization) {
                (Some(e), Some(o)) => format!("{e} · org {o}"),
                (Some(e), None) => e.clone(),
                (None, Some(o)) => format!("org {o}"),
                (None, None) => "(no account details stored)".into(),
            };
            let expiry = match info.expires_at {
                Some(t) => {
                    let left = t - crate::auth::oauth::now_unix();
                    if left <= 0 {
                        "expired (refreshes on next request)".to_string()
                    } else {
                        format!("expires in {} min", left / 60)
                    }
                }
                None => "no expiry recorded".to_string(),
            };
            checks.push(format!("  profile '{}': {who} · {expiry}", info.name));
        }
```

- [ ] **Step 4: Run everything**

Run: `cargo test 2>&1 | grep -E "^test result|FAILED|panicked" && cargo clippy --all-targets 2>&1 | grep -cE "^(warning|error)"`
Expected: all ok; `0`.

Manual check: with no `ANTHROPIC_API_KEY` exported and a profile written by `ant auth login` on another machine copied into `~/.config/anthropic/`, `oxideclaw` starts and `/doctor` shows `via OAuth profile 'default'` plus the profile line.

- [ ] **Step 5: Commit**

```bash
git add src/config.rs src/api/mod.rs src/tui/run.rs src/tui/run/dispatch.rs src/query_engine.rs src/sdk/session.rs src/commands/status.rs
git commit -m "config: live AuthHandle; ApiBackend::from_config; doctor shows the profile

Increment 1 complete: OAuth profiles work without the ant binary and
refresh mid-session.

Co-Authored-By: Arch Linux <noreply@archlinux.org>"
```

---

## Increment 2: `/login` and `/logout` for Anthropic

### Task 7: PKCE, authorize URL, callback parsing (`auth/oauth.rs`, part B)

**Files:**
- Modify: `src/auth/oauth.rs`

**Interfaces:**
- Produces:
  - `pub fn pkce_verifier() -> String`, `pub fn pkce_challenge_s256(verifier: &str) -> String`, `pub fn random_state() -> String`
  - `pub struct AuthorizeParams<'a> { console_url, client_id, redirect_uri, scope, state, code_challenge: &'a str, org_uuid: Option<&'a str>, workspace_id: Option<&'a str> }`
  - `pub fn build_authorize_url(p: &AuthorizeParams) -> String`
  - `pub fn manual_redirect_uri(console_url: &str) -> String`
  - `pub fn parse_callback_query(query: &str, expected_state: &str) -> Result<String>`

- [ ] **Step 1: Write the failing tests**

Append to `src/auth/oauth.rs`:

```rust
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
            assert!(s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'), "{s}");
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
        assert_eq!(parse_callback_query("code=abc&state=st", "st").unwrap(), "abc");
        assert_eq!(parse_callback_query("state=st&code=a%2Bb", "st").unwrap(), "a+b");
        let e = parse_callback_query("code=abc&state=other", "st").unwrap_err().to_string();
        assert!(e.contains("state"), "{e}");
        let e = parse_callback_query("state=st", "st").unwrap_err().to_string();
        assert!(e.contains("code"), "{e}");
        let e = parse_callback_query(
            "error=access_denied&error_description=User%20declined&state=st",
            "st",
        )
        .unwrap_err()
        .to_string();
        assert!(e.contains("access_denied") && e.contains("User declined"), "{e}");
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib auth::oauth::pkce_tests 2>&1 | grep -E "^error" | head -3`
Expected: compile errors.

- [ ] **Step 3: Implement**

Add to `src/auth/oauth.rs` (below the refresh code):

```rust
use base64::Engine as _;
use sha2::{Digest, Sha256};

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
```

`rand::fill` exists in rand 0.9. If clippy complains about `expect` on the static URL, keep it: the input is a literal path.

- [ ] **Step 4: Run tests**

Run: `cargo test --lib auth::oauth 2>&1 | tail -3`
Expected: 11 passed.

- [ ] **Step 5: Commit**

```bash
git add src/auth/oauth.rs
git commit -m "auth: PKCE, authorize URL, and callback parsing for Console OAuth

Co-Authored-By: Arch Linux <noreply@archlinux.org>"
```

### Task 8: Loopback callback listener

**Files:**
- Modify: `src/auth/oauth.rs`

**Interfaces:**
- Consumes: `parse_callback_query`
- Produces:
  - `pub const CALLBACK_TIMEOUT: Duration = 300s`
  - `pub async fn bind_loopback() -> Result<(tokio::net::TcpListener, String)>` → listener and `http://localhost:<port>/callback`
  - `pub async fn wait_for_code(listener: tokio::net::TcpListener, expected_state: &str, timeout: Duration) -> Result<String>`

- [ ] **Step 1: Write the failing tests**

Append to `src/auth/oauth.rs`:

```rust
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
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib auth::oauth::callback_tests 2>&1 | grep -E "^error" | head -3`
Expected: compile errors.

- [ ] **Step 3: Implement**

Add to `src/auth/oauth.rs`:

```rust
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

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

fn html_page(status: u16, reason: &str, title: &str, detail: &str) -> String {
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
        let n = sock.read(&mut buf).await.unwrap_or(0);
        let head = String::from_utf8_lossy(&buf[..n]);
        let request_line = head.lines().next().unwrap_or("");
        let target = request_line.split_whitespace().nth(1).unwrap_or("");
        let (path, query) = target.split_once('?').unwrap_or((target, ""));
        if path != "/callback" {
            let _ = sock
                .write_all(html_page(404, "Not Found", "Not found", "").as_bytes())
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
        let _ = sock.write_all(page.as_bytes()).await;
        let _ = sock.shutdown().await;
        return outcome;
    }
}
```

- [ ] **Step 4: Run tests**

Run: `cargo test --lib auth::oauth 2>&1 | tail -3`
Expected: 14 passed.

- [ ] **Step 5: Commit**

```bash
git add src/auth/oauth.rs
git commit -m "auth: loopback OAuth callback listener (one request, 5 min timeout)

Co-Authored-By: Arch Linux <noreply@archlinux.org>"
```

### Task 9: Code exchange and the login flows

**Files:**
- Modify: `src/auth/oauth.rs`

**Interfaces:**
- Consumes: everything above; `profile::{load_config, save_config, save_credentials, set_active_profile, resolve_profile_name}`
- Produces:
  - `pub async fn exchange_code(base_url, client_id, code, verifier, redirect_uri, state) -> Result<TokenResponse>`
  - `pub struct LoginRequest { pub dir: PathBuf, pub profile: String, pub activate: bool, pub base_url: String, pub console_url: String, pub client_id: String, pub workspace_id: Option<String> }` with `LoginRequest::for_profile(name: Option<&str>) -> Result<Self>` (dir from `config_dir()`, profile from arg → `ANTHROPIC_PROFILE` → active → default; `activate` true when a name was given or nothing is active)
  - `pub struct LoginOutcome { pub profile: String, pub organization: Option<String>, pub email: Option<String>, pub workspace: Option<String>, pub expires_at: Option<i64> }`
  - `pub async fn login_browser(req: &LoginRequest, open_url: impl Fn(&str) -> bool, progress: impl Fn(String)) -> Result<LoginOutcome>`
  - `pub async fn login_manual(req: &LoginRequest, prompt: impl FnOnce(String) -> Pin<Box<dyn Future<Output = Option<String>> + Send>>) -> Result<LoginOutcome>`
  - `pub fn persist(req: &LoginRequest, tok: TokenResponse) -> Result<LoginOutcome>`

- [ ] **Step 1: Write the failing tests**

Append to `src/auth/oauth.rs`:

```rust
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

    #[tokio::test]
    async fn exchange_posts_the_form_grant() {
        let (base, seen) = token_server(TOKEN_JSON).await;
        let t = exchange_code(&base, "cid", "code1", "ver", "http://localhost:1/callback", "st")
            .await
            .unwrap();
        assert_eq!(t.access_token, "at");
        let r = seen.lock().await.clone();
        assert!(r.to_lowercase().contains("content-type: application/x-www-form-urlencoded"), "{r}");
        assert!(r.to_lowercase().contains("anthropic-beta: oauth-2025-04-20"), "{r}");
        let body = r.split("\r\n\r\n").nth(1).unwrap();
        let q: std::collections::HashMap<String, String> =
            url::form_urlencoded::parse(body.as_bytes()).into_owned().collect();
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
        let out = login_browser(&req(d.path(), &base), open, move |s| m2.lock().unwrap().push(s))
            .await
            .unwrap();
        assert_eq!(opened.load(Ordering::SeqCst), 1);
        assert_eq!(out.email.as_deref(), Some("a@example.com"));
        assert!(profile_exists(d.path(), "default"));
        assert!(msgs.lock().unwrap().iter().any(|m| m.contains("Waiting")), "{msgs:?}");
    }

    #[tokio::test]
    async fn manual_flow_uses_the_console_code_page_and_the_prompt() {
        let (base, seen) = token_server(TOKEN_JSON).await;
        let d = tempfile::tempdir().unwrap();
        let out = login_manual(&req(d.path(), &base), |authorize_url| {
            Box::pin(async move {
                assert!(authorize_url.contains("oauth%2Fcode%2Fcallback%3Fapp%3Danthropic-cli"), "{authorize_url}");
                Some("pasted-code".to_string())
            })
        })
        .await
        .unwrap();
        assert_eq!(out.profile, "default");
        let body = seen.lock().await.clone();
        assert!(body.contains("code=pasted-code"), "{body}");
        assert!(body.contains("redirect_uri=https%3A%2F%2Fconsole.test%2Foauth%2Fcode%2Fcallback%3Fapp%3Danthropic-cli"), "{body}");
    }

    #[tokio::test]
    async fn manual_flow_cancelled_at_the_prompt_is_an_error_not_a_hang() {
        let d = tempfile::tempdir().unwrap();
        let err = login_manual(&req(d.path(), "http://unused"), |_| Box::pin(async { None }))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("cancelled"), "{err}");
        assert!(!profile_exists(d.path(), "default"));
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib auth::oauth::login_flow_tests 2>&1 | grep -E "^error" | head -3`
Expected: compile errors.

- [ ] **Step 3: Implement**

Add to `src/auth/oauth.rs`:

```rust
use super::profile;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;

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
        let active_exists = dir.join("active_config").is_file();
        let (profile, activate) = match name.map(str::trim).filter(|n| !n.is_empty()) {
            Some(n) => (n.to_string(), true),
            None => (
                profile::resolve_profile_name(
                    &dir,
                    std::env::var("ANTHROPIC_PROFILE").ok().as_deref(),
                ),
                !active_exists,
            ),
        };
        if !profile
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
        {
            return Err(anyhow!("profile name may contain only letters, digits, '-', '_' and '.'"));
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
    let code = prompt(authorize_url)
        .await
        .map(|c| c.trim().to_string())
        .filter(|c| !c.is_empty())
        .ok_or_else(|| anyhow!("login cancelled"))?;
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
```

- [ ] **Step 4: Run tests**

Run: `cargo test --lib auth:: 2>&1 | grep -E "^test result|FAILED|panicked" && cargo clippy --all-targets 2>&1 | grep -cE "^(warning|error)"`
Expected: all ok; `0`.

- [ ] **Step 5: Commit**

```bash
git add src/auth/oauth.rs
git commit -m "auth: code exchange and the browser / manual login flows

Profiles are written in the ant-compatible layout and activated the way
ant does (named profile, or nothing active yet).

Co-Authored-By: Arch Linux <noreply@archlinux.org>"
```

### Task 10: `/login anthropic`, `/logout`, and the `CredentialChanged` event

**Files:**
- Create: `src/commands/login.rs`
- Modify: `src/commands/mod.rs:132` (enum), `:449-454` (dispatch arm), add `mod login;`
- Modify: `src/tui/events.rs` (two variants), `src/tui/app.rs:508-514` and `:1396` (`secret`), `src/tui/run/api_task.rs:65`
- Modify: `src/tui/run/dispatch.rs` (new arms; factor `open_in_browser`), `src/tui/run.rs:984` (event arm), `src/tui/render.rs:1224` (overlay hint), `src/commands/help.rs:136-148`

**Interfaces:**
- Produces:
  - `CommandAction::LoginAnthropic { profile: Option<String>, manual: bool }`, `CommandAction::LogoutAnthropic`, `CommandAction::LoginBoard` (board itself lands in Task 14; until then it shows a message), `CommandAction::LoginProvider { prefix: String, open_key_page: bool }` and `CommandAction::LogoutProvider(String)` (handled in Task 13; parse now)
  - `pub(super) fn cmd_login(args: &str) -> CommandAction`, `pub(super) fn cmd_logout(args: &str) -> CommandAction` (no context needed; keeps them unit-testable without building a `CommandContext`)
  - `AppEvent::AskUser { question, reply, secret: bool }`
  - `pub enum CredentialChange { Anthropic, Provider { prefix: String, key_env: String, value: Option<String> } }`, `AppEvent::CredentialChanged(CredentialChange)`
  - `pub(crate) fn open_in_browser(url: &str) -> bool` in `src/tui/run/dispatch.rs`
  - `PendingUserQuestion.secret: bool`

- [ ] **Step 1: Write the failing tests**

Create `src/commands/login.rs` with the test module:

```rust
//! `/login` and `/logout`: Anthropic Console OAuth and provider keys.

#[cfg(test)]
mod parse_tests {
    use super::*;

    /// Same split the dispatcher performs, without needing a CommandContext.
    fn parse(input: &str) -> CommandAction {
        let input = input.trim_start_matches('/');
        let (name, args) = input.split_once(' ').unwrap_or((input, ""));
        match name {
            "login" => cmd_login(args),
            "logout" => cmd_logout(args),
            other => panic!("not a login command: {other}"),
        }
    }

    #[test]
    fn bare_login_opens_the_board() {
        assert!(matches!(parse("/login"), CommandAction::LoginBoard));
    }

    #[test]
    fn anthropic_login_variants() {
        assert!(matches!(
            parse("/login anthropic"),
            CommandAction::LoginAnthropic { profile: None, manual: false }
        ));
        match parse("/login anthropic work") {
            CommandAction::LoginAnthropic { profile: Some(p), manual: false } => assert_eq!(p, "work"),
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            parse("/login anthropic manual"),
            CommandAction::LoginAnthropic { profile: None, manual: true }
        ));
        assert!(matches!(
            parse("/login claude"),
            CommandAction::LoginAnthropic { profile: None, manual: false }
        ), "alias");
    }

    #[test]
    fn provider_login_variants() {
        match parse("/login groq") {
            CommandAction::LoginProvider { prefix, open_key_page: false } => assert_eq!(prefix, "groq"),
            other => panic!("{other:?}"),
        }
        match parse("/login openrouter open") {
            CommandAction::LoginProvider { prefix, open_key_page: true } => assert_eq!(prefix, "openrouter"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn unknown_word_lists_the_valid_ones() {
        match parse("/login work") {
            CommandAction::Message(m) => {
                assert!(m.contains("anthropic") && m.contains("groq") && m.contains("/login anthropic work"), "{m}");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn logout_variants() {
        assert!(matches!(parse("/logout"), CommandAction::LogoutAnthropic));
        assert!(matches!(parse("/logout anthropic"), CommandAction::LogoutAnthropic));
        match parse("/logout groq") {
            CommandAction::LogoutProvider(p) => assert_eq!(p, "groq"),
            other => panic!("{other:?}"),
        }
        assert!(matches!(parse("/logout nope"), CommandAction::Message(_)));
    }
}
```

`CommandAction` needs `#[derive(Debug)]` for `{other:?}`; add it if missing (its payloads are strings, numbers, bools, and `Option<String>`s). If any variant holds a non-Debug type, wrap the assertions with `matches!` instead and drop the `panic!("{other:?}")` form.

Also add, in `src/tui/render.rs` `overlay_hint_tests`:

```rust
        assert!(overlay_hint("login", true).contains("Enter login"));
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib commands::login 2>&1 | grep -E "^error" | head -3`
Expected: compile errors.

- [ ] **Step 3: Implement**

**`src/commands/mod.rs`:** add `mod login;` next to the other command modules and `pub use login::*;` only if the other modules are re-exported that way (check how `catalogue` is declared and mirror it). Add variants to `CommandAction`:

```rust
    /// `/login` with no arguments: the credential status board.
    LoginBoard,
    /// Console OAuth for Anthropic (browser, or paste-the-code when `manual`).
    LoginAnthropic { profile: Option<String>, manual: bool },
    /// Masked key entry for an OpenAI-compatible provider.
    LoginProvider { prefix: String, open_key_page: bool },
    /// Remove the active Anthropic profile.
    LogoutAnthropic,
    /// Remove a stored provider key.
    LogoutProvider(String),
```

Replace the `"login" | "logout" => CommandAction::Message(...)` arm with:

```rust
        "login" => login::cmd_login(args),
        "logout" => login::cmd_logout(args),
```

**`src/commands/login.rs`** (above the tests):

```rust
use super::CommandAction;

const ANTHROPIC_WORDS: &[&str] = &["anthropic", "claude"];

fn provider_words() -> Vec<&'static str> {
    crate::api::PROVIDERS.iter().map(|p| p.prefix).collect()
}

fn usage(bad: &str) -> String {
    format!(
        "Unknown login target '{bad}'.\n\
         \n\
         /login                      status board\n\
         /login anthropic [profile]  Console OAuth (profile names go here, e.g. /login anthropic {bad})\n\
         /login anthropic manual     paste-the-code flow for SSH / headless\n\
         /login <provider> [open]    store an API key: {}\n\
         /logout [anthropic|<provider>]",
        provider_words().join(", ")
    )
}

pub(super) fn cmd_login(args: &str) -> CommandAction {
    let mut words = args.split_whitespace();
    let Some(first) = words.next() else {
        return CommandAction::LoginBoard;
    };
    let second = words.next();
    let first_l = first.to_ascii_lowercase();
    if ANTHROPIC_WORDS.contains(&first_l.as_str()) {
        return match second {
            Some("manual") => CommandAction::LoginAnthropic { profile: None, manual: true },
            Some(p) => CommandAction::LoginAnthropic { profile: Some(p.to_string()), manual: false },
            None => CommandAction::LoginAnthropic { profile: None, manual: false },
        };
    }
    if provider_words().contains(&first_l.as_str()) {
        return CommandAction::LoginProvider {
            prefix: first_l,
            open_key_page: second == Some("open"),
        };
    }
    CommandAction::Message(usage(first))
}

pub(super) fn cmd_logout(args: &str) -> CommandAction {
    let word = args.split_whitespace().next().map(str::to_ascii_lowercase);
    match word.as_deref() {
        None => CommandAction::LogoutAnthropic,
        Some(w) if ANTHROPIC_WORDS.contains(&w) => CommandAction::LogoutAnthropic,
        Some(w) if provider_words().contains(&w) => CommandAction::LogoutProvider(w.to_string()),
        Some(w) => CommandAction::Message(usage(w)),
    }
}
```

**`src/tui/events.rs`:**

```rust
    AskUser {
        question: String,
        reply: oneshot::Sender<String>,
        /// Render the answer as bullets (API keys).
        secret: bool,
    },
    /// A login or logout finished; the run loop re-resolves credentials.
    CredentialChanged(CredentialChange),
```

and, in the same file:

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CredentialChange {
    Anthropic,
    /// `value: None` means the key was removed.
    Provider { prefix: String, key_env: String, value: Option<String> },
}
```

Update the emitter in `src/tui/run/api_task.rs:65` to pass `secret: false`, the handler in `src/tui/app.rs:1396` to destructure `secret` and store it, and `PendingUserQuestion` to gain `pub secret: bool`.

**`src/tui/run/dispatch.rs`:** factor the browser opener out of the `OpenBrowser` arm into a free function at the bottom of the file, and make the arm call it:

```rust
/// Best-effort platform opener. Returns whether a launcher was spawned.
pub(crate) fn open_in_browser(url: &str) -> bool {
    std::process::Command::new("xdg-open").arg(url).spawn().is_ok()
        || std::process::Command::new("open").arg(url).spawn().is_ok()
        || std::process::Command::new("cmd.exe")
            .args(["/C", "start", url])
            .spawn()
            .is_ok()
}
```

Add the arms:

```rust
        CommandAction::LoginBoard => {
            // Replaced by the interactive board in the board task.
            app.entries.push(ChatEntry::system(
                "/login anthropic — sign in to Anthropic\n/login <provider> — store a provider key\n/logout — remove".into(),
            ));
            app.scroll_to_bottom();
        }

        CommandAction::LoginAnthropic { profile, manual } => {
            let req = match crate::auth::oauth::LoginRequest::for_profile(profile.as_deref()) {
                Ok(r) => r,
                Err(e) => {
                    app.entries.push(ChatEntry::error(format!("/login: {e}")));
                    app.scroll_to_bottom();
                    return Ok(());
                }
            };
            app.entries.push(ChatEntry::system(format!(
                "Signing in to Anthropic (profile '{}')…",
                req.profile
            )));
            app.scroll_to_bottom();
            let tx2 = tx.clone();
            tokio::spawn(async move {
                use crate::tui::events::{AppEvent, CredentialChange};
                let progress_tx = tx2.clone();
                let progress = move |s: String| {
                    let _ = progress_tx.send(AppEvent::SystemMessage(s));
                };
                let result = if manual {
                    let ask_tx = tx2.clone();
                    crate::auth::oauth::login_manual(&req, move |authorize_url| {
                        Box::pin(async move {
                            let (reply, rx) = tokio::sync::oneshot::channel();
                            let _ = ask_tx.send(AppEvent::AskUser {
                                question: format!(
                                    "Open this URL anywhere, sign in, and paste the code shown:\n{authorize_url}"
                                ),
                                reply,
                                secret: false,
                            });
                            rx.await.ok().filter(|s| !s.trim().is_empty())
                        })
                    })
                    .await
                } else {
                    crate::auth::oauth::login_browser(&req, open_in_browser, progress).await
                };
                match result {
                    Ok(out) => {
                        let who = match (&out.email, &out.organization) {
                            (Some(e), Some(o)) => format!("{e} · org {o}"),
                            (Some(e), None) => e.clone(),
                            (None, Some(o)) => format!("org {o}"),
                            (None, None) => String::new(),
                        };
                        let _ = tx2.send(AppEvent::SystemMessage(format!(
                            "✓ Signed in to Anthropic as profile '{}' {who}",
                            out.profile
                        )));
                        let _ = tx2.send(AppEvent::CredentialChanged(CredentialChange::Anthropic));
                    }
                    Err(e) => {
                        let _ = tx2.send(AppEvent::SystemMessage(format!("✗ Login failed: {e}")));
                    }
                }
            });
        }

        CommandAction::LogoutAnthropic => {
            let msg = match crate::auth::profile::config_dir() {
                None => "Cannot determine the Anthropic config directory.".to_string(),
                Some(dir) => {
                    let name = crate::auth::profile::resolve_profile_name(
                        &dir,
                        std::env::var("ANTHROPIC_PROFILE").ok().as_deref(),
                    );
                    match crate::auth::profile::delete_profile(&dir, &name) {
                        Ok(true) => format!("Removed OAuth profile '{name}'."),
                        Ok(false) => format!("No OAuth profile '{name}' to remove."),
                        Err(e) => format!("Could not remove profile '{name}': {e}"),
                    }
                }
            };
            app.entries.push(ChatEntry::system(msg));
            app.scroll_to_bottom();
            let _ = tx.send(crate::tui::events::AppEvent::CredentialChanged(
                crate::tui::events::CredentialChange::Anthropic,
            ));
        }

        CommandAction::LoginProvider { prefix, .. } | CommandAction::LogoutProvider(prefix) => {
            // Implemented in the keystore task.
            app.entries.push(ChatEntry::system(format!(
                "Provider key management for '{prefix}' is not available yet."
            )));
            app.scroll_to_bottom();
        }
```

**`src/tui/run.rs`:** in the event `match` (before `other => app.apply(other),` at line 984) add:

```rust
                        AppEvent::CredentialChanged(change) => {
                            use crate::tui::events::CredentialChange;
                            match change {
                                CredentialChange::Anthropic => {
                                    config.resolve_anthropic_auth();
                                    for w in &config.auth_warnings {
                                        app.entries.push(ChatEntry::system(format!("⚠ {w}")));
                                    }
                                    let is_anthropic = !crate::api::is_ollama_model(&config.model)
                                        && !crate::api::is_openai_compat_model(&config.model);
                                    if is_anthropic {
                                        match ApiBackend::from_config(&config) {
                                            Ok(c) => client = c,
                                            Err(e) => app.entries.push(ChatEntry::error(format!("Backend error: {e}"))),
                                        }
                                    }
                                    if config.auth.is_none() {
                                        app.entries.push(ChatEntry::system(
                                            "No Anthropic credential is active now. Run /login anthropic, or set ANTHROPIC_API_KEY.".into(),
                                        ));
                                    }
                                }
                                CredentialChange::Provider { .. } => {
                                    // Keystore wiring lands in the keystore task.
                                }
                            }
                            app.scroll_to_bottom();
                        }
```

`config` is the mutable `Config` owned by `run_loop`; if it is bound immutably there, change the binding to `mut` (the `pending_model` handler at line 616 already mutates it, so it is `mut`).

**`src/tui/render.rs`:** add `"login" => " ↑↓ select · Enter login · 1-9 quick · Esc close ",` to `overlay_hint`.

**`src/commands/help.rs`:** in the "Model & behavior" block after `("/model", …)` add:

```rust
            ("/login", "sign in: Anthropic OAuth, or store a provider key"),
            ("/logout", "remove the Anthropic profile or a provider key"),
```

- [ ] **Step 4: Run everything**

Run: `cargo test 2>&1 | grep -E "^test result|FAILED|panicked" && cargo clippy --all-targets 2>&1 | grep -cE "^(warning|error)"`
Expected: all ok; `0`.

Manual check (needs a Console account): `oxideclaw`, `/login anthropic`, complete the browser flow, see "✓ Signed in", `/doctor` shows the profile, send a message. `/logout` then removes it and the warning about no credential appears.

- [ ] **Step 5: Commit**

```bash
git add src/commands/mod.rs src/commands/login.rs src/commands/help.rs src/tui/events.rs src/tui/app.rs src/tui/run/api_task.rs src/tui/run/dispatch.rs src/tui/run.rs src/tui/render.rs
git commit -m "tui: /login anthropic and /logout with live credential swap

Console OAuth runs in a spawned task; the run loop re-resolves the
credential chain on CredentialChanged and rebuilds the Anthropic client.
Manual flow prompts for the pasted code through the ask-user dialog.

Co-Authored-By: Arch Linux <noreply@archlinux.org>"
```

---

## Increment 3: provider keystore

### Task 11: `Keystore` and `.env` editing (`auth/keystore.rs`)

**Files:**
- Create: `src/auth/keystore.rs`
- Modify: `src/auth/mod.rs` (add `pub mod keystore;`), `src/main.rs:403-509` (allowlist and loaders move out)

**Interfaces:**
- Produces:
  - `pub const SAFE_ENV_KEYS: &[&str]` (moved verbatim from `main.rs`)
  - `#[derive(Clone, Copy, Debug, PartialEq, Eq)] pub enum KeySource { ShellEnv, ProjectDotenv, HomeDotenv, UserDotenv }` with `pub fn describe(self) -> &'static str` → `"shell env"`, `"project .env"`, `"~/.env"`, `"~/.config/oxideclaw/.env"`
  - `#[derive(Clone, Default)] pub struct Keystore` with `get(&self, key) -> Option<&str>`, `source(&self, key) -> Option<KeySource>`, `set(&mut self, key, value, source)`, `remove(&mut self, key)`, `lookup(&self) -> impl Fn(&str) -> Option<String> + '_`; `Debug` prints keys and sources only
  - `pub fn parse_dotenv(content: &str) -> Vec<(String, String)>` (allowlisted keys only; `export ` prefix and quotes stripped)
  - `pub fn load_dotenv_auto() -> Keystore` (sets unset env vars from the three files, as before, and returns the attributed store)
  - `pub fn user_env_path() -> Option<PathBuf>`
  - `pub fn upsert_line(content: &str, key: &str, value: &str) -> String`, `pub fn remove_line(content: &str, key: &str) -> String`
  - `pub fn save_key(key: &str, value: &str) -> Result<PathBuf>`, `pub fn remove_key(key: &str) -> Result<bool>` (both operate on `user_env_path()`); `pub fn save_key_at(path, key, value) -> Result<()>` and `pub fn remove_key_at(path, key) -> Result<bool>` for tests

- [ ] **Step 1: Write the failing tests**

Create `src/auth/keystore.rs` with the test module:

```rust
//! Provider API keys: where they come from and how `/login <provider>` stores
//! them. Storage is the user-level `~/.config/oxideclaw/.env`, never the
//! project tree. Nothing here mutates the process environment after startup.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_keeps_only_allowlisted_keys_and_strips_decoration() {
        let got = parse_dotenv(
            "# comment\nexport GROQ_API_KEY=\"gsk_1\"\nPATH=/evil\n\nMISTRAL_API_KEY='m1'\nNOT_A_KEY=x\n",
        );
        assert_eq!(
            got,
            vec![
                ("GROQ_API_KEY".to_string(), "gsk_1".to_string()),
                ("MISTRAL_API_KEY".to_string(), "m1".to_string()),
            ]
        );
    }

    #[test]
    fn upsert_replaces_in_place_and_preserves_everything_else() {
        let before = "# keys\nexport GROQ_API_KEY=old\nOLLAMA_HOST=http://x\n";
        let after = upsert_line(before, "GROQ_API_KEY", "new");
        assert_eq!(after, "# keys\nGROQ_API_KEY=new\nOLLAMA_HOST=http://x\n");
        let added = upsert_line("A=1", "GROQ_API_KEY", "g");
        assert_eq!(added, "A=1\nGROQ_API_KEY=g\n", "missing trailing newline handled");
        assert_eq!(upsert_line("", "GROQ_API_KEY", "g"), "GROQ_API_KEY=g\n");
    }

    #[test]
    fn remove_drops_only_that_key() {
        let before = "GROQ_API_KEY=g\n# note\nexport GROQ_API_KEY=dup\nMISTRAL_API_KEY=m\n";
        assert_eq!(remove_line(before, "GROQ_API_KEY"), "# note\nMISTRAL_API_KEY=m\n");
        assert_eq!(remove_line("A=1\n", "GROQ_API_KEY"), "A=1\n");
    }

    #[test]
    fn save_and_remove_at_a_path_with_secret_permissions() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("cfg").join(".env");
        save_key_at(&path, "GROQ_API_KEY", "g1").unwrap();
        save_key_at(&path, "MISTRAL_API_KEY", "m1").unwrap();
        save_key_at(&path, "GROQ_API_KEY", "g2").unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "GROQ_API_KEY=g2\nMISTRAL_API_KEY=m1\n"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
            assert_eq!(
                std::fs::metadata(path.parent().unwrap()).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
        assert!(remove_key_at(&path, "GROQ_API_KEY").unwrap());
        assert!(!remove_key_at(&path, "GROQ_API_KEY").unwrap());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "MISTRAL_API_KEY=m1\n");
    }

    #[test]
    fn keystore_tracks_sources_and_never_prints_values() {
        let mut ks = Keystore::default();
        ks.set("GROQ_API_KEY", "gsk_secret", KeySource::ShellEnv);
        ks.set("MISTRAL_API_KEY", "m", KeySource::UserDotenv);
        assert_eq!(ks.get("GROQ_API_KEY"), Some("gsk_secret"));
        assert_eq!(ks.source("MISTRAL_API_KEY"), Some(KeySource::UserDotenv));
        assert_eq!(ks.lookup()("GROQ_API_KEY").as_deref(), Some("gsk_secret"));
        assert_eq!(ks.lookup()("NOPE"), None);
        let dbg = format!("{ks:?}");
        assert!(dbg.contains("GROQ_API_KEY") && !dbg.contains("gsk_secret"), "{dbg}");
        ks.remove("GROQ_API_KEY");
        assert_eq!(ks.get("GROQ_API_KEY"), None);
    }

    #[test]
    fn allowlist_still_excludes_redirecting_urls() {
        assert!(!SAFE_ENV_KEYS.contains(&"ANTHROPIC_BASE_URL"));
        assert!(!SAFE_ENV_KEYS.contains(&"OPENAI_BASE_URL"));
        assert!(!SAFE_ENV_KEYS.contains(&"LM_STUDIO_HOST"));
        for p in crate::api::PROVIDERS {
            if !p.key_env.is_empty() {
                assert!(SAFE_ENV_KEYS.contains(&p.key_env), "{} must be storable", p.key_env);
            }
        }
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib auth::keystore 2>&1 | grep -E "^error" | head -3`
Expected: compile errors.

- [ ] **Step 3: Implement**

`src/auth/keystore.rs` (above the tests):

```rust
use anyhow::{Context, Result, anyhow};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Keys that may be loaded from a `.env` file. Copied from `main.rs`; the
/// comments there about `ANTHROPIC_BASE_URL` apply unchanged.
pub const SAFE_ENV_KEYS: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_PROFILE",
    "OXIDECLAW_API_KEY_FILE_DESCRIPTOR",
    "RUSTYCLAW_API_KEY_FILE_DESCRIPTOR",
    "ANTHROPIC_MODEL",
    "OXIDECLAW_VERBOSE",
    "RUSTYCLAW_VERBOSE",
    "OLLAMA_HOST",
    "OPENAI_API_KEY",
    "GROQ_API_KEY",
    "DEEPSEEK_API_KEY",
    "MISTRAL_API_KEY",
    "OPENROUTER_API_KEY",
    "TOGETHER_API_KEY",
    "XAI_API_KEY",
    "VENICE_API_KEY",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeySource {
    ShellEnv,
    ProjectDotenv,
    HomeDotenv,
    UserDotenv,
}

impl KeySource {
    pub fn describe(self) -> &'static str {
        match self {
            KeySource::ShellEnv => "shell env",
            KeySource::ProjectDotenv => "project .env",
            KeySource::HomeDotenv => "~/.env",
            KeySource::UserDotenv => "~/.config/oxideclaw/.env",
        }
    }
}

#[derive(Clone, Default)]
pub struct Keystore {
    entries: HashMap<String, (String, KeySource)>,
}

impl std::fmt::Debug for Keystore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut keys: Vec<_> = self.entries.iter().map(|(k, (_, s))| (k, s)).collect();
        keys.sort();
        f.debug_map().entries(keys).finish()
    }
}

impl Keystore {
    pub fn get(&self, key: &str) -> Option<&str> {
        self.entries.get(key).map(|(v, _)| v.as_str())
    }
    pub fn source(&self, key: &str) -> Option<KeySource> {
        self.entries.get(key).map(|(_, s)| *s)
    }
    pub fn set(&mut self, key: &str, value: &str, source: KeySource) {
        self.entries.insert(key.to_string(), (value.to_string(), source));
    }
    pub fn remove(&mut self, key: &str) {
        self.entries.remove(key);
    }
    /// Closure shape used by `configured_providers` and the client constructor.
    pub fn lookup(&self) -> impl Fn(&str) -> Option<String> + '_ {
        move |k| self.get(k).map(str::to_string)
    }
}

/// Allowlisted `KEY=value` pairs from a `.env` body, in file order.
pub fn parse_dotenv(content: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        if let Some((key, val)) = line.split_once('=') {
            let key = key.trim();
            let val = val.trim().trim_matches('"').trim_matches('\'');
            if !key.is_empty() && SAFE_ENV_KEYS.contains(&key) {
                out.push((key.to_string(), val.to_string()));
            }
        }
    }
    out
}

/// Load one file into the process env (unset keys only — earlier sources win)
/// and record attribution in `ks`.
fn load_one(path: &Path, source: KeySource, ks: &mut Keystore) {
    let Ok(content) = std::fs::read_to_string(path) else {
        return;
    };
    for (key, val) in parse_dotenv(&content) {
        if std::env::var(&key).is_err() {
            // Startup only, before any thread is spawned — same contract as
            // the loader this replaced in main.rs.
            unsafe {
                std::env::set_var(&key, &val);
            }
            ks.set(&key, &val, source);
        }
    }
}

/// `~/.config/oxideclaw/.env` (via the app dir helper, which also migrates
/// the legacy directory name).
pub fn user_env_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| crate::config::app_dir(&h.join(".config")).join(".env"))
}

/// Search the usual locations in priority order and build the keystore.
/// Must run once at startup before the async runtime spawns threads.
pub fn load_dotenv_auto() -> Keystore {
    let mut ks = Keystore::default();
    for k in SAFE_ENV_KEYS {
        if let Ok(v) = std::env::var(k)
            && !v.trim().is_empty()
        {
            ks.set(k, &v, KeySource::ShellEnv);
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        let p = cwd.join(".env");
        if p.exists() {
            load_one(&p, KeySource::ProjectDotenv, &mut ks);
            eprintln!(
                "Note: .env detected in project root. Only oxideclaw-specific keys \
                 (ANTHROPIC_API_KEY, OLLAMA_HOST, etc.) are loaded. \
                 Project vars are NOT injected into tool execution."
            );
        }
    }
    if let Some(home) = dirs::home_dir() {
        load_one(&home.join(".env"), KeySource::HomeDotenv, &mut ks);
        if let Some(user) = user_env_path() {
            load_one(&user, KeySource::UserDotenv, &mut ks);
        }
    }
    ks
}

fn is_line_for(line: &str, key: &str) -> bool {
    let l = line.trim();
    let l = l.strip_prefix("export ").unwrap_or(l);
    l.split_once('=').is_some_and(|(k, _)| k.trim() == key)
}

/// Replace the first `KEY=` line (dropping any duplicates) or append one.
pub fn upsert_line(content: &str, key: &str, value: &str) -> String {
    let mut out = String::new();
    let mut written = false;
    for line in content.lines() {
        if is_line_for(line, key) {
            if !written {
                out.push_str(&format!("{key}={value}\n"));
                written = true;
            }
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    if !written {
        out.push_str(&format!("{key}={value}\n"));
    }
    out
}

pub fn remove_line(content: &str, key: &str) -> String {
    let mut out = String::new();
    for line in content.lines() {
        if is_line_for(line, key) {
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

fn write_env(path: &Path, content: &str) -> Result<()> {
    let parent = path.parent().ok_or_else(|| anyhow!("no parent for {}", path.display()))?;
    std::fs::create_dir_all(parent)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
    }
    let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
    std::io::Write::write_all(&mut tmp, content.as_bytes())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o600))?;
    }
    tmp.persist(path)
        .map_err(|e| anyhow!("persist {}: {}", path.display(), e.error))?;
    Ok(())
}

pub fn save_key_at(path: &Path, key: &str, value: &str) -> Result<()> {
    if !SAFE_ENV_KEYS.contains(&key) {
        return Err(anyhow!("{key} is not a storable key"));
    }
    if value.contains('\n') || value.trim().is_empty() {
        return Err(anyhow!("key value must be a single non-empty line"));
    }
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    write_env(path, &upsert_line(&existing, key, value.trim()))
        .with_context(|| format!("write {}", path.display()))
}

pub fn remove_key_at(path: &Path, key: &str) -> Result<bool> {
    let Ok(existing) = std::fs::read_to_string(path) else {
        return Ok(false);
    };
    let had = existing.lines().any(|l| is_line_for(l, key));
    if had {
        write_env(path, &remove_line(&existing, key))?;
    }
    Ok(had)
}

pub fn save_key(key: &str, value: &str) -> Result<PathBuf> {
    let path = user_env_path().ok_or_else(|| anyhow!("cannot determine the home directory"))?;
    save_key_at(&path, key, value)?;
    Ok(path)
}

pub fn remove_key(key: &str) -> Result<bool> {
    let path = user_env_path().ok_or_else(|| anyhow!("cannot determine the home directory"))?;
    remove_key_at(&path, key)
}
```

Add `pub mod keystore;` to `src/auth/mod.rs`.

`src/main.rs`: delete `SAFE_ENV_KEYS`, `load_dotenv`, and `load_dotenv_auto` (lines 403-439 and 463-509). Keep `FORBIDDEN_ENV_KEYS` and the tests, rewriting them against the new module: replace `SAFE_ENV_KEYS` with `crate::auth::keystore::SAFE_ENV_KEYS`, and `load_dotenv(&path)` in `load_dotenv_blocks_dangerous_vars` with `crate::auth::keystore::parse_dotenv(&std::fs::read_to_string(&path).unwrap())` asserting the forbidden keys are absent from the parsed list. Where `main()` called `load_dotenv_auto();` it now does `let keystore = crate::auth::keystore::load_dotenv_auto();` and passes `keystore` into config in Task 12 (for now store it in a local and add `let _ = &keystore;`).

- [ ] **Step 4: Run tests**

Run: `cargo test 2>&1 | grep -E "^test result|FAILED|panicked" && cargo clippy --all-targets 2>&1 | grep -cE "^(warning|error)"`
Expected: all ok; `0`.

- [ ] **Step 5: Commit**

```bash
git add src/auth/mod.rs src/auth/keystore.rs src/main.rs
git commit -m "auth: provider keystore with .env line editing and source attribution

Moves the .env allowlist and loaders out of main.rs. Keys are stored in
the user-level ~/.config/oxideclaw/.env, 0600 in 0700, atomically.

Co-Authored-By: Arch Linux <noreply@archlinux.org>"
```

### Task 12: Plumb the keystore into config, the picker, and the provider client

**Files:**
- Modify: `src/config.rs` (field + default), `src/main.rs` (pass keystore), `src/api/openai_compat.rs:566-612` (`from_model_with`), `src/api/mod.rs` (`from_config`), `src/commands/catalogue.rs:104` and `src/tui/run/dispatch.rs:157` (picker lookup)

**Interfaces:**
- Produces:
  - `Config.keystore: crate::auth::keystore::Keystore` (serde-skipped, default empty)
  - `OpenAiCompatClient::from_model_with(model: &str, lookup: &dyn Fn(&str) -> Option<String>) -> Result<Self>`; `from_model(model)` = `from_model_with(model, &|k| std::env::var(k).ok())`
  - `ApiBackend::from_config` uses `config.keystore.lookup()` for providers

- [ ] **Step 1: Write the failing tests**

Append to the `provider_detection_tests` module in `src/api/openai_compat.rs`:

```rust
    #[test]
    fn client_takes_the_key_from_the_lookup_not_the_process_env() {
        let c = OpenAiCompatClient::from_model_with("groq:llama-3.3-70b-versatile", &|k| {
            (k == "GROQ_API_KEY").then(|| "gsk_from_store".to_string())
        })
        .unwrap();
        assert_eq!(c.api_key_for_test(), "gsk_from_store");
        let err = OpenAiCompatClient::from_model_with("groq:x", &|_| None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("GROQ_API_KEY"), "{err}");
    }

    #[test]
    fn openai_key_is_the_fallback_for_every_cloud_provider() {
        let c = OpenAiCompatClient::from_model_with("mistral:mistral-large-latest", &|k| {
            (k == "OPENAI_API_KEY").then(|| "sk-shared".to_string())
        })
        .unwrap();
        assert_eq!(c.api_key_for_test(), "sk-shared");
    }
```

Add to `impl OpenAiCompatClient`: `#[cfg(test)] pub(crate) fn api_key_for_test(&self) -> &str { &self.api_key }`.

In `src/config.rs` `auth_handle_tests` add:

```rust
    #[test]
    fn from_config_uses_the_keystore_for_providers() {
        let mut c = Config::default();
        c.model = "groq:llama-3.3-70b-versatile".into();
        assert!(crate::api::ApiBackend::from_config(&c).is_err(), "no key anywhere");
        c.keystore.set("GROQ_API_KEY", "gsk", crate::auth::keystore::KeySource::UserDotenv);
        assert!(matches!(
            crate::api::ApiBackend::from_config(&c).unwrap(),
            crate::api::ApiBackend::OpenAiCompat(_)
        ));
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib provider_detection_tests 2>&1 | grep -E "^error" | head -3`
Expected: compile errors.

- [ ] **Step 3: Implement**

`src/api/openai_compat.rs`: rename the body of `from_model` to `from_model_with` taking `lookup: &dyn Fn(&str) -> Option<String>`, replacing the three `std::env::var(X)` reads with `lookup(X)`:

```rust
    pub fn from_model(model: &str) -> Result<Self> {
        Self::from_model_with(model, &|k| std::env::var(k).ok())
    }

    /// Build a client with credentials from `lookup` (the keystore in the
    /// TUI; the process env for the SDK and tests).
    pub fn from_model_with(model: &str, lookup: &dyn Fn(&str) -> Option<String>) -> Result<Self> {
        let (provider, _bare) = parse_provider_model(model)
            .ok_or_else(|| anyhow!("Unknown provider prefix in '{model}'"))?;

        let base_url = if provider.prefix == "openai-compat" {
            lookup("OPENAI_BASE_URL").ok_or_else(|| {
                anyhow!(
                    "openai-compat: requires OPENAI_BASE_URL in your shell.\n\
                     Set it to your endpoint, e.g.:\n  \
                     export OPENAI_BASE_URL=http://localhost:8080/v1"
                )
            })?
        } else if provider.prefix == "lmstudio" {
            lookup("LM_STUDIO_HOST").unwrap_or_else(|| provider.base_url.to_string())
        } else {
            provider.base_url.to_string()
        };

        let api_key = if provider.key_env.is_empty() {
            String::new()
        } else {
            lookup(provider.key_env)
                .or_else(|| lookup("OPENAI_API_KEY"))
                .unwrap_or_default()
        };
        // … rest of the existing function unchanged …
```

Note `OPENAI_BASE_URL` and `LM_STUDIO_HOST` are not in the allowlist, so the keystore never holds them; `from_config` therefore chains: `let ks = config.keystore.lookup(); let lookup = |k: &str| ks(k).or_else(|| std::env::var(k).ok());`. That keeps the shell-only URL variables working.

`src/config.rs`: add the field after `auth`:

```rust
    /// Provider API keys with their source, built once at startup.
    #[serde(skip)]
    pub keystore: crate::auth::keystore::Keystore,
```

and `keystore: Default::default(),` in `Default`. In `main.rs`, after `Config::load()` succeeds, set `config.keystore = keystore;` (the local from Task 11). Do the same wherever else `Config::load()` is called before a TUI or SDK session starts (`grep -n "Config::load()" src/main.rs src/sdk/*.rs src/acp/*.rs`); for the SDK/ACP paths call `crate::auth::keystore::load_dotenv_auto()` there if `main()` does not run first.

`src/api/mod.rs` `from_config`:

```rust
        } else if is_openai_compat_model(model) {
            let ks = config.keystore.lookup();
            let lookup = |k: &str| ks(k).or_else(|| std::env::var(k).ok());
            Ok(Self::OpenAiCompat(OpenAiCompatClient::from_model_with(model, &lookup)?))
        }
```

Picker: in `src/tui/run/dispatch.rs:157` replace `provider_picker_entries(|k| std::env::var(k).ok())` with:

```rust
            let providers = {
                let ks = config.keystore.lookup();
                crate::commands::provider_picker_entries(|k| ks(k).or_else(|| std::env::var(k).ok()))
            };
```

- [ ] **Step 4: Run tests**

Run: `cargo test 2>&1 | grep -E "^test result|FAILED|panicked" && cargo clippy --all-targets 2>&1 | grep -cE "^(warning|error)"`
Expected: all ok; `0`.

- [ ] **Step 5: Commit**

```bash
git add src/config.rs src/main.rs src/api/openai_compat.rs src/api/mod.rs src/tui/run/dispatch.rs
git commit -m "providers: read keys from the keystore instead of the process env

Co-Authored-By: Arch Linux <noreply@archlinux.org>"
```

### Task 13: Masked entry, key validation, `/login <provider>`, `/logout <provider>`

**Files:**
- Modify: `src/api/openai_compat.rs` (`key_url`, `validate_key`), `src/tui/render.rs:1170-1200` (masked input), `src/tui/run/dispatch.rs` (provider arms), `src/tui/run.rs` (Provider change arm)

**Interfaces:**
- Produces:
  - `ProviderDef.key_url: &'static str`
  - `pub enum KeyValidation { Valid, Rejected(u16), Unverified(String) }`
  - `pub async fn validate_key(provider: &ProviderDef, base_url: &str, key: &str) -> KeyValidation`
  - `pub fn provider_by_prefix(prefix: &str) -> Option<&'static ProviderDef>`

- [ ] **Step 1: Write the failing tests**

Append to `provider_detection_tests` in `src/api/openai_compat.rs`:

```rust
    #[test]
    fn every_cloud_provider_has_a_key_page() {
        for p in PROVIDERS {
            if p.prefix == "lmstudio" || p.prefix == "openai-compat" {
                assert!(p.key_url.is_empty(), "{}", p.prefix);
            } else {
                assert!(p.key_url.starts_with("https://"), "{} needs a key page", p.prefix);
            }
        }
        assert_eq!(provider_by_prefix("groq").map(|p| p.name), Some("Groq"));
        assert!(provider_by_prefix("nope").is_none());
    }

    async fn status_server(status_line: &'static str) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = vec![0u8; 4096];
                let _ = sock.read(&mut buf).await;
                let resp = format!("{status_line}\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{{}}");
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.shutdown().await;
            }
        });
        format!("http://{addr}/v1")
    }

    #[tokio::test]
    async fn validation_maps_status_codes() {
        let groq = provider_by_prefix("groq").unwrap();
        let base = status_server("HTTP/1.1 200 OK").await;
        assert!(matches!(validate_key(groq, &base, "k").await, KeyValidation::Valid));
        let base = status_server("HTTP/1.1 401 Unauthorized").await;
        assert!(matches!(validate_key(groq, &base, "k").await, KeyValidation::Rejected(401)));
        let base = status_server("HTTP/1.1 403 Forbidden").await;
        assert!(matches!(validate_key(groq, &base, "k").await, KeyValidation::Rejected(403)));
        let base = status_server("HTTP/1.1 404 Not Found").await;
        assert!(matches!(validate_key(groq, &base, "k").await, KeyValidation::Unverified(_)));
        assert!(matches!(
            validate_key(groq, "http://127.0.0.1:1/v1", "k").await,
            KeyValidation::Unverified(_)
        ));
    }
```

In `src/tui/render.rs`, add a pure helper with a test:

```rust
/// Input row text for the ask-user dialog. Secrets render as bullets.
pub(crate) fn dialog_input_display(input: &[char], secret: bool) -> String {
    if secret {
        "•".repeat(input.len())
    } else {
        input.iter().collect()
    }
}

#[cfg(test)]
mod dialog_tests {
    use super::dialog_input_display;

    #[test]
    fn secrets_render_as_bullets_of_the_same_length() {
        let chars: Vec<char> = "gsk_abc".chars().collect();
        assert_eq!(dialog_input_display(&chars, true), "•••••••");
        assert_eq!(dialog_input_display(&chars, false), "gsk_abc");
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib provider_detection_tests dialog_tests 2>&1 | grep -E "^error" | head -3`
Expected: compile errors.

- [ ] **Step 3: Implement**

`src/api/openai_compat.rs`: add `pub key_url: &'static str,` to `ProviderDef` and fill the registry:

| prefix | key_url |
|--------|---------|
| groq | `https://console.groq.com/keys` |
| openrouter | `https://openrouter.ai/keys` |
| deepseek | `https://platform.deepseek.com/api_keys` |
| lmstudio | `""` |
| together | `https://api.together.ai/settings/api-keys` |
| mistral | `https://console.mistral.ai/api-keys` |
| venice | `https://venice.ai/settings/api` |
| oai | `https://platform.openai.com/api-keys` |
| openai-compat | `""` |

Add:

```rust
pub fn provider_by_prefix(prefix: &str) -> Option<&'static ProviderDef> {
    PROVIDERS.iter().find(|p| p.prefix == prefix)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyValidation {
    Valid,
    /// 401 or 403: the provider rejected the key.
    Rejected(u16),
    /// Could not tell (network error, no models endpoint, 5xx).
    Unverified(String),
}

/// `GET <base_url>/models` with the key. 10 s timeout.
pub async fn validate_key(provider: &ProviderDef, base_url: &str, key: &str) -> KeyValidation {
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
    {
        Ok(c) => c,
        Err(e) => return KeyValidation::Unverified(e.to_string()),
    };
    let mut req = client
        .get(format!("{}/models", base_url.trim_end_matches('/')))
        .bearer_auth(key);
    for (k, v) in provider.extra_headers {
        req = req.header(*k, *v);
    }
    match req.send().await {
        Ok(resp) => match resp.status().as_u16() {
            200 => KeyValidation::Valid,
            s @ (401 | 403) => KeyValidation::Rejected(s),
            s => KeyValidation::Unverified(format!("HTTP {s} from {}/models", base_url)),
        },
        Err(e) => KeyValidation::Unverified(e.to_string()),
    }
}
```

`src/tui/render.rs` `draw_ask_user`: compute the three spans through the helper:

```rust
    let shown: Vec<char> = dialog_input_display(&q.input, q.secret).chars().collect();
    let before: String = shown[..q.cursor].iter().collect();
    let rest: Vec<char> = shown[q.cursor..].to_vec();
```

(the rest of the cursor logic is unchanged since bullets are one char each). Change the footer to `"  Enter to send  ·  Esc to cancel"` → keep, and when `q.secret` change the title to `" Enter API key "`.

`src/tui/run/dispatch.rs`: replace the placeholder `LoginProvider | LogoutProvider` arm with:

```rust
        CommandAction::LoginProvider { prefix, open_key_page } => {
            let Some(p) = crate::api::provider_by_prefix(&prefix) else {
                return Ok(());
            };
            if p.key_env.is_empty() || p.prefix == "openai-compat" {
                let hint = if p.prefix == "lmstudio" {
                    "LM Studio needs no key. Set the host in your shell:\n  export LM_STUDIO_HOST=http://localhost:1234/v1\nthen /model lmstudio:<model-name>".to_string()
                } else {
                    "The generic endpoint needs the base URL in your shell (never from a file):\n  export OPENAI_BASE_URL=https://host/v1\nStoring OPENAI_API_KEY now…".to_string()
                };
                app.entries.push(ChatEntry::system(hint));
                app.scroll_to_bottom();
                if p.prefix == "lmstudio" {
                    return Ok(());
                }
            }
            if open_key_page && !p.key_url.is_empty() {
                let opened = open_in_browser(p.key_url);
                app.entries.push(ChatEntry::system(if opened {
                    format!("Opened {} in your browser.", p.key_url)
                } else {
                    format!("Could not open a browser. Keys are at {}", p.key_url)
                }));
            }
            let shadow = config
                .keystore
                .source(p.key_env)
                .filter(|s| *s == crate::auth::keystore::KeySource::ShellEnv);
            app.scroll_to_bottom();
            let tx2 = tx.clone();
            let ks_lookup_base = {
                let ks = config.keystore.lookup();
                ks("OPENAI_BASE_URL").or_else(|| std::env::var("OPENAI_BASE_URL").ok())
            };
            tokio::spawn(async move {
                use crate::api::KeyValidation;
                use crate::tui::events::{AppEvent, CredentialChange};
                let base_url = if p.prefix == "openai-compat" {
                    ks_lookup_base.unwrap_or_default()
                } else {
                    p.base_url.to_string()
                };
                let mut question = format!(
                    "Paste your {} API key ({}).{}",
                    p.name,
                    p.key_env,
                    if p.key_url.is_empty() { String::new() } else { format!("\nKeys: {}", p.key_url) }
                );
                for _attempt in 0..3 {
                    let (reply, rx) = tokio::sync::oneshot::channel();
                    let _ = tx2.send(AppEvent::AskUser { question: question.clone(), reply, secret: true });
                    let Some(key) = rx.await.ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()) else {
                        let _ = tx2.send(AppEvent::SystemMessage("Cancelled — no key stored.".into()));
                        return;
                    };
                    let verdict = if base_url.is_empty() {
                        KeyValidation::Unverified("no base URL to test against".into())
                    } else {
                        crate::api::validate_key(p, &base_url, &key).await
                    };
                    if let KeyValidation::Rejected(status) = verdict {
                        question = format!(
                            "{} rejected that key (HTTP {status}). Paste it again, or Esc to cancel.",
                            p.name
                        );
                        continue;
                    }
                    match crate::auth::keystore::save_key(p.key_env, &key) {
                        Ok(path) => {
                            let redacted = crate::auth::Credential::ApiKey(key.clone()).redacted();
                            let note = match verdict {
                                KeyValidation::Valid => String::new(),
                                KeyValidation::Unverified(why) => format!("\n  (could not verify the key: {why})"),
                                KeyValidation::Rejected(_) => unreachable!(),
                            };
                            let shadow_note = match shadow {
                                Some(_) => format!(
                                    "\n  ⚠ {} is also exported in your shell; that value wins on the next launch.",
                                    p.key_env
                                ),
                                None => String::new(),
                            };
                            let _ = tx2.send(AppEvent::SystemMessage(format!(
                                "✓ {} key {redacted} saved to {}{note}{shadow_note}\n  /model {}:{}",
                                p.name,
                                path.display(),
                                p.prefix,
                                if p.default_model.is_empty() { "<model-name>" } else { p.default_model }
                            )));
                            let _ = tx2.send(AppEvent::CredentialChanged(CredentialChange::Provider {
                                prefix: p.prefix.to_string(),
                                key_env: p.key_env.to_string(),
                                value: Some(key),
                            }));
                        }
                        Err(e) => {
                            let _ = tx2.send(AppEvent::SystemMessage(format!("✗ Could not save the key: {e}")));
                        }
                    }
                    return;
                }
                let _ = tx2.send(AppEvent::SystemMessage("Giving up after three rejected keys.".into()));
            });
        }

        CommandAction::LogoutProvider(prefix) => {
            let Some(p) = crate::api::provider_by_prefix(&prefix) else {
                return Ok(());
            };
            if p.key_env.is_empty() {
                app.entries.push(ChatEntry::system(format!("{} stores no key.", p.name)));
                app.scroll_to_bottom();
                return Ok(());
            }
            let msg = match crate::auth::keystore::remove_key(p.key_env) {
                Ok(true) => format!("Removed the stored {} key.", p.name),
                Ok(false) => format!("No stored {} key to remove.", p.name),
                Err(e) => format!("Could not remove the {} key: {e}", p.name),
            };
            let shadow = matches!(
                config.keystore.source(p.key_env),
                Some(crate::auth::keystore::KeySource::ShellEnv | crate::auth::keystore::KeySource::ProjectDotenv | crate::auth::keystore::KeySource::HomeDotenv)
            );
            app.entries.push(ChatEntry::system(if shadow {
                format!("{msg}\n  ⚠ {} still comes from {}; the provider stays configured until you unset it there.", p.key_env, config.keystore.source(p.key_env).map(|s| s.describe()).unwrap_or("elsewhere"))
            } else {
                msg
            }));
            app.scroll_to_bottom();
            if !shadow {
                let _ = tx.send(crate::tui::events::AppEvent::CredentialChanged(
                    crate::tui::events::CredentialChange::Provider {
                        prefix: p.prefix.to_string(),
                        key_env: p.key_env.to_string(),
                        value: None,
                    },
                ));
            }
        }
```

`src/tui/run.rs`: fill the `CredentialChange::Provider` arm:

```rust
                                CredentialChange::Provider { prefix, key_env, value } => {
                                    match value {
                                        Some(v) => config.keystore.set(&key_env, &v, crate::auth::keystore::KeySource::UserDotenv),
                                        None => config.keystore.remove(&key_env),
                                    }
                                    let current_prefix = config.model.split_once(':').map(|(p, _)| p.to_string());
                                    if current_prefix.as_deref() == Some(prefix.as_str()) {
                                        match ApiBackend::from_config(&config) {
                                            Ok(c) => client = c,
                                            Err(e) => app.entries.push(ChatEntry::error(format!("Backend error: {e}"))),
                                        }
                                    }
                                }
```

Every existing `ProviderDef { … }` literal in tests must gain `key_url` — search with `grep -rn "ProviderDef {" src`.

- [ ] **Step 4: Run tests**

Run: `cargo test 2>&1 | grep -E "^test result|FAILED|panicked" && cargo clippy --all-targets 2>&1 | grep -cE "^(warning|error)"`
Expected: all ok; `0`.

Manual check: `/login groq`, paste a bad key → "rejected (HTTP 401)" and the dialog reopens; Esc → "Cancelled". Paste a good key → saved, `/model` lists Groq immediately, `~/.config/oxideclaw/.env` is 0600 with one `GROQ_API_KEY=` line. `/logout groq` removes it.

- [ ] **Step 5: Commit**

```bash
git add src/api/openai_compat.rs src/tui/render.rs src/tui/run/dispatch.rs src/tui/run.rs
git commit -m "tui: /login <provider> with masked entry and validation; /logout <provider>

Keys are validated against the provider's models endpoint before being
saved; 401/403 reopens the dialog. The keystore updates live so the
picker shows the provider without a restart.

Co-Authored-By: Arch Linux <noreply@archlinux.org>"
```

---

## Increment 4: board, doctor, help, docs

### Task 14: The `/login` status board

**Files:**
- Modify: `src/commands/login.rs` (rows), `src/tui/run/dispatch.rs` (`LoginBoard` arm), `src/tui/run/keys.rs:155-200` (overlay selection)

**Interfaces:**
- Produces:
  - `pub fn anthropic_status(config: &Config) -> String`
  - `pub fn provider_status(p: &ProviderDef, keystore: &Keystore) -> String`
  - `pub fn board_rows(config: &Config, ollama_models: &[String]) -> (Vec<String>, Vec<String>)` → `(lines, ids)` where an id is the slash command to run (`"/login anthropic"`, `"/login groq"`, …) or `""` for informational rows.

- [ ] **Step 1: Write the failing tests**

Append to `src/commands/login.rs`:

```rust
#[cfg(test)]
mod board_tests {
    use super::*;
    use crate::auth::keystore::{KeySource, Keystore};

    #[test]
    fn anthropic_row_reflects_the_credential_source() {
        let mut c = crate::config::Config::default();
        assert!(anthropic_status(&c).contains("not signed in"));
        c.api_key = "sk-ant-x".into();
        c.auth_source = Some("ANTHROPIC_API_KEY".into());
        assert!(anthropic_status(&c).contains("API key via ANTHROPIC_API_KEY"));
        let mut creds = crate::auth::profile::ProfileCredentials::new("at", None, Some(crate::auth::oauth::now_unix() + 41 * 60 + 30));
        creds.account_email = Some("a@example.com".into());
        creds.organization_name = Some("Kubereva".into());
        c.auth = crate::auth::AuthHandle::profile(std::env::temp_dir(), "default".into(), None, creds);
        let s = anthropic_status(&c);
        assert!(s.contains("a@example.com") && s.contains("Kubereva") && s.contains("41 min"), "{s}");
    }

    #[test]
    fn provider_rows_show_source_or_the_key_page() {
        let mut ks = Keystore::default();
        ks.set("GROQ_API_KEY", "g", KeySource::ShellEnv);
        let groq = crate::api::provider_by_prefix("groq").unwrap();
        assert_eq!(provider_status(groq, &ks), "key via shell env");
        let deepseek = crate::api::provider_by_prefix("deepseek").unwrap();
        assert!(provider_status(deepseek, &ks).contains("platform.deepseek.com/api_keys"));
        let lm = crate::api::provider_by_prefix("lmstudio").unwrap();
        assert!(provider_status(lm, &ks).contains("LM_STUDIO_HOST"));
    }

    #[test]
    fn rows_are_numbered_in_registry_order_with_commands_as_ids() {
        let c = crate::config::Config::default();
        let (lines, ids) = board_rows(&c, &["llama3".into()]);
        assert_eq!(ids[0], "/login anthropic");
        assert_eq!(ids[1], "/login groq");
        assert_eq!(ids.len(), 1 + crate::api::PROVIDERS.len() + 1);
        assert_eq!(ids.last().unwrap(), "", "Ollama row is informational");
        assert!(lines.iter().any(|l| l.starts_with("  1. Anthropic")), "{lines:?}");
        assert!(lines.iter().any(|l| l.contains("Ollama") && l.contains("1 model")), "{lines:?}");
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib commands::login::board_tests 2>&1 | grep -E "^error" | head -3`
Expected: compile errors.

- [ ] **Step 3: Implement**

Add to `src/commands/login.rs`:

```rust
use crate::api::ProviderDef;
use crate::auth::keystore::Keystore;
use crate::config::Config;

pub fn anthropic_status(config: &Config) -> String {
    if let Some(info) = config.auth.profile_info() {
        let who = match (&info.email, &info.organization) {
            (Some(e), Some(o)) => format!("signed in as {e} · org {o}"),
            (Some(e), None) => format!("signed in as {e}"),
            (None, Some(o)) => format!("signed in · org {o}"),
            (None, None) => format!("signed in (profile '{}')", info.name),
        };
        let expiry = match info.expires_at {
            Some(t) => {
                let left = t - crate::auth::oauth::now_unix();
                if left <= 0 { "token expired, refreshes on use".to_string() } else { format!("expires in {} min", left / 60) }
            }
            None => "no expiry recorded".to_string(),
        };
        return format!("{who} · {expiry}");
    }
    if !config.api_key.is_empty() {
        let kind = if config.auth_is_oauth { "OAuth token" } else { "API key" };
        return format!(
            "{kind} via {}",
            config.auth_source.as_deref().unwrap_or("apiKeyHelper / file descriptor")
        );
    }
    "not signed in · Enter to sign in with your Console account".to_string()
}

pub fn provider_status(p: &ProviderDef, keystore: &Keystore) -> String {
    match p.prefix {
        "lmstudio" => {
            return match std::env::var("LM_STUDIO_HOST") {
                Ok(h) if !h.trim().is_empty() => format!("host {h} (shell env)"),
                _ => "needs LM_STUDIO_HOST in your shell".to_string(),
            };
        }
        "openai-compat" => {
            let url = std::env::var("OPENAI_BASE_URL").ok().filter(|u| !u.trim().is_empty());
            let key = keystore.source("OPENAI_API_KEY");
            return match (url, key) {
                (Some(u), Some(s)) => format!("{u} · key via {}", s.describe()),
                (Some(u), None) => format!("{u} · no OPENAI_API_KEY"),
                (None, _) => "needs OPENAI_BASE_URL in your shell".to_string(),
            };
        }
        _ => {}
    }
    match keystore.source(p.key_env).or_else(|| keystore.source("OPENAI_API_KEY")) {
        Some(s) => format!("key via {}", s.describe()),
        None => format!(
            "not configured · keys at {}",
            p.key_url.trim_start_matches("https://")
        ),
    }
}

/// Board lines and matching selectable ids (a slash command, or "" for an
/// informational row). Numbering follows the picker convention.
pub fn board_rows(config: &Config, ollama_models: &[String]) -> (Vec<String>, Vec<String>) {
    let mut lines = vec!["Credentials\n".to_string()];
    let mut ids = Vec::new();
    let mut n = 1;
    lines.push(format!("  {n}. {:<14} {}", "Anthropic", anthropic_status(config)));
    ids.push("/login anthropic".to_string());
    lines.push(String::new());
    lines.push("── OpenAI-compatible providers ──".to_string());
    for p in crate::api::PROVIDERS {
        n += 1;
        lines.push(format!("  {n}. {:<14} {}", p.name, provider_status(p, &config.keystore)));
        ids.push(format!("/login {}", p.prefix));
    }
    lines.push(String::new());
    lines.push("── Local ──".to_string());
    n += 1;
    let ollama = if ollama_models.is_empty() {
        format!("not reachable at {}", config.ollama_host)
    } else {
        format!(
            "reachable at {} · {} model{}",
            config.ollama_host,
            ollama_models.len(),
            if ollama_models.len() == 1 { "" } else { "s" }
        )
    };
    lines.push(format!("  {n}. {:<14} {ollama}", "Ollama"));
    ids.push(String::new());
    lines.push(String::new());
    lines.push("  Enter puts the row's command in the input · /login <provider> open opens the key page".to_string());
    (lines, ids)
}
```

`src/tui/run/dispatch.rs` `LoginBoard` arm:

```rust
        CommandAction::LoginBoard => {
            let ollama_models = crate::api::list_ollama_models(&config.ollama_host).await;
            let (lines, ids) = crate::commands::login::board_rows(config, &ollama_models);
            app.overlay = Some(Overlay::with_items("login", lines.join("\n"), ids));
        }
```

(`login` must be `pub mod login;` in `commands/mod.rs` for this path, or re-export `board_rows`.)

`src/tui/run/keys.rs`: in both the `Enter` and `'1'..='9'` overlay branches add, before the `else { app.pending_resume = … }` fallback:

```rust
                    } else if title == "login" {
                        if !val.is_empty() {
                            app.pending_help_command = Some(val);
                        }
```

The run loop already turns `pending_help_command` into input-box text (`run.rs:659`), so the user sees `/login groq` ready to send and can append `open` before pressing Enter.

- [ ] **Step 4: Run tests**

Run: `cargo test 2>&1 | grep -E "^test result|FAILED|panicked" && cargo clippy --all-targets 2>&1 | grep -cE "^(warning|error)"`
Expected: all ok; `0`.

Manual check: `/login` shows the board; arrow to Groq, Enter → `/login groq` appears in the input; Enter again starts the masked dialog.

- [ ] **Step 5: Commit**

```bash
git add src/commands/login.rs src/commands/mod.rs src/tui/run/dispatch.rs src/tui/run/keys.rs
git commit -m "tui: /login status board

Co-Authored-By: Arch Linux <noreply@archlinux.org>"
```

### Task 15: Doctor providers, startup messages, system prompt, docs

**Files:**
- Modify: `src/commands/status.rs` (provider lines), `src/tui/run.rs:190-200`, `src/main.rs:698`, `src/query_engine.rs:51-62` (messages), `src/config.rs` `build_system_prompt` (one sentence), `README.md:225-235`, `FEATURES.md` (Authentication section)

**Interfaces:** none new.

- [ ] **Step 1: Write the failing test**

Append to the `tests` module in `src/commands/status.rs` (create `#[cfg(test)] mod doctor_tests` if none exists):

```rust
#[cfg(test)]
mod doctor_tests {
    use super::provider_key_lines;
    use crate::auth::keystore::{KeySource, Keystore};

    #[test]
    fn provider_lines_name_each_configured_provider_and_its_source() {
        let mut ks = Keystore::default();
        ks.set("GROQ_API_KEY", "g", KeySource::UserDotenv);
        ks.set("MISTRAL_API_KEY", "m", KeySource::ShellEnv);
        let lines = provider_key_lines(&ks);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].contains("Groq (~/.config/oxideclaw/.env)"), "{}", lines[0]);
        assert!(lines[0].contains("Mistral (shell env)"), "{}", lines[0]);
        let none = provider_key_lines(&Keystore::default());
        assert!(none[0].contains("/login <provider>"), "{}", none[0]);
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --lib doctor_tests 2>&1 | grep -E "^error" | head -3`
Expected: compile error (`provider_key_lines` not found).

- [ ] **Step 3: Implement**

`src/commands/status.rs`: add a pure helper and call it after the Anthropic block with `checks.extend(provider_key_lines(&ctx.config.keystore));`:

```rust
/// One line naming every provider with a stored key and where it came from,
/// or a hint line when there are none.
pub(super) fn provider_key_lines(keystore: &crate::auth::keystore::Keystore) -> Vec<String> {
    let configured: Vec<String> = crate::api::PROVIDERS
        .iter()
        .filter(|p| !p.key_env.is_empty())
        .filter_map(|p| {
            keystore
                .source(p.key_env)
                .map(|s| format!("{} ({})", p.name, s.describe()))
        })
        .collect();
    if configured.is_empty() {
        vec!["· No provider keys stored — /login <provider> to add one".into()]
    } else {
        vec![format!("✓ Provider keys: {}", configured.join(", "))]
    }
}
```

Startup messages:

- `src/tui/run.rs:190-200`: item 4 becomes `"4. /login                 sign in with your Console account (profile shared with the ant CLI and SDKs)"` and the first line becomes `"No Anthropic credential found. Run `oxideclaw` with a local model, or set one of:"`. Keep the Ollama and provider lines.
- `src/main.rs:698` and `src/query_engine.rs:51-62`: the same list, replacing the `ant auth login` item with `/login`.

`src/config.rs` `build_system_prompt`: find where the prompt describes OxideClaw's slash commands or environment (`grep -n "slash\|/help\|/model" src/config.rs | head`) and add one sentence next to it:

```rust
        prompt.push_str(
            "\nCredentials: the user signs in from inside OxideClaw with /login (Anthropic Console \
             OAuth, or an API key for another provider). Never instruct them to run an external \
             CLI such as `ant auth login` or to paste keys into the chat.\n",
        );
```

`README.md` Quickstart (line 230 area): add above `/model`:

```
/login                  # sign in: Anthropic Console OAuth, or store a provider key
```

`FEATURES.md`: add a `### Authentication` section under "Models & Providers" (after OpenAI-Compatible Providers):

```markdown
### Authentication

`/login` opens a status board: Anthropic, every OpenAI-compatible provider, and Ollama, each with whether a credential is present and where it came from.

**Anthropic.** `/login anthropic` runs Console OAuth (PKCE) in your browser and stores a profile under `~/.config/anthropic/` (`$ANTHROPIC_CONFIG_DIR`) in the same layout the `ant` CLI, the official SDKs, and Claude Code read, so one login serves all of them. Tokens refresh automatically mid-session. `/login anthropic <name>` creates a named profile; `/login anthropic manual` is for SSH or headless hosts (paste the code the Console shows). `/logout` removes the active profile. Usage on a profile is billed as API usage to the org you picked; this is not a Claude subscription login.

Resolution order, first match wins: `ANTHROPIC_API_KEY` → `ANTHROPIC_AUTH_TOKEN` → `OXIDECLAW_API_KEY_FILE_DESCRIPTOR` / `apiKeyHelper` → OAuth profile. An exported key shadows the profile; `/doctor` warns when that happens.

**Providers.** `/login groq` (or any prefix) opens a masked prompt, validates the key against the provider, and saves it to `~/.config/oxideclaw/.env` (0600). `/login groq open` opens the provider's key page first. `/logout groq` removes it. Keys exported in your shell win over the stored file. `OPENAI_BASE_URL` and `LM_STUDIO_HOST` are never read from a file: export them in your shell.

OxideClaw never reads another tool's configuration or credential files.
```

Also update the `/model` row text in the FEATURES.md slash-command table if it still says "(Claude + Ollama)", and add `/login` and `/logout` rows next to it.

- [ ] **Step 4: Run everything**

Run: `cargo test 2>&1 | grep -E "^test result|FAILED|panicked" && cargo clippy --all-targets 2>&1 | grep -cE "^(warning|error)" && cargo fmt --check && echo FMT_OK`
Expected: all ok; `0`; `FMT_OK`.

- [ ] **Step 5: Commit**

```bash
git add src/commands/status.rs src/tui/run.rs src/main.rs src/query_engine.rs src/config.rs README.md FEATURES.md
git commit -m "docs: /login in doctor, startup hints, system prompt, README, FEATURES

Co-Authored-By: Arch Linux <noreply@archlinux.org>"
```

---

## Done criteria

- `cargo test` green on lib and bin targets; `cargo clippy --all-targets` clean; `cargo fmt --check` clean.
- Manual: browser login, manual login, logout, provider key add/reject/cancel/remove, board navigation, doctor output, mid-session refresh (set a profile's `expires_at` in the past and send a message).
- No commit carries any trailer other than `Co-Authored-By: Arch Linux <noreply@archlinux.org>`.
