/// MultiEditTool — port of tools/MultiEditTool.
///
/// Applies multiple find-and-replace edits to one or more files in a single
/// tool call.  Each edit specifies file_path, old_string, new_string and an
/// optional replace_all flag — identical to the Edit tool's parameters.
///
/// Using MultiEdit instead of multiple Edit calls lets Claude batch related
/// changes atomically and reduces round-trips.
use super::{Tool, ToolContext, ToolOutput, async_trait, snapshot_file};
use crate::tools::file_read::resolve_path;
use anyhow::Result;
use serde::Deserialize;
use serde_json::json;
use tokio::fs;

pub struct MultiEditTool;

#[derive(Deserialize)]
struct SingleEdit {
    file_path: String,
    old_string: String,
    new_string: String,
    #[serde(default)]
    replace_all: bool,
}

#[derive(Deserialize)]
struct MultiEditInput {
    edits: Vec<SingleEdit>,
}

#[async_trait]
impl Tool for MultiEditTool {
    fn name(&self) -> &str {
        "MultiEdit"
    }

    fn description(&self) -> &str {
        "Apply multiple file edits in a single call. Each edit is an exact \
        string replacement (old_string → new_string) in the specified file. \
        All edits are applied sequentially; if any edit fails the tool reports \
        the error but continues with remaining edits. Use this instead of \
        multiple Edit calls when making related changes across one or more files."
    }

    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "edits": {
                    "type": "array",
                    "description": "List of edits to apply",
                    "items": {
                        "type": "object",
                        "properties": {
                            "file_path": {
                                "type": "string",
                                "description": "Absolute path to the file to edit"
                            },
                            "old_string": {
                                "type": "string",
                                "description": "The exact string to find and replace"
                            },
                            "new_string": {
                                "type": "string",
                                "description": "The replacement string"
                            },
                            "replace_all": {
                                "type": "boolean",
                                "description": "Replace all occurrences (default: false — requires exactly one match)"
                            }
                        },
                        "required": ["file_path", "old_string", "new_string"]
                    }
                }
            },
            "required": ["edits"]
        })
    }

    async fn execute(&self, input: serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let input: MultiEditInput = match serde_json::from_value(input) {
            Ok(v) => v,
            Err(e) => return Ok(ToolOutput::error(format!("Invalid MultiEdit input: {}", e))),
        };

        if input.edits.is_empty() {
            return Ok(ToolOutput::error("No edits provided"));
        }

        let mut results: Vec<String> = Vec::new();
        let mut had_error = false;

        // path -> fully-edited content, written only if every edit to that path
        // succeeded.
        let mut staged: std::collections::BTreeMap<std::path::PathBuf, String> =
            std::collections::BTreeMap::new();
        let mut failed_files: std::collections::BTreeSet<std::path::PathBuf> =
            std::collections::BTreeSet::new();

        for (i, edit) in input.edits.iter().enumerate() {
            let path = match resolve_path(&edit.file_path, &ctx.cwd) {
                Ok(p) => p,
                Err(e) => {
                    let label = format!("[{}/{}] {}", i + 1, input.edits.len(), edit.file_path);
                    results.push(format!("{label} ✗ {e}"));
                    had_error = true;
                    continue;
                }
            };
            let label = format!("[{}/{}] {}", i + 1, input.edits.len(), path.display());

            if let Some(err) = super::check_protected_path(&path) {
                let msg = err
                    .content
                    .iter()
                    .map(|c| {
                        let super::ToolResultContent::Text { text } = c;
                        text.as_str()
                    })
                    .collect::<Vec<_>>()
                    .join(" ");
                results.push(format!("{label} ✗ {msg}"));
                had_error = true;
                failed_files.insert(path.clone());
                continue;
            }
            if let Some(err) = super::check_sensitive_path_resolved(&path, super::SensitiveOp::Write) {
                let msg = err
                    .content
                    .iter()
                    .map(|c| {
                        let super::ToolResultContent::Text { text } = c;
                        text.as_str()
                    })
                    .collect::<Vec<_>>()
                    .join(" ");
                results.push(format!("{label} ✗ {msg}"));
                had_error = true;
                continue;
            }

            // Snapshot before first edit to this file
            snapshot_file(ctx, &path).await;

            if !path.exists() {
                results.push(format!("{} ✗ File not found", label));
                had_error = true;
                failed_files.insert(path.clone());
                continue;
            }

            // Later edits must see earlier ones. Previously each edit re-read
            // the file, which worked only because each write landed
            // immediately — the very thing that made this non-atomic.
            let content = match staged.get(&path) {
                Some(c) => c.clone(),
                None => match fs::read_to_string(&path).await {
                    Ok(c) => c,
                    Err(e) => {
                        results.push(format!("{} ✗ Read error: {}", label, e));
                        had_error = true;
                        failed_files.insert(path.clone());
                        continue;
                    }
                },
            };

            if edit.replace_all {
                let count = content.matches(&edit.old_string as &str).count();
                if count == 0 {
                    results.push(format!("{} ✗ old_string not found", label));
                    had_error = true;
                    failed_files.insert(path.clone());
                    continue;
                }
                let new_content = content.replace(&edit.old_string, &edit.new_string);
                staged.insert(path.clone(), new_content);
                results.push(format!("{} ✓ Replaced {} occurrence(s)", label, count));
            } else {
                let count = content.matches(&edit.old_string as &str).count();
                match count {
                    0 => {
                        results.push(format!("{} ✗ old_string not found", label));
                        had_error = true;
                        failed_files.insert(path.clone());
                    }
                    1 => {
                        let new_content = content.replacen(&edit.old_string, &edit.new_string, 1);
                        staged.insert(path.clone(), new_content);
                        results.push(format!("{} ✓ Edit applied", label));
                    }
                    n => {
                        results.push(format!(
                            "{} ✗ old_string found {} times — add more context or use replace_all:true",
                            label, n
                        ));
                        had_error = true;
                        failed_files.insert(path.clone());
                    }
                }
            }
        }

        // Commit phase — per-file all-or-nothing.
        //
        // Writing inside the loop meant a batch where edit 3 of 5 failed left
        // the file with edits 1, 2, 4 and 5 applied: a half-finished refactor,
        // on disk, reported only as one "✗" among several "✓". The doc comment
        // promised these were applied atomically; now they are.
        //
        // Granularity is per file, not per batch: a failure in file A should not
        // discard good edits to file B.
        let mut committed = 0usize;
        for (path, content) in &staged {
            if failed_files.contains(path) {
                results.push(format!(
                    "  ↩ {} — no changes written; another edit to this file failed",
                    path.display()
                ));
                continue;
            }
            match super::atomic_write(path, content).await {
                Ok(_) => committed += 1,
                Err(e) => {
                    results.push(format!("  ✗ {} write error: {e}", path.display()));
                    had_error = true;
                }
            }
        }
        let _ = committed;

        let summary = results.join("\n");
        if had_error {
            Ok(ToolOutput::error(summary))
        } else {
            Ok(ToolOutput::success(summary))
        }
    }
}
