//! Policy evaluation for tool approval.
//!
//! Evaluation order: deny > ask > auto_approve > allow.
//! Unlisted tools: ask (if interactive_approval) or deny (if not).
//! The user's `/autonomy` mode fills in for unlisted tools and forces a
//! prompt for edits under `suggest`; it never overrides the host's deny or
//! ask lists.

use crate::permissions::{Autonomy, Verdict};
use crate::sdk::protocol::Policy;

/// What should happen when a tool is called.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalDecision {
    /// Execute silently, no notification.
    Allow,
    /// Execute with a tool/started notification.
    AutoApprove,
    /// Send tool/approval_needed, block until host responds.
    Ask,
    /// Deny immediately.
    Deny,
}

/// Evaluates tool calls against the session policy.
pub struct PolicyEngine {
    policy: Policy,
    interactive_approval: bool,
    autonomy: Autonomy,
    project: std::path::PathBuf,
}

impl PolicyEngine {
    pub fn new(policy: Policy, interactive_approval: bool) -> Self {
        Self {
            policy,
            interactive_approval,
            autonomy: Autonomy::Ask,
            project: std::path::PathBuf::new(),
        }
    }

    /// Apply the user's autonomy mode (already resolved for the sandbox)
    /// to calls in `project`.
    pub fn with_autonomy(mut self, autonomy: Autonomy, project: &std::path::Path) -> Self {
        self.autonomy = autonomy;
        self.project = project.to_path_buf();
        self
    }

    /// Evaluate a call to `tool_name` with `input` against the policy.
    pub fn evaluate(&self, tool_name: &str, input: &serde_json::Value) -> ApprovalDecision {
        self.evaluate_in(tool_name, input, &self.project)
    }

    /// [`Self::evaluate`] for a call whose relative paths resolve against
    /// `work_cwd` (an entered worktree), where the mode judges it.
    pub fn evaluate_in(
        &self,
        tool_name: &str,
        input: &serde_json::Value,
        work_cwd: &std::path::Path,
    ) -> ApprovalDecision {
        let listed = |list: &[String]| list.iter().any(|t| t == tool_name);
        // Deny takes highest priority, then the host's explicit ask.
        if listed(&self.policy.deny) {
            return ApprovalDecision::Deny;
        }
        let ask = if self.interactive_approval {
            ApprovalDecision::Ask
        } else {
            ApprovalDecision::Deny
        };
        if listed(&self.policy.ask) {
            return ApprovalDecision::Ask;
        }
        let verdict = self.autonomy.verdict(tool_name, input, work_cwd);
        if verdict == Verdict::Prompt {
            return ask;
        }
        if listed(&self.policy.auto_approve) {
            return ApprovalDecision::AutoApprove;
        }
        // Allow (silent)
        if listed(&self.policy.allow) {
            return ApprovalDecision::Allow;
        }
        // Not in any list: the autonomy mode, then interactive_approval.
        if verdict == Verdict::PreApproved {
            return ApprovalDecision::AutoApprove;
        }
        ask
    }

    /// Get the approval timeout in seconds.
    pub fn timeout_seconds(&self) -> u64 {
        self.policy.approval_timeout_seconds
    }
}

/// The host's policy for calls made by `Agent` sub-agents. Without it a
/// child ran under the headless gate: every Read/WebFetch ran whatever the
/// host policy said, and every Bash/Edit was refused with no prompt.
pub(crate) struct SdkPolicyAsker {
    pub policy: std::sync::Arc<PolicyEngine>,
    pub session_id: String,
    pub approval_tx: tokio::sync::mpsc::UnboundedSender<crate::sdk::protocol::SdkNotification>,
    /// Shared with the top-level loop, which holds it only while it waits,
    /// never while a tool (and so a child) runs.
    pub approval_rx: crate::sdk::session::ApprovalReceiver,
    pub cancel: std::sync::Arc<crate::sdk::session::CancelSignal>,
}

#[async_trait::async_trait]
impl crate::permissions::PermissionAsker for SdkPolicyAsker {
    async fn ask(
        &self,
        tool_name: &str,
        _description: &str,
        input: &serde_json::Value,
    ) -> Option<crate::permissions::PermissionDecision> {
        use crate::permissions::PermissionDecision;
        use crate::sdk::session::{ApprovalOutcome, await_approval};
        match self.policy.evaluate(tool_name, input) {
            ApprovalDecision::Allow | ApprovalDecision::AutoApprove => {
                return Some(PermissionDecision::Allow);
            }
            ApprovalDecision::Deny => return Some(PermissionDecision::Deny),
            ApprovalDecision::Ask if !self.policy.interactive_approval => {
                return Some(PermissionDecision::Deny);
            }
            ApprovalDecision::Ask => {}
        }
        // Holding the lock before announcing the request means parallel
        // children never consume each other's replies as stale.
        let mut rx = self.approval_rx.lock().await;
        let approval_id = uuid::Uuid::new_v4().to_string();
        // The browser's loopback question is not a sub-agent's tool call.
        let loopback = tool_name == crate::tools::browser_tools::LOOPBACK_QUESTION;
        let tool_use_id = if loopback {
            format!("browser-loopback-{approval_id}")
        } else {
            format!("subagent-{approval_id}")
        };
        self.approval_tx
            .send(crate::sdk::protocol::SdkNotification::ToolApprovalNeeded {
                session_id: self.session_id.clone(),
                approval_id: approval_id.clone(),
                tool: tool_name.to_string(),
                args: input.clone(),
                tool_use_id: tool_use_id.clone(),
            })
            .ok()?;
        let timeout = std::time::Duration::from_secs(self.policy.timeout_seconds());
        let outcome = tokio::select! {
            o = await_approval(&mut rx, &approval_id, timeout) => Some(o),
            _ = self.cancel.cancelled() => None,
        };
        let ok = matches!(outcome, Some(ApprovalOutcome::Approved));
        // Nothing else ever completes this made-up id: the child engine sends
        // no tool/completed. Without this an ACP client shows the approved
        // call spinning forever, and an SDK host waits on it.
        let _ = self
            .approval_tx
            .send(crate::sdk::protocol::SdkNotification::ToolCompleted {
                session_id: self.session_id.clone(),
                tool: tool_name.to_string(),
                tool_use_id,
                success: ok,
                output_summary: match (ok, loopback) {
                    (true, true) => "Approved: the browser may reach this local service.".into(),
                    (true, false) => "Approved for sub-agent.".into(),
                    (false, _) => "Denied.".into(),
                },
                duration_ms: 0,
            });
        outcome?;
        if ok {
            Some(PermissionDecision::Allow)
        } else {
            Some(PermissionDecision::Deny)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permissions::{PermissionAsker, PermissionDecision};
    use crate::sdk::protocol::SdkNotification;
    use std::sync::Arc;

    /// The browser's loopback question went out under a made-up
    /// "subagent-" call id and was closed with "Approved for sub-agent.",
    /// which described something that never happened.
    #[tokio::test]
    async fn the_loopback_question_is_not_reported_as_a_sub_agent_call() {
        let (tx, mut notes) = tokio::sync::mpsc::unbounded_channel();
        let (reply_tx, reply_rx) = tokio::sync::mpsc::unbounded_channel();
        let asker = SdkPolicyAsker {
            policy: Arc::new(PolicyEngine::new(Policy::default(), true)),
            session_id: "s".into(),
            approval_tx: tx,
            approval_rx: Arc::new(tokio::sync::Mutex::new(reply_rx)),
            cancel: Arc::default(),
        };
        let host = tokio::spawn(async move {
            let mut seen = Vec::new();
            while let Some(n) = notes.recv().await {
                match n {
                    SdkNotification::ToolApprovalNeeded {
                        approval_id,
                        tool_use_id,
                        ..
                    } => {
                        seen.push(tool_use_id);
                        let _ = reply_tx.send((approval_id, None));
                    }
                    SdkNotification::ToolCompleted {
                        tool_use_id,
                        output_summary,
                        success,
                        ..
                    } => {
                        seen.push(format!("{tool_use_id} {success} {output_summary}"));
                    }
                    _ => {}
                }
            }
            seen
        });
        let got = asker
            .ask(
                crate::tools::browser_tools::LOOPBACK_QUESTION,
                "q",
                &serde_json::json!({"url": "http://localhost:3000/"}),
            )
            .await;
        assert_eq!(got, Some(PermissionDecision::Allow));
        drop(asker);
        let seen = host.await.unwrap();
        assert_eq!(seen.len(), 2, "{seen:?}");
        assert!(seen[0].starts_with("browser-loopback-"), "{seen:?}");
        assert!(
            seen[1].starts_with(&format!("{} true ", seen[0])),
            "{seen:?}"
        );
        assert!(!seen[1].contains("sub-agent"), "{seen:?}");
    }
    /// full-auto pre-approved every tool name, the browser's loopback
    /// pseudo-tool included: the SDK asker granted a loopback host:port for
    /// a Chrome outside the sandbox without sending tool/approval_needed.
    #[test]
    fn full_auto_does_not_answer_the_loopback_question() {
        let proj = tempfile::tempdir().unwrap();
        let q = crate::tools::browser_tools::LOOPBACK_QUESTION;
        let input = serde_json::json!({"url": "http://localhost:3000/"});
        let engine = |interactive: bool, policy: Policy| {
            PolicyEngine::new(policy, interactive).with_autonomy(Autonomy::FullAuto, proj.path())
        };
        let e = engine(true, Policy::default());
        assert_eq!(e.evaluate(q, &input), ApprovalDecision::Ask);
        assert_eq!(
            e.evaluate("Bash", &serde_json::json!({"command": "ls"})),
            ApprovalDecision::AutoApprove
        );
        assert_eq!(
            engine(false, Policy::default()).evaluate(q, &input),
            ApprovalDecision::Deny
        );
        // The host's own lists still decide it.
        let allow = Policy {
            allow: vec![q.to_string()],
            ..Policy::default()
        };
        assert_eq!(
            engine(false, allow).evaluate(q, &input),
            ApprovalDecision::Allow
        );
    }
}
