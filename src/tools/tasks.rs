/// Task tools — port of TaskCreateTool, TaskGetTool, TaskListTool, TaskUpdateTool, TaskStopTool
/// In-memory task registry shared across all tool instances via Arc<Mutex<>>.
use super::{Tool, ToolContext, ToolOutput, async_trait};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

/// Task status — mirrors the status enum in tasks.ts
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Pending,
    InProgress,
    Completed,
    Failed,
    Stopped,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub subject: String,
    pub description: String,
    pub status: TaskStatus,
    pub output: Option<String>,
    pub active_form: Option<String>,
    /// Insertion order, so a full registry evicts its oldest finished task.
    #[serde(skip)]
    pub seq: u64,
}

/// Shared task registry — passed to all task tools at construction time.
pub type TaskRegistry = Arc<Mutex<HashMap<String, Task>>>;

pub fn new_registry() -> TaskRegistry {
    Arc::new(Mutex::new(HashMap::new()))
}

/// The registry is in-memory and model-driven; without a ceiling a long
/// session (or a looping model) grows it without bound.
pub const MAX_TASKS: usize = 1000;

/// Insert a task. A full registry makes room by dropping its oldest
/// completed, failed or stopped task: no tool deletes tasks, so refusing
/// outright left TaskCreate failing for the rest of the session. Refuses
/// only when every task is still pending or in progress.
fn insert_task(registry: &TaskRegistry, mut task: Task) -> Result<()> {
    static NEXT_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let mut reg = registry.lock().unwrap_or_else(|e| e.into_inner());
    if reg.len() >= MAX_TASKS {
        let oldest_done = reg
            .values()
            .filter(|t| {
                matches!(
                    t.status,
                    TaskStatus::Completed | TaskStatus::Failed | TaskStatus::Stopped
                )
            })
            .min_by_key(|t| t.seq)
            .map(|t| t.id.clone());
        let Some(id) = oldest_done else {
            anyhow::bail!(
                "Task registry is full ({MAX_TASKS} tasks, all pending or in progress). \
                 Mark finished ones completed, failed or stopped (TaskUpdate or TaskStop) \
                 to make room."
            );
        };
        reg.remove(&id);
    }
    task.seq = NEXT_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    reg.insert(task.id.clone(), task);
    Ok(())
}

// ── TaskCreate ────────────────────────────────────────────────────────────────

pub struct TaskCreateTool {
    pub registry: TaskRegistry,
}

#[derive(Deserialize)]
struct CreateInput {
    subject: String,
    description: String,
    #[serde(default)]
    active_form: Option<String>,
}

#[async_trait]
impl Tool for TaskCreateTool {
    fn name(&self) -> &str {
        "TaskCreate"
    }

    fn description(&self) -> &str {
        "Create a new task in the task list. Returns the task id. \
        Use TaskUpdate to set status/output, TaskStop to mark it stopped."
    }

    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "subject":     { "type": "string", "description": "Short title for the task" },
                "description": { "type": "string", "description": "What needs to be done" },
                "active_form": { "type": "string", "description": "Present-continuous label shown while in_progress (e.g. 'Running tests')" }
            },
            "required": ["subject", "description"]
        })
    }

    async fn execute(&self, input: serde_json::Value, _ctx: &ToolContext) -> Result<ToolOutput> {
        let input: CreateInput = serde_json::from_value(input)?;
        let id = Uuid::new_v4().to_string();
        let task = Task {
            id: id.clone(),
            subject: input.subject,
            description: input.description,
            status: TaskStatus::Pending,
            output: None,
            active_form: input.active_form,
            seq: 0,
        };
        if let Err(e) = insert_task(&self.registry, task) {
            return Ok(ToolOutput::error(e.to_string()));
        }
        Ok(ToolOutput::success(
            json!({ "task": { "id": id } }).to_string(),
        ))
    }
}

// ── TaskGet ───────────────────────────────────────────────────────────────────

pub struct TaskGetTool {
    pub registry: TaskRegistry,
}

#[derive(Deserialize)]
struct GetInput {
    task_id: String,
}

#[async_trait]
impl Tool for TaskGetTool {
    fn name(&self) -> &str {
        "TaskGet"
    }

    fn description(&self) -> &str {
        "Get the current status and output of a task by its id."
    }

    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "task_id": { "type": "string", "description": "The task id returned by TaskCreate" }
            },
            "required": ["task_id"]
        })
    }

    async fn execute(&self, input: serde_json::Value, _ctx: &ToolContext) -> Result<ToolOutput> {
        let input: GetInput = serde_json::from_value(input)?;
        let registry = self.registry.lock().unwrap_or_else(|e| e.into_inner());
        match registry.get(&input.task_id) {
            Some(task) => Ok(ToolOutput::success(serde_json::to_string(task)?)),
            None => Ok(ToolOutput::error(format!(
                "Task not found: {}",
                input.task_id
            ))),
        }
    }
}

// ── TaskList ──────────────────────────────────────────────────────────────────

pub struct TaskListTool {
    pub registry: TaskRegistry,
}

#[async_trait]
impl Tool for TaskListTool {
    fn name(&self) -> &str {
        "TaskList"
    }

    fn description(&self) -> &str {
        "List all tasks in the current session with their status and subject."
    }

    fn input_schema(&self) -> serde_json::Value {
        json!({ "type": "object", "properties": {} })
    }

    async fn execute(&self, _input: serde_json::Value, _ctx: &ToolContext) -> Result<ToolOutput> {
        let registry = self.registry.lock().unwrap_or_else(|e| e.into_inner());
        if registry.is_empty() {
            return Ok(ToolOutput::success("No tasks."));
        }
        let mut tasks: Vec<&Task> = registry.values().collect();
        tasks.sort_by(|a, b| a.subject.cmp(&b.subject));
        let lines: Vec<String> = tasks
            .iter()
            .map(|t| format!("[{:?}] {} — {}", t.status, t.id, t.subject))
            .collect();
        Ok(ToolOutput::success(lines.join("\n")))
    }
}

// ── TaskUpdate ────────────────────────────────────────────────────────────────

pub struct TaskUpdateTool {
    pub registry: TaskRegistry,
}

#[derive(Deserialize)]
struct UpdateInput {
    task_id: String,
    status: Option<TaskStatus>,
    output: Option<String>,
}

#[async_trait]
impl Tool for TaskUpdateTool {
    fn name(&self) -> &str {
        "TaskUpdate"
    }

    fn description(&self) -> &str {
        "Update the status and/or output of an existing task."
    }

    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "task_id": { "type": "string", "description": "The task id returned by TaskCreate" },
                "status":  { "type": "string", "enum": ["pending","in_progress","completed","failed","stopped"], "description": "New status for the task" },
                "output":  { "type": "string", "description": "Result or progress message" }
            },
            "required": ["task_id"]
        })
    }

    async fn execute(&self, input: serde_json::Value, _ctx: &ToolContext) -> Result<ToolOutput> {
        let input: UpdateInput = serde_json::from_value(input)?;
        let mut registry = self.registry.lock().unwrap_or_else(|e| e.into_inner());
        match registry.get_mut(&input.task_id) {
            Some(task) => {
                if let Some(s) = input.status {
                    task.status = s;
                }
                if let Some(o) = input.output {
                    task.output = Some(o);
                }
                Ok(ToolOutput::success(format!(
                    "Task {} updated.",
                    input.task_id
                )))
            }
            None => Ok(ToolOutput::error(format!(
                "Task not found: {}",
                input.task_id
            ))),
        }
    }
}

// ── TaskStop ──────────────────────────────────────────────────────────────────

pub struct TaskStopTool {
    pub registry: TaskRegistry,
}

#[derive(Deserialize)]
struct StopInput {
    task_id: String,
}

#[async_trait]
impl Tool for TaskStopTool {
    fn name(&self) -> &str {
        "TaskStop"
    }

    fn description(&self) -> &str {
        "Mark a task as stopped."
    }

    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "task_id": { "type": "string", "description": "The task id returned by TaskCreate" }
            },
            "required": ["task_id"]
        })
    }

    async fn execute(&self, input: serde_json::Value, _ctx: &ToolContext) -> Result<ToolOutput> {
        let input: StopInput = serde_json::from_value(input)?;
        let mut registry = self.registry.lock().unwrap_or_else(|e| e.into_inner());
        match registry.get_mut(&input.task_id) {
            Some(task) => {
                task.status = TaskStatus::Stopped;
                Ok(ToolOutput::success(format!(
                    "Task {} stopped.",
                    input.task_id
                )))
            }
            None => Ok(ToolOutput::error(format!(
                "Task not found: {}",
                input.task_id
            ))),
        }
    }
}

// ── TaskOutput ────────────────────────────────────────────────────────────────

pub struct TaskOutputTool {
    pub registry: TaskRegistry,
}

#[derive(Deserialize)]
struct OutputInput {
    task_id: String,
    /// Output text to append or set
    output: String,
    /// If true, also mark the task as completed
    #[serde(default)]
    complete: bool,
    /// If true, also mark the task as failed
    #[serde(default)]
    error: bool,
}

#[async_trait]
impl Tool for TaskOutputTool {
    fn name(&self) -> &str {
        "TaskOutput"
    }

    fn description(&self) -> &str {
        "Set or append output to a task. Optionally mark it complete or failed. \
        Use this to record the result of an in-progress task."
    }

    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "task_id":  { "type": "string", "description": "Task id returned by TaskCreate" },
                "output":   { "type": "string", "description": "Output text to record" },
                "complete": { "type": "boolean", "description": "Also mark the task as completed", "default": false },
                "error":    { "type": "boolean", "description": "Also mark the task as failed",    "default": false }
            },
            "required": ["task_id", "output"]
        })
    }

    async fn execute(&self, input: serde_json::Value, _ctx: &ToolContext) -> Result<ToolOutput> {
        let input: OutputInput = serde_json::from_value(input)?;
        let mut registry = self.registry.lock().unwrap_or_else(|e| e.into_inner());
        match registry.get_mut(&input.task_id) {
            Some(task) => {
                task.output = Some(input.output);
                if input.error {
                    task.status = TaskStatus::Failed;
                } else if input.complete {
                    task.status = TaskStatus::Completed;
                }
                Ok(ToolOutput::success(format!(
                    "Task {} output recorded.",
                    input.task_id
                )))
            }
            None => Ok(ToolOutput::error(format!(
                "Task not found: {}",
                input.task_id
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(id: &str) -> Task {
        Task {
            id: id.into(),
            subject: "s".into(),
            description: "d".into(),
            status: TaskStatus::Pending,
            output: None,
            active_form: None,
            seq: 0,
        }
    }

    #[test]
    fn registry_refuses_inserts_past_the_cap() {
        let reg = new_registry();
        for i in 0..MAX_TASKS {
            insert_task(&reg, task(&i.to_string())).unwrap();
        }
        let err = insert_task(&reg, task("one-too-many")).unwrap_err();
        assert!(err.to_string().contains("full"), "{err}");
        assert_eq!(reg.lock().unwrap().len(), MAX_TASKS);
    }

    /// Finishing tasks did not free a slot and no tool deletes one, so after
    /// MAX_TASKS creations TaskCreate failed for the rest of the session.
    #[test]
    fn a_full_registry_drops_its_oldest_finished_task() {
        let reg = new_registry();
        for i in 0..MAX_TASKS {
            insert_task(&reg, task(&i.to_string())).unwrap();
        }
        for id in ["7", "3", "900"] {
            reg.lock().unwrap().get_mut(id).unwrap().status = TaskStatus::Completed;
        }
        reg.lock().unwrap().get_mut("5").unwrap().status = TaskStatus::Stopped;

        insert_task(&reg, task("new")).unwrap();
        let r = reg.lock().unwrap();
        assert_eq!(r.len(), MAX_TASKS);
        assert!(!r.contains_key("3"), "the oldest finished task goes first");
        assert!(r.contains_key("0") && r.contains_key("5") && r.contains_key("new"));
    }
}
