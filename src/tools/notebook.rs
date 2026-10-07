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
    // Cell ids only exist from nbformat 4.5 on; older schemas forbid the key,
    // so an absent id must stay absent rather than round-trip as `"id": null`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
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

impl Notebook {
    /// nbformat 4.0-4.4 cells have no `id` and the schema rejects one.
    fn supports_cell_ids(&self) -> bool {
        let major = self
            .extra
            .get("nbformat")
            .and_then(Value::as_u64)
            .unwrap_or(4);
        let minor = self
            .extra
            .get("nbformat_minor")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        major > 4 || minor >= 5
    }
}

/// Label NotebookRead shows for a cell, and which NotebookEdit accepts back.
/// Id-less cells (nbformat < 4.5) get a positional `cell-N`.
fn cell_label(cell: &Cell, index: usize) -> String {
    cell.id
        .clone()
        .unwrap_or_else(|| format!("cell-{}", index + 1))
}

/// Resolve the target cell. A real id always wins; `cell-N` names the Nth cell
/// only when that cell has no id (what `cell_label` shows), and `cell_number`
/// is a plain 1-based position, so notebooks without ids stay editable.
fn target_index(cells: &[Cell], cell_id: Option<&str>, cell_number: Option<u64>) -> Result<usize> {
    let position = |n: u64| -> Result<usize> {
        match usize::try_from(n) {
            Ok(n) if (1..=cells.len()).contains(&n) => Ok(n - 1),
            _ => Err(anyhow!(
                "Cell number {n} is out of range (notebook has {} cells)",
                cells.len()
            )),
        }
    };
    if let Some(id) = cell_id {
        if let Some(i) = cells.iter().position(|c| c.id.as_deref() == Some(id)) {
            return Ok(i);
        }
        // `cell-N` is only the label NotebookRead gives an id-less cell; a
        // stale or made-up id must not retarget whatever cell is Nth.
        if let Some(n) = id
            .strip_prefix("cell-")
            .and_then(|n| n.parse::<usize>().ok())
            && n >= 1
            && cells.get(n - 1).is_some_and(|c| c.id.is_none())
        {
            return Ok(n - 1);
        }
        return Err(anyhow!("Cell not found: {id}"));
    }
    match cell_number {
        Some(n) => position(n),
        None => Err(anyhow!("cell_id or cell_number is required")),
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
            let id = cell_label(cell, i);
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
    /// Cell id to target, as NotebookRead prints it.
    cell_id: Option<String>,
    /// 1-based cell position, the `N` in NotebookRead's "Cell N".
    cell_number: Option<u64>,
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
        Use NotebookRead first to get cell ids; target a cell by cell_id (cells without an id \
        are listed as cell-N) or by its 1-based cell_number. Inserting into an empty notebook \
        needs no target."
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
                    "description": "ID of the target cell as shown by NotebookRead (cells without an id are shown as cell-N)"
                },
                "cell_number": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "1-based position of the target cell; alternative to cell_id"
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
        // Same deny-lists as Write/Edit: a notebook under ~/.ssh is still ~/.ssh.
        if let Some(err) = super::check_protected_path(&path)
            .or_else(|| super::check_write_escape(&path, &ctx.cwd))
        {
            return Ok(err);
        }
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
                let new_source = input
                    .new_source
                    .as_deref()
                    .ok_or_else(|| anyhow!("new_source is required for replace"))?;
                let idx =
                    target_index(&notebook.cells, input.cell_id.as_deref(), input.cell_number)?;
                notebook.cells[idx].source = CellSource::from_text(new_source);
            }
            "insert_before" | "insert_after" => {
                let new_source = input
                    .new_source
                    .as_deref()
                    .ok_or_else(|| anyhow!("new_source is required for insert"))?;
                let cell_type = input.cell_type.as_deref().unwrap_or("code").to_string();

                // An empty notebook has nothing to anchor on; its first cell
                // would otherwise be impossible to create.
                let untargeted = input.cell_id.is_none() && input.cell_number.is_none();
                let insert_at = if untargeted && notebook.cells.is_empty() {
                    0
                } else {
                    let idx =
                        target_index(&notebook.cells, input.cell_id.as_deref(), input.cell_number)?;
                    if input.edit_mode == "insert_before" {
                        idx
                    } else {
                        idx + 1
                    }
                };

                // nbformat v4 requires `metadata` on every cell and `outputs` +
                // `execution_count` on code cells; without them Jupyter refuses
                // to open the notebook. Markdown/raw cells must not carry them.
                let mut extra = std::collections::HashMap::new();
                extra.insert("metadata".to_string(), json!({}));
                if cell_type == "code" {
                    extra.insert("outputs".to_string(), json!([]));
                    extra.insert("execution_count".to_string(), Value::Null);
                }
                let new_cell = Cell {
                    id: notebook
                        .supports_cell_ids()
                        .then(|| uuid::Uuid::new_v4().to_string().chars().take(8).collect()),
                    cell_type,
                    source: CellSource::from_text(new_source),
                    extra,
                };
                notebook.cells.insert(insert_at, new_cell);
            }
            "delete" => {
                let idx =
                    target_index(&notebook.cells, input.cell_id.as_deref(), input.cell_number)?;
                notebook.cells.remove(idx);
            }
            other => return Ok(ToolOutput::error(format!("Unknown edit_mode: {other}"))),
        }

        let updated = serde_json::to_string_pretty(&notebook)?;
        // Without this /rewind silently skipped notebooks. Taken only once the
        // edit succeeded so a rejected one leaves no snapshot behind.
        super::snapshot_file(ctx, &path).await;
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

    fn text(out: &ToolOutput) -> &str {
        let crate::api::types::ToolResultContent::Text { text } = &out.content[0];
        text
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

    /// NotebookEdit took no snapshot, so /rewind never restored a notebook.
    #[tokio::test]
    async fn edit_snapshots_the_notebook_for_rewind() {
        let dir = tempfile::tempdir().unwrap();
        let nb = dir.path().join("a.ipynb");
        std::fs::write(&nb, NB).unwrap();
        let mut c = ctx(dir.path());
        let snaps = dir.path().join("snaps");
        c.snapshot_dir = Some(snaps.clone());

        let out = NotebookEditTool
            .execute(
                json!({"notebook_path": "a.ipynb", "edit_mode": "delete", "cell_id": "c1"}),
                &c,
            )
            .await
            .unwrap();

        assert!(!out.is_error, "{}", text(&out));
        let snap = snaps.join(super::super::snapshot_name(&nb));
        assert_eq!(std::fs::read_to_string(snap).unwrap(), NB);
    }

    /// Inserted cells used to be written as bare {id, cell_type, source},
    /// which nbformat rejects ("missing an expected key: metadata").
    #[tokio::test]
    async fn inserted_cells_carry_the_keys_nbformat_requires() {
        let dir = tempfile::tempdir().unwrap();
        let nb = dir.path().join("a.ipynb");
        std::fs::write(&nb, NB).unwrap();
        for (mode, ty) in [("insert_after", "code"), ("insert_before", "markdown")] {
            let out = NotebookEditTool
                .execute(
                    json!({"notebook_path": "a.ipynb", "edit_mode": mode, "cell_id": "c1",
                           "new_source": format!("new {ty}"), "cell_type": ty}),
                    &ctx(dir.path()),
                )
                .await
                .unwrap();
            assert!(!out.is_error);
        }
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&nb).unwrap()).unwrap();
        let cells = v["cells"].as_array().unwrap();
        assert_eq!(cells.len(), 3);
        let md = &cells[0];
        assert_eq!(md["cell_type"], "markdown");
        assert_eq!(md["metadata"], json!({}));
        assert!(md.get("outputs").is_none() && md.get("execution_count").is_none());
        let code = &cells[2];
        assert_eq!(code["cell_type"], "code");
        assert_eq!(code["metadata"], json!({}));
        assert_eq!(code["outputs"], json!([]));
        assert!(code.get("execution_count").is_some_and(Value::is_null));
    }

    /// nbformat < 4.5 cells have no id. Every edit used to fail with "Cell not
    /// found", and a write would have emitted schema-invalid `"id": null`.
    #[tokio::test]
    async fn cells_without_ids_are_editable_and_stay_id_less() {
        const OLD: &str = r##"{"cells":[{"cell_type":"code","source":["a = 1\n"],"metadata":{},"outputs":[],"execution_count":null},{"cell_type":"markdown","source":["# b"],"metadata":{}}],"nbformat":4,"nbformat_minor":4,"metadata":{}}"##;
        let dir = tempfile::tempdir().unwrap();
        let nb = dir.path().join("old.ipynb");
        std::fs::write(&nb, OLD).unwrap();
        let c = ctx(dir.path());

        let listing = NotebookReadTool
            .execute(json!({"notebook_path": "old.ipynb"}), &c)
            .await
            .unwrap();
        assert!(text(&listing).contains("id=cell-1"), "{}", text(&listing));
        assert!(text(&listing).contains("id=cell-2"), "{}", text(&listing));

        for edit in [
            json!({"edit_mode": "replace", "cell_id": "cell-1", "new_source": "a = 2"}),
            json!({"edit_mode": "insert_after", "cell_number": 2, "new_source": "c = 3"}),
            json!({"edit_mode": "delete", "cell_id": "cell-2"}),
        ] {
            let mut input = edit.clone();
            input["notebook_path"] = json!("old.ipynb");
            let out = NotebookEditTool.execute(input, &c).await.unwrap();
            assert!(!out.is_error, "{edit}: {}", text(&out));
        }

        let text = std::fs::read_to_string(&nb).unwrap();
        let v: Value = serde_json::from_str(&text).unwrap();
        let cells = v["cells"].as_array().unwrap();
        assert_eq!(cells.len(), 2);
        assert_eq!(cells[0]["source"], json!(["a = 2"]));
        assert_eq!(cells[1]["source"], json!(["c = 3"]));
        assert!(
            cells.iter().all(|c| c.get("id").is_none()),
            "a 4.4 notebook must not gain id keys: {text}"
        );

        let out = NotebookEditTool
            .execute(
                json!({"notebook_path": "old.ipynb", "edit_mode": "delete", "cell_number": 9}),
                &c,
            )
            .await;
        assert!(out.is_err(), "out-of-range cell_number must be rejected");
    }

    /// In a notebook whose cells have ids, `cell-1` is a stale or made-up id,
    /// not "the first cell": it used to delete whatever cell came first.
    #[tokio::test]
    async fn an_unknown_cell_n_id_is_not_a_position_when_cells_have_ids() {
        const NEW: &str = r##"{"cells":[{"id":"abc","cell_type":"markdown","source":["# keep"],"metadata":{}}],"nbformat":4,"nbformat_minor":5,"metadata":{}}"##;
        let dir = tempfile::tempdir().unwrap();
        let nb = dir.path().join("new.ipynb");
        std::fs::write(&nb, NEW).unwrap();
        let out = NotebookEditTool
            .execute(
                json!({"notebook_path": "new.ipynb", "edit_mode": "delete", "cell_id": "cell-1"}),
                &ctx(dir.path()),
            )
            .await;
        match out {
            Ok(o) => assert!(o.is_error, "{}", text(&o)),
            Err(e) => assert!(e.to_string().contains("Cell not found"), "{e}"),
        }
        assert_eq!(std::fs::read_to_string(&nb).unwrap(), NEW);
    }

    #[tokio::test]
    async fn the_first_cell_can_be_inserted_into_an_empty_notebook() {
        let dir = tempfile::tempdir().unwrap();
        let nb = dir.path().join("e.ipynb");
        std::fs::write(
            &nb,
            r#"{"cells":[],"nbformat":4,"nbformat_minor":5,"metadata":{}}"#,
        )
        .unwrap();
        let out = NotebookEditTool
            .execute(
                json!({"notebook_path": "e.ipynb", "edit_mode": "insert_after", "new_source": "x"}),
                &ctx(dir.path()),
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{}", text(&out));
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&nb).unwrap()).unwrap();
        let cells = v["cells"].as_array().unwrap();
        assert_eq!(cells.len(), 1);
        assert!(cells[0]["id"].is_string(), "4.5 cells get an id");
    }
}
