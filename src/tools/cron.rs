/// Cron tools — port of cron.ts
/// CronCreate, CronDelete, CronList: schedule recurring prompts via cron expressions.
/// Jobs stored in ~/.claude/cron_jobs.json
use super::{Tool, ToolContext, ToolOutput, async_trait};
use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

// ── Job store ─────────────────────────────────────────────────────────────────

/// First `n` characters — a byte slice would panic on a multi-byte boundary.
fn prefix_chars(s: &str, n: usize) -> &str {
    match s.char_indices().nth(n) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CronJob {
    pub id: String,
    pub schedule: String,
    pub prompt: String,
    pub description: String,
    pub created_at: u64,
    pub last_run: Option<u64>,
    pub enabled: bool,
}

pub type CronStore = Arc<Mutex<Vec<CronJob>>>;

fn jobs_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".claude")
        .join("cron_jobs.json")
}

pub fn load_jobs() -> Vec<CronJob> {
    let path = jobs_path();
    if !path.exists() {
        return Vec::new();
    }
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save_jobs(jobs: &[CronJob]) -> Result<()> {
    let path = jobs_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(jobs)?;
    // Sibling temp file + rename: a crash mid-write cannot truncate the store.
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// Validate a 5-field cron expression (minute hour day month weekday).
/// Returns Ok(()) if valid, Err with explanation otherwise.
pub fn validate_cron(expr: &str) -> Result<()> {
    let fields: Vec<&str> = expr.split_whitespace().collect();
    if fields.len() != 5 {
        return Err(anyhow!(
            "Cron expression must have exactly 5 fields: minute hour day month weekday. Got: '{expr}'"
        ));
    }

    let limits = [(0u32, 59u32), (0, 23), (1, 31), (1, 12), (0, 6)];
    let names = ["minute", "hour", "day", "month", "weekday"];

    for (i, &field) in fields.iter().enumerate() {
        let (min, max) = limits[i];
        validate_cron_field(field, min, max)
            .map_err(|e| anyhow!("Invalid {} field '{}': {}", names[i], field, e))?;
    }
    Ok(())
}

fn validate_cron_field(field: &str, min: u32, max: u32) -> Result<()> {
    if field == "*" {
        return Ok(());
    }

    // Handle */step
    if let Some(step_str) = field.strip_prefix("*/") {
        let step: u32 = step_str
            .parse()
            .map_err(|_| anyhow!("invalid step value"))?;
        if step == 0 {
            return Err(anyhow!("step cannot be zero"));
        }
        if step > max {
            return Err(anyhow!("step {step} exceeds the field maximum {max}"));
        }
        return Ok(());
    }

    // Handle ranges and lists
    for part in field.split(',') {
        if part.contains('-') {
            let mut parts = part.splitn(2, '-');
            let lo: u32 = parts
                .next()
                .unwrap()
                .parse()
                .map_err(|_| anyhow!("invalid range start"))?;
            let hi: u32 = parts
                .next()
                .unwrap_or("0")
                .parse()
                .map_err(|_| anyhow!("invalid range end"))?;
            if lo < min || hi > max || lo > hi {
                return Err(anyhow!("range {lo}-{hi} out of bounds [{min}-{max}]"));
            }
        } else {
            let v: u32 = part.parse().map_err(|_| anyhow!("expected a number"))?;
            if v < min || v > max {
                return Err(anyhow!("value {v} out of bounds [{min}-{max}]"));
            }
        }
    }
    Ok(())
}

// ── CronCreate ────────────────────────────────────────────────────────────────

pub struct CronCreateTool {
    pub store: CronStore,
}

#[derive(Deserialize)]
struct CreateInput {
    schedule: String,
    prompt: String,
    #[serde(default)]
    description: String,
}

#[async_trait]
impl Tool for CronCreateTool {
    fn name(&self) -> &str {
        "CronCreate"
    }

    fn description(&self) -> &str {
        "Record a cron job (a prompt plus a 5-field schedule) in ~/.claude/cron_jobs.json. \
        RustyClaw stores and lists these but does not run them itself yet; an external \
        scheduler must read the file. Schedule syntax: minute hour day month weekday. \
        Examples: '*/15 * * * *' (every 15 minutes), '0 9 * * 1' (Mondays at 9am)."
    }

    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "schedule": {
                    "type": "string",
                    "description": "5-field cron expression (minute hour day month weekday)"
                },
                "prompt": {
                    "type": "string",
                    "description": "The prompt to send when the cron fires"
                },
                "description": {
                    "type": "string",
                    "description": "Human-readable description of what this cron does"
                }
            },
            "required": ["schedule", "prompt"]
        })
    }

    async fn execute(&self, input: serde_json::Value, _ctx: &ToolContext) -> Result<ToolOutput> {
        let input: CreateInput = serde_json::from_value(input)?;

        validate_cron(&input.schedule)?;

        let id = Uuid::new_v4().to_string();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let job = CronJob {
            id: id.clone(),
            schedule: input.schedule.clone(),
            prompt: input.prompt,
            description: input.description,
            created_at: now,
            last_run: None,
            enabled: true,
        };

        {
            let mut jobs = self.store.lock().unwrap_or_else(|e| e.into_inner());
            jobs.push(job);
            save_jobs(&jobs)?;
        }

        Ok(ToolOutput::success(format!(
            "Cron job created: id={id} schedule=\"{}\"",
            input.schedule
        )))
    }
}

// ── CronDelete ────────────────────────────────────────────────────────────────

pub struct CronDeleteTool {
    pub store: CronStore,
}

#[derive(Deserialize)]
struct DeleteInput {
    id: String,
}

#[async_trait]
impl Tool for CronDeleteTool {
    fn name(&self) -> &str {
        "CronDelete"
    }

    fn description(&self) -> &str {
        "Delete a cron job by its id. Use CronList to see existing job ids."
    }

    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "The cron job id to delete" }
            },
            "required": ["id"]
        })
    }

    async fn execute(&self, input: serde_json::Value, _ctx: &ToolContext) -> Result<ToolOutput> {
        let input: DeleteInput = serde_json::from_value(input)?;

        let mut jobs = self.store.lock().unwrap_or_else(|e| e.into_inner());
        let before = jobs.len();
        jobs.retain(|j| j.id != input.id);

        if jobs.len() == before {
            return Ok(ToolOutput::error(format!(
                "No cron job found with id: {}",
                input.id
            )));
        }

        save_jobs(&jobs)?;
        Ok(ToolOutput::success(format!(
            "Cron job {} deleted.",
            input.id
        )))
    }
}

// ── CronList ──────────────────────────────────────────────────────────────────

pub struct CronListTool {
    pub store: CronStore,
}

#[async_trait]
impl Tool for CronListTool {
    fn name(&self) -> &str {
        "CronList"
    }

    fn description(&self) -> &str {
        "List the recorded cron jobs (ids, schedules, prompts). Note: RustyClaw does not run them itself yet."
    }

    fn input_schema(&self) -> serde_json::Value {
        json!({ "type": "object", "properties": {} })
    }

    async fn execute(&self, _input: serde_json::Value, _ctx: &ToolContext) -> Result<ToolOutput> {
        let jobs = self.store.lock().unwrap_or_else(|e| e.into_inner()).clone();

        if jobs.is_empty() {
            return Ok(ToolOutput::success("No cron jobs scheduled."));
        }

        let lines: Vec<String> = jobs
            .iter()
            .map(|j| {
                let status = if j.enabled { "enabled" } else { "disabled" };
                let desc = if j.description.is_empty() {
                    String::new()
                } else {
                    format!(" — {}", j.description)
                };
                format!(
                    "[{status}] {id} | {schedule}{desc}\n  prompt: {prompt}",
                    id = prefix_chars(&j.id, 8),
                    schedule = j.schedule,
                    prompt = prefix_chars(&j.prompt, 80),
                )
            })
            .collect();

        Ok(ToolOutput::success(lines.join("\n\n")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store_with(prompt: &str) -> CronStore {
        Arc::new(Mutex::new(vec![CronJob {
            id: "0123456789abcdef".into(),
            schedule: "* * * * *".into(),
            prompt: prompt.into(),
            description: String::new(),
            created_at: 0,
            last_run: None,
            enabled: true,
        }]))
    }

    /// The listing trimmed the prompt with a byte slice at 80; a multi-byte
    /// character straddling that boundary panicked the whole tool call.
    #[tokio::test]
    async fn listing_a_prompt_with_multibyte_text_at_the_cut_does_not_panic() {
        // 79 ASCII bytes then a 4-byte emoji: byte 80 is inside the emoji.
        let prompt = format!("{}🦀 and more text after", "a".repeat(79));
        let tool = CronListTool {
            store: store_with(&prompt),
        };
        let out = tool
            .execute(json!({}), &ToolContext::new(std::env::temp_dir()))
            .await
            .unwrap();
        assert!(!out.is_error);
    }

    #[test]
    fn cron_validation_rejects_bad_steps_and_ranges() {
        assert!(validate_cron("*/0 * * * *").is_err());
        assert!(
            validate_cron("*/61 * * * *").is_err(),
            "step beyond the field range"
        );
        assert!(validate_cron("5- * * * *").is_err());
        assert!(validate_cron("0 9 * * 1").is_ok());
        assert!(validate_cron("*/15 * * * *").is_ok());
    }
}
