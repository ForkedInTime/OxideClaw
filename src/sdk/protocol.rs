//! SDK protocol types — all requests, responses, and notifications.
//!
//! Every message has a "type" field for tagged dispatch.
//! Requests have an "id" field for correlation.
//!
//! Many variants and fields are defined for Phase B/C but not yet wired.
//! Suppress dead_code warnings — these types must exist for the NDJSON wire protocol.

use serde::{Deserialize, Serialize};

// ── Sub-types ────────────────────────────────────────────────────────────────

/// Tool approval policy — set at session start.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Policy {
    /// Tools that execute silently (no notification).
    #[serde(default)]
    pub allow: Vec<String>,
    /// Tools that execute with a tool/started notification.
    #[serde(default)]
    pub auto_approve: Vec<String>,
    /// Tools that are always denied.
    #[serde(default)]
    pub deny: Vec<String>,
    /// Tools that require host approval (tool/approval_needed).
    #[serde(default)]
    pub ask: Vec<String>,
    /// Seconds to wait for approval before auto-denying (default 60).
    #[serde(default = "default_approval_timeout")]
    pub approval_timeout_seconds: u64,
}

fn default_approval_timeout() -> u64 {
    60
}

/// Host capabilities — tells the agent what the environment supports.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Capabilities {
    #[serde(default = "default_true")]
    pub show_diff: bool,
    #[serde(default = "default_true")]
    pub open_browser: bool,
    #[serde(default)]
    pub play_audio: bool,
    #[serde(default = "default_true")]
    pub interactive_approval: bool,
    #[serde(default)]
    pub max_file_size_bytes: Option<u64>,
    #[serde(default)]
    pub supports_images: bool,
}

fn default_true() -> bool {
    true
}

impl Default for Capabilities {
    fn default() -> Self {
        Self {
            show_diff: true,
            open_browser: true,
            play_audio: false,
            interactive_approval: true,
            max_file_size_bytes: None,
            supports_images: false,
        }
    }
}

/// Token usage summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input: u64,
    pub output: u64,
}

/// Session metadata for list responses.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: String,
    pub name: String,
    pub created_at: String,
    pub preview: String,
}

/// RAG search result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RagResult {
    pub file: String,
    pub line: u32,
    pub symbol: String,
    pub kind: String,
    pub snippet: String,
}

/// Per-model cost breakdown.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelCostEntry {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_usd: f64,
}

/// Per-tool usage count.
pub type ToolUsageMap = std::collections::HashMap<String, u32>;

/// Router decision info.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouterDecision {
    pub complexity: String,
    pub reason: String,
}

// ── Requests (Host → RustyClaw) ─────────────────────────────────────────────

/// All possible requests from the host.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)] // Protocol variants exist for wire compatibility; not all wired in Phase A
#[serde(tag = "type")]
pub enum SdkRequest {
    #[serde(rename = "session/start")]
    SessionStart {
        id: String,
        prompt: String,
        #[serde(default)]
        cwd: Option<String>,
        #[serde(default)]
        model: Option<String>,
        #[serde(default)]
        output_format: Option<String>,
        #[serde(default)]
        output_schema: Option<serde_json::Value>,
        #[serde(default)]
        max_turns: Option<u32>,
        #[serde(default)]
        max_budget_usd: Option<f64>,
        #[serde(default)]
        record: Option<bool>,
        #[serde(default)]
        policy: Option<Policy>,
        #[serde(default)]
        capabilities: Option<Capabilities>,
    },

    #[serde(rename = "session/resume")]
    SessionResume {
        id: String,
        session_id: String,
        #[serde(default)]
        capabilities: Option<Capabilities>,
    },

    #[serde(rename = "session/list")]
    SessionList {
        id: String,
        #[serde(default)]
        limit: Option<usize>,
    },

    #[serde(rename = "session/export")]
    SessionExport { id: String, session_id: String },

    #[serde(rename = "turn/start")]
    TurnStart {
        id: String,
        session_id: String,
        prompt: String,
    },

    #[serde(rename = "turn/interrupt")]
    TurnInterrupt { id: String, session_id: String },

    #[serde(rename = "tool/approve")]
    ToolApprove { id: String, approval_id: String },

    #[serde(rename = "tool/deny")]
    ToolDeny {
        id: String,
        approval_id: String,
        #[serde(default)]
        reason: Option<String>,
    },

    #[serde(rename = "rag/search")]
    RagSearch {
        id: String,
        query: String,
        #[serde(default)]
        limit: Option<usize>,
    },

    #[serde(rename = "cost/report")]
    CostReport {
        id: String,
        #[serde(default)]
        session_id: Option<String>,
    },

    #[serde(rename = "health/check")]
    HealthCheck { id: String },

    #[serde(rename = "browse/start")]
    BrowseStart {
        id: String,
        goal: String,
        #[serde(default)]
        policy: crate::browser::browse_loop::BrowsePolicy,
        #[serde(default)]
        max_steps: Option<u32>,
        #[serde(default)]
        yolo_ack: bool,
    },

    #[serde(rename = "browse/approval_reply")]
    BrowseApprovalReply {
        session_id: String,
        step: u32,
        approved: bool,
    },
}

// ── Responses (RustyClaw → Host, correlated by ID) ──────────────────────────

/// Responses to specific requests.
#[allow(dead_code)]
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type")]
pub enum SdkResponse {
    #[serde(rename = "session/started")]
    SessionStarted {
        id: String,
        session_id: String,
        model: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        router_decision: Option<RouterDecision>,
    },

    #[serde(rename = "session/list")]
    SessionList {
        id: String,
        sessions: Vec<SessionInfo>,
    },

    #[serde(rename = "session/export")]
    SessionExport { id: String, log: serde_json::Value },

    #[serde(rename = "rag/search")]
    RagSearchResult { id: String, results: Vec<RagResult> },

    #[serde(rename = "cost/report")]
    CostReport {
        id: String,
        total_usd: f64,
        by_model: std::collections::HashMap<String, ModelCostEntry>,
        by_tool: ToolUsageMap,
        #[serde(skip_serializing_if = "Option::is_none")]
        budget_remaining_usd: Option<f64>,
    },

    #[serde(rename = "health/check")]
    HealthCheck {
        id: String,
        status: String,
        version: String,
        active_sessions: usize,
        uptime_seconds: u64,
    },

    #[serde(rename = "browse/started")]
    BrowseStarted {
        id: String,
        session_id: String,
    },

    #[serde(rename = "error")]
    Error {
        id: String,
        code: String,
        message: String,
    },
}

// ── Notifications (RustyClaw → Host, streamed, no request ID) ───────────────

/// Streamed events during turn execution.
#[allow(dead_code)]
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type")]
pub enum SdkNotification {
    #[serde(rename = "message/delta")]
    MessageDelta { session_id: String, content: String },

    #[serde(rename = "thinking/delta")]
    ThinkingDelta { session_id: String, content: String },

    #[serde(rename = "tool/started")]
    ToolStarted {
        session_id: String,
        tool: String,
        args: serde_json::Value,
        tool_use_id: String,
    },

    #[serde(rename = "tool/approval_needed")]
    ToolApprovalNeeded {
        session_id: String,
        approval_id: String,
        tool: String,
        args: serde_json::Value,
        tool_use_id: String,
    },

    #[serde(rename = "tool/completed")]
    ToolCompleted {
        session_id: String,
        tool: String,
        tool_use_id: String,
        success: bool,
        output_summary: String,
        duration_ms: u64,
    },

    #[serde(rename = "cost/updated")]
    CostUpdated {
        session_id: String,
        turn_cost_usd: f64,
        session_total_usd: f64,
        #[serde(skip_serializing_if = "Option::is_none")]
        budget_remaining_usd: Option<f64>,
        input_tokens: u64,
        output_tokens: u64,
        model: String,
    },

    #[serde(rename = "model/routed")]
    ModelRouted {
        session_id: String,
        model: String,
        complexity: String,
        reason: String,
    },

    #[serde(rename = "progress/updated")]
    ProgressUpdated {
        session_id: String,
        percent: u8,
        stage: String,
        tools_executed: u32,
        tools_remaining_estimate: u32,
    },

    #[serde(rename = "context/health")]
    ContextHealth {
        session_id: String,
        used_pct: u8,
        tokens_used: u64,
        tokens_max: u64,
        compaction_imminent: bool,
    },

    #[serde(rename = "turn/completed")]
    TurnCompleted {
        session_id: String,
        response: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        structured_output: Option<serde_json::Value>,
        cost_usd: f64,
        total_session_cost_usd: f64,
        tokens: TokenUsage,
        model: String,
        tools_used: Vec<String>,
        duration_ms: u64,
    },

    #[serde(rename = "error")]
    Error {
        session_id: String,
        code: String,
        message: String,
    },

    #[serde(rename = "browse/progress")]
    BrowseProgress {
        session_id: String,
        step: u32,
        action: String,
        target: String,
    },

    #[serde(rename = "browse/approval_needed")]
    BrowseApprovalNeeded {
        session_id: String,
        step: u32,
        tool_name: String,
        target_text: String,
        url: String,
        reason: String,
    },

    #[serde(rename = "browse/completed")]
    BrowseCompleted {
        session_id: String,
        result: crate::browser::browse_loop::BrowseResult,
    },
}
