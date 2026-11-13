//! Zhipu video tests: requests per model, mode and option, the input rules,
//! and the task flow against a local fake server (shapes of Zhipu's docs).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use omni_core::content::{Part, Role};
use omni_core::trigger::OptionToken;
use omni_core::{BlobRef, Origin, Turn};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

use super::*;
use crate::openai::wire::tests::{MemBlobs, file, photo, user};
use crate::provider::Provider;

fn zp_at(base: &str) -> super::super::Zhipu {
    super::super::Zhipu::new(DirectConfig {
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

fn zp() -> super::super::Zhipu {
    zp_at("https://open.bigmodel.cn/api/paas/v4")
}

fn kv(k: &str, v: &str) -> OptionToken {
    OptionToken::KeyValue(k.into(), v.into())
}

fn flag(f: &str) -> OptionToken {
    OptionToken::Flag(f.into())
}

fn input<'a>(model: &'a str, options: &'a [OptionToken], history: &'a [Turn]) -> CallInput<'a> {
    CallInput {
        model,
        options,
        history,
        session_id: None,
        user: Some("user-hash-1".into()),
        known_context: None,
        max_tokens_override: None,
    }
}

async fn prep_with(
    model: &str,
    options: &[OptionToken],
    history: &[Turn],
    blobs: &MemBlobs,
) -> Result<Prepared, LlmError> {
    zp().prepare(&input(model, options, history), &[], blobs).await
}

async fn prep(model: &str, options: &[OptionToken]) -> Result<Prepared, LlmError> {
    prep_with(model, options, &[Turn::user_text("一只猫在玩毛线球")], &MemBlobs(vec![])).await
}

async fn err(model: &str, options: &[OptionToken]) -> String {
    prep(model, options).await.unwrap_err().message
}

fn answer(parts: Vec<Part>) -> Turn {
    Turn { role: Role::Assistant, origin: Origin::Live, parts, native: None }
}

fn generated(blob: &BlobRef) -> Part {
    Part::Image { blob: blob.clone(), filename: None, generated: true, dims: None, thought: false }
}

#[tokio::test]
async fn cogvideox_defaults_are_the_best_the_api_has() {
    let p = prep("cogvideox-3", &[]).await.unwrap();
    assert_eq!((p.dialect, p.url.as_str()), (DIALECT, "https://open.bigmodel.cn/api/paas/v4/videos/generations"));
    assert_eq!(
        p.body,
        json!({"model": "cogvideox-3", "prompt": "一只猫在玩毛线球", "quality": "quality", "size": "3840x2160",
               "fps": 60, "with_audio": true, "watermark_enabled": false, "user_id": "user-hash-1"})
    );
    assert_eq!(
        p.settings,
        ["text-to-video", "quality priority", "size 3840x2160", "60 fps", "AI sound effects", "no watermark"]
    );
    assert!(p.applied.iter().all(|a| a.source == "default"));
    assert!(p.warnings.is_empty() && p.notes.is_empty());
    // CogVideoX-2 and Flash: the same fields; Flash is free.
    let p = prep("cogvideox-flash", &[]).await.unwrap();
    assert_eq!((p.body["size"].clone(), p.body["fps"].clone()), (json!("3840x2160"), json!(60)));
    assert!(zp().knows("cogvideox-3") && zp().knows("vidu2-reference") && !zp().knows("cogvideox-9"));
    // The chat and image models are unchanged.
    assert_eq!(prep("glm-5.3", &[]).await.unwrap().dialect, super::super::DIALECT);
    assert_eq!(prep("glm-image", &[]).await.unwrap().dialect, super::super::images::DIALECT);
}

#[tokio::test]
async fn sizes_ratios_lengths_and_quality() {
    let size = |p: Prepared| p.body["size"].as_str().unwrap().to_string();
    assert_eq!(size(prep("cogvideox-3", &[flag("1")]).await.unwrap()), "1280x720");
    assert_eq!(size(prep("cogvideox-3", &[flag("2")]).await.unwrap()), "1920x1080");
    assert_eq!(size(prep("cogvideox-3", &[flag("H")]).await.unwrap()), "1080x1920");
    assert_eq!(size(prep("cogvideox-3", &[flag("1H")]).await.unwrap()), "720x1280");
    assert_eq!(size(prep("cogvideox-3", &[flag("S")]).await.unwrap()), "1024x1024");
    assert_eq!(size(prep("cogvideox-3", &[kv("ar", "256:135")]).await.unwrap()), "2048x1080");
    assert_eq!(size(prep("cogvideox-3", &[kv("ar", "2048:1080")]).await.unwrap()), "2048x1080");
    assert_eq!(size(prep("cogvideox-2", &[flag("w")]).await.unwrap()), "1280x960");
    assert_eq!(size(prep("cogvideox-2", &[kv("ar", "3:2")]).await.unwrap()), "720x480");
    let e = err("cogvideox-3", &[flag("4H")]).await;
    assert!(e.contains("video size 4K 9:16 is not available") && e.contains("sizes: 1280x720, 720x1280"), "{e}");
    assert!(err("cogvideox-3", &[flag("w")]).await.contains("video size 4:3 is not available"));
    assert!(err("cogvideox-3", &[flag("1S")]).await.contains("1K 1:1"));
    let p = prep("cogvideox-3", &[kv("sec", "10"), kv("q", "speed"), flag("2W")]).await.unwrap();
    assert_eq!(
        (p.body["duration"].clone(), p.body["quality"].clone(), p.body["size"].clone()),
        (json!(10), json!("speed"), json!("1920x1080"))
    );
    assert_eq!(
        p.settings,
        ["text-to-video", "speed priority", "size 1920x1080", "60 fps", "10 s", "AI sound effects", "no watermark"]
    );
    let src: Vec<(&str, &str)> = p
        .applied
        .iter()
        .map(|a| (a.source.as_str(), a.patch.as_object().unwrap().keys().next().unwrap().as_str()))
        .collect();
    assert!(src.contains(&("option", "duration")) && src.contains(&("option", "quality")));
    assert!(src.contains(&("option", "size")) && src.contains(&("default", "fps")));
    assert!(err("cogvideox-3", &[kv("sec", "7")]).await.contains("videos of 5 or 10 s (sec=7)"));
    assert!(err("cogvideox-2", &[kv("sec", "5")]).await.contains("does not take a video length (`sec=`)"));
    assert!(err("cogvideox-3", &[kv("q", "hd")]).await.contains("levels: quality, speed"));
    assert!(err("viduq1-text", &[kv("q", "speed")]).await.contains("does not take an image quality (`q=`)"));
    for (opt, what) in [
        (kv("n", "2"), "(`n=`)"),
        (kv("bg", "opaque"), "(`bg=`)"),
        (flag("s"), "web search"),
        (kv("e", "high"), "no reasoning effort"),
        (kv("tier", "flex"), "no service tiers"),
        (kv("p", "x"), "does not apply"),
    ] {
        let e = err("cogvideox-3", &[opt]).await;
        assert!(e.contains(what), "{e}");
    }
    assert!(err("cogvideox-3", &[flag("i")]).await.contains("makes videos by itself"));
}

#[tokio::test]
async fn vidu_models_have_fixed_lengths_and_resolutions() {
    let p = prep("viduq1-text", &[flag("H")]).await.unwrap();
    assert_eq!(
        p.body,
        json!({"model": "viduq1-text", "prompt": "一只猫在玩毛线球", "size": "1920x1080", "aspect_ratio": "9:16",
               "user_id": "user-hash-1"})
    );
    assert_eq!(p.settings, ["text-to-video", "size 1920x1080", "aspect ratio 9:16"]);
    assert_eq!(prep("viduq1-text", &[kv("sec", "5")]).await.unwrap().body["duration"], 5);
    assert!(err("viduq1-text", &[kv("sec", "4")]).await.contains("videos of 5 s (sec=4)"));
    assert!(err("viduq1-text", &[flag("1")]).await.contains("sizes: 2 (1920x1080)"));
    assert!(err("viduq1-text", &[flag("w")]).await.contains("ratios: 16:9, 9:16, 1:1"));
    // Image models: the image's own ratio, background music only where Vidu makes it.
    let (img, b) = photo("image/webp", b"RIFF\0\0\0\0WEBPVP8 ");
    let blobs = MemBlobs(vec![b]);
    let h = vec![user(vec![Part::text("让它动起来"), img.clone()])];
    let p = prep_with("vidu2-image", &[], &h, &blobs).await.unwrap();
    assert!(p.body["image_url"].as_str().unwrap().starts_with("data:image/webp;base64,"));
    assert_eq!((p.body["size"].clone(), p.body["with_audio"].clone()), (json!("1280x720"), json!(true)));
    assert_eq!(p.settings, ["image-to-video", "size 1280x720", "background music"]);
    let p = prep_with("viduq1-image", &[], &h, &blobs).await.unwrap();
    assert!(p.body.get("with_audio").is_none(), "only for 4 s videos: {}", p.body);
    let e = prep_with("viduq1-image", &[flag("W")], &h, &blobs).await.unwrap_err().message;
    assert!(e.contains("has no aspect ratio option"), "{e}");
    let e = prep("viduq1-image", &[]).await.unwrap_err().message;
    assert!(e.contains("takes one image (the first frame); this message has none") && e.contains("separate models"));
    let e = prep_with("viduq1-text", &[], &h, &blobs).await.unwrap_err().message;
    assert!(e.contains("takes a prompt alone; this message has 1 image"), "{e}");
    // vidu2-start-end has a smaller size without a letter.
    let two = vec![user(vec![Part::text("变身"), img.clone(), img.clone()])];
    let p = prep_with("vidu2-start-end", &[flag("1")], &two, &blobs).await.unwrap();
    assert_eq!((p.body["size"].clone(), p.body["image_url"].as_array().unwrap().len()), (json!("1280x720"), 2));
    assert_eq!(p.settings[0], "first and last frame");
    // References: 1–3 images, always an array.
    let one = vec![user(vec![Part::text("这只猫在跳舞"), img.clone()])];
    let p = prep_with("vidu2-reference", &[flag("S")], &one, &blobs).await.unwrap();
    assert_eq!((p.body["image_url"].as_array().unwrap().len(), p.body["aspect_ratio"].clone()), (1, json!("1:1")));
    assert_eq!(p.settings[0], "reference-to-video (1 image)");
    let four = vec![user(vec![Part::text("x"), img.clone(), img.clone(), img.clone(), img])];
    let e = prep_with("vidu2-reference", &[], &four, &blobs).await.unwrap_err().message;
    assert!(e.contains("takes 1–3 reference images; this message has 4 images"), "{e}");
}

#[tokio::test]
async fn images_choose_the_cogvideox_mode() {
    let (a, ba) = photo("image/png", b"\x89PNG-first");
    let (b, bb) = photo("image/jpeg", b"\xff\xd8\xff-last");
    let blobs = MemBlobs(vec![ba, bb]);
    // One image: the first frame; no size unless asked (Zhipu keeps the image's ratio).
    let p = prep_with("cogvideox-3", &[], &[user(vec![Part::text("让画面动起来"), a.clone()])], &blobs).await.unwrap();
    assert!(p.body["image_url"].as_str().unwrap().starts_with("data:image/png;base64,"));
    assert!(p.body.get("size").is_none());
    assert_eq!(p.settings[0], "image-to-video");
    assert!(p.settings.contains(&"size: Zhipu's default (the image's aspect ratio, short side 1080)".to_string()));
    let p = prep_with("cogvideox-3", &[flag("4")], &[user(vec![a.clone()])], &blobs).await.unwrap();
    assert_eq!(p.body["size"], "3840x2160");
    assert!(p.body.get("prompt").is_none(), "an image without text is enough");
    // Two images: the first and the last frame, in order.
    let p = prep_with("cogvideox-3", &[], &[user(vec![Part::text("龙王变成敖丙"), a.clone(), b.clone()])], &blobs)
        .await
        .unwrap();
    let urls = p.body["image_url"].as_array().unwrap();
    assert!(
        urls[0].as_str().unwrap().starts_with("data:image/png")
            && urls[1].as_str().unwrap().starts_with("data:image/jpeg")
    );
    assert_eq!(p.settings[0], "first and last frame");
    let e = prep_with("cogvideox-2", &[], &[user(vec![Part::text("x"), a.clone(), b.clone()])], &blobs)
        .await
        .unwrap_err()
        .message;
    assert!(e.contains("takes a prompt alone or one image (the first frame); this message has 2 images"), "{e}");
    // Formats and sizes per family.
    let (webp, bw) = photo("image/webp", b"RIFF\0\0\0\0WEBPVP8 ");
    let e = prep_with("cogvideox-3", &[], &[user(vec![Part::text("x"), webp])], &MemBlobs(vec![bw])).await.unwrap_err();
    assert!(e.message.contains("formats: png, jpeg"), "{}", e.message);
    let big = BlobRef { sha256: "0".repeat(64), size: 6 * MB, mime: "image/png".into() };
    let big_img = Part::Image { blob: big, filename: None, generated: false, dims: None, thought: false };
    let e = prep_with("cogvideox-3", &[], &[user(vec![Part::text("x"), big_img])], &blobs).await.unwrap_err();
    assert!(e.message.contains("too large for Zhipu (6291456 bytes; the limit is 5 MB)"), "{}", e.message);
}

#[tokio::test]
async fn replies_and_what_zhipu_cannot_take() {
    // A reply to a generated image animates it.
    let prev = BlobRef::for_bytes(b"\x89PNG-gen", "image/png");
    let blobs = MemBlobs(vec![(prev.clone(), b"\x89PNG-gen".to_vec())]);
    let h = vec![Turn::user_text("画一只猫"), answer(vec![generated(&prev)]), Turn::user_text("让它跑起来")];
    let p = prep_with("cogvideox-3", &[], &h, &blobs).await.unwrap();
    assert!(p.body["image_url"].as_str().unwrap().starts_with("data:image/png;base64,"));
    assert!(p.notes.is_empty(), "{:?}", p.notes);
    // A reply to a generated video: Zhipu takes no video, so it cannot be edited (refused).
    let clip = BlobRef::for_bytes(b"\0\0\0\x18ftypmp42", "video/mp4");
    let video = Part::File { blob: clip, filename: Some("v.mp4".into()), generated: true };
    let h = vec![Turn::user_text("一只猫"), answer(vec![video]), Turn::user_text("改成晚上")];
    let e = prep_with("cogvideox-3", &[], &h, &MemBlobs(vec![])).await.unwrap_err();
    assert_eq!(
        e.message,
        "`cogvideox-3` takes no video input, so it cannot edit the video replied to: start a new message for a new \
         video"
    );
    // Earlier text turns: one prompt per request, so they would be ignored (refused).
    let h = vec![Turn::user_text("讲个故事"), Turn::assistant_text("从前"), Turn::user_text("拍出来")];
    let e = prep_with("cogvideox-3", &[], &h, &MemBlobs(vec![])).await.unwrap_err();
    assert_eq!(
        e.message,
        "`cogvideox-3` takes one prompt via Zhipu's video API, not a conversation: start a new message, or reply to a generated image or video it can take as input"
    );
    // An attached video, a long prompt, an empty message.
    let (mp4, bm) = file("a.mp4", "video/mp4", b"\0\0\0\x18ftypmp42");
    let e = prep_with("cogvideox-3", &[], &[user(vec![Part::text("x"), mp4])], &MemBlobs(vec![bm])).await.unwrap_err();
    assert!(e.message.contains("takes no video input"), "{}", e.message);
    let long = "猫".repeat(513);
    let e = prep_with("cogvideox-3", &[], &[Turn::user_text(long.as_str())], &MemBlobs(vec![])).await.unwrap_err();
    assert!(e.message.contains("at most 512 characters"), "{}", e.message);
    let e = prep_with("cogvideox-3", &[], &[user(vec![])], &MemBlobs(vec![])).await.unwrap_err();
    assert!(e.message.contains("needs a prompt"), "{}", e.message);
}

#[tokio::test]
async fn unknown_models_are_not_supported() {
    // Their price is unknown to the bot, whatever the options.
    for id in ["cogvideox-9", "vidu3-text"] {
        for opts in [&[][..], &[flag("W")]] {
            assert_eq!(
                err(id, opts).await,
                format!(
                    "`{id}` is not in the bot's Zhipu BigModel model table (its price is unknown), so it is not supported"
                )
            );
        }
    }
}

// ------------------------------------------------------------------ HTTP (local fake server)

type Seen = Arc<Mutex<Vec<(String, Value)>>>;

/// Serve canned replies to successive connections (`{base}` in a body is the
/// server's own address); returns the address and the request heads seen.
async fn serve(replies: Vec<(u16, &'static str, String)>) -> (String, Seen) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = format!("http://{}", listener.local_addr().unwrap());
    let seen: Seen = Default::default();
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
            let body = body.replace("{base}", &me);
            let resp = format!(
                "HTTP/1.1 {status} X\r\ncontent-type: {ctype}\r\nx-log-id: log-1\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            sock.write_all(resp.as_bytes()).await.unwrap();
            sock.write_all(body.as_bytes()).await.unwrap();
            let _ = sock.shutdown().await;
        }
    });
    (addr, seen)
}

fn heads(seen: &Seen) -> Vec<String> {
    seen.lock().unwrap().iter().map(|(h, _)| h.lines().next().unwrap().to_string()).collect()
}

const MP4: &str = "\0\0\0\x18ftypmp42-video";

fn created() -> String {
    json!({"model": "cogvideox-3", "id": "task-7", "request_id": "r7", "task_status": "PROCESSING"}).to_string()
}

fn processing() -> String {
    json!({"model": "cogvideox-3", "task_status": "PROCESSING", "request_id": "r7"}).to_string()
}

fn success() -> String {
    json!({"model": "cogvideox-3", "task_status": "SUCCESS", "request_id": "r7",
           "video_result": [{"url": "{base}/files/v.mp4", "cover_image_url": "{base}/files/v.jpg"}]})
    .to_string()
}

async fn run(z: &super::super::Zhipu, p: &Prepared) -> (Result<Value, LlmError>, CallOutcome, Vec<StreamEvent>) {
    let mut evs = Vec::new();
    let (r, o) = z.stream(p, &mut |e: StreamEvent| evs.push(e), &crate::NullObserver, &CancellationToken::new()).await;
    (r, o, evs)
}

fn kinds(evs: &[StreamEvent]) -> Vec<String> {
    evs.iter()
        .map(|e| match e {
            StreamEvent::Meta { upstream_id, model, .. } => {
                format!("meta {}", upstream_id.clone().or(model.clone()).unwrap_or_default())
            }
            StreamEvent::Tool(t) => format!("tool {}", t.status.clone().unwrap_or_default()),
            StreamEvent::Checkpoint(c) => format!("checkpoint {c}"),
            StreamEvent::File { mime, name, data } => {
                format!("file {mime} {} {}", name.clone().unwrap_or_default(), data.len())
            }
            StreamEvent::Usage(u) => format!("usage {:?}", u.cost_usd.map(|c| (c * 7.0 * 100.0).round() / 100.0)),
            StreamEvent::Finish { reason, native } => {
                format!("finish {} {}", reason.clone().unwrap_or_default(), native.clone().unwrap_or_default())
            }
            other => format!("{other:?}"),
        })
        .collect()
}

#[tokio::test]
async fn a_video_task_is_created_checkpointed_polled_and_downloaded() {
    let (addr, seen) = serve(vec![
        (200, "application/json", created()),
        (200, "application/json", processing()),
        (200, "application/json", success()),
        (200, "video/mp4", MP4.into()),
    ])
    .await;
    let z = zp_at(&format!("{addr}/api/paas/v4"));
    let p = z
        .prepare(&input("cogvideox-3", &[kv("sec", "10")], &[Turn::user_text("一只猫")]), &[], &MemBlobs(vec![]))
        .await
        .unwrap();
    let (r, outcome, evs) = run(&z, &p).await;
    let native = r.unwrap();
    assert_eq!(native["task_status"], "SUCCESS");
    assert_eq!(native["task"]["id"], "task-7", "the creation response is kept with the result");
    assert_eq!(outcome.upstream_id.as_deref(), Some("task-7"));
    assert_eq!(
        heads(&seen),
        [
            "POST /api/paas/v4/videos/generations HTTP/1.1",
            "GET /api/paas/v4/async-result/task-7 HTTP/1.1",
            "GET /api/paas/v4/async-result/task-7 HTTP/1.1",
            "GET /files/v.mp4 HTTP/1.1",
        ]
    );
    assert_eq!(seen.lock().unwrap()[0].1, p.body);
    assert!(seen.lock().unwrap()[1].0.to_ascii_lowercase().contains("authorization: bearer k"));
    assert!(!seen.lock().unwrap()[3].0.to_ascii_lowercase().contains("authorization"), "no key to the video link");
    assert_eq!(
        kinds(&evs),
        [
            "meta log-1",
            "meta task-7",
            "tool PROCESSING",
            "checkpoint {\"task_id\":\"task-7\"}",
            "meta log-1",
            "meta cogvideox-3",
            "meta log-1",
            "tool SUCCESS",
            &format!("file video/mp4 cogvideox-3-10s.mp4 {}", MP4.len()),
            "meta task-7",
            "usage Some(1.0)",
            "finish stop SUCCESS",
        ]
    );
    let mut acc = crate::Accumulator::new();
    evs.into_iter().for_each(|e| acc.apply(e));
    let Part::ToolCall(t) = &acc.parts()[0] else { panic!("{:?}", acc.parts()) };
    assert_eq!(t.name, "video_generation");
    assert_eq!(
        t.input,
        Some(json!({"prompt": "一只猫", "quality": "quality", "size": "3840x2160", "fps": 60, "duration": 10,
                    "with_audio": true, "watermark_enabled": false}))
    );
    let out = t.output.clone().unwrap();
    assert_eq!(out["video_result"][0], json!({"cover_image_url": format!("{addr}/files/v.jpg")}), "no video link");
    assert_eq!(acc.upstream_id.as_deref(), Some("task-7"), "the task, not a poll's log id");

    // After a restart: the checkpoint polls the same task (no new one, billed once).
    let hop = p.with_continuation(&json!({"task_id": "task-7"}));
    assert_eq!(hop.url, format!("{addr}/api/paas/v4/async-result/task-7"));
    assert_eq!(hop.with_continuation(&json!({"task_id": "task-7"})).url, hop.url, "idempotent");
    let (addr2, seen2) = serve(vec![(200, "application/json", success()), (200, "video/mp4", MP4.into())]).await;
    let z2 = zp_at(&format!("{addr2}/api/paas/v4"));
    let hop2 = Prepared { url: format!("{addr2}/api/paas/v4/videos/generations"), ..p.clone() }
        .with_continuation(&json!({"task_id": "task-7"}));
    let (r, outcome, evs) = run(&z2, &hop2).await;
    assert!(r.is_ok());
    assert_eq!(outcome.upstream_id.as_deref(), Some("task-7"));
    assert_eq!(heads(&seen2)[0], "GET /api/paas/v4/async-result/task-7 HTTP/1.1");
    assert!(!evs.iter().any(|e| matches!(e, StreamEvent::Checkpoint(_))), "no new task");
    assert_eq!(evs.iter().filter(|e| matches!(e, StreamEvent::Usage(_))).count(), 1);
}

#[tokio::test]
async fn failed_tasks_are_not_resumed() {
    let (addr, _) = serve(vec![
        (200, "application/json", created()),
        (
            200,
            "application/json",
            json!({"model": "cogvideox-3", "task_status": "FAIL", "request_id": "r7"}).to_string(),
        ),
    ])
    .await;
    let z = zp_at(&format!("{addr}/api/paas/v4"));
    let p = z.prepare(&input("cogvideox-3", &[], &[Turn::user_text("一只猫")]), &[], &MemBlobs(vec![])).await.unwrap();
    let (r, outcome, evs) = run(&z, &p).await;
    let e = r.unwrap_err();
    // Without a reason: retried, with a new task (the checkpoint is cleared first).
    assert_eq!((e.kind, e.retry, e.error_type.as_deref()), (ErrorKind::InBand, Retry::Backoff, Some("FAIL")));
    assert!(e.message.contains("video task failed without a reason"), "{}", e.message);
    let checkpoints: Vec<&Value> =
        evs.iter().filter_map(|e| if let StreamEvent::Checkpoint(c) = e { Some(c) } else { None }).collect();
    assert_eq!(checkpoints, [&json!({"task_id": "task-7"}), &Value::Null]);
    assert!(outcome.continuation.is_none());
    assert!(evs.iter().any(|e| matches!(e, StreamEvent::Tool(t) if t.status.as_deref() == Some("FAIL"))));

    // A content filter (at creation, HTTP 400): never retried, verbatim.
    let (addr, _) = serve(vec![(
        400,
        "application/json",
        r#"{"error":{"code":"1301","message":"系统检测到输入或生成内容可能包含不安全或敏感内容，请您避免输入易产生敏感内容的提示语，感谢您的配合。"}}"#.into(),
    )])
    .await;
    let z = zp_at(&format!("{addr}/api/paas/v4"));
    let p = Prepared { url: format!("{addr}/api/paas/v4/videos/generations"), ..p };
    let (r, _, evs) = run(&z, &p).await;
    let e = r.unwrap_err();
    assert_eq!((e.status, e.retry, e.error_type.as_deref()), (Some(400), Retry::Never, Some("1301")));
    assert!(!evs.iter().any(|e| matches!(e, StreamEvent::Checkpoint(_))), "no task was made");
}
