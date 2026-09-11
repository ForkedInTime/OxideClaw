//! `/` command handlers — split out of `commands/mod.rs` mechanically.

use super::*;

pub const KNOWN_MODELS: &[(&str, &str)] = &[
    ("claude-sonnet-5", "Smart & fast — recommended (default)"),
    (
        "claude-opus-5",
        "Most capable general model — complex, long tasks",
    ),
    (
        "claude-fable-5-1",
        "Frontier model — hardest reasoning; priced above Opus",
    ),
    (
        "claude-haiku-4-5",
        "Fastest & cheapest — great for simple tasks",
    ),
    ("claude-opus-4-6", "Previous Opus generation"),
    ("claude-sonnet-4-6", "Previous Sonnet generation"),
];

// Cost per million tokens (input, output) in USD

pub(super) fn model_pricing(model: &str) -> (f64, f64) {
    if crate::api::is_ollama_model(model) {
        (0.0, 0.0) // Local — free
    } else if model.contains("opus-4") {
        (15.0, 75.0)
    } else if model.contains("sonnet-4") {
        (3.0, 15.0)
    } else if model.contains("haiku") {
        (0.25, 1.25)
    } else {
        (3.0, 15.0) // fallback to sonnet pricing
    }
}

// ── Command action ────────────────────────────────────────────────────────────

pub(super) fn cmd_model(args: &str, _ctx: &CommandContext) -> CommandAction {
    if args.is_empty() {
        CommandAction::ListModels
    } else {
        // Normalize "provider name" (space) → "provider:name" (colon)
        let raw = args.trim();
        let model = if raw == "default" {
            crate::api::default_model().to_string()
        } else if let Some(rest) = raw.strip_prefix("ollama ") {
            format!("ollama:{}", rest.trim())
        } else if let Some(space_pos) = raw.find(' ') {
            let prefix = &raw[..space_pos];
            // Check if the word before the space is a known provider prefix
            if crate::api::PROVIDERS.iter().any(|p| p.prefix == prefix) {
                format!("{prefix}:{}", raw[space_pos + 1..].trim())
            } else {
                raw.to_string()
            }
        } else {
            raw.to_string()
        };

        // Ollama models — accept any "ollama:<name>" without checking KNOWN_MODELS
        if crate::api::is_ollama_model(&model) {
            return CommandAction::SetModel(model);
        }

        // OpenAI-compatible provider models — accept any "prefix:<name>"
        if crate::api::is_openai_compat_model(&model) {
            return CommandAction::SetModel(model);
        }

        // Resolve common shorthands before validation
        let model = resolve_model_alias(&model);

        // Anthropic models — validate against known list
        let known = KNOWN_MODELS.iter().any(|(n, _)| *n == model);
        if !known {
            let names: Vec<_> = KNOWN_MODELS.iter().map(|(n, _)| *n).collect();
            let providers: Vec<_> = crate::api::PROVIDERS
                .iter()
                .map(|p| format!("  /model {}:<model-name>  ({})", p.prefix, p.name))
                .collect();
            return CommandAction::Message(format!(
                "Unknown model '{model}'.\n\
                 Known Anthropic models:\n  {}\n\
                 \n\
                 Shorthands: opus, sonnet, haiku\n\
                 \n\
                 Local models:\n  /model ollama:<name>\n\
                 \n\
                 OpenAI-compatible providers:\n{}",
                names.join("\n  "),
                providers.join("\n"),
            ));
        }
        CommandAction::SetModel(model)
    }
}

/// Resolve common model shorthands to full Anthropic model IDs.
/// e.g. "opus" → "claude-opus-5", "sonnet" → "claude-sonnet-5"
pub fn resolve_model_alias(model: &str) -> String {
    match model.to_ascii_lowercase().as_str() {
        // Bare family names mean the current generation.
        "opus" => "claude-opus-5".into(),
        "sonnet" => "claude-sonnet-5".into(),
        "haiku" => "claude-haiku-4-5".into(),
        "fable" => "claude-fable-5-1".into(),
        "opus-4-6" | "opus4.6" => "claude-opus-4-6".into(),
        "sonnet-4-6" | "sonnet4.6" => "claude-sonnet-4-6".into(),
        "opus-4-5" | "opus4.5" => "claude-opus-4-5".into(),
        "sonnet-4-5" | "sonnet4.5" => "claude-sonnet-4-5".into(),
        _ => model.to_string(),
    }
}

pub(super) fn cmd_effort(args: &str, ctx: &CommandContext) -> CommandAction {
    let level = args.trim().to_lowercase();
    let _ = ctx;
    match level.as_str() {
        "1" | "low" | "quick" => CommandAction::SendPrompt(
            "For this conversation, keep responses brief and fast. \
             Prefer quick solutions over thorough analysis. Skip detailed explanations unless asked."
                .into()
        ),
        "2" | "medium" | "normal" | "" => CommandAction::SendPrompt(
            "Use normal effort for this conversation — balance thoroughness with efficiency."
                .into()
        ),
        "3" | "high" | "thorough" | "max" => CommandAction::SendPrompt(
            "For this conversation, use maximum effort. Be thorough, check edge cases, \
             write comprehensive tests, and explain your reasoning in detail."
                .into()
        ),
        _ => CommandAction::Message(format!(
            "Usage: /effort [1|2|3] or [low|medium|high]\n\n\
             1 / low    — quick, brief responses\n\
             2 / medium — balanced (default)\n\
             3 / high   — thorough, detailed responses\n\n\
             Got: '{}'", args.trim()
        )),
    }
}
