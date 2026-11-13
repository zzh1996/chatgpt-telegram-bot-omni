//! Alibaba Model Studio's image models (DashScope, China (Beijing)): a bare
//! Qwen-Image / Wan / Z-Image id is DashScope's synchronous image API,
//! `POST {root}/api/v1/services/aigc/multimodal-generation/generation`
//! ("DashScope同步调用（推荐）…功能最完整"; Wan: "一次请求即可获得结果，流程简单，
//! 推荐大多数场景使用"). No chat or Responses model outputs images and there is
//! no image tool in a chat (the Responses tools are search, extractors, code,
//! image search, file search and MCP). Spec: help.aliyun.com/zh/model-studio
//! as fetched 2026-11-13 (`qwen-image-generation-and-editing-api-reference`,
//! `qwen-image-api`, `qwen-image-edit-api`, `wan-image-generation-and-editing-
//! api-reference`, `wan-image-generation-api-reference`, `text-to-image-v2-api-
//! reference`, `z-image-api-reference`, `image-model`, `model-pricing`,
//! `error-code`, `model-depreciation` / notice 118434).
//!
//! - **Models** ([`MODELS`]): `qwen-image-3.0-pro` (Alibaba's first
//!   recommendation for generating and editing), `qwen-image-3.0`,
//!   `qwen-image-2.1-pro` / `-turbo` (up to 10 references; transparent output
//!   when the prompt asks), the early `qwen-image-2.0(-pro)` ids, `wan2.7-image
//!   (-pro)` (4K text-to-image on pro), `wan2.6-image` (editing: 1–4 images),
//!   `wan2.6-t2i`, `z-image-turbo` (one image). The Qwen-Image 1.x ids
//!   (`qwen-image`, `-plus`, `-max`, `-edit…`) were shut down on 2026-10-10.
//! - **Conversation**: as for Seedream (one request = the new message's text
//!   and images; a reply to a generated image sends that image first, then the
//!   attached ones; earlier messages are not sent).
//! - **Defaults** (D3, D79, the owner): the largest size where every size
//!   costs the same (Qwen-Image 3.0 / 2.x text-to-image 2048*2048, edits keep
//!   the input's aspect at the model's size; Wan 2.7 `4K` (pro,
//!   text-to-image) / `2K`; wan2.6-image `2K`; wan2.6-t2i 1440*1440; z-image
//!   2048*2048); Qwen-Image 3.0 pro, whose sizes above 2,250,000 pixels cost
//!   double, gets the model's own size (`ar=` alone: the largest of that ratio
//!   within 2,250,000 pixels) and a note naming `+size=2K`. `n: 1` sent
//!   explicitly (Wan 2.6's default is 4), `watermark: false`, thinking on where
//!   the model has it (`enable_thinking`, Wan 2.7 `thinking_mode` for
//!   text-to-image). z-image's "智能思考" rewriting (`prompt_extend`, which
//!   doubles its price) is off, Alibaba's default, with a note; `rewrite=on`
//!   turns it on. Alibaba's prompt rewriting on Qwen-Image and Wan 2.6
//!   (`prompt_extend`, default on and recommended there) stays at its default:
//!   no option turns it off. Moderation is always on ("所有 prompt … 都会经过内容安全审核"; the
//!   `X-DashScope-DataInspection` header adds a paid check, it removes none).
//! - **Options**: `size=1K` `2K` `4K` (Wan tiers; elsewhere a size of about
//!   that many pixels), `ar=` (pixels of that ratio within the model's limits),
//!   `n=` (Qwen-Image 1–6, Wan 1–4), `e=none` (thinking off), `rewrite=`
//!   (z-image's `prompt_extend`). Not mapped (no
//!   option): negative prompt, seed, Wan 2.7 image sets, colour palettes and
//!   box selection, Qwen's agent rewriting, wan2.6-image's text-and-image
//!   (streamed) mode.
//! - **Output**: PNG links valid 24 hours, downloaded by the bot; z-image's
//!   rewritten prompt (when it differs from the prompt sent) and reasoning are
//!   shown in order.
//! - **Cost**: CNY per image from `model-pricing` (Beijing list prices):
//!   Qwen 3.0 inputs ¥0.02 each; outputs 3.0-pro ¥0.25 (≤2,250,000 px) /
//!   ¥0.5, 3.0 ¥0.18, 2.1-pro ¥0.25, 2.1-turbo ¥0.1, 2.0-pro ¥0.5, 2.0 ¥0.2,
//!   wan2.7-image-pro ¥0.50, wan2.7-image / 2.6 ¥0.20, z-image ¥0.1 / ¥0.2
//!   with `prompt_extend` ("按输入图像和成功生成的图像张数计费").

use std::time::Duration;

use omni_core::Usage;
use omni_core::content::{Part, ReasoningKind, Role, Turn};
use omni_core::usage::CostSource;
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use super::{NAME, REQUEST_ID_HEADER, error_from_json};
use crate::error::{ErrorKind, LlmError, Retry};
use crate::event::StreamEvent;
use crate::openrouter::options::{Applied, overlay};
use crate::provider::{Currency, DirectConfig, usd};
use crate::request::{self, Generation, Raise, Requested, RouteCaps, Tool};
use crate::{BlobLoader, CallInput, CallOutcome, EventSink, MediaClass, Observer, Prepared, classify_mime};

pub const DIALECT: &str = "alibaba.images";
const PATH: &str = "/api/v1/services/aigc/multimodal-generation/generation";

/// How a model takes its size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Thinking {
    None,
    /// `enable_thinking` (Qwen-Image 3.0 / 2.1; with prompt rewriting).
    EnableThinking,
    /// `thinking_mode` (Wan 2.7; text-to-image only).
    ThinkingMode,
}

#[derive(Debug, Clone, Copy)]
pub struct Spec {
    pub id: &'static str,
    /// Input images: at most, and at least (wan2.6-image edits only).
    pub max_refs: usize,
    pub min_refs: usize,
    /// `size` tiers (Wan); empty: pixels only.
    pub tiers: &'static [&'static str],
    /// Pixels per tier (1:1).
    pub tier_pixels: &'static [(&'static str, u64)],
    /// Total pixels of a `宽*高` size, text-to-image and with input images.
    pub pixels: (u64, u64),
    pub pixels_edit: (u64, u64),
    /// Longest side to shortest side (`w:h` within 1:r–r:1); 0: none documented.
    pub max_ratio: u64,
    /// The largest size when nothing is asked (text-to-image).
    pub default_t2i: Option<&'static str>,
    /// The largest size when the request has images (`None`: the model's own, keeping the input's aspect).
    pub default_edit: Option<&'static str>,
    /// `n` up to this (1: no `n`).
    pub max_n: u32,
    pub thinking: Thinking,
    pub watermark: bool,
    /// CNY per input image.
    pub input_cny: f64,
    /// CNY per output image by its pixels: (up to, price), ascending.
    pub output_cny: &'static [(u64, f64)],
    /// CNY per image with `prompt_extend` on (z-image).
    pub extended_cny: Option<f64>,
    /// `prompt_extend` is a paid switch, off by default (z-image's "智能思考"
    /// rewriting): `rewrite=`.
    pub rewrite: bool,
}

const P512: u64 = 512 * 512;
const P768: u64 = 768 * 768;
const P1280: u64 = 1280 * 1280;
const P1440: u64 = 1440 * 1440;
const P2048: u64 = 2048 * 2048;
const P4096: u64 = 4096 * 4096;
/// "面积≤2,250,000为`qima_output_1k`，面积>2,250,000为`qima_output_2k`".
const QIMA_1K: u64 = 2_250_000;

const QWEN3: Spec = Spec {
    id: "",
    max_refs: 3,
    min_refs: 0,
    tiers: &[],
    tier_pixels: &[],
    pixels: (P512, P2048),
    pixels_edit: (P512, P2048),
    max_ratio: 8,
    default_t2i: Some("2048*2048"),
    default_edit: None,
    max_n: 6,
    thinking: Thinking::EnableThinking,
    watermark: true,
    input_cny: 0.02,
    output_cny: &[(u64::MAX, 0.18)],
    extended_cny: None,
    rewrite: false,
};

const QWEN21: Spec = Spec { max_refs: 10, input_cny: 0.0, ..QWEN3 };

/// The early Qwen-Image 2.0 ids: no thinking switch, no documented aspect limit.
const QWEN20: Spec =
    Spec { max_ratio: 0, thinking: Thinking::None, input_cny: 0.0, output_cny: &[(u64::MAX, 0.5)], ..QWEN3 };

const WAN27_TIERS: &[(&str, u64)] = &[("1K", 1024 * 1024), ("2K", P2048), ("4K", P4096)];

const WAN27: Spec = Spec {
    id: "",
    max_refs: 9,
    min_refs: 0,
    tiers: &["1K", "2K"],
    tier_pixels: WAN27_TIERS,
    pixels: (P768, P2048),
    pixels_edit: (P768, P2048),
    max_ratio: 8,
    default_t2i: Some("2K"),
    default_edit: Some("2K"),
    max_n: 4,
    thinking: Thinking::ThinkingMode,
    watermark: true,
    input_cny: 0.0,
    output_cny: &[(u64::MAX, 0.20)],
    extended_cny: None,
    rewrite: false,
};

/// Alibaba's image models in Beijing (2026-11-13).
pub const MODELS: &[Spec] = &[
    // Larger than 2,250,000 pixels costs double: the model's own size by default (D79).
    Spec { id: "qwen-image-3.0-pro", default_t2i: None, output_cny: &[(QIMA_1K, 0.25), (u64::MAX, 0.5)], ..QWEN3 },
    Spec { id: "qwen-image-3.0", ..QWEN3 },
    Spec { id: "qwen-image-2.1-pro", output_cny: &[(u64::MAX, 0.25)], ..QWEN21 },
    Spec { id: "qwen-image-2.1-turbo", output_cny: &[(u64::MAX, 0.1)], ..QWEN21 },
    Spec { id: "qwen-image-2.0-pro", ..QWEN20 },
    Spec { id: "qwen-image-2.0-pro-2026-06-22", ..QWEN20 },
    Spec { id: "qwen-image-2.0-pro-2026-04-22", ..QWEN20 },
    Spec { id: "qwen-image-2.0-pro-2026-03-03", ..QWEN20 },
    Spec { id: "qwen-image-2.0", output_cny: &[(u64::MAX, 0.2)], ..QWEN20 },
    Spec { id: "qwen-image-2.0-2026-03-03", output_cny: &[(u64::MAX, 0.2)], ..QWEN20 },
    // "wan2.7-image-pro文生图最高支持4096x4096分辨率" (text-to-image, not image sets).
    Spec {
        id: "wan2.7-image-pro",
        tiers: &["1K", "2K", "4K"],
        pixels: (P768, P4096),
        default_t2i: Some("4K"),
        output_cny: &[(u64::MAX, 0.50)],
        ..WAN27
    },
    Spec { id: "wan2.7-image", ..WAN27 },
    // Editing mode (`enable_interleave: false`, the default): 1–4 images; "1K" ≈ 1280*1280.
    Spec {
        id: "wan2.6-image",
        max_refs: 4,
        min_refs: 1,
        tier_pixels: &[("1K", P1280), ("2K", P2048)],
        max_ratio: 4,
        thinking: Thinking::None,
        ..WAN27
    },
    Spec {
        id: "wan2.6-t2i",
        max_refs: 0,
        tiers: &[],
        tier_pixels: &[],
        pixels: (P1280, P1440),
        pixels_edit: (P1280, P1440),
        max_ratio: 4,
        default_t2i: Some("1440*1440"),
        default_edit: None,
        thinking: Thinking::None,
        ..WAN27
    },
    Spec {
        id: "z-image-turbo",
        max_refs: 0,
        min_refs: 0,
        tiers: &[],
        tier_pixels: &[],
        pixels: (P512, P2048),
        pixels_edit: (P512, P2048),
        max_ratio: 0,
        default_t2i: Some("2048*2048"),
        default_edit: None,
        max_n: 1,
        thinking: Thinking::None,
        watermark: false,
        input_cny: 0.0,
        output_cny: &[(u64::MAX, 0.1)],
        extended_cny: Some(0.2),
        rewrite: true,
    },
];

pub fn spec(model: &str) -> Option<&'static Spec> {
    MODELS.iter().find(|m| m.id == model)
}

/// Whether `model` is an image model of this API (the table, or the families).
pub fn is_image_model(model: &str) -> bool {
    spec(model).is_some()
        || model.starts_with("qwen-image")
        || model.starts_with("z-image")
        || (model.starts_with("wan") && (model.contains("-image") || model.contains("-t2i")))
}

/// The native API's root from the configured (OpenAI-compatible) base URL:
/// `https://dashscope.aliyuncs.com/compatible-mode/v1` → `https://dashscope.aliyuncs.com`.
pub fn native_root(base_url: &str) -> String {
    let base = base_url.trim_end_matches('/');
    if let Some(root) = base.strip_suffix("/compatible-mode/v1") {
        return root.to_string();
    }
    match base.find("://").map(|i| i + 3) {
        Some(i) => match base[i..].find('/') {
            Some(j) => base[..i + j].to_string(),
            None => base.to_string(),
        },
        None => base.to_string(),
    }
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
        self.0.is_some_and(|s| s.thinking != Thinking::None)
    }
    fn budget(&self) -> bool {
        false
    }
    fn verbosity(&self) -> bool {
        false
    }
    fn image_options(&self) -> bool {
        self.0.is_some()
    }
    fn tiers(&self) -> Vec<String> {
        Vec::new()
    }
    fn generation(&self) -> Generation {
        Generation {
            count: self.0.is_some_and(|s| s.max_n > 1),
            rewrite: self.0.is_some_and(|s| s.rewrite),
            ..Default::default()
        }
    }
}

fn err(m: impl Into<String>) -> LlmError {
    LlmError::request(m)
}

fn ratio(r: &str) -> Option<(u64, u64)> {
    let (a, b) = r.split_once(':')?;
    let (a, b): (u64, u64) = (a.parse().ok()?, b.parse().ok()?);
    (a > 0 && b > 0).then_some((a, b))
}

/// `宽*高` of ratio `a:b` with about `target` pixels, within `range` (multiples of 16).
pub fn pixels_for(range: (u64, u64), max_ratio: u64, target: u64, (a, b): (u64, u64)) -> Result<String, String> {
    if max_ratio > 0 && (a > max_ratio * b || b > max_ratio * a) {
        return Err(format!("aspect ratio {a}:{b} is outside 1:{max_ratio} to {max_ratio}:1"));
    }
    let target = target.clamp(range.0, range.1) as f64;
    let r = a as f64 / b as f64;
    let w = ((target * r).sqrt() / 16.0).floor() * 16.0;
    let h = ((target / r).sqrt() / 16.0).floor() * 16.0;
    let (w, h) = (w as u64, h as u64);
    if w == 0 || h == 0 || w * h < range.0 || w * h > range.1 {
        return Err(format!("aspect ratio {a}:{b} has no size within {}–{} pixels", range.0, range.1));
    }
    Ok(format!("{w}*{h}"))
}

struct Inputs {
    prompt: String,
    /// Data URLs, the replied-to answer's images first.
    images: Vec<String>,
    edited: usize,
    unsent_history: bool,
}

async fn inputs(history: &[Turn], model: &str, blobs: &dyn BlobLoader) -> Result<Inputs, LlmError> {
    let last = history
        .iter()
        .rposition(|t| t.role == Role::User)
        .ok_or_else(|| err(format!("`{model}` needs a prompt: there is no user message")))?;
    if history.iter().any(|t| t.role == Role::System && !t.answer_text().trim().is_empty()) {
        return Err(err(format!("`{model}` takes no system prompt: Alibaba's image API has a prompt and images only")));
    }
    let mut images = Vec::new();
    let replied = last.checked_sub(1).map(|i| &history[i]).filter(|t| t.role == Role::Assistant);
    if let Some(t) = replied {
        for p in &t.parts {
            if let Part::Image { blob, thought: false, .. } = p {
                let bytes = crate::provider::load(blobs, blob).await?;
                images.push(crate::provider::data_url(&blob.mime, &bytes));
            }
        }
    }
    let edited = images.len();
    let unsent_history = edited == 0
        && history[..last].iter().any(|t| {
            t.role != Role::System
                && t.parts.iter().any(|p| matches!(p, Part::Text { text, .. } if !text.trim().is_empty()))
        });
    let mut texts = Vec::new();
    for p in &history[last].parts {
        match p {
            Part::Text { text, .. } => {
                if !text.is_empty() {
                    texts.push(text.as_str());
                }
            }
            Part::Image { blob, .. } => {
                let bytes = crate::provider::load(blobs, blob).await?;
                images.push(crate::provider::data_url(&blob.mime, &bytes));
            }
            Part::File { blob, filename, .. } => {
                if classify_mime(&blob.mime, filename.as_deref()) != MediaClass::Image {
                    return Err(err(format!(
                        "`{model}` takes text and images only: {} is {}",
                        filename.as_deref().unwrap_or("file"),
                        blob.mime
                    )));
                }
                let bytes = crate::provider::load(blobs, blob).await?;
                images.push(crate::provider::data_url(&blob.mime, &bytes));
            }
            Part::RemoteMedia { url, .. } => {
                return Err(err(format!("`{model}` takes text and images only ({url})")));
            }
            Part::Location { .. } => return Err(crate::provider::no_location(model, NAME)),
            Part::Reasoning(_) | Part::ToolCall(_) | Part::Refusal { .. } => {}
        }
    }
    let prompt = texts.join("\n");
    if prompt.trim().is_empty() {
        return Err(err(format!("`{model}` needs a prompt: the message has no text")));
    }
    Ok(Inputs { prompt, images, edited, unsent_history })
}

fn has_options(req: &Requested) -> bool {
    let mut r = req.clone();
    if matches!(r.tier.as_deref(), Some("default" | "auto")) {
        r.tier = None;
    }
    r != Requested::default()
}

/// The pixel budget of a size chosen for `ar=` alone: the largest, unless
/// larger sizes cost more (then the largest at the cheapest price).
fn default_pixels(spec: &Spec, range: (u64, u64)) -> u64 {
    match spec.output_cny {
        [(first, _), _, ..] => (*first).min(range.1),
        _ => range.1,
    }
}

/// The `size` to send (and how it was chosen), `None` for the model's own.
fn size(spec: &Spec, req: &Requested, edit: bool, model: &str) -> Result<Option<String>, LlmError> {
    let range = if edit { spec.pixels_edit } else { spec.pixels };
    let tier_pixels = |t: &str| spec.tier_pixels.iter().find(|(x, _)| *x == t).map(|(_, p)| *p);
    let tiers_here: Vec<&str> = spec
        .tiers
        .iter()
        .copied()
        // Wan 2.7 pro's 4K: text-to-image only.
        .filter(|t| !(edit && *t == "4K"))
        .collect();
    let target = match req.image_size.as_deref() {
        None => None,
        Some(t) if !spec.tiers.is_empty() => {
            if !tiers_here.contains(&t) {
                return Err(err(format!(
                    "image size {t} is not available for `{model}`{}; sizes: {}",
                    if edit { " with input images" } else { "" },
                    tiers_here.join(", ")
                )));
            }
            if req.aspect_ratio.is_none() {
                return Ok(Some(t.to_string()));
            }
            tier_pixels(t)
        }
        Some(t) => {
            let px = match t {
                "1K" => 1024 * 1024,
                "2K" => P2048,
                _ => P4096,
            };
            if px > range.1 || px < range.0 / 2 {
                return Err(err(format!(
                    "image size {t} is not available for `{model}` (its sizes have {}–{} pixels)",
                    range.0, range.1
                )));
            }
            Some(px)
        }
    };
    match (&req.aspect_ratio, target) {
        (Some(a), t) => {
            let r = ratio(a).ok_or_else(|| err(format!("`{a}` is not an aspect ratio")))?;
            // `ar=` alone: the largest size at the default price.
            let t = t.unwrap_or_else(|| default_pixels(spec, range));
            pixels_for(range, spec.max_ratio, t, r).map(Some).map_err(|e| err(format!("`{model}`: {e}")))
        }
        (None, Some(px)) => pixels_for(range, spec.max_ratio, px, (1, 1)).map(Some).map_err(LlmError::request),
        (None, None) => Ok(if edit { spec.default_edit } else { spec.default_t2i }.map(str::to_string)),
    }
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
    if req.tools.contains(&Tool::Image) {
        return Err(err(format!("`{model}` is an image model: it draws without `+i` (the image tool of chat models)")));
    }
    if let Some(t) = req.tools.first() {
        return Err(err(format!("`{model}` has no {} (`+{}`) on Alibaba's image API", t.describe(), t.letter())));
    }
    let spec = spec(model);
    if spec.is_none() && has_options(&req) {
        return Err(err(format!(
            "`{model}` is not in the bot's Alibaba image model table, so its options cannot be checked: send it \
             without options"
        )));
    }
    let mut basic = req.clone();
    basic.image_size = None;
    basic.aspect_ratio = None;
    request::validate(model, &basic, &Caps(spec)).map_err(LlmError::request)?;
    let ins = inputs(input.history, model, blobs).await?;
    let edit = !ins.images.is_empty();

    let src = |by_user: bool| if by_user { "option" } else { "default" }.to_string();
    let mut parameters = Map::new();
    let mut applied = Vec::new();
    let mut settings = Vec::new();
    let mut warnings = Vec::new();
    let mut notes = Vec::new();
    // Settings written while `set` holds `settings`.
    let mut deferred: Vec<String> = Vec::new();
    if let Some(s) = spec {
        if ins.images.len() > s.max_refs {
            return Err(err(if s.max_refs == 0 {
                format!("`{model}` takes no input image (text-to-image only)")
            } else {
                format!("`{model}` takes at most {} input images; this request has {}", s.max_refs, ins.images.len())
            }));
        }
        if ins.images.len() < s.min_refs {
            return Err(err(format!(
                "`{model}` edits images: it needs {}–{} input images (attach or reply to one)",
                s.min_refs, s.max_refs
            )));
        }
        let mut set = |k: &str, v: Value, by_user: bool, what: Option<String>| {
            parameters.insert(k.into(), v.clone());
            let mut patch = Map::new();
            patch.insert(k.into(), v);
            applied.push(Applied { source: src(by_user), patch: json!({"parameters": patch}) });
            if let Some(w) = what {
                settings.push(w);
            }
        };
        let sz = size(s, &req, edit, model)?;
        if let Some(sz) = &sz {
            let what = Some(format!("size {sz}"));
            set("size", json!(sz), user.image_size.is_some() || user.aspect_ratio.is_some(), what);
        } else {
            deferred.push("size: the model's choice".to_string());
        }
        // Tiered price: the default stays in the cheapest tier (D79).
        if s.output_cny.len() > 1 && user.image_size.is_none() {
            let default = sz.clone().unwrap_or_else(|| "the model's choice".into());
            notes.extend(request::raise_note(&[Raise::new("image size", default, "2K", "+size=2K")]));
        }
        if s.max_n > 1 {
            let n = req.count.unwrap_or(1);
            if n > s.max_n {
                return Err(err(format!("`{model}` makes 1–{} images per request (`n=`)", s.max_n)));
            }
            set("n", json!(n), user.count.is_some(), (n > 1).then(|| format!("{n} images")));
        }
        let off = req.effort.as_deref() == Some("none");
        match s.thinking {
            Thinking::None => {}
            Thinking::EnableThinking => set(
                "enable_thinking",
                json!(!off),
                user.effort.is_some(),
                Some(if off { "thinking off" } else { "thinking on" }.into()),
            ),
            Thinking::ThinkingMode if edit => {
                if off {
                    return Err(err(format!("`{model}` thinks only without input images: `e=none` does not apply")));
                }
            }
            Thinking::ThinkingMode => set(
                "thinking_mode",
                json!(!off),
                user.effort.is_some(),
                Some(if off { "thinking off" } else { "thinking on" }.into()),
            ),
        }
        if s.rewrite {
            let on = req.rewrite.unwrap_or(false);
            set(
                "prompt_extend",
                json!(on),
                user.rewrite.is_some(),
                Some(if on { "smart rewriting with thinking (prompt_extend)" } else { "prompt rewriting off" }.into()),
            );
            if !on && user.rewrite.is_none() {
                notes.push(request::switch_note(
                    "prompt rewriting with thinking (智能思考)",
                    "+rewrite=on",
                    "it doubles the price",
                ));
            }
        }
        if s.watermark {
            set("watermark", json!(false), false, Some("no watermark".into()));
        }
    } else {
        warnings.push(format!(
            "`{model}` is not in the bot's Alibaba image model table: sizes, limits and price unknown (Alibaba's \
             defaults; cost not estimated)"
        ));
    }
    settings.extend(deferred);
    let attached = ins.images.len() - ins.edited;
    match (ins.edited, attached) {
        (0, 0) => {}
        (0, n) => settings.push(format!("{n} input image{}", if n == 1 { "" } else { "s" })),
        (e, n) => {
            let mut s = if e == 1 {
                "editing the replied-to image".to_string()
            } else {
                format!("editing the {e} replied-to images")
            };
            if n > 0 {
                s.push_str(&format!(" + {n} input image{}", if n == 1 { "" } else { "s" }));
            }
            settings.push(s);
        }
    }
    // A reply that edits nothing would ignore the conversation (owner's rule: an error).
    if ins.unsent_history {
        return Err(err(format!(
            "`{model}` takes one prompt, not a conversation: reply to an image it generated to edit it, or start a new message"
        )));
    }
    let mut content: Vec<Value> = ins.images.iter().map(|u| json!({"image": u})).collect();
    content.push(json!({"text": ins.prompt}));
    let mut body = json!({"model": model, "input": {"messages": [{"role": "user", "content": content}]}});
    if !parameters.is_empty() {
        body["parameters"] = Value::Object(parameters);
    }
    Ok(Prepared {
        dialect: DIALECT,
        model: model.to_string(),
        url: format!("{}{PATH}", native_root(&cfg.base_url)),
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

// ------------------------------------------------------------------ running

fn image_mime(bytes: &[u8], header: Option<&str>) -> String {
    if bytes.starts_with(b"\x89PNG") {
        return "image/png".into();
    }
    if bytes.starts_with(&[0xFF, 0xD8]) {
        return "image/jpeg".into();
    }
    if bytes.len() > 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        return "image/webp".into();
    }
    header.filter(|h| h.starts_with("image/")).unwrap_or("image/png").to_string()
}

fn timed_out(what: &str) -> LlmError {
    LlmError::new(ErrorKind::Timeout, Retry::Backoff, format!("{what} within the idle timeout"))
}

/// The CNY price of a finished request.
pub fn cost_cny(spec: &Spec, body: &Value, usage: &Value, images: usize) -> f64 {
    let n_out = ["output_image_count", "image_count"]
        .iter()
        .find_map(|k| usage.get(*k).and_then(Value::as_u64))
        .unwrap_or(images as u64);
    let n_in = usage.get("input_image_count").and_then(Value::as_u64).unwrap_or_else(|| {
        body.pointer("/input/messages/0/content")
            .and_then(Value::as_array)
            .map_or(0, |c| c.iter().filter(|x| x.get("image").is_some()).count() as u64)
    });
    let dims = |w: &str, h: &str| Some(usage.get(w)?.as_u64()? * usage.get(h)?.as_u64()?);
    let area = dims("output_width", "output_height").or_else(|| dims("width", "height")).or_else(|| {
        let s = usage.get("size")?.as_str()?;
        let (w, h) = s.split_once('*')?;
        Some(w.trim().parse::<u64>().ok()? * h.trim().parse::<u64>().ok()?)
    });
    let extended = body.pointer("/parameters/prompt_extend").and_then(Value::as_bool) == Some(true);
    let per_image = match (spec.extended_cny, extended) {
        (Some(p), true) => p,
        _ => {
            let top = spec.output_cny.last().map_or(0.0, |(_, p)| *p);
            area.map_or(top, |a| spec.output_cny.iter().find(|(up, _)| a <= *up).map_or(top, |(_, p)| *p))
        }
    };
    n_in as f64 * spec.input_cny + n_out as f64 * per_image
}

pub async fn stream(
    http: &reqwest::Client,
    cfg: &DirectConfig,
    p: &Prepared,
    sink: &mut dyn EventSink,
    observer: &dyn Observer,
    cancel: &CancellationToken,
) -> (Result<Value, LlmError>, CallOutcome) {
    let mut outcome = CallOutcome::default();
    let result = run(http, cfg, p, sink, observer, cancel, &mut outcome).await;
    (result, outcome)
}

async fn run(
    http: &reqwest::Client,
    cfg: &DirectConfig,
    p: &Prepared,
    sink: &mut dyn EventSink,
    observer: &dyn Observer,
    cancel: &CancellationToken,
    outcome: &mut CallOutcome,
) -> Result<Value, LlmError> {
    let timeout = cfg.idle_timeout;
    observer.request(&p.url, &p.body);
    let rb = http.post(&p.url).bearer_auth(&cfg.api_key).json(&p.body);
    let resp = tokio::select! {
        _ = cancel.cancelled() => return Err(LlmError::cancelled()),
        r = tokio::time::timeout(timeout, rb.send()) => match r {
            Err(_) => return Err(timed_out("no response headers")),
            Ok(Err(e)) => return Err(LlmError::network(format!("request failed: {e}"))),
            Ok(Ok(r)) => r,
        }
    };
    let status = resp.status().as_u16();
    let headers: Vec<(String, String)> = resp
        .headers()
        .iter()
        .map(|(k, v)| (k.to_string(), String::from_utf8_lossy(v.as_bytes()).to_string()))
        .collect();
    observer.response_status(status, &headers);
    let header_id = resp.headers().get(REQUEST_ID_HEADER).and_then(|v| v.to_str().ok()).map(str::to_string);
    let retry_after =
        resp.headers().get("retry-after").and_then(|v| v.to_str().ok()).and_then(|s| s.trim().parse::<u64>().ok());
    let text = tokio::select! {
        _ = cancel.cancelled() => return Err(LlmError::cancelled()),
        t = tokio::time::timeout(timeout, resp.text()) => match t {
            Err(_) => return Err(timed_out("no response body")),
            Ok(Err(e)) => return Err(LlmError::network(format!("response read error: {e}"))),
            Ok(Ok(t)) => t,
        }
    };
    observer.body_text(&text);
    if !(200..300).contains(&status) {
        return Err(crate::chatstream::http_error(status, retry_after, &text, error_from_json));
    }
    let v: Value = serde_json::from_str(&text).map_err(|e| {
        LlmError::new(ErrorKind::Decode, Retry::Backoff, format!("bad JSON response: {e}"))
            .with_details(Value::String(text.clone()))
    })?;
    let id = v.get("request_id").and_then(Value::as_str).map(str::to_string).or(header_id);
    outcome.upstream_id = id.clone();
    sink.event(StreamEvent::Meta { upstream_id: id, provider: None, model: None });
    // A DashScope error object in a 200 answer.
    if v.get("output").is_none() && v.get("code").and_then(Value::as_str).is_some_and(|c| !c.is_empty()) {
        let mut e = error_from_json(&v, Some(status), retry_after, ErrorKind::InBand);
        e.details = Some(v.clone());
        return Err(e);
    }
    let message = v.pointer("/output/choices/0/message").cloned().unwrap_or(Value::Null);
    let sent = p
        .body
        .pointer("/input/messages/0/content")
        .and_then(Value::as_array)
        .and_then(|c| c.iter().find_map(|x| x.get("text").and_then(Value::as_str)).map(str::to_string));
    if let Some(r) = message.get("reasoning_content").and_then(Value::as_str).filter(|r| !r.is_empty()) {
        sink.event(StreamEvent::Reasoning { kind: ReasoningKind::Text, text: r.to_string(), block: 0 });
    }
    let mut images = 0;
    for item in message.get("content").and_then(Value::as_array).into_iter().flatten() {
        if let Some(url) = item.get("image").and_then(Value::as_str) {
            let (data, mime) = fetch(http, url, timeout, observer, cancel).await?;
            images += 1;
            sink.event(StreamEvent::Image { data, mime });
        } else if let Some(t) = item.get("text").and_then(Value::as_str) {
            // The prompt as Alibaba used it: shown when it was rewritten.
            if sent.as_deref() != Some(t) && !t.is_empty() {
                sink.event(StreamEvent::Text(t.to_string()));
            }
        }
    }
    let usage = v.get("usage").cloned().unwrap_or(Value::Null);
    let cost = spec(&p.model)
        .map(|s| cost_cny(s, &p.body, &usage, images))
        .and_then(|c| usd(c, Currency::Cny, cfg.cny_per_usd));
    let g = |k: &str| usage.get(k).and_then(Value::as_u64);
    sink.event(StreamEvent::Usage(Usage {
        input_tokens: g("input_tokens").filter(|n| *n > 0),
        output_tokens: g("output_tokens").filter(|n| *n > 0),
        reasoning_tokens: usage.pointer("/output_tokens_details/reasoning_tokens").and_then(Value::as_u64),
        cached_input_tokens: None,
        cache_write_tokens: None,
        cost_usd: cost,
        cost_source: if cost.is_some() { CostSource::Estimated } else { CostSource::Unknown },
        unbilled: Vec::new(),
        raw: usage,
    }));
    if images == 0 {
        let mut e = LlmError::new(ErrorKind::InBand, Retry::Never, "Alibaba returned no image");
        e.error_type = Some("error".into());
        e.details = Some(v.clone());
        return Err(e);
    }
    let native = v.pointer("/output/choices/0/finish_reason").and_then(Value::as_str).map(str::to_string);
    sink.event(StreamEvent::Finish { reason: Some("stop".into()), native });
    Ok(v)
}

/// Download a generated image (a temporary OSS link).
async fn fetch(
    http: &reqwest::Client,
    url: &str,
    timeout: Duration,
    observer: &dyn Observer,
    cancel: &CancellationToken,
) -> Result<(Vec<u8>, String), LlmError> {
    observer.request(url, &Value::Null);
    let resp = tokio::select! {
        _ = cancel.cancelled() => return Err(LlmError::cancelled()),
        r = tokio::time::timeout(timeout, http.get(url).send()) => match r {
            Err(_) => return Err(timed_out("no image download")),
            Ok(Err(e)) => return Err(LlmError::network(format!("image download failed: {e}"))),
            Ok(Ok(r)) => r,
        }
    };
    let status = resp.status().as_u16();
    observer.response_status(status, &[]);
    if !(200..300).contains(&status) {
        let mut e = LlmError::new(
            ErrorKind::Http,
            crate::error::retry_for_status(status, None),
            format!("downloading the generated image failed: HTTP {status}"),
        );
        e.status = Some(status);
        e.details = Some(json!({"url": url}));
        return Err(e);
    }
    let header = resp.headers().get("content-type").and_then(|v| v.to_str().ok()).map(str::to_string);
    let bytes = tokio::select! {
        _ = cancel.cancelled() => return Err(LlmError::cancelled()),
        b = tokio::time::timeout(timeout, resp.bytes()) => match b {
            Err(_) => return Err(timed_out("no image data")),
            Ok(Err(e)) => return Err(LlmError::network(format!("image download failed: {e}"))),
            Ok(Ok(b)) => b.to_vec(),
        }
    };
    let mime = image_mime(&bytes, header.as_deref());
    Ok((bytes, mime))
}

#[cfg(test)]
mod tests;
