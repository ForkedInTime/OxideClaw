/// Integration tests for MemoryStore — TDD, written before implementation.
use oxideclaw::memory::{Category, MemoryStore};
use tempfile::TempDir;

fn make_store() -> (TempDir, MemoryStore) {
    let tmp = TempDir::new().unwrap();
    let store = MemoryStore::open(tmp.path()).unwrap();
    (tmp, store)
}

// ── open ──────────────────────────────────────────────────────────────────────

#[test]
fn test_open_creates_db() {
    let (_tmp, store) = make_store();
    assert_eq!(store.count().unwrap(), 0);
}

// ── add + list ────────────────────────────────────────────────────────────────

#[test]
fn test_add_and_list() {
    let (_tmp, store) = make_store();
    store
        .add(
            "auth_lib",
            "We use JWT for auth",
            Category::Decision,
            "user",
        )
        .unwrap();
    store
        .add("stack", "TypeScript + React", Category::Preference, "user")
        .unwrap();

    let all = store.list(None).unwrap();
    assert_eq!(all.len(), 2);
    assert_eq!(store.count().unwrap(), 2);
}

#[test]
fn test_list_by_category() {
    let (_tmp, store) = make_store();
    store
        .add("k1", "use postgres", Category::Decision, "user")
        .unwrap();
    store
        .add("k2", "prefers dark mode", Category::Preference, "user")
        .unwrap();
    store
        .add("k3", "always validates input", Category::Pattern, "user")
        .unwrap();

    let decisions = store.list(Some(Category::Decision)).unwrap();
    assert_eq!(decisions.len(), 1);
    assert_eq!(decisions[0].key, "k1");

    let preferences = store.list(Some(Category::Preference)).unwrap();
    assert_eq!(preferences.len(), 1);
}

#[test]
fn test_add_updates_on_duplicate_key() {
    let (_tmp, store) = make_store();
    store
        .add("mykey", "original value", Category::Context, "user")
        .unwrap();
    store
        .add("mykey", "updated value", Category::Context, "user")
        .unwrap();

    let all = store.list(None).unwrap();
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].value, "updated value");
}

// ── search ────────────────────────────────────────────────────────────────────

#[test]
fn test_search_returns_relevant() {
    let (_tmp, store) = make_store();
    store
        .add(
            "auth",
            "We decided to use JWT authentication",
            Category::Decision,
            "user",
        )
        .unwrap();
    store
        .add(
            "db",
            "PostgreSQL is our primary database",
            Category::Decision,
            "user",
        )
        .unwrap();
    store
        .add(
            "style",
            "Prefer functional React components",
            Category::Preference,
            "user",
        )
        .unwrap();

    let results = store.search("JWT authentication", 10).unwrap();
    assert!(!results.is_empty());
    assert!(results.iter().any(|m| m.key == "auth"));
}

#[test]
fn test_search_empty_query() {
    let (_tmp, store) = make_store();
    store
        .add("k1", "some value", Category::Context, "user")
        .unwrap();
    let results = store.search("", 10).unwrap();
    assert!(results.is_empty());
}

#[test]
fn test_search_no_match() {
    let (_tmp, store) = make_store();
    store
        .add(
            "k1",
            "JWT authentication decided",
            Category::Decision,
            "user",
        )
        .unwrap();
    let results = store.search("zzznomatchzzz", 10).unwrap();
    assert!(results.is_empty());
}

#[test]
fn test_search_respects_limit() {
    let (_tmp, store) = make_store();
    for i in 0..10 {
        store
            .add(
                &format!("key_{i}"),
                &format!("common word authentication token session {i}"),
                Category::Context,
                "user",
            )
            .unwrap();
    }
    let results = store.search("authentication token session", 3).unwrap();
    assert!(results.len() <= 3);
}

// ── forget ────────────────────────────────────────────────────────────────────

#[test]
fn test_forget_removes_entry() {
    let (_tmp, store) = make_store();
    store
        .add("to_remove", "delete me", Category::Context, "user")
        .unwrap();
    store
        .add("keep", "keep me", Category::Context, "user")
        .unwrap();

    assert_eq!(store.count().unwrap(), 2);
    store.forget("to_remove").unwrap();
    assert_eq!(store.count().unwrap(), 1);

    let all = store.list(None).unwrap();
    assert_eq!(all[0].key, "keep");
}

#[test]
fn test_forget_nonexistent_key_is_ok() {
    let (_tmp, store) = make_store();
    // Should not error on missing key
    store.forget("nonexistent_key_xyz").unwrap();
}

// ── clear_all ─────────────────────────────────────────────────────────────────

#[test]
fn test_clear_all() {
    let (_tmp, store) = make_store();
    store.add("k1", "v1", Category::Decision, "user").unwrap();
    store.add("k2", "v2", Category::Preference, "user").unwrap();
    store.add("k3", "v3", Category::Context, "user").unwrap();

    assert_eq!(store.count().unwrap(), 3);
    store.clear_all().unwrap();
    assert_eq!(store.count().unwrap(), 0);
}

// ── add_auto (deduplication) ──────────────────────────────────────────────────

#[test]
fn test_add_auto_deduplicates_similar() {
    let (_tmp, store) = make_store();
    store
        .add_auto("We decided to use JWT for authentication tokens", "user")
        .unwrap();
    // Nearly identical — should be skipped
    let added = store
        .add_auto(
            "We decided to use JWT for authentication tokens here",
            "user",
        )
        .unwrap();
    assert!(!added, "should have been deduplicated");
    assert_eq!(store.count().unwrap(), 1);
}

#[test]
fn test_add_auto_allows_distinct_entries() {
    let (_tmp, store) = make_store();
    store
        .add_auto("We decided to use JWT for authentication", "user")
        .unwrap();
    let added = store
        .add_auto("PostgreSQL is our primary database", "user")
        .unwrap();
    assert!(added, "distinct entry should be added");
    assert_eq!(store.count().unwrap(), 2);
}

// ── build_context ─────────────────────────────────────────────────────────────

#[test]
fn test_build_context_empty() {
    let (_tmp, store) = make_store();
    let ctx = store.build_context(5).unwrap();
    assert!(ctx.is_empty());
}

#[test]
fn test_build_context_format() {
    let (_tmp, store) = make_store();
    store
        .add(
            "auth_choice",
            "We use JWT for auth",
            Category::Decision,
            "user",
        )
        .unwrap();
    store
        .add(
            "db_choice",
            "PostgreSQL database",
            Category::Decision,
            "user",
        )
        .unwrap();

    let ctx = store.build_context(10).unwrap();
    assert!(ctx.contains("## Project Memory"));
    assert!(ctx.contains("JWT"));
    assert!(ctx.contains("[decision]"));
}

#[test]
fn test_build_context_respects_limit() {
    let (_tmp, store) = make_store();
    for i in 0..20 {
        store
            .add(
                &format!("k{i}"),
                &format!("value number {i}"),
                Category::Context,
                "user",
            )
            .unwrap();
    }
    let ctx = store.build_context(5).unwrap();
    // Count the number of "- [" prefixes (one per memory item)
    let line_count = ctx
        .lines()
        .filter(|l: &&str| l.trim_start().starts_with("- ["))
        .count();
    assert!(line_count <= 5);
}

// ── auto_categorize ───────────────────────────────────────────────────────────

#[test]
fn test_auto_categorize_decision() {
    assert_eq!(
        oxideclaw::memory::auto_categorize("we decided to use Postgres"),
        Category::Decision
    );
    assert_eq!(
        oxideclaw::memory::auto_categorize("let's use React for the frontend"),
        Category::Decision
    );
    assert_eq!(
        oxideclaw::memory::auto_categorize("the team chose Rust over Go"),
        Category::Decision
    );
}

#[test]
fn test_auto_categorize_preference() {
    assert_eq!(
        oxideclaw::memory::auto_categorize("prefers dark mode always"),
        Category::Preference
    );
    assert_eq!(
        oxideclaw::memory::auto_categorize("user likes functional style"),
        Category::Preference
    );
    assert_eq!(
        oxideclaw::memory::auto_categorize("always format with rustfmt"),
        Category::Preference
    );
}

#[test]
fn test_auto_categorize_pattern() {
    assert_eq!(
        oxideclaw::memory::auto_categorize("typically wraps errors with anyhow"),
        Category::Pattern
    );
    assert_eq!(
        oxideclaw::memory::auto_categorize("usually runs cargo test before commit"),
        Category::Pattern
    );
}

#[test]
fn test_auto_categorize_default() {
    assert_eq!(
        oxideclaw::memory::auto_categorize("some random text with no signal words"),
        Category::Context
    );
}
