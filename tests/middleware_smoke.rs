//! Smoke tests for the ToolMiddleware trait.

use async_trait::async_trait;
use rustyclaw::browser::middleware::{MiddlewareVerdict, ToolMiddleware};
use serde_json::Value;
use std::sync::{Arc, Mutex};

// ── RecordingMiddleware ──────────────────────────────────────────────

/// Records all before/after calls for assertion.
struct RecordingMiddleware {
    log: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl ToolMiddleware for RecordingMiddleware {
    async fn before_tool(&self, tool_name: &str, _input: &Value) -> MiddlewareVerdict {
        self.log
            .lock()
            .unwrap()
            .push(format!("before:{tool_name}"));
        MiddlewareVerdict::Allow
    }

    async fn after_tool(&self, tool_name: &str, output: &str) {
        self.log
            .lock()
            .unwrap()
            .push(format!("after:{tool_name}:{output}"));
    }
}

#[tokio::test]
async fn recording_middleware_fires_before_and_after() {
    let log = Arc::new(Mutex::new(Vec::<String>::new()));
    let mw = RecordingMiddleware { log: log.clone() };

    let verdict = mw.before_tool("bash", &serde_json::json!({"cmd": "ls"})).await;
    assert!(matches!(verdict, MiddlewareVerdict::Allow));

    mw.after_tool("bash", "file1.rs\nfile2.rs").await;

    let entries = log.lock().unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0], "before:bash");
    assert_eq!(entries[1], "after:bash:file1.rs\nfile2.rs");
}

// ── Denier ───────────────────────────────────────────────────────────

/// Always denies tool execution.
struct Denier {
    reason: String,
}

#[async_trait]
impl ToolMiddleware for Denier {
    async fn before_tool(&self, _tool_name: &str, _input: &Value) -> MiddlewareVerdict {
        MiddlewareVerdict::Deny {
            reason: self.reason.clone(),
        }
    }

    async fn after_tool(&self, _tool_name: &str, _output: &str) {
        // Never called when denied.
    }
}

#[tokio::test]
async fn denier_middleware_returns_deny() {
    let mw = Denier {
        reason: "not allowed".into(),
    };

    let verdict = mw.before_tool("file_write", &serde_json::json!({})).await;
    match verdict {
        MiddlewareVerdict::Deny { reason } => {
            assert_eq!(reason, "not allowed");
        }
        other => panic!("Expected Deny, got {other:?}"),
    }
}

// ── Chain ordering ───────────────────────────────────────────────────

#[tokio::test]
async fn middleware_chain_short_circuits_on_deny() {
    let log = Arc::new(Mutex::new(Vec::<String>::new()));
    let recorder = Arc::new(RecordingMiddleware { log: log.clone() });
    let denier = Arc::new(Denier {
        reason: "blocked".into(),
    });

    // Denier first, recorder second — recorder should never fire.
    let chain: Vec<Arc<dyn ToolMiddleware>> = vec![denier, recorder];

    let input = serde_json::json!({});
    let mut denied = false;
    for mw in &chain {
        match mw.before_tool("bash", &input).await {
            MiddlewareVerdict::Allow => {}
            MiddlewareVerdict::Deny { .. } => {
                denied = true;
                break;
            }
        }
    }

    assert!(denied);
    assert!(log.lock().unwrap().is_empty(), "recorder should not have fired");
}
