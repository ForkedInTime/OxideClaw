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
/// `cmd` is a shell-style command string like `"cargo test"` or
/// `"npm test"`. The first whitespace-separated token is the program,
/// the rest are argv. `timeout_secs` caps total wall-clock runtime; if the
/// process is still running past it we send SIGKILL and return
/// `CommandResult::Timeout`. Pass `0` to wait indefinitely.
///
/// NOTE on implementation: the original spec called for `wait_timeout`, but
/// pulling in a new dep for ~20 lines isn't worth it. We use a poll+kill loop
/// via `try_wait`, which has the same behavior with no extra deps.
pub fn run_command(cwd: &Path, cmd: &str, timeout_secs: u64) -> CommandResult {
    let mut parts = cmd.split_whitespace();
    let Some(prog) = parts.next() else {
        return CommandResult::Skipped {
            reason: "empty test command".to_string(),
        };
    };
    let args: Vec<&str> = parts.collect();

    let spawn = Command::new(prog)
        .args(&args)
        .current_dir(cwd)
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

    // Poll until exit or timeout. `timeout_secs == 0` means "wait forever".
    let timeout = std::time::Duration::from_secs(timeout_secs);
    let has_timeout = timeout_secs > 0;
    let start = std::time::Instant::now();
    let poll = std::time::Duration::from_millis(100);

    loop {
        match child.try_wait() {
            Ok(Some(_status)) => break,
            Ok(None) => {
                if has_timeout && start.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
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
    }

    // Collect output.
    let output = match child.wait_with_output() {
        Ok(o) => o,
        Err(e) => {
            return CommandResult::Skipped {
                reason: format!("failed to collect output: {e}"),
            };
        }
    };

    if output.status.success() {
        CommandResult::Pass
    } else {
        let mut stderr = String::from_utf8_lossy(&output.stderr).to_string();
        // Some runners (cargo test, go test) print failure details to stdout.
        if stderr.trim().is_empty() {
            stderr = String::from_utf8_lossy(&output.stdout).to_string();
        }
        CommandResult::Fail { stderr }
    }
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
) -> CheckOutcome {
    if lint_cmd.is_none() && test_cmd.is_none() {
        return CheckOutcome::NoRunners;
    }

    let mut lint_stderr: Option<String> = None;
    let mut lint_failed = false;

    if let Some(cmd) = lint_cmd {
        match run_command(cwd, cmd, timeout_secs) {
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
        match run_command(cwd, cmd, timeout_secs) {
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
}

/// Run lint + tests and decide what the TUI turn loop should do next.
///
/// `autonomy_mode` is the current autonomy string (`"read-only"` /
/// `"plan-only"` / `"auto-edit"` / `"full-auto"`).
/// `retries_used` is the number of retries *already consumed* by this
/// user-prompt turn (so the first call passes `0`).
pub fn run_auto_fix_check(
    cwd: &Path,
    config: &AutoFixConfig,
    autonomy_mode: &str,
    retries_used: u32,
) -> AutoFixAction {
    if !should_trigger(config, autonomy_mode) {
        return AutoFixAction::Continue { status: None };
    }

    let lint_cmd = detect_lint_command(cwd, &config.lint_command);
    let test_cmd = detect_test_command(cwd, &config.test_command);

    if lint_cmd.is_none() && test_cmd.is_none() {
        return AutoFixAction::Continue { status: None };
    }

    let outcome = run_checks(
        cwd,
        lint_cmd.as_deref(),
        test_cmd.as_deref(),
        config.timeout_secs,
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
        let outcome = run_checks(dir.path(), Some("false"), Some(&test_cmd), 5);
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
        let outcome = run_checks(dir.path(), Some("true"), Some("false"), 5);
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
        let outcome = run_checks(dir.path(), None, None, 5);
        assert!(
            matches!(outcome, CheckOutcome::NoRunners),
            "got {outcome:?}"
        );
    }

    #[test]
    fn run_checks_lint_only_pass() {
        let dir = tempdir().unwrap();
        let outcome = run_checks(dir.path(), Some("true"), None, 5);
        assert!(matches!(outcome, CheckOutcome::Pass), "got {outcome:?}");
    }

    #[test]
    fn run_checks_tests_only_fail() {
        let dir = tempdir().unwrap();
        let outcome = run_checks(dir.path(), None, Some("false"), 5);
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
        let action = run_auto_fix_check(dir.path(), &cfg, "auto-edit", 0);
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
        let action = run_auto_fix_check(dir.path(), &cfg, "auto-edit", 0);
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
        let action = run_auto_fix_check(dir.path(), &cfg, "auto-edit", 1);
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
        let action = run_auto_fix_check(dir.path(), &cfg, "auto-edit", 3);
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
        let action = run_auto_fix_check(dir.path(), &cfg, "auto-edit", 0);
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
        let action = run_auto_fix_check(dir.path(), &cfg, "suggest", 0);
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
        let action = run_auto_fix_check(dir.path(), &cfg, "auto-edit", 0);
        assert!(matches!(action, AutoFixAction::Continue { status: None }));
    }
}
