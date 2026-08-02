/// Anthropic API client — port of services/api/claude.ts
pub mod ollama;
pub mod openai_compat;
pub mod types;

use anyhow::{Context, Result, anyhow};
use eventsource_stream::Eventsource;
use futures_util::StreamExt;
use reqwest::{Client, header};
use std::collections::HashMap;
use tracing::{debug, warn};

pub use ollama::{OllamaClient, is_ollama_model, list_ollama_models, strip_ollama_prefix};
pub use openai_compat::{
    OpenAiCompatClient, PROVIDERS, is_openai_compat_model, parse_provider_model,
};
pub use types::*;

const ANTHROPIC_API_BASE: &str = "https://api.anthropic.com";
const ANTHROPIC_VERSION: &str = "2023-06-01";
const DEFAULT_MODEL: &str = "claude-sonnet-4-6";
const DEFAULT_MAX_TOKENS: u32 = 8096;

/// Maximum time to wait for the *next* SSE event before declaring the stream dead.
///
/// This is deliberately an inter-event budget, not a whole-request timeout: a
/// legitimate response can stream for many minutes, so `.timeout()` on the request
/// would truncate valid work. But a healthy connection always delivers *something* —
/// a content delta, a `ping`, or a keepalive — well inside this window.
///
/// Without this bound, a silently dropped TCP connection (NAT idle reaper, laptop
/// sleep, VPN drop) leaves the read future pending forever: the UI hangs with no
/// error and no recovery short of killing the process.
pub(crate) const SSE_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// Await the next event from an SSE stream, bounded by [`SSE_IDLE_TIMEOUT`].
///
/// Returns `Ok(None)` on clean end-of-stream. Shared by the Anthropic backend and
/// the OpenAI-compatible backend (which also serves Ollama), so every streaming path
/// gets the same stall detection.
pub(crate) async fn next_sse_event<S, E>(stream: &mut S) -> Result<Option<eventsource_stream::Event>>
where
    S: futures_util::Stream<Item = std::result::Result<eventsource_stream::Event, E>> + Unpin,
    E: std::fmt::Display,
{
    match tokio::time::timeout(SSE_IDLE_TIMEOUT, stream.next()).await {
        Err(_) => Err(anyhow!(
            "SSE stream stalled: no data received for {}s — the connection was likely \
             dropped upstream. Retry the request.",
            SSE_IDLE_TIMEOUT.as_secs()
        )),
        Ok(None) => Ok(None),
        Ok(Some(Ok(event))) => Ok(Some(event)),
        Ok(Some(Err(e))) => Err(anyhow!("SSE stream error: {e}")),
    }
}

#[derive(Clone)]
#[allow(dead_code)] // api_key retained for future authenticated-header injection
pub struct ClaudeClient {
    client: Client,
    api_key: String,
    base_url: String,
    /// Betas the credential itself requires, merged into every request's
    /// `anthropic-beta`. An OAuth bearer token needs `oauth-2025-04-20`.
    ///
    /// This is *not* a default header: `RequestBuilder::header` appends rather
    /// than replaces, so a default `anthropic-beta` plus a per-request one
    /// would send the field twice. Merging into the single per-request value
    /// keeps exactly one.
    credential_betas: Vec<String>,
}

impl ClaudeClient {
    /// Construct from a static API key. Retained for callers that already hold
    /// a raw key; prefer [`ClaudeClient::with_credential`].
    pub fn new(api_key: impl Into<String>) -> Result<Self> {
        Self::with_credential(&crate::auth::Credential::ApiKey(api_key.into()))
    }

    /// Construct from a resolved credential, selecting the wire format.
    ///
    /// A static key authenticates with `x-api-key`; an OAuth access token uses
    /// `Authorization: Bearer` plus the `oauth-2025-04-20` beta. Sending both
    /// auth headers is rejected by the API, so exactly one is set.
    pub fn with_credential(cred: &crate::auth::Credential) -> Result<Self> {
        let api_key = cred.secret().to_string();
        let mut headers = header::HeaderMap::new();
        headers.insert("anthropic-version", ANTHROPIC_VERSION.parse()?);

        let mut credential_betas = Vec::new();
        match cred {
            crate::auth::Credential::ApiKey(k) => {
                headers.insert("x-api-key", k.parse()?);
            }
            crate::auth::Credential::OAuth(token) => {
                headers.insert(
                    header::AUTHORIZATION,
                    format!("Bearer {token}").parse()?,
                );
                credential_betas.push(crate::auth::OAUTH_BETA.to_string());
            }
        }
        headers.insert(header::CONTENT_TYPE, "application/json".parse()?);

        let client = Client::builder()
            .default_headers(headers)
            // 10s connect timeout — fail fast on dead upstreams instead of
            // hanging forever on a black-holed SYN. The overall request
            // timeout is intentionally unset: legitimate Anthropic streams
            // can run for minutes, and reqwest's `.timeout()` covers the
            // whole request including body streaming.
            .connect_timeout(std::time::Duration::from_secs(10))
            .build()
            .context("Failed to build HTTP client")?;

        Ok(Self {
            client,
            api_key,
            base_url: ANTHROPIC_API_BASE.to_string(),
            credential_betas,
        })
    }

    /// Merge the request's betas with any the credential requires.
    /// Returns `None` when there are none, so the header is omitted entirely.
    fn beta_header(&self, request_betas: &[String]) -> Option<String> {
        if request_betas.is_empty() && self.credential_betas.is_empty() {
            return None;
        }
        let mut all: Vec<&str> = Vec::new();
        for b in request_betas.iter().chain(self.credential_betas.iter()) {
            let b = b.as_str();
            if !b.is_empty() && !all.contains(&b) {
                all.push(b);
            }
        }
        if all.is_empty() {
            None
        } else {
            Some(all.join(","))
        }
    }

    /// Non-streaming API call — mirrors callModel() in services/api/claude.ts
    #[allow(dead_code)] // used by SDK/headless mode (non-streaming path)
    pub async fn messages(&self, request: MessagesRequest) -> Result<MessagesResponse> {
        let url = format!("{}/v1/messages", self.base_url);
        debug!("POST {url} model={}", request.model);

        let mut builder = self.client.post(&url).json(&request);
        if let Some(betas) = self.beta_header(&request.betas) {
            builder = builder.header("anthropic-beta", betas);
        }
        if let Some(ref sid) = request.session_id {
            builder = builder.header("X-Claude-Code-Session-Id", sid.as_str());
        }

        let resp = builder.send().await.context("API request failed")?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!("API error {status}: {body}"));
        }

        resp.json::<MessagesResponse>()
            .await
            .context("Failed to parse API response")
    }

    /// Streaming API call — collects all SSE events into a StreamedResponse.
    /// Mirrors the streaming path in services/api/claude.ts.
    /// The `on_text` callback is called with each text delta for live output.
    pub async fn messages_stream(
        &self,
        mut request: MessagesRequest,
        mut on_text: impl FnMut(&str),
    ) -> Result<StreamedResponse> {
        request.stream = Some(true);
        let url = format!("{}/v1/messages", self.base_url);
        debug!("POST {url} stream=true model={}", request.model);

        let mut builder = self.client.post(&url).json(&request);
        if let Some(betas) = self.beta_header(&request.betas) {
            builder = builder.header("anthropic-beta", betas);
        }
        if let Some(ref sid) = request.session_id {
            builder = builder.header("X-Claude-Code-Session-Id", sid.as_str());
        }

        let resp = builder
            .send()
            .await
            .context("Streaming API request failed")?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!("API stream error {status}: {body}"));
        }

        let mut stream = resp.bytes_stream().eventsource();

        // Accumulator state
        let mut result = StreamedResponse::default();
        // Per-block accumulators: index → (type, text/json buffer)
        let mut text_blocks: HashMap<usize, String> = HashMap::with_capacity(4);
        let mut tool_blocks: HashMap<usize, (String, String, String)> = HashMap::with_capacity(4); // id, name, json
        let mut thinking_blocks: HashMap<usize, (String, String)> = HashMap::with_capacity(4); // thinking, sig

        while let Some(event) = next_sse_event(&mut stream).await? {
            if event.data == "[DONE]" {
                break;
            }
            let parsed: StreamEvent = match serde_json::from_str(&event.data) {
                Ok(e) => e,
                Err(e) => {
                    warn!("Failed to parse SSE event: {e}: {}", event.data);
                    continue;
                }
            };

            match parsed {
                StreamEvent::MessageStart { message } => {
                    result.usage.input_tokens = message.usage.input_tokens;
                }
                StreamEvent::ContentBlockStart {
                    index,
                    content_block,
                } => match content_block {
                    StreamContentBlock::Text { text } => {
                        text_blocks.insert(index, text);
                    }
                    StreamContentBlock::ToolUse { id, name } => {
                        tool_blocks.insert(index, (id, name, String::new()));
                    }
                    StreamContentBlock::Thinking { thinking } => {
                        thinking_blocks.insert(index, (thinking, String::new()));
                    }
                },
                StreamEvent::ContentBlockDelta { index, delta } => match delta {
                    ContentDelta::Text { text } => {
                        on_text(&text);
                        text_blocks.entry(index).or_default().push_str(&text);
                    }
                    ContentDelta::InputJson { partial_json } => {
                        if let Some((_, _, json)) = tool_blocks.get_mut(&index) {
                            json.push_str(&partial_json);
                        }
                    }
                    ContentDelta::Thinking { thinking } => {
                        if let Some((t, _)) = thinking_blocks.get_mut(&index) {
                            t.push_str(&thinking);
                        }
                    }
                    ContentDelta::Signature { signature } => {
                        if let Some((_, s)) = thinking_blocks.get_mut(&index) {
                            s.push_str(&signature);
                        }
                    }
                },
                StreamEvent::ContentBlockStop { index } => {
                    if let Some(text) = text_blocks.remove(&index) {
                        // Skip whitespace-only text blocks that appear alongside thinking blocks —
                        // sending them back to the API causes a 400 (v2.1.92 fix).
                        if !text.trim().is_empty() {
                            result.content.push(ContentBlock::Text { text });
                        }
                    } else if let Some((id, name, json)) = tool_blocks.remove(&index) {
                        let mut input: serde_json::Value = serde_json::from_str(&json)
                            .unwrap_or(serde_json::Value::Object(Default::default()));
                        // Normalize: the API sometimes emits array/object fields as
                        // JSON-encoded strings. Re-parse any string values that look
                        // like JSON arrays or objects (v2.1.89/92 fix).
                        normalize_tool_input(&mut input);
                        result
                            .content
                            .push(ContentBlock::ToolUse { id, name, input });
                    } else if let Some((thinking, signature)) = thinking_blocks.remove(&index) {
                        result.content.push(ContentBlock::Thinking {
                            thinking,
                            signature,
                        });
                    }
                }
                StreamEvent::MessageDelta { delta, usage } => {
                    result.stop_reason = delta.stop_reason;
                    if let Some(u) = usage {
                        result.usage.output_tokens = u.output_tokens;
                    }
                }
                StreamEvent::MessageStop | StreamEvent::Ping => {}
                StreamEvent::Error { error } => {
                    return Err(anyhow!("Stream error {}: {}", error.r#type, error.message));
                }
            }
        }

        Ok(result)
    }
}

/// Normalize streamed tool input: when the API emits array/object fields as
/// JSON-encoded strings (e.g. `"[\"a\",\"b\"]"` instead of `["a","b"]`),
/// re-parse them so downstream tools see the intended shape.
/// Fixes double-encoded JSON from the API.
fn normalize_tool_input(val: &mut serde_json::Value) {
    match val {
        serde_json::Value::Object(map) => {
            for (_, v) in map.iter_mut() {
                normalize_tool_input(v);
            }
        }
        serde_json::Value::Array(arr) => {
            for v in arr.iter_mut() {
                normalize_tool_input(v);
            }
        }
        serde_json::Value::String(s) => {
            let trimmed = s.trim_start();
            if (trimmed.starts_with('[') || trimmed.starts_with('{'))
                && let Ok(mut parsed) = serde_json::from_str::<serde_json::Value>(s)
                && matches!(
                    parsed,
                    serde_json::Value::Array(_) | serde_json::Value::Object(_)
                )
            {
                normalize_tool_input(&mut parsed);
                *val = parsed;
            }
        }
        _ => {}
    }
}

pub fn default_model() -> &'static str {
    DEFAULT_MODEL
}

pub fn default_max_tokens() -> u32 {
    DEFAULT_MAX_TOKENS
}

// ─── Unified backend ──────────────────────────────────────────────────────────

/// Routes API calls to the Anthropic, Ollama, or OpenAI-compatible backend
/// based on the model prefix.  All code above the query engine works with
/// `ApiBackend` instead of `ClaudeClient` directly.
#[derive(Clone)]
pub enum ApiBackend {
    Anthropic(ClaudeClient),
    Ollama(OllamaClient),
    OpenAiCompat(OpenAiCompatClient),
}

impl ApiBackend {
    /// Create the right backend for `model`.
    /// `api_key` is required for Anthropic; ignored for Ollama/OpenAI-compat.
    /// `api_key` is the credential secret; `is_oauth` selects the wire format
    /// (`Authorization: Bearer` + oauth beta, vs `x-api-key`). Ignored for
    /// Ollama / OpenAI-compat backends, which carry their own auth.
    pub fn new_with_auth(
        model: &str,
        api_key: &str,
        is_oauth: bool,
        ollama_host: &str,
    ) -> Result<Self> {
        if !is_ollama_model(model) && !is_openai_compat_model(model) && is_oauth {
            return Ok(Self::Anthropic(ClaudeClient::with_credential(
                &crate::auth::Credential::OAuth(api_key.to_string()),
            )?));
        }
        Self::new(model, api_key, ollama_host)
    }

    pub fn new(model: &str, api_key: &str, ollama_host: &str) -> Result<Self> {
        if is_ollama_model(model) {
            Ok(Self::Ollama(OllamaClient::new(ollama_host)?))
        } else if is_openai_compat_model(model) {
            Ok(Self::OpenAiCompat(OpenAiCompatClient::from_model(model)?))
        } else {
            Ok(Self::Anthropic(ClaudeClient::new(api_key)?))
        }
    }

    /// Streaming call — identical interface to `ClaudeClient::messages_stream`.
    pub async fn messages_stream(
        &self,
        request: MessagesRequest,
        on_text: impl FnMut(&str),
    ) -> Result<StreamedResponse> {
        match self {
            Self::Anthropic(c) => c.messages_stream(request, on_text).await,
            Self::Ollama(c) => c.messages_stream(request, on_text).await,
            Self::OpenAiCompat(c) => c.messages_stream(request, on_text).await,
        }
    }

    /// Non-streaming call (falls through to streaming + collect for non-Anthropic backends).
    #[allow(dead_code)] // SDK/headless non-streaming path
    pub async fn messages(&self, request: MessagesRequest) -> Result<MessagesResponse> {
        match self {
            Self::Anthropic(c) => c.messages(request).await,
            Self::Ollama(c) => {
                let streamed = c.messages_stream(request, |_| {}).await?;
                Ok(MessagesResponse {
                    id: uuid::Uuid::new_v4().to_string(),
                    content: streamed.content,
                    stop_reason: streamed.stop_reason,
                    usage: streamed.usage,
                })
            }
            Self::OpenAiCompat(c) => {
                let streamed = c.messages_stream(request, |_| {}).await?;
                Ok(MessagesResponse {
                    id: uuid::Uuid::new_v4().to_string(),
                    content: streamed.content,
                    stop_reason: streamed.stop_reason,
                    usage: streamed.usage,
                })
            }
        }
    }

    /// The Ollama host URL if this is an Ollama backend.
    #[allow(dead_code)]
    pub fn ollama_host(&self) -> Option<&str> {
        match self {
            Self::Ollama(c) => Some(&c.base_url),
            _ => None,
        }
    }

    /// Provider display name (for status bar / logging).
    #[allow(dead_code)]
    pub fn provider_name(&self) -> &str {
        match self {
            Self::Anthropic(_) => "Anthropic",
            Self::Ollama(_) => "Ollama",
            Self::OpenAiCompat(c) => &c.provider_name,
        }
    }

    /// True if the model has been detected as not supporting tools.
    #[allow(dead_code)]
    pub fn tools_disabled(&self) -> bool {
        match self {
            Self::Ollama(c) => c.tools_disabled(),
            Self::OpenAiCompat(c) => c.tools_disabled(),
            Self::Anthropic(_) => false,
        }
    }

    /// Returns true the first time called after tools are disabled — for a one-time user notice.
    pub fn take_tools_notice(&self) -> bool {
        match self {
            Self::Ollama(c) => c.take_tools_notice(),
            Self::OpenAiCompat(c) => c.take_tools_notice(),
            Self::Anthropic(_) => false,
        }
    }
}

#[cfg(test)]
mod credential_tests {
    use super::*;
    use crate::auth::{Credential, OAUTH_BETA};

    /// An OAuth token must go in `Authorization: Bearer`, never `x-api-key` —
    /// and the request additionally needs the oauth beta or it is rejected.
    #[test]
    fn oauth_credential_adds_the_required_beta() {
        let c = ClaudeClient::with_credential(&Credential::OAuth("tok".into())).unwrap();
        assert_eq!(c.beta_header(&[]).as_deref(), Some(OAUTH_BETA));
    }

    /// A static key needs no extra beta, so the header stays absent when the
    /// request itself asked for none.
    #[test]
    fn api_key_credential_adds_no_beta() {
        let c = ClaudeClient::with_credential(&Credential::ApiKey("sk-ant-x".into())).unwrap();
        assert_eq!(c.beta_header(&[]), None);
    }

    /// The credential beta must be *merged* into the request's betas, not sent
    /// as a second `anthropic-beta` header — `RequestBuilder::header` appends.
    #[test]
    fn request_betas_and_credential_betas_merge_into_one_value() {
        let c = ClaudeClient::with_credential(&Credential::OAuth("tok".into())).unwrap();
        let merged = c.beta_header(&["compact-2026-01-12".into()]).unwrap();
        assert!(merged.contains("compact-2026-01-12"), "{merged}");
        assert!(merged.contains(OAUTH_BETA), "{merged}");
        assert_eq!(merged.matches(OAUTH_BETA).count(), 1, "{merged}");
        assert!(!merged.contains(",,"), "{merged}");
    }

    #[test]
    fn duplicate_betas_are_collapsed() {
        let c = ClaudeClient::with_credential(&Credential::OAuth("tok".into())).unwrap();
        let merged = c.beta_header(&[OAUTH_BETA.into()]).unwrap();
        assert_eq!(merged, OAUTH_BETA, "duplicate must collapse: {merged}");
    }

    #[test]
    fn api_key_request_betas_pass_through_untouched() {
        let c = ClaudeClient::with_credential(&Credential::ApiKey("k".into())).unwrap();
        assert_eq!(c.beta_header(&["a".into(), "b".into()]).as_deref(), Some("a,b"));
    }

    /// `ClaudeClient::new` is the legacy raw-key entry point — it must stay
    /// equivalent to an explicit ApiKey credential.
    #[test]
    fn legacy_new_is_equivalent_to_an_api_key_credential() {
        let legacy = ClaudeClient::new("sk-ant-x").unwrap();
        assert_eq!(legacy.beta_header(&[]), None);
        assert_eq!(legacy.api_key, "sk-ant-x");
    }
}

#[cfg(test)]
mod sse_idle_tests {
    use super::*;
    use eventsource_stream::Event;
    use std::convert::Infallible;

    fn event(data: &str) -> Event {
        Event {
            data: data.to_string(),
            ..Default::default()
        }
    }

    /// A stream that yields nothing and never terminates — models a TCP connection
    /// that was silently dropped upstream (NAT reaper, laptop sleep, VPN drop).
    /// Before the idle-timeout guard this hung the caller forever.
    #[tokio::test(start_paused = true)]
    async fn stalled_stream_errors_instead_of_hanging_forever() {
        let mut stream = futures_util::stream::pending::<Result<Event, Infallible>>();

        let err = next_sse_event(&mut stream)
            .await
            .expect_err("a stream that never yields must time out, not hang");

        let msg = err.to_string();
        assert!(msg.contains("stalled"), "diagnostic should say stalled: {msg}");
        assert!(
            msg.contains(&SSE_IDLE_TIMEOUT.as_secs().to_string()),
            "diagnostic should report the budget that elapsed: {msg}"
        );
    }

    /// A stream that goes quiet for less than the budget is healthy and must be
    /// allowed through — long thinking gaps are legitimate, not a stall.
    #[tokio::test(start_paused = true)]
    async fn quiet_period_within_budget_is_not_treated_as_a_stall() {
        let quiet = SSE_IDLE_TIMEOUT - std::time::Duration::from_secs(1);
        let mut stream = Box::pin(futures_util::stream::once(async move {
            tokio::time::sleep(quiet).await;
            Ok::<_, Infallible>(event("late but valid"))
        }));

        let got = next_sse_event(&mut stream).await.expect("must not time out");
        assert_eq!(got.map(|e| e.data).as_deref(), Some("late but valid"));
    }

    #[tokio::test(start_paused = true)]
    async fn clean_end_of_stream_returns_none() {
        let mut stream = futures_util::stream::empty::<Result<Event, Infallible>>();
        assert!(next_sse_event(&mut stream).await.unwrap().is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn events_pass_through_in_order() {
        let mut stream = futures_util::stream::iter(vec![
            Ok::<_, Infallible>(event("first")),
            Ok(event("second")),
        ]);

        assert_eq!(
            next_sse_event(&mut stream).await.unwrap().map(|e| e.data),
            Some("first".into())
        );
        assert_eq!(
            next_sse_event(&mut stream).await.unwrap().map(|e| e.data),
            Some("second".into())
        );
        assert!(next_sse_event(&mut stream).await.unwrap().is_none());
    }

    /// Transport errors must still surface as errors, not be swallowed by the
    /// timeout wrapper.
    #[tokio::test(start_paused = true)]
    async fn transport_error_is_propagated() {
        let mut stream = Box::pin(futures_util::stream::once(async {
            Err::<Event, _>(std::io::Error::other("connection reset"))
        }));

        let err = next_sse_event(&mut stream).await.unwrap_err();
        assert!(err.to_string().contains("connection reset"), "{err}");
    }
}
