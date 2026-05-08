//! Unified diff parsing for the read-only diff overlay (`/diff`).
//!
//! The TUI overlay renders the raw diff text and only reads the summary
//! counts (`additions` / `deletions`) from `FileDiff`. Hunks and lines are
//! still produced by the parser so unit tests in `tests/diff_tests.rs` can
//! verify hunk-level correctness.

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct DiffHunk {
    pub header: String, // @@ -1,3 +1,4 @@
    pub lines: Vec<DiffLine>,
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct DiffLine {
    pub kind: DiffLineKind,
    pub content: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum DiffLineKind {
    Context, // unchanged line (space prefix)
    Added,   // + line
    Removed, // - line
    #[allow(dead_code)]
    Header,  // @@ header or file header
}

#[derive(Debug, Clone)]
pub struct FileDiff {
    pub path: String,
    #[allow(dead_code)]
    pub hunks: Vec<DiffHunk>,
    pub additions: usize,
    pub deletions: usize,
}

/// Parse a unified diff string (output of `git diff`) into structured FileDiffs.
///
/// Handles multi-file diffs. File path is taken from the `b/<path>` side of
/// the `diff --git` header (the post-image path), matching how git describes
/// the target tree. Lines that aren't part of a hunk (index / ---  / +++) are
/// skipped.
pub fn parse_unified_diff(diff: &str) -> Vec<FileDiff> {
    let mut files = Vec::new();
    let mut current_path = String::new();
    let mut current_hunks: Vec<DiffHunk> = Vec::new();
    let mut current_lines: Vec<DiffLine> = Vec::new();
    let mut current_header = String::new();
    let mut additions = 0usize;
    let mut deletions = 0usize;

    for line in diff.lines() {
        if line.starts_with("diff --git") {
            // Flush any in-progress hunk, then the in-progress file.
            if !current_path.is_empty() {
                if !current_lines.is_empty() {
                    current_hunks.push(DiffHunk {
                        header: current_header.clone(),
                        lines: std::mem::take(&mut current_lines),
                    });
                }
                files.push(FileDiff {
                    path: std::mem::take(&mut current_path),
                    hunks: std::mem::take(&mut current_hunks),
                    additions,
                    deletions,
                });
                additions = 0;
                deletions = 0;
            }
            // `diff --git a/path b/path` — take the b/ side.
            if let Some(b_part) = line.split(" b/").nth(1) {
                current_path = b_part.to_string();
            }
        } else if line.starts_with("@@") {
            // New hunk — flush the previous one.
            if !current_lines.is_empty() {
                current_hunks.push(DiffHunk {
                    header: current_header.clone(),
                    lines: std::mem::take(&mut current_lines),
                });
            }
            current_header = line.to_string();
        } else if line.starts_with("+++") || line.starts_with("---") || line.starts_with("index ") {
            // Skip file/index headers — they're not part of any hunk body.
        } else if let Some(rest) = line.strip_prefix('+') {
            additions += 1;
            current_lines.push(DiffLine {
                kind: DiffLineKind::Added,
                content: rest.to_string(),
            });
        } else if let Some(rest) = line.strip_prefix('-') {
            deletions += 1;
            current_lines.push(DiffLine {
                kind: DiffLineKind::Removed,
                content: rest.to_string(),
            });
        } else if line.starts_with(' ') || line.is_empty() {
            let content = if line.is_empty() {
                String::new()
            } else {
                line[1..].to_string()
            };
            current_lines.push(DiffLine {
                kind: DiffLineKind::Context,
                content,
            });
        }
        // Any other prefix (e.g. "\\ No newline at end of file") is ignored.
    }

    // Flush trailing state.
    if !current_path.is_empty() {
        if !current_lines.is_empty() {
            current_hunks.push(DiffHunk {
                header: current_header,
                lines: current_lines,
            });
        }
        files.push(FileDiff {
            path: current_path,
            hunks: current_hunks,
            additions,
            deletions,
        });
    }

    files
}
