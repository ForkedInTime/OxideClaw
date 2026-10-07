//! Claude Code transcripts as OxideClaw sessions
//! (`oxideclaw config import-claude --sessions`).
//!
//! Claude Code keeps one JSONL file per session in
//! `~/.claude/projects/<project>/<session uuid>.jsonl`, where `<project>` is
//! the working directory with every character but an ASCII letter or digit
//! turned into `-`. Each line is a record. `user` and `assistant` records
//! carry an API `message` (one assistant reply is spread over one record per
//! content block); the rest is bookkeeping: titles (`custom-title`,
//! `ai-title`, `summary`), `system` notes, attachments, queue state. Records
//! flagged `isSidechain` (subagents), `isMeta` (injected context),
//! `isCompactSummary`, `isVisibleInTranscriptOnly` or `isApiErrorMessage`,
//! and assistant records from the `<synthetic>` model, are not part of the
//! conversation and are skipped. Subagent transcripts in subdirectories and
//! `agent-*.jsonl` files are not sessions and are not read. Messages keep
//! the file's order, except that tool results are paired with their calls
//! by id ([`tidy`]).
//!
//! `~/.claude` is only read.

use super::{Session, SessionMeta, unix_now};
use crate::api::types::{ContentBlock, Message, Role, ToolResultContent};
use anyhow::{Context, Result};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::io::Read;
use std::path::{Path, PathBuf};

/// Transcripts larger than this are skipped with a warning, not read.
const MAX_TRANSCRIPT_BYTES: u64 = 50 * 1024 * 1024;

/// Claude Code cuts project directory names at this length and appends a hash.
const MAX_PROJECT_NAME: usize = 200;

/// One Claude Code session, converted.
struct Transcript {
    /// Claude Code's session id (the file name).
    id: String,
    /// The working directory of the first record that names one.
    cwd: Option<String>,
    /// The session's title: the last `custom-title`, `ai-title` or
    /// `summary` record, in that order of preference.
    title: Option<String>,
    /// The first prompt the user typed, on one line, at most 60 characters.
    first_prompt: String,
    /// The model of the last assistant reply.
    model: Option<String>,
    /// RFC 3339 timestamp of the first record that has one.
    started: Option<String>,
    /// Unix seconds of the earliest and latest record timestamps.
    created_at: Option<u64>,
    updated_at: Option<u64>,
    messages: Vec<Message>,
    skipped: Skipped,
}

/// What a conversion left out, for the import summary.
#[derive(Default, Debug, PartialEq)]
struct Skipped {
    /// Lines that are not a JSON object.
    malformed_lines: usize,
    /// Content blocks OxideClaw has no type for, by block type.
    blocks: BTreeMap<String, usize>,
    /// Tool calls without a result, results without a call, and repeats of
    /// either ([`tidy`]).
    orphaned_tool_blocks: usize,
}

impl Skipped {
    fn describe(&self) -> Option<String> {
        let mut parts = Vec::new();
        if self.malformed_lines > 0 {
            parts.push(format!("{} malformed line(s)", self.malformed_lines));
        }
        if !self.blocks.is_empty() {
            let kinds: Vec<String> = self
                .blocks
                .iter()
                .map(|(kind, n)| format!("{kind} x{n}"))
                .collect();
            parts.push(format!(
                "unsupported block(s) dropped: {}",
                kinds.join(", ")
            ));
        }
        if self.orphaned_tool_blocks > 0 {
            parts.push(format!(
                "{} unpaired tool call/result block(s) dropped",
                self.orphaned_tool_blocks
            ));
        }
        (!parts.is_empty()).then(|| parts.join("; "))
    }
}

/// Claude Code's directory name for the project at `cwd`: every character
/// but an ASCII letter or digit becomes `-`, one per UTF-16 unit as
/// JavaScript counts them. Names over [`MAX_PROJECT_NAME`] are cut there and
/// get a hash suffix, which [`project_dirs`] matches by prefix.
fn project_dir_name(cwd: &Path) -> String {
    cwd.to_string_lossy()
        .chars()
        .flat_map(|c| {
            let n = if c.is_ascii_alphanumeric() {
                0
            } else {
                c.len_utf16()
            };
            std::iter::repeat_n('-', n).chain((n == 0).then_some(c))
        })
        .collect()
}

/// The directories under `claude/projects` that may hold `cwd`'s
/// transcripts. Two paths can share a name (`/a-b`, `/a/b`), so the
/// transcripts' own `cwd` decides in [`read_project`].
fn project_dirs(claude: &Path, cwd: &Path) -> Vec<PathBuf> {
    let projects = claude.join("projects");
    let name = project_dir_name(cwd);
    if name.len() <= MAX_PROJECT_NAME {
        return vec![projects.join(name)];
    }
    // The name is ASCII, so cutting it by bytes is safe.
    let prefix = format!("{}-", &name[..MAX_PROJECT_NAME]);
    let Ok(entries) = std::fs::read_dir(&projects) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with(&prefix))
        .map(|e| e.path())
        .collect()
}

/// Every Claude Code session recorded for `cwd` that has a conversation,
/// oldest first, plus warnings for the transcripts that could not be read.
fn read_project(claude: &Path, cwd: &Path) -> (Vec<Transcript>, Vec<String>) {
    let mut found = Vec::new();
    let mut warnings = Vec::new();
    let cwd_text = cwd.to_string_lossy();
    for dir in project_dirs(claude, cwd) {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            // Regular files only: a symlink must not pull in a file from
            // elsewhere. Session files are named by their UUID.
            if !entry.file_type().is_ok_and(|t| t.is_file()) {
                continue;
            }
            let path = entry.path();
            let Some(id) = path
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| n.strip_suffix(".jsonl"))
                .filter(|id| uuid::Uuid::parse_str(id).is_ok())
            else {
                continue;
            };
            match read_transcript(&path, id) {
                Err(e) => warnings.push(format!("skipped {}: {e:#}", path.display())),
                Ok(t) if t.messages.is_empty() => {}
                Ok(t) if t.cwd.as_deref().is_some_and(|c| c != cwd_text) => {}
                Ok(t) => found.push(t),
            }
        }
    }
    found.sort_by(|a, b| (a.created_at, &a.id).cmp(&(b.created_at, &b.id)));
    (found, warnings)
}

fn read_transcript(path: &Path, id: &str) -> Result<Transcript> {
    let file = std::fs::File::open(path)?;
    let too_big = || anyhow::anyhow!("larger than {} MiB", MAX_TRANSCRIPT_BYTES / (1024 * 1024));
    if file.metadata()?.len() > MAX_TRANSCRIPT_BYTES {
        return Err(too_big());
    }
    // Bounded even if the file grows while it is read.
    let mut bytes = Vec::new();
    file.take(MAX_TRANSCRIPT_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_TRANSCRIPT_BYTES {
        return Err(too_big());
    }
    Ok(parse_transcript(id, &bytes))
}

/// Convert one transcript's bytes. Never fails: a line that is not a JSON
/// object is counted and skipped, an unknown block is counted and dropped.
fn parse_transcript(id: &str, bytes: &[u8]) -> Transcript {
    let mut t = Transcript {
        id: id.to_string(),
        cwd: None,
        title: None,
        first_prompt: String::new(),
        model: None,
        started: None,
        created_at: None,
        updated_at: None,
        messages: Vec::new(),
        skipped: Skipped::default(),
    };
    let (mut custom_title, mut ai_title, mut summary) = (None, None, None);
    for line in bytes.split(|b| *b == b'\n') {
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let Ok(Value::Object(r)) = serde_json::from_slice::<Value>(line) else {
            t.skipped.malformed_lines += 1;
            continue;
        };
        let text = |key: &str| r.get(key).and_then(Value::as_str);
        let flag = |key: &str| r.get(key).and_then(Value::as_bool) == Some(true);
        if let Some(ts) = text("timestamp")
            && let Some(secs) = parse_timestamp(ts)
        {
            if t.started.is_none() {
                t.started = Some(ts.to_string());
            }
            t.created_at = Some(t.created_at.map_or(secs, |c| c.min(secs)));
            t.updated_at = Some(t.updated_at.map_or(secs, |u| u.max(secs)));
        }
        if t.cwd.is_none() {
            t.cwd = text("cwd").map(str::to_string);
        }
        let role = match text("type") {
            Some("custom-title") => {
                custom_title = text("customTitle").map(str::to_string).or(custom_title);
                continue;
            }
            Some("ai-title") => {
                ai_title = text("aiTitle").map(str::to_string).or(ai_title);
                continue;
            }
            Some("summary") => {
                summary = text("summary").map(str::to_string).or(summary);
                continue;
            }
            Some("user") => Role::User,
            Some("assistant") => Role::Assistant,
            _ => continue,
        };
        if flag("isSidechain")
            || flag("isMeta")
            || flag("isCompactSummary")
            || flag("isVisibleInTranscriptOnly")
            || flag("isApiErrorMessage")
        {
            continue;
        }
        let message = r.get("message").unwrap_or(&Value::Null);
        if role == Role::Assistant {
            match message.get("model").and_then(Value::as_str) {
                // Claude Code's own stand-in replies; no model wrote them.
                Some("<synthetic>") => continue,
                Some(m) => t.model = Some(m.to_string()),
                None => {}
            }
        }
        let content = convert_content(&message["content"], &mut t.skipped);
        if role == Role::User && t.first_prompt.is_empty() {
            let typed = content.iter().find_map(|b| match b {
                ContentBlock::Text { text } if !is_command_echo(text) => Some(text),
                _ => None,
            });
            if let Some(text) = typed {
                t.first_prompt = one_line(text, 60);
            }
        }
        if !content.is_empty() {
            t.messages.push(Message { role, content });
        }
    }
    t.title = custom_title
        .or(ai_title)
        .or(summary)
        .map(|s| one_line(&s, 80))
        .filter(|s| !s.is_empty());
    t.skipped.orphaned_tool_blocks = tidy(&mut t.messages);
    t
}

/// Slash-command bookkeeping Claude Code records as user text: not what
/// the user typed as a prompt, so never a title or preview.
fn is_command_echo(text: &str) -> bool {
    [
        "<command-name>",
        "<command-message>",
        "<local-command-stdout>",
        "<local-command-stderr>",
        "<local-command-caveat>",
    ]
    .iter()
    .any(|tag| text.trim_start().starts_with(tag))
}

fn convert_content(content: &Value, skipped: &mut Skipped) -> Vec<ContentBlock> {
    match content {
        Value::String(s) => text_block(s).into_iter().collect(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|b| convert_block(b, skipped))
            .collect(),
        _ => Vec::new(),
    }
}

/// Empty text blocks are rejected by the API, so they are left out.
fn text_block(text: &str) -> Option<ContentBlock> {
    (!text.trim().is_empty()).then(|| ContentBlock::Text {
        text: text.to_string(),
    })
}

fn convert_block(block: &Value, skipped: &mut Skipped) -> Option<ContentBlock> {
    let kind = block.get("type").and_then(Value::as_str).unwrap_or("");
    let str_field = |key: &str| {
        block
            .get(key)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
    };
    let converted = match kind {
        "text" => {
            return block
                .get("text")
                .and_then(Value::as_str)
                .and_then(text_block);
        }
        // Reasoning is signed for the request it came from; it is not
        // replayed (OxideClaw's own resumed sessions do the same per turn).
        "thinking" | "redacted_thinking" => return None,
        "tool_use" => match (str_field("id"), str_field("name")) {
            (Some(id), Some(name)) => Some(ContentBlock::ToolUse {
                id: id.to_string(),
                name: name.to_string(),
                input: block
                    .get("input")
                    .filter(|i| i.is_object())
                    .cloned()
                    .unwrap_or_else(|| serde_json::json!({})),
            }),
            _ => None,
        },
        "tool_result" => str_field("tool_use_id").map(|id| ContentBlock::ToolResult {
            tool_use_id: id.to_string(),
            content: convert_result(block.get("content").unwrap_or(&Value::Null), skipped),
            is_error: block.get("is_error").and_then(Value::as_bool),
        }),
        "image" => serde_json::from_value::<ContentBlock>(block.clone())
            .ok()
            .filter(|b| matches!(b, ContentBlock::Image { .. })),
        _ => None,
    };
    if converted.is_none() {
        count_block(skipped, kind);
    }
    converted
}

/// A tool result's content: a string, or a list whose text parts are kept.
/// One that ends up empty gets a placeholder, since the API rejects empty
/// text.
fn convert_result(content: &Value, skipped: &mut Skipped) -> Vec<ToolResultContent> {
    let mut out = Vec::new();
    match content {
        Value::String(s) if !s.is_empty() => out.push(ToolResultContent::text(s.as_str())),
        Value::Array(items) => {
            for item in items {
                let kind = item
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or("untyped");
                match item.get("text").and_then(Value::as_str) {
                    Some(text) if kind == "text" => {
                        if !text.is_empty() {
                            out.push(ToolResultContent::text(text));
                        }
                    }
                    _ => count_block(skipped, &format!("{kind} in a tool result")),
                }
            }
        }
        _ => {}
    }
    if out.is_empty() {
        out.push(ToolResultContent::text("(no text output)"));
    }
    out
}

/// Count a dropped block under its type, which comes from the file: only a
/// plain name is printed as is.
fn count_block(skipped: &mut Skipped, kind: &str) {
    let plain = !kind.is_empty()
        && kind.len() <= 48
        && kind
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == ' ');
    let kind = if plain { kind } else { "unrecognized" };
    *skipped.blocks.entry(kind.to_string()).or_default() += 1;
}

/// Make the conversation one the API accepts. Claude Code writes a reply
/// block by block and does not always write a tool's result after its call
/// (the records of parallel calls, or of a turn flushed late, land out of
/// order), so results are matched to calls by id, not by position: each
/// `tool_result` moves to the start of the user message right after the
/// assistant message holding its `tool_use`, and consecutive messages of one
/// role merge. A call without a result, a result without a call, and a
/// repeat of either are dropped; returns how many.
fn tidy(messages: &mut Vec<Message>) -> usize {
    let mut dropped = 0;
    let mut results: HashMap<String, ContentBlock> = HashMap::new();
    for m in messages.iter_mut() {
        for block in std::mem::take(&mut m.content) {
            match block {
                ContentBlock::ToolResult {
                    ref tool_use_id, ..
                } => {
                    if results.contains_key(tool_use_id) {
                        dropped += 1;
                    } else {
                        results.insert(tool_use_id.clone(), block);
                    }
                }
                other => m.content.push(other),
            }
        }
    }
    // A message emptied here still separates the turns around it.
    merge_roles(messages);

    let mut out: Vec<Message> = Vec::with_capacity(messages.len() + 1);
    // The results of the assistant message just emitted.
    let mut answers: Vec<ContentBlock> = Vec::new();
    for mut m in messages.drain(..) {
        if m.role == Role::Assistant {
            if !answers.is_empty() {
                out.push(Message {
                    role: Role::User,
                    content: std::mem::take(&mut answers),
                });
            }
            m.content.retain(|b| match b {
                ContentBlock::ToolUse { id, .. } => match results.remove(id) {
                    Some(result) => {
                        answers.push(result);
                        true
                    }
                    None => {
                        dropped += 1;
                        false
                    }
                },
                _ => true,
            });
        } else {
            // Results first, as the API requires.
            answers.append(&mut m.content);
            m.content = std::mem::take(&mut answers);
        }
        out.push(m);
    }
    if !answers.is_empty() {
        out.push(Message {
            role: Role::User,
            content: answers,
        });
    }
    dropped += results.len();
    // A message emptied by a moved result or a dropped call can leave two
    // of one role together; merging them keeps every pair intact.
    out.retain(|m| !m.content.is_empty());
    merge_roles(&mut out);
    *messages = out;
    dropped
}

fn merge_roles(messages: &mut Vec<Message>) {
    let mut merged: Vec<Message> = Vec::with_capacity(messages.len());
    for m in messages.drain(..) {
        match merged.last_mut() {
            Some(last) if last.role == m.role => last.content.extend(m.content),
            _ => merged.push(m),
        }
    }
    *messages = merged;
}

/// Unix seconds of an RFC 3339 timestamp (`2026-10-05T21:04:26.149Z`,
/// `...+02:00`).
fn parse_timestamp(s: &str) -> Option<u64> {
    let num = |p: &str| -> Option<i64> {
        (!p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
            .then(|| p.parse().ok())
            .flatten()
    };
    let (date, time) = s.split_once(['T', 't', ' '])?;
    let mut d = date.split('-');
    let (y, mo, day) = (num(d.next()?)?, num(d.next()?)?, num(d.next()?)?);
    let split = time.find(['Z', 'z', '+', '-']).unwrap_or(time.len());
    let (clock, zone) = time.split_at(split);
    let clock = clock.split('.').next()?;
    let mut c = clock.split(':');
    let (h, mi, sec) = (num(c.next()?)?, num(c.next()?)?, num(c.next()?)?);
    if d.next().is_some()
        || c.next().is_some()
        || !(1..=12).contains(&mo)
        || !(1..=31).contains(&day)
        || h > 23
        || mi > 59
        || sec > 60
    {
        return None;
    }
    let offset = match zone {
        "" | "Z" | "z" => 0,
        z => {
            let (oh, om) = z[1..].split_once(':')?;
            let secs = num(oh)? * 3600 + num(om)? * 60;
            if z.starts_with('-') { -secs } else { secs }
        }
    };
    // Days from 1970-01-01 to y-mo-day (proleptic Gregorian).
    let y = if mo <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((mo + 9) % 12) + 2) / 5 + day - 1;
    let days = era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468;
    u64::try_from(days * 86_400 + h * 3600 + mi * 60 + sec - offset).ok()
}

/// `s` on one line with control characters (escape sequences included)
/// blanked, at most `max` characters: transcript text is printed to the
/// terminal.
fn one_line(s: &str, max: usize) -> String {
    let blanked: String = s
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let words: Vec<&str> = blanked.split_whitespace().collect();
    words.join(" ").chars().take(max).collect()
}

/// `path` is `root` or inside it, by name or after resolving symlinks in
/// the deepest part of each that exists.
fn is_within(path: &Path, root: &Path) -> bool {
    let resolve = |p: &Path| {
        p.ancestors().find_map(|a| {
            let rest = p.strip_prefix(a).ok()?;
            a.canonicalize().ok().map(|c| c.join(rest))
        })
    };
    path.starts_with(root)
        || matches!((resolve(path), resolve(root)), (Some(p), Some(r)) if p.starts_with(&r))
}

/// Claude Code session id → OxideClaw session id, for the sessions in `dir`
/// imported before. Reads the `.meta` files only and writes nothing.
fn imported_ids(dir: &Path) -> HashMap<String, String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return HashMap::new();
    };
    entries
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "meta"))
        .filter_map(|e| std::fs::read_to_string(e.path()).ok())
        .filter_map(|body| serde_json::from_str::<SessionMeta>(&body).ok())
        .filter_map(|m| m.claude_code_session.map(|cc| (cc, m.id)))
        .collect()
}

fn short(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

fn no_sessions(claude: &Path, cwd: &Path, warnings: Vec<String>) -> Vec<String> {
    let mut lines = vec![format!(
        "No Claude Code sessions for {} in {}.",
        cwd.display(),
        claude.join("projects").display()
    )];
    lines.extend(warnings);
    lines
}

/// `config import-claude --sessions --list`: this project's Claude Code
/// sessions, and which are imported. Changes nothing.
pub async fn list(claude: &Path, cwd: &Path, sessions_dir: &Path) -> Result<Vec<String>> {
    let (found, warnings) = read_project(claude, cwd);
    if found.is_empty() {
        return Ok(no_sessions(claude, cwd, warnings));
    }
    let done = imported_ids(sessions_dir);
    let mut lines = vec![format!(
        "Claude Code sessions for {} ({}):",
        cwd.display(),
        found.len()
    )];
    for t in &found {
        let date = t
            .started
            .as_deref()
            .and_then(|s| s.get(..16))
            .map_or_else(|| "?".repeat(16), |s| one_line(&s.replace('T', " "), 16));
        let imported = done
            .get(&t.id)
            .map(|ox| format!("  [imported: {}]", short(ox)))
            .unwrap_or_default();
        lines.push(format!(
            "  {}  {date}  {:>4} msgs  {}{imported}",
            t.id,
            t.messages.len(),
            t.first_prompt
        ));
    }
    lines.extend(warnings);
    lines.push(format!(
        "Import the ones not imported yet with `oxideclaw config import-claude --sessions`, \
         or one with `--sessions <id>`. {} is not changed.",
        claude.display()
    ));
    Ok(lines)
}

/// `config import-claude --sessions [<id>]`: copy this project's Claude Code
/// sessions that are not imported yet (or the one `only` names, by id or
/// unique id prefix) into `sessions_dir` as OxideClaw sessions.
pub async fn import(
    claude: &Path,
    cwd: &Path,
    sessions_dir: &Path,
    only: Option<&str>,
) -> Result<Vec<String>> {
    if is_within(sessions_dir, claude) {
        anyhow::bail!(
            "the sessions dir {} is inside Claude Code's {}, which is never written",
            sessions_dir.display(),
            claude.display()
        );
    }
    let (found, warnings) = read_project(claude, cwd);
    let chosen: Vec<&Transcript> = match only {
        None if found.is_empty() => return Ok(no_sessions(claude, cwd, warnings)),
        None => found.iter().collect(),
        Some(q) => {
            let q = q.trim();
            let hits: Vec<&Transcript> = found
                .iter()
                .filter(|t| t.id == q || (!q.is_empty() && t.id.starts_with(q)))
                .collect();
            match hits.as_slice() {
                [t] => vec![*t],
                [] => {
                    let mut msg = format!("no Claude Code session '{q}' for {}", cwd.display());
                    for w in &warnings {
                        msg.push_str(&format!("\n{w}"));
                    }
                    anyhow::bail!(
                        "{msg}\n`oxideclaw config import-claude --sessions --list` shows them"
                    )
                }
                _ => anyhow::bail!("'{q}' matches {} Claude Code sessions", hits.len()),
            }
        }
    };
    let done = imported_ids(sessions_dir);
    let mut lines = Vec::new();
    let (mut imported, mut already) = (0, 0);
    for t in chosen {
        if let Some(ox) = done.get(&t.id) {
            already += 1;
            if only.is_some() {
                lines.push(format!("{} is already imported as {ox}.", t.id));
            }
            continue;
        }
        let id = write_session(sessions_dir, t)
            .await
            .with_context(|| format!("importing Claude Code session {}", t.id))?;
        imported += 1;
        lines.push(format!(
            "  {} -> {id}  {} ({} messages)",
            short(&t.id),
            t.title.as_deref().unwrap_or(&t.first_prompt),
            t.messages.len()
        ));
        if let Some(note) = t.skipped.describe() {
            lines.push(format!("      {note}"));
        }
    }
    lines.extend(warnings);
    if imported > 0 {
        lines.insert(
            0,
            format!(
                "Imported {imported} Claude Code session(s) into {}:",
                sessions_dir.display()
            ),
        );
        lines.push("Resume one with `oxideclaw --resume <id>` or pick it in /session.".to_string());
    } else if only.is_none() {
        lines.insert(
            0,
            format!("Nothing to import: all {already} session(s) are imported already."),
        );
    }
    lines.push(format!("{} was not changed.", claude.display()));
    Ok(lines)
}

/// Save `t` as a new OxideClaw session in `dir` and return its id. The
/// transcript is written before the `.meta` that lists it and marks it
/// imported, and its modification time is set to the last activity, so the
/// session list and `--continue` see the session's real age.
async fn write_session(dir: &Path, t: &Transcript) -> Result<String> {
    let id = uuid::Uuid::new_v4().to_string();
    let created_at = t.created_at.unwrap_or_else(unix_now);
    let session = Session {
        id: id.clone(),
        meta: SessionMeta {
            id: id.clone(),
            name: t
                .title
                .clone()
                .or_else(|| (!t.first_prompt.is_empty()).then(|| t.first_prompt.clone()))
                .unwrap_or_else(|| "Claude Code session".to_string()),
            created_at,
            preview: t.first_prompt.clone(),
            tags: Vec::new(),
            auto_commits: Vec::new(),
            undo_position: 0,
            base_commit: None,
            timeline: Vec::new(),
            cwd: t.cwd.clone(),
            model: t.model.clone(),
            claude_code_session: Some(t.id.clone()),
            redo: Vec::new(),
        },
        dir: dir.to_path_buf(),
        path: Session::jsonl_path(dir, &id),
    };
    session.overwrite(&t.messages).await?;
    if let Some(updated) = t.updated_at {
        let at = std::time::UNIX_EPOCH + std::time::Duration::from_secs(updated);
        std::fs::File::options()
            .write(true)
            .open(&session.path)
            .and_then(|f| f.set_modified(at))?;
    }
    session.save_meta().await?;
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROJECT: &str = "/work/my_app";
    const S1: &str = "11111111-1111-4111-8111-111111111111";
    const S2: &str = "22222222-2222-4222-8222-222222222222";

    fn write(path: &Path, body: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    /// One record per line, in Claude Code's shape.
    fn record(kind: &str, ts: &str, extra: Value) -> String {
        let mut r = serde_json::json!({
            "type": kind,
            "uuid": uuid::Uuid::new_v4().to_string(),
            "sessionId": S1,
            "cwd": PROJECT,
            "timestamp": ts,
            "isSidechain": false,
        });
        r.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        r.to_string()
    }

    fn user(ts: &str, content: Value) -> String {
        record(
            "user",
            ts,
            serde_json::json!({"message": {"role": "user", "content": content}}),
        )
    }

    fn assistant(ts: &str, content: Value) -> String {
        record(
            "assistant",
            ts,
            serde_json::json!({"message": {
                "id": "msg_1", "type": "message", "role": "assistant",
                "model": "claude-test-1", "content": content
            }}),
        )
    }

    /// A synthetic Claude Code transcript covering text, a tool call and
    /// its result, an orphaned tool call, a sidechain record, meta and
    /// compact-summary records, a block OxideClaw has no type for and a
    /// malformed line.
    fn fixture() -> String {
        let lines = [
            r#"{"type":"queue-operation","operation":"enqueue","timestamp":"2026-03-01T09:59:59.000Z","sessionId":"x"}"#.to_string(),
            record("user", "2026-03-01T10:00:00.000Z", serde_json::json!({
                "isMeta": true,
                "message": {"role": "user", "content": "<local-command-caveat>injected</local-command-caveat>"}
            })),
            user("2026-03-01T10:00:01.000Z", "Fix the \u{1b}[31mlogin\u{1b}[0m bug in auth.rs".into()),
            assistant("2026-03-01T10:00:02.000Z", serde_json::json!([
                {"type": "thinking", "thinking": "hmm", "signature": "sig"}
            ])),
            assistant("2026-03-01T10:00:03.000Z", serde_json::json!([
                {"type": "text", "text": "Let me look."}
            ])),
            assistant("2026-03-01T10:00:04.000Z", serde_json::json!([
                {"type": "tool_use", "id": "toolu_read", "name": "Read", "input": {"file_path": "/work/my_app/auth.rs"}}
            ])),
            // A subagent's work, interleaved: not part of this conversation.
            record("assistant", "2026-03-01T10:00:04.500Z", serde_json::json!({
                "isSidechain": true,
                "message": {"role": "assistant", "model": "claude-test-1", "content": [
                    {"type": "text", "text": "SIDECHAIN"}
                ]}
            })),
            user("2026-03-01T10:00:05.000Z", serde_json::json!([
                {"type": "tool_result", "tool_use_id": "toolu_read", "content": [
                    {"type": "text", "text": "fn login() {}"},
                    {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAAA"}}
                ]}
            ])),
            "{\"type\": \"user\", \"message\": {\"role\": \"user\", \"content\": \"torn".to_string(),
            assistant("2026-03-01T10:00:06.000Z", serde_json::json!([
                {"type": "text", "text": "Found it. Editing."},
                {"type": "server_tool_use", "id": "srv_1", "name": "web_search", "input": {}},
                {"type": "tool_use", "id": "toolu_edit", "name": "Edit", "input": {"file_path": "auth.rs"}}
            ])),
            // The run was cut off: toolu_edit never got a result.
            user("2026-03-01T10:05:00.000Z", "never mind, thanks".into()),
            record("user", "2026-03-01T10:06:00.000Z", serde_json::json!({
                "isCompactSummary": true, "isVisibleInTranscriptOnly": true,
                "message": {"role": "user", "content": "This session is being continued..."}
            })),
            record("assistant", "2026-03-01T10:06:30.000Z", serde_json::json!({
                "isApiErrorMessage": true,
                "message": {"role": "assistant", "model": "<synthetic>", "content": [
                    {"type": "text", "text": "API Error"}
                ]}
            })),
            assistant("2026-03-01T10:07:00.000Z", serde_json::json!([
                {"type": "text", "text": "You're welcome."}
            ])),
            r#"{"type":"ai-title","aiTitle":"Login bug fix","sessionId":"x"}"#.to_string(),
            r#"["not", "a", "record"]"#.to_string(),
        ];
        lines.join("\n") + "\n"
    }

    fn claude_home(root: &Path) -> PathBuf {
        let claude = root.join("home/.claude");
        let project = claude
            .join("projects")
            .join(project_dir_name(Path::new(PROJECT)));
        write(&project.join(format!("{S1}.jsonl")), &fixture());
        claude
    }

    fn snapshot(dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
        walkdir::WalkDir::new(dir)
            .into_iter()
            .flatten()
            .filter(|e| e.file_type().is_file())
            .map(|e| (e.path().to_path_buf(), std::fs::read(e.path()).unwrap()))
            .collect()
    }

    fn text(m: &Message) -> Vec<&str> {
        m.content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn project_dir_names_match_claude_codes_encoding() {
        assert_eq!(
            project_dir_name(Path::new("/home/user/OxideClaw")),
            "-home-user-OxideClaw"
        );
        assert_eq!(
            project_dir_name(Path::new("/work/my_app.v2")),
            "-work-my-app-v2"
        );
        // One dash per UTF-16 unit: an astral character takes two.
        assert_eq!(project_dir_name(Path::new("/tmp/é😀")), "-tmp----");
    }

    #[test]
    fn long_project_names_match_by_their_prefix() {
        let td = tempfile::tempdir().unwrap();
        let cwd = PathBuf::from(format!("/{}", "a".repeat(250)));
        let name = project_dir_name(&cwd);
        let hashed = format!("{}-1x2y3z", &name[..MAX_PROJECT_NAME]);
        std::fs::create_dir_all(td.path().join("projects").join(&hashed)).unwrap();
        std::fs::create_dir_all(td.path().join("projects/-other")).unwrap();
        assert_eq!(
            project_dirs(td.path(), &cwd),
            vec![td.path().join("projects").join(hashed)]
        );
    }

    #[test]
    fn timestamps_parse_as_utc_seconds() {
        assert_eq!(parse_timestamp("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            parse_timestamp("2026-10-05T21:04:26.149Z"),
            Some(1_791_234_266)
        );
        assert_eq!(parse_timestamp("2024-02-29T23:59:59Z"), Some(1_709_251_199));
        assert_eq!(
            parse_timestamp("2026-10-05T23:04:26+02:00"),
            Some(1_791_234_266)
        );
        for bad in [
            "",
            "yesterday",
            "2026-13-01T00:00:00Z",
            "2026-01-01T25:00:00Z",
            "2026-01-01",
        ] {
            assert_eq!(parse_timestamp(bad), None, "{bad}");
        }
    }

    #[test]
    fn a_transcript_converts_into_a_coherent_conversation() {
        let t = parse_transcript(S1, fixture().as_bytes());
        assert_eq!(t.cwd.as_deref(), Some(PROJECT));
        assert_eq!(t.model.as_deref(), Some("claude-test-1"));
        assert_eq!(t.title.as_deref(), Some("Login bug fix"));
        // Escape sequences are blanked: this text is printed to a terminal.
        assert_eq!(t.first_prompt, "Fix the [31mlogin [0m bug in auth.rs");
        assert_eq!(t.started.as_deref(), Some("2026-03-01T09:59:59.000Z"));
        assert_eq!(t.created_at, parse_timestamp("2026-03-01T09:59:59Z"));
        assert_eq!(t.updated_at, parse_timestamp("2026-03-01T10:07:00Z"));

        let roles: Vec<Role> = t.messages.iter().map(|m| m.role.clone()).collect();
        assert_eq!(
            roles,
            [
                Role::User,
                Role::Assistant,
                Role::User,
                Role::Assistant,
                Role::User,
                Role::Assistant
            ],
            "one message per turn, alternating"
        );
        // The reply that was split over records is one message, the
        // thinking block is not replayed, and the call is kept with its result.
        assert_eq!(t.messages[1].content.len(), 2);
        assert_eq!(text(&t.messages[1]), ["Let me look."]);
        assert!(matches!(
            &t.messages[1].content[1],
            ContentBlock::ToolUse { id, name, .. } if id == "toolu_read" && name == "Read"
        ));
        match &t.messages[2].content[..] {
            [
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    ..
                },
            ] => {
                assert_eq!(tool_use_id, "toolu_read");
                assert_eq!(content, &vec![ToolResultContent::text("fn login() {}")]);
            }
            other => panic!("{other:?}"),
        }
        // The edit that never got a result is dropped; its text stays.
        assert_eq!(text(&t.messages[3]), ["Found it. Editing."]);
        assert_eq!(t.messages[3].content.len(), 1);
        assert_eq!(text(&t.messages[4]), ["never mind, thanks"]);
        assert_eq!(text(&t.messages[5]), ["You're welcome."]);

        let all: Vec<&str> = t.messages.iter().flat_map(text).collect();
        for skipped in ["SIDECHAIN", "injected", "continued", "API Error", "torn"] {
            assert!(
                !all.iter().any(|s| s.contains(skipped)),
                "{skipped} leaked: {all:?}"
            );
        }
        assert_eq!(
            t.skipped,
            Skipped {
                malformed_lines: 2,
                blocks: BTreeMap::from([
                    ("image in a tool result".to_string(), 1),
                    ("server_tool_use".to_string(), 1),
                ]),
                orphaned_tool_blocks: 1,
            }
        );
    }

    #[test]
    fn orphaned_tool_results_are_dropped_and_results_lead_their_message() {
        let mut messages = vec![
            Message {
                role: Role::User,
                content: vec![
                    ContentBlock::Text { text: "hi".into() },
                    ContentBlock::ToolResult {
                        tool_use_id: "nobody".into(),
                        content: vec![ToolResultContent::text("x")],
                        is_error: None,
                    },
                ],
            },
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: "a".into(),
                    name: "Bash".into(),
                    input: serde_json::json!({}),
                }],
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::Text {
                    text: "[Request interrupted]".into(),
                }],
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "a".into(),
                    content: vec![ToolResultContent::text("ok")],
                    is_error: None,
                }],
            },
        ];
        assert_eq!(tidy(&mut messages), 1);
        assert_eq!(messages.len(), 3);
        assert_eq!(text(&messages[0]), ["hi"]);
        assert!(matches!(
            messages[2].content[..],
            [ContentBlock::ToolResult { .. }, ContentBlock::Text { .. }]
        ));
    }

    /// Results are matched to calls by id: one written before its call, and
    /// one of parallel calls written after the next prompt, still pair up,
    /// in call order, ahead of the user's text, and turns stay apart.
    #[test]
    fn results_pair_with_their_calls_wherever_the_file_put_them() {
        let use_ = |id: &str| ContentBlock::ToolUse {
            id: id.into(),
            name: "Bash".into(),
            input: serde_json::json!({}),
        };
        let result = |id: &str| ContentBlock::ToolResult {
            tool_use_id: id.into(),
            content: vec![ToolResultContent::text(id)],
            is_error: None,
        };
        let msg = |role: Role, content: Vec<ContentBlock>| Message { role, content };
        let mut messages = vec![
            msg(Role::User, vec![ContentBlock::Text { text: "go".into() }]),
            msg(Role::User, vec![result("b")]),
            msg(Role::Assistant, vec![use_("a"), use_("p")]),
            msg(Role::User, vec![result("a")]),
            msg(Role::Assistant, vec![use_("b")]),
            msg(
                Role::User,
                vec![ContentBlock::Text {
                    text: "next".into(),
                }],
            ),
            msg(Role::User, vec![result("p"), result("p")]),
        ];
        assert_eq!(tidy(&mut messages), 1, "the repeated result");
        assert_eq!(
            messages,
            vec![
                msg(Role::User, vec![ContentBlock::Text { text: "go".into() }]),
                msg(Role::Assistant, vec![use_("a"), use_("p")]),
                msg(Role::User, vec![result("a"), result("p")]),
                msg(Role::Assistant, vec![use_("b")]),
                msg(
                    Role::User,
                    vec![
                        result("b"),
                        ContentBlock::Text {
                            text: "next".into()
                        }
                    ]
                ),
            ]
        );
    }

    /// The whole path: list, import, resume, send. A re-run imports
    /// nothing twice, and ~/.claude stays byte-identical.
    #[tokio::test]
    async fn sessions_import_once_and_resume_as_valid_history() {
        let td = tempfile::tempdir().unwrap();
        let claude = claude_home(td.path());
        let project = claude
            .join("projects")
            .join(project_dir_name(Path::new(PROJECT)));
        // A second session; an empty one (meta records only); a subagent
        // file; and a session of another project sharing the dir name.
        write(
            &project.join(format!("{S2}.jsonl")),
            &[
                user("2026-04-01T08:00:00Z", "second session".into()),
                assistant(
                    "2026-04-01T08:00:05Z",
                    serde_json::json!([{"type": "text", "text": "ok"}]),
                ),
            ]
            .join("\n"),
        );
        write(
            &project.join("33333333-3333-4333-8333-333333333333.jsonl"),
            r#"{"type":"summary","summary":"nothing said"}"#,
        );
        write(&project.join("agent-1234.jsonl"), &fixture());
        write(
            &project.join("44444444-4444-4444-8444-444444444444.jsonl"),
            &user("2026-05-01T08:00:00Z", "elsewhere".into()).replace(PROJECT, "/work/my/app"),
        );
        let sessions = td.path().join("data/sessions");
        let cwd = Path::new(PROJECT);
        let before = snapshot(&claude);

        let listed = list(&claude, cwd, &sessions).await.unwrap().join("\n");
        assert!(listed.contains("(2)"), "{listed}");
        assert!(
            listed.contains(&format!("{S1}  2026-03-01 09:59     6 msgs  Fix the")),
            "{listed}"
        );
        assert!(
            listed.contains(S2) && listed.contains("second session"),
            "{listed}"
        );
        assert!(
            !listed.contains("elsewhere") && !listed.contains("imported:"),
            "{listed}"
        );
        assert!(!sessions.exists(), "listing writes nothing");

        let out = import(&claude, cwd, &sessions, None)
            .await
            .unwrap()
            .join("\n");
        assert!(out.contains("Imported 2 Claude Code session(s)"), "{out}");
        assert!(out.contains("Login bug fix (6 messages)"), "{out}");
        assert!(
            out.contains("2 malformed line(s)")
                && out.contains("server_tool_use x1")
                && out.contains("1 unpaired tool call/result block(s) dropped"),
            "{out}"
        );

        // Re-runs import nothing again, by id or all at once.
        let again = import(&claude, cwd, &sessions, None)
            .await
            .unwrap()
            .join("\n");
        assert!(
            again.contains("all 2 session(s) are imported already"),
            "{again}"
        );
        let one = import(&claude, cwd, &sessions, Some(&S1[..8]))
            .await
            .unwrap()
            .join("\n");
        assert!(one.contains("already imported"), "{one}");
        let listed = list(&claude, cwd, &sessions).await.unwrap().join("\n");
        assert_eq!(listed.matches("[imported: ").count(), 2, "{listed}");
        assert_eq!(
            snapshot(&claude),
            before,
            "~/.claude must be byte-identical"
        );

        let metas = Session::list_in(&sessions).await.unwrap();
        assert_eq!(metas.len(), 2);
        let meta = metas
            .iter()
            .find(|m| m.claude_code_session.as_deref() == Some(S1))
            .unwrap();
        assert_eq!(meta.name, "Login bug fix");
        assert_eq!(meta.preview, "Fix the [31mlogin [0m bug in auth.rs");
        assert_eq!(meta.cwd.as_deref(), Some(PROJECT));
        assert_eq!(meta.model.as_deref(), Some("claude-test-1"));
        assert_eq!(
            Some(meta.created_at),
            parse_timestamp("2026-03-01T09:59:59Z")
        );
        // Last activity is the session's, so the newer one lists first.
        assert_eq!(metas[0].claude_code_session.as_deref(), Some(S2));

        let (session, messages) = Session::resume_in(&sessions, &meta.id).await.unwrap();
        assert_eq!(session.meta.claude_code_session.as_deref(), Some(S1));
        assert_eq!(messages.len(), 6);
        assert_eq!(
            messages,
            parse_transcript(S1, fixture().as_bytes()).messages
        );

        // What the providers would be sent.
        let body = serde_json::to_value(&messages).unwrap();
        assert_eq!(body[0]["role"], "user");
        assert_eq!(body[1]["content"][1]["type"], "tool_use");
        let oai =
            crate::api::openai_compat::translate_messages("sys", &messages, false, false, None);
        let tool_calls = oai.iter().filter(|m| m.role == "tool").count();
        assert_eq!(tool_calls, 1, "one paired call, one tool message");
    }

    #[tokio::test]
    async fn one_session_imports_by_id_and_unknown_ids_are_errors() {
        let td = tempfile::tempdir().unwrap();
        let claude = claude_home(td.path());
        let sessions = td.path().join("sessions");
        let cwd = Path::new(PROJECT);
        let err = import(&claude, cwd, &sessions, Some("deadbeef"))
            .await
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("no Claude Code session 'deadbeef'"),
            "{err}"
        );
        let out = import(&claude, cwd, &sessions, Some(S1))
            .await
            .unwrap()
            .join("\n");
        assert!(out.contains("Imported 1"), "{out}");
        assert_eq!(imported_ids(&sessions).len(), 1);
        // Another project has none.
        let other = import(&claude, Path::new("/elsewhere"), &sessions, None)
            .await
            .unwrap();
        assert!(
            other[0].starts_with("No Claude Code sessions for /elsewhere"),
            "{other:?}"
        );
    }

    #[tokio::test]
    async fn oversized_transcripts_are_skipped_with_a_warning() {
        let td = tempfile::tempdir().unwrap();
        let claude = claude_home(td.path());
        let project = claude
            .join("projects")
            .join(project_dir_name(Path::new(PROJECT)));
        let big = std::fs::File::create(project.join(format!("{S2}.jsonl"))).unwrap();
        big.set_len(MAX_TRANSCRIPT_BYTES + 1).unwrap();
        let (found, warnings) = read_project(&claude, Path::new(PROJECT));
        assert_eq!(found.len(), 1);
        assert_eq!(warnings.len(), 1);
        assert!(
            warnings[0].contains(S2) && warnings[0].contains("50 MiB"),
            "{warnings:?}"
        );
    }

    /// A sessions dir inside ~/.claude (a config dir override pointing
    /// there) would mean writing into Claude Code's directory: refused.
    #[tokio::test]
    async fn never_writes_under_dot_claude() {
        let td = tempfile::tempdir().unwrap();
        let claude = claude_home(td.path());
        let before = snapshot(&claude);
        let err = import(&claude, Path::new(PROJECT), &claude.join("sessions"), None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("never written"), "{err}");
        #[cfg(unix)]
        {
            let link = td.path().join("link");
            std::os::unix::fs::symlink(&claude, &link).unwrap();
            let into_link = link.join("data/sessions");
            assert!(
                import(&claude, Path::new(PROJECT), &into_link, None)
                    .await
                    .is_err()
            );
        }
        assert_eq!(snapshot(&claude), before);
    }
}
