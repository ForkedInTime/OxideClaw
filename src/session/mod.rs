/// Session persistence — port of history.ts / session storage.
///
/// Each session is stored as two files in ~/.claude/sessions/:
///   <uuid>.jsonl  — one Message per line (full API history)
///   <uuid>.meta   — JSON with name, created_at, first_preview
use crate::api::types::{ContentBlock, Message, Role};
use crate::tui::app::ChatEntry;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tokio::fs;
use tokio::io::AsyncWriteExt;
use uuid::Uuid;

// ── Metadata ──────────────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Clone)]
pub struct SessionMeta {
    pub id: String,
    pub name: String,
    pub created_at: u64, // unix seconds
    pub preview: String, // first user message (truncated)
    #[serde(default)]
    pub tags: Vec<String>,
    /// Auto-commit SHAs on the session's shadow ref, chronological order
    /// (oldest → newest). Empty when auto-commit is disabled or cwd is
    /// outside a git work tree.
    #[serde(default)]
    pub auto_commits: Vec<String>,
    /// User's current read-head inside `auto_commits`. `0` means
    /// "at session base"; `auto_commits.len()` means "at latest turn".
    #[serde(default)]
    pub undo_position: usize,
}

impl SessionMeta {
    fn path_for(id: &str) -> PathBuf {
        crate::config::Config::sessions_dir().join(format!("{id}.meta"))
    }

    async fn save(&self) -> Result<()> {
        let path = Self::path_for(&self.id);
        let body = serde_json::to_string(self)?;
        atomic_write(&path, body.as_bytes()).await
    }

    async fn load(id: &str) -> Result<Self> {
        let path = Self::path_for(id);
        let s = fs::read_to_string(&path).await?;
        Ok(serde_json::from_str(&s)?)
    }
}

// ── Session ───────────────────────────────────────────────────────────────────

pub struct Session {
    pub id: String,
    pub meta: SessionMeta,
    path: PathBuf,
}

impl Session {
    fn jsonl_path(id: &str) -> PathBuf {
        crate::config::Config::sessions_dir().join(format!("{id}.jsonl"))
    }

    /// Create a new empty session with a human-readable default name.
    pub async fn new() -> Result<Self> {
        let id = Uuid::new_v4().to_string();
        fs::create_dir_all(crate::config::Config::sessions_dir()).await?;
        let meta = SessionMeta {
            id: id.clone(),
            name: human_session_name(),
            created_at: unix_now(),
            preview: String::new(),
            tags: Vec::new(),
            auto_commits: Vec::new(),
            undo_position: 0,
        };
        meta.save().await?;
        Ok(Self {
            id: id.clone(),
            meta,
            path: Self::jsonl_path(&id),
        })
    }

    /// Resume an existing session by ID — loads meta, returns Session + messages.
    pub async fn resume(id: &str) -> Result<(Self, Vec<Message>)> {
        let meta = SessionMeta::load(id)
            .await
            .with_context(|| format!("Session '{id}' not found"))?;
        let messages = Self::load_messages(id).await?;
        let s = Self {
            id: id.to_string(),
            meta,
            path: Self::jsonl_path(id),
        };
        Ok((s, messages))
    }

    /// Append new messages to the session file.
    pub async fn append(&mut self, new_messages: &[Message]) -> Result<()> {
        if new_messages.is_empty() {
            return Ok(());
        }

        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .await?;

        for msg in new_messages {
            let line = serde_json::to_string(msg)?;
            file.write_all(line.as_bytes()).await?;
            file.write_all(b"\n").await?;
        }
        // Durability. Without this the turn is reported as saved while the bytes
        // may still be in the page cache, so a crash loses it — and can leave a
        // half-written final line behind (see `load_messages`, which tolerates
        // exactly that).
        file.sync_all().await?;

        // Update preview from first user message if not yet set
        if self.meta.preview.is_empty()
            && let Some(preview) = first_user_preview(new_messages)
        {
            self.meta.preview = preview;
            self.meta.save().await?;
        }

        Ok(())
    }

    /// Overwrite the session file with a completely new set of messages.
    /// Used after compaction to keep the on-disk file consistent.
    pub async fn overwrite(&self, messages: &[Message]) -> Result<()> {
        // Pre-allocate ~256 bytes per message to reduce re-allocs
        let mut content = String::with_capacity(messages.len() * 256);
        for msg in messages {
            content.push_str(&serde_json::to_string(msg)?);
            content.push('\n');
        }
        atomic_write(&self.path, content.as_bytes()).await
    }

    /// Rename the session.
    pub async fn rename(&mut self, name: &str) -> Result<()> {
        self.meta.name = name.to_string();
        self.meta.save().await
    }

    /// Persist the current `SessionMeta` to disk. Used by the auto-commit loop
    /// to checkpoint updated `auto_commits` / `undo_position` after each turn.
    pub async fn save_meta(&self) -> anyhow::Result<()> {
        self.meta.save().await
    }

    /// Load all messages from a session file. Returns an empty vec if the
    /// session file does not exist — no TOCTOU race between an exists() check
    /// and the read, because we let the read itself surface the NotFound.
    pub async fn load_messages(id: &str) -> Result<Vec<Message>> {
        let path = Self::jsonl_path(id);
        let content = match fs::read_to_string(&path).await {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        parse_message_lines(id, &content)
    }

    /// List all saved sessions, newest first.
    /// Backfills empty previews from session messages (for older sessions).
    pub async fn list() -> Result<Vec<SessionMeta>> {
        let dir = crate::config::Config::sessions_dir();
        if !dir.exists() {
            return Ok(Vec::new());
        }

        let mut entries = fs::read_dir(&dir).await?;
        let mut sessions: Vec<(u64, SessionMeta)> = Vec::new();

        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("meta") {
                let id = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("")
                    .to_string();
                if !id.is_empty()
                    && let Ok(mut meta) = SessionMeta::load(&id).await
                {
                    // Backfill empty preview from session messages
                    if meta.preview.is_empty()
                        && let Ok(msgs) = Self::load_messages(&id).await
                        && let Some(preview) = first_user_preview(&msgs)
                    {
                        meta.preview = preview;
                        let _ = meta.save().await;
                    }
                    sessions.push((meta.created_at, meta));
                }
            }
        }

        sessions.sort_by_key(|e| std::cmp::Reverse(e.0));
        Ok(sessions.into_iter().map(|(_, m)| m).collect())
    }

    /// Delete a session (both .jsonl and .meta).
    pub async fn delete(id: &str) -> Result<()> {
        let jsonl = Self::jsonl_path(id);
        let meta = SessionMeta::path_for(id);
        if jsonl.exists() {
            fs::remove_file(&jsonl).await?;
        }
        if meta.exists() {
            fs::remove_file(&meta).await?;
        }
        Ok(())
    }

    /// Export session to a markdown file, returns the path written.
    pub async fn export(id: &str, dest: &std::path::Path) -> Result<PathBuf> {
        let messages = Self::load_messages(id).await?;
        let meta = SessionMeta::load(id).await.ok();
        let name = meta.map(|m| m.name).unwrap_or_else(|| id.to_string());

        let mut out = format!("# Session: {name}\n\n");
        for msg in &messages {
            let role = match msg.role {
                Role::User => "You",
                Role::Assistant => "Claude",
            };
            for block in &msg.content {
                match block {
                    ContentBlock::Text { text } => {
                        out.push_str(&format!("**{role}:** {text}\n\n"));
                    }
                    ContentBlock::ToolUse { name, .. } => {
                        out.push_str(&format!("**Tool:** {name}\n\n"));
                    }
                    ContentBlock::ToolResult { .. } => {}
                    _ => {}
                }
            }
        }

        // Same treatment as the session files themselves: a direct write
        // truncates the destination first, so an interrupted export leaves the
        // user with an empty or half-written file where their transcript was.
        atomic_write(dest, out.as_bytes()).await?;
        Ok(dest.to_path_buf())
    }

    /// Export session to a markdown string (used for clipboard export).
    pub async fn export_to_string(id: &str) -> Result<String> {
        let messages = Self::load_messages(id).await?;
        let meta = SessionMeta::load(id).await.ok();
        let name = meta.map(|m| m.name).unwrap_or_else(|| id.to_string());

        let mut out = format!("# Session: {name}\n\n");
        for msg in &messages {
            let role = match msg.role {
                Role::User => "You",
                Role::Assistant => "Claude",
            };
            for block in &msg.content {
                match block {
                    ContentBlock::Text { text } => {
                        out.push_str(&format!("**{role}:** {text}\n\n"));
                    }
                    ContentBlock::ToolUse { name, .. } => {
                        out.push_str(&format!("**Tool:** {name}\n\n"));
                    }
                    ContentBlock::ToolResult { .. } => {}
                    _ => {}
                }
            }
        }
        Ok(out)
    }
}

/// Reconstruct ChatEntry display list from a saved message history.
pub fn entries_from_messages(messages: &[Message]) -> Vec<ChatEntry> {
    let mut entries = Vec::new();
    for msg in messages {
        match msg.role {
            Role::User => {
                for block in &msg.content {
                    match block {
                        ContentBlock::Text { text } => {
                            entries.push(ChatEntry::user(text.clone()));
                        }
                        ContentBlock::ToolResult {
                            content, is_error, ..
                        } => {
                            let text = content
                                .iter()
                                .map(|c| {
                                    let crate::api::types::ToolResultContent::Text { text } = c;
                                    text.as_str()
                                })
                                .collect::<Vec<_>>()
                                .join("\n");
                            let preview = if text.len() > 300 {
                                format!("{}…", &text[..300])
                            } else {
                                text
                            };
                            if is_error.unwrap_or(false) {
                                entries.push(ChatEntry::error(preview));
                            } else {
                                entries.push(ChatEntry::tool_result(preview));
                            }
                        }
                        _ => {}
                    }
                }
            }
            Role::Assistant => {
                let mut text_parts: Vec<String> = Vec::new();
                for block in &msg.content {
                    match block {
                        ContentBlock::Text { text } if !text.trim().is_empty() => {
                            text_parts.push(text.clone());
                        }
                        ContentBlock::ToolUse { id: _, name, input } => {
                            let args = serde_json::to_string(input).unwrap_or_default();
                            let preview = crate::tui::app::format_tool_preview_pub(name, &args);
                            entries.push(ChatEntry::tool_call(format!("{name}  {preview}")));
                        }
                        _ => {}
                    }
                }
                if !text_parts.is_empty() {
                    entries.push(ChatEntry::assistant(text_parts.join("\n")));
                }
            }
        }
    }
    entries
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Generate a tmux-friendly session name with hostname prefix.
/// Format: "hostname-adjective-animal"
/// Used when launching with --tmux so the pane has a recognisable title.
pub fn generate_tmux_session_name() -> String {
    const ADJECTIVES: &[&str] = &[
        "bold", "bright", "calm", "crisp", "dawn", "deft", "early", "eager", "fair", "fast",
        "fierce", "free", "glad", "gold", "grand", "great", "keen", "kind", "light", "lush",
        "mild", "neat", "nimble", "noble", "prime", "pure", "quick", "quiet", "rapid", "sharp",
        "sleek", "smart", "soft", "steady", "still", "strong", "swift", "true", "vivid", "warm",
    ];
    const ANIMALS: &[&str] = &[
        "badger", "bear", "bison", "boar", "capybara", "cat", "crane", "deer", "dolphin", "dove",
        "eagle", "elk", "falcon", "finch", "fox", "gecko", "goose", "heron", "ibis", "jaguar",
        "jay", "kite", "kiwi", "leopard", "lion", "lynx", "mink", "moose", "newt", "orca", "otter",
        "owl", "panda", "panther", "parrot", "puma", "raven", "seal", "shark", "stag", "swift",
        "tiger", "toucan", "turtle", "viper", "vole", "wolf", "wren",
    ];

    let hostname = std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            std::process::Command::new("hostname")
                .output()
                .ok()
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .map(|s| s.trim().to_lowercase())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| "host".to_string());

    // Use current timestamp as seed for deterministic but varied names
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as usize)
        .unwrap_or(42);

    let adj = ADJECTIVES[seed % ADJECTIVES.len()];
    let animal = ANIMALS[(seed / ADJECTIVES.len()) % ANIMALS.len()];

    format!("{hostname}-{adj}-{animal}")
}

/// Generate a human-readable default session name in local time.
/// Format: "Thu Apr 3, 6:51 PM"
/// Uses the system `date` command so the timezone is always correct.
fn human_session_name() -> String {
    std::process::Command::new("date")
        .arg("+%a %b %-d, %-I:%M %p")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "New session".to_string())
}


/// Parse a session's JSONL body.
///
/// Split out from `load_messages` so the torn-tail and mid-file-corruption
/// behaviour can be tested against an explicit file rather than the global
/// sessions directory.
fn parse_message_lines(id: &str, content: &str) -> Result<Vec<Message>> {
        let lines: Vec<&str> = content.lines().filter(|l| !l.trim().is_empty()).collect();
        let total = lines.len();
        let mut out = Vec::with_capacity(total);

        for (i, line) in lines.into_iter().enumerate() {
            match serde_json::from_str::<Message>(line) {
                Ok(m) => out.push(m),
                Err(e) => {
                    // A torn *final* line is the expected shape of a crash
                    // mid-append: nothing else references it, so dropping it
                    // recovers the whole session minus one turn. Previously any
                    // bad line failed the entire load via `collect()`, which
                    // turned a half-written last line into total loss of the
                    // conversation — the one thing sessions exist to prevent.
                    if i + 1 == total {
                        tracing::warn!(
                            "session {id}: discarding incomplete final line \
                             (likely an interrupted write): {e}"
                        );
                        break;
                    }
                    // Corruption anywhere else is not a torn write. Skipping it
                    // could drop a tool_use while keeping its tool_result, which
                    // the API rejects outright — a subtly broken conversation is
                    // worse than a clear error.
                    return Err(anyhow::anyhow!(
                        "session {id} is corrupt at line {} of {total}: {e}. \
                         Refusing to load a partial history — later messages may \
                         depend on it.",
                        i + 1
                    ));
                }
            }
        }
    Ok(out)
}

/// Atomic file write: write to a sibling temp file, fsync, then rename over
/// the target. Survives mid-write crashes — the target is either the old
/// content or the new content, never a truncated splice. Falls back to a
/// direct write only if the temp-file path can't be constructed.
async fn atomic_write(path: &std::path::Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("session path has no parent: {}", path.display()))?;
    fs::create_dir_all(parent).await?;

    let file_name = path
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("session path has no file name: {}", path.display()))?;
    let tmp = parent.join(format!(
        ".{}.tmp.{}",
        file_name.to_string_lossy(),
        Uuid::new_v4()
    ));

    {
        let mut f = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&tmp)
            .await?;
        f.write_all(bytes).await?;
        f.sync_all().await?;
    }

    // tokio::fs::rename is atomic on POSIX and on Windows when paths are on
    // the same volume. Both paths are siblings under the sessions dir, so
    // we always satisfy that constraint.
    if let Err(e) = fs::rename(&tmp, path).await {
        // Best-effort cleanup on rename failure.
        let _ = fs::remove_file(&tmp).await;
        return Err(e.into());
    }
    Ok(())
}

fn first_user_preview(messages: &[Message]) -> Option<String> {
    for msg in messages {
        if matches!(msg.role, Role::User) {
            for block in &msg.content {
                if let ContentBlock::Text { text } = block {
                    let preview = text.chars().take(60).collect::<String>();
                    let preview = preview.replace('\n', " ");
                    return Some(preview);
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod meta_serde_tests {
    use super::SessionMeta;

    #[test]
    fn loads_legacy_meta_without_autocommit_fields() {
        let json = r#"{
            "id": "abc",
            "name": "Test",
            "created_at": 1700000000,
            "preview": "hello"
        }"#;
        let m: SessionMeta = serde_json::from_str(json).unwrap();
        assert_eq!(m.id, "abc");
        assert!(m.auto_commits.is_empty());
        assert_eq!(m.undo_position, 0);
    }

    #[test]
    fn roundtrips_with_autocommit_fields() {
        let json = r#"{
            "id": "xyz",
            "name": "Test",
            "created_at": 1700000000,
            "preview": "hi",
            "auto_commits": ["aaa111", "bbb222"],
            "undo_position": 2
        }"#;
        let m: SessionMeta = serde_json::from_str(json).unwrap();
        assert_eq!(m.auto_commits, vec!["aaa111", "bbb222"]);
        assert_eq!(m.undo_position, 2);

        let out = serde_json::to_string(&m).unwrap();
        assert!(out.contains("auto_commits"));
        assert!(out.contains("undo_position"));
    }
}

#[cfg(test)]
mod atomic_write_tests {
    use super::atomic_write;
    use tempfile::tempdir;

    #[tokio::test]
    async fn writes_then_replaces() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("session.meta");
        atomic_write(&target, b"v1").await.unwrap();
        assert_eq!(tokio::fs::read_to_string(&target).await.unwrap(), "v1");
        atomic_write(&target, b"v2").await.unwrap();
        assert_eq!(tokio::fs::read_to_string(&target).await.unwrap(), "v2");
    }

    #[tokio::test]
    async fn leaves_no_temp_files_behind() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("session.meta");
        atomic_write(&target, b"hello").await.unwrap();
        let mut entries = tokio::fs::read_dir(dir.path()).await.unwrap();
        let mut names = Vec::new();
        while let Some(e) = entries.next_entry().await.unwrap() {
            names.push(e.file_name().to_string_lossy().to_string());
        }
        assert_eq!(names, vec!["session.meta"]);
    }

    #[tokio::test]
    async fn creates_parent_dir_if_missing() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("nested/sub/session.meta");
        atomic_write(&target, b"x").await.unwrap();
        assert_eq!(tokio::fs::read_to_string(&target).await.unwrap(), "x");
    }
}

#[cfg(test)]
mod durability_tests {
    use super::*;
    use crate::api::types::{ContentBlock, Role};

    fn msg(text: &str) -> Message {
        Message {
            role: Role::User,
            content: vec![ContentBlock::Text { text: text.into() }],
        }
    }

    /// Write a JSONL file directly so these tests do not depend on the real
    /// sessions directory or on `Session::new`'s side effects.
    fn write_jsonl(dir: &std::path::Path, id: &str, msgs: &[Message], torn_tail: Option<usize>) {
        let mut body = String::new();
        for (i, m) in msgs.iter().enumerate() {
            let line = serde_json::to_string(m).unwrap();
            if let Some(keep) = torn_tail.filter(|_| i + 1 == msgs.len()) {
                let keep = keep.min(line.len());
                body.push_str(&line[..keep]); // no trailing newline: a cut-off write
            } else {
                body.push_str(&line);
                body.push('\n');
            }
        }
        std::fs::write(dir.join(format!("{id}.jsonl")), body).unwrap();
    }

    fn parse(dir: &std::path::Path, id: &str) -> Result<Vec<Message>> {
        // Mirror of load_messages' parsing over an explicit path, so the test
        // does not have to relocate the global sessions directory.
        let content = std::fs::read_to_string(dir.join(format!("{id}.jsonl"))).unwrap_or_default();
        parse_message_lines(id, &content)
    }

    /// The regression: a crash mid-append leaves the final line cut off. That
    /// used to fail the entire load via `collect()`, losing the whole
    /// conversation rather than one turn.
    #[test]
    fn torn_final_line_costs_one_turn_not_the_session() {
        let d = tempfile::tempdir().unwrap();
        write_jsonl(d.path(), "s", &[msg("one"), msg("two"), msg("three")], Some(14));

        let got = parse(d.path(), "s").expect("a torn tail must not fail the load");
        assert_eq!(got.len(), 2, "complete turns survive, the torn one is dropped");
    }

    /// Corruption that is not a torn tail must fail loudly: silently skipping a
    /// middle line can drop a tool_use while keeping its tool_result, which the
    /// API rejects outright. A subtly broken conversation is worse than an error.
    #[test]
    fn mid_file_corruption_is_reported_with_its_location() {
        let d = tempfile::tempdir().unwrap();
        write_jsonl(d.path(), "s", &[msg("one"), msg("two"), msg("three")], None);
        let p = d.path().join("s.jsonl");
        let mut lines: Vec<String> = std::fs::read_to_string(&p)
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect();
        lines[1] = "{ not json".into();
        std::fs::write(&p, lines.join("\n")).unwrap();

        let err = parse(d.path(), "s").expect_err("must not silently skip");
        let m = err.to_string();
        assert!(m.contains("corrupt"), "{m}");
        assert!(m.contains("line 2"), "must locate the damage: {m}");
    }

    #[test]
    fn intact_history_round_trips() {
        let d = tempfile::tempdir().unwrap();
        write_jsonl(d.path(), "s", &[msg("a"), msg("b"), msg("c")], None);
        assert_eq!(parse(d.path(), "s").unwrap().len(), 3);
    }

    #[test]
    fn empty_history_is_not_corruption() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("s.jsonl"), "").unwrap();
        assert!(parse(d.path(), "s").unwrap().is_empty());
    }

    /// Blank lines are padding, not damage.
    #[test]
    fn blank_lines_are_ignored() {
        let d = tempfile::tempdir().unwrap();
        write_jsonl(d.path(), "s", &[msg("a"), msg("b")], None);
        let p = d.path().join("s.jsonl");
        let c = std::fs::read_to_string(&p).unwrap();
        std::fs::write(&p, c.replace('\n', "\n\n")).unwrap();
        assert_eq!(parse(d.path(), "s").unwrap().len(), 2);
    }
}
