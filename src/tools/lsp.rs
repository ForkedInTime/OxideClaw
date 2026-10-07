/// LSPTool — port of lsp.ts
/// Communicates with language servers via JSON-RPC 2.0 over stdio using LSP protocol.
/// Supports: goToDefinition, findReferences, hover, documentSymbol, workspaceSymbol,
///           goToImplementation, prepareCallHierarchy, incomingCalls, outgoingCalls
use super::{Tool, ToolContext, ToolOutput, async_trait};
use anyhow::{Result, anyhow};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::ChildStdin;
use tokio::sync::{Mutex, oneshot, watch};
use tokio::time::Instant;

/// One language server per (command, project root). A fresh server per call
/// meant paying rust-analyzer's full startup and indexing on every query.
type ClientCache = Arc<Mutex<HashMap<(String, PathBuf), Arc<LspClient>>>>;

/// The session's language servers: the LSP tool and the auto-fix
/// diagnostics share them, so a server starts once whichever needs it first.
#[derive(Clone, Default)]
pub struct LspPool {
    clients: ClientCache,
    /// Servers the auto-fix check stopped using for this session: one that
    /// crashed, did not answer within the cap, or that the sandbox refused.
    given_up: Arc<std::sync::Mutex<HashSet<(String, PathBuf)>>>,
}

/// How to start a server that is not running yet.
pub(crate) enum Launch {
    /// The command name, found on the process PATH.
    Plain,
    /// This executable (the command resolved on a given PATH).
    Program(PathBuf),
    /// This shell line: the command wrapped by the Bash tool's sandbox.
    Shell(String),
}

impl Launch {
    /// How to start `exe args` in `root` under `containment`'s sandbox: the
    /// executable itself without one, the wrapped shell line with one.
    /// `Err` when the sandbox refuses it: never start it bare then.
    pub(crate) fn contained(
        exe: &Path,
        args: &[String],
        containment: &crate::autofix::Containment,
        root: &Path,
    ) -> Result<Launch, String> {
        let plain = std::iter::once(exe.display().to_string())
            .chain(args.iter().cloned())
            .map(|a| crate::sandbox::shell_quote(&a))
            .collect::<Vec<_>>()
            .join(" ");
        Ok(match containment.wrap(&plain, root)? {
            line if line == plain => Launch::Program(exe.to_path_buf()),
            line => Launch::Shell(line),
        })
    }
}

fn cache_key(command: &str, args: &[String], root: &Path) -> (String, PathBuf) {
    (format!("{command} {}", args.join(" ")), root.to_path_buf())
}

impl LspPool {
    /// The cached, initialised client for this server + root, starting it
    /// with `launch` on first use.
    pub(crate) async fn client_for(
        &self,
        command: &str,
        args: &[String],
        root: &Path,
        launch: &Launch,
    ) -> Result<Arc<LspClient>> {
        let key = cache_key(command, args, root);
        let mut cache = self.clients.lock().await;
        if let Some(c) = cache.get(&key) {
            // A sandboxed caller needs a server started under that exact
            // sandbox line: one started before `/sandbox enable` (or before
            // its network was turned off) runs project code unconfined.
            // Unsandboxed callers take whatever runs.
            let fits = match launch {
                Launch::Shell(line) => c.sandbox_line.as_deref() == Some(line.as_str()),
                Launch::Plain | Launch::Program(_) => true,
            };
            // A server that crashed or was killed would otherwise fail every
            // later call (EPIPE) until OxideClaw restarts: start a new one.
            if !c.dead.load(Ordering::SeqCst) && fits {
                return Ok(Arc::clone(c));
            }
            if let Some(stale) = cache.remove(&key) {
                stale.mark_dead();
                tokio::spawn(async move { stale.shutdown().await });
            }
        }
        let client = LspClient::connect(command, args, root, launch).await?;
        client.initialize(command, root).await?;
        let client = Arc::new(client);
        cache.insert(key, Arc::clone(&client));
        Ok(client)
    }

    /// The running client for this server + root, without starting one.
    pub(crate) async fn running(
        &self,
        command: &str,
        args: &[String],
        root: &Path,
    ) -> Option<Arc<LspClient>> {
        let cache = self.clients.lock().await;
        cache
            .get(&cache_key(command, args, root))
            .filter(|c| !c.dead.load(Ordering::SeqCst))
            .cloned()
    }

    pub(crate) fn give_up(&self, command: &str, args: &[String], root: &Path) {
        if let Ok(mut g) = self.given_up.lock() {
            g.insert(cache_key(command, args, root));
        }
    }

    pub(crate) fn gave_up(&self, command: &str, args: &[String], root: &Path) -> bool {
        self.given_up
            .lock()
            .is_ok_and(|g| g.contains(&cache_key(command, args, root)))
    }

    /// Ask every server to shut down (`shutdown`, then `exit`), killing any
    /// that has not gone within a second or two. For a clean exit.
    pub async fn shutdown(&self) {
        let clients: Vec<Arc<LspClient>> =
            self.clients.lock().await.drain().map(|(_, c)| c).collect();
        futures_util::future::join_all(clients.iter().map(|c| c.shutdown())).await;
    }
}

/// Language servers by file extension, most preferred first. The LSP tool
/// and the auto-fix diagnostics use the first one that is installed.
fn servers_for_ext(ext: &str) -> &'static [(&'static str, &'static [&'static str])] {
    match ext {
        "rs" => &[("rust-analyzer", &[])],
        "py" | "pyi" => &[("pyright-langserver", &["--stdio"]), ("pylsp", &[])],
        "ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs" => {
            &[("typescript-language-server", &["--stdio"])]
        }
        "c" | "cpp" | "cc" | "h" | "hpp" => &[("clangd", &[])],
        "go" => &[("gopls", &[])],
        "java" => &[("jdtls", &[])],
        "rb" => &[("solargraph", &["stdio"])],
        "lua" => &[("lua-language-server", &[])],
        _ => &[],
    }
}

/// A language server for `path` found on `search` (a PATH value): its
/// command, arguments and executable.
pub(crate) fn installed_server(
    path: &Path,
    search: Option<&std::ffi::OsStr>,
) -> Option<(&'static str, Vec<String>, PathBuf)> {
    let ext = path.extension()?.to_str()?;
    servers_for_ext(ext).iter().find_map(|(cmd, args)| {
        let exe = crate::autofix::find_on_path(cmd, search)?;
        Some((*cmd, args.iter().map(|a| a.to_string()).collect(), exe))
    })
}

#[derive(Default)]
pub struct LSPTool {
    pool: LspPool,
}

// ── Input schema ──────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct Input {
    /// LSP operation to perform
    operation: String,
    /// File path (for file-scoped operations)
    #[serde(default)]
    file_path: Option<String>,
    /// 0-based line number
    #[serde(default)]
    line: Option<u32>,
    /// 0-based character offset
    #[serde(default)]
    character: Option<u32>,
    /// Symbol query (for workspaceSymbol)
    #[serde(default)]
    query: Option<String>,
}

#[async_trait]
impl Tool for LSPTool {
    fn name(&self) -> &str {
        "LSP"
    }

    fn description(&self) -> &str {
        "Query a language server for code intelligence. Operations: \
        goToDefinition, findReferences, hover, documentSymbol, workspaceSymbol, \
        goToImplementation, prepareCallHierarchy, incomingCalls, outgoingCalls. \
        Automatically selects the appropriate language server based on file extension; \
        workspaceSymbol without file_path picks it from the project's build files \
        (Cargo.toml, package.json, pyproject.toml, go.mod, ...). \
        Results print locations 1-based as path:line:col, like Read and grep; \
        the line and character inputs are 0-based, so subtract 1 from a result \
        before passing it back."
    }

    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "operation": {
                    "type": "string",
                    "enum": [
                        "goToDefinition", "findReferences", "hover",
                        "documentSymbol", "workspaceSymbol",
                        "goToImplementation", "prepareCallHierarchy",
                        "incomingCalls", "outgoingCalls"
                    ],
                    "description": "LSP operation to perform"
                },
                "file_path": {
                    "type": "string",
                    "description": "Absolute or relative path to the file"
                },
                "line": {
                    "type": "integer",
                    "description": "0-based line number (required for position operations); results print 1-based lines"
                },
                "character": {
                    "type": "integer",
                    "description": "0-based character offset (required for position operations); results print 1-based columns"
                },
                "query": {
                    "type": "string",
                    "description": "Symbol name query (required for workspaceSymbol)"
                }
            },
            "required": ["operation"]
        })
    }

    fn lsp_pool(&self) -> Option<LspPool> {
        Some(self.pool.clone())
    }

    async fn execute(&self, input: serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let input: Input = serde_json::from_value(input)?;

        // Resolve file path
        let file_path = match &input.file_path {
            Some(p) => match super::file_read::resolve_path(p, &ctx.cwd) {
                Ok(resolved) => resolved,
                Err(e) => return Ok(ToolOutput::error(e.to_string())),
            },
            None if input.operation != "workspaceSymbol" => {
                return Ok(ToolOutput::error(
                    "file_path is required for this operation",
                ));
            }
            None => ctx.cwd.clone(),
        };

        // Determine language server command from file extension. A workspace
        // query has no file to go by, so the project's build files pick it.
        let ext = match file_path.extension().and_then(|e| e.to_str()) {
            None if input.operation == "workspaceSymbol" => match project_ext(&file_path) {
                Some(ext) => Some(ext),
                None => {
                    return Ok(ToolOutput::error(
                        "workspaceSymbol: no project build file (Cargo.toml, package.json, \
                             pyproject.toml, go.mod, ...) found here. Pass file_path of any \
                             source file in the project to select a language server.",
                    ));
                }
            },
            ext => ext,
        };
        let candidates = ext.map(servers_for_ext).unwrap_or_default();
        if candidates.is_empty() {
            return Ok(ToolOutput::error(format!(
                "No language server configured for extension: {:?}. \
                Supported: .rs, .py, .ts/.js, .c/.cpp, .go, .java, .rb, .lua",
                ext
            )));
        }
        // The first installed server, else the preferred one, whose failure
        // to start then names what to install.
        let path_var = std::env::var_os("PATH");
        let (command, args) = candidates
            .iter()
            .find(|(cmd, _)| crate::autofix::find_on_path(cmd, path_var.as_deref()).is_some())
            .unwrap_or(&candidates[0]);
        let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();

        // A language server loads the workspace, which runs project code
        // (build scripts, proc macros, plugins): only in a trusted project,
        // and under the Bash tool's sandbox when one is on, as auto-fix does.
        if !ctx.project_trusted {
            return Ok(ToolOutput::error(format!(
                "LSP: not started. Language servers run project code (build scripts, \
                 proc macros, plugins), so '{command}' only runs in a trusted project. \
                 Trust this folder with /trust in the TUI, or add it to \
                 `trustedProjects` in the user settings.json."
            )));
        }
        let launch = match crate::autofix::find_on_path(command, path_var.as_deref()) {
            Some(exe) => {
                let containment = crate::autofix::Containment {
                    trusted: true,
                    sandbox_mode: ctx.sandbox_mode.clone(),
                    sandbox_allow_network: ctx.sandbox_allow_network,
                };
                match Launch::contained(&exe, &args, &containment, &ctx.cwd) {
                    Ok(launch) => launch,
                    Err(reason) => {
                        return Ok(ToolOutput::error(format!(
                            "LSP: '{command}' was not started: {reason}"
                        )));
                    }
                }
            }
            // Not installed: starting the bare name fails and says what to
            // install. A sandbox has no command line to wrap then.
            None if ctx.sandbox_mode.is_some() => {
                return Ok(ToolOutput::error(format!(
                    "Could not start language server '{command}': not found on PATH\n\
                     Make sure it is installed."
                )));
            }
            None => Launch::Plain,
        };

        // One initialised server per (command, root), cached across calls.
        let client = match self
            .pool
            .client_for(command, &args, &ctx.cwd, &launch)
            .await
        {
            Ok(c) => c,
            Err(e) => {
                return Ok(ToolOutput::error(format!(
                    "Could not start language server '{command}': {e}\nMake sure it is installed."
                )));
            }
        };

        // Convert file path to URI
        let uri = path_to_uri(&file_path);

        // Open the document (or send its current text if it is already
        // open) so the server answers about what is on disk.
        // A file that is not UTF-8 (Latin-1 C, legacy Python) is queried
        // without syncing, as before: the server reads it from disk.
        if file_path.is_file() && client.sync_document(&file_path).await.is_ok() {
            // Small delay to let server process the document
            tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
        }

        let position = json!({
            "line": input.line.unwrap_or(0),
            "character": input.character.unwrap_or(0)
        });

        let result = match input.operation.as_str() {
            "goToDefinition" => {
                client
                    .request(
                        "textDocument/definition",
                        json!({
                            "textDocument": { "uri": uri },
                            "position": position
                        }),
                    )
                    .await?
            }
            "findReferences" => {
                client
                    .request(
                        "textDocument/references",
                        json!({
                            "textDocument": { "uri": uri },
                            "position": position,
                            "context": { "includeDeclaration": true }
                        }),
                    )
                    .await?
            }
            "hover" => {
                client
                    .request(
                        "textDocument/hover",
                        json!({
                            "textDocument": { "uri": uri },
                            "position": position
                        }),
                    )
                    .await?
            }
            "documentSymbol" => {
                client
                    .request(
                        "textDocument/documentSymbol",
                        json!({
                            "textDocument": { "uri": uri }
                        }),
                    )
                    .await?
            }
            "workspaceSymbol" => {
                client
                    .request(
                        "workspace/symbol",
                        json!({
                            "query": input.query.as_deref().unwrap_or("")
                        }),
                    )
                    .await?
            }
            "goToImplementation" => {
                client
                    .request(
                        "textDocument/implementation",
                        json!({
                            "textDocument": { "uri": uri },
                            "position": position
                        }),
                    )
                    .await?
            }
            "prepareCallHierarchy" => {
                client
                    .request(
                        "textDocument/prepareCallHierarchy",
                        json!({
                            "textDocument": { "uri": uri },
                            "position": position
                        }),
                    )
                    .await?
            }
            "incomingCalls" => {
                // First prepare
                let items = client
                    .request(
                        "textDocument/prepareCallHierarchy",
                        json!({
                            "textDocument": { "uri": uri },
                            "position": position
                        }),
                    )
                    .await?;
                if let Some(item) = items.as_array().and_then(|a| a.first()) {
                    client
                        .request(
                            "callHierarchy/incomingCalls",
                            json!({
                                "item": item
                            }),
                        )
                        .await?
                } else {
                    Value::Null
                }
            }
            "outgoingCalls" => {
                let items = client
                    .request(
                        "textDocument/prepareCallHierarchy",
                        json!({
                            "textDocument": { "uri": uri },
                            "position": position
                        }),
                    )
                    .await?;
                if let Some(item) = items.as_array().and_then(|a| a.first()) {
                    client
                        .request(
                            "callHierarchy/outgoingCalls",
                            json!({
                                "item": item
                            }),
                        )
                        .await?
                } else {
                    Value::Null
                }
            }
            other => return Ok(ToolOutput::error(format!("Unknown operation: {other}"))),
        };

        let formatted = format_lsp_result(&input.operation, &result);
        Ok(ToolOutput::success(formatted))
    }
}

// ── LSP JSON-RPC Client ───────────────────────────────────────────────────────

/// The diagnostics a server last published for one document.
#[derive(Clone)]
struct Published {
    /// Position in the client's publish sequence.
    seq: u64,
    /// The document version they are for, when the server says.
    version: Option<i64>,
    diagnostics: Vec<Value>,
}

/// What was last sent for one open document.
struct DocState {
    version: i64,
    text: String,
    /// The version and text sent before it.
    previous: Option<(i64, String)>,
}

/// Diagnostics a server published for a synced document.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Reported {
    pub(crate) diagnostics: Vec<Value>,
    /// `None` when they are for the text just synced. Otherwise the server
    /// has not caught up yet and they are its last set, for this older text:
    /// their line numbers belong to it, not to the file as it is now.
    pub(crate) stale_text: Option<String>,
}

/// A document sent to the server by `sync_document`.
pub(crate) struct Synced {
    path: PathBuf,
    version: i64,
    /// The publish sequence number just before it was sent: later publishes
    /// for it may describe this text.
    seq: u64,
}

pub(crate) struct LspClient {
    stdin: Arc<Mutex<ChildStdin>>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value>>>>>,
    id_counter: Arc<AtomicU64>,
    /// Set once the server's stdout closes or a write to it fails. Writing to
    /// a dead server's stdin raises SIGPIPE, which kills `-p` runs outright.
    dead: Arc<AtomicBool>,
    /// `textDocument/publishDiagnostics` by document path.
    published: Arc<std::sync::Mutex<HashMap<PathBuf, Published>>>,
    /// The latest publish sequence number; closes when the server exits.
    publish_seq: watch::Receiver<u64>,
    /// Open documents: the version and text last sent for each, and the
    /// one before, so a set published for that older text is read against it.
    documents: Mutex<HashMap<String, DocState>>,
    /// The sandbox line the server was started with (`Launch::Shell`), or
    /// `None` when it was started unsandboxed.
    sandbox_line: Option<String>,
    /// Owns the language server. `kill_on_drop` means the server lives
    /// exactly as long as this client — previously the handle was dropped at
    /// the end of `connect`, which killed the server before `initialize`.
    child: std::sync::Mutex<Option<tokio::process::Child>>,
}

/// How published diagnostics are looked up by path. Windows servers
/// differ in the drive letter's case (`file:///c%3A/...`).
fn doc_key(path: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        let s = path.to_string_lossy();
        PathBuf::from(s.strip_prefix(r"\\?\").unwrap_or(&*s).to_lowercase())
    }
    #[cfg(not(windows))]
    path.to_path_buf()
}

/// `initializationOptions` for `command`. rust-analyzer builds into
/// `target/rust-analyzer` rather than `target/`: its workspace load runs
/// `cargo check` for build scripts and proc macros, which would otherwise
/// queue on the build-directory lock with auto-fix's own `cargo clippy` and
/// `cargo test` (and the user's builds) and hold them past their timeout.
fn initialization_options(command: &str) -> Value {
    match command {
        "rust-analyzer" => json!({ "cargo": { "targetDir": true } }),
        _ => json!({}),
    }
}

fn frame(msg: &Value) -> Result<String> {
    let body = serde_json::to_string(msg)?;
    Ok(format!("Content-Length: {}\r\n\r\n{}", body.len(), body))
}

/// A JSON-RPC message; `params` is left out when null (`shutdown`, `exit`).
fn message(id: Option<u64>, method: &str, params: Value) -> Value {
    let mut msg = json!({ "jsonrpc": "2.0", "method": method });
    if let Some(id) = id {
        msg["id"] = json!(id);
    }
    if !params.is_null() {
        msg["params"] = params;
    }
    msg
}

impl LspClient {
    async fn connect(command: &str, args: &[String], cwd: &Path, launch: &Launch) -> Result<Self> {
        use tokio::process::Command;

        let mut cmd = match launch {
            Launch::Plain => {
                // npm's typescript-language-server and pyright-langserver (and
                // the gem/jdtls launchers) are `.cmd`/`.bat` shims on Windows,
                // which spawning the bare name never finds.
                #[cfg(windows)]
                let program = crate::mcp::client::resolve_on_path(
                    command,
                    std::env::var_os("PATH").as_deref(),
                    std::env::var_os("PATHEXT").as_deref(),
                )
                .unwrap_or_else(|| command.into());
                #[cfg(not(windows))]
                let program = command;
                let mut c = Command::new(program);
                c.args(args);
                c
            }
            Launch::Program(program) => {
                let mut c = Command::new(program);
                c.args(args);
                c
            }
            // `exec`, so killing the child kills the sandbox wrapper itself
            // (bwrap's --die-with-parent then takes the server down).
            Launch::Shell(line) => {
                let mut c = Command::new("sh");
                c.arg("-c").arg(format!("exec {line}"));
                // A sandboxed server runs project code (build scripts,
                // proc macros) like the Bash tool's commands.
                crate::sandbox::scrub_credentials(c.as_std_mut());
                c
            }
        };
        let mut child = cmd
            .current_dir(cwd)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| anyhow!("Failed to spawn '{}': {}", command, e))?;

        let stdin = child.stdin.take().ok_or_else(|| anyhow!("no stdin"))?;
        let stdout = child.stdout.take().ok_or_else(|| anyhow!("no stdout"))?;
        let stdin = Arc::new(Mutex::new(stdin));

        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value>>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let pending_clone = pending.clone();
        let dead = Arc::new(AtomicBool::new(false));
        let dead_clone = dead.clone();
        let published: Arc<std::sync::Mutex<HashMap<PathBuf, Published>>> = Arc::default();
        let published_clone = published.clone();
        let (seq_tx, publish_seq) = watch::channel(0u64);
        let reply_to = stdin.clone();

        // Spawn reader task
        tokio::spawn(async move {
            let mut reader = BufReader::new(stdout);
            loop {
                // Read Content-Length header
                let mut header = String::new();
                if reader.read_line(&mut header).await.unwrap_or(0) == 0 {
                    // Server gone: fail every in-flight request now rather
                    // than letting each sit out the full request timeout.
                    dead_clone.store(true, Ordering::SeqCst);
                    pending_clone.lock().await.clear();
                    break;
                }
                let header = header.trim().to_string();

                if !header.starts_with("Content-Length:") {
                    continue;
                }

                let content_length: usize = header
                    .trim_start_matches("Content-Length:")
                    .trim()
                    .parse()
                    .unwrap_or(0);

                // Read the blank line
                let mut blank = String::new();
                let _ = reader.read_line(&mut blank).await;

                if content_length == 0 {
                    continue;
                }

                // Read the body
                let mut body = vec![0u8; content_length];
                if reader.read_exact(&mut body).await.is_err() {
                    dead_clone.store(true, Ordering::SeqCst);
                    pending_clone.lock().await.clear();
                    break;
                }

                let text = match String::from_utf8(body) {
                    Ok(t) => t,
                    Err(_) => continue,
                };

                let msg: Value = match serde_json::from_str(&text) {
                    Ok(v) => v,
                    Err(_) => continue,
                };

                match (msg.get("method").and_then(Value::as_str), msg.get("id")) {
                    // A request from the server (workDoneProgress/create,
                    // workspace/configuration, ...): servers wait for the
                    // answer, and its id must not resolve one of ours.
                    (Some(method), Some(id)) => {
                        let result = match method {
                            "workspace/configuration" => {
                                let n = msg["params"]["items"].as_array().map_or(0, Vec::len);
                                Value::Array(vec![Value::Null; n])
                            }
                            _ => Value::Null,
                        };
                        let reply = json!({ "jsonrpc": "2.0", "id": id, "result": result });
                        // Written off the reader: `send_raw` holds stdin for a
                        // whole (possibly multi-MB) write, and a server that
                        // stops reading until this answer arrives would fill
                        // its stdout while the reader waited for the lock.
                        if let Ok(f) = frame(&reply) {
                            let w = reply_to.clone();
                            tokio::spawn(async move {
                                let mut w = w.lock().await;
                                let _ = w.write_all(f.as_bytes()).await;
                                let _ = w.flush().await;
                            });
                        }
                    }
                    (Some("textDocument/publishDiagnostics"), None) => {
                        let params = &msg["params"];
                        let Some(path) = params["uri"]
                            .as_str()
                            .and_then(|u| url::Url::parse(u).ok())
                            .and_then(|u| u.to_file_path().ok())
                            .map(|p| doc_key(&p))
                        else {
                            continue;
                        };
                        let seq = *seq_tx.borrow() + 1;
                        if let Ok(mut p) = published_clone.lock() {
                            p.insert(
                                path,
                                Published {
                                    seq,
                                    version: params["version"].as_i64(),
                                    diagnostics: params["diagnostics"]
                                        .as_array()
                                        .cloned()
                                        .unwrap_or_default(),
                                },
                            );
                        }
                        seq_tx.send_replace(seq);
                    }
                    (Some(_), None) => {}
                    // Match to pending request
                    (None, id) => {
                        let Some(id) = id.and_then(Value::as_u64) else {
                            continue;
                        };
                        let mut p = pending_clone.lock().await;
                        if let Some(tx) = p.remove(&id) {
                            let result = if let Some(error) = msg.get("error") {
                                Err(anyhow!("LSP error: {}", error))
                            } else {
                                Ok(msg.get("result").cloned().unwrap_or(Value::Null))
                            };
                            let _ = tx.send(result);
                        }
                    }
                }
            }
        });

        Ok(Self {
            stdin,
            pending,
            id_counter: Arc::new(AtomicU64::new(1)),
            dead,
            published,
            publish_seq,
            documents: Mutex::default(),
            sandbox_line: match launch {
                Launch::Shell(line) => Some(line.clone()),
                Launch::Plain | Launch::Program(_) => None,
            },
            child: std::sync::Mutex::new(Some(child)),
        })
    }

    async fn send_raw(&self, msg: Value) -> Result<()> {
        let frame = frame(&msg)?;
        if self.dead.load(Ordering::SeqCst) {
            return Err(anyhow!("language server exited"));
        }
        let mut stdin = self.stdin.lock().await;
        // A write dropped part-way (a caller's timeout on a server that has
        // stopped reading) leaves half a frame on the pipe: nothing sent
        // after it would parse, so the client is dead from then on.
        struct DeadUnlessDone<'a>(&'a AtomicBool, bool);
        impl Drop for DeadUnlessDone<'_> {
            fn drop(&mut self) {
                if !self.1 {
                    self.0.store(true, Ordering::SeqCst);
                }
            }
        }
        let mut guard = DeadUnlessDone(&self.dead, false);
        let written = async {
            stdin.write_all(frame.as_bytes()).await?;
            stdin.flush().await
        }
        .await;
        guard.1 = written.is_ok();
        Ok(written?)
    }

    /// Stop using this client: the pool starts a new server next time.
    pub(crate) fn mark_dead(&self) {
        self.dead.store(true, Ordering::SeqCst);
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.id_counter.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();

        {
            let mut pending = self.pending.lock().await;
            pending.insert(id, tx);
        }

        if let Err(e) = self.send_raw(message(Some(id), method, params)).await {
            self.pending.lock().await.remove(&id);
            return Err(e);
        }

        // Wait up to 15 seconds
        tokio::time::timeout(tokio::time::Duration::from_secs(15), rx)
            .await
            .map_err(|_| anyhow!("LSP request '{}' timed out", method))?
            .map_err(|_| anyhow!("LSP request '{}' cancelled", method))?
    }

    async fn notify(&self, method: &str, params: Value) -> Result<()> {
        self.send_raw(message(None, method, params)).await
    }

    async fn initialize(&self, command: &str, root: &Path) -> Result<()> {
        let root_uri = path_to_uri(root);
        self.request(
            "initialize",
            json!({
                "processId": std::process::id(),
                "rootUri": root_uri,
                "rootPath": root.to_string_lossy(),
                "capabilities": {
                    "textDocument": {
                        "synchronization": { "dynamicRegistration": false },
                        "publishDiagnostics": { "versionSupport": true },
                        "definition": { "dynamicRegistration": false },
                        "references": { "dynamicRegistration": false },
                        "hover": { "dynamicRegistration": false, "contentFormat": ["plaintext"] },
                        "documentSymbol": { "dynamicRegistration": false },
                        "implementation": { "dynamicRegistration": false },
                        "callHierarchy": { "dynamicRegistration": false }
                    },
                    "workspace": {
                        "symbol": { "dynamicRegistration": false }
                    }
                },
                "initializationOptions": initialization_options(command)
            }),
        )
        .await?;

        self.notify("initialized", json!({})).await?;
        Ok(())
    }

    /// Send the file's current text: `didOpen` the first time, a full-text
    /// `didChange` after. A second `didOpen` of an open document is a
    /// protocol error, and without the change the server keeps answering
    /// about the text it was first given.
    pub(crate) async fn sync_document(&self, path: &Path) -> Result<Synced> {
        let text = tokio::fs::read_to_string(path).await?;
        let uri = path_to_uri(path);
        let mut docs = self.documents.lock().await;
        let seq = *self.publish_seq.borrow();
        let previous = docs.remove(&uri).map(|d| (d.version, d.text));
        let version = match &previous {
            Some((v, _)) => {
                let version = v + 1;
                self.notify(
                    "textDocument/didChange",
                    json!({
                        "textDocument": { "uri": uri, "version": version },
                        "contentChanges": [{ "text": text }]
                    }),
                )
                .await?;
                version
            }
            None => {
                let lang_id = lang_id_for_ext(path.extension().and_then(|e| e.to_str()));
                self.notify(
                    "textDocument/didOpen",
                    json!({
                        "textDocument": {
                            "uri": uri,
                            "languageId": lang_id,
                            "version": 1,
                            "text": text
                        }
                    }),
                )
                .await?;
                1
            }
        };
        docs.insert(
            uri,
            DocState {
                version,
                text,
                previous,
            },
        );
        Ok(Synced {
            path: path.to_path_buf(),
            version,
            seq,
        })
    }

    pub(crate) fn is_dead(&self) -> bool {
        self.dead.load(Ordering::SeqCst)
    }

    /// The diagnostics last published for `path`, if any.
    pub(crate) fn diagnostics(&self, path: &Path) -> Option<Vec<Value>> {
        self.published_for(path).map(|p| p.diagnostics)
    }

    fn published_for(&self, path: &Path) -> Option<Published> {
        let published = self.published.lock().ok()?;
        // Servers may answer with the resolved path (`/tmp` → `/private/tmp`).
        published
            .get(&doc_key(path))
            .or_else(|| published.get(&doc_key(&std::fs::canonicalize(path).ok()?)))
            .cloned()
    }

    /// Diagnostics published for this text of the document since it was
    /// synced, if any yet.
    fn fresh(&self, synced: &Synced) -> Option<Vec<Value>> {
        self.published_for(&synced.path)
            .filter(|p| p.seq > synced.seq && p.version.is_none_or(|v| v >= synced.version))
            .map(|p| p.diagnostics)
    }

    /// Wait until every synced document has diagnostics for its new text,
    /// then `settle` longer for the follow-ups servers send (a fast syntax
    /// pass, then a semantic one); never past `deadline`, and not after
    /// `cancel` is set. Servers such as rust-analyzer publish only when a
    /// document's diagnostics change, so a document the server has reported
    /// on before counts as unchanged once the server has published nothing
    /// for `settle`. Returns each document's diagnostics by then: its last
    /// published set when nothing new came, marked with the older text it
    /// was published for; `None` for one the server has never reported on,
    /// or whose last set is for a text no longer kept.
    pub(crate) async fn wait_for_diagnostics(
        &self,
        synced: &[Synced],
        settle: Duration,
        deadline: Instant,
        cancel: &AtomicBool,
    ) -> Vec<Option<Reported>> {
        let mut seq = self.publish_seq.clone();
        seq.borrow_and_update();
        let mut settled_at: Option<Instant> = None;
        let mut last_publish = Instant::now();
        loop {
            let now = Instant::now();
            let fresh: Vec<bool> = synced.iter().map(|s| self.fresh(s).is_some()).collect();
            if settled_at.is_none() && fresh.iter().all(|f| *f) {
                settled_at = Some(now + settle);
            }
            let reported = synced
                .iter()
                .zip(&fresh)
                .all(|(s, f)| *f || self.published_for(&s.path).is_some());
            let quiet_at = reported.then_some(last_publish + settle);
            if now >= deadline
                || settled_at.is_some_and(|t| now >= t)
                || quiet_at.is_some_and(|t| now >= t)
                || cancel.load(Ordering::SeqCst)
                || self.dead.load(Ordering::SeqCst)
            {
                break;
            }
            // Short slices, so Esc is noticed.
            let wake = [settled_at, quiet_at, Some(now + Duration::from_millis(100))]
                .into_iter()
                .flatten()
                .fold(deadline, Instant::min);
            match tokio::time::timeout_at(wake, seq.changed()).await {
                Ok(Ok(())) => last_publish = Instant::now(),
                // The reader is gone: the server exited.
                Ok(Err(_)) => break,
                Err(_) => {}
            }
        }
        let docs = self.documents.lock().await;
        synced
            .iter()
            .map(|s| {
                if let Some(diagnostics) = self.fresh(s) {
                    return Some(Reported {
                        diagnostics,
                        stale_text: None,
                    });
                }
                let p = self.published_for(&s.path)?;
                // A set without a version, published before this sync, is
                // about the text the server had: the one sent before.
                let for_version = p.version.unwrap_or(s.version - 1);
                let doc = docs.get(&path_to_uri(&s.path))?;
                let stale_text = if for_version == doc.version {
                    None
                } else {
                    match &doc.previous {
                        Some((v, text)) if *v == for_version => Some(text.clone()),
                        // Its text is gone: say nothing rather than read
                        // its line numbers against the wrong text.
                        _ => return None,
                    }
                };
                Some(Reported {
                    diagnostics: p.diagnostics,
                    stale_text,
                })
            })
            .collect()
    }

    /// `shutdown`, then `exit`; the process is killed if it has not exited
    /// within a second of that.
    async fn shutdown(&self) {
        if !self.dead.load(Ordering::SeqCst) {
            let second = Duration::from_secs(1);
            let _ = tokio::time::timeout(second, self.request("shutdown", Value::Null)).await;
            // A server that stopped reading its input would hold this write
            // (and quitting) forever.
            let _ = tokio::time::timeout(second, self.notify("exit", Value::Null)).await;
        }
        let child = self.child.lock().ok().and_then(|mut c| c.take());
        if let Some(mut child) = child
            && tokio::time::timeout(Duration::from_secs(1), child.wait())
                .await
                .is_err()
        {
            let _ = child.kill().await;
        }
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn path_to_uri(path: &Path) -> String {
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    };
    // `Url::from_file_path` percent-encodes reserved characters (spaces,
    // `#`, `?`) that a raw `format!` left in the URI.
    url::Url::from_file_path(&abs)
        .map(|u| u.to_string())
        .unwrap_or_else(|_| format!("file://{}", abs.display()))
}

/// The source extension a project directory's build files point to, for
/// choosing a server when there is no file to go by.
fn project_ext(dir: &Path) -> Option<&'static str> {
    const MARKERS: &[(&str, &str)] = &[
        ("Cargo.toml", "rs"),
        ("go.mod", "go"),
        ("tsconfig.json", "ts"),
        ("package.json", "ts"),
        ("pyproject.toml", "py"),
        ("setup.py", "py"),
        ("requirements.txt", "py"),
        ("pom.xml", "java"),
        ("build.gradle", "java"),
        ("build.gradle.kts", "java"),
        ("Gemfile", "rb"),
        ("compile_commands.json", "cpp"),
        ("CMakeLists.txt", "cpp"),
    ];
    MARKERS
        .iter()
        .find(|(marker, _)| dir.join(marker).is_file())
        .map(|(_, ext)| *ext)
}

fn lang_id_for_ext(ext: Option<&str>) -> &'static str {
    match ext {
        Some("rs") => "rust",
        Some("py") | Some("pyi") => "python",
        Some("ts") => "typescript",
        Some("tsx") => "typescriptreact",
        Some("js") | Some("mjs") | Some("cjs") => "javascript",
        Some("jsx") => "javascriptreact",
        Some("c") => "c",
        Some("cpp") | Some("cc") => "cpp",
        Some("h") | Some("hpp") => "cpp",
        Some("go") => "go",
        Some("java") => "java",
        Some("rb") => "ruby",
        Some("lua") => "lua",
        _ => "plaintext",
    }
}

fn format_lsp_result(operation: &str, result: &Value) -> String {
    if result.is_null() {
        return format!("{operation}: no results");
    }

    match operation {
        "hover" => {
            // { contents: { kind, value } | string | [strings] }

            result
                .get("contents")
                .and_then(|c| {
                    if let Some(s) = c.as_str() {
                        return Some(s.to_string());
                    }
                    if let Some(obj) = c.as_object() {
                        return obj
                            .get("value")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string());
                    }
                    None
                })
                .unwrap_or_else(|| result.to_string())
        }
        "documentSymbol" | "workspaceSymbol" => {
            if let Some(arr) = result.as_array() {
                let lines: Vec<String> = arr
                    .iter()
                    .map(|sym| {
                        let name = sym.get("name").and_then(|v| v.as_str()).unwrap_or("?");
                        let kind = sym.get("kind").and_then(|v| v.as_u64()).unwrap_or(0);
                        let kind_str = symbol_kind(kind);
                        // SymbolInformation carries a location; DocumentSymbol
                        // only a range in the file that was asked about.
                        if sym.get("location").is_some() {
                            return format!("{kind_str} {name} — {}", format_location(sym));
                        }
                        let start = sym
                            .get("selectionRange")
                            .or_else(|| sym.get("range"))
                            .and_then(|r| r.get("start"));
                        match start {
                            Some(start) => format!(
                                "{kind_str} {name} — {}:{}",
                                start.get("line").and_then(|v| v.as_u64()).unwrap_or(0) + 1,
                                start.get("character").and_then(|v| v.as_u64()).unwrap_or(0) + 1
                            ),
                            None => format!("{kind_str} {name}"),
                        }
                    })
                    .collect();
                lines.join("\n")
            } else {
                result.to_string()
            }
        }
        _ => {
            // Locations array
            if let Some(arr) = result.as_array() {
                let lines: Vec<String> = arr.iter().map(format_location).collect();
                if lines.is_empty() {
                    format!("{operation}: no results")
                } else {
                    lines.join("\n")
                }
            } else {
                // Single location
                format_location(result)
            }
        }
    }
}

fn format_location(loc: &Value) -> String {
    let uri = loc
        .get("uri")
        .or_else(|| loc.get("location").and_then(|l| l.get("uri")))
        .and_then(|v| v.as_str())
        .unwrap_or("?");

    // Servers return percent-encoded URIs (`my%20proj`, `/C:/...`), which
    // Read and Edit cannot open; non-file schemes (`jdt://`) stay as given.
    let path = url::Url::parse(uri)
        .ok()
        .filter(|u| u.scheme() == "file")
        .and_then(|u| u.to_file_path().ok())
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| uri.to_string());

    let range = loc
        .get("range")
        .or_else(|| loc.get("location").and_then(|l| l.get("range")));

    if let Some(range) = range {
        let line = range
            .get("start")
            .and_then(|s| s.get("line"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let character = range
            .get("start")
            .and_then(|s| s.get("character"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        format!("{path}:{}:{}", line + 1, character + 1)
    } else {
        path
    }
}

fn symbol_kind(kind: u64) -> &'static str {
    match kind {
        1 => "File",
        2 => "Module",
        3 => "Namespace",
        4 => "Package",
        5 => "Class",
        6 => "Method",
        7 => "Property",
        8 => "Field",
        9 => "Constructor",
        10 => "Enum",
        11 => "Interface",
        12 => "Function",
        13 => "Variable",
        14 => "Constant",
        15 => "String",
        16 => "Number",
        17 => "Boolean",
        18 => "Array",
        19 => "Object",
        20 => "Key",
        21 => "Null",
        22 => "EnumMember",
        23 => "Struct",
        24 => "Event",
        25 => "Operator",
        26 => "TypeParameter",
        _ => "Symbol",
    }
}

#[cfg(test)]
mod path_tests {
    use super::*;

    /// `"~"` alone used to slice `p[2..]` on a 1-byte string and panic.
    #[tokio::test]
    async fn a_bare_tilde_path_does_not_panic() {
        let dir = tempfile::tempdir().unwrap();
        let out = LSPTool::default()
            .execute(
                json!({"operation": "hover", "file_path": "~"}),
                &ToolContext::new(dir.path().to_path_buf()),
            )
            .await;
        assert!(out.is_ok() || out.is_err()); // reaching here is the assertion
    }
}

#[cfg(all(test, unix))]
mod lifecycle_tests {
    use super::*;

    /// A stand-in language server: waits for the first request line, then
    /// answers request id 1 (the client's `initialize`) and stays alive.
    fn fake_server() -> (String, Vec<String>) {
        let body = r#"{"jsonrpc":"2.0","id":1,"result":{"capabilities":{}}}"#;
        let script = format!(
            "read -r _line; printf 'Content-Length: {}\\r\\n\\r\\n%s' '{}'; sleep 5",
            body.len(),
            body
        );
        ("sh".to_string(), vec!["-c".to_string(), script])
    }

    /// The server must still be alive when `initialize` is sent. Before the
    /// fix the `Child` was dropped at the end of `connect` with
    /// `kill_on_drop`, so every LSP call killed its own server and timed out.
    #[tokio::test]
    async fn server_survives_connect_and_answers_initialize() {
        let (cmd, args) = fake_server();
        let dir = tempfile::tempdir().unwrap();
        let client = LspClient::connect(&cmd, &args, dir.path(), &Launch::Plain)
            .await
            .unwrap();
        let init = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            client.initialize(&cmd, dir.path()),
        )
        .await;
        assert!(
            matches!(init, Ok(Ok(()))),
            "initialize must succeed against a live server: {init:?}"
        );
    }

    /// When the server dies, in-flight requests must fail promptly instead of
    /// sitting out the 15 s request timeout.
    #[tokio::test]
    async fn a_dead_server_fails_requests_promptly() {
        let dir = tempfile::tempdir().unwrap();
        let script = "read -r _line; exit 0".to_string();
        let client = LspClient::connect(
            "sh",
            &["-c".to_string(), script],
            dir.path(),
            &Launch::Plain,
        )
        .await
        .unwrap();
        let started = std::time::Instant::now();
        let r = client.request("initialize", json!({})).await;
        assert!(r.is_err());
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "took {:?}; the pending request was left waiting for the full timeout",
            started.elapsed()
        );
    }
}

#[cfg(test)]
mod symbol_tests {
    use super::*;

    /// A bare `{operation: workspaceSymbol, query}` used to resolve the cwd's
    /// (missing) extension and always fail with "No language server
    /// configured for extension: None".
    #[test]
    fn workspace_symbol_picks_the_server_from_project_files() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(project_ext(dir.path()), None);
        std::fs::write(dir.path().join("Cargo.toml"), "[package]\n").unwrap();
        assert_eq!(project_ext(dir.path()), Some("rs"));
        let py = tempfile::tempdir().unwrap();
        std::fs::write(py.path().join("pyproject.toml"), "").unwrap();
        assert_eq!(project_ext(py.path()), Some("py"));
    }

    #[tokio::test]
    async fn workspace_symbol_without_a_project_explains_what_to_pass() {
        let dir = tempfile::tempdir().unwrap();
        let out = LSPTool::default()
            .execute(
                json!({"operation": "workspaceSymbol", "query": "main"}),
                &ToolContext::new(dir.path().to_path_buf()),
            )
            .await
            .unwrap();
        assert!(out.is_error);
        let text: String = out
            .content
            .iter()
            .map(|c| match c {
                crate::api::types::ToolResultContent::Text { text } => text.as_str(),
            })
            .collect();
        assert!(text.contains("Pass file_path"), "{text}");
        assert!(!text.contains("extension: None"), "{text}");
    }

    #[test]
    fn symbol_results_say_where_each_symbol_is() {
        // A real absolute path, so the URI round-trips on Windows too.
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("lib.rs");
        let workspace = json!([{
            "name": "parse",
            "kind": 12,
            "location": {
                "uri": url::Url::from_file_path(&file).unwrap().to_string(),
                "range": {"start": {"line": 41, "character": 7}, "end": {"line": 41, "character": 12}}
            }
        }]);
        assert_eq!(
            format_lsp_result("workspaceSymbol", &workspace),
            format!("Function parse — {}:42:8", file.display())
        );
        let document = json!([{
            "name": "Config",
            "kind": 23,
            "range": {"start": {"line": 3, "character": 0}, "end": {"line": 9, "character": 1}},
            "selectionRange": {"start": {"line": 4, "character": 11}, "end": {"line": 4, "character": 17}}
        }]);
        assert_eq!(
            format_lsp_result("documentSymbol", &document),
            "Struct Config — 5:12"
        );
    }

    /// Locations came back as `/p/my%20proj/lib.rs:41:7`: an encoded path
    /// Read cannot open and a 0-based line that reads as one line too early.
    #[test]
    fn locations_print_decoded_paths_and_one_based_positions() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("my proj #1").join("lib.rs");
        let uri = url::Url::from_file_path(&file).unwrap().to_string();
        assert!(uri.contains("my%20proj%20%231"), "{uri}");
        let defs = json!([{
            "uri": uri,
            "range": {"start": {"line": 41, "character": 7}, "end": {"line": 41, "character": 12}}
        }]);
        assert_eq!(
            format_lsp_result("goToDefinition", &defs),
            format!("{}:42:8", file.display())
        );
        let jdt = json!({"uri": "jdt://contents/rt.jar/String.class"});
        assert_eq!(
            format_lsp_result("goToDefinition", &jdt),
            "jdt://contents/rt.jar/String.class"
        );
    }
}

#[cfg(test)]
mod uri_tests {
    use super::path_to_uri;

    /// Spaces and other reserved characters must be percent-encoded, or the
    /// server cannot resolve the document.
    #[test]
    fn paths_are_percent_encoded_file_uris() {
        let uri = path_to_uri(std::path::Path::new("/tmp/my project/a b.rs"));
        // Windows resolves `/tmp` under a drive letter; the encoding is the point.
        assert!(uri.starts_with("file:///"), "{uri}");
        assert!(uri.ends_with("/tmp/my%20project/a%20b.rs"), "{uri}");
        assert!(path_to_uri(std::path::Path::new("/plain/x.rs")).starts_with("file:///"));
    }
}

#[cfg(all(test, unix))]
mod cache_tests {
    use super::*;

    /// A server that exits must be replaced on the next call, not handed out
    /// from the cache to fail with EPIPE for the rest of the session.
    #[tokio::test]
    async fn a_dead_cached_server_is_respawned() {
        let dir = tempfile::tempdir().unwrap();
        let counter = dir.path().join("starts");
        let body = r#"{"jsonrpc":"2.0","id":1,"result":{"capabilities":{}}}"#;
        // Answers initialize, then exits shortly after (a crash mid-session).
        let script = format!(
            "echo x >> '{}'; read -r _l; printf 'Content-Length: {}\\r\\n\\r\\n%s' '{}'; sleep 0.5",
            counter.display(),
            body.len(),
            body
        );
        let tool = LSPTool::default();
        let args = vec!["-c".to_string(), script];
        let a = tool
            .pool
            .client_for("sh", &args, dir.path(), &Launch::Plain)
            .await
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !a.dead.load(Ordering::SeqCst) {
            assert!(std::time::Instant::now() < deadline, "exit never noticed");
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(a.notify("initialized", json!({})).await.is_err());
        assert!(a.pending.lock().await.is_empty());
        let b = tool
            .pool
            .client_for("sh", &args, dir.path(), &Launch::Plain)
            .await
            .unwrap();
        assert!(
            !Arc::ptr_eq(&a, &b),
            "the dead server came back from the cache"
        );
        let starts = std::fs::read_to_string(&counter).unwrap().lines().count();
        assert_eq!(starts, 2, "server spawned {starts} times");
    }

    /// A server started unsandboxed (before `/sandbox enable`) must not be
    /// handed to a caller that asks for a sandboxed one: it is replaced.
    /// A sandboxed server serves unsandboxed callers too.
    #[tokio::test]
    async fn a_sandboxed_launch_replaces_an_unsandboxed_server() {
        let dir = tempfile::tempdir().unwrap();
        let counter = dir.path().join("starts");
        let body = r#"{"jsonrpc":"2.0","id":1,"result":{"capabilities":{}}}"#;
        let script = format!(
            "echo x >> '{}'; read -r _l; printf 'Content-Length: {}\\r\\n\\r\\n%s' '{}'; sleep 5",
            counter.display(),
            body.len(),
            body
        );
        let tool = LSPTool::default();
        let args = vec!["-c".to_string(), script.clone()];
        let plain = tool
            .pool
            .client_for("sh", &args, dir.path(), &Launch::Plain)
            .await
            .unwrap();
        let line = format!("sh -c {}", crate::sandbox::shell_quote(&script));
        let shell = Launch::Shell(line.clone());
        let boxed = tool
            .pool
            .client_for("sh", &args, dir.path(), &shell)
            .await
            .unwrap();
        assert!(
            !Arc::ptr_eq(&plain, &boxed),
            "the unsandboxed server was reused"
        );
        assert!(plain.is_dead(), "the unsandboxed server is retired");
        assert_eq!(boxed.sandbox_line.as_deref(), Some(line.as_str()));
        let again = tool
            .pool
            .client_for("sh", &args, dir.path(), &shell)
            .await
            .unwrap();
        assert!(Arc::ptr_eq(&boxed, &again));
        let other = Launch::Shell(format!("{line} "));
        let rewrapped = tool
            .pool
            .client_for("sh", &args, dir.path(), &other)
            .await
            .unwrap();
        assert!(!Arc::ptr_eq(&boxed, &rewrapped), "a different sandbox line");
        let unboxed = tool
            .pool
            .client_for("sh", &args, dir.path(), &Launch::Plain)
            .await
            .unwrap();
        assert!(Arc::ptr_eq(&rewrapped, &unboxed));
        let starts = std::fs::read_to_string(&counter).unwrap().lines().count();
        assert_eq!(starts, 3, "server spawned {starts} times");
    }

    /// Two queries against the same root must reuse one server process.
    #[tokio::test]
    async fn the_same_root_reuses_one_server() {
        let dir = tempfile::tempdir().unwrap();
        let counter = dir.path().join("starts");
        let body = r#"{"jsonrpc":"2.0","id":1,"result":{"capabilities":{}}}"#;
        let script = format!(
            "echo x >> '{}'; read -r _l; printf 'Content-Length: {}\\r\\n\\r\\n%s' '{}'; sleep 5",
            counter.display(),
            body.len(),
            body
        );
        let tool = LSPTool::default();
        let args = vec!["-c".to_string(), script];
        let a = tool
            .pool
            .client_for("sh", &args, dir.path(), &Launch::Plain)
            .await
            .unwrap();
        let b = tool
            .pool
            .client_for("sh", &args, dir.path(), &Launch::Plain)
            .await
            .unwrap();
        assert!(Arc::ptr_eq(&a, &b), "second call must hit the cache");
        let starts = std::fs::read_to_string(&counter).unwrap().lines().count();
        assert_eq!(starts, 1, "server spawned {starts} times");
    }
}

#[cfg(test)]
mod init_tests {
    use super::*;

    /// rust-analyzer's own builds stay out of the `target/` dir auto-fix's
    /// cargo commands lock.
    #[test]
    fn rust_analyzer_gets_its_own_target_dir() {
        assert_eq!(
            initialization_options("rust-analyzer"),
            json!({ "cargo": { "targetDir": true } })
        );
        assert_eq!(initialization_options("pyright-langserver"), json!({}));
    }
}

#[cfg(all(test, unix))]
mod sync_tests {
    use super::*;

    fn trusted_ctx(dir: &Path) -> ToolContext {
        let mut ctx = ToolContext::new(dir.to_path_buf());
        ctx.project_trusted = true;
        ctx
    }

    /// A file that is not UTF-8 is still queried, unsynced, as before.
    #[tokio::test]
    async fn a_file_that_is_not_utf8_is_still_queried() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let server = dir.path().join("server");
        // Answers every request with a null result.
        std::fs::write(
            &server,
            r#"#!/usr/bin/env python3
import json, sys
while True:
    n = None
    while True:
        line = sys.stdin.buffer.readline()
        if not line:
            sys.exit(0)
        line = line.strip()
        if not line:
            break
        k, v = line.split(b":", 1)
        if k.strip().lower() == b"content-length":
            n = int(v)
    m = json.loads(sys.stdin.buffer.read(n))
    if "id" in m and "method" in m:
        b = json.dumps({"jsonrpc": "2.0", "id": m["id"], "result": None}).encode()
        sys.stdout.buffer.write(b"Content-Length: %d\r\n\r\n" % len(b) + b)
        sys.stdout.buffer.flush()
"#,
        )
        .unwrap();
        std::fs::set_permissions(&server, std::fs::Permissions::from_mode(0o755)).unwrap();
        let file = dir.path().join("legacy.lua");
        std::fs::write(&file, b"-- caf\xe9\nlocal x = 1\n").unwrap();

        let tool = LSPTool::default();
        // The tool's server for .lua, already running (from the cache).
        tool.pool
            .client_for(
                "lua-language-server",
                &[],
                dir.path(),
                &Launch::Program(server),
            )
            .await
            .unwrap();
        let out = tool
            .execute(
                json!({"operation": "hover", "file_path": "legacy.lua", "line": 1, "character": 6}),
                &trusted_ctx(dir.path()),
            )
            .await
            .expect("a Latin-1 file made the query fail");
        assert!(!out.is_error);
    }
}

/// The LSP tool against a stand-in server (python3) that logs what it reads
/// and answers every request with a null result.
#[cfg(all(test, unix))]
mod server_tests {
    use super::*;

    /// `mode`: `ok`, or `deaf` (stops reading its input after `initialized`).
    /// Logs `initialize processId=<id>`, and `<method> <uri>` for the rest.
    fn fake_server(dir: &Path, mode: &str) -> (PathBuf, PathBuf) {
        use std::os::unix::fs::PermissionsExt;
        let log = dir.join("server.log");
        let server = dir.join("server");
        std::fs::write(
            &server,
            format!(
                r#"#!/usr/bin/env python3
import json, sys, time
LOG, MODE = {log:?}, {mode:?}
def log(s):
    with open(LOG, "a") as f:
        f.write(s + "\n")
while True:
    n = None
    while True:
        line = sys.stdin.buffer.readline()
        if not line:
            sys.exit(0)
        line = line.strip()
        if not line:
            break
        k, v = line.split(b":", 1)
        if k.strip().lower() == b"content-length":
            n = int(v)
    m = json.loads(sys.stdin.buffer.read(n))
    method, params = m.get("method"), m.get("params") or {{}}
    if method == "initialize":
        log("initialize processId=" + json.dumps(params.get("processId")))
    else:
        log("%s %s" % (method, params.get("textDocument", {{}}).get("uri", "")))
    if method == "initialized" and MODE == "deaf":
        time.sleep(60)
    if "id" in m and method:
        b = json.dumps({{"jsonrpc": "2.0", "id": m["id"], "result": None}}).encode()
        sys.stdout.buffer.write(b"Content-Length: %d\r\n\r\n" % len(b) + b)
        sys.stdout.buffer.flush()
"#,
                log = log.display().to_string(),
            ),
        )
        .unwrap();
        std::fs::set_permissions(&server, std::fs::Permissions::from_mode(0o755)).unwrap();
        (server, log)
    }

    fn text(out: &ToolOutput) -> String {
        out.content
            .iter()
            .map(|c| match c {
                crate::api::types::ToolResultContent::Text { text } => text.as_str(),
            })
            .collect()
    }

    /// The tool's server for `.lua`, started from `server` and cached, as if
    /// lua-language-server were on PATH.
    async fn lua_server(tool: &LSPTool, dir: &Path, server: &Path) -> Arc<LspClient> {
        tool.pool
            .client_for(
                "lua-language-server",
                &[],
                dir,
                &Launch::Program(server.to_path_buf()),
            )
            .await
            .unwrap()
    }

    /// A language server runs project code: the tool used to start one (and
    /// query it) in any folder, without a prompt, ignoring `/trust`.
    #[tokio::test]
    async fn the_lsp_tool_needs_a_trusted_project() {
        let dir = tempfile::tempdir().unwrap();
        let (server, log) = fake_server(dir.path(), "ok");
        std::fs::write(dir.path().join("a.lua"), "local x = 1\n").unwrap();
        let tool = LSPTool::default();
        lua_server(&tool, dir.path(), &server).await;
        let hover = json!({"operation": "hover", "file_path": "a.lua"});

        let mut ctx = ToolContext::new(dir.path().to_path_buf());
        let out = tool.execute(hover.clone(), &ctx).await.unwrap();
        assert!(out.is_error);
        assert!(text(&out).contains("/trust"), "{}", text(&out));
        let seen = std::fs::read_to_string(&log).unwrap();
        assert!(!seen.contains("a.lua"), "{seen}");

        ctx.project_trusted = true;
        let out = tool.execute(hover, &ctx).await.unwrap();
        assert!(!out.is_error, "{}", text(&out));
        let seen = std::fs::read_to_string(&log).unwrap();
        assert!(seen.contains("textDocument/hover"), "{seen}");
    }

    /// The tool starts servers the way auto-fix does: the executable itself
    /// without a sandbox, never bare when the sandbox refuses it.
    #[test]
    fn servers_start_under_the_sandbox() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("rust-analyzer");
        let mut containment = crate::autofix::Containment {
            trusted: true,
            ..Default::default()
        };
        assert!(matches!(
            Launch::contained(&exe, &[], &containment, dir.path()),
            Ok(Launch::Program(p)) if p == exe
        ));
        containment.sandbox_mode = Some("no-such-sandbox".into());
        assert!(Launch::contained(&exe, &[], &containment, dir.path()).is_err());
    }
}
