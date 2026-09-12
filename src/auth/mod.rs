//! Anthropic credential resolution.
//!
//! OxideClaw previously read `ANTHROPIC_API_KEY` and nothing else, which meant
//! it ignored credentials the user may already have configured for Claude Code,
//! the official SDKs, or the `ant` CLI — all of which share one resolution
//! order. This module implements that same order so an existing login just
//! works:
//!
//! ```text
//! ANTHROPIC_API_KEY → ANTHROPIC_AUTH_TOKEN → active `ant auth login` profile
//!   → Workload Identity Federation → default profile on disk
//! ```
//!
//! First match wins.
//!
//! **Profiles are read natively.** `ant auth login` (and OxideClaw's own
//! `/login`) store `credentials/<profile>.json` under the Anthropic config
//! dir. We read that file directly and refresh the short-lived token
//! ourselves (see [`oauth`]), so the `ant` binary is never required. This
//! also sidesteps the Apache Ant name collision on PATH.
//!
//! **Wire format differs by credential kind.** A static key goes in `x-api-key`;
//! an OAuth token goes in `Authorization: Bearer` *and* additionally requires
//! the `oauth-2025-04-20` beta header. Sending both auth headers at once is
//! rejected, so exactly one is ever set.

#![allow(dead_code)] // AuthHandle and friends: Task 5 wires them into the HTTP client

pub mod oauth;
pub mod profile;

/// Beta header value required alongside a bearer token.
pub const OAUTH_BETA: &str = "oauth-2025-04-20";

/// How a credential authenticates on the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Credential {
    /// Static API key — sent as `x-api-key`.
    ApiKey(String),
    /// Short-lived OAuth access token — sent as `Authorization: Bearer`,
    /// and requires [`OAUTH_BETA`] in `anthropic-beta`.
    OAuth(String),
}

impl Credential {
    /// The secret itself. Only for redacted status display and for passing to
    /// sub-agents — never log this.
    pub fn secret(&self) -> &str {
        match self {
            Credential::ApiKey(s) | Credential::OAuth(s) => s,
        }
    }

    pub fn is_oauth(&self) -> bool {
        matches!(self, Credential::OAuth(_))
    }

    /// `sk-ant-…` style prefix for status output, never the full secret.
    pub fn redacted(&self) -> String {
        let s = self.secret();
        let head: String = s.chars().take(8).collect();
        match self {
            Credential::ApiKey(_) => format!("{head}… (API key)"),
            Credential::OAuth(_) => format!("{head}… (OAuth token)"),
        }
    }
}

/// Where the winning credential came from — surfaced by `/doctor` so the
/// "stale env var shadows your profile" trap is visible rather than mysterious.
#[derive(Debug, Clone, PartialEq, Eq)]
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

#[derive(Debug, Clone)]
pub struct Resolved {
    pub credential: Credential,
    pub source: CredentialSource,
    /// Non-fatal notes worth showing the user (e.g. a shadowed profile).
    pub warnings: Vec<String>,
}

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
    // Boxed: `ProfileState` is much larger than the other variants.
    Profile(Box<ProfileState>),
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
        Self {
            inner: Arc::new(AuthInner::None),
        }
    }

    pub fn static_credential(c: Credential) -> Self {
        Self {
            inner: Arc::new(AuthInner::Static(c)),
        }
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
            inner: Arc::new(AuthInner::Profile(Box::new(ProfileState {
                dir,
                name,
                client_id,
                base_url,
                creds: Mutex::new(creds),
                refresh_gate: tokio::sync::Mutex::new(()),
            }))),
        }
    }

    /// Test seam: point the refresh at a local server.
    #[cfg(test)]
    pub(crate) fn with_base_url(self, base_url: String) -> Self {
        match Arc::try_unwrap(self.inner) {
            Ok(AuthInner::Profile(mut p)) => {
                p.base_url = base_url;
                Self {
                    inner: Arc::new(AuthInner::Profile(p)),
                }
            }
            Ok(other) => Self {
                inner: Arc::new(other),
            },
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

/// Injection seam so resolution can be tested without mutating process env
/// (which races under the parallel test harness) or requiring `ant` on PATH.
pub trait AuthEnv {
    fn var(&self, key: &str) -> Option<String>;
    /// Access token from the active profile's credentials file (no refresh).
    fn profile_access_token(&self) -> Option<String>;
    /// Whether any profile exists — used only for the shadowing warning.
    fn profile_present(&self) -> bool {
        false
    }
}

/// Treat an empty or whitespace-only value as unset.
///
/// The official SDKs let an empty `ANTHROPIC_API_KEY=""` win its precedence
/// slot and then authenticate with an empty key, producing a confusing 401.
/// We deliberately diverge: an empty value falls through to the next source and
/// the user is told, which is the same outcome they wanted with a clearer path.
fn non_empty(v: Option<String>) -> Option<String> {
    v.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// Resolve a credential from an injected environment. Pure and order-defining.
///
/// This is the canonical full chain. The binary drives the staged variants
/// (`resolve_env` / `resolve_profile`) so OxideClaw's own explicit mechanisms
/// can sit between them; this entry point exists for library/SDK consumers and
/// is what the ordering tests exercise.
#[allow(dead_code)]
pub fn resolve_with(env: &impl AuthEnv) -> Option<Resolved> {
    resolve_stage(env, true)
}

/// Environment variables only — stops before consulting the `ant` profile.
///
/// OxideClaw has two credential mechanisms of its own that predate this module
/// (`OXIDECLAW_API_KEY_FILE_DESCRIPTOR` and `apiKeyHelper`). Both are *explicit*
/// local configuration, whereas an `ant` profile is ambient machine state, so
/// config.rs runs: env vars → fd → helper → profile. Splitting the stages here
/// keeps that ordering without duplicating the env-var precedence rules.
pub fn resolve_env_with(env: &impl AuthEnv) -> Option<Resolved> {
    resolve_stage(env, false)
}

fn resolve_stage(env: &impl AuthEnv, allow_profile: bool) -> Option<Resolved> {
    let mut warnings = Vec::new();

    let api_key = non_empty(env.var("ANTHROPIC_API_KEY"));
    let auth_token = non_empty(env.var("ANTHROPIC_AUTH_TOKEN"));
    let profile = non_empty(env.var("ANTHROPIC_PROFILE"));

    if env.var("ANTHROPIC_API_KEY").is_some() && api_key.is_none() {
        warnings.push(
            "ANTHROPIC_API_KEY is set but empty — ignoring it and falling through to the \
             next credential source. Unset it to silence this."
                .into(),
        );
    }

    // Both set is a hard error at the API: the SDKs send both headers and the
    // request is rejected. Say so here rather than letting it surface as a 401.
    if api_key.is_some() && auth_token.is_some() {
        warnings.push(
            "Both ANTHROPIC_API_KEY and ANTHROPIC_AUTH_TOKEN are set. Using ANTHROPIC_API_KEY \
             (first in the resolution order); unset one to remove the ambiguity."
                .into(),
        );
    }

    if let Some(key) = api_key {
        if env.profile_present() {
            warnings.push(
                "ANTHROPIC_API_KEY is shadowing your OAuth profile — requests will use the \
                 key's org/workspace, not the profile's. Unset the variable to use the profile."
                    .into(),
            );
        }
        return Some(Resolved {
            credential: Credential::ApiKey(key),
            source: CredentialSource::ApiKeyEnv,
            warnings,
        });
    }

    if let Some(token) = auth_token {
        return Some(Resolved {
            credential: Credential::OAuth(token),
            source: CredentialSource::AuthTokenEnv,
            warnings,
        });
    }

    if allow_profile && let Some(token) = non_empty(env.profile_access_token()) {
        let name = profile.unwrap_or_else(|| "default".to_string());
        return Some(Resolved {
            credential: Credential::OAuth(token),
            source: CredentialSource::Profile(name),
            warnings,
        });
    }

    None
}

/// Environment variables only, against the real process environment.
pub fn resolve_env() -> Option<Resolved> {
    resolve_env_with(&ProcessAuthEnv)
}

/// The active profile as a refreshing handle, or `None` when no profile exists.
pub fn load_profile_handle() -> Option<AuthHandle> {
    let dir = profile::config_dir()?;
    let name = profile::resolve_profile_name(
        dir.as_path(),
        std::env::var("ANTHROPIC_PROFILE").ok().as_deref(),
    );
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
        load_profile_handle()
            .and_then(|h| h.snapshot())
            .map(|c| c.secret().to_string())
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

/// Resolve against the real process environment, full documented chain.
#[allow(dead_code)] // library/SDK entry point; the binary uses the staged variants
pub fn resolve() -> Option<Resolved> {
    resolve_with(&ProcessAuthEnv)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[derive(Default)]
    struct FakeEnv {
        vars: HashMap<String, String>,
        profile_token: Option<String>,
        profile_present: bool,
    }

    impl FakeEnv {
        fn with(mut self, k: &str, v: &str) -> Self {
            self.vars.insert(k.into(), v.into());
            self
        }
        fn with_profile(mut self, token: &str) -> Self {
            self.profile_token = Some(token.into());
            self.profile_present = true;
            self
        }
    }

    impl AuthEnv for FakeEnv {
        fn var(&self, key: &str) -> Option<String> {
            self.vars.get(key).cloned()
        }
        fn profile_access_token(&self) -> Option<String> {
            self.profile_token.clone()
        }
        fn profile_present(&self) -> bool {
            self.profile_present
        }
    }

    #[test]
    fn no_credential_anywhere_resolves_to_none() {
        assert!(resolve_with(&FakeEnv::default()).is_none());
    }

    #[test]
    fn api_key_wins_first() {
        let r = resolve_with(&FakeEnv::default().with("ANTHROPIC_API_KEY", "sk-ant-key")).unwrap();
        assert_eq!(r.credential, Credential::ApiKey("sk-ant-key".into()));
        assert_eq!(r.source, CredentialSource::ApiKeyEnv);
    }

    #[test]
    fn auth_token_is_second_and_is_oauth() {
        let r = resolve_with(&FakeEnv::default().with("ANTHROPIC_AUTH_TOKEN", "oat-tok")).unwrap();
        assert_eq!(r.credential, Credential::OAuth("oat-tok".into()));
        assert!(r.credential.is_oauth());
        assert_eq!(r.source, CredentialSource::AuthTokenEnv);
    }

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

    #[test]
    fn full_order_is_respected() {
        let env = FakeEnv::default()
            .with_profile("from-profile")
            .with("ANTHROPIC_AUTH_TOKEN", "from-token")
            .with("ANTHROPIC_API_KEY", "from-key");
        assert_eq!(
            resolve_with(&env).unwrap().credential,
            Credential::ApiKey("from-key".into())
        );

        let env = FakeEnv::default()
            .with_profile("from-profile")
            .with("ANTHROPIC_AUTH_TOKEN", "from-token");
        assert_eq!(
            resolve_with(&env).unwrap().credential,
            Credential::OAuth("from-token".into())
        );
    }

    /// The documented #1 auth trap: a stale exported key silently overrides the
    /// profile, sending requests to a different org/workspace.
    #[test]
    fn shadowed_profile_is_warned_about() {
        let env = FakeEnv::default()
            .with_profile("tok")
            .with("ANTHROPIC_API_KEY", "sk-ant-key");
        let r = resolve_with(&env).unwrap();
        assert_eq!(r.source, CredentialSource::ApiKeyEnv);
        assert!(
            r.warnings.iter().any(|w| w.contains("shadowing")),
            "must warn that the profile is being shadowed: {:?}",
            r.warnings
        );
    }

    /// An empty value would otherwise win its slot and 401 with an empty key.
    #[test]
    fn empty_api_key_falls_through_with_a_warning() {
        let env = FakeEnv::default()
            .with("ANTHROPIC_API_KEY", "")
            .with("ANTHROPIC_AUTH_TOKEN", "tok");
        let r = resolve_with(&env).unwrap();
        assert_eq!(r.credential, Credential::OAuth("tok".into()));
        assert!(
            r.warnings.iter().any(|w| w.contains("empty")),
            "{:?}",
            r.warnings
        );
    }

    #[test]
    fn whitespace_only_values_are_treated_as_unset() {
        let env = FakeEnv::default().with("ANTHROPIC_API_KEY", "   \n ");
        assert!(resolve_with(&env).is_none());
    }

    #[test]
    fn values_are_trimmed() {
        let r =
            resolve_with(&FakeEnv::default().with("ANTHROPIC_API_KEY", "  sk-ant-x\n")).unwrap();
        assert_eq!(r.credential.secret(), "sk-ant-x");
    }

    /// Sending both auth headers is rejected by the API — warn instead of
    /// letting it surface as an opaque 401.
    #[test]
    fn both_env_credentials_set_is_warned_about() {
        let env = FakeEnv::default()
            .with("ANTHROPIC_API_KEY", "k")
            .with("ANTHROPIC_AUTH_TOKEN", "t");
        let r = resolve_with(&env).unwrap();
        assert!(
            r.warnings.iter().any(|w| w.contains("Both")),
            "{:?}",
            r.warnings
        );
    }

    #[test]
    fn redacted_never_leaks_the_whole_secret() {
        let key = Credential::ApiKey("sk-ant-super-secret-value".into());
        let shown = key.redacted();
        assert!(!shown.contains("super-secret-value"), "{shown}");
        assert!(shown.contains("API key"), "{shown}");

        let tok = Credential::OAuth("sk-ant-oat01-secret".into());
        assert!(tok.redacted().contains("OAuth token"));
    }
}

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
        assert!(
            !format!("{h:?}").contains("at"),
            "debug must not leak the token"
        );
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
