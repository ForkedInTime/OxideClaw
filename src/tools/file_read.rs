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

        if let Some(err) = super::check_sensitive_path(&path, super::SensitiveOp::Read) {
            return Ok(err);
        }

        // Check file exists
        if !path.exists() {
            return Ok(ToolOutput::error(format!(
                "File not found: {}",
                path.display()
            )));
        }

        // Check file size
        let meta = fs::metadata(&path).await?;
        if meta.len() > MAX_FILE_BYTES {
            return Ok(ToolOutput::error(format!(
                "File too large ({} bytes). Use offset/limit to read sections.",
                meta.len()
            )));
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
        let limit = input.limit.unwrap_or(MAX_LINES_DEFAULT);

        let end = (offset + limit).min(total_lines);
        let selected = &lines[offset.min(total_lines)..end];

        // Format with line numbers (cat -n style), 1-indexed
        let mut output = String::new();
        for (i, line) in selected.iter().enumerate() {
            let line_num = offset + i + 1;
            output.push_str(&format!("{}\t{}\n", line_num, line));
        }

        if output.is_empty() {
            output = "(empty file)".to_string();
        }

        Ok(ToolOutput::success(output))
    }
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
