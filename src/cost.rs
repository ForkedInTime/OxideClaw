/// Cost tracking + budget management.
///
/// Tracks per-model token usage and cost across the session.
/// Supports budget limits with warnings at 80% and hard stop at 100%.
use std::collections::HashMap;

// ─── Per-model pricing (USD per million tokens) ─────────────────────────────

struct ModelPrice {
    input: f64,
    output: f64,
    /// True when this is a guess for an unrecognised model rather than a known
    /// published rate. Surfaced in `/cost` so a wrong number is never presented
    /// as an authoritative one.
    estimated: bool,
}

/// Get pricing for a model.  Returns (input_price, output_price) per million tokens.
fn model_price(model: &str) -> ModelPrice {
    if model.contains("opus") {
        ModelPrice {
            input: 15.0,
            output: 75.0,
            estimated: false,
        }
    } else if model.contains("haiku") {
        ModelPrice {
            input: 0.25,
            output: 1.25,
            estimated: false,
        }
    } else if model.contains("sonnet") {
        ModelPrice {
            input: 3.0,
            output: 15.0,
            estimated: false,
        }
    } else if model.starts_with("ollama:") {
        // Local models are free
        ModelPrice {
            input: 0.0,
            output: 0.0,
            estimated: false,
        }
    } else if model.contains("groq:") || model.contains("together:") {
        // Rough estimate for hosted open-source models
        ModelPrice {
            input: 0.5,
            output: 1.0,
            estimated: false,
        }
    } else if model.contains("deepseek:") {
        ModelPrice {
            input: 0.27,
            output: 1.10,
            estimated: false,
        }
    } else if model.contains("mistral:") {
        ModelPrice {
            input: 2.0,
            output: 6.0,
            estimated: false,
        }
    } else if model.contains("oai:") || model.contains("openai:") {
        // GPT-4o class pricing
        ModelPrice {
            input: 2.5,
            output: 10.0,
            estimated: false,
        }
    } else {
        // Unknown model — fall back to Sonnet-tier rates so a budget still
        // functions, but flag it: an unrecognised model may be an order of
        // magnitude cheaper or dearer, and silently reporting a guess as fact
        // is how a /budget cap gets trusted when it should not be.
        ModelPrice {
            input: 3.0,
            output: 15.0,
            estimated: true,
        }
    }
}

// ─── Per-model usage tracking ───────────────────────────────────────────────

/// Token usage for a single model.
#[derive(Debug, Clone, Default)]
pub struct ModelUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub turns: u32,
    pub cost_usd: f64,
    /// Cost for this model is based on fallback rates, not published ones.
    pub estimated: bool,
}

/// Session-wide cost tracker.
#[derive(Debug, Clone)]
pub struct CostTracker {
    /// Per-model usage breakdown.
    pub by_model: HashMap<String, ModelUsage>,
    /// Total session cost.
    pub total_cost_usd: f64,
    /// Budget limit (None = unlimited).
    pub budget_usd: Option<f64>,
    /// Input tokens from the most recent API turn (for context % display).
    pub last_input_tokens: u64,
}

impl Default for CostTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl CostTracker {
    pub fn new() -> Self {
        Self {
            by_model: HashMap::new(),
            total_cost_usd: 0.0,
            budget_usd: None,
            last_input_tokens: 0,
        }
    }

    /// Set a budget limit for this session.
    pub fn set_budget(&mut self, usd: f64) {
        self.budget_usd = Some(usd);
    }

    /// Clear the budget limit.
    pub fn clear_budget(&mut self) {
        self.budget_usd = None;
    }

    /// Record token usage for a turn.
    pub fn record(&mut self, model: &str, input_tokens: u64, output_tokens: u64) {
        let price = model_price(model);
        let cost = (input_tokens as f64 / 1_000_000.0) * price.input
            + (output_tokens as f64 / 1_000_000.0) * price.output;

        let entry = self.by_model.entry(model.to_string()).or_default();
        if price.estimated && !entry.estimated {
            entry.estimated = true;
            tracing::warn!(
                "cost: '{model}' is not a recognised model — pricing it at Sonnet-tier \
                 rates. Reported cost and any /budget cap are estimates for this model."
            );
        }
        entry.input_tokens += input_tokens;
        entry.output_tokens += output_tokens;
        entry.turns += 1;
        entry.cost_usd += cost;

        self.total_cost_usd += cost;
        self.last_input_tokens = input_tokens;
    }

    /// Check if the budget is exceeded.
    pub fn over_budget(&self) -> bool {
        match self.budget_usd {
            Some(b) => self.total_cost_usd >= b,
            None => false,
        }
    }

    /// Check if we're at the warning threshold (80%).
    pub fn budget_warning(&self) -> bool {
        match self.budget_usd {
            Some(b) => self.total_cost_usd >= b * 0.8 && self.total_cost_usd < b,
            None => false,
        }
    }

    /// Budget remaining (None if no budget set).
    pub fn remaining(&self) -> Option<f64> {
        self.budget_usd.map(|b| (b - self.total_cost_usd).max(0.0))
    }

    /// Format a summary suitable for display.
    pub fn summary(&self) -> String {
        let mut lines = Vec::new();

        lines.push(format!("Session cost: ${:.4}", self.total_cost_usd));
        if let Some(budget) = self.budget_usd {
            let pct = (self.total_cost_usd / budget * 100.0).min(100.0);
            lines.push(format!(
                "Budget: ${:.2} ({:.0}% used, ${:.4} remaining)",
                budget,
                pct,
                (budget - self.total_cost_usd).max(0.0)
            ));
        }

        if !self.by_model.is_empty() {
            lines.push(String::new());
            lines.push("Per-model breakdown:".into());

            let mut models: Vec<_> = self.by_model.iter().collect();
            // `total_cmp` rather than `partial_cmp().unwrap()`: the release profile
            // sets `panic = "abort"`, so a NaN cost would take the whole process down
            // just to render a cost summary. NaN is not reachable today, but a total
            // order costs nothing and removes the failure mode permanently.
            models.sort_by(|a, b| b.1.cost_usd.total_cmp(&a.1.cost_usd));

            let mut any_estimated = false;
            for (model, usage) in models {
                let short = short_model_name(model);
                if usage.estimated {
                    any_estimated = true;
                }
                lines.push(format!(
                    "  {short}: {turns} turns, {in_tok} in / {out_tok} out, {approx}${cost:.4}",
                    turns = usage.turns,
                    in_tok = format_tokens(usage.input_tokens),
                    out_tok = format_tokens(usage.output_tokens),
                    approx = if usage.estimated { "~" } else { "" },
                    cost = usage.cost_usd,
                ));
            }
            if any_estimated {
                lines.push(String::new());
                lines.push(
                    "  ~ estimated — model not recognised, priced at Sonnet-tier rates.".into(),
                );
            }
        }

        let total_in: u64 = self.by_model.values().map(|u| u.input_tokens).sum();
        let total_out: u64 = self.by_model.values().map(|u| u.output_tokens).sum();
        let total_turns: u32 = self.by_model.values().map(|u| u.turns).sum();
        if total_turns > 0 {
            lines.push(String::new());
            lines.push(format!(
                "Total: {} turns, {} in / {} out",
                total_turns,
                format_tokens(total_in),
                format_tokens(total_out),
            ));
        }

        lines.join("\n")
    }

    /// Total input tokens across all models (approximation of context usage).
    #[allow(dead_code)] // used by /cost and future dashboards
    pub fn total_input_tokens(&self) -> u64 {
        self.by_model.values().map(|u| u.input_tokens).sum()
    }

    /// Total output tokens across all models.
    #[allow(dead_code)] // used by /cost and future dashboards
    pub fn total_output_tokens(&self) -> u64 {
        self.by_model.values().map(|u| u.output_tokens).sum()
    }

    /// Context usage as percentage of a given context window size.
    pub fn context_pct(&self, context_window: u64) -> f64 {
        if context_window == 0 {
            return 0.0;
        }
        (self.last_input_tokens as f64 / context_window as f64 * 100.0).min(100.0)
    }

    /// One-line status for the TUI banner.
    pub fn banner_text(&self) -> String {
        if self.total_cost_usd < 0.0001 {
            return String::new();
        }
        match self.budget_usd {
            Some(b) => format!("${:.3} / ${:.2}", self.total_cost_usd, b),
            None => format!("${:.3}", self.total_cost_usd),
        }
    }

    /// Estimate the savings from routing (compare actual cost vs all-high-model cost).
    pub fn routing_savings(&self, high_model: &str) -> f64 {
        let high_price = model_price(high_model);
        let hypothetical: f64 = self
            .by_model
            .values()
            .map(|u| {
                (u.input_tokens as f64 / 1_000_000.0) * high_price.input
                    + (u.output_tokens as f64 / 1_000_000.0) * high_price.output
            })
            .sum();
        (hypothetical - self.total_cost_usd).max(0.0)
    }
}

/// Shorten model IDs for display: "claude-sonnet-4-6-20250514" → "Sonnet 4.6"
fn short_model_name(model: &str) -> String {
    if model.contains("opus") {
        "Opus".into()
    } else if model.contains("haiku") {
        "Haiku".into()
    } else if model.contains("sonnet") {
        "Sonnet".into()
    } else if let Some(rest) = model.strip_prefix("ollama:") {
        format!("Ollama ({rest})")
    } else if let Some(rest) = model.strip_prefix("groq:") {
        format!("Groq ({rest})")
    } else if let Some(rest) = model.strip_prefix("deepseek:") {
        format!("DeepSeek ({rest})")
    } else {
        model.to_string()
    }
}

/// Format token count for display: 1234 → "1.2K", 1234567 → "1.2M"
fn format_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}K", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `summary()` sorts per-model costs. With `partial_cmp().unwrap()` a non-finite
    /// cost aborted the process (release sets `panic = "abort"`); `total_cmp` orders
    /// it instead. Rendering a cost report must never be able to kill the session.
    #[test]
    fn summary_survives_non_finite_costs() {
        let mut tracker = CostTracker::new();
        tracker.record("claude-sonnet-4-6", 1_000, 100);

        for (name, cost) in [
            ("model-nan", f64::NAN),
            ("model-inf", f64::INFINITY),
            ("model-neg-inf", f64::NEG_INFINITY),
        ] {
            tracker.by_model.insert(
                name.to_string(),
                ModelUsage {
                    input_tokens: 1,
                    output_tokens: 1,
                    turns: 1,
                    cost_usd: cost,
                    estimated: false,
                },
            );
        }

        let summary = tracker.summary();
        assert!(summary.contains("Per-model breakdown:"));
        assert!(summary.contains("model-nan"), "every model must still render");
    }

    /// An unrecognised model is priced at Sonnet-tier rates so a budget still
    /// functions — but reporting that guess as fact is how a /budget cap gets
    /// trusted when it should not be.
    #[test]
    fn unknown_model_cost_is_marked_as_estimated() {
        let mut t = CostTracker::new();
        t.record("some-new-provider:mystery-model", 1_000_000, 0);
        assert!(
            t.by_model["some-new-provider:mystery-model"].estimated,
            "unrecognised model must be flagged"
        );
        let s = t.summary();
        assert!(s.contains('~'), "estimate must be marked in the report: {s}");
        assert!(s.contains("not recognised"), "and explained: {s}");
    }

    #[test]
    fn known_models_are_not_marked_as_estimated() {
        let mut t = CostTracker::new();
        for m in ["claude-opus-5", "claude-sonnet-4-6", "claude-haiku-4-5", "ollama:llama3"] {
            t.record(m, 1000, 100);
            assert!(!t.by_model[m].estimated, "{m} has published rates");
        }
        let s = t.summary();
        assert!(!s.contains("not recognised"), "no footnote expected: {s}");
    }

    /// Local models are free — pricing them at Sonnet rates would invent spend.
    #[test]
    fn ollama_models_are_free_and_not_estimated() {
        let mut t = CostTracker::new();
        t.record("ollama:llama3", 5_000_000, 5_000_000);
        assert_eq!(t.total_cost_usd, 0.0);
        assert!(!t.by_model["ollama:llama3"].estimated);
    }

    #[test]
    fn test_cost_tracking() {
        let mut tracker = CostTracker::new();
        tracker.record("claude-haiku-4-5-20251001", 10_000, 1_000);
        // Haiku: 0.25/M in + 1.25/M out = 0.0025 + 0.00125 = 0.00375
        assert!((tracker.total_cost_usd - 0.00375).abs() < 0.0001);
        assert_eq!(tracker.by_model.len(), 1);
    }

    #[test]
    fn test_budget() {
        let mut tracker = CostTracker::new();
        tracker.set_budget(1.0);
        tracker.record("claude-sonnet-4-6-20250514", 100_000, 10_000);
        assert!(!tracker.over_budget());
        // Sonnet: 3.0/M * 0.1 + 15.0/M * 0.01 = 0.30 + 0.15 = 0.45
        assert!((tracker.total_cost_usd - 0.45).abs() < 0.01);
    }

    #[test]
    fn test_ollama_is_free() {
        let mut tracker = CostTracker::new();
        tracker.record("ollama:llama3", 500_000, 50_000);
        assert_eq!(tracker.total_cost_usd, 0.0);
    }

    #[test]
    fn test_routing_savings() {
        let mut tracker = CostTracker::new();
        // 5 turns on Haiku instead of Opus
        for _ in 0..5 {
            tracker.record("claude-haiku-4-5-20251001", 10_000, 2_000);
        }
        let savings = tracker.routing_savings("claude-opus-4-6-20250514");
        // Should be significant
        assert!(savings > 0.5);
    }
}
