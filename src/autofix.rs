//! Auto-fix loop — core primitives.
//!
//! Watches Write/Edit tool calls. On lint/test failure, the caller feeds the
//! failure output back to the model as a synthetic user turn and re-enters the
//! agentic loop, up to `max_retries` times. No files are reverted; the working
//! tree is left as-is on retry-cap.
//!
//! This module provides only the building blocks:
//!   - `detect_lint_command` / `detect_test_command` — infer runners from project files
//!   - `should_trigger`       — apply trigger-mode rules
//!   - `run_command`          — execute a single lint or test command
//!   - `run_checks`           — run lint + test together and classify the outcome
//!   - `format_feedback_message` — build the anti-cheat retry prompt
//!   - `run_auto_fix_check`   — top-level decision helper returning `AutoFixAction`
//!   - `LspDiagnostics`       — language-server errors in the edited files, gathered
//!     while lint and tests run

use crate::permissions::Autonomy;
use crate::tools::lsp::{Launch, LspPool};
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

// ── Config types ──────────────────────────────────────────────────────────────

/// Configuration for auto-fix loop feature.
#[derive(Debug, Clone)]
pub struct AutoFixConfig {
    pub enabled: bool,
    pub trigger: AutoFixTrigger,
    pub lint_command: Option<String>,
    pub test_command: Option<String>,
    /// Maximum number of lint/test → feedback → re-prompt rounds within a
    /// single user turn. Honored by the TUI agentic loop: on
    /// `AutoFixAction::Retry`, the failure output is appended as a
    /// synthetic user message and the model is re-prompted; on
    /// `AutoFixAction::GiveUp` the working tree is left as-is.
    /// Clamped to `1..=10` by `Config::load`.
    pub max_retries: u32,
    /// Maximum wall-clock seconds to let the test command run before killing
    /// it and returning `CommandResult::Timeout`. `0` means no timeout.
    pub timeout_secs: u64,
    /// Language-server diagnostics for the edited files (`autoFixLoop.lsp`).
    pub lsp: LspDiagnosticsConfig,
}

/// How the check asks language servers about the edited files.
#[derive(Debug, Clone)]
pub struct LspDiagnosticsConfig {
    /// On by default whenever the auto-fix loop is (`autoFixLoop.lsp`).
    pub enabled: bool,
    /// How long to keep listening after a file's first diagnostics arrive,
    /// for the slower semantic pass many servers send after a syntax pass.
    pub settle: Duration,
    /// Hard cap on the whole step, server start-up included.
    pub timeout: Duration,
    /// Report warnings too, not only errors.
    pub warnings: bool,
}

pub const DEFAULT_LSP_SETTLE_MS: u64 = 2_000;
pub const DEFAULT_LSP_TIMEOUT_MS: u64 = 10_000;

impl Default for LspDiagnosticsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            settle: Duration::from_millis(DEFAULT_LSP_SETTLE_MS),
            timeout: Duration::from_millis(DEFAULT_LSP_TIMEOUT_MS),
            warnings: false,
        }
    }
}

/// Default test timeout if the user hasn't overridden it.
pub const DEFAULT_TEST_TIMEOUT_SECS: u64 = 60;

/// Shown once per session when an edit would have started a check in a
/// project that is not in `trustedProjects`.
pub const UNTRUSTED_NOTICE: &str = "Auto-fix skipped: this folder is not trusted. \
     Run /trust to let OxideClaw run its lint and test commands and language servers.";

/// What auto-fix may run and how contained. Lint and test commands execute
/// project code (`build.rs`, `conftest.py`, npm scripts, Makefiles), so an
/// untrusted project runs none of them, and a trusted one runs them under
/// the same sandbox as the Bash tool.
#[derive(Debug, Clone, Default)]
pub struct Containment {
    /// The project is in the global `trustedProjects` list (`/trust`).
    pub trusted: bool,
    /// The Bash tool's sandbox mode when the user enabled one; `None` runs
    /// the commands directly, as Bash does.
    pub sandbox_mode: Option<String>,
    pub sandbox_allow_network: bool,
}

impl Containment {
    /// `cmd` as the shell should run it: wrapped by the active sandbox, or
    /// unchanged without one. `Err` when the sandbox refuses it (a strict
    /// pattern, a missing backend, an unknown mode): never run it bare then.
    pub fn wrap(&self, cmd: &str, cwd: &Path) -> Result<String, String> {
        match &self.sandbox_mode {
            Some(mode) => crate::sandbox::apply_sandbox(cmd, mode, cwd, self.sandbox_allow_network),
            None => Ok(cmd.to_string()),
        }
    }
}

/// When should the auto-fix check run?
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutoFixTrigger {
    /// Run in every `/autonomy` mode but `suggest`.
    Autonomous,
    /// Run after every edit regardless of autonomy.
    Always,
    /// Never run.
    Off,
}

/// Result of running a single lint or test command.
#[derive(Debug)]
pub enum CommandResult {
    Pass,
    Fail {
        stderr: String,
    },
    /// Skipped due to trigger rules, git errors, etc.
    Skipped {
        reason: String,
    },
    Timeout,
}

impl Default for AutoFixConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            trigger: AutoFixTrigger::Autonomous,
            lint_command: None,
            test_command: None,
            max_retries: 3,
            timeout_secs: DEFAULT_TEST_TIMEOUT_SECS,
            lsp: LspDiagnosticsConfig::default(),
        }
    }
}

// ── Detection ─────────────────────────────────────────────────────────────────

/// Detect a test command based on project files in `cwd`.
/// If `override_cmd` is `Some`, return it unchanged.
/// Returns `None` if no runner detected AND no override given.
///
/// Detection order (first hit wins):
///   1. Cargo.toml      → `cargo test`
///   2. package.json    → `npm test`
///   3. pyproject.toml  → `pytest`
///   4. setup.py        → `pytest`
///   5. go.mod          → `go test ./...`
pub fn detect_test_command(cwd: &Path, override_cmd: &Option<String>) -> Option<String> {
    if let Some(cmd) = override_cmd {
        return Some(cmd.clone());
    }

    if cwd.join("Cargo.toml").is_file() {
        return Some("cargo test".to_string());
    }
    if cwd.join("package.json").is_file() {
        return Some("npm test".to_string());
    }
    if cwd.join("pyproject.toml").is_file() || cwd.join("setup.py").is_file() {
        return Some("pytest".to_string());
    }
    if cwd.join("go.mod").is_file() {
        return Some("go test ./...".to_string());
    }
    None
}

/// Detect a lint command based on project files in `cwd`.
/// If `override_cmd` is `Some`, return it unchanged.
/// Returns `None` if no linter detected AND no override given.
///
/// Detection order (first hit wins, mirrors `detect_test_command`):
///   1. Cargo.toml      → `cargo clippy --all-targets -- -D warnings`
///   2. package.json    → `npx --no-install eslint .`
///   3. pyproject.toml  → `ruff check .`
///   4. setup.py        → `ruff check .`
///   5. go.mod          → `go vet ./...`
pub fn detect_lint_command(cwd: &Path, override_cmd: &Option<String>) -> Option<String> {
    if let Some(cmd) = override_cmd {
        return Some(cmd.clone());
    }

    if cwd.join("Cargo.toml").is_file() {
        return Some("cargo clippy --all-targets -- -D warnings".to_string());
    }
    if cwd.join("package.json").is_file() {
        return Some("npx --no-install eslint .".to_string());
    }
    if cwd.join("pyproject.toml").is_file() || cwd.join("setup.py").is_file() {
        return Some("ruff check .".to_string());
    }
    if cwd.join("go.mod").is_file() {
        return Some("go vet ./...".to_string());
    }
    None
}

/// An auto-detected command if it can actually run in `cwd`, else `None`.
/// Project files say which runner a project uses, not that it is installed:
/// a Python repo without ruff, a Rust toolchain without clippy, or npm's
/// default `"test": "echo \"Error: no test specified\" && exit 1"` would
/// otherwise fail every check and send the model chasing an error it cannot
/// fix. `path` is the PATH to search (a parameter so tests do not depend on
/// the machine's). The clippy probe runs a binary the project picks (rustup
/// honours `rust-toolchain.toml`, whose `path` can point into the repo), so
/// it goes through `containment` like the checks themselves; a probe the
/// sandbox refuses means "not runnable".
fn runnable_detected(
    cwd: &Path,
    cmd: String,
    path: Option<&std::ffi::OsStr>,
    containment: &Containment,
    timeout_secs: u64,
    cancel: &AtomicBool,
) -> Option<String> {
    let program = cmd.split_whitespace().next()?.to_string();
    // A project virtualenv is where Python tools usually live, and it is
    // often not activated in the shell oxideclaw was started from.
    if matches!(program.as_str(), "ruff" | "pytest")
        && is_executable(&cwd.join(VENV_BIN).join(&program))
    {
        return Some(format!("{VENV_BIN}{}{cmd}", std::path::MAIN_SEPARATOR));
    }
    let resolved = find_on_path(&program, path)?;
    if cmd.starts_with("cargo clippy") {
        // `cargo clippy` without the clippy component exits 101.
        let resolved = resolved.display().to_string();
        // run_command goes through `sh -c` on unix and `cmd /D /S /C` on
        // Windows; both split an unquoted path at a space (`C:\Users\John
        // Doe\...`). Windows paths cannot contain `"`. Not the bare name:
        // cmd.exe searches the current directory (the repo) before PATH.
        #[cfg(unix)]
        let resolved = crate::sandbox::shell_quote(&resolved);
        #[cfg(windows)]
        let resolved = format!("\"{resolved}\"");
        let probe = format!("{resolved} clippy --version");
        let wrapped = containment.wrap(&probe, cwd).ok()?;
        if !matches!(
            run_command(
                cwd,
                &wrapped,
                timeout_secs,
                cancel,
                containment.sandbox_mode.is_some()
            ),
            CommandResult::Pass
        ) {
            return None;
        }
    }
    if cmd.starts_with("npx --no-install eslint") && !eslint_configured(cwd) {
        return None;
    }
    if cmd == "npm test" && !has_real_npm_test_script(cwd) {
        return None;
    }
    Some(cmd)
}

#[cfg(unix)]
fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(p: &Path) -> bool {
    p.is_file() || p.with_extension("exe").is_file()
}

#[cfg(unix)]
const VENV_BIN: &str = ".venv/bin";
// Backslashes: cmd.exe reads `/Scripts` in `.venv/Scripts/ruff` as a switch.
#[cfg(windows)]
const VENV_BIN: &str = r".venv\Scripts";

#[cfg(unix)]
pub(crate) fn find_on_path(
    program: &str,
    path: Option<&std::ffi::OsStr>,
) -> Option<std::path::PathBuf> {
    std::env::split_paths(path?)
        .map(|dir| dir.join(program))
        .find(|p| is_executable(p))
}

/// npm and npx are `.cmd` shims on Windows, so a bare `dir\npm` or
/// `dir\npm.exe` never matched and every JS check was dropped unseen.
#[cfg(windows)]
pub(crate) fn find_on_path(
    program: &str,
    path: Option<&std::ffi::OsStr>,
) -> Option<std::path::PathBuf> {
    crate::mcp::client::resolve_on_path(program, path, std::env::var_os("PATHEXT").as_deref())
}

fn eslint_configured(cwd: &Path) -> bool {
    const CONFIGS: [&str; 12] = [
        "eslint.config.js",
        "eslint.config.mjs",
        "eslint.config.cjs",
        "eslint.config.ts",
        "eslint.config.mts",
        "eslint.config.cts",
        ".eslintrc",
        ".eslintrc.js",
        ".eslintrc.cjs",
        ".eslintrc.json",
        ".eslintrc.yml",
        ".eslintrc.yaml",
    ];
    let has_config = CONFIGS.iter().any(|f| cwd.join(f).is_file())
        || read_package_json(cwd).is_some_and(|v| v.get("eslintConfig").is_some());
    has_config && cwd.join("node_modules/.bin/eslint").exists()
}

fn has_real_npm_test_script(cwd: &Path) -> bool {
    read_package_json(cwd)
        .and_then(|v| v["scripts"]["test"].as_str().map(str::to_owned))
        .is_some_and(|s| !s.trim().is_empty() && !s.contains("no test specified"))
}

fn read_package_json(cwd: &Path) -> Option<serde_json::Value> {
    serde_json::from_str(&std::fs::read_to_string(cwd.join("package.json")).ok()?).ok()
}

// ── Trigger logic ─────────────────────────────────────────────────────────────

/// Check if the auto-fix loop should trigger right now.
///
/// `autonomy_mode` is the session's current `/autonomy` mode.
pub fn should_trigger(config: &AutoFixConfig, autonomy_mode: Autonomy) -> bool {
    if !config.enabled {
        return false;
    }
    match config.trigger {
        AutoFixTrigger::Off => false,
        AutoFixTrigger::Always => true,
        AutoFixTrigger::Autonomous => autonomy_mode != Autonomy::Suggest,
    }
}

// ── Test runner ───────────────────────────────────────────────────────────────

/// Run a single command and return a `CommandResult`.
///
/// `cmd` is a shell command line like `"cargo test"` or `"npm test"`, run
/// by `sh -c` on Unix and `cmd /C` on Windows. `timeout_secs` caps total wall-clock runtime; if the
/// process is still running past it we send SIGKILL and return
/// `CommandResult::Timeout`. Pass `0` to wait indefinitely. Setting
/// `cancel` (Esc in the TUI) kills the command the same way and returns
/// `Skipped`: lint and tests otherwise ran on, unseen, for minutes.
///
/// NOTE on implementation: the original spec called for `wait_timeout`, but
/// pulling in a new dep for ~20 lines isn't worth it. We use a poll+kill loop
/// via `try_wait`, which has the same behavior with no extra deps.
///
/// `sandboxed` (the command is contained like the Bash tool's) keeps
/// OxideClaw's provider keys out of its environment, as Bash does.
pub fn run_command(
    cwd: &Path,
    cmd: &str,
    timeout_secs: u64,
    cancel: &AtomicBool,
    sandboxed: bool,
) -> CommandResult {
    if cmd.trim().is_empty() {
        return CommandResult::Skipped {
            reason: "empty test command".to_string(),
        };
    }
    if cancel.load(Ordering::SeqCst) {
        return CommandResult::Skipped {
            reason: "cancelled".to_string(),
        };
    }
    // Through the shell, so quoting, env assignments and `&&` work
    // (`pytest -k "a b"` used to be split on the space).
    #[cfg(unix)]
    let mut command = {
        let mut c = Command::new("sh");
        c.arg("-c").arg(cmd);
        // Own process group for the timeout kill, and no controlling terminal
        // so a check that prompts on /dev/tty fails instead of hanging.
        crate::tools::bash::new_session(&mut c);
        c
    };
    // cmd.exe, which also runs `.cmd` shims such as npm and npx (process
    // creation alone finds only `.exe`). `/S` with the outer quotes makes it
    // strip exactly those, leaving the command's own quoting intact; `/D`
    // skips AutoRun hooks from the registry.
    #[cfg(windows)]
    let mut command = {
        use std::os::windows::process::CommandExt;
        let mut c = Command::new("cmd");
        c.args(["/D", "/S", "/C"]).raw_arg(format!("\"{cmd}\""));
        c
    };
    if sandboxed {
        crate::sandbox::scrub_credentials(&mut command);
    }
    let spawn = command
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();

    let mut child = match spawn {
        Ok(c) => c,
        Err(e) => {
            return CommandResult::Skipped {
                reason: format!("failed to spawn `{cmd}`: {e}"),
            };
        }
    };
    // Killing cmd.exe alone leaves the cargo/npm/pytest tree it started
    // running and holding the pipes; the job reaches all of it. Dropped (and
    // closed) on every return path.
    #[cfg(windows)]
    let job = {
        use std::os::windows::io::AsRawHandle;
        crate::tools::bash::Job::assign(child.as_raw_handle())
    };

    // Drain both pipes while the command runs. Reading only after exit
    // deadlocked any run printing more than a pipe buffer (64 KiB — a normal
    // `cargo test`): the child blocked on write and was reported as timed out.
    // Results come back over a channel so the wait for them can be bounded:
    // a process that left the group (setsid) can hold a pipe open forever.
    fn drain(
        pipe: Option<impl std::io::Read + Send + 'static>,
        is_stdout: bool,
        tx: std::sync::mpsc::Sender<(bool, Vec<u8>)>,
    ) {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut p) = pipe {
                let _ = p.read_to_end(&mut buf);
            }
            let _ = tx.send((is_stdout, buf));
        });
    }
    let (tx, rx) = std::sync::mpsc::channel();
    drain(child.stdout.take(), true, tx.clone());
    drain(child.stderr.take(), false, tx);

    // Poll until exit or timeout. `timeout_secs == 0` means "wait forever".
    let timeout = std::time::Duration::from_secs(timeout_secs);
    let has_timeout = timeout_secs > 0;
    let start = std::time::Instant::now();
    let poll = std::time::Duration::from_millis(100);

    let status = loop {
        // Background jobs the command left behind (`server &`, a daemon)
        // keep the pipes open, so the reads below would wait on them past
        // the timeout. Kill the group while the exited leader is still an
        // unreaped zombie: that keeps its pgid from being reused, so the
        // kill cannot reach an unrelated group.
        #[cfg(unix)]
        let exited = exited_unreaped(child.id());
        #[cfg(not(unix))]
        let exited: Option<bool> = None;
        #[cfg(unix)]
        if exited == Some(true) {
            unsafe {
                libc::kill(-(child.id() as i32), libc::SIGKILL);
            }
        }
        // Reap only after that kill, or an exit between the two calls would
        // skip it. Without waitid, fall back to plain polling.
        let polled = if exited == Some(false) {
            Ok(None)
        } else {
            child.try_wait()
        };
        match polled {
            Ok(Some(status)) => break status,
            Ok(None) => {
                let cancelled = cancel.load(Ordering::SeqCst);
                if cancelled || (has_timeout && start.elapsed() >= timeout) {
                    // The whole group: test binaries outlive a killed `cargo`
                    // and would hold the pipes open.
                    #[cfg(unix)]
                    unsafe {
                        libc::kill(-(child.id() as i32), libc::SIGKILL);
                    }
                    #[cfg(windows)]
                    if let Some(job) = &job {
                        job.terminate();
                    }
                    let _ = child.kill();
                    let _ = child.wait();
                    if cancelled {
                        return CommandResult::Skipped {
                            reason: "cancelled".to_string(),
                        };
                    }
                    return CommandResult::Timeout;
                }
                std::thread::sleep(poll);
            }
            Err(e) => {
                return CommandResult::Skipped {
                    reason: format!("failed to wait on child: {e}"),
                };
            }
        }
    };
    // The leader has exited and been reaped, so `status` is the result. A
    // process that left the group (setsid, a daemonizing test server) can
    // hold the pipes open where the group kill cannot reach it: wait a short
    // grace for the output, in slices that honour Esc, and never turn a
    // finished check into a Timeout (or, with no timeout, wait forever).
    let grace = std::time::Duration::from_secs(2);
    let exit_at = std::time::Instant::now();
    let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
    let mut got_n = 0;
    while got_n < 2 {
        if cancel.load(Ordering::SeqCst) {
            return CommandResult::Skipped {
                reason: "cancelled".to_string(),
            };
        }
        if exit_at.elapsed() >= grace {
            break;
        }
        match rx.recv_timeout(poll) {
            Ok((true, buf)) => {
                stdout = buf;
                got_n += 1;
            }
            Ok((false, buf)) => {
                stderr = buf;
                got_n += 1;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    if status.success() {
        CommandResult::Pass
    } else {
        // Both streams: cargo test / go test put the failures on stdout while
        // stderr has build noise. stdout goes last so the tail-trimmed
        // feedback keeps the failure summary.
        let stderr = String::from_utf8_lossy(&stderr);
        let stdout = String::from_utf8_lossy(&stdout);
        let combined = match (stderr.trim().is_empty(), stdout.trim().is_empty()) {
            (_, true) => stderr.into_owned(),
            (true, false) => stdout.into_owned(),
            (false, false) => format!("{stderr}\n{stdout}"),
        };
        CommandResult::Fail { stderr: combined }
    }
}

/// Whether `pid` has exited but is not yet reaped (`WNOWAIT` leaves it a
/// zombie for the `try_wait` that follows). `None` if waitid failed.
#[cfg(unix)]
fn exited_unreaped(pid: u32) -> Option<bool> {
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    let rc = unsafe {
        libc::waitid(
            libc::P_PID,
            pid as libc::id_t,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    // WNOHANG with nothing to report returns 0 and leaves si_pid zero.
    (rc == 0).then(|| unsafe { info.si_pid() } != 0)
}

/// Aggregate outcome of `run_checks`: lint + tests combined.
#[derive(Debug)]
pub enum CheckOutcome {
    /// Both commands passed (or were not configured and had no effect).
    Pass,
    /// At least one command failed. `test_stderr` is `None` when lint
    /// failed and tests were skipped (fast-fail).
    Fail {
        lint_stderr: Option<String>,
        test_stderr: Option<String>,
    },
    /// Neither lint nor tests are configured/detected. Caller should
    /// treat this as a silent pass.
    NoRunners,
    /// Something prevented the checks from running (bad command string,
    /// spawn error, etc.). Caller should log and continue.
    Skipped { reason: String },
}

/// Run the lint command (if any), then the test command (if any), and
/// return a combined `CheckOutcome`. If lint fails, tests are skipped
/// (fast-fail) — `test_stderr` will be `None` in that case.
pub fn run_checks(
    cwd: &Path,
    lint_cmd: Option<&str>,
    test_cmd: Option<&str>,
    timeout_secs: u64,
    cancel: &AtomicBool,
    sandboxed: bool,
) -> CheckOutcome {
    if lint_cmd.is_none() && test_cmd.is_none() {
        return CheckOutcome::NoRunners;
    }

    let mut lint_stderr: Option<String> = None;
    let mut lint_failed = false;

    if let Some(cmd) = lint_cmd {
        match run_command(cwd, cmd, timeout_secs, cancel, sandboxed) {
            CommandResult::Pass => {}
            CommandResult::Fail { stderr } => {
                lint_failed = true;
                lint_stderr = Some(stderr);
            }
            CommandResult::Timeout => {
                lint_failed = true;
                lint_stderr = Some(format!("(lint timed out after {timeout_secs}s)"));
            }
            CommandResult::Skipped { reason } => {
                return CheckOutcome::Skipped { reason };
            }
        }
    }

    // Fast-fail: skip tests if lint failed.
    if lint_failed {
        return CheckOutcome::Fail {
            lint_stderr,
            test_stderr: None,
        };
    }

    let mut test_stderr: Option<String> = None;
    let mut test_failed = false;

    if let Some(cmd) = test_cmd {
        match run_command(cwd, cmd, timeout_secs, cancel, sandboxed) {
            CommandResult::Pass => {}
            CommandResult::Fail { stderr } => {
                test_failed = true;
                test_stderr = Some(stderr);
            }
            CommandResult::Timeout => {
                test_failed = true;
                test_stderr = Some(format!("(tests timed out after {timeout_secs}s)"));
            }
            CommandResult::Skipped { reason } => {
                return CheckOutcome::Skipped { reason };
            }
        }
    }

    if test_failed {
        CheckOutcome::Fail {
            lint_stderr,
            test_stderr,
        }
    } else {
        CheckOutcome::Pass
    }
}

/// Maximum stderr bytes to include per section in the retry feedback message.
/// Keeps context cost predictable across retries.
pub const MAX_FEEDBACK_SECTION_BYTES: usize = 2048;

/// Build the synthetic user-text message sent to the model after a
/// failed lint/test round. Trims each stderr section to
/// `MAX_FEEDBACK_SECTION_BYTES` and appends an explicit anti-cheat
/// clause so the model does not converge on `#[allow(...)]` /
/// `eslint-disable` / etc. `lsp` is the language-server section, if any
/// new errors were reported; when lint and tests did not fail it is the
/// only section.
pub fn format_feedback_message(
    lint_cmd: Option<&str>,
    test_cmd: Option<&str>,
    lint_stderr: Option<&str>,
    test_stderr: Option<&str>,
    lsp: Option<&str>,
) -> String {
    let lint_cmd_str = lint_cmd.unwrap_or("(none)");
    let test_cmd_str = test_cmd.unwrap_or("(none)");

    let lint_body = match lint_stderr {
        Some(s) => trim_section(s),
        None => "(no output)".to_string(),
    };
    let test_body = match test_stderr {
        Some(s) => trim_section(s),
        None if lint_stderr.is_some() => "(skipped: lint failed)".to_string(),
        None => "(no output)".to_string(),
    };

    let mut sections = Vec::new();
    if lint_stderr.is_some() || test_stderr.is_some() || lsp.is_none() {
        sections.push(format!("## Lint ({lint_cmd_str})\n{lint_body}"));
        sections.push(format!("## Tests ({test_cmd_str})\n{test_body}"));
    }
    sections.extend(lsp.map(str::to_string));

    format!(
        "Your last edits failed automated checks. Fix the issues below.\n\
         \n\
         {}\n\
         \n\
         Make the minimum edits required to make every check pass. Do not disable \
         lints, skip tests, or add `#[allow(...)]` / `# type: ignore` / \
         `eslint-disable` / `//nolint` unless the original code had them. \
         If a test assertion is genuinely wrong, explain why before changing it.",
        sections.join("\n\n")
    )
}

/// Output that says the check could not run in a namespace sandbox (no
/// network, a tool or path outside it), as opposed to code that is wrong.
/// Lower-case substrings matched against lower-cased output.
const SANDBOX_ENVIRONMENT_ERRORS: [&str; 13] = [
    // cargo with an empty or unreachable registry
    "failed to download",
    "failed to get `",
    "failed to load source for dependency",
    "failed to update registry",
    // name resolution and sockets (curl, git, go, npm, pip)
    "could not resolve host",
    "couldn't resolve host",
    "temporary failure in name resolution",
    "network is unreachable",
    "dial tcp",
    "eai_again",
    "enotfound",
    "failed to establish a new connection",
    // a path the sandbox does not expose (a missing program is matched by
    // name in `sandbox_environment_failure`)
    "read-only file system",
];

/// The status to show instead of a retry when a check failed inside a
/// namespace sandbox (bwrap, firejail) because of the sandbox itself. The
/// model would otherwise be told its edit broke the build and spend every
/// retry chasing an error no edit can fix.
///
/// `outputs` pairs each check's plain (unwrapped) command with its output. A
/// "not found" line counts only when it names that command's own program:
/// `do_bild: command not found` from a script the model edited is the
/// model's bug, while `ruff: command not found` means the sandbox hid ruff.
fn sandbox_environment_failure(
    containment: &Containment,
    outputs: &[(Option<&str>, &Option<String>)],
) -> Option<String> {
    let mode = containment
        .sandbox_mode
        .as_deref()
        .filter(|m| crate::sandbox::mode_enforces_isolation(m))?;
    let line = outputs.iter().find_map(|(cmd, out)| {
        let missing: Vec<String> = cmd
            .and_then(|c| c.split_whitespace().next())
            .and_then(|p| Path::new(p).file_name())
            .map(|p| p.to_string_lossy().to_ascii_lowercase())
            .map(|p| vec![format!("{p}: command not found"), format!("{p}: not found")])
            .unwrap_or_default();
        out.as_deref()?.lines().find(|line| {
            let line = line.to_ascii_lowercase();
            SANDBOX_ENVIRONMENT_ERRORS.iter().any(|e| line.contains(e))
                || missing.iter().any(|m| line.contains(m.as_str()))
        })
    })?;
    let line: String = line.trim().chars().take(200).collect();
    Some(format!(
        "[auto-fix] skipped: the check could not run inside the {mode} sandbox \
         (`{line}`), so it says nothing about the edit. Allow network with \
         sandboxAllowNetwork, or set autoFixLoop.lintCommand / testCommand."
    ))
}

fn trim_section(s: &str) -> String {
    if s.len() <= MAX_FEEDBACK_SECTION_BYTES {
        return s.to_string();
    }
    // Trim from the start so the tail of the stderr (usually the most
    // actionable part) is preserved.
    let start = s.len() - MAX_FEEDBACK_SECTION_BYTES;
    // Back up to the nearest char boundary so we don't slice inside a codepoint.
    let mut start = start;
    while start < s.len() && !s.is_char_boundary(start) {
        start += 1;
    }
    format!("... (output trimmed)\n{}", &s[start..])
}

/// The decision returned by `run_auto_fix_check` — tells the TUI turn
/// loop what to do next.
#[derive(Debug)]
pub enum AutoFixAction {
    /// Trigger rules said skip, no runners detected, or checks passed.
    /// `status` is a short human-readable description for the
    /// SystemMessage stream (or `None` for silent skip).
    Continue { status: Option<String> },
    /// Checks failed and we have retries left. Caller should append
    /// `Message { role: User, content: [Text { feedback }] }` to the
    /// conversation and re-enter the agentic loop. `status` is the
    /// SystemMessage to emit before the retry.
    Retry { feedback: String, status: String },
    /// Checks failed and we're at the retry cap. Caller should emit
    /// `status` as a SystemMessage and end the turn with a `Done`
    /// event preserving the partial work.
    GiveUp { status: String },
    /// A check would have run but the project is not trusted, so nothing
    /// ran. Caller shows `UNTRUSTED_NOTICE` once per session and continues.
    Untrusted,
}

/// Run lint + tests, and ask language servers about the edited files, then
/// decide what the TUI turn loop should do next.
///
/// `autonomy_mode` is the session's current `/autonomy` mode.
/// `retries_used` is the number of retries *already consumed* by this
/// user-prompt turn (so the first call passes `0`). Nothing runs unless
/// `containment.trusted`; what does run goes through its sandbox. `lsp`
/// gathers the language-server diagnostics on its own thread while lint
/// and tests run.
pub fn run_auto_fix_check(
    cwd: &Path,
    config: &AutoFixConfig,
    autonomy_mode: Autonomy,
    retries_used: u32,
    containment: &Containment,
    cancel: &AtomicBool,
    lsp: Option<&LspDiagnostics>,
) -> AutoFixAction {
    if !should_trigger(config, autonomy_mode) {
        return AutoFixAction::Continue { status: None };
    }
    let lsp = lsp.filter(|l| config.lsp.enabled && l.would_start());

    // Before detection: even the clippy probe below runs a binary in the
    // project (rustup honours its `rust-toolchain.toml`). Only file checks
    // decide whether there was anything to skip. Language servers run
    // project code too (build scripts, proc macros, plugins).
    if !containment.trusted {
        let would_run = detect_lint_command(cwd, &config.lint_command).is_some()
            || detect_test_command(cwd, &config.test_command).is_some()
            || lsp.is_some();
        return if would_run {
            AutoFixAction::Untrusted
        } else {
            AutoFixAction::Continue { status: None }
        };
    }

    // Overrides run as given, so a script the model broke still fails. An
    // auto-detected runner that is not installed here is skipped instead:
    // `sh: ruff: not found` is not something the model can fix by editing.
    let path = std::env::var_os("PATH");
    let runnable = |cmd| {
        runnable_detected(
            cwd,
            cmd,
            path.as_deref(),
            containment,
            config.timeout_secs,
            cancel,
        )
    };
    let lint_cmd = match &config.lint_command {
        Some(cmd) => Some(cmd.clone()),
        None => detect_lint_command(cwd, &None).and_then(runnable),
    };
    let test_cmd = match &config.test_command {
        Some(cmd) => Some(cmd.clone()),
        None => detect_test_command(cwd, &None).and_then(runnable),
    };

    if lint_cmd.is_none() && test_cmd.is_none() && lsp.is_none() {
        return AutoFixAction::Continue { status: None };
    }

    // The feedback names the plain commands; the shell gets the wrapped ones.
    let wrap = |cmd: &Option<String>| cmd.as_deref().map(|c| containment.wrap(c, cwd)).transpose();
    let (lint_run, test_run) = match (wrap(&lint_cmd), wrap(&test_cmd)) {
        (Ok(lint), Ok(test)) => (lint, test),
        (Err(reason), _) | (_, Err(reason)) => {
            return AutoFixAction::Continue {
                status: Some(format!("[auto-fix] skipped: {reason}")),
            };
        }
    };

    let (outcome, lsp) = std::thread::scope(|s| {
        let lsp = lsp.map(|l| {
            s.spawn(|| {
                l.runtime
                    .block_on(l.collect(&config.lsp, containment, cancel))
            })
        });
        let outcome = run_checks(
            cwd,
            lint_run.as_deref(),
            test_run.as_deref(),
            config.timeout_secs,
            cancel,
            containment.sandbox_mode.is_some(),
        );
        let lsp = lsp.and_then(|h| h.join().ok()).unwrap_or_default();
        (outcome, lsp)
    });

    // A language server that reported counts as a check that ran.
    let passed = match outcome {
        CheckOutcome::Pass => true,
        CheckOutcome::NoRunners => lsp.checked > 0,
        _ => false,
    };
    let mut notes = Vec::new();
    let (lint_stderr, test_stderr) = match outcome {
        CheckOutcome::Pass | CheckOutcome::NoRunners => (None, None),
        CheckOutcome::Skipped { reason } => {
            notes.push(format!("[auto-fix] skipped: {reason}"));
            (None, None)
        }
        CheckOutcome::Fail {
            lint_stderr,
            test_stderr,
        } => match sandbox_environment_failure(
            containment,
            &[
                (lint_cmd.as_deref(), &lint_stderr),
                (test_cmd.as_deref(), &test_stderr),
            ],
        ) {
            Some(status) => {
                notes.push(status);
                (None, None)
            }
            None => (lint_stderr, test_stderr),
        },
    };
    notes.extend(lsp.notes.iter().cloned());
    let lsp_section = lsp.section();
    let with_notes = |status: String| {
        std::iter::once(status)
            .chain(notes.iter().cloned())
            .collect::<Vec<_>>()
            .join("\n")
    };

    if lint_stderr.is_none() && test_stderr.is_none() && lsp_section.is_none() {
        let status = if passed {
            Some(with_notes("[auto-fix] checks passed".to_string()))
        } else {
            (!notes.is_empty()).then(|| notes.join("\n"))
        };
        return AutoFixAction::Continue { status };
    }

    if retries_used >= config.max_retries {
        let mut status = format!(
            "[auto-fix] cap reached ({0}/{0}) — giving up, working tree left as-is",
            config.max_retries
        );
        if lint_stderr.is_some() || test_stderr.is_some() {
            let lint_tail = lint_stderr
                .as_deref()
                .map(trim_section)
                .unwrap_or_else(|| "(no output)".to_string());
            let test_tail = test_stderr
                .as_deref()
                .map(trim_section)
                .unwrap_or_else(|| "(skipped: lint failed)".to_string());
            status.push_str(&format!(
                "\nFinal lint output:\n{lint_tail}\nFinal test output:\n{test_tail}"
            ));
        }
        if !lsp.errors.is_empty() {
            status.push_str(&format!(
                "\nFinal language server errors:\n{}",
                lsp.errors.join("\n")
            ));
        }
        AutoFixAction::GiveUp {
            status: with_notes(status),
        }
    } else {
        let feedback = format_feedback_message(
            lint_cmd.as_deref(),
            test_cmd.as_deref(),
            lint_stderr.as_deref(),
            test_stderr.as_deref(),
            lsp_section.as_deref(),
        );
        AutoFixAction::Retry {
            feedback,
            status: with_notes(format!(
                "[auto-fix] checks failed — retry {}/{}",
                retries_used + 1,
                config.max_retries,
            )),
        }
    }
}

// ── Language-server diagnostics ───────────────────────────────────────────────

/// Lines of language-server errors fed back per check.
pub const MAX_LSP_LINES: usize = 30;

/// Files above this size keep no pre-edit text (every error in them counts
/// as new unless a running server had diagnostics for them already).
const MAX_BASELINE_BYTES: u64 = 4 * 1024 * 1024;

/// A file as it was before the turn's first edit to it, so errors it
/// already had are not reported as the edit's.
#[derive(Debug, Clone, Default)]
pub struct LspBaseline {
    /// Its text: empty for a file the edit creates, `None` if unreadable.
    pub content: Option<String>,
    /// What a language server that was already running had published for it.
    pub diagnostics: Option<Vec<Value>>,
}

/// Record `path` (absolute) before an edit: its text, and the diagnostics a
/// running server (looked up on `search`, a PATH) already has for it.
/// Starts nothing, and reads nothing for a file no server would check.
pub async fn capture_lsp_baseline(
    pool: &LspPool,
    root: &Path,
    path: &Path,
    search: Option<&std::ffi::OsStr>,
) -> LspBaseline {
    if !in_root(root, path) {
        return LspBaseline::default();
    }
    let Some((command, args, _)) = crate::tools::lsp::installed_server(path, search) else {
        return LspBaseline::default();
    };
    let content = match tokio::fs::metadata(path).await {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some(String::new()),
        Ok(m) if m.len() <= MAX_BASELINE_BYTES => tokio::fs::read_to_string(path).await.ok(),
        _ => None,
    };
    let diagnostics = pool
        .running(command, &args, root)
        .await
        .and_then(|c| c.diagnostics(path));
    LspBaseline {
        content,
        diagnostics,
    }
}

/// Whether `path` is inside `root`, `..` resolved. Servers are started in
/// the trusted project and only see its files: one outside it would make
/// gopls or tsserver load that file's own module, in a folder never trusted.
fn in_root(root: &Path, path: &Path) -> bool {
    use crate::tools::file_read::clean_path;
    clean_path(path).starts_with(clean_path(root))
}

/// Language-server diagnostics for the files a round of edits wrote. The
/// servers are the LSP tool's (`pool`), started on first use.
pub struct LspDiagnostics {
    pub pool: LspPool,
    /// Where the servers run: the session's working directory.
    pub root: PathBuf,
    /// The edited files (absolute), with their state before the turn's edits.
    pub files: Vec<(PathBuf, Option<LspBaseline>)>,
    /// The PATH servers are looked up on.
    pub search_path: Option<std::ffi::OsString>,
    /// Runs the server I/O for the blocking check.
    pub runtime: tokio::runtime::Handle,
}

/// What the language servers said about one round of edits.
#[derive(Debug, Default)]
pub struct LspOutcome {
    /// Files a server reported on.
    pub checked: usize,
    /// The servers that reported.
    pub servers: Vec<&'static str>,
    /// New problems, `file:line:col message`, at most `MAX_LSP_LINES`.
    pub errors: Vec<String>,
    /// Status lines: a server that could not start or answer in time.
    pub notes: Vec<String>,
}

impl LspOutcome {
    /// The feedback section, when there is anything to fix.
    pub fn section(&self) -> Option<String> {
        (!self.errors.is_empty()).then(|| {
            format!(
                "## Language server ({})\n{}",
                self.servers.join(", "),
                self.errors.join("\n")
            )
        })
    }
}

/// The edited files one server covers.
struct ServerFiles<'a> {
    command: &'static str,
    args: Vec<String>,
    exe: PathBuf,
    files: Vec<&'a (PathBuf, Option<LspBaseline>)>,
}

/// What one server said: per file, its new problems (`None`: no report).
#[derive(Default)]
struct ServerReport {
    files: Vec<(PathBuf, Option<Vec<Diag>>)>,
    note: Option<String>,
}

fn seconds(d: Duration) -> String {
    format!("{}s", d.as_secs_f64())
}

impl LspDiagnostics {
    /// The installed servers for the edited files inside the project,
    /// skipping any the check has given up on this session.
    fn servers(&self) -> Vec<ServerFiles<'_>> {
        let mut groups: Vec<ServerFiles> = Vec::new();
        for file in self.files.iter().filter(|f| in_root(&self.root, &f.0)) {
            let Some((command, args, exe)) =
                crate::tools::lsp::installed_server(&file.0, self.search_path.as_deref())
            else {
                continue;
            };
            if self.pool.gave_up(command, &args, &self.root) {
                continue;
            }
            match groups
                .iter_mut()
                .find(|g| g.command == command && g.args == args)
            {
                Some(g) => g.files.push(file),
                None => groups.push(ServerFiles {
                    command,
                    args,
                    exe,
                    files: vec![file],
                }),
            }
        }
        groups
    }

    /// Whether checking would start (or use) a language server.
    pub fn would_start(&self) -> bool {
        !self.servers().is_empty()
    }

    /// Sync the edited files to their servers, wait for what they publish
    /// (at most `config.timeout` in all), and keep the new problems.
    pub async fn collect(
        &self,
        config: &LspDiagnosticsConfig,
        containment: &Containment,
        cancel: &AtomicBool,
    ) -> LspOutcome {
        let deadline = tokio::time::Instant::now() + config.timeout;
        let groups = self.servers();
        let reports = futures_util::future::join_all(
            groups
                .iter()
                .map(|g| self.ask(g, config, containment, deadline, cancel)),
        )
        .await;

        let mut out = LspOutcome::default();
        let mut lines = Vec::new();
        for (group, report) in groups.iter().zip(reports) {
            out.notes.extend(report.note);
            let mut reported = false;
            for (path, problems) in report.files {
                let Some(problems) = problems else { continue };
                reported = true;
                out.checked += 1;
                let shown = path
                    .strip_prefix(&self.root)
                    .unwrap_or(&path)
                    .display()
                    .to_string();
                lines.extend(problems.iter().map(|d| d.render(&shown)));
            }
            if reported {
                out.servers.push(group.command);
            }
        }
        if lines.len() > MAX_LSP_LINES {
            let more = lines.len() - (MAX_LSP_LINES - 1);
            lines.truncate(MAX_LSP_LINES - 1);
            lines.push(format!("... and {more} more"));
        }
        out.errors = lines;
        out
    }

    /// One server: start it if needed (sandboxed like the lint and test
    /// commands), send the files, wait. A server that cannot start, exits,
    /// or does not answer `initialize` or take the files by `deadline` is
    /// given up on for the session.
    async fn ask(
        &self,
        group: &ServerFiles<'_>,
        config: &LspDiagnosticsConfig,
        containment: &Containment,
        deadline: tokio::time::Instant,
        cancel: &AtomicBool,
    ) -> ServerReport {
        let command = group.command;
        let give_up = |why: String| {
            self.pool.give_up(command, &group.args, &self.root);
            ServerReport {
                files: Vec::new(),
                note: Some(format!(
                    "[auto-fix] {command} {why}; language-server diagnostics from it are \
                     off for this session"
                )),
            }
        };

        let launch = match Launch::contained(&group.exe, &group.args, containment, &self.root) {
            Ok(launch) => launch,
            Err(reason) => return give_up(format!("was not started: {reason}")),
        };
        let start = tokio::time::timeout_at(
            deadline,
            self.pool
                .client_for(command, &group.args, &self.root, &launch),
        );
        // Esc and quit set `cancel`. Dropping the start kills the
        // half-started server; the next check starts it again. The cap
        // covers only this server's own start (and a start of the same
        // server already under way): other servers start alongside it.
        let cancelled = async {
            while !cancel.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        };
        let client = tokio::select! {
            r = start => match r {
                Ok(Ok(c)) => c,
                Ok(Err(e)) => return give_up(format!("did not start: {e}")),
                Err(_) => {
                    return give_up(format!("did not answer within {}", seconds(config.timeout)));
                }
            },
            () = cancelled => return ServerReport::default(),
        };

        // Other files the server has open may have changed since it was
        // last sent them (`/undo`, a Bash edit): its view of them shapes
        // the edited files' diagnostics.
        let edited: Vec<PathBuf> = group.files.iter().map(|f| f.0.clone()).collect();
        match tokio::time::timeout_at(deadline, client.refresh_open_documents(&edited)).await {
            Ok(Ok(())) => {}
            Ok(Err(_)) if !client.is_dead() => {}
            Ok(Err(_)) => return give_up("exited".into()),
            Err(_) => {
                client.mark_dead();
                return give_up(format!(
                    "did not take the edited files within {}",
                    seconds(config.timeout)
                ));
            }
        }

        let mut synced = Vec::new();
        let mut files = Vec::new();
        for (path, baseline) in &group.files {
            if cancel.load(Ordering::SeqCst) {
                return ServerReport::default();
            }
            // A server that stopped reading its input blocks this write once
            // the text outgrows the pipe buffer: the cap covers it too.
            match tokio::time::timeout_at(deadline, client.sync_document(path)).await {
                Ok(Ok(s)) => {
                    synced.push(s);
                    files.push((path, baseline));
                }
                // A file deleted since is simply not checked.
                Ok(Err(_)) if !client.is_dead() => {}
                Ok(Err(_)) => return give_up("exited".into()),
                Err(_) => {
                    client.mark_dead();
                    return give_up(format!(
                        "did not take the edited files within {}",
                        seconds(config.timeout)
                    ));
                }
            }
        }
        if synced.is_empty() {
            return ServerReport::default();
        }
        let published = client
            .wait_for_diagnostics(&synced, config.settle, deadline, cancel)
            .await;
        if cancel.load(Ordering::SeqCst) {
            return ServerReport::default();
        }
        if client.is_dead() {
            return give_up("exited".into());
        }

        // A live server that has said nothing about a file yet (a slow cold
        // start, or one that publishes only on change and found the file
        // clean) is kept: the next check asks it again.
        let mut report = ServerReport::default();
        let mut silent = 0;
        for ((path, baseline), published) in files.into_iter().zip(published) {
            let problems = published.map(|r| {
                // A set the server published for an older text is read
                // against that text: its line numbers belong to it.
                let text = match r.stale_text {
                    Some(stale) => Some(stale),
                    None => std::fs::read_to_string(path).ok(),
                };
                new_problems(
                    &r.diagnostics,
                    baseline.as_ref(),
                    text.as_deref(),
                    config.warnings,
                )
            });
            silent += usize::from(problems.is_none());
            report.files.push((path.clone(), problems));
        }
        if silent > 0 {
            report.note = Some(format!(
                "[auto-fix] {command} reported nothing on {silent} edited file(s) within {}",
                seconds(config.timeout)
            ));
        }
        report
    }
}

/// One diagnostic, positions 0-based as LSP sends them.
#[derive(Debug, Clone, PartialEq)]
struct Diag {
    line: usize,
    col: usize,
    end_line: usize,
    warning: bool,
    message: String,
    code: Option<String>,
}

impl Diag {
    /// Errors, and warnings when `warnings`; a diagnostic with no severity
    /// is an error, as most editors show it.
    fn parse(v: &Value, warnings: bool) -> Option<Diag> {
        let severity = v.get("severity").and_then(Value::as_u64).unwrap_or(1);
        if !(severity == 1 || warnings && severity == 2) {
            return None;
        }
        let start = &v["range"]["start"];
        let pos = |p: &Value, k: &str| p.get(k).and_then(Value::as_u64).unwrap_or(0) as usize;
        let line = pos(start, "line");
        Some(Diag {
            line,
            col: pos(start, "character"),
            end_line: pos(&v["range"]["end"], "line").max(line),
            warning: severity == 2,
            message: v.get("message").and_then(Value::as_str)?.to_string(),
            code: match v.get("code") {
                Some(Value::String(c)) => Some(c.clone()),
                Some(Value::Number(n)) => Some(n.to_string()),
                _ => None,
            },
        })
    }

    /// `file:line:col message`, 1-based like Read and grep, on one line.
    fn render(&self, file: &str) -> String {
        let message: String = self
            .message
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .chars()
            .take(200)
            .collect();
        let kind = if self.warning { "warning: " } else { "" };
        format!("{file}:{}:{} {kind}{message}", self.line + 1, self.col + 1)
    }

    /// What identifies the same problem before and after an edit that
    /// moved it: its message, code, and the text of its line.
    fn key(&self, text: Option<&str>) -> (String, Option<String>, Option<String>) {
        let line = text
            .and_then(|t| t.lines().nth(self.line))
            .map(|l| l.trim().to_string());
        (self.message.clone(), self.code.clone(), line)
    }
}

/// The diagnostics in `published` that the turn's edits introduced:
/// measured against what a server had already reported for the file when
/// there was one, else only those on lines the edits changed.
fn new_problems(
    published: &[Value],
    baseline: Option<&LspBaseline>,
    after: Option<&str>,
    warnings: bool,
) -> Vec<Diag> {
    let mut problems: Vec<Diag> = published
        .iter()
        .filter_map(|v| Diag::parse(v, warnings))
        .collect();
    problems.sort_by_key(|d| (d.line, d.col));
    let Some(baseline) = baseline else {
        return problems;
    };
    if let Some(before) = &baseline.diagnostics {
        let mut known: HashMap<_, usize> = HashMap::new();
        for d in before.iter().filter_map(|v| Diag::parse(v, warnings)) {
            *known.entry(d.key(baseline.content.as_deref())).or_default() += 1;
        }
        problems.retain(|d| match known.get_mut(&d.key(after)) {
            Some(n) if *n > 0 => {
                *n -= 1;
                false
            }
            _ => true,
        });
        return problems;
    }
    if let (Some(before), Some(after)) = (&baseline.content, after) {
        let changed = changed_lines(before, after);
        // A problem past the last line (a missing `}` at EOF) is on it.
        let last = changed.len().saturating_sub(1);
        problems.retain(|d| {
            (d.line.min(last)..=d.end_line.min(last)).any(|l| changed.get(l) == Some(&true))
        });
    }
    problems
}

/// For each line of `after`, whether the edit from `before` changed it.
/// Lines outside the common head and tail count as changed unless the old
/// middle had the same line (a line that only moved). At a pure deletion
/// the lines either side of it count.
fn changed_lines(before: &str, after: &str) -> Vec<bool> {
    let old: Vec<&str> = before.lines().collect();
    let new: Vec<&str> = after.lines().collect();
    let head = old.iter().zip(&new).take_while(|(a, b)| a == b).count();
    let tail = old[head..]
        .iter()
        .rev()
        .zip(new[head..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    let mut changed = vec![false; new.len()];
    let (old_mid, new_mid) = (head..old.len() - tail, head..new.len() - tail);
    if new_mid.is_empty() {
        if !old_mid.is_empty() {
            for l in [head.wrapping_sub(1), head] {
                if let Some(c) = changed.get_mut(l) {
                    *c = true;
                }
            }
        }
        return changed;
    }
    let mut unmatched: HashMap<&str, usize> = HashMap::new();
    for l in &old[old_mid] {
        *unmatched.entry(*l).or_default() += 1;
    }
    for i in new_mid {
        match unmatched.get_mut(new[i]) {
            Some(n) if *n > 0 => *n -= 1,
            _ => changed[i] = true,
        }
    }
    changed
}

#[cfg(test)]
mod tests {
    static NOT_CANCELLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

    /// A trusted project with no sandbox: commands run as given.
    fn trusted() -> super::Containment {
        super::Containment {
            trusted: true,
            ..Default::default()
        }
    }

    // ── Auto-detected runners must be runnable ───────────────────────────────

    /// `runnable_detected` in a trusted project with no sandbox.
    fn runnable(
        cwd: &std::path::Path,
        cmd: String,
        path: Option<&std::ffi::OsStr>,
    ) -> Option<String> {
        super::runnable_detected(cwd, cmd, path, &trusted(), 10, &NOT_CANCELLED)
    }

    #[cfg(unix)]
    fn fake_bin(dir: &std::path::Path, name: &str, exit: i32) {
        use std::os::unix::fs::PermissionsExt;
        let p = dir.join(name);
        std::fs::write(&p, format!("#!/bin/sh\nexit {exit}\n")).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// The clippy probe runs the resolved cargo path through cmd.exe, which
    /// split an unquoted `C:\Users\John Doe\...` at the space and dropped
    /// clippy for every Rust project on such a machine.
    #[cfg(windows)]
    #[test]
    fn windows_clippy_probe_survives_a_space_in_the_cargo_path() {
        let parent = tempfile::tempdir().unwrap();
        let bin = parent.path().join("John Doe");
        std::fs::create_dir(&bin).unwrap();
        std::fs::write(bin.join("cargo.cmd"), "@exit /b 0\r\n").unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let cmd = "cargo clippy --all-targets -- -D warnings".to_string();
        assert_eq!(
            runnable(cwd.path(), cmd.clone(), Some(bin.as_os_str())),
            Some(cmd)
        );
    }

    /// Windows ran checks by splitting on spaces and spawning the first word,
    /// which cannot launch `.cmd` shims (npm, npx) or honour quotes and `&&`,
    /// and detection looked only for `npm`/`npm.exe`, never `npm.cmd`.
    #[cfg(windows)]
    #[test]
    fn windows_checks_run_through_cmd_and_find_cmd_shims() {
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let td = tempfile::tempdir().unwrap();
        std::fs::write(td.path().join("shim.cmd"), "@exit /b 0\r\n").unwrap();
        std::fs::write(td.path().join("in.txt"), "xa by\r\n").unwrap();
        let run = |cmd: &str| super::run_command(td.path(), cmd, 60, &cancel, false);
        assert!(
            matches!(run("shim"), super::CommandResult::Pass),
            "{:?}",
            run("shim")
        );
        assert!(matches!(
            run(r#"shim && findstr /c:"zzz" in.txt"#),
            super::CommandResult::Fail { .. }
        ));
        let quoted = r#"findstr /c:"a b" in.txt"#;
        assert!(
            matches!(run(quoted), super::CommandResult::Pass),
            "{:?}",
            run(quoted)
        );

        let bin = tempfile::tempdir().unwrap();
        std::fs::write(bin.path().join("npm.cmd"), "@exit /b 0\r\n").unwrap();
        assert!(super::find_on_path("npm", Some(bin.path().as_os_str())).is_some());
        assert!(super::find_on_path("npx", Some(bin.path().as_os_str())).is_none());
    }

    /// Esc only aborted the async task; the lint/test process it was
    /// waiting on ran on to completion or its timeout.
    #[cfg(unix)]
    #[test]
    fn cancelling_kills_a_running_check() {
        let td = tempfile::tempdir().unwrap();
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = cancel.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(300));
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
        });
        let started = std::time::Instant::now();
        let r = super::run_command(td.path(), "sleep 30", 0, &cancel, false);
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
        assert!(matches!(r, super::CommandResult::Skipped { .. }), "{r:?}");
        // A cancelled lint never goes on to start the tests.
        let outcome = super::run_checks(
            td.path(),
            Some("true"),
            Some("touch ran"),
            0,
            &cancel,
            false,
        );
        assert!(matches!(outcome, super::CheckOutcome::Skipped { .. }));
        assert!(!td.path().join("ran").exists());
    }

    #[cfg(unix)]
    #[test]
    fn python_repo_without_ruff_or_pytest_has_no_runner() {
        let proj = tempfile::tempdir().unwrap();
        let empty_path = tempfile::tempdir().unwrap();
        std::fs::write(proj.path().join("pyproject.toml"), "[project]\n").unwrap();
        let path = empty_path.path().as_os_str();
        for cmd in ["ruff check .", "pytest"] {
            assert_eq!(runnable(proj.path(), cmd.into(), Some(path)), None);
        }
    }

    #[cfg(unix)]
    #[test]
    fn project_venv_tools_are_used_when_present() {
        let proj = tempfile::tempdir().unwrap();
        let empty_path = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(proj.path().join(".venv/bin")).unwrap();
        fake_bin(&proj.path().join(".venv/bin"), "ruff", 0);
        assert_eq!(
            runnable(
                proj.path(),
                "ruff check .".into(),
                Some(empty_path.path().as_os_str())
            ),
            Some(".venv/bin/ruff check .".to_string())
        );
    }

    #[cfg(unix)]
    #[test]
    fn cargo_without_clippy_skips_lint_but_keeps_tests() {
        let proj = tempfile::tempdir().unwrap();
        let bin = tempfile::tempdir().unwrap();
        fake_bin(bin.path(), "cargo", 101); // `cargo clippy --version` fails
        let path = Some(bin.path().as_os_str());
        let lint = "cargo clippy --all-targets -- -D warnings";
        assert_eq!(runnable(proj.path(), lint.into(), path), None);
        assert_eq!(
            runnable(proj.path(), "cargo test".into(), path),
            Some("cargo test".to_string())
        );
    }

    /// The probe runs a binary the project can choose (`rust-toolchain.toml`
    /// `path`), so it is contained like the checks: a sandbox that refuses
    /// it means no clippy, never a bare run.
    #[cfg(unix)]
    #[test]
    fn the_clippy_probe_goes_through_the_sandbox() {
        use std::os::unix::fs::PermissionsExt;
        let proj = tempfile::tempdir().unwrap();
        let bin = tempfile::tempdir().unwrap();
        let marker = proj.path().join("probed");
        let cargo = bin.path().join("cargo");
        std::fs::write(
            &cargo,
            format!("#!/bin/sh\ntouch '{}'\nexit 0\n", marker.display()),
        )
        .unwrap();
        std::fs::set_permissions(&cargo, std::fs::Permissions::from_mode(0o755)).unwrap();
        let path = Some(bin.path().as_os_str());
        let lint = "cargo clippy --all-targets -- -D warnings";

        let refused = super::Containment {
            trusted: true,
            sandbox_mode: Some("bogus".to_string()),
            sandbox_allow_network: false,
        };
        assert_eq!(
            super::runnable_detected(proj.path(), lint.into(), path, &refused, 10, &NOT_CANCELLED),
            None
        );
        assert!(!marker.exists(), "the probe ran outside the sandbox");

        assert_eq!(
            runnable(proj.path(), lint.into(), path),
            Some(lint.to_string())
        );
        assert!(marker.exists());
    }

    #[cfg(unix)]
    #[test]
    fn npm_default_test_stub_and_unconfigured_eslint_are_skipped() {
        let proj = tempfile::tempdir().unwrap();
        let bin = tempfile::tempdir().unwrap();
        fake_bin(bin.path(), "npm", 0);
        fake_bin(bin.path(), "npx", 0);
        let path = Some(bin.path().as_os_str());
        std::fs::write(
            proj.path().join("package.json"),
            r#"{"scripts":{"test":"echo \"Error: no test specified\" && exit 1"}}"#,
        )
        .unwrap();
        assert_eq!(runnable(proj.path(), "npm test".into(), path), None);
        let eslint = "npx --no-install eslint .";
        assert_eq!(runnable(proj.path(), eslint.into(), path), None);

        std::fs::write(
            proj.path().join("package.json"),
            r#"{"scripts":{"test":"vitest run"}}"#,
        )
        .unwrap();
        std::fs::write(proj.path().join("eslint.config.js"), "export default [];").unwrap();
        std::fs::create_dir_all(proj.path().join("node_modules/.bin")).unwrap();
        fake_bin(&proj.path().join("node_modules/.bin"), "eslint", 0);
        assert_eq!(
            runnable(proj.path(), "npm test".into(), path),
            Some("npm test".to_string())
        );
        assert_eq!(
            runnable(proj.path(), eslint.into(), path),
            Some(eslint.to_string())
        );
    }

    /// An explicit override is never second-guessed: if the model deletes
    /// the script it names, that is a failure to report, not a skip.
    #[cfg(unix)]
    #[test]
    fn missing_override_command_still_fails() {
        let proj = tempfile::tempdir().unwrap();
        let cfg = super::AutoFixConfig {
            enabled: true,
            trigger: super::AutoFixTrigger::Always,
            lint_command: Some("./scripts/check.sh".into()),
            test_command: None,
            max_retries: 3,
            timeout_secs: 10,
            lsp: Default::default(),
        };
        let action = super::run_auto_fix_check(
            proj.path(),
            &cfg,
            crate::permissions::Autonomy::AutoEdit,
            0,
            &trusted(),
            &NOT_CANCELLED,
            None,
        );
        assert!(
            matches!(action, super::AutoFixAction::Retry { .. }),
            "{action:?}"
        );
    }

    /// More output than a pipe buffer used to deadlock until the timeout.
    #[cfg(unix)]
    #[test]
    fn large_output_does_not_deadlock_and_keeps_stdout() {
        let td = tempfile::TempDir::new().unwrap();
        let started = std::time::Instant::now();
        let r = run_command(
            td.path(),
            "echo build-noise >&2; head -c 200000 /dev/zero | tr '\\0' x; echo; echo 'test result: FAILED'; exit 1",
            30,
            &NOT_CANCELLED,
            false,
        );
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
        match r {
            CommandResult::Fail { stderr } => {
                assert!(stderr.starts_with("build-noise"));
                assert!(stderr.trim_end().ends_with("test result: FAILED"));
            }
            other => panic!("expected Fail, got {other:?}"),
        }
    }

    /// A check that prompts on /dev/tty must fail, not hang as a stopped
    /// background job: the shell leads its own session with no terminal.
    #[cfg(target_os = "linux")]
    #[test]
    fn commands_run_in_their_own_session() {
        let td = tempfile::TempDir::new().unwrap();
        assert!(matches!(
            run_command(
                td.path(),
                "read -r pid comm state ppid pgrp sid rest < /proc/$$/stat; \
                 test \"$sid\" = \"$$\" && test \"$pgrp\" = \"$$\"",
                10,
                &NOT_CANCELLED,
                false
            ),
            CommandResult::Pass
        ));
    }

    /// A job the command backgrounds holds the pipes open; the check used
    /// to wait for it however long it lived, past the timeout.
    #[cfg(unix)]
    #[test]
    fn leftover_background_jobs_do_not_hold_the_check() {
        let td = tempfile::TempDir::new().unwrap();
        let started = std::time::Instant::now();
        let r = run_command(
            td.path(),
            "sleep 30 & echo 'test failed'; exit 1",
            20,
            &NOT_CANCELLED,
            false,
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "{:?}",
            started.elapsed()
        );
        match r {
            CommandResult::Fail { stderr } => assert_eq!(stderr.trim(), "test failed"),
            other => panic!("expected Fail, got {other:?}"),
        }
    }

    /// A job that escaped the process group is out of reach of the kill;
    /// a short grace bounds the wait for its pipes, with or without a
    /// timeout, and the check's own exit status stands (it was reported as
    /// a Timeout, and with timeout 0 the wait never ended).
    #[cfg(unix)]
    #[test]
    fn a_job_outside_the_group_cannot_outlast_the_timeout() {
        if std::process::Command::new("setsid")
            .arg("true")
            .status()
            .is_err()
        {
            return;
        }
        let td = tempfile::TempDir::new().unwrap();
        // Exit only once the job has left the group: exiting first let the
        // group kill reach it before its setsid() under a loaded test run.
        for timeout in [2, 0] {
            let started = std::time::Instant::now();
            let _ = std::fs::remove_file(td.path().join("escaped"));
            let r = run_command(
                td.path(),
                "setsid sh -c ': > escaped; exec sleep 15' & \
                 while [ ! -e escaped ]; do sleep 0.05; done; exit 0",
                timeout,
                &NOT_CANCELLED,
                false,
            );
            assert!(
                started.elapsed() < std::time::Duration::from_secs(10),
                "{:?}",
                started.elapsed()
            );
            assert!(matches!(r, CommandResult::Pass), "timeout {timeout}: {r:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn commands_run_through_the_shell() {
        let td = tempfile::TempDir::new().unwrap();
        assert!(matches!(
            run_command(
                td.path(),
                "test \"a b\" = 'a b' && X=1 true",
                10,
                &NOT_CANCELLED,
                false
            ),
            CommandResult::Pass
        ));
    }

    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn detect_lint_command_cargo() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"x\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        assert_eq!(
            detect_lint_command(dir.path(), &None),
            Some("cargo clippy --all-targets -- -D warnings".to_string())
        );
    }

    #[test]
    fn detect_lint_command_npm() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("package.json"), "{}").unwrap();
        assert_eq!(
            detect_lint_command(dir.path(), &None),
            Some("npx --no-install eslint .".to_string())
        );
    }

    #[test]
    fn detect_lint_command_python_pyproject() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("pyproject.toml"),
            "[project]\nname = \"x\"\n",
        )
        .unwrap();
        assert_eq!(
            detect_lint_command(dir.path(), &None),
            Some("ruff check .".to_string())
        );
    }

    #[test]
    fn detect_lint_command_python_setuppy() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("setup.py"),
            "from setuptools import setup; setup()",
        )
        .unwrap();
        assert_eq!(
            detect_lint_command(dir.path(), &None),
            Some("ruff check .".to_string())
        );
    }

    #[test]
    fn detect_lint_command_go() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("go.mod"), "module x\n").unwrap();
        assert_eq!(
            detect_lint_command(dir.path(), &None),
            Some("go vet ./...".to_string())
        );
    }

    #[test]
    fn detect_lint_command_override_wins() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("Cargo.toml"), "").unwrap();
        assert_eq!(
            detect_lint_command(dir.path(), &Some("my-linter".to_string())),
            Some("my-linter".to_string())
        );
    }

    #[test]
    fn detect_lint_command_no_runner() {
        let dir = tempdir().unwrap();
        assert_eq!(detect_lint_command(dir.path(), &None), None);
    }

    #[test]
    fn run_checks_both_pass() {
        let dir = tempdir().unwrap();
        let outcome = run_checks(
            dir.path(),
            Some("true"), // lint: unix `true` exits 0
            Some("true"), // tests: same
            5,
            &NOT_CANCELLED,
            false,
        );
        assert!(matches!(outcome, CheckOutcome::Pass), "got {outcome:?}");
    }

    #[test]
    fn run_checks_lint_fail_skips_tests() {
        let dir = tempdir().unwrap();
        // lint `false` exits 1; tests command writes a sentinel file if run.
        let sentinel = dir.path().join("tests_ran");
        let sentinel_str = sentinel.display().to_string();
        let test_cmd = format!("sh -c 'touch {sentinel_str}'");
        let outcome = run_checks(
            dir.path(),
            Some("false"),
            Some(&test_cmd),
            5,
            &NOT_CANCELLED,
            false,
        );
        match outcome {
            CheckOutcome::Fail {
                lint_stderr: _,
                test_stderr,
            } => {
                assert!(
                    test_stderr.is_none(),
                    "tests must be skipped when lint fails"
                );
            }
            other => panic!("expected Fail, got {other:?}"),
        }
        assert!(
            !sentinel.exists(),
            "sentinel file should not exist — tests must not have run"
        );
    }

    #[test]
    fn run_checks_lint_pass_tests_fail() {
        let dir = tempdir().unwrap();
        let outcome = run_checks(
            dir.path(),
            Some("true"),
            Some("false"),
            5,
            &NOT_CANCELLED,
            false,
        );
        match outcome {
            CheckOutcome::Fail {
                lint_stderr,
                test_stderr,
            } => {
                assert!(lint_stderr.is_none());
                assert!(test_stderr.is_some());
            }
            other => panic!("expected Fail, got {other:?}"),
        }
    }

    #[test]
    fn run_checks_no_runners() {
        let dir = tempdir().unwrap();
        let outcome = run_checks(dir.path(), None, None, 5, &NOT_CANCELLED, false);
        assert!(
            matches!(outcome, CheckOutcome::NoRunners),
            "got {outcome:?}"
        );
    }

    #[test]
    fn run_checks_lint_only_pass() {
        let dir = tempdir().unwrap();
        let outcome = run_checks(dir.path(), Some("true"), None, 5, &NOT_CANCELLED, false);
        assert!(matches!(outcome, CheckOutcome::Pass), "got {outcome:?}");
    }

    #[test]
    fn run_checks_tests_only_fail() {
        let dir = tempdir().unwrap();
        let outcome = run_checks(dir.path(), None, Some("false"), 5, &NOT_CANCELLED, false);
        match outcome {
            CheckOutcome::Fail {
                lint_stderr,
                test_stderr,
            } => {
                assert!(lint_stderr.is_none());
                assert!(test_stderr.is_some());
            }
            other => panic!("expected Fail, got {other:?}"),
        }
    }

    #[test]
    fn format_feedback_message_both_failed() {
        let msg = format_feedback_message(
            Some("cargo clippy"),
            Some("cargo test"),
            Some("warning: unused variable `x`"),
            Some("test foo ... FAILED"),
            None,
        );
        assert!(msg.contains("Your last edits failed"));
        assert!(msg.contains("## Lint (cargo clippy)"));
        assert!(msg.contains("warning: unused variable"));
        assert!(msg.contains("## Tests (cargo test)"));
        assert!(msg.contains("test foo ... FAILED"));
        assert!(msg.contains("Do not disable lints"));
        assert!(msg.contains("#[allow"));
    }

    #[test]
    fn format_feedback_message_lint_only() {
        let msg = format_feedback_message(
            Some("cargo clippy"),
            Some("cargo test"),
            Some("warning: dead code"),
            None,
            None,
        );
        assert!(msg.contains("## Lint (cargo clippy)"));
        assert!(msg.contains("warning: dead code"));
        assert!(msg.contains("(skipped: lint failed)"));
    }

    #[test]
    fn format_feedback_message_tests_only() {
        let msg = format_feedback_message(
            Some("cargo clippy"),
            Some("cargo test"),
            None,
            Some("assertion failed: x == 1"),
            None,
        );
        assert!(msg.contains("(no output)"));
        assert!(msg.contains("assertion failed"));
    }

    #[test]
    fn format_feedback_message_truncates_lint() {
        let big = "x".repeat(5000);
        let msg = format_feedback_message(
            Some("cargo clippy"),
            Some("cargo test"),
            Some(&big),
            None,
            None,
        );
        // Should contain the truncation marker
        assert!(msg.contains("(output trimmed)"));
        // Should not contain all 5000 x's
        let x_count = msg.matches('x').count();
        assert!(
            x_count <= MAX_FEEDBACK_SECTION_BYTES + 10,
            "x_count = {x_count}"
        );
    }

    #[test]
    fn format_feedback_message_truncates_tests() {
        let big = "y".repeat(5000);
        let msg = format_feedback_message(
            Some("cargo clippy"),
            Some("cargo test"),
            Some("(lint passed)"),
            Some(&big),
            None,
        );
        assert!(msg.contains("(output trimmed)"));
        let y_count = msg.matches('y').count();
        assert!(y_count <= MAX_FEEDBACK_SECTION_BYTES + 10);
    }

    #[test]
    fn trim_section_respects_utf8_boundaries() {
        // 2049 bytes where the byte at index 1 starts a 3-byte codepoint ('€' = E2 82 AC)
        let s = format!("€{}", "a".repeat(MAX_FEEDBACK_SECTION_BYTES));
        // trim_section should not panic and should return valid UTF-8
        let out = trim_section(&s);
        assert!(out.is_char_boundary(0));
    }

    #[test]
    fn run_auto_fix_check_pass() {
        let dir = tempdir().unwrap();
        let cfg = AutoFixConfig {
            enabled: true,
            trigger: AutoFixTrigger::Always,
            lint_command: Some("true".to_string()),
            test_command: Some("true".to_string()),
            max_retries: 3,
            timeout_secs: 5,
            lsp: Default::default(),
        };
        let action = run_auto_fix_check(
            dir.path(),
            &cfg,
            Autonomy::AutoEdit,
            0,
            &trusted(),
            &NOT_CANCELLED,
            None,
        );
        assert!(
            matches!(action, AutoFixAction::Continue { .. }),
            "got {action:?}"
        );
    }

    #[test]
    fn run_auto_fix_check_lint_fail_under_cap() {
        let dir = tempdir().unwrap();
        let cfg = AutoFixConfig {
            enabled: true,
            trigger: AutoFixTrigger::Always,
            lint_command: Some("false".to_string()),
            test_command: Some("true".to_string()),
            max_retries: 3,
            timeout_secs: 5,
            lsp: Default::default(),
        };
        let action = run_auto_fix_check(
            dir.path(),
            &cfg,
            Autonomy::AutoEdit,
            0,
            &trusted(),
            &NOT_CANCELLED,
            None,
        );
        match action {
            AutoFixAction::Retry { feedback, status } => {
                assert!(feedback.contains("Your last edits failed"));
                assert!(feedback.contains("Do not disable lints"));
                assert!(feedback.contains("(skipped: lint failed)"));
                assert!(status.contains("1/3"));
            }
            other => panic!("expected Retry, got {other:?}"),
        }
    }

    #[test]
    fn run_auto_fix_check_tests_fail_under_cap() {
        let dir = tempdir().unwrap();
        let cfg = AutoFixConfig {
            enabled: true,
            trigger: AutoFixTrigger::Always,
            lint_command: Some("true".to_string()),
            test_command: Some("false".to_string()),
            max_retries: 3,
            timeout_secs: 5,
            lsp: Default::default(),
        };
        let action = run_auto_fix_check(
            dir.path(),
            &cfg,
            Autonomy::AutoEdit,
            1,
            &trusted(),
            &NOT_CANCELLED,
            None,
        );
        match action {
            AutoFixAction::Retry { feedback, status } => {
                assert!(feedback.contains("## Tests"));
                assert!(status.contains("2/3"));
            }
            other => panic!("expected Retry, got {other:?}"),
        }
    }

    #[test]
    fn run_auto_fix_check_cap_reached() {
        let dir = tempdir().unwrap();
        let cfg = AutoFixConfig {
            enabled: true,
            trigger: AutoFixTrigger::Always,
            lint_command: Some("false".to_string()),
            test_command: Some("true".to_string()),
            max_retries: 3,
            timeout_secs: 5,
            lsp: Default::default(),
        };
        let action = run_auto_fix_check(
            dir.path(),
            &cfg,
            Autonomy::AutoEdit,
            3,
            &trusted(),
            &NOT_CANCELLED,
            None,
        );
        match action {
            AutoFixAction::GiveUp { status } => {
                assert!(status.contains("cap reached"));
                assert!(status.contains("3/3"));
                assert!(status.contains("working tree left as-is"));
            }
            other => panic!("expected GiveUp, got {other:?}"),
        }
    }

    #[test]
    fn run_auto_fix_check_trigger_off() {
        let dir = tempdir().unwrap();
        let cfg = AutoFixConfig {
            enabled: true,
            trigger: AutoFixTrigger::Off,
            lint_command: Some("false".to_string()),
            test_command: Some("false".to_string()),
            max_retries: 3,
            timeout_secs: 5,
            lsp: Default::default(),
        };
        let action = run_auto_fix_check(
            dir.path(),
            &cfg,
            Autonomy::AutoEdit,
            0,
            &trusted(),
            &NOT_CANCELLED,
            None,
        );
        match action {
            AutoFixAction::Continue { status } => {
                assert!(status.is_none(), "trigger off should be silent");
            }
            other => panic!("expected Continue, got {other:?}"),
        }
    }

    #[test]
    fn run_auto_fix_check_autonomous_skips_in_suggest_mode() {
        let dir = tempdir().unwrap();
        let cfg = AutoFixConfig {
            enabled: true,
            trigger: AutoFixTrigger::Autonomous,
            lint_command: Some("false".to_string()),
            test_command: Some("false".to_string()),
            max_retries: 3,
            timeout_secs: 5,
            lsp: Default::default(),
        };
        let action = run_auto_fix_check(
            dir.path(),
            &cfg,
            Autonomy::Suggest,
            0,
            &trusted(),
            &NOT_CANCELLED,
            None,
        );
        assert!(matches!(action, AutoFixAction::Continue { status: None }));
    }

    /// The default mode became `ask` (prompted edits, as `auto-edit` was);
    /// the loop must keep running after approved edits there.
    #[test]
    fn the_autonomous_trigger_runs_in_every_mode_but_suggest() {
        let cfg = AutoFixConfig::default();
        assert_eq!(cfg.trigger, AutoFixTrigger::Autonomous);
        for mode in Autonomy::ALL {
            assert_eq!(
                should_trigger(&cfg, mode),
                mode != Autonomy::Suggest,
                "{mode}"
            );
        }
    }

    #[test]
    fn run_auto_fix_check_no_runners() {
        let dir = tempdir().unwrap();
        let cfg = AutoFixConfig {
            enabled: true,
            trigger: AutoFixTrigger::Always,
            lint_command: None,
            test_command: None,
            max_retries: 3,
            timeout_secs: 5,
            lsp: Default::default(),
        };
        let action = run_auto_fix_check(
            dir.path(),
            &cfg,
            Autonomy::AutoEdit,
            0,
            &trusted(),
            &NOT_CANCELLED,
            None,
        );
        assert!(matches!(action, AutoFixAction::Continue { status: None }));
    }

    // ── Trust and sandbox ────────────────────────────────────────────────────

    fn marker_cfg() -> AutoFixConfig {
        AutoFixConfig {
            enabled: true,
            trigger: AutoFixTrigger::Always,
            lint_command: Some("touch lint.marker".to_string()),
            test_command: Some("touch test.marker".to_string()),
            max_retries: 3,
            timeout_secs: 10,
            lsp: Default::default(),
        }
    }

    /// Lint and test commands run project code; an untrusted folder used to
    /// run them after every edit in the default `auto-edit` autonomy.
    #[test]
    fn an_untrusted_project_runs_no_check_at_all() {
        let dir = tempdir().unwrap();
        let untrusted = Containment::default();
        let action = run_auto_fix_check(
            dir.path(),
            &marker_cfg(),
            Autonomy::AutoEdit,
            0,
            &untrusted,
            &NOT_CANCELLED,
            None,
        );
        assert!(matches!(action, AutoFixAction::Untrusted), "{action:?}");
        assert!(!dir.path().join("lint.marker").exists());
        assert!(!dir.path().join("test.marker").exists());

        // An auto-detected runner is not even probed.
        let proj = tempdir().unwrap();
        std::fs::write(proj.path().join("Cargo.toml"), "[package]\n").unwrap();
        let cfg = AutoFixConfig {
            lint_command: None,
            test_command: None,
            ..marker_cfg()
        };
        let action = run_auto_fix_check(
            proj.path(),
            &cfg,
            Autonomy::AutoEdit,
            0,
            &untrusted,
            &NOT_CANCELLED,
            None,
        );
        assert!(matches!(action, AutoFixAction::Untrusted), "{action:?}");

        // Nothing would have run: nothing to tell the user either.
        let empty = tempdir().unwrap();
        let action = run_auto_fix_check(
            empty.path(),
            &cfg,
            Autonomy::AutoEdit,
            0,
            &untrusted,
            &NOT_CANCELLED,
            None,
        );
        assert!(
            matches!(action, AutoFixAction::Continue { status: None }),
            "{action:?}"
        );
        // Nor when auto-fix would not have triggered.
        let action = run_auto_fix_check(
            dir.path(),
            &AutoFixConfig {
                trigger: AutoFixTrigger::Off,
                ..marker_cfg()
            },
            Autonomy::AutoEdit,
            0,
            &untrusted,
            &NOT_CANCELLED,
            None,
        );
        assert!(
            matches!(action, AutoFixAction::Continue { status: None }),
            "{action:?}"
        );
    }

    #[test]
    fn a_trusted_project_runs_its_checks() {
        let dir = tempdir().unwrap();
        let action = run_auto_fix_check(
            dir.path(),
            &marker_cfg(),
            Autonomy::AutoEdit,
            0,
            &trusted(),
            &NOT_CANCELLED,
            None,
        );
        assert!(
            matches!(action, AutoFixAction::Continue { status: Some(_) }),
            "{action:?}"
        );
        assert!(dir.path().join("lint.marker").exists());
        assert!(dir.path().join("test.marker").exists());
    }

    /// Auto-fix is never less contained than Bash: the strict denylist
    /// applies, and a sandbox that cannot be applied refuses instead of
    /// running the command bare.
    #[test]
    fn checks_go_through_the_bash_sandbox() {
        let dir = tempdir().unwrap();
        let strict = Containment {
            trusted: true,
            sandbox_mode: Some("strict".to_string()),
            sandbox_allow_network: false,
        };
        let cfg = AutoFixConfig {
            lint_command: None,
            test_command: Some("touch test.marker # rm -rf /".to_string()),
            ..marker_cfg()
        };
        let action = run_auto_fix_check(
            dir.path(),
            &cfg,
            Autonomy::AutoEdit,
            0,
            &strict,
            &NOT_CANCELLED,
            None,
        );
        match action {
            AutoFixAction::Continue { status: Some(s) } => {
                assert!(s.contains("Blocked by strict sandbox"), "{s}")
            }
            other => panic!("expected a skip, got {other:?}"),
        }
        assert!(!dir.path().join("test.marker").exists());

        let broken = Containment {
            sandbox_mode: Some("bogus".to_string()),
            ..strict
        };
        let action = run_auto_fix_check(
            dir.path(),
            &marker_cfg(),
            Autonomy::AutoEdit,
            0,
            &broken,
            &NOT_CANCELLED,
            None,
        );
        match action {
            AutoFixAction::Continue { status: Some(s) } => assert!(s.contains("bogus"), "{s}"),
            other => panic!("expected a skip, got {other:?}"),
        }
        assert!(!dir.path().join("lint.marker").exists());
        assert!(!dir.path().join("test.marker").exists());
    }

    /// Under bwrap, `cargo test` with no crate cache or network used to be
    /// reported to the model as "your last edits failed", retry after retry.
    #[test]
    fn sandbox_environment_errors_are_not_sent_as_edit_failures() {
        let bwrap = Containment {
            trusted: true,
            sandbox_mode: Some("bwrap".to_string()),
            sandbox_allow_network: false,
        };
        let cargo = Some(
            "    Updating crates.io index\n\
             error: failed to get `serde` as a dependency of package `x v0.1.0`\n"
                .to_string(),
        );
        let sh = Some("bash: line 1: ruff: command not found\n".to_string());
        let dash = Some("sh: 1: ruff: not found\n".to_string());
        let go = Some("dial tcp: lookup proxy.golang.org: Temporary failure\n".to_string());
        for out in [&cargo, &sh, &dash, &go] {
            let status = super::sandbox_environment_failure(
                &bwrap,
                &[(None, &None), (Some("ruff check ."), out)],
            )
            .expect("an environment failure");
            assert!(status.contains("bwrap sandbox"), "{status}");
        }
        // The venv runner is matched by its file name.
        assert!(
            super::sandbox_environment_failure(&bwrap, &[(Some(".venv/bin/ruff check ."), &sh)])
                .is_some()
        );

        // A real build error is still the model's to fix.
        let rustc = Some(
            "error[E0583]: file not found for module `foo`\n\
             error[E0425]: cannot find value `x` in this scope\n"
                .to_string(),
        );
        assert_eq!(
            super::sandbox_environment_failure(
                &bwrap,
                &[(Some("cargo clippy"), &rustc), (Some("cargo test"), &None)]
            ),
            None
        );
        // "not found" from inside the project (a function the model renamed
        // in a script, an HTTP 404) is the model's to fix, not the sandbox's.
        for out in [
            "./build.sh: line 3: do_bild: command not found\n",
            "404 Client Error: Not Found for url: http://localhost/x\n",
        ] {
            let out = Some(out.to_string());
            assert_eq!(
                super::sandbox_environment_failure(
                    &bwrap,
                    &[(Some("make check"), &None), (Some("./build.sh test"), &out)]
                ),
                None,
                "{out:?}"
            );
        }
        // Without a namespace sandbox the environment is the user's own, and
        // a failure is reported as before.
        for mode in [None, Some("strict".to_string())] {
            let c = Containment {
                sandbox_mode: mode,
                ..bwrap.clone()
            };
            assert_eq!(
                super::sandbox_environment_failure(
                    &c,
                    &[(Some("cargo test"), &cargo), (Some("ruff check ."), &sh)]
                ),
                None
            );
        }
    }

    /// The namespace modes wrap the command exactly as for Bash; without
    /// the backend installed they refuse. Either way the bare command never
    /// reaches the shell.
    #[test]
    fn namespace_sandboxes_wrap_the_check_command() {
        let cwd = std::path::Path::new("/work/proj");
        let no_sandbox = trusted();
        assert_eq!(no_sandbox.wrap("make test", cwd).unwrap(), "make test");
        for (mode, binary, no_net) in [
            ("bwrap", "bwrap ", "--unshare-net"),
            ("firejail", "firejail ", "--net=none"),
        ] {
            let c = Containment {
                trusted: true,
                sandbox_mode: Some(mode.to_string()),
                sandbox_allow_network: false,
            };
            match c.wrap("make test", cwd) {
                Ok(cmd) => {
                    assert!(cmd.starts_with(binary), "{cmd}");
                    assert!(cmd.contains(no_net), "{cmd}");
                    assert!(cmd.ends_with("-c 'make test'"), "{cmd}");
                    assert_eq!(
                        Ok(cmd),
                        crate::sandbox::apply_sandbox("make test", mode, cwd, false)
                    );
                }
                Err(e) => assert!(e.contains(&format!("{mode} not found")), "{e}"),
            }
        }
    }
}

#[cfg(test)]
mod lsp_diff_tests {
    use super::*;
    use serde_json::json;

    fn diag(line: u64, severity: u64, message: &str) -> Value {
        json!({
            "range": {"start": {"line": line, "character": 4}, "end": {"line": line, "character": 7}},
            "severity": severity,
            "message": message
        })
    }

    #[test]
    fn changed_lines_ignore_lines_that_only_moved() {
        let before = "a\nb\nc\nd\n";
        // A line inserted at the top, `c` edited.
        let after = "new\na\nb\nC\nd\n";
        assert_eq!(
            changed_lines(before, after),
            vec![true, false, false, true, false]
        );
        // A pure deletion marks the lines either side of it.
        assert_eq!(changed_lines("a\nb\nc\n", "a\nc\n"), vec![true, true]);
        // A new file: every line is the edit's.
        assert_eq!(changed_lines("", "x\ny\n"), vec![true, true]);
    }

    /// Without a server's earlier report, only errors on changed lines count;
    /// warnings never do unless asked for.
    #[test]
    fn without_a_prior_report_only_changed_lines_count() {
        let baseline = LspBaseline {
            content: Some("x = ERR\ny = 1\n".into()),
            diagnostics: None,
        };
        let after = "z = 0\nx = ERR\ny = ERR2\nw = WARN\n";
        let published = [
            diag(1, 1, "old"),
            diag(2, 1, "new"),
            diag(3, 2, "a warning"),
        ];
        let got = new_problems(&published, Some(&baseline), Some(after), false);
        assert_eq!(
            got.iter().map(|d| d.message.as_str()).collect::<Vec<_>>(),
            ["new"]
        );
        let got = new_problems(&published, Some(&baseline), Some(after), true);
        assert_eq!(got.len(), 2);
        assert_eq!(got[1].render("a.py"), "a.py:4:5 warning: a warning");
    }

    /// With a report from before the edit, a problem it already had is not
    /// new even where the edit moved it; a second copy of it is.
    #[test]
    fn a_prior_report_subtracts_problems_that_were_already_there() {
        let baseline = LspBaseline {
            content: Some("x = ERR\n".into()),
            diagnostics: Some(vec![diag(0, 1, "undefined ERR")]),
        };
        let after = "import os\nx = ERR\nx = ERR\n";
        let published = [diag(1, 1, "undefined ERR"), diag(2, 1, "undefined ERR")];
        let got = new_problems(&published, Some(&baseline), Some(after), false);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].line, 2);
    }

    #[test]
    fn rendered_lines_are_one_based_and_on_one_line() {
        let d = Diag::parse(&diag(9, 1, "expected `;`\n  found `}`"), false).unwrap();
        assert_eq!(d.render("src/a.rs"), "src/a.rs:10:5 expected `;` found `}`");
        assert!(Diag::parse(&diag(0, 3, "info"), true).is_none());
    }
}

/// Auto-fix against a stand-in language server (python3 speaking LSP over
/// stdio): it reports an error on every line containing `ERR` and a
/// warning on every line containing `WARN`.
#[cfg(all(test, unix))]
mod lsp_check_tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    static NOT_CANCELLED: AtomicBool = AtomicBool::new(false);

    /// `mode`: `ok`, `hang` (never answers `initialize`), `same` (publishes
    /// only when a document's diagnostics change, like rust-analyzer),
    /// `silent` (never publishes), `deaf` (stops reading its input after
    /// `initialized`), `lag` (publishes for `didOpen` only, like a server
    /// still analysing every later change).
    fn fake_server(bin: &Path, log: &Path, mode: &str) {
        use std::os::unix::fs::PermissionsExt;
        let script = format!(
            r#"#!/usr/bin/env python3
import json, sys, time
LOG, MODE = {log:?}, {mode:?}
def log(s):
    with open(LOG, "a") as f:
        f.write(s + "\n")
def read():
    n = None
    while True:
        line = sys.stdin.buffer.readline()
        if not line:
            return None
        line = line.strip()
        if not line:
            break
        k, v = line.split(b":", 1)
        if k.strip().lower() == b"content-length":
            n = int(v)
    return json.loads(sys.stdin.buffer.read(n))
def send(msg):
    b = json.dumps(msg).encode()
    sys.stdout.buffer.write(b"Content-Length: %d\r\n\r\n" % len(b) + b)
    sys.stdout.buffer.flush()
log("start")
held = []
last = {{}}
while True:
    m = read()
    if m is None:
        break
    method = m.get("method")
    if method == "initialize":
        if MODE != "hang":
            send({{"jsonrpc": "2.0", "id": m["id"], "result": {{"capabilities": {{"textDocumentSync": 1}}}}}})
    elif method == "initialized" and MODE == "deaf":
        log("deaf")
        time.sleep(60)
    elif method in ("textDocument/didOpen", "textDocument/didChange"):
        log(method + " " + m["params"]["textDocument"]["uri"])
        doc = m["params"]["textDocument"]
        text = doc["text"] if "text" in doc else m["params"]["contentChanges"][-1]["text"]
        diags = []
        for i, l in enumerate(text.splitlines()):
            for word, sev in (("ERR", 1), ("WARN", 2)):
                if word in l:
                    c = l.index(word)
                    diags.append({{"range": {{"start": {{"line": i, "character": c}}, "end": {{"line": i, "character": c + 3}}}}, "severity": sev, "message": "bad " + l.strip()}})
        if MODE == "lag" and method == "textDocument/didChange":
            log("lagging")
        elif MODE == "silent" or (MODE == "same" and last.get(doc["uri"]) == diags):
            log("unchanged")
        else:
            held.append({{"uri": doc["uri"], "version": doc["version"], "diagnostics": diags}})
        last[doc["uri"]] = diags
        # Like pyright: ask for configuration and wait for the answer.
        send({{"jsonrpc": "2.0", "id": 1, "method": "workspace/configuration", "params": {{"items": [{{}}]}}}})
    elif "method" not in m and m.get("id") == 1:
        log("configured")
        for p in held:
            send({{"jsonrpc": "2.0", "method": "textDocument/publishDiagnostics", "params": p}})
        held = []
    elif method == "shutdown":
        log("shutdown")
        send({{"jsonrpc": "2.0", "id": m["id"], "result": None}})
    elif method == "exit":
        log("exit")
        sys.exit(0)
"#
        );
        let p = bin.join("pyright-langserver");
        std::fs::write(&p, script).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    struct Fixture {
        project: tempfile::TempDir,
        bin: tempfile::TempDir,
        log: PathBuf,
        pool: LspPool,
        rt: tokio::runtime::Runtime,
    }

    fn fixture(mode: &str) -> Fixture {
        let project = tempfile::tempdir().unwrap();
        let bin = tempfile::tempdir().unwrap();
        let log = bin.path().join("server.log");
        fake_server(bin.path(), &log, mode);
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        Fixture {
            project,
            bin,
            log,
            pool: LspPool::default(),
            rt,
        }
    }

    impl Fixture {
        fn file(&self, name: &str) -> PathBuf {
            self.project.path().join(name)
        }

        /// The file's state before an edit, as the TUI records it.
        fn baseline(&self, file: &Path) -> LspBaseline {
            self.rt.block_on(capture_lsp_baseline(
                &self.pool,
                self.project.path(),
                file,
                Some(self.bin.path().as_os_str()),
            ))
        }

        fn check(
            &self,
            files: Vec<(PathBuf, Option<LspBaseline>)>,
            config: &AutoFixConfig,
            containment: &Containment,
        ) -> AutoFixAction {
            self.check_cancellable(files, config, containment, &NOT_CANCELLED)
        }

        fn check_cancellable(
            &self,
            files: Vec<(PathBuf, Option<LspBaseline>)>,
            config: &AutoFixConfig,
            containment: &Containment,
            cancel: &AtomicBool,
        ) -> AutoFixAction {
            let lsp = LspDiagnostics {
                pool: self.pool.clone(),
                root: self.project.path().to_path_buf(),
                files,
                search_path: Some(self.bin.path().as_os_str().to_owned()),
                runtime: self.rt.handle().clone(),
            };
            run_auto_fix_check(
                self.project.path(),
                config,
                Autonomy::AutoEdit,
                0,
                containment,
                cancel,
                Some(&lsp),
            )
        }

        fn log(&self) -> String {
            std::fs::read_to_string(&self.log).unwrap_or_default()
        }
    }

    fn config() -> AutoFixConfig {
        AutoFixConfig {
            trigger: AutoFixTrigger::Always,
            lsp: LspDiagnosticsConfig {
                settle: Duration::from_millis(200),
                timeout: Duration::from_secs(5),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn trusted() -> Containment {
        Containment {
            trusted: true,
            ..Default::default()
        }
    }

    /// An edit that adds an error: it reaches the model as
    /// `file:line:col message`; the warning next to it does not.
    #[test]
    fn a_new_error_is_fed_back_and_warnings_are_not() {
        let f = fixture("ok");
        let file = f.file("app.py");
        std::fs::write(&file, "x = 1\n").unwrap();
        let before = f.baseline(&file);
        std::fs::write(&file, "x = 1\ny = ERR\nz = WARN\n").unwrap();
        let action = f.check(vec![(file, Some(before))], &config(), &trusted());
        let AutoFixAction::Retry { feedback, .. } = action else {
            panic!("expected a retry: {action:?}");
        };
        assert!(
            feedback.contains("## Language server (pyright-langserver)\napp.py:2:5 bad y = ERR"),
            "{feedback}"
        );
        assert!(!feedback.contains("WARN"), "{feedback}");
        assert!(!feedback.contains("## Lint"), "{feedback}");
        // The server's configuration request was answered.
        assert!(f.log().contains("configured"));
    }

    /// An error the file already had is not the edit's: neither on a first
    /// check (no server ran before it) nor on a later one, where the
    /// server's earlier report is the baseline.
    #[test]
    fn pre_existing_errors_are_not_repeated() {
        let f = fixture("ok");
        let file = f.file("app.py");
        std::fs::write(&file, "x = ERR\n").unwrap();
        let before = f.baseline(&file);
        assert!(before.diagnostics.is_none(), "no server was running yet");
        std::fs::write(&file, "import os\nx = ERR\n").unwrap();
        let action = f.check(vec![(file.clone(), Some(before))], &config(), &trusted());
        let AutoFixAction::Continue { status } = &action else {
            panic!("expected no retry: {action:?}");
        };
        assert_eq!(status.as_deref(), Some("[auto-fix] checks passed"));

        // Next turn: the running server's report is the baseline.
        let before = f.baseline(&file);
        assert_eq!(before.diagnostics.as_ref().map(Vec::len), Some(1));
        std::fs::write(&file, "import os\nimport sys\nx = ERR\nw = ERR2\n").unwrap();
        let action = f.check(vec![(file, Some(before))], &config(), &trusted());
        let AutoFixAction::Retry { feedback, .. } = action else {
            panic!("expected a retry: {action:?}");
        };
        assert!(feedback.contains("app.py:4:5 bad w = ERR2"), "{feedback}");
        assert!(!feedback.contains("bad x = ERR"), "{feedback}");
        assert_eq!(f.log().matches("start").count(), 1, "one server, reused");
        // The open document gets its new text, not a second didOpen.
        assert_eq!(f.log().matches("didOpen").count(), 1, "{}", f.log());
        assert_eq!(f.log().matches("didChange").count(), 1, "{}", f.log());
    }

    /// Language servers run project code: an untrusted folder starts none,
    /// and says so through the same notice as lint and tests.
    #[test]
    fn an_untrusted_project_starts_no_language_server() {
        let f = fixture("ok");
        let file = f.file("app.py");
        std::fs::write(&file, "y = ERR\n").unwrap();
        let action = f.check(vec![(file, None)], &config(), &Containment::default());
        assert!(matches!(action, AutoFixAction::Untrusted), "{action:?}");
        std::thread::sleep(Duration::from_millis(200));
        assert!(!f.log.exists(), "a server started: {}", f.log());
    }

    #[test]
    fn the_lsp_opt_out_is_respected() {
        let f = fixture("ok");
        let file = f.file("app.py");
        std::fs::write(&file, "y = ERR\n").unwrap();
        let mut cfg = config();
        cfg.lsp.enabled = false;
        let action = f.check(vec![(file, None)], &cfg, &trusted());
        assert!(
            matches!(action, AutoFixAction::Continue { status: None }),
            "{action:?}"
        );
        assert!(!f.log.exists(), "a server started: {}", f.log());
    }

    /// A server that never answers holds the turn for the cap at most, and
    /// is not waited on again this session.
    #[test]
    fn a_hung_server_hits_the_cap_and_is_given_up() {
        let f = fixture("hang");
        let file = f.file("app.py");
        std::fs::write(&file, "y = ERR\n").unwrap();
        let mut cfg = config();
        cfg.lsp.timeout = Duration::from_secs(1);
        let started = std::time::Instant::now();
        let action = f.check(vec![(file.clone(), None)], &cfg, &trusted());
        let took = started.elapsed();
        assert!(took < Duration::from_secs(4), "took {took:?}");
        let AutoFixAction::Continue {
            status: Some(status),
        } = &action
        else {
            panic!("expected a note: {action:?}");
        };
        assert!(
            status.contains("pyright-langserver did not answer within 1s"),
            "{status}"
        );

        let started = std::time::Instant::now();
        let action = f.check(vec![(file, None)], &cfg, &trusted());
        assert!(started.elapsed() < Duration::from_millis(500));
        assert!(
            matches!(action, AutoFixAction::Continue { status: None }),
            "{action:?}"
        );
        assert_eq!(f.log().matches("start").count(), 1);
    }

    /// Esc (or quit) while a server is starting returns at once and frees
    /// the pool: the exit path's shutdown waited on its lock for up to the
    /// whole cap. The server is not given up on: it was not its fault.
    #[test]
    fn esc_during_a_server_start_returns_at_once() {
        let f = fixture("hang");
        let file = f.file("app.py");
        std::fs::write(&file, "y = ERR\n").unwrap();
        let mut cfg = config();
        cfg.lsp.timeout = Duration::from_secs(20);
        let cancel = AtomicBool::new(false);
        let started = std::time::Instant::now();
        let action = std::thread::scope(|s| {
            s.spawn(|| {
                std::thread::sleep(Duration::from_millis(300));
                cancel.store(true, Ordering::SeqCst);
            });
            f.check_cancellable(vec![(file, None)], &cfg, &trusted(), &cancel)
        });
        let took = started.elapsed();
        assert!(took < Duration::from_secs(3), "took {took:?}: {action:?}");
        let quit = std::time::Instant::now();
        f.rt.block_on(f.pool.shutdown());
        assert!(
            quit.elapsed() < Duration::from_secs(2),
            "{:?}",
            quit.elapsed()
        );
        assert!(!f.pool.gave_up(
            "pyright-langserver",
            &["--stdio".to_string()],
            f.project.path()
        ));
    }

    /// A server that hangs in `initialize` costs only itself: another
    /// server in the same round used to wait behind its start for the pool
    /// lock, hit the cap and be given up for the session too.
    #[test]
    fn a_hung_server_does_not_take_another_down_with_it() {
        let f = fixture("ok");
        let hung = tempfile::tempdir().unwrap();
        fake_server(hung.path(), &hung.path().join("log"), "hang");
        std::fs::copy(
            hung.path().join("pyright-langserver"),
            f.bin.path().join("gopls"),
        )
        .unwrap();
        let go = f.file("main.go");
        std::fs::write(&go, "package main\n").unwrap();
        let py = f.file("app.py");
        std::fs::write(&py, "y = ERR\n").unwrap();
        let mut cfg = config();
        cfg.lsp.timeout = Duration::from_secs(2);
        let action = f.check(vec![(go, None), (py, None)], &cfg, &trusted());
        let AutoFixAction::Retry { feedback, status } = &action else {
            panic!("expected a retry: {action:?}");
        };
        assert!(feedback.contains("app.py:1:5 bad y = ERR"), "{feedback}");
        assert!(
            status.contains("gopls did not answer within 2s"),
            "{status}"
        );
        assert!(!status.contains("pyright-langserver"), "{status}");
        assert!(!f.pool.gave_up(
            "pyright-langserver",
            &["--stdio".to_string()],
            f.project.path()
        ));
    }

    /// A file the server has open that changed on disk since (`/undo`
    /// restored it) is sent again before the next check, even when that
    /// check's edits are elsewhere: the server kept the old text.
    #[test]
    fn files_restored_since_the_last_check_are_resent() {
        let f = fixture("ok");
        let (a, b) = (f.file("a.py"), f.file("b.py"));
        std::fs::write(&a, "x = 1\n").unwrap();
        std::fs::write(&b, "def g(): pass\n").unwrap();
        let action = f.check(
            vec![(a.clone(), None), (b.clone(), None)],
            &config(),
            &trusted(),
        );
        assert!(
            matches!(action, AutoFixAction::Continue { .. }),
            "{action:?}"
        );
        std::fs::write(&b, "def f(): pass\n").unwrap();
        std::fs::write(&a, "x = 2\n").unwrap();
        let action = f.check(vec![(a, None)], &config(), &trusted());
        assert!(
            matches!(action, AutoFixAction::Continue { .. }),
            "{action:?}"
        );
        let log = f.log();
        let b_uri = crate::tools::lsp::path_to_uri(&b);
        assert_eq!(
            log.matches(&format!("didChange {b_uri}")).count(),
            1,
            "{log}"
        );
    }

    /// The feedback stays bounded however many errors a file has.
    #[test]
    fn errors_are_capped_at_thirty_lines() {
        let f = fixture("ok");
        let file = f.file("gen.py");
        let body: String = (0..40).map(|i| format!("v{i} = ERR\n")).collect();
        std::fs::write(&file, &body).unwrap();
        let action = f.check(
            vec![(file, Some(LspBaseline::default()))],
            &config(),
            &trusted(),
        );
        let AutoFixAction::Retry { feedback, .. } = action else {
            panic!("expected a retry: {action:?}");
        };
        let section = feedback.split("## Language server").nth(1).unwrap();
        let lines: Vec<&str> = section
            .lines()
            .skip(1)
            .take_while(|l| !l.is_empty())
            .collect();
        assert_eq!(lines.len(), MAX_LSP_LINES, "{section}");
        assert_eq!(lines.last(), Some(&"... and 11 more"));
    }

    /// Exit sends `shutdown` and `exit` rather than killing the server.
    #[test]
    fn servers_are_shut_down_cleanly() {
        let f = fixture("ok");
        let file = f.file("app.py");
        std::fs::write(&file, "y = 1\n").unwrap();
        let action = f.check(vec![(file, None)], &config(), &trusted());
        assert!(
            matches!(action, AutoFixAction::Continue { .. }),
            "{action:?}"
        );
        f.rt.block_on(f.pool.shutdown());
        let log = f.log();
        assert!(log.ends_with("shutdown\nexit\n"), "{log}");
    }

    /// rust-analyzer publishes only when a file's diagnostics change. An
    /// edit that keeps them as they were (still clean, or the same error
    /// still there) neither holds the turn to the cap nor drops the server.
    #[test]
    fn a_server_that_publishes_only_changes_is_kept() {
        let f = fixture("same");
        let file = f.file("app.py");
        let cfg = config();
        let edit = |text: &str| {
            let before = f.baseline(&file);
            std::fs::write(&file, text).unwrap();
            let started = std::time::Instant::now();
            let action = f.check(vec![(file.clone(), Some(before))], &cfg, &trusted());
            let took = started.elapsed();
            assert!(took < Duration::from_secs(2), "took {took:?}: {action:?}");
            action
        };
        let passed = |action: &AutoFixAction| {
            matches!(action, AutoFixAction::Continue { status: Some(s) }
                if s == "[auto-fix] checks passed")
        };

        let first = edit("x = 1\n");
        assert!(passed(&first), "{first:?}");
        let clean = edit("x = 1\n# still clean\n");
        assert!(passed(&clean), "{clean:?}");
        let error = edit("x = 1\ny = ERR\n");
        let AutoFixAction::Retry { feedback, .. } = &error else {
            panic!("expected a retry: {error:?}");
        };
        assert!(feedback.contains("app.py:2:5 bad y = ERR"), "{feedback}");
        // The error stays where it was: nothing new, nothing published.
        let kept = edit("x = 1\ny = ERR\n# note\n");
        assert!(passed(&kept), "{kept:?}");

        let log = f.log();
        assert_eq!(log.matches("unchanged").count(), 2, "{log}");
        assert_eq!(log.matches("start").count(), 1, "{log}");
        assert!(!f.pool.gave_up(
            "pyright-langserver",
            &["--stdio".to_string()],
            f.project.path()
        ));
    }

    /// A server still analysing a change leaves its last set in place. That
    /// set's line numbers belong to the text it was published for: read
    /// against the new text (lines inserted above, the error fixed), the
    /// fixed error came back as new, on a line that no longer had it.
    #[test]
    fn a_stale_set_is_read_against_the_text_it_was_published_for() {
        let f = fixture("lag");
        let file = f.file("app.py");
        std::fs::write(
            &file, "x = 1
",
        )
        .unwrap();
        let before = f.baseline(&file);
        std::fs::write(
            &file,
            "x = 1
y = ERR
",
        )
        .unwrap();
        let action = f.check(vec![(file.clone(), Some(before))], &config(), &trusted());
        let AutoFixAction::Retry { feedback, .. } = &action else {
            panic!("expected a retry: {action:?}");
        };
        assert!(feedback.contains("app.py:2:5 bad y = ERR"), "{feedback}");

        // The retry fixes it and adds imports above; the server has not
        // published for that text by the time the check stops waiting.
        let before = f.baseline(&file);
        assert_eq!(before.diagnostics.as_ref().map(Vec::len), Some(1));
        std::fs::write(
            &file,
            "import a
import b
x = 1
y = 2
",
        )
        .unwrap();
        let action = f.check(vec![(file.clone(), Some(before))], &config(), &trusted());
        assert!(
            matches!(&action, AutoFixAction::Continue { status: Some(s) }
                if s == "[auto-fix] checks passed"),
            "{action:?}"
        );
        // The set is for the first text, which is gone after a further
        // change: nothing is said about it rather than misplace it.
        let before = f.baseline(&file);
        std::fs::write(
            &file,
            "import a
import b
import c
x = 1
y = 2
",
        )
        .unwrap();
        let action = f.check(vec![(file, Some(before))], &config(), &trusted());
        let AutoFixAction::Continue { status } = &action else {
            panic!("expected no retry: {action:?}");
        };
        assert!(
            !status.as_deref().unwrap_or_default().contains("ERR"),
            "{action:?}"
        );
        assert_eq!(f.log().matches("lagging").count(), 2, "{}", f.log());
    }

    /// A live server that says nothing by the cap (a slow cold start) is
    /// noted but kept, and asked again next time.
    #[test]
    fn a_server_with_nothing_to_say_yet_is_not_given_up() {
        let f = fixture("silent");
        let file = f.file("app.py");
        std::fs::write(&file, "y = ERR\n").unwrap();
        let mut cfg = config();
        cfg.lsp.timeout = Duration::from_secs(1);
        for _ in 0..2 {
            let action = f.check(vec![(file.clone(), None)], &cfg, &trusted());
            let AutoFixAction::Continue {
                status: Some(status),
            } = &action
            else {
                panic!("expected a note: {action:?}");
            };
            assert!(
                status
                    .contains("pyright-langserver reported nothing on 1 edited file(s) within 1s"),
                "{status}"
            );
            assert!(!status.contains("off for this session"), "{status}");
        }
        let log = f.log();
        assert_eq!(log.matches("didOpen").count(), 1, "{log}");
        assert_eq!(log.matches("didChange").count(), 1, "{log}");
    }

    /// A server that stops reading its input cannot hold the turn past the
    /// cap by blocking the write of a large file.
    #[test]
    fn a_server_that_stops_reading_is_given_up_within_the_cap() {
        let f = fixture("deaf");
        let file = f.file("big.py");
        std::fs::write(&file, "x = 1\n".repeat(400_000)).unwrap();
        let mut cfg = config();
        cfg.lsp.timeout = Duration::from_secs(2);
        let started = std::time::Instant::now();
        let action = f.check(vec![(file, None)], &cfg, &trusted());
        let took = started.elapsed();
        assert!(took < Duration::from_secs(5), "took {took:?}");
        let AutoFixAction::Continue {
            status: Some(status),
        } = &action
        else {
            panic!("expected a note: {action:?}");
        };
        assert!(
            status.contains("pyright-langserver did not take the edited files within 2s"),
            "{status}"
        );
        assert!(f.log().contains("deaf"), "{}", f.log());
    }

    /// Quitting is bounded too when a server stops reading mid-write.
    #[test]
    fn shutdown_does_not_hang_on_a_server_that_stopped_reading() {
        let f = fixture("deaf");
        let file = f.file("big.py");
        std::fs::write(&file, "x = 1\n".repeat(400_000)).unwrap();
        let exe = f.bin.path().join("pyright-langserver");
        let args = vec!["--stdio".to_string()];
        let root = f.project.path().to_path_buf();
        f.rt.block_on(async {
            let client = f
                .pool
                .client_for("pyright-langserver", &args, &root, &Launch::Program(exe))
                .await
                .unwrap();
            // Holds the input stream, stuck once the pipe is full.
            let stuck = tokio::spawn(async move { client.sync_document(&file).await.is_ok() });
            tokio::time::sleep(Duration::from_millis(300)).await;
            let started = std::time::Instant::now();
            tokio::time::timeout(Duration::from_secs(10), f.pool.shutdown())
                .await
                .expect("shutdown hung");
            assert!(started.elapsed() < Duration::from_secs(5));
            let _ = tokio::time::timeout(Duration::from_secs(5), stuck).await;
        });
    }

    /// Only files inside the trusted project go to its servers.
    #[test]
    fn files_outside_the_project_are_not_opened() {
        let f = fixture("ok");
        let elsewhere = tempfile::tempdir().unwrap();
        let outside = elsewhere.path().join("other.py");
        std::fs::write(&outside, "y = ERR\n").unwrap();
        let sneaky = f.project.path().join("..").join(
            elsewhere
                .path()
                .strip_prefix(f.project.path().parent().unwrap())
                .unwrap_or(elsewhere.path())
                .join("other.py"),
        );
        let action = f.check(vec![(outside.clone(), None)], &config(), &trusted());
        assert!(
            matches!(action, AutoFixAction::Continue { status: None }),
            "{action:?}"
        );
        assert!(!f.log.exists(), "a server started: {}", f.log());
        assert!(f.baseline(&outside).content.is_none());

        let inside = f.file("app.py");
        std::fs::write(&inside, "y = 1\n").unwrap();
        let action = f.check(
            vec![(inside, None), (outside, None), (sneaky, None)],
            &config(),
            &trusted(),
        );
        assert!(
            matches!(action, AutoFixAction::Continue { .. }),
            "{action:?}"
        );
        let log = f.log();
        assert_eq!(log.matches("didOpen").count(), 1, "{log}");
        assert!(log.contains("app.py") && !log.contains("other.py"), "{log}");
    }

    /// No server for the file: the baseline reads nothing.
    #[test]
    fn a_file_no_server_checks_gets_an_empty_baseline() {
        let f = fixture("ok");
        let file = f.file("data.json");
        std::fs::write(&file, "{}").unwrap();
        let b = f.baseline(&file);
        assert!(b.content.is_none() && b.diagnostics.is_none());
        let py = f.file("app.py");
        std::fs::write(&py, "x = 1\n").unwrap();
        assert_eq!(f.baseline(&py).content.as_deref(), Some("x = 1\n"));
    }
}
