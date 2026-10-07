//! File watcher with AI marker scanning.
//!
//! Watches files for changes and scans for action markers (AI:, AGENT:).
//! Integrates with the TUI event loop via AppEvent.

use notify::{Event as NotifyEvent, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
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

/// The paths of one notify event worth reporting: files outside `.git` that
/// match `pattern_exts` (`*.EXT` patterns; empty passes all), each at most
/// once per `rate_limit`.
///
/// The limit is per path and applied after filtering. A single global window
/// taken before filtering let an editor's swap or write-probe file (vim's
/// `4913`, `.swp`), a `.git/index.lock` or a build under `target/` use it up,
/// so the source-file save that followed was silently dropped.
fn paths_to_report(
    paths: &[PathBuf],
    pattern_exts: &[String],
    rate_limit: Duration,
    last_trigger: &mut HashMap<PathBuf, Instant>,
    now: Instant,
) -> Vec<PathBuf> {
    last_trigger.retain(|_, t| now.duration_since(*t) < rate_limit);
    let mut out = Vec::new();
    for path in paths {
        if !path.is_file() || path.components().any(|c| c.as_os_str() == ".git") {
            continue;
        }
        if !pattern_exts.is_empty() && !has_ext(path, pattern_exts) {
            continue;
        }
        if last_trigger.contains_key(path) {
            continue;
        }
        last_trigger.insert(path.clone(), now);
        out.push(path.clone());
    }
    out
}

fn has_ext(path: &Path, exts: &[String]) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|ext| exts.iter().any(|p| p == ext))
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

    let mut last_trigger: HashMap<PathBuf, Instant> = HashMap::new();

    let mut watcher = notify::recommended_watcher(move |res: notify::Result<NotifyEvent>| {
        let Ok(event) = res else { return };

        // Only care about modifications and creations
        if !matches!(event.kind, EventKind::Modify(_) | EventKind::Create(_)) {
            return;
        }

        let now = Instant::now();
        for path in paths_to_report(
            &event.paths,
            &pattern_exts,
            rate_limit,
            &mut last_trigger,
            now,
        ) {
            let _ = tx.send(WatchEvent::FileChanged { path: path.clone() });

            // Scan for markers
            if let Ok(content) = std::fs::read_to_string(&path) {
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

#[cfg(test)]
mod tests {
    use super::paths_to_report;
    use std::collections::HashMap;
    use std::time::{Duration, Instant};

    /// A vim save: write probe, swap file, then the real file, all within
    /// the rate-limit window. The source file must still be reported.
    #[test]
    fn editor_scratch_files_do_not_use_up_the_rate_limit() {
        let dir = tempfile::tempdir().unwrap();
        let probe = dir.path().join("4913");
        let swap = dir.path().join(".lib.rs.swp");
        let src = dir.path().join("lib.rs");
        let other = dir.path().join("main.rs");
        let git = dir.path().join(".git");
        std::fs::create_dir(&git).unwrap();
        let lock = git.join("index.rs");
        for p in [&probe, &swap, &src, &other, &lock] {
            std::fs::write(p, "// AI: x").unwrap();
        }
        let exts = vec!["rs".to_string()];
        let limit = Duration::from_secs(10);
        let mut last = HashMap::new();
        let t0 = Instant::now();
        let mut report = |paths: &[&std::path::PathBuf], secs| {
            let paths: Vec<_> = paths.iter().map(|p| (*p).clone()).collect();
            paths_to_report(
                &paths,
                &exts,
                limit,
                &mut last,
                t0 + Duration::from_secs(secs),
            )
        };

        assert!(report(&[&probe], 0).is_empty());
        assert!(report(&[&swap], 0).is_empty());
        assert!(report(&[&lock], 0).is_empty());
        assert_eq!(report(&[&src], 0), vec![src.clone()]);
        // The same file again inside the window is suppressed; another isn't.
        assert!(report(&[&src], 1).is_empty());
        assert_eq!(report(&[&other], 1), vec![other.clone()]);
        // After the window the file reports again.
        assert_eq!(report(&[&src], 11), vec![src.clone()]);
    }
}
