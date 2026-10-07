//! The undo timeline: `/undo`, `/redo` and `/rewind` move the files (the
//! shadow-ref snapshots in `autocommit`) and the conversation together, one
//! turn at a time. A turn is a prompt and everything after it; its
//! [`TurnMark`] records where the files were when it started.

use super::*;
use crate::session::{TurnMark, UndoneTurn, prompt_fingerprint};

/// Put a new turn on the timeline. Any new turn ends what /redo could
/// bring back.
pub(super) fn begin_turn(session: &mut Session, prompt: &Message) {
    session.meta.redo.clear();
    session.meta.timeline.push(TurnMark {
        prompt: prompt_fingerprint(prompt),
        before: session.meta.undo_position,
    });
}

/// Indices into `messages` of each prompt, oldest first.
fn prompt_indices(messages: &[Message]) -> Vec<usize> {
    messages
        .iter()
        .enumerate()
        .filter(|(_, m)| is_prompt(m))
        .map(|(i, _)| i)
        .collect()
}

/// The marks that pair with the newest prompts, oldest first: the last
/// `n` returned belong to the last `n` prompts. Marks saved for prompts
/// that never reached the transcript (a crash between the two writes) are
/// skipped; pairing stops at the first prompt without its own mark (turns
/// before a compaction, an import, or a session from before the timeline),
/// so a mark never stands in for another turn.
fn paired_marks(messages: &[Message], prompts: &[usize], timeline: &[TurnMark]) -> Vec<TurnMark> {
    let fingerprints: Vec<String> = prompts
        .iter()
        .map(|&i| prompt_fingerprint(&messages[i]))
        .collect();
    let Some(newest) = fingerprints.last() else {
        return Vec::new();
    };
    let Some(end) = timeline.iter().rposition(|m| m.prompt == *newest) else {
        return Vec::new();
    };
    let count = timeline[..=end]
        .iter()
        .rev()
        .zip(fingerprints.iter().rev())
        .take_while(|(m, fp)| m.prompt == **fp)
        .count();
    timeline[end + 1 - count..=end].to_vec()
}

/// What `/undo n` does: where the conversation is cut, the file position
/// it returns to, and the turns it takes off (oldest first).
#[derive(Debug, PartialEq)]
struct UndoPlan {
    cut: usize,
    target: usize,
    undone: Vec<UndoneTurn>,
    /// Marks left on the timeline.
    keep: usize,
    /// Undone turns with no mark: only their conversation moves.
    unmarked: usize,
}

fn plan_undo(
    messages: &[Message],
    timeline: &[TurnMark],
    position: usize,
    n: usize,
) -> Option<UndoPlan> {
    let prompts = prompt_indices(messages);
    let n = n.min(prompts.len());
    if n == 0 {
        return None;
    }
    let marks = paired_marks(messages, &prompts, timeline);
    let first_marked = prompts.len() - marks.len();
    let first = prompts.len() - n;
    // Newest to oldest: each turn ends where the next one started, the
    // newest where the files are now. A turn without a mark moved nothing.
    let mut after = position;
    let mut undone = Vec::with_capacity(n);
    for i in (first..prompts.len()).rev() {
        let start = prompts[i];
        let end = prompts.get(i + 1).copied().unwrap_or(messages.len());
        let before = match i.checked_sub(first_marked) {
            Some(m) => marks[m].before,
            None => after,
        };
        undone.push(UndoneTurn {
            mark: TurnMark {
                prompt: prompt_fingerprint(&messages[start]),
                before,
            },
            after,
            messages: messages[start..end].to_vec(),
        });
        after = before;
    }
    undone.reverse();
    let unmarked = first_marked.saturating_sub(first);
    Some(UndoPlan {
        cut: prompts[first],
        target: after,
        undone,
        keep: marks.len() - (n - unmarked),
        unmarked,
    })
}

/// Whether file changes are on the timeline at all.
fn files_tracked(config: &Config) -> bool {
    config.auto_commit.enabled && oxideclaw::autocommit::is_git_repo(&config.cwd)
}

const UNTRACKED_NOTE: &str = "File changes are tracked only inside a git repository with \
                              auto-commit on, so only the conversation moved.";

/// Move the files from the current position to `target`, refusing to
/// overwrite anything changed since the current snapshot. `Ok(None)` when
/// there is nothing to move.
async fn move_files(
    session: &Session,
    config: &Config,
    target: usize,
) -> anyhow::Result<Option<oxideclaw::autocommit::RestoreReport>> {
    if !files_tracked(config) || target == session.meta.undo_position {
        return Ok(None);
    }
    oxideclaw::autocommit::restore_from_blocking(
        config.cwd.clone(),
        session.id.clone(),
        session.meta.auto_commits.clone(),
        session.meta.undo_position,
        target,
    )
    .await
    .map(Some)
}

/// What happened to the files, for the /undo and /redo summary line.
fn files_note(
    config: &Config,
    report: Option<&oxideclaw::autocommit::RestoreReport>,
    unmarked: usize,
) -> String {
    if !files_tracked(config) {
        return format!(" {UNTRACKED_NOTE}");
    }
    let mut note = match report {
        Some(r) => format!(
            " Files restored ({} in the tree{}).{}",
            r.files_restored,
            r.removed_note(),
            r.saved_edits_note()
        ),
        None => " Files unchanged: no file changes were recorded.".to_string(),
    };
    if unmarked > 0 {
        note.push_str(&format!(
            " {unmarked} of these turn{} predate the undo timeline; their files were left as they are.",
            if unmarked == 1 { "" } else { "s" }
        ));
    }
    note
}

fn turns(n: usize) -> String {
    format!("{n} turn{}", if n == 1 { "" } else { "s" })
}

/// `/undo n` (and `/rewind`): take the last `n` turns off the conversation
/// and the saved session, and put the files back to where they were before
/// the oldest of them. Nothing changes if the files cannot be moved.
pub(super) async fn undo(
    app: &mut App,
    messages: &mut Vec<Message>,
    session: &mut Session,
    saved_count: &mut usize,
    config: &Config,
    n: usize,
) {
    if app.is_loading {
        app.entries.push(ChatEntry::system(
            "[undo] cannot undo while an assistant turn is running",
        ));
        return;
    }
    let Some(plan) = plan_undo(
        messages,
        &session.meta.timeline,
        session.meta.undo_position,
        n,
    ) else {
        app.entries.push(ChatEntry::system(
            "[undo] nothing to undo in this conversation",
        ));
        return;
    };
    let report = match move_files(session, config, plan.target).await {
        Ok(report) => report,
        Err(e) => {
            app.entries.push(ChatEntry::error(format!("[undo] {e}")));
            return;
        }
    };
    let n = plan.undone.len();
    messages.truncate(plan.cut);
    messages.shrink_to_fit();
    session.meta.timeline.truncate(plan.keep);
    // The next one to redo goes last.
    session.meta.redo.extend(plan.undone.into_iter().rev());
    if report.is_some() {
        session.meta.undo_position = plan.target;
    }
    // Transcript first: a crash before the meta is saved then leaves marks
    // ahead of the transcript, which pairing skips.
    if let Err(e) = rewrite_session_history(
        session,
        messages,
        !config.no_session_persistence,
        saved_count,
    )
    .await
    {
        tracing::warn!("undo: session rewrite failed: {e}");
        app.entries.push(ChatEntry::error(format!(
            "[undo] could not rewrite the session file ({e}); a resume may still show the undone turns."
        )));
    }
    if let Err(e) = session.save_meta().await {
        tracing::warn!("[undo] failed to save meta: {e}");
    }

    // Display: drop everything from the n-th last prompt on.
    let user_entries: Vec<usize> = app
        .entries
        .iter()
        .enumerate()
        .filter(|(_, e)| matches!(e.kind, crate::tui::app::EntryKind::User))
        .map(|(i, _)| i)
        .collect();
    if user_entries.len() >= n {
        app.entries.truncate(user_entries[user_entries.len() - n]);
    } else {
        app.entries = entries_from_messages(messages);
    }
    app.entries.push(ChatEntry::system(format!(
        "[undo] Undid {}.{} /redo brings {} back.",
        turns(n),
        files_note(config, report.as_ref(), plan.unmarked),
        if n == 1 { "it" } else { "them" }
    )));
}

/// `/redo n`: put back the last `n` turns /undo took off, conversation and
/// files, oldest first.
pub(super) async fn redo(
    app: &mut App,
    messages: &mut Vec<Message>,
    session: &mut Session,
    saved_count: &mut usize,
    config: &Config,
    n: usize,
) {
    if app.is_loading {
        app.entries.push(ChatEntry::system(
            "[redo] cannot redo while an assistant turn is running",
        ));
        return;
    }
    let n = n.min(session.meta.redo.len());
    if n == 0 {
        app.entries
            .push(ChatEntry::system("[redo] nothing to redo"));
        return;
    }
    let redone: Vec<UndoneTurn> = session.meta.redo.iter().rev().take(n).cloned().collect();
    let target = redone
        .last()
        .map_or(session.meta.undo_position, |t| t.after);
    let report = match move_files(session, config, target).await {
        Ok(report) => report,
        Err(e) => {
            app.entries.push(ChatEntry::error(format!("[redo] {e}")));
            return;
        }
    };
    let keep = session.meta.redo.len() - n;
    session.meta.redo.truncate(keep);
    if report.is_some() {
        session.meta.undo_position = target;
    }
    let first_new = messages.len();
    for turn in redone {
        session.meta.timeline.push(turn.mark);
        messages.extend(turn.messages);
    }
    // Meta first: marks may run ahead of the transcript, never behind.
    if let Err(e) = session.save_meta().await {
        tracing::warn!("[redo] failed to save meta: {e}");
    }
    if let Err(e) = rewrite_session_history(
        session,
        messages,
        !config.no_session_persistence,
        saved_count,
    )
    .await
    {
        tracing::warn!("redo: session rewrite failed: {e}");
        app.entries.push(ChatEntry::error(format!(
            "[redo] could not rewrite the session file ({e}); a resume may not show the redone turns."
        )));
    }
    app.entries
        .extend(entries_from_messages(&messages[first_new..]));
    app.entries.push(ChatEntry::system(format!(
        "[redo] Redid {}.{}",
        turns(n),
        files_note(config, report.as_ref(), 0)
    )));
}

/// `/rewind` with no count: pick the turn to go back to. Row `i` undoes
/// the `i` newest turns; the last row goes back to the start.
pub(super) fn open_rewind_picker(app: &mut App, messages: &[Message]) {
    let prompts = prompt_indices(messages);
    if prompts.is_empty() {
        app.entries.push(ChatEntry::system(
            "[undo] nothing to undo in this conversation",
        ));
        return;
    }
    let preview = |m: &Message| -> String {
        let text = m
            .content
            .iter()
            .find_map(|b| match b {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .unwrap_or("");
        let line = text.lines().next().unwrap_or("").trim();
        if line.chars().count() > 60 {
            format!("{}…", line.chars().take(59).collect::<String>())
        } else {
            line.to_string()
        }
    };
    let total = prompts.len();
    let mut labels = Vec::with_capacity(total + 1);
    let mut undo_counts = Vec::with_capacity(total + 1);
    for (back, &i) in prompts.iter().rev().enumerate() {
        let current = if back == 0 { "  ← current" } else { "" };
        labels.push(format!(
            "turn {}  ·  {}{current}",
            total - back,
            preview(&messages[i])
        ));
        undo_counts.push(back);
    }
    labels.push("start of the conversation".to_string());
    undo_counts.push(total);
    // The body is what is drawn; the picker highlights lines starting "N."
    let body = std::iter::once(
        "Rewind to the end of (↑/↓ then Enter; later turns are undone, files included):\n"
            .to_string(),
    )
    .chain(
        labels
            .iter()
            .enumerate()
            .map(|(i, l)| format!("  {}. {l}", i + 1)),
    )
    .collect::<Vec<_>>()
    .join("\n");
    app.overlay = Some(Overlay::with_items("rewind", body, labels));
    app.pending_rewind = Some(undo_counts);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::process::Command;

    fn git(repo: &Path, args: &[&str]) {
        let ok = Command::new("git")
            .args(args)
            .current_dir(repo)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .stdout(std::process::Stdio::null())
            .status()
            .unwrap()
            .success();
        assert!(ok, "git {args:?}");
    }

    /// A conversation in a temp work tree with its session saved in a temp
    /// sessions dir: nothing here reads or writes the real HOME.
    struct Harness {
        repo: tempfile::TempDir,
        sessions: tempfile::TempDir,
        app: App,
        messages: Vec<Message>,
        session: Session,
        saved: usize,
        config: Config,
    }

    impl Harness {
        /// `a.txt` = "a0" committed; `git` false leaves the dir a plain one.
        async fn new(git_repo: bool) -> Self {
            let repo = tempfile::tempdir().unwrap();
            std::fs::write(repo.path().join("a.txt"), "a0\n").unwrap();
            if git_repo {
                git(repo.path(), &["init", "-q"]);
                for (k, v) in [
                    ("user.name", "oxideclaw-test"),
                    ("user.email", "noreply@oxideclaw.local"),
                    ("commit.gpgsign", "false"),
                    ("core.autocrlf", "false"),
                ] {
                    git(repo.path(), &["config", k, v]);
                }
                git(repo.path(), &["add", "-A"]);
                git(repo.path(), &["commit", "-q", "-m", "base"]);
                oxideclaw::autocommit::pin_filters(repo.path()).unwrap();
            }
            let sessions = tempfile::tempdir().unwrap();
            let session = Session::create_in(sessions.path(), "s1".into())
                .await
                .unwrap();
            let config = Config {
                cwd: repo.path().to_path_buf(),
                ..Config::default()
            };
            Self {
                app: App::new("claude-sonnet-4-6", repo.path()),
                repo,
                sessions,
                messages: Vec::new(),
                session,
                saved: 0,
                config,
            }
        }

        /// One turn as the run loop drives it: prompt on the timeline, the
        /// agent's edits, then `Done` saves the transcript and snapshots.
        async fn turn(&mut self, prompt: &str, edits: &[(&str, &str)]) {
            begin_agent_turn(&mut self.session, &self.config).await;
            self.app.entries.push(ChatEntry::user(prompt));
            push_prompt_turn(
                &mut self.messages,
                vec![ContentBlock::Text {
                    text: prompt.into(),
                }],
                &mut self.session,
            )
            .await;
            for (rel, body) in edits {
                self.write(rel, body);
            }
            self.messages.push(Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: format!("did {prompt}"),
                }],
            });
            self.session
                .append(&self.messages[self.saved..])
                .await
                .unwrap();
            self.saved = self.messages.len();
            let meta = &mut self.session.meta;
            let (mut commits, mut pos) = (meta.auto_commits.clone(), meta.undo_position);
            oxideclaw::autocommit::snapshot_turn_raw(
                &self.config.cwd,
                "oxideclaw:",
                "s1",
                prompt,
                pos as u32 + 1,
                &mut commits,
                &mut pos,
                meta.base_commit.as_deref(),
            )
            .unwrap();
            meta.auto_commits = commits;
            meta.undo_position = pos;
            self.session.save_meta().await.unwrap();
        }

        async fn undo(&mut self, n: usize) {
            undo(
                &mut self.app,
                &mut self.messages,
                &mut self.session,
                &mut self.saved,
                &self.config,
                n,
            )
            .await;
        }

        async fn redo(&mut self, n: usize) {
            redo(
                &mut self.app,
                &mut self.messages,
                &mut self.session,
                &mut self.saved,
                &self.config,
                n,
            )
            .await;
        }

        fn write(&self, rel: &str, body: &str) {
            std::fs::write(self.repo.path().join(rel), body).unwrap();
        }

        fn read(&self, rel: &str) -> Option<String> {
            std::fs::read_to_string(self.repo.path().join(rel)).ok()
        }

        fn last_note(&self) -> String {
            self.app
                .entries
                .last()
                .map(|e| e.text.clone())
                .unwrap_or_default()
        }

        /// The prompts of the live conversation and of the saved session,
        /// which must always agree.
        async fn prompts(&self) -> Vec<String> {
            let texts = |msgs: &[Message]| -> Vec<String> {
                msgs.iter()
                    .filter(|m| is_prompt(m))
                    .filter_map(|m| match &m.content[0] {
                        ContentBlock::Text { text } => Some(text.clone()),
                        _ => None,
                    })
                    .collect()
            };
            let (_, saved) = Session::resume_in(self.sessions.path(), "s1")
                .await
                .unwrap();
            assert_eq!(
                texts(&saved),
                texts(&self.messages),
                "saved session drifted"
            );
            texts(&self.messages)
        }

        /// a.txt edited in turn 1 and 3, b.txt created in turn 2.
        async fn three_turns(&mut self) {
            self.turn("one", &[("a.txt", "a1\n")]).await;
            self.turn("two", &[("b.txt", "b2\n")]).await;
            self.turn("three", &[("a.txt", "a3\n")]).await;
        }
    }

    #[tokio::test]
    async fn undo_two_turns_restores_files_and_drops_them_from_the_conversation() {
        let mut h = Harness::new(true).await;
        h.three_turns().await;

        h.undo(2).await;

        assert_eq!(h.prompts().await, vec!["one"]);
        assert_eq!(h.read("a.txt").as_deref(), Some("a1\n"));
        assert_eq!(h.read("b.txt"), None, "turn 2 created it");
        assert_eq!(h.messages.len(), 2, "turn 1's reply stays");
        let note = h.last_note();
        assert!(note.starts_with("[undo] Undid 2 turns."), "{note}");
        let users = h
            .app
            .entries
            .iter()
            .filter(|e| matches!(e.kind, crate::tui::app::EntryKind::User))
            .count();
        assert_eq!(users, 1, "the undone prompts are gone from the screen");
    }

    #[tokio::test]
    async fn redo_puts_back_one_turn_at_a_time_in_order() {
        let mut h = Harness::new(true).await;
        h.three_turns().await;
        h.undo(2).await;

        h.redo(1).await;
        assert_eq!(h.prompts().await, vec!["one", "two"]);
        assert_eq!(h.read("a.txt").as_deref(), Some("a1\n"));
        assert_eq!(h.read("b.txt").as_deref(), Some("b2\n"));

        h.redo(1).await;
        assert_eq!(h.prompts().await, vec!["one", "two", "three"]);
        assert_eq!(h.read("a.txt").as_deref(), Some("a3\n"));
        assert_eq!(h.messages.len(), 6, "each turn back with its reply");

        h.redo(1).await;
        assert_eq!(h.last_note(), "[redo] nothing to redo");
        // Back on the timeline: /undo works on the redone turns again.
        h.undo(1).await;
        assert_eq!(h.prompts().await, vec!["one", "two"]);
        assert_eq!(h.read("a.txt").as_deref(), Some("a1\n"));
    }

    #[tokio::test]
    async fn a_new_turn_clears_redo() {
        let mut h = Harness::new(true).await;
        h.three_turns().await;
        h.undo(1).await;
        assert_eq!(h.session.meta.redo.len(), 1);

        h.turn("four", &[("c.txt", "c4\n")]).await;
        h.redo(1).await;

        assert_eq!(h.last_note(), "[redo] nothing to redo");
        assert_eq!(h.prompts().await, vec!["one", "two", "four"]);
        assert_eq!(h.read("a.txt").as_deref(), Some("a1\n"));
        // And the new turn undoes back to where the undo left the files.
        h.undo(1).await;
        assert_eq!(h.read("c.txt"), None);
        assert_eq!(h.read("b.txt").as_deref(), Some("b2\n"));
        assert_eq!(h.prompts().await, vec!["one", "two"]);
    }

    /// The /rewind picker's "turn 1" row is /undo 2: same files, same
    /// conversation, same saved session.
    #[tokio::test]
    async fn rewind_to_turn_one_equals_undo_two() {
        let mut picked = Harness::new(true).await;
        picked.three_turns().await;
        open_rewind_picker(&mut picked.app, &picked.messages);
        let overlay = picked.app.overlay.as_ref().expect("picker opened");
        assert_eq!(overlay.title, "rewind");
        let row = overlay
            .selectable_ids
            .iter()
            .position(|l| l.starts_with("turn 1 "))
            .unwrap();
        let n = picked.app.pending_rewind.as_ref().unwrap()[row];
        picked.app.overlay = None;
        picked.undo(n).await;

        let mut undone = Harness::new(true).await;
        undone.three_turns().await;
        undone.undo(2).await;

        assert_eq!(picked.prompts().await, undone.prompts().await);
        assert_eq!(picked.messages, undone.messages);
        for f in ["a.txt", "b.txt"] {
            assert_eq!(picked.read(f), undone.read(f), "{f}");
        }
        assert_eq!(
            picked.session.meta.undo_position,
            undone.session.meta.undo_position
        );
    }

    /// The picker row is chosen with a key like any other picker.
    #[tokio::test]
    async fn rewind_picker_row_undoes_through_the_key_handler() {
        let mut h = Harness::new(true).await;
        h.three_turns().await;
        open_rewind_picker(&mut h.app, &h.messages);
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut client =
            ApiBackend::Anthropic(crate::api::ClaudeClient::new("sk-ant-test").unwrap());
        let mut system_prompt = String::new();
        let todo_state = TodoState::default();
        let spawn_registry = crate::spawn::new_registry();
        handle_key(KeyCtx {
            key: crossterm::event::KeyEvent::new(KeyCode::Char('3'), KeyModifiers::NONE),
            app: &mut h.app,
            messages: &mut h.messages,
            client: &mut client,
            tools: &[],
            config: &mut h.config,
            perm_state: &PermissionState::new(false, &[], &[]),
            skills: &std::collections::HashMap::new(),
            system_prompt: &mut system_prompt,
            tx: &tx,
            todo_state: &todo_state,
            session: &mut h.session,
            saved_count: &mut h.saved,
            mcp_statuses: &[],
            spawn_registry: &spawn_registry,
        })
        .await
        .unwrap();
        assert!(h.app.pending_rewind.is_none());
        assert_eq!(h.prompts().await, vec!["one"]);
        assert_eq!(h.read("a.txt").as_deref(), Some("a1\n"));
    }

    /// A file edited by hand after the turns being undone is never
    /// overwritten: the whole undo is refused, conversation included.
    #[tokio::test]
    async fn a_hand_edited_file_refuses_the_undo() {
        let mut h = Harness::new(true).await;
        h.three_turns().await;
        h.write("b.txt", "mine\n");

        h.undo(2).await;

        let note = h.last_note();
        assert!(note.contains("b.txt"), "{note}");
        assert!(note.contains("nothing was changed"), "{note}");
        assert_eq!(h.read("b.txt").as_deref(), Some("mine\n"));
        assert_eq!(h.read("a.txt").as_deref(), Some("a3\n"));
        assert_eq!(h.prompts().await, vec!["one", "two", "three"]);
        assert!(h.session.meta.redo.is_empty());
        assert_eq!(h.session.meta.undo_position, 3);
    }

    #[tokio::test]
    async fn outside_git_undo_and_redo_move_the_conversation_only() {
        let mut h = Harness::new(false).await;
        h.turn("one", &[("a.txt", "a1\n")]).await;
        h.turn("two", &[("a.txt", "a2\n")]).await;

        h.undo(1).await;
        assert_eq!(h.prompts().await, vec!["one"]);
        assert_eq!(h.read("a.txt").as_deref(), Some("a2\n"), "files untouched");
        let note = h.last_note();
        assert!(
            note.contains(
                "File changes are tracked only inside a git repository with auto-commit on"
            ),
            "{note}"
        );
        assert_eq!(note.lines().count(), 1, "one line: {note}");

        h.redo(1).await;
        assert_eq!(h.prompts().await, vec!["one", "two"]);
        assert!(h.last_note().contains("only inside a git repository"));
    }

    #[tokio::test]
    async fn auto_commit_off_moves_the_conversation_only() {
        let mut h = Harness::new(true).await;
        h.config.auto_commit.enabled = false;
        h.turn("one", &[("a.txt", "a1\n")]).await;
        h.undo(1).await;
        assert!(h.prompts().await.is_empty());
        assert_eq!(h.read("a.txt").as_deref(), Some("a1\n"));
        assert!(h.last_note().contains("auto-commit on"));
    }

    /// Resume: the timeline and redo come back with the session, so /undo
    /// and /redo of the resumed run move exactly its own turns.
    #[tokio::test]
    async fn a_resumed_session_undoes_and_redoes_its_own_turns() {
        let mut h = Harness::new(true).await;
        h.three_turns().await;
        h.undo(1).await;

        let (session, messages) = Session::resume_in(h.sessions.path(), "s1").await.unwrap();
        h.saved = messages.len();
        h.messages = messages;
        h.session = session;
        h.app = App::new("claude-sonnet-4-6", h.repo.path());

        h.redo(1).await;
        assert_eq!(h.prompts().await, vec!["one", "two", "three"]);
        assert_eq!(h.read("a.txt").as_deref(), Some("a3\n"));
        h.undo(2).await;
        assert_eq!(h.prompts().await, vec!["one"]);
        assert_eq!(h.read("a.txt").as_deref(), Some("a1\n"));
        assert_eq!(h.read("b.txt"), None);
    }

    /// Turns from before the timeline (an older session, a compacted
    /// summary, an import) have no mark: undoing them moves only the
    /// conversation, never some other turn's files.
    #[tokio::test]
    async fn turns_without_a_mark_never_move_files() {
        let mut h = Harness::new(true).await;
        h.turn("one", &[("a.txt", "a1\n")]).await;
        h.turn("two", &[("a.txt", "a2\n")]).await;
        // Turn one as an older version saved it: in the transcript, with no
        // mark on the timeline.
        h.session.meta.timeline.remove(0);

        h.undo(2).await;

        assert!(h.prompts().await.is_empty());
        assert_eq!(
            h.read("a.txt").as_deref(),
            Some("a1\n"),
            "only turn two's files"
        );
        assert!(
            h.last_note().contains("predate the undo timeline"),
            "{}",
            h.last_note()
        );
    }

    /// A mark saved for a prompt the transcript never got (a crash between
    /// the two writes) is skipped, not paired with the previous prompt.
    #[test]
    fn marks_ahead_of_the_transcript_are_skipped() {
        let prompt = |t: &str| Message {
            role: Role::User,
            content: vec![ContentBlock::Text { text: t.into() }],
        };
        let reply = Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Text { text: "ok".into() }],
        };
        let messages = vec![prompt("one"), reply.clone(), prompt("two"), reply];
        let mark = |m: &Message, before| TurnMark {
            prompt: prompt_fingerprint(m),
            before,
        };
        let timeline = vec![
            mark(&messages[0], 0),
            mark(&messages[2], 1),
            mark(&prompt("lost"), 2),
        ];
        let plan = plan_undo(&messages, &timeline, 2, 1).unwrap();
        assert_eq!(plan.target, 1);
        assert_eq!(plan.cut, 2);
        assert_eq!(plan.keep, 1);
        assert_eq!(plan.unmarked, 0);
        assert_eq!(plan.undone[0].after, 2);
        assert_eq!(plan_undo(&messages, &timeline, 2, 0), None);
        assert_eq!(plan_undo(&[], &timeline, 2, 1), None);
    }
}
