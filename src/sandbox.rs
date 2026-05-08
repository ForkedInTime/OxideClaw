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

pub fn firejail_wrap(command: &str, cwd: &std::path::Path) -> String {
    let cwd_quoted = shell_quote(&cwd.display().to_string());
    format!(
        "firejail --quiet --private-tmp --noroot --chdir={cwd} -- /bin/sh -c {cmd}",
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
            Ok(firejail_wrap(command, cwd))
        }
        _ => Ok(command.to_string()),
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
