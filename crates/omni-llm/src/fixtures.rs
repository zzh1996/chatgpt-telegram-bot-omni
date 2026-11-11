//! Recorded upstream responses (tests only): `tests/fixtures/*.sse` hold the
//! SSE `data:` payloads exactly as received, so parsers are tested against what
//! providers really send without calling any API.

use serde_json::Value;

/// A recorded case of `tests/fixtures/errors/` (see `errprobe` in the docs):
/// `{case, endpoint, model, status, headers?, body? | sse?}`.
pub fn case(name: &str) -> Value {
    let path = format!("{}/tests/fixtures/errors/{name}.json", env!("CARGO_MANIFEST_DIR"));
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{path}: {e}"))
}

/// The `data:` payloads of a recorded stream, in order (`[DONE]` skipped).
pub fn sse(name: &str) -> Vec<Value> {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
    text.lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .filter(|d| *d != "[DONE]")
        .map(|d| serde_json::from_str(d).unwrap_or_else(|e| panic!("{path}: {e}: {d}")))
        .collect()
}
