//! `/` command handlers — split out of `commands/mod.rs` mechanically.

use super::*;

pub(super) fn cmd_copy(ctx: &CommandContext) -> CommandAction {
    let Some(text) = ctx.last_assistant else {
        return CommandAction::Message("No assistant message to copy yet.".into());
    };
    clipboard_write(text)
}

/// Clipboard tools in the order they are tried. Every one gets the text on
/// stdin: as an argv element (how wl-copy used to be called) a reply starting
/// with `-` is parsed as flags, one over 128 KiB fails with E2BIG, and the
/// forked wl-copy server exposes it in /proc/<pid>/cmdline to other users.
const CLIPBOARD_TOOLS: &[(&str, &[&str])] = &[
    ("wl-copy", &[]),                        // Wayland
    ("xclip", &["-selection", "clipboard"]), // X11
    ("xsel", &["--clipboard", "--input"]),   // X11
    ("pbcopy", &[]),                         // macOS
    ("clip.exe", &[]),                       // WSL / Windows
];

/// Pipe `text` into `cmd`. `wait()` closes stdin first, so tools that read to
/// EOF (wl-copy forks its selection server only then) see the end of input.
fn pipe_to(cmd: &str, args: &[&str], text: &str) -> std::io::Result<std::process::ExitStatus> {
    use std::io::Write;
    // Quiet: an error ("Can't open display") would print over the TUI frame,
    // and the exit status already says whether it worked.
    let mut child = std::process::Command::new(cmd)
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    child
        .stdin
        .as_mut()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "stdin not available"))?
        .write_all(text.as_bytes())?;
    child.wait()
}

/// What to install when no clipboard tool worked.
pub const CLIPBOARD_INSTALL_HINT: &str =
    "Install one of: wl-clipboard (Wayland), xclip or xsel (X11), pbcopy (macOS), clip.exe (WSL).";

/// Put `text` on the system clipboard with the first tool that succeeds.
/// Blocking; /copy and /share clip both go through here so neither is
/// limited to the Linux tools.
pub fn copy_to_clipboard(text: &str) -> bool {
    copy_with(CLIPBOARD_TOOLS, text)
}

fn copy_with(tools: &[(&str, &[&str])], text: &str) -> bool {
    tools
        .iter()
        .any(|(cmd, args)| pipe_to(cmd, args, text).is_ok_and(|s| s.success()))
}

/// Write text to the system clipboard. Tries all known clipboard tools across platforms.
pub fn clipboard_write(text: &str) -> CommandAction {
    if copy_to_clipboard(text) {
        CommandAction::Message("Copied to clipboard.".into())
    } else {
        CommandAction::Message(format!(
            "Could not copy to clipboard.\n{CLIPBOARD_INSTALL_HINT}"
        ))
    }
}

pub(super) fn cmd_session(args: &str) -> CommandAction {
    let (sub, sub_args) = split_first_word(args);
    match sub {
        "list" | "" => CommandAction::ListSessions,
        // Deletes every saved conversation in every project, with no way
        // back, so a guessed `/session clear` must not be enough.
        "clear-all" | "clearall" if sub_args.trim() == "--yes" => CommandAction::ClearAllSessions,
        "clear-all" | "clearall" | "clear" => CommandAction::Message(
            "This permanently deletes ALL saved sessions (every project) except the current one.\n\
             Re-run as: /session clear-all --yes"
                .into(),
        ),
        "search" => {
            if sub_args.is_empty() {
                CommandAction::Message("Usage: /session search <query>".into())
            } else {
                CommandAction::SearchSessions(sub_args.to_string())
            }
        }
        "delete" => {
            // The hint is a copy-pasted `rm -rf`, so the prefix must be one
            // plain id token: `a /` would print `rm -rf ... /*/`, and `*` or
            // `../x` would reach every session or outside the sessions dir.
            let valid = !sub_args.is_empty()
                && sub_args
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
            if !valid {
                CommandAction::Message(
                    "Usage: /session delete <id-prefix>  (letters, digits, '-' and '_' only)"
                        .into(),
                )
            } else {
                // Sessions live under the XDG data dir when XDG_DATA_HOME is
                // set, so a hard-coded ~/.claude path would point at nothing.
                let dir = crate::config::Config::sessions_dir();
                let dir = dir.display().to_string().replace('\'', "'\\''");
                CommandAction::Message(format!(
                    "To delete session, run:\n  rm -rf '{dir}'/{sub_args}*.jsonl '{dir}'/{sub_args}*.meta '{dir}'/{sub_args}*.redo '{dir}'/{sub_args}*/\n\nThe .redo file holds the turns /undo took off; the directory holds the file snapshots older versions kept for /rewind. Use /session list to confirm the ID prefix."
                ))
            }
        }
        // Treat anything else as a session ID prefix to resume
        _ => CommandAction::ResumeSession(sub.to_string()),
    }
}

pub(super) fn cmd_resume(args: &str) -> CommandAction {
    if args.is_empty() {
        // Show session list — run_loop handles async I/O
        CommandAction::ListSessions
    } else {
        // Pass the prefix to run_loop for async matching
        CommandAction::ResumeSession(args.trim().to_string())
    }
}

pub(super) fn cmd_export(_ctx: &CommandContext) -> CommandAction {
    CommandAction::ExportCurrentSession
}

pub(super) fn cmd_image(args: &str) -> CommandAction {
    let path = args.trim();
    if path.is_empty() {
        return CommandAction::Message(
            "Usage: /image <path>\n\nAttaches an image to your next message.\n\
             Supported formats: PNG, JPEG, GIF, WebP (up to 3.75 MB)\n\n\
             Example: /image /home/user/screenshot.png"
                .into(),
        );
    }
    let expanded = if path.starts_with('~') {
        if let Some(home) = dirs::home_dir() {
            home.join(path.trim_start_matches("~/"))
                .to_string_lossy()
                .into_owned()
        } else {
            path.to_string()
        }
    } else {
        path.to_string()
    };
    if let Err(why) = check_image_file(std::path::Path::new(&expanded)) {
        return CommandAction::Message(why);
    }
    CommandAction::AttachImage(expanded)
}

/// Largest image file /image accepts. The API caps an image at 5 MB of
/// base64, which is ~3.75 MB of raw bytes; anything bigger is a 400.
pub const MAX_IMAGE_BYTES: u64 = 3_750_000;

/// Media type from an image's magic bytes, for the formats the API accepts.
/// The extension is not trusted: a JPEG saved as `.png` sent as image/png is
/// a 400 just like a HEIC or a PDF.
pub fn image_media_type(head: &[u8]) -> Option<&'static str> {
    if head.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if head.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if head.starts_with(b"GIF87a") || head.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if head.len() >= 12 && &head[..4] == b"RIFF" && &head[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

/// Validate an /image path before it is attached. A rejected image would stay
/// in the history and fail every later request, so reject it here instead.
pub fn check_image_file(path: &std::path::Path) -> Result<(), String> {
    use std::io::Read;
    let shown = path.display();
    let meta = std::fs::metadata(path).map_err(|_| format!("File not found: {shown}"))?;
    // Not a FIFO or device: reading those can block the UI forever.
    if !meta.is_file() {
        return Err(format!("Not a regular file: {shown}"));
    }
    if meta.len() > MAX_IMAGE_BYTES {
        return Err(format!(
            "Image too large: {shown} is {:.1} MB; the API accepts at most {:.2} MB. \
             Resize or compress it first.",
            meta.len() as f64 / 1e6,
            MAX_IMAGE_BYTES as f64 / 1e6
        ));
    }
    let mut head = [0u8; 12];
    let n = std::fs::File::open(path)
        .and_then(|mut f| f.read(&mut head))
        .map_err(|e| format!("Could not read {shown}: {e}"))?;
    if image_media_type(&head[..n]).is_none() {
        return Err(format!(
            "Unsupported image: {shown} is not PNG, JPEG, GIF or WebP."
        ));
    }
    Ok(())
}

/// A single help command entry: (slash_command, description).
pub type HelpCommand = (&'static str, &'static str);

pub(super) fn cmd_teleport(args: &str) -> CommandAction {
    match args.trim() {
        "export" => CommandAction::TeleportExport,
        "import" => CommandAction::TeleportImport,
        _ => CommandAction::Message(
            "Teleport — session context transfer\n\n\
             /teleport export   — save session context to teleport.json in the config dir\n\
             /teleport import   — load session context from teleport.json in the config dir\n\n\
             Use this to transfer a conversation to another terminal or instance."
                .into(),
        ),
    }
}

// ── Feedback ───────────────────────────────────────────────────────────────────

pub(super) fn cmd_share(args: &str) -> CommandAction {
    match args.trim() {
        "clip" | "clipboard" => CommandAction::ShareClipboard,
        "" => CommandAction::ShareSession,
        _ => CommandAction::Message(
            "Share session\n\n\
             /share          — export session to a markdown file in the current directory\n\
             /share clip     — copy session markdown to clipboard (wl-copy, xclip, xsel, pbcopy or clip.exe)"
                .into(),
        ),
    }
}

// ── Notifications ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod session_command_tests {
    use super::{CommandAction, cmd_session};
    // Only the unix clipboard tests use these.
    #[cfg(unix)]
    use super::{CLIPBOARD_TOOLS, copy_with, pipe_to};

    /// /share clip only knew wl-copy and xclip, so it always failed on
    /// macOS and Windows; it now shares this list and its fallthrough.
    #[cfg(unix)]
    #[test]
    fn clipboard_falls_through_to_a_working_tool() {
        let names: Vec<&str> = CLIPBOARD_TOOLS.iter().map(|(c, _)| *c).collect();
        assert!(names.contains(&"pbcopy") && names.contains(&"clip.exe"));

        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("clip.txt");
        let script = format!("cat > '{}'", out.display());
        let tools: &[(&str, &[&str])] = &[
            ("definitely-not-a-clipboard-xyz", &[]),
            ("false", &[]),
            ("sh", &["-c", &script]),
        ];
        assert!(copy_with(tools, "session"));
        assert_eq!(std::fs::read_to_string(&out).unwrap(), "session");
        assert!(!copy_with(&tools[..2], "session"));
    }

    /// /copy passed the reply to wl-copy as an argument, so a Markdown
    /// bullet list ("- item") was parsed as an option and the copy failed.
    #[cfg(unix)]
    #[test]
    fn clipboard_text_goes_through_stdin_not_argv() {
        let (wl_cmd, wl_args) = CLIPBOARD_TOOLS[0];
        assert_eq!(wl_cmd, "wl-copy");
        assert!(wl_args.is_empty());

        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("clip.txt");
        let script = format!("cat > '{}'", out.display());
        let text = "- first bullet\n--second\n";
        let status = pipe_to("sh", &["-c", &script], text).unwrap();
        assert!(status.success());
        assert_eq!(std::fs::read_to_string(&out).unwrap(), text);
    }

    /// `/session clear` wiped every saved session at once, unasked.
    #[test]
    fn clearing_all_sessions_needs_an_explicit_yes() {
        for args in [
            "clear",
            "clear-all",
            "clearall",
            "clear --yes",
            "clear-all yes",
        ] {
            match cmd_session(args) {
                CommandAction::Message(m) => {
                    assert!(m.contains("/session clear-all --yes"), "{args:?}: {m}")
                }
                _ => panic!("{args:?} must not clear sessions"),
            }
        }
        for args in ["clear-all --yes", "clearall --yes", "clear-all  --yes "] {
            assert!(
                matches!(cmd_session(args), CommandAction::ClearAllSessions),
                "{args:?}"
            );
        }
    }

    /// The delete hint hard-coded ~/.claude/sessions, which is wrong once
    /// XDG_DATA_HOME moves the sessions dir.
    #[test]
    fn delete_hint_points_at_the_real_sessions_dir() {
        let dir = crate::config::Config::sessions_dir();
        match cmd_session("delete abc123") {
            CommandAction::Message(m) => {
                assert!(
                    m.contains(&format!("'{}'/abc123*.jsonl", dir.display())),
                    "{m}"
                );
                assert!(
                    m.contains(&format!("'{}'/abc123*.meta", dir.display())),
                    "{m}"
                );
                assert!(m.contains(&format!("'{}'/abc123*/", dir.display())), "{m}");
                // Undone turns' prompts and replies live in the .redo file.
                assert!(
                    m.contains(&format!("'{}'/abc123*.redo", dir.display())),
                    "{m}"
                );
            }
            _ => panic!("/session delete must only print a hint"),
        }
    }

    /// The prefix went into the `rm -rf` hint unquoted: `delete a /` printed
    /// a command that runs `rm -rf /*/` when pasted.
    #[test]
    fn delete_hint_refuses_anything_but_an_id_prefix() {
        for args in [
            "delete a /",
            "delete *",
            "delete ../x",
            "delete x ~",
            "delete",
        ] {
            match cmd_session(args) {
                CommandAction::Message(m) => {
                    assert!(m.starts_with("Usage: /session delete"), "{args}: {m}");
                    assert!(!m.contains("rm "), "{args}: {m}");
                }
                _ => panic!("{args}: /session delete must only print a message"),
            }
        }
    }
}
