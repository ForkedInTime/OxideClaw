//! Integration tests for the autonomous browser agent.
//!
//! Tests middleware composition, sentinel parsing, type serialization,
//! stagnation termination, and denial-counter termination.
//! No network, no Chrome — all in-process with channels.

use rustyclaw::browser::approval_gate::{ApprovalGate, ApprovalGateMiddleware, GateContext, GateVerdict};
use rustyclaw::browser::browse_loop::{BrowsePolicy, BrowseReason, BrowseResult};
use rustyclaw::browser::loop_detector::LoopDetectorMiddleware;
use rustyclaw::browser::middleware::{MiddlewareVerdict, ToolMiddleware};
use serde_json::json;
use std::sync::atomic::AtomicU32;
use std::sync::Arc;

// ── Approval gate (standalone) ──────────────────────────────────────────────

#[test]
fn gate_allows_read_only_tools() {
    let gate = ApprovalGate::default();
    let ctx = GateContext {
        tool_name: "browser_navigate".into(),
        url: "https://evil.com/checkout".into(),
        target_text: "Go".into(),
        ..Default::default()
    };
    assert!(matches!(gate.check(&ctx), GateVerdict::Allow));
}

#[test]
fn gate_trips_on_checkout_url() {
    let gate = ApprovalGate::default();
    let ctx = GateContext {
        tool_name: "browser_click".into(),
        url: "https://shop.example.com/checkout".into(),
        target_text: "Continue".into(),
        ..Default::default()
    };
    assert!(matches!(gate.check(&ctx), GateVerdict::RequireConfirmation { .. }));
}

#[test]
fn gate_ignores_article_about_checkout() {
    let gate = ApprovalGate::default();
    let ctx = GateContext {
        tool_name: "browser_click".into(),
        url: "https://blog.example.com/articles/checkout-guide".into(),
        target_text: "Read More".into(),
        ..Default::default()
    };
    assert!(matches!(gate.check(&ctx), GateVerdict::Allow));
}

// ── Loop detector middleware ────────────────────────────────────────────────

#[tokio::test]
async fn loop_detector_middleware_fires_nudge() {
    let (nudge_tx, mut nudge_rx) = tokio::sync::mpsc::channel(32);
    let mw = LoopDetectorMiddleware::new(nudge_tx);

    // 3 identical after_tool calls → stagnation
    for _ in 0..3 {
        mw.after_tool("browser_click", "same page content").await;
    }

    let nudge = nudge_rx.try_recv().expect("should have received a nudge");
    assert!(nudge.contains("different approach"));
}

#[tokio::test]
async fn loop_detector_stops_at_level_three() {
    let (nudge_tx, mut nudge_rx) = tokio::sync::mpsc::channel(32);
    let mw = LoopDetectorMiddleware::new(nudge_tx);

    // Fire 3 levels of nudges — each fires after 3 identical after_tool calls
    // But check_stagnation fires every call once threshold is met
    for _ in 0..9 {
        mw.after_tool("browser_click", "same").await;
    }

    // Drain nudges
    let mut nudges = Vec::new();
    while let Ok(n) = nudge_rx.try_recv() {
        nudges.push(n);
    }
    assert!(!nudges.is_empty(), "should have received nudges");
    assert!(nudges.last().unwrap().contains("Stopping"), "last nudge should be terminal");

    // After L3, before_tool should return Deny
    let verdict = mw.before_tool("browser_click", &json!({})).await;
    assert!(matches!(verdict, MiddlewareVerdict::Deny { .. }));
    assert!(mw.is_stopped());
}

#[tokio::test]
async fn loop_detector_resets_on_navigate() {
    let (nudge_tx, _nudge_rx) = tokio::sync::mpsc::channel(32);
    let mw = LoopDetectorMiddleware::new(nudge_tx);

    mw.after_tool("browser_click", "same").await;
    mw.after_tool("browser_click", "same").await;
    // Navigate resets
    mw.after_tool("browser_navigate", "new page").await;
    mw.after_tool("browser_click", "same").await;
    // Only 1 click after reset — not enough for stagnation
    assert!(!mw.is_stopped());
}

// ── Approval gate middleware ────────────────────────────────────────────────

#[tokio::test]
async fn approval_middleware_allows_read_tools() {
    let gate = ApprovalGate::default();
    let current_url = Arc::new(tokio::sync::Mutex::new("https://example.com".into()));
    let (approval_tx, _) = tokio::sync::mpsc::channel(4);
    let step = Arc::new(AtomicU32::new(0));

    let mw = ApprovalGateMiddleware::new(gate, BrowsePolicy::Pattern, current_url, approval_tx, step, false);

    let verdict = mw.before_tool("browser_navigate", &json!({"url": "https://example.com"})).await;
    assert!(matches!(verdict, MiddlewareVerdict::Allow));
}

#[tokio::test]
async fn approval_middleware_yolo_allows_everything() {
    let gate = ApprovalGate::default();
    let current_url = Arc::new(tokio::sync::Mutex::new("https://shop.com/checkout".into()));
    let (approval_tx, _) = tokio::sync::mpsc::channel(4);
    let step = Arc::new(AtomicU32::new(0));

    let mw = ApprovalGateMiddleware::new(gate, BrowsePolicy::Yolo, current_url, approval_tx, step, false);

    let verdict = mw.before_tool("browser_click", &json!({"ref": "@e1"})).await;
    assert!(matches!(verdict, MiddlewareVerdict::Allow));
}

#[tokio::test]
async fn approval_middleware_denies_on_dropped_channel() {
    let gate = ApprovalGate::default();
    let current_url = Arc::new(tokio::sync::Mutex::new("https://shop.com/checkout".into()));
    let (approval_tx, approval_rx) = tokio::sync::mpsc::channel(4);
    let step = Arc::new(AtomicU32::new(0));

    drop(approval_rx);

    let mw = ApprovalGateMiddleware::new(gate, BrowsePolicy::Pattern, current_url, approval_tx, step, false);

    let verdict = mw.before_tool("browser_click", &json!({"ref": "@e1"})).await;
    assert!(matches!(verdict, MiddlewareVerdict::Deny { .. }));
}

// ── Type serialization ──────────────────────────────────────────────────────

#[test]
fn browse_result_roundtrip() {
    let result = BrowseResult {
        achieved: true,
        summary: "Found cheapest flight: $847 United".into(),
        reason: BrowseReason::Done,
        steps_used: 12,
        final_url: Some("https://flights.example.com/results".into()),
    };
    let json = serde_json::to_string(&result).unwrap();
    let parsed: BrowseResult = serde_json::from_str(&json).unwrap();
    assert!(parsed.achieved);
    assert_eq!(parsed.reason, BrowseReason::Done);
    assert_eq!(parsed.steps_used, 12);
    assert!(parsed.summary.contains("$847"));
}

#[test]
fn browse_policy_serializes_lowercase() {
    assert_eq!(serde_json::to_string(&BrowsePolicy::Pattern).unwrap(), r#""pattern""#);
    assert_eq!(serde_json::to_string(&BrowsePolicy::Yolo).unwrap(), r#""yolo""#);
    assert_eq!(serde_json::to_string(&BrowsePolicy::Ask).unwrap(), r#""ask""#);
}

#[test]
fn browse_reason_serializes_snake_case() {
    assert_eq!(serde_json::to_string(&BrowseReason::StepCap).unwrap(), r#""step_cap""#);
    assert_eq!(serde_json::to_string(&BrowseReason::BrowserCrashed).unwrap(), r#""browser_crashed""#);
    assert_eq!(serde_json::to_string(&BrowseReason::UserDenied).unwrap(), r#""user_denied""#);
}

// ── Voice routing ───────────────────────────────────────────────────────────

#[test]
fn voice_routes_browse_not_find() {
    use rustyclaw::voice::{voice_routes_to_browse, strip_browse_prefix};

    assert!(voice_routes_to_browse("browse find flights to Tokyo"));
    assert!(voice_routes_to_browse("book a hotel in Paris"));
    assert!(voice_routes_to_browse("go to flights.google.com"));
    assert!(!voice_routes_to_browse("find the bug in my code"));
    assert!(!voice_routes_to_browse("search for todo items"));
    assert!(!voice_routes_to_browse("what time is it"));

    assert_eq!(strip_browse_prefix("browse Find flights"), "Find flights");
    assert_eq!(strip_browse_prefix("Book a Hotel"), "a Hotel");
}
