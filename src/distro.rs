/// Distro detection and package manager helpers.
///
/// Used by /doctor and /install-missing to show and run the correct
/// install commands for the user's Linux distribution.
use std::path::Path;

// ── Distro detection ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub enum Distro {
    Arch,
    Debian, // Ubuntu, Debian, Mint, Pop!_OS, …
    Fedora, // Fedora, RHEL, CentOS, AlmaLinux, Rocky, …
    OpenSuse,
    Unknown,
}

impl Distro {
    /// Detect the running distro by checking release files and available package managers.
    pub fn detect() -> Self {
        // Check /etc/os-release for ID field — most reliable
        if let Some(d) = std::fs::read_to_string("/etc/os-release")
            .ok()
            .and_then(|content| Self::from_os_release(&content))
        {
            return d;
        }
        // Fallback: check for well-known release files
        if Path::new("/etc/arch-release").exists() {
            return Distro::Arch;
        }
        if Path::new("/etc/debian_version").exists() {
            return Distro::Debian;
        }
        if Path::new("/etc/fedora-release").exists() {
            return Distro::Fedora;
        }
        if Path::new("/etc/SuSE-release").exists() {
            return Distro::OpenSuse;
        }
        // Last resort: check for package manager binaries in PATH
        if which("pacman") {
            return Distro::Arch;
        }
        if which("apt-get") {
            return Distro::Debian;
        }
        if which("dnf") {
            return Distro::Fedora;
        }
        if which("zypper") {
            return Distro::OpenSuse;
        }
        Distro::Unknown
    }

    /// Classify by os-release `ID`, then `ID_LIKE` (e.g. `ID=zorin`,
    /// `ID_LIKE="ubuntu debian"`).
    fn from_os_release(content: &str) -> Option<Self> {
        let field = |key: &str| {
            content
                .lines()
                .find_map(|l| l.strip_prefix(key)?.strip_prefix('='))
                .map(|v| {
                    v.trim()
                        .trim_matches(|c| c == '"' || c == '\'')
                        .to_lowercase()
                })
        };
        [field("ID"), field("ID_LIKE")]
            .into_iter()
            .flatten()
            .find_map(|id| Self::from_id(&id))
    }

    fn from_id(id: &str) -> Option<Self> {
        if id.contains("arch")
            || id.contains("manjaro")
            || id.contains("endeavour")
            || id.contains("garuda")
            || id.contains("artix")
        {
            return Some(Distro::Arch);
        }
        if id.contains("debian")
            || id.contains("ubuntu")
            || id.contains("mint")
            || id.contains("pop")
            || id.contains("elementary")
            || id.contains("kali")
        {
            return Some(Distro::Debian);
        }
        // Oracle Linux's ID is the bare token `ol`; a substring check also
        // matched `solus` and handed it `dnf`.
        if id.contains("fedora")
            || id.contains("rhel")
            || id.contains("centos")
            || id.contains("alma")
            || id.contains("rocky")
            || id.split_whitespace().any(|t| t == "ol")
        {
            return Some(Distro::Fedora);
        }
        if id.contains("opensuse") || id.contains("suse") {
            return Some(Distro::OpenSuse);
        }
        None
    }

    /// Human-readable distro name for display.
    pub fn name(&self) -> &'static str {
        match self {
            Distro::Arch => "Arch Linux",
            Distro::Debian => "Debian/Ubuntu",
            Distro::Fedora => "Fedora/RHEL",
            Distro::OpenSuse => "openSUSE",
            Distro::Unknown => "Linux",
        }
    }
}

/// Looks `cmd` up on PATH in-process. Shelling out to `which` reported every
/// tool missing where `which` itself is not installed (Arch `base`, minimal
/// Fedora/RHEL images), so /install-missing reinstalled present packages and
/// voice/TTS refused to start.
pub(crate) fn which(cmd: &str) -> bool {
    which_in(cmd, std::env::var_os("PATH").as_deref())
}

fn which_in(cmd: &str, path: Option<&std::ffi::OsStr>) -> bool {
    crate::autofix::find_on_path(cmd, path).is_some()
}

// ── Package manager ───────────────────────────────────────────────────────────

/// Returns the base install command (without package names) for the distro.
/// On Arch, prefers yay > paru > sudo pacman. `None` when the package manager
/// is unknown: guessing `apt` handed Alpine, NixOS, Void and macOS a command
/// that /install-missing would run and that could only fail.
pub fn install_prefix(distro: &Distro) -> Option<String> {
    Some(match distro {
        Distro::Arch => {
            if which("yay") {
                return Some("yay -S --noconfirm".into());
            }
            if which("paru") {
                return Some("paru -S --noconfirm".into());
            }
            "sudo pacman -S --noconfirm".into()
        }
        Distro::Debian => "sudo apt install -y".into(),
        Distro::Fedora => "sudo dnf install -y".into(),
        Distro::OpenSuse => "sudo zypper install -y".into(),
        Distro::Unknown => return None,
    })
}

// ── Tool → package name mapping ───────────────────────────────────────────────

/// A tool we check for and may need to install.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[allow(dead_code)] // variants for /doctor completeness, not all checked at startup
pub enum Tool {
    Arecord,    // voice input (recording)
    Ffmpeg,     // voice input (alternative recorder / audio processing)
    Sox,        // voice input (alternative recorder)
    Aplay,      // TTS playback
    Mpv,        // TTS playback
    Ffplay,     // TTS playback (part of ffmpeg)
    Bwrap,      // bubblewrap sandbox
    Firejail,   // firejail sandbox
    WlCopy,     // clipboard (Wayland)
    Xclip,      // clipboard (X11)
    NotifySend, // desktop notifications
    Git,        // upgrade check
    Nodejs,     // plugins (npm)
    Npm,        // plugins
    CoquiTts,   // XTTS v2 voice cloning (tts CLI)
}

impl Tool {
    pub fn binary(&self) -> &'static str {
        match self {
            Tool::Arecord => "arecord",
            Tool::Ffmpeg => "ffmpeg",
            Tool::Sox => "sox",
            Tool::Aplay => "aplay",
            Tool::Mpv => "mpv",
            Tool::Ffplay => "ffplay",
            Tool::Bwrap => "bwrap",
            Tool::Firejail => "firejail",
            Tool::WlCopy => "wl-copy",
            Tool::Xclip => "xclip",
            Tool::NotifySend => "notify-send",
            Tool::Git => "git",
            Tool::Nodejs => "node",
            Tool::Npm => "npm",
            Tool::CoquiTts => "tts",
        }
    }

    pub fn description(&self) -> &'static str {
        match self {
            Tool::Arecord => "voice recording (ALSA)",
            Tool::Ffmpeg => "voice recording / audio encoding",
            Tool::Sox => "voice recording (alternative)",
            Tool::Aplay => "TTS audio playback (ALSA)",
            Tool::Mpv => "TTS audio playback",
            Tool::Ffplay => "TTS audio playback",
            Tool::Bwrap => "bubblewrap sandbox (/sandbox bwrap)",
            Tool::Firejail => "firejail sandbox (/sandbox firejail)",
            Tool::WlCopy => "clipboard — Wayland (/copy, /share clip)",
            Tool::Xclip => "clipboard — X11 (/copy, /share clip)",
            Tool::NotifySend => "desktop notifications (/notifications)",
            Tool::Git => "git (upgrade check, /upgrade)",
            Tool::Nodejs => "Node.js (plugin system)",
            Tool::Npm => "npm (plugin install)",
            Tool::CoquiTts => "XTTS v2 — natural TTS (PRIMARY engine)",
        }
    }

    /// Return the system package name for this tool on the given distro.
    /// Returns `None` for tools that are not in the system package manager (pip, binary download, etc.).
    pub fn package(&self, distro: &Distro) -> Option<&'static str> {
        match self {
            Tool::Arecord | Tool::Aplay => Some("alsa-utils"),
            Tool::Ffmpeg | Tool::Ffplay => Some("ffmpeg"),
            Tool::Sox => Some("sox"),
            Tool::Mpv => Some("mpv"),
            Tool::Bwrap => Some("bubblewrap"),
            Tool::Firejail => Some("firejail"),
            Tool::WlCopy => Some("wl-clipboard"),
            Tool::Xclip => Some("xclip"),
            Tool::NotifySend => match distro {
                Distro::Debian => Some("libnotify-bin"),
                _ => Some("libnotify"),
            },
            Tool::Git => Some("git"),
            Tool::Nodejs => Some("nodejs"),
            Tool::Npm => match distro {
                Distro::Arch => None, // npm is bundled with nodejs on Arch
                _ => Some("npm"),
            },
            Tool::CoquiTts => None, // uv tool install TTS (all distros — AUR pkg has broken deps)
        }
    }

    pub fn is_available(&self) -> bool {
        which(self.binary())
    }
}

// ── Missing tool analysis ─────────────────────────────────────────────────────

/// A missing tool with the install instructions for the detected distro.
pub struct MissingTool {
    pub tool: Tool,
    /// System package to install, or None if it's a pip/manual install.
    pub package: Option<&'static str>,
    /// Human-readable install note (shown when no system package exists).
    pub manual_note: Option<String>,
}

/// Check the tools that oxideclaw uses and return those that are missing.
pub fn find_missing(distro: &Distro) -> Vec<MissingTool> {
    let mut missing = Vec::new();

    // Audio recorder (need at least one)
    let has_recorder =
        Tool::Arecord.is_available() || Tool::Ffmpeg.is_available() || Tool::Sox.is_available();
    if !has_recorder {
        missing.push(MissingTool {
            tool: Tool::Arecord,
            package: Tool::Arecord.package(distro),
            manual_note: None,
        });
    }

    // Audio player for TTS (need at least one)
    let has_player = Tool::Aplay.is_available()
        || Tool::Mpv.is_available()
        || Tool::Ffplay.is_available()
        || which("paplay")
        || which("play");
    if !has_player {
        // Suggest aplay (comes with alsa-utils, same as arecord)
        missing.push(MissingTool {
            tool: Tool::Aplay,
            package: Tool::Aplay.package(distro),
            manual_note: None,
        });
        // Also suggest mpv as a good alternative
        missing.push(MissingTool {
            tool: Tool::Mpv,
            package: Tool::Mpv.package(distro),
            manual_note: None,
        });
    }

    // XTTS v2 / Coqui TTS — the only TTS engine
    if !crate::voice::xtts_available() {
        let note = Some(
            "uv tool install TTS --python 3.11 \\\n\
             \x20        --with 'transformers<4.46' --with 'torch<2.6' --with 'torchaudio<2.6'"
                .into(),
        );
        missing.push(MissingTool {
            tool: Tool::CoquiTts,
            package: None,
            manual_note: note,
        });
    }

    // Clipboard (need at least one)
    let has_clip = which("wl-copy") || which("xclip") || which("xsel") || which("pbcopy");
    if !has_clip {
        missing.push(MissingTool {
            tool: Tool::WlCopy,
            package: Tool::WlCopy.package(distro),
            manual_note: None,
        });
        missing.push(MissingTool {
            tool: Tool::Xclip,
            package: Tool::Xclip.package(distro),
            manual_note: None,
        });
    }

    // Optional but recommended tools
    for tool in &[Tool::Bwrap, Tool::Firejail, Tool::NotifySend, Tool::Git] {
        if !tool.is_available() {
            missing.push(MissingTool {
                tool: tool.clone(),
                package: tool.package(distro),
                manual_note: None,
            });
        }
    }

    missing
}

/// The distinct system packages behind `missing`, sorted.
pub fn system_packages(missing: &[MissingTool]) -> Vec<&'static str> {
    let mut pkgs: Vec<&str> = missing
        .iter()
        .filter_map(|m| m.package)
        // Deduplicate (e.g. arecord+aplay both map to alsa-utils)
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect();
    pkgs.sort(); // stable order
    pkgs
}

/// Build the single consolidated install command for all missing system packages.
/// Returns None if nothing needs to be installed via the package manager, or
/// the package manager is unknown.
pub fn build_install_command(missing: &[MissingTool], distro: &Distro) -> Option<String> {
    let pkgs = system_packages(missing);
    if pkgs.is_empty() {
        return None;
    }
    let prefix = install_prefix(distro)?;
    Some(format!("{prefix} {}", pkgs.join(" ")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn os_release_ids_map_to_package_managers() {
        for (content, want) in [
            ("ID=solus\nNAME=Solus\n", None),
            ("ID=\"ol\"\nID_LIKE=\"fedora\"\n", Some(Distro::Fedora)),
            (
                "ID=zorin\nID_LIKE=\"ubuntu debian\"\n",
                Some(Distro::Debian),
            ),
            ("ID_LIKE=arch\nID=endeavouros\n", Some(Distro::Arch)),
            ("ID=alpine\n", None),
            ("ID=nixos\n", None),
            ("ID=\"opensuse-tumbleweed\"\n", Some(Distro::OpenSuse)),
        ] {
            assert_eq!(Distro::from_os_release(content), want, "{content:?}");
        }
    }

    /// Unknown distros used to get `sudo apt install -y`, which
    /// /install-missing then ran.
    #[test]
    fn unknown_distro_gets_no_install_command() {
        assert_eq!(install_prefix(&Distro::Unknown), None);
        let missing = [MissingTool {
            tool: Tool::Git,
            package: Some("git"),
            manual_note: None,
        }];
        assert_eq!(build_install_command(&missing, &Distro::Unknown), None);
        assert_eq!(system_packages(&missing), ["git"]);
        assert_eq!(
            build_install_command(&missing, &Distro::Debian).as_deref(),
            Some("sudo apt install -y git")
        );
    }

    /// Probes walk PATH in-process: the `which` binary is absent from
    /// minimal installs, which made every tool read as missing.
    #[cfg(unix)]
    #[test]
    fn which_scans_path_without_the_which_binary() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("probe-tool");
        std::fs::write(&exe, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(dir.path().join("plain-file"), "").unwrap();
        // PATH holds only this dir, so a `which` subprocess could not be found.
        let path = Some(dir.path().as_os_str());
        assert!(which_in("probe-tool", path));
        assert!(!which_in("plain-file", path), "not executable");
        assert!(!which_in("which", path));
        assert!(!which_in("probe-tool", None));
    }
}
