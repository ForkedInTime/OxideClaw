//! Transport abstraction — read requests, send responses/notifications.

pub mod stdio;

use crate::sdk::protocol::{SdkNotification, SdkRequest, SdkResponse};
use anyhow::Result;
use async_trait::async_trait;

/// An input line that does not parse as a request. Hosts wait on replies by
/// `id`, so it is answered with an `error` carrying whatever `id` the line
/// had instead of only being logged.
#[derive(Debug, Clone, PartialEq)]
pub struct BadRequest {
    pub id: String,
    pub code: &'static str,
    pub message: String,
}

/// The `id` of a request object, as a host would match its reply ("" when
/// there is none).
pub(crate) fn request_id(v: &serde_json::Value) -> String {
    match v.get("id") {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(n @ serde_json::Value::Number(_)) => n.to_string(),
        _ => String::new(),
    }
}

/// The top-level `"id"` (a string or number) of a request object of which
/// only `prefix` is at hand: the start of a line too long to parse whole,
/// or one that does not parse. Hosts write `jsonrpc`/`id`/`method` before
/// the params, so the id is there even when the params are cut off.
pub(crate) fn request_id_from_prefix(prefix: &str) -> Option<serde_json::Value> {
    let bytes = prefix.as_bytes();
    let mut depth = 0usize;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'{' | b'[' => depth += 1,
            b'}' | b']' => depth = depth.checked_sub(1)?,
            b'"' => {
                let start = i;
                i += 1;
                while i < bytes.len() && bytes[i] != b'"' {
                    i += if bytes[i] == b'\\' { 2 } else { 1 };
                }
                if i >= bytes.len() {
                    return None;
                }
                // A key, not a string value that happens to read "id".
                if depth == 1
                    && &prefix[start..=i] == "\"id\""
                    && let Some(rest) = prefix[i + 1..].trim_start().strip_prefix(':')
                {
                    let value = serde_json::Deserializer::from_str(rest)
                        .into_iter::<serde_json::Value>()
                        .next()?
                        .ok()?;
                    return (value.is_string() || value.is_number()).then_some(value);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// `parse_error` for a line that is not JSON, `invalid_request` for JSON
/// that is not a known request (unknown `type`, missing field, numeric `id`).
pub fn parse_request(line: &str) -> Result<SdkRequest, BadRequest> {
    serde_json::from_str(line).map_err(|e| BadRequest::from_line(line, &e))
}

impl BadRequest {
    fn from_line(line: &str, err: &serde_json::Error) -> Self {
        let value = serde_json::from_str::<serde_json::Value>(line).ok();
        let id = value.as_ref().map(request_id).unwrap_or_default();
        Self {
            id,
            code: if value.is_some() {
                "invalid_request"
            } else {
                "parse_error"
            },
            message: format!("Invalid request: {err}"),
        }
    }
}

#[async_trait]
pub trait Transport: Send + Sync {
    /// Must be cancel-safe: `SdkServer::run` races it against outgoing
    /// notifications in a `select!`, dropping it whenever they win.
    /// `Ok(None)` is end of input; `Some(Err(_))` is a line that is not a
    /// request, which the server answers with an `error` response.
    async fn read_request(&mut self) -> Result<Option<Result<SdkRequest, BadRequest>>>;
    async fn send_response(&self, response: SdkResponse) -> Result<()>;
    async fn send_notification(&self, notification: SdkNotification) -> Result<()>;
}

#[cfg(test)]
mod tests {
    use super::request_id_from_prefix;
    use serde_json::json;

    #[test]
    fn the_id_is_read_from_a_cut_off_request() {
        let id = |s: &str| request_id_from_prefix(s);
        assert_eq!(
            id(
                r#"{"jsonrpc":"2.0","id":7,"method":"session/prompt","params":{"prompt":[{"text":"xx"#
            ),
            Some(json!(7))
        );
        assert_eq!(
            id(r#"{"type":"session/start", "id" : "r1", "cwd":"/"#),
            Some(json!("r1"))
        );
        // A nested "id", or a string value reading "id", is not the request's.
        assert_eq!(
            id(r#"{"params":{"id":1,"x":["id"]},"method":"id","id":"top","p":"#),
            Some(json!("top"))
        );
        assert_eq!(
            id(r#"{"text":"a \"id\": 3 inside","id":4}"#),
            Some(json!(4))
        );
        assert_eq!(id(r#"{"method":"x","params":{"id":1,"text":"cut"#), None);
        assert_eq!(id(r#"{"id":{"nested":1}}"#), None);
        assert_eq!(id(""), None);
        assert_eq!(id("not json at all"), None);
    }
}
