//! Session token generation and CSRF token derivation.
//!
//! Session tokens are 32 random bytes, base64url-encoded without padding.
//! The CSRF token is a separate random value stored alongside the session, so
//! a leaked CSRF token alone cannot be used to authenticate.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use rand::RngCore;

use crate::error::{AppError, AppResult};

pub const SESSION_TOKEN_BYTES: usize = 32;
pub const CSRF_TOKEN_BYTES: usize = 32;

/// Generate a URL-safe, unpadded base64 token over `n` random bytes.
pub fn random_token(n: usize) -> String {
    let mut buf = vec![0u8; n];
    rand::rngs::OsRng.fill_bytes(&mut buf);
    URL_SAFE_NO_PAD.encode(buf)
}

pub fn new_session_token() -> String {
    random_token(SESSION_TOKEN_BYTES)
}

pub fn new_csrf_token() -> String {
    random_token(CSRF_TOKEN_BYTES)
}

/// Constant-time string comparison, so a caller cannot distinguish a
/// prefix match from a full match by timing.
pub fn constant_time_eq(a: &str, b: &str) -> bool {
    let a = a.as_bytes();
    let b = b.as_bytes();
    if a.len() != b.len() {
        return false;
    }
    use subtle::ConstantTimeEq;
    a.ct_eq(b).into()
}

/// Seconds since the Unix epoch.
pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Parse an optional absolute expiry into an absolute Unix timestamp.
/// Returns `None` for an absent or unparseable value, which the caller treats
/// as "no expiry".
///
/// Accepts a Unix timestamp, or `YYYY-MM-DD` interpreted as UTC midnight. The
/// `time` crate's well-known format descriptions need a full RFC 3339
/// timestamp, so the date is widened to one rather than hand-parsed.
pub fn parse_expiry(raw: Option<&str>) -> Option<i64> {
    let raw = raw?.trim();
    if raw.is_empty() {
        return None;
    }
    if let Ok(ts) = raw.parse::<i64>() {
        return Some(ts);
    }
    let date = parse_iso_date(raw)?;
    Some(
        time::PrimitiveDateTime::new(date, time::Time::MIDNIGHT)
            .assume_utc()
            .unix_timestamp(),
    )
}

/// Parse `YYYY-MM-DD` by hand. `time`'s own parsing helpers sit behind a
/// sealed trait, and this is the only date shape the panel accepts.
fn parse_iso_date(raw: &str) -> Option<time::Date> {
    let (y, rest) = raw.split_once('-')?;
    let (m, d) = rest.split_once('-')?;
    // Reject anything with more than two separators (e.g. a stray timestamp).
    if d.contains('-') || y.contains('-') {
        return None;
    }
    let year: i32 = y.parse().ok()?;
    let month: u8 = m.parse().ok()?;
    let day: u8 = d.parse().ok()?;
    time::Date::from_calendar_date(year, time::Month::try_from(month).ok()?, day).ok()
}

pub fn validate_expiry(raw: Option<&str>) -> AppResult<Option<i64>> {
    match raw {
        None => Ok(None),
        Some(s) if s.trim().is_empty() => Ok(None),
        Some(s) => parse_expiry(Some(s)).map(Some).ok_or_else(|| {
            AppError::Config(format!(
                "expiry must be a Unix timestamp or YYYY-MM-DD, got {s:?}"
            ))
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_unique_and_url_safe() {
        let a = new_session_token();
        let b = new_session_token();
        assert_ne!(a, b);
        assert!(!a.contains('+') && !a.contains('/') && !a.contains('='));
        assert_eq!(
            a.len(),
            URL_SAFE_NO_PAD.encode([0u8; SESSION_TOKEN_BYTES]).len()
        );
    }

    #[test]
    fn constant_time_eq_matches_semantics() {
        assert!(constant_time_eq("abc", "abc"));
        assert!(!constant_time_eq("abc", "abd"));
        assert!(!constant_time_eq("abc", "ab"));
        assert!(!constant_time_eq("", "a"));
    }

    #[test]
    fn expiry_accepts_absolute_timestamp() {
        assert_eq!(parse_expiry(Some("1700000000")), Some(1_700_000_000));
    }

    #[test]
    fn expiry_accepts_iso_date() {
        let ts = parse_expiry(Some("2024-06-15")).expect("date should parse");
        assert!(ts > 1_700_000_000, "2024 should be after 2023 epoch");
    }

    #[test]
    fn expiry_treats_blank_as_absent() {
        assert_eq!(parse_expiry(Some("")), None);
        assert_eq!(parse_expiry(Some("   ")), None);
        assert_eq!(parse_expiry(None), None);
    }

    #[test]
    fn expiry_rejects_garbage() {
        assert!(validate_expiry(Some("not-a-date")).is_err());
    }
}
