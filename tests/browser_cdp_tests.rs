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

// ── Fake CDP endpoint ──────────────────────────────────────────────────────
//
// Speaks just enough of Chrome's DevTools WebSocket protocol for the client:
// answers every command with `{}` (or a scripted result) and records what the
// client sent, so the tests run without a real Chrome.

mod fake_cdp {
    use futures_util::{SinkExt, StreamExt};
    use serde_json::{Value, json};
    use std::collections::HashMap;
    use tokio::net::TcpListener;
    use tokio::sync::mpsc;
    use tokio_tungstenite::tungstenite::Message;

    #[derive(Default)]
    pub struct Script {
        /// Result object per method; anything else gets `{}`.
        pub results: HashMap<&'static str, Value>,
        /// Close the socket right after answering `Runtime.enable`.
        pub close_after_enable: bool,
        /// Raise this `Page.javascriptDialogOpening` event on
        /// `Input.dispatchMouseEvent` and, like Chrome, hold that command's
        /// reply until the client answers the dialog.
        pub dialog_on_click: Option<Value>,
    }

    /// Serve one scripted connection per entry, in order. Returns the ws URL
    /// and a feed of every `(method, params)` the client sent.
    pub async fn serve(scripts: Vec<Script>) -> (String, mpsc::UnboundedReceiver<(String, Value)>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}/devtools/page/FAKE", listener.local_addr().unwrap());
        let (seen_tx, seen_rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            for script in scripts {
                let (tcp, _) = listener.accept().await.unwrap();
                let seen_tx = seen_tx.clone();
                tokio::spawn(async move {
                    let ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
                    run(ws, script, seen_tx).await;
                });
            }
        });
        (url, seen_rx)
    }

    async fn run(
        mut ws: tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
        script: Script,
        seen: mpsc::UnboundedSender<(String, Value)>,
    ) {
        let mut held_click: Option<u64> = None;
        while let Some(Ok(msg)) = ws.next().await {
            let Message::Text(text) = msg else { continue };
            let cmd: Value = serde_json::from_str(&text).unwrap();
            let id = cmd["id"].as_u64().unwrap();
            let method = cmd["method"].as_str().unwrap().to_string();
            let _ = seen.send((method.clone(), cmd["params"].clone()));

            if method == "Input.dispatchMouseEvent"
                && let Some(ev) = &script.dialog_on_click
            {
                send(&mut ws, ev.clone()).await;
                held_click = Some(id);
                continue;
            }

            let result = script.results.get(method.as_str()).cloned();
            send(
                &mut ws,
                json!({ "id": id, "result": result.unwrap_or(json!({})) }),
            )
            .await;

            match method.as_str() {
                "Page.navigate" => {
                    send(
                        &mut ws,
                        json!({ "method": "Page.loadEventFired", "params": {} }),
                    )
                    .await;
                }
                "Page.handleJavaScriptDialog" => {
                    if let Some(click_id) = held_click.take() {
                        send(&mut ws, json!({ "id": click_id, "result": {} })).await;
                    }
                }
                "Runtime.enable" if script.close_after_enable => {
                    let _ = ws.close(None).await;
                    return;
                }
                _ => {}
            }
        }
    }

    async fn send(ws: &mut tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>, v: Value) {
        ws.send(Message::Text(v.to_string().into())).await.unwrap();
    }
}

/// Chrome ships each CDP reply as one unfragmented frame. A long page's
/// accessibility tree is bigger than tungstenite's 16 MiB default frame cap,
/// which used to kill the socket and leave the browser session unusable.
#[tokio::test]
async fn cdp_reply_larger_than_16_mib_is_received() {
    use oxideclaw::browser::cdp::CdpClient;

    let pad = "x".repeat(20 << 20);
    let (url, _seen) = fake_cdp::serve(vec![fake_cdp::Script {
        results: [(
            "Accessibility.getFullAXTree",
            json!({ "nodes": [], "pad": pad }),
        )]
        .into(),
        ..Default::default()
    }])
    .await;

    let client = CdpClient::connect(&url).await.unwrap();
    let reply = client
        .send("Accessibility.getFullAXTree", json!({}))
        .await
        .expect("oversized reply must not drop the connection");
    assert_eq!(reply["pad"].as_str().map(str::len), Some(20 << 20));
    assert!(client.is_alive());
}

/// A confirm() raised by a click blocks the renderer until the CDP client
/// answers it. The session must dismiss it on its own (never confirm it) and
/// report it so the model knows the click did not go through.
#[tokio::test]
async fn javascript_dialog_is_dismissed_and_reported() {
    use oxideclaw::browser::BrowserSession;

    let (url, mut seen) = fake_cdp::serve(vec![fake_cdp::Script {
        dialog_on_click: Some(json!({
            "method": "Page.javascriptDialogOpening",
            "params": { "type": "confirm", "message": "Delete repo?", "url": "about:blank" }
        })),
        ..Default::default()
    }])
    .await;

    let mut session = BrowserSession::default();
    session.connect(&url).await.unwrap();
    let client = session.client().unwrap().clone();

    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        client.send(
            "Input.dispatchMouseEvent",
            json!({ "type": "mouseReleased" }),
        ),
    )
    .await
    .expect("click stayed blocked on an unanswered dialog")
    .unwrap();

    let mut answer = None;
    while let Ok((method, params)) = seen.try_recv() {
        if method == "Page.handleJavaScriptDialog" {
            answer = Some(params);
        }
    }
    assert_eq!(answer, Some(json!({ "accept": false })));
    assert_eq!(
        session.take_dialog_messages().await,
        vec!["[dialog:confirm] Delete repo? (auto-dismissed)".to_string()]
    );
    assert!(session.take_console_messages().await.is_empty());
}

/// Once the CDP socket dies the stale client must not stick around: the next
/// browser_navigate reconnects instead of failing with "connection is closed"
/// until the user runs /browser close.
#[tokio::test]
async fn navigate_reconnects_after_the_cdp_socket_dies() {
    use oxideclaw::browser::BrowserSession;
    use oxideclaw::tools::browser_tools::BrowserNavigateTool;
    use oxideclaw::tools::{Tool, ToolContext};
    use std::sync::Arc;

    let (url, _seen) = fake_cdp::serve(vec![
        fake_cdp::Script {
            close_after_enable: true,
            ..Default::default()
        },
        fake_cdp::Script {
            results: [
                ("Runtime.evaluate", json!({ "result": { "value": "Fake" } })),
                ("Accessibility.getFullAXTree", json!({ "nodes": [] })),
            ]
            .into(),
            ..Default::default()
        },
    ])
    .await;

    let session = Arc::new(tokio::sync::Mutex::new(BrowserSession::default()));
    session.lock().await.connect(&url).await.unwrap();
    let first = session.lock().await.client().unwrap().clone();
    for _ in 0..100 {
        if !first.is_alive() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(
        !first.is_alive(),
        "fake server should have closed the socket"
    );

    let tool = BrowserNavigateTool {
        session: session.clone(),
        headless: true,
        chrome_path: None,
        cdp_endpoint: Some(url),
        timeout_ms: 5_000,
        net_policy: oxideclaw::net_policy::NetPolicy::STRICT,
    };
    let tmp = tempfile::tempdir().unwrap();
    let out = tool
        .execute(
            json!({ "url": "about:blank" }),
            &ToolContext::new(tmp.path().to_path_buf()),
        )
        .await
        .expect("navigate should reconnect over a dead CDP socket");
    let oxideclaw::api::types::ToolResultContent::Text { text } = &out.content[0];
    assert!(!out.is_error && text.contains("Title: Fake"), "{text}");
    assert!(session.lock().await.client().unwrap().is_alive());
}

/// Answers every permission prompt with `answer` and counts the prompts.
struct CountingAsker {
    answer: oxideclaw::permissions::PermissionDecision,
    asked: std::sync::Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl oxideclaw::permissions::PermissionAsker for CountingAsker {
    async fn ask(
        &self,
        _tool_name: &str,
        description: &str,
        _input: &serde_json::Value,
    ) -> Option<oxideclaw::permissions::PermissionDecision> {
        self.asked.lock().unwrap().push(description.to_string());
        Some(self.answer.clone())
    }
}

/// A navigate tool on a fake page at a loopback dev server (the fake's
/// `href`), plus the feed of CDP commands it sent.
async fn loopback_navigator(
    href: &'static str,
) -> (
    oxideclaw::tools::browser_tools::BrowserNavigateTool,
    tokio::sync::mpsc::UnboundedReceiver<(String, serde_json::Value)>,
) {
    use oxideclaw::browser::BrowserSession;
    use oxideclaw::tools::browser_tools::BrowserNavigateTool;
    use std::sync::Arc;
    let page = fake_cdp::Script {
        results: [
            ("Runtime.evaluate", json!({ "result": { "value": href } })),
            (
                "Accessibility.getFullAXTree",
                json!({ "nodes": [
                    {"nodeId": "1", "role": {"value": "RootWebArea"}, "name": {"value": "Dev"}},
                    {"nodeId": "2", "parentId": "1", "role": {"value": "StaticText"},
                     "name": {"value": "Total: $12.50"}},
                ]}),
            ),
        ]
        .into(),
        ..Default::default()
    };
    let (url, seen) = fake_cdp::serve(vec![page]).await;
    let tool = BrowserNavigateTool {
        session: Arc::new(tokio::sync::Mutex::new(BrowserSession::default())),
        headless: true,
        chrome_path: None,
        cdp_endpoint: Some(url),
        timeout_ms: 5_000,
        net_policy: oxideclaw::net_policy::NetPolicy::STRICT,
    };
    (tool, seen)
}

fn sent_navigate(
    seen: &mut tokio::sync::mpsc::UnboundedReceiver<(String, serde_json::Value)>,
) -> bool {
    let mut any = false;
    while let Ok((method, _)) = seen.try_recv() {
        any |= method == "Page.navigate";
    }
    any
}

/// The CDP browser reached loopback with no question asked, whatever
/// allowPrivateNetworkFetch said. Without the setting, browser_navigate to
/// 127.0.0.1 is refused where nobody can be asked, asks once in an
/// interactive session, and remembers the answer for that host:port.
#[tokio::test]
async fn navigate_to_loopback_needs_consent_once() {
    use oxideclaw::permissions::{PermissionDecision, PermissionGate};
    use oxideclaw::tools::{Tool, ToolContext};
    use std::sync::Arc;
    let tmp = tempfile::tempdir().unwrap();
    let target = json!({ "url": "http://127.0.0.1:3000/" });

    // Headless: refused before Chrome is touched.
    let (tool, mut seen) = loopback_navigator("http://127.0.0.1:3000/").await;
    let err = tool
        .execute(target.clone(), &ToolContext::new(tmp.path().to_path_buf()))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("allowPrivateNetworkFetch"), "{err}");
    assert!(!sent_navigate(&mut seen));
    assert!(!tool.session.lock().await.is_connected());

    // Interactive, and the user says no.
    let no = Arc::new(CountingAsker {
        answer: PermissionDecision::Deny,
        asked: Default::default(),
    });
    let mut ctx = ToolContext::new(tmp.path().to_path_buf());
    ctx.permission_gate =
        Some(PermissionGate::bypass_with_deny(&[], tmp.path()).with_asker(no.clone()));
    let err = tool
        .execute(target.clone(), &ctx)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("did not allow"), "{err}");
    assert_eq!(no.asked.lock().unwrap().len(), 1);
    assert!(no.asked.lock().unwrap()[0].contains("127.0.0.1:3000"));
    assert!(!sent_navigate(&mut seen));

    // Interactive, and the user says yes: navigated, and asked only once.
    let yes = Arc::new(CountingAsker {
        answer: PermissionDecision::Allow,
        asked: Default::default(),
    });
    ctx.permission_gate =
        Some(PermissionGate::bypass_with_deny(&[], tmp.path()).with_asker(yes.clone()));
    for _ in 0..2 {
        let out = tool.execute(target.clone(), &ctx).await.unwrap();
        let oxideclaw::api::types::ToolResultContent::Text { text } = &out.content[0];
        assert!(
            text.starts_with("Navigated to: http://127.0.0.1:3000/"),
            "{text}"
        );
        // The page's text reaches the model, fenced as page data.
        let fence = text.find("<page-content id=").unwrap();
        assert!(text[fence..].contains("[text] \"Total: $12.50\""), "{text}");
        assert!(text[..fence].contains("not instructions"), "{text}");
    }
    assert_eq!(
        yes.asked.lock().unwrap().len(),
        1,
        "one prompt per host:port"
    );
    assert!(sent_navigate(&mut seen));
    // The gate's copy of the page is the DOM text.
    assert!(tool.session.lock().await.last_page_text.contains("$12.50"));
}

/// The metadata service is refused before any question, whatever the user
/// or the settings would say.
#[tokio::test]
async fn navigate_never_asks_about_the_metadata_service() {
    use oxideclaw::permissions::{PermissionDecision, PermissionGate};
    use oxideclaw::tools::{Tool, ToolContext};
    use std::sync::Arc;
    let tmp = tempfile::tempdir().unwrap();
    let (tool, mut seen) = loopback_navigator("about:blank").await;
    let yes = Arc::new(CountingAsker {
        answer: PermissionDecision::Allow,
        asked: Default::default(),
    });
    let mut ctx = ToolContext::new(tmp.path().to_path_buf());
    ctx.permission_gate =
        Some(PermissionGate::bypass_with_deny(&[], tmp.path()).with_asker(yes.clone()));
    let err = tool
        .execute(
            json!({ "url": "http://169.254.169.254/latest/meta-data/" }),
            &ctx,
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("169.254.169.254"), "{err}");
    assert!(yes.asked.lock().unwrap().is_empty());
    assert!(!sent_navigate(&mut seen));
}
