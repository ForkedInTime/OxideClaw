//! Autonomy modes: how much the permission gate approves on its own.
//!
//! The mode only adjusts what the rules leave open. A `permissions.deny`
//! rule (or an SDK host's deny list) refuses a call in every mode, and an
//! explicit allow rule still allows in every mode but `suggest`.

use serde::{Deserialize, Serialize};
use std::path::{Component, Path};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Autonomy {
    /// Every edit prompts, even one an allow rule or `[a]lways` covers; the
    /// auto-fix loop does not run.
    Suggest,
    /// Edits and commands prompt unless a rule allows them.
    #[default]
    Ask,
    /// Edits inside the project are pre-approved, except to the files in
    /// [`is_protected`]; commands still prompt.
    AutoEdit,
    /// Everything is pre-approved. Only with the bwrap sandbox and its
    /// network off, and never when started from `$HOME` or above it.
    FullAuto,
}

/// What a mode says about one call, before the rules are consulted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Prompt even if a rule allows it (`suggest` on an edit).
    Prompt,
    /// Allow it without a prompt unless a rule denies it.
    PreApproved,
    /// The rules decide.
    Rules,
}

/// Protected paths, relative to the project root, whose contents code
/// outside any sandbox runs later: git's hooks and config (fsmonitor,
/// `core.hooksPath`, filter drivers, submodules'), Mercurial and Sapling
/// config hooks, OxideClaw's and Claude Code's project config (hooks, MCP
/// servers) and the hook managers' config. bwrap binds the ones that exist
/// read-only, so a command `full-auto` pre-approved cannot plant code that
/// runs unsandboxed on the user's next `git commit` or session.
pub(crate) const HOST_RUN_PATHS: &[&str] = &[
    ".git/hooks",
    ".git/config",
    ".git/config.worktree",
    ".git/modules",
    ".hg/hgrc",
    ".sl/config",
    ".claude",
    ".oxideclaw",
    ".agents",
    ".husky",
    ".githooks",
    ".mcp.json",
    ".pre-commit-config.yaml",
    "lefthook.yml",
    "lefthook.yaml",
    ".lefthook.yml",
    ".lefthook.yaml",
];

/// The tools that write files.
pub const EDIT_TOOLS: &[&str] = &["Write", "Edit", "MultiEdit", "NotebookEdit"];

/// Directories whose contents a pre-approved edit must not touch: VCS and
/// agent state, CI, git hook managers, and what auto-fix's runners execute
/// or load (`.cargo/config.toml` runners, the `.venv` ruff/pytest, the
/// `node_modules` eslint and the packages `npm test` loads). Mercurial,
/// Sapling and Jujutsu keep repo config that runs hooks or commands, like
/// `.git/config`.
const PROTECTED_DIRS: &[&str] = &[
    ".git",
    ".hg",
    ".sl",
    ".jj",
    ".claude",
    ".oxideclaw",
    ".agents",
    ".github",
    ".gitlab",
    ".circleci",
    ".buildkite",
    ".woodpecker",
    ".husky",
    ".githooks",
    ".cargo",
    ".venv",
    "node_modules",
];

/// File names (lowercase) that are hook, CI or build/test-runner config: they
/// change what the auto-fix, git or CI commands run.
const PROTECTED_FILES: &[&str] = &[
    ".mcp.json",
    ".gitlab-ci.yml",
    ".travis.yml",
    ".drone.yml",
    ".woodpecker.yml",
    "azure-pipelines.yml",
    "bitbucket-pipelines.yml",
    "appveyor.yml",
    "jenkinsfile",
    ".pre-commit-config.yaml",
    "lefthook.yml",
    "lefthook.yaml",
    ".lefthook.yml",
    ".lefthook.yaml",
    "package.json",
    ".npmrc",
    "cargo.toml",
    "build.rs",
    "rust-toolchain",
    "rust-toolchain.toml",
    "conftest.py",
    "pytest.ini",
    "setup.py",
    "setup.cfg",
    "pyproject.toml",
    "tox.ini",
    "noxfile.py",
    "makefile",
    "gnumakefile",
    "justfile",
    ".justfile",
];

/// Name prefixes: `.env`, `.env.local`, `.envrc` (direnv runs it), the
/// ESLint configs `npx eslint` executes, and the JavaScript test-runner and
/// transpiler configs `npm test` loads.
const PROTECTED_PREFIXES: &[&str] = &[
    ".env",
    "eslint.config.",
    ".eslintrc",
    "jest.config.",
    "vitest.config.",
    "babel.config.",
    "karma.conf.",
];

/// MSBuild projects and the `.props` / `.targets` files they import.
const PROTECTED_EXTENSIONS: &[&str] = &["csproj", "fsproj", "vbproj", "props", "targets"];

impl Autonomy {
    pub const ALL: [Autonomy; 4] = [Self::Suggest, Self::Ask, Self::AutoEdit, Self::FullAuto];

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "suggest" => Some(Self::Suggest),
            "ask" => Some(Self::Ask),
            "auto-edit" => Some(Self::AutoEdit),
            "full-auto" => Some(Self::FullAuto),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Suggest => "suggest",
            Self::Ask => "ask",
            Self::AutoEdit => "auto-edit",
            Self::FullAuto => "full-auto",
        }
    }

    fn rank(self) -> u8 {
        match self {
            Self::FullAuto => 0,
            Self::AutoEdit => 1,
            Self::Ask => 2,
            Self::Suggest => 3,
        }
    }

    /// Prompts at least wherever `other` does.
    pub fn at_least_as_strict_as(self, other: Self) -> bool {
        self.rank() >= other.rank()
    }

    /// The mode the gates apply: `full-auto` without a usable,
    /// network-isolated bwrap sandbox (see [`full_auto_blocker`]) is `ask`.
    pub fn effective(self, sandbox_enabled: bool, sandbox_mode: &str, allow_network: bool) -> Self {
        if self == Self::FullAuto
            && full_auto_blocker(sandbox_enabled, sandbox_mode, allow_network).is_some()
        {
            Self::Ask
        } else {
            self
        }
    }

    /// This mode's say on `tool` called with `input` in project `project`.
    pub fn verdict(self, tool: &str, input: &serde_json::Value, project: &Path) -> Verdict {
        self.verdict_with_home(tool, input, project, dirs::home_dir().as_deref())
    }

    pub(crate) fn verdict_with_home(
        self,
        tool: &str,
        input: &serde_json::Value,
        project: &Path,
        home: Option<&Path>,
    ) -> Verdict {
        let edit = EDIT_TOOLS.contains(&tool);
        match self {
            Self::Suggest if edit => Verdict::Prompt,
            Self::AutoEdit if edit && edit_preapproved(tool, input, project, home) => {
                Verdict::PreApproved
            }
            // Leaving plan mode is the user's review of the plan, and the
            // browser's loopback question is a consent question for a Chrome
            // outside the sandbox: full-auto answers neither for them.
            Self::FullAuto
                if tool == "ExitPlanMode"
                    || tool == crate::tools::browser_tools::LOOPBACK_QUESTION =>
            {
                Verdict::Rules
            }
            // bwrap confines only shell commands. Edit tools run in-process
            // and MCP servers run unsandboxed, so full-auto gives edits
            // auto-edit's rule (in-project, unprotected) and leaves MCP tools
            // to the rules.
            Self::FullAuto if edit => {
                if edit_preapproved(tool, input, project, home) {
                    Verdict::PreApproved
                } else {
                    Verdict::Rules
                }
            }
            Self::FullAuto if tool.starts_with("mcp__") => Verdict::Rules,
            // Throwing away a worktree's changes is not something the
            // sandbox can undo.
            Self::FullAuto
                if tool == "ExitWorktree"
                    && input
                        .get("discard_changes")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false) =>
            {
                Verdict::Rules
            }
            // The sandbox binds the cwd read-write: from `$HOME` a
            // pre-approved command could rewrite every dotfile, so full-auto
            // pre-approves nothing there, edits or commands.
            Self::FullAuto if project_holds_home(project, home) => Verdict::Rules,
            // Only shell commands run under bwrap. Reads outside the project,
            // WebFetch, the browser and sub-agents run in-process, so they
            // keep the rules (in the SDK and ACP: the host's prompt).
            Self::FullAuto if crate::permissions::is_command_tool(tool) => Verdict::PreApproved,
            _ => Verdict::Rules,
        }
    }
}

impl std::fmt::Display for Autonomy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Why `full-auto` cannot be used with this sandbox setting, or `None` when
/// it can: it needs bwrap, enabled and installed, on Linux, with its network
/// off. firejail does not qualify: its default profile leaves all of `$HOME`
/// writable, where bwrap binds only the project directory read-write. bwrap
/// confines only the filesystem: with the network on, a command injected by
/// a file the model read could post the project's source anywhere, unprompted.
pub fn full_auto_blocker(
    sandbox_enabled: bool,
    sandbox_mode: &str,
    allow_network: bool,
) -> Option<String> {
    if !cfg!(target_os = "linux") {
        return Some(
            "full-auto is unavailable on this platform until a native sandbox ships: it \
             requires bwrap, which exists only on Linux."
                .into(),
        );
    }
    if !sandbox_enabled || sandbox_mode != "bwrap" {
        return Some(
            "full-auto requires the bwrap sandbox, which leaves only the project \
             directory writable (firejail and strict do not): run /sandbox enable bwrap \
             first."
                .into(),
        );
    }
    if allow_network {
        return Some(
            "full-auto requires the sandbox's network off, so an unprompted command cannot \
             send the project anywhere: run /sandbox network off first \
             (sandboxAllowNetwork: false)."
                .into(),
        );
    }
    (!crate::sandbox::bwrap_available())
        .then(|| "full-auto requires the bwrap sandbox, but bwrap is not installed.".into())
}

/// The line saying `mode` pre-approves nothing because the session started
/// in `$HOME` (or above it), or `None` when that does not apply.
pub fn home_notice(mode: Autonomy, project: &Path) -> Option<String> {
    home_notice_with(mode, project, dirs::home_dir().as_deref())
}

fn home_notice_with(mode: Autonomy, project: &Path, home: Option<&Path>) -> Option<String> {
    let mode_preapproves = matches!(mode, Autonomy::AutoEdit | Autonomy::FullAuto);
    (mode_preapproves && project_holds_home(project, home)).then(|| {
        format!(
            "Autonomy \"{mode}\" pre-approves nothing here: started in your home directory \
             (or above it), every edit and command prompts as under \"ask\". Start \
             oxideclaw in a project directory to use it."
        )
    })
}

/// Run from `$HOME` (or a directory above it), "the project" is every
/// dotfile the user has: no edit is pre-approved there.
fn project_holds_home(project: &Path, home: Option<&Path>) -> bool {
    let Some(home) = home else {
        return false;
    };
    let real = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    home.starts_with(project) || real(home).starts_with(real(project))
}

/// The files an edit tool call writes, or `None` when its input names none.
fn edit_paths<'a>(tool: &str, input: &'a serde_json::Value) -> Option<Vec<&'a str>> {
    let field = |v: &'a serde_json::Value, key: &str| v.get(key).and_then(|p| p.as_str());
    let paths: Vec<&str> = match tool {
        "Write" | "Edit" => vec![field(input, "file_path")?],
        "NotebookEdit" => vec![field(input, "notebook_path")?],
        "MultiEdit" => input
            .get("edits")?
            .as_array()?
            .iter()
            .map(|e| field(e, "file_path"))
            .collect::<Option<_>>()?,
        _ => return None,
    };
    (!paths.is_empty()).then_some(paths)
}

/// `auto-edit`'s rule: every file the call writes is inside `project` (after
/// resolving symlinks) and none is protected, and `project` is not `$HOME`.
fn edit_preapproved(
    tool: &str,
    input: &serde_json::Value,
    project: &Path,
    home: Option<&Path>,
) -> bool {
    if project_holds_home(project, home) {
        return false;
    }
    let Some(paths) = edit_paths(tool, input) else {
        return false;
    };
    let real_root = std::fs::canonicalize(project).unwrap_or_else(|_| project.to_path_buf());
    paths
        .iter()
        .all(|p| path_preapproved(p, project, &real_root))
}

fn path_preapproved(file: &str, project: &Path, real_root: &Path) -> bool {
    // The tools' own resolution: `~` expanded, relative paths joined to the
    // project and refused when they climb out of it.
    let Ok(path) = crate::tools::file_read::resolve_path(file, project) else {
        return false;
    };
    let lexical = std::path::PathBuf::from(super::normalize_lexically(&path.to_string_lossy()));
    // A symlink inside the project can point anywhere, `.git/hooks` included.
    let real = crate::tools::resolve_for_sensitivity_check(&lexical);
    let Ok(rel) = real.strip_prefix(real_root) else {
        return false;
    };
    if rel.as_os_str().is_empty() || is_protected(rel) {
        return false;
    }
    lexical
        .strip_prefix(project)
        .map_or(true, |rel| !is_protected(rel))
}

/// Whether `rel` (relative to the project root) is a file `auto-edit` still
/// prompts for. Compared case-insensitively, as macOS and Windows resolve
/// `.GIT/hooks` to `.git/hooks`, and as Windows names them: Win32 drops
/// trailing dots and spaces (`Makefile.` creates `Makefile`), and
/// `name:stream` writes an alternate data stream of `name`. Applied on every
/// platform; elsewhere it only makes a few odd names prompt.
pub fn is_protected(rel: &Path) -> bool {
    let names: Vec<String> = rel
        .components()
        .filter_map(|c| match c {
            Component::Normal(n) => Some(windows_name(&n.to_string_lossy())),
            _ => None,
        })
        .collect();
    let Some(file) = names.last() else {
        return true;
    };
    names.iter().any(|n| PROTECTED_DIRS.contains(&n.as_str()))
        || PROTECTED_FILES.contains(&file.as_str())
        || PROTECTED_PREFIXES.iter().any(|p| file.starts_with(p))
        || Path::new(file)
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| PROTECTED_EXTENSIONS.contains(&e))
}

/// `name` as Windows resolves it, lowercased: any `:stream` suffix cut and
/// trailing dots and spaces dropped.
fn windows_name(name: &str) -> String {
    let name = name.split(':').next().unwrap_or_default();
    name.trim_end_matches(['.', ' ']).to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn write(path: &str) -> serde_json::Value {
        json!({"file_path": path, "content": "x"})
    }

    #[test]
    fn names_round_trip_and_unknown_names_are_refused() {
        for a in Autonomy::ALL {
            assert_eq!(Autonomy::parse(a.as_str()), Some(a));
            assert_eq!(
                serde_json::to_value(a).unwrap(),
                json!(a.as_str()),
                "config serialises the same names"
            );
        }
        assert_eq!(Autonomy::parse(" Full-Auto "), Some(Autonomy::FullAuto));
        assert_eq!(Autonomy::parse("read-only"), None);
        assert_eq!(Autonomy::default(), Autonomy::Ask);
    }

    #[test]
    fn protected_paths_are_recognised_anywhere_in_the_project() {
        // What bwrap keeps read-only is what auto-edit protects.
        for p in HOST_RUN_PATHS {
            assert!(is_protected(Path::new(p)), "{p}");
        }
        for p in [
            ".git/hooks/pre-commit",
            "sub/.git/config",
            ".hg/hgrc",
            ".HG/hgrc",
            ".sl/config",
            ".jj/repo/config.toml",
            ".claude/settings.json",
            ".oxideclaw/x",
            ".agents/skills/a/SKILL.md",
            ".mcp.json",
            ".env",
            ".env.production",
            ".envrc",
            ".github/workflows/ci.yml",
            ".gitlab-ci.yml",
            ".husky/pre-commit",
            ".pre-commit-config.yaml",
            "package.json",
            "web/package.json",
            "Cargo.toml",
            "crates/x/build.rs",
            "conftest.py",
            "tests/conftest.py",
            "pytest.ini",
            "Makefile",
            "justfile",
            "setup.py",
            "pyproject.toml",
            "tox.ini",
            "app/App.csproj",
            "Directory.Build.targets",
            "jest.config.js",
            "web/vitest.config.ts",
            "babel.config.json",
            "karma.conf.js",
            ".cargo/config.toml",
            ".venv/bin/pytest",
            "node_modules/.bin/eslint",
            "web/node_modules/eslint/bin/eslint.js",
            "eslint.config.mjs",
            ".GIT/hooks/pre-push",
            // Win32 drops trailing dots and spaces; `:` names a data stream.
            "tests/conftest.py.",
            "Makefile.",
            "Makefile .",
            ".mcp.json. ",
            ".env.",
            ".git./hooks/pre-commit",
            "package.json:stream",
            "Cargo.toml::$DATA",
        ] {
            assert!(is_protected(Path::new(p)), "{p}");
        }
        for p in [
            "src/main.rs",
            "README.md",
            "docs/github.md",
            "src/environment.rs",
            "tests/test_app.py",
            "Cargo.lock",
            "src/node_modules.rs",
            "notes.txt.",
        ] {
            assert!(!is_protected(Path::new(p)), "{p}");
        }
    }

    #[test]
    fn auto_edit_pre_approves_only_unprotected_edits_inside_the_project() {
        let proj = tempfile::tempdir().unwrap();
        let root = proj.path();
        let home = Some(Path::new("/nonexistent-home"));
        let v = |tool: &str, input: serde_json::Value| {
            Autonomy::AutoEdit.verdict_with_home(tool, &input, root, home)
        };
        assert_eq!(v("Write", write("src/a.rs")), Verdict::PreApproved);
        let abs = root.join("src/b.rs").to_string_lossy().into_owned();
        assert_eq!(v("Edit", write(&abs)), Verdict::PreApproved);
        assert_eq!(
            v("NotebookEdit", json!({"notebook_path": "nb.ipynb"})),
            Verdict::PreApproved
        );
        assert_eq!(
            v(
                "MultiEdit",
                json!({"edits": [{"file_path": "a.rs"}, {"file_path": "b.rs"}]})
            ),
            Verdict::PreApproved
        );

        // Outside the project, through `..`, `~` or an absolute path.
        assert_eq!(v("Write", write("../x.rs")), Verdict::Rules);
        assert_eq!(v("Write", write("/etc/hosts")), Verdict::Rules);
        assert_eq!(v("Write", write("~/.bashrc")), Verdict::Rules);
        let climb = format!("{}/src/../../x", root.display());
        assert_eq!(v("Write", write(&climb)), Verdict::Rules);
        // One protected file makes the whole MultiEdit prompt.
        assert_eq!(
            v(
                "MultiEdit",
                json!({"edits": [{"file_path": "a.rs"}, {"file_path": "Cargo.toml"}]})
            ),
            Verdict::Rules
        );
        assert_eq!(v("MultiEdit", json!({"edits": []})), Verdict::Rules);
        assert_eq!(v("Write", json!({})), Verdict::Rules);
        assert_eq!(v("Write", write(".git/hooks/pre-commit")), Verdict::Rules);
        assert_eq!(v("Edit", write(".github/workflows/ci.yml")), Verdict::Rules);
        // Commands and other tools are left to the rules.
        assert_eq!(v("Bash", json!({"command": "ls"})), Verdict::Rules);
        assert_eq!(v("mcp__fs__write_file", json!({})), Verdict::Rules);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_out_of_the_project_or_into_a_protected_dir_is_not_pre_approved() {
        let proj = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let root = proj.path();
        std::fs::create_dir_all(root.join(".git/hooks")).unwrap();
        std::os::unix::fs::symlink(outside.path(), root.join("out")).unwrap();
        std::os::unix::fs::symlink(root.join(".git/hooks"), root.join("hooks")).unwrap();
        let v = |p: &str| Autonomy::AutoEdit.verdict_with_home("Write", &write(p), root, None);
        assert_eq!(v("out/x.rs"), Verdict::Rules);
        assert_eq!(v("hooks/pre-commit"), Verdict::Rules);
        assert_eq!(v("src/x.rs"), Verdict::PreApproved);
    }

    #[test]
    fn no_edit_is_pre_approved_when_the_project_is_home_or_above_it() {
        let home = tempfile::tempdir().unwrap();
        let h = home.path();
        for root in [h, h.parent().unwrap()] {
            for mode in [Autonomy::AutoEdit, Autonomy::FullAuto] {
                assert_eq!(
                    mode.verdict_with_home("Write", &write("notes.txt"), root, Some(h)),
                    Verdict::Rules,
                    "{mode} in {}",
                    root.display()
                );
            }
            // The sandbox binds the cwd read-write, so from $HOME a command
            // reaches every dotfile: full-auto pre-approves none either.
            for tool in ["Bash", "PowerShell", "mcp__fs__write_file"] {
                assert_eq!(
                    Autonomy::FullAuto.verdict_with_home(
                        tool,
                        &json!({"command": "echo x >> ~/.bashrc"}),
                        root,
                        Some(h)
                    ),
                    Verdict::Rules,
                    "{tool} in {}",
                    root.display()
                );
            }
        }
        // A project inside $HOME is fine.
        let proj = h.join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        assert_eq!(
            Autonomy::AutoEdit.verdict_with_home("Write", &write("a.rs"), &proj, Some(h)),
            Verdict::PreApproved
        );
        assert_eq!(
            Autonomy::FullAuto.verdict_with_home("Bash", &json!({"command": "ls"}), &proj, Some(h)),
            Verdict::PreApproved
        );

        // The user is told, for the two modes that would pre-approve.
        for mode in Autonomy::ALL {
            let line = home_notice_with(mode, h, Some(h));
            let preapproves = matches!(mode, Autonomy::AutoEdit | Autonomy::FullAuto);
            assert_eq!(line.is_some(), preapproves, "{mode}");
            if let Some(line) = line {
                assert!(
                    line.contains(mode.as_str()) && line.contains("home"),
                    "{line}"
                );
            }
            assert_eq!(home_notice_with(mode, &proj, Some(h)), None, "{mode}");
        }
    }

    #[test]
    fn suggest_prompts_every_edit_and_full_auto_pre_approves_commands() {
        let root = Path::new("/proj");
        let home = Some(Path::new("/home/u"));
        for tool in EDIT_TOOLS {
            assert_eq!(
                Autonomy::Suggest.verdict_with_home(tool, &write("a"), root, home),
                Verdict::Prompt,
                "{tool}"
            );
        }
        assert_eq!(
            Autonomy::Suggest.verdict_with_home("Bash", &json!({"command": "ls"}), root, home),
            Verdict::Rules
        );
        for tool in ["Bash", "PowerShell"] {
            assert_eq!(
                Autonomy::FullAuto.verdict_with_home(tool, &write("/etc/x"), root, home),
                Verdict::PreApproved,
                "{tool}"
            );
        }
        // bwrap confines only shell commands: in-process edits outside the
        // project and MCP servers still go through the rules.
        for tool in ["Write", "mcp__fs__write_file"] {
            assert_eq!(
                Autonomy::FullAuto.verdict_with_home(tool, &write("/etc/x"), root, home),
                Verdict::Rules,
                "{tool}"
            );
        }
        for tool in [
            "ExitPlanMode",
            crate::tools::browser_tools::LOOPBACK_QUESTION,
        ] {
            assert_eq!(
                Autonomy::FullAuto.verdict_with_home(tool, &json!({}), root, home),
                Verdict::Rules,
                "{tool}"
            );
        }
        assert_eq!(
            Autonomy::FullAuto.verdict_with_home(
                "ExitWorktree",
                &json!({"discard_changes": true}),
                root,
                home
            ),
            Verdict::Rules
        );
        // Tools bwrap does not confine are left to the rules too.
        for tool in [
            "ExitWorktree",
            "Read",
            "WebFetch",
            "Agent",
            "EnterWorktree",
            "browser_navigate",
        ] {
            assert_eq!(
                Autonomy::FullAuto.verdict_with_home(tool, &json!({}), root, home),
                Verdict::Rules,
                "{tool}"
            );
        }
        for tool in ["Bash", "Write"] {
            assert_eq!(
                Autonomy::Ask.verdict_with_home(tool, &write("a"), root, home),
                Verdict::Rules
            );
        }
    }

    #[test]
    fn full_auto_gives_edits_auto_edits_rule() {
        let home = tempfile::tempdir().unwrap();
        let h = home.path();
        let proj = h.join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        let outside = tempfile::tempdir().unwrap();
        let out_file = outside.path().join("x.rs");
        let out_file = out_file.to_str().unwrap();
        // In-project, unprotected: pre-approved.
        assert_eq!(
            Autonomy::FullAuto.verdict_with_home("Write", &write("src/a.rs"), &proj, Some(h)),
            Verdict::PreApproved
        );
        // A dotfile in $HOME, an absolute path outside the project, a
        // relative escape, or a protected file: the rules decide (prompt).
        for target in [
            "~/.bashrc",
            out_file,
            "../escape.rs",
            ".mcp.json",
            "Cargo.toml",
        ] {
            assert_eq!(
                Autonomy::FullAuto.verdict_with_home("Write", &write(target), &proj, Some(h)),
                Verdict::Rules,
                "{target}"
            );
        }
    }

    #[test]
    fn full_auto_falls_back_to_ask_without_an_isolating_sandbox() {
        // firejail's default profile leaves $HOME writable: not isolation
        // enough for unprompted commands.
        for (enabled, mode) in [
            (false, "bwrap"),
            (true, "strict"),
            (true, "firejail"),
            (true, "nonsense"),
        ] {
            assert!(
                full_auto_blocker(enabled, mode, false).is_some(),
                "{enabled} {mode}"
            );
            assert_eq!(
                Autonomy::FullAuto.effective(enabled, mode, false),
                Autonomy::Ask
            );
        }
        assert_eq!(
            Autonomy::AutoEdit.effective(false, "strict", true),
            Autonomy::AutoEdit
        );
        if !cfg!(target_os = "linux") {
            assert!(full_auto_blocker(true, "bwrap", false).is_some());
        } else if crate::sandbox::bwrap_available() {
            assert_eq!(full_auto_blocker(true, "bwrap", false), None);
            assert_eq!(
                Autonomy::FullAuto.effective(true, "bwrap", false),
                Autonomy::FullAuto
            );
        } else {
            let why = full_auto_blocker(true, "bwrap", false).unwrap();
            assert!(why.contains("not installed"), "{why}");
        }
    }

    /// bwrap confines the filesystem, not the network: with it on, an
    /// unprompted command could send the project and secrets anywhere.
    #[test]
    fn full_auto_requires_the_sandbox_network_off() {
        if !cfg!(target_os = "linux") {
            return;
        }
        let why = full_auto_blocker(true, "bwrap", true).unwrap();
        assert!(why.contains("/sandbox network off"), "{why}");
        assert_eq!(
            Autonomy::FullAuto.effective(true, "bwrap", true),
            Autonomy::Ask
        );
    }

    #[test]
    fn strictness_orders_the_modes() {
        assert!(Autonomy::Suggest.at_least_as_strict_as(Autonomy::Ask));
        assert!(Autonomy::Ask.at_least_as_strict_as(Autonomy::Ask));
        assert!(Autonomy::Ask.at_least_as_strict_as(Autonomy::AutoEdit));
        assert!(!Autonomy::AutoEdit.at_least_as_strict_as(Autonomy::Ask));
        assert!(!Autonomy::FullAuto.at_least_as_strict_as(Autonomy::AutoEdit));
    }
}
