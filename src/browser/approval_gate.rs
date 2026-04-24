//! Destructive-action approval gate for autonomous browser agent.
//!
//! Pattern-matches tool calls against URL paths, button text, form-field
//! signals, visible prices, and user-defined extension patterns. Read-only
//! tools always pass; everything else is checked against compiled RegexSets.

use crate::browser::middleware::{MiddlewareVerdict, ToolMiddleware};
use async_trait::async_trait;
use regex::{Regex, RegexSet};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, oneshot};

/// Read-only tools that never require approval.
const READ_ONLY_TOOLS: &[&str] = &[
    "browser_navigate",
    "browser_snapshot",
    "browser_screenshot",
    "browser_get_text",
    "browser_wait",
    "browse_done",
];

/// Verdict returned by the approval gate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateVerdict {
    Allow,
    RequireConfirmation { reason: String, detail: String },
}

/// Context passed to the gate for each tool invocation.
#[derive(Debug, Clone, Default)]
pub struct GateContext {
    pub tool_name: String,
    pub url: String,
    pub target_text: String,
    /// e.g. "input:type=password", "input:autocomplete=cc-number"
    pub form_field_signals: Vec<String>,
    /// e.g. "$12.99", "€0.00"
    pub visible_prices: Vec<String>,
}

/// Compiled approval gate — all regexes are built once at construction.
pub struct ApprovalGate {
    url_set: RegexSet,
    button_set: RegexSet,
    form_set: RegexSet,
    price_re: Regex,
    extra_patterns: Vec<Regex>,
}

// --- Built-in pattern strings ---------------------------------------------------

fn url_patterns() -> Vec<String> {
    vec![
        r"/pay(ments?)?(/|\?|$)".into(),
        r"/checkout(/|\?|$)".into(),
        r"/purchase(/|\?|$)".into(),
        r"/order-review".into(),
        r"/billing/add-card".into(),
        r"/wallet/transfer".into(),
        r"/oauth/authorize".into(),
        r"/consent".into(),
        r"/authorize/grant".into(),
    ]
}

fn button_patterns() -> Vec<String> {
    vec![
        // Category 2 — payment / purchase
        r"(?i)confirm (purchase|order|payment)".into(),
        r"(?i)submit payment".into(),
        r"(?i)place (my )?order".into(),
        r"(?i)complete (purchase|order)".into(),
        r"(?i)buy now".into(),
        r"(?i)pay (now|\$)".into(),
        r"(?i)start (free )?trial".into(),
        r"(?i)try free for \d+ days?".into(),
        r"(?i)upgrade (to premium|plan|account)".into(),
        // Category 3 — account destruction
        r"(?i)delete (account|repository|organization|workspace|project)".into(),
        r"(?i)remove (account|user)".into(),
        r"(?i)revoke (access|permissions|api key)".into(),
        r"(?i)permanently delete".into(),
        r"(?i)empty trash".into(),
        r"(?i)cancel subscription".into(),
        r"(?i)close account".into(),
        r"(?i)deactivate".into(),
        // Category 4 — publication / blast radius
        r"(?i)post (tweet|publicly|to public)".into(),
        r"(?i)publish (article|page|post)".into(),
        r"(?i)go live".into(),
        r"(?i)^tweet$".into(),
        r"(?i)share publicly".into(),
        r"(?i)send (email|message|invitation)".into(),
        r"(?i)reply all".into(),
        // Category 5 — OAuth / permission grants
        r"(?i)(authorize|allow) (this )?(app|application|access)".into(),
        r"(?i)grant (access|permissions)".into(),
        r"(?i)i authorize".into(),
        // Category 6 — contracts / legal
        r"(?i)(accept|agree to) (terms|contract|agreement)".into(),
        r"(?i)sign contract".into(),
        r"(?i)sign electronically".into(),
        r"(?i)i agree (and|to) (continue|proceed)".into(),
    ]
}

fn form_field_patterns() -> Vec<String> {
    vec![
        r"^input:type=password$".into(),
        r"^input:autocomplete=cc-(number|exp|csc|name)$".into(),
        r"^input:name=(card|cc|cvv|cvc|pin|ssn)$".into(),
        r"^input:id=(card|cc|cvv|cvc|pin|ssn)$".into(),
    ]
}

const PRICE_PATTERN: &str = r"[\$€£]\s*(\d+\.\d{2}|\d+,\d{2})";

// --- Implementation ------------------------------------------------------------

impl Default for ApprovalGate {
    fn default() -> Self {
        Self::with_user_patterns(Vec::new())
    }
}

impl ApprovalGate {
    /// Create a gate, appending user-supplied regex patterns.
    /// Invalid user patterns are logged to stderr and skipped.
    pub fn with_user_patterns(user_patterns: Vec<String>) -> Self {
        let mut extras = Vec::new();
        for pat in &user_patterns {
            match Regex::new(pat) {
                Ok(re) => extras.push(re),
                Err(e) => eprintln!("approval_gate: skipping invalid user pattern {pat:?}: {e}"),
            }
        }

        Self {
            url_set: RegexSet::new(url_patterns()).expect("built-in URL patterns must compile"),
            button_set: RegexSet::new(button_patterns())
                .expect("built-in button patterns must compile"),
            form_set: RegexSet::new(form_field_patterns())
                .expect("built-in form patterns must compile"),
            price_re: Regex::new(PRICE_PATTERN).expect("price pattern must compile"),
            extra_patterns: extras,
        }
    }

    /// Evaluate a tool call context. Returns `Allow` or `RequireConfirmation`.
    pub fn check(&self, c: &GateContext) -> GateVerdict {
        // 1. Read-only tools always pass.
        if READ_ONLY_TOOLS.contains(&c.tool_name.as_str()) {
            return GateVerdict::Allow;
        }

        let mut reasons: Vec<String> = Vec::new();

        // 2. URL patterns
        if self.url_set.is_match(&c.url) {
            reasons.push(format!("url_pattern: {}", c.url));
        }

        // 3. Button / text patterns (categories 2-6)
        if self.button_set.is_match(&c.target_text) {
            reasons.push(format!("button_text: {}", c.target_text));
        }

        // 4. Form field signals
        for sig in &c.form_field_signals {
            if self.form_set.is_match(sig) {
                reasons.push(format!("form_field: {sig}"));
            }
        }

        // 5. Visible prices — skip zero amounts
        for price in &c.visible_prices {
            if let Some(caps) = self.price_re.captures(price) {
                if let Some(amount_str) = caps.get(1) {
                    let normalized = amount_str.as_str().replace(',', ".");
                    if let Ok(val) = normalized.parse::<f64>() {
                        if val >= 0.01 {
                            reasons.push(format!("visible_price: {price}"));
                        }
                    }
                }
            }
        }

        // 6. Extra (user-defined) patterns — checked against url + target_text
        for re in &self.extra_patterns {
            if re.is_match(&c.url) || re.is_match(&c.target_text) {
                reasons.push(format!("user_pattern: {}", re.as_str()));
            }
        }

        // 7. Verdict
        if reasons.is_empty() {
            GateVerdict::Allow
        } else {
            GateVerdict::RequireConfirmation {
                reason: reasons[0].clone(),
                detail: reasons.join("; "),
            }
        }
    }
}

// ── Middleware bridge ─────────────────────────────────────────────────────────

/// Approval prompt sent to the host (TUI/SDK/voice).
pub struct ApprovalPrompt {
    pub step: u32,
    pub tool_name: String,
    pub target_text: String,
    pub url: String,
    pub reason: String,
    pub reply: oneshot::Sender<bool>,
}

pub struct ApprovalGateMiddleware {
    gate: ApprovalGate,
    policy: crate::browser::browse_loop::BrowsePolicy,
    current_url: Arc<tokio::sync::Mutex<String>>,
    approval_tx: mpsc::Sender<ApprovalPrompt>,
    step_counter: Arc<AtomicU32>,
    /// Tracks consecutive denial count per action key ("{tool_name}:{target_text}").
    denial_counts: Mutex<HashMap<String, u32>>,
    /// Set when the same action is denied twice — triggers session termination.
    user_denied: AtomicBool,
    /// When true, also listen for a spoken "yes/approve" alongside the keyboard reply.
    voice: bool,
    /// Optional handle to the live browser session — used to resolve an @eN
    /// ref to its real button label so button-text patterns (e.g. "buy now")
    /// can actually match. None in tests where no browser is attached.
    browser_session: Option<Arc<tokio::sync::Mutex<crate::browser::BrowserSession>>>,
}

impl ApprovalGateMiddleware {
    pub fn new(
        gate: ApprovalGate,
        policy: crate::browser::browse_loop::BrowsePolicy,
        current_url: Arc<tokio::sync::Mutex<String>>,
        approval_tx: mpsc::Sender<ApprovalPrompt>,
        step_counter: Arc<AtomicU32>,
        voice: bool,
    ) -> Self {
        Self {
            gate,
            policy,
            current_url,
            approval_tx,
            step_counter,
            denial_counts: Mutex::new(HashMap::new()),
            user_denied: AtomicBool::new(false),
            voice,
            browser_session: None,
        }
    }

    /// Attach a browser session so the gate can resolve @eN refs to button
    /// labels before applying the pattern set.
    pub fn with_browser_session(
        mut self,
        session: Option<Arc<tokio::sync::Mutex<crate::browser::BrowserSession>>>,
    ) -> Self {
        self.browser_session = session;
        self
    }

    /// Returns true if the user denied the same action twice, triggering termination.
    pub fn is_user_denied(&self) -> bool {
        self.user_denied.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl ToolMiddleware for ApprovalGateMiddleware {
    async fn before_tool(&self, tool_name: &str, input: &serde_json::Value) -> MiddlewareVerdict {
        use crate::browser::browse_loop::BrowsePolicy;

        // If user already denied twice, block everything.
        if self.user_denied.load(Ordering::SeqCst) {
            return MiddlewareVerdict::Deny {
                reason: "User denied this action twice. Terminating browse session.".into(),
            };
        }

        // Yolo policy: always allow.
        if self.policy == BrowsePolicy::Yolo {
            return MiddlewareVerdict::Allow;
        }

        // Read-only tools always pass, regardless of policy.
        if READ_ONLY_TOOLS.contains(&tool_name) {
            return MiddlewareVerdict::Allow;
        }

        let url = self.current_url.lock().await.clone();
        // `ref_or_selector` is the raw identifier (e.g. "@e2" or ".btn-submit").
        // It's what we use for the denial-counter key so the same action is
        // tracked consistently across retries.
        let ref_or_selector = input["ref"]
            .as_str()
            .or_else(|| input["selector"].as_str())
            .unwrap_or("")
            .to_string();
        // `target_text` is what we match against `button_patterns`. For @eN
        // refs, resolve to the element's accessible name via the browser
        // session's ref-name map; otherwise fall back to the raw identifier.
        let target_text = if ref_or_selector.starts_with('@')
            && let Some(session_arc) = &self.browser_session
        {
            let session = session_arc.lock().await;
            session
                .resolve_ref_name(&ref_or_selector)
                .map(|s| s.to_string())
                .unwrap_or_else(|| ref_or_selector.clone())
        } else {
            ref_or_selector.clone()
        };

        let gate_ctx = GateContext {
            tool_name: tool_name.to_string(),
            url: url.clone(),
            target_text: target_text.clone(),
            form_field_signals: Vec::new(),
            visible_prices: Vec::new(),
        };

        // Ask policy: force confirmation for every non-read-only tool.
        let verdict = if self.policy == BrowsePolicy::Ask {
            GateVerdict::RequireConfirmation {
                reason: "ask policy".to_string(),
                detail: format!("{tool_name} on {url}"),
            }
        } else {
            // Pattern policy: delegate to the compiled gate.
            self.gate.check(&gate_ctx)
        };

        match verdict {
            GateVerdict::Allow => {
                // Clear denial counter on approval for this action key.
                let key = format!("{tool_name}:{target_text}");
                self.denial_counts.lock().unwrap_or_else(|e| e.into_inner()).remove(&key);
                MiddlewareVerdict::Allow
            }
            GateVerdict::RequireConfirmation { reason, .. } => {
                // +1 because the step_emitter middleware increments after the gate runs.
                let step = self.step_counter.load(Ordering::Relaxed) + 1;
                let (tx, rx) = oneshot::channel();
                let prompt = ApprovalPrompt {
                    step,
                    tool_name: tool_name.to_string(),
                    target_text: target_text.clone(),
                    url,
                    reason: reason.clone(),
                    reply: tx,
                };
                // If the host receiver is gone, deny by default.
                if self.approval_tx.send(prompt).await.is_err() {
                    return MiddlewareVerdict::Deny {
                        reason: "approval channel closed".to_string(),
                    };
                }
                use tokio::time::{timeout, Duration};
                // If voice is on, race the keyboard reply against a voice-approval
                // listener. Whichever resolves first wins. Voice only contributes
                // an Approve vote (false/timeout is ignored unless no keyboard reply
                // arrives either).
                let approved_opt: Option<bool> = if self.voice {
                    tokio::select! {
                        kb = timeout(Duration::from_secs(60), rx) => match kb {
                            Ok(Ok(b)) => Some(b),
                            _ => None,
                        },
                        voice_yes = crate::voice::await_voice_approval(60) => {
                            if voice_yes { Some(true) } else { None }
                        }
                    }
                } else {
                    match timeout(Duration::from_secs(60), rx).await {
                        Ok(Ok(b)) => Some(b),
                        _ => None,
                    }
                };
                match approved_opt {
                    Some(true) => {
                        // Approved — clear denial counter for this action.
                        let key = format!("{tool_name}:{target_text}");
                        self.denial_counts.lock().unwrap_or_else(|e| e.into_inner()).remove(&key);
                        MiddlewareVerdict::Allow
                    }
                    Some(false) => {
                        // User explicitly denied — increment counter; terminate after 2 denials.
                        let key = format!("{tool_name}:{target_text}");
                        let count = {
                            let mut counts = self.denial_counts.lock().unwrap_or_else(|e| e.into_inner());
                            let entry = counts.entry(key).or_insert(0);
                            *entry += 1;
                            *entry
                        };
                        if count >= 2 {
                            self.user_denied.store(true, Ordering::SeqCst);
                            return MiddlewareVerdict::Deny {
                                reason: "User denied this action twice. Terminating browse session."
                                    .into(),
                            };
                        }
                        MiddlewareVerdict::Deny { reason }
                    }
                    None => MiddlewareVerdict::Deny {
                        reason: "approval timed out or channel dropped".into(),
                    },
                }
            }
        }
    }

    async fn after_tool(&self, _tool_name: &str, _output: &str) {
        // Step counting is owned by StepEmitterMiddleware (runs after this middleware).
    }
}
