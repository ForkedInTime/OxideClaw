/// Sandbox execution wrapper for the Bash tool.
///
/// Two categories of mode, and the difference matters:
///
///   **Isolation** — the kernel enforces the boundary.
///     bwrap    — bubblewrap namespaces, read-only system mounts (Linux only)
///     firejail — firejail profiles (Linux only)
///
///   **Best-effort pattern blocking** — no enforcement, just a blocklist.
///     strict   — substring match against a list of catastrophic commands
///
/// `strict` is **not a sandbox**. It is a small denylist of literal substrings
/// and it is trivially bypassed — `rm -fr /`, `rm  -rf /` (two spaces),
/// `$(echo rm) -rf /`, or any base64/variable indirection all walk straight
/// past it. It catches fat-finger accidents, not an adversary, and it cannot
/// restrict filesystem or network access at all.
///
/// This distinction is load-bearing because **neither bwrap nor firejail exists
/// on macOS or Windows**, so `best_available_mode()` returns `strict` there.
/// On those platforms "sandbox enabled" means pattern matching and nothing
/// more. [`isolation_available`] reports whether real isolation is obtainable,
/// and the UI must say so rather than implying protection that isn't there.
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

/// Can this machine actually isolate a command, or only pattern-match it?
///
/// False on macOS and Windows (no bwrap, no firejail) and on any Linux box
/// without them installed. Callers must use this to avoid telling the user they
/// are sandboxed when the only thing standing between the model and their
/// filesystem is a substring denylist.
pub fn isolation_available() -> bool {
    bwrap_available() || firejail_available()
}

/// Does this mode enforce a boundary, or is it best-effort only?
pub fn mode_enforces_isolation(mode: &str) -> bool {
    matches!(mode, "bwrap" | "firejail")
}

/// Warning to show when a mode cannot enforce anything. `None` when the active
/// mode really does isolate.
pub fn weak_mode_warning(mode: &str) -> Option<String> {
    if mode_enforces_isolation(mode) {
        return None;
    }
    let why = if isolation_available() {
        "Real isolation IS available on this machine — prefer `/sandbox enable bwrap` \
         (or firejail)."
    } else if cfg!(target_os = "linux") {
        "No isolation backend is installed. Install one for real containment: \
         `sudo apt install bubblewrap` (or firejail)."
    } else {
        "Neither bubblewrap nor firejail exists on this platform, so no isolation \
         backend is available at all."
    };
    Some(format!(
        "strict mode is a best-effort denylist, NOT isolation. It matches literal \
         substrings and is trivially bypassed (`rm -fr /`, `$(echo rm) -rf /`, \
         variable indirection). It cannot restrict filesystem or network access.\n  \
         {why}"
    ))
}

// ── Strict mode: pattern-based blocking ──────────────────────────────────────

/// Returns Some(reason) if the command matches a dangerous pattern.
///
/// **Best-effort denylist, not a security boundary.** This is a case-insensitive
/// substring match over a fixed list. It stops the exact literal forms below and
/// nothing else — every one of these gets through:
///
/// ```text
/// rm -fr /                  flag order
/// rm  -rf /                 extra whitespace
/// rm --recursive --force /  long flags
/// $(echo rm) -rf /          command substitution
/// X="rm -rf /"; $X          variable indirection
/// echo cm0gLXJmIC8= | base64 -d | sh
/// ```
///
/// Denylists cannot be made complete; do not add patterns expecting to close
/// the gap. Its job is catching an accidental catastrophic command, and it runs
/// in every mode as a cheap second layer. Actual containment comes from
/// bwrap/firejail — see [`isolation_available`].
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
        let kind = if mode_enforces_isolation(mode) {
            "isolation"
        } else {
            "pattern blocking only — NOT isolation"
        };
        format!("ENABLED  [mode: {mode} — {kind}]")
    } else {
        "DISABLED".to_string()
    };

    // State the platform's actual ceiling rather than letting the mode list
    // imply every option is equivalent.
    let ceiling = if isolation_available() {
        String::new()
    } else {
        format!(
            "\n\
             ⚠ No isolation backend on this machine{}.\n  \
             The only available mode is `strict`, which is a best-effort denylist:\n  \
             it matches literal substrings, is trivially bypassed, and cannot restrict\n  \
             filesystem or network access. Treat it as a guard against accidents, not\n  \
             against an adversary.\n",
            if cfg!(any(target_os = "macos", target_os = "windows")) {
                " (bubblewrap and firejail are Linux-only)"
            } else {
                ""
            }
        )
    };

    format!(
        "Sandbox  {status}\n\
         {ceiling}\n\
         Modes:\n\
           bwrap    — kernel namespace isolation   [{bwrap}]\n\
           firejail — kernel namespace isolation   [{fjail}]\n\
           strict   — best-effort denylist, no isolation (always available)\n\
         \n\
         Commands:\n\
           /sandbox enable            — enable (auto-selects the strongest available)\n\
           /sandbox enable bwrap      — bubblewrap isolation\n\
           /sandbox enable firejail   — firejail isolation\n\
           /sandbox enable strict     — denylist only\n\
           /sandbox disable           — disable\n\
         \n\
         When enabled, all Bash tool calls go through the selected mode.\n\
         The `strict` denylist is also applied in bwrap and firejail mode as a\n\
         second layer, but it is never the thing doing the containment.",
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

    // ── Honesty about what `strict` actually does ────────────────────────────

    /// `strict` is the automatic fallback wherever no isolation backend exists
    /// — which is *always* on macOS and Windows. The UI must not describe it in
    /// terms that imply containment.
    #[test]
    fn strict_is_not_described_as_isolation() {
        assert!(!mode_enforces_isolation("strict"));
        assert!(mode_enforces_isolation("bwrap"));
        assert!(mode_enforces_isolation("firejail"));

        let status = sandbox_status(true, "strict");
        assert!(
            status.contains("NOT isolation"),
            "status must say strict is not isolation: {status}"
        );
        assert!(
            !status.contains("catastrophic patterns regardless"),
            "the old overclaim must not come back: {status}"
        );
    }

    /// Enabling is when the user forms a belief about how protected they are.
    #[test]
    fn weak_mode_warns_and_strong_mode_does_not() {
        let w = weak_mode_warning("strict").expect("strict must warn");
        assert!(w.contains("NOT isolation"), "{w}");
        assert!(w.contains("trivially bypassed"), "{w}");

        assert!(weak_mode_warning("bwrap").is_none());
        assert!(weak_mode_warning("firejail").is_none());
    }

    /// The denylist is documented as best-effort precisely because these get
    /// through. Pinning them stops anyone "fixing" it by adding more literals
    /// and believing the gap is closed.
    #[test]
    fn known_bypasses_are_not_caught_and_that_is_expected() {
        for bypass in [
            "rm -fr /",
            "rm  -rf /",
            "rm --recursive --force /",
            "$(echo rm) -rf /",
            // Indirection only evades when the literal is never spelled out —
            // `X=\"rm -rf /\"; $X` *is* caught, because the substring is right
            // there in the assignment. Split it and the denylist is blind.
            "A=rm; B=-rf; $A $B /",
            "rm -r -f /",
        ] {
            assert!(
                strict_check(bypass).is_none(),
                "denylist is not expected to catch {bypass:?} — if this now passes, \
                 the docs claiming best-effort need revisiting, not celebrating"
            );
        }
        // The literal forms it does catch still work.
        assert!(strict_check("rm -rf /").is_some());
        assert!(strict_check("RM -RF /").is_some(), "matching is case-insensitive");
    }

    #[test]
    fn isolation_availability_matches_backend_presence() {
        assert_eq!(
            isolation_available(),
            bwrap_available() || firejail_available()
        );
        // best_available_mode only returns a weak mode when nothing can isolate.
        if isolation_available() {
            assert!(mode_enforces_isolation(best_available_mode()));
        } else {
            assert_eq!(best_available_mode(), "strict");
        }
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
