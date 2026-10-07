//! Where `mcp add` / `list` / `get` / `remove` keep a server.
//!
//! - `local` (the default): private to you, this project only. A file under
//!   the config dir named after the project's canonical path
//!   (`Settings::local_mcp_path`); it never enters the repo and, being your
//!   own config, loads without `/trust`.
//! - `project`: the repo's `.mcp.json` (and, read-only here, `mcpServers` in
//!   `.claude/settings.json`). Shared and usually committed, so it loads only
//!   in a `/trust`ed project and `add` refuses literal secrets for it.
//! - `user`: `mcpServers` in the config dir's `settings.json`, every project.
//!
//! Load order, later wins: user, project settings, `.mcp.json`, local.

use crate::config::{read_json_object, write_json_atomic};
use crate::mcp::types::McpServerConfig;
use crate::settings::Settings;
use anyhow::Result;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Local,
    Project,
    User,
}

impl Scope {
    /// Highest precedence first, the order `list` shows them in.
    pub const ALL: [Scope; 3] = [Scope::Local, Scope::Project, Scope::User];

    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "local" => Ok(Self::Local),
            "project" => Ok(Self::Project),
            "user" => Ok(Self::User),
            other => anyhow::bail!("unknown scope '{other}' (expected local, project or user)"),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Project => "project",
            Self::User => "user",
        }
    }

    /// The files holding this scope's servers, in load order (later wins).
    fn files(self, cwd: &Path, config_dir: &Path) -> Vec<PathBuf> {
        match self {
            Self::Local => vec![Settings::local_mcp_path(config_dir, cwd)],
            Self::Project => {
                let global = config_dir.join("settings.json");
                let settings = cwd.join(".claude").join("settings.json");
                // Run from $HOME that file is Claude Code's (or our own
                // global one), not a project's: never read or written here.
                let settings = (!crate::settings::is_user_settings_file(&global, &settings))
                    .then_some(settings);
                settings
                    .into_iter()
                    .chain([cwd.join(".mcp.json")])
                    .collect()
            }
            Self::User => vec![config_dir.join("settings.json")],
        }
    }

    /// The file `mcp add --scope <self>` writes.
    pub fn write_path(self, cwd: &Path, config_dir: &Path) -> PathBuf {
        match self {
            Self::Project => cwd.join(".mcp.json"),
            _ => self
                .files(cwd, config_dir)
                .pop()
                .expect("local and user have one file"),
        }
    }
}

impl std::fmt::Display for Scope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One configured server and where it comes from.
#[derive(Debug, Clone)]
pub struct ScopedServer {
    pub name: String,
    pub scope: Scope,
    pub path: PathBuf,
    pub config: McpServerConfig,
    /// A project server in a project that is not `/trust`ed: not started.
    pub needs_trust: bool,
    /// Another started entry of the same name wins at load.
    pub overridden_by: Option<Scope>,
}

impl ScopedServer {
    /// Whether this is the entry that starts for its name.
    pub fn is_effective(&self) -> bool {
        !self.needs_trust && self.overridden_by.is_none()
    }
}

/// Every server in every scope, local first and by name within a scope.
/// Files that fail to parse contribute nothing; startup reports them.
pub fn list(cwd: &Path, config_dir: &Path) -> Vec<ScopedServer> {
    let trusted =
        Settings::is_trusted(&Settings::load_file(&config_dir.join("settings.json")), cwd);
    // Load order, so a later entry overrides an earlier one of its name.
    let mut loaded: Vec<ScopedServer> = Vec::new();
    for scope in Scope::ALL.into_iter().rev() {
        for path in scope.files(cwd, config_dir) {
            if !path.exists() {
                continue;
            }
            let servers = if path.ends_with("settings.json") {
                Settings::load_file(&path).mcp_servers
            } else {
                Settings::load_mcp_json(&path).mcp_servers
            };
            let mut names: Vec<_> = servers.into_iter().collect();
            names.sort_by(|a, b| a.0.cmp(&b.0));
            for (name, config) in names {
                loaded.push(ScopedServer {
                    name,
                    scope,
                    path: path.clone(),
                    config,
                    needs_trust: scope == Scope::Project && !trusted,
                    overridden_by: None,
                });
            }
        }
    }
    for i in 0..loaded.len() {
        loaded[i].overridden_by = loaded[i + 1..]
            .iter()
            .rev()
            .find(|later| later.name == loaded[i].name && !later.needs_trust)
            .map(|later| later.scope);
    }
    // Stable, so within a scope `.mcp.json` stays after `.claude/settings.json`.
    loaded.sort_by_key(|s| Scope::ALL.iter().position(|&x| x == s.scope));
    loaded
}

/// Literal env values or headers in `cfg`, named for the refusal message. A
/// `${NAME}` reference (or `Bearer ${NAME}`) holds no secret: each user's
/// environment fills it in at startup.
fn literal_secrets(cfg: &McpServerConfig) -> Vec<String> {
    let (kind, map) = match cfg {
        McpServerConfig::Stdio(s) => ("env", &s.env),
        McpServerConfig::Http(h) => ("header", &h.headers),
    };
    let mut keys: Vec<String> = map
        .iter()
        .filter(|(_, v)| !is_reference(v))
        .map(|(k, _)| format!("{kind} {k}"))
        .collect();
    keys.sort();
    keys
}

fn is_reference(v: &str) -> bool {
    let var = match v.trim().split_once(' ') {
        Some((scheme, var)) if scheme.chars().all(|c| c.is_ascii_alphabetic()) => var,
        Some(_) => return false,
        None => v.trim(),
    };
    var.strip_prefix("${")
        .and_then(|v| v.strip_suffix('}'))
        .is_some_and(|name| {
            let mut b = name.bytes();
            b.next()
                .is_some_and(|c| c == b'_' || c.is_ascii_alphabetic())
                && b.all(|c| c == b'_' || c.is_ascii_alphanumeric())
        })
}

/// Write `name` into `scope`'s file and return the file. The project scope
/// refuses literal env values and headers unless `force`: `.mcp.json` is
/// shared and usually committed, so they would reach everyone with the repo.
pub fn add(
    name: &str,
    cfg: McpServerConfig,
    scope: Scope,
    cwd: &Path,
    config_dir: &Path,
    force: bool,
) -> Result<PathBuf> {
    let path = scope.write_path(cwd, config_dir);
    if scope == Scope::Project && !force {
        let secrets = literal_secrets(&cfg);
        if !secrets.is_empty() {
            anyhow::bail!(
                "not writing '{name}' to {}: it has {} and that file is shared with \
                 everyone who has the repo (it is usually committed). Use the default \
                 local scope to keep it private to you, reference a variable each user \
                 sets (e.g. -e TOKEN='${{TOKEN}}'), or pass --force to write it anyway.",
                path.display(),
                secrets.join(", ")
            );
        }
    }
    let mut json = read_json_object(&path)?;
    if !json.get("mcpServers").is_some_and(|v| v.is_object()) {
        json["mcpServers"] = serde_json::json!({});
    }
    json["mcpServers"][name] = serde_json::to_value(&cfg)?;
    // New files are created 0600 and existing modes kept, so a token in
    // `env` is not left world-readable.
    write_json_atomic(&path, &serde_json::to_string_pretty(&json)?)?;
    Ok(path)
}

/// Scopes whose files define `name`.
fn scopes_defining(name: &str, cwd: &Path, config_dir: &Path) -> Result<Vec<Scope>> {
    let mut found = Vec::new();
    for scope in Scope::ALL {
        for path in scope.files(cwd, config_dir) {
            if path.exists()
                && read_json_object(&path)?
                    .get("mcpServers")
                    .and_then(|m| m.get(name))
                    .is_some()
            {
                found.push(scope);
                break;
            }
        }
    }
    Ok(found)
}

/// Remove `name` from `scope`, or with no scope from the one scope that
/// defines it. Returns the scope it was removed from; `None` if not found.
/// A name in several scopes needs an explicit scope: guessing could delete
/// a shared `.mcp.json` entry when the private one was meant.
pub fn remove(
    name: &str,
    scope: Option<Scope>,
    cwd: &Path,
    config_dir: &Path,
) -> Result<Option<Scope>> {
    let scope = match scope {
        Some(s) => s,
        None => match scopes_defining(name, cwd, config_dir)?.as_slice() {
            [] => return Ok(None),
            [one] => *one,
            many => anyhow::bail!(
                "'{name}' is defined in the {} scopes; pass --scope to pick one",
                many.iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(" and ")
            ),
        },
    };
    let mut removed = false;
    for path in scope.files(cwd, config_dir) {
        if !path.exists() {
            continue;
        }
        let mut json = read_json_object(&path)?;
        if let Some(servers) = json.get_mut("mcpServers").and_then(|v| v.as_object_mut())
            && servers.remove(name).is_some()
        {
            write_json_atomic(&path, &serde_json::to_string_pretty(&json)?)?;
            removed = true;
        }
    }
    Ok(removed.then_some(scope))
}

/// Set or clear `disabled` on the entry of `name` that startup uses (the
/// highest-precedence one that loads, else the highest of any). Returns the
/// scope and file changed; `None` if no scope defines it.
pub fn set_disabled(
    name: &str,
    disabled: bool,
    cwd: &Path,
    config_dir: &Path,
) -> Result<Option<(Scope, PathBuf)>> {
    let all: Vec<ScopedServer> = list(cwd, config_dir)
        .into_iter()
        .filter(|s| s.name == name)
        .collect();
    let Some(target) = all
        .iter()
        .find(|s| s.is_effective())
        .or_else(|| all.first())
    else {
        return Ok(None);
    };
    let mut json = read_json_object(&target.path)?;
    let Some(entry) = json
        .get_mut("mcpServers")
        .and_then(|m| m.get_mut(name))
        .and_then(|e| e.as_object_mut())
    else {
        return Ok(None);
    };
    if disabled {
        entry.insert("disabled".into(), serde_json::json!(true));
    } else {
        entry.remove("disabled");
    }
    write_json_atomic(&target.path, &serde_json::to_string_pretty(&json)?)?;
    Ok(Some((target.scope, target.path.clone())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::types::{HttpServerConfig, StdioServerConfig};

    fn stdio(env: &[(&str, &str)]) -> McpServerConfig {
        McpServerConfig::Stdio(StdioServerConfig {
            command: "npx".into(),
            args: vec!["srv".into()],
            env: env
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            disabled: false,
        })
    }

    fn trust(home: &Path, repo: &Path) {
        let json = serde_json::json!({ "trustedProjects": [repo] });
        std::fs::write(home.join("settings.json"), json.to_string()).unwrap();
    }

    fn files_under(dir: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(&d).unwrap().flatten() {
                if e.file_type().unwrap().is_dir() {
                    stack.push(e.path());
                } else {
                    out.push(e.path());
                }
            }
        }
        out
    }

    /// The default scope writes one private file in the user's config dir,
    /// keyed by the canonical project path, and nothing in the repo or the
    /// user's settings.json (which every project reads).
    #[test]
    fn local_scope_writes_only_the_users_per_project_file() {
        let home = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let tok = [("GITHUB_TOKEN", "ghp_secret")];
        let path = add(
            "gh",
            stdio(&tok),
            Scope::Local,
            repo.path(),
            home.path(),
            false,
        )
        .unwrap();

        assert_eq!(files_under(home.path()), vec![path.clone()]);
        assert!(files_under(repo.path()).is_empty());
        assert_eq!(
            path,
            Settings::local_mcp_path(home.path(), &repo.path().canonicalize().unwrap())
        );
        // Another project does not see it.
        let other = tempfile::tempdir().unwrap();
        assert!(list(other.path(), home.path()).is_empty());
    }

    #[test]
    fn project_scope_refuses_literal_secrets_without_force() {
        let home = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let mcp_json = repo.path().join(".mcp.json");

        let err = add(
            "gh",
            stdio(&[("GITHUB_TOKEN", "ghp_secret")]),
            Scope::Project,
            repo.path(),
            home.path(),
            false,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("env GITHUB_TOKEN"), "{err}");
        assert!(err.contains("shared") && err.contains("--force"), "{err}");
        assert!(!mcp_json.exists());

        // Same for an HTTP server's headers.
        let http = McpServerConfig::Http(HttpServerConfig {
            url: "https://mcp.example.test".into(),
            headers: [("Authorization".to_string(), "Bearer abc".to_string())].into(),
            disabled: false,
        });
        assert!(add("h", http, Scope::Project, repo.path(), home.path(), false).is_err());
        assert!(!mcp_json.exists());

        // A reference names the secret without holding it.
        let refs = [
            ("GITHUB_TOKEN", "${GITHUB_TOKEN}"),
            ("AUTH", "Bearer ${TOK}"),
        ];
        let path = add(
            "ref",
            stdio(&refs),
            Scope::Project,
            repo.path(),
            home.path(),
            false,
        )
        .unwrap();
        assert_eq!(path, mcp_json);

        // --force writes it.
        let tok = [("GITHUB_TOKEN", "ghp_secret")];
        add(
            "gh",
            stdio(&tok),
            Scope::Project,
            repo.path(),
            home.path(),
            true,
        )
        .unwrap();
        let text = std::fs::read_to_string(&mcp_json).unwrap();
        assert!(
            text.contains("ghp_secret") && text.contains("\"ref\""),
            "{text}"
        );
        assert!(!repo.path().join(".claude").exists());
    }

    #[test]
    fn reference_detection() {
        for ok in ["${T}", " ${GH_TOKEN} ", "Bearer ${T}", "token ${_X1}"] {
            assert!(is_reference(ok), "{ok}");
        }
        for literal in [
            "ghp_x",
            "${T:-default}",
            "x${T}",
            "Bearer abc",
            "ghp_x ${T}",
            "a b ${T}",
            "${1}",
            "",
        ] {
            assert!(!is_reference(literal), "{literal}");
        }
    }

    /// Each server is listed with its scope; untrusted project servers are
    /// marked, and a name in several scopes shows which entry wins.
    #[test]
    fn list_shows_every_scope_and_what_starts() {
        let home = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let (r, h) = (repo.path(), home.path());
        add("mine", stdio(&[]), Scope::Local, r, h, false).unwrap();
        add("shared", stdio(&[]), Scope::Project, r, h, false).unwrap();
        add("everywhere", stdio(&[]), Scope::User, r, h, false).unwrap();
        add("mine", stdio(&[]), Scope::User, r, h, false).unwrap();
        add("shared", stdio(&[]), Scope::User, r, h, false).unwrap();

        let got = |l: &[ScopedServer]| -> Vec<(String, Scope, bool, Option<Scope>)> {
            l.iter()
                .map(|s| (s.name.clone(), s.scope, s.needs_trust, s.overridden_by))
                .collect()
        };
        let untrusted = list(r, h);
        assert_eq!(
            got(&untrusted),
            vec![
                ("mine".into(), Scope::Local, false, None),
                ("shared".into(), Scope::Project, true, None),
                ("everywhere".into(), Scope::User, false, None),
                ("mine".into(), Scope::User, false, Some(Scope::Local)),
                // The project entry does not start, so the user one does.
                ("shared".into(), Scope::User, false, None),
            ]
        );
        assert_eq!(untrusted[1].path, r.join(".mcp.json"));

        // trustedProjects lives in the same settings.json as user servers.
        let mut s = read_json_object(&h.join("settings.json")).unwrap();
        s["trustedProjects"] = serde_json::json!([r]);
        std::fs::write(h.join("settings.json"), s.to_string()).unwrap();
        let trusted = list(r, h);
        assert!(!trusted[1].needs_trust);
        assert_eq!(trusted[4].overridden_by, Some(Scope::Project));
    }

    /// Local servers are the user's own config and load in any project;
    /// `.mcp.json` and `.claude/settings.json` servers need `/trust`.
    #[test]
    fn loading_gates_only_the_project_scope_on_trust() {
        let home = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let (r, h) = (repo.path(), home.path());
        add("mine", stdio(&[]), Scope::Local, r, h, false).unwrap();
        add("shared", stdio(&[]), Scope::Project, r, h, false).unwrap();
        std::fs::create_dir(r.join(".claude")).unwrap();
        std::fs::write(
            r.join(".claude/settings.json"),
            r#"{"mcpServers": {"team": {"command": "x"}}}"#,
        )
        .unwrap();

        let s = Settings::load_in(h, r);
        assert!(s.mcp_servers.contains_key("mine"));
        assert!(!s.mcp_servers.contains_key("shared"));
        assert!(!s.mcp_servers.contains_key("team"));
        assert_eq!(s.untrusted_project_config, vec!["mcpServers"]);

        trust(h, r);
        let s = Settings::load_in(h, r);
        for name in ["mine", "shared", "team"] {
            assert!(s.mcp_servers.contains_key(name), "{name}");
        }
        let scopes: Vec<_> = list(r, h)
            .iter()
            .map(|s| (s.name.clone(), s.scope))
            .collect();
        assert!(scopes.contains(&("team".into(), Scope::Project)));
    }

    #[test]
    fn remove_needs_a_scope_when_the_name_is_in_several() {
        let home = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let (r, h) = (repo.path(), home.path());
        add("gh", stdio(&[]), Scope::Local, r, h, false).unwrap();
        add("gh", stdio(&[]), Scope::Project, r, h, false).unwrap();

        let err = remove("gh", None, r, h).unwrap_err().to_string();
        assert!(err.contains("local and project"), "{err}");
        assert_eq!(list(r, h).len(), 2, "nothing removed");

        assert_eq!(
            remove("gh", Some(Scope::Project), r, h).unwrap(),
            Some(Scope::Project)
        );
        assert_eq!(remove("gh", None, r, h).unwrap(), Some(Scope::Local));
        assert!(list(r, h).is_empty());
        assert_eq!(remove("gh", None, r, h).unwrap(), None);
    }

    /// `.mcp.json` written by versions whose `--scope local` targeted it, and
    /// `.claude/settings.json` written by `--scope project` before it moved
    /// to `.mcp.json`, are both the project scope and can be removed.
    #[test]
    fn remove_project_covers_both_project_files() {
        let home = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let (r, h) = (repo.path(), home.path());
        std::fs::create_dir(r.join(".claude")).unwrap();
        let old = r#"{"mcpServers": {"gh": {"command": "npx"}}, "model": "m"}"#;
        std::fs::write(r.join(".claude/settings.json"), old).unwrap();
        std::fs::write(
            r.join(".mcp.json"),
            r#"{"mcpServers": {"gh": {"command": "npx"}}}"#,
        )
        .unwrap();

        assert_eq!(remove("gh", None, r, h).unwrap(), Some(Scope::Project));
        assert!(list(r, h).is_empty());
        let left = read_json_object(&r.join(".claude/settings.json")).unwrap();
        assert_eq!(left["model"], "m");
    }

    #[test]
    fn disable_changes_the_entry_that_starts() {
        let home = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let (r, h) = (repo.path(), home.path());
        add("gh", stdio(&[]), Scope::User, r, h, false).unwrap();
        add("gh", stdio(&[]), Scope::Project, r, h, false).unwrap();

        // Untrusted: the user entry is the one that starts.
        let (scope, _) = set_disabled("gh", true, r, h).unwrap().unwrap();
        assert_eq!(scope, Scope::User);
        let l = list(r, h);
        assert!(
            l.iter()
                .any(|s| s.scope == Scope::User && s.config.is_disabled())
        );
        assert!(
            !l.iter()
                .any(|s| s.scope == Scope::Project && s.config.is_disabled())
        );

        assert_eq!(
            set_disabled("gh", false, r, h).unwrap().unwrap().0,
            Scope::User
        );
        assert!(!list(r, h).iter().any(|s| s.config.is_disabled()));
        assert!(set_disabled("nope", true, r, h).unwrap().is_none());
    }

    #[test]
    fn unknown_scope_is_an_error() {
        assert!(Scope::parse("loacl").is_err());
        assert_eq!(Scope::parse("project").unwrap(), Scope::Project);
    }
}
