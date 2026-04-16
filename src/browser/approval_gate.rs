//! Destructive-action approval gate for autonomous browser agent.
//!
//! Pattern-matches tool calls against URL paths, button text, form-field
//! signals, visible prices, and user-defined extension patterns. Read-only
//! tools always pass; everything else is checked against compiled RegexSets.

use regex::{Regex, RegexSet};

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
