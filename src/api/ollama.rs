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

        let oai_messages = translate_messages(&system, &request.messages, false);
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
                    translate_messages(&patched_system, &request.messages, false);
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
