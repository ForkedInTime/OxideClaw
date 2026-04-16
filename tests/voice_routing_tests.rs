use rustyclaw::voice::{voice_routes_to_browse, strip_browse_prefix};

#[test]
fn routes_browse_prefix() {
    assert!(voice_routes_to_browse("browse find the flight"));
    assert!(voice_routes_to_browse("Browser open flights"));
    assert!(voice_routes_to_browse("web find something"));
    assert!(voice_routes_to_browse("go to amazon.com"));
    assert!(voice_routes_to_browse("book a flight"));
    assert!(voice_routes_to_browse("shop for coffee"));
    assert!(voice_routes_to_browse("order pizza"));
}

#[test]
fn does_not_route_plain_find() {
    assert!(!voice_routes_to_browse("find the bug in my code"));
    assert!(!voice_routes_to_browse("what's the capital of france"));
    assert!(!voice_routes_to_browse("search for todo items"));
    assert!(!voice_routes_to_browse("look up documentation"));
}

#[test]
fn strip_preserves_case() {
    assert_eq!(strip_browse_prefix("browse Find Tokyo flights"), "Find Tokyo flights");
    assert_eq!(strip_browse_prefix("Book a Hotel in Paris"), "a Hotel in Paris");
    assert_eq!(strip_browse_prefix("GO TO example.com"), "example.com");
}
