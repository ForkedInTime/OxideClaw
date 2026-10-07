/// WebSearchTool — port of tools/WebSearchTool/WebSearchTool.ts
/// Uses the Anthropic web search beta API (web-search-2025-03-05).
use super::{Tool, ToolContext, ToolOutput, async_trait};
use anyhow::Result;
use serde::Deserialize;
use serde_json::json;

const WEB_SEARCH_BETA: &str = "web-search-2025-03-05";
/// The API answers a search with a full model turn; allow for that, but
/// never hang the agent on a silent connection.
const SEARCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(90);

pub struct WebSearchTool {
    pub api_key: String,
    pub model: String,
    /// `api_key` is an OAuth access token: send `Authorization: Bearer` +
    /// the oauth beta instead of `x-api-key` (the API rejects both at once).
    pub auth_is_oauth: bool,
}

impl WebSearchTool {
    /// The credential to send now: an `ant` profile token is swapped for
    /// the newest one, since the copy taken at startup expires.
    fn current_secret(&self) -> String {
        crate::auth::refreshable(self.auth_is_oauth).live(&self.api_key)
    }

    /// The request headers, with `secret` in the wire format the credential
    /// kind requires. Mirrors `api::ClaudeClient::auth_header`.
    fn headers(&self, secret: &str) -> Vec<(&'static str, String)> {
        let mut betas = vec![WEB_SEARCH_BETA];
        let mut h: Vec<(&'static str, String)> = vec![
            ("anthropic-version", "2023-06-01".into()),
            ("content-type", "application/json".into()),
        ];
        if self.auth_is_oauth {
            h.push(("authorization", format!("Bearer {secret}")));
            betas.push(crate::auth::OAUTH_BETA);
        } else {
            h.push(("x-api-key", secret.to_string()));
        }
        h.push(("anthropic-beta", betas.join(",")));
        h
    }

    /// The Claude model to run the search on. The search always goes to
    /// api.anthropic.com, so an Ollama or OpenAI-compat id (the startup model,
    /// or one picked with /model) would be a 400 on every call; the live
    /// model wins over the startup snapshot when it is a Claude one.
    fn search_model(&self, live: Option<&str>) -> String {
        let foreign =
            |m: &str| crate::api::is_ollama_model(m) || crate::api::is_openai_compat_model(m);
        let model = [live, Some(self.model.as_str())]
            .into_iter()
            .flatten()
            .find(|m| !m.is_empty() && !foreign(m))
            .unwrap_or(crate::api::default_model());
        crate::commands::resolve_model_alias(model)
    }
}

/// The model's answer followed by the deduplicated sources it searched.
/// Sources arrive inside `web_search_tool_result` blocks (and as citations
/// on text blocks), never as top-level `web_search_result` blocks.
fn render_response(resp: &serde_json::Value) -> Result<String, String> {
    let mut answer = String::new();
    let mut sources: Vec<(String, String)> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut add = |title: Option<&str>, url: Option<&str>| {
        if let Some(url) = url
            && seen.insert(url.to_string())
        {
            sources.push((title.unwrap_or(url).to_string(), url.to_string()));
        }
    };
    let mut error = None;
    for block in resp["content"].as_array().into_iter().flatten() {
        match block["type"].as_str() {
            Some("text") => {
                if let Some(text) = block["text"].as_str() {
                    answer.push_str(text);
                }
                for c in block["citations"].as_array().into_iter().flatten() {
                    add(c["title"].as_str(), c["url"].as_str());
                }
            }
            Some("web_search_tool_result") => match &block["content"] {
                serde_json::Value::Array(results) => {
                    for r in results {
                        if r["type"] == "web_search_result" {
                            add(r["title"].as_str(), r["url"].as_str());
                        }
                    }
                }
                other => {
                    if let Some(code) = other["error_code"].as_str() {
                        error = Some(code.to_string());
                    }
                }
            },
            _ => {}
        }
    }
    if answer.trim().is_empty() && sources.is_empty() {
        return Err(match error {
            Some(code) => format!("Web search failed: {code}"),
            None => "No search results returned".into(),
        });
    }
    if !sources.is_empty() {
        answer.push_str("\n\nSources:");
        for (title, url) in &sources {
            answer.push_str(&format!("\n- [{title}]({url})"));
        }
    }
    Ok(answer)
}

#[derive(Deserialize)]
struct WebSearchInput {
    query: String,
    #[serde(default)]
    allowed_domains: Option<Vec<String>>,
    #[serde(default)]
    blocked_domains: Option<Vec<String>>,
}

#[async_trait]
impl Tool for WebSearchTool {
    fn name(&self) -> &str {
        "WebSearch"
    }

    fn description(&self) -> &str {
        "Search the web for current information. Returns summarized results \
        with sources. Use for questions about recent events, documentation, \
        or anything requiring up-to-date information."
    }

    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "The search query"
                },
                "allowed_domains": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Only include results from these domains"
                },
                "blocked_domains": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Exclude results from these domains"
                }
            },
            "required": ["query"]
        })
    }

    async fn execute(&self, input: serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let input: WebSearchInput = serde_json::from_value(input)?;

        // Build the web_search tool definition for the Anthropic beta API
        let mut web_search_tool = json!({
            "type": "web_search_20250305",
            "name": "web_search"
        });

        if let Some(allowed) = &input.allowed_domains {
            web_search_tool["allowed_domains"] = json!(allowed);
        }
        if let Some(blocked) = &input.blocked_domains {
            web_search_tool["blocked_domains"] = json!(blocked);
        }

        let request_body = json!({
            "model": self.search_model(ctx.live_model.as_deref()),
            "max_tokens": 4096,
            "messages": [{
                "role": "user",
                "content": format!("Search for: {}", input.query)
            }],
            "tools": [web_search_tool]
        });

        let client = reqwest::Client::builder().timeout(SEARCH_TIMEOUT).build()?;
        let send = |secret: String| {
            let mut request = client.post("https://api.anthropic.com/v1/messages");
            for (name, value) in self.headers(&secret) {
                request = request.header(name, value);
            }
            request.json(&request_body).send()
        };
        let secret = self.current_secret();
        if secret.is_empty() {
            return Ok(ToolOutput::error(
                "WebSearch needs an Anthropic API key (ANTHROPIC_API_KEY or /login); \
                 it runs on Anthropic's server-side search whatever the chat model.",
            ));
        }
        let mut response = send(secret.clone()).await?;
        // An expired profile token or helper key: refresh it once, as the
        // main client does.
        let store = crate::auth::refreshable(self.auth_is_oauth);
        if response.status() == reqwest::StatusCode::UNAUTHORIZED
            && let Ok(Some(fresh)) =
                tokio::task::spawn_blocking(move || store.refresh(&secret)).await
        {
            response = send(fresh).await?;
        }

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Ok(ToolOutput::error(format!(
                "Web search API error {status}: {body}"
            )));
        }

        let resp: serde_json::Value = response.json().await?;

        Ok(match render_response(&resp) {
            Ok(text) => ToolOutput::success(text),
            Err(e) => ToolOutput::error(e),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header<'a>(h: &'a [(&'static str, String)], name: &str) -> Option<&'a str> {
        h.iter().find(|(k, _)| *k == name).map(|(_, v)| v.as_str())
    }

    #[test]
    fn static_key_goes_in_x_api_key() {
        let t = WebSearchTool {
            api_key: "sk-ant-x".into(),
            model: "m".into(),
            auth_is_oauth: false,
        };
        let h = t.headers(&t.current_secret());
        assert_eq!(header(&h, "x-api-key"), Some("sk-ant-x"));
        assert_eq!(header(&h, "authorization"), None);
        assert_eq!(header(&h, "anthropic-beta"), Some("web-search-2025-03-05"));
    }

    /// An OAuth token in `x-api-key` is a guaranteed 401 — the whole
    /// credential chain was useless for WebSearch.
    #[test]
    fn oauth_token_goes_in_bearer_with_the_oauth_beta() {
        let t = WebSearchTool {
            api_key: "tok".into(),
            model: "m".into(),
            auth_is_oauth: true,
        };
        let h = t.headers(&t.current_secret());
        assert_eq!(header(&h, "authorization"), Some("Bearer tok"));
        assert_eq!(header(&h, "x-api-key"), None);
        let beta = header(&h, "anthropic-beta").unwrap();
        assert!(beta.contains("web-search-2025-03-05"), "{beta}");
        assert!(beta.contains(crate::auth::OAUTH_BETA), "{beta}");
    }

    fn tool(model: &str) -> WebSearchTool {
        WebSearchTool {
            api_key: "k".into(),
            model: model.into(),
            auth_is_oauth: false,
        }
    }

    /// The startup model id went to api.anthropic.com even when it was an
    /// Ollama/OpenAI-compat one, and a later /model switch never reached it.
    #[test]
    fn the_search_runs_on_a_claude_model() {
        let t = tool("ollama:qwen3");
        assert_eq!(t.search_model(None), crate::api::default_model());
        assert_eq!(
            t.search_model(Some("groq:llama-3.3-70b")),
            crate::api::default_model()
        );
        assert_eq!(t.search_model(Some("claude-haiku-4-5")), "claude-haiku-4-5");

        let t = tool("claude-sonnet-4-6");
        assert_eq!(t.search_model(Some("claude-opus-4-6")), "claude-opus-4-6");
        assert_eq!(t.search_model(Some("ollama:qwen3")), "claude-sonnet-4-6");
        assert_eq!(t.search_model(Some("opus")), "claude-opus-5");
    }

    /// Sources live inside `web_search_tool_result` blocks and text
    /// citations; the parser looked for a top-level block that never exists.
    #[test]
    fn sources_come_from_tool_result_blocks_and_citations() {
        let resp = serde_json::json!({"content": [
            {"type": "server_tool_use", "id": "s1", "name": "web_search", "input": {"query": "q"}},
            {"type": "web_search_tool_result", "tool_use_id": "s1", "content": [
                {"type": "web_search_result", "url": "https://a.example/", "title": "A",
                 "encrypted_content": "x"},
                {"type": "web_search_result", "url": "https://b.example/", "title": "B",
                 "encrypted_content": "y"}
            ]},
            {"type": "text", "text": "Answer.", "citations": [
                {"type": "web_search_result_location", "url": "https://a.example/",
                 "title": "A", "cited_text": "..."},
                {"type": "web_search_result_location", "url": "https://c.example/",
                 "title": "C", "cited_text": "..."}
            ]}
        ]});
        let out = render_response(&resp).unwrap();
        assert!(out.starts_with("Answer."), "{out}");
        for src in [
            "[A](https://a.example/)",
            "[B](https://b.example/)",
            "[C](https://c.example/)",
        ] {
            assert_eq!(out.matches(src).count(), 1, "{out}");
        }

        let err = serde_json::json!({"content": [
            {"type": "web_search_tool_result", "tool_use_id": "s1",
             "content": {"type": "web_search_tool_result_error", "error_code": "max_uses_exceeded"}}
        ]});
        assert!(
            render_response(&err)
                .unwrap_err()
                .contains("max_uses_exceeded")
        );
    }
}
