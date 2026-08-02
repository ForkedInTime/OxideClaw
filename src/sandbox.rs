/// Sandbox execution wrapper for the Bash tool.
///
/// Three modes:
///   strict  — pattern-based blocking of destructive commands (no external deps)
///   bwrap   — bubblewrap (Linux namespaces, read-only system mounts)
///   firejail— firejail profile-based sandboxing
///
/// Mode selection: `/sandbox enable [strict|bwrap|firejail]`
/// The active mode is stored in config.sandbox_mode and applied by BashTool.
use std::process::Command;

// ── Availability checks ───────────────────────────────────────────────────────

pub fn bwrap_available() -> bool {
    Command::new("bwrap")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

pub fn firejail_available() -> bool {
    Command::new("firejail")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

pub fn best_available_mode() -> &'static str {
    if bwrap_available() {
        "bwrap"
    } else if firejail_available() {
        "firejail"
    } else {
        "strict"
    }
}

// ── Strict mode: pattern-based blocking ──────────────────────────────────────

/// Returns Some(reason) if the command matches a dangerous pattern.
/// This runs before the command is executed in strict mode.
pub fn strict_check(cmd: &str) -> Option<String> {
    let low = cmd.to_lowercase();
    let patterns: &[(&str, &str)] = &[
        ("rm -rf /", "Recursive delete of root filesystem"),
        ("rm -rf /*", "Recursive delete of root filesystem"),
        ("mkfs", "Filesystem format command"),
        ("dd if=/dev/zero of=/dev/", "Disk overwrite"),
        ("dd if=/dev/urandom of=/dev/", "Disk overwrite"),
        (":(){ :|:& };:", "Fork bomb"),
        (":(){:|:&};:", "Fork bomb"),
        ("> /dev/sda", "Disk overwrite via redirect"),
        ("chmod -R 000 /", "Remove all permissions from root"),
        ("chmod -R 777 /", "Dangerous permission change on root"),
        (":() { :|: & };", "Fork bomb variant"),
        (
            "sudo rm -rf /",
            "Recursive delete of root filesystem (sudo)",
        ),
    ];
    for (pattern, desc) in patterns {
        if low.contains(pattern) {
            return Some(format!(
                "Blocked by strict sandbox: {} (matched '{}')",
                desc, pattern
            ));
        }
    }
    None
}

// ── bwrap (bubblewrap) wrapper ────────────────────────────────────────────────

/// Wrap a shell command string in a bubblewrap sandbox.
/// The sandbox:
///   - Mounts /usr, /lib, /lib64, /bin, /sbin as read-only
///   - Binds the current working directory as read-write
///   - Binds /tmp as read-write (tmpfs)
///   - Uses --unshare-net to block network (configurable)
///   - Uses --unshare-pid for process isolation
///   - Uses --die-with-parent so cleanup is automatic
pub fn bwrap_wrap(command: &str, cwd: &std::path::Path, allow_network: bool) -> String {
    let cwd_quoted = shell_quote(&cwd.display().to_string());
    let net_flag = if allow_network { "" } else { "--unshare-net " };

    format!(
        "bwrap \
         --ro-bind /usr /usr \
         --ro-bind /lib /lib \
         --ro-bind-try /lib64 /lib64 \
         --ro-bind-try /lib32 /lib32 \
         --ro-bind /bin /bin \
         --ro-bind /sbin /sbin \
         --ro-bind-try /etc/ssl /etc/ssl \
         --ro-bind-try /etc/resolv.conf /etc/resolv.conf \
         --ro-bind-try /etc/passwd /etc/passwd \
         --bind {cwd} {cwd} \
         --tmpfs /tmp \
         --proc /proc \
         --dev /dev \
         --chdir {cwd} \
         {net_flag}\
         --unshare-pid \
         --die-with-parent \
         -- /bin/sh -c {shell_quoted}",
        cwd = cwd_quoted,
        net_flag = net_flag,
        shell_quoted = shell_quote(command),
    )
}

// ── firejail wrapper ──────────────────────────────────────────────────────────

pub fn firejail_wrap(command: &str, cwd: &std::path::Path, allow_network: bool) -> String {
    let cwd_quoted = shell_quote(&cwd.display().to_string());
    // `--net=none` is firejail's equivalent of bwrap's `--unshare-net`. Without
    // it, firejail mode silently ignored `sandbox_allow_network` and always had
    // full egress, so the same setting meant different things in the two modes.
    let net_flag = if allow_network { "" } else { "--net=none " };
    format!(
        "firejail --quiet --private-tmp --noroot {net_flag}--chdir={cwd} -- /bin/sh -c {cmd}",
        net_flag = net_flag,
        cwd = cwd_quoted,
        cmd = shell_quote(command),
    )
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Apply sandboxing to a command string based on the active mode.
/// Returns (final_command, error_message_if_blocked).
pub fn apply_sandbox(
    command: &str,
    mode: &str,
    cwd: &std::path::Path,
    allow_network: bool,
) -> Result<String, String> {
    match mode {
        "strict" => {
            if let Some(reason) = strict_check(command) {
                return Err(reason);
            }
            Ok(command.to_string())
        }
        "bwrap" => {
            if let Some(reason) = strict_check(command) {
                return Err(reason);
            }
            if !bwrap_available() {
                return Err(
                    "bwrap not found. Install with: sudo apt install bubblewrap  \
                     or switch mode: /sandbox enable strict"
                        .into(),
                );
            }
            Ok(bwrap_wrap(command, cwd, allow_network))
        }
        "firejail" => {
            if let Some(reason) = strict_check(command) {
                return Err(reason);
            }
            if !firejail_available() {
                return Err(
                    "firejail not found. Install with: sudo apt install firejail  \
                     or switch mode: /sandbox enable strict"
                        .into(),
                );
            }
            Ok(firejail_wrap(command, cwd, allow_network))
        }
        // Fail CLOSED on an unrecognised mode. `ctx.sandbox_mode` is only `Some`
        // when the sandbox is enabled, so reaching this arm means the configured
        // mode string is invalid — a typo or a stale value in settings.json,
        // which (unlike `/sandbox enable`) does not validate the field.
        //
        // Returning the command unchanged here used to run it fully unsandboxed
        // AND skip `strict_check`, while the UI still reported the sandbox as
        // enabled. A security control that silently does nothing is worse than
        // one that is off, so refuse the command and name the bad value.
        other => Err(format!(
            "Sandbox is enabled but the configured mode '{other}' is not recognised. \
             Valid modes: strict, bwrap, firejail. Refusing to run the command \
             unsandboxed — fix `sandboxMode` in settings.json or run: /sandbox enable strict"
        )),
    }
}

/// Sandbox gate for command-executing tools that the namespace wrappers cannot
/// wrap. `bwrap_wrap` / `firejail_wrap` hard-code `/bin/sh -c`, so routing a
/// PowerShell command through them would hand the script to `sh` and change its
/// meaning entirely.
///
/// Pattern blocking still applies in every mode. For the namespace modes there
/// is no correct wrapping, so this fails closed: better to refuse than to run
/// outside the sandbox the user believes is active.
pub fn guard_unwrappable_tool(command: &str, mode: &str, tool: &str) -> Result<(), String> {
    if let Some(reason) = strict_check(command) {
        return Err(reason);
    }
    match mode {
        "strict" => Ok(()),
        other => Err(format!(
            "The {tool} tool cannot be sandboxed under mode '{other}' — the {other} \
             wrapper executes through /bin/sh, which would not run a PowerShell \
             script correctly. Refusing rather than running it unsandboxed. \
             Use /sandbox enable strict, or use the Bash tool instead."
        )),
    }
}

/// Status display for /sandbox command
pub fn sandbox_status(enabled: bool, mode: &str) -> String {
    let bwrap = if bwrap_available() {
        "✓ available"
    } else {
        "✗ not installed"
    };
    let fjail = if firejail_available() {
        "✓ available"
    } else {
        "✗ not installed"
    };

    let status = if enabled {
        format!("ENABLED  [mode: {}]", mode)
    } else {
        "DISABLED".to_string()
    };

    format!(
        "Sandbox  {status}\n\
         \n\
         Modes:\n\
           strict   — pattern-based blocking (always available)\n\
           bwrap    — bubblewrap namespaces   [{bwrap}]\n\
           firejail — firejail profiles       [{fjail}]\n\
         \n\
         Commands:\n\
           /sandbox enable            — enable (auto-selects best mode)\n\
           /sandbox enable strict     — enable strict pattern blocking\n\
           /sandbox enable bwrap      — enable bubblewrap sandboxing\n\
           /sandbox enable firejail   — enable firejail sandboxing\n\
           /sandbox disable           — disable sandboxing\n\
         \n\
         When enabled, all Bash tool calls run inside the sandbox.\n\
         Strict mode blocks fork-bombs, disk overwrites, and other\n\
         catastrophic patterns regardless of sandbox mode.",
    )
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// An unrecognised mode used to return the command unchanged — running it
    /// fully unsandboxed, skipping `strict_check`, while the UI still reported
    /// the sandbox as enabled. Reachable via an unvalidated `sandboxMode` in
    /// settings.json.
    #[test]
    fn unknown_mode_fails_closed() {
        let err = apply_sandbox("echo hi", "strict-typo", Path::new("/tmp"), false)
            .expect_err("an unrecognised mode must not run the command unsandboxed");
        assert!(err.contains("strict-typo"), "error should name the bad mode: {err}");
        assert!(err.contains("strict"), "error should list valid modes: {err}");
    }

    #[test]
    fn known_modes_still_pass_through() {
        let out = apply_sandbox("echo hi", "strict", Path::new("/tmp"), false)
            .expect("strict mode is valid");
        assert_eq!(out, "echo hi");
    }

    #[test]
    fn strict_mode_still_blocks_dangerous_patterns() {
        assert!(apply_sandbox("rm -rf /", "strict", Path::new("/tmp"), false).is_err());
    }

    /// `firejail_wrap` ignored `allow_network` entirely, so firejail mode always
    /// had full egress while bwrap mode honoured the setting — the same config
    /// meaning two different things.
    #[test]
    fn firejail_honours_network_setting() {
        let blocked = firejail_wrap("echo hi", Path::new("/tmp"), false);
        assert!(blocked.contains("--net=none"), "network must be blocked: {blocked}");

        let allowed = firejail_wrap("echo hi", Path::new("/tmp"), true);
        assert!(!allowed.contains("--net=none"), "network must be allowed: {allowed}");
    }

    #[test]
    fn bwrap_and_firejail_agree_on_network_policy() {
        let bw = bwrap_wrap("echo hi", Path::new("/tmp"), false);
        let fj = firejail_wrap("echo hi", Path::new("/tmp"), false);
        assert!(bw.contains("--unshare-net"));
        assert!(fj.contains("--net=none"));
    }

    #[test]
    fn shell_quote_escapes_embedded_single_quotes() {
        assert_eq!(shell_quote("it's"), r#"'it'\''s'"#);
        let wrapped = firejail_wrap("echo 'pwn'", Path::new("/tmp/a b"), true);
        assert!(wrapped.contains(r#"'/tmp/a b'"#), "cwd must stay quoted: {wrapped}");
    }

    /// PowerShell cannot be wrapped by the namespace modes (they exec /bin/sh),
    /// so the gate must refuse rather than run it outside the active sandbox.
    #[test]
    fn unwrappable_tool_gate_fails_closed_on_namespace_modes() {
        assert!(guard_unwrappable_tool("Get-ChildItem", "strict", "PowerShell").is_ok());

        for mode in ["bwrap", "firejail"] {
            let err = guard_unwrappable_tool("Get-ChildItem", mode, "PowerShell")
                .unwrap_err_or_else_msg();
            assert!(err.contains(mode), "error should name the mode: {err}");
        }
    }

    #[test]
    fn unwrappable_tool_gate_applies_pattern_blocking_in_every_mode() {
        for mode in ["strict", "bwrap", "firejail"] {
            assert!(
                guard_unwrappable_tool("rm -rf /", mode, "PowerShell").is_err(),
                "dangerous pattern must be blocked under {mode}"
            );
        }
    }

    trait UnwrapErrMsg {
        fn unwrap_err_or_else_msg(self) -> String;
    }
    impl UnwrapErrMsg for Result<(), String> {
        fn unwrap_err_or_else_msg(self) -> String {
            self.expect_err("expected the gate to refuse")
        }
    }
}
