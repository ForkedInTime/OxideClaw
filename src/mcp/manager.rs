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
    /// Servers that failed or timed out at startup, by name.
    pub failed: Vec<String>,
}

/// How long startup waits for any one server to finish `initialize`.
pub const PER_SERVER_STARTUP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

impl McpManager {
    /// Start the servers a session built from `cfg` should have: settings.json
    /// and .mcp.json (trust-gated inside `Settings::load`) unless
    /// --strict-mcp-config, plus --mcp-config / ACP `session/new` servers.
    pub async fn start_for_config(cfg: &crate::config::Config) -> Self {
        let settings = if cfg.strict_mcp_config {
            Settings::default()
        } else {
            Settings::load(&cfg.cwd)
        };
        Self::start_with_extra(&settings, &cfg.extra_mcp_servers, &cfg.cwd).await
    }

    /// Start all MCP servers listed in settings + any injected via CLI --mcp-config.
    /// Errors per-server are logged; the manager is always returned. Stdio
    /// servers start in `cwd`, the session's project directory: SDK and ACP
    /// sessions name their own, which need not be the process directory.
    pub async fn start_with_extra(
        settings: &Settings,
        extra: &std::collections::HashMap<String, McpServerConfig>,
        cwd: &std::path::Path,
    ) -> Self {
        Self::start_with_extra_timeout(settings, extra, PER_SERVER_STARTUP_TIMEOUT, cwd).await
    }

    /// Servers are connected **concurrently**, each under `per_server`. One
    /// hung server used to block startup for the full 60 s request timeout,
    /// and N of them serialized — with a hard failure being the only exit.
    pub async fn start_with_extra_timeout(
        settings: &Settings,
        extra: &std::collections::HashMap<String, McpServerConfig>,
        per_server: std::time::Duration,
        cwd: &std::path::Path,
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
        entries.retain(|(name, cfg)| {
            if cfg.is_disabled() {
                tracing::info!("MCP '{}': disabled in settings — skipped", name);
            }
            !cfg.is_disabled()
        });

        let handles: Vec<_> = entries
            .into_iter()
            .map(|(name, cfg)| {
                let cwd = cwd.to_path_buf();
                tokio::spawn(async move {
                    let result = tokio::time::timeout(
                        per_server,
                        Self::connect_one(name.clone(), &cfg, &cwd),
                    )
                    .await;
                    (name, result)
                })
            })
            .collect();

        let mut clients = Vec::new();
        let mut failed = Vec::new();
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
                    failed.push(name);
                }
                Err(_) => {
                    tracing::warn!(
                        "MCP '{}': no response within {:?} at startup — skipped",
                        name,
                        per_server
                    );
                    failed.push(name);
                }
            }
        }
        Self { clients, failed }
    }

    async fn connect_one(
        name: String,
        cfg: &McpServerConfig,
        cwd: &std::path::Path,
    ) -> anyhow::Result<McpClient> {
        // Expanded here, not at load, so `/mcp list` and anything that
        // writes the config back keep the placeholders, not the secrets.
        let lookup = |k: &str| std::env::var(k).ok();
        let x = |v: &str| expand_vars(v, &lookup);
        match cfg {
            McpServerConfig::Stdio(s) => {
                let command = x(&s.command)?;
                let args = s.args.iter().map(|a| x(a)).collect::<Result<Vec<_>, _>>()?;
                let env = s
                    .env
                    .iter()
                    .map(|(k, v)| Ok((k.clone(), x(v)?)))
                    .collect::<anyhow::Result<_>>()?;
                McpClient::connect_stdio(name, &command, &args, &env, cwd).await
            }
            McpServerConfig::Http(h) => {
                let url = x(&h.url)?;
                let headers = h
                    .headers
                    .iter()
                    .map(|(k, v)| Ok((k.clone(), x(v)?)))
                    .collect::<anyhow::Result<_>>()?;
                McpClient::connect_http(name, &url, &headers).await
            }
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

/// Expand `${NAME}` and `${NAME:-default}` as Claude Code does in
/// .mcp.json, so a shared config can name a secret without containing it.
/// Passing the placeholder through literally replaced the real value in the
/// server's environment, or sent `Bearer ${TOKEN}` as the credential. An
/// unset variable with no default is an error: starting the server with an
/// empty token only moves the failure somewhere harder to read.
fn expand_vars(s: &str, lookup: &dyn Fn(&str) -> Option<String>) -> anyhow::Result<String> {
    let is_name = |n: &str| {
        let mut b = n.bytes();
        b.next()
            .is_some_and(|c| c == b'_' || c.is_ascii_alphabetic())
            && b.all(|c| c == b'_' || c.is_ascii_alphanumeric())
    };
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let body = &rest[start + 2..];
        let Some(end) = body.find('}') else {
            out.push_str(&rest[start..]);
            return Ok(out);
        };
        let inner = &body[..end];
        let (var, default) = match inner.split_once(":-") {
            Some((v, d)) => (v, Some(d)),
            None => (inner, None),
        };
        if !is_name(var) {
            // Not ours (a shell `${1}` in an `sh -c` arg, say): keep it.
            out.push_str(&rest[start..start + 2]);
            rest = body;
            continue;
        }
        match (
            lookup(var).filter(|v| default.is_none() || !v.is_empty()),
            default,
        ) {
            (Some(v), _) => out.push_str(&v),
            (None, Some(d)) => out.push_str(d),
            (None, None) => {
                anyhow::bail!("environment variable {var} is not set (used as ${{{var}}})")
            }
        }
        rest = &body[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

#[cfg(test)]
mod expand_tests {
    use super::expand_vars;

    fn env(k: &str) -> Option<String> {
        match k {
            "TOKEN" => Some("s3cret".into()),
            "EMPTY" => Some(String::new()),
            _ => None,
        }
    }

    #[test]
    fn placeholders_take_the_environment_value_or_the_default() {
        let x = |s: &str| expand_vars(s, &env).unwrap();
        assert_eq!(x("Bearer ${TOKEN}"), "Bearer s3cret");
        assert_eq!(x("${TOKEN}-${TOKEN}"), "s3cret-s3cret");
        assert_eq!(
            x("${MISSING:-http://localhost:3000}/mcp"),
            "http://localhost:3000/mcp"
        );
        assert_eq!(x("${TOKEN:-unused}"), "s3cret");
        assert_eq!(x("${EMPTY:-fallback}"), "fallback");
        assert_eq!(x("${EMPTY}"), "");
        assert_eq!(x("plain"), "plain");
    }

    #[test]
    fn non_variable_braces_are_left_alone() {
        let x = |s: &str| expand_vars(s, &env).unwrap();
        assert_eq!(x("echo ${1} ${@}"), "echo ${1} ${@}");
        assert_eq!(x("cost $5 and ${TOKEN"), "cost $5 and ${TOKEN");
        assert_eq!(x("${ TOKEN}"), "${ TOKEN}");
    }

    #[test]
    fn an_unset_variable_without_default_names_itself() {
        let err = expand_vars("${GITHUB_TOKEN}", &env).unwrap_err();
        assert!(err.to_string().contains("GITHUB_TOKEN"), "{err}");
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
                disabled: false,
            }),
        )
    }

    /// A minimal stdio MCP server: answers initialize (id 1) and tools/list
    /// (id 2, after the initialized notification) with one `ping` tool.
    fn fake_server(name: &str) -> (String, McpServerConfig) {
        let script = r#"read l
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"fake","version":"0"}}}'
read l; read l
printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"ping","description":"p","inputSchema":{"type":"object"}}]}}'
cat >/dev/null"#;
        (
            name.to_string(),
            McpServerConfig::Stdio(StdioServerConfig {
                command: "sh".into(),
                args: vec!["-c".into(), script.into()],
                env: Default::default(),
                disabled: false,
            }),
        )
    }

    /// Print mode, the SDK and ACP used to build tools without ever starting
    /// MCP, so --mcp-config and ACP `session/new` servers were dropped.
    #[tokio::test]
    async fn tools_for_config_includes_configured_servers() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = crate::config::Config {
            cwd: dir.path().to_path_buf(),
            strict_mcp_config: true,
            extra_mcp_servers: [fake_server("fake")].into_iter().collect(),
            ..Default::default()
        };
        let tools = crate::mcp::tools_for_config(&cfg).await;
        let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        assert!(names.contains(&"mcp__fake__ping"), "{names:?}");
        assert!(names.contains(&"Read"), "built-ins must still be there");
    }

    /// SDK and ACP sessions name their own cwd, usually not the directory the
    /// editor launched the process in: a project server like
    /// `./tools/mcp.sh` failed to start, or worked on the wrong tree.
    #[tokio::test]
    async fn stdio_servers_start_in_the_session_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let (_, server) = fake_server("rel");
        let McpServerConfig::Stdio(s) = &server else {
            unreachable!()
        };
        let script = s.args[1].clone();
        std::fs::write(dir.path().join("server.sh"), script).unwrap();
        let rel = McpServerConfig::Stdio(StdioServerConfig {
            command: "sh".into(),
            args: vec!["./server.sh".into()],
            env: Default::default(),
            disabled: false,
        });
        let cfg = crate::config::Config {
            cwd: dir.path().to_path_buf(),
            strict_mcp_config: true,
            extra_mcp_servers: [("rel".to_string(), rel)].into_iter().collect(),
            ..Default::default()
        };
        let tools = crate::mcp::tools_for_config(&cfg).await;
        let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        assert!(names.contains(&"mcp__rel__ping"), "{names:?}");
    }

    /// `.mcp.json` placeholders reached the server literally: the tool
    /// name below comes from an env value and an arg, both defaulted.
    #[tokio::test]
    async fn placeholders_are_expanded_before_the_server_starts() {
        let dir = tempfile::tempdir().unwrap();
        let (_, server) = fake_server("vars");
        let McpServerConfig::Stdio(s) = &server else {
            unreachable!()
        };
        let script = s.args[1].replace(r#""name":"ping""#, r#""name":"'"$TOOL$1"'""#);
        let unset = "OXIDECLAW_TEST_SURELY_UNSET_VAR";
        let cfg = McpServerConfig::Stdio(StdioServerConfig {
            command: "sh".into(),
            args: vec![
                "-c".into(),
                script,
                "sh".into(),
                format!("${{{unset}:-_two}}"),
            ],
            env: [("TOOL".to_string(), format!("${{{unset}:-one}}"))].into(),
            disabled: false,
        });
        let cfg = crate::config::Config {
            cwd: dir.path().to_path_buf(),
            strict_mcp_config: true,
            extra_mcp_servers: [("vars".to_string(), cfg)].into_iter().collect(),
            ..Default::default()
        };
        let tools = crate::mcp::tools_for_config(&cfg).await;
        let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        assert!(names.contains(&"mcp__vars__one_two"), "{names:?}");
    }

    /// ACP and `--headless` sessions take their tools from here; the CLI
    /// filters used to apply only in -p and the TUI.
    #[tokio::test]
    async fn tools_for_config_applies_the_cli_tool_filters() {
        let dir = tempfile::tempdir().unwrap();
        let base = crate::config::Config {
            cwd: dir.path().to_path_buf(),
            strict_mcp_config: true,
            ..Default::default()
        };
        let names = |tools: Vec<crate::tools::DynTool>| -> Vec<String> {
            tools.iter().map(|t| t.name().to_string()).collect()
        };

        let cfg = crate::config::Config {
            disallowed_tools: vec!["bash".into(), "Write".into()],
            ..base.clone()
        };
        let got = names(crate::mcp::tools_for_config(&cfg).await);
        assert!(!got.iter().any(|n| n == "Bash" || n == "Write"), "{got:?}");
        assert!(got.iter().any(|n| n == "Read"), "{got:?}");

        let cfg = crate::config::Config {
            allowed_tools: vec!["read".into(), "Grep".into()],
            ..base.clone()
        };
        let mut got = names(crate::mcp::tools_for_config(&cfg).await);
        got.sort();
        assert_eq!(got, ["Grep", "Read"]);

        let cfg = crate::config::Config {
            allowed_tools: vec!["__none__".into()],
            ..base
        };
        assert!(crate::mcp::tools_for_config(&cfg).await.is_empty());
    }

    /// `/mcp disable` and `/plugin disable` write `"disabled": true` into the
    /// server's settings entry; that server must not be launched.
    #[tokio::test]
    async fn disabled_servers_are_not_started() {
        let (_, on) = fake_server("on");
        let mut off = serde_json::to_value(&on).unwrap();
        assert!(
            off.get("disabled").is_none(),
            "false must not be written out"
        );
        off["disabled"] = serde_json::json!(true);
        let off: McpServerConfig = serde_json::from_value(off).unwrap();
        assert!(off.is_disabled());

        let extra: std::collections::HashMap<_, _> =
            [("on".to_string(), on), ("off".to_string(), off)]
                .into_iter()
                .collect();
        let m = McpManager::start_with_extra_timeout(
            &Settings::default(),
            &extra,
            std::time::Duration::from_secs(10),
            std::path::Path::new("."),
        )
        .await;
        let started: Vec<&str> = m.clients.iter().map(|c| c.server_name.as_str()).collect();
        assert_eq!(started, ["on"]);
        assert!(m.failed.is_empty(), "{:?}", m.failed);
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
            std::path::Path::new("."),
        )
        .await;
        assert!(m.clients.is_empty());
        assert_eq!(m.failed, ["a", "b", "c"]);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(1),
            "took {:?}: servers were connected sequentially or untimed",
            started.elapsed()
        );
    }
}
