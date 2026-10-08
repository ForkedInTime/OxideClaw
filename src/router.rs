/// Optional model router: each turn goes to a tier (low, mid, high,
/// super-high), and each tier is any model string the backends accept, so a
/// local Ollama model can take the simple turns and Claude the hard ones.
///
/// The tier comes from a keyword and length heuristic, or, opt-in, from the
/// low tier answering with a one-word label (the heuristic answers when it
/// times out or says anything else). A turn the cheap tier fails (an API
/// error that is not auth or rate limiting, malformed tool calls twice in a
/// row, the loop detector, a context too big for its window) moves once to
/// the next tier up, within the `/budget` cap. Tiers without a credential or
/// with an unreachable host are skipped for the session.
use crate::api::ApiBackend;
use crate::api::types::{
    ContentBlock, Message, MessagesRequest, Role, SystemContent, ToolDefinition, Usage,
};
use crate::config::Config;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tracing::debug;

// ─── Complexity levels ──────────────────────────────────────────────────────

/// Task complexity determines which model tier handles the request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Complexity {
    /// Trivial: one-liner answers, typo fixes, simple explanations
    Low,
    /// Moderate: small code changes, test writing, focused debugging
    Medium,
    /// Hard: refactoring, architecture, multi-file changes, complex debugging
    High,
    /// Massive: full codebase analysis, huge multi-file refactors, needs 1M context
    SuperHigh,
}

impl std::fmt::Display for Complexity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Complexity::Low => write!(f, "low"),
            Complexity::Medium => write!(f, "medium"),
            Complexity::High => write!(f, "high"),
            Complexity::SuperHigh => write!(f, "super-high"),
        }
    }
}

// ─── Router configuration ───────────────────────────────────────────────────

impl Complexity {
    /// Every tier, cheapest first.
    pub const ALL: [Complexity; 4] = [
        Complexity::Low,
        Complexity::Medium,
        Complexity::High,
        Complexity::SuperHigh,
    ];

    /// A tier name as settings.json, `/router <tier>` and the classifier
    /// write it.
    pub fn parse(label: &str) -> Option<Self> {
        match label.trim().to_ascii_lowercase().as_str() {
            "low" => Some(Complexity::Low),
            "mid" | "medium" => Some(Complexity::Medium),
            "high" => Some(Complexity::High),
            "super-high" | "superhigh" | "super_high" => Some(Complexity::SuperHigh),
            _ => None,
        }
    }

    fn rank(self) -> usize {
        self as usize
    }
}

/// How a turn's tier is picked (`router.classifier` in settings.json).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Classifier {
    /// Keywords and length; free and instant.
    #[default]
    Heuristic,
    /// Ask the low tier for a one-word label; the heuristic answers when it
    /// times out or says anything else.
    Model,
}

impl Classifier {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "heuristic" => Some(Classifier::Heuristic),
            "model" => Some(Classifier::Model),
            _ => None,
        }
    }
}

/// Tiers found unusable (no credential, host not reachable) or usable,
/// by model, for the rest of the session. Shared by every clone of the
/// router, so each skipped tier is announced once. A usable OpenAI-compatible
/// tier keeps its client: the client carries session state (the Responses
/// reasoning store, summaries or tools found unsupported), which a client
/// built fresh every prompt would lose.
#[derive(Clone, Default)]
pub struct TierHealth(Arc<Mutex<HashMap<String, Verdict>>>);

/// Usable (with the client kept for it, if any) or why not.
type Verdict = Result<Option<ApiBackend>, String>;

impl std::fmt::Debug for TierHealth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let verdicts: Vec<(String, Result<(), String>)> = match self.0.lock() {
            Ok(m) => m
                .iter()
                .map(|(k, v)| (k.clone(), v.as_ref().map(|_| ()).map_err(Clone::clone)))
                .collect(),
            Err(_) => Vec::new(),
        };
        f.debug_tuple("TierHealth").field(&verdicts).finish()
    }
}

impl TierHealth {
    fn get(&self, model: &str) -> Option<Verdict> {
        self.0.lock().ok()?.get(model).cloned()
    }

    pub(crate) fn set(&self, model: &str, health: Result<(), String>) {
        self.store(model, health.map(|()| None));
    }

    fn store(&self, model: &str, health: Verdict) {
        if let Ok(mut m) = self.0.lock() {
            m.insert(model.to_string(), health);
        }
    }

    /// Forget every verdict, so `/router on` looks again (Ollama started
    /// since, a key exported in another shell does not count).
    pub fn reset(&self) {
        if let Ok(mut m) = self.0.lock() {
            m.clear();
        }
    }

    /// Models skipped so far, with the reason.
    pub fn skipped(&self) -> Vec<(String, String)> {
        let Ok(m) = self.0.lock() else {
            return Vec::new();
        };
        let mut v: Vec<(String, String)> = m
            .iter()
            .filter_map(|(k, h)| h.as_ref().err().map(|why| (k.clone(), why.clone())))
            .collect();
        v.sort();
        v
    }
}

/// Model assignments per complexity tier.
#[derive(Debug, Clone)]
pub struct RouterConfig {
    /// Model for low-complexity tasks (default: claude-haiku-4-5, or the
    /// session model when that is not a Claude model)
    pub low_model: String,
    /// Model for medium-complexity tasks (default: claude-sonnet-5, or the
    /// session model when that is not a Claude model)
    pub medium_model: String,
    /// Model for high-complexity tasks (default: whatever the user configured)
    pub high_model: String,
    /// Model for super-high tasks needing 1M context (default: claude-opus-5,
    /// or the session model when that is not a Claude model)
    pub super_high_model: String,
    /// Whether the router is enabled
    pub enabled: bool,
    /// Heuristic (default) or the low tier as classifier.
    pub classifier: Classifier,
    /// How long the model classifier may take before the heuristic answers.
    pub classifier_timeout: Duration,
    pub health: TierHealth,
    /// Tiers set by settings.json or `/router <tier>`, by rank: these keep
    /// their model when the session model changes.
    pinned: [bool; 4],
}

impl Default for RouterConfig {
    fn default() -> Self {
        Self {
            low_model: "claude-haiku-4-5".into(),
            medium_model: "claude-sonnet-5".into(),
            high_model: String::new(), // filled from config.model at runtime
            super_high_model: "claude-opus-5".into(),
            enabled: false,
            classifier: Classifier::Heuristic,
            classifier_timeout: CLASSIFIER_TIMEOUT,
            health: TierHealth::default(),
            pinned: [false; 4],
        }
    }
}

/// The model a tier gets when nothing sets one. The Claude defaults apply
/// to Claude sessions only: a local or OpenAI-compatible session keeps its
/// own model on every unset tier, so turning the router on sends prompts to
/// no provider the user did not name.
pub fn default_tier_model(tier: Complexity, session_model: &str) -> String {
    let defaults = RouterConfig::default();
    if tier == Complexity::High
        || crate::api::is_ollama_model(session_model)
        || crate::api::is_openai_compat_model(session_model)
    {
        return session_model.to_string();
    }
    defaults.model_for(tier).to_string()
}

impl RouterConfig {
    /// Create a new router config with every tier at its default for the
    /// session model `user_model` (see [`default_tier_model`]).
    pub fn new(user_model: &str) -> Self {
        let mut r = Self::default();
        for tier in Complexity::ALL {
            *r.model_mut(tier) = default_tier_model(tier, user_model);
        }
        r
    }

    /// The router as settings.json and the defaults describe it: tiers the
    /// settings leave out keep their default for the session model.
    pub fn from_config(config: &Config) -> Self {
        let mut r = Self::new(&config.model);
        r.enabled = config.router_enabled;
        r.classifier = config.router_classifier;
        for (tier, model) in [
            (Complexity::Low, &config.router_low_model),
            (Complexity::Medium, &config.router_medium_model),
            (Complexity::High, &config.router_high_model),
            (Complexity::SuperHigh, &config.router_super_high_model),
        ] {
            if let Some(m) = model {
                r.set_model(tier, m.clone());
            }
        }
        r
    }

    /// Settings moved router fields away from `before` (/trust, /reload):
    /// take those fields from `config` and leave the rest as this session
    /// set them, so a `/router off` or `/router <tier>` is not undone by a
    /// change that never touched it. Tier health is kept.
    pub fn reapply_settings(&mut self, config: &Config, before: &crate::config::RouterFingerprint) {
        let after = config.router_fingerprint();
        let fresh = Self::from_config(config);
        if after.0 != before.0 {
            self.enabled = fresh.enabled;
        }
        if after.1 != before.1 {
            self.classifier = fresh.classifier;
        }
        for (tier, moved) in [
            (Complexity::Low, after.2 != before.2),
            (Complexity::Medium, after.3 != before.3),
            (Complexity::High, after.4 != before.4),
            (Complexity::SuperHigh, after.5 != before.5),
        ] {
            if moved {
                *self.model_mut(tier) = fresh.model_for(tier).to_string();
                self.pinned[tier.rank()] = fresh.pinned[tier.rank()];
            }
        }
    }

    /// Tier models that would take a local session's prompts off this
    /// machine: empty unless the router is on and `session_model` runs on
    /// Ollama or LM Studio.
    pub fn tiers_off_machine(&self, session_model: &str) -> Vec<String> {
        let local = |m: &str| m.starts_with("ollama:") || m.starts_with("lmstudio:");
        if !self.enabled || !local(session_model) {
            return Vec::new();
        }
        let mut out: Vec<String> = Vec::new();
        for tier in Complexity::ALL {
            let m = self.model_for(tier);
            if !m.is_empty() && !local(m) && !out.iter().any(|o| o == m) {
                out.push(m.to_string());
            }
        }
        out
    }

    /// Select the model for a given complexity level.
    pub fn model_for(&self, complexity: Complexity) -> &str {
        match complexity {
            Complexity::Low => &self.low_model,
            Complexity::Medium => &self.medium_model,
            Complexity::High => &self.high_model,
            Complexity::SuperHigh => &self.super_high_model,
        }
    }

    fn model_mut(&mut self, complexity: Complexity) -> &mut String {
        match complexity {
            Complexity::Low => &mut self.low_model,
            Complexity::Medium => &mut self.medium_model,
            Complexity::High => &mut self.high_model,
            Complexity::SuperHigh => &mut self.super_high_model,
        }
    }

    /// Whether a turn can go to `tier`: not skipped this session and, if not
    /// checked yet, its credential is there. A host not checked yet counts
    /// as up.
    pub fn may_route_to(&self, config: &Config, tier: Complexity) -> bool {
        let model = self.model_for(tier);
        if model.is_empty() {
            return false;
        }
        match self.health.get(model) {
            Some(verdict) => verdict.is_ok(),
            None => model == config.model || config.has_credential_for(model),
        }
    }

    /// Pin `complexity` to `model`: it no longer follows the session model.
    pub fn set_model(&mut self, complexity: Complexity, model: String) {
        self.pinned[complexity.rank()] = true;
        *self.model_mut(complexity) = model;
    }

    /// The session model changed from `old` to `new` (`/model`): every tier
    /// still on its default for `old` moves to its default for `new`. Tiers
    /// set in settings.json or with `/router <tier>` stay.
    pub fn follow_session_model(&mut self, old: &str, new: &str) {
        for tier in Complexity::ALL {
            if !self.pinned[tier.rank()] && self.model_for(tier) == default_tier_model(tier, old) {
                *self.model_mut(tier) = default_tier_model(tier, new);
            }
        }
    }
}

/// Whether the router starts on: an explicit `enabled` wins; otherwise it
/// is on once at least two tiers are configured. One tier or none, with no
/// explicit switch, leaves it off as before.
pub fn starts_enabled(explicit: Option<bool>, configured_tiers: usize) -> bool {
    explicit.unwrap_or(configured_tiers >= 2)
}

// ─── Complexity detection ───────────────────────────────────────────────────

/// Keywords / patterns that signal super-high complexity (needs 1M context / Opus).
const SUPER_HIGH_SIGNALS: &[&str] = &[
    "entire codebase",
    "all files",
    "every file",
    "whole project",
    "whole repo",
    "full codebase",
    "full project",
    "analyze everything",
    "review everything",
    "complete rewrite",
    "full rewrite",
    "full audit",
    "codebase-wide",
    "massive refactor",
    "large-scale",
    "cross-cutting",
];

/// Keywords / patterns that signal high complexity.
const HIGH_SIGNALS: &[&str] = &[
    "refactor",
    "rewrite",
    "architect",
    "redesign",
    "migrate",
    "race condition",
    "deadlock",
    "concurrency",
    "parallel",
    "security",
    "vulnerability",
    "exploit",
    "optimize",
    "performance",
    "benchmark",
    "implement",
    "build",
    "create a",
    "design",
    "multi-file",
    "across files",
    "debug",
    "investigate",
    "diagnose",
    "root cause",
    "review",
    "audit",
    "analyze",
];

/// Keywords / patterns that signal low complexity.
/// NOTE: multi-word phrases only to avoid false positives from substring matching.
/// Single-word signals are checked with word-boundary matching below.
const LOW_SIGNALS: &[&str] = &[
    "explain",
    "what is",
    "what does",
    "what's",
    "how does",
    "rename",
    "typo",
    "spelling",
    "format",
    "lint",
    "add a comment",
    "add comment",
    "docstring",
    "show me",
    "go ahead",
    "do it",
    "thank you",
];

/// Single-word low-complexity signals (matched as whole words).
const LOW_WORDS: &[&str] = &[
    "yes", "no", "ok", "sure", "thanks", "hello", "hi", "hey", "help", "version", "list", "print",
    "proceed", "continue",
];

/// Analyze a user message and determine its complexity.
pub fn detect_complexity(input: &str) -> Complexity {
    let lower = input.to_lowercase();
    let word_count = input.split_whitespace().count();

    // Very short messages are usually low complexity (confirmations, simple questions)
    if word_count <= 4 {
        // Unless they contain a high signal word
        for signal in HIGH_SIGNALS {
            if lower.contains(signal) {
                return Complexity::Medium;
            }
        }
        return Complexity::Low;
    }

    // Score-based detection
    let mut score: i32 = 0;

    // High-complexity signals (strong)
    for signal in HIGH_SIGNALS {
        if lower.contains(signal) {
            score += 4;
        }
    }

    // Low-complexity signals (moderate pull-down)
    for signal in LOW_SIGNALS {
        if lower.contains(signal) {
            score -= 2;
        }
    }
    // Single-word low signals (whole-word match to avoid "no" matching inside "tokens")
    let words: Vec<&str> = lower.split_whitespace().collect();
    for lw in LOW_WORDS {
        if words.iter().any(|w| w == lw) {
            score -= 2;
        }
    }

    // Length heuristic: longer messages tend to be more complex
    if word_count > 50 {
        score += 3;
    } else if word_count > 20 {
        score += 1;
    }

    // Code blocks suggest implementation work
    if input.contains("```") {
        score += 2;
    }

    // Multiple questions suggest complexity
    let question_marks = input.chars().filter(|c| *c == '?').count();
    if question_marks >= 2 {
        score += 1;
    }

    // File paths suggest code work
    if input.contains(".rs")
        || input.contains(".ts")
        || input.contains(".py")
        || input.contains(".go")
        || input.contains(".js")
        || input.contains("src/")
        || input.contains("./")
    {
        score += 1;
    }

    // Action verbs that suggest implementation (not just reading)
    for verb in &[
        "add", "write", "fix", "change", "update", "modify", "remove", "delete", "test",
    ] {
        if lower.starts_with(verb) || lower.contains(&format!(" {verb} ")) {
            score += 1;
            break;
        }
    }

    // Super-high signals: massive scope tasks needing 1M context
    let mut super_high = false;
    for signal in SUPER_HIGH_SIGNALS {
        if lower.contains(signal) {
            super_high = true;
            score += 6;
        }
    }

    debug!("Complexity score for input ({word_count} words): {score}");

    if super_high {
        Complexity::SuperHigh
    } else if score >= 4 {
        Complexity::High
    } else if score >= 1 {
        Complexity::Medium
    } else {
        Complexity::Low
    }
}

// ─── Phase enum ─────────────────────────────────────────────────────────────

/// Conversational phase — what kind of work the user is asking for right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Reading, understanding, searching the codebase
    Research,
    /// Designing, planning, strategising
    Plan,
    /// Writing or modifying code
    Edit,
    /// Checking, verifying, auditing
    Review,
    /// Couldn't determine a phase
    Default,
}

impl std::fmt::Display for Phase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Phase::Research => write!(f, "research"),
            Phase::Plan => write!(f, "plan"),
            Phase::Edit => write!(f, "edit"),
            Phase::Review => write!(f, "review"),
            Phase::Default => write!(f, "default"),
        }
    }
}

// ─── Phase detection signals ─────────────────────────────────────────────────

const RESEARCH_SIGNALS: &[&str] = &[
    "what does",
    "how does",
    "find",
    "search",
    "explain",
    "show me",
    "where is",
    "list",
    "read",
    "what is",
];

const PLAN_SIGNALS: &[&str] = &[
    "plan",
    "design",
    "approach",
    "strategy",
    "how should we",
    "architecture",
    "propose",
    "outline",
];

// NOTE: single words here are matched as whole words (space-delimited) to
// avoid "implement" matching inside "implementation", etc.
const EDIT_SIGNALS: &[&str] = &[
    "implement",
    "add",
    "fix",
    "change",
    "refactor",
    "write",
    "create",
    "update",
    "modify",
    "build",
];

const REVIEW_SIGNALS: &[&str] = &[
    "review",
    "check",
    "verify",
    "test",
    "does this look",
    "is this correct",
    "audit",
    "validate",
];

/// Match a signal phrase against a lowercased input string.
/// Single-word signals are matched as whole words to avoid false positives
/// (e.g. "implement" inside "implementation").
fn signal_matches(input: &str, signal: &str) -> bool {
    if !input.contains(signal) {
        return false;
    }
    // Multi-word signals: substring match is fine.
    if signal.contains(' ') {
        return true;
    }
    // Single-word signal: require word boundaries (space, start, or end).
    for (i, _) in input.match_indices(signal) {
        let before_ok = i == 0 || !input.as_bytes()[i - 1].is_ascii_alphanumeric();
        let after = i + signal.len();
        let after_ok = after >= input.len() || !input.as_bytes()[after].is_ascii_alphanumeric();
        if before_ok && after_ok {
            return true;
        }
    }
    false
}

/// Detect the conversational phase of the user's request.
///
/// Scores each phase by the number of matching signals.  If the top score is
/// ≥ 1 **and** that phase is the sole winner (no tie), it is returned.
/// Ties or all-zero inputs return `Phase::Default`.
pub fn detect_phase(input: &str) -> Phase {
    let lower = input.to_lowercase();

    let score = |signals: &[&str]| -> usize {
        signals
            .iter()
            .filter(|&&s| signal_matches(&lower, s))
            .count()
    };

    let research = score(RESEARCH_SIGNALS);
    let plan = score(PLAN_SIGNALS);
    let edit = score(EDIT_SIGNALS);
    let review = score(REVIEW_SIGNALS);

    let max = research.max(plan).max(edit).max(review);

    if max == 0 {
        return Phase::Default;
    }

    // Unique winner required — ties yield Default
    let winners: usize = [research, plan, edit, review]
        .iter()
        .filter(|&&s| s == max)
        .count();
    if winners > 1 {
        return Phase::Default;
    }

    if research == max {
        Phase::Research
    } else if plan == max {
        Phase::Plan
    } else if edit == max {
        Phase::Edit
    } else {
        Phase::Review
    }
}

// ─── Phase router configuration ──────────────────────────────────────────────

/// Model assignments per conversational phase.
#[derive(Debug, Clone)]
pub struct PhaseRouterConfig {
    /// Whether phase-based routing is active (default: false)
    pub enabled: bool,
    pub research_model: String,
    pub plan_model: String,
    pub edit_model: String,
    pub review_model: String,
    pub default_model: String,
}

impl Default for PhaseRouterConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            research_model: "claude-haiku-4-5".into(),
            plan_model: "claude-sonnet-5".into(),
            edit_model: "claude-sonnet-5".into(),
            review_model: "claude-opus-5".into(),
            default_model: "claude-sonnet-5".into(),
        }
    }
}

impl PhaseRouterConfig {
    /// Select the model for a given phase.
    pub fn model_for(&self, phase: Phase) -> &str {
        match phase {
            Phase::Research => &self.research_model,
            Phase::Plan => &self.plan_model,
            Phase::Edit => &self.edit_model,
            Phase::Review => &self.review_model,
            Phase::Default => &self.default_model,
        }
    }
}

// ─── Routing a turn ─────────────────────────────────────────────────────────

/// How long the model classifier may take before the heuristic answers.
pub const CLASSIFIER_TIMEOUT: Duration = Duration::from_secs(3);

/// How long a self-hosted tier's host (Ollama, LM Studio, `openai-compat:`)
/// gets to accept a connection before the tier is skipped.
const REACH_TIMEOUT: Duration = Duration::from_millis(1500);

/// Room for the label and nothing else; a model that writes more is
/// answering something other than the question.
const CLASSIFIER_MAX_TOKENS: u32 = 16;

/// The cap for a classifier that may think first (an OpenAI reasoning
/// model, a thinking model behind Ollama or Chat Completions, or a Claude
/// model whose thinking cannot be switched off): reasoning
/// tokens count against the cap, and 16 is spent before any label.
const CLASSIFIER_REASONING_MAX_TOKENS: u32 = 1024;

/// The prompt the classifier sees, cut to this many characters.
const CLASSIFIER_PROMPT_CHARS: usize = 4000;

const CLASSIFIER_SYSTEM: &str = "You route requests to a coding assistant to a model tier. \
Reply with exactly one word: low, mid, high or super-high. \
low: questions, explanations, confirmations, renames, typo fixes. \
mid: a small focused code change, one test, a contained bug fix. \
high: multi-file changes, refactors, architecture, hard debugging, security work. \
super-high: work across a whole codebase that needs a very large context. \
Write nothing else.";

/// The model and client a turn runs on.
pub struct Route {
    pub tier: Complexity,
    pub model: String,
    pub client: ApiBackend,
    /// Why this tier: shown in the transcript and sent as the SDK's
    /// `model/routed` reason.
    pub reason: String,
}

impl Route {
    /// The one dim line the frontends show.
    pub fn line(&self) -> String {
        format!("Router: {} → {} ({})", self.tier, self.model, self.reason)
    }
}

/// What [`RouterConfig::route`] decided.
#[derive(Default)]
pub struct RouteOutcome {
    /// None when no tier is usable: the turn stays on the session model.
    pub route: Option<Route>,
    /// Tiers skipped for the first time this session, one line each.
    pub notices: Vec<String>,
    /// The classifier's call, billed like any other: (model, usage).
    pub classifier_usage: Option<(String, Usage)>,
}

/// What made a turn leave its tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    ApiError,
    MalformedToolCalls,
    Loop,
    ContextOverflow,
}

impl std::fmt::Display for Trigger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Trigger::ApiError => "API error",
            Trigger::MalformedToolCalls => "malformed tool calls twice in a row",
            Trigger::Loop => "loop detected",
            Trigger::ContextOverflow => "context too large",
        })
    }
}

pub enum Escalation {
    /// Continue the turn here.
    To(Box<Route>),
    /// A tier up exists, but resending the history there could pass the
    /// `/budget` cap. The line says so.
    OverBudget(String),
    /// Nothing above, already escalated, or not routed.
    None,
}

impl RouterConfig {
    /// Pick the tier for a turn: the classified tier if it is usable and
    /// its window holds `context_tokens`, else the next one up that is,
    /// else the nearest one below. With no usable tier the route is None.
    pub async fn route(
        &self,
        config: &Config,
        session: &ApiBackend,
        prompt: &str,
        context_tokens: u64,
    ) -> RouteOutcome {
        let mut out = RouteOutcome::default();
        let heuristic = detect_complexity(prompt);
        let (wanted, mut reason) = match self.classifier {
            Classifier::Heuristic => (heuristic, "heuristic".to_string()),
            Classifier::Model => {
                match self
                    .usable(config, session, Complexity::Low, &mut out.notices)
                    .await
                {
                    None => (
                        heuristic,
                        "heuristic: the low tier cannot classify".to_string(),
                    ),
                    Some(client) => {
                        let (label, usage) = classify(
                            &client,
                            &self.low_model,
                            prompt,
                            self.classifier_timeout,
                            config.openai_api,
                        )
                        .await;
                        out.classifier_usage = usage.map(|u| (self.low_model.clone(), u));
                        match label {
                            Ok(c) => (c, "classifier".to_string()),
                            Err(why) => (heuristic, format!("heuristic: classifier {why}")),
                        }
                    }
                }
            }
        };

        let above = Complexity::ALL[wanted.rank()..].iter();
        let below = Complexity::ALL[..wanted.rank()].iter().rev();
        let mut moved: Option<String> = None;
        let mut largest: Option<(Complexity, ApiBackend, u64)> = None;
        for &tier in above.chain(below) {
            let model = self.model_for(tier).to_string();
            if model.is_empty() {
                continue;
            }
            let Some(client) = self.usable(config, session, tier, &mut out.notices).await else {
                moved.get_or_insert_with(|| format!("{tier} tier unavailable"));
                continue;
            };
            let window = crate::api::context_window_for_model(&model);
            if fits(context_tokens, window) {
                if tier != wanted
                    && let Some(why) = moved
                {
                    reason.push_str(&format!("; {why}"));
                }
                out.route = Some(Route {
                    tier,
                    model,
                    client,
                    reason,
                });
                return out;
            }
            moved.get_or_insert_with(|| {
                format!(
                    "~{}k tokens of context is too much for the {tier} tier",
                    context_tokens / 1000
                )
            });
            if largest.as_ref().is_none_or(|l| window > l.2) {
                largest = Some((tier, client, window));
            }
        }
        // Nothing holds the history: the largest window does, once compacted.
        if let Some((tier, client, _)) = largest {
            reason.push_str("; no tier's window holds the context, using the largest");
            out.route = Some(Route {
                tier,
                model: self.model_for(tier).to_string(),
                client,
                reason,
            });
        }
        out
    }

    /// A client for `tier`'s model, or None when it has no credential or
    /// its self-hosted host does not answer. Decided once per model per
    /// session; the first skip adds a line to `notices`.
    async fn usable(
        &self,
        config: &Config,
        session: &ApiBackend,
        tier: Complexity,
        notices: &mut Vec<String>,
    ) -> Option<ApiBackend> {
        let model = self.model_for(tier);
        match self.health.get(model) {
            Some(Err(_)) => return None,
            Some(Ok(Some(client))) => return Some(client),
            Some(Ok(None)) => {
                let client = client_for(config, session, model).ok()?;
                self.health.store(model, Ok(cacheable(&client)));
                return Some(client);
            }
            None => {}
        }
        let checked = match client_for(config, session, model) {
            Err(e) => Err(e.to_string().lines().next().unwrap_or_default().to_string()),
            Ok(client) => match unreachable_host(&client, model).await {
                Some(why) => Err(why),
                None => Ok(client),
            },
        };
        match checked {
            Ok(client) => {
                self.health.store(model, Ok(cacheable(&client)));
                Some(client)
            }
            Err(why) => {
                notices.push(format!(
                    "Router: skipping the {tier} tier ({model}) for this session: {why}"
                ));
                self.health.set(model, Err(why));
                None
            }
        }
    }
}

/// The client to keep for later prompts: an OpenAI-compatible one, built for
/// this tier and holding its session state. The session's own Anthropic or
/// Ollama client is taken afresh each time, so a changed login reaches it.
fn cacheable(client: &ApiBackend) -> Option<ApiBackend> {
    matches!(client, ApiBackend::OpenAiCompat(_)).then(|| client.clone())
}

/// Whether a request of `context_tokens` leaves the model room to work:
/// below the point where compaction would start trimming.
fn fits(context_tokens: u64, window: u64) -> bool {
    context_tokens < crate::compact::thresholds(window).1
}

/// The client for `model`: the session's own when it already serves that
/// backend (it carries the session's auth), a new one otherwise. Each
/// OpenAI-compatible provider has its own base URL and key, so those are
/// always built fresh.
pub fn client_for(
    config: &Config,
    session: &ApiBackend,
    model: &str,
) -> anyhow::Result<ApiBackend> {
    let ollama = crate::api::is_ollama_model(model);
    let compat = crate::api::is_openai_compat_model(model);
    let same = match session {
        ApiBackend::Anthropic(_) => !ollama && !compat,
        ApiBackend::Ollama(_) => ollama,
        ApiBackend::OpenAiCompat(_) => false,
    };
    if same {
        return Ok(session.clone());
    }
    config.backend_for(model)
}

/// Why a self-hosted tier cannot be used, if its host refuses or ignores a
/// connection. Named cloud providers are not probed, and neither is a host
/// requests reach through a proxy (`HTTPS_PROXY` and friends, minus
/// `NO_PROXY`): a direct connect says nothing about it. Their failures
/// surface as API errors, which escalate.
async fn unreachable_host(client: &ApiBackend, model: &str) -> Option<String> {
    let base = match client {
        ApiBackend::Ollama(_) => client.ollama_host()?.to_string(),
        ApiBackend::OpenAiCompat(c)
            if crate::api::parse_provider_model(model)
                .is_some_and(|(p, _)| matches!(p.prefix, "lmstudio" | "openai-compat")) =>
        {
            c.base_url.clone()
        }
        _ => return None,
    };
    if crate::api::proxy_applies(&base) {
        return None;
    }
    if crate::api::host_reachable(&base, REACH_TIMEOUT).await {
        None
    } else {
        Some(format!("{base} is not reachable"))
    }
}

/// Ask `model` for a one-word tier label. Err says why the heuristic has to
/// answer instead; the usage is whatever the call was billed.
pub(crate) async fn classify(
    client: &ApiBackend,
    model: &str,
    prompt: &str,
    timeout: Duration,
    api: crate::api::OpenAiApi,
) -> (Result<Complexity, String>, Option<Usage>) {
    // An OpenAI reasoning model thinks at its default effort unless told
    // otherwise, and its reasoning tokens count against the cap: ask for the
    // least it takes and leave room for it. Other non-Claude models may
    // think too (Gemini 2.5, qwen3, deepseek-r1), so they get the room.
    let reasoning = crate::api::openai_compat::responses_reasoning_model(model, api);
    let output_config = reasoning.then(|| {
        let bare = crate::api::parse_provider_model(model).map_or(model, |(_, b)| b);
        crate::api::thinking::OutputConfig {
            effort: crate::api::openai_compat::responses::lowest_effort(bare).into(),
        }
    });
    let mut may_think =
        crate::api::is_ollama_model(model) || crate::api::is_openai_compat_model(model);
    // Claude 5+ and Fable/Mythos think with the field omitted, which spends
    // the 16 tokens before any label: switch thinking off where the model
    // allows it, and where it does not, ask for low effort and leave room.
    let mut output_config = output_config;
    let thinking = if may_think {
        None
    } else {
        use crate::api::thinking;
        let off = thinking::thinking_for(model, Some(0), CLASSIFIER_MAX_TOKENS, None, false);
        if off.is_none() && thinking::supports_adaptive_thinking(model) {
            may_think = true;
            output_config = Some(thinking::OutputConfig {
                effort: "low".into(),
            });
        }
        off
    };
    let request = MessagesRequest {
        model: model.to_string(),
        max_tokens: if may_think {
            CLASSIFIER_REASONING_MAX_TOKENS
        } else {
            CLASSIFIER_MAX_TOKENS
        },
        system: SystemContent::Plain(CLASSIFIER_SYSTEM.into()),
        messages: vec![Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: prompt.chars().take(CLASSIFIER_PROMPT_CHARS).collect(),
            }],
        }],
        tools: Vec::new(),
        stream: None,
        thinking,
        output_config,
        betas: Vec::new(),
        session_id: None,
        explicit_max_tokens: true,
        cache_history: false,
    };
    let response =
        match tokio::time::timeout(timeout, client.messages_stream(request, |_| {})).await {
            Err(_) => {
                return (Err(format!("timed out after {:.0?}", timeout)), None);
            }
            Ok(Err(e)) => {
                let e = e.to_string();
                return (
                    Err(format!("failed ({})", e.lines().next().unwrap_or_default())),
                    None,
                );
            }
            Ok(Ok(r)) => r,
        };
    let answer: String = response
        .content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    // A thinking model may lead with its reasoning in `<think>` tags; the
    // label is the first word after it.
    let visible = match answer.split_once("</think>") {
        Some((_, rest)) => rest,
        None => answer.as_str(),
    };
    let label = visible
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .trim_matches(|c: char| matches!(c, '.' | ',' | ':' | '"' | '\'' | '`' | '*'));
    match Complexity::parse(label) {
        Some(c) => (Ok(c), Some(response.usage)),
        None => {
            let shown: String = answer.trim().chars().take(40).collect();
            (Err(format!("answered {shown:?}")), Some(response.usage))
        }
    }
}

/// A turn's routing: the tier it runs on and whether it already moved up.
pub struct TurnRoute {
    pub router: RouterConfig,
    pub tier: Complexity,
    escalated: bool,
    malformed_streak: u8,
}

impl TurnRoute {
    pub fn new(router: RouterConfig, tier: Complexity) -> Self {
        Self {
            router,
            tier,
            escalated: false,
            malformed_streak: 0,
        }
    }

    /// Record one response's tool calls; true when this one and the one
    /// before both had a malformed call.
    pub fn malformed_twice(&mut self, content: &[ContentBlock], tools: &[ToolDefinition]) -> bool {
        if has_malformed_tool_call(content, tools) {
            self.malformed_streak += 1;
        } else {
            self.malformed_streak = 0;
        }
        self.malformed_streak >= 2
    }

    /// Move the turn one tier up, once per turn: to the first tier above
    /// with a different, usable model (and, for a context overflow, a
    /// larger window), unless the history resent there could pass
    /// `budget_left`. On `To` the caller swaps in the route's client and
    /// model.
    pub async fn escalate(
        &mut self,
        config: &Config,
        session: &ApiBackend,
        context_tokens: u64,
        budget_left: Option<f64>,
        trigger: Trigger,
        notices: &mut Vec<String>,
    ) -> Escalation {
        if self.escalated {
            return Escalation::None;
        }
        let from_model = config.model.as_str();
        let from_window = crate::api::context_window_for_model(from_model);
        for &tier in &Complexity::ALL[self.tier.rank() + 1..] {
            let model = self.router.model_for(tier);
            if model.is_empty() || model == from_model {
                continue;
            }
            if trigger == Trigger::ContextOverflow
                && crate::api::context_window_for_model(model) <= from_window
            {
                continue;
            }
            let Some(client) = self.router.usable(config, session, tier, notices).await else {
                continue;
            };
            // The retry resends the whole history at the new tier's rate.
            let estimate = crate::cost::model_price(model).cost(context_tokens, 0, 0, 0);
            if let Some(left) = budget_left
                && (left <= 0.0 || estimate > left)
            {
                // Asking again after every later failure would repeat the line.
                self.escalated = true;
                return Escalation::OverBudget(format!(
                    "Router: {trigger} on {from_model}; not escalating to {model}: \
                     resending the history (~${estimate:.4}) could pass the /budget cap \
                     (${left:.4} left)."
                ));
            }
            self.escalated = true;
            self.tier = tier;
            self.malformed_streak = 0;
            return Escalation::To(Box::new(Route {
                tier,
                model: model.to_string(),
                client,
                reason: format!("{trigger} on {from_model}, retrying one tier up"),
            }));
        }
        Escalation::None
    }
}

/// Whether an API error says the request or model failed, rather than the
/// account: auth and rate limits fail the same way one tier up.
pub fn escalates_on(err: &str) -> bool {
    let e = err.to_ascii_lowercase();
    ![
        "error 401",
        "error 403",
        "error 429",
        "unauthorized",
        "authentication",
        "invalid x-api-key",
        "invalid api key",
        "incorrect api key",
        "permission_error",
        "rate limit",
        "rate_limit",
        "too many requests",
        "no api key",
        "credential",
        "budget",
    ]
    .iter()
    .any(|k| e.contains(k))
}

/// A tool call the model could not have meant: an unknown tool, input that
/// is not an object, or a required parameter missing.
pub fn has_malformed_tool_call(content: &[ContentBlock], tools: &[ToolDefinition]) -> bool {
    content.iter().any(|b| {
        let ContentBlock::ToolUse { name, input, .. } = b else {
            return false;
        };
        let Some(def) = tools.iter().find(|t| t.name == *name) else {
            return true;
        };
        let Some(obj) = input.as_object() else {
            return true;
        };
        def.input_schema["required"].as_array().is_some_and(|req| {
            req.iter()
                .filter_map(|k| k.as_str())
                .any(|k| !obj.contains_key(k))
        })
    })
}

/// Rough prompt size in tokens (four characters each) for picking a tier
/// whose window holds the history before any request has measured it.
/// Images count as a fixed 1,600.
pub fn estimate_context_tokens(system: &str, messages: &[Message]) -> u64 {
    let chars: usize = messages
        .iter()
        .flat_map(|m| &m.content)
        .map(|b| match b {
            ContentBlock::Text { text } => text.len(),
            ContentBlock::ToolUse { input, .. } => input.to_string().len(),
            ContentBlock::ToolResult { content, .. } => content
                .iter()
                .map(|c| {
                    let crate::api::types::ToolResultContent::Text { text } = c;
                    text.len()
                })
                .sum(),
            ContentBlock::Thinking { thinking, .. } => thinking.len(),
            ContentBlock::RedactedThinking { data } => data.len(),
            ContentBlock::Image { .. } => 6_400,
        })
        .sum();
    ((chars + system.len()) / 4) as u64
}

/// The text of the last user message: the prompt the turn is routed on.
pub fn last_prompt(messages: &[Message]) -> String {
    messages
        .iter()
        .rfind(|m| m.role == Role::User)
        .and_then(|m| {
            m.content.iter().find_map(|b| match b {
                ContentBlock::Text { text } => Some(text.clone()),
                _ => None,
            })
        })
        .unwrap_or_default()
}

/// A stand-in for an OpenAI-compatible chat endpoint (Ollama's
/// `/v1/chat/completions`) that answers per requested model, so one host
/// can serve every tier of a test router.
#[cfg(test)]
pub(crate) mod fake_chat {
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// What the server sends back for a request.
    #[derive(Clone)]
    pub(crate) enum Reply {
        Text(&'static str),
        /// One tool call: name and JSON arguments.
        Tool(&'static str, &'static str),
        /// A tool call whose usage reports this many prompt tokens.
        // Only the TUI's tests use it, and the library build has no TUI.
        #[allow(dead_code)]
        ToolAt(&'static str, &'static str, u64),
        /// An HTTP error with this status and body.
        Status(u16, &'static str),
        /// Accept and never answer.
        Hang,
    }

    fn sse(chunks: &[serde_json::Value]) -> String {
        let mut body: String = chunks.iter().map(|c| format!("data: {c}\n\n")).collect();
        body.push_str("data: [DONE]\n\n");
        format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\n\
             connection: close\r\n\r\n{body}",
            body.len()
        )
    }

    fn render(reply: &Reply) -> Option<String> {
        let prompt_tokens = match reply {
            Reply::ToolAt(_, _, n) => *n,
            _ => 100,
        };
        let usage = serde_json::json!({"prompt_tokens": prompt_tokens, "completion_tokens": 2});
        Some(match reply {
            Reply::Text(t) => sse(&[
                serde_json::json!({"choices":[{"delta":{"content":t},"finish_reason":null}]}),
                serde_json::json!({"choices":[{"delta":{},"finish_reason":"stop"}],"usage":usage}),
            ]),
            Reply::Tool(name, args) | Reply::ToolAt(name, args, _) => sse(&[
                serde_json::json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":name,"arguments":args}}]},"finish_reason":null}]}),
                serde_json::json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}],"usage":usage}),
            ]),
            Reply::Status(code, body) => format!(
                "HTTP/1.1 {code} Error\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\
                 connection: close\r\n\r\n{body}",
                body.len()
            ),
            Reply::Hang => return None,
        })
    }

    /// Serve until the test ends. `reply` picks the answer from the
    /// requested model (without `ollama:`) and how many requests that model
    /// has had before. Returns the base URL and the models asked, in order.
    pub(crate) async fn start(
        reply: impl Fn(&str, usize) -> Reply + Send + Sync + 'static,
    ) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let log = seen.clone();
        let reply = Arc::new(reply);
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let (log, reply) = (log.clone(), reply.clone());
                tokio::spawn(async move {
                    let req = read_request(&mut sock).await;
                    // The Ollama client also asks /api/ps and /api/show for
                    // the served context; those are not model requests.
                    if !req
                        .lines()
                        .next()
                        .unwrap_or("")
                        .contains("/chat/completions")
                    {
                        let _ = sock
                            .write_all(b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\n\r\n")
                            .await;
                        return;
                    }
                    let body = req.split("\r\n\r\n").nth(1).unwrap_or("");
                    let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else {
                        return;
                    };
                    let model = v["model"].as_str().unwrap_or("").to_string();
                    let n = {
                        let mut seen = log.lock().unwrap();
                        let n = seen.iter().filter(|m| **m == model).count();
                        seen.push(model.clone());
                        n
                    };
                    match render(&reply(&model, n)) {
                        Some(resp) => {
                            let _ = sock.write_all(resp.as_bytes()).await;
                            let _ = sock.shutdown().await;
                        }
                        None => std::future::pending::<()>().await,
                    }
                });
            }
        });
        (format!("http://{addr}"), seen)
    }

    async fn read_request(sock: &mut tokio::net::TcpStream) -> String {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 8192];
        loop {
            let n = sock.read(&mut chunk).await.unwrap_or(0);
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
            let text = String::from_utf8_lossy(&buf).to_string();
            if let Some(end) = text.find("\r\n\r\n") {
                let len = text[..end]
                    .lines()
                    .find_map(|l| {
                        l.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                    })
                    .unwrap_or(0);
                if buf.len() >= end + 4 + len {
                    return text;
                }
            }
        }
        String::from_utf8_lossy(&buf).to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::fake_chat::{self, Reply};
    use super::*;

    /// A router whose tiers all live on one fake Ollama host.
    fn ollama_setup(host: &str, low: &str, mid: &str, high: &str) -> (Config, RouterConfig) {
        let config = Config {
            model: format!("ollama:{high}"),
            ollama_host: host.to_string(),
            api_key: String::new(),
            ..Config::default()
        };
        let mut r = RouterConfig::new(&config.model);
        r.enabled = true;
        r.low_model = format!("ollama:{low}");
        r.medium_model = format!("ollama:{mid}");
        r.super_high_model = format!("ollama:{high}");
        (config, r)
    }

    fn session(config: &Config) -> ApiBackend {
        config.backend_for(&config.model).unwrap()
    }

    #[test]
    fn tier_labels_and_the_on_off_rule() {
        assert_eq!(Complexity::parse("mid"), Some(Complexity::Medium));
        assert_eq!(Complexity::parse(" Medium "), Some(Complexity::Medium));
        assert_eq!(Complexity::parse("super-high"), Some(Complexity::SuperHigh));
        assert_eq!(Complexity::parse("HIGH"), Some(Complexity::High));
        assert_eq!(Complexity::parse("high please"), None);
        assert_eq!(Classifier::parse("Model"), Some(Classifier::Model));
        assert_eq!(Classifier::parse("llm"), None);

        // Off with fewer than two tiers, unless switched on.
        assert!(!starts_enabled(None, 0));
        assert!(!starts_enabled(None, 1));
        assert!(starts_enabled(Some(true), 0));
        // On with two, unless switched off.
        assert!(starts_enabled(None, 2));
        assert!(!starts_enabled(Some(false), 4));
    }

    /// /trust and /reload rebuilt the router from settings whenever a
    /// router setting moved, undoing this session's `/router off` and
    /// `/router <tier>` even though only another tier changed.
    #[test]
    fn a_settings_change_keeps_the_session_choices_it_did_not_touch() {
        let mut config = Config {
            model: "claude-sonnet-5".into(),
            router_enabled: true,
            router_low_model: Some("groq:llama-3.1-8b-instant".into()),
            ..Config::default()
        };
        let mut r = RouterConfig::from_config(&config);
        r.enabled = false;
        r.set_model(Complexity::Medium, "claude-haiku-4-5".into());

        // Trust revoked: the project's low tier goes, nothing else moved.
        let before = config.router_fingerprint();
        config.router_low_model = None;
        r.reapply_settings(&config, &before);
        assert_eq!(r.low_model, "claude-haiku-4-5", "revoked tier dropped");
        assert!(!r.enabled, "/router off survives");
        assert_eq!(r.medium_model, "claude-haiku-4-5", "/router mid survives");

        // A setting that does move the switch still applies.
        let before = config.router_fingerprint();
        config.router_enabled = false;
        r.enabled = true;
        r.reapply_settings(&config, &before);
        assert!(!r.enabled);
    }

    /// A local session with the router on names the tiers that leave the
    /// machine; off, or on a cloud session, there is nothing to say.
    #[test]
    fn a_local_session_names_its_off_machine_tiers() {
        let mut r = RouterConfig::new("ollama:qwen3-coder");
        r.set_model(Complexity::Low, "claude-haiku-4-5".into());
        r.set_model(Complexity::SuperHigh, "claude-haiku-4-5".into());
        assert!(r.tiers_off_machine("ollama:qwen3-coder").is_empty(), "off");
        r.enabled = true;
        assert_eq!(
            r.tiers_off_machine("ollama:qwen3-coder"),
            vec!["claude-haiku-4-5".to_string()]
        );
        assert!(r.tiers_off_machine("claude-sonnet-5").is_empty());
    }

    /// A routed OpenAI-compatible tier got a fresh client every prompt, so
    /// its refused-summary flag and reasoning store were lost each time.
    /// The first usable client is kept and handed out again.
    #[tokio::test]
    async fn an_openai_compatible_tier_keeps_its_client() {
        let (config, mut router) = ollama_setup("http://127.0.0.1:9", "small", "mid", "big");
        router.low_model = "lmstudio:m".into();
        router.health.set("lmstudio:m", Ok(()));
        let session = session(&config);
        let mut notices = Vec::new();
        assert!(matches!(router.health.get("lmstudio:m"), Some(Ok(None))));
        let first = router
            .usable(&config, &session, Complexity::Low, &mut notices)
            .await;
        assert!(matches!(first, Some(ApiBackend::OpenAiCompat(_))));
        assert!(
            matches!(
                router.health.get("lmstudio:m"),
                Some(Ok(Some(ApiBackend::OpenAiCompat(_))))
            ),
            "kept for the next prompt"
        );
        // The session's own Ollama client is not kept.
        router.health.set("ollama:mid", Ok(()));
        let mid = router
            .usable(&config, &session, Complexity::Medium, &mut notices)
            .await;
        assert!(matches!(mid, Some(ApiBackend::Ollama(_))));
        assert!(matches!(router.health.get("ollama:mid"), Some(Ok(None))));
        assert!(notices.is_empty(), "{notices:?}");
    }

    /// The classifier gets 3 s in use; past its timeout the heuristic
    /// answers, and the turn is not held up waiting.
    #[tokio::test]
    async fn a_classifier_that_times_out_falls_back_to_the_heuristic() {
        let (host, seen) = fake_chat::start(|_, _| Reply::Hang).await;
        let (config, mut router) = ollama_setup(&host, "small", "mid", "big");
        router.classifier = Classifier::Model;
        router.classifier_timeout = Duration::from_millis(300);

        let started = std::time::Instant::now();
        let out = router
            .route(
                &config,
                &session(&config),
                "refactor the auth module across files to use JWT",
                0,
            )
            .await;
        assert!(started.elapsed() < Duration::from_secs(2));
        let route = out.route.unwrap();
        assert_eq!(route.tier, Complexity::High, "the heuristic's tier");
        assert!(route.reason.contains("timed out"), "{}", route.reason);
        assert!(out.classifier_usage.is_none());
        assert_eq!(*seen.lock().unwrap(), vec!["small".to_string()]);
    }

    /// A Claude 5 classifier thinks unless told otherwise, and its thinking
    /// spent the 16-token cap before any label: off where the model allows
    /// it, low effort and room to think where it does not.
    #[tokio::test]
    async fn a_claude_classifier_does_not_spend_its_cap_thinking() {
        use crate::query_engine::scripted_api_tests::{serve, sse};
        let label = || {
            sse(
                &[serde_json::json!({"type":"text","text":"low"})],
                "end_turn",
            )
        };
        let models = [
            "claude-haiku-4-5",
            "claude-sonnet-5",
            "claude-sonnet-5-5",
            "claude-opus-5-5",
            "claude-fable-5-1",
        ];
        let (url, seen) = serve(models.iter().map(|_| label()).collect()).await;
        let mut c = crate::api::ClaudeClient::new("sk-ant-test").unwrap();
        c.set_base_url_for_test(url);
        let client = ApiBackend::Anthropic(c);
        for model in models {
            let (tier, _) = classify(
                &client,
                model,
                "yes",
                Duration::from_secs(10),
                crate::api::OpenAiApi::default(),
            )
            .await;
            assert_eq!(tier, Ok(Complexity::Low), "{model}");
        }
        let fields: Vec<serde_json::Value> = seen
            .lock()
            .unwrap()
            .iter()
            .map(|b| {
                let v: serde_json::Value = serde_json::from_str(b).unwrap();
                serde_json::json!([
                    v["max_tokens"],
                    v["thinking"]["type"],
                    v["output_config"]["effort"]
                ])
            })
            .collect();
        use serde_json::json;
        assert_eq!(
            fields,
            [
                json!([16, null, null]),
                json!([16, "disabled", null]),
                json!([16, "between_tools", null]),
                json!([1024, null, "low"]),
                json!([1024, null, "low"]),
            ]
        );
    }

    #[tokio::test]
    async fn the_classifier_label_decides_and_garbage_does_not() {
        let (host, _) = fake_chat::start(|_, n| {
            if n == 0 {
                Reply::Text("high")
            } else {
                Reply::Text("I think this is a medium task.")
            }
        })
        .await;
        let (config, mut router) = ollama_setup(&host, "small", "mid", "big");
        router.classifier = Classifier::Model;
        let client = session(&config);

        let out = router.route(&config, &client, "yes", 0).await;
        let route = out.route.unwrap();
        assert_eq!(route.tier, Complexity::High);
        assert_eq!(route.reason, "classifier");
        // Billed like any other call, at the low tier.
        let (model, usage) = out.classifier_usage.unwrap();
        assert_eq!(model, "ollama:small");
        assert_eq!(usage.output_tokens, 2);

        let out = router.route(&config, &client, "yes", 0).await;
        assert_eq!(out.route.unwrap().tier, Complexity::Low, "heuristic");
        assert!(
            out.classifier_usage.is_some(),
            "a garbage answer is still billed"
        );
    }

    /// `routerEnabled` with no tiers on a local session must not start
    /// sending its prompts to the Claude defaults: every unset tier is the
    /// session model, so nothing is routed anywhere new and nothing is
    /// skipped. Only a tier the user names crosses providers.
    #[tokio::test]
    async fn a_local_session_with_no_tiers_routes_nowhere_new() {
        let (host, seen) = fake_chat::start(|_, _| Reply::Text("ok")).await;
        let config = Config {
            model: "ollama:qwen3-coder".into(),
            ollama_host: host.clone(),
            api_key: String::new(),
            router_enabled: true,
            ..Config::default()
        };
        let router = RouterConfig::from_config(&config);
        assert!(router.enabled);
        for tier in Complexity::ALL {
            assert_eq!(router.model_for(tier), "ollama:qwen3-coder", "{tier}");
        }
        let client = session(&config);
        for prompt in ["yes", "analyze the entire codebase for dead code"] {
            let out = router.route(&config, &client, prompt, 0).await;
            assert_eq!(out.route.unwrap().model, "ollama:qwen3-coder");
            assert!(out.notices.is_empty(), "{:?}", out.notices);
        }
        assert!(seen.lock().unwrap().is_empty(), "no classifier call");

        let compat = RouterConfig::new("groq:llama-3.3-70b-versatile");
        assert_eq!(compat.low_model, "groq:llama-3.3-70b-versatile");
        assert_eq!(compat.super_high_model, "groq:llama-3.3-70b-versatile");

        // A named tier still crosses providers; the rest stay local.
        let named = Config {
            router_medium_model: Some("claude-sonnet-5".into()),
            ..config
        };
        let r = RouterConfig::from_config(&named);
        assert_eq!(r.medium_model, "claude-sonnet-5");
        assert_eq!(r.low_model, "ollama:qwen3-coder");

        // A Claude session keeps the Claude defaults.
        let claude = RouterConfig::new("claude-opus-5");
        assert_eq!(claude.low_model, "claude-haiku-4-5");
        assert_eq!(claude.medium_model, "claude-sonnet-5");
        assert_eq!(claude.high_model, "claude-opus-5");
    }

    /// A tier without its credential is never routed to; the turn takes the
    /// next tier up, and the skip is announced once per session.
    #[tokio::test]
    async fn a_tier_without_a_credential_is_skipped_once() {
        let (host, _) = fake_chat::start(|_, _| Reply::Text("ok")).await;
        let (config, mut router) = ollama_setup(&host, "small", "mid", "big");
        router.low_model = "claude-haiku-4-5".into();
        let client = session(&config);

        let out = router.route(&config, &client, "yes", 0).await;
        let route = out.route.unwrap();
        assert_eq!(route.tier, Complexity::Medium);
        assert_eq!(route.model, "ollama:mid");
        assert!(
            route.reason.contains("low tier unavailable"),
            "{}",
            route.reason
        );
        assert_eq!(out.notices.len(), 1);
        assert!(
            out.notices[0].contains("claude-haiku-4-5"),
            "{:?}",
            out.notices
        );
        assert!(
            out.notices[0].contains("No Anthropic credential"),
            "{:?}",
            out.notices
        );

        let again = router.route(&config, &client, "yes", 0).await;
        assert_eq!(again.route.unwrap().model, "ollama:mid");
        assert!(again.notices.is_empty(), "announced once per session");
        assert_eq!(router.health.skipped().len(), 1);
    }

    /// An Ollama tier whose host refuses connections is skipped like a
    /// missing key.
    #[tokio::test]
    async fn a_tier_on_an_unreachable_host_is_skipped() {
        let config = Config {
            model: "claude-sonnet-5".into(),
            api_key: "sk-ant-test".into(),
            ollama_host: crate::api::ollama::fake_server::closed_port().await,
            ..Config::default()
        };
        let mut router = RouterConfig::new(&config.model);
        router.enabled = true;
        router.low_model = "ollama:qwen3-coder".into();
        let out = router.route(&config, &session(&config), "yes", 0).await;
        let route = out.route.unwrap();
        assert_eq!(route.tier, Complexity::Medium);
        assert_eq!(route.model, "claude-sonnet-5");
        assert!(
            out.notices[0].contains("not reachable"),
            "{:?}",
            out.notices
        );
    }

    #[tokio::test]
    async fn a_history_too_big_for_the_low_tier_goes_up() {
        let (host, _) = fake_chat::start(|_, _| Reply::Text("ok")).await;
        let (config, mut router) = ollama_setup(&host, "gemma2", "mid", "big");
        // gemma2 holds 8k; the others are taken as 200k.
        router.low_model = "ollama:gemma2".into();
        let out = router
            .route(&config, &session(&config), "yes", 50_000)
            .await;
        let route = out.route.unwrap();
        assert_eq!(route.tier, Complexity::Medium);
        assert!(
            route.reason.contains("too much for the low tier"),
            "{}",
            route.reason
        );
    }

    /// The next tier up costs more; a retry that could pass the cap is not
    /// made, and the line says why.
    #[tokio::test]
    async fn escalation_stops_at_the_budget() {
        let (host, _) = fake_chat::start(|_, _| Reply::Text("ok")).await;
        let (mut config, mut router) = ollama_setup(&host, "small", "mid", "big");
        config.api_key = "sk-ant-test".into();
        config.model = "ollama:small".into();
        router.medium_model = "claude-opus-5".into();
        let client = session(&config);
        let mut turn = TurnRoute::new(router, Complexity::Low);
        let mut notices = Vec::new();

        // 100k tokens at Opus input rates is well over a cent.
        match turn
            .escalate(
                &config,
                &client,
                100_000,
                Some(0.01),
                Trigger::ApiError,
                &mut notices,
            )
            .await
        {
            Escalation::OverBudget(line) => {
                assert!(line.contains("/budget"), "{line}");
                assert!(line.contains("claude-opus-5"), "{line}");
            }
            _ => panic!("escalated past the budget"),
        }
        // Once per turn: a later failure does not ask again.
        assert!(matches!(
            turn.escalate(&config, &client, 0, None, Trigger::ApiError, &mut notices)
                .await,
            Escalation::None
        ));

        let (config2, router2) = ollama_setup(&host, "small", "mid", "big");
        let mut config2 = config2;
        config2.model = "ollama:small".into();
        let mut turn = TurnRoute::new(router2, Complexity::Low);
        match turn
            .escalate(
                &config2,
                &session(&config2),
                100_000,
                Some(0.01),
                Trigger::ApiError,
                &mut notices,
            )
            .await
        {
            Escalation::To(r) => {
                assert_eq!(r.model, "ollama:mid");
                assert_eq!(turn.tier, Complexity::Medium);
            }
            _ => panic!("a free local tier is within any budget"),
        }
    }

    #[test]
    fn auth_and_rate_limits_do_not_escalate() {
        for e in [
            "API error 401 Unauthorized: invalid x-api-key",
            "Groq error 429 Too Many Requests: rate limit reached",
            "OpenAI error 403 Forbidden: {}",
        ] {
            assert!(!escalates_on(e), "{e}");
        }
        for e in [
            "Ollama error 500 Internal Server Error: model crashed",
            "API stream error 400 Bad Request: tool_use ids must be unique",
            "Ollama request failed — is Ollama running?",
        ] {
            assert!(escalates_on(e), "{e}");
        }
    }

    #[test]
    fn malformed_tool_calls_are_recognised() {
        let read = ToolDefinition {
            name: "Read".into(),
            description: String::new(),
            input_schema: serde_json::json!({"type":"object","required":["file_path"]}),
            cache_control: None,
        };
        let call = |name: &str, input: serde_json::Value| {
            vec![ContentBlock::ToolUse {
                id: "t".into(),
                name: name.into(),
                input,
            }]
        };
        let tools = [read];
        assert!(!has_malformed_tool_call(
            &call("Read", serde_json::json!({"file_path":"a.rs"})),
            &tools
        ));
        assert!(has_malformed_tool_call(
            &call("Read", serde_json::json!({})),
            &tools
        ));
        assert!(has_malformed_tool_call(
            &call("Raed", serde_json::json!({})),
            &tools
        ));
        assert!(has_malformed_tool_call(
            &call("Read", serde_json::json!("a.rs")),
            &tools
        ));

        let mut turn = TurnRoute::new(RouterConfig::default(), Complexity::Low);
        let bad = call("Read", serde_json::json!({}));
        let good = call("Read", serde_json::json!({"file_path":"a.rs"}));
        assert!(!turn.malformed_twice(&bad, &tools));
        assert!(!turn.malformed_twice(&good, &tools), "a good one resets");
        assert!(!turn.malformed_twice(&bad, &tools));
        assert!(turn.malformed_twice(&bad, &tools));
    }

    #[test]
    fn test_short_confirmations_are_low() {
        assert_eq!(detect_complexity("yes"), Complexity::Low);
        assert_eq!(detect_complexity("ok"), Complexity::Low);
        assert_eq!(detect_complexity("do it"), Complexity::Low);
    }

    #[test]
    fn test_simple_questions_are_low() {
        assert_eq!(
            detect_complexity("what does this function do?"),
            Complexity::Low
        );
        assert_eq!(
            detect_complexity("explain the config module"),
            Complexity::Low
        );
    }

    #[test]
    fn test_refactor_is_high() {
        assert_eq!(
            detect_complexity(
                "refactor the authentication module to use JWT tokens instead of sessions"
            ),
            Complexity::High
        );
    }

    #[test]
    fn test_complex_debug_is_high() {
        assert_eq!(
            detect_complexity(
                "there's a race condition in the connection pool when multiple threads try to acquire a connection simultaneously, can you debug and fix it?"
            ),
            Complexity::High
        );
    }

    #[test]
    fn test_medium_tasks() {
        assert_eq!(
            detect_complexity("add a test for the parse_config function"),
            Complexity::Medium
        );
    }

    #[test]
    fn test_super_high_entire_codebase() {
        assert_eq!(
            detect_complexity(
                "analyze the entire codebase and refactor all error handling to use a consistent pattern"
            ),
            Complexity::SuperHigh
        );
    }

    #[test]
    fn test_super_high_full_audit() {
        assert_eq!(
            detect_complexity(
                "do a full audit of every file in the project for security vulnerabilities"
            ),
            Complexity::SuperHigh
        );
    }
}
