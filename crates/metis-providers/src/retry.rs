//! Retry policy for transient provider failures.
//!
//! Without this, one overloaded response ended the whole turn. MiniMax
//! answers peak-hour load with HTTP 529 ("the server is temporarily busy. It
//! usually recovers within 1–5 minutes"), so an agent that had already run
//! its tools successfully reported the task as failed, and a scheduled 06:00
//! job simply produced nothing. Transient statuses are retried with
//! exponential backoff, honouring Retry-After when the server sends one.

use std::time::Duration;

/// Attempts after the first. With the delays below this waits up to about
/// ninety seconds in total — long for a chat, but a scheduled job that
/// waits is far better than one that silently fails.
pub const MAX_TRANSIENT_RETRIES: u32 = 5;

#[cfg(not(test))]
const BASE_DELAY_MS: u64 = 3_000;
#[cfg(test)]
const BASE_DELAY_MS: u64 = 1;

#[cfg(not(test))]
const MAX_DELAY_MS: u64 = 45_000;
#[cfg(test)]
const MAX_DELAY_MS: u64 = 5;

/// Longest server-requested wait honoured.
#[cfg(not(test))]
const RETRY_AFTER_CAP_MS: u64 = 60_000;
#[cfg(test)]
const RETRY_AFTER_CAP_MS: u64 = 5;

/// Statuses worth retrying: timeouts, rate limits, server errors, and the
/// non-standard 529 "overloaded" several providers use. A 400 or 401 is the
/// request's fault and retrying it only wastes time.
pub fn is_transient_status(code: u16) -> bool {
    matches!(code, 408 | 409 | 425 | 429 | 500 | 502 | 503 | 504 | 529)
}

/// A 429 that will never clear by waiting.
///
/// Providers use 429 for two different things: "slow down" (retry) and
/// "your account has no credit / quota" (permanent). Retrying the second
/// spent about ninety seconds per call before reporting an error that was
/// final from the first response.
pub fn is_permanent_quota_error(body: &str) -> bool {
    let b = body.to_lowercase();
    b.contains("insufficient_quota")
        || b.contains("no credits remaining")
        || b.contains("exceeded your current quota")
        || b.contains("credit balance is too low")
        || b.contains("billing")
}

/// Whether a failed response should be retried, given its status and body.
pub fn should_retry(code: u16, body: &str) -> bool {
    is_transient_status(code) && !is_permanent_quota_error(body)
}

/// Delay before retry number `attempt` (1-based).
///
/// A server-supplied Retry-After wins, capped so a hostile or broken value
/// cannot stall the agent for hours. Otherwise exponential with a little
/// jitter, so many clients hitting the same overload do not retry in step.
pub fn backoff_delay(attempt: u32, retry_after: Option<Duration>) -> Duration {
    if let Some(ra) = retry_after {
        return ra.min(Duration::from_millis(RETRY_AFTER_CAP_MS));
    }
    let exp = BASE_DELAY_MS.saturating_mul(1u64 << attempt.saturating_sub(1).min(10));
    let jitter = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| (d.subsec_nanos() as u64) % (BASE_DELAY_MS / 4 + 1))
        .unwrap_or(0);
    Duration::from_millis(exp.min(MAX_DELAY_MS) + jitter)
}

/// Parse a Retry-After header given in seconds. (The HTTP-date form is rare
/// from API providers and is simply ignored, falling back to backoff.)
pub fn parse_retry_after(value: Option<&str>) -> Option<Duration> {
    let secs: f64 = value?.trim().parse().ok()?;
    if secs.is_finite() && secs >= 0.0 {
        Some(Duration::from_millis((secs * 1000.0) as u64))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overload_and_rate_limits_are_transient() {
        for code in [429, 500, 502, 503, 504, 529] {
            assert!(is_transient_status(code), "{code} should be retried");
        }
    }

    #[test]
    fn client_errors_are_not_retried() {
        for code in [400, 401, 403, 404, 422] {
            assert!(!is_transient_status(code), "{code} must not be retried");
        }
    }

    #[test]
    fn an_empty_account_is_not_retried() {
        // Verbatim shape from a real OpenAI response.
        let body = r#"{"error":{"message":"You have no credits remaining. Add credits to continue using the API","type":"insufficient_quota"}}"#;
        assert!(!should_retry(429, body), "a quota 429 is permanent");
        // An ordinary rate limit still is retried.
        assert!(should_retry(429, r#"{"error":{"message":"Rate limit reached","type":"rate_limit_error"}}"#));
        assert!(should_retry(529, "overloaded"));
    }

    #[test]
    fn backoff_grows_and_is_capped() {
        let d1 = backoff_delay(1, None);
        let d5 = backoff_delay(5, None);
        assert!(d5 >= d1);
        assert!(d5 <= Duration::from_millis(MAX_DELAY_MS + BASE_DELAY_MS));
    }

    #[test]
    fn retry_after_seconds_are_parsed() {
        assert_eq!(parse_retry_after(Some("2")), Some(Duration::from_secs(2)));
        assert_eq!(parse_retry_after(Some(" 0.5 ")), Some(Duration::from_millis(500)));
        assert_eq!(parse_retry_after(Some("Wed, 21 Oct 2026 07:28:00 GMT")), None);
        assert_eq!(parse_retry_after(None), None);
    }
}
