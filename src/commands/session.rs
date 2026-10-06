//! `/` command handlers — split out of `commands/mod.rs` mechanically.

use super::*;

pub(super) fn cmd_copy(ctx: &CommandContext) -> CommandAction {
    let Some(text) = ctx.last_assistant else {
        return CommandAction::Message("No assistant message to copy yet.".into());
    };
    clipboard_write(text)
}

/// Write text to the system clipboard. Tries all known clipboard tools across platforms.
pub fn clipboard_write(text: &str) -> CommandAction {
    use std::io::Write;

    // Helper: pipe text into a child process
    let pipe_to = |cmd: &str, args: &[&str]| -> std::io::Result<std::process::ExitStatus> {
        let mut child = std::process::Command::new(cmd)
            .args(args)
            .stdin(std::process::Stdio::piped())
            .spawn()?;
        child
            .stdin
            .as_mut()
            .ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::BrokenPipe, "stdin not available")
            })?
            .write_all(text.as_bytes())?;
        child.wait()
    };

    // Wayland
    let ok = std::process::Command::new("wl-copy").arg(text).status()
        .map(|s| s.success()).unwrap_or(false)
    // X11 xclip
    || pipe_to("xclip", &["-selection", "clipboard"]).map(|s| s.success()).unwrap_or(false)
    // X11 xsel
    || pipe_to("xsel", &["--clipboard", "--input"]).map(|s| s.success()).unwrap_or(false)
    // macOS
    || pipe_to("pbcopy", &[]).map(|s| s.success()).unwrap_or(false)
    // WSL / Windows
    || pipe_to("clip.exe", &[]).map(|s| s.success()).unwrap_or(false);

    if ok {
        CommandAction::Message("Copied to clipboard.".into())
    } else {
        CommandAction::Message(
            "Could not copy to clipboard.\n\
             Install one of: wl-clipboard (Wayland), xclip or xsel (X11), pbcopy (macOS), clip.exe (WSL)."
            .into()
        )
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
            if sub_args.is_empty() {
                CommandAction::Message("Usage: /session delete <id-prefix>".into())
            } else {
                CommandAction::Message(format!(
                    "To delete session, run:\n  rm ~/.claude/sessions/{sub_args}*.jsonl ~/.claude/sessions/{sub_args}*.meta\n\nUse /session list to confirm the ID prefix."
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
             /teleport export   — save session context to ~/.claude/teleport.json\n\
             /teleport import   — load session context from ~/.claude/teleport.json\n\n\
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
             /share clip     — copy session markdown to clipboard (requires xclip or wl-copy)"
                .into(),
        ),
    }
}

// ── Notifications ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod session_command_tests {
    use super::{CommandAction, cmd_session};

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
}
