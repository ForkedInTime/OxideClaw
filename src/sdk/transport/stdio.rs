//! NDJSON stdin/stdout transport.
//!
//! Reads one JSON object per line from stdin.
//! Writes one JSON object per line to stdout.
//! Stderr is reserved for debug/log output.

use crate::sdk::protocol::{SdkNotification, SdkRequest, SdkResponse};
use crate::sdk::transport::Transport;
use anyhow::{Context, Result};
use async_trait::async_trait;
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::sync::Mutex;

/// Max line size: 4MB — generous limit for large tool outputs.
const MAX_LINE_SIZE: usize = 4 * 1024 * 1024;

pub struct StdioTransport {
    reader: Mutex<BufReader<tokio::io::Stdin>>,
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
            reader: Mutex::new(BufReader::new(tokio::io::stdin())),
            writer: Mutex::new(tokio::io::stdout()),
        }
    }
}

#[async_trait]
impl Transport for StdioTransport {
    async fn read_request(&mut self) -> Result<Option<SdkRequest>> {
        let mut line = String::new();
        let mut reader = self.reader.lock().await;
        loop {
            line.clear();
            match read_line_bounded(&mut *reader, &mut line, MAX_LINE_SIZE)
                .await
                .context("Failed to read from stdin")?
            {
                LineRead::Eof => return Ok(None), // EOF — host closed stdin
                LineRead::TooLong => {
                    eprintln!("[sdk] Warning: line exceeds 4MB, skipping");
                    continue;
                }
                LineRead::Line => {}
            }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue; // skip blank lines
            }
            let req: SdkRequest = serde_json::from_str(trimmed)
                .with_context(|| format!("Invalid JSON request: {}", preview(trimmed, 200)))?;
            return Ok(Some(req));
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

/// Result of one bounded line read.
#[derive(Debug, PartialEq)]
pub(crate) enum LineRead {
    Eof,
    Line,
    /// The line exceeded `max` bytes; it was drained and discarded.
    TooLong,
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
    // Read at most `max + 1` bytes of the line; a full read without a
    // newline means the line is longer than allowed.
    let n = {
        let mut limited = AsyncReadExt::take(&mut *reader, max as u64 + 1);
        limited.read_line(buf).await?
    };
    if n == 0 {
        return Ok(LineRead::Eof);
    }
    if buf.ends_with('\n') {
        return Ok(LineRead::Line);
    }
    // No newline within the cap: drain the rest of the line in bounded
    // chunks and report it as over-long.
    buf.clear();
    let mut scratch = String::new();
    loop {
        scratch.clear();
        let n = {
            let mut limited = AsyncReadExt::take(&mut *reader, max as u64);
            limited.read_line(&mut scratch).await?
        };
        if n == 0 || scratch.ends_with('\n') {
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

    #[test]
    fn preview_never_splits_a_character() {
        let s = format!("{}日本語", "a".repeat(199));
        assert!(preview(&s, 200).ends_with('日'));
        assert_eq!(preview("short", 200), "short");
    }
}
