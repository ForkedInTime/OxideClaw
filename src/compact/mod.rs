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
/// Token thresholds (mirroring apiMicrocompact.ts DEFAULT_* values):
///   160 000  →  warn user that context is getting full
///   170 000  →  auto-snip (if auto_compact_enabled)
///   180 000  →  auto-summarise (if auto_compact_enabled)
use crate::api::{ApiBackend, MessagesRequest};
use crate::config::Config;
use anyhow::Result;

/// Warn the user at this many input tokens.
pub const COMPACT_WARN_TOKENS: u64 = 160_000;
/// Auto-snip at this threshold.
pub const COMPACT_SNIP_TOKENS: u64 = 170_000;
/// Auto-summarise at this threshold.
pub const COMPACT_SUMMARISE_TOKENS: u64 = 180_000;

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

/// Decide what action to take based on the latest input_tokens count.
pub fn compact_needed(input_tokens: u64) -> CompactNeeded {
    if input_tokens >= COMPACT_SUMMARISE_TOKENS {
        CompactNeeded::Summarise
    } else if input_tokens >= COMPACT_SNIP_TOKENS {
        CompactNeeded::Snip
    } else if input_tokens >= COMPACT_WARN_TOKENS {
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
                    out.push_str(&format!(
                        "[Thinking: {}]\n",
                        &thinking[..thinking.len().min(200)]
                    ));
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
        max_tokens: 4096,
        system: crate::api::types::SystemContent::Plain(SUMMARISE_SYSTEM.to_string()),
        messages: vec![Message {
            role: Role::User,
            content: vec![ContentBlock::Text { text: prompt }],
        }],
        tools: vec![],
        stream: None,
        thinking: None,
        output_config: None,
        betas: vec![],
        session_id: None,
    };

    let mut summary_text = String::new();
    client
        .messages_stream(request, |chunk| {
            summary_text.push_str(chunk);
        })
        .await?;

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
