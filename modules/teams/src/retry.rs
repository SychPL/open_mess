//! The chat service's retry policy, as the official client runs it (docs/protocols/teams.md 8,
//! spec 9.2): up to 4 retries of 429 and 5xx, delay `max(Retry-After, min(300 s, 1 s * 2^attempt))`,
//! and no wait longer than 300 s.

use crate::time;

const MAX_RETRIES: u8 = 4;
const MAX_WAIT_MS: i64 = 300_000;

#[derive(Debug, PartialEq, Eq)]
pub enum Retry {
    /// Hand the response to the caller as it is.
    Done,
    /// Send the request again after this many milliseconds.
    After(u32),
    GiveUp(GiveUpReason),
}

#[derive(Debug, PartialEq, Eq)]
pub enum GiveUpReason {
    /// The server asked for a wait longer than 300 s; the value is that wait in seconds.
    Throttled(u32),
    /// The 4 retries are used up.
    Exhausted,
}

/// `attempt` is the number of retries already made: 0 after the first response.
pub fn decide(status: u16, retry_after: Option<&str>, attempt: u8, now_ms: i64) -> Retry {
    // 401 is a token refresh, not a timed retry, and 507 is never retried (docs 8).
    let retryable = status == 429 || ((500..=599).contains(&status) && status != 507);
    if !retryable {
        return Retry::Done;
    }
    let wait_ms = retry_after.and_then(|value| retry_after_ms(value, now_ms));
    if let Some(ms) = wait_ms.filter(|&ms| ms > MAX_WAIT_MS) {
        let secs = ms.saturating_add(999) / 1000;
        return Retry::GiveUp(GiveUpReason::Throttled(u32::try_from(secs).unwrap_or(u32::MAX)));
    }
    if attempt >= MAX_RETRIES {
        return Retry::GiveUp(GiveUpReason::Exhausted);
    }
    let backoff_ms = (1000_i64 << attempt).min(MAX_WAIT_MS);
    Retry::After(u32::try_from(wait_ms.unwrap_or(0).max(backoff_ms)).unwrap_or(u32::MAX))
}

/// `Retry-After` as a wait from `now_ms`: delay seconds or an HTTP date. Anything else is `None`,
/// and the caller falls back to the backoff.
fn retry_after_ms(value: &str, now_ms: i64) -> Option<i64> {
    let value = value.trim();
    if !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()) {
        // Too many digits for an i64 is still a very long wait.
        return Some(value.parse::<i64>().map_or(i64::MAX, |secs| secs.saturating_mul(1000)));
    }
    Some(time::http_date_ms(value)?.saturating_sub(now_ms).max(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fri, 15 Jan 2027 08:00:00 GMT.
    const NOW: i64 = 1_800_000_000_000;

    #[test]
    fn ok_is_done() {
        for status in [200, 201, 204, 302] {
            assert_eq!(decide(status, None, 0, NOW), Retry::Done, "{status}");
        }
    }

    #[test]
    fn retry_after_seconds_is_honoured() {
        assert_eq!(decide(429, Some("7"), 0, NOW), Retry::After(7_000));
        assert_eq!(decide(503, Some(" 2 "), 0, NOW), Retry::After(2_000));
        // The backoff is a floor: delay = max(Retry-After, backoff) (docs 8).
        assert_eq!(decide(429, Some("1"), 3, NOW), Retry::After(8_000));
    }

    #[test]
    fn retry_after_date_counts_from_now() {
        assert_eq!(decide(429, Some("Fri, 15 Jan 2027 08:00:30 GMT"), 0, NOW), Retry::After(30_000));
        assert_eq!(decide(429, Some("Fri, 15 Jan 2027 07:59:50 GMT"), 0, NOW), Retry::After(1_000));
    }

    #[test]
    fn retry_after_over_300_gives_up() {
        assert_eq!(decide(429, Some("301"), 0, NOW), Retry::GiveUp(GiveUpReason::Throttled(301)));
        assert_eq!(decide(429, Some("300"), 0, NOW), Retry::After(300_000));
        assert_eq!(decide(503, Some("Fri, 15 Jan 2027 08:05:01 GMT"), 0, NOW), Retry::GiveUp(GiveUpReason::Throttled(301)));
        assert_eq!(decide(429, Some("99999999999999999999999"), 0, NOW), Retry::GiveUp(GiveUpReason::Throttled(u32::MAX)));
    }

    #[test]
    fn garbage_retry_after_uses_backoff() {
        for garbage in ["soon", "", "-5", "1.5", "Fri, 15 Jan 2027 08:00:30"] {
            assert_eq!(decide(429, Some(garbage), 0, NOW), Retry::After(1_000), "{garbage}");
        }
        assert_eq!(decide(500, None, 2, NOW), Retry::After(4_000));
    }

    #[test]
    fn fifth_attempt_gives_up() {
        assert_eq!(decide(503, None, 3, NOW), Retry::After(8_000));
        assert_eq!(decide(503, None, 4, NOW), Retry::GiveUp(GiveUpReason::Exhausted));
    }

    #[test]
    fn client_errors_are_not_retried() {
        // 401 is a token refresh, not a timed retry; 507 is never retried (docs 8).
        for status in [400, 401, 403, 404, 507] {
            assert_eq!(decide(status, None, 0, NOW), Retry::Done, "{status}");
        }
    }
}
