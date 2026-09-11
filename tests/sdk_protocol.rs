// tests/sdk_protocol.rs
use oxideclaw::sdk::protocol::*;

#[test]
fn test_session_start_deserialize() {
    let json = r#"{
        "id": "req-1",
        "type": "session/start",
        "prompt": "Fix the tests",
        "cwd": "/tmp/project",
        "model": "claude-sonnet-4-6-20250514",
        "max_turns": 10,
        "max_budget_usd": 5.0,
        "record": true,
        "policy": {
            "allow": ["Read", "Glob"],
            "auto_approve": ["Edit"],
            "deny": [],
            "ask": ["Bash"]
        },
        "capabilities": {
            "show_diff": true,
            "interactive_approval": true
        }
    }"#;
    let req: SdkRequest = serde_json::from_str(json).unwrap();
    match req {
        SdkRequest::SessionStart {
            id,
            prompt,
            cwd,
            model,
            max_turns,
            max_budget_usd,
            record,
            policy,
            ..
        } => {
            assert_eq!(id, "req-1");
            assert_eq!(prompt, "Fix the tests");
            assert_eq!(cwd.as_deref(), Some("/tmp/project"));
            assert_eq!(model.as_deref(), Some("claude-sonnet-4-6-20250514"));
            assert_eq!(max_turns, Some(10));
            assert!((max_budget_usd.unwrap() - 5.0).abs() < 0.001);
            assert_eq!(record, Some(true));
            let p = policy.unwrap();
            assert_eq!(p.allow, vec!["Read", "Glob"]);
            assert_eq!(p.ask, vec!["Bash"]);
        }
        other => panic!("Expected SessionStart, got {:?}", other),
    }
}

#[test]
fn test_turn_start_deserialize() {
    let json = r#"{"id":"req-2","type":"turn/start","session_id":"abc-123","prompt":"Add error handling"}"#;
    let req: SdkRequest = serde_json::from_str(json).unwrap();
    match req {
        SdkRequest::TurnStart {
            id,
            session_id,
            prompt,
        } => {
            assert_eq!(id, "req-2");
            assert_eq!(session_id, "abc-123");
            assert_eq!(prompt, "Add error handling");
        }
        other => panic!("Expected TurnStart, got {:?}", other),
    }
}

#[test]
fn test_tool_approve_deserialize() {
    let json = r#"{"id":"req-3","type":"tool/approve","approval_id":"appr-1"}"#;
    let req: SdkRequest = serde_json::from_str(json).unwrap();
    match req {
        SdkRequest::ToolApprove { id, approval_id } => {
            assert_eq!(id, "req-3");
            assert_eq!(approval_id, "appr-1");
        }
        other => panic!("Expected ToolApprove, got {:?}", other),
    }
}

#[test]
fn test_tool_deny_with_reason_deserialize() {
    let json =
        r#"{"id":"req-4","type":"tool/deny","approval_id":"appr-1","reason":"No shell in CI"}"#;
    let req: SdkRequest = serde_json::from_str(json).unwrap();
    match req {
        SdkRequest::ToolDeny {
            id,
            approval_id,
            reason,
        } => {
            assert_eq!(id, "req-4");
            assert_eq!(approval_id, "appr-1");
            assert_eq!(reason.as_deref(), Some("No shell in CI"));
        }
        other => panic!("Expected ToolDeny, got {:?}", other),
    }
}

#[test]
fn test_notification_serialize() {
    let notif = SdkNotification::MessageDelta {
        session_id: "abc-123".into(),
        content: "Looking at the code".into(),
    };
    let json = serde_json::to_string(&notif).unwrap();
    assert!(json.contains(r#""type":"message/delta""#));
    assert!(json.contains(r#""session_id":"abc-123""#));
    assert!(json.contains(r#""content":"Looking at the code""#));
}

#[test]
fn test_turn_completed_serialize() {
    let notif = SdkNotification::TurnCompleted {
        session_id: "abc-123".into(),
        response: "Fixed 3 tests".into(),
        structured_output: None,
        cost_usd: 0.03,
        total_session_cost_usd: 0.12,
        tokens: TokenUsage {
            input: 5000,
            output: 2000,
        },
        model: "claude-sonnet-4-6-20250514".into(),
        tools_used: vec!["Read".into(), "Edit".into()],
        duration_ms: 12500,
    };
    let json = serde_json::to_string(&notif).unwrap();
    assert!(json.contains(r#""type":"turn/completed""#));
    assert!(json.contains(r#""cost_usd""#));
    assert!(json.contains(r#""tools_used""#));
}

#[test]
fn test_cost_updated_serialize() {
    let notif = SdkNotification::CostUpdated {
        session_id: "abc-123".into(),
        turn_cost_usd: 0.03,
        session_total_usd: 0.12,
        budget_remaining_usd: Some(4.88),
        input_tokens: 1500,
        output_tokens: 800,
        model: "claude-sonnet-4-6-20250514".into(),
    };
    let json = serde_json::to_string(&notif).unwrap();
    assert!(json.contains(r#""type":"cost/updated""#));
    assert!(json.contains(r#""budget_remaining_usd":4.88"#));
}

#[test]
fn test_error_response_serialize() {
    let resp = SdkResponse::Error {
        id: "req-5".into(),
        code: "session_not_found".into(),
        message: "No session with ID xyz".into(),
    };
    let json = serde_json::to_string(&resp).unwrap();
    assert!(json.contains(r#""type":"error""#));
    assert!(json.contains(r#""code":"session_not_found""#));
}

#[test]
fn test_health_check_response_serialize() {
    let resp = SdkResponse::HealthCheck {
        id: "req-6".into(),
        status: "ok".into(),
        version: "0.1.0".into(),
        protocol_version: 1,
        active_sessions: 1,
        uptime_seconds: 3600,
    };
    let json = serde_json::to_string(&resp).unwrap();
    assert!(json.contains(r#""type":"health/check""#));
    assert!(json.contains(r#""active_sessions":1"#));
}
