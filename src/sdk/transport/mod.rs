//! Transport abstraction — read requests, send responses/notifications.

pub mod stdio;

use crate::sdk::protocol::{SdkNotification, SdkRequest, SdkResponse};
use anyhow::Result;
use async_trait::async_trait;

#[async_trait]
pub trait Transport: Send + Sync {
    /// Must be cancel-safe: `SdkServer::run` races it against outgoing
    /// notifications in a `select!`, dropping it whenever they win.
    async fn read_request(&mut self) -> Result<Option<SdkRequest>>;
    async fn send_response(&self, response: SdkResponse) -> Result<()>;
    async fn send_notification(&self, notification: SdkNotification) -> Result<()>;
}
