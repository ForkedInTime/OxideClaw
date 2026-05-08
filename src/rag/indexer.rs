/// tree-sitter based code indexer.
///
/// Walks the project, parses source files with language-specific grammars,
/// extracts top-level symbols (functions, structs, classes, impls, etc.),
/// and stores them as searchable chunks in the RAG database.
///
/// Incremental: only re-indexes files whose mtime changed since last index.
use anyhow::Result;
use std::collections::HashMap;
use std::path::Path;
use std::time::Instant;
use tracing::{debug, warn};
use walkdir::WalkDir;

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
/// Looks for the first `identifier` or `name` child.
fn extract_symbol_name(node: &tree_sitter::Node, source: &[u8]) -> String {
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

/// Recursively collect symbol nodes from the AST.
fn collect_symbols(
    node: &tree_sitter::Node,
    ctx: &CollectCtx<'_>,
    chunks: &mut Vec<CodeChunk>,
    depth: usize,
) {
    // Don't recurse too deep
    if depth > 3 {
        return;
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        let kind = child.kind();

        if ctx.symbol_nodes.contains(&kind) {
            let start_line = (child.start_position().row + 1) as i64; // 1-indexed
            let end_line = (child.end_position().row + 1) as i64;

            // Extract the source text for this node
            let start_byte = child.start_byte();
            let end_byte = child.end_byte();
            let src_len = ctx.full_source.len();
            let content = &ctx.full_source[start_byte.min(src_len)..end_byte.min(src_len)];

            // Cap chunk size at 200 lines — huge functions get truncated
            let content = if end_line - start_line > 200 {
                let lines: Vec<&str> = content.lines().take(200).collect();
                format!(
                    "{}\n// ... ({} more lines)",
                    lines.join("\n"),
                    end_line - start_line - 200
                )
            } else {
                content.to_string()
            };

            let symbol_name = extract_symbol_name(&child, ctx.source);
            let symbol_kind = node_kind_to_symbol_kind(kind).to_string();

            chunks.push(CodeChunk {
                file_path: ctx.file_path.to_string(),
                symbol_name,
                symbol_kind,
                language: ctx.lang_name.to_string(),
                start_line,
                end_line,
                content,
            });
        } else {
            // Recurse into non-symbol nodes (e.g. module bodies, program root)
            collect_symbols(&child, ctx, chunks, depth + 1);
        }
    }
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

/// Index a project directory into the RAG database.
/// Incremental: only re-indexes files whose mtime changed.
/// Set `force` to true to clear and re-index everything.
pub fn index_project(db: &RagDb, cwd: &Path, force: bool) -> Result<IndexResult> {
    let start = Instant::now();
    let ext_map = ext_to_lang();

    if force {
        db.clear()?;
    }

    let mut files_scanned = 0i64;
    let mut files_indexed = 0i64;
    let mut files_skipped = 0i64;
    let mut chunks_added = 0i64;

    // Collect files to index
    let walker = WalkDir::new(cwd)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| {
            let name = e.file_name().to_string_lossy();
            // Skip hidden dirs and known build/vendor dirs (but not the root cwd itself)
            if e.file_type().is_dir() && e.depth() > 0 {
                return !name.starts_with('.') && !SKIP_DIRS.contains(&name.as_ref());
            }
            true
        });

    // Batch insert with a transaction for speed
    let tx = db.conn.unchecked_transaction()?;

    for entry in walker.filter_map(|e| e.ok()) {
        if !entry.file_type().is_file() {
            continue;
        }
        // Defense in depth: WalkDir's `follow_links(false)` skips traversal
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

        // Get relative path for storage
        let rel_path = path
            .strip_prefix(cwd)
            .unwrap_or(path)
            .to_string_lossy()
            .to_string();

        // Check mtime for incremental indexing
        let mtime: i64 = path
            .metadata()
            .map(|m| {
                m.modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0)
            })
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
        if source.len() > 100_000 || source.lines().count() > 5000 {
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

        // Delete old chunks for this file
        tx.execute("DELETE FROM code_chunks WHERE file_path = ?1", [&rel_path])?;

        // Extract and insert new chunks
        let chunks = extract_chunks(&rel_path, &source, lang_name, ts_lang);
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

        chunks_added += chunks.len() as i64;
        files_indexed += 1;
        debug!("Indexed {rel_path}: {} chunks", chunks.len());
    }

    tx.commit()?;

    Ok(IndexResult {
        files_scanned,
        files_indexed,
        files_skipped,
        chunks_added,
        elapsed_ms: start.elapsed().as_millis(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rag::RagDb;
    use tempfile::TempDir;

    fn setup_project(files: &[(&str, &str)]) -> TempDir {
        let tmp = TempDir::new().unwrap();
        for (path, content) in files {
            let full = tmp.path().join(path);
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::fs::write(&full, content).unwrap();
        }
        tmp
    }

    #[test]
    fn test_index_rust_file() {
        let tmp = setup_project(&[(
            "src/lib.rs",
            "pub fn hello() -> &'static str { \"hello\" }\n\nstruct Config { name: String }\n",
        )]);
        let db = RagDb::open(tmp.path()).unwrap();
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
        let db = RagDb::open(tmp.path()).unwrap();
        let result = index_project(&db, tmp.path(), false).unwrap();
        assert_eq!(result.files_scanned, 3);
        assert_eq!(result.files_indexed, 3);
        assert!(result.chunks_added >= 3);
    }

    #[test]
    fn test_incremental_index_skips_unchanged() {
        let tmp = setup_project(&[("lib.rs", "fn foo() {}")]);
        let db = RagDb::open(tmp.path()).unwrap();

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
        let db = RagDb::open(tmp.path()).unwrap();

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
        let db = RagDb::open(tmp.path()).unwrap();
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
        let db = RagDb::open(tmp.path()).unwrap();
        let result = index_project(&db, tmp.path(), false).unwrap();
        assert_eq!(result.files_scanned, 1); // only .rs
    }

    #[test]
    fn test_large_file_skipped() {
        let tmp = setup_project(&[
            ("huge.rs", &"fn x() {}\n".repeat(6000)), // >5000 lines
        ]);
        let db = RagDb::open(tmp.path()).unwrap();
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
        let db = RagDb::open(tmp.path()).unwrap();
        index_project(&db, tmp.path(), false).unwrap();

        // Should have extracted: public_func, MyStruct, Color, MyStruct (impl)
        let chunks = db.chunk_count().unwrap();
        assert!(chunks >= 4, "expected at least 4 chunks, got {chunks}");
    }
}
