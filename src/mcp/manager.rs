/// McpManager — connects to all MCP servers from settings.json at startup.
///
/// Each server runs independently; failures are logged and skipped so a
/// broken MCP server never prevents oxideclaw from starting.
use crate::mcp::client::McpClient;
use crate::mcp::types::{McpServerConfig, McpServerStatus};
use crate::settings::Settings;
use std::sync::Arc;

pub struct McpManager {
    pub clients: Vec<Arc<McpClient>>,
}

/// How long startup waits for any one server to finish `initialize`.
pub const PER_SERVER_STARTUP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

impl McpManager {
    /// Start all MCP servers listed in settings + any injected via CLI --mcp-config.
    /// Errors per-server are logged; the manager is always returned.
    pub async fn start_with_extra(
        settings: &Settings,
        extra: &std::collections::HashMap<String, McpServerConfig>,
    ) -> Self {
        Self::start_with_extra_timeout(settings, extra, PER_SERVER_STARTUP_TIMEOUT).await
    }

    /// Servers are connected **concurrently**, each under `per_server`. One
    /// hung server used to block startup for the full 60 s request timeout,
    /// and N of them serialized — with a hard failure being the only exit.
    pub async fn start_with_extra_timeout(
        settings: &Settings,
        extra: &std::collections::HashMap<String, McpServerConfig>,
        per_server: std::time::Duration,
    ) -> Self {
        let mut all: std::collections::HashMap<String, McpServerConfig> =
            settings.mcp_servers.clone();
        // extra (CLI --mcp-config) wins over settings on name conflicts
        for (k, v) in extra {
            all.insert(k.clone(), v.clone());
        }
        // Stable order so tool registration is deterministic across runs.
        let mut entries: Vec<(String, McpServerConfig)> = all.into_iter().collect();
        entries.sort_by(|a, b| a.0.cmp(&b.0));

        let handles: Vec<_> = entries
            .into_iter()
            .map(|(name, cfg)| {
                tokio::spawn(async move {
                    let result =
                        tokio::time::timeout(per_server, Self::connect_one(name.clone(), &cfg))
                            .await;
                    (name, result)
                })
            })
            .collect();

        let mut clients = Vec::new();
        for h in handles {
            let Ok((name, result)) = h.await else {
                continue;
            };
            match result {
                Ok(Ok(client)) => {
                    tracing::info!(
                        "MCP '{}': connected ({} tools via {})",
                        name,
                        client.tools.len(),
                        client.transport_kind
                    );
                    clients.push(Arc::new(client));
                }
                Ok(Err(e)) => {
                    tracing::warn!("MCP '{}': failed to connect — {}", name, e);
                }
                Err(_) => {
                    tracing::warn!(
                        "MCP '{}': no response within {:?} at startup — skipped",
                        name,
                        per_server
                    );
                }
            }
        }
        Self { clients }
    }

    async fn connect_one(name: String, cfg: &McpServerConfig) -> anyhow::Result<McpClient> {
        match cfg {
            McpServerConfig::Stdio(s) => {
                McpClient::connect_stdio(name, &s.command, &s.args, &s.env).await
            }
            McpServerConfig::Http(h) => McpClient::connect_http(name, &h.url, &h.headers).await,
        }
    }

    /// Return a snapshot of server statuses for the /mcp command.
    pub fn statuses(&self) -> Vec<McpServerStatus> {
        self.clients
            .iter()
            .map(|c| McpServerStatus {
                name: c.server_name.clone(),
                transport: c.transport_kind,
                tool_count: c.tools.len(),
            })
            .collect()
    }
}

#[cfg(all(test, unix))]
mod startup_tests {
    use super::*;
    use crate::mcp::types::StdioServerConfig;

    fn hung_server(name: &str) -> (String, McpServerConfig) {
        (
            name.to_string(),
            McpServerConfig::Stdio(StdioServerConfig {
                command: "sh".into(),
                args: vec!["-c".into(), "sleep 30".into()],
                env: Default::default(),
            }),
        )
    }

    /// Three servers that never answer must cost one timeout, not three, and
    /// must not take the whole session down with them.
    #[tokio::test]
    async fn hung_servers_are_skipped_concurrently_within_the_per_server_budget() {
        let extra: std::collections::HashMap<_, _> =
            [hung_server("a"), hung_server("b"), hung_server("c")]
                .into_iter()
                .collect();
        let started = std::time::Instant::now();
        let m = McpManager::start_with_extra_timeout(
            &Settings::default(),
            &extra,
            std::time::Duration::from_millis(500),
        )
        .await;
        assert!(m.clients.is_empty());
        assert!(
            started.elapsed() < std::time::Duration::from_secs(1),
            "took {:?}: servers were connected sequentially or untimed",
            started.elapsed()
        );
    }
}
