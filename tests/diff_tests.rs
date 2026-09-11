#[test]
fn parse_unified_diff_single_hunk() {
    use rustyclaw::tui::diff::parse_unified_diff;
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
    use rustyclaw::tui::diff::parse_unified_diff;
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
