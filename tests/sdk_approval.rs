// tests/sdk_approval.rs
use oxideclaw::sdk::approval::{ApprovalDecision, PolicyEngine};
use oxideclaw::sdk::protocol::Policy;

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
    assert_eq!(engine.evaluate("Bash"), ApprovalDecision::Deny);
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
    assert_eq!(engine.evaluate("Bash"), ApprovalDecision::Ask);
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
    assert_eq!(engine.evaluate("Edit"), ApprovalDecision::AutoApprove);
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
    assert_eq!(engine.evaluate("Read"), ApprovalDecision::Allow);
}

#[test]
fn test_unlisted_with_interactive_becomes_ask() {
    let policy = Policy::default();
    let engine = PolicyEngine::new(policy, true); // interactive_approval = true
    assert_eq!(engine.evaluate("Bash"), ApprovalDecision::Ask);
}

#[test]
fn test_unlisted_without_interactive_becomes_deny() {
    let policy = Policy::default();
    let engine = PolicyEngine::new(policy, false); // interactive_approval = false
    assert_eq!(engine.evaluate("Bash"), ApprovalDecision::Deny);
}

#[test]
fn test_no_policy_all_tools_ask() {
    let engine = PolicyEngine::new(Policy::default(), true);
    assert_eq!(engine.evaluate("Read"), ApprovalDecision::Ask);
    assert_eq!(engine.evaluate("Write"), ApprovalDecision::Ask);
    assert_eq!(engine.evaluate("Bash"), ApprovalDecision::Ask);
}
