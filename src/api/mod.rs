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

pub use ollama::{OllamaClient, is_ollama_model, list_ollama_models, strip_ollama_prefix};
pub use openai_compat::configured_providers;
pub use openai_compat::{
    KeyValidation, OpenAiCompatClient, PROVIDERS, ProviderDef, is_openai_compat_model,
    parse_provider_model, provider_by_prefix, validate_key,
};
pub use types::*;

const ANTHROPIC_API_BASE: &str = "https://api.anthropic.com";

/// `GET /v1/models` with the key. 10 s timeout. Same verdicts as
/// `validate_key` so the two key flows can share their retry loop.
pub async fn validate_anthropic_key(key: &str) -> KeyValidation {
    let client = match Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
    {
        Ok(c) => c,
        Err(e) => return KeyValidation::Unverified(e.to_string()),
    };
    let req = client
        .get(format!("{ANTHROPIC_API_BASE}/v1/models"))
        .header("x-api-key", key)
        .header("anthropic-version", ANTHROPIC_VERSION);
    match req.send().await {
        Ok(resp) => match resp.status().as_u16() {
            200 => KeyValidation::Valid,
            s @ (401 | 403) => KeyValidation::Rejected(s),
            s => KeyValidation::Unverified(format!("HTTP {s} from {ANTHROPIC_API_BASE}/v1/models")),
        },
        Err(e) => KeyValidation::Unverified(e.to_string()),
    }
}
const ANTHROPIC_VERSION: &str = "2023-06-01";
const DEFAULT_MODEL: &str = "claude-sonnet-5";
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
pub(crate) async fn next_sse_event<S, E>(
    stream: &mut S,
) -> Result<Option<eventsource_stream::Event>>
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
pub struct ClaudeClient {
    client: Client,
    auth: crate::auth::AuthHandle,
    base_url: String,
    /// Optional sink for retry notices, so a backoff sleep is visible rather
    /// than looking like a hang. Set by the TUI and the headless runner.
    retry_notifier: Option<retry::RetryNotifier>,
}

impl ClaudeClient {
    /// Construct from a static API key. Kept as public API for library
    /// consumers and tests; in-tree callers go through `with_auth` /
    /// `ApiBackend::from_config` so the credential can self-refresh.
    #[allow(dead_code)]
    pub fn new(api_key: impl Into<String>) -> Result<Self> {
        Self::with_credential(&crate::auth::Credential::ApiKey(api_key.into()))
    }

    /// Construct from a resolved static credential. Kept as public API for
    /// library consumers and tests; see `new` above.
    #[allow(dead_code)]
    pub fn with_credential(cred: &crate::auth::Credential) -> Result<Self> {
        Self::with_auth(crate::auth::AuthHandle::static_credential(cred.clone()))
    }

    /// Construct from a credential handle. Authentication headers are set per
    /// request (never as client defaults) so a refreshed profile token is
    /// picked up without rebuilding the client.
    pub fn with_auth(auth: crate::auth::AuthHandle) -> Result<Self> {
        let mut headers = header::HeaderMap::new();
        headers.insert("anthropic-version", ANTHROPIC_VERSION.parse()?);
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
            auth,
            base_url: ANTHROPIC_API_BASE.to_string(),
            retry_notifier: None,
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

    /// Install a sink for retry notices. Without one a backoff sleep is
    /// invisible and a rate-limited turn looks like a hang.
    pub fn set_retry_notifier(&mut self, n: retry::RetryNotifier) {
        self.retry_notifier = Some(n);
    }

    /// Merge the request's betas with any the credential requires.
    /// Returns `None` when there are none, so the header is omitted entirely.
    fn beta_header(&self, request_betas: &[String]) -> Option<String> {
        let credential_betas: &[&str] = if self.auth.is_oauth() {
            &[crate::auth::OAUTH_BETA]
        } else {
            &[]
        };
        if request_betas.is_empty() && credential_betas.is_empty() {
            return None;
        }
        let mut all: Vec<&str> = Vec::new();
        for b in request_betas
            .iter()
            .map(String::as_str)
            .chain(credential_betas.iter().copied())
        {
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

    fn apply_auth(
        builder: reqwest::RequestBuilder,
        cred: &crate::auth::Credential,
    ) -> reqwest::RequestBuilder {
        match cred {
            crate::auth::Credential::ApiKey(k) => builder.header("x-api-key", k.as_str()),
            crate::auth::Credential::OAuth(t) => builder.bearer_auth(t),
        }
    }

    /// POST `/v1/messages` with the current credential. A 401 on a profile
    /// credential forces one refresh and one retry; anything else is returned
    /// to the caller as-is.
    async fn post_messages(
        &self,
        url: &str,
        request: &MessagesRequest,
        context: &str,
    ) -> Result<reqwest::Response> {
        let betas = self.beta_header(&request.betas);
        let mut cred = self.auth.credential().await?;
        for attempt in 0..2 {
            let resp = retry::send_with_retry(
                || {
                    let mut b = Self::apply_auth(self.client.post(url).json(request), &cred);
                    if let Some(ref b2) = betas {
                        b = b.header("anthropic-beta", b2.as_str());
                    }
                    if let Some(ref sid) = request.session_id {
                        b = b.header("X-Claude-Code-Session-Id", sid.as_str());
                    }
                    b
                },
                self.retry_notifier.as_ref(),
                context,
            )
            .await?;
            if resp.status() == reqwest::StatusCode::UNAUTHORIZED
                && self.auth.is_profile()
                && attempt == 0
            {
                debug!("401 on profile credential — refreshing and retrying once");
                cred = self.auth.force_refresh().await?;
                continue;
            }
            return Ok(resp);
        }
        unreachable!("loop returns on the second attempt")
    }

    /// Non-streaming API call — mirrors callModel() in services/api/claude.ts
    #[allow(dead_code)] // used by SDK/headless mode (non-streaming path)
    pub async fn messages(&self, request: MessagesRequest) -> Result<MessagesResponse> {
        let url = format!("{}/v1/messages", self.base_url);
        debug!("POST {url} model={}", request.model);
        let resp = self
            .post_messages(&url, &request, "API request failed")
            .await?;
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

        // Retrying is safe here and only here: nothing has been handed to
        // `on_text` yet, so a retry cannot duplicate text the user has seen.
        let resp = self
            .post_messages(&url, &request, "Streaming API request failed")
            .await?;

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
    ///
    /// Kept as public API for library consumers and tests; in-tree callers
    /// use `from_config` so the credential can self-refresh mid-session.
    #[allow(dead_code)]
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

    /// Kept as public API for library consumers and tests; see `new_with_auth`.
    #[allow(dead_code)]
    pub fn new(model: &str, api_key: &str, ollama_host: &str) -> Result<Self> {
        if is_ollama_model(model) {
            Ok(Self::Ollama(OllamaClient::new(ollama_host)?))
        } else if is_openai_compat_model(model) {
            Ok(Self::OpenAiCompat(OpenAiCompatClient::from_model(model)?))
        } else {
            Ok(Self::Anthropic(ClaudeClient::new(api_key)?))
        }
    }

    /// The right backend for `config.model`, using the live credential handle
    /// for Anthropic and the process environment for other providers.
    pub fn from_config(config: &crate::config::Config) -> Result<Self> {
        let model = config.model.as_str();
        if is_ollama_model(model) {
            Ok(Self::Ollama(OllamaClient::new(&config.ollama_host)?))
        } else if is_openai_compat_model(model) {
            let ks = config.keystore.lookup();
            let lookup = |k: &str| ks(k).or_else(|| std::env::var(k).ok());
            Ok(Self::OpenAiCompat(OpenAiCompatClient::from_model_with(
                model, &lookup,
            )?))
        } else {
            Ok(Self::Anthropic(ClaudeClient::with_auth(
                config.auth.clone(),
            )?))
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
}

#[cfg(test)]
mod auth_header_tests {
    use super::*;
    use crate::auth::{AuthHandle, Credential, OAUTH_BETA};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn oauth_credential_adds_the_required_beta() {
        let c = ClaudeClient::with_credential(&Credential::OAuth("tok".into())).unwrap();
        assert_eq!(c.beta_header(&[]).as_deref(), Some(OAUTH_BETA));
    }

    #[test]
    fn api_key_credential_adds_no_beta() {
        let c = ClaudeClient::new("sk-ant-test").unwrap();
        assert_eq!(c.beta_header(&[]), None);
    }

    #[test]
    fn oauth_beta_merges_with_request_betas_without_duplicates() {
        let c = ClaudeClient::with_credential(&Credential::OAuth("tok".into())).unwrap();
        let merged = c.beta_header(&["compact-2026-01-12".into()]).unwrap();
        assert!(merged.contains(OAUTH_BETA), "{merged}");
        assert!(merged.contains("compact-2026-01-12"), "{merged}");
        assert_eq!(merged.matches(OAUTH_BETA).count(), 1, "{merged}");
        let merged = c.beta_header(&[OAUTH_BETA.into()]).unwrap();
        assert_eq!(merged, OAUTH_BETA, "duplicate must collapse: {merged}");
    }

    /// Scripted server: each accepted connection gets the next response and
    /// the raw request is stored for inspection.
    async fn scripted(
        script: Vec<&'static str>,
    ) -> (
        String,
        Arc<tokio::sync::Mutex<Vec<String>>>,
        Arc<AtomicUsize>,
    ) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let hits = Arc::new(AtomicUsize::new(0));
        let (seen2, hits2) = (seen.clone(), hits.clone());
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let i = hits2.fetch_add(1, Ordering::SeqCst);
                let body = *script.get(i).unwrap_or_else(|| script.last().unwrap());
                let seen3 = seen2.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 16384];
                    let n = sock.read(&mut buf).await.unwrap_or(0);
                    seen3
                        .lock()
                        .await
                        .push(String::from_utf8_lossy(&buf[..n]).to_string());
                    let _ = sock.write_all(body.as_bytes()).await;
                    let _ = sock.shutdown().await;
                });
            }
        });
        (format!("http://{addr}"), seen, hits)
    }

    const UNAUTHORIZED: &str = "HTTP/1.1 401 Unauthorized\r\ncontent-type: application/json\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}";
    const TOKEN_OK: &str = "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 68\r\nconnection: close\r\n\r\n{\"access_token\":\"at-new\",\"refresh_token\":\"rt-new\",\"expires_in\":3600}";
    const MSG_OK: &str = "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 141\r\nconnection: close\r\n\r\n{\"id\":\"m\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"x\",\"content\":[],\"stop_reason\":\"end_turn\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}";

    fn req() -> MessagesRequest {
        MessagesRequest {
            model: "claude-opus-5".into(),
            max_tokens: 8,
            messages: vec![],
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn api_key_goes_in_x_api_key_on_every_request() {
        let (base, seen, _) = scripted(vec![MSG_OK]).await;
        let mut c = ClaudeClient::new("sk-ant-test").unwrap();
        c.set_base_url_for_test(base);
        c.messages(req()).await.unwrap();
        let r = seen.lock().await[0].to_lowercase();
        assert!(r.contains("x-api-key: sk-ant-test"), "{r}");
        assert!(!r.contains("authorization:"), "{r}");
    }

    #[tokio::test]
    async fn a_401_on_a_profile_forces_one_refresh_and_one_retry() {
        // Same server answers both /v1/messages and /v1/oauth/token, in order:
        // messages → 401, token → 200, messages → 200.
        let (base, seen, hits) = scripted(vec![UNAUTHORIZED, TOKEN_OK, MSG_OK]).await;
        let d = tempfile::tempdir().unwrap();
        let creds = crate::auth::profile::ProfileCredentials::new(
            "at-old",
            Some("rt-old".into()),
            Some(9_999_999_999),
        );
        let handle = AuthHandle::profile(d.path().to_path_buf(), "default".into(), None, creds)
            .with_base_url(base.clone());
        let mut c = ClaudeClient::with_auth(handle).unwrap();
        c.set_base_url_for_test(base);
        c.messages(req()).await.unwrap();
        assert_eq!(hits.load(Ordering::SeqCst), 3);
        let seen = seen.lock().await;
        assert!(
            seen[0]
                .to_lowercase()
                .contains("authorization: bearer at-old"),
            "{}",
            seen[0]
        );
        assert!(seen[1].starts_with("POST /v1/oauth/token"), "{}", seen[1]);
        assert!(
            seen[2]
                .to_lowercase()
                .contains("authorization: bearer at-new"),
            "{}",
            seen[2]
        );
        assert!(
            seen[2]
                .to_lowercase()
                .contains("anthropic-beta: oauth-2025-04-20"),
            "{}",
            seen[2]
        );
    }

    #[tokio::test]
    async fn a_401_on_a_static_key_is_not_retried() {
        let (base, _, hits) = scripted(vec![UNAUTHORIZED]).await;
        let mut c = ClaudeClient::new("sk-ant-test").unwrap();
        c.set_base_url_for_test(base);
        let err = c.messages(req()).await.unwrap_err().to_string();
        assert!(err.contains("401"), "{err}");
        assert_eq!(hits.load(Ordering::SeqCst), 1);
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
        let mut stream = Box::pin(futures_util::stream::once(async move {
            tokio::time::sleep(quiet).await;
            Ok::<_, Infallible>(event("late but valid"))
        }));

        let got = next_sse_event(&mut stream)
            .await
            .expect("must not time out");
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
