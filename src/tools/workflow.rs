/// WorkflowTool — port of workflow.ts
/// Executes named multi-step workflows defined in .claude/workflows/ as JSON/YAML files.
/// Each workflow is a sequence of steps: { prompt: string, tool?: string, args?: object }
use super::{Tool, ToolContext, ToolOutput, async_trait};
use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::PathBuf;

pub struct WorkflowTool;

#[derive(Deserialize)]
struct Input {
    /// Workflow name (filename without extension) or absolute path
    name: String,
    /// Optional key=value overrides for workflow variables
    #[serde(default)]
    vars: std::collections::HashMap<String, String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct WorkflowStep {
    /// Human-readable description of this step
    #[serde(default)]
    description: String,
    /// Prompt to send (may contain {{var}} placeholders)
    #[serde(default)]
    prompt: String,
    /// Optional tool to invoke directly (bypasses LLM)
    #[serde(default)]
    tool: Option<String>,
    /// Arguments for the tool (if tool is set)
    #[serde(default)]
    args: Option<Value>,
}

#[derive(Debug, Serialize, Deserialize)]
struct Workflow {
    name: String,
    #[serde(default)]
    description: String,
    steps: Vec<WorkflowStep>,
}

#[async_trait]
impl Tool for WorkflowTool {
    fn name(&self) -> &str {
        "Workflow"
    }

    fn description(&self) -> &str {
        "Execute a named workflow from .claude/workflows/. Workflows are JSON files \
        containing a sequence of steps with prompts and optional tool calls. \
        Use this to run repeatable multi-step processes."
    }

    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Workflow name (filename without extension, e.g. 'deploy') or path"
                },
                "vars": {
                    "type": "object",
                    "description": "Key-value pairs to substitute into {{var}} placeholders in the workflow",
                    "additionalProperties": { "type": "string" }
                }
            },
            "required": ["name"]
        })
    }

    async fn execute(&self, input: serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let input: Input = serde_json::from_value(input)?;

        let workflow = load_workflow(&ctx.cwd, &input.name).await?;
        let vars = &input.vars;

        let mut output = format!("Workflow: {}\n", workflow.name);
        if !workflow.description.is_empty() {
            output.push_str(&format!("Description: {}\n", workflow.description));
        }
        output.push_str(&format!("Steps: {}\n\n", workflow.steps.len()));

        for (i, step) in workflow.steps.iter().enumerate() {
            let prompt = substitute_vars(&step.prompt, vars);
            let desc = if step.description.is_empty() {
                format!("Step {}", i + 1)
            } else {
                step.description.clone()
            };

            output.push_str(&format!("--- {desc} ---\n"));

            if let Some(tool_name) = &step.tool {
                let args = step.args.clone().unwrap_or(json!({}));
                output.push_str(&format!("Tool: {tool_name}\nArgs: {args}\n\n"));
            } else if !prompt.is_empty() {
                output.push_str(&format!("Prompt: {prompt}\n\n"));
            }
        }

        output.push_str(
            "\n[Workflow loaded. Steps listed above — execute each step in order using the \
            tools and prompts specified.]",
        );

        Ok(ToolOutput::success(output))
    }
}

async fn load_workflow(cwd: &std::path::Path, name: &str) -> Result<Workflow> {
    // Candidate paths: direct path, then .claude/workflows/ in the project,
    // the config dir, and Claude Code's ~/.claude (read-only).
    let candidates: Vec<PathBuf> = {
        let mut c = Vec::new();

        // Direct path
        let direct = PathBuf::from(name);
        if direct.is_absolute() {
            c.push(direct);
        } else {
            // Try .json then .yaml/.yml in local and home directories
            let dirs = [cwd.join(".claude"), crate::config::Config::config_dir()]
                .into_iter()
                .chain(crate::config::Config::claude_code_dir())
                .map(|d| d.join("workflows"));
            for dir in dirs {
                for ext in &["json", "yaml", "yml"] {
                    c.push(dir.join(format!("{name}.{ext}")));
                }
            }
        }
        c
    };

    for path in &candidates {
        if path.exists() {
            let content = tokio::fs::read_to_string(path).await?;
            // Try JSON first, then YAML (serde_json can parse both if it's just JSON)
            let workflow: Workflow = if path.extension().and_then(|e| e.to_str()) == Some("json") {
                serde_json::from_str(&content)
                    .map_err(|e| anyhow!("Invalid workflow JSON in {}: {e}", path.display()))?
            } else {
                // YAML fallback: treat as JSON (basic YAML is valid JSON superset concern —
                // for simplicity we just try serde_json and return a clear error)
                serde_json::from_str(&content).map_err(|_| {
                    anyhow!(
                        "YAML workflows require serde_yaml. Found at {}.\n\
                        Convert to JSON format or add serde_yaml dependency.",
                        path.display()
                    )
                })?
            };
            return Ok(workflow);
        }
    }

    Err(anyhow!(
        "Workflow '{}' not found. Searched in .claude/workflows/, the config dir's workflows/ \
         and ~/.claude/workflows/.",
        name
    ))
}

fn substitute_vars(template: &str, vars: &std::collections::HashMap<String, String>) -> String {
    let mut result = template.to_string();
    for (key, value) in vars {
        result = result.replace(&format!("{{{{{key}}}}}"), value);
    }
    result
}
