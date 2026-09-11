/// Local codebase RAG — tree-sitter AST indexing + SQLite FTS5 search.
///
/// Indexes the project's source code into per-symbol chunks, stored in a local
/// SQLite database.  When the agent needs context, FTS5 retrieves the most
/// relevant code spans — so the LLM sees surgical context instead of whole files.
///
/// Index location: `<project>/.claude/rag.db`
///
/// This is the feature [redacted] charges $20/month for.  We do it locally, for free,
/// in a single binary with zero external dependencies.
pub mod indexer;
pub mod search;

use anyhow::{Context, Result};
use rusqlite::{Connection, params};
use std::path::{Path, PathBuf};
use tracing::{debug, warn};

/// Current schema version for the RAG database.
///
/// Bump this every time you add a column, table, or index that existing
/// databases won't pick up through `CREATE ... IF NOT EXISTS`. Then add a
/// migration step in `apply_migrations` for the new version.
///
/// Version history:
///   1 — Baseline: code_chunks, chunks_fts, memory, memory_fts, rag_meta.
pub(crate) const RAG_SCHEMA_VERSION: i64 = 1;

/// The RAG database — owns a SQLite connection with FTS5 tables.
pub struct RagDb {
    pub conn: Connection,
    pub db_path: PathBuf,
}

impl RagDb {
    /// Open (or create) the RAG database for a project.
    /// Stored at `<cwd>/.claude/rag.db`.
    pub fn open(cwd: &Path) -> Result<Self> {
        let claude_dir = cwd.join(".claude");
        std::fs::create_dir_all(&claude_dir)
            .context("Failed to create .claude directory for RAG index")?;

        let db_path = claude_dir.join("rag.db");
        let conn = Connection::open(&db_path).context("Failed to open RAG database")?;

        // Performance: WAL mode + relaxed sync for indexing speed
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA cache_size = -8000;", // 8MB cache
        )?;

        // Create tables if they don't exist
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS code_chunks (
                id          INTEGER PRIMARY KEY,
                file_path   TEXT NOT NULL,
                symbol_name TEXT NOT NULL,
                symbol_kind TEXT NOT NULL,
                language    TEXT NOT NULL,
                start_line  INTEGER NOT NULL,
                end_line    INTEGER NOT NULL,
                content     TEXT NOT NULL,
                mtime       INTEGER NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_chunks_file ON code_chunks(file_path);
            CREATE INDEX IF NOT EXISTS idx_chunks_symbol ON code_chunks(symbol_name);

            CREATE VIRTUAL TABLE IF NOT EXISTS chunks_fts USING fts5(
                symbol_name,
                content,
                content=code_chunks,
                content_rowid=id,
                tokenize='porter unicode61'
            );

            -- Triggers to keep FTS in sync with the content table
            CREATE TRIGGER IF NOT EXISTS chunks_ai AFTER INSERT ON code_chunks BEGIN
                INSERT INTO chunks_fts(rowid, symbol_name, content)
                VALUES (new.id, new.symbol_name, new.content);
            END;

            CREATE TRIGGER IF NOT EXISTS chunks_ad AFTER DELETE ON code_chunks BEGIN
                INSERT INTO chunks_fts(chunks_fts, rowid, symbol_name, content)
                VALUES ('delete', old.id, old.symbol_name, old.content);
            END;

            CREATE TRIGGER IF NOT EXISTS chunks_au AFTER UPDATE ON code_chunks BEGIN
                INSERT INTO chunks_fts(chunks_fts, rowid, symbol_name, content)
                VALUES ('delete', old.id, old.symbol_name, old.content);
                INSERT INTO chunks_fts(rowid, symbol_name, content)
                VALUES (new.id, new.symbol_name, new.content);
            END;

            -- Metadata table for tracking index state
            CREATE TABLE IF NOT EXISTS rag_meta (
                key   TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );

            -- ── Memory tables ──────────────────────────────────────────────────
            CREATE TABLE IF NOT EXISTS memory (
                id          INTEGER PRIMARY KEY,
                key         TEXT NOT NULL UNIQUE,
                value       TEXT NOT NULL,
                category    TEXT NOT NULL DEFAULT 'context',
                source      TEXT NOT NULL DEFAULT 'user',
                created_at  INTEGER NOT NULL DEFAULT (unixepoch()),
                updated_at  INTEGER NOT NULL DEFAULT (unixepoch())
            );

            CREATE INDEX IF NOT EXISTS idx_memory_category ON memory(category);
            CREATE INDEX IF NOT EXISTS idx_memory_updated  ON memory(updated_at DESC);

            CREATE VIRTUAL TABLE IF NOT EXISTS memory_fts USING fts5(
                key,
                value,
                content=memory,
                content_rowid=id,
                tokenize='porter unicode61'
            );

            -- Triggers to keep memory_fts in sync with the memory table
            CREATE TRIGGER IF NOT EXISTS memory_ai AFTER INSERT ON memory BEGIN
                INSERT INTO memory_fts(rowid, key, value)
                VALUES (new.id, new.key, new.value);
            END;

            CREATE TRIGGER IF NOT EXISTS memory_ad AFTER DELETE ON memory BEGIN
                INSERT INTO memory_fts(memory_fts, rowid, key, value)
                VALUES ('delete', old.id, old.key, old.value);
            END;

            CREATE TRIGGER IF NOT EXISTS memory_au AFTER UPDATE ON memory BEGIN
                INSERT INTO memory_fts(memory_fts, rowid, key, value)
                VALUES ('delete', old.id, old.key, old.value);
                INSERT INTO memory_fts(rowid, key, value)
                VALUES (new.id, new.key, new.value);
            END;",
        )?;

        // Run migrations if the persisted schema version is behind the
        // current version. The first run also records the baseline v1.
        apply_migrations(&conn).context("Failed to apply RAG schema migrations")?;

        debug!("RAG database opened at {}", db_path.display());
        Ok(Self { conn, db_path })
    }

    /// Total number of indexed chunks.
    pub fn chunk_count(&self) -> Result<i64> {
        let count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM code_chunks", [], |row| row.get(0))?;
        Ok(count)
    }

    /// Number of unique files indexed.
    pub fn file_count(&self) -> Result<i64> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(DISTINCT file_path) FROM code_chunks",
            [],
            |row| row.get(0),
        )?;
        Ok(count)
    }

    /// Get the mtime we last indexed for a file (0 if never indexed).
    pub fn file_mtime(&self, path: &str) -> Result<i64> {
        let result = self.conn.query_row(
            "SELECT MAX(mtime) FROM code_chunks WHERE file_path = ?1",
            [path],
            |row| row.get::<_, Option<i64>>(0),
        )?;
        Ok(result.unwrap_or(0))
    }

    /// Delete all chunks for a given file (before re-indexing it).
    #[allow(dead_code)] // public API, used in tests, will be called by incremental re-index
    pub fn delete_file_chunks(&self, path: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM code_chunks WHERE file_path = ?1", [path])?;
        Ok(())
    }

    /// Delete all chunks (full re-index).
    pub fn clear(&self) -> Result<()> {
        self.conn.execute_batch(
            "DELETE FROM code_chunks;
             INSERT INTO chunks_fts(chunks_fts) VALUES ('rebuild');",
        )?;
        Ok(())
    }

    /// Database file size in bytes.
    pub fn db_size(&self) -> i64 {
        std::fs::metadata(&self.db_path)
            .map(|m| m.len() as i64)
            .unwrap_or(0)
    }
}

/// Read the persisted schema version from the `rag_meta` table.
/// Returns 0 if the row is missing (fresh DB, pre-versioning DB, or one
/// opened by an older OxideClaw that never wrote the key).
pub(crate) fn read_schema_version(conn: &Connection) -> Result<i64> {
    let v: Option<String> = conn
        .query_row(
            "SELECT value FROM rag_meta WHERE key = 'schema_version'",
            [],
            |row| row.get(0),
        )
        .map_or_else(
            |e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(other),
            },
            |s: String| Ok::<Option<String>, rusqlite::Error>(Some(s)),
        )?;
    Ok(v.and_then(|s| s.parse::<i64>().ok()).unwrap_or(0))
}

/// Apply the schema migration ladder.
///
/// The strategy:
///   - Read `rag_meta['schema_version']`. Missing → treat as 0.
///   - If 0, this is either a fresh DB or one created by a pre-migration
///     OxideClaw build. Either way, the v1 shape has already been ensured
///     above by `CREATE TABLE IF NOT EXISTS`, so we can safely jump to 1.
///   - For any future version N, add a `from_{N-1}_to_{N}(&conn)?` step
///     here and bump `RAG_SCHEMA_VERSION`. Each step runs inside a
///     transaction so a partial migration cannot leave the DB wedged.
///   - If the persisted version is HIGHER than `RAG_SCHEMA_VERSION` (user
///     downgraded OxideClaw), we don't fail — we just log and continue
///     with the assumption that newer schemas are backward-compatible for
///     read. This matches well-behaved tooling in the space and avoids the
///     "downgrade destroys your index" failure mode.
pub(crate) fn apply_migrations(conn: &Connection) -> Result<()> {
    let current = read_schema_version(conn)?;

    if current > RAG_SCHEMA_VERSION {
        warn!(
            "RAG DB schema version {current} is newer than this build ({RAG_SCHEMA_VERSION}). \
             Proceeding read-only-ish — downgrade may miss new columns."
        );
        return Ok(());
    }

    if current == RAG_SCHEMA_VERSION {
        return Ok(());
    }

    // Run each step in its own transaction so a failure mid-ladder can't
    // leave the DB at an intermediate undefined state.
    let mut version = current;
    while version < RAG_SCHEMA_VERSION {
        match version {
            0 => {
                // Baseline: the v1 tables are already present via the
                // CREATE ... IF NOT EXISTS block in RagDb::open. Nothing to
                // ALTER — just record the version.
                conn.execute(
                    "INSERT INTO rag_meta (key, value) VALUES ('schema_version', ?1) \
                     ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                    params!["1"],
                )?;
                debug!("RAG schema migrated: 0 -> 1 (baseline recorded)");
            }
            // Future migrations go here. Example:
            //
            // 1 => {
            //     conn.execute_batch(
            //         "BEGIN;
            //          ALTER TABLE code_chunks ADD COLUMN embedding BLOB;
            //          UPDATE rag_meta SET value='2' WHERE key='schema_version';
            //          COMMIT;"
            //     )?;
            //     debug!("RAG schema migrated: 1 -> 2");
            // }
            _ => {
                return Err(anyhow::anyhow!(
                    "No migration defined from RAG schema version {version} — \
                     this is a bug: add a branch in apply_migrations()."
                ));
            }
        }
        version += 1;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn test_db() -> (TempDir, RagDb) {
        let tmp = TempDir::new().unwrap();
        let db = RagDb::open(tmp.path()).unwrap();
        (tmp, db)
    }

    #[test]
    fn test_open_creates_tables() {
        let (_tmp, db) = test_db();
        assert_eq!(db.chunk_count().unwrap(), 0);
        assert_eq!(db.file_count().unwrap(), 0);
        assert!(db.db_size() > 0);
    }

    #[test]
    fn test_insert_and_count() {
        let (_tmp, db) = test_db();
        db.conn.execute(
            "INSERT INTO code_chunks (file_path, symbol_name, symbol_kind, language, start_line, end_line, content, mtime)
             VALUES ('src/main.rs', 'main', 'function', 'rust', 1, 10, 'fn main() {}', 1000)",
            [],
        ).unwrap();
        assert_eq!(db.chunk_count().unwrap(), 1);
        assert_eq!(db.file_count().unwrap(), 1);
    }

    #[test]
    fn test_file_mtime() {
        let (_tmp, db) = test_db();
        assert_eq!(db.file_mtime("src/main.rs").unwrap(), 0);
        db.conn.execute(
            "INSERT INTO code_chunks (file_path, symbol_name, symbol_kind, language, start_line, end_line, content, mtime)
             VALUES ('src/main.rs', 'main', 'function', 'rust', 1, 10, 'fn main() {}', 42)",
            [],
        ).unwrap();
        assert_eq!(db.file_mtime("src/main.rs").unwrap(), 42);
    }

    #[test]
    fn test_delete_file_chunks() {
        let (_tmp, db) = test_db();
        db.conn.execute(
            "INSERT INTO code_chunks (file_path, symbol_name, symbol_kind, language, start_line, end_line, content, mtime)
             VALUES ('a.rs', 'foo', 'function', 'rust', 1, 5, 'fn foo() {}', 1)",
            [],
        ).unwrap();
        db.conn.execute(
            "INSERT INTO code_chunks (file_path, symbol_name, symbol_kind, language, start_line, end_line, content, mtime)
             VALUES ('b.rs', 'bar', 'function', 'rust', 1, 5, 'fn bar() {}', 1)",
            [],
        ).unwrap();
        assert_eq!(db.chunk_count().unwrap(), 2);
        db.delete_file_chunks("a.rs").unwrap();
        assert_eq!(db.chunk_count().unwrap(), 1);
        assert_eq!(db.file_count().unwrap(), 1);
    }

    // ── Schema-migration regression tests (Sprint #2 HIGH) ──────────────────

    /// A freshly-opened DB must record the current schema version in
    /// rag_meta so that future ALTER TABLE migrations can key off it.
    #[test]
    fn fresh_db_records_current_schema_version() {
        let (_tmp, db) = test_db();
        let v = read_schema_version(&db.conn).unwrap();
        assert_eq!(
            v, RAG_SCHEMA_VERSION,
            "fresh DB must record current version"
        );
    }

    /// A DB created by an older OxideClaw build (pre-versioning, so no
    /// `schema_version` row exists) must be treated as version 0 and
    /// migrated up to current on open. This is the concrete "fallback"
    /// case: old data must keep working after a schema change.
    #[test]
    fn pre_versioning_db_is_migrated_to_current() {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join(".claude").join("rag.db");
        std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();

        // Simulate a pre-migration DB: create just the v1 tables and
        // rag_meta, but do NOT write a schema_version row.
        {
            let conn = Connection::open(&db_path).unwrap();
            conn.execute_batch(
                "CREATE TABLE code_chunks (
                    id INTEGER PRIMARY KEY, file_path TEXT NOT NULL,
                    symbol_name TEXT NOT NULL, symbol_kind TEXT NOT NULL,
                    language TEXT NOT NULL, start_line INTEGER NOT NULL,
                    end_line INTEGER NOT NULL, content TEXT NOT NULL,
                    mtime INTEGER NOT NULL
                );
                 CREATE TABLE rag_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);",
            )
            .unwrap();
            // Insert a real row so we can prove the data survives the migration.
            conn.execute(
                "INSERT INTO code_chunks (file_path, symbol_name, symbol_kind, language, start_line, end_line, content, mtime)
                 VALUES ('legacy.rs', 'old', 'function', 'rust', 1, 3, 'fn old(){}', 99)",
                [],
            )
            .unwrap();
            assert_eq!(read_schema_version(&conn).unwrap(), 0);
        }

        // Re-open through RagDb::open — this should detect v0, migrate
        // to the current version, and preserve the existing row.
        let db = RagDb::open(tmp.path()).unwrap();
        assert_eq!(
            read_schema_version(&db.conn).unwrap(),
            RAG_SCHEMA_VERSION,
            "migration must update the persisted version"
        );
        assert_eq!(
            db.chunk_count().unwrap(),
            1,
            "migration must preserve existing data"
        );
    }

    /// Re-opening an already-current DB must be idempotent — no duplicate
    /// rag_meta rows, no errors, version stays pinned.
    #[test]
    fn migration_is_idempotent() {
        let tmp = TempDir::new().unwrap();
        // Open twice.
        {
            let _db = RagDb::open(tmp.path()).unwrap();
        }
        let db = RagDb::open(tmp.path()).unwrap();

        // Exactly one row in rag_meta for schema_version.
        let count: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM rag_meta WHERE key = 'schema_version'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "duplicate schema_version rows after re-open");
        assert_eq!(read_schema_version(&db.conn).unwrap(), RAG_SCHEMA_VERSION);
    }

    /// A DB claiming a higher schema_version than this build supports
    /// must not fail — the user downgraded OxideClaw, and we should keep
    /// the old data usable rather than crash at startup.
    #[test]
    fn future_version_opens_without_error() {
        let tmp = TempDir::new().unwrap();
        // First open normally.
        {
            let _db = RagDb::open(tmp.path()).unwrap();
        }
        // Now simulate a newer build by forcibly bumping the version.
        {
            let conn = Connection::open(tmp.path().join(".claude").join("rag.db")).unwrap();
            conn.execute(
                "UPDATE rag_meta SET value = '9999' WHERE key = 'schema_version'",
                [],
            )
            .unwrap();
        }
        // Re-open should succeed (not panic, not error) and leave the
        // persisted version alone.
        let db = RagDb::open(tmp.path()).unwrap();
        assert_eq!(read_schema_version(&db.conn).unwrap(), 9999);
    }

    #[test]
    fn test_clear() {
        let (_tmp, db) = test_db();
        for i in 0..5 {
            db.conn.execute(
                "INSERT INTO code_chunks (file_path, symbol_name, symbol_kind, language, start_line, end_line, content, mtime)
                 VALUES (?1, 'sym', 'function', 'rust', 1, 5, 'code', 1)",
                [format!("file{i}.rs")],
            ).unwrap();
        }
        assert_eq!(db.chunk_count().unwrap(), 5);
        db.clear().unwrap();
        assert_eq!(db.chunk_count().unwrap(), 0);
    }
}
