//! Chrome DevTools Protocol WebSocket client.
//!
//! Connects to Chrome's CDP endpoint, sends JSON-RPC commands, and receives
//! events and responses via a split WebSocket stream.

use anyhow::{Result, bail};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::{Mutex, broadcast, oneshot};
use tokio_tungstenite::tungstenite::Message as WsMessage;

/// A CDP command to send to Chrome.
#[derive(Debug, Clone, Serialize)]
pub struct CdpCommand {
    pub id: u64,
    pub method: String,
    pub params: serde_json::Value,
}

/// A CDP event received from Chrome (no id field).
/// `params` is parsed but not yet consumed — event handlers currently filter
/// on `method` only. Kept so future handlers (e.g. `Page.frameNavigated`
/// → extract frame id) don't need a breaking API change.
#[derive(Debug, Clone, Deserialize)]
pub struct CdpEvent {
    pub method: String,
    #[serde(default)]
    #[allow(dead_code)]
    pub params: serde_json::Value,
}

/// A CDP response to a command (has id field).
#[derive(Debug, Clone, Deserialize)]
pub struct CdpResponse {
    pub id: u64,
    pub result: Option<serde_json::Value>,
    pub error: Option<serde_json::Value>,
}

/// Incoming CDP message — either a response (has id) or an event (has method, no id).
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum CdpIncoming {
    Response(CdpResponse),
    Event(CdpEvent),
}

/// CDP WebSocket client. Thread-safe: cloneable handle backed by Arc internals.
#[derive(Clone)]
pub struct CdpClient {
    /// WebSocket write half — protected by mutex for concurrent sends.
    writer: Arc<Mutex<futures_util::stream::SplitSink<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        WsMessage,
    >>>,
    /// Monotonically increasing command ID.
    next_id: Arc<AtomicU64>,
    /// Pending command responses: id -> oneshot sender.
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<CdpResponse>>>>,
    /// Broadcast channel for CDP events.
    event_tx: broadcast::Sender<CdpEvent>,
    /// True while the reader loop is alive.
    alive: Arc<AtomicBool>,
}

impl CdpClient {
    /// Connect to a CDP WebSocket endpoint (e.g. ws://127.0.0.1:9222/devtools/page/xxx).
    pub async fn connect(ws_url: &str) -> Result<Self> {
        let (stream, _) = tokio_tungstenite::connect_async(ws_url)
            .await
            .map_err(|e| anyhow::anyhow!("CDP WebSocket connect failed: {e}"))?;
        let (writer, reader) = stream.split();

        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<CdpResponse>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let (event_tx, _) = broadcast::channel(256);

        let alive = Arc::new(AtomicBool::new(true));

        let client = Self {
            writer: Arc::new(Mutex::new(writer)),
            next_id: Arc::new(AtomicU64::new(1)),
            pending: pending.clone(),
            event_tx: event_tx.clone(),
            alive: alive.clone(),
        };

        // Spawn reader task to dispatch incoming messages.
        let pending_r = pending;
        let event_tx_r = event_tx;
        tokio::spawn(async move {
            Self::reader_loop(reader, pending_r, event_tx_r, alive).await;
        });

        // Enable Page and Runtime domains by default.
        client.send("Page.enable", serde_json::json!({})).await?;
        client.send("Runtime.enable", serde_json::json!({})).await?;

        Ok(client)
    }

    /// Send a CDP command and wait for its response.
    pub async fn send(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value> {
        if !self.alive.load(Ordering::Relaxed) {
            bail!("CDP connection is closed");
        }

        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let cmd = CdpCommand {
            id,
            method: method.to_string(),
            params,
        };

        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);

        let json = serde_json::to_string(&cmd)?;
        self.writer
            .lock()
            .await
            .send(WsMessage::Text(json.into()))
            .await
            .map_err(|e| anyhow::anyhow!("CDP send failed: {e}"))?;

        let resp = match tokio::time::timeout(std::time::Duration::from_secs(30), rx).await {
            Ok(Ok(resp)) => resp,
            Ok(Err(_)) => bail!("CDP connection lost while waiting for: {}", cmd.method),
            Err(_) => {
                // Clean up leaked pending entry on timeout
                self.pending.lock().await.remove(&id);
                bail!("CDP command timed out: {}", cmd.method);
            }
        };

        if let Some(err) = resp.error {
            bail!("CDP error: {err}");
        }
        Ok(resp.result.unwrap_or(serde_json::json!({})))
    }

    /// Subscribe to CDP events.
    pub fn subscribe(&self) -> broadcast::Receiver<CdpEvent> {
        self.event_tx.subscribe()
    }

    /// Background reader loop: dispatches responses to pending waiters, events to broadcast.
    /// On exit, marks alive=false and drains all pending senders so callers get errors fast.
    async fn reader_loop(
        mut reader: futures_util::stream::SplitStream<
            tokio_tungstenite::WebSocketStream<
                tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
            >,
        >,
        pending: Arc<Mutex<HashMap<u64, oneshot::Sender<CdpResponse>>>>,
        event_tx: broadcast::Sender<CdpEvent>,
        alive: Arc<AtomicBool>,
    ) {
        while let Some(Ok(msg)) = reader.next().await {
            let text = match msg {
                WsMessage::Text(t) => t.to_string(),
                WsMessage::Close(_) => break,
                _ => continue,
            };

            let Ok(incoming) = serde_json::from_str::<CdpIncoming>(&text) else {
                continue;
            };

            match incoming {
                CdpIncoming::Response(resp) => {
                    if let Some(tx) = pending.lock().await.remove(&resp.id) {
                        let _ = tx.send(resp);
                    }
                }
                CdpIncoming::Event(event) => {
                    let _ = event_tx.send(event);
                }
            }
        }

        // Connection dead — notify callers immediately
        alive.store(false, Ordering::Relaxed);
        // Drop all pending senders so their receivers get RecvError immediately
        pending.lock().await.drain();
    }
}
