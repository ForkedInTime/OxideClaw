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

    // Exclude VCS metadata and common vendor dirs (v2.1.92: added .jj and .sl).
    for excl in EXCLUDED_DIRS {
        args.push("--glob".into());
        args.push(format!("!**/{excl}/**"));
    }

    // Same read deny-list the fallback backend and FileRead enforce. rg opens
    // files itself, so exclusions have to be declared rather than checked.
    for g in super::denied_read_globs() {
        args.push("--glob".into());
        args.push(g);
    }

    args.push("--".into());
    args.push(input.pattern.clone());

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
    args.push(search_path.to_string_lossy().into_owned());

    let output = Command::new("rg")
        .args(&args)
        .current_dir(&ctx.cwd)
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
            // Skip VCS metadata and common vendor dirs — matches rg's default
            // ignore set plus .jj / .sl (v2.1.92 fix).
            if e.file_type().is_dir() {
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
