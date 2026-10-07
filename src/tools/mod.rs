/// Tool trait and registry — port of Tool.ts and tools.ts
pub mod agent;
pub mod ask_user;
pub mod bash;
pub mod browser_tools;
pub mod config_tool;
pub mod discover_skills;
pub mod file_edit;
pub mod file_read;
pub mod file_write;
pub mod glob;
pub mod grep;
pub mod lsp;
pub mod mcp_resources;
pub mod memory;
pub mod multi_edit;
pub mod notebook;
pub mod plan_mode;
pub mod powershell;
pub mod send_message;
pub mod skill_tool;
pub mod sleep;
pub mod tasks;
pub mod team_tools;
pub mod todo;
pub mod tool_search;
pub mod web_browser;
pub mod web_fetch;
pub mod web_search;
pub mod workflow;
pub mod worktree;

use crate::api::types::{ToolDefinition, ToolResultContent};
use anyhow::Result;
pub use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::oneshot;

/// Cache of (path → content-hash) populated by the Read tool so it can
/// emit a compact "unchanged since last read" notice instead of re-sending
/// the full file body on repeat reads. v2.1.86 token-overhead fix.
pub type ReadCache = Arc<Mutex<HashMap<std::path::PathBuf, u64>>>;

pub fn new_read_cache() -> ReadCache {
    Arc::new(Mutex::new(HashMap::new()))
}

/// Permission mode for the session — mirrors PermissionMode in types/permissions.ts
#[derive(Debug, Clone, PartialEq)]
#[allow(dead_code)] // variants wired to CLI flags, not all exposed yet
pub enum PermissionMode {
    Default,
    AutoEdit,
    BypassPermissions,
}

/// Minimal tool execution context — mirrors ToolUseContext in Tool.ts
#[derive(Clone)]
pub struct ToolContext {
    pub cwd: std::path::PathBuf,
    pub permission_mode: PermissionMode,
    pub verbose: bool,
    /// Optional channel for streaming live output lines to the TUI during tool execution.
    pub stream_tx: Option<tokio::sync::mpsc::UnboundedSender<String>>,
    /// Optional channel for AskUserQuestion tool to request user input.
    /// Tuple: (question_text, reply_sender).
    pub ask_user_tx: Option<tokio::sync::mpsc::UnboundedSender<(String, oneshot::Sender<String>)>>,
    /// Optional channel for plan mode tools to toggle plan mode.
    pub plan_mode_tx: Option<tokio::sync::mpsc::UnboundedSender<bool>>,
    /// Default shell for the Bash tool ("bash", "powershell", etc.).
    /// None = $SHELL when it is bash or zsh, else "bash", else a POSIX sh (see `bash_tool_shell`).
    pub default_shell: Option<String>,
    /// `env` from settings.json, set on every Bash / PowerShell command.
    pub env: std::collections::HashMap<String, String>,

    /// If set, Write/Edit tools snapshot the original file here before modifying it.
    /// Set to `<data dir>/sessions/<sid>/snapshots/turn-<n>/` by the run loop.
    pub snapshot_dir: Option<std::path::PathBuf>,

    /// Active sandbox mode for the Bash tool ("strict", "bwrap", "firejail").
    /// None means sandbox is disabled.
    pub sandbox_mode: Option<String>,

    /// Whether bwrap sandbox allows outbound network (passed to bwrap_wrap).
    pub sandbox_allow_network: bool,

    /// Shared Read-tool cache: path → content hash. Lets the Read tool skip
    /// re-emitting a file body that hasn't changed since the last read.
    pub read_cache: Option<ReadCache>,

    /// Live provider snapshot taken at turn start. Tools that spawn a new
    /// `QueryEngine` (AgentTool, background spawn) must read these instead
    /// of their own cached `config` snapshot, which goes stale the moment
    /// the user runs `/model foo` mid-session. When `None`, tools fall back
    /// to their internal snapshot.
    pub live_model: Option<String>,
    pub live_api_key: Option<String>,
    pub live_ollama_host: Option<String>,
    /// Plan mode and thinking budget as the executor sees them now; `/plan`,
    /// EnterPlanMode and `/reload` change these after the registry (and
    /// `Config` tool's snapshot) was built.
    pub live_plan_mode: Option<bool>,
    pub live_thinking_budget: Option<Option<u32>>,
    /// The permission gate of the executor running this tool. A tool that
    /// launches a nested engine (`Agent`) must hand it on so every
    /// descendant prompts through the same human — or, headless, fails
    /// closed the same way.
    pub permission_gate: Option<crate::permissions::PermissionGate>,
    /// How many `Agent` launches deep this executor is (0 = the session).
    pub agent_depth: u8,
    /// Where a tool that runs its own engine (`Agent`) reports each API
    /// response it pays for, so the executor's cost tracking and budget
    /// include it.
    pub usage_sink: Option<UsageSink>,
    /// What is left of the executor's budget when this tool starts. A
    /// sub-agent caps itself at this rather than at the full budget.
    pub budget_remaining_usd: Option<f64>,

    /// Middleware chain: pre/post hooks around every tool call.
    /// Default empty = no-op (existing behavior unchanged).
    pub middlewares: crate::browser::middleware::MiddlewareChain,
}

/// One sub-agent API response: the model it ran on and what it used.
pub type UsageSink = tokio::sync::mpsc::UnboundedSender<(String, crate::api::types::Usage)>;

impl std::fmt::Debug for ToolContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolContext")
            .field("cwd", &self.cwd)
            .field("permission_mode", &self.permission_mode)
            .field("verbose", &self.verbose)
            .finish()
    }
}

impl ToolContext {
    pub fn new(cwd: std::path::PathBuf) -> Self {
        Self {
            cwd,
            permission_mode: PermissionMode::Default,
            verbose: false,
            stream_tx: None,
            ask_user_tx: None,
            plan_mode_tx: None,
            default_shell: None,
            env: std::collections::HashMap::new(),
            snapshot_dir: None,
            sandbox_mode: None,
            sandbox_allow_network: true,
            read_cache: None,
            live_model: None,
            live_api_key: None,
            live_ollama_host: None,
            live_plan_mode: None,
            live_thinking_budget: None,
            permission_gate: None,
            agent_depth: 0,
            usage_sink: None,
            budget_remaining_usd: None,
            middlewares: Vec::new(),
        }
    }
}

/// Snapshot a file to the ctx.snapshot_dir before it is modified.
/// The snapshot preserves the file at its current state so /rewind can restore it.
/// Silently skips if snapshot_dir is None or the file doesn't exist yet (new file).
pub async fn snapshot_file(ctx: &ToolContext, path: &std::path::Path) {
    let Some(ref snap_dir) = ctx.snapshot_dir else {
        return;
    };
    if !path.exists() {
        return;
    } // new file — nothing to snapshot

    let flat_name = snapshot_name(path);

    if tokio::fs::create_dir_all(snap_dir).await.is_err() {
        return;
    }
    let dest = snap_dir.join(&flat_name);
    // A name that is not a plain file name would make join() escape snap_dir
    // (an absolute Windows path replaces it outright, so dest == path).
    if dest.parent() != Some(snap_dir.as_path()) {
        return;
    }
    // The first snapshot of a turn is the state /rewind must return to; a
    // second edit in the same turn would otherwise overwrite it.
    if dest.exists() {
        return;
    }
    let _ = tokio::fs::copy(path, &dest).await;
}

/// Reversible flat file name for a snapshot of `path`: separators → `_`, with
/// literal `%` and `_` escaped. (Mapping only `/` → `_` restored
/// `src/query_engine.rs` to `src/query/engine.rs`.)
pub fn snapshot_name(path: &std::path::Path) -> String {
    flatten_path(&path.to_string_lossy(), cfg!(windows))
}

/// Inverse of [`snapshot_name`].
pub fn snapshot_path(flat: &str) -> std::path::PathBuf {
    std::path::PathBuf::from(unflatten_path(flat, cfg!(windows)))
}

/// On Windows both `\` and `/` separate, the drive colon is not a legal file
/// name character, and canonicalize() adds a `\\?\` prefix whose `?` is not
/// either. Unix names keep `\` and `:` as is, so existing local-mcp file
/// names stay the same.
fn flatten_path(path: &str, windows: bool) -> String {
    let mut path = path.replace('%', "%25").replace('_', "%5F");
    if !windows {
        return path.replace('/', "_").trim_start_matches('_').to_string();
    }
    if let Some(unc) = path.strip_prefix(r"\\?\UNC\") {
        path = format!(r"\\{unc}");
    } else if let Some(rest) = path.strip_prefix(r"\\?\") {
        path = rest.to_string();
    }
    // Leading separators are kept: they mark a UNC path.
    path.replace(':', "%3A").replace(['\\', '/'], "_")
}

fn unflatten_path(flat: &str, windows: bool) -> String {
    let parts = flat.split('_').map(|part| {
        part.replace("%5F", "_")
            .replace("%3A", ":")
            .replace("%25", "%")
    });
    if windows {
        parts.collect::<Vec<_>>().join("\\")
    } else {
        format!("/{}", parts.collect::<Vec<_>>().join("/"))
    }
}

#[cfg(test)]
mod snapshot_name_tests {
    use super::{flatten_path, unflatten_path};

    #[test]
    fn names_round_trip_paths_with_underscores() {
        for p in ["/home/u/src/query_engine.rs", "/a/100%_done/b_c", "/x/y.rs"] {
            let path = std::path::Path::new(p);
            assert_eq!(super::snapshot_path(&super::snapshot_name(path)), path);
        }
    }

    #[test]
    fn windows_names_are_flat_and_round_trip() {
        for (p, back) in [
            (r"C:\Users\u\my_proj\a.rs", r"C:\Users\u\my_proj\a.rs"),
            (r"C:/Users/u/100%/a.rs", r"C:\Users\u\100%\a.rs"),
            (r"\\?\C:\Users\u\proj", r"C:\Users\u\proj"),
            (r"\\?\UNC\srv\share\a.rs", r"\\srv\share\a.rs"),
            (r"\\srv\share\a.rs", r"\\srv\share\a.rs"),
        ] {
            let flat = flatten_path(p, true);
            assert!(
                !flat.contains(['\\', '/', ':', '?']),
                "{p} -> {flat} is not a plain file name"
            );
            assert_eq!(unflatten_path(&flat, true), back, "{p} -> {flat}");
        }
    }

    #[test]
    fn unix_names_are_unchanged() {
        assert_eq!(flatten_path("/home/u/a:b\\c_d", false), "home_u_a:b\\c%5Fd");
    }
}

/// Result of a tool execution
#[derive(Debug)]
pub struct ToolOutput {
    pub content: Vec<ToolResultContent>,
    pub is_error: bool,
}

impl ToolOutput {
    pub fn success(text: impl Into<String>) -> Self {
        Self {
            content: vec![ToolResultContent::text(text)],
            is_error: false,
        }
    }

    pub fn error(text: impl Into<String>) -> Self {
        Self {
            content: vec![ToolResultContent::text(text)],
            is_error: true,
        }
    }
}

/// Directories that Write/Edit tools must never modify.
/// Protected directories that Write/Edit tools must never modify.
pub const PROTECTED_DIRS: &[&str] = &[".git", ".husky"];

/// Is any component of `path` `name`, ignoring ASCII case? macOS and
/// Windows filesystems are case-insensitive: `.GIT/config` is `.git/config`.
fn has_component_ignore_case(path: &std::path::Path, name: &str) -> bool {
    path.components().any(|c| {
        c.as_os_str()
            .to_str()
            .is_some_and(|s| s.eq_ignore_ascii_case(name))
    })
}

/// Returns Some(error ToolOutput) if `path` is inside a protected directory.
/// The resolved path is checked too: a repo can commit `gl -> .git`, and a
/// write to `gl/config` adding `core.fsmonitor` runs code on the next git
/// command; `link/newdir/../x` otherwise reaches `.git/hooks` under an
/// innocent name.
pub fn check_protected_path(path: &std::path::Path) -> Option<ToolOutput> {
    let resolved = resolve_for_sensitivity_check(path);
    for &protected in PROTECTED_DIRS {
        if has_component_ignore_case(path, protected)
            || has_component_ignore_case(&resolved, protected)
        {
            return Some(ToolOutput::error(format!(
                "Modifying files inside '{}' directories is not allowed.",
                protected
            )));
        }
    }
    None
}

/// Which tool operation is asking the sensitive-path guard.
/// `Read` is permissive: only private-key material is blocked so normal
/// project work can still read `.env.example` and inspect config.
/// `Write` is strict: blocks every class of secret to stop accidental
/// overwrites of `.ssh/`, `.aws/credentials`, `.env`, etc.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SensitiveOp {
    Read,
    Write,
}

/// File names / suffixes that represent private-key material.
/// Blocked for BOTH read and write.
const PRIVATE_KEY_NAMES: &[&str] = &["id_rsa", "id_ed25519", "id_ecdsa", "id_dsa", "identity"];
const PRIVATE_KEY_SUFFIXES: &[&str] = &[".pem", ".key", ".p12", ".pfx", ".jks", ".keystore"];

/// File names that are secret-material for write-only.
/// These are commonly read by tooling (e.g. `.env` for config inspection)
/// but must never be clobbered by the agent.
const SECRET_WRITE_BLOCK_NAMES: &[&str] = &[
    "credentials",
    "credentials.json",
    ".netrc",
    ".pgpass",
    "kubeconfig",
];

/// Directory components that signal a secrets tree.
/// Any path containing one of these components is blocked on write.
const SECRET_DIR_COMPONENTS: &[&str] = &[".ssh", ".aws", ".gnupg", ".kube"];

/// Does `file_name` look like a dotenv variant (`.env`, `.env.local`,
/// `.env.production`, etc.) — but NOT `.env.example` / `.env.sample`
/// which are safe templates?
fn is_dotenv(file_name: &str) -> bool {
    if !file_name.starts_with(".env") {
        return false;
    }
    let rest = &file_name[4..];
    if rest.is_empty()
        || rest == ".local"
        || rest == ".production"
        || rest == ".development"
        || rest == ".test"
        || rest == ".staging"
    {
        return true;
    }
    // Skip known-safe templates.
    if rest == ".example" || rest == ".sample" || rest == ".template" || rest == ".dist" {
        return false;
    }
    // Other `.env.*` variants — treat as sensitive by default.
    rest.starts_with('.')
}

/// Returns Some(error ToolOutput) if the path should be blocked for `op`.
/// Read mode blocks private keys only. Write mode additionally blocks
/// dotenv files, credential stores, and secrets directories.
/// Write a file atomically: temp file in the same directory, fsync, rename.
///
/// A direct `fs::write` truncates the target first, so a crash, a full disk, or
/// a kill between truncate and write leaves the user's file empty or partial —
/// silently, and with the original gone. `session/` was given this treatment in
/// an earlier audit; the file tools were not.
///
/// The temp file is created in the *same directory* so the rename is a
/// same-filesystem operation and therefore atomic; `/tmp` may be a different
/// mount, where rename degrades to copy-then-delete and loses the guarantee.
///
/// **Permissions are carried over from the original.** Renaming replaces the
/// inode, so without this an edit to a `0600` file would silently republish it
/// at the default `0644` — turning a routine edit into a disclosure.
///
/// An existing file that can be written but not replaced (single-file bind
/// mount, non-writable directory) is written in place instead, as `fs::write`
/// would.
pub async fn atomic_write(path: &std::path::Path, content: &str) -> std::io::Result<()> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);

    // rename(2) replaces the directory entry, so renaming onto a symlink
    // (CLAUDE.md -> AGENTS.md, stow-managed dotfiles) would swap the link for
    // a regular file and leave its target stale. Write the target instead.
    let target = resolve_symlink_target(path).await;
    let path = target.as_path();

    // rename only needs a writable directory, so without this a 0444 file
    // that fs::write would have refused gets silently replaced.
    let existing = tokio::fs::metadata(path).await.ok();
    if existing
        .as_ref()
        .is_some_and(|m| m.permissions().readonly())
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("{} is read-only", path.display()),
        ));
    }

    let parent = path.parent().unwrap_or_else(|| std::path::Path::new("."));
    let stem = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let tmp = parent.join(format!(
        ".{stem}.oxideclaw-{}-{}.tmp",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));

    // Mode of the file we are replacing, if it exists.
    #[cfg(unix)]
    let mode = existing.as_ref().map(|m| {
        use std::os::unix::fs::PermissionsExt;
        m.permissions().mode()
    });
    #[cfg(not(unix))]
    let mode = None;

    // Some existing files can be written but not replaced: a single-file bind
    // mount (`docker -v ./config.yml:/work/config.yml`, /etc/hosts) refuses
    // rename with EBUSY/EXDEV, and a writable file in a non-writable
    // directory refuses the temp file. fs::write handled those, so fall back
    // to it there rather than failing an edit the user can plainly make.
    let in_place = |e: &std::io::Error| {
        use std::io::ErrorKind::*;
        existing.is_some()
            && matches!(
                e.kind(),
                PermissionDenied | ResourceBusy | CrossesDevices | ReadOnlyFilesystem
            )
    };

    let mut f = match create_temp(&tmp, mode).await {
        Ok(f) => f,
        Err(e) if in_place(&e) => return write_in_place(path, content).await,
        // Not ours (create_new refused an existing name), so not removed.
        Err(e) => return Err(e),
    };

    let staged = async {
        tokio::io::AsyncWriteExt::write_all(&mut f, content.as_bytes()).await?;
        // Durability: without this the rename can land before the data does, so
        // a crash yields a present-but-empty file — the exact outcome this is
        // meant to prevent.
        f.sync_all().await?;
        drop(f);

        // umask may have cleared bits the original had; restore them exactly.
        #[cfg(unix)]
        if let Some(mode) = mode {
            use std::os::unix::fs::PermissionsExt;
            tokio::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode)).await?;
        }
        Ok(())
    }
    .await;

    let renamed = match staged {
        Ok(()) => tokio::fs::rename(&tmp, path).await,
        Err(e) => {
            // Never leave a stray temp file behind on failure.
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(e);
        }
    };
    match renamed {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = tokio::fs::remove_file(&tmp).await;
            if in_place(&e) {
                return write_in_place(path, content).await;
            }
            Err(e)
        }
    }
}

/// Create `tmp` already at the replaced file's mode. Creating it at the
/// default 0644 and chmod-ing afterwards left a 0600 file's new contents
/// world-readable for the whole write+fsync, and an fd opened in that window
/// keeps read access after the chmod and the rename.
async fn create_temp(tmp: &std::path::Path, mode: Option<u32>) -> std::io::Result<tokio::fs::File> {
    let mut opts = tokio::fs::OpenOptions::new();
    // create_new: never reuse or follow something already at the temp name.
    opts.write(true).create_new(true);
    #[cfg(unix)]
    opts.mode(mode.map_or(0o666, |m| m & 0o777));
    #[cfg(not(unix))]
    let _ = mode;
    opts.open(tmp).await
}

/// Non-atomic fallback for files that can be written but not replaced.
async fn write_in_place(path: &std::path::Path, content: &str) -> std::io::Result<()> {
    let mut f = tokio::fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(path)
        .await?;
    tokio::io::AsyncWriteExt::write_all(&mut f, content.as_bytes()).await?;
    f.sync_all().await
}

#[cfg(all(test, unix))]
mod write_escape_tests {
    use super::{Tool, ToolContext};
    use std::os::unix::fs::symlink;

    /// atomic_write follows a symlink to its target, so a repo-shipped
    /// `docs/notes.md -> ~/.bashrc` let an approved project edit rewrite a
    /// file outside the project.
    #[tokio::test]
    async fn writes_through_a_link_out_of_the_project_are_refused() {
        let proj = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("bashrc");
        std::fs::write(&target, "orig\n").unwrap();
        std::fs::create_dir(proj.path().join("docs")).unwrap();
        symlink(&target, proj.path().join("docs/notes.md")).unwrap();
        symlink(outside.path(), proj.path().join("ext")).unwrap();
        std::fs::write(proj.path().join("AGENTS.md"), "a\n").unwrap();
        symlink("AGENTS.md", proj.path().join("CLAUDE.md")).unwrap();
        let ctx = ToolContext::new(proj.path().to_path_buf());

        for (tool, input) in [
            (
                Box::new(super::file_write::FileWriteTool) as Box<dyn Tool>,
                serde_json::json!({"file_path": "docs/notes.md", "content": "pwned"}),
            ),
            (
                Box::new(super::file_edit::FileEditTool),
                serde_json::json!({"file_path": "docs/notes.md", "old_string": "orig", "new_string": "pwned"}),
            ),
            (
                Box::new(super::file_write::FileWriteTool),
                serde_json::json!({"file_path": "ext/bashrc", "content": "pwned"}),
            ),
        ] {
            let out = tool.execute(input, &ctx).await.unwrap();
            assert!(out.is_error, "{:?}", out.content);
        }
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "orig\n");

        // A link inside the project is still written through.
        let out = super::file_write::FileWriteTool
            .execute(
                serde_json::json!({"file_path": "CLAUDE.md", "content": "b\n"}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{:?}", out.content);
        assert_eq!(
            std::fs::read_to_string(proj.path().join("AGENTS.md")).unwrap(),
            "b\n"
        );
        // So is a file outside the project named by its own path.
        let out = super::file_write::FileWriteTool
            .execute(
                serde_json::json!({"file_path": target.to_str().unwrap(), "content": "direct"}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{:?}", out.content);
    }
}

#[cfg(all(test, unix))]
mod atomic_write_tests {
    use super::atomic_write;
    use std::os::unix::fs::{PermissionsExt, symlink};

    /// Writing CLAUDE.md -> AGENTS.md replaced the link with a regular file
    /// and left AGENTS.md stale.
    #[tokio::test]
    async fn a_symlink_survives_and_its_target_is_written() {
        let dir = tempfile::tempdir().unwrap();
        let link = dir.path().join("CLAUDE.md");
        std::fs::write(dir.path().join("AGENTS.md"), "orig").unwrap();
        symlink("AGENTS.md", &link).unwrap();

        atomic_write(&link, "new").await.unwrap();

        assert!(link.symlink_metadata().unwrap().file_type().is_symlink());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("AGENTS.md")).unwrap(),
            "new"
        );
    }

    /// A dangling link is not followed: its target is never created.
    #[tokio::test]
    async fn a_dangling_symlink_is_replaced_not_followed() {
        let dir = tempfile::tempdir().unwrap();
        let link = dir.path().join("notes");
        symlink("elsewhere", &link).unwrap();

        atomic_write(&link, "x").await.unwrap();

        assert!(!dir.path().join("elsewhere").exists());
        assert_eq!(std::fs::read_to_string(&link).unwrap(), "x");
    }

    /// rename only needs a writable directory, so a 0444 file was replaced.
    #[tokio::test]
    async fn a_read_only_file_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("locked.txt");
        std::fs::write(&f, "orig").unwrap();
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o444)).unwrap();

        let err = atomic_write(&f, "new").await.unwrap_err();

        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "orig");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    /// The temp file was created 0644 and only chmod-ed to 0600 after the
    /// new contents were written and fsynced.
    #[tokio::test]
    async fn the_temp_file_is_born_with_the_original_mode() {
        let dir = tempfile::tempdir().unwrap();
        let tmp = dir.path().join(".secrets.tmp");

        let f = super::create_temp(&tmp, Some(0o100600)).await.unwrap();

        let mode = f.metadata().await.unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "{mode:o}");
        // Something already at the temp name is never reused.
        assert!(super::create_temp(&tmp, None).await.is_err());
    }

    /// A writable file in a non-writable directory could not be edited at
    /// all, because the temp file had nowhere to go.
    #[tokio::test]
    async fn a_writable_file_in_a_locked_directory_is_written_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let locked = dir.path().join("locked");
        std::fs::create_dir(&locked).unwrap();
        let f = locked.join("config.yml");
        std::fs::write(&f, "orig").unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o555)).unwrap();
        // Root ignores directory permissions, so there is nothing to test.
        let probe = locked.join("probe");
        let enforced = std::fs::write(&probe, "").is_err();
        let _ = std::fs::remove_file(&probe);

        let res = atomic_write(&f, "new").await;
        let entries = std::fs::read_dir(&locked).unwrap().count();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        if !enforced {
            return;
        }

        res.unwrap();
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "new");
        assert_eq!(entries, 1);
        // A new file there still fails: there is nothing to write in place.
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o555)).unwrap();
        let err = atomic_write(&locked.join("new.txt"), "x")
            .await
            .unwrap_err();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
    }
}

/// The file a write to `path` should land on: `path` itself unless it is a
/// symlink to an existing file, then that file.
///
/// A dangling link is left to be replaced, not followed: the sensitive-path
/// guard can only see where a link points once the target exists, so creating
/// it here would let `notes -> ~/.ssh/authorized_keys` through.
async fn resolve_symlink_target(path: &std::path::Path) -> std::path::PathBuf {
    let is_link = tokio::fs::symlink_metadata(path)
        .await
        .is_ok_and(|m| m.file_type().is_symlink());
    if is_link && let Ok(real) = tokio::fs::canonicalize(path).await {
        return real;
    }
    path.to_path_buf()
}

/// CRLF forms of an Edit's `old`/`new` strings, when the raw `old` cannot match
/// only because of line endings.
///
/// Read shows files through `str::lines()`, which drops the `\r`, so on a CRLF
/// file (the default checkout on Windows) the model writes an LF-joined
/// multi-line `old_string` that never matches. Raw matches always win, so LF and
/// mixed-ending files behave exactly as before; only a zero-match multi-line
/// `old` in a file that has CRLF falls back, and then `new` is converted too so
/// inserted lines keep the file's endings.
pub fn crlf_fallback(content: &str, old: &str, new: &str) -> Option<(String, String)> {
    if !old.contains('\n')
        || old.contains('\r')
        || !content.contains("\r\n")
        || content.contains(old)
    {
        return None;
    }
    let to_crlf = |s: &str| s.replace("\r\n", "\n").replace('\n', "\r\n");
    Some((to_crlf(old), to_crlf(new)))
}

/// Deny-listed read paths expressed as ripgrep exclusion globs.
///
/// The Grep tool has two backends — a `ripgrep` subprocess and a pure-Rust
/// fallback — and both must honour the same rules. The fallback can check each
/// file as it opens it; `rg` opens files itself, so it has to be told up front.
///
/// Each entry is `(rg flag, glob)`. [`check_sensitive_path`] matches names
/// exactly but suffixes case-insensitively, so the suffix globs go out as
/// `--iglob`: with plain `--glob`, `server.KEY` or `CERT.PEM` were printed by
/// Grep while Read refused them.
pub fn denied_read_globs() -> Vec<(&'static str, String)> {
    let mut g: Vec<(&'static str, String)> = PRIVATE_KEY_NAMES
        .iter()
        .map(|n| ("--glob", format!("!**/{n}")))
        .collect();
    g.extend(
        PRIVATE_KEY_SUFFIXES
            .iter()
            .map(|s| ("--iglob", format!("!**/*{s}"))),
    );
    g
}

/// Resolve symlinks so [`check_sensitive_path`] sees the file that will actually
/// be touched, not the name the caller supplied.
///
/// The deny-list matches on the *file name*, so without this every protection it
/// offers is defeated by a symlink with an innocuous name. Verified: a link
/// named `notes.md` pointing at `~/.ssh/id_rsa` returned the private key, and a
/// link named `config.json` pointing at `~/.aws/credentials` overwrote them —
/// while the same operations on the real names were correctly refused.
///
/// That needs no unusual privileges: a repository can simply *ship* a symlink
/// called `README.md`, and asking the agent to read it exfiltrates the target.
///
/// For paths that do not exist yet (a fresh write) it canonicalizes the deepest
/// existing ancestor and replays the rest lexically. Checking only the parent
/// was bypassable: in `link/newdir/../authorized_keys` the parent cannot be
/// canonicalized (`newdir` does not exist yet), so the literal path was checked
/// while `create_dir_all` + rename landed in the link's target. The replay
/// re-resolves after every component that exists: a `..` after a missing
/// directory pops back into an existing one, and the next component may be a
/// symlink (`missing/../gl/config` with `gl -> .git`). Falls back to the input
/// unchanged only when no ancestor resolves.
pub fn resolve_for_sensitivity_check(path: &std::path::Path) -> std::path::PathBuf {
    use std::path::Component;
    if let Ok(real) = std::fs::canonicalize(path) {
        return real;
    }
    for anc in path.ancestors().skip(1) {
        let Ok(mut real) = std::fs::canonicalize(anc) else {
            continue;
        };
        let Ok(rest) = path.strip_prefix(anc) else {
            break;
        };
        for c in rest.components() {
            match c {
                Component::Normal(n) => {
                    real.push(n);
                    if real.symlink_metadata().is_ok()
                        && let Ok(r) = std::fs::canonicalize(&real)
                    {
                        real = r;
                    }
                }
                Component::ParentDir => {
                    real.pop();
                }
                _ => {}
            }
        }
        return real;
    }
    path.to_path_buf()
}

/// Refuses a write that would land outside the project through a symlink:
/// a link named by the path (`docs/notes.md -> ~/.bashrc`), or a linked
/// directory on the way to a path inside the project (`docs -> /etc`).
/// Permission rules and the approval prompt see the name, not where it
/// leads, and `/undo` cannot revert a write outside the repository. Editing
/// the real path directly still works, with its own approval.
pub fn check_write_escape(path: &std::path::Path, cwd: &std::path::Path) -> Option<ToolOutput> {
    let real = resolve_for_sensitivity_check(path);
    let root = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    if real.starts_with(&root) {
        return None;
    }
    let is_link = path
        .symlink_metadata()
        .is_ok_and(|m| m.file_type().is_symlink());
    let names_project_file = path.starts_with(cwd) || path.starts_with(&root);
    if !is_link && !names_project_file {
        return None;
    }
    Some(ToolOutput::error(format!(
        "{} resolves through a symlink to {}, outside the project; edit that path directly so it can be approved.",
        path.display(),
        real.display()
    )))
}

/// [`check_sensitive_path`] applied to both the supplied path and its symlink
/// target. Either looking sensitive is a refusal.
///
/// Use this at every filesystem entry point; the raw `check_sensitive_path` only
/// inspects the name it is given.
pub fn check_sensitive_path_resolved(
    path: &std::path::Path,
    op: SensitiveOp,
) -> Option<ToolOutput> {
    if let Some(err) = check_sensitive_path(path, op) {
        return Some(err);
    }
    let resolved = resolve_for_sensitivity_check(path);
    if resolved != path {
        return check_sensitive_path(&resolved, op);
    }
    None
}

pub fn check_sensitive_path(path: &std::path::Path, op: SensitiveOp) -> Option<ToolOutput> {
    let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");

    // Private-key material — blocked in both modes.
    if PRIVATE_KEY_NAMES.contains(&file_name) {
        return Some(ToolOutput::error(format!(
            "Refusing to {} private-key file '{}'. This path is on the hard deny-list.",
            match op {
                SensitiveOp::Read => "read",
                SensitiveOp::Write => "modify",
            },
            file_name
        )));
    }
    if PRIVATE_KEY_SUFFIXES
        .iter()
        .any(|s| file_name.to_ascii_lowercase().ends_with(s))
    {
        return Some(ToolOutput::error(format!(
            "Refusing to {} key-material file '{}'. This path is on the hard deny-list.",
            match op {
                SensitiveOp::Read => "read",
                SensitiveOp::Write => "modify",
            },
            file_name
        )));
    }

    // For read operations, allow everything else (agent can legitimately
    // inspect `.env`, `credentials`, etc. with user permission prompts).
    if op == SensitiveOp::Read {
        return None;
    }

    // Write-only blocks below.

    // Dotenv files — never let the agent overwrite these.
    if is_dotenv(file_name) {
        return Some(ToolOutput::error(format!(
            "Refusing to modify dotenv file '{}'. The agent must not overwrite environment secrets.",
            file_name
        )));
    }

    // Well-known credential files.
    if SECRET_WRITE_BLOCK_NAMES.contains(&file_name) {
        return Some(ToolOutput::error(format!(
            "Refusing to modify credential file '{}'. This path is on the hard deny-list.",
            file_name
        )));
    }

    // Paths inside secrets directories anywhere in the ancestor chain.
    for d in SECRET_DIR_COMPONENTS {
        if has_component_ignore_case(path, d) {
            return Some(ToolOutput::error(format!(
                "Refusing to modify files inside '{d}'. This directory is on the hard deny-list."
            )));
        }
    }

    None
}

/// The core Tool trait — mirrors the Tool interface in Tool.ts
#[async_trait]
pub trait Tool: Send + Sync {
    /// Unique tool name sent to the API
    fn name(&self) -> &str;

    /// Human-readable description sent to the API
    fn description(&self) -> &str;

    /// JSON Schema for the tool's input parameters
    fn input_schema(&self) -> serde_json::Value;

    /// Execute the tool with the given JSON input
    async fn execute(&self, input: serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput>;

    /// Directory this tool has moved the session into (EnterWorktree). The
    /// executors run later tool calls there instead of `config.cwd`.
    fn session_cwd(&self) -> Option<std::path::PathBuf> {
        None
    }

    /// Build the ToolDefinition to include in API requests
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().to_string(),
            description: self.description().to_string(),
            input_schema: self.input_schema(),
            cache_control: None,
        }
    }
}

pub type DynTool = Arc<dyn Tool>;

/// Apply `--tools` / `--allowed-tools` / `--disallowed-tools`. Names match
/// case-insensitively, and as in permission rules `mcp__<server>` (or
/// `mcp__<server>__*`) covers every tool of that server and `mcp__*` every
/// MCP tool. Every entry point that builds a tool list must call this:
/// embedders rely on the flags to keep Bash or Write away from the model.
pub fn apply_tool_filters(tools: &mut Vec<DynTool>, config: &crate::config::Config) {
    use crate::permissions::name_rule_matches;
    if !config.allowed_tools.is_empty() {
        // `--tools ""` stores the "__none__" sentinel: no tools at all.
        tools.retain(|t| {
            config
                .allowed_tools
                .iter()
                .any(|a| name_rule_matches(a, t.name()))
        });
    }
    if !config.disallowed_tools.is_empty() {
        tools.retain(|t| {
            !config
                .disallowed_tools
                .iter()
                .any(|d| name_rule_matches(d, t.name()))
        });
    }
    refresh_tool_search(tools);
}

/// Rebuild ToolSearch's snapshot from `tools` as it now stands. The snapshot
/// is a copy taken at construction, which happened before MCP tools were
/// appended and before the filters ran, so it missed every MCP tool and
/// advertised filtered-out ones.
pub fn refresh_tool_search(tools: &mut Vec<DynTool>) {
    let Some(pos) = tools.iter().position(|t| t.name() == "ToolSearch") else {
        return;
    };
    tools.remove(pos);
    let snapshot = tools
        .iter()
        .map(|t| (t.name().to_string(), t.description().to_string()))
        .collect();
    tools.push(Arc::new(tool_search::ToolSearchTool {
        tools_snapshot: snapshot,
    }));
}

/// The cwd for the next tool call: the active worktree if EnterWorktree put
/// the session in one, else `default`. Executors re-read it before every call
/// so tools later in the same batch as EnterWorktree land in the worktree too.
pub fn session_cwd(tools: &[DynTool], default: &std::path::Path) -> std::path::PathBuf {
    tools
        .iter()
        .find_map(|t| t.session_cwd())
        .unwrap_or_else(|| default.to_path_buf())
}

/// Build the default tool set.
pub fn default_tools(net: crate::net_policy::NetPolicy) -> Vec<DynTool> {
    vec![
        Arc::new(bash::BashTool),
        Arc::new(file_read::FileReadTool),
        Arc::new(file_write::FileWriteTool),
        Arc::new(file_edit::FileEditTool),
        Arc::new(multi_edit::MultiEditTool),
        Arc::new(glob::GlobTool),
        Arc::new(grep::GrepTool),
        Arc::new(web_fetch::WebFetchTool { policy: net }),
    ]
}

/// All shared state needed by tools that must also be readable by slash commands.
pub struct SharedToolState {
    pub todo: todo::TodoState,
    /// Shared across the 8 browser tools and the /browser slash commands.
    /// None when config.browser_enabled == false.
    pub browser_session: Option<std::sync::Arc<tokio::sync::Mutex<crate::browser::BrowserSession>>>,
}

/// Build the full tool set. Returns tools + shared state so slash commands can read it.
pub fn all_tools_with_state(config: &crate::config::Config) -> (Vec<DynTool>, SharedToolState) {
    let net = crate::net_policy::NetPolicy::from_config(config);
    let mut tools = default_tools(net);

    tools.push(Arc::new(web_search::WebSearchTool {
        api_key: config.api_key.clone(),
        model: config.model.clone(),
        auth_is_oauth: config.auth_is_oauth,
    }));
    tools.push(Arc::new(agent::AgentTool {
        config: config.clone(),
    }));

    // Task management tools (shared registry)
    let registry = tasks::new_registry();
    tools.push(Arc::new(tasks::TaskCreateTool {
        registry: registry.clone(),
    }));
    tools.push(Arc::new(tasks::TaskGetTool {
        registry: registry.clone(),
    }));
    tools.push(Arc::new(tasks::TaskListTool {
        registry: registry.clone(),
    }));
    tools.push(Arc::new(tasks::TaskUpdateTool {
        registry: registry.clone(),
    }));
    tools.push(Arc::new(tasks::TaskStopTool {
        registry: registry.clone(),
    }));
    tools.push(Arc::new(tasks::TaskOutputTool {
        registry: registry.clone(),
    }));

    // Worktree tools (shared state)
    let wt_state = worktree::WorktreeState::default();
    tools.push(Arc::new(worktree::EnterWorktreeTool {
        state: wt_state.clone(),
    }));
    tools.push(Arc::new(worktree::ExitWorktreeTool { state: wt_state }));

    // TodoWrite — shared state readable by /tasks command
    let todo_state = todo::new_todo_state();
    tools.push(Arc::new(todo::TodoWriteTool {
        state: todo_state.clone(),
    }));

    // Interactive / meta tools
    tools.push(Arc::new(ask_user::AskUserQuestionTool));
    tools.push(Arc::new(plan_mode::EnterPlanModeTool));
    tools.push(Arc::new(plan_mode::ExitPlanModeTool));
    tools.push(Arc::new(memory::MemoryReadTool));
    tools.push(Arc::new(memory::MemoryWriteTool));

    // Agent swarm tools — enabled when OXIDECLAW_EXPERIMENTAL_AGENT_TEAMS=1
    if send_message::is_agent_swarms_enabled() {
        tools.push(Arc::new(send_message::SendMessageTool));
        tools.push(Arc::new(team_tools::TeamCreateTool));
        tools.push(Arc::new(team_tools::TeamDeleteTool));
    }

    // Simple utilities
    tools.push(Arc::new(sleep::SleepTool));
    tools.push(Arc::new(powershell::PowerShellTool));
    tools.push(Arc::new(web_browser::WebBrowserTool { policy: net }));

    // Browser automation tools (shared session across all browser_* tools AND /browser commands).
    let browser_session_shared = if config.browser_enabled {
        let browser_session = std::sync::Arc::new(tokio::sync::Mutex::new(
            crate::browser::BrowserSession::default(),
        ));
        tools.push(Arc::new(browser_tools::BrowserNavigateTool {
            session: browser_session.clone(),
            headless: config.browser_headless,
            chrome_path: config.browser_chrome_path.clone(),
            cdp_endpoint: config.browser_cdp_endpoint.clone(),
            timeout_ms: config.browser_timeout_ms,
            net_policy: net,
        }));
        tools.push(Arc::new(browser_tools::BrowserSnapshotTool {
            session: browser_session.clone(),
        }));
        tools.push(Arc::new(browser_tools::BrowserClickTool {
            session: browser_session.clone(),
        }));
        tools.push(Arc::new(browser_tools::BrowserFillTool {
            session: browser_session.clone(),
        }));
        tools.push(Arc::new(browser_tools::BrowserScreenshotTool {
            session: browser_session.clone(),
        }));
        tools.push(Arc::new(browser_tools::BrowserGetTextTool {
            session: browser_session.clone(),
        }));
        tools.push(Arc::new(browser_tools::BrowserPressKeyTool {
            session: browser_session.clone(),
        }));
        tools.push(Arc::new(browser_tools::BrowserWaitTool {
            session: browser_session.clone(),
            default_timeout_ms: config.browser_timeout_ms,
        }));
        tools.push(Arc::new(browser_tools::BrowserConsoleTool {
            session: browser_session.clone(),
        }));
        tools.push(Arc::new(browser_tools::BrowseDoneTool));
        Some(browser_session)
    } else {
        None
    };

    // --bare promises no LSP; the tool spawns language servers on use.
    if !config.bare_mode {
        tools.push(Arc::new(lsp::LSPTool::default()));
    }
    tools.push(Arc::new(discover_skills::DiscoverSkillsTool));
    tools.push(Arc::new(skill_tool::SkillTool));
    tools.push(Arc::new(workflow::WorkflowTool));

    // Notebook tools
    tools.push(Arc::new(notebook::NotebookReadTool));
    tools.push(Arc::new(notebook::NotebookEditTool));

    // Config tool
    tools.push(Arc::new(config_tool::ConfigTool {
        config: config.clone(),
    }));

    // ToolSearch — built last so it can include all tool names+descriptions
    let snapshot: Vec<(String, String)> = tools
        .iter()
        .map(|t| (t.name().to_string(), t.description().to_string()))
        .collect();
    tools.push(Arc::new(tool_search::ToolSearchTool {
        tools_snapshot: snapshot,
    }));

    let shared = SharedToolState {
        todo: todo_state,
        browser_session: browser_session_shared,
    };
    (tools, shared)
}

/// Build tools + shared state, then append MCP dynamic tools and resource tools.
pub fn all_tools_with_state_and_mcp(
    config: &crate::config::Config,
    mcp_tools: Vec<DynTool>,
    mcp_clients: Vec<Arc<crate::mcp::client::McpClient>>,
) -> (Vec<DynTool>, SharedToolState) {
    let (mut tools, shared) = all_tools_with_state(config);
    tools.extend(mcp_tools);

    // MCP resource tools (need the client list)
    if !mcp_clients.is_empty() {
        tools.push(Arc::new(mcp_resources::ListMcpResourcesTool {
            clients: mcp_clients.clone(),
        }));
        tools.push(Arc::new(mcp_resources::ReadMcpResourceTool {
            clients: mcp_clients,
        }));
    }
    refresh_tool_search(&mut tools);

    (tools, shared)
}

/// Every built-in tool name, including those only some setups build (the
/// browser tools, LSP, agent swarms, MCP resources): the names
/// `--allowed-tools` and `--disallowed-tools` accept besides MCP tools.
pub fn builtin_tool_names(config: &crate::config::Config) -> Vec<String> {
    let all = crate::config::Config {
        browser_enabled: true,
        bare_mode: false,
        ..config.clone()
    };
    let conditional: [DynTool; 5] = [
        Arc::new(send_message::SendMessageTool),
        Arc::new(team_tools::TeamCreateTool),
        Arc::new(team_tools::TeamDeleteTool),
        Arc::new(mcp_resources::ListMcpResourcesTool { clients: vec![] }),
        Arc::new(mcp_resources::ReadMcpResourceTool { clients: vec![] }),
    ];
    let mut names: Vec<String> = all_tools_with_state(&all)
        .0
        .iter()
        .chain(conditional.iter())
        .map(|t| t.name().to_string())
        .collect();
    names.sort();
    names.dedup();
    names
}

#[cfg(test)]
mod sensitive_path_tests {
    use super::*;
    use std::path::PathBuf;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    /// A repo-shipped `gl -> .git` let Write reach `.git/config`
    /// (`core.fsmonitor` runs on the next git command) and create files in a
    /// new directory under `.git`.
    #[cfg(unix)]
    #[tokio::test]
    async fn write_through_a_symlink_into_git_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        std::fs::write(dir.path().join(".git/config"), "[core]\n").unwrap();
        std::os::unix::fs::symlink(".git", dir.path().join("gl")).unwrap();
        let ctx = ToolContext::new(dir.path().to_path_buf());

        for target in ["gl/config", "gl/newdir/x"] {
            assert!(
                check_protected_path(&dir.path().join(target)).is_some(),
                "{target}"
            );
            let out = file_write::FileWriteTool
                .execute(
                    serde_json::json!({"file_path": target, "content": "pwned"}),
                    &ctx,
                )
                .await
                .unwrap();
            assert!(out.is_error, "{target}");
        }
        assert_eq!(
            std::fs::read_to_string(dir.path().join(".git/config")).unwrap(),
            "[core]\n"
        );
        assert!(!dir.path().join(".git/newdir").exists());
        assert!(check_protected_path(&dir.path().join("src/main.rs")).is_none());
    }

    /// `missing/../gl/config`: canonicalize fails up to the project root and
    /// a purely lexical replay gave `gl/config`, hiding that `gl -> .git`.
    #[cfg(unix)]
    #[test]
    fn dotdot_after_a_missing_dir_still_resolves_a_later_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        std::fs::create_dir(root.join(".git")).unwrap();
        std::fs::write(root.join(".git/config"), "[core]\n").unwrap();
        std::os::unix::fs::symlink(".git", root.join("gl")).unwrap();
        let sneaky = root.join("missing/../gl/config");
        assert_eq!(
            resolve_for_sensitivity_check(&sneaky),
            root.join(".git/config")
        );
        assert!(check_protected_path(&sneaky).is_some());

        std::fs::create_dir(root.join(".ssh")).unwrap();
        std::os::unix::fs::symlink(root.join(".ssh"), root.join("keys")).unwrap();
        let sneaky = root.join("missing/../keys/authorized_keys");
        assert!(check_sensitive_path_resolved(&sneaky, SensitiveOp::Write).is_some());

        assert!(check_protected_path(&root.join("missing/../src/main.rs")).is_none());
    }

    #[test]
    fn protected_and_secret_dirs_match_any_case() {
        assert!(check_protected_path(&p("/proj/.GIT/hooks/pre-commit")).is_some());
        assert!(check_protected_path(&p("/proj/.Husky/pre-push")).is_some());
        assert!(
            check_sensitive_path(&p("/home/u/.SSH/authorized_keys"), SensitiveOp::Write).is_some()
        );
        assert!(check_protected_path(&p("/proj/.github/workflows/ci.yml")).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn resolve_sees_through_a_symlink_above_missing_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().canonicalize().unwrap();
        std::fs::create_dir(real.join("target")).unwrap();
        std::os::unix::fs::symlink(real.join("target"), real.join("link")).unwrap();
        assert_eq!(
            resolve_for_sensitivity_check(&real.join("link/a/b")),
            real.join("target/a/b")
        );
    }

    #[test]
    fn write_blocks_dotenv() {
        assert!(check_sensitive_path(&p("/proj/.env"), SensitiveOp::Write).is_some());
        assert!(check_sensitive_path(&p("/proj/.env.local"), SensitiveOp::Write).is_some());
        assert!(check_sensitive_path(&p("/proj/.env.production"), SensitiveOp::Write).is_some());
    }

    #[test]
    fn write_allows_dotenv_templates() {
        assert!(check_sensitive_path(&p("/proj/.env.example"), SensitiveOp::Write).is_none());
        assert!(check_sensitive_path(&p("/proj/.env.sample"), SensitiveOp::Write).is_none());
        assert!(check_sensitive_path(&p("/proj/.env.template"), SensitiveOp::Write).is_none());
    }

    #[test]
    fn write_blocks_ssh_dir() {
        assert!(
            check_sensitive_path(&p("/home/u/.ssh/authorized_keys"), SensitiveOp::Write).is_some()
        );
        assert!(check_sensitive_path(&p("/home/u/.ssh/config"), SensitiveOp::Write).is_some());
    }

    #[test]
    fn write_blocks_aws_creds() {
        assert!(check_sensitive_path(&p("/home/u/.aws/credentials"), SensitiveOp::Write).is_some());
        assert!(check_sensitive_path(&p("/home/u/.aws/config"), SensitiveOp::Write).is_some());
    }

    #[test]
    fn write_blocks_credential_files() {
        assert!(check_sensitive_path(&p("/proj/credentials.json"), SensitiveOp::Write).is_some());
        assert!(check_sensitive_path(&p("/home/u/.netrc"), SensitiveOp::Write).is_some());
        assert!(check_sensitive_path(&p("/home/u/.pgpass"), SensitiveOp::Write).is_some());
    }

    #[test]
    fn read_and_write_block_private_keys() {
        for op in [SensitiveOp::Read, SensitiveOp::Write] {
            assert!(check_sensitive_path(&p("/home/u/.ssh/id_rsa"), op).is_some());
            assert!(check_sensitive_path(&p("/proj/server.pem"), op).is_some());
            assert!(check_sensitive_path(&p("/proj/keystore.jks"), op).is_some());
            assert!(check_sensitive_path(&p("/proj/tls.key"), op).is_some());
        }
    }

    #[test]
    fn read_allows_dotenv_and_credentials() {
        // Read is permissive — user can still inspect secrets via prompts.
        assert!(check_sensitive_path(&p("/proj/.env"), SensitiveOp::Read).is_none());
        assert!(check_sensitive_path(&p("/proj/credentials.json"), SensitiveOp::Read).is_none());
        assert!(check_sensitive_path(&p("/home/u/.aws/config"), SensitiveOp::Read).is_none());
    }

    #[test]
    fn normal_files_pass() {
        assert!(check_sensitive_path(&p("/proj/src/main.rs"), SensitiveOp::Read).is_none());
        assert!(check_sensitive_path(&p("/proj/src/main.rs"), SensitiveOp::Write).is_none());
        assert!(check_sensitive_path(&p("/proj/README.md"), SensitiveOp::Write).is_none());
    }

    // ── Glob-semantics regression tests (Sprint #2 HIGH) ────────────────────
    //
    // The deny-list is implemented with literal equality (`PRIVATE_KEY_NAMES`,
    // `SECRET_WRITE_BLOCK_NAMES`, `SECRET_DIR_COMPONENTS`) and case-insensitive
    // `ends_with` (`PRIVATE_KEY_SUFFIXES`). It does NOT interpret glob
    // metacharacters. A known bug in another tool in this space was adding
    // entries like `"*.pem"` thinking they'd be globbed, which then silently
    // failed to match anything because literal-equality saw the asterisk as
    // part of the filename. These tests lock in the invariant that every
    // entry is a plain literal and that each one actually matches a realistic
    // path.

    /// Static-assertion: no deny-list entry contains glob metacharacters.
    /// If this test fails, someone added a pattern assuming globs are
    /// supported — the match is literal, so the "rule" is a no-op.
    #[test]
    fn no_deny_list_entry_contains_glob_metacharacters() {
        const META: &[char] = &['*', '?', '[', ']', '{', '}'];
        let all_entries: Vec<(&str, &str)> = PRIVATE_KEY_NAMES
            .iter()
            .map(|e| ("PRIVATE_KEY_NAMES", *e))
            .chain(
                PRIVATE_KEY_SUFFIXES
                    .iter()
                    .map(|e| ("PRIVATE_KEY_SUFFIXES", *e)),
            )
            .chain(
                SECRET_WRITE_BLOCK_NAMES
                    .iter()
                    .map(|e| ("SECRET_WRITE_BLOCK_NAMES", *e)),
            )
            .chain(
                SECRET_DIR_COMPONENTS
                    .iter()
                    .map(|e| ("SECRET_DIR_COMPONENTS", *e)),
            )
            .collect();

        for (list, entry) in all_entries {
            for ch in META {
                assert!(
                    !entry.contains(*ch),
                    "{list}: entry {entry:?} contains glob metacharacter {ch:?} — \
                     the deny-list uses literal equality, not glob matching. \
                     Either drop the glob chars or convert the check to a \
                     real glob matcher."
                );
            }
        }
    }

    /// Coverage: every literal entry in the deny-lists actually blocks a
    /// representative path that a well-meaning engineer would expect it to.
    /// If someone silently adds an entry that never fires, this catches it.
    #[test]
    fn every_deny_list_entry_blocks_a_representative_path() {
        // PRIVATE_KEY_NAMES — blocked in both modes.
        for name in PRIVATE_KEY_NAMES {
            let path = p(&format!("/home/u/.ssh/{name}"));
            assert!(
                check_sensitive_path(&path, SensitiveOp::Read).is_some(),
                "PRIVATE_KEY_NAMES entry {name:?} did not block a read of {path:?}"
            );
            assert!(
                check_sensitive_path(&path, SensitiveOp::Write).is_some(),
                "PRIVATE_KEY_NAMES entry {name:?} did not block a write of {path:?}"
            );
        }

        // PRIVATE_KEY_SUFFIXES — blocked in both modes via ends_with.
        for suffix in PRIVATE_KEY_SUFFIXES {
            let path = p(&format!("/proj/server{suffix}"));
            assert!(
                check_sensitive_path(&path, SensitiveOp::Read).is_some(),
                "PRIVATE_KEY_SUFFIXES entry {suffix:?} did not block {path:?}"
            );
            assert!(
                check_sensitive_path(&path, SensitiveOp::Write).is_some(),
                "PRIVATE_KEY_SUFFIXES entry {suffix:?} did not block {path:?}"
            );
        }

        // SECRET_WRITE_BLOCK_NAMES — write only.
        for name in SECRET_WRITE_BLOCK_NAMES {
            let path = p(&format!("/proj/{name}"));
            assert!(
                check_sensitive_path(&path, SensitiveOp::Write).is_some(),
                "SECRET_WRITE_BLOCK_NAMES entry {name:?} did not block a write of {path:?}"
            );
        }

        // SECRET_DIR_COMPONENTS — any file beneath should be write-blocked.
        for dir in SECRET_DIR_COMPONENTS {
            let path = p(&format!("/home/u/{dir}/some_file.txt"));
            assert!(
                check_sensitive_path(&path, SensitiveOp::Write).is_some(),
                "SECRET_DIR_COMPONENTS entry {dir:?} did not block a write of {path:?}"
            );
        }
    }

    /// Case-insensitivity on the suffix side: `FOO.PEM` / `server.Key` /
    /// mixed-case suffix variants must still match PRIVATE_KEY_SUFFIXES
    /// because the check lowercases the filename before comparing.
    #[test]
    fn private_key_suffixes_match_case_insensitively() {
        for case in &["SERVER.PEM", "tls.KEY", "Secrets.P12", "foo.JKS"] {
            let path = p(&format!("/proj/{case}"));
            assert!(
                check_sensitive_path(&path, SensitiveOp::Write).is_some(),
                "uppercase/mixed-case suffix {case:?} must still match the deny-list"
            );
            assert!(
                check_sensitive_path(&path, SensitiveOp::Read).is_some(),
                "uppercase/mixed-case suffix {case:?} must still block reads of private-key material"
            );
        }
    }

    /// A file nested arbitrarily deep inside a secret directory must still
    /// be blocked — the check walks every path component, not just the
    /// immediate parent.
    #[test]
    fn secret_dir_blocks_nested_children() {
        assert!(
            check_sensitive_path(
                &p("/home/u/.aws/sub/dir/deep/creds.toml"),
                SensitiveOp::Write
            )
            .is_some(),
            "deeply nested file under .aws/ must be write-blocked"
        );
        assert!(
            check_sensitive_path(
                &p("/srv/secrets/.gnupg/private-keys-v1.d/ABCDEF.key"),
                SensitiveOp::Write
            )
            .is_some(),
            "nested file under .gnupg/ must be write-blocked"
        );
    }

    /// A path using `..` to traverse INTO a secret directory must still be
    /// blocked. We do not canonicalize (symlink traversal is a separate
    /// threat model), but the component walk should still see the `.ssh`
    /// component in the path.
    #[test]
    fn secret_dir_blocked_through_traversal_component() {
        assert!(
            check_sensitive_path(
                &p("/proj/../home/u/.ssh/authorized_keys"),
                SensitiveOp::Write
            )
            .is_some(),
            ".ssh component in a traversal path must still trip the deny-list"
        );
    }

    /// A literal filename that happens to CONTAIN a deny-list entry as a
    /// substring must NOT be blocked. For example, `my_id_rsa_notes.md` is
    /// not `id_rsa`. This locks in that PRIVATE_KEY_NAMES uses equality,
    /// not substring matching.
    #[test]
    fn private_key_names_require_exact_filename_match() {
        // These are NOT id_rsa — they merely contain the substring.
        assert!(
            check_sensitive_path(&p("/proj/my_id_rsa_notes.md"), SensitiveOp::Read).is_none(),
            "substring match must not block unrelated files"
        );
        assert!(
            check_sensitive_path(&p("/proj/not_id_ed25519.txt"), SensitiveOp::Write).is_none(),
            "substring match must not block unrelated files"
        );
    }
}

#[cfg(test)]
mod tool_search_snapshot_tests {
    use super::*;

    struct FakeMcp;

    #[async_trait]
    impl Tool for FakeMcp {
        fn name(&self) -> &str {
            "mcp__jira__create_issue"
        }
        fn description(&self) -> &str {
            "Create a Jira issue"
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        async fn execute(&self, _: serde_json::Value, _: &ToolContext) -> Result<ToolOutput> {
            Ok(ToolOutput::success(""))
        }
    }

    async fn search(tools: &[DynTool], query: &str) -> String {
        let ts = tools.iter().find(|t| t.name() == "ToolSearch").unwrap();
        let out = ts
            .execute(
                serde_json::json!({"query": query, "max_results": 20}),
                &ToolContext::new(std::env::temp_dir()),
            )
            .await
            .unwrap();
        out.content
            .iter()
            .map(|c| {
                let ToolResultContent::Text { text } = c;
                text.as_str()
            })
            .collect()
    }

    /// The snapshot was taken before MCP tools were appended and before the
    /// filters ran: MCP tools never matched and removed tools still did.
    #[tokio::test]
    async fn tool_search_sees_mcp_tools_and_not_filtered_ones() {
        let cfg = crate::config::Config {
            disallowed_tools: vec!["NotebookEdit".into()],
            ..Default::default()
        };
        let (mut tools, _) = all_tools_with_state_and_mcp(&cfg, vec![Arc::new(FakeMcp)], vec![]);
        assert!(
            search(&tools, "jira")
                .await
                .contains("mcp__jira__create_issue")
        );

        apply_tool_filters(&mut tools, &cfg);
        assert!(tools.iter().any(|t| t.name() == "ToolSearch"));
        let found = search(&tools, "notebook").await;
        assert!(!found.contains("NotebookEdit:"), "{found}");
        assert!(found.contains("NotebookRead:"), "{found}");
    }

    /// `--disallowed-tools mcp__jira` must remove the server's tools, as the
    /// same name does in a permission rule, not match nothing.
    #[test]
    fn mcp_server_names_filter_every_tool_of_the_server() {
        let mcp = || -> Vec<DynTool> { vec![Arc::new(FakeMcp)] };
        let has_jira = |cfg: &crate::config::Config| {
            let (mut tools, _) = all_tools_with_state_and_mcp(cfg, mcp(), vec![]);
            apply_tool_filters(&mut tools, cfg);
            tools.iter().any(|t| t.name() == "mcp__jira__create_issue")
        };
        for name in [
            "mcp__jira",
            "mcp__jira__*",
            "mcp__*",
            "MCP__JIRA__CREATE_ISSUE",
        ] {
            let deny = crate::config::Config {
                disallowed_tools: vec![name.into()],
                ..Default::default()
            };
            assert!(!has_jira(&deny), "--disallowed-tools {name}");
            let allow = crate::config::Config {
                allowed_tools: vec![name.into()],
                ..Default::default()
            };
            assert!(has_jira(&allow), "--allowed-tools {name}");
        }
        let other = crate::config::Config {
            disallowed_tools: vec!["mcp__jir".into()],
            ..Default::default()
        };
        assert!(has_jira(&other));
    }

    /// Every name the tool set can hold is one the flags accept.
    #[test]
    fn builtin_tool_names_cover_the_optional_tools() {
        let names = builtin_tool_names(&crate::config::Config {
            bare_mode: true,
            ..Default::default()
        });
        for n in [
            "Bash",
            "LSP",
            "SendMessage",
            "ListMcpResources",
            "ToolSearch",
        ] {
            assert!(names.iter().any(|x| x == n), "{n} missing: {names:?}");
        }
        assert!(names.iter().any(|x| x == "browser_navigate"), "{names:?}");
    }
}

#[cfg(test)]
mod schema_contract_tests {
    use super::*;

    /// SendUserMessage (upstream's KAIROS-only Brief tool) only streamed its
    /// text: dropped in -p/SDK/ACP, collapsed out of sight in the TUI, yet it
    /// told the model "delivered" and billed itself as the primary output
    /// channel. Answers belong in plain assistant text.
    #[test]
    fn no_tool_swallows_messages_meant_for_the_user() {
        let cfg = crate::config::Config::default();
        let tools = all_tools_with_state(&cfg).0;
        assert!(!tools.iter().any(|t| t.name() == "SendUserMessage"));
    }

    /// Every tool's advertised schema must be internally consistent.
    ///
    /// This is the standing form of a one-off sweep: a tool that advertises a
    /// parameter it does not implement (or requires one it never declares)
    /// silently misleads the model, which then sends inputs that are ignored.
    /// One such bug — `run_in_background` on the Agent tool — was found and
    /// removed in a previous audit; this stops the class returning rather than
    /// catching them one at a time.
    #[test]
    fn every_tool_schema_is_self_consistent() {
        // The full registry, not `default_tools()` — that is a subset, and a
        // schema bug in a tool only reachable via the full set is exactly the
        // kind this is meant to catch.
        let cfg = crate::config::Config::default();
        let tools = all_tools_with_state(&cfg).0;
        assert!(
            tools.len() > 30,
            "expected the full registry, got {} tools",
            tools.len()
        );

        let mut problems: Vec<String> = Vec::new();
        let mut seen: Vec<String> = Vec::new();

        for t in &tools {
            let name = t.name().to_string();

            if name.trim().is_empty() {
                problems.push("a tool has an empty name".into());
            }
            if seen.contains(&name) {
                problems.push(format!("{name}: duplicate tool name in the registry"));
            }
            seen.push(name.clone());

            if t.description().trim().is_empty() {
                problems.push(format!(
                    "{name}: empty description — the model selects on this"
                ));
            }

            let schema = t.input_schema();
            if schema.get("type").and_then(|v| v.as_str()) != Some("object") {
                problems.push(format!("{name}: input_schema must be type=object"));
                continue;
            }

            let props = match schema.get("properties").and_then(|v| v.as_object()) {
                Some(p) => p,
                None => {
                    // A tool taking no input is legitimate, but then it must not
                    // declare anything required either.
                    if schema.get("required").is_some() {
                        problems.push(format!("{name}: has `required` but no `properties`"));
                    }
                    continue;
                }
            };

            for (prop, def) in props {
                if def.get("type").is_none() && def.get("enum").is_none() {
                    problems.push(format!(
                        "{name}.{prop}: property has neither `type` nor `enum`"
                    ));
                }
                if def
                    .get("description")
                    .and_then(|d| d.as_str())
                    .is_none_or(str::is_empty)
                {
                    problems.push(format!(
                        "{name}.{prop}: no description — the model has to guess what it means"
                    ));
                }
            }

            // Anything required must actually be advertised.
            if let Some(req) = schema.get("required").and_then(|v| v.as_array()) {
                for r in req {
                    let Some(r) = r.as_str() else {
                        problems.push(format!("{name}: non-string entry in `required`"));
                        continue;
                    };
                    if !props.contains_key(r) {
                        problems.push(format!(
                            "{name}: `{r}` is required but never declared in properties"
                        ));
                    }
                }
            }
        }

        assert!(
            problems.is_empty(),
            "tool schema contract violations ({}):\n  {}",
            problems.len(),
            problems.join("\n  ")
        );
    }
}
