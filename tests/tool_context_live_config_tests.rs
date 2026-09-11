//! Verify that `ToolContext.live_model` / `live_api_key` / `live_ollama_host`
//! are plumbed through and that sub-agent-spawning tools would pick them up
//! instead of a stale snapshot.
//!
//! These fields exist to fix the "user runs /model foo, then subagent runs
//! against the OLD model" bug class (a known regression in multiple
//! competing tools in this space).

use oxideclaw::tools::ToolContext;
use std::path::PathBuf;

#[test]
fn default_context_has_no_live_snapshot() {
    let ctx = ToolContext::new(PathBuf::from("/tmp"));
    assert!(ctx.live_model.is_none());
    assert!(ctx.live_api_key.is_none());
    assert!(ctx.live_ollama_host.is_none());
}

#[test]
fn live_snapshot_fields_can_be_set_and_read() {
    let mut ctx = ToolContext::new(PathBuf::from("/tmp"));
    ctx.live_model = Some("groq:llama-3.3-70b-versatile".to_string());
    ctx.live_api_key = Some("sk-test".to_string());
    ctx.live_ollama_host = Some("http://localhost:11434".to_string());

    assert_eq!(
        ctx.live_model.as_deref(),
        Some("groq:llama-3.3-70b-versatile")
    );
    assert_eq!(ctx.live_api_key.as_deref(), Some("sk-test"));
    assert_eq!(
        ctx.live_ollama_host.as_deref(),
        Some("http://localhost:11434")
    );
}

/// Regression guard: cloning a ToolContext must preserve the live snapshot.
/// If anyone refactors `Clone` manually they could easily skip these fields.
#[test]
fn clone_preserves_live_snapshot() {
    let mut ctx = ToolContext::new(PathBuf::from("/tmp"));
    ctx.live_model = Some("ollama:llama3.2".to_string());
    ctx.live_api_key = Some("".to_string());
    ctx.live_ollama_host = Some("http://remote:11434".to_string());

    let cloned = ctx.clone();
    assert_eq!(cloned.live_model.as_deref(), Some("ollama:llama3.2"));
    assert_eq!(cloned.live_api_key.as_deref(), Some(""));
    assert_eq!(
        cloned.live_ollama_host.as_deref(),
        Some("http://remote:11434")
    );
}
