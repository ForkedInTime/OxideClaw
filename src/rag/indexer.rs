/// tree-sitter based code indexer.
///
/// Walks the project, parses source files with language-specific grammars,
/// extracts symbols (functions, structs, classes, impls and their methods),
/// and stores them as searchable chunks in the RAG database.
///
/// Incremental: only re-indexes files whose mtime changed since last index.
/// Gitignore-aware: anything git would not track never reaches the index,
/// so a gitignored `config.local.js` full of keys is never sent to a model.
use anyhow::Result;
use std::collections::HashMap;
use std::path::Path;
use std::time::Instant;
use tracing::{debug, warn};

use super::RagDb;

// ─── Language registry ───────────────────────────────────────────────────────

/// Supported languages and their file extensions.
struct LangDef {
    name: &'static str,
    extensions: &'static [&'static str],
    /// tree-sitter node types to extract as top-level symbols.
    /// These are language-specific AST node type names.
    symbol_nodes: &'static [&'static str],
}

static LANGUAGES: &[LangDef] = &[
    LangDef {
        name: "rust",
        extensions: &["rs"],
        symbol_nodes: &[
            "function_item",
            "struct_item",
            "enum_item",
            "impl_item",
            "trait_item",
            "type_item",
            "const_item",
            "static_item",
            "macro_definition",
            "mod_item",
        ],
    },
    LangDef {
        name: "javascript",
        extensions: &["js", "jsx", "mjs", "cjs"],
        symbol_nodes: &[
            "function_declaration",
            "class_declaration",
            "export_statement",
            "lexical_declaration",
            "variable_declaration",
            "arrow_function",
            "method_definition",
        ],
    },
    LangDef {
        name: "typescript",
        extensions: &["ts", "tsx"],
        symbol_nodes: &[
            "function_declaration",
            "class_declaration",
            "interface_declaration",
            "type_alias_declaration",
            "enum_declaration",
            "export_statement",
            "lexical_declaration",
            "method_definition",
        ],
    },
    LangDef {
        name: "python",
        extensions: &["py"],
        symbol_nodes: &[
            "function_definition",
            "class_definition",
            "decorated_definition",
        ],
    },
    LangDef {
        name: "go",
        extensions: &["go"],
        symbol_nodes: &[
            "function_declaration",
            "method_declaration",
            "type_declaration",
            "const_declaration",
            "var_declaration",
        ],
    },
    LangDef {
        name: "c",
        extensions: &["c", "h"],
        symbol_nodes: &[
            "function_definition",
            "struct_specifier",
            "enum_specifier",
            "type_definition",
            "declaration",
            "preproc_function_def",
        ],
    },
    LangDef {
        name: "java",
        extensions: &["java"],
        symbol_nodes: &[
            "class_declaration",
            "method_declaration",
            "interface_declaration",
            "enum_declaration",
            "constructor_declaration",
        ],
    },
    LangDef {
        name: "bash",
        extensions: &["sh", "bash"],
        symbol_nodes: &["function_definition"],
    },
];

/// Build extension → language name lookup.
fn ext_to_lang() -> HashMap<&'static str, &'static str> {
    let mut m = HashMap::new();
    for lang in LANGUAGES {
        for ext in lang.extensions {
            m.insert(*ext, lang.name);
        }
    }
    m
}

/// Get the tree-sitter Language for a language name.
fn get_ts_language(name: &str) -> Option<tree_sitter::Language> {
    match name {
        "rust" => Some(tree_sitter_rust::LANGUAGE.into()),
        "javascript" => Some(tree_sitter_javascript::LANGUAGE.into()),
        "typescript" => Some(tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()),
        "python" => Some(tree_sitter_python::LANGUAGE.into()),
        "go" => Some(tree_sitter_go::LANGUAGE.into()),
        "c" => Some(tree_sitter_c::LANGUAGE.into()),
        "java" => Some(tree_sitter_java::LANGUAGE.into()),
        "bash" => Some(tree_sitter_bash::LANGUAGE.into()),
        _ => None,
    }
}

/// Get the symbol node types for a language.
fn get_symbol_nodes(name: &str) -> &'static [&'static str] {
    LANGUAGES
        .iter()
        .find(|l| l.name == name)
        .map(|l| l.symbol_nodes)
        .unwrap_or(&[])
}

// ─── Chunk extraction ────────────────────────────────────────────────────────

/// A code chunk extracted from a source file.
pub struct CodeChunk {
    pub file_path: String,
    pub symbol_name: String,
    pub symbol_kind: String,
    pub language: String,
    pub start_line: i64,
    pub end_line: i64,
    pub content: String,
}

/// Extract symbol name from a tree-sitter node.
/// The node's `name` field, else its first `identifier` or `name` child.
fn extract_symbol_name(node: &tree_sitter::Node, source: &[u8]) -> String {
    // The grammar's `name` field first: the first identifier child is the
    // return type of a Java method like `Widget build()`.
    if let Some(name) = node
        .child_by_field_name("name")
        .and_then(|n| n.utf8_text(source).ok())
    {
        return name.to_string();
    }
    // Walk direct children looking for an identifier
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        let kind = child.kind();
        if (kind == "identifier"
            || kind == "name"
            || kind == "type_identifier"
            || kind == "property_identifier")
            && let Ok(name) = child.utf8_text(source)
        {
            return name.to_string();
        }
        // For export statements, look deeper (one level)
        if kind == "function_declaration"
            || kind == "class_declaration"
            || kind == "lexical_declaration"
        {
            let mut inner = child.walk();
            for grandchild in child.children(&mut inner) {
                let gk = grandchild.kind();
                if (gk == "identifier" || gk == "type_identifier")
                    && let Ok(name) = grandchild.utf8_text(source)
                {
                    return name.to_string();
                }
            }
        }
    }
    // Fallback: use the node kind
    node.kind().to_string()
}

/// Map a tree-sitter node type to a human-readable kind.
fn node_kind_to_symbol_kind(node_type: &str) -> &str {
    match node_type {
        s if s.contains("function") || s.contains("method") || s.contains("constructor") => {
            "function"
        }
        s if s.contains("struct") => "struct",
        s if s.contains("class") => "class",
        s if s.contains("enum") => "enum",
        s if s.contains("trait") || s.contains("interface") => "interface",
        s if s.contains("impl") => "impl",
        s if s.contains("type") => "type",
        s if s.contains("const") || s.contains("static") || s.contains("var") => "variable",
        s if s.contains("macro") => "macro",
        s if s.contains("mod") => "module",
        s if s.contains("export") => "export",
        s if s.contains("decorated") => "decorated",
        s if s.contains("preproc") => "macro",
        _ => "other",
    }
}

/// Parse a source file and extract code chunks using tree-sitter.
fn extract_chunks(
    file_path: &str,
    source: &str,
    lang_name: &str,
    ts_lang: tree_sitter::Language,
) -> Vec<CodeChunk> {
    let symbol_nodes = get_symbol_nodes(lang_name);
    let source_bytes = source.as_bytes();

    let mut parser = tree_sitter::Parser::new();
    if parser.set_language(&ts_lang).is_err() {
        warn!("Failed to set tree-sitter language for {lang_name}");
        return vec![];
    }

    let tree = match parser.parse(source, None) {
        Some(t) => t,
        None => {
            warn!("tree-sitter failed to parse {file_path}");
            return vec![];
        }
    };

    let mut chunks = Vec::new();
    let root = tree.root_node();

    // Walk top-level children (and one level deeper for module bodies)
    let ctx = CollectCtx {
        source: source_bytes,
        file_path,
        lang_name,
        symbol_nodes,
        full_source: source,
    };
    collect_symbols(&root, &ctx, &mut chunks, 0);

    // If we got zero symbols (e.g. a config file or unusual structure),
    // fall back to indexing the entire file as one chunk.
    if chunks.is_empty() && !source.trim().is_empty() {
        let line_count = source.lines().count() as i64;
        // Only index files up to 500 lines as a single chunk
        if line_count <= 500 {
            chunks.push(CodeChunk {
                file_path: file_path.to_string(),
                symbol_name: file_path
                    .rsplit('/')
                    .next()
                    .unwrap_or(file_path)
                    .to_string(),
                symbol_kind: "file".to_string(),
                language: lang_name.to_string(),
                start_line: 1,
                end_line: line_count,
                content: source.to_string(),
            });
        }
    }

    chunks
}

/// Invariant context passed down each recursive call in `collect_symbols`.
struct CollectCtx<'a> {
    source: &'a [u8],
    file_path: &'a str,
    lang_name: &'a str,
    symbol_nodes: &'a [&'a str],
    full_source: &'a str,
}

/// Symbols whose members are indexed as chunks of their own. A 500-line
/// `impl` or class stored as one chunk was cut at 200 lines, so every
/// method below that was missing from the index.
const CONTAINER_NODES: &[&str] = &[
    "impl_item",
    "trait_item",
    "mod_item",
    "class_declaration",
    "class_definition",
    "interface_declaration",
    "enum_declaration",
];

/// `export ...` and `@decorator ...`: the chunk is the wrapped declaration,
/// with the wrapper's text kept so the decorators and `export` stay in it.
const WRAPPER_NODES: &[&str] = &["export_statement", "decorated_definition"];

/// How deep the walk goes: members sit two levels below their container
/// (container -> body -> member), and containers nest (mod -> impl -> fn).
const MAX_SYMBOL_DEPTH: usize = 8;

/// Longest chunk stored; the rest of a huge function is cut off.
const MAX_CHUNK_LINES: usize = 200;

/// Recursively collect symbol nodes from the AST.
fn collect_symbols(
    node: &tree_sitter::Node,
    ctx: &CollectCtx<'_>,
    chunks: &mut Vec<CodeChunk>,
    depth: usize,
) {
    if depth > MAX_SYMBOL_DEPTH {
        return;
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if !ctx.symbol_nodes.contains(&child.kind()) {
            // Recurse into non-symbol nodes (e.g. module bodies, program root)
            collect_symbols(&child, ctx, chunks, depth + 1);
            continue;
        }

        let symbol = if WRAPPER_NODES.contains(&child.kind()) {
            // `export default <expr>` has no declaration: the wrapper is the chunk.
            child
                .child_by_field_name("declaration")
                .or_else(|| child.child_by_field_name("definition"))
                .filter(|d| ctx.symbol_nodes.contains(&d.kind()))
                .unwrap_or(child)
        } else {
            child
        };
        let src_len = ctx.full_source.len();
        let start_byte = child.start_byte().min(src_len);
        let start_line = (child.start_position().row + 1) as i64; // 1-indexed
        let symbol_name = extract_symbol_name(&symbol, ctx.source);
        let symbol_kind = node_kind_to_symbol_kind(symbol.kind()).to_string();

        if CONTAINER_NODES.contains(&symbol.kind())
            && let Some(first) = first_symbol_start(&symbol, ctx, depth + 1)
        {
            // The container's own chunk is its header (signature, fields,
            // docs) up to the first member; the members follow as their own.
            let header = ctx.full_source[start_byte..first.clamp(start_byte, src_len)].trim_end();
            let end_line = start_line + header.lines().count().max(1) as i64 - 1;
            chunks.push(CodeChunk {
                file_path: ctx.file_path.to_string(),
                symbol_name,
                symbol_kind,
                language: ctx.lang_name.to_string(),
                start_line,
                end_line,
                content: cap_chunk_lines(header),
            });
            collect_symbols(&symbol, ctx, chunks, depth + 1);
            continue;
        }

        let end_line = (child.end_position().row + 1) as i64;
        let content = &ctx.full_source[start_byte..child.end_byte().clamp(start_byte, src_len)];
        chunks.push(CodeChunk {
            file_path: ctx.file_path.to_string(),
            symbol_name,
            symbol_kind,
            language: ctx.lang_name.to_string(),
            start_line,
            end_line,
            content: cap_chunk_lines(content),
        });
    }
}

/// Start byte of the first symbol `collect_symbols` would find below `node`
/// at `depth`, so a container's header ends where its first member begins.
fn first_symbol_start(
    node: &tree_sitter::Node,
    ctx: &CollectCtx<'_>,
    depth: usize,
) -> Option<usize> {
    if depth > MAX_SYMBOL_DEPTH {
        return None;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if ctx.symbol_nodes.contains(&child.kind()) {
            return Some(child.start_byte());
        }
        if let Some(start) = first_symbol_start(&child, ctx, depth + 1) {
            return Some(start);
        }
    }
    None
}

/// `content` cut to `MAX_CHUNK_LINES`, with a note of how much was dropped.
fn cap_chunk_lines(content: &str) -> String {
    let total = content.lines().count();
    if total <= MAX_CHUNK_LINES {
        return content.to_string();
    }
    let lines: Vec<&str> = content.lines().take(MAX_CHUNK_LINES).collect();
    format!(
        "{}\n// ... ({} more lines)",
        lines.join("\n"),
        total - MAX_CHUNK_LINES
    )
}

// ─── Indexing engine ─────────────────────────────────────────────────────────

/// Directories to always skip during indexing.
const SKIP_DIRS: &[&str] = &[
    "target",
    "node_modules",
    ".git",
    ".hg",
    ".svn",
    ".claude",
    "__pycache__",
    ".mypy_cache",
    ".pytest_cache",
    "dist",
    "build",
    ".next",
    ".nuxt",
    "vendor",
    ".venv",
    "venv",
    "env",
    ".tox",
    ".eggs",
    "*.egg-info",
    ".jj",
    ".sl",
];

/// Result of an indexing run.
pub struct IndexResult {
    pub files_scanned: i64,
    pub files_indexed: i64,
    pub files_skipped: i64,
    pub chunks_added: i64,
    pub elapsed_ms: u128,
}

/// Files larger than this are never read for indexing.
const MAX_INDEX_BYTES: u64 = 100_000;

/// The size gate, checked from metadata before a file is read.
fn too_large_to_index(meta: &std::fs::Metadata) -> bool {
    meta.len() > MAX_INDEX_BYTES
}

/// Index a project directory into the RAG database; stored paths are
/// relative to `project`.
/// Incremental: only re-indexes files whose mtime changed.
/// Set `force` to true to clear and re-index everything.
///
/// Inside a git work tree the walk starts at the work tree's root and only
/// descends towards `project`, so every rule above `project` applies to it:
/// an ignored `project` (a launch from `repo/secrets/` with `/secrets/` in
/// `repo/.gitignore`) yields nothing. Walking from `project` itself would
/// not, since the ignore walker never matches its root against them.
pub fn index_project(db: &RagDb, project: &Path, force: bool) -> Result<IndexResult> {
    let start = Instant::now();
    let project = std::fs::canonicalize(project).unwrap_or_else(|_| project.to_path_buf());
    let walk_root = super::git_work_tree_root(&project).unwrap_or_else(|| project.clone());
    let ext_map = ext_to_lang();

    if force {
        db.clear()?;
    }

    let mut files_scanned = 0i64;
    let mut files_indexed = 0i64;
    let mut files_skipped = 0i64;
    let mut chunks_added = 0i64;

    // Collect files to index. `.gitignore` (in git repos and out of them),
    // `.git/info/exclude`, the global excludes file and `.ignore` all apply.
    // Hidden files are kept as before; hidden dirs and SKIP_DIRS are not.
    let scope = project.clone();
    let walker = ignore::WalkBuilder::new(&walk_root)
        .follow_links(false)
        .hidden(false)
        .require_git(false)
        .filter_entry(move |e| {
            let path = e.path();
            if !path.starts_with(&scope) {
                // Between the work-tree root and the project: only the
                // directories leading down to it.
                return scope.starts_with(path);
            }
            let name = e.file_name().to_string_lossy();
            // Skip hidden dirs and known build/vendor dirs (but not the project itself)
            if e.file_type().is_some_and(|t| t.is_dir()) && path != scope {
                return !name.starts_with('.') && !SKIP_DIRS.contains(&name.as_ref());
            }
            true
        })
        .build();

    // Every indexable file we saw this pass; anything in the index that is
    // not here was deleted or renamed and gets pruned below.
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    for entry in walker.filter_map(|e| e.ok()) {
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        // Defense in depth: the walker's `follow_links(false)` skips traversal
        // into symlinked dirs but still surfaces symlinked files; `read_to_string`
        // would then follow them at I/O time. Reject symlinked entries so a
        // crafted repo can't trick the indexer into pulling in `~/.ssh/id_rsa`.
        if entry.path_is_symlink() {
            files_skipped += 1;
            continue;
        }

        let path = entry.path();
        let ext = match path.extension().and_then(|e| e.to_str()) {
            Some(e) => e,
            None => continue,
        };

        let lang_name = match ext_map.get(ext) {
            Some(l) => *l,
            None => continue,
        };

        files_scanned += 1;

        // Relative path for storage, `/`-separated on every OS so the keys
        // (and search results) are the same everywhere. Old `\` keys from a
        // Windows index are not `seen`, so the prune below drops them.
        let rel_path = slash_path(path.strip_prefix(&project).unwrap_or(path));
        seen.insert(rel_path.clone());

        let meta = match path.metadata() {
            Ok(m) => m,
            Err(_) => {
                files_skipped += 1;
                continue;
            }
        };
        // Check size before reading: oversized files never get a row, so the
        // mtime check below never skips them and every pass (one per prompt)
        // would buffer the whole file (a 200 MB .ts video, a bundled .js).
        if too_large_to_index(&meta) {
            files_skipped += 1;
            continue;
        }

        // Check mtime for incremental indexing
        let mtime: i64 = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            // Nanoseconds: whole seconds missed an edit made within the
            // same second as the previous index.
            .map(|d| d.as_nanos() as i64)
            .unwrap_or(0);

        if !force {
            let indexed_mtime = db.file_mtime(&rel_path).unwrap_or(0);
            if mtime <= indexed_mtime {
                files_skipped += 1;
                continue;
            }
        }

        // Read source
        let source = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(_) => {
                files_skipped += 1;
                continue;
            }
        };

        // Skip very large files (>100KB or >5000 lines)
        if source.len() as u64 > MAX_INDEX_BYTES || source.lines().count() > 5000 {
            files_skipped += 1;
            continue;
        }

        // Get tree-sitter language
        let ts_lang = match get_ts_language(lang_name) {
            Some(l) => l,
            None => {
                files_skipped += 1;
                continue;
            }
        };

        let chunks = extract_chunks(&rel_path, &source, lang_name, ts_lang);

        // One short transaction per file, opened only after the read and
        // parse: /memory writes share this database and give up after the
        // 5 s busy timeout, so the write lock must never span the whole walk.
        // Starting with the DELETE takes the lock through the busy handler
        // instead of upgrading a read snapshot (SQLITE_BUSY_SNAPSHOT).
        let tx = db.conn.unchecked_transaction()?;
        tx.execute("DELETE FROM code_chunks WHERE file_path = ?1", [&rel_path])?;
        for chunk in &chunks {
            tx.execute(
                "INSERT INTO code_chunks (file_path, symbol_name, symbol_kind, language, start_line, end_line, content, mtime)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                rusqlite::params![
                    chunk.file_path,
                    chunk.symbol_name,
                    chunk.symbol_kind,
                    chunk.language,
                    chunk.start_line,
                    chunk.end_line,
                    chunk.content,
                    mtime,
                ],
            )?;
        }
        tx.commit()?;

        chunks_added += chunks.len() as i64;
        files_indexed += 1;
        debug!("Indexed {rel_path}: {} chunks", chunks.len());
    }

    // Prune chunks whose file is gone. Files that became too large or
    // unreadable this pass were still *seen*, so their old chunks stay.
    let stale: Vec<String> = {
        let mut stmt = db
            .conn
            .prepare("SELECT DISTINCT file_path FROM code_chunks")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        rows.flatten().filter(|p| !seen.contains(p)).collect()
    };
    if !stale.is_empty() {
        let tx = db.conn.unchecked_transaction()?;
        for path in &stale {
            tx.execute("DELETE FROM code_chunks WHERE file_path = ?1", [path])?;
            debug!("Pruned {path}: file no longer present");
        }
        tx.commit()?;
    }

    Ok(IndexResult {
        files_scanned,
        files_indexed,
        files_skipped,
        chunks_added,
        elapsed_ms: start.elapsed().as_millis(),
    })
}

/// `p` with its components joined by `/`, whatever the OS separator.
pub(crate) fn slash_path(p: &Path) -> String {
    p.components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rag::RagDb;
    use tempfile::TempDir;

    /// Index database for a test project, kept out of the real cache dir.
    fn test_db(project: &Path) -> RagDb {
        RagDb::open_at(&project.join(".rag-test.db")).unwrap()
    }

    fn setup_project(files: &[(&str, &str)]) -> TempDir {
        let tmp = TempDir::new().unwrap();
        for (path, content) in files {
            let full = tmp.path().join(path);
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::fs::write(&full, content).unwrap();
        }
        tmp
    }

    /// The indexer used to hold one write transaction across the whole walk
    /// and parse, so a /memory write on another connection waited out its
    /// busy timeout and failed with "database is locked". Files now commit
    /// one by one, so another connection sees the index grow while it runs.
    #[test]
    fn index_progress_is_committed_file_by_file() {
        let body: String = (0..40)
            .map(|i| format!("pub fn helper_{i}(x: u32) -> u32 {{ x + {i} }}\n"))
            .collect();
        let files: Vec<(String, String)> = (0..200)
            .map(|i| (format!("src/m{i}.rs"), body.clone()))
            .collect();
        let refs: Vec<(&str, &str)> = files
            .iter()
            .map(|(p, c)| (p.as_str(), c.as_str()))
            .collect();
        let tmp = setup_project(&refs);
        let idx = TempDir::new().unwrap();
        let db_path = idx.path().join("rag.db");
        drop(RagDb::open_at(&db_path).unwrap());

        let root = tmp.path().to_path_buf();
        let writer_path = db_path.clone();
        let indexer = std::thread::spawn(move || {
            let db = RagDb::open_at(&writer_path).unwrap();
            index_project(&db, &root, true).unwrap();
        });

        let reader = rusqlite::Connection::open(&db_path).unwrap();
        let mut partial = false;
        while !indexer.is_finished() {
            let n: i64 = reader
                .query_row(
                    "SELECT COUNT(DISTINCT file_path) FROM code_chunks",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            partial |= n > 0 && n < 200;
        }
        indexer.join().unwrap();
        assert!(partial, "the whole index was committed in one transaction");
    }

    fn indexed_files(db: &RagDb) -> Vec<String> {
        let mut stmt = db
            .conn
            .prepare("SELECT DISTINCT file_path FROM code_chunks ORDER BY file_path")
            .unwrap();
        stmt.query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    /// Whatever git would not track never reaches the index (and so never
    /// a model): `.gitignore` at any level, `.git/info/exclude` and
    /// `.ignore`. A file that becomes ignored drops out on the next pass.
    #[test]
    fn gitignored_files_are_not_indexed() {
        let tmp = setup_project(&[
            ("src/app.js", "function startApp() { return 1; }\n"),
            ("src/config.local.js", "const apiKey = 'sk-live-123';\n"),
            ("secrets/keys.py", "def token():\n    return 'x'\n"),
            ("pkg/gen.rs", "fn generated() {}\n"),
            ("pkg/excluded.rs", "fn excluded() {}\n"),
            ("scratch.rs", "fn scratch() {}\n"),
            ("later.rs", "fn later() {}\n"),
            (".gitignore", "*.local.js\n/secrets/\n"),
            ("pkg/.gitignore", "gen.rs\n"),
            (".ignore", "scratch.rs\n"),
        ]);
        let ok = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(tmp.path())
            .status()
            .unwrap()
            .success();
        assert!(ok, "git init failed");
        std::fs::create_dir_all(tmp.path().join(".git/info")).unwrap();
        std::fs::write(tmp.path().join(".git/info/exclude"), "pkg/excluded.rs\n").unwrap();

        let db = test_db(tmp.path());
        index_project(&db, tmp.path(), false).unwrap();
        assert_eq!(indexed_files(&db), ["later.rs", "src/app.js"]);
        assert!(
            super::super::search::search(&db, "apiKey", 10)
                .unwrap()
                .is_empty()
        );

        std::fs::write(
            tmp.path().join(".gitignore"),
            "*.local.js\n/secrets/\nlater.rs\n",
        )
        .unwrap();
        index_project(&db, tmp.path(), false).unwrap();
        assert_eq!(indexed_files(&db), ["src/app.js"]);
    }

    /// Chunks for a file that no longer exists must not survive an
    /// incremental re-index — otherwise search keeps returning code that is
    /// gone until the user thinks to `--force`.
    #[test]
    fn deleted_files_are_pruned_on_incremental_reindex() {
        let tmp = setup_project(&[("keep.rs", "fn keep() {}"), ("gone.rs", "fn gone() {}")]);
        let db = test_db(tmp.path());
        index_project(&db, tmp.path(), false).unwrap();
        assert_eq!(db.file_count().unwrap(), 2);

        std::fs::remove_file(tmp.path().join("gone.rs")).unwrap();
        index_project(&db, tmp.path(), false).unwrap();
        assert_eq!(db.file_count().unwrap(), 1, "gone.rs chunks must be pruned");
        let hits = super::super::search::search(&db, "gone", 10).unwrap();
        assert!(hits.is_empty(), "search still returns the deleted file");
    }

    /// Oversized files are skipped from metadata alone, every pass: they never
    /// get a row, so the mtime check cannot short-circuit them.
    #[test]
    fn oversized_files_are_skipped_on_every_pass() {
        let tmp = setup_project(&[("small.rs", "fn small() {}")]);
        let big = std::fs::File::create(tmp.path().join("bundle.js")).unwrap();
        big.set_len(64 * 1024 * 1024).unwrap();
        drop(big);
        // The gate is decided from metadata alone, before any read: the
        // old code read the whole 64 MiB file and dropped it afterwards.
        let meta = |name: &str| std::fs::metadata(tmp.path().join(name)).unwrap();
        assert!(too_large_to_index(&meta("bundle.js")));
        assert!(!too_large_to_index(&meta("small.rs")));
        let db = test_db(tmp.path());
        for _ in 0..2 {
            let r = index_project(&db, tmp.path(), false).unwrap();
            assert_eq!(r.files_scanned, 2);
            assert_eq!(db.file_count().unwrap(), 1, "bundle.js must not be indexed");
        }
    }

    /// Whole-second mtimes missed an edit made within the same second as
    /// the previous index; nanoseconds do not.
    #[test]
    fn an_edit_shortly_after_indexing_is_picked_up() {
        let tmp = setup_project(&[("lib.rs", "fn a() {}")]);
        let db = test_db(tmp.path());
        index_project(&db, tmp.path(), false).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(30));
        std::fs::write(tmp.path().join("lib.rs"), "fn b() {}").unwrap();
        let r = index_project(&db, tmp.path(), false).unwrap();
        assert_eq!(r.files_indexed, 1, "the edited file must be re-indexed");
    }

    #[test]
    fn test_index_rust_file() {
        let tmp = setup_project(&[(
            "src/lib.rs",
            "pub fn hello() -> &'static str { \"hello\" }\n\nstruct Config { name: String }\n",
        )]);
        let db = test_db(tmp.path());
        let result = index_project(&db, tmp.path(), false).unwrap();
        assert_eq!(result.files_scanned, 1);
        assert_eq!(result.files_indexed, 1);
        assert!(result.chunks_added >= 2); // hello + Config
        assert_eq!(db.file_count().unwrap(), 1);
    }

    #[test]
    fn test_index_multiple_languages() {
        let tmp = setup_project(&[
            ("main.rs", "fn main() {}"),
            ("app.py", "def run():\n    pass\n"),
            ("index.js", "function init() { return 1; }\n"),
        ]);
        let db = test_db(tmp.path());
        let result = index_project(&db, tmp.path(), false).unwrap();
        assert_eq!(result.files_scanned, 3);
        assert_eq!(result.files_indexed, 3);
        assert!(result.chunks_added >= 3);
    }

    #[test]
    fn test_incremental_index_skips_unchanged() {
        let tmp = setup_project(&[("lib.rs", "fn foo() {}")]);
        let db = test_db(tmp.path());

        let r1 = index_project(&db, tmp.path(), false).unwrap();
        assert_eq!(r1.files_indexed, 1);

        // Second run: nothing changed, should skip
        let r2 = index_project(&db, tmp.path(), false).unwrap();
        assert_eq!(r2.files_indexed, 0);
        assert_eq!(r2.files_skipped, 1);
    }

    #[test]
    fn test_force_reindex() {
        let tmp = setup_project(&[("lib.rs", "fn foo() {}")]);
        let db = test_db(tmp.path());

        index_project(&db, tmp.path(), false).unwrap();
        let r2 = index_project(&db, tmp.path(), true).unwrap();
        assert_eq!(r2.files_indexed, 1); // force = re-indexed even though unchanged
    }

    #[test]
    fn test_skips_target_dir() {
        let tmp = setup_project(&[
            ("src/lib.rs", "fn good() {}"),
            ("target/debug/out.rs", "fn bad() {}"),
        ]);
        let db = test_db(tmp.path());
        let result = index_project(&db, tmp.path(), false).unwrap();
        assert_eq!(result.files_scanned, 1); // only src/lib.rs
        assert_eq!(db.file_count().unwrap(), 1);
    }

    #[test]
    fn test_skips_non_source_files() {
        let tmp = setup_project(&[
            ("README.md", "# Hello"),
            ("data.csv", "a,b,c"),
            ("lib.rs", "fn works() {}"),
        ]);
        let db = test_db(tmp.path());
        let result = index_project(&db, tmp.path(), false).unwrap();
        assert_eq!(result.files_scanned, 1); // only .rs
    }

    #[test]
    fn test_large_file_skipped() {
        let tmp = setup_project(&[
            ("huge.rs", &"fn x() {}\n".repeat(6000)), // >5000 lines
        ]);
        let db = test_db(tmp.path());
        let result = index_project(&db, tmp.path(), false).unwrap();
        assert_eq!(result.files_skipped, 1);
        assert_eq!(result.files_indexed, 0);
    }

    #[test]
    fn test_symbol_extraction_rust() {
        let tmp = setup_project(&[(
            "lib.rs",
            "\
pub fn public_func() -> i32 { 42 }

struct MyStruct {
    field: String,
}

enum Color {
    Red,
    Blue,
}

impl MyStruct {
    fn method(&self) {}
}
",
        )]);
        let db = test_db(tmp.path());
        index_project(&db, tmp.path(), false).unwrap();

        // Should have extracted: public_func, MyStruct, Color, MyStruct (impl)
        let chunks = db.chunk_count().unwrap();
        assert!(chunks >= 4, "expected at least 4 chunks, got {chunks}");
    }

    fn chunks_of(path: &str, source: &str, lang: &str) -> Vec<CodeChunk> {
        extract_chunks(path, source, lang, get_ts_language(lang).unwrap())
    }

    fn chunk<'a>(chunks: &'a [CodeChunk], name: &str) -> &'a CodeChunk {
        chunks
            .iter()
            .find(|c| c.symbol_name == name)
            .unwrap_or_else(|| {
                let names: Vec<&str> = chunks.iter().map(|c| c.symbol_name.as_str()).collect();
                panic!("no chunk named {name}; got {names:?}")
            })
    }

    /// An impl was one chunk cut at 200 lines, so a method further down
    /// (`prune_inactive` at line 508 of `impl Session`) was not in the index.
    #[test]
    fn methods_past_line_200_of_an_impl_are_their_own_chunks() {
        let mut src =
            String::from("pub struct Session;\n\nimpl Session {\n    // Sessions on disk.\n");
        for i in 0..250 {
            src.push_str(&format!("    fn filler_{i}(&self) -> u32 {{ {i} }}\n"));
        }
        src.push_str("    pub fn prune_inactive(&self) -> u32 {\n        3\n    }\n}\n");
        src.push_str("\nmod outer {\n    impl super::Session {\n        fn nested_method(&self) {}\n    }\n}\n");
        let chunks = chunks_of("session.rs", &src, "rust");

        let late = chunk(&chunks, "prune_inactive");
        assert_eq!((late.start_line, late.end_line), (255, 257));
        assert_eq!(late.symbol_kind, "function");
        assert!(late.content.starts_with("pub fn prune_inactive"));

        // The impl's own chunk is its header, not a second copy of every method.
        let header = chunks.iter().find(|c| c.symbol_kind == "impl").unwrap();
        assert_eq!((header.start_line, header.end_line), (3, 4));
        assert!(header.content.contains("Sessions on disk"));
        assert!(!header.content.contains("filler_0"));

        // mod -> impl -> fn sits deeper than the old depth cap.
        assert_eq!(chunk(&chunks, "nested_method").start_line, 262);
    }

    /// Same for a Python class; a decorated method keeps its decorator and
    /// is named after the function, not `decorated_definition`.
    #[test]
    fn decorated_methods_past_line_200_of_a_class_are_indexed_by_name() {
        let mut src = String::from("class Store:\n    \"\"\"Keeps things.\"\"\"\n\n");
        for i in 0..210 {
            src.push_str(&format!("    def filler_{i}(self):\n        return {i}\n"));
        }
        src.push_str("    @staticmethod\n    def rebuild_cache():\n        return 1\n");
        let chunks = chunks_of("store.py", &src, "python");

        let late = chunk(&chunks, "rebuild_cache");
        assert_eq!((late.start_line, late.end_line), (424, 426));
        assert!(late.content.starts_with("@staticmethod"));
        assert!(
            !chunks
                .iter()
                .any(|c| c.symbol_name == "decorated_definition")
        );
        assert!(chunk(&chunks, "Store").content.contains("Keeps things"));
    }

    /// A Java method's first identifier-like child is its return type.
    #[test]
    fn java_methods_are_named_after_the_method_not_the_return_type() {
        let src = "class Factory {\n    Widget build() {\n        return new Widget();\n    }\n}\n";
        let chunks = chunks_of("Factory.java", src, "java");
        assert_eq!(chunk(&chunks, "build").start_line, 2);
        assert!(!chunks.iter().any(|c| c.symbol_name == "Widget"));
    }

    /// `export class` is indexed through the class: its methods too.
    #[test]
    fn exported_class_methods_are_indexed() {
        let src =
            "export class Api {\n  fetchUser(id) {\n    return id;\n  }\n}\nexport default 42;\n";
        let chunks = chunks_of("api.ts", src, "typescript");
        let class = chunk(&chunks, "Api");
        assert_eq!(class.symbol_kind, "class");
        assert!(class.content.starts_with("export class Api"));
        assert_eq!(chunk(&chunks, "fetchUser").start_line, 2);
        // No declaration to unwrap: the export itself is the chunk, as before.
        assert!(chunks.iter().any(|c| c.symbol_kind == "export"));
    }
}
