//! The one implementation of "may this tool call run?".
//!
//! Until 2026-09 the decision lived inline in the TUI's tool loop, and
//! `QueryEngine` — which runs every sub-agent (`Agent` tool), spawned
//! worktree agent, and `-p` print-mode session — had no check at all. A
//! model could call `Agent { prompt: "run <anything>" }` and the child ran
//! Bash unprompted. The gate is what every executor now goes through; the
//! TUI plugs in a [`PermissionAsker`] that shows the prompt, a headless
//! engine has no asker and therefore **fails closed** on anything that would
//! have needed one.

use super::{
    CheckResult, PermissionDecision, PermissionState, blocked_entry_matches,
    check_compound_command, describe_tool_call, is_command_tool,
};
use std::sync::Arc;

/// Something that can put a permission prompt in front of a human.
#[async_trait::async_trait]
pub trait PermissionAsker: Send + Sync {
    /// `description` is the `describe_tool_call` rendering of `input`.
    /// Return `None` when no answer could be obtained (UI gone, channel
    /// dropped) — the gate treats that as Deny.
    async fn ask(
        &self,
        tool_name: &str,
        description: &str,
        input: &serde_json::Value,
    ) -> Option<PermissionDecision>;
}

/// Shell tools refused for the rest of a turn that invoked a skill while
/// `disableSkillShellExecution` is set. `Agent` needs no entry: children
/// inherit this gate, block list included.
pub const SKILL_SHELL_BLOCKED_TOOLS: &[&str] = &["Bash", "PowerShell"];

const PLAN_MODE_REASON: &str = "in plan mode. Use ExitPlanMode when the plan is approved.";
const SKILL_SHELL_REASON: &str =
    "during skill invocations (disableSkillShellExecution is set). Do not retry it this turn.";

#[derive(Debug, PartialEq, Eq)]
pub enum GateOutcome {
    Allowed,
    /// The text handed back to the model as the tool result.
    Denied(String),
}

#[derive(Clone)]
pub struct PermissionGate {
    state: PermissionState,
    /// `autonomy: "suggest"` — Write/Edit always prompt, even if a rule
    /// would allow them.
    suggest_mode: bool,
    asker: Option<Arc<dyn PermissionAsker>>,
    /// Tools refused outright for this turn (plan mode, skill turns), each
    /// with the reason the model is told. Inherited by sub-agents through
    /// the gate, so a child launched in plan mode cannot write either.
    blocked: Vec<(String, &'static str)>,
    /// Consult the asker for every tool that passes the deny list, not only
    /// the sensitive ones: an SDK/ACP host policy covers Read and WebFetch
    /// too, and an `Agent` child must not get around it.
    ask_every_tool: bool,
}

impl PermissionGate {
    pub fn new(
        state: PermissionState,
        suggest_mode: bool,
        asker: Option<Arc<dyn PermissionAsker>>,
    ) -> Self {
        Self {
            state,
            suggest_mode,
            asker,
            blocked: Vec::new(),
            ask_every_tool: false,
        }
    }

    /// See [`PermissionState::read_deny`].
    pub fn read_deny(&self, tool_name: &str) -> super::ReadDeny {
        self.state.read_deny(tool_name)
    }

    /// Route every call the deny list lets through to the asker.
    pub fn with_asker_for_all_tools(mut self) -> Self {
        self.ask_every_tool = true;
        self
    }

    /// Refuse these tools for the life of this gate (plan mode).
    pub fn with_blocked_tools(self, tools: &[&str]) -> Self {
        self.block(tools, PLAN_MODE_REASON)
    }

    /// Refuse shell tools for the life of this gate: a skill ran while
    /// `disableSkillShellExecution` is set.
    pub fn with_skill_shell_blocked(self) -> Self {
        self.block(SKILL_SHELL_BLOCKED_TOOLS, SKILL_SHELL_REASON)
    }

    /// Adds to the block list rather than replacing it, so plan mode and a
    /// skill turn can both be in force.
    fn block(mut self, tools: &[&str], reason: &'static str) -> Self {
        for t in tools {
            if !self.blocked.iter().any(|(b, _)| b == t) {
                self.blocked.push((t.to_string(), reason));
            }
        }
        self
    }

    /// A gate for an engine with no human attached (`-p`, SDK-less
    /// headless use). Settings/CLI allow and deny rules still apply;
    /// anything that would need a prompt is refused.
    pub fn headless(cfg: &crate::config::Config) -> Self {
        Self::new(
            PermissionState::new(
                cfg.dangerously_skip_permissions,
                &cfg.permissions_allow,
                &cfg.permissions_deny,
            )
            .with_cwd(&cfg.cwd),
            false,
            None,
        )
    }

    /// Allow everything. Only for executors the user has explicitly asked
    /// to run autonomously (`/spawn`).
    #[cfg(test)]
    pub fn bypass() -> Self {
        Self::bypass_with_deny(&[], std::path::Path::new("/"))
    }

    /// No prompts, but `permissions.deny` still holds: the user's written
    /// rules are not something "autonomous" waives.
    pub fn bypass_with_deny(deny: &[String], cwd: &std::path::Path) -> Self {
        Self::new(
            PermissionState::new(true, &[], deny).with_cwd(cwd),
            false,
            None,
        )
    }

    pub async fn decide(&self, tool_name: &str, input: &serde_json::Value) -> GateOutcome {
        if let Some((_, reason)) = self
            .blocked
            .iter()
            .find(|(b, _)| blocked_entry_matches(b, tool_name))
        {
            return GateOutcome::Denied(format!("{tool_name} is blocked {reason}"));
        }
        let check = if is_command_tool(tool_name) {
            // Compound commands are split so a prefix rule cannot authorise
            // whatever is chained after the first statement.
            match input.get("command").and_then(|c| c.as_str()) {
                Some(cmd) => check_compound_command(&self.state, tool_name, cmd),
                None => self.state.check_with_input(tool_name, Some(input)),
            }
        } else {
            self.state.check_with_input(tool_name, Some(input))
        };
        // Suggest mode only turns an Allow into a prompt: a deny rule must
        // still refuse outright instead of becoming one more routine approval.
        let check = match check {
            CheckResult::Allow
                if self.ask_every_tool
                    || (self.suggest_mode
                        && matches!(tool_name, "Write" | "Edit" | "MultiEdit")) =>
            {
                CheckResult::Ask
            }
            c => c,
        };

        match check {
            CheckResult::Allow => GateOutcome::Allowed,
            CheckResult::Deny => GateOutcome::Denied(format!("Permission denied: {tool_name}")),
            CheckResult::Ask => match &self.asker {
                // Only name remedies that grant permission: --allowed-tools
                // filters the tool list but never authorises a call.
                None => GateOutcome::Denied(format!(
                    "Permission denied: {tool_name} requires approval and no interactive \
                     session is attached. Allow it with permissions.allow in settings.json \
                     or --dangerously-skip-permissions."
                )),
                Some(asker) => {
                    let description = describe_tool_call(tool_name, input);
                    match asker.ask(tool_name, &description, input).await {
                        Some(PermissionDecision::Allow) => GateOutcome::Allowed,
                        Some(PermissionDecision::AlwaysAllow) => {
                            self.state.record_always_allow(tool_name);
                            GateOutcome::Allowed
                        }
                        // A dropped or failed prompt is a Deny, never an Allow
                        // ("close terminal = auto-approve" class).
                        Some(PermissionDecision::Deny) | None => {
                            GateOutcome::Denied(format!("Permission denied: {tool_name}"))
                        }
                    }
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    /// Replies from a script; records every question it was asked.
    struct Scripted {
        replies: Mutex<VecDeque<Option<PermissionDecision>>>,
        asked: Mutex<Vec<(String, String)>>,
    }

    impl Scripted {
        fn new(replies: Vec<Option<PermissionDecision>>) -> Arc<Self> {
            Arc::new(Self {
                replies: Mutex::new(replies.into()),
                asked: Mutex::new(Vec::new()),
            })
        }
        fn asked(&self) -> Vec<(String, String)> {
            self.asked.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl PermissionAsker for Scripted {
        async fn ask(
            &self,
            tool_name: &str,
            description: &str,
            _input: &serde_json::Value,
        ) -> Option<PermissionDecision> {
            self.asked
                .lock()
                .unwrap()
                .push((tool_name.to_string(), description.to_string()));
            self.replies.lock().unwrap().pop_front().flatten()
        }
    }

    fn gate(allow: &[&str], asker: Option<Arc<Scripted>>) -> PermissionGate {
        let allow: Vec<String> = allow.iter().map(|s| s.to_string()).collect();
        PermissionGate::new(
            PermissionState::new(false, &allow, &[]),
            false,
            asker.map(|a| a as Arc<dyn PermissionAsker>),
        )
    }

    #[tokio::test]
    async fn non_sensitive_tool_is_allowed_without_asking() {
        let asker = Scripted::new(vec![]);
        let g = gate(&[], Some(asker.clone()));
        assert_eq!(
            g.decide("Read", &json!({"file_path": "x"})).await,
            GateOutcome::Allowed
        );
        assert!(asker.asked().is_empty());
    }

    #[tokio::test]
    async fn sensitive_tool_without_a_rule_asks_and_honours_deny() {
        let asker = Scripted::new(vec![Some(PermissionDecision::Deny)]);
        let g = gate(&[], Some(asker.clone()));
        let out = g.decide("Bash", &json!({"command": "ls -la"})).await;
        assert!(
            matches!(out, GateOutcome::Denied(ref m) if m.contains("Permission denied")),
            "{out:?}"
        );
        let asked = asker.asked();
        assert_eq!(asked.len(), 1);
        assert_eq!(asked[0].0, "Bash");
        assert!(asked[0].1.contains("ls -la"), "{}", asked[0].1);
    }

    #[tokio::test]
    async fn always_allow_is_remembered_for_the_session() {
        let asker = Scripted::new(vec![Some(PermissionDecision::AlwaysAllow)]);
        let g = gate(&[], Some(asker.clone()));
        assert_eq!(
            g.decide("Write", &json!({"file_path": "a"})).await,
            GateOutcome::Allowed
        );
        assert_eq!(
            g.decide("Write", &json!({"file_path": "b"})).await,
            GateOutcome::Allowed
        );
        assert_eq!(asker.asked().len(), 1, "second call must not prompt again");
    }

    /// The security contract from the TUI: a prompt that never gets an
    /// answer (terminal closed, runtime torn down) is a Deny, never an Allow.
    #[tokio::test]
    async fn an_unanswered_prompt_is_a_deny() {
        let asker = Scripted::new(vec![None]);
        let g = gate(&[], Some(asker));
        assert!(matches!(
            g.decide("Bash", &json!({"command": "id"})).await,
            GateOutcome::Denied(_)
        ));
    }

    #[tokio::test]
    async fn headless_gate_fails_closed_when_a_prompt_would_be_needed() {
        let g = gate(&[], None);
        let out = g.decide("Bash", &json!({"command": "id"})).await;
        assert!(
            matches!(out, GateOutcome::Denied(ref m) if m.contains("no interactive session")),
            "{out:?}"
        );
        // The hint must not send users to a flag that does not exist
        // (`--allowedTools`) or one that grants nothing (`--allowed-tools`).
        let GateOutcome::Denied(msg) = out else {
            unreachable!()
        };
        assert!(msg.contains("permissions.allow"), "{msg}");
        assert!(msg.contains("--dangerously-skip-permissions"), "{msg}");
        assert!(!msg.to_lowercase().contains("allowedtools"), "{msg}");
        assert!(!msg.contains("--allowed-tools"), "{msg}");
        assert_eq!(
            g.decide("Read", &json!({"file_path": "x"})).await,
            GateOutcome::Allowed
        );
    }

    #[tokio::test]
    async fn headless_gate_still_honours_allow_rules() {
        let g = gate(&["Bash(git:*)"], None);
        assert_eq!(
            g.decide("Bash", &json!({"command": "git status"})).await,
            GateOutcome::Allowed
        );
    }

    /// Phase 1's lesson: a prefix rule must be applied per segment, or
    /// `git status && rm -rf /` rides in on the `git` rule.
    #[tokio::test]
    async fn chained_commands_are_checked_per_segment() {
        let asker = Scripted::new(vec![Some(PermissionDecision::Deny)]);
        let g = gate(&["Bash(git:*)"], Some(asker.clone()));
        assert_eq!(
            g.decide("Bash", &json!({"command": "git status"})).await,
            GateOutcome::Allowed
        );
        assert!(asker.asked().is_empty());
        let out = g
            .decide("Bash", &json!({"command": "git status && rm -rf /"}))
            .await;
        assert!(matches!(out, GateOutcome::Denied(_)), "{out:?}");
        assert_eq!(
            asker.asked().len(),
            1,
            "the chained command must have prompted"
        );
    }

    #[tokio::test]
    async fn suggest_mode_prompts_for_edits_even_when_a_rule_allows_them() {
        let asker = Scripted::new(vec![Some(PermissionDecision::Allow)]);
        let allow = vec!["Write".to_string()];
        let g = PermissionGate::new(
            PermissionState::new(false, &allow, &[]),
            true,
            Some(asker.clone() as Arc<dyn PermissionAsker>),
        );
        assert_eq!(
            g.decide("Write", &json!({"file_path": "a"})).await,
            GateOutcome::Allowed
        );
        assert_eq!(asker.asked().len(), 1);
    }

    #[tokio::test]
    async fn suggest_mode_still_refuses_denied_edits_without_prompting() {
        let asker = Scripted::new(vec![Some(PermissionDecision::Allow)]);
        let deny = vec!["Write(./secrets/**)".to_string()];
        let g = PermissionGate::new(
            PermissionState::new(false, &[], &deny).with_cwd(std::path::Path::new("/proj")),
            true,
            Some(asker.clone() as Arc<dyn PermissionAsker>),
        );
        let out = g
            .decide("Write", &json!({"file_path": "/proj/secrets/x"}))
            .await;
        assert!(matches!(out, GateOutcome::Denied(_)), "{out:?}");
        assert!(asker.asked().is_empty(), "a denied write must not prompt");
        assert_eq!(
            g.decide("Write", &json!({"file_path": "/proj/src/x"}))
                .await,
            GateOutcome::Allowed
        );
        assert_eq!(asker.asked().len(), 1, "other writes still prompt");
    }

    #[tokio::test]
    async fn deny_rules_win_over_everything() {
        let deny = vec!["Bash".to_string()];
        let g = PermissionGate::new(PermissionState::new(true, &[], &deny), false, None);
        // Even with bypass on, an explicit deny is still a deny.
        let out = g.decide("Bash", &json!({"command": "id"})).await;
        assert!(matches!(out, GateOutcome::Denied(_)), "{out:?}");
    }

    /// Plan mode was enforced only in the TUI loop; a sub-agent launched
    /// during plan mode inherited the gate but not the block, and wrote.
    #[tokio::test]
    async fn blocked_tools_are_refused_without_a_prompt() {
        let asker = Scripted::new(vec![Some(PermissionDecision::Allow)]);
        let g = gate(&[], Some(asker.clone())).with_blocked_tools(&["Write", "Bash"]);
        let out = g.decide("Write", &json!({"file_path": "a"})).await;
        assert!(
            matches!(out, GateOutcome::Denied(ref m) if m.contains("plan mode")),
            "{out:?}"
        );
        assert!(
            asker.asked().is_empty(),
            "a blocked tool must not even prompt"
        );
        assert_eq!(
            g.decide("Read", &json!({"file_path": "a"})).await,
            GateOutcome::Allowed
        );
    }

    /// disableSkillShellExecution only added a sentence to the prompt; Bash
    /// still ran under an allow rule or skip-permissions. The block must
    /// hold on a bypass gate, stack with plan mode, and say why.
    #[tokio::test]
    async fn skill_shell_block_holds_under_bypass_and_stacks_with_plan_mode() {
        let g = PermissionGate::bypass().with_skill_shell_blocked();
        for tool in ["Bash", "PowerShell"] {
            let out = g.decide(tool, &json!({"command": "id"})).await;
            assert!(
                matches!(out, GateOutcome::Denied(ref m) if m.contains("disableSkillShellExecution")),
                "{out:?}"
            );
        }
        assert_eq!(
            g.decide("Read", &json!({"file_path": "a"})).await,
            GateOutcome::Allowed
        );

        let both = PermissionGate::bypass()
            .with_blocked_tools(&["Write"])
            .with_skill_shell_blocked();
        let out = both.decide("Write", &json!({"file_path": "a"})).await;
        assert!(
            matches!(out, GateOutcome::Denied(ref m) if m.contains("plan mode")),
            "{out:?}"
        );
        assert!(matches!(
            both.decide("Bash", &json!({"command": "id"})).await,
            GateOutcome::Denied(_)
        ));
    }

    /// MCP tools fell through as "not sensitive", so a server's write_file
    /// or start_process ran with no prompt, plan mode included.
    #[tokio::test]
    async fn mcp_tools_prompt_and_honour_server_wide_rules() {
        let write = "mcp__fs__write_file";
        let asker = Scripted::new(vec![Some(PermissionDecision::Deny)]);
        let g = gate(&[], Some(asker.clone()));
        assert!(matches!(
            g.decide(write, &json!({"path": "a"})).await,
            GateOutcome::Denied(_)
        ));
        assert_eq!(asker.asked().len(), 1, "an MCP call must prompt");
        assert!(matches!(
            gate(&[], None).decide(write, &json!({})).await,
            GateOutcome::Denied(_)
        ));

        for rule in ["mcp__fs", "mcp__fs__*", "MCP__*", write] {
            assert_eq!(
                gate(&[rule], None).decide(write, &json!({})).await,
                GateOutcome::Allowed,
                "{rule}"
            );
        }
        for rule in ["mcp__f", "mcp__fsx", "mcp__other__*", "mcp__fs__read"] {
            assert!(
                matches!(
                    gate(&[rule], None).decide(write, &json!({})).await,
                    GateOutcome::Denied(_)
                ),
                "{rule} must not allow {write}"
            );
        }

        let deny = PermissionGate::bypass_with_deny(&["mcp__fs".into()], std::path::Path::new("/"));
        assert!(matches!(
            deny.decide(write, &json!({})).await,
            GateOutcome::Denied(_)
        ));
        assert_eq!(
            deny.decide("mcp__github__get_issue", &json!({})).await,
            GateOutcome::Allowed
        );

        let asker = Scripted::new(vec![Some(PermissionDecision::Allow)]);
        let plan = gate(&["mcp__*"], Some(asker.clone())).with_blocked_tools(&["Write", "mcp__*"]);
        let out = plan.decide(write, &json!({})).await;
        assert!(
            matches!(out, GateOutcome::Denied(ref m) if m.contains("plan mode")),
            "{out:?}"
        );
        assert!(asker.asked().is_empty());
        assert_eq!(
            plan.decide("Read", &json!({"file_path": "a"})).await,
            GateOutcome::Allowed
        );
    }

    #[tokio::test]
    async fn bypass_gate_still_honours_deny_rules() {
        let g = PermissionGate::bypass_with_deny(
            &["Bash(git push:*)".into(), "Bash(git reset --hard)".into()],
            std::path::Path::new("/proj"),
        );
        for cmd in [
            "git push origin main",
            "git push",
            "git push\torigin",
            "git reset --hard",
            "git  reset --hard",
        ] {
            assert!(
                matches!(
                    g.decide("Bash", &json!({ "command": cmd })).await,
                    GateOutcome::Denied(_)
                ),
                "{cmd:?}"
            );
        }
        assert_eq!(
            g.decide("Bash", &json!({"command": "git status"})).await,
            GateOutcome::Allowed
        );
    }

    #[tokio::test]
    async fn bypass_gate_allows_sensitive_tools_silently() {
        let g = PermissionGate::bypass();
        assert_eq!(
            g.decide("Bash", &json!({"command": "id"})).await,
            GateOutcome::Allowed
        );
    }
}
