/// Anthropic API message types — mirrors @anthropic-ai/sdk types
use serde::{Deserialize, Serialize};

// ── Prompt caching ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheControl {
    #[serde(rename = "type")]
    pub cache_type: String,
}

impl CacheControl {
    pub fn ephemeral() -> Self {
        Self {
            cache_type: "ephemeral".into(),
        }
    }
}

/// A single text block in the system prompt (used for prompt caching).
#[derive(Debug, Clone, Serialize)]
pub struct SystemBlock {
    #[serde(rename = "type")]
    pub block_type: String,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControl>,
}

/// System prompt: either a plain string or an array of blocks (for prompt caching).
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum SystemContent {
    Plain(String),
    Blocks(Vec<SystemBlock>),
}

impl From<String> for SystemContent {
    fn from(s: String) -> Self {
        SystemContent::Plain(s)
    }
}

impl Default for SystemContent {
    fn default() -> Self {
        SystemContent::Plain(String::new())
    }
}

// ── Extended thinking / effort ───────────────────────────────────────────

pub use super::thinking::{OutputConfig, ThinkingConfig};

// ── Image source ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ImageSource {
    Base64 { media_type: String, data: String },
    Url { url: String },
}

/// A message in the conversation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<ContentBlock>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
}

/// A content block inside a message
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        tool_use_id: String,
        content: Vec<ToolResultContent>,
        #[serde(skip_serializing_if = "Option::is_none")]
        is_error: Option<bool>,
    },
    Thinking {
        thinking: String,
        signature: String,
    },
    Image {
        source: ImageSource,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolResultContent {
    Text { text: String },
}

impl ToolResultContent {
    pub fn text(s: impl Into<String>) -> Self {
        ToolResultContent::Text { text: s.into() }
    }
}

/// Request body for /v1/messages
#[derive(Debug, Clone, Default, Serialize)]
pub struct MessagesRequest {
    pub model: String,
    pub max_tokens: u32,
    pub messages: Vec<Message>,
    pub system: SystemContent,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolDefinition>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<ThinkingConfig>,
    /// `output_config.effort` — only on models that support it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_config: Option<OutputConfig>,
    /// Beta headers to include in the request (not serialised to body).
    #[serde(skip)]
    pub betas: Vec<String>,
    /// Session UUID sent via header to the Anthropic API (not serialised to body).
    /// Enables proxies and backends to aggregate requests by session.
    #[serde(skip)]
    pub session_id: Option<String>,
}

/// Tool definition sent to the API
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControl>,
}

/// Top-level API response (non-streaming)
#[derive(Debug, Deserialize)]
#[allow(dead_code)] // used by non-streaming API path (SDK/headless)
pub struct MessagesResponse {
    pub id: String,
    pub content: Vec<ContentBlock>,
    pub stop_reason: Option<StopReason>,
    pub usage: Usage,
}

#[derive(Debug, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    StopSequence,
}

#[derive(Debug, Deserialize, Default)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_creation_input_tokens: u64,
    #[serde(default)]
    pub cache_read_input_tokens: u64,
}

/// SSE streaming event types
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamEvent {
    MessageStart {
        message: StreamMessageStart,
    },
    ContentBlockStart {
        index: usize,
        content_block: StreamContentBlock,
    },
    ContentBlockDelta {
        index: usize,
        delta: ContentDelta,
    },
    ContentBlockStop {
        index: usize,
    },
    MessageDelta {
        delta: MessageDelta,
        usage: Option<Usage>,
    },
    MessageStop,
    Ping,
    Error {
        error: ApiError,
    },
}

#[derive(Debug, Deserialize)]
pub struct StreamMessageStart {
    pub usage: Usage,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamContentBlock {
    Text { text: String },
    ToolUse { id: String, name: String },
    Thinking { thinking: String },
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentDelta {
    #[serde(rename = "text_delta")]
    Text { text: String },
    #[serde(rename = "input_json_delta")]
    InputJson { partial_json: String },
    #[serde(rename = "thinking_delta")]
    Thinking { thinking: String },
    #[serde(rename = "signature_delta")]
    Signature { signature: String },
}

#[derive(Debug, Deserialize)]
pub struct MessageDelta {
    pub stop_reason: Option<StopReason>,
}

#[derive(Debug, Deserialize)]
pub struct ApiError {
    pub r#type: String,
    pub message: String,
}

/// Accumulated result from a streaming response
#[derive(Debug, Default)]
pub struct StreamedResponse {
    pub content: Vec<ContentBlock>,
    pub stop_reason: Option<StopReason>,
    pub usage: Usage,
}
