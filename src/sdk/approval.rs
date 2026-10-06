//! Policy evaluation for tool approval.
//!
//! Evaluation order: deny > ask > auto_approve > allow.
//! Unlisted tools: ask (if interactive_approval) or deny (if not).

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
}

impl PolicyEngine {
    pub fn new(policy: Policy, interactive_approval: bool) -> Self {
        Self {
            policy,
            interactive_approval,
        }
    }

    /// Evaluate a tool name against the policy.
    pub fn evaluate(&self, tool_name: &str) -> ApprovalDecision {
        // Deny takes highest priority
        if self.policy.deny.iter().any(|t| t == tool_name) {
            return ApprovalDecision::Deny;
        }
        // Ask is next
        if self.policy.ask.iter().any(|t| t == tool_name) {
            return ApprovalDecision::Ask;
        }
        // Auto-approve
        if self.policy.auto_approve.iter().any(|t| t == tool_name) {
            return ApprovalDecision::AutoApprove;
        }
        // Allow (silent)
        if self.policy.allow.iter().any(|t| t == tool_name) {
            return ApprovalDecision::Allow;
        }
        // Not in any list — depends on interactive_approval capability
        if self.interactive_approval {
            ApprovalDecision::Ask
        } else {
            ApprovalDecision::Deny
        }
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
        match self.policy.evaluate(tool_name) {
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
        let tool_use_id = format!("subagent-{approval_id}");
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
                output_summary: if ok {
                    "Approved for sub-agent.".into()
                } else {
                    "Denied.".into()
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
