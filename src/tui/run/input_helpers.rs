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
pub(super) fn attach_image(path: &str) -> AResult<ContentBlock> {
    // Re-checked here: the file may have changed since /image accepted it.
    crate::commands::check_image_file(std::path::Path::new(path)).map_err(anyhow::Error::msg)?;
    let bytes = std::fs::read(path)?;
    if bytes.len() as u64 > crate::commands::MAX_IMAGE_BYTES {
        anyhow::bail!("image grew past the size limit after /image");
    }
    let media_type = crate::commands::image_media_type(&bytes)
        .ok_or_else(|| anyhow::anyhow!("not a PNG, JPEG, GIF or WebP image"))?;

    let data = base64_encode(&bytes);
    Ok(ContentBlock::Image {
        source: ImageSource::Base64 {
            media_type: media_type.to_string(),
            data,
        },
    })
}

/// After a failed API turn, swap the image blocks of the unanswered trailing
/// user message for a note. An image the API rejected would otherwise be
/// re-sent, and rejected again, with every later prompt. Returns whether any
/// image was removed.
pub(super) fn drop_unsent_images(messages: &mut [Message]) -> bool {
    let Some(last) = messages.last_mut() else {
        return false;
    };
    if last.role != Role::User {
        return false;
    }
    let mut dropped = false;
    for block in last.content.iter_mut() {
        if matches!(block, ContentBlock::Image { .. }) {
            *block = ContentBlock::Text {
                text: "[image removed after the request failed]".into(),
            };
            dropped = true;
        }
    }
    dropped
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

#[cfg(test)]
mod image_tests {
    use super::*;

    fn write(dir: &std::path::Path, name: &str, bytes: &[u8]) -> String {
        let p = dir.join(name);
        std::fs::write(&p, bytes).unwrap();
        p.to_string_lossy().into_owned()
    }

    #[test]
    fn media_type_comes_from_the_bytes_not_the_extension() {
        let dir = tempfile::tempdir().unwrap();
        let jpeg_named_png = write(dir.path(), "shot.png", &[0xFF, 0xD8, 0xFF, 0xE0, 0, 0]);
        let ContentBlock::Image {
            source: ImageSource::Base64 { media_type, .. },
        } = attach_image(&jpeg_named_png).unwrap()
        else {
            panic!("expected an image block");
        };
        assert_eq!(media_type, "image/jpeg");
    }

    #[test]
    fn non_images_and_oversized_files_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let pdf = write(dir.path(), "doc.png", b"%PDF-1.7 not an image");
        assert!(attach_image(&pdf).is_err());

        let mut big = b"\x89PNG\r\n\x1a\n".to_vec();
        big.resize(crate::commands::MAX_IMAGE_BYTES as usize + 1, 0);
        let big = write(dir.path(), "big.png", &big);
        assert!(attach_image(&big).is_err());

        // A directory (or FIFO) must never be read.
        assert!(attach_image(&dir.path().to_string_lossy()).is_err());
    }

    #[test]
    fn a_failed_turn_stops_resending_its_image() {
        let image = ContentBlock::Image {
            source: ImageSource::Base64 {
                media_type: "image/png".into(),
                data: "AAAA".into(),
            },
        };
        let mut messages = vec![Message {
            role: Role::User,
            content: vec![
                image,
                ContentBlock::Text {
                    text: "what is this?".into(),
                },
            ],
        }];
        assert!(drop_unsent_images(&mut messages));
        assert!(
            !messages[0]
                .content
                .iter()
                .any(|b| matches!(b, ContentBlock::Image { .. }))
        );
        // The prompt text survives.
        assert!(
            matches!(&messages[0].content[1], ContentBlock::Text { text } if text == "what is this?")
        );
        assert!(!drop_unsent_images(&mut messages), "nothing left to drop");
    }
}
