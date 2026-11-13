//! Gemini Omni Flash: video generation and editing on the **Interactions
//! API** (`POST {base}/interactions`, `GET {base}/interactions/{id}`), the
//! Gemini API's video model since Veo was shut down. A bare Omni id is this
//! API, with no chat model (the owner's rule); the Gemini chat models stay on
//! generateContent ([`super`]).
//!
//! Sources (fetched 2026-11-13): aistudio.google.com/docs `omni` (the Omni
//! guide), `video`, `models/gemini-omni-flash`, `models/veo-3.1-generate-preview`,
//! `deprecations`, `pricing`, `changelog`, `interactions-overview`,
//! `background-execution`, `streaming`, `api-errors`, `safety-settings`,
//! `files`, `media-resolution`, `interactions-breaking-changes-may-2026`, the
//! Interactions API reference (create, get; the Interaction, Step, Content,
//! ResponseFormat and SSE event resources) and the Files API reference; and
//! unauthenticated probes (2026-11-13: Google's front end answers a request
//! without a valid key with `[{"error": {"code": 403, "status":
//! "PERMISSION_DENIED", …}}]`, an array).
//!
//! - **Models** ([`MODELS`]): `gemini-omni-1.2-flash` (GA 2026-11-05: 3–15 s
//!   videos with native audio, 360p / 720p / 1080p / 4K at 24 fps, up to 6
//!   reference images, conversational editing and extension). Deprecated
//!   and shut-down ids (`gemini-omni-1.1-flash`, which Google routes to 1.2
//!   from 2026-12-03; the preview; every Veo model) are not in the table.
//! - **The job**: created with `background: true` (it runs on Google's side,
//!   stored as background execution requires; `store` is left at its
//!   default, true), the interaction id checkpointed at once, then polled
//!   (`GET /interactions/{id}` every 5 s; every 20 s when the request carries
//!   over 1 MB of inline media, which every `GET` returns with the stored
//!   `user_input` steps) with its `status` (`in_progress`; no percentage) on
//!   a `video_generation` tool call. A restart or a dropped
//!   poll polls the same interaction again ([`continuation_request`]), never a
//!   second job. `failed` (with its `errors`), `cancelled` and the statuses a
//!   video request cannot continue (`requires_action`, `requires_continuation`)
//!   clear the checkpoint before the error (polling cannot change them); the
//!   error is classified by its code ([`error_from_json`]: safety blocks and
//!   invalid requests are never retried, overload and a failure without a
//!   reason are retried with a new job).
//! - **Output**: the `model_output` step's `video` content, inline base64
//!   (`data`; the guide: "calling GET /v1beta/interactions/{id} returns the
//!   video as inline base64 data … even if the interaction was originally
//!   created with delivery: uri") or a Files API URI (polled until `ACTIVE`,
//!   then downloaded through the configured base URL with the key; any other
//!   URI is fetched without credentials), as the answer's video file; thought
//!   summaries as reasoning, text as text, in the steps' order. A completed
//!   interaction with text but no video is an answer (with a notice), one with
//!   neither an error. `incomplete` is a `length` stop.
//! - **Input**: the message's text and media in a content list (the text
//!   alone as a string, as in the guide): videos first (the answer's, then
//!   attached ones; edited or extended by the model, or references), then
//!   images (a generated image of the answer replied to first, then the
//!   message's; the model decides from the prompt whether they are frames or
//!   references), then the text; at most 6 images and 3 videos on 1.2. A reply
//!   to this model's own answer continues that interaction
//!   (`previous_interaction_id`: "edit generated videos … without re-uploading
//!   the previous video"; extending generated videos works in every region and
//!   may add dialogue), so its video is not sent again. Image types: PNG, JPEG,
//!   WebP, HEIC, HEIF, GIF, BMP, TIFF; video types: MP4, MPEG, MPG, MOV
//!   (`video/quicktime` is sent as `video/mov`), AVI, FLV, WebM, WMV, 3GPP.
//!   Not taken (errors): system prompts ("System instructions … are not
//!   supported"), audio ("Uploading audio inputs … is unsupported"), YouTube
//!   links, documents. Earlier turns of another model are not sent (noted).
//! - **Options**: `size=1K` `2K` `4K` → `response_format.resolution` 720p /
//!   1080p / 4k (360p has no size), `ar=16:9|9:16` → `aspect_ratio`, `sec=`
//!   → `duration` (`"10s"`; whole seconds, 3–15 on 1.2, 3–10 on 1.1; Google
//!   rejects more than 10 for first-and-last-frame interpolation and per-turn
//!   extension). Nothing else applies: no count, quality, background, tools,
//!   thinking level or service tier (Standard only).
//! - **Defaults** (D79): the API's default resolution, 720p (not sent; the
//!   reply notes the highest, 4k, and `+size=4K`; 1080p and 4K are upscaled),
//!   `delivery: "uri"` above 720p
//!   (the guide's best practice for videos over 4 MB), the model's own length
//!   (10 s) and aspect ratio (16:9), `labels.safety_identifier` = the user
//!   hash (as on generateContent). Not sent: safety settings (the adjustable
//!   filters are off by default on Gemini 2.5 and later; Omni's built-in
//!   filters cannot be changed), `task` (the guide: try prompting first, the
//!   model infers the mode), thinking settings (not documented for Omni).
//!   Every video carries Google's invisible SynthID watermark (no option).
//! - **Cost**: Google reports tokens, not cost; estimated from the price list
//!   (2026-11-13, Standard, USD per 1M tokens): input $1.50 (text, image,
//!   video), text and thinking output $9.00, video output $12.00 on 1.2 and
//!   $17.50 on 1.1 ("5,792 tokens per second of 720p video", 1,931 for 360p,
//!   8,688 for 1080p, 17,376 for 4K: 4K costs about $0.21 per second),
//!   by the model that served the request. Without usage, the video's length
//!   (asked, else 10 s) at the resolution's token rate.

use std::time::Duration;

use omni_core::content::{ReasoningKind, Role, ToolCall, Turn};
use omni_core::usage::CostSource;
use omni_core::{BlobRef, Usage};
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use super::NAME;
use crate::error::{ErrorKind, LlmError, Retry, retry_for_status};
use crate::event::StreamEvent;
use crate::openai::media::{self, MediaInput};
use crate::openrouter::options::{Applied, overlay};
use crate::provider::{self, DirectConfig};
use crate::request::{self, Generation, Raise, Requested, RouteCaps, Tool};
use crate::{BlobLoader, CallInput, CallOutcome, EventSink, Observer, Prepared};

pub const DIALECT: &str = "google.interactions";

/// How often an interaction (and a generated file) is polled.
const POLL_EVERY: Duration = if cfg!(test) { Duration::from_millis(5) } else { Duration::from_secs(5) };
/// How often an interaction with large inline input media is polled: the
/// stored interaction returns its `user_input` steps (the media too) with
/// every `GET` ("the stored resource … also includes user_input steps").
const POLL_EVERY_HEAVY: Duration = if cfg!(test) { Duration::from_millis(5) } else { Duration::from_secs(20) };
/// Base64 characters of inline input media above which polls are spaced out.
const HEAVY_INPUT: usize = 1_000_000;

/// A Gemini Omni Flash model.
#[derive(Debug, Clone, Copy)]
pub struct Spec {
    pub id: &'static str,
    /// The longest video (seconds; the shortest is 3).
    pub max_seconds: u32,
    /// Reference images per request (`None`: not documented).
    pub max_images: Option<usize>,
    /// Videos per request (references, or one to edit or extend).
    pub max_videos: Option<usize>,
    /// USD per 1M tokens: input (text, image, video), text and thinking output, video output.
    pub input: f64,
    pub text_output: f64,
    pub video_output: f64,
}

/// The models and prices, 2026-11-13: the model page, the Omni guide, the
/// deprecations page and the price list (Standard; Omni has no other tier).
pub const MODELS: &[Spec] = &[Spec {
    id: "gemini-omni-1.2-flash",
    max_seconds: 15,
    max_images: Some(6),
    max_videos: Some(3),
    input: 1.50,
    text_output: 9.00,
    video_output: 12.00,
}];

pub fn spec(model: &str) -> Option<&'static Spec> {
    MODELS.iter().find(|s| s.id == model)
}

/// Whether `model` is a video model of the Gemini API (served by this module).
pub fn is_video_model(model: &str) -> bool {
    model.starts_with("gemini-omni") || model.starts_with("veo-")
}

/// The video output tokens per second of each resolution (the price list).
fn tokens_per_second(resolution: &str) -> Option<f64> {
    match resolution {
        "360p" => Some(1_931.0),
        "720p" => Some(5_792.0),
        "1080p" => Some(8_688.0),
        "4k" => Some(17_376.0),
        _ => None,
    }
}

/// `ImageContent.mime_type` values.
const IMAGE_TYPES: &[&str] =
    &["image/png", "image/jpeg", "image/webp", "image/heic", "image/heif", "image/gif", "image/bmp", "image/tiff"];
/// `VideoContent.mime_type` values.
const VIDEO_TYPES: &[&str] = &[
    "video/mp4",
    "video/mpeg",
    "video/mpg",
    "video/mov",
    "video/avi",
    "video/x-flv",
    "video/webm",
    "video/wmv",
    "video/3gpp",
];
/// `VideoResponseFormat.aspect_ratio` values.
const RATIOS: &[&str] = &["16:9", "9:16"];

/// What the Interactions API takes for an Omni model (official = route).
struct Caps;

impl RouteCaps for Caps {
    fn name(&self) -> &str {
        NAME
    }
    fn tool(&self, _: Tool) -> bool {
        false
    }
    fn efforts(&self) -> Option<Vec<String>> {
        None
    }
    fn can_disable_reasoning(&self) -> bool {
        false
    }
    fn budget(&self) -> bool {
        false
    }
    fn verbosity(&self) -> bool {
        false
    }
    fn image_options(&self) -> bool {
        true
    }
    fn tiers(&self) -> Vec<String> {
        // The price list has Standard only for Omni.
        Vec::new()
    }
    fn generation(&self) -> Generation {
        Generation { seconds: true, ..Default::default() }
    }
}

fn err(m: impl Into<String>) -> LlmError {
    LlmError::request(m)
}

fn src(by_user: bool) -> String {
    if by_user { "option" } else { "default" }.into()
}

/// The interaction a reply to this model's own answer continues.
fn previous_interaction(history: &[Turn], model: &str) -> Option<String> {
    let (last, earlier) = history.split_last()?;
    let prev = earlier.last().filter(|t| t.role == Role::Assistant && last.role == Role::User)?;
    let native = prev.native.as_ref().filter(|n| n.dialect == DIALECT && n.model == model)?;
    native.payload.get("id").and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string)
}

/// An input image or video as a content item (base64), its type checked.
async fn media_item(b: &BlobRef, kind: &str, model: &str, blobs: &dyn BlobLoader) -> Result<Value, LlmError> {
    let mime = media::base_mime(&b.mime);
    let (mime, allowed) = match kind {
        "image" => (if mime == "image/jpg" { "image/jpeg".to_string() } else { mime }, IMAGE_TYPES),
        // The same container under its registered name.
        _ => (if mime == "video/quicktime" { "video/mov".to_string() } else { mime }, VIDEO_TYPES),
    };
    if !allowed.contains(&mime.as_str()) {
        return Err(err(format!(
            "`{model}` does not take {} {kind}s via the {NAME}; formats: {}",
            b.mime,
            allowed.iter().map(|m| m.split_once('/').map_or(*m, |(_, t)| t)).collect::<Vec<_>>().join(", ")
        )));
    }
    let bytes = provider::load(blobs, b).await?;
    Ok(json!({"type": kind, "data": provider::b64(&bytes), "mime_type": mime}))
}

fn count(n: usize, what: &str) -> String {
    format!("{n} {what}{}", if n == 1 { "" } else { "s" })
}

/// Build an Omni request (see the module docs).
pub async fn prepare(
    cfg: &DirectConfig,
    input: &CallInput<'_>,
    presets: &[Requested],
    blobs: &dyn BlobLoader,
) -> Result<Prepared, LlmError> {
    let model = input.model;
    let user = request::parse(input.options).map_err(LlmError::request)?;
    let mut req = Requested::default();
    for p in presets {
        req = overlay(&req, p);
    }
    let req = overlay(&req, &user);
    if req.providers.is_some() {
        return Err(err(format!("`p=` (provider choice) does not apply to {NAME}")));
    }
    if req.tools.contains(&Tool::Image) {
        return Err(err(format!(
            "`{model}` is a video model: it makes videos by itself (`+i` is the image generation tool of chat models)"
        )));
    }
    let spec = spec(model);
    let mut basic = req.clone();
    basic.image_size = None;
    basic.aspect_ratio = None;
    request::validate(model, &basic, &Caps).map_err(LlmError::request)?;
    let resolution = match req.image_size.as_deref() {
        // The API's default (720p).
        None => None,
        Some("1K") => Some("720p"),
        Some("2K") => Some("1080p"),
        Some("4K") => Some("4k"),
        Some(s) => {
            return Err(err(format!(
                "video size {s} is not available for `{model}`; sizes: 1K (720p), 2K (1080p), 4K (4k)"
            )));
        }
    };
    if let Some(r) = &req.aspect_ratio
        && !RATIOS.contains(&r.as_str())
    {
        return Err(err(format!("aspect ratio {r} is not available for `{model}`; ratios: {}", RATIOS.join(", "))));
    }
    if let Some(s) = req.seconds {
        let max = spec.map_or(15, |s| s.max_seconds);
        if !(3..=max).contains(&s) {
            return Err(err(format!("`{model}` makes videos of 3–{max} s (sec={s})")));
        }
    }

    // Input: this message, continuing this model's own interaction when it replies to one.
    if input.history.iter().any(|t| t.role == Role::System) {
        return Err(err(format!(
            "`{model}` takes no system prompt (Gemini Omni Flash supports no system instructions)"
        )));
    }
    let previous = previous_interaction(input.history, model);
    let MediaInput { prompt, answer_images, answer_video, own_images, own_videos, mut notes } =
        media::media_input(input.history, model, "the Gemini API")?;
    let videos: Vec<&BlobRef> = answer_video.iter().filter(|_| previous.is_none()).chain(&own_videos).collect();
    let images: Vec<&BlobRef> = answer_images.iter().chain(&own_images).collect();
    if let Some(max) = spec.and_then(|s| s.max_images)
        && images.len() > max
    {
        return Err(err(format!("`{model}` takes at most {max} reference images ({} given)", images.len())));
    }
    if let Some(max) = spec.and_then(|s| s.max_videos)
        && videos.len() > max
    {
        return Err(err(format!("`{model}` takes at most {max} videos ({} given)", videos.len())));
    }
    if prompt.is_empty() && images.is_empty() && videos.is_empty() {
        return Err(err(format!("`{model}` needs a prompt (the text of the message)")));
    }
    let mut content = Vec::new();
    for v in &videos {
        content.push(media_item(v, "video", model, blobs).await?);
    }
    for i in &images {
        content.push(media_item(i, "image", model, blobs).await?);
    }
    let body_input = if content.is_empty() {
        json!(prompt)
    } else {
        if !prompt.is_empty() {
            content.push(json!({"type": "text", "text": prompt}));
        }
        Value::Array(content)
    };

    let mut applied = Vec::new();
    let media = [(videos.len(), "video"), (images.len(), "image")]
        .iter()
        .filter(|(n, _)| *n > 0)
        .map(|(n, w)| count(*n, w))
        .collect::<Vec<_>>()
        .join(" + ");
    let mut settings = vec![match (&previous, media.is_empty()) {
        (Some(_), true) => "follow-up of the previous video".to_string(),
        (Some(_), false) => format!("follow-up of the previous video + {media}"),
        (None, true) => "text-to-video".to_string(),
        (None, false) => format!("from {media}"),
    }];
    let mut format = Map::new();
    format.insert("type".into(), json!("video"));
    if user.image_size.is_none() && resolution != Some("4k") && spec.is_some() {
        notes.extend(request::raise_note(&[Raise::new(
            "video resolution",
            resolution.unwrap_or("720p"),
            "4k, upscaled",
            "+size=4K",
        )]));
    }
    if resolution.is_none() && spec.is_some() {
        settings.push("resolution 720p".into());
    }
    if let Some(r) = resolution {
        format.insert("resolution".into(), json!(r));
        settings.push(format!("resolution {r}"));
        applied.push(Applied {
            source: src(user.image_size.is_some()),
            patch: json!({"response_format": {"resolution": r}}),
        });
        if matches!(r, "1080p" | "4k") {
            format.insert("delivery".into(), json!("uri"));
            applied.push(Applied { source: "default".into(), patch: json!({"response_format": {"delivery": "uri"}}) });
        }
    }
    if let Some(a) = &req.aspect_ratio {
        format.insert("aspect_ratio".into(), json!(a));
        settings.push(format!("aspect ratio {a}"));
        applied.push(Applied {
            source: src(user.aspect_ratio.is_some()),
            patch: json!({"response_format": {"aspect_ratio": a}}),
        });
    }
    if let Some(s) = req.seconds {
        let d = format!("{s}s");
        format.insert("duration".into(), json!(d));
        settings.push(format!("{s} s"));
        applied
            .push(Applied { source: src(user.seconds.is_some()), patch: json!({"response_format": {"duration": d}}) });
    }

    let mut body = json!({"model": model, "input": body_input});
    if let Some(id) = &previous {
        body["previous_interaction_id"] = json!(id);
        applied.push(Applied { source: "default".into(), patch: json!({"previous_interaction_id": id}) });
    }
    if format.len() > 1 {
        body["response_format"] = Value::Object(format);
    }
    // Runs on Google's side: a restart resumes it (background requires `store`, the default).
    body["background"] = json!(true);
    applied.push(Applied { source: "default".into(), patch: json!({"background": true}) });
    if let Some(l) = input.user.as_deref().and_then(super::build::label_value) {
        applied.push(Applied { source: "default".into(), patch: json!({"labels": {"safety_identifier": l}}) });
        body["labels"] = json!({"safety_identifier": l});
    }
    let warnings = Vec::new();
    Ok(Prepared {
        dialect: DIALECT,
        model: model.to_string(),
        url: cfg.url("/interactions"),
        body,
        applied,
        warnings,
        notes,
        show_debug: false,
        auto_max_tokens: None,
        long_decode: false,
        settings,
    })
}

/// The request that resumes a video job from its checkpoint
/// (`{"interaction_id"}`, after a restart or a dropped poll): polling
/// `GET {base}/interactions/{id}` instead of creating another video.
pub fn continuation_request(p: &Prepared, item: &Value) -> Prepared {
    let mut q = p.clone();
    if let Some(id) = item.get("interaction_id").and_then(Value::as_str) {
        let base = match q.url.find("/interactions") {
            Some(i) => &q.url[..i + "/interactions".len()],
            None => q.url.as_str(),
        };
        q.url = format!("{base}/{id}");
    }
    q
}

// ------------------------------------------------------------------ errors

/// Codes that cannot succeed when retried (the API errors page: request
/// errors, generation-blocked and generation errors; snake_case HTTP statuses).
const FINAL_CODES: &[&str] = &[
    "invalid_request",
    "failed_precondition",
    "out_of_range",
    "parameter_unknown",
    "authentication",
    "payment_required",
    "permission_denied",
    "not_found",
    "model_not_found",
    "already_exists",
    "unimplemented",
    "quota_exceeded",
    "safety",
    "recitation",
    "language",
    "prohibited_content",
    "spii",
    "blocklist",
    "image_safety",
    "image_prohibited_content",
    "image_recitation",
    "image_other",
    "content_blocked",
    "jailbreak",
    "malformed_function_call",
    "malformed_tool_call",
    "unexpected_tool_call",
    "no_image",
    "too_many_tool_calls",
    "missing_thought_signature",
    "bad_request",
    "unauthorized",
    "forbidden",
];

/// Classify an Interactions API error, `{"code": "invalid_request", "message": …}`
/// (a `snake_case` code; `errors[].code` of a failed interaction may be a URI,
/// its last segment is the code), or Google's front-end error (numeric `code`
/// and `status`, also wrapped in an array) by [`super::errors::error_from_json`].
pub fn error_from_json(err: &Value, http_status: Option<u16>, retry_after: Option<u64>, kind: ErrorKind) -> LlmError {
    let err = match err {
        Value::Array(a) => a.iter().find_map(|x| x.get("error")).unwrap_or(err),
        other => other,
    };
    let Some(code) = err.get("code").and_then(Value::as_str) else {
        return super::errors::error_from_json(err, http_status, retry_after, kind);
    };
    let code = code.rsplit('/').next().unwrap_or(code);
    let message = err.get("message").and_then(Value::as_str).unwrap_or("(no message)").to_string();
    let retry = match code {
        "rate_limit_exceeded" | "too_many_requests" | "resource_exhausted" => Retry::After(retry_after.unwrap_or(20)),
        "api_error"
        | "internal"
        | "internal_error"
        | "internal_server_error"
        | "service_unavailable"
        | "unavailable"
        | "deadline_exceeded"
        | "gateway_timeout"
        | "bad_gateway"
        | "aborted"
        | "cancelled"
        | "unknown" => retry_after.map_or(Retry::Backoff, Retry::After),
        c if FINAL_CODES.contains(&c) => Retry::Never,
        _ => match http_status {
            Some(s) => retry_for_status(s, retry_after),
            None => Retry::Never,
        },
    };
    LlmError {
        kind,
        retry,
        status: http_status,
        error_type: Some(code.to_string()),
        message,
        details: Some(err.clone()),
        partial_output: false,
    }
}

// ------------------------------------------------------------------ calls

/// JSON calls to the Gemini API (key in `x-goog-api-key`), logged like the
/// streams; HTTP error bodies are kept verbatim.
struct Api<'a> {
    client: &'a reqwest::Client,
    key: &'a str,
    idle: Duration,
    observer: &'a dyn Observer,
    cancel: &'a CancellationToken,
}

impl Api<'_> {
    async fn post(&self, url: &str, body: &Value) -> Result<Value, LlmError> {
        self.observer.request(url, body);
        self.json(self.client.post(url).header("x-goog-api-key", self.key).json(body)).await
    }

    async fn get(&self, url: &str) -> Result<Value, LlmError> {
        self.observer.request(url, &Value::Null);
        self.json(self.client.get(url).header("x-goog-api-key", self.key)).await
    }

    async fn respond(&self, rb: reqwest::RequestBuilder) -> Result<reqwest::Response, LlmError> {
        tokio::select! {
            _ = self.cancel.cancelled() => Err(LlmError::cancelled()),
            r = tokio::time::timeout(self.idle, rb.send()) => match r {
                Err(_) => Err(LlmError::new(ErrorKind::Timeout, Retry::Backoff, "no response within the idle timeout")),
                Ok(Err(e)) => Err(LlmError::network(format!("request failed: {e}"))),
                Ok(Ok(r)) => Ok(r),
            }
        }
    }

    /// The body of `resp` (within the idle timeout), or the HTTP error it carries.
    async fn read(&self, resp: reqwest::Response, what: &str) -> Result<(Vec<u8>, Option<String>), LlmError> {
        let status = resp.status().as_u16();
        let headers: Vec<(String, String)> = resp
            .headers()
            .iter()
            .map(|(k, v)| (k.to_string(), String::from_utf8_lossy(v.as_bytes()).to_string()))
            .collect();
        self.observer.response_status(status, &headers);
        let (retry_after, should_retry) = crate::chatstream::retry_hints(resp.headers());
        let mime = resp.headers().get("content-type").and_then(|v| v.to_str().ok()).map(media::base_mime);
        let bytes = tokio::select! {
            _ = self.cancel.cancelled() => return Err(LlmError::cancelled()),
            b = tokio::time::timeout(self.idle, resp.bytes()) => match b {
                Err(_) => return Err(LlmError::new(ErrorKind::Timeout, Retry::Backoff, format!("{what} not complete within the idle timeout"))),
                Ok(Err(e)) => return Err(LlmError::network(format!("{what} read error: {e}"))),
                Ok(Ok(b)) => b.to_vec(),
            }
        };
        if !(200..300).contains(&status) {
            let text = String::from_utf8_lossy(&bytes).to_string();
            self.observer.body_text(&text);
            let mut e = crate::chatstream::http_error(status, retry_after, &text, error_from_json);
            // The server's explicit hint wins, as in the official SDKs.
            match should_retry {
                Some(false) => e.retry = Retry::Never,
                Some(true) if e.retry == Retry::Never => e.retry = retry_after.map_or(Retry::Backoff, Retry::After),
                _ => {}
            }
            return Err(e);
        }
        Ok((bytes, mime))
    }

    async fn json(&self, rb: reqwest::RequestBuilder) -> Result<Value, LlmError> {
        let resp = self.respond(rb).await?;
        let (bytes, _) = self.read(resp, "response").await?;
        let text = String::from_utf8_lossy(&bytes);
        // Logged as data: the bot's logger stores the large base64 media of a JSON body as blobs.
        self.observer.sse_data(&text);
        serde_json::from_str(&text).map_err(|e| {
            LlmError::new(ErrorKind::Decode, Retry::Backoff, format!("bad JSON response: {e}"))
                .with_details(Value::String(text.chars().take(4000).collect()))
        })
    }

    /// Download a file the model generated: from the Files API with the key
    /// (`keyed`), else (another host) without credentials.
    async fn download(&self, url: &str, keyed: bool) -> Result<(Vec<u8>, Option<String>), LlmError> {
        self.observer.request(url, &Value::Null);
        let rb = self.client.get(url);
        let rb = if keyed { rb.header("x-goog-api-key", self.key) } else { rb };
        let (bytes, mime) = self.read(self.respond(rb).await?, "download").await?;
        self.observer.body_text(&format!("({} bytes of {})", bytes.len(), mime.as_deref().unwrap_or("unknown type")));
        Ok((bytes, mime))
    }
}

// ------------------------------------------------------------------ running

/// What the tool call shows of a request: the prompt, the media (without
/// bytes), the response format and the interaction continued.
fn video_input(body: &Value) -> Value {
    let mut o = Map::new();
    match body.get("input") {
        Some(Value::String(s)) => {
            o.insert("prompt".into(), json!(s));
        }
        Some(Value::Array(items)) => {
            let text: Vec<&str> = items.iter().filter_map(|c| c.get("text").and_then(Value::as_str)).collect();
            if !text.is_empty() {
                o.insert("prompt".into(), json!(text.join("\n\n")));
            }
            let media: Vec<Value> = items
                .iter()
                .filter(|c| c.get("type").and_then(Value::as_str) != Some("text"))
                .map(|c| json!({"type": c.get("type"), "mime_type": c.get("mime_type")}))
                .collect();
            if !media.is_empty() {
                o.insert("media".into(), json!(media));
            }
        }
        _ => {}
    }
    for k in ["response_format", "previous_interaction_id"] {
        if let Some(v) = body.get(k) {
            o.insert(k.into(), v.clone());
        }
    }
    Value::Object(o)
}

fn call(id: &str, status: &str, input: Option<Value>, output: Option<Value>) -> StreamEvent {
    StreamEvent::Tool(ToolCall {
        id: Some(id.to_string()),
        name: "video_generation".into(),
        status: Some(status.to_string()),
        input,
        output,
    })
}

fn status_of(r: &Value) -> String {
    r.get("status").and_then(Value::as_str).unwrap_or("in_progress").to_string()
}

/// The error of an interaction that ended without a result (see the module docs).
fn job_error(status: &str, r: &Value) -> LlmError {
    let first = r.get("errors").and_then(Value::as_array).and_then(|a| a.first()).filter(|e| e.is_object());
    let mut e = match (status, first) {
        ("failed", Some(e)) => error_from_json(e, None, None, ErrorKind::InBand),
        ("failed", None) => LlmError::new(
            ErrorKind::InBand,
            Retry::Backoff,
            "the interaction failed without an error; a new one may succeed",
        ),
        ("cancelled", _) => LlmError::new(ErrorKind::InBand, Retry::Backoff, "the interaction was cancelled"),
        (other, _) => LlmError::new(
            ErrorKind::InBand,
            Retry::Never,
            format!("the interaction ended with status {other}, which a video request cannot continue"),
        ),
    };
    e.details = Some(media::without_media(r, "data"));
    e
}

/// Run an Omni request: create the interaction in the background (unless
/// resuming one), checkpoint its id, poll it, fetch the video.
pub async fn stream(
    client: &reqwest::Client,
    cfg: &DirectConfig,
    p: &Prepared,
    sink: &mut dyn EventSink,
    observer: &dyn Observer,
    cancel: &CancellationToken,
) -> (Result<Value, LlmError>, CallOutcome) {
    let api = Api { client, key: &cfg.api_key, idle: cfg.idle_timeout, observer, cancel };
    let mut outcome = CallOutcome::default();
    let result = job(&api, cfg, p, sink, &mut outcome).await;
    (result, outcome)
}

async fn job(
    api: &Api<'_>,
    cfg: &DirectConfig,
    p: &Prepared,
    sink: &mut dyn EventSink,
    outcome: &mut CallOutcome,
) -> Result<Value, LlmError> {
    let (id, poll_url, mut next) = if p.url.ends_with("/interactions") {
        let created = api.post(&p.url, &p.body).await?;
        let Some(id) = created.get("id").and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string) else {
            return Err(LlmError::new(
                ErrorKind::Decode,
                Retry::Backoff,
                "the Interactions API returned no interaction id",
            )
            .with_details(media::without_media(&created, "data")));
        };
        let model = created.get("model").and_then(Value::as_str).map(str::to_string);
        sink.event(StreamEvent::Meta { upstream_id: Some(id.clone()), provider: None, model });
        sink.event(call(&id, &status_of(&created), Some(video_input(&p.body)), None));
        // A restart polls this interaction instead of paying for another.
        let item = json!({"interaction_id": id});
        sink.event(StreamEvent::Checkpoint(item.clone()));
        let poll_url = continuation_request(p, &item).url;
        (id, poll_url, Some(created))
    } else {
        // Resuming (see [`continuation_request`]).
        (p.url.rsplit('/').next().unwrap_or_default().to_string(), p.url.clone(), None)
    };
    outcome.upstream_id = Some(id.clone());
    outcome.continuation = Some(json!({"interaction_id": id}));
    let inline: usize = p
        .body
        .get("input")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|c| c.get("data").and_then(Value::as_str))
        .map(str::len)
        .sum();
    let every = if inline > HEAVY_INPUT { POLL_EVERY_HEAVY } else { POLL_EVERY };
    let mut shown = next.as_ref().map(status_of).unwrap_or_default();
    loop {
        let r = match next.take() {
            Some(r) => r,
            None => api.get(&poll_url).await?,
        };
        let status = status_of(&r);
        match status.as_str() {
            "completed" | "incomplete" => return finish(api, cfg, p, &id, r, sink).await,
            "failed" | "cancelled" | "requires_action" | "requires_continuation" => {
                let errors = r.get("errors").cloned().unwrap_or(Value::Null);
                sink.event(call(&id, &status, None, Some(json!({"status": status, "errors": errors}))));
                // Final: not resumable from the checkpoint (a retry makes a new job).
                sink.event(StreamEvent::Checkpoint(Value::Null));
                outcome.continuation = None;
                return Err(job_error(&status, &r));
            }
            _ => {
                if status != shown {
                    sink.event(call(&id, &status, None, None));
                    shown = status;
                }
                media::pause(every, api.cancel).await?;
            }
        }
    }
}

/// A generated file referenced by URI (`delivery: "uri"`): a Files API file
/// (`…/files/{name}:download?alt=media`, or `files/{name}`) is polled until it
/// is `ACTIVE` and downloaded through the configured base URL with the key;
/// a URI of another kind is fetched as it is, without credentials.
async fn fetch_file(api: &Api<'_>, cfg: &DirectConfig, uri: &str) -> Result<(Vec<u8>, Option<String>), LlmError> {
    let name = uri
        .split_once("files/")
        .filter(|(head, _)| head.is_empty() || head.ends_with('/'))
        .and_then(|(_, rest)| rest.split([':', '?', '/']).next())
        .filter(|n| !n.is_empty());
    let Some(name) = name else {
        if !uri.starts_with("http") {
            let e = LlmError::new(ErrorKind::Decode, Retry::Never, format!("unrecognized video URI `{uri}`"));
            return Err(e);
        }
        return api.download(uri, false).await;
    };
    loop {
        let f = api.get(&cfg.url(&format!("/files/{name}"))).await?;
        match f.get("state").and_then(Value::as_str) {
            Some("ACTIVE") | None => break,
            Some("FAILED") => {
                let mut e =
                    LlmError::new(ErrorKind::InBand, Retry::Never, "the generated video file failed processing");
                e.details = Some(f);
                return Err(e);
            }
            Some(_) => media::pause(POLL_EVERY, api.cancel).await?,
        }
    }
    api.download(&cfg.url(&format!("/files/{name}:download?alt=media")), true).await
}

/// One output of a finished interaction, in the steps' order.
enum Out {
    Thought(String, usize),
    Text(String),
    Media(Value),
}

/// A finished interaction: its reasoning, text and video, the usage, the finish.
async fn finish(
    api: &Api<'_>,
    cfg: &DirectConfig,
    p: &Prepared,
    id: &str,
    r: Value,
    sink: &mut dyn EventSink,
) -> Result<Value, LlmError> {
    let status = status_of(&r);
    let served = r.get("model").and_then(Value::as_str).map(str::to_string);
    sink.event(StreamEvent::Meta { upstream_id: Some(id.to_string()), provider: None, model: served.clone() });
    let mut outs = Vec::new();
    for (i, step) in r.get("steps").and_then(Value::as_array).into_iter().flatten().enumerate() {
        let items = |k: &str| step.get(k).and_then(Value::as_array).cloned().unwrap_or_default();
        match step.get("type").and_then(Value::as_str) {
            // `summary` per the reference; the guide's example has `content`.
            Some("thought") => {
                for c in items("summary").into_iter().chain(items("content")) {
                    if let Some(t) = c.get("text").and_then(Value::as_str).filter(|t| !t.is_empty()) {
                        outs.push(Out::Thought(t.to_string(), i));
                    }
                }
            }
            Some("model_output") => {
                for c in items("content") {
                    match c.get("type").and_then(Value::as_str) {
                        Some("text") => {
                            if let Some(t) = c.get("text").and_then(Value::as_str).filter(|t| !t.is_empty()) {
                                outs.push(Out::Text(t.to_string()));
                            }
                        }
                        Some(_) => outs.push(Out::Media(c)),
                        None => {}
                    }
                }
            }
            _ => {}
        }
    }
    // The videos as the tool call shows them: what the API says of them, without the bytes.
    let videos: Vec<Value> = outs
        .iter()
        .filter_map(|o| match o {
            Out::Media(c) if c.get("type").and_then(Value::as_str) == Some("video") => {
                let mut c = c.as_object().cloned().unwrap_or_default();
                c.remove("data");
                Some(Value::Object(c))
            }
            _ => None,
        })
        .collect();
    sink.event(call(id, &status, None, Some(json!({"status": status, "model": served, "video": videos}))));
    let has_text = outs.iter().any(|o| matches!(o, Out::Text(_)));
    let mut delivered = 0;
    let seconds = p.body.pointer("/response_format/duration").and_then(Value::as_str).and_then(|d| d.strip_suffix('s'));
    let name = match seconds {
        Some(s) => format!("{}-{s}s.mp4", p.model),
        None => format!("{}.mp4", p.model),
    };
    for o in outs {
        match o {
            Out::Thought(text, block) => {
                sink.event(StreamEvent::Reasoning { kind: ReasoningKind::Summary, text, block })
            }
            Out::Text(t) => sink.event(StreamEvent::Text(t)),
            Out::Media(c) => {
                let kind = c.get("type").and_then(Value::as_str).unwrap_or_default().to_string();
                let declared = c.get("mime_type").and_then(Value::as_str).map(str::to_string);
                let (data, mime) = match (c.get("data").and_then(Value::as_str), c.get("uri").and_then(Value::as_str)) {
                    (Some(d), _) if !d.is_empty() => match media::decode_b64(d) {
                        Some(bytes) => (bytes, declared.clone()),
                        None => {
                            return Err(LlmError::new(
                                ErrorKind::Decode,
                                Retry::Backoff,
                                format!("the {kind} is not valid base64"),
                            )
                            .with_details(media::without_media(&c, "data")));
                        }
                    },
                    (_, Some(u)) if !u.is_empty() => {
                        let (bytes, header) = fetch_file(api, cfg, u).await?;
                        (bytes, declared.clone().or(header))
                    }
                    _ => continue,
                };
                match kind.as_str() {
                    "image" => {
                        let mime = media::sniff_image(&data, declared.as_deref().unwrap_or("image/png"));
                        sink.event(StreamEvent::Image { data, mime });
                    }
                    _ => {
                        let mime = mime.filter(|m| m.contains('/')).unwrap_or_else(|| "video/mp4".into());
                        let name = (kind == "video").then(|| name.clone());
                        delivered += usize::from(kind == "video");
                        sink.event(StreamEvent::File { data, mime, name });
                    }
                }
            }
        }
    }
    sink.event(StreamEvent::Usage(usage(&p.model, served.as_deref(), r.get("usage"), &p.body)));
    if delivered == 0 {
        if !has_text {
            let mut e = LlmError::new(ErrorKind::InBand, Retry::Never, "the interaction completed without a video");
            e.details = Some(media::without_media(&r, "data"));
            return Err(e);
        }
        sink.event(StreamEvent::Notice(format!("{} answered without a video", p.model)));
    }
    let reason = if status == "incomplete" { "length" } else { "stop" };
    sink.event(StreamEvent::Finish { reason: Some(reason.into()), native: Some(status) });
    Ok(media::without_media(&r, "data"))
}

/// Tokens of one modality in a `*_by_modality` list (`None`: no list).
fn by_modality(u: &Value, key: &str, m: &str) -> Option<u64> {
    let list = u.get(key)?.as_array()?;
    Some(
        list.iter()
            .filter(|x| x.get("modality").and_then(Value::as_str) == Some(m))
            .filter_map(|x| x.get("tokens").and_then(Value::as_u64))
            .sum(),
    )
}

/// Normalized usage of an interaction and its estimated cost (see the module docs).
fn usage(model: &str, served: Option<&str>, u: Option<&Value>, body: &Value) -> Usage {
    let priced = served.and_then(spec).or_else(|| spec(model));
    let Some(u) = u.filter(|u| u.is_object()) else {
        // No usage: the video's length at the resolution's token rate.
        let cost = priced.and_then(|s| {
            let res = body.pointer("/response_format/resolution").and_then(Value::as_str).unwrap_or("720p");
            let secs = body
                .pointer("/response_format/duration")
                .and_then(Value::as_str)
                .and_then(|d| d.strip_suffix('s'))
                .and_then(|d| d.parse::<f64>().ok())
                .unwrap_or(10.0);
            tokens_per_second(res).map(|t| t * secs * s.video_output / 1e6)
        });
        return Usage {
            cost_usd: cost,
            cost_source: if cost.is_some() { CostSource::Estimated } else { CostSource::Unknown },
            ..Default::default()
        };
    };
    let g = |k: &str| u.get(k).and_then(Value::as_u64);
    let sum = |a: Option<u64>, b: Option<u64>| match (a, b) {
        (None, None) => None,
        (a, b) => Some(a.unwrap_or(0) + b.unwrap_or(0)),
    };
    let (input, tools, output, thoughts) =
        (g("total_input_tokens"), g("total_tool_use_tokens"), g("total_output_tokens"), g("total_thought_tokens"));
    let cost = priced.zip(output).map(|(s, out)| {
        // Output is the video ("billing is based on total output token consumption") unless broken down.
        let video = by_modality(u, "output_tokens_by_modality", "video").unwrap_or(out).min(out);
        let f = |n: u64| n as f64;
        (f(input.unwrap_or(0) + tools.unwrap_or(0)) * s.input
            + f(out - video + thoughts.unwrap_or(0)) * s.text_output
            + f(video) * s.video_output)
            / 1e6
    });
    Usage {
        input_tokens: sum(input, tools),
        output_tokens: sum(output, thoughts),
        reasoning_tokens: thoughts.filter(|t| *t > 0),
        cached_input_tokens: g("total_cached_tokens"),
        cost_usd: cost,
        cost_source: if cost.is_some() { CostSource::Estimated } else { CostSource::Unknown },
        raw: u.clone(),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests;
