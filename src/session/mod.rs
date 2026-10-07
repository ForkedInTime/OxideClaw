/// Session persistence — port of history.ts / session storage.
///
/// Each session is stored as two files in <data dir>/sessions/:
///   <uuid>.jsonl  — one Message per line (full API history)
///   <uuid>.meta   — JSON with name, created_at, first_preview
use crate::api::types::{ContentBlock, Message, Role, ToolResultContent};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tokio::fs;
use tokio::io::AsyncWriteExt;
use uuid::Uuid;

pub mod claude_code;

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
    /// Snapshot of the uncommitted work present before the first recorded
    /// turn: the parent of turn 1, i.e. what `/undo` to the session base
    /// restores. `None` when the tree matched HEAD (or in legacy metas), in
    /// which case HEAD is the base.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_commit: Option<String>,
    /// The undo timeline: one mark per prompt of the conversation, oldest
    /// first, saved before the prompt reaches the transcript. After a crash
    /// it can run ahead of the transcript, never behind it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub timeline: Vec<TurnMark>,
    /// The directory the session ran in, when recorded (imported sessions).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// The model that wrote the last reply, when recorded (imported sessions).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The Claude Code session this one was imported from
    /// (`config import-claude --sessions`), so a re-run skips it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude_code_session: Option<String>,
    /// When the session was imported (unix seconds). Imports keep the
    /// original session's age for display, so this is what keeps
    /// `cleanupPeriodDays` from deleting one right after the import.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub imported_at: Option<u64>,
    /// Turns /undo took off the conversation, the next one to /redo last.
    /// Any new turn clears it. Never part of the `.meta`: it holds whole
    /// messages (tool output, file contents), which `Session::list` would
    /// read for every session and `--no-session-persistence` must keep off
    /// disk. [`Session::save_redo`] keeps it in `<id>.redo`.
    #[serde(skip)]
    pub redo: Vec<UndoneTurn>,
}

/// A prompt on the undo timeline and where the files were when it started.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct TurnMark {
    /// [`prompt_fingerprint`] of the prompt this mark belongs to. A mark
    /// only ever pairs with that prompt, so a transcript that changed
    /// under the timeline (compaction, an import, a crash between saves)
    /// can never make /undo restore another turn's files.
    pub prompt: String,
    /// `undo_position` when the turn started: what /undo of it restores.
    pub before: usize,
}

/// A turn /undo removed: its messages, and the file positions to put back.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct UndoneTurn {
    pub mark: TurnMark,
    /// `undo_position` after the turn: what /redo of it restores.
    pub after: usize,
    pub messages: Vec<Message>,
}

/// Stable identity of a prompt message, for [`TurnMark::prompt`].
pub fn prompt_fingerprint(message: &Message) -> String {
    use sha2::{Digest, Sha256};
    let bytes = serde_json::to_vec(message).unwrap_or_default();
    Sha256::digest(&bytes)[..16]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

impl SessionMeta {
    fn path_in(dir: &Path, id: &str) -> PathBuf {
        dir.join(format!("{id}.meta"))
    }

    async fn save_in(&self, dir: &Path) -> Result<()> {
        let path = Self::path_in(dir, &self.id);
        let body = serde_json::to_string(self)?;
        atomic_write(&path, body.as_bytes()).await
    }

    async fn load_in(dir: &Path, id: &str) -> Result<Self> {
        let path = Self::path_in(dir, id);
        let s = fs::read_to_string(&path).await?;
        let mut meta: Self = serde_json::from_str(&s)?;
        match fs::read_to_string(redo_path(dir, id)).await {
            Ok(body) => match serde_json::from_str(&body) {
                Ok(redo) => meta.redo = redo,
                Err(e) => tracing::warn!("session {id}: ignoring unreadable redo file: {e}"),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => tracing::warn!("session {id}: could not read redo file: {e}"),
        }
        Ok(meta)
    }
}

/// Where a session's /redo turns are kept between runs.
fn redo_path(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.redo"))
}

// ── Session ───────────────────────────────────────────────────────────────────

pub struct Session {
    pub id: String,
    pub meta: SessionMeta,
    /// The sessions directory this session's files live in.
    dir: PathBuf,
    path: PathBuf,
}

impl Session {
    fn jsonl_path(dir: &Path, id: &str) -> PathBuf {
        dir.join(format!("{id}.jsonl"))
    }

    /// Create a new empty session with a human-readable default name.
    pub async fn new() -> Result<Self> {
        Self::new_with_id(Uuid::new_v4().to_string()).await
    }

    /// A new empty session under a caller-chosen ID (`--session-id`). The
    /// caller validates it as a UUID, which also keeps it a plain file name.
    pub async fn new_with_id(id: String) -> Result<Self> {
        Self::create_in(&crate::config::Config::sessions_dir(), id).await
    }

    /// `new_with_id` in the sessions directory `dir`.
    pub async fn create_in(dir: &Path, id: String) -> Result<Self> {
        create_private_dir(dir).await?;
        let meta = SessionMeta {
            id: id.clone(),
            name: human_session_name(),
            created_at: unix_now(),
            preview: String::new(),
            tags: Vec::new(),
            auto_commits: Vec::new(),
            undo_position: 0,
            base_commit: None,
            timeline: Vec::new(),
            cwd: None,
            model: None,
            claude_code_session: None,
            imported_at: None,
            redo: Vec::new(),
        };
        meta.save_in(dir).await?;
        Ok(Self {
            id: id.clone(),
            meta,
            dir: dir.to_path_buf(),
            path: Self::jsonl_path(dir, &id),
        })
    }

    /// A session whose transcript lives at `path`, so tests never touch the
    /// real sessions directory. Only the `.jsonl` is written through it.
    #[cfg(test)]
    pub(crate) fn at_path(id: &str, path: PathBuf) -> Self {
        Self {
            id: id.to_string(),
            meta: SessionMeta {
                id: id.to_string(),
                name: String::new(),
                created_at: 0,
                preview: "set".into(),
                tags: Vec::new(),
                auto_commits: Vec::new(),
                undo_position: 0,
                base_commit: None,
                timeline: Vec::new(),
                cwd: None,
                model: None,
                claude_code_session: None,
                imported_at: None,
                redo: Vec::new(),
            },
            dir: path.parent().map(Path::to_path_buf).unwrap_or_default(),
            path,
        }
    }

    /// Is there a saved session with exactly this ID?
    pub fn exists(id: &str) -> bool {
        Self::exists_in(&crate::config::Config::sessions_dir(), id)
    }

    /// `exists` in the sessions directory `dir`. An ID that is not a plain
    /// file name never exists, so it cannot name a file outside `dir`.
    pub fn exists_in(dir: &Path, id: &str) -> bool {
        is_safe_session_id(id) && SessionMeta::path_in(dir, id).exists()
    }

    /// Resolve what a user typed to a saved session's ID: the full ID, a
    /// unique ID prefix (the short IDs the picker and banner show), or an
    /// exact session name.
    pub async fn resolve(query: &str) -> Result<String> {
        Self::resolve_in(&crate::config::Config::sessions_dir(), query).await
    }

    async fn resolve_in(dir: &std::path::Path, query: &str) -> Result<String> {
        let q = query.trim();
        anyhow::ensure!(!q.is_empty(), "No session ID or name given");
        let list = Self::list_in(dir).await?;
        if let Some(m) = list.iter().find(|m| m.id == q) {
            return Ok(m.id.clone());
        }
        let matched: Vec<_> = list
            .iter()
            .filter(|m| m.id.starts_with(q) || m.name == q)
            .collect();
        match matched.as_slice() {
            [] => anyhow::bail!("No saved session matches '{q}'"),
            [m] => Ok(m.id.clone()),
            many => {
                let ids: Vec<_> = many
                    .iter()
                    .map(|m| format!("  {} — {}", m.id, m.name))
                    .collect();
                anyhow::bail!(
                    "Multiple sessions match '{q}':\n{}\nBe more specific.",
                    ids.join("\n")
                )
            }
        }
    }

    /// Resume an existing session by ID — loads meta, returns Session + messages.
    pub async fn resume(id: &str) -> Result<(Self, Vec<Message>)> {
        Self::resume_in(&crate::config::Config::sessions_dir(), id).await
    }

    /// `resume` from the sessions directory `dir`.
    pub async fn resume_in(dir: &Path, id: &str) -> Result<(Self, Vec<Message>)> {
        anyhow::ensure!(is_safe_session_id(id), "invalid session id: {id:?}");
        let meta = SessionMeta::load_in(dir, id)
            .await
            .with_context(|| format!("Session '{id}' not found"))?;
        let s = Self {
            id: id.to_string(),
            meta,
            dir: dir.to_path_buf(),
            path: Self::jsonl_path(dir, id),
        };
        let messages = s.load_and_heal().await?;
        Ok((s, messages))
    }

    /// Load this session's transcript, first rewriting the file if its tail
    /// was torn by a crash mid-append. Dropping the torn line only in memory
    /// was one-shot: the next append glued onto the fragment, and the session
    /// then refused to load ("corrupt at line N").
    async fn load_and_heal(&self) -> Result<Vec<Message>> {
        let content = match fs::read_to_string(&self.path).await {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        let (mut messages, torn) = parse_intact_lines(&self.id, &content)?;
        if torn {
            // The intact lines, not the repaired list: repair stubs are
            // rebuilt on every load, and persisting one would put two user
            // turns in a row on disk once the next prompt is appended.
            if let Err(e) = self.overwrite(&messages).await {
                tracing::warn!(
                    "session {}: could not rewrite torn transcript: {e}",
                    self.id
                );
            }
        }
        repair_loaded(&self.id, &mut messages);
        Ok(messages)
    }

    /// Turn this into a copy under a new id: new files holding the same
    /// history. Later writes go to the copy and the original is untouched.
    /// (Changing only `id` left `path` and `meta.id` on the original, so a
    /// "fork" appended to and renamed the session it forked from.)
    pub async fn fork(&mut self, messages: &[Message]) -> Result<()> {
        let id = Uuid::new_v4().to_string();
        let origin: String = self.id.chars().take(8).collect();
        self.meta.name = format!("fork-of-{origin}");
        self.meta.id = id.clone();
        self.meta.created_at = unix_now();
        // Undo history lives on the original's shadow ref; the copied turns
        // can still be undone, conversation only.
        self.meta.auto_commits.clear();
        self.meta.undo_position = 0;
        self.meta.base_commit = None;
        self.meta.timeline.clear();
        self.meta.redo.clear();
        // A fork is not the import: only the original claims the Claude Code
        // session (re-runs skip it, `--list` marks it) and its grace period.
        self.meta.claude_code_session = None;
        self.meta.imported_at = None;
        self.path = Self::jsonl_path(&self.dir, &id);
        self.id = id;
        self.meta.save_in(&self.dir).await?;
        self.overwrite(messages).await
    }

    /// Append new messages to the session file.
    pub async fn append(&mut self, new_messages: &[Message]) -> Result<()> {
        if new_messages.is_empty() {
            return Ok(());
        }

        let mut batch = Vec::new();
        for msg in new_messages {
            serde_json::to_writer(&mut batch, msg)?;
            batch.push(b'\n');
        }

        let mut options = fs::OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options.open(&self.path).await?;
        let original_len = file.metadata().await?.len();
        // A failed write (ENOSPC, EIO) can leave part of the batch behind;
        // cut it off so the caller can retry the whole batch and the next
        // append does not glue onto half a line. tokio's File reports a
        // buffered write as done before it runs: only `flush` returns its
        // result (`sync_all` would swallow it).
        let written = async {
            file.write_all(&batch).await?;
            file.flush().await
        }
        .await;
        if let Err(e) = written {
            let _ = file.set_len(original_len).await;
            return Err(e.into());
        }
        // Durability. Without this the turn is reported as saved while the bytes
        // may still be in the page cache, so a crash loses it — and can leave a
        // half-written final line behind (see `load_and_heal`).
        file.sync_all().await?;

        // Update preview from first user message if not yet set
        if self.meta.preview.is_empty()
            && let Some(preview) = first_user_preview(new_messages)
        {
            self.meta.preview = preview;
            self.meta.save_in(&self.dir).await?;
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
        self.meta.save_in(&self.dir).await
    }

    /// Save the /redo turns to `<id>.redo`, or remove that file when there
    /// are none or the session is not persisted (`persist` false): the
    /// undone turns' messages then stay in memory only.
    pub async fn save_redo(&self, persist: bool) -> Result<()> {
        let path = redo_path(&self.dir, &self.id);
        if persist && !self.meta.redo.is_empty() {
            let body = serde_json::to_string(&self.meta.redo)?;
            return atomic_write(&path, body.as_bytes()).await;
        }
        match fs::remove_file(&path).await {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
            _ => Ok(()),
        }
    }

    /// Persist the current `SessionMeta` to disk. Used by the auto-commit loop
    /// to checkpoint updated `auto_commits` / `undo_position` after each turn.
    pub async fn save_meta(&self) -> anyhow::Result<()> {
        self.meta.save_in(&self.dir).await
    }

    /// Load all messages from a session file. Returns an empty vec if the
    /// session file does not exist — no TOCTOU race between an exists() check
    /// and the read, because we let the read itself surface the NotFound.
    pub async fn load_messages(id: &str) -> Result<Vec<Message>> {
        let path = Self::jsonl_path(&crate::config::Config::sessions_dir(), id);
        let content = match fs::read_to_string(&path).await {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        parse_message_lines(id, &content)
    }

    /// List all saved sessions, most recently active first.
    /// Backfills empty previews from session messages (for older sessions).
    pub async fn list() -> Result<Vec<SessionMeta>> {
        Self::list_in(&crate::config::Config::sessions_dir()).await
    }

    async fn list_in(dir: &std::path::Path) -> Result<Vec<SessionMeta>> {
        Ok(Self::list_with_activity_in(dir)
            .await?
            .into_iter()
            .map(|(_, m)| m)
            .collect())
    }

    /// Like `list_in`, paired with each session's last-activity time:
    /// max(created_at, mtime of its .jsonl). The .meta mtime is left out on
    /// purpose: the preview backfill below rewrites it on every listing.
    async fn list_with_activity_in(dir: &std::path::Path) -> Result<Vec<(u64, SessionMeta)>> {
        if !dir.exists() {
            return Ok(Vec::new());
        }

        let mut entries = fs::read_dir(dir).await?;
        let mut sessions: Vec<(u64, SessionMeta)> = Vec::new();

        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("meta") {
                let id = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("")
                    .to_string();
                let Some(mut meta) = fs::read_to_string(&path)
                    .await
                    .ok()
                    .and_then(|s| serde_json::from_str::<SessionMeta>(&s).ok())
                else {
                    continue;
                };
                if id.is_empty() {
                    continue;
                }
                let jsonl = dir.join(format!("{id}.jsonl"));
                // Backfill empty preview from session messages
                if meta.preview.is_empty()
                    && let Ok(content) = fs::read_to_string(&jsonl).await
                    && let Ok(msgs) = parse_message_lines(&id, &content)
                    && let Some(preview) = first_user_preview(&msgs)
                {
                    meta.preview = preview;
                    if let Ok(body) = serde_json::to_string(&meta) {
                        let _ = atomic_write(&path, body.as_bytes()).await;
                    }
                }
                // created_at never changes, so ordering by it alone put a
                // session worked on today below one merely opened later.
                let modified = fs::metadata(&jsonl)
                    .await
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map_or(0, |d| d.as_secs());
                sessions.push((meta.created_at.max(modified), meta));
            }
        }

        sessions.sort_by_key(|e| std::cmp::Reverse(e.0));
        Ok(sessions)
    }

    /// `cleanupPeriodDays`: delete sessions idle for more than `days`, never
    /// `keep` (the session about to be resumed). Judged by last activity, not
    /// created_at, or a long-lived session used daily is deleted, right
    /// before `--continue` would reopen it.
    pub async fn prune_inactive(days: u32, keep: Option<&str>) {
        let cutoff = unix_now().saturating_sub(u64::from(days) * 86400);
        Self::prune_inactive_in(&crate::config::Config::sessions_dir(), cutoff, keep).await;
    }

    async fn prune_inactive_in(dir: &std::path::Path, cutoff: u64, keep: Option<&str>) {
        let Ok(list) = Self::list_with_activity_in(dir).await else {
            return;
        };
        for (last_active, meta) in list {
            if last_active.max(meta.imported_at.unwrap_or(0)) >= cutoff
                || keep == Some(meta.id.as_str())
                || !is_safe_session_id(&meta.id)
            {
                continue;
            }
            let _ = fs::remove_file(dir.join(format!("{}.jsonl", meta.id))).await;
            let _ = fs::remove_file(dir.join(format!("{}.meta", meta.id))).await;
            let _ = fs::remove_file(redo_path(dir, &meta.id)).await;
            Self::remove_snapshots_in(dir, &meta.id).await;
        }
    }

    /// Remove `<dir>/<id>/`, where older versions kept per-turn copies of
    /// edited files for /rewind. Without this a deleted session kept them
    /// forever. The id comes from the .meta body, so anything but a plain id
    /// is refused rather than letting a tampered meta point remove_dir_all
    /// at sessions_dir, its parent, or (Windows `C:`) a drive's cwd.
    async fn remove_snapshots_in(dir: &std::path::Path, id: &str) {
        if !is_safe_session_id(id) {
            return;
        }
        let _ = fs::remove_dir_all(dir.join(id)).await;
    }

    /// The session `--continue` / `--resume` reopens: the most recently
    /// active one that has messages. Every launch writes a `.meta` before
    /// the first prompt, so a launch-and-quit leaves an empty session that
    /// would otherwise win.
    pub async fn most_recent() -> Option<String> {
        Self::most_recent_in(&crate::config::Config::sessions_dir()).await
    }

    async fn most_recent_in(dir: &std::path::Path) -> Option<String> {
        for meta in Self::list_in(dir).await.ok()? {
            let jsonl = dir.join(format!("{}.jsonl", meta.id));
            if fs::metadata(&jsonl).await.is_ok_and(|m| m.len() > 0) {
                return Some(meta.id);
            }
        }
        None
    }

    /// Delete a session: its .jsonl, .meta, .redo and file-snapshot directory.
    pub async fn delete(id: &str) -> Result<()> {
        Self::delete_in(&crate::config::Config::sessions_dir(), id).await
    }

    async fn delete_in(dir: &std::path::Path, id: &str) -> Result<()> {
        if !is_safe_session_id(id) {
            anyhow::bail!("invalid session id: {id:?}");
        }
        let jsonl = dir.join(format!("{id}.jsonl"));
        let meta = dir.join(format!("{id}.meta"));
        if jsonl.exists() {
            fs::remove_file(&jsonl).await?;
        }
        if meta.exists() {
            fs::remove_file(&meta).await?;
        }
        let _ = fs::remove_file(redo_path(dir, id)).await;
        Self::remove_snapshots_in(dir, id).await;
        Ok(())
    }

    /// Export session to a markdown file, returns the path written.
    pub async fn export(id: &str, dest: &std::path::Path) -> Result<PathBuf> {
        let messages = Self::load_messages(id).await?;
        let meta = SessionMeta::load_in(&crate::config::Config::sessions_dir(), id)
            .await
            .ok();
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
        atomic_write_shared(dest, out.as_bytes()).await?;
        Ok(dest.to_path_buf())
    }

    /// Export session to a markdown string (used for clipboard export).
    pub async fn export_to_string(id: &str) -> Result<String> {
        let messages = Self::load_messages(id).await?;
        let meta = SessionMeta::load_in(&crate::config::Config::sessions_dir(), id)
            .await
            .ok();
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

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Session ids are UUIDs; anything else (`..`, `a/b`, a Windows `C:`) must
/// never become a path component that delete or cleanup removes.
fn is_safe_session_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

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

/// Ensure every `tool_use` is answered by a `tool_result`.
///
/// The API rejects an assistant turn containing a `tool_use` that no following
/// user turn answers. History can reach that shape legitimately: a crash during
/// `append` can tear the *tool_result* line, and `parse_message_lines` then
/// drops it — recovering the session file but leaving it API-invalid. Nothing
/// validated history before sending, so the next request 400s, and the one after
/// that, permanently: the session file is intact and the session is unusable.
///
/// Repairing means synthesising the missing results. That is honest — the tool
/// genuinely produced no recorded result — and it is the only shape the API will
/// accept short of discarding the assistant turn, which would lose more.
fn repair_dangling_tool_uses(messages: &mut Vec<Message>) -> usize {
    let mut repaired = 0usize;

    for i in 0..messages.len() {
        if messages[i].role != Role::Assistant {
            continue;
        }
        let pending: Vec<String> = messages[i]
            .content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::ToolUse { id, .. } => Some(id.clone()),
                _ => None,
            })
            .collect();
        if pending.is_empty() {
            continue;
        }

        // Which of them the following user turn already answers.
        let answered: Vec<String> = messages
            .get(i + 1)
            .filter(|m| m.role == Role::User)
            .map(|m| {
                m.content
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id.clone()),
                        _ => None,
                    })
                    .collect()
            })
            .unwrap_or_default();

        let missing: Vec<String> = pending
            .into_iter()
            .filter(|id| !answered.contains(id))
            .collect();
        if missing.is_empty() {
            continue;
        }

        let stubs: Vec<ContentBlock> = missing
            .iter()
            .map(|id| ContentBlock::ToolResult {
                tool_use_id: id.clone(),
                content: vec![ToolResultContent::text(
                    "[no result recorded — the session was interrupted before this tool \
                     finished]",
                )],
                is_error: Some(true),
            })
            .collect();
        repaired += stubs.len();

        match messages.get_mut(i + 1) {
            // Extend the existing answer turn rather than inserting a second
            // user message, which would leave two user turns in a row.
            Some(next) if next.role == Role::User => {
                next.content.splice(0..0, stubs);
            }
            _ => messages.insert(
                i + 1,
                Message {
                    role: Role::User,
                    content: stubs,
                },
            ),
        }
    }

    repaired
}

/// Parse a session's JSONL body.
///
/// Split out from `load_messages` so the torn-tail and mid-file-corruption
/// behaviour can be tested against an explicit file rather than the global
/// sessions directory.
fn parse_message_lines(id: &str, content: &str) -> Result<Vec<Message>> {
    let (mut out, _) = parse_intact_lines(id, content)?;
    repair_loaded(id, &mut out);
    Ok(out)
}

/// The messages of every intact line, and whether the tail is torn (a final
/// line was dropped or the file does not end in a newline) and the file
/// must be rewritten before anything is appended to it.
fn parse_intact_lines(id: &str, content: &str) -> Result<(Vec<Message>, bool)> {
    let lines: Vec<&str> = content.lines().filter(|l| !l.trim().is_empty()).collect();
    let total = lines.len();
    let mut out = Vec::with_capacity(total);
    let mut torn = !content.is_empty() && !content.ends_with('\n');

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
                    torn = true;
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
    Ok((out, torn))
}

/// Recovery can leave an assistant `tool_use` unanswered (its `tool_result`
/// was the torn line). The API rejects that outright, so repair before the
/// history is ever sent.
fn repair_loaded(id: &str, messages: &mut Vec<Message>) {
    let repaired = repair_dangling_tool_uses(messages);
    if repaired > 0 {
        tracing::warn!(
            "session {id}: synthesised {repaired} missing tool result(s) for an \
             interrupted turn"
        );
    }
}

/// Create `dir` and its missing parents. Session files hold tool output,
/// file contents and often secrets, so on unix the directories created here
/// are the owner's only (0700), like Claude Code's `~/.claude/projects`.
async fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    builder.mode(0o700);
    builder.create(dir).await
}

/// Atomic file write: write to a sibling temp file, fsync, then rename over
/// the target. Survives mid-write crashes — the target is either the old
/// content or the new content, never a truncated splice. On unix the file is
/// the owner's only (0600) in a 0700 directory, as Claude Code keeps its
/// transcripts. For files in the sessions dir.
async fn atomic_write(path: &std::path::Path, bytes: &[u8]) -> Result<()> {
    write_atomically(path, bytes, true).await
}

/// [`atomic_write`] for a file the user chose (`/export`): new directories
/// and a new file get the usual umask-based modes, and a file it replaces
/// keeps its own.
async fn atomic_write_shared(path: &std::path::Path, bytes: &[u8]) -> Result<()> {
    write_atomically(path, bytes, false).await
}

async fn write_atomically(path: &std::path::Path, bytes: &[u8], private: bool) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("session path has no parent: {}", path.display()))?;
    if private {
        create_private_dir(parent).await?;
    } else if !parent.as_os_str().is_empty() {
        fs::create_dir_all(parent).await?;
    }
    // Replacing a user's file keeps its mode; the temp file would otherwise
    // bring its own over in the rename.
    let keep = if private {
        None
    } else {
        fs::metadata(path).await.ok().map(|m| m.permissions())
    };

    let file_name = path
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("session path has no file name: {}", path.display()))?;
    let tmp = parent.join(format!(
        ".{}.tmp.{}",
        file_name.to_string_lossy(),
        Uuid::new_v4()
    ));

    {
        let mut options = fs::OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        if private {
            options.mode(0o600);
        }
        let mut f = options.open(&tmp).await?;
        f.write_all(bytes).await?;
        f.sync_all().await?;
    }
    if let Some(perms) = keep
        && let Err(e) = fs::set_permissions(&tmp, perms).await
    {
        let _ = fs::remove_file(&tmp).await;
        return Err(e.into());
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

    /// An export goes where the user chose: it keeps the umask's mode, or
    /// the mode of the file it replaces, and new directories are not made
    /// owner-only. Session files stay 0600.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_export_keeps_ordinary_modes() {
        use super::atomic_write_shared;
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir().unwrap();
        let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        let umask = {
            let probe = dir.path().join("probe");
            std::fs::write(&probe, "").unwrap();
            0o666 & !mode(&probe)
        };

        let session = dir.path().join("s.meta");
        atomic_write(&session, b"x").await.unwrap();
        assert_eq!(mode(&session), 0o600);

        let export = dir.path().join("docs/new/session.md");
        atomic_write_shared(&export, b"x").await.unwrap();
        assert_eq!(mode(&export), 0o666 & !umask);
        assert_eq!(mode(&dir.path().join("docs/new")), 0o777 & !umask);

        let existing = dir.path().join("shared.md");
        std::fs::write(&existing, "old").unwrap();
        std::fs::set_permissions(&existing, std::fs::Permissions::from_mode(0o644)).unwrap();
        atomic_write_shared(&existing, b"new").await.unwrap();
        assert_eq!(mode(&existing), 0o644);
        assert_eq!(std::fs::read_to_string(&existing).unwrap(), "new");
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

    /// tokio's File buffers a write and reports Ok before the blocking write
    /// runs; `sync_all` then swallowed its ENOSPC, so a failed append
    /// returned Ok with a torn line on disk. `/dev/full` fails every write.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_failed_append_is_reported() {
        let mut s = Session::at_path("s", PathBuf::from("/dev/full"));
        let err = s.append(&[msg("one")]).await.unwrap_err();
        // The write's own error, not a later fsync complaint about the device.
        let io = err.downcast_ref::<std::io::Error>().expect("io error");
        assert_eq!(io.raw_os_error(), Some(28), "{err}"); // ENOSPC
    }

    /// Recovery used to drop the torn line in memory only: the next append
    /// glued onto the fragment and the following resume refused the whole
    /// session as corrupt mid-file. Same for a complete final line whose
    /// newline never made it to disk.
    #[tokio::test]
    async fn a_resumed_torn_session_still_loads_after_the_next_append() {
        for torn_tail in [Some(14), Some(usize::MAX)] {
            let d = tempfile::tempdir().unwrap();
            write_jsonl(
                d.path(),
                "s",
                &[msg("one"), msg("two"), msg("three")],
                torn_tail,
            );
            let mut s = Session::at_path("s", d.path().join("s.jsonl"));
            let mut loaded = s.load_and_heal().await.unwrap();
            loaded.push(msg("four"));
            let start = if torn_tail == Some(14) { 2 } else { 3 };
            s.append(&loaded[start..]).await.unwrap();
            let reloaded = s.load_and_heal().await.unwrap();
            assert_eq!(reloaded, loaded, "{torn_tail:?}");
        }
    }

    /// The regression: a crash mid-append leaves the final line cut off. That
    /// used to fail the entire load via `collect()`, losing the whole
    /// conversation rather than one turn.
    #[test]
    fn torn_final_line_costs_one_turn_not_the_session() {
        let d = tempfile::tempdir().unwrap();
        write_jsonl(
            d.path(),
            "s",
            &[msg("one"), msg("two"), msg("three")],
            Some(14),
        );

        let got = parse(d.path(), "s").expect("a torn tail must not fail the load");
        assert_eq!(
            got.len(),
            2,
            "complete turns survive, the torn one is dropped"
        );
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

    fn tool_use(id: &str) -> Message {
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: id.into(),
                name: "Bash".into(),
                input: serde_json::json!({"command": "ls"}),
            }],
        }
    }

    fn tool_result(id: &str) -> Message {
        Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: id.into(),
                content: vec![ToolResultContent::text("ok")],
                is_error: None,
            }],
        }
    }

    fn ids_of_results(m: &Message) -> Vec<String> {
        m.content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id.clone()),
                _ => None,
            })
            .collect()
    }

    /// The interaction between torn-line recovery and the API contract: if the
    /// line that was torn was the tool_result, recovery leaves an assistant
    /// tool_use unanswered. Nothing validated history before sending, so every
    /// later request 400s — the file is intact and the session is unusable.
    #[test]
    fn dangling_tool_use_is_repaired_so_the_session_stays_usable() {
        let mut msgs = vec![msg("hello"), tool_use("toolu_1")];
        let n = repair_dangling_tool_uses(&mut msgs);
        assert_eq!(n, 1, "the unanswered tool_use must be repaired");
        assert_eq!(msgs.len(), 3, "a user turn answering it must be appended");
        assert_eq!(msgs[2].role, Role::User);
        assert_eq!(ids_of_results(&msgs[2]), vec!["toolu_1".to_string()]);
    }

    /// Parallel tool calls: only the unanswered ones get stubs, and they join
    /// the existing user turn rather than creating two user turns in a row.
    #[test]
    fn partially_answered_turn_is_completed_in_place() {
        let mut a = tool_use("toolu_1");
        a.content.push(ContentBlock::ToolUse {
            id: "toolu_2".into(),
            name: "Read".into(),
            input: serde_json::json!({}),
        });
        let mut msgs = vec![a, tool_result("toolu_1")];

        let n = repair_dangling_tool_uses(&mut msgs);
        assert_eq!(n, 1, "only the missing one is synthesised");
        assert_eq!(
            msgs.len(),
            2,
            "must not insert a second consecutive user turn"
        );
        let mut got = ids_of_results(&msgs[1]);
        got.sort();
        assert_eq!(got, vec!["toolu_1".to_string(), "toolu_2".to_string()]);
    }

    /// A fully-answered history must be left exactly as it is.
    #[test]
    fn complete_history_is_not_modified() {
        let mut msgs = vec![msg("hi"), tool_use("toolu_1"), tool_result("toolu_1")];
        let before = msgs.len();
        assert_eq!(repair_dangling_tool_uses(&mut msgs), 0);
        assert_eq!(msgs.len(), before);
    }

    #[test]
    fn history_without_tool_use_is_untouched() {
        let mut msgs = vec![msg("a"), msg("b")];
        assert_eq!(repair_dangling_tool_uses(&mut msgs), 0);
        assert_eq!(msgs.len(), 2);
    }

    /// The end-to-end shape: a torn tail that removes the tool_result must load
    /// AND come back API-valid.
    #[test]
    fn torn_tool_result_recovers_to_a_sendable_history() {
        let d = tempfile::tempdir().unwrap();
        let msgs = vec![msg("go"), tool_use("toolu_9"), tool_result("toolu_9")];
        write_jsonl(d.path(), "s", &msgs, Some(10));

        let got = parse(d.path(), "s").expect("must load");
        let last = got.last().expect("history must not be empty");
        assert_eq!(last.role, Role::User, "must end answering the tool_use");
        assert_eq!(ids_of_results(last), vec!["toolu_9".to_string()]);
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

#[cfg(test)]
mod continue_tests {
    use super::*;
    use std::time::{Duration, UNIX_EPOCH};

    fn meta(dir: &std::path::Path, id: &str, created_at: u64) {
        let m = SessionMeta {
            id: id.into(),
            name: id.into(),
            created_at,
            preview: String::new(),
            tags: Vec::new(),
            auto_commits: Vec::new(),
            undo_position: 0,
            base_commit: None,
            timeline: Vec::new(),
            cwd: None,
            model: None,
            claude_code_session: None,
            imported_at: None,
            redo: Vec::new(),
        };
        std::fs::write(
            dir.join(format!("{id}.meta")),
            serde_json::to_string(&m).unwrap(),
        )
        .unwrap();
    }

    fn jsonl(dir: &std::path::Path, id: &str, modified: u64) {
        let path = dir.join(format!("{id}.jsonl"));
        std::fs::write(
            &path,
            "{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"hi\"}]}\n",
        )
        .unwrap();
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(UNIX_EPOCH + Duration::from_secs(modified))
            .unwrap();
    }

    /// `-c` took the newest-created session: after "work in A, quit; open
    /// and quit B" it reopened the empty B, and a session resumed and worked
    /// on today ranked below one created later.
    #[tokio::test]
    async fn continue_picks_the_last_active_session_with_messages() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        meta(d, "worked-today", 1_000);
        jsonl(d, "worked-today", 5_000);
        meta(d, "opened-and-quit", 6_000);
        meta(d, "created-later", 4_000);
        jsonl(d, "created-later", 4_000);

        let order: Vec<String> = Session::list_in(d)
            .await
            .unwrap()
            .into_iter()
            .map(|m| m.id)
            .collect();
        assert_eq!(order, ["opened-and-quit", "worked-today", "created-later"]);
        assert_eq!(
            Session::most_recent_in(d).await.as_deref(),
            Some("worked-today")
        );
    }

    /// cleanupPeriodDays compared created_at, so a month-old session used
    /// yesterday was deleted, as was the one `--resume` was about to open.
    #[tokio::test]
    async fn cleanup_keeps_recently_active_and_resumed_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        meta(d, "old-but-active", 1_000);
        jsonl(d, "old-but-active", 9_000);
        meta(d, "idle", 1_000);
        jsonl(d, "idle", 2_000);
        meta(d, "idle-resumed", 1_000);
        jsonl(d, "idle-resumed", 2_000);

        Session::prune_inactive_in(d, 5_000, Some("idle-resumed")).await;

        let mut left: Vec<String> = Session::list_in(d)
            .await
            .unwrap()
            .into_iter()
            .map(|m| m.id)
            .collect();
        left.sort();
        assert_eq!(left, ["idle-resumed", "old-but-active"]);
        assert!(!d.join("idle.jsonl").exists());
        assert!(!d.join("idle.meta").exists());
    }

    /// Delete and cleanupPeriodDays removed only .jsonl/.meta, so every
    /// deleted session kept its snapshots/turn-N copies of edited files.
    #[tokio::test]
    async fn delete_and_cleanup_remove_the_snapshot_dir() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        let snap = |id: &str| {
            let turn = d.join(id).join("snapshots").join("turn-1");
            std::fs::create_dir_all(&turn).unwrap();
            std::fs::write(turn.join("main.rs"), "fn main() {}").unwrap();
        };
        for id in ["deleted", "idle", "kept"] {
            meta(d, id, 1_000);
            jsonl(d, id, if id == "kept" { 9_000 } else { 2_000 });
            snap(id);
        }

        Session::delete_in(d, "deleted").await.unwrap();
        assert!(!d.join("deleted.meta").exists());
        assert!(!d.join("deleted").exists());

        Session::prune_inactive_in(d, 5_000, None).await;
        assert!(!d.join("idle").exists());
        assert!(d.join("kept").join("snapshots").join("turn-1").exists());
    }

    /// The pruned id comes from the .meta body; ".." or "" must not turn
    /// into remove_dir_all on sessions_dir or its parent.
    #[tokio::test]
    async fn cleanup_ignores_a_tampered_meta_id() {
        let root = tempfile::tempdir().unwrap();
        let d = root.path().join("sessions");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(root.path().join("precious"), "x").unwrap();
        for (file, id) in [
            ("dotdot", ".."),
            ("empty", ""),
            ("drive", "C:"),
            ("dot", "."),
        ] {
            std::fs::write(
                d.join(format!("{file}.meta")),
                format!(r#"{{"id":"{id}","name":"n","created_at":1,"preview":""}}"#),
            )
            .unwrap();
        }
        Session::prune_inactive_in(&d, 5_000, None).await;
        assert!(root.path().join("precious").exists());
        assert!(d.exists());
        assert!(Session::delete_in(&d, "..").await.is_err());
        assert!(Session::delete_in(&d, "C:").await.is_err());
        assert!(d.exists());
        assert!(super::is_safe_session_id(
            "0b6c1d2e-3f40-4a5b-8c6d-7e8f9a0b1c2d"
        ));
    }

    #[tokio::test]
    async fn continue_with_only_empty_sessions_starts_fresh() {
        let dir = tempfile::tempdir().unwrap();
        meta(dir.path(), "empty", 1_000);
        assert_eq!(Session::most_recent_in(dir.path()).await, None);
    }

    /// `--session` is documented as taking an id prefix, but the prefix was
    /// passed on as the id and the session was never found.
    #[tokio::test]
    async fn session_flag_resolves_a_unique_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        meta(d, "ab12-first", 1_000);
        meta(d, "ab34-second", 2_000);
        meta(d, "ab34", 3_000);
        let find = |p: &'static str| async move { Session::resolve_in(d, p).await.ok() };
        assert_eq!(find("ab1").await.as_deref(), Some("ab12-first"));
        assert_eq!(find("ab34").await.as_deref(), Some("ab34"), "exact id wins");
        assert_eq!(find("ab").await, None, "ambiguous");
        assert_eq!(find("zz").await, None);
        assert_eq!(find("").await, None);
    }
}

#[cfg(test)]
mod resolve_tests {
    use super::*;

    async fn write_meta(dir: &std::path::Path, id: &str, name: &str) {
        let meta = SessionMeta {
            id: id.into(),
            name: name.into(),
            created_at: 0,
            preview: "p".into(),
            tags: Vec::new(),
            auto_commits: Vec::new(),
            undo_position: 0,
            base_commit: None,
            timeline: Vec::new(),
            cwd: None,
            model: None,
            claude_code_session: None,
            imported_at: None,
            redo: Vec::new(),
        };
        let body = serde_json::to_string(&meta).unwrap();
        atomic_write(&dir.join(format!("{id}.meta")), body.as_bytes())
            .await
            .unwrap();
    }

    /// `--session <short id>` read `<short id>.meta` verbatim, failed, and
    /// dropped the user into a fresh session. It now resolves like /resume.
    #[tokio::test]
    async fn short_ids_and_names_resolve_and_misses_are_errors() {
        let d = tempfile::tempdir().unwrap();
        let a = "3f2a9c1e-0000-4000-8000-000000000001";
        let b = "3f2a9c1e-0000-4000-8000-000000000002";
        let c = "77aa0000-0000-4000-8000-000000000003";
        write_meta(d.path(), a, "alpha").await;
        write_meta(d.path(), b, "beta").await;
        write_meta(d.path(), c, "gamma").await;

        let r = |q: &'static str| Session::resolve_in(d.path(), q);
        assert_eq!(r(a).await.unwrap(), a);
        assert_eq!(r("77aa0000").await.unwrap(), c);
        assert_eq!(r("beta").await.unwrap(), b);
        let many = r("3f2a9c1e").await.unwrap_err().to_string();
        assert!(many.contains("Multiple") && many.contains(a) && many.contains(b));
        assert!(
            r("deadbeef")
                .await
                .unwrap_err()
                .to_string()
                .contains("No saved session")
        );
        assert!(r("  ").await.is_err());
    }
}

#[cfg(test)]
mod fork_tests {
    use super::*;

    /// A fork of an imported session claimed the same Claude Code session:
    /// `--list` marked whichever came last, a re-run skipped a deleted
    /// import, and the fork got the import's cleanup grace period.
    #[tokio::test]
    async fn a_fork_does_not_claim_the_import() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = Session::create_in(dir.path(), "orig".into()).await.unwrap();
        s.meta.claude_code_session = Some("cc-1234".into());
        s.meta.imported_at = Some(1_700_000_000);
        s.save_meta().await.unwrap();
        s.fork(&[]).await.unwrap();
        assert_ne!(s.id, "orig");
        assert_eq!(s.meta.claude_code_session, None);
        assert_eq!(s.meta.imported_at, None);
        let saved: SessionMeta = serde_json::from_str(
            &std::fs::read_to_string(dir.path().join(format!("{}.meta", s.id))).unwrap(),
        )
        .unwrap();
        assert_eq!(saved.claude_code_session, None);
        let orig: SessionMeta =
            serde_json::from_str(&std::fs::read_to_string(dir.path().join("orig.meta")).unwrap())
                .unwrap();
        assert_eq!(orig.claude_code_session.as_deref(), Some("cc-1234"));
    }
}
