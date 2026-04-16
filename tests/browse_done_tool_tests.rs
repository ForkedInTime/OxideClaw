use rustyclaw::api::types::ToolResultContent;
use rustyclaw::tools::{browser_tools::BrowseDoneTool, Tool, ToolContext};
use serde_json::json;

fn extract_text(output: &rustyclaw::tools::ToolOutput) -> String {
    output
        .content
        .iter()
        .map(|c| {
            let ToolResultContent::Text { text } = c;
            text.as_str()
        })
        .collect::<Vec<_>>()
        .join("")
}

#[tokio::test]
async fn browse_done_records_summary_and_achieved() {
    let tool = BrowseDoneTool::new();
    let ctx = ToolContext::new(std::env::current_dir().unwrap());
    let input = json!({ "summary": "Found flight", "achieved": true });
    let result = tool.execute(input, &ctx).await.unwrap();
    let text = extract_text(&result);
    assert!(text.contains("BROWSE_DONE"), "expected BROWSE_DONE sentinel in: {text}");
    assert!(text.contains("achieved=true"), "expected achieved=true in: {text}");
    assert!(text.contains("Found flight"), "expected summary in: {text}");
}

#[tokio::test]
async fn browse_done_handles_not_achieved() {
    let tool = BrowseDoneTool::new();
    let ctx = ToolContext::new(std::env::current_dir().unwrap());
    let input = json!({ "summary": "Stuck", "achieved": false });
    let result = tool.execute(input, &ctx).await.unwrap();
    let text = extract_text(&result);
    assert!(text.contains("achieved=false"), "expected achieved=false in: {text}");
}
