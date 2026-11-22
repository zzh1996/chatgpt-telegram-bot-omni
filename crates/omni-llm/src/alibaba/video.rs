//! Alibaba Model Studio's video models (DashScope, China (Beijing)): a bare
//! HappyHorse or Wan video id is DashScope's asynchronous video API, `POST
//! {root}/api/v1/services/aigc/video-generation/video-synthesis` (Wan 2.2 /
//! 2.1 first and last frames: `…/image2video/video-synthesis`) with
//! `X-DashScope-Async: enable` ("HTTP请求只支持异步"), then `GET
//! {root}/api/v1/tasks/{task_id}`. No chat model makes videos. Spec:
//! help.aliyun.com/zh/model-studio as fetched 2026-11-13:
//! `video-generate-edit-model`, the model pages (`happyhorse-1-1-t2v` …
//! `wanx2-1-kf2v-plus`: Beijing prices, providers), the API references
//! `happyhorse-{text,image,reference}-to-video-api-reference`,
//! `happyhorse-video-edit-api-reference`, `wan3-video-generation-api-reference`
//! (and its guide), `text-to-video-api-reference`, `image-to-video-general-
//! api-reference`, `wan-video-to-video-api-reference`, `wan-video-editing-api-
//! reference`, the 2.1–2.6 `legacy-wan-text-to-video-…`, `legacy-image-to-
//! video-…`, `legacy-wan-reference-to-video-…`, `legacy-image-to-video-by-
//! first-and-last-frame-api-reference`, `manage-asynchronous-tasks`,
//! `error-code`.
//!
//! **Models** ([`MODELS`]; all first-party, "推理服务供应商为阿里云百炼"):
//! HappyHorse 1.1 (Alibaba's first recommendation: `-t2v`, `-i2v` first frame,
//! `-r2v` 1–9 reference images; 480P–1080P, 3–15 s, with audio) and 1.0
//! (720P / 1080P, and `happyhorse-1.0-video-edit`); `wan3.0-video` and
//! `-prime` (all-in-one: text, first / first and last frames, references,
//! edits and extensions of a video, a web page; 480P–1080P, 2–30 s, audio
//! switch); Wan 2.7 (`-t2v`, `-i2v` first / last frame and continuing a video,
//! `-r2v` references, `-videoedit`; 720P / 1080P; dated snapshots); Wan 2.6,
//! 2.5 (preview), 2.2 and 2.1 (`wanx2.1-…`: the `wan2.1-…` ids are only in
//! Singapore) on the older protocol (`size` / `img_url` / `reference_urls` /
//! `first_frame_url`). Not served by the bot, each with the reason: VACE, the
//! character animation, speech-to-video, lip-sync and style models (they need
//! input videos or audio by URL, and options the bot has not).
//!
//! **Modes, from the inputs** (one prompt per task, as in [`super::images`]:
//! the new message's text and images; a reply to a generated answer uses what
//! it made, first). Images go as base64 data URLs; DashScope takes videos and
//! audio only as URLs (public, or uploaded to its temporary storage, which the
//! bot does not use), so the only video is the one of an Alibaba answer
//! replied to while its URL lives (24 hours), and audio is an error.
//! - The model id picks the task where the models are separate (t2v, i2v,
//!   r2v, edit). Wan 2.7 i2v: one image is the first frame, two the first and
//!   last frames, a video replied to is continued (`first_clip`, with an image
//!   as its last frame). Wan 2.6 r2v takes references only as URLs: a video
//!   replied to, no attached images.
//! - `wan3.0-video`: text only is text-to-video; one image the first frame,
//!   two the first and last frames, three or more (or a prompt naming them,
//!   "图1", "[Image 1]") references; a video replied to makes everything a
//!   reference (Wan decides from the prompt whether that is an edit, an
//!   extension or a new video); `+u` sends the message's one URL as a `link`
//!   (a web page Wan reads).
//!
//! **Defaults** (D3, D79, the owner): Alibaba's default resolution, which is
//! the largest (1080P; Wan 2.1 720P; the older `size` the largest of the
//! tier, 16:9 unless a ratio is asked), `watermark: false` (HappyHorse's
//! default adds "Happy Horse"), audio on where it is a switch (`audio: true`,
//! Alibaba's default on Wan 3.0 and Wan 2.6 flash; `audio=off` makes a silent
//! video, half the price on Wan 2.6 flash), Alibaba's own length and ratio.
//! Alibaba's prompt rewriting (`prompt_extend`, default on) stays at its
//! default, as for the image models; the rewritten prompt it returns
//! (`actual_prompt`, Wan 2.5 and older) is in the tool call's output.
//! Moderation is always on.
//!
//! **Options**: `size=1K` `2K` = 720P / 1080P (480P has no size); `ar=`
//! from the model's ratios (HappyHorse also 4:5, 5:4, 9:21, 21:9); `audio=`
//! (Wan 3.0, Wan 2.6 flash); `sec=` from
//! its lengths (2.5: 5 or 10; Wan 2.2, kf2v and wanx2.1 make 5 s only, so
//! `sec=` is an error, except wanx2.1-i2v-turbo's 3–5); `+u` (wan3.0). Options
//! a mode cannot use are not sent, with a note (a conversation's options carry
//! over to replies). Not used (no option): negative prompts, seeds, shot types,
//! effect templates, the audio setting of edits, Wan 2.7 r2v's first frame and
//! voices, files and asset ids.
//!
//! **Cost**: CNY per second by resolution (Beijing list prices), for the
//! seconds DashScope bills (`usage.duration`: input video plus output where a
//! video goes in; `video_duration` on the older models), at `usage.SR`.
//!
//! **Tasks**: the `task_id` is checkpointed when created ([`continuation_request`]
//! polls it again after a restart or a dropped poll; ids live 24 hours) and
//! polled every 5 s, its status on a `video_generation` (`video_edit`) tool
//! call; the MP4 is downloaded (no credentials). `FAILED` (by Alibaba's code,
//! e.g. `DataInspectionFailed`: never retried), `CANCELED` and `UNKNOWN` (gone)
//! clear the checkpoint first: a retry makes a new task.

use std::time::Duration;

use omni_core::Usage;
use omni_core::content::{Part, Role, ToolCall, Turn};
use omni_core::usage::CostSource;
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use super::images::native_root;
use super::{NAME, error_from_json};
use crate::ark::video::{names_images, unix_now};
use crate::error::{ErrorKind, LlmError, Retry};
use crate::event::StreamEvent;
use crate::openai::media::{self, Http};
use crate::openrouter::options::{Applied, overlay};
use crate::provider::{Currency, DirectConfig, usd};
use crate::request::{self, Generation, Requested, RouteCaps, Tool};
use crate::{BlobLoader, CallInput, CallOutcome, EventSink, MediaClass, Observer, Prepared, classify_mime};

pub const DIALECT: &str = "alibaba.videos";
const SYNTHESIS: &str = "/api/v1/services/aigc/video-generation/video-synthesis";
const IMAGE2VIDEO: &str = "/api/v1/services/aigc/image2video/video-synthesis";
const TASKS: &str = "/api/v1/tasks";
/// Between two polls (Alibaba suggests ~15 s; its poll API allows 20 QPS).
const POLL_EVERY: Duration = if cfg!(test) { Duration::from_millis(5) } else { Duration::from_secs(5) };
/// "链接有效期24小时".
const URL_VALID_SECS: i64 = 24 * 3600;
/// Images: "不超过20MB" (10 MB on Wan 2.2 / 2.1).
const MAX_IMAGE: u64 = 20 * 1024 * 1024;
const MAX_IMAGE_OLD: u64 = 10 * 1024 * 1024;

/// The request protocol of a model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Api {
    /// HappyHorse text-to-video: `input.prompt`; resolution, ratio, duration.
    HhT2v,
    /// HappyHorse first frame: `media` with one `first_frame`.
    HhI2v,
    /// HappyHorse references: `media` with 1–9 `reference_image`.
    HhR2v,
    /// HappyHorse video edit: `media` with a `video` and 0–5 `reference_image`.
    HhEdit,
    /// Wan 3.0 all-in-one `media`.
    Wan3,
    /// Wan 2.7 text-to-video.
    Wan27T2v,
    /// Wan 2.7 `media`: first frame, last frame, first clip.
    Wan27I2v,
    /// Wan 2.7 `media`: reference images and videos.
    Wan27R2v,
    /// Wan 2.7 video edit: `media` with a `video` and 0–4 `reference_image`.
    Wan27Edit,
    /// Wan 2.6 and older text-to-video: `parameters.size`.
    LegacyT2v,
    /// Wan 2.6 and older first frame: `input.img_url`.
    LegacyI2v,
    /// Wan 2.6 references: `input.reference_urls` (URLs only), `parameters.size`.
    LegacyR2v,
    /// Wan 2.2 / 2.1 first and last frames: `first_frame_url`, `last_frame_url`.
    Kf2v,
}

impl Api {
    fn edits(self) -> bool {
        matches!(self, Api::HhEdit | Api::Wan27Edit)
    }
    /// `parameters.size` instead of `resolution` + `ratio`.
    fn sized(self) -> bool {
        matches!(self, Api::LegacyT2v | Api::LegacyR2v)
    }
    /// Takes `ratio` (else the output follows the input).
    fn ratio(self) -> bool {
        matches!(self, Api::HhT2v | Api::HhR2v | Api::Wan3 | Api::Wan27T2v | Api::Wan27R2v | Api::Wan27Edit)
            || self.sized()
    }
    fn old(self) -> bool {
        matches!(self, Api::LegacyT2v | Api::LegacyI2v | Api::LegacyR2v | Api::Kf2v)
    }
}

/// The lengths a model makes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Seconds {
    Range(u32, u32),
    Set(&'static [u32]),
    /// Always this long.
    Fixed(u32),
    /// The input video's length.
    Input,
}

#[derive(Debug, Clone, Copy)]
pub struct Spec {
    pub id: &'static str,
    pub api: Api,
    /// Resolution tiers, lowest first (the last is the default).
    pub resolutions: &'static [&'static str],
    /// `ratio` values (for `size` models: the ratios of the largest tier).
    pub ratios: &'static [&'static str],
    pub seconds: Seconds,
    /// The output has sound.
    pub audio: bool,
    /// `parameters.audio` (Alibaba's default `true`; `audio=off` sends `false`).
    pub audio_switch: bool,
    /// CNY per second by resolution tier.
    pub price: &'static [(&'static str, f64)],
    /// CNY per second of a silent video (`audio: false`), where it differs.
    pub silent_price: &'static [(&'static str, f64)],
}

const HH_RATIOS: &[&str] = &["16:9", "9:16", "1:1", "4:3", "3:4", "4:5", "5:4", "9:21", "21:9"];
const WAN3_RATIOS: &[&str] = &["21:9", "16:9", "4:3", "1:1", "3:4", "9:16"];
const WAN_RATIOS: &[&str] = &["16:9", "9:16", "1:1", "4:3", "3:4"];
const R3: &[&str] = &["480P", "720P", "1080P"];
const R2: &[&str] = &["720P", "1080P"];

/// `size` of the older protocol, by tier and ratio.
const SIZES: &[(&str, &str, &str)] = &[
    ("480P", "16:9", "832*480"),
    ("480P", "9:16", "480*832"),
    ("480P", "1:1", "624*624"),
    ("720P", "16:9", "1280*720"),
    ("720P", "9:16", "720*1280"),
    ("720P", "1:1", "960*960"),
    ("720P", "4:3", "1088*832"),
    ("720P", "3:4", "832*1088"),
    ("1080P", "16:9", "1920*1080"),
    ("1080P", "9:16", "1080*1920"),
    ("1080P", "1:1", "1440*1440"),
    ("1080P", "4:3", "1632*1248"),
    ("1080P", "3:4", "1248*1632"),
];

/// CNY per second by resolution, China (Beijing) list prices of the model
/// pages ("模型价格", "原价", 2026-11-13; Wan 2.6 flash: with audio, as sent).
const HH11_PRICE: &[(&str, f64)] = &[("480P", 0.45), ("720P", 0.9), ("1080P", 1.2)];
const HH10_PRICE: &[(&str, f64)] = &[("720P", 0.9), ("1080P", 1.6)];
const WAN27_PRICE: &[(&str, f64)] = &[("720P", 0.6), ("1080P", 1.0)];
const WAN26_FLASH_PRICE: &[(&str, f64)] = &[("720P", 0.3), ("1080P", 0.5)];
const WAN26_FLASH_SILENT: &[(&str, f64)] = &[("720P", 0.15), ("1080P", 0.25)];
const WAN25_PRICE: &[(&str, f64)] = &[("480P", 0.3), ("720P", 0.6), ("1080P", 1.0)];
const WAN22_PLUS_PRICE: &[(&str, f64)] = &[("480P", 0.14), ("1080P", 0.7)];
const WAN22_FLASH_PRICE: &[(&str, f64)] = &[("480P", 0.1), ("720P", 0.2), ("1080P", 0.48)];

const HH11: Spec = Spec {
    id: "",
    api: Api::HhT2v,
    resolutions: R3,
    ratios: HH_RATIOS,
    seconds: Seconds::Range(3, 15),
    audio: true,
    audio_switch: false,
    price: HH11_PRICE,
    silent_price: &[],
};
const HH10: Spec = Spec { resolutions: R2, price: HH10_PRICE, ..HH11 };
const WAN27: Spec = Spec {
    id: "",
    api: Api::Wan27T2v,
    resolutions: R2,
    ratios: WAN_RATIOS,
    seconds: Seconds::Range(2, 15),
    audio: true,
    audio_switch: false,
    price: WAN27_PRICE,
    silent_price: &[],
};
const WAN26_T2V: Spec = Spec { api: Api::LegacyT2v, ..WAN27 };
const WAN26_I2V: Spec = Spec { api: Api::LegacyI2v, ratios: &[], ..WAN27 };
const WAN26_R2V: Spec = Spec { api: Api::LegacyR2v, seconds: Seconds::Range(2, 10), ..WAN27 };
const SILENT_5S: Spec = Spec {
    id: "",
    api: Api::LegacyT2v,
    resolutions: &["480P", "1080P"],
    ratios: WAN_RATIOS,
    seconds: Seconds::Fixed(5),
    audio: false,
    audio_switch: false,
    price: WAN22_PLUS_PRICE,
    silent_price: &[],
};

/// Alibaba's first-party video models in Beijing (2026-11-13).
pub const MODELS: &[Spec] = &[
    Spec { id: "happyhorse-1.1-t2v", ..HH11 },
    Spec { id: "happyhorse-1.1-i2v", api: Api::HhI2v, ratios: &[], ..HH11 },
    Spec { id: "happyhorse-1.1-r2v", api: Api::HhR2v, ..HH11 },
    Spec { id: "happyhorse-1.0-t2v", ..HH10 },
    Spec { id: "happyhorse-1.0-i2v", api: Api::HhI2v, ratios: &[], ..HH10 },
    Spec { id: "happyhorse-1.0-r2v", api: Api::HhR2v, ..HH10 },
    Spec { id: "happyhorse-1.0-video-edit", api: Api::HhEdit, ratios: &[], seconds: Seconds::Input, ..HH10 },
    Spec {
        id: "wan3.0-video",
        api: Api::Wan3,
        resolutions: R3,
        ratios: WAN3_RATIOS,
        seconds: Seconds::Range(2, 30),
        audio: true,
        audio_switch: true,
        price: &[("480P", 0.3), ("720P", 0.6), ("1080P", 1.2)],
        // "开关声音价格相同".
        silent_price: &[],
    },
    Spec {
        id: "wan3.0-video-prime",
        api: Api::Wan3,
        resolutions: R3,
        ratios: WAN3_RATIOS,
        seconds: Seconds::Range(2, 30),
        audio: true,
        audio_switch: true,
        price: &[("480P", 0.45), ("720P", 0.9), ("1080P", 1.8)],
        silent_price: &[],
    },
    Spec { id: "wan2.7-t2v", ..WAN27 },
    Spec { id: "wan2.7-t2v-2026-06-12", ..WAN27 },
    Spec { id: "wan2.7-t2v-2026-04-25", ..WAN27 },
    Spec { id: "wan2.7-i2v", api: Api::Wan27I2v, ratios: &[], ..WAN27 },
    Spec { id: "wan2.7-i2v-2026-04-25", api: Api::Wan27I2v, ratios: &[], ..WAN27 },
    Spec { id: "wan2.7-r2v", api: Api::Wan27R2v, ..WAN27 },
    Spec { id: "wan2.7-r2v-2026-06-12", api: Api::Wan27R2v, ..WAN27 },
    // "此参数仅在需要截断视频时才需配置": 2–10 s cuts the source.
    Spec { id: "wan2.7-videoedit", api: Api::Wan27Edit, seconds: Seconds::Range(2, 10), ..WAN27 },
    Spec { id: "wan2.6-t2v", ..WAN26_T2V },
    Spec { id: "wan2.6-i2v", ..WAN26_I2V },
    Spec {
        id: "wan2.6-i2v-flash",
        audio_switch: true,
        price: WAN26_FLASH_PRICE,
        silent_price: WAN26_FLASH_SILENT,
        ..WAN26_I2V
    },
    Spec { id: "wan2.6-r2v", ..WAN26_R2V },
    Spec {
        id: "wan2.6-r2v-flash",
        audio_switch: true,
        price: WAN26_FLASH_PRICE,
        silent_price: WAN26_FLASH_SILENT,
        ..WAN26_R2V
    },
    Spec {
        id: "wan2.5-t2v-preview",
        api: Api::LegacyT2v,
        resolutions: R3,
        seconds: Seconds::Set(&[5, 10]),
        price: WAN25_PRICE,
        ..WAN27
    },
    Spec {
        id: "wan2.5-i2v-preview",
        api: Api::LegacyI2v,
        resolutions: R3,
        ratios: &[],
        seconds: Seconds::Set(&[5, 10]),
        price: WAN25_PRICE,
        ..WAN27
    },
    Spec { id: "wan2.2-t2v-plus", ..SILENT_5S },
    Spec { id: "wan2.2-i2v-plus", api: Api::LegacyI2v, ratios: &[], ..SILENT_5S },
    Spec {
        id: "wan2.2-i2v-flash",
        api: Api::LegacyI2v,
        resolutions: R3,
        ratios: &[],
        price: WAN22_FLASH_PRICE,
        ..SILENT_5S
    },
    Spec {
        id: "wan2.2-kf2v-flash",
        api: Api::Kf2v,
        resolutions: R3,
        ratios: &[],
        price: WAN22_FLASH_PRICE,
        ..SILENT_5S
    },
    Spec { id: "wanx2.1-t2v-plus", resolutions: &["720P"], price: &[("720P", 0.7)], ..SILENT_5S },
    Spec {
        id: "wanx2.1-t2v-turbo",
        resolutions: &["480P", "720P"],
        price: &[("480P", 0.24), ("720P", 0.24)],
        ..SILENT_5S
    },
    Spec {
        id: "wanx2.1-i2v-plus",
        api: Api::LegacyI2v,
        resolutions: &["720P"],
        ratios: &[],
        price: &[("720P", 0.7)],
        ..SILENT_5S
    },
    Spec {
        id: "wanx2.1-i2v-turbo",
        api: Api::LegacyI2v,
        resolutions: &["480P", "720P"],
        ratios: &[],
        seconds: Seconds::Set(&[3, 4, 5]),
        price: &[("480P", 0.24), ("720P", 0.24)],
        ..SILENT_5S
    },
    Spec {
        id: "wanx2.1-kf2v-plus",
        api: Api::Kf2v,
        resolutions: &["720P"],
        ratios: &[],
        price: &[("720P", 0.7)],
        ..SILENT_5S
    },
];

pub fn spec(model: &str) -> Option<&'static Spec> {
    MODELS.iter().find(|m| m.id == model)
}

/// The protocol of a video id outside the table, by its family's naming.
fn api_by_name(model: &str) -> Option<Api> {
    if let Some(rest) = model.strip_prefix("happyhorse-") {
        return Some(if rest.ends_with("-video-edit") {
            Api::HhEdit
        } else if rest.ends_with("-i2v") {
            Api::HhI2v
        } else if rest.ends_with("-r2v") {
            Api::HhR2v
        } else {
            Api::HhT2v
        });
    }
    if model.starts_with("wan3") && model.contains("-video") {
        return Some(Api::Wan3);
    }
    let rest = model.strip_prefix("wan2.7-")?;
    Some(if rest.starts_with("t2v") {
        Api::Wan27T2v
    } else if rest.starts_with("i2v") {
        Api::Wan27I2v
    } else if rest.starts_with("r2v") {
        Api::Wan27R2v
    } else if rest.starts_with("videoedit") {
        Api::Wan27Edit
    } else {
        return None;
    })
}

/// Whether `model` is a video model of this API (the table, or the families).
pub fn is_video_model(model: &str) -> bool {
    spec(model).is_some()
        || model.starts_with("happyhorse")
        || (model.starts_with("wan")
            && ["-t2v", "-i2v", "-r2v", "-kf2v", "-video", "-vace", "-animate", "-s2v"]
                .iter()
                .any(|k| model.contains(k)))
        || matches!(model, "video-style-transform" | "videoretalk")
}

/// What `/model` shows for a video model of the table.
pub fn card(s: &'static Spec) -> crate::card::ModelCard {
    use crate::card::{self, LIMITS, OUTPUT, PRICE};
    let mut c = card::ModelCard::default();
    let length = match s.seconds {
        Seconds::Range(a, b) => format!("{a} to {b} seconds"),
        Seconds::Set(v) => format!("{} seconds", v.iter().map(u32::to_string).collect::<Vec<_>>().join(", ")),
        Seconds::Fixed(n) => format!("always {n} seconds"),
        Seconds::Input => "the input video's".to_string(),
    };
    c.row(LIMITS, "length", length);
    c.caps(&Caps(Some(s)));
    c.row(OUTPUT, "resolutions", format!("{} (the last is the default)", s.resolutions.join(", ")));
    if !s.ratios.is_empty() {
        c.row(OUTPUT, "aspect ratios", s.ratios.join(", "));
    }
    let sound = match (s.audio, s.audio_switch) {
        (true, true) => "on (audio=off turns it off)",
        (true, false) => "always",
        (false, _) => "none",
    };
    c.row(OUTPUT, "sound", sound);
    for (r, cny) in s.price {
        c.row(PRICE, r.to_string(), format!("{} per second", card::cny(*cny)));
    }
    for (r, cny) in s.silent_price {
        c.row(PRICE, format!("{r} silent"), format!("{} per second", card::cny(*cny)));
    }
    c
}

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
        Generation { seconds: self.0.is_some(), audio: self.0.is_some_and(|s| s.audio_switch), ..Default::default() }
    }
}

fn err(m: impl Into<String>) -> LlmError {
    LlmError::request(m)
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

fn src(by_user: bool) -> String {
    if by_user { "option" } else { "default" }.into()
}

fn has_options(req: &Requested) -> bool {
    let mut r = req.clone();
    if matches!(r.tier.as_deref(), Some("default" | "auto")) {
        r.tier = None;
    }
    r != Requested::default()
}

// ------------------------------------------------------------------ inputs

#[derive(Debug, Default)]
struct Inputs {
    prompt: String,
    /// Data URLs, the replied-to answer's images first.
    images: Vec<String>,
    answer_images: usize,
    /// The URL of the Alibaba video replied to.
    video: Option<String>,
    /// Why the video of the answer replied to cannot be passed.
    video_problem: Option<String>,
    notes: Vec<String>,
}

/// Image types DashScope's video models take (HappyHorse: no BMP).
fn image_type(mime: &str, api: Option<Api>) -> Option<&'static str> {
    let t = match media::base_mime(mime).as_str() {
        "image/jpeg" | "image/jpg" => "image/jpeg",
        "image/png" => "image/png",
        "image/webp" => "image/webp",
        "image/bmp" | "image/x-ms-bmp" => "image/bmp",
        _ => return None,
    };
    let happyhorse = matches!(api, Some(Api::HhI2v | Api::HhR2v | Api::HhEdit));
    (!(happyhorse && t == "image/bmp")).then_some(t)
}

async fn image_url(
    blob: &omni_core::BlobRef,
    api: Option<Api>,
    model: &str,
    blobs: &dyn BlobLoader,
) -> Result<String, LlmError> {
    let Some(t) = image_type(&blob.mime, api) else {
        let bmp = if matches!(api, Some(Api::HhI2v | Api::HhR2v | Api::HhEdit)) { "" } else { ", bmp" };
        return Err(err(format!("`{model}` does not take {} images; formats: jpeg, png, webp{bmp}", blob.mime)));
    };
    let max = if api.is_some_and(Api::old) && !model.starts_with("wan2.6") && !model.starts_with("wan2.5") {
        MAX_IMAGE_OLD
    } else {
        MAX_IMAGE
    };
    if blob.size > max {
        return Err(err(format!(
            "an image is too large for `{model}` ({} bytes; at most {} MB)",
            blob.size,
            max / (1024 * 1024)
        )));
    }
    let bytes = crate::provider::load(blobs, blob).await?;
    Ok(crate::provider::data_url(t, &bytes))
}

/// Days since 1970-01-01 of a calendar date.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + d - 1;
    era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468
}

/// `2026-04-20 17:56:36.658` (Beijing time, as DashScope writes its task
/// times) as a Unix time.
fn beijing_time(s: &str) -> Option<i64> {
    let (date, time) = s.trim().split_once(' ')?;
    let mut d = date.split('-').map(|x| x.parse::<i64>().ok());
    let (y, m, day) = (d.next()??, d.next()??, d.next()??);
    let mut t = time.split(['.', ':']).map(|x| x.parse::<i64>().ok());
    let (h, mi, sec) = (t.next()??, t.next()??, t.next()??);
    Some(days_from_civil(y, m, day) * 86_400 + h * 3600 + mi * 60 + sec - 8 * 3600)
}

/// The URL of an Alibaba video answer and whether it still works: its signed
/// URL's `Expires`, else 24 hours from the task's end.
fn answer_video_url(task: &Value, now: i64) -> Result<String, &'static str> {
    let url = task.pointer("/output/video_url").and_then(Value::as_str).filter(|u| !u.is_empty());
    let Some(url) = url else { return Err("its task has no video URL") };
    let expires = url.split(['?', '&']).find_map(|kv| kv.strip_prefix("Expires=")).and_then(|v| v.parse::<i64>().ok());
    let ended = task.pointer("/output/end_time").and_then(Value::as_str).and_then(beijing_time);
    let expired = match (expires, ended) {
        (Some(e), _) => now >= e,
        (None, Some(t)) => now - t >= URL_VALID_SECS,
        (None, None) => false,
    };
    if expired {
        return Err("its URL has expired (DashScope keeps generated videos 24 hours)");
    }
    Ok(url.to_string())
}

async fn inputs(
    history: &[Turn],
    model: &str,
    api: Option<Api>,
    now: i64,
    blobs: &dyn BlobLoader,
) -> Result<Inputs, LlmError> {
    let Some((last, earlier)) = history.split_last() else {
        return Err(err(format!("`{model}` needs a prompt")));
    };
    if history.iter().any(|t| t.role == Role::System && !t.answer_text().trim().is_empty()) {
        return Err(err(format!("`{model}` takes no system prompt: Alibaba's video API has a prompt and media only")));
    }
    if last.role != Role::User {
        return Err(err(format!(
            "`{model}` takes a prompt, not an assistant prefill (the conversation must end with a user message)"
        )));
    }
    let mut ins = Inputs::default();
    if let Some(prev) = earlier.last().filter(|t| t.role == Role::Assistant) {
        for p in &prev.parts {
            match p {
                Part::Image { blob, generated: true, thought: false, .. } => {
                    ins.images.push(image_url(blob, api, model, blobs).await?);
                }
                Part::File { blob, filename, generated: true }
                    if classify_mime(&blob.mime, filename.as_deref()) == MediaClass::Video =>
                {
                    match prev.native.as_ref().filter(|n| n.dialect == DIALECT) {
                        Some(n) => match answer_video_url(&n.payload, now) {
                            Ok(url) => ins.video = Some(url),
                            Err(why) => {
                                ins.video_problem = Some(format!(
                                    "the video replied to cannot be passed to `{model}`: {why}, and DashScope takes \
                                     videos only as URLs (the bot uploads nothing)"
                                ));
                            }
                        },
                        None => {
                            ins.video_problem = Some(format!(
                                "`{model}` takes videos only as URLs, and the bot uploads nothing: it can pass the \
                                 video of an Alibaba answer (in the last 24 hours), not this one{}",
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
    let edit_chain = ins.answer_images > 0 || ins.video.is_some() || ins.video_problem.is_some();
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
            Part::Image { blob, .. } => ins.images.push(image_url(blob, api, model, blobs).await?),
            Part::File { blob, filename, .. } => {
                let name = filename.clone().unwrap_or_else(|| blob.mime.clone());
                match classify_mime(&blob.mime, filename.as_deref()) {
                    MediaClass::Image => ins.images.push(image_url(blob, api, model, blobs).await?),
                    MediaClass::Video | MediaClass::Audio => {
                        return Err(err(format!(
                            "`{model}` takes {} only as URLs (public, or DashScope's temporary storage), and the bot \
                             uploads nothing ({name}){}",
                            if classify_mime(&blob.mime, filename.as_deref()) == MediaClass::Video {
                                "videos"
                            } else {
                                "audio"
                            },
                            if classify_mime(&blob.mime, filename.as_deref()) == MediaClass::Video {
                                ": reply to a video Alibaba made in the last 24 hours to use it"
                            } else {
                                ""
                            }
                        )));
                    }
                    _ => {
                        return Err(err(format!(
                            "`{model}` takes text and images (and its own videos) only: {name} is {}",
                            blob.mime
                        )));
                    }
                }
            }
            Part::RemoteMedia { url, .. } => {
                return Err(err(format!("`{model}` cannot read remote media on Alibaba's video API ({url})")));
            }
            Part::Location { .. } => return Err(crate::provider::no_location(model, NAME)),
            Part::Reasoning(_) | Part::ToolCall(_) | Part::Refusal { .. } => {}
        }
    }
    ins.prompt = texts.join("\n");
    Ok(ins)
}

/// The one http(s) URL of a text (`+u`: a web page for Wan 3.0).
fn the_url(text: &str, model: &str) -> Result<String, LlmError> {
    let urls: Vec<&str> = text
        .split(|c: char| c.is_whitespace() || "<>\"'()[]（）【】，。".contains(c))
        .filter(|w| w.starts_with("http://") || w.starts_with("https://"))
        .collect();
    match urls.as_slice() {
        [u] => Ok(u.to_string()),
        [] => Err(err(format!("`+u` sends the message's web page to `{model}`, but the message has no URL"))),
        _ => Err(err(format!("`{model}` reads one web page (`+u`); the message has {} URLs", urls.len()))),
    }
}

// ------------------------------------------------------------------ requests

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Text,
    FirstFrame,
    FirstLast,
    /// Images (and a video, a web page) as references.
    References,
    /// Continuing the video replied to (Wan 2.7 `first_clip`, an image as its last frame).
    Continue,
    /// Editing the video replied to (with reference images).
    Edit,
}

fn mode_for(api: Api, ins: &Inputs, link: bool, model: &str) -> Result<Mode, LlmError> {
    let n = ins.images.len();
    let video = ins.video.is_some();
    let no_video = |m: Mode| {
        if video {
            Err(err(format!(
                "`{model}` takes no video: reply to the video with a model that edits or continues videos \
                 (happyhorse-1.0-video-edit, wan2.7-videoedit, wan2.7-i2v, wan2.7-r2v, wan3.0-video)"
            )))
        } else {
            Ok(m)
        }
    };
    match api {
        Api::HhT2v | Api::Wan27T2v | Api::LegacyT2v => {
            if n > 0 {
                return Err(err(format!(
                    "`{model}` is text-to-video: it takes no images (its image-to-video and reference models do)"
                )));
            }
            no_video(Mode::Text)
        }
        Api::HhI2v | Api::LegacyI2v => match n {
            0 => Err(err(format!("`{model}` needs an image (the first frame): attach or reply to one"))),
            1 => no_video(Mode::FirstFrame),
            _ => Err(err(format!("`{model}` takes one image (the first frame), not {n}"))),
        },
        Api::Kf2v => match n {
            0 => Err(err(format!("`{model}` needs a first frame image (and optionally a last frame)"))),
            1 => no_video(Mode::FirstFrame),
            2 => no_video(Mode::FirstLast),
            _ => Err(err(format!("`{model}` takes a first and a last frame, not {n} images"))),
        },
        Api::HhR2v => match n {
            0 => Err(err(format!("`{model}` needs 1–9 reference images"))),
            1..=9 => no_video(Mode::References),
            _ => Err(err(format!("`{model}` takes at most 9 reference images ({n} given)"))),
        },
        Api::LegacyR2v => {
            if n > 0 {
                return Err(err(format!(
                    "`{model}` takes references only as URLs, not base64 images, and the bot uploads nothing: reply \
                     to a video Alibaba made (wan2.7-r2v takes images)"
                )));
            }
            if !video {
                return Err(err(format!("`{model}` needs a reference: reply to a video Alibaba made")));
            }
            Ok(Mode::References)
        }
        Api::Wan27R2v => {
            let total = n + usize::from(video);
            match total {
                0 => Err(err(format!("`{model}` needs 1–5 references (images, or a video replied to)"))),
                1..=5 => Ok(Mode::References),
                _ => Err(err(format!("`{model}` takes at most 5 references ({total} given)"))),
            }
        }
        Api::Wan27I2v => match (video, n) {
            (true, 0 | 1) => Ok(Mode::Continue),
            (true, _) => Err(err(format!("`{model}` continues a video with at most one image (its last frame)"))),
            (false, 0) => Err(err(format!("`{model}` needs a first frame image, or a video replied to"))),
            (false, 1) => Ok(Mode::FirstFrame),
            (false, 2) => Ok(Mode::FirstLast),
            (false, _) => Err(err(format!("`{model}` takes a first and a last frame, not {n} images"))),
        },
        Api::HhEdit | Api::Wan27Edit => {
            let max = if api == Api::HhEdit { 5 } else { 4 };
            if !video {
                return Err(err(format!(
                    "`{model}` edits a video: reply to a video Alibaba made in the last 24 hours"
                )));
            }
            if n > max {
                return Err(err(format!("`{model}` takes at most {max} reference images ({n} given)")));
            }
            Ok(Mode::Edit)
        }
        Api::Wan3 => {
            if video || link || n >= 3 || (n > 0 && names_images(&ins.prompt)) {
                if n > 10 {
                    return Err(err(format!("`{model}` takes at most 10 reference images ({n} given)")));
                }
                Ok(Mode::References)
            } else {
                Ok(match n {
                    0 => Mode::Text,
                    1 => Mode::FirstFrame,
                    _ => Mode::FirstLast,
                })
            }
        }
    }
}

fn describe(mode: Mode, ins: &Inputs, link: bool) -> String {
    let previous =
        |what: &str| if ins.answer_images > 0 { format!("{what} (the previous image)") } else { what.into() };
    let n = ins.images.len();
    match mode {
        Mode::Text => "text-to-video".into(),
        Mode::FirstFrame => format!("image-to-video: {}", previous("first frame")),
        Mode::FirstLast => format!("image-to-video: {} + last frame", previous("first frame")),
        Mode::References => {
            let mut parts = Vec::new();
            if ins.video.is_some() {
                parts.push("the previous video".to_string());
            }
            if n > 0 {
                parts.push(format!("{n} image{}", plural(n)));
            }
            if link {
                parts.push("a web page".into());
            }
            format!("reference-to-video: {}", parts.join(" + "))
        }
        Mode::Continue => {
            if n > 0 {
                "continuing the previous video to a last frame".into()
            } else {
                "continuing the previous video".into()
            }
        }
        Mode::Edit => {
            if n > 0 {
                format!("video edit of the previous video + {n} reference image{}", plural(n))
            } else {
                "video edit of the previous video".into()
            }
        }
    }
}

fn tier_for(size: &str) -> Option<&'static str> {
    match size {
        "1K" => Some("720P"),
        "2K" => Some("1080P"),
        _ => None,
    }
}

fn size_letters(spec: &Spec) -> String {
    let list: Vec<String> = [("1K", "720P"), ("2K", "1080P")]
        .iter()
        .filter(|(_, t)| spec.resolutions.contains(t))
        .map(|(l, t)| format!("{l} ({t})"))
        .collect();
    if list.is_empty() { "none (480P only)".into() } else { list.join(", ") }
}

/// A `media` item.
fn item(ty: &str, url: &str) -> Value {
    json!({"type": ty, "url": url})
}

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
    let spec = spec(model);
    let Some(api) = spec.map(|s| s.api).or_else(|| api_by_name(model)) else {
        return Err(err(format!(
            "`{model}` is not in the bot's Alibaba video model table and its API is unknown: use one of the listed \
             HappyHorse or Wan video models"
        )));
    };
    let link = req.tools.contains(&Tool::Fetch);
    for &t in &req.tools {
        match t {
            Tool::Fetch if api == Api::Wan3 => {}
            Tool::Image => {
                return Err(err(format!(
                    "`{model}` is a video model: it makes videos without `+i` (the image tool of chat models)"
                )));
            }
            Tool::Fetch => {
                return Err(err(format!(
                    "`{model}` reads no web pages (`+u`) on Alibaba's video API (wan3.0-video does)"
                )));
            }
            t => {
                return Err(err(format!(
                    "`{model}` has no {} (`+{}`) on Alibaba's video API",
                    t.describe(),
                    t.letter()
                )));
            }
        }
    }
    if spec.is_none() && has_options(&req) {
        return Err(err(format!(
            "`{model}` is not in the bot's Alibaba video model table, so its options cannot be checked: send it \
             without options"
        )));
    }
    let mut basic = req.clone();
    basic.tools.clear();
    basic.image_size = None;
    basic.aspect_ratio = None;
    request::validate(model, &basic, &Caps(spec)).map_err(LlmError::request)?;
    let now = unix_now();
    let ins = inputs(input.history, model, Some(api), now, blobs).await?;
    if let Some(why) = &ins.video_problem {
        return Err(err(why.clone()));
    }
    let mode = mode_for(api, &ins, link, model)?;
    let needs_prompt =
        matches!(mode, Mode::Text) || matches!(api, Api::HhR2v | Api::HhEdit | Api::Wan27R2v | Api::LegacyR2v);
    if needs_prompt && ins.prompt.is_empty() {
        return Err(err(format!("`{model}` needs a prompt (the text of the message)")));
    }
    let link_url = if link { Some(the_url(&ins.prompt, model)?) } else { None };

    let mut notes = ins.notes.clone();
    let mut warnings = Vec::new();
    let mut unused: Vec<String> = Vec::new();
    let mut why_unused = "";
    let mut parameters = Map::new();
    let mut applied = Vec::new();
    let mut settings = vec![describe(mode, &ins, link)];
    let mut set = |k: &str, v: Value, source: String, parameters: &mut Map<String, Value>| {
        parameters.insert(k.into(), v.clone());
        let mut patch = Map::new();
        patch.insert(k.into(), v);
        applied.push(Applied { source, patch: json!({"parameters": patch}) });
    };

    if let Some(s) = spec {
        // Resolution (or the older `size`, with the ratio).
        let tier = match req.image_size.as_deref() {
            None => s.resolutions.last().copied().unwrap_or("1080P"),
            Some(z) => match tier_for(z).filter(|t| s.resolutions.contains(t)) {
                Some(t) => t,
                None => {
                    return Err(err(format!(
                        "video size {z} is not available for `{model}`; sizes: {}",
                        size_letters(s)
                    )));
                }
            },
        };
        let ratio = match &req.aspect_ratio {
            Some(r) if s.api.sized() => {
                if !SIZES.iter().any(|(t, x, _)| *t == tier && x == r) {
                    let here: Vec<&str> = SIZES.iter().filter(|(t, _, _)| *t == tier).map(|(_, x, _)| *x).collect();
                    return Err(err(format!(
                        "aspect ratio {r} is not available for `{model}` at {tier}; ratios: {}",
                        here.join(", ")
                    )));
                }
                Some(r.clone())
            }
            Some(r) if s.api.ratio() => {
                if !s.ratios.contains(&r.as_str()) {
                    return Err(err(format!(
                        "aspect ratio {r} is not available for `{model}`; ratios: {}",
                        s.ratios.join(", ")
                    )));
                }
                if api == Api::Wan3 && ins.video.is_some() {
                    unused.push(format!("aspect ratio {r}"));
                    why_unused = "Wan keeps the source video's aspect ratio in an edit or an extension";
                    None
                } else {
                    Some(r.clone())
                }
            }
            Some(r) => {
                unused.push(format!("aspect ratio {r}"));
                why_unused = if s.api.edits() {
                    "an edit keeps the source video's aspect ratio"
                } else if mode == Mode::Continue {
                    "a continued video keeps its aspect ratio"
                } else {
                    "image-to-video takes the image's aspect ratio"
                };
                None
            }
            None => None,
        };
        if s.api.sized() {
            let r = ratio.as_deref().unwrap_or("16:9");
            let size = SIZES.iter().find(|(t, x, _)| *t == tier && *x == r).map(|(_, _, z)| *z).unwrap_or("1920*1080");
            set("size", json!(size), src(user.image_size.is_some() || user.aspect_ratio.is_some()), &mut parameters);
            settings.push(format!("size {size} ({tier} {r})"));
        } else {
            set("resolution", json!(tier), src(user.image_size.is_some()), &mut parameters);
            settings.push(format!("resolution {tier}"));
            if let Some(r) = &ratio {
                set("ratio", json!(r), src(user.aspect_ratio.is_some()), &mut parameters);
                settings.push(format!("aspect ratio {r}"));
            }
        }
        // Length.
        if let Some(sec) = req.seconds {
            let video_refs = api == Api::Wan27R2v && ins.video.is_some();
            let ok = match s.seconds {
                Seconds::Range(a, b) => (a..=if video_refs { b.min(10) } else { b }).contains(&sec),
                Seconds::Set(v) => v.contains(&sec),
                Seconds::Fixed(_) | Seconds::Input => false,
            };
            if !ok {
                return Err(err(match s.seconds {
                    Seconds::Range(a, b) => format!(
                        "`{model}` makes videos of {a}–{} s{} (sec={sec})",
                        if video_refs { b.min(10) } else { b },
                        if video_refs { " with a reference video" } else { "" }
                    ),
                    Seconds::Set(v) => format!(
                        "`{model}` makes videos of {} s (sec={sec})",
                        v.iter().map(u32::to_string).collect::<Vec<_>>().join(", ")
                    ),
                    Seconds::Fixed(f) => format!("`{model}` makes {f}-second videos only (no `sec=`)"),
                    Seconds::Input => format!("`{model}` keeps the source video's length (no `sec=`)"),
                }));
            }
            if api == Api::Wan3 && ins.video.is_some() {
                unused.push(format!("sec={sec}"));
                why_unused = "Wan keeps the source's length in an edit and picks the length of an extension";
            } else {
                set("duration", json!(sec), src(user.seconds.is_some()), &mut parameters);
                settings.push(format!("{sec} s"));
            }
        }
        let silent = s.audio_switch && req.audio == Some(false);
        if s.audio_switch {
            set("audio", json!(!silent), src(user.audio.is_some()), &mut parameters);
        }
        if s.audio && !silent {
            settings.push("with audio".into());
        } else if silent {
            settings.push("no audio".into());
        }
        set("watermark", json!(false), "default".into(), &mut parameters);
        settings.push("no watermark".into());
    } else {
        // Outside the table: no option, only what every model of the family takes.
        set("watermark", json!(false), "default".into(), &mut parameters);
        settings.push("no watermark".into());
        warnings.push(format!(
            "`{model}` is not in the bot's Alibaba video model table: resolutions, lengths and price unknown \
             (Alibaba's defaults; cost not estimated)"
        ));
    }
    if !unused.is_empty() {
        notes.push(format!("ℹ️ {} not sent: {why_unused}", unused.join(", ")));
    }

    // Input.
    let first = |i: usize| ins.images[i].clone();
    let mut input_obj = Map::new();
    if !ins.prompt.is_empty() {
        input_obj.insert("prompt".into(), json!(ins.prompt));
    }
    let video = ins.video.clone().unwrap_or_default();
    match api {
        Api::LegacyT2v | Api::HhT2v | Api::Wan27T2v => {}
        Api::LegacyI2v => {
            input_obj.insert("img_url".into(), json!(first(0)));
        }
        Api::Kf2v => {
            input_obj.insert("first_frame_url".into(), json!(first(0)));
            if mode == Mode::FirstLast {
                input_obj.insert("last_frame_url".into(), json!(first(1)));
            }
        }
        Api::LegacyR2v => {
            input_obj.insert("reference_urls".into(), json!([video]));
        }
        _ => {
            let mut media = Vec::new();
            match mode {
                Mode::Text => {}
                Mode::FirstFrame => media.push(item("first_frame", &ins.images[0])),
                Mode::FirstLast => {
                    media.push(item("first_frame", &ins.images[0]));
                    media.push(item("last_frame", &ins.images[1]));
                }
                Mode::References => {
                    if ins.video.is_some() {
                        media.push(item("reference_video", &video));
                    }
                    media.extend(ins.images.iter().map(|u| item("reference_image", u)));
                    if let Some(u) = &link_url {
                        media.push(item("link", u));
                    }
                }
                Mode::Continue => {
                    media.push(item("first_clip", &video));
                    if let Some(u) = ins.images.first() {
                        media.push(item("last_frame", u));
                    }
                }
                Mode::Edit => {
                    media.push(item("video", &video));
                    media.extend(ins.images.iter().map(|u| item("reference_image", u)));
                }
            }
            if !media.is_empty() {
                input_obj.insert("media".into(), Value::Array(media));
            }
        }
    }
    let path = if api == Api::Kf2v { IMAGE2VIDEO } else { SYNTHESIS };
    let body = json!({"model": model, "input": input_obj, "parameters": parameters});
    Ok(Prepared {
        dialect: DIALECT,
        model: model.to_string(),
        url: format!("{}{path}", native_root(&cfg.base_url)),
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

/// The request that resumes a task from its checkpoint (`{"task_id"}`):
/// polling `GET {root}/api/v1/tasks/{id}` instead of creating another video
/// (no body: nothing is sent).
pub fn continuation_request(p: &Prepared, item: &Value) -> Prepared {
    let mut q = p.clone();
    if let Some(id) = item.get("task_id").and_then(Value::as_str) {
        let root = p.url.find("/api/v1/").map_or(p.url.as_str(), |i| &p.url[..i]);
        q.url = format!("{root}{TASKS}/{id}");
        q.body = Value::Null;
    }
    q
}

// ------------------------------------------------------------------ running

fn identity(e: LlmError) -> LlmError {
    e
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
        map_error: error_from_json,
        refine: identity,
        observer,
        cancel,
    }
}

/// What the tool call shows of a request: the prompt, the media by type (no
/// data; a web page by its URL) and the parameters.
fn tool_input(body: &Value) -> Value {
    let mut o = Map::new();
    let input = body.get("input").cloned().unwrap_or(Value::Null);
    if let Some(p) = input.get("prompt") {
        o.insert("prompt".into(), p.clone());
    }
    let mut kinds: Vec<(String, usize)> = Vec::new();
    let mut count = |k: &str| match kinds.iter_mut().find(|(x, _)| x == k) {
        Some((_, n)) => *n += 1,
        None => kinds.push((k.to_string(), 1)),
    };
    for m in input.get("media").and_then(Value::as_array).into_iter().flatten() {
        match m.get("type").and_then(Value::as_str) {
            Some("link") => {
                if let Some(u) = m.get("url") {
                    o.insert("link".into(), u.clone());
                }
            }
            Some(t) => count(t),
            None => {}
        }
    }
    for k in ["img_url", "first_frame_url", "last_frame_url"] {
        if input.get(k).is_some() {
            count(k);
        }
    }
    if let Some(r) = input.get("reference_urls").and_then(Value::as_array) {
        for _ in r {
            count("reference_urls");
        }
    }
    for (k, n) in kinds {
        o.insert(k, json!(n));
    }
    if let Some(params) = body.get("parameters").and_then(Value::as_object) {
        for (k, v) in params {
            o.insert(k.clone(), v.clone());
        }
    }
    Value::Object(o)
}

/// What the tool call shows of a finished task: what DashScope says about the
/// video (the rewritten prompt, the billed length and resolution), no URL,
/// ids, times or the original prompt.
fn tool_output(task: &Value) -> Value {
    let mut o = task.get("output").and_then(Value::as_object).cloned().unwrap_or_default();
    for k in ["task_id", "task_status", "video_url", "submit_time", "scheduled_time", "end_time", "orig_prompt"] {
        o.remove(k);
    }
    if let Some(u) = task.get("usage").filter(|u| u.is_object()) {
        o.insert("usage".into(), u.clone());
    }
    Value::Object(o)
}

fn call(name: &str, id: &str, status: &str, input: Option<Value>, output: Option<Value>) -> StreamEvent {
    StreamEvent::Tool(ToolCall {
        id: Some(id.to_string()),
        name: name.into(),
        status: Some(status.to_string()),
        input,
        output,
    })
}

/// The error of a task that ended without a video.
fn task_error(status: &str, task: &Value) -> LlmError {
    let out = task.get("output").cloned().unwrap_or(Value::Null);
    let has_code = out.get("code").and_then(Value::as_str).is_some_and(|c| !c.is_empty());
    let mut e = if has_code {
        error_from_json(&out, None, None, ErrorKind::InBand)
    } else {
        match status {
            "UNKNOWN" => LlmError::new(
                ErrorKind::InBand,
                Retry::Backoff,
                "DashScope does not know the video task (task ids are kept 24 hours)",
            ),
            "CANCELED" => LlmError::new(ErrorKind::InBand, Retry::Never, "the video task was cancelled"),
            _ => LlmError::new(ErrorKind::InBand, Retry::Never, format!("the video task ended with status {status}")),
        }
    };
    if e.error_type.is_none() {
        e.error_type = Some(status.to_string());
    }
    e.details = Some(task.clone());
    e
}

/// Run a video task: create it (unless resuming one), checkpoint its id, poll
/// it, download the video. `async_client` sends `X-DashScope-Async: enable`.
pub async fn stream(
    client: &reqwest::Client,
    async_client: &reqwest::Client,
    cfg: &DirectConfig,
    p: &Prepared,
    sink: &mut dyn EventSink,
    observer: &dyn Observer,
    cancel: &CancellationToken,
) -> (Result<Value, LlmError>, CallOutcome) {
    let h = http(client, cfg, observer, cancel);
    let mut outcome = CallOutcome::default();
    let name = if spec(&p.model).map(|s| s.api).or_else(|| api_by_name(&p.model)).is_some_and(Api::edits) {
        "video_edit"
    } else {
        "video_generation"
    };
    let mut shown = String::new();
    let (id, poll_url) = if p.body.is_null() {
        (p.url.rsplit('/').next().unwrap_or_default().to_string(), p.url.clone())
    } else {
        let reply = match http(async_client, cfg, observer, cancel).post(&p.url, &p.body).await {
            Ok(r) => r,
            Err(e) => return (Err(e), outcome),
        };
        let body = reply.body;
        let id = body.pointer("/output/task_id").and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string);
        let Some(id) = id else {
            let e = if body.get("code").and_then(Value::as_str).is_some_and(|c| !c.is_empty()) {
                error_from_json(&body, None, None, ErrorKind::InBand).with_details(body.clone())
            } else {
                LlmError::new(ErrorKind::Decode, Retry::Backoff, "DashScope created no task (no `output.task_id`)")
                    .with_details(body)
            };
            return (Err(e), outcome);
        };
        sink.event(StreamEvent::Meta { upstream_id: Some(id.clone()), provider: None, model: None });
        let status = body.pointer("/output/task_status").and_then(Value::as_str).unwrap_or("PENDING").to_string();
        sink.event(call(name, &id, &status, Some(tool_input(&p.body)), None));
        shown = status;
        let item = json!({"task_id": id});
        sink.event(StreamEvent::Checkpoint(item.clone()));
        outcome.continuation = Some(item);
        let root = p.url.find("/api/v1/").map_or(p.url.as_str(), |i| &p.url[..i]);
        let poll = format!("{root}{TASKS}/{id}");
        (id, poll)
    };
    outcome.upstream_id = Some(id.clone());
    loop {
        let task = match h.get(&poll_url).await {
            Ok(r) => r.body,
            Err(e) => return (Err(e), outcome),
        };
        let status = task.pointer("/output/task_status").and_then(Value::as_str).unwrap_or("PENDING").to_string();
        match status.as_str() {
            "SUCCEEDED" => return (finish(&h, cfg, p, name, &id, &task, sink).await, outcome),
            "FAILED" | "CANCELED" | "UNKNOWN" => {
                let out = task.get("output").cloned().unwrap_or(Value::Null);
                let shown_error = out.get("code").map(|_| {
                    let mut o = Map::new();
                    for k in ["code", "message"] {
                        if let Some(v) = out.get(k) {
                            o.insert(k.into(), v.clone());
                        }
                    }
                    json!({"error": o})
                });
                sink.event(call(name, &id, &status, None, shown_error));
                sink.event(StreamEvent::Checkpoint(Value::Null));
                outcome.continuation = None;
                return (Err(task_error(&status, &task)), outcome);
            }
            _ => {
                if status != shown {
                    sink.event(call(name, &id, &status, None, None));
                    shown = status;
                }
                if let Err(e) = media::pause(POLL_EVERY, cancel).await {
                    return (Err(e), outcome);
                }
            }
        }
    }
}

async fn finish(
    h: &Http<'_>,
    cfg: &DirectConfig,
    p: &Prepared,
    name: &str,
    id: &str,
    task: &Value,
    sink: &mut dyn EventSink,
) -> Result<Value, LlmError> {
    let Some(url) = task.pointer("/output/video_url").and_then(Value::as_str).filter(|u| !u.is_empty()) else {
        let mut e = LlmError::new(ErrorKind::InBand, Retry::Never, "the video task succeeded without a video URL");
        e.details = Some(task.clone());
        return Err(e);
    };
    // Billed once the video is made: counted even if downloading it fails.
    sink.event(StreamEvent::Usage(usage(p, task, cfg.cny_per_usd)));
    let (data, mime) = h.download(url).await?;
    let mime = mime.filter(|m| m.starts_with("video/")).unwrap_or_else(|| "video/mp4".into());
    sink.event(call(name, id, "SUCCEEDED", None, Some(tool_output(task))));
    // The output's own length (the billed one adds an input video).
    let u = task.get("usage").unwrap_or(&Value::Null);
    let secs =
        ["output_video_duration", "video_duration", "duration"].iter().find_map(|k| u.get(*k).and_then(Value::as_f64));
    let file = match secs.filter(|s| *s > 0.0) {
        Some(s) if s.fract() == 0.0 => format!("{}-{s}s.mp4", p.model),
        Some(s) => format!("{}-{s:.1}s.mp4", p.model),
        None => format!("{}.mp4", p.model),
    };
    sink.event(StreamEvent::File { data, mime, name: Some(file) });
    sink.event(StreamEvent::Finish { reason: Some("stop".into()), native: Some("SUCCEEDED".into()) });
    Ok(task.clone())
}

/// The seconds DashScope bills: `duration` (input video + output), else
/// `video_duration` (older models), else the output's.
fn billed_seconds(u: &Value) -> Option<f64> {
    ["duration", "video_duration", "output_video_duration"].iter().find_map(|k| u.get(*k).and_then(Value::as_f64))
}

/// The resolution tier of a finished task: `usage.SR`, else the request's.
fn billed_tier(task: &Value, body: &Value) -> Option<String> {
    let u = task.get("usage").cloned().unwrap_or(Value::Null);
    if let Some(sr) = u.get("SR").and_then(Value::as_u64) {
        return Some(format!("{sr}P"));
    }
    let params = body.get("parameters").cloned().unwrap_or(Value::Null);
    if let Some(r) = params.get("resolution").and_then(Value::as_str) {
        return Some(r.to_string());
    }
    let size = u.get("size").or_else(|| params.get("size")).and_then(Value::as_str)?;
    SIZES.iter().find(|(_, _, z)| *z == size).map(|(t, _, _)| t.to_string())
}

/// CNY of a finished task: its billed seconds at the price of its resolution
/// (the highest when unknown), silent or with audio.
pub fn cost_cny(spec: &Spec, seconds: f64, tier: Option<&str>, silent: bool) -> f64 {
    let table = if silent && !spec.silent_price.is_empty() { spec.silent_price } else { spec.price };
    let top = table.last().map_or(0.0, |(_, p)| *p);
    let per = tier.and_then(|t| table.iter().find(|(x, _)| x.eq_ignore_ascii_case(t))).map_or(top, |(_, p)| *p);
    seconds * per
}

fn usage(p: &Prepared, task: &Value, cny_per_usd: f64) -> Usage {
    let u = task.get("usage").cloned().unwrap_or(Value::Null);
    let cost = spec(&p.model)
        .zip(billed_seconds(&u))
        .map(|(s, secs)| {
            let silent = p.body.pointer("/parameters/audio").and_then(Value::as_bool) == Some(false);
            cost_cny(s, secs, billed_tier(task, &p.body).as_deref(), silent)
        })
        .and_then(|c| usd(c, Currency::Cny, cny_per_usd));
    Usage {
        cost_usd: cost,
        cost_source: if cost.is_some() { CostSource::Estimated } else { CostSource::Unknown },
        raw: u,
        ..Default::default()
    }
}

#[cfg(test)]
mod tests;
