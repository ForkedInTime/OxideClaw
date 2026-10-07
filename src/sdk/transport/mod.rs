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

/// `parse_error` for a line that is not JSON, `invalid_request` for JSON
/// that is not a known request (unknown `type`, missing field, numeric `id`).
pub fn parse_request(line: &str) -> Result<SdkRequest, BadRequest> {
    serde_json::from_str(line).map_err(|e| BadRequest::from_line(line, &e))
}

impl BadRequest {
    fn from_line(line: &str, err: &serde_json::Error) -> Self {
        let value = serde_json::from_str::<serde_json::Value>(line).ok();
        let id = match value.as_ref().and_then(|v| v.get("id")) {
            Some(serde_json::Value::String(s)) => s.clone(),
            Some(v @ serde_json::Value::Number(_)) => v.to_string(),
            _ => String::new(),
        };
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
