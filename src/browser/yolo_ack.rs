//! First-run --yolo acknowledgment.
//!
//! Writes a timestamp+version file to $XDG_STATE_HOME/oxideclaw/yolo-ack
//! on first --yolo use. Subsequent runs are silent.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn ack_path() -> Option<PathBuf> {
    ack_path_in(
        std::env::var("XDG_STATE_HOME").ok().as_deref(),
        dirs::home_dir().as_deref(),
    )
}

/// An empty or relative `$XDG_STATE_HOME` (or home) would put the file, and
/// `app_dir`'s legacy rename, in the current directory; the XDG spec says to
/// ignore such values. With no absolute base there is no ack file.
fn ack_path_in(xdg_state_home: Option<&str>, home: Option<&Path>) -> Option<PathBuf> {
    let base = match xdg_state_home.map(Path::new).filter(|x| x.is_absolute()) {
        Some(x) => x.to_path_buf(),
        None => home.filter(|h| h.is_absolute())?.join(".local/state"),
    };
    Some(crate::config::app_dir(&base).join("yolo-ack"))
}

pub fn is_acknowledged() -> bool {
    ack_path().is_some_and(|p| p.exists())
}

pub fn acknowledge() -> std::io::Result<()> {
    let Some(p) = ack_path() else {
        return Ok(());
    };
    if let Some(parent) = p.parent() {
        fs::create_dir_all(parent)?;
    }
    // Format: seconds since epoch as ISO-8601-ish timestamp (no chrono dep)
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let contents = format!("{secs} oxideclaw v{VERSION}\n");
    fs::write(p, contents)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `XDG_STATE_HOME=` (exported but empty) wrote `oxideclaw/yolo-ack`
    /// into the current directory and renamed a `rustyclaw/` there.
    #[test]
    fn empty_or_relative_state_home_falls_back_to_home() {
        let home = tempfile::tempdir().unwrap();
        let want = home.path().join(".local/state/oxideclaw/yolo-ack");
        for xdg in [None, Some(""), Some("rel/state")] {
            assert_eq!(
                ack_path_in(xdg, Some(home.path())),
                Some(want.clone()),
                "{xdg:?}"
            );
        }
        let state = tempfile::tempdir().unwrap();
        assert_eq!(
            ack_path_in(state.path().to_str(), Some(home.path())),
            Some(state.path().join("oxideclaw/yolo-ack"))
        );
    }

    #[test]
    fn no_absolute_base_means_no_ack_file() {
        assert_eq!(ack_path_in(Some(""), None), None);
        assert_eq!(ack_path_in(None, Some(Path::new("rel-home"))), None);
    }
}
