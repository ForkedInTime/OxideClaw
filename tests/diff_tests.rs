#[test]
fn parse_unified_diff_single_hunk() {
    use oxideclaw::tui::diff::parse_unified_diff;
    let diff = "\
diff --git a/src/main.rs b/src/main.rs
index abc..def 100644
--- a/src/main.rs
+++ b/src/main.rs
@@ -1,3 +1,4 @@
 fn main() {
+    println!(\"hello\");
     let x = 1;
 }
";
    let file_diffs = parse_unified_diff(diff);
    assert_eq!(file_diffs.len(), 1);
    assert_eq!(file_diffs[0].path, "src/main.rs");
    assert_eq!(file_diffs[0].hunks.len(), 1);
    assert!(
        file_diffs[0].hunks[0]
            .lines
            .iter()
            .any(|l| l.content.contains("println"))
    );
}

#[test]
fn parse_multi_file_diff() {
    use oxideclaw::tui::diff::parse_unified_diff;
    let diff = "\
diff --git a/a.rs b/a.rs
--- a/a.rs
+++ b/a.rs
@@ -1 +1 @@
-old
+new
diff --git a/b.rs b/b.rs
--- a/b.rs
+++ b/b.rs
@@ -1 +1,2 @@
 keep
+added
";
    let files = parse_unified_diff(diff);
    assert_eq!(files.len(), 2);
    assert_eq!(files[0].path, "a.rs");
    assert_eq!(files[1].path, "b.rs");
}

#[test]
fn hunk_lines_that_look_like_file_headers_are_counted() {
    use oxideclaw::tui::diff::{DiffLineKind, parse_unified_diff};
    // Dropping Markdown front matter yields `----` body lines; adding `++i;`
    // yields `+++i;`. Both are hunk content, not `---`/`+++` file headers.
    let diff = "\
diff --git a/a.md b/a.md
index 1111111..2222222 100644
--- a/a.md
+++ b/a.md
@@ -1,3 +1 @@
----
-title: x
----
diff --git a/b.c b/b.c
index 3333333..4444444 100644
--- a/b.c
+++ b/b.c
@@ -1 +1,2 @@
 int i;
+++i;
";
    let files = parse_unified_diff(diff);
    assert_eq!(files.len(), 2);
    assert_eq!((files[0].additions, files[0].deletions), (0, 3));
    assert_eq!((files[1].additions, files[1].deletions), (1, 0));
    let added = &files[1].hunks[0].lines[1];
    assert_eq!(added.kind, DiffLineKind::Added);
    assert_eq!(added.content, "++i;");
}

/// Without git's a/ b/ prefixes (diff.noprefix) the path came only from a
/// " b/" split, so the file was dropped and its counts were added to the
/// next one.
#[test]
fn noprefix_and_quoted_paths_are_read_from_the_file_headers() {
    use oxideclaw::tui::diff::parse_unified_diff;

    let diff = "\
diff --git src/a.rs src/a.rs
index 1..2 100644
--- src/a.rs
+++ src/a.rs
@@ -1 +1,2 @@
 x
+y
diff --git \"a/docs/\\346\\227\\245.md\" \"b/docs/\\346\\227\\245.md\"
index 1..2 100644
--- \"a/docs/\\346\\227\\245.md\"
+++ \"b/docs/\\346\\227\\245.md\"
@@ -1 +1 @@
-old
+new
diff --git my file.txt my file.txt
deleted file mode 100644
index 1..0
--- my file.txt\t
+++ /dev/null
@@ -1 +0,0 @@
-gone
";
    let files = parse_unified_diff(diff);
    let summary: Vec<_> = files
        .iter()
        .map(|f| (f.path.as_str(), f.additions, f.deletions))
        .collect();
    assert_eq!(
        summary,
        [
            ("src/a.rs", 1, 0),
            ("docs/日.md", 1, 1),
            ("my file.txt", 0, 1)
        ]
    );
}
