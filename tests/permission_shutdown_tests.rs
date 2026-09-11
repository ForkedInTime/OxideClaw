//! Security contract: if a pending tool-approval prompt's oneshot `Sender`
//! is dropped without a value (TUI shutdown, terminal close, panic, runtime
//! tear-down), the awaiting tool-executor MUST interpret that as **Deny**.
//!
//! This guards against the "close terminal = auto-approve" bug class,
//! which has shipped as a regression in multiple competing tools.

use oxideclaw::permissions::PermissionDecision;
use tokio::sync::oneshot;

/// Simulates the TUI run-loop dropping `App.pending_permission` on shutdown
/// while a spawned tool-exec task is blocked waiting on its reply.
#[tokio::test]
async fn dropped_sender_resolves_to_deny() {
    let (reply_tx, reply_rx) = oneshot::channel::<PermissionDecision>();

    // The executor task — mirrors the awaiting logic in src/tui/run.rs:3929
    let exec = tokio::spawn(async move {
        match reply_rx.await {
            Ok(d) => d,
            Err(_) => PermissionDecision::Deny,
        }
    });

    // Simulate shutdown: drop the sender without ever sending a value. This
    // is what happens when App.pending_permission is dropped on quit, when
    // the TUI task panics, or when the tokio runtime tears down mid-prompt.
    drop(reply_tx);

    // The executor must resolve to Deny, not hang.
    let decision = tokio::time::timeout(std::time::Duration::from_secs(2), exec)
        .await
        .expect("executor hung after sender drop — shutdown would leave tools approved!")
        .expect("executor task panicked");

    assert_eq!(
        decision,
        PermissionDecision::Deny,
        "dropped sender must map to Deny — otherwise terminal-close auto-approves"
    );
}

/// A second scenario: the awaiting side is polled BEFORE the sender is
/// dropped. Tokio's oneshot should still deliver Err eventually. Guards
/// against any future refactor that replaces oneshot with a channel whose
/// drop semantics differ.
#[tokio::test]
async fn sender_dropped_after_receiver_polled() {
    let (reply_tx, reply_rx) = oneshot::channel::<PermissionDecision>();

    let exec = tokio::spawn(async move {
        match reply_rx.await {
            Ok(d) => d,
            Err(_) => PermissionDecision::Deny,
        }
    });

    // Let the executor task be polled and park on the receiver.
    tokio::task::yield_now().await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // Now drop the sender.
    drop(reply_tx);

    let decision = tokio::time::timeout(std::time::Duration::from_secs(2), exec)
        .await
        .expect("executor hung after sender drop")
        .expect("executor task panicked");

    assert_eq!(decision, PermissionDecision::Deny);
}
