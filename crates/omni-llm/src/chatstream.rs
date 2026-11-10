//! Streaming an OpenAI-compatible Chat Completions SSE response (shared by
//! providers with that wire format: OpenRouter, Volcengine Ark).

use std::time::Duration;

use futures::StreamExt;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::error::{ErrorKind, LlmError, Retry};
use crate::event::StreamEvent;
use crate::openrouter::chat::{ChunkParser, ErrorMapper};
use crate::sse::{SseItem, SseParser};
use crate::{EventSink, Observer};

/// Result of [`run`]: the stream outcome, the parser (accumulated native
/// payload and extras) and the id from the response header, if any.
pub struct Streamed {
    pub result: Result<(), LlmError>,
    pub parser: ChunkParser,
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
) -> Streamed {
    let mut cp = ChunkParser::default();
    cp.map_error = Some(map_error);
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
    let retry_after =
        resp.headers().get("retry-after").and_then(|v| v.to_str().ok()).and_then(|s| s.trim().parse::<u64>().ok());
    if !(200..300).contains(&status) {
        let text = resp.text().await.unwrap_or_default();
        observer.body_text(&text);
        let err = match serde_json::from_str::<Value>(&text) {
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
                le.details = Some(Value::String(text));
                le
            }
        };
        return Streamed { result: Err(err), parser: cp, header_id };
    }

    let mut parser = SseParser::new();
    let mut stream = resp.bytes_stream();
    let mut done = false;
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
                e.partial_output = cp.any_output();
                break Err(e);
            }
            Ok(None) => break Ok(()),
            Ok(Some(Err(e))) => {
                let mut le = LlmError::network(format!("stream read error: {e}"));
                le.partial_output = cp.any_output();
                break Err(le);
            }
            Ok(Some(Ok(b))) => b,
        };
        let items = parser.feed(&chunk);
        if let Err(e) = handle_items(items, &mut cp, sink, observer, &mut done) {
            break Err(e);
        }
        if done {
            break Ok(());
        }
    };
    let result = match result {
        Ok(()) if !done => {
            let tail = parser.finish();
            match handle_items(tail, &mut cp, sink, observer, &mut done) {
                Err(e) => Err(e),
                Ok(()) if !cp.finished && !done => {
                    let mut e = LlmError::network("stream ended without a finish reason or [DONE]");
                    e.partial_output = cp.any_output();
                    Err(e)
                }
                Ok(()) => Ok(()),
            }
        }
        other => other,
    };
    Streamed { result, parser: cp, header_id }
}

fn handle_items(
    items: Vec<SseItem>,
    cp: &mut ChunkParser,
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
                for e in cp.chunk(&v)? {
                    sink.event(e);
                }
            }
        }
    }
    Ok(())
}
