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
///   { "continue": false, "stopReason": "...", "decision": "block",
///     "systemMessage": "...", "reason": "..." }
/// `"decision": "approve"` parses but grants nothing: hooks can only block,
/// and an approved call still goes through the permission gate.
/// Claude Code's `{"hookSpecificOutput": {"permissionDecision": "deny",
/// "permissionDecisionReason": "...", "additionalContext": "..."}}` is read
/// too: `deny` blocks, and so does `ask`, since a hook cannot force a prompt
/// here and full-auto or an allow rule would otherwise run the call.
///
/// Exit codes:
///   0   — success (allow, continue)
///   2   — blocking error (block tool/continue, show stopReason, stdout or
///         stderr)
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
    #[serde(rename = "hookSpecificOutput", default)]
    hook_specific_output: Option<HookSpecificOutput>,
}

/// Claude Code's per-event output object.
#[derive(Debug, Deserialize, Default)]
struct HookSpecificOutput {
    #[serde(rename = "permissionDecision")]
    permission_decision: Option<String>,
    #[serde(rename = "permissionDecisionReason")]
    permission_decision_reason: Option<String>,
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
        // `{"decision":"block"}` with exit 0 is the documented JSON way to
        // block a tool; it was parsed and then ignored.
        if r.decision == Some(HookDecision::Block) {
            return HookResult {
                should_continue: false,
                stop_reason: r
                    .stop_reason
                    .or_else(|| Some(format!("Blocked by PreToolUse hook '{}'", hook.command))),
                ..r
            };
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

/// Run all UserPromptSubmit hooks. The first hook that stops the prompt (exit
/// 2 or `continue: false`) wins and the caller must not send it; otherwise
/// every hook's context is joined and the last system message is kept.
pub async fn run_user_prompt_hooks(
    hooks: &HooksConfig,
    prompt: &str,
    session_id: &str,
    cwd: &std::path::Path,
) -> HookResult {
    let mut result = HookResult::allow();
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
        if !r.should_continue {
            return r;
        }
        // `{"decision":"block"}` is the documented way for this hook to
        // reject a prompt; it does not set `continue: false`.
        if r.decision == Some(HookDecision::Block) {
            let stop_reason = r.stop_reason.clone().or_else(|| {
                Some(format!(
                    "Blocked by UserPromptSubmit hook '{}'",
                    hook.command
                ))
            });
            return HookResult {
                should_continue: false,
                stop_reason,
                ..r
            };
        }
        if let Some(ctx) = r.additional_context {
            additional.push(ctx);
        }
        if r.system_message.is_some() {
            result.system_message = r.system_message;
        }
    }
    if !additional.is_empty() {
        result.additional_context = Some(additional.join("\n"));
    }
    result
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

/// Run Notification hooks when a turn ends with a text reply.
/// `message` is exported as `CLAUDE_MESSAGE` and `prompt` on stdin.
pub async fn run_notification_hooks(
    hooks: &HooksConfig,
    message: &str,
    session_id: &str,
    cwd: &std::path::Path,
) {
    for hook in &hooks.notification {
        execute_hook(
            hook,
            HookEnvVars {
                event: "Notification",
                tool_name: None,
                tool_input: None,
                tool_result: None,
                prompt: Some(message),
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
    format!("{}…[truncated by oxideclaw]", &v[..cut])
}

/// The JSON object written to a hook's stdin, with every value uncapped.
/// Field names follow Claude Code's hook input so existing hooks port over.
fn hook_stdin_payload(env: &HookEnvVars<'_>) -> String {
    let mut obj = serde_json::json!({
        "hook_event_name": env.event,
        "session_id": env.session_id,
        "cwd": env.cwd.to_string_lossy(),
    });
    if let Some(name) = env.tool_name {
        obj["tool_name"] = name.into();
    }
    if let Some(inp) = env.tool_input {
        // Callers pass the serialized tool input; hand it back as an object
        // so `jq .tool_input.command` works.
        obj["tool_input"] = serde_json::from_str(inp).unwrap_or_else(|_| inp.into());
    }
    if let Some(res) = env.tool_result {
        obj["tool_response"] = res.into();
    }
    if let Some(msg) = env.prompt {
        obj["prompt"] = msg.into();
    }
    obj.to_string()
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

/// Hooks are written in POSIX shell syntax. Under fish, nu or xonsh a guard
/// fails to parse, exits non-2, and the tool it was meant to block runs, so
/// those shells fall back to `sh`. A POSIX-family `$SHELL` is kept: bash
/// users' `[[ ... ]]` guards would exit 127 (allow) under dash.
fn hook_shell(login_shell: Option<&str>) -> String {
    login_shell
        .filter(|s| {
            let name = std::path::Path::new(s)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("");
            // Git Bash/MSYS hands native programs `...\bash.exe`.
            let name = match name.len().checked_sub(4) {
                Some(i) if name.is_char_boundary(i) && name[i..].eq_ignore_ascii_case(".exe") => {
                    &name[..i]
                }
                _ => name,
            };
            matches!(
                name.to_ascii_lowercase().as_str(),
                "sh" | "bash" | "dash" | "zsh" | "ksh" | "mksh" | "ash" | "yash"
            )
        })
        .unwrap_or("sh")
        .to_string()
}

#[cfg(unix)]
struct KillGroupOnDrop(Option<i32>);

#[cfg(unix)]
impl Drop for KillGroupOnDrop {
    fn drop(&mut self) {
        // SAFETY: libc::kill with a negative pid signals the whole process
        // group. Unsafe only because of FFI; the pid is one we spawned and
        // have not reaped, so it cannot have been reused.
        if let Some(pgid) = self.0 {
            unsafe {
                libc::kill(-pgid, libc::SIGKILL);
            }
        }
    }
}

async fn execute_hook(hook: &HookEntry, env: HookEnvVars<'_>) -> HookResult {
    use tokio::process::Command;

    let shell = hook_shell(std::env::var("SHELL").ok().as_deref());

    let mut cmd = Command::new(&shell);
    cmd.arg("-c").arg(&hook.command);
    cmd.current_dir(env.cwd);

    // Standard env vars
    cmd.env("CLAUDE_HOOK_EVENT", env.event);
    cmd.env("CLAUDE_SESSION_ID", env.session_id);
    cmd.env("CLAUDE_CWD", env.cwd.to_string_lossy().as_ref());
    // Claude Code hooks commonly run `"$CLAUDE_PROJECT_DIR"/.claude/hooks/x.sh`.
    // Unset, that path is `/.claude/...`, the shell exits 127 and an imported
    // guard allows every call. Project settings load from `<cwd>/.claude`, so
    // cwd is the project root here.
    cmd.env("CLAUDE_PROJECT_DIR", env.cwd);
    cmd.env("OXIDECLAW_PROJECT_DIR", env.cwd);

    if let Some(name) = env.tool_name {
        cmd.env("TOOL_NAME", name);
    }
    // A capped value hides its tail, which is exactly where a padded command
    // puts the dangerous part. `<VAR>_TRUNCATED=1` lets an env-only guard fail
    // closed; the full value is always on stdin.
    for (var, value) in [
        ("TOOL_INPUT", env.tool_input),
        ("TOOL_RESULT", env.tool_result),
        ("CLAUDE_MESSAGE", env.prompt),
    ] {
        if let Some(v) = value {
            cmd.env(var, cap_env_value(v));
            if v.len() > MAX_HOOK_ENV_BYTES {
                cmd.env(format!("{var}_TRUNCATED"), "1");
            }
        }
    }
    let payload = hook_stdin_payload(&env);

    // stdin is a pipe carrying the payload rather than inherited: a hook that
    // reads input must never compete with the TUI for the user's keystrokes.
    cmd.stdin(std::process::Stdio::piped());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    cmd.kill_on_drop(true);
    // Own process group so a timeout can take out anything the hook spawned,
    // rather than leaving orphans reparented to init; a new session so a hook
    // that touches /dev/tty fails instead of being stopped as a background job.
    #[cfg(unix)]
    crate::tools::bash::new_session(cmd.as_std_mut());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return hook_unevaluable(env.event, hook, &format!("failed to start: {e}")),
    };

    // Kills the hook's process group if this future is dropped before the
    // hook finished (a timeout, or Esc aborting a prompt's hooks):
    // kill_on_drop alone reaches only the shell, not what it started.
    #[cfg(unix)]
    let mut group = KillGroupOnDrop(child.id().map(|id| id as i32));

    let Some(mut child_stdout) = child.stdout.take() else {
        return hook_unevaluable(env.event, hook, "produced no stdout pipe");
    };
    let Some(mut child_stderr) = child.stderr.take() else {
        return hook_unevaluable(env.event, hook, "produced no stderr pipe");
    };
    let Some(mut child_stdin) = child.stdin.take() else {
        return hook_unevaluable(env.event, hook, "produced no stdin pipe");
    };

    // Feed stdin and read both output pipes concurrently. Doing any of them
    // to completion first deadlocks once the hook fills a pipe we are not
    // servicing yet.
    let collect = async {
        let feed = async move {
            use tokio::io::AsyncWriteExt;
            let r = child_stdin.write_all(payload.as_bytes()).await;
            // Dropping `child_stdin` here sends EOF. A hook that exits without
            // reading stdin closes the pipe, which is fine.
            match r {
                Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
                r => r,
            }
        };
        let (fed, out, err) = tokio::join!(
            feed,
            read_capped(&mut child_stdout, MAX_HOOK_OUTPUT_BYTES),
            read_capped(&mut child_stderr, MAX_HOOK_OUTPUT_BYTES),
        );
        // Errors first: once waited on, the pid is free to be reused, so
        // the group must not be killed after that.
        fed?;
        let (out, err) = (out?, err?);
        let status = child.wait().await?;
        Ok::<_, std::io::Error>((status, out, err))
    };

    let (status, stdout, stderr) = match tokio::time::timeout(HOOK_TIMEOUT, collect).await {
        Ok(Ok(v)) => {
            // Reaped: the pgid may be reused, and what the hook left running
            // in the background is its own business.
            #[cfg(unix)]
            {
                group.0 = None;
            }
            v
        }
        Ok(Err(e)) => {
            return hook_unevaluable(env.event, hook, &format!("could not be read: {e}"));
        }
        // The group is killed as `group` drops.
        Err(_) => {
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
        // Claude Code hooks write the exit-2 reason to stderr.
        let fallback = || {
            Some(stderr.trim())
                .filter(|e| !e.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| format!("Hook '{}' blocked execution (exit 2)", hook.command))
        };
        let stop_reason = if trimmed.starts_with('{') {
            if let Ok(hook_out) = serde_json::from_str::<HookOutput>(trimmed) {
                hook_out
                    .stop_reason
                    .or(hook_out.reason)
                    .or_else(|| {
                        hook_out
                            .hook_specific_output
                            .and_then(|h| h.permission_decision_reason)
                    })
                    .unwrap_or_else(fallback)
            } else {
                // Malformed JSON — show raw so the hook author can debug
                trimmed.to_string()
            }
        } else if trimmed.is_empty() {
            fallback()
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
        let specific = hook_out.hook_specific_output.unwrap_or_default();
        let mut reason = hook_out.reason;
        let mut decision = match hook_out.decision.as_deref() {
            Some("approve") => Some(HookDecision::Approve),
            Some("block") => Some(HookDecision::Block),
            _ => None,
        };
        // An imported Claude Code guard answers in `hookSpecificOutput`.
        // Reading only the top-level fields turned its deny into an allow.
        match specific.permission_decision.as_deref() {
            Some("deny") => {
                decision = Some(HookDecision::Block);
                reason = specific.permission_decision_reason.or(reason);
            }
            Some("ask") => {
                decision = Some(HookDecision::Block);
                let why = specific
                    .permission_decision_reason
                    .or(reason)
                    .unwrap_or_else(|| format!("Hook '{}' asked for confirmation", hook.command));
                reason = Some(format!(
                    "{why} (the hook asked for confirmation, which OxideClaw hooks cannot \
                     request, so the call is blocked)"
                ));
            }
            _ => {}
        }
        let additional_context = hook_out.additional_context.or(specific.additional_context);

        if !hook_out.continue_ {
            return HookResult {
                should_continue: false,
                stop_reason: hook_out
                    .stop_reason
                    .or(reason)
                    .or_else(|| Some(format!("Hook '{}' requested stop", hook.command))),
                system_message: hook_out.system_message,
                decision,
                additional_context,
            };
        }

        // A block carries its `reason` so the model is told why.
        let stop_reason = if decision == Some(HookDecision::Block) {
            reason
        } else {
            None
        };
        return HookResult {
            should_continue: true,
            stop_reason,
            system_message: hook_out.system_message,
            decision,
            additional_context,
        };
    }

    // Plain stdout from a prompt hook is documented as extra context for the
    // model. Other events keep ignoring it.
    if env.event == "UserPromptSubmit" && !trimmed.is_empty() {
        return HookResult {
            should_continue: true,
            additional_context: Some(trimmed.to_string()),
            ..Default::default()
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

    #[test]
    fn hooks_run_under_a_posix_shell_even_for_fish_or_nu_users() {
        for non_posix in ["/usr/bin/fish", "/usr/local/bin/nu", "/usr/bin/xonsh", ""] {
            assert_eq!(hook_shell(Some(non_posix)), "sh", "{non_posix}");
        }
        assert_eq!(hook_shell(None), "sh");
        for posix in ["/bin/bash", "/usr/bin/zsh", "/bin/sh", "/bin/dash"] {
            assert_eq!(hook_shell(Some(posix)), posix);
        }
        // Windows shells carry `.exe`, in any case.
        for exe in ["/usr/bin/bash.exe", "/usr/bin/BASH.EXE"] {
            assert_eq!(hook_shell(Some(exe)), exe);
        }
        assert_eq!(hook_shell(Some("/usr/bin/fish.exe")), "sh");
        assert_eq!(hook_shell(Some("/usr/bin/bash.old")), "sh");
    }

    #[cfg(windows)]
    #[test]
    fn git_bash_on_windows_keeps_its_shell() {
        let p = r"C:\Program Files\Git\usr\bin\bash.exe";
        assert_eq!(hook_shell(Some(p)), p);
    }

    fn cfg_pre(command: &str) -> HooksConfig {
        HooksConfig {
            pre_tool_use: vec![entry(command)],
            ..Default::default()
        }
    }

    /// Only used by the signal-termination test, which is unix-only — so this
    /// helper is dead code on Windows and trips `-D warnings` there.
    #[cfg(unix)]
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
        let r = hook_unevaluable(
            "PreToolUse",
            &entry("/bin/broken"),
            "failed to start: ENOENT",
        );
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

    #[cfg(unix)]
    #[tokio::test]
    async fn json_block_decision_blocks_the_tool() {
        let r = run_pre_tool_hooks(
            &cfg_pre(r#"echo '{"decision":"block","reason":"no pushes"}'"#),
            "Bash",
            "{}",
            "sess",
            std::path::Path::new("."),
        )
        .await;
        assert!(!r.should_continue, "documented: decision block blocks");
        assert_eq!(r.stop_reason.as_deref(), Some("no pushes"));
    }

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

    /// The env copy keeps only the first 64 KiB, so a guard grepping
    /// `$TOOL_INPUT` never saw a command padded past that. stdin carries all
    /// of it, and the env says when it was cut.
    #[cfg(unix)]
    #[tokio::test]
    async fn guard_reading_stdin_sees_the_tail_of_a_padded_input() {
        let padded = serde_json::json!({
            "command": format!("# {}\nrm -rf ~", "x".repeat(200 * 1024)),
        })
        .to_string();
        let r = run_pre_tool_hooks(
            &cfg_pre("grep -q 'rm -rf' && { echo \"cut=$TOOL_INPUT_TRUNCATED\"; exit 2; }; exit 0"),
            "Bash",
            &padded,
            "sess",
            std::path::Path::new("."),
        )
        .await;
        assert!(!r.should_continue, "the guard must see the tail on stdin");
        assert_eq!(r.stop_reason.as_deref(), Some("cut=1"));

        let r = run_pre_tool_hooks(
            &cfg_pre("echo \"[$TOOL_INPUT_TRUNCATED]\"; exit 2"),
            "Bash",
            "{}",
            "sess",
            std::path::Path::new("."),
        )
        .await;
        assert_eq!(
            r.stop_reason.as_deref(),
            Some("[]"),
            "small inputs are not marked"
        );
    }

    #[test]
    fn stdin_payload_carries_full_values_as_json() {
        let big = "y".repeat(MAX_HOOK_ENV_BYTES * 2);
        let input = serde_json::json!({ "command": big }).to_string();
        let payload = hook_stdin_payload(&HookEnvVars {
            event: "PreToolUse",
            tool_name: Some("Bash"),
            tool_input: Some(&input),
            tool_result: None,
            prompt: None,
            session_id: "sess",
            cwd: std::path::Path::new("/w"),
        });
        let v: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(v["hook_event_name"], "PreToolUse");
        assert_eq!(v["tool_name"], "Bash");
        assert_eq!(v["session_id"], "sess");
        assert_eq!(v["cwd"], "/w");
        assert_eq!(
            v["tool_input"]["command"].as_str().unwrap().len(),
            big.len()
        );
        assert!(v.get("tool_response").is_none());
    }

    #[test]
    fn env_values_are_capped_on_a_char_boundary() {
        let small = "hello";
        assert_eq!(cap_env_value(small), small);

        let big = "é".repeat(MAX_HOOK_ENV_BYTES);
        let capped = cap_env_value(&big);
        assert!(
            capped.len() <= MAX_HOOK_ENV_BYTES + 64,
            "len {}",
            capped.len()
        );
        assert!(capped.contains("truncated"));
        // Round-trips as valid UTF-8 (would have panicked on a bad slice).
        assert!(!capped.is_empty());
    }

    // ── userPromptSubmit ─────────────────────────────────────────────────────

    /// `notification` was parsed and documented but nothing ever ran it.
    #[cfg(unix)]
    #[tokio::test]
    async fn notification_hook_receives_the_reply() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = HooksConfig {
            notification: vec![entry(
                "printf '%s|%s' \"$CLAUDE_HOOK_EVENT\" \"$CLAUDE_MESSAGE\" > out.txt",
            )],
            ..Default::default()
        };
        run_notification_hooks(&cfg, "All tests pass.", "sess", dir.path()).await;
        let out = std::fs::read_to_string(dir.path().join("out.txt")).unwrap();
        assert_eq!(out, "Notification|All tests pass.");
    }

    fn cfg_prompt(commands: &[&str]) -> HooksConfig {
        HooksConfig {
            user_prompt_submit: commands.iter().map(|c| entry(c)).collect(),
            ..Default::default()
        }
    }

    async fn prompt_hooks(commands: &[&str]) -> HookResult {
        let dir = tempfile::tempdir().unwrap();
        run_user_prompt_hooks(&cfg_prompt(commands), "hello", "sess", dir.path()).await
    }

    #[tokio::test]
    async fn prompt_hook_plain_stdout_becomes_context() {
        let r = prompt_hooks(&["echo feature/login", "echo '  second  '"]).await;
        assert!(r.should_continue);
        assert_eq!(
            r.additional_context.as_deref(),
            Some("feature/login\nsecond")
        );
    }

    #[tokio::test]
    async fn prompt_hook_exit_2_stops_the_prompt() {
        let r = prompt_hooks(&["echo context", "echo 'contains a secret'; exit 2"]).await;
        assert!(!r.should_continue, "exit 2 must stop the prompt");
        assert_eq!(r.stop_reason.as_deref(), Some("contains a secret"));
    }

    /// `decision: block` was mapped to should_continue = true, so the
    /// prompt was sent anyway.
    #[tokio::test]
    async fn prompt_hook_decision_block_stops_the_prompt() {
        let r = prompt_hooks(&[r#"echo '{"decision":"block","reason":"x"}'"#]).await;
        assert!(!r.should_continue);
        assert_eq!(r.stop_reason.as_deref(), Some("x"));
    }

    #[tokio::test]
    async fn prompt_hook_continue_false_stops_the_prompt() {
        let r = prompt_hooks(&[r#"echo '{"continue":false,"stopReason":"frozen"}'"#]).await;
        assert!(!r.should_continue);
        assert_eq!(r.stop_reason.as_deref(), Some("frozen"));
    }

    /// Claude Code's PreToolUse guards answer in `hookSpecificOutput` with
    /// exit 0; reading only the top-level fields let their deny run the call.
    #[cfg(unix)]
    #[tokio::test]
    async fn claude_code_permission_decision_deny_and_ask_block() {
        let dir = tempfile::tempdir().unwrap();
        let run = |out: &'static str| {
            let path = dir.path().to_path_buf();
            async move {
                let cmd = format!("echo '{out}'");
                run_pre_tool_hooks(&cfg_pre(&cmd), "Bash", "{}", "sess", &path).await
            }
        };
        let r = run(r#"{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":"no force push"}}"#).await;
        assert!(!r.should_continue);
        assert_eq!(r.stop_reason.as_deref(), Some("no force push"));

        let r = run(r#"{"hookSpecificOutput":{"permissionDecision":"ask","permissionDecisionReason":"confirm push"}}"#).await;
        assert!(!r.should_continue, "ask must not run the call unconfirmed");
        assert!(
            r.stop_reason
                .as_deref()
                .unwrap()
                .starts_with("confirm push"),
            "{:?}",
            r.stop_reason
        );

        let r = run(r#"{"hookSpecificOutput":{"permissionDecision":"allow"}}"#).await;
        assert!(r.should_continue);
        assert_eq!(r.decision, None, "allow grants nothing");
    }

    /// Claude Code hooks locate their script through `$CLAUDE_PROJECT_DIR`.
    /// Unset, the shell exited 127 (non-blocking) and the guard allowed the call.
    #[cfg(unix)]
    #[tokio::test]
    async fn claude_project_dir_finds_the_guard_script() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let hooks_dir = dir.path().join(".claude/hooks");
        std::fs::create_dir_all(&hooks_dir).unwrap();
        let guard = hooks_dir.join("guard.sh");
        std::fs::write(&guard, "#!/bin/sh\necho 'guarded' >&2\nexit 2\n").unwrap();
        std::fs::set_permissions(&guard, std::fs::Permissions::from_mode(0o755)).unwrap();
        for var in ["CLAUDE_PROJECT_DIR", "OXIDECLAW_PROJECT_DIR"] {
            let cmd = format!("\"${var}\"/.claude/hooks/guard.sh");
            let r = run_pre_tool_hooks(&cfg_pre(&cmd), "Bash", "{}", "sess", dir.path()).await;
            assert!(!r.should_continue, "{var}: guard did not run");
            assert_eq!(r.stop_reason.as_deref(), Some("guarded"));
        }
    }

    #[tokio::test]
    async fn prompt_hook_specific_additional_context_is_kept() {
        let r = prompt_hooks(&[
            r#"echo '{"hookSpecificOutput":{"hookEventName":"UserPromptSubmit","additionalContext":"branch: main"}}'"#,
        ])
        .await;
        assert!(r.should_continue);
        assert_eq!(r.additional_context.as_deref(), Some("branch: main"));
    }

    /// Claude Code hooks print the exit-2 reason on stderr.
    #[tokio::test]
    async fn exit_two_reason_falls_back_to_stderr() {
        let dir = tempfile::tempdir().unwrap();
        let r = run_pre_tool_hooks(
            &cfg_pre("echo 'nope, dangerous' >&2; exit 2"),
            "Bash",
            "{}",
            "sess",
            dir.path(),
        )
        .await;
        assert!(!r.should_continue);
        assert_eq!(r.stop_reason.as_deref(), Some("nope, dangerous"));
    }

    #[tokio::test]
    async fn plain_stdout_is_still_ignored_for_tool_hooks() {
        let dir = tempfile::tempdir().unwrap();
        let r = run_pre_tool_hooks(&cfg_pre("echo hi"), "Bash", "{}", "sess", dir.path()).await;
        assert!(r.should_continue);
        assert!(r.additional_context.is_none());
    }
}
