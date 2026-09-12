//! `/login` and `/logout`: Anthropic Console OAuth and provider keys.

use super::CommandAction;
use crate::api::ProviderDef;
use crate::auth::keystore::Keystore;
use crate::config::Config;

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

pub fn anthropic_status(config: &Config) -> String {
    if let Some(info) = config.auth.profile_info() {
        let who = match (&info.email, &info.organization) {
            (Some(e), Some(o)) => format!("signed in as {e} · org {o}"),
            (Some(e), None) => format!("signed in as {e}"),
            (None, Some(o)) => format!("signed in · org {o}"),
            (None, None) => format!("signed in (profile '{}')", info.name),
        };
        let expiry = match info.expires_at {
            Some(t) => {
                let left = t - crate::auth::oauth::now_unix();
                if left <= 0 {
                    "token expired, refreshes on use".to_string()
                } else {
                    format!("expires in {} min", left / 60)
                }
            }
            None => "no expiry recorded".to_string(),
        };
        return format!("{who} · {expiry}");
    }
    if !config.api_key.is_empty() {
        let kind = if config.auth_is_oauth {
            "OAuth token"
        } else {
            "API key"
        };
        return format!(
            "{kind} via {}",
            config
                .auth_source
                .as_deref()
                .unwrap_or("apiKeyHelper / file descriptor")
        );
    }
    "not signed in · Enter to sign in with your Console account".to_string()
}

pub fn provider_status(p: &ProviderDef, keystore: &Keystore) -> String {
    match p.prefix {
        "lmstudio" => {
            return match std::env::var("LM_STUDIO_HOST") {
                Ok(h) if !h.trim().is_empty() => format!("host {h} (shell env)"),
                _ => "needs LM_STUDIO_HOST in your shell".to_string(),
            };
        }
        "openai-compat" => {
            let url = std::env::var("OPENAI_BASE_URL")
                .ok()
                .filter(|u| !u.trim().is_empty());
            let key = keystore.source("OPENAI_API_KEY");
            return match (url, key) {
                (Some(u), Some(s)) => format!("{u} · key via {}", s.describe()),
                (Some(u), None) => format!("{u} · no OPENAI_API_KEY"),
                (None, _) => "needs OPENAI_BASE_URL in your shell".to_string(),
            };
        }
        _ => {}
    }
    match keystore
        .source(p.key_env)
        .or_else(|| keystore.source("OPENAI_API_KEY"))
    {
        Some(s) => format!("key via {}", s.describe()),
        None => format!(
            "not configured · keys at {}",
            p.key_url.trim_start_matches("https://")
        ),
    }
}

/// Board lines and matching selectable ids (a slash command, or "" for an
/// informational row). Numbering follows the picker convention.
pub fn board_rows(config: &Config, ollama_models: &[String]) -> (Vec<String>, Vec<String>) {
    let mut lines = vec!["Credentials\n".to_string()];
    let mut ids = Vec::new();
    let mut n = 1;
    lines.push(format!(
        "  {n}. {:<14} {}",
        "Anthropic",
        anthropic_status(config)
    ));
    ids.push("/login anthropic".to_string());
    lines.push(String::new());
    lines.push("── OpenAI-compatible providers ──".to_string());
    for p in crate::api::PROVIDERS {
        n += 1;
        lines.push(format!(
            "  {n}. {:<14} {}",
            p.name,
            provider_status(p, &config.keystore)
        ));
        ids.push(format!("/login {}", p.prefix));
    }
    lines.push(String::new());
    lines.push("── Local ──".to_string());
    n += 1;
    let ollama = if ollama_models.is_empty() {
        format!("not reachable at {}", config.ollama_host)
    } else {
        format!(
            "reachable at {} · {} model{}",
            config.ollama_host,
            ollama_models.len(),
            if ollama_models.len() == 1 { "" } else { "s" }
        )
    };
    lines.push(format!("  {n}. {:<14} {ollama}", "Ollama"));
    ids.push(String::new());
    lines.push(String::new());
    lines.push(
        "  Enter puts the row's command in the input · /login <provider> open opens the key page"
            .to_string(),
    );
    (lines, ids)
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

#[cfg(test)]
mod board_tests {
    use super::*;
    use crate::auth::keystore::{KeySource, Keystore};

    #[test]
    fn anthropic_row_reflects_the_credential_source() {
        let mut c = crate::config::Config::default();
        assert!(anthropic_status(&c).contains("not signed in"));
        c.api_key = "sk-ant-x".into();
        c.auth_source = Some("ANTHROPIC_API_KEY".into());
        assert!(anthropic_status(&c).contains("API key via ANTHROPIC_API_KEY"));
        let mut creds = crate::auth::profile::ProfileCredentials::new(
            "at",
            None,
            Some(crate::auth::oauth::now_unix() + 41 * 60 + 30),
        );
        creds.account_email = Some("a@example.com".into());
        creds.organization_name = Some("Kubereva".into());
        c.auth =
            crate::auth::AuthHandle::profile(std::env::temp_dir(), "default".into(), None, creds);
        let s = anthropic_status(&c);
        assert!(
            s.contains("a@example.com") && s.contains("Kubereva") && s.contains("41 min"),
            "{s}"
        );
    }

    #[test]
    fn provider_rows_show_source_or_the_key_page() {
        let mut ks = Keystore::default();
        ks.set("GROQ_API_KEY", "g", KeySource::ShellEnv);
        let groq = crate::api::provider_by_prefix("groq").unwrap();
        assert_eq!(provider_status(groq, &ks), "key via shell env");
        let deepseek = crate::api::provider_by_prefix("deepseek").unwrap();
        assert!(provider_status(deepseek, &ks).contains("platform.deepseek.com/api_keys"));
        let lm = crate::api::provider_by_prefix("lmstudio").unwrap();
        assert!(provider_status(lm, &ks).contains("LM_STUDIO_HOST"));
    }

    #[test]
    fn rows_are_numbered_in_registry_order_with_commands_as_ids() {
        let c = crate::config::Config::default();
        let (lines, ids) = board_rows(&c, &["llama3".into()]);
        assert_eq!(ids[0], "/login anthropic");
        assert_eq!(ids[1], "/login groq");
        assert_eq!(ids.len(), 1 + crate::api::PROVIDERS.len() + 1);
        assert_eq!(ids.last().unwrap(), "", "Ollama row is informational");
        assert!(
            lines.iter().any(|l| l.starts_with("  1. Anthropic")),
            "{lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains("Ollama") && l.contains("1 model")),
            "{lines:?}"
        );
    }
}
