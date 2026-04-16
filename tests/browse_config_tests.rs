#[test]
fn parses_browse_settings_from_json() {
    let json = r#"{"browseMaxSteps": 75, "browseApprovalPatterns": ["force-merge"], "browseDefaultPolicy": "ask"}"#;
    let s: rustyclaw::settings::Settings = serde_json::from_str(json).unwrap();
    assert_eq!(s.browse_max_steps, Some(75));
    assert_eq!(
        s.browse_approval_patterns,
        Some(vec!["force-merge".to_string()])
    );
    assert_eq!(s.browse_default_policy.as_deref(), Some("ask"));
}

#[test]
fn config_default_max_steps_is_fifty() {
    let cfg = rustyclaw::config::Config::default();
    assert_eq!(cfg.browse_max_steps, 50);
}
