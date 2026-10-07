/// Anthropic API client — port of services/api/claude.ts
pub mod ollama;
pub mod openai_compat;
pub mod retry;
pub mod thinking;
pub mod types;

use anyhow::{Context, Result, anyhow};
use eventsource_stream::Eventsource;
use futures_util::StreamExt;
use reqwest::{Client, header};
use std::collections::HashMap;
use tracing::{debug, warn};

pub use ollama::{
    OllamaClient, OllamaProbe, host_reachable, is_ollama_model, list_ollama_models, probe_ollama,
    proxy_applies, strip_ollama_prefix,
};
pub use openai_compat::{
    OpenAiApi, OpenAiCompatClient, PROVIDERS, is_openai_compat_model, parse_provider_model,
};
pub use types::*;

const ANTHROPIC_API_BASE: &str = "https://api.anthropic.com";
const ANTHROPIC_VERSION: &str = "2023-06-01";
const DEFAULT_MODEL: &str = "claude-sonnet-5";
const DEFAULT_MAX_TOKENS: u32 = 8_192;

/// Maximum time to wait for the *next* bytes of an SSE response before declaring
/// the stream dead.
///
/// This is deliberately an inter-chunk budget, not a whole-request timeout: a
/// legitimate response can stream for many minutes, so `.timeout()` on the request
/// would truncate valid work. But a healthy connection always delivers *something* —
/// a content delta, a `ping`, or a keepalive comment — well inside this window.
///
/// Without this bound, a silently dropped TCP connection (NAT idle reaper, laptop
/// sleep, VPN drop) leaves the read future pending forever: the UI hangs with no
/// error and no recovery short of killing the process.
pub(crate) const SSE_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// Bound the gap between body chunks by [`SSE_IDLE_TIMEOUT`]; on expiry yield
/// one `TimedOut` error and end.
///
/// The timer runs on bytes, not parsed events: the SSE parser swallows comment
/// lines without yielding anything, so OpenRouter's `: OPENROUTER PROCESSING`
/// keepalives during a long silent reasoning phase never reset an event-level
/// timer, and the healthy request was aborted as stalled and re-sent.
pub(crate) fn idle_bounded<S, B, E>(
    stream: S,
) -> impl futures_util::Stream<Item = std::result::Result<B, std::io::Error>> + Unpin
where
    S: futures_util::Stream<Item = std::result::Result<B, E>>,
    E: std::fmt::Display,
{
    Box::pin(futures_util::stream::unfold(
        Some(Box::pin(stream)),
        |state| async move {
            let mut stream = state?;
            match tokio::time::timeout(SSE_IDLE_TIMEOUT, stream.next()).await {
                Err(_) => Some((
                    Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        format!(
                            "SSE stream stalled: no data received for {}s — the connection \
                             was likely dropped upstream. Retry the request.",
                            SSE_IDLE_TIMEOUT.as_secs()
                        ),
                    )),
                    None,
                )),
                Ok(None) => None,
                Ok(Some(Ok(bytes))) => Some((Ok(bytes), Some(stream))),
                Ok(Some(Err(e))) => Some((Err(std::io::Error::other(e.to_string())), Some(stream))),
            }
        },
    ))
}

/// Await the next event from an SSE stream built on [`idle_bounded`] bytes.
///
/// Returns `Ok(None)` on clean end-of-stream. Shared by the Anthropic backend and
/// the OpenAI-compatible backend (which also serves Ollama).
pub(crate) async fn next_sse_event<S, E>(
    stream: &mut S,
) -> Result<Option<eventsource_stream::Event>>
where
    S: futures_util::Stream<Item = std::result::Result<eventsource_stream::Event, E>> + Unpin,
    E: std::fmt::Display,
{
    match stream.next().await {
        None => Ok(None),
        Some(Ok(event)) => Ok(Some(event)),
        Some(Err(e)) => {
            let e = e.to_string();
            // reqwest reports a connection that dies mid-body only as "error
            // decoding response body"; name the drop so callers that retry
            // dropped connections recognise it.
            if e.starts_with("Transport error") {
                Err(anyhow!(
                    "SSE stream error: connection dropped mid-stream ({e})"
                ))
            } else {
                Err(anyhow!("SSE stream error: {e}"))
            }
        }
    }
}

/// Drop thinking blocks that carry no signature before history goes to
/// Anthropic. OpenAI-compatible reasoning models (DeepSeek, vLLM, QwQ) produce
/// unsigned `Thinking` blocks, and Anthropic rejects any history containing one
/// with a 400 — so after `/model claude-…` (or resuming such a session) every
/// turn would fail. Genuine Anthropic thinking always has a signature. An
/// assistant turn left empty keeps a placeholder text block, because removing
/// it would put two user turns back to back, which is also a 400.
pub(crate) fn strip_unsigned_thinking(messages: &mut [Message]) {
    for msg in messages {
        let before = msg.content.len();
        msg.content.retain(
            |b| !matches!(b, ContentBlock::Thinking { signature, .. } if signature.is_empty()),
        );
        if msg.content.is_empty() && before > 0 {
            msg.content.push(ContentBlock::Text {
                text: "(no response)".into(),
            });
        }
    }
}

#[derive(Clone)]
pub struct ClaudeClient {
    client: Client,
    api_key: String,
    /// `api_key` is an OAuth token: sent as `Authorization: Bearer`, and
    /// swapped for the newest `ant` profile token when it is one.
    is_oauth: bool,
    /// Where `api_key` is refreshed after a 401: the `ant` profile for an
    /// OAuth token, the apiKeyHelper for a key.
    profile: &'static crate::auth::ProfileTokens,
    base_url: String,
    /// Betas the credential itself requires, merged into every request's
    /// `anthropic-beta`. An OAuth bearer token needs `oauth-2025-04-20`.
    ///
    /// This is *not* a default header: `RequestBuilder::header` appends rather
    /// than replaces, so a default `anthropic-beta` plus a per-request one
    /// would send the field twice. Merging into the single per-request value
    /// keeps exactly one.
    credential_betas: Vec<String>,
    /// Optional sink for retry notices, so a backoff sleep is visible rather
    /// than looking like a hang. Set by the TUI and the headless runner.
    retry_notifier: Option<retry::RetryNotifier>,
    /// Retry an overloaded API (HTTP 529, or an `overloaded_error` event
    /// before any text). Off only where a fallback model takes over instead.
    retry_overloaded: bool,
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

        // The auth header is set per request (`auth_header`), not here: an
        // `ant` profile token is replaced when it expires, and a header baked
        // into the client would keep sending the dead one.
        let mut credential_betas = Vec::new();
        if cred.is_oauth() {
            credential_betas.push(crate::auth::OAUTH_BETA.to_string());
        }
        // Fail at construction on a key that cannot be a header, as before.
        header::HeaderValue::from_str(cred.secret())?;
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
            is_oauth: cred.is_oauth(),
            profile: crate::auth::refreshable(cred.is_oauth()),
            base_url: ANTHROPIC_API_BASE.to_string(),
            credential_betas,
            retry_notifier: None,
            retry_overloaded: true,
        })
    }

    /// Test-only: point the client at a local scripted server.
    ///
    /// Deliberately `#[cfg(test)]`. A runtime base-URL override is a
    /// credential-exfiltration vector, which is exactly why
    /// `ANTHROPIC_BASE_URL` is excluded from the `.env` allowlist in main.rs.
    /// This exists so the retry wiring can be proven without weakening that.
    #[cfg(test)]
    pub(crate) fn set_base_url_for_test(&mut self, url: impl Into<String>) {
        self.base_url = url.into();
    }

    /// Test-only: refresh credentials from `profile` instead of `ant` or
    /// the apiKeyHelper.
    #[cfg(test)]
    pub(crate) fn set_profile_for_test(&mut self, profile: &'static crate::auth::ProfileTokens) {
        self.profile = profile;
    }

    /// The credential to send now; see [`crate::auth::ProfileTokens`].
    fn current_secret(&self) -> String {
        self.profile.live(&self.api_key)
    }

    fn auth_header(
        &self,
        builder: reqwest::RequestBuilder,
        secret: &str,
    ) -> reqwest::RequestBuilder {
        if self.is_oauth {
            builder.header(header::AUTHORIZATION, format!("Bearer {secret}"))
        } else {
            builder.header("x-api-key", secret)
        }
    }

    /// POST `request` with retries. A profile token or helper key refused
    /// with 401 has expired: fetch a fresh one and send once more.
    async fn send(
        &self,
        url: &str,
        request: &MessagesRequest,
        context: &str,
    ) -> Result<reqwest::Response> {
        let betas = self.beta_header(&request.betas);
        let betas = betas.as_deref();
        let body = request_body(request)?;
        let body = &body;
        let attempt = |secret: String| {
            retry::send_with_retry(
                move || {
                    let mut builder = self.auth_header(self.client.post(url).json(body), &secret);
                    if let Some(b) = betas {
                        builder = builder.header("anthropic-beta", b);
                    }
                    if let Some(ref sid) = request.session_id {
                        builder = builder.header("X-Claude-Code-Session-Id", sid.as_str());
                    }
                    builder
                },
                self.retry_notifier.as_ref(),
                self.retry_overloaded,
                context,
            )
        };
        let secret = self.current_secret();
        let resp = attempt(secret.clone()).await?;
        if resp.status() != reqwest::StatusCode::UNAUTHORIZED {
            return Ok(resp);
        }
        let profile = self.profile;
        match tokio::task::spawn_blocking(move || profile.refresh(&secret)).await {
            Ok(Some(fresh)) => attempt(fresh).await,
            _ => Ok(resp),
        }
    }

    /// Install a sink for retry notices. Without one a backoff sleep is
    /// invisible and a rate-limited turn looks like a hang.
    pub fn set_retry_notifier(&mut self, n: retry::RetryNotifier) {
        self.retry_notifier = Some(n);
    }

    /// See [`ClaudeClient::retry_overloaded`].
    pub fn set_retry_overloaded(&mut self, on: bool) {
        self.retry_overloaded = on;
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
    pub async fn messages(&self, mut request: MessagesRequest) -> Result<MessagesResponse> {
        strip_unsigned_thinking(&mut request.messages);
        let url = format!("{}/v1/messages", self.base_url);
        debug!("POST {url} model={}", request.model);

        let resp = self.send(&url, &request, "API request failed").await?;

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
        strip_unsigned_thinking(&mut request.messages);
        let url = format!("{}/v1/messages", self.base_url);
        debug!("POST {url} stream=true model={}", request.model);

        // An overload often arrives as an `overloaded_error` event right after
        // the 200 rather than as a 529. Until a text delta has gone out the
        // request can be re-sent exactly like a 529; after that, never.
        let start = std::time::Instant::now();
        let mut attempt = 0u32;
        loop {
            let mut emitted = false;
            let res = self
                .stream_once(&url, &request, &mut on_text, &mut emitted)
                .await;
            let e = match res {
                Err(e) if self.retry_overloaded && !emitted && is_stream_overloaded(&e) => e,
                other => return other,
            };
            let jitter: f64 = rand::random::<f64>();
            match retry::decide(attempt, true, None, start.elapsed(), jitter) {
                retry::RetryDecision::Retry(delay) => {
                    let notice = retry::RetryNotice {
                        attempt,
                        max_attempts: retry::MAX_ATTEMPTS,
                        delay,
                        reason: retry::describe_status(529),
                    };
                    warn!("Streaming API request failed: {}", notice.message());
                    if let Some(n) = &self.retry_notifier {
                        n(&notice);
                    }
                    tokio::time::sleep(delay).await;
                    attempt += 1;
                }
                retry::RetryDecision::GiveUp(why) => {
                    return Err(anyhow!("{e:#}{}", why.describe()));
                }
            }
        }
    }

    /// One request/stream round of [`ClaudeClient::messages_stream`].
    /// `emitted` turns true once any text has been handed to `on_text`.
    async fn stream_once(
        &self,
        url: &str,
        request: &MessagesRequest,
        on_text: &mut impl FnMut(&str),
        emitted: &mut bool,
    ) -> Result<StreamedResponse> {
        // Retrying is safe here and only here: nothing has been handed to
        // `on_text` yet, so a retry cannot duplicate text the user has seen.
        let resp = self
            .send(url, request, "Streaming API request failed")
            .await?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!("API stream error {status}: {body}"));
        }

        let mut stream = idle_bounded(resp.bytes_stream()).eventsource();

        // Tool schemas by name, so string arguments are only re-parsed where
        // the schema asks for an array or object.
        let schemas: HashMap<&str, &serde_json::Value> = request
            .tools
            .iter()
            .map(|t| (t.name.as_str(), &t.input_schema))
            .collect();

        // Accumulator state
        let mut result = StreamedResponse::default();
        // Per-block accumulators: index → (type, text/json buffer)
        let mut text_blocks: HashMap<usize, String> = HashMap::with_capacity(4);
        let mut tool_blocks: HashMap<usize, (String, String, String)> = HashMap::with_capacity(4); // id, name, json
        let mut thinking_blocks: HashMap<usize, (String, String)> = HashMap::with_capacity(4); // thinking, sig
        // Redacted thinking arrives whole in content_block_start; no deltas.
        let mut redacted_blocks: HashMap<usize, String> = HashMap::new();

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
                    result.usage = message.usage;
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
                    StreamContentBlock::RedactedThinking { data } => {
                        redacted_blocks.insert(index, data);
                    }
                },
                StreamEvent::ContentBlockDelta { index, delta } => match delta {
                    ContentDelta::Text { text } => {
                        *emitted = true;
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
                        // JSON-encoded strings (v2.1.89/92 fix).
                        if let Some(schema) = schemas.get(name.as_str()) {
                            normalize_tool_input(&mut input, schema);
                        }
                        result
                            .content
                            .push(ContentBlock::ToolUse { id, name, input });
                    } else if let Some((thinking, signature)) = thinking_blocks.remove(&index) {
                        result.content.push(ContentBlock::Thinking {
                            thinking,
                            signature,
                        });
                    } else if let Some(data) = redacted_blocks.remove(&index) {
                        result.content.push(ContentBlock::RedactedThinking { data });
                    }
                }
                StreamEvent::MessageDelta { delta, usage } => {
                    result.stop_reason = delta.stop_reason;
                    // Counts here are cumulative; keep start's values where
                    // the delta omits a field.
                    if let Some(u) = usage {
                        let r = &mut result.usage;
                        r.output_tokens = u.output_tokens;
                        r.input_tokens = r.input_tokens.max(u.input_tokens);
                        r.cache_read_input_tokens =
                            r.cache_read_input_tokens.max(u.cache_read_input_tokens);
                        r.cache_creation_input_tokens = r
                            .cache_creation_input_tokens
                            .max(u.cache_creation_input_tokens);
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

/// The `Stream error ...` that an `overloaded_error` SSE event becomes.
fn is_stream_overloaded(e: &anyhow::Error) -> bool {
    e.to_string().starts_with("Stream error overloaded_error")
}

/// Normalize streamed tool input: when the API emits an array/object field as
/// a JSON-encoded string (e.g. `"[\"a\",\"b\"]"` instead of `["a","b"]`),
/// re-parse it. Only properties the schema types as array or object are
/// touched: a string argument that happens to be JSON (Write's `content` for
/// `package.json`) must stay a string.
fn normalize_tool_input(val: &mut serde_json::Value, schema: &serde_json::Value) {
    let (Some(map), Some(props)) = (
        val.as_object_mut(),
        schema.get("properties").and_then(|p| p.as_object()),
    ) else {
        return;
    };
    for (key, v) in map.iter_mut() {
        let Some(prop) = props.get(key) else {
            continue;
        };
        let wants = |t: &str| match prop.get("type") {
            Some(serde_json::Value::String(s)) => s == t,
            Some(serde_json::Value::Array(a)) => {
                a.iter().any(|x| x == t) && !a.iter().any(|x| x == "string")
            }
            _ => false,
        };
        if let serde_json::Value::String(s) = v
            && (wants("array") || wants("object"))
            && let Ok(parsed) = serde_json::from_str::<serde_json::Value>(s)
            && ((parsed.is_array() && wants("array")) || (parsed.is_object() && wants("object")))
        {
            *v = parsed;
        }
        if v.is_object() && wants("object") {
            normalize_tool_input(v, prop);
        }
    }
}

#[cfg(test)]
mod normalize_tests {
    use super::normalize_tool_input;
    use serde_json::json;

    #[test]
    fn json_text_in_a_string_property_stays_a_string() {
        let schema = json!({"properties": {
            "file_path": {"type": "string"},
            "content": {"type": "string"}
        }});
        let mut input = json!({"file_path": "package.json", "content": "{\"name\": \"x\"}"});
        normalize_tool_input(&mut input, &schema);
        assert_eq!(input["content"], "{\"name\": \"x\"}");
    }

    #[test]
    fn double_encoded_array_and_object_properties_are_reparsed() {
        let schema = json!({"properties": {
            "todos": {"type": "array"},
            "opts": {"type": "object", "properties": {"tags": {"type": "array"}}}
        }});
        let mut input = json!({"todos": "[1,2]", "opts": "{\"tags\": \"[\\\"a\\\"]\"}"});
        normalize_tool_input(&mut input, &schema);
        assert_eq!(input["todos"], json!([1, 2]));
        assert_eq!(input["opts"]["tags"], json!(["a"]));
    }
}

pub fn default_model() -> &'static str {
    DEFAULT_MODEL
}

/// Per-turn output cap when the user has not set `maxTokens`. Adaptive
/// thinking shares this allowance with the answer, so 8k leaves a turn that
/// thinks first no room for a large Write. Turns stream, so a 32k cap is safe
/// on every Claude model from 4.5 on; older and non-Claude models (whose
/// caps vary) keep 8k.
pub fn default_max_tokens(model: &str) -> u32 {
    let model = crate::commands::resolve_model_alias(model);
    if thinking::model_version(&model).is_some_and(|v| v >= (4, 5)) {
        32_000
    } else {
        DEFAULT_MAX_TOKENS
    }
}

/// Context window (input tokens) for `model`. Compaction thresholds and every
/// "context % full" display scale with this, so it must not undercount: a
/// 200k guess on a 1M model throws away history at 18% of the real window.
pub fn context_window_for_model(model: &str) -> u64 {
    let m = crate::commands::resolve_model_alias(model).to_lowercase();
    // Bedrock/Vertex ids wrap the first-party id (`us.anthropic.claude-…`,
    // `claude-…@date`), so parse from the `claude-` token on.
    if let Some(i) = m.find("claude-") {
        let id = m[i..].split(['@', ':']).next().unwrap_or_default();
        let one_million = if id.contains("-fable") || id.contains("-mythos") {
            true
        } else if id.contains("-opus") || id.contains("-sonnet") {
            matches!(thinking::model_version(id), Some((major, minor)) if major >= 5 || (major == 4 && minor >= 6))
        } else {
            // Haiku 4.5 and every Claude 3.x model.
            false
        };
        return if one_million { 1_000_000 } else { 200_000 };
    }
    // Gemma before Gemini, so an id naming both is not given 1M: a guess
    // above the real window means compaction never fires before the server
    // rejects the request.
    if m.contains("gemma") {
        // Gemma 3n and Gemma 3 1B before Gemma 3, which "gemma3n" contains.
        if ["gemma-3n", "gemma3n", "gemma-3-1b", "gemma3:1b"]
            .iter()
            .any(|k| m.contains(k))
        {
            32_768
        } else if ["gemma-3", "gemma3"].iter().any(|k| m.contains(k)) {
            131_072
        } else if [
            "gemma-2", "gemma2", "gemma-7b", "gemma-2b", "gemma:7b", "gemma:2b",
        ]
        .iter()
        .any(|k| m.contains(k))
        {
            // Gemma 1 and 2: 8k.
            8_192
        } else {
            // Newer generations: assume the modern window rather than
            // undercount.
            131_072
        }
    } else if m.contains("gemini") {
        // Gemini 2.x and 3 take 1M input tokens on Google's endpoint and
        // through OpenRouter alike.
        1_048_576
    } else if ["gpt-4o", "gpt-4", "llama"].iter().any(|k| m.contains(k)) {
        128_000
    } else if m.contains("deepseek") {
        64_000
    } else if m.contains("mistral") {
        32_000
    } else {
        // Unknown models: assume a 200k Claude-class window.
        200_000
    }
}

/// Whether an API error is the provider rejecting the request as larger than
/// the model's context window. Each backend words it differently: Anthropic
/// says "prompt is too long", OpenAI/Groq/DeepSeek/OpenRouter/Mistral say
/// "maximum context length" or `context_length_exceeded`, Gemini says the
/// input token count "exceeds the maximum number of tokens allowed". Only Anthropic's
/// wording used to be recognised, so on the others an overflowing turn
/// failed instead of compacting, and every later prompt failed the same way.
pub fn is_context_overflow(err: &str) -> bool {
    let e = err.to_lowercase();
    [
        "prompt is too long",
        "prompt_too_long",
        "context_length_exceeded",
        "maximum context length",
        "context length exceeded",
        "exceeds the context window",
        "exceeds the maximum number of tokens allowed",
    ]
    .iter()
    .any(|k| e.contains(k))
}

#[cfg(test)]
mod context_overflow_tests {
    use super::is_context_overflow;

    #[test]
    fn every_backends_overflow_wording_is_recognised() {
        for e in [
            "API stream error 400 Bad Request: prompt is too long: 205290 tokens > 200000 maximum",
            r#"OpenAI error 400 Bad Request: {"error":{"message":"This model's maximum context length is 128000 tokens. However, your messages resulted in 130512 tokens.","type":"invalid_request_error","code":"context_length_exceeded"}}"#,
            r#"DeepSeek error 400 Bad Request: {"error":{"message":"This model's maximum context length is 65536 tokens. However, you requested 70321 tokens (70321 in the messages, 0 in the completion).","type":"invalid_request_error"}}"#,
            r#"Mistral error 400 Bad Request: {"object":"error","message":"Prompt contains 40000 tokens and 0 draft tokens, too large for model with 32768 maximum context length","type":"invalid_request_error"}"#,
            "OpenAI error 400 Bad Request: Your input exceeds the context window of this model.",
            r#"Gemini error 400 Bad Request: [{"error":{"code":400,"message":"The input token count (1100000) exceeds the maximum number of tokens allowed (1048576).","status":"INVALID_ARGUMENT"}}]"#,
        ] {
            assert!(is_context_overflow(e), "{e}");
        }
    }

    #[test]
    fn other_errors_are_not_overflows() {
        for e in [
            "API stream error 401 Unauthorized: invalid x-api-key",
            r#"Groq error 429 Too Many Requests: {"error":{"message":"Rate limit reached"}}"#,
            "OpenAI error 400 Bad Request: max_tokens is too large",
        ] {
            assert!(!is_context_overflow(e), "{e}");
        }
    }
}

#[cfg(test)]
mod context_window_tests {
    use super::context_window_for_model as w;

    #[test]
    fn current_claude_models_have_a_1m_window() {
        for m in [
            "claude-sonnet-5",
            "claude-sonnet-5-5",
            "claude-opus-5-5",
            "claude-opus-4-6",
            "claude-sonnet-4-6",
            "claude-fable-5-1",
            "claude-mythos-5-1",
            "sonnet",
            "opus",
            "us.anthropic.claude-opus-4-8",
        ] {
            assert_eq!(w(m), 1_000_000, "{m}");
        }
    }

    #[test]
    fn haiku_and_older_claude_models_stay_at_200k() {
        for m in [
            "claude-haiku-4-5",
            "haiku",
            "claude-sonnet-4-5-20250929",
            "claude-opus-4-1",
            "claude-3-5-sonnet-20241022",
            "claude-3-7-sonnet-20250219",
            "claude-opus-4-5@20251101",
        ] {
            assert_eq!(w(m), 200_000, "{m}");
        }
    }

    #[test]
    fn non_claude_models_keep_their_table_values() {
        assert_eq!(w("groq:llama-3.3-70b"), 128_000);
        assert_eq!(w("deepseek-chat"), 64_000);
        assert_eq!(w("gemini:gemini-2.5-flash"), 1_048_576);
        assert_eq!(w("openrouter:google/gemini-3-pro-preview"), 1_048_576);
        assert_eq!(w("gemma-7b-it"), 8_192);
        assert_eq!(w("openrouter:google/gemma-3-27b-it"), 131_072);
        assert_eq!(w("openrouter:google/gemma-2-9b-it"), 8_192);
        assert_eq!(w("ollama:gemma3:27b"), 131_072);
        assert_eq!(w("ollama:gemma3:1b"), 32_768);
        assert_eq!(w("openrouter:google/gemma-3n-e4b-it"), 32_768);
        assert_eq!(w("ollama:gemma3n:e4b"), 32_768);
        assert_eq!(w("ollama:gemma3:12b"), 131_072);
        assert_eq!(w("ollama:gemma2:9b"), 8_192);
        assert_eq!(w("ollama:gemma4"), 131_072);
        assert_eq!(w("something-new"), 200_000);
    }
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
    /// `openai_api` is the `openaiApi` setting; see [`OpenAiApi`].
    pub fn new_with_auth(
        model: &str,
        api_key: &str,
        is_oauth: bool,
        ollama_host: &str,
        openai_api: OpenAiApi,
    ) -> Result<Self> {
        if !is_ollama_model(model) && !is_openai_compat_model(model) && is_oauth {
            return Ok(Self::Anthropic(ClaudeClient::with_credential(
                &crate::auth::Credential::OAuth(api_key.to_string()),
            )?));
        }
        Self::new(model, api_key, ollama_host, openai_api)
    }

    pub fn new(
        model: &str,
        api_key: &str,
        ollama_host: &str,
        openai_api: OpenAiApi,
    ) -> Result<Self> {
        if is_ollama_model(model) {
            Ok(Self::Ollama(OllamaClient::new(ollama_host)?))
        } else if is_openai_compat_model(model) {
            Ok(Self::OpenAiCompat(OpenAiCompatClient::from_model(
                model, openai_api,
            )?))
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
        let mut response = match self {
            Self::Anthropic(c) => c.messages_stream(request, on_text).await,
            Self::Ollama(c) => c.messages_stream(request, on_text).await,
            Self::OpenAiCompat(c) => c.messages_stream(request, on_text).await,
        }?;
        response.drop_unanswerable_tool_calls();
        Ok(response)
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

    /// See [`ClaudeClient::set_retry_overloaded`]. Only Anthropic overloads
    /// with a 529.
    pub fn set_retry_overloaded(&mut self, on: bool) {
        if let Self::Anthropic(c) = self {
            c.set_retry_overloaded(on);
        }
    }

    /// Route retry notices to the UI. Applies to every backend that talks to
    /// a remote provider.
    pub fn set_retry_notifier(&mut self, n: retry::RetryNotifier) {
        match self {
            Self::Anthropic(c) => c.set_retry_notifier(n),
            Self::OpenAiCompat(c) => c.set_retry_notifier(n),
            // Ollama is a local daemon: it does not rate limit, and a
            // connection failure there means the server is down, not busy.
            Self::Ollama(_) => {}
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

    /// Returns true the first time called after OpenAI refused reasoning
    /// summaries (an unverified organization) and they were turned off.
    pub fn take_summary_notice(&self) -> bool {
        match self {
            Self::OpenAiCompat(c) => c.take_summary_notice(),
            Self::Ollama(_) | Self::Anthropic(_) => false,
        }
    }
}

/// The JSON body for `request`. With `cache_history` the last block of the
/// final message becomes the third cache breakpoint (tools and system hold
/// the others; the API allows four). Thinking blocks reject cache_control,
/// so the mark goes on the last block that is not one.
fn request_body(request: &MessagesRequest) -> Result<serde_json::Value> {
    let mut body = serde_json::to_value(request)?;
    if request.cache_history
        && let Some(block) = body["messages"]
            .as_array_mut()
            .and_then(|m| m.last_mut())
            .and_then(|m| m["content"].as_array_mut())
            .and_then(|c| {
                c.iter_mut()
                    .rev()
                    .find(|b| !matches!(b["type"].as_str(), Some("thinking" | "redacted_thinking")))
            })
    {
        block["cache_control"] = serde_json::to_value(CacheControl::ephemeral())?;
    }
    Ok(body)
}

#[cfg(test)]
mod redacted_thinking_tests {
    use super::*;
    use crate::query_engine::scripted_api_tests::{serve, sse};
    use serde_json::json;

    fn request(messages: Vec<Message>) -> MessagesRequest {
        MessagesRequest {
            model: "claude-haiku-4-5".into(),
            max_tokens: 16,
            messages,
            system: Default::default(),
            tools: vec![],
            stream: None,
            thinking: None,
            output_config: None,
            betas: vec![],
            session_id: None,
            explicit_max_tokens: false,
            cache_history: false,
        }
    }

    /// A redacted_thinking block failed to parse and vanished, so the tool
    /// round was replayed as a bare tool_use: a 400 on the next request.
    #[tokio::test]
    async fn redacted_thinking_survives_the_stream_and_is_replayed() {
        let first = sse(
            &[
                json!({"type": "redacted_thinking", "data": "ENCRYPTED"}),
                json!({"type": "tool_use", "id": "t1", "name": "Read", "input": {}}),
            ],
            "tool_use",
        );
        let (url, seen) = serve(vec![first, sse(&[], "end_turn")]).await;
        let mut c = ClaudeClient::new("sk-ant-test").unwrap();
        c.set_base_url_for_test(url);

        let user = Message {
            role: Role::User,
            content: vec![ContentBlock::Text { text: "go".into() }],
        };
        let r = c
            .messages_stream(request(vec![user.clone()]), |_| {})
            .await
            .unwrap();
        assert_eq!(
            r.content[0],
            ContentBlock::RedactedThinking {
                data: "ENCRYPTED".into()
            }
        );
        assert!(matches!(r.content[1], ContentBlock::ToolUse { .. }));

        let history = vec![
            user,
            Message {
                role: Role::Assistant,
                content: r.content,
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "t1".into(),
                    content: vec![ToolResultContent::text("ok")],
                    is_error: None,
                }],
            },
        ];
        c.messages_stream(request(history), |_| {}).await.unwrap();
        let body: serde_json::Value = serde_json::from_str(&seen.lock().unwrap()[1]).unwrap();
        assert_eq!(
            body["messages"][1]["content"][0],
            json!({"type": "redacted_thinking", "data": "ENCRYPTED"})
        );
    }
}

/// promptCache marked only tools and system, so every tool-loop round paid
/// full input price for the whole conversation.
#[cfg(test)]
mod cache_history_tests {
    use super::*;
    use crate::query_engine::scripted_api_tests::{serve, sse};
    use serde_json::json;

    fn history() -> Vec<Message> {
        vec![
            Message {
                role: Role::User,
                content: vec![ContentBlock::Text { text: "go".into() }],
            },
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: "t1".into(),
                    name: "Read".into(),
                    input: json!({}),
                }],
            },
            Message {
                role: Role::User,
                content: vec![
                    ContentBlock::ToolResult {
                        tool_use_id: "t1".into(),
                        content: vec![ToolResultContent::text("ok")],
                        is_error: None,
                    },
                    ContentBlock::Text {
                        text: "and then?".into(),
                    },
                ],
            },
        ]
    }

    fn marks(body: &serde_json::Value) -> Vec<(usize, usize)> {
        let mut found = vec![];
        for (i, m) in body["messages"].as_array().unwrap().iter().enumerate() {
            for (j, b) in m["content"].as_array().unwrap().iter().enumerate() {
                if b.get("cache_control").is_some() {
                    found.push((i, j));
                }
            }
        }
        found
    }

    #[tokio::test]
    async fn cache_history_marks_the_end_of_the_conversation() {
        let (url, seen) = serve(vec![sse(&[], "end_turn"), sse(&[], "end_turn")]).await;
        let mut c = ClaudeClient::new("sk-ant-test").unwrap();
        c.set_base_url_for_test(url);
        let mut req = request(history());
        req.cache_history = true;
        c.messages_stream(req.clone(), |_| {}).await.unwrap();
        req.cache_history = false;
        c.messages_stream(req, |_| {}).await.unwrap();

        let seen = seen.lock().unwrap();
        let on: serde_json::Value = serde_json::from_str(&seen[0]).unwrap();
        assert_eq!(marks(&on), vec![(2, 1)]);
        assert_eq!(
            on["messages"][2]["content"][1]["cache_control"],
            json!({"type": "ephemeral"})
        );
        let off: serde_json::Value = serde_json::from_str(&seen[1]).unwrap();
        assert!(marks(&off).is_empty());
    }

    #[test]
    fn cache_mark_skips_trailing_thinking() {
        let mut msgs = history();
        msgs.push(Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Text { text: "hm".into() },
                ContentBlock::Thinking {
                    thinking: "t".into(),
                    signature: "s".into(),
                },
            ],
        });
        let mut req = request(msgs);
        req.cache_history = true;
        let body = request_body(&req).unwrap();
        assert_eq!(marks(&body), vec![(3, 0)]);
    }

    fn request(messages: Vec<Message>) -> MessagesRequest {
        MessagesRequest {
            model: "claude-haiku-4-5".into(),
            max_tokens: 16,
            messages,
            system: Default::default(),
            tools: vec![],
            stream: None,
            thinking: None,
            output_config: None,
            betas: vec![],
            session_id: None,
            explicit_max_tokens: false,
            cache_history: false,
        }
    }
}

#[cfg(test)]
mod strip_thinking_tests {
    use super::*;

    fn thinking(signature: &str) -> ContentBlock {
        ContentBlock::Thinking {
            thinking: "hmm".into(),
            signature: signature.into(),
        }
    }

    #[test]
    fn unsigned_thinking_is_dropped_and_signed_kept() {
        let mut msgs = vec![
            Message {
                role: Role::User,
                content: vec![ContentBlock::Text { text: "hi".into() }],
            },
            Message {
                role: Role::Assistant,
                content: vec![
                    thinking(""),
                    ContentBlock::ToolUse {
                        id: "t1".into(),
                        name: "Read".into(),
                        input: serde_json::json!({}),
                    },
                ],
            },
            Message {
                role: Role::Assistant,
                content: vec![thinking("sig"), ContentBlock::Text { text: "ok".into() }],
            },
        ];
        strip_unsigned_thinking(&mut msgs);
        let json = serde_json::to_string(&msgs).unwrap();
        assert!(!json.contains(r#""signature":"""#), "{json}");
        assert!(json.contains(r#""signature":"sig""#), "{json}");
        assert_eq!(msgs[1].content.len(), 1);
        assert!(matches!(msgs[1].content[0], ContentBlock::ToolUse { .. }));
    }

    /// An assistant turn made only of reasoning must not vanish: that would
    /// leave two consecutive user turns.
    #[test]
    fn thinking_only_turn_keeps_a_placeholder() {
        let mut msgs = vec![Message {
            role: Role::Assistant,
            content: vec![thinking("")],
        }];
        strip_unsigned_thinking(&mut msgs);
        assert_eq!(msgs.len(), 1);
        assert!(
            matches!(&msgs[0].content[..], [ContentBlock::Text { text }] if !text.trim().is_empty())
        );
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
        assert_eq!(
            c.beta_header(&["a".into(), "b".into()]).as_deref(),
            Some("a,b")
        );
    }

    const UNAUTHORIZED: &str = "HTTP/1.1 401 Unauthorized\r\ncontent-length: 0\r\n\
                                connection: close\r\n\r\n";
    const OK_SSE: &str = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                          content-length: 0\r\nconnection: close\r\n\r\n";

    /// Answers each connection with the next scripted response and records
    /// the auth header every request carried.
    async fn recording_server(
        script: Vec<&'static str>,
    ) -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let log = seen.clone();
        tokio::spawn(async move {
            let mut i = 0;
            while let Ok((mut sock, _)) = listener.accept().await {
                let body = *script.get(i).unwrap_or_else(|| script.last().unwrap());
                i += 1;
                let mut buf = vec![0u8; 16384];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).to_ascii_lowercase();
                let auth = req
                    .lines()
                    .find_map(|l| {
                        l.strip_prefix("authorization: ")
                            .or_else(|| l.strip_prefix("x-api-key: "))
                    })
                    .unwrap_or("")
                    .trim()
                    .to_string();
                log.lock().unwrap().push(auth);
                let _ = sock.write_all(body.as_bytes()).await;
                let _ = sock.shutdown().await;
            }
        });
        (format!("http://{addr}"), seen)
    }

    fn request() -> MessagesRequest {
        MessagesRequest {
            model: "claude-opus-5".into(),
            max_tokens: 16,
            messages: vec![],
            system: Default::default(),
            tools: vec![],
            stream: None,
            thinking: None,
            output_config: None,
            betas: vec![],
            session_id: None,
            explicit_max_tokens: false,
            cache_history: false,
        }
    }

    fn fresh_token() -> Option<String> {
        Some("fresh".into())
    }

    /// An `ant` profile token expires mid-session. The 401 must re-run `ant`
    /// and retry, and a client rebuilt later from the startup token (a
    /// `/model` switch, a sub-agent) must send the fresh one straight away.
    #[tokio::test]
    async fn expired_profile_token_is_refreshed_and_retried() {
        static PROFILE: crate::auth::ProfileTokens = crate::auth::ProfileTokens::new(fresh_token);
        PROFILE.register("stale");
        let (url, seen) = recording_server(vec![UNAUTHORIZED, OK_SSE]).await;
        let client = |url: &str| {
            let mut c = ClaudeClient::with_credential(&Credential::OAuth("stale".into())).unwrap();
            c.set_base_url_for_test(url);
            c.set_profile_for_test(&PROFILE);
            c
        };
        let res = client(&url).messages_stream(request(), |_| {}).await;
        assert!(res.is_ok(), "{:?}", res.err());
        assert_eq!(*seen.lock().unwrap(), ["bearer stale", "bearer fresh"]);

        let (url, seen) = recording_server(vec![OK_SSE]).await;
        let res = client(&url).messages_stream(request(), |_| {}).await;
        assert!(res.is_ok(), "{:?}", res.err());
        assert_eq!(*seen.lock().unwrap(), ["bearer fresh"]);
    }

    /// An apiKeyHelper key is short-lived too: a 401 re-runs the helper and
    /// retries with its new key, sent as `x-api-key`.
    #[tokio::test]
    async fn expired_helper_key_is_refreshed_and_retried() {
        fn rotated() -> Option<String> {
            Some("sk-new".into())
        }
        static HELPER: crate::auth::ProfileTokens = crate::auth::ProfileTokens::new(rotated);
        HELPER.register("sk-old");
        let (url, seen) = recording_server(vec![UNAUTHORIZED, OK_SSE]).await;
        let mut c = ClaudeClient::with_credential(&Credential::ApiKey("sk-old".into())).unwrap();
        c.set_base_url_for_test(&url);
        c.set_profile_for_test(&HELPER);
        let res = c.messages_stream(request(), |_| {}).await;
        assert!(res.is_ok(), "{:?}", res.err());
        assert_eq!(*seen.lock().unwrap(), ["sk-old", "sk-new"]);
    }

    /// A static key or ANTHROPIC_AUTH_TOKEN has nothing to refresh from: the
    /// 401 surfaces on the first try.
    #[tokio::test]
    async fn a_401_on_a_credential_without_a_profile_is_not_retried() {
        static PROFILE: crate::auth::ProfileTokens = crate::auth::ProfileTokens::new(fresh_token);
        for cred in [
            Credential::ApiKey("sk-ant-x".into()),
            Credential::OAuth("env-token".into()),
        ] {
            let (url, seen) = recording_server(vec![UNAUTHORIZED, OK_SSE]).await;
            let mut c = ClaudeClient::with_credential(&cred).unwrap();
            c.set_base_url_for_test(&url);
            c.set_profile_for_test(&PROFILE);
            let err = c.messages(request()).await.unwrap_err().to_string();
            assert!(err.contains("401"), "{err}");
            assert_eq!(seen.lock().unwrap().len(), 1, "{cred:?}");
        }
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
        let bytes = futures_util::stream::pending::<Result<Vec<u8>, Infallible>>();
        let mut stream = idle_bounded(bytes).eventsource();

        let err = next_sse_event(&mut stream)
            .await
            .expect_err("a stream that never yields must time out, not hang");

        let msg = err.to_string();
        assert!(
            msg.contains("stalled"),
            "diagnostic should say stalled: {msg}"
        );
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
        let bytes = futures_util::stream::once(async move {
            tokio::time::sleep(quiet).await;
            Ok::<_, Infallible>(b"data: late but valid\n\n".to_vec())
        });
        let mut stream = idle_bounded(bytes).eventsource();

        let got = next_sse_event(&mut stream)
            .await
            .expect("must not time out");
        assert_eq!(got.map(|e| e.data).as_deref(), Some("late but valid"));
    }

    /// OpenRouter sends only `: OPENROUTER PROCESSING` comments while a
    /// reasoning model thinks. The parser yields no event for a comment, so
    /// an event-level timer fired after 120 s of healthy keepalives.
    #[tokio::test(start_paused = true)]
    async fn keepalive_comments_reset_the_idle_timer() {
        let gap = SSE_IDLE_TIMEOUT / 2;
        let bytes = futures_util::stream::unfold(0u32, move |i| async move {
            tokio::time::sleep(gap).await;
            let chunk: &[u8] = match i {
                0..=5 => b": OPENROUTER PROCESSING\n\n",
                6 => b"data: answer\n\n",
                _ => return None,
            };
            Some((Ok::<_, Infallible>(chunk.to_vec()), i + 1))
        });
        let mut stream = idle_bounded(bytes).eventsource();

        let got = next_sse_event(&mut stream)
            .await
            .expect("keepalives must keep the stream alive");
        assert_eq!(got.map(|e| e.data).as_deref(), Some("answer"));
        assert!(next_sse_event(&mut stream).await.unwrap().is_none());
    }

    /// A connection reset mid-body reaches the parser as a body-decode
    /// error whose text never mentions the connection, so the TUI's
    /// dropped-connection retry never matched it.
    #[tokio::test(start_paused = true)]
    async fn a_mid_stream_transport_error_names_the_dropped_connection() {
        let bytes = futures_util::stream::iter(vec![
            Ok(b"data: first\n\n".to_vec()),
            Err("error decoding response body"),
        ]);
        let mut stream = idle_bounded(bytes).eventsource();
        assert!(next_sse_event(&mut stream).await.unwrap().is_some());
        let err = next_sse_event(&mut stream).await.unwrap_err().to_string();
        assert!(err.contains("connection dropped"), "{err}");
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
        let bytes = futures_util::stream::once(async {
            Err::<Vec<u8>, _>(std::io::Error::other("connection reset"))
        });
        let mut stream = idle_bounded(bytes).eventsource();

        let err = next_sse_event(&mut stream).await.unwrap_err();
        assert!(err.to_string().contains("connection reset"), "{err}");
    }
}
