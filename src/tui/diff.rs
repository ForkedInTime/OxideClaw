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
    Header, // @@ header or file header
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
/// Handles multi-file diffs. The path is the post-image one: the `+++` line's
/// when there is one (it is unambiguous, and C-quoted paths are unquoted),
/// else the `b/<path>` side of the `diff --git` header (binary, mode-only),
/// else the `---` line's (a deletion without a parsable header). Lines
/// between a `diff --git` header and its first `@@` are otherwise skipped.
pub fn parse_unified_diff(diff: &str) -> Vec<FileDiff> {
    let mut files = Vec::new();
    let mut current_path = String::new();
    let mut current_hunks: Vec<DiffHunk> = Vec::new();
    let mut current_lines: Vec<DiffLine> = Vec::new();
    let mut current_header = String::new();
    let mut additions = 0usize;
    let mut deletions = 0usize;
    // Inside a hunk `---x` is a removed `--x` line and `+++x` an added `++x`
    // line, so the file-header shapes only mean "header" before the first @@.
    let mut in_hunk = false;
    // Whether the header carried git's `a/` `b/` prefixes (not with
    // diff.noprefix), so the `---`/`+++` paths have them to strip.
    let mut prefixed = false;

    for line in diff.lines() {
        if line.starts_with("diff --git") {
            in_hunk = false;
            // Flush any in-progress hunk, then the in-progress file.
            if !current_lines.is_empty() {
                current_hunks.push(DiffHunk {
                    header: current_header.clone(),
                    lines: std::mem::take(&mut current_lines),
                });
            }
            let hunks = std::mem::take(&mut current_hunks);
            // A file without a readable path is dropped, but its counts
            // must not be added to the next file's.
            if !current_path.is_empty() {
                files.push(FileDiff {
                    path: std::mem::take(&mut current_path),
                    hunks,
                    additions,
                    deletions,
                });
            }
            additions = 0;
            deletions = 0;
            prefixed = line.starts_with("diff --git a/") || line.starts_with("diff --git \"a/");
            // `diff --git a/path b/path` — take the b/ side.
            if let Some(b_part) = line.split(" b/").nth(1) {
                current_path = b_part.to_string();
            }
        } else if !in_hunk && let Some(raw) = line.strip_prefix("+++ ") {
            if let Some(path) = header_path(raw, prefixed.then_some("b/")) {
                current_path = path;
            }
        } else if !in_hunk && let Some(raw) = line.strip_prefix("--- ") {
            // Only a deletion's `+++` is /dev/null; this is its path.
            if current_path.is_empty()
                && let Some(path) = header_path(raw, prefixed.then_some("a/"))
            {
                current_path = path;
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
            in_hunk = true;
        } else if !in_hunk {
            // File/index/mode headers — not part of any hunk body.
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

/// The path in a `---`/`+++` header line, without its `a/`/`b/` prefix;
/// None for /dev/null. git C-quotes a path with special characters
/// (`"b/tab\there"`) and ends one that contains a space with a tab.
fn header_path(raw: &str, prefix: Option<&str>) -> Option<String> {
    let raw = raw.strip_suffix('\t').unwrap_or(raw);
    if raw == "/dev/null" {
        return None;
    }
    let path = match raw.strip_prefix('"').and_then(|r| r.strip_suffix('"')) {
        Some(quoted) => unquote_c(quoted),
        None => raw.to_string(),
    };
    let path = match prefix {
        Some(p) => path.strip_prefix(p).map(str::to_string).unwrap_or(path),
        None => path,
    };
    (!path.is_empty()).then_some(path)
}

/// Undo git's C-style path quoting: backslash escapes and octal bytes
/// (non-ASCII under the default core.quotePath).
fn unquote_c(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        i += 1;
        if b != b'\\' || i == bytes.len() {
            out.push(b);
            continue;
        }
        let e = bytes[i];
        i += 1;
        match e {
            b'0'..=b'7' => {
                let mut v = u32::from(e - b'0');
                for _ in 0..2 {
                    if let Some(&d @ b'0'..=b'7') = bytes.get(i) {
                        v = v * 8 + u32::from(d - b'0');
                        i += 1;
                    }
                }
                out.push(v as u8);
            }
            b'a' => out.push(0x07),
            b'b' => out.push(0x08),
            b'f' => out.push(0x0c),
            b'n' => out.push(b'\n'),
            b'r' => out.push(b'\r'),
            b't' => out.push(b'\t'),
            b'v' => out.push(0x0b),
            other => out.push(other),
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}
