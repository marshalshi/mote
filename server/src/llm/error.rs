//! Typed provider errors, so the agent loop can decide whether to retry by
//! error class instead of matching substrings in messages.

use std::fmt;
use std::time::Duration;

/// How the agent loop should react to a provider failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderErrorKind {
    /// HTTP 429: retry after the provider's `Retry-After` (or backoff).
    RateLimited,
    /// Network failure, 5xx, timeout, or a stream cut short: retry.
    Transient,
    /// The request exceeds the model's context window: retrying the same
    /// request cannot succeed.
    ContextOverflow,
    /// Anything else (bad request, auth, malformed response): do not retry.
    Fatal,
}

/// A classified provider failure. Travels inside `anyhow::Error`; recover it
/// with `error.downcast_ref::<ProviderError>()`.
#[derive(Debug)]
pub struct ProviderError {
    pub kind: ProviderErrorKind,
    /// HTTP status, when the failure came from a response.
    pub status: Option<u16>,
    /// Delay the provider asked for before retrying.
    pub retry_after: Option<Duration>,
    message: String,
}

impl fmt::Display for ProviderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ProviderError {}

impl ProviderError {
    pub fn new(kind: ProviderErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            status: None,
            retry_after: None,
            message: message.into(),
        }
    }

    pub fn transient(message: impl Into<String>) -> Self {
        Self::new(ProviderErrorKind::Transient, message)
    }

    pub fn fatal(message: impl Into<String>) -> Self {
        Self::new(ProviderErrorKind::Fatal, message)
    }

    /// Classify a non-success HTTP response. `provider` names the provider
    /// in the message, e.g. "DeepSeek API error (429 Too Many Requests): …".
    pub fn from_response(
        provider: &str,
        status: reqwest::StatusCode,
        headers: &reqwest::header::HeaderMap,
        body: &str,
    ) -> Self {
        Self {
            kind: classify_status(status.as_u16(), body),
            status: Some(status.as_u16()),
            retry_after: parse_retry_after(headers),
            message: format!("{provider} API error ({status}): {body}"),
        }
    }

    /// Classify a failure to send a request (connect, TLS, timeout, …).
    pub fn from_send_error(provider: &str, error: &reqwest::Error) -> Self {
        let kind = if error.is_builder() {
            ProviderErrorKind::Fatal
        } else {
            ProviderErrorKind::Transient
        };
        // reqwest's Display omits the cause ("connection refused", TLS
        // details, …); append the source chain so the user can act on it.
        let mut message = format!("{provider} request failed: {error}");
        let mut source = std::error::Error::source(error);
        while let Some(cause) = source {
            message.push_str(&format!(": {cause}"));
            source = cause.source();
        }
        Self::new(kind, message)
    }

    /// An error object sent inside an otherwise successful stream (e.g.
    /// `data: {"error": {...}}` from OpenAI-compatible gateways, or
    /// `{"error": "..."}` from Ollama). Returns `None` when `payload` is not
    /// an error object.
    pub fn from_stream_payload(provider: &str, payload: &serde_json::Value) -> Option<Self> {
        let error = payload.get("error")?;
        let message = error
            .get("message")
            .and_then(serde_json::Value::as_str)
            .or_else(|| error.as_str())
            .map_or_else(|| error.to_string(), str::to_string);
        let lower = message.to_ascii_lowercase();
        let kind = if lower.contains("rate limit") {
            ProviderErrorKind::RateLimited
        } else if mentions_context_overflow(&lower) {
            ProviderErrorKind::ContextOverflow
        } else if [
            "overloaded",
            "unavailable",
            "timeout",
            "timed out",
            "try again",
            "capacity",
            "internal",
        ]
        .iter()
        .any(|marker| lower.contains(marker))
        {
            ProviderErrorKind::Transient
        } else {
            ProviderErrorKind::Fatal
        };
        Some(Self::new(
            kind,
            format!("{provider} stream error: {message}"),
        ))
    }
}

fn classify_status(status: u16, body: &str) -> ProviderErrorKind {
    match status {
        429 => ProviderErrorKind::RateLimited,
        413 => ProviderErrorKind::ContextOverflow,
        400 if mentions_context_overflow(body) => ProviderErrorKind::ContextOverflow,
        // Request timeout, too early, and overloaded (529).
        408 | 425 | 529 => ProviderErrorKind::Transient,
        500..=599 => ProviderErrorKind::Transient,
        _ => ProviderErrorKind::Fatal,
    }
}

/// Provider phrasings for "your prompt is too long".
fn mentions_context_overflow(body: &str) -> bool {
    let body = body.to_ascii_lowercase();
    [
        "context_length_exceeded",
        "maximum context length",
        "context length",
        "context window",
        "prompt is too long",
        "too many tokens",
        "reduce the length",
    ]
    .iter()
    .any(|marker| body.contains(marker))
}

/// `retry-after-ms` (milliseconds) or `retry-after` (seconds or HTTP date).
fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let header = |name: &str| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
    };
    // `try_from_secs_f64` rejects negative, NaN and overflowing values
    // instead of panicking like `from_secs_f64`. An unusable
    // `retry-after-ms` falls back to `retry-after`.
    if let Some(wait) = header("retry-after-ms")
        .and_then(|v| v.parse::<f64>().ok())
        .and_then(|ms| Duration::try_from_secs_f64(ms / 1000.0).ok())
    {
        return Some(wait);
    }
    let value = header("retry-after")?;
    if let Ok(secs) = value.parse::<f64>() {
        return Duration::try_from_secs_f64(secs).ok();
    }
    let at = chrono::DateTime::parse_from_rfc2822(value).ok()?;
    let wait = at.with_timezone(&chrono::Utc) - chrono::Utc::now();
    Some(wait.to_std().unwrap_or(Duration::ZERO))
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::StatusCode;
    use reqwest::header::{HeaderMap, HeaderValue};

    fn classify(status: u16, body: &str) -> ProviderErrorKind {
        ProviderError::from_response(
            "Test",
            StatusCode::from_u16(status).unwrap(),
            &HeaderMap::new(),
            body,
        )
        .kind
    }

    #[test]
    fn classifies_by_status_not_by_digits_in_the_message() {
        use ProviderErrorKind::*;
        assert_eq!(classify(429, ""), RateLimited);
        assert_eq!(classify(503, ""), Transient);
        assert_eq!(classify(529, "overloaded"), Transient);
        assert_eq!(classify(401, "invalid api key"), Fatal);
        // The motivating bug: "70500" contains "500" but this is a 400.
        assert_eq!(classify(400, "bad parameter value 70500"), Fatal);
        assert_eq!(
            classify(
                400,
                "This model's maximum context length is 65536 tokens, you requested 70500 tokens"
            ),
            ContextOverflow
        );
        assert_eq!(classify(413, ""), ContextOverflow);
    }

    #[test]
    fn message_keeps_provider_status_and_body() {
        let error = ProviderError::from_response(
            "DeepSeek",
            StatusCode::TOO_MANY_REQUESTS,
            &HeaderMap::new(),
            "slow down",
        );
        assert_eq!(
            error.to_string(),
            "DeepSeek API error (429 Too Many Requests): slow down"
        );
        assert_eq!(error.status, Some(429));
    }

    #[test]
    fn classifies_error_payloads_inside_streams() {
        let kind = |payload: serde_json::Value| {
            ProviderError::from_stream_payload("Test", &payload).map(|e| e.kind)
        };
        use ProviderErrorKind::*;
        assert_eq!(
            kind(serde_json::json!({"error": {"message": "Server overloaded, try again"}})),
            Some(Transient)
        );
        assert_eq!(
            kind(serde_json::json!({"error": "model runner has unexpectedly stopped"})),
            Some(Fatal)
        );
        assert_eq!(
            kind(serde_json::json!({"error": {"message": "Rate limit reached"}})),
            Some(RateLimited)
        );
        assert_eq!(kind(serde_json::json!({"choices": []})), None);
        let error = ProviderError::from_stream_payload(
            "Ollama",
            &serde_json::json!({"error": "out of memory"}),
        )
        .unwrap();
        assert_eq!(error.to_string(), "Ollama stream error: out of memory");
    }

    #[test]
    fn parses_retry_after_variants() {
        let mut headers = HeaderMap::new();
        headers.insert("retry-after", HeaderValue::from_static("7"));
        assert_eq!(parse_retry_after(&headers), Some(Duration::from_secs(7)));

        headers.insert("retry-after-ms", HeaderValue::from_static("1500"));
        assert_eq!(
            parse_retry_after(&headers),
            Some(Duration::from_millis(1500))
        );

        let mut headers = HeaderMap::new();
        headers.insert(
            "retry-after",
            HeaderValue::from_static("Wed, 21 Oct 2015 07:28:00 GMT"),
        );
        // A date in the past means "retry now".
        assert_eq!(parse_retry_after(&headers), Some(Duration::ZERO));

        // Absurd values must never panic.
        for bad in ["1e400", "1e30", "-5", "NaN", "inf"] {
            let mut headers = HeaderMap::new();
            headers.insert("retry-after", HeaderValue::from_static(bad));
            assert_eq!(parse_retry_after(&headers), None, "{bad}");
        }

        let mut headers = HeaderMap::new();
        headers.insert("retry-after", HeaderValue::from_static("soon"));
        assert_eq!(parse_retry_after(&headers), None);
        assert_eq!(parse_retry_after(&HeaderMap::new()), None);
    }
}
