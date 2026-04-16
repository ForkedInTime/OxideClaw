use rustyclaw::browser::loop_detector::{fingerprint_action, LoopDetector};

// 1. Same inputs → same hash
#[test]
fn fingerprint_equality_same_inputs() {
    let h1 = fingerprint_action("click", "#submit", "");
    let h2 = fingerprint_action("click", "#submit", "");
    assert_eq!(h1, h2);
}

// 2. Different target → different hash
#[test]
fn fingerprint_differs_on_target() {
    let h1 = fingerprint_action("click", "#submit", "");
    let h2 = fingerprint_action("click", "#cancel", "");
    assert_ne!(h1, h2);
}

// 3. 11 records → window capped at 10
#[test]
fn window_prunes_oldest_at_eleven() {
    let mut d = LoopDetector::new();
    for i in 0..11 {
        d.record_action("click", &format!("#{i}"), "page");
    }
    assert_eq!(d.window_len(), 10);
}

// 4. Three identical actions → level-1 nudge ("different approach")
#[test]
fn three_identical_trips_level_one_nudge() {
    let mut d = LoopDetector::new();
    for _ in 0..3 {
        d.record_action("click", "#btn", "same page text");
    }
    let nudge = d.check_stagnation().expect("expected a nudge");
    assert!(nudge.contains("different approach"), "got: {nudge}");
}

// 5. After level 1, three more identical → level-2 nudge ("multiple times")
#[test]
fn three_more_identical_escalate_to_level_two() {
    let mut d = LoopDetector::new();
    // Trigger level 1
    for _ in 0..3 {
        d.record_action("click", "#btn", "same page text");
    }
    d.check_stagnation();
    // Trigger level 2 — window already has 3 identical, check_stagnation consumed level 0.
    // We need to push 3 more identical entries so tail is still identical.
    for _ in 0..3 {
        d.record_action("click", "#btn", "same page text");
    }
    let nudge = d.check_stagnation().expect("expected escalated nudge");
    assert!(nudge.contains("multiple times"), "got: {nudge}");
}

// 6. Third check → terminal "Stopping" nudge
#[test]
fn level_three_is_terminal_stop() {
    let mut d = LoopDetector::new();
    for _ in 0..3 {
        d.record_action("click", "#btn", "same page text");
    }
    d.check_stagnation(); // level 0 → emit NUDGES[0], nudge_level becomes 1
    for _ in 0..3 {
        d.record_action("click", "#btn", "same page text");
    }
    d.check_stagnation(); // level 1 → emit NUDGES[1], nudge_level becomes 2
    for _ in 0..3 {
        d.record_action("click", "#btn", "same page text");
    }
    let nudge = d.check_stagnation().expect("expected terminal nudge");
    assert!(nudge.contains("Stopping"), "got: {nudge}");
}

// 7. reset() clears window and resets nudge level
#[test]
fn reset_clears_window_and_nudge_level() {
    let mut d = LoopDetector::new();
    for _ in 0..3 {
        d.record_action("click", "#btn", "same page text");
    }
    d.check_stagnation(); // consumes level 0
    d.reset();
    assert_eq!(d.window_len(), 0);
    // After reset, 3 identical actions should produce level-1 nudge again
    for _ in 0..3 {
        d.record_action("click", "#btn", "same page text");
    }
    let nudge = d.check_stagnation().expect("expected nudge after reset");
    assert!(nudge.contains("different approach"), "got: {nudge}");
}

// 8. Same action but different page_text each time → no stagnation
#[test]
fn page_change_breaks_stagnation() {
    let mut d = LoopDetector::new();
    for i in 0..5 {
        d.record_action("click", "#btn", &format!("page content {i}"));
    }
    assert!(d.check_stagnation().is_none());
}
