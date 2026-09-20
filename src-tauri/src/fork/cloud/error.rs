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
    /// The provider refused the request itself, not the key: the call is too big
    /// for the account's per-minute ceiling however long you wait, or the answer
    /// came back cut off at the cap. Another key on a higher tier may still take
    /// it, so the rotation continues, but no strike: benching a key for six
    /// hours over a request no key could have served is how a working setup goes
    /// dark for an afternoon.
    RequestRejected,
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
    /// Set by a caller that already knows the request, not the key, is at fault.
    /// Kept separate from `status` because the clearest case of it, an answer
    /// truncated at the output cap, arrives as a perfectly ordinary 200.
    request_fault: bool,
}

impl ApiError {
    pub fn transport(message: impl Into<String>) -> Self {
        Self {
            status: None,
            retry_after: None,
            message: message.into(),
            request_fault: false,
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
            request_fault: false,
        }
    }

    /// The request could not be served as sent, and sending it again unchanged
    /// will fail the same way.
    pub fn request_rejected(message: impl Into<String>) -> Self {
        Self {
            status: None,
            retry_after: None,
            message: message.into(),
            request_fault: true,
        }
    }

    pub fn is_request_fault(&self) -> bool {
        matches!(self.class(), FailureClass::RequestRejected)
    }

    pub fn is_auth_failure(&self) -> bool {
        matches!(self.status, Some(401) | Some(403))
    }

    pub fn class(&self) -> FailureClass {
        if self.request_fault {
            return FailureClass::RequestRejected;
        }

        let Some(status) = self.status else {
            return FailureClass::Unreachable;
        };

        if matches!(status, 401 | 403) {
            return FailureClass::Permanent;
        }

        // Read before the `retry-after` branch on purpose. Providers do attach a
        // wait to this one, and honouring it would bench the key for that long
        // over something waiting cannot fix.
        if rejects_the_request_itself(status, &self.message) {
            return FailureClass::RequestRejected;
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

/// Whether a provider is refusing this request's shape rather than reporting on
/// the key.
///
/// Matching on the body text is not a choice. The two cases arrive as the same
/// status, from the same endpoint, and only the prose distinguishes them: "rate
/// limit reached, used 30 of 30" is the key's quota and clears itself, while
/// "request too large, limit 1000, requested 1155" is a single call that exceeds
/// the whole per-minute ceiling and will do so in an empty minute just the same.
/// The phrases below are the ones OpenAI-compatible providers actually emit for
/// the second. Missing one costs a strike, which is what happened before this
/// existed, so the list errs toward the specific.
fn rejects_the_request_itself(status: u16, message: &str) -> bool {
    // Payload Too Large has exactly one meaning.
    if status == 413 {
        return true;
    }
    if !matches!(status, 400 | 429) {
        return false;
    }

    let message = message.to_ascii_lowercase();
    [
        "request too large",
        "too large for model",
        "reduce max_tokens",
        "reduce the length",
        "context_length_exceeded",
        "context length",
        "maximum context",
    ]
    .iter()
    .any(|phrase| message.contains(phrase))
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
    fn a_request_the_provider_will_never_accept_does_not_strike_the_key() {
        // The real body, from handy.log. Three of these benched both keys for
        // six hours over a call that no key on that tier could have served.
        let groq = ApiError::from_status(
            429,
            None,
            "API request failed: {\"error\":{\"message\":\"Request too large for model \
             `qwen/qwen3.8-27b` in organization `org_x` service tier `on_demand` on output \
             tokens per minute (OTPM): Limit 1000, Requested 1155. The request's expected \
             output tokens exceed the enforced limit; reduce max_tokens (or the request's \
             expected output) and try again.\",\"type\":\"tokens\",\"code\":\"rate_limit_exceeded\"}}",
        );
        assert_eq!(groq.class(), FailureClass::RequestRejected);
        assert!(groq.is_request_fault());

        // Even with a wait attached: waiting does not shrink the request.
        let with_wait = ApiError::from_status(
            429,
            Some(Duration::from_secs(30)),
            "Request too large for model `x`",
        );
        assert_eq!(with_wait.class(), FailureClass::RequestRejected);

        // Payload Too Large has only the one meaning.
        assert_eq!(
            ApiError::from_status(413, None, "whatever").class(),
            FailureClass::RequestRejected
        );

        // An answer cut off at the cap is the same category, on a 200.
        assert_eq!(
            ApiError::request_rejected("answer stopped at the 640 token output limit").class(),
            FailureClass::RequestRejected
        );
    }

    #[test]
    fn an_ordinary_exhausted_quota_still_strikes() {
        // The distinction that matters: this one clears itself and says
        // something about the key, so it must keep its old classification.
        let quota = ApiError::from_status(
            429,
            None,
            "API request failed: {\"error\":{\"message\":\"Rate limit reached for model \
             `qwen` in organization `org_x`: Limit 30, Used 30, Requested 1.\",\
             \"code\":\"rate_limit_exceeded\"}}",
        );
        assert_eq!(quota.class(), FailureClass::Failure);
        assert!(!quota.is_request_fault());
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
