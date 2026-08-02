/// Hooks execution engine — port of utils/hooks.ts
///
/// Hooks are user-defined shell commands that run at lifecycle events:
///   PreToolUse      — before a tool executes (can block)
///   PostToolUse     — after a tool completes
///   UserPromptSubmit — when the user sends a message
///   Notification    — when Claude sends a text chunk
///   Stop            — when the session ends
///   SessionStart    — when the session begins
///   PreCompact      — before a compact/summarize cycle
///   PostCompact     — after a compact/summarize cycle
///
/// Hook JSON output (parsed from stdout):
///   { "continue": false, "stopReason": "...", "decision": "approve"|"block",
///     "systemMessage": "...", "reason": "..." }
///
/// Exit codes:
///   0   — success (allow, continue)
///   2   — blocking error (block tool/continue, show stopReason or stdout)
///   other — non-blocking error (logged, execution continues)
use crate::settings::{HookEntry, HooksConfig};
use serde::Deserialize;

/// Result returned by a hook execution.
#[derive(Debug, Default)]
pub struct HookResult {
    /// If false, block the tool call / stop the turn (from `continue: false` or exit 2).
    pub should_continue: bool,
    /// Human-readable reason shown when blocked.
    pub stop_reason: Option<String>,
    /// System-level message to inject into the conversation.
    pub system_message: Option<String>,
    /// Permission decision returned by PreToolUse hooks.
    pub decision: Option<HookDecision>,
    /// Additional context to inject into the tool's environment.
    pub additional_context: Option<String>,
}

impl HookResult {
    pub fn allow() -> Self {
        Self {
            should_continue: true,
            ..Default::default()
        }
    }
}

#[derive(Debug, PartialEq)]
pub enum HookDecision {
    Approve,
    Block,
}

/// Minimal JSON output from a hook script.
#[derive(Debug, Deserialize, Default)]
struct HookOutput {
    #[serde(rename = "continue", default = "default_true")]
    continue_: bool,
    #[serde(rename = "stopReason")]
    stop_reason: Option<String>,
    #[serde(rename = "systemMessage")]
    system_message: Option<String>,
    decision: Option<String>,
    reason: Option<String>,
    #[serde(rename = "additionalContext")]
    additional_context: Option<String>,
}

fn default_true() -> bool {
    true
}

/// Run all matching PreToolUse hooks. Returns a HookResult — if `should_continue` is false,
/// the caller must block the tool call.
pub async fn run_pre_tool_hooks(
    hooks: &HooksConfig,
    tool_name: &str,
    tool_input: &str,
    session_id: &str,
    cwd: &std::path::Path,
) -> HookResult {
    let mut result = HookResult::allow();
    for hook in &hooks.pre_tool_use {
        if !hook.matches(tool_name) {
            continue;
        }
        let r = execute_hook(
            hook,
            HookEnvVars {
                event: "PreToolUse",
                tool_name: Some(tool_name),
                tool_input: Some(tool_input),
                tool_result: None,
                prompt: None,
                session_id,
                cwd,
            },
        )
        .await;
        if !r.should_continue {
            return r;
        }
        // Merge system messages / decisions
        if r.system_message.is_some() {
            result.system_message = r.system_message;
        }
        if r.decision.is_some() {
            result.decision = r.decision;
        }
        if r.additional_context.is_some() {
            result.additional_context = r.additional_context;
        }
    }
    result
}

/// Run all matching PostToolUse hooks. Fire-and-forget (result is not blocking).
pub async fn run_post_tool_hooks(
    hooks: &HooksConfig,
    tool_name: &str,
    tool_result: &str,
    session_id: &str,
    cwd: &std::path::Path,
) {
    for hook in &hooks.post_tool_use {
        if !hook.matches(tool_name) {
            continue;
        }
        execute_hook(
            hook,
            HookEnvVars {
                event: "PostToolUse",
                tool_name: Some(tool_name),
                tool_input: None,
                tool_result: Some(tool_result),
                prompt: None,
                session_id,
                cwd,
            },
        )
        .await;
    }
}

/// Run all UserPromptSubmit hooks. Returns any additional_context to prepend.
pub async fn run_user_prompt_hooks(
    hooks: &HooksConfig,
    prompt: &str,
    session_id: &str,
    cwd: &std::path::Path,
) -> Option<String> {
    let mut additional: Vec<String> = Vec::new();
    for hook in &hooks.user_prompt_submit {
        let r = execute_hook(
            hook,
            HookEnvVars {
                event: "UserPromptSubmit",
                tool_name: None,
                tool_input: None,
                tool_result: None,
                prompt: Some(prompt),
                session_id,
                cwd,
            },
        )
        .await;
        if let Some(ctx) = r.additional_context {
            additional.push(ctx);
        }
    }
    if additional.is_empty() {
        None
    } else {
        Some(additional.join("\n"))
    }
}

/// Run Stop hooks when the session ends.
pub async fn run_stop_hooks(hooks: &HooksConfig, session_id: &str, cwd: &std::path::Path) {
    for hook in &hooks.stop {
        execute_hook(
            hook,
            HookEnvVars {
                event: "Stop",
                tool_name: None,
                tool_input: None,
                tool_result: None,
                prompt: None,
                session_id,
                cwd,
            },
        )
        .await;
    }
}

/// Run SessionStart hooks.
pub async fn run_session_start_hooks(hooks: &HooksConfig, session_id: &str, cwd: &std::path::Path) {
    for hook in &hooks.session_start {
        execute_hook(
            hook,
            HookEnvVars {
                event: "SessionStart",
                tool_name: None,
                tool_input: None,
                tool_result: None,
                prompt: None,
                session_id,
                cwd,
            },
        )
        .await;
    }
}

/// Run PreCompact hooks.
pub async fn run_pre_compact_hooks(hooks: &HooksConfig, session_id: &str, cwd: &std::path::Path) {
    for hook in &hooks.pre_compact {
        execute_hook(
            hook,
            HookEnvVars {
                event: "PreCompact",
                tool_name: None,
                tool_input: None,
                tool_result: None,
                prompt: None,
                session_id,
                cwd,
            },
        )
        .await;
    }
}

/// Run PostCompact hooks.
pub async fn run_post_compact_hooks(hooks: &HooksConfig, session_id: &str, cwd: &std::path::Path) {
    for hook in &hooks.post_compact {
        execute_hook(
            hook,
            HookEnvVars {
                event: "PostCompact",
                tool_name: None,
                tool_input: None,
                tool_result: None,
                prompt: None,
                session_id,
                cwd,
            },
        )
        .await;
    }
}

// ── Internal ──────────────────────────────────────────────────────────────��───

struct HookEnvVars<'a> {
    event: &'a str,
    tool_name: Option<&'a str>,
    tool_input: Option<&'a str>,
    tool_result: Option<&'a str>,
    prompt: Option<&'a str>,
    session_id: &'a str,
    cwd: &'a std::path::Path,
}

/// Wall-clock bound on a single hook. Hooks sit on the critical path of every
/// tool call, so one that waits on input, a network call, or a lock would
/// otherwise block the agent indefinitely with no diagnostic.
const HOOK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// Cap on captured stdout/stderr per stream. The pipe is still drained past
/// this point so the hook can exit rather than blocking on a full pipe.
const MAX_HOOK_OUTPUT_BYTES: usize = 256 * 1024;

/// Cap on a single env var handed to a hook.
///
/// Linux limits one env entry to ~128 KB (`MAX_ARG_STRLEN`). A large
/// `TOOL_INPUT` — a big Write, a long diff — would push `spawn` over that and
/// fail with E2BIG. Combined with the old fail-open behaviour that meant a
/// PreToolUse gate was *silently skipped precisely on the largest tool calls*.
/// Truncating keeps the hook running on the inputs that matter most.
const MAX_HOOK_ENV_BYTES: usize = 64 * 1024;

/// Does this event's result actually gate anything?
///
/// Only `PreToolUse` can block a tool call, so it is the only event where a
/// failure to *evaluate* the hook is a security-relevant outcome. For every
/// other event there is nothing to gate — a notification or post-hoc hook that
/// fails is genuinely non-blocking, and failing closed there would break
/// sessions for no safety benefit.
fn is_gating_event(event: &str) -> bool {
    event == "PreToolUse"
}

/// A hook that could not be evaluated.
///
/// For a gating event this **fails closed**: a gate that did not run has not
/// approved anything, and the previous behaviour (return `allow()` after a
/// `tracing::warn!` the user never sees in the TUI) meant a broken or
/// missing PreToolUse hook silently disabled itself.
fn hook_unevaluable(event: &str, hook: &HookEntry, why: &str) -> HookResult {
    if is_gating_event(event) {
        tracing::error!("Blocking: PreToolUse hook '{}' {}", hook.command, why);
        HookResult {
            should_continue: false,
            stop_reason: Some(format!(
                "PreToolUse hook could not be evaluated: it {why}.\n  \
                 Hook: {}\n\
                 Blocking the tool call — a hook that cannot run has not approved it. \
                 Fix or remove the hook in settings.json.",
                hook.command
            )),
            ..Default::default()
        }
    } else {
        tracing::warn!("Hook '{}' {} (non-blocking event)", hook.command, why);
        HookResult::allow()
    }
}

/// Truncate an env value to stay under the per-entry limit, on a char boundary.
fn cap_env_value(v: &str) -> String {
    if v.len() <= MAX_HOOK_ENV_BYTES {
        return v.to_string();
    }
    let cut = (0..=MAX_HOOK_ENV_BYTES)
        .rev()
        .find(|&i| v.is_char_boundary(i))
        .unwrap_or(0);
    format!("{}…[truncated by rustyclaw]", &v[..cut])
}

/// Read a pipe to EOF, keeping at most `cap` bytes.
///
/// Draining past the cap matters: if we stopped reading, the hook would block
/// writing to a full pipe and only die at the timeout, turning a fast hook into
/// a 60-second stall.
async fn read_capped<R>(reader: &mut R, cap: usize) -> std::io::Result<String>
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt;
    let mut buf = vec![0u8; 8192];
    let mut kept: Vec<u8> = Vec::new();
    loop {
        let n = reader.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        if kept.len() < cap {
            let room = cap - kept.len();
            kept.extend_from_slice(&buf[..room.min(n)]);
        }
    }
    Ok(String::from_utf8_lossy(&kept).into_owned())
}

async fn execute_hook(hook: &HookEntry, env: HookEnvVars<'_>) -> HookResult {
    use tokio::process::Command;

    let shell = std::env::var("SHELL").unwrap_or_else(|_| "sh".into());

    let mut cmd = Command::new(&shell);
    cmd.arg("-c").arg(&hook.command);
    cmd.current_dir(env.cwd);

    // Standard env vars
    cmd.env("CLAUDE_HOOK_EVENT", env.event);
    cmd.env("CLAUDE_SESSION_ID", env.session_id);
    cmd.env("CLAUDE_CWD", env.cwd.to_string_lossy().as_ref());

    if let Some(name) = env.tool_name {
        cmd.env("TOOL_NAME", name);
    }
    if let Some(inp) = env.tool_input {
        cmd.env("TOOL_INPUT", cap_env_value(inp));
    }
    if let Some(res) = env.tool_result {
        cmd.env("TOOL_RESULT", cap_env_value(res));
    }
    if let Some(msg) = env.prompt {
        cmd.env("CLAUDE_MESSAGE", cap_env_value(msg));
    }

    // Capture stdout and stderr. stdin is /dev/null: inherited stdin would let
    // a hook that reads input compete with the TUI for the user's keystrokes
    // and hang until the timeout.
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    cmd.kill_on_drop(true);
    // Own process group so a timeout can take out anything the hook spawned,
    // rather than leaving orphans reparented to init.
    #[cfg(unix)]
    cmd.process_group(0);

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return hook_unevaluable(env.event, hook, &format!("failed to start: {e}")),
    };

    #[cfg(unix)]
    let pgid = child.id().map(|id| id as i32);

    let Some(mut child_stdout) = child.stdout.take() else {
        return hook_unevaluable(env.event, hook, "produced no stdout pipe");
    };
    let Some(mut child_stderr) = child.stderr.take() else {
        return hook_unevaluable(env.event, hook, "produced no stderr pipe");
    };

    // Read both pipes concurrently. Draining one to EOF before starting the
    // other deadlocks if the hook fills the second pipe first.
    let collect = async {
        let (out, err) = tokio::join!(
            read_capped(&mut child_stdout, MAX_HOOK_OUTPUT_BYTES),
            read_capped(&mut child_stderr, MAX_HOOK_OUTPUT_BYTES),
        );
        let status = child.wait().await?;
        Ok::<_, std::io::Error>((status, out?, err?))
    };

    let (status, stdout, stderr) = match tokio::time::timeout(HOOK_TIMEOUT, collect).await {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => {
            return hook_unevaluable(env.event, hook, &format!("could not be read: {e}"));
        }
        Err(_) => {
            // SAFETY: libc::kill with a negative pid signals the whole process
            // group. Unsafe only because of FFI; the pid is one we just spawned.
            #[cfg(unix)]
            if let Some(pgid) = pgid {
                unsafe {
                    libc::kill(-pgid, libc::SIGKILL);
                }
            }
            return hook_unevaluable(
                env.event,
                hook,
                &format!("timed out after {}s", HOOK_TIMEOUT.as_secs()),
            );
        }
    };

    // A hook killed by a signal (OOM killer, external SIGKILL) has no exit
    // code. `unwrap_or(0)` previously read that as success — a second silent
    // fail-open, and the one an attacker would reach for.
    let Some(exit_code) = status.code() else {
        return hook_unevaluable(env.event, hook, "was killed by a signal");
    };

    if !stderr.is_empty() {
        tracing::debug!("Hook stderr: {stderr}");
    }

    // Exit code 2 = blocking error — always blocks regardless of stdout content.
    // If stdout is JSON, extract a human-readable reason from it instead of dumping raw JSON.
    if exit_code == 2 {
        let trimmed = stdout.trim();
        let stop_reason = if trimmed.starts_with('{') {
            if let Ok(hook_out) = serde_json::from_str::<HookOutput>(trimmed) {
                hook_out.stop_reason.or(hook_out.reason).unwrap_or_else(|| {
                    format!("Hook '{}' blocked execution (exit 2)", hook.command)
                })
            } else {
                // Malformed JSON — show raw so the hook author can debug
                trimmed.to_string()
            }
        } else if trimmed.is_empty() {
            format!("Hook '{}' blocked execution (exit 2)", hook.command)
        } else {
            trimmed.to_string()
        };
        return HookResult {
            should_continue: false,
            stop_reason: Some(stop_reason),
            ..Default::default()
        };
    }

    // Non-zero (not 2) = non-blocking error, log and continue
    if exit_code != 0 {
        tracing::warn!(
            "Hook '{}' exited with code {} (non-blocking)",
            hook.command,
            exit_code
        );
        return HookResult::allow();
    }

    // Parse JSON output from stdout if present
    let trimmed = stdout.trim();
    if trimmed.starts_with('{')
        && let Ok(hook_out) = serde_json::from_str::<HookOutput>(trimmed)
    {
        let decision = match hook_out.decision.as_deref() {
            Some("approve") => Some(HookDecision::Approve),
            Some("block") => Some(HookDecision::Block),
            _ => None,
        };

        if !hook_out.continue_ {
            return HookResult {
                should_continue: false,
                stop_reason: hook_out
                    .stop_reason
                    .or(hook_out.reason)
                    .or_else(|| Some(format!("Hook '{}' requested stop", hook.command))),
                system_message: hook_out.system_message,
                decision,
                additional_context: hook_out.additional_context,
            };
        }

        return HookResult {
            should_continue: true,
            stop_reason: None,
            system_message: hook_out.system_message,
            decision,
            additional_context: hook_out.additional_context,
        };
    }

    HookResult::allow()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::{HookEntry, HooksConfig};

    fn entry(command: &str) -> HookEntry {
        HookEntry {
            matcher: String::new(),
            command: command.to_string(),
        }
    }

    fn cfg_pre(command: &str) -> HooksConfig {
        HooksConfig {
            pre_tool_use: vec![entry(command)],
            ..Default::default()
        }
    }

    fn cfg_post(command: &str) -> HooksConfig {
        HooksConfig {
            post_tool_use: vec![entry(command)],
            ..Default::default()
        }
    }

    // ── Which events fail closed ─────────────────────────────────────────────

    #[test]
    fn only_pre_tool_use_gates() {
        assert!(is_gating_event("PreToolUse"));
        for e in [
            "PostToolUse",
            "UserPromptSubmit",
            "Notification",
            "Stop",
            "SessionStart",
            "PreCompact",
            "PostCompact",
        ] {
            assert!(!is_gating_event(e), "{e} does not gate a tool call");
        }
    }

    /// A gate that could not run has not approved anything.
    #[test]
    fn unevaluable_gating_hook_blocks() {
        let r = hook_unevaluable("PreToolUse", &entry("/bin/broken"), "failed to start: ENOENT");
        assert!(!r.should_continue, "PreToolUse must fail closed");
        let reason = r.stop_reason.expect("must explain why it blocked");
        assert!(reason.contains("/bin/broken"), "{reason}");
        assert!(reason.contains("has not approved"), "{reason}");
    }

    /// Nothing to gate — failing closed here would break sessions for no gain.
    #[test]
    fn unevaluable_non_gating_hook_allows() {
        let r = hook_unevaluable("PostToolUse", &entry("/bin/broken"), "timed out");
        assert!(r.should_continue, "non-gating events stay non-blocking");
        assert!(r.stop_reason.is_none());
    }

    // ── Signal-killed hooks ──────────────────────────────────────────────────

    /// `status.code()` is None when a process dies by signal. The old
    /// `unwrap_or(0)` read that as exit 0 — success — so a PreToolUse gate
    /// killed by the OOM killer (or anything else) silently allowed the call.
    ///
    /// Unix-only: Windows has no POSIX signal termination and `ExitStatus::code()`
    /// there always returns `Some`, so neither the bug nor this test applies.
    #[cfg(unix)]
    #[tokio::test]
    async fn signal_killed_gating_hook_blocks() {
        let r = run_pre_tool_hooks(
            &cfg_pre("kill -9 $$"),
            "Bash",
            "{}",
            "sess",
            std::path::Path::new("."),
        )
        .await;
        assert!(
            !r.should_continue,
            "a signal-killed PreToolUse hook must not be read as approval"
        );
        let reason = r.stop_reason.unwrap_or_default();
        assert!(reason.contains("signal"), "reason should say why: {reason}");
    }

    /// The same failure on a non-gating event is still non-blocking.
    #[cfg(unix)]
    #[tokio::test]
    async fn signal_killed_non_gating_hook_is_tolerated() {
        // Must simply return without blocking anything.
        run_post_tool_hooks(
            &cfg_post("kill -9 $$"),
            "Bash",
            "ok",
            "sess",
            std::path::Path::new("."),
        )
        .await;
    }

    // ── Documented exit-code contract is preserved ───────────────────────────

    #[tokio::test]
    async fn exit_zero_allows() {
        let r = run_pre_tool_hooks(
            &cfg_pre("exit 0"),
            "Bash",
            "{}",
            "sess",
            std::path::Path::new("."),
        )
        .await;
        assert!(r.should_continue);
    }

    #[tokio::test]
    async fn exit_two_blocks_with_reason() {
        let r = run_pre_tool_hooks(
            &cfg_pre("echo 'nope, dangerous' >&2; exit 2"),
            "Bash",
            "{}",
            "sess",
            std::path::Path::new("."),
        )
        .await;
        assert!(!r.should_continue, "exit 2 is the documented block signal");
    }

    /// Documented contract: a non-zero exit other than 2 is a *non-blocking*
    /// error. Preserved deliberately — fail-closed applies to hooks that could
    /// not be evaluated, not to hooks that ran and reported failure.
    #[tokio::test]
    async fn other_nonzero_exit_stays_non_blocking() {
        let r = run_pre_tool_hooks(
            &cfg_pre("exit 127"),
            "Bash",
            "{}",
            "sess",
            std::path::Path::new("."),
        )
        .await;
        assert!(r.should_continue, "exit 127 is documented as non-blocking");
    }

    // ── Resource bounds ──────────────────────────────────────────────────────

    /// A hook emitting far more than the cap must still complete promptly —
    /// capping without draining would leave it blocked on a full pipe until
    /// the 60s timeout.
    #[tokio::test]
    async fn large_hook_output_does_not_stall() {
        let start = std::time::Instant::now();
        let r = run_pre_tool_hooks(
            &cfg_pre("head -c 4000000 /dev/zero | tr '\\0' 'a'"),
            "Bash",
            "{}",
            "sess",
            std::path::Path::new("."),
        )
        .await;
        assert!(r.should_continue);
        assert!(
            start.elapsed() < std::time::Duration::from_secs(20),
            "took {:?} — the pipe is not being drained",
            start.elapsed()
        );
    }

    /// An oversized TOOL_INPUT previously pushed `spawn` past the per-entry env
    /// limit (E2BIG), which under the old fail-open meant the gate was skipped
    /// exactly on the biggest tool calls.
    #[tokio::test]
    async fn oversized_tool_input_still_runs_the_hook() {
        let huge = "x".repeat(2 * 1024 * 1024);
        let r = run_pre_tool_hooks(
            &cfg_pre("test -n \"$TOOL_INPUT\" && exit 2"),
            "Bash",
            &huge,
            "sess",
            std::path::Path::new("."),
        )
        .await;
        assert!(
            !r.should_continue,
            "hook must still receive TOOL_INPUT and be able to block"
        );
        // Distinguish "the hook ran and blocked" from "spawn failed and the new
        // fail-closed path caught it" — both set should_continue=false, so
        // asserting that alone would pass even with the env cap removed.
        let reason = r.stop_reason.unwrap_or_default();
        assert!(
            !reason.contains("could not be evaluated"),
            "the hook must actually have run, not been rescued by fail-closed: {reason}"
        );
    }

    /// Direct check that oversized values are handed to the process at a size
    /// it will accept, independent of how the hook reports its decision.
    #[tokio::test]
    async fn oversized_env_reaches_the_hook_truncated() {
        let huge = "x".repeat(2 * 1024 * 1024);
        // Echo the length the hook actually observed; exit 2 carries it back
        // through stop_reason.
        let r = run_pre_tool_hooks(
            &cfg_pre("echo \"len=${#TOOL_INPUT}\"; exit 2"),
            "Bash",
            &huge,
            "sess",
            std::path::Path::new("."),
        )
        .await;
        let reason = r.stop_reason.unwrap_or_default();
        assert!(reason.contains("len="), "hook did not run: {reason}");
        assert!(
            !reason.contains(&format!("len={}", huge.len())),
            "value should have been truncated before spawn: {reason}"
        );
    }

    #[test]
    fn env_values_are_capped_on_a_char_boundary() {
        let small = "hello";
        assert_eq!(cap_env_value(small), small);

        let big = "é".repeat(MAX_HOOK_ENV_BYTES);
        let capped = cap_env_value(&big);
        assert!(capped.len() <= MAX_HOOK_ENV_BYTES + 64, "len {}", capped.len());
        assert!(capped.contains("truncated"));
        // Round-trips as valid UTF-8 (would have panicked on a bad slice).
        assert!(!capped.is_empty());
    }
}
