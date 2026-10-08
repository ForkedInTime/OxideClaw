//! NDJSON stdin/stdout transport.
//!
//! Reads one JSON object per line from stdin.
//! Writes one JSON object per line to stdout.
//! Stderr is reserved for debug/log output.

use crate::sdk::protocol::{SdkNotification, SdkRequest, SdkResponse};
use crate::sdk::transport::{BadRequest, Transport, parse_request};
use anyhow::{Context, Result};
use async_trait::async_trait;
use tokio::io::{AsyncBufRead, AsyncWriteExt, BufReader};
use tokio::sync::{Mutex, mpsc};

/// Max line size: 4MB — generous limit for large tool outputs.
const MAX_LINE_SIZE: usize = 4 * 1024 * 1024;

/// How much of an over-long line is kept to find its request id in.
const TOO_LONG_PREFIX: usize = 64 * 1024;

pub struct StdioTransport {
    /// Started on the first read, so `new()` works outside a runtime.
    lines: Option<mpsc::Receiver<std::io::Result<(LineRead, String)>>>,
    writer: Mutex<tokio::io::Stdout>,
}

impl Default for StdioTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl StdioTransport {
    pub fn new() -> Self {
        Self {
            lines: None,
            writer: Mutex::new(tokio::io::stdout()),
        }
    }
}

#[async_trait]
impl Transport for StdioTransport {
    async fn read_request(&mut self) -> Result<Option<Result<SdkRequest, BadRequest>>> {
        let lines = self.lines.get_or_insert_with(|| {
            spawn_line_reader(BufReader::new(tokio::io::stdin()), MAX_LINE_SIZE)
        });
        loop {
            let read = match lines.recv().await {
                None => return Ok(None),
                Some(read) => read.context("Failed to read from stdin")?,
            };
            match classify_line(read) {
                Classified::Eof => return Ok(None),
                Classified::Skip => continue,
                Classified::Request(req) => return Ok(Some(req)),
            }
        }
    }

    async fn send_response(&self, response: SdkResponse) -> Result<()> {
        let mut json = serde_json::to_string(&response)?;
        json.push('\n');
        let mut writer = self.writer.lock().await;
        writer.write_all(json.as_bytes()).await?;
        writer.flush().await?;
        Ok(())
    }

    async fn send_notification(&self, notification: SdkNotification) -> Result<()> {
        let mut json = serde_json::to_string(&notification)?;
        json.push('\n');
        let mut writer = self.writer.lock().await;
        writer.write_all(json.as_bytes()).await?;
        writer.flush().await?;
        Ok(())
    }
}

// Built and matched once per line, never stored: boxing would only add an
// allocation per request.
#[allow(clippy::large_enum_variant)]
enum Classified {
    Eof,
    Skip,
    Request(Result<SdkRequest, BadRequest>),
}

/// What one read line is to the server. Every line that is not blank gets
/// a reply, so a host waiting on its id never hangs: a non-UTF-8 or
/// over-long line is a `parse_error`, with the id recovered where possible.
fn classify_line((read, line): (LineRead, String)) -> Classified {
    let bad = |id: String, message: &str| {
        Classified::Request(Err(BadRequest {
            id,
            code: "parse_error",
            message: message.into(),
        }))
    };
    match read {
        LineRead::Eof => Classified::Eof, // EOF — host closed stdin
        LineRead::TooLong => {
            eprintln!("[sdk] Warning: line exceeds 4MB");
            let id = crate::sdk::transport::request_id_from_prefix(&line)
                .map(|id| crate::sdk::transport::request_id(&serde_json::json!({ "id": id })))
                .unwrap_or_default();
            bad(id, "line exceeds 4MB")
        }
        LineRead::InvalidUtf8 => {
            eprintln!("[sdk] Warning: line is not valid UTF-8");
            // The lossy text is used only to find the id: U+FFFD inside a
            // JSON string can still parse, and must never be dispatched.
            let id = serde_json::from_str::<serde_json::Value>(line.trim())
                .map(|v| crate::sdk::transport::request_id(&v))
                .unwrap_or_default();
            bad(id, "line is not valid UTF-8")
        }
        LineRead::Line => {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                return Classified::Skip; // skip blank lines
            }
            let req = parse_request(trimmed);
            if req.is_err() {
                eprintln!("[sdk] Invalid JSON request: {}", preview(trimmed, 200));
            }
            Classified::Request(req)
        }
    }
}

/// Result of one bounded line read.
#[derive(Debug, PartialEq)]
pub(crate) enum LineRead {
    Eof,
    Line,
    /// The line exceeded `max` bytes; it was drained and discarded. The
    /// buffer holds the lossy text of its first `TOO_LONG_PREFIX` bytes, for
    /// recovering the request id only.
    TooLong,
    /// The line was not valid UTF-8. Not fatal: one bad line from the host
    /// must not end the session. Its lossy text is in the buffer, for
    /// recovering the request id only.
    InvalidUtf8,
}

/// Read lines on a task of their own and hand them over a channel.
///
/// The server loops `select!` a read against outgoing notifications, and a
/// `read_line` future is not cancel-safe: when the other branch wins, the
/// bytes it already consumed are lost and the rest of the line parses as
/// garbage. `mpsc::Receiver::recv` is cancel-safe. The channel is bounded so
/// a busy server applies backpressure instead of buffering stdin.
/// Stops after EOF or the first error, which it delivers.
pub(crate) fn spawn_line_reader<R: AsyncBufRead + Unpin + Send + 'static>(
    mut reader: R,
    max: usize,
) -> mpsc::Receiver<std::io::Result<(LineRead, String)>> {
    let (tx, rx) = mpsc::channel(16);
    tokio::spawn(async move {
        loop {
            let mut line = String::new();
            let read = read_line_bounded(&mut reader, &mut line, max).await;
            let last = !matches!(
                read,
                Ok(LineRead::Line | LineRead::TooLong | LineRead::InvalidUtf8)
            );
            if tx.send(read.map(|r| (r, line))).await.is_err() || last {
                break;
            }
        }
    });
    rx
}

/// Read one line into `buf` without ever buffering more than `max` bytes
/// of it. An over-long line is drained to its newline and reported, so a
/// hostile or buggy host cannot make the sidecar allocate without bound.
pub(crate) async fn read_line_bounded<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
    buf: &mut String,
    max: usize,
) -> std::io::Result<LineRead> {
    use tokio::io::{AsyncBufReadExt, AsyncReadExt};
    // Bytes, not `read_line`: that fails with InvalidData on any non-UTF-8
    // input, including a valid line whose cap lands mid-character, and the
    // error stopped the reader and with it the whole server.
    // Read at most `max + 1` bytes of the line; a full read without a
    // newline means the line is longer than allowed.
    let mut bytes = Vec::new();
    let n = {
        let mut limited = AsyncReadExt::take(&mut *reader, max as u64 + 1);
        limited.read_until(b'\n', &mut bytes).await?
    };
    if n == 0 {
        return Ok(LineRead::Eof);
    }
    if bytes.ends_with(b"\n") {
        return Ok(match String::from_utf8(bytes) {
            Ok(line) => {
                buf.push_str(&line);
                LineRead::Line
            }
            Err(e) => {
                buf.push_str(&String::from_utf8_lossy(e.as_bytes()));
                LineRead::InvalidUtf8
            }
        });
    }
    // No newline within the cap: keep the start, where the request id is,
    // so the host's request is not left waiting on a reply without one.
    // Then drain the rest of the line in bounded chunks.
    buf.push_str(&String::from_utf8_lossy(
        &bytes[..bytes.len().min(TOO_LONG_PREFIX)],
    ));
    loop {
        bytes.clear();
        let n = {
            let mut limited = AsyncReadExt::take(&mut *reader, max as u64);
            limited.read_until(b'\n', &mut bytes).await?
        };
        if n == 0 || bytes.ends_with(b"\n") {
            return Ok(LineRead::TooLong);
        }
    }
}

/// First `n` characters — never a byte slice, which panicked on a
/// multi-byte character at the cut and took the whole sidecar down.
pub(crate) fn preview(s: &str, n: usize) -> &str {
    match s.char_indices().nth(n) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

#[cfg(test)]
mod bounded_read_tests {
    use super::*;
    use tokio::io::BufReader;

    #[tokio::test]
    async fn ordinary_lines_read_normally() {
        let mut r = BufReader::new(std::io::Cursor::new(b"{\"a\":1}\nnext\n".to_vec()));
        let mut buf = String::new();
        assert_eq!(
            read_line_bounded(&mut r, &mut buf, 1024).await.unwrap(),
            LineRead::Line
        );
        assert_eq!(buf.trim(), "{\"a\":1}");
        buf.clear();
        assert_eq!(
            read_line_bounded(&mut r, &mut buf, 1024).await.unwrap(),
            LineRead::Line
        );
        assert_eq!(buf.trim(), "next");
        buf.clear();
        assert_eq!(
            read_line_bounded(&mut r, &mut buf, 1024).await.unwrap(),
            LineRead::Eof
        );
    }

    /// A 10 KB line under a 1 KB cap must not be buffered, and the line
    /// *after* it must still be readable.
    #[tokio::test]
    async fn an_over_long_line_is_discarded_without_buffering_it() {
        let mut data = "x".repeat(10_000).into_bytes();
        data.extend_from_slice(b"\nok\n");
        let mut r = BufReader::new(std::io::Cursor::new(data));
        let mut buf = String::new();
        assert_eq!(
            read_line_bounded(&mut r, &mut buf, 1024).await.unwrap(),
            LineRead::TooLong
        );
        assert!(buf.len() <= 1024 + 1, "buffered {} bytes", buf.len());
        buf.clear();
        assert_eq!(
            read_line_bounded(&mut r, &mut buf, 1024).await.unwrap(),
            LineRead::Line
        );
        assert_eq!(buf.trim(), "ok");
    }

    /// The servers race the next line against notifications. A 20 KiB line
    /// arriving in 4 KiB pieces over a pipe must survive the other branch
    /// winning over and over; racing `read_line` directly lost the consumed
    /// prefix each time and parsed the tail as a new request.
    #[tokio::test]
    async fn lines_survive_a_select_that_keeps_cancelling_the_read() {
        use tokio::io::AsyncWriteExt;
        let (mut tx, rx) = tokio::io::duplex(4096);
        let big = format!("{{\"pad\":\"{}\"}}", "x".repeat(20 * 1024));
        let payload = format!("{big}\nok\n");
        tokio::spawn(async move {
            for chunk in payload.as_bytes().chunks(1000) {
                tx.write_all(chunk).await.unwrap();
                tokio::task::yield_now().await;
            }
        });
        let mut lines = spawn_line_reader(BufReader::new(rx), 1 << 20);
        let (notif_tx, mut notif_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
        let mut got = Vec::new();
        let mut notifs = 0usize;
        while got.len() < 2 {
            let _ = notif_tx.send(());
            tokio::select! {
                read = lines.recv() => match read.unwrap().unwrap() {
                    (LineRead::Line, l) => got.push(l.trim().to_string()),
                    other => panic!("unexpected {other:?}"),
                },
                Some(()) = notif_rx.recv() => notifs += 1,
            }
        }
        assert!(notifs > 0, "the notification branch never won");
        assert_eq!(got[0], big);
        assert_eq!(got[1], "ok");
        assert!(matches!(lines.recv().await, Some(Ok((LineRead::Eof, _)))));
    }

    /// A cap landing inside a multi-byte character used to surface as an
    /// InvalidData error that stopped the reader.
    #[tokio::test]
    async fn an_over_long_line_cut_mid_character_is_too_long_not_an_error() {
        let mut data = format!("aaa{}", "日".repeat(10)).into_bytes();
        data.extend_from_slice(b"\nok\n");
        let mut r = BufReader::new(std::io::Cursor::new(data));
        let mut buf = String::new();
        // Cap 4 -> the first read takes 5 bytes: "aaa" + 2 of 3 bytes of 日.
        assert_eq!(
            read_line_bounded(&mut r, &mut buf, 4).await.unwrap(),
            LineRead::TooLong
        );
        buf.clear();
        assert_eq!(
            read_line_bounded(&mut r, &mut buf, 4).await.unwrap(),
            LineRead::Line
        );
        assert_eq!(buf.trim(), "ok");
    }

    #[tokio::test]
    async fn an_invalid_utf8_line_is_reported_and_the_reader_keeps_going() {
        let data = b"\xff\xfe{}\nok\n".to_vec();
        let mut lines = spawn_line_reader(BufReader::new(std::io::Cursor::new(data)), 1024);
        assert!(matches!(
            lines.recv().await,
            Some(Ok((LineRead::InvalidUtf8, _)))
        ));
        match lines.recv().await {
            Some(Ok((LineRead::Line, l))) => assert_eq!(l.trim(), "ok"),
            other => panic!("unexpected {other:?}"),
        }
        assert!(matches!(lines.recv().await, Some(Ok((LineRead::Eof, _)))));
    }

    /// A non-UTF-8 line was skipped with no reply, so a host waiting on
    /// that request's id hung forever.
    #[tokio::test]
    async fn an_invalid_utf8_request_gets_an_error_reply_with_its_id() {
        let data =
            b"{\"type\":\"session/start\",\"id\":\"r7\",\"cwd\":\"/tmp/\xff\"}\n\xff\xfe{}\n"
                .to_vec();
        let mut lines = spawn_line_reader(BufReader::new(std::io::Cursor::new(data)), 1024);
        for want_id in ["r7", ""] {
            match classify_line(lines.recv().await.unwrap().unwrap()) {
                Classified::Request(Err(bad)) => {
                    assert_eq!(bad.id, want_id);
                    assert_eq!(bad.code, "parse_error");
                }
                _ => panic!("no error reply for a non-UTF-8 line"),
            }
        }
        assert!(matches!(
            classify_line((LineRead::TooLong, String::new())),
            Classified::Request(Err(_))
        ));
        assert!(matches!(
            classify_line((LineRead::Line, "  \n".into())),
            Classified::Skip
        ));
    }

    /// An over-long request was answered with id "", so the host's request
    /// waited forever. The line's start is kept, and its id read from it.
    #[tokio::test]
    async fn an_over_long_request_gets_an_error_reply_with_its_id() {
        let line = format!(
            "{{\"type\":\"session/prompt\",\"id\":\"p9\",\"prompt\":\"{}\"}}\nok\n",
            "x".repeat(10_000)
        );
        let mut lines = spawn_line_reader(
            BufReader::new(std::io::Cursor::new(line.into_bytes())),
            1024,
        );
        match classify_line(lines.recv().await.unwrap().unwrap()) {
            Classified::Request(Err(bad)) => {
                assert_eq!(bad.id, "p9");
                assert_eq!(bad.code, "parse_error");
            }
            _ => panic!("no error reply for an over-long line"),
        }
        match lines.recv().await {
            Some(Ok((LineRead::Line, l))) => assert_eq!(l.trim(), "ok"),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn preview_never_splits_a_character() {
        let s = format!("{}日本語", "a".repeat(199));
        assert!(preview(&s, 200).ends_with('日'));
        assert_eq!(preview("short", 200), "short");
    }
}
