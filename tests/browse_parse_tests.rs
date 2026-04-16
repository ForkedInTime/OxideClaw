use rustyclaw::commands::{parse_browse_command, CommandAction};
use rustyclaw::browser::browse_loop::BrowsePolicy;

#[test]
fn parses_plain_browse() {
    match parse_browse_command("find the cheapest flight") {
        CommandAction::Browse { goal, policy, max_steps } => {
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
        CommandAction::Browse { max_steps, goal, .. } => {
            assert_eq!(max_steps, Some(100));
            assert_eq!(goal, "research X");
        }
        _ => panic!(),
    }
}

#[test]
fn parses_ask_and_max_steps_combined() {
    match parse_browse_command("--ask --max-steps 25 quick check") {
        CommandAction::Browse { policy, max_steps, goal } => {
            assert_eq!(policy, BrowsePolicy::Ask);
            assert_eq!(max_steps, Some(25));
            assert_eq!(goal, "quick check");
        }
        _ => panic!(),
    }
}
