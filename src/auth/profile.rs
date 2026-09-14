//! On-disk profile files shared with the `ant` CLI, the official SDKs, and
//! Claude Code. Wire shapes copied from anthropic-sdk-go `config/writers.go`
//! and `config/config.go`; unknown fields are ignored on read.

#![allow(dead_code)]

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
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
    };
    serde_json::from_str(&text)
        .map(Some)
        .with_context(|| format!("parse {}", path.display()))
}

pub fn load_credentials(dir: &Path, profile: &str) -> Result<Option<ProfileCredentials>> {
    let path = credentials_path(dir, profile);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
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
    write_secret_file(
        &credentials_path(dir, profile),
        serde_json::to_vec_pretty(creds)?,
    )
}

pub fn set_active_profile(dir: &Path, profile: &str) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    write_atomic(
        &dir.join("active_config"),
        format!("{profile}\n").into_bytes(),
        0o644,
    )
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
    if std::fs::read_to_string(&pointer)
        .map(|s| s.trim() == profile)
        .unwrap_or(false)
    {
        let _ = std::fs::remove_file(&pointer);
    }
    Ok(removed)
}

fn write_secret_file(path: &Path, bytes: Vec<u8>) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("no parent for {}", path.display()))?;
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
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("no parent for {}", path.display()))?;
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
        assert_eq!(
            resolve_profile_name(d.path(), Some("  ")),
            "work",
            "blank env is unset"
        );
    }

    #[test]
    fn delete_removes_both_files_and_clears_the_pointer() {
        let d = tmp();
        save_credentials(d.path(), "work", &ProfileCredentials::new("at", None, None)).unwrap();
        save_config(
            d.path(),
            "work",
            &ProfileConfig::user_oauth("cid", None, None),
        )
        .unwrap();
        set_active_profile(d.path(), "work").unwrap();
        assert!(profile_exists(d.path(), "work"));
        assert!(delete_profile(d.path(), "work").unwrap());
        assert!(!profile_exists(d.path(), "work"));
        assert!(!d.path().join("active_config").exists());
        assert!(
            !delete_profile(d.path(), "work").unwrap(),
            "second delete is a no-op"
        );
    }

    #[test]
    fn permission_errors_are_propagated_not_silenced() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            // Skip test if running as root (can read files with 0o000 permissions)
            if std::env::var("USER").is_ok_and(|u| u == "root") {
                return;
            }

            let d = tmp();
            std::fs::create_dir_all(d.path().join("credentials")).unwrap();
            let path = d.path().join("credentials/default.json");
            std::fs::write(&path, "{}").unwrap();

            // Make file unreadable
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();

            // Verify it's actually unreadable (detect the error before assertion)
            let is_unreadable = std::fs::read_to_string(&path).is_err();
            if !is_unreadable {
                // Running as root or permissions don't work as expected; skip the test
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
                return;
            }

            // load_credentials should return Err with "read" context, not Ok(None)
            let err = load_credentials(d.path(), "default").unwrap_err();
            assert!(
                err.to_string().contains("read"),
                "error should mention 'read': {err}"
            );

            // Restore permissions so tempdir cleanup works
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
    }
}
