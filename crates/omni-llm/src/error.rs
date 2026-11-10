//! Errors from LLM calls, with retry classification.
//!
//! Principle: the upstream error is shown to the user *verbatim*. `details`
//! always carries the full error JSON (or raw text) exactly as received.

use std::fmt;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    /// Non-2xx HTTP response before streaming started.
    Http,
    /// Error reported inside a 200 response / stream.
    InBand,
    /// Connection failed, reset, or the stream ended without a terminal event.
    Network,
    /// No data within the idle timeout.
    Timeout,
    /// The response could not be parsed.
    Decode,
    /// The request could not be built (bad options, unsupported input...).
    Request,
    /// Cancelled by the user or shutdown.
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type", content = "secs")]
pub enum Retry {
    /// Retrying cannot help (bad request, auth, policy, ...).
    Never,
    /// Transient: retry with the normal backoff.
    Backoff,
    /// Retry after at least this many seconds (e.g. `Retry-After`).
    After(u64),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LlmError {
    pub kind: ErrorKind,
    pub retry: Retry,
    /// HTTP status (or in-band numeric code), if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    /// Canonical error type when the provider supplies one (e.g. OpenRouter `error_type`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_type: Option<String>,
    /// Short human-readable summary.
    pub message: String,
    /// Verbatim error payload (JSON) or raw body text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
    /// Whether some output had already been received when the error happened.
    #[serde(default)]
    pub partial_output: bool,
}

impl LlmError {
    pub fn new(kind: ErrorKind, retry: Retry, message: impl Into<String>) -> Self {
        Self {
            kind,
            retry,
            status: None,
            error_type: None,
            message: message.into(),
            details: None,
            partial_output: false,
        }
    }

    pub fn request(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Request, Retry::Never, message)
    }

    pub fn network(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Network, Retry::Backoff, message)
    }

    pub fn cancelled() -> Self {
        Self::new(ErrorKind::Cancelled, Retry::Never, "cancelled")
    }

    pub fn with_details(mut self, d: Value) -> Self {
        self.details = Some(d);
        self
    }

    pub fn is_retryable(&self) -> bool {
        !matches!(self.retry, Retry::Never)
    }

    pub fn retry_delay(&self, base: Duration) -> Option<Duration> {
        match self.retry {
            Retry::Never => None,
            Retry::Backoff => Some(base),
            Retry::After(s) => Some(base.max(Duration::from_secs(s))),
        }
    }
}

impl fmt::Display for LlmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self.kind)?;
        if let Some(s) = self.status {
            write!(f, " {s}")?;
        }
        if let Some(t) = &self.error_type {
            write!(f, " [{t}]")?;
        }
        write!(f, ": {}", self.message)
    }
}

impl std::error::Error for LlmError {}

/// Retry classification for HTTP statuses (used when no canonical error type is available).
pub fn retry_for_status(status: u16, retry_after: Option<u64>) -> Retry {
    match status {
        408 | 409 | 425 | 500 | 502 | 503 | 504 | 520..=529 => match retry_after {
            Some(s) => Retry::After(s),
            None => Retry::Backoff,
        },
        429 => Retry::After(retry_after.unwrap_or(20)),
        _ => Retry::Never,
    }
}
