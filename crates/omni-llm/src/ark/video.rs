//! Seedance video generation on Volcengine Ark (`ark:doubao-seedance-…`): the
//! Video Generation API, `POST {base}/contents/generations/tasks` to create a
//! task and `GET {base}/contents/generations/tasks/{id}` to poll it (Ark has no
//! chat route for these models; a bare Seedance id is this API). Spec: Ark's
//! docs as fetched 2026-11-13 (`docs.volcengine.com/docs/zh/ark/…`:
//! `create-video-generation-task-api`, `get-video-generation-task-api`,
//! `video-generation-tutorial`, `seedance-2-5`, `seedance-2-0`,
//! `seedance-portrait-asset-guide`, `seedance-model-activation-usage-and-refund`,
//! `model-list` §视频生成能力, `model-pricing` §视频生成模型,
//! `model-deprecation-notice` and `…-announcement-2025`, `error-codes`).
//!
//! **Models** ([`MODELS`]): Seedance 2.5 (`doubao-seedance-2-5-260628`, Ark's
//! first recommendation): 4–30 s, 480p / 720p / 1080p (10-bit HEVC), up to 30
//! reference images, 10 videos and 10 audio clips (audio alone too), draft
//! mode. Seedance 2.0 (`…-2-0-260128`, also 4K, 10-bit HEVC), 2.0 fast and 2.0
//! mini (480p / 720p): 4–15 s, up to 9 images, 3 videos and 3 audio clips (audio
//! only with an image or a video). Models with an end of service (Seedance 1.0
//! pro and pro fast) and retired ones are not in the table (owner's rule), nor
//! is Seedance 2.5 flash until Ark publishes its dated id (2026-11-15).
//! Ark's activation rule (the console's 开通管理; Seedance 2.x needs a ¥200
//! balance, savings plan or resource pack) is added to its `ModelNotOpen`
//! error, and a full task queue (`QuotaExceeded` "has exceeded the quota") is
//! retried later.
//!
//! **Modes, from the inputs** (the conversation rule of [`super::images`]: the
//! new message's text and media; a reply to a generated answer uses what it
//! made, first):
//! - text only: text-to-video;
//! - images: one is the first frame, two the first and last frames, three or
//!   more (or a prompt that names them, "图片1", "@image 1", the reference
//!   syntax Seedance asks for) reference images;
//! - audio (WAV or MP3, base64) or a video: full-modal reference (all images are
//!   references). Ark takes input videos only as URLs, and the bot uploads
//!   nothing: the video of a Seedance answer replied to within its URL's 24
//!   hours is passed by that URL (Ark trusts its own outputs of the same
//!   account, faces included); Seedance decides from the prompt whether that is
//!   a new video, an edit or an extension (`omni_reference_task_type: auto`).
//!   An attached video, or an older or foreign one, is an error.
//! - `q=draft` (2.5): a 480p draft (`draft: true`); `q=final` on a reply to a
//!   draft: the final 1080p video from it (`draft_task`), reusing the draft's
//!   prompt, inputs, length, ratio, seed and audio (the message's text is not
//!   sent).
//!
//! **Defaults** (D79, the owner): Ark's default resolution, 720p, sent
//! explicitly (the highest: 2.5 1080p, 2.0 4K, fast / mini 720p; the reply
//! notes it and the option that raises it), audio on (`generate_audio: true`,
//! Ark's default; `audio=off` turns it off), `watermark: false`, Ark's own
//! length (2.5: `-1`, the model picks) and aspect ratio (`adaptive` on 2.x),
//! `safety_identifier` = the hashed user. Ark documents no moderation switch.
//!
//! **Options**: `size=1K` `2K` `4K` = 720p / 1080p / 4K (480p has no size);
//! `ar=` = 16:9, 4:3, 1:1, 3:4, 9:16, 21:9; `sec=`; `audio=`; `q=draft|final` (2.5);
//! no `+s` (Ark publishes no price for the video API's web search). Options a mode cannot use (a conversation's
//! options carry over to replies) are not sent, with a note: 2.5 keeps the first
//! frame's or the source video's aspect ratio, and an edit the source's length.
//! Not used (no option): seed, fixed camera, frame counts, the last-frame
//! image, `mov` output, queue priority, the task type hint
//! (`omni_reference_task_type`), Ark's asset library, callbacks.
//!
//! **Cost**: Ark bills `usage.completion_tokens` ("可作为计费对账依据"; about
//! (input video + output) seconds × width × height × 24 / 1024) at the CNY
//! price per million tokens of the model, the output resolution and whether
//! the input has a video (a draft's final: whether the draft had one), halved
//! for flex. Failed tasks are not billed. The video API's web search has no
//! listed fee (not estimated; warned).
//!
//! **Tasks**: the task id is checkpointed when created ([`continuation_request`]
//! polls it again after a restart or a dropped poll; ids live 7 days) and
//! polled every 5 s, its status on a `video_generation` tool call; the MP4 is
//! downloaded (no credentials) as a `File`. `failed` (by Ark's error code, e.g.
//! moderation: never retried), `expired` and `cancelled` clear the checkpoint
//! first: a retry makes a new task.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use omni_core::Usage;
use omni_core::content::{Part, Role, ToolCall, Turn};
use omni_core::usage::CostSource;
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use super::{ArkConfig, NO_SEARCH, error_from_json};
use crate::error::{ErrorKind, LlmError, Retry};
use crate::event::StreamEvent;
use crate::openai::media::{self, Http};
use crate::openrouter::options::{Applied, overlay};
use crate::request::{self, Generation, Raise, Requested, RouteCaps, Tool};
use crate::{BlobLoader, CallInput, CallOutcome, EventSink, MediaClass, Observer, Prepared, classify_mime};

pub const DIALECT: &str = "ark.videos";
const NAME: &str = "Volcengine Ark";
const PATH: &str = "/contents/generations/tasks";
/// Between two polls of a task (Seedance takes minutes; the poll API allows 20 QPS).
const POLL_EVERY: Duration = if cfg!(test) { Duration::from_millis(5) } else { Duration::from_secs(5) };
/// "长度不超过 64 个字符".
const SAFETY_ID_MAX: usize = 64;
/// Generated video URLs: "有效期为 24 小时".
const URL_VALID_SECS: i64 = 24 * 3600;
/// Draft task ids: 14 days (created since 2026-10-28 10:00 UTC+8).
const DRAFT_VALID_SECS: i64 = 14 * 86_400;
/// "请求体大小不超过 64 MB".
const MAX_BODY: usize = 64 * 1024 * 1024;
/// "单张图片小于 30 MB".
const MAX_IMAGE: u64 = 30 * 1024 * 1024;
/// "单个音频不超过 15 MB".
const MAX_AUDIO: u64 = 15 * 1024 * 1024;
/// `ratio` values besides `adaptive` (Ark's default on 2.x).
const RATIOS: &[&str] = &["16:9", "4:3", "1:1", "3:4", "9:16", "21:9"];
/// The settings line of a draft's final; with an input video in the draft it
/// is priced at the with-video rate.
const FINAL: &str = "final video of the draft";
const FINAL_WITH_VIDEO: &str = "final video of the draft (the draft had an input video)";

/// Whether `model` is a Seedance video model (or a retired Ark video model).
pub fn is_video_model(model: &str) -> bool {
    model.starts_with("doubao-seedance-") || model.starts_with("doubao-seaweed") || model.starts_with("wan2-1-14b-")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    V25,
    V20,
    V10,
}

/// Full-modal reference limits.
#[derive(Debug, Clone, Copy)]
pub struct Refs {
    pub images: usize,
    pub videos: usize,
    pub audios: usize,
    /// Audio without an image or a video.
    pub audio_alone: bool,
}

/// A Seedance model: what its API takes and what it costs.
#[derive(Debug, Clone, Copy)]
pub struct Spec {
    pub id: &'static str,
    /// Matches every id starting with `id` (a family without a published dated id).
    pub prefix: bool,
    pub name: &'static str,
    pub family: Family,
    /// `resolution` values, lowest first (the last is the highest; Ark's
    /// default is 720p).
    pub resolutions: &'static [&'static str],
    /// `duration` range, seconds.
    pub seconds: (u32, u32),
    /// Generates audio (`generate_audio`).
    pub audio: bool,
    /// First and last frames.
    pub first_last: bool,
    /// Full-modal references (and editing / extending videos).
    pub refs: Option<Refs>,
    /// Draft mode.
    pub draft: bool,
    /// The `web_search` tool.
    pub web_search: bool,
    /// `service_tier: flex`.
    pub flex: bool,
    /// CNY per 1M tokens: (resolution, input without video, input with video).
    pub price: &'static [(&'static str, f64, f64)],
}

/// CNY per 1M tokens, online inference (`model-pricing` §视频生成模型, "按token单价",
/// 2026-11-13): (output resolution, input without video, input with video).
const P25: &[(&str, f64, f64)] = &[("480p", 70.0, 42.0), ("720p", 70.0, 42.0), ("1080p", 77.0, 46.0)];
const P20: &[(&str, f64, f64)] =
    &[("480p", 46.0, 28.0), ("720p", 46.0, 28.0), ("1080p", 51.0, 31.0), ("4k", 26.0, 16.0)];
const P20_FAST: &[(&str, f64, f64)] = &[("480p", 37.0, 22.0), ("720p", 37.0, 22.0)];
const P20_MINI: &[(&str, f64, f64)] = &[("480p", 23.0, 14.0), ("720p", 23.0, 14.0)];
const SEEDANCE_20: Spec = Spec {
    id: "",
    prefix: false,
    name: "",
    family: Family::V20,
    resolutions: &["480p", "720p"],
    seconds: (4, 15),
    audio: true,
    first_last: true,
    refs: Some(Refs { images: 9, videos: 3, audios: 3, audio_alone: false }),
    draft: false,
    // Ark publishes no price for the video API's web search: not offered (owner's rule).
    web_search: false,
    flex: false,
    price: P20_FAST,
};

const REFS_25: Refs = Refs { images: 30, videos: 10, audios: 10, audio_alone: true };

/// Ark's video models (2026-11-13).
pub const MODELS: &[Spec] = &[
    Spec {
        id: "doubao-seedance-2-5-260628",
        name: "Seedance 2.5",
        family: Family::V25,
        resolutions: &["480p", "720p", "1080p"],
        seconds: (4, 30),
        refs: Some(REFS_25),
        draft: true,
        price: P25,
        ..SEEDANCE_20
    },
    Spec {
        id: "doubao-seedance-2-0-260128",
        name: "Seedance 2.0",
        resolutions: &["480p", "720p", "1080p", "4k"],
        price: P20,
        ..SEEDANCE_20
    },
    Spec { id: "doubao-seedance-2-0-fast-260128", name: "Seedance 2.0 fast", ..SEEDANCE_20 },
    Spec { id: "doubao-seedance-2-0-mini-260615", name: "Seedance 2.0 mini", price: P20_MINI, ..SEEDANCE_20 },
];

pub fn spec(model: &str) -> Option<&'static Spec> {
    MODELS.iter().find(|s| s.id == model || (s.prefix && model.starts_with(s.id)))
}

/// What Ark's video API takes for a model (the vendor's own API: official = route).
/// What `/model` shows for a Seedance model of the table.
pub fn card(s: &'static Spec) -> crate::card::ModelCard {
    use crate::card::{self, INPUT, LIMITS, NOTES, OUTPUT, PRICE, TOOLS};
    let mut c = card::ModelCard::default();
    c.row(LIMITS, "length (sec=)", format!("{} to {} seconds", s.seconds.0, s.seconds.1));
    c.caps(&Caps(s));
    c.row(OUTPUT, "resolutions", format!("{} (Ark's default 720p)", s.resolutions.join(", ")));
    c.row(OUTPUT, "sound", if s.audio { "generated (audio=)" } else { "none" });
    if s.draft {
        c.row(OUTPUT, "draft mode", "yes");
    }
    let mut takes = vec!["text".to_string(), "a first frame".to_string()];
    if s.first_last {
        takes.push("first and last frames".into());
    }
    if let Some(r) = s.refs {
        takes.push(format!(
            "full-modal references: up to {} images, {} videos, {} audio clips{}",
            r.images,
            r.videos,
            r.audios,
            if r.audio_alone { " (audio alone too)" } else { "" }
        ));
    }
    c.row(INPUT, "takes", takes.join("; "));
    if s.web_search {
        c.row(TOOLS, "web_search", "available");
    }
    for (r, plain, with_video) in s.price {
        c.row(
            PRICE,
            r.to_string(),
            format!("{} / 1M tokens ({} with video input)", card::cny(*plain), card::cny(*with_video)),
        );
    }
    if s.flex {
        c.row(PRICE, "tier=flex", "offered");
    }
    c.row(NOTES, "name in Ark's docs", s.name);
    if s.prefix {
        c.row(NOTES, "matches", format!("ids starting with {}", s.id));
    }
    c
}

struct Caps(&'static Spec);

impl RouteCaps for Caps {
    fn name(&self) -> &str {
        NAME
    }
    fn tool(&self, tool: Tool) -> bool {
        tool == Tool::Search && self.0.web_search
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
        if self.0.flex { vec!["flex".into()] } else { Vec::new() }
    }
    fn generation(&self) -> Generation {
        Generation { quality: self.0.draft, seconds: true, audio: self.0.audio, ..Default::default() }
    }
}

fn err(m: impl Into<String>) -> LlmError {
    LlmError::request(m)
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

pub(crate) fn unix_now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// Whether the prompt names its images the way reference prompts do
/// ("图片1", "图 2", "@图像1", "image 1", "[Image 2]", "img3"): then the images
/// are references, not frames.
pub(crate) fn names_images(prompt: &str) -> bool {
    let lower = prompt.to_lowercase();
    ["图片", "图像", "图", "image", "img"].iter().any(|kw| {
        lower.match_indices(kw).any(|(i, _)| {
            let rest = lower[i + kw.len()..].trim_start_matches([' ', '\u{3000}', '_', '-']);
            rest.chars().next().is_some_and(|c| c.is_ascii_digit() || ('０'..='９').contains(&c))
        })
    })
}

// ------------------------------------------------------------------ inputs

/// What a request takes from the conversation.
#[derive(Debug, Default)]
struct Inputs {
    prompt: String,
    /// Data URLs, the replied-to answer's images first.
    images: Vec<String>,
    answer_images: usize,
    /// Reference video URLs (a Seedance video replied to).
    videos: Vec<String>,
    /// Why the video of the answer replied to cannot be passed (then `videos` is empty).
    video_problem: Option<String>,
    /// Data URLs of reference audio.
    audios: Vec<String>,
    /// The replied-to Seedance task (its last poll), when the answer is a Seedance video.
    answer_task: Option<Value>,
    /// The replied-to answer's model.
    answer_model: Option<String>,
    /// Characters of base64 media.
    data_len: usize,
    notes: Vec<String>,
}

/// Image types Ark takes ("jpeg、png、webp、bmp、tiff、gif", 2.x also heic, heif).
fn image_type(mime: &str, family: Option<Family>) -> Option<&'static str> {
    let m = media::base_mime(mime);
    let t = match m.as_str() {
        "image/jpeg" | "image/jpg" => "jpeg",
        "image/png" => "png",
        "image/webp" => "webp",
        "image/bmp" | "image/x-ms-bmp" => "bmp",
        "image/tiff" => "tiff",
        "image/gif" => "gif",
        "image/heic" => "heic",
        "image/heif" => "heif",
        _ => return None,
    };
    if matches!(t, "heic" | "heif") && family == Some(Family::V10) {
        return None;
    }
    Some(t)
}

/// Audio types Ark takes ("wav、mp3").
fn audio_type(mime: &str, filename: Option<&str>) -> Option<&'static str> {
    let ext = filename.and_then(|f| f.rsplit_once('.')).map(|(_, e)| e.to_ascii_lowercase());
    match media::base_mime(mime).as_str() {
        "audio/wav" | "audio/x-wav" | "audio/wave" | "audio/vnd.wave" => Some("wav"),
        "audio/mpeg" | "audio/mp3" | "audio/mpeg3" | "audio/x-mpeg-3" => Some("mp3"),
        _ => match ext.as_deref() {
            Some("wav") => Some("wav"),
            Some("mp3") => Some("mp3"),
            _ => None,
        },
    }
}

async fn image_url(
    blob: &omni_core::BlobRef,
    family: Option<Family>,
    model: &str,
    blobs: &dyn BlobLoader,
) -> Result<String, LlmError> {
    let Some(t) = image_type(&blob.mime, family) else {
        return Err(err(format!(
            "`{model}` does not take {} images on Ark; formats: jpeg, png, webp, bmp, tiff, gif{}",
            blob.mime,
            if family == Some(Family::V10) { "" } else { ", heic, heif" }
        )));
    };
    if blob.size >= MAX_IMAGE {
        return Err(err(format!("an image is too large for Ark ({} bytes; each must be under 30 MB)", blob.size)));
    }
    let bytes = crate::provider::load(blobs, blob).await?;
    // "`<图片格式>` 需小写".
    Ok(crate::provider::data_url(&format!("image/{t}"), &bytes))
}

/// The URL of a Seedance video answer and whether it still works.
fn answer_video_url(task: &Value, now: i64) -> Result<String, &'static str> {
    let url = task.pointer("/content/video_url").and_then(Value::as_str).filter(|u| !u.is_empty());
    let Some(url) = url else { return Err("its task has no video URL") };
    let made = task.get("updated_at").or_else(|| task.get("created_at")).and_then(Value::as_i64);
    if made.is_some_and(|t| now - t >= URL_VALID_SECS) {
        return Err("its URL has expired (Ark keeps generated videos 24 hours)");
    }
    Ok(url.to_string())
}

/// The new message's text and media, and what the answer it replies to made.
async fn inputs(
    history: &[Turn],
    model: &str,
    family: Option<Family>,
    now: i64,
    blobs: &dyn BlobLoader,
) -> Result<Inputs, LlmError> {
    let Some((last, earlier)) = history.split_last() else {
        return Err(err(format!("`{model}` needs a prompt")));
    };
    if history.iter().any(|t| t.role == Role::System && !t.answer_text().trim().is_empty()) {
        return Err(err(format!(
            "`{model}` takes no system prompt: Ark's video API has a prompt and reference media only"
        )));
    }
    if last.role != Role::User {
        return Err(err(format!(
            "`{model}` takes a prompt, not an assistant prefill (the conversation must end with a user message)"
        )));
    }
    let mut ins = Inputs::default();
    // A reply to a generated answer: its images (first) and its video.
    if let Some(prev) = earlier.last().filter(|t| t.role == Role::Assistant) {
        for p in &prev.parts {
            match p {
                Part::Image { blob, generated: true, thought: false, .. } => {
                    let url = image_url(blob, family, model, blobs).await?;
                    ins.data_len += url.len();
                    ins.images.push(url);
                }
                Part::File { blob, filename, generated: true }
                    if classify_mime(&blob.mime, filename.as_deref()) == MediaClass::Video =>
                {
                    match prev.native.as_ref().filter(|n| n.dialect == DIALECT) {
                        Some(n) => {
                            match answer_video_url(&n.payload, now) {
                                Ok(url) => ins.videos.push(url),
                                Err(why) => {
                                    ins.video_problem = Some(format!(
                                        "the video replied to cannot be passed to `{model}`: {why}, and Ark takes \
                                         input videos only as URLs (the bot uploads nothing)"
                                    ));
                                }
                            }
                            ins.answer_task = Some(n.payload.clone());
                            ins.answer_model = Some(n.model.clone());
                        }
                        None => {
                            ins.video_problem = Some(format!(
                                "`{model}` takes input videos only as URLs, and the bot uploads nothing: it can pass \
                                 the video of a Seedance answer (made on Ark in the last 24 hours), not this one{}",
                                prev.native.as_ref().map(|n| format!(" (made by `{}`)", n.model)).unwrap_or_default()
                            ));
                        }
                    }
                }
                _ => {}
            }
        }
    }
    ins.answer_images = ins.images.len();
    let edit_chain = ins.answer_images > 0 || ins.answer_task.is_some() || ins.video_problem.is_some();
    if !edit_chain
        && earlier.iter().any(|t| {
            t.role != Role::System
                && t.parts.iter().any(|p| matches!(p, Part::Text { text, .. } if !text.trim().is_empty()))
        })
    {
        return Err(err(format!(
            "`{model}` takes one prompt, not a conversation: reply to an image or video it generated to edit it, or start a new message"
        )));
    }
    let mut texts: Vec<&str> = Vec::new();
    for p in &last.parts {
        match p {
            Part::Text { text, .. } => {
                if !text.trim().is_empty() {
                    texts.push(text);
                }
            }
            Part::Image { blob, .. } => {
                let url = image_url(blob, family, model, blobs).await?;
                ins.data_len += url.len();
                ins.images.push(url);
            }
            Part::File { blob, filename, .. } => {
                let name = filename.clone().unwrap_or_else(|| blob.mime.clone());
                match classify_mime(&blob.mime, filename.as_deref()) {
                    MediaClass::Image => {
                        let url = image_url(blob, family, model, blobs).await?;
                        ins.data_len += url.len();
                        ins.images.push(url);
                    }
                    MediaClass::Audio => {
                        let Some(t) = audio_type(&blob.mime, filename.as_deref()) else {
                            return Err(err(format!(
                                "`{model}` takes WAV or MP3 audio on Ark, not {} ({name}); the bot does not convert it",
                                blob.mime
                            )));
                        };
                        if blob.size > MAX_AUDIO {
                            return Err(err(format!(
                                "{name} is too large for Ark ({} bytes; audio must be at most 15 MB)",
                                blob.size
                            )));
                        }
                        let bytes = crate::provider::load(blobs, blob).await?;
                        let url = crate::provider::data_url(&format!("audio/{t}"), &bytes);
                        ins.data_len += url.len();
                        ins.audios.push(url);
                    }
                    MediaClass::Video => {
                        return Err(err(format!(
                            "`{model}` takes input videos only as URLs, and the bot uploads nothing ({name}): reply to \
                             a video Seedance made in the last 24 hours to edit or extend it"
                        )));
                    }
                    _ => {
                        return Err(err(format!(
                            "`{model}` takes text, images, audio and its own videos only: {name} is {}",
                            blob.mime
                        )));
                    }
                }
            }
            Part::RemoteMedia { url, .. } => {
                return Err(err(format!("`{model}` cannot read remote media on Ark's video API ({url})")));
            }
            Part::Location { .. } => return Err(crate::provider::no_location(model, NAME)),
            Part::Reasoning(_) | Part::ToolCall(_) | Part::Refusal { .. } => {}
        }
    }
    ins.prompt = texts.join("\n");
    if ins.data_len + ins.prompt.len() > MAX_BODY {
        return Err(err(format!(
            "the media of this request are too large for Ark's 64 MB request limit ({} bytes as base64)",
            ins.data_len
        )));
    }
    Ok(ins)
}

// ------------------------------------------------------------------ requests

/// The kind of request, from its inputs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Text,
    FirstFrame,
    FirstLast,
    Reference,
    DraftFinal,
}

fn mode_of(ins: &Inputs, final_of_draft: bool) -> Mode {
    if final_of_draft {
        Mode::DraftFinal
    } else if !ins.videos.is_empty() || !ins.audios.is_empty() {
        Mode::Reference
    } else if ins.images.is_empty() {
        Mode::Text
    } else if ins.images.len() >= 3 || names_images(&ins.prompt) {
        Mode::Reference
    } else if ins.images.len() == 1 {
        Mode::FirstFrame
    } else {
        Mode::FirstLast
    }
}

/// The settings line of a mode.
fn describe(mode: Mode, ins: &Inputs) -> String {
    let previous =
        |what: &str| if ins.answer_images > 0 { format!("{what} (the previous image)") } else { what.into() };
    match mode {
        Mode::Text => "text-to-video".into(),
        Mode::FirstFrame => format!("image-to-video: {}", previous("first frame")),
        Mode::FirstLast => format!("image-to-video: {} + last frame", previous("first frame")),
        Mode::Reference => {
            let mut parts = Vec::new();
            if !ins.videos.is_empty() {
                parts.push("the previous video".to_string());
            }
            let n = ins.images.len();
            if n > 0 {
                parts.push(format!("{n} image{}", plural(n)));
            }
            let a = ins.audios.len();
            if a > 0 {
                parts.push(format!("{a} audio clip{}", plural(a)));
            }
            format!("reference-to-video: {}", parts.join(" + "))
        }
        Mode::DraftFinal => FINAL.into(),
    }
}

fn src(by_user: bool) -> String {
    if by_user { "option" } else { "default" }.into()
}

/// `1` `2` `4` as Seedance resolutions.
/// Ark's default `resolution` on every Seedance 2.x model.
const DEFAULT_RESOLUTION: &str = "720p";

/// The `size=` value of a resolution.
fn size_for(resolution: &str) -> &'static str {
    match resolution {
        "1080p" => "2K",
        "4k" => "4K",
        _ => "1K",
    }
}

fn resolution_for(size: &str) -> Option<&'static str> {
    match size {
        "1K" => Some("720p"),
        "2K" => Some("1080p"),
        "4K" => Some("4k"),
        _ => None,
    }
}

fn size_letters(spec: &Spec) -> String {
    [("1K", "720p"), ("2K", "1080p"), ("4K", "4k")]
        .iter()
        .filter(|(_, r)| spec.resolutions.contains(r))
        .map(|(l, r)| format!("{l} ({r})"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// A content item of an image.
fn image_item(url: &str, role: &str) -> Value {
    json!({"type": "image_url", "image_url": {"url": url}, "role": role})
}

/// Build a Seedance video task (see the module docs).
pub async fn prepare(
    cfg: &ArkConfig,
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
    let spec = spec(model);
    for &t in &req.tools {
        match t {
            Tool::Search if spec.is_some_and(|s| s.web_search) => {}
            Tool::Image => {
                return Err(err(format!(
                    "`{model}` is a video model: it makes videos without `+i` (the image tool of chat models)"
                )));
            }
            Tool::Search => {
                return Err(err(format!(
                    "`{model}`: web search (`+s`) on Ark's video API has no published price, so the bot does not use it"
                )));
            }
            t => {
                return Err(err(format!("`{model}` has no {} (`+{}`) on Ark's video API", t.describe(), t.letter())));
            }
        }
    }
    let now = unix_now();
    let Some(spec) = spec else {
        return Err(crate::provider::not_in_table(NAME, model));
    };
    let q = req.quality.as_deref();
    match q {
        Some(q) if !spec.draft => {
            return Err(err(format!("`{model}` has no mode switch (`q={q}`): draft mode is on Seedance 2.5")));
        }
        Some(q) if !matches!(q, "draft" | "final") => {
            return Err(err(format!(
                "`{model}` has no mode `q={q}`; modes: draft (a 480p draft), final (the final video of a draft \
                 replied to)"
            )));
        }
        _ => {}
    }
    let mut basic = req.clone();
    basic.tools.clear();
    basic.image_size = None;
    basic.aspect_ratio = None;
    request::validate(model, &basic, &Caps(spec)).map_err(LlmError::request)?;
    let draft = q == Some("draft");
    let ins = inputs(input.history, model, Some(spec.family), now, blobs).await?;
    let mode = mode_of(&ins, q == Some("final"));
    if mode != Mode::DraftFinal
        && let Some(why) = &ins.video_problem
    {
        return Err(err(why.clone()));
    }
    let mut notes = ins.notes.clone();
    let mut warnings = Vec::new();

    // What each mode needs, and what the model takes.
    let mut draft_id = None;
    match mode {
        Mode::Text if ins.prompt.is_empty() => {
            return Err(err(format!("`{model}` needs a prompt (the text of the message) for text-to-video")));
        }
        Mode::FirstLast if !spec.first_last => {
            return Err(err(format!(
                "`{model}` takes one image (the first frame), not two: first and last frames are on Seedance 2.x"
            )));
        }
        Mode::Reference => {
            let Some(r) = spec.refs else {
                return Err(err(format!(
                    "`{model}` takes a first frame (or first and last frames), not reference {}: references are on \
                     Seedance 2.x",
                    if ins.images.len() >= 3 { "images" } else { "media" }
                )));
            };
            for (n, max, what) in [
                (ins.images.len(), r.images, "images"),
                (ins.videos.len(), r.videos, "videos"),
                (ins.audios.len(), r.audios, "audio clips"),
            ] {
                if n > max {
                    return Err(err(format!("`{model}` takes at most {max} reference {what} ({n} given)")));
                }
            }
            if !r.audio_alone && ins.images.is_empty() && ins.videos.is_empty() {
                return Err(err(format!(
                    "`{model}` takes reference audio only with a reference image or video (Seedance 2.5 takes audio \
                     alone)"
                )));
            }
        }
        Mode::DraftFinal => {
            let task = ins.answer_task.as_ref().filter(|t| t.get("draft").and_then(Value::as_bool) == Some(true));
            let Some(task) = task else {
                return Err(err(
                    "`q=final` makes the final video of a Seedance 2.5 draft: reply /m with this model and `+q=final` to a \
                     draft video (one made with `+q=draft`)",
                ));
            };
            if ins.answer_model.as_deref() != Some(model) {
                return Err(err(format!(
                    "the draft was made by `{}`: its final must be made by the same model",
                    ins.answer_model.as_deref().unwrap_or("another model")
                )));
            }
            if task.get("created_at").and_then(Value::as_i64).is_some_and(|t| now - t >= DRAFT_VALID_SECS) {
                return Err(err("the draft has expired (Ark keeps draft tasks 14 days): make a new one"));
            }
            if ins.images.len() > ins.answer_images || !ins.audios.is_empty() {
                return Err(err(
                    "the final video reuses the draft's inputs: it takes no new images or audio".to_string()
                ));
            }
            draft_id = task.get("id").and_then(Value::as_str).map(str::to_string);
            if draft_id.is_none() {
                return Err(err("the draft's task id is unknown: make a new draft"));
            }
            if !ins.prompt.is_empty() {
                notes.push("ℹ️ the message's text is not sent: the final video reuses the draft's prompt".into());
            }
        }
        _ => {}
    }

    // Options, by mode.
    let mut unused: Vec<String> = Vec::new();
    let mut why_unused = "";
    let resolution = match (mode, req.image_size.as_deref()) {
        (Mode::DraftFinal, None | Some("2K")) => "1080p",
        (Mode::DraftFinal, Some(s)) => {
            return Err(err(format!("the final video of a draft is 1080p (2K); size {s} is not available")));
        }
        (_, Some(s)) if draft => {
            return Err(err(format!("a draft is 480p, which has no size letter: drop size {s}")));
        }
        (_, None) if draft => "480p",
        // Ark's default.
        (_, None) => DEFAULT_RESOLUTION,
        (_, Some(s)) => match resolution_for(s).filter(|r| spec.resolutions.contains(r)) {
            Some(r) => r,
            None => {
                return Err(err(format!(
                    "video size {s} is not available for `{model}`; sizes: {}",
                    size_letters(spec)
                )));
            }
        },
    };
    let video_input = !ins.videos.is_empty();
    let ratio = match &req.aspect_ratio {
        None => None,
        Some(r) if !RATIOS.contains(&r.as_str()) => {
            return Err(err(format!("aspect ratio {r} is not available for `{model}`; ratios: {}", RATIOS.join(", "))));
        }
        Some(r) => {
            let keep = match mode {
                Mode::DraftFinal => Some("the final video reuses the draft's aspect ratio and length"),
                Mode::FirstFrame | Mode::FirstLast if spec.family == Family::V25 => {
                    Some("Seedance 2.5 keeps the first frame's aspect ratio")
                }
                Mode::Reference if video_input && spec.family == Family::V25 => {
                    Some("Seedance 2.5 keeps the source video's aspect ratio in an edit or an extension")
                }
                _ => None,
            };
            match keep {
                Some(why) => {
                    unused.push(format!("aspect ratio {r}"));
                    why_unused = why;
                    None
                }
                None => Some(r.clone()),
            }
        }
    };
    let duration = match req.seconds {
        None => None,
        Some(s) if !(spec.seconds.0..=spec.seconds.1).contains(&s) => {
            return Err(err(format!("`{model}` makes videos of {}–{} s (sec={s})", spec.seconds.0, spec.seconds.1)));
        }
        Some(s) => {
            let keep = match mode {
                Mode::DraftFinal => Some("the final video reuses the draft's aspect ratio and length"),
                Mode::Reference if video_input && spec.family == Family::V25 => Some(
                    "Seedance 2.5 keeps the source's length in a video edit and picks it for an extension (duration -1)",
                ),
                _ => None,
            };
            match keep {
                Some(why) => {
                    unused.push(format!("sec={s}"));
                    why_unused = why;
                    None
                }
                None => Some(s),
            }
        }
    };
    let search = req.tools.contains(&Tool::Search);
    if search && mode != Mode::Text {
        unused.push("web search (+s)".into());
        why_unused = "Ark's web search works on text-only requests";
    }
    if !unused.is_empty() {
        notes.push(format!("ℹ️ {} not sent: {why_unused}", unused.join(", ")));
    }

    // The body.
    let mut content = Vec::new();
    if mode != Mode::DraftFinal && !ins.prompt.is_empty() {
        content.push(json!({"type": "text", "text": ins.prompt}));
    }
    match mode {
        Mode::Text => {}
        Mode::FirstFrame => content.push(image_item(&ins.images[0], "first_frame")),
        Mode::FirstLast => {
            content.push(image_item(&ins.images[0], "first_frame"));
            content.push(image_item(&ins.images[1], "last_frame"));
        }
        Mode::Reference => {
            content.extend(ins.images.iter().map(|u| image_item(u, "reference_image")));
            content.extend(
                ins.videos
                    .iter()
                    .map(|u| json!({"type": "video_url", "video_url": {"url": u}, "role": "reference_video"})),
            );
            content.extend(
                ins.audios
                    .iter()
                    .map(|u| json!({"type": "audio_url", "audio_url": {"url": u}, "role": "reference_audio"})),
            );
        }
        Mode::DraftFinal => content.push(json!({"type": "draft_task", "draft_task": {"id": draft_id}})),
    }
    let mut body = json!({"model": model, "content": content});
    let mut applied = Vec::new();
    let mut settings = Vec::new();
    let mut set = |body: &mut Value, k: &str, v: Value, source: String| {
        body[k] = v.clone();
        let mut patch = Map::new();
        patch.insert(k.into(), v);
        applied.push(Applied { source, patch: Value::Object(patch) });
    };
    if mode == Mode::DraftFinal {
        // The draft's input video sets the final's price (Ark: "token 单价根据 Step 1 是否包含输入视频确定").
        let had_video = draft_had_video(input.history);
        settings.push(if had_video { FINAL_WITH_VIDEO } else { FINAL }.to_string());
    } else {
        settings.push(describe(mode, &ins));
    }
    set(&mut body, "resolution", json!(resolution), src(user.image_size.is_some()));
    settings.push(format!("resolution {resolution}"));
    if let Some(top) = spec
        .resolutions
        .last()
        .filter(|t| user.image_size.is_none() && !draft && mode != Mode::DraftFinal && **t != resolution)
    {
        let option = format!("+size={}", size_for(top));
        notes.extend(request::raise_note(&[Raise::new("video resolution", resolution, *top, option)]));
    }
    if let Some(d) = duration {
        set(&mut body, "duration", json!(d), src(user.seconds.is_some()));
        settings.push(format!("{d} s"));
    }
    if let Some(r) = ratio {
        set(&mut body, "ratio", json!(r), src(user.aspect_ratio.is_some()));
        settings.push(format!("aspect ratio {r}"));
    }
    if spec.audio && mode != Mode::DraftFinal {
        let on = req.audio.unwrap_or(true);
        set(&mut body, "generate_audio", json!(on), src(user.audio.is_some()));
        settings.push(if on { "with audio" } else { "no audio" }.into());
    } else if let Some(a) = req.audio {
        notes.push(format!(
            "ℹ️ audio={} not sent: {}",
            if a { "on" } else { "off" },
            if mode == Mode::DraftFinal {
                "the final video reuses the draft's audio"
            } else {
                "this model makes no audio"
            }
        ));
    }
    set(&mut body, "watermark", json!(false), "default".into());
    settings.push("no watermark".into());
    if draft {
        set(&mut body, "draft", json!(true), src(user.quality.is_some()));
        settings.push("draft (480p)".into());
    }
    if search && mode == Mode::Text {
        set(&mut body, "tools", json!([{"type": "web_search"}]), src(user.tools.contains(&Tool::Search)));
        settings.push("web search".into());
        warnings.push(
            "Ark's price list has no fee for the video API's web search: searches are not in the cost estimate".into(),
        );
    }
    if let Some(t) = req.tier.as_deref().filter(|t| *t == "flex") {
        set(&mut body, "service_tier", json!(t), "option".into());
        settings.push(format!("tier {t}"));
    }
    if let Some(u) = input.user.as_deref().filter(|u| !u.is_empty() && u.len() <= SAFETY_ID_MAX) {
        set(&mut body, "safety_identifier", json!(u), "default".into());
    }
    Ok(Prepared {
        dialect: DIALECT,
        model: model.to_string(),
        url: format!("{}{PATH}", cfg.base_url.trim_end_matches('/')),
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

/// Whether the draft replied to had an input video: its own request replied
/// to a Seedance video (the bot passes the video of the answer replied to).
fn draft_had_video(history: &[Turn]) -> bool {
    // … the draft's request (user), the answer before it.
    let n = history.len();
    n >= 4
        && history[n - 4].role == Role::Assistant
        && history[n - 4].native.as_ref().is_some_and(|x| x.dialect == DIALECT)
        && history[n - 4].parts.iter().any(|p| {
            matches!(p, Part::File { blob, filename, generated: true }
                if classify_mime(&blob.mime, filename.as_deref()) == MediaClass::Video)
        })
}

/// The request that resumes a task from its checkpoint (`{"task_id"}`):
/// polling `GET …/contents/generations/tasks/{id}` instead of creating
/// another video (no body: nothing is sent).
pub fn continuation_request(p: &Prepared, item: &Value) -> Prepared {
    let mut q = p.clone();
    if let Some(id) = item.get("task_id").and_then(Value::as_str) {
        let base = match p.url.find(PATH) {
            Some(i) => &p.url[..i + PATH.len()],
            None => p.url.as_str(),
        };
        q.url = format!("{base}/{id}");
        q.body = Value::Null;
    }
    q
}

// ------------------------------------------------------------------ running

/// Ark's errors ([`super::error_from_json`]), plus: a full task queue
/// (`QuotaExceeded` "The request has exceeded the quota": "当前账号处于排队中
/// 状态的任务数已超过限制，请稍后重试") is retried later, unlike the exhausted
/// free trial (`QuotaExceeded` "…has exhausted its free trial quota…").
pub fn video_error(err: &Value, status: Option<u16>, retry_after: Option<u64>, kind: ErrorKind) -> LlmError {
    let mut e = error_from_json(err, status, retry_after, kind);
    if e.error_type.as_deref() == Some("QuotaExceeded") && e.message.contains("has exceeded the quota") {
        e.retry = Retry::After(retry_after.unwrap_or(30));
    }
    e
}

/// Ark's `ModelNotOpen` with what it takes: the model must be activated in the
/// Ark console, and Seedance 2.x only for accounts with ¥200 (Ark's rule).
fn refine(mut e: LlmError) -> LlmError {
    if e.error_type.as_deref() == Some("ModelNotOpen") {
        e.message = format!(
            "{} — the account must activate the model in the Ark console (开通管理, \
             https://ark.volcengine.com/region:cn-beijing/openManagement); Seedance 2.5 and 2.0 can be \
             activated with an account balance of at least ¥200, a ¥200+ savings plan or a Seedance resource pack",
            e.message
        );
    }
    e
}

fn http<'a>(
    client: &'a reqwest::Client,
    cfg: &'a ArkConfig,
    observer: &'a dyn Observer,
    cancel: &'a CancellationToken,
) -> Http<'a> {
    Http {
        client,
        api_key: &cfg.api_key,
        idle_timeout: cfg.idle_timeout,
        map_error: video_error,
        refine,
        observer,
        cancel,
    }
}

/// What the tool call shows of a request: its settings and the media by role (no data).
fn tool_input(body: &Value) -> Value {
    let mut o = Map::new();
    let mut counts: Vec<(String, usize)> = Vec::new();
    for c in body.get("content").and_then(Value::as_array).into_iter().flatten() {
        match c.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(t) = c.get("text") {
                    o.insert("prompt".into(), t.clone());
                }
            }
            Some("draft_task") => {
                if let Some(id) = c.pointer("/draft_task/id") {
                    o.insert("draft_task".into(), id.clone());
                }
            }
            Some(_) => {
                let role = c.get("role").and_then(Value::as_str).unwrap_or("media").to_string();
                match counts.iter_mut().find(|(r, _)| *r == role) {
                    Some((_, n)) => *n += 1,
                    None => counts.push((role, 1)),
                }
            }
            None => {}
        }
    }
    for (role, n) in counts {
        o.insert(role, json!(n));
    }
    for k in ["resolution", "ratio", "duration", "generate_audio", "draft", "service_tier", "tools"] {
        if let Some(v) = body.get(k) {
            o.insert(k.into(), v.clone());
        }
    }
    Value::Object(o)
}

/// What the tool call shows of a finished task: what the API echoes about the
/// video (no URLs, timestamps or the user hash).
fn tool_output(task: &Value) -> Value {
    let mut o = task.as_object().cloned().unwrap_or_default();
    for k in [
        "id",
        "status",
        "content",
        "usage",
        "created_at",
        "updated_at",
        "execution_expires_after",
        "safety_identifier",
        "priority",
    ] {
        o.remove(k);
    }
    if o.get("error").is_some_and(Value::is_null) {
        o.remove("error");
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

/// The error of a task that ended without a video.
fn task_error(status: &str, task: &Value) -> LlmError {
    let mut e = match task.get("error").filter(|e| e.is_object()) {
        Some(e) => video_error(e, None, None, ErrorKind::InBand),
        None => match status {
            "expired" => LlmError::new(
                ErrorKind::InBand,
                Retry::Backoff,
                "the video task expired (not finished within its execution_expires_after)",
            ),
            "cancelled" => LlmError::new(ErrorKind::InBand, Retry::Never, "the video task was cancelled"),
            _ => LlmError::new(ErrorKind::InBand, Retry::Backoff, format!("the video task ended with status {status}")),
        },
    };
    if e.error_type.is_none() {
        e.error_type = Some(status.to_string());
    }
    let mut task = task.clone();
    super::hide_account(&mut task);
    e.details = Some(task);
    e
}

/// Run a video task: create it (unless resuming one), checkpoint its id, poll
/// it, download the video.
pub async fn stream(
    client: &reqwest::Client,
    cfg: &ArkConfig,
    p: &Prepared,
    sink: &mut dyn EventSink,
    observer: &dyn Observer,
    cancel: &CancellationToken,
) -> (Result<Value, LlmError>, CallOutcome) {
    let h = http(client, cfg, observer, cancel);
    let mut outcome = CallOutcome::default();
    let mut shown = String::new();
    let (id, poll_url) = if p.body.is_null() {
        // Resuming a task (see [`continuation_request`]).
        (p.url.rsplit('/').next().unwrap_or_default().to_string(), p.url.clone())
    } else {
        let reply = match h.post(&p.url, &p.body).await {
            Ok(r) => r,
            Err(e) => return (Err(e), outcome),
        };
        let Some(id) = reply.body.get("id").and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string)
        else {
            let e = match reply.body.get("error").filter(|e| e.is_object()) {
                Some(e) => video_error(e, None, None, ErrorKind::InBand).with_details(reply.body.clone()),
                None => LlmError::new(ErrorKind::Decode, Retry::Backoff, "Ark created no task (no `id`)")
                    .with_details(reply.body),
            };
            return (Err(e), outcome);
        };
        sink.event(StreamEvent::Meta { upstream_id: Some(id.clone()), provider: None, model: None });
        sink.event(call(&id, "queued", Some(tool_input(&p.body)), None));
        shown = "queued".into();
        // A restart resumes polling this task instead of paying for another.
        let item = json!({"task_id": id});
        sink.event(StreamEvent::Checkpoint(item.clone()));
        outcome.continuation = Some(item);
        let poll = format!("{}/{id}", p.url.trim_end_matches('/'));
        (id, poll)
    };
    outcome.upstream_id = Some(id.clone());
    let mut meta_model = false;
    loop {
        let task = match h.get(&poll_url).await {
            Ok(r) => r.body,
            Err(e) => return (Err(e), outcome),
        };
        if !meta_model && let Some(m) = task.get("model").and_then(Value::as_str) {
            meta_model = true;
            sink.event(StreamEvent::Meta { upstream_id: None, provider: None, model: Some(m.to_string()) });
        }
        let status = task.get("status").and_then(Value::as_str).unwrap_or("queued").to_string();
        match status.as_str() {
            "succeeded" => return (finish(&h, cfg, p, &id, &task, sink).await, outcome),
            "failed" | "expired" | "cancelled" => {
                sink.event(call(&id, &status, None, task.get("error").filter(|e| e.is_object()).cloned()));
                // Final: not resumable from the checkpoint (a retry makes a new task).
                sink.event(StreamEvent::Checkpoint(Value::Null));
                outcome.continuation = None;
                return (Err(task_error(&status, &task)), outcome);
            }
            _ => {
                if status != shown {
                    sink.event(call(&id, &status, None, None));
                    shown = status;
                }
                if let Err(e) = media::pause(POLL_EVERY, cancel).await {
                    return (Err(e), outcome);
                }
            }
        }
    }
}

/// A finished task: the searches it ran, the video, its usage, the finish.
async fn finish(
    h: &Http<'_>,
    cfg: &ArkConfig,
    p: &Prepared,
    id: &str,
    task: &Value,
    sink: &mut dyn EventSink,
) -> Result<Value, LlmError> {
    let Some(url) = task.pointer("/content/video_url").and_then(Value::as_str).filter(|u| !u.is_empty()) else {
        let mut e = LlmError::new(ErrorKind::InBand, Retry::Never, "the video task succeeded without a video URL");
        e.details = Some(task.clone());
        return Err(e);
    };
    let (data, mime) = h.download(url).await?;
    let mime = mime.filter(|m| m.starts_with("video/")).unwrap_or_else(|| "video/mp4".into());
    sink.event(call(id, "succeeded", None, Some(tool_output(task))));
    // The searches Ark ran ("实际搜索次数…如果为 0 表示未搜索"): no query or results.
    let searches = task.pointer("/usage/tool_usage/web_search").and_then(Value::as_u64);
    for i in 1..=searches.unwrap_or(0) {
        sink.event(StreamEvent::Tool(ToolCall {
            id: Some(format!("web_search_{i}")),
            name: "web_search".into(),
            status: Some("completed".into()),
            input: None,
            output: None,
        }));
    }
    if searches == Some(0) || (searches.is_none() && p.body.get("tools").is_some()) {
        sink.event(StreamEvent::Notice(NO_SEARCH.into()));
    }
    let ext = if mime == "video/quicktime" { "mov" } else { "mp4" };
    let name = match task.get("duration").and_then(Value::as_i64).filter(|d| *d > 0) {
        Some(d) => format!("{}-{d}s.{ext}", p.model),
        None => format!("{}.{ext}", p.model),
    };
    sink.event(StreamEvent::File { data, mime, name: Some(name) });
    sink.event(StreamEvent::Usage(usage(p, task, cfg.cny_per_usd)));
    sink.event(StreamEvent::Finish { reason: Some("stop".into()), native: Some("succeeded".into()) });
    Ok(task.clone())
}

/// Whether the request (or the draft of a final) has an input video.
fn has_video_input(p: &Prepared) -> bool {
    p.settings.iter().any(|s| s == FINAL_WITH_VIDEO)
        || p.body
            .get("content")
            .and_then(Value::as_array)
            .is_some_and(|c| c.iter().any(|x| x.get("type").and_then(Value::as_str) == Some("video_url")))
}

/// CNY of a finished task: its tokens at the model's price for the output
/// resolution and the input (with or without video), halved for flex.
pub fn cost_cny(spec: &Spec, tokens: u64, resolution: &str, video_input: bool, flex: bool) -> Option<f64> {
    let &(_, without, with) = spec.price.iter().find(|(r, _, _)| r.eq_ignore_ascii_case(resolution))?;
    let per_m = if video_input { with } else { without };
    Some(tokens as f64 * per_m / 1e6 * if flex { 0.5 } else { 1.0 })
}

fn usage(p: &Prepared, task: &Value, cny_per_usd: f64) -> Usage {
    let u = task.get("usage").cloned().unwrap_or(Value::Null);
    let tokens = u.get("completion_tokens").and_then(Value::as_u64);
    let resolution = task
        .get("resolution")
        .or_else(|| p.body.get("resolution"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let flex = task.get("service_tier").or_else(|| p.body.get("service_tier")).and_then(Value::as_str) == Some("flex");
    let cost = spec(&p.model)
        .zip(tokens)
        .and_then(|(s, t)| cost_cny(s, t, &resolution, has_video_input(p), flex))
        .and_then(|c| crate::provider::usd(c, crate::provider::Currency::Cny, cny_per_usd));
    Usage {
        output_tokens: tokens,
        cost_usd: cost,
        cost_source: if cost.is_some() { CostSource::Estimated } else { CostSource::Unknown },
        raw: u,
        ..Default::default()
    }
}

#[cfg(test)]
pub(crate) mod tests;
