//! Where `mcp add` / `list` / `get` / `remove` keep a server.
//!
//! - `local` (the default): private to you, this project only. A file under
//!   the config dir named after the project's canonical path
//!   (`Settings::local_mcp_path`); it never enters the repo and, being your
//!   own config, loads without `/trust`.
//! - `project`: the repo's `.mcp.json` (and, read-only here, `mcpServers` in
//!   `.claude/settings.json`). Shared and usually committed, so it loads only
//!   in a `/trust`ed project and `add` refuses literal secrets for it (env
//!   values, headers, URL userinfo, and key- or token-named URL query
//!   values and command args).
//! - `user`: `mcpServers` in the config dir's `settings.json`, every project.
//!
//! Load order, later wins: user, project settings, `.mcp.json`, local.

use crate::config::{read_json_object, write_json_atomic};
use crate::mcp::types::McpServerConfig;
use crate::settings::Settings;
use anyhow::{Context, Result};
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

/// Literal secrets in `cfg`, named for the refusal message: env values and
/// headers, URL userinfo, and URL query values or command args whose name
/// says they hold a key or token. A `${NAME}` reference (or
/// `Bearer ${NAME}`) holds no secret: each user's environment fills it in
/// at startup.
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
    match cfg {
        McpServerConfig::Http(h) => keys.extend(url_secrets(&h.url)),
        McpServerConfig::Stdio(s) => {
            let mut args = s.args.iter().peekable();
            while let Some(arg) = args.next() {
                if arg.contains("://") {
                    keys.extend(url_secrets(arg));
                    continue;
                }
                let (name, value) = match arg.split_once('=') {
                    Some((name, value)) => (name, Some(value)),
                    // `--api-key sk-…`: the value is the next arg.
                    None if arg.starts_with('-') => (
                        arg.as_str(),
                        args.peek()
                            .filter(|v| !v.starts_with('-'))
                            .map(|v| v.as_str()),
                    ),
                    None => continue,
                };
                if secretish(name) && value.is_some_and(is_literal) {
                    keys.push(format!("arg {name}"));
                }
            }
        }
    }
    keys
}

/// Userinfo and secret-named query values in what looks like a URL. Parsed
/// by hand, since a `${HOST}` placeholder is no valid host for a URL parser.
fn url_secrets(url: &str) -> Vec<String> {
    let Some((_, rest)) = url.split_once("://") else {
        return Vec::new();
    };
    let rest = rest.split('#').next().unwrap_or_default();
    let (before_query, query) = rest.split_once('?').unwrap_or((rest, ""));
    let authority = before_query.split('/').next().unwrap_or_default();
    let mut found = Vec::new();
    if let Some((userinfo, _)) = authority.rsplit_once('@')
        && is_literal(userinfo)
    {
        found.push("url userinfo".to_string());
    }
    for pair in query.split('&') {
        if let Some((name, value)) = pair.split_once('=')
            && secretish(name)
            && is_literal(value)
        {
            found.push(format!("url query {name}"));
        }
    }
    found
}

/// A value that is neither empty nor built from `${VAR}` references.
fn is_literal(v: &str) -> bool {
    !v.is_empty() && !v.contains("${")
}

/// Whether a flag, variable or query name says its value is a credential:
/// `--api-key`, `GITHUB_TOKEN`, `apiKey`, `sig`. Settings about one, such
/// as `--token-file` or `--auth-mode`, are not.
fn secretish(name: &str) -> bool {
    let name = name.trim_start_matches('-').to_ascii_lowercase();
    let words: Vec<&str> = name
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect();
    let Some(last) = words.last() else {
        return false;
    };
    if matches!(
        *last,
        "file" | "path" | "env" | "mode" | "type" | "url" | "dir" | "name" | "id"
    ) {
        return false;
    }
    words.iter().any(|w| {
        matches!(
            *w,
            "key"
                | "apikey"
                | "token"
                | "secret"
                | "password"
                | "passwd"
                | "pwd"
                | "auth"
                | "authorization"
                | "sig"
                | "signature"
                | "credential"
                | "credentials"
                | "pat"
        ) || [
            "apikey",
            "accesskey",
            "secretkey",
            "privatekey",
            "token",
            "secret",
        ]
        .iter()
        .any(|s| w.ends_with(s) && w.len() > s.len())
    })
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
/// refuses literal secrets (see `literal_secrets`) unless `force`:
/// `.mcp.json` is shared and usually committed, so they would reach everyone
/// with the repo.
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
                 sets (e.g. -e TOKEN='${{TOKEN}}', ?api_key=${{KEY}} or --api-key \
                 '${{KEY}}'), or pass --force to write it anyway.",
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

/// The files that define `name`, with their scope, in `Scope::ALL` order. A
/// file that cannot be read or parsed is skipped, as `list` skips it, so a
/// broken (or hostile) `.mcp.json` does not block removing a local server.
fn files_defining(name: &str, cwd: &Path, config_dir: &Path) -> Vec<(Scope, PathBuf)> {
    Scope::ALL
        .into_iter()
        .flat_map(|scope| {
            scope
                .files(cwd, config_dir)
                .into_iter()
                .map(move |p| (scope, p))
        })
        .filter(|(_, path)| {
            read_json_object(path)
                .is_ok_and(|json| json.get("mcpServers").and_then(|m| m.get(name)).is_some())
        })
        .collect()
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
    let (scope, paths) = match scope {
        Some(s) => (s, s.files(cwd, config_dir)),
        None => {
            let found = files_defining(name, cwd, config_dir);
            let mut scopes: Vec<Scope> = found.iter().map(|(s, _)| *s).collect();
            scopes.dedup();
            match scopes.as_slice() {
                [] => return Ok(None),
                [one] => (*one, found.into_iter().map(|(_, p)| p).collect()),
                many => anyhow::bail!(
                    "'{name}' is defined in the {} scopes; pass --scope to pick one",
                    many.iter()
                        .map(|s| s.as_str())
                        .collect::<Vec<_>>()
                        .join(" and ")
                ),
            }
        }
    };
    let mut removed = false;
    for path in paths {
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
/// scope and file changed; `None` if no scope defines it. A project-scope
/// entry is refused: `.mcp.json` is shared, so the flag would switch the
/// server off (or on) for everyone with the repo.
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
    if target.scope == Scope::Project {
        anyhow::bail!(
            "'{name}' is in the shared {}, so this would change it for everyone with \
             the repo. Edit that file, or add a local server named '{name}' to \
             override it for yourself.",
            target.path.display()
        );
    }
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
    write_json_atomic(&target.path, &serde_json::to_string_pretty(&json)?)
        .with_context(|| format!("failed to write {}", target.path.display()))?;
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
            literal: false,
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
            sse: false,
            literal: false,
        });
        assert!(add("h", http, Scope::Project, repo.path(), home.path(), false).is_err());
        assert!(!mcp_json.exists());

        // So are keys in a URL or in command args.
        let http = |url: &str| {
            McpServerConfig::Http(HttpServerConfig {
                url: url.into(),
                headers: Default::default(),
                disabled: false,
                sse: false,
                literal: false,
            })
        };
        let with_args = |args: &[&str]| {
            McpServerConfig::Stdio(StdioServerConfig {
                command: "npx".into(),
                args: args.iter().map(|a| a.to_string()).collect(),
                env: Default::default(),
                disabled: false,
                literal: false,
            })
        };
        let refused = |cfg: McpServerConfig, what: &str| {
            let err = add("x", cfg, Scope::Project, repo.path(), home.path(), false)
                .unwrap_err()
                .to_string();
            assert!(err.contains(what), "{what}: {err}");
            assert!(!mcp_json.exists());
        };
        refused(http("https://u:pw@mcp.example.test/mcp"), "url userinfo");
        refused(http("https://ghp_x@mcp.example.test/mcp"), "url userinfo");
        refused(
            http("https://mcp.example.test/mcp?transport=sse&api_key=sk-live-1"),
            "url query api_key",
        );
        refused(http("https://${HOST}/mcp?apiKey=abc"), "url query apiKey");
        refused(
            with_args(&["-y", "pkg", "--api-key", "sk-live-1"]),
            "arg --api-key",
        );
        refused(with_args(&["pkg", "--token=lit"]), "arg --token");
        refused(
            with_args(&["pkg", "GITHUB_TOKEN=ghp_x"]),
            "arg GITHUB_TOKEN",
        );
        refused(
            with_args(&["mcp-remote", "https://h.test/mcp?access_token=t"]),
            "url query access_token",
        );

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
        // Settings about a credential, and references, are not secrets.
        for (name, cfg) in [
            (
                "u1",
                http("https://mcp.example.test/mcp?api_key=${KEY}&transport=sse"),
            ),
            ("u2", http("https://${USER}:${PASS}@mcp.example.test/mcp")),
            ("u3", http("https://mcp.example.test/mcp?max_tokens=100")),
            (
                "a1",
                with_args(&[
                    "pkg",
                    "--token-file",
                    "x",
                    "--auth-mode",
                    "oauth",
                    "--api-key",
                    "${KEY}",
                    "--max-tokens",
                    "100",
                    "--debug",
                ]),
            ),
        ] {
            add(name, cfg, Scope::Project, repo.path(), home.path(), false)
                .unwrap_or_else(|e| panic!("{name}: {e}"));
        }

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

    /// Claude Code requires `"type": "http"` on every URL server and drops
    /// the whole `.mcp.json` when one entry lacks it, so a project HTTP
    /// server written without the tag cost teammates every project server.
    #[test]
    fn http_servers_are_written_with_their_transport_type() {
        let home = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let (r, h) = (repo.path(), home.path());
        let http = |url: &str, sse| {
            McpServerConfig::Http(HttpServerConfig {
                url: url.into(),
                headers: Default::default(),
                disabled: false,
                sse,
                literal: false,
            })
        };
        add(
            "api",
            http("https://h.test/mcp", false),
            Scope::Project,
            r,
            h,
            false,
        )
        .unwrap();
        add(
            "old",
            http("https://h.test/sse", true),
            Scope::Project,
            r,
            h,
            false,
        )
        .unwrap();
        add("gh", stdio(&[]), Scope::Project, r, h, false).unwrap();

        let json = read_json_object(&r.join(".mcp.json")).unwrap();
        assert_eq!(json["mcpServers"]["api"]["type"], "http");
        assert_eq!(json["mcpServers"]["old"]["type"], "sse");
        assert!(json["mcpServers"]["gh"].get("type").is_none());
    }

    /// `"type": "sse"` (Claude Code's legacy HTTP+SSE entries) was ignored
    /// on load, so the server was spoken to over Streamable HTTP and failed.
    #[test]
    fn sse_type_in_a_config_file_selects_the_sse_transport() {
        let home = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let (r, h) = (repo.path(), home.path());
        std::fs::write(
            r.join(".mcp.json"),
            r#"{"mcpServers": {
                "atl": {"type": "sse", "url": "https://h.test/v1/sse"},
                "api": {"type": "http", "url": "https://h.test/mcp"},
                "bare": {"url": "https://h.test/mcp"}
            }}"#,
        )
        .unwrap();
        let sse: Vec<(String, bool)> = list(r, h)
            .into_iter()
            .map(|s| match s.config {
                McpServerConfig::Http(c) => (s.name, c.sse),
                other => panic!("expected http: {other:?}"),
            })
            .collect();
        assert_eq!(
            sse,
            vec![
                ("api".into(), false),
                ("atl".into(), true),
                ("bare".into(), false)
            ]
        );
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

    /// Without --scope, `remove` looks through every scope's files. A repo's
    /// `.mcp.json` linked to /dev/zero used to be read until memory ran out,
    /// and an unparseable one failed the removal of a local-only server.
    #[cfg(unix)]
    #[test]
    fn remove_without_scope_skips_a_hostile_mcp_json() {
        let home = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let (r, h) = (repo.path(), home.path());
        let mcp_json = r.join(".mcp.json");
        add("gh", stdio(&[]), Scope::Local, r, h, false).unwrap();
        std::os::unix::fs::symlink("/dev/zero", &mcp_json).unwrap();
        assert_eq!(remove("gh", None, r, h).unwrap(), Some(Scope::Local));
        assert!(list(r, h).is_empty());

        // Named explicitly, the file is refused rather than read.
        let err = remove("gh", Some(Scope::Project), r, h)
            .unwrap_err()
            .to_string();
        assert!(err.contains("not a regular file"), "{err}");
        assert!(mcp_json.is_symlink());
    }

    #[test]
    fn remove_without_scope_skips_an_invalid_mcp_json() {
        let home = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let (r, h) = (repo.path(), home.path());
        add("gh", stdio(&[]), Scope::Local, r, h, false).unwrap();
        std::fs::write(r.join(".mcp.json"), "{ not json").unwrap();
        assert_eq!(remove("gh", None, r, h).unwrap(), Some(Scope::Local));
        assert_eq!(
            std::fs::read_to_string(r.join(".mcp.json")).unwrap(),
            "{ not json"
        );
    }

    /// `.mcp.json` is shared: a `disabled` flag written there would turn a
    /// team server off for everyone with the repo.
    #[test]
    fn disable_leaves_the_shared_project_file_alone() {
        let home = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let (r, h) = (repo.path(), home.path());
        add("team", stdio(&[]), Scope::Project, r, h, false).unwrap();
        let mcp_json = r.join(".mcp.json");
        let before = std::fs::read(&mcp_json).unwrap();

        // Untrusted, project only: the project entry is the only target.
        let err = set_disabled("team", true, r, h).unwrap_err().to_string();
        assert!(
            err.contains("shared") && err.contains("local server"),
            "{err}"
        );
        assert_eq!(std::fs::read(&mcp_json).unwrap(), before);

        // Trusted, the project entry wins over a user one: still refused.
        trust(h, r);
        add("team", stdio(&[]), Scope::User, r, h, false).unwrap();
        assert!(set_disabled("team", true, r, h).is_err());
        assert_eq!(std::fs::read(&mcp_json).unwrap(), before);

        // A local entry of the same name wins, and is the user's own.
        add("team", stdio(&[]), Scope::Local, r, h, false).unwrap();
        let (scope, _) = set_disabled("team", true, r, h).unwrap().unwrap();
        assert_eq!(scope, Scope::Local);
        assert_eq!(std::fs::read(&mcp_json).unwrap(), before);
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
