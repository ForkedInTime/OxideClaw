//! `/login` and `/logout`: Anthropic Console OAuth and provider keys.

use super::CommandAction;

const ANTHROPIC_WORDS: &[&str] = &["anthropic", "claude"];

fn provider_words() -> Vec<&'static str> {
    crate::api::PROVIDERS.iter().map(|p| p.prefix).collect()
}

fn usage(bad: &str) -> String {
    format!(
        "Unknown login target '{bad}'.\n\
         \n\
         /login                      status board\n\
         /login anthropic [profile]  Console OAuth (profile names go here, e.g. /login anthropic {bad})\n\
         /login anthropic manual     paste-the-code flow for SSH / headless\n\
         /login <provider> [open]    store an API key: {}\n\
         /logout [anthropic|<provider>]",
        provider_words().join(", ")
    )
}

pub(super) fn cmd_login(args: &str) -> CommandAction {
    let mut words = args.split_whitespace();
    let Some(first) = words.next() else {
        return CommandAction::LoginBoard;
    };
    let second = words.next();
    let first_l = first.to_ascii_lowercase();
    if ANTHROPIC_WORDS.contains(&first_l.as_str()) {
        return match second {
            Some("manual") => CommandAction::LoginAnthropic {
                profile: None,
                manual: true,
            },
            Some(p) => CommandAction::LoginAnthropic {
                profile: Some(p.to_string()),
                manual: false,
            },
            None => CommandAction::LoginAnthropic {
                profile: None,
                manual: false,
            },
        };
    }
    if provider_words().contains(&first_l.as_str()) {
        return CommandAction::LoginProvider {
            prefix: first_l,
            open_key_page: second == Some("open"),
        };
    }
    CommandAction::Message(usage(first))
}

pub(super) fn cmd_logout(args: &str) -> CommandAction {
    let word = args.split_whitespace().next().map(str::to_ascii_lowercase);
    match word.as_deref() {
        None => CommandAction::LogoutAnthropic,
        Some(w) if ANTHROPIC_WORDS.contains(&w) => CommandAction::LogoutAnthropic,
        Some(w) if provider_words().contains(&w) => CommandAction::LogoutProvider(w.to_string()),
        Some(w) => CommandAction::Message(usage(w)),
    }
}

#[cfg(test)]
mod parse_tests {
    use super::*;

    /// Same split the dispatcher performs, without needing a CommandContext.
    fn parse(input: &str) -> CommandAction {
        let input = input.trim_start_matches('/');
        let (name, args) = input.split_once(' ').unwrap_or((input, ""));
        match name {
            "login" => cmd_login(args),
            "logout" => cmd_logout(args),
            other => panic!("not a login command: {other}"),
        }
    }

    #[test]
    fn bare_login_opens_the_board() {
        assert!(matches!(parse("/login"), CommandAction::LoginBoard));
    }

    #[test]
    fn anthropic_login_variants() {
        assert!(matches!(
            parse("/login anthropic"),
            CommandAction::LoginAnthropic {
                profile: None,
                manual: false
            }
        ));
        match parse("/login anthropic work") {
            CommandAction::LoginAnthropic {
                profile: Some(p),
                manual: false,
            } => assert_eq!(p, "work"),
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            parse("/login anthropic manual"),
            CommandAction::LoginAnthropic {
                profile: None,
                manual: true
            }
        ));
        assert!(
            matches!(
                parse("/login claude"),
                CommandAction::LoginAnthropic {
                    profile: None,
                    manual: false
                }
            ),
            "alias"
        );
    }

    #[test]
    fn provider_login_variants() {
        match parse("/login groq") {
            CommandAction::LoginProvider {
                prefix,
                open_key_page: false,
            } => assert_eq!(prefix, "groq"),
            other => panic!("{other:?}"),
        }
        match parse("/login openrouter open") {
            CommandAction::LoginProvider {
                prefix,
                open_key_page: true,
            } => assert_eq!(prefix, "openrouter"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn unknown_word_lists_the_valid_ones() {
        match parse("/login work") {
            CommandAction::Message(m) => {
                assert!(
                    m.contains("anthropic")
                        && m.contains("groq")
                        && m.contains("/login anthropic work"),
                    "{m}"
                );
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn logout_variants() {
        assert!(matches!(parse("/logout"), CommandAction::LogoutAnthropic));
        assert!(matches!(
            parse("/logout anthropic"),
            CommandAction::LogoutAnthropic
        ));
        match parse("/logout groq") {
            CommandAction::LogoutProvider(p) => assert_eq!(p, "groq"),
            other => panic!("{other:?}"),
        }
        assert!(matches!(parse("/logout nope"), CommandAction::Message(_)));
    }
}
