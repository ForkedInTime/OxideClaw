/// Ollama backend — translates between Anthropic's /v1/messages format
/// and Ollama's OpenAI-compatible /v1/chat/completions format.
///
/// Usage: any model prefixed with "ollama:" is routed here instead of
/// the Anthropic API.  The translation is transparent to the rest of the
/// codebase — callers receive the same `StreamedResponse` they would get
/// from `ClaudeClient`.
///
///   /model ollama:dolphin3
///   /model ollama:qwen3:14b
///   /model ollama:llama4
use anyhow::{Context, Result, anyhow};
use reqwest::Client;
use serde::Deserialize;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use tracing::debug;

use crate::api::openai_compat::{
    OaiRequest, OaiStreamOptions, parse_oai_stream, patch_system_no_tools, system_to_string,
    translate_messages, translate_tools,
};
use crate::api::types::*;

// ─── Prefix helpers ───────────────────────────────────────────────────────────

pub const OLLAMA_PREFIX: &str = "ollama:";

pub fn is_ollama_model(model: &str) -> bool {
    model.starts_with(OLLAMA_PREFIX)
}

/// Strip "ollama:" prefix → bare model name for the Ollama API.
pub fn strip_ollama_prefix(model: &str) -> &str {
    model.strip_prefix(OLLAMA_PREFIX).unwrap_or(model)
}

// ─── Model discovery ──────────────────────────────────────────────────────────

/// Ask the local Ollama instance for its installed model list.
/// Returns `ollama:<name>` prefixed strings ready for use as config.model.
/// Returns an empty Vec if Ollama is not running or unreachable.
pub async fn list_ollama_models(base_url: &str) -> Vec<String> {
    #[derive(Deserialize)]
    struct OllamaModel {
        name: String,
    }
    #[derive(Deserialize)]
    struct TagsResponse {
        models: Vec<OllamaModel>,
    }

    let url = format!("{base_url}/api/tags");
    let client = Client::new();
    match client
        .get(&url)
        .timeout(std::time::Duration::from_secs(2))
        .send()
        .await
    {
        Err(_) => vec![],
        Ok(resp) if !resp.status().is_success() => vec![],
        Ok(resp) => match resp.json::<TagsResponse>().await {
            Err(_) => vec![],
            Ok(data) => data
                .models
                .into_iter()
                .map(|m| format!("{OLLAMA_PREFIX}{}", m.name))
                .collect(),
        },
    }
}

/// What a quick look at the local Ollama found, for starting without an
/// Anthropic credential.
#[derive(Debug, PartialEq, Eq)]
pub enum OllamaProbe {
    /// Not running, not answering within the budget, or not Ollama.
    Unreachable,
    /// Running, but nothing has been pulled.
    NoModels,
    /// The installed model to start with, bare (no `ollama:` prefix).
    Model(String),
}

/// Families that handle tool calls, best first. Used when `/api/show` does
/// not report capabilities (older Ollama) or reports `tools` for none.
const TOOL_FAMILIES: &[&str] = &[
    "qwen3-coder",
    "qwen2.5-coder",
    "qwen3",
    "llama3.1",
    "llama3.2",
    "mistral-nemo",
    "mistral",
];

/// Ask Ollama at `base_url` for its models and pick one that can drive
/// tools. Everything, `/api/show` included, finishes within `budget`: a
/// dead or firewalled host must not hold up startup. When the capability
/// lookups run out of time, the choice falls back to the family list.
pub async fn probe_ollama(base_url: &str, budget: std::time::Duration) -> OllamaProbe {
    #[derive(Deserialize)]
    struct OllamaModel {
        name: String,
    }
    #[derive(Deserialize)]
    struct TagsResponse {
        models: Vec<OllamaModel>,
    }
    #[derive(Deserialize)]
    struct ShowResponse {
        #[serde(default)]
        capabilities: Option<Vec<String>>,
    }

    let deadline = tokio::time::Instant::now() + budget;
    let Ok(client) = Client::builder()
        .dns_resolver(Arc::new(DetachedResolver))
        .build()
    else {
        return OllamaProbe::Unreachable;
    };
    let tags = async {
        let resp = client
            .get(format!("{base_url}/api/tags"))
            .send()
            .await
            .ok()?;
        if !resp.status().is_success() {
            return None;
        }
        resp.json::<TagsResponse>().await.ok()
    };
    let names: Vec<String> = match tokio::time::timeout_at(deadline, tags).await {
        Ok(Some(t)) => t.models.into_iter().map(|m| m.name).collect(),
        _ => return OllamaProbe::Unreachable,
    };
    if names.is_empty() {
        return OllamaProbe::NoModels;
    }

    let show = |name: &str| {
        let req = client
            .post(format!("{base_url}/api/show"))
            .json(&serde_json::json!({ "model": name }));
        async move {
            let resp = req.send().await.ok()?;
            if !resp.status().is_success() {
                return None;
            }
            resp.json::<ShowResponse>().await.ok()?.capabilities
        }
    };
    // The deadline is per lookup: one model Ollama is slow to describe must
    // not throw away what the others already reported.
    let caps = futures_util::future::join_all(names.iter().map(|n| {
        let lookup = show(n);
        async move {
            tokio::time::timeout_at(deadline, lookup)
                .await
                .ok()
                .flatten()
        }
    }))
    .await;
    let models: Vec<(String, Option<Vec<String>>)> = names.into_iter().zip(caps).collect();
    match pick_tool_model(&models) {
        Some(m) => OllamaProbe::Model(m.to_string()),
        // Only embedding models: nothing that can hold a conversation.
        None => OllamaProbe::NoModels,
    }
}

/// Resolves host names on a detached thread. reqwest's default resolver runs
/// `getaddrinfo` under `spawn_blocking`, and dropping the runtime waits for
/// that: an `OLLAMA_HOST` name with DNS down would hold the missing-credential
/// error at exit until the resolver times out, long after the probe gave up.
/// A detached thread is simply abandoned when the process exits.
struct DetachedResolver;

impl reqwest::dns::Resolve for DetachedResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_string();
        Box::pin(async move {
            let addrs = resolve_detached(move || {
                use std::net::ToSocketAddrs;
                // Port 0: the connector fills in the URL's port.
                (host.as_str(), 0).to_socket_addrs().map(Iterator::collect)
            })
            .await?;
            Ok(Box::new(addrs.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

/// Run a blocking `lookup` on its own detached thread and await its result.
async fn resolve_detached<F>(lookup: F) -> std::io::Result<Vec<std::net::SocketAddr>>
where
    F: FnOnce() -> std::io::Result<Vec<std::net::SocketAddr>> + Send + 'static,
{
    let (tx, rx) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name("ollama-probe-dns".into())
        .spawn(move || {
            let _ = tx.send(lookup());
        })?;
    rx.await
        .unwrap_or_else(|_| Err(std::io::Error::other("DNS lookup thread died")))
}

/// The model to start with: one whose reported capabilities include
/// `tools`, then a known tool-capable family, then the first model that can
/// chat. `caps` is `None` where `/api/show` did not report capabilities.
/// `None` when every model is one that cannot chat (embeddings only).
fn pick_tool_model(models: &[(String, Option<Vec<String>>)]) -> Option<&str> {
    let has = |caps: &Option<Vec<String>>, c: &str| {
        caps.as_ref().is_some_and(|v| v.iter().any(|x| x == c))
    };
    // `qwen3-coder:30b`, `library/qwen3:8b` → the family name.
    fn family(name: &str) -> String {
        let base = name.split(':').next().unwrap_or(name);
        base.rsplit('/').next().unwrap_or(base).to_ascii_lowercase()
    }
    fn by_family<'a>(pool: &[&'a (String, Option<Vec<String>>)]) -> Option<&'a str> {
        TOOL_FAMILIES.iter().find_map(|f| {
            pool.iter()
                .find(|(name, _)| family(name) == *f)
                .map(|(name, _)| name.as_str())
        })
    }

    let tool_capable: Vec<_> = models.iter().filter(|(_, c)| has(c, "tools")).collect();
    if let Some((first, _)) = tool_capable.first() {
        return by_family(&tool_capable).or(Some(first.as_str()));
    }
    // An embedding-only model reports capabilities without `completion`.
    let chat: Vec<_> = models
        .iter()
        .filter(|(_, c)| c.is_none() || has(c, "completion"))
        .collect();
    by_family(&chat).or_else(|| chat.first().map(|(name, _)| name.as_str()))
}

/// Check whether `model` (bare, without prefix) exists in Ollama.
// ─── Ollama client ────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct OllamaClient {
    client: Client,
    pub base_url: String,
    /// Set to true after the first 400 "does not support tools" — skips tools on all future calls.
    no_tools: Arc<AtomicBool>,
    /// Set to true after the user has already been notified about text-only mode.
    tools_notice_sent: Arc<AtomicBool>,
}

impl OllamaClient {
    pub fn new(base_url: impl Into<String>) -> Result<Self> {
        let client = Client::builder()
            // 5s connect timeout — Ollama is local so a dead daemon should
            // be detected quickly. No overall timeout because model warmup
            // + long generation is normal and should not be truncated.
            .connect_timeout(std::time::Duration::from_secs(5))
            .build()
            .context("Failed to build Ollama HTTP client")?;
        Ok(Self {
            client,
            base_url: base_url.into(),
            no_tools: Arc::new(AtomicBool::new(false)),
            tools_notice_sent: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Returns true if this model has been detected as not supporting tools.
    #[allow(dead_code)]
    pub fn tools_disabled(&self) -> bool {
        self.no_tools.load(Ordering::Relaxed)
    }

    /// Returns true the first time called after tools are disabled — used to send a one-time notice.
    pub fn take_tools_notice(&self) -> bool {
        self.no_tools.load(Ordering::Relaxed)
            && !self.tools_notice_sent.swap(true, Ordering::Relaxed)
    }

    /// Streaming call to Ollama — translates request and response.
    /// Drop-in replacement for `ClaudeClient::messages_stream`.
    pub async fn messages_stream(
        &self,
        request: MessagesRequest,
        on_text: impl FnMut(&str),
    ) -> Result<StreamedResponse> {
        let model = strip_ollama_prefix(&request.model).to_string();
        let url = format!("{}/v1/chat/completions", self.base_url);
        debug!("POST {url} model={model} (via Ollama)");

        let no_tools = self.no_tools.load(Ordering::Relaxed);
        let system_str = system_to_string(&request.system);

        let system = if no_tools {
            patch_system_no_tools(&system_str)
        } else {
            system_str.clone()
        };

        let oai_messages = translate_messages(&system, &request.messages, false, false);
        let oai_tools = if no_tools {
            vec![]
        } else {
            translate_tools(&request.tools)
        };

        let mut oai_request = OaiRequest {
            model,
            messages: oai_messages,
            tools: oai_tools,
            stream: true,
            stream_options: Some(OaiStreamOptions {
                include_usage: true,
            }),
            // Ollama maps this to num_predict, which is unlimited by
            // default; a default cap would stop thinking models mid-thought.
            max_tokens: request.explicit_max_tokens.then_some(request.max_tokens),
            max_completion_tokens: None,
        };

        let resp = self
            .client
            .post(&url)
            .json(&oai_request)
            .send()
            .await
            .context("Ollama request failed — is Ollama running?")?;

        let status = resp.status();
        let resp = if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            // If the model doesn't support tools, cache that and retry without them
            if status.as_u16() == 400 && body.contains("does not support tools") {
                self.no_tools.store(true, Ordering::Relaxed);
                debug!("Model does not support tools — disabling tools for this session");
                let patched_system = patch_system_no_tools(&system_str);
                oai_request.messages =
                    translate_messages(&patched_system, &request.messages, false, false);
                oai_request.tools = vec![];
                self.client
                    .post(&url)
                    .json(&oai_request)
                    .send()
                    .await
                    .context("Ollama request failed — is Ollama running?")?
            } else {
                return Err(anyhow!("Ollama error {status}: {body}"));
            }
        } else {
            resp
        };

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!("Ollama error {status}: {body}"));
        }

        let (result, _) = parse_oai_stream(resp, on_text).await?;
        Ok(result)
    }
}

/// A stand-in for Ollama's `/api/tags` and `/api/show`, shared with the
/// startup fallback tests in `config`.
#[cfg(test)]
pub(crate) mod fake_server {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// How `/api/show` answers.
    #[derive(Clone)]
    pub(crate) enum Show {
        /// Capabilities per model; a model not listed gets `{}`, as an
        /// Ollama too old to report them answers.
        Caps(HashMap<&'static str, Vec<&'static str>>),
        /// Never answers: the lookups must give up at the budget.
        Hang,
        /// Answers like `Caps`, except for the one model whose lookup never
        /// returns, as when Ollama is busy loading it.
        HangOn(&'static str, HashMap<&'static str, Vec<&'static str>>),
    }

    /// Serve `models` until the test ends. Returns the base URL and the
    /// request lines seen, so a test can assert Ollama was never asked.
    pub(crate) async fn start(models: &[&str], show: Show) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let names: Vec<String> = models
            .iter()
            .map(|m| format!("{{\"name\":\"{m}\"}}"))
            .collect();
        let tags = format!("{{\"models\":[{}]}}", names.join(","));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let (tags, show, log) = (tags.clone(), show.clone(), log.clone());
                tokio::spawn(async move {
                    let req = read_request(&mut sock).await;
                    let line = req.lines().next().unwrap_or("").to_string();
                    log.lock().unwrap().push(line.clone());
                    let body = if line.starts_with("GET /api/tags") {
                        tags
                    } else if line.starts_with("POST /api/show") {
                        let body = req.split("\r\n\r\n").nth(1).unwrap_or("");
                        let v: serde_json::Value = serde_json::from_str(body).unwrap();
                        let model = v["model"].as_str().unwrap_or("");
                        let caps = match show {
                            Show::Caps(caps) => caps,
                            Show::HangOn(slow, caps) if slow != model => caps,
                            _ => return std::future::pending::<()>().await,
                        };
                        match caps.get(model) {
                            Some(c) => serde_json::json!({ "capabilities": c }).to_string(),
                            None => "{}".to_string(),
                        }
                    } else {
                        let _ = sock
                            .write_all(b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\n\r\n")
                            .await;
                        return;
                    };
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
                         content-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                    let _ = sock.shutdown().await;
                });
            }
        });
        (format!("http://{addr}"), seen)
    }

    /// Headers plus a `content-length` body, however the reads split them.
    async fn read_request(sock: &mut tokio::net::TcpStream) -> String {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let n = sock.read(&mut chunk).await.unwrap_or(0);
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
            let text = String::from_utf8_lossy(&buf).to_string();
            if let Some(end) = text.find("\r\n\r\n") {
                let len = text[..end]
                    .lines()
                    .find_map(|l| {
                        l.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                    })
                    .unwrap_or(0);
                if buf.len() >= end + 4 + len {
                    return text;
                }
            }
        }
        String::from_utf8_lossy(&buf).to_string()
    }

    /// A port nothing listens on: connections are refused at once.
    pub(crate) async fn closed_port() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        format!("http://{addr}")
    }

    /// Accepts connections and never answers, like a host behind a
    /// firewall that swallows packets after the handshake.
    pub(crate) async fn silent() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((sock, _)) = listener.accept().await {
                held.push(sock);
            }
        });
        format!("http://{addr}")
    }
}

#[cfg(test)]
mod tests {
    use super::fake_server::{self, Show};
    use super::*;
    use std::collections::HashMap;
    use std::time::{Duration, Instant};

    fn models(list: &[(&str, Option<&[&str]>)]) -> Vec<(String, Option<Vec<String>>)> {
        list.iter()
            .map(|(n, c)| {
                (
                    n.to_string(),
                    c.map(|c| c.iter().map(|s| s.to_string()).collect()),
                )
            })
            .collect()
    }

    #[test]
    fn reported_tools_capability_beats_the_family_list() {
        let m = models(&[
            ("qwen3-coder:30b", Some(&["completion"])),
            ("gemma3:12b", Some(&["completion", "vision"])),
            ("granite3.3:8b", Some(&["completion", "tools"])),
        ]);
        assert_eq!(pick_tool_model(&m), Some("granite3.3:8b"));
        // Among the tool-capable ones, the family order decides.
        let m = models(&[
            ("mistral:7b", Some(&["completion", "tools"])),
            ("llama3.1:8b", Some(&["completion", "tools"])),
            (
                "library/qwen3:8b",
                Some(&["completion", "tools", "thinking"]),
            ),
        ]);
        assert_eq!(pick_tool_model(&m), Some("library/qwen3:8b"));
    }

    #[test]
    fn without_capabilities_the_family_order_decides() {
        let m = models(&[
            ("gemma3:12b", None),
            ("llama3.2:3b", None),
            ("qwen2.5-coder:7b", None),
            ("qwen3:8b", None),
        ]);
        assert_eq!(pick_tool_model(&m), Some("qwen2.5-coder:7b"));
        // `qwen3` is a family of its own, not a prefix of `qwen3-coder`.
        let m = models(&[("qwen3:8b", None), ("qwen3-coder:30b", None)]);
        assert_eq!(pick_tool_model(&m), Some("qwen3-coder:30b"));
        let m = models(&[("gemma3:12b", None), ("phi4:14b", None)]);
        assert_eq!(pick_tool_model(&m), Some("gemma3:12b"));
    }

    #[test]
    fn embedding_only_models_are_never_picked() {
        let m = models(&[
            ("nomic-embed-text:latest", Some(&["embedding"])),
            ("gemma3:12b", Some(&["completion"])),
        ]);
        assert_eq!(pick_tool_model(&m), Some("gemma3:12b"));
        let m = models(&[("nomic-embed-text:latest", Some(&["embedding"]))]);
        assert_eq!(pick_tool_model(&m), None);
    }

    #[tokio::test]
    async fn probe_prefers_the_model_api_show_says_has_tools() {
        let caps = HashMap::from([
            ("qwen3-coder:30b", vec!["completion"]),
            ("gemma3:12b", vec!["completion"]),
            ("llama3.2:3b", vec!["completion", "tools"]),
        ]);
        let (url, seen) = fake_server::start(
            &["gemma3:12b", "qwen3-coder:30b", "llama3.2:3b"],
            Show::Caps(caps),
        )
        .await;
        let got = probe_ollama(&url, Duration::from_millis(800)).await;
        assert_eq!(got, OllamaProbe::Model("llama3.2:3b".into()));
        assert_eq!(
            seen.lock()
                .unwrap()
                .iter()
                .filter(|l| l.starts_with("POST /api/show"))
                .count(),
            3
        );
    }

    #[tokio::test]
    async fn probe_falls_back_to_families_on_an_ollama_without_capabilities() {
        let (url, _) = fake_server::start(
            &["gemma3:12b", "qwen3-coder:30b"],
            Show::Caps(HashMap::new()),
        )
        .await;
        let got = probe_ollama(&url, Duration::from_millis(800)).await;
        assert_eq!(got, OllamaProbe::Model("qwen3-coder:30b".into()));
    }

    #[tokio::test]
    async fn probe_does_not_wait_past_the_budget_for_api_show() {
        let (url, _) = fake_server::start(&["gemma3:12b", "llama3.1:8b"], Show::Hang).await;
        let start = Instant::now();
        let got = probe_ollama(&url, Duration::from_millis(400)).await;
        assert!(
            start.elapsed() < Duration::from_millis(1500),
            "{:?}",
            start.elapsed()
        );
        assert_eq!(got, OllamaProbe::Model("llama3.1:8b".into()));
    }

    #[tokio::test]
    async fn probe_keeps_the_capabilities_that_arrived_when_one_lookup_hangs() {
        let caps = HashMap::from([
            ("qwen3-coder:30b", vec!["completion"]),
            ("granite3.3:8b", vec!["completion", "tools"]),
        ]);
        let (url, _) = fake_server::start(
            &["qwen3-coder:30b", "llama3.1:8b", "granite3.3:8b"],
            Show::HangOn("llama3.1:8b", caps),
        )
        .await;
        let start = Instant::now();
        let got = probe_ollama(&url, Duration::from_millis(400)).await;
        assert!(
            start.elapsed() < Duration::from_millis(1500),
            "{:?}",
            start.elapsed()
        );
        // Not the family list's `qwen3-coder` or `llama3.1`: Ollama said
        // granite has tools before the deadline.
        assert_eq!(got, OllamaProbe::Model("granite3.3:8b".into()));
    }

    #[test]
    fn a_hung_dns_lookup_does_not_hold_up_runtime_shutdown() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let start = Instant::now();
        let got = rt.block_on(async {
            let lookup = resolve_detached(|| {
                std::thread::sleep(Duration::from_secs(5));
                Ok(vec![])
            });
            tokio::time::timeout(Duration::from_millis(50), lookup).await
        });
        assert!(got.is_err(), "the lookup should still be running");
        drop(rt);
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "runtime drop waited for the lookup: {:?}",
            start.elapsed()
        );
    }

    #[tokio::test]
    async fn the_detached_resolver_resolves_localhost() {
        use reqwest::dns::Resolve;
        let addrs: Vec<_> = DetachedResolver
            .resolve("localhost".parse().unwrap())
            .await
            .unwrap()
            .collect();
        assert!(addrs.iter().any(|a| a.ip().is_loopback()), "{addrs:?}");
    }

    #[tokio::test]
    async fn probe_reports_an_ollama_with_nothing_pulled() {
        let (url, _) = fake_server::start(&[], Show::Caps(HashMap::new())).await;
        assert_eq!(
            probe_ollama(&url, Duration::from_millis(800)).await,
            OllamaProbe::NoModels
        );
    }

    #[tokio::test]
    async fn probe_fails_fast_when_nothing_listens() {
        let url = fake_server::closed_port().await;
        let start = Instant::now();
        assert_eq!(
            probe_ollama(&url, Duration::from_millis(800)).await,
            OllamaProbe::Unreachable
        );
        assert!(
            start.elapsed() < Duration::from_millis(800),
            "{:?}",
            start.elapsed()
        );
    }

    #[tokio::test]
    async fn probe_gives_up_on_a_host_that_never_answers() {
        let url = fake_server::silent().await;
        let start = Instant::now();
        assert_eq!(
            probe_ollama(&url, Duration::from_millis(300)).await,
            OllamaProbe::Unreachable
        );
        assert!(
            start.elapsed() < Duration::from_millis(1500),
            "{:?}",
            start.elapsed()
        );
    }
}
