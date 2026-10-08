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
    /// One API call finished. A turn makes one call per tool round-trip, and
    /// every call is billed, so cost is recorded here rather than on `Done`.
    Usage {
        model: String,
        input: u64,
        output: u64,
        cache_read: u64,
        cache_write: u64,
        /// The call carried the session's history, so its input is what the
        /// status bar's ctx % shows. Side calls (the router's classifier,
        /// summaries, sub-agents, /spawn, /browse) read something else.
        context: bool,
    },
    /// The full turn is complete — carries the updated full message history
    Done {
        tokens_in: u64,
        tokens_out: u64,
        cache_read: u64,
        cache_write: u64,
        messages: Vec<Message>,
        model_used: String,
    },
    /// An error from a background task that is not part of a turn (voice
    /// preview, transcription). Never touches the loading state: a turn
    /// started meanwhile keeps running and stays cancellable.
    Error(String),
    /// The recorder failed to start. Clears the recording state so Ctrl+R
    /// starts a new recording instead of "stopping" one that never ran.
    RecordingFailed(String),
    /// The running API turn stopped early (API error, iteration cap,
    /// failed compaction). What it did so far is in `App::turn_history`.
    TurnFailed(String),
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
        /// For a compaction that ran in the background between turns: the
        /// session id and the exact history it summarised. The user can keep
        /// working meanwhile, so the result is only valid if the history
        /// still starts with that snapshot. `None` for a turn's own
        /// mid-turn compaction, which the turn already continued from.
        base: Option<(String, Vec<Message>)>,
    },
    /// A background compaction failed. Separate from `Error`, which carries
    /// no state to reset.
    CompactFailed(String),
    /// Informational notice from the harness (not from Claude)
    SystemMessage(String),
    /// The router sent this turn (or the rest of it, after an escalation)
    /// to `model`. `line` is shown when the model differs from the last
    /// routed one.
    Routed { model: String, line: String },
    /// Auto-fix skipped its lint and test run because the project is not
    /// trusted. Shown once per session, not after every edit.
    AutoFixUntrusted,
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

impl AppEvent {
    /// One billed side call, recorded by the event loop like a turn's own
    /// but leaving the ctx % alone.
    pub fn usage(model: &str, u: &crate::api::types::Usage) -> Self {
        AppEvent::Usage {
            model: model.to_string(),
            input: u.input_tokens,
            output: u.output_tokens,
            cache_read: u.cache_read_input_tokens,
            cache_write: u.cache_creation_input_tokens,
            context: false,
        }
    }
}

/// A usage sink for engines that run outside the turn task (/spawn,
/// /browse), so their spend reaches /cost and /budget. Forwarding ends when
/// the engine drops its sender.
pub fn forward_usage(tx: tokio::sync::mpsc::UnboundedSender<AppEvent>) -> crate::tools::UsageSink {
    let (sink, mut rx) = tokio::sync::mpsc::unbounded_channel::<(String, _)>();
    tokio::spawn(async move {
        while let Some((model, usage)) = rx.recv().await {
            if tx.send(AppEvent::usage(&model, &usage)).is_err() {
                break;
            }
        }
    });
    sink
}
