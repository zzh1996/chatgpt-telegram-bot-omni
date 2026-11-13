//! Alibaba video tests: requests per model family, mode and option, the
//! conversation rules (a reply passes Alibaba's own video URL), task runs
//! against a local fake server written per DashScope's documented shapes
//! (create with `X-DashScope-Async`, polls, the download), failures, resuming
//! and cost.

use std::time::Duration;

use omni_core::content::{NativeOutput, Part, Role};
use omni_core::trigger::OptionToken;
use omni_core::{BlobRef, Origin, Turn};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::*;
use crate::ark::video::tests::{VIDEO, kinds, task_server};
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

fn flag(f: &str) -> OptionToken {
    OptionToken::Flag(f.into())
}

fn kv(k: &str, v: &str) -> OptionToken {
    OptionToken::KeyValue(k.into(), v.into())
}

async fn prep_with(
    model: &str,
    options: &[OptionToken],
    history: &[Turn],
    blobs: &Blobs,
) -> Result<Prepared, LlmError> {
    let input = CallInput {
        model,
        options,
        history,
        session_id: None,
        user: Some("u".into()),
        known_context: None,
        max_tokens_override: None,
    };
    ali().prepare(&input, &[], blobs).await
}

async fn prep(model: &str, options: &[OptionToken]) -> Result<Prepared, LlmError> {
    prep_with(model, options, &[Turn::user_text("一只猫")], &Blobs(vec![])).await
}

async fn prep_err(model: &str, options: &[OptionToken]) -> String {
    prep(model, options).await.unwrap_err().message
}

fn user(parts: Vec<Part>) -> Turn {
    Turn { role: Role::User, origin: Origin::Live, parts, native: None }
}

fn photo(bytes: &[u8], mime: &str, generated: bool) -> (Part, (BlobRef, Vec<u8>)) {
    let b = BlobRef::for_bytes(bytes, mime);
    (Part::Image { blob: b.clone(), filename: None, generated, dims: None, thought: false }, (b, bytes.to_vec()))
}

fn file(name: &str, mime: &str, bytes: &[u8]) -> (Part, (BlobRef, Vec<u8>)) {
    let b = BlobRef::for_bytes(bytes, mime);
    (Part::File { blob: b.clone(), filename: Some(name.into()), generated: false }, (b, bytes.to_vec()))
}

/// A finished task as the bot stores it (its last poll).
fn task(url: &str, end: &str) -> Value {
    json!({"request_id": "r-1", "output": {"task_id": "t-prev", "task_status": "SUCCEEDED",
            "submit_time": end, "scheduled_time": end, "end_time": end, "orig_prompt": "一只猫", "video_url": url},
           "usage": {"duration": 5, "input_video_duration": 0, "output_video_duration": 5, "video_count": 1, "SR": 720, "ratio": "16:9"}})
}

fn video_answer(dialect: &str, model: &str, payload: Value) -> Turn {
    let clip = BlobRef::for_bytes(VIDEO, "video/mp4");
    Turn {
        role: Role::Assistant,
        origin: Origin::Live,
        parts: vec![Part::File { blob: clip, filename: Some("v.mp4".into()), generated: true }],
        native: Some(NativeOutput { dialect: dialect.into(), model: model.into(), payload, context_tokens: None }),
    }
}

/// A reply to an Alibaba video whose signed URL expires in an hour.
fn reply_to_video(text: &str, extra: Vec<Part>) -> Vec<Turn> {
    let url = format!(
        "https://dashscope-result-bj.oss-cn-beijing.aliyuncs.com/v.mp4?Expires={}&Signature=x",
        unix_now() + 3600
    );
    let mut parts = vec![Part::text(text)];
    parts.extend(extra);
    vec![
        Turn::user_text("一只猫"),
        video_answer(DIALECT, "happyhorse-1.1-t2v", task(&url, "2026-11-13 10:00:00.000")),
        user(parts),
    ]
}

const PNG: &[u8] = b"\x89PNG\r\n\x1a\nframe";
const JPEG: &[u8] = b"\xFF\xD8\xFF\xE0frame";
const ROOT: &str = "https://dashscope.aliyuncs.com";

// ------------------------------------------------------------------ requests

#[tokio::test]
async fn defaults_are_the_largest_resolution_without_watermark() {
    let p = prep("happyhorse-1.1-t2v", &[]).await.unwrap();
    assert_eq!((p.dialect, p.url.as_str()), (DIALECT, format!("{ROOT}{SYNTHESIS}").as_str()));
    assert_eq!(
        p.body,
        json!({"model": "happyhorse-1.1-t2v", "input": {"prompt": "一只猫"},
               "parameters": {"resolution": "1080P", "watermark": false}})
    );
    assert_eq!(p.settings, ["text-to-video", "resolution 1080P", "with audio", "no watermark"]);
    assert!(p.warnings.is_empty() && p.notes.is_empty(), "{:?} {:?}", p.warnings, p.notes);
    assert_eq!(p.max_tokens(), None);
    // The audio switch where there is one: on (Alibaba's default); `audio=off` a silent video.
    let p = prep("wan3.0-video", &[]).await.unwrap();
    assert_eq!(p.body["parameters"], json!({"resolution": "1080P", "audio": true, "watermark": false}));
    let p = prep("wan3.0-video", &[kv("audio", "off")]).await.unwrap();
    assert_eq!(p.body["parameters"]["audio"], false);
    assert!(p.settings.contains(&"no audio".to_string()), "{:?}", p.settings);
    // No switch: an error (HappyHorse always has sound).
    assert!(prep_err("happyhorse-1.1-t2v", &[kv("audio", "off")]).await.contains("(`audio=`)"));
    // Wan 2.6 flash: a silent video costs half (¥0.15/s at 720P, ¥0.25/s at 1080P).
    let s = |m: &str| spec(m).unwrap();
    assert!((cost_cny(s("wan2.6-i2v-flash"), 10.0, Some("720P"), true) - 1.5).abs() < 1e-9);
    assert!((cost_cny(s("wan2.6-r2v-flash"), 10.0, Some("1080P"), false) - 5.0).abs() < 1e-9);
    assert!((cost_cny(s("wan3.0-video"), 10.0, Some("1080P"), true) - 12.0).abs() < 1e-9, "same price");
    // The older protocol: the largest `size`, 16:9.
    assert_eq!(prep("wan2.6-t2v", &[]).await.unwrap().body["parameters"]["size"], "1920*1080");
    assert_eq!(prep("wanx2.1-t2v-plus", &[]).await.unwrap().body["parameters"]["size"], "1280*720");
    let p = prep("wan2.2-t2v-plus", &[]).await.unwrap();
    assert_eq!(p.settings, ["text-to-video", "size 1920*1080 (1080P 16:9)", "no watermark"], "silent");
    // The workspace domain is the root too.
    let a = ali_at("https://ws-1.cn-beijing.maas.aliyuncs.com/compatible-mode/v1");
    let input = CallInput {
        model: "wan2.7-t2v",
        options: &[],
        history: &[Turn::user_text("x")],
        session_id: None,
        user: None,
        known_context: None,
        max_tokens_override: None,
    };
    let p = a.prepare(&input, &[], &Blobs(vec![])).await.unwrap();
    assert_eq!(p.url, format!("https://ws-1.cn-beijing.maas.aliyuncs.com{SYNTHESIS}"));
    assert!(ali().knows("happyhorse-1.0-video-edit") && ali().knows("wan2.7-r2v-2026-06-12"));
    assert!(!ali().knows("wan2.1-t2v-plus"));
}

#[tokio::test]
async fn options_are_checked_per_model() {
    let p = prep("happyhorse-1.1-t2v", &[kv("size", "1K"), kv("ar", "16:9"), kv("sec", "10")]).await.unwrap();
    assert_eq!(
        p.body["parameters"],
        json!({"resolution": "720P", "ratio": "16:9", "duration": 10, "watermark": false})
    );
    assert_eq!(
        p.settings,
        ["text-to-video", "resolution 720P", "aspect ratio 16:9", "10 s", "with audio", "no watermark"]
    );
    assert_eq!(prep("happyhorse-1.1-t2v", &[kv("ar", "4:5")]).await.unwrap().body["parameters"]["ratio"], "4:5");
    assert!(prep_err("wan2.7-t2v", &[kv("ar", "4:5")]).await.contains("ratios: 16:9, 9:16, 1:1, 4:3, 3:4"));
    assert!(prep_err("happyhorse-1.1-t2v", &[kv("size", "4K")]).await.contains("sizes: 1K (720P), 2K (1080P)"));
    assert!(prep_err("wanx2.1-t2v-turbo", &[kv("size", "2K")]).await.contains("sizes: 1K (720P)"));
    // `size` from tier and ratio.
    assert_eq!(
        prep("wan2.6-t2v", &[kv("size", "1K"), kv("ar", "4:3")]).await.unwrap().body["parameters"]["size"],
        "1088*832"
    );
    assert!(prep_err("wan2.6-t2v", &[kv("ar", "21:9")]).await.contains("at 1080P; ratios: 16:9, 9:16, 1:1, 4:3, 3:4"));
    // Lengths.
    assert_eq!(prep("wan2.5-t2v-preview", &[kv("sec", "10")]).await.unwrap().body["parameters"]["duration"], 10);
    assert!(prep_err("wan2.5-t2v-preview", &[kv("sec", "7")]).await.contains("videos of 5, 10 s"));
    assert!(prep_err("wan2.2-t2v-plus", &[kv("sec", "5")]).await.contains("5-second videos only"));
    assert!(prep_err("wan3.0-video", &[kv("sec", "31")]).await.contains("videos of 2–30 s"));
    assert!(prep_err("happyhorse-1.1-t2v", &[kv("sec", "2")]).await.contains("videos of 3–15 s"));
    // What video models do not take.
    assert!(prep_err("wan3.0-video", &[kv("n", "2")]).await.contains("(`n=`)"));
    assert!(prep_err("wan3.0-video", &[kv("q", "high")]).await.contains("(`q=`)"));
    assert!(prep_err("wan3.0-video", &[kv("tier", "flex")]).await.contains("no service tiers"));
    assert!(prep_err("wan3.0-video", &[kv("e", "none")]).await.contains("cannot turn reasoning off"));
    assert!(prep_err("wan3.0-video", &[flag("s")]).await.contains("has no web search"));
    assert!(prep_err("wan2.7-t2v", &[flag("u")]).await.contains("wan3.0-video does"));
    assert!(prep_err("wan3.0-video", &[flag("i")]).await.contains("is a video model"));
    assert!(prep_err("wan3.0-video", &[kv("p", "x")]).await.contains("does not apply"));
}

#[tokio::test]
async fn images_by_model_family() {
    let (a, ba) = photo(PNG, "image/png", false);
    let (b, bb) = photo(JPEG, "image/jpeg", false);
    let (c, bc) = photo(JPEG, "image/jpg", false);
    let blobs = Blobs(vec![ba, bb, bc]);
    let one = || vec![user(vec![a.clone()])];
    let two = || vec![user(vec![Part::text("x"), a.clone(), b.clone()])];
    // HappyHorse i2v: exactly one first frame; the prompt is optional.
    let p = prep_with("happyhorse-1.1-i2v", &[], &one(), &blobs).await.unwrap();
    assert_eq!(p.body["input"]["media"][0]["type"], "first_frame");
    assert!(p.body["input"]["media"][0]["url"].as_str().unwrap().starts_with("data:image/png;base64,"));
    assert!(p.body["input"].get("prompt").is_none());
    let e = prep("happyhorse-1.1-i2v", &[]).await.unwrap_err();
    assert!(e.message.contains("needs an image (the first frame)"), "{}", e.message);
    let e = prep_with("happyhorse-1.1-i2v", &[], &two(), &blobs).await.unwrap_err();
    assert!(e.message.contains("takes one image"), "{}", e.message);
    // Its ratio follows the image: not sent, noted.
    let p = prep_with("happyhorse-1.1-i2v", &[kv("ar", "16:9")], &one(), &blobs).await.unwrap();
    assert!(p.body["parameters"].get("ratio").is_none());
    assert_eq!(p.notes, ["ℹ️ aspect ratio 16:9 not sent: image-to-video takes the image's aspect ratio"]);
    // References need a prompt.
    let p = prep_with("happyhorse-1.1-r2v", &[], &two(), &blobs).await.unwrap();
    let types: Vec<&str> =
        p.body["input"]["media"].as_array().unwrap().iter().filter_map(|m| m["type"].as_str()).collect();
    assert_eq!(types, ["reference_image", "reference_image"]);
    let e = prep_with("happyhorse-1.1-r2v", &[], &one(), &blobs).await.unwrap_err();
    assert!(e.message.contains("needs a prompt"), "{}", e.message);
    // Wan 2.7 i2v: one or two frames.
    let p = prep_with("wan2.7-i2v", &[], &two(), &blobs).await.unwrap();
    let types: Vec<&str> =
        p.body["input"]["media"].as_array().unwrap().iter().filter_map(|m| m["type"].as_str()).collect();
    assert_eq!(types, ["first_frame", "last_frame"]);
    // Wan 3.0: frames, or references when three or named.
    let p = prep_with("wan3.0-video", &[], &two(), &blobs).await.unwrap();
    assert_eq!(p.settings[0], "image-to-video: first frame + last frame");
    let three = vec![user(vec![Part::text("x"), a.clone(), b.clone(), c.clone()])];
    assert_eq!(
        prep_with("wan3.0-video", &[], &three, &blobs).await.unwrap().settings[0],
        "reference-to-video: 3 images"
    );
    let named = vec![user(vec![Part::text("图1的猫坐在图2的沙发上"), a.clone(), b.clone()])];
    assert_eq!(
        prep_with("wan3.0-video", &[], &named, &blobs).await.unwrap().settings[0],
        "reference-to-video: 2 images"
    );
    // The older protocol: `img_url`; first and last frames on the image2video endpoint.
    let p = prep_with("wan2.6-i2v-flash", &[], &one(), &blobs).await.unwrap();
    assert!(p.body["input"]["img_url"].as_str().unwrap().starts_with("data:image/png;base64,"));
    assert_eq!(p.body["parameters"], json!({"resolution": "1080P", "audio": true, "watermark": false}));
    let p = prep_with("wan2.2-kf2v-flash", &[], &two(), &blobs).await.unwrap();
    assert_eq!(p.url, format!("{ROOT}{IMAGE2VIDEO}"));
    assert!(p.body["input"]["first_frame_url"].is_string() && p.body["input"]["last_frame_url"].is_string());
    assert_eq!(p.body["parameters"]["resolution"], "1080P", "the largest, not the API's 720P");
    // Text-to-video takes no image; Wan 2.6 r2v no base64.
    let e = prep_with("wan2.7-t2v", &[], &two(), &blobs).await.unwrap_err();
    assert!(e.message.contains("is text-to-video"), "{}", e.message);
    let e = prep_with("wan2.6-r2v", &[], &two(), &blobs).await.unwrap_err();
    assert!(e.message.contains("references only as URLs"), "{}", e.message);
    // HappyHorse takes no BMP; Wan does.
    let (bmp, bbmp) = photo(b"BMxx", "image/bmp", false);
    let e =
        prep_with("happyhorse-1.1-i2v", &[], &[user(vec![bmp.clone()])], &Blobs(vec![bbmp.clone()])).await.unwrap_err();
    assert!(e.message.contains("formats: jpeg, png, webp"), "{}", e.message);
    assert!(prep_with("wan2.7-i2v", &[], &[user(vec![bmp])], &Blobs(vec![bbmp])).await.is_ok());
    // A replied-to generated image is first.
    let (generated, bg) = photo(b"\x89PNG-generated", "image/png", true);
    let answer = Turn { role: Role::Assistant, origin: Origin::Live, parts: vec![generated], native: None };
    let history = vec![Turn::user_text("画猫"), answer, user(vec![Part::text("动起来")])];
    let p = prep_with("happyhorse-1.1-i2v", &[], &history, &Blobs(vec![bg])).await.unwrap();
    assert_eq!(p.settings[0], "image-to-video: first frame (the previous image)");
}

#[tokio::test]
async fn a_reply_to_an_alibaba_video_passes_its_url() {
    let url = |p: &Prepared, i: usize| p.body["input"]["media"][i]["url"].as_str().unwrap().to_string();
    // Edits.
    let p = prep_with("wan2.7-videoedit", &[], &reply_to_video("换成黏土风格", vec![]), &Blobs(vec![])).await.unwrap();
    assert_eq!(p.body["input"]["media"][0]["type"], "video");
    assert!(url(&p, 0).starts_with("https://dashscope-result-bj.oss-cn-beijing.aliyuncs.com/v.mp4?Expires="));
    assert_eq!(p.settings[0], "video edit of the previous video");
    let (img, bi) = photo(PNG, "image/png", false);
    let p = prep_with(
        "happyhorse-1.0-video-edit",
        &[kv("ar", "16:9")],
        &reply_to_video("换衣服", vec![img.clone()]),
        &Blobs(vec![bi.clone()]),
    )
    .await
    .unwrap();
    let types: Vec<&str> =
        p.body["input"]["media"].as_array().unwrap().iter().filter_map(|m| m["type"].as_str()).collect();
    assert_eq!(types, ["video", "reference_image"]);
    assert_eq!(p.notes, ["ℹ️ aspect ratio 16:9 not sent: an edit keeps the source video's aspect ratio"]);
    assert!(
        prep_with("happyhorse-1.0-video-edit", &[kv("sec", "5")], &reply_to_video("x", vec![]), &Blobs(vec![]))
            .await
            .unwrap_err()
            .message
            .contains("keeps the source video's length")
    );
    // Continuing (Wan 2.7 i2v), to a last frame.
    let p = prep_with("wan2.7-i2v", &[], &reply_to_video("继续", vec![img.clone()]), &Blobs(vec![bi.clone()]))
        .await
        .unwrap();
    let types: Vec<&str> =
        p.body["input"]["media"].as_array().unwrap().iter().filter_map(|m| m["type"].as_str()).collect();
    assert_eq!(types, ["first_clip", "last_frame"]);
    assert_eq!(p.settings[0], "continuing the previous video to a last frame");
    // References (Wan 2.7 r2v: 2–10 s with a video).
    let p = prep_with("wan2.7-r2v", &[kv("sec", "10")], &reply_to_video("视频1在跳舞", vec![]), &Blobs(vec![]))
        .await
        .unwrap();
    assert_eq!(p.body["input"]["media"][0]["type"], "reference_video");
    let e =
        prep_with("wan2.7-r2v", &[kv("sec", "12")], &reply_to_video("x", vec![]), &Blobs(vec![])).await.unwrap_err();
    assert!(e.message.contains("2–10 s with a reference video"), "{}", e.message);
    let p =
        prep_with("wan2.6-r2v-flash", &[], &reply_to_video("character1 跳舞", vec![]), &Blobs(vec![])).await.unwrap();
    assert_eq!(p.body["input"]["reference_urls"].as_array().unwrap().len(), 1);
    // Wan 3.0: everything a reference; its ratio and length are Wan's.
    let p = prep_with(
        "wan3.0-video",
        &[kv("ar", "16:9"), kv("sec", "8")],
        &reply_to_video("向后延长", vec![img]),
        &Blobs(vec![bi]),
    )
    .await
    .unwrap();
    let types: Vec<&str> =
        p.body["input"]["media"].as_array().unwrap().iter().filter_map(|m| m["type"].as_str()).collect();
    assert_eq!(types, ["reference_video", "reference_image"]);
    assert!(p.body["parameters"].get("ratio").is_none() && p.body["parameters"].get("duration").is_none());
    assert!(p.notes[0].starts_with("ℹ️ aspect ratio 16:9, sec=8 not sent"), "{:?}", p.notes);
    // Text-to-video takes no video.
    let e = prep_with("happyhorse-1.1-t2v", &[], &reply_to_video("x", vec![]), &Blobs(vec![])).await.unwrap_err();
    assert!(e.message.contains("takes no video"), "{}", e.message);
    // An edit model needs the video.
    let e = prep("wan2.7-videoedit", &[]).await.unwrap_err();
    assert!(e.message.contains("edits a video: reply to a video Alibaba made"), "{}", e.message);
}

#[tokio::test]
async fn videos_alibaba_cannot_take() {
    let now = unix_now();
    // An expired signed URL, or 24 hours after the task's end (Beijing time).
    let gone = format!("https://dashscope-result-bj.oss-cn-beijing.aliyuncs.com/v.mp4?Expires={}", now - 10);
    let history = vec![
        Turn::user_text("猫"),
        video_answer(DIALECT, "wan3.0-video", task(&gone, "2026-11-13 10:00:00.000")),
        Turn::user_text("x"),
    ];
    let e = prep_with("wan2.7-videoedit", &[], &history, &Blobs(vec![])).await.unwrap_err();
    assert!(e.message.contains("its URL has expired (DashScope keeps generated videos 24 hours)"), "{}", e.message);
    let old = vec![
        Turn::user_text("猫"),
        video_answer(DIALECT, "wan3.0-video", task("https://x/v.mp4", "2026-04-20 17:56:36.658")),
        Turn::user_text("x"),
    ];
    assert!(prep_with("wan2.7-videoedit", &[], &old, &Blobs(vec![])).await.unwrap_err().message.contains("expired"));
    // Another platform's video.
    let ark = video_answer("ark.videos", "doubao-seedance-2-5-260628", json!({"content": {"video_url": "https://x"}}));
    let history = vec![Turn::user_text("猫"), ark, Turn::user_text("x")];
    let e = prep_with("wan2.7-videoedit", &[], &history, &Blobs(vec![])).await.unwrap_err();
    assert!(e.message.contains("not this one (made by `doubao-seedance-2-5-260628`)"), "{}", e.message);
    // Attached videos and audio: URLs only.
    let (mp4, bv) = file("clip.mp4", "video/mp4", VIDEO);
    let e =
        prep_with("wan2.7-videoedit", &[], &[user(vec![Part::text("x"), mp4])], &Blobs(vec![bv])).await.unwrap_err();
    assert!(e.message.contains("takes videos only as URLs"), "{}", e.message);
    let (mp3, ba) = file("a.mp3", "audio/mpeg", b"ID3");
    let e = prep_with("wan3.0-video", &[], &[user(vec![Part::text("x"), mp3])], &Blobs(vec![ba])).await.unwrap_err();
    assert!(e.message.contains("takes audio only as URLs"), "{}", e.message);
}

#[tokio::test]
async fn a_web_page_for_wan_3() {
    let history = vec![Turn::user_text("把这篇文章做成短片 https://example.com/post/1。")];
    let p = prep_with("wan3.0-video", &[flag("u")], &history, &Blobs(vec![])).await.unwrap();
    assert_eq!(p.body["input"]["media"], json!([{"type": "link", "url": "https://example.com/post/1"}]));
    assert_eq!(p.settings[0], "reference-to-video: a web page");
    assert!(prep_err("wan3.0-video", &[flag("u")]).await.contains("the message has no URL"));
    let two = vec![Turn::user_text("https://a.com https://b.com")];
    let e = prep_with("wan3.0-video", &[flag("u")], &two, &Blobs(vec![])).await.unwrap_err();
    assert!(e.message.contains("reads one web page"), "{}", e.message);
}

/// Ids outside the table (other tools, other regions' names, newer ids of the
/// families) are not supported: their price is unknown.
#[tokio::test]
async fn models_the_bot_does_not_serve() {
    for id in [
        "wan2.2-animate-move",
        "wanx2.1-vace-plus",
        "wan2.2-s2v",
        "videoretalk",
        "video-style-transform",
        "wan2.1-t2v-plus",
        "happyhorse-1.2-t2v",
        "wan2.8-t2v",
    ] {
        assert_eq!(
            prep_err(id, &[]).await,
            format!(
                "`{id}` is not in the bot's Alibaba Model Studio model table (its price is unknown), so it is not supported"
            )
        );
    }
}

#[test]
fn task_times_are_beijing_time() {
    // 2026-04-20 17:56:36 UTC+8 = 09:56:36 UTC.
    assert_eq!(beijing_time("2026-04-20 17:56:36.658"), Some(1_776_678_996));
    assert_eq!(days_from_civil(2000, 2, 29) * 86_400, 951_782_400);
    assert_eq!(beijing_time("not a time"), None);
}

// ------------------------------------------------------------------ tasks

fn pending(status: &str) -> Value {
    json!({"request_id": "r-2", "output": {"task_id": "t-1", "task_status": status,
            "submit_time": "2026-11-13 17:55:17.075", "scheduled_time": "2026-11-13 17:55:17.129"}})
}

fn succeeded() -> Value {
    json!({"request_id": "r-3", "output": {"task_id": "t-1", "task_status": "SUCCEEDED",
            "submit_time": "2026-11-13 17:55:17.075", "scheduled_time": "2026-11-13 17:55:17.129",
            "end_time": "2026-11-13 17:56:36.658", "orig_prompt": "一只猫", "video_url": "DOWNLOAD"},
           "usage": {"duration": 5, "input_video_duration": 0, "output_video_duration": 5, "video_count": 1, "SR": 720, "ratio": "16:9"}})
}

fn created() -> (u16, Value) {
    (200, json!({"output": {"task_status": "PENDING", "task_id": "t-1"}, "request_id": "r-1"}))
}

async fn prepared(origin: &str, model: &str, options: &[OptionToken]) -> Prepared {
    let p = prep(model, options).await.unwrap();
    let path = p.url.trim_start_matches(ROOT).to_string();
    Prepared { url: format!("{origin}{path}"), ..p }
}

async fn run(p: &Prepared, origin: &str) -> (Result<Value, LlmError>, CallOutcome, Vec<StreamEvent>) {
    let a = ali_at(&format!("{origin}/compatible-mode/v1"));
    let mut evs = Vec::new();
    let mut sink = |e: StreamEvent| evs.push(e);
    let (r, outcome) = a.stream(p, &mut sink, &crate::NullObserver, &CancellationToken::new()).await;
    (r, outcome, evs)
}

#[tokio::test]
async fn a_task_is_created_asynchronously_polled_and_downloaded() {
    let (origin, seen) =
        task_server(created(), vec![pending("PENDING"), pending("RUNNING"), succeeded()], "/output/video_url").await;
    let p = prepared(&origin, "happyhorse-1.1-t2v", &[kv("size", "1K")]).await;
    let (r, outcome, evs) = run(&p, &origin).await;
    let native = r.unwrap();
    assert_eq!(native["output"]["video_url"], format!("{origin}/files/video.mp4"), "kept for edits of replies");
    assert_eq!(
        (outcome.upstream_id.as_deref(), outcome.continuation.clone()),
        (Some("t-1"), Some(json!({"task_id": "t-1"})))
    );
    let reqs = seen.lock().unwrap().clone();
    let lines: Vec<&str> = reqs.iter().map(|r| r.line.as_str()).collect();
    assert_eq!(
        lines,
        [
            "POST /api/v1/services/aigc/video-generation/video-synthesis",
            "GET /api/v1/tasks/t-1",
            "GET /api/v1/tasks/t-1",
            "GET /api/v1/tasks/t-1",
            "GET /files/video.mp4"
        ]
    );
    assert_eq!((reqs[0].auth.as_deref(), reqs[0].header("x-dashscope-async")), (Some("Bearer k"), Some("enable")));
    assert_eq!(reqs[1].header("x-dashscope-async"), None, "polls are plain GETs");
    assert_eq!(reqs[4].auth, None, "no credentials are sent to the download URL");
    assert_eq!(reqs[0].body, p.body);
    // 5 s at 720P, ¥0.9 / s, ¥7 / USD.
    let cost = format!("usage {:?}", Some((5.0 * 0.9 / 7.0 * 1e6_f64).round() / 1e6));
    assert_eq!(
        kinds(&evs),
        [
            "meta t-1",
            "video_generation PENDING",
            "checkpoint {\"task_id\":\"t-1\"}",
            "video_generation RUNNING",
            "video_generation SUCCEEDED",
            &format!("file video/mp4 happyhorse-1.1-t2v-5s.mp4 {}", VIDEO.len()),
            &cost,
            "finish stop SUCCEEDED",
        ]
    );
    let mut acc = crate::Accumulator::new();
    evs.iter().cloned().for_each(|e| acc.apply(e));
    let Part::ToolCall(t) = &acc.parts()[0] else { panic!("{:?}", acc.parts()) };
    assert_eq!(t.input, Some(json!({"prompt": "一只猫", "resolution": "720P", "watermark": false})));
    assert_eq!(
        t.output,
        Some(json!({"usage": {"duration": 5, "input_video_duration": 0, "output_video_duration": 5, "video_count": 1,
                              "SR": 720, "ratio": "16:9"}}))
    );
}

#[tokio::test]
async fn edits_show_as_video_edit_and_resume_from_the_checkpoint() {
    let (origin, seen) = task_server(created(), vec![succeeded()], "/output/video_url").await;
    let p = prep_with("wan2.7-videoedit", &[], &reply_to_video("黏土风格", vec![]), &Blobs(vec![])).await.unwrap();
    let p = Prepared { url: format!("{origin}{SYNTHESIS}"), ..p };
    let resumed = p.with_continuation(&json!({"task_id": "t-1"}));
    assert_eq!((resumed.url.clone(), resumed.body.clone()), (format!("{origin}/api/v1/tasks/t-1"), Value::Null));
    let (r, outcome, evs) = run(&resumed, &origin).await;
    assert!(r.is_ok());
    assert_eq!(outcome.upstream_id.as_deref(), Some("t-1"));
    let lines: Vec<String> = seen.lock().unwrap().iter().map(|r| r.line.clone()).collect();
    assert_eq!(lines, ["GET /api/v1/tasks/t-1", "GET /files/video.mp4"], "no new task");
    assert!(kinds(&evs).contains(&"video_edit SUCCEEDED".to_string()));
    assert!(!evs.iter().any(|e| matches!(e, StreamEvent::Checkpoint(_))));
    // Not a video checkpoint: unchanged.
    assert_eq!(continuation_request(&p, &json!({"x": 1})).url, p.url);
}

#[tokio::test]
async fn failed_unknown_and_cancelled_tasks() {
    let failed = json!({"request_id": "r", "output": {"task_id": "t-1", "task_status": "FAILED",
        "code": "DataInspectionFailed", "message": "Output data may contain inappropriate content."}});
    let (origin, _) = task_server(created(), vec![failed.clone()], "/x").await;
    let p = prepared(&origin, "wan3.0-video", &[]).await;
    let (r, outcome, evs) = run(&p, &origin).await;
    let e = r.unwrap_err();
    assert_eq!(
        (e.kind, e.retry, e.error_type.as_deref()),
        (ErrorKind::InBand, Retry::Never, Some("DataInspectionFailed"))
    );
    assert_eq!(e.message, "Output data may contain inappropriate content.");
    assert_eq!(e.details, Some(failed));
    assert!(outcome.continuation.is_none());
    let k = kinds(&evs);
    assert_eq!(&k[k.len() - 2..], ["video_generation FAILED", "checkpoint null"]);
    let Some(StreamEvent::Tool(t)) = evs.iter().rev().find(|e| matches!(e, StreamEvent::Tool(_))) else { panic!() };
    assert_eq!(
        t.output,
        Some(
            json!({"error": {"code": "DataInspectionFailed", "message": "Output data may contain inappropriate content."}})
        )
    );
    // A server-side failure: a new task may succeed.
    let busy = json!({"output": {"task_id": "t-1", "task_status": "FAILED", "code": "InternalError", "message": "x"}});
    let (origin, _) = task_server(created(), vec![busy], "/x").await;
    let p = prepared(&origin, "wan3.0-video", &[]).await;
    assert_eq!(run(&p, &origin).await.0.unwrap_err().retry, Retry::Backoff);
    // Gone (after 24 hours) or cancelled.
    let (origin, _) = task_server(created(), vec![pending("UNKNOWN")], "/x").await;
    let resumed = prepared(&origin, "wan3.0-video", &[]).await.with_continuation(&json!({"task_id": "t-1"}));
    let (r, _, evs) = run(&resumed, &origin).await;
    let e = r.unwrap_err();
    assert_eq!((e.retry, e.error_type.as_deref()), (Retry::Backoff, Some("UNKNOWN")));
    assert!(e.message.contains("task ids are kept 24 hours"), "{}", e.message);
    assert!(matches!(evs.last(), Some(StreamEvent::Checkpoint(Value::Null))));
    let (origin, _) = task_server(created(), vec![pending("CANCELED")], "/x").await;
    let p = prepared(&origin, "wan3.0-video", &[]).await;
    assert_eq!(run(&p, &origin).await.0.unwrap_err().retry, Retry::Never);
    // A creation error: DashScope's `{code, message, request_id}`.
    let bad = json!({"code": "InvalidParameter", "message": "The size is not match xxxxxx", "request_id": "r"});
    let (origin, _) = task_server((400, bad.clone()), vec![], "/x").await;
    let p = prepared(&origin, "wan3.0-video", &[]).await;
    let (r, outcome, evs) = run(&p, &origin).await;
    let e = r.unwrap_err();
    assert_eq!((e.status, e.retry, e.error_type.as_deref()), (Some(400), Retry::Never, Some("InvalidParameter")));
    assert_eq!(e.details, Some(bad));
    assert!(evs.is_empty() && outcome.continuation.is_none());
}

#[test]
fn cost_by_billed_seconds_and_resolution() {
    let s = |id| spec(id).unwrap();
    // Wan 3.0 with an input video bills input + output (`duration`).
    let t =
        json!({"usage": {"duration": 15.0, "input_video_duration": 5.0, "output_video_duration": 10.0, "SR": 1080}});
    assert_eq!(billed_seconds(&t["usage"]), Some(15.0));
    assert_eq!(billed_tier(&t, &Value::Null).as_deref(), Some("1080P"));
    assert!((cost_cny(s("wan3.0-video"), 15.0, Some("1080P"), false) - 18.0).abs() < 1e-9);
    // Wan 2.2: `video_duration` and the requested resolution.
    let t = json!({"usage": {"video_duration": 5, "video_count": 1}});
    let body = json!({"parameters": {"resolution": "480P"}});
    assert_eq!(billed_tier(&t, &body).as_deref(), Some("480P"));
    assert!((cost_cny(s("wan2.2-i2v-flash"), 5.0, Some("480P"), false) - 0.5).abs() < 1e-9);
    // The older `size`.
    let body = json!({"parameters": {"size": "832*480"}});
    assert_eq!(billed_tier(&json!({"usage": {"video_duration": 5}}), &body).as_deref(), Some("480P"));
    // Unknown resolution: the highest price.
    assert!((cost_cny(s("happyhorse-1.1-t2v"), 2.0, None, false) - 2.4).abs() < 1e-9);
}
