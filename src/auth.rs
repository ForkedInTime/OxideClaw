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
//! **Why shell out to `ant` for the profile rather than parsing its JSON.**
//! Tokens minted by `ant auth login` are short-lived and must be refreshed.
//! `ant auth print-credentials --access-token` is the documented way to hand
//! the active credential to a non-SDK client, and it *refreshes the token if
//! needed* before printing. Reading `credentials/<profile>.json` directly would
//! mean reimplementing OAuth refresh against an on-disk format that is an
//! implementation detail. Shelling out keeps us on a supported interface and
//! gets refresh for free — but only when `ant` runs, which is why
//! [`ProfileTokens`] runs it again when the API rejects an expired token.
//!
//! **Wire format differs by credential kind.** A static key goes in `x-api-key`;
//! an OAuth token goes in `Authorization: Bearer` *and* additionally requires
//! the `oauth-2025-04-20` beta header. Sending both auth headers at once is
//! rejected, so exactly one is ever set.

use std::time::{Duration, Instant};

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
    /// `ant auth print-credentials`, for the named profile (or the active one).
    AntProfile(Option<String>),
}

impl CredentialSource {
    pub fn describe(&self) -> String {
        match self {
            CredentialSource::ApiKeyEnv => "ANTHROPIC_API_KEY".into(),
            CredentialSource::AuthTokenEnv => "ANTHROPIC_AUTH_TOKEN".into(),
            CredentialSource::AntProfile(None) => "ant auth login (active profile)".into(),
            CredentialSource::AntProfile(Some(p)) => format!("ant auth login (profile '{p}')"),
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

/// Injection seam so resolution can be tested without mutating process env
/// (which races under the parallel test harness) or requiring `ant` on PATH.
pub trait AuthEnv {
    fn var(&self, key: &str) -> Option<String>;
    /// Active access token via `ant auth print-credentials --access-token`.
    fn ant_access_token(&self) -> Option<String>;
    /// Whether an `ant` profile exists at all — used only to warn that an env
    /// var is shadowing it.
    fn ant_profile_present(&self) -> bool {
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
        if env.ant_profile_present() {
            warnings.push(
                "ANTHROPIC_API_KEY is shadowing your `ant auth login` profile — requests will \
                 use the key's org/workspace, not the profile's. Unset the variable to use the \
                 profile."
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

    if allow_profile && let Some(token) = non_empty(env.ant_access_token()) {
        return Some(Resolved {
            credential: Credential::OAuth(token),
            source: CredentialSource::AntProfile(profile),
            warnings,
        });
    }

    None
}

/// Environment variables only, against the real process environment.
pub fn resolve_env() -> Option<Resolved> {
    resolve_env_with(&ProcessAuthEnv)
}

/// The `ant` profile only, against the real process environment.
pub fn resolve_profile() -> Option<Resolved> {
    let env = ProcessAuthEnv;
    non_empty(env.ant_access_token()).map(|token| Resolved {
        credential: Credential::OAuth(token),
        source: CredentialSource::AntProfile(non_empty(env.var("ANTHROPIC_PROFILE"))),
        warnings: Vec::new(),
    })
}

/// Every access token the `ant` profile has handed this process, newest last.
///
/// `print-credentials` refreshes only when `ant` runs, so the token fetched
/// at startup expired mid-session and every later request failed with 401.
/// Copies of the startup token live on in the config, `/model` rebuilds,
/// sub-agents and WebSearch; mapping any token we issued to the newest one
/// at send time keeps all of them current without threading a handle through.
/// Static keys and `ANTHROPIC_AUTH_TOKEN` are never registered here, so they
/// pass through unchanged and a 401 on them still surfaces immediately.
pub struct ProfileTokens {
    issued: std::sync::RwLock<Vec<String>>,
    /// Held while `fetch` runs, so concurrent 401s refresh once.
    refreshing: std::sync::Mutex<()>,
    fetch: fn() -> Option<String>,
}

/// The process-wide profile token store.
pub static PROFILE_TOKENS: ProfileTokens = ProfileTokens::new(fetch_profile_token);

fn fetch_profile_token() -> Option<String> {
    non_empty(ProcessAuthEnv.ant_access_token())
}

impl ProfileTokens {
    pub const fn new(fetch: fn() -> Option<String>) -> Self {
        Self {
            issued: std::sync::RwLock::new(Vec::new()),
            refreshing: std::sync::Mutex::new(()),
            fetch,
        }
    }

    /// Record a token the profile issued.
    pub fn register(&self, token: &str) {
        let mut issued = self.issued.write().unwrap_or_else(|e| e.into_inner());
        if issued.last().map(String::as_str) != Some(token) {
            issued.push(token.to_string());
        }
    }

    /// The token to send in place of `secret`: the newest profile token if
    /// `secret` is one the profile issued, else `secret` itself.
    pub fn live(&self, secret: &str) -> String {
        let issued = self.issued.read().unwrap_or_else(|e| e.into_inner());
        match issued.last() {
            Some(newest) if issued.iter().any(|t| t == secret) => newest.clone(),
            _ => secret.to_string(),
        }
    }

    /// After the API refused `rejected`, a token worth retrying with: one a
    /// concurrent refresh already fetched, or a fresh one from `ant`. `None`
    /// when `rejected` is not a profile token or `ant` has nothing newer.
    /// Blocks on `ant` for up to [`ANT_TIMEOUT`]; call from `spawn_blocking`.
    pub fn refresh(&self, rejected: &str) -> Option<String> {
        if !self
            .issued
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .any(|t| t == rejected)
        {
            return None;
        }
        let _single_flight = self.refreshing.lock().unwrap_or_else(|e| e.into_inner());
        let newest = self.live(rejected);
        if newest != rejected {
            return Some(newest);
        }
        let fresh = (self.fetch)().filter(|t| t != rejected)?;
        self.register(&fresh);
        Some(fresh)
    }
}

/// Every key `apiKeyHelper` has printed for this process, newest last.
///
/// Helpers exist to mint short-lived keys (vault, gateway, STS), and the
/// key fetched at startup was sent until restart, so a long session failed
/// every request with 401 once it expired. A 401 on a helper key re-runs
/// the helper, exactly as an `ant` profile token is refreshed.
pub static HELPER_KEYS: ProfileTokens = ProfileTokens::new(fetch_helper_key);

/// The helper command `HELPER_KEYS` re-runs; the latest one configured.
static HELPER_CMD: std::sync::RwLock<Option<String>> = std::sync::RwLock::new(None);

fn fetch_helper_key() -> Option<String> {
    let cmd = HELPER_CMD
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone()?;
    run_api_key_helper(&cmd)
        .ok()
        .and_then(|k| non_empty(Some(k)))
}

/// How long an `apiKeyHelper` may run. It is re-run on a 401 mid-session,
/// while the refresh lock is held, so a helper stuck on a prompt (an
/// expired 1Password or vault session) must not wedge every API call.
const HELPER_TIMEOUT: Duration = Duration::from_secs(30);

/// Run an `apiKeyHelper` command: the key is its trimmed stdout. `Err`
/// carries the message to warn with.
///
/// While the TUI owns the terminal (raw mode) the helper runs without a
/// controlling terminal, so a prompt on /dev/tty fails at once instead of
/// drawing over the TUI and fighting it for keystrokes. Before the TUI
/// starts (and in `-p`), an interactive MFA prompt still works.
pub fn run_api_key_helper(cmd: &str) -> Result<String, String> {
    let detach = crossterm::terminal::is_raw_mode_enabled().unwrap_or(false);
    run_api_key_helper_with(cmd, detach, HELPER_TIMEOUT)
}

fn run_api_key_helper_with(cmd: &str, detach: bool, timeout: Duration) -> Result<String, String> {
    use std::io::Read;
    use std::process::{Command, Stdio};

    let piped = |c: &mut Command| {
        c.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
    };
    let mut c = Command::new("sh");
    c.arg("-c").arg(cmd);
    piped(&mut c);
    #[cfg(unix)]
    if detach {
        crate::tools::bash::new_session(&mut c);
    }
    #[cfg(not(unix))]
    let _ = detach;
    let spawned = c.spawn();
    // Stock Windows has no `sh` (Git for Windows only puts git\cmd on PATH),
    // so the helper never ran there. Keep `sh` first for Git Bash/MSYS users
    // with POSIX helpers, and fall back to cmd.exe. `/S /C "<cmd>"` makes cmd
    // strip just the outer quotes; `arg` would escape inner quotes with
    // backslashes, which cmd does not understand.
    #[cfg(windows)]
    let spawned = match spawned {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            use std::os::windows::process::CommandExt;
            let mut c = Command::new("cmd");
            c.args(["/S", "/C"]).raw_arg(format!("\"{cmd}\""));
            piped(&mut c);
            c.spawn()
        }
        other => other,
    };
    let mut child = spawned.map_err(|e| format!("apiKeyHelper could not run: {e}"))?;

    // Drained on threads: a full pipe must not stall the helper, and the
    // deadline must hold even if it never closes them.
    let drain = |pipe: Option<Box<dyn Read + Send>>| {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut p) = pipe {
                let _ = p.read_to_end(&mut buf);
            }
            let _ = tx.send(buf);
        });
        rx
    };
    let out_rx = drain(
        child
            .stdout
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );
    let err_rx = drain(
        child
            .stderr
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                // new_session made the child its group leader: take the
                // whole group, not just `sh`.
                #[cfg(unix)]
                if detach {
                    // SAFETY: kill has no memory-safety preconditions.
                    unsafe {
                        libc::kill(-(child.id() as i32), libc::SIGKILL);
                    }
                }
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "apiKeyHelper timed out after {}s",
                    timeout.as_secs()
                ));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(25)),
            Err(e) => return Err(format!("apiKeyHelper could not run: {e}")),
        }
    };
    // A background grandchild can hold the pipes open after `sh` exits.
    let grace = Duration::from_secs(2);
    let stdout = out_rx.recv_timeout(grace).unwrap_or_default();
    let stderr = err_rx.recv_timeout(grace).unwrap_or_default();
    if status.success() {
        Ok(String::from_utf8_lossy(&stdout).trim().to_string())
    } else {
        Err(format!(
            "apiKeyHelper failed: {}",
            String::from_utf8_lossy(&stderr).trim()
        ))
    }
}

/// Record that `key` came from the helper `cmd`, so a 401 on it (or on any
/// copy of it) re-runs `cmd`.
pub fn register_helper_key(cmd: &str, key: &str) {
    *HELPER_CMD.write().unwrap_or_else(|e| e.into_inner()) = Some(cmd.to_string());
    HELPER_KEYS.register(key);
}

/// Where a credential of this kind is refreshed from: the `ant` profile for
/// OAuth tokens, the apiKeyHelper for keys. A secret neither issued passes
/// through unchanged and is never retried.
pub fn refreshable(is_oauth: bool) -> &'static ProfileTokens {
    if is_oauth {
        &PROFILE_TOKENS
    } else {
        &HELPER_KEYS
    }
}

/// The real environment: process env vars plus the `ant` CLI.
pub struct ProcessAuthEnv;

/// `ant` is a local CLI, but a wedged binary must not hang startup forever.
const ANT_TIMEOUT: Duration = Duration::from_secs(5);

/// Directory `ant auth login` writes profiles to.
///
/// `$ANTHROPIC_CONFIG_DIR`, else `~/.config/anthropic` on Unix and
/// `%APPDATA%\Anthropic` on Windows.
fn anthropic_config_dir() -> Option<std::path::PathBuf> {
    if let Ok(dir) = std::env::var("ANTHROPIC_CONFIG_DIR")
        && !dir.trim().is_empty()
    {
        return Some(std::path::PathBuf::from(dir));
    }
    #[cfg(windows)]
    {
        std::env::var("APPDATA")
            .ok()
            .map(|d| std::path::PathBuf::from(d).join("Anthropic"))
    }
    #[cfg(not(windows))]
    {
        dirs::home_dir().map(|h| h.join(".config").join("anthropic"))
    }
}

/// Has `ant auth login` ever stored a profile on this machine?
///
/// This gate exists because **`ant` is a name collision**: Apache Ant owns that
/// binary name on many systems, including the Windows CI image. Spawning
/// whatever `ant` happens to be on PATH is both wrong and slow — running an
/// unrelated build tool on every startup where no API key is set. Checking for
/// the credentials directory first means we never execute anything unless the
/// real CLI has actually been used here.
fn ant_profile_dir_exists() -> bool {
    profile_dir_exists_at(anthropic_config_dir().as_deref())
}

/// Pure form of the check, so it can be tested without mutating process-global
/// env. `set_var` races under the parallel test harness — the whole point of
/// the `AuthEnv` seam above is to avoid exactly that.
fn profile_dir_exists_at(config_dir: Option<&std::path::Path>) -> bool {
    config_dir.is_some_and(|d| d.join("credentials").is_dir())
}

impl AuthEnv for ProcessAuthEnv {
    fn var(&self, key: &str) -> Option<String> {
        std::env::var(key).ok()
    }

    fn ant_access_token(&self) -> Option<String> {
        if !ant_profile_dir_exists() {
            return None;
        }
        // `--access-token` is required: the bare form prints the whole
        // credentials JSON, which as an Authorization header yields an empty
        // response or an HTTP/2 protocol error rather than an obvious failure.
        run_ant(&["auth", "print-credentials", "--access-token"])
    }

    fn ant_profile_present(&self) -> bool {
        ant_profile_dir_exists()
    }
}

/// Run `ant` with a wall-clock bound, returning trimmed stdout on success.
///
/// Absent `ant` is the common case, not an error — it just means this source
/// does not apply.
fn run_ant(args: &[&str]) -> Option<String> {
    use std::process::{Command, Stdio};

    let mut child = Command::new("ant")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;

    let deadline = Instant::now() + ANT_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    return None;
                }
                break;
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    tracing::warn!("`ant {}` timed out after {ANT_TIMEOUT:?}", args.join(" "));
                    return None;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(_) => return None,
        }
    }

    let out = child.wait_with_output().ok()?;
    let token = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if token.is_empty() { None } else { Some(token) }
}

/// Resolve against the real process environment, full documented chain.
#[allow(dead_code)] // library/SDK entry point; the binary uses the staged variants
pub fn resolve() -> Option<Resolved> {
    resolve_with(&ProcessAuthEnv)
}

#[cfg(test)]
mod profile_token_tests {
    use super::ProfileTokens;

    fn renewed() -> Option<String> {
        Some("t2".into())
    }
    fn unchanged() -> Option<String> {
        Some("t1".into())
    }

    #[test]
    fn every_issued_token_resolves_to_the_newest() {
        let p = ProfileTokens::new(renewed);
        p.register("t1");
        assert_eq!(p.live("t1"), "t1");
        assert_eq!(p.refresh("t1").as_deref(), Some("t2"));
        assert_eq!(p.live("t1"), "t2");
        assert_eq!(p.live("t2"), "t2");
        // A request that raced the refresh with the old token reuses it.
        assert_eq!(p.refresh("t1").as_deref(), Some("t2"));
    }

    #[test]
    fn foreign_credentials_are_never_swapped_or_refreshed() {
        let p = ProfileTokens::new(renewed);
        assert_eq!(p.refresh("env-token"), None);
        p.register("t1");
        assert_eq!(p.live("sk-ant-key"), "sk-ant-key");
        assert_eq!(p.refresh("sk-ant-key"), None);
    }

    #[test]
    fn no_retry_when_ant_returns_the_rejected_token() {
        let p = ProfileTokens::new(unchanged);
        p.register("t1");
        assert_eq!(p.refresh("t1"), None);
    }

    /// The helper ran once at startup, so a rotated key never reached a
    /// running session. A 401 on its key must re-run it.
    #[cfg(unix)]
    #[test]
    fn a_rejected_helper_key_reruns_the_helper() {
        use super::{HELPER_KEYS, register_helper_key, run_api_key_helper};
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("key");
        let cmd = format!("cat '{}'", file.display());
        std::fs::write(&file, "  helper-key-1\n").unwrap();
        let first = run_api_key_helper(&cmd).unwrap();
        assert_eq!(first, "helper-key-1");
        register_helper_key(&cmd, &first);

        std::fs::write(&file, "helper-key-2\n").unwrap();
        assert_eq!(
            HELPER_KEYS.refresh("helper-key-1").as_deref(),
            Some("helper-key-2")
        );
        assert_eq!(HELPER_KEYS.live("helper-key-1"), "helper-key-2");
        // A key the helper never printed is not its to refresh.
        assert_eq!(HELPER_KEYS.refresh("sk-ant-static"), None);

        assert!(
            run_api_key_helper("echo nope >&2; exit 3")
                .unwrap_err()
                .contains("nope")
        );
    }

    /// A helper stuck on a prompt held the refresh lock forever, wedging
    /// every later API call until restart.
    #[cfg(unix)]
    #[test]
    fn a_stuck_helper_times_out() {
        use super::run_api_key_helper_with;
        use std::time::{Duration, Instant};
        for detach in [true, false] {
            let started = Instant::now();
            let err = run_api_key_helper_with("sleep 30", detach, Duration::from_millis(300))
                .unwrap_err();
            assert!(err.contains("timed out"), "{err}");
            assert!(started.elapsed() < Duration::from_secs(5), "{detach}");
        }
        // Detached, a /dev/tty prompt fails instead of waiting for a key.
        let started = Instant::now();
        let r = run_api_key_helper_with("read k </dev/tty; echo $k", true, Duration::from_secs(10));
        assert!(r.is_err() || r.as_deref() == Ok(""), "{r:?}");
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(
            run_api_key_helper_with("echo key", true, Duration::from_secs(10)).as_deref(),
            Ok("key")
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[derive(Default)]
    struct FakeEnv {
        vars: HashMap<String, String>,
        ant_token: Option<String>,
        profile_present: bool,
    }

    impl FakeEnv {
        fn with(mut self, k: &str, v: &str) -> Self {
            self.vars.insert(k.into(), v.into());
            self
        }
        fn with_ant(mut self, token: &str) -> Self {
            self.ant_token = Some(token.into());
            self.profile_present = true;
            self
        }
    }

    impl AuthEnv for FakeEnv {
        fn var(&self, key: &str) -> Option<String> {
            self.vars.get(key).cloned()
        }
        fn ant_access_token(&self) -> Option<String> {
            self.ant_token.clone()
        }
        fn ant_profile_present(&self) -> bool {
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
        let r = resolve_with(&FakeEnv::default().with_ant("sk-ant-oat01-abc")).unwrap();
        assert_eq!(r.credential, Credential::OAuth("sk-ant-oat01-abc".into()));
        assert_eq!(r.source, CredentialSource::AntProfile(None));
    }

    #[test]
    fn profile_name_is_recorded_when_set() {
        let env = FakeEnv::default()
            .with_ant("tok")
            .with("ANTHROPIC_PROFILE", "work");
        let r = resolve_with(&env).unwrap();
        assert_eq!(r.source, CredentialSource::AntProfile(Some("work".into())));
    }

    #[test]
    fn full_order_is_respected() {
        let env = FakeEnv::default()
            .with_ant("from-profile")
            .with("ANTHROPIC_AUTH_TOKEN", "from-token")
            .with("ANTHROPIC_API_KEY", "from-key");
        assert_eq!(
            resolve_with(&env).unwrap().credential,
            Credential::ApiKey("from-key".into())
        );

        let env = FakeEnv::default()
            .with_ant("from-profile")
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
            .with_ant("tok")
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

    /// Regression: `ant` collides with Apache Ant, which ships on the Windows
    /// CI image. Spawning a bare `ant` from PATH on every credential-less
    /// startup ran an unrelated build tool and stalled the process long enough
    /// to fail the headless SDK test. Nothing may be executed unless the real
    /// CLI has actually stored a profile here.
    ///
    /// Pure over the directory — no `set_var`, so it cannot race other tests.
    #[test]
    fn no_subprocess_when_no_profile_directory_exists() {
        let empty = tempfile::tempdir().unwrap();
        assert!(
            !profile_dir_exists_at(Some(empty.path())),
            "a config dir with no credentials/ must not trigger a spawn"
        );
        assert!(
            !profile_dir_exists_at(None),
            "no config dir at all must not trigger a spawn"
        );
    }

    #[test]
    fn profile_directory_is_detected_when_present() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("credentials")).unwrap();
        assert!(
            profile_dir_exists_at(Some(dir.path())),
            "credentials/ present ⇒ the real CLI has been used here"
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
