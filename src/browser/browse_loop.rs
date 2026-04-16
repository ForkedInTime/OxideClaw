use anyhow::Result;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

/// Policy for the approval gate during this run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BrowsePolicy {
    Pattern,
    Ask,
    Yolo,
}

impl Default for BrowsePolicy {
    fn default() -> Self { Self::Pattern }
}

/// A single browse-run configuration.
#[derive(Debug, Clone)]
pub struct BrowseRequest {
    pub goal: String,
    pub policy: BrowsePolicy,
    pub max_steps: u32,
    pub voice: bool,
}

/// Progress events streamed to the caller during a run.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BrowseProgress {
    Started { goal: String, max_steps: u32 },
    Step { n: u32, action: String, target: String },
    Nudge { level: u8, text: String },
    ApprovalNeeded { step: u32, action: String, target_text: String, url: String, reason: String },
    Completed(BrowseResult),
}

/// Final result of a browse run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrowseResult {
    pub achieved: bool,
    pub summary: String,
    pub reason: BrowseReason,
    pub steps_used: u32,
    pub final_url: Option<String>,
}

/// Why the loop terminated.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BrowseReason {
    Done,
    Bailed,
    StepCap,
    Stagnation,
    Budget,
    BrowserCrashed,
    UserDenied,
    Cancelled,
}

/// Stub — wiring happens in Task 7.
pub async fn run_browse(
    _req: BrowseRequest,
    _progress_tx: mpsc::Sender<BrowseProgress>,
) -> Result<BrowseResult> {
    anyhow::bail!("run_browse not yet implemented (Task 7)")
}
