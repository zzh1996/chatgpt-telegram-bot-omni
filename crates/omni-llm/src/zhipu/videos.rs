//! Zhipu's video models on BigModel (`https://open.bigmodel.cn/api/paas/v4`):
//! CogVideoX and Vidu. A bare video-model id is this API, with no chat model
//! (the owner's rule; Zhipu has no chat route that outputs video). Spec:
//! docs.bigmodel.cn as fetched 2026-11-13 (`api-reference/模型-api/视频生成异步`,
//! `…/查询异步结果`, `cn/guide/models/video-generation/cogvideox-3`, `…/cogvideox-2`,
//! `…/viduq1`, `…/vidu2`, `cn/guide/models/free/cogvideox-flash`,
//! `cn/guide/start/model-overview`, `cn/guide/start/pricing`,
//! `cn/update/new-releases`) and the official Python SDK (`zai-sdk` 0.2.3,
//! `videos.generations`).
//!
//! - **Models** ([`MODELS`]; the overview's video models and the API's enums):
//!   `cogvideox-3` (the flagship: text, one image, or first + last frame; 5 or
//!   10 s; up to 4K; ¥1 per video), `cogvideox-2` (text or one image; ¥0.5),
//!   `cogvideox-flash` (as 2, free), and Vidu, whose modes are separate model
//!   ids: `viduq1-text`, `viduq1-image`, `viduq1-start-end` (5 s, 1080p, ¥2.5),
//!   `vidu2-image`, `vidu2-start-end` (4 s, 720p, ¥1.25), `vidu2-reference`
//!   (1–3 reference images, 4 s, 720p, ¥2.5). One video per request.
//! - **The job**: `POST /videos/generations` returns `{model, id, request_id,
//!   task_status}`; the id is checkpointed at once (a restart or a dropped
//!   poll polls the same task again, [`continuation_request`], never a second
//!   job) and `GET /async-result/{id}` is polled every 5 s, its `task_status`
//!   (`PROCESSING`, `SUCCESS`, `FAIL`; no percentage) on a `video_generation`
//!   tool call. `SUCCESS` carries `video_result[].url` (and a cover image), a
//!   temporary link ("有效期为 1 天" on the Vidu pages) that the bot downloads
//!   (no credentials) as the answer's video file. A `FAIL` clears the
//!   checkpoint (polling cannot change it) before the error: without a reason
//!   it is retried with a new task, a content filter or an error object is
//!   classified as such (shared with GLM-Image, [`super::images`]).
//! - **Input**: the message's text (at most 512 characters) and its images,
//!   the generated image of an answer replied to first (an image made with
//!   GLM-Image, say, animated by `/m zhipu:cogvideox-3$…`). Images are data
//!   URLs: PNG/JPEG up to 5 MB on CogVideoX, PNG/JPEG/WebP under 50 MB on
//!   Vidu. The mode follows the number of images (CogVideoX-3: none =
//!   text-to-video, one = first frame, two = first and last frame). Zhipu
//!   takes no video input: a reply to a generated video makes a new one from
//!   the text (noted), an attached video is an error. Earlier messages are
//!   not sent (noted).
//! - **Defaults**: `quality: "quality"` (the API's default is `speed`),
//!   `with_audio: true` where the model makes audio (CogVideoX "AI 音效",
//!   documented default off; Vidu background music on `vidu2-image` (only
//!   for 4 s videos, so not on the 5 s `viduq1-image`), the start-end and
//!   reference models; none on `viduq1-text`), `fps: 60` (the highest; default 30),
//!   `watermark_enabled: false` (CogVideoX; "仅允许已签署免责声明的客户使用",
//!   signed under 个人中心 → 安全管理 → 去水印管理; Zhipu documents no watermark
//!   switch for Vidu), the largest `size` (text-to-video on CogVideoX:
//!   3840x2160; with images the API's default keeps the image's aspect ratio
//!   at a short side of 1080, so no size is sent unless asked; Vidu: the
//!   model's resolution, sent), `user_id` = the bot's user hash (6–128
//!   characters). The length is the API's default (CogVideoX-3 5 s; Vidu's
//!   is fixed). Zhipu documents no moderation switch.
//! - **Options**: `sec=` the `duration` (CogVideoX-3 5 or 10; Vidu its one
//!   value; CogVideoX-2 has none), `q=quality|speed` (CogVideoX), size and
//!   ratio: CogVideoX's `size` is the video's pixel size, picked by ratio
//!   (`S W H w h ar=`, else 16:9) and size letter (`1` `2` `4` = short side
//!   720 / 1080 / 2160, else the largest of that ratio); Vidu's `size` is the
//!   resolution (`1` 720p, `2` 1080p; 480x360 of `vidu2-start-end` has no
//!   letter) and `aspect_ratio` (16:9, 9:16, 1:1) exists on `viduq1-text` and
//!   `vidu2-reference` only.
//! - **Not used**: Vidu's `style` (`general`/`anime`) and `movement_amplitude`
//!   (left at `auto`), since no bot option asks for them; the SDK's
//!   undocumented `off_peak` and `sensitive_word_check`; `request_id`.
//! - **Cost**: the CNY price per video (pricing page, 2026-11-13), estimated
//!   when the video arrives.

use std::time::Duration;

use omni_core::Usage;
use omni_core::content::ToolCall;
use omni_core::usage::CostSource;
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use super::NAME;
use super::images::{Run, has_options, ratio, task_failed};
use crate::error::{ErrorKind, LlmError, Retry};
use crate::event::StreamEvent;
use crate::openai::media::{self, MediaInput};
use crate::openrouter::options::{Applied, overlay};
use crate::provider::{Currency, DirectConfig, usd};
use crate::request::{self, Generation, Requested, RouteCaps, Tool};
use crate::{BlobLoader, CallInput, CallOutcome, EventSink, Observer, Prepared};

pub const DIALECT: &str = "zhipu.videos";
/// Between two polls of a task (videos take minutes).
const POLL_EVERY: Duration = if cfg!(test) { Duration::from_millis(5) } else { Duration::from_secs(5) };
/// "视频的文本描述，最大输入长度为 512 个字符" (every video model).
const PROMPT_CHARS: usize = 512;
const MB: u64 = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    /// `size` is the video's pixel size; `quality`, `fps`, `with_audio`, `watermark_enabled`.
    CogVideoX,
    /// `size` is the resolution; `aspect_ratio` on the text and reference models.
    Vidu,
}

/// How a request uses its images.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// The prompt alone.
    Text,
    /// One image: the first frame.
    FirstFrame,
    /// Two images: the first and the last frame.
    FirstLast,
    /// 1–3 images whose subjects the video keeps.
    References,
}

impl Mode {
    fn describe(self, n: usize) -> String {
        match self {
            Mode::Text => "text-to-video".into(),
            Mode::FirstFrame => "image-to-video".into(),
            Mode::FirstLast => "first and last frame".into(),
            Mode::References => format!("reference-to-video ({n} image{})", if n == 1 { "" } else { "s" }),
        }
    }

    fn takes(self) -> &'static str {
        match self {
            Mode::Text => "a prompt alone",
            Mode::FirstFrame => "one image (the first frame)",
            Mode::FirstLast => "two images (the first and the last frame)",
            Mode::References => "1–3 reference images",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Spec {
    pub id: &'static str,
    pub family: Family,
    /// What the model does, chosen by the number of images.
    pub modes: &'static [Mode],
    /// `size` values: pixel sizes (CogVideoX), or resolutions, largest first (Vidu).
    pub sizes: &'static [(u32, u32)],
    /// `duration` values in seconds, the API's default first; empty: none documented.
    pub durations: &'static [u32],
    /// `fps` values, highest first; empty: none.
    pub fps: &'static [u32],
    /// `aspect_ratio` values (Vidu's text and reference models).
    pub ratios: &'static [&'static str],
    /// `quality` (`quality` / `speed`).
    pub quality: bool,
    /// `with_audio` can be on (CogVideoX: AI sound effects; Vidu: background music).
    pub audio: bool,
    /// `watermark_enabled`.
    pub watermark: bool,
    /// Input image types and the largest input image (bytes).
    pub image_types: &'static [&'static str],
    pub max_image_bytes: u64,
    /// CNY per video.
    pub price_cny: f64,
}

/// CogVideoX: "图片支持 .png、.jpeg、.jpg 格式、图片大小：不超过 5M".
const COG_TYPES: &[&str] = &["image/png", "image/jpeg"];
/// Vidu: "支持的格式：png、jpeg、jpg、webp", "图像文件大小不得超过 50MB".
const VIDU_TYPES: &[&str] = &["image/png", "image/jpeg", "image/webp"];
const COG3_SIZES: &[(u32, u32)] =
    &[(1280, 720), (720, 1280), (1024, 1024), (1920, 1080), (1080, 1920), (2048, 1080), (3840, 2160)];
const COG2_SIZES: &[(u32, u32)] =
    &[(720, 480), (1024, 1024), (1280, 960), (960, 1280), (1920, 1080), (1080, 1920), (2048, 1080), (3840, 2160)];
const VIDU_RATIOS: &[&str] = &["16:9", "9:16", "1:1"];

const COG2: Spec = Spec {
    id: "cogvideox-2",
    family: Family::CogVideoX,
    modes: &[Mode::Text, Mode::FirstFrame],
    sizes: COG2_SIZES,
    durations: &[],
    fps: &[60, 30],
    ratios: &[],
    quality: true,
    audio: true,
    watermark: true,
    image_types: COG_TYPES,
    max_image_bytes: 5 * MB,
    price_cny: 0.5,
};

const VIDU_Q1: Spec = Spec {
    id: "",
    family: Family::Vidu,
    modes: &[],
    sizes: &[(1920, 1080)],
    durations: &[5],
    fps: &[],
    ratios: &[],
    quality: false,
    audio: false,
    watermark: false,
    image_types: VIDU_TYPES,
    // "Base64 解码后，字节长度必须小于 50MB".
    max_image_bytes: 50 * MB - 1,
    price_cny: 2.5,
};

const VIDU_2: Spec = Spec { sizes: &[(1280, 720)], durations: &[4], audio: true, price_cny: 1.25, ..VIDU_Q1 };

/// Zhipu's video models and prices (CNY per video), 2026-11-13: the API
/// reference's request schemas (models, sizes, durations, fps, inputs), the
/// model pages and the price list.
pub const MODELS: &[Spec] = &[
    Spec {
        id: "cogvideox-3",
        modes: &[Mode::Text, Mode::FirstFrame, Mode::FirstLast],
        sizes: COG3_SIZES,
        durations: &[5, 10],
        price_cny: 1.0,
        ..COG2
    },
    COG2,
    Spec { id: "cogvideox-flash", price_cny: 0.0, ..COG2 },
    Spec { id: "viduq1-text", modes: &[Mode::Text], ratios: VIDU_RATIOS, ..VIDU_Q1 },
    // `with_audio`: "仅当最终生成的视频时长为 4 秒时支持", and Q1 makes 5 s videos.
    Spec { id: "viduq1-image", modes: &[Mode::FirstFrame], ..VIDU_Q1 },
    Spec { id: "viduq1-start-end", modes: &[Mode::FirstLast], audio: true, ..VIDU_Q1 },
    Spec { id: "vidu2-image", modes: &[Mode::FirstFrame], ..VIDU_2 },
    Spec { id: "vidu2-start-end", modes: &[Mode::FirstLast], sizes: &[(1280, 720), (480, 360)], ..VIDU_2 },
    Spec { id: "vidu2-reference", modes: &[Mode::References], ratios: VIDU_RATIOS, price_cny: 2.5, ..VIDU_2 },
];

pub fn spec(model: &str) -> Option<&'static Spec> {
    MODELS.iter().find(|m| m.id == model)
}

/// Whether `model` is one of Zhipu's video models (served by this module).
pub fn is_video_model(model: &str) -> bool {
    spec(model).is_some() || model.starts_with("cogvideo") || model.starts_with("vidu")
}

/// What Zhipu's video API takes for a model (official = route).
struct Caps(Option<&'static Spec>);

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
        Vec::new()
    }
    fn generation(&self) -> Generation {
        let s = self.0;
        Generation {
            quality: s.is_some_and(|s| s.quality),
            seconds: s.is_some_and(|s| !s.durations.is_empty()),
            audio: s.is_some_and(|s| s.audio),
            ..Default::default()
        }
    }
}

fn err(m: impl Into<String>) -> LlmError {
    LlmError::request(m)
}

fn src(by_user: bool) -> String {
    if by_user { "option" } else { "default" }.into()
}

/// The size option's short side (`1` `2` `4`).
fn short_side(size: &str) -> Option<u32> {
    match size {
        "1K" => Some(720),
        "2K" => Some(1080),
        "4K" => Some(2160),
        _ => None,
    }
}

fn wxh((w, h): (u32, u32)) -> String {
    format!("{w}x{h}")
}

/// A CogVideoX `size` (pixels): the size of the asked ratio (16:9 without one)
/// with the asked short side, else the largest of that ratio. `None`: nothing
/// asked and the video starts from images (Zhipu's default keeps their ratio).
fn pixel_size(model: &str, s: &Spec, mode: Mode, req: &Requested) -> Result<Option<(u32, u32)>, LlmError> {
    let (a, b) = match req.aspect_ratio.as_deref() {
        Some(r) => ratio(r).ok_or_else(|| err(format!("`{r}` is not an aspect ratio")))?,
        None if req.image_size.is_some() || mode == Mode::Text => (16, 9),
        None => return Ok(None),
    };
    let fits: Vec<(u32, u32)> = s.sizes.iter().copied().filter(|&(w, h)| w as u64 * b == h as u64 * a).collect();
    let pick = match req.image_size.as_deref() {
        Some(z) => short_side(z).and_then(|n| fits.iter().copied().find(|&(w, h)| w.min(h) == n)),
        None => fits.iter().copied().max_by_key(|&(w, h)| w as u64 * h as u64),
    };
    pick.map(Some).ok_or_else(|| {
        let asked = match (&req.image_size, &req.aspect_ratio) {
            (Some(z), Some(r)) => format!("{z} {r}"),
            (Some(z), None) => format!("{z} (16:9)"),
            (None, Some(r)) => r.clone(),
            (None, None) => String::new(),
        };
        err(format!(
            "video size {asked} is not available for `{model}`; sizes: {} (a ratio `S W H w h ar=` picks the largest \
             of that ratio, 16:9 by default; `1` `2` `4` the one with a short side of 720, 1080 or 2160)",
            s.sizes.iter().map(|&z| wxh(z)).collect::<Vec<_>>().join(", ")
        ))
    })
}

/// A Vidu `size` (the resolution): the asked one (`1` 720p, `2` 1080p), else the largest.
fn resolution(model: &str, s: &Spec, req: &Requested) -> Result<(u32, u32), LlmError> {
    let Some(z) = req.image_size.as_deref() else { return Ok(s.sizes[0]) };
    let want = short_side(z);
    s.sizes.iter().copied().find(|&(w, h)| Some(w.min(h)) == want).ok_or_else(|| {
        let sizes: Vec<String> = s
            .sizes
            .iter()
            .map(|&(w, h)| match w.min(h) {
                720 => format!("1 ({w}x{h})"),
                1080 => format!("2 ({w}x{h})"),
                _ => format!("{w}x{h} (no size option)"),
            })
            .collect();
        err(format!("video size {z} is not available for `{model}`; sizes: {}", sizes.join(", ")))
    })
}

/// The mode for `n` images, if the model has one.
fn mode_for(s: &Spec, n: usize) -> Option<Mode> {
    let has = |m| s.modes.contains(&m);
    match n {
        0 if has(Mode::Text) => Some(Mode::Text),
        1 if has(Mode::FirstFrame) => Some(Mode::FirstFrame),
        2 if has(Mode::FirstLast) => Some(Mode::FirstLast),
        1..=3 if has(Mode::References) => Some(Mode::References),
        _ => None,
    }
}

fn no_mode(model: &str, s: &Spec, n: usize) -> LlmError {
    let takes: Vec<&str> = s.modes.iter().map(|m| m.takes()).collect();
    let has = match n {
        0 => "none".to_string(),
        1 => "1 image".to_string(),
        n => format!("{n} images"),
    };
    let mut m = format!("`{model}` takes {}; this message has {has}", takes.join(" or "));
    if s.family == Family::Vidu {
        m.push_str(
            " (Vidu's modes are separate models: viduq1-text, viduq1-image, viduq1-start-end, vidu2-image, \
             vidu2-start-end, vidu2-reference)",
        );
    }
    err(m)
}

/// Build a video request (see the module docs).
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
    if spec.is_none() && has_options(&req) {
        return Err(err(format!(
            "`{model}` is not in the bot's Zhipu video model table, so its options cannot be checked: send it \
             without options"
        )));
    }
    let mut basic = req.clone();
    basic.image_size = None;
    basic.aspect_ratio = None;
    request::validate(model, &basic, &Caps(spec)).map_err(LlmError::request)?;

    let MediaInput { prompt, answer_images, answer_video, own_images, own_videos, notes } =
        media::media_input(input.history, model, "Zhipu's video API")?;
    if !own_videos.is_empty() {
        return Err(err(format!("`{model}` takes no video input: Zhipu's video API takes a prompt and images")));
    }
    if answer_video.is_some() {
        return Err(err(format!(
            "`{model}` takes no video input, so it cannot edit the video replied to: start a new message for a new video"
        )));
    }
    let chars = prompt.chars().count();
    if chars > PROMPT_CHARS {
        return Err(err(format!("`{model}` takes at most {PROMPT_CHARS} characters of prompt (this one has {chars})")));
    }
    let images: Vec<_> = answer_images.iter().chain(&own_images).collect();
    let mode = match spec {
        Some(s) => Some(mode_for(s, images.len()).ok_or_else(|| no_mode(model, s, images.len()))?),
        None => None,
    };
    if prompt.is_empty() && images.is_empty() {
        return Err(err(format!("`{model}` needs a prompt (the text of the message)")));
    }

    let mut body = json!({"model": model});
    if !prompt.is_empty() {
        body["prompt"] = json!(prompt);
    }
    let mut applied = Vec::new();
    let mut settings = Vec::new();
    let mut warnings = Vec::new();
    if !images.is_empty() {
        let (types, max) = match spec {
            Some(s) => (s.image_types, s.max_image_bytes),
            None if model.starts_with("cogvideo") => (COG2.image_types, COG2.max_image_bytes),
            None => (VIDU_Q1.image_types, VIDU_Q1.max_image_bytes),
        };
        let mut urls = Vec::new();
        for b in &images {
            if b.size > max {
                return Err(err(format!(
                    "an input image of `{model}` is too large for Zhipu ({} bytes; the limit is {} MB)",
                    b.size,
                    max.div_ceil(MB)
                )));
            }
            urls.push(media::image_data_url(b, types, None, model, NAME, blobs).await?);
        }
        // One image is a string, several an array (first and last frame, references).
        body["image_url"] =
            if urls.len() == 1 && mode != Some(Mode::References) { json!(urls[0]) } else { json!(urls) };
    }
    match (spec, mode) {
        (Some(s), Some(mode)) => {
            settings.push(mode.describe(images.len()));
            if s.quality {
                let q = req.quality.clone().unwrap_or_else(|| "quality".into());
                if !["quality", "speed"].contains(&q.as_str()) {
                    return Err(err(format!(
                        "quality `{q}` is not available for `{model}`; levels: quality, speed (Zhipu's output mode: \
                         quality first or speed first)"
                    )));
                }
                settings.push(format!("{q} priority"));
                body["quality"] = json!(q);
                applied.push(Applied { source: src(user.quality.is_some()), patch: json!({"quality": q}) });
            }
            let size = match s.family {
                Family::CogVideoX => pixel_size(model, s, mode, &req)?,
                Family::Vidu => Some(resolution(model, s, &req)?),
            };
            if s.family == Family::Vidu && !s.ratios.is_empty() {
                if let Some(r) = &req.aspect_ratio {
                    if !s.ratios.contains(&r.as_str()) {
                        return Err(err(format!(
                            "aspect ratio {r} is not available for `{model}`; ratios: {}",
                            s.ratios.join(", ")
                        )));
                    }
                    body["aspect_ratio"] = json!(r);
                    applied
                        .push(Applied { source: src(user.aspect_ratio.is_some()), patch: json!({"aspect_ratio": r}) });
                }
            } else if s.family == Family::Vidu && req.aspect_ratio.is_some() {
                return Err(err(format!(
                    "`{model}` has no aspect ratio option (Vidu takes the image's; `aspect_ratio` is on viduq1-text \
                     and vidu2-reference)"
                )));
            }
            match size {
                Some(z) => {
                    let z = wxh(z);
                    settings.push(format!("size {z}"));
                    body["size"] = json!(z);
                    applied.push(Applied {
                        source: src(user.image_size.is_some() || user.aspect_ratio.is_some()),
                        patch: json!({"size": z}),
                    });
                }
                None => settings.push("size: Zhipu's default (the image's aspect ratio, short side 1080)".into()),
            }
            if let Some(r) = body.get("aspect_ratio").and_then(Value::as_str) {
                settings.push(format!("aspect ratio {r}"));
            }
            if let Some(&f) = s.fps.first() {
                settings.push(format!("{f} fps"));
                body["fps"] = json!(f);
                applied.push(Applied { source: "default".into(), patch: json!({"fps": f}) });
            }
            if let Some(d) = req.seconds {
                if !s.durations.contains(&d) {
                    return Err(err(format!(
                        "`{model}` makes videos of {} s (sec={d})",
                        s.durations.iter().map(u32::to_string).collect::<Vec<_>>().join(" or ")
                    )));
                }
                settings.push(format!("{d} s"));
                body["duration"] = json!(d);
                applied.push(Applied { source: src(user.seconds.is_some()), patch: json!({"duration": d}) });
            }
            if s.audio {
                // On by default: the price is per video either way (D79: free quality stays on).
                let on = req.audio.unwrap_or(true);
                let what = if s.family == Family::Vidu { "background music" } else { "AI sound effects" };
                settings.push(if on { what.to_string() } else { format!("no {what}") });
                body["with_audio"] = json!(on);
                applied.push(Applied { source: src(user.audio.is_some()), patch: json!({"with_audio": on}) });
            }
            if s.watermark {
                settings.push("no watermark".into());
                body["watermark_enabled"] = json!(false);
                applied.push(Applied { source: "default".into(), patch: json!({"watermark_enabled": false}) });
            }
        }
        _ if model.starts_with("cogvideo") => {
            // CogVideoX's common fields (`BaseVideoGenerationRequest`), at their best.
            body["quality"] = json!("quality");
            body["with_audio"] = json!(true);
            body["watermark_enabled"] = json!(false);
            settings.extend(["quality priority".to_string(), "AI sound effects".into(), "no watermark".into()]);
            applied.push(Applied {
                source: "default".into(),
                patch: json!({"quality": "quality", "with_audio": true, "watermark_enabled": false}),
            });
            warnings.push(format!(
                "`{model}` is not in the bot's Zhipu video model table: sizes, lengths and price unknown (Zhipu's \
                 default size, fps and length; cost not estimated)"
            ));
        }
        _ => warnings.push(format!(
            "`{model}` is not in the bot's Zhipu video model table: its options and price are unknown (only the \
             prompt and images are sent; cost not estimated)"
        )),
    }
    if let Some(u) = input.user.as_deref().filter(|u| (6..=128).contains(&u.chars().count())) {
        body["user_id"] = json!(u);
    }
    Ok(Prepared {
        dialect: DIALECT,
        model: model.to_string(),
        url: cfg.url("/videos/generations"),
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

/// The request that resumes a video task from its checkpoint (`{"task_id"}`,
/// after a restart or a dropped poll): polling `/async-result/{id}` instead
/// of creating another video.
pub fn continuation_request(p: &Prepared, item: &Value) -> Prepared {
    let mut q = p.clone();
    if let Some(id) = item.get("task_id").and_then(Value::as_str) {
        let base = match q.url.find("/async-result/") {
            Some(i) => &q.url[..i],
            None => q.url.strip_suffix("/videos/generations").unwrap_or(&q.url),
        };
        q.url = format!("{base}/async-result/{id}");
    }
    q
}

// ------------------------------------------------------------------ running

/// What the tool call shows of a request (no image bytes).
fn video_input(body: &Value) -> Value {
    let mut o = Map::new();
    for k in ["prompt", "quality", "size", "fps", "duration", "aspect_ratio", "with_audio", "watermark_enabled"] {
        if let Some(v) = body.get(k) {
            o.insert(k.into(), v.clone());
        }
    }
    let model = body.get("model").and_then(Value::as_str).unwrap_or_default();
    match body.get("image_url") {
        Some(Value::String(_)) => {
            o.insert("image_url".into(), json!("the first frame"));
        }
        Some(Value::Array(a)) => {
            let mode = spec(model).and_then(|s| mode_for(s, a.len()));
            let what = match mode {
                Some(Mode::FirstLast) => json!(["the first frame", "the last frame"]),
                _ => json!(format!("{} reference image{}", a.len(), if a.len() == 1 { "" } else { "s" })),
            };
            o.insert("image_url".into(), what);
        }
        _ => {}
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

/// The task's result as shown: without the (temporary, signed) video links.
fn shown(v: &Value) -> Value {
    let mut o = v.as_object().cloned().unwrap_or_default();
    if let Some(Value::Array(results)) = o.get_mut("video_result") {
        for r in results.iter_mut().filter_map(Value::as_object_mut) {
            r.remove("url");
        }
    }
    Value::Object(o)
}

/// Run a video request: create the task (unless resuming one), checkpoint its
/// id, poll it, download the video.
pub async fn stream(
    http: &reqwest::Client,
    cfg: &DirectConfig,
    p: &Prepared,
    sink: &mut dyn EventSink,
    observer: &dyn Observer,
    cancel: &CancellationToken,
) -> (Result<Value, LlmError>, CallOutcome) {
    let mut run = Run { http, cfg, sink, observer, cancel };
    let mut outcome = CallOutcome::default();
    let result = job(&mut run, p, &mut outcome).await;
    (result, outcome)
}

async fn job(run: &mut Run<'_>, p: &Prepared, outcome: &mut CallOutcome) -> Result<Value, LlmError> {
    let (id, poll_url, created, mut status) = match p.url.find("/async-result/") {
        // Resuming a task (see [`continuation_request`]).
        Some(i) => (p.url[i + "/async-result/".len()..].to_string(), p.url.clone(), None, String::new()),
        None => {
            let rb = run.http.post(&p.url).bearer_auth(&run.cfg.api_key).json(&p.body);
            let created = run.call(rb, &p.url, &p.body).await?;
            let Some(id) = created.get("id").and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string)
            else {
                let mut e = LlmError::new(ErrorKind::InBand, Retry::Never, "Zhipu created no task (no `id`)");
                e.details = Some(created);
                return Err(e);
            };
            run.sink.event(StreamEvent::Meta { upstream_id: Some(id.clone()), provider: None, model: None });
            let status = created.get("task_status").and_then(Value::as_str).unwrap_or("PROCESSING").to_string();
            run.sink.event(call(&id, &status, Some(video_input(&p.body)), None));
            // A restart resumes polling this task instead of paying for another.
            let item = json!({"task_id": id});
            run.sink.event(StreamEvent::Checkpoint(item.clone()));
            let url = continuation_request(p, &item).url;
            (id, url, Some(created), status)
        }
    };
    outcome.upstream_id = Some(id.clone());
    outcome.continuation = Some(json!({"task_id": id}));
    let mut model_seen = false;
    loop {
        let rb = run.http.get(&poll_url).bearer_auth(&run.cfg.api_key);
        let v = run.call(rb, &poll_url, &Value::Null).await?;
        if !model_seen && let Some(m) = v.get("model").and_then(Value::as_str) {
            run.sink.event(StreamEvent::Meta { upstream_id: None, provider: None, model: Some(m.to_string()) });
            model_seen = true;
        }
        let now = v.get("task_status").and_then(Value::as_str).unwrap_or("").to_string();
        match now.as_str() {
            "SUCCESS" => {
                let mut done = finish(run, p, &id, &v).await?;
                if let Some(c) = created {
                    done["task"] = c;
                }
                return Ok(done);
            }
            "FAIL" => {
                run.sink.event(call(&id, "FAIL", None, Some(shown(&v))));
                // The task, not the last poll's log id, is the call's upstream id.
                run.sink.event(StreamEvent::Meta { upstream_id: Some(id.clone()), provider: None, model: None });
                // Final: not resumable from the checkpoint (a retry makes a new task).
                run.sink.event(StreamEvent::Checkpoint(Value::Null));
                outcome.continuation = None;
                return Err(task_failed(v, "video"));
            }
            _ => {
                if !now.is_empty() && now != status {
                    run.sink.event(call(&id, &now, None, None));
                    status = now;
                }
            }
        }
        tokio::select! {
            _ = run.cancel.cancelled() => return Err(LlmError::cancelled()),
            _ = tokio::time::sleep(POLL_EVERY) => {}
        }
    }
}

/// A finished task: its videos downloaded, billed, the finish.
async fn finish(run: &mut Run<'_>, p: &Prepared, id: &str, v: &Value) -> Result<Value, LlmError> {
    run.sink.event(call(id, "SUCCESS", None, Some(shown(v))));
    let urls: Vec<String> = v
        .get("video_result")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|r| r.get("url").and_then(Value::as_str))
        .filter(|u| !u.is_empty())
        .map(str::to_string)
        .collect();
    if urls.is_empty() {
        let mut e = LlmError::new(ErrorKind::InBand, Retry::Never, "Zhipu's video task succeeded without a video");
        e.details = Some(v.clone());
        return Err(e);
    }
    let spec = spec(&p.model);
    let seconds =
        p.body.get("duration").and_then(Value::as_u64).or(spec.and_then(|s| s.durations.first()).map(|&d| d as u64));
    let name = match seconds {
        Some(d) => format!("{}-{d}s.mp4", p.model),
        None => format!("{}.mp4", p.model),
    };
    let n = urls.len();
    for url in urls {
        let (data, header) = run.download(&url, "video").await?;
        let mime = header
            .map(|h| media::base_mime(&h))
            .filter(|m| m.starts_with("video/"))
            .unwrap_or_else(|| "video/mp4".into());
        run.sink.event(StreamEvent::File { data, mime, name: Some(name.clone()) });
    }
    // The task, not the last poll's log id, is the call's upstream id.
    run.sink.event(StreamEvent::Meta { upstream_id: Some(id.to_string()), provider: None, model: None });
    let cost = spec.and_then(|s| usd(s.price_cny * n as f64, Currency::Cny, run.cfg.cny_per_usd));
    run.sink.event(StreamEvent::Usage(Usage {
        cost_usd: cost,
        cost_source: if cost.is_some() { CostSource::Estimated } else { CostSource::Unknown },
        raw: json!({"videos": n}),
        ..Default::default()
    }));
    run.sink.event(StreamEvent::Finish { reason: Some("stop".into()), native: Some("SUCCESS".into()) });
    Ok(v.clone())
}

#[cfg(test)]
mod tests;
