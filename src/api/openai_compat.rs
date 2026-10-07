/// Generic OpenAI-compatible provider adapter.
///
/// One client that talks to ANY endpoint implementing the OpenAI
/// `/v1/chat/completions` API: OpenRouter, Groq, DeepSeek, Gemini, LM Studio,
/// llama.cpp, Together AI, Mistral, and hundreds more. OpenAI itself (`oai:`)
/// is served over the Responses API instead; see [`OpenAiApi`] and the
/// `responses` submodule.
///
/// Named provider shortcuts give convenient prefixes:
///
///   /model groq:llama-3.3-70b-versatile
///   /model openrouter:meta-llama/llama-3.3-70b-instruct
///   /model deepseek:deepseek-chat
///   /model gemini:gemini-2.5-flash
///   /model lmstudio:llama-3.2-3b-instruct
///   /model together:meta-llama/Llama-3.3-70b-chat-hf
///   /model mistral:codestral-latest
///   /model oai:gpt-4o                          (real OpenAI)
///   /model openai-compat:my-model              (generic, needs OPENAI_BASE_URL)
///
/// Provider API keys come from environment variables:
///   GROQ_API_KEY, OPENROUTER_API_KEY, DEEPSEEK_API_KEY, etc.
///   Gemini reads GEMINI_API_KEY, then GOOGLE_API_KEY.
///   OPENAI_API_KEY is used only by `oai:` and `openai-compat:`; named
///   third-party providers require their own key variable.
///   OPENAI_BASE_URL overrides the base URL for the generic `openai-compat:` prefix.
use anyhow::{Context, Result, anyhow};
use eventsource_stream::Eventsource;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use tracing::{debug, warn};

use crate::api::types::*;

pub(crate) mod responses;

// ─── Provider registry ───────────────────────────────────────────────────────

/// A known provider with its default base URL and API key env var.
#[derive(Debug, Clone)]
pub struct ProviderDef {
    /// Short prefix used in `/model <prefix>:<model>` (e.g. "groq")
    pub prefix: &'static str,
    /// Human-friendly display name
    pub name: &'static str,
    /// Default base URL (the `/v1/chat/completions` path is appended)
    pub base_url: &'static str,
    /// Env var name for the API key (e.g. "GROQ_API_KEY")
    pub key_env: &'static str,
    /// Optional extra HTTP headers (e.g. OpenRouter requires HTTP-Referer)
    pub extra_headers: &'static [(&'static str, &'static str)],
}

/// All known providers. Order matters for display in `/model` help.
pub static PROVIDERS: &[ProviderDef] = &[
    ProviderDef {
        prefix: "groq",
        name: "Groq",
        base_url: "https://api.groq.com/openai/v1",
        key_env: "GROQ_API_KEY",
        extra_headers: &[],
    },
    ProviderDef {
        prefix: "openrouter",
        name: "OpenRouter",
        base_url: "https://openrouter.ai/api/v1",
        key_env: "OPENROUTER_API_KEY",
        extra_headers: &[
            ("HTTP-Referer", "https://github.com/ForkedInTime/OxideClaw"),
            ("X-Title", "OxideClaw"),
        ],
    },
    ProviderDef {
        prefix: "deepseek",
        name: "DeepSeek",
        base_url: "https://api.deepseek.com/v1",
        key_env: "DEEPSEEK_API_KEY",
        extra_headers: &[],
    },
    // Google's OpenAI-compatible surface of the Gemini API. The key also
    // comes from GOOGLE_API_KEY; see `provider_api_key`.
    ProviderDef {
        prefix: "gemini",
        name: "Gemini",
        base_url: "https://generativelanguage.googleapis.com/v1beta/openai",
        key_env: "GEMINI_API_KEY",
        extra_headers: &[],
    },
    ProviderDef {
        prefix: "lmstudio",
        name: "LM Studio",
        base_url: "http://localhost:1234/v1",
        key_env: "",
        extra_headers: &[],
    },
    ProviderDef {
        prefix: "together",
        name: "Together AI",
        base_url: "https://api.together.xyz/v1",
        key_env: "TOGETHER_API_KEY",
        extra_headers: &[],
    },
    ProviderDef {
        prefix: "mistral",
        name: "Mistral",
        base_url: "https://api.mistral.ai/v1",
        key_env: "MISTRAL_API_KEY",
        extra_headers: &[],
    },
    ProviderDef {
        prefix: "venice",
        name: "Venice.ai",
        base_url: "https://api.venice.ai/api/v1",
        key_env: "VENICE_API_KEY",
        extra_headers: &[],
    },
    ProviderDef {
        prefix: "oai",
        name: "OpenAI",
        base_url: "https://api.openai.com/v1",
        key_env: "OPENAI_API_KEY",
        extra_headers: &[],
    },
    // Generic escape hatch — user MUST set OPENAI_BASE_URL
    ProviderDef {
        prefix: "openai-compat",
        name: "OpenAI-compatible",
        base_url: "",
        key_env: "OPENAI_API_KEY",
        extra_headers: &[],
    },
];

/// Check if a model string uses any known provider prefix.
pub fn is_openai_compat_model(model: &str) -> bool {
    if let Some((prefix, _)) = model.split_once(':') {
        PROVIDERS.iter().any(|p| p.prefix == prefix)
    } else {
        false
    }
}

/// Split "prefix:model_name" → (ProviderDef, bare_model_name).
/// Returns None if the prefix doesn't match a known provider.
pub fn parse_provider_model(model: &str) -> Option<(&'static ProviderDef, &str)> {
    let (prefix, bare) = model.split_once(':')?;
    let provider = PROVIDERS.iter().find(|p| p.prefix == prefix)?;
    Some((provider, bare))
}

/// Which OpenAI API the `oai:` and `openai-compat:` prefixes speak
/// (`openaiApi` in settings.json, `OXIDECLAW_OPENAI_API` in the shell).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum OpenAiApi {
    /// Responses for `oai:` (always the official https://api.openai.com/v1),
    /// Chat Completions everywhere else.
    #[default]
    Auto,
    /// Chat Completions for every provider.
    Chat,
    /// Responses for `oai:` and for the endpoints whose base URL the user
    /// sets (`openai-compat:`, `lmstudio:`), which must then serve
    /// `/responses`. The named cloud presets keep Chat Completions.
    Responses,
}

impl OpenAiApi {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(Self::Auto),
            "chat" => Some(Self::Chat),
            "responses" => Some(Self::Responses),
            _ => None,
        }
    }
}

/// Whether requests for `model` go to `/responses` rather than
/// `/chat/completions`. Named third-party presets (Groq, Gemini, DeepSeek,
/// ...) always keep Chat Completions.
pub fn uses_responses_api(model: &str, api: OpenAiApi) -> bool {
    let Some((provider, _)) = parse_provider_model(model) else {
        return false;
    };
    match api {
        OpenAiApi::Chat => false,
        OpenAiApi::Auto => provider.prefix == "oai",
        OpenAiApi::Responses => matches!(provider.prefix, "oai" | "openai-compat" | "lmstudio"),
    }
}

/// An OpenAI reasoning model reached over the Responses API: the requests
/// that take `reasoning.effort` and return reasoning items.
pub fn responses_reasoning_model(model: &str, api: OpenAiApi) -> bool {
    uses_responses_api(model, api)
        && parse_provider_model(model).is_some_and(|(_, bare)| responses::is_reasoning_model(bare))
}

/// List known provider prefixes — used in /model help text.
// ─── Shared OpenAI wire types ────────────────────────────────────────────────

#[derive(Serialize)]
pub(crate) struct OaiMessage {
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<OaiToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
}

#[derive(Serialize, Deserialize, Clone)]
pub(crate) struct OaiToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub call_type: String,
    pub function: OaiFunction,
    /// Gemini's `{"google": {"thought_signature": ...}}`, sent back on the
    /// calls it came with; see [`gemini_extra_content`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra_content: Option<serde_json::Value>,
}

#[derive(Serialize, Deserialize, Clone)]
pub(crate) struct OaiFunction {
    pub name: String,
    pub arguments: String,
}

#[derive(Serialize)]
pub(crate) struct OaiTool {
    #[serde(rename = "type")]
    pub tool_type: String,
    pub function: OaiFunctionDef,
}

#[derive(Serialize)]
pub(crate) struct OaiFunctionDef {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

/// deepseek-chat's own output limit. Its default (4096) cut long Write/Edit
/// calls short, so this is sent even when `maxTokens` is unset.
const DEEPSEEK_CHAT_MAX_TOKENS: u32 = 8_192;

#[derive(Serialize)]
pub(crate) struct OaiRequest {
    pub model: String,
    pub messages: Vec<OaiMessage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<OaiTool>,
    pub stream: bool,
    pub stream_options: Option<OaiStreamOptions>,
    /// Output cap for most servers, sent only when the user set `maxTokens` /
    /// `maxTokensByModel` (and for deepseek-chat); otherwise each provider
    /// applies its own default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// OpenAI's own API rejects `max_tokens` on its reasoning models (o-series,
    /// gpt-5) and wants this name instead; every model there accepts it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_completion_tokens: Option<u32>,
}

#[derive(Serialize)]
pub(crate) struct OaiStreamOptions {
    pub include_usage: bool,
}

// ── Streaming response chunks ────────────────────────────────────────────────

#[derive(Deserialize)]
pub(crate) struct OaiChunk {
    // Absent on error and usage-only chunks from some servers.
    #[serde(default)]
    pub choices: Vec<OaiChoice>,
    #[serde(default)]
    pub usage: Option<OaiUsage>,
}

#[derive(Deserialize)]
pub(crate) struct OaiChoice {
    pub delta: OaiDelta,
    pub finish_reason: Option<String>,
}

#[derive(Deserialize, Default)]
pub(crate) struct OaiDelta {
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub tool_calls: Option<Vec<OaiToolCallDelta>>,
    /// DeepSeek R1 / QwQ reasoning content
    #[serde(default)]
    pub reasoning_content: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct OaiToolCallDelta {
    /// Required by the OpenAI spec, but some compat servers (Gemini's among
    /// them) omit it; a hard field failed the whole chunk and lost the call.
    #[serde(default)]
    pub index: Option<usize>,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub function: Option<OaiFunctionDelta>,
    /// Gemini puts the call's thought signature here.
    #[serde(default)]
    pub extra_content: Option<serde_json::Value>,
}

#[derive(Deserialize)]
pub(crate) struct OaiFunctionDelta {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub arguments: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct OaiUsage {
    /// Every prompt token, prompt-cache hits included.
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    /// OpenAI (also Groq, OpenRouter, Gemini): `cached_tokens` is the part
    /// of `prompt_tokens` read from the prompt cache.
    #[serde(default)]
    pub prompt_tokens_details: Option<OaiPromptTokensDetails>,
    /// DeepSeek: the part of `prompt_tokens` that hit its context cache
    /// (the rest is `prompt_cache_miss_tokens`).
    #[serde(default)]
    pub prompt_cache_hit_tokens: Option<u64>,
    /// Gemini's thinking tokens are billed as output but left out of
    /// `completion_tokens`; `total_tokens` counts them.
    #[serde(default)]
    pub total_tokens: Option<u64>,
}

#[derive(Deserialize)]
pub(crate) struct OaiPromptTokensDetails {
    #[serde(default)]
    pub cached_tokens: Option<u64>,
}

impl From<OaiUsage> for Usage {
    /// `prompt_tokens` counts cache hits; `Usage` keeps them apart (as the
    /// Anthropic API does) so cost prices them at the cached rate.
    fn from(u: OaiUsage) -> Self {
        let cached = u
            .prompt_cache_hit_tokens
            .or(u.prompt_tokens_details.and_then(|d| d.cached_tokens))
            .unwrap_or(0)
            .min(u.prompt_tokens);
        // Everything past the prompt is billed as output: the same as
        // `completion_tokens` where total = prompt + completion (OpenAI,
        // DeepSeek, Groq), plus the thinking where it is not (Gemini).
        let output = u.total_tokens.map_or(u.completion_tokens, |t| {
            u.completion_tokens.max(t.saturating_sub(u.prompt_tokens))
        });
        Usage {
            input_tokens: u.prompt_tokens - cached,
            output_tokens: output,
            cache_read_input_tokens: cached,
            cache_creation_input_tokens: 0,
        }
    }
}

// ─── Shared translation: Anthropic ↔ OpenAI ─────────────────────────────────

/// Translate Anthropic `Message`s into OpenAI-format messages.
/// System prompt is handled separately (passed as role:system first message).
///
/// `echo_reasoning` sends an assistant turn's own (unsigned) reasoning back as
/// `reasoning_content`. DeepSeek thinking models return 400 on a tool-call
/// follow-up without it; other providers reject the unknown field, so it is
/// per provider rather than always on (history survives `/model` switches).
///
/// `mistral_tool_ids` rewrites tool-call ids through [`mistral_tool_id`].
///
/// `thought_signatures` is set for Gemini only: the signature each tool call
/// came back with, by call id; see [`gemini_extra_content`].
pub(crate) fn translate_messages(
    system: &str,
    messages: &[Message],
    echo_reasoning: bool,
    mistral_tool_ids: bool,
    thought_signatures: Option<&HashMap<String, String>>,
) -> Vec<OaiMessage> {
    let mut out = Vec::with_capacity(messages.len() + 1);
    let tool_id = |id: &str| {
        if mistral_tool_ids {
            mistral_tool_id(id)
        } else {
            id.to_string()
        }
    };

    if !system.is_empty() {
        out.push(OaiMessage {
            role: "system".into(),
            content: Some(serde_json::Value::String(system.to_string())),
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
        });
    }

    for msg in messages {
        match msg.role {
            Role::User => {
                let text_blocks: Vec<&ContentBlock> = msg
                    .content
                    .iter()
                    .filter(|b| matches!(b, ContentBlock::Text { .. }))
                    .collect();
                let result_blocks: Vec<&ContentBlock> = msg
                    .content
                    .iter()
                    .filter(|b| matches!(b, ContentBlock::ToolResult { .. }))
                    .collect();

                // `tool` messages must directly follow the assistant's
                // tool_calls, so any user text (auto-fix feedback) goes after.
                for block in result_blocks {
                    if let ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        ..
                    } = block
                    {
                        let text = content
                            .iter()
                            .map(|c| {
                                let ToolResultContent::Text { text } = c;
                                text.as_str()
                            })
                            .collect::<Vec<_>>()
                            .join("\n");
                        out.push(OaiMessage {
                            role: "tool".into(),
                            content: Some(serde_json::Value::String(text)),
                            tool_calls: None,
                            tool_call_id: Some(tool_id(tool_use_id)),
                            reasoning_content: None,
                        });
                    }
                }

                // `/image` attachments go as OpenAI content parts. A model
                // without vision answers 400, and the TUI then drops the
                // image so it is not re-sent every turn.
                let image_parts: Vec<serde_json::Value> = msg
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::Image { source } => {
                            let url = match source {
                                ImageSource::Base64 { media_type, data } => {
                                    format!("data:{media_type};base64,{data}")
                                }
                                ImageSource::Url { url } => url.clone(),
                            };
                            Some(serde_json::json!({
                                "type": "image_url",
                                "image_url": { "url": url },
                            }))
                        }
                        _ => None,
                    })
                    .collect();

                if !text_blocks.is_empty() || !image_parts.is_empty() {
                    let text = text_blocks
                        .iter()
                        .filter_map(|b| {
                            if let ContentBlock::Text { text } = b {
                                Some(text.as_str())
                            } else {
                                None
                            }
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    let content = if image_parts.is_empty() {
                        serde_json::Value::String(text)
                    } else {
                        let mut parts = Vec::with_capacity(image_parts.len() + 1);
                        if !text.is_empty() {
                            parts.push(serde_json::json!({ "type": "text", "text": text }));
                        }
                        parts.extend(image_parts);
                        serde_json::Value::Array(parts)
                    };
                    out.push(OaiMessage {
                        role: "user".into(),
                        content: Some(content),
                        tool_calls: None,
                        tool_call_id: None,
                        reasoning_content: None,
                    });
                }
            }

            Role::Assistant => {
                let mut text_parts: Vec<&str> = Vec::new();
                let mut reasoning_parts: Vec<&str> = Vec::new();
                let mut tool_calls: Vec<OaiToolCall> = Vec::new();

                for block in &msg.content {
                    match block {
                        ContentBlock::Text { text } => text_parts.push(text.as_str()),
                        ContentBlock::ToolUse { id, name, input } => {
                            let extra_content = thought_signatures.and_then(|sigs| {
                                gemini_extra_content(sigs.get(id), tool_calls.is_empty())
                            });
                            tool_calls.push(OaiToolCall {
                                id: tool_id(id),
                                call_type: "function".into(),
                                function: OaiFunction {
                                    name: name.clone(),
                                    arguments: serde_json::to_string(input)
                                        .unwrap_or_else(|_| "{}".into()),
                                },
                                extra_content,
                            });
                        }
                        // A signed block is Anthropic's own thinking, not
                        // this provider's reasoning: never echo it.
                        ContentBlock::Thinking {
                            thinking,
                            signature,
                        } if signature.is_empty() => reasoning_parts.push(thinking.as_str()),
                        ContentBlock::Thinking { .. }
                        | ContentBlock::RedactedThinking { .. }
                        | ContentBlock::ToolResult { .. }
                        | ContentBlock::Image { .. } => {}
                    }
                }

                // OpenAI and Groq require content unless tool_calls is set; a
                // thinking-only turn (reasoning cut off by max_tokens) would
                // otherwise go out bare and 400 every later request.
                let content = if !text_parts.is_empty() {
                    Some(serde_json::Value::String(text_parts.join("\n")))
                } else if tool_calls.is_empty() {
                    Some(serde_json::Value::String(String::new()))
                } else {
                    None
                };

                let reasoning_content = if !echo_reasoning {
                    None
                } else if !reasoning_parts.is_empty() {
                    Some(reasoning_parts.join("\n"))
                } else if !tool_calls.is_empty() {
                    // Tool calls made by another model have no reasoning to
                    // echo, and DeepSeek rejects both a missing and an empty
                    // value there.
                    Some(" ".into())
                } else {
                    None
                };

                out.push(OaiMessage {
                    role: "assistant".into(),
                    content,
                    tool_calls: if tool_calls.is_empty() {
                        None
                    } else {
                        Some(tool_calls)
                    },
                    tool_call_id: None,
                    reasoning_content,
                });
            }
        }
    }

    out
}

/// Gemini 3 answers 400 when a function call in the current turn comes back
/// without the thought signature it was sent with. Google's OpenAI-compatible
/// surface carries it as `extra_content.google.thought_signature`, on the
/// first call of each step only, so that is where it goes back. A first call
/// with no signature on record (made by another model, before a `/model`
/// switch or resume, or evicted) gets the documented placeholder that skips
/// the check; later parallel calls never carried one.
const GEMINI_SKIP_SIGNATURE: &str = "skip_thought_signature_validator";

fn gemini_extra_content(signature: Option<&String>, first_call: bool) -> Option<serde_json::Value> {
    let signature = signature
        .map(String::as_str)
        .or(first_call.then_some(GEMINI_SKIP_SIGNATURE))?;
    Some(serde_json::json!({ "google": { "thought_signature": signature } }))
}

/// A tool-call delta's Gemini thought signature, if it carries one.
fn delta_thought_signature(extra_content: Option<&serde_json::Value>) -> Option<&str> {
    extra_content?
        .pointer("/google/thought_signature")?
        .as_str()
        .filter(|s| !s.is_empty())
}

/// Mistral rejects (400) any tool-call id that is not exactly 9 ASCII
/// alphanumerics, and a history from Claude (`toolu_…`) or another provider
/// (`call_…`) carries longer ones after a `/model` switch or resume. The
/// mapping is a pure function of the id, so each assistant tool call and its
/// tool result still pair up; stored history keeps the original ids.
fn mistral_tool_id(id: &str) -> String {
    if id.len() == 9 && id.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return id.to_string();
    }
    let mut h = fnv1a(id);
    const BASE62: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
    (0..9)
        .map(|_| {
            let c = BASE62[(h % 62) as usize] as char;
            h /= 62;
            c
        })
        .collect()
}

/// FNV-1a: stable across builds and platforms, unlike `DefaultHasher`.
fn fnv1a(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

/// Translate Anthropic `ToolDefinition`s to OpenAI tool format.
pub(crate) fn translate_tools(tools: &[ToolDefinition]) -> Vec<OaiTool> {
    tools
        .iter()
        .map(|t| OaiTool {
            tool_type: "function".into(),
            function: OaiFunctionDef {
                name: t.name.clone(),
                description: t.description.clone(),
                parameters: t.input_schema.clone(),
            },
        })
        .collect()
}

/// Extract a flat system string from SystemContent.
pub(crate) fn system_to_string(system: &SystemContent) -> String {
    match system {
        SystemContent::Plain(s) => s.clone(),
        SystemContent::Blocks(blocks) => blocks
            .iter()
            .map(|b| b.text.as_str())
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

/// Patch a system prompt for text-only mode (no tools).
pub(crate) fn patch_system_no_tools(system: &str) -> String {
    let patched: String = system
        .lines()
        .filter(|l| !l.contains("You have access to tools") && !l.contains("Use tools to actually"))
        .collect::<Vec<_>>()
        .join("\n");
    format!("{patched}\n- Text-only mode: answer from knowledge, no file/command access.")
}

/// The message of a mid-stream error chunk, if `chunk` is one. OpenAI,
/// Ollama and OpenRouter send `{"error": {...}}` (OpenRouter alongside
/// `finish_reason: "error"`); vLLM sends `{"object": "error", "message": ...}`.
/// Providers have already answered 200 by then, so this is the only place
/// the failure shows up.
fn chunk_error(chunk: &serde_json::Value) -> Option<String> {
    let err = match chunk.get("error") {
        Some(e) if !e.is_null() => e,
        _ if chunk.get("object").and_then(|o| o.as_str()) == Some("error") => chunk,
        _ => return None,
    };
    let msg = err
        .get("message")
        .and_then(|m| m.as_str())
        .or_else(|| err.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| err.to_string());
    Some(msg)
}

/// Parse an SSE stream of OpenAI-format chunks into a StreamedResponse.
/// Shared between OllamaClient and OpenAiCompatClient.
///
/// A provider failure after the 200 is an `Err`, never a short reply that
/// looks finished: the caller would otherwise save a truncated answer, or
/// run a tool whose cut-off arguments parsed to `{}`.
///
/// Also returns the Gemini thought signatures that came with tool calls, by
/// call id (empty for every other server).
pub(crate) async fn parse_oai_stream(
    resp: reqwest::Response,
    mut on_text: impl FnMut(&str),
) -> Result<(StreamedResponse, HashMap<String, String>)> {
    let mut stream = super::idle_bounded(resp.bytes_stream()).eventsource();
    let mut result = StreamedResponse::default();

    let mut text_buf = String::new();
    let mut thinking_buf = String::new();
    let mut tool_bufs: HashMap<usize, (String, String, String)> = HashMap::new();
    let mut tool_signatures: HashMap<usize, String> = HashMap::new();
    let mut last_tool_idx: Option<usize> = None;
    let mut finish_reason: Option<String> = None;
    let mut saw_done = false;

    while let Some(event) = super::next_sse_event(&mut stream).await? {
        if event.data == "[DONE]" {
            saw_done = true;
            break;
        }

        let chunk: OaiChunk = match serde_json::from_str::<serde_json::Value>(&event.data) {
            Ok(v) => {
                if let Some(msg) = chunk_error(&v) {
                    return Err(anyhow!("provider stream error: {msg}"));
                }
                match serde_json::from_value(v) {
                    Ok(c) => c,
                    Err(e) => {
                        warn!("Failed to parse SSE chunk: {e}: {}", event.data);
                        continue;
                    }
                }
            }
            Err(e) => {
                warn!("Failed to parse SSE chunk: {e}: {}", event.data);
                continue;
            }
        };

        if let Some(usage) = chunk.usage {
            result.usage = usage.into();
        }

        for choice in chunk.choices {
            if let Some(fr) = choice.finish_reason {
                finish_reason = Some(fr);
            }

            let delta = choice.delta;

            // Text delta
            if let Some(text) = delta.content
                && !text.is_empty()
            {
                on_text(&text);
                text_buf.push_str(&text);
            }

            // DeepSeek R1 / QwQ reasoning content → Thinking block
            if let Some(reasoning) = delta.reasoning_content
                && !reasoning.is_empty()
            {
                thinking_buf.push_str(&reasoning);
            }

            // Tool call deltas
            if let Some(tc_deltas) = delta.tool_calls {
                for tc in tc_deltas {
                    // Without an index, a new id starts a new call and an
                    // id-less delta continues the previous one.
                    let idx = match (tc.index, last_tool_idx) {
                        (Some(i), _) => i,
                        (None, Some(last))
                            if tc.id.as_deref().is_none_or(|id| {
                                tool_bufs
                                    .get(&last)
                                    .is_none_or(|e| e.0.is_empty() || e.0 == id)
                            }) =>
                        {
                            last
                        }
                        (None, _) => tool_bufs.keys().max().map_or(0, |m| m + 1),
                    };
                    last_tool_idx = Some(idx);
                    let entry = tool_bufs
                        .entry(idx)
                        .or_insert_with(|| (String::new(), String::new(), String::new()));

                    if let Some(id) = tc.id {
                        entry.0 = id;
                    }
                    if let Some(sig) = delta_thought_signature(tc.extra_content.as_ref()) {
                        tool_signatures.insert(idx, sig.to_string());
                    }
                    if let Some(func) = tc.function {
                        if let Some(name) = func.name {
                            entry.1 = name;
                        }
                        if let Some(args) = func.arguments {
                            entry.2.push_str(&args);
                        }
                    }
                }
            }
        }
    }

    if finish_reason.as_deref() == Some("error") {
        return Err(anyhow!(
            "provider stream error: the reply ended with finish_reason \"error\""
        ));
    }
    // A clean close with neither marker is a dropped connection (proxy or
    // server restart), not a finished reply.
    if !saw_done && finish_reason.is_none() {
        return Err(anyhow!(
            "provider stream ended before the reply finished (no finish_reason or [DONE])"
        ));
    }

    // ── Assemble final ContentBlocks ─────────────────────────────────────────

    // Thinking block from reasoning_content (DeepSeek R1, QwQ, etc.)
    if !thinking_buf.is_empty() {
        result.content.push(ContentBlock::Thinking {
            thinking: thinking_buf,
            signature: String::new(),
        });
    }

    if !text_buf.is_empty() {
        result.content.push(ContentBlock::Text { text: text_buf });
    }

    let mut tool_entries: Vec<(usize, (String, String, String))> = tool_bufs.into_iter().collect();
    tool_entries.sort_by_key(|(idx, _)| *idx);
    let signatures: HashMap<String, String> = tool_entries
        .iter()
        .filter(|(_, (id, _, _))| !id.is_empty())
        .filter_map(|(idx, (id, _, _))| Some((id.clone(), tool_signatures.remove(idx)?)))
        .collect();
    // Some servers (older Ollama, llama.cpp, vLLM) finish with "stop" even
    // when the turn is tool calls. The calls are the turn either way.
    let has_tool_calls = tool_entries
        .iter()
        .any(|(_, (_, name, _))| !name.is_empty());

    for (_, (id, name, args)) in tool_entries {
        let input =
            serde_json::from_str(&args).unwrap_or(serde_json::Value::Object(Default::default()));
        result
            .content
            .push(ContentBlock::ToolUse { id, name, input });
    }

    // ── Map finish_reason → StopReason ───────────────────────────────────────

    result.stop_reason = match finish_reason.as_deref() {
        Some("length") => Some(StopReason::MaxTokens),
        Some("content_filter") => Some(StopReason::Refusal),
        _ if has_tool_calls => Some(StopReason::ToolUse),
        Some("tool_calls") | Some("function_call") => Some(StopReason::ToolUse),
        Some("stop_sequence") => Some(StopReason::StopSequence),
        _ => Some(StopReason::EndTurn),
    };

    Ok((result, signatures))
}

// ─── OpenAI-compatible client ────────────────────────────────────────────────

#[derive(Clone)]
pub struct OpenAiCompatClient {
    client: Client,
    pub base_url: String,
    api_key: String,
    pub provider_name: String,
    extra_headers: Vec<(String, String)>,
    /// Set to true after the first 400 "does not support tools" error.
    no_tools: Arc<AtomicBool>,
    tools_notice_sent: Arc<AtomicBool>,
    /// Responses API: set after OpenAI refuses reasoning summaries (an
    /// organization that is not verified); later requests leave them out.
    no_summary: Arc<AtomicBool>,
    summary_notice_sent: Arc<AtomicBool>,
    /// See [`translate_messages`]: only DeepSeek wants reasoning echoed back.
    echo_reasoning: bool,
    /// See [`mistral_tool_id`].
    mistral_tool_ids: bool,
    /// Gemini only: the thought signatures of recent tool calls, by call id,
    /// newest last; see [`gemini_extra_content`]. Shared by the clones each
    /// turn runs on, and capped at [`MAX_THOUGHT_SIGNATURES`].
    thought_signatures: Option<Arc<Mutex<SignatureStore>>>,
    /// Set when this client speaks the Responses API (see
    /// [`uses_responses_api`]): the reasoning items of recent turns, replayed
    /// in the next request's input. Shared by the clones each turn runs on.
    responses: Option<Arc<Mutex<responses::TurnStore>>>,
    /// See `ClaudeClient::retry_notifier`. Rate limiting is far more common on
    /// these providers than on Anthropic — Groq and OpenRouter throttle hard.
    retry_notifier: Option<super::retry::RetryNotifier>,
}

/// Only the current turn's signatures are checked, so the most recent calls
/// are all that matter; the cap keeps a long session from growing without
/// bound.
const MAX_THOUGHT_SIGNATURES: usize = 256;

/// Gemini thought signatures as (tool call id, signature), oldest first.
type SignatureStore = VecDeque<(String, String)>;

/// Adds `new` to `store`, dropping the oldest past [`MAX_THOUGHT_SIGNATURES`].
fn remember_signatures(store: &mut SignatureStore, new: HashMap<String, String>) {
    store.extend(new);
    let excess = store.len().saturating_sub(MAX_THOUGHT_SIGNATURES);
    store.drain(..excess);
}

/// Gemini's second key variable: the name Google's SDKs and Gemini CLI also
/// read. GEMINI_API_KEY wins when both are set.
const GEMINI_FALLBACK_KEY_ENV: &str = "GOOGLE_API_KEY";

/// The key variables `provider` reads, in order.
pub(crate) fn provider_key_envs(provider: &ProviderDef) -> Vec<&'static str> {
    match provider.key_env {
        "" => vec![],
        k if provider.prefix == "gemini" => vec![k, GEMINI_FALLBACK_KEY_ENV],
        k => vec![k],
    }
}

/// The bearer token for `provider`, read only from its own key variables.
/// There is deliberately no fallback to OPENAI_API_KEY: that would hand the
/// user's OpenAI key to Groq, DeepSeek, OpenRouter, etc. whenever their own
/// variable is unset. Local providers (LM Studio) have no key variable.
fn provider_api_key(provider: &ProviderDef, env: impl Fn(&str) -> Option<String>) -> String {
    provider_key_envs(provider)
        .into_iter()
        .filter_map(env)
        .find(|v| !v.is_empty())
        .unwrap_or_default()
}

/// The provider, base URL and key `model` would use, or why it cannot be
/// used: an unknown prefix, `openai-compat:` without `OPENAI_BASE_URL`, or a
/// cloud provider without its key variable.
fn resolve_endpoint(
    model: &str,
    env: impl Fn(&str) -> Option<String>,
) -> Result<(&'static ProviderDef, String, String)> {
    let (provider, _bare) = parse_provider_model(model)
        .ok_or_else(|| anyhow!("Unknown provider prefix in '{model}'"))?;

    // Resolve base URL
    let base_url = if provider.prefix == "openai-compat" {
        // Generic escape hatch: MUST have OPENAI_BASE_URL set
        env("OPENAI_BASE_URL").ok_or_else(|| {
            anyhow!(
                "openai-compat: requires OPENAI_BASE_URL env var.\n\
                     Set it to your endpoint, e.g.:\n  \
                     export OPENAI_BASE_URL=http://localhost:8080/v1"
            )
        })?
    } else if provider.prefix == "lmstudio" {
        // LM Studio: allow override via LM_STUDIO_HOST
        env("LM_STUDIO_HOST").unwrap_or_else(|| provider.base_url.to_string())
    } else {
        provider.base_url.to_string()
    };
    // Requests append "/chat/completions" (or "/responses"); a trailing
    // slash made "//" and a 404 on servers that route on the exact path.
    let base_url = base_url.trim_end_matches('/').to_string();

    let api_key = provider_api_key(provider, &env);

    // Warn if cloud provider has no key (local providers are fine without).
    // openai-compat is whatever endpoint the user pointed it at, often a
    // keyless vLLM/llama.cpp box on the LAN; no key just means no
    // Authorization header.
    if api_key.is_empty()
        && !provider.key_env.is_empty()
        && provider.prefix != "openai-compat"
        && !base_url.starts_with("http://localhost")
        && !base_url.starts_with("http://127.0.0.1")
    {
        return Err(anyhow!(
            "{}: no API key found.\n  Set {} in your environment.\n  \
                 Example: export {}=your-key-here",
            provider.name,
            provider_key_envs(provider).join(" or "),
            provider.key_env
        ));
    }
    Ok((provider, base_url, api_key))
}

/// Whether `model`'s provider has what it needs to be called (base URL and
/// key), without building a client.
pub fn has_credential(model: &str) -> bool {
    resolve_endpoint(model, |k| std::env::var(k).ok()).is_ok()
}

impl OpenAiCompatClient {
    /// Create a client for a specific provider prefix + model string.
    /// Resolves base_url from the provider registry and API key from env vars.
    /// `api` picks Chat Completions or Responses; see [`uses_responses_api`].
    pub fn from_model(model: &str, api: OpenAiApi) -> Result<Self> {
        Self::from_model_env(model, api, |k| std::env::var(k).ok())
    }

    fn from_model_env(
        model: &str,
        api: OpenAiApi,
        env: impl Fn(&str) -> Option<String>,
    ) -> Result<Self> {
        let (provider, base_url, api_key) = resolve_endpoint(model, env)?;

        let extra_headers: Vec<(String, String)> = provider
            .extra_headers
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();

        let client = Client::builder()
            // 10s connect timeout — fail fast on dead upstreams. Don't set
            // an overall timeout here because legitimate long streams would
            // be killed mid-response.
            .connect_timeout(std::time::Duration::from_secs(10))
            .build()
            .context("Failed to build HTTP client")?;

        Ok(Self {
            retry_notifier: None,
            client,
            base_url,
            api_key,
            provider_name: provider.name.to_string(),
            extra_headers,
            no_tools: Arc::new(AtomicBool::new(false)),
            tools_notice_sent: Arc::new(AtomicBool::new(false)),
            no_summary: Arc::new(AtomicBool::new(false)),
            summary_notice_sent: Arc::new(AtomicBool::new(false)),
            echo_reasoning: provider.prefix == "deepseek",
            mistral_tool_ids: provider.prefix == "mistral",
            thought_signatures: (provider.prefix == "gemini").then(Default::default),
            responses: uses_responses_api(model, api).then(Default::default),
        })
    }

    /// A POST to `url` with this provider's auth and extra headers.
    fn post(&self, url: &str, body: &impl Serialize) -> reqwest::RequestBuilder {
        let mut builder = self.client.post(url).json(body);
        if !self.api_key.is_empty() {
            builder = builder.bearer_auth(&self.api_key);
        }
        // Provider-specific headers (e.g. OpenRouter's HTTP-Referer)
        for (k, v) in &self.extra_headers {
            builder = builder.header(k.as_str(), v.as_str());
        }
        builder
    }

    #[allow(dead_code)]
    pub fn tools_disabled(&self) -> bool {
        self.no_tools.load(Ordering::Relaxed)
    }

    /// See `ClaudeClient::set_retry_notifier`.
    pub fn set_retry_notifier(&mut self, n: super::retry::RetryNotifier) {
        self.retry_notifier = Some(n);
    }

    pub fn take_tools_notice(&self) -> bool {
        self.no_tools.load(Ordering::Relaxed)
            && !self.tools_notice_sent.swap(true, Ordering::Relaxed)
    }

    /// True once, after reasoning summaries were refused and turned off.
    pub fn take_summary_notice(&self) -> bool {
        self.no_summary.load(Ordering::Relaxed)
            && !self.summary_notice_sent.swap(true, Ordering::Relaxed)
    }

    /// Streaming call — drop-in replacement for ClaudeClient::messages_stream.
    pub async fn messages_stream(
        &self,
        request: MessagesRequest,
        on_text: impl FnMut(&str),
    ) -> Result<StreamedResponse> {
        if let Some(store) = &self.responses {
            return self.responses_stream(request, on_text, store).await;
        }
        let (prefix, bare_model) = request
            .model
            .split_once(':')
            .unwrap_or(("", &request.model));
        let model = bare_model.to_string();
        let url = format!("{}/chat/completions", self.base_url);
        debug!(
            "POST {url} model={model} (via {}, prefix={prefix})",
            self.provider_name
        );

        let no_tools = self.no_tools.load(Ordering::Relaxed);
        let system_str = system_to_string(&request.system);

        let system = if no_tools {
            patch_system_no_tools(&system_str)
        } else {
            system_str.clone()
        };

        let signatures: Option<HashMap<String, String>> =
            self.thought_signatures.as_ref().map(|sigs| {
                sigs.lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .iter()
                    .cloned()
                    .collect()
            });
        let oai_messages = translate_messages(
            &system,
            &request.messages,
            self.echo_reasoning,
            self.mistral_tool_ids,
            signatures.as_ref(),
        );
        let oai_tools = if no_tools {
            vec![]
        } else {
            translate_tools(&request.tools)
        };

        let official_openai = prefix == "oai";
        // Only a user-configured cap is sent: the provider default is right
        // for reasoning models (whose cap also counts reasoning tokens), and
        // OpenAI-fronting endpoints reject `max_tokens` on them. DeepSeek's
        // chat model defaults to 4k, below what it accepts, so raise it.
        let cap = if request.explicit_max_tokens {
            Some(request.max_tokens)
        } else if prefix == "deepseek" && bare_model == "deepseek-chat" {
            Some(DEEPSEEK_CHAT_MAX_TOKENS)
        } else {
            None
        };
        let mut oai_request = OaiRequest {
            model: model.clone(),
            messages: oai_messages,
            tools: oai_tools,
            stream: true,
            stream_options: Some(OaiStreamOptions {
                include_usage: true,
            }),
            max_tokens: if official_openai { None } else { cap },
            max_completion_tokens: if official_openai { cap } else { None },
        };

        // Nothing has reached `on_text` yet, so retrying cannot duplicate
        // output. `send_with_retry` hands back the final response even on an
        // error status, which keeps the "does not support tools" sniff below
        // working exactly as before.
        let build = |req: &OaiRequest| self.post(&url, req);

        let resp = super::retry::send_with_retry(
            || build(&oai_request),
            self.retry_notifier.as_ref(),
            false,
            &format!("{} request failed", self.provider_name),
        )
        .await?;

        let status = resp.status();
        let resp = if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();

            // If the model doesn't support tools, cache that and retry without
            if status.as_u16() == 400 && body.contains("does not support tools") {
                self.no_tools.store(true, Ordering::Relaxed);
                debug!("Model does not support tools — disabling for this session");
                let patched_system = patch_system_no_tools(&system_str);
                oai_request.messages = translate_messages(
                    &patched_system,
                    &request.messages,
                    self.echo_reasoning,
                    self.mistral_tool_ids,
                    signatures.as_ref(),
                );
                oai_request.tools = vec![];

                super::retry::send_with_retry(
                    || build(&oai_request),
                    self.retry_notifier.as_ref(),
                    false,
                    &format!("{} request failed", self.provider_name),
                )
                .await?
            } else {
                return Err(anyhow!("{} error {status}: {body}", self.provider_name));
            }
        } else {
            resp
        };

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!("{} error {status}: {body}", self.provider_name));
        }

        let (result, new_signatures) = parse_oai_stream(resp, on_text).await?;
        if let Some(sigs) = &self.thought_signatures
            && !new_signatures.is_empty()
        {
            remember_signatures(
                &mut sigs.lock().unwrap_or_else(|e| e.into_inner()),
                new_signatures,
            );
        }
        Ok(result)
    }
}

#[cfg(test)]
mod reasoning_echo_tests {
    use super::*;

    fn assistant(blocks: Vec<ContentBlock>) -> Vec<Message> {
        vec![
            Message {
                role: Role::User,
                content: vec![ContentBlock::Text { text: "go".into() }],
            },
            Message {
                role: Role::Assistant,
                content: blocks,
            },
        ]
    }

    fn tool_use() -> ContentBlock {
        ContentBlock::ToolUse {
            id: "call_1".into(),
            name: "Read".into(),
            input: serde_json::json!({"file_path": "a.rs"}),
        }
    }

    fn thinking(signature: &str) -> ContentBlock {
        ContentBlock::Thinking {
            thinking: "plan: read a.rs".into(),
            signature: signature.into(),
        }
    }

    fn assistant_json(msgs: &[Message], echo: bool) -> serde_json::Value {
        let out = translate_messages("", msgs, echo, false, None);
        serde_json::to_value(&out[1]).unwrap()
    }

    #[test]
    fn deepseek_gets_its_reasoning_back_on_tool_turns() {
        let msgs = assistant(vec![thinking(""), tool_use()]);
        let v = assistant_json(&msgs, true);
        assert_eq!(v["reasoning_content"], "plan: read a.rs");
        assert_eq!(v["tool_calls"][0]["id"], "call_1");
    }

    #[test]
    fn other_providers_never_see_reasoning_content() {
        let msgs = assistant(vec![thinking(""), tool_use()]);
        let v = assistant_json(&msgs, false);
        assert!(v.get("reasoning_content").is_none(), "{v}");
    }

    /// Anthropic's signed thinking is not this provider's reasoning; a tool
    /// call without our own reasoning still needs a non-empty placeholder.
    #[test]
    fn signed_thinking_is_not_echoed() {
        let msgs = assistant(vec![thinking("anthropic-sig"), tool_use()]);
        let v = assistant_json(&msgs, true);
        assert_eq!(v["reasoning_content"], " ");
        let msgs = assistant(vec![ContentBlock::Text { text: "hi".into() }]);
        let v = assistant_json(&msgs, true);
        assert!(v.get("reasoning_content").is_none(), "{v}");
    }

    /// Reasoning cut off by max_tokens leaves a thinking-only turn; OpenAI
    /// and Groq reject an assistant message with neither content nor
    /// tool_calls, which wedged every later request.
    #[test]
    fn thinking_only_turn_still_has_content() {
        for echo in [false, true] {
            for sig in ["", "anthropic-sig"] {
                let v = assistant_json(&assistant(vec![thinking(sig)]), echo);
                assert_eq!(v["content"], "", "{v}");
            }
        }
        let v = assistant_json(&assistant(vec![thinking("")]), true);
        assert_eq!(v["reasoning_content"], "plan: read a.rs");
        // A tool-call turn keeps omitting content, as before.
        let v = assistant_json(&assistant(vec![tool_use()]), false);
        assert!(v.get("content").is_none(), "{v}");
    }
}

#[cfg(test)]
mod api_key_tests {
    use super::*;

    fn provider(prefix: &str) -> &'static ProviderDef {
        PROVIDERS.iter().find(|p| p.prefix == prefix).unwrap()
    }

    /// Only OPENAI_API_KEY is set, as for most users with an OpenAI account.
    fn only_openai(k: &str) -> Option<String> {
        (k == "OPENAI_API_KEY").then(|| "sk-openai-secret".to_string())
    }

    #[test]
    fn openai_key_is_never_sent_to_third_party_providers() {
        for p in PROVIDERS {
            if p.key_env.is_empty() || p.key_env == "OPENAI_API_KEY" {
                continue;
            }
            assert_eq!(
                provider_api_key(p, only_openai),
                "",
                "{} got the OpenAI key",
                p.prefix
            );
        }
        let env = |k: &str| (k == "GROQ_API_KEY").then(|| "gsk-groq".to_string());
        assert_eq!(provider_api_key(provider("groq"), env), "gsk-groq");
    }

    #[test]
    fn openai_key_still_serves_openai_and_generic_endpoints() {
        assert_eq!(
            provider_api_key(provider("oai"), only_openai),
            "sk-openai-secret"
        );
        assert_eq!(
            provider_api_key(provider("openai-compat"), only_openai),
            "sk-openai-secret"
        );
        assert_eq!(provider_api_key(provider("lmstudio"), only_openai), "");
    }

    /// A keyless vLLM/llama.cpp box on the LAN is a normal openai-compat
    /// target; only loopback hosts used to be allowed without a key.
    #[test]
    fn openai_compat_on_a_lan_host_needs_no_key() {
        let env = |k: &str| (k == "OPENAI_BASE_URL").then(|| "http://10.0.0.5:8000/v1".to_string());
        let c =
            OpenAiCompatClient::from_model_env("openai-compat:qwen", OpenAiApi::Auto, env).unwrap();
        assert_eq!(c.api_key, "");
        // Cloud providers still refuse to start without their key.
        assert!(
            OpenAiCompatClient::from_model_env("groq:llama", OpenAiApi::Auto, |_| None).is_err()
        );
    }

    /// "/chat/completions" is appended to the base; a trailing slash made
    /// "//chat/completions" and a 404.
    #[test]
    fn trailing_slash_on_base_url_is_dropped() {
        let env = |k: &str| (k == "OPENAI_BASE_URL").then(|| "http://h:8000/v1/".to_string());
        let c =
            OpenAiCompatClient::from_model_env("openai-compat:m", OpenAiApi::Auto, env).unwrap();
        assert_eq!(c.base_url, "http://h:8000/v1");
        let env = |k: &str| (k == "LM_STUDIO_HOST").then(|| "http://box:1234/v1//".to_string());
        let c = OpenAiCompatClient::from_model_env("lmstudio:m", OpenAiApi::Auto, env).unwrap();
        assert_eq!(c.base_url, "http://box:1234/v1");
    }
}

#[cfg(test)]
mod cached_usage_tests {
    use super::stream_error_tests::parse;
    use crate::cost::CostTracker;

    /// One reply whose final chunk carries `usage`, as with
    /// `stream_options.include_usage`.
    macro_rules! reply_with_usage {
        ($usage:literal) => {
            concat!(
                "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"},\"finish_reason\":\"stop\"}]}\n\n",
                "data: {\"choices\":[],\"usage\":",
                $usage,
                "}\n\n",
                "data: [DONE]\n\n",
            )
        };
    }

    /// Gemini thinks by default and bills the thought tokens as output, but
    /// reports them only in `total_tokens`: a thinking turn was priced at
    /// its visible answer alone.
    #[tokio::test]
    async fn thinking_tokens_outside_completion_tokens_are_billed_as_output() {
        let r = parse(reply_with_usage!(
            r#"{"prompt_tokens":1000,"completion_tokens":50,"total_tokens":1650}"#
        ))
        .await
        .unwrap();
        assert_eq!((r.usage.input_tokens, r.usage.output_tokens), (1_000, 650));
        // Where total = prompt + completion nothing changes.
        let r = parse(reply_with_usage!(
            r#"{"prompt_tokens":1000,"completion_tokens":50,"total_tokens":1050}"#
        ))
        .await
        .unwrap();
        assert_eq!(r.usage.output_tokens, 50);
    }

    /// OpenAI counts cache hits inside `prompt_tokens` and reports them in
    /// `prompt_tokens_details.cached_tokens`; they used to be billed at the
    /// full input rate, overstating cost and tripping /budget early.
    #[tokio::test]
    async fn openai_cached_tokens_are_billed_as_cache_reads() {
        let r = parse(reply_with_usage!(
            r#"{"prompt_tokens":10000,"completion_tokens":100,"prompt_tokens_details":{"cached_tokens":8000,"audio_tokens":0}}"#
        ))
        .await
        .unwrap();
        let u = &r.usage;
        assert_eq!(
            (u.input_tokens, u.cache_read_input_tokens, u.output_tokens),
            (2_000, 8_000, 100)
        );
        assert_eq!(u.cache_creation_input_tokens, 0);
        assert_eq!(u.context_tokens(), 10_000, "context still counts the hits");

        let mut t = CostTracker::new();
        t.record_with_cache(
            "oai:gpt-4o",
            u.input_tokens,
            u.output_tokens,
            u.cache_read_input_tokens,
            u.cache_creation_input_tokens,
        );
        // 2K in at $2.50 + 8K cached at $1.25 + 100 out at $10, per MTok.
        assert!(
            (t.total_cost_usd - 0.016).abs() < 1e-12,
            "{}",
            t.total_cost_usd
        );
        assert_eq!(t.last_input_tokens, 10_000);
    }

    /// DeepSeek splits `prompt_tokens` into cache hits and misses.
    #[tokio::test]
    async fn deepseek_cache_hits_are_billed_as_cache_reads() {
        let r = parse(reply_with_usage!(
            r#"{"prompt_tokens":10000,"completion_tokens":100,"prompt_cache_hit_tokens":9000,"prompt_cache_miss_tokens":1000}"#
        ))
        .await
        .unwrap();
        let u = &r.usage;
        assert_eq!(
            (u.input_tokens, u.cache_read_input_tokens, u.output_tokens),
            (1_000, 9_000, 100)
        );

        let mut t = CostTracker::new();
        t.record_with_cache(
            "deepseek:deepseek-chat",
            u.input_tokens,
            u.output_tokens,
            u.cache_read_input_tokens,
            u.cache_creation_input_tokens,
        );
        // 1K miss at $0.27 + 9K hit at $0.07 + 100 out at $1.10, per MTok.
        assert!(
            (t.total_cost_usd - 0.00101).abs() < 1e-12,
            "{}",
            t.total_cost_usd
        );
    }

    /// Servers without prompt caching report usage exactly as before.
    #[tokio::test]
    async fn usage_without_cache_fields_is_unchanged() {
        for body in [
            reply_with_usage!(r#"{"prompt_tokens":3000,"completion_tokens":7}"#),
            reply_with_usage!(
                r#"{"prompt_tokens":3000,"completion_tokens":7,"prompt_tokens_details":null}"#
            ),
            reply_with_usage!(
                r#"{"prompt_tokens":3000,"completion_tokens":7,"prompt_tokens_details":{"cached_tokens":null}}"#
            ),
            reply_with_usage!(
                r#"{"prompt_tokens":3000,"completion_tokens":7,"prompt_tokens_details":{"cached_tokens":0}}"#
            ),
        ] {
            let u = parse(body).await.unwrap().usage;
            assert_eq!(
                (u.input_tokens, u.output_tokens, u.cache_read_input_tokens),
                (3_000, 7, 0),
                "{body}"
            );
        }
    }

    /// A server reporting more hits than prompt tokens must not underflow.
    #[tokio::test]
    async fn cache_hits_never_exceed_the_prompt() {
        let u = parse(reply_with_usage!(
            r#"{"prompt_tokens":100,"completion_tokens":1,"prompt_tokens_details":{"cached_tokens":500}}"#
        ))
        .await
        .unwrap()
        .usage;
        assert_eq!((u.input_tokens, u.cache_read_input_tokens), (0, 100));
    }
}

#[cfg(test)]
mod max_tokens_tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Answers one chat completion with an empty stream and returns the JSON
    /// body the client sent.
    pub(super) async fn capture_one_body() -> (String, tokio::task::JoinHandle<serde_json::Value>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            let body_start = loop {
                let n = sock.read(&mut chunk).await.unwrap();
                assert!(n > 0, "connection closed before the body");
                buf.extend_from_slice(&chunk[..n]);
                if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    break i + 4;
                }
            };
            let head = String::from_utf8_lossy(&buf[..body_start]).to_ascii_lowercase();
            let len: usize = head
                .lines()
                .find_map(|l| l.strip_prefix("content-length:"))
                .unwrap()
                .trim()
                .parse()
                .unwrap();
            while buf.len() < body_start + len {
                let n = sock.read(&mut chunk).await.unwrap();
                assert!(n > 0, "connection closed mid-body");
                buf.extend_from_slice(&chunk[..n]);
            }
            let _ = sock
                .write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                      connection: close\r\n\r\ndata: [DONE]\n\n",
                )
                .await;
            let _ = sock.shutdown().await;
            serde_json::from_slice(&buf[body_start..body_start + len]).unwrap()
        });
        (format!("http://{addr}"), handle)
    }

    pub(super) fn request(model: &str) -> MessagesRequest {
        MessagesRequest {
            model: model.into(),
            max_tokens: 12345,
            messages: vec![Message {
                role: Role::User,
                content: vec![ContentBlock::Text { text: "hi".into() }],
            }],
            system: Default::default(),
            tools: vec![],
            stream: None,
            thinking: None,
            output_config: None,
            betas: vec![],
            session_id: None,
            explicit_max_tokens: true,
            cache_history: false,
        }
    }

    pub(super) fn client(base_url: String) -> OpenAiCompatClient {
        OpenAiCompatClient {
            client: Client::new(),
            base_url,
            api_key: String::new(),
            provider_name: "test".into(),
            extra_headers: vec![],
            no_tools: Arc::new(AtomicBool::new(false)),
            tools_notice_sent: Arc::new(AtomicBool::new(false)),
            no_summary: Arc::new(AtomicBool::new(false)),
            summary_notice_sent: Arc::new(AtomicBool::new(false)),
            echo_reasoning: false,
            mistral_tool_ids: false,
            thought_signatures: None,
            responses: None,
            retry_notifier: None,
        }
    }

    #[tokio::test]
    async fn compat_providers_receive_max_tokens() {
        let (url, body) = capture_one_body().await;
        let _ = client(url)
            .messages_stream(request("deepseek:deepseek-chat"), |_| {})
            .await;
        let body = body.await.unwrap();
        assert_eq!(body["max_tokens"], 12345, "{body}");
        assert!(body.get("max_completion_tokens").is_none(), "{body}");
    }

    #[tokio::test]
    async fn openai_receives_max_completion_tokens() {
        let (url, body) = capture_one_body().await;
        let _ = client(url)
            .messages_stream(request("oai:o4-mini"), |_| {})
            .await;
        let body = body.await.unwrap();
        assert_eq!(body["max_completion_tokens"], 12345, "{body}");
        assert!(body.get("max_tokens").is_none(), "{body}");
    }

    /// An unset `maxTokens` sent the 8k model default to every backend,
    /// truncating reasoning models and 400ing endpoints that reject
    /// `max_tokens`. Only deepseek-chat (default 4k) still gets a cap.
    #[tokio::test]
    async fn default_cap_is_not_sent() {
        for model in [
            "groq:llama-3.3-70b",
            "oai:o4-mini",
            "deepseek:deepseek-reasoner",
        ] {
            let (url, body) = capture_one_body().await;
            let mut req = request(model);
            req.explicit_max_tokens = false;
            let _ = client(url).messages_stream(req, |_| {}).await;
            let body = body.await.unwrap();
            assert!(body.get("max_tokens").is_none(), "{model}: {body}");
            assert!(
                body.get("max_completion_tokens").is_none(),
                "{model}: {body}"
            );
        }
        let (url, body) = capture_one_body().await;
        let mut req = request("ollama:qwen3");
        req.explicit_max_tokens = false;
        let _ = crate::api::ollama::OllamaClient::new(url)
            .unwrap()
            .messages_stream(req, |_| {})
            .await;
        let body = body.await.unwrap();
        assert!(body.get("max_tokens").is_none(), "{body}");

        let (url, body) = capture_one_body().await;
        let mut req = request("deepseek:deepseek-chat");
        req.explicit_max_tokens = false;
        let _ = client(url).messages_stream(req, |_| {}).await;
        let body = body.await.unwrap();
        assert_eq!(body["max_tokens"], DEEPSEEK_CHAT_MAX_TOKENS, "{body}");
    }

    #[tokio::test]
    async fn ollama_receives_max_tokens() {
        let (url, body) = capture_one_body().await;
        let _ = crate::api::ollama::OllamaClient::new(url)
            .unwrap()
            .messages_stream(request("ollama:qwen3"), |_| {})
            .await;
        let body = body.await.unwrap();
        assert_eq!(body["max_tokens"], 12345, "{body}");
        assert!(body.get("max_completion_tokens").is_none(), "{body}");
    }
}

#[cfg(test)]
mod stream_error_tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Serves `body` as one SSE response and parses it.
    pub(super) async fn parse(body: &'static str) -> Result<StreamedResponse> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            let _ = sock.read(&mut buf).await;
            let resp = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                 content-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = sock.write_all(resp.as_bytes()).await;
            let _ = sock.shutdown().await;
        });
        let resp = reqwest::get(format!("http://{addr}")).await.unwrap();
        parse_oai_stream(resp, |_| {}).await.map(|(r, _)| r)
    }

    #[tokio::test]
    async fn error_chunk_is_an_error() {
        let err = parse(
            "data: {\"choices\":[{\"delta\":{\"content\":\"Hal\"},\"finish_reason\":null}]}\n\n\
             data: {\"error\":{\"message\":\"model overloaded\",\"code\":503}}\n\n\
             data: [DONE]\n\n",
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("model overloaded"), "{err}");
    }

    #[tokio::test]
    async fn vllm_error_object_is_an_error() {
        let err = parse(
            "data: {\"object\":\"error\",\"message\":\"context too long\",\"code\":400}\n\n\
             data: [DONE]\n\n",
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("context too long"), "{err}");
    }

    /// OpenRouter: the error rides on a normal-looking chunk whose tool call
    /// was cut off mid-arguments.
    #[tokio::test]
    async fn finish_reason_error_is_an_error() {
        let err = parse(
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c1\",\
             \"function\":{\"name\":\"Write\",\"arguments\":\"{\\\"file_pa\"}}]},\
             \"finish_reason\":null}]}\n\n\
             data: {\"choices\":[{\"delta\":{\"content\":\"\"},\"finish_reason\":\"error\"}]}\n\n\
             data: [DONE]\n\n",
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("error"), "{err}");
    }

    #[tokio::test]
    async fn stream_closed_without_an_end_marker_is_an_error() {
        let err = parse("data: {\"choices\":[{\"delta\":{\"content\":\"Half an ans\"}}]}\n\n")
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("before the reply finished"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn normal_streams_still_parse() {
        let r = parse(
            "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"},\"finish_reason\":\"stop\"}]}\n\n\
             data: {\"choices\":[],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":1}}\n\n\
             data: [DONE]\n\n",
        )
        .await
        .unwrap();
        assert_eq!(r.stop_reason, Some(StopReason::EndTurn));
        assert_eq!(r.usage.output_tokens, 1);
        // A server that closes after finish_reason without [DONE] is fine.
        let r = parse(
            "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"},\"finish_reason\":\"stop\"}]}\n\n",
        )
        .await
        .unwrap();
        assert_eq!(r.content, vec![ContentBlock::Text { text: "hi".into() }]);
    }

    /// Gemini's compat endpoint omits `index`; the chunk used to fail to
    /// parse, losing both the calls and the finish_reason.
    #[tokio::test]
    async fn tool_call_deltas_without_index_still_parse() {
        let r = parse(
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[\
             {\"id\":\"c1\",\"type\":\"function\",\"function\":{\"name\":\"Read\",\"arguments\":\"{\\\"file_path\\\":\"}},\
             {\"function\":{\"arguments\":\"\\\"a.rs\\\"}\"}},\
             {\"id\":\"c2\",\"type\":\"function\",\"function\":{\"name\":\"Glob\",\"arguments\":\"{}\"}}\
             ]},\"finish_reason\":\"tool_calls\"}]}\n\n\
             data: [DONE]\n\n",
        )
        .await
        .unwrap();
        assert_eq!(r.stop_reason, Some(StopReason::ToolUse));
        assert_eq!(
            r.content,
            vec![
                ContentBlock::ToolUse {
                    id: "c1".into(),
                    name: "Read".into(),
                    input: serde_json::json!({"file_path": "a.rs"}),
                },
                ContentBlock::ToolUse {
                    id: "c2".into(),
                    name: "Glob".into(),
                    input: serde_json::json!({}),
                },
            ]
        );
    }
}

#[cfg(test)]
mod image_tests {
    use super::*;

    #[test]
    fn user_images_become_image_url_parts() {
        let msgs = vec![Message {
            role: Role::User,
            content: vec![
                ContentBlock::Text {
                    text: "what is this?".into(),
                },
                ContentBlock::Image {
                    source: ImageSource::Base64 {
                        media_type: "image/png".into(),
                        data: "iVBORw0KGgo=".into(),
                    },
                },
                ContentBlock::Image {
                    source: ImageSource::Url {
                        url: "https://example.com/a.jpg".into(),
                    },
                },
            ],
        }];
        let out = translate_messages("", &msgs, false, false, None);
        let v = serde_json::to_value(&out[0]).unwrap();
        assert_eq!(
            v["content"],
            serde_json::json!([
                {"type": "text", "text": "what is this?"},
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,iVBORw0KGgo="}},
                {"type": "image_url", "image_url": {"url": "https://example.com/a.jpg"}},
            ])
        );
    }

    #[test]
    fn text_only_user_messages_stay_plain_strings() {
        let msgs = vec![Message {
            role: Role::User,
            content: vec![ContentBlock::Text { text: "hi".into() }],
        }];
        let v =
            serde_json::to_value(&translate_messages("", &msgs, false, false, None)[0]).unwrap();
        assert_eq!(v["content"], "hi");
    }
}

#[cfg(test)]
mod mistral_tool_id_tests {
    use super::*;

    fn history(id: &str) -> Vec<Message> {
        vec![
            Message {
                role: Role::User,
                content: vec![ContentBlock::Text { text: "go".into() }],
            },
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: id.into(),
                    name: "Read".into(),
                    input: serde_json::json!({}),
                }],
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: id.into(),
                    content: vec![ToolResultContent::Text { text: "ok".into() }],
                    is_error: None,
                }],
            },
        ]
    }

    fn ids(msgs: &[Message], mistral: bool) -> (String, String) {
        let out = translate_messages("", msgs, false, mistral, None);
        let call = serde_json::to_value(&out[1]).unwrap()["tool_calls"][0]["id"]
            .as_str()
            .unwrap()
            .to_string();
        let result = out[2].tool_call_id.clone().unwrap();
        (call, result)
    }

    #[test]
    fn foreign_ids_are_rewritten_to_nine_alphanumerics_and_stay_paired() {
        for id in ["toolu_01A09q90qw90lq917835lq9", "call_abc123", "c1"] {
            let (call, result) = ids(&history(id), true);
            assert_eq!(call.len(), 9, "{call}");
            assert!(call.bytes().all(|b| b.is_ascii_alphanumeric()), "{call}");
            assert_eq!(call, result, "tool result must still match its call");
        }
        assert_ne!(mistral_tool_id("call_a"), mistral_tool_id("call_b"));
    }

    #[test]
    fn mistral_ids_and_other_providers_are_untouched() {
        assert_eq!(ids(&history("aB3dE6gH9"), true).0, "aB3dE6gH9");
        let (call, result) = ids(&history("toolu_01xyz"), false);
        assert_eq!(
            (call.as_str(), result.as_str()),
            ("toolu_01xyz", "toolu_01xyz")
        );
    }
}

#[cfg(test)]
mod gemini_tests {
    use super::max_tokens_tests::{capture_one_body, client, request};
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn env_of(vars: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |k| {
            vars.iter()
                .find(|(name, _)| *name == k)
                .map(|(_, v)| v.to_string())
        }
    }

    #[test]
    fn gemini_prefix_selects_googles_openai_endpoint() {
        assert!(is_openai_compat_model("gemini:gemini-2.5-flash"));
        let (p, bare) = parse_provider_model("gemini:gemini-2.5-flash").unwrap();
        assert_eq!(
            (p.prefix, p.name, bare),
            ("gemini", "Gemini", "gemini-2.5-flash")
        );

        let env = env_of(&[("GEMINI_API_KEY", "gem-key")]);
        let c = OpenAiCompatClient::from_model_env("gemini:gemini-2.5-flash", OpenAiApi::Auto, env)
            .unwrap();
        // Requests go to <base>/chat/completions.
        assert_eq!(
            c.base_url,
            "https://generativelanguage.googleapis.com/v1beta/openai"
        );
        assert_eq!(c.api_key, "gem-key");
        assert_eq!(c.provider_name, "Gemini");
        assert!(c.thought_signatures.is_some());
        assert!(!c.echo_reasoning && !c.mistral_tool_ids);
    }

    #[test]
    fn gemini_key_falls_back_to_google_api_key() {
        let gemini = PROVIDERS.iter().find(|p| p.prefix == "gemini").unwrap();
        let key = |vars| provider_api_key(gemini, env_of(vars));
        assert_eq!(key(&[("GOOGLE_API_KEY", "g")]), "g");
        assert_eq!(
            key(&[("GEMINI_API_KEY", "gem"), ("GOOGLE_API_KEY", "g")]),
            "gem"
        );
        // An empty GEMINI_API_KEY is unset, not a key.
        assert_eq!(key(&[("GEMINI_API_KEY", ""), ("GOOGLE_API_KEY", "g")]), "g");
        assert_eq!(key(&[("OPENAI_API_KEY", "sk")]), "");

        // GOOGLE_API_KEY is Gemini's alone.
        let groq = PROVIDERS.iter().find(|p| p.prefix == "groq").unwrap();
        assert_eq!(
            provider_api_key(groq, env_of(&[("GOOGLE_API_KEY", "g")])),
            ""
        );

        let err =
            OpenAiCompatClient::from_model_env("gemini:gemini-2.5-flash", OpenAiApi::Auto, |_| {
                None
            })
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains("GEMINI_API_KEY or GOOGLE_API_KEY"), "{err}");
    }

    fn tool_use(id: &str) -> ContentBlock {
        ContentBlock::ToolUse {
            id: id.into(),
            name: "Read".into(),
            input: serde_json::json!({"file_path": "a.rs"}),
        }
    }

    fn calls(
        blocks: Vec<ContentBlock>,
        sigs: Option<&HashMap<String, String>>,
    ) -> serde_json::Value {
        let msgs = [
            Message {
                role: Role::User,
                content: vec![ContentBlock::Text { text: "go".into() }],
            },
            Message {
                role: Role::Assistant,
                content: blocks,
            },
        ];
        serde_json::to_value(&translate_messages("", &msgs, false, false, sigs)[1]).unwrap()
            ["tool_calls"]
            .clone()
    }

    /// Gemini 3 rejects a tool-call follow-up whose call lost its thought
    /// signature; it goes back where it came from, and a first call with no
    /// signature on record gets the documented skip value.
    #[test]
    fn thought_signatures_go_back_on_gemini_tool_calls() {
        let sigs = HashMap::from([("fc-1".to_string(), "SIG-1".to_string())]);
        let v = calls(vec![tool_use("fc-1"), tool_use("fc-2")], Some(&sigs));
        assert_eq!(
            v[0]["extra_content"]["google"]["thought_signature"],
            "SIG-1"
        );
        // Parallel calls after the first never carried one.
        assert!(v[1].get("extra_content").is_none(), "{v}");

        // History from Claude or a resumed session: no signature on record.
        let v = calls(
            vec![tool_use("toolu_01"), tool_use("toolu_02")],
            Some(&sigs),
        );
        assert_eq!(
            v[0]["extra_content"]["google"]["thought_signature"],
            GEMINI_SKIP_SIGNATURE
        );
        assert!(v[1].get("extra_content").is_none(), "{v}");

        // Every other provider rejects unknown fields: none is sent.
        let v = calls(vec![tool_use("fc-1")], None);
        assert!(v[0].get("extra_content").is_none(), "{v}");
    }

    /// Serves one SSE `body` and returns the base URL.
    async fn serve_stream(body: &'static str) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 16384];
            let _ = sock.read(&mut buf).await;
            let resp = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                 content-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = sock.write_all(resp.as_bytes()).await;
            let _ = sock.shutdown().await;
        });
        format!("http://{addr}")
    }

    /// The shape Google's endpoint streams: parallel calls without `index`,
    /// the signature on the first call only.
    const GEMINI_TOOL_STREAM: &str = "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\",\"tool_calls\":[\
        {\"extra_content\":{\"google\":{\"thought_signature\":\"SIG-A\"}},\
        \"function\":{\"arguments\":\"{\\\"file_path\\\":\\\"a.rs\\\"}\",\"name\":\"Read\"},\
        \"id\":\"function-call-1\",\"type\":\"function\"},\
        {\"function\":{\"arguments\":\"{\\\"file_path\\\":\\\"b.rs\\\"}\",\"name\":\"Read\"},\
        \"id\":\"function-call-2\",\"type\":\"function\"}]},\
        \"finish_reason\":\"tool_calls\",\"index\":0}],\"object\":\"chat.completion.chunk\"}\n\n\
        data: [DONE]\n\n";

    #[tokio::test]
    async fn stream_parser_reads_thought_signatures() {
        let url = serve_stream(GEMINI_TOOL_STREAM).await;
        let resp = reqwest::get(url).await.unwrap();
        let (result, sigs) = parse_oai_stream(resp, |_| {}).await.unwrap();
        assert_eq!(result.stop_reason, Some(StopReason::ToolUse));
        let ids: Vec<_> = result
            .content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::ToolUse { id, .. } => Some(id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(ids, ["function-call-1", "function-call-2"]);
        assert_eq!(
            sigs,
            HashMap::from([("function-call-1".to_string(), "SIG-A".to_string())])
        );
    }

    /// End to end over a real socket: the signature a turn's tool call came
    /// with is in the next request, sent from a clone of the client as each
    /// TUI turn does.
    #[tokio::test]
    async fn next_request_carries_the_signature_back() {
        let mut c = client(serve_stream(GEMINI_TOOL_STREAM).await);
        c.thought_signatures = Some(Default::default());
        let first = c
            .clone()
            .messages_stream(request("gemini:gemini-3-pro-preview"), |_| {})
            .await
            .unwrap();

        let (url, body) = capture_one_body().await;
        c.base_url = url;
        let mut req = request("gemini:gemini-3-pro-preview");
        req.messages.push(Message {
            role: Role::Assistant,
            content: first.content,
        });
        req.messages.push(Message {
            role: Role::User,
            content: ["function-call-1", "function-call-2"]
                .into_iter()
                .map(|id| ContentBlock::ToolResult {
                    tool_use_id: id.into(),
                    content: vec![ToolResultContent::Text { text: "ok".into() }],
                    is_error: None,
                })
                .collect(),
        });
        let _ = c.messages_stream(req, |_| {}).await;
        let body = body.await.unwrap();
        let calls = &body["messages"][1]["tool_calls"];
        assert_eq!(calls[0]["id"], "function-call-1", "{body}");
        assert_eq!(
            calls[0]["extra_content"]["google"]["thought_signature"], "SIG-A",
            "{body}"
        );
        assert!(calls[1].get("extra_content").is_none(), "{body}");
    }

    #[test]
    fn signature_store_keeps_the_newest() {
        let mut store: SignatureStore = (0..MAX_THOUGHT_SIGNATURES)
            .map(|i| (format!("old{i}"), "s".into()))
            .collect();
        remember_signatures(
            &mut store,
            HashMap::from([("new".to_string(), "SIG".to_string())]),
        );
        assert_eq!(store.len(), MAX_THOUGHT_SIGNATURES);
        assert_eq!(store.front().unwrap().0, "old1");
        assert_eq!(
            store.back().unwrap(),
            &("new".to_string(), "SIG".to_string())
        );
    }
}
