/// Cost tracking + budget management.
///
/// Tracks per-model token usage and cost across the session.
/// Supports budget limits with warnings at 80% and hard stop at 100%.
use std::collections::HashMap;

// ─── Per-model pricing (USD per million tokens) ─────────────────────────────

pub(crate) struct ModelPrice {
    pub(crate) input: f64,
    pub(crate) output: f64,
    /// Cache-read price as a fraction of `input`: the provider's published
    /// cached-input rate, or 1.0 where none is known, so a cache hit is
    /// never priced below what it may cost. Writes (5-minute TTL) are always
    /// 1.25× input.
    cache_read_mult: f64,
    /// True when this is an approximation (a rough third-party rate or the
    /// unknown-model fallback) rather than a known published rate. Surfaced in
    /// `/cost` so a wrong number is never presented as an authoritative one.
    estimated: bool,
    /// True only for the unknown-model Sonnet-tier fallback, so `/cost` does
    /// not tell a DeepSeek or Groq user their model was not recognised.
    fallback: bool,
    /// `(threshold, input, output)`: a call whose prompt (uncached input
    /// plus cache reads and writes) exceeds `threshold` tokens is billed at
    /// these rates instead, as Gemini Pro does above 200k.
    long_context: Option<(u64, f64, f64)>,
}

/// Cache writes with the default 5-minute TTL bill at 1.25× the input rate.
const CACHE_WRITE_MULT: f64 = 1.25;

impl ModelPrice {
    /// USD for one API call's usage. `input` excludes cached tokens, which the
    /// API reports separately.
    pub(crate) fn cost(&self, input: u64, output: u64, cache_read: u64, cache_write: u64) -> f64 {
        let prompt = input + cache_read + cache_write;
        let (inp, out) = match self.long_context {
            Some((threshold, i, o)) if prompt > threshold => (i, o),
            _ => (self.input, self.output),
        };
        let per_m = |tokens: u64, rate: f64| tokens as f64 / 1_000_000.0 * rate;
        per_m(input, inp)
            + per_m(output, out)
            + per_m(cache_read, inp * self.cache_read_mult)
            + per_m(cache_write, inp * CACHE_WRITE_MULT)
    }
}

/// Get pricing for a model (USD per million tokens).
pub(crate) fn model_price(model: &str) -> ModelPrice {
    let published = |input: f64, output: f64| ModelPrice {
        input,
        output,
        cache_read_mult: 0.1,
        estimated: false,
        fallback: false,
        long_context: None,
    };
    // No cached-input rate known: a cache hit costs the full input rate.
    let rough = |input: f64, output: f64| ModelPrice {
        input,
        output,
        cache_read_mult: 1.0,
        estimated: true,
        fallback: false,
        long_context: None,
    };
    // OpenRouter spells versions with dots (`anthropic/claude-opus-4.1`);
    // the generation checks below are written against Anthropic's dashes.
    let m = model.to_ascii_lowercase().replace('.', "-");

    // Ollama and LM Studio run on the user's own hardware: free, and checked
    // first so a local GGUF named after a Claude or GPT model is not billed
    // at that model's rate (which tripped /budget on free inference).
    if m.starts_with("ollama:") || m.starts_with("lmstudio:") {
        return published(0.0, 0.0);
    }

    // Anthropic list prices per million tokens (docs, 2026-06). Newer
    // generations are cheaper than older ones, so match the generation, not
    // just the family — "opus" alone would charge Opus 5 at Opus 4.1 rates.
    if m.contains("fable") || m.contains("mythos") {
        // The 5.1 generation reads cache at $0.25/MTok.
        if m.contains("-5-1") {
            ModelPrice {
                cache_read_mult: 0.025,
                ..published(10.0, 50.0)
            }
        } else {
            published(10.0, 50.0)
        }
    } else if m.contains("opus") {
        // Opus 5.5: $4/$20, cache reads $0.20. Opus 4.5–5: $5/$25.
        // Opus 4.1 and earlier: $15/$75.
        if m.contains("opus-5-5") {
            ModelPrice {
                cache_read_mult: 0.05,
                ..published(4.0, 20.0)
            }
        } else if m.contains("3-opus") || m.contains("opus-3") || is_opus_4_0_or_4_1(&m) {
            published(15.0, 75.0)
        } else {
            published(5.0, 25.0)
        }
    } else if m.contains("sonnet") {
        if m.contains("sonnet-5") {
            published(2.0, 10.0)
        } else {
            published(3.0, 15.0)
        }
    } else if m.contains("haiku") {
        if m.contains("3-5-haiku") || m.contains("haiku-3-5") {
            published(0.8, 4.0)
        } else if m.contains("3-haiku") || m.contains("haiku-3") {
            published(0.25, 1.25)
        } else {
            published(1.0, 5.0)
        }
    } else if let Some(id) = m.strip_prefix("gemini:") {
        // Google's paid-tier list prices (2026); dots are dashes by now, so
        // `gemini-2.5-flash` reads `gemini-2-5-flash`. Flash-Lite before
        // Flash, which it contains. An unrecognised Gemini model gets the
        // dearest current rate (Gemini 3 Pro above 200k prompt tokens) so a
        // /budget cap errs early, not late. Implicit and explicit cache hits
        // are 75% off (2.5 and later; some models now charge less), so a
        // quarter of input is an upper bound. Pro bills a prompt over 200k
        // tokens at a higher rate, cache reads included.
        let gemini = |input: f64, output: f64| ModelPrice {
            cache_read_mult: 0.25,
            ..rough(input, output)
        };
        if id.contains("flash-lite") {
            // Per generation: 3.x Flash-Lite costs 2.5-3.75x what 2.x does.
            if id.contains("gemini-2-") {
                gemini(0.10, 0.40)
            } else if id.contains("gemini-3") {
                gemini(0.25, 1.50)
            } else {
                // As for any unrecognised Gemini model below.
                rough(4.0, 18.0)
            }
        } else if id.contains("gemini-3") && id.contains("flash") {
            gemini(0.50, 3.0)
        } else if id.contains("gemini-2-0-flash") {
            gemini(0.10, 0.40)
        } else if id.contains("flash") {
            gemini(0.30, 2.50)
        } else if id.contains("gemini-2-5-pro") {
            ModelPrice {
                long_context: Some((200_000, 2.50, 15.0)),
                ..gemini(1.25, 10.0)
            }
        } else if id.contains("gemini-3") && id.contains("pro") {
            ModelPrice {
                long_context: Some((200_000, 4.0, 18.0)),
                ..gemini(2.0, 12.0)
            }
        } else {
            // No published cached rate for an unrecognised model: a cache
            // hit costs the full input rate.
            rough(4.0, 18.0)
        }
    } else if m.contains("groq:") || m.contains("together:") {
        // Rough estimate for hosted open-source models
        rough(0.5, 1.0)
    } else if m.contains("deepseek:") {
        // DeepSeek-V3's list rates: $0.27 cache miss, $0.07 cache hit, $1.10
        // out. Later DeepSeek models charge less for a hit, never more.
        ModelPrice {
            cache_read_mult: 0.07 / 0.27,
            ..rough(0.27, 1.10)
        }
    } else if m.contains("mistral:") {
        rough(2.0, 6.0)
    } else if let Some(id) = model
        .to_ascii_lowercase()
        .strip_prefix("openrouter:openai/")
    {
        // OpenRouter passes OpenAI's list prices through. The Sonnet-tier
        // fallback billed o1-pro or gpt-5-pro at a fortieth of that, so a
        // /budget cap stopped far too late on the dearest models. Flagged:
        // OpenRouter's own rate may differ.
        ModelPrice {
            estimated: true,
            ..openai_price(id)
        }
    } else if let Some(id) = ["oai:", "openai:"].iter().find_map(|p| {
        model
            .to_ascii_lowercase()
            .strip_prefix(p)
            .map(str::to_string)
    }) {
        openai_price(&id)
    } else {
        // Unknown model — fall back to Sonnet-tier rates so a budget still
        // functions, but flag it: an unrecognised model may be an order of
        // magnitude cheaper or dearer, and silently reporting a guess as fact
        // is how a /budget cap gets trusted when it should not be.
        // An unprefixed model goes to the Anthropic backend (a proxy alias,
        // a Bedrock ARN, a new Claude family name), where Sonnet-tier cache
        // reads are 10% of input; a provider-prefixed one has no known
        // cached rate, so a hit costs the full input rate.
        let cache_read_mult = if crate::api::openai_compat::is_openai_compat_model(model) {
            1.0
        } else {
            0.1
        };
        ModelPrice {
            cache_read_mult,
            fallback: true,
            ..rough(3.0, 15.0)
        }
    }
}

/// OpenAI list prices per million tokens (standard tier, 2026-10) for the
/// bare model id, dots kept (`gpt-5.4-mini`). Each family has its own
/// cached-input rate: a tenth of input on GPT-5, a quarter on GPT-4.1 and
/// o3 / o4-mini, half on GPT-4o and older o-series; the `-pro` models have
/// none. Variants are matched before their base id (`-pro`, `-mini`,
/// `-nano`). A GPT-5 point release or GPT-6 model newer than this table is
/// billed at the dearest rate known for its tier and flagged, so a
/// `/budget` cap errs early rather than late.
fn openai_price(id: &str) -> ModelPrice {
    let price = |input: f64, output: f64, cache_read_mult: f64| ModelPrice {
        input,
        output,
        cache_read_mult,
        estimated: false,
        fallback: false,
        long_context: None,
    };
    let estimated = |p: ModelPrice| ModelPrice {
        estimated: true,
        ..p
    };
    // 1.05M-window models: a prompt over 272K input tokens bills at 2x
    // input and 1.5x output.
    let long = |p: ModelPrice| ModelPrice {
        long_context: Some((272_000, p.input * 2.0, p.output * 1.5)),
        ..p
    };
    let tier = |t: &str| id.contains(&format!("-{t}"));
    if let Some((major, minor)) = crate::api::openai_compat::responses::gpt_version(id)
        && major >= 5
    {
        let v = (major, minor);
        return if tier("pro") {
            match v {
                (5, 0) | (5, 1) => price(15.0, 120.0, 1.0),
                (5, 2) | (5, 3) => price(21.0, 168.0, 1.0),
                (5, 4) | (5, 5) => long(price(30.0, 180.0, 1.0)),
                _ => estimated(long(price(30.0, 180.0, 1.0))),
            }
        } else if tier("nano") {
            match v {
                (5, 0..=3) => price(0.05, 0.40, 0.1),
                (5, 4) => price(0.20, 1.25, 0.1),
                _ => estimated(price(0.20, 1.25, 0.1)),
            }
        } else if tier("mini") {
            match v {
                (5, 0..=3) => price(0.25, 2.0, 0.1),
                (5, 4) => price(0.75, 4.50, 0.1),
                _ => estimated(price(0.75, 4.50, 0.1)),
            }
        } else {
            // The base model, its `-codex` and `-chat-latest` variants.
            match v {
                (5, 0) | (5, 1) => price(1.25, 10.0, 0.1),
                (5, 2) | (5, 3) => price(1.75, 14.0, 0.1),
                (5, 4) => long(price(2.50, 15.0, 0.1)),
                (5, 5) => long(price(5.0, 30.0, 0.1)),
                // Later tiers run from $0.20 to $10 in (GPT-6 Astra).
                _ => estimated(long(price(10.0, 50.0, 0.1))),
            }
        };
    }
    if id.starts_with("gpt-4.1") {
        if tier("nano") {
            price(0.10, 0.40, 0.25)
        } else if tier("mini") {
            price(0.40, 1.60, 0.25)
        } else {
            price(2.0, 8.0, 0.25)
        }
    } else if id.starts_with("gpt-4o") || id.starts_with("chatgpt-4o") {
        if tier("mini") {
            price(0.15, 0.60, 0.5)
        } else {
            price(2.50, 10.0, 0.5)
        }
    } else if id.starts_with("o3-deep-research") {
        price(10.0, 40.0, 0.25)
    } else if id.starts_with("o4-mini-deep-research") {
        price(2.0, 8.0, 0.25)
    } else if id.starts_with("o3-pro") {
        price(20.0, 80.0, 1.0)
    } else if id.starts_with("o3-mini") {
        price(1.10, 4.40, 0.5)
    } else if id.starts_with("o3") {
        price(2.0, 8.0, 0.25)
    } else if id.starts_with("o4-mini") {
        price(1.10, 4.40, 0.25)
    } else if id.starts_with("o1-pro") {
        price(150.0, 600.0, 1.0)
    } else if id.starts_with("o1-mini") {
        price(1.10, 4.40, 0.5)
    } else if id.starts_with("o1") {
        price(15.0, 60.0, 0.5)
    } else if id.starts_with("codex-mini") {
        price(1.50, 6.0, 0.25)
    } else {
        // Anything else (GPT-4 Turbo, GPT-3.5, a new family): GPT-4o rates,
        // flagged. Half price is the largest cached share OpenAI charges.
        estimated(price(2.50, 10.0, 0.5))
    }
}

/// Opus 4.0 / 4.1 ids: `opus-4` followed by nothing, a date
/// (`claude-opus-4-20250514`), a Vertex `@date`, or minor version 0 or 1.
/// Substring checks cannot tell `opus-4-1` from a future `opus-4-10`, nor
/// `opus-4-20250514` (4.0) from `opus-4-5` (4.5).
fn is_opus_4_0_or_4_1(m: &str) -> bool {
    let Some(i) = m.find("opus-4") else {
        return false;
    };
    let rest = &m[i + "opus-4".len()..];
    let Some(rest) = rest.strip_prefix('-') else {
        // `opus-4`, `opus-4@20250514`, `opus-4:free`; `opus-45` is not an id.
        return !rest.starts_with(|c: char| c.is_ascii_digit());
    };
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    match &rest[..digits] {
        "" => true, // `opus-4-v1`
        "0" | "1" => true,
        d => d.len() == 8, // a date, so Opus 4.0
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
    /// Cost for this model is based on approximate rates, not published ones.
    pub estimated: bool,
    /// The model was not recognised and is priced at Sonnet-tier rates.
    pub fallback: bool,
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

    /// Record token usage for one API call with no prompt-cache traffic.
    #[cfg(test)]
    pub fn record(&mut self, model: &str, input_tokens: u64, output_tokens: u64) {
        self.record_with_cache(model, input_tokens, output_tokens, 0, 0);
    }

    /// Record token usage for one API call. `input_tokens` excludes the
    /// cache reads and writes, which are priced at their own rates.
    pub fn record_with_cache(
        &mut self,
        model: &str,
        input_tokens: u64,
        output_tokens: u64,
        cache_read: u64,
        cache_write: u64,
    ) {
        let price = model_price(model);
        let cost = price.cost(input_tokens, output_tokens, cache_read, cache_write);

        let entry = self.by_model.entry(model.to_string()).or_default();
        if price.estimated && !entry.estimated {
            entry.estimated = true;
            entry.fallback = price.fallback;
            if price.fallback {
                tracing::warn!(
                    "cost: '{model}' is not a recognised model — pricing it at Sonnet-tier \
                     rates. Reported cost and any /budget cap are estimates for this model."
                );
            } else {
                tracing::warn!(
                    "cost: '{model}' is priced at an approximate third-party rate. \
                     Reported cost and any /budget cap are estimates for this model."
                );
            }
        }
        entry.input_tokens += input_tokens;
        entry.output_tokens += output_tokens;
        entry.turns += 1;
        entry.cost_usd += cost;

        self.total_cost_usd += cost;
        // Context size is everything the model read, cached or not.
        self.last_input_tokens = input_tokens + cache_read + cache_write;
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

            let (mut any_rough, mut any_fallback) = (false, false);
            for (model, usage) in models {
                let short = short_model_name(model);
                if usage.fallback {
                    any_fallback = true;
                } else if usage.estimated {
                    any_rough = true;
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
            if any_rough || any_fallback {
                lines.push(String::new());
            }
            if any_rough {
                lines.push("  ~ estimated — approximate third-party rates.".into());
            }
            if any_fallback {
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
    } else if let Some(rest) = model.strip_prefix("gemini:") {
        format!("Gemini ({rest})")
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
                    fallback: false,
                },
            );
        }

        let summary = tracker.summary();
        assert!(summary.contains("Per-model breakdown:"));
        assert!(
            summary.contains("model-nan"),
            "every model must still render"
        );
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
        assert!(
            s.contains('~'),
            "estimate must be marked in the report: {s}"
        );
        assert!(s.contains("not recognised"), "and explained: {s}");
    }

    /// DeepSeek/Groq rows are known approximate rates, not the Sonnet-tier
    /// fallback; the footnote used to claim they were not recognised.
    #[test]
    fn rough_third_party_rates_are_not_reported_as_unrecognised() {
        let mut t = CostTracker::new();
        t.record("deepseek:deepseek-chat", 1_000_000, 0);
        t.record("groq:llama-3.3-70b", 1_000_000, 0);
        assert!(t.by_model["deepseek:deepseek-chat"].estimated);
        assert!(!t.by_model["deepseek:deepseek-chat"].fallback);
        let s = t.summary();
        assert!(s.contains("approximate third-party rates"), "{s}");
        assert!(!s.contains("not recognised"), "{s}");

        t.record("some-new-provider:mystery-model", 1_000_000, 0);
        let s = t.summary();
        assert!(s.contains("approximate third-party rates"), "{s}");
        assert!(s.contains("not recognised"), "{s}");
    }

    /// OpenRouter-hosted OpenAI models fell to the $3/$15 fallback, so
    /// /budget let o1-pro ($150/$600) run about 40x past the cap.
    #[test]
    fn openrouter_openai_models_use_openai_rates() {
        for (model, input, output) in [
            ("openrouter:openai/o1-pro", 150.0, 600.0),
            ("openrouter:openai/gpt-5-pro", 15.0, 120.0),
            ("openrouter:openai/o3-pro", 20.0, 80.0),
            ("openrouter:openai/gpt-5.4-mini", 0.75, 4.50),
            ("OpenRouter:OpenAI/GPT-4.1", 2.0, 8.0),
        ] {
            let p = model_price(model);
            assert!(p.estimated, "{model}");
            assert!(!p.fallback, "{model}");
            assert_eq!((p.input, p.output), (input, output), "{model}");
        }
    }

    /// Gemini fell through to the Sonnet-tier fallback, so a `/budget` cap
    /// on gemini-2.5-flash ($0.30/$2.50) tripped about 6-10x early and `/cost`
    /// called the model unrecognised.
    #[test]
    fn gemini_models_have_rough_rates_not_the_fallback() {
        for (model, input, output) in [
            ("gemini:gemini-2.5-flash", 0.30, 2.50),
            ("gemini:gemini-2.5-flash-lite", 0.10, 0.40),
            ("gemini:gemini-2.0-flash-lite", 0.10, 0.40),
            ("gemini:gemini-3.1-flash-lite-preview", 0.25, 1.50),
            ("gemini:gemini-flash-lite-latest", 4.0, 18.0),
            ("gemini:gemini-2.5-pro", 1.25, 10.0),
            ("gemini:gemini-3-pro-preview", 2.0, 12.0),
        ] {
            let p = model_price(model);
            assert!(p.estimated, "{model}");
            assert!(!p.fallback, "{model}");
            assert_eq!((p.input, p.output), (input, output), "{model}");
        }

        let mut t = CostTracker::new();
        t.record("gemini:gemini-2.5-flash", 1_000_000, 1_000_000);
        assert!(t.by_model["gemini:gemini-2.5-flash"].estimated);
        assert!(!t.by_model["gemini:gemini-2.5-flash"].fallback);
        assert!(
            (t.total_cost_usd - 2.80).abs() < 1e-9,
            "{}",
            t.total_cost_usd
        );
        let s = t.summary();
        assert!(s.contains("Gemini (gemini-2.5-flash)"), "{s}");
        assert!(s.contains("approximate third-party rates"), "{s}");
        assert!(!s.contains("not recognised"), "{s}");
    }

    #[test]
    fn known_models_are_not_marked_as_estimated() {
        let mut t = CostTracker::new();
        for m in [
            "claude-opus-5",
            "claude-sonnet-4-6",
            "claude-haiku-4-5",
            "ollama:llama3",
        ] {
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
        // Haiku 4.5: 1.0/M * 0.01 + 5.0/M * 0.001 = 0.010 + 0.005 = 0.015
        assert!((tracker.total_cost_usd - 0.015).abs() < 0.0001);
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
        // Haiku cost 5 × (0.010 + 0.010) = 0.10; Opus 4.6 would have been
        // 5 × (0.050 + 0.050) = 0.50 → 0.40 saved.
        assert!((savings - 0.40).abs() < 0.01, "{savings}");
    }
}

#[cfg(test)]
mod price_table_tests {
    use super::model_price;

    /// Anthropic list prices per million tokens (docs, 2026-06). The old
    /// table charged Opus at 3× and Haiku at ¼ of reality, so `/budget`
    /// stopped a session far too early or far too late.
    #[test]
    fn current_claude_models_use_published_rates() {
        for (model, input, output) in [
            ("claude-fable-5-1", 10.0, 50.0),
            ("claude-opus-5-5", 4.0, 20.0),
            ("claude-opus-5", 5.0, 25.0),
            ("claude-sonnet-5-5", 2.0, 10.0),
            ("claude-opus-4-6", 5.0, 25.0),
            ("claude-opus-4-5", 5.0, 25.0),
            ("claude-opus-4-5-20251101", 5.0, 25.0),
            ("openrouter:anthropic/claude-opus-4.5", 5.0, 25.0),
            ("claude-opus-4-1", 15.0, 75.0),
            ("claude-opus-4-1-20250805", 15.0, 75.0),
            ("claude-opus-4-0", 15.0, 75.0),
            ("claude-opus-4-20250514", 15.0, 75.0),
            ("us.anthropic.claude-opus-4-20250514-v1:0", 15.0, 75.0),
            ("claude-opus-4@20250514", 15.0, 75.0),
            ("openrouter:anthropic/claude-opus-4.1", 15.0, 75.0),
            ("openrouter:anthropic/claude-opus-4", 15.0, 75.0),
            ("claude-3-opus-20240229", 15.0, 75.0),
            ("openrouter:anthropic/claude-3.5-haiku", 0.8, 4.0),
            ("claude-sonnet-5", 2.0, 10.0),
            ("claude-sonnet-4-6", 3.0, 15.0),
            ("claude-haiku-4-5", 1.0, 5.0),
        ] {
            let p = model_price(model);
            assert_eq!((p.input, p.output), (input, output), "{model}");
            assert!(!p.estimated, "{model} is a published rate");
        }
    }

    /// GPT-5.4 and 5.5 (1.05M window) bill a prompt over 272K input tokens
    /// at 2x input and 1.5x output; at list rates a long session was
    /// under-billed by up to 2x and `/budget` stopped late.
    #[test]
    fn gpt_5_4_and_5_5_long_prompts_bill_at_the_long_context_rate() {
        let close = |a: f64, b: f64| (a - b).abs() < 1e-6;
        let m = 1_000_000;
        for (model, want) in [
            ("oai:gpt-5.4", 0.3 * 5.0 + 22.5),
            ("oai:gpt-5.4-pro", 0.3 * 60.0 + 270.0),
            ("oai:gpt-5.5", 0.3 * 10.0 + 45.0),
            ("oai:gpt-5.5-pro", 0.3 * 60.0 + 270.0),
        ] {
            let got = model_price(model).cost(300_000, m, 0, 0);
            assert!(close(got, want), "{model}: {got} != {want}");
        }
        // Exactly 272K is still the base rate.
        let got = model_price("oai:gpt-5.4").cost(272_000, m, 0, 0);
        assert!(close(got, 0.272 * 2.5 + 15.0), "{got}");
        // Cached input counts toward the threshold.
        let got = model_price("oai:gpt-5.4").cost(100_000, 0, 200_000, 0);
        assert!(close(got, 0.1 * 5.0 + 0.2 * 0.5), "{got}");
        // Unknown later versions err early, mini/nano and 5.0-5.3 do not.
        assert!(model_price("oai:gpt-5.9").long_context.is_some());
        for model in [
            "oai:gpt-5.4-mini",
            "oai:gpt-5.4-nano",
            "oai:gpt-5.2",
            "oai:gpt-5",
        ] {
            assert!(model_price(model).long_context.is_none(), "{model}");
        }
    }

    /// OpenAI list prices per family: variants before their base id, and
    /// each family's own cached-input share. `oai:gpt-5` was billed at
    /// GPT-4o rates (2x input, 5x cache reads) and the `-pro` models at a
    /// tenth of theirs, so `/budget` stopped them far too late.
    #[test]
    fn openai_models_have_their_own_list_prices() {
        let m = 1_000_000;
        for (model, input, output, cached) in [
            ("oai:gpt-5", 1.25, 10.0, 0.125),
            ("oai:gpt-5-mini", 0.25, 2.0, 0.025),
            ("oai:gpt-5-nano", 0.05, 0.40, 0.005),
            ("oai:gpt-5-pro", 15.0, 120.0, 15.0),
            ("oai:gpt-5-chat-latest", 1.25, 10.0, 0.125),
            ("oai:gpt-5.1-codex-max", 1.25, 10.0, 0.125),
            ("oai:gpt-5.1-codex-mini", 0.25, 2.0, 0.025),
            ("oai:gpt-5.2", 1.75, 14.0, 0.175),
            ("oai:gpt-5.2-pro", 21.0, 168.0, 21.0),
            ("oai:gpt-5.3-codex", 1.75, 14.0, 0.175),
            ("oai:gpt-5.4", 2.50, 15.0, 0.25),
            ("oai:gpt-5.4-mini", 0.75, 4.50, 0.075),
            ("oai:gpt-5.4-nano", 0.20, 1.25, 0.02),
            ("oai:gpt-5.4-pro", 30.0, 180.0, 30.0),
            ("oai:gpt-5.5", 5.0, 30.0, 0.5),
            ("oai:gpt-5.5-pro", 30.0, 180.0, 30.0),
            ("oai:o3", 2.0, 8.0, 0.5),
            ("oai:o3-pro", 20.0, 80.0, 20.0),
            ("oai:o3-mini", 1.10, 4.40, 0.55),
            ("oai:o4-mini", 1.10, 4.40, 0.275),
            ("oai:o1", 15.0, 60.0, 7.5),
            ("oai:gpt-4.1", 2.0, 8.0, 0.5),
            ("oai:gpt-4.1-mini", 0.40, 1.60, 0.1),
            ("oai:gpt-4.1-nano", 0.10, 0.40, 0.025),
            ("OAI:GPT-4o", 2.50, 10.0, 1.25),
            ("openai:gpt-4o-mini", 0.15, 0.60, 0.075),
        ] {
            let p = model_price(model);
            let close = |a: f64, b: f64| (a - b).abs() < 1e-9;
            assert!(
                close(p.input, input) && close(p.output, output),
                "{model}: {} / {}",
                p.input,
                p.output
            );
            // 100K cached tokens: under GPT-5.4's long-context threshold.
            let got = p.cost(0, 0, m / 10, 0) * 10.0;
            assert!(close(got, cached), "{model} cache read: {got}");
            assert!(!p.estimated, "{model} is a list price");
        }
        // Newer than the table: flagged, and never cheaper than the newest
        // known model of its tier.
        for (model, floor) in [("oai:gpt-6-astra", 5.0), ("oai:gpt-5.9-pro", 30.0)] {
            let p = model_price(model);
            assert!(p.estimated && p.input >= floor, "{model}: {}", p.input);
        }
    }

    /// Third-party rows the code itself calls "rough" must say so, or the
    /// dashboard presents a guess as fact (the exact bug PR #14 fixed for
    /// unknown models).
    #[test]
    fn approximate_third_party_rates_are_flagged_as_estimates() {
        for model in ["groq:llama-3", "together:mixtral", "oai:gpt-4-turbo"] {
            assert!(model_price(model).estimated, "{model}");
        }
        assert!(
            !model_price("ollama:llama3").estimated,
            "local is exactly free"
        );
    }

    /// LM Studio fell through to the unknown-model Sonnet rate, so a
    /// /budget cap stopped sessions running on free local inference.
    #[test]
    fn local_providers_are_free() {
        for model in [
            "ollama:llama3",
            "lmstudio:qwen2.5-coder",
            "lmstudio:claude-sonnet-distill",
            "ollama:gpt-oss",
        ] {
            let p = model_price(model);
            assert_eq!((p.input, p.output), (0.0, 0.0), "{model}");
            assert!(!p.estimated, "{model}");
        }
        assert!(
            model_price("openai-compat:my-model").estimated,
            "an arbitrary endpoint is still a flagged guess"
        );
    }

    /// Cache reads and writes are billed, so `/cost` must count them.
    #[test]
    fn cache_reads_and_writes_are_priced() {
        let m = 1_000_000;
        let p = model_price("claude-sonnet-5");
        assert!((p.cost(0, 0, m, 0) - 0.2).abs() < 1e-9);
        assert!((p.cost(0, 0, 0, m) - 2.5).abs() < 1e-9);
        assert!((model_price("claude-opus-5-5").cost(0, 0, m, 0) - 0.2).abs() < 1e-9);
        assert!((model_price("claude-fable-5-1").cost(0, 0, m, 0) - 0.25).abs() < 1e-9);

        let mut t = super::CostTracker::new();
        t.record_with_cache("claude-sonnet-5", 100, 0, 9_000, 900);
        assert_eq!(t.last_input_tokens, 10_000, "context counts cached tokens");
    }

    /// Cached input on OpenAI-compatible providers is priced at the
    /// provider's published cached rate, or at the full input rate where
    /// none is known: a cache hit is never priced below what it may cost.
    #[test]
    fn cache_reads_use_provider_cached_rates_or_full_input() {
        let m = 1_000_000;
        // Under Gemini Pro's 200k long-context threshold: a tenth of a
        // million, so a tenth of the per-million rate.
        let tenth = 100_000;
        for (model, cached_per_m) in [
            ("oai:gpt-4o", 1.25),
            ("openai:gpt-4.1", 0.5),
            ("oai:gpt-5", 0.125),
            ("oai:o4-mini", 0.275),
            ("deepseek:deepseek-chat", 0.07),
            ("deepseek:deepseek-reasoner", 0.07),
            ("gemini:gemini-2.5-flash", 0.075),
            ("gemini:gemini-2.5-pro", 0.3125),
            ("gemini:gemini-3-pro-preview", 0.5),
            ("openrouter:openai/gpt-4o", 1.25),
        ] {
            let got = model_price(model).cost(0, 0, tenth, 0);
            assert!((got - cached_per_m / 10.0).abs() < 1e-9, "{model}: {got}");
        }
        for model in [
            "groq:llama-3.3-70b",
            "together:mixtral",
            "mistral:mistral-large",
            "venice:llama-3.3-70b",
            "openai-compat:my-model",
            "openrouter:openai/o1-pro",
            "gemini:learnlm-1.5-pro-experimental",
            "gemini:gemini-1.5-pro",
        ] {
            let p = model_price(model);
            assert_eq!(p.cost(0, 0, m, 0), p.cost(m, 0, 0, 0), "{model}");
        }
    }

    /// An unrecognised unprefixed model runs on the Anthropic backend (a
    /// proxy alias, a Bedrock ARN, a new Claude family), where Sonnet-tier
    /// cache reads are 10% of input; full input there would report a mostly
    /// cached session at several times its cost and trip /budget early.
    #[test]
    fn unknown_anthropic_backend_model_keeps_sonnet_cache_rate() {
        let m = 1_000_000;
        for model in [
            "my-claude-proxy",
            "arn:aws:bedrock:us-east-1:123456789012:application-inference-profile/abc",
        ] {
            let p = model_price(model);
            assert!(p.fallback, "{model}");
            assert!((p.cost(0, 0, m, 0) - 0.3).abs() < 1e-9, "{model}");
        }
        // Behind an OpenAI-compatible prefix the cached rate is unknown.
        let p = model_price("openai-compat:my-claude-proxy");
        assert!(p.fallback);
        assert!((p.cost(0, 0, m, 0) - 3.0).abs() < 1e-9);
    }
    /// Gemini Pro bills a prompt over 200k tokens at its long-context rate;
    /// with a 1M window those prompts are normal, and the flat rate priced
    /// them at half, so /budget undercounted.
    #[test]
    fn gemini_pro_long_context_prompts_use_the_higher_rate() {
        let mut t = crate::cost::CostTracker::new();
        t.record_with_cache("gemini:gemini-2.5-pro", 300_000, 0, 0, 0);
        assert!(
            (t.total_cost_usd - 0.75).abs() < 1e-9,
            "{}",
            t.total_cost_usd
        );
        let p = model_price("gemini:gemini-3-pro-preview");
        // At the threshold: the base rate. Above it, cache reads count
        // towards the prompt and are doubled too.
        assert!((p.cost(200_000, 0, 0, 0) - 0.4).abs() < 1e-9);
        assert!((p.cost(100_000, 1_000_000, 200_000, 0) - (0.4 + 18.0 + 0.2)).abs() < 1e-9);
        // An unrecognised Gemini model is priced at the dearest rate.
        let u = model_price("gemini:gemini-9-ultra");
        assert_eq!((u.input, u.output), (4.0, 18.0));
    }
}
