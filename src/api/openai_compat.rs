/// Generic OpenAI-compatible provider adapter.
///
/// One client that talks to ANY endpoint implementing the OpenAI
/// `/v1/chat/completions` API: OpenRouter, Groq, DeepSeek, LM Studio,
/// llama.cpp, Together AI, Mistral, and hundreds more.
///
/// Named provider shortcuts give convenient prefixes:
///
///   /model groq:llama-3.3-70b-versatile
///   /model openrouter:meta-llama/llama-3.3-70b-instruct
///   /model deepseek:deepseek-chat
///   /model lmstudio:llama-3.2-3b-instruct
///   /model together:meta-llama/Llama-3.3-70b-chat-hf
///   /model mistral:codestral-latest
///   /model oai:gpt-4o                          (real OpenAI)
///   /model openai-compat:my-model              (generic, needs OPENAI_BASE_URL)
///
/// Provider API keys come from environment variables:
///   GROQ_API_KEY, OPENROUTER_API_KEY, DEEPSEEK_API_KEY, etc.
///   OPENAI_API_KEY is the fallback for any provider without a specific key.
///   OPENAI_BASE_URL overrides the base URL for the generic `openai-compat:` prefix.
use anyhow::{Context, Result, anyhow};
use eventsource_stream::Eventsource;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use tracing::{debug, warn};

use crate::api::types::*;

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
            ("HTTP-Referer", "https://github.com/ForkedInTime/RustyClaw"),
            ("X-Title", "RustyClaw"),
        ],
    },
    ProviderDef {
        prefix: "deepseek",
        name: "DeepSeek",
        base_url: "https://api.deepseek.com/v1",
        key_env: "DEEPSEEK_API_KEY",
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
}

#[derive(Serialize, Deserialize, Clone)]
pub(crate) struct OaiToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub call_type: String,
    pub function: OaiFunction,
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

#[derive(Serialize)]
pub(crate) struct OaiRequest {
    pub model: String,
    pub messages: Vec<OaiMessage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<OaiTool>,
    pub stream: bool,
    pub stream_options: Option<OaiStreamOptions>,
}

#[derive(Serialize)]
pub(crate) struct OaiStreamOptions {
    pub include_usage: bool,
}

// ── Streaming response chunks ────────────────────────────────────────────────

#[derive(Deserialize)]
pub(crate) struct OaiChunk {
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
    pub index: usize,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub function: Option<OaiFunctionDelta>,
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
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
}

// ─── Shared translation: Anthropic ↔ OpenAI ─────────────────────────────────

/// Translate Anthropic `Message`s into OpenAI-format messages.
/// System prompt is handled separately (passed as role:system first message).
pub(crate) fn translate_messages(system: &str, messages: &[Message]) -> Vec<OaiMessage> {
    let mut out = Vec::with_capacity(messages.len() + 1);

    if !system.is_empty() {
        out.push(OaiMessage {
            role: "system".into(),
            content: Some(serde_json::Value::String(system.to_string())),
            tool_calls: None,
            tool_call_id: None,
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

                if !text_blocks.is_empty() {
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
                    out.push(OaiMessage {
                        role: "user".into(),
                        content: Some(serde_json::Value::String(text)),
                        tool_calls: None,
                        tool_call_id: None,
                    });
                }

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
                            tool_call_id: Some(tool_use_id.clone()),
                        });
                    }
                }
            }

            Role::Assistant => {
                let mut text_parts: Vec<&str> = Vec::new();
                let mut tool_calls: Vec<OaiToolCall> = Vec::new();

                for block in &msg.content {
                    match block {
                        ContentBlock::Text { text } => text_parts.push(text.as_str()),
                        ContentBlock::ToolUse { id, name, input } => {
                            tool_calls.push(OaiToolCall {
                                id: id.clone(),
                                call_type: "function".into(),
                                function: OaiFunction {
                                    name: name.clone(),
                                    arguments: serde_json::to_string(input)
                                        .unwrap_or_else(|_| "{}".into()),
                                },
                            });
                        }
                        ContentBlock::Thinking { .. }
                        | ContentBlock::ToolResult { .. }
                        | ContentBlock::Image { .. } => {}
                    }
                }

                let content = if text_parts.is_empty() {
                    None
                } else {
                    Some(serde_json::Value::String(text_parts.join("\n")))
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
                });
            }
        }
    }

    out
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

/// Parse an SSE stream of OpenAI-format chunks into a StreamedResponse.
/// Shared between OllamaClient and OpenAiCompatClient.
pub(crate) async fn parse_oai_stream(
    resp: reqwest::Response,
    mut on_text: impl FnMut(&str),
) -> Result<(StreamedResponse, Option<String>)> {
    let mut stream = resp.bytes_stream().eventsource();
    let mut result = StreamedResponse::default();

    let mut text_buf = String::new();
    let mut thinking_buf = String::new();
    let mut tool_bufs: HashMap<usize, (String, String, String)> = HashMap::new();
    let mut finish_reason: Option<String> = None;

    while let Some(event) = super::next_sse_event(&mut stream).await? {
        if event.data == "[DONE]" {
            break;
        }

        let chunk: OaiChunk = match serde_json::from_str(&event.data) {
            Ok(c) => c,
            Err(e) => {
                warn!("Failed to parse SSE chunk: {e}: {}", event.data);
                continue;
            }
        };

        if let Some(usage) = chunk.usage {
            result.usage.input_tokens = usage.prompt_tokens;
            result.usage.output_tokens = usage.completion_tokens;
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
                    let entry = tool_bufs
                        .entry(tc.index)
                        .or_insert_with(|| (String::new(), String::new(), String::new()));

                    if let Some(id) = tc.id {
                        entry.0 = id;
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

    for (_, (id, name, args)) in tool_entries {
        let input =
            serde_json::from_str(&args).unwrap_or(serde_json::Value::Object(Default::default()));
        result
            .content
            .push(ContentBlock::ToolUse { id, name, input });
    }

    // ── Map finish_reason → StopReason ───────────────────────────────────────

    result.stop_reason = match finish_reason.as_deref() {
        Some("stop") | Some("eos") => Some(StopReason::EndTurn),
        Some("tool_calls") | Some("function_call") => Some(StopReason::ToolUse),
        Some("length") => Some(StopReason::MaxTokens),
        Some("stop_sequence") => Some(StopReason::StopSequence),
        _ => Some(StopReason::EndTurn),
    };

    Ok((result, finish_reason))
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
    /// See `ClaudeClient::retry_notifier`. Rate limiting is far more common on
    /// these providers than on Anthropic — Groq and OpenRouter throttle hard.
    retry_notifier: Option<super::retry::RetryNotifier>,
}

impl OpenAiCompatClient {
    /// Create a client for a specific provider prefix + model string.
    /// Resolves base_url from the provider registry and API key from env vars.
    pub fn from_model(model: &str) -> Result<Self> {
        let (provider, _bare) = parse_provider_model(model)
            .ok_or_else(|| anyhow!("Unknown provider prefix in '{model}'"))?;

        // Resolve base URL
        let base_url = if provider.prefix == "openai-compat" {
            // Generic escape hatch: MUST have OPENAI_BASE_URL set
            std::env::var("OPENAI_BASE_URL").map_err(|_| {
                anyhow!(
                    "openai-compat: requires OPENAI_BASE_URL env var.\n\
                     Set it to your endpoint, e.g.:\n  \
                     export OPENAI_BASE_URL=http://localhost:8080/v1"
                )
            })?
        } else if provider.prefix == "lmstudio" {
            // LM Studio: allow override via LM_STUDIO_HOST
            std::env::var("LM_STUDIO_HOST").unwrap_or_else(|_| provider.base_url.to_string())
        } else {
            provider.base_url.to_string()
        };

        // Resolve API key: provider-specific env var → OPENAI_API_KEY fallback → empty
        let api_key = if provider.key_env.is_empty() {
            // Local providers (LM Studio) don't need a key
            String::new()
        } else {
            std::env::var(provider.key_env)
                .or_else(|_| std::env::var("OPENAI_API_KEY"))
                .unwrap_or_default()
        };

        // Warn if cloud provider has no key (local providers are fine without)
        if api_key.is_empty()
            && !provider.key_env.is_empty()
            && !base_url.starts_with("http://localhost")
            && !base_url.starts_with("http://127.0.0.1")
        {
            return Err(anyhow!(
                "{}: no API key found.\n  Set {} or OPENAI_API_KEY in your environment.\n  \
                 Example: export {}=your-key-here",
                provider.name,
                provider.key_env,
                provider.key_env
            ));
        }

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
        })
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

    /// Streaming call — drop-in replacement for ClaudeClient::messages_stream.
    pub async fn messages_stream(
        &self,
        request: MessagesRequest,
        on_text: impl FnMut(&str),
    ) -> Result<StreamedResponse> {
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

        let oai_messages = translate_messages(&system, &request.messages);
        let oai_tools = if no_tools {
            vec![]
        } else {
            translate_tools(&request.tools)
        };

        let mut oai_request = OaiRequest {
            model: model.clone(),
            messages: oai_messages,
            tools: oai_tools,
            stream: true,
            stream_options: Some(OaiStreamOptions {
                include_usage: true,
            }),
        };

        // Nothing has reached `on_text` yet, so retrying cannot duplicate
        // output. `send_with_retry` hands back the final response even on an
        // error status, which keeps the "does not support tools" sniff below
        // working exactly as before.
        let build = |req: &OaiRequest| {
            let mut builder = self.client.post(&url).json(req);
            if !self.api_key.is_empty() {
                builder = builder.bearer_auth(&self.api_key);
            }
            // Provider-specific headers (e.g. OpenRouter's HTTP-Referer)
            for (k, v) in &self.extra_headers {
                builder = builder.header(k.as_str(), v.as_str());
            }
            builder
        };

        let resp = super::retry::send_with_retry(
            || build(&oai_request),
            self.retry_notifier.as_ref(),
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
                oai_request.messages = translate_messages(&patched_system, &request.messages);
                oai_request.tools = vec![];

                super::retry::send_with_retry(
                    || build(&oai_request),
                    self.retry_notifier.as_ref(),
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

        let (result, _) = parse_oai_stream(resp, on_text).await?;
        Ok(result)
    }
}
