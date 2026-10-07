/// GrepTool — port of tools/GrepTool/GrepTool.ts
/// Delegates to `rg` (ripgrep) when available, falls back to pure Rust regex.
use super::{Tool, ToolContext, ToolOutput, async_trait};
use anyhow::Result;
use regex::Regex;
use serde::Deserialize;
use serde_json::json;
use std::path::Path;
use tokio::process::Command;
use walkdir::WalkDir;

/// Directories skipped by walkers: VCS metadata + common vendor dirs.
/// Includes `.jj` (Jujutsu) and `.sl` (Sapling) VCS directories.
pub(crate) const EXCLUDED_DIRS: &[&str] = &[
    ".git",
    ".jj",
    ".sl",
    ".hg",
    ".svn",
    ".husky",
    "node_modules",
    "target",
    "dist",
    "build",
    ".next",
    ".cache",
];

pub struct GrepTool;

#[derive(Deserialize)]
struct GrepInput {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    glob: Option<String>,
    #[serde(default)]
    output_mode: Option<OutputMode>,
    #[serde(rename = "-A", default)]
    after: Option<u32>,
    #[serde(rename = "-B", default)]
    before: Option<u32>,
    #[serde(rename = "-C", default)]
    context: Option<u32>,
    #[serde(rename = "-i", default)]
    case_insensitive: bool,
    #[serde(rename = "-n", default)]
    line_numbers: bool,
    #[serde(default)]
    head_limit: Option<usize>,
    #[serde(default)]
    multiline: bool,
}

#[derive(Deserialize, Default, PartialEq)]
#[serde(rename_all = "snake_case")]
enum OutputMode {
    #[default]
    FilesWithMatches,
    Content,
    Count,
}

#[async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &str {
        "Grep"
    }

    fn description(&self) -> &str {
        "Search file contents using regex patterns. Supports full regex syntax. \
        Filter files with glob parameter (e.g. '*.rs', '**/*.ts'). \
        Output modes: 'content' shows matching lines, 'files_with_matches' shows \
        file paths (default), 'count' shows match counts."
    }

    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "The regular expression pattern to search for"
                },
                "path": {
                    "type": "string",
                    "description": "File or directory to search in (defaults to cwd)"
                },
                "glob": {
                    "type": "string",
                    "description": "Glob pattern to filter files (e.g. '*.rs', '*.{ts,tsx}')"
                },
                "output_mode": {
                    "type": "string",
                    "enum": ["content", "files_with_matches", "count"],
                    "description": "Output mode (default: files_with_matches)"
                },
                "-A": { "type": "number", "description": "Lines after each match" },
                "-B": { "type": "number", "description": "Lines before each match" },
                "-C": { "type": "number", "description": "Lines before and after each match" },
                "-i": { "type": "boolean", "description": "Case insensitive search" },
                "-n": { "type": "boolean", "description": "Show line numbers" },
                "head_limit": { "type": "number", "description": "Limit output to first N results" },
                "multiline": { "type": "boolean", "description": "Enable multiline matching" }
            },
            "required": ["pattern"]
        })
    }

    async fn execute(&self, input: serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let input: GrepInput = serde_json::from_value(input)?;

        // Try rg first (faster, handles binary files, respects .gitignore)
        if let Ok(out) = run_with_rg(&input, ctx).await {
            return Ok(out);
        }

        // Fallback: pure Rust regex search
        run_with_regex(&input, ctx).await
    }
}

/// The user's `permissions.deny` Read/Grep rules, which this search must
/// skip inside the directories it walks.
fn read_deny(ctx: &ToolContext) -> crate::permissions::ReadDeny {
    ctx.permission_gate
        .as_ref()
        .map(|g| g.read_deny("Grep"))
        .unwrap_or_default()
}

/// `/`-separated form of a path, for comparing with deny globs.
fn slash_path(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

/// `path` is `dir` or inside it.
fn within(path: &str, dir: &str) -> bool {
    let dir = dir.trim_end_matches('/');
    path == dir || dir.is_empty() || path.strip_prefix(dir).is_some_and(|r| r.starts_with('/'))
}

/// Translate deny globs (absolute) into rg exclusions. rg anchors a
/// leading-`/` glob at its working directory `base`, so a glob under `base`
/// becomes `!/<rel>` (plus `!/<rel>/**`, so a denied directory's contents go
/// too). `None` when a glob could match under `root` but is not under
/// `base`: rg cannot express it and the caller must use the walker.
fn rg_exclusions<'a>(
    patterns: impl Iterator<Item = &'a str>,
    base: &str,
    root: &str,
) -> Option<Vec<String>> {
    let base = base.trim_end_matches('/');
    let mut out = Vec::new();
    for p in patterns {
        if let Some(rel) = p.strip_prefix(base).and_then(|r| r.strip_prefix('/')) {
            if rel.is_empty() {
                return None;
            }
            out.push(format!("!/{rel}"));
            out.push(format!("!/{rel}/**"));
            continue;
        }
        // The literal directory the glob starts with; if neither it nor the
        // search root contains the other, the glob cannot match anything here.
        let lit_end = p.find(['*', '?', '[']).unwrap_or(p.len());
        let lit_dir = p[..lit_end].rsplit_once('/').map_or("", |(d, _)| d);
        if within(root, lit_dir) || within(lit_dir, root) {
            return None;
        }
    }
    Some(out)
}

async fn run_with_rg(input: &GrepInput, ctx: &ToolContext) -> Result<ToolOutput> {
    let mut args: Vec<String> = Vec::new();

    match &input.output_mode {
        None | Some(OutputMode::FilesWithMatches) => args.push("-l".into()),
        Some(OutputMode::Content) => {} // default rg output (matching lines)
        Some(OutputMode::Count) => args.push("-c".into()),
    }

    // Content mode — explicit (no -l)
    if input.output_mode == Some(OutputMode::Content) {
        args.retain(|a| a != "-l");
    }

    if input.case_insensitive {
        args.push("-i".into());
    }
    if input.multiline {
        args.push("-U".into());
        args.push("--multiline-dotall".into());
    }
    if let Some(b) = input.before.or(input.context) {
        args.push(format!("-B{b}"));
    }
    if let Some(a) = input.after.or(input.context) {
        args.push(format!("-A{a}"));
    }
    if let Some(g) = &input.glob {
        args.push("--glob".into());
        args.push(g.clone());
    }

    let search_path = match &input.path {
        Some(p) => {
            let p = Path::new(p);
            if p.is_absolute() {
                p.to_path_buf()
            } else {
                ctx.cwd.join(p)
            }
        }
        None => ctx.cwd.clone(),
    };

    // rg skips dotfiles by default, which hid .github/, .eslintrc, .vscode/
    // and the like from every search; the walker fallback searches them.
    // VCS metadata stays out through the exclusions below.
    args.push("--hidden".into());

    // Exclude VCS metadata and common vendor dirs (v2.1.92: added .jj and .sl),
    // except one the search path itself names: `path: "node_modules/react"`
    // asked for it, and the exclusion would drop every file under it and read
    // as "No matches found.". rg matches globs against paths relative to its
    // working directory, so only the components below the cwd count.
    let named: Vec<String> = search_path
        .strip_prefix(&ctx.cwd)
        .unwrap_or(&search_path)
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    for excl in EXCLUDED_DIRS {
        if named.iter().any(|c| c == excl) {
            continue;
        }
        args.push("--glob".into());
        args.push(format!("!**/{excl}/**"));
    }

    // Same read deny-list the fallback backend and FileRead enforce. rg opens
    // files itself, so exclusions have to be declared rather than checked.
    for (flag, g) in super::denied_read_globs() {
        args.push(flag.into());
        args.push(g);
    }

    args.push("--".into());
    args.push(input.pattern.clone());

    // rg applies `--glob` exclusions only while walking: a denied file named
    // directly (`path: "~/.ssh/id_rsa"`) is searched and printed regardless.
    if let Some(err) = super::check_sensitive_path_resolved(&search_path, super::SensitiveOp::Read)
    {
        return Ok(err);
    }
    let deny = read_deny(ctx);
    if deny.denies(&search_path) {
        return Ok(ToolOutput::error(
            "Permission denied: a permissions.deny rule covers this path.",
        ));
    }
    let mut search_arg = search_path.clone();
    let mut rg_cwd = ctx.cwd.clone();
    if !deny.is_empty() && search_path.is_dir() {
        // rg matches its globs against paths relative to the directory it
        // runs in (as the OS reports it, symlinks resolved), so both sides
        // must be canonical. Windows canonical paths carry a `\\?\` prefix
        // rg does not use: leave those searches to the walker.
        if cfg!(windows) {
            anyhow::bail!("deny rules are enforced by the walker on Windows");
        }
        let (Ok(base), Ok(root)) = (
            std::fs::canonicalize(&ctx.cwd),
            std::fs::canonicalize(&search_path),
        ) else {
            anyhow::bail!("cannot resolve the search root for deny rules");
        };
        let Some(excl) = rg_exclusions(deny.patterns(), &slash_path(&base), &slash_path(&root))
        else {
            anyhow::bail!("deny rules need the walker for this search root");
        };
        let flag = if crate::permissions::PATH_RULES_FOLD_CASE {
            "--iglob"
        } else {
            "--glob"
        };
        // Before `--` and the pattern, like the other globs.
        let at = args.iter().position(|a| a == "--").unwrap_or(args.len());
        for g in excl.into_iter().rev() {
            args.insert(at, g);
            args.insert(at, flag.into());
        }
        search_arg = root;
        rg_cwd = base;
    }
    args.push(search_arg.to_string_lossy().into_owned());

    let output = Command::new("rg")
        .args(&args)
        .current_dir(&rg_cwd)
        .output()
        .await?;

    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();

    if let Some(limit) = input.head_limit {
        let lines: Vec<&str> = text.lines().take(limit).collect();
        text = lines.join("\n");
    }

    if text.trim().is_empty() {
        return Ok(ToolOutput::success("No matches found."));
    }

    Ok(ToolOutput::success(text))
}

async fn run_with_regex(input: &GrepInput, ctx: &ToolContext) -> Result<ToolOutput> {
    let pattern = if input.case_insensitive {
        format!("(?i){}", input.pattern)
    } else {
        input.pattern.clone()
    };

    let re = Regex::new(&pattern)?;

    let search_path = match &input.path {
        Some(p) => {
            let p = Path::new(p);
            if p.is_absolute() {
                p.to_path_buf()
            } else {
                ctx.cwd.join(p)
            }
        }
        None => ctx.cwd.clone(),
    };

    let deny = read_deny(ctx);
    if deny.denies(&search_path) {
        return Ok(ToolOutput::error(
            "Permission denied: a permissions.deny rule covers this path.",
        ));
    }

    let glob_re = input.glob.as_ref().and_then(|g| {
        let escaped = regex::escape(g)
            .replace(r"\*\*", ".*")
            .replace(r"\*", "[^/]*")
            .replace(r"\?", "[^/]");
        Regex::new(&format!("(?i){}$", escaped)).ok()
    });

    let mut matched_files: Vec<String> = Vec::new();
    let mut content_lines: Vec<String> = Vec::new();

    for entry in WalkDir::new(&search_path)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| {
            // The user's deny rules prune whole directories, like rg's globs.
            if deny.denies(e.path()) {
                return false;
            }
            // Skip VCS metadata and common vendor dirs — matches rg's default
            // ignore set plus .jj / .sl (v2.1.92 fix).
            // The search root itself is what the caller asked for, even when
            // it is `build` or `node_modules`.
            if e.depth() > 0 && e.file_type().is_dir() {
                let name = e.file_name().to_string_lossy();
                !EXCLUDED_DIRS.contains(&name.as_ref())
            } else {
                true
            }
        })
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
    {
        let path = entry.path();
        let path_str = path.to_string_lossy();

        // Apply glob filter
        if let Some(ref gre) = glob_re
            && !gre.is_match(&path_str)
        {
            continue;
        }

        // Honour the same read deny-list the FileRead tool enforces. Grep
        // returns matching lines verbatim, so without this it is a read
        // primitive that bypasses the guard entirely — verified: a search for a
        // string inside `server.pem` returned the key material, while FileRead
        // on the same file was correctly refused.
        if super::check_sensitive_path_resolved(path, super::SensitiveOp::Read).is_some() {
            continue;
        }

        let Ok(contents) = tokio::fs::read_to_string(path).await else {
            continue;
        };

        let mut file_matched = false;
        let mut file_count = 0usize;

        for (i, line) in contents.lines().enumerate() {
            if re.is_match(line) {
                file_matched = true;
                file_count += 1;
                if input.output_mode == Some(OutputMode::Content) {
                    content_lines.push(if input.line_numbers {
                        format!("{}:{}: {}", path_str, i + 1, line)
                    } else {
                        format!("{}: {}", path_str, line)
                    });
                }
            }
        }

        if file_matched {
            match input.output_mode {
                Some(OutputMode::Count) => {
                    content_lines.push(format!("{}: {}", path_str, file_count));
                }
                None | Some(OutputMode::FilesWithMatches) => {
                    matched_files.push(path_str.into_owned());
                }
                _ => {}
            }
        }
    }

    let mut output = match input.output_mode {
        Some(OutputMode::Content) => content_lines.join("\n"),
        Some(OutputMode::Count) => content_lines.join("\n"),
        _ => matched_files.join("\n"),
    };

    if let Some(limit) = input.head_limit {
        let lines: Vec<&str> = output.lines().take(limit).collect();
        output = lines.join("\n");
    }

    if output.trim().is_empty() {
        return Ok(ToolOutput::success("No matches found."));
    }

    Ok(ToolOutput::success(output))
}

#[cfg(test)]
mod rg_exclusion_tests {
    use super::rg_exclusions;

    #[test]
    fn globs_under_rgs_directory_are_anchored_there() {
        let pats = ["/proj/secrets", "/proj/**/.npmrc", "/home/u/.ssh/**"];
        let got = rg_exclusions(pats.into_iter(), "/proj", "/proj").unwrap();
        assert_eq!(
            got,
            ["!/secrets", "!/secrets/**", "!/**/.npmrc", "!/**/.npmrc/**"]
        );
        // A search of a subdirectory still runs in /proj.
        assert!(rg_exclusions(pats.into_iter(), "/proj", "/proj/src").is_some());
    }

    #[test]
    fn a_glob_rg_cannot_anchor_falls_back_to_the_walker() {
        // Searching ~ from /proj: the ~/.ssh rule applies but is not under /proj.
        assert!(rg_exclusions(["/home/u/.ssh/**"].into_iter(), "/proj", "/home/u").is_none());
        assert!(rg_exclusions(["/proj"].into_iter(), "/proj", "/proj").is_none());
    }
}

#[cfg(test)]
mod deny_rule_tests {
    use super::*;
    use crate::permissions::{PermissionGate, PermissionState};

    fn setup() -> (tempfile::TempDir, ToolContext) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("secrets")).unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join("web")).unwrap();
        std::fs::write(root.join("secrets/prod.yml"), "token: NEEDLE-prod\n").unwrap();
        std::fs::write(root.join("web/creds.txt"), "NEEDLE-creds\n").unwrap();
        std::fs::write(root.join("src/a.txt"), "NEEDLE-src\n").unwrap();
        let deny = vec!["Read(./secrets)".to_string(), "Read(creds.txt)".to_string()];
        let mut ctx = ToolContext::new(root.to_path_buf());
        ctx.permission_gate = Some(PermissionGate::new(
            PermissionState::new(false, &[], &deny).with_cwd(root),
            false,
            None,
        ));
        (dir, ctx)
    }

    fn content(ctx_input: serde_json::Value) -> GrepInput {
        serde_json::from_value(ctx_input).unwrap()
    }

    fn text(out: &ToolOutput) -> String {
        out.content
            .iter()
            .map(|c| {
                let super::super::ToolResultContent::Text { text } = c;
                text.as_str()
            })
            .collect()
    }

    /// `Read(./secrets)` only stopped a Grep whose path named the denied
    /// directory; a project-wide search printed `secrets/prod.yml`.
    #[tokio::test]
    async fn a_directory_search_skips_what_read_rules_deny() {
        let (_dir, ctx) = setup();
        let input = content(json!({"pattern": "NEEDLE", "output_mode": "content"}));
        let mut outs = vec![run_with_regex(&input, &ctx).await.unwrap()];
        // On Windows deny rules always go to the walker (run_with_rg bails).
        let rg = std::process::Command::new("rg").arg("--version").output();
        if !cfg!(windows) && rg.is_ok_and(|o| o.status.success()) {
            outs.push(run_with_rg(&input, &ctx).await.unwrap());
        }
        for out in &outs {
            let t = text(out);
            assert!(t.contains("NEEDLE-src"), "{t}");
            assert!(!t.contains("NEEDLE-prod"), "{t}");
            assert!(!t.contains("NEEDLE-creds"), "{t}");
        }

        let ctx_glob = ctx;
        let out = super::super::glob::GlobTool
            .execute(json!({"pattern": "**/*"}), &ctx_glob)
            .await
            .unwrap();
        let t = text(&out);
        assert!(t.contains("a.txt"), "{t}");
        assert!(!t.contains("prod.yml"), "{t}");
        assert!(!t.contains("creds.txt"), "{t}");
    }
}

#[cfg(test)]
mod search_scope_tests {
    use super::*;

    fn text(out: &ToolOutput) -> String {
        out.content
            .iter()
            .map(|c| {
                let super::super::ToolResultContent::Text { text } = c;
                text.as_str()
            })
            .collect()
    }

    fn write(root: &Path, rel: &str, body: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    fn has_rg() -> bool {
        std::process::Command::new("rg")
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
    }

    /// Both backends, rg only where it is installed.
    async fn both(ctx: &ToolContext, input: serde_json::Value) -> Vec<String> {
        let input: GrepInput = serde_json::from_value(input).unwrap();
        let mut outs = vec![text(&run_with_regex(&input, ctx).await.unwrap())];
        if has_rg() {
            outs.push(text(&run_with_rg(&input, ctx).await.unwrap()));
        }
        outs
    }

    /// rg ran without --hidden, so .github/ and .eslintrc never matched.
    #[tokio::test]
    async fn hidden_files_are_searched_but_vcs_metadata_is_not() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, ".github/workflows/ci.yml", "needle\n");
        write(root, ".eslintrc", "needle\n");
        write(root, ".git/config", "needle\n");
        write(root, "src/a.rs", "needle\n");
        let ctx = ToolContext::new(root.to_path_buf());
        for t in both(&ctx, json!({"pattern": "needle"})).await {
            assert!(t.contains("ci.yml") && t.contains(".eslintrc"), "{t}");
            assert!(t.contains("a.rs") && !t.contains("config"), "{t}");
        }
    }

    /// A search rooted in node_modules/ or build/ answered "No matches
    /// found." on both backends; a project under /build/ must still work.
    #[tokio::test]
    async fn an_explicit_vendor_dir_path_is_searched() {
        let dir = tempfile::tempdir().unwrap();
        let proj = dir.path().join("build/app");
        write(&proj, "node_modules/react/index.js", "useState\n");
        write(&proj, "build/out.js", "useState\n");
        write(&proj, "src/app.js", "useState\n");
        let ctx = ToolContext::new(proj.clone());

        for t in both(&ctx, json!({"pattern": "useState"})).await {
            assert!(t.contains("app.js"), "{t}");
            assert!(!t.contains("index.js") && !t.contains("out.js"), "{t}");
        }
        for t in both(
            &ctx,
            json!({"pattern": "useState", "path": "node_modules/react"}),
        )
        .await
        {
            assert!(t.contains("index.js"), "{t}");
        }
        for t in both(&ctx, json!({"pattern": "useState", "path": "build"})).await {
            assert!(t.contains("out.js"), "{t}");
        }
    }
}
