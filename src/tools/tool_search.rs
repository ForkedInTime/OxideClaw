/// ToolSearchTool — port of toolSearch.ts (deferred tool schema fetching)
/// Searches available tool names and descriptions by keyword query.
use super::{Tool, ToolContext, ToolOutput, async_trait};
use anyhow::Result;
use serde::Deserialize;
use serde_json::json;

pub struct ToolSearchTool {
    /// Snapshot of all tool definitions (name + description) at construction time.
    pub tools_snapshot: Vec<(String, String)>,
}

#[derive(Deserialize)]
struct Input {
    query: String,
    #[serde(default = "default_max")]
    max_results: usize,
}

fn default_max() -> usize {
    5
}

#[async_trait]
impl Tool for ToolSearchTool {
    fn name(&self) -> &str {
        "ToolSearch"
    }

    fn description(&self) -> &str {
        "Search available tools by keyword. Returns matching tool names and descriptions. \
        Use this to discover which tools are available before calling ToolSearch itself."
    }

    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Keyword(s) to search for in tool names and descriptions"
                },
                "max_results": {
                    "type": "integer",
                    "description": "Maximum number of results to return (default 5)",
                    "default": 5,
                    "minimum": 1,
                    "maximum": 20
                }
            },
            "required": ["query"]
        })
    }

    async fn execute(&self, input: serde_json::Value, _ctx: &ToolContext) -> Result<ToolOutput> {
        let input: Input = serde_json::from_value(input)?;
        let query_lower = input.query.to_lowercase();
        let terms: Vec<&str> = query_lower.split_whitespace().collect();

        let mut scored: Vec<(usize, &str, &str)> = self
            .tools_snapshot
            .iter()
            .filter_map(|(name, desc)| {
                let combined = format!("{} {}", name.to_lowercase(), desc.to_lowercase());
                let score: usize = terms
                    .iter()
                    .map(|t| if combined.contains(t) { 1 } else { 0 })
                    .sum();
                if score > 0 {
                    Some((score, name.as_str(), desc.as_str()))
                } else {
                    None
                }
            })
            .collect();

        scored.sort_by_key(|e| std::cmp::Reverse(e.0));
        scored.truncate(input.max_results);

        if scored.is_empty() {
            return Ok(ToolOutput::success(format!(
                "No tools matched '{}'.",
                input.query
            )));
        }

        let lines: Vec<String> = scored
            .iter()
            .map(|(_, name, desc)| format!("{name}: {desc}"))
            .collect();

        Ok(ToolOutput::success(lines.join("\n")))
    }
}
