//! OpenAI Responses API (`POST /v1/responses`), used for the `oai:` preset.
//!
//! Chat Completions drops a reasoning model's reasoning between tool calls;
//! the Responses API hands it back as `reasoning` output items. With
//! `store: false` and `include: ["reasoning.encrypted_content"]` each item
//! carries its encrypted reasoning, and sending the items back in the next
//! request's `input` lets the model continue a tool loop from where it was.
//!
//! Wire format: the OpenAI API reference for "Create a model response" and
//! "Streaming events" (https://platform.openai.com/docs/api-reference/responses,
//! https://platform.openai.com/docs/api-reference/responses-streaming), as
//! generated into openai-python's `src/openai/types/responses/` (checked
//! 2026-10-07: `response_create_params.py`, `response_input_item_param.py`,
//! `response_stream_event.py`, `response_usage.py`, `response_reasoning_item.py`).

use super::*;
use std::collections::BTreeMap;

// ─── Models ──────────────────────────────────────────────────────────────────

/// `gpt-<major>[.<minor>]` → `(major, minor)`.
pub(crate) fn gpt_version(model: &str) -> Option<(u32, u32)> {
    let rest = model.strip_prefix("gpt-")?;
    let mut nums = rest
        .split(|c: char| !c.is_ascii_digit())
        .take(2)
        .map(|n| n.parse::<u32>().ok());
    let major = nums.next()??;
    // Only a dot separates a minor version: `gpt-5-mini` is 5.0.
    let minor = if rest[major.to_string().len()..].starts_with('.') {
        nums.next().flatten().unwrap_or(0)
    } else {
        0
    };
    Some((major, minor))
}

/// OpenAI's reasoning models: the o-series, GPT-5 and later, and Codex. They
/// take `reasoning.effort` and return reasoning items. The `-chat` variants
/// (`gpt-5-chat-latest`) are not reasoning models, and o1-mini / o1-preview
/// reject the `reasoning` parameter.
pub(crate) fn is_reasoning_model(bare: &str) -> bool {
    let m = bare.to_ascii_lowercase();
    if m.contains("chat") || m.starts_with("o1-mini") || m.starts_with("o1-preview") {
        return false;
    }
    ["o1", "o3", "o4", "codex-"]
        .iter()
        .any(|p| m.starts_with(p))
        || gpt_version(&m).is_some_and(|(major, _)| major >= 5)
}

/// The `reasoning.effort` to send for the configured `level`, or `None`
/// for a level OxideClaw does not know. Every reasoning model takes
/// `low`/`medium`/`high`; `xhigh` arrived with gpt-5.1-codex-max and
/// GPT-5.2, so older models get `high` instead of a 400. `max` is sent as
/// the highest of those two the model takes: OpenAI documents it per model,
/// and a wrong guess is a failed request. GPT-5 `-pro` models think at
/// `high` or above only (gpt-5-pro takes nothing else), so `low` and
/// `medium` go out as `high` there.
pub(crate) fn reasoning_effort(bare: &str, level: &str) -> Option<&'static str> {
    let m = bare.to_ascii_lowercase();
    let version = gpt_version(&m);
    let xhigh = m.contains("codex-max") || version.is_some_and(|v| v >= (5, 2));
    let pro = m.contains("-pro") && version.is_some_and(|(major, _)| major >= 5);
    match level.trim().to_ascii_lowercase().as_str() {
        "low" | "medium" if pro => Some("high"),
        "low" => Some("low"),
        "medium" => Some("medium"),
        "high" => Some("high"),
        "xhigh" | "max" if xhigh => Some("xhigh"),
        "xhigh" | "max" => Some("high"),
        _ => None,
    }
}

/// The least reasoning `bare` takes, for a call that needs one word back
/// (the router's classifier). GPT-5 (5.0, mini and nano included) goes down
/// to `minimal`, GPT-5.1 and later to `none`; the o-series and Codex start
/// at `low`, and GPT-5 `-pro` models at `high`.
pub(crate) fn lowest_effort(bare: &str) -> &'static str {
    let m = bare.to_ascii_lowercase();
    let version = gpt_version(&m);
    if m.contains("-pro") && version.is_some_and(|(major, _)| major >= 5) {
        return "high";
    }
    if m.contains("codex") {
        return "low";
    }
    match version {
        Some((5, 0)) => "minimal",
        Some(v) if v >= (5, 1) => "none",
        _ => "low",
    }
}

// ─── Reasoning carried between requests ──────────────────────────────────────

/// One output item of a finished turn, in the order the model produced it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum TurnItem {
    /// A `reasoning` item with `encrypted_content`, sent back verbatim.
    Reasoning(serde_json::Value),
    /// An assistant `message` and the text it streamed. The history holds
    /// every message item's text joined by a blank line; `text` splits it
    /// back into one input item per message, each with its own `phase`.
    Message { phase: Option<String>, text: String },
    /// A `function_call` by `call_id`; its name and arguments are in the
    /// history.
    Call(String),
}

#[derive(Debug, Clone)]
pub(crate) struct StoredTurn {
    /// [`turn_key`] of the assistant message the turn became.
    key: String,
    /// Bare model id: encrypted reasoning is only valid for the model that
    /// produced it, so a `/model` switch leaves it out.
    model: String,
    items: Vec<TurnItem>,
}

/// Recent turns' reasoning, oldest first, capped at [`MAX_STORED_TURNS`].
/// A resumed session or another model's history finds nothing here, and
/// those turns go out without reasoning, as Chat Completions sends them.
pub(crate) type TurnStore = VecDeque<StoredTurn>;

/// Only the current tool loop's reasoning matters to most models; the cap
/// keeps a long session from growing without bound.
const MAX_STORED_TURNS: usize = 128;

fn remember_turn(store: &mut TurnStore, turn: StoredTurn) {
    store.retain(|t| t.key != turn.key);
    store.push_back(turn);
    let excess = store.len().saturating_sub(MAX_STORED_TURNS);
    store.drain(..excess);
}

/// Records a finished turn, or forgets an older one with the same key when
/// this turn has nothing to replay: two replies with the same text ("Done.")
/// share a key, and the older turn's reasoning must not be sent with the
/// newer reply.
fn record_turn(store: &mut TurnStore, key: String, model: String, items: Vec<TurnItem>) {
    let worth_keeping = items.iter().any(|i| match i {
        TurnItem::Reasoning(_) => true,
        TurnItem::Message { phase, .. } => phase.is_some(),
        TurnItem::Call(_) => false,
    });
    if worth_keeping {
        remember_turn(store, StoredTurn { key, model, items });
    } else {
        store.retain(|t| t.key != key);
    }
}

/// Identifies an assistant message across requests: its first tool call id,
/// else a hash of its text. The same blocks are stored in history, so the
/// key computed from the parsed reply finds the turn again.
fn turn_key(content: &[ContentBlock]) -> Option<String> {
    if let Some(id) = content.iter().find_map(|b| match b {
        ContentBlock::ToolUse { id, .. } => Some(id),
        _ => None,
    }) {
        return Some(format!("call:{id}"));
    }
    let text = joined_text(content);
    (!text.is_empty()).then(|| format!("text:{:016x}", fnv1a(&text)))
}

/// Between the texts of a response's message items, in the transcript and
/// in the history.
const MESSAGE_SEPARATOR: &str = "\n\n";

fn joined_text(content: &[ContentBlock]) -> String {
    content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// ─── Request ─────────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct ResponsesRequest {
    model: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    instructions: String,
    input: Vec<serde_json::Value>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<serde_json::Value>,
    stream: bool,
    /// Nothing is kept server-side; the reasoning comes back encrypted
    /// instead (`include`), so this works under Zero Data Retention too.
    store: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    include: Vec<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_output_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<serde_json::Value>,
}

/// The `input` items for `messages`. User turns become `message` and
/// `function_call_output` items, assistant turns `message` and
/// `function_call` items paired with them by `call_id`, preceded by the
/// turn's stored reasoning when `model` produced it. Thinking blocks (from
/// Claude, or this API's own summaries) are display only and never sent.
pub(super) fn translate_input(
    messages: &[Message],
    model: &str,
    store: &TurnStore,
) -> Vec<serde_json::Value> {
    use serde_json::json;
    // A stored turn goes back with the last assistant message that has its
    // key only. Two replies with the same text share a key, and sending its
    // reasoning item twice is a 400 (duplicate item id).
    let mut last_with_key = HashMap::new();
    for (i, msg) in messages.iter().enumerate() {
        if matches!(msg.role, Role::Assistant)
            && let Some(key) = turn_key(&msg.content)
        {
            last_with_key.insert(key, i);
        }
    }
    let mut out = Vec::with_capacity(messages.len());
    for (i, msg) in messages.iter().enumerate() {
        match msg.role {
            Role::User => {
                for block in &msg.content {
                    if let ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        ..
                    } = block
                    {
                        let output = content
                            .iter()
                            .map(|c| {
                                let ToolResultContent::Text { text } = c;
                                text.as_str()
                            })
                            .collect::<Vec<_>>()
                            .join("\n");
                        out.push(json!({
                            "type": "function_call_output",
                            "call_id": tool_use_id,
                            "output": output,
                        }));
                    }
                }
                let text = joined_text(&msg.content);
                let images: Vec<serde_json::Value> = msg
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::Image { source } => {
                            let url = match source {
                                ImageSource::Base64 { media_type, data } => {
                                    format!("data:{media_type};base64,{data}")
                                }
                                ImageSource::Url { url } => url.clone(),
                            };
                            Some(json!({ "type": "input_image", "image_url": url, "detail": "auto" }))
                        }
                        _ => None,
                    })
                    .collect();
                let has_text = msg
                    .content
                    .iter()
                    .any(|b| matches!(b, ContentBlock::Text { .. }));
                if images.is_empty() {
                    if has_text {
                        out.push(json!({ "type": "message", "role": "user", "content": text }));
                    }
                } else {
                    let mut parts = Vec::with_capacity(images.len() + 1);
                    if !text.is_empty() {
                        parts.push(json!({ "type": "input_text", "text": text }));
                    }
                    parts.extend(images);
                    out.push(json!({ "type": "message", "role": "user", "content": parts }));
                }
            }
            Role::Assistant => {
                let text = joined_text(&msg.content);
                let calls: Vec<(&String, &String, &serde_json::Value)> = msg
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::ToolUse { id, name, input } => Some((id, name, input)),
                        _ => None,
                    })
                    .collect();
                let message = |text: &str, phase: Option<&str>| {
                    let mut m = json!({ "type": "message", "role": "assistant", "content": text });
                    if let Some(p) = phase {
                        m["phase"] = p.into();
                    }
                    m
                };
                let call = |(id, name, input): (&String, &String, &serde_json::Value)| {
                    json!({
                        "type": "function_call",
                        "call_id": id,
                        "name": name,
                        "arguments": serde_json::to_string(input).unwrap_or_else(|_| "{}".into()),
                    })
                };
                let stored = turn_key(&msg.content)
                    .filter(|key| last_with_key.get(key) == Some(&i))
                    .and_then(|key| {
                        store
                            .iter()
                            .rev()
                            .find(|t| t.key == key && t.model == model)
                    });
                let items = stored.map(|t| t.items.as_slice()).unwrap_or_default();
                // Several message items (commentary, then the final answer)
                // go back one per item with its own phase while their texts
                // still make up the history's text. Otherwise the text goes
                // as one message, with a phase only if there was one item.
                let texts: Vec<&str> = items
                    .iter()
                    .filter_map(|i| match i {
                        TurnItem::Message { text, .. } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect();
                let split = texts.len() > 1 && texts.join(MESSAGE_SEPARATOR) == text;
                let one_message = texts.len() == 1;

                // Replay the turn in the order the model produced it: each
                // reasoning item must be followed by the item it led to.
                let mut turn = Vec::new();
                let mut text_sent = text.is_empty() || split;
                let mut calls_sent = vec![false; calls.len()];
                for item in items {
                    match item {
                        TurnItem::Reasoning(r) => turn.push(r.clone()),
                        TurnItem::Message { phase, text } if split => {
                            if !text.is_empty() {
                                turn.push(message(text, phase.as_deref()));
                            }
                        }
                        TurnItem::Message { phase, .. } if !text_sent => {
                            let phase = phase.as_deref().filter(|_| one_message);
                            turn.push(message(&text, phase));
                            text_sent = true;
                        }
                        TurnItem::Message { .. } => {}
                        TurnItem::Call(call_id) => {
                            if let Some(i) = calls.iter().position(|c| c.0 == call_id)
                                && !calls_sent[i]
                            {
                                calls_sent[i] = true;
                                turn.push(call(calls[i]));
                            }
                        }
                    }
                }
                if !text_sent {
                    turn.push(message(&text, None));
                }
                for (c, sent) in calls.iter().zip(calls_sent) {
                    if !sent {
                        turn.push(call(*c));
                    }
                }
                // Reasoning with nothing after it is rejected.
                while turn.last().is_some_and(|i| i["type"] == "reasoning") {
                    turn.pop();
                }
                out.extend(turn);
            }
        }
    }
    out
}

/// Function tools. `strict` defaults to true on this API, which requires
/// every property to be listed in `required`; the tool schemas are written
/// for the non-strict mode Chat Completions uses.
fn translate_tools(tools: &[ToolDefinition]) -> Vec<serde_json::Value> {
    tools
        .iter()
        .map(|t| {
            serde_json::json!({
                "type": "function",
                "name": t.name,
                "description": t.description,
                "parameters": t.input_schema,
                "strict": false,
            })
        })
        .collect()
}

// ─── Streaming response ──────────────────────────────────────────────────────

#[derive(Deserialize)]
struct ResponsesUsage {
    /// Every prompt token, cache hits included.
    #[serde(default)]
    input_tokens: u64,
    /// Includes `output_tokens_details.reasoning_tokens`, which are billed
    /// as output.
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    input_tokens_details: Option<OaiPromptTokensDetails>,
}

impl From<ResponsesUsage> for Usage {
    /// Cache hits are counted inside `input_tokens`; `Usage` keeps them
    /// apart so cost prices them at the cached rate, as for Chat Completions.
    fn from(u: ResponsesUsage) -> Self {
        let cached = u
            .input_tokens_details
            .and_then(|d| d.cached_tokens)
            .unwrap_or(0)
            .min(u.input_tokens);
        Usage {
            input_tokens: u.input_tokens - cached,
            output_tokens: u.output_tokens,
            cache_read_input_tokens: cached,
            cache_creation_input_tokens: 0,
        }
    }
}

/// `"<message> (<code>)"` from an `error` event or a failed response's
/// `error` object. The code (`context_length_exceeded`, `rate_limit_exceeded`)
/// stays in the text so `is_context_overflow` and the retry checks see it.
fn error_text(err: &serde_json::Value, fallback: &str) -> String {
    let msg = err["message"].as_str().unwrap_or(fallback);
    match err["code"].as_str() {
        Some(code) => format!("{msg} ({code})"),
        None => msg.to_string(),
    }
}

/// How the response ended.
enum End {
    Completed,
    /// `response.incomplete` with its `incomplete_details.reason`.
    Incomplete(String),
}

/// Parse a Responses SSE stream. Also returns the turn's items to replay
/// (empty unless the response completed). As with Chat Completions, a
/// failure after the 200 is an `Err`, never a short reply that looks done.
pub(super) async fn parse_responses_stream(
    resp: reqwest::Response,
    idle: std::time::Duration,
    on_text: impl FnMut(&str),
) -> Result<(StreamedResponse, Vec<TurnItem>)> {
    parse_responses_bytes(resp.bytes_stream(), idle, on_text).await
}

/// How long a reasoning model's stream may send nothing. With summaries
/// off OpenAI streams nothing between a reasoning item's start and end,
/// and `-pro` or `xhigh` reasoning can run for several minutes.
const REASONING_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);

/// [`parse_responses_stream`] over the body's bytes.
async fn parse_responses_bytes<S, B, E>(
    bytes: S,
    idle: std::time::Duration,
    mut on_text: impl FnMut(&str),
) -> Result<(StreamedResponse, Vec<TurnItem>)>
where
    S: futures_util::Stream<Item = std::result::Result<B, E>>,
    B: AsRef<[u8]>,
    E: std::fmt::Display,
{
    let mut stream = crate::api::idle_bounded(bytes, idle).eventsource();
    // A reasoning item has started; see the stall handling below.
    let mut reasoning_started = false;
    let mut result = StreamedResponse::default();
    let mut text_buf = String::new();
    // output_index → that message item's text; a new item starts a new
    // paragraph in `text_buf`.
    let mut texts: BTreeMap<u64, String> = BTreeMap::new();
    let mut text_item: Option<u64> = None;
    let mut thinking_buf = String::new();
    // (output_index, summary or content index) of the last reasoning delta:
    // a new summary part starts a new paragraph.
    let mut thinking_part: Option<(u64, u64)> = None;
    // output_index → (call_id, name, arguments)
    let mut calls: BTreeMap<u64, (String, String, String)> = BTreeMap::new();
    let mut items: BTreeMap<u64, TurnItem> = BTreeMap::new();
    let mut refusal = false;
    let mut end: Option<End> = None;

    loop {
        let event = match crate::api::next_sse_event(&mut stream).await {
            Ok(Some(event)) => event,
            Ok(None) => break,
            // Silence after a reasoning item started is the model thinking,
            // not a dropped connection. The stall error names the
            // connection, which the TUI retries; re-sending would pay for
            // the same reasoning again and none of it reaches /cost.
            Err(e) if reasoning_started && e.to_string().contains("stalled") => {
                return Err(anyhow!(
                    "the model's reasoning produced no output for {}s and the turn was \
                     abandoned. The reasoning may still be billed by the provider, but it is \
                     not counted in /cost or /budget.",
                    idle.as_secs()
                ));
            }
            Err(e) => return Err(e),
        };
        if event.data == "[DONE]" {
            break;
        }
        let v: serde_json::Value = match serde_json::from_str(&event.data) {
            Ok(v) => v,
            Err(e) => {
                warn!("Failed to parse Responses event: {e}: {}", event.data);
                continue;
            }
        };
        let idx = v["output_index"].as_u64().unwrap_or(0);
        let delta = v["delta"].as_str().unwrap_or("");
        match v["type"].as_str().unwrap_or("") {
            "response.output_text.delta" | "response.refusal.delta" => {
                refusal |= v["type"] == "response.refusal.delta";
                if !delta.is_empty() {
                    if !text_buf.is_empty() && text_item != Some(idx) {
                        on_text(MESSAGE_SEPARATOR);
                        text_buf.push_str(MESSAGE_SEPARATOR);
                    }
                    text_item = Some(idx);
                    on_text(delta);
                    text_buf.push_str(delta);
                    texts.entry(idx).or_default().push_str(delta);
                }
            }
            // Summaries (OpenAI) and raw reasoning text (gpt-oss servers).
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                let part = (
                    idx,
                    v["summary_index"]
                        .as_u64()
                        .or(v["content_index"].as_u64())
                        .unwrap_or(0),
                );
                if !thinking_buf.is_empty() && thinking_part != Some(part) {
                    thinking_buf.push_str("\n\n");
                }
                thinking_part = Some(part);
                thinking_buf.push_str(delta);
            }
            t @ ("response.output_item.added" | "response.output_item.done") => {
                let done = t == "response.output_item.done";
                let item = &v["item"];
                reasoning_started |= item["type"] == "reasoning";
                match item["type"].as_str() {
                    Some("function_call") => {
                        let entry = calls.entry(idx).or_default();
                        if let Some(id) = item["call_id"].as_str() {
                            entry.0 = id.to_string();
                        }
                        if let Some(name) = item["name"].as_str() {
                            entry.1 = name.to_string();
                        }
                        // The finished item has the whole arguments string.
                        if let Some(args) = item["arguments"].as_str()
                            && (done || entry.2.is_empty())
                        {
                            entry.2 = args.to_string();
                        }
                        if done {
                            items.insert(idx, TurnItem::Call(entry.0.clone()));
                        }
                    }
                    // `encrypted_content` is complete only on the done event.
                    // Without it the item cannot be replayed under
                    // `store: false` (the server would look up its id).
                    Some("reasoning")
                        if done
                            && item["encrypted_content"]
                                .as_str()
                                .is_some_and(|c| !c.is_empty()) =>
                    {
                        items.insert(idx, TurnItem::Reasoning(item.clone()));
                    }
                    Some("message") if done => {
                        let phase = item["phase"].as_str().map(str::to_string);
                        let text = texts.get(&idx).cloned().unwrap_or_default();
                        items.insert(idx, TurnItem::Message { phase, text });
                    }
                    _ => {}
                }
            }
            "response.function_call_arguments.delta" => {
                calls.entry(idx).or_default().2.push_str(delta);
            }
            "response.function_call_arguments.done" => {
                if let Some(args) = v["arguments"].as_str() {
                    calls.entry(idx).or_default().2 = args.to_string();
                }
            }
            t @ ("response.completed" | "response.incomplete") => {
                if let Ok(usage) =
                    serde_json::from_value::<ResponsesUsage>(v["response"]["usage"].clone())
                {
                    result.usage = usage.into();
                }
                end = Some(if t == "response.completed" {
                    End::Completed
                } else {
                    End::Incomplete(
                        v["response"]["incomplete_details"]["reason"]
                            .as_str()
                            .unwrap_or("")
                            .to_string(),
                    )
                });
                break;
            }
            "response.failed" => {
                return Err(anyhow!(
                    "provider stream error: {}",
                    error_text(&v["response"]["error"], "the response failed")
                ));
            }
            "error" => {
                return Err(anyhow!(
                    "provider stream error: {}",
                    error_text(&v, "unknown error")
                ));
            }
            // A proxy's or compat server's own `{"error": ...}` chunk.
            "" => {
                if let Some(msg) = chunk_error(&v) {
                    return Err(anyhow!("provider stream error: {msg}"));
                }
            }
            _ => {}
        }
    }

    let Some(end) = end else {
        return Err(anyhow!(
            "provider stream ended before the reply finished (no response.completed)"
        ));
    };

    if !thinking_buf.is_empty() {
        result.content.push(ContentBlock::Thinking {
            thinking: thinking_buf,
            signature: String::new(),
        });
    }
    if !text_buf.is_empty() {
        result.content.push(ContentBlock::Text { text: text_buf });
    }
    let mut has_calls = false;
    for (id, name, args) in calls.into_values() {
        if id.is_empty() || name.is_empty() {
            continue;
        }
        has_calls = true;
        let input =
            serde_json::from_str(&args).unwrap_or(serde_json::Value::Object(Default::default()));
        result
            .content
            .push(ContentBlock::ToolUse { id, name, input });
    }

    let items = match end {
        End::Completed => {
            result.stop_reason = Some(if has_calls {
                StopReason::ToolUse
            } else if refusal {
                StopReason::Refusal
            } else {
                StopReason::EndTurn
            });
            items.into_values().collect()
        }
        // `max_output_tokens` is the usual reason; tool calls cut off with
        // it are dropped by the caller (`drop_unanswerable_tool_calls`).
        End::Incomplete(reason) => {
            result.stop_reason = Some(if reason == "content_filter" {
                StopReason::Refusal
            } else {
                StopReason::MaxTokens
            });
            Vec::new()
        }
    };
    Ok((result, items))
}

// ─── Client ──────────────────────────────────────────────────────────────────

impl OpenAiCompatClient {
    /// `messages_stream` over `POST {base_url}/responses`.
    pub(super) async fn responses_stream(
        &self,
        request: MessagesRequest,
        on_text: impl FnMut(&str),
        store: &Mutex<TurnStore>,
    ) -> Result<StreamedResponse> {
        let model = request
            .model
            .split_once(':')
            .map_or(request.model.as_str(), |(_, bare)| bare)
            .to_string();
        let url = format!("{}/responses", self.base_url);
        debug!(
            "POST {url} model={model} (via {}, Responses API)",
            self.provider_name
        );

        let no_tools = self.no_tools.load(Ordering::Relaxed);
        let mut instructions = system_to_string(&request.system);
        if no_tools {
            instructions = patch_system_no_tools(&instructions);
        }
        let input = translate_input(
            &request.messages,
            &model,
            &store.lock().unwrap_or_else(|e| e.into_inner()),
        );
        let reasoning_model = is_reasoning_model(&model);
        // `output_config.effort` and a summarized `thinking` are set only for
        // reasoning models on this API; see `thinking::request_knobs`.
        let effort = request.output_config.as_ref().map(|o| o.effort.as_str());
        let summary = matches!(
            request.thinking,
            Some(ThinkingConfig::Adaptive { summarized: true })
        );
        // Reasoning summaries need a verified organization; see
        // `summary_refused`.
        let summary = summary && !self.no_summary.load(Ordering::Relaxed);
        let reasoning_param = |summary: bool| {
            (reasoning_model && (effort.is_some() || summary)).then(|| {
                let mut r = serde_json::json!({});
                if let Some(effort) = effort {
                    r["effort"] = effort.into();
                }
                if summary {
                    r["summary"] = "auto".into();
                }
                r
            })
        };
        let reasoning = reasoning_param(summary);
        let mut body = ResponsesRequest {
            model: model.clone(),
            instructions,
            input,
            tools: if no_tools {
                vec![]
            } else {
                translate_tools(&request.tools)
            },
            stream: true,
            store: false,
            // Other models return no reasoning, and some reject the field.
            include: if reasoning_model {
                vec!["reasoning.encrypted_content"]
            } else {
                vec![]
            },
            // As on Chat Completions: only a cap the user set.
            max_output_tokens: request.explicit_max_tokens.then_some(request.max_tokens),
            reasoning,
        };

        let mut resp = self.send_responses(&url, &body).await?;
        // Each fallback drops one feature the server refused and resends;
        // neither can fire twice, since its own condition no longer holds.
        let mut summary_sent = summary;
        while !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            if !body.tools.is_empty()
                && status == reqwest::StatusCode::BAD_REQUEST
                && text.contains("does not support tools")
            {
                // As on Chat Completions: text-only for the rest of the
                // session, with a one-time notice, rather than fail every turn.
                self.no_tools.store(true, Ordering::Relaxed);
                debug!("Model does not support tools — disabling for this session");
                body.instructions = patch_system_no_tools(&body.instructions);
                body.tools = vec![];
            } else if summary_sent && summary_refused(status, &text) {
                // Leave summaries off for the rest of the session rather than
                // fail every turn over a display setting.
                self.no_summary.store(true, Ordering::Relaxed);
                debug!("Reasoning summaries refused, sending without: {text}");
                body.reasoning = reasoning_param(false);
                summary_sent = false;
            } else {
                return Err(anyhow!("{} error {status}: {text}", self.provider_name));
            }
            resp = self.send_responses(&url, &body).await?;
        }

        let idle = if reasoning_model {
            REASONING_IDLE_TIMEOUT
        } else {
            crate::api::SSE_IDLE_TIMEOUT
        };
        let (result, items) = parse_responses_stream(resp, idle, on_text).await?;
        if let Some(key) = turn_key(&result.content) {
            record_turn(
                &mut store.lock().unwrap_or_else(|e| e.into_inner()),
                key,
                model,
                items,
            );
        }
        Ok(result)
    }
}

impl OpenAiCompatClient {
    async fn send_responses(
        &self,
        url: &str,
        body: &ResponsesRequest,
    ) -> Result<reqwest::Response> {
        super::super::retry::send_with_retry(
            || self.post(url, body),
            self.retry_notifier.as_ref(),
            false,
            &format!("{} request failed", self.provider_name),
        )
        .await
    }
}

/// A 400 over `reasoning.summary`: OpenAI generates reasoning summaries only
/// for organizations that have verified their identity, and says so in the
/// error ("must be verified to generate reasoning summaries").
fn summary_refused(status: reqwest::StatusCode, body: &str) -> bool {
    status == reqwest::StatusCode::BAD_REQUEST && body.to_ascii_lowercase().contains("summar")
}

#[cfg(test)]
mod tests {
    use super::super::max_tokens_tests::{client, request};
    use super::*;
    use serde_json::json;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Event stream in the shape the API sends: `event:` names the type,
    /// `data:` carries the event with its `sequence_number`.
    fn sse(events: &[serde_json::Value]) -> String {
        events
            .iter()
            .enumerate()
            .map(|(i, e)| {
                let mut e = e.clone();
                e["sequence_number"] = i.into();
                format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap())
            })
            .collect()
    }

    /// Answers one request with `body` as an event stream; the handle yields
    /// the request line and the JSON body the client sent.
    async fn serve(body: String) -> (String, tokio::task::JoinHandle<(String, serde_json::Value)>) {
        let (url, all) = serve_seq(vec![("200 OK", "text/event-stream", body)]).await;
        (
            url,
            tokio::spawn(async move { all.await.unwrap().remove(0) }),
        )
    }

    /// Answers one request per `(status, content type, body)`, in order, each
    /// on its own connection; the handle yields every request line and body.
    async fn serve_seq(
        replies: Vec<(&'static str, &'static str, String)>,
    ) -> (
        String,
        tokio::task::JoinHandle<Vec<(String, serde_json::Value)>>,
    ) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let mut seen = Vec::new();
            for (status, content_type, body) in replies {
                let (mut sock, _) = listener.accept().await.unwrap();
                let mut buf = Vec::new();
                let mut chunk = [0u8; 8192];
                let body_start = loop {
                    let n = sock.read(&mut chunk).await.unwrap();
                    assert!(n > 0, "connection closed before the body");
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        break i + 4;
                    }
                };
                let head = String::from_utf8_lossy(&buf[..body_start]).to_string();
                let len: usize = head
                    .to_ascii_lowercase()
                    .lines()
                    .find_map(|l| {
                        l.strip_prefix("content-length:")
                            .map(|v| v.trim().to_string())
                    })
                    .unwrap()
                    .parse()
                    .unwrap();
                while buf.len() < body_start + len {
                    let n = sock.read(&mut chunk).await.unwrap();
                    assert!(n > 0, "connection closed mid-body");
                    buf.extend_from_slice(&chunk[..n]);
                }
                let resp = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: {content_type}\r\n\
                     content-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.shutdown().await;
                let request_line = head.lines().next().unwrap_or_default().to_string();
                let json = serde_json::from_slice(&buf[body_start..body_start + len]).unwrap();
                seen.push((request_line, json));
            }
            seen
        });
        (format!("http://{addr}"), handle)
    }

    fn responses_client(base_url: String) -> OpenAiCompatClient {
        let mut c = client(base_url);
        c.responses = Some(Default::default());
        c
    }

    fn usage(input: u64, cached: u64, output: u64, reasoning: u64) -> serde_json::Value {
        json!({
            "input_tokens": input,
            "input_tokens_details": { "cached_tokens": cached },
            "output_tokens": output,
            "output_tokens_details": { "reasoning_tokens": reasoning },
            "total_tokens": input + output,
        })
    }

    fn completed(usage: serde_json::Value) -> serde_json::Value {
        json!({
            "type": "response.completed",
            "response": { "id": "resp_1", "object": "response", "status": "completed", "usage": usage },
        })
    }

    fn text_turn() -> String {
        sse(&[
            json!({"type": "response.created", "response": {"id": "resp_1", "status": "in_progress", "output": []}}),
            json!({"type": "response.in_progress", "response": {"id": "resp_1", "status": "in_progress", "output": []}}),
            json!({"type": "response.output_item.added", "output_index": 0,
                   "item": {"id": "msg_1", "type": "message", "status": "in_progress", "role": "assistant", "content": []}}),
            json!({"type": "response.content_part.added", "item_id": "msg_1", "output_index": 0, "content_index": 0,
                   "part": {"type": "output_text", "text": "", "annotations": []}}),
            json!({"type": "response.output_text.delta", "item_id": "msg_1", "output_index": 0, "content_index": 0,
                   "delta": "Hello", "logprobs": []}),
            json!({"type": "response.output_text.delta", "item_id": "msg_1", "output_index": 0, "content_index": 0,
                   "delta": " world", "logprobs": []}),
            json!({"type": "response.output_text.done", "item_id": "msg_1", "output_index": 0, "content_index": 0,
                   "text": "Hello world", "logprobs": []}),
            json!({"type": "response.output_item.done", "output_index": 0,
                   "item": {"id": "msg_1", "type": "message", "status": "completed", "role": "assistant",
                            "content": [{"type": "output_text", "text": "Hello world", "annotations": []}]}}),
            completed(usage(120, 0, 5, 0)),
        ])
    }

    fn reasoning_item() -> serde_json::Value {
        json!({
            "id": "rs_1",
            "type": "reasoning",
            "summary": [{"type": "summary_text", "text": "**Reading a.rs** first."}],
            "encrypted_content": "gAAAAB-ENCRYPTED",
        })
    }

    /// Reasoning (with a streamed summary), then a function call whose
    /// arguments arrive in pieces.
    fn tool_turn() -> String {
        sse(&[
            json!({"type": "response.created", "response": {"id": "resp_2", "status": "in_progress", "output": []}}),
            json!({"type": "response.output_item.added", "output_index": 0,
                   "item": {"id": "rs_1", "type": "reasoning", "summary": []}}),
            json!({"type": "response.reasoning_summary_part.added", "item_id": "rs_1", "output_index": 0,
                   "summary_index": 0, "part": {"type": "summary_text", "text": ""}}),
            json!({"type": "response.reasoning_summary_text.delta", "item_id": "rs_1", "output_index": 0,
                   "summary_index": 0, "delta": "**Reading a.rs**"}),
            json!({"type": "response.reasoning_summary_text.delta", "item_id": "rs_1", "output_index": 0,
                   "summary_index": 0, "delta": " first."}),
            json!({"type": "response.reasoning_summary_text.done", "item_id": "rs_1", "output_index": 0,
                   "summary_index": 0, "text": "**Reading a.rs** first."}),
            json!({"type": "response.output_item.done", "output_index": 0, "item": reasoning_item()}),
            json!({"type": "response.output_item.added", "output_index": 1,
                   "item": {"id": "fc_1", "type": "function_call", "status": "in_progress",
                            "call_id": "call_1", "name": "Read", "arguments": ""}}),
            json!({"type": "response.function_call_arguments.delta", "item_id": "fc_1", "output_index": 1,
                   "delta": "{\"file_pa"}),
            json!({"type": "response.function_call_arguments.delta", "item_id": "fc_1", "output_index": 1,
                   "delta": "th\":\"a.rs\"}"}),
            json!({"type": "response.function_call_arguments.done", "item_id": "fc_1", "output_index": 1,
                   "arguments": "{\"file_path\":\"a.rs\"}"}),
            json!({"type": "response.output_item.done", "output_index": 1,
                   "item": {"id": "fc_1", "type": "function_call", "status": "completed",
                            "call_id": "call_1", "name": "Read", "arguments": "{\"file_path\":\"a.rs\"}"}}),
            completed(usage(900, 0, 140, 96)),
        ])
    }

    #[tokio::test]
    async fn text_turn_streams_to_the_transcript() {
        let (url, req) = serve(text_turn()).await;
        let mut shown = String::new();
        let r = responses_client(url)
            .messages_stream(request("oai:gpt-4.1"), |t| shown.push_str(t))
            .await
            .unwrap();
        assert_eq!(shown, "Hello world");
        assert_eq!(
            r.content,
            vec![ContentBlock::Text {
                text: "Hello world".into()
            }]
        );
        assert_eq!(r.stop_reason, Some(StopReason::EndTurn));
        assert_eq!((r.usage.input_tokens, r.usage.output_tokens), (120, 5));
        let (line, _) = req.await.unwrap();
        assert!(line.starts_with("POST /responses "), "{line}");
    }

    #[tokio::test]
    async fn function_call_arguments_are_assembled_into_a_tool_use() {
        let (url, _req) = serve(tool_turn()).await;
        let r = responses_client(url)
            .messages_stream(request("oai:gpt-5"), |_| {})
            .await
            .unwrap();
        assert_eq!(r.stop_reason, Some(StopReason::ToolUse));
        assert_eq!(
            r.content,
            vec![
                ContentBlock::Thinking {
                    thinking: "**Reading a.rs** first.".into(),
                    signature: String::new(),
                },
                ContentBlock::ToolUse {
                    id: "call_1".into(),
                    name: "Read".into(),
                    input: json!({"file_path": "a.rs"}),
                },
            ]
        );
        // Reasoning tokens are part of output_tokens, billed as output.
        assert_eq!(r.usage.output_tokens, 140);
    }

    /// The request after a tool call: history plus the tool's result.
    fn follow_up(model: &str, first: Vec<ContentBlock>) -> MessagesRequest {
        let mut req = request(model);
        req.messages.push(Message {
            role: Role::Assistant,
            content: first,
        });
        req.messages.push(Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "call_1".into(),
                content: vec![ToolResultContent::Text {
                    text: "fn main() {}".into(),
                }],
                is_error: None,
            }],
        });
        req
    }

    /// The point of this API: the reasoning behind a tool call goes back,
    /// encrypted and verbatim, ahead of the call it led to, from a clone of
    /// the client as each TUI turn runs on.
    #[tokio::test]
    async fn reasoning_items_go_back_in_the_next_request() {
        let (url, _req) = serve(tool_turn()).await;
        let mut c = responses_client(url);
        let first = c
            .clone()
            .messages_stream(request("oai:gpt-5"), |_| {})
            .await
            .unwrap();

        let (url, req) = serve(text_turn()).await;
        c.base_url = url;
        c.clone()
            .messages_stream(follow_up("oai:gpt-5", first.content.clone()), |_| {})
            .await
            .unwrap();
        let (_, body) = req.await.unwrap();
        assert_eq!(
            body["input"],
            json!([
                {"type": "message", "role": "user", "content": "hi"},
                reasoning_item(),
                {"type": "function_call", "call_id": "call_1", "name": "Read",
                 "arguments": "{\"file_path\":\"a.rs\"}"},
                {"type": "function_call_output", "call_id": "call_1", "output": "fn main() {}"},
            ]),
            "{body}"
        );
        assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
        assert_eq!(body["store"], false);

        // Another model cannot read that reasoning: after /model it is left
        // out and the call goes on alone.
        let (url, req) = serve(text_turn()).await;
        c.base_url = url;
        c.clone()
            .messages_stream(follow_up("oai:gpt-5-mini", first.content.clone()), |_| {})
            .await
            .unwrap();
        let (_, body) = req.await.unwrap();
        let types: Vec<_> = body["input"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["type"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(
            types,
            ["message", "function_call", "function_call_output"],
            "{body}"
        );

        // A resumed session (a new client) has none on record either.
        let (url, req) = serve(text_turn()).await;
        responses_client(url)
            .messages_stream(follow_up("oai:gpt-5", first.content), |_| {})
            .await
            .unwrap();
        let (_, body) = req.await.unwrap();
        assert!(
            body["input"]
                .as_array()
                .unwrap()
                .iter()
                .all(|i| i["type"] != "reasoning"),
            "{body}"
        );
    }

    #[test]
    fn stored_turns_are_bounded_and_replace_their_key() {
        let turn = |key: String| StoredTurn {
            key,
            model: "gpt-5".into(),
            items: vec![TurnItem::Reasoning(reasoning_item())],
        };
        let mut store = TurnStore::new();
        for i in 0..MAX_STORED_TURNS + 10 {
            remember_turn(&mut store, turn(format!("call:{i}")));
        }
        assert_eq!(store.len(), MAX_STORED_TURNS);
        assert_eq!(store.front().unwrap().key, "call:10");
        remember_turn(&mut store, turn("call:10".into()));
        assert_eq!(store.len(), MAX_STORED_TURNS);
        assert_eq!(store.back().unwrap().key, "call:10");
    }

    /// A reasoning item must be followed by the item it led to; one whose
    /// call is no longer in the history is dropped rather than sent last.
    #[test]
    fn reasoning_is_never_sent_without_a_following_item() {
        let store = TurnStore::from([StoredTurn {
            key: "text:".to_string() + &format!("{:016x}", fnv1a("done")),
            model: "gpt-5".into(),
            items: vec![
                TurnItem::Reasoning(reasoning_item()),
                TurnItem::Message {
                    phase: Some("final_answer".into()),
                    text: "done".into(),
                },
            ],
        }]);
        let msgs = [Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Text {
                text: "done".into(),
            }],
        }];
        let input = translate_input(&msgs, "gpt-5", &store);
        assert_eq!(input[0], reasoning_item());
        assert_eq!(
            input[1],
            json!({"type": "message", "role": "assistant", "content": "done", "phase": "final_answer"})
        );

        let store = TurnStore::from([StoredTurn {
            key: "call:call_9".into(),
            model: "gpt-5".into(),
            items: vec![
                TurnItem::Call("call_9".into()),
                TurnItem::Reasoning(reasoning_item()),
                TurnItem::Call("call_gone".into()),
            ],
        }]);
        let msgs = [Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "call_9".into(),
                name: "Glob".into(),
                input: json!({}),
            }],
        }];
        let input = translate_input(&msgs, "gpt-5", &store);
        assert_eq!(input.len(), 1, "{input:?}");
        assert_eq!(input[0]["call_id"], "call_9");
    }

    /// `cached_tokens` is part of `input_tokens`; it is billed at the cached
    /// rate through the same cost path as Chat Completions.
    #[tokio::test]
    async fn cached_tokens_are_billed_as_cache_reads() {
        let body = sse(&[
            json!({"type": "response.output_text.delta", "item_id": "msg_1", "output_index": 0,
                   "content_index": 0, "delta": "ok", "logprobs": []}),
            completed(usage(10_000, 8_000, 300, 200)),
        ]);
        let (url, _req) = serve(body).await;
        let u = responses_client(url)
            .messages_stream(request("oai:gpt-4o"), |_| {})
            .await
            .unwrap()
            .usage;
        assert_eq!(
            (u.input_tokens, u.cache_read_input_tokens, u.output_tokens),
            (2_000, 8_000, 300)
        );
        assert_eq!(u.context_tokens(), 10_000);

        let mut t = crate::cost::CostTracker::new();
        t.record_with_cache(
            "oai:gpt-4o",
            u.input_tokens,
            u.output_tokens,
            u.cache_read_input_tokens,
            u.cache_creation_input_tokens,
        );
        // 2K in at $2.50 + 8K cached at $1.25 + 300 out at $10, per MTok.
        assert!(
            (t.total_cost_usd - 0.018).abs() < 1e-12,
            "{}",
            t.total_cost_usd
        );

        // GPT-5 reads cache at a tenth of its $1.25 input: 2K in at $1.25 +
        // 8K cached at $0.125 + 300 out at $10, per MTok.
        let mut t = crate::cost::CostTracker::new();
        t.record_with_cache(
            "oai:gpt-5",
            u.input_tokens,
            u.output_tokens,
            u.cache_read_input_tokens,
            u.cache_creation_input_tokens,
        );
        assert!(
            (t.total_cost_usd - 0.0065).abs() < 1e-12,
            "{}",
            t.total_cost_usd
        );
    }

    async fn stream_error(events: &[serde_json::Value]) -> String {
        let (url, _req) = serve(sse(events)).await;
        responses_client(url)
            .messages_stream(request("oai:gpt-5"), |_| {})
            .await
            .unwrap_err()
            .to_string()
    }

    #[tokio::test]
    async fn error_events_and_failed_responses_are_errors() {
        let err = stream_error(&[
            json!({"type": "response.output_text.delta", "item_id": "msg_1", "output_index": 0,
                   "content_index": 0, "delta": "Hal", "logprobs": []}),
            json!({"type": "error", "code": "server_error", "message": "The server had an error", "param": null}),
        ])
        .await;
        assert!(
            err.contains("The server had an error (server_error)"),
            "{err}"
        );

        let err = stream_error(&[json!({"type": "response.failed", "response": {
            "id": "resp_1", "status": "failed",
            "error": {"code": "rate_limit_exceeded", "message": "Rate limit reached for gpt-5"}}})])
        .await;
        assert!(err.contains("Rate limit reached for gpt-5"), "{err}");

        // A prompt over the window is recognised, so the TUI compacts and
        // retries instead of failing the turn.
        let err = stream_error(&[json!({"type": "error", "code": "context_length_exceeded",
            "message": "Your input exceeds the context window of this model. Please adjust your input and try again.",
            "param": "input"})])
        .await;
        assert!(crate::api::is_context_overflow(&err), "{err}");

        // A stream cut off before response.completed is not a finished reply.
        let err = stream_error(&[
            json!({"type": "response.output_text.delta", "item_id": "msg_1",
            "output_index": 0, "content_index": 0, "delta": "Half an ans", "logprobs": []}),
        ])
        .await;
        assert!(err.contains("before the reply finished"), "{err}");
    }

    /// `max_output_tokens` ends the response as incomplete: the partial text
    /// is kept, the cut-off tool call is dropped, and the turn reports
    /// max_tokens so the existing continuation handling runs.
    #[tokio::test]
    async fn incomplete_response_is_a_max_tokens_stop() {
        let body = sse(&[
            json!({"type": "response.output_text.delta", "item_id": "msg_1", "output_index": 0,
                   "content_index": 0, "delta": "Writing it now", "logprobs": []}),
            json!({"type": "response.output_item.added", "output_index": 1,
                   "item": {"id": "fc_1", "type": "function_call", "call_id": "call_1",
                            "name": "Write", "arguments": ""}}),
            json!({"type": "response.function_call_arguments.delta", "item_id": "fc_1",
                   "output_index": 1, "delta": "{\"file_path\":\"a.rs\",\"cont"}),
            json!({"type": "response.incomplete", "response": {
                "id": "resp_1", "status": "incomplete",
                "incomplete_details": {"reason": "max_output_tokens"},
                "usage": usage(500, 0, 64, 0)}}),
        ]);
        let (url, _req) = serve(body).await;
        let backend = crate::api::ApiBackend::OpenAiCompat(responses_client(url));
        let r = backend
            .messages_stream(request("oai:gpt-5"), |_| {})
            .await
            .unwrap();
        assert_eq!(r.stop_reason, Some(StopReason::MaxTokens));
        assert_eq!(
            r.content,
            vec![ContentBlock::Text {
                text: "Writing it now".into()
            }]
        );
        assert_eq!(r.usage.output_tokens, 64);
    }

    #[tokio::test]
    async fn request_has_the_responses_shape() {
        let (url, req) = serve(text_turn()).await;
        let mut r = request("oai:gpt-5");
        r.system = SystemContent::Plain("You are terse.".into());
        r.tools = vec![ToolDefinition {
            name: "Read".into(),
            description: "Read a file".into(),
            input_schema: json!({"type": "object", "properties": {"file_path": {"type": "string"}}}),
            cache_control: None,
        }];
        r.messages = vec![
            Message {
                role: Role::User,
                content: vec![
                    ContentBlock::Text {
                        text: "what is this?".into(),
                    },
                    ContentBlock::Image {
                        source: ImageSource::Base64 {
                            media_type: "image/png".into(),
                            data: "iVBORw0KGgo=".into(),
                        },
                    },
                ],
            },
            Message {
                role: Role::Assistant,
                content: vec![
                    // Claude's signed thinking from before a /model switch.
                    ContentBlock::Thinking {
                        thinking: "hmm".into(),
                        signature: "sig".into(),
                    },
                    ContentBlock::Text {
                        text: "Let me look.".into(),
                    },
                    ContentBlock::ToolUse {
                        id: "toolu_01".into(),
                        name: "Read".into(),
                        input: json!({"file_path": "a.png"}),
                    },
                ],
            },
            Message {
                role: Role::User,
                content: vec![
                    ContentBlock::ToolResult {
                        tool_use_id: "toolu_01".into(),
                        content: vec![ToolResultContent::Text {
                            text: "binary".into(),
                        }],
                        is_error: None,
                    },
                    ContentBlock::Text {
                        text: "go on".into(),
                    },
                ],
            },
        ];
        r.output_config = Some(OutputConfig {
            effort: "high".into(),
        });
        r.thinking = Some(ThinkingConfig::Adaptive { summarized: true });
        responses_client(url)
            .messages_stream(r, |_| {})
            .await
            .unwrap();
        let (_, body) = req.await.unwrap();
        assert_eq!(
            body,
            json!({
                "model": "gpt-5",
                "instructions": "You are terse.",
                "input": [
                    {"type": "message", "role": "user", "content": [
                        {"type": "input_text", "text": "what is this?"},
                        {"type": "input_image", "image_url": "data:image/png;base64,iVBORw0KGgo=", "detail": "auto"},
                    ]},
                    {"type": "message", "role": "assistant", "content": "Let me look."},
                    {"type": "function_call", "call_id": "toolu_01", "name": "Read",
                     "arguments": "{\"file_path\":\"a.png\"}"},
                    {"type": "function_call_output", "call_id": "toolu_01", "output": "binary"},
                    {"type": "message", "role": "user", "content": "go on"},
                ],
                "tools": [{"type": "function", "name": "Read", "description": "Read a file",
                           "parameters": {"type": "object", "properties": {"file_path": {"type": "string"}}},
                           "strict": false}],
                "stream": true,
                "store": false,
                "include": ["reasoning.encrypted_content"],
                "max_output_tokens": 12345,
                "reasoning": {"effort": "high", "summary": "auto"},
            })
        );

        // A non-reasoning model gets neither reasoning field, and an unset
        // maxTokens leaves the server default.
        let (url, req) = serve(text_turn()).await;
        let mut r = request("oai:gpt-4.1");
        r.explicit_max_tokens = false;
        responses_client(url)
            .messages_stream(r, |_| {})
            .await
            .unwrap();
        let (_, body) = req.await.unwrap();
        for field in ["include", "reasoning", "max_output_tokens", "instructions"] {
            assert!(body.get(field).is_none(), "{field}: {body}");
        }
    }

    fn env_with_key(k: &str) -> Option<String> {
        match k {
            "OPENAI_API_KEY" => Some("sk-test".into()),
            "OPENAI_BASE_URL" => Some("http://10.0.0.5:8000/v1".into()),
            _ => None,
        }
    }

    #[test]
    fn only_official_openai_uses_responses_by_default() {
        let oai = PROVIDERS.iter().find(|p| p.prefix == "oai").unwrap();
        assert_eq!(oai.base_url, "https://api.openai.com/v1");
        let uses = |model: &str, api| {
            OpenAiCompatClient::from_model_env(model, api, |k| {
                env_with_key(k).or_else(|| Some("key".into()))
            })
            .unwrap()
            .responses
            .is_some()
        };
        assert!(uses("oai:gpt-5", OpenAiApi::Auto));
        // The opt-out.
        assert!(!uses("oai:gpt-5", OpenAiApi::Chat));
        // A non-official base URL keeps Chat Completions unless forced.
        assert!(!uses("openai-compat:gpt-5", OpenAiApi::Auto));
        assert!(uses("openai-compat:gpt-5", OpenAiApi::Responses));
        // So does LM Studio (a user-set LM_STUDIO_HOST), which serves
        // /v1/responses too.
        assert!(!uses("lmstudio:qwen", OpenAiApi::Auto));
        assert!(uses("lmstudio:qwen", OpenAiApi::Responses));
        assert!(!uses("lmstudio:qwen", OpenAiApi::Chat));
        // Named cloud presets never switch.
        for model in [
            "groq:llama-3.3-70b",
            "gemini:gemini-2.5-flash",
            "deepseek:deepseek-chat",
            "openrouter:openai/gpt-5",
            "mistral:mistral-large-latest",
        ] {
            assert!(!uses(model, OpenAiApi::Auto), "{model}");
            assert!(!uses(model, OpenAiApi::Responses), "{model}");
        }
        assert!(!uses_responses_api("claude-sonnet-5", OpenAiApi::Responses));
        assert_eq!(OpenAiApi::parse(" Chat "), Some(OpenAiApi::Chat));
        assert_eq!(OpenAiApi::parse("responses"), Some(OpenAiApi::Responses));
        assert_eq!(OpenAiApi::parse("auto"), Some(OpenAiApi::Auto));
        assert_eq!(OpenAiApi::parse("completions"), None);
    }

    /// `openaiApi: "chat"` sends oai: back to /chat/completions, end to end.
    #[tokio::test]
    async fn chat_opt_out_posts_to_chat_completions() {
        let (url, req) = serve(String::from("data: [DONE]\n\n")).await;
        let mut c =
            OpenAiCompatClient::from_model_env("oai:gpt-5", OpenAiApi::Chat, env_with_key).unwrap();
        c.base_url = url;
        let _ = c.messages_stream(request("oai:gpt-5"), |_| {}).await;
        let (line, body) = req.await.unwrap();
        assert!(line.starts_with("POST /chat/completions "), "{line}");
        assert!(body.get("messages").is_some(), "{body}");
    }

    #[test]
    fn reasoning_models_and_their_effort_levels() {
        for m in [
            "o1",
            "o3",
            "o3-mini",
            "o4-mini",
            "o3-pro",
            "gpt-5",
            "gpt-5-mini",
            "gpt-5.1",
            "gpt-5.1-codex-max",
            "gpt-5.2",
            "gpt-5.5-pro",
            "gpt-6-sol",
            "codex-mini-latest",
        ] {
            assert!(is_reasoning_model(m), "{m}");
        }
        for m in [
            "gpt-4o",
            "gpt-4.1-mini",
            "gpt-5-chat-latest",
            "gpt-5.3-chat-latest",
            "chatgpt-4o-latest",
            "o1-mini",
            "gpt-oss-120b",
        ] {
            assert!(!is_reasoning_model(m), "{m}");
        }
        assert_eq!(reasoning_effort("gpt-5", " HIGH "), Some("high"));
        assert_eq!(reasoning_effort("o4-mini", "xhigh"), Some("high"));
        assert_eq!(reasoning_effort("gpt-5.1", "max"), Some("high"));
        assert_eq!(reasoning_effort("gpt-5.2", "xhigh"), Some("xhigh"));
        assert_eq!(reasoning_effort("gpt-5.1-codex-max", "max"), Some("xhigh"));
        assert_eq!(reasoning_effort("gpt-6-sol", "xhigh"), Some("xhigh"));
        assert_eq!(reasoning_effort("gpt-5", "ultra"), None);
        // gpt-5-pro takes `high` only; the -pro models never go below it.
        assert_eq!(reasoning_effort("gpt-5-pro", "low"), Some("high"));
        assert_eq!(reasoning_effort("gpt-5-pro", "medium"), Some("high"));
        assert_eq!(reasoning_effort("gpt-5-pro", "max"), Some("high"));
        assert_eq!(reasoning_effort("gpt-5.5-pro", "medium"), Some("high"));
        assert_eq!(reasoning_effort("gpt-5.5-pro", "xhigh"), Some("xhigh"));
        assert_eq!(reasoning_effort("o3-pro", "low"), Some("low"));
        assert_eq!(reasoning_effort("gpt-5-mini", "low"), Some("low"));
        assert_eq!(lowest_effort("gpt-5-mini"), "minimal");
        assert_eq!(lowest_effort("gpt-5-nano"), "minimal");
        assert_eq!(lowest_effort("gpt-5.1"), "none");
        assert_eq!(lowest_effort("gpt-5.4-mini"), "none");
        assert_eq!(lowest_effort("o4-mini"), "low");
        assert_eq!(lowest_effort("gpt-5.1-codex"), "low");
        assert_eq!(lowest_effort("gpt-5-pro"), "high");
        assert_eq!(gpt_version("gpt-5-mini"), Some((5, 0)));
        assert_eq!(gpt_version("gpt-5.2-codex"), Some((5, 2)));
    }
    fn assistant(text: &str) -> Message {
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Text { text: text.into() }],
        }
    }

    fn user(text: &str) -> Message {
        Message {
            role: Role::User,
            content: vec![ContentBlock::Text { text: text.into() }],
        }
    }

    /// Two replies with the same text share a key. The stored turn's
    /// reasoning goes back once, with the newest of them: the same item id
    /// twice in `input` is a 400 on every later request.
    #[test]
    fn identical_replies_replay_their_reasoning_once() {
        let key = turn_key(&assistant("Done.").content).unwrap();
        let store = TurnStore::from([StoredTurn {
            key: key.clone(),
            model: "gpt-5".into(),
            items: vec![
                TurnItem::Reasoning(reasoning_item()),
                TurnItem::Message {
                    phase: Some("final_answer".into()),
                    text: "Done.".into(),
                },
            ],
        }]);
        let msgs = [
            user("a"),
            assistant("Done."),
            user("b"),
            assistant("Done."),
            user("c"),
        ];
        let input = translate_input(&msgs, "gpt-5", &store);
        let reasoning: Vec<usize> = input
            .iter()
            .enumerate()
            .filter(|(_, i)| i["type"] == "reasoning")
            .map(|(n, _)| n)
            .collect();
        assert_eq!(reasoning, [3], "{input:#?}");
        assert_eq!(input[4]["phase"], "final_answer");
        assert!(input[1].get("phase").is_none(), "{input:#?}");

        // A newer "Done." with nothing to replay forgets the older turn, so
        // its reasoning is not sent with a reply it did not lead to.
        let mut store = store;
        record_turn(
            &mut store,
            key,
            "gpt-5".into(),
            vec![TurnItem::Message {
                phase: None,
                text: "Done.".into(),
            }],
        );
        assert!(store.is_empty());
    }

    /// Commentary, reasoning, then the final answer: the transcript and
    /// history keep the two messages apart, and the replay sends each with
    /// its own phase, in the order the model produced them.
    #[tokio::test]
    async fn message_items_stay_separate_with_their_own_phase() {
        let msg = |idx: u64, id: &str, text: &str, phase: &str| {
            [
                json!({"type": "response.output_item.added", "output_index": idx,
                       "item": {"id": id, "type": "message", "status": "in_progress",
                                "role": "assistant", "content": [], "phase": phase}}),
                json!({"type": "response.output_text.delta", "item_id": id, "output_index": idx,
                       "content_index": 0, "delta": text, "logprobs": []}),
                json!({"type": "response.output_item.done", "output_index": idx,
                       "item": {"id": id, "type": "message", "status": "completed", "role": "assistant",
                                "phase": phase,
                                "content": [{"type": "output_text", "text": text, "annotations": []}]}}),
            ]
        };
        let mut events = Vec::new();
        events.extend(msg(0, "msg_1", "I'll check.", "commentary"));
        events.push(
            json!({"type": "response.output_item.done", "output_index": 1,
                           "item": reasoning_item()}),
        );
        events.extend(msg(2, "msg_2", "All good.", "final_answer"));
        events.push(completed(usage(100, 0, 20, 8)));

        let (url, _req) = serve(sse(&events)).await;
        let mut c = responses_client(url);
        let mut shown = String::new();
        let first = c
            .clone()
            .messages_stream(request("oai:gpt-5.2-codex"), |t| shown.push_str(t))
            .await
            .unwrap();
        assert_eq!(shown, "I'll check.\n\nAll good.");
        assert_eq!(
            first.content,
            vec![ContentBlock::Text {
                text: "I'll check.\n\nAll good.".into()
            }]
        );

        let (url, req) = serve(text_turn()).await;
        c.base_url = url;
        let mut next = request("oai:gpt-5.2-codex");
        next.messages.push(Message {
            role: Role::Assistant,
            content: first.content,
        });
        next.messages.push(user("thanks"));
        c.messages_stream(next, |_| {}).await.unwrap();
        let (_, body) = req.await.unwrap();
        assert_eq!(
            body["input"],
            json!([
                {"type": "message", "role": "user", "content": "hi"},
                {"type": "message", "role": "assistant", "content": "I'll check.", "phase": "commentary"},
                reasoning_item(),
                {"type": "message", "role": "assistant", "content": "All good.", "phase": "final_answer"},
                {"type": "message", "role": "user", "content": "thanks"},
            ]),
            "{body}"
        );
    }

    /// The router's classifier on a GPT-5 mini low tier: 16 output tokens
    /// were spent on reasoning at the default effort, so every prompt was
    /// billed and the heuristic decided anyway.
    #[tokio::test]
    async fn the_classifier_asks_a_reasoning_model_for_its_lowest_effort() {
        let turn = sse(&[
            json!({"type": "response.created", "response": {"id": "resp_1", "status": "in_progress", "output": []}}),
            json!({"type": "response.output_item.added", "output_index": 0,
                   "item": {"id": "rs_1", "type": "reasoning", "summary": []}}),
            json!({"type": "response.output_item.done", "output_index": 0,
                   "item": {"id": "rs_1", "type": "reasoning", "summary": [], "encrypted_content": "gAAAAB"}}),
            json!({"type": "response.output_item.added", "output_index": 1,
                   "item": {"id": "msg_1", "type": "message", "status": "in_progress", "role": "assistant", "content": []}}),
            json!({"type": "response.output_text.delta", "item_id": "msg_1", "output_index": 1, "content_index": 0,
                   "delta": "low", "logprobs": []}),
            json!({"type": "response.output_item.done", "output_index": 1,
                   "item": {"id": "msg_1", "type": "message", "status": "completed", "role": "assistant",
                            "content": [{"type": "output_text", "text": "low", "annotations": []}]}}),
            completed(usage(120, 0, 70, 64)),
        ]);
        let (url, req) = serve(turn).await;
        let backend = crate::api::ApiBackend::OpenAiCompat(responses_client(url));
        let (label, usage) = crate::router::classify(
            &backend,
            "oai:gpt-5-mini",
            "what does this function do?",
            std::time::Duration::from_secs(10),
            crate::api::OpenAiApi::Auto,
        )
        .await;
        assert_eq!(label, Ok(crate::router::Complexity::Low));
        assert!(usage.is_some(), "billed");
        let (_, body) = req.await.unwrap();
        assert_eq!(body["reasoning"]["effort"], "minimal", "{body}");
        assert!(body["max_output_tokens"].as_u64().unwrap() >= 512, "{body}");
    }

    /// OpenAI refuses reasoning summaries to organizations that are not
    /// verified. The request goes again without them, later requests leave
    /// them out, and the user is told once.
    #[tokio::test]
    async fn refused_summaries_are_dropped_for_the_session() {
        let refusal = json!({"error": {
            "message": "Your organization must be verified to generate reasoning summaries. \
                        Please go to: https://platform.openai.com/settings/organization/general \
                        and click on Verify Organization.",
            "type": "invalid_request_error", "param": "reasoning.summary", "code": "unsupported_value"}})
        .to_string();
        let (url, reqs) = serve_seq(vec![
            ("400 Bad Request", "application/json", refusal),
            ("200 OK", "text/event-stream", text_turn()),
            ("200 OK", "text/event-stream", text_turn()),
        ])
        .await;
        let c = responses_client(url);
        let summarized = || {
            let mut r = request("oai:gpt-5");
            r.output_config = Some(OutputConfig {
                effort: "low".into(),
            });
            r.thinking = Some(ThinkingConfig::Adaptive { summarized: true });
            r
        };
        let backend = crate::api::ApiBackend::OpenAiCompat(c.clone());
        let r = backend.messages_stream(summarized(), |_| {}).await.unwrap();
        assert_eq!(r.stop_reason, Some(StopReason::EndTurn));
        assert!(backend.take_summary_notice());
        assert!(!backend.take_summary_notice(), "told once");
        c.clone()
            .messages_stream(summarized(), |_| {})
            .await
            .unwrap();

        let bodies: Vec<_> = reqs.await.unwrap().into_iter().map(|(_, b)| b).collect();
        assert_eq!(
            bodies[0]["reasoning"],
            json!({"effort": "low", "summary": "auto"})
        );
        assert_eq!(bodies[1]["reasoning"], json!({"effort": "low"}));
        assert_eq!(bodies[2]["reasoning"], json!({"effort": "low"}));
    }

    /// A reasoning model that thinks silently past the 120 s stall bound
    /// was cut off and re-sent (the stall error names the connection), each
    /// attempt billed and none counted. Its bound is longer, and a stall
    /// mid-reasoning is reported as such, not as a dropped connection.
    #[tokio::test(start_paused = true)]
    async fn silent_reasoning_gets_a_longer_bound_and_is_not_retried() {
        use futures_util::StreamExt as _;
        let started = format!(
            "data: {}\n\n",
            json!({"type": "response.output_item.added", "output_index": 0,
                   "item": {"id": "rs_1", "type": "reasoning", "summary": []}})
        );
        let silent_for = |quiet: std::time::Duration| {
            let first = started.clone();
            futures_util::stream::iter([Ok::<_, std::convert::Infallible>(first.into_bytes())])
                .chain(futures_util::stream::once(async move {
                    tokio::time::sleep(quiet).await;
                    Ok(text_turn().into_bytes())
                }))
        };

        // Five silent minutes are fine for a reasoning model.
        let quiet = std::time::Duration::from_secs(300);
        let (r, _) = parse_responses_bytes(silent_for(quiet), REASONING_IDLE_TIMEOUT, |_| {})
            .await
            .unwrap();
        assert_eq!(r.stop_reason, Some(StopReason::EndTurn));

        // Past the bound: not a "connection" error, which the TUI re-sends.
        let quiet = REASONING_IDLE_TIMEOUT + std::time::Duration::from_secs(1);
        let err = parse_responses_bytes(silent_for(quiet), REASONING_IDLE_TIMEOUT, |_| {})
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("reasoning produced no output"), "{err}");
        assert!(!err.contains("connection"), "{err}");

        // A stall before any reasoning still reads as a dropped connection.
        let err = parse_responses_bytes(
            futures_util::stream::pending::<Result<Vec<u8>, std::convert::Infallible>>(),
            crate::api::SSE_IDLE_TIMEOUT,
            |_| {},
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("connection"), "{err}");
    }

    /// The text-only fallback for a model without tool support existed only
    /// on Chat Completions, so over the Responses API every turn failed.
    #[tokio::test]
    async fn a_model_without_tools_falls_back_to_text_only() {
        let refusal =
            json!({"error": {"message": "registry.ollama.ai/library/gemma:2b does not support tools"}})
                .to_string();
        let (url, reqs) = serve_seq(vec![
            ("400 Bad Request", "application/json", refusal),
            ("200 OK", "text/event-stream", text_turn()),
            ("200 OK", "text/event-stream", text_turn()),
        ])
        .await;
        let c = responses_client(url);
        let with_tools = || {
            let mut r = request("oai:gemma:2b");
            r.system = crate::api::types::SystemContent::Plain("Be terse.".into());
            r.tools = vec![ToolDefinition {
                name: "Read".into(),
                description: "read a file".into(),
                input_schema: json!({"type": "object", "properties": {}}),
                cache_control: None,
            }];
            r
        };
        let r = c.messages_stream(with_tools(), |_| {}).await.unwrap();
        assert_eq!(r.stop_reason, Some(StopReason::EndTurn));
        assert!(c.take_tools_notice());
        c.messages_stream(with_tools(), |_| {}).await.unwrap();

        let bodies: Vec<_> = reqs.await.unwrap().into_iter().map(|(_, b)| b).collect();
        assert_eq!(bodies[0]["tools"].as_array().map(Vec::len), Some(1));
        for b in &bodies[1..] {
            assert!(b.get("tools").is_none(), "{b}");
            let instructions = b["instructions"].as_str().unwrap();
            assert!(instructions.starts_with("Be terse."), "{instructions}");
            assert_ne!(instructions, "Be terse.", "not patched for text-only");
        }
    }

    /// Any other 400 is the user's error to see, not a reason to retry.
    #[tokio::test]
    async fn other_bad_requests_are_not_retried_without_summaries() {
        let (url, reqs) = serve_seq(vec![(
            "400 Bad Request",
            "application/json",
            json!({"error": {"message": "Invalid value for 'reasoning.effort'", "code": "invalid_value"}})
                .to_string(),
        )])
        .await;
        let c = responses_client(url);
        let mut r = request("oai:gpt-5");
        r.output_config = Some(OutputConfig {
            effort: "low".into(),
        });
        r.thinking = Some(ThinkingConfig::Adaptive { summarized: true });
        let err = c.messages_stream(r, |_| {}).await.unwrap_err().to_string();
        assert!(err.contains("Invalid value"), "{err}");
        assert!(!c.take_summary_notice());
        assert_eq!(reqs.await.unwrap().len(), 1);
    }
}
