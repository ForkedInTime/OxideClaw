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
    /// Auth handle captured when the tools were built. `/login` installs a new
    /// handle on the config, so this one can go stale — `execute` prefers
    /// `ctx.live_auth` and falls back to this.
    pub auth: crate::auth::AuthHandle,
    pub model: String,
}

impl WebSearchTool {
    /// The handle that decides this request's credential: the live one the run
    /// loop published this turn, else our build-time snapshot.
    fn auth_for<'a>(&'a self, ctx: &'a ToolContext) -> &'a crate::auth::AuthHandle {
        ctx.live_auth.as_ref().unwrap_or(&self.auth)
    }

    /// The request headers, with the credential in the wire format the
    /// credential kind requires. Mirrors `api::AnthropicClient::with_credential`.
    /// An OAuth token in `x-api-key` is a guaranteed 401, and the API rejects
    /// `x-api-key` and `Authorization` together, so exactly one goes on.
    fn headers(cred: &crate::auth::Credential) -> Vec<(&'static str, String)> {
        let mut betas = vec![WEB_SEARCH_BETA];
        let mut h: Vec<(&'static str, String)> = vec![
            ("anthropic-version", "2023-06-01".into()),
            ("content-type", "application/json".into()),
        ];
        match cred {
            crate::auth::Credential::OAuth(t) => {
                h.push(("authorization", format!("Bearer {t}")));
                betas.push(crate::auth::OAUTH_BETA);
            }
            crate::auth::Credential::ApiKey(k) => {
                h.push(("x-api-key", k.clone()));
            }
        }
        h.push(("anthropic-beta", betas.join(",")));
        h
    }
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

        // Read the credential per request: a `/login` mid-session replaces the
        // handle, and an OAuth profile rotates its token underneath us.
        let Some(cred) = self.auth_for(ctx).snapshot() else {
            return Ok(ToolOutput::error(
                "Web search needs an Anthropic credential. Run /login, or set ANTHROPIC_API_KEY.",
            ));
        };

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
            "model": self.model,
            "max_tokens": 4096,
            "messages": [{
                "role": "user",
                "content": format!("Search for: {}", input.query)
            }],
            "tools": [web_search_tool]
        });

        let client = reqwest::Client::builder().timeout(SEARCH_TIMEOUT).build()?;
        let mut request = client.post("https://api.anthropic.com/v1/messages");
        for (name, value) in Self::headers(&cred) {
            request = request.header(name, value);
        }
        let response = request.json(&request_body).send().await?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Ok(ToolOutput::error(format!(
                "Web search API error {status}: {body}"
            )));
        }

        let resp: serde_json::Value = response.json().await?;

        // Extract text content from the response
        let mut result = String::new();
        if let Some(content) = resp["content"].as_array() {
            for block in content {
                match block["type"].as_str() {
                    Some("text") => {
                        if let Some(text) = block["text"].as_str() {
                            result.push_str(text);
                        }
                    }
                    Some("web_search_result") => {
                        // Include source URLs
                        if let Some(results) = block["results"].as_array() {
                            for r in results {
                                if let (Some(title), Some(url)) =
                                    (r["title"].as_str(), r["url"].as_str())
                                {
                                    result.push_str(&format!("\n[{title}]({url})"));
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        }

        if result.trim().is_empty() {
            return Ok(ToolOutput::error("No search results returned"));
        }

        Ok(ToolOutput::success(result))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::auth::{AuthHandle, Credential};

    fn header<'a>(h: &'a [(&'static str, String)], name: &str) -> Option<&'a str> {
        h.iter().find(|(k, _)| *k == name).map(|(_, v)| v.as_str())
    }

    fn tool(auth: AuthHandle) -> WebSearchTool {
        WebSearchTool {
            auth,
            model: "m".into(),
        }
    }

    fn ctx() -> ToolContext {
        ToolContext::new(std::path::PathBuf::from("/tmp"))
    }

    #[test]
    fn static_key_goes_in_x_api_key() {
        let h = WebSearchTool::headers(&Credential::ApiKey("sk-ant-x".into()));
        assert_eq!(header(&h, "x-api-key"), Some("sk-ant-x"));
        assert_eq!(header(&h, "authorization"), None);
        assert_eq!(header(&h, "anthropic-beta"), Some("web-search-2025-03-05"));
    }

    /// An OAuth token in `x-api-key` is a guaranteed 401 — the whole
    /// credential chain was useless for WebSearch.
    #[test]
    fn oauth_token_goes_in_bearer_with_the_oauth_beta() {
        let h = WebSearchTool::headers(&Credential::OAuth("tok".into()));
        assert_eq!(header(&h, "authorization"), Some("Bearer tok"));
        assert_eq!(header(&h, "x-api-key"), None);
        let beta = header(&h, "anthropic-beta").unwrap();
        assert!(beta.contains("web-search-2025-03-05"), "{beta}");
        assert!(beta.contains(crate::auth::OAUTH_BETA), "{beta}");
    }

    /// The startup handle is dead after `/login`; the header must come from
    /// the handle the run loop publishes on the context.
    #[test]
    fn the_live_handle_beats_the_build_time_one() {
        let t = tool(AuthHandle::static_credential(Credential::ApiKey(
            "sk-ant-stale".into(),
        )));
        let mut c = ctx();

        // No live handle published: fall back to our own.
        let h = WebSearchTool::headers(&t.auth_for(&c).snapshot().unwrap());
        assert_eq!(header(&h, "x-api-key"), Some("sk-ant-stale"));

        // After /login the run loop publishes the new OAuth handle.
        c.live_auth = Some(AuthHandle::static_credential(Credential::OAuth(
            "fresh-token".into(),
        )));
        let live = t.auth_for(&c);
        assert!(live.is_oauth());
        let h = WebSearchTool::headers(&live.snapshot().unwrap());
        assert_eq!(header(&h, "authorization"), Some("Bearer fresh-token"));
        assert_eq!(header(&h, "x-api-key"), None);
    }

    /// With no credential anywhere the tool reports, it does not send a
    /// request with an empty key.
    #[tokio::test]
    async fn no_credential_is_a_tool_error_not_a_bare_request() {
        let t = tool(AuthHandle::none());
        let out = t
            .execute(json!({"query": "anything"}), &ctx())
            .await
            .unwrap();
        assert!(out.is_error);
        let text = format!("{:?}", out.content);
        assert!(text.contains("/login"), "{text}");
    }
}
