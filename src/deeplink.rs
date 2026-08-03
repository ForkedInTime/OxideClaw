//! Deep link protocol handler — port of utils/deepLink/
//!
//! Registers a custom URI scheme so external apps can open a rustyclaw session
//! by navigating to: `<protocol>://open?q=<prompt>&cwd=<dir>&repo=<name>`
//!
//! The protocol name is derived from the binary name at compile time
//! (e.g., "rustyclaw" → protocol "rustyclaw-cli"), keeping it configurable
//! without hardcoding "rustrustyclaw" or any other specific name.
//!
//! Linux: creates a .desktop file and registers via xdg-mime.

/// The URL scheme suffix appended to the binary name.
const SCHEME_SUFFIX: &str = "-cli";

/// Maximum allowed query string length (matches upstream reference).
const MAX_QUERY_LEN: usize = 5_000;
/// Maximum allowed cwd path length.
const MAX_CWD_LEN: usize = 4_096;

/// Returns the registered URL scheme, e.g. "rustyclaw-cli".
pub fn protocol_name() -> String {
    format!("{}{}", env!("CARGO_BIN_NAME"), SCHEME_SUFFIX)
}

/// Parsed deep link parameters.
#[derive(Debug)]
#[allow(dead_code)] // fields parsed from URI, will be used by deep link handler
pub struct DeepLinkParams {
    /// The prompt / query string from `q=`
    pub query: String,
    /// Optional working directory from `cwd=`
    pub cwd: Option<String>,
    /// Optional repository name from `repo=`
    pub repo: Option<String>,
}

/// Parse a deep link URI into its parameters.
///
/// Accepts URIs of the form:
///   <scheme>://open?q=<prompt>&cwd=<dir>&repo=<name>
///
/// Returns None if the URI is malformed, has the wrong scheme, or fails
/// security validation (control chars, excessive lengths).
pub fn parse_deep_link(uri: &str) -> Option<DeepLinkParams> {
    let scheme = protocol_name();
    let prefix = format!("{}://open?", scheme);
    let prefix2 = format!("{}://open", scheme);

    let query_str = if let Some(rest) = uri.strip_prefix(&prefix) {
        rest
    } else if uri == prefix2 || uri == format!("{}://open/", scheme) {
        ""
    } else {
        return None;
    };

    let mut q: Option<String> = None;
    let mut cwd: Option<String> = None;
    let mut repo: Option<String> = None;

    for pair in query_str.split('&').filter(|s| !s.is_empty()) {
        if let Some((key, val)) = pair.split_once('=') {
            let decoded = percent_decode(val);
            match key {
                "q" => q = Some(decoded),
                "cwd" => cwd = Some(decoded),
                "repo" => repo = Some(decoded),
                _ => {}
            }
        }
    }

    let query = q?;

    // Security: reject control characters
    if contains_control_chars(&query) || cwd.as_deref().map(contains_control_chars).unwrap_or(false)
    {
        return None;
    }

    // Length limits
    if query.len() > MAX_QUERY_LEN {
        return None;
    }
    if let Some(ref c) = cwd
        && c.len() > MAX_CWD_LEN
    {
        return None;
    }

    Some(DeepLinkParams { query, cwd, repo })
}

fn contains_control_chars(s: &str) -> bool {
    s.chars()
        .any(|c| c.is_control() && c != '\n' && c != '\r' && c != '\t')
}

fn percent_decode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = bytes[i + 1];
            let lo = bytes[i + 2];
            if let (Some(h), Some(l)) = (hex_val(hi), hex_val(lo)) {
                let byte = (h << 4) | l;
                out.push(byte as char);
                i += 3;
                continue;
            }
        } else if bytes[i] == b'+' {
            out.push(' ');
            i += 1;
            continue;
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Register the deep link protocol handler on Linux.
///
/// Creates ~/.local/share/applications/<scheme>-handler.desktop and registers
/// it with xdg-mime as the handler for x-scheme-handler/<scheme>.
///
/// Requires xdg-utils to be installed (standard on most desktops).
/// On non-Linux platforms, prints a notice and returns Ok(()).
#[cfg(not(target_os = "linux"))]
pub fn register_protocol() -> anyhow::Result<()> {
    eprintln!("Deep link protocol registration is only supported on Linux.");
    Ok(())
}

/// Register the deep link protocol handler on Linux.
#[cfg(target_os = "linux")]
pub fn register_protocol() -> anyhow::Result<()> {
    {
        let scheme = protocol_name();
        let binary = std::env::current_exe()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| env!("CARGO_BIN_NAME").to_string());

        let desktop_name = format!("{scheme}-handler");
        let desktop_dir = dirs::home_dir()
            .ok_or_else(|| anyhow::anyhow!("Cannot determine home directory"))?
            .join(".local")
            .join("share")
            .join("applications");

        std::fs::create_dir_all(&desktop_dir)?;

        let desktop_path = desktop_dir.join(format!("{desktop_name}.desktop"));
        let desktop_content = format!(
            "[Desktop Entry]\n\
             Name={bin} deep link handler\n\
             Exec=\"{binary}\" --handle-uri %u\n\
             Type=Application\n\
             NoDisplay=true\n\
             MimeType=x-scheme-handler/{scheme};\n",
            bin = env!("CARGO_BIN_NAME"),
        );

        std::fs::write(&desktop_path, &desktop_content)?;

        // Register with xdg-mime
        let status = std::process::Command::new("xdg-mime")
            .args([
                "default",
                &format!("{desktop_name}.desktop"),
                &format!("x-scheme-handler/{scheme}"),
            ])
            .status();

        match status {
            Ok(s) if s.success() => {
                println!("Registered {scheme}:// protocol handler.");
                println!("Desktop file: {}", desktop_path.display());
            }
            Ok(s) => {
                eprintln!("xdg-mime exited with status {s} — you may need to run it manually:");
                eprintln!("  xdg-mime default {desktop_name}.desktop x-scheme-handler/{scheme}");
            }
            Err(e) => {
                eprintln!("xdg-mime not found ({e}) — install xdg-utils and run:");
                eprintln!("  xdg-mime default {desktop_name}.desktop x-scheme-handler/{scheme}");
            }
        }

        // Also update the MIME database so GTK apps pick up the new handler
        let _ = std::process::Command::new("update-desktop-database")
            .arg(desktop_dir.to_string_lossy().as_ref())
            .status();

        Ok(())
    }
}
