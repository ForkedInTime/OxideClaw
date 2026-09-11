/// NotebookEditTool — port of notebook.ts
/// Read and edit Jupyter notebook (.ipynb) cells.
use super::{Tool, ToolContext, ToolOutput, async_trait};
use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub struct NotebookReadTool;
pub struct NotebookEditTool;

// ── Notebook JSON types ───────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize)]
struct Notebook {
    cells: Vec<Cell>,
    #[serde(flatten)]
    extra: std::collections::HashMap<String, Value>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct Cell {
    id: Option<String>,
    cell_type: String,
    source: CellSource,
    #[serde(flatten)]
    extra: std::collections::HashMap<String, Value>,
}

/// source can be a string or an array of strings in .ipynb
#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(untagged)]
enum CellSource {
    Lines(Vec<String>),
    Text(String),
}

impl std::fmt::Display for CellSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CellSource::Lines(v) => f.write_str(&v.join("")),
            CellSource::Text(s) => f.write_str(s),
        }
    }
}

impl CellSource {
    fn from_text(s: &str) -> Self {
        // Store as lines array (each line ends with \n except the last)
        let lines: Vec<String> = if s.is_empty() {
            vec![]
        } else {
            let mut lines: Vec<String> = s.lines().map(|l| l.to_string()).collect();
            // Add \n to all but the last line
            for i in 0..lines.len().saturating_sub(1) {
                lines[i].push('\n');
            }
            lines
        };
        CellSource::Lines(lines)
    }
}

// ── NotebookRead ──────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct ReadInput {
    notebook_path: String,
}

#[async_trait]
impl Tool for NotebookReadTool {
    fn name(&self) -> &str {
        "NotebookRead"
    }

    fn description(&self) -> &str {
        "Read the contents of a Jupyter notebook (.ipynb) file. Returns each cell's id, \
        type (code/markdown), and source. Outputs from previous executions are not included."
    }

    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "notebook_path": {
                    "type": "string",
                    "description": "Path to the .ipynb notebook file"
                }
            },
            "required": ["notebook_path"]
        })
    }

    async fn execute(&self, input: serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let input: ReadInput = serde_json::from_value(input)?;
        let path = match super::file_read::resolve_path(&input.notebook_path, &ctx.cwd) {
            Ok(p) => p,
            Err(e) => return Ok(ToolOutput::error(e.to_string())),
        };
        if let Some(err) = super::check_sensitive_path_resolved(&path, super::SensitiveOp::Read) {
            return Ok(err);
        }

        let content = tokio::fs::read_to_string(&path)
            .await
            .map_err(|e| anyhow!("Cannot read {}: {}", path.display(), e))?;

        let notebook: Notebook =
            serde_json::from_str(&content).map_err(|e| anyhow!("Invalid notebook JSON: {e}"))?;

        let mut out = String::new();
        for (i, cell) in notebook.cells.iter().enumerate() {
            let id = cell.id.as_deref().unwrap_or("(no id)");
            let source = cell.source.to_string();
            out.push_str(&format!(
                "Cell {} [{}] id={}\n{}\n\n",
                i + 1,
                cell.cell_type,
                id,
                source
            ));
        }

        if out.is_empty() {
            out = "Notebook has no cells.".to_string();
        }

        Ok(ToolOutput::success(out))
    }
}

// ── NotebookEdit ──────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct EditInput {
    notebook_path: String,
    /// Cell id to target (for replace/delete). Required for replace and delete.
    cell_id: Option<String>,
    /// New source content (for replace/insert).
    new_source: Option<String>,
    /// Cell type for insert: "code" or "markdown" (default "code")
    cell_type: Option<String>,
    /// Operation: "replace" | "insert_before" | "insert_after" | "delete"
    edit_mode: String,
}

#[async_trait]
impl Tool for NotebookEditTool {
    fn name(&self) -> &str {
        "NotebookEdit"
    }

    fn description(&self) -> &str {
        "Edit a Jupyter notebook cell. Supports replace, insert_before, insert_after, and delete. \
        Use NotebookRead first to get cell ids."
    }

    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "notebook_path": {
                    "type": "string",
                    "description": "Path to the .ipynb notebook file"
                },
                "cell_id": {
                    "type": "string",
                    "description": "ID of the target cell (required for replace, insert_before, insert_after, delete)"
                },
                "new_source": {
                    "type": "string",
                    "description": "New source content for replace or insert operations"
                },
                "cell_type": {
                    "type": "string",
                    "enum": ["code", "markdown"],
                    "description": "Cell type for insert operations (default: code)"
                },
                "edit_mode": {
                    "type": "string",
                    "enum": ["replace", "insert_before", "insert_after", "delete"],
                    "description": "Operation to perform"
                }
            },
            "required": ["notebook_path", "edit_mode"]
        })
    }

    async fn execute(&self, input: serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let input: EditInput = serde_json::from_value(input)?;
        let path = match super::file_read::resolve_path(&input.notebook_path, &ctx.cwd) {
            Ok(p) => p,
            Err(e) => return Ok(ToolOutput::error(e.to_string())),
        };
        // Same deny-list as Write/Edit: a notebook under ~/.ssh is still ~/.ssh.
        if let Some(err) = super::check_sensitive_path_resolved(&path, super::SensitiveOp::Write) {
            return Ok(err);
        }

        let content = tokio::fs::read_to_string(&path)
            .await
            .map_err(|e| anyhow!("Cannot read {}: {}", path.display(), e))?;

        let mut notebook: Notebook =
            serde_json::from_str(&content).map_err(|e| anyhow!("Invalid notebook JSON: {e}"))?;

        match input.edit_mode.as_str() {
            "replace" => {
                let cell_id = input
                    .cell_id
                    .as_deref()
                    .ok_or_else(|| anyhow!("cell_id is required for replace"))?;
                let new_source = input
                    .new_source
                    .as_deref()
                    .ok_or_else(|| anyhow!("new_source is required for replace"))?;

                let cell = notebook
                    .cells
                    .iter_mut()
                    .find(|c| c.id.as_deref() == Some(cell_id))
                    .ok_or_else(|| anyhow!("Cell not found: {cell_id}"))?;

                cell.source = CellSource::from_text(new_source);
            }
            "insert_before" | "insert_after" => {
                let cell_id = input
                    .cell_id
                    .as_deref()
                    .ok_or_else(|| anyhow!("cell_id is required for insert"))?;
                let new_source = input
                    .new_source
                    .as_deref()
                    .ok_or_else(|| anyhow!("new_source is required for insert"))?;
                let cell_type = input.cell_type.as_deref().unwrap_or("code").to_string();

                let idx = notebook
                    .cells
                    .iter()
                    .position(|c| c.id.as_deref() == Some(cell_id))
                    .ok_or_else(|| anyhow!("Cell not found: {cell_id}"))?;

                let new_cell = Cell {
                    id: Some(uuid::Uuid::new_v4().to_string().chars().take(8).collect()),
                    cell_type,
                    source: CellSource::from_text(new_source),
                    extra: Default::default(),
                };

                let insert_at = if input.edit_mode == "insert_before" {
                    idx
                } else {
                    idx + 1
                };
                notebook.cells.insert(insert_at, new_cell);
            }
            "delete" => {
                let cell_id = input
                    .cell_id
                    .as_deref()
                    .ok_or_else(|| anyhow!("cell_id is required for delete"))?;

                let idx = notebook
                    .cells
                    .iter()
                    .position(|c| c.id.as_deref() == Some(cell_id))
                    .ok_or_else(|| anyhow!("Cell not found: {cell_id}"))?;

                notebook.cells.remove(idx);
            }
            other => return Ok(ToolOutput::error(format!("Unknown edit_mode: {other}"))),
        }

        let updated = serde_json::to_string_pretty(&notebook)?;
        // Atomic like the other file tools: a crash mid-write must not leave
        // a truncated notebook behind.
        super::atomic_write(&path, &updated)
            .await
            .map_err(|e| anyhow!("Cannot write {}: {}", path.display(), e))?;

        Ok(ToolOutput::success(format!(
            "Notebook {} updated successfully.",
            path.display()
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NB: &str = r#"{"cells":[{"id":"c1","cell_type":"code","source":["x = 1\n"],"metadata":{}}],"nbformat":4,"nbformat_minor":5,"metadata":{}}"#;

    fn ctx(dir: &std::path::Path) -> ToolContext {
        ToolContext::new(dir.to_path_buf())
    }

    /// `"~"` alone used to slice `p[2..]` on a 1-byte string and panic.
    #[tokio::test]
    async fn a_bare_tilde_path_does_not_panic() {
        let dir = tempfile::tempdir().unwrap();
        let out = NotebookReadTool
            .execute(json!({"notebook_path": "~"}), &ctx(dir.path()))
            .await;
        assert!(out.is_ok() || out.is_err()); // reaching here is the assertion
    }

    /// Notebook tools bypassed the deny-list the other file tools honour.
    #[tokio::test]
    async fn read_refuses_a_private_key_named_file() {
        let dir = tempfile::tempdir().unwrap();
        let key = dir.path().join("id_rsa");
        std::fs::write(&key, NB).unwrap();
        let out = NotebookReadTool
            .execute(
                json!({"notebook_path": key.to_string_lossy()}),
                &ctx(dir.path()),
            )
            .await
            .unwrap();
        assert!(out.is_error, "must refuse to read a private-key path");
    }

    #[tokio::test]
    async fn edit_refuses_a_path_inside_a_secrets_directory() {
        let dir = tempfile::tempdir().unwrap();
        let ssh = dir.path().join(".ssh");
        std::fs::create_dir(&ssh).unwrap();
        let nb = ssh.join("notes.ipynb");
        std::fs::write(&nb, NB).unwrap();
        let out = NotebookEditTool
            .execute(
                json!({"notebook_path": nb.to_string_lossy(), "edit_mode": "delete", "cell_id": "c1"}),
                &ctx(dir.path()),
            )
            .await
            .unwrap();
        assert!(out.is_error, "must refuse to write under .ssh");
        assert_eq!(
            std::fs::read_to_string(&nb).unwrap(),
            NB,
            "file must be untouched"
        );
    }

    #[tokio::test]
    async fn edit_replaces_a_cell_in_an_ordinary_notebook() {
        let dir = tempfile::tempdir().unwrap();
        let nb = dir.path().join("a.ipynb");
        std::fs::write(&nb, NB).unwrap();
        let out = NotebookEditTool
            .execute(
                json!({"notebook_path": "a.ipynb", "edit_mode": "replace", "cell_id": "c1", "new_source": "y = 2"}),
                &ctx(dir.path()),
            )
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(std::fs::read_to_string(&nb).unwrap().contains("y = 2"));
    }
}
