//! Moving off Claude Code's `~/.claude`.
//!
//! Older OxideClaw versions kept their settings, sessions, memory and plugin
//! registry in `~/.claude`, the directory Claude Code owns, and wrote into
//! Claude Code's own `settings.json`. OxideClaw now lives in its own XDG
//! directories (`Config::config_dir`, `Config::data_dir`). On the first run
//! of such a version, [`migrate`] copies OxideClaw's state out of
//! `~/.claude` once; [`import_claude`] (`oxideclaw config import-claude`)
//! copies the settings that run code or change permissions, but only when
//! asked. Neither ever writes, moves or deletes anything in `~/.claude`.

use crate::config::{read_json_object, write_json_atomic};
use serde_json::{Map, Value};
use std::path::Path;

/// Settings copied from `~/.claude/settings.json` on first run: preferences
/// that run nothing, send nothing anywhere new and loosen nothing, plus
/// `trustedProjects`, OxideClaw's own `/trust` list. `env` is copied too,
/// filtered to [`crate::SAFE_ENV_KEYS`].
const PREFERENCE_KEYS: &[&str] = &[
    "model",
    "trustedProjects",
    "maxTokens",
    "maxTokensByModel",
    "autoCompact",
    "verbose",
    "thinkingBudgetTokens",
    "showThinkingSummaries",
    "promptCache",
    "effort",
    "cleanupPeriodDays",
    "includeCoAuthoredBy",
    "outputStyle",
    "theme",
    "voiceEnabled",
    "ttsEnabled",
    "ttsVoiceModel",
    "notificationsEnabled",
    "spinnerStyle",
    "updateCheck",
    "routerEnabled",
    "routerBudget",
    "routerLowModel",
    "routerMediumModel",
    "routerHighModel",
    "routerSuperHighModel",
    "phaseRouter",
    "memoryAutoCapture",
    "autoCommit",
    "browseMaxSteps",
    "browserEnabled",
    "browserHeadless",
    "browserTimeoutMs",
    "disableAllHooks",
    "disableSkillShellExecution",
];

/// Settings OxideClaw understands that run commands, pick where prompts go
/// or change what is allowed. A value that only tightens (see
/// [`tightening_value`]) is copied, since dropping it would turn on what the
/// user had turned off; any other value is left behind, and the first-run
/// summary names it so the user can set it again on purpose.
const LEFT_BEHIND_KEYS: &[&str] = &[
    "autoFixLoop",
    "autoRollback",
    "autonomy",
    "ollamaHost",
    "voiceApiUrl",
    "defaultShell",
    "sandboxEnabled",
    "sandboxMode",
    "sandboxAllowNetwork",
    "allowPrivateNetworkFetch",
    "browseApprovalPatterns",
    "browseDefaultPolicy",
    "browserChromePath",
    "browserCdpEndpoint",
];

/// Marker left in the config dir once the first-run migration has looked at
/// `~/.claude`, holding its summary. Its presence stops a second run.
pub const MARKER: &str = ".claude-import";

/// Whether the first-run migration still has to run: the config dir does
/// not exist yet, or holds nothing but a `.env` (the documented place for
/// API keys, which predates it becoming the config dir).
pub fn needs_migration(config_dir: &Path) -> bool {
    match std::fs::read_dir(config_dir) {
        Ok(entries) => entries.flatten().all(|e| e.file_name() == ".env"),
        Err(_) => !config_dir.exists(),
    }
}

/// Copy OxideClaw's own state out of Claude Code's directory `claude` into
/// `config` and `data`: sessions, memory, the plugin registry, the
/// `/trust` list, the per-project MCP files, the banner label, and from
/// `settings.json` only [`PREFERENCE_KEYS`], allow-listed `env`, the MCP
/// servers of installed plugins, `permissions.deny` and the
/// [`LEFT_BEHIND_KEYS`] whose values only tighten. Hooks, allow rules,
/// `apiKeyHelper` and other MCP servers are listed, not copied. Nothing already present
/// in `config` / `data` is overwritten, and `claude` is only read.
///
/// Returns the summary lines (empty when there was nothing to report) and
/// records them in [`MARKER`].
pub fn migrate(claude: &Path, config: &Path, data: &Path) -> Vec<String> {
    let mut imported: Vec<String> = Vec::new();
    let mut problems: Vec<String> = Vec::new();
    // What was not copied, with the `config import-claude` flag that copies it.
    let mut opt_in: Vec<(String, &str)> = Vec::new();
    let mut left_behind: Vec<String> = Vec::new();

    let plugins = read_json_object(&claude.join("plugins.json")).unwrap_or(Value::Null);
    let plugin_names: Vec<&String> = plugins
        .as_object()
        .map(|o| o.keys().collect())
        .unwrap_or_default();

    // ── settings.json ────────────────────────────────────────────────────────
    match read_json_object(&claude.join("settings.json")) {
        Err(e) => problems.push(format!("settings.json not imported: {e}")),
        Ok(theirs) => {
            let theirs = theirs.as_object().cloned().unwrap_or_default();
            let mut ours = Map::new();
            let mut names: Vec<String> = Vec::new();
            for key in PREFERENCE_KEYS {
                if let Some(v) = theirs.get(*key).filter(|v| !v.is_null()) {
                    // Claude Code's `/model` writes `default` for "no choice";
                    // copied, it would count as one and skip the keyless
                    // start on a local Ollama model. Its other spellings
                    // (`opusplan`, `sonnet[1m]`) resolve on load.
                    if *key == "model"
                        && v.as_str()
                            .is_some_and(|m| crate::commands::settings_model(m).is_none())
                    {
                        continue;
                    }
                    ours.insert(key.to_string(), v.clone());
                    names.push(key.to_string());
                }
            }
            if let Some(env) = theirs.get("env").and_then(Value::as_object) {
                let safe: Map<String, Value> = env
                    .iter()
                    .filter(|(k, v)| crate::SAFE_ENV_KEYS.contains(&k.as_str()) && v.is_string())
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
                if !safe.is_empty() {
                    names.push(format!("env ({})", join_keys(safe.keys())));
                    ours.insert("env".into(), Value::Object(safe));
                }
                let rest: Vec<&String> = env
                    .keys()
                    .filter(|k| !crate::SAFE_ENV_KEYS.contains(&k.as_str()))
                    .collect();
                if !rest.is_empty() {
                    left_behind.push(format!("env ({})", join_keys(rest)));
                }
            }
            if let Some(servers) = theirs.get("mcpServers").and_then(Value::as_object) {
                let (from_plugins, other): (Vec<_>, Vec<_>) = servers
                    .iter()
                    .partition(|(name, _)| plugin_names.contains(name));
                if !from_plugins.is_empty() {
                    names.push(format!(
                        "mcpServers of installed plugins ({})",
                        join_keys(from_plugins.iter().map(|(n, _)| *n))
                    ));
                    ours.insert(
                        "mcpServers".into(),
                        Value::Object(
                            from_plugins
                                .into_iter()
                                .map(|(n, v)| (n.clone(), v.clone()))
                                .collect(),
                        ),
                    );
                }
                if !other.is_empty() {
                    opt_in.push((
                        format!("mcpServers ({})", join_keys(other.iter().map(|(n, _)| *n))),
                        "--mcp",
                    ));
                }
            }
            if theirs.get("hooks").is_some_and(is_set) {
                opt_in.push(("hooks".into(), "--hooks"));
            }
            // Deny rules only restrict, so they come along; allow rules wait
            // for `--permissions`.
            let permissions = theirs.get("permissions").and_then(Value::as_object);
            let rules = |kind: &str| -> Vec<Value> {
                permissions
                    .and_then(|p| p.get(kind))
                    .and_then(Value::as_array)
                    .map(|a| a.iter().filter(|r| r.is_string()).cloned().collect())
                    .unwrap_or_default()
            };
            let deny = rules("deny");
            if !deny.is_empty() {
                names.push(format!("permissions.deny ({} rule(s))", deny.len()));
                ours.insert("permissions".into(), serde_json::json!({ "deny": deny }));
            }
            if !rules("allow").is_empty() {
                opt_in.push(("permissions.allow".into(), "--permissions"));
            }
            if theirs.get("apiKeyHelper").is_some_and(is_set) {
                opt_in.push(("apiKeyHelper".into(), "--api-key-helper"));
            }
            for key in LEFT_BEHIND_KEYS {
                let Some(v) = theirs.get(*key).filter(|v| !v.is_null()) else {
                    continue;
                };
                // `autoRollback` is the old name of `autoFixLoop`.
                let name = if *key == "autoRollback" {
                    "autoFixLoop"
                } else {
                    key
                };
                match tightening_value(key, v, &theirs) {
                    Some(v) if !ours.contains_key(name) => {
                        ours.insert(name.to_string(), v);
                        names.push(name.to_string());
                    }
                    Some(_) => {}
                    None => left_behind.push(key.to_string()),
                }
            }
            // Without its voiceApiUrl, voice falls back to OpenAI's endpoint and
            // sends the recording and WHISPER_API_KEY there. Even "" counts:
            // an empty URL is not a reason to switch providers.
            if theirs.get("voiceApiUrl").is_some_and(|v| !v.is_null())
                && !ours.contains_key("voiceApiUrl")
                && ours.get("voiceEnabled") == Some(&Value::Bool(true))
            {
                ours.remove("voiceEnabled");
                names.retain(|n| n != "voiceEnabled");
                left_behind.push("voiceEnabled (its voiceApiUrl was left behind)".into());
            }
            if !ours.is_empty() {
                let target = config.join("settings.json");
                if target.exists() {
                    problems.push(format!(
                        "{} already exists; settings not imported",
                        target.display()
                    ));
                } else {
                    let text = serde_json::to_string_pretty(&Value::Object(ours))
                        .expect("a JSON map serializes");
                    match write_json_atomic(&target, &text) {
                        Ok(()) => imported.push(format!("settings ({})", names.join(", "))),
                        Err(e) => problems.push(format!("settings.json not imported: {e}")),
                    }
                }
            }
        }
    }

    // ── config.json: only the key OxideClaw writes there ─────────────────────
    if let Ok(theirs) = read_json_object(&claude.join("config.json"))
        && let Some(label) = theirs.get("bannerOrgDisplay").filter(|v| v.is_string())
        && !config.join("config.json").exists()
    {
        let json = serde_json::json!({ "bannerOrgDisplay": label });
        match write_json_atomic(&config.join("config.json"), &json.to_string()) {
            Ok(()) => imported.push("config.json (bannerOrgDisplay)".into()),
            Err(e) => problems.push(format!("config.json not imported: {e}")),
        }
    }

    // ── Sessions: Claude Code keeps files of its own in ~/.claude/sessions ──
    let (src, dst) = (claude.join("sessions"), data.join("sessions"));
    if src.is_dir() && !dst.exists() {
        match copy_sessions(&src, &dst) {
            Ok(0) => {}
            Ok(n) => imported.push(format!("{n} session(s)")),
            Err(e) => problems.push(format!("sessions not imported: {e}")),
        }
    }

    // ── Files and directories that are OxideClaw's alone ─────────────────────
    for (name, dest_dir) in [
        ("memory.md", config),
        ("plugins.json", config),
        ("local-mcp", config),
        // /teleport export wrote here before the config dir moved.
        ("teleport.json", config),
    ] {
        let (src, dst) = (claude.join(name), dest_dir.join(name));
        if !src.exists() || dst.exists() {
            continue;
        }
        let result = if src.is_dir() {
            copy_dir(&src, &dst).map(|n| format!("{name}/ ({n} files)"))
        } else {
            copy_file(&src, &dst).map(|()| name.to_string())
        };
        match result {
            Ok(what) => imported.push(what),
            Err(e) => problems.push(format!("{name} not imported: {e}")),
        }
    }

    let mut lines = Vec::new();
    if !imported.is_empty() || !opt_in.is_empty() || !left_behind.is_empty() || !problems.is_empty()
    {
        lines.push(format!(
            "OxideClaw now keeps its config in {} and sessions in {}. {} belongs to Claude Code: \
             it is read, never changed.",
            config.display(),
            data.display(),
            claude.display()
        ));
    }
    if !imported.is_empty() {
        lines.push(format!(
            "Imported from {}: {}.",
            claude.display(),
            imported.join("; ")
        ));
    }
    if !opt_in.is_empty() {
        let (what, flags): (Vec<String>, Vec<&str>) = opt_in.into_iter().unzip();
        lines.push(format!(
            "Not imported: {}. These run code or change permissions; review them, then run \
             `oxideclaw config import-claude {}` (or only the flags you want) to copy them. \
             Without flags it only lists them.",
            what.join(", "),
            flags.join(" ")
        ));
    }
    if !left_behind.is_empty() {
        lines.push(format!(
            "Left behind (set them again in {} if you want them): {}.",
            config.join("settings.json").display(),
            left_behind.join(", ")
        ));
    }
    lines.extend(problems);

    // Record that the migration ran, so it does not run (or print) again.
    let marker = config.join(MARKER);
    let body = if lines.is_empty() {
        "nothing to import\n".to_string()
    } else {
        lines.join("\n") + "\n"
    };
    if let Err(e) = std::fs::create_dir_all(config).and_then(|()| std::fs::write(&marker, body)) {
        lines.push(format!("could not write {}: {e}", marker.display()));
    }
    lines
}

/// Sessions older versions kept in the config dir (`$XDG_CONFIG_HOME/
/// oxideclaw/sessions` without `$XDG_DATA_HOME`) move to the data dir, once,
/// when it has none yet. Both are OxideClaw's own, so this is a move; a
/// failed rename (another filesystem) falls back to a copy.
pub fn move_sessions_to_data_dir(config: &Path, data: &Path) -> Option<String> {
    let (src, dst) = (config.join("sessions"), data.join("sessions"));
    if config == data || !src.is_dir() || dst.exists() {
        return None;
    }
    let moved = std::fs::create_dir_all(data)
        .and_then(|()| std::fs::rename(&src, &dst))
        .map(|()| {
            // A failed chmod is reported, never a reason to undo the move.
            #[cfg(unix)]
            if let Err(e) = make_tree_private(&dst) {
                tracing::warn!("could not make {} owner-only: {e}", dst.display());
            }
            "moved"
        })
        .or_else(|_| copy_dir_private(&src, &dst).map(|_| "copied"));
    Some(match moved {
        Ok(how) => format!(
            "OxideClaw {how} your sessions to {} (XDG data dir).",
            dst.display()
        ),
        Err(e) => format!("could not move {} to {}: {e}", src.display(), dst.display()),
    })
}

/// What `oxideclaw config import-claude` copies.
#[derive(Debug, Default, Clone, Copy)]
pub struct ImportOptions {
    pub hooks: bool,
    pub permissions: bool,
    pub api_key_helper: bool,
    pub mcp: bool,
}

impl ImportOptions {
    fn any(&self) -> bool {
        self.hooks || self.permissions || self.api_key_helper || self.mcp
    }
}

/// OxideClaw's names for the hook events it runs, by Claude Code's name.
const HOOK_EVENTS: &[(&str, &str)] = &[
    ("PreToolUse", "preToolUse"),
    ("PostToolUse", "postToolUse"),
    ("UserPromptSubmit", "userPromptSubmit"),
    ("Notification", "notification"),
    ("Stop", "stop"),
    ("SessionStart", "sessionStart"),
    ("PreCompact", "preCompact"),
    ("PostCompact", "postCompact"),
];

/// Claude Code's state file (`~/.claude.json`, or `.claude.json` in
/// `$CLAUDE_CONFIG_DIR`) is where `claude mcp add` keeps servers. It also
/// holds per-project history, so it may outgrow the settings cap.
const MAX_CLAUDE_STATE_BYTES: u64 = 64 * 1024 * 1024;

/// The MCP servers `claude mcp add` wrote to Claude Code's state file
/// `state`: user scope (top-level `mcpServers`) and local scope for `cwd`
/// (`projects[<dir>].mcpServers`). A missing file has none.
fn claude_code_mcp_servers(
    state: &Path,
    cwd: &Path,
) -> anyhow::Result<(Map<String, Value>, Map<String, Value>)> {
    let text = crate::settings::read_config_file_capped(state, MAX_CLAUDE_STATE_BYTES)
        .map_err(|e| anyhow::anyhow!("{}: {e}", state.display()))?
        .unwrap_or_default();
    if text.trim().is_empty() {
        return Ok(Default::default());
    }
    let json: Value = serde_json::from_str(&text)
        .map_err(|e| anyhow::anyhow!("{} is not valid JSON ({e})", state.display()))?;
    let servers = |v: Option<&Value>| {
        v.and_then(|v| v.get("mcpServers"))
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default()
    };
    let projects = json.get("projects").and_then(Value::as_object);
    // Keyed by the directory Claude Code was started in, as it spelled it.
    let local = [Some(cwd.to_path_buf()), cwd.canonicalize().ok()]
        .into_iter()
        .flatten()
        .find_map(|dir| projects?.get(dir.to_str()?));
    Ok((servers(Some(&json)), servers(local)))
}

/// `oxideclaw config import-claude`: merge the chosen executable settings
/// from `claude/settings.json` into `config/settings.json`. MCP servers also
/// come from Claude Code's state file `claude_state` (`~/.claude.json`):
/// user-scope ones join `config/settings.json`, and the local-scope ones of
/// `cwd` its local MCP file, so they stay private to that project. Without
/// any option, lists what could be imported and changes nothing. Existing
/// OxideClaw entries win; `claude` is only read, so a config dir that is
/// `claude` itself (an override, or a symlink to it) is refused.
pub fn import_claude(
    claude: &Path,
    claude_state: Option<&Path>,
    cwd: &Path,
    config: &Path,
    opts: ImportOptions,
) -> anyhow::Result<Vec<String>> {
    let src = claude.join("settings.json");
    let dst = config.join("settings.json");
    if crate::config::same_dir(claude, config) || crate::config::same_dir(&src, &dst) {
        anyhow::bail!(
            "the config dir {} is Claude Code's {}; nothing to import, and it is not changed",
            config.display(),
            claude.display()
        );
    }
    let theirs = read_json_object(&src)?;
    let mut ours = read_json_object(&dst)?;
    let mut lines = Vec::new();

    let hooks = theirs.get("hooks").map(convert_hooks).unwrap_or_default();
    let permissions = theirs.get("permissions").and_then(Value::as_object);
    let helper = theirs.get("apiKeyHelper").and_then(Value::as_str);
    // settings.json first: older OxideClaw versions wrote their servers there.
    let mut servers = theirs
        .get("mcpServers")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let mut local_servers = Map::new();
    let state = claude_state.filter(|p| p.is_file());
    if let Some(state) = state {
        let (user, local) = claude_code_mcp_servers(state, cwd)?;
        for (name, cfg) in user {
            servers.entry(name).or_insert(cfg);
        }
        local_servers = local;
    }
    let local_path = crate::settings::Settings::local_mcp_path(config, cwd);
    let read_only = match state {
        Some(state) => format!("{} and {} are", src.display(), state.display()),
        None => format!("{} is", src.display()),
    };

    if !opts.any() {
        match state {
            Some(state) => lines.push(format!("In {} and {}:", src.display(), state.display())),
            None => lines.push(format!("In {}:", src.display())),
        }
        let count: usize = hooks.entries.values().map(Vec::len).sum();
        lines.push(format!("  --hooks           {count} hook(s)"));
        let rules = |k: &str| {
            permissions
                .and_then(|p| p.get(k))
                .and_then(Value::as_array)
                .map_or(0, Vec::len)
        };
        lines.push(format!(
            "  --permissions     {} allow, {} deny, {} ask rule(s)",
            rules("allow"),
            rules("deny"),
            rules("ask")
        ));
        if rules("ask") > 0 {
            lines.push(
                "                    OxideClaw has no ask list: allow rules an ask rule narrows \
                 are not imported, so those calls keep prompting."
                    .to_string(),
            );
        }
        lines.push(format!(
            "  --api-key-helper  {}",
            helper.map_or("none".to_string(), |h| format!("`{h}`"))
        ));
        let mut mcp = if servers.is_empty() {
            "none".to_string()
        } else {
            join_keys(servers.keys())
        };
        if !local_servers.is_empty() {
            mcp.push_str(&format!(
                "; for {} only: {}",
                cwd.display(),
                join_keys(local_servers.keys())
            ));
        }
        lines.push(format!("  --mcp             {mcp}"));
        lines.push(format!(
            "Each runs code or changes permissions. Pass the options you want to copy them \
             into {}; {read_only} not changed.",
            dst.display(),
        ));
        lines.push(
            "`--sessions` imports the current directory's Claude Code sessions; \
             `--sessions --list` shows them first."
                .to_string(),
        );
        return Ok(lines);
    }

    let root = ours
        .as_object_mut()
        .expect("read_json_object returns an object");
    if opts.hooks {
        let target = root
            .entry("hooks")
            .or_insert_with(|| Value::Object(Map::new()));
        if !target.is_object() {
            anyhow::bail!(
                "hooks in {} is not an object; not overwriting it",
                dst.display()
            );
        }
        let target = target.as_object_mut().expect("checked above");
        let mut added = 0;
        for (event, entries) in &hooks.entries {
            let list = target
                .entry(event.clone())
                .or_insert_with(|| Value::Array(Vec::new()));
            let Some(list) = list.as_array_mut() else {
                anyhow::bail!("hooks.{event} in {} is not a list", dst.display());
            };
            for e in entries {
                if !list.contains(e) {
                    list.push(e.clone());
                    added += 1;
                }
            }
        }
        lines.push(format!("hooks: {added} added"));
        if !hooks.skipped.is_empty() {
            lines.push(format!(
                "  not supported by OxideClaw, skipped: {}",
                hooks.skipped.join(", ")
            ));
        }
        let regex: Vec<&str> = hooks
            .entries
            .values()
            .flatten()
            .filter_map(|e| e["matcher"].as_str())
            .filter(|m| *m != "*" && m.contains(['|', '*', '.', '(', '[', '^', '$']))
            .collect();
        if !regex.is_empty() {
            lines.push(format!(
                "  check these matchers: OxideClaw matches one exact tool name, \"*\" or \"\", \
                 not a regex: {}",
                regex.join(", ")
            ));
        }
        // A guard that rewrites the call (say, adding `--dry-run`) instead of
        // denying it would run the original call here.
        let rewriting: Vec<&str> = hooks
            .entries
            .get("preToolUse")
            .into_iter()
            .flatten()
            .filter_map(|e| e["command"].as_str())
            .filter(|c| c.contains("updatedInput"))
            .collect();
        if !rewriting.is_empty() {
            lines.push(format!(
                "  check these preToolUse hooks: OxideClaw does not apply `updatedInput`, \
                 so the call runs unchanged: {}",
                rewriting.join(", ")
            ));
        }
    }
    if opts.permissions {
        let target = root
            .entry("permissions")
            .or_insert_with(|| Value::Object(Map::new()));
        let Some(target) = target.as_object_mut() else {
            anyhow::bail!(
                "permissions in {} is not an object; not overwriting it",
                dst.display()
            );
        };
        let ask: Vec<&str> = permissions
            .and_then(|p| p.get("ask"))
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let mut narrowed: Vec<String> = Vec::new();
        for kind in ["allow", "deny"] {
            let mut rules: Vec<Value> = permissions
                .and_then(|p| p.get(kind))
                .and_then(Value::as_array)
                .map(|a| a.iter().filter(|r| r.is_string()).cloned().collect())
                .unwrap_or_default();
            // Claude Code checks ask before allow; with no ask list here, an
            // allow rule an ask rule narrows would run that call unprompted.
            if kind == "allow" {
                rules.retain(|r| {
                    let r = r.as_str().expect("filtered to strings");
                    let hit = ask
                        .iter()
                        .any(|a| crate::permissions::ask_rule_narrows(a, r));
                    if hit {
                        narrowed.push(r.to_string());
                    }
                    !hit
                });
            }
            let list = target
                .entry(kind)
                .or_insert_with(|| Value::Array(Vec::new()));
            let Some(list) = list.as_array_mut() else {
                anyhow::bail!("permissions.{kind} in {} is not a list", dst.display());
            };
            let mut added = 0;
            for r in rules {
                if !list.contains(&r) {
                    list.push(r);
                    added += 1;
                }
            }
            lines.push(format!("permissions.{kind}: {added} added"));
        }
        if !narrowed.is_empty() {
            lines.push(format!(
                "  permissions.allow: skipped {} rule(s) that Claude Code's ask rules narrow, \
                 so they would run unprompted here: {}",
                narrowed.len(),
                narrowed.join(", ")
            ));
        }
        let other: Vec<&String> = permissions
            .map(|p| p.keys().filter(|k| *k != "allow" && *k != "deny").collect())
            .unwrap_or_default();
        if !other.is_empty() {
            lines.push(format!(
                "  not supported by OxideClaw, skipped: permissions.{}",
                join_keys(other)
            ));
        }
    }
    if opts.api_key_helper {
        match (helper, root.get("apiKeyHelper").and_then(Value::as_str)) {
            (None, _) => lines.push("apiKeyHelper: none to import".into()),
            (Some(h), Some(mine)) if h != mine => {
                lines.push(format!("apiKeyHelper: kept the existing `{mine}`"))
            }
            (Some(h), _) => {
                root.insert("apiKeyHelper".into(), Value::String(h.to_string()));
                lines.push(format!("apiKeyHelper: `{h}`"));
            }
        }
    }
    if opts.mcp {
        let target = root
            .entry("mcpServers")
            .or_insert_with(|| Value::Object(Map::new()));
        let Some(target) = target.as_object_mut() else {
            anyhow::bail!(
                "mcpServers in {} is not an object; not overwriting it",
                dst.display()
            );
        };
        let mut added = Vec::new();
        for (name, cfg) in &servers {
            if !target.contains_key(name) {
                target.insert(name.clone(), cfg.clone());
                added.push(name);
            }
        }
        lines.push(format!(
            "mcpServers: {}",
            if added.is_empty() {
                "none added".to_string()
            } else {
                join_keys(added)
            }
        ));
    }
    // Claude Code's local scope is one project's, often with a token in
    // `env`: it goes to that project's private file, not every project's.
    let mut local_json = None;
    if opts.mcp && !local_servers.is_empty() {
        let mut json = read_json_object(&local_path)?;
        if !json.get("mcpServers").is_some_and(Value::is_object) {
            json["mcpServers"] = Value::Object(Map::new());
        }
        let target = json["mcpServers"].as_object_mut().expect("set above");
        let mut added = Vec::new();
        for (name, cfg) in &local_servers {
            if !target.contains_key(name) {
                target.insert(name.clone(), cfg.clone());
                added.push(name);
            }
        }
        lines.push(format!(
            "mcpServers for {} only: {}",
            cwd.display(),
            if added.is_empty() {
                "none added".to_string()
            } else {
                join_keys(added)
            }
        ));
        local_json = Some(json);
    }
    write_json_atomic(&dst, &serde_json::to_string_pretty(&ours)?)?;
    let mut wrote = dst.display().to_string();
    if let Some(json) = local_json {
        write_json_atomic(&local_path, &serde_json::to_string_pretty(&json)?)?;
        wrote = format!("{wrote} and {}", local_path.display());
    }
    lines.push(format!("Wrote {wrote}; {read_only} not changed."));
    Ok(lines)
}

#[derive(Default)]
struct Hooks {
    /// OxideClaw event name → `{matcher, command}` entries, in order.
    entries: std::collections::BTreeMap<String, Vec<Value>>,
    /// Events or hook types OxideClaw does not run.
    skipped: Vec<String>,
}

/// Claude Code's hooks (`{"PreToolUse": [{"matcher", "hooks": [{"type":
/// "command", "command"}]}]}`) or OxideClaw's own (`{"preToolUse":
/// [{"matcher", "command"}]}`, which older versions wrote to the same file)
/// as OxideClaw entries.
fn convert_hooks(hooks: &Value) -> Hooks {
    let mut out = Hooks::default();
    for (event, groups) in hooks.as_object().into_iter().flatten() {
        let Some(ours) = HOOK_EVENTS
            .iter()
            .find(|(cc, ox)| event == cc || event == ox)
            .map(|(_, ox)| *ox)
        else {
            out.skipped.push(event.clone());
            continue;
        };
        for group in groups.as_array().into_iter().flatten() {
            let matcher = group.get("matcher").and_then(Value::as_str).unwrap_or("");
            let commands: Vec<&str> = match group.get("hooks").and_then(Value::as_array) {
                Some(list) => list
                    .iter()
                    .filter_map(|h| {
                        let ty = h.get("type").and_then(Value::as_str).unwrap_or("command");
                        if ty != "command" {
                            out.skipped.push(format!("{event} {ty} hook"));
                            return None;
                        }
                        h.get("command").and_then(Value::as_str)
                    })
                    .collect(),
                None => group
                    .get("command")
                    .and_then(Value::as_str)
                    .into_iter()
                    .collect(),
            };
            for command in commands {
                out.entries
                    .entry(ours.to_string())
                    .or_default()
                    .push(serde_json::json!({ "matcher": matcher, "command": command }));
            }
        }
    }
    out
}

/// The part of a [`LEFT_BEHIND_KEYS`] setting that only tightens, by the
/// rule `Settings::merge_with_trust` applies to untrusted project files, or
/// `None` when the value loosens something or names an endpoint, binary or
/// command. Dropping a tightening value on upgrade would turn on what the
/// user had turned off.
fn tightening_value(key: &str, v: &Value, all: &Map<String, Value>) -> Option<Value> {
    let tightens = match key {
        "sandboxEnabled" => *v == Value::Bool(true),
        // The mode of a sandbox that is on; the modes are fixed names.
        "sandboxMode" => v.is_string() && all.get("sandboxEnabled") == Some(&Value::Bool(true)),
        "sandboxAllowNetwork" | "allowPrivateNetworkFetch" => *v == Value::Bool(false),
        "autonomy" => v.as_str() == Some("suggest"),
        "browseDefaultPolicy" => v
            .as_str()
            .is_some_and(|p| p.trim().eq_ignore_ascii_case("ask")),
        // Each pattern only adds an approval prompt.
        "browseApprovalPatterns" => v.as_array().is_some_and(|a| a.iter().all(Value::is_string)),
        // Only the switch that turns the loop off: its lint / test commands
        // stay behind.
        "autoFixLoop" | "autoRollback" => {
            let mut off = Map::new();
            match v {
                Value::Bool(false) => {
                    off.insert("enabled".into(), Value::Bool(false));
                }
                Value::Object(o) => {
                    for key in ["enabled", "lsp"] {
                        if o.get(key) == Some(&Value::Bool(false)) {
                            off.insert(key.into(), Value::Bool(false));
                        }
                    }
                    if o.get("trigger")
                        .and_then(Value::as_str)
                        .is_some_and(|t| t.eq_ignore_ascii_case("off"))
                    {
                        off.insert("trigger".into(), "off".into());
                    }
                }
                _ => {}
            }
            return (!off.is_empty()).then_some(Value::Object(off));
        }
        _ => false,
    };
    tightens.then(|| v.clone())
}

fn is_set(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => o.values().any(is_set),
        _ => true,
    }
}

fn join_keys<'a>(keys: impl IntoIterator<Item = &'a String>) -> String {
    keys.into_iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(", ")
}

fn copy_file(src: &Path, dst: &Path) -> std::io::Result<()> {
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::copy(src, dst)?;
    keep_mtime(src, dst);
    Ok(())
}

/// `std::fs::copy` keeps the mode but not the timestamps. The session list
/// and `--continue` order sessions by their transcript's mtime, and
/// `cleanupPeriodDays` prunes by it, so a copy stamped "now" would make
/// every migrated session look like the latest. Best effort: a timestamp is
/// no reason to fail the migration.
fn keep_mtime(src: &Path, dst: &Path) {
    if let Ok(at) = std::fs::metadata(src).and_then(|m| m.modified()) {
        let _ = std::fs::File::options()
            .write(true)
            .open(dst)
            .and_then(|f| f.set_modified(at));
    }
}

/// Copy OxideClaw's sessions (`<id>.meta`, `<id>.jsonl` and the `<id>/`
/// snapshot dir) and nothing else: Claude Code keeps its own files in
/// `~/.claude/sessions` too. Returns how many sessions were copied.
fn copy_sessions(src: &Path, dst: &Path) -> std::io::Result<usize> {
    let mut ids: Vec<String> = Vec::new();
    for entry in std::fs::read_dir(src)? {
        let path = entry?.path();
        if path.extension().is_some_and(|e| e == "meta")
            && path.is_file()
            && let Some(id) = path.file_stem().and_then(|s| s.to_str())
        {
            ids.push(id.to_string());
        }
    }
    for id in &ids {
        for name in [format!("{id}.meta"), format!("{id}.jsonl")] {
            let from = src.join(&name);
            if from.symlink_metadata().is_ok_and(|m| m.is_file()) {
                copy_private(&from, &dst.join(&name))?;
            }
        }
        let snapshots = src.join(id);
        if snapshots.symlink_metadata().is_ok_and(|m| m.is_dir()) {
            copy_dir_private(&snapshots, &dst.join(id))?;
        }
    }
    Ok(ids.len())
}

/// Sessions hold tool output, file contents and secrets. Older versions
/// wrote them with umask defaults (0644 in a 0755 dir); the copy is owner-only
/// (0600 in 0700), as the session module writes them now.
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(dir)
}

/// [`copy_file`] for session files; see [`create_private_dir`].
fn copy_private(src: &Path, dst: &Path) -> std::io::Result<()> {
    if let Some(parent) = dst.parent() {
        create_private_dir(parent)?;
    }
    std::fs::copy(src, dst)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(e) = std::fs::set_permissions(dst, std::fs::Permissions::from_mode(0o600)) {
            let _ = std::fs::remove_file(dst);
            return Err(e);
        }
    }
    keep_mtime(src, dst);
    Ok(())
}

/// [`copy_dir`] for session trees; see [`create_private_dir`].
fn copy_dir_private(src: &Path, dst: &Path) -> std::io::Result<usize> {
    create_private_dir(dst)?;
    let mut n = 0;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let to = dst.join(entry.file_name());
        if ty.is_dir() {
            n += copy_dir_private(&entry.path(), &to)?;
        } else if ty.is_file() && !to.exists() {
            copy_private(&entry.path(), &to)?;
            n += 1;
        }
    }
    Ok(n)
}

/// Give a renamed sessions tree the modes [`copy_dir_private`] would have.
/// Symlinks are left alone, so nothing outside the tree changes mode.
#[cfg(unix)]
fn make_tree_private(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        if ty.is_dir() {
            make_tree_private(&entry.path())?;
        } else if ty.is_file() {
            std::fs::set_permissions(entry.path(), std::fs::Permissions::from_mode(0o600))?;
        }
    }
    Ok(())
}

/// Copy a directory tree, regular files only: symlinks are skipped, so a
/// link inside `~/.claude` cannot pull files from elsewhere into ours.
fn copy_dir(src: &Path, dst: &Path) -> std::io::Result<usize> {
    std::fs::create_dir_all(dst)?;
    let mut n = 0;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let to = dst.join(entry.file_name());
        if ty.is_dir() {
            n += copy_dir(&entry.path(), &to)?;
        } else if ty.is_file() && !to.exists() {
            std::fs::copy(entry.path(), &to)?;
            keep_mtime(&entry.path(), &to);
            n += 1;
        }
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    /// Every file under `dir` with its bytes (and every directory), so a test
    /// can prove nothing in it changed.
    fn snapshot(dir: &Path) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
        let mut out = BTreeMap::new();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(&d).unwrap().flatten() {
                let p = e.path();
                if e.file_type().unwrap().is_dir() {
                    out.insert(p.clone(), None);
                    stack.push(p);
                } else {
                    out.insert(p.clone(), Some(std::fs::read(&p).unwrap()));
                }
            }
        }
        out
    }

    fn write(path: &Path, body: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    fn read(path: &Path) -> Value {
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    /// A ~/.claude shared by Claude Code and an old OxideClaw.
    fn fake_claude(root: &Path) -> PathBuf {
        let claude = root.join("home/.claude");
        write(
            &claude.join("settings.json"),
            r#"{
                "model": "opus",
                "trustedProjects": ["/work/repo"],
                "spinnerStyle": "minimal",
                "env": {"ANTHROPIC_API_KEY": "sk-1", "LD_PRELOAD": "/evil.so"},
                "hooks": {"PreToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": "audit"}]}]},
                "permissions": {"allow": ["Bash(ls)"], "deny": ["Read(./.env)"]},
                "apiKeyHelper": "vault read key",
                "mcpServers": {
                    "ctx-plugin": {"command": "node", "args": ["/p/index.js"]},
                    "github": {"command": "npx", "args": ["gh-mcp"]}
                },
                "ollamaHost": "http://box:11434",
                "statusLine": {"type": "command", "command": "cc-status"}
            }"#,
        );
        write(
            &claude.join("config.json"),
            r#"{"bannerOrgDisplay": "ACME", "primaryApiKey": "sk-secret"}"#,
        );
        write(&claude.join("memory.md"), "- prefers tabs\n");
        write(
            &claude.join("plugins.json"),
            r#"{"ctx-plugin": {"spec": "ctx-plugin", "marketplace": false}}"#,
        );
        write(&claude.join("local-mcp/proj.json"), r#"{"mcpServers": {}}"#);
        write(&claude.join("teleport.json"), r#"{"messages": []}"#);
        // OxideClaw sessions, and Claude Code's own files beside them.
        write(&claude.join("sessions/abc.meta"), r#"{"id": "abc"}"#);
        write(&claude.join("sessions/abc.jsonl"), "{}\n");
        write(&claude.join("sessions/abc/snapshots/turn-1/a.txt"), "old");
        write(&claude.join("sessions/95.json"), r#"{"pid": 95}"#);
        write(&claude.join("sessions/95.abcdef.key"), "secret");
        write(&claude.join("CLAUDE.md"), "claude code rules");
        write(&claude.join("projects/x/transcript.jsonl"), "{}");
        claude
    }

    #[test]
    fn migration_copies_only_oxideclaws_state_and_never_touches_dot_claude() {
        let td = tempfile::tempdir().unwrap();
        let claude = fake_claude(td.path());
        let config = td.path().join("home/.config/oxideclaw");
        let data = td.path().join("home/.local/share/oxideclaw");
        let before = snapshot(&claude);

        assert!(needs_migration(&config));
        let lines = migrate(&claude, &config, &data);
        assert_eq!(
            snapshot(&claude),
            before,
            "~/.claude must be byte-identical"
        );

        let settings = read(&config.join("settings.json"));
        assert_eq!(settings["model"], "opus");
        assert_eq!(settings["trustedProjects"][0], "/work/repo");
        assert_eq!(settings["spinnerStyle"], "minimal");
        assert_eq!(
            settings["env"],
            serde_json::json!({"ANTHROPIC_API_KEY": "sk-1"})
        );
        assert_eq!(
            settings["mcpServers"],
            serde_json::json!({"ctx-plugin": {"command": "node", "args": ["/p/index.js"]}}),
            "only the MCP servers of OxideClaw's own plugins"
        );
        assert_eq!(
            settings["permissions"],
            serde_json::json!({"deny": ["Read(./.env)"]}),
            "deny rules only restrict; allow rules wait for --permissions"
        );
        for key in ["hooks", "apiKeyHelper", "ollamaHost", "statusLine"] {
            assert!(settings.get(key).is_none(), "{key} must not be imported");
        }
        assert_eq!(
            read(&config.join("config.json")),
            serde_json::json!({"bannerOrgDisplay": "ACME"})
        );
        assert_eq!(
            std::fs::read_to_string(config.join("memory.md")).unwrap(),
            "- prefers tabs\n"
        );
        assert!(config.join("plugins.json").is_file());
        assert!(config.join("local-mcp/proj.json").is_file());
        assert!(
            config.join("teleport.json").is_file(),
            "a /teleport export from before the move still imports"
        );
        assert!(
            !config.join("CLAUDE.md").exists(),
            "read in place, not copied"
        );
        let sessions = data.join("sessions");
        assert!(sessions.join("abc.meta").is_file());
        assert!(sessions.join("abc.jsonl").is_file());
        assert!(sessions.join("abc/snapshots/turn-1/a.txt").is_file());
        assert!(
            !sessions.join("95.json").exists(),
            "Claude Code's own session file"
        );
        assert!(!sessions.join("95.abcdef.key").exists());
        assert!(!config.join("sessions").exists() && !data.join("projects").exists());

        let text = lines.join("\n");
        assert!(text.contains("1 session(s)"), "{text}");
        assert!(
            text.contains("hooks, permissions.allow, apiKeyHelper"),
            "{text}"
        );
        assert!(text.contains("mcpServers (github)"), "{text}");
        // The command printed is one that copies them, not the bare listing.
        assert!(
            text.contains(
                "`oxideclaw config import-claude --mcp --hooks --permissions --api-key-helper`"
            ),
            "{text}"
        );
        assert!(
            text.contains("ollamaHost") && text.contains("LD_PRELOAD"),
            "{text}"
        );
        assert!(!text.contains("sk-1"), "values are never printed: {text}");

        // Ran once: the marker stops a second run.
        assert!(config.join(MARKER).is_file());
        assert!(!needs_migration(&config));
    }

    /// Safety settings whose value only tightens survive the upgrade: the
    /// defaults are looser, so dropping them would turn on what the user had
    /// turned off. Values that loosen, or name a command, stay behind.
    #[test]
    fn migration_keeps_settings_that_only_tighten() {
        let td = tempfile::tempdir().unwrap();
        let claude = td.path().join(".claude");
        write(
            &claude.join("settings.json"),
            r#"{
                "trustedProjects": ["/work/repo"],
                "sandboxEnabled": true,
                "sandboxMode": "bwrap",
                "sandboxAllowNetwork": false,
                "allowPrivateNetworkFetch": false,
                "autonomy": "suggest",
                "autoFixLoop": false,
                "browseDefaultPolicy": " Ask ",
                "browseApprovalPatterns": ["checkout"],
                "defaultShell": "/bin/zsh",
                "permissions": {"deny": ["Bash(rm:*)"]}
            }"#,
        );
        let config = td.path().join("config");
        let before = snapshot(&claude);
        let lines = migrate(&claude, &config, &td.path().join("data"));
        assert_eq!(snapshot(&claude), before);

        let path = config.join("settings.json");
        let json = read(&path);
        assert_eq!(json["sandboxEnabled"], true);
        assert_eq!(json["sandboxMode"], "bwrap");
        assert_eq!(json["sandboxAllowNetwork"], false);
        assert_eq!(json["allowPrivateNetworkFetch"], false);
        assert_eq!(json["autonomy"], "suggest");
        assert_eq!(json["autoFixLoop"], serde_json::json!({"enabled": false}));
        assert_eq!(json["browseApprovalPatterns"][0], "checkout");
        assert_eq!(json["permissions"]["deny"][0], "Bash(rm:*)");
        assert!(json.get("defaultShell").is_none());

        // And they take effect once loaded.
        let s = crate::settings::Settings::load_file(&path);
        assert_eq!(s.sandbox_enabled, Some(true));
        assert_eq!(s.autonomy.as_deref(), Some("suggest"));
        assert_eq!(s.auto_fix.as_ref().and_then(|a| a.enabled), Some(false));
        assert_eq!(s.permissions.deny, vec!["Bash(rm:*)".to_string()]);

        let left = lines
            .iter()
            .find(|l| l.starts_with("Left behind"))
            .expect("defaultShell is left behind");
        assert!(left.contains("defaultShell"), "{left}");
        for key in ["sandbox", "autonomy", "autoFixLoop", "browse", "Private"] {
            assert!(!left.contains(key), "{key} was imported: {left}");
        }
    }

    /// Voice without its custom voiceApiUrl would upload recordings and
    /// WHISPER_API_KEY to OpenAI, so voiceEnabled stays behind with it.
    #[test]
    fn voice_is_not_enabled_without_its_endpoint() {
        let td = tempfile::tempdir().unwrap();
        let claude = td.path().join(".claude");
        write(
            &claude.join("settings.json"),
            r#"{"voiceEnabled": true, "voiceApiUrl": "https://api.groq.com/openai/v1/audio/transcriptions", "theme": "dark"}"#,
        );
        let config = td.path().join("config");
        let lines = migrate(&claude, &config, &td.path().join("data"));
        let json = read(&config.join("settings.json"));
        assert!(json.get("voiceEnabled").is_none(), "{json}");
        assert!(json.get("voiceApiUrl").is_none(), "{json}");
        let text = lines.join("\n");
        assert!(!text.contains("settings (theme, voiceEnabled"), "{text}");
        let left = lines
            .iter()
            .find(|l| l.starts_with("Left behind"))
            .unwrap_or_else(|| panic!("{lines:?}"));
        assert!(
            left.contains("voiceApiUrl") && left.contains("voiceEnabled"),
            "{left}"
        );

        // No custom endpoint (absent or null): voice keeps the default and
        // stays on; a null URL is not reported as left behind.
        for extra in ["", r#", "voiceApiUrl": null"#] {
            let td = tempfile::tempdir().unwrap();
            let claude = td.path().join(".claude");
            write(
                &claude.join("settings.json"),
                &format!(r#"{{"voiceEnabled": true{extra}}}"#),
            );
            let config = td.path().join("config");
            let lines = migrate(&claude, &config, &td.path().join("data"));
            assert_eq!(read(&config.join("settings.json"))["voiceEnabled"], true);
            assert!(
                !lines.iter().any(|l| l.starts_with("Left behind")),
                "{lines:?}"
            );
        }
    }

    /// Values that loosen, and the commands in an auto-fix block, are left
    /// behind; a loop turned off by `trigger` under the old `autoRollback`
    /// name keeps only that switch.
    #[test]
    fn migration_leaves_loosening_values_behind() {
        let td = tempfile::tempdir().unwrap();
        let claude = td.path().join(".claude");
        write(
            &claude.join("settings.json"),
            r#"{
                "sandboxEnabled": false,
                "sandboxMode": "firejail",
                "sandboxAllowNetwork": true,
                "allowPrivateNetworkFetch": true,
                "autonomy": "full-auto",
                "browseDefaultPolicy": "pattern",
                "autoRollback": {"trigger": "OFF", "lintCommand": "make lint"}
            }"#,
        );
        let config = td.path().join("config");
        let lines = migrate(&claude, &config, &td.path().join("data"));
        let json = read(&config.join("settings.json"));
        assert_eq!(
            json,
            serde_json::json!({"autoFixLoop": {"trigger": "off"}}),
            "only the switch that turns the loop off"
        );
        let text = lines.join("\n");
        for key in [
            "sandboxEnabled",
            "sandboxMode",
            "sandboxAllowNetwork",
            "allowPrivateNetworkFetch",
            "autonomy",
            "browseDefaultPolicy",
        ] {
            assert!(text.contains(key), "{key} named as left behind: {text}");
        }
    }

    /// Turning the language-server step off only tightens; its timings do not.
    #[test]
    fn an_auto_fix_lsp_opt_out_is_kept() {
        let v = serde_json::json!({"lsp": false, "lspTimeoutMs": 500});
        assert_eq!(
            tightening_value("autoFixLoop", &v, &Map::new()),
            Some(serde_json::json!({"lsp": false}))
        );
    }

    #[test]
    fn migration_never_overwrites_what_is_already_there() {
        let td = tempfile::tempdir().unwrap();
        let claude = fake_claude(td.path());
        let config = td.path().join("config");
        let data = td.path().join("data");
        write(&data.join("sessions/new.meta"), r#"{"id": "new"}"#);
        write(&config.join("memory.md"), "mine");
        migrate(&claude, &config, &data);
        assert_eq!(
            std::fs::read_to_string(config.join("memory.md")).unwrap(),
            "mine"
        );
        assert!(!data.join("sessions/abc.meta").exists());
    }

    #[test]
    fn a_config_dir_holding_only_a_dotenv_is_still_fresh() {
        let td = tempfile::tempdir().unwrap();
        let config = td.path().join("oxideclaw");
        assert!(needs_migration(&config));
        write(&config.join(".env"), "OPENAI_API_KEY=x\n");
        assert!(needs_migration(&config));
        write(&config.join("settings.json"), "{}");
        assert!(!needs_migration(&config));
    }

    /// Nothing of OxideClaw's in ~/.claude: the marker is written so the
    /// check is not repeated, and nothing is printed.
    #[test]
    fn an_empty_dot_claude_imports_nothing_quietly() {
        let td = tempfile::tempdir().unwrap();
        let claude = td.path().join(".claude");
        std::fs::create_dir(&claude).unwrap();
        let config = td.path().join("config");
        assert!(migrate(&claude, &config, &td.path().join("data")).is_empty());
        assert!(!needs_migration(&config));
        assert!(!config.join("settings.json").exists());
    }

    /// Writes after the migration (here: /plugin install's registration and
    /// /model's save) go to the new settings file, beside what was imported.
    #[test]
    fn later_writes_land_in_the_new_dir() {
        let td = tempfile::tempdir().unwrap();
        let claude = fake_claude(td.path());
        let config = td.path().join("config");
        migrate(&claude, &config, &td.path().join("data"));
        let before = snapshot(&claude);
        let path = config.join("settings.json");
        let mut json = read_json_object(&path).unwrap();
        json["model"] = "sonnet".into();
        write_json_atomic(&path, &json.to_string()).unwrap();
        assert_eq!(read(&path)["model"], "sonnet");
        assert_eq!(read(&path)["trustedProjects"][0], "/work/repo");
        assert_eq!(snapshot(&claude), before);
    }

    /// A config dir that is ~/.claude itself (an override, or a symlink to
    /// it) would mean rewriting Claude Code's own settings.json: refused.
    #[test]
    fn import_claude_refuses_when_the_config_dir_is_dot_claude() {
        let td = tempfile::tempdir().unwrap();
        let claude = fake_claude(td.path());
        let all = ImportOptions {
            hooks: true,
            permissions: true,
            api_key_helper: true,
            mcp: true,
        };
        let before = snapshot(&claude);
        let err = import_claude(&claude, None, &claude, &claude, all).unwrap_err();
        assert!(err.to_string().contains("nothing to import"), "{err}");
        #[cfg(unix)]
        {
            let link = td.path().join("oxideclaw-link");
            std::os::unix::fs::symlink(&claude, &link).unwrap();
            assert!(import_claude(&claude, None, &claude, &link, all).is_err());
            assert!(
                import_claude(&claude, None, &claude, &link, ImportOptions::default()).is_err()
            );
        }
        assert_eq!(snapshot(&claude), before, "byte-identical");
    }

    #[test]
    fn import_claude_lists_without_options_and_changes_nothing() {
        let td = tempfile::tempdir().unwrap();
        let claude = fake_claude(td.path());
        let config = td.path().join("config");
        let lines =
            import_claude(&claude, None, &claude, &config, ImportOptions::default()).unwrap();
        let text = lines.join("\n");
        assert!(
            text.contains("1 hook(s)") && text.contains("1 allow, 1 deny"),
            "{text}"
        );
        assert!(
            text.contains("`vault read key`") && text.contains("github"),
            "{text}"
        );
        assert!(!config.exists(), "nothing written");
    }

    #[test]
    fn import_claude_converts_hooks_and_merges_the_rest() {
        let td = tempfile::tempdir().unwrap();
        let claude = fake_claude(td.path());
        let config = td.path().join("config");
        write(
            &config.join("settings.json"),
            r#"{"model": "haiku", "permissions": {"allow": ["Bash(ls)"]},
                "mcpServers": {"github": {"command": "mine"}}, "apiKeyHelper": "my helper"}"#,
        );
        let before = snapshot(&claude);
        let all = ImportOptions {
            hooks: true,
            permissions: true,
            api_key_helper: true,
            mcp: true,
        };
        import_claude(&claude, None, &claude, &config, all).unwrap();
        // Twice: nothing is duplicated.
        let lines = import_claude(&claude, None, &claude, &config, all).unwrap();
        assert_eq!(snapshot(&claude), before);
        assert!(lines.iter().any(|l| l == "hooks: 0 added"), "{lines:?}");

        let s = read(&config.join("settings.json"));
        assert_eq!(
            s["hooks"],
            serde_json::json!({"preToolUse": [{"matcher": "Bash", "command": "audit"}]})
        );
        assert_eq!(s["permissions"]["allow"], serde_json::json!(["Bash(ls)"]));
        assert_eq!(
            s["permissions"]["deny"],
            serde_json::json!(["Read(./.env)"])
        );
        assert_eq!(s["apiKeyHelper"], "my helper", "ours is kept");
        assert_eq!(s["mcpServers"]["github"]["command"], "mine", "ours is kept");
        assert_eq!(s["mcpServers"]["ctx-plugin"]["command"], "node");
        assert_eq!(s["model"], "haiku");
        // The result parses as OxideClaw settings with the hook in force.
        let parsed = crate::settings::Settings::load_file(&config.join("settings.json"));
        assert_eq!(parsed.hooks.unwrap().pre_tool_use[0].command, "audit");
    }

    /// A guard that rewrites the call through `updatedInput` would run the
    /// original call here; the import names it.
    #[test]
    fn import_claude_flags_hooks_that_rewrite_tool_input() {
        let td = tempfile::tempdir().unwrap();
        let claude = td.path().join("claude");
        write(
            &claude.join("settings.json"),
            r#"{"hooks": {"PreToolUse": [{"matcher": "Bash", "hooks": [
                {"type": "command", "command": "jq '{hookSpecificOutput: {updatedInput: .tool_input}}'"},
                {"type": "command", "command": "audit"}
            ]}]}}"#,
        );
        let config = td.path().join("config");
        let opts = ImportOptions {
            hooks: true,
            ..Default::default()
        };
        let lines = import_claude(&claude, None, &claude, &config, opts).unwrap();
        let note = lines
            .iter()
            .find(|l| l.contains("updatedInput"))
            .unwrap_or_else(|| panic!("{lines:?}"));
        assert!(note.contains("jq '") && !note.contains("audit"), "{note}");
    }

    /// `ask: Bash(git push:*)` prompts in Claude Code despite `allow:
    /// Bash(git:*)`; importing the allow rule alone ran pushes unprompted.
    #[test]
    fn import_claude_skips_allow_rules_an_ask_rule_narrows() {
        let td = tempfile::tempdir().unwrap();
        let claude = td.path().join("claude");
        write(
            &claude.join("settings.json"),
            r#"{"permissions": {
                "allow": ["Bash(git:*)", "Read(./src/**)", "mcp__github", "mcp__slack"],
                "ask": ["Bash(git push:*)", "mcp__github__push_files"]
            }}"#,
        );
        let config = td.path().join("config");
        let listing = import_claude(&claude, None, &claude, &config, ImportOptions::default())
            .unwrap()
            .join("\n");
        assert!(listing.contains("4 allow, 0 deny, 2 ask"), "{listing}");
        assert!(listing.contains("no ask list"), "{listing}");

        let opts = ImportOptions {
            permissions: true,
            ..Default::default()
        };
        let lines = import_claude(&claude, None, &claude, &config, opts).unwrap();
        let s = read(&config.join("settings.json"));
        assert_eq!(
            s["permissions"]["allow"],
            serde_json::json!(["Read(./src/**)", "mcp__slack"])
        );
        let note = lines
            .iter()
            .find(|l| l.contains("skipped 2 rule(s)"))
            .unwrap_or_else(|| panic!("{lines:?}"));
        assert!(
            note.contains("Bash(git:*)") && note.contains("mcp__github"),
            "{note}"
        );
    }

    #[test]
    fn claude_code_hooks_and_oxideclaw_hooks_both_convert() {
        let hooks = serde_json::json!({
            "PostToolUse": [{"matcher": "Edit|Write", "hooks": [
                {"type": "command", "command": "fmt"},
                {"type": "prompt", "prompt": "check"}
            ]}],
            "stop": [{"matcher": "", "command": "bell"}],
            "SubagentStop": [{"hooks": [{"type": "command", "command": "x"}]}]
        });
        let h = convert_hooks(&hooks);
        assert_eq!(
            h.entries["postToolUse"],
            vec![serde_json::json!({"matcher": "Edit|Write", "command": "fmt"})]
        );
        assert_eq!(
            h.entries["stop"],
            vec![serde_json::json!({"matcher": "", "command": "bell"})]
        );
        assert!(
            h.skipped.contains(&"SubagentStop".to_string()),
            "{:?}",
            h.skipped
        );
        assert!(
            h.skipped.iter().any(|s| s.contains("prompt")),
            "{:?}",
            h.skipped
        );
    }

    /// Older versions wrote sessions with umask defaults.
    #[cfg(unix)]
    fn make_world_readable(dir: &Path) {
        use std::os::unix::fs::PermissionsExt;
        for entry in walkdir::WalkDir::new(dir) {
            let entry = entry.unwrap();
            let mode = if entry.file_type().is_dir() {
                0o755
            } else {
                0o644
            };
            std::fs::set_permissions(entry.path(), std::fs::Permissions::from_mode(mode)).unwrap();
        }
    }

    #[cfg(unix)]
    fn assert_private_tree(dir: &Path) {
        use std::os::unix::fs::PermissionsExt;
        let mut files = 0;
        for entry in walkdir::WalkDir::new(dir) {
            let entry = entry.unwrap();
            let mode = entry.metadata().unwrap().permissions().mode() & 0o777;
            if entry.file_type().is_dir() {
                assert_eq!(mode, 0o700, "{}", entry.path().display());
            } else {
                assert_eq!(mode, 0o600, "{}", entry.path().display());
                files += 1;
            }
        }
        assert!(files > 0, "nothing under {}", dir.display());
    }

    /// The migrated copy of 0644 sessions in a 0755 dir is owner-only.
    #[cfg(unix)]
    #[test]
    fn migrated_sessions_are_owner_only() {
        let td = tempfile::tempdir().unwrap();
        let claude = fake_claude(td.path());
        make_world_readable(&claude.join("sessions"));
        let data = td.path().join("data");
        migrate(&claude, &td.path().join("config"), &data);
        assert!(data.join("sessions/abc.jsonl").is_file());
        assert_private_tree(&data.join("sessions"));
    }

    #[test]
    fn sessions_in_the_old_xdg_config_dir_move_to_the_data_dir() {
        let td = tempfile::tempdir().unwrap();
        let config = td.path().join("config/oxideclaw");
        let data = td.path().join("data/oxideclaw");
        write(&config.join("sessions/abc.meta"), "{}");
        write(&config.join("sessions/abc/snapshots/turn-1/a.txt"), "old");
        #[cfg(unix)]
        make_world_readable(&config.join("sessions"));
        let line = move_sessions_to_data_dir(&config, &data).unwrap();
        assert!(line.contains("moved"), "{line}");
        assert!(data.join("sessions/abc.meta").is_file());
        assert!(!config.join("sessions").exists());
        #[cfg(unix)]
        assert_private_tree(&data.join("sessions"));
        // Once only; and never onto existing sessions.
        write(&config.join("sessions/later.meta"), "{}");
        assert_eq!(move_sessions_to_data_dir(&config, &data), None);
        assert_eq!(move_sessions_to_data_dir(&data, &data), None);
    }

    /// The session list and `--continue` go by each transcript's mtime: a
    /// migration that stamped every copy "now" put a months-old session
    /// first. The cross-filesystem fallback copies the same way.
    #[test]
    fn migrated_sessions_keep_their_age() {
        let td = tempfile::tempdir().unwrap();
        let claude = fake_claude(td.path());
        let old = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_600_000_000);
        for name in ["abc.jsonl", "abc/snapshots/turn-1/a.txt"] {
            std::fs::File::options()
                .write(true)
                .open(claude.join("sessions").join(name))
                .unwrap()
                .set_modified(old)
                .unwrap();
        }
        let (config, data) = (td.path().join("config"), td.path().join("data"));
        migrate(&claude, &config, &data);
        let mtime = |p: &Path| std::fs::metadata(p).unwrap().modified().unwrap();
        assert_eq!(mtime(&data.join("sessions/abc.jsonl")), old);
        assert_eq!(
            mtime(&data.join("sessions/abc/snapshots/turn-1/a.txt")),
            old
        );

        let other = td.path().join("other");
        copy_dir_private(&data.join("sessions"), &other).unwrap();
        assert_eq!(mtime(&other.join("abc.jsonl")), old);
    }

    /// Claude Code's `/model` writes spellings the API rejects; `default`
    /// is no choice at all and is left out, the rest resolve on load.
    #[test]
    fn claude_code_model_spellings_import_as_models() {
        let td = tempfile::tempdir().unwrap();
        let claude = td.path().join("claude");
        let (config, data) = (td.path().join("config"), td.path().join("data"));
        write(
            &claude.join("settings.json"),
            r#"{"model": "default", "theme": "dark"}"#,
        );
        migrate(&claude, &config, &data);
        let settings = read(&config.join("settings.json"));
        assert!(settings.get("model").is_none(), "{settings}");
        assert_eq!(settings["theme"], "dark");

        for (theirs, ours) in [
            ("opusplan", "claude-opus-5"),
            ("sonnet[1m]", "claude-sonnet-5"),
            (
                "claude-sonnet-4-5-20250929[1M]",
                "claude-sonnet-4-5-20250929",
            ),
        ] {
            let config = td.path().join(format!("config-{ours}"));
            write(
                &claude.join("settings.json"),
                &format!(r#"{{"model": "{theirs}"}}"#),
            );
            migrate(&claude, &config, &data);
            let model = read(&config.join("settings.json"))["model"].clone();
            assert_eq!(
                crate::commands::settings_model(model.as_str().unwrap()).as_deref(),
                Some(ours),
                "{theirs}"
            );
        }
    }

    /// `claude mcp add` writes to `~/.claude.json`, not settings.json: user
    /// scope at the top level, local scope under the project's directory.
    /// The listing showed `--mcp none` and `--mcp` imported nothing.
    #[test]
    fn import_claude_reads_mcp_servers_from_claude_json() {
        let td = tempfile::tempdir().unwrap();
        let claude = td.path().join("claude");
        let project = td.path().join("repo");
        std::fs::create_dir_all(&project).unwrap();
        write(
            &claude.join("settings.json"),
            r#"{"mcpServers": {"github": {"command": "from-settings"}}}"#,
        );
        let state = td.path().join(".claude.json");
        let body = serde_json::json!({
            "numStartups": 3,
            "mcpServers": {
                "github": {"command": "from-state"},
                "fs": {"type": "stdio", "command": "npx", "args": ["fs-mcp"]}
            },
            "projects": {
                project.to_str().unwrap(): {
                    "mcpServers": {"db": {"command": "pg-mcp", "env": {"TOKEN": "t"}}}
                },
                "/elsewhere": {"mcpServers": {"other": {"command": "x"}}}
            }
        });
        write(&state, &body.to_string());
        let before = std::fs::read(&state).unwrap();
        let config = td.path().join("config");

        let listing = import_claude(
            &claude,
            Some(&state),
            &project,
            &config,
            ImportOptions::default(),
        )
        .unwrap()
        .join("\n");
        assert!(listing.contains("fs, github"), "{listing}");
        assert!(listing.contains("only: db"), "{listing}");
        assert!(!listing.contains("other"), "{listing}");
        assert!(!config.exists(), "the listing changes nothing");

        let opts = ImportOptions {
            mcp: true,
            ..Default::default()
        };
        let lines = import_claude(&claude, Some(&state), &project, &config, opts).unwrap();
        let s = read(&config.join("settings.json"));
        assert_eq!(s["mcpServers"]["github"]["command"], "from-settings");
        assert_eq!(s["mcpServers"]["fs"]["command"], "npx");
        assert!(s["mcpServers"].get("db").is_none(), "one project's only");
        assert!(s["mcpServers"].get("other").is_none());
        let local = read(&crate::settings::Settings::local_mcp_path(
            &config, &project,
        ));
        assert_eq!(local["mcpServers"]["db"]["env"]["TOKEN"], "t");
        assert_eq!(std::fs::read(&state).unwrap(), before, "never written");
        let text = lines.join("\n");
        assert!(text.contains(".claude.json are not changed"), "{text}");

        // A missing state file is no servers, not an error.
        let missing = td.path().join("none.json");
        let config = td.path().join("config2");
        import_claude(&claude, Some(&missing), &project, &config, opts).unwrap();
        assert_eq!(
            read(&config.join("settings.json"))["mcpServers"],
            serde_json::json!({"github": {"command": "from-settings"}})
        );
    }
}
