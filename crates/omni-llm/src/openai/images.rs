//! OpenAI's Images API for bare image-model ids (`openai:gpt-image-2.6-sunburst$…`):
//! `POST {base}/images/generations`, or `/images/edits` when there are input
//! images — no chat model at all (the owner's rule). `+i` on a chat model is
//! the Responses API's `image_generation` tool instead, configured by
//! [`tool`] here with the same defaults.
//!
//! Sources (fetched 2026-11-13): the image generation guide and the image
//! generation tool guide, the Images API reference (create, edit, streaming
//! events), the Responses API reference (`image_generation` tool, output item,
//! `tool_usage`), the model pages, pricing, deprecations.
//!
//! Models: GPT-Image-2.6 Sunburst ("detailed visual work") and Flare ("fast
//! iteration"), recommended "for new integrations"; GPT-Image-2.5 Sunburst
//! and Flare; and the deprecated `gpt-image-2` (removed 2027-02-10),
//! `gpt-image-1.5`, `gpt-image-1-mini`, `chatgpt-image-latest` (removed
//! 2026-12-01). DALL·E 2 and 3 were shut down on 2026-05-12 and `gpt-image-1`
//! on 2026-10-23: errors, as is every `sora-*` id (OpenAI shut down the Videos
//! API and the Sora 2 models on 2026-09-24, with no replacement).
//!
//! What the bot sends (defaults = the most the model offers, D3):
//! - `quality`: the model's highest (`max` on 2.5/2.6, `high` before);
//!   `q=` picks any of its levels or `auto`.
//! - `size`: OpenAI's `size` fixes the aspect ratio as well, so "the largest
//!   size" needs a ratio. With `S W H w h` / `ar=`: the largest size of
//!   exactly that ratio; without one: `auto` (OpenAI picks size and ratio for
//!   the prompt). `1` `2` `4` pick the size class — about 1024² and 2048²
//!   pixels, and OpenAI's maximum (8,294,400 = 3840×2160) — at the given
//!   ratio, 1:1 when none is given. The 2.x models take any `WIDTHxHEIGHT`
//!   with both edges multiples of 16 and at most 3840, a ratio within
//!   1:3–3:1 and 655,360–8,294,400 pixels ("above 2560x1440 experimental");
//!   the older ones only 1024x1024, 1536x1024, 1024x1536.
//! - `moderation: "low"`, the lowest OpenAI offers (`auto` is the default).
//! - `background` from `bg=` (`auto`, `opaque`, `transparent`; PNG output).
//! - `n` from `n=` (1–10); `complexity` (GPT-Image-2.6: how much the model
//!   plans before rendering) from `e=`, `high` (the most) by default: OpenAI
//!   ties the two ("overrides the outer response's reasoning.effort for image
//!   generation"); `input_fidelity: "high"` on `gpt-image-1.5` edits (its
//!   default is `low`; 2.x always use high fidelity).
//! - One image: `stream: true` with `partial_images: 3` (the most): previews
//!   while it renders (each costs 100 more image output tokens). Several
//!   images: one plain JSON response, since stream events carry no image
//!   index to tell the previews of different images apart.
//! - `safety_identifier` = the hashed user. Output stays PNG (the default):
//!   `output_format` (JPEG, WebP, GIF, SVG), `output_compression`, GIF frame
//!   options, `svg`, masks and markup have no option in the bot.
//! - Edits: JSON `images: [{image_url: <data URL>}]`, up to 16, PNG, JPEG or
//!   WebP, each data URL at most 20,971,520 characters.
//!
//! Cost: estimated from the response's `usage` (text input, image input,
//! image output; GPT Image 1.5 and chatgpt-image-latest also bill text
//! output) at the per-model token prices below; the Images API has no
//! cached-input discount ("cached input pricing applies only to the image
//! generation tool in the Responses API").

use omni_core::Usage;
use omni_core::usage::CostSource;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::media::{self, Http, MediaInput};
use crate::error::{ErrorKind, LlmError, Retry};
use crate::event::StreamEvent;
use crate::openrouter::options::Applied;
use crate::provider::DirectConfig;
use crate::request::{self, Generation, Requested, RouteCaps, Tool};
use crate::{BlobLoader, CallInput, CallOutcome, EventSink, Observer, Prepared};

pub const DIALECT: &str = "openai.images";

const NAME: &str = "OpenAI";

/// The image model of the `image_generation` tool (`+i`): the best one,
/// GPT-Image-2.6 Sunburst, which is also OpenAI's default for the tool.
pub const TOOL_MODEL: &str = "gpt-image-2.6-sunburst";

/// Partial images (previews) requested while streaming: the most OpenAI allows.
pub const PARTIAL_IMAGES: u32 = 3;

/// USD per 1M tokens (standard processing), from the pricing page and the
/// model pages, 2026-11-13.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ImagePrice {
    pub text_in: f64,
    pub text_cached: f64,
    pub image_in: f64,
    pub image_cached: f64,
    pub image_out: f64,
    /// Text output (0: not billed, the model outputs images only).
    pub text_out: f64,
}

/// What an OpenAI image model takes.
#[derive(Debug, Clone, Copy)]
pub struct ImageSpec {
    pub id: &'static str,
    /// Dated snapshots and aliases.
    pub aliases: &'static [&'static str],
    /// `quality` levels, highest first (`auto` is accepted as well).
    pub qualities: &'static [&'static str],
    /// Any `WIDTHxHEIGHT` within OpenAI's limits (else the three standard sizes).
    pub flexible: bool,
    /// `complexity` (low / medium / high).
    pub complexity: bool,
    /// `input_fidelity: high` on edits.
    pub input_fidelity: bool,
    pub price: ImagePrice,
}

const MAX: &[&str] = &["max", "xhigh", "high", "medium", "low"];

const P2: ImagePrice =
    ImagePrice { text_in: 5.0, text_cached: 1.25, image_in: 8.0, image_cached: 2.0, image_out: 30.0, text_out: 0.0 };

const SPEC26: ImageSpec = ImageSpec {
    id: "",
    aliases: &[],
    qualities: MAX,
    flexible: true,
    complexity: true,
    input_fidelity: false,
    price: P2,
};

/// OpenAI's image models, 2026-11-13 (see the module docs). Quality levels:
/// the API reference and the guide's cost calculator ("for the Sunburst and
/// Flare models in both GPT-Image-2.6 and GPT-Image-2.5, the quality options
/// are low, medium, high, xhigh, and max"); flexible sizes: the `size`
/// reference names gpt-image-2 and 2.5, the 2.6 models are taken to be the
/// same (UNVERIFIED).
pub const IMAGE_MODELS: &[ImageSpec] = &[
    ImageSpec { id: "gpt-image-2.6-sunburst", ..SPEC26 },
    ImageSpec { id: "gpt-image-2.6-flare", ..SPEC26 },
    ImageSpec {
        id: "gpt-image-2.5-sunburst",
        aliases: &["gpt-image-2.5-sunburst-2026-09-08"],
        complexity: false,
        ..SPEC26
    },
    ImageSpec { id: "gpt-image-2.5-flare", aliases: &["gpt-image-2.5-flare-2026-09-08"], complexity: false, ..SPEC26 },
];

/// A bare image-model id (served by the Images API, not the Responses API).
pub fn is_image_model(model: &str) -> bool {
    model.starts_with("gpt-image-") || model.starts_with("chatgpt-image-") || model.starts_with("dall-e-")
}

pub fn image_spec(model: &str) -> Option<&'static ImageSpec> {
    IMAGE_MODELS.iter().find(|s| s.id == model || s.aliases.contains(&model))
}

// ------------------------------------------------------------------ sizes

/// Edge and pixel limits of the flexible sizes.
const MAX_EDGE: u32 = 3840;
const MIN_PIXELS: u64 = 655_360;
const MAX_PIXELS: u64 = 8_294_400;
/// Sizes above 2560x1440 are "experimental" (OpenAI).
const EXPERIMENTAL_ABOVE: u64 = 2560 * 1440;

/// Pixel budget of a size class (`1K`, `2K`, `4K`).
fn budget(class: &str) -> u64 {
    match class {
        "1K" => 1024 * 1024,
        "2K" => 2048 * 2048,
        _ => MAX_PIXELS,
    }
}

fn gcd(a: u32, b: u32) -> u32 {
    if b == 0 { a } else { gcd(b, a % b) }
}

/// `16:9` → (16, 9), reduced.
fn ratio(r: &str) -> Option<(u32, u32)> {
    let (a, b) = r.split_once(':')?;
    let (a, b): (u32, u32) = (a.parse().ok()?, b.parse().ok()?);
    if a == 0 || b == 0 {
        return None;
    }
    let g = gcd(a, b);
    Some((a / g, b / g))
}

/// The largest size of exactly `w:h` within `budget` pixels that the flexible
/// models take (edges multiples of 16 and at most 3840, ratio within
/// 1:3–3:1, at least 655,360 pixels).
pub(crate) fn largest_size(w: u32, h: u32, budget: u64) -> Result<(u32, u32), String> {
    if w > 3 * h || h > 3 * w {
        return Err("OpenAI takes aspect ratios between 1:3 and 3:1".into());
    }
    // Both edges multiples of 16: w·t and h·t with t a multiple of `step`.
    let step = |x: u32| 16 / gcd(16, x);
    let (sw, sh) = (step(w), step(h));
    let lcm = sw / gcd(sw, sh) * sh;
    let (uw, uh) = (u64::from(w * lcm), u64::from(h * lcm));
    let mut k = ((budget as f64 / (uw * uh) as f64).sqrt()) as u64;
    while k > 0 && (uw * k * uh * k > budget || uw.max(uh) * k > u64::from(MAX_EDGE)) {
        k -= 1;
    }
    while uw * (k + 1) * uh * (k + 1) <= budget && uw.max(uh) * (k + 1) <= u64::from(MAX_EDGE) {
        k += 1;
    }
    let (width, height) = (uw * k, uh * k);
    if k == 0 || width * height < MIN_PIXELS {
        return Err(format!(
            "no size of exactly {w}:{h} fits OpenAI's limits (edges multiples of 16, at most {MAX_EDGE}, {MIN_PIXELS}–{MAX_PIXELS} pixels)"
        ));
    }
    Ok((width as u32, height as u32))
}

/// Width and height.
type Wh = (u32, u32);

/// The standard sizes of the older models, by ratio.
const STANDARD: &[(Wh, Wh)] = &[((1, 1), (1024, 1024)), ((3, 2), (1536, 1024)), ((2, 3), (1024, 1536))];

/// A chosen `size`: `None` = `auto`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Size {
    pub wh: Option<(u32, u32)>,
    /// e.g. `size 3840x2160 (4K, 16:9)` (`size auto` when not sent).
    pub desc: String,
    pub note: Option<String>,
}

/// The `size` for the options (see the module docs). `flexible`: the model
/// takes any size; `None` for models not in the table (treated as flexible,
/// OpenAI checks).
pub(crate) fn choose_size(
    model: &str,
    flexible: bool,
    class: Option<&str>,
    ratio_opt: Option<&str>,
) -> Result<Size, LlmError> {
    if class.is_none() && ratio_opt.is_none() {
        return Ok(Size { wh: None, desc: "size auto".into(), note: None });
    }
    let r = ratio_opt.unwrap_or("1:1");
    let Some((w, h)) = ratio(r) else {
        return Err(LlmError::request(format!("aspect ratio {r} is not available for `{model}`")));
    };
    if !flexible {
        if let Some(c) = class.filter(|c| *c != "1K") {
            return Err(LlmError::request(format!("image size {c} is not available for `{model}`; sizes: 1K")));
        }
        let Some((_, wh)) = STANDARD.iter().find(|(k, _)| *k == (w, h)) else {
            return Err(LlmError::request(format!(
                "aspect ratio {r} is not available for `{model}`; ratios: 1:1, 3:2, 2:3 (1024x1024, 1536x1024, 1024x1536)"
            )));
        };
        return Ok(Size { wh: Some(*wh), desc: format!("size {}x{} ({r})", wh.0, wh.1), note: None });
    }
    let c = class.unwrap_or("4K");
    let (width, height) =
        largest_size(w, h, budget(c)).map_err(|e| LlmError::request(format!("aspect ratio {r} for `{model}`: {e}")))?;
    let note = (u64::from(width) * u64::from(height) > EXPERIMENTAL_ABOVE)
        .then(|| format!("ℹ️ OpenAI calls sizes above 2560x1440 experimental ({width}x{height})"));
    Ok(Size { wh: Some((width, height)), desc: format!("size {width}x{height} ({c}, {r})"), note })
}

// ------------------------------------------------------------------ options

/// What the Images API takes for a model (the vendor's own API: official = route).
struct Caps(Option<&'static ImageSpec>);

const COMPLEXITY: &[&str] = &["high", "medium", "low"];

impl RouteCaps for Caps {
    fn name(&self) -> &str {
        NAME
    }
    fn tool(&self, _: Tool) -> bool {
        false
    }
    fn efforts(&self) -> Option<Vec<String>> {
        // `e=` sets `complexity`, the image model's planning effort.
        self.0.is_none_or(|s| s.complexity).then(|| COMPLEXITY.iter().map(|e| e.to_string()).collect())
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
        Generation { quality: true, count: true, background: true, seconds: false }
    }
}

const BACKGROUNDS: &[&str] = &["auto", "opaque", "transparent"];
/// `n`: "Must be between 1 and 10".
const MAX_N: u32 = 10;
/// Edits: "up to 16 images"; each `image_url` at most 20,971,520 characters.
const MAX_INPUT_IMAGES: usize = 16;
const MAX_IMAGE_URL: usize = 20_971_520;
const INPUT_TYPES: &[&str] = &["image/png", "image/jpeg", "image/webp"];

fn src(by_user: bool) -> String {
    if by_user { "option" } else { "default" }.into()
}

/// Check `q=` and `bg=` for an image model (`None`: not in the table, OpenAI checks).
fn check_quality_background(model: &str, spec: Option<&ImageSpec>, req: &Requested) -> Result<(), LlmError> {
    if let (Some(q), Some(s)) = (&req.quality, spec)
        && q != "auto"
        && !s.qualities.contains(&q.as_str())
    {
        return Err(LlmError::request(format!(
            "image quality `{q}` is not available for `{model}`; levels: {}, auto",
            s.qualities.join(", ")
        )));
    }
    if let Some(b) = &req.background
        && !BACKGROUNDS.contains(&b.as_str())
    {
        return Err(LlmError::request(format!(
            "background `{b}` is not available for `{model}`; backgrounds: {}",
            BACKGROUNDS.join(", ")
        )));
    }
    Ok(())
}

/// The `image_generation` tool of the Responses API (`+i`): the plan for its
/// parameters.
pub(crate) struct ToolPlan {
    pub tool: Value,
    pub applied: Vec<Applied>,
    pub settings: Vec<String>,
    pub notes: Vec<String>,
}

/// The `image_generation` tool with the bot's defaults (see the module docs):
/// [`TOOL_MODEL`], its highest quality, the size from the options,
/// `moderation: "low"`, the background from `bg=`, three partial images. Its
/// `complexity` is left to OpenAI, which takes it from the request's
/// reasoning effort when that is one of its levels (else `high`).
pub(crate) fn tool(chat_model: &str, req: &Requested, user: &Requested) -> Result<ToolPlan, LlmError> {
    let spec = image_spec(TOOL_MODEL).expect("the tool model is in the table");
    check_quality_background(TOOL_MODEL, Some(spec), req)?;
    let quality = req.quality.clone().unwrap_or_else(|| spec.qualities[0].to_string());
    let size = choose_size(TOOL_MODEL, spec.flexible, req.image_size.as_deref(), req.aspect_ratio.as_deref())?;
    let mut tool = json!({
        "type": "image_generation",
        "model": TOOL_MODEL,
        "quality": quality,
        "moderation": "low",
        "partial_images": PARTIAL_IMAGES,
    });
    let mut applied = vec![Applied {
        source: "default".into(),
        patch: json!({"tools": [{"type": "image_generation", "model": TOOL_MODEL, "moderation": "low", "partial_images": PARTIAL_IMAGES}]}),
    }];
    applied.push(Applied {
        source: src(user.quality.is_some()),
        patch: json!({"tools": [{"type": "image_generation", "quality": quality}]}),
    });
    let mut settings = vec![format!("image generation ({TOOL_MODEL})"), format!("image quality {quality}")];
    if let Some((w, h)) = size.wh {
        tool["size"] = json!(format!("{w}x{h}"));
        applied.push(Applied {
            source: src(user.image_size.is_some() || user.aspect_ratio.is_some()),
            patch: json!({"tools": [{"type": "image_generation", "size": tool["size"]}]}),
        });
        settings.push(format!("image {}", size.desc));
    }
    if let Some(b) = &req.background {
        tool["background"] = json!(b);
        applied.push(Applied {
            source: src(user.background.is_some()),
            patch: json!({"tools": [{"type": "image_generation", "background": b}]}),
        });
        settings.push(format!("background {b}"));
    }
    settings.push("moderation low".into());
    let mut notes: Vec<String> = size.note.into_iter().collect();
    if req.effort.as_deref().is_some_and(|e| COMPLEXITY.contains(&e)) {
        notes.push(format!(
            "ℹ️ OpenAI uses the reasoning effort of `{chat_model}` as the image complexity too (e={})",
            req.effort.as_deref().unwrap_or("")
        ));
    }
    Ok(ToolPlan { tool, applied, settings, notes })
}

// ------------------------------------------------------------------ request

/// Build the Images API request for a bare image-model id. `req`: the
/// options with presets applied; `user`: the user's own (for `applied`).
pub(crate) async fn prepare(
    cfg: &DirectConfig,
    input: &CallInput<'_>,
    req: &Requested,
    user: &Requested,
    blobs: &dyn BlobLoader,
) -> Result<Prepared, LlmError> {
    let model = input.model;
    let spec = image_spec(model);
    if req.tools.contains(&Tool::Image) {
        return Err(LlmError::request(format!(
            "`{model}` is an image model: it makes images by itself (`+i` is the image generation tool of chat models)"
        )));
    }
    let mut checked = req.clone();
    checked.image_size = None;
    checked.aspect_ratio = None;
    checked.verbosity = None;
    request::validate(model, &checked, &Caps(spec)).map_err(LlmError::request)?;
    if req.verbosity.is_some() {
        return Err(LlmError::request(format!("`{model}` has no verbosity setting")));
    }
    check_quality_background(model, spec, req)?;
    let n = req.count.unwrap_or(1);
    if n > MAX_N {
        return Err(LlmError::request(format!("`{model}` makes at most {MAX_N} images per request (n={n})")));
    }
    let size =
        choose_size(model, spec.is_none_or(|s| s.flexible), req.image_size.as_deref(), req.aspect_ratio.as_deref())?;

    let MediaInput { prompt, answer_images, answer_video, own_images, own_videos, mut notes } =
        media::media_input(input.history, model, "the Images API")?;
    if !own_videos.is_empty() {
        return Err(LlmError::request(format!("`{model}` takes images, not video, via the Images API")));
    }
    if answer_video.is_some() {
        return Err(LlmError::request(format!(
            "`{model}` takes images, not the video of the answer replied to, via the Images API"
        )));
    }
    if prompt.is_empty() {
        return Err(LlmError::request(format!("`{model}` needs a prompt (the text of the message)")));
    }
    let images: Vec<_> = answer_images.iter().chain(&own_images).collect();
    if images.len() > MAX_INPUT_IMAGES {
        return Err(LlmError::request(format!(
            "`{model}` takes at most {MAX_INPUT_IMAGES} input images per edit ({} given)",
            images.len()
        )));
    }
    let edit = !images.is_empty();
    let mut body = json!({"model": model, "prompt": prompt});
    let mut applied = Vec::new();
    let mut settings = Vec::new();
    if edit {
        let mut refs = Vec::new();
        for b in &images {
            let url =
                media::image_data_url(b, INPUT_TYPES, Some(MAX_IMAGE_URL), model, "the Images API", blobs).await?;
            refs.push(json!({"image_url": url}));
        }
        body["images"] = json!(refs);
        settings.push(match (answer_images.len(), own_images.len()) {
            (0, k) => format!("edit ({k} input image{})", if k == 1 { "" } else { "s" }),
            (a, 0) => format!("edit of the previous image{}", if a == 1 { "" } else { "s" }),
            (a, k) => format!("edit of the previous image{} + {k} input image{}", plural(a), plural(k)),
        });
    }
    if let Some(s) = spec {
        let quality = req.quality.clone().unwrap_or_else(|| s.qualities[0].to_string());
        body["quality"] = json!(quality);
        applied.push(Applied { source: src(user.quality.is_some()), patch: json!({"quality": quality}) });
        settings.insert(0, format!("quality {quality}"));
        if s.complexity {
            let c = req.effort.clone().unwrap_or_else(|| COMPLEXITY[0].to_string());
            body["complexity"] = json!(c);
            applied.push(Applied { source: src(user.effort.is_some()), patch: json!({"complexity": c}) });
            settings.insert(1, format!("complexity {c}"));
        }
        body["moderation"] = json!("low");
        applied.push(Applied { source: "default".into(), patch: json!({"moderation": "low"}) });
        if edit && s.input_fidelity {
            body["input_fidelity"] = json!("high");
            applied.push(Applied { source: "default".into(), patch: json!({"input_fidelity": "high"}) });
            settings.push("input fidelity high".into());
        }
    } else {
        // Not in the table: only what was asked for.
        if let Some(q) = &req.quality {
            body["quality"] = json!(q);
            applied.push(Applied { source: src(user.quality.is_some()), patch: json!({"quality": q}) });
            settings.insert(0, format!("quality {q}"));
        }
        if let Some(c) = &req.effort {
            body["complexity"] = json!(c);
            applied.push(Applied { source: src(user.effort.is_some()), patch: json!({"complexity": c}) });
            settings.push(format!("complexity {c}"));
        }
    }
    if let Some((w, h)) = size.wh {
        body["size"] = json!(format!("{w}x{h}"));
        applied.push(Applied {
            source: src(user.image_size.is_some() || user.aspect_ratio.is_some()),
            patch: json!({"size": body["size"]}),
        });
        settings.push(size.desc.clone());
    }
    notes.extend(size.note);
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
    if spec.is_some() {
        settings.push("moderation low".into());
        if n == 1 {
            body["stream"] = json!(true);
            body["partial_images"] = json!(PARTIAL_IMAGES);
            applied.push(Applied {
                source: "default".into(),
                patch: json!({"stream": true, "partial_images": PARTIAL_IMAGES}),
            });
        }
    }
    if let Some(u) = &input.user {
        body["safety_identifier"] = json!(u);
    }
    let warnings = Vec::new();
    Ok(Prepared {
        dialect: DIALECT,
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

// ------------------------------------------------------------------ cost

/// Cost in USD of an image `usage` object (the Images API's `usage`, or the
/// Responses API's `tool_usage.image_gen`): text and image input, image and
/// text output. `cache_discount`: cached counts (where reported) at the cached
/// rates; not on the Images API ("cached input pricing … doesn't apply to
/// direct Images API requests", OpenAI's image generation guide, 2026-11-13).
pub fn usage_cost(p: &ImagePrice, u: &Value, cache_discount: bool) -> f64 {
    let g = |ptr: &str| u.pointer(ptr).and_then(Value::as_u64).unwrap_or(0) as f64;
    let input = g("/input_tokens");
    let (mut text_in, mut image_in) = (g("/input_tokens_details/text_tokens"), g("/input_tokens_details/image_tokens"));
    if u.pointer("/input_tokens_details").is_none() {
        text_in = input;
        image_in = 0.0;
    }
    let cached = |ptr: &str, of: f64| if cache_discount { g(ptr).min(of) } else { 0.0 };
    let cached_text = cached("/cached_tokens_details/cached_text_tokens", text_in);
    let cached_image = cached("/cached_tokens_details/cached_image_tokens", image_in);
    let output = g("/output_tokens");
    let (mut image_out, mut text_out) =
        (g("/output_tokens_details/image_tokens"), g("/output_tokens_details/text_tokens"));
    if u.pointer("/output_tokens_details").is_none() {
        image_out = output;
        text_out = 0.0;
    }
    ((text_in - cached_text) * p.text_in
        + cached_text * p.text_cached
        + (image_in - cached_image) * p.image_in
        + cached_image * p.image_cached
        + image_out * p.image_out
        + text_out * p.text_out)
        / 1e6
}

/// Normalized usage of an Images API response, with the estimated cost.
fn image_usage(model: &str, u: &Value) -> Usage {
    let g = |p: &str| u.pointer(p).and_then(Value::as_u64);
    let cost = image_spec(model).map(|s| usage_cost(&s.price, u, false));
    Usage {
        input_tokens: g("/input_tokens"),
        output_tokens: g("/output_tokens"),
        cost_usd: cost,
        cost_source: if cost.is_some() { CostSource::Estimated } else { CostSource::Unknown },
        raw: u.clone(),
        ..Default::default()
    }
}

// ------------------------------------------------------------------ stream

/// `image/png` for the `output_format` an event reports (PNG by default).
pub(crate) fn format_mime(f: Option<&Value>) -> String {
    match f.and_then(Value::as_str) {
        Some("jpeg") => "image/jpeg".into(),
        Some("webp") => "image/webp".into(),
        Some("gif") => "image/gif".into(),
        _ => "image/png".into(),
    }
}

/// The Images API's stream (`image_generation.*` / `image_edit.*` events):
/// partial images become previews, a completed event the image, its usage
/// and the finish.
pub(crate) struct Parser {
    model: String,
    finished: bool,
    images: usize,
    /// The completed events without their image data (native payload).
    completed: Vec<Value>,
}

impl Parser {
    pub fn new(model: &str) -> Self {
        Self { model: model.to_string(), finished: false, images: 0, completed: Vec::new() }
    }

    pub fn native_payload(&self) -> Value {
        json!({"events": self.completed})
    }
}

impl crate::chatstream::EventParser for Parser {
    fn event(&mut self, name: Option<&str>, v: &Value) -> Result<Vec<StreamEvent>, LlmError> {
        let ty = v.get("type").and_then(Value::as_str).or(name).unwrap_or("");
        let image = |v: &Value| v.get("b64_json").and_then(Value::as_str).and_then(media::decode_b64);
        match ty {
            "image_generation.partial_image" | "image_edit.partial_image" => {
                let Some(data) = image(v) else { return Ok(Vec::new()) };
                let mime = media::sniff_image(&data, &format_mime(v.get("output_format")));
                Ok(vec![StreamEvent::PartialImage { data, mime, slot: self.images.to_string() }])
            }
            "image_generation.completed" | "image_edit.completed" => {
                let slot = self.images.to_string();
                let Some(data) = image(v) else {
                    let mut e = LlmError::new(ErrorKind::InBand, Retry::Never, "the completed event carries no image");
                    e.details = Some(media::without_media(v, "b64_json"));
                    return Err(e);
                };
                self.images += 1;
                self.finished = true;
                self.completed.push(media::without_media(v, "b64_json"));
                let mime = media::sniff_image(&data, &format_mime(v.get("output_format")));
                let mut evs = vec![StreamEvent::Image { data, mime }, StreamEvent::PreviewDone { slot }];
                if let Some(u) = v.get("usage").filter(|u| u.is_object()) {
                    evs.push(StreamEvent::Usage(image_usage(&self.model, u)));
                }
                evs.push(StreamEvent::Finish { reason: Some("stop".into()), native: Some(ty.to_string()) });
                Ok(evs)
            }
            _ if ty == "error" || v.get("error").is_some_and(|e| !e.is_null()) => {
                let err = v.get("error").filter(|e| e.is_object()).unwrap_or(v);
                let mut e = super::error_from_json(err, None, None, ErrorKind::InBand);
                e.partial_output = self.images > 0;
                e.details = Some(v.clone());
                Err(e)
            }
            _ => Ok(Vec::new()),
        }
    }
    fn finished(&self) -> bool {
        self.finished
    }
    fn any_output(&self) -> bool {
        self.images > 0
    }
}

/// Run a prepared Images API request: streamed (one image, with previews) or
/// a plain JSON response (several images).
pub(crate) async fn stream(
    http: &reqwest::Client,
    cfg: &DirectConfig,
    p: &Prepared,
    sink: &mut dyn EventSink,
    observer: &dyn Observer,
    cancel: &CancellationToken,
) -> (Result<Value, LlmError>, CallOutcome) {
    if p.body.get("stream").and_then(Value::as_bool) == Some(true) {
        let rb = http.post(&p.url).bearer_auth(&cfg.api_key).json(&p.body);
        let crate::chatstream::Streamed { result, parser, header_id } = crate::chatstream::run_with(
            rb,
            &p.url,
            &p.body,
            cfg.idle_timeout,
            Some("x-request-id"),
            super::error_from_json,
            Parser::new(&p.model),
            sink,
            observer,
            cancel,
        )
        .await;
        let outcome = CallOutcome { upstream_id: header_id, ..Default::default() };
        let result = match result {
            // `data: [DONE]` before the completed event: cut off.
            Ok(()) if !crate::chatstream::EventParser::finished(&parser) => {
                let mut e = LlmError::network("stream ended without the completed event");
                e.partial_output = crate::chatstream::EventParser::any_output(&parser);
                Err(e)
            }
            r => r.map(|()| parser.native_payload()),
        };
        return (result, outcome);
    }
    let h = Http {
        client: http,
        api_key: &cfg.api_key,
        idle_timeout: cfg.idle_timeout,
        map_error: super::error_from_json,
        refine: std::convert::identity,
        observer,
        cancel,
    };
    let reply = match h.post(&p.url, &p.body).await {
        Ok(r) => r,
        Err(e) => return (Err(e), CallOutcome::default()),
    };
    let outcome = CallOutcome { upstream_id: reply.request_id.clone(), ..Default::default() };
    if reply.request_id.is_some() {
        sink.event(StreamEvent::Meta { upstream_id: reply.request_id.clone(), provider: None, model: None });
    }
    (images_of(&p.model, &reply.body, sink), outcome)
}

/// The images (in order), usage and finish of a JSON Images API response.
fn images_of(model: &str, body: &Value, sink: &mut dyn EventSink) -> Result<Value, LlmError> {
    let data: Vec<&Value> = body.get("data").and_then(Value::as_array).into_iter().flatten().collect();
    let mut n = 0;
    for d in &data {
        if let Some(bytes) = d.get("b64_json").and_then(Value::as_str).and_then(media::decode_b64) {
            let mime = media::sniff_image(&bytes, &format_mime(body.get("output_format")));
            sink.event(StreamEvent::Image { data: bytes, mime });
            n += 1;
        }
    }
    if let Some(u) = body.get("usage").filter(|u| u.is_object()) {
        sink.event(StreamEvent::Usage(image_usage(model, u)));
    }
    if n == 0 {
        let mut e = LlmError::new(ErrorKind::InBand, Retry::Never, "the response carries no image");
        e.details = Some(media::without_media(body, "b64_json"));
        return Err(e);
    }
    sink.event(StreamEvent::Finish { reason: Some("stop".into()), native: None });
    Ok(media::without_media(body, "b64_json"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::openai::wire::tests::{MemBlobs, photo, serve, user};
    use omni_core::Turn;
    use omni_core::content::{Part, Role};
    use omni_core::trigger::OptionToken;
    use std::time::Duration;

    fn cfg(base: &str) -> DirectConfig {
        DirectConfig {
            api_key: "sk-test".into(),
            base_url: base.into(),
            connect_timeout: Duration::from_secs(5),
            idle_timeout: Duration::from_secs(5),
            max_tokens_cap: None,
            cny_per_usd: 7.1,
            long_decode_budget: 393_216,
        }
    }

    fn openai() -> crate::openai::OpenAi {
        crate::openai::OpenAi::new(cfg("https://api.openai.com/v1")).unwrap()
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
        use crate::provider::Provider;
        let input = CallInput {
            model,
            options: opts,
            history,
            session_id: Some("tg-1".into()),
            user: Some("u-hash".into()),
            known_context: None,
            max_tokens_override: None,
        };
        openai().prepare(&input, &[], blobs).await
    }

    async fn prep(model: &str, opts: &[OptionToken]) -> Result<Prepared, LlmError> {
        prep_with(model, opts, &[Turn::user_text("a cat")], &MemBlobs(vec![])).await
    }

    async fn err(model: &str, opts: &[OptionToken]) -> String {
        prep(model, opts).await.unwrap_err().message
    }

    #[test]
    fn sizes_are_the_largest_of_the_exact_ratio() {
        assert_eq!(largest_size(16, 9, budget("4K")), Ok((3840, 2160)));
        assert_eq!(largest_size(9, 16, budget("4K")), Ok((2160, 3840)));
        assert_eq!(largest_size(1, 1, budget("4K")), Ok((2880, 2880)));
        assert_eq!(largest_size(1, 1, budget("2K")), Ok((2048, 2048)));
        assert_eq!(largest_size(1, 1, budget("1K")), Ok((1024, 1024)));
        assert_eq!(largest_size(16, 9, budget("2K")), Ok((2560, 1440)));
        assert_eq!(largest_size(16, 9, budget("1K")), Ok((1280, 720)));
        assert_eq!(largest_size(4, 3, budget("4K")), Ok((3264, 2448)));
        // 21:9 = 7:3: capped by the 3840 edge before the pixel budget.
        assert_eq!(largest_size(7, 3, budget("4K")), Ok((3808, 1632)));
        assert_eq!(largest_size(3, 1, budget("4K")), Ok((3840, 1280)));
        for (w, h) in [(16, 9), (4, 3), (3, 2), (7, 3), (5, 4), (1, 3), (2, 1)] {
            for c in ["1K", "2K", "4K"] {
                let (a, b) = largest_size(w, h, budget(c)).unwrap();
                assert!(a % 16 == 0 && b % 16 == 0 && a.max(b) <= 3840, "{w}:{h} {c}: {a}x{b}");
                let px = u64::from(a) * u64::from(b);
                assert!((MIN_PIXELS..=budget(c)).contains(&px), "{w}:{h} {c}: {a}x{b}");
                assert_eq!(u64::from(a) * u64::from(h), u64::from(b) * u64::from(w), "exact ratio");
            }
        }
        assert!(largest_size(4, 1, MAX_PIXELS).unwrap_err().contains("between 1:3 and 3:1"));
        assert!(largest_size(997, 991, MAX_PIXELS).unwrap_err().contains("no size of exactly"));
        assert_eq!(choose_size("m", true, None, None).unwrap().wh, None);
        let s = choose_size("m", true, Some("2K"), None).unwrap();
        assert_eq!((s.wh, s.desc.as_str()), (Some((2048, 2048)), "size 2048x2048 (2K, 1:1)"));
        assert!(choose_size("m", true, None, Some("16:9")).unwrap().note.unwrap().contains("experimental"));
    }

    #[tokio::test]
    async fn defaults_are_the_most_the_model_offers() {
        let p = prep("gpt-image-2.6-sunburst", &[]).await.unwrap();
        assert_eq!(p.dialect, DIALECT);
        assert_eq!(p.url, "https://api.openai.com/v1/images/generations");
        assert_eq!(
            p.body,
            json!({"model": "gpt-image-2.6-sunburst", "prompt": "a cat", "quality": "max", "complexity": "high",
                "moderation": "low", "stream": true, "partial_images": 3, "safety_identifier": "u-hash"})
        );
        assert_eq!(p.settings, ["quality max", "complexity high", "moderation low"]);
        assert!(p.applied.iter().all(|a| a.source == "default"));
        assert!(p.warnings.is_empty() && p.notes.is_empty());
        // 2.5: the same highest quality, no complexity (also by its dated alias).
        for model in ["gpt-image-2.5-sunburst", "gpt-image-2.5-flare-2026-09-08"] {
            let p = prep(model, &[]).await.unwrap();
            assert_eq!(p.body["quality"], "max", "{model}");
            assert!(p.body.get("complexity").is_none(), "{model}");
            assert!(p.warnings.is_empty(), "{model}: {:?}", p.warnings);
        }
        // Not in the table (newer, deprecated and shut-down ids alike): not supported, the price is unknown.
        for model in [
            "gpt-image-3",
            "gpt-image-2",
            "gpt-image-2-2026-04-21",
            "gpt-image-1.5",
            "gpt-image-1-mini",
            "chatgpt-image-latest",
            "gpt-image-1",
            "dall-e-3",
            "sora-2-pro",
        ] {
            assert_eq!(
                err(model, &[kv("q", "high")]).await,
                format!(
                    "`{model}` is not in the bot's OpenAI model table (its price is unknown), so it is not supported"
                )
            );
        }
    }

    #[tokio::test]
    async fn options_map_to_the_request() {
        let p = prep(
            "gpt-image-2.6-flare",
            &[flag("W"), kv("q", "low"), kv("bg", "transparent"), kv("n", "3"), kv("e", "low")],
        )
        .await
        .unwrap();
        let b = &p.body;
        assert_eq!(
            (b["size"].clone(), b["quality"].clone(), b["background"].clone()),
            (json!("3840x2160"), json!("low"), json!("transparent"))
        );
        assert_eq!((b["n"].clone(), b["complexity"].clone()), (json!(3), json!("low")));
        // Several images: no stream (previews could not be told apart).
        assert!(b.get("stream").is_none() && b.get("partial_images").is_none());
        assert_eq!(
            p.settings,
            [
                "quality low",
                "complexity low",
                "size 3840x2160 (4K, 16:9)",
                "background transparent",
                "3 images",
                "moderation low"
            ]
        );
        assert!(p.notes[0].contains("experimental"));
        assert_eq!(prep("gpt-image-2.6-flare", &[flag("1h")]).await.unwrap().body["size"], "864x1152");
        assert_eq!(prep("gpt-image-2.5-flare", &[kv("ar", "21:9")]).await.unwrap().body["size"], "3808x1632");
        let p = prep("gpt-image-2.5-sunburst", &[kv("bg", "transparent")]).await.unwrap();
        assert_eq!(p.body["background"], "transparent");
        assert!(p.notes.is_empty(), "{:?}", p.notes);
        assert_eq!(prep("gpt-image-2.5-sunburst", &[kv("q", "auto")]).await.unwrap().body["quality"], "auto");
    }

    #[tokio::test]
    async fn what_the_images_api_does_not_take_is_an_error() {
        assert!(
            err("gpt-image-2.6-sunburst", &[kv("q", "ultra")])
                .await
                .contains("levels: max, xhigh, high, medium, low, auto")
        );
        assert!(
            err("gpt-image-2.6-sunburst", &[kv("bg", "plaid")])
                .await
                .contains("backgrounds: auto, opaque, transparent")
        );
        assert!(err("gpt-image-2.6-sunburst", &[kv("n", "11")]).await.contains("at most 10 images"));
        assert!(err("gpt-image-2.6-sunburst", &[kv("e", "max")]).await.contains("levels: high, medium, low"));
        assert!(err("gpt-image-2.5-sunburst", &[kv("e", "high")]).await.contains("has no reasoning effort levels"));
        assert!(err("gpt-image-2.6-sunburst", &[kv("sec", "5")]).await.contains("(`sec=`) via OpenAI"));
        assert!(err("gpt-image-2.6-sunburst", &[flag("s")]).await.contains("has no web search"));
        assert!(err("gpt-image-2.6-sunburst", &[flag("i")]).await.contains("makes images by itself"));
        assert!(err("gpt-image-2.6-sunburst", &[kv("tier", "flex")]).await.contains("no service tiers"));
        assert!(err("gpt-image-2.6-sunburst", &[kv("v", "low")]).await.contains("no verbosity"));
        assert!(err("gpt-image-2.6-sunburst", &[kv("ar", "4:1")]).await.contains("between 1:3 and 3:1"));
        // A system prompt cannot be sent.
        let sys = Turn { role: Role::System, origin: Default::default(), parts: vec![Part::text("x")], native: None };
        let e =
            prep_with("gpt-image-2.6-flare", &[], &[sys, Turn::user_text("a")], &MemBlobs(vec![])).await.unwrap_err();
        assert!(e.message.contains("no system prompt"), "{}", e.message);
    }

    #[tokio::test]
    async fn images_make_an_edit_and_replies_edit_the_previous_image() {
        let (img, b1) = photo("image/webp", b"RIFF\0\0\0\0WEBPown");
        let prev = omni_core::BlobRef::for_bytes(b"\x89PNG-prev", "image/png");
        let blobs = MemBlobs(vec![b1, (prev.clone(), b"\x89PNG-prev".to_vec())]);
        // A message with an image: an edit of it.
        let p = prep_with("gpt-image-2.5-flare", &[], &[user(vec![Part::text("watercolor"), img.clone()])], &blobs)
            .await
            .unwrap();
        assert_eq!(p.url, "https://api.openai.com/v1/images/edits");
        assert!(p.body["images"][0]["image_url"].as_str().unwrap().starts_with("data:image/webp;base64,"));
        assert!(p.body.get("input_fidelity").is_none(), "no model in the table takes it");
        assert!(p.settings.contains(&"edit (1 input image)".to_string()));
        // A reply to a generated image (plus an attached one): the previous image first.
        let answer = Turn {
            role: Role::Assistant,
            origin: Default::default(),
            parts: vec![Part::Image { blob: prev, filename: None, generated: true, dims: None, thought: false }],
            native: None,
        };
        let history = vec![Turn::user_text("a cat"), answer, user(vec![Part::text("add a hat"), img])];
        let p = prep_with("gpt-image-2.6-sunburst", &[], &history, &blobs).await.unwrap();
        let images = p.body["images"].as_array().unwrap();
        assert_eq!(images.len(), 2);
        assert!(images[0]["image_url"].as_str().unwrap().starts_with("data:image/png;base64,"));
        assert_eq!(p.body["prompt"], "add a hat");
        assert!(p.body.get("input_fidelity").is_none());
        assert!(p.settings.contains(&"edit of the previous image + 1 input image".to_string()));
        assert!(p.notes.is_empty());
        // GIF input is not taken by edits.
        let (gif, b3) = photo("image/gif", b"GIF89a");
        let e = prep_with("gpt-image-2.6-flare", &[], &[user(vec![Part::text("x"), gif])], &MemBlobs(vec![b3]))
            .await
            .unwrap_err();
        assert!(e.message.contains("formats: png, jpeg, webp"), "{}", e.message);
        // No prompt.
        let (img2, b4) = photo("image/png", b"\x89PNG");
        let e = prep_with("gpt-image-2.6-flare", &[], &[user(vec![img2])], &MemBlobs(vec![b4])).await.unwrap_err();
        assert!(e.message.contains("needs a prompt"));
        // A conversation without an image to edit: one prompt per request, so it would be ignored,
        // which is refused.
        let chat =
            vec![Turn::user_text("a story"), Turn::assistant_text("Once upon a time"), Turn::user_text("draw it")];
        let e = prep_with("gpt-image-2.6-flare", &[], &chat, &MemBlobs(vec![])).await.unwrap_err();
        assert_eq!(
            e.message,
            "`gpt-image-2.6-flare` takes one prompt via the Images API, not a conversation: start a new message, or reply to a generated image or video it can take as input"
        );
    }

    #[test]
    fn cost_follows_the_image_token_prices() {
        let p = image_spec("gpt-image-2.6-sunburst").unwrap().price;
        // 50 text + 1000 image input tokens, 4000 image output tokens.
        let u = json!({"input_tokens": 1050, "input_tokens_details": {"text_tokens": 50, "image_tokens": 1000},
            "output_tokens": 4000, "output_tokens_details": {"image_tokens": 4000, "text_tokens": 0}, "total_tokens": 5050});
        let c = usage_cost(&p, &u, false);
        assert!((c - (50.0 * 5.0 + 1000.0 * 8.0 + 4000.0 * 30.0) / 1e6).abs() < 1e-12, "{c}");
        // The tool's usage reports cached counts; text output is not billed on GPT Image 2.x.
        let u = json!({"input_tokens": 1100, "input_tokens_details": {"text_tokens": 100, "image_tokens": 1000},
            "cached_tokens": 600, "cached_tokens_details": {"cached_text_tokens": 100, "cached_image_tokens": 500},
            "output_tokens": 300, "output_tokens_details": {"image_tokens": 200, "text_tokens": 100}});
        let flare = image_spec("gpt-image-2.5-flare").unwrap().price;
        let c = usage_cost(&flare, &u, true);
        let want = (100.0 * 1.25 + 500.0 * 8.0 + 500.0 * 2.0 + 200.0 * 30.0) / 1e6;
        assert!((c - want).abs() < 1e-12, "{c} vs {want}");
        // The Images API has no cached-input pricing: the same counts at the input rates.
        let c = usage_cost(&flare, &u, false);
        let want = (100.0 * flare.text_in + 1000.0 * 8.0 + 200.0 * 30.0) / 1e6;
        assert!((c - want).abs() < 1e-12, "{c} vs {want}");
        // Without details: input as text, output as image tokens.
        let c = usage_cost(&p, &json!({"input_tokens": 10, "output_tokens": 100}), false);
        assert!((c - (10.0 * 5.0 + 100.0 * 30.0) / 1e6).abs() < 1e-12);
    }

    fn sse(events: &[Value]) -> Vec<String> {
        events.iter().map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap())).collect()
    }

    #[tokio::test]
    async fn a_streamed_image_shows_its_previews_then_the_image() {
        let p = prep("gpt-image-2.6-sunburst", &[]).await.unwrap();
        let png = |s: &str| crate::provider::b64(&[b"\x89PNG".as_slice(), s.as_bytes()].concat());
        let events = [
            json!({"type": "image_generation.partial_image", "b64_json": png("p0"), "partial_image_index": 0, "size": "1024x1024", "quality": "max", "background": "opaque", "output_format": "png", "created_at": 1}),
            json!({"type": "image_generation.partial_image", "b64_json": png("p1"), "partial_image_index": 1, "output_format": "png", "created_at": 2}),
            json!({"type": "image_generation.completed", "b64_json": png("final"), "size": "1024x1024", "quality": "max", "background": "opaque", "output_format": "png", "created_at": 3,
                "usage": {"total_tokens": 4300, "input_tokens": 50, "output_tokens": 4250, "input_tokens_details": {"text_tokens": 50, "image_tokens": 0}}}),
        ];
        let (base, captured) = serve(200, "text/event-stream", "x-request-id: req_img\r\n", sse(&events)).await;
        let o = crate::openai::OpenAi::new(cfg(&base)).unwrap();
        let p = Prepared { url: format!("{base}/images/generations"), ..p };
        let mut evs = Vec::new();
        let mut sink = |e: StreamEvent| evs.push(e);
        use crate::provider::Provider;
        let (r, outcome) = o.stream(&p, &mut sink, &crate::NullObserver, &CancellationToken::new()).await;
        let native = r.unwrap();
        assert_eq!(outcome.upstream_id.as_deref(), Some("req_img"));
        let req = captured.lock().unwrap().take().unwrap();
        assert_eq!((req.path.as_str(), req.body["stream"].clone()), ("/v1/images/generations", json!(true)));
        let kinds: Vec<String> = evs
            .iter()
            .map(|e| match e {
                StreamEvent::Meta { .. } => "meta".into(),
                StreamEvent::PartialImage { slot, data, .. } => format!("preview {slot} {}", data.len()),
                StreamEvent::Image { mime, .. } => format!("image {mime}"),
                StreamEvent::PreviewDone { slot } => format!("done {slot}"),
                StreamEvent::Usage(_) => "usage".into(),
                StreamEvent::Finish { reason, .. } => format!("finish {}", reason.clone().unwrap_or_default()),
                other => format!("{other:?}"),
            })
            .collect();
        assert_eq!(kinds, ["meta", "preview 0 6", "preview 0 6", "image image/png", "done 0", "usage", "finish stop"]);
        let usage =
            evs.iter().find_map(|e| if let StreamEvent::Usage(u) = e { Some(u.clone()) } else { None }).unwrap();
        assert!((usage.cost_usd.unwrap() - (50.0 * 5.0 + 4250.0 * 30.0) / 1e6).abs() < 1e-12);
        assert_eq!(usage.cost_source, CostSource::Estimated);
        // The native payload keeps the event, not the image bytes.
        assert_eq!(native["events"][0]["size"], "1024x1024");
        assert!(native["events"][0]["b64_json"].as_str().unwrap().contains("stored as the turn's media"));
    }

    #[tokio::test]
    async fn several_images_come_in_one_json_response() {
        let p = prep("gpt-image-2.5-sunburst", &[kv("n", "2")]).await.unwrap();
        let png = crate::provider::b64(b"\x89PNG-1");
        let jpg = crate::provider::b64(b"\xff\xd8\xff-2");
        let body = json!({"created": 1, "data": [{"b64_json": png}, {"b64_json": jpg}], "output_format": "png", "size": "1024x1024",
            "usage": {"input_tokens": 10, "input_tokens_details": {"text_tokens": 10, "image_tokens": 0}, "output_tokens": 2000, "total_tokens": 2010}});
        let (base, _) = serve(200, "application/json", "x-request-id: req_2\r\n", vec![body.to_string()]).await;
        let o = crate::openai::OpenAi::new(cfg(&base)).unwrap();
        let p = Prepared { url: format!("{base}/images/generations"), ..p };
        let mut evs = Vec::new();
        let mut sink = |e: StreamEvent| evs.push(e);
        use crate::provider::Provider;
        let (r, outcome) = o.stream(&p, &mut sink, &crate::NullObserver, &CancellationToken::new()).await;
        assert!(r.unwrap()["data"][0]["b64_json"].as_str().unwrap().contains("stored as"));
        assert_eq!(outcome.upstream_id.as_deref(), Some("req_2"));
        let images: Vec<&str> = evs
            .iter()
            .filter_map(|e| if let StreamEvent::Image { mime, .. } = e { Some(mime.as_str()) } else { None })
            .collect();
        assert_eq!(images, ["image/png", "image/jpeg"]);
        assert!(matches!(evs.last(), Some(StreamEvent::Finish { reason: Some(r), .. }) if r == "stop"));
    }

    #[tokio::test]
    async fn blocked_requests_and_stream_errors_are_failures() {
        // Probed shape of a moderation block (image generation guide).
        let body = r#"{"error":{"type":"image_generation_user_error","code":"moderation_blocked","message":"Your request was rejected by the safety system.","moderation_details":{"moderation_stage":"input","categories":["harassment"]}}}"#;
        let e = crate::chatstream::http_error(400, None, body, crate::openai::error_from_json);
        assert_eq!((e.retry, e.error_type.as_deref()), (Retry::Never, Some("moderation_blocked")));
        assert_eq!(e.details.unwrap()["error"]["moderation_details"]["categories"][0], "harassment");
        // An error event inside the stream.
        let p = prep("gpt-image-2.6-sunburst", &[]).await.unwrap();
        let ev = json!({"type": "error", "error": {"type": "image_generation_server_error", "code": "server_error", "message": "boom"}});
        let (base, _) = serve(200, "text/event-stream", "", sse(&[ev])).await;
        let o = crate::openai::OpenAi::new(cfg(&base)).unwrap();
        let p = Prepared { url: format!("{base}/images/generations"), ..p };
        use crate::provider::Provider;
        let (r, _) = o.stream(&p, &mut |_e: StreamEvent| {}, &crate::NullObserver, &CancellationToken::new()).await;
        let e = r.unwrap_err();
        assert_eq!(
            (e.kind, e.retry, e.error_type.as_deref()),
            (ErrorKind::InBand, Retry::Backoff, Some("server_error"))
        );
        // A stream cut before the completed event: retryable.
        let partial = json!({"type": "image_generation.partial_image", "b64_json": crate::provider::b64(b"\x89PNG"), "partial_image_index": 0});
        let (base, _) = serve(200, "text/event-stream", "", sse(&[partial])).await;
        let o = crate::openai::OpenAi::new(cfg(&base)).unwrap();
        let p = Prepared { url: format!("{base}/images/generations"), ..p };
        let (r, _) = o.stream(&p, &mut |_e: StreamEvent| {}, &crate::NullObserver, &CancellationToken::new()).await;
        assert_eq!(r.unwrap_err().kind, ErrorKind::Network);
    }
}
