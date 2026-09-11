//! Integration tests for `run_auto_fix_check` against real tmpdir fixtures.
//!
//! The TUI wiring itself is exercised by manual QA; this test pins the
//! public helper's behaviour against synthetic project layouts that
//! exercise each decision branch.

#![cfg(unix)]

use oxideclaw::autofix::{AutoFixAction, AutoFixConfig, AutoFixTrigger, run_auto_fix_check};
use std::fs;
use tempfile::tempdir;

fn base_cfg() -> AutoFixConfig {
    AutoFixConfig {
        enabled: true,
        trigger: AutoFixTrigger::Always,
        lint_command: None,
        test_command: None,
        max_retries: 3,
        timeout_secs: 10,
    }
}

#[test]
fn empty_dir_no_runners_continues_silently() {
    let dir = tempdir().unwrap();
    let cfg = base_cfg();
    let action = run_auto_fix_check(dir.path(), &cfg, "auto-edit", 0);
    assert!(matches!(action, AutoFixAction::Continue { status: None }));
}

#[test]
fn cargo_project_detects_and_passes_with_overrides() {
    let dir = tempdir().unwrap();
    fs::write(
        dir.path().join("Cargo.toml"),
        "[package]\nname=\"x\"\nversion=\"0.1.0\"\n",
    )
    .unwrap();

    let mut cfg = base_cfg();
    cfg.lint_command = Some("true".to_string());
    cfg.test_command = Some("true".to_string());

    let action = run_auto_fix_check(dir.path(), &cfg, "auto-edit", 0);
    match action {
        AutoFixAction::Continue { status } => {
            assert!(status.is_some());
            assert!(status.unwrap().contains("checks passed"));
        }
        other => panic!("expected Continue, got {other:?}"),
    }
}

#[test]
fn lint_fail_under_cap_returns_retry_with_anticheat() {
    let dir = tempdir().unwrap();
    let mut cfg = base_cfg();
    cfg.lint_command = Some("sh -c 'echo \"warning: unused variable\" >&2; exit 1'".to_string());
    cfg.test_command = Some("true".to_string());

    let action = run_auto_fix_check(dir.path(), &cfg, "auto-edit", 0);
    match action {
        AutoFixAction::Retry { feedback, status } => {
            assert!(feedback.contains("Your last edits failed"));
            assert!(feedback.contains("Do not disable lints"));
            assert!(feedback.contains("#[allow"));
            assert!(feedback.contains("(skipped: lint failed)"));
            assert!(status.contains("1/3"));
        }
        other => panic!("expected Retry, got {other:?}"),
    }
}

#[test]
fn cap_reached_returns_giveup_with_working_tree_note() {
    let dir = tempdir().unwrap();
    let mut cfg = base_cfg();
    cfg.lint_command = Some("false".to_string());
    cfg.test_command = Some("true".to_string());

    let action = run_auto_fix_check(dir.path(), &cfg, "auto-edit", 3);
    match action {
        AutoFixAction::GiveUp { status } => {
            assert!(status.contains("cap reached"));
            assert!(status.contains("working tree left as-is"));
        }
        other => panic!("expected GiveUp, got {other:?}"),
    }
}

#[test]
fn trigger_autonomous_skips_in_read_only_mode() {
    let dir = tempdir().unwrap();
    let mut cfg = base_cfg();
    cfg.trigger = AutoFixTrigger::Autonomous;
    cfg.lint_command = Some("false".to_string());

    let action = run_auto_fix_check(dir.path(), &cfg, "read-only", 0);
    assert!(matches!(action, AutoFixAction::Continue { status: None }));
}

#[test]
fn trigger_off_short_circuits() {
    let dir = tempdir().unwrap();
    let mut cfg = base_cfg();
    cfg.trigger = AutoFixTrigger::Off;
    cfg.lint_command = Some("false".to_string());
    cfg.test_command = Some("false".to_string());

    let action = run_auto_fix_check(dir.path(), &cfg, "full-auto", 0);
    assert!(matches!(action, AutoFixAction::Continue { status: None }));
}
