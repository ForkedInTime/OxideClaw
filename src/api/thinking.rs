//! Extended-thinking and effort wire formats, chosen per model generation.
//!
//! The Messages API takes two shapes for thinking. Claude 4.6+ Opus/Sonnet
//! and every Claude 5 model take `{"type":"adaptive"}`; Claude 5 rejects the
//! older `{"type":"enabled","budget_tokens":N}` with a 400. Haiku 4.5 and
//! anything older take only the `budget_tokens` form and reject `adaptive`.
//! `output_config.effort` follows the same split. Verified against the live
//! API and `GET /v1/models/{id}` capabilities on 2026-09-11.

use serde::Serialize;

/// Beta header that lets thinking blocks interleave with tool calls on the
/// `budget_tokens` generation. Adaptive-thinking models need nothing.
pub const INTERLEAVED_THINKING_BETA: &str = "interleaved-thinking-2025-05-14";

/// Smallest `budget_tokens` the API accepts.
pub const MIN_BUDGET_TOKENS: u32 = 1024;

/// `thinking` request field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThinkingConfig {
    Adaptive,
    Enabled { budget_tokens: u32 },
    Disabled,
}

impl Serialize for ThinkingConfig {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        match self {
            ThinkingConfig::Adaptive => {
                let mut st = s.serialize_struct("ThinkingConfig", 1)?;
                st.serialize_field("type", "adaptive")?;
                st.end()
            }
            ThinkingConfig::Enabled { budget_tokens } => {
                let mut st = s.serialize_struct("ThinkingConfig", 2)?;
                st.serialize_field("type", "enabled")?;
                st.serialize_field("budget_tokens", budget_tokens)?;
                st.end()
            }
            ThinkingConfig::Disabled => {
                let mut st = s.serialize_struct("ThinkingConfig", 1)?;
                st.serialize_field("type", "disabled")?;
                st.end()
            }
        }
    }
}

/// `output_config` request field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OutputConfig {
    pub effort: String,
}

/// Effort levels the API accepts.
pub const EFFORT_LEVELS: [&str; 4] = ["low", "medium", "high", "max"];

/// How a requested effort level reaches the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EffortWire {
    /// Model supports `output_config.effort`; send it.
    Param(OutputConfig),
    /// Model has no effort parameter; append this to the system prompt.
    Prompt(String),
}

/// `(major, minor)` parsed from a Claude model id, in either modern
/// (`claude-sonnet-4-6[-date]`) or legacy (`claude-3-5-haiku-date`) form.
pub fn model_version(model: &str) -> Option<(u32, u32)> {
    let (family_idx, _) = family_of(model)?;
    let tokens: Vec<&str> = model.split('-').collect();
    // A version token is 1–2 digits; an 8-digit token is a release date.
    let is_ver = |t: &str| !t.is_empty() && t.len() <= 2 && t.bytes().all(|b| b.is_ascii_digit());
    let after: Vec<u32> = tokens[family_idx + 1..]
        .iter()
        .take_while(|t| is_ver(t))
        .map(|t| t.parse().unwrap())
        .collect();
    let nums = if !after.is_empty() {
        after
    } else {
        tokens[..family_idx]
            .iter()
            .filter(|t| is_ver(t))
            .map(|t| t.parse().unwrap())
            .collect()
    };
    let major = *nums.first()?;
    let minor = nums.get(1).copied().unwrap_or(0);
    Some((major, minor))
}

/// Index and name of the family token in a Claude model id.
fn family_of(model: &str) -> Option<(usize, &str)> {
    const FAMILIES: [&str; 5] = ["opus", "sonnet", "haiku", "fable", "mythos"];
    let model = model.strip_prefix("claude-")?;
    model
        .split('-')
        .enumerate()
        .find(|(_, t)| FAMILIES.contains(t))
        .map(|(i, t)| (i + 1, t)) // +1 for the stripped "claude" token
}

/// Canonical id for the capability check (`sonnet` → `claude-sonnet-5`).
fn canonical(model: &str) -> String {
    crate::commands::resolve_model_alias(model)
}

/// Adaptive thinking: Claude 5+, Fable/Mythos, and Opus/Sonnet 4.6+.
pub fn supports_adaptive_thinking(model: &str) -> bool {
    let model = canonical(model);
    let Some((_, family)) = family_of(&model) else {
        return false;
    };
    if family == "fable" || family == "mythos" {
        return true;
    }
    match model_version(&model) {
        Some((major, _)) if major >= 5 => true,
        Some((4, minor)) => (family == "opus" || family == "sonnet") && minor >= 6,
        _ => false,
    }
}

/// `output_config.effort` support tracks adaptive thinking exactly.
pub fn supports_effort(model: &str) -> bool {
    supports_adaptive_thinking(model)
}

/// The `thinking` field for `model` given the user's budget setting.
/// `None` budget = leave the API default. `Some(0)` = off.
pub fn thinking_for(model: &str, budget: Option<u32>, max_tokens: u32) -> Option<ThinkingConfig> {
    let budget = budget?;
    let model = canonical(model);
    family_of(&model)?;
    let adaptive = supports_adaptive_thinking(&model);
    if budget == 0 {
        return adaptive.then_some(ThinkingConfig::Disabled);
    }
    if adaptive {
        return Some(ThinkingConfig::Adaptive);
    }
    // budget_tokens must be ≥ MIN and < max_tokens, or the API returns 400.
    let ceiling = max_tokens.checked_sub(1)?;
    if ceiling < MIN_BUDGET_TOKENS {
        return None;
    }
    Some(ThinkingConfig::Enabled {
        budget_tokens: budget.clamp(MIN_BUDGET_TOKENS, ceiling),
    })
}

/// Beta headers a thinking config needs.
pub fn thinking_betas(cfg: Option<&ThinkingConfig>) -> Vec<String> {
    match cfg {
        Some(ThinkingConfig::Enabled { .. }) => vec![INTERLEAVED_THINKING_BETA.to_string()],
        _ => Vec::new(),
    }
}

/// How to deliver `effort` to `model`, or `None` when unset or invalid.
pub fn effort_for(model: &str, effort: Option<&str>) -> Option<EffortWire> {
    let level = effort?.trim().to_ascii_lowercase();
    if !EFFORT_LEVELS.contains(&level.as_str()) {
        return None;
    }
    if supports_effort(model) {
        return Some(EffortWire::Param(OutputConfig { effort: level }));
    }
    Some(EffortWire::Prompt(effort_prompt(&level).to_string()))
}

/// Prompt-level substitute for models without `output_config.effort`.
pub fn effort_prompt(level: &str) -> &'static str {
    match level {
        "low" => {
            "Effort: low. Keep responses brief and fast. Prefer quick solutions over \
             thorough analysis. Skip detailed explanations unless asked."
        }
        "medium" => "Effort: medium. Balance thoroughness with efficiency.",
        _ => {
            "Effort: high. Be thorough, check edge cases, write comprehensive tests, \
             and explain your reasoning in detail."
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn json<T: Serialize>(v: &T) -> String {
        serde_json::to_string(v).unwrap()
    }

    // ── version parsing ──────────────────────────────────────────────────

    #[test]
    fn parses_modern_and_legacy_model_ids() {
        assert_eq!(model_version("claude-sonnet-4-6"), Some((4, 6)));
        assert_eq!(model_version("claude-sonnet-4-6-20250514"), Some((4, 6)));
        assert_eq!(model_version("claude-haiku-4-5-20251001"), Some((4, 5)));
        assert_eq!(model_version("claude-sonnet-5"), Some((5, 0)));
        assert_eq!(model_version("claude-fable-5-1"), Some((5, 1)));
        assert_eq!(model_version("claude-3-5-haiku-20241022"), Some((3, 5)));
        assert_eq!(model_version("claude-3-opus-20240229"), Some((3, 0)));
        assert_eq!(model_version("llama3.2"), None);
        assert_eq!(model_version("groq:llama-3.3-70b"), None);
    }

    // ── capability matrix (live-verified 2026-09-11) ─────────────────────

    #[test]
    fn adaptive_thinking_matches_the_live_capability_matrix() {
        for m in [
            "claude-sonnet-5",
            "claude-sonnet-4-6",
            "claude-opus-4-6",
            "claude-opus-4-7",
            "claude-opus-4-8",
            "claude-opus-5",
            "claude-fable-5-1",
            "claude-fable-5",
            "claude-mythos-5-1",
            "claude-haiku-5",
        ] {
            assert!(supports_adaptive_thinking(m), "{m} should be adaptive");
        }
        for m in [
            "claude-haiku-4-5",
            "claude-haiku-4-5-20251001",
            "claude-sonnet-4-5",
            "claude-opus-4-5",
            "claude-opus-4-1",
            "claude-3-5-haiku-20241022",
            "claude-3-opus-20240229",
            "llama3.2",
            "groq:llama-3.3-70b",
        ] {
            assert!(!supports_adaptive_thinking(m), "{m} should be budget-form");
        }
    }

    #[test]
    fn aliases_resolve_before_the_capability_check() {
        assert!(supports_adaptive_thinking("sonnet"));
        assert!(supports_adaptive_thinking("opus"));
        assert!(supports_adaptive_thinking("fable"));
        assert!(!supports_adaptive_thinking("haiku"));
    }

    #[test]
    fn effort_support_tracks_adaptive_thinking() {
        assert!(supports_effort("claude-sonnet-5"));
        assert!(supports_effort("claude-sonnet-4-6"));
        assert!(supports_effort("claude-opus-4-6"));
        assert!(!supports_effort("claude-haiku-4-5"));
        assert!(!supports_effort("llama3.2"));
    }

    // ── wire format ──────────────────────────────────────────────────────

    #[test]
    fn thinking_serialises_to_the_three_api_shapes() {
        assert_eq!(json(&ThinkingConfig::Adaptive), r#"{"type":"adaptive"}"#);
        assert_eq!(
            json(&ThinkingConfig::Enabled {
                budget_tokens: 2048
            }),
            r#"{"type":"enabled","budget_tokens":2048}"#
        );
        assert_eq!(json(&ThinkingConfig::Disabled), r#"{"type":"disabled"}"#);
    }

    #[test]
    fn output_config_serialises_effort() {
        assert_eq!(
            json(&OutputConfig {
                effort: "high".into()
            }),
            r#"{"effort":"high"}"#
        );
    }

    // ── thinking_for ─────────────────────────────────────────────────────

    #[test]
    fn claude_5_gets_adaptive_never_budget_tokens() {
        assert_eq!(
            thinking_for("claude-sonnet-5", Some(10_000), 64_000),
            Some(ThinkingConfig::Adaptive)
        );
        assert_eq!(
            thinking_for("claude-opus-5", Some(100), 64_000),
            Some(ThinkingConfig::Adaptive)
        );
    }

    #[test]
    fn haiku_keeps_the_budget_form() {
        assert_eq!(
            thinking_for("claude-haiku-4-5", Some(10_000), 64_000),
            Some(ThinkingConfig::Enabled {
                budget_tokens: 10_000
            })
        );
    }

    #[test]
    fn budget_is_clamped_to_the_api_minimum_and_below_max_tokens() {
        assert_eq!(
            thinking_for("claude-haiku-4-5", Some(100), 64_000),
            Some(ThinkingConfig::Enabled {
                budget_tokens: MIN_BUDGET_TOKENS
            })
        );
        assert_eq!(
            thinking_for("claude-haiku-4-5", Some(10_000), 4_096),
            Some(ThinkingConfig::Enabled {
                budget_tokens: 4_095
            })
        );
        // max_tokens too small for any legal budget: omit rather than 400.
        assert_eq!(thinking_for("claude-haiku-4-5", Some(10_000), 1_000), None);
    }

    #[test]
    fn zero_budget_disables_explicitly_on_adaptive_models_and_omits_elsewhere() {
        assert_eq!(
            thinking_for("claude-sonnet-5", Some(0), 64_000),
            Some(ThinkingConfig::Disabled)
        );
        assert_eq!(thinking_for("claude-haiku-4-5", Some(0), 64_000), None);
    }

    #[test]
    fn unset_budget_and_non_claude_models_send_nothing() {
        assert_eq!(thinking_for("claude-sonnet-5", None, 64_000), None);
        assert_eq!(thinking_for("llama3.2", Some(4_096), 64_000), None);
        assert_eq!(
            thinking_for("groq:llama-3.3-70b", Some(4_096), 64_000),
            None
        );
    }

    #[test]
    fn only_the_budget_form_needs_the_interleaved_beta() {
        assert_eq!(
            thinking_betas(Some(&ThinkingConfig::Enabled {
                budget_tokens: 2048
            })),
            vec![INTERLEAVED_THINKING_BETA.to_string()]
        );
        assert!(thinking_betas(Some(&ThinkingConfig::Adaptive)).is_empty());
        assert!(thinking_betas(Some(&ThinkingConfig::Disabled)).is_empty());
        assert!(thinking_betas(None).is_empty());
    }

    // ── effort_for ───────────────────────────────────────────────────────

    #[test]
    fn effort_is_a_parameter_on_supporting_models() {
        assert_eq!(
            effort_for("claude-sonnet-5", Some("high")),
            Some(EffortWire::Param(OutputConfig {
                effort: "high".into()
            }))
        );
    }

    #[test]
    fn effort_falls_back_to_a_prompt_nudge_elsewhere() {
        match effort_for("claude-haiku-4-5", Some("low")) {
            Some(EffortWire::Prompt(p)) => assert!(p.to_lowercase().contains("brief"), "{p}"),
            other => panic!("expected prompt fallback, got {other:?}"),
        }
        assert!(matches!(
            effort_for("llama3.2", Some("max")),
            Some(EffortWire::Prompt(_))
        ));
    }

    #[test]
    fn unset_or_invalid_effort_sends_nothing() {
        assert_eq!(effort_for("claude-sonnet-5", None), None);
        assert_eq!(effort_for("claude-sonnet-5", Some("ultra")), None);
        assert_eq!(effort_for("claude-sonnet-5", Some("")), None);
    }

    #[test]
    fn effort_levels_are_case_insensitive() {
        assert_eq!(
            effort_for("claude-sonnet-5", Some("HIGH")),
            Some(EffortWire::Param(OutputConfig {
                effort: "high".into()
            }))
        );
    }

    // ── request body ─────────────────────────────────────────────────────

    #[test]
    fn request_omits_output_config_until_effort_is_set() {
        use crate::api::types::{MessagesRequest, SystemContent};
        let mut req = MessagesRequest {
            model: "claude-sonnet-5".into(),
            max_tokens: 64,
            messages: vec![],
            system: SystemContent::Plain(String::new()),
            tools: vec![],
            stream: None,
            thinking: Some(ThinkingConfig::Adaptive),
            output_config: None,
            betas: vec![],
            session_id: None,
        };
        let body = json(&req);
        assert!(!body.contains("output_config"), "{body}");
        assert!(body.contains(r#""thinking":{"type":"adaptive"}"#), "{body}");
        req.output_config = Some(OutputConfig {
            effort: "low".into(),
        });
        assert!(json(&req).contains(r#""output_config":{"effort":"low"}"#));
    }

    /// End-to-end against the real API. Run with
    /// `ANTHROPIC_API_KEY=... cargo test --lib thinking::tests::live -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn live_thinking_and_effort_are_accepted_on_both_generations() {
        use crate::api::ClaudeClient;
        use crate::api::types::{ContentBlock, Message, MessagesRequest, Role, SystemContent};
        let key = std::env::var("ANTHROPIC_API_KEY").expect("ANTHROPIC_API_KEY");
        let client = ClaudeClient::new(key).unwrap();
        for model in ["claude-sonnet-5", "claude-haiku-4-5", "claude-sonnet-4-6"] {
            let thinking = thinking_for(model, Some(2048), 4096);
            let betas = thinking_betas(thinking.as_ref());
            let mut system = String::from("Reply with one word.");
            let output_config = match effort_for(model, Some("low")) {
                Some(EffortWire::Param(oc)) => Some(oc),
                Some(EffortWire::Prompt(p)) => {
                    system.push('\n');
                    system.push_str(&p);
                    None
                }
                None => None,
            };
            let req = MessagesRequest {
                model: model.into(),
                max_tokens: 4096,
                messages: vec![Message {
                    role: Role::User,
                    content: vec![ContentBlock::Text {
                        text: "Say hi".into(),
                    }],
                }],
                system: SystemContent::Plain(system),
                tools: vec![],
                stream: None,
                thinking,
                output_config,
                betas,
                session_id: None,
            };
            let resp = client
                .messages(req)
                .await
                .unwrap_or_else(|e| panic!("{model}: {e}"));
            assert!(
                resp.content
                    .iter()
                    .any(|b| matches!(b, ContentBlock::Text { .. })),
                "{model}: no text block"
            );
        }
    }
}
