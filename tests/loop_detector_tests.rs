use rustyclaw::browser::loop_detector::LoopDetector;

// 1. Three identical actions → level-1 nudge ("different approach")
#[test]
fn three_identical_trips_level_one_nudge() {
    let mut d = LoopDetector::new();
    for _ in 0..3 {
        d.record_action("click", "#btn", "same page text");
    }
    let nudge = d.check_stagnation().expect("expected a nudge");
    assert!(nudge.contains("different approach"), "got: {nudge}");
}

// 2. After level 1, three more identical → level-2 nudge ("multiple times")
#[test]
fn three_more_identical_escalate_to_level_two() {
    let mut d = LoopDetector::new();
    for _ in 0..3 {
        d.record_action("click", "#btn", "same page text");
    }
    d.check_stagnation();
    for _ in 0..3 {
        d.record_action("click", "#btn", "same page text");
    }
    let nudge = d.check_stagnation().expect("expected escalated nudge");
    assert!(nudge.contains("multiple times"), "got: {nudge}");
}

// 3. Third check → terminal "Stopping" nudge
#[test]
fn level_three_is_terminal_stop() {
    let mut d = LoopDetector::new();
    for _ in 0..3 {
        d.record_action("click", "#btn", "same page text");
    }
    d.check_stagnation();
    for _ in 0..3 {
        d.record_action("click", "#btn", "same page text");
    }
    d.check_stagnation();
    for _ in 0..3 {
        d.record_action("click", "#btn", "same page text");
    }
    let nudge = d.check_stagnation().expect("expected terminal nudge");
    assert!(nudge.contains("Stopping"), "got: {nudge}");
}

// 4. reset() clears window and resets nudge level
#[test]
fn reset_clears_window_and_nudge_level() {
    let mut d = LoopDetector::new();
    for _ in 0..3 {
        d.record_action("click", "#btn", "same page text");
    }
    d.check_stagnation();
    d.reset();
    // After reset, no stagnation from zero entries.
    assert!(d.check_stagnation().is_none());
    // And 3 identical actions should produce level-1 nudge again
    for _ in 0..3 {
        d.record_action("click", "#btn", "same page text");
    }
    let nudge = d.check_stagnation().expect("expected nudge after reset");
    assert!(nudge.contains("different approach"), "got: {nudge}");
}

// 5. Same action but different page_text each time → no stagnation
#[test]
fn page_change_breaks_stagnation() {
    let mut d = LoopDetector::new();
    for i in 0..5 {
        d.record_action("click", "#btn", &format!("page content {i}"));
    }
    assert!(d.check_stagnation().is_none());
}
