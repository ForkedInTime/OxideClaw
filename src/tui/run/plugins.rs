//! Plugin / marketplace install tasks and the upgrade check.
//! Split out of `tui/run.rs` mechanically — no behaviour change.

use super::*;

/// Install a plugin from npm and register it as an MCP server.
/// `spec` is either "marketplace:<user/repo>" or a direct npm package spec (e.g. "context-mode@context-mode").
/// Check git log for the current commit hash, then fetch the latest release from GitHub.
pub(super) async fn upgrade_check_task(tx: tokio::sync::mpsc::UnboundedSender<AppEvent>) {
    let current_version = env!("CARGO_PKG_VERSION");

    // Get current git commit hash (best-effort)
    let git_hash = tokio::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .current_dir(std::env::current_dir().unwrap_or_default())
        .output()
        .await
        .ok()
        .and_then(|o| {
            if o.status.success() {
                String::from_utf8(o.stdout)
                    .ok()
                    .map(|s| s.trim().to_string())
            } else {
                None
            }
        });

    // Fetch latest OxideClaw release from GitHub API
    let latest = async {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(8))
            .user_agent("oxideclaw")
            .build()?;
        let resp: serde_json::Value = client
            .get("https://api.github.com/repos/ForkedInTime/OxideClaw/releases/latest")
            .send()
            .await?
            .json()
            .await?;
        anyhow::Ok(resp["tag_name"].as_str().unwrap_or("unknown").to_string())
    }
    .await;

    let hash_str = git_hash.map(|h| format!(" ({})", h)).unwrap_or_default();
    let msg = match latest {
        Ok(tag) => format!(
            "Upgrade Check\n\n\
             oxideclaw v{current_version}{hash_str}\n\
             Latest release: {tag}\n\n\
             To rebuild from source:\n\
               cd ~/Projects/OxideClaw\n\
               git pull\n\
               cargo build --release"
        ),
        Err(_) => format!(
            "Upgrade Check\n\n\
             oxideclaw v{current_version}{hash_str}\n\
             (Could not reach GitHub — check your connection)\n\n\
             To rebuild from source:\n\
               cd ~/Projects/OxideClaw\n\
               git pull\n\
               cargo build --release"
        ),
    };
    let _ = tx.send(AppEvent::UpgradeCheckDone { message: msg });
}

/// Detect the best available JS package manager: bun > pnpm > npm.
pub(super) fn detect_package_manager() -> (&'static str, &'static str) {
    // Returns (command, runner) — e.g. ("bun", "bunx"), ("pnpm", "pnpx"), ("npm", "npx")
    fn has(cmd: &str) -> bool {
        std::process::Command::new(cmd)
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
    if has("bun") {
        ("bun", "bunx")
    } else if has("pnpm") {
        ("pnpm", "pnpx")
    } else {
        ("npm", "npx")
    }
}

const MARKETPLACE_CLONE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// `git clone` for a marketplace plugin that can never ask for input. A
/// typo'd, private or renamed repo makes GitHub answer 401, and git then
/// prompts for a username on /dev/tty, which is the TUI's terminal: the
/// prompt was drawn over the UI, it read the keys the user typed, and the
/// spinner ran forever. Credential helpers still work; only the prompt goes.
fn marketplace_clone_cmd(url: &str, dir: &std::path::Path) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new("git");
    cmd.args(["clone", "--depth", "1", url])
        .arg(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    // An insteadOf rewrite to ssh would prompt for a passphrase or host key
    // on the same tty; keep any ssh command the user configured themselves.
    if std::env::var_os("GIT_SSH_COMMAND").is_none() && std::env::var_os("GIT_SSH").is_none() {
        cmd.env("GIT_SSH_COMMAND", "ssh -oBatchMode=yes");
    }
    cmd
}

pub(super) async fn plugin_install_task(
    spec: String,
    tx: tokio::sync::mpsc::UnboundedSender<AppEvent>,
) {
    let (is_marketplace, raw_spec) = if let Some(repo) = spec.strip_prefix("marketplace:") {
        (true, repo.to_string())
    } else {
        (false, spec.clone())
    };

    let result: anyhow::Result<String> = async {
        let (pm, pm_runner) = tokio::task::spawn_blocking(detect_package_manager)
            .await
            .unwrap_or(("npm", "npx"));

        if is_marketplace {
            // ── Marketplace install: git clone + install deps locally ─────────
            let marketplace_dir = dirs::home_dir()
                .ok_or_else(|| anyhow::anyhow!("Cannot find home directory"))?
                .join(".claude")
                .join("plugins")
                .join("marketplaces");
            std::fs::create_dir_all(&marketplace_dir)?;

            let safe_name = raw_spec.replace('/', "-");
            let clone_dir = marketplace_dir.join(&safe_name);

            // Clone (shallow) — if already cloned, remove and re-clone for a
            // clean state.  We strip .git/ after cloning so updates always
            // start fresh rather than accumulating history.
            if clone_dir.exists() {
                let _ = tokio::fs::remove_dir_all(&clone_dir).await;
            }
            let url = format!("https://github.com/{}.git", raw_spec);
            let output = tokio::time::timeout(
                MARKETPLACE_CLONE_TIMEOUT,
                marketplace_clone_cmd(&url, &clone_dir).output(),
            )
            .await
            .map_err(|_| {
                anyhow::anyhow!(
                    "git clone timed out after {}s",
                    MARKETPLACE_CLONE_TIMEOUT.as_secs()
                )
            })?
            .map_err(|e| anyhow::anyhow!("git clone failed: {e}"))?;
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                anyhow::bail!("git clone failed:\n{}", stderr.trim());
            }
            // Remove .git/ — we don't need history at runtime and it's
            // the bulk of the cloned data.
            let _ = tokio::fs::remove_dir_all(clone_dir.join(".git")).await;

            // Read package.json for the plugin name
            let pkg_path = clone_dir.join("package.json");
            let npm_name = if pkg_path.exists() {
                let pkg: serde_json::Value = serde_json::from_str(
                    &tokio::fs::read_to_string(&pkg_path).await.unwrap_or_default()
                ).unwrap_or_default();
                pkg["name"].as_str()
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| raw_spec.split('/').next_back().unwrap_or(&raw_spec).to_string())
            } else {
                raw_spec.split('/').next_back().unwrap_or(&raw_spec).to_string()
            };

            // Install dependencies in the cloned directory
            let install_output = tokio::process::Command::new(pm)
                .args(["install"])
                .current_dir(&clone_dir)
                .output()
                .await
                .map_err(|e| anyhow::anyhow!("{pm} install failed: {e}"))?;

            if !install_output.status.success() {
                let stderr = String::from_utf8_lossy(&install_output.stderr);
                anyhow::bail!("{pm} install failed:\n{}", stderr.trim());
            }

            // Build if a build script exists
            let has_build = if let Ok(pkg_str) = tokio::fs::read_to_string(&pkg_path).await {
                let pkg: serde_json::Value = serde_json::from_str(&pkg_str).unwrap_or_default();
                pkg["scripts"]["build"].is_string()
            } else { false };
            if has_build {
                let _ = tokio::process::Command::new(pm)
                    .args(["run", "build"])
                    .current_dir(&clone_dir)
                    .output()
                    .await;
            }

            // Find entry point for MCP server registration
            let bin_path = clone_dir.join("node_modules").join(".bin").join(&npm_name);
            let main_path = clone_dir.join("index.js");
            let server_cfg = if bin_path.exists() {
                serde_json::json!({ "command": bin_path.to_string_lossy().as_ref(), "args": [] })
            } else if main_path.exists() {
                serde_json::json!({ "command": "node", "args": [main_path.to_string_lossy().as_ref()] })
            } else {
                serde_json::json!({ "command": pm_runner, "args": ["-y", &npm_name] })
            };

            // Register MCP server in settings.json
            register_mcp_server(&npm_name, server_cfg).await?;

            // Track in plugins.json
            track_plugin(&npm_name, &raw_spec, true).await?;

            Ok(format!(
                "Plugin '{npm_name}' installed successfully (via {pm}).\n\
                 Registered as MCP server '{npm_name}' in ~/.claude/settings.json.\n\
                 \n\
                 Restart oxideclaw for the plugin to take effect.\n\
                 After restart, verify with: /{npm_name}:ctx-doctor"
            ))
        } else {
            // ── npm/bun/pnpm registry install ────────────────────────────────
            let plugins_dir = dirs::home_dir()
                .ok_or_else(|| anyhow::anyhow!("Cannot find home directory"))?
                .join(".claude")
                .join("plugins");
            std::fs::create_dir_all(&plugins_dir)?;

            let npm_name = raw_spec.split('@').next().unwrap_or(&raw_spec).to_string();

            let output = tokio::process::Command::new(pm)
                .args(["install", "--prefix", &plugins_dir.to_string_lossy(), &raw_spec])
                .output()
                .await
                .map_err(|e| anyhow::anyhow!("{pm} not found — is a JS runtime installed?\n{e}"))?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                anyhow::bail!("{pm} install failed:\n{}", stderr.trim());
            }

            let bin_path = plugins_dir.join("node_modules").join(".bin").join(&npm_name);
            let server_cfg = if bin_path.exists() {
                serde_json::json!({ "command": bin_path.to_string_lossy().as_ref(), "args": [] })
            } else {
                serde_json::json!({ "command": pm_runner, "args": ["-y", &npm_name] })
            };

            register_mcp_server(&npm_name, server_cfg).await?;
            track_plugin(&npm_name, &raw_spec, false).await?;

            Ok(format!(
                "Plugin '{npm_name}' installed successfully (via {pm}).\n\
                 Registered as MCP server '{npm_name}' in ~/.claude/settings.json.\n\
                 \n\
                 Restart oxideclaw for the plugin to take effect."
            ))
        }
    }.await;

    let (success, message) = match result {
        Ok(msg) => (true, msg),
        Err(e) => (false, format!("Plugin install failed:\n{}", e)),
    };
    let _ = tx.send(AppEvent::PluginInstallDone { success, message });
}

/// Register an MCP server entry in the global settings.json.
pub(super) async fn register_mcp_server(
    name: &str,
    server_cfg: serde_json::Value,
) -> anyhow::Result<()> {
    let settings_path = crate::config::Config::claude_dir().join("settings.json");
    let name = name.to_string();
    tokio::task::spawn_blocking(move || add_mcp_server_entry(&settings_path, &name, server_cfg))
        .await?
}

/// Read-modify-write that refuses a settings.json it cannot parse: falling
/// back to `{}` would replace the user's whole file with just `mcpServers`.
fn add_mcp_server_entry(
    settings_path: &std::path::Path,
    name: &str,
    server_cfg: serde_json::Value,
) -> anyhow::Result<()> {
    let mut json = crate::config::read_json_object(settings_path)?;
    if !json.get("mcpServers").is_some_and(|v| v.is_object()) {
        json["mcpServers"] = serde_json::json!({});
    }
    json["mcpServers"][name] = server_cfg;
    crate::config::write_json_atomic(settings_path, &serde_json::to_string_pretty(&json)?)?;
    Ok(())
}

/// Track a plugin in ~/.claude/plugins.json.
pub(super) async fn track_plugin(name: &str, spec: &str, marketplace: bool) -> anyhow::Result<()> {
    let plugins_path = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("Cannot determine home directory"))?
        .join(".claude")
        .join("plugins.json");
    let mut plugins: serde_json::Value = if plugins_path.exists() {
        let content = tokio::fs::read_to_string(&plugins_path)
            .await
            .unwrap_or_default();
        serde_json::from_str(&content).unwrap_or(serde_json::json!({}))
    } else {
        serde_json::json!({})
    };
    plugins[name] = serde_json::json!({
        "spec": spec,
        "marketplace": marketplace,
    });
    tokio::fs::write(&plugins_path, serde_json::to_string_pretty(&plugins)?).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::add_mcp_server_entry;

    #[test]
    fn plugin_install_never_overwrites_an_unparsable_settings_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let broken = r#"{"permissions": {"deny": ["Bash(rm:*)"]},}"#;
        std::fs::write(&path, broken).unwrap();

        let err = add_mcp_server_entry(&path, "p", serde_json::json!({"command": "x"}));
        assert!(err.is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), broken);
    }

    #[test]
    fn plugin_install_keeps_existing_settings() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, r#"{"permissions": {"deny": ["Bash(rm:*)"]}}"#).unwrap();

        add_mcp_server_entry(&path, "p", serde_json::json!({"command": "x"})).unwrap();
        let json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(json["permissions"]["deny"][0], "Bash(rm:*)");
        assert_eq!(json["mcpServers"]["p"]["command"], "x");
    }
}

#[cfg(test)]
mod marketplace_clone_tests {
    use super::*;

    /// A clone that needs credentials must fail fast instead of prompting on
    /// the TUI's terminal: GIT_TERMINAL_PROMPT=0 is what turns git's
    /// "Username for 'https://github.com':" into an immediate error.
    #[test]
    fn marketplace_clone_never_prompts_on_the_terminal() {
        let cmd = marketplace_clone_cmd(
            "https://github.com/x/y.git",
            std::path::Path::new("/nonexistent/y"),
        );
        let envs: Vec<_> = cmd.as_std().get_envs().collect();
        assert!(envs.contains(&(
            std::ffi::OsStr::new("GIT_TERMINAL_PROMPT"),
            Some(std::ffi::OsStr::new("0"))
        )));
        let args: Vec<_> = cmd.as_std().get_args().collect();
        assert_eq!(
            args[..4],
            ["clone", "--depth", "1", "https://github.com/x/y.git"]
        );
    }
}
