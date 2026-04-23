use crate::browser::middleware::{MiddlewareVerdict, ToolMiddleware};
use async_trait::async_trait;
use sha2::{Digest, Sha256};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use tokio::sync::mpsc;

const WINDOW_SIZE: usize = 10;
const REPEAT_THRESHOLD: usize = 3;

const NUDGES: [&str; 3] = [
    "You seem to be repeating the same action with no effect. Try a different approach — \
     perhaps a different element, a different selector, or navigate to a different section.",
    "This action has failed multiple times on the same page state. Consider: \
     (1) the element may be disabled or overlaid, (2) you may need to scroll first, \
     (3) try using JavaScript evaluation as a fallback, (4) the page may require authentication.",
    "Stopping — the browser agent has repeated the same action 3 times with no progress. \
     The page may be stuck, require a CAPTCHA, or the target element may not be interactable.",
];

#[derive(Clone, Debug, PartialEq, Eq)]
struct ActionFingerprint {
    action_hash: String,
    page_hash: String,
}

pub struct LoopDetector {
    window: VecDeque<ActionFingerprint>,
    nudge_level: usize,
}

impl LoopDetector {
    pub fn new() -> Self {
        Self {
            window: VecDeque::new(),
            nudge_level: 0,
        }
    }

    /// Record an action. Hashes action_type+target and page_text, appends to window,
    /// pruning to WINDOW_SIZE.
    pub fn record_action(&mut self, action_type: &str, target: &str, page_text: &str) {
        let fp = ActionFingerprint {
            action_hash: hash_string(&format!("{action_type}:{target}")),
            page_hash: hash_string(page_text),
        };
        self.window.push_back(fp);
        if self.window.len() > WINDOW_SIZE {
            self.window.pop_front();
        }
    }

    /// Check whether the last REPEAT_THRESHOLD entries are identical.
    /// Returns an escalating nudge string if stagnation is detected, None otherwise.
    /// Nudge level advances each time stagnation is confirmed (capped at NUDGES.len()-1).
    pub fn check_stagnation(&mut self) -> Option<String> {
        if self.window.len() < REPEAT_THRESHOLD {
            return None;
        }
        let tail_start = self.window.len() - REPEAT_THRESHOLD;
        let tail: Vec<_> = self.window.range(tail_start..).collect();
        let first = &tail[0];
        let all_same = tail.iter().all(|fp| fp == first);
        if !all_same {
            return None;
        }
        let level = self.nudge_level.min(NUDGES.len() - 1);
        let nudge = NUDGES[level].to_string();
        self.nudge_level = (self.nudge_level + 1).min(NUDGES.len() - 1);
        Some(nudge)
    }

    /// Clear the window and reset escalation state.
    pub fn reset(&mut self) {
        self.window.clear();
        self.nudge_level = 0;
    }

    pub fn window_len(&self) -> usize {
        self.window.len()
    }
}

impl Default for LoopDetector {
    fn default() -> Self {
        Self::new()
    }
}

/// Public helper: SHA-256 of "{action_type}:{target}".
pub fn fingerprint_action(action_type: &str, target: &str, _extra: &str) -> String {
    hash_string(&format!("{action_type}:{target}"))
}

fn hash_string(s: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(s.as_bytes());
    format!("{:x}", hasher.finalize())
}

// ── Middleware bridge ─────────────────────────────────────────────────────────

pub struct LoopDetectorMiddleware {
    inner: Mutex<LoopDetector>,
    nudge_tx: mpsc::Sender<String>,
    stopped: AtomicBool,
}

impl LoopDetectorMiddleware {
    pub fn new(nudge_tx: mpsc::Sender<String>) -> Self {
        Self {
            inner: Mutex::new(LoopDetector::new()),
            nudge_tx,
            stopped: AtomicBool::new(false),
        }
    }

    /// Returns true if the loop detector reached terminal stagnation (level 3).
    pub fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl ToolMiddleware for LoopDetectorMiddleware {
    async fn before_tool(&self, _tool_name: &str, _input: &serde_json::Value) -> MiddlewareVerdict {
        if self.stopped.load(Ordering::SeqCst) {
            return MiddlewareVerdict::Deny {
                reason: "Stagnation detected: the browser agent has been stopped after \
                         repeating the same action with no effect."
                    .into(),
            };
        }
        MiddlewareVerdict::Allow
    }

    async fn after_tool(&self, tool_name: &str, output: &str) {
        // browser_navigate resets the detector (new page = fresh state).
        if tool_name == "browser_navigate" {
            self.inner.lock().unwrap_or_else(|e| e.into_inner()).reset();
            return;
        }
        let nudge = {
            let mut ld = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            ld.record_action(tool_name, "", output);
            ld.check_stagnation()
        };
        if let Some(nudge) = nudge {
            // If this is the terminal nudge (level 3 — contains "Stopping"),
            // set the stopped flag so before_tool denies subsequent calls.
            if nudge.contains("Stopping") {
                self.stopped.store(true, Ordering::SeqCst);
            }
            let _ = self.nudge_tx.send(nudge).await;
        }
    }
}
