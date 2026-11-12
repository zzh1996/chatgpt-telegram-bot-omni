//! Classification of the Gemini API's error objects (docs/research/gemini.md §8,
//! <https://aistudio.google.com/docs/api-errors?generate-content=true>):
//! `{"error": {"code": 400, "message": "…", "status": "INVALID_ARGUMENT",
//! "details": [{"@type": "type.googleapis.com/google.rpc.ErrorInfo", "reason": …}, …]}}`.
//!
//! Retried: rate limits (429 `RESOURCE_EXHAUSTED`, after `RetryInfo.retryDelay`
//! or `Retry-After`, else 20 s) except daily quotas, overload and server errors
//! (500 `INTERNAL`, 503 `UNAVAILABLE`, 504 `DEADLINE_EXCEEDED`), cancelled
//! streams. Never retried: invalid requests, prepay credits used up (402),
//! permission / key problems (403), unknown models or files (404), daily
//! quotas, context overflows and exhausted long-decode budgets.

use serde_json::Value;

use crate::error::{ErrorKind, LlmError, Retry, retry_for_status};

/// `error_type` of a long-decode hop resumed without output budget left
/// ("Continuation token has already used output tokens…": `max_output_tokens`
/// must be above the tokens used so far).
pub const BUDGET_EXHAUSTED: &str = "max_output_tokens_exhausted";

/// Seconds of a protobuf `Duration` in JSON (`"52s"`, `"1.5s"`), rounded up.
pub fn duration_secs(s: &str) -> Option<u64> {
    let v: f64 = s.trim().strip_suffix('s')?.parse().ok()?;
    (v.is_finite() && v >= 0.0).then(|| v.ceil() as u64)
}

/// Build an [`LlmError`] from a Gemini API error object (the inner `error`).
pub fn error_from_json(err: &Value, http_status: Option<u16>, retry_after: Option<u64>, kind: ErrorKind) -> LlmError {
    let code = err.get("code").and_then(Value::as_u64).and_then(|c| u16::try_from(c).ok());
    let status = http_status.or(code);
    let grpc = err.get("status").and_then(Value::as_str).unwrap_or("");
    let message = err.get("message").and_then(Value::as_str).unwrap_or("(no message)").to_string();
    let details = err.get("details").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]);
    let detail =
        |ty: &str| details.iter().find(|d| d.get("@type").and_then(Value::as_str).is_some_and(|t| t.ends_with(ty)));
    let reason = detail("google.rpc.ErrorInfo").and_then(|d| d.get("reason")).and_then(Value::as_str);
    let delay = detail("google.rpc.RetryInfo")
        .and_then(|d| d.get("retryDelay"))
        .and_then(Value::as_str)
        .and_then(duration_secs);
    // Per-day quotas cannot recover within the retry window.
    let daily = detail("google.rpc.QuotaFailure")
        .and_then(|d| d.get("violations"))
        .and_then(Value::as_array)
        .is_some_and(|vs| {
            vs.iter().any(|v| {
                ["quotaId", "quotaMetric"]
                    .iter()
                    .filter_map(|k| v.get(*k).and_then(Value::as_str))
                    .any(|s| s.to_ascii_lowercase().contains("perday"))
            })
        });
    let lower = message.to_ascii_lowercase();
    // "The input token count (1200000) exceeds the maximum number of tokens allowed (1048576)."
    let overflow = lower.contains("exceeds the maximum number of tokens")
        || (lower.contains("input token count") && lower.contains("exceed"))
        || lower.contains("context is too long");
    let exhausted = lower.contains("already used output tokens");
    let wait = delay.or(retry_after);
    let rate_limited = status == Some(429) || (status.is_none() && grpc == "RESOURCE_EXHAUSTED");
    let retry = if overflow || exhausted {
        Retry::Never
    } else if rate_limited {
        if daily { Retry::Never } else { Retry::After(wait.unwrap_or(20)) }
    } else {
        match status {
            // 499 CANCELLED: the connection was closed (not by the request's fault).
            Some(499) => wait.map(Retry::After).unwrap_or(Retry::Backoff),
            Some(s) => retry_for_status(s, wait),
            None => match grpc {
                "UNAVAILABLE" | "INTERNAL" | "DEADLINE_EXCEEDED" | "ABORTED" | "UNKNOWN" | "CANCELLED" | "" => {
                    wait.map(Retry::After).unwrap_or(Retry::Backoff)
                }
                _ => Retry::Never,
            },
        }
    };
    // Not `context_length_exceeded`: the input has its own limit on Gemini, so a
    // smaller output budget (the bot's answer to an overflow) cannot help.
    let error_type = if overflow {
        Some("input_too_long".to_string())
    } else if exhausted {
        Some(BUDGET_EXHAUSTED.to_string())
    } else {
        reason.or((!grpc.is_empty()).then_some(grpc)).map(str::to_string)
    };
    LlmError { kind, retry, status, error_type, message, details: Some(err.clone()), partial_output: false }
}
