//! Alibaba image tests: requests per model and option, the conversation rules,
//! responses (DashScope's documented shapes) against a local fake server, cost.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use omni_core::content::{Part, Role};
use omni_core::trigger::OptionToken;
use omni_core::{BlobRef, Origin, Turn};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

use super::*;
use crate::provider::Provider;

struct Blobs(Vec<(BlobRef, Vec<u8>)>);

impl BlobLoader for Blobs {
    fn load<'a>(&'a self, r: &'a BlobRef) -> crate::BoxFuture<'a, anyhow::Result<Vec<u8>>> {
        let found = self.0.iter().find(|(b, _)| b == r).map(|(_, d)| d.clone());
        Box::pin(async move { found.ok_or_else(|| anyhow::anyhow!("no blob")) })
    }
}

fn ali_at(base: &str) -> super::super::Alibaba {
    super::super::Alibaba::new(DirectConfig {
        api_key: "k".into(),
        base_url: base.into(),
        connect_timeout: Duration::from_secs(1),
        idle_timeout: Duration::from_secs(5),
        max_tokens_cap: None,
        cny_per_usd: 7.0,
        long_decode_budget: 393_216,
    })
    .unwrap()
}

fn ali() -> super::super::Alibaba {
    ali_at("https://dashscope.aliyuncs.com/compatible-mode/v1")
}

fn kv(k: &str, v: &str) -> OptionToken {
    OptionToken::KeyValue(k.into(), v.into())
}

fn flag(f: &str) -> OptionToken {
    OptionToken::Flag(f.into())
}

fn input<'a>(model: &'a str, options: &'a [OptionToken], history: &'a [Turn]) -> CallInput<'a> {
    CallInput { model, options, history, session_id: None, user: None, known_context: None, max_tokens_override: None }
}

async fn prep_with(
    model: &str,
    options: &[OptionToken],
    history: &[Turn],
    blobs: &Blobs,
) -> Result<Prepared, LlmError> {
    ali().prepare(&input(model, options, history), &[], blobs).await
}

async fn prep(model: &str, options: &[OptionToken]) -> Result<Prepared, LlmError> {
    prep_with(model, options, &[Turn::user_text("一只猫")], &Blobs(vec![])).await
}

fn image(bytes: &[u8], generated: bool) -> (Part, (BlobRef, Vec<u8>)) {
    let b = BlobRef::for_bytes(bytes, "image/png");
    (Part::Image { blob: b.clone(), filename: None, generated, dims: None, thought: false }, (b, bytes.to_vec()))
}

#[tokio::test]
async fn defaults_are_the_largest_size_one_image_thinking_no_watermark() {
    // Qwen-Image 3.0 pro: sizes above 2,250,000 pixels cost double, so the model's own size
    // (D79), with a note naming the option.
    let p = prep("qwen-image-3.0-pro", &[]).await.unwrap();
    assert_eq!(p.dialect, DIALECT);
    assert_eq!(p.url, "https://dashscope.aliyuncs.com/api/v1/services/aigc/multimodal-generation/generation");
    assert_eq!(
        p.body,
        json!({
            "model": "qwen-image-3.0-pro",
            "input": {"messages": [{"role": "user", "content": [{"text": "一只猫"}]}]},
            "parameters": {"n": 1, "enable_thinking": true, "watermark": false}
        })
    );
    assert_eq!(p.settings, vec!["thinking on", "no watermark", "size: the model's choice"]);
    assert!(p.warnings.is_empty());
    assert_eq!(p.notes, ["image size is the model's choice by default, not the largest (2K); add +size=2K"]);
    // `ar=` alone: the largest of that ratio within the cheaper tier (2,250,000 px).
    let p = prep("qwen-image-3.0-pro", &[kv("ar", "16:9")]).await.unwrap();
    let size = p.body["parameters"]["size"].as_str().unwrap().to_string();
    let (w, h) = size.split_once('*').unwrap();
    assert!(w.parse::<u64>().unwrap() * h.parse::<u64>().unwrap() <= 2_250_000, "{size}");
    // Asked for: no note.
    let p = prep("qwen-image-3.0-pro", &[kv("size", "2K")]).await.unwrap();
    assert_eq!((p.body["parameters"]["size"].as_str(), p.notes.len()), (Some("2048*2048"), 0));
    // The other models cost the same at every size: the largest.
    let p = prep("qwen-image-3.0", &[]).await.unwrap();
    assert_eq!((p.body["parameters"]["size"].as_str(), p.notes.len()), (Some("2048*2048"), 0));
    assert!(ali().knows("wan2.7-image-pro") && ali().knows("z-image-turbo"));
    // Wan 2.7 pro: 4K text-to-image; wan2.6-t2i: n 1 (its default is 4), the largest size.
    let p = prep("wan2.7-image-pro", &[]).await.unwrap();
    assert_eq!(p.body["parameters"], json!({"size": "4K", "n": 1, "thinking_mode": true, "watermark": false}));
    let p = prep("wan2.6-t2i", &[]).await.unwrap();
    assert_eq!(p.body["parameters"], json!({"size": "1440*1440", "n": 1, "watermark": false}));
    // z-image: its paid "智能思考" rewriting off (Alibaba's default) with a note; `rewrite=on` turns it on.
    let p = prep("z-image-turbo", &[]).await.unwrap();
    assert_eq!(p.body["parameters"], json!({"size": "2048*2048", "prompt_extend": false}));
    assert_eq!(
        p.notes,
        [
            "prompt rewriting with thinking (智能思考) is off by default; add +rewrite=on to turn it on (it doubles the price)"
        ]
    );
    let p = prep("z-image-turbo", &[kv("rewrite", "on")]).await.unwrap();
    assert_eq!((p.body["parameters"]["prompt_extend"].as_bool(), p.notes.len()), (Some(true), 0));
    let e = prep("qwen-image-3.0", &[kv("rewrite", "on")]).await.unwrap_err().message;
    assert!(e.contains("prompt-rewriting switch (`rewrite=`)"), "{e}");
    // Workspace domains keep their host.
    assert_eq!(
        native_root("https://ws-1.cn-beijing.maas.aliyuncs.com/compatible-mode/v1"),
        "https://ws-1.cn-beijing.maas.aliyuncs.com"
    );
    // The chat models are unchanged.
    assert_eq!(prep("qwen3.8-max", &[]).await.unwrap().dialect, super::super::DIALECT);
}

#[tokio::test]
async fn options_per_model() {
    let size = |p: &Prepared| p.body["parameters"]["size"].as_str().map(str::to_string);
    assert_eq!(size(&prep("qwen-image-3.0", &[kv("ar", "16:9")]).await.unwrap()).as_deref(), Some("2720*1536"));
    assert_eq!(size(&prep("qwen-image-2.1-pro", &[kv("size", "1K")]).await.unwrap()).as_deref(), Some("1024*1024"));
    assert_eq!(size(&prep("wan2.6-t2i", &[kv("ar", "16:9")]).await.unwrap()).as_deref(), Some("1920*1072"));
    assert_eq!(size(&prep("wan2.7-image", &[kv("size", "1K")]).await.unwrap()).as_deref(), Some("1K"));
    assert_eq!(
        size(&prep("wan2.7-image-pro", &[kv("size", "4K"), kv("ar", "9:16")]).await.unwrap()).as_deref(),
        Some("3072*5456")
    );
    let e = prep("wan2.7-image", &[kv("size", "4K")]).await.unwrap_err().message;
    assert!(e.contains("sizes: 1K, 2K"), "{e}");
    assert!(prep("qwen-image-3.0", &[kv("ar", "9:1")]).await.unwrap_err().message.contains("1:8 to 8:1"));
    assert!(prep("wan2.6-t2i", &[kv("size", "4K")]).await.unwrap_err().message.contains("not available"));

    assert_eq!(prep("qwen-image-3.0-pro", &[kv("n", "6")]).await.unwrap().body["parameters"]["n"], 6);
    assert!(prep("qwen-image-3.0-pro", &[kv("n", "7")]).await.unwrap_err().message.contains("1–6"));
    assert!(prep("wan2.7-image", &[kv("n", "5")]).await.unwrap_err().message.contains("1–4"));
    assert!(prep("z-image-turbo", &[kv("n", "2")]).await.unwrap_err().message.contains("(`n=`)"));

    let p = prep("qwen-image-3.0-pro", &[kv("e", "none")]).await.unwrap();
    assert_eq!(p.body["parameters"]["enable_thinking"], false);
    assert_eq!(
        p.applied.iter().find(|a| a.patch["parameters"].get("enable_thinking").is_some()).unwrap().source,
        "option"
    );
    // z-image's rewriting is `rewrite=`, not reasoning (owner, 2026-11-14).
    assert!(prep("z-image-turbo", &[kv("e", "none")]).await.unwrap_err().message.contains("cannot turn reasoning off"));
    assert!(prep("wan2.6-t2i", &[kv("e", "none")]).await.unwrap_err().message.contains("cannot turn reasoning off"));
    assert!(prep("qwen-image-3.0", &[kv("e", "high")]).await.is_err());
    for (opt, what) in [(kv("q", "hd"), "(`q=`)"), (kv("bg", "transparent"), "(`bg=`)"), (flag("s"), "web search")] {
        let e = prep("qwen-image-3.0", &[opt]).await.unwrap_err().message;
        assert!(e.contains(what), "{e}");
    }
    assert!(prep("qwen-image-3.0", &[flag("i")]).await.unwrap_err().message.contains("without `+i`"));
    // Not in the table (retired, task-only and unknown ids alike): not supported, their price is unknown.
    for id in ["qwen-image-plus", "wan2.5-t2i-preview", "qwen-image-4.0"] {
        let e = prep(id, &[]).await.unwrap_err().message;
        assert_eq!(
            e,
            format!(
                "`{id}` is not in the bot's Alibaba Model Studio model table (its price is unknown), so it is not supported"
            )
        );
    }
}

#[tokio::test]
async fn edits_and_inputs() {
    let (out, out_blob) = image(b"\x89PNGgenerated", true);
    let (attached, att_blob) = image(b"\x89PNGphoto", false);
    let blobs = Blobs(vec![out_blob, att_blob]);
    let answer = Turn { role: Role::Assistant, origin: Origin::Live, parts: vec![out], native: None };
    let reply = Turn {
        role: Role::User,
        origin: Origin::Live,
        parts: vec![Part::text("换成夜景"), attached.clone()],
        native: None,
    };
    let history = vec![Turn::user_text("画一座城市"), answer.clone(), reply.clone()];
    let p = prep_with("qwen-image-3.0-pro", &[], &history, &blobs).await.unwrap();
    let content = p.body["input"]["messages"][0]["content"].as_array().unwrap();
    assert_eq!(content.len(), 3);
    assert_eq!(content[0]["image"], format!("data:image/png;base64,{}", crate::provider::b64(b"\x89PNGgenerated")));
    assert_eq!(content[1]["image"], format!("data:image/png;base64,{}", crate::provider::b64(b"\x89PNGphoto")));
    assert_eq!(content[2], json!({"text": "换成夜景"}));
    assert!(p.body["parameters"].get("size").is_none(), "edits keep the input's aspect");
    assert!(p.settings.contains(&"editing the replied-to image + 1 input image".to_string()), "{:?}", p.settings);
    // Wan 2.7: no 4K and no thinking switch with input images.
    let p = prep_with("wan2.7-image-pro", &[], &history, &blobs).await.unwrap();
    assert_eq!(p.body["parameters"]["size"], "2K");
    assert!(p.body["parameters"].get("thinking_mode").is_none());
    assert!(
        prep_with("wan2.7-image-pro", &[kv("size", "4K")], &history, &blobs)
            .await
            .unwrap_err()
            .message
            .contains("with input images")
    );
    assert!(
        prep_with("wan2.7-image", &[kv("e", "none")], &history, &blobs)
            .await
            .unwrap_err()
            .message
            .contains("only without input images")
    );
    // Text-to-image only models, and the editing-only one.
    assert!(prep_with("wan2.6-t2i", &[], &history, &blobs).await.unwrap_err().message.contains("no input image"));
    assert!(prep("wan2.6-image", &[]).await.unwrap_err().message.contains("needs 1–4 input images"));
    assert!(prep_with("wan2.6-image", &[], &history, &blobs).await.is_ok());
    // Limits and other inputs.
    let mut parts = vec![Part::text("x")];
    parts.extend((0..4).map(|_| attached.clone()));
    let four = vec![Turn { role: Role::User, origin: Origin::Live, parts, native: None }];
    assert!(prep_with("qwen-image-3.0", &[], &four, &blobs).await.unwrap_err().message.contains("at most 3"));
    assert!(prep_with("qwen-image-2.1-pro", &[], &four, &blobs).await.is_ok());
    let system = vec![
        Turn { role: Role::System, origin: Origin::Synthetic, parts: vec![Part::text("s")], native: None },
        Turn::user_text("x"),
    ];
    assert!(prep_with("qwen-image-3.0", &[], &system, &blobs).await.unwrap_err().message.contains("no system prompt"));
    // A conversation without a replied-to image to edit is refused (one prompt per request).
    let chat = vec![Turn::user_text("讲个故事"), Turn::assistant_text("从前"), Turn::user_text("画出来")];
    let e = prep_with("qwen-image-3.0", &[], &chat, &blobs).await.unwrap_err().message;
    assert!(e.contains("`qwen-image-3.0` takes one prompt, not a conversation"), "{e}");
    // Edits keep working: only the replied-to answer's images and this message go.
    assert!(prep_with("qwen-image-3.0", &[], &history, &blobs).await.unwrap().notes.is_empty());
}

#[test]
fn prices() {
    let s = spec("qwen-image-3.0-pro").unwrap();
    let body = json!({"input": {"messages": [{"content": [{"image": "x"}, {"text": "t"}]}]}});
    let u = |w: u64, h: u64| json!({"output_width": w, "output_height": h, "input_image_count": 1, "output_image_count": 1});
    assert!((cost_cny(s, &body, &u(1024, 1024), 1) - 0.27).abs() < 1e-12);
    assert!((cost_cny(s, &body, &u(2048, 2048), 1) - 0.52).abs() < 1e-12);
    let w = spec("wan2.7-image-pro").unwrap();
    assert!((cost_cny(w, &json!({}), &json!({"image_count": 2, "size": "4096*4096"}), 2) - 1.0).abs() < 1e-12);
    let z = spec("z-image-turbo").unwrap();
    assert!(
        (cost_cny(z, &json!({"parameters": {"prompt_extend": true}}), &json!({"image_count": 1}), 1) - 0.2).abs()
            < 1e-12
    );
    assert!(
        (cost_cny(z, &json!({"parameters": {"prompt_extend": false}}), &json!({"image_count": 1}), 1) - 0.1).abs()
            < 1e-12
    );
}

// ------------------------------------------------------------------ HTTP (local fake server)

async fn serve(replies: Vec<(u16, &'static str, Vec<u8>)>) -> (String, Arc<Mutex<Vec<(String, Value)>>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = format!("http://{}", listener.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let (log, me) = (seen.clone(), addr.clone());
    tokio::spawn(async move {
        for (status, ctype, body) in replies {
            let (mut sock, _) = listener.accept().await.unwrap();
            let (mut buf, mut tmp) = (Vec::new(), vec![0u8; 65536]);
            let (head, sent) = loop {
                let n = sock.read(&mut tmp).await.unwrap();
                buf.extend_from_slice(&tmp[..n]);
                if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&buf[..pos]).to_string();
                    let len = head
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse().unwrap())
                        })
                        .unwrap_or(0usize);
                    while buf.len() < pos + 4 + len {
                        let n = sock.read(&mut tmp).await.unwrap();
                        buf.extend_from_slice(&tmp[..n]);
                    }
                    break (head, serde_json::from_slice(&buf[pos + 4..pos + 4 + len]).unwrap_or(Value::Null));
                }
            };
            log.lock().unwrap().push((head, sent));
            let body = if ctype == "application/json" {
                String::from_utf8_lossy(&body).replace("{base}", &me).into_bytes()
            } else {
                body
            };
            let resp = format!(
                "HTTP/1.1 {status} X\r\ncontent-type: {ctype}\r\nx-request-id: hdr-1\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            sock.write_all(resp.as_bytes()).await.unwrap();
            sock.write_all(&body).await.unwrap();
            let _ = sock.shutdown().await;
        }
    });
    (addr, seen)
}

async fn run(a: &super::super::Alibaba, p: &Prepared) -> (Result<Value, LlmError>, CallOutcome, Vec<StreamEvent>) {
    let mut evs = Vec::new();
    let (r, o) = a.stream(p, &mut |e: StreamEvent| evs.push(e), &crate::NullObserver, &CancellationToken::new()).await;
    (r, o, evs)
}

#[tokio::test]
async fn end_to_end() {
    let qwen = json!({"output": {"choices": [{"finish_reason": "stop", "message": {"role": "assistant",
                        "content": [{"image": "{base}/r/a.png?Expires=1"}]}}]},
                      "usage": {"output_height": 2048, "output_width": 2048, "input_image_count": 0,
                                "input_image_type": "qima_input_1k", "output_image_count": 1, "output_image_type": "qima_output_2k"},
                      "request_id": "571ae02f"});
    let z = json!({"output": {"choices": [{"finish_reason": "stop", "message": {"role": "assistant",
                    "reasoning_content": "用户想要一只猫……",
                    "content": [{"image": "{base}/r/z.png"}, {"text": "一只橘猫趴在窗台上，午后阳光"}]}}]},
                   "usage": {"width": 2048, "height": 2048, "image_count": 1, "input_tokens": 30, "output_tokens": 120,
                             "output_tokens_details": {"reasoning_tokens": 100}, "total_tokens": 150},
                   "request_id": "z1"});
    let denied = r#"{"request_id":"31f808fd","code":"DataInspectionFailed","message":"Output data may contain inappropriate content."}"#;
    let (addr, seen) = serve(vec![
        (200, "application/json", qwen.to_string().into_bytes()),
        (200, "image/png", b"\x89PNG-a".to_vec()),
        (200, "application/json", z.to_string().into_bytes()),
        (200, "image/png", b"\x89PNG-z".to_vec()),
        (400, "application/json", denied.as_bytes().to_vec()),
    ])
    .await;
    let a = ali_at(&format!("{addr}/compatible-mode/v1"));
    let h = vec![Turn::user_text("一只猫")];
    let p = a.prepare(&input("qwen-image-3.0-pro", &[], &h), &[], &Blobs(vec![])).await.unwrap();
    let (r, outcome, evs) = run(&a, &p).await;
    assert_eq!(r.unwrap()["request_id"], "571ae02f");
    assert_eq!(outcome.upstream_id.as_deref(), Some("571ae02f"));
    assert!(evs.contains(&StreamEvent::Image { data: b"\x89PNG-a".to_vec(), mime: "image/png".into() }));
    let u = evs.iter().find_map(|e| if let StreamEvent::Usage(u) = e { Some(u) } else { None }).unwrap();
    assert!((u.cost_usd.unwrap() - 0.5 / 7.0).abs() < 1e-12, "2K output at ¥0.5");
    assert!(evs.contains(&StreamEvent::Finish { reason: Some("stop".into()), native: Some("stop".into()) }));
    {
        let s = seen.lock().unwrap();
        assert!(
            s[0].0.starts_with("POST /api/v1/services/aigc/multimodal-generation/generation HTTP/1.1"),
            "{}",
            s[0].0
        );
        assert!(s[0].0.to_ascii_lowercase().contains("authorization: bearer k"));
        assert_eq!(s[0].1, p.body);
        assert!(s[1].0.starts_with("GET /r/a.png?Expires=1 HTTP/1.1"));
    }

    // z-image: its reasoning, the image, then the rewritten prompt, in the order given.
    let p = a.prepare(&input("z-image-turbo", &[kv("rewrite", "on")], &h), &[], &Blobs(vec![])).await.unwrap();
    let (r, _, evs) = run(&a, &p).await;
    assert!(r.is_ok());
    let kinds: Vec<&str> = evs
        .iter()
        .filter_map(|e| match e {
            StreamEvent::Reasoning { .. } => Some("reasoning"),
            StreamEvent::Image { .. } => Some("image"),
            StreamEvent::Text(_) => Some("text"),
            _ => None,
        })
        .collect();
    assert_eq!(kinds, ["reasoning", "image", "text"]);
    let u = evs.iter().find_map(|e| if let StreamEvent::Usage(u) = e { Some(u) } else { None }).unwrap();
    assert_eq!((u.input_tokens, u.output_tokens, u.reasoning_tokens), (Some(30), Some(120), Some(100)));
    assert!((u.cost_usd.unwrap() - 0.2 / 7.0).abs() < 1e-12);

    // Moderation: never retried, the object verbatim.
    let (r, _, _) = run(&a, &p).await;
    let e = r.unwrap_err();
    assert_eq!((e.status, e.retry, e.error_type.as_deref()), (Some(400), Retry::Never, Some("DataInspectionFailed")));
    assert_eq!(e.details.unwrap()["code"], "DataInspectionFailed");
}
