/// PowerShellTool — port of powershell.ts
/// Runs a PowerShell command via `pwsh` and returns stdout + stderr.
use super::{Tool, ToolContext, ToolOutput, async_trait};
use anyhow::Result;
use serde::Deserialize;
use serde_json::json;

pub struct PowerShellTool;

#[derive(Deserialize)]
struct Input {
    command: String,
    #[serde(default = "default_timeout")]
    timeout_ms: u64,
}

fn default_timeout() -> u64 {
    30_000
}

#[async_trait]
impl Tool for PowerShellTool {
    fn name(&self) -> &str {
        "PowerShell"
    }

    fn description(&self) -> &str {
        "Execute a PowerShell command using `pwsh`. Returns combined stdout and stderr. \
        Only available when PowerShell Core (pwsh) is installed."
    }

    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The PowerShell command or script to execute"
                },
                "timeout_ms": {
                    "type": "integer",
                    "description": "Timeout in milliseconds (default 30 000, max 120 000)",
                    "default": 30000,
                    "minimum": 1000,
                    "maximum": 120000
                }
            },
            "required": ["command"]
        })
    }

    async fn execute(&self, input: serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let input: Input = serde_json::from_value(input)?;
        let timeout_ms = input.timeout_ms.min(120_000);

        // This tool executes arbitrary commands exactly like Bash, so it must
        // honour the sandbox. It previously bypassed it entirely — `apply_sandbox`
        // was called from bash.rs and nowhere else — so an enabled sandbox had no
        // effect here at all.
        if let Some(ref mode) = ctx.sandbox_mode
            && let Err(reason) =
                crate::sandbox::guard_unwrappable_tool(&input.command, mode, "PowerShell")
        {
            return Ok(ToolOutput::error(reason));
        }

        use super::bash::{BACKGROUND_NOTE, Drained, MAX_OUTPUT_BYTES, read_to_cap};
        use tokio::process::Command;
        use tokio::time::{Duration, Instant, timeout_at};

        // Parity with the Bash tool, which this had drifted from on two counts:
        //
        //  1. `Command::output()` reads both pipes to EOF with no cap, so a
        //     runaway command exhausts memory — the same OOM class already
        //     fixed for Bash.
        //  2. Nothing killed the child on timeout. Dropping an `output()` future
        //     does not kill the process unless `kill_on_drop` is set, so a
        //     timed-out command (and anything it spawned) kept running forever.
        //
        // Uses the same ProcessGroupGuard as Bash so the whole subtree dies,
        // and the same exit-aware drain so a background job neither blocks
        // until the timeout nor costs the output read so far.
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
        let mut truncated = false;
        let fut = async {
            let mut cmd = Command::new("pwsh");
            cmd.args(["-NoProfile", "-NonInteractive", "-Command", &input.command])
                .current_dir(&ctx.cwd)
                // Inherited stdin lets an interactive prompt fight the TUI for
                // the user's keystrokes.
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .kill_on_drop(true)
                .envs(&ctx.env);
            #[cfg(unix)]
            super::bash::new_session(cmd.as_std_mut());

            let mut guard = super::bash::ProcessGroupGuard::new(cmd.spawn()?);
            let mut child_out = guard
                .child_mut()
                .stdout
                .take()
                .ok_or_else(|| std::io::Error::other("no stdout pipe"))?;
            let mut child_err = guard
                .child_mut()
                .stderr
                .take()
                .ok_or_else(|| std::io::Error::other("no stderr pipe"))?;

            // Read both concurrently — draining one to EOF first deadlocks if
            // the command fills the other pipe.
            let mut err_truncated = false;
            let reads = async {
                let (o, e) = tokio::join!(
                    read_to_cap(
                        &mut child_out,
                        MAX_OUTPUT_BYTES,
                        &mut stdout,
                        &mut truncated
                    ),
                    read_to_cap(
                        &mut child_err,
                        MAX_OUTPUT_BYTES,
                        &mut stderr,
                        &mut err_truncated
                    ),
                );
                o.and(e)
            };
            let (drained, status) =
                super::bash::drain_until_exit(reads, guard.child_mut(), deadline).await?;
            truncated |= err_truncated;
            let status = match (drained, status) {
                (Drained::TimedOut, _) => None,
                (_, Some(st)) => Some(st),
                (_, None) => timeout_at(deadline, guard.child_mut().wait())
                    .await
                    .ok()
                    .transpose()?,
            };
            if status.is_some() {
                guard.disarm();
            }
            if drained == Drained::ShellExited {
                super::bash::discard_rest(child_out);
                super::bash::discard_rest(child_err);
            }
            Ok::<_, std::io::Error>((status, drained))
        };

        let result = fut.await;
        let mut stdout = String::from_utf8_lossy(&stdout).into_owned();
        let stderr = String::from_utf8_lossy(&stderr).into_owned();

        match result {
            Ok((None, _)) => {
                if !stderr.is_empty() {
                    stdout.push_str("\n[stderr]\n");
                    stdout.push_str(&stderr);
                }
                if !stdout.is_empty() && !stdout.ends_with('\n') {
                    stdout.push('\n');
                }
                stdout.push_str(&format!(
                    "PowerShell command timed out after {timeout_ms} ms."
                ));
                Ok(ToolOutput::error(stdout))
            }
            Err(e) => {
                if e.kind() == std::io::ErrorKind::NotFound {
                    Ok(ToolOutput::error(
                        "pwsh not found. Install PowerShell Core to use this tool.",
                    ))
                } else {
                    Ok(ToolOutput::error(format!("Failed to run pwsh: {e}")))
                }
            }
            Ok((Some(status), drained)) => {
                let mut out = String::new();
                if !stdout.is_empty() {
                    out.push_str(&stdout);
                }
                if !stderr.is_empty() {
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    out.push_str("[stderr]\n");
                    out.push_str(&stderr);
                }
                if truncated {
                    out.push_str("\n... (output truncated)");
                }
                if drained == Drained::ShellExited {
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    out.push_str(BACKGROUND_NOTE);
                }
                if out.is_empty() {
                    out = format!("(exit code {})", status.code().unwrap_or(-1));
                }

                let is_error = !status.success();
                if is_error {
                    Ok(ToolOutput::error(out))
                } else {
                    Ok(ToolOutput::success(out))
                }
            }
        }
    }
}
