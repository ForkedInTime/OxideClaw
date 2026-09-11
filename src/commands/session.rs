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

pub(super) fn cmd_session(args: &str, _ctx: &CommandContext) -> CommandAction {
    let (sub, sub_args) = split_first_word(args);
    match sub {
        "list" | "" => CommandAction::ListSessions,
        "clear-all" | "clearall" | "clear" => CommandAction::ClearAllSessions,
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
             Supported formats: PNG, JPEG, GIF, WebP\n\n\
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
    if !std::path::Path::new(&expanded).exists() {
        return CommandAction::Message(format!("File not found: {expanded}"));
    }
    CommandAction::AttachImage(expanded)
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
