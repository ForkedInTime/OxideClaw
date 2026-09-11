//! Regression tests for the v0.2.0 security audit (April 2026).
//!
//! Each test pins a specific defense so a future refactor can't silently
//! regress it. Focused on the two CRITICAL items that ship with test-friendly
//! public APIs:
//!
//!   - Path traversal via `resolve_path` (file_read / file_write / file_edit /
//!     multi_edit all share this resolver)
//!   - `apiKeyHelper` shell-injection hardening via settings-file permission
//!     check
//!
//! Both are Unix-only because the POSIX permission model and `/etc/passwd`
//! path conventions are unix-specific.

#![cfg(unix)]

use rustyclaw::tools::file_read::resolve_path;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use tempfile::TempDir;

// ── Path traversal ────────────────────────────────────────────────────────────

#[test]
fn path_traversal_relative_escape_rejected() {
    let td = TempDir::new().unwrap();
    let cwd = td.path().to_path_buf();

    // Classic escape patterns should all be rejected.
    let attacks = [
        "../../../etc/passwd",
        "../etc/passwd",
        "./../../etc/shadow",
        "foo/../../../etc/passwd",
        "foo/../../bar",
    ];
    for attack in attacks {
        let err = resolve_path(attack, &cwd)
            .err()
            .unwrap_or_else(|| panic!("expected '{attack}' to be rejected"));
        let msg = err.to_string();
        assert!(
            msg.contains("escapes working directory"),
            "unexpected error for '{attack}': {msg}"
        );
    }
}

#[test]
fn path_traversal_legitimate_relative_paths_allowed() {
    let td = TempDir::new().unwrap();
    let cwd = td.path().to_path_buf();

    // Normal day-to-day relative paths must still resolve.
    for ok in [
        "src/main.rs",
        "./Cargo.toml",
        "deeply/nested/file.rs",
        "a.txt",
    ] {
        let resolved = resolve_path(ok, &cwd)
            .unwrap_or_else(|e| panic!("legitimate path '{ok}' rejected: {e}"));
        assert!(
            resolved.starts_with(&cwd),
            "resolved path {} not inside cwd {}",
            resolved.display(),
            cwd.display()
        );
    }
}

#[test]
fn path_traversal_dotdot_that_lands_back_inside_cwd_allowed() {
    let td = TempDir::new().unwrap();
    let cwd = td.path().to_path_buf();

    // `src/../Cargo.toml` cleans to `Cargo.toml` — still inside cwd, so allowed.
    let resolved = resolve_path("src/../Cargo.toml", &cwd).expect("should be allowed");
    assert!(resolved.ends_with("Cargo.toml"));
    assert!(resolved.starts_with(&cwd));
}

#[test]
fn path_traversal_absolute_paths_pass_through() {
    let td = TempDir::new().unwrap();
    let cwd = td.path().to_path_buf();

    // Absolute paths bypass the containment check — they're the explicit
    // escape hatch and are still subject to check_sensitive_path downstream.
    // We pin this behavior so a future tightening is a deliberate decision.
    let resolved = resolve_path("/tmp/explicit", &cwd).expect("absolute path allowed");
    assert_eq!(resolved, PathBuf::from("/tmp/explicit"));
}

// ── apiKeyHelper hardening ────────────────────────────────────────────────────
//
// Settings::load is private, so we test the observable end-to-end behavior:
// after writing a settings file with an api_key_helper and setting the mode
// to 0666 (world-writable), loading settings must NOT surface the helper.
// With mode 0600 it must surface.

fn write_settings_file(path: &std::path::Path, helper: &str, mode: u32) {
    let body = format!(r#"{{ "apiKeyHelper": "{helper}" }}"#);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, body).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

#[test]
fn api_key_helper_stripped_when_settings_file_world_writable() {
    let td = TempDir::new().unwrap();
    let cwd = td.path();
    let settings_path = cwd.join(".claude").join("settings.json");

    // World-writable settings file (0666) — the exact shell-injection
    // threat model: any user on the box can drop `"$(malicious)"` in here.
    write_settings_file(&settings_path, "echo sk-test", 0o666);

    let settings = rustyclaw::settings::Settings::load(cwd);
    assert!(
        settings.api_key_helper.is_none(),
        "apiKeyHelper must be stripped when source file is world-writable"
    );
}

#[test]
fn api_key_helper_stripped_when_settings_file_group_writable() {
    let td = TempDir::new().unwrap();
    let cwd = td.path();
    let settings_path = cwd.join(".claude").join("settings.json");

    write_settings_file(&settings_path, "echo sk-test", 0o664);

    let settings = rustyclaw::settings::Settings::load(cwd);
    assert!(
        settings.api_key_helper.is_none(),
        "apiKeyHelper must be stripped when source file is group-writable"
    );
}

#[test]
fn api_key_helper_allowed_when_settings_file_mode_0600() {
    let td = TempDir::new().unwrap();
    let cwd = td.path();
    let settings_path = cwd.join(".claude").join("settings.json");

    write_settings_file(&settings_path, "echo sk-test", 0o600);

    // The file-mode rule, on the file itself.
    let settings = rustyclaw::settings::Settings::load_file(&settings_path);
    assert_eq!(
        settings.api_key_helper.as_deref(),
        Some("echo sk-test"),
        "apiKeyHelper must load from a 0600 file"
    );
}

/// Mode is necessary, not sufficient: a *project's* helper is a shell
/// command shipped by whoever you cloned from. Until the folder is trusted
/// (`/trust` → global `trustedProjects`) it must not run, whatever its mode.
#[test]
fn api_key_helper_from_an_untrusted_project_is_ignored() {
    let td = TempDir::new().unwrap();
    let cwd = td.path();
    let settings_path = cwd.join(".claude").join("settings.json");

    write_settings_file(&settings_path, "echo sk-test", 0o600);

    let settings = rustyclaw::settings::Settings::load(cwd);
    assert!(
        settings.api_key_helper.is_none(),
        "a project apiKeyHelper ran before the user trusted the folder"
    );
    assert!(
        settings
            .untrusted_project_config
            .iter()
            .any(|k| k == "apiKeyHelper"),
        "the user must be told what was ignored"
    );
}

#[test]
fn api_key_helper_allowed_when_settings_file_mode_0644() {
    let td = TempDir::new().unwrap();
    let cwd = td.path();
    let settings_path = cwd.join(".claude").join("settings.json");

    // 0644 is the common "readable by everyone, writable only by owner"
    // convention. The threat is WRITE, not READ, so this is considered safe.
    write_settings_file(&settings_path, "echo sk-test", 0o644);

    let settings = rustyclaw::settings::Settings::load_file(&settings_path);
    assert_eq!(
        settings.api_key_helper.as_deref(),
        Some("echo sk-test"),
        "apiKeyHelper should load from a 0644 file (read-only to others)"
    );
}
