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

    /// The request did not fit the context window (prompt + max_tokens).
    pub fn is_context_overflow(&self) -> bool {
        if matches!(
            self.error_type.as_deref(),
            Some("context_length_exceeded" | "max_tokens_exceeded" | "token_limit_exceeded")
        ) {
            return true;
        }
        let m = self.message.to_ascii_lowercase();
        [
            "maximum context length",
            "context window",
            "context length",
            "too many tokens",
            "prompt is too long",
            "max_tokens",
        ]
        .iter()
        .any(|k| m.contains(k))
            && matches!(self.kind, ErrorKind::Http | ErrorKind::InBand)
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

/// How users read an error: the bot's own checks are just their message; the
/// rest say where it happened (`HTTP 429 [rate_limit]: …`, `provider error
/// [content_filter]: …`, `network error: …`).
impl fmt::Display for LlmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let what = match self.kind {
            ErrorKind::Request => return f.write_str(&self.message),
            ErrorKind::Cancelled => return f.write_str(&self.message),
            ErrorKind::Http => "HTTP",
            ErrorKind::InBand => "provider error",
            ErrorKind::Network => "network error",
            ErrorKind::Timeout => "timeout",
            ErrorKind::Decode => "unreadable response",
        };
        f.write_str(what)?;
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

/// The retry for an error no status or known type classifies, from its own
/// words (a code, a type or the message): rate limits wait, overload, server
/// trouble and timeouts can pass; anything else is not retried (owner,
/// 2026-11-22: only what can pass by itself is retried automatically, never a
/// refusal, an unknown model or a bad request). `None`: the text says neither.
pub fn transient(text: &str) -> Option<Retry> {
    let t = text.to_ascii_lowercase();
    let any = |ks: &[&str]| ks.iter().any(|k| t.contains(k));
    if any(&["rate_limit", "rate limit", "rate-limit", "ratelimit", "too many requests", "throttl"]) {
        Some(Retry::After(20))
    } else if any(&[
        "overload",
        "server_error",
        "server error",
        "internal_error",
        "internal error",
        "internalerror",
        "timeout",
        "timed out",
        "unavailable",
        "bad gateway",
        "temporarily",
        "try again later",
    ]) {
        Some(Retry::Backoff)
    } else {
        None
    }
}

/// Whether an error is about money (credits used up, arrears, a billing
/// problem): never retried automatically, since a top-up does not happen within
/// the retry window; the 🔁 Retry button is there for later (owner, 2026-11-22).
pub fn about_money(text: &str) -> bool {
    let t = text.to_lowercase();
    ["credit", "balance", "billing", "payment", "insufficient_quota", "arrear", "prepay", "余额", "欠费", "充值"]
        .iter()
        .any(|k| t.contains(k))
}

/// [`transient`] over several texts (code, type, message), else `Never`.
pub fn transient_or_never(texts: &[Option<&str>]) -> Retry {
    texts.iter().flatten().find_map(|t| transient(t)).unwrap_or(Retry::Never)
}

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

// ------------------------------------------------------------------ provider reasons
//
// The reason a provider gives (its category or code) belongs in the error's
// title (owner, 2026-11-22/23); its explanation texts stay in the fold, which
// shows the whole payload. No wording per reason: the codes go as they were
// sent, whatever they are.

/// Fields that carry a provider's reason as a code: types, categories,
/// codes, statuses and finish or block reasons.
const CODE_FIELDS: &[&str] = &[
    "type",
    "category",
    "code",
    "status",
    "reason",
    "reasons",
    "error_type",
    "provider_code",
    "provider_error_code",
    "limit_source",
    "blockReason",
    "block_reason",
    "finishReason",
    "finish_reason",
    "native_finish_reason",
    "respect_moderation",
];

/// Objects whose code fields count too: an error inside a stop, OpenRouter's
/// `metadata`, a refusal's `stop_details`.
const CODE_OBJECTS: &[&str] = &["error", "metadata", "stop_details"];

/// A value that reads as a code (one word, not a sentence): `cyber`,
/// `bio_policy`, `SAFETY`, `429`, `false`.
fn as_code(v: &Value) -> Option<String> {
    let token = |s: &str| (!s.is_empty() && s.len() <= 64 && !s.contains(char::is_whitespace)).then(|| s.to_string());
    match v {
        Value::String(s) => token(s.trim()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        Value::Array(a) if !a.is_empty() => {
            let codes: Option<Vec<String>> = a.iter().map(|x| x.as_str().and_then(token)).collect();
            codes.map(|c| c.join(", "))
        }
        _ => None,
    }
}

/// The reason codes of a provider's payload that `title` does not show yet,
/// as `name: value` in the payload's order, exactly as sent.
pub fn reason_codes(v: &Value, title: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen = title.to_lowercase();
    collect_codes(v, &mut seen, &mut out, 0);
    out
}

fn collect_codes(v: &Value, seen: &mut String, out: &mut Vec<String>, depth: usize) {
    let Some(o) = v.as_object() else { return };
    for (k, val) in o {
        if CODE_OBJECTS.contains(&k.as_str()) && val.is_object() {
            if depth < 3 {
                collect_codes(val, seen, out, depth + 1);
            }
            continue;
        }
        if !CODE_FIELDS.contains(&k.as_str()) {
            continue;
        }
        let Some(code) = as_code(val) else { continue };
        if seen.contains(&code.to_lowercase()) {
            continue;
        }
        seen.push(' ');
        seen.push_str(&code.to_lowercase());
        out.push(format!("{k}: {code}"));
    }
}

#[cfg(test)]
mod reason_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn codes_the_title_lacks_as_sent() {
        // Claude through OpenRouter (a test in the test group): the category, not the explanation.
        let d = json!({"type": "refusal", "category": "cyber", "explanation": "Cyber safeguards flagged this message. API integrators: …"});
        assert_eq!(reason_codes(&d, "stopped (finish_reason: content_filter / refusal"), ["category: cyber"]);
        // OpenAI through OpenRouter's Responses API.
        let e = json!({"code": "bio_policy", "message": "This content was flagged for possible biological risk.", "error_type": "refusal"});
        assert_eq!(
            reason_codes(&e, "refusal This content was flagged for possible biological risk."),
            ["code: bio_policy"]
        );
        // What the title says already is not repeated; prose and names are no codes.
        let g = json!({"code": 403, "message": "Gemini blocked the request: OTHER", "metadata": {"error_type": "content_policy_violation", "provider_code": "OTHER", "provider_name": "Google AI Studio"}});
        assert!(reason_codes(&g, "403 content_policy_violation Gemini blocked the request: OTHER").is_empty());
        let m = json!({"code": 403, "message": "Your input was flagged.", "metadata": {"reasons": ["violence", "harassment"], "flagged_input": "…"}});
        assert_eq!(reason_codes(&m, "403 Your input was flagged."), ["reasons: violence, harassment"]);
        let r = json!({"code": 429, "message": "Provider returned error", "metadata": {"raw": "upstream text, any length", "provider_error_code": "429", "limit_source": "upstream_provider_shared_pool"}});
        assert_eq!(reason_codes(&r, "429 Provider returned error"), ["limit_source: upstream_provider_shared_pool"]);
        assert!(reason_codes(&json!({"reason": "a long sentence, not a code"}), "").is_empty());
    }
}
