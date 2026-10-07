/// LSPTool — port of lsp.ts
/// Communicates with language servers via JSON-RPC 2.0 over stdio using LSP protocol.
/// Supports: goToDefinition, findReferences, hover, documentSymbol, workspaceSymbol,
///           goToImplementation, prepareCallHierarchy, incomingCalls, outgoingCalls
use super::{Tool, ToolContext, ToolOutput, async_trait};
use anyhow::{Result, anyhow};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::ChildStdin;
use tokio::sync::{Mutex, oneshot};

/// One language server per (command, project root), kept for the life of
/// the tool set. A fresh server per call meant paying rust-analyzer's full
/// startup and indexing on every query.
type ClientCache = Arc<Mutex<HashMap<(String, PathBuf), Arc<LspClient>>>>;

#[derive(Default)]
pub struct LSPTool {
    cache: ClientCache,
}

impl LSPTool {
    /// The cached, initialised client for this server + root, spawning it
    /// on first use.
    async fn client_for(
        &self,
        command: &str,
        args: &[String],
        root: &Path,
    ) -> Result<Arc<LspClient>> {
        let key = (format!("{command} {}", args.join(" ")), root.to_path_buf());
        let mut cache = self.cache.lock().await;
        if let Some(c) = cache.get(&key) {
            // A server that crashed or was killed would otherwise fail every
            // later call (EPIPE) until OxideClaw restarts: start a new one.
            if !c.dead.load(Ordering::SeqCst) {
                return Ok(Arc::clone(c));
            }
            cache.remove(&key);
        }
        let client = LspClient::connect(command, args, root).await?;
        client.initialize(root).await?;
        let client = Arc::new(client);
        cache.insert(key, Arc::clone(&client));
        Ok(client)
    }
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
        let server_cmd = match ext {
            Some("rs") => vec!["rust-analyzer".to_string()],
            Some("py") | Some("pyi") => {
                vec!["pyright-langserver".to_string(), "--stdio".to_string()]
            }
            Some("ts") | Some("tsx") | Some("js") | Some("jsx") | Some("mjs") | Some("cjs") => {
                vec![
                    "typescript-language-server".to_string(),
                    "--stdio".to_string(),
                ]
            }
            Some("c") | Some("cpp") | Some("cc") | Some("h") | Some("hpp") => {
                vec!["clangd".to_string()]
            }
            Some("go") => vec!["gopls".to_string()],
            Some("java") => vec!["jdtls".to_string()],
            Some("rb") => vec!["solargraph".to_string(), "stdio".to_string()],
            Some("lua") => vec!["lua-language-server".to_string()],
            ext => {
                return Ok(ToolOutput::error(format!(
                    "No language server configured for extension: {:?}. \
                    Supported: .rs, .py, .ts/.js, .c/.cpp, .go, .java, .rb, .lua",
                    ext
                )));
            }
        };

        // One initialised server per (command, root), cached across calls.
        let client = match self
            .client_for(&server_cmd[0], &server_cmd[1..], &ctx.cwd)
            .await
        {
            Ok(c) => c,
            Err(e) => {
                return Ok(ToolOutput::error(format!(
                    "Could not start language server '{}': {e}\nMake sure it is installed.",
                    server_cmd[0]
                )));
            }
        };

        // Convert file path to URI
        let uri = path_to_uri(&file_path);

        // Open the document so the server can process it
        if file_path.exists()
            && let Ok(content) = tokio::fs::read_to_string(&file_path).await
        {
            let lang_id = lang_id_for_ext(file_path.extension().and_then(|e| e.to_str()));
            client
                .notify(
                    "textDocument/didOpen",
                    json!({
                        "textDocument": {
                            "uri": uri,
                            "languageId": lang_id,
                            "version": 1,
                            "text": content
                        }
                    }),
                )
                .await?;
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

struct LspClient {
    stdin: Arc<Mutex<ChildStdin>>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value>>>>>,
    id_counter: Arc<AtomicU64>,
    /// Set once the server's stdout closes or a write to it fails. Writing to
    /// a dead server's stdin raises SIGPIPE, which kills `-p` runs outright.
    dead: Arc<AtomicBool>,
    /// Owns the language server. `kill_on_drop` means the server lives
    /// exactly as long as this client — previously the handle was dropped at
    /// the end of `connect`, which killed the server before `initialize`.
    _child: tokio::process::Child,
}

impl LspClient {
    async fn connect(command: &str, args: &[String], cwd: &Path) -> Result<Self> {
        use tokio::process::Command;

        // npm's typescript-language-server and pyright-langserver (and the
        // gem/jdtls launchers) are `.cmd`/`.bat` shims on Windows, which
        // spawning the bare name never finds.
        #[cfg(windows)]
        let program = crate::mcp::client::resolve_on_path(
            command,
            std::env::var_os("PATH").as_deref(),
            std::env::var_os("PATHEXT").as_deref(),
        )
        .unwrap_or_else(|| command.into());
        #[cfg(not(windows))]
        let program = command;

        let mut child = Command::new(program)
            .args(args)
            .current_dir(cwd)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| anyhow!("Failed to spawn '{}': {}", command, e))?;

        let stdin = child.stdin.take().ok_or_else(|| anyhow!("no stdin"))?;
        let stdout = child.stdout.take().ok_or_else(|| anyhow!("no stdout"))?;

        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value>>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let pending_clone = pending.clone();
        let dead = Arc::new(AtomicBool::new(false));
        let dead_clone = dead.clone();

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

                // Match to pending request
                if let Some(id) = msg.get("id").and_then(|v| v.as_u64()) {
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
        });

        Ok(Self {
            stdin: Arc::new(Mutex::new(stdin)),
            pending,
            id_counter: Arc::new(AtomicU64::new(1)),
            dead,
            _child: child,
        })
    }

    async fn send_raw(&self, msg: Value) -> Result<()> {
        let body = serde_json::to_string(&msg)?;
        let frame = format!("Content-Length: {}\r\n\r\n{}", body.len(), body);
        if self.dead.load(Ordering::SeqCst) {
            return Err(anyhow!("language server exited"));
        }
        let mut stdin = self.stdin.lock().await;
        let written = async {
            stdin.write_all(frame.as_bytes()).await?;
            stdin.flush().await
        }
        .await;
        if written.is_err() {
            self.dead.store(true, Ordering::SeqCst);
        }
        Ok(written?)
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.id_counter.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();

        {
            let mut pending = self.pending.lock().await;
            pending.insert(id, tx);
        }

        if let Err(e) = self
            .send_raw(json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": method,
                "params": params
            }))
            .await
        {
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
        self.send_raw(json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params
        }))
        .await
    }

    async fn initialize(&self, root: &Path) -> Result<()> {
        let root_uri = path_to_uri(root);
        self.request(
            "initialize",
            json!({
                "processId": std::process::id(),
                "rootUri": root_uri,
                "rootPath": root.to_string_lossy(),
                "capabilities": {
                    "textDocument": {
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
                "initializationOptions": {}
            }),
        )
        .await?;

        self.notify("initialized", json!({})).await?;
        Ok(())
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
        let client = LspClient::connect(&cmd, &args, dir.path()).await.unwrap();
        let init = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            client.initialize(dir.path()),
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
        let client = LspClient::connect("sh", &["-c".to_string(), script], dir.path())
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
        let a = tool.client_for("sh", &args, dir.path()).await.unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !a.dead.load(Ordering::SeqCst) {
            assert!(std::time::Instant::now() < deadline, "exit never noticed");
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(a.notify("initialized", json!({})).await.is_err());
        assert!(a.pending.lock().await.is_empty());
        let b = tool.client_for("sh", &args, dir.path()).await.unwrap();
        assert!(
            !Arc::ptr_eq(&a, &b),
            "the dead server came back from the cache"
        );
        let starts = std::fs::read_to_string(&counter).unwrap().lines().count();
        assert_eq!(starts, 2, "server spawned {starts} times");
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
        let a = tool.client_for("sh", &args, dir.path()).await.unwrap();
        let b = tool.client_for("sh", &args, dir.path()).await.unwrap();
        assert!(Arc::ptr_eq(&a, &b), "second call must hit the cache");
        let starts = std::fs::read_to_string(&counter).unwrap().lines().count();
        assert_eq!(starts, 1, "server spawned {starts} times");
    }
}
