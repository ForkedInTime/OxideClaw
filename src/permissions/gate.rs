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
    Autonomy, CheckResult, PermissionDecision, PermissionState, ShellGrammar, Verdict,
    blocked_entry_matches, check_compound_command_as, describe_tool_call, is_command_tool,
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
    /// What the mode pre-approves or forces to a prompt (see
    /// [`Autonomy::verdict`]); never what a deny rule refuses.
    autonomy: Autonomy,
    asker: Option<Arc<dyn PermissionAsker>>,
    /// Tools refused outright for this turn (plan mode, skill turns), each
    /// with the reason the model is told. Inherited by sub-agents through
    /// the gate, so a child launched in plan mode cannot write either.
    blocked: Vec<(String, &'static str)>,
    /// Consult the asker for every tool that passes the deny list, not only
    /// the sensitive ones: an SDK/ACP host policy covers Read and WebFetch
    /// too, and an `Agent` child must not get around it.
    ask_every_tool: bool,
    /// How the shell that runs Bash commands reads them (`defaultShell`).
    bash_shell: ShellGrammar,
}

impl PermissionGate {
    pub fn new(
        state: PermissionState,
        autonomy: Autonomy,
        asker: Option<Arc<dyn PermissionAsker>>,
    ) -> Self {
        Self {
            state,
            autonomy,
            asker,
            blocked: Vec::new(),
            ask_every_tool: false,
            bash_shell: ShellGrammar::Posix,
        }
    }

    /// Check Bash commands with the grammar of `shell`, the program that
    /// will parse them (see `tools::bash::command_shell`): rules read with
    /// bash quoting let a pwsh or fish `defaultShell` hide a second command.
    pub fn with_bash_shell(mut self, shell: &str) -> Self {
        self.bash_shell = ShellGrammar::of_shell(shell);
        self
    }

    /// See [`PermissionState::read_deny`].
    pub fn read_deny(&self, tool_name: &str) -> super::ReadDeny {
        self.state.read_deny(tool_name)
    }

    /// Put prompts to `asker` (a browse run's approval channel).
    pub fn with_asker(mut self, asker: Arc<dyn PermissionAsker>) -> Self {
        self.asker = Some(asker);
        self
    }

    /// Ask the human behind this gate a yes/no question that no tool rule
    /// answers: whether the browser may reach a loopback service. `None`
    /// when nobody can be asked (headless, `/spawn`); an unanswered prompt
    /// is `Some(false)`.
    pub async fn ask_human(
        &self,
        tool_name: &str,
        description: &str,
        input: &serde_json::Value,
    ) -> Option<bool> {
        let asker = self.asker.as_ref()?;
        Some(matches!(
            asker.ask(tool_name, description, input).await,
            Some(PermissionDecision::Allow | PermissionDecision::AlwaysAllow)
        ))
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
    /// headless use). Settings/CLI allow and deny rules and the autonomy
    /// mode still apply; anything that would need a prompt is refused.
    pub fn headless(cfg: &crate::config::Config) -> Self {
        Self::new(
            PermissionState::new(
                cfg.dangerously_skip_permissions,
                &cfg.permissions_allow,
                &cfg.permissions_deny,
            )
            .with_cwd(&cfg.cwd),
            cfg.effective_autonomy(),
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
            Autonomy::Ask,
            None,
        )
    }

    /// [`Self::decide_in`] the gate's own project directory.
    pub async fn decide(&self, tool_name: &str, input: &serde_json::Value) -> GateOutcome {
        self.decide_in(tool_name, input, &self.state.cwd()).await
    }

    /// Decide a call whose relative paths the tool resolves against
    /// `work_cwd`: the session's cwd, which `EnterWorktree` moves out of the
    /// launch project into a sibling worktree. The autonomy mode judges the
    /// call there, so `auto-edit` pre-approves only edits inside the tree the
    /// tool actually writes under, with symlinks checked in that tree.
    pub async fn decide_in(
        &self,
        tool_name: &str,
        input: &serde_json::Value,
        work_cwd: &std::path::Path,
    ) -> GateOutcome {
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
                Some(cmd) => {
                    check_compound_command_as(&self.state, tool_name, cmd, self.bash_shell)
                }
                None => self.state.check_with_input(tool_name, Some(input)),
            }
        } else {
            self.state.check_with_input(tool_name, Some(input))
        };
        // The mode moves a call between Allow and Ask, never out of Deny: a
        // deny rule refuses outright in every mode instead of becoming one
        // more routine approval.
        let verdict = self.autonomy.verdict(tool_name, input, work_cwd);
        // Nothing pre-approves a command the deny rules could not be checked
        // against; where no prompt is shown it is refused.
        if matches!(check, CheckResult::Unverified)
            && (self.state.bypass() || verdict == Verdict::PreApproved)
        {
            return GateOutcome::Denied(format!(
                "Permission denied: {tool_name}: the command has a quote, bracket or heredoc \
                 that does not close, so it cannot be checked against your permissions.deny \
                 rules, and no prompt is shown in this mode to confirm it. Close it or split \
                 the command."
            ));
        }
        // `suggest` turned an allowed edit into a prompt: no rule or flag
        // can let it through without one.
        let forced_prompt = matches!((&check, verdict), (CheckResult::Allow, Verdict::Prompt));
        let check = match (check, verdict) {
            (CheckResult::Unverified, _) => CheckResult::Ask,
            (CheckResult::Allow, Verdict::Prompt) => CheckResult::Ask,
            (CheckResult::Ask, Verdict::PreApproved) => CheckResult::Allow,
            (CheckResult::Allow, _) if self.ask_every_tool => CheckResult::Ask,
            (c, _) => c,
        };

        match check {
            CheckResult::Allow => GateOutcome::Allowed,
            CheckResult::Deny => GateOutcome::Denied(format!("Permission denied: {tool_name}")),
            CheckResult::Ask | CheckResult::Unverified => match &self.asker {
                // Only name remedies that grant permission: a bare
                // --allowed-tools name filters the tool list but never
                // authorises a call; an --allowed-tools rule does.
                None if forced_prompt => GateOutcome::Denied(format!(
                    "Permission denied: {tool_name} needs a prompt, which no interactive \
                     session is attached to show: autonomy is \"{}\", where every edit \
                     prompts whatever the rules allow. Set \"autonomy\" to \"ask\" or \
                     looser to let permissions.allow rules through.",
                    self.autonomy
                )),
                None => GateOutcome::Denied(format!(
                    "Permission denied: {tool_name} requires approval and no interactive \
                     session is attached. Allow it with a permissions.allow rule in \
                     settings.json or an --allowed-tools rule such as \
                     'Bash(git status:*)', or pass --dangerously-skip-permissions."
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
            Autonomy::Ask,
            asker.map(|a| a as Arc<dyn PermissionAsker>),
        )
    }

    /// The browser's loopback question goes to the same human as tool
    /// prompts; with nobody attached the caller is told so, and a dropped
    /// prompt is a no.
    #[tokio::test]
    async fn ask_human_reaches_the_asker_or_reports_nobody() {
        let input = json!({"url": "http://127.0.0.1:3000/"});
        assert_eq!(
            gate(&[], None)
                .ask_human("browser_loopback", "q", &input)
                .await,
            None
        );
        let asker = Scripted::new(vec![
            Some(PermissionDecision::Allow),
            Some(PermissionDecision::Deny),
            None,
        ]);
        let g = gate(&[], Some(asker.clone()));
        assert_eq!(
            g.ask_human("browser_loopback", "q", &input).await,
            Some(true)
        );
        assert_eq!(
            g.ask_human("browser_loopback", "q", &input).await,
            Some(false)
        );
        assert_eq!(
            g.ask_human("browser_loopback", "q", &input).await,
            Some(false)
        );
        assert_eq!(asker.asked().len(), 3);
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
        // (`--allowedTools`), and must name the --allowed-tools form that
        // grants (a rule), not a bare name, which only filters the tools.
        let GateOutcome::Denied(msg) = out else {
            unreachable!()
        };
        assert!(msg.contains("permissions.allow"), "{msg}");
        assert!(msg.contains("--dangerously-skip-permissions"), "{msg}");
        assert!(!msg.to_lowercase().contains("allowedtools"), "{msg}");
        assert!(
            msg.contains("--allowed-tools rule such as 'Bash(git status:*)'"),
            "{msg}"
        );
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
            Autonomy::Suggest,
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
            Autonomy::Suggest,
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
        let g = PermissionGate::new(PermissionState::new(true, &[], &deny), Autonomy::Ask, None);
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

    /// `auto-edit` and `full-auto` were stored and never read: every mode
    /// but `suggest` prompted for exactly the same calls. Each mode against
    /// edits inside the project, of protected files and outside it, a
    /// command, and a deny rule.
    #[tokio::test]
    async fn each_mode_against_the_tool_call_matrix() {
        let proj = tempfile::tempdir().unwrap();
        let root = proj.path();
        let outside = tempfile::tempdir().unwrap();
        let w = |p: &str| json!({"file_path": p, "content": "x"});
        let out_path = outside.path().join("x.rs").to_string_lossy().into_owned();
        let protected = [
            ".git/hooks/pre-commit",
            ".claude/settings.json",
            ".oxideclaw/x",
            ".agents/skills/s/SKILL.md",
            ".mcp.json",
            ".env.local",
            ".github/workflows/ci.yml",
            ".gitlab-ci.yml",
            ".husky/pre-commit",
            "package.json",
            "Cargo.toml",
            "build.rs",
            "conftest.py",
            "pytest.ini",
            "Makefile",
            "justfile",
            "setup.py",
            "pyproject.toml",
            "tox.ini",
            "App.csproj",
            "jest.config.js",
        ];
        // (call, prompts under: suggest, ask, auto-edit, full-auto)
        let mut calls: Vec<(&str, serde_json::Value, [bool; 4])> = vec![
            ("Write", w("src/a.rs"), [true, true, false, false]),
            ("Edit", w("src/a.rs"), [true, true, false, false]),
            (
                "MultiEdit",
                json!({"edits": [{"file_path": "a.rs"}, {"file_path": "b.rs"}]}),
                [true, true, false, false],
            ),
            (
                "NotebookEdit",
                json!({"notebook_path": "nb.ipynb"}),
                [true, true, false, false],
            ),
            ("Write", w(&out_path), [true, true, true, true]),
            ("Write", w("../escape.rs"), [true, true, true, true]),
            (
                "Bash",
                json!({"command": "cargo build"}),
                [true, true, true, false],
            ),
            ("mcp__fs__write_file", json!({}), [true, true, true, true]),
            ("ExitPlanMode", json!({}), [true, true, true, true]),
        ];
        for p in protected {
            calls.push(("Write", w(p), [true, true, true, true]));
        }
        let modes = [
            Autonomy::Suggest,
            Autonomy::Ask,
            Autonomy::AutoEdit,
            Autonomy::FullAuto,
        ];
        for (i, mode) in modes.into_iter().enumerate() {
            for (tool, input, prompts) in &calls {
                let asker = Scripted::new(vec![Some(PermissionDecision::Allow)]);
                let g = PermissionGate::new(
                    PermissionState::new(false, &[], &[]).with_cwd(root),
                    mode,
                    Some(asker.clone() as Arc<dyn PermissionAsker>),
                );
                assert_eq!(g.decide(tool, input).await, GateOutcome::Allowed);
                assert_eq!(
                    asker.asked().len(),
                    usize::from(prompts[i]),
                    "{mode}: {tool} {input}"
                );
            }
            // A deny rule refuses without a prompt in every mode.
            let asker = Scripted::new(vec![Some(PermissionDecision::Allow)]);
            let deny = vec!["Edit(./src/**)".to_string(), "Bash(cargo:*)".to_string()];
            let g = PermissionGate::new(
                PermissionState::new(false, &[], &deny).with_cwd(root),
                mode,
                Some(asker.clone() as Arc<dyn PermissionAsker>),
            );
            for (tool, input) in [
                ("Write", w("src/a.rs")),
                ("Bash", json!({"command": "cargo build"})),
            ] {
                let out = g.decide(tool, &input).await;
                assert!(
                    matches!(out, GateOutcome::Denied(_)),
                    "{mode}: {tool} {out:?}"
                );
            }
            assert!(asker.asked().is_empty(), "{mode}: a denied call prompted");
        }
    }

    /// A bare `deny: ["Edit"]` (a read-only session) stopped only Edit, so
    /// auto-edit pre-approved Write and MultiEdit of in-project files.
    #[tokio::test]
    async fn bare_edit_deny_holds_for_every_writing_tool_under_auto_edit() {
        let proj = tempfile::tempdir().unwrap();
        let g = PermissionGate::new(
            PermissionState::new(false, &[], &["Edit".into()]).with_cwd(proj.path()),
            Autonomy::AutoEdit,
            None,
        );
        for (tool, input) in [
            ("Write", json!({"file_path": "src/a.rs", "content": "x"})),
            ("MultiEdit", json!({"edits": [{"file_path": "src/a.rs"}]})),
            ("NotebookEdit", json!({"notebook_path": "nb.ipynb"})),
        ] {
            let out = g.decide(tool, &input).await;
            assert!(matches!(out, GateOutcome::Denied(_)), "{tool}: {out:?}");
        }
    }

    /// Bash commands that pwsh parses are checked with its quoting: `\"`
    /// does not hide the `rm` from a `git:*` rule.
    #[tokio::test]
    async fn bash_rules_use_the_grammar_of_the_shell_that_runs_them() {
        let cmd = json!({"command": r#"git log "a\"; rm -rf ~; "b""#});
        let gate = |shell: &str| {
            PermissionGate::new(
                PermissionState::new(false, &["Bash(git:*)".into()], &[]),
                Autonomy::Ask,
                None,
            )
            .with_bash_shell(shell)
        };
        assert_eq!(
            gate("bash").decide("Bash", &cmd).await,
            GateOutcome::Allowed
        );
        assert!(matches!(
            gate("pwsh").decide("Bash", &cmd).await,
            GateOutcome::Denied(_)
        ));
    }

    /// Under full-auto and bypass, deny rules are the only guard on a
    /// command; a subshell or `VAR=x` prefix slipped past them.
    #[tokio::test]
    async fn deny_rules_hold_for_nested_commands_in_every_mode() {
        let deny = ["Bash(git push:*)".to_string()];
        let unreadable = json!({"command": "echo $(cat <<E\n$(git push)\nE\n)"});
        let bypass = PermissionGate::bypass_with_deny(&deny, std::path::Path::new("/proj"));
        for cmd in [
            "(git push --force)",
            "GIT_TRACE=1 git push",
            "echo $(git push)",
        ] {
            assert!(
                matches!(
                    bypass.decide("Bash", &json!({ "command": cmd })).await,
                    GateOutcome::Denied(_)
                ),
                "{cmd:?}"
            );
        }
        let out = bypass.decide("Bash", &unreadable).await;
        assert!(
            matches!(out, GateOutcome::Denied(ref m) if m.contains("does not close")),
            "{out:?}"
        );
        let full_auto = PermissionGate::new(
            PermissionState::new(false, &[], &deny).with_cwd(std::path::Path::new("/proj")),
            Autonomy::FullAuto,
            None,
        );
        assert!(matches!(
            full_auto.decide("Bash", &unreadable).await,
            GateOutcome::Denied(_)
        ));
        // Where a prompt is shown, it asks, even with Bash allowed outright.
        let asker = Scripted::new(vec![Some(PermissionDecision::Allow)]);
        let ask = PermissionGate::new(
            PermissionState::new(false, &["Bash".into()], &deny),
            Autonomy::Ask,
            Some(asker.clone() as Arc<dyn PermissionAsker>),
        );
        assert_eq!(ask.decide("Bash", &unreadable).await, GateOutcome::Allowed);
        assert_eq!(asker.asked().len(), 1);
    }

    #[tokio::test]
    async fn auto_edit_pre_approves_nothing_when_the_project_is_home() {
        let Some(home) = dirs::home_dir() else {
            return;
        };
        let asker = Scripted::new(vec![Some(PermissionDecision::Deny)]);
        let g = PermissionGate::new(
            PermissionState::new(false, &[], &[]).with_cwd(&home),
            Autonomy::AutoEdit,
            Some(asker.clone() as Arc<dyn PermissionAsker>),
        );
        let out = g
            .decide("Write", &json!({"file_path": "notes.txt", "content": "x"}))
            .await;
        assert!(matches!(out, GateOutcome::Denied(_)), "{out:?}");
        assert_eq!(asker.asked().len(), 1, "the edit must have prompted");
    }

    /// After `EnterWorktree` the tools resolve relative paths in a sibling
    /// worktree outside the launch project. The gate judged them against the
    /// launch project, so a relative Write was "inside the project" (and its
    /// symlinks checked there) while the file landed elsewhere.
    #[cfg(unix)]
    #[tokio::test]
    async fn auto_edit_judges_edits_where_the_session_writes() {
        let launch = tempfile::tempdir().unwrap();
        let worktree = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        // In the worktree only: a symlink out of it.
        std::os::unix::fs::symlink(outside.path(), worktree.path().join("out")).unwrap();
        let w = |p: &str| json!({"file_path": p, "content": "x"});
        let launch_dir = launch.path().to_path_buf();
        let asks = |work: &std::path::Path, input: serde_json::Value| {
            let (work, launch_dir) = (work.to_path_buf(), launch_dir.clone());
            async move {
                let asker = Scripted::new(vec![Some(PermissionDecision::Allow)]);
                let g = PermissionGate::new(
                    PermissionState::new(false, &[], &[]).with_cwd(&launch_dir),
                    Autonomy::AutoEdit,
                    Some(asker.clone() as Arc<dyn PermissionAsker>),
                );
                assert_eq!(
                    g.decide_in("Write", &input, &work).await,
                    GateOutcome::Allowed
                );
                asker.asked().len()
            }
        };
        let wt = worktree.path();
        assert_eq!(asks(wt, w("src/a.rs")).await, 0, "inside the worktree");
        let abs = wt.join("src/b.rs").to_string_lossy().into_owned();
        assert_eq!(asks(wt, w(&abs)).await, 0, "absolute, inside the worktree");
        assert_eq!(
            asks(wt, w("out/x.rs")).await,
            1,
            "symlink out of the worktree"
        );
        assert_eq!(
            asks(wt, w("Cargo.toml")).await,
            1,
            "protected in the worktree"
        );
        // Judged in the launch project, where `out` is a plain directory.
        assert_eq!(asks(launch.path(), w("out/x.rs")).await, 0);
    }

    /// `-p` has no one to ask: auto-edit runs in-project edits, commands are
    /// still refused, and full-auto without its sandbox is `ask`.
    #[tokio::test]
    async fn headless_gate_applies_the_configured_mode() {
        let proj = tempfile::tempdir().unwrap();
        let mut cfg = crate::config::Config {
            cwd: proj.path().to_path_buf(),
            autonomy: Autonomy::AutoEdit,
            ..crate::config::Config::default()
        };
        let edit = json!({"file_path": "src/a.rs", "content": "x"});
        let bash = json!({"command": "ls"});
        let g = PermissionGate::headless(&cfg);
        assert_eq!(g.decide("Write", &edit).await, GateOutcome::Allowed);
        assert!(matches!(
            g.decide("Write", &json!({"file_path": "Makefile"})).await,
            GateOutcome::Denied(_)
        ));
        assert!(matches!(
            g.decide("Bash", &bash).await,
            GateOutcome::Denied(_)
        ));

        // `suggest` refuses every edit in -p, and must not point at rules
        // or flags that cannot get past it.
        cfg.autonomy = Autonomy::Suggest;
        cfg.permissions_allow = vec!["Write".into()];
        cfg.dangerously_skip_permissions = true;
        let GateOutcome::Denied(why) = PermissionGate::headless(&cfg).decide("Write", &edit).await
        else {
            panic!("suggest let an edit through with no one to ask");
        };
        assert!(
            why.contains("\"suggest\"") && why.contains("\"ask\""),
            "{why}"
        );
        assert!(!why.contains("--dangerously-skip-permissions"), "{why}");
        cfg.autonomy = Autonomy::Ask;
        cfg.dangerously_skip_permissions = false;
        let GateOutcome::Denied(why) = PermissionGate::headless(&cfg).decide("Bash", &bash).await
        else {
            panic!("an unallowed command ran with no one to ask");
        };
        assert!(why.contains("permissions.allow"), "{why}");
        cfg.permissions_allow.clear();

        cfg.autonomy = Autonomy::FullAuto;
        cfg.sandbox_enabled = false;
        let g = PermissionGate::headless(&cfg);
        assert!(matches!(
            g.decide("Bash", &bash).await,
            GateOutcome::Denied(_)
        ));
        assert!(matches!(
            g.decide("Write", &edit).await,
            GateOutcome::Denied(_)
        ));
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
