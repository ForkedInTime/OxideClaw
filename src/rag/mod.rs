/// Local codebase RAG — tree-sitter AST indexing + SQLite FTS5 search.
///
/// Indexes the project's source code into per-symbol chunks, stored in a local
/// SQLite database.  When the agent needs context, FTS5 (BM25) retrieves the
/// most relevant code spans — so the LLM sees surgical context instead of
/// whole files.
///
/// Index location: `$XDG_CACHE_HOME/oxideclaw/rag/<project hash>.db`
/// (fallback `~/.cache/oxideclaw/rag/`), never inside the project; off when
/// neither is known. A project is the enclosing git work tree (see
/// `project_root`), walked from its root so `.gitignore`,
/// `.git/info/exclude`, the global excludes file and `.ignore` all apply,
/// and the filesystem root, `$HOME` and its ancestors are never indexed (a
/// work tree at `$HOME`, such as a dotfiles repo, is never the project).
///
/// Paid tools charge for this; we do it locally, for free, in a single binary
/// with zero external dependencies.
pub mod indexer;
pub mod search;

use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags, params};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use tracing::{debug, info, warn};

/// Ignore pattern covering the memory database and its WAL/SHM side files
/// at any depth.
const GIT_EXCLUDE_PATTERN: &str = "**/.claude/memory.db*";

/// The memory rows live inside the user's repo (`.claude/memory.db`);
/// `/checkpoint`, `/commit` and `/spawn merge` all `git add -A`, which would
/// commit them. Once per cwd per process, so a turn does not fork git every
/// time. The code index itself lives in the cache dir, outside the repo.
pub(crate) fn ensure_git_excluded_once(cwd: &Path) {
    static SEEN: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<PathBuf>>> =
        std::sync::OnceLock::new();
    let first = SEEN
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(cwd.to_path_buf());
    if first {
        ensure_git_excluded(cwd);
    }
}

/// Best effort: add [`GIT_EXCLUDE_PATTERN`] to the repo's private
/// `info/exclude` (shared by every linked worktree), never `.gitignore`.
fn ensure_git_excluded(cwd: &Path) {
    let Ok(out) = std::process::Command::new("git")
        .args(["rev-parse", "--git-path", "info/exclude"])
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
    else {
        return;
    };
    if !out.status.success() {
        return;
    }
    let rel = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if rel.is_empty() {
        return;
    }
    // From a subdirectory git prints a cwd-relative path (`../.git/...`).
    let path = cwd.join(rel);
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    if existing.lines().any(|l| l.trim() == GIT_EXCLUDE_PATTERN) {
        return;
    }
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let sep = if existing.is_empty() || existing.ends_with('\n') {
        ""
    } else {
        "\n"
    };
    let line = format!("{sep}{GIT_EXCLUDE_PATTERN}\n");
    let res = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .and_then(|mut f| std::io::Write::write_all(&mut f, line.as_bytes()));
    if let Err(e) = res {
        debug!("could not add memory.db to {}: {e}", path.display());
    }
}

/// Current schema version for the RAG database.
///
/// Bump this every time you add a column, table, or index that existing
/// databases won't pick up through `CREATE ... IF NOT EXISTS`. Then add a
/// migration step in `apply_migrations` for the new version.
///
/// Version history:
///   1 — Baseline: code_chunks, chunks_fts, rag_meta. (Indexes that lived in
///       `<project>/.claude/rag.db` also held the memory tables; memory now
///       has its own database, see `crate::memory`.)
///   2 — Same tables. Members of impls and classes became chunks of their
///       own, so every file is re-indexed once (its stored mtime is reset).
pub(crate) const RAG_SCHEMA_VERSION: i64 = 2;

/// Where code indexes live: `$XDG_CACHE_HOME/oxideclaw/rag`, falling back
/// to `~/.cache/oxideclaw/rag`. `None` when neither is known: the index is
/// then off rather than written somewhere relative (inside the project).
pub fn cache_index_dir() -> Option<PathBuf> {
    crate::config::Config::cache_dir().map(|d| d.join("rag"))
}

/// Why there is no index when `cache_index_dir` is `None`.
pub const NO_CACHE_DIR: &str =
    "no cache directory ($XDG_CACHE_HOME and the home directory are unset)";

/// `<index_dir>/<first 16 hex digits of sha256(canonical project root)>.db`.
///
/// Keyed by the canonical path so `./x`, a symlink to it and `x/../x` share
/// one index, and two checkouts of the same repo never do.
pub fn db_path_in(index_dir: &Path, project: &Path) -> PathBuf {
    let root = canonical(project);
    let digest = Sha256::digest(root.as_os_str().as_encoded_bytes());
    let name: String = digest[..8].iter().map(|b| format!("{b:02x}")).collect();
    index_dir.join(format!("{name}.db"))
}

fn canonical(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

/// Why `dir` may never be indexed, even on request: the filesystem root and
/// the home directory (or anything above it) hold far more than a project,
/// including credentials. `home` is passed in so tests never depend on the
/// real one.
pub fn index_refusal(dir: &Path, home: Option<&Path>) -> Option<&'static str> {
    let dir = canonical(dir);
    if dir.parent().is_none() {
        return Some("the filesystem root is never indexed");
    }
    if home.is_some_and(|h| canonical(h).starts_with(&dir)) {
        return Some("the home directory is never indexed");
    }
    None
}

/// Why `dir` is not indexed automatically (startup, before each prompt):
/// on top of `index_refusal`, it must sit inside a git work tree, and that
/// work tree must not be the home directory itself (a dotfiles repo).
pub fn auto_index_refusal(dir: &Path, home: Option<&Path>) -> Option<&'static str> {
    if let Some(why) = index_refusal(dir, home) {
        return Some(why);
    }
    match git_work_tree_root(&canonical(dir)) {
        None => Some("not inside a git repository"),
        Some(root) if index_refusal(&root, home).is_some() => {
            Some("the enclosing git repository is the home directory")
        }
        Some(_) => None,
    }
}

/// The nearest ancestor (or `dir` itself) holding `.git`: a directory in a
/// normal checkout, a file in a linked worktree or submodule.
pub(crate) fn git_work_tree_root(dir: &Path) -> Option<PathBuf> {
    dir.ancestors()
        .find(|a| a.join(".git").exists())
        .map(Path::to_path_buf)
}

/// The directory an index launched from `cwd` covers and is keyed by: the
/// enclosing git work tree, so `repo` and `repo/src` share one index of the
/// whole project. `cwd` itself outside git, or when the work tree is one
/// that is never indexed (a dotfiles repo at `$HOME`).
pub fn project_root(cwd: &Path, home: Option<&Path>) -> PathBuf {
    let cwd = canonical(cwd);
    match git_work_tree_root(&cwd) {
        Some(root) if index_refusal(&root, home).is_none() => root,
        _ => cwd,
    }
}

/// The index for a launch directory: what it covers and where it lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexTarget {
    /// Canonical project root (see `project_root`); stored paths are
    /// relative to it.
    pub root: PathBuf,
    /// Canonical launch directory, for showing stored paths.
    pub cwd: PathBuf,
    pub db_path: PathBuf,
}

impl IndexTarget {
    /// The index for `cwd` under `index_dir` (`None`: no cache dir known),
    /// or why there is none. `auto` applies `auto_index_refusal` (startup,
    /// per-prompt refresh, print/SDK context); otherwise `index_refusal`
    /// (`/index`, `/rag`, SDK `rag/search`).
    pub fn resolve(
        index_dir: Option<&Path>,
        cwd: &Path,
        home: Option<&Path>,
        auto: bool,
    ) -> std::result::Result<Self, &'static str> {
        let refusal = if auto {
            auto_index_refusal(cwd, home)
        } else {
            index_refusal(cwd, home)
        };
        if let Some(why) = refusal {
            return Err(why);
        }
        let index_dir = index_dir.ok_or(NO_CACHE_DIR)?;
        let root = project_root(cwd, home);
        Ok(Self {
            db_path: db_path_in(index_dir, &root),
            cwd: canonical(cwd),
            root,
        })
    }

    /// `resolve` with the real cache dir and home directory.
    pub fn for_cwd(cwd: &Path, auto: bool) -> std::result::Result<Self, &'static str> {
        Self::resolve(
            cache_index_dir().as_deref(),
            cwd,
            dirs::home_dir().as_deref(),
            auto,
        )
    }

    /// Open the index, creating it if needed, and retire a pre-cache-dir
    /// `<root>/.claude/rag.db`.
    pub fn open(&self) -> Result<RagDb> {
        retire_legacy_db(&self.root);
        RagDb::open_at(&self.db_path)
    }

    /// Open the index only if it exists: searching or inspecting never
    /// creates one.
    pub fn open_existing(&self) -> Result<Option<RagDb>> {
        if !self.db_path.is_file() {
            return Ok(None);
        }
        RagDb::open_at(&self.db_path).map(Some)
    }

    /// Bring the index up to date with the project.
    pub fn index(&self, db: &RagDb, force: bool) -> Result<indexer::IndexResult> {
        indexer::index_project(db, &self.root, force)
    }

    /// A stored (root-relative) path as seen from the launch directory:
    /// relative below it, absolute elsewhere in the project.
    pub fn display_path(&self, stored: &str) -> String {
        let abs = self.root.join(stored);
        match abs.strip_prefix(&self.cwd) {
            Ok(rel) => indexer::slash_path(rel),
            Err(_) => abs.to_string_lossy().into_owned(),
        }
    }

    /// `display_path` applied to every result.
    pub fn localize(&self, results: &mut [search::SearchResult]) {
        if self.root != self.cwd {
            for r in results {
                r.file_path = self.display_path(&r.file_path);
            }
        }
    }

    /// What to tell the user when there is no index to read yet.
    pub fn missing_message(&self) -> String {
        format!(
            "No code index for {} yet. Run /index to build it.",
            self.root.display()
        )
    }
}

/// Code context for a prompt from the project's index, refreshed first.
///
/// Used by every non-TUI turn (print, SDK, ACP). It never builds an index:
/// it stays empty until the TUI or `/index` has created one, and is off
/// wherever `auto_index_refusal` says auto-indexing is. `index_dir` is the
/// cache dir in production and a temp dir in tests.
pub fn auto_context(index_dir: Option<&Path>, cwd: &Path, user_input: &str) -> String {
    let index_dir = index_dir.map(Path::to_path_buf).or_else(cache_index_dir);
    let target =
        match IndexTarget::resolve(index_dir.as_deref(), cwd, dirs::home_dir().as_deref(), true) {
            Ok(t) => t,
            Err(why) => {
                debug!("RAG context off: {why}");
                return String::new();
            }
        };
    let db = match target.open_existing() {
        Ok(Some(db)) => db,
        Ok(None) | Err(_) => return String::new(),
    };

    // Skip if the index is empty (not yet built)
    if db.chunk_count().unwrap_or(0) == 0 {
        return String::new();
    }

    // Only the TUI indexes on its own; without this, print/SDK/ACP turns
    // inject whatever a past TUI run stored, including deleted files and
    // code this session already edited. Incremental, so cheap when idle.
    if let Err(e) = target.index(&db, false) {
        debug!("RAG refresh failed: {e}");
    }

    // Fetch more candidates, then filter by relevance threshold
    let mut results = match search::search(&db, user_input, 20) {
        Ok(r) => r,
        Err(e) => {
            debug!("RAG search failed: {e}");
            return String::new();
        }
    };

    if results.is_empty() {
        return String::new();
    }
    target.localize(&mut results);

    // Filter: only keep results with a decent relevance score.
    // FTS5 rank is negative (closer to 0 = more relevant); discard weak matches.
    let top_rank = results[0].rank;
    let threshold = if top_rank < -5.0 {
        top_rank * 0.3
    } else {
        top_rank * 0.5
    };
    let filtered: Vec<_> = results
        .into_iter()
        .filter(|r| r.rank <= threshold || r.rank <= top_rank * 0.8)
        .take(10) // cap at 10 injected chunks
        .collect();

    if filtered.is_empty() {
        return String::new();
    }

    // Context budget: ~12KB for rich models, keeps well within token limits
    let context = search::build_context(&filtered, 12288);
    if !context.is_empty() {
        debug!(
            "RAG injected {} results ({} chars)",
            filtered.len(),
            context.len()
        );
    }
    context
}

/// Retire `<project>/.claude/rag.db`, where the index (and `/memory`) lived
/// before the index moved to the cache dir. Its chunks may hold files that
/// are gitignored now, so it is never searched again. It is deleted only when
/// its tables show it is OxideClaw's, after its memories are copied to
/// `.claude/memory.db`; anything else at that path is left alone.
pub(crate) fn retire_legacy_db(project: &Path) {
    // `~/.claude/rag.db` belongs to Claude Code's directory: never touched.
    if crate::config::Config::is_claude_code_project(project) {
        return;
    }
    let legacy = project.join(".claude").join("rag.db");
    if !legacy.is_file() {
        return;
    }
    let memory_db = crate::memory::memory_db_path(project);
    let res = retire_legacy_db_at(&legacy, &memory_db);
    // Retirement can leave the project its first memory.db (TUI startup
    // reaches here through IndexTarget::open, not MemoryStore::open), and a
    // `git add -A` must not commit it.
    if memory_db.is_file() {
        ensure_git_excluded_once(project);
    }
    match res {
        Ok(true) => info!("removed the old code index {}", legacy.display()),
        Ok(false) => debug!("left {} alone: not an OxideClaw index", legacy.display()),
        Err(e) => warn!("could not retire {}: {e}", legacy.display()),
    }
}

/// `Ok(false)` when `legacy` is not an OxideClaw index (or cannot be read
/// as SQLite at all) and was left untouched.
fn retire_legacy_db_at(legacy: &Path, memory_db: &Path) -> Result<bool> {
    {
        // Read-write (never create) so a crash-left WAL can be recovered;
        // only `sqlite_master` and `memory` are read.
        let flags = OpenFlags::default().difference(OpenFlags::SQLITE_OPEN_CREATE);
        let conn = Connection::open_with_flags(legacy, flags)?;
        let tables: Vec<String> = match conn
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table'")
            .and_then(|mut stmt| {
                stmt.query_map([], |row| row.get(0))?
                    .collect::<rusqlite::Result<_>>()
            }) {
            Ok(t) => t,
            Err(_) => return Ok(false),
        };
        let has = |name: &str| tables.iter().any(|t| t == name);
        if !(has("code_chunks") && has("rag_meta")) {
            return Ok(false);
        }
        // Memories are the user's own words, not regenerable like chunks.
        // An empty table (every old index had one) creates no memory.db.
        if has("memory")
            && conn.query_row("SELECT EXISTS(SELECT 1 FROM memory)", [], |r| {
                r.get::<_, bool>(0)
            })?
        {
            crate::memory::MemoryStore::open_at(memory_db)?.import_from(&conn)?;
        }
    }
    let name = legacy.as_os_str();
    for suffix in ["", "-wal", "-shm"] {
        let mut p = name.to_os_string();
        p.push(suffix);
        if let Err(e) = std::fs::remove_file(&p)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            return Err(e.into());
        }
    }
    Ok(true)
}

/// The RAG database — owns a SQLite connection with FTS5 tables.
pub struct RagDb {
    pub conn: Connection,
    pub db_path: PathBuf,
}

impl RagDb {
    /// Open (or create) the index for `project` under `index_dir`, retiring
    /// a pre-cache-dir `<project>/.claude/rag.db` on the way.
    #[cfg(test)]
    pub(crate) fn open_in(index_dir: &Path, project: &Path) -> Result<Self> {
        retire_legacy_db(project);
        Self::open_at(&db_path_in(index_dir, project))
    }

    /// Open (or create) the index database at `db_path`.
    pub fn open_at(db_path: &Path) -> Result<Self> {
        if let Some(dir) = db_path.parent() {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("Failed to create {}", dir.display()))?;
        }
        let db_path = db_path.to_path_buf();
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
            );",
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
                // CREATE ... IF NOT EXISTS block in RagDb::open_at. Nothing to
                // ALTER — just record the version.
                conn.execute(
                    "INSERT INTO rag_meta (key, value) VALUES ('schema_version', ?1) \
                     ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                    params!["1"],
                )?;
                debug!("RAG schema migrated: 0 -> 1 (baseline recorded)");
            }
            1 => {
                // Rows stay searchable until the next pass replaces them;
                // no file has mtime 0, so that pass re-indexes every one.
                // The UPDATE trigger deletes each row from chunks_fts, which
                // fails on a row it never held (an index from before the FTS
                // table existed), so bring chunks_fts in sync first.
                conn.execute_batch(
                    "BEGIN;
                     INSERT INTO chunks_fts(chunks_fts) VALUES ('rebuild');
                     UPDATE code_chunks SET mtime = 0;
                     UPDATE rag_meta SET value = '2' WHERE key = 'schema_version';
                     COMMIT;",
                )?;
                debug!("RAG schema migrated: 1 -> 2 (chunks marked for re-index)");
            }
            // Future migrations go here. Example:
            //
            // 2 => {
            //     conn.execute_batch(
            //         "BEGIN;
            //          ALTER TABLE code_chunks ADD COLUMN embedding BLOB;
            //          UPDATE rag_meta SET value='3' WHERE key='schema_version';
            //          COMMIT;"
            //     )?;
            //     debug!("RAG schema migrated: 2 -> 3");
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
        let db = RagDb::open_at(&tmp.path().join("rag.db")).unwrap();
        (tmp, db)
    }

    #[test]
    fn open_existing_does_not_create_the_database() {
        let tmp = TempDir::new().unwrap();
        let idx = TempDir::new().unwrap();
        let t = IndexTarget::resolve(Some(idx.path()), tmp.path(), None, false).unwrap();
        assert!(t.open_existing().unwrap().is_none());
        assert!(!tmp.path().join(".claude").exists());
        assert_eq!(std::fs::read_dir(idx.path()).unwrap().count(), 0);
        t.open().unwrap();
        assert!(t.open_existing().unwrap().is_some());
        assert!(!tmp.path().join(".claude").exists());
    }

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}");
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    #[test]
    fn database_is_git_excluded_from_a_subdirectory_and_only_once() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path();
        git(repo, &["init", "-q"]);
        std::fs::create_dir_all(repo.join(".claude")).unwrap();
        std::fs::write(repo.join(".claude/settings.json"), "{}").unwrap();
        let sub = repo.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        // An exclude file without a trailing newline must not get the
        // pattern glued onto its last line.
        std::fs::write(repo.join(".git/info/exclude"), "*.log").unwrap();

        drop(crate::memory::MemoryStore::open(&sub).unwrap());
        drop(crate::memory::MemoryStore::open(repo).unwrap());
        ensure_git_excluded(&sub);

        let exclude = std::fs::read_to_string(repo.join(".git/info/exclude")).unwrap();
        assert_eq!(exclude, format!("*.log\n{GIT_EXCLUDE_PATTERN}\n"));
        let status = git(repo, &["status", "--porcelain", "--untracked-files=all"]);
        assert!(!status.contains("memory.db"), "{status}");
        assert!(status.contains(".claude/settings.json"), "{status}");
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
        let db_path = tmp.path().join("rag.db");

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

        // Re-open through RagDb::open_at — this should detect v0, migrate
        // to the current version, and preserve the existing row.
        let db = RagDb::open_at(&db_path).unwrap();
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

    /// A v1 index stored impls and classes whole; opening it as v2 marks
    /// every file for re-indexing so the members get their own chunks.
    #[test]
    fn v1_index_is_marked_for_reindex() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("rag.db");
        {
            let db = RagDb::open_at(&path).unwrap();
            db.conn
                .execute_batch(
                    "INSERT INTO code_chunks (file_path, symbol_name, symbol_kind, language, start_line, end_line, content, mtime)
                     VALUES ('lib.rs', 'Big', 'impl', 'rust', 1, 300, 'impl Big {}', 1700000000000000000);
                     UPDATE rag_meta SET value = '1' WHERE key = 'schema_version';",
                )
                .unwrap();
        }
        let db = RagDb::open_at(&path).unwrap();
        assert_eq!(read_schema_version(&db.conn).unwrap(), 2);
        assert_eq!(db.chunk_count().unwrap(), 1, "old rows stay searchable");
        assert_eq!(db.file_mtime("lib.rs").unwrap(), 0);
    }

    /// Re-opening an already-current DB must be idempotent — no duplicate
    /// rag_meta rows, no errors, version stays pinned.
    #[test]
    fn migration_is_idempotent() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("rag.db");
        // Open twice.
        {
            let _db = RagDb::open_at(&path).unwrap();
        }
        let db = RagDb::open_at(&path).unwrap();

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
        let path = tmp.path().join("rag.db");
        // First open normally.
        {
            let _db = RagDb::open_at(&path).unwrap();
        }
        // Now simulate a newer build by forcibly bumping the version.
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute(
                "UPDATE rag_meta SET value = '9999' WHERE key = 'schema_version'",
                [],
            )
            .unwrap();
        }
        // Re-open should succeed (not panic, not error) and leave the
        // persisted version alone.
        let db = RagDb::open_at(&path).unwrap();
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

    // ── Location, scope and the pre-cache-dir index ─────────────────────────

    /// The index lives at `<index dir>/<16 hex of sha256(canonical root)>.db`:
    /// never inside the project, the same file for every spelling of one
    /// directory, and a different file per project.
    #[test]
    fn db_path_is_the_hash_of_the_canonical_project_root() {
        let tmp = TempDir::new().unwrap();
        let idx = tmp.path().join("idx");
        let proj = tmp.path().join("proj");
        let other = tmp.path().join("other");
        std::fs::create_dir_all(proj.join("sub")).unwrap();
        std::fs::create_dir_all(&other).unwrap();

        let path = db_path_in(&idx, &proj);
        let canon = std::fs::canonicalize(&proj).unwrap();
        let digest = Sha256::digest(canon.as_os_str().as_encoded_bytes());
        let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(path, idx.join(format!("{}.db", &hex[..16])));

        assert_eq!(db_path_in(&idx, &proj.join("sub").join("..")), path);
        #[cfg(unix)]
        {
            let link = tmp.path().join("link");
            std::os::unix::fs::symlink(&proj, &link).unwrap();
            assert_eq!(db_path_in(&idx, &link), path);
        }
        assert_ne!(db_path_in(&idx, &other), path);

        let db = RagDb::open_in(&idx, &proj).unwrap();
        assert_eq!(db.db_path, path);
        assert!(path.is_file());
        assert!(
            !proj.join(".claude").exists(),
            "nothing is written into the project"
        );
    }

    /// `/`, `$HOME` and anything above `$HOME` are refused even on request;
    /// a directory below home is fine.
    #[test]
    fn root_and_home_are_never_indexed() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home").join("u");
        let proj = home.join("code").join("proj");
        std::fs::create_dir_all(&proj).unwrap();

        assert!(index_refusal(Path::new("/"), None).is_some());
        assert!(index_refusal(Path::new("/"), Some(&home)).is_some());
        assert_eq!(
            index_refusal(&home, Some(&home)),
            Some("the home directory is never indexed")
        );
        assert!(index_refusal(&home.join("code").join(".."), Some(&home)).is_some());
        assert!(index_refusal(home.parent().unwrap(), Some(&home)).is_some());
        assert_eq!(index_refusal(&proj, Some(&home)), None);
        assert_eq!(index_refusal(&proj, None), None);

        // A git repository at home (dotfiles) does not make home, or a
        // plain directory under it, a project.
        std::fs::create_dir(home.join(".git")).unwrap();
        assert!(auto_index_refusal(&home, Some(&home)).is_some());
        assert_eq!(
            auto_index_refusal(&proj, Some(&home)),
            Some("the enclosing git repository is the home directory")
        );
    }

    /// Auto-indexing (TUI startup, every prompt, print/SDK refresh) needs a
    /// git work tree; a subdirectory of one, or a linked worktree whose
    /// `.git` is a file, counts.
    #[test]
    fn auto_index_needs_a_git_work_tree() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let plain = home.join("downloads");
        let repo = home.join("repo");
        let worktree = home.join("wt");
        std::fs::create_dir_all(&plain).unwrap();
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(repo.join("src")).unwrap();
        std::fs::create_dir_all(&worktree).unwrap();
        std::fs::write(worktree.join(".git"), "gitdir: ../repo/.git/worktrees/wt\n").unwrap();

        assert_eq!(
            auto_index_refusal(&plain, Some(&home)),
            Some("not inside a git repository")
        );
        assert_eq!(auto_index_refusal(&repo, Some(&home)), None);
        assert_eq!(auto_index_refusal(&repo.join("src"), Some(&home)), None);
        assert_eq!(auto_index_refusal(&worktree, Some(&home)), None);
        // Explicit /index still works outside a repository.
        assert_eq!(index_refusal(&plain, Some(&home)), None);
    }

    /// Print/SDK turns never index a directory outside a git repository,
    /// and never search one even when an explicit /index built it.
    #[test]
    fn non_git_directories_get_no_automatic_context() {
        let proj = TempDir::new().unwrap();
        let idx = TempDir::new().unwrap();
        std::fs::write(
            proj.path().join("billing.rs"),
            "fn compute_invoice_total() -> u32 { 0 }\n",
        )
        .unwrap();
        assert!(auto_context(Some(idx.path()), proj.path(), "compute invoice total").is_empty());
        assert_eq!(
            std::fs::read_dir(idx.path()).unwrap().count(),
            0,
            "no index may be created"
        );

        let db = RagDb::open_in(idx.path(), proj.path()).unwrap();
        indexer::index_project(&db, proj.path(), true).unwrap();
        assert!(db.chunk_count().unwrap() > 0);
        assert!(auto_context(Some(idx.path()), proj.path(), "compute invoice total").is_empty());

        // The same project inside a work tree does get context.
        std::fs::create_dir(proj.path().join(".git")).unwrap();
        let ctx = auto_context(Some(idx.path()), proj.path(), "compute invoice total");
        assert!(ctx.contains("compute_invoice_total"), "{ctx}");
    }

    fn write_files(root: &Path, files: &[(&str, &str)]) {
        for (rel, body) in files {
            let path = root.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, body).unwrap();
        }
    }

    fn stored_files(db: &RagDb) -> Vec<String> {
        let mut stmt = db
            .conn
            .prepare("SELECT DISTINCT file_path FROM code_chunks ORDER BY file_path")
            .unwrap();
        stmt.query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    /// Launched from a directory the repository ignores (`/secrets/`, a
    /// nested `config/local/`, a hidden `.private/`), nothing in it is
    /// indexed: the walk starts at the work-tree root, so the parent rules
    /// that exclude it apply.
    #[test]
    fn launching_from_a_gitignored_directory_indexes_nothing_in_it() {
        let home = TempDir::new().unwrap();
        let idx = TempDir::new().unwrap();
        let repo = home.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        write_files(
            &repo,
            &[
                (".gitignore", "/secrets/\nconfig/local/\n.private/\n"),
                ("src/app.rs", "fn start_app() {}\n"),
                (
                    "secrets/keys.py",
                    "def signing_key():\n    return 'sk-live'\n",
                ),
                ("config/local/creds.rs", "fn local_creds() {}\n"),
                (".private/notes.rs", "fn private_notes() {}\n"),
            ],
        );

        for launch in ["secrets", "config/local", ".private"] {
            let cwd = repo.join(launch);
            let target =
                IndexTarget::resolve(Some(idx.path()), &cwd, Some(home.path()), true).unwrap();
            assert_eq!(target.root, canonical(&repo), "{launch}");
            let db = target.open().unwrap();
            target.index(&db, true).unwrap();
            assert_eq!(stored_files(&db), ["src/app.rs"], "{launch}");
            for secret in ["signing_key", "local_creds", "private_notes"] {
                let ctx = auto_context(Some(idx.path()), &cwd, secret);
                assert!(!ctx.contains(secret), "{launch}: {secret} leaked:\n{ctx}");
            }

            // Indexing the ignored directory on its own yields nothing too.
            let alone = RagDb::open_at(&idx.path().join("alone.db")).unwrap();
            indexer::index_project(&alone, &cwd, true).unwrap();
            assert_eq!(alone.chunk_count().unwrap(), 0, "{launch}");
        }
    }

    /// `repo` and `repo/src` are one project: one database, keyed by the
    /// work-tree root, covering the whole tree, with paths shown relative
    /// to where the user launched.
    #[test]
    fn a_subdirectory_launch_shares_the_project_index() {
        let home = TempDir::new().unwrap();
        let idx = TempDir::new().unwrap();
        let repo = home.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        write_files(
            &repo,
            &[
                (
                    "src/billing.rs",
                    "fn compute_invoice_total() -> u32 { 0 }\n",
                ),
                ("lib/tax.rs", "fn compute_invoice_tax() -> u32 { 0 }\n"),
            ],
        );
        let at = |dir: &Path| {
            IndexTarget::resolve(Some(idx.path()), dir, Some(home.path()), true).unwrap()
        };
        let top = at(&repo);
        let sub = at(&repo.join("src"));
        assert_eq!(sub.root, top.root);
        assert_eq!(sub.db_path, top.db_path);
        assert_eq!(sub.db_path, db_path_in(idx.path(), &repo));

        let db = sub.open().unwrap();
        sub.index(&db, false).unwrap();
        assert_eq!(stored_files(&db), ["lib/tax.rs", "src/billing.rs"]);
        assert_eq!(sub.display_path("src/billing.rs"), "billing.rs");
        assert_eq!(
            sub.display_path("lib/tax.rs"),
            canonical(&repo).join("lib/tax.rs").to_string_lossy()
        );
        assert_eq!(top.display_path("src/billing.rs"), "src/billing.rs");

        let ctx = auto_context(Some(idx.path()), &repo.join("src"), "compute invoice total");
        assert!(ctx.contains("billing.rs"), "{ctx}");
        assert!(!ctx.contains("src/billing.rs"), "{ctx}");
        assert_eq!(
            std::fs::read_dir(idx.path())
                .unwrap()
                .filter(|e| e.as_ref().unwrap().path().extension() == Some("db".as_ref()))
                .count(),
            1,
            "one database"
        );

        // Outside git, an explicit /index covers the directory itself.
        let plain = home.path().join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        let t = IndexTarget::resolve(Some(idx.path()), &plain, Some(home.path()), false).unwrap();
        assert_eq!(t.root, canonical(&plain));
        // Under a dotfiles repo at $HOME, too: $HOME is never the project.
        std::fs::create_dir(home.path().join(".git")).unwrap();
        let t = IndexTarget::resolve(Some(idx.path()), &plain, Some(home.path()), false).unwrap();
        assert_eq!(t.root, canonical(&plain));
    }

    /// Without a cache dir there is no index, never one somewhere relative.
    #[test]
    fn no_cache_dir_means_no_index() {
        let home = TempDir::new().unwrap();
        let repo = home.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        for auto in [true, false] {
            assert_eq!(
                IndexTarget::resolve(None, &repo, Some(home.path()), auto),
                Err(NO_CACHE_DIR)
            );
        }
    }

    /// Reading an index (search, status, clear) never creates one.
    #[test]
    fn open_existing_creates_nothing() {
        let home = TempDir::new().unwrap();
        let idx = TempDir::new().unwrap();
        let repo = home.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        let t = IndexTarget::resolve(Some(idx.path()), &repo, Some(home.path()), false).unwrap();
        assert!(t.open_existing().unwrap().is_none());
        assert_eq!(std::fs::read_dir(idx.path()).unwrap().count(), 0);
        t.open().unwrap();
        assert!(t.open_existing().unwrap().is_some());
    }

    /// An old-layout `<project>/.claude/rag.db`: index tables and memories
    /// in one WAL-mode database, as OxideClaw wrote it before the move.
    fn write_legacy_db(project: &Path) -> PathBuf {
        let legacy = project.join(".claude").join("rag.db");
        let db = RagDb::open_at(&legacy).unwrap();
        db.conn
            .execute(
                "INSERT INTO code_chunks (file_path, symbol_name, symbol_kind, language, start_line, end_line, content, mtime)
                 VALUES ('config.local.js', 'apiKey', 'lexical_declaration', 'javascript', 1, 1, 'const apiKey = \"sk-live\"', 1)",
                [],
            )
            .unwrap();
        let mem = crate::memory::MemoryStore::open_at(&legacy).unwrap();
        mem.add(
            "auth_lib",
            "We use JWT for auth",
            crate::memory::Category::Decision,
            "user",
        )
        .unwrap();
        legacy
    }

    /// The old in-project index is never searched: opening the new one
    /// deletes it, after carrying its memories over to `.claude/memory.db`.
    #[test]
    fn legacy_oxideclaw_index_is_deleted_and_its_memories_kept() {
        let proj = TempDir::new().unwrap();
        let idx = TempDir::new().unwrap();
        let legacy = write_legacy_db(proj.path());
        std::fs::write(proj.path().join(".claude").join("rag.db-shm"), "stale").unwrap();

        let db = RagDb::open_in(idx.path(), proj.path()).unwrap();
        assert!(!legacy.exists(), "old index must be deleted");
        for leftover in ["rag.db-wal", "rag.db-shm"] {
            assert!(
                !proj.path().join(".claude").join(leftover).exists(),
                "{leftover}"
            );
        }
        assert_eq!(db.chunk_count().unwrap(), 0, "old chunks must not be read");

        let mem = crate::memory::MemoryStore::open(proj.path()).unwrap();
        let all = mem.list(None).unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].value, "We use JWT for auth");
        assert_eq!(all[0].category, crate::memory::Category::Decision);
        assert_eq!(
            mem.search("JWT", 5).unwrap().len(),
            1,
            "FTS must cover imports"
        );
    }

    /// Memory is usually what touches the old file first (the system prompt
    /// opens it every session): it retires it the same way.
    #[test]
    fn memory_store_retires_the_legacy_index_too() {
        let proj = TempDir::new().unwrap();
        let legacy = write_legacy_db(proj.path());
        let mem = crate::memory::MemoryStore::open(proj.path()).unwrap();
        assert!(!legacy.exists());
        assert_eq!(mem.count().unwrap(), 1);
        assert!(crate::memory::memory_db_path(proj.path()).is_file());
    }

    /// Retiring the old index from TUI startup (IndexTarget::open, not
    /// MemoryStore::open) wrote `.claude/memory.db` without the git exclude,
    /// so the next `git add -A` committed it.
    #[test]
    fn retiring_the_legacy_index_excludes_the_memory_db_it_leaves() {
        let proj = TempDir::new().unwrap();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(proj.path())
                .output()
                .unwrap()
        };
        if !git(&["init", "-q"]).status.success() {
            return; // no git here
        }
        let legacy = write_legacy_db(proj.path());
        retire_legacy_db(proj.path());
        assert!(!legacy.exists());
        assert!(crate::memory::memory_db_path(proj.path()).is_file());
        let exclude =
            std::fs::read_to_string(proj.path().join(".git").join("info").join("exclude"))
                .unwrap_or_default();
        assert!(
            exclude.lines().any(|l| l.trim() == GIT_EXCLUDE_PATTERN),
            "{exclude}"
        );
        let status = String::from_utf8(git(&["status", "--porcelain"]).stdout).unwrap();
        assert!(!status.contains(".claude"), "{status}");
    }

    /// An old index with no memories is just deleted: no memory.db appears.
    #[test]
    fn legacy_index_without_memories_leaves_no_memory_db() {
        let proj = TempDir::new().unwrap();
        let legacy = proj.path().join(".claude").join("rag.db");
        RagDb::open_at(&legacy).unwrap();
        crate::memory::MemoryStore::open_at(&legacy).unwrap();
        assert!(
            crate::memory::MemoryStore::open_existing(proj.path())
                .unwrap()
                .is_none()
        );
        assert!(!legacy.exists());
        assert!(!crate::memory::memory_db_path(proj.path()).exists());
    }

    /// Reading memories next to someone else's `.claude/rag.db` must not
    /// create `.claude/memory.db` (or touch git's exclude file).
    #[test]
    fn open_existing_creates_nothing_beside_a_foreign_rag_db() {
        let proj = TempDir::new().unwrap();
        let path = proj.path().join(".claude").join("rag.db");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "my notes, not a database\n").unwrap();
        assert!(
            crate::memory::MemoryStore::open_existing(proj.path())
                .unwrap()
                .is_none()
        );
        assert!(path.is_file());
        assert!(!crate::memory::memory_db_path(proj.path()).exists());
    }

    /// A `.claude/rag.db` that is not OxideClaw's (another tool's SQLite
    /// file, or not SQLite at all) is left exactly as it is.
    #[test]
    fn foreign_rag_db_is_left_alone() {
        let foreign_sqlite = TempDir::new().unwrap();
        let path = foreign_sqlite.path().join(".claude").join("rag.db");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        Connection::open(&path)
            .unwrap()
            .execute_batch("CREATE TABLE documents (id INTEGER PRIMARY KEY, body TEXT);")
            .unwrap();
        let not_sqlite = TempDir::new().unwrap();
        let text = not_sqlite.path().join(".claude").join("rag.db");
        std::fs::create_dir_all(text.parent().unwrap()).unwrap();
        std::fs::write(&text, "my notes, not a database\n").unwrap();

        for (project, file) in [(foreign_sqlite.path(), &path), (not_sqlite.path(), &text)] {
            let before = std::fs::read(file).unwrap();
            let idx = TempDir::new().unwrap();
            RagDb::open_in(idx.path(), project).unwrap();
            crate::memory::MemoryStore::open(project).unwrap();
            assert_eq!(std::fs::read(file).unwrap(), before, "{}", file.display());
        }
    }
}
