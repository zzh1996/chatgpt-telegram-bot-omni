//! Gemini Omni tests: requests per option and input, ids outside the table, the
//! background job against a local fake Gemini API (shapes of the Interactions
//! API reference and the Omni guide), errors and cost.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use omni_core::content::{NativeOutput, Part, ReasoningKind, Role};
use omni_core::trigger::OptionToken;
use omni_core::{BlobRef, Origin, Turn};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

use super::*;
use crate::openai::wire::tests::{MemBlobs, file, photo, user};
use crate::provider::Provider;

fn cfg(base: &str) -> DirectConfig {
    DirectConfig {
        api_key: "test-key".into(),
        base_url: base.into(),
        connect_timeout: Duration::from_secs(5),
        idle_timeout: Duration::from_secs(5),
        max_tokens_cap: None,
        cny_per_usd: 7.1,
        long_decode_budget: 393_216,
    }
}

fn google_at(base: &str) -> super::super::Google {
    super::super::Google::new(cfg(base)).unwrap()
}

fn google() -> super::super::Google {
    google_at("https://generativelanguage.googleapis.com/v1beta")
}

fn flag(f: &str) -> OptionToken {
    OptionToken::Flag(f.into())
}

fn kv(k: &str, v: &str) -> OptionToken {
    OptionToken::KeyValue(k.into(), v.into())
}

const OMNI: &str = "gemini-omni-1.2-flash";

async fn prep_with(
    model: &str,
    opts: &[OptionToken],
    history: &[Turn],
    blobs: &MemBlobs,
) -> Result<Prepared, LlmError> {
    let input = CallInput {
        model,
        options: opts,
        history,
        session_id: Some("tg-1".into()),
        user: Some("AB12cd34".into()),
        known_context: None,
        max_tokens_override: None,
    };
    google().prepare(&input, &[], blobs).await
}

async fn prep(model: &str, opts: &[OptionToken]) -> Result<Prepared, LlmError> {
    prep_with(model, opts, &[Turn::user_text("A marble rolling fast on a chain reaction track")], &MemBlobs(vec![]))
        .await
}

async fn err(model: &str, opts: &[OptionToken]) -> String {
    prep(model, opts).await.unwrap_err().message
}

fn answer(parts: Vec<Part>, native: Option<NativeOutput>) -> Turn {
    Turn { role: Role::Assistant, origin: Origin::Live, parts, native }
}

#[tokio::test]
async fn defaults_are_4k_in_the_background() {
    let p = prep(OMNI, &[]).await.unwrap();
    assert_eq!((p.dialect, p.url.as_str()), (DIALECT, "https://generativelanguage.googleapis.com/v1beta/interactions"));
    assert_eq!(
        p.body,
        json!({"model": OMNI, "input": "A marble rolling fast on a chain reaction track",
               "response_format": {"type": "video", "resolution": "4k", "delivery": "uri"},
               "background": true, "labels": {"safety_identifier": "ab12cd34"}})
    );
    assert_eq!(p.settings, ["text-to-video", "resolution 4k"]);
    assert!(p.applied.iter().all(|a| a.source == "default"));
    assert!(p.warnings.is_empty() && p.notes.is_empty());
    assert_eq!(p.max_tokens(), None);
    assert!(google().knows(OMNI) && !google().knows("gemini-omni-1.1-flash"));
    assert!(!google().knows("veo-3.1-generate-preview") && !google().knows("gemini-omni-flash-preview"));
    // The chat models stay on generateContent.
    assert_eq!(prep("gemini-3.8-flash", &[]).await.unwrap().dialect, super::super::DIALECT);
}

#[tokio::test]
async fn options_map_to_the_response_format() {
    let p = prep(OMNI, &[kv("size", "1K"), kv("ar", "9:16"), kv("sec", "15")]).await.unwrap();
    assert_eq!(
        p.body["response_format"],
        json!({"type": "video", "resolution": "720p", "aspect_ratio": "9:16", "duration": "15s"}),
        "no URI delivery at 720p"
    );
    assert_eq!(p.settings, ["text-to-video", "resolution 720p", "aspect ratio 9:16", "15 s"]);
    let sources: Vec<&str> = p.applied.iter().map(|a| a.source.as_str()).collect();
    assert_eq!(sources[..3], ["option", "option", "option"]);
    let p = prep(OMNI, &[kv("size", "2K"), kv("ar", "16:9")]).await.unwrap();
    assert_eq!(
        p.body["response_format"],
        json!({"type": "video", "resolution": "1080p", "delivery": "uri", "aspect_ratio": "16:9"})
    );
    assert!(err(OMNI, &[kv("ar", "4:3")]).await.contains("ratios: 16:9, 9:16"));
    assert!(err(OMNI, &[kv("sec", "16")]).await.contains("videos of 3–15 s (sec=16)"));
    assert!(err(OMNI, &[kv("sec", "2")]).await.contains("3–15 s"));
    for (opt, what) in [
        (kv("q", "high"), "(`q=`)"),
        (kv("n", "2"), "(`n=`)"),
        (kv("bg", "auto"), "(`bg=`)"),
        (flag("s"), "has no web search"),
        (kv("e", "high"), "no reasoning effort levels"),
        (kv("tier", "flex"), "no service tiers"),
        (kv("p", "google-vertex"), "does not apply"),
        (flag("i"), "makes videos by itself"),
    ] {
        let e = err(OMNI, &[opt]).await;
        assert!(e.contains(what), "{e}");
    }
}

/// Shut-down (Veo, the Omni preview), deprecated (1.1, routed to 1.2) and unknown
/// ids: their price is unknown to the bot, so they are not supported.
#[tokio::test]
async fn ids_outside_the_table() {
    for id in [
        "veo-3.1-generate-preview",
        "veo-2.0-generate-001",
        "veo-4.0-generate",
        "gemini-omni-flash-preview",
        "gemini-omni-1.1-flash",
        "gemini-omni-1.3-flash",
    ] {
        for opts in [&[][..], &[kv("sec", "20")]] {
            assert_eq!(
                err(id, opts).await,
                format!(
                    "`{id}` is not in the bot's Gemini API model table (its price is unknown), so it is not supported"
                )
            );
        }
    }
}

#[tokio::test]
async fn images_and_videos_go_as_content() {
    let (img, b1) = photo("image/jpg", b"\xff\xd8\xff-first");
    let (img2, b2) = photo("image/png", b"\x89PNG-last");
    let (mov, b3) = file("clip.mov", "video/quicktime", b"\0\0\0\x14ftypqt  ");
    let blobs = MemBlobs(vec![b1, b2, b3]);
    let h = vec![user(vec![Part::text("A smooth transition between the two"), img.clone(), img2.clone()])];
    let p = prep_with(OMNI, &[], &h, &blobs).await.unwrap();
    let input = p.body["input"].as_array().unwrap();
    assert_eq!(input.len(), 3);
    assert_eq!((input[0]["type"].clone(), input[0]["mime_type"].clone()), (json!("image"), json!("image/jpeg")));
    assert_eq!(input[0]["data"], crate::provider::b64(b"\xff\xd8\xff-first"));
    assert_eq!(input[1]["mime_type"], "image/png");
    assert_eq!(input[2], json!({"type": "text", "text": "A smooth transition between the two"}));
    assert_eq!(p.settings[0], "from 2 images");
    // An attached video (QuickTime is MOV) comes first; the model decides edit / extend / reference.
    let h = vec![user(vec![Part::text("Extend this video"), img.clone(), mov])];
    let p = prep_with(OMNI, &[], &h, &blobs).await.unwrap();
    let input = p.body["input"].as_array().unwrap();
    assert_eq!((input[0]["type"].clone(), input[0]["mime_type"].clone()), (json!("video"), json!("video/mov")));
    assert_eq!(input[1]["type"], "image");
    assert_eq!(p.settings[0], "from 1 video + 1 image");
    // Limits and what Omni does not take.
    let seven: Vec<Part> = std::iter::once(Part::text("x")).chain((0..7).map(|_| img.clone())).collect();
    let e = prep_with(OMNI, &[], &[user(seven)], &blobs).await.unwrap_err();
    assert!(e.message.contains("at most 6 reference images (7 given)"), "{}", e.message);
    let (svg, bs) = photo("image/svg+xml", b"<svg/>");
    let e = prep_with(OMNI, &[], &[user(vec![Part::text("x"), svg])], &MemBlobs(vec![bs])).await.unwrap_err();
    assert!(e.message.contains("formats: png, jpeg, webp"), "{}", e.message);
    let (voice, bv) = file("voice.ogg", "audio/ogg", b"OggS");
    let e = prep_with(OMNI, &[], &[user(vec![Part::text("x"), voice])], &MemBlobs(vec![bv])).await.unwrap_err();
    assert!(e.message.contains("takes no audio/ogg files"), "{}", e.message);
    let sys = Turn { role: Role::System, origin: Origin::Synthetic, parts: vec![Part::text("be nice")], native: None };
    let e = prep_with(OMNI, &[], &[sys, Turn::user_text("x")], &blobs).await.unwrap_err();
    assert!(e.message.contains("no system prompt"), "{}", e.message);
    let e = prep_with(OMNI, &[], &[user(vec![])], &blobs).await.unwrap_err();
    assert!(e.message.contains("needs a prompt"), "{}", e.message);
    // A conversation with nothing to edit (earlier turns of another model): one prompt per
    // request, so it would be ignored, which is refused.
    let h = vec![Turn::user_text("a story"), Turn::assistant_text("Once upon a time"), Turn::user_text("film it")];
    let e = prep_with(OMNI, &[], &h, &blobs).await.unwrap_err();
    assert_eq!(
        e.message,
        "`gemini-omni-1.2-flash` takes one prompt via the Gemini API, not a conversation: start a new message, or reply to a generated image or video it can take as input"
    );
}

#[tokio::test]
async fn a_reply_continues_the_interaction_or_sends_the_video() {
    let clip = BlobRef::for_bytes(b"\0\0\0\x18ftypmp42-gen", "video/mp4");
    let blobs = MemBlobs(vec![(clip.clone(), b"\0\0\0\x18ftypmp42-gen".to_vec())]);
    let video = Part::File { blob: clip, filename: Some("v.mp4".into()), generated: true };
    // This model's own answer: `previous_interaction_id`, its video stays on Google's side.
    let own = NativeOutput {
        dialect: DIALECT.into(),
        model: OMNI.into(),
        payload: json!({"id": "v1_prev", "status": "completed"}),
        context_tokens: None,
    };
    let h = vec![
        Turn::user_text("A violinist on a stage"),
        answer(vec![video.clone()], Some(own.clone())),
        Turn::user_text("Make the violin invisible."),
    ];
    let p = prep_with(OMNI, &[], &h, &blobs).await.unwrap();
    assert_eq!(p.body["previous_interaction_id"], "v1_prev");
    assert_eq!(p.body["input"], "Make the violin invisible.");
    assert_eq!(p.settings[0], "follow-up of the previous video");
    assert!(p.notes.is_empty(), "{:?}", p.notes);
    // Another model's video (or another Omni model): sent inline, first.
    let other = NativeOutput { model: "gemini-omni-1.1-flash".into(), ..own };
    let h2 = vec![h[0].clone(), answer(vec![video.clone()], Some(other)), h[2].clone()];
    let p = prep_with(OMNI, &[], &h2, &blobs).await.unwrap();
    assert!(p.body.get("previous_interaction_id").is_none());
    let input = p.body["input"].as_array().unwrap();
    assert_eq!((input[0]["type"].clone(), input[0]["mime_type"].clone()), (json!("video"), json!("video/mp4")));
    assert_eq!(p.settings[0], "from 1 video");
    // A generated image replied to (Nano Banana, say): an input image.
    let pic = BlobRef::for_bytes(b"\x89PNG-gen", "image/png");
    let blobs2 = MemBlobs(vec![(pic.clone(), b"\x89PNG-gen".to_vec())]);
    let gen_img = Part::Image { blob: pic, filename: None, generated: true, dims: None, thought: false };
    let h3 = vec![Turn::user_text("a cat"), answer(vec![gen_img], None), Turn::user_text("animate it")];
    let p = prep_with(OMNI, &[], &h3, &blobs2).await.unwrap();
    assert_eq!(p.body["input"][0]["type"], "image");
    assert!(p.notes.is_empty());
}

// ------------------------------------------------------------------ HTTP (local fake Gemini API)

/// Requests the server got: `METHOD path?query`, the `x-goog-api-key`, the JSON body.
type Seen = Arc<Mutex<Vec<(String, Option<String>, Value)>>>;

const MP4: &[u8] = b"\0\0\0\x18ftypmp42-omni-video";

/// A fake Gemini API: `POST /v1beta/interactions` answers `created`, each
/// `GET /v1beta/interactions/…` the next of `polls`, `GET /v1beta/files/f1`
/// the next of `files`, the download the MP4. `{base}` in a reply is the
/// server's `…/v1beta`.
async fn server(created: (u16, Value), polls: Vec<Value>, files: Vec<Value>) -> (String, Seen) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/v1beta", listener.local_addr().unwrap());
    let seen: Seen = Default::default();
    let (log, me) = (seen.clone(), base.clone());
    tokio::spawn(async move {
        let mut polls = polls.into_iter();
        let mut files = files.into_iter();
        loop {
            let Ok((mut sock, _)) = listener.accept().await else { return };
            let (mut buf, mut tmp) = (Vec::new(), vec![0u8; 65536]);
            let (head, body) = loop {
                let n = sock.read(&mut tmp).await.unwrap_or(0);
                if n == 0 {
                    break (String::new(), Value::Null);
                }
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
                        let n = sock.read(&mut tmp).await.unwrap_or(0);
                        if n == 0 {
                            break;
                        }
                        buf.extend_from_slice(&tmp[..n]);
                    }
                    let end = (pos + 4 + len).min(buf.len());
                    break (head, serde_json::from_slice(&buf[pos + 4..end]).unwrap_or(Value::Null));
                }
            };
            let mut it = head.lines().next().unwrap_or("").split(' ');
            let line = format!("{} {}", it.next().unwrap_or(""), it.next().unwrap_or(""));
            let key = head.lines().find_map(|l| {
                let (k, v) = l.split_once(':')?;
                k.eq_ignore_ascii_case("x-goog-api-key").then(|| v.trim().to_string())
            });
            log.lock().unwrap().push((line.clone(), key, body));
            let (status, ctype, out): (u16, &str, Vec<u8>) = if line.starts_with("POST") {
                (created.0, "application/json", created.1.to_string().replace("{base}", &me).into_bytes())
            } else if line.contains(":download") || line.contains("/other/") {
                (200, "video/mp4", MP4.to_vec())
            } else if line.contains("/files/") {
                let f = files.next().unwrap_or(json!({"name": "files/f1", "state": "ACTIVE"}));
                (200, "application/json", f.to_string().into_bytes())
            } else {
                let p = polls.next().unwrap_or(json!({"id": "v1_abc", "status": "in_progress"}));
                (200, "application/json", p.to_string().replace("{base}", &me).into_bytes())
            };
            let head = format!(
                "HTTP/1.1 {status} X\r\ncontent-type: {ctype}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                out.len()
            );
            let _ = sock.write_all(head.as_bytes()).await;
            let _ = sock.write_all(&out).await;
            let _ = sock.flush().await;
        }
    });
    (base, seen)
}

fn created() -> (u16, Value) {
    (
        200,
        json!({"id": "v1_abc", "status": "in_progress", "object": "interaction", "model": OMNI,
                 "created": "2026-11-13T08:00:00Z", "updated": "2026-11-13T08:00:00Z"}),
    )
}

/// A completed interaction as `GET` returns it: the whole timeline, the video inline.
fn completed(video: Value) -> Value {
    json!({"id": "v1_abc", "status": "completed", "object": "interaction", "model": OMNI,
           "created": "2026-11-13T08:00:00Z", "updated": "2026-11-13T08:02:10Z",
           "steps": [
               {"type": "user_input", "content": [{"type": "text", "text": "A marble rolling fast on a chain reaction track"}]},
               {"type": "thought", "signature": "c2ln", "summary": [{"type": "text", "text": "Planning a continuous shot."}]},
               {"type": "model_output", "content": [video]}
           ],
           "usage": {"total_input_tokens": 20, "input_tokens_by_modality": [{"modality": "text", "tokens": 20}],
                     "total_cached_tokens": 0, "total_output_tokens": 173760,
                     "output_tokens_by_modality": [{"modality": "video", "tokens": 173760}],
                     "total_tool_use_tokens": 0, "total_thought_tokens": 300, "total_tokens": 174080},
           "service_tier": "standard"})
}

fn inline_video() -> Value {
    json!({"type": "video", "mime_type": "video/mp4", "data": crate::provider::b64(MP4)})
}

async fn run(base: &str, p: &Prepared) -> (Result<Value, LlmError>, CallOutcome, Vec<StreamEvent>) {
    let g = google_at(base);
    let mut evs = Vec::new();
    let mut sink = |e: StreamEvent| evs.push(e);
    let (r, outcome) = g.stream(p, &mut sink, &crate::NullObserver, &CancellationToken::new()).await;
    (r, outcome, evs)
}

fn lines(seen: &Seen) -> Vec<String> {
    seen.lock().unwrap().iter().map(|r| r.0.clone()).collect()
}

fn kinds(evs: &[StreamEvent]) -> Vec<String> {
    evs.iter()
        .map(|e| match e {
            StreamEvent::Meta { upstream_id, model, .. } => {
                format!("meta {} {}", upstream_id.clone().unwrap_or_default(), model.clone().unwrap_or_default())
            }
            StreamEvent::Tool(t) => format!("tool {}", t.status.clone().unwrap_or_default()),
            StreamEvent::Checkpoint(c) => format!("checkpoint {c}"),
            StreamEvent::Reasoning { kind, text, .. } => format!("reasoning {kind:?} {text}"),
            StreamEvent::File { mime, name, data } => {
                format!("file {mime} {} {}", name.clone().unwrap_or_default(), data.len())
            }
            StreamEvent::Usage(u) => format!("usage {:?}", u.cost_usd.map(|c| (c * 1e5).round() / 1e5)),
            StreamEvent::Finish { reason, native } => {
                format!("finish {} {}", reason.clone().unwrap_or_default(), native.clone().unwrap_or_default())
            }
            other => format!("{other:?}"),
        })
        .collect()
}

#[tokio::test]
async fn a_background_job_is_checkpointed_polled_and_its_video_decoded() {
    let (base, seen) =
        server(created(), vec![json!({"id": "v1_abc", "status": "in_progress"}), completed(inline_video())], vec![])
            .await;
    let p = Prepared { url: format!("{base}/interactions"), ..prep(OMNI, &[]).await.unwrap() };
    let (r, outcome, evs) = run(&base, &p).await;
    let native = r.unwrap();
    assert_eq!(native["id"], "v1_abc", "kept for replies (previous_interaction_id)");
    assert!(native["steps"][2]["content"][0]["data"].as_str().unwrap().contains("stored as the turn's media"));
    assert_eq!(outcome.upstream_id.as_deref(), Some("v1_abc"));
    assert_eq!(
        lines(&seen),
        ["POST /v1beta/interactions", "GET /v1beta/interactions/v1_abc", "GET /v1beta/interactions/v1_abc"]
    );
    let reqs = seen.lock().unwrap().clone();
    assert_eq!(reqs[0].2, p.body);
    assert!(reqs.iter().all(|r| r.1.as_deref() == Some("test-key")), "the key in x-goog-api-key");
    assert_eq!(
        kinds(&evs),
        [
            format!("meta v1_abc {OMNI}"),
            "tool in_progress".into(),
            "checkpoint {\"interaction_id\":\"v1_abc\"}".into(),
            format!("meta v1_abc {OMNI}"),
            "tool completed".into(),
            "reasoning Summary Planning a continuous shot.".into(),
            format!("file video/mp4 {OMNI}.mp4 {}", MP4.len()),
            // 20 × $1.50 + 300 × $9.00 + 173,760 × $12.00 per 1M.
            "usage Some(2.08785)".into(),
            "finish stop completed".into(),
        ]
    );
    let usage = evs.iter().find_map(|e| if let StreamEvent::Usage(u) = e { Some(u.clone()) } else { None }).unwrap();
    assert_eq!(
        (usage.input_tokens, usage.output_tokens, usage.reasoning_tokens, usage.cost_source),
        (Some(20), Some(174_060), Some(300), CostSource::Estimated)
    );
    let mut acc = crate::Accumulator::new();
    evs.into_iter().for_each(|e| acc.apply(e));
    let Part::ToolCall(t) = &acc.parts()[0] else { panic!("{:?}", acc.parts()) };
    assert_eq!(t.name, "video_generation");
    assert_eq!(
        t.input,
        Some(json!({"prompt": "A marble rolling fast on a chain reaction track",
                    "response_format": {"type": "video", "resolution": "4k", "delivery": "uri"}}))
    );
    assert_eq!(
        t.output,
        Some(json!({"status": "completed", "model": OMNI, "video": [{"type": "video", "mime_type": "video/mp4"}]}))
    );
    assert!(matches!(&acc.parts()[1], Part::Reasoning(r) if r.kind == ReasoningKind::Summary));
}

#[tokio::test]
async fn a_restart_polls_the_same_interaction() {
    let (base, seen) = server(created(), vec![completed(inline_video())], vec![]).await;
    let p = Prepared { url: format!("{base}/interactions"), ..prep(OMNI, &[]).await.unwrap() };
    // What the bot builds from the checkpoint (`Prepared::with_continuation`, dispatched by dialect).
    let resumed = p.with_continuation(&json!({"interaction_id": "v1_abc"}));
    assert_eq!(resumed.url, format!("{base}/interactions/v1_abc"));
    assert_eq!(resumed.with_continuation(&json!({"interaction_id": "v1_abc"})).url, resumed.url, "idempotent");
    let (r, outcome, evs) = run(&base, &resumed).await;
    assert!(r.is_ok());
    assert_eq!(outcome.upstream_id.as_deref(), Some("v1_abc"));
    assert_eq!(lines(&seen), ["GET /v1beta/interactions/v1_abc"], "no new job");
    assert!(!evs.iter().any(|e| matches!(e, StreamEvent::Checkpoint(_))));
    assert!(evs.iter().any(|e| matches!(e, StreamEvent::File { .. })));
}

#[tokio::test]
async fn a_uri_video_is_downloaded_from_the_files_api() {
    let uri = json!({"type": "video", "mime_type": "video/mp4", "uri": "{base}/files/f1:download?alt=media"});
    let (base, seen) = server(
        created(),
        vec![completed(uri)],
        vec![json!({"name": "files/f1", "state": "PROCESSING"}), json!({"name": "files/f1", "state": "ACTIVE"})],
    )
    .await;
    let p = Prepared { url: format!("{base}/interactions"), ..prep(OMNI, &[kv("sec", "8")]).await.unwrap() };
    let (r, _, evs) = run(&base, &p).await;
    assert!(r.is_ok(), "{r:?}");
    assert_eq!(
        lines(&seen),
        [
            "POST /v1beta/interactions",
            "GET /v1beta/interactions/v1_abc",
            "GET /v1beta/files/f1",
            "GET /v1beta/files/f1",
            "GET /v1beta/files/f1:download?alt=media",
        ]
    );
    assert!(seen.lock().unwrap().iter().all(|r| r.1.as_deref() == Some("test-key")));
    let file =
        evs.iter().find_map(|e| if let StreamEvent::File { data, name, .. } = e { Some((data, name)) } else { None });
    assert_eq!(file, Some((&MP4.to_vec(), &Some(format!("{OMNI}-8s.mp4")))));
    // A URI that is not a Files API file: fetched as it is, without the key.
    let other = json!({"type": "video", "mime_type": "video/mp4", "uri": "{base}/other/v.mp4"});
    let (base, seen) = server(created(), vec![completed(other)], vec![]).await;
    let p = Prepared { url: format!("{base}/interactions"), ..p };
    let (r, _, evs) = run(&base, &p).await;
    assert!(r.is_ok(), "{r:?}");
    let reqs = seen.lock().unwrap().clone();
    assert_eq!((reqs[2].0.as_str(), reqs[2].1.as_deref()), ("GET /v1beta/other/v.mp4", None));
    assert!(evs.iter().any(|e| matches!(e, StreamEvent::File { .. })));
}

#[tokio::test]
async fn failures_clear_the_checkpoint_and_are_classified() {
    let p = prep(OMNI, &[]).await.unwrap();
    // A safety block in the background: `failed` with the code in `errors`; never retried.
    let failed = json!({"id": "v1_abc", "status": "failed", "errors": [{"code": "safety",
        "message": "Request blocked due to safety violations (harmful content). Please modify your input and retry."}]});
    let (base, _) = server(created(), vec![failed], vec![]).await;
    let (r, outcome, evs) = run(&base, &Prepared { url: format!("{base}/interactions"), ..p.clone() }).await;
    let e = r.unwrap_err();
    assert_eq!((e.kind, e.retry, e.error_type.as_deref()), (ErrorKind::InBand, Retry::Never, Some("safety")));
    assert!(e.message.starts_with("Request blocked"), "{}", e.message);
    assert_eq!(e.details.unwrap()["status"], "failed", "the interaction, verbatim");
    let checkpoints: Vec<&Value> =
        evs.iter().filter_map(|e| if let StreamEvent::Checkpoint(c) = e { Some(c) } else { None }).collect();
    assert_eq!(checkpoints, [&json!({"interaction_id": "v1_abc"}), &Value::Null]);
    assert!(outcome.continuation.is_none());
    // Without a reason, or cancelled: retried with a new job.
    for (status, msg) in [("failed", "without an error"), ("cancelled", "was cancelled")] {
        let (base, _) = server(created(), vec![json!({"id": "v1_abc", "status": status})], vec![]).await;
        let (r, _, evs) = run(&base, &Prepared { url: format!("{base}/interactions"), ..p.clone() }).await;
        let e = r.unwrap_err();
        assert_eq!(e.retry, Retry::Backoff);
        assert!(e.message.contains(msg), "{}", e.message);
        assert!(matches!(
            evs.iter().rev().find(|e| matches!(e, StreamEvent::Checkpoint(_))),
            Some(StreamEvent::Checkpoint(Value::Null))
        ));
    }
    // An invalid request at creation (the API errors page's shape): no job, never retried.
    let bad = json!({"error": {"code": "invalid_request", "message": "Setting a duration over 10 seconds with two frames is not supported."}});
    let (base, seen) = server((400, bad), vec![], vec![]).await;
    let (r, _, evs) = run(&base, &Prepared { url: format!("{base}/interactions"), ..p.clone() }).await;
    let e = r.unwrap_err();
    assert_eq!((e.status, e.retry, e.error_type.as_deref()), (Some(400), Retry::Never, Some("invalid_request")));
    assert_eq!(e.details.unwrap()["error"]["code"], "invalid_request");
    assert!(evs.is_empty() && lines(&seen).len() == 1);
    // Google's front end (probed without a key): an array; by its gRPC status.
    let front = json!([{"error": {"code": 403, "message": "Permission denied on the service.", "status": "PERMISSION_DENIED",
        "details": [{"@type": "type.googleapis.com/google.rpc.ErrorInfo", "reason": "ACCESS_DENIED", "domain": "googleapis.com"}]}}]);
    let (base, _) = server((403, front), vec![], vec![]).await;
    let (r, _, _) = run(&base, &Prepared { url: format!("{base}/interactions"), ..p.clone() }).await;
    let e = r.unwrap_err();
    assert_eq!((e.status, e.retry, e.message.as_str()), (Some(403), Retry::Never, "Permission denied on the service."));
    assert!(e.details.unwrap().is_array(), "the body verbatim");
    // Completed without any output: an error, not an empty answer.
    let empty = json!({"id": "v1_abc", "status": "completed", "steps": [{"type": "model_output", "content": []}]});
    let (base, _) = server(created(), vec![empty], vec![]).await;
    let (r, _, _) = run(&base, &Prepared { url: format!("{base}/interactions"), ..p }).await;
    assert!(r.unwrap_err().message.contains("completed without a video"));
}

#[test]
fn interactions_error_codes() {
    let e = error_from_json(
        &json!({"code": "rate_limit_exceeded", "message": "slow down"}),
        Some(429),
        None,
        ErrorKind::Http,
    );
    assert_eq!(e.retry, Retry::After(20));
    let e = error_from_json(&json!({"code": "too_many_requests", "message": "x"}), Some(429), Some(7), ErrorKind::Http);
    assert_eq!(e.retry, Retry::After(7));
    let e = error_from_json(&json!({"code": "quota_exceeded", "message": "daily"}), Some(429), None, ErrorKind::Http);
    assert_eq!(e.retry, Retry::Never);
    for c in ["service_unavailable", "api_error", "deadline_exceeded", "gateway_timeout"] {
        assert_eq!(error_from_json(&json!({"code": c}), None, None, ErrorKind::InBand).retry, Retry::Backoff, "{c}");
    }
    for c in ["prohibited_content", "content_blocked", "jailbreak", "model_not_found", "payment_required"] {
        assert_eq!(error_from_json(&json!({"code": c}), None, None, ErrorKind::InBand).retry, Retry::Never, "{c}");
    }
    // `errors[].code` may be a URI; an unknown code follows the HTTP status.
    let e = error_from_json(
        &json!({"code": "https://example/errors/safety", "message": "m"}),
        None,
        None,
        ErrorKind::InBand,
    );
    assert_eq!((e.error_type.as_deref(), e.retry), (Some("safety"), Retry::Never));
    assert_eq!(error_from_json(&json!({"code": "new_code"}), Some(503), None, ErrorKind::Http).retry, Retry::Backoff);
    assert_eq!(error_from_json(&json!({"code": "new_code"}), None, None, ErrorKind::InBand).retry, Retry::Never);
}

#[test]
fn cost_by_the_serving_model_and_without_usage() {
    let u = json!({"total_input_tokens": 1000, "total_output_tokens": 57920,
                   "output_tokens_by_modality": [{"modality": "video", "tokens": 57_920}], "total_thought_tokens": 0});
    let body = json!({"response_format": {"type": "video", "resolution": "720p"}});
    let c = |model: &str, served: Option<&str>| usage(model, served, Some(&u), &body).cost_usd.unwrap();
    // 10 s at 720p on 1.2: ≈ $0.70 (the price list's own example).
    assert!((c(OMNI, None) - (1000.0 * 1.5 + 57_920.0 * 12.0) / 1e6).abs() < 1e-12);
    // Priced by the model that served it: an id outside the table served by 1.2 costs 1.2's rates.
    assert!((c("gemini-omni-1.1-flash", Some(OMNI)) - c(OMNI, None)).abs() < 1e-12);
    // Without a breakdown, the output is the video.
    let u2 = json!({"total_input_tokens": 0, "total_output_tokens": 1_000_000});
    assert!((usage(OMNI, None, Some(&u2), &body).cost_usd.unwrap() - 12.0).abs() < 1e-9);
    // Without usage: the length (10 s by default) at the resolution's rate; unknown models: unknown.
    let est = usage(OMNI, None, None, &json!({"response_format": {"resolution": "4k", "duration": "15s"}}));
    assert!((est.cost_usd.unwrap() - 17_376.0 * 15.0 * 12.0 / 1e6).abs() < 1e-9);
    assert_eq!(est.cost_source, CostSource::Estimated);
    for id in ["gemini-omni-1.1-flash", "gemini-omni-1.3-flash"] {
        assert_eq!(usage(id, None, Some(&u), &body).cost_source, CostSource::Unknown, "{id}");
    }
}
