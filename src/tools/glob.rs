/// GlobTool — port of tools/GlobTool/GlobTool.ts
/// Fast file pattern matching, results sorted by modification time.
use super::{Tool, ToolContext, ToolOutput, async_trait};
use anyhow::Result;
use glob::MatchOptions;
use glob::glob_with;
use serde::Deserialize;
use serde_json::json;
use std::path::Path;
use std::time::SystemTime;

/// Cap on returned matches. Generous enough that ordinary searches are never
/// truncated, low enough that a repo-wide `**/*` cannot produce a multi-megabyte
/// tool result.
const MAX_GLOB_RESULTS: usize = 1000;

pub struct GlobTool;

#[derive(Deserialize)]
struct GlobInput {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
}

#[async_trait]
impl Tool for GlobTool {
    fn name(&self) -> &str {
        "Glob"
    }

    fn description(&self) -> &str {
        "Find files matching a glob pattern. Returns matching file paths sorted by \
        modification time (most recently modified first). Use patterns like '**/*.rs', \
        'src/**/*.ts', or '*.json'. Optionally specify a directory to search in."
    }

    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Glob pattern to match (e.g. '**/*.rs', 'src/**/*.ts')"
                },
                "path": {
                    "type": "string",
                    "description": "Directory to search in (defaults to current working directory)"
                }
            },
            "required": ["pattern"]
        })
    }

    async fn execute(&self, input: serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let input: GlobInput = serde_json::from_value(input)?;

        let base = match &input.path {
            Some(p) => {
                let p = Path::new(p);
                if p.is_absolute() {
                    p.to_path_buf()
                } else {
                    ctx.cwd.join(p)
                }
            }
            None => ctx.cwd.clone(),
        };

        let full_pattern = if input.pattern.starts_with('/') {
            input.pattern.clone()
        } else {
            format!("{}/{}", base.display(), input.pattern)
        };

        let options = MatchOptions {
            case_sensitive: true,
            require_literal_separator: false,
            require_literal_leading_dot: false,
        };

        let mut entries: Vec<(SystemTime, String)> = glob_with(&full_pattern, options)
            .map_err(|e| anyhow::anyhow!("Invalid glob pattern: {e}"))?
            .filter_map(|entry| {
                let path = entry.ok()?;
                if path.is_dir() {
                    return None;
                }
                // Skip files inside VCS metadata or common vendor dirs (v2.1.92: + .jj, .sl).
                if path.components().any(|c| {
                    let s = c.as_os_str().to_string_lossy();
                    crate::tools::grep::EXCLUDED_DIRS.contains(&s.as_ref())
                }) {
                    return None;
                }
                let mtime = path
                    .metadata()
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .unwrap_or(SystemTime::UNIX_EPOCH);
                Some((mtime, path.display().to_string()))
            })
            .collect();

        // Sort by modification time, most recent first
        entries.sort_by_key(|e| std::cmp::Reverse(e.0));

        // Bound the result. A broad pattern over a large tree (`**/*`) otherwise
        // builds an unbounded Vec and then joins it into one enormous string
        // that is sent to the model as a tool result — cost and memory scale
        // with the repository, and nothing downstream caps it. Sorting first
        // means the cap keeps the most recently modified matches, which is what
        // the ordering exists to surface.
        let total = entries.len();
        let truncated = total > MAX_GLOB_RESULTS;
        entries.truncate(MAX_GLOB_RESULTS);

        if entries.is_empty() {
            return Ok(ToolOutput::success("No files matched the pattern."));
        }

        let mut output = entries
            .into_iter()
            .map(|(_, path)| path)
            .collect::<Vec<_>>()
            .join("\n");

        if truncated {
            output.push_str(&format!(
                "\n\n... {} of {total} matches shown (most recently modified first). \
                 Narrow the pattern or search a subdirectory to see the rest.",
                MAX_GLOB_RESULTS
            ));
        }

        Ok(ToolOutput::success(output))
    }
}
