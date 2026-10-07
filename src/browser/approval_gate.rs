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
        // Case-insensitive: paths like "/Checkout/Payment" are common, and
        // Shopify serves its checkout under "/checkouts/<token>".
        r"(?i)/pay(ments?)?(/|\?|$)".into(),
        r"(?i)/checkouts?(/|\?|$)".into(),
        r"(?i)/purchase(/|\?|$)".into(),
        r"(?i)/order-review".into(),
        r"(?i)/billing/add-card".into(),
        r"(?i)/wallet/transfer".into(),
        r"(?i)/oauth/authorize".into(),
        r"(?i)/consent".into(),
        r"(?i)/authorize/grant".into(),
    ]
}

fn button_patterns() -> Vec<String> {
    vec![
        // Category 2 — payment / purchase
        // Real labels carry a determiner ("Place your order", "Delete this
        // repository", "Close my account"), so the patterns allow one.
        r"(?i)(confirm|complete) (my |your |the )?(purchase|order|payment)".into(),
        r"(?i)submit payment".into(),
        r"(?i)place (my |your |the )?order".into(),
        r"(?i)buy now".into(),
        r"(?i)pay (now|\$)".into(),
        r"(?i)^\s*pay\s*$".into(),
        r"(?i)start (free )?trial".into(),
        r"(?i)try free for \d+ days?".into(),
        r"(?i)upgrade (to premium|plan|account)".into(),
        // Category 3 — account destruction
        r"(?i)\bdelete (this |my |your |the )?(account|repository|repo|organization|workspace|project)"
            .into(),
        r"(?i)remove (account|user)".into(),
        r"(?i)revoke (access|permissions|api key)".into(),
        r"(?i)permanently delete".into(),
        r"(?i)empty trash".into(),
        r"(?i)cancel subscription".into(),
        r"(?i)(close|deactivate) (my |your |this )?account".into(),
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
        // Accessible-name signals (what the middleware can actually see).
        r"^name:.*(password|passcode|passphrase|card number|credit card|debit card|cvv|cvc|security code|expir|social security|\bssn\b|\bpin\b|routing number|account number|iban|sort code)"
            .into(),
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

    /// Price-looking strings in page text, for `GateContext::visible_prices`.
    /// Capped so a catalogue page does not produce thousands of reasons.
    pub fn visible_prices_in(&self, text: &str) -> Vec<String> {
        self.price_re
            .find_iter(text)
            .take(20)
            .map(|m| m.as_str().to_string())
            .collect()
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
            if let Some(caps) = self.price_re.captures(price)
                && let Some(amount_str) = caps.get(1)
            {
                let normalized = amount_str.as_str().replace(',', ".");
                if let Ok(val) = normalized.parse::<f64>()
                    && val >= 0.01
                {
                    reasons.push(format!("visible_price: {price}"));
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

/// Form-field signals for the gate, derived from what the middleware can
/// see: the field's accessible name. Only `browser_fill` produces one.
pub fn form_signals_for(tool_name: &str, target_text: &str) -> Vec<String> {
    if tool_name == "browser_fill" && !target_text.is_empty() {
        vec![format!("name:{}", target_text.to_lowercase())]
    } else {
        Vec::new()
    }
}

// ── Middleware bridge ─────────────────────────────────────────────────────────

/// What the voice gate says before it listens. Fixed wording on purpose: it
/// carries no page text (a button labelled "Confirm purchase" read aloud is a
/// "confirm" the mic may hear) and no yes/no words of its own.
fn gate_trip_phrase(tool_name: &str) -> String {
    let action = match tool_name {
        "browser_click" => "click",
        "browser_fill" => "fill in a field",
        "browser_press_key" => "press a key",
        "browser_navigate" => "open a page",
        _ => "take a browser action",
    };
    format!("Approval needed to {action}. Please answer.")
}

/// How long an approval prompt waits for an answer.
const APPROVAL_WINDOW: std::time::Duration = std::time::Duration::from_secs(60);
/// One voice recording. Short, so a spoken yes is acted on seconds after it
/// is said rather than after the whole prompt window plus transcription.
const VOICE_LISTEN_WINDOW: std::time::Duration = std::time::Duration::from_secs(8);

/// The answer to a voice-mode prompt: a key until `deadline`, or a spoken yes
/// heard in back-to-back `listen` windows opened before `deadline`.
/// `listen(window)` is `None` when it cannot record at all, else whether it
/// heard an approval. Voice only approves; silence, a "no" or a failed
/// transcription leaves the keyboard to decide. A window opened before the
/// deadline may finish transcribing after it, and a key still answers then.
async fn await_key_or_voice<F, Fut>(
    mut rx: oneshot::Receiver<bool>,
    deadline: tokio::time::Instant,
    mut listen: F,
) -> Option<bool>
where
    F: FnMut(std::time::Duration) -> Fut,
    Fut: std::future::Future<Output = Option<bool>>,
{
    let voice = async {
        loop {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            if left.is_zero() {
                return false;
            }
            match listen(left.min(VOICE_LISTEN_WINDOW)).await {
                Some(true) => return true,
                Some(false) => {}
                None => return false,
            }
        }
    };
    let heard_yes = tokio::select! {
        kb = &mut rx => return kb.ok(),
        heard = voice => heard,
    };
    if heard_yes {
        return Some(true);
    }
    match tokio::time::timeout_at(deadline, rx).await {
        Ok(Ok(b)) => Some(b),
        _ => None,
    }
}

/// Source of [`ApprovalPrompt::id`]: never repeats in this process.
static PROMPT_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// A fresh, never-repeating id for an approval prompt.
pub fn next_prompt_id() -> u64 {
    PROMPT_SEQ.fetch_add(1, Ordering::Relaxed) + 1
}

/// Approval prompt sent to the host (TUI/SDK/voice).
pub struct ApprovalPrompt {
    /// Unique per prompt. `step` repeats after a denied or timed-out prompt
    /// (the step counter only moves for allowed calls), so a late reply
    /// must be matched on this instead.
    pub id: u64,
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
    /// Tracks consecutive denial count per action key ("{tool_name}:{target_text}",
    /// or "activate:{label}" for clicks and Enter/Space).
    denial_counts: Mutex<HashMap<String, u32>>,
    /// Set when the same action is denied twice — triggers session termination.
    user_denied: AtomicBool,
    /// When true, also listen for a spoken "yes/approve" alongside the keyboard reply.
    voice: bool,
    /// The user's `voiceApiUrl`, for transcribing that spoken reply.
    voice_api_url: Option<String>,
    /// Optional handle to the live browser session — used to resolve an @eN
    /// ref to its real button label so button-text patterns (e.g. "buy now")
    /// can actually match. None in tests where no browser is attached.
    browser_session: Option<Arc<tokio::sync::Mutex<crate::browser::BrowserSession>>>,
    /// A gated-and-approved `browser_fill` happened on the current page:
    /// the Enter key that would submit it must be gated as well, or the
    /// form goes out without any button the text patterns could see.
    sensitive_fill_pending: AtomicBool,
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
            voice_api_url: None,
            browser_session: None,
            sensitive_fill_pending: AtomicBool::new(false),
        }
    }

    /// Transcribe spoken approvals at the user's `voiceApiUrl`.
    pub fn with_voice_api_url(mut self, url: Option<String>) -> Self {
        self.voice_api_url = url;
        self
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

    /// The page's URL as Chrome sees it now. The cached value is what the
    /// last tool recorded, but Chrome follows redirects on its own: an OAuth
    /// "Sign in" link lands on /oauth/authorize while the cache still holds
    /// the app's URL, so URL patterns never matched the consent page.
    async fn live_url(&self) -> String {
        let client = match &self.browser_session {
            // Clone the client out: the tool about to run re-locks the session.
            Some(s) => s.lock().await.client().ok().cloned(),
            None => None,
        };
        let live = match client {
            // CdpClient::send waits up to 30s; a hung page must not stall
            // every gated action, so fall back to the cache instead.
            Some(c) => tokio::time::timeout(
                std::time::Duration::from_secs(2),
                crate::browser::actions::current_url(&c),
            )
            .await
            .ok()
            .flatten(),
            None => None,
        };
        match live {
            Some(u) => {
                if let Some(s) = &self.browser_session {
                    s.lock().await.current_url = u.clone();
                }
                *self.current_url.lock().await = u.clone();
                u
            }
            None => self.current_url.lock().await.clone(),
        }
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

        let url = self.live_url().await;
        // `ref_or_selector` is the element identifier, with refs normalized to
        // "@eN": the tools also accept a bare "eN", and the raw spelling would
        // skip name resolution below (so "Delete account" never matched) and
        // split the denial counter across spellings.
        let ref_or_selector = match input["ref"].as_str() {
            Some(r) if !r.is_empty() => super::normalize_ref(r),
            _ => input["selector"].as_str().unwrap_or("").to_string(),
        };
        let key_lower = input["key"].as_str().map(|k| k.to_ascii_lowercase());
        let is_submit_key = tool_name == "browser_press_key"
            && matches!(key_lower.as_deref(), Some("enter" | "return"));
        let is_activating_key = is_submit_key
            || (tool_name == "browser_press_key"
                && matches!(key_lower.as_deref(), Some("space" | " ")));
        // `target_text` is what we match against `button_patterns`. For refs,
        // resolve to the element's accessible name via the browser session's
        // ref-name map; otherwise fall back to the identifier.
        let (mut target_text, visible_prices, client) =
            if let Some(session_arc) = &self.browser_session {
                let session = session_arc.lock().await;
                let name = session
                    .resolve_ref_name(&ref_or_selector)
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| ref_or_selector.clone());
                (
                    name,
                    self.gate.visible_prices_in(&session.last_page_text),
                    session.client().ok().cloned(),
                )
            } else {
                (ref_or_selector.clone(), Vec::new(), None)
            };
        // Enter/Space press whatever has focus, which the input does not
        // name. Ask the page, bounded like live_url; on failure keep the old
        // behaviour rather than wedge the loop.
        if is_activating_key
            && let Some(c) = client
            && let Ok(Some(label)) = tokio::time::timeout(
                std::time::Duration::from_secs(2),
                crate::browser::actions::active_element_label(&c, is_submit_key),
            )
            .await
        {
            target_text = label;
        }
        // A click and a key press on the same control are one action: a
        // denied "Delete account" click must not get a fresh counter when
        // retried as Tab + Enter.
        let action_key = if tool_name == "browser_click" || is_activating_key {
            format!("activate:{target_text}")
        } else {
            format!("{tool_name}:{target_text}")
        };

        let gate_ctx = GateContext {
            tool_name: tool_name.to_string(),
            url: url.clone(),
            target_text: target_text.clone(),
            form_field_signals: form_signals_for(tool_name, &target_text),
            visible_prices,
        };

        // Ask policy: force confirmation for every non-read-only tool.
        let verdict = if self.policy == BrowsePolicy::Ask {
            GateVerdict::RequireConfirmation {
                reason: "ask policy".to_string(),
                detail: format!("{tool_name} on {url}"),
            }
        } else if is_submit_key && self.sensitive_fill_pending.load(Ordering::SeqCst) {
            GateVerdict::RequireConfirmation {
                reason: "submit after sensitive fill".to_string(),
                detail: format!("Enter would submit the form holding sensitive data on {url}"),
            }
        } else {
            // Pattern policy: delegate to the compiled gate.
            self.gate.check(&gate_ctx)
        };
        let gated_fill = tool_name == "browser_fill"
            && matches!(verdict, GateVerdict::RequireConfirmation { .. });

        match verdict {
            GateVerdict::Allow => {
                // Clear denial counter on approval for this action key.
                self.denial_counts
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&action_key);
                MiddlewareVerdict::Allow
            }
            GateVerdict::RequireConfirmation { reason, .. } => {
                // +1 because the step_emitter middleware increments after the gate runs.
                let step = self.step_counter.load(Ordering::Relaxed) + 1;
                let (tx, rx) = oneshot::channel();
                let prompt = ApprovalPrompt {
                    id: next_prompt_id(),
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
                // If voice is on, announce the gate, then race the keyboard reply
                // against a voice-approval listener (see `await_key_or_voice`).
                let approved_opt: Option<bool> = if self.voice {
                    let mut rx = rx;
                    // The announcement must finish before the mic opens, or the
                    // speaker's own words are transcribed as the reply. A key
                    // pressed meanwhile still answers.
                    let phrase = gate_trip_phrase(tool_name);
                    let early = tokio::select! {
                        kb = &mut rx => Some(kb.ok()),
                        _ = crate::voice::speak_browse_milestone(
                            crate::voice::BrowseMilestone::GateTrip,
                            &phrase,
                        ) => None,
                    };
                    match early {
                        Some(kb) => kb,
                        // One 60 s recording raced a 60 s keyboard timeout,
                        // which always expired before the transcription
                        // finished, and a missing recorder denied at once.
                        None => {
                            let deadline = tokio::time::Instant::now() + APPROVAL_WINDOW;
                            let api_url = self.voice_api_url.as_deref();
                            let phrase = phrase.as_str();
                            await_key_or_voice(rx, deadline, |window| async move {
                                crate::voice::find_recorder()?;
                                Some(
                                    crate::voice::await_voice_approval(
                                        window.as_secs().max(1),
                                        api_url,
                                        phrase,
                                    )
                                    .await,
                                )
                            })
                            .await
                        }
                    }
                } else {
                    match tokio::time::timeout(APPROVAL_WINDOW, rx).await {
                        Ok(Ok(b)) => Some(b),
                        _ => None,
                    }
                };
                match approved_opt {
                    Some(true) => {
                        // Approved — clear denial counter for this action.
                        self.denial_counts
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .remove(&action_key);
                        if gated_fill {
                            self.sensitive_fill_pending.store(true, Ordering::SeqCst);
                        }
                        MiddlewareVerdict::Allow
                    }
                    Some(false) => {
                        // User explicitly denied — increment counter; terminate after 2 denials.
                        let count = {
                            let mut counts =
                                self.denial_counts.lock().unwrap_or_else(|e| e.into_inner());
                            let entry = counts.entry(action_key).or_insert(0);
                            *entry += 1;
                            *entry
                        };
                        if count >= 2 {
                            self.user_denied.store(true, Ordering::SeqCst);
                            return MiddlewareVerdict::Deny {
                                reason:
                                    "User denied this action twice. Terminating browse session."
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

    async fn after_tool(&self, tool_name: &str, output: &str) -> Option<String> {
        // Step counting is owned by StepEmitterMiddleware (runs after this middleware).
        // A navigation leaves the form behind. A failed one (bad scheme,
        // blocked host) never left the page, so the filled form is still
        // there and Enter must stay gated; "Navigated to:" is only emitted
        // by browser_navigate's success path.
        if tool_name == "browser_navigate" && output.starts_with("Navigated to:") {
            self.sensitive_fill_pending.store(false, Ordering::SeqCst);
        }
        None
    }

    fn should_stop(&self) -> bool {
        self.is_user_denied()
    }
}

#[cfg(test)]
mod wiring_tests {
    use super::*;
    use crate::browser::browse_loop::BrowsePolicy;
    use serde_json::json;
    use std::sync::atomic::AtomicU32;

    /// The gate has form-field patterns, but production never fed it a
    /// signal (`form_field_signals: Vec::new()`), so a fill into a card or
    /// password field was never gated. Fields are identified by their
    /// accessible name — the only thing the middleware can see.
    #[test]
    fn a_fill_into_a_sensitive_field_requires_confirmation_by_name() {
        let gate = ApprovalGate::default();
        for name in ["Card number", "CVV", "Password", "Social Security Number"] {
            let ctx = GateContext {
                tool_name: "browser_fill".into(),
                url: "https://shop.example/account".into(),
                target_text: name.into(),
                form_field_signals: form_signals_for("browser_fill", name),
                visible_prices: vec![],
            };
            assert!(
                matches!(gate.check(&ctx), GateVerdict::RequireConfirmation { .. }),
                "{name} must be gated"
            );
        }
        let ctx = GateContext {
            tool_name: "browser_fill".into(),
            url: "https://shop.example/search".into(),
            target_text: "Search".into(),
            form_field_signals: form_signals_for("browser_fill", "Search"),
            visible_prices: vec![],
        };
        assert_eq!(gate.check(&ctx), GateVerdict::Allow);
    }

    /// The gate announcement is spoken right before the mic opens; any of
    /// its words that reached the recording must neither approve nor veto a
    /// real "yes", whatever the tool.
    #[test]
    fn the_gate_announcement_is_neutral_to_the_voice_matcher() {
        for tool in [
            "browser_click",
            "browser_fill",
            "browser_press_key",
            "browser_navigate",
            "browser_unknown",
        ] {
            let phrase = gate_trip_phrase(tool);
            assert!(
                !crate::voice::is_spoken_approval(&phrase, ""),
                "{phrase:?} approves by itself"
            );
            assert!(
                crate::voice::is_spoken_approval(&format!("{phrase} yes"), ""),
                "{phrase:?} vetoes a spoken yes"
            );
        }
    }

    /// Build a middleware with an auto-approving host.
    fn middleware() -> (Arc<ApprovalGateMiddleware>, tokio::task::JoinHandle<u32>) {
        let (tx, mut rx) = mpsc::channel::<ApprovalPrompt>(8);
        let mw = Arc::new(ApprovalGateMiddleware::new(
            ApprovalGate::default(),
            BrowsePolicy::Pattern,
            Arc::new(tokio::sync::Mutex::new(
                "https://shop.example/account".into(),
            )),
            tx,
            Arc::new(AtomicU32::new(0)),
            false,
        ));
        let host = tokio::spawn(async move {
            let mut prompts = 0;
            while let Some(p) = rx.recv().await {
                prompts += 1;
                let _ = p.reply.send(true);
            }
            prompts
        });
        (mw, host)
    }

    /// Typing a card number and then pressing Enter submits the form
    /// without ever clicking a button the button-patterns could see.
    #[tokio::test]
    async fn enter_after_an_approved_sensitive_fill_is_gated_too() {
        let (mw, host) = middleware();
        let fill = json!({"selector": "Card number", "value": "4111"});
        assert!(matches!(
            mw.before_tool("browser_fill", &fill).await,
            MiddlewareVerdict::Allow
        ));
        let enter = json!({"key": "Enter"});
        assert!(matches!(
            mw.before_tool("browser_press_key", &enter).await,
            MiddlewareVerdict::Allow
        ));
        drop(mw);
        assert_eq!(
            host.await.unwrap(),
            2,
            "fill and the Enter that submits it must both prompt"
        );
    }

    /// A navigate that fails before leaving the page keeps the filled form,
    /// so it must not disarm the Enter gate; a real navigation does.
    #[tokio::test]
    async fn a_failed_navigate_keeps_enter_gated_after_a_sensitive_fill() {
        let (mw, host) = middleware();
        let fill = json!({"selector": "Card number", "value": "4111"});
        mw.before_tool("browser_fill", &fill).await;
        mw.after_tool(
            "browser_navigate",
            "Tool error: navigation URL 'x' is missing an http(s):// scheme",
        )
        .await;
        assert!(mw.sensitive_fill_pending.load(Ordering::SeqCst));
        mw.after_tool(
            "browser_navigate",
            "Navigated to: https://shop.example/done\nTitle: Done",
        )
        .await;
        assert!(!mw.sensitive_fill_pending.load(Ordering::SeqCst));
        drop(mw);
        assert_eq!(host.await.unwrap(), 1, "only the fill prompted");
    }

    /// A denied prompt leaves the step counter where it was, so the next
    /// gated action has the same step; only the prompt id tells them apart.
    #[tokio::test]
    async fn prompts_at_the_same_step_get_distinct_ids() {
        let (tx, mut rx) = mpsc::channel::<ApprovalPrompt>(8);
        let mw = ApprovalGateMiddleware::new(
            ApprovalGate::default(),
            BrowsePolicy::Pattern,
            Arc::new(tokio::sync::Mutex::new(
                "https://shop.example/account".into(),
            )),
            tx,
            Arc::new(AtomicU32::new(0)),
            false,
        );
        let host = tokio::spawn(async move {
            let mut seen = Vec::new();
            while let Some(p) = rx.recv().await {
                seen.push((p.step, p.id));
                let _ = p.reply.send(false);
            }
            seen
        });
        let fill = json!({"selector": "Card number", "value": "4111"});
        for _ in 0..2 {
            assert!(matches!(
                mw.before_tool("browser_fill", &fill).await,
                MiddlewareVerdict::Deny { .. }
            ));
        }
        drop(mw);
        let seen = host.await.unwrap();
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0].0, seen[1].0, "same step");
        assert_ne!(seen[0].1, seen[1].1, "distinct prompt ids");
    }

    #[tokio::test]
    async fn enter_with_no_sensitive_fill_is_not_gated() {
        let (mw, host) = middleware();
        let enter = json!({"key": "Enter"});
        assert!(matches!(
            mw.before_tool("browser_press_key", &enter).await,
            MiddlewareVerdict::Allow
        ));
        drop(mw);
        assert_eq!(host.await.unwrap(), 0);
    }
}

#[cfg(test)]
mod price_signal_tests {
    use super::*;
    use crate::browser::browse_loop::BrowsePolicy;
    use serde_json::json;
    use std::sync::atomic::AtomicU32;

    /// A price on screen is the gate's fifth signal and was never fed in
    /// production. With the last page text on the session, a click on a
    /// checkout page showing "$49.99" prompts even without a matching
    /// button name.
    #[tokio::test]
    async fn a_visible_price_gates_a_click() {
        let (tx, mut rx) = mpsc::channel::<ApprovalPrompt>(8);
        let session = Arc::new(tokio::sync::Mutex::new(
            crate::browser::BrowserSession::default(),
        ));
        {
            let mut s = session.lock().await;
            s.last_page_text = "Your total today: $49.99 [Continue]".into();
            s.set_refs_with_names(
                std::collections::HashMap::from([("@e1".to_string(), 1i64)]),
                std::collections::HashMap::from([("@e1".to_string(), "Continue".to_string())]),
            );
        }
        let mw = ApprovalGateMiddleware::new(
            ApprovalGate::default(),
            BrowsePolicy::Pattern,
            Arc::new(tokio::sync::Mutex::new("https://shop.example/cart".into())),
            tx,
            Arc::new(AtomicU32::new(0)),
            false,
        )
        .with_browser_session(Some(session));
        let host = tokio::spawn(async move {
            let mut n = 0;
            while let Some(p) = rx.recv().await {
                n += 1;
                let _ = p.reply.send(true);
            }
            n
        });
        mw.before_tool("browser_click", &json!({"ref": "@e1"}))
            .await;
        drop(mw);
        assert_eq!(
            host.await.unwrap(),
            1,
            "the visible price must have prompted"
        );
    }

    /// The tools accept a bare "e1" as well as "@e1"; the gate must resolve
    /// the element name for both spellings or a page can steer the model
    /// past it with "click ref e1".
    #[tokio::test]
    async fn bare_refs_are_resolved_before_pattern_checks() {
        for (tool, input, name) in [
            ("browser_click", json!({"ref": "e1"}), "Delete account"),
            (
                "browser_fill",
                json!({"ref": "e1", "value": "4111"}),
                "Card number",
            ),
        ] {
            let (tx, mut rx) = mpsc::channel::<ApprovalPrompt>(8);
            let session = Arc::new(tokio::sync::Mutex::new(
                crate::browser::BrowserSession::default(),
            ));
            session.lock().await.set_refs_with_names(
                std::collections::HashMap::from([("@e1".to_string(), 1i64)]),
                std::collections::HashMap::from([("@e1".to_string(), name.to_string())]),
            );
            let mw = ApprovalGateMiddleware::new(
                ApprovalGate::default(),
                BrowsePolicy::Pattern,
                Arc::new(tokio::sync::Mutex::new("https://app.example/home".into())),
                tx,
                Arc::new(AtomicU32::new(0)),
                false,
            )
            .with_browser_session(Some(session));
            let host = tokio::spawn(async move {
                let mut n = 0;
                while let Some(p) = rx.recv().await {
                    n += 1;
                    let _ = p.reply.send(true);
                }
                n
            });
            mw.before_tool(tool, &input).await;
            drop(mw);
            assert_eq!(host.await.unwrap(), 1, "{tool} on bare ref must prompt");
        }
    }

    /// Minimal CDP endpoint: answers every command, and reports `href` as
    /// the page location (what Chrome shows after following a redirect).
    async fn fake_cdp(href: &'static str) -> String {
        fake_cdp_focused(href, "").await
    }

    /// As `fake_cdp`, with `focused` as the label of the control that has
    /// keyboard focus.
    async fn fake_cdp_focused(href: &'static str, focused: &'static str) -> String {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
            while let Some(Ok(Message::Text(t))) = ws.next().await {
                let cmd: serde_json::Value = serde_json::from_str(&t).unwrap();
                let expr = cmd["params"]["expression"].as_str().unwrap_or("");
                let result = if cmd["method"] == "Runtime.evaluate" {
                    let value = if expr.contains("activeElement") {
                        focused
                    } else {
                        href
                    };
                    json!({"result": {"type": "string", "value": value}})
                } else {
                    json!({})
                };
                let reply = json!({"id": cmd["id"], "result": result}).to_string();
                if ws.send(Message::Text(reply.into())).await.is_err() {
                    break;
                }
            }
        });
        format!("ws://{addr}")
    }

    /// browser_navigate recorded the requested URL, so after a redirect to
    /// an OAuth consent page the URL patterns matched the stale app URL and
    /// a click on "Authorize octocat" (no button pattern) went through.
    #[tokio::test]
    async fn url_patterns_see_the_page_after_a_redirect() {
        let consent = "https://github.com/login/oauth/authorize?client_id=x";
        let session = Arc::new(tokio::sync::Mutex::new(
            crate::browser::BrowserSession::default(),
        ));
        {
            let mut s = session.lock().await;
            s.connect(&fake_cdp(consent).await).await.unwrap();
            s.current_url = "https://app.example/login".into();
            s.set_refs_with_names(
                std::collections::HashMap::from([("@e1".to_string(), 1i64)]),
                std::collections::HashMap::from([(
                    "@e1".to_string(),
                    "Authorize octocat".to_string(),
                )]),
            );
        }
        let shared = Arc::new(tokio::sync::Mutex::new(
            "https://app.example/login".to_string(),
        ));
        let (tx, mut rx) = mpsc::channel::<ApprovalPrompt>(8);
        let mw = ApprovalGateMiddleware::new(
            ApprovalGate::default(),
            BrowsePolicy::Pattern,
            shared.clone(),
            tx,
            Arc::new(AtomicU32::new(0)),
            false,
        )
        .with_browser_session(Some(session.clone()));
        let host = tokio::spawn(async move {
            let mut n = 0;
            while let Some(p) = rx.recv().await {
                n += 1;
                let _ = p.reply.send(true);
            }
            n
        });
        mw.before_tool("browser_click", &json!({"ref": "@e1"}))
            .await;
        drop(mw);
        assert_eq!(host.await.unwrap(), 1, "the consent URL must prompt");
        assert_eq!(*shared.lock().await, consent);
        assert_eq!(session.lock().await.current_url, consent);
    }

    /// Middleware on a live (fake) page whose focused control is `focused`,
    /// with a host that answers every prompt with `approve`.
    async fn keyboard_middleware(
        focused: &'static str,
        approve: bool,
    ) -> (ApprovalGateMiddleware, tokio::task::JoinHandle<Vec<String>>) {
        let session = Arc::new(tokio::sync::Mutex::new(
            crate::browser::BrowserSession::default(),
        ));
        {
            let mut s = session.lock().await;
            s.connect(&fake_cdp_focused("https://github.com/o/r/settings", focused).await)
                .await
                .unwrap();
            s.set_refs_with_names(
                std::collections::HashMap::from([("@e1".to_string(), 1i64)]),
                std::collections::HashMap::from([(
                    "@e1".to_string(),
                    "Delete this repository".to_string(),
                )]),
            );
        }
        let (tx, mut rx) = mpsc::channel::<ApprovalPrompt>(8);
        let mw = ApprovalGateMiddleware::new(
            ApprovalGate::default(),
            BrowsePolicy::Pattern,
            Arc::new(tokio::sync::Mutex::new(
                "https://github.com/o/r/settings".into(),
            )),
            tx,
            Arc::new(AtomicU32::new(0)),
            false,
        )
        .with_browser_session(Some(session));
        let host = tokio::spawn(async move {
            let mut seen = Vec::new();
            while let Some(p) = rx.recv().await {
                seen.push(p.target_text.clone());
                let _ = p.reply.send(approve);
            }
            seen
        });
        (mw, host)
    }

    /// press_key names no element, so Enter or Space on a focused
    /// "Delete this repository" button (or Enter in the confirm field of its
    /// form) was matched against an empty label and went through.
    #[tokio::test]
    async fn enter_or_space_on_a_focused_destructive_button_is_gated() {
        for key in ["Enter", "space", " "] {
            let (mw, host) = keyboard_middleware("Delete this repository", true).await;
            mw.before_tool("browser_press_key", &json!({"key": key}))
                .await;
            drop(mw);
            assert_eq!(
                host.await.unwrap(),
                vec!["Delete this repository".to_string()],
                "{key:?} must prompt with the focused control's label"
            );
        }
        let (mw, host) = keyboard_middleware("", true).await;
        mw.before_tool("browser_press_key", &json!({"key": "Enter"}))
            .await;
        drop(mw);
        assert!(host.await.unwrap().is_empty(), "nothing focused: no prompt");
    }

    /// Denying the click and then pressing Enter on the same button is the
    /// same action twice, so the session ends instead of starting a fresh
    /// denial counter for the keyboard route.
    #[tokio::test]
    async fn a_denied_click_retried_as_enter_counts_as_the_second_denial() {
        let (mw, host) = keyboard_middleware("Delete this repository", false).await;
        assert!(matches!(
            mw.before_tool("browser_click", &json!({"ref": "@e1"}))
                .await,
            MiddlewareVerdict::Deny { .. }
        ));
        assert!(!mw.is_user_denied() && !mw.should_stop());
        assert!(matches!(
            mw.before_tool("browser_press_key", &json!({"key": "Enter"}))
                .await,
            MiddlewareVerdict::Deny { .. }
        ));
        assert!(mw.is_user_denied(), "second denial must end the session");
        assert!(mw.should_stop(), "the engine must stop with it");
        drop(mw);
        assert_eq!(host.await.unwrap().len(), 2);
    }
}

#[cfg(test)]
mod voice_reply_tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;
    use tokio::time::Instant;

    /// Records for `window`, then spends `transcribe` turning it into text.
    async fn record(window: Duration, transcribe: Duration) {
        tokio::time::sleep(window + transcribe).await;
    }

    /// A yes spoken late in the prompt is transcribed after the 60 s
    /// keyboard timeout; that timeout used to win and deny.
    #[tokio::test(start_paused = true)]
    async fn a_spoken_yes_late_in_the_window_approves() {
        let (_tx, rx) = oneshot::channel();
        let start = Instant::now();
        let deadline = start + APPROVAL_WINDOW;
        let answer = await_key_or_voice(rx, deadline, |window| async move {
            let opened = start.elapsed();
            record(window, Duration::from_secs(20)).await;
            Some(opened >= Duration::from_secs(45))
        })
        .await;
        assert_eq!(answer, Some(true));
        assert!(start.elapsed() > APPROVAL_WINDOW, "{:?}", start.elapsed());
    }

    #[tokio::test(start_paused = true)]
    async fn a_spoken_yes_is_acted_on_within_one_short_window() {
        let (_tx, rx) = oneshot::channel();
        let start = Instant::now();
        let answer = await_key_or_voice(rx, start + APPROVAL_WINDOW, |window| async move {
            record(window, Duration::from_secs(2)).await;
            Some(true)
        })
        .await;
        assert_eq!(answer, Some(true));
        assert_eq!(
            start.elapsed(),
            VOICE_LISTEN_WINDOW + Duration::from_secs(2)
        );
    }

    /// With no recorder the voice side gave up at once and that denied the
    /// action before the user could press a key.
    #[tokio::test(start_paused = true)]
    async fn no_recorder_leaves_the_keyboard_to_answer() {
        for reply in [true, false] {
            let (tx, rx) = oneshot::channel();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(30)).await;
                let _ = tx.send(reply);
            });
            let answer =
                await_key_or_voice(rx, Instant::now() + APPROVAL_WINDOW, |_| async { None }).await;
            assert_eq!(answer, Some(reply));
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_key_answers_while_the_mic_is_listening() {
        let (tx, rx) = oneshot::channel();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(10)).await;
            let _ = tx.send(false);
        });
        let start = Instant::now();
        let answer = await_key_or_voice(rx, start + APPROVAL_WINDOW, |window| async move {
            record(window, Duration::from_secs(3)).await;
            Some(false)
        })
        .await;
        assert_eq!(answer, Some(false));
        assert_eq!(start.elapsed(), Duration::from_secs(10));
    }

    #[tokio::test(start_paused = true)]
    async fn silence_times_out_and_opens_no_window_after_the_deadline() {
        let (_tx, rx) = oneshot::channel::<bool>();
        let start = Instant::now();
        let deadline = start + APPROVAL_WINDOW;
        let late = AtomicUsize::new(0);
        let answer = await_key_or_voice(rx, deadline, |window| {
            if Instant::now() + window > deadline {
                late.fetch_add(1, Ordering::SeqCst);
            }
            async move {
                record(window, Duration::from_secs(1)).await;
                Some(false)
            }
        })
        .await;
        assert_eq!(answer, None);
        assert_eq!(late.load(Ordering::SeqCst), 0);
        // The last window closes at the deadline; its transcription is the
        // only wait past it.
        assert_eq!(start.elapsed(), APPROVAL_WINDOW + Duration::from_secs(1));
    }
}
