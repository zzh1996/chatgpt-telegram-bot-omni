//! xAI's Imagine API for bare Grok Imagine ids — images (`POST
//! {base}/images/generations`, `/images/edits`) and videos (`POST
//! {base}/videos/generations`, `/videos/edits`, then `GET
//! {base}/videos/{request_id}` until done) — with no chat model (the owner's
//! rule). `+i` on a chat model is the Responses API's `image_generation` tool
//! instead ([`super`]).
//!
//! Sources (fetched 2026-11-13): docs.x.ai Imagine overview, image generation,
//! editing and multi-image editing, the video overview, generation,
//! image-to-video, reference-to-video, editing and extension guides, the
//! image generation tool, the REST reference (images, videos), the models and
//! pricing pages, the 2026-11-02 image quality migration note,
//! `openapi.json`, and unauthenticated probes of the request validation
//! (2026-11-13): `quality` (`low`, `medium`, `high`, `auto`), `background`
//! (`transparent`, `opaque`, `auto`), `generate_audio` and `last_frame` are
//! fields the API knows although `openapi.json` lists none of them.
//!
//! **Images** (xAI recommends `grok-imagine-image-2.0`; `grok-imagine-image`
//! is 1.0):
//! - Generations from the prompt; edits when there are images (the reply's
//!   generated image first, then the message's own; one as `image`, up to
//!   five as `images`; JPEG, PNG or WebP data URLs).
//! - `resolution`: `2k` by default (the largest; `1` → `1k`, `2` → `2k`;
//!   `1.5k` has no option). `aspect_ratio` from `S W H w h` / `ar=`, from
//!   xAI's list (none: `auto` for generations, the first image's for edits).
//! - `quality` (2.0 only): `medium` by default, the highest the model takes
//!   (`auto`, the API default, serves `low` for generations and `medium` for
//!   edits); `q=` takes `low`, `medium` and `auto`. `high` is in the API's
//!   enum and price tiers, but the model rejects it (live, 2026-11-13: "This
//!   model only supports the following quality value(s): low, medium,
//!   auto."), so the bot rejects it before sending.
//! - `n` from `n=` (1–10); `background` from `bg=` (in the API's enum, not in
//!   the guide); `response_format: "b64_json"` (the URLs are temporary);
//!   `user` = the hashed user. No streaming or partial images, and no
//!   moderation or watermark option exists.
//!
//! **Videos** (xAI recommends `grok-imagine-video-1.5`, `-1.5-lite` for
//! simple videos; `grok-imagine-video` is the older model, the only one that
//! edits):
//! - Text-to-video (the prompt); image-to-video (one image, the first frame;
//!   the video takes its aspect ratio); reference-to-video (two or more
//!   images as `reference_images`: up to 14 on 1.5, 7 on grok-imagine-video,
//!   none on lite; at most 720p); a video edit (a reply to a generated video,
//!   or an attached MP4; grok-imagine-video only; the result keeps the
//!   source's length, ratio and resolution, at most 720p).
//! - `resolution`: the largest for the mode by default (1080p on 1.5 and lite
//!   for text- and image-to-video, else 720p); `1` → 720p, `2` → 1080p (480p
//!   has no option). `duration` from `sec=` (1–15 s; reference-to-video on
//!   grok-imagine-video up to 10): xAI's default (8 s) otherwise — length is
//!   content, not quality. `aspect_ratio` from the ratio options for text-
//!   and reference-to-video (xAI's default 16:9). Options a mode cannot use
//!   (a conversation's options carry over to replies) are not sent, with a
//!   note.
//! - Asynchronous: the job is created, its `request_id` checkpointed
//!   ([`continuation_request`] polls it again after a restart or a dropped
//!   connection, instead of making a new video), polled every 5 s with its
//!   progress on the tool call, then the MP4 is downloaded (no credentials
//!   sent to its URL) as a `File`. A video that fails moderation
//!   (`respect_moderation: false`, no URL) is a content-filter stop. A job
//!   that ended `failed` or `expired` clears its checkpoint
//!   (`Checkpoint(null)`) before the error: polling it again cannot change
//!   the outcome, so the bot does not resume it; a retryable failure is
//!   retried with a new job, as any failed attempt.
//! - Not used, since no bot option can ask for them: first/last frame pins
//!   and keyframes, voice references, video extension, silent videos
//!   (`generate_audio: false`); nor Files API inputs, storage options,
//!   webhooks or deferred images.
//!
//! Cost: as billed (`usage.cost_in_usd_ticks`); if missing, estimated from
//! the list prices below (per image at low quality, per second), a lower
//! bound.

use std::time::Duration;

use omni_core::Usage;
use omni_core::content::ToolCall;
use omni_core::usage::CostSource;
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use crate::error::{ErrorKind, LlmError, Retry};
use crate::event::StreamEvent;
use crate::openai::media::{self, Http, MediaInput};
use crate::openrouter::options::Applied;
use crate::provider::DirectConfig;
use crate::request::{self, Generation, Requested, RouteCaps, Tool};
use crate::{BlobLoader, CallInput, CallOutcome, EventSink, Observer, Prepared};

pub const IMAGE_DIALECT: &str = "xai.images";
pub const VIDEO_DIALECT: &str = "xai.videos";

const NAME: &str = "xAI";

/// How often a video job is polled.
pub const POLL_EVERY: Duration = Duration::from_secs(5);

/// A Grok Imagine image model.
#[derive(Debug, Clone, Copy)]
pub struct ImageSpec {
    pub id: &'static str,
    pub aliases: &'static [&'static str],
    /// Takes `quality`.
    pub quality: bool,
    /// USD per image at 1k / 2k (low quality, the lowest tier).
    pub per_image: (f64, f64),
    /// USD per input image of an edit.
    pub per_input_image: f64,
}

/// A Grok Imagine video model.
#[derive(Debug, Clone, Copy)]
pub struct VideoSpec {
    pub id: &'static str,
    pub aliases: &'static [&'static str],
    /// The highest resolution of text- and image-to-video.
    pub max_resolution: &'static str,
    /// Reference images of reference-to-video (0: not supported).
    pub references: usize,
    /// Longest reference-to-video (seconds).
    pub reference_seconds: u32,
    /// Edits videos.
    pub edit: bool,
    /// USD per second of video (the pricing page's single figure).
    pub per_second: f64,
}

/// xAI's image models, 2026-11-13: pricing page ($0.04 / $0.02 per image),
/// migration note (2.0 at low: $0.04 per 1K image, $0.06 per 2K image, plus
/// $0.01 per input image on edits).
pub const IMAGE_MODELS: &[ImageSpec] = &[
    ImageSpec {
        id: "grok-imagine-image-2.0",
        aliases: &[],
        quality: true,
        per_image: (0.04, 0.06),
        per_input_image: 0.01,
    },
    ImageSpec {
        id: "grok-imagine-image",
        aliases: &["grok-imagine-image-2026-03-02"],
        quality: false,
        per_image: (0.02, 0.02),
        per_input_image: 0.0,
    },
];

/// xAI's video models, 2026-11-13: the video overview's capability table, the
/// reference-to-video limits and the pricing page.
pub const VIDEO_MODELS: &[VideoSpec] = &[
    VideoSpec {
        id: "grok-imagine-video-1.5",
        aliases: &["grok-imagine-video-1.5-preview", "grok-imagine-video-1.5-2026-05-30"],
        max_resolution: "1080p",
        references: 14,
        reference_seconds: 15,
        edit: false,
        per_second: 0.08,
    },
    VideoSpec {
        id: "grok-imagine-video-1.5-lite",
        aliases: &[],
        max_resolution: "1080p",
        references: 0,
        reference_seconds: 0,
        edit: false,
        per_second: 0.02,
    },
    VideoSpec {
        id: "grok-imagine-video",
        aliases: &[],
        max_resolution: "720p",
        references: 7,
        reference_seconds: 10,
        edit: true,
        per_second: 0.05,
    },
];

pub fn is_image_model(model: &str) -> bool {
    model.starts_with("grok-imagine-image")
}

pub fn is_video_model(model: &str) -> bool {
    model.starts_with("grok-imagine-video")
}

pub fn image_spec(model: &str) -> Option<&'static ImageSpec> {
    IMAGE_MODELS.iter().find(|s| s.id == model || s.aliases.contains(&model))
}

pub fn video_spec(model: &str) -> Option<&'static VideoSpec> {
    VIDEO_MODELS.iter().find(|s| s.id == model || s.aliases.contains(&model))
}

/// The aspect ratios of the images API (probed enum; `19.5:9` / `9:19.5`
/// cannot be written as `ar=`) and of the videos API.
const IMAGE_RATIOS: &[&str] = &[
    "1:1", "3:4", "4:3", "9:16", "16:9", "2:3", "3:2", "9:19.5", "19.5:9", "9:20", "20:9", "1:2", "2:1", "21:9", "5:2",
];
const VIDEO_RATIOS: &[&str] = &["1:1", "16:9", "9:16", "4:3", "3:4", "3:2", "2:3", "21:9", "5:2"];
/// What `grok-imagine-image-2.0` takes (`high` is in the enum but rejected, live 2026-11-13).
const QUALITIES: &[&str] = &["low", "medium", "auto"];
const BACKGROUNDS: &[&str] = &["auto", "opaque", "transparent"];
const MAX_N: u32 = 10;
const MAX_EDIT_IMAGES: usize = 5;
/// Images xAI takes as input ("JPEG, PNG, or WebP").
const INPUT_TYPES: &[&str] = &["image/jpeg", "image/png", "image/webp"];

/// What the Imagine API takes for a model (official = route).
struct Caps {
    quality: bool,
    count: bool,
    background: bool,
    seconds: bool,
}

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
        // Priority processing "is not supported for image generation, video generation".
        Vec::new()
    }
    fn generation(&self) -> Generation {
        Generation { quality: self.quality, count: self.count, background: self.background, seconds: self.seconds }
    }
}

fn src(by_user: bool) -> String {
    if by_user { "option" } else { "default" }.into()
}

/// The generic checks (tools, effort, verbosity, tiers, which generation
/// options apply); sizes and ratios are checked per model by the caller.
fn validate(model: &str, req: &Requested, caps: &Caps, what: &str) -> Result<(), LlmError> {
    if req.tools.contains(&Tool::Image) {
        return Err(LlmError::request(format!(
            "`{model}` is {what}: it makes them by itself (`+i` is the image generation tool of chat models)"
        )));
    }
    let mut checked = req.clone();
    checked.image_size = None;
    checked.aspect_ratio = None;
    checked.verbosity = None;
    request::validate(model, &checked, caps).map_err(LlmError::request)?;
    if req.verbosity.is_some() {
        return Err(LlmError::request(format!("`{model}` has no verbosity setting")));
    }
    Ok(())
}

fn one_of(model: &str, what: &str, v: &str, valid: &[&str]) -> Result<(), LlmError> {
    if valid.contains(&v) {
        return Ok(());
    }
    Err(LlmError::request(format!(
        "{what} `{v}` is not available for `{model}`; {}: {}",
        plural_of(what),
        valid.join(", ")
    )))
}

fn plural_of(what: &str) -> String {
    match what {
        "image quality" => "levels".into(),
        "aspect ratio" => "ratios".into(),
        other => format!("{other}s"),
    }
}

// ------------------------------------------------------------------ images

/// Build an images request (generation or edit) for a Grok Imagine image model.
pub(crate) async fn prepare_image(
    cfg: &DirectConfig,
    input: &CallInput<'_>,
    req: &Requested,
    user: &Requested,
    blobs: &dyn BlobLoader,
) -> Result<Prepared, LlmError> {
    let model = input.model;
    let spec = image_spec(model);
    let quality_ok = spec.is_none_or(|s| s.quality);
    let caps = Caps { quality: quality_ok, count: true, background: true, seconds: false };
    validate(model, req, &caps, "an image model")?;
    if let Some(q) = &req.quality {
        one_of(model, "image quality", q, QUALITIES)?;
    }
    if let Some(b) = &req.background {
        one_of(model, "background", b, BACKGROUNDS)?;
    }
    let n = req.count.unwrap_or(1);
    if n > MAX_N {
        return Err(LlmError::request(format!("`{model}` makes at most {MAX_N} images per request (n={n})")));
    }
    let resolution = match req.image_size.as_deref() {
        None => spec.map(|_| "2k"),
        Some("1K") => Some("1k"),
        Some("2K") => Some("2k"),
        Some(s) => {
            return Err(LlmError::request(format!("image size {s} is not available for `{model}`; sizes: 1K, 2K")));
        }
    };
    if let Some(r) = &req.aspect_ratio {
        one_of(model, "aspect ratio", r, IMAGE_RATIOS)?;
    }

    let MediaInput { prompt, answer_images, answer_video, own_images, own_videos, notes } =
        media::media_input(input.history, model, "xAI's image API")?;
    if !own_videos.is_empty() {
        return Err(LlmError::request(format!("`{model}` takes images, not video, via xAI")));
    }
    if answer_video.is_some() {
        return Err(LlmError::request(format!(
            "`{model}` takes images, not the video of the answer replied to, via xAI's image API"
        )));
    }
    if prompt.is_empty() {
        return Err(LlmError::request(format!("`{model}` needs a prompt (the text of the message)")));
    }
    let images: Vec<_> = answer_images.iter().chain(&own_images).collect();
    if images.len() > MAX_EDIT_IMAGES {
        return Err(LlmError::request(format!(
            "`{model}` edits with at most {MAX_EDIT_IMAGES} source images ({} given)",
            images.len()
        )));
    }
    let mut body = json!({"model": model, "prompt": prompt});
    let mut applied = Vec::new();
    let mut settings = Vec::new();
    let edit = !images.is_empty();
    if edit {
        let mut refs = Vec::new();
        for b in &images {
            refs.push(json!({"url": media::image_data_url(b, INPUT_TYPES, None, model, "xAI", blobs).await?}));
        }
        if refs.len() == 1 {
            body["image"] = refs.remove(0);
        } else {
            body["images"] = json!(refs);
        }
        settings.push(match (answer_images.len(), own_images.len()) {
            (0, k) => format!("edit ({k} source image{})", plural(k)),
            (a, 0) => format!("edit of the previous image{}", plural(a)),
            (a, k) => format!("edit of the previous image{} + {k} source image{}", plural(a), plural(k)),
        });
    }
    if let Some(q) = req.quality.as_deref().or(spec.filter(|s| s.quality).map(|_| "medium")) {
        body["quality"] = json!(q);
        applied.push(Applied { source: src(user.quality.is_some()), patch: json!({"quality": q}) });
        settings.insert(0, format!("quality {q}"));
    }
    if let Some(r) = resolution {
        body["resolution"] = json!(r);
        applied.push(Applied { source: src(user.image_size.is_some()), patch: json!({"resolution": r}) });
        settings.push(format!("resolution {r}"));
    }
    if let Some(r) = &req.aspect_ratio {
        body["aspect_ratio"] = json!(r);
        applied.push(Applied { source: src(user.aspect_ratio.is_some()), patch: json!({"aspect_ratio": r}) });
        settings.push(format!("aspect ratio {r}"));
    }
    if let Some(b) = &req.background {
        body["background"] = json!(b);
        applied.push(Applied { source: src(user.background.is_some()), patch: json!({"background": b}) });
        settings.push(format!("background {b}"));
    }
    if n > 1 {
        body["n"] = json!(n);
        applied.push(Applied { source: src(user.count.is_some()), patch: json!({"n": n}) });
        settings.push(format!("{n} images"));
    }
    body["response_format"] = json!("b64_json");
    if let Some(u) = &input.user {
        body["user"] = json!(u);
    }
    let warnings = Vec::new();
    Ok(Prepared {
        dialect: IMAGE_DIALECT,
        model: model.to_string(),
        url: cfg.url(if edit { "/images/edits" } else { "/images/generations" }),
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

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// Normalized usage of a media response: the billed cost (`cost_in_usd_ticks`,
/// 10^10 per USD), else `estimate`; token counts as reported (images).
fn media_usage(u: Option<&Value>, estimate: Option<f64>) -> Usage {
    let u = u.cloned().unwrap_or(Value::Null);
    let g = |p: &str| u.pointer(p).and_then(Value::as_u64);
    let reported = u.get("cost_in_usd_ticks").and_then(Value::as_f64).map(|t| t / 1e10);
    let cost = reported.or(estimate);
    Usage {
        input_tokens: g("/input_tokens"),
        output_tokens: g("/output_tokens"),
        reasoning_tokens: g("/output_tokens_details/reasoning_tokens").filter(|r| *r > 0),
        cached_input_tokens: g("/input_tokens_details/cached_tokens"),
        cost_usd: cost,
        cost_source: match (reported, cost) {
            (Some(_), _) => CostSource::Reported,
            (None, Some(_)) => CostSource::Estimated,
            _ => CostSource::Unknown,
        },
        raw: u,
        ..Default::default()
    }
}

/// List-price estimate of an images request (only used without a billed cost).
fn image_estimate(p: &Prepared, images: usize) -> Option<f64> {
    let s = image_spec(&p.model)?;
    let per =
        if p.body.get("resolution").and_then(Value::as_str) == Some("2k") { s.per_image.1 } else { s.per_image.0 };
    let inputs = p
        .body
        .get("images")
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or(usize::from(p.body.get("image").is_some()));
    Some(per * images as f64 + s.per_input_image * inputs as f64)
}

fn http<'a>(
    client: &'a reqwest::Client,
    cfg: &'a DirectConfig,
    observer: &'a dyn Observer,
    cancel: &'a CancellationToken,
) -> Http<'a> {
    Http {
        client,
        api_key: &cfg.api_key,
        idle_timeout: cfg.idle_timeout,
        map_error: super::error_from_json,
        refine: super::refine,
        observer,
        cancel,
    }
}

/// Run an images request: one JSON response with all images.
pub(crate) async fn stream_image(
    client: &reqwest::Client,
    cfg: &DirectConfig,
    p: &Prepared,
    sink: &mut dyn EventSink,
    observer: &dyn Observer,
    cancel: &CancellationToken,
) -> (Result<Value, LlmError>, CallOutcome) {
    let reply = match http(client, cfg, observer, cancel).post(&p.url, &p.body).await {
        Ok(r) => r,
        Err(e) => return (Err(e), CallOutcome::default()),
    };
    let outcome = CallOutcome { upstream_id: reply.request_id.clone(), ..Default::default() };
    if reply.request_id.is_some() {
        sink.event(StreamEvent::Meta { upstream_id: reply.request_id.clone(), provider: None, model: None });
    }
    let mut n = 0;
    for d in reply.body.get("data").and_then(Value::as_array).into_iter().flatten() {
        if let Some(bytes) = d.get("b64_json").and_then(Value::as_str).and_then(media::decode_b64) {
            let declared = d.get("mime_type").and_then(Value::as_str).unwrap_or("image/jpeg");
            let mime = media::sniff_image(&bytes, declared);
            sink.event(StreamEvent::Image { data: bytes, mime });
            n += 1;
        }
    }
    sink.event(StreamEvent::Usage(media_usage(reply.body.get("usage"), image_estimate(p, n))));
    let native = media::without_media(&reply.body, "b64_json");
    if n == 0 {
        let mut e = LlmError::new(ErrorKind::InBand, Retry::Never, "the response carries no image");
        e.details = Some(native);
        return (Err(e), outcome);
    }
    sink.event(StreamEvent::Finish { reason: Some("stop".into()), native: None });
    (Ok(native), outcome)
}

// ------------------------------------------------------------------ videos

/// The kind of video request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Text,
    Image,
    References(usize),
    Edit,
}

impl Mode {
    fn describe(self) -> String {
        match self {
            Mode::Text => "text-to-video".into(),
            Mode::Image => "image-to-video".into(),
            Mode::References(n) => format!("reference-to-video ({n} images)"),
            Mode::Edit => "video edit".into(),
        }
    }
}

/// Build a video request for a Grok Imagine video model (see the module docs).
pub(crate) async fn prepare_video(
    cfg: &DirectConfig,
    input: &CallInput<'_>,
    req: &Requested,
    user: &Requested,
    blobs: &dyn BlobLoader,
) -> Result<Prepared, LlmError> {
    let model = input.model;
    let spec = video_spec(model);
    let caps = Caps { quality: false, count: false, background: false, seconds: true };
    validate(model, req, &caps, "a video model")?;
    let MediaInput { prompt, answer_images, answer_video, own_images, own_videos, mut notes } =
        media::media_input(input.history, model, "xAI's video API")?;
    if own_videos.len() > 1 {
        return Err(LlmError::request(format!("`{model}` edits one video at a time ({} given)", own_videos.len())));
    }
    let source = own_videos.first().or(answer_video.as_ref());
    let images: Vec<_> = answer_images.iter().chain(&own_images).collect();
    let mode = match (source, images.len()) {
        (Some(_), 0) => Mode::Edit,
        (Some(_), _) => {
            return Err(LlmError::request(format!(
                "`{model}`: a video edit takes no images (xAI edits a video from a prompt)"
            )));
        }
        (None, 0) => Mode::Text,
        (None, 1) => Mode::Image,
        (None, k) => Mode::References(k),
    };
    match (mode, spec) {
        (Mode::Edit, Some(s)) if !s.edit => {
            return Err(LlmError::request(format!(
                "`{model}` does not edit videos (xAI's video editing is on grok-imagine-video only): a reply to a \
                 video edits it; start a new message for a new video"
            )));
        }
        (Mode::References(k), Some(s)) if k > s.references => {
            return Err(LlmError::request(if s.references == 0 {
                format!(
                    "`{model}` takes one image (the first frame), not {k}: reference images are on grok-imagine-video-1.5"
                )
            } else {
                format!("`{model}` takes at most {} reference images ({k} given)", s.references)
            }));
        }
        _ => {}
    }
    if prompt.is_empty() && matches!(mode, Mode::Text | Mode::Edit | Mode::References(_)) {
        return Err(LlmError::request(format!(
            "`{model}` needs a prompt (the text of the message) for a {}",
            mode.describe()
        )));
    }

    // Options by mode: what a mode cannot use is not sent (they carry over to replies), with a note.
    let mut unused = Vec::new();
    let duration = match (mode, req.seconds) {
        (Mode::Edit, Some(s)) => {
            unused.push(format!("sec={s}"));
            None
        }
        (_, Some(s)) => {
            let max = match (mode, spec) {
                (Mode::References(_), Some(v)) => v.reference_seconds,
                _ => 15,
            };
            if !(1..=max).contains(&s) {
                return Err(LlmError::request(format!(
                    "`{model}` makes {} of 1–{max} s (sec={s})",
                    if matches!(mode, Mode::References(_)) { "reference-to-video" } else { "videos" }
                )));
            }
            Some(s)
        }
        (_, None) => None,
    };
    let max_res = match (mode, spec) {
        (Mode::Edit, _) => None,
        (Mode::References(_), _) => Some("720p"),
        (_, Some(s)) => Some(s.max_resolution),
        (_, None) => None,
    };
    let resolution = match (mode, req.image_size.as_deref()) {
        (Mode::Edit, Some(s)) => {
            unused.push(format!("size {s}"));
            None
        }
        (_, None) => max_res,
        (_, Some("1K")) => Some("720p"),
        (_, Some("2K")) if max_res.is_none_or(|m| m == "1080p") => Some("1080p"),
        (_, Some(s)) => {
            let sizes = if max_res == Some("1080p") { "1K (720p), 2K (1080p)" } else { "1K (720p)" };
            return Err(LlmError::request(format!(
                "video size {s} is not available for `{model}` ({}); sizes: {sizes}",
                mode.describe()
            )));
        }
    };
    let ratio = match (mode, &req.aspect_ratio) {
        (Mode::Edit | Mode::Image, Some(r)) => {
            unused.push(format!("aspect ratio {r}"));
            None
        }
        (_, Some(r)) => {
            one_of(model, "aspect ratio", r, VIDEO_RATIOS)?;
            Some(r.clone())
        }
        (_, None) => None,
    };
    if !unused.is_empty() {
        notes.push(format!(
            "ℹ️ {} not sent: {}",
            unused.join(", "),
            if mode == Mode::Edit {
                "a video edit keeps the source's length, aspect ratio and resolution (at most 720p)"
            } else {
                "image-to-video takes the image's aspect ratio"
            }
        ));
    }

    let mut body = json!({"model": model});
    if !prompt.is_empty() {
        body["prompt"] = json!(prompt);
    }
    let mut applied = Vec::new();
    let mut settings = vec![mode.describe()];
    match mode {
        Mode::Edit => {
            let v = source.expect("an edit has a source video");
            if media::base_mime(&v.mime) != "video/mp4" {
                return Err(LlmError::request(format!("`{model}` edits MP4 videos only ({})", v.mime)));
            }
            body["video"] = json!({"url": media::data_url_checked(v, "video/mp4", None, model, "xAI", blobs).await?});
            if own_videos.is_empty() {
                settings[0] = "video edit of the previous video".into();
            }
        }
        Mode::Image => {
            body["image"] =
                json!({"url": media::image_data_url(images[0], INPUT_TYPES, None, model, "xAI", blobs).await?});
        }
        Mode::References(_) => {
            let mut refs = Vec::new();
            for b in &images {
                refs.push(json!({"url": media::image_data_url(b, INPUT_TYPES, None, model, "xAI", blobs).await?}));
            }
            body["reference_images"] = json!(refs);
        }
        Mode::Text => {}
    }
    if let Some(r) = resolution {
        body["resolution"] = json!(r);
        applied.push(Applied { source: src(user.image_size.is_some()), patch: json!({"resolution": r}) });
        settings.push(format!("resolution {r}"));
    }
    if let Some(d) = duration {
        body["duration"] = json!(d);
        applied.push(Applied { source: src(user.seconds.is_some()), patch: json!({"duration": d}) });
        settings.push(format!("{d} s"));
    }
    if let Some(r) = ratio {
        body["aspect_ratio"] = json!(r);
        applied.push(Applied { source: src(user.aspect_ratio.is_some()), patch: json!({"aspect_ratio": r}) });
        settings.push(format!("aspect ratio {r}"));
    }
    if let Some(u) = &input.user {
        body["user"] = json!(u);
    }
    let mut warnings = Vec::new();
    if spec.is_none() {
        warnings.push(format!(
            "`{model}` is not in the bot's xAI video model table: its limits and price are unknown (the bot's defaults are not sent)"
        ));
    }
    Ok(Prepared {
        dialect: VIDEO_DIALECT,
        model: model.to_string(),
        url: cfg.url(if mode == Mode::Edit { "/videos/edits" } else { "/videos/generations" }),
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

/// `{base}/videos` of a videos endpoint URL.
fn videos_base(url: &str) -> &str {
    match url.rfind("/videos/") {
        Some(i) => &url[..i + "/videos".len()],
        None => url.rsplit_once('/').map_or(url, |(b, _)| b),
    }
}

/// The request that resumes a video job from its checkpoint
/// (`{"request_id"}`): polling `GET {base}/videos/{request_id}` instead of
/// creating another video (no body: nothing is sent).
pub fn continuation_request(p: &Prepared, item: &Value) -> Prepared {
    let mut q = p.clone();
    if let Some(id) = item.get("request_id").and_then(Value::as_str) {
        q.url = format!("{}/{id}", videos_base(&p.url));
        q.body = Value::Null;
    }
    q
}

/// What the tool call shows of a video request (no media bytes).
fn video_input(body: &Value) -> Value {
    let mut o = Map::new();
    for k in ["prompt", "duration", "aspect_ratio", "resolution"] {
        if let Some(v) = body.get(k) {
            o.insert(k.into(), v.clone());
        }
    }
    if body.get("image").is_some() {
        o.insert("image".into(), json!("the first frame"));
    }
    if let Some(r) = body.get("reference_images").and_then(Value::as_array) {
        o.insert("reference_images".into(), json!(r.len()));
    }
    if body.get("video").is_some() {
        o.insert("video".into(), json!("the source video"));
    }
    Value::Object(o)
}

/// The tool call of a video job (`video_edit` for edits; updates keep the
/// name the call was shown with).
fn video_call(id: &str, status: &str, input: Option<Value>, output: Option<Value>) -> StreamEvent {
    video_call_named("video_generation", id, status, input, output)
}

fn video_call_named(name: &str, id: &str, status: &str, input: Option<Value>, output: Option<Value>) -> StreamEvent {
    StreamEvent::Tool(ToolCall {
        id: Some(id.to_string()),
        name: name.into(),
        status: Some(status.to_string()),
        input,
        output,
    })
}

/// The error of a failed (or expired) video job: by xAI's codes
/// (`invalid_argument`, `permission_denied`, `failed_precondition` never
/// succeed again; `service_unavailable`, `internal_error` may, with a new job).
fn job_error(status: &str, body: &Value) -> LlmError {
    let r = body.get("response").filter(|r| r.is_object()).unwrap_or(body);
    let mut e = match r.get("error").filter(|e| e.is_object()) {
        Some(err) => super::error_from_json(err, None, None, ErrorKind::InBand),
        None if status == "expired" => LlmError::new(ErrorKind::InBand, Retry::Backoff, "the video job expired"),
        None => LlmError::new(ErrorKind::InBand, Retry::Backoff, format!("the video job ended with status {status}")),
    };
    e.details = Some(body.clone());
    e
}

/// Run a video request: create the job (unless resuming one), checkpoint its
/// id, poll it with its progress, download the video.
pub(crate) async fn stream_video(
    client: &reqwest::Client,
    cfg: &DirectConfig,
    poll: Duration,
    p: &Prepared,
    sink: &mut dyn EventSink,
    observer: &dyn Observer,
    cancel: &CancellationToken,
) -> (Result<Value, LlmError>, CallOutcome) {
    let h = http(client, cfg, observer, cancel);
    let mut outcome = CallOutcome::default();
    let mut shown = String::new();
    let (id, poll_url) = if p.body.is_null() {
        // Resuming a job (see [`continuation_request`]).
        (p.url.rsplit('/').next().unwrap_or_default().to_string(), p.url.clone())
    } else {
        let reply = match h.post(&p.url, &p.body).await {
            Ok(r) => r,
            Err(e) => return (Err(e), outcome),
        };
        let Some(id) = reply.body.get("request_id").and_then(Value::as_str).map(str::to_string) else {
            let e = LlmError::new(ErrorKind::Decode, Retry::Backoff, "the video request returned no request_id")
                .with_details(reply.body);
            return (Err(e), outcome);
        };
        sink.event(StreamEvent::Meta { upstream_id: Some(id.clone()), provider: None, model: None });
        let name = if p.url.ends_with("/edits") { "video_edit" } else { "video_generation" };
        sink.event(video_call_named(name, &id, "pending", Some(video_input(&p.body)), None));
        shown = "pending".into();
        let item = json!({"request_id": id});
        sink.event(StreamEvent::Checkpoint(item.clone()));
        outcome.continuation = Some(item);
        let poll_url = format!("{}/{id}", videos_base(&p.url));
        (id, poll_url)
    };
    outcome.upstream_id = Some(id.clone());
    loop {
        let reply = match h.get(&poll_url).await {
            Ok(r) => r,
            Err(e) => return (Err(e), outcome),
        };
        let body = reply.body;
        // The job's fields, at the top level (REST reference) or under `response`.
        let r = body.get("response").filter(|r| r.is_object()).unwrap_or(&body).clone();
        let status =
            r.get("status").or_else(|| body.get("status")).and_then(Value::as_str).unwrap_or("pending").to_string();
        match status.as_str() {
            "done" => return (finish_video(&h, p, &id, &r, &body, sink).await, outcome),
            "failed" | "expired" => {
                // Final: not resumable from the checkpoint (a retry makes a new job).
                sink.event(StreamEvent::Checkpoint(Value::Null));
                outcome.continuation = None;
                return (Err(job_error(&status, &body)), outcome);
            }
            _ => {
                let now = match r.get("progress").and_then(Value::as_u64) {
                    Some(pct) => format!("{status} {pct}%"),
                    None => status.clone(),
                };
                if now != shown {
                    sink.event(video_call(&id, &now, None, None));
                    shown = now;
                }
                if let Err(e) = media::pause(poll, cancel).await {
                    return (Err(e), outcome);
                }
            }
        }
    }
}

/// A finished job: the video (or the moderation stop), its usage, the finish.
async fn finish_video(
    h: &Http<'_>,
    p: &Prepared,
    id: &str,
    r: &Value,
    body: &Value,
    sink: &mut dyn EventSink,
) -> Result<Value, LlmError> {
    let video = r.get("video").cloned().unwrap_or(Value::Null);
    let url = video.get("url").and_then(Value::as_str).filter(|u| !u.is_empty());
    let mut shown = video.as_object().cloned().unwrap_or_default();
    shown.remove("url");
    if let Some(m) = r.get("model") {
        shown.insert("model".into(), m.clone());
    }
    let seconds = video.get("duration").and_then(Value::as_f64);
    let estimate = video_spec(&p.model).zip(seconds).map(|(s, d)| s.per_second * d);
    let usage = media_usage(r.get("usage"), estimate);
    let Some(url) = url.filter(|_| video.get("respect_moderation").and_then(Value::as_bool) != Some(false)) else {
        // Filtered by moderation: no video (xAI: "the video url field will be empty").
        sink.event(video_call(id, "done", None, Some(Value::Object(shown))));
        sink.event(StreamEvent::Usage(usage));
        sink.event(StreamEvent::FinishDetails(json!({"respect_moderation": false, "video": video})));
        sink.event(StreamEvent::Finish {
            reason: Some("content_filter".into()),
            native: Some("respect_moderation: false".into()),
        });
        return Ok(body.clone());
    };
    let (data, mime) = h.download(url).await?;
    let mime = mime.filter(|m| m.starts_with("video/")).unwrap_or_else(|| "video/mp4".into());
    let name = match seconds {
        Some(d) => format!("{}-{d}s.mp4", p.model),
        None => format!("{}.mp4", p.model),
    };
    sink.event(video_call(id, "done", None, Some(Value::Object(shown))));
    sink.event(StreamEvent::File { data, mime, name: Some(name) });
    sink.event(StreamEvent::Usage(usage));
    sink.event(StreamEvent::Finish { reason: Some("stop".into()), native: Some("done".into()) });
    Ok(body.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::openai::media::tests::{Requests, serve_seq};
    use crate::openai::wire::tests::{MemBlobs, file, photo, user};
    use crate::provider::Provider;
    use omni_core::content::{Part, Role};
    use omni_core::trigger::OptionToken;
    use omni_core::{BlobRef, Turn};

    fn cfg(base: &str) -> DirectConfig {
        DirectConfig {
            api_key: "xai-test".into(),
            base_url: base.into(),
            connect_timeout: Duration::from_secs(5),
            idle_timeout: Duration::from_secs(5),
            max_tokens_cap: None,
            cny_per_usd: 7.1,
            long_decode_budget: 393_216,
        }
    }

    fn xai(base: &str) -> super::super::Xai {
        let mut x = super::super::Xai::new(cfg(base)).unwrap();
        x.poll = Duration::from_millis(5);
        x
    }

    fn flag(f: &str) -> OptionToken {
        OptionToken::Flag(f.into())
    }

    fn kv(k: &str, v: &str) -> OptionToken {
        OptionToken::KeyValue(k.into(), v.into())
    }

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
            user: Some("u-hash".into()),
            known_context: None,
            max_tokens_override: None,
        };
        xai("https://api.x.ai/v1").prepare(&input, &[], blobs).await
    }

    async fn prep(model: &str, opts: &[OptionToken]) -> Result<Prepared, LlmError> {
        prep_with(model, opts, &[Turn::user_text("a lighthouse")], &MemBlobs(vec![])).await
    }

    async fn err(model: &str, opts: &[OptionToken]) -> String {
        prep(model, opts).await.unwrap_err().message
    }

    fn generated(blob: &BlobRef) -> Part {
        Part::Image { blob: blob.clone(), filename: None, generated: true, dims: None, thought: false }
    }

    fn answer(parts: Vec<Part>) -> Turn {
        Turn { role: Role::Assistant, origin: Default::default(), parts, native: None }
    }

    #[tokio::test]
    async fn image_defaults_are_the_largest_and_highest() {
        let p = prep("grok-imagine-image-2.0", &[]).await.unwrap();
        assert_eq!((p.dialect, p.url.as_str()), (IMAGE_DIALECT, "https://api.x.ai/v1/images/generations"));
        assert_eq!(
            p.body,
            json!({"model": "grok-imagine-image-2.0", "prompt": "a lighthouse", "quality": "medium", "resolution": "2k",
                "response_format": "b64_json", "user": "u-hash"})
        );
        assert_eq!(p.settings, ["quality medium", "resolution 2k"]);
        assert!(p.warnings.is_empty() && p.notes.is_empty());
        // 1.0 has no quality.
        let p = prep("grok-imagine-image", &[]).await.unwrap();
        assert!(p.body.get("quality").is_none());
        assert_eq!(p.body["resolution"], "2k");
        // Options.
        let p = prep("grok-imagine-image-2.0", &[flag("1W"), kv("q", "low"), kv("n", "4"), kv("bg", "transparent")])
            .await
            .unwrap();
        let b = &p.body;
        assert_eq!(
            (b["resolution"].clone(), b["aspect_ratio"].clone(), b["quality"].clone()),
            (json!("1k"), json!("16:9"), json!("low"))
        );
        assert_eq!((b["n"].clone(), b["background"].clone()), (json!(4), json!("transparent")));
        assert_eq!(
            p.settings,
            ["quality low", "resolution 1k", "aspect ratio 16:9", "background transparent", "4 images"]
        );
        assert_eq!(prep("grok-imagine-image-2.0", &[kv("ar", "21:9")]).await.unwrap().body["aspect_ratio"], "21:9");
        // Retired slugs and unknown ids: their price is unknown to the bot, so they are not supported.
        for id in ["grok-imagine-image-quality", "grok-imagine-image-pro", "grok-imagine-image-3.0"] {
            assert_eq!(
                err(id, &[]).await,
                format!("`{id}` is not in the bot's xAI model table (its price is unknown), so it is not supported")
            );
        }
    }

    #[tokio::test]
    async fn what_the_image_api_does_not_take_is_an_error() {
        assert!(err("grok-imagine-image-2.0", &[flag("4")]).await.contains("sizes: 1K, 2K"));
        assert!(err("grok-imagine-image-2.0", &[kv("ar", "5:4")]).await.contains("ratios: 1:1, 3:4"));
        assert!(err("grok-imagine-image-2.0", &[kv("q", "max")]).await.contains("levels: low, medium, auto"));
        // `high` is in the API's enum, but the model rejects it (live): refused before sending.
        assert!(err("grok-imagine-image-2.0", &[kv("q", "high")]).await.contains("levels: low, medium, auto"));
        assert!(
            err("grok-imagine-image", &[kv("q", "low")])
                .await
                .contains("does not take an image quality (`q=`) via xAI")
        );
        assert!(err("grok-imagine-image-2.0", &[kv("n", "11")]).await.contains("at most 10 images"));
        assert!(
            err("grok-imagine-image-2.0", &[kv("bg", "red")]).await.contains("backgrounds: auto, opaque, transparent")
        );
        assert!(err("grok-imagine-image-2.0", &[kv("e", "high")]).await.contains("no reasoning effort levels"));
        assert!(err("grok-imagine-image-2.0", &[kv("sec", "5")]).await.contains("(`sec=`) via xAI"));
        assert!(err("grok-imagine-image-2.0", &[kv("tier", "priority")]).await.contains("no service tiers"));
        assert!(err("grok-imagine-image-2.0", &[flag("s")]).await.contains("has no web search"));
        assert!(err("grok-imagine-image-2.0", &[flag("i")]).await.contains("makes them by itself"));
    }

    #[tokio::test]
    async fn image_edits_take_the_previous_image_and_the_message_s() {
        let (img, b1) = photo("image/png", b"\x89PNG-own");
        let (img2, b2) = photo("image/jpeg", b"\xff\xd8\xff-own2");
        let prev = BlobRef::for_bytes(b"\xff\xd8\xff-prev", "image/jpeg");
        let blobs = MemBlobs(vec![b1, b2, (prev.clone(), b"\xff\xd8\xff-prev".to_vec())]);
        // One image: `image`.
        let p =
            prep_with("grok-imagine-image-2.0", &[], &[user(vec![Part::text("pencil sketch"), img.clone()])], &blobs)
                .await
                .unwrap();
        assert_eq!(p.url, "https://api.x.ai/v1/images/edits");
        assert!(p.body["image"]["url"].as_str().unwrap().starts_with("data:image/png;base64,"));
        assert!(p.body.get("images").is_none());
        assert!(p.settings.contains(&"edit (1 source image)".to_string()));
        // A reply to a generated image with two more: `images`, the previous first.
        let history = vec![
            Turn::user_text("a lighthouse"),
            answer(vec![generated(&prev)]),
            user(vec![Part::text("put <IMAGE_1> and <IMAGE_2> in it"), img, img2]),
        ];
        let p = prep_with("grok-imagine-image-2.0", &[], &history, &blobs).await.unwrap();
        let images = p.body["images"].as_array().unwrap();
        assert_eq!(images.len(), 3);
        assert!(images[0]["url"].as_str().unwrap().starts_with("data:image/jpeg;base64,"));
        assert!(p.settings.contains(&"edit of the previous image + 2 source images".to_string()));
        // Six images: too many.
        let six: Vec<Part> =
            std::iter::once(Part::text("x")).chain((0..6).map(|_| photo("image/png", b"\x89PNG").0)).collect();
        let e = prep_with("grok-imagine-image-2.0", &[], &[user(six)], &MemBlobs(vec![])).await.unwrap_err();
        assert!(e.message.contains("at most 5 source images"), "{}", e.message);
    }

    #[tokio::test]
    async fn images_come_back_with_the_billed_cost() {
        let p = prep("grok-imagine-image-2.0", &[kv("n", "2")]).await.unwrap();
        let body = json!({"data": [{"b64_json": crate::provider::b64(b"\xff\xd8\xff-1"), "mime_type": "image/jpeg"},
                                   {"b64_json": crate::provider::b64(b"\x89PNG-2"), "mime_type": "image/png"}],
            "usage": {"cost_in_usd_ticks": 1_400_000_000u64, "input_tokens": 20, "input_tokens_details": {"cached_tokens": 0, "image_tokens": 0, "text_tokens": 20},
                "output_tokens": 300, "output_tokens_details": {"image_tokens": 0, "reasoning_tokens": 200, "text_tokens": 100}, "total_tokens": 320}});
        let (base, got) = serve_seq(vec![(200, "application/json", body.to_string().into_bytes())]).await;
        let x = xai(&base);
        let p = Prepared { url: format!("{base}/images/generations"), ..p };
        let mut evs = Vec::new();
        let mut sink = |e: StreamEvent| evs.push(e);
        let (r, outcome) = x.stream(&p, &mut sink, &crate::NullObserver, &CancellationToken::new()).await;
        assert!(r.unwrap()["data"][0]["b64_json"].as_str().unwrap().contains("stored as"));
        assert_eq!(outcome.upstream_id.as_deref(), Some("req_1"));
        let reqs = got.lock().unwrap().clone();
        assert_eq!(
            (reqs[0].0.as_str(), reqs[0].1.as_deref()),
            ("POST /v1/images/generations", Some("Bearer xai-test"))
        );
        assert_eq!(reqs[0].2, p.body);
        let images: Vec<&str> = evs
            .iter()
            .filter_map(|e| if let StreamEvent::Image { mime, .. } = e { Some(mime.as_str()) } else { None })
            .collect();
        assert_eq!(images, ["image/jpeg", "image/png"]);
        let u = evs.iter().find_map(|e| if let StreamEvent::Usage(u) = e { Some(u.clone()) } else { None }).unwrap();
        assert_eq!((u.cost_usd, u.cost_source, u.reasoning_tokens), (Some(0.14), CostSource::Reported, Some(200)));
        // Without a billed cost: the list price (2k at low, a lower bound).
        let u = media_usage(None, image_estimate(&p, 2));
        assert_eq!((u.cost_usd, u.cost_source), (Some(0.12), CostSource::Estimated));
    }

    #[tokio::test]
    async fn video_modes_and_their_defaults() {
        // Text-to-video: the largest resolution, xAI's default length.
        let p = prep("grok-imagine-video-1.5", &[]).await.unwrap();
        assert_eq!((p.dialect, p.url.as_str()), (VIDEO_DIALECT, "https://api.x.ai/v1/videos/generations"));
        assert_eq!(
            p.body,
            json!({"model": "grok-imagine-video-1.5", "prompt": "a lighthouse", "resolution": "1080p", "user": "u-hash"})
        );
        assert_eq!(p.settings, ["text-to-video", "resolution 1080p"]);
        assert_eq!(prep("grok-imagine-video", &[]).await.unwrap().body["resolution"], "720p");
        let p = prep("grok-imagine-video-1.5-lite", &[kv("sec", "12"), flag("1H")]).await.unwrap();
        assert_eq!(
            (p.body["duration"].clone(), p.body["resolution"].clone(), p.body["aspect_ratio"].clone()),
            (json!(12), json!("720p"), json!("9:16"))
        );
        assert_eq!(p.settings, ["text-to-video", "resolution 720p", "12 s", "aspect ratio 9:16"]);
        // Image-to-video: the image is the first frame; a ratio is not sent (noted).
        let (img, b1) = photo("image/png", b"\x89PNG-frame");
        let blobs = MemBlobs(vec![b1.clone()]);
        let p = prep_with(
            "grok-imagine-video-1.5",
            &[flag("W")],
            &[user(vec![Part::text("animate"), img.clone()])],
            &blobs,
        )
        .await
        .unwrap();
        assert!(p.body["image"]["url"].as_str().unwrap().starts_with("data:image/png;base64,"));
        assert!(p.body.get("aspect_ratio").is_none());
        assert!(
            p.notes[0].contains("aspect ratio 16:9 not sent: image-to-video takes the image's aspect ratio"),
            "{:?}",
            p.notes
        );
        // …also from a generated image replied to; the prompt is optional then.
        let prev = BlobRef::for_bytes(b"\x89PNG-gen", "image/png");
        let blobs2 = MemBlobs(vec![(prev.clone(), b"\x89PNG-gen".to_vec())]);
        let history = vec![Turn::user_text("a cat"), answer(vec![generated(&prev)]), user(vec![])];
        let p = prep_with("grok-imagine-video-1.5", &[], &history, &blobs2).await.unwrap();
        assert!(p.body.get("prompt").is_none() && p.body.get("image").is_some());
        // Reference-to-video: 720p at most, grok-imagine-video up to 10 s.
        let (img2, b2) = photo("image/jpeg", b"\xff\xd8\xff-ref");
        let blobs3 = MemBlobs(vec![b1, b2]);
        let refs = vec![user(vec![Part::text("<IMAGE_0> meets <IMAGE_1>"), img.clone(), img2.clone()])];
        let p = prep_with("grok-imagine-video-1.5", &[kv("sec", "15")], &refs, &blobs3).await.unwrap();
        assert_eq!(p.body["reference_images"].as_array().unwrap().len(), 2);
        assert_eq!((p.body["resolution"].clone(), p.body["duration"].clone()), (json!("720p"), json!(15)));
        assert_eq!(p.settings[0], "reference-to-video (2 images)");
        let e = prep_with("grok-imagine-video", &[kv("sec", "15")], &refs, &blobs3).await.unwrap_err();
        assert!(e.message.contains("reference-to-video of 1–10 s"), "{}", e.message);
        let e = prep_with("grok-imagine-video-1.5", &[flag("2")], &refs, &blobs3).await.unwrap_err();
        assert!(e.message.contains("sizes: 1K (720p)"), "{}", e.message);
        let e = prep_with("grok-imagine-video-1.5-lite", &[], &refs, &blobs3).await.unwrap_err();
        assert!(e.message.contains("takes one image (the first frame), not 2"), "{}", e.message);
    }

    #[tokio::test]
    async fn a_reply_to_a_video_edits_it_where_xai_can() {
        let clip = BlobRef::for_bytes(b"\0\0\0\x18ftypmp42", "video/mp4");
        let blobs = MemBlobs(vec![(clip.clone(), b"\0\0\0\x18ftypmp42".to_vec())]);
        let video = Part::File { blob: clip, filename: Some("v.mp4".into()), generated: true };
        let history =
            vec![Turn::user_text("a dancer"), answer(vec![video]), Turn::user_text("give her a silver necklace")];
        let p = prep_with("grok-imagine-video", &[kv("sec", "6"), flag("W")], &history, &blobs).await.unwrap();
        assert_eq!(p.url, "https://api.x.ai/v1/videos/edits");
        assert!(p.body["video"]["url"].as_str().unwrap().starts_with("data:video/mp4;base64,"));
        assert!(
            p.body.get("duration").is_none()
                && p.body.get("aspect_ratio").is_none()
                && p.body.get("resolution").is_none()
        );
        assert_eq!(p.settings, ["video edit of the previous video"]);
        assert!(
            p.notes[0].contains("sec=6, aspect ratio 16:9 not sent: a video edit keeps the source's"),
            "{:?}",
            p.notes
        );
        // The 1.5 models do not edit.
        let e = prep_with("grok-imagine-video-1.5", &[], &history, &blobs).await.unwrap_err();
        assert!(e.message.contains("does not edit videos"), "{}", e.message);
        // An edit takes no images; video models take no image options.
        let (img, b) = photo("image/png", b"\x89PNG");
        let h2 = vec![Turn::user_text("a"), history[1].clone(), user(vec![Part::text("x"), img])];
        let e = prep_with("grok-imagine-video", &[], &h2, &MemBlobs(vec![b])).await.unwrap_err();
        assert!(e.message.contains("a video edit takes no images"), "{}", e.message);
        assert!(err("grok-imagine-video-1.5", &[kv("q", "high")]).await.contains("does not take an image quality"));
        assert!(err("grok-imagine-video-1.5", &[kv("n", "2")]).await.contains("(`n=`) via xAI"));
        assert!(err("grok-imagine-video-1.5", &[kv("sec", "16")]).await.contains("videos of 1–15 s"));
        assert!(err("grok-imagine-video-1.5", &[flag("4")]).await.contains("sizes: 1K (720p), 2K (1080p)"));
        assert!(err("grok-imagine-video", &[flag("2")]).await.contains("sizes: 1K (720p)"));
        assert!(err("grok-imagine-video-1.5", &[kv("ar", "2:1")]).await.contains("ratios: 1:1, 16:9"));
        // An attached non-MP4 video cannot be edited.
        let (mov, bm) = file("a.mov", "video/quicktime", b"moov");
        let e = prep_with("grok-imagine-video", &[], &[user(vec![Part::text("x"), mov])], &MemBlobs(vec![bm]))
            .await
            .unwrap_err();
        assert!(e.message.contains("MP4 videos only"), "{}", e.message);
    }

    fn mp4() -> Vec<u8> {
        b"\0\0\0\x18ftypmp42-video-bytes".to_vec()
    }

    /// A server for a whole video job: create, polls, and the download at
    /// `/files/video.mp4` of the same server.
    async fn video_server(polls: Vec<Value>) -> (String, Requests) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base = format!("http://{addr}/v1");
        let got: Requests = Default::default();
        let log = got.clone();
        let download = format!("http://{addr}/files/video.mp4");
        tokio::spawn(async move {
            let mut polls = polls.into_iter();
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
                                l.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|v| v.trim().parse().unwrap())
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
                let auth = head.lines().find_map(|l| {
                    let (k, v) = l.split_once(':')?;
                    k.eq_ignore_ascii_case("authorization").then(|| v.trim().to_string())
                });
                let body_start = buf.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4).unwrap_or(buf.len());
                log.lock().unwrap().push((
                    line.clone(),
                    auth,
                    serde_json::from_slice(&buf[body_start..]).unwrap_or(Value::Null),
                ));
                let (ctype, body): (&str, Vec<u8>) = if line.starts_with("POST") {
                    ("application/json", json!({"request_id": "vid-1"}).to_string().into_bytes())
                } else if line.contains("/files/") {
                    ("video/mp4", mp4())
                } else {
                    let mut p = polls.next().unwrap_or(json!({"status": "pending"}));
                    if let Some(v) = p.pointer_mut("/video/url").filter(|u| u.as_str() == Some("DOWNLOAD")) {
                        *v = json!(download);
                    }
                    ("application/json", p.to_string().into_bytes())
                };
                let head = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: {ctype}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                let _ = sock.write_all(head.as_bytes()).await;
                let _ = sock.write_all(&body).await;
            }
        });
        (base, got)
    }

    fn done_poll() -> Value {
        json!({"status": "done", "progress": 100, "model": "grok-imagine-video-1.5",
            "video": {"url": "DOWNLOAD", "duration": 8, "respect_moderation": true},
            "usage": {"cost_in_usd_ticks": 6_400_000_000u64}})
    }

    async fn run_video(base: &str, p: &Prepared) -> (Result<Value, LlmError>, CallOutcome, Vec<StreamEvent>) {
        let x = xai(base);
        let mut evs = Vec::new();
        let mut sink = |e: StreamEvent| evs.push(e);
        let (r, outcome) = x.stream(p, &mut sink, &crate::NullObserver, &CancellationToken::new()).await;
        (r, outcome, evs)
    }

    #[tokio::test]
    async fn a_video_is_downloaded_as_a_file_with_its_billed_cost() {
        let p = prep("grok-imagine-video-1.5", &[kv("sec", "8")]).await.unwrap();
        let (base, got) = video_server(vec![json!({"status": "pending", "progress": 40}), done_poll()]).await;
        let p = Prepared { url: format!("{base}/videos/generations"), ..p };
        let (r, outcome, evs) = run_video(&base, &p).await;
        let native = r.unwrap();
        assert_eq!(native["video"]["duration"], 8);
        assert_eq!(outcome.upstream_id.as_deref(), Some("vid-1"));
        let reqs = got.lock().unwrap().clone();
        let lines: Vec<&str> = reqs.iter().map(|r| r.0.as_str()).collect();
        assert_eq!(
            lines,
            ["POST /v1/videos/generations", "GET /v1/videos/vid-1", "GET /v1/videos/vid-1", "GET /files/video.mp4"]
        );
        assert_eq!(reqs[0].2, p.body);
        assert_eq!(reqs[3].1, None, "no credentials are sent to the download URL");
        let kinds: Vec<String> = evs
            .iter()
            .map(|e| match e {
                StreamEvent::Meta { .. } => "meta".into(),
                StreamEvent::Tool(t) => format!("tool {}", t.status.clone().unwrap_or_default()),
                StreamEvent::Checkpoint(_) => "checkpoint".into(),
                StreamEvent::File { mime, name, data } => {
                    format!("file {mime} {} {}", name.clone().unwrap_or_default(), data.len())
                }
                StreamEvent::Usage(u) => format!("usage {:?}", u.cost_usd),
                StreamEvent::Finish { reason, .. } => format!("finish {}", reason.clone().unwrap_or_default()),
                other => format!("{other:?}"),
            })
            .collect();
        assert_eq!(
            kinds,
            [
                "meta",
                "tool pending",
                "checkpoint",
                "tool pending 40%",
                "tool done",
                &format!("file video/mp4 grok-imagine-video-1.5-8s.mp4 {}", mp4().len()),
                "usage Some(0.64)",
                "finish stop"
            ]
        );
        let mut acc = crate::Accumulator::new();
        evs.into_iter().for_each(|e| acc.apply(e));
        let Part::ToolCall(t) = &acc.parts()[0] else { panic!("{:?}", acc.parts()) };
        assert_eq!(t.name, "video_generation");
        assert_eq!(t.input, Some(json!({"prompt": "a lighthouse", "duration": 8, "resolution": "1080p"})));
        assert_eq!(
            t.output,
            Some(json!({"duration": 8, "respect_moderation": true, "model": "grok-imagine-video-1.5"}))
        );
    }

    #[tokio::test]
    async fn a_restart_resumes_polling_the_same_job() {
        let p = prep("grok-imagine-video-1.5", &[]).await.unwrap();
        let (base, got) = video_server(vec![done_poll()]).await;
        let p = Prepared { url: format!("{base}/videos/generations"), ..p };
        // What the bot builds from the checkpoint (`Prepared::with_continuation`, dispatched by dialect).
        let resumed = p.with_continuation(&json!({"request_id": "vid-1"}));
        assert_eq!(
            (resumed.url.as_str(), resumed.body.clone()),
            (format!("{base}/videos/vid-1").as_str(), Value::Null)
        );
        let (r, outcome, evs) = run_video(&base, &resumed).await;
        assert!(r.is_ok());
        assert_eq!(outcome.upstream_id.as_deref(), Some("vid-1"));
        let lines: Vec<String> = got.lock().unwrap().iter().map(|r| r.0.clone()).collect();
        assert_eq!(lines, ["GET /v1/videos/vid-1", "GET /files/video.mp4"], "no new job");
        assert!(evs.iter().any(|e| matches!(e, StreamEvent::File { .. })));
        assert!(!evs.iter().any(|e| matches!(e, StreamEvent::Checkpoint(_))));
    }

    #[tokio::test]
    async fn failed_and_filtered_jobs() {
        let p = prep("grok-imagine-video-1.5", &[]).await.unwrap();
        // Failed: by xAI's code (moderation blocks are invalid_argument: never retried).
        let failed = json!({"status": "failed", "error": {"code": "invalid_argument", "message": "Content blocked by moderation."}});
        let (base, _) = video_server(vec![failed.clone()]).await;
        let p1 = Prepared { url: format!("{base}/videos/generations"), ..p.clone() };
        let e = run_video(&base, &p1).await.0.unwrap_err();
        assert_eq!(
            (e.kind, e.retry, e.error_type.as_deref()),
            (ErrorKind::InBand, Retry::Never, Some("invalid_argument"))
        );
        assert_eq!(e.message, "Content blocked by moderation.");
        assert_eq!(e.details, Some(failed));
        let busy = json!({"status": "failed", "error": {"code": "service_unavailable", "message": "overloaded"}});
        let (base, _) = video_server(vec![busy]).await;
        let p2 = Prepared { url: format!("{base}/videos/generations"), ..p.clone() };
        let (r, outcome, evs) = run_video(&base, &p2).await;
        // Retried with a new job, never resumed: the checkpoint is cleared before the error.
        assert_eq!(r.unwrap_err().retry, Retry::Backoff);
        let checkpoints: Vec<&Value> =
            evs.iter().filter_map(|e| if let StreamEvent::Checkpoint(c) = e { Some(c) } else { None }).collect();
        assert_eq!(checkpoints, [&json!({"request_id": "vid-1"}), &Value::Null]);
        assert!(outcome.continuation.is_none());
        // Also when the job was resumed (after a restart) and had failed meanwhile, or expired.
        let (base, _) = video_server(vec![json!({"status": "expired"})]).await;
        let resumed = Prepared { url: format!("{base}/videos/generations"), ..p.clone() }
            .with_continuation(&json!({"request_id": "vid-1"}));
        let (r, _, evs) = run_video(&base, &resumed).await;
        assert_eq!(r.unwrap_err().message, "the video job expired");
        assert!(matches!(evs.last(), Some(StreamEvent::Checkpoint(Value::Null))));
        // Done, but filtered by moderation: a content-filter stop, billed.
        let filtered = json!({"status": "done", "video": {"url": "", "duration": 8, "respect_moderation": false},
            "usage": {"cost_in_usd_ticks": 6_400_000_000u64}});
        let (base, got) = video_server(vec![filtered]).await;
        let p3 = Prepared { url: format!("{base}/videos/generations"), ..p };
        let (r, _, evs) = run_video(&base, &p3).await;
        assert!(r.is_ok());
        assert!(matches!(evs.last(), Some(StreamEvent::Finish { reason: Some(r), .. }) if r == "content_filter"));
        assert!(evs.iter().any(|e| matches!(e, StreamEvent::FinishDetails(d) if d["respect_moderation"] == false)));
        assert!(evs.iter().any(|e| matches!(e, StreamEvent::Usage(u) if u.cost_usd == Some(0.64))));
        assert_eq!(got.lock().unwrap().len(), 2, "nothing to download");
    }

    #[test]
    fn continuation_urls() {
        assert_eq!(videos_base("https://api.x.ai/v1/videos/generations"), "https://api.x.ai/v1/videos");
        assert_eq!(videos_base("https://api.x.ai/v1/videos/edits"), "https://api.x.ai/v1/videos");
        let p = Prepared {
            dialect: VIDEO_DIALECT,
            model: "m".into(),
            url: "https://api.x.ai/v1/videos/edits".into(),
            body: json!({"prompt": "x"}),
            applied: vec![],
            warnings: vec![],
            notes: vec![],
            show_debug: false,
            auto_max_tokens: None,
            long_decode: false,
            settings: vec![],
        };
        let q = continuation_request(&p, &json!({"request_id": "abc"}));
        assert_eq!((q.url.as_str(), q.body.is_null()), ("https://api.x.ai/v1/videos/abc", true));
        // Not a video checkpoint: unchanged.
        assert_eq!(continuation_request(&p, &json!({"x": 1})).url, p.url);
        // No output budget anywhere in these requests.
        assert_eq!(q.max_tokens(), None);
    }
}
