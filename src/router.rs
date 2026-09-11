/// Smart Model Router — auto-detect task complexity and route to the cheapest capable model.
///
/// Simple tasks (explain, rename, format, typos) → cheap model (Haiku / Ollama)
/// Complex tasks (refactor, architect, debug race conditions) → expensive model (Opus / Sonnet)
///
/// Other tools exist solely to provide this functionality.
/// We do it natively, in the binary.
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

/// Model assignments per complexity tier.
#[derive(Debug, Clone)]
pub struct RouterConfig {
    /// Model for low-complexity tasks (default: claude-haiku-4-5)
    pub low_model: String,
    /// Model for medium-complexity tasks (default: claude-sonnet-4-6)
    pub medium_model: String,
    /// Model for high-complexity tasks (default: whatever the user configured)
    pub high_model: String,
    /// Model for super-high tasks needing 1M context (default: claude-opus-4-6)
    pub super_high_model: String,
    /// Whether the router is enabled
    pub enabled: bool,
}

impl Default for RouterConfig {
    fn default() -> Self {
        Self {
            low_model: "claude-haiku-4-5".into(),
            medium_model: "claude-sonnet-5".into(),
            high_model: String::new(), // filled from config.model at runtime
            super_high_model: "claude-opus-5".into(),
            enabled: false,
        }
    }
}

impl RouterConfig {
    /// Create a new router config, inheriting the user's configured model as the high tier.
    pub fn new(user_model: &str) -> Self {
        Self {
            high_model: user_model.to_string(),
            ..Default::default()
        }
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

// ─── Router state (shared across turns) ─────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

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
