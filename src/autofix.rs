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

use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};

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
}

/// Default test timeout if the user hasn't overridden it.
pub const DEFAULT_TEST_TIMEOUT_SECS: u64 = 60;

/// Shown once per session when an edit would have started a check in a
/// project that is not in `trustedProjects`.
pub const UNTRUSTED_NOTICE: &str = "Auto-fix skipped: this folder is not trusted. \
     Run /trust to let OxideClaw run its lint and test commands.";

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
    /// Only run when `/autonomy` mode is `auto-edit` or `full-auto`.
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
        // run_command goes through `sh -c` on unix only.
        #[cfg(unix)]
        let resolved = crate::sandbox::shell_quote(&resolved);
        let probe = format!("{resolved} clippy --version");
        let wrapped = containment.wrap(&probe, cwd).ok()?;
        if !matches!(
            run_command(cwd, &wrapped, timeout_secs, cancel),
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
/// `autonomy_mode` is a &str matching the current autonomy level:
/// `"read-only"`, `"plan-only"`, `"auto-edit"`, or `"full-auto"`.
pub fn should_trigger(config: &AutoFixConfig, autonomy_mode: &str) -> bool {
    if !config.enabled {
        return false;
    }
    match config.trigger {
        AutoFixTrigger::Off => false,
        AutoFixTrigger::Always => true,
        AutoFixTrigger::Autonomous => {
            matches!(autonomy_mode, "auto-edit" | "full-auto")
        }
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
pub fn run_command(cwd: &Path, cmd: &str, timeout_secs: u64, cancel: &AtomicBool) -> CommandResult {
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
) -> CheckOutcome {
    if lint_cmd.is_none() && test_cmd.is_none() {
        return CheckOutcome::NoRunners;
    }

    let mut lint_stderr: Option<String> = None;
    let mut lint_failed = false;

    if let Some(cmd) = lint_cmd {
        match run_command(cwd, cmd, timeout_secs, cancel) {
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
        match run_command(cwd, cmd, timeout_secs, cancel) {
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
/// `eslint-disable` / etc.
pub fn format_feedback_message(
    lint_cmd: Option<&str>,
    test_cmd: Option<&str>,
    lint_stderr: Option<&str>,
    test_stderr: Option<&str>,
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

    format!(
        "Your last edits failed automated checks. Fix the issues below.\n\
         \n\
         ## Lint ({lint_cmd_str})\n\
         {lint_body}\n\
         \n\
         ## Tests ({test_cmd_str})\n\
         {test_body}\n\
         \n\
         Make the minimum edits required to make both pass. Do not disable \
         lints, skip tests, or add `#[allow(...)]` / `# type: ignore` / \
         `eslint-disable` / `//nolint` unless the original code had them. \
         If a test assertion is genuinely wrong, explain why before changing it."
    )
}

/// Output that says the check could not run in a namespace sandbox (no
/// network, a tool or path outside it), as opposed to code that is wrong.
/// Lower-case substrings matched against lower-cased output.
const SANDBOX_ENVIRONMENT_ERRORS: [&str; 15] = [
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
    // a program or path the sandbox does not expose
    "command not found",
    ": not found",
    "read-only file system",
];

/// The status to show instead of a retry when a check failed inside a
/// namespace sandbox (bwrap, firejail) because of the sandbox itself. The
/// model would otherwise be told its edit broke the build and spend every
/// retry chasing an error no edit can fix.
fn sandbox_environment_failure(
    containment: &Containment,
    outputs: &[&Option<String>],
) -> Option<String> {
    let mode = containment
        .sandbox_mode
        .as_deref()
        .filter(|m| crate::sandbox::mode_enforces_isolation(m))?;
    let line = outputs
        .iter()
        .filter_map(|o| o.as_deref())
        .flat_map(str::lines)
        .find(|line| {
            let line = line.to_ascii_lowercase();
            SANDBOX_ENVIRONMENT_ERRORS.iter().any(|e| line.contains(e))
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

/// Run lint + tests and decide what the TUI turn loop should do next.
///
/// `autonomy_mode` is the current autonomy string (`"read-only"` /
/// `"plan-only"` / `"auto-edit"` / `"full-auto"`).
/// `retries_used` is the number of retries *already consumed* by this
/// user-prompt turn (so the first call passes `0`). Nothing runs unless
/// `containment.trusted`; what does run goes through its sandbox.
pub fn run_auto_fix_check(
    cwd: &Path,
    config: &AutoFixConfig,
    autonomy_mode: &str,
    retries_used: u32,
    containment: &Containment,
    cancel: &AtomicBool,
) -> AutoFixAction {
    if !should_trigger(config, autonomy_mode) {
        return AutoFixAction::Continue { status: None };
    }

    // Before detection: even the clippy probe below runs a binary in the
    // project (rustup honours its `rust-toolchain.toml`). Only file checks
    // decide whether there was anything to skip.
    if !containment.trusted {
        let would_run = detect_lint_command(cwd, &config.lint_command).is_some()
            || detect_test_command(cwd, &config.test_command).is_some();
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

    if lint_cmd.is_none() && test_cmd.is_none() {
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

    let outcome = run_checks(
        cwd,
        lint_run.as_deref(),
        test_run.as_deref(),
        config.timeout_secs,
        cancel,
    );

    match outcome {
        CheckOutcome::Pass => AutoFixAction::Continue {
            status: Some("[auto-fix] checks passed".to_string()),
        },
        CheckOutcome::NoRunners => AutoFixAction::Continue { status: None },
        CheckOutcome::Skipped { reason } => AutoFixAction::Continue {
            status: Some(format!("[auto-fix] skipped: {reason}")),
        },
        CheckOutcome::Fail {
            lint_stderr,
            test_stderr,
        } => {
            if let Some(status) =
                sandbox_environment_failure(containment, &[&lint_stderr, &test_stderr])
            {
                return AutoFixAction::Continue {
                    status: Some(status),
                };
            }
            if retries_used >= config.max_retries {
                let lint_tail = lint_stderr
                    .as_deref()
                    .map(trim_section)
                    .unwrap_or_else(|| "(no output)".to_string());
                let test_tail = test_stderr
                    .as_deref()
                    .map(trim_section)
                    .unwrap_or_else(|| "(skipped: lint failed)".to_string());
                AutoFixAction::GiveUp {
                    status: format!(
                        "[auto-fix] cap reached ({0}/{0}) — giving up, \
                         working tree left as-is\n\
                         Final lint output:\n{1}\n\
                         Final test output:\n{2}",
                        config.max_retries, lint_tail, test_tail,
                    ),
                }
            } else {
                let feedback = format_feedback_message(
                    lint_cmd.as_deref(),
                    test_cmd.as_deref(),
                    lint_stderr.as_deref(),
                    test_stderr.as_deref(),
                );
                AutoFixAction::Retry {
                    feedback,
                    status: format!(
                        "[auto-fix] checks failed — retry {}/{}",
                        retries_used + 1,
                        config.max_retries,
                    ),
                }
            }
        }
    }
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
        let run = |cmd: &str| super::run_command(td.path(), cmd, 60, &cancel);
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
        let r = super::run_command(td.path(), "sleep 30", 0, &cancel);
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
        assert!(matches!(r, super::CommandResult::Skipped { .. }), "{r:?}");
        // A cancelled lint never goes on to start the tests.
        let outcome = super::run_checks(td.path(), Some("true"), Some("touch ran"), 0, &cancel);
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
        };
        let action = super::run_auto_fix_check(
            proj.path(),
            &cfg,
            "auto-edit",
            0,
            &trusted(),
            &NOT_CANCELLED,
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
                &NOT_CANCELLED
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
                &NOT_CANCELLED
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
        let outcome = run_checks(dir.path(), Some("true"), Some("false"), 5, &NOT_CANCELLED);
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
        let outcome = run_checks(dir.path(), None, None, 5, &NOT_CANCELLED);
        assert!(
            matches!(outcome, CheckOutcome::NoRunners),
            "got {outcome:?}"
        );
    }

    #[test]
    fn run_checks_lint_only_pass() {
        let dir = tempdir().unwrap();
        let outcome = run_checks(dir.path(), Some("true"), None, 5, &NOT_CANCELLED);
        assert!(matches!(outcome, CheckOutcome::Pass), "got {outcome:?}");
    }

    #[test]
    fn run_checks_tests_only_fail() {
        let dir = tempdir().unwrap();
        let outcome = run_checks(dir.path(), None, Some("false"), 5, &NOT_CANCELLED);
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
        );
        assert!(msg.contains("(no output)"));
        assert!(msg.contains("assertion failed"));
    }

    #[test]
    fn format_feedback_message_truncates_lint() {
        let big = "x".repeat(5000);
        let msg =
            format_feedback_message(Some("cargo clippy"), Some("cargo test"), Some(&big), None);
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
        };
        let action =
            run_auto_fix_check(dir.path(), &cfg, "auto-edit", 0, &trusted(), &NOT_CANCELLED);
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
        };
        let action =
            run_auto_fix_check(dir.path(), &cfg, "auto-edit", 0, &trusted(), &NOT_CANCELLED);
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
        };
        let action =
            run_auto_fix_check(dir.path(), &cfg, "auto-edit", 1, &trusted(), &NOT_CANCELLED);
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
        };
        let action =
            run_auto_fix_check(dir.path(), &cfg, "auto-edit", 3, &trusted(), &NOT_CANCELLED);
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
        };
        let action =
            run_auto_fix_check(dir.path(), &cfg, "auto-edit", 0, &trusted(), &NOT_CANCELLED);
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
        };
        let action = run_auto_fix_check(dir.path(), &cfg, "suggest", 0, &trusted(), &NOT_CANCELLED);
        assert!(matches!(action, AutoFixAction::Continue { status: None }));
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
        };
        let action =
            run_auto_fix_check(dir.path(), &cfg, "auto-edit", 0, &trusted(), &NOT_CANCELLED);
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
            "auto-edit",
            0,
            &untrusted,
            &NOT_CANCELLED,
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
            "auto-edit",
            0,
            &untrusted,
            &NOT_CANCELLED,
        );
        assert!(matches!(action, AutoFixAction::Untrusted), "{action:?}");

        // Nothing would have run: nothing to tell the user either.
        let empty = tempdir().unwrap();
        let action = run_auto_fix_check(
            empty.path(),
            &cfg,
            "auto-edit",
            0,
            &untrusted,
            &NOT_CANCELLED,
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
            "auto-edit",
            0,
            &untrusted,
            &NOT_CANCELLED,
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
            "auto-edit",
            0,
            &trusted(),
            &NOT_CANCELLED,
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
        let action = run_auto_fix_check(dir.path(), &cfg, "auto-edit", 0, &strict, &NOT_CANCELLED);
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
            "auto-edit",
            0,
            &broken,
            &NOT_CANCELLED,
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
        let go = Some("dial tcp: lookup proxy.golang.org: Temporary failure\n".to_string());
        for out in [&cargo, &sh, &go] {
            let status = super::sandbox_environment_failure(&bwrap, &[&None, out])
                .expect("an environment failure");
            assert!(status.contains("bwrap sandbox"), "{status}");
        }

        // A real build error is still the model's to fix.
        let rustc = Some(
            "error[E0583]: file not found for module `foo`\n\
             error[E0425]: cannot find value `x` in this scope\n"
                .to_string(),
        );
        assert_eq!(
            super::sandbox_environment_failure(&bwrap, &[&rustc, &None]),
            None
        );
        // Without a namespace sandbox the environment is the user's own, and
        // a failure is reported as before.
        for mode in [None, Some("strict".to_string())] {
            let c = Containment {
                sandbox_mode: mode,
                ..bwrap.clone()
            };
            assert_eq!(super::sandbox_environment_failure(&c, &[&cargo, &sh]), None);
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
