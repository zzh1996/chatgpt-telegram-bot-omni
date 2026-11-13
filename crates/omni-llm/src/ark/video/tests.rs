//! Seedance tests: requests per model, mode and option, the conversation
//! rules (replies, drafts), task runs against a local fake server written per
//! Ark's documented shapes (create, polls, the video download), failures,
//! resuming, errors and cost. [`task_server`] also serves the Alibaba tests.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use omni_core::content::{NativeOutput, Part, Role};
use omni_core::trigger::OptionToken;
use omni_core::{BlobRef, Origin, Turn};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::*;
use crate::provider::Provider;

// ------------------------------------------------------------------ helpers

/// A request the fake server got.
#[derive(Debug, Clone)]
pub(crate) struct Req {
    /// `METHOD /path`.
    pub line: String,
    pub auth: Option<String>,
    /// Header names, lowercase, with values.
    pub headers: Vec<(String, String)>,
    pub body: Value,
}

impl Req {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }
}

pub(crate) type Seen = Arc<Mutex<Vec<Req>>>;

pub(crate) const VIDEO: &[u8] = b"\0\0\0\x18ftypmp42-video-bytes";

/// A fake task API on a local port: a `POST` answers `create` (status, body);
/// a `GET` of `/files/…` the video bytes; any other `GET` the next of `polls`
/// (the last one again when they run out), its string at `url_ptr` replaced by
/// the file's URL when it is "DOWNLOAD". Returns `http://127.0.0.1:port`.
pub(crate) async fn task_server(create: (u16, Value), polls: Vec<Value>, url_ptr: &'static str) -> (String, Seen) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let seen: Seen = Default::default();
    let log = seen.clone();
    let download = format!("{origin}/files/video.mp4");
    tokio::spawn(async move {
        let mut i = 0usize;
        loop {
            let Ok((mut sock, _)) = listener.accept().await else { return };
            let mut buf = Vec::new();
            let mut tmp = [0u8; 65536];
            let head = loop {
                let n = sock.read(&mut tmp).await.unwrap_or(0);
                if n == 0 {
                    break String::new();
                }
                buf.extend_from_slice(&tmp[..n]);
                if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&buf[..pos]).to_string();
                    let len: usize = head
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse().unwrap())
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
            let line = format!("{} {}", it.next().unwrap_or(""), it.next().unwrap_or(""));
            let headers: Vec<(String, String)> = head
                .lines()
                .skip(1)
                .filter_map(|l| l.split_once(':').map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string())))
                .collect();
            let auth = headers.iter().find(|(k, _)| k == "authorization").map(|(_, v)| v.clone());
            let start = buf.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4).unwrap_or(buf.len());
            let body = serde_json::from_slice(&buf[start..]).unwrap_or(Value::Null);
            log.lock().unwrap().push(Req { line: line.clone(), auth, headers, body });
            let (status, ctype, out): (u16, &str, Vec<u8>) = if line.starts_with("POST") {
                (create.0, "application/json", create.1.to_string().into_bytes())
            } else if line.contains("/files/") {
                (200, "video/mp4", VIDEO.to_vec())
            } else {
                let mut p = polls.get(i.min(polls.len().saturating_sub(1))).cloned().unwrap_or(Value::Null);
                i += 1;
                if let Some(v) = p.pointer_mut(url_ptr).filter(|u| u.as_str() == Some("DOWNLOAD")) {
                    *v = json!(download);
                }
                (200, "application/json", p.to_string().into_bytes())
            };
            let head = format!(
                "HTTP/1.1 {status} X\r\ncontent-type: {ctype}\r\ncontent-length: {}\r\nx-request-id: req-{}\r\nconnection: close\r\n\r\n",
                out.len(),
                log.lock().unwrap().len()
            );
            let _ = sock.write_all(head.as_bytes()).await;
            let _ = sock.write_all(&out).await;
        }
    });
    (origin, seen)
}

/// What a stream emitted, in short.
pub(crate) fn kinds(evs: &[StreamEvent]) -> Vec<String> {
    evs.iter()
        .map(|e| match e {
            StreamEvent::Meta { upstream_id: Some(id), .. } => format!("meta {id}"),
            StreamEvent::Meta { model: Some(m), .. } => format!("model {m}"),
            StreamEvent::Tool(t) => format!("{} {}", t.name, t.status.clone().unwrap_or_default()),
            StreamEvent::Checkpoint(c) => format!("checkpoint {c}"),
            StreamEvent::File { mime, name, data } => {
                format!("file {mime} {} {}", name.clone().unwrap_or_default(), data.len())
            }
            StreamEvent::Usage(u) => format!("usage {:?}", u.cost_usd.map(|c| (c * 1e6).round() / 1e6)),
            StreamEvent::Finish { reason, native } => {
                format!("finish {} {}", reason.clone().unwrap_or_default(), native.clone().unwrap_or_default())
            }
            StreamEvent::Notice(n) => format!("notice {n}"),
            other => format!("{other:?}"),
        })
        .collect()
}

struct Blobs(Vec<(BlobRef, Vec<u8>)>);

impl BlobLoader for Blobs {
    fn load<'a>(&'a self, r: &'a BlobRef) -> crate::BoxFuture<'a, anyhow::Result<Vec<u8>>> {
        let found = self.0.iter().find(|(b, _)| b == r).map(|(_, d)| d.clone());
        Box::pin(async move { found.ok_or_else(|| anyhow::anyhow!("no blob")) })
    }
}

fn cfg(base: &str) -> ArkConfig {
    ArkConfig {
        api_key: "k".into(),
        base_url: base.into(),
        connect_timeout: Duration::from_secs(1),
        idle_timeout: Duration::from_secs(5),
        max_tokens_cap: 65_536,
        cny_per_usd: 7.0,
    }
}

fn ark_at(base: &str) -> crate::ark::Ark {
    crate::ark::Ark::new(cfg(base)).unwrap()
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
        session_id: Some("conv".into()),
        user: Some("u-hash".into()),
        known_context: None,
        max_tokens_override: None,
    };
    ark().prepare(&input, &[], blobs).await
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

/// An answer with a generated video, as the bot stores a finished task.
fn video_answer(dialect: &str, model: &str, task: Value) -> Turn {
    let clip = BlobRef::for_bytes(VIDEO, "video/mp4");
    Turn {
        role: Role::Assistant,
        origin: Origin::Live,
        parts: vec![Part::File { blob: clip, filename: Some("v.mp4".into()), generated: true }],
        native: Some(NativeOutput {
            dialect: dialect.into(),
            model: model.into(),
            payload: task,
            context_tokens: None,
        }),
    }
}

fn task(id: &str, model: &str, made: i64, draft: bool) -> Value {
    json!({"id": id, "model": model, "status": "succeeded",
           "content": {"video_url": "https://ark-content-generation-cn-beijing.tos-cn-beijing.volces.com/v.mp4?X-Tos-Expires=86400"},
           "usage": {"completion_tokens": 108900, "total_tokens": 108900}, "created_at": made - 60, "updated_at": made,
           "seed": 78674, "resolution": "720p", "ratio": "16:9", "duration": 5, "framespersecond": 24,
           "service_tier": "default", "execution_expires_after": 172800, "generate_audio": true, "draft": draft, "priority": 0})
}

const S25: &str = "doubao-seedance-2-5-260628";
const S20: &str = "doubao-seedance-2-0-260128";
const FAST: &str = "doubao-seedance-2-0-fast-260128";
const MINI: &str = "doubao-seedance-2-0-mini-260615";
const PNG: &[u8] = b"\x89PNG\r\n\x1a\nframe";
const JPEG: &[u8] = b"\xFF\xD8\xFF\xE0frame";

// ------------------------------------------------------------------ requests

#[tokio::test]
async fn defaults_are_the_largest_resolution_with_audio_and_no_watermark() {
    let p = prep(S25, &[]).await.unwrap();
    assert_eq!(
        (p.dialect, p.url.as_str()),
        (DIALECT, "https://ark.cn-beijing.volces.com/api/v3/contents/generations/tasks")
    );
    assert_eq!(
        p.body,
        json!({"model": S25, "content": [{"type": "text", "text": "一只猫"}], "resolution": "1080p",
               "generate_audio": true, "watermark": false, "safety_identifier": "u-hash"})
    );
    assert_eq!(p.settings, ["text-to-video", "resolution 1080p", "with audio", "no watermark"]);
    assert!(p.warnings.is_empty() && p.notes.is_empty(), "{:?} {:?}", p.warnings, p.notes);
    assert_eq!(p.max_tokens(), None);
    assert!(ark().knows(S25) && ark().knows(S20) && ark().knows(FAST) && ark().knows(MINI));
    // 2.0: 4K; fast and mini: 720p.
    assert_eq!(prep(S20, &[]).await.unwrap().body["resolution"], "4k");
    assert_eq!(prep(FAST, &[]).await.unwrap().body["resolution"], "720p");
    assert_eq!(prep(MINI, &[]).await.unwrap().body["resolution"], "720p");
    for model in [S20, FAST, MINI] {
        let p = prep(model, &[]).await.unwrap();
        assert_eq!(p.body["generate_audio"], true, "{model}");
        assert!(p.warnings.is_empty() && p.notes.is_empty(), "{model}: {:?} {:?}", p.warnings, p.notes);
    }
}

#[tokio::test]
async fn options_are_checked_per_model() {
    let p = prep(S20, &[flag("1W"), kv("sec", "10")]).await.unwrap();
    assert_eq!(
        (p.body["resolution"].clone(), p.body["ratio"].clone(), p.body["duration"].clone()),
        (json!("720p"), json!("16:9"), json!(10))
    );
    assert_eq!(
        p.settings,
        ["text-to-video", "resolution 720p", "10 s", "aspect ratio 16:9", "with audio", "no watermark"]
    );
    assert_eq!(p.applied.iter().filter(|a| a.source == "option").count(), 3);
    assert_eq!(prep(S25, &[kv("ar", "21:9")]).await.unwrap().body["ratio"], "21:9");
    assert_eq!(prep(S20, &[flag("4")]).await.unwrap().body["resolution"], "4k");
    assert!(prep_err(S25, &[flag("4")]).await.contains("sizes: 1K (720p), 2K (1080p)"));
    assert!(prep_err(FAST, &[flag("2")]).await.contains("sizes: 1K (720p)"));
    assert!(prep_err(S25, &[kv("ar", "5:4")]).await.contains("ratios: 16:9, 4:3, 1:1, 3:4, 9:16, 21:9"));
    assert!(prep_err(S25, &[kv("sec", "31")]).await.contains("videos of 4–30 s"));
    assert!(prep_err(S20, &[kv("sec", "16")]).await.contains("videos of 4–15 s"));
    // No model in the table has a flex tier.
    for model in [S25, S20, FAST, MINI] {
        assert!(prep_err(model, &[kv("tier", "flex")]).await.contains("no service tiers"), "{model}");
    }
    // What video models do not take.
    assert!(prep_err(S25, &[kv("n", "2")]).await.contains("(`n=`)"));
    assert!(prep_err(S25, &[kv("bg", "opaque")]).await.contains("(`bg=`)"));
    assert!(prep_err(S25, &[kv("e", "high")]).await.contains("no reasoning effort"));
    assert!(prep_err(S25, &[flag("i")]).await.contains("is a video model"));
    assert!(prep_err(S25, &[flag("c")]).await.contains("has no code execution"));
    assert!(prep_err(S25, &[kv("p", "x")]).await.contains("does not apply"));
    // Web search: Ark publishes no price for it, so it is not offered.
    assert!(prep_err(S25, &[flag("s")]).await.contains("has no published price"));
}

#[tokio::test]
async fn drafts_are_480p_on_seedance_2_5() {
    let p = prep(S25, &[kv("q", "draft")]).await.unwrap();
    assert_eq!((p.body["draft"].clone(), p.body["resolution"].clone()), (json!(true), json!("480p")));
    assert!(p.settings.contains(&"draft (480p)".to_string()), "{:?}", p.settings);
    assert!(prep_err(S25, &[kv("q", "draft"), flag("2")]).await.contains("a draft is 480p"));
    assert!(prep_err(S25, &[kv("q", "high")]).await.contains("modes: draft"));
    assert!(prep_err(S20, &[kv("q", "draft")]).await.contains("draft mode is on Seedance 2.5"));
    assert!(prep_err(S25, &[kv("q", "final")]).await.contains("reply to a draft video"));
}

#[tokio::test]
async fn images_are_frames_or_references() {
    let (a, ba) = photo(PNG, "image/png", false);
    let (b, bb) = photo(JPEG, "image/jpeg", false);
    let (c, bc) = photo(JPEG, "image/jpg", false);
    let blobs = Blobs(vec![ba, bb, bc]);
    // One image: the first frame (the prompt is optional).
    let p = prep_with(S25, &[], &[user(vec![a.clone()])], &blobs).await.unwrap();
    let content = p.body["content"].as_array().unwrap();
    assert_eq!(content.len(), 1);
    assert_eq!((content[0]["type"].clone(), content[0]["role"].clone()), (json!("image_url"), json!("first_frame")));
    assert!(content[0]["image_url"]["url"].as_str().unwrap().starts_with("data:image/png;base64,"));
    assert_eq!(p.settings[0], "image-to-video: first frame");
    // Two: first and last frames.
    let p = prep_with(S20, &[], &[user(vec![Part::text("飞起来"), a.clone(), b.clone()])], &blobs).await.unwrap();
    let roles: Vec<&str> = p.body["content"].as_array().unwrap().iter().filter_map(|c| c["role"].as_str()).collect();
    assert_eq!(roles, ["first_frame", "last_frame"]);
    // Three, or named in the prompt: references.
    let p = prep_with(S20, &[], &[user(vec![Part::text("x"), a.clone(), b.clone(), c.clone()])], &blobs).await.unwrap();
    assert_eq!(p.settings[0], "reference-to-video: 3 images");
    let named = user(vec![Part::text("@图片1 的猫坐在 图片2 的沙发上"), a.clone(), b.clone()]);
    let p = prep_with(S20, &[], &[named], &blobs).await.unwrap();
    let roles: Vec<&str> = p.body["content"].as_array().unwrap().iter().filter_map(|c| c["role"].as_str()).collect();
    assert_eq!(roles, ["reference_image", "reference_image"]);
    // `image/jpg` goes as `image/jpeg` (lowercase, as Ark asks).
    let p = prep_with(S25, &[], &[user(vec![c.clone()])], &blobs).await.unwrap();
    assert!(p.body["content"][0]["image_url"]["url"].as_str().unwrap().starts_with("data:image/jpeg;base64,"));
    // A replied-to generated image comes first.
    let (generated, bg) = photo(b"\x89PNG-generated", "image/png", true);
    let answer = Turn { role: Role::Assistant, origin: Origin::Live, parts: vec![generated], native: None };
    let blobs2 = Blobs(vec![bg, blobs.0[1].clone()]);
    let history = vec![Turn::user_text("画猫"), answer, user(vec![Part::text("让它跑"), b.clone()])];
    let p = prep_with(S25, &[], &history, &blobs2).await.unwrap();
    assert_eq!(p.settings[0], "image-to-video: first frame (the previous image) + last frame");
    assert!(p.notes.is_empty(), "an edit chain: {:?}", p.notes);
    // What models cannot take.
    let ten: Vec<Part> = std::iter::once(Part::text("x")).chain((0..10).map(|_| a.clone())).collect();
    let e = prep_with(S20, &[], &[user(ten)], &blobs).await.unwrap_err();
    assert!(e.message.contains("at most 9 reference images"), "{}", e.message);
    let (gif, bgif) = photo(b"GIF89a", "image/svg+xml", false);
    let e = prep_with(S25, &[], &[user(vec![gif])], &Blobs(vec![bgif])).await.unwrap_err();
    assert!(e.message.contains("formats: jpeg, png, webp, bmp, tiff, gif, heic, heif"), "{}", e.message);
    // Text-to-video needs a prompt.
    let e = prep_with(S25, &[], &[user(vec![])], &blobs).await.unwrap_err();
    assert!(e.message.contains("needs a prompt"), "{}", e.message);
}

#[tokio::test]
async fn ratios_a_mode_cannot_use_are_noted_not_sent() {
    let (a, ba) = photo(PNG, "image/png", false);
    let blobs = Blobs(vec![ba]);
    // 2.5 keeps the first frame's ratio (Ark: "仅支持 adaptive").
    let p = prep_with(S25, &[flag("W")], &[user(vec![a.clone()])], &blobs).await.unwrap();
    assert!(p.body.get("ratio").is_none());
    assert_eq!(p.notes, ["ℹ️ aspect ratio 16:9 not sent: Seedance 2.5 keeps the first frame's aspect ratio"]);
    // 2.0 crops the image to it.
    let p = prep_with(S20, &[flag("W")], &[user(vec![a])], &blobs).await.unwrap();
    assert_eq!(p.body["ratio"], "16:9");
    assert!(p.notes.is_empty());
    // Web search is not offered (no published price).
    let (a, ba) = photo(PNG, "image/png", false);
    let e = prep_with(S25, &[flag("s")], &[user(vec![Part::text("x"), a])], &Blobs(vec![ba])).await.unwrap_err();
    assert!(e.message.contains("has no published price"), "{}", e.message);
}

#[tokio::test]
async fn audio_goes_as_base64_wav_or_mp3() {
    let (mp3, bm) = file("song.mp3", "audio/mpeg", b"ID3-mp3");
    let (img, bi) = photo(PNG, "image/png", false);
    let blobs = Blobs(vec![bm, bi]);
    // 2.5: audio alone.
    let p = prep_with(S25, &[], &[user(vec![Part::text("配上舞蹈"), mp3.clone()])], &blobs).await.unwrap();
    let c = &p.body["content"][1];
    assert_eq!((c["type"].clone(), c["role"].clone()), (json!("audio_url"), json!("reference_audio")));
    assert!(c["audio_url"]["url"].as_str().unwrap().starts_with("data:audio/mp3;base64,"));
    assert_eq!(p.settings[0], "reference-to-video: 1 audio clip");
    // 2.0: only with an image or a video.
    let e = prep_with(S20, &[], &[user(vec![Part::text("x"), mp3.clone()])], &blobs).await.unwrap_err();
    assert!(e.message.contains("reference audio only with a reference image or video"), "{}", e.message);
    let p = prep_with(S20, &[], &[user(vec![Part::text("x"), img, mp3])], &blobs).await.unwrap();
    assert_eq!(p.settings[0], "reference-to-video: 1 image + 1 audio clip");
    // A voice message (OGG) is not converted.
    let (ogg, bo) = file("voice.ogg", "audio/ogg", b"OggS");
    let e = prep_with(S25, &[], &[user(vec![ogg])], &Blobs(vec![bo])).await.unwrap_err();
    assert!(e.message.contains("takes WAV or MP3 audio"), "{}", e.message);
    // Too large (Ark: at most 15 MB).
    let big = BlobRef { sha256: "0".repeat(64), size: 16 * 1024 * 1024, mime: "audio/wav".into() };
    let part = Part::File { blob: big, filename: Some("a.wav".into()), generated: false };
    let e = prep_with(S25, &[], &[user(vec![part])], &Blobs(vec![])).await.unwrap_err();
    assert!(e.message.contains("at most 15 MB"), "{}", e.message);
}

#[tokio::test]
async fn a_reply_to_a_seedance_video_passes_its_url() {
    let now = unix_now();
    let answer = video_answer(DIALECT, S25, task("cgt-prev", S25, now - 600, false));
    let history = vec![Turn::user_text("一只猫"), answer.clone(), Turn::user_text("把猫换成狗")];
    let p = prep_with(S25, &[flag("W"), kv("sec", "8")], &history, &Blobs(vec![])).await.unwrap();
    let c = p.body["content"].as_array().unwrap();
    assert_eq!(c[0], json!({"type": "text", "text": "把猫换成狗"}));
    assert_eq!(c[1]["role"], "reference_video");
    assert!(c[1]["video_url"]["url"].as_str().unwrap().starts_with("https://ark-content-generation-cn-beijing"));
    assert_eq!(p.settings[0], "reference-to-video: the previous video");
    // 2.5 keeps the source's ratio (and length in an edit): not sent, noted.
    assert!(p.body.get("ratio").is_none() && p.body.get("duration").is_none());
    assert!(p.notes[0].starts_with("ℹ️ aspect ratio 16:9, sec=8 not sent"), "{:?}", p.notes);
    // 2.0 takes them.
    let p = prep_with(S20, &[flag("W"), kv("sec", "8")], &history, &Blobs(vec![])).await.unwrap();
    assert_eq!((p.body["ratio"].clone(), p.body["duration"].clone()), (json!("16:9"), json!(8)));
    // After 24 hours the URL is gone.
    let old = video_answer(DIALECT, S25, task("cgt-old", S25, now - 25 * 3600, false));
    let history = vec![Turn::user_text("一只猫"), old, Turn::user_text("换成狗")];
    let e = prep_with(S25, &[], &history, &Blobs(vec![])).await.unwrap_err();
    assert!(e.message.contains("its URL has expired (Ark keeps generated videos 24 hours)"), "{}", e.message);
    // Another platform's video, or an attached one: no URL to pass.
    let xai = video_answer("xai.videos", "grok-imagine-video-1.5", json!({"video": {"url": "https://x"}}));
    let history = vec![Turn::user_text("一只猫"), xai, Turn::user_text("换成狗")];
    let e = prep_with(S25, &[], &history, &Blobs(vec![])).await.unwrap_err();
    assert!(e.message.contains("not this one (made by `grok-imagine-video-1.5`)"), "{}", e.message);
    let (mp4, bv) = file("clip.mp4", "video/mp4", VIDEO);
    let e = prep_with(S25, &[], &[user(vec![Part::text("x"), mp4])], &Blobs(vec![bv])).await.unwrap_err();
    assert!(e.message.contains("takes input videos only as URLs"), "{}", e.message);
}

#[tokio::test]
async fn the_final_video_of_a_draft() {
    let now = unix_now();
    // A draft from three days ago: its video URL is gone, its task id still valid.
    let draft = video_answer(DIALECT, S25, task("cgt-draft", S25, now - 3 * 86_400, true));
    let history = vec![Turn::user_text("一只猫"), draft.clone(), Turn::user_text("好，出正片")];
    let p = prep_with(S25, &[kv("q", "final")], &history, &Blobs(vec![])).await.unwrap();
    assert_eq!(
        p.body,
        json!({"model": S25, "content": [{"type": "draft_task", "draft_task": {"id": "cgt-draft"}}],
               "resolution": "1080p", "watermark": false, "safety_identifier": "u-hash"})
    );
    assert_eq!(p.settings, ["final video of the draft", "resolution 1080p", "no watermark"]);
    assert_eq!(p.notes, ["ℹ️ the message's text is not sent: the final video reuses the draft's prompt"]);
    // Length and ratio are the draft's.
    let p = prep_with(S25, &[kv("q", "final"), flag("W"), kv("sec", "5")], &history, &Blobs(vec![])).await.unwrap();
    assert!(p.body.get("ratio").is_none() && p.body.get("duration").is_none());
    assert!(p.notes.iter().any(|n| n.contains("reuses the draft's aspect ratio and length")), "{:?}", p.notes);
    // Only 1080p, the same model, a draft, within 14 days.
    let e = prep_with(S25, &[kv("q", "final"), flag("1")], &history, &Blobs(vec![])).await.unwrap_err();
    assert!(e.message.contains("is 1080p"), "{}", e.message);
    let other = vec![
        Turn::user_text("一只猫"),
        video_answer(DIALECT, "doubao-seedance-2-5-flash-261115", task("cgt-d", "x", now, true)),
        Turn::user_text("正片"),
    ];
    let e = prep_with(S25, &[kv("q", "final")], &other, &Blobs(vec![])).await.unwrap_err();
    assert!(e.message.contains("its final must be made by the same model"), "{}", e.message);
    let not_draft = vec![
        Turn::user_text("猫"),
        video_answer(DIALECT, S25, task("cgt-n", S25, now, false)),
        Turn::user_text("正片"),
    ];
    let e = prep_with(S25, &[kv("q", "final")], &not_draft, &Blobs(vec![])).await.unwrap_err();
    assert!(e.message.contains("reply to a draft video"), "{}", e.message);
    let stale = vec![
        Turn::user_text("猫"),
        video_answer(DIALECT, S25, task("cgt-s", S25, now - 15 * 86_400, true)),
        Turn::user_text("正片"),
    ];
    let e = prep_with(S25, &[kv("q", "final")], &stale, &Blobs(vec![])).await.unwrap_err();
    assert!(e.message.contains("Ark keeps draft tasks 14 days"), "{}", e.message);
    // A draft that edited a video: the final is priced with video.
    let source = video_answer(DIALECT, S25, task("cgt-src", S25, now - 600, false));
    let chain = vec![Turn::user_text("猫"), source, Turn::user_text("换成狗"), draft, Turn::user_text("正片")];
    let p = prep_with(S25, &[kv("q", "final")], &chain, &Blobs(vec![])).await.unwrap();
    assert_eq!(p.settings[0], "final video of the draft (the draft had an input video)");
    assert!(has_video_input(&p));
}

#[tokio::test]
async fn ids_outside_the_table() {
    // Models with an end of service (1.0 pro, pro fast), retired ones, 2.5 flash before Ark
    // publishes its dated id, and newer ids: their price is unknown to the bot, so they are
    // not supported, with or without options.
    for id in [
        "doubao-seedance-1-0-pro-250528",
        "doubao-seedance-1-0-pro-fast-251015",
        "doubao-seedance-1-5-pro-251215",
        "doubao-seedance-1-0-lite-i2v-250428",
        "wan2-1-14b-t2v-250225",
        "doubao-seedance-2-5-flash-261115",
        "doubao-seedance-3-0-270101",
    ] {
        assert!(!ark().knows(id), "{id}");
        for options in [&[][..], &[flag("2")]] {
            assert_eq!(
                prep_err(id, options).await,
                format!(
                    "`{id}` is not in the bot's Volcengine Ark model table (its price is unknown), so it is not supported"
                )
            );
        }
    }
}

#[test]
fn prompts_that_name_their_images() {
    for p in
        ["图片1中的猫", "@图片 2 跳舞", "图1和图2", "@图像1", "the cat of image 1", "[Image 2] runs", "img3", "图片１"]
    {
        assert!(names_images(p), "{p}");
    }
    for p in ["让图片动起来", "an image of a cat", "imagine 1 cat", "images 2", "两只猫"] {
        assert!(!names_images(p), "{p}");
    }
}

// ------------------------------------------------------------------ tasks

fn running(status: &str) -> Value {
    json!({"id": "cgt-1", "model": S20, "status": status, "created_at": 1779348818, "updated_at": 1779348818,
           "service_tier": "default", "execution_expires_after": 172800})
}

fn succeeded() -> Value {
    json!({"id": "cgt-1", "model": S20, "status": "succeeded", "content": {"video_url": "DOWNLOAD"},
           "usage": {"completion_tokens": 108900, "total_tokens": 108900},
           "created_at": 1779348818, "updated_at": 1779348874, "seed": 78674, "resolution": "720p", "ratio": "16:9",
           "duration": 5, "framespersecond": 24, "service_tier": "default", "execution_expires_after": 172800,
           "generate_audio": true, "draft": false, "priority": 0})
}

async fn run(p: &Prepared, origin: &str) -> (Result<Value, LlmError>, CallOutcome, Vec<StreamEvent>) {
    let a = ark_at(&format!("{origin}/api/v3"));
    let mut evs = Vec::new();
    let mut sink = |e: StreamEvent| evs.push(e);
    let (r, outcome) = a.stream(p, &mut sink, &crate::NullObserver, &CancellationToken::new()).await;
    (r, outcome, evs)
}

async fn prepared(origin: &str, model: &str, options: &[OptionToken]) -> Prepared {
    let p = prep(model, options).await.unwrap();
    Prepared { url: format!("{origin}/api/v3/contents/generations/tasks"), ..p }
}

#[tokio::test]
async fn a_task_is_created_polled_and_downloaded() {
    let (origin, seen) = task_server(
        (200, json!({"id": "cgt-1"})),
        vec![running("queued"), running("running"), succeeded()],
        "/content/video_url",
    )
    .await;
    let p = prepared(&origin, S20, &[flag("1")]).await;
    let (r, outcome, evs) = run(&p, &origin).await;
    let native = r.unwrap();
    assert_eq!(native["content"]["video_url"], format!("{origin}/files/video.mp4"), "kept for edits of replies");
    assert_eq!(outcome.upstream_id.as_deref(), Some("cgt-1"));
    assert_eq!(outcome.continuation, Some(json!({"task_id": "cgt-1"})));
    let reqs = seen.lock().unwrap().clone();
    let lines: Vec<&str> = reqs.iter().map(|r| r.line.as_str()).collect();
    assert_eq!(
        lines,
        [
            "POST /api/v3/contents/generations/tasks",
            "GET /api/v3/contents/generations/tasks/cgt-1",
            "GET /api/v3/contents/generations/tasks/cgt-1",
            "GET /api/v3/contents/generations/tasks/cgt-1",
            "GET /files/video.mp4"
        ]
    );
    assert_eq!(reqs[0].auth.as_deref(), Some("Bearer k"));
    assert_eq!(reqs[0].body, p.body);
    assert_eq!(reqs[4].auth, None, "no credentials are sent to the download URL");
    // 108,900 tokens at ¥46 / M (2.0, 720p, no input video), ¥7 / USD.
    let cost = format!("usage {:?}", Some((108_900.0 * 46.0 / 1e6 / 7.0 * 1e6_f64).round() / 1e6));
    assert_eq!(
        kinds(&evs),
        [
            "meta cgt-1",
            "video_generation queued",
            "checkpoint {\"task_id\":\"cgt-1\"}",
            &format!("model {S20}"),
            "video_generation running",
            "video_generation succeeded",
            &format!("file video/mp4 {S20}-5s.mp4 {}", VIDEO.len()),
            &cost,
            "finish stop succeeded",
        ]
    );
    let mut acc = crate::Accumulator::new();
    evs.iter().cloned().for_each(|e| acc.apply(e));
    let Part::ToolCall(t) = &acc.parts()[0] else { panic!("{:?}", acc.parts()) };
    assert_eq!(t.input, Some(json!({"prompt": "一只猫", "resolution": "720p", "generate_audio": true})));
    assert_eq!(
        t.output,
        Some(json!({"model": S20, "seed": 78674, "resolution": "720p", "ratio": "16:9", "duration": 5,
                    "framespersecond": 24, "service_tier": "default", "generate_audio": true, "draft": false}))
    );
    let u = acc.usage.unwrap();
    assert_eq!((u.output_tokens, u.cost_source), (Some(108_900), CostSource::Estimated));
}

#[tokio::test]
async fn a_restart_polls_the_same_task() {
    let (origin, seen) = task_server((200, json!({"id": "never"})), vec![succeeded()], "/content/video_url").await;
    let p = prepared(&origin, S20, &[]).await;
    let resumed = p.with_continuation(&json!({"task_id": "cgt-1"}));
    assert_eq!(resumed.url, format!("{origin}/api/v3/contents/generations/tasks/cgt-1"));
    assert!(resumed.body.is_null());
    let (r, outcome, evs) = run(&resumed, &origin).await;
    assert!(r.is_ok());
    assert_eq!(outcome.upstream_id.as_deref(), Some("cgt-1"));
    let lines: Vec<String> = seen.lock().unwrap().iter().map(|r| r.line.clone()).collect();
    assert_eq!(lines, ["GET /api/v3/contents/generations/tasks/cgt-1", "GET /files/video.mp4"], "no new task");
    assert!(!evs.iter().any(|e| matches!(e, StreamEvent::Checkpoint(_))));
    // Not a video checkpoint: unchanged.
    assert_eq!(continuation_request(&p, &json!({"x": 1})).url, p.url);
}

#[tokio::test]
async fn failed_expired_and_cancelled_tasks() {
    let failed = json!({"id": "cgt-1", "model": S20, "status": "failed",
        "error": {"code": "OutputVideoSensitiveContentDetected", "message": "The request failed because the output video may contain sensitive information.Request ID: 0217"}});
    let (origin, _) = task_server((200, json!({"id": "cgt-1"})), vec![failed.clone()], "/content/video_url").await;
    let p = prepared(&origin, S20, &[]).await;
    let (r, outcome, evs) = run(&p, &origin).await;
    let e = r.unwrap_err();
    // Moderation is never retried; the checkpoint is cleared before the error.
    assert_eq!(
        (e.kind, e.retry, e.error_type.as_deref()),
        (ErrorKind::InBand, Retry::Never, Some("OutputVideoSensitiveContentDetected"))
    );
    assert_eq!(e.details, Some(failed));
    assert!(outcome.continuation.is_none());
    let k = kinds(&evs);
    assert_eq!(&k[k.len() - 2..], ["video_generation failed", "checkpoint null"]);
    let Some(StreamEvent::Tool(t)) = evs.iter().rev().find(|e| matches!(e, StreamEvent::Tool(_))) else { panic!() };
    assert_eq!(t.output.as_ref().unwrap()["code"], "OutputVideoSensitiveContentDetected");
    // An internal error: a new task may succeed.
    let busy = json!({"id": "cgt-1", "status": "failed", "error": {"code": "InternalServiceError", "message": "x"}});
    let (origin, _) = task_server((200, json!({"id": "cgt-1"})), vec![busy], "/content/video_url").await;
    let p = prepared(&origin, S20, &[]).await;
    assert_eq!(run(&p, &origin).await.0.unwrap_err().retry, Retry::Backoff);
    // Expired: retried; cancelled: not.
    let (origin, _) =
        task_server((200, json!({"id": "cgt-1"})), vec![json!({"id": "cgt-1", "status": "expired"})], "/x").await;
    let resumed = prepared(&origin, S20, &[]).await.with_continuation(&json!({"task_id": "cgt-1"}));
    let (r, _, evs) = run(&resumed, &origin).await;
    let e = r.unwrap_err();
    assert_eq!((e.retry, e.error_type.as_deref()), (Retry::Backoff, Some("expired")));
    assert!(matches!(evs.last(), Some(StreamEvent::Checkpoint(Value::Null))));
    let (origin, _) =
        task_server((200, json!({"id": "cgt-1"})), vec![json!({"id": "cgt-1", "status": "cancelled"})], "/x").await;
    let p = prepared(&origin, S20, &[]).await;
    assert_eq!(run(&p, &origin).await.0.unwrap_err().retry, Retry::Never);
}

#[tokio::test]
async fn http_errors_say_what_to_do() {
    let not_open = json!({"error": {"code": "ModelNotOpen", "message": "Your account 2100xxxx has not activated the model doubao-seedance-2-0. Please activate the model service in the Ark Console. Request id: 0217", "param": "", "type": "NotFound"}});
    let (origin, _) = task_server((404, not_open.clone()), vec![], "/x").await;
    let p = prepared(&origin, S20, &[]).await;
    let (r, outcome, evs) = run(&p, &origin).await;
    let e = r.unwrap_err();
    assert_eq!((e.retry, e.status, e.error_type.as_deref()), (Retry::Never, Some(404), Some("ModelNotOpen")));
    assert!(e.message.starts_with("Your account 2100xxxx has not activated the model"), "{}", e.message);
    assert!(e.message.contains("activate the model in the Ark console") && e.message.contains("¥200"), "{}", e.message);
    assert_eq!(e.details, Some(not_open));
    assert!(evs.is_empty() && outcome.continuation.is_none(), "no task, nothing to resume");
    // A full task queue is retried later; an exhausted free trial is not.
    let queue = json!({"error": {"code": "QuotaExceeded", "message": "The request has exceeded the quota. Request ID: 0217", "type": "TooManyRequests"}});
    let e = crate::chatstream::http_error(429, None, &queue.to_string(), video_error);
    assert_eq!(e.retry, Retry::After(30));
    let trial = json!({"error": {"code": "QuotaExceeded", "message": "Your account [2100] has exhausted its free trial quota for the [doubao-seedance-2-0] model. Request ID: 0217"}});
    let e = crate::chatstream::http_error(429, None, &trial.to_string(), video_error);
    assert_eq!(e.retry, Retry::Never);
    let rpm = json!({"error": {"code": "ModelAccountRpmRateLimitExceeded", "message": "x"}});
    assert_eq!(crate::chatstream::http_error(429, Some(7), &rpm.to_string(), video_error).retry, Retry::After(7));
}

#[test]
fn cost_follows_resolution_input_and_tier() {
    let s25 = spec(S25).unwrap();
    // 5 s 1080p 16:9: 1920 × 1080 × 24 × 5 / 1024 = 243,000 tokens; × ¥77 / M ≈ ¥18.71 (Ark's example).
    let c = cost_cny(s25, 243_000, "1080p", false, false).unwrap();
    assert!((c - 18.711).abs() < 1e-9, "{c}");
    assert!((cost_cny(s25, 1_000_000, "720p", true, false).unwrap() - 42.0).abs() < 1e-9);
    // 2.0 4K: 3840 × 2160 × 24 × 5 / 1024 = 972,000 tokens × ¥26 / M ≈ ¥25.27.
    assert!((cost_cny(spec(S20).unwrap(), 972_000, "4k", false, false).unwrap() - 25.272).abs() < 1e-9);
    // A resolution the model has not: unknown.
    assert_eq!(cost_cny(spec(FAST).unwrap(), 1, "1080p", false, false), None);
}
