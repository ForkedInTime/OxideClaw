/// Events sent from the background API task to the render loop.
use crate::api::types::Message;
use crate::permissions::PermissionDecision;
use tokio::sync::oneshot;

pub enum AppEvent {
    /// A streamed text chunk from Claude
    TextChunk(String),
    /// A thinking/reasoning block from extended thinking
    ThinkingBlock(String),
    /// A tool call is about to execute
    ToolCall { name: String, args: String },
    /// Live output line from a running tool (e.g. bash command progress)
    ToolOutputStream(String),
    /// A tool call result
    ToolResult { is_error: bool, text: String },
    /// The full turn is complete — carries the updated full message history
    Done {
        tokens_in: u64,
        tokens_out: u64,
        cache_read: u64,
        cache_write: u64,
        messages: Vec<Message>,
        model_used: String,
    },
    /// An API-level error
    Error(String),
    /// Claude wants to run a sensitive tool — needs user permission
    PermissionRequest {
        tool_name: String,
        description: String,
        reply: oneshot::Sender<PermissionDecision>,
    },
    /// Summarise-compact completed — replacement history + display length
    Compacted {
        replacement: Vec<Message>,
        summary_len: usize,
    },
    /// Informational notice from the harness (not from Claude)
    SystemMessage(String),
    /// Claude called AskUserQuestion — show a text-input dialog
    AskUser {
        question: String,
        reply: oneshot::Sender<String>,
    },
    /// A tool (EnterPlanMode/ExitPlanMode) toggled plan mode
    SetPlanMode(bool),
    // ToggleBriefMode was here — removed: brief mode is toggled directly in
    // run.rs via CommandAction::ToggleBriefMode without needing a round-trip
    // through the AppEvent channel.
    /// Voice transcription completed — insert text into input buffer
    VoiceTranscription(String),
    /// Voice transcription matched a browse prefix — dispatch as /browse
    VoiceBrowse(String),
    /// Plugin install completed (success or failure)
    PluginInstallDone { success: bool, message: String },
    /// GitHub upgrade check completed
    UpgradeCheckDone { message: String },
}
