// tests/sdk_approval.rs
use oxideclaw::sdk::approval::{ApprovalDecision, PolicyEngine};
use oxideclaw::sdk::protocol::Policy;
use serde_json::json;

#[test]
fn test_deny_takes_priority() {
    let policy = Policy {
        allow: vec!["Bash".into()],
        deny: vec!["Bash".into()],
        auto_approve: vec![],
        ask: vec![],
        approval_timeout_seconds: 60,
    };
    let engine = PolicyEngine::new(policy, true);
    assert_eq!(engine.evaluate("Bash", &json!({})), ApprovalDecision::Deny);
}

#[test]
fn test_ask_triggers_callback() {
    let policy = Policy {
        allow: vec![],
        deny: vec![],
        auto_approve: vec![],
        ask: vec!["Bash".into()],
        approval_timeout_seconds: 60,
    };
    let engine = PolicyEngine::new(policy, true);
    assert_eq!(engine.evaluate("Bash", &json!({})), ApprovalDecision::Ask);
}

#[test]
fn test_auto_approve() {
    let policy = Policy {
        allow: vec![],
        deny: vec![],
        auto_approve: vec!["Edit".into()],
        ask: vec![],
        approval_timeout_seconds: 60,
    };
    let engine = PolicyEngine::new(policy, true);
    assert_eq!(
        engine.evaluate("Edit", &json!({})),
        ApprovalDecision::AutoApprove
    );
}

#[test]
fn test_allow_silent() {
    let policy = Policy {
        allow: vec!["Read".into()],
        deny: vec![],
        auto_approve: vec![],
        ask: vec![],
        approval_timeout_seconds: 60,
    };
    let engine = PolicyEngine::new(policy, true);
    assert_eq!(engine.evaluate("Read", &json!({})), ApprovalDecision::Allow);
}

#[test]
fn test_unlisted_with_interactive_becomes_ask() {
    let policy = Policy::default();
    let engine = PolicyEngine::new(policy, true); // interactive_approval = true
    assert_eq!(engine.evaluate("Bash", &json!({})), ApprovalDecision::Ask);
}

#[test]
fn test_unlisted_without_interactive_becomes_deny() {
    let policy = Policy::default();
    let engine = PolicyEngine::new(policy, false); // interactive_approval = false
    assert_eq!(engine.evaluate("Bash", &json!({})), ApprovalDecision::Deny);
}

#[test]
fn test_no_policy_all_tools_ask() {
    let engine = PolicyEngine::new(Policy::default(), true);
    assert_eq!(engine.evaluate("Read", &json!({})), ApprovalDecision::Ask);
    assert_eq!(engine.evaluate("Write", &json!({})), ApprovalDecision::Ask);
    assert_eq!(engine.evaluate("Bash", &json!({})), ApprovalDecision::Ask);
}

/// The SDK and ACP sessions ignored `/autonomy`: an editor user with
/// `auto-edit` was asked about every in-project edit, and `suggest` let a
/// host's auto_approve list write without a prompt.
#[test]
fn autonomy_fills_in_what_the_host_policy_leaves_open() {
    use oxideclaw::permissions::Autonomy;
    let proj = tempfile::tempdir().unwrap();
    let engine = |autonomy, policy: Policy, interactive| {
        PolicyEngine::new(policy, interactive).with_autonomy(autonomy, proj.path())
    };
    let write = |p: &str| json!({"file_path": p, "content": "x"});
    let bash = json!({"command": "ls"});

    let auto_edit = engine(Autonomy::AutoEdit, Policy::default(), true);
    assert_eq!(
        auto_edit.evaluate("Write", &write("src/a.rs")),
        ApprovalDecision::AutoApprove
    );
    for protected in [".git/hooks/pre-commit", ".env", "package.json", "../x"] {
        assert_eq!(
            auto_edit.evaluate("Write", &write(protected)),
            ApprovalDecision::Ask,
            "{protected}"
        );
    }
    assert_eq!(auto_edit.evaluate("Bash", &bash), ApprovalDecision::Ask);

    // The host's deny and ask lists are never overridden.
    let listed = Policy {
        deny: vec!["Write".into()],
        ask: vec!["Edit".into()],
        ..Policy::default()
    };
    for mode in [Autonomy::AutoEdit, Autonomy::FullAuto] {
        let e = engine(mode, listed.clone(), true);
        assert_eq!(e.evaluate("Write", &write("a.rs")), ApprovalDecision::Deny);
        assert_eq!(e.evaluate("Edit", &write("a.rs")), ApprovalDecision::Ask);
    }

    let full = engine(Autonomy::FullAuto, Policy::default(), false);
    assert_eq!(full.evaluate("Bash", &bash), ApprovalDecision::AutoApprove);
    assert_eq!(
        full.evaluate("Write", &write("/etc/hosts")),
        ApprovalDecision::AutoApprove
    );

    // suggest prompts for edits the host would auto-approve, and refuses
    // them when the host cannot answer a prompt.
    let approving = Policy {
        auto_approve: vec!["Write".into(), "Bash".into()],
        ..Policy::default()
    };
    let suggest = engine(Autonomy::Suggest, approving.clone(), true);
    assert_eq!(
        suggest.evaluate("Write", &write("a.rs")),
        ApprovalDecision::Ask
    );
    assert_eq!(
        suggest.evaluate("Bash", &bash),
        ApprovalDecision::AutoApprove
    );
    let headless = engine(Autonomy::Suggest, approving, false);
    assert_eq!(
        headless.evaluate("Write", &write("a.rs")),
        ApprovalDecision::Deny
    );

    // The default mode leaves the policy as it was.
    let ask = engine(Autonomy::Ask, Policy::default(), true);
    assert_eq!(ask.evaluate("Write", &write("a.rs")), ApprovalDecision::Ask);
}
