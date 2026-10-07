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
/// or change what is allowed. Never copied; the first-run summary names the
/// ones present so the user can set them again on purpose.
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
/// `settings.json` only [`PREFERENCE_KEYS`], allow-listed `env` and the MCP
/// servers of installed plugins. Hooks, permission rules, `apiKeyHelper`
/// and other MCP servers are listed, not copied. Nothing already present
/// in `config` / `data` is overwritten, and `claude` is only read.
///
/// Returns the summary lines (empty when there was nothing to report) and
/// records them in [`MARKER`].
pub fn migrate(claude: &Path, config: &Path, data: &Path) -> Vec<String> {
    let mut imported: Vec<String> = Vec::new();
    let mut problems: Vec<String> = Vec::new();
    let mut opt_in: Vec<String> = Vec::new();
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
                    opt_in.push(format!(
                        "mcpServers ({})",
                        join_keys(other.iter().map(|(n, _)| *n))
                    ));
                }
            }
            for (key, label) in [
                ("hooks", "hooks"),
                ("permissions", "permissions"),
                ("apiKeyHelper", "apiKeyHelper"),
            ] {
                if theirs.get(key).is_some_and(is_set) {
                    opt_in.push(label.to_string());
                }
            }
            for key in LEFT_BEHIND_KEYS {
                if theirs.contains_key(*key) {
                    left_behind.push(key.to_string());
                }
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
        lines.push(format!(
            "Not imported: {}. These run code or change permissions; review them, then run \
             `oxideclaw config import-claude` to copy them.",
            opt_in.join(", ")
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
        .map(|()| "moved")
        .or_else(|_| copy_dir(&src, &dst).map(|_| "copied"));
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

/// `oxideclaw config import-claude`: merge the chosen executable settings
/// from `claude/settings.json` into `config/settings.json`. Without any
/// option, lists what could be imported and changes nothing. Existing
/// OxideClaw entries win; `claude` is only read.
pub fn import_claude(
    claude: &Path,
    config: &Path,
    opts: ImportOptions,
) -> anyhow::Result<Vec<String>> {
    let src = claude.join("settings.json");
    let theirs = read_json_object(&src)?;
    let dst = config.join("settings.json");
    let mut ours = read_json_object(&dst)?;
    let mut lines = Vec::new();

    let hooks = theirs.get("hooks").map(convert_hooks).unwrap_or_default();
    let permissions = theirs.get("permissions").and_then(Value::as_object);
    let helper = theirs.get("apiKeyHelper").and_then(Value::as_str);
    let servers = theirs.get("mcpServers").and_then(Value::as_object);

    if !opts.any() {
        lines.push(format!("In {}:", src.display()));
        let count: usize = hooks.entries.values().map(Vec::len).sum();
        lines.push(format!("  --hooks           {count} hook(s)"));
        let rules = |k: &str| {
            permissions
                .and_then(|p| p.get(k))
                .and_then(Value::as_array)
                .map_or(0, Vec::len)
        };
        lines.push(format!(
            "  --permissions     {} allow, {} deny rule(s)",
            rules("allow"),
            rules("deny")
        ));
        lines.push(format!(
            "  --api-key-helper  {}",
            helper.map_or("none".to_string(), |h| format!("`{h}`"))
        ));
        lines.push(format!(
            "  --mcp             {}",
            servers
                .filter(|s| !s.is_empty())
                .map_or("none".to_string(), |s| join_keys(s.keys()))
        ));
        lines.push(format!(
            "Each runs code or changes permissions. Pass the options you want to copy them \
             into {}; {} is not changed.",
            dst.display(),
            src.display()
        ));
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
        for kind in ["allow", "deny"] {
            let rules: Vec<Value> = permissions
                .and_then(|p| p.get(kind))
                .and_then(Value::as_array)
                .map(|a| a.iter().filter(|r| r.is_string()).cloned().collect())
                .unwrap_or_default();
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
        for (name, cfg) in servers.into_iter().flatten() {
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
    write_json_atomic(&dst, &serde_json::to_string_pretty(&ours)?)?;
    lines.push(format!(
        "Wrote {}; {} was not changed.",
        dst.display(),
        src.display()
    ));
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
    std::fs::copy(src, dst).map(|_| ())
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
                copy_file(&from, &dst.join(&name))?;
            }
        }
        let snapshots = src.join(id);
        if snapshots.symlink_metadata().is_ok_and(|m| m.is_dir()) {
            copy_dir(&snapshots, &dst.join(id))?;
        }
    }
    Ok(ids.len())
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
        for key in [
            "hooks",
            "permissions",
            "apiKeyHelper",
            "ollamaHost",
            "statusLine",
        ] {
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
        assert!(text.contains("hooks, permissions, apiKeyHelper"), "{text}");
        assert!(text.contains("mcpServers (github)"), "{text}");
        assert!(text.contains("oxideclaw config import-claude"), "{text}");
        assert!(
            text.contains("ollamaHost") && text.contains("LD_PRELOAD"),
            "{text}"
        );
        assert!(!text.contains("sk-1"), "values are never printed: {text}");

        // Ran once: the marker stops a second run.
        assert!(config.join(MARKER).is_file());
        assert!(!needs_migration(&config));
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

    #[test]
    fn import_claude_lists_without_options_and_changes_nothing() {
        let td = tempfile::tempdir().unwrap();
        let claude = fake_claude(td.path());
        let config = td.path().join("config");
        let lines = import_claude(&claude, &config, ImportOptions::default()).unwrap();
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
        import_claude(&claude, &config, all).unwrap();
        // Twice: nothing is duplicated.
        let lines = import_claude(&claude, &config, all).unwrap();
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

    #[test]
    fn sessions_in_the_old_xdg_config_dir_move_to_the_data_dir() {
        let td = tempfile::tempdir().unwrap();
        let config = td.path().join("config/oxideclaw");
        let data = td.path().join("data/oxideclaw");
        write(&config.join("sessions/abc.meta"), "{}");
        let line = move_sessions_to_data_dir(&config, &data).unwrap();
        assert!(line.contains("moved"), "{line}");
        assert!(data.join("sessions/abc.meta").is_file());
        assert!(!config.join("sessions").exists());
        // Once only; and never onto existing sessions.
        write(&config.join("sessions/later.meta"), "{}");
        assert_eq!(move_sessions_to_data_dir(&config, &data), None);
        assert_eq!(move_sessions_to_data_dir(&data, &data), None);
    }
}
