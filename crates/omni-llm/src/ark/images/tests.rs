//! Seedream tests: requests per model and option, the conversation rules,
//! responses (whole and streamed) written per Ark's documented shapes, cost,
//! and the HTTP path against a local fake server.

use std::time::Duration;

use base64::Engine as _;
use omni_core::content::{Part, Role};
use omni_core::trigger::OptionToken;
use omni_core::{BlobRef, Origin, Turn};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::*;
use crate::chatstream::EventParser;
use crate::provider::Provider;

const PNG: &[u8] = b"\x89PNG\r\n\x1a\nimage-bytes";
const JPEG: &[u8] = b"\xFF\xD8\xFF\xE0jpeg-bytes";

struct Blobs(Vec<(BlobRef, Vec<u8>)>);

impl BlobLoader for Blobs {
    fn load<'a>(&'a self, r: &'a BlobRef) -> crate::BoxFuture<'a, anyhow::Result<Vec<u8>>> {
        let found = self.0.iter().find(|(b, _)| b == r).map(|(_, d)| d.clone());
        Box::pin(async move { found.ok_or_else(|| anyhow::anyhow!("no blob")) })
    }
}

fn blob(bytes: &[u8], mime: &str) -> (BlobRef, Vec<u8>) {
    (BlobRef::for_bytes(bytes, mime), bytes.to_vec())
}

fn ark_at(base: &str) -> crate::ark::Ark {
    crate::ark::Ark::new(ArkConfig {
        api_key: "k".into(),
        base_url: base.into(),
        connect_timeout: Duration::from_secs(1),
        idle_timeout: Duration::from_secs(5),
        max_tokens_cap: 65_536,
        cny_per_usd: 7.0,
    })
    .unwrap()
}

fn ark() -> crate::ark::Ark {
    ark_at(crate::ark::DEFAULT_BASE_URL)
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
        user: Some("u".repeat(64)),
        known_context: None,
        max_tokens_override: None,
    }
}

async fn prep_with(
    model: &str,
    options: &[OptionToken],
    history: &[Turn],
    blobs: &Blobs,
) -> Result<Prepared, LlmError> {
    ark().prepare(&input(model, options, history), &[], blobs).await
}

async fn prep(model: &str, options: &[OptionToken]) -> Prepared {
    prep_with(model, options, &[Turn::user_text("一只猫")], &Blobs(vec![])).await.unwrap()
}

async fn prep_err(model: &str, options: &[OptionToken]) -> String {
    prep_with(model, options, &[Turn::user_text("一只猫")], &Blobs(vec![])).await.unwrap_err().message
}

const S55: &str = "doubao-seedream-5-5-261028";
const PRO: &str = "doubao-seedream-5-0-pro-260628";
const FLASH: &str = "doubao-seedream-5-0-flash-260915";
const LITE_ID: &str = "doubao-seedream-5-0-260128";
const S45: &str = "doubao-seedream-4-5-251128";
const S40: &str = "doubao-seedream-4-0-250828";

// ------------------------------------------------------------------ requests

#[tokio::test]
async fn defaults_are_the_largest_size_without_watermark() {
    let p = prep(S55, &[]).await;
    assert_eq!(p.dialect, DIALECT);
    assert_eq!(p.url, "https://ark.cn-beijing.volces.com/api/v3/images/generations");
    assert_eq!(
        p.body,
        json!({
            "model": S55,
            "prompt": "一只猫",
            "size": "2K",
            "response_format": "b64_json",
            "output_format": "png",
            "watermark": false,
            "safety_identifier": "u".repeat(64),
        })
    );
    assert_eq!(p.settings, vec!["size 2K", "png", "no watermark"]);
    assert!(p.warnings.is_empty() && p.notes.is_empty(), "{:?} {:?}", p.warnings, p.notes);
    assert_eq!(p.max_tokens(), None);
    assert!(ark().knows(S55) && ark().knows("doubao-seedream-5-0-lite-260128"));

    // The group models: 4K, one image unless n= asks, streamed, jpeg only on 4.x.
    let p = prep(S45, &[]).await;
    assert_eq!(
        p.body,
        json!({
            "model": S45,
            "prompt": "一只猫",
            "size": "4K",
            "sequential_image_generation": "disabled",
            "stream": true,
            "response_format": "b64_json",
            "watermark": false,
        })
    );
    assert_eq!(p.settings, vec!["size 4K", "no watermark"]);
    assert!(p.warnings[0].contains("2026-11-24") && p.warnings[0].contains(PRO), "{:?}", p.warnings);
    let p = prep(LITE_ID, &[]).await;
    assert_eq!((p.body["size"].as_str(), p.body["output_format"].as_str()), (Some("4K"), Some("png")));
    assert!(p.body.get("safety_identifier").is_none(), "not documented for 5.0 lite");
    assert_eq!(prep(S40, &[]).await.body["size"], "4K");
    assert_eq!(prep(FLASH, &[]).await.body["size"], "2K");
}

#[tokio::test]
async fn sizes_and_ratios() {
    // Ark's tier × ratio table.
    assert_eq!(prep(S55, &[flag("W")]).await.body["size"], "2816x1584");
    assert_eq!(prep(S55, &[flag("1h")]).await.body["size"], "864x1152");
    assert_eq!(prep(LITE_ID, &[flag("2W")]).await.body["size"], "2848x1600");
    assert_eq!(prep(S40, &[flag("1W")]).await.body["size"], "1280x720");
    assert_eq!(prep(S45, &[kv("ar", "21:9")]).await.body["size"], "6240x2656");
    let p = prep(PRO, &[flag("1")]).await;
    assert_eq!((p.body["size"].as_str(), p.settings[0].as_str()), (Some("1K"), "size 1K"));
    let p = prep(S55, &[flag("H")]).await;
    assert_eq!(p.settings[0], "size 1584x2816 (2K 9:16)");
    assert_eq!(p.applied[0].source, "option");
    // Other ratios: the exact ratio at the tier's pixel count.
    assert_eq!(prep(S55, &[kv("ar", "5:4")]).await.body["size"], "2285x1828");
    assert_eq!(prep(S55, &[kv("ar", "10:8")]).await.body["size"], "2285x1828", "reduced first");
    assert_eq!(prep(S45, &[kv("ar", "1:4")]).await.body["size"], "2048x8192");
    // Outside what Ark takes.
    assert!(prep_err(S55, &[kv("ar", "1:20")]).await.contains("1:16 to 16:1"));
    let e = prep_err(S45, &[flag("1")]).await;
    assert!(e.contains("sizes: 2K, 4K"), "{e}");
    let e = prep_err(S55, &[flag("4")]).await;
    assert!(e.contains("sizes: 1K, 2K"), "{e}");
    assert_eq!(dimensions(spec(S55).unwrap(), "1.5K", "16:9"), Ok((2048, 1152)));
}

#[tokio::test]
async fn groups_background_quality_search() {
    let p = prep(LITE_ID, &[kv("n", "4")]).await;
    assert_eq!(p.body["sequential_image_generation"], "auto");
    assert_eq!(p.body["sequential_image_generation_options"], json!({"max_images": 4}));
    assert_eq!(p.body["stream"], true);
    assert!(p.settings.contains(&"image group: up to 4 images".to_string()), "{:?}", p.settings);
    assert_eq!(prep(S45, &[kv("n", "1")]).await.body["sequential_image_generation"], "disabled");
    let e = prep_err(S55, &[kv("n", "2")]).await;
    assert!(e.contains("(`n=`)"), "{e}");
    assert!(prep_err(S40, &[kv("n", "16")]).await.contains("at most 15"));

    let p = prep(PRO, &[kv("bg", "transparent"), kv("q", "fast")]).await;
    assert_eq!(p.body["background"], "transparent");
    assert_eq!(p.body["optimize_prompt_options"], json!({"mode": "fast"}));
    assert!(p.settings.contains(&"background transparent".to_string()));
    assert!(p.settings.contains(&"prompt optimization fast".to_string()));
    assert_eq!(prep(S55, &[kv("bg", "auto")]).await.body["background"], "auto");
    let e = prep_err(PRO, &[kv("bg", "auto")]).await;
    assert!(e.contains("backgrounds: transparent, opaque"), "{e}");
    assert!(prep_err(S45, &[kv("bg", "opaque")]).await.contains("(`bg=`)"));
    let e = prep_err(S55, &[kv("q", "fast")]).await;
    assert!(e.contains("modes: standard"), "{e}");
    assert_eq!(prep(S40, &[kv("q", "fast")]).await.body["optimize_prompt_options"]["mode"], "fast");

    // Web search (5.0 lite): not streamed, so the search shows before the images.
    let p = prep(LITE_ID, &[flag("s")]).await;
    assert_eq!(p.body["tools"], json!([{"type": "web_search"}]));
    assert!(p.body.get("stream").is_none());
    assert!(p.warnings.iter().any(|w| w.contains("web search")), "{:?}", p.warnings);
    assert!(prep_err(S55, &[flag("s")]).await.contains("only Seedream 5.0 lite searches"));
    assert!(prep_err(S55, &[flag("i")]).await.contains("draws without `+i`"));
    assert!(prep_err(S55, &[flag("c")]).await.contains("has no code execution"));
    assert!(prep_err(S55, &[kv("e", "high")]).await.contains("reasoning effort"));
    assert!(prep_err(S55, &[kv("p", "x")]).await.contains("`p=`"));
    assert!(prep_err(S55, &[kv("tier", "flex")]).await.contains("service tiers"));
    assert!(prep_err(S55, &[kv("sec", "5")]).await.contains("(`sec=`)"));
}

fn generated(blob: BlobRef) -> Part {
    Part::Image { blob, filename: None, generated: true, dims: Some((2048, 2048)), thought: false }
}

#[tokio::test]
async fn replies_edit_the_generated_images() {
    let out = blob(PNG, "image/png");
    let draft = blob(b"draft", "image/png");
    let photo = blob(JPEG, "image/jpeg");
    let blobs = Blobs(vec![out.clone(), draft.clone(), photo.clone()]);
    let answer = Turn {
        role: Role::Assistant,
        origin: Origin::Live,
        parts: vec![
            Part::Image { blob: draft.0.clone(), filename: None, generated: true, dims: None, thought: true },
            generated(out.0.clone()),
        ],
        native: None,
    };
    let reply = Turn {
        role: Role::User,
        origin: Origin::Live,
        parts: vec![
            Part::text("改成蓝色"),
            Part::Image { blob: photo.0.clone(), filename: None, generated: false, dims: None, thought: false },
            Part::text("加一顶帽子"),
        ],
        native: None,
    };
    let history = vec![Turn::user_text("画一只猫"), answer, reply];
    let p = prep_with(S55, &[], &history, &blobs).await.unwrap();
    // The replied-to image first (the draft made while thinking is not an answer), then the attached one.
    assert_eq!(
        p.body["image"],
        json!([
            format!("data:image/png;base64,{}", crate::provider::b64(PNG)),
            format!("data:image/jpeg;base64,{}", crate::provider::b64(JPEG)),
        ])
    );
    assert_eq!(p.body["prompt"], "改成蓝色\n加一顶帽子");
    assert!(p.settings.contains(&"editing the replied-to image + 1 reference image".to_string()), "{:?}", p.settings);
    assert!(p.notes.is_empty(), "editing is what is sent: {:?}", p.notes);
    // A new request: only its own images.
    let fresh = vec![Turn {
        role: Role::User,
        origin: Origin::Live,
        parts: vec![
            Part::File { blob: photo.0.clone(), filename: Some("p.jpg".into()), generated: false },
            Part::text("变成油画"),
        ],
        native: None,
    }];
    let p = prep_with(S55, &[], &fresh, &blobs).await.unwrap();
    assert_eq!(p.body["image"], json!(format!("data:image/jpeg;base64,{}", crate::provider::b64(JPEG))));
    assert!(p.settings.contains(&"1 reference image".to_string()));
    // Switched here from a chat answer (no images): the conversation is not sent, and the bot says so.
    let switched = vec![Turn::user_text("讲个故事"), Turn::assistant_text("从前……"), Turn::user_text("画出来")];
    let p = prep_with(S55, &[], &switched, &blobs).await.unwrap();
    assert!(p.body.get("image").is_none());
    assert_eq!(p.body["prompt"], "画出来");
    assert!(p.notes[0].contains("no conversation"), "{:?}", p.notes);
}

#[tokio::test]
async fn inputs_the_api_does_not_take() {
    let none = Blobs(vec![]);
    let system = vec![
        Turn { role: Role::System, origin: Origin::Synthetic, parts: vec![Part::text("你是画家")], native: None },
        Turn::user_text("画猫"),
    ];
    assert!(prep_with(S55, &[], &system, &none).await.unwrap_err().message.contains("no system prompt"));
    let pdf = blob(b"%PDF", "application/pdf");
    let doc = vec![Turn {
        role: Role::User,
        origin: Origin::Live,
        parts: vec![
            Part::text("x"),
            Part::File { blob: pdf.0.clone(), filename: Some("a.pdf".into()), generated: false },
        ],
        native: None,
    }];
    let e = prep_with(S55, &[], &doc, &Blobs(vec![pdf])).await.unwrap_err();
    assert!(e.message.contains("text and images only"), "{e}");
    let here = vec![Turn {
        role: Role::User,
        origin: Origin::Live,
        parts: vec![
            Part::text("x"),
            Part::Location { latitude: 1.0, longitude: 2.0, accuracy_m: None, title: None, address: None },
        ],
        native: None,
    }];
    assert!(prep_with(S55, &[], &here, &none).await.unwrap_err().message.contains("location"));
    let photo = blob(JPEG, "image/jpeg");
    let silent = vec![Turn {
        role: Role::User,
        origin: Origin::Live,
        parts: vec![Part::Image {
            blob: photo.0.clone(),
            filename: None,
            generated: false,
            dims: None,
            thought: false,
        }],
        native: None,
    }];
    let blobs = Blobs(vec![photo.clone()]);
    assert!(prep_with(S55, &[], &silent, &blobs).await.unwrap_err().message.contains("needs a prompt"));
    // Reference limits: 10 on 5.x, references + images ≤ 15 in a group.
    let many = |n: usize| {
        let mut parts = vec![Part::text("x")];
        parts.extend((0..n).map(|_| Part::Image {
            blob: photo.0.clone(),
            filename: None,
            generated: false,
            dims: None,
            thought: false,
        }));
        vec![Turn { role: Role::User, origin: Origin::Live, parts, native: None }]
    };
    let e = prep_with(S55, &[], &many(11), &blobs).await.unwrap_err();
    assert!(e.message.contains("at most 10 reference images"), "{e}");
    assert!(prep_with(S45, &[], &many(14), &blobs).await.is_ok());
    let e = prep_with(S45, &[kv("n", "3")], &many(13), &blobs).await.unwrap_err();
    assert!(e.message.contains("can be at most 2"), "{e}");
}

#[tokio::test]
async fn ids_outside_the_table() {
    let p = prep("doubao-seedream-4-0-20260415", &[]).await;
    assert_eq!(
        p.body,
        json!({"model": "doubao-seedream-4-0-20260415", "prompt": "一只猫", "response_format": "b64_json", "watermark": false})
    );
    assert!(p.warnings[0].contains("not in the bot's Seedream table"), "{:?}", p.warnings);
    assert!(!ark().knows("doubao-seedream-4-0-20260415"));
    assert!(prep_err("doubao-seedream-9-0-261231", &[flag("W")]).await.contains("cannot be checked"));
    // Chat models still go to the Chat API.
    let p = prep("doubao-seed-2-1-pro-260915", &[]).await;
    assert_eq!(p.dialect, crate::ark::DIALECT);
}

// ------------------------------------------------------------------ responses

fn b64(b: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(b)
}

fn parser_for(model: &str, body: Value) -> Parser {
    let p = Prepared {
        dialect: DIALECT,
        model: model.into(),
        url: String::new(),
        body,
        applied: vec![],
        warnings: vec![],
        notes: vec![],
        show_debug: false,
        auto_max_tokens: None,
        long_decode: false,
        settings: vec![],
    };
    Parser::new(&p, 7.0)
}

fn usages(evs: &[StreamEvent]) -> Vec<&Usage> {
    evs.iter().filter_map(|e| if let StreamEvent::Usage(u) = e { Some(u) } else { None }).collect()
}

#[test]
fn a_whole_response() {
    // The shape of Ark's 图生图 example (with b64_json).
    let v = json!({
        "model": S55,
        "created": 1757323224,
        "data": [{"b64_json": b64(PNG), "size": "2048x2048", "output_format": "png", "background": "opaque"}],
        "usage": {"generated_images": 1, "input_images": 1, "output_tokens": 16384, "total_tokens": 16384}
    });
    let mut p = parser_for(S55, json!({"image": "data:image/png;base64,x", "output_format": "png"}));
    let evs = p.response(&v).unwrap();
    assert!(matches!(&evs[0], StreamEvent::Meta { model: Some(m), .. } if m == S55));
    assert_eq!(evs[1], StreamEvent::Image { data: PNG.to_vec(), mime: "image/png".into() });
    let u = usages(&evs)[0];
    // ¥0.05 input + ¥0.23 for a 4.19M-pixel image, at 7 CNY/USD.
    assert!((u.cost_usd.unwrap() - 0.28 / 7.0).abs() < 1e-12, "{:?}", u.cost_usd);
    assert_eq!((u.output_tokens, u.input_tokens), (Some(16384), None));
    assert_eq!(u.cost_source, CostSource::Estimated);
    assert_eq!(evs.last(), Some(&StreamEvent::Finish { reason: Some("stop".into()), native: None }));
    assert!(p.failure().is_none());
    // The replay payload keeps everything but the image data.
    let n = p.native_payload();
    assert_eq!(n["data"], json!([{"size": "2048x2048", "output_format": "png", "background": "opaque"}]));
    assert_eq!(n["usage"]["generated_images"], 1);

    // Ark's top-level error when nothing was generated.
    let mut p = parser_for(S55, json!({}));
    let e = p
        .response(&json!({"error": {"code": "OutputImageSensitiveContentDetected", "message": "The request failed because the output image may contain sensitive information."}}))
        .unwrap_err();
    assert_eq!((e.kind, e.retry), (ErrorKind::InBand, Retry::Never));
    assert_eq!(e.error_type.as_deref(), Some("OutputImageSensitiveContentDetected"));
}

#[test]
fn a_streamed_group_with_a_failed_image() {
    // Ark's 流式输出 example (b64_json), with one image refused by moderation.
    let evs_in = [
        json!({"type": "image_generation.partial_succeeded", "model": S45, "created": 1757396757, "image_index": 0, "b64_json": b64(JPEG), "size": "2496x1664"}),
        json!({"type": "image_generation.partial_failed", "model": S45, "created": 1757396785, "image_index": 1,
               "error": {"code": "OutputImageSensitiveContentDetected", "message": "The request failed because the output image may contain sensitive information."}}),
        json!({"type": "image_generation.partial_succeeded", "model": S45, "created": 1757396825, "image_index": 2, "b64_json": b64(JPEG), "size": "2496×1664"}),
        json!({"type": "image_generation.completed", "model": S45, "created": 1757396825,
               "usage": {"generated_images": 2, "output_tokens": 32448, "total_tokens": 32448}}),
    ];
    let mut p = parser_for(S45, json!({"stream": true}));
    let mut evs = Vec::new();
    for v in &evs_in {
        let name = v["type"].as_str().map(str::to_string);
        evs.extend(p.event(name.as_deref(), v).unwrap());
    }
    assert!(p.finished());
    let images = evs.iter().filter(|e| matches!(e, StreamEvent::Image { mime, .. } if mime == "image/jpeg")).count();
    assert_eq!(images, 2);
    assert!(evs.iter().any(|e| matches!(e, StreamEvent::Notice(n)
        if n.starts_with("image 2 of the group was not generated: OutputImageSensitiveContentDetected"))));
    // Two images at ¥0.25 each.
    assert!((usages(&evs)[0].cost_usd.unwrap() - 0.5 / 7.0).abs() < 1e-12);
    assert!(p.failure().is_none(), "the other images are kept");
    assert_eq!(p.native_payload()["data"].as_array().unwrap().len(), 3);

    // Every image refused: a content-filter failure with Ark's errors.
    let mut p = parser_for(LITE_ID, json!({"stream": true}));
    p.event(None, &evs_in[1]).unwrap();
    let evs = p
        .event(None, &json!({"type": "image_generation.completed", "model": LITE_ID, "usage": {"generated_images": 0, "output_tokens": 0, "total_tokens": 0}}))
        .unwrap();
    assert!(evs.contains(&StreamEvent::Finish {
        reason: Some("content_filter".into()),
        native: Some("OutputImageSensitiveContentDetected".into())
    }));
    let e = p.failure().unwrap();
    assert!(e.message.starts_with("OutputImageSensitiveContentDetected: "), "{e}");
    assert_eq!(e.retry, Retry::Never);
    assert_eq!(usages(&evs)[0].cost_usd, Some(0.0));

    // An error event.
    let mut p = parser_for(S45, json!({"stream": true}));
    let e = p
        .event(Some("error"), &json!({"error": {"code": "ModelAccountIpmRateLimitExceeded", "message": "IPM (Images Per Minute) limit of the model is exceeded."}}))
        .unwrap_err();
    assert_eq!(e.retry, Retry::After(20));
}

#[test]
fn search_is_reported_before_the_images() {
    let v = json!({
        "model": LITE_ID, "created": 1757321139,
        "data": [{"b64_json": b64(PNG), "size": "3104x1312"}],
        "tools": [{"type": "web_search"}],
        "usage": {"generated_images": 1, "output_tokens": 15912, "total_tokens": 15912, "tool_usage": {"web_search": 1}}
    });
    let mut p = parser_for(LITE_ID, json!({"output_format": "png", "tools": [{"type": "web_search"}]}));
    let evs = p.response(&v).unwrap();
    let tool = evs.iter().position(|e| matches!(e, StreamEvent::Tool(t) if t.name == "web_search")).unwrap();
    let image = evs.iter().position(|e| matches!(e, StreamEvent::Image { .. })).unwrap();
    assert!(tool < image);
    assert!(
        matches!(&evs[tool], StreamEvent::Tool(t) if t.id.as_deref() == Some("web_search_1") && t.output.is_none())
    );
    assert_eq!(evs.iter().filter(|e| matches!(e, StreamEvent::Tool(_))).count(), 1, "one call per reported search");
    assert!(!evs.iter().any(|e| matches!(e, StreamEvent::Notice(_))));
    // Searches have no listed price: ¥0.22 for the image only.
    assert!((usages(&evs)[0].cost_usd.unwrap() - 0.22 / 7.0).abs() < 1e-12);
}

/// "如果为 0 表示未搜索": no call, a note.
#[test]
fn a_search_that_did_not_run() {
    let v = json!({
        "model": LITE_ID, "created": 1757321139,
        "data": [{"b64_json": b64(PNG), "size": "3104x1312"}],
        "tools": [{"type": "web_search"}],
        "usage": {"generated_images": 1, "output_tokens": 15912, "total_tokens": 15912, "tool_usage": {"web_search": 0}}
    });
    let mut p = parser_for(LITE_ID, json!({"output_format": "png", "tools": [{"type": "web_search"}]}));
    let evs = p.response(&v).unwrap();
    assert!(!evs.iter().any(|e| matches!(e, StreamEvent::Tool(_))));
    assert!(evs.iter().any(|e| matches!(e, StreamEvent::Notice(n) if n == "no web search ran (the model decides)")));
    assert!(evs.iter().any(|e| matches!(e, StreamEvent::Image { .. })));
}

#[test]
fn prices_by_pixels_and_inputs() {
    let pro = spec(PRO).unwrap();
    assert!((cost_cny(pro, 0, &[(1536, 1536)], 1) - 0.30).abs() < 1e-12);
    assert!((cost_cny(pro, 0, &[(2048, 2048)], 1) - 0.60).abs() < 1e-12);
    // The first input image is free, then ¥0.02 each.
    assert!((cost_cny(pro, 3, &[(1536, 1536)], 1) - 0.34).abs() < 1e-12);
    let s55 = spec(S55).unwrap();
    assert!((cost_cny(s55, 2, &[(1424, 800)], 1) - 0.23).abs() < 1e-12);
    // Generated images without a reported size: the highest price.
    assert!((cost_cny(s55, 0, &[], 1) - 0.23).abs() < 1e-12);
    assert!((cost_cny(spec(S40).unwrap(), 0, &[(4096, 4096)], 3) - 0.60).abs() < 1e-12);
    assert_eq!(cost_cny(spec(FLASH).unwrap(), 5, &[], 0), 0.0);
}

// ------------------------------------------------------------------ HTTP (local fake server)

async fn serve(status: u16, ctype: &str, body: String) -> (String, tokio::task::JoinHandle<(String, Value)>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/api/v3", listener.local_addr().unwrap());
    let ctype = ctype.to_string();
    let server = tokio::spawn(async move {
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
        let resp = format!(
            "HTTP/1.1 {status} X\r\ncontent-type: {ctype}\r\nx-request-id: req-1\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        sock.write_all(resp.as_bytes()).await.unwrap();
        let _ = sock.shutdown().await;
        (head, sent)
    });
    (base, server)
}

async fn exchange(
    status: u16,
    ctype: &str,
    body: String,
    model: &str,
    options: &[OptionToken],
) -> (Result<Value, LlmError>, CallOutcome, Vec<StreamEvent>, String, Value) {
    let (base, server) = serve(status, ctype, body).await;
    let a = ark_at(&base);
    let h = vec![Turn::user_text("一只猫")];
    let p = a.prepare(&input(model, options, &h), &[], &Blobs(vec![])).await.unwrap();
    let mut evs = Vec::new();
    let (r, o) = a.stream(&p, &mut |e: StreamEvent| evs.push(e), &crate::NullObserver, &CancellationToken::new()).await;
    let (head, sent) = server.await.unwrap();
    assert_eq!(sent, p.body);
    (r, o, evs, head, sent)
}

#[tokio::test]
async fn end_to_end_whole_and_streamed() {
    let body = json!({
        "model": S55, "created": 1757321139,
        "data": [{"b64_json": b64(PNG), "size": "1424x800"}],
        "usage": {"generated_images": 1, "output_tokens": 4450, "total_tokens": 4450}
    })
    .to_string();
    let (r, outcome, evs, head, _) = exchange(200, "application/json", body, S55, &[flag("1W")]).await;
    assert!(head.starts_with("POST /api/v3/images/generations HTTP/1.1"), "{head}");
    assert!(head.to_ascii_lowercase().contains("authorization: bearer k"), "{head}");
    assert_eq!(outcome.upstream_id.as_deref(), Some("req-1"));
    assert_eq!(r.unwrap()["usage"]["generated_images"], 1);
    assert!(evs.iter().any(|e| matches!(e, StreamEvent::Image { data, .. } if data == PNG)));
    assert!((usages(&evs)[0].cost_usd.unwrap() - 0.13 / 7.0).abs() < 1e-12);

    let sse: String = [
        json!({"type": "image_generation.partial_succeeded", "model": S45, "created": 1, "image_index": 0, "b64_json": b64(JPEG), "size": "4096x4096"}),
        json!({"type": "image_generation.completed", "model": S45, "created": 2, "usage": {"generated_images": 1, "output_tokens": 65536, "total_tokens": 65536}}),
    ]
    .iter()
    .map(|v| format!("event: {}\ndata: {v}\n\n", v["type"].as_str().unwrap()))
    .chain(std::iter::once("data: [DONE]\n\n".to_string()))
    .collect();
    let (r, _, evs, _, sent) = exchange(200, "text/event-stream", sse, S45, &[]).await;
    assert_eq!(sent["stream"], true);
    assert!(r.is_ok());
    assert!(evs.iter().any(|e| matches!(e, StreamEvent::Image { mime, .. } if mime == "image/jpeg")));
    assert!((usages(&evs)[0].cost_usd.unwrap() - 0.25 / 7.0).abs() < 1e-12);

    // An HTTP error, verbatim and classified.
    let err = r#"{"error":{"code":"InputTextSensitiveContentDetected","message":"The request failed because the input text may contain sensitive information.","param":"","type":"BadRequest"}}"#;
    let (r, _, _, _, _) = exchange(400, "application/json", err.into(), S55, &[]).await;
    let e = r.unwrap_err();
    assert_eq!((e.kind, e.status, e.retry), (ErrorKind::Http, Some(400), Retry::Never));
    assert_eq!(e.details.unwrap(), serde_json::from_str::<Value>(err).unwrap());
}
