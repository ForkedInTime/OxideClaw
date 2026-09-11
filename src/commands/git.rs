//! `/` command handlers — split out of `commands/mod.rs` mechanically.

use super::*;

pub(super) fn cmd_init(ctx: &CommandContext) -> CommandAction {
    let path = ctx.config.cwd.join("CLAUDE.md");
    let exists = path.exists();

    // Send a prompt to Claude to analyze the codebase and generate CLAUDE.md
    // This mirrors the real source's behavior: Claude explores the codebase
    // and writes a minimal, accurate CLAUDE.md rather than a generic template.
    let action_desc = if exists {
        format!(
            "CLAUDE.md already exists at {}. Suggest improvements to it.",
            path.display()
        )
    } else {
        format!("Create a new CLAUDE.md at {}.", path.display())
    };

    CommandAction::SendPrompt(format!(
        "Please analyze this codebase and {}

CLAUDE.md is loaded into every OxideClaw session. It must be concise — only include \
what Claude would get wrong without it.

## What to analyze

Read these key files if they exist:
- manifest files: package.json, Cargo.toml, pyproject.toml, go.mod, pom.xml, etc.
- README.md, Makefile, CI config (.github/workflows/, .circleci/, etc.)
- Existing CLAUDE.md (if any)
- .cursor/rules, .cursorrules, .github/copilot-instructions.md, AGENTS.md

Detect:
- Build, test, and lint commands (especially non-standard ones)
- Languages, frameworks, and package manager
- Project structure
- Code style rules that differ from language defaults
- Non-obvious gotchas or required environment variables

## What to include in CLAUDE.md

Only include lines that pass this test: \"Would removing this cause Claude to make mistakes?\"

Good candidates:
1. **Commands**: build, test, lint, format commands — especially non-standard ones
2. **Architecture**: High-level structure that requires reading multiple files to understand
3. **Gotchas**: Workflow quirks, required env vars, non-obvious conventions

Do NOT include:
- Generic practices like \"write unit tests\", \"provide helpful errors\", \"don't expose secrets\"
- Things that can be easily discovered by reading the code
- Repetition of what's in README unless it's critical to know

## Format

Start the file with:
```
# CLAUDE.md

This file provides guidance to OxideClaw when working with code in this repository.
```

Then add only the sections that have real content. Use terse, actionable language.",
        action_desc
    ))
}

pub(super) fn cmd_review(args: &str) -> CommandAction {
    let pr_ref = args.trim();
    CommandAction::SendPrompt(format!(
        "You are an expert code reviewer. Follow these steps:\n\n\
         1. {}
         2. Run `gh pr diff <number>` to get the diff\n\
         3. Analyze the changes and provide a thorough code review that includes:\n\
            - Overview of what the PR does\n\
            - Analysis of code quality and style\n\
            - Specific suggestions for improvements\n\
            - Any potential issues or risks\n\n\
         Keep your review concise but thorough. Focus on:\n\
         - Code correctness and logic\n\
         - Project conventions\n\
         - Performance implications\n\
         - Test coverage\n\
         - Security considerations\n\n\
         Format your review with clear sections and bullet points.",
        if pr_ref.is_empty() {
            "Run `gh pr list` to show open PRs, then ask the user which PR to review".to_string()
        } else {
            format!("Run `gh pr view {pr_ref}` to get PR details. PR: {pr_ref}")
        }
    ))
}

pub(super) fn cmd_lint(ctx: &CommandContext) -> CommandAction {
    // Detect project type from the working directory
    let cwd = &ctx.config.cwd;
    let mut checks = Vec::new();

    if cwd.join("Cargo.toml").exists() {
        checks.push("cargo clippy --all-targets -- -D warnings");
        checks.push("cargo test");
    }
    if cwd.join("package.json").exists() {
        checks.push("npm run lint 2>/dev/null || npx eslint . 2>/dev/null || true");
        checks.push("npm test 2>/dev/null || true");
    }
    if cwd.join("pyproject.toml").exists() || cwd.join("setup.py").exists() {
        checks.push("ruff check . 2>/dev/null || python -m flake8 . 2>/dev/null || true");
        checks.push("python -m pytest 2>/dev/null || true");
    }
    if cwd.join("go.mod").exists() {
        checks.push("go vet ./...");
        checks.push("go test ./...");
    }

    if checks.is_empty() {
        return CommandAction::Message(
            "No recognized project type found (Cargo.toml, package.json, pyproject.toml, go.mod)."
                .into(),
        );
    }

    let cmds = checks.join("\n  ");
    CommandAction::SendPrompt(format!(
        "Run the following lint/test commands for this project and fix any errors or warnings. \
         Keep running them in a loop until they all pass cleanly:\n\n  {cmds}\n\n\
         For each failure:\n\
         1. Read the error output carefully\n\
         2. Fix the root cause in the source code\n\
         3. Re-run the failing command to verify the fix\n\
         4. Repeat until all commands pass with zero errors/warnings\n\n\
         Report what you fixed when done."
    ))
}

pub(super) fn cmd_branch(ctx: &CommandContext) -> CommandAction {
    let run = |args: &[&str]| -> Option<String> {
        std::process::Command::new("git")
            .args(args)
            .current_dir(&ctx.config.cwd)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .filter(|s| !s.is_empty())
    };

    let current =
        run(&["rev-parse", "--abbrev-ref", "HEAD"]).unwrap_or_else(|| "not a git repo".into());
    if current == "not a git repo" {
        return CommandAction::Message("Not a git repository.".into());
    }

    let mut lines = vec![format!("Current branch: {current}\n")];

    if let Some(log) = run(&["log", "--oneline", "-5"]) {
        lines.push("Recent commits:".into());
        for l in log.lines() {
            lines.push(format!("  {l}"));
        }
        lines.push(String::new());
    }

    if let Some(status) = run(&["status", "--short"]) {
        if !status.is_empty() {
            lines.push("Working tree changes:".into());
            for l in status.lines().take(15) {
                lines.push(format!("  {l}"));
            }
        } else {
            lines.push("Working tree: clean".into());
        }
    }

    if let Some(branches) = run(&["branch", "-a", "--format=%(refname:short)"]) {
        let all: Vec<_> = branches
            .lines()
            .filter(|b| !b.contains("HEAD"))
            .take(10)
            .collect();
        if !all.is_empty() {
            lines.push(String::new());
            lines.push("Branches:".into());
            for b in all {
                let marker = if b == current { "▶ " } else { "  " };
                lines.push(format!("{marker}{b}"));
            }
        }
    }

    CommandAction::Message(lines.join("\n"))
}

pub(super) fn cmd_pr_comments(args: &str, ctx: &CommandContext) -> CommandAction {
    let pr = args.trim();
    let mut cmd = std::process::Command::new("gh");
    if pr.is_empty() {
        cmd.args(["pr", "view", "--json", "comments,reviews,number,title"]);
    } else {
        cmd.args(["pr", "view", pr, "--json", "comments,reviews,number,title"]);
    }
    cmd.current_dir(&ctx.config.cwd);

    match cmd.output() {
        Err(_) => CommandAction::Message(
            "gh CLI not found. Install the GitHub CLI (gh) to use /pr_comments.".into(),
        ),
        Ok(o) if !o.status.success() => {
            let err = String::from_utf8_lossy(&o.stderr).trim().to_string();
            CommandAction::Message(format!("gh pr view failed: {err}"))
        }
        Ok(o) => {
            let out = String::from_utf8_lossy(&o.stdout).to_string();
            // Parse minimally — just show raw JSON summary or pass to Claude
            CommandAction::SendPrompt(format!(
                "The following is the GitHub PR data. Please summarize the comments and reviews:\n\n{out}"
            ))
        }
    }
}

pub(super) fn cmd_commit(args: &str, _ctx: &CommandContext) -> CommandAction {
    let msg = args.trim();
    if msg.is_empty() {
        // Ask Claude to generate a commit
        CommandAction::SendPrompt(
            "Please review the git diff and staged changes, then create an appropriate git commit \
             with a clear, concise commit message. Use conventional commit format if applicable. \
             Run `git add -A && git commit -m <message>` to commit."
                .into(),
        )
    } else {
        // User provided the message — just run it
        CommandAction::SendPrompt(format!(
            "Please run: git add -A && git commit -m {msg:?}\n\
             Then confirm the commit was successful."
        ))
    }
}

pub(super) fn cmd_commit_push_pr(args: &str, ctx: &CommandContext) -> CommandAction {
    let branch = args.trim();
    let branch_clause = if branch.is_empty() {
        String::new()
    } else {
        format!(" to branch '{branch}'")
    };
    let _ = ctx;
    CommandAction::SendPrompt(format!(
        "Please: \
         1. Review the git diff for any staged/unstaged changes. \
         2. Create a git commit with an appropriate message. \
         3. Push the commit{branch_clause}. \
         4. Create a GitHub pull request using `gh pr create` with a clear title and description. \
         Show me the PR URL when done."
    ))
}

pub(super) fn cmd_security_review() -> CommandAction {
    // Mirrors the real security-review.ts prompt methodology:
    // git diff to get changes, 3-phase analysis, false-positive filtering
    CommandAction::SendPrompt(concat!(
        "You are a senior security engineer conducting a focused security review of the changes on this branch.\n\n",
        "First, run these commands to gather context:\n",
        "- git status\n",
        "- git diff --name-only origin/HEAD...\n",
        "- git log --no-decorate origin/HEAD...\n",
        "- git diff origin/HEAD...\n\n",
        "OBJECTIVE:\n",
        "Identify HIGH-CONFIDENCE security vulnerabilities with real exploitation potential. ",
        "Focus ONLY on security issues newly introduced by the current changes. ",
        "Do not comment on pre-existing concerns.\n\n",
        "CRITICAL INSTRUCTIONS:\n",
        "1. MINIMIZE FALSE POSITIVES: Only flag issues where >80% confidence of actual exploitability\n",
        "2. EXCLUSIONS — do NOT report:\n",
        "   - Denial of Service vulnerabilities\n",
        "   - Secrets stored on disk (handled elsewhere)\n",
        "   - Rate limiting/resource exhaustion\n",
        "   - UUIDs (assumed unguessable)\n",
        "   - Environment variable / CLI flag injection (trusted values)\n",
        "   - Memory/file descriptor leaks\n",
        "   - Tabnabbing, XS-Leaks, open redirects (unless very high confidence)\n",
        "   - XSS in React/Angular unless using dangerouslySetInnerHTML or similar\n",
        "   - Missing auth checks in client-side code (server is responsible)\n\n",
        "SECURITY CATEGORIES TO EXAMINE:\n",
        "- SQL/command/XXE/template/NoSQL injection\n",
        "- Authentication bypass, privilege escalation, session flaws\n",
        "- Hardcoded secrets, weak crypto, improper key storage\n",
        "- Path traversal, insecure deserialization, RCE\n",
        "- SSRF, CORS misconfigurations, insecure direct object references\n\n",
        "ANALYSIS PROCESS (3 phases):\n",
        "1. Use a sub-task (Task tool) to identify all candidate vulnerabilities\n",
        "2. For each candidate, launch a parallel sub-task to filter false positives\n",
        "3. Only include findings with confidence >= 8/10\n\n",
        "FINAL REPORT FORMAT (markdown):\n",
        "## Security Review\n",
        "### [CRITICAL|HIGH|MEDIUM] Finding Title\n",
        "- **File**: path/to/file.rs:line\n",
        "- **Description**: What the vulnerability is\n",
        "- **Attack path**: Concrete exploitation steps\n",
        "- **Fix**: Recommended remediation\n",
        "- **Confidence**: N/10\n\n",
        "If no issues found, state: 'No high-confidence security vulnerabilities identified in these changes.'"
    ).into())
}

pub(super) fn cmd_init_verifiers() -> CommandAction {
    CommandAction::SendPrompt(
        r#"Use the TodoWrite tool to track your progress through this multi-step task.

## Goal

Create one or more verifier skills that can be used by the Verify agent to automatically verify code changes in this project or folder. You may create multiple verifiers if the project has different verification needs (e.g., both web UI and API endpoints).

**Do NOT create verifiers for unit tests or typechecking.** Those are already handled by the standard build/test workflow and don't need dedicated verifier skills. Focus on functional verification: web UI (Playwright), CLI (Tmux), and API (HTTP) verifiers.

## Phase 1: Auto-Detection

Analyze the project to detect what's in different subdirectories. The project may contain multiple sub-projects or areas that need different verification approaches (e.g., a web frontend, an API backend, and shared libraries all in one repo).

1. **Scan top-level directories** to identify distinct project areas:
   - Look for separate package.json, Cargo.toml, pyproject.toml, go.mod in subdirectories
   - Identify distinct application types in different folders

2. **For each area, detect:**

   a. **Project type and stack** — Primary language(s), frameworks, package managers

   b. **Application type**
      - Web app (React, Next.js, Vue, etc.) -> suggest Playwright-based verifier
      - CLI tool -> suggest Tmux-based verifier
      - API service (Express, FastAPI, etc.) -> suggest HTTP-based verifier

   c. **Existing verification tools** — Test frameworks, E2E tools, dev server scripts

   d. **Dev server configuration** — How to start, URL, ready signal

3. **Installed verification packages** (for web apps)
   - Check if Playwright is installed (look in package.json dependencies/devDependencies)
   - Check MCP configuration (.mcp.json) for browser automation tools

## Phase 2: Verification Tool Setup

Based on what was detected in Phase 1, help the user set up appropriate verification tools.

### For Web Applications

1. **If browser automation tools are already installed/configured**, ask the user which one they want to use via AskUserQuestion.

2. **If NO browser automation tools are detected**, ask if they want to install/configure one:
   - Options: Playwright (Recommended), Chrome DevTools MCP, Claude Chrome Extension, None

3. **If user chooses to install Playwright**, run the appropriate command based on package manager.

4. **If user chooses Chrome DevTools MCP or Claude Chrome Extension**, configure .mcp.json accordingly.

### For CLI Tools

1. Check if asciinema is available (run `which asciinema`).
2. Tmux is typically system-installed, just verify it's available.

### For API Services

1. Check if HTTP testing tools are available (curl, httpie).

## Phase 3: Interactive Q&A

For each distinct area, use AskUserQuestion to confirm:

1. **Verifier name** — suggest based on detection:
   - Single area: "verifier-playwright", "verifier-cli", "verifier-api"
   - Multiple areas: "verifier-<project>-<type>" (e.g., "verifier-frontend-playwright")
   - MUST include "verifier" in the name for auto-discovery

2. **Project-specific questions** based on type (dev server command, URL, ready signal, etc.)

3. **Authentication & Login** — ask if the app requires authentication to access pages/endpoints being verified.

## Phase 4: Generate Verifier Skill

Write the skill file to `.claude/skills/<verifier-name>/SKILL.md`.

Use this template:

```
---
name: <verifier-name>
description: <description based on type>
allowed-tools:
  # Tools appropriate for the verifier type
---

# <Verifier Title>

You are a verification executor. You receive a verification plan and execute it EXACTLY as written.

## Project Context
<Project-specific details from detection>

## Setup Instructions
<How to start any required services>

## Authentication
<If auth is required, include step-by-step login instructions here>
<If no auth needed, omit this section>

## Reporting

Report PASS or FAIL for each step using the format specified in the verification plan.

## Cleanup

After verification:
1. Stop any dev servers started
2. Close any browser sessions
3. Report final summary

## Self-Update

If verification fails because this skill's instructions are outdated (not because the feature under test is broken), use AskUserQuestion to confirm and then Edit this SKILL.md with a minimal targeted fix.
```

Allowed tools by type:
- verifier-playwright: Bash(npm:*), Bash(yarn:*), Bash(pnpm:*), Bash(bun:*), mcp__playwright__*, Read, Glob, Grep
- verifier-cli: Tmux, Bash(asciinema:*), Read, Glob, Grep
- verifier-api: Bash(curl:*), Bash(http:*), Bash(npm:*), Bash(yarn:*), Read, Glob, Grep

## Phase 5: Confirm Creation

After writing the skill file(s), inform the user:
1. Where each skill was created (always in `.claude/skills/`)
2. How the Verify agent will discover them (folder name must contain "verifier")
3. That they can edit the skills to customize them
4. That they can run /init-verifiers again to add more verifiers for other areas"#.into()
    )
}

pub(super) fn cmd_autofix_pr(args: &str) -> CommandAction {
    let pr_ref = args.trim();
    if pr_ref.is_empty() {
        return CommandAction::Message(
            "Auto-fix PR review comments.\n\n\
             Usage: /autofix-pr [<pr-number-or-url>]\n\
             Example: /autofix-pr 42\n\n\
             Without an argument, fixes comments on the current branch's open PR.\n\
             Requires: gh CLI installed and authenticated."
                .into(),
        );
    }
    CommandAction::SendPrompt(format!(
        "Using the gh CLI, read all review comments on PR {pr_ref}.\n\
         For each actionable comment:\n\
         1. Understand what change is needed\n\
         2. Make the change in the relevant file\n\
         3. Note what was changed\n\
         After all fixes, run any relevant tests and commit with a summary message."
    ))
}

pub(super) fn cmd_autocommit(_args: &str) -> CommandAction {
    // v1 only supports `status`. Any arg (or none) shows status.
    CommandAction::AutoCommitStatus
}

pub(super) fn cmd_redo(args: &str) -> CommandAction {
    let n = args.trim().parse::<u32>().ok().filter(|n| *n > 0);
    CommandAction::Redo { n }
}

pub(super) fn cmd_undo(args: &str) -> CommandAction {
    let n = args.trim().parse::<u32>().ok().filter(|n| *n > 0);
    CommandAction::Undo { n }
}

pub(super) fn cmd_issue(args: &str) -> CommandAction {
    let desc = args.trim();
    if desc.is_empty() {
        return CommandAction::Message(
            "Create a GitHub issue.\n\n\
             Usage: /issue <description>\n\
             Example: /issue login fails when username contains spaces\n\n\
             Requires: gh CLI installed and authenticated (gh auth login)"
                .into(),
        );
    }
    CommandAction::SendPrompt(format!(
        "Create a GitHub issue for: {desc}\n\n\
         Steps:\n\
         1. Analyse the codebase and conversation for relevant context\n\
         2. Draft a structured issue: title, description, steps to reproduce, \
            expected vs actual behaviour, environment\n\
         3. Use the gh CLI: gh issue create --title '...' --body '...'\n\
         4. Return the issue URL."
    ))
}
