//! Stream-side tests of the OpenAI provider against a local server:
//! recovery of a broken stream from the stored response, container files
//! fetched where cited, and the notices about the response's input.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use omni_core::content::{Part, ReasoningKind};
use omni_core::trigger::OptionToken;
use omni_core::{BlobRef, Turn};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::wire::tests::MemBlobs;
use super::wire::{Parser, Platform};
use super::*;
use crate::{CallInput, StreamEvent};

/// What the test server answers.
enum Reply {
    /// Status, content type, body.
    Full(u16, &'static str, Vec<u8>),
    /// An SSE stream of these events, then the connection drops before its end.
    Cut(Vec<Value>),
}

type Log = Arc<Mutex<Vec<(String, Option<String>)>>>;

/// A local server answering each request with `route(method, path with query,
/// how many requests that path had before)`; returns its base URL and the
/// requests (`METHOD path`, authorization).
async fn serve(route: impl Fn(&str, &str, usize) -> Reply + Send + Sync + 'static) -> (String, Log) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let log: Log = Default::default();
    let got = log.clone();
    let route = Arc::new(route);
    tokio::spawn(async move {
        let seen: Arc<Mutex<HashMap<String, usize>>> = Default::default();
        loop {
            let Ok((mut sock, _)) = listener.accept().await else { return };
            let (route, got, seen) = (route.clone(), got.clone(), seen.clone());
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut tmp = [0u8; 65536];
                let head = loop {
                    let n = sock.read(&mut tmp).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                    if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&buf[..pos]).to_string();
                        let len: usize = head
                            .lines()
                            .find_map(|l| {
                                l.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|v| v.trim().parse().unwrap())
                            })
                            .unwrap_or(0);
                        while buf.len() < pos + 4 + len {
                            let n = sock.read(&mut tmp).await.unwrap_or(0);
                            if n == 0 {
                                break;
                            }
                            buf.extend_from_slice(&tmp[..n]);
                        }
                        break head;
                    }
                };
                let mut it = head.lines().next().unwrap_or("").split(' ');
                let (method, path) = (it.next().unwrap_or("").to_string(), it.next().unwrap_or("").to_string());
                let auth = head.lines().find_map(|l| {
                    let (k, v) = l.split_once(':')?;
                    k.eq_ignore_ascii_case("authorization").then(|| v.trim().to_string())
                });
                got.lock().unwrap().push((format!("{method} {path}"), auth));
                let n = {
                    let mut s = seen.lock().unwrap();
                    let c = s.entry(path.clone()).or_insert(0);
                    *c += 1;
                    *c - 1
                };
                match route(&method, &path, n) {
                    Reply::Full(status, ctype, body) => {
                        let head = format!(
                            "HTTP/1.1 {status} X\r\ncontent-type: {ctype}\r\ncontent-length: {}\r\nx-request-id: req_{n}\r\nconnection: close\r\n\r\n",
                            body.len()
                        );
                        let _ = sock.write_all(head.as_bytes()).await;
                        let _ = sock.write_all(&body).await;
                        let _ = sock.flush().await;
                    }
                    Reply::Cut(events) => {
                        let head =
                            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\n";
                        let _ = sock.write_all(head.as_bytes()).await;
                        for e in events {
                            let data = format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap_or(""));
                            let _ = sock.write_all(format!("{:x}\r\n{data}\r\n", data.len()).as_bytes()).await;
                        }
                        let _ = sock.flush().await;
                        // Dropped without the last chunk: the client sees the stream break.
                    }
                }
            });
        }
    });
    (format!("http://{addr}/v1"), log)
}

fn sse(events: &[Value]) -> Vec<u8> {
    events
        .iter()
        .map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap()))
        .collect::<String>()
        .into_bytes()
}

fn cfg(base: &str) -> DirectConfig {
    DirectConfig {
        api_key: "sk-test".into(),
        base_url: base.into(),
        connect_timeout: Duration::from_secs(5),
        idle_timeout: Duration::from_secs(5),
        max_tokens_cap: None,
        cny_per_usd: 7.1,
        long_decode_budget: 393_216,
    }
}

const QUICK: recover::Backoff = recover::Backoff {
    first: Duration::from_millis(5),
    max: Duration::from_millis(20),
    min_window: Duration::from_millis(500),
    max_window: Duration::from_millis(500),
};

async fn prepared(o: &OpenAi, model: &str, opts: &[OptionToken], history: &[Turn]) -> Prepared {
    let input = CallInput {
        model,
        options: opts,
        history,
        session_id: Some("tg-1".into()),
        user: None,
        known_context: None,
        max_tokens_override: None,
    };
    o.prepare(&input, &[], &MemBlobs(vec![])).await.unwrap()
}

/// Run `opts` on `gpt-6.1-sol` against the server at `base`.
async fn run(base: &str, opts: &[OptionToken]) -> (Result<Value, LlmError>, CallOutcome, Vec<StreamEvent>) {
    let mut o = OpenAi::new(cfg(base)).unwrap();
    o.recovery = QUICK;
    let p = prepared(&o, "gpt-6.1-sol", opts, &[Turn::user_text("hi")]).await;
    let mut events = Vec::new();
    let mut sink = |e: StreamEvent| events.push(e);
    let (r, outcome) = o.stream(&p, &mut sink, &crate::NullObserver, &CancellationToken::new()).await;
    (r, outcome, events)
}

/// The turn the bot builds from the events (generated images and files stored as it stores them).
fn accumulate(events: &[StreamEvent]) -> crate::Accumulator {
    let mut acc = crate::Accumulator::new();
    acc.begin_call();
    for e in events {
        acc.apply(e.clone());
        for (data, mime, thought) in acc.take_pending_images() {
            acc.attach_image(BlobRef::for_bytes(&data, mime), None, thought);
        }
        for (data, mime, name) in acc.take_pending_files() {
            acc.attach_file(BlobRef::for_bytes(&data, mime), name);
        }
    }
    acc
}

fn flag(f: &str) -> OptionToken {
    OptionToken::Flag(f.into())
}

const PNG: &[u8] = b"\x89PNG\r\n\x1a\n-sine-plot";

/// A finished response as `GET /v1/responses/{id}` returns it: reasoning in two summary
/// parts, a search, a code run with an inline plot, an opaque reasoning item, and an
/// answer with a citation (shapes of turns 276 and 351, 2026-11-13).
fn finished() -> Value {
    json!({"id": "resp_rec", "object": "response", "created_at": 1794521536, "status": "completed", "background": false,
        "completed_at": 1794521546, "error": null, "incomplete_details": null, "model": "gpt-6.1-sol", "service_tier": "default",
        "store": true, "input_transformations": [],
        "output": [
            {"id": "rs_1", "type": "reasoning", "content": [], "encrypted_content": "gAAAA-1",
             "summary": [{"type": "summary_text", "text": "**Looking for version details**\n\nI need the latest release."},
                         {"type": "summary_text", "text": "**Crafting the answer**\n\nOne line."}]},
            {"id": "ws_1", "type": "web_search_call", "status": "completed",
             "action": {"type": "search", "queries": ["latest stable Rust"], "query": "latest stable Rust",
                        "sources": [{"type": "url", "url": "https://blog.rust-lang.org/"}]}},
            {"id": "ci_1", "type": "code_interpreter_call", "status": "completed", "code": "import numpy as np\nplt.show()",
             "container_id": "cntr_1", "outputs": [{"type": "logs", "logs": "ok"},
                {"type": "image", "url": format!("data:image/png;base64,{}", crate::provider::b64(PNG))}]},
            {"id": "rs_2", "type": "reasoning", "content": [], "encrypted_content": "gAAAA-2", "summary": []},
            {"id": "msg_1", "type": "message", "status": "completed", "role": "assistant", "phase": "final_answer",
             "content": [{"type": "output_text", "logprobs": [],
                "text": "Rust 1.100.0 — released November 12, 2026. ([doc.rust-lang.org](https://doc.rust-lang.org/releases.html))",
                "annotations": [{"type": "url_citation", "end_index": 105, "start_index": 43, "title": "Rust Release Notes",
                                 "url": "https://doc.rust-lang.org/releases.html"}]}]}
        ],
        "usage": {"input_tokens": 17334, "input_tokens_details": {"cached_tokens": 0, "cache_write_tokens": 0},
                  "output_tokens": 805, "output_tokens_details": {"reasoning_tokens": 767}, "total_tokens": 18139},
        "tool_usage": {"web_search": {"num_requests": 1}}})
}

/// The live stream of [`finished`], with the deltas split as a stream splits them.
fn live() -> Vec<Value> {
    let r = finished();
    let out = &r["output"];
    let mut created = r.clone();
    created["status"] = json!("in_progress");
    created["output"] = json!([]);
    created["usage"] = Value::Null;
    let mut e = vec![
        json!({"type": "response.created", "response": created.clone()}),
        json!({"type": "response.in_progress", "response": created}),
        json!({"type": "response.output_item.added", "output_index": 0, "item": {"id": "rs_1", "type": "reasoning", "content": [], "encrypted_content": "gAAAA-1", "summary": []}}),
    ];
    for (j, part) in out[0]["summary"].as_array().unwrap().iter().enumerate() {
        let text = part["text"].as_str().unwrap();
        let (a, b) = text.split_at(text.len() / 2);
        e.push(json!({"type": "response.reasoning_summary_part.added", "item_id": "rs_1", "output_index": 0, "summary_index": j, "part": {"type": "summary_text", "text": ""}}));
        for d in [a, b] {
            e.push(json!({"type": "response.reasoning_summary_text.delta", "item_id": "rs_1", "output_index": 0, "summary_index": j, "delta": d}));
        }
        e.push(json!({"type": "response.reasoning_summary_text.done", "item_id": "rs_1", "output_index": 0, "summary_index": j, "text": text}));
        e.push(json!({"type": "response.reasoning_summary_part.done", "item_id": "rs_1", "output_index": 0, "summary_index": j, "part": part}));
    }
    e.push(json!({"type": "response.output_item.done", "output_index": 0, "item": out[0]}));
    e.push(json!({"type": "response.output_item.added", "output_index": 1, "item": {"id": "ws_1", "type": "web_search_call", "status": "in_progress"}}));
    for s in ["in_progress", "searching", "completed"] {
        e.push(json!({"type": format!("response.web_search_call.{s}"), "item_id": "ws_1", "output_index": 1}));
    }
    e.push(json!({"type": "response.output_item.done", "output_index": 1, "item": out[1]}));
    e.push(json!({"type": "response.output_item.added", "output_index": 2, "item": {"id": "ci_1", "type": "code_interpreter_call", "status": "in_progress", "code": "", "container_id": "cntr_1", "outputs": []}}));
    e.push(json!({"type": "response.code_interpreter_call.in_progress", "item_id": "ci_1", "output_index": 2}));
    for d in ["import numpy", " as np\n", "plt.show()"] {
        e.push(json!({"type": "response.code_interpreter_call_code.delta", "item_id": "ci_1", "output_index": 2, "delta": d}));
    }
    e.push(json!({"type": "response.code_interpreter_call_code.done", "item_id": "ci_1", "output_index": 2, "code": out[2]["code"]}));
    e.push(json!({"type": "response.code_interpreter_call.interpreting", "item_id": "ci_1", "output_index": 2}));
    e.push(json!({"type": "response.code_interpreter_call.completed", "item_id": "ci_1", "output_index": 2}));
    e.push(json!({"type": "response.output_item.done", "output_index": 2, "item": out[2]}));
    e.push(json!({"type": "response.output_item.added", "output_index": 3, "item": {"id": "rs_2", "type": "reasoning", "content": [], "encrypted_content": "gAAAA-2", "summary": []}}));
    e.push(json!({"type": "response.output_item.done", "output_index": 3, "item": out[3]}));
    e.push(json!({"type": "response.output_item.added", "output_index": 4, "item": {"id": "msg_1", "type": "message", "status": "in_progress", "content": [], "phase": "final_answer", "role": "assistant"}}));
    e.push(json!({"type": "response.content_part.added", "item_id": "msg_1", "output_index": 4, "content_index": 0, "part": {"type": "output_text", "annotations": [], "logprobs": [], "text": ""}}));
    for d in [
        "Rust 1.100.0 — released November 12",
        ", 2026. ",
        "([doc.rust-lang.org](https://doc.rust-lang.org/releases.html))",
    ] {
        e.push(json!({"type": "response.output_text.delta", "item_id": "msg_1", "output_index": 4, "content_index": 0, "delta": d, "logprobs": []}));
    }
    let text = &out[4]["content"][0];
    e.push(json!({"type": "response.output_text.annotation.added", "item_id": "msg_1", "output_index": 4, "content_index": 0, "annotation_index": 0, "annotation": text["annotations"][0]}));
    e.push(json!({"type": "response.output_text.done", "item_id": "msg_1", "output_index": 4, "content_index": 0, "text": text["text"], "logprobs": []}));
    e.push(json!({"type": "response.content_part.done", "item_id": "msg_1", "output_index": 4, "content_index": 0, "part": text}));
    e.push(json!({"type": "response.output_item.done", "output_index": 4, "item": out[4]}));
    e.push(json!({"type": "response.completed", "response": r}));
    e
}

fn price(model: &str, r: &Value, u: &omni_core::Usage) -> Option<f64> {
    estimate_cost(model, r, u)
}

/// Feed `events` to a parser: its events, and the parser.
fn parse(events: &[Value]) -> (Vec<StreamEvent>, Parser) {
    let mut p = Parser::new(Platform::OpenAi, "gpt-6.1-sol", price, error_from_json);
    let mut out = Vec::new();
    for e in events {
        out.extend(p.event(e).unwrap());
    }
    (out, p)
}

#[test]
fn a_stream_cut_anywhere_and_recovered_gives_the_uncut_answer() {
    let live = live();
    let (all, whole) = parse(&live);
    let uncut = accumulate(&all);
    assert!(whole.finished());
    // What it is: summaries, the search, the code and its plot, the answer with its source.
    let kinds: Vec<String> = uncut
        .parts()
        .iter()
        .map(|p| match p {
            Part::Reasoning(r) if r.kind == ReasoningKind::Summary => "summary".into(),
            Part::Reasoning(_) => "opaque".into(),
            Part::ToolCall(t) => format!("{} {}", t.name, t.status.clone().unwrap_or_default()),
            Part::Image { .. } => "image".into(),
            Part::Text { citations, .. } => format!("text ({} source)", citations.len()),
            other => format!("{other:?}"),
        })
        .collect();
    assert_eq!(
        kinds,
        [
            "summary",
            "summary",
            "web_search completed",
            "code_interpreter completed",
            "image",
            "opaque",
            "text (1 source)"
        ]
    );
    // The stored copy's own event sequence parses to the same answer.
    let (replayed, p2) = parse(&recover::events(&finished()));
    assert_eq!(
        serde_json::to_value(accumulate(&replayed).parts()).unwrap(),
        serde_json::to_value(uncut.parts()).unwrap()
    );
    assert_eq!(p2.native_payload(), whole.native_payload());
    // Cut after any event: what came, then the restart and the stored copy.
    for cut in 1..live.len() - 1 {
        let (mut evs, _) = parse(&live[..cut]);
        evs.push(StreamEvent::Restart);
        evs.extend(replayed.iter().cloned());
        let acc = accumulate(&evs);
        assert_eq!(
            serde_json::to_value(acc.parts()).unwrap(),
            serde_json::to_value(uncut.parts()).unwrap(),
            "cut after event {cut}"
        );
        assert_eq!(acc.usage, uncut.usage, "cut after event {cut}");
        assert_eq!(acc.finish_reason.as_deref(), Some("stop"));
    }
}

#[tokio::test]
async fn a_broken_stream_is_recovered_from_the_stored_response() {
    let (base, log) = serve(|method, path, n| match (method, path) {
        ("POST", "/v1/responses") => Reply::Cut(live()[..8].to_vec()),
        // Still generating (404), then done.
        ("GET", p) if p.starts_with("/v1/responses/resp_rec?") && n == 0 => Reply::Full(
            404,
            "application/json",
            br#"{"error":{"message":"Response with id 'resp_rec' not found.","type":"invalid_request_error","param":null,"code":null}}"#.to_vec(),
        ),
        ("GET", p) if p.starts_with("/v1/responses/resp_rec?") => {
            Reply::Full(200, "application/json", finished().to_string().into_bytes())
        }
        _ => Reply::Full(500, "text/plain", b"unexpected".to_vec()),
    })
    .await;
    let (r, outcome, evs) = run(&base, &[flag("s")]).await;
    let native = r.unwrap();
    assert_eq!(native["output"], finished()["output"]);
    assert_eq!(native["id"], "resp_rec");
    assert_eq!(outcome.upstream_id.as_deref(), Some("resp_rec"));
    // The broken call's output is replaced by the stored copy, said in a notice.
    let restart = evs.iter().position(|e| matches!(e, StreamEvent::Restart)).expect("a restart");
    assert!(evs[..restart].iter().any(|e| matches!(e, StreamEvent::Reasoning { .. })), "the live part came first");
    assert!(evs.iter().any(|e| matches!(e, StreamEvent::Notice(n)
        if n.starts_with("ℹ️ the stream broke after 0s (stream read error") && n.ends_with("the answer is OpenAI's stored copy of the response"))));
    let acc = accumulate(&evs);
    let (all, _) = parse(&live());
    assert_eq!(serde_json::to_value(acc.parts()).unwrap(), serde_json::to_value(accumulate(&all).parts()).unwrap());
    let u = acc.usage.unwrap();
    assert_eq!((u.input_tokens, u.output_tokens), (Some(17334), Some(805)));
    assert!(u.cost_usd.is_some());
    // Polled with the request's `include` (the SDKs' `include[]=` form), with the key.
    let log = log.lock().unwrap().clone();
    let include = "include%5B%5D=reasoning.encrypted_content&include%5B%5D=web_search_call.action.sources&include%5B%5D=web_search_call.results";
    assert_eq!(
        log.iter().map(|(l, _)| l.as_str()).collect::<Vec<_>>(),
        [
            "POST /v1/responses",
            &format!("GET /v1/responses/resp_rec?{include}"),
            &format!("GET /v1/responses/resp_rec?{include}")
        ]
    );
    assert!(log.iter().all(|(_, auth)| auth.as_deref() == Some("Bearer sk-test")));
}

#[tokio::test]
async fn a_stored_copy_that_never_comes_leaves_the_original_error() {
    let (base, log) = serve(|method, _, _| match method {
        "POST" => Reply::Cut(live()[..5].to_vec()),
        _ => Reply::Full(
            404,
            "application/json",
            br#"{"error":{"message":"not found","type":"invalid_request_error"}}"#.to_vec(),
        ),
    })
    .await;
    let (r, _, evs) = run(&base, &[]).await;
    let e = r.unwrap_err();
    assert_eq!((e.kind, e.retry, e.partial_output), (ErrorKind::Network, Retry::Backoff, true));
    assert!(e.message.starts_with("stream read error"), "{}", e.message);
    assert!(!evs.iter().any(|e| matches!(e, StreamEvent::Restart)));
    let gets = log.lock().unwrap().iter().filter(|(l, _)| l.starts_with("GET")).count();
    assert!(gets >= 3, "polled until the window ended ({gets})");
}

#[tokio::test]
async fn only_streams_that_broke_after_the_response_started_are_recovered() {
    // Cut before `response.created`: no id to look up.
    let (base, log) = serve(|_, _, _| Reply::Cut(vec![])).await;
    let e = run(&base, &[]).await.0.unwrap_err();
    assert_eq!(e.kind, ErrorKind::Network);
    assert_eq!(log.lock().unwrap().len(), 1, "no polling");
    // An error OpenAI reported (in the stream, or over HTTP) is the answer.
    let failed = json!({"type": "response.failed", "response": {"id": "resp_rec", "status": "failed",
        "error": {"code": "server_error", "message": "The model failed to generate a response."}}});
    let mut events = live()[..3].to_vec();
    events.push(failed);
    let body = sse(&events);
    let (base, log) = serve(move |_, _, _| Reply::Full(200, "text/event-stream", body.clone())).await;
    let e = run(&base, &[]).await.0.unwrap_err();
    assert_eq!((e.kind, e.error_type.as_deref()), (ErrorKind::InBand, Some("server_error")));
    assert_eq!(log.lock().unwrap().len(), 1, "no polling");
    let (base, log) = serve(|_, _, _| {
        Reply::Full(500, "application/json", br#"{"error":{"message":"boom","type":"server_error"}}"#.to_vec())
    })
    .await;
    assert_eq!(run(&base, &[]).await.0.unwrap_err().kind, ErrorKind::Http);
    assert_eq!(log.lock().unwrap().len(), 1, "no polling");
}

#[tokio::test]
async fn a_stored_copy_that_failed_or_was_cancelled_is_an_error() {
    // Failed upstream: its output so far and its error, as a stream would have ended.
    let (base, _) = serve(|method, _, _| match method {
        "POST" => Reply::Cut(live()[..8].to_vec()),
        _ => {
            let mut r = finished();
            r["status"] = json!("failed");
            r["error"] = json!({"code": "server_error", "message": "The model failed to generate a response."});
            r["output"] = json!([r["output"][0].clone()]);
            Reply::Full(200, "application/json", r.to_string().into_bytes())
        }
    })
    .await;
    let (r, _, evs) = run(&base, &[]).await;
    let e = r.unwrap_err();
    assert_eq!((e.kind, e.retry, e.error_type.as_deref()), (ErrorKind::InBand, Retry::Backoff, Some("server_error")));
    assert!(evs.iter().any(|e| matches!(e, StreamEvent::Restart)));
    assert!(matches!(evs.last(), Some(StreamEvent::Usage(u)) if u.input_tokens == Some(17334)), "billed");
    // Cancelled: nothing to show, the original error (retried), its usage billed.
    let (base, _) = serve(|method, _, _| match method {
        "POST" => Reply::Cut(live()[..8].to_vec()),
        _ => {
            let mut r = finished();
            r["status"] = json!("cancelled");
            Reply::Full(200, "application/json", r.to_string().into_bytes())
        }
    })
    .await;
    let (r, _, evs) = run(&base, &[]).await;
    let e = r.unwrap_err();
    assert_eq!((e.kind, e.retry), (ErrorKind::Network, Retry::Backoff));
    assert!(e.message.ends_with("(OpenAI's stored copy of the response is cancelled)"), "{}", e.message);
    assert!(!evs.iter().any(|e| matches!(e, StreamEvent::Restart)));
    assert!(matches!(evs.last(), Some(StreamEvent::Usage(_))));
}

/// The answer of a code run that wrote files (turn 351's shape): the plot also returned inline,
/// a CSV cited in the middle of the text, and a file the container no longer has.
fn with_files() -> Vec<Value> {
    let png = format!("data:image/png;base64,{}", crate::provider::b64(PNG));
    let code = json!({"id": "ci_1", "type": "code_interpreter_call", "status": "completed", "code": "plt.show()",
        "container_id": "cntr_1", "outputs": [{"type": "image", "url": png}]});
    let cite = |file: &str, start: usize, end: usize| {
        json!({"type": "container_file_citation", "container_id": "cntr_1", "file_id": file,
            "filename": format!("{file}.png"), "start_index": start, "end_index": end})
    };
    let text = "Here is [the data](sandbox:/mnt/data/data.csv). Done.";
    let annotations = [cite("cfile_csv", 8, 46), cite("cfile_png", 0, 0), cite("cfile_gone", 0, 0)];
    let message = json!({"id": "msg_1", "type": "message", "status": "completed", "role": "assistant", "phase": "final_answer",
        "content": [{"type": "output_text", "text": text, "annotations": annotations, "logprobs": []}]});
    let at = |ty: &str| json!({"type": ty, "item_id": "msg_1", "output_index": 1, "content_index": 0});
    let delta = |d: &str| {
        let mut v = at("response.output_text.delta");
        v["delta"] = json!(d);
        v
    };
    let annotation = |n: usize| {
        let mut v = at("response.output_text.annotation.added");
        v["annotation_index"] = json!(n);
        v["annotation"] = annotations[n].clone();
        v
    };
    vec![
        json!({"type": "response.created", "response": {"id": "resp_f", "status": "in_progress", "model": "gpt-6.1-sol", "output": []}}),
        json!({"type": "response.output_item.added", "output_index": 0, "item": {"id": "ci_1", "type": "code_interpreter_call", "status": "in_progress", "code": "", "container_id": "cntr_1", "outputs": []}}),
        json!({"type": "response.output_item.done", "output_index": 0, "item": code}),
        json!({"type": "response.output_item.added", "output_index": 1, "item": {"id": "msg_1", "type": "message", "status": "in_progress", "content": [], "role": "assistant"}}),
        delta("Here is [the data](sandbox:/mnt/data/data.csv)"),
        annotation(0),
        delta(". Done."),
        annotation(1),
        annotation(2),
        json!({"type": "response.output_item.done", "output_index": 1, "item": message}),
        json!({"type": "response.completed", "response": {"id": "resp_f", "status": "completed", "service_tier": "default",
            "output": [code, message], "usage": {"input_tokens": 1809, "output_tokens": 255, "output_tokens_details": {"reasoning_tokens": 200}, "total_tokens": 2064}}}),
    ]
}

fn container(path: &str) -> Reply {
    match path {
        "/v1/containers/cntr_1/files/cfile_csv" => Reply::Full(
            200,
            "application/json",
            json!({"id": "cfile_csv", "object": "container.file", "bytes": 8, "container_id": "cntr_1",
                "path": "/mnt/data/data.csv", "source": "assistant"})
            .to_string()
            .into_bytes(),
        ),
        "/v1/containers/cntr_1/files/cfile_csv/content" => {
            Reply::Full(200, "application/octet-stream", b"a,b\n1,2\n".to_vec())
        }
        "/v1/containers/cntr_1/files/cfile_png" => Reply::Full(
            200,
            "application/json",
            json!({"id": "cfile_png", "path": "/mnt/data/cfile_png.png"}).to_string().into_bytes(),
        ),
        "/v1/containers/cntr_1/files/cfile_png/content" => Reply::Full(200, "image/png", PNG.to_vec()),
        _ => Reply::Full(
            404,
            "application/json",
            br#"{"error":{"message":"Container is expired.","type":"invalid_request_error","param":null,"code":"container_expired"}}"#.to_vec(),
        ),
    }
}

#[tokio::test]
async fn container_files_are_shown_where_cited() {
    let body = sse(&with_files());
    let (base, log) = serve(move |method, path, _| match method {
        "POST" => Reply::Full(200, "text/event-stream", body.clone()),
        _ => container(path),
    })
    .await;
    let (r, _, evs) = run(&base, &[flag("c")]).await;
    r.unwrap();
    let acc = accumulate(&evs);
    let shape: Vec<String> = acc
        .parts()
        .iter()
        .map(|p| match p {
            Part::ToolCall(t) => format!("tool {}", t.name),
            Part::Image { blob, .. } => format!("image {}", blob.size),
            Part::File { filename, blob, .. } => format!("file {} {}", filename.clone().unwrap_or_default(), blob.mime),
            Part::Text { text, .. } => format!("text {text}"),
            other => format!("{other:?}"),
        })
        .collect();
    // The plot once (inline; the container copy has the same bytes), the CSV under its real name
    // where it was cited.
    assert_eq!(
        shape,
        [
            "tool code_interpreter".to_string(),
            format!("image {}", PNG.len()),
            "text Here is [the data](sandbox:/mnt/data/data.csv)".into(),
            "file data.csv text/csv".into(),
            "text . Done.".into()
        ]
    );
    assert_eq!(acc.notices.len(), 1, "{:?}", acc.notices);
    assert!(
        acc.notices[0]
            .starts_with("📎 could not download cfile_gone.png from the code interpreter's container: HTTP 404")
    );
    assert!(acc.notices[0].contains("Container is expired."), "{}", acc.notices[0]);
    let log = log.lock().unwrap().clone();
    let fetched: Vec<&str> = log.iter().map(|(l, _)| l.as_str()).filter(|l| l.contains("/containers/")).collect();
    assert_eq!(fetched.len(), 6, "{fetched:?}");
    assert!(log.iter().all(|(_, auth)| auth.as_deref() == Some("Bearer sk-test")));
}

#[tokio::test]
async fn container_files_of_a_recovered_stream_are_fetched_once() {
    let events = with_files();
    let stored = events.last().unwrap()["response"].clone();
    let live = events[..6].to_vec();
    let (base, log) = serve(move |method, path, _| match (method, path) {
        ("POST", _) => Reply::Cut(live.clone()),
        ("GET", p) if p.starts_with("/v1/responses/resp_f") => {
            Reply::Full(200, "application/json", stored.to_string().into_bytes())
        }
        _ => container(path),
    })
    .await;
    let (r, _, evs) = run(&base, &[flag("c")]).await;
    r.unwrap();
    let acc = accumulate(&evs);
    let files: Vec<&Part> = acc.parts().iter().filter(|p| matches!(p, Part::File { .. })).collect();
    assert_eq!(files.len(), 1);
    assert_eq!(acc.parts().iter().filter(|p| matches!(p, Part::Image { .. })).count(), 1);
    // The CSV cited before the break was not fetched again for the stored copy.
    let csv = log.lock().unwrap().iter().filter(|(l, _)| l.ends_with("/cfile_csv/content")).count();
    assert_eq!(csv, 1);
}

#[test]
fn withheld_reasoning_and_cache_misses_are_noted() {
    let progress =
        |t: Value| json!({"type": "response.in_progress", "response": {"id": "resp_1", "input_transformations": t}});
    // Expected on a model with only `reasoning.context: current_turn` (the request says so): nothing more.
    let turn = json!([{"id": "rs_0", "type": "reasoning", "action": "removed", "reason": "turn_boundary"}]);
    let mut old = Parser::new(Platform::OpenAi, "gpt-5.2", price, error_from_json);
    assert!(old.event(&progress(turn.clone())).unwrap().iter().all(|e| !matches!(e, StreamEvent::Notice(_))));
    // Not expected where the bot asks for `all_turns`: said.
    let mut sol = Parser::new(Platform::OpenAi, "gpt-6.1-sol", price, error_from_json);
    assert!(sol.event(&progress(turn)).unwrap().iter().any(|e| matches!(e, StreamEvent::Notice(n)
        if n == "ℹ️ OpenAI withheld 1 earlier reasoning item from the model: turn_boundary")));
    let mut p = Parser::new(Platform::OpenAi, "gpt-5.2", price, error_from_json);
    let mixed = json!([
        {"id": "rs_1", "type": "reasoning", "action": "removed", "reason": "prefix_mismatch"},
        {"id": "rs_2", "type": "reasoning", "action": "removed", "reason": "turn_boundary"},
        {"id": "rs_3", "type": "reasoning", "action": "removed", "reason": "prefix_mismatch"},
        {"id": "rs_4", "type": "reasoning", "action": "removed", "reason": "end_user_mismatch"}
    ]);
    let evs = p.event(&progress(mixed.clone())).unwrap();
    assert!(matches!(&evs[..], [StreamEvent::Meta { .. }, StreamEvent::Notice(n)]
        if n == "ℹ️ OpenAI withheld 3 earlier reasoning items from the model: prefix_mismatch ×2, end_user_mismatch"));
    // Said once: the completed response repeats it.
    let completed = |t: &Value, d: Value| {
        json!({"type": "response.completed", "response": {"id": "resp_1", "status": "completed", "output": [],
            "input_transformations": t, "prompt_cache_diagnostics": d}})
    };
    let notices = |p: &mut Parser, v: Value| -> Vec<String> {
        p.event(&v)
            .unwrap()
            .into_iter()
            .filter_map(|e| if let StreamEvent::Notice(n) = e { Some(n) } else { None })
            .collect()
    };
    assert!(notices(&mut p, completed(&mixed, json!({"type": "cache_hit"}))).is_empty());
    let fresh = || Parser::new(Platform::OpenAi, "gpt-6.1-sol", price, error_from_json);
    let none = json!([]);
    let miss = json!({"type": "cache_miss", "reason": "input_changed", "cache_missed_tokens": 5629, "comparison_reusable_tokens": 5629});
    assert_eq!(
        notices(&mut fresh(), completed(&none, miss)),
        [
            "ℹ️ prompt cache miss: the earlier conversation sent differs from the previous turn's (input_changed, 5629 of 5629 reusable tokens missed)"
        ]
    );
    let tools = json!({"type": "cache_miss", "reason": "tools_changed", "cache_missed_tokens": 900});
    assert_eq!(
        notices(&mut fresh(), completed(&none, tools)),
        ["ℹ️ prompt cache miss: the tools differ from the previous turn's (tools_changed, 900 tokens missed)"]
    );
    // Other reasons and outcomes stay in the response (/info).
    for d in [
        json!({"type": "cache_miss", "reason": "model_changed", "cache_missed_tokens": 10}),
        json!({"type": "comparison_response_not_found"}),
        json!({"type": "unavailable"}),
    ] {
        assert!(notices(&mut fresh(), completed(&none, d)).is_empty());
    }
    // Reported only in the completed response: said there.
    assert_eq!(notices(&mut fresh(), completed(&mixed, Value::Null)).len(), 1);
}

#[tokio::test]
async fn the_previous_turn_is_the_prompt_cache_comparison() {
    let o = OpenAi::new(cfg("https://api.openai.com/v1")).unwrap();
    let answer = |model: &str, payload: Value| {
        let mut t = Turn::assistant_text("pong");
        t.native = Some(omni_core::content::NativeOutput {
            dialect: DIALECT.into(),
            model: model.into(),
            payload,
            context_tokens: None,
        });
        t
    };
    let output = json!([{"type": "message", "id": "msg_1", "role": "assistant", "status": "completed",
        "content": [{"type": "output_text", "text": "pong", "annotations": []}]}]);
    let history = |a: Turn| vec![Turn::user_text("ping"), a, Turn::user_text("again")];
    let with_id = json!({"output": output, "id": "resp_prev"});
    let p = prepared(&o, "gpt-6.1-sol", &[], &history(answer("gpt-6.1-sol", with_id.clone()))).await;
    assert_eq!(p.body["prompt_cache_options"], json!({"comparison_response_id": "resp_prev"}));
    assert!(
        p.applied.iter().any(|a| a.patch == json!({"prompt_cache_options": {"comparison_response_id": "resp_prev"}}))
    );
    assert_eq!(p.body["input"][1], output[0], "replayed as before");
    // Only for the models with diagnostics (GPT-5.6 and later), only after a native turn with its id.
    let p = prepared(&o, "gpt-5.4-nano", &[], &history(answer("gpt-5.4-nano", with_id.clone()))).await;
    assert!(p.body.get("prompt_cache_options").is_none());
    let p = prepared(&o, "gpt-6.1-sol", &[], &history(answer("gpt-6-luna", with_id))).await;
    assert!(p.body.get("prompt_cache_options").is_none());
    let p = prepared(&o, "gpt-6.1-sol", &[], &history(answer("gpt-6.1-sol", json!({"output": output})))).await;
    assert!(p.body.get("prompt_cache_options").is_none());
}

#[tokio::test]
async fn media_of_other_answers_cannot_be_sent() {
    let o = OpenAi::new(cfg("https://api.openai.com/v1")).unwrap();
    let png = BlobRef::for_bytes(b"\x89PNG-gen", "image/png");
    let draft = BlobRef::for_bytes(b"\x89PNG-draft", "image/png");
    let video = BlobRef::for_bytes(b"....ftypmp42", "video/mp4");
    let gemini = Turn {
        role: omni_core::Role::Assistant,
        origin: omni_core::Origin::Live,
        parts: vec![
            Part::Image { blob: draft, filename: None, generated: true, dims: None, thought: true },
            Part::text("Here it is."),
            Part::Image { blob: png.clone(), filename: None, generated: true, dims: None, thought: false },
            Part::Image { blob: png, filename: None, generated: true, dims: None, thought: false },
            Part::File { blob: video, filename: Some("v.mp4".into()), generated: true },
        ],
        native: None,
    };
    let history = vec![Turn::user_text("draw"), gemini.clone(), Turn::user_text("now blue")];
    let input = CallInput {
        model: "gpt-6.1-sol",
        options: &[],
        history: &history,
        session_id: Some("tg-1".into()),
        user: None,
        known_context: None,
        max_tokens_override: None,
    };
    // What the API cannot take refuses the request; the draft made while thinking is not counted.
    let e = o.prepare(&input, &[], &MemBlobs(vec![])).await.unwrap_err();
    assert_eq!(
        e.message,
        "2 images and 1 video from earlier answers cannot be sent: OpenAI shows the model only the text of other models' answers; send it the file as an attachment"
    );
    // A draft alone is reasoning, which never goes to another model: the answer goes as its text.
    let drafted = Turn { parts: gemini.parts[..2].to_vec(), ..gemini };
    let p = prepared(&o, "gpt-6.1-sol", &[], &[Turn::user_text("draw"), drafted, Turn::user_text("now blue")]).await;
    assert_eq!(p.body["input"][1], json!({"type": "message", "role": "assistant", "content": "Here it is."}));
    assert!(p.warnings.is_empty(), "{:?}", p.warnings);
}

#[test]
fn stored_images_refusals_and_incomplete_answers_replay_as_streamed() {
    let jpeg = crate::provider::b64(b"\xff\xd8\xff\xe0-otter");
    let r = json!({"id": "resp_i", "object": "response", "status": "incomplete", "model": "gpt-6.1-sol",
        "incomplete_details": {"reason": "max_output_tokens"},
        "output": [
            {"id": "ig_1", "type": "image_generation_call", "status": "completed", "action": "generate",
             "revised_prompt": "An otter", "size": "1024x1024", "output_format": "jpeg", "result": jpeg},
            {"id": "msg_1", "type": "message", "status": "incomplete", "role": "assistant",
             "content": [{"type": "refusal", "refusal": "I can't help with that."}]}
        ],
        "usage": {"input_tokens": 10, "output_tokens": 5, "output_tokens_details": {"reasoning_tokens": 0}, "total_tokens": 15}});
    let events = recover::events(&r);
    let types: Vec<&str> = events.iter().map(|e| e["type"].as_str().unwrap()).collect();
    assert_eq!(
        types,
        [
            "response.created",
            "response.output_item.added",
            "response.image_generation_call.in_progress",
            "response.image_generation_call.generating",
            "response.image_generation_call.completed",
            "response.output_item.done",
            "response.output_item.added",
            "response.content_part.added",
            "response.refusal.delta",
            "response.refusal.done",
            "response.content_part.done",
            "response.output_item.done",
            "response.incomplete"
        ]
    );
    assert!(events.iter().enumerate().all(|(i, e)| e["sequence_number"] == i));
    let (evs, p) = parse(&events);
    let acc = accumulate(&evs);
    assert!(
        matches!(&acc.parts()[0], Part::ToolCall(t) if t.name == "image_generation" && t.input == Some(json!("An otter")))
    );
    assert!(matches!(&acc.parts()[1], Part::Image { blob, .. } if blob.mime == "image/jpeg"));
    assert_eq!(acc.parts()[2], Part::Refusal { text: "I can't help with that.".into() });
    assert_eq!(
        (acc.finish_reason.as_deref(), acc.native_finish_reason.as_deref()),
        (Some("length"), Some("max_output_tokens"))
    );
    assert_eq!(p.native_payload()["output"], r["output"]);
}
