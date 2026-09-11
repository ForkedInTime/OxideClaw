//! File watcher with AI marker scanning.
//!
//! Watches files for changes and scans for action markers (AI:, AGENT:).
//! Integrates with the TUI event loop via AppEvent.

use notify::{Event as NotifyEvent, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

/// A marker found in a file.
#[derive(Debug, Clone)]
pub struct Marker {
    pub file: PathBuf,
    pub line: usize,
    pub text: String,
    pub kind: String, // "AI", "AGENT", "TODO", etc.
}

/// Scan file content for action markers.
pub fn scan_markers(content: &str, patterns: &[&str]) -> Vec<Marker> {
    let mut markers = Vec::new();

    for (line_idx, line) in content.lines().enumerate() {
        let trimmed = line.trim();
        // Strip comment prefixes. For `/*` comments, also strip a trailing
        // `*/` so `/* AI: foo */` yields `foo`, not `foo */`.
        let stripped = if let Some(s) = trimmed.strip_prefix("/*") {
            s.trim().trim_end_matches("*/").trim()
        } else if let Some(s) = trimmed.strip_prefix("//") {
            s.trim()
        } else if let Some(s) = trimmed.strip_prefix('#') {
            s.trim()
        } else if let Some(s) = trimmed.strip_prefix("--") {
            s.trim()
        } else {
            ""
        };

        for pattern in patterns {
            if let Some(rest) = stripped.strip_prefix(pattern) {
                markers.push(Marker {
                    file: PathBuf::new(), // Caller fills this in
                    line: line_idx + 1,
                    text: rest.trim().to_string(),
                    kind: pattern.trim_end_matches(':').to_string(),
                });
            }
        }
    }

    markers
}

/// Configuration for the file watcher.
pub struct WatchConfig {
    pub paths: Vec<PathBuf>,
    /// Glob patterns to include (e.g. `["*.rs", "*.py"]`). Only `*.<ext>` form
    /// is supported for v1; other patterns pass through unconditionally.
    pub patterns: Vec<String>,
    pub markers: Vec<String>, // marker patterns to scan for (e.g. "AI:", "AGENT:")
    /// Debounce window. Reserved — actual debouncing will coalesce rapid
    /// bursts in a follow-up task. Rate-limiting (`rate_limit_ms`) currently
    /// provides the only back-pressure.
    #[allow(dead_code)]
    pub debounce_ms: u64,
    pub rate_limit_ms: u64,
}

impl Default for WatchConfig {
    fn default() -> Self {
        Self {
            paths: vec![PathBuf::from(".")],
            patterns: vec!["*.rs".into(), "*.py".into(), "*.ts".into(), "*.js".into()],
            markers: vec!["AI:".into(), "AGENT:".into()],
            debounce_ms: 500,
            rate_limit_ms: 10_000,
        }
    }
}

/// Watch event sent to the TUI event loop.
#[derive(Debug, Clone)]
pub enum WatchEvent {
    FileChanged { path: PathBuf },
    MarkerFound { marker: Marker },
}

/// Start a file watcher. Returns a receiver for watch events.
/// The watcher handle must be kept alive (dropping it stops watching).
pub fn start_watcher(
    config: WatchConfig,
    tx: mpsc::UnboundedSender<WatchEvent>,
) -> notify::Result<RecommendedWatcher> {
    let rate_limit = Duration::from_millis(config.rate_limit_ms);
    let marker_patterns: Vec<String> = config.markers.clone();

    // Precompute `*.EXT` patterns into a Vec<String> of bare extensions.
    // Skip patterns that aren't `*.<ext>` form — they're no-ops for now.
    let pattern_exts: Vec<String> = config
        .patterns
        .iter()
        .filter_map(|p| p.strip_prefix("*.").map(|s| s.to_string()))
        .collect();

    // `None` means "never triggered" — allows the first event through without
    // relying on `Instant::now() - rate_limit`, which can underflow on
    // short-uptime systems where `Duration::from_millis(rate_limit_ms)` is
    // greater than the time since boot.
    let mut last_trigger: Option<Instant> = None;

    let mut watcher = notify::recommended_watcher(move |res: notify::Result<NotifyEvent>| {
        let Ok(event) = res else { return };

        // Only care about modifications and creations
        if !matches!(event.kind, EventKind::Modify(_) | EventKind::Create(_)) {
            return;
        }

        // Rate limit. Update `last_trigger` unconditionally once we decide
        // to handle this event — otherwise a burst where every path is
        // filtered out (e.g., all under `.git/`) would never suppress the
        // following burst.
        let now = Instant::now();
        if last_trigger.is_some_and(|t| now.duration_since(t) < rate_limit) {
            return;
        }
        last_trigger = Some(now);

        for path in &event.paths {
            // Skip non-files and gitignored files
            if !path.is_file() {
                continue;
            }
            if path.components().any(|c| c.as_os_str() == ".git") {
                continue;
            }

            // Extension filter. Patterns like `*.rs` only — anything else
            // (or an empty list) passes through. Keeps the implementation
            // tight; globbing can come later.
            if !pattern_exts.is_empty() {
                let matches = path
                    .extension()
                    .and_then(|e| e.to_str())
                    .map(|ext| pattern_exts.iter().any(|p| p == ext))
                    .unwrap_or(false);
                if !matches {
                    continue;
                }
            }

            let _ = tx.send(WatchEvent::FileChanged { path: path.clone() });

            // Scan for markers
            if let Ok(content) = std::fs::read_to_string(path) {
                let pattern_refs: Vec<&str> = marker_patterns.iter().map(|s| s.as_str()).collect();
                let mut markers = scan_markers(&content, &pattern_refs);
                for m in &mut markers {
                    m.file = path.clone();
                }
                for m in markers {
                    let _ = tx.send(WatchEvent::MarkerFound { marker: m });
                }
            }
        }
    })?;

    for path in &config.paths {
        watcher.watch(path, RecursiveMode::Recursive)?;
    }

    Ok(watcher)
}
