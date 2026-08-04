//! Retry policy for transient API failures.
//!
//! Before this existed there was no retry anywhere on the request path: a
//! single 429 or a transient TCP reset failed the whole turn and threw away
//! the conversation's momentum. For a rate-limited user that meant an error
//! message and nothing else — the one failure mode that is *guaranteed* to
//! happen under enterprise load.
//!
//! Two deliberate scope limits:
//!
//!   * **529 is not retried here.** It is owned by the fallback-model fast
//!     path in `query_engine`, which switches to a cheaper model immediately.
//!     Sleeping 30s before doing that would be strictly worse.
//!   * **Retries happen only before any bytes are streamed.** Every caller
//!     retries at the send-and-check-status step, so a retry can never
//!     duplicate text the user has already seen.
//!
//! The decision logic is pure (`decide`) so the policy is testable without a
//! network, a clock, or a live rate limit.

use std::sync::Arc;
use std::time::Duration;

/// Total attempts including the first. 5 => up to 4 retries.
pub const MAX_ATTEMPTS: u32 = 5;
/// First backoff step; doubles each attempt.
pub const BASE_BACKOFF: Duration = Duration::from_millis(500);
/// Ceiling for a single computed (non-server-directed) backoff.
pub const MAX_BACKOFF: Duration = Duration::from_secs(32);
/// A `retry-after` longer than this is not worth waiting out — fail fast with
/// a clear message instead of freezing the UI for minutes.
pub const MAX_RETRY_AFTER: Duration = Duration::from_secs(120);
/// Cumulative ceiling across all retries for one request.
pub const MAX_TOTAL_RETRY: Duration = Duration::from_secs(150);

/// Reported to the UI before each backoff sleep so a long wait is never silent.
#[derive(Debug, Clone)]
pub struct RetryNotice {
    pub attempt: u32,
    pub max_attempts: u32,
    pub delay: Duration,
    pub reason: String,
}

impl RetryNotice {
    /// One-line, user-facing. Shown in the TUI and on stderr in headless mode.
    pub fn message(&self) -> String {
        format!(
            "{} — retrying in {} (attempt {}/{})",
            self.reason,
            format_delay(self.delay),
            self.attempt + 1,
            self.max_attempts
        )
    }
}

fn format_delay(d: Duration) -> String {
    let secs = d.as_secs_f64();
    if secs < 1.0 {
        format!("{}ms", d.as_millis())
    } else {
        format!("{secs:.1}s")
    }
}

pub type RetryNotifier = Arc<dyn Fn(&RetryNotice) + Send + Sync>;

/// Statuses worth retrying.
///
/// 529 is excluded on purpose — see the module docs. 4xx other than 408/429
/// are caller errors: retrying a 400 just burns the same tokens again, and
/// retrying a 401 hammers an auth failure.
pub fn is_retryable_status(status: u16) -> bool {
    matches!(status, 408 | 429 | 500 | 502 | 503 | 504)
}

/// True for transport failures that a retry can plausibly fix.
///
/// Deliberately narrow: a body/decode error means we already got a response,
/// and `is_request()` covers URL/builder mistakes that will fail identically
/// every time.
pub fn is_retryable_transport(e: &reqwest::Error) -> bool {
    e.is_connect() || e.is_timeout()
}

/// Parse a server-directed retry delay.
///
/// Supports `retry-after` as numeric seconds (integer or fractional — Groq
/// and friends send `2.5`) and the `retry-after-ms` extension used by some
/// OpenAI-compatible providers. The RFC also allows an HTTP-date, but no
/// provider on our supported list sends one and the crate has no date parser,
/// so a date is ignored rather than mis-parsed as zero.
pub fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    if let Some(ms) = headers
        .get("retry-after-ms")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<f64>().ok())
        && ms.is_finite()
        && ms >= 0.0
    {
        return Some(Duration::from_secs_f64(ms / 1000.0));
    }
    let raw = headers.get("retry-after")?.to_str().ok()?;
    let secs = raw.trim().parse::<f64>().ok()?;
    if !secs.is_finite() || secs < 0.0 {
        return None;
    }
    Some(Duration::from_secs_f64(secs))
}

/// What to do after a failed attempt.
#[derive(Debug, Clone, PartialEq)]
pub enum RetryDecision {
    /// Sleep this long, then try again.
    Retry(Duration),
    /// Stop. The string explains why, for the user-facing error.
    GiveUp(GiveUpReason),
}

#[derive(Debug, Clone, PartialEq)]
pub enum GiveUpReason {
    /// Not a transient failure.
    NotRetryable,
    /// Ran out of attempts.
    Exhausted,
    /// Server asked for a longer wait than we are willing to block for.
    RetryAfterTooLong(Duration),
    /// Retrying again would exceed the per-request time budget.
    BudgetExhausted,
}

impl GiveUpReason {
    pub fn describe(&self) -> String {
        match self {
            Self::NotRetryable => String::new(),
            Self::Exhausted => format!(" (gave up after {MAX_ATTEMPTS} attempts)"),
            Self::RetryAfterTooLong(d) => format!(
                " (server asked to retry after {}, longer than the {}s limit — try again later)",
                format_delay(*d),
                MAX_RETRY_AFTER.as_secs()
            ),
            Self::BudgetExhausted => format!(
                " (gave up after {}s of retries)",
                MAX_TOTAL_RETRY.as_secs()
            ),
        }
    }
}

/// Pure retry policy.
///
/// `attempt` is 0-based (0 = the first try just failed). `jitter` is in
/// `[0.0, 1.0)` and is supplied by the caller so tests are deterministic.
///
/// A server-directed `retry_after` is honoured verbatim — no jitter, no
/// exponential growth. The server knows when its window opens; second-guessing
/// it just causes another 429.
pub fn decide(
    attempt: u32,
    retryable: bool,
    retry_after: Option<Duration>,
    elapsed: Duration,
    jitter: f64,
) -> RetryDecision {
    if !retryable {
        return RetryDecision::GiveUp(GiveUpReason::NotRetryable);
    }
    if attempt + 1 >= MAX_ATTEMPTS {
        return RetryDecision::GiveUp(GiveUpReason::Exhausted);
    }

    let delay = match retry_after {
        Some(d) => {
            if d > MAX_RETRY_AFTER {
                return RetryDecision::GiveUp(GiveUpReason::RetryAfterTooLong(d));
            }
            d
        }
        None => {
            // Exponential with equal jitter: half fixed, half random. Full
            // jitter can retry almost immediately, which defeats the point
            // under a rate limit; no jitter synchronises every client.
            let exp = BASE_BACKOFF.saturating_mul(1u32 << attempt.min(16));
            let exp = exp.min(MAX_BACKOFF);
            let half = exp.as_secs_f64() / 2.0;
            Duration::from_secs_f64(half + half * jitter.clamp(0.0, 1.0))
        }
    };

    if elapsed + delay > MAX_TOTAL_RETRY {
        return RetryDecision::GiveUp(GiveUpReason::BudgetExhausted);
    }
    RetryDecision::Retry(delay)
}

/// Human-readable cause for the notice line.
pub fn describe_status(status: u16) -> String {
    match status {
        429 => "Rate limited by the API".to_string(),
        408 => "Request timed out".to_string(),
        500 | 502 | 503 | 504 => format!("API server error ({status})"),
        _ => format!("API error ({status})"),
    }
}

/// Whether a caller that drives its own streaming loop may re-issue the call.
///
/// Lives here rather than inline in the TUI so it is reachable from tests —
/// the streaming loop itself is binary-only. The rule that matters is
/// `already_streamed`: once any text has been handed to the UI, re-issuing the
/// request replays the entire response and the user sees a truncated answer
/// followed by a complete one.
pub fn may_retry_stream(
    is_retryable: bool,
    already_streamed: bool,
    attempt: u32,
    max_attempts: u32,
) -> bool {
    is_retryable && !already_streamed && attempt < max_attempts
}

/// Send with retries, returning the final response for the caller to inspect.
///
/// Crucially this returns the `Response` even when the status is an error, so
/// every existing caller-side branch still works — the OpenAI-compat
/// "model does not support tools" 400 sniff and the 529 fallback detection
/// both depend on seeing the real response.
///
/// `make` rebuilds the request from scratch each attempt; `RequestBuilder` is
/// consumed by `send()` and `try_clone` returns `None` for streamed bodies, so
/// a closure is the only form that always works.
pub async fn send_with_retry<F>(
    make: F,
    notifier: Option<&RetryNotifier>,
    context: &str,
) -> anyhow::Result<reqwest::Response>
where
    F: Fn() -> reqwest::RequestBuilder,
{
    let start = std::time::Instant::now();
    let mut attempt: u32 = 0;

    loop {
        let result = make().send().await;

        let (retryable, retry_after, reason) = match &result {
            Ok(resp) => {
                let status = resp.status().as_u16();
                if resp.status().is_success() {
                    return Ok(result.unwrap());
                }
                (
                    is_retryable_status(status),
                    parse_retry_after(resp.headers()),
                    describe_status(status),
                )
            }
            Err(e) => (
                is_retryable_transport(e),
                None,
                format!("Connection to the API failed ({e})"),
            ),
        };

        let decision = {
            let jitter: f64 = rand::random::<f64>();
            decide(attempt, retryable, retry_after, start.elapsed(), jitter)
        };

        match decision {
            RetryDecision::Retry(delay) => {
                let notice = RetryNotice {
                    attempt,
                    max_attempts: MAX_ATTEMPTS,
                    delay,
                    reason: reason.clone(),
                };
                tracing::warn!("{context}: {}", notice.message());
                if let Some(n) = notifier {
                    n(&notice);
                }
                tokio::time::sleep(delay).await;
                attempt += 1;
            }
            RetryDecision::GiveUp(why) => {
                return match result {
                    // Hand the response back untouched; the caller owns the
                    // error message so provider-specific handling still runs.
                    // The reason we stopped would otherwise be lost, so report
                    // it separately — "gave up after 5 attempts" is far more
                    // actionable than a bare 429 body.
                    Ok(resp) => {
                        if attempt > 0 && why != GiveUpReason::NotRetryable {
                            let msg = format!("{reason}{}", why.describe());
                            tracing::warn!("{context}: {msg}");
                            if let Some(n) = notifier {
                                n(&RetryNotice {
                                    attempt,
                                    max_attempts: MAX_ATTEMPTS,
                                    delay: Duration::ZERO,
                                    reason: msg,
                                });
                            }
                        }
                        Ok(resp)
                    }
                    Err(e) => Err(anyhow::anyhow!("{context}: {e}{}", why.describe())),
                };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::{HeaderMap, HeaderValue};

    #[test]
    fn retryable_statuses_are_transient_only() {
        for s in [408, 429, 500, 502, 503, 504] {
            assert!(is_retryable_status(s), "{s} should retry");
        }
        // 529 belongs to the fallback-model path — retrying it here would
        // delay the model switch that already handles it.
        for s in [200, 400, 401, 403, 404, 413, 422, 529] {
            assert!(!is_retryable_status(s), "{s} should not retry");
        }
    }

    #[test]
    fn parses_integer_and_fractional_retry_after() {
        let mut h = HeaderMap::new();
        h.insert("retry-after", HeaderValue::from_static("30"));
        assert_eq!(parse_retry_after(&h), Some(Duration::from_secs(30)));

        let mut h = HeaderMap::new();
        h.insert("retry-after", HeaderValue::from_static("2.5"));
        assert_eq!(parse_retry_after(&h), Some(Duration::from_millis(2500)));
    }

    #[test]
    fn retry_after_ms_takes_precedence() {
        let mut h = HeaderMap::new();
        h.insert("retry-after", HeaderValue::from_static("30"));
        h.insert("retry-after-ms", HeaderValue::from_static("1500"));
        assert_eq!(parse_retry_after(&h), Some(Duration::from_millis(1500)));
    }

    /// An HTTP-date must be ignored, not silently parsed as a zero delay —
    /// that would turn a polite backoff into a hot retry loop.
    #[test]
    fn http_date_retry_after_is_ignored_not_zero() {
        let mut h = HeaderMap::new();
        h.insert(
            "retry-after",
            HeaderValue::from_static("Wed, 21 Oct 2026 07:28:00 GMT"),
        );
        assert_eq!(parse_retry_after(&h), None);
    }

    #[test]
    fn junk_and_negative_retry_after_rejected() {
        for v in ["", "abc", "-5", "NaN", "inf"] {
            let mut h = HeaderMap::new();
            h.insert("retry-after", HeaderValue::from_str(v).unwrap());
            assert_eq!(parse_retry_after(&h), None, "value {v:?} should be rejected");
        }
    }

    #[test]
    fn non_retryable_gives_up_immediately() {
        assert_eq!(
            decide(0, false, None, Duration::ZERO, 0.0),
            RetryDecision::GiveUp(GiveUpReason::NotRetryable)
        );
    }

    #[test]
    fn server_retry_after_is_honoured_verbatim() {
        // No jitter, no exponential growth — the server named the time.
        let d = decide(0, true, Some(Duration::from_secs(7)), Duration::ZERO, 0.9);
        assert_eq!(d, RetryDecision::Retry(Duration::from_secs(7)));
        // ...and it does not drift with the attempt number.
        let d = decide(3, true, Some(Duration::from_secs(7)), Duration::ZERO, 0.1);
        assert_eq!(d, RetryDecision::Retry(Duration::from_secs(7)));
    }

    #[test]
    fn absurd_retry_after_fails_fast_rather_than_hanging() {
        let long = MAX_RETRY_AFTER + Duration::from_secs(1);
        assert_eq!(
            decide(0, true, Some(long), Duration::ZERO, 0.0),
            RetryDecision::GiveUp(GiveUpReason::RetryAfterTooLong(long))
        );
    }

    #[test]
    fn backoff_grows_exponentially_and_is_bounded() {
        let mut last = Duration::ZERO;
        for attempt in 0..MAX_ATTEMPTS - 1 {
            let RetryDecision::Retry(d) =
                decide(attempt, true, None, Duration::ZERO, 0.0)
            else {
                panic!("attempt {attempt} should retry");
            };
            assert!(d > last, "attempt {attempt}: {d:?} !> {last:?}");
            assert!(d <= MAX_BACKOFF, "attempt {attempt} exceeded cap");
            last = d;
        }
    }

    /// Equal jitter: never zero (that defeats the backoff) and never above
    /// the nominal exponential step.
    #[test]
    fn jitter_stays_within_half_to_full_of_the_step() {
        let exp = BASE_BACKOFF * 4; // attempt 2
        let RetryDecision::Retry(lo) = decide(2, true, None, Duration::ZERO, 0.0) else {
            panic!()
        };
        let RetryDecision::Retry(hi) = decide(2, true, None, Duration::ZERO, 1.0) else {
            panic!()
        };
        assert_eq!(lo, exp / 2);
        assert_eq!(hi, exp);
    }

    /// Out-of-range jitter must not produce a negative or runaway delay.
    #[test]
    fn jitter_is_clamped() {
        let RetryDecision::Retry(d) = decide(1, true, None, Duration::ZERO, -3.0) else {
            panic!()
        };
        assert_eq!(d, BASE_BACKOFF); // (2*BASE)/2
        let RetryDecision::Retry(d) = decide(1, true, None, Duration::ZERO, 9.0) else {
            panic!()
        };
        assert_eq!(d, BASE_BACKOFF * 2);
    }

    #[test]
    fn attempts_are_bounded() {
        assert_eq!(
            decide(MAX_ATTEMPTS - 1, true, None, Duration::ZERO, 0.0),
            RetryDecision::GiveUp(GiveUpReason::Exhausted)
        );
    }

    /// A short per-attempt delay must still not let total wall time run away.
    #[test]
    fn total_time_budget_is_enforced() {
        let nearly_spent = MAX_TOTAL_RETRY - Duration::from_millis(10);
        assert_eq!(
            decide(0, true, Some(Duration::from_secs(5)), nearly_spent, 0.0),
            RetryDecision::GiveUp(GiveUpReason::BudgetExhausted)
        );
    }

    // ─── Guard against re-issuing a call that already produced output ───

    #[test]
    fn stream_retry_allowed_only_before_any_output() {
        // The whole point: identical inputs, opposite answers on `streamed`.
        assert!(may_retry_stream(true, false, 1, 3));
        assert!(
            !may_retry_stream(true, true, 1, 3),
            "retrying after output has streamed duplicates the response"
        );
        // And the ordinary guards still hold.
        assert!(!may_retry_stream(false, false, 1, 3), "non-retryable");
        assert!(!may_retry_stream(true, false, 3, 3), "attempts exhausted");
    }

    // ─── End-to-end wiring: does a real 429 actually get retried? ───
    //
    // The pure tests above only prove the policy is right. These prove
    // `send_with_retry` is on the request path at all — a policy that is
    // never called is worth nothing.

    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    const OK: &str = "HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok";
    const RATE_LIMITED: &str = "HTTP/1.1 429 Too Many Requests\r\nretry-after: 0\r\n\
                                content-length: 0\r\nconnection: close\r\n\r\n";
    const BAD_REQUEST: &str =
        "HTTP/1.1 400 Bad Request\r\ncontent-length: 0\r\nconnection: close\r\n\r\n";

    /// Serves `script[i]` for the i-th connection, repeating the last entry
    /// once exhausted. `retry-after: 0` keeps the tests instant while still
    /// exercising the real sleep/retry path.
    async fn scripted_server(script: Vec<&'static str>) -> (String, Arc<AtomicUsize>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = hits.clone();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let i = counter.fetch_add(1, Ordering::SeqCst);
                let body = *script.get(i).unwrap_or_else(|| script.last().unwrap());
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    let _ = sock.read(&mut buf).await;
                    let _ = sock.write_all(body.as_bytes()).await;
                    let _ = sock.shutdown().await;
                });
            }
        });
        (format!("http://{addr}/"), hits)
    }

    fn post(url: &str) -> reqwest::RequestBuilder {
        reqwest::Client::new().post(url).body("{}")
    }

    #[tokio::test]
    async fn a_429_is_retried_and_then_succeeds() {
        let (url, hits) = scripted_server(vec![RATE_LIMITED, OK]).await;
        let resp = send_with_retry(|| post(&url), None, "test").await.unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(hits.load(Ordering::SeqCst), 2, "should have retried once");
    }

    #[tokio::test]
    async fn a_400_is_not_retried() {
        let (url, hits) = scripted_server(vec![BAD_REQUEST]).await;
        let resp = send_with_retry(|| post(&url), None, "test").await.unwrap();
        // The response is handed back untouched so callers keep their own
        // error handling — notably the OpenAI-compat "no tools" 400 sniff.
        assert_eq!(resp.status(), 400);
        assert_eq!(hits.load(Ordering::SeqCst), 1, "400 must not be retried");
    }

    #[tokio::test]
    async fn a_permanent_429_stops_at_the_attempt_cap() {
        let (url, hits) = scripted_server(vec![RATE_LIMITED]).await;
        let resp = send_with_retry(|| post(&url), None, "test").await.unwrap();
        assert_eq!(resp.status(), 429);
        assert_eq!(
            hits.load(Ordering::SeqCst),
            MAX_ATTEMPTS as usize,
            "must not retry forever"
        );
    }

    /// A silent 30s backoff reads as a hang, so every retry must notify.
    #[tokio::test]
    async fn every_retry_is_reported_to_the_user() {
        let (url, _) = scripted_server(vec![RATE_LIMITED, RATE_LIMITED, OK]).await;
        let seen = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let sink = seen.clone();
        let notifier: RetryNotifier =
            Arc::new(move |n: &RetryNotice| sink.lock().unwrap().push(n.message()));

        let resp = send_with_retry(|| post(&url), Some(&notifier), "test")
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);

        let msgs = seen.lock().unwrap();
        assert_eq!(msgs.len(), 2, "one notice per retry: {msgs:?}");
        assert!(
            msgs[0].contains("Rate limited"),
            "notice should name the cause: {}",
            msgs[0]
        );
    }

    /// Proves `retry-after` is honoured *on the wire*, not merely parseable.
    ///
    /// The other server tests use `retry-after: 0`, so they would still pass
    /// if the header were ignored entirely — exponential backoff would just
    /// take over. Here the server asks for 1s while the attempt-0 exponential
    /// step is 250–500ms, so the two are distinguishable by elapsed time.
    #[tokio::test]
    async fn retry_after_header_is_obeyed_end_to_end() {
        const SLOW: &str = "HTTP/1.1 429 Too Many Requests\r\nretry-after: 1\r\n\
                            content-length: 0\r\nconnection: close\r\n\r\n";
        let (url, hits) = scripted_server(vec![SLOW, OK]).await;
        let start = std::time::Instant::now();
        let resp = send_with_retry(|| post(&url), None, "test").await.unwrap();
        let elapsed = start.elapsed();

        assert_eq!(resp.status(), 200);
        assert_eq!(hits.load(Ordering::SeqCst), 2);
        assert!(
            elapsed >= Duration::from_millis(900),
            "waited {elapsed:?}; the server asked for 1s, so `retry-after` was ignored \
             and exponential backoff (250-500ms) was used instead"
        );
    }

    /// Giving up must explain itself; a bare 429 body is not actionable.
    #[tokio::test]
    async fn giving_up_reports_why() {
        let (url, _) = scripted_server(vec![RATE_LIMITED]).await;
        let seen = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let sink = seen.clone();
        let notifier: RetryNotifier =
            Arc::new(move |n: &RetryNotice| sink.lock().unwrap().push(n.reason.clone()));

        let _ = send_with_retry(|| post(&url), Some(&notifier), "test").await;
        let msgs = seen.lock().unwrap();
        assert!(
            msgs.last().unwrap().contains("gave up after"),
            "final notice should say we stopped: {msgs:?}"
        );
    }

    // ─── Client wiring ───
    //
    // Everything above tests `send_with_retry` directly, so it would all still
    // pass if the client called plain `send()` and never reached the retry
    // path at all. These two tests fail if that wiring is removed.

    const OK_SSE: &str = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                          content-length: 0\r\nconnection: close\r\n\r\n";

    fn test_client(base: &str) -> crate::api::ClaudeClient {
        let mut c =
            crate::api::ClaudeClient::new("sk-ant-test").expect("client should build");
        c.set_base_url_for_test(base.trim_end_matches('/'));
        c
    }

    fn empty_request() -> crate::api::types::MessagesRequest {
        crate::api::types::MessagesRequest {
            model: "claude-opus-5".into(),
            max_tokens: 16,
            messages: vec![],
            system: Default::default(),
            tools: vec![],
            stream: None,
            thinking: None,
            betas: vec![],
            session_id: None,
        }
    }

    /// The streaming path is the one every interactive turn goes through.
    #[tokio::test]
    async fn streaming_client_retries_a_429() {
        let (url, hits) = scripted_server(vec![RATE_LIMITED, OK_SSE]).await;
        let client = test_client(&url);
        let res = client.messages_stream(empty_request(), |_| {}).await;

        assert!(res.is_ok(), "should have recovered: {:?}", res.err());
        assert_eq!(
            hits.load(Ordering::SeqCst),
            2,
            "messages_stream did not retry — is it still calling send_with_retry?"
        );
    }

    /// The non-streaming path is used by SDK/headless callers.
    #[tokio::test]
    async fn non_streaming_client_retries_a_429() {
        let (url, hits) = scripted_server(vec![RATE_LIMITED, OK]).await;
        let client = test_client(&url);
        // The 200 body is not valid JSON, so this errors at the parse step —
        // irrelevant here. What matters is that the 429 was retried first.
        let _ = client.messages(empty_request()).await;
        assert_eq!(
            hits.load(Ordering::SeqCst),
            2,
            "messages() did not retry the 429"
        );
    }

    /// OpenAI-compatible providers rate limit far more aggressively than
    /// Anthropic — Groq and OpenRouter are the common case for a 429 — so this
    /// backend needs the same proof its retry is wired.
    ///
    /// Built from the `lmstudio` provider because it requires no API key and
    /// no environment variable, so the test never mutates process-global env
    /// (which raced the parallel harness when the auth tests did it).
    #[tokio::test]
    async fn openai_compat_client_retries_a_429() {
        let (url, hits) = scripted_server(vec![RATE_LIMITED, OK]).await;
        let mut client = match crate::api::OpenAiCompatClient::from_model("lmstudio:test") {
            Ok(c) => c,
            Err(e) => panic!("lmstudio client should build without credentials: {e}"),
        };
        client.base_url = url.trim_end_matches('/').to_string();

        // The 200 body is not a valid SSE stream, so this errors at parse
        // time. Only the request count matters here.
        let _ = client.messages_stream(empty_request(), |_| {}).await;
        assert_eq!(
            hits.load(Ordering::SeqCst),
            2,
            "OpenAI-compat backend did not retry the 429"
        );
    }

    /// Esc cancels a turn by aborting the task. A backoff of up to 32s sits
    /// inside that task, so the sleep must not keep a doomed request alive —
    /// otherwise a cancelled turn fires another API call minutes later.
    #[tokio::test]
    async fn aborting_the_task_cancels_a_pending_backoff() {
        const SLOW: &str = "HTTP/1.1 429 Too Many Requests\r\nretry-after: 1\r\n\
                            content-length: 0\r\nconnection: close\r\n\r\n";
        let (url, hits) = scripted_server(vec![SLOW]).await;
        let handle = tokio::spawn(async move {
            let _ = send_with_retry(|| post(&url), None, "test").await;
        });

        // Wait for the first attempt to land rather than assuming a fixed
        // delay is enough — a loaded CI runner is not that punctual.
        for _ in 0..100 {
            if hits.load(Ordering::SeqCst) >= 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(hits.load(Ordering::SeqCst), 1, "first attempt should land");
        handle.abort();

        // Wait past when the retry would have fired.
        tokio::time::sleep(Duration::from_millis(1200)).await;
        assert_eq!(
            hits.load(Ordering::SeqCst),
            1,
            "a cancelled turn must not fire its queued retry"
        );
    }

    #[test]
    fn notice_message_is_readable() {
        let n = RetryNotice {
            attempt: 0,
            max_attempts: 5,
            delay: Duration::from_millis(750),
            reason: describe_status(429),
        };
        assert_eq!(
            n.message(),
            "Rate limited by the API — retrying in 750ms (attempt 1/5)"
        );
    }
}
