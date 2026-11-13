//! Tests of the Gemini API provider (no network: synthetic chunks written per
//! the documented formats, and a local fake server for the HTTP path).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use omni_core::content::{NativeOutput, Part, ReasoningKind, RemoteMediaKind, Role};
use omni_core::trigger::OptionToken;
use omni_core::{BlobRef, Origin, Turn};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use super::errors::{BUDGET_EXHAUSTED, duration_secs, error_from_json};
use super::pricing::{self, GroundingUse, Tier};
use super::stream::ChunkParser;
use super::*;
use crate::error::{ErrorKind, Retry};
use crate::event::{Accumulator, StreamEvent};
use crate::openrouter::options::Applied;
use crate::{BoxFuture, NullObserver};

/// 2026-11-12T00:00:00Z: introductory Gemini 4 and Flash prices.
const NOW: i64 = 1_794_441_600;

struct Blobs(HashMap<String, Vec<u8>>);

impl Blobs {
    fn none() -> Self {
        Blobs(HashMap::new())
    }
    fn add(&mut self, bytes: &[u8], mime: &str) -> BlobRef {
        let r = BlobRef::for_bytes(bytes, mime);
        self.0.insert(r.sha256.clone(), bytes.to_vec());
        r
    }
}

impl BlobLoader for Blobs {
    fn load<'a>(&'a self, r: &'a BlobRef) -> BoxFuture<'a, anyhow::Result<Vec<u8>>> {
        Box::pin(async move { self.0.get(&r.sha256).cloned().ok_or_else(|| anyhow::anyhow!("missing blob")) })
    }
}

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

fn google() -> Google {
    Google::new(cfg("https://generativelanguage.googleapis.com/v1beta")).unwrap()
}

fn flag(f: &str) -> OptionToken {
    OptionToken::Flag(f.into())
}

fn kv(k: &str, v: &str) -> OptionToken {
    OptionToken::KeyValue(k.into(), v.into())
}

fn hi() -> Vec<Turn> {
    vec![Turn::user_text("hi")]
}

async fn prep_full(
    g: &Google,
    model: &str,
    opts: &[OptionToken],
    history: &[Turn],
    presets: &[Requested],
    blobs: &Blobs,
    user: Option<&str>,
) -> Result<Prepared, LlmError> {
    let input = CallInput {
        model,
        options: opts,
        history,
        session_id: Some("tg-1".into()),
        user: user.map(str::to_string),
        known_context: None,
        max_tokens_override: None,
    };
    g.prepare(&input, presets, blobs).await
}

async fn prep(model: &str, opts: &[OptionToken], history: &[Turn]) -> Result<Prepared, LlmError> {
    prep_full(&google(), model, opts, history, &[], &Blobs::none(), None).await
}

async fn prep_err(model: &str, opts: &[OptionToken], history: &[Turn]) -> String {
    prep(model, opts, history).await.expect_err("should be rejected").message
}

fn user_turn(parts: Vec<Part>) -> Turn {
    Turn { role: Role::User, origin: Origin::Live, parts, native: None }
}

fn file(blob: BlobRef, name: &str) -> Part {
    Part::File { blob, filename: Some(name.into()), generated: false }
}

fn location(lat: f64, lng: f64) -> Part {
    Part::Location { latitude: lat, longitude: lng, accuracy_m: None, title: None, address: None }
}

// ------------------------------------------------------------------ requests

#[test]
fn table_covers_the_configured_aliases() {
    // The `g g4 g31 gf g38f g37f g36f g35f gi` aliases of config/omni.example.toml.
    let g = google();
    for id in [
        "gemini-4-hydrogen",
        "gemini-4-carbon",
        "gemini-4-argon",
        "gemini-3.1-pro-preview",
        "gemini-3.8-flash",
        "gemini-3.7-flash",
        "gemini-3.6-flash",
        "gemini-3.5-flash",
        "gemini-nano-banana-2.1",
        "gemini-2.5-flash-image",
    ] {
        assert!(g.knows(id), "{id}");
        assert!(pricing::price(id, Tier::Standard, NOW).is_some(), "{id}");
    }
    assert_eq!(models::spec("gemini-3.7-flash").routes_to, Some("gemini-3.8-flash"));
    assert_eq!(models::spec("gemini-3.5-flash").routes_to, Some("gemini-3.6-flash"));
    assert!(!g.knows("gemini-3.8-flash-tts"));
    assert_eq!(g.id(), "google");
    assert_eq!(g.name(), "Gemini API");
    assert_eq!(crate::provider::info("google").unwrap().name, NAME);
}

#[tokio::test]
async fn gemini_4_defaults_are_the_maximum() {
    let p = prep("gemini-4-carbon", &[], &hi()).await.unwrap();
    assert_eq!(p.dialect, DIALECT);
    assert_eq!(
        p.url,
        "https://generativelanguage.googleapis.com/v1beta/models/gemini-4-carbon:streamGenerateContent?alt=sse"
    );
    assert_eq!(
        p.body,
        json!({
            "contents": [{"role": "user", "parts": [{"text": "hi"}]}],
            "generationConfig": {
                // Long decode: the total budget of all hops (D13).
                "maxOutputTokens": 393_216,
                "thinkingConfig": {"thinkingLevel": "high", "includeThoughts": true}
            }
        })
    );
    assert!(p.long_decode);
    assert_eq!(p.auto_max_tokens, Some(393_216));
    assert_eq!(p.max_tokens(), Some(393_216));
    assert_eq!(p.settings, vec!["effort high"]);
    assert!(p.warnings.is_empty() && p.notes.is_empty());
    let sources: Vec<&str> = p.applied.iter().map(|a| a.source.as_str()).collect();
    assert_eq!(sources, vec!["budget", "default"]);
    // The shared helpers find the output budget in its Gemini place.
    let mut q = p.clone();
    q.set_max_tokens(Some(500_000));
    assert_eq!(q.body["generationConfig"]["maxOutputTokens"], 500_000);
    assert!(q.body.get("max_tokens").is_none());
}

#[tokio::test]
async fn per_model_thinking_and_output_defaults() {
    let p = prep("gemini-3.8-flash", &[], &hi()).await.unwrap();
    assert_eq!(p.body["generationConfig"]["maxOutputTokens"], 65_536);
    assert_eq!(p.body["generationConfig"]["thinkingConfig"]["thinkingLevel"], "high");
    assert!(!p.long_decode);
    // A configured cap applies (not to Gemini 4's long-decode budget).
    let mut c = cfg("https://x/v1beta");
    c.max_tokens_cap = Some(32_000);
    let capped = Google::new(c).unwrap();
    let p = prep_full(&capped, "gemini-3.8-flash", &[], &hi(), &[], &Blobs::none(), None).await.unwrap();
    assert_eq!(p.body["generationConfig"]["maxOutputTokens"], 32_000);
    let p = prep_full(&capped, "gemini-4-argon", &[], &hi(), &[], &Blobs::none(), None).await.unwrap();
    assert_eq!(p.body["generationConfig"]["maxOutputTokens"], 393_216);

    let p = prep("gemini-nano-banana-2.1", &[], &hi()).await.unwrap();
    let g = &p.body["generationConfig"];
    assert_eq!(g["maxOutputTokens"], 32_768);
    assert_eq!(g["thinkingConfig"], json!({"thinkingLevel": "high", "includeThoughts": true}));
    assert_eq!(g["responseModalities"], json!(["TEXT", "IMAGE"]));
    assert!(g.get("imageConfig").is_none(), "no image size unless asked (presets set it)");

    let p = prep("gemini-3-pro-image", &[], &hi()).await.unwrap();
    assert_eq!(p.body["generationConfig"]["thinkingConfig"], json!({"includeThoughts": true}));
    assert_eq!(p.settings, vec!["reasoning on"]);

    let p = prep("gemini-2.5-pro", &[], &hi()).await.unwrap();
    assert_eq!(
        p.body["generationConfig"]["thinkingConfig"],
        json!({"thinkingBudget": 32_768, "includeThoughts": true})
    );
    assert_eq!(p.settings, vec!["thinking budget 32768"]);
    let p = prep("gemini-2.5-flash", &[kv("e", "none")], &hi()).await.unwrap();
    assert_eq!(p.body["generationConfig"]["thinkingConfig"], json!({"thinkingBudget": 0}));
    assert_eq!(p.settings, vec!["thinking off"]);
    assert_eq!(p.applied[1].source, "option");
    let p = prep("gemini-2.5-flash-lite", &[kv("budget", "2048")], &hi()).await.unwrap();
    assert_eq!(p.body["generationConfig"]["thinkingConfig"]["thinkingBudget"], 2048);

    let p = prep("gemini-3.6-flash", &[kv("e", "minimal")], &hi()).await.unwrap();
    assert_eq!(p.body["generationConfig"]["thinkingConfig"]["thinkingLevel"], "minimal");
    assert_eq!(p.settings, vec!["effort minimal"]);

    // The routing of a deprecated id is reported from the response (modelVersion), not guessed.
    let p = prep("gemini-3.7-flash", &[], &hi()).await.unwrap();
    assert!(p.warnings.is_empty(), "{:?}", p.warnings);

    // Gemini 4 Hydrogen: the highest of its four levels; long decode like Carbon and Argon.
    let p = prep("gemini-4-hydrogen", &[], &hi()).await.unwrap();
    assert_eq!(p.body["generationConfig"]["thinkingConfig"]["thinkingLevel"], "xhigh");
    assert_eq!(p.body["generationConfig"]["maxOutputTokens"], 393_216);
    assert!(p.long_decode);
    assert_eq!(p.settings, vec!["effort xhigh"]);
    assert!(prep("gemini-4-hydrogen", &[kv("e", "medium")], &hi()).await.is_ok());
    let e = prep_err("gemini-4-hydrogen", &[kv("e", "minimal")], &hi()).await;
    assert!(e.contains("levels: xhigh, high, medium, low"), "{e}");

    // Gemini 2.5 Flash Image: no thinking, ratios only.
    let p = prep("gemini-2.5-flash-image", &[flag("W")], &hi()).await.unwrap();
    let g = &p.body["generationConfig"];
    assert!(g.get("thinkingConfig").is_none(), "{g}");
    assert_eq!(g["imageConfig"], json!({"aspectRatio": "16:9"}));
    assert_eq!(g["maxOutputTokens"], 32_768);
    let e = prep_err("gemini-2.5-flash-image", &[flag("4")], &hi()).await;
    assert!(e.contains("has no image size option"), "{e}");
    let e = prep_err("gemini-2.5-flash-image", &[kv("e", "high")], &hi()).await;
    assert!(e.contains("no reasoning effort"), "{e}");
    let e = prep_err("gemini-2.5-flash-image", &[flag("s")], &hi()).await;
    assert!(e.contains("has no web search"), "{e}");
}

#[tokio::test]
async fn options_outside_the_api_are_rejected() {
    let e = prep_err("gemini-4-carbon", &[kv("e", "none")], &hi()).await;
    assert!(e.contains("cannot turn reasoning off"), "{e}");
    let e = prep_err("gemini-4-carbon", &[kv("e", "minimal")], &hi()).await;
    assert!(e.contains("levels: high, medium, low"), "{e}");
    let e = prep_err("gemini-3.8-flash", &[kv("e", "minimal")], &hi()).await;
    assert!(e.contains("levels: high, medium, low"), "{e}");
    let e = prep_err("gemini-nano-banana-2.1", &[kv("e", "low")], &hi()).await;
    assert!(e.contains("levels: high, medium, minimal"), "{e}");
    let e = prep_err("gemini-4-carbon", &[kv("budget", "1000")], &hi()).await;
    assert!(e.contains("has no reasoning token budget"), "{e}");
    let e = prep_err("gemini-2.5-pro", &[kv("budget", "64")], &hi()).await;
    assert!(e.contains("128–32768"), "{e}");
    let e = prep_err("gemini-2.5-pro", &[kv("e", "none")], &hi()).await;
    assert!(e.contains("cannot turn reasoning off"), "{e}");
    let e = prep_err("gemini-2.5-flash", &[kv("e", "high")], &hi()).await;
    assert!(e.contains("has no reasoning effort levels"), "{e}");
    let e = prep_err("gemini-3-pro-image", &[kv("e", "high")], &hi()).await;
    assert!(e.contains("has no reasoning effort levels"), "{e}");
    let e = prep_err("gemini-3.8-flash", &[kv("v", "high")], &hi()).await;
    assert!(e.contains("has no verbosity"), "{e}");
    let e = prep_err("gemini-3.8-flash", &[kv("p", "google-vertex")], &hi()).await;
    assert_eq!(e, "`p=` (provider choice) does not apply to Gemini API");
    let e = prep_err("gemini-3.8-flash", &[flag("x")], &hi()).await;
    assert!(e.contains("unknown option"), "{e}");
    let e = prep_err("gemini-3.8-flash", &[OptionToken::Json(json!({"temperature": 1}))], &hi()).await;
    assert!(e.contains("JSON options are not supported"), "{e}");
    let e = prep_err("models/gemini-3.8-flash", &[], &hi()).await;
    assert!(e.contains("is not a Gemini API model id"), "{e}");
}

#[tokio::test]
async fn tools_tiers_and_their_validation() {
    let p = prep("gemini-4-carbon", &[flag("scu"), kv("tier", "flex")], &hi()).await.unwrap();
    assert_eq!(p.body["tools"], json!([{"googleSearch": {}}, {"codeExecution": {}}, {"urlContext": {}}]));
    // The search and URL-context calls come back as parts, where they ran.
    assert_eq!(p.body["toolConfig"], json!({"includeServerSideToolInvocations": true}));
    assert!(p.applied.iter().any(|a| a.patch.pointer("/toolConfig/includeServerSideToolInvocations").is_some()));
    assert_eq!(p.body["service_tier"], "flex");
    assert_eq!(p.settings, vec!["effort high", "web search", "code execution", "URL fetching", "tier flex"]);
    // Code execution reports its parts anyway; no tools, nothing to ask for.
    assert!(prep("gemini-4-carbon", &[flag("c")], &hi()).await.unwrap().body.get("toolConfig").is_none());
    assert!(prep("gemini-3.8-flash", &[], &hi()).await.unwrap().body.get("toolConfig").is_none());
    // Not on Gemini 2.5, nor on the image models (the tool-combination guide names Gemini 3 and 4).
    assert!(prep("gemini-2.5-flash", &[flag("s")], &hi()).await.unwrap().body.get("toolConfig").is_none());
    let p = prep("gemini-3.8-flash", &[kv("tier", "priority")], &hi()).await.unwrap();
    assert_eq!(p.body["service_tier"], "priority");
    let p = prep("gemini-3.8-flash", &[kv("tier", "default")], &hi()).await.unwrap();
    assert!(p.body.get("service_tier").is_none());
    // Web and image search on the models that have both.
    for model in ["gemini-nano-banana-2.1", "gemini-3.1-flash-image"] {
        let p = prep(model, &[flag("s")], &hi()).await.unwrap();
        assert_eq!(p.body["tools"], json!([{"googleSearch": {"searchTypes": {"webSearch": {}, "imageSearch": {}}}}]));
        assert_eq!(p.settings, vec!["effort high", "web and image search"], "{model}");
        assert!(p.body.get("toolConfig").is_none(), "{model}");
    }
    let p = prep("gemini-3-pro-image", &[flag("s")], &hi()).await.unwrap();
    assert_eq!(p.body["tools"], json!([{"googleSearch": {}}]), "web search only");
    let p = prep("gemini-3.8-flash", &[flag("sm")], &hi()).await.unwrap();
    assert_eq!(p.body["tools"], json!([{"googleSearch": {}}, {"googleMaps": {}}]));

    let e = prep_err("gemini-nano-banana-2.1", &[flag("c")], &hi()).await;
    assert_eq!(e, "`gemini-nano-banana-2.1` has no code execution (`+c`)");
    let e = prep_err("gemini-nano-banana-2.1", &[flag("m")], &hi()).await;
    assert!(e.contains("has no Google Maps"), "{e}");
    let e = prep_err("gemini-3.1-flash-lite-image", &[flag("s")], &hi()).await;
    assert!(e.contains("has no web search"), "{e}");
    let e = prep_err("gemini-nano-banana-2.1", &[kv("tier", "flex")], &hi()).await;
    assert!(e.contains("has no service tiers"), "{e}");
    let e = prep_err("gemini-3.8-flash", &[kv("tier", "standard")], &hi()).await;
    assert!(e.contains("tiers: flex, priority"), "{e}");
    let e = prep_err("gemini-3.1-pro-preview", &[flag("sm")], &hi()).await;
    assert!(e.contains("cannot use web search and Google Maps together"), "{e}");
}

#[tokio::test]
async fn image_options_per_model() {
    let p = prep("gemini-nano-banana-2.1", &[flag("4W")], &hi()).await.unwrap();
    assert_eq!(p.body["generationConfig"]["imageConfig"], json!({"imageSize": "4K", "aspectRatio": "16:9"}));
    assert_eq!(p.settings, vec!["effort high", "image size 4K", "aspect ratio 16:9"]);
    assert!(prep("gemini-nano-banana-2.1", &[kv("ar", "1:8")], &hi()).await.is_ok());
    assert!(prep("gemini-3.1-flash-image", &[kv("ar", "8:1")], &hi()).await.is_ok());
    let e = prep_err("gemini-3-pro-image", &[kv("ar", "1:8")], &hi()).await;
    assert!(e.contains("aspect ratio 1:8 is not available") && e.contains("21:9"), "{e}");
    let e = prep_err("gemini-3.1-flash-lite-image", &[flag("4")], &hi()).await;
    assert_eq!(e, "image size 4K is not available for `gemini-3.1-flash-lite-image`; sizes: 1K");
    let e = prep_err("gemini-3.8-flash", &[flag("4")], &hi()).await;
    assert_eq!(e, "`gemini-3.8-flash` has no image size/aspect options");
    // Presets first, the user's options over them.
    let preset = Requested { image_size: Some("4K".into()), ..Default::default() };
    let p = prep_full(
        &google(),
        "gemini-nano-banana-2.1",
        &[flag("1")],
        &hi(),
        std::slice::from_ref(&preset),
        &Blobs::none(),
        None,
    )
    .await
    .unwrap();
    assert_eq!(p.body["generationConfig"]["imageConfig"]["imageSize"], "1K");
    let p = prep_full(&google(), "gemini-nano-banana-2.1", &[], &hi(), &[preset], &Blobs::none(), None).await.unwrap();
    assert_eq!(p.body["generationConfig"]["imageConfig"]["imageSize"], "4K");
    let a = p.applied.iter().find(|a| a.patch.pointer("/generationConfig/imageConfig").is_some()).unwrap();
    assert_eq!(a.source, "default");
}

#[tokio::test]
async fn maps_takes_the_latest_location() {
    let history = vec![
        user_turn(vec![location(1.0, 2.0), Part::text("first")]),
        Turn::assistant_text("ok"),
        user_turn(vec![location(31.2304, 121.4737), Part::text("cafes near me?")]),
    ];
    let p = prep("gemini-3.8-flash", &[flag("m")], &history).await.unwrap();
    assert_eq!(p.body["tools"], json!([{"googleMaps": {}}]));
    assert_eq!(
        p.body["toolConfig"],
        json!({"retrievalConfig": {"latLng": {"latitude": 31.2304, "longitude": 121.4737}},
               "includeServerSideToolInvocations": true})
    );
    assert_eq!(p.settings, vec!["effort high", "Google Maps near 31.2304,121.4737"]);
    // A location has no content form: only the text is sent.
    assert_eq!(p.body["contents"][2], json!({"role": "user", "parts": [{"text": "cafes near me?"}]}));

    let e = prep_err("gemini-3.8-flash", &[], &history).await;
    assert!(e.contains("add `+m`"), "{e}");
    let e = prep_err("gemini-nano-banana-2.1", &[], &history).await;
    assert!(e.contains("has no Google Maps grounding"), "{e}");

    let p = prep("gemini-4-argon", &[flag("m")], &hi()).await.unwrap();
    assert_eq!(p.body["toolConfig"], json!({"includeServerSideToolInvocations": true}), "no position");
    assert_eq!(p.settings, vec!["effort high", "Google Maps (no position sent)"]);

    let only_location = vec![user_turn(vec![location(1.0, 2.0)])];
    let e = prep_err("gemini-3.8-flash", &[flag("m")], &only_location).await;
    assert!(e.contains("add a question"), "{e}");

    let mut blobs = Blobs::none();
    let img = blobs.add(b"\x89PNG", "image/png");
    let history = vec![user_turn(vec![
        location(1.0, 2.0),
        Part::Image { blob: img, filename: None, generated: false, dims: None, thought: false },
        Part::text("what is this place?"),
    ])];
    let p = prep_full(&google(), "gemini-3.8-flash", &[flag("m")], &history, &[], &blobs, None).await.unwrap();
    assert!(p.warnings.iter().any(|w| w.contains("multimodal")), "{:?}", p.warnings);
}

fn native_turn(model: &str, parts: Value) -> Turn {
    Turn {
        role: Role::Assistant,
        origin: Origin::Live,
        parts: vec![Part::text("The answer is 42.")],
        native: Some(NativeOutput {
            dialect: DIALECT.into(),
            model: model.into(),
            payload: json!({"role": "model", "parts": parts}),
            context_tokens: None,
        }),
    }
}

#[tokio::test]
async fn history_replays_the_models_own_parts() {
    let parts = json!([
        {"text": "**Plan**", "thought": true},
        {"executableCode": {"language": "PYTHON", "code": "print(6*7)", "id": "c1"}},
        {"codeExecutionResult": {"outcome": "OUTCOME_OK", "output": "42\n", "id": "c1"}},
        {"text": "The answer is", "thoughtSignature": "Cs8B"},
        {"text": " 42."},
        {"text": "", "thoughtSignature": "Zm9v"}
    ]);
    let system =
        Turn { role: Role::System, origin: Origin::Synthetic, parts: vec![Part::text("Be brief.")], native: None };
    let history = vec![
        system,
        Turn::user_text("6*7?"),
        native_turn("gemini-3.8-flash", parts.clone()),
        Turn::user_text("thanks"),
    ];
    let p = prep("gemini-3.8-flash", &[], &history).await.unwrap();
    assert_eq!(p.body["systemInstruction"], json!({"parts": [{"text": "Be brief."}]}));
    assert_eq!(p.body["contents"][1], json!({"role": "model", "parts": parts}), "verbatim for the same model");
    assert_eq!(p.body["contents"].as_array().unwrap().len(), 3);

    // Another Gemini model (`/m`): the same parts ("the backend manages compatibility").
    let p = prep("gemini-4-carbon", &[], &history).await.unwrap();
    assert_eq!(p.body["contents"][1]["parts"], parts);

    // Other dialects (e.g. the same Gemini via OpenRouter) and synthetic turns: the text.
    let mut or = native_turn("gemini-3.8-flash", parts.clone());
    or.native.as_mut().unwrap().dialect = "openrouter.chat".into();
    let history = vec![Turn::user_text("6*7?"), or, Turn::user_text("thanks")];
    let p = prep("gemini-3.8-flash", &[], &history).await.unwrap();
    assert_eq!(p.body["contents"][1], json!({"role": "model", "parts": [{"text": "The answer is 42."}]}));
}

#[tokio::test]
async fn media_goes_natively_at_the_highest_resolution() {
    let mut b = Blobs::none();
    let img = b.add(b"\x89PNG\r\n", "image/png");
    let pdf = b.add(b"%PDF-1.7", "application/pdf");
    let rs = b.add("fn main() {} // 你好".as_bytes(), "application/octet-stream");
    let md = b.add(b"# Title", "text/markdown");
    let docx = b.add(b"PK\x03\x04", "application/vnd.openxmlformats-officedocument.wordprocessingml.document");
    let ods = b.add(b"PK\x03\x04ods", "application/octet-stream");
    let ogg = b.add(b"OggS", "audio/ogg");
    let mp4 = b.add(b"\0\0\0\x18ftypmp4", "video/mp4");
    let history = vec![user_turn(vec![
        Part::RemoteMedia { url: "https://www.youtube.com/watch?v=9hE5-98ZeCg".into(), kind: RemoteMediaKind::YouTube },
        Part::Image { blob: img.clone(), filename: None, generated: false, dims: None, thought: false },
        file(pdf.clone(), "paper.pdf"),
        file(rs, "main.rs"),
        file(md, "README.md"),
        file(docx.clone(), "report.docx"),
        file(ods, "sheet.ods"),
        file(ogg.clone(), "voice.ogg"),
        file(mp4.clone(), "clip.mp4"),
        Part::text("Compare all of these."),
    ])];
    let p = prep_full(&google(), "gemini-3.8-flash", &[], &history, &[], &b, None).await.unwrap();
    let parts = p.body["contents"][0]["parts"].as_array().unwrap().clone();
    let b64 = |x: &[u8]| base64::engine::general_purpose::STANDARD.encode(x);
    assert_eq!(
        parts[0],
        json!({"fileData": {"fileUri": "https://www.youtube.com/watch?v=9hE5-98ZeCg"},
               "mediaResolution": {"level": "MEDIA_RESOLUTION_HIGH"}})
    );
    assert_eq!(
        parts[1],
        json!({"inlineData": {"mimeType": "image/png", "data": b64(b"\x89PNG\r\n")},
               "mediaResolution": {"level": "MEDIA_RESOLUTION_ULTRA_HIGH"}})
    );
    assert_eq!(parts[2]["inlineData"]["mimeType"], "application/pdf");
    assert_eq!(parts[2]["inlineData"]["displayName"], "paper.pdf");
    assert_eq!(parts[2]["mediaResolution"]["level"], "MEDIA_RESOLUTION_HIGH");
    // Text files natively (no inlining, no note): their own listed type, else text/plain.
    assert_eq!(parts[3]["inlineData"]["mimeType"], "text/plain");
    assert_eq!(parts[3]["inlineData"]["data"], b64("fn main() {} // 你好".as_bytes()));
    assert_eq!(parts[4]["inlineData"]["mimeType"], "text/markdown");
    assert_eq!(parts[5]["inlineData"]["mimeType"], docx.mime);
    assert!(parts[5].get("mediaResolution").is_none());
    assert_eq!(parts[6]["inlineData"]["mimeType"], "application/vnd.oasis.opendocument.spreadsheet");
    assert_eq!(parts[7]["inlineData"]["mimeType"], "audio/ogg");
    assert!(parts[7].get("mediaResolution").is_none());
    assert_eq!(parts[8]["inlineData"]["mimeType"], "video/mp4");
    assert_eq!(parts[8]["mediaResolution"]["level"], "MEDIA_RESOLUTION_HIGH");
    assert_eq!(parts[9], json!({"text": "Compare all of these."}));
    assert!(p.notes.is_empty());
    let a = p.applied.iter().find(|a| a.patch.get("mediaResolution (per part)").is_some()).unwrap();
    assert_eq!(a.source, "default");

    // Image models take no per-part media resolution and fewer kinds of input.
    let one = |part: Part| vec![user_turn(vec![part, Part::text("?")])];
    let image = Part::Image { blob: img, filename: None, generated: false, dims: None, thought: false };
    let p = prep_full(&google(), "gemini-nano-banana-2.1", &[], &one(image), &[], &b, None).await.unwrap();
    assert!(p.body["contents"][0]["parts"][0].get("mediaResolution").is_none());
    let check = |model: &'static str, part: Part, what: &'static str| {
        let b = &b;
        async move {
            let e = prep_full(&google(), model, &[], &one(part), &[], b, None).await.unwrap_err().message;
            assert!(e.contains(&format!("does not take {what} input")), "{model}: {e}");
        }
    };
    check("gemini-nano-banana-2.1", file(ogg, "voice.ogg"), "audio").await;
    check("gemini-nano-banana-2.1", file(docx, "report.docx"), "Office/OpenDocument").await;
    check("gemini-3-pro-image", file(pdf, "paper.pdf"), "PDF").await;
    check("gemini-3-pro-image", file(mp4, "clip.mp4"), "video").await;

    let bad = b.add(&[0xff, 0xfe, 0x00], "text/plain");
    let e = prep_full(&google(), "gemini-3.8-flash", &[], &one(file(bad, "x.txt")), &[], &b, None)
        .await
        .unwrap_err()
        .message;
    assert!(e.contains("not valid UTF-8"), "{e}");
    let zip = b.add(b"PK", "application/zip");
    let e = prep_full(&google(), "gemini-3.8-flash", &[], &one(file(zip, "x.zip")), &[], &b, None)
        .await
        .unwrap_err()
        .message;
    assert!(e.contains("unsupported file type application/zip"), "{e}");
    let remote = Part::RemoteMedia { url: "https://example.com/a.mp4".into(), kind: RemoteMediaKind::Video };
    let e = prep_err("gemini-3.8-flash", &[], &one(remote)).await;
    assert!(e.contains("only YouTube links"), "{e}");
}

#[tokio::test]
async fn end_user_label_and_unknown_models() {
    let hash = "a".repeat(64);
    let p = prep_full(&google(), "gemini-3.8-flash", &[], &hi(), &[], &Blobs::none(), Some(&hash)).await.unwrap();
    assert_eq!(p.body["labels"], json!({"safety_identifier": "a".repeat(63)}));
    assert!(p.applied.iter().any(|a: &Applied| a.patch.get("labels").is_some() && a.source == "default"));
    assert!(p.body.get("session_id").is_none(), "no documented cache key");

    // A new Gemini 4 model: the family's defaults, with a warning.
    let p = prep("gemini-4-neon", &[], &hi()).await.unwrap();
    assert!(p.long_decode);
    assert_eq!(p.body["generationConfig"]["maxOutputTokens"], 393_216);
    assert_eq!(p.body["generationConfig"]["thinkingConfig"]["thinkingLevel"], "high");
    assert!(p.warnings[0].contains("not in the bot's Gemini API model table"), "{:?}", p.warnings);
    assert!(!google().knows("gemini-4-neon"));
    // Something else: only what is asked for.
    let p = prep("gemma-5-27b-it", &[], &hi()).await.unwrap();
    assert!(p.body.get("generationConfig").is_none());
    assert!(p.settings.is_empty());
}

// ------------------------------------------------------------------ streams

fn parser(model: &str, body: Value) -> ChunkParser {
    ChunkParser::new(model, &body, NOW, Vec::new())
}

fn run(p: &mut ChunkParser, chunks: &[Value]) -> Vec<StreamEvent> {
    chunks.iter().flat_map(|c| p.chunk(c).unwrap()).collect()
}

fn accumulate(evs: &[StreamEvent]) -> Accumulator {
    let mut a = Accumulator::new();
    for e in evs {
        a.apply(e.clone());
    }
    a
}

fn usages(evs: &[StreamEvent]) -> Vec<&omni_core::Usage> {
    evs.iter().filter_map(|e| if let StreamEvent::Usage(u) = e { Some(u) } else { None }).collect()
}

#[test]
fn stream_thoughts_text_signature_and_usage() {
    // Shapes of docs/research/gemini.md §7.
    let chunks = [
        json!({"candidates": [{"content": {"role": "model", "parts": [{"text": "**Plan**\n\nAdd them.", "thought": true}]}, "index": 0}],
               "usageMetadata": {"promptTokenCount": 812, "thoughtsTokenCount": 214},
               "modelVersion": "gemini-3.8-flash", "responseId": "mAitaLmkHPPlz7IPvtfUqQ4"}),
        json!({"candidates": [{"content": {"role": "model", "parts": [{"text": "The answer is", "thoughtSignature": "Cs8B"}]}, "index": 0}],
               "usageMetadata": {"promptTokenCount": 812, "candidatesTokenCount": 3, "thoughtsTokenCount": 214},
               "modelVersion": "gemini-3.8-flash", "responseId": "mAitaLmkHPPlz7IPvtfUqQ4"}),
        json!({"candidates": [{"content": {"role": "model", "parts": [{"text": " 42."}]}, "finishReason": "STOP", "index": 0}],
               "usageMetadata": {"promptTokenCount": 812, "candidatesTokenCount": 5, "thoughtsTokenCount": 214, "totalTokenCount": 1031,
                                 "promptTokensDetails": [{"modality": "TEXT", "tokenCount": 812}]},
               "modelVersion": "gemini-3.8-flash", "responseId": "mAitaLmkHPPlz7IPvtfUqQ4"}),
    ];
    let mut p = parser("gemini-3.8-flash", json!({}));
    let evs = run(&mut p, &chunks);
    assert!(p.finished);
    assert!(matches!(&evs[0], StreamEvent::Meta { upstream_id: Some(id), model: Some(m), .. }
        if id == "mAitaLmkHPPlz7IPvtfUqQ4" && m == "gemini-3.8-flash"));
    assert_eq!(
        evs[1],
        StreamEvent::Reasoning { kind: ReasoningKind::Summary, text: "**Plan**\n\nAdd them.".into(), block: 0 }
    );
    assert!(
        matches!(&evs[2], StreamEvent::LiveUsage(u) if u.input_tokens == Some(812) && u.output_tokens == Some(214))
    );
    assert_eq!(evs[3], StreamEvent::Text("The answer is".into()));
    assert!(matches!(&evs[4], StreamEvent::LiveUsage(_)));
    assert_eq!(evs[5], StreamEvent::Text(" 42.".into()));
    assert_eq!(evs[6], StreamEvent::Finish { reason: Some("stop".into()), native: Some("STOP".into()) });
    assert!(matches!(&evs[7], StreamEvent::Usage(_)) && evs.len() == 8);
    let u = usages(&evs);
    assert_eq!(u.len(), 1, "only the end of the stream is billed");
    assert_eq!((u[0].input_tokens, u[0].output_tokens, u[0].reasoning_tokens), (Some(812), Some(219), Some(214)));
    let expected = (812.0 * 0.75 + 219.0 * 3.75) / 1e6;
    assert!((u[0].cost_usd.unwrap() - expected).abs() < 1e-12);
    assert_eq!(u[0].cost_source, omni_core::usage::CostSource::Estimated);
    assert_eq!(u[0].raw, chunks[2]["usageMetadata"]);
    // Replay payload: every part exactly as received.
    assert_eq!(
        p.native_payload(),
        json!({"role": "model", "parts": [
            {"text": "**Plan**\n\nAdd them.", "thought": true},
            {"text": "The answer is", "thoughtSignature": "Cs8B"},
            {"text": " 42."}
        ]})
    );
    let a = accumulate(&evs);
    assert!(matches!(&a.parts()[0], Part::Reasoning(r) if r.kind == ReasoningKind::Summary));
    assert_eq!(a.turn().answer_text(), "The answer is 42.");
    assert!(p.continuation().is_none());
}

#[test]
fn search_grounding_becomes_a_tool_call_and_citations() {
    let t1 = "Spain won Euro 2024, defeating England 2-1 in the final.";
    let t2 = " This victory marks Spain's record fourth European Championship title.";
    let suggestions = json!([["UEFA Euro 2024 winner", "https://www.google.com/search?q=UEFA+Euro+2024+winner"]]);
    let blob = base64::engine::general_purpose::STANDARD.encode(suggestions.to_string());
    let s2 = t1.len() + 1;
    let chunks = [
        json!({"candidates": [{"content": {"role": "model", "parts": [{"text": t1}]}}], "responseId": "r1"}),
        json!({"candidates": [{"content": {"role": "model", "parts": [{"text": t2}]},
            "finishReason": "STOP",
            "groundingMetadata": {
                "webSearchQueries": ["UEFA Euro 2024 winner", "who won euro 2024", ""],
                "searchEntryPoint": {"renderedContent": "<!-- HTML and CSS for the search widget -->", "sdkBlob": blob},
                "groundingChunks": [
                    {"web": {"uri": "https://vertexaisearch.cloud.google.com/grounding-api-redirect/a", "title": "aljazeera.com"}},
                    {"web": {"uri": "https://vertexaisearch.cloud.google.com/grounding-api-redirect/b", "title": "uefa.com"}}
                ],
                "groundingSupports": [
                    {"segment": {"endIndex": t1.len(), "text": t1}, "groundingChunkIndices": [0]},
                    {"segment": {"startIndex": s2, "endIndex": t1.len() + t2.len(), "text": t2.trim_start()},
                     "groundingChunkIndices": [0, 1], "confidenceScores": [0.9, 0.8]}
                ]
            }}],
            "usageMetadata": {"promptTokenCount": 100, "candidatesTokenCount": 40, "toolUsePromptTokenCount": 300}}),
    ];
    let mut p = parser("gemini-3.8-flash", json!({"tools": [{"googleSearch": {}}]}));
    let evs = run(&mut p, &chunks);
    let tool = evs.iter().find_map(|e| if let StreamEvent::ToolBefore(t) = e { Some(t) } else { None }).unwrap();
    assert_eq!(tool.name, "google_search");
    assert_eq!(tool.input, Some(json!({"queries": ["UEFA Euro 2024 winner", "who won euro 2024", ""]})));
    assert_eq!(tool.output, Some(json!({"search_suggestions": suggestions})));
    let cites = evs.iter().find_map(|e| if let StreamEvent::Citations(c) = e { Some(c) } else { None }).unwrap();
    assert_eq!(cites.len(), 2);
    assert_eq!(cites[0].url.as_deref(), Some("https://vertexaisearch.cloud.google.com/grounding-api-redirect/a"));
    assert_eq!(cites[0].title.as_deref(), Some("aljazeera.com"));
    assert_eq!(cites[0].range, Some((0, t1.len())));
    assert_eq!(cites[1].range, Some((s2, t1.len() + t2.len())));
    assert_eq!(cites[1].extra.as_ref().unwrap()["supports"][0]["confidenceScores"], json!([0.9, 0.8]));
    // The search ran before the answer (it is only reported with the last chunk);
    // the sources follow the text they ground (attached to it).
    let a = accumulate(&evs);
    assert!(matches!(&a.parts()[0], Part::ToolCall(t) if t.name == "google_search"));
    assert!(matches!(&a.parts()[1], Part::Text { citations, .. } if citations.len() == 2));
    // Two billable queries (the empty one is not billed); tool-use tokens at the input rate.
    let u = usages(&evs)[0];
    assert_eq!(u.input_tokens, Some(400));
    let expected = (400.0 * 0.75 + 40.0 * 3.75) / 1e6 + 2.0 * 0.014;
    assert!((u.cost_usd.unwrap() - expected).abs() < 1e-12, "{:?}", u.cost_usd);
}

#[test]
fn support_offsets_are_utf8_bytes_per_part() {
    let thought = "思考中";
    let text = "西班牙赢得了2024年欧洲杯。这是第四次夺冠。";
    let first = "西班牙赢得了2024年欧洲杯。";
    let second = "这是第四次夺冠。";
    let chunks = [
        json!({"candidates": [{"content": {"parts": [{"text": thought, "thought": true}]}}]}),
        json!({"candidates": [{"content": {"parts": [{"text": &text[..9]}]}}]}),
        json!({"candidates": [{"content": {"parts": [{"text": &text[9..]}]}}]}),
        json!({"candidates": [{"finishReason": "STOP", "groundingMetadata": {
        "groundingChunks": [{"web": {"uri": "https://a", "title": "a"}}, {"web": {"uri": "https://b", "title": "b"}},
                            {"web": {"uri": "https://c", "title": "c"}}],
        "groundingSupports": [
            // Bytes within part 1 (part 0 is the thought summary).
            {"segment": {"partIndex": 1, "startIndex": first.len(), "endIndex": text.len(), "text": second},
             "groundingChunkIndices": [0]},
            // Without partIndex: offsets into the answer.
            {"segment": {"startIndex": 0, "endIndex": first.len(), "text": first}, "groundingChunkIndices": [1]},
            // Offsets that do not fit: found by the segment text.
            {"segment": {"startIndex": 5, "endIndex": 7, "text": second}, "groundingChunkIndices": [2]}
        ]}}]}),
    ];
    let mut p = parser("gemini-3.8-flash", json!({}));
    let evs = run(&mut p, &chunks);
    let cites = evs.iter().find_map(|e| if let StreamEvent::Citations(c) = e { Some(c) } else { None }).unwrap();
    assert_eq!(cites[0].range, Some((first.len(), text.len())));
    assert_eq!(cites[1].range, Some((0, first.len())));
    assert_eq!(cites[2].range, Some((first.len(), text.len())));
    assert_eq!(&text[cites[0].range.unwrap().0..], second);
}

#[test]
fn maps_grounding_attributes_google_maps() {
    // Shapes of the Maps grounding guide.
    let text = "CanteenM is an American restaurant with a 4.6-star rating and is open 24 hours.";
    let chunks = [json!({"candidates": [{"content": {"parts": [{"text": text}], "role": "model"},
        "finishReason": "STOP",
        "groundingMetadata": {
            "groundingChunks": [{"maps": {
                "uri": "https://maps.google.com/?cid=13100894621228039586",
                "title": "Heaven on 7th Marketplace",
                "placeId": "places/ChIJ0-zA1vBZwokRon0fGj-6z7U",
                "placeAnswerSources": {"reviewSnippets": [
                    {"reviewId": "rv1", "googleMapsUri": "https://maps.google.com/?cid=1&review=rv1", "title": "Great coffee"}
                ]}}}],
            "groundingSupports": [{"segment": {"startIndex": 0, "endIndex": 79, "text": text}, "groundingChunkIndices": [0]}],
            "webSearchQueries": ["restaurants near me"]
        }}],
        "usageMetadata": {"promptTokenCount": 10, "candidatesTokenCount": 20}})];
    let mut p = parser("gemini-3.8-flash", json!({"tools": [{"googleMaps": {}}]}));
    let evs = run(&mut p, &chunks);
    let tool = evs.iter().find_map(|e| if let StreamEvent::ToolBefore(t) = e { Some(t) } else { None }).unwrap();
    assert_eq!(tool.name, "google_maps");
    let cites = evs.iter().find_map(|e| if let StreamEvent::Citations(c) = e { Some(c) } else { None }).unwrap();
    assert_eq!(cites[0].title.as_deref(), Some("Heaven on 7th Marketplace · Google Maps"));
    assert_eq!(cites[0].url.as_deref(), Some("https://maps.google.com/?cid=13100894621228039586"));
    assert_eq!(cites[0].range, Some((0, 79)));
    assert_eq!(cites[1].title.as_deref(), Some("Great coffee · Google Maps"));
    assert_eq!(cites[1].url.as_deref(), Some("https://maps.google.com/?cid=1&review=rv1"));
    let u = usages(&evs)[0];
    let expected = (10.0 * 0.75 + 20.0 * 3.75) / 1e6 + 0.014;
    assert!((u.cost_usd.unwrap() - expected).abs() < 1e-12);
}

#[test]
fn code_execution_url_context_and_images() {
    let png = b"\x89PNG\r\n\x1a\n-plot";
    let chunks = [
        json!({"candidates": [{"content": {"parts": [{"executableCode": {"language": "PYTHON", "code": "print(sum(range(10)))"}}]}}]}),
        json!({"candidates": [{"content": {"parts": [{"codeExecutionResult": {"outcome": "OUTCOME_OK", "output": "45\n"}}]}}]}),
        json!({"candidates": [{"content": {"parts": [
            {"inlineData": {"mimeType": "image/png", "data": base64::engine::general_purpose::STANDARD.encode(png)}},
            {"text": "The sum is 45."}
        ]}, "urlContextMetadata": {"urlMetadata": [
            {"retrievedUrl": "https://example.com/a", "urlRetrievalStatus": "URL_RETRIEVAL_STATUS_SUCCESS"},
            {"retrievedUrl": "https://example.com/paywalled", "urlRetrievalStatus": "URL_RETRIEVAL_STATUS_PAYWALL"}
        ]}, "finishReason": "STOP"}]}),
    ];
    let mut p = parser("gemini-3.8-flash", json!({}));
    let evs = run(&mut p, &chunks);
    let tools: Vec<&ToolCall> = evs
        .iter()
        .filter_map(|e| match e {
            StreamEvent::Tool(t) | StreamEvent::ToolBefore(t) => Some(t),
            _ => None,
        })
        .collect();
    assert_eq!(tools[0].name, "code_execution");
    assert_eq!(tools[0].status.as_deref(), Some("in_progress"));
    assert_eq!(tools[0].input.as_ref().unwrap()["code"], "print(sum(range(10)))");
    assert_eq!(tools[1].id, tools[0].id, "the result updates its call");
    assert_eq!(tools[1].status.as_deref(), Some("completed"));
    assert_eq!(tools[1].output.as_ref().unwrap()["output"], "45\n");
    assert_eq!(tools[2].name, "url_context");
    assert_eq!(tools[2].input, Some(json!({"urls": ["https://example.com/a", "https://example.com/paywalled"]})));
    assert!(evs.iter().any(|e| matches!(e, StreamEvent::Image { data, mime } if data == png && mime == "image/png")));
    let a = accumulate(&evs);
    assert!(
        matches!(&a.parts()[0], Part::ToolCall(t) if t.status.as_deref() == Some("completed") && t.input.is_some() && t.output.is_some())
    );
}

use omni_core::content::ToolCall;

#[test]
fn image_model_thought_images_and_image_pricing() {
    let img = |b: &[u8]| base64::engine::general_purpose::STANDARD.encode(b);
    let chunks = [json!({"candidates": [{"content": {"parts": [
            {"text": "Composing", "thought": true},
            {"inlineData": {"mimeType": "image/png", "data": img(b"draft")}, "thought": true},
            {"text": "Here is your cat.", "thoughtSignature": "SigA"},
            {"inlineData": {"mimeType": "image/png", "data": img(b"final")}, "thoughtSignature": "SigB"}
        ]}, "finishReason": "STOP"}],
        "usageMetadata": {"promptTokenCount": 20, "candidatesTokenCount": 1130, "thoughtsTokenCount": 100,
                          "candidatesTokensDetails": [{"modality": "TEXT", "tokenCount": 10}, {"modality": "IMAGE", "tokenCount": 1120}]}})];
    let mut p = parser("gemini-nano-banana-2.1", json!({}));
    let evs = run(&mut p, &chunks);
    // The draft made while thinking stays with the reasoning; the answer image is an image.
    let images: Vec<(&Vec<u8>, bool)> = evs
        .iter()
        .filter_map(|e| match e {
            StreamEvent::Image { data, .. } => Some((data, false)),
            StreamEvent::ThoughtImage { data, .. } => Some((data, true)),
            _ => None,
        })
        .collect();
    assert_eq!(images, vec![(&b"draft".to_vec(), true), (&b"final".to_vec(), false)]);
    assert_eq!(p.native_payload()["parts"], chunks[0]["candidates"][0]["content"]["parts"]);
    // 1,120 image tokens at $30/M ($0.0336 per 1K image); text and thinking at $7.50/M.
    let u = usages(&evs)[0];
    let expected = (20.0 * 1.50 + 110.0 * 7.50 + 1120.0 * 30.0) / 1e6;
    assert!((u.cost_usd.unwrap() - expected).abs() < 1e-12);
}

#[test]
fn long_decode_checkpoints_and_continuation() {
    let chunks = [
        json!({"candidates": [{"content": {"parts": [{"text": "a", "thought": true}]}}], "responseId": "h1"}),
        // A checkpoint (after 32K tokens): token without finishReason.
        json!({"candidates": [{"content": {"parts": [{"text": "b", "thought": true}]}, "continuationToken": "CK1"}],
               "usageMetadata": {"promptTokenCount": 142, "thoughtsTokenCount": 40000}}),
        json!({"candidates": [{"content": {"parts": [{"text": "c", "thought": true}]}}],
               "usageMetadata": {"promptTokenCount": 142, "thoughtsTokenCount": 50000}}),
        // The hop's end, as in the long-decode guide.
        json!({"candidates": [{"content": {"role": "model", "parts": [{"text": ""}]}, "finishReason": "CONTINUATION",
                               "continuationToken": "CiQ4ZmQ3"}],
               "usageMetadata": {"promptTokenCount": 142, "candidatesTokenCount": 0, "thoughtsTokenCount": 65536,
                                 "totalTokenCount": 65678}}),
    ];
    let mut p = parser("gemini-4-carbon", json!({}));
    let evs = run(&mut p, &chunks);
    let cp = evs.iter().position(|e| matches!(e, StreamEvent::Checkpoint(_))).unwrap();
    // Billed up to the checkpoint before it (a drop after it is paid up to it).
    assert!(matches!(&evs[cp - 1], StreamEvent::Usage(u) if u.output_tokens == Some(40000)));
    assert_eq!(
        evs[cp],
        StreamEvent::Checkpoint(json!({"continuationToken": "CK1", "parts": [
            {"text": "a", "thought": true}, {"text": "b", "thought": true}
        ]}))
    );
    assert!(evs.iter().any(|e| matches!(e, StreamEvent::LiveUsage(u) if u.output_tokens == Some(50000))));
    assert!(evs.contains(&StreamEvent::Finish { reason: Some("length".into()), native: Some("CONTINUATION".into()) }));
    assert_eq!(usages(&evs).last().unwrap().output_tokens, Some(65536));
    let item = p.continuation().unwrap().clone();
    assert_eq!(item, json!({"continuationToken": "CiQ4ZmQ3"}), "the final token resumes after this hop");

    // The next hop: the identical request plus the token.
    let p0 = Prepared {
        dialect: DIALECT,
        model: "gemini-4-carbon".into(),
        url: "https://x/models/gemini-4-carbon:streamGenerateContent?alt=sse".into(),
        body: json!({"contents": [{"role": "user", "parts": [{"text": "prove it"}]}],
                     "generationConfig": {"maxOutputTokens": 393216}}),
        applied: vec![],
        warnings: vec![],
        notes: vec![],
        show_debug: false,
        auto_max_tokens: Some(393_216),
        long_decode: true,
        settings: vec![],
    };
    let hop = p0.with_continuation(&item);
    let mut expected = p0.body.clone();
    expected["continuationToken"] = json!("CiQ4ZmQ3");
    assert_eq!(hop.body, expected);
    assert_eq!(hop.url, p0.url);

    // MAX_TOKENS keeps its token (`/continue` with a higher budget); STOP has nothing to resume.
    let mut p = parser("gemini-4-carbon", json!({}));
    let evs = run(
        &mut p,
        &[
            json!({"candidates": [{"content": {"parts": [{"text": "x"}]}, "finishReason": "MAX_TOKENS", "continuationToken": "T9"}]}),
        ],
    );
    assert!(evs.contains(&StreamEvent::Finish { reason: Some("length".into()), native: Some("MAX_TOKENS".into()) }));
    assert_eq!(p.continuation(), Some(&json!({"continuationToken": "T9"})));
    let mut p = parser("gemini-4-carbon", json!({}));
    run(
        &mut p,
        &[
            json!({"candidates": [{"content": {"parts": [{"text": "x"}]}, "continuationToken": "T1"}]}),
            json!({"candidates": [{"content": {"parts": [{"text": "y"}]}, "finishReason": "STOP"}]}),
        ],
    );
    assert!(p.continuation().is_none());
}

#[test]
fn merging_hops() {
    let h1 = json!({"role": "model", "parts": [{"text": "think", "thought": true}, {"text": "A "}, {"text": "B ", "thoughtSignature": "s1"}]});
    let h2 = json!({"role": "model", "parts": [{"text": "D"}]});
    // All hops present: parts in order.
    let full = json!({"role": "model", "parts": [{"text": "D"}]});
    assert_eq!(merge_natives(std::slice::from_ref(&h2), "D"), full);
    assert_eq!(crate::merge_natives(DIALECT, &[h1.clone(), h2.clone()], "A B D")["parts"].as_array().unwrap().len(), 4);
    // A hop resumed after a restart lost the parts before its checkpoint ("C "): put back as text.
    let merged = merge_natives(&[h1.clone(), h2.clone()], "A B C D");
    assert_eq!(
        merged["parts"],
        json!([{"text": "think", "thought": true}, {"text": "A "}, {"text": "B ", "thoughtSignature": "s1"}, {"text": "C "}, {"text": "D"}])
    );
    let tail = merge_natives(std::slice::from_ref(&h1), "A B tail");
    assert_eq!(tail["parts"].as_array().unwrap().last().unwrap(), &json!({"text": "tail"}));
    // Parts that do not align with the shown text are left as they are.
    assert_eq!(merge_natives(std::slice::from_ref(&h1), "something else")["parts"], h1["parts"]);
}

#[test]
fn blocks_and_abnormal_finishes() {
    let mut p = parser("gemini-3.8-flash", json!({}));
    let block = json!({"promptFeedback": {"blockReason": "PROHIBITED_CONTENT"},
                       "usageMetadata": {"promptTokenCount": 12, "totalTokenCount": 12},
                       "modelVersion": "gemini-3.8-flash", "responseId": "r"});
    let evs = run(&mut p, &[block]);
    assert!(p.finished);
    assert_eq!(p.abnormal().unwrap().message, "the prompt was blocked: PROHIBITED_CONTENT");
    assert!(evs.contains(&StreamEvent::FinishDetails(json!({"blockReason": "PROHIBITED_CONTENT"}))));
    assert!(evs.contains(&StreamEvent::Finish {
        reason: Some("content_filter".into()),
        native: Some("PROHIBITED_CONTENT".into())
    }));
    assert_eq!(usages(&evs)[0].input_tokens, Some(12));

    let mut p = parser("gemini-3.8-flash", json!({}));
    let ratings = json!([{"category": "HARM_CATEGORY_DANGEROUS_CONTENT", "probability": "HIGH", "blocked": true}]);
    let evs = run(
        &mut p,
        &[
            json!({"candidates": [{"content": {"parts": [{"text": "Sure, "}]}}]}),
            json!({"candidates": [{"finishReason": "SAFETY", "safetyRatings": ratings, "index": 0}]}),
        ],
    );
    assert!(evs.contains(&StreamEvent::FinishDetails(
        json!({"finishReason": "SAFETY", "safetyRatings": ratings, "index": 0})
    )));
    assert!(
        evs.contains(&StreamEvent::Finish { reason: Some("content_filter".into()), native: Some("SAFETY".into()) })
    );
    let e = p.abnormal().unwrap();
    assert_eq!(e.message, "SAFETY: stopped by the provider's content filter");
    assert_eq!((e.kind, e.retry, e.error_type.as_deref()), (ErrorKind::InBand, Retry::Never, Some("content_filter")));
    assert!(e.partial_output);
    assert_eq!(e.details.as_ref().unwrap()["safetyRatings"], ratings);

    let mut p = parser("gemini-3.8-flash", json!({}));
    let evs = run(
        &mut p,
        &[json!({"candidates": [{"content": {"parts": [{"text": "def f():"}]}, "finishReason": "RECITATION",
        "citationMetadata": {"citationSources": [
            {"startIndex": 0, "endIndex": 8, "uri": "", "license": ""},
            {"startIndex": 0, "endIndex": 8, "uri": "https://github.com/x/y", "license": "mit"}]}}]})],
    );
    let cites = evs.iter().find_map(|e| if let StreamEvent::Citations(c) = e { Some(c) } else { None }).unwrap();
    assert_eq!(cites.len(), 1, "a recitation without a link is not listed: {cites:?}");
    assert_eq!(cites[0].url.as_deref(), Some("https://github.com/x/y"));
    assert_eq!(cites[0].title.as_deref(), Some("https://github.com/x/y (recitation, license: mit)"));
    assert_eq!(cites[0].range, Some((0, 8)));
    assert!(
        evs.contains(&StreamEvent::Finish { reason: Some("content_filter".into()), native: Some("RECITATION".into()) })
    );

    for (native, reason) in [
        ("MALFORMED_FUNCTION_CALL", "error"),
        ("MISSING_THOUGHT_SIGNATURE", "error"),
        ("NO_IMAGE", "error"),
        ("IMAGE_SAFETY", "content_filter"),
        ("SOMETHING_NEW", "error"),
    ] {
        let mut p = parser("gemini-3.8-flash", json!({}));
        let evs = run(&mut p, &[json!({"candidates": [{"finishReason": native, "finishMessage": "why"}]})]);
        assert!(
            evs.contains(&StreamEvent::Finish { reason: Some(reason.into()), native: Some(native.into()) }),
            "{native}"
        );
        assert!(
            evs.iter().any(|e| matches!(e, StreamEvent::FinishDetails(d) if d["finishMessage"] == "why")),
            "{native}"
        );
        // The failure is told in Google's words.
        let e = p.abnormal().unwrap();
        assert_eq!((e.message.as_str(), e.error_type.as_deref()), (format!("{native}: why").as_str(), Some(reason)));
    }
    // A normal finish with a message: shown as a note, no failure.
    let mut p = parser("gemini-3.8-flash", json!({}));
    let evs = run(
        &mut p,
        &[
            json!({"candidates": [{"content": {"parts": [{"text": "x"}]}, "finishReason": "MAX_TOKENS", "finishMessage": "Reached the limit."}]}),
        ],
    );
    assert!(p.abnormal().is_none());
    assert!(evs.contains(&StreamEvent::Notice("Google (MAX_TOKENS): Reached the limit.".into())));

    // An error object inside the stream.
    let mut p = parser("gemini-3.8-flash", json!({}));
    p.chunk(&json!({"candidates": [{"content": {"parts": [{"text": "Hel"}]}}]})).unwrap();
    let e = p
        .chunk(&json!({"error": {"code": 500, "message": "Internal error encountered.", "status": "INTERNAL"}}))
        .unwrap_err();
    assert_eq!((e.kind, e.retry, e.status), (ErrorKind::InBand, Retry::Backoff, Some(500)));
    assert!(e.partial_output);
    assert_eq!(e.error_type.as_deref(), Some("INTERNAL"));
}

#[test]
fn search_suggestions_from_the_rendered_chips() {
    let chip1 =
        "https://www.google.com/search?q=latest+stable+Rust+version&client=app-vertex-grounding&safesearch=active";
    let chip2 = "https://www.google.com/search?q=rust+1.100&client=app-vertex-grounding";
    let html = "<style>.container { align-items: center; }</style><div class=\"container\"><div class=\"headline\">\
        <svg class=\"logo-light\"></svg></div><div class=\"carousel\"><a class=\"chip\" \
        href=\"https://www.google.com/search?q=latest+stable+Rust+version&amp;client=app-vertex-grounding&amp;safesearch=active\">\
        latest stable Rust version</a><a href=\"https://www.google.com/about\">not a chip</a>\
        <a class='chip extra' href='https://www.google.com/search?q=rust+1.100&#38;client=app-vertex-grounding'>rust <b>1.100</b> \
        release&#39;s notes</a></div></div>";
    let text = "Rust 1.100.0 is the latest stable version.";
    let chunks = [
        json!({"candidates": [{"content": {"role": "model", "parts": [{"text": text}]}, "finishReason": "STOP",
        "groundingMetadata": {
            "webSearchQueries": ["latest stable Rust version"],
            "searchEntryPoint": {"renderedContent": html},
            "groundingChunks": [{"web": {"uri": "https://vertexaisearch.cloud.google.com/grounding-api-redirect/x", "title": "releases.rs"}}],
            "groundingSupports": [{"segment": {"endIndex": text.len(), "text": text}, "groundingChunkIndices": [0]}]}}],
        "usageMetadata": {"promptTokenCount": 22, "candidatesTokenCount": 10},
        "modelVersion": "gemini-3.8-flash", "responseId": "r"}),
    ];
    let mut p = parser("gemini-3.8-flash", json!({"tools": [{"googleSearch": {}}]}));
    let evs = run(&mut p, &chunks);
    let tool = evs.iter().find_map(|e| if let StreamEvent::ToolBefore(t) = e { Some(t) } else { None }).unwrap();
    assert_eq!(
        tool.output,
        Some(json!({"search_suggestions": [
            {"text": "latest stable Rust version", "url": chip1},
            {"text": "rust 1.100 release's notes", "url": chip2}
        ]}))
    );
    // Listed with the sources, links exactly as Google gave them.
    let cites = evs.iter().find_map(|e| if let StreamEvent::Citations(c) = e { Some(c) } else { None }).unwrap();
    let listed: Vec<(Option<&str>, Option<&str>)> =
        cites.iter().map(|c| (c.title.as_deref(), c.url.as_deref())).collect();
    assert_eq!(
        listed,
        vec![
            (Some("releases.rs"), Some("https://vertexaisearch.cloud.google.com/grounding-api-redirect/x")),
            (Some("Google Search: latest stable Rust version"), Some(chip1)),
            (Some("Google Search: rust 1.100 release's notes"), Some(chip2)),
        ]
    );
    let a = accumulate(&evs);
    assert!(matches!(&a.parts()[1], Part::Text { citations, .. } if citations.len() == 3));
    assert!(
        super::stream::search_chips("<a class=\"chip\" href=\"\">empty</a><a class=\"chipped\" href=\"x\">no</a>")
            .is_empty()
    );
}

#[test]
fn what_served_the_request_is_the_footers_and_odd_states_are_noted() {
    let chunk = |model: &str, tier: &str| {
        json!({"candidates": [{"content": {"parts": [{"text": "hi"}]}, "finishReason": "STOP"}],
               "usageMetadata": {"promptTokenCount": 3, "candidatesTokenCount": 1, "serviceTier": tier},
               "modelVersion": model, "responseId": "r"})
    };
    let notices = |evs: &[StreamEvent]| -> Vec<String> {
        evs.iter().filter_map(|e| if let StreamEvent::Notice(n) = e { Some(n.clone()) } else { None }).collect()
    };
    // Another model, another tier: the footer's (from the response envelope), not notices.
    let mut p = parser("gemini-3.7-flash", json!({}));
    let evs = run(&mut p, &[chunk("gemini-3.8-flash", "standard")]);
    assert!(notices(&evs).is_empty(), "{:?}", notices(&evs));
    assert!(matches!(&evs[0], StreamEvent::Meta { model: Some(m), .. } if m == "gemini-3.8-flash"));
    let mut p = parser("gemini-3.8-flash", json!({"service_tier": "priority"}));
    let evs = run(&mut p, &[chunk("gemini-3.8-flash", "standard")]);
    assert!(notices(&evs).is_empty());
    // Priced as served.
    assert!((usages(&evs)[0].cost_usd.unwrap() - (3.0 * 0.75 + 1.0 * 3.75) / 1e6).abs() < 1e-12);
    // A model status other than STABLE.
    let mut c = chunk("gemini-2.5-flash-image", "standard");
    c["modelStatus"] = json!({"modelStage": "LEGACY", "retirementTime": "2027-03-15T00:00:00Z", "message": "Use gemini-3.1-flash-lite-image."});
    let mut p = parser("gemini-2.5-flash-image", json!({}));
    assert_eq!(
        notices(&run(&mut p, &[c])),
        vec![
            "Google: `gemini-2.5-flash-image` is LEGACY, retiring 2027-03-15T00:00:00Z: Use gemini-3.1-flash-lite-image."
        ]
    );
}

#[test]
fn safety_ratings_are_noted_when_they_matter() {
    let notices = |evs: &[StreamEvent]| -> Vec<String> {
        evs.iter().filter_map(|e| if let StreamEvent::Notice(n) = e { Some(n.clone()) } else { None }).collect()
    };
    // The image models' ratings of 2026-11-12: NEGLIGIBLE, nothing to say.
    let mut p = parser("gemini-nano-banana-2.1", json!({}));
    let evs = run(
        &mut p,
        &[json!({"candidates": [{"content": {"parts": [{"text": "a cat"}]}, "finishReason": "STOP",
            "safetyRatings": [{"category": "HARM_CATEGORY_HATE_SPEECH", "probability": "NEGLIGIBLE"},
                              {"category": "HARM_CATEGORY_HARASSMENT", "probability": "LOW"}]}]})],
    );
    assert!(notices(&evs).is_empty());
    // MEDIUM or HIGH, or blocked, of the prompt or the answer: once each.
    let mut p = parser("gemini-3.8-flash", json!({}));
    let rated = json!({"candidates": [{"content": {"parts": [{"text": "x"}]},
        "safetyRatings": [{"category": "HARM_CATEGORY_DANGEROUS_CONTENT", "probability": "MEDIUM"}]}],
        "promptFeedback": {"safetyRatings": [{"category": "HARM_CATEGORY_HARASSMENT", "probability": "HIGH"},
                                             {"category": "HARM_CATEGORY_HATE_SPEECH", "probability": "LOW", "blocked": true}]}});
    let mut end = rated.clone();
    end["candidates"][0]["finishReason"] = json!("STOP");
    let evs = run(&mut p, &[rated, end]);
    assert_eq!(
        notices(&evs),
        vec![
            "Google's safety rating of the prompt: HARM_CATEGORY_HARASSMENT HIGH",
            "Google's safety rating of the prompt: HARM_CATEGORY_HATE_SPEECH LOW (blocked)",
            "Google's safety rating of the answer: HARM_CATEGORY_DANGEROUS_CONTENT MEDIUM",
        ]
    );
    assert!(p.abnormal().is_none(), "a rating alone is no failure");
}

#[test]
fn live_usage_adds_up_internal_calls() {
    // The shapes of a code-execution turn of the 2026-11-12 traffic: the chunk with the
    // result has no output counts and the next internal call counts from zero again.
    let chunks = [
        json!({"candidates": [{"content": {"parts": [{"executableCode": {"language": "PYTHON", "code": "print(2**100)", "id": "c1"}}]}}],
               "usageMetadata": {"promptTokenCount": 21, "candidatesTokenCount": 22, "thoughtsTokenCount": 86}}),
        json!({"candidates": [{"content": {"parts": [{"codeExecutionResult": {"outcome": "OUTCOME_OK", "output": "1267650600228229401496703205376\n", "id": "c1"}}]}}],
               "usageMetadata": {"promptTokenCount": 21, "totalTokenCount": 21}}),
        json!({"candidates": [{"content": {"parts": [{"text": "1267"}]}}],
               "usageMetadata": {"promptTokenCount": 21, "candidatesTokenCount": 4, "thoughtsTokenCount": 88}}),
        json!({"candidates": [{"content": {"parts": [{"text": "650600228229401496703205376"}]}, "finishReason": "STOP"}],
               "usageMetadata": {"promptTokenCount": 107, "candidatesTokenCount": 53, "toolUsePromptTokenCount": 159, "thoughtsTokenCount": 174}}),
    ];
    let mut p = parser("gemini-3.8-flash", json!({"tools": [{"codeExecution": {}}]}));
    let evs = run(&mut p, &chunks);
    let live: Vec<(Option<u64>, Option<u64>)> = evs
        .iter()
        .filter_map(
            |e| if let StreamEvent::LiveUsage(u) = e { Some((u.output_tokens, u.reasoning_tokens)) } else { None },
        )
        .collect();
    assert_eq!(live, vec![(Some(108), Some(86)), (Some(108), Some(86)), (Some(200), Some(174))]);
    // The final usage is Google's own total (the sum of the calls).
    let u = usages(&evs)[0];
    assert_eq!((u.input_tokens, u.output_tokens, u.reasoning_tokens), (Some(266), Some(227), Some(174)));
}

#[test]
fn files_written_by_code_execution() {
    let csv = b"a,b\n1,2\n";
    let chunks = [json!({"candidates": [{"content": {"parts": [
        {"inlineData": {"mimeType": "text/csv", "data": base64::engine::general_purpose::STANDARD.encode(csv), "displayName": "output_1.csv"}},
        {"text": "Done."}]}, "finishReason": "STOP"}]})];
    let mut p = parser("gemini-3.8-flash", json!({}));
    let evs = run(&mut p, &chunks);
    assert!(evs.contains(&StreamEvent::File {
        data: csv.to_vec(),
        mime: "text/csv".into(),
        name: Some("output_1.csv".into())
    }));
    assert!(!evs.iter().any(|e| matches!(e, StreamEvent::Notice(_))));
}

/// `toolConfig.includeServerSideToolInvocations` (the tool-combination
/// guide's shapes): the search call and its result are parts where they ran,
/// replacing the call made up from the grounding metadata; sources and
/// Search Suggestions still come from the metadata.
#[test]
fn server_side_tool_parts_are_the_tool_calls() {
    let html = "<div class=\"carousel\"><a class=\"chip\" href=\"https://www.google.com/search?q=latest+stable+Rust+version&amp;client=app-vertex-grounding\">latest stable Rust version</a></div>";
    let text = "Rust 1.100.0 is the latest stable version.";
    let chunks = [
        json!({"candidates": [{"content": {"role": "model", "parts": [{"text": "**Searching**", "thought": true}]}}],
               "usageMetadata": {"promptTokenCount": 22, "thoughtsTokenCount": 30}, "responseId": "r1", "modelVersion": "gemini-3.8-flash"}),
        json!({"candidates": [{"content": {"role": "model", "parts": [
            {"thoughtSignature": "SIG1", "toolCall": {"toolType": "GOOGLE_SEARCH_WEB", "args": {"queries": ["latest stable Rust version"]}, "id": "a7b3k9p2"}}]}}]}),
        json!({"candidates": [{"content": {"role": "model", "parts": [
            {"thoughtSignature": "SIG2", "toolResponse": {"toolType": "GOOGLE_SEARCH_WEB", "response": {"search_suggestions": html}, "id": "a7b3k9p2"}}]}}]}),
        json!({"candidates": [{"content": {"role": "model", "parts": [{"text": text}]}, "finishReason": "STOP",
            "groundingMetadata": {
                "webSearchQueries": ["latest stable Rust version"],
                "searchEntryPoint": {"renderedContent": html},
                "groundingChunks": [{"web": {"uri": "https://vertexaisearch.cloud.google.com/grounding-api-redirect/x", "title": "releases.rs"}}],
                "groundingSupports": [{"segment": {"partIndex": 3, "endIndex": text.len(), "text": text}, "groundingChunkIndices": [0]}]}}],
            "usageMetadata": {"promptTokenCount": 196, "candidatesTokenCount": 38, "thoughtsTokenCount": 408}}),
    ];
    let mut p = parser(
        "gemini-3.8-flash",
        json!({"tools": [{"googleSearch": {}}], "toolConfig": {"includeServerSideToolInvocations": true}}),
    );
    let evs = run(&mut p, &chunks);
    assert!(!evs.iter().any(|e| matches!(e, StreamEvent::ToolBefore(_))), "no made-up call: {evs:#?}");
    let tools: Vec<&ToolCall> =
        evs.iter().filter_map(|e| if let StreamEvent::Tool(t) = e { Some(t) } else { None }).collect();
    assert_eq!(tools.len(), 2);
    assert_eq!(
        (tools[0].id.as_deref(), tools[0].name.as_str(), tools[0].status.as_deref()),
        (Some("a7b3k9p2"), "google_search", Some("in_progress"))
    );
    assert_eq!(tools[0].input, Some(json!({"queries": ["latest stable Rust version"]})));
    assert_eq!(tools[1].id, tools[0].id, "the result is paired with its call");
    assert_eq!(tools[1].output, Some(json!({"search_suggestions": html})));
    // In stream order: thought, the search, the answer with its sources and suggestions.
    let a = accumulate(&evs);
    let kinds: Vec<&str> = a
        .parts()
        .iter()
        .map(|p| match p {
            Part::Reasoning(_) => "reasoning",
            Part::ToolCall(_) => "tool",
            Part::Text { .. } => "text",
            _ => "other",
        })
        .collect();
    assert_eq!(kinds, ["reasoning", "tool", "text"]);
    let Part::Text { citations, .. } = &a.parts()[2] else { panic!() };
    assert_eq!(citations.len(), 2, "{citations:?}");
    assert_eq!(citations[0].range, Some((0, text.len())), "partIndex counts the tool parts");
    assert_eq!(citations[1].title.as_deref(), Some("Google Search: latest stable Rust version"));
    // One billed query; the parts go back verbatim, signatures included.
    let u = usages(&evs)[0];
    assert!((u.cost_usd.unwrap() - ((196.0 * 0.75 + 446.0 * 3.75) / 1e6 + 0.014)).abs() < 1e-12);
    assert_eq!(p.native_payload()["parts"][1], chunks[1]["candidates"][0]["content"]["parts"][0]);
    assert_eq!(p.native_payload()["parts"][2]["thoughtSignature"], "SIG2");
}

#[test]
fn every_kind_of_tool_part() {
    let part = |key: &str, v: Value| json!({"candidates": [{"content": {"parts": [{key: v}]}}]});
    let chunks = [
        // URL context: no URL retrieved, a failure.
        part(
            "toolCall",
            json!({"toolType": "URL_CONTEXT", "args": {"urls": ["https://example.com/paywalled"]}, "id": "u1"}),
        ),
        part(
            "toolResponse",
            json!({"toolType": "URL_CONTEXT", "id": "u1", "response": {"url_metadata": [
            {"retrieved_url": "https://example.com/paywalled", "url_retrieval_status": "URL_RETRIEVAL_STATUS_PAYWALL"}]}}),
        ),
        // Maps, without ids: paired by type.
        part("toolCall", json!({"toolType": "GOOGLE_MAPS", "args": {"queries": ["cafes near me"]}})),
        part(
            "toolResponse",
            json!({"toolType": "GOOGLE_MAPS", "response": {"places": [{"place_id": "ChIJ"}], "google_maps_widget_context_token": "w"}}),
        ),
        // Image search and media processing (snake_case spelling too).
        part(
            "tool_call",
            json!({"tool_type": "GOOGLE_SEARCH_IMAGE", "args": {"queries": ["timareta butterfly"]}, "id": "i1"}),
        ),
        part("toolCall", json!({"toolType": "MEDIA_PROCESSING", "id": "m1"})),
        json!({"candidates": [{"content": {"parts": [{"text": "ok"}]}, "finishReason": "STOP",
            "urlContextMetadata": {"urlMetadata": [{"retrievedUrl": "https://example.com/paywalled", "urlRetrievalStatus": "URL_RETRIEVAL_STATUS_PAYWALL"}]},
            "groundingMetadata": {"webSearchQueries": ["cafes near me"], "imageSearchQueries": ["timareta butterfly"]}}],
            "usageMetadata": {"promptTokenCount": 10, "candidatesTokenCount": 2}}),
    ];
    let mut p = parser("gemini-3.8-flash", json!({"tools": [{"urlContext": {}}, {"googleMaps": {}}]}));
    let evs = run(&mut p, &chunks);
    assert!(!evs.iter().any(|e| matches!(e, StreamEvent::ToolBefore(_))));
    let a = accumulate(&evs);
    let calls: Vec<&ToolCall> =
        a.parts().iter().filter_map(|p| if let Part::ToolCall(t) = p { Some(t) } else { None }).collect();
    let names: Vec<(&str, Option<&str>)> = calls.iter().map(|c| (c.name.as_str(), c.status.as_deref())).collect();
    assert_eq!(
        names,
        vec![
            ("url_context", Some("failed")),
            ("google_maps", Some("completed")),
            ("google_search_image", Some("in_progress")),
            ("media_processing", Some("in_progress")),
        ]
    );
    assert_eq!(calls[1].output.as_ref().unwrap()["places"][0]["place_id"], "ChIJ");
    // One web (Maps) query and one image query.
    let u = usages(&evs)[0];
    assert!((u.cost_usd.unwrap() - ((10.0 * 0.75 + 2.0 * 3.75) / 1e6 + 2.0 * 0.014)).abs() < 1e-12);
}

#[test]
fn live_counts_that_go_on_after_a_tool_response_are_not_added_twice() {
    let chunks = [
        json!({"candidates": [{"content": {"parts": [{"toolCall": {"toolType": "GOOGLE_SEARCH_WEB", "args": {"queries": ["q"]}, "id": "t"}}]}}],
               "usageMetadata": {"promptTokenCount": 20, "thoughtsTokenCount": 30}}),
        json!({"candidates": [{"content": {"parts": [{"toolResponse": {"toolType": "GOOGLE_SEARCH_WEB", "response": {}, "id": "t"}}]}}],
               "usageMetadata": {"promptTokenCount": 20, "thoughtsTokenCount": 30}}),
        json!({"candidates": [{"content": {"parts": [{"text": "x"}]}}],
               "usageMetadata": {"promptTokenCount": 20, "candidatesTokenCount": 1, "thoughtsTokenCount": 50}}),
    ];
    let mut p = parser("gemini-3.8-flash", json!({"tools": [{"googleSearch": {}}]}));
    let live: Vec<Option<u64>> = run(&mut p, &chunks)
        .iter()
        .filter_map(|e| if let StreamEvent::LiveUsage(u) = e { Some(u.reasoning_tokens) } else { None })
        .collect();
    assert_eq!(live, vec![Some(30), Some(30), Some(50)], "cumulative counts stay as they are");
}

#[test]
fn search_suggestions_from_the_tool_response_when_the_metadata_has_none() {
    let html = "<a class=\"chip\" href=\"https://www.google.com/search?q=euro+2024\">euro 2024</a>";
    let chunks = [
        json!({"candidates": [{"content": {"parts": [{"toolCall": {"toolType": "GOOGLE_SEARCH_WEB", "args": {"queries": ["euro 2024"]}, "id": "s1"}}]}}]}),
        json!({"candidates": [{"content": {"parts": [{"toolResponse": {"toolType": "GOOGLE_SEARCH_WEB", "response": {"search_suggestions": html}, "id": "s1"}}]}}]}),
        json!({"candidates": [{"content": {"parts": [{"text": "Spain."}]}, "finishReason": "STOP"}]}),
    ];
    let mut p = parser("gemini-4-carbon", json!({"tools": [{"googleSearch": {}}]}));
    let evs = run(&mut p, &chunks);
    let cites = evs.iter().find_map(|e| if let StreamEvent::Citations(c) = e { Some(c) } else { None }).unwrap();
    assert_eq!(cites[0].url.as_deref(), Some("https://www.google.com/search?q=euro+2024"));
    assert_eq!(cites[0].extra.as_ref().unwrap()["searchSuggestion"]["text"], "euro 2024");
}

#[tokio::test]
async fn tool_parts_in_the_history_keep_the_flag_on() {
    let parts = json!([
        {"thoughtSignature": "S1", "toolCall": {"toolType": "GOOGLE_SEARCH_WEB", "args": {"queries": ["q"]}, "id": "t1"}},
        {"thoughtSignature": "S2", "toolResponse": {"toolType": "GOOGLE_SEARCH_WEB", "response": {"search_suggestions": "<a></a>"}, "id": "t1"}},
        {"text": "Answer.", "thoughtSignature": "S3"}
    ]);
    let history =
        vec![Turn::user_text("q?"), native_turn("gemini-3.8-flash", parts.clone()), Turn::user_text("thanks")];
    // No tools now: the parts go back as they were, with the flag their replay needs.
    let p = prep("gemini-3.8-flash", &[], &history).await.unwrap();
    assert_eq!(p.body["contents"][1]["parts"], parts);
    assert_eq!(p.body["toolConfig"], json!({"includeServerSideToolInvocations": true}));
    assert!(p.body.get("tools").is_none());
    // A model without tool traces (Gemini 2.5): no flag.
    let p = prep("gemini-2.5-flash", &[], &history).await.unwrap();
    assert!(p.body.get("toolConfig").is_none());
    assert_eq!(p.body["contents"][1]["parts"], parts);
}

#[test]
fn web_and_image_queries_are_billed_apart() {
    let chunk = json!({"candidates": [{"content": {"parts": [{"text": "x"}]}, "finishReason": "STOP",
        "groundingMetadata": {"webSearchQueries": ["timareta butterfly", "timareta butterfly"], "imageSearchQueries": ["timareta butterfly", ""],
            "groundingChunks": [{"image": {"sourceUri": "https://example.org/butterflies", "imageUri": "https://example.org/b.jpg",
                                            "title": "Butterflies of Peru", "domain": "example.org"}}]}}],
        "usageMetadata": {"promptTokenCount": 10, "candidatesTokenCount": 1120,
                          "candidatesTokensDetails": [{"modality": "IMAGE", "tokenCount": 1120}]}});
    let mut p = parser(
        "gemini-nano-banana-2.1",
        json!({"tools": [{"googleSearch": {"searchTypes": {"webSearch": {}, "imageSearch": {}}}}]}),
    );
    let evs = run(&mut p, &[chunk]);
    let tool = evs.iter().find_map(|e| if let StreamEvent::ToolBefore(t) = e { Some(t) } else { None }).unwrap();
    assert_eq!(
        tool.input,
        Some(json!({"queries": ["timareta butterfly"], "image_queries": ["timareta butterfly", ""]}))
    );
    // The image's source is the page that has it (Google's attribution rule).
    let cites = evs.iter().find_map(|e| if let StreamEvent::Citations(c) = e { Some(c) } else { None }).unwrap();
    assert_eq!(
        (cites[0].url.as_deref(), cites[0].title.as_deref()),
        (Some("https://example.org/butterflies"), Some("Butterflies of Peru"))
    );
    // One web and one image query at $14 per 1,000 each; 1,120 image tokens at $30/M.
    let u = usages(&evs)[0];
    let expected = (10.0 * 1.50 + 1120.0 * 30.0) / 1e6 + 2.0 * 0.014;
    assert!((u.cost_usd.unwrap() - expected).abs() < 1e-12, "{:?}", u.cost_usd);
}

#[tokio::test]
async fn media_other_models_generated_goes_in_the_model_turn() {
    let mut b = Blobs::none();
    let png = b.add(b"\x89PNG-made-by-another-model", "image/png");
    let draft = b.add(b"\x89PNG-draft", "image/png");
    let wav = b.add(b"RIFF....WAVE", "audio/wav");
    let csv = b.add(b"a,b\n1,2\n", "text/csv");
    let answer = Turn {
        role: Role::Assistant,
        origin: Origin::Live,
        parts: vec![
            Part::Image { blob: draft, filename: None, generated: true, dims: None, thought: true },
            Part::text("Here is the cat."),
            Part::Image { blob: png.clone(), filename: None, generated: true, dims: Some((64, 64)), thought: false },
            Part::text(" And the data:"),
            Part::File { blob: csv, filename: Some("data.csv".into()), generated: true },
            Part::File { blob: wav, filename: Some("meow.wav".into()), generated: true },
        ],
        native: None,
    };
    let history = vec![Turn::user_text("a cat"), answer.clone(), Turn::user_text("make it blue")];
    let b64 = |x: &[u8]| base64::engine::general_purpose::STANDARD.encode(x);
    // A text model takes everything: in order, at the highest resolution; no signature is made up.
    let p = prep_full(&google(), "gemini-3.8-flash", &[], &history, &[], &b, None).await.unwrap();
    assert_eq!(
        p.body["contents"][1],
        json!({"role": "model", "parts": [
            {"text": "Here is the cat."},
            {"inlineData": {"mimeType": "image/png", "data": b64(b"\x89PNG-made-by-another-model")}, "mediaResolution": {"level": "MEDIA_RESOLUTION_ULTRA_HIGH"}},
            {"text": " And the data:"},
            {"inlineData": {"mimeType": "text/csv", "data": b64(b"a,b\n1,2\n"), "displayName": "data.csv"}},
            {"inlineData": {"mimeType": "audio/wav", "data": b64(b"RIFF....WAVE"), "displayName": "meow.wav"}}
        ]})
    );
    assert!(p.warnings.is_empty(), "{:?}", p.warnings);
    // An image model edits the image (no per-part resolution there); it takes no audio.
    let p = prep_full(&google(), "gemini-nano-banana-2.1", &[], &history, &[], &b, None).await.unwrap();
    let parts = p.body["contents"][1]["parts"].as_array().unwrap().clone();
    assert_eq!(
        parts[1],
        json!({"inlineData": {"mimeType": "image/png", "data": b64(b"\x89PNG-made-by-another-model")}})
    );
    assert_eq!(parts.len(), 4, "{parts:?}");
    assert_eq!(
        p.warnings,
        vec!["1 file generated in earlier turns was not sent: `gemini-nano-banana-2.1` does not take audio input"]
    );
    // An answer that is only an image is a model turn with the image.
    let only = Turn {
        role: Role::Assistant,
        origin: Origin::Live,
        parts: vec![Part::Image { blob: png, filename: None, generated: true, dims: None, thought: false }],
        native: None,
    };
    let p = prep_full(
        &google(),
        "gemini-3-pro-image",
        &[],
        &[Turn::user_text("cat"), only, Turn::user_text("bluer")],
        &[],
        &b,
        None,
    )
    .await
    .unwrap();
    assert_eq!(p.body["contents"][1]["parts"].as_array().unwrap().len(), 1);
    assert_eq!(p.body["contents"][1]["role"], "model");
}

// ------------------------------------------------------------------ errors

fn http(status: u16, retry_after: Option<u64>, body: &str) -> LlmError {
    crate::chatstream::http_error(status, retry_after, body, error_from_json)
}

#[test]
fn errors_are_classified() {
    // The documented example.
    let body = r#"{"error":{"code":400,"message":"API key not valid. Please pass a valid API key.","status":"INVALID_ARGUMENT",
      "details":[{"@type":"type.googleapis.com/google.rpc.ErrorInfo","reason":"API_KEY_INVALID","domain":"googleapis.com","metadata":{"service":"generativelanguage.googleapis.com"}},
                 {"@type":"type.googleapis.com/google.rpc.LocalizedMessage","locale":"en-US","message":"API key not valid. Please pass a valid API key."}]}}"#;
    let e = http(400, None, body);
    assert_eq!((e.retry, e.status, e.error_type.as_deref()), (Retry::Never, Some(400), Some("API_KEY_INVALID")));
    assert_eq!(e.message, "API key not valid. Please pass a valid API key.");
    assert_eq!(e.details, Some(serde_json::from_str::<Value>(body).unwrap()), "kept verbatim");
    // Recorded 2026-11-12 (no key sent).
    let e = http(
        403,
        None,
        r#"{"error": {"code": 403, "message": "Method doesn't allow unregistered callers (callers without established identity). Please use API Key or other form of API consumer identity to call this API.", "status": "PERMISSION_DENIED"}}"#,
    );
    assert_eq!((e.retry, e.error_type.as_deref()), (Retry::Never, Some("PERMISSION_DENIED")));
    let e = http(
        400,
        None,
        r#"{"error": {"code": 400, "message": "Invalid JSON payload received. Unknown name \"bogusField\": Cannot find field.", "status": "INVALID_ARGUMENT", "details": [{"@type": "type.googleapis.com/google.rpc.BadRequest", "fieldViolations": [{"description": "Invalid JSON payload received. Unknown name \"bogusField\": Cannot find field."}]}]}}"#,
    );
    assert_eq!((e.retry, e.error_type.as_deref()), (Retry::Never, Some("INVALID_ARGUMENT")));

    let rate = |details: &str| {
        format!(
            r#"{{"error": {{"code": 429, "message": "You exceeded your current quota.", "status": "RESOURCE_EXHAUSTED", "details": [{details}]}}}}"#
        )
    };
    let e = http(
        429,
        None,
        &rate(
            r#"{"@type": "type.googleapis.com/google.rpc.QuotaFailure", "violations": [{"quotaMetric": "generativelanguage.googleapis.com/generate_content_paid_tier_input_token_count", "quotaId": "GenerateContentPaidTierInputTokensPerModelPerMinute"}]},
        {"@type": "type.googleapis.com/google.rpc.RetryInfo", "retryDelay": "37.2s"}"#,
        ),
    );
    assert_eq!(e.retry, Retry::After(38));
    let e = http(
        429,
        Some(5),
        &rate(
            r#"{"@type": "type.googleapis.com/google.rpc.QuotaFailure", "violations": [{"quotaId": "GenerateRequestsPerDayPerProjectPerModel"}]}"#,
        ),
    );
    assert_eq!(e.retry, Retry::Never, "a daily quota does not recover within the retries");
    let e = http(
        429,
        Some(5),
        r#"{"error": {"code": 429, "message": "Resource exhausted.", "status": "RESOURCE_EXHAUSTED"}}"#,
    );
    assert_eq!(e.retry, Retry::After(5));
    let e = http(
        429,
        None,
        r#"{"error": {"code": 429, "message": "Resource exhausted.", "status": "RESOURCE_EXHAUSTED"}}"#,
    );
    assert_eq!(e.retry, Retry::After(20));
    let e = http(
        402,
        None,
        r#"{"error": {"code": 402, "message": "Your prepayment credits are depleted.", "status": "RESOURCE_EXHAUSTED"}}"#,
    );
    assert_eq!(e.retry, Retry::Never);
    for (status, grpc) in [(500, "INTERNAL"), (503, "UNAVAILABLE"), (504, "DEADLINE_EXCEEDED")] {
        let e =
            http(status, None, &format!(r#"{{"error": {{"code": {status}, "message": "x", "status": "{grpc}"}}}}"#));
        assert_eq!(e.retry, Retry::Backoff, "{status}");
    }
    let e = http(
        404,
        None,
        r#"{"error": {"code": 404, "message": "models/gemini-9 is not found for API version v1beta", "status": "NOT_FOUND"}}"#,
    );
    assert_eq!(e.retry, Retry::Never);
    let e = http(502, None, "<html>bad gateway</html>");
    assert_eq!((e.retry, e.details), (Retry::Backoff, Some(Value::String("<html>bad gateway</html>".into()))));

    let e = http(
        400,
        None,
        r#"{"error": {"code": 400, "message": "The input token count (1200000) exceeds the maximum number of tokens allowed (1048576).", "status": "INVALID_ARGUMENT"}}"#,
    );
    // Too long an input: not fixable with a smaller output budget, so not an overflow.
    assert_eq!(e.error_type.as_deref(), Some("input_too_long"));
    assert!(!e.is_context_overflow());
    assert_eq!(e.retry, Retry::Never);
    let e = http(
        400,
        None,
        r#"{"error": {"code": 400, "message": "Continuation token has already used output tokens (393216); max_output_tokens must be greater.", "status": "INVALID_ARGUMENT"}}"#,
    );
    assert_eq!((e.error_type.as_deref(), e.retry), (Some(BUDGET_EXHAUSTED), Retry::Never));

    assert_eq!(duration_secs("52s"), Some(52));
    assert_eq!(duration_secs("0.5s"), Some(1));
    assert_eq!(duration_secs("soon"), None);
}

// ------------------------------------------------------------------ pricing

#[test]
fn prices_by_tier_date_and_modality() {
    let m = |u: Value, tier: Option<Tier>, now: i64| {
        pricing::estimate("gemini-4-carbon", &u, tier, now, &GroundingUse::default()).unwrap()
    };
    let million_in = json!({"promptTokenCount": 1_000_000});
    assert!((m(million_in.clone(), None, NOW) - 2.0).abs() < 1e-9);
    assert!((m(million_in.clone(), None, pricing::DEC_1_2026) - 4.0).abs() < 1e-9, "introductory price ends");
    assert!((m(million_in.clone(), Some(Tier::Flex), NOW) - 1.0).abs() < 1e-9);
    assert!((m(million_in.clone(), Some(Tier::Priority), NOW) - 3.6).abs() < 1e-9);
    // The tier actually served wins (priority traffic may be downgraded).
    let served = json!({"promptTokenCount": 1_000_000, "serviceTier": "standard"});
    assert!((m(served, Some(Tier::Priority), NOW) - 2.0).abs() < 1e-9);
    // Cache reads.
    let cached = json!({"promptTokenCount": 1_000_000, "cachedContentTokenCount": 1_000_000});
    assert!((m(cached, None, NOW) - 0.10).abs() < 1e-9);

    let pro = |prompt: u64| {
        pricing::estimate(
            "gemini-3.1-pro-preview",
            &json!({"promptTokenCount": prompt, "candidatesTokenCount": 1000}),
            None,
            NOW,
            &GroundingUse::default(),
        )
        .unwrap()
    };
    assert!((pro(200_000) - (200_000.0 * 2.0 + 1000.0 * 12.0) / 1e6).abs() < 1e-12);
    assert!((pro(200_001) - (200_001.0 * 4.0 + 1000.0 * 18.0) / 1e6).abs() < 1e-12, ">200K tier");

    let audio = json!({"promptTokenCount": 3000, "promptTokensDetails": [{"modality": "TEXT", "tokenCount": 2000}, {"modality": "AUDIO", "tokenCount": 1000}]});
    let c = pricing::estimate("gemini-3.1-flash-lite", &audio, None, NOW, &GroundingUse::default()).unwrap();
    assert!((c - (2000.0 * 0.25 + 1000.0 * 0.50) / 1e6).abs() < 1e-12);

    let flash = |g: GroundingUse, model: &str| pricing::estimate(model, &json!({}), None, NOW, &g).unwrap();
    assert!((flash(GroundingUse { queries: 3, ..Default::default() }, "gemini-3.8-flash") - 0.042).abs() < 1e-12);
    let g25 = GroundingUse { queries: 4, web_results: true, maps_results: true };
    assert!((flash(g25, "gemini-2.5-flash") - (0.035 + 0.025)).abs() < 1e-12, "2.5: per grounded prompt");
    // 2.5 Pro is billed as 3.8 Flash once Google routes it there.
    let after = pricing::estimate(
        "gemini-2.5-pro",
        &json!({"promptTokenCount": 1_000_000}),
        None,
        pricing::DEC_1_2026,
        &GroundingUse::default(),
    )
    .unwrap();
    assert!((after - 0.75).abs() < 1e-9);
    assert!(pricing::estimate("gemini-9", &json!({}), None, NOW, &GroundingUse::default()).is_none());
    assert!(pricing::price("gemini-nano-banana-2.1", Tier::Flex, NOW).is_none(), "image models have no flex tier");

    // Gemini 4 Hydrogen: flat, no introductory price.
    let m = |tier, now| {
        pricing::estimate("gemini-4-hydrogen", &million_in, Some(tier), now, &GroundingUse::default()).unwrap()
    };
    assert!((m(Tier::Standard, NOW) - 1.50).abs() < 1e-9);
    assert!((m(Tier::Standard, pricing::JAN_1_2027) - 1.50).abs() < 1e-9);
    assert!((m(Tier::Flex, NOW) - 0.75).abs() < 1e-9);
    assert!((m(Tier::Priority, NOW) - 2.70).abs() < 1e-9);

    // Image output at the price list's tokens per image, also when `candidatesTokenCount`
    // says less (as logged on 2026-11-12: 305 against IMAGE 1,120).
    let u = json!({"promptTokenCount": 11, "candidatesTokenCount": 305, "thoughtsTokenCount": 287,
                   "totalTokenCount": 603, "candidatesTokensDetails": [{"modality": "IMAGE", "tokenCount": 1120}]});
    let c = pricing::estimate("gemini-3.1-flash-lite-image", &u, None, NOW, &GroundingUse::default()).unwrap();
    assert!((c - (11.0 * 0.25 + 287.0 * 1.50 + 1120.0 * 30.0) / 1e6).abs() < 1e-12, "{c}");
    // Gemini 2.5 Flash Image: $0.039 per 1,290-token image.
    let u = json!({"promptTokenCount": 10, "candidatesTokenCount": 1290,
                   "candidatesTokensDetails": [{"modality": "IMAGE", "tokenCount": 1290}]});
    let c = pricing::estimate("gemini-2.5-flash-image", &u, None, NOW, &GroundingUse::default()).unwrap();
    assert!((c - (10.0 * 0.30 / 1e6 + 0.0387)).abs() < 1e-9, "{c}");
}

// ------------------------------------------------------------------ HTTP path (local fake server)

/// One canned response: status and body pieces; `complete: false` drops the
/// connection without ending the chunked body (a broken stream).
struct Reply {
    status: u16,
    pieces: Vec<String>,
    complete: bool,
}

/// Serve `replies` to successive connections; returns the base URL and the
/// captured (request head, body) of each.
async fn serve(replies: Vec<Reply>) -> (String, Arc<Mutex<Vec<(String, Value)>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    tokio::spawn(async move {
        for reply in replies {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = Vec::new();
            let mut tmp = [0u8; 65536];
            let (head, body) = loop {
                let n = sock.read(&mut tmp).await.unwrap();
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
                        let n = sock.read(&mut tmp).await.unwrap();
                        buf.extend_from_slice(&tmp[..n]);
                    }
                    break (head, serde_json::from_slice(&buf[pos + 4..pos + 4 + len]).unwrap_or(Value::Null));
                }
            };
            log.lock().unwrap().push((head, body));
            let ctype = if reply.status == 200 { "text/event-stream" } else { "application/json" };
            let head = format!(
                "HTTP/1.1 {} X\r\ncontent-type: {ctype}\r\nconnection: close\r\ntransfer-encoding: chunked\r\n\r\n",
                reply.status
            );
            sock.write_all(head.as_bytes()).await.unwrap();
            for p in &reply.pieces {
                sock.write_all(format!("{:x}\r\n{p}\r\n", p.len()).as_bytes()).await.unwrap();
                sock.flush().await.unwrap();
            }
            if reply.complete {
                let _ = sock.write_all(b"0\r\n\r\n").await;
            }
            let _ = sock.shutdown().await;
        }
    });
    (format!("http://{addr}/v1beta"), seen)
}

fn sse(v: Value) -> String {
    format!("data: {v}\r\n\r\n")
}

#[tokio::test]
async fn dropped_stream_resumes_from_its_checkpoint() {
    let text =
        |t: &str| json!({"candidates": [{"content": {"role": "model", "parts": [{"text": t}]}}], "responseId": "h1"});
    let mut checkpoint = text("Part two. ");
    checkpoint["candidates"][0]["continuationToken"] = json!("CK1");
    checkpoint["usageMetadata"] = json!({"promptTokenCount": 10, "candidatesTokenCount": 40000});
    let mut end = text("Part three.");
    end["candidates"][0]["finishReason"] = json!("STOP");
    end["usageMetadata"] =
        json!({"promptTokenCount": 40010, "cachedContentTokenCount": 40000, "candidatesTokenCount": 3});
    let (base, seen) = serve(vec![
        Reply {
            status: 200,
            pieces: vec![sse(text("Part one. ")), sse(checkpoint), sse(text("lost tail"))],
            complete: false,
        },
        Reply { status: 200, pieces: vec![sse(end)], complete: true },
    ])
    .await;
    let g = Google::new(cfg(&base)).unwrap();
    let p =
        prep_full(&g, "gemini-4-carbon", &[], &[Turn::user_text("write")], &[], &Blobs::none(), None).await.unwrap();

    let mut evs = Vec::new();
    let mut sink = |e: StreamEvent| evs.push(e);
    let (r, outcome) = g.stream(&p, &mut sink, &NullObserver, &CancellationToken::new()).await;
    let e = r.unwrap_err();
    assert!(e.is_retryable() && e.partial_output, "{e}");
    let item = evs
        .iter()
        .find_map(|e| if let StreamEvent::Checkpoint(i) = e { Some(i.clone()) } else { None })
        .expect("checkpoint");
    assert_eq!(item["parts"], json!([{"text": "Part one. "}, {"text": "Part two. "}]));
    assert_eq!(outcome.continuation.as_ref(), Some(&item));
    assert_eq!(outcome.upstream_id.as_deref(), Some("h1"));
    let (head, body) = seen.lock().unwrap()[0].clone();
    assert!(head.starts_with("POST /v1beta/models/gemini-4-carbon:streamGenerateContent?alt=sse "), "{head}");
    assert!(head.to_ascii_lowercase().contains("x-goog-api-key: test-key"), "{head}");
    assert_eq!(body, p.body);

    // The bot resumes with the same request from the checkpoint; the committed parts come back.
    let hop = p.with_continuation(&item);
    let mut evs = Vec::new();
    let mut sink = |e: StreamEvent| evs.push(e);
    let (r, outcome) = g.stream(&hop, &mut sink, &NullObserver, &CancellationToken::new()).await;
    let native = r.unwrap();
    assert!(outcome.continuation.is_none());
    let mut expected = p.body.clone();
    expected["continuationToken"] = json!("CK1");
    assert_eq!(seen.lock().unwrap()[1].1, expected, "identical request plus the token");
    assert_eq!(native["parts"], json!([{"text": "Part one. "}, {"text": "Part two. "}, {"text": "Part three."}]));
    assert_eq!(
        evs.iter().filter(|e| matches!(e, StreamEvent::Text(_))).count(),
        1,
        "carried parts are not shown again"
    );
    let merged = crate::merge_natives(DIALECT, std::slice::from_ref(&native), "Part one. Part two. Part three.");
    assert_eq!(merged["parts"], native["parts"]);
    let u = usages(&evs)[0];
    assert_eq!((u.input_tokens, u.cached_input_tokens), (Some(40010), Some(40000)));
}

#[tokio::test]
async fn http_errors_and_unfinished_streams() {
    let (base, _) = serve(vec![
        Reply {
            status: 429,
            pieces: vec![r#"{"error": {"code": 429, "message": "Resource exhausted. Please try again later.", "status": "RESOURCE_EXHAUSTED"}}"#.into()],
            complete: true,
        },
        Reply { status: 200, pieces: vec![sse(json!({"candidates": [{"content": {"parts": [{"text": "Hi"}]}}]}))], complete: true },
    ])
    .await;
    let g = Google::new(cfg(&base)).unwrap();
    let p = prep_full(&g, "gemini-3.8-flash", &[], &hi(), &[], &Blobs::none(), None).await.unwrap();
    let mut sink = |_e: StreamEvent| {};
    let (r, _) = g.stream(&p, &mut sink, &NullObserver, &CancellationToken::new()).await;
    let e = r.unwrap_err();
    assert_eq!((e.kind, e.retry, e.status), (ErrorKind::Http, Retry::After(20), Some(429)));
    assert_eq!(e.details.unwrap()["error"]["status"], "RESOURCE_EXHAUSTED");
    // A stream that ends without a finishReason: retryable network error.
    let (r, _) = g.stream(&p, &mut sink, &NullObserver, &CancellationToken::new()).await;
    let e = r.unwrap_err();
    assert_eq!(e.kind, ErrorKind::Network);
    assert!(e.message.contains("without a finish reason"), "{e}");
}

#[tokio::test]
async fn a_stopped_answer_fails_in_googles_words_after_billing() {
    let message = "Unable to show the generated image. The image was filtered out because it violated Google's \
                   Generative AI Prohibited Use policy. You will not be charged for this request.";
    let end = json!({"candidates": [{"content": {"role": "model", "parts": [{"text": "", "thoughtSignature": "S"}]},
                      "finishReason": "IMAGE_RECITATION", "finishMessage": message, "index": 0}],
                     "usageMetadata": {"promptTokenCount": 11, "totalTokenCount": 11},
                     "modelVersion": "gemini-3.1-flash-lite-image", "responseId": "img1"});
    let (base, _) = serve(vec![Reply { status: 200, pieces: vec![sse(end)], complete: true }]).await;
    let g = Google::new(cfg(&base)).unwrap();
    let p = prep_full(&g, "gemini-3.1-flash-lite-image", &[], &hi(), &[], &Blobs::none(), None).await.unwrap();
    let mut evs = Vec::new();
    let mut sink = |e: StreamEvent| evs.push(e);
    let (r, outcome) = g.stream(&p, &mut sink, &NullObserver, &CancellationToken::new()).await;
    let e = r.unwrap_err();
    assert_eq!(e.message, format!("IMAGE_RECITATION: {message}"));
    assert_eq!((e.kind, e.retry, e.error_type.as_deref()), (ErrorKind::InBand, Retry::Never, Some("content_filter")));
    assert_eq!(e.details.as_ref().unwrap()["finishMessage"], message);
    assert_eq!(outcome.upstream_id.as_deref(), Some("img1"));
    assert_eq!(usages(&evs).len(), 1, "the usage is billed before the failure");
}
