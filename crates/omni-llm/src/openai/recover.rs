//! A broken OpenAI stream, recovered from the stored copy of its response.
//!
//! Requests are stored (`store: true`). When the client only stops receiving
//! (a dead network: nothing reaches OpenAI), OpenAI finishes the response and
//! `GET {base}/responses/{id}` returns it once it is done (404 while it is
//! still generating); a connection the client closes (a bot restart) cancels
//! the response and nothing is stored (verified live, 2026-11-13). So after a
//! network error or an idle timeout in the middle of a stream whose
//! `response.created` gave the id, [`stored`] polls that URL (with the
//! request's `include`) with a growing pause until the response reached a
//! terminal status or the window of [`window`] ended. [`events`] turns the
//! stored response into the events a live stream of it carries, which go
//! through the same parser, so the answer comes out as the stream would have
//! given it.

use std::time::Duration;

use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::media::{self, Http};
use crate::error::{ErrorKind, LlmError};

/// How the stored copy is polled.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Backoff {
    /// The first pause (after the stream broke).
    pub first: Duration,
    /// Pauses grow by half up to this.
    pub max: Duration,
    /// Bounds of the polling window ([`window`]).
    pub min_window: Duration,
    pub max_window: Duration,
}

impl Backoff {
    pub const DEFAULT: Backoff = Backoff {
        first: Duration::from_secs(2),
        max: Duration::from_secs(15),
        min_window: Duration::from_secs(30),
        max_window: Duration::from_secs(30 * 60),
    };
}

/// How long to poll after a stream broke `elapsed` after its request was
/// sent: twice that, within the bounds (a response that ran for a while may
/// need as long again to finish).
pub(crate) fn window(b: &Backoff, elapsed: Duration) -> Duration {
    (elapsed * 2).clamp(b.min_window, b.max_window)
}

/// Statuses a response ends with.
const TERMINAL: &[&str] = &["completed", "incomplete", "failed", "cancelled"];

/// Poll `url` (the response's own URL) with `include` until it has a
/// terminal status (`Some`), or `window` ends or polling cannot help
/// (`None`). Not stored yet (404), network trouble and transient HTTP errors
/// mean polling on. `Err` only when cancelled.
pub(crate) async fn stored(
    http: &Http<'_>,
    url: &str,
    include: &[String],
    b: &Backoff,
    window: Duration,
    cancel: &CancellationToken,
) -> Result<Option<Value>, LlmError> {
    let end = tokio::time::Instant::now() + window;
    let query: Vec<(&str, &str)> = include.iter().map(|i| ("include[]", i.as_str())).collect();
    let mut pause = b.first;
    loop {
        let left = end.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            return Ok(None);
        }
        media::pause(pause.min(left), cancel).await?;
        match http.get_query(url, &query).await {
            Ok(reply) => {
                if reply.body.get("status").and_then(Value::as_str).is_some_and(|s| TERMINAL.contains(&s)) {
                    return Ok(Some(reply.body));
                }
                // `queued` / `in_progress`: not finished yet.
            }
            Err(e) if e.kind == ErrorKind::Cancelled => return Err(e),
            Err(e)
                if e.status == Some(404)
                    || e.is_retryable()
                    || matches!(e.kind, ErrorKind::Network | ErrorKind::Timeout | ErrorKind::Decode) => {}
            // Authentication, permission, a bad request: polling cannot help.
            Err(_) => return Ok(None),
        }
        pause = pause.mul_f64(1.5).min(b.max);
    }
}

/// Numbered events, as a stream carries them.
struct Seq(Vec<Value>);

impl Seq {
    fn push(&mut self, mut v: Value) {
        v["sequence_number"] = json!(self.0.len());
        self.0.push(v);
    }
}

/// The events a live stream of the final response `r` carries, in order
/// (streaming events reference): `response.created`; per output item its
/// `output_item.added`, its content as deltas (reasoning summary parts,
/// reasoning text, message text with its annotations, refusals, code) and
/// its progress events, `output_item.done`; then `response.completed`,
/// `.incomplete` or `.failed` with the whole response.
pub(crate) fn events(r: &Value) -> Vec<Value> {
    let mut s = Seq(Vec::new());
    let mut start = r.clone();
    if let Some(o) = start.as_object_mut() {
        o.insert("status".into(), json!("in_progress"));
        o.insert("output".into(), json!([]));
        o.insert("usage".into(), Value::Null);
    }
    s.push(json!({"type": "response.created", "response": start}));
    for (i, item) in r.get("output").and_then(Value::as_array).into_iter().flatten().enumerate() {
        item_events(&mut s, i, item);
    }
    let ty = match r.get("status").and_then(Value::as_str) {
        Some("incomplete") => "response.incomplete",
        Some("failed") => "response.failed",
        _ => "response.completed",
    };
    s.push(json!({"type": ty, "response": r}));
    s.0
}

/// An item as `output_item.added` announces it: no content yet.
fn added(item: &Value) -> Value {
    let ty = item.get("type").and_then(Value::as_str).unwrap_or("");
    let mut a = serde_json::Map::new();
    for k in ["id", "type"] {
        if let Some(v) = item.get(k) {
            a.insert(k.into(), v.clone());
        }
    }
    if item.get("status").is_some() {
        a.insert("status".into(), json!("in_progress"));
    }
    match ty {
        "message" => {
            for k in ["role", "phase"] {
                if let Some(v) = item.get(k) {
                    a.insert(k.into(), v.clone());
                }
            }
            a.insert("content".into(), json!([]));
        }
        "reasoning" => {
            a.insert("summary".into(), json!([]));
            if item.get("content").is_some() {
                a.insert("content".into(), json!([]));
            }
            if let Some(e) = item.get("encrypted_content") {
                a.insert("encrypted_content".into(), e.clone());
            }
        }
        "code_interpreter_call" => {
            a.insert("code".into(), json!(""));
            if let Some(c) = item.get("container_id") {
                a.insert("container_id".into(), c.clone());
            }
            a.insert("outputs".into(), json!([]));
        }
        _ => {}
    }
    Value::Object(a)
}

fn item_events(s: &mut Seq, i: usize, item: &Value) {
    let ty = item.get("type").and_then(Value::as_str).unwrap_or("");
    let id = item.get("id").cloned().unwrap_or(Value::Null);
    let completed = item.get("status").and_then(Value::as_str).is_none_or(|st| st == "completed");
    s.push(json!({"type": "response.output_item.added", "output_index": i, "item": added(item)}));
    let at = |ty: &str| json!({"type": ty, "item_id": id, "output_index": i});
    let progress = |s: &mut Seq, tool: &str, steps: &[&str]| {
        for step in steps {
            s.push(at(&format!("response.{tool}.{step}")));
        }
        if completed {
            s.push(at(&format!("response.{tool}.completed")));
        }
    };
    match ty {
        "reasoning" => {
            for (j, part) in item.get("summary").and_then(Value::as_array).into_iter().flatten().enumerate() {
                let text = part.get("text").and_then(Value::as_str).unwrap_or("");
                let ev = |ty: &str| {
                    let mut v = at(ty);
                    v["summary_index"] = json!(j);
                    v
                };
                let mut added = ev("response.reasoning_summary_part.added");
                added["part"] = json!({"type": "summary_text", "text": ""});
                s.push(added);
                let mut delta = ev("response.reasoning_summary_text.delta");
                delta["delta"] = json!(text);
                s.push(delta);
                let mut done = ev("response.reasoning_summary_text.done");
                done["text"] = json!(text);
                s.push(done);
                let mut part_done = ev("response.reasoning_summary_part.done");
                part_done["part"] = part.clone();
                s.push(part_done);
            }
            for (k, part) in item.get("content").and_then(Value::as_array).into_iter().flatten().enumerate() {
                let text = part.get("text").and_then(Value::as_str).unwrap_or("");
                let mut delta = at("response.reasoning_text.delta");
                delta["content_index"] = json!(k);
                delta["delta"] = json!(text);
                s.push(delta);
                let mut done = at("response.reasoning_text.done");
                done["content_index"] = json!(k);
                done["text"] = json!(text);
                s.push(done);
            }
        }
        "message" => {
            for (k, part) in item.get("content").and_then(Value::as_array).into_iter().flatten().enumerate() {
                let ev = |ty: &str| {
                    let mut v = at(ty);
                    v["content_index"] = json!(k);
                    v
                };
                let kind = part.get("type").and_then(Value::as_str).unwrap_or("output_text");
                let mut part_added = ev("response.content_part.added");
                part_added["part"] = match kind {
                    "refusal" => json!({"type": "refusal", "refusal": ""}),
                    _ => json!({"type": kind, "annotations": [], "logprobs": [], "text": ""}),
                };
                s.push(part_added);
                match kind {
                    "refusal" => {
                        let text = part.get("refusal").and_then(Value::as_str).unwrap_or("");
                        let mut delta = ev("response.refusal.delta");
                        delta["delta"] = json!(text);
                        s.push(delta);
                        let mut done = ev("response.refusal.done");
                        done["refusal"] = json!(text);
                        s.push(done);
                    }
                    _ => {
                        let text = part.get("text").and_then(Value::as_str).unwrap_or("");
                        let annotations =
                            part.get("annotations").and_then(Value::as_array).cloned().unwrap_or_default();
                        for piece in text_pieces(text, &annotations) {
                            match piece {
                                Piece::Text(t) => {
                                    let mut delta = ev("response.output_text.delta");
                                    delta["delta"] = json!(t);
                                    delta["logprobs"] = json!([]);
                                    s.push(delta);
                                }
                                Piece::Annotation(n, a) => {
                                    let mut added = ev("response.output_text.annotation.added");
                                    added["annotation_index"] = json!(n);
                                    added["annotation"] = a;
                                    s.push(added);
                                }
                            }
                        }
                        let mut done = ev("response.output_text.done");
                        done["text"] = json!(text);
                        done["logprobs"] = json!([]);
                        s.push(done);
                    }
                }
                let mut part_done = ev("response.content_part.done");
                part_done["part"] = part.clone();
                s.push(part_done);
            }
        }
        "web_search_call" => progress(s, "web_search_call", &["in_progress", "searching"]),
        "file_search_call" => progress(s, "file_search_call", &["in_progress", "searching"]),
        "image_generation_call" => progress(s, "image_generation_call", &["in_progress", "generating"]),
        "code_interpreter_call" => {
            s.push(at("response.code_interpreter_call.in_progress"));
            if let Some(code) = item.get("code").and_then(Value::as_str).filter(|c| !c.is_empty()) {
                let mut delta = at("response.code_interpreter_call_code.delta");
                delta["delta"] = json!(code);
                s.push(delta);
                let mut done = at("response.code_interpreter_call_code.done");
                done["code"] = json!(code);
                s.push(done);
            }
            progress(s, "code_interpreter_call", &["interpreting"]);
        }
        _ => {}
    }
    s.push(json!({"type": "response.output_item.done", "output_index": i, "item": item}));
}

enum Piece<'a> {
    Text(&'a str),
    Annotation(usize, Value),
}

/// A message text with its annotations where a stream announces them: right
/// after the text they end with (`end_index`, `index` for file citations;
/// counted in characters), or after the whole text when they point nowhere in
/// it (a container file citation is `0`–`0` and comes after the text).
fn text_pieces<'a>(text: &'a str, annotations: &[Value]) -> Vec<Piece<'a>> {
    let bytes: Vec<usize> = text.char_indices().map(|(b, _)| b).chain(std::iter::once(text.len())).collect();
    let chars = bytes.len() - 1;
    let mut at: Vec<(usize, usize)> = annotations
        .iter()
        .enumerate()
        .map(|(n, a)| {
            let end = a.get("end_index").or_else(|| a.get("index")).and_then(Value::as_u64).map(|e| e as usize);
            (end.filter(|e| (1..=chars).contains(e)).unwrap_or(chars), n)
        })
        .collect();
    at.sort_by_key(|(pos, _)| *pos);
    let mut out = Vec::new();
    let mut cursor = 0;
    for (pos, n) in at {
        if pos > cursor {
            out.push(Piece::Text(&text[bytes[cursor]..bytes[pos]]));
            cursor = pos;
        }
        out.push(Piece::Annotation(n, annotations[n].clone()));
    }
    if cursor < chars {
        out.push(Piece::Text(&text[bytes[cursor]..]));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_polling_window_is_twice_the_time_so_far_within_bounds() {
        let b = Backoff::DEFAULT;
        assert_eq!(window(&b, Duration::from_secs(3)), Duration::from_secs(30));
        assert_eq!(window(&b, Duration::from_secs(100)), Duration::from_secs(200));
        assert_eq!(window(&b, Duration::from_secs(3600)), Duration::from_secs(1800));
    }

    #[test]
    fn annotations_come_after_the_text_they_end() {
        // Turn 276: the citation ends with the text (character indices: the dash is one).
        let text =
            "Rust 1.100.0 — released November 12, 2026. ([doc.rust-lang.org](https://doc.rust-lang.org/releases.html))";
        let cite = json!({"type": "url_citation", "end_index": 105, "start_index": 43, "title": "Rust Release Notes",
            "url": "https://doc.rust-lang.org/releases.html"});
        let p = text_pieces(text, &[cite]);
        assert!(matches!(&p[..], [Piece::Text(t), Piece::Annotation(0, _)] if *t == text));
        // A link in the middle splits the text; a container file (0–0) goes last.
        let text = "See [plot](sandbox:/mnt/data/plot.png) and the table.";
        let file = json!({"type": "container_file_citation", "start_index": 4, "end_index": 38, "file_id": "f1"});
        let cfile = json!({"type": "container_file_citation", "start_index": 0, "end_index": 0, "file_id": "f2"});
        let p = text_pieces(text, &[cfile, file]);
        let shape: Vec<String> = p
            .iter()
            .map(|x| match x {
                Piece::Text(t) => format!("text {t}"),
                Piece::Annotation(n, _) => format!("annotation {n}"),
            })
            .collect();
        assert_eq!(
            shape,
            ["text See [plot](sandbox:/mnt/data/plot.png)", "annotation 1", "text  and the table.", "annotation 0"]
        );
        assert!(text_pieces("", &[]).is_empty());
    }
}
