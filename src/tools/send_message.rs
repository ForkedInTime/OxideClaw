/// SendMessageTool — inter-agent messaging for swarm mode.
///
/// Enabled when RUSTYCLAW_EXPERIMENTAL_AGENT_TEAMS=1.
///
/// The TS predecessor integrates with a full mailbox/team-file
/// infrastructure and in-process routing. This implementation uses
/// the file-based mailbox protocol (same format) so agents running
/// in separate processes can exchange messages.
///
/// Mailbox layout: ~/.claude/mailboxes/<team>/<recipient>/messages/<uuid>.json
use crate::tools::{Tool, ToolContext, ToolOutput};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::{Value, json};

pub struct SendMessageTool;

/// A team or teammate name is used as a path component under
/// `~/.claude/{teams,mailboxes}`. Anything but `[A-Za-z0-9_-]` (or the
/// broadcast `*`, where allowed) is refused so a model-supplied
/// `../../.claude/settings` cannot reach outside the mailbox tree.
pub fn valid_team_ident(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Check whether agent swarms are enabled.
pub fn is_agent_swarms_enabled() -> bool {
    std::env::var("RUSTYCLAW_EXPERIMENTAL_AGENT_TEAMS")
        .map(|v| matches!(v.as_str(), "1" | "true" | "yes"))
        .unwrap_or(false)
}

#[async_trait]
impl Tool for SendMessageTool {
    fn name(&self) -> &str {
        "SendMessage"
    }

    fn description(&self) -> &str {
        "Send a message to an agent teammate (swarm protocol). \
         Enabled when RUSTYCLAW_EXPERIMENTAL_AGENT_TEAMS=1. \
         Use to coordinate with other Claude agents in a team. \
         The 'to' field is a teammate name or '*' to broadcast."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["to", "message"],
            "properties": {
                "to": {
                    "type": "string",
                    "description": "Recipient: teammate name, or \"*\" for broadcast to all teammates"
                },
                "summary": {
                    "type": "string",
                    "description": "A 5-10 word summary shown as a preview in the UI (required when message is a string)"
                },
                "message": {
                    "oneOf": [
                        {
                            "type": "string",
                            "description": "Plain text message content"
                        },
                        {
                            "type": "object",
                            "description": "Structured message (shutdown_request, shutdown_response, plan_approval_response)",
                            "properties": {
                                "type": {
                                    "type": "string",
                                    "enum": ["shutdown_request", "shutdown_response", "plan_approval_response"]
                                }
                            },
                            "required": ["type"]
                        }
                    ]
                }
            }
        })
    }

    async fn execute(&self, input: Value, _ctx: &ToolContext) -> Result<ToolOutput> {
        if !is_agent_swarms_enabled() {
            return Ok(ToolOutput::error(
                "Agent swarms are not enabled. Set RUSTYCLAW_EXPERIMENTAL_AGENT_TEAMS=1 to use this tool.",
            ));
        }

        let to = match input["to"].as_str() {
            Some(s) if !s.is_empty() => s,
            _ => return Ok(ToolOutput::error("'to' must not be empty")),
        };

        if to != "*" && !valid_team_ident(to) {
            return Ok(ToolOutput::error(
                "to must be a bare teammate name ([A-Za-z0-9_-]) or \"*\" — it becomes a \
                 mailbox path component",
            ));
        }

        let message = &input["message"];
        let summary = input["summary"].as_str();
        let team_name = std::env::var("RUSTYCLAW_TEAM_NAME").unwrap_or_else(|_| "default".into());
        if !valid_team_ident(&team_name) {
            return Ok(ToolOutput::error(
                "RUSTYCLAW_TEAM_NAME must match [A-Za-z0-9_-] — it becomes a mailbox path component",
            ));
        }
        let sender_name =
            std::env::var("RUSTYCLAW_AGENT_NAME").unwrap_or_else(|_| "team-lead".into());
        let timestamp = {
            let secs = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            // Simple ISO-like UTC timestamp (seconds precision)
            let s = secs;
            let sec = s % 60;
            let min = (s / 60) % 60;
            let hour = (s / 3600) % 24;
            let days = s / 86400;
            // Days since 1970-01-01 to year/month/day (simplified Gregorian)
            let (year, month, day) = days_to_ymd(days);
            format!("{year:04}-{month:02}-{day:02}T{hour:02}:{min:02}:{sec:02}Z")
        };

        // Validate string messages need a summary
        if message.is_string() && to != "*" && summary.map(|s| s.trim().is_empty()).unwrap_or(true)
        {
            return Ok(ToolOutput::error(
                "summary is required when message is a string",
            ));
        }

        // Broadcast: send to all members of the team file
        if to == "*" {
            if !message.is_string() {
                return Ok(ToolOutput::error(
                    "structured messages cannot be broadcast (to: \"*\")",
                ));
            }
            let content = message.as_str().unwrap_or("");
            let team_file_path = dirs::home_dir().map(|h| {
                h.join(".claude")
                    .join("teams")
                    .join(format!("{}.json", team_name))
            });

            let members = if let Some(path) = team_file_path.as_ref().filter(|p| p.exists()) {
                let data = std::fs::read_to_string(path).unwrap_or_default();
                let json: Value = serde_json::from_str(&data).unwrap_or_default();
                json["members"]
                    .as_array()
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|m| m["name"].as_str())
                            .filter(|name| *name != sender_name.as_str())
                            .map(|s| s.to_string())
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default()
            } else {
                vec![]
            };

            if members.is_empty() {
                return Ok(ToolOutput::success(
                    "{\"success\":true,\"message\":\"No teammates to broadcast to\",\"recipients\":[]}",
                ));
            }

            for recipient in &members {
                write_mailbox_message(
                    &team_name,
                    recipient,
                    &sender_name,
                    content,
                    summary,
                    &timestamp,
                )?;
            }

            let result = json!({
                "success": true,
                "message": format!("Message broadcast to {} teammate(s): {}", members.len(), members.join(", ")),
                "recipients": members
            });
            return Ok(ToolOutput::success(result.to_string()));
        }

        // Structured message routing
        if let Some(msg_obj) = message.as_object() {
            let msg_type = msg_obj.get("type").and_then(|v| v.as_str()).unwrap_or("");
            match msg_type {
                "shutdown_request" => {
                    let reason = msg_obj.get("reason").and_then(|v| v.as_str()).unwrap_or("");
                    let request_id = format!("shutdown-{}-{}", to, &timestamp[..10]);
                    let payload = json!({
                        "type": "shutdown_request",
                        "requestId": request_id,
                        "from": sender_name,
                        "reason": reason,
                        "timestamp": timestamp
                    });
                    write_mailbox_message(
                        &team_name,
                        to,
                        &sender_name,
                        &payload.to_string(),
                        None,
                        &timestamp,
                    )?;
                    let result = json!({
                        "success": true,
                        "message": format!("Shutdown request sent to {}. Request ID: {}", to, request_id),
                        "request_id": request_id,
                        "target": to
                    });
                    return Ok(ToolOutput::success(result.to_string()));
                }
                "shutdown_response" => {
                    let request_id = msg_obj
                        .get("request_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let approve = msg_obj
                        .get("approve")
                        .map(|v| v.as_bool().unwrap_or(false))
                        .unwrap_or(false);
                    let reason = msg_obj.get("reason").and_then(|v| v.as_str()).unwrap_or("");
                    if to != "team-lead" {
                        return Ok(ToolOutput::error(
                            "shutdown_response must be sent to \"team-lead\"",
                        ));
                    }
                    if !approve && reason.trim().is_empty() {
                        return Ok(ToolOutput::error(
                            "reason is required when rejecting a shutdown request",
                        ));
                    }
                    let resp_type = if approve {
                        "shutdown_approved"
                    } else {
                        "shutdown_rejected"
                    };
                    let payload = json!({
                        "type": resp_type,
                        "requestId": request_id,
                        "from": sender_name,
                        "reason": reason,
                        "timestamp": timestamp
                    });
                    write_mailbox_message(
                        &team_name,
                        to,
                        &sender_name,
                        &payload.to_string(),
                        None,
                        &timestamp,
                    )?;
                    let msg = if approve {
                        format!(
                            "Shutdown approved. Sent confirmation to team-lead. Agent {} is now exiting.",
                            sender_name
                        )
                    } else {
                        format!(
                            "Shutdown rejected. Reason: \"{}\". Continuing to work.",
                            reason
                        )
                    };
                    let result =
                        json!({ "success": true, "message": msg, "request_id": request_id });
                    return Ok(ToolOutput::success(result.to_string()));
                }
                "plan_approval_response" => {
                    let request_id = msg_obj
                        .get("request_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let approve = msg_obj
                        .get("approve")
                        .map(|v| v.as_bool().unwrap_or(false))
                        .unwrap_or(false);
                    let feedback = msg_obj
                        .get("feedback")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let payload = json!({
                        "type": "plan_approval_response",
                        "requestId": request_id,
                        "approved": approve,
                        "feedback": feedback,
                        "timestamp": timestamp
                    });
                    write_mailbox_message(
                        &team_name,
                        to,
                        &sender_name,
                        &payload.to_string(),
                        None,
                        &timestamp,
                    )?;
                    let msg = if approve {
                        format!("Plan approved for {}.", to)
                    } else {
                        format!("Plan rejected for {} with feedback: \"{}\"", to, feedback)
                    };
                    let result =
                        json!({ "success": true, "message": msg, "request_id": request_id });
                    return Ok(ToolOutput::success(result.to_string()));
                }
                _ => {
                    return Ok(ToolOutput::error(format!(
                        "Unknown structured message type: {}",
                        msg_type
                    )));
                }
            }
        }

        // Plain text message to a named recipient
        let content = message.as_str().unwrap_or("");
        write_mailbox_message(&team_name, to, &sender_name, content, summary, &timestamp)?;

        let result = json!({
            "success": true,
            "message": format!("Message sent to {}'s inbox", to),
            "routing": {
                "sender": sender_name,
                "target": format!("@{}", to),
                "summary": summary,
                "content": content
            }
        });
        Ok(ToolOutput::success(result.to_string()))
    }
}

fn write_mailbox_message(
    team: &str,
    recipient: &str,
    sender: &str,
    text: &str,
    summary: Option<&str>,
    timestamp: &str,
) -> Result<()> {
    let mailbox_dir = dirs::home_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join(".claude")
        .join("mailboxes")
        .join(team)
        .join(recipient)
        .join("messages");

    std::fs::create_dir_all(&mailbox_dir)?;

    let id = uuid::Uuid::new_v4().to_string();
    let path = mailbox_dir.join(format!("{}.json", id));

    let msg = json!({
        "id": id,
        "from": sender,
        "text": text,
        "summary": summary,
        "timestamp": timestamp
    });

    std::fs::write(path, serde_json::to_string_pretty(&msg)?)?;
    Ok(())
}

/// Convert days since Unix epoch (1970-01-01) to (year, month, day).
fn days_to_ymd(days: u64) -> (u64, u64, u64) {
    // Proleptic Gregorian algorithm (accurate for 1970+)
    let z = days + 719468;
    let era = z / 146097;
    let doe = z % 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

#[cfg(test)]
mod ident_tests {
    use super::valid_team_ident;

    #[test]
    fn plain_names_pass() {
        for n in ["alice", "team-lead", "worker_2", "A1"] {
            assert!(valid_team_ident(n), "{n}");
        }
    }

    /// Every one of these was accepted before and became a path component.
    #[test]
    fn traversal_and_separators_are_refused() {
        for n in [
            "",
            "..",
            "../x",
            "../../.claude/settings",
            "a/b",
            "a\\b",
            ".",
            "name with space",
            "*",
        ] {
            assert!(!valid_team_ident(n), "{n:?} must be refused");
        }
    }
}
