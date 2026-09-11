use serde_json::json;

/// Test that CdpMessage serializes to the correct JSON-RPC format
#[test]
fn cdp_message_serialization() {
    use oxideclaw::browser::cdp::CdpCommand;
    let cmd = CdpCommand {
        id: 1,
        method: "Page.navigate".to_string(),
        params: json!({"url": "https://example.com"}),
    };
    let serialized = serde_json::to_string(&cmd).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&serialized).unwrap();
    assert_eq!(parsed["id"], 1);
    assert_eq!(parsed["method"], "Page.navigate");
    assert_eq!(parsed["params"]["url"], "https://example.com");
}

/// Test that CDP events are correctly deserialized
#[test]
fn cdp_event_deserialization() {
    use oxideclaw::browser::cdp::CdpEvent;
    let raw = r#"{"method":"Page.loadEventFired","params":{"timestamp":12345.0}}"#;
    let event: CdpEvent = serde_json::from_str(raw).unwrap();
    assert_eq!(event.method, "Page.loadEventFired");
    assert!(event.params["timestamp"].as_f64().unwrap() > 0.0);
}

/// Test that CDP response with result is parsed correctly
#[test]
fn cdp_response_with_result() {
    use oxideclaw::browser::cdp::CdpResponse;
    let raw = r#"{"id":1,"result":{"frameId":"ABC","loaderId":"XYZ"}}"#;
    let resp: CdpResponse = serde_json::from_str(raw).unwrap();
    assert_eq!(resp.id, 1);
    assert!(resp.error.is_none());
    assert_eq!(resp.result.as_ref().unwrap()["frameId"], "ABC");
}

/// Test that CDP error response is parsed correctly
#[test]
fn cdp_response_with_error() {
    use oxideclaw::browser::cdp::CdpResponse;
    let raw = r#"{"id":2,"error":{"code":-32000,"message":"Page not found"}}"#;
    let resp: CdpResponse = serde_json::from_str(raw).unwrap();
    assert_eq!(resp.id, 2);
    assert!(resp.result.is_none());
    let err = resp.error.as_ref().unwrap();
    assert_eq!(err["code"], -32000);
}

#[test]
fn chrome_path_discovery_returns_known_binaries() {
    use oxideclaw::browser::find_chrome;
    let result = find_chrome();
    let _ = result; // Just verify it doesn't panic
}

#[test]
fn browser_session_default_state() {
    use oxideclaw::browser::BrowserSession;
    let session = BrowserSession::default();
    assert!(!session.is_connected());
    assert!(session.ref_map().is_empty());
}
