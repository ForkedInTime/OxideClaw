use crate::api::types::*;
/// Context compaction — port of services/compact/
///
/// Two strategies, matching the original TypeScript design:
///
///  1. snipCompact (client-side, instant)
///     When input_tokens approaches the context limit, strip the content out
///     of old ToolResult blocks in the message history.  The tool call
///     structure is preserved so Claude still knows what tools were used,
///     but the potentially-large result payloads are replaced with a short
///     placeholder.  No API call required.
///
///  2. summarizeCompact (API-based, async)
///     Sends the full conversation to Claude with a summarisation prompt
///     and returns a replacement history containing just one user message
///     with the generated summary.  This is the "full compact" operation
///     available via `/compact` or triggered automatically when auto-compact
///     is enabled and the snip threshold has already been crossed.
///
/// Token thresholds scale with the model's context window (80% warn, 85% snip,
/// 90% summarise), which on a 200k window are apiMicrocompact.ts's
/// 160k / 170k / 180k DEFAULT_* values.
use crate::api::{ApiBackend, MessagesRequest};
use crate::config::Config;
use anyhow::Result;

/// Largest tool result, in characters, that goes to the model. Read and Grep
/// cap lines, not characters, so one minified bundle or a broad content grep
/// returns megabytes and every request that carries it is a 400.
pub const TOOL_RESULT_MAX_CHARS: usize = 100_000;

/// Cut `text` to `TOOL_RESULT_MAX_CHARS` characters, saying so in the text.
pub fn budget_tool_result(text: &mut String) {
    if let Some((cut, _)) = text.char_indices().nth(TOOL_RESULT_MAX_CHARS) {
        text.truncate(cut);
        text.push_str(&format!(
            "\n\n[... output truncated to {TOOL_RESULT_MAX_CHARS} characters; \
             use offset/limit or head_limit to narrow]"
        ));
    }
}

/// `(warn, snip, summarise)` input-token thresholds for a context window.
pub fn thresholds(window: u64) -> (u64, u64, u64) {
    (window / 100 * 80, window / 100 * 85, window / 100 * 90)
}

/// The window to compact against: the smallest among the configured model and
/// every model a router may send the next turn to. A history that is fine on a
/// 1M model is a prompt-too-long 400 once a simple prompt routes to Haiku.
pub fn compaction_window(config: &Config, router: Option<&crate::router::RouterConfig>) -> u64 {
    let mut models: Vec<&str> = vec![&config.model];
    if let Some(r) = router.filter(|r| r.enabled) {
        models.extend([
            r.low_model.as_str(),
            r.medium_model.as_str(),
            r.high_model.as_str(),
            r.super_high_model.as_str(),
        ]);
    }
    let p = &config.phase_router;
    if p.enabled {
        models.extend([
            p.research_model.as_str(),
            p.plan_model.as_str(),
            p.edit_model.as_str(),
            p.review_model.as_str(),
            p.default_model.as_str(),
        ]);
    }
    models
        .into_iter()
        .filter(|m| !m.is_empty())
        .map(crate::api::context_window_for_model)
        .min()
        .unwrap_or(200_000)
}

/// How many recent messages snipCompact always keeps untouched.
const SNIP_KEEP_RECENT: usize = 20;

/// What kind of compaction (if any) is needed right now.
#[derive(Debug, Clone, PartialEq)]
pub enum CompactNeeded {
    None,
    Warn,
    Snip,
    Summarise,
}

/// Decide what action to take based on the latest input_tokens count and the
/// context window it is measured against.
pub fn compact_needed(input_tokens: u64, window: u64) -> CompactNeeded {
    let (warn, snip, summarise) = thresholds(window);
    if input_tokens >= summarise {
        CompactNeeded::Summarise
    } else if input_tokens >= snip {
        CompactNeeded::Snip
    } else if input_tokens >= warn {
        CompactNeeded::Warn
    } else {
        CompactNeeded::None
    }
}

// ── snipCompact ─────────────────────────────────────────────────────────────

/// Client-side compaction: replace ToolResult content in old messages with a
/// short placeholder, keeping the most recent `SNIP_KEEP_RECENT` messages
/// entirely untouched.
///
/// This mirrors the snipCompactIfNeeded strategy: tool call *structure* is
/// preserved (Claude can see what tools were invoked) but the large payloads
/// that fill the context window are cleared.
pub fn snip_compact(messages: &mut [Message]) {
    let len = messages.len();
    if len <= SNIP_KEEP_RECENT {
        return;
    }
    let snip_until = len - SNIP_KEEP_RECENT;

    for msg in messages[..snip_until].iter_mut() {
        for block in msg.content.iter_mut() {
            if let ContentBlock::ToolResult { content, .. } = block {
                *content = vec![ToolResultContent::text(
                    "[content removed by snipCompact to reduce context size]",
                )];
            }
        }
    }
}

// ── summarizeCompact ─────────────────────────────────────────────────────────

const SUMMARISE_SYSTEM: &str = "\
You are a helpful AI assistant tasked with summarizing conversations.";

/// The compact prompt mirrors the real source's BASE_COMPACT_PROMPT structure:
/// a detailed 9-section format capturing all context needed to continue work.
const SUMMARISE_PROMPT_PREFIX: &str = "\
Your task is to create a detailed summary of the conversation so far, paying \
close attention to the user's explicit requests and your previous actions. \
This summary should be thorough in capturing technical details, code patterns, \
and architectural decisions that would be essential for continuing development \
work without losing context.

Your summary should include the following sections:

1. Primary Request and Intent: Capture all of the user's explicit requests and intents in detail
2. Key Technical Concepts: List all important technical concepts, technologies, and frameworks discussed.
3. Files and Code Sections: Enumerate specific files and code sections examined, modified, or created. \
Pay special attention to the most recent messages and include full code snippets where applicable \
and include a summary of why this file read or edit is important.
4. Errors and fixes: List all errors that you ran into, and how you fixed them. Pay special \
attention to specific user feedback that you received, especially if the user told you to do \
something differently.
5. Problem Solving: Document problems solved and any ongoing troubleshooting efforts.
6. All user messages: List ALL user messages that are not tool results. These are critical for \
understanding the users' feedback and changing intent.
7. Pending Tasks: Outline any pending tasks that you have explicitly been asked to work on.
8. Current Work: Describe in detail precisely what was being worked on immediately before this \
summary request, paying special attention to the most recent messages from both user and assistant. \
Include file names and code snippets where applicable.
9. Optional Next Step: List the next step that you will take that is related to the most recent \
work you were doing. IMPORTANT: ensure that this step is DIRECTLY in line with the user's most \
recent explicit requests, and the task you were working on immediately before this summary request. \
If your last task was concluded, then only list next steps if they are explicitly in line with the \
users request.

Please provide your summary based on the conversation so far, following this structure and \
ensuring precision and thoroughness in your response.

Here is the conversation to summarize:

";

/// Build a plain-text rendering of the message history suitable for sending
/// to the summarisation model.
fn render_history(messages: &[Message]) -> String {
    let mut out = String::new();
    for msg in messages {
        let role = match msg.role {
            Role::User => "User",
            Role::Assistant => "Assistant",
        };
        out.push_str(&format!("--- {role} ---\n"));
        for block in &msg.content {
            match block {
                ContentBlock::Text { text } => {
                    out.push_str(text);
                    out.push('\n');
                }
                ContentBlock::ToolUse { name, input, .. } => {
                    let args = serde_json::to_string(input).unwrap_or_default();
                    out.push_str(&format!("[Tool call: {name}({args})]\n"));
                }
                ContentBlock::ToolResult {
                    content, is_error, ..
                } => {
                    let label = if is_error == &Some(true) {
                        "error"
                    } else {
                        "result"
                    };
                    let text = content
                        .iter()
                        .map(|c| {
                            let ToolResultContent::Text { text } = c;
                            text.as_str()
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    out.push_str(&format!("[Tool {label}: {text}]\n"));
                }
                ContentBlock::Thinking { thinking, .. } => {
                    // chars, not bytes: a byte cut can split a code point and panic.
                    let head: String = thinking.chars().take(200).collect();
                    out.push_str(&format!("[Thinking: {head}]\n"));
                }
                ContentBlock::Image { .. } => {
                    out.push_str("[Image attachment]\n");
                }
            }
        }
        out.push('\n');
    }
    out
}

/// Output budget for the summary request. A detailed 9-section summary of up
/// to ~180k tokens of history, plus adaptive thinking, does not fit in the
/// usual turn budget. Streaming makes a large cap safe; Claude 3.x (other
/// than 3.7) cannot emit that much, so it keeps the configured value.
fn summary_max_tokens(config: &Config) -> u32 {
    let configured = config.max_tokens_for(&config.model);
    let model = crate::commands::resolve_model_alias(&config.model);
    if model.starts_with("claude-3-") && !model.starts_with("claude-3-7") {
        return configured;
    }
    configured.max(32_000)
}

/// API-based compaction: asks Claude to summarise the full conversation,
/// then returns a replacement history with a single user message containing
/// the summary, prefixed so Claude knows the context is compacted.
///
/// The caller should replace its `messages` vec with the returned vec.
pub async fn summarize_compact(
    client: &ApiBackend,
    messages: &[Message],
    config: &Config,
) -> Result<Vec<Message>> {
    let history_text = render_history(messages);
    let prompt = format!("{SUMMARISE_PROMPT_PREFIX}{history_text}");

    let request = MessagesRequest {
        model: config.model.clone(),
        max_tokens: summary_max_tokens(config),
        system: crate::api::types::SystemContent::Plain(SUMMARISE_SYSTEM.to_string()),
        messages: vec![Message {
            role: Role::User,
            content: vec![ContentBlock::Text { text: prompt }],
        }],
        tools: vec![],
        stream: None,
        // Thinking stays at the API default (adaptive on current models, and
        // Opus/Sonnet 5.5 and Fable reject `disabled`); medium effort keeps it
        // from eating the output budget the summary itself needs.
        thinking: None,
        output_config: crate::api::thinking::supports_effort(&config.model).then(|| {
            crate::api::types::OutputConfig {
                effort: "medium".into(),
            }
        }),
        betas: vec![],
        session_id: None,
    };

    let mut summary_text = String::new();
    let resp = client
        .messages_stream(request, |chunk| {
            summary_text.push_str(chunk);
        })
        .await?;

    // Every caller replaces (and the TUI persists) the history on Ok, so a
    // cut-off summary would silently drop the newest work (sections 8 and 9
    // come last). Fail instead and let the caller keep or snip the original.
    if matches!(
        resp.stop_reason,
        Some(StopReason::MaxTokens | StopReason::Refusal | StopReason::ModelContextWindowExceeded)
    ) || summary_text.trim().is_empty()
    {
        anyhow::bail!(
            "compaction summary incomplete (stop reason: {:?}); history left unchanged",
            resp.stop_reason
        );
    }

    // Return a replacement history: one user message with the summary,
    // wrapped so the model understands the context was compacted.
    let replacement = vec![Message {
        role: Role::User,
        content: vec![ContentBlock::Text {
            text: format!(
                "[This conversation was automatically compacted to save context space.]\n\
                 [Summary of previous conversation:]\n\n{summary_text}"
            ),
        }],
    }];

    Ok(replacement)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn sse(stop_reason: &str, text: &str) -> String {
        let events = [
            r#"{"type":"message_start","message":{"id":"m","type":"message","role":"assistant","content":[],"model":"x","stop_reason":null,"usage":{"input_tokens":1,"output_tokens":0}}}"#.to_string(),
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#.to_string(),
            serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":text}}).to_string(),
            r#"{"type":"content_block_stop","index":0}"#.to_string(),
            serde_json::json!({"type":"message_delta","delta":{"stop_reason":stop_reason},"usage":{"output_tokens":5}}).to_string(),
            r#"{"type":"message_stop"}"#.to_string(),
        ];
        let body: String = events.iter().map(|e| format!("data: {e}\n\n")).collect();
        format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    /// One-shot Anthropic stand-in that records the request body it was sent.
    async fn serve_once(response: String) -> (String, Arc<Mutex<String>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = Arc::new(Mutex::new(String::new()));
        let sink = seen.clone();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut raw = Vec::new();
            let mut buf = [0u8; 8192];
            loop {
                let n = sock.read(&mut buf).await.unwrap();
                raw.extend_from_slice(&buf[..n]);
                let text = String::from_utf8_lossy(&raw).to_string();
                if let Some(split) = text.find("\r\n\r\n") {
                    let len = text[..split]
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse::<usize>().unwrap())
                        })
                        .unwrap_or(0);
                    if raw.len() >= split + 4 + len || n == 0 {
                        *sink.lock().unwrap() = text[split + 4..].to_string();
                        break;
                    }
                }
                if n == 0 {
                    break;
                }
            }
            let _ = sock.write_all(response.as_bytes()).await;
            let _ = sock.shutdown().await;
        });
        (format!("http://{addr}"), seen)
    }

    fn backend(base: &str) -> ApiBackend {
        let mut c = crate::api::ClaudeClient::new("sk-ant-test").unwrap();
        c.set_base_url_for_test(base);
        ApiBackend::Anthropic(c)
    }

    fn history() -> Vec<Message> {
        vec![Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: "refactor auth".into(),
            }],
        }]
    }

    fn config() -> Config {
        Config {
            model: "claude-sonnet-5".into(),
            ..Config::default()
        }
    }

    #[tokio::test]
    async fn truncated_summary_is_an_error_not_a_replacement() {
        let (url, _) = serve_once(sse("max_tokens", "1. Primary Request: refac")).await;
        let res = summarize_compact(&backend(&url), &history(), &config()).await;
        assert!(
            res.is_err(),
            "a max_tokens summary must not replace history"
        );
    }

    #[tokio::test]
    async fn empty_summary_is_an_error() {
        let (url, _) = serve_once(sse("end_turn", "  ")).await;
        let res = summarize_compact(&backend(&url), &history(), &config()).await;
        assert!(res.is_err(), "an empty summary must not replace history");
    }

    #[tokio::test]
    async fn complete_summary_requests_a_real_budget() {
        let (url, seen) = serve_once(sse("end_turn", "1. Primary Request: auth")).await;
        let out = summarize_compact(&backend(&url), &history(), &config())
            .await
            .expect("complete summary");
        let ContentBlock::Text { text } = &out[0].content[0] else {
            panic!("summary should be text");
        };
        assert!(text.contains("1. Primary Request: auth"));

        let body: serde_json::Value = serde_json::from_str(&seen.lock().unwrap()).unwrap();
        assert!(body["max_tokens"].as_u64().unwrap() >= 32_000, "{body}");
        assert_eq!(body["output_config"]["effort"], "medium");
    }

    #[test]
    fn thresholds_scale_with_the_window() {
        assert_eq!(thresholds(200_000), (160_000, 170_000, 180_000));
        // 180k on a 1M model is 18% full: nothing to do yet.
        assert_eq!(compact_needed(180_000, 1_000_000), CompactNeeded::None);
        assert_eq!(compact_needed(180_000, 200_000), CompactNeeded::Summarise);
        assert_eq!(compact_needed(900_000, 1_000_000), CompactNeeded::Summarise);
    }

    #[test]
    fn routed_sessions_compact_for_the_smallest_candidate() {
        let cfg = config();
        assert_eq!(compaction_window(&cfg, None), 1_000_000);

        let mut router = crate::router::RouterConfig::new(&cfg.model);
        assert_eq!(
            compaction_window(&cfg, Some(&router)),
            1_000_000,
            "router off"
        );
        router.enabled = true;
        // Default low tier is Haiku 4.5 (200k).
        assert_eq!(compaction_window(&cfg, Some(&router)), 200_000);

        let mut phased = config();
        phased.phase_router.enabled = true;
        assert_eq!(compaction_window(&phased, None), 200_000);
    }

    #[test]
    fn legacy_claude_3_keeps_its_configured_cap() {
        let mut cfg = config();
        cfg.max_tokens = 8_192;
        cfg.model = "claude-3-5-sonnet-20241022".into();
        assert_eq!(summary_max_tokens(&cfg), 8_192);
        cfg.model = "claude-haiku-4-5".into();
        assert_eq!(summary_max_tokens(&cfg), 32_000);
    }
}

#[cfg(test)]
mod tool_result_budget_tests {
    use super::*;

    #[test]
    fn oversized_results_are_cut_on_a_char_boundary() {
        let mut small = "é".repeat(TOOL_RESULT_MAX_CHARS);
        budget_tool_result(&mut small);
        assert_eq!(
            small.chars().count(),
            TOOL_RESULT_MAX_CHARS,
            "at the cap: kept"
        );

        let mut big = "é".repeat(TOOL_RESULT_MAX_CHARS + 1);
        budget_tool_result(&mut big);
        assert!(big.starts_with(&"é".repeat(TOOL_RESULT_MAX_CHARS)));
        assert!(
            big.contains("output truncated"),
            "{}",
            &big[big.len() - 120..]
        );
        assert!(big.chars().count() < TOOL_RESULT_MAX_CHARS + 200);
    }
}
