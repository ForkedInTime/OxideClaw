/// GlobTool — port of tools/GlobTool/GlobTool.ts
/// Fast file pattern matching, results sorted by modification time.
use super::{Tool, ToolContext, ToolOutput, async_trait};
use anyhow::Result;
use glob::{MatchOptions, Pattern};
use serde::Deserialize;
use serde_json::json;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use walkdir::WalkDir;

/// Cap on returned matches. Generous enough that ordinary searches are never
/// truncated, low enough that a repo-wide `**/*` cannot produce a multi-megabyte
/// tool result.
const MAX_GLOB_RESULTS: usize = 1000;

/// Matches collected before the walk stops. Sorting needs more than the
/// returned 1000 to pick the newest, but a `**/*` over a huge tree must not
/// grow memory without bound.
const MAX_SCANNED_MATCHES: usize = 10 * MAX_GLOB_RESULTS;

/// Cap on the patterns one `{a,b}` expansion may produce, so a pattern like
/// `{a,b}{c,d}{e,f}...` cannot turn one call into thousands of tree walks.
const MAX_BRACE_PATTERNS: usize = 256;

/// Expand `{a,b}` alternations (nested ones too) into separate patterns. The
/// glob crate treats braces as literal characters, so `**/*.{ts,tsx}` would
/// otherwise only match a file literally named `x.{ts,tsx}`. As in bash, a
/// brace group without a top-level comma or without a closing brace stays
/// literal.
pub(crate) fn expand_braces(pattern: &str) -> Vec<String> {
    let mut start = 0;
    while let Some(off) = pattern[start..].find('{') {
        let open = start + off;
        let mut depth = 0usize;
        let mut bounds = vec![open];
        let mut close = None;
        for (i, ch) in pattern[open..].char_indices() {
            match ch {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        close = Some(open + i);
                        break;
                    }
                }
                ',' if depth == 1 => bounds.push(open + i),
                _ => {}
            }
        }
        let Some(close) = close.filter(|_| bounds.len() > 1) else {
            start = open + 1;
            continue;
        };
        bounds.push(close);
        let (prefix, suffix) = (&pattern[..open], &pattern[close + 1..]);
        let mut out = Vec::new();
        for w in bounds.windows(2) {
            let alt = &pattern[w[0] + 1..w[1]];
            out.extend(expand_braces(&format!("{prefix}{alt}{suffix}")));
            if out.len() >= MAX_BRACE_PATTERNS {
                break;
            }
        }
        out.truncate(MAX_BRACE_PATTERNS);
        return out;
    }
    vec![pattern.to_string()]
}

/// Split `pattern` into the directory to walk and the part left to match
/// against paths relative to it: leading wildcard-free components join the
/// root (so `src/**/*.rs` never visits `docs/`), and a base path containing
/// `[` or `*` is never read as a pattern. The last component always stays in
/// the pattern so a fully literal one (`Cargo.toml`) still matches.
fn split_pattern(base: &Path, pattern: &str) -> (PathBuf, String) {
    let (mut root, rest) = match pattern.strip_prefix('/') {
        Some(rest) => (PathBuf::from("/"), rest),
        None => (base.to_path_buf(), pattern),
    };
    let comps: Vec<&str> = rest.split('/').filter(|c| !c.is_empty()).collect();
    let literal = comps
        .iter()
        .take_while(|c| !c.contains(['*', '?', '[']))
        .count()
        .min(comps.len().saturating_sub(1));
    for c in &comps[..literal] {
        root.push(c);
    }
    (root, comps[literal..].join("/"))
}

pub struct GlobTool;

#[derive(Deserialize)]
struct GlobInput {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
}

#[async_trait]
impl Tool for GlobTool {
    fn name(&self) -> &str {
        "Glob"
    }

    fn description(&self) -> &str {
        "Find files matching a glob pattern. Returns matching file paths sorted by \
        modification time (most recently modified first). Use patterns like '**/*.rs', \
        'src/**/*.ts', or '*.json'. Optionally specify a directory to search in."
    }

    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Glob pattern to match (e.g. '**/*.rs', 'src/**/*.ts')"
                },
                "path": {
                    "type": "string",
                    "description": "Directory to search in (defaults to current working directory)"
                }
            },
            "required": ["pattern"]
        })
    }

    async fn execute(&self, input: serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let input: GlobInput = serde_json::from_value(input)?;

        let base = match super::file_read::resolve_search_path(input.path.as_deref(), &ctx.cwd) {
            Ok(p) => p,
            Err(e) => return Ok(ToolOutput::error(e.to_string())),
        };

        // One walk per `{a,b}` alternative, each from its own literal prefix.
        let mut walks: Vec<(PathBuf, Pattern, usize)> = Vec::new();
        for alt in expand_braces(&input.pattern) {
            let (root, rel_pattern) = split_pattern(&base, &alt);
            let pattern = Pattern::new(&rel_pattern)
                .map_err(|e| anyhow::anyhow!("Invalid glob pattern: {e}"))?;
            let max_depth = if rel_pattern.contains("**") {
                usize::MAX
            } else {
                rel_pattern.split('/').count()
            };
            walks.push((root, pattern, max_depth));
        }
        // `*` and `?` stay within one path component, as when the pattern was
        // expanded directory by directory; only `**` crosses separators.
        let options = MatchOptions {
            case_sensitive: true,
            require_literal_separator: true,
            require_literal_leading_dot: false,
        };

        // What the user's deny rules keep from Glob: the per-call check only
        // sees the base directory, not the files listed under it.
        let deny = ctx
            .permission_gate
            .as_ref()
            .map(|g| g.read_deny("Glob", &ctx.cwd))
            .unwrap_or_default();

        // walkdir, not `glob::glob_with`: glob follows directory symlinks with
        // no visited set, so two `up -> ..` links made `**` exponential, and
        // it collected every match before the cap. walkdir still follows links
        // but refuses one that leads back to an ancestor. Off the async
        // runtime, since the walk has no await point.
        let walk = tokio::task::spawn_blocking(move || {
            let mut seen: HashSet<PathBuf> = HashSet::new();
            let mut entries: Vec<(SystemTime, String)> = Vec::new();
            for (root, pattern, max_depth) in &walks {
                let walker = WalkDir::new(root)
                    .follow_links(true)
                    .max_depth(*max_depth)
                    .into_iter()
                    .filter_entry(|e| {
                        if e.depth() == 0 {
                            return true;
                        }
                        if deny.denies(e.path()) {
                            return false;
                        }
                        // Skip VCS metadata and common vendor dirs (v2.1.92: + .jj, .sl).
                        !(e.file_type().is_dir()
                            && crate::tools::grep::EXCLUDED_DIRS
                                .contains(&e.file_name().to_string_lossy().as_ref()))
                    });
                for entry in walker.filter_map(|e| e.ok()) {
                    if entry.depth() == 0 || entry.file_type().is_dir() {
                        continue;
                    }
                    let path = entry.path();
                    let Ok(rel) = path.strip_prefix(root) else {
                        continue;
                    };
                    if !pattern.matches_path_with(rel, options) || !seen.insert(path.to_path_buf())
                    {
                        continue;
                    }
                    let mtime = entry
                        .metadata()
                        .ok()
                        .and_then(|m| m.modified().ok())
                        .unwrap_or(SystemTime::UNIX_EPOCH);
                    entries.push((mtime, path.display().to_string()));
                    if entries.len() >= MAX_SCANNED_MATCHES {
                        return (entries, true);
                    }
                }
            }
            (entries, false)
        });
        let (mut entries, walk_capped) = walk.await?;

        // Sort by modification time, most recent first
        entries.sort_by_key(|e| std::cmp::Reverse(e.0));

        // Bound the result. A broad pattern over a large tree (`**/*`) otherwise
        // builds an unbounded Vec and then joins it into one enormous string
        // that is sent to the model as a tool result — cost and memory scale
        // with the repository, and nothing downstream caps it. Sorting first
        // means the cap keeps the most recently modified matches, which is what
        // the ordering exists to surface.
        let total = entries.len();
        let truncated = total > MAX_GLOB_RESULTS;
        let total = if walk_capped {
            format!("{total}+")
        } else {
            total.to_string()
        };
        entries.truncate(MAX_GLOB_RESULTS);

        if entries.is_empty() {
            return Ok(ToolOutput::success("No files matched the pattern."));
        }

        let mut output = entries
            .into_iter()
            .map(|(_, path)| path)
            .collect::<Vec<_>>()
            .join("\n");

        if truncated {
            output.push_str(&format!(
                "\n\n... {} of {total} matches shown (most recently modified first). \
                 Narrow the pattern or search a subdirectory to see the rest.",
                MAX_GLOB_RESULTS
            ));
        }

        Ok(ToolOutput::success(output))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    async fn glob(ctx: &ToolContext, input: serde_json::Value) -> String {
        let out = GlobTool.execute(input, ctx).await.unwrap();
        out.content
            .iter()
            .map(|c| match c {
                crate::api::types::ToolResultContent::Text { text } => text.as_str(),
            })
            .collect()
    }

    /// Two `up -> ..` links made glob's `**` walk exponentially (957k matches
    /// in 10 s and climbing) with no way to cancel it.
    #[cfg(unix)]
    #[tokio::test]
    async fn symlink_cycles_do_not_explode_the_walk() {
        let dir = tempfile::tempdir().unwrap();
        for d in ["a", "b"] {
            fs::create_dir(dir.path().join(d)).unwrap();
            for f in ["1.txt", "2.txt"] {
                fs::write(dir.path().join(d).join(f), "x").unwrap();
            }
            std::os::unix::fs::symlink("..", dir.path().join(d).join("up")).unwrap();
        }
        let ctx = ToolContext::new(dir.path().to_path_buf());
        let out = tokio::time::timeout(
            std::time::Duration::from_secs(20),
            glob(&ctx, json!({"pattern": "**/*.txt"})),
        )
        .await
        .expect("glob over a symlink cycle must finish");
        assert_eq!(out.lines().count(), 4, "{out}");
    }

    #[tokio::test]
    async fn pattern_semantics_match_directory_by_directory_globbing() {
        let dir = tempfile::tempdir().unwrap();
        // A base directory whose name is itself glob syntax.
        let base = dir.path().join("proj[1]");
        fs::create_dir_all(base.join("src/nested")).unwrap();
        fs::create_dir_all(base.join("node_modules/dep")).unwrap();
        fs::write(base.join("top.rs"), "").unwrap();
        fs::write(base.join("Cargo.toml"), "").unwrap();
        fs::write(base.join("src/lib.rs"), "").unwrap();
        fs::write(base.join("src/nested/deep.rs"), "").unwrap();
        fs::write(base.join("node_modules/dep/vendored.rs"), "").unwrap();
        let ctx = ToolContext::new(base.clone());

        // `*` stays within one directory.
        let out = glob(&ctx, json!({"pattern": "*.rs"})).await;
        assert_eq!(out, base.join("top.rs").display().to_string());

        let out = glob(&ctx, json!({"pattern": "src/*.rs"})).await;
        assert_eq!(out, base.join("src/lib.rs").display().to_string());

        // `**` also matches zero directories, and vendor dirs are skipped.
        let out = glob(&ctx, json!({"pattern": "**/*.rs"})).await;
        let mut lines: Vec<&str> = out.lines().collect();
        lines.sort();
        let mut want = vec![
            base.join("src/lib.rs").display().to_string(),
            base.join("src/nested/deep.rs").display().to_string(),
            base.join("top.rs").display().to_string(),
        ];
        want.sort();
        assert_eq!(lines, want);

        // A fully literal pattern, and an absolute one.
        let out = glob(&ctx, json!({"pattern": "Cargo.toml"})).await;
        assert_eq!(out, base.join("Cargo.toml").display().to_string());
        let plain = dir.path().join("plain");
        fs::create_dir_all(plain.join("x/y")).unwrap();
        fs::write(plain.join("x/y/abs.rs"), "").unwrap();
        let abs = format!("{}/**/abs.rs", plain.display());
        let out = glob(&ctx, json!({ "pattern": abs })).await;
        assert_eq!(out, plain.join("x/y/abs.rs").display().to_string());

        // `path` as base.
        let out = glob(&ctx, json!({"pattern": "*.rs", "path": "src/nested"})).await;
        assert_eq!(out, base.join("src/nested/deep.rs").display().to_string());
    }

    fn touch(root: &Path, rel: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, "x").unwrap();
    }

    #[test]
    fn braces_expand_nested_and_leave_unbalanced_literal() {
        assert_eq!(expand_braces("**/*.{ts,tsx}"), ["**/*.ts", "**/*.tsx"]);
        assert_eq!(
            expand_braces("{src,lib/{a,b}}/*.rs"),
            ["src/*.rs", "lib/a/*.rs", "lib/b/*.rs"]
        );
        assert_eq!(expand_braces("a{b"), ["a{b"]);
        assert_eq!(expand_braces("{x}.rs"), ["{x}.rs"]);
        assert_eq!(expand_braces("{,.}env"), ["env", ".env"]);
    }

    /// `**/*.{ts,tsx}` returned "No files matched" because the glob crate
    /// has no brace syntax.
    #[tokio::test]
    async fn brace_patterns_match_each_alternative_once() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        touch(root, "src/a.ts");
        touch(root, "src/b.tsx");
        touch(root, "src/c.js");
        let ctx = ToolContext::new(root.to_path_buf());
        let t = glob(&ctx, json!({"pattern": "**/*.{ts,tsx}"})).await;
        assert!(t.contains("a.ts") && t.contains("b.tsx"), "{t}");
        assert!(!t.contains("c.js"), "{t}");
        let t = glob(&ctx, json!({"pattern": "**/{a,a}.ts"})).await;
        assert_eq!(t.lines().count(), 1, "{t}");
    }

    /// A `path` naming a Next.js `[slug]` route was spliced in as a
    /// character class and never matched the directory.
    #[tokio::test]
    async fn base_path_with_brackets_is_literal() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        touch(root, "app/[slug]/page.tsx");
        let ctx = ToolContext::new(root.to_path_buf());
        let t = glob(&ctx, json!({"pattern": "*.tsx", "path": "app/[slug]"})).await;
        assert!(t.contains("page.tsx"), "{t}");
    }

    /// Every component of the absolute match was checked against the
    /// vendor-dir list, so a project under `/build/...` or an explicit
    /// `path: node_modules/x` matched nothing.
    #[tokio::test]
    async fn vendor_dirs_are_skipped_only_below_the_requested_root() {
        let dir = tempfile::tempdir().unwrap();
        let proj = dir.path().join("build/app");
        touch(&proj, "src/main.rs");
        touch(&proj, "node_modules/x/index.js");
        touch(&proj, "target/debug/gen.rs");
        let ctx = ToolContext::new(proj.clone());

        let t = glob(&ctx, json!({"pattern": "**/*.rs"})).await;
        assert!(t.contains("main.rs"), "{t}");
        assert!(!t.contains("gen.rs"), "{t}");
        let t = glob(&ctx, json!({"pattern": "**/*.js"})).await;
        assert!(!t.contains("index.js"), "{t}");

        let t = glob(
            &ctx,
            json!({"pattern": "**/*.js", "path": "node_modules/x"}),
        )
        .await;
        assert!(t.contains("index.js"), "{t}");
        let t = glob(&ctx, json!({"pattern": "target/debug/*.rs"})).await;
        assert!(t.contains("gen.rs"), "{t}");
    }
}
