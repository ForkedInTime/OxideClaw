/// FileReadTool — port of tools/FileReadTool/FileReadTool.ts
use super::{Tool, ToolContext, ToolOutput, async_trait};
use anyhow::Result;
use serde::Deserialize;
use serde_json::json;
use std::hash::{Hash, Hasher};
use std::path::{Component, Path, PathBuf};
use tokio::fs;

const MAX_LINES_DEFAULT: usize = 2000;
const MAX_FILE_BYTES: u64 = 10 * 1024 * 1024; // 10MB

pub struct FileReadTool;

#[derive(Deserialize)]
struct FileReadInput {
    file_path: String,
    #[serde(default)]
    offset: Option<usize>,
    #[serde(default)]
    limit: Option<usize>,
}

#[async_trait]
impl Tool for FileReadTool {
    fn name(&self) -> &str {
        "Read"
    }

    fn description(&self) -> &str {
        "Read the contents of a file. Optionally specify offset (line number to start \
        reading from) and limit (number of lines to read). Line numbers in output \
        start at 1. For large files, use offset and limit to read specific sections."
    }

    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "file_path": {
                    "type": "string",
                    "description": "Absolute path to the file to read"
                },
                "offset": {
                    "type": "number",
                    "description": "Line number to start reading from (1-indexed)"
                },
                "limit": {
                    "type": "number",
                    "description": "Maximum number of lines to read"
                }
            },
            "required": ["file_path"]
        })
    }

    async fn execute(&self, input: serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let input: FileReadInput = serde_json::from_value(input)?;
        let path = match resolve_path(&input.file_path, &ctx.cwd) {
            Ok(p) => p,
            Err(e) => return Ok(ToolOutput::error(e.to_string())),
        };

        if let Some(err) = super::check_sensitive_path_resolved(&path, super::SensitiveOp::Read) {
            return Ok(err);
        }

        // Check file exists
        if !path.exists() {
            return Ok(ToolOutput::error(format!(
                "File not found: {}",
                path.display()
            )));
        }

        let meta = fs::metadata(&path).await?;
        // Devices and FIFOs report length 0: /dev/zero would be read until the
        // process runs out of memory, and a FIFO would block the tool forever.
        if !meta.is_file() {
            return Ok(ToolOutput::error(format!(
                "Not a regular file: {} (directories, devices, FIFOs and sockets cannot be read)",
                path.display()
            )));
        }

        if meta.len() > MAX_FILE_BYTES {
            if input.offset.is_none() && input.limit.is_none() {
                return Ok(ToolOutput::error(format!(
                    "File too large to read whole ({} bytes, limit {MAX_FILE_BYTES}). \
                     Pass offset/limit to read a section.",
                    meta.len()
                )));
            }
            // The section is streamed so a huge file never sits in memory.
            let offset = input.offset.unwrap_or(1).saturating_sub(1);
            let limit = input.limit.unwrap_or(MAX_LINES_DEFAULT).max(1);
            return read_section(&path, offset, limit, meta.len())
                .await
                .map(ToolOutput::success)
                .map_err(|e| anyhow::anyhow!("Failed to read {}: {}", path.display(), e));
        }

        let content = fs::read_to_string(&path)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to read {}: {}", path.display(), e))?;

        // v2.1.86: dedup unchanged re-reads. Hash the content and check against
        // the shared read-cache; if identical to a previous read of the same
        // path, emit a compact notice instead of re-sending the whole file.
        // Only applies when no offset/limit is requested (partial reads always
        // return their slice).
        if input.offset.is_none()
            && input.limit.is_none()
            && let Some(cache) = &ctx.read_cache
        {
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            content.hash(&mut hasher);
            let hash = hasher.finish();
            let mut guard = cache.lock().unwrap_or_else(|e| e.into_inner());
            if guard.get(&path) == Some(&hash) {
                return Ok(ToolOutput::success(format!(
                    "(unchanged since last read: {})",
                    path.display()
                )));
            }
            guard.insert(path.clone(), hash);
        }

        let lines: Vec<&str> = content.lines().collect();
        let total_lines = lines.len();

        let offset = input.offset.unwrap_or(1).saturating_sub(1); // convert to 0-indexed
        let limit = input.limit.unwrap_or(MAX_LINES_DEFAULT).max(1);

        // offset/limit come straight from model input: a limit near usize::MAX
        // would wrap `end` below the start and the slice would panic, which
        // aborts the whole process under panic = "abort".
        let start = offset.min(total_lines);
        let end = offset.saturating_add(limit).min(total_lines);
        let selected = &lines[start..end];

        // Format with line numbers (cat -n style), 1-indexed
        let mut output = String::new();
        for (i, line) in selected.iter().enumerate() {
            let line_num = offset + i + 1;
            output.push_str(&format!("{}\t{}\n", line_num, line));
        }

        if total_lines == 0 {
            output = "(empty file)".to_string();
        } else if output.is_empty() {
            output = format!(
                "(no lines at offset {}: the file has {total_lines} lines)",
                offset + 1
            );
        } else if end < total_lines {
            // Without this the model treats the default 2000-line cap as the
            // whole file and edits or reasons from a partial view.
            output.push_str(&format!(
                "\n... (showing lines {}-{end} of {total_lines}; use offset/limit to read more)\n",
                offset + 1
            ));
        }

        Ok(ToolOutput::success(output))
    }
}

/// Stream `limit` lines starting at 0-indexed line `offset` from a file too
/// large to load whole, formatted like the in-memory path. Output is capped
/// at `MAX_FILE_BYTES` so a file with one enormous line cannot exhaust memory.
async fn read_section(
    path: &Path,
    offset: usize,
    limit: usize,
    file_len: u64,
) -> std::io::Result<String> {
    use tokio::io::AsyncBufReadExt;

    let mut reader = tokio::io::BufReader::with_capacity(256 * 1024, fs::File::open(path).await?);
    let budget = MAX_FILE_BYTES as usize;
    let mut output = String::new();
    let mut line_no = 0usize;
    let mut capped = false;
    let mut line = Vec::new();

    'lines: while line_no < offset.saturating_add(limit) {
        line.clear();
        let keep = line_no >= offset;
        let mut saw_bytes = false;
        loop {
            let buf = reader.fill_buf().await?;
            if buf.is_empty() {
                if !saw_bytes {
                    break 'lines;
                }
                break;
            }
            saw_bytes = true;
            let (chunk, used, eol) = match buf.iter().position(|&b| b == b'\n') {
                Some(i) => (&buf[..i], i + 1, true),
                None => (buf, buf.len(), false),
            };
            if keep {
                let room = budget.saturating_sub(output.len() + line.len());
                if chunk.len() > room {
                    capped = true;
                }
                line.extend_from_slice(&chunk[..chunk.len().min(room)]);
            }
            reader.consume(used);
            if eol {
                break;
            }
        }
        line_no += 1;
        if keep {
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            output.push_str(&format!("{line_no}\t{}\n", String::from_utf8_lossy(&line)));
            if capped {
                break;
            }
        }
    }

    if output.is_empty() {
        return Ok(format!(
            "(no lines at offset {}: the file has {line_no} lines)",
            offset + 1
        ));
    }
    let more = capped || !reader.fill_buf().await?.is_empty();
    if more {
        output.push_str(&format!(
            "\n... (showing lines {}-{line_no}; the file is {file_len} bytes and continues; \
             use offset/limit to read more)\n",
            offset + 1
        ));
    }
    Ok(output)
}

/// Lexically clean a path by resolving `.` and `..` components WITHOUT
/// touching the filesystem (so this works for files that don't exist yet,
/// unlike `Path::canonicalize`). Leading `..`s are preserved — they're what
/// `resolve_path_safe` uses to detect attempted escapes.
fn clean_path(p: &Path) -> PathBuf {
    let mut out: Vec<Component<'_>> = Vec::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                // Only collapse a `..` when the last component is a concrete
                // directory name. Refuse to pop past RootDir/Prefix, and keep
                // a leading `..` so the escape check can notice it.
                match out.last() {
                    Some(Component::Normal(_)) => {
                        out.pop();
                    }
                    _ => out.push(c),
                }
            }
            other => out.push(other),
        }
    }
    out.iter().collect()
}

/// Models write `~/.zshrc` the way a shell would; without expansion that is a
/// relative path and Write would create a literal `<cwd>/~/.zshrc`. Only `~`
/// and `~/...` are expanded (not `~user`). The result is an ordinary absolute
/// path and goes through the same sensitive/protected-path checks as one.
pub fn expand_home(file_path: &str) -> Result<Option<PathBuf>> {
    let rest = match file_path.strip_prefix('~') {
        Some("") => "",
        Some(r) if r.starts_with(std::path::is_separator) => &r[1..],
        _ => return Ok(None),
    };
    let home = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("cannot expand '{file_path}': home directory unknown"))?;
    Ok(Some(if rest.is_empty() {
        home
    } else {
        home.join(rest)
    }))
}

/// The directory Grep and Glob search: `~` expanded, relative paths joined
/// to `cwd`. Unlike [`resolve_path`] there is no containment check, since
/// searching outside the project is an ordinary request.
pub fn resolve_search_path(path: Option<&str>, cwd: &Path) -> Result<PathBuf> {
    let Some(p) = path else {
        return Ok(cwd.to_path_buf());
    };
    if let Some(home) = expand_home(p)? {
        return Ok(home);
    }
    Ok(cwd.join(p))
}

/// Resolve a user-supplied path for a tool. Relative paths are joined to
/// `cwd` and lexically cleaned; if the cleaned result escapes `cwd` (e.g.
/// `../../etc/passwd`), this returns an error instead of a path the caller
/// would happily read.
///
/// Absolute paths are passed through untouched — tools that run on absolute
/// paths (e.g. reading `/tmp/foo`) are a legitimate workflow and are still
/// subject to [`super::check_sensitive_path`]. The containment check here
/// is a defense-in-depth layer specifically targeting relative-path escapes.
pub fn resolve_path(file_path: &str, cwd: &Path) -> Result<PathBuf> {
    if let Some(home) = expand_home(file_path)? {
        return Ok(home);
    }
    let input = Path::new(file_path);
    if input.is_absolute() {
        return Ok(input.to_path_buf());
    }
    let joined = cwd.join(input);
    let cleaned = clean_path(&joined);
    let cwd_cleaned = clean_path(cwd);
    if !cleaned.starts_with(&cwd_cleaned) {
        anyhow::bail!(
            "path '{file_path}' escapes working directory '{}'",
            cwd_cleaned.display()
        );
    }
    Ok(cleaned)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(o: &ToolOutput) -> String {
        o.content
            .iter()
            .map(|c| match c {
                crate::api::types::ToolResultContent::Text { text } => text.as_str(),
            })
            .collect()
    }

    async fn read(ctx: &ToolContext, input: serde_json::Value) -> ToolOutput {
        FileReadTool.execute(input, ctx).await.unwrap()
    }

    #[tokio::test]
    async fn default_line_cap_says_the_file_continues() {
        let dir = tempfile::tempdir().unwrap();
        let body: String = (1..=2500).map(|i| format!("l{i}\n")).collect();
        std::fs::write(dir.path().join("big.txt"), body).unwrap();
        let ctx = ToolContext::new(dir.path().to_path_buf());

        let out = text(&read(&ctx, json!({"file_path": "big.txt"})).await);
        assert!(out.contains("2000\tl2000\n"), "{out}");
        assert!(!out.contains("l2001"), "{out}");
        assert!(out.contains("showing lines 1-2000 of 2500"), "{out}");

        let tail = text(&read(&ctx, json!({"file_path": "big.txt", "offset": 2001})).await);
        assert!(tail.contains("2500\tl2500"), "{tail}");
        assert!(!tail.contains("showing lines"), "{tail}");
    }

    /// `offset + limit` wrapped (or panicked) for a huge model-supplied
    /// limit, reading nothing.
    #[tokio::test]
    async fn huge_limit_does_not_overflow() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("f.txt");
        std::fs::write(&f, "a\nb\nc\n").unwrap();
        let ctx = ToolContext::new(dir.path().to_path_buf());
        let out = text(
            &read(
                &ctx,
                json!({"file_path": "f.txt", "offset": 2, "limit": usize::MAX}),
            )
            .await,
        );
        assert!(out.contains("2\tb\n3\tc"), "{out}");
        let out = read_section(&f, 1, usize::MAX, 6).await.unwrap();
        assert!(out.contains("2\tb") && out.contains("3\tc"), "{out}");
    }

    #[tokio::test]
    async fn non_regular_files_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        let ctx = ToolContext::new(dir.path().to_path_buf());
        let out = read(&ctx, json!({"file_path": "sub"})).await;
        assert!(out.is_error && text(&out).contains("Not a regular file"));

        // A device reports length 0; /dev/zero would be read until OOM.
        #[cfg(unix)]
        {
            let out = read(&ctx, json!({"file_path": "/dev/null"})).await;
            assert!(out.is_error && text(&out).contains("Not a regular file"));
        }
    }

    #[tokio::test]
    async fn files_over_the_size_cap_can_be_read_by_section() {
        let dir = tempfile::tempdir().unwrap();
        let mut body = String::new();
        let mut n = 0;
        while body.len() as u64 <= MAX_FILE_BYTES {
            n += 1;
            body.push_str(&format!("row {n}\r\n"));
        }
        std::fs::write(dir.path().join("huge.log"), &body).unwrap();
        let ctx = ToolContext::new(dir.path().to_path_buf());

        let whole = read(&ctx, json!({"file_path": "huge.log"})).await;
        assert!(whole.is_error, "{}", text(&whole));
        assert!(text(&whole).contains("offset/limit"));

        let mid = read(
            &ctx,
            json!({"file_path": "huge.log", "offset": 800_000, "limit": 2}),
        )
        .await;
        let mid = text(&mid);
        assert!(
            mid.starts_with("800000\trow 800000\n800001\trow 800001\n"),
            "{mid}"
        );
        assert!(mid.contains("continues"), "{mid}");

        let last = text(&read(&ctx, json!({"file_path": "huge.log", "offset": n})).await);
        assert_eq!(last, format!("{n}\trow {n}\n"));

        let past = text(&read(&ctx, json!({"file_path": "huge.log", "offset": n + 5})).await);
        assert!(past.contains(&format!("the file has {n} lines")), "{past}");
    }

    #[test]
    fn tilde_paths_resolve_to_the_home_directory() {
        let Some(home) = dirs::home_dir() else { return };
        let cwd = Path::new("/proj");
        assert_eq!(resolve_path("~/.zshrc", cwd).unwrap(), home.join(".zshrc"));
        assert_eq!(resolve_path("~", cwd).unwrap(), home);
        // `~user` and a `~` mid-name are not shell expansions.
        assert_eq!(resolve_path("~bob/x", cwd).unwrap(), cwd.join("~bob/x"));
        assert_eq!(resolve_path("a/~/b", cwd).unwrap(), cwd.join("a/~/b"));

        assert_eq!(
            resolve_search_path(Some("~/src"), cwd).unwrap(),
            home.join("src")
        );
        assert_eq!(
            resolve_search_path(Some("lib"), cwd).unwrap(),
            cwd.join("lib")
        );
        assert_eq!(
            resolve_search_path(Some("/abs"), cwd).unwrap(),
            Path::new("/abs")
        );
        assert_eq!(resolve_search_path(None, cwd).unwrap(), cwd);
    }

    #[tokio::test]
    async fn offset_past_end_is_not_reported_as_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("three.txt"), "a\nb\nc\n").unwrap();
        std::fs::write(dir.path().join("empty.txt"), "").unwrap();
        let ctx = ToolContext::new(dir.path().to_path_buf());

        let past = text(&read(&ctx, json!({"file_path": "three.txt", "offset": 10})).await);
        assert_eq!(past, "(no lines at offset 10: the file has 3 lines)");

        let empty = text(&read(&ctx, json!({"file_path": "empty.txt"})).await);
        assert_eq!(empty, "(empty file)");
    }

    #[tokio::test]
    async fn huge_limit_reads_exactly_to_the_end() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("three.txt"), "a\nb\nc\n").unwrap();
        let ctx = ToolContext::new(dir.path().to_path_buf());

        let out = read(
            &ctx,
            json!({"file_path": "three.txt", "offset": 2, "limit": u64::MAX}),
        )
        .await;
        assert_eq!(text(&out), "2\tb\n3\tc\n");

        // The streamed path for files over the size cap has the same sum.
        let path = dir.path().join("three.txt");
        let section = read_section(&path, 1, usize::MAX, 6).await.unwrap();
        assert_eq!(section, "2\tb\n3\tc\n");
    }
}
