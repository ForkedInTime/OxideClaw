use rustyclaw::browser::approval_gate::{ApprovalGate, GateContext, GateVerdict};

fn gate() -> ApprovalGate {
    ApprovalGate::default()
}

fn ctx(tool: &str) -> GateContext {
    GateContext {
        tool_name: tool.into(),
        ..Default::default()
    }
}

// 1. Read-only tools always pass.
#[test]
fn allows_plain_read_tools() {
    let g = gate();
    for tool in &[
        "browser_navigate",
        "browser_snapshot",
        "browser_screenshot",
        "browser_get_text",
        "browser_wait",
        "browse_done",
    ] {
        let c = ctx(tool);
        assert_eq!(g.check(&c), GateVerdict::Allow, "tool {tool} should Allow");
    }
}

// 2. /checkout → RequireConfirmation
#[test]
fn trips_on_checkout_url() {
    let g = gate();
    let c = GateContext {
        tool_name: "browser_click".into(),
        url: "https://shop.example.com/checkout".into(),
        ..Default::default()
    };
    assert!(matches!(g.check(&c), GateVerdict::RequireConfirmation { .. }));
}

// 3. Article about checkout should not trip (path is /articles/checkout-guide).
#[test]
fn does_not_trip_on_article_about_checkout() {
    let g = gate();
    let c = GateContext {
        tool_name: "browser_click".into(),
        url: "https://blog.example.com/articles/checkout-guide".into(),
        target_text: "Read more".into(),
        ..Default::default()
    };
    assert_eq!(g.check(&c), GateVerdict::Allow);
}

// 4. "Confirm Purchase" → trips
#[test]
fn trips_on_confirm_purchase_button() {
    let g = gate();
    let c = GateContext {
        tool_name: "browser_click".into(),
        target_text: "Confirm Purchase".into(),
        ..Default::default()
    };
    assert!(matches!(g.check(&c), GateVerdict::RequireConfirmation { .. }));
}

// 5. "Start Free Trial" → trips
#[test]
fn trips_on_start_free_trial_autobill() {
    let g = gate();
    let c = GateContext {
        tool_name: "browser_click".into(),
        target_text: "Start Free Trial".into(),
        ..Default::default()
    };
    assert!(matches!(g.check(&c), GateVerdict::RequireConfirmation { .. }));
}

// 6. Bare "Submit" does NOT trip.
#[test]
fn does_not_trip_on_bare_submit() {
    let g = gate();
    let c = GateContext {
        tool_name: "browser_click".into(),
        target_text: "Submit".into(),
        ..Default::default()
    };
    assert_eq!(g.check(&c), GateVerdict::Allow);
}

// 7. /oauth/authorize → trips
#[test]
fn trips_on_oauth_authorize_url() {
    let g = gate();
    let c = GateContext {
        tool_name: "browser_click".into(),
        url: "https://accounts.google.com/oauth/authorize?client_id=abc".into(),
        ..Default::default()
    };
    assert!(matches!(g.check(&c), GateVerdict::RequireConfirmation { .. }));
}

// 8. "Delete Account" → trips
#[test]
fn trips_on_delete_account() {
    let g = gate();
    let c = GateContext {
        tool_name: "browser_click".into(),
        target_text: "Delete Account".into(),
        ..Default::default()
    };
    assert!(matches!(g.check(&c), GateVerdict::RequireConfirmation { .. }));
}

// 9. "$12.99" → trips, reason contains "visible_price"
#[test]
fn trips_on_visible_price() {
    let g = gate();
    let c = GateContext {
        tool_name: "browser_click".into(),
        visible_prices: vec!["$12.99".into()],
        ..Default::default()
    };
    match g.check(&c) {
        GateVerdict::RequireConfirmation { reason, .. } => {
            assert!(reason.contains("visible_price"), "reason should mention visible_price, got: {reason}");
        }
        GateVerdict::Allow => panic!("expected RequireConfirmation for $12.99"),
    }
}

// 10. "$0.00" → Allow (zero price)
#[test]
fn zero_price_does_not_trip() {
    let g = gate();
    let c = GateContext {
        tool_name: "browser_click".into(),
        visible_prices: vec!["$0.00".into()],
        ..Default::default()
    };
    assert_eq!(g.check(&c), GateVerdict::Allow);
}

// 11. Password field → trips
#[test]
fn trips_on_password_field() {
    let g = gate();
    let c = GateContext {
        tool_name: "browser_fill".into(),
        form_field_signals: vec!["input:type=password".into()],
        ..Default::default()
    };
    assert!(matches!(g.check(&c), GateVerdict::RequireConfirmation { .. }));
}

// 12. CC autocomplete → trips
#[test]
fn trips_on_cc_autocomplete() {
    let g = gate();
    let c = GateContext {
        tool_name: "browser_fill".into(),
        form_field_signals: vec!["input:autocomplete=cc-number".into()],
        ..Default::default()
    };
    assert!(matches!(g.check(&c), GateVerdict::RequireConfirmation { .. }));
}

// 13. User extension pattern matches → trips
#[test]
fn user_extension_patterns_append() {
    let g = ApprovalGate::with_user_patterns(vec!["force-merge".into()]);
    let c = GateContext {
        tool_name: "browser_click".into(),
        target_text: "force-merge into main".into(),
        ..Default::default()
    };
    assert!(matches!(g.check(&c), GateVerdict::RequireConfirmation { .. }));
}

// 14. Invalid user regex is skipped — no panic, still Allow for benign context.
#[test]
fn invalid_user_pattern_is_skipped_not_crash() {
    let g = ApprovalGate::with_user_patterns(vec!["[".into()]);
    let c = GateContext {
        tool_name: "browser_click".into(),
        target_text: "OK".into(),
        ..Default::default()
    };
    assert_eq!(g.check(&c), GateVerdict::Allow);
}
