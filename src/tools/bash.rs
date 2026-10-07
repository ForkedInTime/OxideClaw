/// BashTool — port of tools/BashTool/BashTool.ts
use super::{Tool, ToolContext, ToolOutput, async_trait};
use anyhow::Result;
use serde::Deserialize;
use serde_json::json;
use std::process::Stdio;
use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};
use tokio::time::{Duration, Instant, sleep_until, timeout_at};

/// RAII guard that owns a running `tokio::process::Child` and, on drop, kills
/// the *entire* Unix process group the child leads. This is required because:
///
///   1. `tokio::process::Child` does not kill on drop unless `kill_on_drop(true)`
///      is set — and even then it only SIGKILLs the direct child (the shell).
///   2. Any grandchildren spawned by the shell (e.g. `sleep 100 &`) would be
///      reparented to init and continue running as orphans.
///
/// By putting the shell in its own process group (a new session via
/// [`new_session`]) and sending SIGKILL to the negated pgid on drop, we
/// guarantee the whole subtree dies when the tool future is dropped (Esc
/// cancellation, tokio::time::timeout, task::abort, etc.).
///
/// Windows has no process groups, and TerminateProcess on the shell leaves
/// everything it started running, so there the shell goes into a job object
/// (which its descendants inherit) and Drop terminates the job. Processes the
/// shell starts in the few instructions before the assignment escape it.
pub(crate) struct ProcessGroupGuard {
    child: Child,
    /// Process group ID = child pid (we always spawn with [`new_session`]).
    /// `None` means the child was already reaped cleanly via `wait().await`,
    /// so Drop becomes a no-op.
    pgid: Option<i32>,
    /// The job's HANDLE, kept as an integer so the guard stays `Send`.
    /// Without KILL_ON_JOB_CLOSE, so closing it on a disarmed guard leaves
    /// deliberate background jobs alone, as on Unix.
    #[cfg(windows)]
    job: Option<Job>,
}

impl ProcessGroupGuard {
    pub(crate) fn new(child: Child) -> Self {
        // child.id() is None only if the child has already been polled to
        // completion. Since we just spawned it, this is always Some.
        let pgid = child.id().map(|id| id as i32);
        #[cfg(windows)]
        let job = child
            .raw_handle()
            .and_then(|h| Job::assign(h as std::os::windows::io::RawHandle));
        Self {
            child,
            pgid,
            #[cfg(windows)]
            job,
        }
    }

    pub(crate) fn child_mut(&mut self) -> &mut Child {
        &mut self.child
    }

    /// Called after a successful `wait()` so Drop does not try to signal
    /// an already-reaped pid.
    pub(crate) fn disarm(&mut self) {
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
        #[cfg(windows)]
        if let Some(job) = self.job.take()
            && self.pgid.take().is_some()
        {
            job.terminate();
        }
    }
}

/// A Windows job object holding a process and everything it starts (children
/// inherit the job). Dropping it only closes the handle: no
/// KILL_ON_JOB_CLOSE, so a deliberate background job outlives a clean exit.
/// The HANDLE is kept as an integer so holders stay `Send`.
#[cfg(windows)]
pub(crate) struct Job(usize);

#[cfg(windows)]
impl Job {
    /// Put `process` (a live process handle) in a fresh job; `None` (the old
    /// shell-only kill) if Windows refuses.
    pub(crate) fn assign(process: std::os::windows::io::RawHandle) -> Option<Self> {
        use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
        use windows_sys::Win32::System::JobObjects::{AssignProcessToJobObject, CreateJobObjectW};
        // SAFETY: plain FFI with null (default) attributes and name; the
        // process handle is valid while its owner is alive, and a failed job
        // is closed.
        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job.is_null() {
                return None;
            }
            if AssignProcessToJobObject(job, process as HANDLE) == 0 {
                CloseHandle(job);
                return None;
            }
            Some(Job(job as usize))
        }
    }

    /// Kill every process in the job.
    pub(crate) fn terminate(&self) {
        use windows_sys::Win32::Foundation::HANDLE;
        use windows_sys::Win32::System::JobObjects::TerminateJobObject;
        // SAFETY: `self.0` is the live handle `assign` created; only Drop
        // closes it.
        unsafe {
            TerminateJobObject(self.0 as HANDLE, 1);
        }
    }
}

#[cfg(windows)]
impl Drop for Job {
    fn drop(&mut self) {
        use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
        // SAFETY: the handle `assign` created, closed exactly once here.
        unsafe {
            CloseHandle(self.0 as HANDLE);
        }
    }
}

/// Start the command as leader of a new session: its own process group (pgid
/// == pid, which [`ProcessGroupGuard`] relies on) and no controlling terminal.
///
/// `process_group(0)` alone left it a background job in the TUI's session, so
/// anything that opens `/dev/tty` to prompt (sudo, ssh, git's credential
/// prompt) was stopped by SIGTTIN/SIGTTOU and sat there until the timeout.
/// With no terminal that open fails with ENXIO and the command errors at once.
#[cfg(unix)]
pub(crate) fn new_session(cmd: &mut std::process::Command) {
    use std::os::unix::process::CommandExt;
    // SAFETY: the closure runs between fork and exec and only calls setsid,
    // which is async-signal-safe.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

const DEFAULT_TIMEOUT_MS: u64 = 120_000; // 2 minutes, same as TypeScript default
pub(crate) const MAX_OUTPUT_BYTES: usize = 1_000_000; // 1MB cap
const CHUNK_SIZE: usize = 8192;

type StreamTx = Option<tokio::sync::mpsc::UnboundedSender<String>>;

/// Append one output line to the captured buffer and forward it to the TUI.
///
/// Storage stops at [`MAX_OUTPUT_BYTES`], but the caller keeps *reading* past
/// that point so the child process never blocks on a full pipe.
fn emit_line(raw: &str, tx: &StreamTx, combined: &mut String, truncated: &mut bool) {
    let clean = strip_ansi(raw);
    // Drop lines that were only escape codes, but keep real blank lines:
    // without them `cat` output no longer matches the file and a later Edit
    // copied from it fails with "not found".
    if clean.is_empty() && !raw.is_empty() {
        return;
    }
    // Past the cap we keep *draining* the pipe (so the child can exit instead of
    // blocking on a full one) but stop *forwarding*. `stream_tx` is unbounded:
    // without this, a command emitting millions of lines queues a clone of every
    // one, so bounding `combined` alone did not bound memory.
    if *truncated || combined.len() >= MAX_OUTPUT_BYTES {
        *truncated = true;
        return;
    }
    if let Some(tx) = tx {
        let _ = tx.send(clean.clone());
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

/// Read a pipe to EOF into `kept`, keeping at most `cap` bytes. What was read
/// stays in `kept` if the future is dropped (timeout), so it can be reported.
///
/// Draining past the cap matters: stopping the read leaves the child blocked on
/// a full pipe until its timeout fires.
pub(crate) async fn read_to_cap<R>(
    reader: &mut R,
    cap: usize,
    kept: &mut Vec<u8>,
    truncated: &mut bool,
) -> std::io::Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut buf = vec![0u8; CHUNK_SIZE];
    loop {
        let n = reader.read(&mut buf).await?;
        if n == 0 {
            return Ok(());
        }
        if kept.len() < cap {
            let room = cap - kept.len();
            kept.extend_from_slice(&buf[..room.min(n)]);
            if n > room {
                *truncated = true;
            }
        } else {
            *truncated = true;
        }
    }
}

/// How long a background job's output is still collected once the shell
/// itself has exited.
const BACKGROUND_GRACE: Duration = Duration::from_millis(200);

/// Why [`drain_until_exit`] stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Drained {
    /// Both pipes reached EOF.
    All,
    /// The shell exited but something it started in the background still
    /// holds the pipes open.
    ShellExited,
    TimedOut,
}

/// Run `reads` (which drains the command's pipes) until they close, the shell
/// exits while a background job (`server &`) keeps them open, or `deadline`.
/// Waiting for EOF alone blocked every `cmd &` until the timeout, which then
/// discarded all output and killed the job the model had just started.
/// Also returns the shell's exit status if it was reaped.
pub(crate) async fn drain_until_exit<F>(
    reads: F,
    child: &mut Child,
    deadline: Instant,
) -> std::io::Result<(Drained, Option<std::process::ExitStatus>)>
where
    F: std::future::Future<Output = std::io::Result<()>>,
{
    tokio::pin!(reads);
    let mut status = None;
    let mut grace: Option<Instant> = None;
    loop {
        tokio::select! {
            r = &mut reads => {
                r?;
                return Ok((Drained::All, status));
            }
            st = child.wait(), if status.is_none() => {
                status = Some(st?);
                grace = Some(Instant::now() + BACKGROUND_GRACE);
            }
            _ = sleep_until(grace.unwrap_or(deadline)), if grace.is_some() => {
                return Ok((Drained::ShellExited, status));
            }
            _ = sleep_until(deadline) => return Ok((Drained::TimedOut, status)),
        }
    }
}

/// Keep reading a pipe a background job still writes to after we stop
/// capturing it: closing our end would kill the job with SIGPIPE.
pub(crate) fn discard_rest<R>(mut reader: R)
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let _ = tokio::io::copy(&mut reader, &mut tokio::io::sink()).await;
    });
}

pub(crate) const BACKGROUND_NOTE: &str = "(a background process is still running; its \
     further output is not captured: redirect it to a file to read it)";

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

/// File name of a shell path without a Windows `.exe` suffix, e.g.
/// `/usr/bin/bash` -> `bash`, `C:\Git\bin\bash.exe` -> `bash`.
pub fn shell_file_name(shell: &str) -> &str {
    let name = shell.rsplit(['/', '\\']).next().unwrap_or(shell);
    match name.len().checked_sub(4) {
        Some(i) if name.is_char_boundary(i) && name[i..].eq_ignore_ascii_case(".exe") => &name[..i],
        _ => name,
    }
}

/// The interpreter the Bash tool runs commands with: the `defaultShell`
/// setting, else the login shell only when it speaks bash syntax, else `bash`.
/// The tool contract and the model's commands are bash; under a fish, nu or
/// tcsh login shell heredocs, `if ...; then` and `$?` would all fail to parse.
/// Where bash is not installed (Alpine/BusyBox, SHELL=/bin/sh) a POSIX login
/// shell, else `sh`, beats a shell that does not exist.
/// The shell that parses the Bash tool's final command string. A bwrap or
/// firejail wrapper quotes the command for a POSIX shell (`'\''`); fish or
/// PowerShell read that quoting differently, so the command could close the
/// quote and run on the host outside the jail. Only `/bin/sh` may parse it,
/// and bash runs the command inside the jail either way.
pub fn command_shell(
    sandbox_mode: Option<&str>,
    default_shell: Option<&str>,
    login_shell: Option<&str>,
) -> String {
    if sandbox_mode.is_some_and(crate::sandbox::wraps_in_shell) {
        return "/bin/sh".to_string();
    }
    bash_tool_shell(default_shell, login_shell)
}

pub fn bash_tool_shell(default_shell: Option<&str>, login_shell: Option<&str>) -> String {
    bash_tool_shell_with(default_shell, login_shell, has_bash)
}

fn bash_tool_shell_with(
    default_shell: Option<&str>,
    login_shell: Option<&str>,
    has_bash: impl Fn() -> bool,
) -> String {
    if let Some(s) = default_shell {
        return s.to_string();
    }
    let login_is = |names: &[&str]| {
        login_shell.filter(|s| names.contains(&shell_file_name(s).to_ascii_lowercase().as_str()))
    };
    if let Some(s) = login_is(&["bash", "zsh"]) {
        return s.to_string();
    }
    if has_bash() {
        return "bash".to_string();
    }
    login_is(&["sh", "ash", "dash", "ksh", "mksh", "posh"])
        .unwrap_or("sh")
        .to_string()
}

/// Whether `bash` is on PATH (looked up once).
pub fn has_bash() -> bool {
    static HAS: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *HAS.get_or_init(|| {
        std::env::var_os("PATH").is_some_and(|p| {
            std::env::split_paths(&p)
                .any(|d| d.join("bash").is_file() || d.join("bash.exe").is_file())
        })
    })
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
        a timeout. To start a server or other long-running background process, \
        redirect its output (cmd > /tmp/cmd.log 2>&1 &) and read the log."
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
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        let stream_tx = ctx.stream_tx.clone();
        let cwd = ctx.cwd.clone();
        let extra_env = ctx.env.clone();
        let shell = command_shell(
            ctx.sandbox_mode.as_deref(),
            ctx.default_shell.as_deref(),
            std::env::var("SHELL").ok().as_deref(),
        );

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
                .kill_on_drop(true)
                .envs(&extra_env);

            // Own process group so we can SIGKILL the entire subtree on
            // cancellation. Without this, grandchildren spawned via
            // `sh -c '... & ...'` escape as orphans.
            #[cfg(unix)]
            new_session(cmd.as_std_mut());

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

            let reads = async {
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
                Ok(())
            };
            let (drained, status) = drain_until_exit(reads, guard.child_mut(), deadline).await?;

            // Flush any trailing text that never ended in a newline.
            for partial in [&mut stdout_partial, &mut stderr_partial] {
                if !partial.is_empty() {
                    let line = String::from_utf8_lossy(partial).into_owned();
                    emit_line(&line, &stream_tx, &mut combined, &mut truncated);
                    partial.clear();
                }
            }

            if truncated {
                combined.push_str("\n... (output truncated)");
            }

            // Both pipes closed does not mean the shell is gone
            // (`exec >&- 2>&-; sleep 999`), so the deadline still applies.
            let status = match (drained, status) {
                (Drained::TimedOut, _) => None,
                (_, Some(st)) => Some(st),
                (_, None) => timeout_at(deadline, guard.child_mut().wait())
                    .await
                    .ok()
                    .transpose()?,
            };
            let Some(status) = status else {
                // The guard kills the whole group as it drops; what the
                // command printed so far is still worth returning.
                if !combined.is_empty() && !combined.ends_with('\n') {
                    combined.push('\n');
                }
                combined.push_str(&format!(
                    "Command timed out after {timeout_ms}ms: {command_str}"
                ));
                return Ok(ToolOutput::error(combined));
            };
            // The shell has been reaped — disarm the kill guard so Drop
            // doesn't signal the group, which now holds only whatever the
            // command deliberately left running in the background.
            guard.disarm();
            if drained == Drained::ShellExited {
                discard_rest(stdout);
                discard_rest(stderr);
                if !combined.is_empty() && !combined.ends_with('\n') {
                    combined.push('\n');
                }
                combined.push_str(BACKGROUND_NOTE);
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

        fut.await
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// settings.json `env` was documented as reaching Bash but was never set
    /// on the child process.
    #[tokio::test]
    async fn settings_env_reaches_the_command() {
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = ToolContext::new(dir.path().to_path_buf());
        ctx.default_shell = Some("sh".into());
        ctx.env
            .insert("OXIDECLAW_TEST_SETTING".into(), "from-settings".into());
        let out = BashTool
            .execute(
                serde_json::json!({ "command": "echo \"v=$OXIDECLAW_TEST_SETTING\"" }),
                &ctx,
            )
            .await
            .unwrap();
        let text: String = out
            .content
            .iter()
            .map(|c| match c {
                crate::api::types::ToolResultContent::Text { text } => text.as_str(),
            })
            .collect();
        assert!(text.contains("v=from-settings"), "{text}");
    }

    /// The shell must lead its own session: as a mere background process
    /// group in the TUI's session, `sudo`/`ssh` prompts on /dev/tty stopped it
    /// (SIGTTIN/SIGTTOU) until the timeout.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn command_runs_in_its_own_session() {
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = ToolContext::new(dir.path().to_path_buf());
        ctx.default_shell = Some("sh".into());
        let out = BashTool
            .execute(
                serde_json::json!({
                    "command": "read -r pid comm state ppid pgrp sid rest < /proc/$$/stat; \
                                echo \"pid=$pid pgrp=$pgrp sid=$sid\""
                }),
                &ctx,
            )
            .await
            .unwrap();
        let text: String = out
            .content
            .iter()
            .map(|c| match c {
                crate::api::types::ToolResultContent::Text { text } => text.as_str(),
            })
            .collect();
        let field = |k: &str| {
            text.split_whitespace()
                .find_map(|w| w.strip_prefix(k))
                .unwrap_or_else(|| panic!("no {k} in {text}"))
                .to_string()
        };
        let pid = field("pid=");
        assert_eq!(field("pgrp="), pid, "{text}");
        assert_eq!(field("sid="), pid, "{text}");
    }
}

#[cfg(test)]
mod shell_choice_tests {
    use super::*;

    #[test]
    fn non_bash_login_shells_fall_back_to_bash() {
        let shell = |d: Option<&str>, l: Option<&str>| bash_tool_shell_with(d, l, || true);
        for login in [
            "/usr/bin/fish",
            "/usr/bin/nu",
            "/bin/tcsh",
            "xonsh",
            "/bin/sh",
        ] {
            assert_eq!(shell(None, Some(login)), "bash", "{login}");
        }
        assert_eq!(shell(None, None), "bash");
        for login in ["/bin/bash", "/usr/local/bin/zsh", r"C:\Git\bin\bash.exe"] {
            assert_eq!(shell(None, Some(login)), login);
        }
        // An explicit defaultShell is the user's choice and always wins.
        assert_eq!(shell(Some("powershell"), Some("/bin/bash")), "powershell");
        assert_eq!(shell(Some("/usr/bin/fish"), None), "/usr/bin/fish");
    }

    /// A bwrap/firejail wrapper is POSIX-quoted; run under a fish
    /// `defaultShell`, `echo \'; rm -rf ~ #` closed the quote and ran
    /// `rm -rf ~` on the host, outside the jail.
    #[test]
    fn a_wrapped_command_is_parsed_by_a_posix_shell() {
        for default in [Some("/usr/bin/fish"), Some("pwsh"), None] {
            for mode in ["bwrap", "firejail"] {
                assert_eq!(
                    command_shell(Some(mode), default, Some("/usr/bin/fish")),
                    "/bin/sh",
                    "{mode} {default:?}"
                );
            }
            for mode in [Some("strict"), None] {
                assert_eq!(
                    command_shell(mode, default, Some("/bin/bash")),
                    bash_tool_shell(default, Some("/bin/bash")),
                    "{mode:?} {default:?}"
                );
            }
        }
        assert_eq!(
            command_shell(Some("strict"), Some("/usr/bin/fish"), None),
            "/usr/bin/fish"
        );
    }

    /// Alpine/BusyBox ship no bash and set SHELL=/bin/sh: every Bash tool
    /// call failed to spawn a `bash` that does not exist.
    #[test]
    fn without_bash_a_posix_shell_runs_commands() {
        let shell = |l: Option<&str>| bash_tool_shell_with(None, l, || false);
        assert_eq!(shell(Some("/bin/sh")), "/bin/sh");
        assert_eq!(shell(Some("/bin/ash")), "/bin/ash");
        assert_eq!(shell(Some("/usr/bin/fish")), "sh");
        assert_eq!(shell(None), "sh");
        assert_eq!(shell(Some("/bin/zsh")), "/bin/zsh");
    }

    #[test]
    fn shell_file_name_strips_dirs_and_exe() {
        assert_eq!(shell_file_name("/usr/bin/zsh"), "zsh");
        assert_eq!(shell_file_name(r"C:\Git\bin\bash.EXE"), "bash");
        assert_eq!(shell_file_name("bash"), "bash");
    }
}
