//! Seedream image generation on Volcengine Ark: `POST {base}/images/generations`
//! (Ark has no chat route for these models; a bare `doubao-seedream-…` id is
//! this API). Spec: Ark's docs as fetched 2026-11-13 (`docs.volcengine.com/docs/
//! zh/ark/…`: `image-generation-api`, `image-generation-streaming-responses`,
//! `image-generation-tutorial`, `seedream-5-5`, `model-list` §图片生成能力,
//! `model-pricing` §图片生成模型, `model-deprecation-notice` §第十批).
//!
//! **The models** ([`MODELS`]): Seedream 5.5 (`doubao-seedream-5-5-261028`,
//! first of Ark's "推荐模型"), 5.0 pro and 5.0 flash: one image per request,
//! no streaming, 1K / 1.5K / 2K, up to 10 reference images, `background`
//! (transparent / opaque; 5.5 also auto), `safety_identifier`. The models
//! that made image groups, streamed and searched (5.0 lite, 4.5, 4.0) end
//! service on 2026-11-24 and are not in the table (owner's rule); the code for
//! groups, streams and searches stays for models that bring them back.
//!
//! **Conversation** (the owner's rule): a request sends only the new message's
//! text (its text parts, an album's captions included, joined by line breaks)
//! and its images (attached or replied to); a reply to a generated image (the
//! previous assistant turn's images, from any model) **edits** it: those images
//! come first as references ("每轮编辑时，将上一轮生成结果作为输入", 5.5
//! tutorial), then the attached ones. Earlier turns are not sent (the API has no
//! conversation); a system prompt is an error.
//!
//! **Defaults** (D3, D79 and the owner): Ark's default resolution tier, which
//! is also the largest (5.5 / 5.0 pro / flash 2K, the others 4K), with the aspect ratio left to the model ("方式
//! 1（推荐）"; an aspect option sends the pixels of Ark's tier × ratio table, or
//! computed ones for other ratios), `watermark: false` (Ark's default adds "AI
//! 生成"), PNG where the format can be set (lossless; Ark's 5.0 default is
//! jpeg, 4.x are jpeg only), `response_format: b64_json` (no 24-hour URLs to
//! fetch), groups off (`sequential_image_generation: disabled`) unless `n=`
//! asks for up to n images (`auto`, `max_images`; references + images ≤ 15).
//! Ark documents no moderation or safety switch. The models that stream are
//! streamed (each image as it is done) unless `+s`: Ark reports a search only
//! at the end of a stream, so a searched request is not streamed and the
//! search is shown before the images, where it ran (one `web_search` call per
//! search in `usage.tool_usage.web_search`; 0, "未搜索", is noted).
//!
//! **Options**: `size=1K` `2K` `4K`, the resolution tiers the model has (Ark's
//! 1.5K and 3K have no option); `ar=` aspect ratios (as pixels; Ark takes any
//! ratio in [1/16, 16] within the model's pixel range); `n=` image groups;
//! `bg=` background; `q=` the prompt optimization mode (`standard`, Ark's
//! "质量更高", or `fast` on 5.0 pro).
//! Layer decomposition, its OCR and interactive editing by coordinates have
//! no option.
//!
//! **Cost** (no cost in responses): CNY list prices per image, input images
//! included (5.5 ¥0.05 each, 5.0 pro the first free then ¥0.02), output by
//! the image's pixels (5.5 ¥0.13 up to 2.61M px, ¥0.23 up to 4.63M; 5.0 pro
//! ¥0.30 / ¥0.60 above 2.61M; flash ¥0.12),
//! for the `generated_images` Ark reports ("因审核等原因未成功输出的图片不计费").
//! The image API's web search has no listed price (not estimated; noted).
//!
//! Errors are Ark's `{"error":{"code","message",…}}` ([`super::error_from_json`]);
//! a group image that fails (`image_generation.partial_failed`, `data[].error`)
//! is noted and the others are kept; a request that produced no image fails
//! with the errors (content filter for Ark's moderation codes).

use base64::Engine as _;
use omni_core::Usage;
use omni_core::content::{Part, Role, ToolCall, Turn};
use omni_core::usage::CostSource;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::{ArkConfig, error_from_json};
use crate::error::{ErrorKind, LlmError, Retry};
use crate::event::StreamEvent;
use crate::openrouter::options::{Applied, overlay};
use crate::request::{self, Generation, Requested, RouteCaps, Tool};
use crate::{BlobLoader, CallInput, CallOutcome, EventSink, MediaClass, Observer, Prepared, classify_mime};

pub const DIALECT: &str = "ark.images";
const NAME: &str = "Volcengine Ark";
const ID_HEADER: &str = "x-request-id";
/// References plus generated images of a group request.
const GROUP_TOTAL: usize = 15;
/// "长度不超过 64 个字符".
const SAFETY_ID_MAX: usize = 64;

/// Whether `model` is a Seedream image model (served by this module).
pub fn is_image_model(model: &str) -> bool {
    model.starts_with("doubao-seedream-")
}

/// A Seedream model: what its API takes and what it costs.
#[derive(Debug, Clone, Copy)]
pub struct Spec {
    pub id: &'static str,
    /// The model's name in Ark's docs.
    pub name: &'static str,
    /// Reference images per request.
    pub max_refs: usize,
    /// Resolution tiers (`size: "2K"`), smallest first.
    pub tiers: &'static [&'static str],
    /// Ark's tier × ratio → pixels table ("模型实际映射的宽高像素参考值").
    pub table: &'static [(&'static str, &'static str, u32, u32)],
    /// Total pixels of an explicit `WxH` size.
    pub pixels: (u64, u64),
    /// Image groups (`sequential_image_generation`) and streaming.
    pub groups: bool,
    /// The `web_search` tool.
    pub web_search: bool,
    /// `output_format` can be set (else the output is always jpeg).
    pub formats: bool,
    /// `background` values.
    pub backgrounds: &'static [&'static str],
    /// `optimize_prompt_options.mode` values, best first.
    pub prompt_modes: &'static [&'static str],
    /// `safety_identifier` (end user).
    pub safety_identifier: bool,
    /// CNY per input image: the first one, each further one.
    pub input_cny: (f64, f64),
    /// CNY per output image by its pixels: (up to, price), ascending.
    pub output_cny: &'static [(u64, f64)],
}

/// 5.5 / 5.0 pro / 5.0 flash: 1K, 1.5K, 2K.
const TABLE_5: &[(&str, &str, u32, u32)] = &[
    ("1K", "1:1", 1024, 1024),
    ("1K", "4:3", 1152, 864),
    ("1K", "3:4", 864, 1152),
    ("1K", "16:9", 1424, 800),
    ("1K", "9:16", 800, 1424),
    ("1K", "3:2", 1248, 832),
    ("1K", "2:3", 832, 1248),
    ("1K", "21:9", 1568, 672),
    ("1.5K", "1:1", 1536, 1536),
    ("1.5K", "4:3", 1792, 1344),
    ("1.5K", "3:4", 1344, 1792),
    ("1.5K", "16:9", 2048, 1152),
    ("1.5K", "9:16", 1152, 2048),
    ("1.5K", "3:2", 1872, 1248),
    ("1.5K", "2:3", 1248, 1872),
    ("1.5K", "21:9", 2352, 1008),
    ("2K", "1:1", 2048, 2048),
    ("2K", "4:3", 2368, 1776),
    ("2K", "3:4", 1776, 2368),
    ("2K", "16:9", 2816, 1584),
    ("2K", "9:16", 1584, 2816),
    ("2K", "3:2", 2496, 1664),
    ("2K", "2:3", 1664, 2496),
    ("2K", "21:9", 3136, 1344),
];

/// [1280x720, 2048x2048x1.1025].
const PIXELS_5: (u64, u64) = (921_600, 4_624_220);
/// "P ≤ 2.61M" (M = 10⁶: Ark writes 2048x2048x1.1025 = 4,624,220 as "4.63M").
const P_2_61M: u64 = 2_610_000;

const SEEDREAM_5: Spec = Spec {
    id: "",
    name: "",
    max_refs: 10,
    tiers: &["1K", "1.5K", "2K"],
    table: TABLE_5,
    pixels: PIXELS_5,
    groups: false,
    web_search: false,
    formats: true,
    backgrounds: &["transparent", "opaque"],
    prompt_modes: &["standard"],
    safety_identifier: true,
    input_cny: (0.0, 0.0),
    output_cny: &[(u64::MAX, 0.0)],
};

/// Ark's image models (2026-11-13).
pub const MODELS: &[Spec] = &[
    Spec {
        id: "doubao-seedream-5-5-261028",
        name: "Seedream 5.5",
        backgrounds: &["auto", "transparent", "opaque"],
        input_cny: (0.05, 0.05),
        output_cny: &[(P_2_61M, 0.13), (u64::MAX, 0.23)],
        ..SEEDREAM_5
    },
    Spec {
        id: "doubao-seedream-5-0-pro-260628",
        name: "Seedream 5.0 pro",
        prompt_modes: &["standard", "fast"],
        input_cny: (0.0, 0.02),
        output_cny: &[(P_2_61M, 0.30), (u64::MAX, 0.60)],
        ..SEEDREAM_5
    },
    Spec {
        id: "doubao-seedream-5-0-flash-260915",
        name: "Seedream 5.0 flash",
        output_cny: &[(u64::MAX, 0.12)],
        ..SEEDREAM_5
    },
];

pub fn spec(model: &str) -> Option<&'static Spec> {
    MODELS.iter().find(|m| m.id == model)
}

/// What Ark's image API takes for a model (the vendor's own API: official = route).
struct Caps(Option<&'static Spec>);

impl RouteCaps for Caps {
    fn name(&self) -> &str {
        NAME
    }
    fn tool(&self, tool: Tool) -> bool {
        tool == Tool::Search && self.0.is_some_and(|s| s.web_search)
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
        self.0.is_some()
    }
    fn tiers(&self) -> Vec<String> {
        Vec::new()
    }
    fn generation(&self) -> Generation {
        let Some(s) = self.0 else { return Generation::default() };
        Generation { quality: true, count: s.groups, background: !s.backgrounds.is_empty(), ..Default::default() }
    }
}

fn err(m: impl Into<String>) -> LlmError {
    LlmError::request(m)
}

fn gcd(a: u64, b: u64) -> u64 {
    if b == 0 { a } else { gcd(b, a % b) }
}

/// `w:h` as numbers.
fn ratio(r: &str) -> Option<(u64, u64)> {
    let (a, b) = r.split_once(':')?;
    let (a, b): (u64, u64) = (a.parse().ok()?, b.parse().ok()?);
    (a > 0 && b > 0).then_some((a, b))
}

/// The pixels for `tier` at ratio `r`: Ark's table, else the largest size of
/// exactly that ratio with about the tier's 1:1 pixel count, within the range.
pub fn dimensions(spec: &Spec, tier: &str, r: &str) -> Result<(u32, u32), String> {
    let (a, b) = ratio(r).ok_or_else(|| format!("`{r}` is not an aspect ratio"))?;
    let g = gcd(a, b);
    let (a, b) = (a / g, b / g);
    if a > 16 * b || b > 16 * a {
        return Err(format!("aspect ratio {r} is outside Ark's range for `{}` (1:16 to 16:1)", spec.id));
    }
    let same = |x: &str| ratio(x).is_some_and(|(c, d)| c * b == d * a);
    if let Some(&(_, _, w, h)) = spec.table.iter().find(|(t, x, _, _)| *t == tier && same(x)) {
        return Ok((w, h));
    }
    let side = spec
        .table
        .iter()
        .find(|(t, x, _, _)| *t == tier && *x == "1:1")
        .map(|&(_, _, w, _)| w as u64)
        .ok_or_else(|| format!("no {tier} size for `{}`", spec.id))?;
    let target = side * side;
    let (lo, hi) = spec.pixels;
    let fits = |w: u64, h: u64| (lo..=hi).contains(&(w * h)) && w > 14 && h > 14;
    // Exactly the ratio: w = a·k, h = b·k.
    let k = ((target as f64) / (a * b) as f64).sqrt().floor() as u64;
    if k > 0 && fits(a * k, b * k) {
        return Ok(((a * k) as u32, (b * k) as u32));
    }
    let w = ((target * a) as f64 / b as f64).sqrt().round() as u64;
    let h = ((target * b) as f64 / a as f64).sqrt().round() as u64;
    if fits(w, h) {
        return Ok((w as u32, h as u32));
    }
    Err(format!("aspect ratio {r} gives no {tier} size within `{}`'s pixel range ({lo}–{hi} pixels)", spec.id))
}

/// The input images and text of a request.
struct Inputs {
    prompt: String,
    /// Data URLs, references first.
    images: Vec<String>,
    /// Images of the replied-to answer (they come first).
    edited: usize,
    /// Earlier turns with content that is not sent.
    unsent_history: bool,
}

fn data_url(mime: &str, bytes: &[u8]) -> String {
    // "`<图片格式>` 需小写".
    crate::provider::data_url(&mime.to_ascii_lowercase(), bytes)
}

/// The new message's text and images, and the images of the answer it replies to.
async fn inputs(history: &[Turn], model: &str, blobs: &dyn BlobLoader) -> Result<Inputs, LlmError> {
    let last = history
        .iter()
        .rposition(|t| t.role == Role::User)
        .ok_or_else(|| err(format!("`{model}` needs a prompt: there is no user message")))?;
    if history.iter().any(|t| t.role == Role::System && !t.answer_text().trim().is_empty()) {
        return Err(err(format!(
            "`{model}` takes no system prompt: Seedream's image API has a prompt and reference images only"
        )));
    }
    let mut images = Vec::new();
    // A reply to an answer with images edits them (the previous turn).
    let replied = last.checked_sub(1).map(|i| &history[i]).filter(|t| t.role == Role::Assistant);
    if let Some(t) = replied {
        for p in &t.parts {
            if let Part::Image { blob, thought: false, .. } = p {
                let bytes = crate::provider::load(blobs, blob).await?;
                images.push(data_url(&blob.mime, &bytes));
            }
        }
    }
    let edited = images.len();
    // Editing sends what the settings say; otherwise say that earlier messages are not sent.
    let unsent_history = edited == 0
        && history[..last].iter().any(|t| {
            t.role != Role::System
                && t.parts.iter().any(|p| matches!(p, Part::Text { text, .. } if !text.trim().is_empty()))
        });
    let mut texts: Vec<&str> = Vec::new();
    for p in &history[last].parts {
        match p {
            Part::Text { text, .. } => {
                if !text.is_empty() {
                    texts.push(text);
                }
            }
            Part::Image { blob, .. } => {
                let bytes = crate::provider::load(blobs, blob).await?;
                images.push(data_url(&blob.mime, &bytes));
            }
            Part::File { blob, filename, .. } => {
                let name = filename.as_deref().unwrap_or("file");
                if classify_mime(&blob.mime, filename.as_deref()) != MediaClass::Image {
                    return Err(err(format!(
                        "`{model}` takes text and images only (Seedream image generation): {name} is {}",
                        blob.mime
                    )));
                }
                let bytes = crate::provider::load(blobs, blob).await?;
                images.push(data_url(&blob.mime, &bytes));
            }
            Part::RemoteMedia { url, .. } => {
                return Err(err(format!("`{model}` takes text and images only (Seedream image generation): {url}")));
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

/// Whether `req` asks for anything (`tier=default|auto` changes nothing).
fn has_options(req: &Requested) -> bool {
    let mut r = req.clone();
    if matches!(r.tier.as_deref(), Some("default" | "auto")) {
        r.tier = None;
    }
    r != Requested::default()
}

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
                    "`{model}` is an image model: it draws without `+i` (the image tool of chat models)"
                )));
            }
            Tool::Search => {
                return Err(err(format!("`{model}` has no web search (`+s`) on Ark's image API")));
            }
            t => return Err(err(format!("`{model}` has no {} (`+{}`) on Ark's image API", t.describe(), t.letter()))),
        }
    }
    let Some(spec) = spec else {
        if has_options(&req) {
            return Err(err(format!(
                "`{model}` is not in the bot's Seedream table, so its options cannot be checked: send it without \
                 options"
            )));
        }
        return unknown(cfg, input, blobs).await;
    };
    let mut basic = req.clone();
    basic.tools.clear();
    basic.image_size = None;
    basic.aspect_ratio = None;
    request::validate(model, &basic, &Caps(Some(spec))).map_err(LlmError::request)?;
    if let Some(q) = &req.quality
        && !spec.prompt_modes.contains(&q.as_str())
    {
        return Err(err(format!(
            "`{model}` has no prompt optimization mode `{q}` (`q=`); modes: {}",
            spec.prompt_modes.join(", ")
        )));
    }
    if let Some(bg) = &req.background
        && !spec.backgrounds.contains(&bg.as_str())
    {
        return Err(err(format!(
            "`{model}` has no background `{bg}` (`bg=`); backgrounds: {}",
            spec.backgrounds.join(", ")
        )));
    }
    let tier = match &req.image_size {
        Some(t) if spec.tiers.contains(&t.as_str()) => t.clone(),
        Some(t) => {
            let letters: Vec<&str> = spec.tiers.iter().copied().filter(|t| matches!(*t, "1K" | "2K" | "4K")).collect();
            return Err(err(format!("image size {t} is not available for `{model}`; sizes: {}", letters.join(", "))));
        }
        None => spec.tiers.last().copied().unwrap_or("2K").to_string(),
    };
    let size = match &req.aspect_ratio {
        Some(r) => {
            let (w, h) = dimensions(spec, &tier, r).map_err(LlmError::request)?;
            Some((format!("{w}x{h}"), r.clone()))
        }
        None => None,
    };

    let ins = inputs(input.history, model, blobs).await?;
    // A reply that edits nothing would ignore the conversation (owner's rule: an error).
    if ins.unsent_history {
        return Err(err(format!(
            "`{model}` takes one prompt, not a conversation: reply to an image it generated to edit it, or start a new \
             message"
        )));
    }
    if ins.images.len() > spec.max_refs {
        return Err(err(format!(
            "`{model}` takes at most {} reference images; this request has {}",
            spec.max_refs,
            ins.images.len()
        )));
    }
    let count = req.count.unwrap_or(1) as usize;
    if spec.groups && count + ins.images.len() > GROUP_TOTAL && count > 1 {
        return Err(err(format!(
            "`{model}`: reference images plus generated images are at most {GROUP_TOTAL}; with {} references `n=` \
             can be at most {}",
            ins.images.len(),
            GROUP_TOTAL.saturating_sub(ins.images.len())
        )));
    }
    if count > GROUP_TOTAL {
        return Err(err(format!("`n=` can be at most {GROUP_TOTAL} for `{model}` (`max_images` 1–15)")));
    }
    let search = req.tools.contains(&Tool::Search);

    let src = |by_user: bool| if by_user { "option" } else { "default" }.to_string();
    let mut applied = Vec::new();
    let mut settings = Vec::new();
    let mut warnings = Vec::new();
    let notes = Vec::new();
    let mut body = json!({"model": model, "prompt": ins.prompt});
    if !ins.images.is_empty() {
        body["image"] = if ins.images.len() == 1 { json!(ins.images[0]) } else { json!(ins.images) };
    }
    match &size {
        Some((px, r)) => {
            body["size"] = json!(px);
            settings.push(format!("size {px} ({tier} {r})"));
            applied.push(Applied {
                source: src(user.aspect_ratio.is_some() || user.image_size.is_some()),
                patch: json!({"size": px}),
            });
        }
        None => {
            body["size"] = json!(tier);
            settings.push(format!("size {tier}"));
            applied.push(Applied { source: src(user.image_size.is_some()), patch: json!({"size": tier}) });
        }
    }
    if spec.groups {
        if count > 1 {
            body["sequential_image_generation"] = json!("auto");
            body["sequential_image_generation_options"] = json!({"max_images": count});
            settings.push(format!("image group: up to {count} images"));
            applied.push(Applied {
                source: src(user.count.is_some()),
                patch: json!({"sequential_image_generation": "auto", "sequential_image_generation_options": {"max_images": count}}),
            });
        } else {
            body["sequential_image_generation"] = json!("disabled");
            applied.push(Applied {
                source: src(user.count.is_some()),
                patch: json!({"sequential_image_generation": "disabled"}),
            });
        }
        if !search {
            body["stream"] = json!(true);
            applied.push(Applied { source: "default".into(), patch: json!({"stream": true}) });
        }
    }
    body["response_format"] = json!("b64_json");
    applied.push(Applied { source: "default".into(), patch: json!({"response_format": "b64_json"}) });
    if spec.formats {
        body["output_format"] = json!("png");
        settings.push("png".into());
        applied.push(Applied { source: "default".into(), patch: json!({"output_format": "png"}) });
    }
    body["watermark"] = json!(false);
    settings.push("no watermark".into());
    applied.push(Applied { source: "default".into(), patch: json!({"watermark": false}) });
    if let Some(bg) = &req.background {
        body["background"] = json!(bg);
        settings.push(format!("background {bg}"));
        applied.push(Applied { source: src(user.background.is_some()), patch: json!({"background": bg}) });
    }
    if let Some(q) = &req.quality {
        body["optimize_prompt_options"] = json!({"mode": q});
        settings.push(format!("prompt optimization {q}"));
        applied.push(Applied {
            source: src(user.quality.is_some()),
            patch: json!({"optimize_prompt_options": {"mode": q}}),
        });
    }
    if search {
        body["tools"] = json!([{"type": "web_search"}]);
        settings.push("web search".into());
        applied
            .push(Applied { source: src(user.tools.contains(&Tool::Search)), patch: json!({"tools": body["tools"]}) });
        warnings.push(
            "Ark's price list has no fee for the image API's web search: searches are not in the cost estimate"
                .to_string(),
        );
    }
    if spec.safety_identifier
        && let Some(u) = input.user.as_deref().filter(|u| !u.is_empty() && u.len() <= SAFETY_ID_MAX)
    {
        body["safety_identifier"] = json!(u);
        applied.push(Applied { source: "default".into(), patch: json!({"safety_identifier": u}) });
    }
    describe_inputs(&ins, &mut settings);
    Ok(Prepared {
        dialect: DIALECT,
        model: model.to_string(),
        url: format!("{}/images/generations", cfg.base_url.trim_end_matches('/')),
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

fn describe_inputs(ins: &Inputs, settings: &mut Vec<String>) {
    let attached = ins.images.len() - ins.edited;
    match (ins.edited, attached) {
        (0, 0) => {}
        (0, n) => settings.push(format!("{n} reference image{}", if n == 1 { "" } else { "s" })),
        (e, n) => {
            let mut s = if e == 1 {
                "editing the replied-to image".to_string()
            } else {
                format!("editing the {e} replied-to images")
            };
            if n > 0 {
                s.push_str(&format!(" + {n} reference image{}", if n == 1 { "" } else { "s" }));
            }
            settings.push(s);
        }
    }
}

/// A Seedream id outside the table: the documented fields every Seedream model
/// takes, and nothing that differs between them.
async fn unknown(cfg: &ArkConfig, input: &CallInput<'_>, blobs: &dyn BlobLoader) -> Result<Prepared, LlmError> {
    let model = input.model;
    let ins = inputs(input.history, model, blobs).await?;
    // A reply that edits nothing would ignore the conversation (owner's rule: an error).
    if ins.unsent_history {
        return Err(err(format!(
            "`{model}` takes one prompt, not a conversation: reply to an image it generated to edit it, or start a new \
             message"
        )));
    }
    let mut body = json!({"model": model, "prompt": ins.prompt});
    if !ins.images.is_empty() {
        body["image"] = if ins.images.len() == 1 { json!(ins.images[0]) } else { json!(ins.images) };
    }
    body["response_format"] = json!("b64_json");
    body["watermark"] = json!(false);
    let applied = vec![
        Applied { source: "default".into(), patch: json!({"response_format": "b64_json"}) },
        Applied { source: "default".into(), patch: json!({"watermark": false}) },
    ];
    let mut settings = vec!["no watermark".to_string()];
    let notes = Vec::new();
    describe_inputs(&ins, &mut settings);
    Ok(Prepared {
        dialect: DIALECT,
        model: model.to_string(),
        url: format!("{}/images/generations", cfg.base_url.trim_end_matches('/')),
        body,
        applied,
        warnings: vec![format!(
            "`{model}` is not in the bot's Seedream table: size, limits and price unknown (Ark's default size; cost \
             not estimated)"
        )],
        notes,
        show_debug: false,
        auto_max_tokens: None,
        long_decode: false,
        settings,
    })
}

// ------------------------------------------------------------------ responses

/// `"2048x2048"` (also with `×`, as in Ark's streaming example).
fn parse_size(s: &str) -> Option<(u32, u32)> {
    let (w, h) = s.split_once(['x', 'X', '×'])?;
    Some((w.trim().parse().ok()?, h.trim().parse().ok()?))
}

fn image_mime(bytes: &[u8], format: Option<&str>) -> String {
    if bytes.starts_with(b"\x89PNG") {
        return "image/png".into();
    }
    if bytes.starts_with(&[0xFF, 0xD8]) {
        return "image/jpeg".into();
    }
    if bytes.len() > 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        return "image/webp".into();
    }
    match format {
        Some("png") => "image/png".into(),
        _ => "image/jpeg".into(),
    }
}

/// Ark's moderation codes (`…SensitiveContentDetected…`, `…RiskDetection`).
fn moderation(code: &str) -> bool {
    code.contains("SensitiveContent") || code.contains("RiskDetection")
}

/// Images, failures and usage of one request (streamed or not).
#[derive(Debug)]
pub struct Parser {
    model: String,
    spec: Option<&'static Spec>,
    cny_per_usd: f64,
    /// Reference images sent (for the input price).
    inputs: usize,
    format: Option<String>,
    sizes: Vec<(u32, u32)>,
    images: usize,
    failures: Vec<Value>,
    /// The response without image data (replay payload, /dump).
    record: Vec<Value>,
    usage: Option<Value>,
    served: Option<String>,
    tools: Option<Value>,
    /// Ark reported that the asked-for search did not run (`tool_usage.web_search: 0`).
    no_search: bool,
    meta_sent: bool,
    /// The request produced no image: its error.
    failure: Option<LlmError>,
    pub finished: bool,
    any_output: bool,
}

impl Parser {
    pub fn new(p: &Prepared, cny_per_usd: f64) -> Self {
        let inputs = match p.body.get("image") {
            Some(Value::Array(a)) => a.len(),
            Some(Value::String(_)) => 1,
            _ => 0,
        };
        Parser {
            model: p.model.clone(),
            spec: spec(&p.model),
            cny_per_usd,
            inputs,
            format: p.body.get("output_format").and_then(Value::as_str).map(str::to_string),
            sizes: Vec::new(),
            images: 0,
            failures: Vec::new(),
            record: Vec::new(),
            usage: None,
            served: None,
            tools: None,
            no_search: false,
            meta_sent: false,
            failure: None,
            finished: false,
            any_output: false,
        }
    }

    /// The error of a request that produced no image.
    pub fn failure(&self) -> Option<&LlmError> {
        self.failure.as_ref()
    }

    /// The response as received, without the image data.
    pub fn native_payload(&self) -> Value {
        let mut v = json!({"model": self.served.as_deref().unwrap_or(&self.model), "data": self.record});
        if let Some(u) = &self.usage {
            v["usage"] = u.clone();
        }
        if let Some(t) = &self.tools {
            v["tools"] = t.clone();
        }
        v
    }

    fn meta(&mut self, v: &Value, evs: &mut Vec<StreamEvent>) {
        if self.meta_sent {
            return;
        }
        if let Some(m) = v.get("model").and_then(Value::as_str) {
            self.meta_sent = true;
            self.served = Some(m.to_string());
            evs.push(StreamEvent::Meta { upstream_id: None, provider: None, model: Some(m.to_string()) });
        }
    }

    /// One image (an item of `data`, or a `partial_succeeded` event).
    fn image(&mut self, item: &Value, evs: &mut Vec<StreamEvent>) {
        let mut rec = item.clone();
        if let Some(o) = rec.as_object_mut() {
            o.remove("b64_json");
        }
        if let Some(e) = item.get("error").filter(|e| e.is_object()) {
            let index =
                item.get("image_index").and_then(Value::as_u64).map(|i| i as usize).unwrap_or(self.record.len());
            self.failures.push(json!({"image_index": index, "error": e}));
            let code = e.get("code").and_then(Value::as_str).unwrap_or("error");
            let message = e.get("message").and_then(Value::as_str).unwrap_or("");
            evs.push(StreamEvent::Notice(format!(
                "image {} of the group was not generated: {code}: {message}",
                index + 1
            )));
            self.record.push(rec);
            return;
        }
        self.record.push(rec);
        let Some(b64) = item.get("b64_json").and_then(Value::as_str) else {
            evs.push(StreamEvent::Notice("Ark returned an image without data (no b64_json)".into()));
            return;
        };
        match base64::engine::general_purpose::STANDARD.decode(b64.trim()) {
            Ok(bytes) => {
                if let Some(sz) = item.get("size").and_then(Value::as_str).and_then(parse_size) {
                    self.sizes.push(sz);
                }
                let format = item.get("output_format").and_then(Value::as_str).or(self.format.as_deref());
                let mime = image_mime(&bytes, format);
                self.images += 1;
                self.any_output = true;
                evs.push(StreamEvent::Image { data: bytes, mime });
            }
            Err(_) => evs.push(StreamEvent::Notice("Ark returned image data that is not base64".into())),
        }
    }

    /// The searches Ark reports (`usage.tool_usage.web_search`, "实际搜索次数…如果为
    /// 0 表示未搜索"): one `web_search` call each (Ark gives no query or
    /// results), or a note that none ran; the echoed `tools` are kept.
    fn search(&mut self, v: &Value, evs: &mut Vec<StreamEvent>) {
        let count = v.pointer("/usage/tool_usage/web_search").and_then(Value::as_u64);
        let tools = v.get("tools").filter(|t| t.as_array().is_some_and(|a| !a.is_empty()));
        if let Some(t) = tools {
            self.tools = Some(t.clone());
        }
        match count {
            // Said when the request produced images (a failed one says why itself).
            Some(0) => self.no_search = true,
            Some(n) => {
                self.any_output = true;
                for i in 1..=n {
                    evs.push(StreamEvent::Tool(ToolCall {
                        id: Some(format!("web_search_{i}")),
                        name: "web_search".into(),
                        status: Some("completed".into()),
                        input: None,
                        output: None,
                    }));
                }
            }
            None => {}
        }
    }

    /// Usage (with the estimated cost) and the end of the request.
    fn finish(&mut self, v: &Value, evs: &mut Vec<StreamEvent>) {
        let usage = v.get("usage").cloned().unwrap_or(Value::Null);
        if usage.is_object() {
            self.usage = Some(usage.clone());
            let generated = usage.get("generated_images").and_then(Value::as_u64).unwrap_or(self.images as u64);
            let inputs = usage.get("input_images").and_then(Value::as_u64).unwrap_or(self.inputs as u64);
            let cost = self
                .spec
                .map(|s| cost_cny(s, inputs, &self.sizes, generated))
                .and_then(|c| crate::provider::usd(c, crate::provider::Currency::Cny, self.cny_per_usd));
            evs.push(StreamEvent::Usage(Usage {
                input_tokens: None,
                output_tokens: usage.get("output_tokens").and_then(Value::as_u64),
                reasoning_tokens: None,
                cached_input_tokens: None,
                cache_write_tokens: None,
                cost_usd: cost,
                cost_source: if cost.is_some() { CostSource::Estimated } else { CostSource::Unknown },
                unbilled: Vec::new(),
                raw: usage,
            }));
        }
        self.finished = true;
        if self.images > 0 {
            if self.no_search {
                evs.push(StreamEvent::Notice(super::NO_SEARCH.into()));
            }
            evs.push(StreamEvent::Finish { reason: Some("stop".into()), native: None });
            return;
        }
        // Nothing was generated: a failure, with Ark's errors.
        let first = self.failures.first().and_then(|f| f.get("error")).cloned();
        let code = first.as_ref().and_then(|e| e.get("code")).and_then(Value::as_str).unwrap_or("").to_string();
        let message = first.as_ref().and_then(|e| e.get("message")).and_then(Value::as_str).unwrap_or("").to_string();
        let reason = if !code.is_empty()
            && self.failures.iter().all(|f| f.pointer("/error/code").and_then(Value::as_str).is_some_and(moderation))
        {
            "content_filter"
        } else {
            "error"
        };
        let details = json!({"errors": self.failures});
        evs.push(StreamEvent::FinishDetails(details.clone()));
        evs.push(StreamEvent::Finish { reason: Some(reason.into()), native: (!code.is_empty()).then(|| code.clone()) });
        let text = if code.is_empty() { "Ark returned no image".to_string() } else { format!("{code}: {message}") };
        let mut e = LlmError::new(ErrorKind::InBand, Retry::Never, text);
        e.error_type = Some(if code.is_empty() { reason.to_string() } else { code });
        e.details = Some(details);
        self.failure = Some(e);
    }

    /// A whole (non-streamed) response.
    pub fn response(&mut self, v: &Value) -> Result<Vec<StreamEvent>, LlmError> {
        let mut evs = Vec::new();
        self.meta(v, &mut evs);
        let data = v.get("data").and_then(Value::as_array).cloned().unwrap_or_default();
        if data.is_empty()
            && let Some(e) = v.get("error").filter(|e| e.is_object())
        {
            let mut le = error_from_json(e, None, None, ErrorKind::InBand);
            le.details = Some(v.clone());
            return Err(le);
        }
        // The search ran before the images.
        self.search(v, &mut evs);
        for item in &data {
            self.image(item, &mut evs);
        }
        self.finish(v, &mut evs);
        Ok(evs)
    }
}

impl crate::chatstream::EventParser for Parser {
    fn event(&mut self, name: Option<&str>, v: &Value) -> Result<Vec<StreamEvent>, LlmError> {
        let mut evs = Vec::new();
        let kind = v.get("type").and_then(Value::as_str).or(name).unwrap_or("");
        if kind == "error" || (v.get("type").is_none() && v.get("error").is_some_and(Value::is_object)) {
            let e = v.get("error").filter(|e| e.is_object()).unwrap_or(v);
            let mut le = error_from_json(e, None, None, ErrorKind::InBand);
            le.details = Some(v.clone());
            le.partial_output = self.any_output;
            return Err(le);
        }
        self.meta(v, &mut evs);
        match kind {
            "image_generation.partial_succeeded" | "image_generation.partial_failed" => self.image(v, &mut evs),
            "image_generation.completed" => {
                self.search(v, &mut evs);
                self.finish(v, &mut evs);
            }
            other => {
                evs.push(StreamEvent::Notice(format!("Ark sent an event the bot does not know: {other}")));
            }
        }
        Ok(evs)
    }
    fn finished(&self) -> bool {
        self.finished
    }
    fn any_output(&self) -> bool {
        self.any_output
    }
}

/// CNY of a request: input images, then each generated image at the price of
/// its pixels (images without a reported size at the highest price).
pub fn cost_cny(spec: &Spec, inputs: u64, sizes: &[(u32, u32)], generated: u64) -> f64 {
    let input = if inputs == 0 { 0.0 } else { spec.input_cny.0 + (inputs - 1) as f64 * spec.input_cny.1 };
    let price = |px: u64| spec.output_cny.iter().find(|(up_to, _)| px <= *up_to).map_or(0.0, |(_, p)| *p);
    let top = spec.output_cny.last().map_or(0.0, |(_, p)| *p);
    let output: f64 =
        (0..generated as usize).map(|i| sizes.get(i).map_or(top, |&(w, h)| price(w as u64 * h as u64))).sum();
    input + output
}

/// Run a prepared request: streamed (events) or one JSON response.
pub async fn stream(
    http: &reqwest::Client,
    cfg: &ArkConfig,
    p: &Prepared,
    sink: &mut dyn EventSink,
    observer: &dyn Observer,
    cancel: &CancellationToken,
) -> (Result<Value, LlmError>, CallOutcome) {
    let rb = http.post(&p.url).bearer_auth(&cfg.api_key).json(&p.body);
    let parser = Parser::new(p, cfg.cny_per_usd);
    if p.body.get("stream").and_then(Value::as_bool) == Some(true) {
        let crate::chatstream::Streamed { result, parser, header_id } = crate::chatstream::run_with(
            rb,
            &p.url,
            &p.body,
            cfg.idle_timeout,
            Some(ID_HEADER),
            error_from_json,
            parser,
            sink,
            observer,
            cancel,
        )
        .await;
        let outcome = CallOutcome { upstream_id: header_id, ..Default::default() };
        return match (result, parser.failure()) {
            (Err(e), _) => (Err(e), outcome),
            (Ok(()), Some(e)) => (Err(e.clone()), outcome),
            (Ok(()), None) => (Ok(parser.native_payload()), outcome),
        };
    }
    let mut parser = parser;
    let (result, id) = post_json(rb, p, cfg.idle_timeout, sink, observer, cancel).await;
    let outcome = CallOutcome { upstream_id: id, ..Default::default() };
    let v = match result {
        Ok(v) => v,
        Err(e) => return (Err(e), outcome),
    };
    match parser.response(&v) {
        Err(e) => (Err(e), outcome),
        Ok(evs) => {
            for e in evs {
                sink.event(e);
            }
            match parser.failure() {
                Some(e) => (Err(e.clone()), outcome),
                None => (Ok(parser.native_payload()), outcome),
            }
        }
    }
}

/// Send `rb` and read one JSON response (the request id header as `Meta`).
async fn post_json(
    rb: reqwest::RequestBuilder,
    p: &Prepared,
    timeout: std::time::Duration,
    sink: &mut dyn EventSink,
    observer: &dyn Observer,
    cancel: &CancellationToken,
) -> (Result<Value, LlmError>, Option<String>) {
    observer.request(&p.url, &p.body);
    let resp = tokio::select! {
        _ = cancel.cancelled() => return (Err(LlmError::cancelled()), None),
        r = tokio::time::timeout(timeout, rb.send()) => match r {
            Err(_) => {
                let e = LlmError::new(ErrorKind::Timeout, Retry::Backoff, "no response headers within idle timeout");
                return (Err(e), None);
            }
            Ok(Err(e)) => return (Err(LlmError::network(format!("request failed: {e}"))), None),
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
    let id = resp.headers().get(ID_HEADER).and_then(|v| v.to_str().ok()).map(str::to_string);
    if let Some(g) = &id {
        sink.event(StreamEvent::Meta { upstream_id: Some(g.clone()), provider: None, model: None });
    }
    let retry_after =
        resp.headers().get("retry-after").and_then(|v| v.to_str().ok()).and_then(|s| s.trim().parse::<u64>().ok());
    let text = tokio::select! {
        _ = cancel.cancelled() => return (Err(LlmError::cancelled()), id),
        t = tokio::time::timeout(timeout, resp.text()) => match t {
            Err(_) => {
                let e = LlmError::new(ErrorKind::Timeout, Retry::Backoff, format!("no response body within {}s", timeout.as_secs()));
                return (Err(e), id);
            }
            Ok(Err(e)) => return (Err(LlmError::network(format!("response read error: {e}"))), id),
            Ok(Ok(t)) => t,
        }
    };
    if !(200..300).contains(&status) {
        observer.body_text(&text);
        return (Err(crate::chatstream::http_error(status, retry_after, &text, error_from_json)), id);
    }
    // Logged like stream data: the observer leaves out the image data of large bodies.
    observer.sse_data(&text);
    match serde_json::from_str::<Value>(&text) {
        Ok(v) => (Ok(v), id),
        Err(e) => (
            Err(LlmError::new(ErrorKind::Decode, Retry::Backoff, format!("bad JSON response: {e}"))
                .with_details(Value::String(text.chars().take(2000).collect()))),
            id,
        ),
    }
}

#[cfg(test)]
mod tests;
