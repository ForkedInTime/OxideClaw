//! `/` command handlers — split out of `commands/mod.rs` mechanically.

use super::*;

pub(super) fn cmd_status(ctx: &CommandContext) -> CommandAction {
    let cwd = ctx.config.cwd.display().to_string();

    let git_branch = std::process::Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(&ctx.config.cwd)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "not a git repo".into());

    let api_key_status = if ctx.config.api_key.len() >= 4 {
        format!("set ({}...)", &ctx.config.api_key[..4])
    } else if !ctx.config.api_key.is_empty() {
        "set".into()
    } else {
        "not set".into()
    };

    let text = format!(
        "OxideClaw v{ver}\n\
         \n\
         Model:      {model}\n\
         Max tokens: {max_tok}\n\
         CWD:        {cwd}\n\
         Git branch: {branch}\n\
         API key:    {api}\n\
         Vim mode:   {vim}\n\
         Auto-compact: {compact}",
        ver = env!("CARGO_PKG_VERSION"),
        model = ctx.config.model,
        max_tok = ctx.config.max_tokens,
        branch = git_branch,
        api = api_key_status,
        vim = if ctx.vim_mode { "on" } else { "off" },
        compact = if ctx.config.auto_compact_enabled {
            "on"
        } else {
            "off"
        },
    );
    CommandAction::Message(text)
}

pub(super) fn cmd_cost(ctx: &CommandContext) -> CommandAction {
    if ctx.tokens_in == 0 && ctx.tokens_out == 0 {
        return CommandAction::Message("No tokens used in this session yet.".into());
    }
    let (price_in, price_out) = model_pricing(&ctx.config.model);
    let cost_in = ctx.tokens_in as f64 * price_in / 1_000_000.0;
    let cost_out = ctx.tokens_out as f64 * price_out / 1_000_000.0;
    let total = cost_in + cost_out;

    // Cache pricing: read = 10% of input price, write = 125% of input price (Anthropic prompt cache)
    let cache_read_cost = ctx.cache_read_tokens as f64 * (price_in * 0.10) / 1_000_000.0;
    let cache_write_cost = ctx.cache_write_tokens as f64 * (price_in * 1.25) / 1_000_000.0;
    let cache_hit_pct = if ctx.tokens_in > 0 {
        ctx.cache_read_tokens as f64 * 100.0 / ctx.tokens_in as f64
    } else {
        0.0
    };

    let cache_section = if ctx.cache_read_tokens > 0 || ctx.cache_write_tokens > 0 {
        format!(
            "\n\nPrompt cache\n\
             Cache read:   {} tokens (${:.4}, {:.1}% hit rate)\n\
             Cache write:  {} tokens (${:.4})\n\
             Cache savings: ${:.4}",
            ctx.cache_read_tokens,
            cache_read_cost,
            cache_hit_pct,
            ctx.cache_write_tokens,
            cache_write_cost,
            // savings = what it would have cost at full price minus what was actually charged
            ctx.cache_read_tokens as f64 * price_in / 1_000_000.0 - cache_read_cost,
        )
    } else {
        String::new()
    };

    CommandAction::Message(format!(
        "Session cost estimate\n\
         \n\
         Model:        {model}\n\
         Input tokens: {tin} (${cin:.4})\n\
         Output tokens:{tout} (${cout:.4})\n\
         Total:        ${total:.4}\n\
         \n\
         Prices: ${pin}/1M input, ${pout}/1M output{cache}",
        model = ctx.config.model,
        tin = ctx.tokens_in,
        cin = cost_in,
        tout = ctx.tokens_out,
        cout = cost_out,
        pin = price_in,
        pout = price_out,
        cache = cache_section,
    ))
}

pub(super) fn cmd_context(ctx: &CommandContext) -> CommandAction {
    let limit: u64 = 200_000;
    let used = ctx.tokens_in;
    let pct = (used * 100).checked_div(limit).unwrap_or(0);
    let bar_len = 30usize;
    let filled = (pct as usize * bar_len / 100).min(bar_len);
    let bar: String = "█".repeat(filled) + &"░".repeat(bar_len - filled);

    CommandAction::Message(format!(
        "Context window usage\n\
         \n\
         [{bar}] {pct}%\n\
         Used:      {used} tokens\n\
         Remaining: {rem} tokens\n\
         Limit:     {limit} tokens\n\
         \n\
         Run /compact to free context space.",
        rem = limit.saturating_sub(used),
    ))
}

pub(super) fn cmd_files(ctx: &CommandContext) -> CommandAction {
    let Ok(rd) = std::fs::read_dir(&ctx.config.cwd) else {
        return CommandAction::Message("Could not read current directory.".into());
    };
    let mut entries: Vec<String> = rd
        .filter_map(|e| e.ok())
        .map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
            if is_dir { format!("{name}/") } else { name }
        })
        .filter(|n| !n.starts_with('.'))
        .collect();
    entries.sort();

    if entries.is_empty() {
        return CommandAction::Message("No files in current directory.".into());
    }
    CommandAction::Message(format!(
        "Files in {}\n\n{}",
        ctx.config.cwd.display(),
        entries.join("\n")
    ))
}

pub(super) fn cmd_doctor(ctx: &CommandContext) -> CommandAction {
    use crate::distro::{Distro, build_install_command, find_missing};

    let distro = Distro::detect();
    let mut checks: Vec<String> = Vec::new();

    checks.push(format!("System: {}", distro.name()));
    checks.push(format!("OxideClaw v{}", env!("CARGO_PKG_VERSION")));
    checks.push(String::new());

    // XDG / config directory
    let config_dir = crate::config::Config::claude_dir();
    let data_dir = crate::config::Config::data_dir();
    checks.push(format!("✓ Config dir: {}", config_dir.display()));
    if data_dir != config_dir {
        checks.push(format!("✓ Data dir: {}", data_dir.display()));
    }

    // API key
    if ctx.config.api_key.len() >= 4 {
        let src = ctx
            .config
            .auth_source
            .as_deref()
            .unwrap_or("apiKeyHelper / file descriptor");
        let cred = if ctx.config.auth_is_oauth {
            crate::auth::Credential::OAuth(ctx.config.api_key.clone())
        } else {
            crate::auth::Credential::ApiKey(ctx.config.api_key.clone())
        };
        checks.push(format!(
            "✓ Anthropic credential: {} via {src}",
            cred.redacted()
        ));
        // Surface the "stale env var shadows your profile" trap, which is
        // otherwise invisible and sends requests to the wrong org/workspace.
        for w in &ctx.config.auth_warnings {
            checks.push(format!("⚠ {w}"));
        }
    } else {
        checks.push("✗ No Anthropic credential — run /login, or set ANTHROPIC_API_KEY".into());
    }

    // cwd / git / config
    if ctx.config.cwd.exists() {
        checks.push(format!("✓ Working directory: {}", ctx.config.cwd.display()));
    } else {
        checks.push(format!(
            "✗ Working directory missing: {}",
            ctx.config.cwd.display()
        ));
    }
    let is_git = std::process::Command::new("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .current_dir(&ctx.config.cwd)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if is_git {
        checks.push("✓ Git repository".into());
    }

    if ctx.config.cwd.join("CLAUDE.md").exists() {
        checks.push("✓ CLAUDE.md present".into());
    } else {
        checks.push("  No CLAUDE.md — run /init to create one".into());
    }
    if ctx.config.cwd.join("AGENTS.md").exists() {
        checks.push("✓ AGENTS.md present".into());
    }
    checks.push(format!("✓ Model: {}", ctx.config.model));

    // .env
    let env_paths = [
        ctx.config.cwd.join(".env"),
        dirs::home_dir().unwrap_or_default().join(".env"),
        crate::config::app_dir(&dirs::home_dir().unwrap_or_default().join(".config")).join(".env"),
    ];
    for p in env_paths.iter().filter(|p| p.exists()) {
        checks.push(format!("✓ .env loaded: {}", p.display()));
    }

    // Node.js / plugins
    let node_ok = std::process::Command::new("node")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if node_ok {
        let v = std::process::Command::new("node")
            .arg("--version")
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .unwrap_or_default();
        checks.push(format!("✓ Node.js {} (plugins supported)", v.trim()));
    } else {
        checks.push("  ✗ Node.js not found — plugins need Node.js".to_string());
        checks.push(format!("      {}", install_one_liner(&distro, "nodejs")));
    }

    // MCP
    if !ctx.mcp_statuses.is_empty() {
        let tc: usize = ctx.mcp_statuses.iter().map(|s| s.tool_count).sum();
        checks.push(format!(
            "✓ {} MCP server(s) ({} tools)",
            ctx.mcp_statuses.len(),
            tc
        ));
    }

    // Whisper
    let whisper_ok = std::process::Command::new("whisper")
        .args(["--help"])
        .output()
        .map(|o| o.status.success() || o.status.code() == Some(1))
        .unwrap_or(false);
    let openai_key = std::env::var("OPENAI_API_KEY")
        .or_else(|_| std::env::var("WHISPER_API_KEY"))
        .is_ok();
    if whisper_ok {
        checks.push("✓ whisper (offline STT transcription)".into());
    } else if openai_key {
        checks.push("✓ OPENAI_API_KEY set (cloud Whisper transcription)".into());
    } else {
        checks.push("  ✗ No speech-to-text — for /voice input:".into());
        match distro {
            crate::distro::Distro::Arch => {
                checks.push("      Offline: pipx install openai-whisper  (NOT bare pip)".into());
            }
            _ => {
                checks.push("      Offline: pipx install openai-whisper".into());
            }
        }
        checks.push("      Cloud:   add OPENAI_API_KEY=sk-... to ~/.env".into());
    }

    // TTS engine — XTTS v2
    if crate::voice::xtts_available() {
        checks.push("✓ XTTS v2 — natural neural TTS (primary engine)".into());
        if let Some(clone_path) = crate::voice::voice_clone_sample_path() {
            if clone_path.exists() {
                checks.push(format!("✓ Voice clone: {}", clone_path.display()));
            } else {
                checks.push(format!(
                    "  Using default speaker ({}) — /voice clone to set a custom voice",
                    crate::voice::XTTS_DEFAULT_SPEAKER
                ));
            }
        }
    } else {
        checks.push("  ✗ XTTS v2 not found — required for TTS".into());
        checks.push("      uv tool install TTS --python 3.11 \\".into());
        checks.push(
            "        --with 'transformers<4.46' --with 'torch<2.6' --with 'torchaudio<2.6'".into(),
        );
    }

    // System tool check
    checks.push(String::new());
    checks.push("── System tools ──────────────────────────────".into());

    let missing = find_missing(&distro);
    if missing.is_empty() {
        checks.push("✓ All system tools present".into());
    } else {
        for m in &missing {
            if let Some(pkg) = m.package {
                checks.push(format!(
                    "  ✗ {} — {}",
                    m.tool.binary(),
                    m.tool.description()
                ));
                checks.push(format!("      {}", install_one_liner(&distro, pkg)));
            } else if let Some(note) = &m.manual_note {
                checks.push(format!(
                    "  ✗ {} — {}",
                    m.tool.binary(),
                    m.tool.description()
                ));
                checks.push(format!("      {note}"));
            }
        }

        // Consolidated fix command
        if let Some(cmd) = build_install_command(&missing, &distro) {
            checks.push(String::new());
            checks.push("── Install all missing system packages ────────".into());
            checks.push(format!("  {cmd}"));
            checks.push(String::new());
            checks.push("  Or run /install-missing to install automatically.".into());
        }
    }

    CommandAction::Message(format!("Diagnostics\n\n{}", checks.join("\n")))
}

pub(super) fn install_one_liner(distro: &crate::distro::Distro, pkg: &str) -> String {
    format!("{} {pkg}", crate::distro::install_prefix(distro))
}

pub(super) fn cmd_install_missing() -> CommandAction {
    use crate::distro::{Distro, build_install_command, find_missing};

    let distro = Distro::detect();
    let missing = find_missing(&distro);

    if missing.is_empty() {
        return CommandAction::Message(
            "✓ All system tools are already installed. Nothing to do.".into(),
        );
    }

    match build_install_command(&missing, &distro) {
        Some(cmd) => CommandAction::RunInstall(cmd),
        None => {
            // Only pip/manual installs missing — no package manager command to run
            let notes: Vec<String> = missing
                .iter()
                .filter_map(|m| m.manual_note.as_ref().map(|n| format!("  • {n}")))
                .collect();
            CommandAction::Message(format!(
                "Missing tools require manual installation:\n\n{}\n\nNo package manager command needed.",
                notes.join("\n")
            ))
        }
    }
}

pub(super) fn cmd_add_dir(args: &str, ctx: &CommandContext) -> CommandAction {
    let dir = args.trim();
    if dir.is_empty() {
        return CommandAction::Message(
            "Usage: /add-dir <path>\nAdds a directory to your context for Claude to reference."
                .into(),
        );
    }
    let path = std::path::Path::new(dir);
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        ctx.config.cwd.join(path)
    };
    if !path.exists() {
        return CommandAction::Message(format!("Directory not found: {}", path.display()));
    }
    if !path.is_dir() {
        return CommandAction::Message(format!("Not a directory: {}", path.display()));
    }
    // We communicate the added dir by sending a system message to Claude
    CommandAction::SendPrompt(format!(
        "<context>\nThe user has added the directory {} to the conversation context. \
         When working with files, also consider files in this directory.\n</context>",
        path.display()
    ))
}

pub(super) fn cmd_usage(ctx: &CommandContext) -> CommandAction {
    // Usage = cost information for this session
    cmd_cost(ctx)
}

pub(super) fn cmd_insights(ctx: &CommandContext) -> CommandAction {
    let _ = ctx;
    CommandAction::SendPrompt(
        "Please analyze this codebase and provide insights on: \
         1. Architecture and design patterns used \
         2. Code quality and potential improvements \
         3. Security considerations \
         4. Performance bottlenecks or opportunities \
         5. Test coverage gaps \
         6. Technical debt items \
         Be specific with file paths and line numbers where relevant."
            .into(),
    )
}

pub(super) fn cmd_env(ctx: &CommandContext) -> CommandAction {
    let mut lines = vec!["Environment\n".to_string()];

    lines.push(format!("CWD:     {}", ctx.config.cwd.display()));
    lines.push(format!("Model:   {}", ctx.config.model));
    lines.push(format!(
        "Shell:   {}",
        std::env::var("SHELL").unwrap_or_else(|_| "unknown".into())
    ));
    lines.push(format!(
        "User:    {}",
        std::env::var("USER")
            .or_else(|_| std::env::var("USERNAME"))
            .unwrap_or_else(|_| "unknown".into())
    ));
    lines.push(format!(
        "HOME:    {}",
        std::env::var("HOME").unwrap_or_else(|_| "unknown".into())
    ));

    if let Ok(path) = std::env::var("PATH") {
        let entries: Vec<&str> = path.split(':').take(5).collect();
        lines.push(format!("PATH:    {} ...", entries.join(":")));
    }

    lines.push(String::new());
    lines.push(format!(
        "ANTHROPIC_API_KEY: {}",
        if ctx.config.api_key.is_empty() {
            "not set".to_string()
        } else {
            format!(
                "{}...",
                &ctx.config.api_key[..4.min(ctx.config.api_key.len())]
            )
        }
    ));

    CommandAction::Message(lines.join("\n"))
}

pub(super) fn cmd_ctx_viz(ctx: &CommandContext) -> CommandAction {
    let limit: u64 = 200_000;
    let used = ctx.tokens_in;
    let pct = (used * 100).checked_div(limit).unwrap_or(0);

    // Build a visual histogram of context usage
    let bar_width = 40usize;
    let filled = (pct as usize * bar_width / 100).min(bar_width);

    let color_bar = if pct >= 90 {
        format!("{}{}", "▓".repeat(filled), "░".repeat(bar_width - filled))
    } else if pct >= 70 {
        format!("{}{}", "▒".repeat(filled), "░".repeat(bar_width - filled))
    } else {
        format!("{}{}", "█".repeat(filled), "░".repeat(bar_width - filled))
    };

    let thresholds = [
        (crate::compact::COMPACT_WARN_TOKENS, "warn"),
        (crate::compact::COMPACT_SNIP_TOKENS, "snip"),
        (crate::compact::COMPACT_SUMMARISE_TOKENS, "summarise"),
    ];

    let mut lines = vec![
        "Context Visualizer\n".to_string(),
        format!("[{color_bar}] {pct}%"),
        format!("{used} / {limit} tokens used"),
        String::new(),
        "Thresholds:".to_string(),
    ];

    for (threshold, label) in &thresholds {
        let t_pct = threshold * 100 / limit;
        let marker_pos = (t_pct as usize * bar_width / 100).min(bar_width);
        lines.push(format!(
            "  {threshold:>7} ({t_pct}%) — auto-{label}  {}",
            " ".repeat(marker_pos) + "^"
        ));
    }

    lines.push(String::new());
    if pct >= 90 {
        lines.push("⚠ Context nearly full — consider /compact".to_string());
    } else if pct >= 70 {
        lines.push("Context usage is elevated — monitor closely.".to_string());
    } else {
        lines.push("Context usage is healthy.".to_string());
    }

    CommandAction::Message(lines.join("\n"))
}

pub(super) fn cmd_stats(ctx: &CommandContext) -> CommandAction {
    // Show usage statistics from session history
    let sessions_dir = dirs::home_dir()
        .map(|h| h.join(".claude").join("sessions"))
        .unwrap_or_else(|| std::path::PathBuf::from(".claude/sessions"));

    let mut total_sessions = 0usize;
    let mut total_tokens_in: u64 = 0;
    let mut total_tokens_out: u64 = 0;

    if sessions_dir.exists()
        && let Ok(entries) = std::fs::read_dir(&sessions_dir)
    {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("meta") {
                total_sessions += 1;
                if let Ok(data) = std::fs::read_to_string(&path)
                    && let Ok(json) = serde_json::from_str::<serde_json::Value>(&data)
                {
                    total_tokens_in += json["tokens_in"].as_u64().unwrap_or(0);
                    total_tokens_out += json["tokens_out"].as_u64().unwrap_or(0);
                }
            }
        }
    }

    let (price_in, price_out) = {
        let m = &ctx.config.model;
        if m.contains("opus-4") {
            (15.0f64, 75.0f64)
        } else if m.contains("sonnet-4") {
            (3.0, 15.0)
        } else if m.contains("haiku") {
            (0.25, 1.25)
        } else {
            (3.0, 15.0)
        }
    };
    let cost = (total_tokens_in as f64 / 1_000_000.0 * price_in)
        + (total_tokens_out as f64 / 1_000_000.0 * price_out);

    let session_in = ctx.tokens_in;
    let session_out = ctx.tokens_out;
    let session_cost = (session_in as f64 / 1_000_000.0 * price_in)
        + (session_out as f64 / 1_000_000.0 * price_out);

    let lines = vec![
        "Usage Statistics\n".to_string(),
        "Current session:".to_string(),
        format!("  Tokens in:  {session_in}"),
        format!("  Tokens out: {session_out}"),
        format!("  Est. cost:  ${session_cost:.4}"),
        String::new(),
        format!("All sessions ({total_sessions} total):"),
        format!("  Tokens in:  {total_tokens_in}"),
        format!("  Tokens out: {total_tokens_out}"),
        format!("  Est. cost:  ${cost:.4}"),
        String::new(),
        format!("Model: {}", ctx.config.model),
        format!("Pricing: ${price_in}/M in, ${price_out}/M out"),
    ];
    CommandAction::Message(lines.join("\n"))
}
