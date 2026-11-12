//! Alibaba Model Studio provider tests: request building, history replay,
//! stream parsing (synthetic events written per help.aliyun.com/zh/model-studio),
//! cost, errors.

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
    fn new(items: &[Vec<u8>]) -> (Self, Vec<BlobRef>) {
        let mut m = HashMap::new();
        let mut refs = Vec::new();
        for b in items {
            let r = BlobRef::for_bytes(b, "application/octet-stream");
            m.insert(r.sha256.clone(), b.clone());
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

fn alibaba(cap: Option<u64>) -> Alibaba {
    Alibaba::new(DirectConfig {
        api_key: "k".into(),
        base_url: "https://dashscope.aliyuncs.com/compatible-mode/v1".into(),
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
    CallInput {
        model,
        options,
        history,
        session_id: Some("conv".into()),
        user: Some("u-hash".into()),
        known_context: None,
        max_tokens_override: None,
    }
}

async fn prepare(model: &str, options: &[OptionToken]) -> Result<Prepared, LlmError> {
    let (blobs, _) = Blobs::new(&[]);
    alibaba(None).prepare(&input(model, options, &[Turn::user_text("hi")]), &[], &blobs).await
}

async fn prepare_err(model: &str, options: &[OptionToken]) -> String {
    prepare(model, options).await.unwrap_err().message
}

fn user(parts: Vec<Part>) -> Turn {
    Turn { role: Role::User, origin: Origin::Live, parts, native: None }
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
    for id in ["qwen3.8-max", "qwen3.8-max-0902", "qwen3.8-max-2026-09-02", "qwen3.7-max"] {
        assert!(model_spec(id).is_some(), "{id}");
    }
    let m = model_spec("qwen3.8-max-0902").unwrap();
    assert!(m.vision && m.effort && m.search == "max");
    assert_eq!((m.context, m.max_output, m.max_cot), (1_000_000, 131_072, Some(262_144)));
    let p = m.tiers[0].price;
    assert_eq!((p.input, p.cached_input, p.output), (12.0, 1.5, 36.0));
    let q37 = model_spec("qwen3.7-max").unwrap();
    assert!(!q37.vision && !q37.effort && q37.search == "agent");
    assert_eq!(q37.tiers[0].price.cached_input, 2.4);
    assert_eq!(model_spec("qwen3.7-max-preview").unwrap().thinking, Thinking::Always);
    assert!(model_spec("qwen3.7-max-2026-06-08").unwrap().vision);
    assert_eq!(model_spec("qwen3.7-plus").unwrap().tiers.len(), 2);
    assert!(model_spec("qwen3.8").is_none());
}

#[tokio::test]
async fn qwen38_defaults() {
    let p = prepare("qwen3.8-max", &[]).await.unwrap();
    assert_eq!(p.dialect, DIALECT);
    assert_eq!(p.url, "https://dashscope.aliyuncs.com/compatible-mode/v1/chat/completions");
    assert_eq!(p.body["enable_thinking"], true);
    // Explicit: unset, Qwen3.8 thinks with a 131072 budget; xhigh means 262144.
    assert_eq!(p.body["reasoning_effort"], "xhigh");
    assert_eq!(p.body["stream"], true);
    assert_eq!(p.body["stream_options"], json!({"include_usage": true}));
    assert!(p.body.get("max_completion_tokens").is_none() && p.body.get("max_tokens").is_none());
    assert_eq!(p.auto_max_tokens, None);
    for k in ["thinking_budget", "preserve_thinking", "enable_search", "vl_high_resolution_images", "user"] {
        assert!(p.body.get(k).is_none(), "{k}");
    }
    for k in ["temperature", "top_p", "top_k", "presence_penalty", "seed", "n"] {
        assert!(p.body.get(k).is_none(), "{k}");
    }
    assert_eq!(p.settings, vec!["effort xhigh"]);
    assert_eq!(p.applied[0].source, "default");
    assert_eq!(p.applied[0].patch, json!({"enable_thinking": true, "reasoning_effort": "xhigh"}));
    assert!(p.warnings.is_empty() && p.notes.is_empty());
}

#[tokio::test]
async fn thinking_options() {
    let p = prepare("qwen3.8-max", &[kv("e", "medium")]).await.unwrap();
    assert_eq!(p.body["reasoning_effort"], "medium");
    assert_eq!(p.applied[0].source, "option");
    let p = prepare("qwen3.8-max", &[kv("e", "none")]).await.unwrap();
    assert_eq!(p.body["enable_thinking"], false);
    assert!(p.body.get("reasoning_effort").is_none());
    assert_eq!(p.settings, vec!["thinking off"]);
    let p = prepare("qwen3.8-max", &[kv("budget", "5000")]).await.unwrap();
    assert_eq!(p.body["thinking_budget"], 5000);
    assert_eq!(p.body["enable_thinking"], true);
    assert!(p.body.get("reasoning_effort").is_none(), "effort and budget together are an error");
    assert_eq!(p.settings, vec!["thinking budget 5000"]);
    let e = prepare_err("qwen3.8-max", &[kv("budget", "300000")]).await;
    assert!(e.contains("262144"), "{e}");
    // Only the native levels: the API would silently map max/high to xhigh.
    let e = prepare_err("qwen3.8-max", &[kv("e", "max")]).await;
    assert!(e.contains("levels: xhigh, medium, low"), "{e}");
    assert!(prepare("qwen3.8-max", &[kv("e", "high")]).await.is_err());
    // Qwen3.7: the switch and a budget, no levels.
    let p = prepare("qwen3.7-max", &[]).await.unwrap();
    assert_eq!(p.body["enable_thinking"], true);
    assert!(p.body.get("reasoning_effort").is_none());
    assert_eq!(p.settings, vec!["thinking on"]);
    let e = prepare_err("qwen3.7-max", &[kv("e", "low")]).await;
    assert!(e.contains("no reasoning effort levels"), "{e}");
    let p = prepare("qwen3.7-max", &[kv("budget", "8192")]).await.unwrap();
    assert_eq!((p.body["thinking_budget"].as_u64(), p.body["enable_thinking"].as_bool()), (Some(8192), Some(true)));
    // Thinking-only snapshots take no switch.
    let p = prepare("qwen3.7-max-preview", &[]).await.unwrap();
    assert!(p.body.get("enable_thinking").is_none());
    assert_eq!(p.settings, vec!["thinking (always on)"]);
    let e = prepare_err("qwen3.7-max-preview", &[kv("e", "none")]).await;
    assert!(e.contains("cannot turn reasoning off"), "{e}");
}

#[tokio::test]
async fn invalid_options_are_rejected() {
    let e = prepare_err("qwen3.8-max", &[kv("p", "alibaba")]).await;
    assert!(e.contains("does not apply to Alibaba Model Studio"), "{e}");
    let e = prepare_err("qwen3.8-max", &[kv("tier", "flex")]).await;
    assert!(e.contains("no service tiers"), "{e}");
    let e = prepare_err("qwen3.8-max", &[flag("m")]).await;
    assert!(e.contains("no Google Maps"), "{e}");
    let e = prepare_err("qwen3.8-max", &[flag("u")]).await;
    assert!(e.contains("use +su"), "{e}");
    let e = prepare_err("qwen3.8-max", &[flag("su"), kv("e", "none")]).await;
    assert!(e.contains("needs thinking mode"), "{e}");
    let e = prepare_err("qwen3.8-max", &[flag("c"), kv("e", "none")]).await;
    assert!(e.contains("needs thinking mode"), "{e}");
    let e = prepare_err("qwen3.8-max", &[flag("c"), kv("budget", "1000")]).await;
    assert!(e.contains("no thinking budget") && e.contains("e=xhigh|medium|low"), "{e}");
    let e = prepare_err("qwen3.8-max", &[kv("v", "low")]).await;
    assert!(e.contains("no verbosity"), "{e}");
    let e = prepare_err("qwen9", &[flag("s")]).await;
    assert!(e.contains("cannot be checked"), "{e}");
    let p = prepare("qwen9", &[]).await.unwrap();
    assert!(p.warnings[0].contains("not in the bot's Alibaba Model Studio model table"));
    assert!(p.body.get("enable_thinking").is_none());
}

#[tokio::test]
async fn output_budget() {
    let (blobs, _) = Blobs::new(&[]);
    let h = [Turn::user_text("hi")];
    let p = alibaba(Some(65_536)).prepare(&input("qwen3.8-max", &[], &h), &[], &blobs).await.unwrap();
    assert_eq!(p.body["max_completion_tokens"], 65_536);
    assert_eq!(p.auto_max_tokens, Some(65_536));
    assert_eq!(p.max_tokens(), Some(65_536));
    assert!(p.applied.iter().any(|a| a.source == "budget"));
    // A cap above the model's maximum changes nothing: the default is the maximum.
    let p = alibaba(Some(393_216)).prepare(&input("qwen3.8-max", &[], &h), &[], &blobs).await.unwrap();
    assert!(p.body.get("max_completion_tokens").is_none());
    let mut inp = input("qwen3.7-max", &[], &h);
    inp.max_tokens_override = Some(4_000);
    let p = alibaba(None).prepare(&inp, &[], &blobs).await.unwrap();
    assert_eq!(p.body["max_completion_tokens"], 4_000);
    // On the Responses API: `max_output_tokens`.
    let p = alibaba(Some(65_536)).prepare(&input("qwen3.8-max", &[flag("c")], &h), &[], &blobs).await.unwrap();
    assert_eq!(p.body["max_output_tokens"], 65_536);
    assert_eq!(p.max_tokens(), Some(65_536));
}

#[tokio::test]
async fn web_search() {
    let p = prepare("qwen3.8-max", &[flag("s")]).await.unwrap();
    assert_eq!(p.body["enable_search"], true);
    assert_eq!(p.body["search_options"], json!({"search_strategy": "max"}));
    assert_eq!(p.body["include_tool_usage"], true);
    assert_eq!(p.settings, vec!["effort xhigh", "web search (max)"]);
    assert!(p.notes.iter().any(|n| n.contains("returns no sources")));
    let p = prepare("qwen3.7-max", &[flag("s")]).await.unwrap();
    assert_eq!(p.body["search_options"], json!({"search_strategy": "agent"}));
    let p = prepare("qwen3.8-max", &[flag("su")]).await.unwrap();
    assert_eq!(p.body["search_options"], json!({"search_strategy": "agent_max"}));
    assert_eq!(p.settings[1], "web search + web extractor (agent_max)");
}

#[tokio::test]
async fn code_interpreter_uses_the_responses_api() {
    let p = prepare("qwen3.8-max", &[flag("c")]).await.unwrap();
    assert_eq!(p.dialect, RESPONSES_DIALECT);
    assert_eq!(p.url, "https://dashscope.aliyuncs.com/compatible-mode/v1/responses");
    assert_eq!(p.body["store"], false);
    assert_eq!(p.body["stream"], true);
    assert_eq!(p.body["tools"], json!([{"type": "code_interpreter"}]));
    assert_eq!(p.body["reasoning"], json!({"effort": "xhigh"}));
    assert!(p.body.get("enable_thinking").is_none() && p.body.get("messages").is_none());
    assert_eq!(
        p.body["input"],
        json!([{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]}])
    );
    assert_eq!(p.settings, vec!["effort xhigh", "code interpreter"]);
    let p = prepare("qwen3.8-max", &[flag("scu")]).await.unwrap();
    assert_eq!(
        p.body["tools"],
        json!([{"type": "code_interpreter"}, {"type": "web_search"}, {"type": "web_extractor"}])
    );
    assert!(p.body.get("enable_search").is_none());
    assert_eq!(p.settings, vec!["effort xhigh", "code interpreter", "web search", "web extractor"]);
    // Qwen3.7: no `reasoning.effort` on this API, the switch instead.
    let p = prepare("qwen3.7-max", &[flag("c")]).await.unwrap();
    assert_eq!(p.body["enable_thinking"], true);
    assert!(p.body.get("reasoning").is_none());
    let p = prepare("qwen3.7-plus", &[flag("c"), kv("e", "none")]).await.unwrap();
    assert_eq!(p.body["enable_thinking"], false);
    // Long conversations: the Responses API would silently truncate past ~80% of the window.
    let (blobs, _) = Blobs::new(&[]);
    let long = [Turn::user_text("字".repeat(700_000))];
    let e = alibaba(None).prepare(&input("qwen3.8-max", &[flag("c")], &long), &[], &blobs).await.unwrap_err();
    assert!(e.message.contains("silently truncate"), "{}", e.message);
    assert!(alibaba(None).prepare(&input("qwen3.8-max", &[], &long), &[], &blobs).await.is_ok());
}

#[tokio::test]
async fn media() {
    let big_video = vec![0u8; 8 * 1024 * 1024];
    let (blobs, refs) = Blobs::new(&[
        b"\xff\xd8\xff".to_vec(),
        b"\x00\x00mp4".to_vec(),
        b"%PDF".to_vec(),
        b"a,b\n1,2".to_vec(),
        b"ID3".to_vec(),
        big_video,
    ]);
    let with = |mime: &str, r: &BlobRef| BlobRef { mime: mime.into(), ..r.clone() };
    let image = Part::Image {
        blob: with("image/jpeg", &refs[0]),
        filename: None,
        generated: false,
        dims: None,
        thought: false,
    };
    let video = Part::File { blob: with("video/mp4", &refs[1]), filename: Some("v.mp4".into()), generated: false };
    let h = [user(vec![Part::text("look"), image.clone(), video.clone()])];
    let p = alibaba(None).prepare(&input("qwen3.8-max", &[], &h), &[], &blobs).await.unwrap();
    let c = &p.body["messages"][0]["content"];
    assert_eq!(c[1], json!({"type": "image_url", "image_url": {"url": "data:image/jpeg;base64,/9j/"}}));
    assert_eq!(c[2]["type"], "video_url");
    assert_eq!(p.body["vl_high_resolution_images"], true, "the highest image detail");
    assert_eq!(p.settings, vec!["effort xhigh", "high-resolution images"]);
    // Text-only model.
    let e = alibaba(None).prepare(&input("qwen3.7-max", &[], &h), &[], &blobs).await.unwrap_err();
    assert!(e.message.contains("takes text only"), "{}", e.message);
    // Base64 videos are limited to 10 MB encoded.
    let big = Part::File { blob: with("video/mp4", &refs[5]), filename: Some("big.mp4".into()), generated: false };
    let e = alibaba(None).prepare(&input("qwen3.8-max", &[], &[user(vec![big])]), &[], &blobs).await.unwrap_err();
    assert!(e.message.contains("under 10 MB"), "{}", e.message);
    let pdf = Part::File { blob: with("application/pdf", &refs[2]), filename: Some("a.pdf".into()), generated: false };
    let e = alibaba(None).prepare(&input("qwen3.8-max", &[], &[user(vec![pdf])]), &[], &blobs).await.unwrap_err();
    assert!(e.message.contains("documents only go by upload"), "{}", e.message);
    let audio = Part::File { blob: with("audio/mpeg", &refs[4]), filename: Some("a.mp3".into()), generated: false };
    let e = alibaba(None).prepare(&input("qwen3.8-max", &[], &[user(vec![audio])]), &[], &blobs).await.unwrap_err();
    assert!(e.message.contains("no audio input"), "{}", e.message);
    let csv = Part::File { blob: with("text/csv", &refs[3]), filename: Some("t.csv".into()), generated: false };
    let p = alibaba(None).prepare(&input("qwen3.7-max", &[], &[user(vec![csv])]), &[], &blobs).await.unwrap();
    assert_eq!(p.body["messages"][0]["content"], "a,b\n1,2");
    assert!(p.notes[0].contains("t.csv inlined"), "{:?}", p.notes);
    let loc = Part::Location { latitude: 1.0, longitude: 2.0, accuracy_m: None, title: None, address: None };
    let e = alibaba(None).prepare(&input("qwen3.8-max", &[], &[user(vec![loc])]), &[], &blobs).await.unwrap_err();
    assert!(e.message.contains("does not take a location via Alibaba Model Studio"), "{}", e.message);
    // Responses (code interpreter): images without the high-resolution switch, no video.
    let h2 = [user(vec![Part::text("look"), image])];
    let p = alibaba(None).prepare(&input("qwen3.8-max", &[flag("c")], &h2), &[], &blobs).await.unwrap();
    assert_eq!(
        p.body["input"][0]["content"][1],
        json!({"type": "input_image", "image_url": "data:image/jpeg;base64,/9j/"})
    );
    assert!(p.body.get("vl_high_resolution_images").is_none());
    assert!(p.notes.iter().any(|n| n.contains("default resolution")));
    let e = alibaba(None).prepare(&input("qwen3.8-max", &[flag("c")], &h), &[], &blobs).await.unwrap_err();
    assert!(e.message.contains("videos only as public URLs"), "{}", e.message);
}

#[tokio::test]
async fn history_replay() {
    let chat_native = json!({"role": "assistant", "content": "A1", "reasoning_content": "R1"});
    let resp_output = json!([
        {"type": "reasoning", "id": "rs_1", "summary": [{"type": "summary_text", "text": "R2"}]},
        {"type": "code_interpreter_call", "id": "ci_1", "status": "completed", "code": "print(1)", "outputs": [{"type": "logs", "logs": "1"}]},
        {"type": "web_search_call", "id": "ws_1", "status": "completed", "action": {"type": "search", "query": "q"}},
        {"type": "message", "id": "msg_1", "role": "assistant", "status": "completed", "content": [{"type": "output_text", "text": "A2", "annotations": []}]},
    ]);
    let h = vec![
        user(vec![Part::text("Q1")]),
        native(DIALECT, "qwen3.8-max", chat_native.clone(), "A1"),
        user(vec![Part::text("Q2")]),
        native(RESPONSES_DIALECT, "qwen3.8-max", json!({"output": resp_output}), "A2"),
        user(vec![Part::text("Q3")]),
        native(DIALECT, "qwen3.7-max", json!({"role": "assistant", "content": "B", "reasoning_content": "x"}), "B"),
        user(vec![Part::text("Q4")]),
    ];
    let (blobs, _) = Blobs::new(&[]);
    let p = alibaba(None).prepare(&input("qwen3.8-max", &[], &h), &[], &blobs).await.unwrap();
    let m = p.body["messages"].as_array().unwrap();
    assert_eq!(m[1], chat_native, "same model: verbatim, reasoning_content in its own field");
    assert_eq!(m[3], json!({"role": "assistant", "content": "A2", "reasoning_content": "R2"}));
    assert_eq!(m[5], json!({"role": "assistant", "content": "B"}));
    let p = alibaba(None).prepare(&input("qwen3.8-max", &[flag("c")], &h), &[], &blobs).await.unwrap();
    let items = p.body["input"].as_array().unwrap();
    assert_eq!(
        items[1],
        json!({"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "A1"}]})
    );
    // Own items the API takes back as input; tool calls without an input form are left out.
    assert_eq!(items[3], resp_output[0]);
    assert_eq!(items[4], resp_output[2]);
    assert_eq!(items[5], resp_output[3]);
    assert_eq!(items[6]["role"], "user");
}

fn run_chat(chunks: &[Value]) -> (ChatParser, crate::Accumulator) {
    let mut p = ChatParser::default();
    let mut acc = crate::Accumulator::new();
    for c in chunks {
        for e in p.event(None, c).unwrap() {
            acc.apply(e);
        }
    }
    (p, acc)
}

/// The qwen3.8-max stream of the Chat Completions reference (with search usage).
#[test]
fn chat_stream() {
    let id = "chatcmpl-730609ad-25b4-9809-8bd2-29b3fb280cb9";
    let chunks = [
        json!({"model": "qwen3.8-max", "id": id, "created": 1794384852, "object": "chat.completion.chunk", "usage": null,
               "choices": [{"logprobs": null, "index": 0, "delta": {"reasoning_content": "", "content": "", "role": "assistant"}, "finish_reason": null}]}),
        json!({"id": id, "object": "chat.completion.chunk", "created": 1794384852, "model": "qwen3.8-max",
               "choices": [{"index": 0, "delta": {"content": "", "reasoning_content": "用户"}, "logprobs": null, "finish_reason": null}], "usage": null}),
        json!({"id": id, "object": "chat.completion.chunk", "created": 1794384858, "model": "qwen3.8-max",
               "choices": [{"index": 0, "delta": {"content": "我是通义", "reasoning_content": ""}, "logprobs": null, "finish_reason": null}], "usage": null}),
        json!({"id": id, "object": "chat.completion.chunk", "created": 1794384861, "model": "qwen3.8-max",
               "choices": [{"index": 0, "delta": {"content": "", "reasoning_content": ""}, "finish_reason": "stop", "logprobs": null}], "usage": null}),
        json!({"id": id, "object": "chat.completion.chunk", "created": 1794384861, "model": "qwen3.8-max", "choices": [],
               "usage": {"prompt_tokens": 2883, "total_tokens": 2962, "completion_tokens": 79,
                         "prompt_tokens_details": {"cached_tokens": 0, "text_tokens": 2883},
                         "completion_tokens_details": {"reasoning_tokens": 58, "text_tokens": 79},
                         "x_tools": {"search": {"count": 1, "strategy": "turbo"}}}}),
    ];
    let (p, acc) = run_chat(&chunks);
    assert!(p.finished());
    let parts = acc.parts();
    assert!(
        matches!(&parts[0], Part::Reasoning(r) if r.kind == ReasoningKind::Text && r.text.as_deref() == Some("用户"))
    );
    assert_eq!(acc.turn().answer_text(), "我是通义");
    assert_eq!(acc.upstream_id.as_deref(), Some(id));
    assert_eq!(p.native_payload(), json!({"role": "assistant", "content": "我是通义", "reasoning_content": "用户"}));
    let u = acc.usage.unwrap();
    assert_eq!(search_calls(&u), 1);
    // 2883 × ¥12/M + 79 × ¥36/M + one turbo search × ¥0.003
    let c = estimate_cost(model_spec("qwen3.8-max").unwrap(), &u, 1, "max", 7.0).unwrap();
    let cny = 2883.0 * 12.0 / 1e6 + 79.0 * 36.0 / 1e6 + 0.003;
    assert!((c - cny / 7.0).abs() < 1e-12, "{c}");
}

#[test]
fn chat_multimodal_content_and_length() {
    let (_, acc) = run_chat(&[
        json!({"id": "c", "choices": [{"index": 0, "delta": {"content": [{"text": "图中"}]}, "finish_reason": null}]}),
        json!({"id": "c", "choices": [{"index": 0, "delta": {"content": [{"text": "是猫"}]}, "finish_reason": "length"}]}),
    ]);
    assert_eq!(acc.turn().answer_text(), "图中是猫");
    assert_eq!(acc.finish_reason.as_deref(), Some("length"));
}

#[test]
fn chat_in_band_errors() {
    let mut p = ChatParser::default();
    p.event(None, &json!({"id": "c", "choices": [{"index": 0, "delta": {"content": "x"}}]})).unwrap();
    let chunk = json!({"error": {"message": "Output data may contain inappropriate content.", "type": "data_inspection_failed",
                                 "param": null, "code": "data_inspection_failed"}, "id": "c", "request_id": "r"});
    let e = p.event(None, &chunk).unwrap_err();
    assert_eq!((e.kind, e.retry), (ErrorKind::InBand, Retry::Never));
    assert_eq!(e.error_type.as_deref(), Some("data_inspection_failed"));
    assert!(e.partial_output);
    assert_eq!(e.details, Some(chunk));
    // DashScope's own error shape, without the `error` wrapper.
    let raw = json!({"code": "DataInspectionFailed", "message": "Output data may contain inappropriate content.", "request_id": "r"});
    let e = p.event(None, &raw).unwrap_err();
    assert_eq!((e.retry, e.error_type.as_deref()), (Retry::Never, Some("DataInspectionFailed")));
    assert_eq!(e.details, Some(raw));
}

/// A Responses stream with the code interpreter and the web extractor
/// (event and item shapes of the Responses reference).
#[test]
fn responses_stream() {
    let reasoning =
        json!({"type": "reasoning", "id": "rs_1", "summary": [{"type": "summary_text", "text": "Compute."}]});
    let code = json!({"type": "code_interpreter_call", "id": "ci_1", "status": "completed", "code": "print(123**21)",
                      "outputs": [{"type": "logs", "logs": "77269..."}], "container_id": "c1"});
    let extract = json!({"type": "web_extractor_call", "id": "msg_x", "status": "completed", "urls": ["https://cn.aliyun.com/"],
                         "goal": "提取关键信息", "output": "The useful information in https://cn.aliyun.com/ …"});
    let message = json!({"type": "message", "id": "msg_1", "role": "assistant", "status": "completed",
                         "content": [{"type": "output_text", "text": "= 77269…", "annotations": []}]});
    let mut seq = 0;
    let mut ev = |mut v: Value| {
        v["sequence_number"] = json!(seq);
        seq += 1;
        v
    };
    let events = [
        ev(
            json!({"type": "response.created", "response": {"id": "resp_1", "model": "qwen3.8-max", "status": "queued", "output": []}}),
        ),
        ev(json!({"type": "response.in_progress", "response": {"id": "resp_1", "status": "in_progress"}})),
        ev(
            json!({"type": "response.output_item.added", "output_index": 0, "item": {"type": "reasoning", "id": "rs_1", "summary": []}}),
        ),
        ev(
            json!({"type": "response.reasoning_text.delta", "output_index": 0, "item_id": "rs_1", "content_index": 0, "delta": "Comp"}),
        ),
        ev(
            json!({"type": "response.reasoning_text.delta", "output_index": 0, "item_id": "rs_1", "content_index": 0, "delta": "ute."}),
        ),
        ev(
            json!({"type": "response.reasoning_text.done", "output_index": 0, "item_id": "rs_1", "content_index": 0, "text": "Compute."}),
        ),
        ev(json!({"type": "response.output_item.done", "output_index": 0, "item": reasoning})),
        ev(json!({"type": "response.output_item.added", "output_index": 1,
                   "item": {"type": "code_interpreter_call", "id": "ci_1", "status": "in_progress"}})),
        ev(json!({"type": "response.code_interpreter_call.in_progress", "output_index": 1, "item_id": "ci_1"})),
        ev(json!({"type": "response.code_interpreter_call.interpreting", "output_index": 1, "item_id": "ci_1"})),
        ev(json!({"type": "response.code_interpreter_call.completed", "output_index": 1, "item_id": "ci_1"})),
        ev(json!({"type": "response.output_item.done", "output_index": 1, "item": code})),
        ev(json!({"type": "response.output_item.added", "output_index": 2,
                   "item": {"type": "web_extractor_call", "id": "msg_x", "status": "in_progress", "urls": ["https://cn.aliyun.com/"], "goal": "提取关键信息"}})),
        ev(json!({"type": "response.output_item.done", "output_index": 2, "item": extract})),
        ev(json!({"type": "response.output_item.added", "output_index": 3,
                   "item": {"type": "message", "id": "msg_1", "role": "assistant", "status": "in_progress", "content": []}})),
        ev(json!({"type": "response.content_part.added", "output_index": 3, "item_id": "msg_1", "content_index": 0,
                   "part": {"type": "output_text", "text": "", "annotations": []}})),
        ev(
            json!({"type": "response.output_text.delta", "output_index": 3, "item_id": "msg_1", "content_index": 0, "delta": "= 77269…"}),
        ),
        ev(
            json!({"type": "response.output_text.done", "output_index": 3, "item_id": "msg_1", "content_index": 0, "text": "= 77269…"}),
        ),
        ev(json!({"type": "response.output_item.done", "output_index": 3, "item": message})),
        ev(
            json!({"type": "response.completed", "response": {"id": "resp_1", "model": "qwen3.8-max", "status": "completed",
                   "output": [reasoning, code, extract, message],
                   "usage": {"input_tokens": 1000, "input_tokens_details": {"cached_tokens": 200}, "output_tokens": 100,
                             "output_tokens_details": {"reasoning_tokens": 40}, "total_tokens": 1100,
                             "x_tools": {"web_search": {"count": 2}}}}}),
        ),
    ];
    let mut p = responses::Parser::new(model_spec("qwen3.8-max"), 7.0);
    let mut acc = crate::Accumulator::new();
    for e in &events {
        for x in p.event(None, e).unwrap() {
            acc.apply(x);
        }
    }
    assert!(p.finished());
    let parts = acc.parts();
    assert_eq!(parts.len(), 4, "{parts:?}");
    assert!(
        matches!(&parts[0], Part::Reasoning(r) if r.kind == ReasoningKind::Text && r.text.as_deref() == Some("Compute."))
    );
    match &parts[1] {
        Part::ToolCall(t) => {
            assert_eq!((t.name.as_str(), t.status.as_deref()), ("code_interpreter", Some("completed")));
            assert_eq!(t.input, Some(json!("print(123**21)")));
            assert_eq!(t.output, Some(json!([{"type": "logs", "logs": "77269..."}])));
        }
        other => panic!("{other:?}"),
    }
    match &parts[2] {
        Part::ToolCall(t) => {
            assert_eq!(t.name, "web_extractor");
            assert_eq!(t.input, Some(json!({"urls": ["https://cn.aliyun.com/"], "goal": "提取关键信息"})));
            assert!(t.output.as_ref().unwrap().as_str().unwrap().starts_with("The useful information"));
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(acc.turn().answer_text(), "= 77269…");
    let u = acc.usage.unwrap();
    // 800 × 12 + 200 × 1.5 + 100 × 36 (per M) + 2 searches × ¥0.004 (billed as agent).
    let cny = (800.0 * 12.0 + 200.0 * 1.5 + 100.0 * 36.0) / 1e6 + 0.008;
    assert!((u.cost_usd.unwrap() - cny / 7.0).abs() < 1e-12, "{:?}", u.cost_usd);
    assert_eq!(p.native_payload()["output"].as_array().unwrap().len(), 4);
}

#[test]
fn responses_errors_and_incomplete() {
    let mut p = responses::Parser::new(model_spec("qwen3.8-max"), 7.0);
    let e = p
        .event(None, &json!({"type": "response.failed", "sequence_number": 1, "response": {"status": "failed",
            "error": {"code": "InternalError", "message": "An internal error has occured, please try again later or contact service support."}}}))
        .unwrap_err();
    assert_eq!((e.kind, e.retry), (ErrorKind::InBand, Retry::Backoff));
    let e = p
        .event(None, &json!({"type": "error", "sequence_number": 2, "code": "DataInspectionFailed", "message": "Output data may contain inappropriate content."}))
        .unwrap_err();
    assert_eq!((e.retry, e.error_type.as_deref()), (Retry::Never, Some("DataInspectionFailed")));
    let mut p = responses::Parser::new(model_spec("qwen3.8-max"), 7.0);
    let evs = p
        .event(
            None,
            &json!({"type": "response.incomplete", "sequence_number": 3, "response": {"status": "incomplete",
            "incomplete_details": {"reason": "max_output_tokens"}, "output": []}}),
        )
        .unwrap();
    assert!(evs.iter().any(|e| matches!(e, StreamEvent::Finish { reason: Some(r), .. } if r == "length")));
}

#[test]
fn errors() {
    let http = |status: u16, retry_after: Option<u64>, body: &str| {
        crate::chatstream::http_error(status, retry_after, body, error_from_json)
    };
    // As probed without a key (2026-11-12): Chat Completions, then the Responses API.
    let e = http(
        401,
        None,
        r#"{"error":{"message":"You didn't provide an API key. You need to provide your API key in an Authorization header using Bearer auth (i.e. Authorization: Bearer YOUR_KEY). ","type":"invalid_request_error","param":null,"code":null},"request_id":"24be5515-7625-9bab-ba14-da27b415bd9c"}"#,
    );
    assert_eq!((e.retry, e.status, e.error_type.as_deref()), (Retry::Never, Some(401), Some("invalid_request_error")));
    assert_eq!(e.details.unwrap()["request_id"], "24be5515-7625-9bab-ba14-da27b415bd9c");
    let e = http(401, None, r#"{"code":"InvalidApiKey","message":"No API-key provided.","request_id":"ca97d3b2"}"#);
    assert_eq!((e.retry, e.error_type.as_deref()), (Retry::Never, Some("InvalidApiKey")));
    assert_eq!(e.message, "No API-key provided.");
    let e = http(
        429,
        None,
        r#"{"error":{"message":"Requests rate limit exceeded, please try again later.","type":"limit_requests","code":"limit_requests"}}"#,
    );
    assert_eq!(e.retry, Retry::After(20));
    let e = http(429, Some(30), r#"{"code":"Throttling.RateQuota","message":"You have exceeded your request limit."}"#);
    assert_eq!(e.retry, Retry::After(30));
    let e = http(
        429,
        None,
        r#"{"error":{"message":"You exceeded your current quota, please check your plan and billing details.","type":"insufficient_quota","code":"insufficient_quota"}}"#,
    );
    assert_eq!(e.retry, Retry::After(20), "documented as a TPS/TPM limit");
    let e = http(429, None, r#"{"code":"Throttling.AllocationQuota","message":"Free allocated quota exceeded."}"#);
    assert_eq!(e.retry, Retry::Never);
    let e = http(
        400,
        None,
        r#"{"code":"Arrearage","message":"Access denied, please make sure your account is in good standing."}"#,
    );
    assert_eq!(e.retry, Retry::Never);
    let e = http(
        400,
        None,
        r#"{"error":{"message":"Input data may contain inappropriate content.","type":"data_inspection_failed","code":"data_inspection_failed"}}"#,
    );
    assert_eq!(e.retry, Retry::Never);
    let e = http(
        500,
        None,
        r#"{"error":{"message":"An internal error has occured","type":"internal_error","code":"internal_error"}}"#,
    );
    assert_eq!(e.retry, Retry::Backoff);
    let e = http(503, None, r#"{"code":"ServiceUnavailable","message":"busy"}"#);
    assert_eq!(e.retry, Retry::Backoff);
    let e = http(
        404,
        None,
        r#"{"error":{"message":"The model `qwen9` does not exist","type":"invalid_request_error","code":"model_not_found"}}"#,
    );
    assert_eq!((e.retry, e.error_type.as_deref()), (Retry::Never, Some("model_not_found")));
    let e = http(
        400,
        None,
        r#"{"code":"InternalError.Algo.InvalidParameter","message":"Range of input length should be [1, 983616]"}"#,
    );
    assert_eq!(e.retry, Retry::Never);
    assert!(!e.is_context_overflow(), "the prompt alone is too long: a smaller output budget cannot help");
    let e = http(502, None, "<html>Bad Gateway</html>");
    assert_eq!(e.retry, Retry::Backoff);
}

#[test]
fn tiered_cost() {
    let spec = model_spec("qwen3.7-plus").unwrap();
    let u = |input| Usage { input_tokens: Some(input), output_tokens: Some(1_000), ..Default::default() };
    // ≤256K input: ¥2 / ¥8; above: ¥6 / ¥24 for all tokens of the request.
    let c = estimate_cost(spec, &u(100_000), 0, "agent", 1.0).unwrap();
    assert!((c - (0.1 * 2.0 + 0.001 * 8.0)).abs() < 1e-12, "{c}");
    let c = estimate_cost(spec, &u(300_000), 0, "agent", 1.0).unwrap();
    assert!((c - (0.3 * 6.0 + 0.001 * 24.0)).abs() < 1e-12, "{c}");
    assert_eq!(search_fee_cny("turbo"), 0.003);
    assert_eq!(search_fee_cny("agent_max"), 0.004);
}
