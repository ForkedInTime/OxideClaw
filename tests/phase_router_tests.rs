/// Integration tests for detect_phase() — phase-declarative model routing.
use oxideclaw::router::{Phase, detect_phase};

#[test]
fn test_research_phase() {
    assert_eq!(
        detect_phase("what does the config module do?"),
        Phase::Research
    );
    assert_eq!(
        detect_phase("explain how the router works"),
        Phase::Research
    );
    assert_eq!(detect_phase("find all uses of RagDb"), Phase::Research);
}

#[test]
fn test_plan_phase() {
    assert_eq!(
        detect_phase("plan the implementation of persistent memory"),
        Phase::Plan
    );
    assert_eq!(
        detect_phase("how should we design the auto-rollback feature?"),
        Phase::Plan
    );
}

#[test]
fn test_edit_phase() {
    assert_eq!(
        detect_phase("implement the memory store with SQLite"),
        Phase::Edit
    );
    assert_eq!(detect_phase("fix the bug in the router"), Phase::Edit);
    assert_eq!(
        detect_phase("add a new slash command for memory"),
        Phase::Edit
    );
}

#[test]
fn test_review_phase() {
    assert_eq!(
        detect_phase("review the changes I just made"),
        Phase::Review
    );
    assert_eq!(
        detect_phase("check if this implementation is correct"),
        Phase::Review
    );
}

#[test]
fn test_ambiguous_defaults() {
    assert_eq!(detect_phase("hello"), Phase::Default);
    assert_eq!(detect_phase("thanks"), Phase::Default);
}
