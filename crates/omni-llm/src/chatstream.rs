//! Streaming an SSE response through a dialect parser (Chat Completions chunks,
//! Responses events, Messages events), shared by all providers.

use std::time::Duration;

use futures::StreamExt;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::error::{ErrorKind, LlmError, Retry};
use crate::event::StreamEvent;
use crate::openrouter::chat::{ChunkParser, ErrorMapper};

/// A dialect's stream parser.
pub trait EventParser: Send {
    /// One SSE event (`name` is the `event:` field, if any).
    fn event(&mut self, name: Option<&str>, data: &Value) -> Result<Vec<StreamEvent>, LlmError>;
    /// The stream reached its terminal event (finish reason, `response.completed`, `message_stop`…).
    fn finished(&self) -> bool;
    fn any_output(&self) -> bool;
}

impl EventParser for ChunkParser {
    fn event(&mut self, _name: Option<&str>, data: &Value) -> Result<Vec<StreamEvent>, LlmError> {
        self.chunk(data)
    }
    fn finished(&self) -> bool {
        self.finished
    }
    fn any_output(&self) -> bool {
        ChunkParser::any_output(self)
    }
}
use crate::sse::{SseItem, SseParser};
use crate::{EventSink, Observer};

/// Result of [`run`]: the stream outcome, the parser (accumulated native
/// payload and extras) and the id from the response header, if any.
pub struct Streamed<P> {
    pub result: Result<(), LlmError>,
    pub parser: P,
    pub header_id: Option<String>,
}

/// Send `rb` (a prepared POST with the JSON body) and stream the response.
///
/// - `id_header`: response header carrying the generation id (emitted as `Meta`);
/// - `map_error`: classification of the provider's error objects (HTTP and in-band).
#[allow(clippy::too_many_arguments)]
pub async fn run(
    rb: reqwest::RequestBuilder,
    url: &str,
    body: &Value,
    idle_timeout: Duration,
    id_header: Option<&str>,
    map_error: ErrorMapper,
    sink: &mut dyn EventSink,
    observer: &dyn Observer,
    cancel: &CancellationToken,
) -> Streamed<ChunkParser> {
    let mut cp = ChunkParser::default();
    cp.map_error = Some(map_error);
    run_with(rb, url, body, idle_timeout, id_header, map_error, cp, sink, observer, cancel).await
}

/// What a response says about retrying: the delay (`retry-after-ms`, else
/// `retry-after` in seconds, fractional, or an HTTP date), rounded up to whole
/// seconds, and `x-should-retry` (`true`/`false`), the explicit hint the
/// official SDKs follow.
pub fn retry_hints(h: &reqwest::header::HeaderMap) -> (Option<u64>, Option<bool>) {
    let get = |k: &str| h.get(k).and_then(|v| v.to_str().ok()).map(str::trim);
    let secs = |x: f64| (x.is_finite() && x >= 0.0).then(|| x.ceil() as u64);
    let after = get("retry-after-ms")
        .and_then(|v| v.parse::<f64>().ok())
        .and_then(|ms| secs(ms / 1000.0))
        .or_else(|| get("retry-after").and_then(|v| v.parse::<f64>().ok().and_then(secs).or_else(|| http_date_in(v))));
    let should = match get("x-should-retry") {
        Some("true") => Some(true),
        Some("false") => Some(false),
        _ => None,
    };
    (after, should)
}

/// Seconds from now until an IMF-fixdate (`Wed, 21 Oct 2015 07:28:00 GMT`); 0 if past.
fn http_date_in(v: &str) -> Option<u64> {
    let p: Vec<&str> = v.split_whitespace().collect();
    let [_, day, mon, year, time, "GMT"] = p.as_slice() else { return None };
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let m = MONTHS.iter().position(|x| x == mon)? as i64 + 1;
    let (d, y): (i64, i64) = (day.parse().ok()?, year.parse().ok()?);
    let hms: Vec<i64> = time.split(':').map(|x| x.parse().ok()).collect::<Option<_>>()?;
    let [hh, mm, ss] = hms.as_slice() else { return None };
    // Days since 1970-01-01 of a civil date (Howard Hinnant's algorithm).
    let (y2, m2) = if m <= 2 { (y - 1, m + 9) } else { (y, m - 3) };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let doy = (153 * m2 + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    let at = days * 86400 + hh * 3600 + mm * 60 + ss;
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).ok()?.as_secs() as i64;
    Some((at - now).max(0) as u64)
}

/// The error for a non-2xx response before streaming started (`text`: the body).
/// The whole body is kept as the error's details.
pub fn http_error(status: u16, retry_after: Option<u64>, text: &str, map_error: ErrorMapper) -> LlmError {
    match serde_json::from_str::<Value>(text) {
        Ok(v) => {
            let e = v.get("error").cloned().unwrap_or(v.clone());
            let mut le = map_error(&e, Some(status), retry_after, ErrorKind::Http);
            le.details = Some(v);
            le
        }
        Err(_) => {
            let mut le = LlmError::new(
                ErrorKind::Http,
                crate::error::retry_for_status(status, retry_after),
                format!("HTTP {status}"),
            );
            le.status = Some(status);
            le.details = Some(Value::String(text.to_string()));
            le
        }
    }
}

/// [`run`] with any dialect parser.
#[allow(clippy::too_many_arguments)]
pub async fn run_with<P: EventParser>(
    rb: reqwest::RequestBuilder,
    url: &str,
    body: &Value,
    idle_timeout: Duration,
    id_header: Option<&str>,
    map_error: ErrorMapper,
    mut cp: P,
    sink: &mut dyn EventSink,
    observer: &dyn Observer,
    cancel: &CancellationToken,
) -> Streamed<P> {
    observer.request(url, body);
    let send = rb.send();
    let resp = tokio::select! {
        _ = cancel.cancelled() => return Streamed { result: Err(LlmError::cancelled()), parser: cp, header_id: None },
        r = tokio::time::timeout(idle_timeout, send) => match r {
            Err(_) => {
                let e = LlmError::new(ErrorKind::Timeout, Retry::Backoff, "no response headers within idle timeout");
                return Streamed { result: Err(e), parser: cp, header_id: None };
            }
            Ok(Err(e)) => {
                return Streamed { result: Err(LlmError::network(format!("request failed: {e}"))), parser: cp, header_id: None };
            }
            Ok(Ok(r)) => r,
        }
    };
    let status = resp.status().as_u16();
    let headers: Vec<(String, String)> = resp
        .headers()
        .iter()
        .map(|(k, v)| (k.to_string(), String::from_utf8_lossy(v.as_bytes()).to_string()))
        .collect();
    observer.response_status(status, &headers);
    let header_id = id_header.and_then(|h| resp.headers().get(h)).and_then(|v| v.to_str().ok()).map(str::to_string);
    if let Some(g) = &header_id {
        sink.event(StreamEvent::Meta { upstream_id: Some(g.clone()), provider: None, model: None });
    }
    let (retry_after, should_retry) = retry_hints(resp.headers());
    if !(200..300).contains(&status) {
        let text = resp.text().await.unwrap_or_default();
        observer.body_text(&text);
        let mut err = http_error(status, retry_after, &text, map_error);
        // The server's explicit hint wins, as in the official SDKs.
        match should_retry {
            Some(false) => err.retry = Retry::Never,
            Some(true) if err.retry == Retry::Never => {
                err.retry = retry_after.map_or(Retry::Backoff, Retry::After);
            }
            _ => {}
        }
        return Streamed { result: Err(err), parser: cp, header_id };
    }

    let mut parser = SseParser::new();
    let mut stream = resp.bytes_stream();
    let mut done = false;
    // Longest silence between two chunks (keep-alive comments included), logged at
    // the end: shows how close streams come to the idle timeout.
    let mut last_data = tokio::time::Instant::now();
    let mut longest_gap = std::time::Duration::ZERO;
    let result: Result<(), LlmError> = loop {
        let next = tokio::select! {
            _ = cancel.cancelled() => break Err(LlmError::cancelled()),
            n = tokio::time::timeout(idle_timeout, stream.next()) => n,
        };
        let chunk = match next {
            Err(_) => {
                let mut e = LlmError::new(
                    ErrorKind::Timeout,
                    Retry::Backoff,
                    format!("stream idle for {}s", idle_timeout.as_secs()),
                );
                e.partial_output = EventParser::any_output(&cp);
                break Err(e);
            }
            Ok(None) => break Ok(()),
            Ok(Some(Err(e))) => {
                let mut le = LlmError::network(format!("stream read error: {e}"));
                le.partial_output = EventParser::any_output(&cp);
                break Err(le);
            }
            Ok(Some(Ok(b))) => b,
        };
        longest_gap = longest_gap.max(last_data.elapsed());
        last_data = tokio::time::Instant::now();
        let items = parser.feed(&chunk);
        if let Err(e) = handle_items(items, map_error, &mut cp, sink, observer, &mut done) {
            break Err(e);
        }
        if done {
            break Ok(());
        }
    };
    tracing::info!(
        longest_gap_ms = longest_gap.as_millis() as u64,
        idle_timeout_s = idle_timeout.as_secs(),
        "stream ended"
    );
    let result = match result {
        Ok(()) if !done => {
            let tail = parser.finish();
            match handle_items(tail, map_error, &mut cp, sink, observer, &mut done) {
                Err(e) => Err(e),
                Ok(()) if !cp.finished() && !done => {
                    let mut e = LlmError::network("stream ended without a finish reason or [DONE]");
                    e.partial_output = EventParser::any_output(&cp);
                    Err(e)
                }
                Ok(()) => Ok(()),
            }
        }
        other => other,
    };
    Streamed { result, parser: cp, header_id }
}

fn handle_items<P: EventParser>(
    items: Vec<SseItem>,
    map_error: ErrorMapper,
    cp: &mut P,
    sink: &mut dyn EventSink,
    observer: &dyn Observer,
    done: &mut bool,
) -> Result<(), LlmError> {
    for item in items {
        if *done {
            return Ok(());
        }
        match item {
            SseItem::Comment(c) => observer.sse_comment(&c),
            // A bare JSON line instead of an event: an error object, reported as
            // one (anything else is only logged).
            SseItem::Json(line) => {
                observer.sse_data(&line);
                if let Ok(v) = serde_json::from_str::<Value>(&line)
                    && let Some(err) = v.get("error")
                {
                    let mut e = map_error(err, None, None, ErrorKind::InBand);
                    e.partial_output = cp.any_output();
                    e.details = Some(v);
                    return Err(e);
                }
            }
            SseItem::Event(ev) => {
                observer.sse_data(&ev.data);
                let data = ev.data.trim();
                if data.is_empty() {
                    continue;
                }
                if data == "[DONE]" {
                    *done = true;
                    return Ok(());
                }
                let v: Value = serde_json::from_str(data).map_err(|e| {
                    LlmError::new(ErrorKind::Decode, Retry::Backoff, format!("bad JSON chunk: {e}"))
                        .with_details(Value::String(data.to_string()))
                })?;
                for e in cp.event(ev.event.as_deref(), &v)? {
                    sink.event(e);
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use reqwest::header::{HeaderMap, HeaderValue};

    use super::*;

    fn h(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut m = HeaderMap::new();
        for (k, v) in pairs {
            m.insert(*k, HeaderValue::from_str(v).unwrap());
        }
        m
    }

    #[test]
    fn retry_hints_from_headers() {
        assert_eq!(retry_hints(&h(&[("retry-after", "7")])), (Some(7), None));
        assert_eq!(retry_hints(&h(&[("retry-after", "1.2")])), (Some(2), None));
        // Milliseconds win over seconds.
        assert_eq!(retry_hints(&h(&[("retry-after-ms", "1500"), ("retry-after", "9")])), (Some(2), None));
        assert_eq!(retry_hints(&h(&[("x-should-retry", "false")])), (None, Some(false)));
        assert_eq!(retry_hints(&h(&[("x-should-retry", "true")])), (None, Some(true)));
        // A date in the past means now.
        assert_eq!(retry_hints(&h(&[("retry-after", "Wed, 21 Oct 2015 07:28:00 GMT")])), (Some(0), None));
        let soon = http_date_in("Fri, 01 Jan 2100 00:00:00 GMT").unwrap();
        assert!(soon > 2_000_000_000, "{soon}");
        assert_eq!(retry_hints(&h(&[("retry-after", "soon")])), (None, None));
    }
}
