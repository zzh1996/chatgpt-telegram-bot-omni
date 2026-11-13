//! Moonshot provider tests: request building, history replay, stream parsing
//! (synthetic events written per platform.kimi.com/docs), cost, errors.

use std::collections::HashMap;
use std::time::Duration;

use omni_core::content::{NativeOutput, Part, ReasoningKind};
use omni_core::trigger::OptionToken;
use omni_core::{BlobRef, Origin, Turn};
use serde_json::{Value, json};

use super::*;
use crate::chatstream::EventParser as _;

struct Blobs(HashMap<String, Vec<u8>>);

impl Blobs {
    fn new(items: &[&[u8]]) -> (Self, Vec<BlobRef>) {
        let mut m = HashMap::new();
        let mut refs = Vec::new();
        for b in items {
            let r = BlobRef::for_bytes(b, "application/octet-stream");
            m.insert(r.sha256.clone(), b.to_vec());
            refs.push(r);
        }
        (Self(m), refs)
    }
}

impl BlobLoader for Blobs {
    fn load<'a>(&'a self, r: &'a BlobRef) -> crate::BoxFuture<'a, anyhow::Result<Vec<u8>>> {
        Box::pin(async move { self.0.get(&r.sha256).cloned().ok_or_else(|| anyhow::anyhow!("missing blob")) })
    }
}

fn moonshot(cap: Option<u64>) -> Moonshot {
    Moonshot::new(DirectConfig {
        api_key: "k".into(),
        base_url: "https://api.moonshot.cn/v1".into(),
        connect_timeout: Duration::from_secs(1),
        idle_timeout: Duration::from_secs(1),
        max_tokens_cap: cap,
        cny_per_usd: 7.0,
        long_decode_budget: 393_216,
    })
    .unwrap()
}

fn flag(f: &str) -> OptionToken {
    OptionToken::Flag(f.into())
}

fn kv(k: &str, v: &str) -> OptionToken {
    OptionToken::KeyValue(k.into(), v.into())
}

fn input<'a>(model: &'a str, options: &'a [OptionToken], history: &'a [Turn]) -> CallInput<'a> {
    CallInput { model, options, history, session_id: None, user: None, known_context: None, max_tokens_override: None }
}

async fn prepare(model: &str, options: &[OptionToken], history: &[Turn]) -> Result<Prepared, LlmError> {
    let (blobs, _) = Blobs::new(&[]);
    moonshot(None).prepare(&input(model, options, history), &[], &blobs).await
}

async fn prepare_err(model: &str, options: &[OptionToken]) -> String {
    prepare(model, options, &[Turn::user_text("hi")]).await.unwrap_err().message
}

fn system(t: &str) -> Turn {
    Turn { role: Role::System, origin: Origin::Synthetic, parts: vec![Part::text(t)], native: None }
}

fn native(dialect: &str, model: &str, payload: Value, text: &str) -> Turn {
    Turn {
        role: Role::Assistant,
        origin: Origin::Live,
        parts: vec![Part::text(text)],
        native: Some(NativeOutput { dialect: dialect.into(), model: model.into(), payload, context_tokens: None }),
    }
}

#[test]
fn model_table() {
    assert!(model_spec("kimi-k3").unwrap().responses);
    assert!(model_spec("kimi-k3.1-pro-highspeed").unwrap().responses);
    assert!(MODELS.iter().all(|m| m.thinking == Thinking::Effort && m.context == 1_048_576), "the K3 series");
    // The K2 series retires at the end of 2026-12: not in the table (owner's rule).
    for k2 in ["kimi-k2.6", "kimi-k2.7-code", "kimi-k2.7-code-highspeed"] {
        assert!(model_spec(k2).is_none(), "{k2}");
    }
    assert!(model_spec("kimi-k2.5").is_none(), "retired (404 since 2026-08)");
    // Exact ids only: no prefix matching onto another model's limits and prices.
    assert!(model_spec("kimi-k3.1").is_none());
    let p = model_spec("kimi-k3.1-pro").unwrap().price;
    assert_eq!((p.input, p.cached_input, p.cache_write, p.output), (13.0, 1.3, 13.0, 68.0));
}

#[tokio::test]
async fn k3_defaults() {
    let history = vec![Turn::user_text("hi")];
    let mut inp = input("kimi-k3", &[], &history);
    inp.session_id = Some("conv-1".into());
    inp.user = Some("u-hash".into());
    let (blobs, _) = Blobs::new(&[]);
    let p = moonshot(None).prepare(&inp, &[], &blobs).await.unwrap();
    assert_eq!(p.dialect, DIALECT);
    assert_eq!(p.url, "https://api.moonshot.cn/v1/chat/completions");
    assert_eq!(p.body["reasoning_effort"], "max", "the highest effort, sent explicitly");
    assert!(p.body.get("thinking").is_none(), "K3 takes no thinking parameter");
    assert_eq!(p.body["stream"], true);
    assert_eq!(p.body["stream_options"], json!({"include_usage": true}));
    assert_eq!(p.body["prompt_cache_key"], "conv-1");
    assert_eq!(p.body["safety_identifier"], "u-hash");
    // The whole window minus the prompt and a margin: Moonshot's default (131072) would cut long answers.
    let n = p.body["max_completion_tokens"].as_u64().unwrap();
    assert!(n > 1_000_000 && n < 1_048_576, "{n}");
    assert_eq!(p.auto_max_tokens, Some(n));
    assert_eq!(p.max_tokens(), Some(n));
    assert!(p.body.get("max_tokens").is_none(), "max_tokens is deprecated");
    for k in ["temperature", "top_p", "n", "presence_penalty", "frequency_penalty", "user"] {
        assert!(p.body.get(k).is_none(), "{k} must not be sent");
    }
    assert_eq!(p.settings, vec!["effort max"]);
    assert_eq!(p.applied[0].source, "default");
    assert_eq!(p.applied[0].patch, json!({"reasoning_effort": "max"}));
    assert!(p.applied.iter().any(|a| a.source == "budget" && a.patch == json!({"max_completion_tokens": n})));
    assert!(p.warnings.is_empty());

    // A configured cap, a lower effort.
    let opts = [kv("e", "low")];
    let p = moonshot(Some(65_536)).prepare(&input("kimi-k3.1-pro", &opts, &history), &[], &blobs).await.unwrap();
    assert_eq!(p.body["reasoning_effort"], "low");
    assert_eq!(p.body["max_completion_tokens"], 65_536);
    assert_eq!(p.applied[0].source, "option");
    assert_eq!(p.settings, vec!["effort low"]);
    // The budget after a context overflow wins.
    let mut inp = input("kimi-k3", &[], &history);
    inp.max_tokens_override = Some(5_000);
    let p = moonshot(None).prepare(&inp, &[], &blobs).await.unwrap();
    assert_eq!(p.body["max_completion_tokens"], 5_000);
}

#[tokio::test]
async fn invalid_options_are_rejected() {
    let e = prepare_err("kimi-k3", &[kv("e", "xhigh")]).await;
    assert!(e.contains("levels: max, high, low"), "{e}");
    let e = prepare_err("kimi-k3", &[kv("e", "none")]).await;
    assert!(e.contains("cannot turn reasoning off"), "{e}");
    let e = prepare_err("kimi-k3", &[kv("budget", "1000")]).await;
    assert!(e.contains("no reasoning token budget") && e.contains("e=max|high|low"), "{e}");
    let e = prepare_err("kimi-k3", &[flag("c")]).await;
    assert_eq!(e, "`kimi-k3` has no code execution (`+c`)");
    let e = prepare_err("kimi-k3", &[flag("u")]).await;
    assert!(e.contains("no URL fetching"), "{e}");
    let e = prepare_err("kimi-k3", &[flag("m")]).await;
    assert!(e.contains("no Google Maps"), "{e}");
    let e = prepare_err("kimi-k3", &[kv("tier", "priority")]).await;
    assert!(e.contains("no service tiers"), "{e}");
    let e = prepare_err("kimi-k3", &[kv("p", "moonshotai")]).await;
    assert!(e.contains("does not apply to Moonshot"), "{e}");
    let e = prepare_err("kimi-k3", &[kv("v", "high")]).await;
    assert!(e.contains("no verbosity"), "{e}");
    let e = prepare_err("kimi-k3", &[kv("size", "4K")]).await;
    assert!(e.contains("no image size"), "{e}");
    assert!(prepare("kimi-k3", &[flag("q")], &[Turn::user_text("hi")]).await.is_err());
    // tier=default changes nothing.
    assert!(prepare("kimi-k3", &[kv("tier", "default")], &[Turn::user_text("hi")]).await.is_ok());
}

#[tokio::test]
async fn unknown_models() {
    // Newer and retiring ids alike: their price is unknown to the bot, so they are not supported.
    for id in ["kimi-k9", "kimi-k2.6", "kimi-k2.7-code"] {
        for opts in [&[][..], &[kv("e", "max")]] {
            assert_eq!(
                prepare_err(id, opts).await,
                format!(
                    "`{id}` is not in the bot's Moonshot model table (its price is unknown), so it is not supported"
                )
            );
        }
    }
}

#[tokio::test]
async fn web_search_uses_the_responses_api() {
    let h = [system("Be brief."), Turn::user_text("news?")];
    let opts = [flag("s")];
    let mut inp = input("kimi-k3", &opts, &h);
    inp.session_id = Some("conv-1".into());
    let (blobs, _) = Blobs::new(&[]);
    let p = moonshot(Some(100_000)).prepare(&inp, &[], &blobs).await.unwrap();
    assert_eq!(p.dialect, RESPONSES_DIALECT);
    assert_eq!(p.url, "https://api.moonshot.cn/v1/responses");
    assert_eq!(p.body["tools"], json!([{"type": "web_search"}]));
    assert_eq!(p.body["include"], json!(["web_search_call.action.sources"]));
    assert_eq!(p.body["reasoning"], json!({"effort": "max"}));
    assert_eq!(p.body["max_output_tokens"], 100_000);
    assert_eq!(p.max_tokens(), Some(100_000));
    assert_eq!(p.body["prompt_cache_key"], "conv-1");
    assert!(p.body.get("messages").is_none() && p.body.get("stream_options").is_none());
    assert!(p.body.get("store").is_none(), "not a request field on Moonshot (fixed false)");
    assert_eq!(
        p.body["input"],
        json!([
            {"type": "message", "role": "developer", "content": "Be brief."},
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "news?"}]},
        ])
    );
    assert_eq!(p.settings, vec!["effort max", "web search"]);
    assert!(p.notes.is_empty() && p.warnings.is_empty(), "{:?} {:?}", p.notes, p.warnings);
    assert!(p.applied.iter().any(|a| a.source == "option" && a.patch["tools"][0]["type"] == "web_search"));
}

#[tokio::test]
async fn history_replay_on_chat() {
    let chat_native = json!({"role": "assistant", "content": "A1", "reasoning_content": "R1"});
    let resp_native = json!({"output": [
        {"type": "web_search_call", "id": "ws_1", "status": "completed", "action": {"type": "search", "query": "q"}},
        {"type": "reasoning", "id": "rs_1", "summary": [{"type": "summary_text", "text": "R2"}], "encrypted_content": null},
        {"type": "message", "id": "msg_1", "role": "assistant", "status": "completed",
         "content": [{"type": "output_text", "text": "A2", "annotations": []}]},
    ]});
    let h = vec![
        Turn::user_text("Q1"),
        native(DIALECT, "kimi-k3", chat_native.clone(), "A1"),
        Turn::user_text("Q2"),
        native(RESPONSES_DIALECT, "kimi-k3", resp_native, "A2"),
        Turn::user_text("Q3"),
        native(DIALECT, "kimi-k2.6", json!({"role": "assistant", "content": "B", "reasoning_content": "x"}), "B"),
        Turn::user_text("Q4"),
    ];
    let p = prepare("kimi-k3", &[], &h).await.unwrap();
    let m = p.body["messages"].as_array().unwrap();
    assert_eq!(m[1], chat_native, "the same model's message, verbatim (reasoning_content is required on K3)");
    assert_eq!(m[3], json!({"role": "assistant", "content": "A2", "reasoning_content": "R2"}));
    assert_eq!(m[5], json!({"role": "assistant", "content": "B"}), "another model: text only");
    assert_eq!(m[6], json!({"role": "user", "content": "Q4"}));
}

#[tokio::test]
async fn history_replay_on_responses() {
    let items = json!([
        {"type": "reasoning", "id": "rs_1", "summary": [{"type": "summary_text", "text": "R2"}], "encrypted_content": null},
        {"type": "message", "id": "msg_1", "role": "assistant", "content": [{"type": "output_text", "text": "A2"}]},
    ]);
    let h = vec![
        Turn::user_text("Q1"),
        native(DIALECT, "kimi-k3", json!({"role": "assistant", "content": "A1", "reasoning_content": "R1"}), "A1"),
        Turn::user_text("Q2"),
        native(RESPONSES_DIALECT, "kimi-k3", json!({"output": items}), "A2"),
        Turn::user_text("Q3"),
    ];
    let p = prepare("kimi-k3", &[flag("s")], &h).await.unwrap();
    let input = p.body["input"].as_array().unwrap();
    assert_eq!(input[1], json!({"type": "reasoning", "content": [{"type": "reasoning_text", "text": "R1"}]}));
    assert_eq!(
        input[2],
        json!({"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "A1"}]})
    );
    assert_eq!(input[4], items[0], "own output items verbatim");
    assert_eq!(input[5], items[1]);
    assert_eq!(input.len(), 7);
}

#[tokio::test]
async fn media() {
    let (blobs, refs) = Blobs::new(&[b"\x89PNG", b"\x00\x00mp4", b"%PDF", b"fn main() {}", b"<svg/>", b"ID3"]);
    let with = |mime: &str, r: &BlobRef| BlobRef { mime: mime.into(), ..r.clone() };
    let user = |parts: Vec<Part>| Turn { role: Role::User, origin: Origin::Live, parts, native: None };
    let image =
        Part::Image { blob: with("image/png", &refs[0]), filename: None, generated: false, dims: None, thought: false };
    let video = Part::File { blob: with("video/mp4", &refs[1]), filename: Some("v.mp4".into()), generated: false };
    let h = vec![user(vec![Part::text("look"), image.clone(), video.clone()])];
    let p = moonshot(None).prepare(&input("kimi-k3", &[], &h), &[], &blobs).await.unwrap();
    let c = &p.body["messages"][0]["content"];
    assert_eq!(c[0], json!({"type": "text", "text": "look"}));
    assert_eq!(c[1], json!({"type": "image_url", "image_url": {"url": "data:image/png;base64,iVBORw=="}}));
    assert_eq!(c[2]["type"], "video_url");
    assert!(c[2]["video_url"]["url"].as_str().unwrap().starts_with("data:video/mp4;base64,"));
    // Responses (web search): the highest image detail; no video.
    let h2 = vec![user(vec![Part::text("look"), image.clone()])];
    let p = moonshot(None).prepare(&input("kimi-k3", &[flag("s")], &h2), &[], &blobs).await.unwrap();
    assert_eq!(
        p.body["input"][0]["content"][1],
        json!({"type": "input_image", "image_url": "data:image/png;base64,iVBORw==", "detail": "original"})
    );
    let e = moonshot(None).prepare(&input("kimi-k3", &[flag("s")], &h), &[], &blobs).await.unwrap_err();
    assert!(e.message.contains("takes no video on Moonshot's Responses API"), "{}", e.message);
    // A text file is inlined.
    let code = Part::File { blob: with("text/x-rust", &refs[3]), filename: Some("main.rs".into()), generated: false };
    let p = moonshot(None).prepare(&input("kimi-k3", &[], &[user(vec![code])]), &[], &blobs).await.unwrap();
    assert_eq!(p.body["messages"][0]["content"], "fn main() {}");
    assert!(p.notes.is_empty(), "{:?}", p.notes);
    // PDFs, audio, SVG, remote media, locations: rejected with the reason.
    let pdf = Part::File { blob: with("application/pdf", &refs[2]), filename: Some("a.pdf".into()), generated: false };
    let e = moonshot(None).prepare(&input("kimi-k3", &[], &[user(vec![pdf])]), &[], &blobs).await.unwrap_err();
    assert!(e.message.contains("file-extract"), "{}", e.message);
    let audio = Part::File { blob: with("audio/mpeg", &refs[5]), filename: Some("a.mp3".into()), generated: false };
    let e = moonshot(None).prepare(&input("kimi-k3", &[], &[user(vec![audio])]), &[], &blobs).await.unwrap_err();
    assert!(e.message.contains("no audio input"), "{}", e.message);
    let svg = Part::Image {
        blob: with("image/svg+xml", &refs[4]),
        filename: None,
        generated: false,
        dims: None,
        thought: false,
    };
    let e = moonshot(None).prepare(&input("kimi-k3", &[], &[user(vec![svg])]), &[], &blobs).await.unwrap_err();
    assert!(e.message.contains("SVG"), "{}", e.message);
    let yt = Part::RemoteMedia { url: "https://youtu.be/x".into(), kind: omni_core::content::RemoteMediaKind::YouTube };
    assert!(moonshot(None).prepare(&input("kimi-k3", &[], &[user(vec![yt])]), &[], &blobs).await.is_err());
    let loc = Part::Location { latitude: 1.0, longitude: 2.0, accuracy_m: None, title: None, address: None };
    let e = moonshot(None).prepare(&input("kimi-k3", &[], &[user(vec![loc])]), &[], &blobs).await.unwrap_err();
    assert!(e.message.contains("does not take a location via Moonshot"), "{}", e.message);
}

fn run_chat(chunks: &[Value]) -> (ChatParser, crate::Accumulator, Vec<StreamEvent>) {
    let mut p = ChatParser::default();
    let mut acc = crate::Accumulator::new();
    let mut all = Vec::new();
    for c in chunks {
        for e in p.event(None, c).unwrap() {
            acc.apply(e.clone());
            all.push(e);
        }
    }
    (p, acc, all)
}

/// A Chat stream as in platform.kimi.com/docs/api/chat and the streaming guide.
#[test]
fn chat_stream() {
    let id = "chatcmpl-6adb0ef5ba679679914a2719";
    let base = |choices: Value| json!({"id": id, "object": "chat.completion.chunk", "created": 1792741109, "model": "kimi-k3.1-pro", "choices": choices, "system_fingerprint": "fpv0_dd3f7976"});
    let chunks = [
        base(json!([{"index": 0, "delta": {"role": "assistant", "content": ""}, "finish_reason": null}])),
        base(json!([{"index": 0, "delta": {"reasoning_content": "The"}, "finish_reason": null}])),
        base(json!([{"index": 0, "delta": {"reasoning_content": " user"}, "finish_reason": null}])),
        base(json!([{"index": 0, "delta": {"content": "1"}, "finish_reason": null}])),
        base(json!([{"index": 0, "delta": {"content": " + 1 = 2"}, "finish_reason": null}])),
        base(
            json!([{"index": 0, "delta": {}, "finish_reason": "stop", "usage": {"prompt_tokens": 309, "completion_tokens": 101, "total_tokens": 410}}]),
        ),
        json!({"id": id, "object": "chat.completion.chunk", "created": 1792741109, "model": "kimi-k3.1-pro", "choices": [],
               "usage": {"prompt_tokens": 309, "completion_tokens": 101, "total_tokens": 410,
                         "completion_tokens_details": {"reasoning_tokens": 71}, "prompt_tokens_details": {"cache_write_tokens": 256}}}),
    ];
    let (p, acc, all) = run_chat(&chunks);
    let parts = acc.parts();
    assert!(
        matches!(&parts[0], Part::Reasoning(r) if r.kind == ReasoningKind::Text && r.text.as_deref() == Some("The user"))
    );
    assert_eq!(acc.turn().answer_text(), "1 + 1 = 2");
    assert_eq!(acc.finish_reason.as_deref(), Some("stop"));
    assert_eq!(acc.upstream_id.as_deref(), Some(id));
    assert_eq!(acc.served_model.as_deref(), Some("kimi-k3.1-pro"));
    // Two usage events (inside the finish choice, then the final chunk); the last wins.
    assert_eq!(all.iter().filter(|e| matches!(e, StreamEvent::Usage(_))).count(), 2);
    let u = acc.usage.unwrap();
    assert_eq!((u.input_tokens, u.output_tokens, u.reasoning_tokens), (Some(309), Some(101), Some(71)));
    assert_eq!(u.cache_write_tokens, Some(256));
    assert_eq!(
        p.native_payload(),
        json!({"role": "assistant", "content": "1 + 1 = 2", "reasoning_content": "The user"})
    );
    // A finish reason alone does not complete a Moonshot stream: only `data: [DONE]` does.
    assert!(!p.finished());
    assert!(p.any_output());
}

#[test]
fn chat_length_and_cached_tokens() {
    let chunks = [
        json!({"id": "c", "choices": [{"index": 0, "delta": {"content": "abc"}, "finish_reason": "length"}]}),
        json!({"id": "c", "choices": [], "usage": {"prompt_tokens": 10, "completion_tokens": 5, "cached_tokens": 4}}),
    ];
    let (_, acc, _) = run_chat(&chunks);
    assert_eq!(acc.finish_reason.as_deref(), Some("length"));
    assert_eq!(acc.usage.unwrap().cached_input_tokens, Some(4), "top-level usage.cached_tokens");
}

#[test]
fn chat_in_band_error() {
    let mut p = ChatParser::default();
    p.event(None, &json!({"id": "c", "choices": [{"index": 0, "delta": {"content": "partial"}}]})).unwrap();
    let e = p
        .event(
            None,
            &json!({"error": {"type": "content_filter", "message": "The request was rejected because it was considered high risk"}}),
        )
        .unwrap_err();
    assert_eq!(e.kind, ErrorKind::InBand);
    assert_eq!(e.retry, Retry::Never);
    assert_eq!(e.error_type.as_deref(), Some("content_filter"));
    assert!(e.partial_output);
    assert_eq!(e.details.unwrap()["error"]["type"], "content_filter", "the whole chunk, verbatim");
}

fn run_responses(events: &[Value]) -> (responses::Parser, crate::Accumulator, Vec<StreamEvent>) {
    let mut p = responses::Parser::new(model_spec("kimi-k3"), 7.0);
    let mut acc = crate::Accumulator::new();
    let mut all = Vec::new();
    for e in events {
        for ev in p.event(Some(e["type"].as_str().unwrap()), e).unwrap() {
            acc.apply(ev.clone());
            all.push(ev);
        }
    }
    (p, acc, all)
}

/// A Responses stream with a web search, per platform.kimi.com/docs/api/responses
/// and turn 282 of 2026-11-13 (the sources already complete when the search
/// item is added; `annotations` empty); `search: false`: no search ran.
fn search_stream(search: bool, annotations: Value) -> Vec<Value> {
    let sources = json!([
        {"type": "url", "url": "https://example.com/news/1", "title": "新闻标题"},
        {"type": "url", "url": "https://example.com/news/2", "title": "另一条"},
        {"type": "url", "url": "https://example.com/news/1", "title": "新闻标题"},
    ]);
    let action = json!({"type": "search", "query": "2026年9月3日 重大新闻", "sources": sources});
    let search_done = json!({"type": "web_search_call", "id": "ws_1", "status": "completed", "action": action});
    let reasoning_done = json!({"type": "reasoning", "id": "rs_1", "status": "completed",
        "summary": [{"type": "summary_text", "text": "Think."}], "encrypted_content": null});
    let message_done = json!({"type": "message", "id": "msg_1", "role": "assistant", "status": "completed",
        "content": [{"type": "output_text", "text": "News: …", "annotations": annotations, "logprobs": []}]});
    let usage = json!({"input_tokens": 1000, "input_tokens_details": {"cached_tokens": 0, "cache_write_tokens": 0},
                       "output_tokens": 200, "output_tokens_details": {"reasoning_tokens": 50}, "total_tokens": 1200});
    let mut n = 0;
    let mut ev = |mut v: Value| {
        v["sequence_number"] = json!(n);
        n += 1;
        v
    };
    let mut events = vec![
        ev(
            json!({"type": "response.created", "response": {"id": "resp_1", "model": "kimi-k3", "status": "in_progress", "output": []}}),
        ),
        ev(
            json!({"type": "response.in_progress", "response": {"id": "resp_1", "model": "kimi-k3", "status": "in_progress"}}),
        ),
    ];
    if search {
        events.extend([
            ev(json!({"type": "response.output_item.added", "output_index": 0,
                       "item": {"type": "web_search_call", "id": "ws_1", "status": "in_progress", "action": action}})),
            ev(json!({"type": "response.web_search_call.in_progress", "output_index": 0, "item_id": "ws_1"})),
            ev(json!({"type": "response.web_search_call.searching", "output_index": 0, "item_id": "ws_1"})),
            ev(json!({"type": "response.web_search_call.completed", "output_index": 0, "item_id": "ws_1"})),
            ev(json!({"type": "response.output_item.done", "output_index": 0, "item": search_done})),
        ]);
    }
    events.extend([
        ev(json!({"type": "response.output_item.added", "output_index": 1,
                   "item": {"type": "reasoning", "id": "rs_1", "status": "in_progress", "summary": []}})),
        ev(
            json!({"type": "response.reasoning_summary_part.added", "output_index": 1, "item_id": "rs_1", "summary_index": 0,
                   "part": {"type": "summary_text", "text": ""}}),
        ),
        ev(
            json!({"type": "response.reasoning_summary_text.delta", "output_index": 1, "item_id": "rs_1", "summary_index": 0, "delta": "Thi"}),
        ),
        ev(
            json!({"type": "response.reasoning_summary_text.delta", "output_index": 1, "item_id": "rs_1", "summary_index": 0, "delta": "nk."}),
        ),
        ev(
            json!({"type": "response.reasoning_summary_text.done", "output_index": 1, "item_id": "rs_1", "summary_index": 0, "text": "Think."}),
        ),
        ev(
            json!({"type": "response.reasoning_summary_part.done", "output_index": 1, "item_id": "rs_1", "summary_index": 0,
                   "part": {"type": "summary_text", "text": "Think."}}),
        ),
        ev(json!({"type": "response.output_item.done", "output_index": 1, "item": reasoning_done})),
        ev(json!({"type": "response.output_item.added", "output_index": 2,
                   "item": {"type": "message", "id": "msg_1", "role": "assistant", "status": "in_progress", "content": []}})),
        ev(json!({"type": "response.content_part.added", "output_index": 2, "item_id": "msg_1", "content_index": 0,
                   "part": {"type": "output_text", "text": "", "annotations": []}})),
        ev(
            json!({"type": "response.output_text.delta", "output_index": 2, "item_id": "msg_1", "content_index": 0, "delta": "News"}),
        ),
        ev(
            json!({"type": "response.output_text.delta", "output_index": 2, "item_id": "msg_1", "content_index": 0, "delta": ": …"}),
        ),
        ev(
            json!({"type": "response.output_text.done", "output_index": 2, "item_id": "msg_1", "content_index": 0, "text": "News: …"}),
        ),
        ev(json!({"type": "response.output_item.done", "output_index": 2, "item": message_done})),
    ]);
    let output =
        if search { json!([search_done, reasoning_done, message_done]) } else { json!([reasoning_done, message_done]) };
    events.push(ev(
        json!({"type": "response.completed", "response": {"id": "resp_1", "model": "kimi-k3", "status": "completed",
                                "output": output, "usage": usage}}),
    ));
    events
}

#[test]
fn responses_stream_with_search() {
    let (p, acc, _) = run_responses(&search_stream(true, json!([])));
    assert!(p.finished() && p.searched());
    let parts = acc.parts();
    assert_eq!(parts.len(), 3, "{parts:?}");
    match &parts[0] {
        Part::ToolCall(t) => {
            assert_eq!(t.name, "web_search");
            assert_eq!(t.status.as_deref(), Some("completed"));
            assert_eq!(t.input, Some(json!({"type": "search", "query": "2026年9月3日 重大新闻"})));
            assert_eq!(t.output.as_ref().unwrap()["sources"].as_array().unwrap().len(), 3, "verbatim");
        }
        other => panic!("{other:?}"),
    }
    assert!(
        matches!(&parts[1], Part::Reasoning(r) if r.kind == ReasoningKind::Text && r.text.as_deref() == Some("Think.")),
        "no separator added after the single reasoning part: {:?}",
        parts[1]
    );
    // Kimi's annotations are empty: the search's sources are the answer's (each URL once).
    let Part::Text { text, citations } = &parts[2] else { panic!("{parts:?}") };
    assert_eq!(text, "News: …");
    let got: Vec<_> = citations.iter().map(|c| (c.url.as_deref().unwrap(), c.title.as_deref().unwrap())).collect();
    assert_eq!(got, [("https://example.com/news/1", "新闻标题"), ("https://example.com/news/2", "另一条")]);
    assert_eq!(acc.finish_reason.as_deref(), Some("stop"));
    assert_eq!(acc.upstream_id.as_deref(), Some("resp_1"));
    let u = acc.usage.unwrap();
    assert_eq!(u.reasoning_tokens, Some(50));
    // 1000 × ¥20/M + 200 × ¥100/M + one search × ¥0.03 = ¥0.07, at 7 CNY/USD.
    assert!((u.cost_usd.unwrap() - 0.07 / 7.0).abs() < 1e-12, "{:?}", u.cost_usd);
    assert_eq!(u.cost_source, omni_core::usage::CostSource::Estimated);
    let native = p.native_payload();
    assert_eq!(native["output"].as_array().unwrap().len(), 3);
    assert_eq!(native["output"][0]["type"], "web_search_call");
    // Real `url_citation`s (should Moonshot send them) are the answer's sources instead.
    let cite = json!([{"type": "url_citation", "url": "https://example.com/news/1", "title": "新闻标题", "start_index": 0, "end_index": 4}]);
    let (_, acc, _) = run_responses(&search_stream(true, cite));
    assert!(
        matches!(acc.parts().last(), Some(Part::Text { citations, .. }) if citations.len() == 1 && citations[0].range == Some((0, 4)))
    );
}

/// No `web_search_call`: Moonshot judged that no search was needed (none is billed).
#[test]
fn responses_without_a_search() {
    let (p, acc, _) = run_responses(&search_stream(false, json!([])));
    assert!(p.finished() && !p.searched());
    assert!(acc.parts().iter().all(|p| !matches!(p, Part::ToolCall(_))));
    // 1000 × ¥20/M + 200 × ¥100/M, no search fee.
    assert!((acc.usage.unwrap().cost_usd.unwrap() - 0.04 / 7.0).abs() < 1e-12);
}

/// `+s` end to end: a notice when Moonshot ran no search, none when it did.
#[tokio::test]
async fn end_to_end_search_notice() {
    let h = [Turn::user_text("今天有什么新闻？")];
    for (search, want) in
        [(false, vec!["no web search ran (Moonshot decides from the input whether to search)"]), (true, vec![])]
    {
        let (base, server) = serve(sse(&search_stream(search, json!([])))).await;
        let m = Moonshot::new(DirectConfig { base_url: base, ..moonshot(None).cfg.clone() }).unwrap();
        let (blobs, _) = Blobs::new(&[]);
        let p = m.prepare(&input("kimi-k3", &[flag("s")], &h), &[], &blobs).await.unwrap();
        let mut evs = Vec::new();
        let (r, _) =
            m.stream(&p, &mut |e: StreamEvent| evs.push(e), &crate::NullObserver, &CancellationToken::new()).await;
        assert!(r.is_ok(), "{r:?}");
        assert_eq!(server.await.unwrap()["tools"], json!([{"type": "web_search"}]));
        assert_eq!(notices(&evs), want, "search: {search}");
    }
}

/// Media other models generated in earlier turns: assistant messages carry text
/// only (both APIs), so the request is refused.
#[tokio::test]
async fn generated_media_of_other_models_are_not_sent() {
    let (blobs, refs) = Blobs::new(&[b"\x89PNG", b"\x00\x00mp4"]);
    let with = |mime: &str, r: &BlobRef| BlobRef { mime: mime.into(), ..r.clone() };
    let earlier = Turn {
        role: Role::Assistant,
        origin: Origin::Live,
        parts: vec![
            Part::text("Here you go."),
            Part::File { blob: with("video/mp4", &refs[1]), filename: Some("v.mp4".into()), generated: true },
            Part::Image {
                blob: with("image/png", &refs[0]),
                filename: None,
                generated: true,
                dims: None,
                thought: false,
            },
        ],
        native: None,
    };
    let h = vec![Turn::user_text("make a video"), earlier.clone(), Turn::user_text("describe it")];
    for opts in [vec![], vec![flag("s")]] {
        let e = moonshot(None).prepare(&input("kimi-k3", &opts, &h), &[], &blobs).await.unwrap_err();
        assert_eq!(
            e.message,
            "1 image and 1 video generated in an earlier turn cannot be sent: this model sees only the text of other models' answers; send it the file as an attachment",
            "{opts:?}"
        );
    }
    // The answer's text alone goes as an assistant message.
    let text = Turn { parts: vec![Part::text("Here you go.")], ..earlier };
    let h = vec![Turn::user_text("make a video"), text, Turn::user_text("describe it")];
    let p = moonshot(None).prepare(&input("kimi-k3", &[], &h), &[], &blobs).await.unwrap();
    assert_eq!(p.body["messages"][1], json!({"role": "assistant", "content": "Here you go."}));
    assert!(p.warnings.is_empty(), "{:?}", p.warnings);
}

#[test]
fn responses_abnormal_ends() {
    let (p, acc, _) = run_responses(&[json!({"type": "response.incomplete", "sequence_number": 0, "response": {
        "id": "resp_2", "status": "incomplete", "incomplete_details": {"reason": "content_filter"}, "output": [],
        "usage": {"input_tokens": 10, "output_tokens": 0}}})]);
    assert_eq!(acc.finish_reason.as_deref(), Some("content_filter"));
    assert_eq!(acc.finish_details, Some(json!({"reason": "content_filter"})));
    assert!(!p.answered_without_search(), "a blocked answer: nothing to say about the search");
    let (p, acc, _) = run_responses(&[json!({"type": "response.incomplete", "sequence_number": 0, "response": {
        "status": "incomplete", "incomplete_details": {"reason": "max_output_tokens"}, "output": []}})]);
    assert_eq!(acc.finish_reason.as_deref(), Some("length"));
    assert!(p.answered_without_search(), "cut at the output limit without a search");
    // A failed response is Moonshot's error, verbatim.
    let mut p = responses::Parser::new(model_spec("kimi-k3"), 7.0);
    let failed = json!({"type": "response.failed", "sequence_number": 3, "response": {"id": "resp_3", "status": "failed",
        "error": {"code": "server_error", "message": "internal"}}});
    let e = p.event(None, &failed).unwrap_err();
    assert_eq!((e.kind, e.retry), (ErrorKind::InBand, Retry::Backoff));
    assert_eq!(e.error_type.as_deref(), Some("server_error"));
    assert_eq!(e.details, Some(failed));
    let e = p
        .event(None, &json!({"type": "error", "sequence_number": 4, "code": "content_filter", "message": "high risk"}))
        .unwrap_err();
    assert_eq!((e.retry, e.error_type.as_deref()), (Retry::Never, Some("content_filter")));
}

#[test]
fn errors() {
    let http = |status: u16, retry_after: Option<u64>, body: &str| {
        crate::chatstream::http_error(status, retry_after, body, error_from_json)
    };
    // As probed without a key (2026-11-12).
    let e = http(401, None, r#"{"error":{"message":"Incorrect API key provided","type":"incorrect_api_key_error"}}"#);
    assert_eq!((e.retry, e.status), (Retry::Never, Some(401)));
    assert_eq!(e.error_type.as_deref(), Some("incorrect_api_key_error"));
    assert_eq!(e.message, "Incorrect API key provided");
    assert_eq!(e.details.unwrap()["error"]["type"], "incorrect_api_key_error");
    let e = http(
        400,
        None,
        r#"{"error":{"type":"content_filter","message":"The request was rejected because it was considered high risk"}}"#,
    );
    assert_eq!(e.retry, Retry::Never);
    let e = http(
        429,
        Some(7),
        r#"{"error":{"type":"engine_overloaded_error","message":"The engine is currently overloaded, please try again later"}}"#,
    );
    assert_eq!(e.retry, Retry::After(7));
    let e = http(
        429,
        None,
        r#"{"error":{"type":"rate_limit_reached_error","message":"Your account request reached TPM rate limit, current:1, limit:1"}}"#,
    );
    assert_eq!(e.retry, Retry::After(20));
    let e = http(
        429,
        None,
        r#"{"error":{"type":"rate_limit_reached_error","message":"Your account request reached TPD rate limit"}}"#,
    );
    assert_eq!(e.retry, Retry::Never, "a daily limit resets tomorrow");
    let e = http(429, None, r#"{"error":{"type":"exceeded_current_quota_error","message":"balance"}}"#);
    assert_eq!(e.retry, Retry::Never);
    let e = http(404, None, r#"{"error":{"type":"resource_not_found_error","message":"model not found"}}"#);
    assert_eq!(e.retry, Retry::Never);
    let e = http(500, None, r#"{"error":{"type":"server_error","message":"x"}}"#);
    assert_eq!(e.retry, Retry::Backoff);
    let e = http(503, None, r#"{"error":{"type":"something_new","message":"x"}}"#);
    assert_eq!(e.retry, Retry::Backoff, "unknown type: by HTTP status");
    let e = http(504, None, "<html>504 Gateway Time-out</html>");
    assert_eq!(e.retry, Retry::Backoff);
    let e = http(400, None, r#"{"error":{"type":"invalid_request_error","message":"Input token length too long"}}"#);
    assert_eq!(e.retry, Retry::Never);
    assert!(!e.is_context_overflow(), "the prompt alone is too long: a smaller output budget cannot help");
}

#[test]
fn cost() {
    let u = Usage {
        input_tokens: Some(1_000_000),
        cached_input_tokens: Some(400_000),
        cache_write_tokens: Some(100_000),
        output_tokens: Some(100_000),
        ..Default::default()
    };
    // kimi-k3: 0.5M × 20 + 0.4M × 2 + 0.1M × 20 (5-min write) + 0.1M × 100 + 2 searches × 0.03 = ¥22.86
    let c = estimate_cost(model_spec("kimi-k3").unwrap(), &u, 2, 7.0).unwrap();
    assert!((c - 22.86 / 7.0).abs() < 1e-9, "{c}");
    assert!(estimate_cost(model_spec("kimi-k3").unwrap(), &Usage::default(), 0, 7.0).is_none());
    assert!(estimate_cost(model_spec("kimi-k3").unwrap(), &u, 0, 0.0).is_none(), "no exchange rate");
}

/// One HTTP exchange on a local port: the base URL and a handle yielding the JSON body sent.
async fn serve(body: String) -> (String, tokio::task::JoinHandle<Value>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let (mut buf, mut tmp) = (Vec::new(), [0u8; 8192]);
        let sent = loop {
            let n = sock.read(&mut tmp).await.unwrap();
            buf.extend_from_slice(&tmp[..n]);
            if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&buf[..pos]).to_ascii_lowercase();
                let len: usize = head
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length:").map(|v| v.trim().parse().unwrap()))
                    .unwrap_or(0);
                while buf.len() < pos + 4 + len {
                    let n = sock.read(&mut tmp).await.unwrap();
                    buf.extend_from_slice(&tmp[..n]);
                }
                break serde_json::from_slice(&buf[pos + 4..pos + 4 + len]).unwrap_or(Value::Null);
            }
        };
        let resp = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        sock.write_all(resp.as_bytes()).await.unwrap();
        let _ = sock.shutdown().await;
        sent
    });
    (base, server)
}

fn sse(events: &[Value]) -> String {
    events.iter().map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap_or("message"))).collect()
}

fn notices(evs: &[StreamEvent]) -> Vec<&str> {
    evs.iter().filter_map(|e| if let StreamEvent::Notice(n) = e { Some(n.as_str()) } else { None }).collect()
}
