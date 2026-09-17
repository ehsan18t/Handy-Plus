//! Typed errors for provider API calls.
//!
//! Upstream collapsed every failure into a `String` and dropped response
//! headers, but the pool has to tell a rate-limited healthy key from a revoked
//! one, and those need opposite handling.

use reqwest::header::HeaderMap;
use std::error::Error as StdError;
use std::fmt;
use std::time::Duration;

/// Cap on an honoured `retry-after`. The header is provider-controlled, so
/// without this one hostile response could bench a working key indefinitely.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(60 * 60);

/// How the pool should react to a failed call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureClass {
    /// Healthy key, momentarily busy. Wait the carried delay, count no strike.
    Transient(Duration),
    /// The provider never rendered a verdict: a failure below HTTP, or a config
    /// error caught before sending. Skipped, never struck.
    Unreachable,
    /// Configuration error. Mark invalid and stop retrying.
    Permanent,
    /// Everything else. Counts one strike against (credential, capability, model).
    Failure,
}

/// A failed provider call.
///
/// `message` is sanitized by the caller: it never contains an API key, and
/// never a raw body from a path that could quote transcription content.
#[derive(Debug, Clone)]
pub struct ApiError {
    /// `None` means the failure happened below HTTP (DNS, TLS, timeout).
    pub status: Option<u16>,
    pub retry_after: Option<Duration>,
    pub message: String,
}

impl ApiError {
    pub fn transport(message: impl Into<String>) -> Self {
        Self {
            status: None,
            retry_after: None,
            message: message.into(),
        }
    }

    pub fn from_status(
        status: u16,
        retry_after: Option<Duration>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            status: Some(status),
            retry_after,
            message: message.into(),
        }
    }

    pub fn is_auth_failure(&self) -> bool {
        matches!(self.status, Some(401) | Some(403))
    }

    pub fn class(&self) -> FailureClass {
        let Some(status) = self.status else {
            return FailureClass::Unreachable;
        };

        if matches!(status, 401 | 403) {
            return FailureClass::Permanent;
        }

        // A wait is honoured only when the server stated one. No header, or a
        // zero wait, is indistinguishable from a key that is out of quota, so it
        // takes a strike; otherwise `Retry-After: 0` forever would never bench
        // anything.
        if let Some(wait) = self.retry_after {
            if wait > Duration::ZERO && (status == 429 || (500..=599).contains(&status)) {
                return FailureClass::Transient(wait);
            }
        }

        FailureClass::Failure
    }

    /// Build an error from a non-success response.
    ///
    /// `retry-after` must be read before `text()` consumes the response, and the
    /// body must be redacted before it can reach a log or the UI.
    pub async fn from_response(context: &str, response: reqwest::Response, secret: &str) -> Self {
        let status = response.status().as_u16();
        let retry_after = parse_retry_after(response.headers());
        let body = match response.text().await {
            Ok(body) => crate::llm_client::redact_and_truncate(&body, secret),
            // Never format the reqwest error: its Display carries the raw URL.
            Err(_) => "<error body could not be read>".to_string(),
        };

        let error = Self::from_status(status, retry_after, format!("{context}: {body}"));
        log::error!("{error}");
        error
    }
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.status {
            Some(status) => write!(f, "{} (status {})", self.message, status),
            None => f.write_str(&self.message),
        }
    }
}

impl StdError for ApiError {}

/// Read `retry-after`, accepting both RFC 9110 forms plus the fractional
/// delay some providers emit. Anything unparseable is treated as absent.
pub fn parse_retry_after(headers: &HeaderMap) -> Option<Duration> {
    let raw = headers.get(reqwest::header::RETRY_AFTER)?.to_str().ok()?;
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }

    if let Ok(seconds) = raw.parse::<f64>() {
        if !seconds.is_finite() || seconds < 0.0 {
            return None;
        }
        // Clamp before constructing: `Duration::from_secs_f64` panics above
        // ~1.8e19, and this value comes straight off the wire.
        return Duration::try_from_secs_f64(seconds.min(MAX_RETRY_AFTER.as_secs_f64())).ok();
    }

    let target = chrono::DateTime::parse_from_rfc2822(raw).ok()?;
    let delta = target.timestamp_millis() - chrono::Utc::now().timestamp_millis();
    if delta <= 0 {
        return Some(Duration::ZERO);
    }
    Some(Duration::from_millis(delta as u64).min(MAX_RETRY_AFTER))
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::{HeaderMap, HeaderValue, RETRY_AFTER};

    fn headers_with(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(RETRY_AFTER, HeaderValue::from_str(value).unwrap());
        headers
    }

    #[test]
    fn failures_are_classified_by_status_and_header() {
        use FailureClass::*;
        let thirty = Duration::from_secs(30);
        let cases = [
            (ApiError::from_status(401, None, ""), Permanent),
            (ApiError::from_status(403, None, ""), Permanent),
            (
                ApiError::from_status(429, Some(thirty), ""),
                Transient(thirty),
            ),
            (
                ApiError::from_status(503, Some(thirty), ""),
                Transient(thirty),
            ),
            // No header, or a zero wait, is indistinguishable from exhaustion.
            (ApiError::from_status(429, None, ""), Failure),
            (
                ApiError::from_status(429, Some(Duration::ZERO), ""),
                Failure,
            ),
            (ApiError::from_status(500, None, ""), Failure),
            (ApiError::transport("connection reset"), Unreachable),
        ];
        for (error, expected) in cases {
            assert_eq!(error.class(), expected, "{error:?}");
        }
    }

    #[test]
    fn retry_after_accepts_every_legal_form() {
        assert_eq!(
            parse_retry_after(&headers_with("30")),
            Some(Duration::from_secs(30))
        );
        assert_eq!(
            parse_retry_after(&headers_with("1.5")),
            Some(Duration::from_millis(1500))
        );

        let future = chrono::Utc::now() + chrono::Duration::seconds(120);
        let parsed = parse_retry_after(&headers_with(&future.to_rfc2822())).unwrap();
        assert!(parsed <= Duration::from_secs(120) && parsed >= Duration::from_secs(110));

        // A date already past means "retry now", not "no header".
        let past = chrono::Utc::now() - chrono::Duration::seconds(60);
        assert_eq!(
            parse_retry_after(&headers_with(&past.to_rfc2822())),
            Some(Duration::ZERO)
        );
    }

    #[test]
    fn hostile_retry_after_values_clamp_or_are_ignored_but_never_panic() {
        // `Duration::from_secs_f64` panics past ~1.8e19 seconds, and this value
        // is provider-controlled: a panic here unwinds the transcription task.
        for huge in ["1e300", "99999999999999999999", "18446744073709551616"] {
            assert_eq!(
                parse_retry_after(&headers_with(huge)),
                Some(MAX_RETRY_AFTER)
            );
        }
        for bad in ["NaN", "inf", "-inf", "-5", "soon", ""] {
            assert_eq!(parse_retry_after(&headers_with(bad)), None, "{bad}");
        }
        assert_eq!(parse_retry_after(&HeaderMap::new()), None);
    }
}
