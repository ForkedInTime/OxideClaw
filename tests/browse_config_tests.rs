#[test]
fn parses_browse_settings_from_json() {
    let json = r#"{"browseMaxSteps": 75, "browseApprovalPatterns": ["force-merge"], "browseDefaultPolicy": "ask"}"#;
    let s: oxideclaw::settings::Settings = serde_json::from_str(json).unwrap();
    assert_eq!(s.browse_max_steps, Some(75));
    assert_eq!(
        s.browse_approval_patterns,
        Some(vec!["force-merge".to_string()])
    );
    assert_eq!(s.browse_default_policy.as_deref(), Some("ask"));
}

#[test]
fn config_default_max_steps_is_fifty() {
    let cfg = oxideclaw::config::Config::default();
    assert_eq!(cfg.browse_max_steps, 50);
}

/// settings.json may come from an untrusted cloned repo; it must never be
/// able to switch off the browser approval gate.
#[test]
fn settings_cannot_select_yolo_policy() {
    use oxideclaw::browser::browse_loop::BrowsePolicy;
    assert_eq!(
        BrowsePolicy::from_settings_str("yolo"),
        BrowsePolicy::Pattern
    );
    assert_eq!(
        BrowsePolicy::from_settings_str(" YOLO "),
        BrowsePolicy::Pattern
    );
    assert_eq!(BrowsePolicy::from_settings_str("ask"), BrowsePolicy::Ask);
    assert_eq!(BrowsePolicy::from_settings_str(""), BrowsePolicy::Pattern);
}
