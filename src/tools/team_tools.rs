/// TeamCreateTool and TeamDeleteTool — agent swarm team lifecycle management.
///
/// Enabled when OXIDECLAW_EXPERIMENTAL_AGENT_TEAMS=1 (same gate as SendMessageTool).
///
/// Teams are stored in <config dir>/teams/<name>.json
use crate::tools::{Tool, ToolContext, ToolOutput};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::{Value, json};

pub struct TeamCreateTool;
pub struct TeamDeleteTool;

#[async_trait]
impl Tool for TeamCreateTool {
    fn name(&self) -> &str {
        "TeamCreate"
    }

    fn description(&self) -> &str {
        "Create a named agent team (teams/<name>.json under the OxideClaw config dir). \
         Experimental: this only records the team; it does not start any agents, \
         and SendMessage only writes file mailboxes that no OxideClaw agent reads. \
         Requires OXIDECLAW_EXPERIMENTAL_AGENT_TEAMS=1."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["name"],
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Team name (alphanumeric, hyphens allowed)"
                },
                "members": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "name": {
                                "type": "string",
                                "description": "Teammate name ([A-Za-z0-9_-])"
                            },
                            "role": { "type": "string" }
                        },
                        "required": ["name"]
                    },
                    "description": "Initial team members"
                },
                "description": {
                    "type": "string",
                    "description": "Optional description of the team's purpose"
                }
            }
        })
    }

    async fn execute(&self, input: Value, _ctx: &ToolContext) -> Result<ToolOutput> {
        if !crate::tools::send_message::is_agent_swarms_enabled() {
            return Ok(ToolOutput::error(
                "Agent swarms are not enabled. Set OXIDECLAW_EXPERIMENTAL_AGENT_TEAMS=1.",
            ));
        }

        let name = match input["name"].as_str() {
            Some(s) if !s.is_empty() => s,
            _ => return Ok(ToolOutput::error("team name must not be empty")),
        };

        if !crate::tools::send_message::valid_team_ident(name) {
            return Ok(ToolOutput::error(
                "team name must contain only ASCII alphanumerics, hyphens, or underscores",
            ));
        }

        let teams_dir = crate::config::Config::config_dir().join("teams");
        std::fs::create_dir_all(&teams_dir)?;

        let team_file = teams_dir.join(format!("{}.json", name));
        if team_file.exists() {
            return Ok(ToolOutput::error(format!("Team '{}' already exists", name)));
        }

        let members: Vec<Value> = input["members"].as_array().cloned().unwrap_or_default();
        if let Some(err) = invalid_member(&members) {
            return Ok(ToolOutput::error(err));
        }

        let description = input["description"].as_str().unwrap_or("");

        let team = json!({
            "name": name,
            "description": description,
            "members": members,
            "createdAt": {
                "secs_since_epoch": std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0)
            }
        });

        std::fs::write(&team_file, serde_json::to_string_pretty(&team)?)?;

        let result = json!({
            "success": true,
            "message": format!("Team '{}' created with {} member(s)", name, members.len()),
            "name": name,
            "members": members
        });
        Ok(ToolOutput::success(result.to_string()))
    }
}

/// SendMessage broadcasts use each member name as a mailbox path component,
/// so a name that `to` would refuse must not reach the team file either.
fn invalid_member(members: &[Value]) -> Option<String> {
    members.iter().find_map(|m| match m["name"].as_str() {
        Some(n) if crate::tools::send_message::valid_team_ident(n) => None,
        Some(n) => Some(format!(
            "member name {n:?} must contain only ASCII alphanumerics, hyphens, or underscores"
        )),
        None => Some("every member needs a string \"name\"".to_string()),
    })
}

#[async_trait]
impl Tool for TeamDeleteTool {
    fn name(&self) -> &str {
        "TeamDelete"
    }

    fn description(&self) -> &str {
        "Delete a named agent team. \
         Requires OXIDECLAW_EXPERIMENTAL_AGENT_TEAMS=1."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["name"],
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Name of the team to delete"
                }
            }
        })
    }

    async fn execute(&self, input: Value, _ctx: &ToolContext) -> Result<ToolOutput> {
        if !crate::tools::send_message::is_agent_swarms_enabled() {
            return Ok(ToolOutput::error(
                "Agent swarms are not enabled. Set OXIDECLAW_EXPERIMENTAL_AGENT_TEAMS=1.",
            ));
        }

        let name = match input["name"].as_str() {
            Some(s) if !s.is_empty() => s,
            _ => return Ok(ToolOutput::error("team name must not be empty")),
        };
        // Same rule as TeamCreate. Without it `../../.claude/settings` deleted
        // an arbitrary .json under home and `remove_dir_all`'d a directory.
        if !crate::tools::send_message::valid_team_ident(name) {
            return Ok(ToolOutput::error(
                "team name must contain only ASCII alphanumerics, hyphens, or underscores",
            ));
        }

        let team_file = crate::config::Config::config_dir()
            .join("teams")
            .join(format!("{}.json", name));

        if !team_file.exists() {
            return Ok(ToolOutput::error(format!("Team '{}' does not exist", name)));
        }

        std::fs::remove_file(&team_file)?;

        // Also clean up mailboxes for this team
        let mailbox_dir = crate::config::Config::config_dir()
            .join("mailboxes")
            .join(name);
        if mailbox_dir.exists() {
            let _ = std::fs::remove_dir_all(&mailbox_dir);
        }

        let result = json!({
            "success": true,
            "message": format!("Team '{}' deleted", name)
        });
        Ok(ToolOutput::success(result.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn member_names_must_be_mailbox_idents() {
        assert_eq!(
            invalid_member(&[
                json!({"name": "alice", "role": "dev"}),
                json!({"name": "b-2"})
            ]),
            None
        );
        for bad in [
            json!({"name": "/tmp/evil"}),
            json!({"name": "../.."}),
            json!({"name": "code reviewer"}),
            json!({"role": "nameless"}),
            json!({"name": 7}),
        ] {
            assert!(
                invalid_member(&[json!({"name": "ok"}), bad.clone()]).is_some(),
                "{bad} must be refused"
            );
        }
    }
}
