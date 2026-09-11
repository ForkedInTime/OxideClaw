//! Integration tests for auto-rollback core logic — TDD, written before implementation.

use oxideclaw::autofix::{AutoFixConfig, AutoFixTrigger, detect_test_command, should_trigger};
use std::fs;
use tempfile::TempDir;

// ── detect_test_command ──────────────────────────────────────────────────────

#[test]
fn test_detect_rust_project() {
    let tmp = TempDir::new().unwrap();
    fs::write(tmp.path().join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
    let got = detect_test_command(tmp.path(), &None);
    assert_eq!(got, Some("cargo test".to_string()));
}

#[test]
fn test_detect_node_project() {
    let tmp = TempDir::new().unwrap();
    fs::write(tmp.path().join("package.json"), "{}").unwrap();
    let got = detect_test_command(tmp.path(), &None);
    assert_eq!(got, Some("npm test".to_string()));
}

#[test]
fn test_detect_python_project_pyproject() {
    let tmp = TempDir::new().unwrap();
    fs::write(tmp.path().join("pyproject.toml"), "[project]\nname=\"x\"\n").unwrap();
    let got = detect_test_command(tmp.path(), &None);
    assert_eq!(got, Some("pytest".to_string()));
}

#[test]
fn test_detect_python_project_setup() {
    let tmp = TempDir::new().unwrap();
    fs::write(
        tmp.path().join("setup.py"),
        "from setuptools import setup\n",
    )
    .unwrap();
    let got = detect_test_command(tmp.path(), &None);
    assert_eq!(got, Some("pytest".to_string()));
}

#[test]
fn test_detect_go_project() {
    let tmp = TempDir::new().unwrap();
    fs::write(tmp.path().join("go.mod"), "module x\n").unwrap();
    let got = detect_test_command(tmp.path(), &None);
    assert_eq!(got, Some("go test ./...".to_string()));
}

#[test]
fn test_detect_no_runner() {
    let tmp = TempDir::new().unwrap();
    let got = detect_test_command(tmp.path(), &None);
    assert_eq!(got, None);
}

#[test]
fn test_custom_override() {
    let tmp = TempDir::new().unwrap();
    let override_cmd = Some("make test".to_string());
    let got = detect_test_command(tmp.path(), &override_cmd);
    assert_eq!(got, Some("make test".to_string()));
}

// ── should_trigger ────────────────────────────────────────────────────────────

#[test]
fn test_should_trigger_off() {
    let cfg = AutoFixConfig {
        trigger: AutoFixTrigger::Off,
        ..AutoFixConfig::default()
    };
    assert!(!should_trigger(&cfg, "auto-edit"));
    assert!(!should_trigger(&cfg, "full-auto"));
    assert!(!should_trigger(&cfg, "read-only"));
}

#[test]
fn test_should_trigger_always() {
    let cfg = AutoFixConfig {
        trigger: AutoFixTrigger::Always,
        ..AutoFixConfig::default()
    };
    assert!(should_trigger(&cfg, "read-only"));
    assert!(should_trigger(&cfg, "plan-only"));
    assert!(should_trigger(&cfg, "auto-edit"));
    assert!(should_trigger(&cfg, "full-auto"));
}

#[test]
fn test_should_trigger_autonomous_auto_edit() {
    let cfg = AutoFixConfig {
        trigger: AutoFixTrigger::Autonomous,
        ..AutoFixConfig::default()
    };
    assert!(should_trigger(&cfg, "auto-edit"));
    assert!(should_trigger(&cfg, "full-auto"));
}

#[test]
fn test_should_trigger_autonomous_read_only() {
    let cfg = AutoFixConfig {
        trigger: AutoFixTrigger::Autonomous,
        ..AutoFixConfig::default()
    };
    assert!(!should_trigger(&cfg, "read-only"));
    assert!(!should_trigger(&cfg, "plan-only"));
}

#[test]
fn test_should_trigger_disabled() {
    let cfg = AutoFixConfig {
        enabled: false,
        trigger: AutoFixTrigger::Always,
        ..AutoFixConfig::default()
    };
    assert!(!should_trigger(&cfg, "auto-edit"));
    assert!(!should_trigger(&cfg, "full-auto"));
}

// ── default config ────────────────────────────────────────────────────────────

#[test]
fn test_default_config() {
    let cfg = AutoFixConfig::default();
    assert!(cfg.enabled);
    assert_eq!(cfg.trigger, AutoFixTrigger::Autonomous);
    assert_eq!(cfg.max_retries, 3);
    assert!(cfg.test_command.is_none());
    assert_eq!(
        cfg.timeout_secs,
        oxideclaw::autofix::DEFAULT_TEST_TIMEOUT_SECS
    );
}

// ── settings parsing ──────────────────────────────────────────────────────────

#[test]
fn test_settings_parse_auto_rollback_full() {
    let json = r#"{
        "autoRollback": {
            "enabled": true,
            "trigger": "always",
            "testCommand": "cargo test --all",
            "maxRetries": 5,
            "timeoutSecs": 120
        }
    }"#;
    let s: oxideclaw::settings::Settings = serde_json::from_str(json).unwrap();
    let ar = s.auto_fix.expect("autoRollback missing");
    assert_eq!(ar.enabled, Some(true));
    assert_eq!(ar.trigger.as_deref(), Some("always"));
    assert_eq!(ar.test_command.as_deref(), Some("cargo test --all"));
    assert_eq!(ar.max_retries, Some(5));
    assert_eq!(ar.timeout_secs, Some(120));
}

#[test]
fn test_settings_parse_auto_rollback_trigger_case_insensitive() {
    // The parser itself keeps the raw string; Config::load lowercases it.
    // Verify by hand that the three canonical variants survive JSON parse.
    for t in &["autonomous", "AUTONOMOUS", "Always", "off", "Off"] {
        let json = format!(r#"{{"autoRollback": {{"trigger": "{t}"}}}}"#);
        let s: oxideclaw::settings::Settings = serde_json::from_str(&json).unwrap();
        let ar = s.auto_fix.expect("autoRollback missing");
        assert_eq!(ar.trigger.as_deref(), Some(*t));
    }
}
