/// FTS5-powered code search over the RAG index.
///
/// Given a natural language query (or keywords), retrieves the most relevant
/// code chunks ranked by FTS5 relevance score + symbol-kind boost.
use anyhow::Result;

use super::RagDb;

/// A search result from the RAG index.
#[derive(Debug, Clone)]
pub struct SearchResult {
    pub file_path: String,
    pub symbol_name: String,
    pub symbol_kind: String,
    pub language: String,
    pub start_line: i64,
    pub end_line: i64,
    pub content: String,
    /// FTS5 relevance rank (lower = more relevant).
    pub rank: f64,
}

impl SearchResult {
    /// Format this result as context for injection into the system prompt.
    pub fn as_context(&self) -> String {
        format!(
            "# {file}:{start}-{end} ({kind} `{name}`, {lang})\n{content}",
            file = self.file_path,
            start = self.start_line,
            end = self.end_line,
            kind = self.symbol_kind,
            name = self.symbol_name,
            lang = self.language,
            content = self.content,
        )
    }
}

/// Sanitize a query string for FTS5.
/// FTS5 syntax uses quotes, AND, OR, NOT, NEAR etc.
/// We escape user input to prevent syntax errors.
pub(crate) fn sanitize_fts_query(query: &str) -> String {
    // Split into words, quote each as a prefix term, OR them (recall)
    query
        .split_whitespace()
        .filter(|w| w.len() >= 2) // skip very short words
        .map(|w| {
            // Strip FTS5 operators
            let clean: String = w
                .chars()
                .filter(|c| c.is_alphanumeric() || *c == '_' || *c == '.')
                .collect();
            if clean.is_empty() {
                return String::new();
            }
            // Use prefix matching for flexibility
            format!("\"{clean}\"*")
        })
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" OR ")
}

/// Search the RAG index for code chunks matching a query.
///
/// Returns up to `limit` results sorted by relevance.
/// The query can be natural language — keywords are extracted and matched
/// against symbol names and code content via FTS5.
pub fn search(db: &RagDb, query: &str, limit: i64) -> Result<Vec<SearchResult>> {
    let fts_query = sanitize_fts_query(query);
    if fts_query.is_empty() {
        return Ok(vec![]);
    }

    let mut stmt = db.conn.prepare(
        "SELECT
            c.file_path,
            c.symbol_name,
            c.symbol_kind,
            c.language,
            c.start_line,
            c.end_line,
            c.content,
            chunks_fts.rank
         FROM chunks_fts
         JOIN code_chunks c ON c.id = chunks_fts.rowid
         WHERE chunks_fts MATCH ?1
         ORDER BY chunks_fts.rank
         LIMIT ?2",
    )?;

    let results = stmt.query_map(rusqlite::params![fts_query, limit], |row| {
        Ok(SearchResult {
            file_path: row.get(0)?,
            symbol_name: row.get(1)?,
            symbol_kind: row.get(2)?,
            language: row.get(3)?,
            start_line: row.get(4)?,
            end_line: row.get(5)?,
            content: row.get(6)?,
            rank: row.get(7)?,
        })
    })?;

    let mut out = Vec::new();
    for r in results.flatten() {
        out.push(r);
    }
    Ok(out)
}

/// Search specifically for a symbol by name (exact or prefix match).
/// Useful for "find me the function called X" queries.
#[allow(dead_code)] // public API for future /rag symbol command
pub fn search_symbol(db: &RagDb, name: &str, limit: i64) -> Result<Vec<SearchResult>> {
    let mut stmt = db.conn.prepare(
        "SELECT
            file_path, symbol_name, symbol_kind, language,
            start_line, end_line, content, 0.0 as rank
         FROM code_chunks
         WHERE symbol_name LIKE ?1
         ORDER BY
            CASE WHEN symbol_name = ?2 THEN 0 ELSE 1 END,
            file_path
         LIMIT ?3",
    )?;

    let pattern = format!("{name}%");
    let results = stmt.query_map(rusqlite::params![pattern, name, limit], |row| {
        Ok(SearchResult {
            file_path: row.get(0)?,
            symbol_name: row.get(1)?,
            symbol_kind: row.get(2)?,
            language: row.get(3)?,
            start_line: row.get(4)?,
            end_line: row.get(5)?,
            content: row.get(6)?,
            rank: row.get(7)?,
        })
    })?;

    let mut out = Vec::new();
    for r in results.flatten() {
        out.push(r);
    }
    Ok(out)
}

/// Build a context string from search results, suitable for injection
/// into the system prompt or a user message.
///
/// Caps total output at `max_chars` to avoid blowing up the context window.
pub fn build_context(results: &[SearchResult], max_chars: usize) -> String {
    if results.is_empty() {
        return String::new();
    }

    let mut parts = Vec::new();
    let mut total_chars = 0;

    for r in results {
        let chunk = r.as_context();
        if total_chars + chunk.len() > max_chars {
            break;
        }
        total_chars += chunk.len();
        parts.push(chunk);
    }

    if parts.is_empty() {
        return String::new();
    }

    format!(
        "<codebase_context>\n{}\n</codebase_context>",
        parts.join("\n\n")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rag::RagDb;
    use tempfile::TempDir;

    fn seeded_db() -> (TempDir, RagDb) {
        let tmp = TempDir::new().unwrap();
        let db = RagDb::open_at(&tmp.path().join("rag.db")).unwrap();
        let chunks = vec![
            (
                "src/auth.rs",
                "authenticate_user",
                "function",
                "rust",
                "fn authenticate_user(token: &str) -> Result<User> { verify(token) }",
            ),
            (
                "src/auth.rs",
                "verify_token",
                "function",
                "rust",
                "fn verify_token(t: &str) -> bool { !t.is_empty() }",
            ),
            (
                "src/api.rs",
                "handle_request",
                "function",
                "rust",
                "fn handle_request(req: Request) -> Response { route(req) }",
            ),
            (
                "src/api.rs",
                "ApiServer",
                "struct",
                "rust",
                "struct ApiServer { port: u16, host: String }",
            ),
            (
                "src/db.rs",
                "DatabasePool",
                "struct",
                "rust",
                "struct DatabasePool { connections: Vec<Connection> }",
            ),
            (
                "src/db.rs",
                "query",
                "function",
                "rust",
                "fn query(pool: &DatabasePool, sql: &str) -> Vec<Row> { pool.execute(sql) }",
            ),
        ];
        for (path, name, kind, lang, content) in chunks {
            db.conn.execute(
                "INSERT INTO code_chunks (file_path, symbol_name, symbol_kind, language, start_line, end_line, content, mtime)
                 VALUES (?1, ?2, ?3, ?4, 1, 10, ?5, 1000)",
                rusqlite::params![path, name, kind, lang, content],
            ).unwrap();
        }
        (tmp, db)
    }

    #[test]
    fn test_sanitize_fts_query_basic() {
        let q = sanitize_fts_query("authenticate user");
        assert!(q.contains("\"authenticate\"*"));
        assert!(q.contains("\"user\"*"));
    }

    #[test]
    fn test_sanitize_fts_query_strips_operators() {
        let q = sanitize_fts_query("NOT foo AND bar");
        // Short words like "NOT" and "AND" (3 chars) pass the len >= 2 filter
        // but their special meaning is neutralized by quoting
        assert!(q.contains("\"NOT\"*"));
        assert!(q.contains("\"foo\"*"));
    }

    #[test]
    fn test_sanitize_fts_query_skips_short_words() {
        let q = sanitize_fts_query("a b cc dd");
        assert!(!q.contains("\"a\""));
        assert!(!q.contains("\"b\""));
        assert!(q.contains("\"cc\"*"));
        assert!(q.contains("\"dd\"*"));
    }

    #[test]
    fn test_search_finds_relevant_results() {
        let (_tmp, db) = seeded_db();
        let results = search(&db, "authenticate token", 10).unwrap();
        assert!(!results.is_empty());
        assert_eq!(results[0].symbol_name, "authenticate_user");
    }

    #[test]
    fn test_search_empty_query() {
        let (_tmp, db) = seeded_db();
        let results = search(&db, "", 10).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn test_search_no_results() {
        let (_tmp, db) = seeded_db();
        let results = search(&db, "zzzznonexistent", 10).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn test_search_respects_limit() {
        let (_tmp, db) = seeded_db();
        let results = search(&db, "function struct", 2).unwrap();
        assert!(results.len() <= 2);
    }

    #[test]
    fn test_search_symbol() {
        let (_tmp, db) = seeded_db();
        let results = search_symbol(&db, "authenticate_user", 10).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].file_path, "src/auth.rs");
    }

    #[test]
    fn test_search_symbol_prefix() {
        let (_tmp, db) = seeded_db();
        let results = search_symbol(&db, "authenticate", 10).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].symbol_name, "authenticate_user");
    }

    #[test]
    fn test_build_context_basic() {
        let results = vec![SearchResult {
            file_path: "src/main.rs".to_string(),
            symbol_name: "main".to_string(),
            symbol_kind: "function".to_string(),
            language: "rust".to_string(),
            start_line: 1,
            end_line: 5,
            content: "fn main() {}".to_string(),
            rank: -10.0,
        }];
        let ctx = build_context(&results, 10000);
        assert!(ctx.contains("<codebase_context>"));
        assert!(ctx.contains("src/main.rs:1-5"));
        assert!(ctx.contains("fn main() {}"));
    }

    #[test]
    fn test_build_context_respects_char_limit() {
        let results: Vec<SearchResult> = (0..100)
            .map(|i| SearchResult {
                file_path: format!("src/file{i}.rs"),
                symbol_name: format!("func_{i}"),
                symbol_kind: "function".to_string(),
                language: "rust".to_string(),
                start_line: 1,
                end_line: 10,
                content: "x".repeat(200),
                rank: -(100 - i) as f64,
            })
            .collect();
        let ctx = build_context(&results, 500);
        assert!(ctx.len() < 600); // some overhead from wrapper tags
    }

    #[test]
    fn test_build_context_empty() {
        let ctx = build_context(&[], 10000);
        assert!(ctx.is_empty());
    }
}
