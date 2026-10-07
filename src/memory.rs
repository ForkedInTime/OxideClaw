/// Persistent project memory — stores decisions, preferences, patterns, and
/// contextual notes in a per-project SQLite database.
///
/// Each memory is a keyed (key, value) pair with a category and source.
/// FTS5 full-text search allows fuzzy retrieval; `build_context()` formats
/// the top-N items for injection into the system prompt.
///
/// Database location: `<cwd>/.claude/memory.db`. Memories used to share
/// `.claude/rag.db` with the code index; that index is regenerable and now
/// lives in the cache dir, so memories moved out on first open.
use anyhow::{Context, Result};
use rusqlite::Connection;
use std::fmt;
use std::path::{Path, PathBuf};
use tracing::debug;

/// Where `project`'s memories live.
pub fn memory_db_path(project: &Path) -> PathBuf {
    project.join(".claude").join("memory.db")
}

// ── Category ──────────────────────────────────────────────────────────────────

/// Memory category — used for filtering and display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Category {
    Decision,
    Preference,
    Pattern,
    Context,
    Custom(String),
}

impl Category {
    pub fn as_str(&self) -> &str {
        match self {
            Category::Decision => "decision",
            Category::Preference => "preference",
            Category::Pattern => "pattern",
            Category::Context => "context",
            Category::Custom(s) => s.as_str(),
        }
    }

    pub fn parse(s: &str) -> Self {
        match s {
            "decision" => Category::Decision,
            "preference" => Category::Preference,
            "pattern" => Category::Pattern,
            "context" => Category::Context,
            other => Category::Custom(other.to_string()),
        }
    }
}

impl fmt::Display for Category {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

// ── Memory ────────────────────────────────────────────────────────────────────

/// A single memory entry.
#[derive(Debug, Clone)]
#[allow(dead_code)] // fields consumed by future memory-UI overlay work
pub struct Memory {
    pub id: i64,
    pub key: String,
    pub value: String,
    pub category: Category,
    pub source: String,
    pub created_at: i64,
    pub updated_at: i64,
}

// ── MemoryStore ───────────────────────────────────────────────────────────────

/// Wraps the project's SQLite connection for memory CRUD operations.
pub struct MemoryStore {
    conn: Connection,
}

impl MemoryStore {
    /// Open (or create) the memory store for a project, first carrying over
    /// the memories of a pre-cache-dir `.claude/rag.db`.
    pub fn open(cwd: &Path) -> Result<Self> {
        crate::rag::retire_legacy_db(cwd);
        let store = Self::open_at(&memory_db_path(cwd))?;
        crate::rag::ensure_git_excluded_once(cwd);
        Ok(store)
    }

    /// Open the project's memory store only if it has one (or a
    /// pre-cache-dir `.claude/rag.db` whose memories move into one), so
    /// reading memories (the system prompt every -p/SDK/browse engine builds)
    /// never adds `.claude/memory.db` to a project that has none. Writes
    /// (`/memory add`, auto-capture) go through `open`.
    pub fn open_existing(cwd: &Path) -> Result<Option<Self>> {
        // Free when there is no rag.db. Another tool's rag.db, or an old
        // index with no memories, leaves no memory.db behind.
        crate::rag::retire_legacy_db(cwd);
        let path = memory_db_path(cwd);
        if !path.is_file() {
            return Ok(None);
        }
        let store = Self::open_at(&path)?;
        crate::rag::ensure_git_excluded_once(cwd);
        Ok(Some(store))
    }

    /// Open (or create) the memory database at `path`.
    pub(crate) fn open_at(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("Failed to create {}", dir.display()))?;
        }
        let conn = Connection::open(path).context("Failed to open memory database")?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;

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
        debug!("MemoryStore opened at {}", path.display());
        Ok(Self { conn })
    }

    /// Copy every memory from another database's `memory` table, keeping
    /// timestamps. A key already present here wins.
    pub(crate) fn import_from(&self, other: &Connection) -> Result<()> {
        let mut stmt = other.prepare(
            "SELECT id, key, value, category, source, created_at, updated_at FROM memory",
        )?;
        let rows = stmt
            .query_map([], row_to_memory)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let tx = self.conn.unchecked_transaction()?;
        for m in rows {
            tx.execute(
                "INSERT INTO memory (key, value, category, source, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6) ON CONFLICT(key) DO NOTHING",
                rusqlite::params![
                    m.key,
                    m.value,
                    m.category.as_str(),
                    m.source,
                    m.created_at,
                    m.updated_at
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    // ── Write ─────────────────────────────────────────────────────────────────

    /// Insert or update a memory entry by key.
    /// If the key already exists, the value, category, source, and updated_at
    /// are all replaced (upsert semantics).
    pub fn add(&self, key: &str, value: &str, category: Category, source: &str) -> Result<()> {
        // Check whether this key exists so we can choose INSERT vs UPDATE.
        // Using OR REPLACE would reset the id and re-fire the INSERT trigger,
        // breaking the FTS delete trigger. Manual upsert is safer.
        let exists: bool =
            self.conn
                .query_row("SELECT COUNT(*) FROM memory WHERE key = ?1", [key], |r| {
                    r.get::<_, i64>(0)
                })?
                > 0;

        if exists {
            self.conn.execute(
                "UPDATE memory SET value=?1, category=?2, source=?3, updated_at=unixepoch() WHERE key=?4",
                rusqlite::params![value, category.as_str(), source, key],
            )?;
        } else {
            self.conn.execute(
                "INSERT INTO memory (key, value, category, source) VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![key, value, category.as_str(), source],
            )?;
        }
        Ok(())
    }

    /// Auto-add a memory, skipping if a near-duplicate already exists.
    ///
    /// Deduplication is FTS-first: runs an FTS5 search against existing memories
    /// to get the top 10 candidates, then checks Jaccard word-overlap. If any
    /// candidate has overlap > 0.8, the entry is considered a duplicate and
    /// skipped. This is O(FTS) rather than O(N) over the full table.
    ///
    /// Returns `true` if a new memory was stored, `false` if skipped.
    pub fn add_auto(&self, text: &str, source: &str) -> Result<bool> {
        // FTS search for the top 10 nearest candidates.
        let candidates = self.search(text, 10)?;

        let text_words: std::collections::HashSet<&str> = text.split_whitespace().collect();

        for candidate in &candidates {
            if jaccard_overlap(&text_words, &candidate.value) > 0.8 {
                debug!(
                    "add_auto: skipping near-duplicate (candidate key={})",
                    candidate.key
                );
                return Ok(false);
            }
        }

        // Generate a collision-resistant key: first 4 words + 8-char content hash.
        let prefix = text
            .split_whitespace()
            .take(4)
            .collect::<Vec<_>>()
            .join("_")
            .to_lowercase();
        let hash8 = fnv32(text);
        let raw_key = format!("{}_{:08x}", prefix, hash8);
        let key = sanitize_key(&raw_key);

        let cat = auto_categorize(text);
        self.add(&key, text, cat, source)?;
        Ok(true)
    }

    // ── Read ──────────────────────────────────────────────────────────────────

    /// Return all memories, optionally filtered by category.
    /// Ordered by `updated_at DESC` (most recently touched first).
    pub fn list(&self, category: Option<Category>) -> Result<Vec<Memory>> {
        let rows = if let Some(cat) = category {
            let mut stmt = self.conn.prepare(
                "SELECT id, key, value, category, source, created_at, updated_at
                 FROM memory WHERE category = ?1 ORDER BY updated_at DESC",
            )?;
            stmt.query_map([cat.as_str()], row_to_memory)?
                .filter_map(|r| r.ok())
                .collect()
        } else {
            let mut stmt = self.conn.prepare(
                "SELECT id, key, value, category, source, created_at, updated_at
                 FROM memory ORDER BY updated_at DESC",
            )?;
            stmt.query_map([], row_to_memory)?
                .filter_map(|r| r.ok())
                .collect()
        };
        Ok(rows)
    }

    /// FTS5 search over key + value. Returns up to `limit` results.
    pub fn search(&self, query: &str, limit: i64) -> Result<Vec<Memory>> {
        let fts_query = crate::rag::search::sanitize_fts_query(query);
        if fts_query.is_empty() {
            return Ok(vec![]);
        }

        let mut stmt = self.conn.prepare(
            "SELECT m.id, m.key, m.value, m.category, m.source, m.created_at, m.updated_at
             FROM memory_fts
             JOIN memory m ON m.id = memory_fts.rowid
             WHERE memory_fts MATCH ?1
             ORDER BY memory_fts.rank
             LIMIT ?2",
        )?;

        let results = stmt
            .query_map(rusqlite::params![fts_query, limit], row_to_memory)?
            .filter_map(|r| r.ok())
            .collect();
        Ok(results)
    }

    /// Total memory count.
    #[allow(dead_code)] // exposed for tests and future /memory stats command
    pub fn count(&self) -> Result<i64> {
        let n: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM memory", [], |r| r.get(0))?;
        Ok(n)
    }

    /// Build a context string from the top `n` most-recent memories for
    /// injection into the system prompt.
    ///
    /// Format:
    /// ```text
    /// ## Project Memory
    /// - [decision] We use JWT for auth (2026-04-09)
    /// - [preference] Prefer functional React components (2026-04-09)
    /// ```
    ///
    /// Returns an empty string if no memories exist.
    pub fn build_context(&self, n: usize) -> Result<String> {
        let memories = self.list(None)?;
        if memories.is_empty() {
            return Ok(String::new());
        }

        let lines: Vec<String> = memories
            .iter()
            .take(n)
            .map(|m| {
                let date = format_unix_date(m.updated_at);
                format!("- [{}] {} ({})", m.category, m.value, date)
            })
            .collect();

        Ok(format!("## Project Memory\n{}", lines.join("\n")))
    }

    // ── Delete ────────────────────────────────────────────────────────────────

    /// Remove a memory by key. No-op if the key does not exist.
    pub fn forget(&self, key: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM memory WHERE key = ?1", [key])
            .context("Failed to delete memory entry")?;
        Ok(())
    }

    /// Remove what `/forget <query>` names: the memory whose key is exactly
    /// `query`, else every memory containing each word of `query` as a whole
    /// word. Returns the removed entries. `search` ORs prefix terms, which
    /// suits recall but made a forget of "the JWT decision" delete every
    /// note containing "the…" or "decision…".
    pub fn forget_matching(&self, query: &str) -> Result<Vec<Memory>> {
        let query = query.trim();
        let all = self.list(None)?;
        let hits: Vec<Memory> = match all.iter().find(|m| m.key == query) {
            Some(exact) => vec![exact.clone()],
            None => {
                let wanted = words(query);
                if wanted.is_empty() {
                    return Ok(Vec::new());
                }
                all.into_iter()
                    .filter(|m| {
                        let have = words(&format!("{} {}", m.key, m.value));
                        wanted.iter().all(|w| have.contains(w))
                    })
                    .collect()
            }
        };
        for m in &hits {
            self.forget(&m.key)?;
        }
        Ok(hits)
    }

    /// Delete all memories. Rebuilds the FTS index.
    pub fn clear_all(&self) -> Result<()> {
        self.conn.execute_batch(
            "DELETE FROM memory;
             INSERT INTO memory_fts(memory_fts) VALUES ('rebuild');",
        )?;
        Ok(())
    }
}

// ── Auto-capture ──────────────────────────────────────────────────────────────

/// Scan an assistant response for signal phrases and return candidate memory
/// strings. Callers can then pass each candidate to `MemoryStore::add_auto`.
pub fn auto_capture_memories(response: &str) -> Vec<String> {
    // Signal phrases that indicate a notable decision or preference.
    let signals = [
        "let's use",
        "we'll use",
        "decided to",
        "we decided",
        "going to use",
        "i'll use",
        "we should use",
        "we are using",
        "always use",
        "never use",
        "prefer to",
        "we prefer",
    ];

    let mut candidates = Vec::new();
    for line in response.lines() {
        let lower = line.to_lowercase();
        if signals.iter().any(|s| lower.contains(s)) {
            let trimmed = line.trim().to_string();
            if trimmed.len() >= 20 && trimmed.len() <= 300 {
                candidates.push(trimmed);
            }
        }
    }
    candidates
}

// ── auto_categorize ───────────────────────────────────────────────────────────

/// Classify text into a category using keyword heuristics.
pub fn auto_categorize(text: &str) -> Category {
    let lower = text.to_lowercase();

    let decision_signals = [
        "decided",
        "decision",
        "chose",
        "chosen",
        "let's use",
        "we'll use",
        "going to use",
        "will use",
        "selected",
        "opted",
        "adopted",
    ];
    let preference_signals = [
        "prefer",
        "prefers",
        "preferred",
        "likes",
        "always",
        "never",
        "want",
        "wants",
        "favor",
        "favors",
    ];
    let pattern_signals = [
        "typically",
        "usually",
        "pattern",
        "convention",
        "habit",
        "tends to",
        "tends",
        "often",
        "regularly",
        "approach",
    ];

    if decision_signals.iter().any(|s| lower.contains(s)) {
        Category::Decision
    } else if preference_signals.iter().any(|s| lower.contains(s)) {
        Category::Preference
    } else if pattern_signals.iter().any(|s| lower.contains(s)) {
        Category::Pattern
    } else {
        Category::Context
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn row_to_memory(row: &rusqlite::Row<'_>) -> rusqlite::Result<Memory> {
    let cat_str: String = row.get(3)?;
    Ok(Memory {
        id: row.get(0)?,
        key: row.get(1)?,
        value: row.get(2)?,
        category: Category::parse(&cat_str),
        source: row.get(4)?,
        created_at: row.get(5)?,
        updated_at: row.get(6)?,
    })
}

/// Format a Unix timestamp as YYYY-MM-DD (UTC, no-std).
fn format_unix_date(unix: i64) -> String {
    // Simple calculation — days since 1970-01-01.
    let secs = unix.max(0) as u64;
    let days_since_epoch = secs / 86400;

    // Gregorian calendar: algorithm from http://howardhinnant.github.io/date_algorithms.html
    let z = days_since_epoch as i64 + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };

    format!("{:04}-{:02}-{:02}", y, m, d)
}

/// Strip characters that would break a SQLite key used as a column value.
fn sanitize_key(key: &str) -> String {
    key.chars()
        .filter(|c| c.is_alphanumeric() || *c == '_')
        .take(64)
        .collect()
}

/// Lowercased alphanumeric words; `_` splits too, so generated keys
/// (`we_use_jwt_for_1a2b3c4d`) yield their words.
fn words(text: &str) -> std::collections::HashSet<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// Jaccard overlap between `a_words` (pre-built set) and the whitespace-split
/// words of `b`. Returns 0.0 when the union is empty.
fn jaccard_overlap(a_words: &std::collections::HashSet<&str>, b: &str) -> f64 {
    let b_words: std::collections::HashSet<&str> = b.split_whitespace().collect();
    if b_words.is_empty() && a_words.is_empty() {
        return 0.0;
    }
    let intersection = a_words.intersection(&b_words).count();
    let union = a_words.union(&b_words).count();
    if union == 0 {
        0.0
    } else {
        intersection as f64 / union as f64
    }
}

/// Simple FNV-1a 32-bit hash — no external deps, deterministic.
fn fnv32(s: &str) -> u32 {
    let mut hash: u32 = 2166136261;
    for byte in s.bytes() {
        hash ^= byte as u32;
        hash = hash.wrapping_mul(16777619);
    }
    hash
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // Tests for private helper functions only.
    // Public API tests live in tests/memory_tests.rs.

    #[test]
    fn test_format_unix_date() {
        // 2025-04-09 00:00:00 UTC = 1744156800
        assert_eq!(format_unix_date(1744156800), "2025-04-09");
        // 2026-04-09 00:00:00 UTC = 1775692800
        assert_eq!(format_unix_date(1775692800), "2026-04-09");
        // epoch
        assert_eq!(format_unix_date(0), "1970-01-01");
    }

    #[test]
    fn test_sanitize_key() {
        assert_eq!(sanitize_key("hello world!"), "helloworld");
        assert_eq!(sanitize_key("foo_bar_42"), "foo_bar_42");
        // hash suffix format: alphanumeric + underscore allowed
        assert_eq!(sanitize_key("we_decided_1a2b3c4d"), "we_decided_1a2b3c4d");
    }

    #[test]
    fn test_jaccard_overlap_identical() {
        let words: std::collections::HashSet<&str> = "foo bar baz".split_whitespace().collect();
        assert!((jaccard_overlap(&words, "foo bar baz") - 1.0).abs() < 1e-9);
    }

    #[test]
    fn test_jaccard_overlap_disjoint() {
        let words: std::collections::HashSet<&str> = "alpha beta".split_whitespace().collect();
        assert!((jaccard_overlap(&words, "gamma delta")).abs() < 1e-9);
    }

    #[test]
    fn test_jaccard_overlap_partial() {
        let words: std::collections::HashSet<&str> = "a b c d".split_whitespace().collect();
        // "a b c x" — intersection={a,b,c}, union={a,b,c,d,x} => 3/5 = 0.6
        let overlap = jaccard_overlap(&words, "a b c x");
        assert!((overlap - 0.6).abs() < 1e-9);
    }

    #[test]
    fn test_jaccard_overlap_empty_b() {
        let words: std::collections::HashSet<&str> = "foo".split_whitespace().collect();
        assert_eq!(jaccard_overlap(&words, ""), 0.0);
    }

    #[test]
    fn test_fnv32_deterministic() {
        assert_eq!(fnv32("hello"), fnv32("hello"));
        assert_ne!(fnv32("hello"), fnv32("world"));
    }
}
