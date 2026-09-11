//! Vim-normal keys, clipboard, image attachment, base64.
//! Split out of `tui/run.rs` mechanically — no behaviour change.

use super::*;

pub(super) fn handle_vim_normal(key: crossterm::event::KeyEvent, app: &mut App) {
    use crossterm::event::KeyCode::*;

    // Ctrl+C always quits
    if key.code == Char('c') && key.modifiers == crossterm::event::KeyModifiers::CONTROL {
        app.should_quit = true;
        return;
    }

    // Handle pending two-char commands (e.g. "dd")
    if let Some(pending) = app.vim_pending.take() {
        // Unknown combinations are silently dropped.
        if let ('d', Char('d')) = (pending, key.code) {
            app.clear_line();
        }
        return;
    }

    match key.code {
        // ── Enter insert mode ──────────────────────────────────────────────────
        Char('i') => app.vim_enter_insert(),
        Char('a') => {
            app.cursor_right();
            app.vim_enter_insert();
        }
        Char('A') => {
            app.cursor_end();
            app.vim_enter_insert();
        }
        Char('I') => {
            app.cursor_home();
            app.vim_enter_insert();
        }

        // ── Horizontal motion ──────────────────────────────────────────────────
        Char('h') | Left => app.cursor_left(),
        Char('l') | Right => app.cursor_right(),
        Char('0') | Home => app.cursor_home(),
        Char('$') | End => {
            // In normal mode $ lands on last char, not past it
            if !app.input.is_empty() {
                app.cursor = app.input.len() - 1;
            }
        }

        // ── Word motion ────────────────────────────────────────────────────────
        Char('w') => app.word_forward(),
        Char('b') => app.word_back(),
        Char('e') => app.word_end(),

        // ── Edit operators ─────────────────────────────────────────────────────
        Char('x') => app.delete_under(),
        // 'd' starts a two-char sequence; 'dd' = clear line
        Char('d') => {
            app.vim_pending = Some('d');
        }

        // ── Chat scrolling (j/k in normal mode scroll chat, not move cursor) ──
        Char('j') | Down => {
            app.follow_bottom = false;
            app.scroll = app.scroll.saturating_add(3);
        }
        Char('k') | Up => {
            app.follow_bottom = false;
            app.scroll = app.scroll.saturating_sub(3);
        }
        Char('G') => app.scroll_to_bottom(),

        // ── Esc in normal mode: clear pending, stay in normal ─────────────────
        Esc => {
            app.vim_pending = None;
        }

        _ => {}
    }
}

// ── API task ──────────────────────────────────────────────────────────────────

// ── Image attachment helper ───────────────────────────────────────────────────

/// Read an image file and return it as a ContentBlock::Image (base64).
/// Try to write `content` to the system clipboard via wl-copy (Wayland) or xclip (X11).
/// Returns true if a clipboard tool was found and succeeded.
pub(super) async fn try_clipboard_write(content: &str) -> bool {
    use std::process::Stdio;
    // Try wl-copy first (Wayland)
    if let Ok(mut child) = tokio::process::Command::new("wl-copy")
        .stdin(Stdio::piped())
        .spawn()
    {
        if let Some(stdin) = child.stdin.take() {
            use tokio::io::AsyncWriteExt;
            let mut s = stdin;
            let _ = s.write_all(content.as_bytes()).await;
        }
        if child.wait().await.map(|s| s.success()).unwrap_or(false) {
            return true;
        }
    }
    // Fall back to xclip (X11)
    if let Ok(mut child) = tokio::process::Command::new("xclip")
        .args(["-selection", "clipboard"])
        .stdin(Stdio::piped())
        .spawn()
    {
        if let Some(stdin) = child.stdin.take() {
            use tokio::io::AsyncWriteExt;
            let mut s = stdin;
            let _ = s.write_all(content.as_bytes()).await;
        }
        if child.wait().await.map(|s| s.success()).unwrap_or(false) {
            return true;
        }
    }
    false
}

pub(super) fn attach_image(path: &str) -> AResult<ContentBlock> {
    use std::io::Read;
    let mut f = std::fs::File::open(path)?;
    let mut bytes = Vec::new();
    f.read_to_end(&mut bytes)?;

    let media_type = match std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase())
        .as_deref()
    {
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        _ => "image/png",
    };

    let data = base64_encode(&bytes);
    Ok(ContentBlock::Image {
        source: ImageSource::Base64 {
            media_type: media_type.to_string(),
            data,
        },
    })
}

/// Minimal base64 encoder (avoids pulling in a whole crate for a simple use case).
pub(super) fn base64_encode(data: &[u8]) -> String {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as usize;
        let b1 = if chunk.len() > 1 {
            chunk[1] as usize
        } else {
            0
        };
        let b2 = if chunk.len() > 2 {
            chunk[2] as usize
        } else {
            0
        };
        out.push(CHARS[b0 >> 2] as char);
        out.push(CHARS[((b0 & 3) << 4) | (b1 >> 4)] as char);
        out.push(if chunk.len() > 1 {
            CHARS[((b1 & 0xf) << 2) | (b2 >> 6)] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            CHARS[b2 & 0x3f] as char
        } else {
            '='
        });
    }
    out
}

// ── API task ──────────────────────────────────────────────────────────────────
