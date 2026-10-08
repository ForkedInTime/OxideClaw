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

/// Probed once per process: the old version forked `bwrap --version` on
/// every Bash call.
pub fn bwrap_available() -> bool {
    static CACHE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CACHE.get_or_init(|| probe("bwrap"))
}

pub fn firejail_available() -> bool {
    static CACHE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CACHE.get_or_init(|| probe("firejail"))
}

fn probe(binary: &str) -> bool {
    Command::new(binary)
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
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
        // Patterns are compared lowercased (the `chmod -R` ones never matched
        // lowercased input). One aimed at root (` /`) must hit `/` itself, not
        // any absolute path: a bare substring blocked `rm -rf /tmp/build`.
        // Prefix patterns like `of=/dev/` still match as substrings, since
        // every real target has a device name after the slash.
        let pattern = pattern.to_lowercase();
        let hit = low.match_indices(&pattern).any(|(at, m)| {
            !pattern.ends_with(" /")
                || low[at + m.len()..]
                    .chars()
                    .next()
                    .is_none_or(|c| c.is_whitespace() || matches!(c, ';' | '&' | '|' | '*'))
        });
        if hit {
            return Some(format!(
                "Blocked by strict sandbox: {} (matched '{}')",
                desc, pattern
            ));
        }
    }
    None
}

/// Global options for a git command OxideClaw runs on the host on its own:
/// no fsmonitor, no hooks. A command run without the bwrap sandbox can write
/// `.git/`, and a planted fsmonitor or hook (e.g. `reference-transaction`
/// on our update-ref, `post-checkout` on `worktree add`) would run outside
/// any sandbox. They do not stop `filter.*` drivers; see
/// `autocommit::check_filters_unchanged`.
pub(crate) const GIT_NO_REPO_CODE: [&str; 4] = [
    "-c",
    "core.fsmonitor=false",
    "-c",
    "core.hooksPath=/dev/null",
];

// ── bwrap (bubblewrap) wrapper ────────────────────────────────────────────────

/// Wrap a shell command string in a bubblewrap sandbox.
/// The sandbox:
///   - Mounts /usr, /lib, /lib64, /bin, /sbin as read-only
///   - Mounts the parts of /etc that ordinary tools resolve through, read-only:
///     /etc/alternatives (Debian/Ubuntu route awk, cc, java... through it),
///     /etc/ssl + /etc/pki (Fedora/RHEL keep the CA bundle under pki), the
///     dynamic-linker cache, and the name-service/timezone files
///   - Mounts per-user toolchains (~/.cargo/bin, ~/.rustup, ~/.local/bin,
///     ~/.nvm) read-only so PATH entries pointing at them still resolve, and
///     the package caches their builds read (~/.cargo/registry, ~/.cargo/git,
///     ~/go/pkg/mod, ~/.local/lib), since with the network off nothing can be
///     downloaded again. $CARGO_HOME, $RUSTUP_HOME, $GOMODCACHE and $GOPATH
///     are honoured. The rest of $HOME stays hidden; ~/.cargo itself is not
///     bound because it holds registry credentials.
///   - Mounts a fresh tmpfs on /tmp, then binds the current working directory
///     read-write on top. bwrap applies mounts in argument order, so the cwd
///     bind must come last or a project under /tmp would be buried by the tmpfs
///   - Uses --unshare-net to block network (configurable)
///   - Uses --unshare-pid for process isolation
///   - Uses --new-session to detach from the controlling terminal. Without it
///     the sandboxed process shares our tty and can push characters back into
///     it with TIOCSTI, which the parent shell then executes as if the user had
///     typed them — an escape straight out of the sandbox. Modern kernels
///     default `dev.tty.legacy_tiocsti=0`, but that is a host setting we do not
///     control, so bwrap's own guard is the right place to rely on.
///   - Uses --die-with-parent so cleanup is automatic
pub fn bwrap_wrap(command: &str, cwd: &std::path::Path, allow_network: bool) -> String {
    bwrap_wrap_with_home(
        command,
        cwd,
        allow_network,
        dirs::home_dir().as_deref(),
        &|name| std::env::var_os(name),
    )
}

/// The per-user toolchain and package-cache directories bound read-only.
/// `var` reads an environment variable (a parameter so tests do not depend
/// on the machine's); relative values are ignored.
fn toolchain_dirs(
    home: Option<&std::path::Path>,
    var: &dyn Fn(&str) -> Option<std::ffi::OsString>,
) -> Vec<std::path::PathBuf> {
    use std::path::PathBuf;
    let abs = |name: &str| var(name).map(PathBuf::from).filter(|p| p.is_absolute());
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(h) = home {
        for d in [
            ".cargo/bin",
            ".cargo/registry",
            ".cargo/git",
            ".rustup",
            ".local/bin",
            ".local/lib",
            ".nvm",
            "go/pkg/mod",
        ] {
            dirs.push(h.join(d));
        }
    }
    if let Some(cargo_home) = abs("CARGO_HOME") {
        for d in ["bin", "registry", "git"] {
            dirs.push(cargo_home.join(d));
        }
    }
    dirs.extend(abs("RUSTUP_HOME"));
    dirs.extend(abs("GOMODCACHE"));
    if let Some(gopath) = var("GOPATH") {
        dirs.extend(
            std::env::split_paths(&gopath)
                .filter(|p| p.is_absolute())
                .map(|p| p.join("pkg/mod")),
        );
    }
    let mut seen = std::collections::HashSet::new();
    dirs.retain(|d| seen.insert(d.clone()));
    dirs
}

fn bwrap_wrap_with_home(
    command: &str,
    cwd: &std::path::Path,
    allow_network: bool,
    home: Option<&std::path::Path>,
    var: &dyn Fn(&str) -> Option<std::ffi::OsString>,
) -> String {
    let cwd_quoted = shell_quote(&cwd.display().to_string());
    let net_flag = if allow_network { "" } else { "--unshare-net " };
    let home_binds: String = toolchain_dirs(home, var)
        .iter()
        .map(|d| {
            let p = shell_quote(&d.display().to_string());
            format!("--ro-bind-try {p} {p} ")
        })
        .collect();

    format!(
        "bwrap \
         --ro-bind /usr /usr \
         --ro-bind /lib /lib \
         --ro-bind-try /lib64 /lib64 \
         --ro-bind-try /lib32 /lib32 \
         --ro-bind /bin /bin \
         --ro-bind /sbin /sbin \
         --ro-bind-try /etc/alternatives /etc/alternatives \
         --ro-bind-try /etc/ssl /etc/ssl \
         --ro-bind-try /etc/pki /etc/pki \
         --ro-bind-try /etc/ca-certificates /etc/ca-certificates \
         --ro-bind-try /etc/ld.so.cache /etc/ld.so.cache \
         --ro-bind-try /etc/resolv.conf /etc/resolv.conf \
         --ro-bind-try /etc/hosts /etc/hosts \
         --ro-bind-try /etc/nsswitch.conf /etc/nsswitch.conf \
         --ro-bind-try /etc/localtime /etc/localtime \
         --ro-bind-try /etc/passwd /etc/passwd \
         --ro-bind-try /etc/group /etc/group \
         --tmpfs /tmp \
         {home_binds}\
         --bind {cwd} {cwd} \
         {host_run_binds}\
         --proc /proc \
         --dev /dev \
         --chdir {cwd} \
         {net_flag}\
         --unshare-pid \
         --new-session \
         --die-with-parent \
         -- {shell} -c {shell_quoted}",
        shell = sandbox_shell(),
        cwd = cwd_quoted,
        home_binds = home_binds,
        host_run_binds = host_run_binds(cwd),
        net_flag = net_flag,
        shell_quoted = shell_quote(command),
    )
}

/// Read-only binds, after the project's read-write one, over the files in
/// it that code outside the sandbox runs (`HOST_RUN_PATHS`: git hooks and
/// config, the agent's project config, hook-manager config), so a command
/// cannot plant a hook or fsmonitor that the user's next `git commit` runs
/// unsandboxed. `.git` itself is bound first: a mount point cannot be
/// renamed or replaced, so the protected files cannot be swapped out from
/// above. A symlink is skipped: bwrap would mount over its target.
fn host_run_binds(cwd: &std::path::Path) -> String {
    let real = |p: &std::path::Path| std::fs::symlink_metadata(p).is_ok_and(|m| !m.is_symlink());
    let mut out = String::new();
    let git = cwd.join(".git");
    if real(&git) && git.is_dir() {
        let q = shell_quote(&git.display().to_string());
        out.push_str(&format!("--bind {q} {q} "));
    }
    for rel in crate::permissions::autonomy::HOST_RUN_PATHS {
        let p = cwd.join(rel);
        // A parent symlink (`.git/hooks` under a linked `.git`) as well.
        let parent_real = p.parent().is_none_or(|d| d == cwd || real(d));
        if real(&p) && parent_real {
            let q = shell_quote(&p.display().to_string());
            out.push_str(&format!("--ro-bind-try {q} {q} "));
        }
    }
    out
}

/// The shell inside the namespace sandboxes: bash, the Bash tool's
/// contract, or `/bin/sh` on hosts without it (Alpine/BusyBox), where a
/// hard-coded `bash` failed every command.
pub fn sandbox_shell() -> &'static str {
    if crate::tools::bash::has_bash() {
        "bash"
    } else {
        "/bin/sh"
    }
}

// ── firejail wrapper ──────────────────────────────────────────────────────────

// firejail has no `--chdir` (it rejects unknown options and exits 1); the
// jail inherits the caller's cwd, which bash.rs sets with `current_dir`.
pub fn firejail_wrap(command: &str, _cwd: &std::path::Path, allow_network: bool) -> String {
    // `--net=none` is firejail's equivalent of bwrap's `--unshare-net`. Without
    // it, firejail mode silently ignored `sandbox_allow_network` and always had
    // full egress, so the same setting meant different things in the two modes.
    let net_flag = if allow_network { "" } else { "--net=none " };
    format!(
        "firejail --quiet --private-tmp --noroot {net_flag}-- {shell} -c {cmd}",
        shell = sandbox_shell(),
        net_flag = net_flag,
        cmd = shell_quote(command),
    )
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Whether `apply_sandbox` wraps the command in a POSIX-quoted
/// `bwrap ... -- bash -c '<cmd>'` string. Keep in step with its arms.
pub fn wraps_in_shell(mode: &str) -> bool {
    matches!(mode, "bwrap" | "firejail")
}

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
            // A repo without `.git/hooks` would let a command create it
            // with a hook in it; git makes it on `init` anyway.
            let git = cwd.join(".git");
            if git.is_dir() && !git.is_symlink() {
                let _ = std::fs::create_dir(git.join("hooks"));
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

/// The variables that carry OxideClaw's own credentials: the Anthropic
/// chain, every OpenAI-compatible provider's key and the Whisper key.
pub fn credential_env_keys() -> Vec<&'static str> {
    use crate::api::openai_compat::{PROVIDERS, provider_key_envs};
    let mut keys = vec![
        "ANTHROPIC_API_KEY",
        "ANTHROPIC_AUTH_TOKEN",
        "XAI_API_KEY",
        "WHISPER_API_KEY",
    ];
    keys.extend(PROVIDERS.iter().flat_map(provider_key_envs));
    keys.sort_unstable();
    keys.dedup();
    keys
}

/// Keep OxideClaw's credentials out of a command run under the sandbox. The
/// sandbox binds no `.env`, so the inherited environment was the only way
/// in, and the user who turned the sandbox on did so to contain a command
/// the model chose. Variables set after this (settings.json `env`) still
/// reach it: those the user named for commands on purpose.
pub fn scrub_credentials(cmd: &mut Command) {
    for key in credential_env_keys() {
        cmd.env_remove(key);
    }
}

/// Sandbox gate for command-executing tools that the namespace wrappers cannot
/// wrap. `bwrap_wrap` / `firejail_wrap` hard-code `bash -c`, so routing a
/// PowerShell command through them would hand the script to bash and change its
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
             wrapper executes through bash, which would not run a PowerShell \
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

pub(crate) fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// A sandboxed command inherited every provider key, so an injected
    /// `curl -d "$ANTHROPIC_API_KEY" ...` sent it out without a prompt.
    #[cfg(unix)]
    #[test]
    fn scrubbed_commands_see_no_provider_credentials() {
        let keys = credential_env_keys();
        for k in [
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_AUTH_TOKEN",
            "OPENAI_API_KEY",
            "GROQ_API_KEY",
            "OPENROUTER_API_KEY",
            "GEMINI_API_KEY",
            "GOOGLE_API_KEY",
            "WHISPER_API_KEY",
        ] {
            assert!(keys.contains(&k), "{k}");
        }
        assert!(!keys.contains(&"PATH") && !keys.contains(&""));

        let mut cmd = Command::new("sh");
        cmd.args(["-c", "printf '%s|%s' \"${GROQ_API_KEY-unset}\" \"$KEEP\""])
            .env("GROQ_API_KEY", "gsk-leak")
            .env("KEEP", "kept");
        scrub_credentials(&mut cmd);
        let out = cmd.output().unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout), "unset|kept");
    }

    /// An unrecognised mode used to return the command unchanged — running it
    /// fully unsandboxed, skipping `strict_check`, while the UI still reported
    /// the sandbox as enabled. Reachable via an unvalidated `sandboxMode` in
    /// settings.json.
    #[test]
    fn unknown_mode_fails_closed() {
        let err = apply_sandbox("echo hi", "strict-typo", Path::new("/tmp"), false)
            .expect_err("an unrecognised mode must not run the command unsandboxed");
        assert!(
            err.contains("strict-typo"),
            "error should name the bad mode: {err}"
        );
        assert!(
            err.contains("strict"),
            "error should list valid modes: {err}"
        );
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
        assert!(
            blocked.contains("--net=none"),
            "network must be blocked: {blocked}"
        );

        let allowed = firejail_wrap("echo hi", Path::new("/tmp"), true);
        assert!(
            !allowed.contains("--net=none"),
            "network must be allowed: {allowed}"
        );
    }

    /// The Bash tool's commands are bash. /bin/sh is dash on Debian/Ubuntu,
    /// where `source` and `[[ ]]` exit 127 inside the sandbox.
    #[test]
    fn namespace_wrappers_run_commands_with_bash() {
        let bw = bwrap_wrap("echo hi", Path::new("/tmp"), true);
        let fj = firejail_wrap("echo hi", Path::new("/tmp"), true);
        let want = if crate::tools::bash::has_bash() {
            "-- bash -c 'echo hi'"
        } else {
            "-- /bin/sh -c 'echo hi'"
        };
        for w in [&bw, &fj] {
            assert!(w.contains(want), "{w}");
        }
    }

    #[test]
    fn bwrap_and_firejail_agree_on_network_policy() {
        let bw = bwrap_wrap("echo hi", Path::new("/tmp"), false);
        let fj = firejail_wrap("echo hi", Path::new("/tmp"), false);
        assert!(bw.contains("--unshare-net"));
        assert!(fj.contains("--net=none"));
    }

    /// bwrap applies mounts in argument order. The cwd bind used to come
    /// before `--tmpfs /tmp`, so a project under /tmp was buried by the tmpfs
    /// and every sandboxed command died with "Can't chdir".
    #[test]
    fn bwrap_binds_cwd_after_tmp_tmpfs() {
        let cmd = bwrap_wrap_with_home(
            "ls",
            Path::new("/tmp/proj"),
            true,
            Some(Path::new("/tmp/home")),
            &no_env,
        );
        let tmpfs = cmd.find("--tmpfs /tmp ").expect("tmpfs on /tmp");
        let bind = cmd
            .find("--bind '/tmp/proj' '/tmp/proj'")
            .expect("cwd bind");
        assert!(tmpfs < bind, "cwd bind must follow the /tmp tmpfs: {cmd}");
        let home = cmd
            .find("--ro-bind-try '/tmp/home/.cargo/bin'")
            .expect("toolchain bind");
        assert!(
            tmpfs < home && home < bind,
            "a $HOME under /tmp must not be hidden, and cwd must win: {cmd}"
        );
    }

    /// The project bind left `.git/hooks` and `.git/config` writable, so a
    /// command full-auto pre-approved could plant a hook or fsmonitor that
    /// the user's next `git commit` ran outside the sandbox.
    #[test]
    fn bwrap_rebinds_host_run_files_read_only_after_the_project() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join(".git/hooks")).unwrap();
        std::fs::write(root.join(".git/config"), "").unwrap();
        std::fs::write(root.join(".mcp.json"), "{}").unwrap();
        std::fs::create_dir(root.join(".claude")).unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(elsewhere.path(), root.join(".husky")).unwrap();
        let cmd = bwrap_wrap_with_home("ls", root, false, None, &no_env);
        let at = |what: &str| {
            cmd.find(what)
                .unwrap_or_else(|| panic!("{what} missing: {cmd}"))
        };
        let q = |rel: &str| shell_quote(&root.join(rel).display().to_string());
        let project = at(&format!(
            "--bind {r} {r} ",
            r = shell_quote(&root.display().to_string())
        ));
        let git = at(&format!("--bind {g} {g} ", g = q(".git")));
        assert!(project < git, "{cmd}");
        for rel in [".git/hooks", ".git/config", ".mcp.json", ".claude"] {
            assert!(
                git < at(&format!("--ro-bind-try {p} {p} ", p = q(rel))),
                "{rel}: {cmd}"
            );
        }
        // Missing paths and symlinks get no bind.
        assert!(!cmd.contains(".githooks"), "{cmd}");
        assert!(!cmd.contains(".husky"), "{cmd}");
    }

    /// Run in a real bwrap where one works: the planted hook, the rewritten
    /// config and a `.git` swapped for one of the command's own all fail.
    #[test]
    fn a_bwrap_command_cannot_plant_git_hooks_or_config() {
        let usable = bwrap_available()
            && std::process::Command::new("bwrap")
                .args(["--ro-bind", "/", "/", "true"])
                .status()
                .is_ok_and(|s| s.success());
        if !usable {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir(root.join(".git")).unwrap();
        std::fs::write(root.join(".git/config"), "[core]\n").unwrap();
        let script = "printf x > .git/hooks/post-checkout; \
                      printf '[core]\\nfsmonitor=evil\\n' > .git/config; \
                      mv .git .git.old; \
                      printf ok > notes.txt";
        let wrapped = apply_sandbox(script, "bwrap", root, false).unwrap();
        let out = std::process::Command::new("sh")
            .args(["-c", &wrapped])
            .output()
            .unwrap();
        let log = String::from_utf8_lossy(&out.stderr);
        assert_eq!(
            std::fs::read_to_string(root.join("notes.txt")).unwrap(),
            "ok",
            "{log}"
        );
        assert!(!root.join(".git/hooks/post-checkout").exists(), "{log}");
        assert_eq!(
            std::fs::read_to_string(root.join(".git/config")).unwrap(),
            "[core]\n"
        );
        assert!(
            root.join(".git").is_dir() && !root.join(".git.old").exists(),
            "{log}"
        );
    }

    /// Debian routes awk/cc/java through /etc/alternatives and Fedora keeps the
    /// CA bundle under /etc/pki; without them common commands and TLS fail.
    #[test]
    fn bwrap_exposes_alternatives_pki_and_user_toolchains() {
        let cmd = bwrap_wrap_with_home(
            "awk 1",
            Path::new("/work"),
            true,
            Some(Path::new("/home/o'neil")),
            &no_env,
        );
        for needed in [
            "--ro-bind-try /etc/alternatives /etc/alternatives",
            "--ro-bind-try /etc/pki /etc/pki",
            "--ro-bind-try /etc/ld.so.cache /etc/ld.so.cache",
            "--ro-bind-try '/home/o'\\''neil/.rustup' '/home/o'\\''neil/.rustup'",
            "--ro-bind-try '/home/o'\\''neil/.cargo/bin'",
        ] {
            assert!(cmd.contains(needed), "missing `{needed}` in: {cmd}");
        }
        assert!(
            !cmd.contains("neil/.cargo' "),
            "~/.cargo holds registry credentials and must stay hidden: {cmd}"
        );
        let no_home = bwrap_wrap_with_home("true", Path::new("/work"), true, None, &no_env);
        assert!(!no_home.contains(".rustup"));
    }

    fn no_env(_: &str) -> Option<std::ffi::OsString> {
        None
    }

    /// With the network off nothing can be downloaded, so a Rust, Go or
    /// user-site Python build needs its package cache; auto-fix's `cargo
    /// test` failed on every edit of any project with dependencies.
    #[test]
    fn bwrap_exposes_package_caches_read_only() {
        let home = Path::new("/home/dev");
        let cmd =
            bwrap_wrap_with_home("cargo test", Path::new("/work"), false, Some(home), &no_env);
        for dir in [".cargo/registry", ".cargo/git", "go/pkg/mod", ".local/lib"] {
            let p = format!("'/home/dev/{dir}'");
            assert!(
                cmd.contains(&format!("--ro-bind-try {p} {p} ")),
                "missing {dir} in: {cmd}"
            );
        }
        assert!(!cmd.contains("--bind '/home/dev/.cargo"), "{cmd}");

        let env = |name: &str| -> Option<std::ffi::OsString> {
            match name {
                "CARGO_HOME" => Some("/opt/cargo".into()),
                "RUSTUP_HOME" => Some("/opt/rustup".into()),
                "GOMODCACHE" => Some("/opt/gomod".into()),
                "GOPATH" => Some("/opt/go:relative/go".into()),
                _ => None,
            }
        };
        let cmd = bwrap_wrap_with_home("go test", Path::new("/work"), false, None, &env);
        for dir in [
            "/opt/cargo/bin",
            "/opt/cargo/registry",
            "/opt/cargo/git",
            "/opt/rustup",
            "/opt/gomod",
            "/opt/go/pkg/mod",
        ] {
            assert!(
                cmd.contains(&format!("--ro-bind-try '{dir}' '{dir}' ")),
                "missing {dir} in: {cmd}"
            );
        }
        assert!(!cmd.contains("relative"), "{cmd}");
        assert!(!cmd.contains("'/opt/cargo' "), "{cmd}");
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
        assert!(strict_check("rm -rf / --no-preserve-root").is_some());
        assert!(strict_check("sudo rm -rf /*").is_some());
        assert!(strict_check("chmod -R 777 /").is_some());
        assert!(strict_check("dd if=/dev/zero of=/dev/sda").is_some());
        assert!(strict_check("dd if=/dev/urandom of=/dev/nvme0n1 bs=1M").is_some());
        assert!(strict_check("chmod -R 777 /srv/x").is_none());
        assert!(strict_check("rm -rf /tmp/build").is_none());
        assert!(strict_check("rm -rf /home/u/proj/target && ls").is_none());
        assert!(
            strict_check("RM -RF /").is_some(),
            "matching is case-insensitive"
        );
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
        assert!(
            wrapped.contains(r#"'echo '\''pwn'\'''"#),
            "command must stay quoted: {wrapped}"
        );
        assert!(
            !wrapped.contains("--chdir"),
            "firejail has no --chdir option: {wrapped}"
        );
    }

    /// PowerShell cannot be wrapped by the namespace modes (they exec bash),
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

#[cfg(test)]
mod availability_cache_tests {
    #[test]
    fn availability_is_stable_within_a_process() {
        assert_eq!(super::bwrap_available(), super::bwrap_available());
        assert_eq!(super::firejail_available(), super::firejail_available());
    }
}
