use oxideclaw::watch::scan_markers;

#[test]
fn scan_ai_markers() {
    let content = r#"
fn main() {
    // AI: add error handling here
    let x = 1;
    // TODO: clean up later
    // AI: refactor this block
}
"#;
    let markers = scan_markers(content, &["AI:", "AGENT:"]);
    assert_eq!(markers.len(), 2);
    assert!(markers[0].text.contains("add error handling"));
    assert!(markers[1].text.contains("refactor this block"));
    assert_eq!(markers[0].line, 3);
    assert_eq!(markers[1].line, 6);
}

#[test]
fn scan_no_markers() {
    let content = "fn main() { let x = 1; }";
    let markers = scan_markers(content, &["AI:", "AGENT:"]);
    assert!(markers.is_empty());
}

#[test]
fn scan_custom_markers() {
    let content = "// FIXME: broken\n// AGENT: implement auth";
    let markers = scan_markers(content, &["AGENT:"]);
    assert_eq!(markers.len(), 1);
    assert!(markers[0].text.contains("implement auth"));
}

#[test]
fn scan_marker_on_first_line() {
    let content = "// AI: very first line\nfn main() {}";
    let markers = scan_markers(content, &["AI:"]);
    assert_eq!(markers.len(), 1);
    assert_eq!(markers[0].line, 1);
    assert!(markers[0].text.contains("very first line"));
}

#[test]
fn scan_markers_respects_extension_via_patterns_stub() {
    // scan_markers itself has no notion of patterns. This test documents the
    // contract that pattern filtering happens at the `start_watcher` level,
    // not inside `scan_markers`. Kept minimal — we don't spin up a real
    // watcher in unit tests.
    let content = "// AI: inside any file extension";
    let markers = scan_markers(content, &["AI:"]);
    assert_eq!(markers.len(), 1, "scan_markers doesn't filter by extension");
}
