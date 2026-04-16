//! First-run --yolo acknowledgment.
//!
//! Writes a timestamp+version file to $XDG_STATE_HOME/rustyclaw/yolo-ack
//! on first --yolo use. Subsequent runs are silent.

use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn ack_path() -> PathBuf {
    let state_home = std::env::var("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            dirs::home_dir()
                .unwrap_or_else(|| {
                    std::env::var("HOME")
                        .map(PathBuf::from)
                        .unwrap_or_default()
                })
                .join(".local/state")
        });
    state_home.join("rustyclaw").join("yolo-ack")
}

pub fn is_acknowledged() -> bool {
    ack_path().exists()
}

pub fn acknowledge() -> std::io::Result<()> {
    let p = ack_path();
    if let Some(parent) = p.parent() {
        fs::create_dir_all(parent)?;
    }
    // Format: seconds since epoch as ISO-8601-ish timestamp (no chrono dep)
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let contents = format!("{secs} rustyclaw v{VERSION}\n");
    fs::write(p, contents)
}
