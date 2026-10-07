use oxideclaw::browser::browse_loop::BrowsePolicy;
use oxideclaw::commands::{CommandAction, parse_browse_command};

#[test]
fn parses_plain_browse() {
    match parse_browse_command("find the cheapest flight") {
        CommandAction::Browse {
            goal,
            policy,
            max_steps,
        } => {
            assert_eq!(goal, "find the cheapest flight");
            assert_eq!(policy, BrowsePolicy::Pattern);
            assert_eq!(max_steps, None);
        }
        _ => panic!("wrong variant"),
    }
}

#[test]
fn parses_yolo_flag() {
    match parse_browse_command("--yolo book the flight") {
        CommandAction::Browse { policy, goal, .. } => {
            assert_eq!(policy, BrowsePolicy::Yolo);
            assert_eq!(goal, "book the flight");
        }
        _ => panic!(),
    }
}

#[test]
fn parses_max_steps() {
    match parse_browse_command("--max-steps 100 research X") {
        CommandAction::Browse {
            max_steps, goal, ..
        } => {
            assert_eq!(max_steps, Some(100));
            assert_eq!(goal, "research X");
        }
        _ => panic!(),
    }
}

#[test]
fn parses_ask_and_max_steps_combined() {
    match parse_browse_command("--ask --max-steps 25 quick check") {
        CommandAction::Browse {
            policy,
            max_steps,
            goal,
        } => {
            assert_eq!(policy, BrowsePolicy::Ask);
            assert_eq!(max_steps, Some(25));
            assert_eq!(goal, "quick check");
        }
        _ => panic!(),
    }
}

/// A zero cap used to reach the engine as "no limit" and run 50 turns; it
/// is now rejected with usage like any other bad cap.
#[test]
fn a_zero_max_steps_never_starts_a_run() {
    assert_usage("--max-steps 0 quick check");
}

fn assert_usage(input: &str) {
    match parse_browse_command(input) {
        CommandAction::Message(m) => assert!(m.starts_with("Usage: /browse"), "{input:?}: {m}"),
        CommandAction::Browse { goal, .. } => panic!("{input:?} started a run with goal {goal:?}"),
        _ => panic!("{input:?}: unexpected action"),
    }
}

/// A bare `/browse` (or one with only flags) started an autonomous run with
/// an empty goal.
#[test]
fn a_missing_goal_shows_usage() {
    for input in [
        "",
        "   ",
        "--yolo",
        "--ask",
        "--max-steps 5",
        "--yolo --max-steps 3",
    ] {
        assert_usage(input);
    }
}

/// `--max-steps` with no value became the goal; a non-numeric value was
/// silently dropped, leaving the run uncapped.
#[test]
fn a_bad_max_steps_shows_usage() {
    for input in [
        "--max-steps",
        "find it --max-steps",
        "--max-steps abc find it",
        "--max-steps -3 x",
    ] {
        assert_usage(input);
    }
}
