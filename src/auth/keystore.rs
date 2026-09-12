//! Provider API keys: where they come from and how `/login <provider>` stores
//! them. Storage is the user-level `~/.config/oxideclaw/.env`, never the
//! project tree. Nothing here mutates the process environment after startup.

use anyhow::{Context, Result, anyhow};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Keys that may be loaded from a `.env` file. Copied from `main.rs`; the
/// comments there about `ANTHROPIC_BASE_URL` apply unchanged.
pub const SAFE_ENV_KEYS: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_PROFILE",
    "OXIDECLAW_API_KEY_FILE_DESCRIPTOR",
    "RUSTYCLAW_API_KEY_FILE_DESCRIPTOR",
    "ANTHROPIC_MODEL",
    "OXIDECLAW_VERBOSE",
    "RUSTYCLAW_VERBOSE",
    "OLLAMA_HOST",
    "OPENAI_API_KEY",
    "GROQ_API_KEY",
    "DEEPSEEK_API_KEY",
    "MISTRAL_API_KEY",
    "OPENROUTER_API_KEY",
    "TOGETHER_API_KEY",
    "XAI_API_KEY",
    "VENICE_API_KEY",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeySource {
    ShellEnv,
    ProjectDotenv,
    HomeDotenv,
    UserDotenv,
}

impl KeySource {
    pub fn describe(self) -> &'static str {
        match self {
            KeySource::ShellEnv => "shell env",
            KeySource::ProjectDotenv => "project .env",
            KeySource::HomeDotenv => "~/.env",
            KeySource::UserDotenv => "~/.config/oxideclaw/.env",
        }
    }
}

#[derive(Clone, Default)]
pub struct Keystore {
    entries: HashMap<String, (String, KeySource)>,
}

impl std::fmt::Debug for Keystore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut keys: Vec<_> = self.entries.iter().map(|(k, (_, s))| (k, s)).collect();
        keys.sort_by_key(|(k, _)| k.as_str());
        f.debug_map().entries(keys).finish()
    }
}

impl Keystore {
    pub fn get(&self, key: &str) -> Option<&str> {
        self.entries.get(key).map(|(v, _)| v.as_str())
    }
    pub fn source(&self, key: &str) -> Option<KeySource> {
        self.entries.get(key).map(|(_, s)| *s)
    }
    pub fn set(&mut self, key: &str, value: &str, source: KeySource) {
        self.entries
            .insert(key.to_string(), (value.to_string(), source));
    }
    pub fn remove(&mut self, key: &str) {
        self.entries.remove(key);
    }
    /// Closure shape used by `configured_providers` and the client constructor.
    pub fn lookup(&self) -> impl Fn(&str) -> Option<String> + '_ {
        move |k| self.get(k).map(str::to_string)
    }
}

/// Allowlisted `KEY=value` pairs from a `.env` body, in file order.
pub fn parse_dotenv(content: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        if let Some((key, val)) = line.split_once('=') {
            let key = key.trim();
            let val = val.trim().trim_matches('"').trim_matches('\'');
            if !key.is_empty() && SAFE_ENV_KEYS.contains(&key) {
                out.push((key.to_string(), val.to_string()));
            }
        }
    }
    out
}

/// Load one file into the process env (unset keys only — earlier sources win)
/// and record attribution in `ks`.
fn load_one(path: &Path, source: KeySource, ks: &mut Keystore) {
    let Ok(content) = std::fs::read_to_string(path) else {
        return;
    };
    for (key, val) in parse_dotenv(&content) {
        if std::env::var(&key).is_err() {
            // Startup only, before any thread is spawned — same contract as
            // the loader this replaced in main.rs.
            unsafe {
                std::env::set_var(&key, &val);
            }
            ks.set(&key, &val, source);
        }
    }
}

/// `~/.config/oxideclaw/.env` (via the app dir helper, which also migrates
/// the legacy directory name).
pub fn user_env_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| crate::config::app_dir(&h.join(".config")).join(".env"))
}

/// Search the usual locations in priority order and build the keystore.
/// Must run once at startup before the async runtime spawns threads.
pub fn load_dotenv_auto() -> Keystore {
    let mut ks = Keystore::default();
    for k in SAFE_ENV_KEYS {
        if let Ok(v) = std::env::var(k)
            && !v.trim().is_empty()
        {
            ks.set(k, &v, KeySource::ShellEnv);
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        let p = cwd.join(".env");
        if p.exists() {
            load_one(&p, KeySource::ProjectDotenv, &mut ks);
            eprintln!(
                "Note: .env detected in project root. Only oxideclaw-specific keys \
                 (ANTHROPIC_API_KEY, OLLAMA_HOST, etc.) are loaded. \
                 Project vars are NOT injected into tool execution."
            );
        }
    }
    if let Some(home) = dirs::home_dir() {
        load_one(&home.join(".env"), KeySource::HomeDotenv, &mut ks);
        if let Some(user) = user_env_path() {
            load_one(&user, KeySource::UserDotenv, &mut ks);
        }
    }
    ks
}

fn is_line_for(line: &str, key: &str) -> bool {
    let l = line.trim();
    let l = l.strip_prefix("export ").unwrap_or(l);
    l.split_once('=').is_some_and(|(k, _)| k.trim() == key)
}

/// Replace the first `KEY=` line (dropping any duplicates) or append one.
pub fn upsert_line(content: &str, key: &str, value: &str) -> String {
    let mut out = String::new();
    let mut written = false;
    for line in content.lines() {
        if is_line_for(line, key) {
            if !written {
                out.push_str(&format!("{key}={value}\n"));
                written = true;
            }
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    if !written {
        out.push_str(&format!("{key}={value}\n"));
    }
    out
}

pub fn remove_line(content: &str, key: &str) -> String {
    let mut out = String::new();
    for line in content.lines() {
        if is_line_for(line, key) {
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

fn write_env(path: &Path, content: &str) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("no parent for {}", path.display()))?;
    std::fs::create_dir_all(parent)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
    }
    let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
    std::io::Write::write_all(&mut tmp, content.as_bytes())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o600))?;
    }
    tmp.persist(path)
        .map_err(|e| anyhow!("persist {}: {}", path.display(), e.error))?;
    Ok(())
}

pub fn save_key_at(path: &Path, key: &str, value: &str) -> Result<()> {
    if !SAFE_ENV_KEYS.contains(&key) {
        return Err(anyhow!("{key} is not a storable key"));
    }
    if value.contains('\n') || value.trim().is_empty() {
        return Err(anyhow!("key value must be a single non-empty line"));
    }
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    write_env(path, &upsert_line(&existing, key, value.trim()))
        .with_context(|| format!("write {}", path.display()))
}

pub fn remove_key_at(path: &Path, key: &str) -> Result<bool> {
    let Ok(existing) = std::fs::read_to_string(path) else {
        return Ok(false);
    };
    let had = existing.lines().any(|l| is_line_for(l, key));
    if had {
        write_env(path, &remove_line(&existing, key))?;
    }
    Ok(had)
}

pub fn save_key(key: &str, value: &str) -> Result<PathBuf> {
    let path = user_env_path().ok_or_else(|| anyhow!("cannot determine the home directory"))?;
    save_key_at(&path, key, value)?;
    Ok(path)
}

pub fn remove_key(key: &str) -> Result<bool> {
    let path = user_env_path().ok_or_else(|| anyhow!("cannot determine the home directory"))?;
    remove_key_at(&path, key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_keeps_only_allowlisted_keys_and_strips_decoration() {
        let got = parse_dotenv(
            "# comment\nexport GROQ_API_KEY=\"gsk_1\"\nPATH=/evil\n\nMISTRAL_API_KEY='m1'\nNOT_A_KEY=x\n",
        );
        assert_eq!(
            got,
            vec![
                ("GROQ_API_KEY".to_string(), "gsk_1".to_string()),
                ("MISTRAL_API_KEY".to_string(), "m1".to_string()),
            ]
        );
    }

    #[test]
    fn upsert_replaces_in_place_and_preserves_everything_else() {
        let before = "# keys\nexport GROQ_API_KEY=old\nOLLAMA_HOST=http://x\n";
        let after = upsert_line(before, "GROQ_API_KEY", "new");
        assert_eq!(after, "# keys\nGROQ_API_KEY=new\nOLLAMA_HOST=http://x\n");
        let added = upsert_line("A=1", "GROQ_API_KEY", "g");
        assert_eq!(
            added, "A=1\nGROQ_API_KEY=g\n",
            "missing trailing newline handled"
        );
        assert_eq!(upsert_line("", "GROQ_API_KEY", "g"), "GROQ_API_KEY=g\n");
    }

    #[test]
    fn remove_drops_only_that_key() {
        let before = "GROQ_API_KEY=g\n# note\nexport GROQ_API_KEY=dup\nMISTRAL_API_KEY=m\n";
        assert_eq!(
            remove_line(before, "GROQ_API_KEY"),
            "# note\nMISTRAL_API_KEY=m\n"
        );
        assert_eq!(remove_line("A=1\n", "GROQ_API_KEY"), "A=1\n");
    }

    #[test]
    fn save_and_remove_at_a_path_with_secret_permissions() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("cfg").join(".env");
        save_key_at(&path, "GROQ_API_KEY", "g1").unwrap();
        save_key_at(&path, "MISTRAL_API_KEY", "m1").unwrap();
        save_key_at(&path, "GROQ_API_KEY", "g2").unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "GROQ_API_KEY=g2\nMISTRAL_API_KEY=m1\n"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                std::fs::metadata(path.parent().unwrap())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
        }
        assert!(remove_key_at(&path, "GROQ_API_KEY").unwrap());
        assert!(!remove_key_at(&path, "GROQ_API_KEY").unwrap());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "MISTRAL_API_KEY=m1\n"
        );
    }

    #[test]
    fn keystore_tracks_sources_and_never_prints_values() {
        let mut ks = Keystore::default();
        ks.set("GROQ_API_KEY", "gsk_secret", KeySource::ShellEnv);
        ks.set("MISTRAL_API_KEY", "m", KeySource::UserDotenv);
        assert_eq!(ks.get("GROQ_API_KEY"), Some("gsk_secret"));
        assert_eq!(ks.source("MISTRAL_API_KEY"), Some(KeySource::UserDotenv));
        assert_eq!(ks.lookup()("GROQ_API_KEY").as_deref(), Some("gsk_secret"));
        assert_eq!(ks.lookup()("NOPE"), None);
        let dbg = format!("{ks:?}");
        assert!(
            dbg.contains("GROQ_API_KEY") && !dbg.contains("gsk_secret"),
            "{dbg}"
        );
        ks.remove("GROQ_API_KEY");
        assert_eq!(ks.get("GROQ_API_KEY"), None);
    }

    #[test]
    fn allowlist_still_excludes_redirecting_urls() {
        assert!(!SAFE_ENV_KEYS.contains(&"ANTHROPIC_BASE_URL"));
        assert!(!SAFE_ENV_KEYS.contains(&"OPENAI_BASE_URL"));
        assert!(!SAFE_ENV_KEYS.contains(&"LM_STUDIO_HOST"));
        for p in crate::api::PROVIDERS {
            if !p.key_env.is_empty() {
                assert!(
                    SAFE_ENV_KEYS.contains(&p.key_env),
                    "{} must be storable",
                    p.key_env
                );
            }
        }
    }
}
