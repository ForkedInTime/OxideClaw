//! Daily update notice for the interactive TUI.
//!
//! At most once per [`INTERVAL`] the TUI asks the GitHub releases that
//! `oxideclaw update` installs from for the newest version, on a detached
//! thread that never delays startup or the first frame. The answer is cached
//! in `update-check.json` under the cache dir, so launches in between show
//! the notice without touching the network. Only the TUI calls [`spawn`]:
//! `-p`, the SDK, ACP and `oxideclaw browse` never check.
//!
//! Off with `"updateCheck": false` in any settings layer or
//! `OXIDECLAW_NO_UPDATE_CHECK=1`, and always off in test builds.

use crate::config::Config;
use crate::tui::events::AppEvent;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Minimum time between two release lookups.
pub const INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
/// Budget for one lookup, DNS and proxy included.
pub const TIMEOUT: Duration = Duration::from_secs(3);
const CACHE_FILE: &str = "update-check.json";

#[derive(Debug, Default, Serialize, Deserialize)]
struct Cache {
    /// Unix seconds of the last lookup, whether or not it succeeded.
    checked_at: u64,
    /// The newest version the last successful lookup found.
    latest: Option<String>,
}

/// Whether the check may run: the `updateCheck` setting and the
/// `OXIDECLAW_NO_UPDATE_CHECK` env value (`1` or `true` opts out).
pub fn enabled(setting: bool, no_update_check_env: Option<&str>) -> bool {
    let env_opt_out =
        no_update_check_env.is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("true"));
    setting && !env_opt_out
}

/// `latest` is a newer stable release than `current`. Pre-releases never
/// notify, and neither does anything that is not valid semver. Build
/// metadata is dropped first: semver orders on it, so `0.4.1+build.7` would
/// otherwise rank above `0.4.1`.
pub fn is_newer(latest: &str, current: &str) -> bool {
    let latest = latest.split('+').next().unwrap_or(latest);
    let current = current.split('+').next().unwrap_or(current);
    if latest.contains('-') {
        return false;
    }
    self_update::version::bump_is_greater(current, latest).unwrap_or(false)
}

pub fn notice(latest: &str) -> String {
    format!("OxideClaw v{latest} is available - run `oxideclaw update`.")
}

/// The newest known release version. Returns the cached answer when the last
/// lookup is under [`INTERVAL`] old at `now`; otherwise calls `fetch` and
/// caches what it finds. The attempt is recorded before `fetch` runs, so a
/// lookup that fails or hangs still counts as the day's one.
fn latest_version(
    cache_dir: &Path,
    now: SystemTime,
    fetch: impl FnOnce() -> anyhow::Result<String>,
) -> Option<String> {
    let path = cache_dir.join(CACHE_FILE);
    let cache: Cache = std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    let now_secs = now.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    // A timestamp in the future (clock moved back) counts as stale, or the
    // check would stay off until the clock caught up.
    let fresh = now_secs
        .checked_sub(cache.checked_at)
        .is_some_and(|age| age < INTERVAL.as_secs());
    if fresh {
        return cache.latest;
    }
    let write = |c: &Cache| {
        if let Ok(json) = serde_json::to_string(c)
            && let Err(e) = crate::config::write_json_atomic(&path, &json)
        {
            tracing::debug!("update check: could not write {}: {e}", path.display());
        }
    };
    write(&Cache {
        checked_at: now_secs,
        latest: cache.latest.clone(),
    });
    match fetch() {
        Ok(v) => {
            write(&Cache {
                checked_at: now_secs,
                latest: Some(v.clone()),
            });
            Some(v)
        }
        Err(e) => {
            tracing::debug!("update check failed: {e}");
            cache.latest
        }
    }
}

/// Run `f` on a detached thread and wait at most `deadline` for it. A
/// thread left behind is abandoned at exit; `spawn_blocking` would make the
/// runtime wait for it on shutdown.
async fn run_detached<T: Send + 'static>(
    f: impl FnOnce() -> T + Send + 'static,
    deadline: Duration,
) -> Option<T> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name("update-check".into())
        .spawn(move || {
            let _ = tx.send(f());
        })
        .ok()?;
    tokio::time::timeout(deadline, rx).await.ok()?.ok()
}

/// Start the background check; a newer release arrives on `tx` as one
/// system line. Returns at once.
pub fn spawn(config: &Config, tx: tokio::sync::mpsc::UnboundedSender<AppEvent>) {
    if cfg!(test)
        || !enabled(
            config.update_check,
            crate::config::app_env("NO_UPDATE_CHECK").as_deref(),
        )
    {
        return;
    }
    tokio::spawn(async move {
        let check = || {
            latest_version(&Config::cache_dir(), SystemTime::now(), || {
                crate::latest_release_version(TIMEOUT)
            })
        };
        // The request has its own TIMEOUT; this bounds the cache I/O too.
        let latest = run_detached(check, TIMEOUT + Duration::from_secs(1))
            .await
            .flatten();
        if let Some(v) = latest.filter(|v| is_newer(v, env!("CARGO_PKG_VERSION"))) {
            let _ = tx.send(AppEvent::SystemMessage(notice(&v)));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn at(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs)
    }

    const T0: u64 = 1_800_000_000;

    #[test]
    fn only_a_newer_stable_release_notifies() {
        assert!(is_newer("0.5.0", "0.4.1"));
        assert!(is_newer("1.0.0", "0.9.9"));
        assert!(is_newer("0.4.2", "0.4.1"));
        // A running pre-release is behind its final release.
        assert!(is_newer("0.5.0", "0.5.0-rc.1"));

        assert!(!is_newer("0.4.1", "0.4.1"), "equal");
        assert!(!is_newer("0.4.1+build.7", "0.4.1"), "equal with build");
        assert!(
            !is_newer("0.4.1", "0.4.1+build.7"),
            "equal, current has build"
        );
        assert!(is_newer("0.4.2+build.7", "0.4.1"), "newer with build");
        assert!(!is_newer("0.4.0", "0.4.1"), "older");
        assert!(!is_newer("0.5.0-rc.1", "0.4.1"), "pre-release");
        assert!(
            !is_newer("0.5.0-beta+build.7", "0.4.1"),
            "pre-release with build"
        );
        assert!(!is_newer("not-a-version", "0.4.1"));
        assert!(!is_newer("", "0.4.1"));
    }

    #[test]
    fn notice_names_the_version_and_the_command() {
        assert_eq!(
            notice("0.5.0"),
            "OxideClaw v0.5.0 is available - run `oxideclaw update`."
        );
    }

    #[test]
    fn opt_out_switches() {
        assert!(enabled(true, None));
        assert!(enabled(true, Some("0")));
        assert!(enabled(true, Some("")));
        assert!(!enabled(false, None), "updateCheck: false");
        assert!(!enabled(true, Some("1")));
        assert!(!enabled(true, Some("TRUE")));
        assert!(!enabled(false, Some("1")));
    }

    #[test]
    fn looks_up_at_most_once_a_day() {
        let dir = tempfile::tempdir().unwrap();
        let calls = Cell::new(0);
        let fetch = |v: &'static str| -> anyhow::Result<String> {
            calls.set(calls.get() + 1);
            Ok(v.to_string())
        };

        // First launch: no cache, so it looks the version up.
        let got = latest_version(dir.path(), at(T0), || fetch("0.5.0"));
        assert_eq!(got.as_deref(), Some("0.5.0"));
        assert_eq!(calls.get(), 1);

        // Within 24 h: the cached answer, no lookup.
        let got = latest_version(dir.path(), at(T0 + INTERVAL.as_secs() - 1), || {
            fetch("9.9.9")
        });
        assert_eq!(got.as_deref(), Some("0.5.0"));
        assert_eq!(calls.get(), 1);

        // 24 h later: looks again and caches the new answer.
        let later = T0 + INTERVAL.as_secs();
        let got = latest_version(dir.path(), at(later), || fetch("0.6.0"));
        assert_eq!(got.as_deref(), Some("0.6.0"));
        assert_eq!(calls.get(), 2);
        let got = latest_version(dir.path(), at(later + 60), || fetch("9.9.9"));
        assert_eq!(got.as_deref(), Some("0.6.0"));
        assert_eq!(calls.get(), 2);
    }

    #[test]
    fn a_failed_lookup_still_waits_a_day_and_keeps_the_last_answer() {
        let dir = tempfile::tempdir().unwrap();
        latest_version(dir.path(), at(T0), || Ok("0.5.0".into()));

        let day = INTERVAL.as_secs();
        let got = latest_version(dir.path(), at(T0 + day), || anyhow::bail!("offline"));
        assert_eq!(got.as_deref(), Some("0.5.0"));

        // The failure was recorded: no retry on the next launch.
        let retried = Cell::new(false);
        latest_version(dir.path(), at(T0 + day + 60), || {
            retried.set(true);
            Ok("0.6.0".into())
        });
        assert!(!retried.get());
    }

    #[test]
    fn a_cache_from_the_future_or_unreadable_is_stale() {
        let dir = tempfile::tempdir().unwrap();
        latest_version(dir.path(), at(T0), || Ok("0.5.0".into()));
        // The clock moved back a week: look again rather than wait it out.
        let got = latest_version(dir.path(), at(T0 - 7 * 86_400), || Ok("0.6.0".into()));
        assert_eq!(got.as_deref(), Some("0.6.0"));

        std::fs::write(dir.path().join(CACHE_FILE), "{not json").unwrap();
        let got = latest_version(dir.path(), at(T0), || Ok("0.7.0".into()));
        assert_eq!(got.as_deref(), Some("0.7.0"));
    }

    #[test]
    fn the_cache_dir_is_created_on_first_use() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("cache").join("oxideclaw");
        latest_version(&nested, at(T0), || Ok("0.5.0".into()));
        let cache: Cache =
            serde_json::from_str(&std::fs::read_to_string(nested.join(CACHE_FILE)).unwrap())
                .unwrap();
        assert_eq!(cache.checked_at, T0);
        assert_eq!(cache.latest.as_deref(), Some("0.5.0"));
    }

    #[tokio::test]
    async fn a_hung_lookup_is_abandoned_at_the_deadline() {
        let start = std::time::Instant::now();
        let got = run_detached(
            || {
                std::thread::sleep(Duration::from_secs(30));
                Some("0.5.0".to_string())
            },
            Duration::from_millis(100),
        )
        .await;
        assert_eq!(got, None);
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn spawn_never_checks_in_test_builds() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let config = Config {
            update_check: true,
            ..Config::default()
        };
        spawn(&config, tx);
        // `spawn` returned without starting a task, so the sender is gone.
        assert!(rx.recv().await.is_none());
    }
}
