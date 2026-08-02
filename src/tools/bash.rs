/// BashTool — port of tools/BashTool/BashTool.ts
use super::{Tool, ToolContext, ToolOutput, async_trait};
use anyhow::Result;
use serde::Deserialize;
use serde_json::json;
use std::process::Stdio;
use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};
use tokio::time::{Duration, timeout};

/// RAII guard that owns a running `tokio::process::Child` and, on drop, kills
/// the *entire* Unix process group the child leads. This is required because:
///
///   1. `tokio::process::Child` does not kill on drop unless `kill_on_drop(true)`
///      is set — and even then it only SIGKILLs the direct child (the shell).
///   2. Any grandchildren spawned by the shell (e.g. `sleep 100 &`) would be
///      reparented to init and continue running as orphans.
///
/// By putting the shell in its own process group (`setpgid(0,0)` via
/// `process_group(0)`) and sending SIGKILL to the negated pgid on drop, we
/// guarantee the whole subtree dies when the tool future is dropped (Esc
/// cancellation, tokio::time::timeout, task::abort, etc.).
struct ProcessGroupGuard {
    child: Child,
    /// Process group ID = child pid (we always spawn with process_group(0)).
    /// `None` means the child was already reaped cleanly via `wait().await`,
    /// so Drop becomes a no-op.
    pgid: Option<i32>,
}

impl ProcessGroupGuard {
    fn new(child: Child) -> Self {
        // child.id() is None only if the child has already been polled to
        // completion. Since we just spawned it, this is always Some.
        let pgid = child.id().map(|id| id as i32);
        Self { child, pgid }
    }

    fn child_mut(&mut self) -> &mut Child {
        &mut self.child
    }

    /// Called after a successful `wait()` so Drop does not try to signal
    /// an already-reaped pid.
    fn disarm(&mut self) {
        self.pgid = None;
    }
}

impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(pgid) = self.pgid.take() {
            // `kill(-pgid, SIGKILL)` → send SIGKILL to every process in the
            // group. Using SIGKILL (not SIGTERM) because this path only runs
            // on cancellation/timeout; graceful shutdown is not possible when
            // the caller has already given up on the process.
            // SAFETY: libc::kill with a negative pid sends the signal to the
            // process group. Unsafe only because of FFI; arguments are valid.
            unsafe {
                libc::kill(-pgid, libc::SIGKILL);
            }
        }
    }
}

const DEFAULT_TIMEOUT_MS: u64 = 120_000; // 2 minutes, same as TypeScript default
const MAX_OUTPUT_BYTES: usize = 1_000_000; // 1MB cap
const CHUNK_SIZE: usize = 8192;

type StreamTx = Option<tokio::sync::mpsc::UnboundedSender<String>>;

/// Append one output line to the captured buffer and forward it to the TUI.
///
/// Storage stops at [`MAX_OUTPUT_BYTES`], but the caller keeps *reading* past
/// that point so the child process never blocks on a full pipe.
fn emit_line(raw: &str, tx: &StreamTx, combined: &mut String, truncated: &mut bool) {
    let clean = strip_ansi(raw);
    if clean.is_empty() {
        return;
    }
    if let Some(tx) = tx {
        let _ = tx.send(clean.clone());
    }
    if combined.len() >= MAX_OUTPUT_BYTES {
        *truncated = true;
        return;
    }
    // Trim the final line so the buffer never overshoots the cap, however long
    // a single line happens to be.
    let room = MAX_OUTPUT_BYTES - combined.len();
    if clean.len() > room {
        let cut = (0..=room)
            .rev()
            .find(|&i| clean.is_char_boundary(i))
            .unwrap_or(0);
        combined.push_str(&clean[..cut]);
        *truncated = true;
    } else {
        combined.push_str(&clean);
    }
    combined.push('\n');
}

/// Split a freshly-read chunk into complete lines, carrying any trailing
/// partial line over to the next chunk.
fn absorb(
    chunk: &[u8],
    partial: &mut Vec<u8>,
    tx: &StreamTx,
    combined: &mut String,
    truncated: &mut bool,
) {
    partial.extend_from_slice(chunk);
    while let Some(nl) = partial.iter().position(|&b| b == b'\n') {
        let line = partial.drain(..=nl).collect::<Vec<u8>>();
        let text = String::from_utf8_lossy(&line[..line.len() - 1]).into_owned();
        emit_line(&text, tx, combined, truncated);
    }
    // A single line longer than the cap would otherwise grow `partial` without
    // bound — flush it early rather than waiting for a newline that may never
    // arrive.
    if partial.len() > MAX_OUTPUT_BYTES {
        let text = String::from_utf8_lossy(partial).into_owned();
        emit_line(&text, tx, combined, truncated);
        partial.clear();
        *truncated = true;
    }
}

/// Strip ANSI escape sequences and carriage returns from terminal output.
/// Prevents progress-bar output (e.g. from `ollama pull`) from corrupting the TUI.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\r' => {
                // Carriage return — skip (progress bars use \r to overwrite lines)
            }
            '\x1b' => {
                // ESC — consume the escape sequence
                if chars.peek() == Some(&'[') {
                    chars.next(); // consume '['
                    // consume until a letter (the command character)
                    for c in chars.by_ref() {
                        if c.is_ascii_alphabetic() {
                            break;
                        }
                    }
                } else {
                    // Other ESC sequences — consume next char
                    chars.next();
                }
            }
            _ => out.push(ch),
        }
    }
    out
}

pub struct BashTool;

#[derive(Deserialize)]
#[allow(dead_code)] // fields populated by serde from LLM tool calls
struct BashInput {
    command: String,
    #[serde(default)]
    timeout: Option<u64>,
    #[serde(default)]
    description: Option<String>,
}

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &str {
        "Bash"
    }

    fn description(&self) -> &str {
        "Execute a bash command in the shell. Use for running tests, git commands, \
        build commands, installing packages, and other shell operations. \
        Avoid interactive commands. For long-running operations, consider adding \
        a timeout."
    }

    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The bash command to execute"
                },
                "timeout": {
                    "type": "number",
                    "description": "Timeout in milliseconds (default: 120000)"
                },
                "description": {
                    "type": "string",
                    "description": "Short description of what this command does"
                }
            },
            "required": ["command"]
        })
    }

    async fn execute(&self, input: serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let input: BashInput = serde_json::from_value(input)?;
        let timeout_ms = input.timeout.unwrap_or(DEFAULT_TIMEOUT_MS);

        // Apply sandbox if enabled
        let allow_net = ctx.sandbox_allow_network;
        let command = if let Some(ref mode) = ctx.sandbox_mode {
            match crate::sandbox::apply_sandbox(&input.command, mode, &ctx.cwd, allow_net) {
                Ok(cmd) => cmd,
                Err(reason) => return Ok(ToolOutput::error(reason)),
            }
        } else {
            input.command.clone()
        };

        let command_str = command.clone();
        let stream_tx = ctx.stream_tx.clone();
        let cwd = ctx.cwd.clone();
        // Resolve shell: ctx.default_shell → $SHELL env var → "bash"
        let shell = ctx
            .default_shell
            .clone()
            .unwrap_or_else(|| std::env::var("SHELL").unwrap_or_else(|_| "bash".into()));

        let fut = async move {
            let mut cmd = Command::new(&shell);
            cmd.arg("-c")
                .arg(&command)
                .current_dir(&cwd)
                // stdin defaults to *inherit*, which hands the spawned command the
                // TUI's own terminal. An interactive command (`sudo`, `ssh`, a bare
                // `read`) then competes with crossterm for the user's keystrokes and
                // hangs until the timeout. Nothing here can answer a prompt, so give
                // it EOF immediately and let the command fail fast instead.
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                // Defense in depth: if our Drop guard somehow doesn't fire,
                // tokio's kill_on_drop still SIGKILLs the direct shell.
                .kill_on_drop(true);

            // Put the shell in its own process group so we can SIGKILL the
            // entire subtree on cancellation. Without this, grandchildren
            // spawned via `sh -c '... & ...'` escape as orphans.
            #[cfg(unix)]
            {
                cmd.process_group(0);
            }

            let mut guard = ProcessGroupGuard::new(cmd.spawn()?);
            let child = guard.child_mut();

            let mut stdout = child
                .stdout
                .take()
                .ok_or_else(|| anyhow::anyhow!("Failed to capture stdout"))?;
            let mut stderr = child
                .stderr
                .take()
                .ok_or_else(|| anyhow::anyhow!("Failed to capture stderr"))?;

            // Read in fixed-size chunks rather than by line.
            //
            // `BufReader::lines()` accumulates until it sees a newline, so output
            // with no newlines at all (`yes | tr -d '\n'`) buffered the entire
            // stream into one String — the MAX_OUTPUT_BYTES check ran per line and
            // never got a chance to trip.
            //
            // Simply capping the reader is not enough either: once we stop reading,
            // the child blocks on a full pipe and never exits, so every command
            // over the cap would burn the full timeout instead of finishing.
            // Chunked reads let us bound what we *keep* while still draining to
            // EOF, so the child always completes.
            let mut combined = String::new();
            let mut truncated = false;
            let mut stdout_buf = vec![0u8; CHUNK_SIZE];
            let mut stderr_buf = vec![0u8; CHUNK_SIZE];
            // Partial trailing line per stream, kept as bytes so a multi-byte UTF-8
            // character split across a chunk boundary is not mangled.
            let mut stdout_partial: Vec<u8> = Vec::new();
            let mut stderr_partial: Vec<u8> = Vec::new();
            let mut stdout_done = false;
            let mut stderr_done = false;

            while !(stdout_done && stderr_done) {
                tokio::select! {
                    r = stdout.read(&mut stdout_buf), if !stdout_done => {
                        match r? {
                            0 => stdout_done = true,
                            n => absorb(
                                &stdout_buf[..n], &mut stdout_partial,
                                &stream_tx, &mut combined, &mut truncated,
                            ),
                        }
                    }
                    r = stderr.read(&mut stderr_buf), if !stderr_done => {
                        match r? {
                            0 => stderr_done = true,
                            n => absorb(
                                &stderr_buf[..n], &mut stderr_partial,
                                &stream_tx, &mut combined, &mut truncated,
                            ),
                        }
                    }
                }
            }

            // Flush any trailing text that never ended in a newline.
            for partial in [&mut stdout_partial, &mut stderr_partial] {
                if !partial.is_empty() {
                    let line = String::from_utf8_lossy(partial).into_owned();
                    emit_line(&line, &stream_tx, &mut combined, &mut truncated);
                    partial.clear();
                }
            }

            let status = guard.child_mut().wait().await?;
            // Process exited normally — disarm the kill guard so Drop
            // doesn't try to signal a pid that has already been reaped.
            guard.disarm();

            if truncated {
                combined.push_str("\n... (output truncated)");
            }

            if combined.is_empty() {
                combined = format!("(exit code: {})", status.code().unwrap_or(-1));
            }

            let is_error = !status.success();
            Ok::<ToolOutput, anyhow::Error>(if is_error {
                ToolOutput::error(combined)
            } else {
                ToolOutput::success(combined)
            })
        };

        match timeout(Duration::from_millis(timeout_ms), fut).await {
            Ok(result) => result,
            Err(_) => Ok(ToolOutput::error(format!(
                "Command timed out after {}ms: {}",
                timeout_ms, command_str
            ))),
        }
    }
}
