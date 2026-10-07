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
///
/// The phase router is passed in rather than read from `config`: only the TUI
/// phase-routes, and headless/SDK sessions always send `config.model`.
pub fn compaction_window(
    config: &Config,
    router: Option<&crate::router::RouterConfig>,
    phase: Option<&crate::router::PhaseRouterConfig>,
) -> u64 {
    let mut models: Vec<&str> = vec![&config.model];
    if let Some(r) = router.filter(|r| r.enabled) {
        models.extend([
            r.low_model.as_str(),
            r.medium_model.as_str(),
            r.high_model.as_str(),
            r.super_high_model.as_str(),
        ]);
    }
    if let Some(p) = phase.filter(|p| p.enabled) {
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

const SNIP_PLACEHOLDER: &str = "[content removed by snipCompact to reduce context size]";

/// Client-side compaction: replace ToolResult content in old messages with a
/// short placeholder, keeping the most recent `SNIP_KEEP_RECENT` messages
/// entirely untouched. Returns whether anything changed.
///
/// This mirrors the snipCompactIfNeeded strategy: tool call *structure* is
/// preserved (Claude can see what tools were invoked) but the large payloads
/// that fill the context window are cleared.
///
/// On models that bind thinking to the conversation, editing a message
/// invalidates every later thinking block, so those are dropped from the
/// first edited message on. Earlier blocks still chain correctly.
pub fn snip_compact(messages: &mut [Message], model: &str) -> bool {
    let len = messages.len();
    if len <= SNIP_KEEP_RECENT {
        return false;
    }
    let binds = crate::api::thinking::binds_thinking_to_conversation(model);
    // A tool round still waiting on its results needs its thinking replayed,
    // and after the edit that thinking is a 400: snip once the round ends.
    if binds
        && messages.last().is_some_and(|m| {
            m.role == Role::Assistant
                && m.content
                    .iter()
                    .any(|b| matches!(b, ContentBlock::ToolUse { .. }))
        })
    {
        return false;
    }
    let snip_until = len - SNIP_KEEP_RECENT;

    let mut first_changed = None;
    for (i, msg) in messages[..snip_until].iter_mut().enumerate() {
        for block in msg.content.iter_mut() {
            if let ContentBlock::ToolResult { content, .. } = block
                && !matches!(content.as_slice(), [ToolResultContent::Text { text }] if text == SNIP_PLACEHOLDER)
            {
                *content = vec![ToolResultContent::text(SNIP_PLACEHOLDER)];
                first_changed.get_or_insert(i);
            }
        }
    }
    let Some(first) = first_changed else {
        return false;
    };
    if binds {
        drop_thinking(&mut messages[first..]);
    }
    true
}

/// Remove every thinking block. A turn left empty keeps a placeholder text
/// block: dropping it would put two user turns back to back, a 400.
pub fn drop_thinking(messages: &mut [Message]) {
    for msg in messages {
        let before = msg.content.len();
        msg.content
            .retain(|b| !matches!(b, ContentBlock::Thinking { .. }));
        if msg.content.is_empty() && before > 0 {
            msg.content.push(ContentBlock::Text {
                text: "(no response)".into(),
            });
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

/// Largest tool result, in characters, rendered into the summary prompt. The
/// history being summarised already overflows the window; whole results
/// (Bash keeps up to 1 MB) would make the summary request overflow too.
const SUMMARY_TOOL_RESULT_MAX_CHARS: usize = 8_000;

/// `text` cut to `max` characters, keeping the head and tail (the command
/// and its final error are usually at the ends).
fn elide_middle(text: &str, max: usize) -> std::borrow::Cow<'_, str> {
    let total = text.chars().count();
    if total <= max {
        return text.into();
    }
    let half = max / 2;
    let head_end = text.char_indices().nth(half).map_or(text.len(), |(i, _)| i);
    let tail_start = text
        .char_indices()
        .nth(total - half)
        .map_or(text.len(), |(i, _)| i);
    format!(
        "{}\n[... {} chars elided ...]\n{}",
        &text[..head_end],
        total - 2 * half,
        &text[tail_start..]
    )
    .into()
}

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
                    let text = elide_middle(&text, SUMMARY_TOOL_RESULT_MAX_CHARS);
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
///
/// Capped at what the window has left after `prompt`: models before 4.5
/// reject a request whose input plus `max_tokens` exceeds the window, and
/// auto-summarise starts at 90% full.
fn summary_max_tokens(config: &Config, prompt: &str) -> u32 {
    let configured = config.max_tokens_for(&config.model);
    let model = crate::commands::resolve_model_alias(&config.model);
    if model.starts_with("claude-3-") && !model.starts_with("claude-3-7") {
        return configured;
    }
    // The 32k floor is for Claude (Bedrock/Vertex ids wrap `claude-`).
    // OpenAI-compatible providers cap output far lower (deepseek-chat 8k,
    // gpt-4o 16k) and 400 on a larger max_tokens.
    if !model.to_lowercase().contains("claude-") {
        return configured;
    }
    let window = crate::api::context_window_for_model(&config.model);
    // ~3.5 chars/token is conservative for code-heavy text; the constant
    // covers the system prompt and message framing.
    let est_input = (prompt.len() as u64 * 2 / 7) + 2_000;
    let room = window.saturating_sub(est_input).min(u32::MAX as u64) as u32;
    configured.max(32_000).min(room).max(configured.min(4_096))
}

/// API-based compaction: asks Claude to summarise the full conversation,
/// then returns a replacement history with a single user message containing
/// the summary, prefixed so Claude knows the context is compacted.
///
/// The caller should replace its `messages` vec with the returned vec.
/// `on_usage` gets the summary call's usage whenever the API answered, even
/// if the summary is then rejected: that call was billed either way, and it
/// carries the whole history as input.
pub async fn summarize_compact(
    client: &ApiBackend,
    messages: &[Message],
    config: &Config,
    on_usage: impl FnOnce(&Usage),
) -> Result<Vec<Message>> {
    let history_text = render_history(messages);
    let prompt = format!("{SUMMARISE_PROMPT_PREFIX}{history_text}");

    let request = MessagesRequest {
        model: config.model.clone(),
        max_tokens: summary_max_tokens(config, &prompt),
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
        explicit_max_tokens: config.explicit_max_tokens_for(&config.model).is_some(),
    };

    let mut summary_text = String::new();
    let resp = client
        .messages_stream(request, |chunk| {
            summary_text.push_str(chunk);
        })
        .await?;
    on_usage(&resp.usage);

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
        let res = summarize_compact(&backend(&url), &history(), &config(), |_| {}).await;
        assert!(
            res.is_err(),
            "a max_tokens summary must not replace history"
        );
    }

    /// A rejected summary was still paid for.
    #[tokio::test]
    async fn truncated_summary_is_still_billed() {
        let (url, _) = serve_once(sse("max_tokens", "1. Primary")).await;
        let mut billed = None;
        let res = summarize_compact(&backend(&url), &history(), &config(), |u| {
            billed = Some(u.output_tokens)
        })
        .await;
        assert!(res.is_err());
        assert_eq!(billed, Some(5));
    }

    #[tokio::test]
    async fn empty_summary_is_an_error() {
        let (url, _) = serve_once(sse("end_turn", "  ")).await;
        let res = summarize_compact(&backend(&url), &history(), &config(), |_| {}).await;
        assert!(res.is_err(), "an empty summary must not replace history");
    }

    #[tokio::test]
    async fn complete_summary_requests_a_real_budget() {
        let (url, seen) = serve_once(sse("end_turn", "1. Primary Request: auth")).await;
        let mut billed = None;
        let out = summarize_compact(&backend(&url), &history(), &config(), |u| {
            billed = Some(u.clone())
        })
        .await
        .expect("complete summary");
        let billed = billed.expect("the summary call is billed");
        assert_eq!((billed.input_tokens, billed.output_tokens), (1, 5));
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
        assert_eq!(compaction_window(&cfg, None, None), 1_000_000);

        let mut router = crate::router::RouterConfig::new(&cfg.model);
        assert_eq!(
            compaction_window(&cfg, Some(&router), None),
            1_000_000,
            "router off"
        );
        router.enabled = true;
        // Default low tier is Haiku 4.5 (200k).
        assert_eq!(compaction_window(&cfg, Some(&router), None), 200_000);

        let mut phased = config();
        phased.phase_router.enabled = true;
        assert_eq!(
            compaction_window(&phased, None, Some(&phased.phase_router)),
            200_000
        );
        // Headless and SDK sessions never phase-route.
        assert_eq!(compaction_window(&phased, None, None), 1_000_000);
    }

    #[test]
    fn legacy_claude_3_keeps_its_configured_cap() {
        let mut cfg = config();
        cfg.max_tokens = Some(8_192);
        cfg.model = "claude-3-5-sonnet-20241022".into();
        assert_eq!(summary_max_tokens(&cfg, ""), 8_192);
        cfg.model = "claude-haiku-4-5".into();
        assert_eq!(summary_max_tokens(&cfg, ""), 32_000);
    }

    /// The Claude 32k floor 400'd OpenAI-compatible providers whose output
    /// limit is lower (deepseek-chat 8k, gpt-4o 16k).
    #[test]
    fn non_claude_summary_keeps_the_configured_cap() {
        let mut cfg = config();
        cfg.max_tokens = None;
        cfg.max_tokens_by_model.clear();
        for model in ["deepseek:deepseek-chat", "oai:gpt-4o"] {
            cfg.model = model.into();
            assert_eq!(summary_max_tokens(&cfg, ""), 8_192, "{model}");
        }
        // Bedrock ids keep the floor.
        cfg.model = "us.anthropic.claude-sonnet-4-5-20250929-v1:0".into();
        assert_eq!(summary_max_tokens(&cfg, ""), 32_000);
    }

    /// At 90% of a 200k window the history leaves no room for 32k of
    /// output; models before 4.5 reject input + max_tokens over the window.
    #[test]
    fn summary_budget_fits_in_what_the_window_has_left() {
        let mut cfg = config();
        cfg.max_tokens = Some(8_192);
        cfg.model = "claude-sonnet-4-0".into();
        let window = crate::api::context_window_for_model(&cfg.model);
        assert_eq!(window, 200_000);
        let prompt = "x".repeat(600_000);
        let got = summary_max_tokens(&cfg, &prompt) as u64;
        let est_input = prompt.len() as u64 * 2 / 7 + 2_000;
        assert!(got + est_input <= window, "{got}");
        assert!(got >= 4_096);
        // Plenty of room on a 1M window.
        cfg.model = "claude-sonnet-5".into();
        assert_eq!(summary_max_tokens(&cfg, &prompt), 32_000);
    }
}

#[cfg(test)]
mod snip_tests {
    use super::*;

    fn tool_round(i: usize) -> [Message; 2] {
        [
            Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Thinking {
                        thinking: String::new(),
                        signature: format!("sig{i}"),
                    },
                    ContentBlock::ToolUse {
                        id: format!("t{i}"),
                        name: "Read".into(),
                        input: serde_json::json!({}),
                    },
                ],
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: format!("t{i}"),
                    content: vec![ToolResultContent::text(format!("result {i}"))],
                    is_error: None,
                }],
            },
        ]
    }

    /// 30 tool rounds, then a final answer that thought before replying.
    fn history() -> Vec<Message> {
        let mut h = vec![Message {
            role: Role::User,
            content: vec![ContentBlock::Text { text: "go".into() }],
        }];
        // Round 0's result is already a placeholder from an earlier snip.
        for i in 0..30 {
            h.extend(tool_round(i));
        }
        let ContentBlock::ToolResult { content, .. } = &mut h[2].content[0] else {
            unreachable!()
        };
        *content = vec![ToolResultContent::text(SNIP_PLACEHOLDER)];
        h.push(Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Thinking {
                    thinking: String::new(),
                    signature: "final".into(),
                },
                ContentBlock::Text {
                    text: "done".into(),
                },
            ],
        });
        h
    }

    fn thinking_at(h: &[Message]) -> Vec<usize> {
        (0..h.len())
            .filter(|&i| {
                h[i].content
                    .iter()
                    .any(|b| matches!(b, ContentBlock::Thinking { .. }))
            })
            .collect()
    }

    #[test]
    fn bound_thinking_after_the_first_edit_is_dropped() {
        let mut h = history();
        assert!(snip_compact(&mut h, "claude-opus-5-5"));
        // Messages 0-2 are unchanged (round 0 was snipped before), so the
        // thinking in message 1 still matches its prefix; message 4 is the
        // first edit, so every block from there on is stale.
        assert_eq!(thinking_at(&h), vec![1, 3]);
        let ContentBlock::ToolResult { content, .. } = &h[4].content[0] else {
            panic!()
        };
        assert_eq!(content[0], ToolResultContent::text(SNIP_PLACEHOLDER));
        assert_eq!(
            h.last().unwrap().content,
            vec![ContentBlock::Text {
                text: "done".into()
            }]
        );

        // Nothing left to snip: a second pass changes nothing.
        let before = h.clone();
        assert!(!snip_compact(&mut h, "claude-opus-5-5"));
        assert_eq!(h, before);
    }

    #[test]
    fn unbound_models_keep_their_thinking() {
        let mut h = history();
        let all = thinking_at(&h);
        assert!(snip_compact(&mut h, "claude-sonnet-5"));
        assert_eq!(thinking_at(&h), all);
    }

    /// Mid tool round the pending turn's thinking must be replayed, and an
    /// edit before it would invalidate it.
    #[test]
    fn bound_models_do_not_snip_mid_tool_round() {
        let mut h = history();
        h.pop();
        h.push(tool_round(99)[0].clone());
        let before = h.clone();
        assert!(!snip_compact(&mut h, "claude-fable-5-1"));
        assert_eq!(h, before);
        assert!(snip_compact(&mut h, "claude-sonnet-5"));
    }

    #[test]
    fn summary_prompt_elides_the_middle_of_huge_tool_results() {
        let big = format!(
            "{}{}{}",
            "a".repeat(50_000),
            "é".repeat(100_000),
            "z".repeat(50_000)
        );
        let h = vec![Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "t".into(),
                content: vec![ToolResultContent::text(big)],
                is_error: None,
            }],
        }];
        let out = render_history(&h);
        assert!(
            out.chars().count() < SUMMARY_TOOL_RESULT_MAX_CHARS + 200,
            "{}",
            out.len()
        );
        assert!(out.contains(&"a".repeat(4_000)));
        assert!(out.contains(&"z".repeat(4_000)));
        assert!(out.contains("[... 192000 chars elided ...]"));
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
