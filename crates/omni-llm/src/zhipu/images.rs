//! Zhipu's image models on BigModel (`https://open.bigmodel.cn/api/paas/v4`):
//! GLM-Image and CogView. A bare image-model id is this API; Zhipu has no
//! chat route that outputs images (the chat tools are functions, retrieval,
//! web search and MCP; the Responses API's outputs are messages, reasoning,
//! function and search calls). Spec: docs.bigmodel.cn as fetched 2026-11-13
//! (`api-reference/模型-api/图像生成`, `…/图像生成异步`, `…/查询异步结果`,
//! `cn/guide/models/image-generation/glm-image`, `…/cogview-4`,
//! `cn/guide/start/pricing`, `cn/guide/start/model-overview`).
//!
//! - **Models** ([`MODELS`]): `glm-image` ("智谱新旗舰图像生成模型", the only
//!   image model in the overview's recommended table; ¥0.1 per image),
//!   `cogview-4-250304` and `cogview-4` (¥0.06), `cogview-3-flash` (free).
//!   One image per request ("目前数组中只包含一张图片"): no `n=`.
//! - **GLM-Image runs as a task**: `POST /async/images/generations` returns
//!   `{model, id, request_id, task_status}`; the bot checkpoints the id (a
//!   restart resumes polling, D14) and polls `GET /async-result/{id}` until
//!   `task_status` is `SUCCESS` (`image_result[].url`) or `FAIL` ("异步任务没有
//!   过期时间"). A `FAIL` without a reason is retried (the next attempt makes a
//!   new task). The other models only have `POST /images/generations`, which
//!   answers with `data[].url` (a restart retries).
//! - **Defaults**: the largest size (sides 512–2048 px; GLM-Image multiples of
//!   32 and at most 2²² px: 2048x2048, CogView multiples of 16 and at most 2²¹
//!   px: 1440x1440; the API defaults are 1280x1280 / 1024x1024), `quality: hd`
//!   (CogView's default is `standard`; GLM-Image has only `hd`), and
//!   `watermark_enabled: false` (the owner's "no watermark"; Zhipu: "关闭所有
//!   水印，仅允许已签署免责声明的客户使用", the disclaimer is signed under
//!   个人中心 → 安全管理 → 去水印管理). `user_id`: the bot's user hash (6–128
//!   characters). Zhipu documents no moderation switch.
//! - **Options**: `1` / `2` a size of about 1K / 2K pixels (2K: GLM-Image),
//!   `S W H w h ar=` the largest size of that ratio (computed: Zhipu has no
//!   ratio parameter, only pixel sizes), `q=hd|standard` (CogView).
//! - **Input**: the message's text only (GLM-Image: "最大输入 1000 字符"). Zhipu
//!   documents no image input, so images in the message are an error, and so
//!   is a reply to a generated image (it cannot be edited). Earlier messages
//!   are not sent (no conversation).
//! - **Output**: images as temporary URLs ("有效期为`30`天"), downloaded by the
//!   bot; `content_filter` is kept in the replay payload.
//! - Errors: Zhipu's `{"error":{"code","message"}}` ([`super::error_from_json`];
//!   1301 is the content filter, never retried).

use std::time::Duration;

use omni_core::Usage;
use omni_core::content::{Part, Role, Turn};
use omni_core::usage::CostSource;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::{NAME, error_from_json};
use crate::error::{ErrorKind, LlmError, Retry};
use crate::event::StreamEvent;
use crate::openrouter::options::{Applied, overlay};
use crate::provider::{Currency, DirectConfig, usd};
use crate::request::{self, Generation, Requested, RouteCaps, Tool};
use crate::{CallInput, CallOutcome, EventSink, Observer, Prepared};

pub const DIALECT: &str = "zhipu.images";
const ID_HEADER: &str = "x-log-id";
/// Between two polls of a task (GLM-Image takes about 20 s).
const POLL_EVERY: Duration = if cfg!(test) { Duration::from_millis(5) } else { Duration::from_secs(3) };
/// GLM-Image: "最大输入 1000 字符".
const GLM_IMAGE_PROMPT_CHARS: usize = 1000;

/// Pixel sizes a model takes (`size: "WxH"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sizes {
    /// Both sides are multiples of this.
    pub multiple: u32,
    pub min_side: u32,
    pub max_side: u32,
    pub max_pixels: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct Spec {
    pub id: &'static str,
    pub sizes: Sizes,
    /// `quality` values, best first.
    pub qualities: &'static [&'static str],
    /// Runs as a task (`/async/images/generations` + `/async-result/{id}`).
    pub task: bool,
    /// CNY per image.
    pub price_cny: f64,
}

/// "长宽需在 512px-2048px 范围内，且长宽均需为32的整数倍", "最大像素数不超过`2^22px`".
/// (The API reference "recommends" 1024–2048; the CN model guide's limit is 512.)
const GLM_IMAGE_SIZES: Sizes = Sizes { multiple: 32, min_side: 512, max_side: 2048, max_pixels: 1 << 22 };
/// "长宽均需满足`512px-2048px`之间，需被`16`整除，并保证最大像素数不超过`2^21px`".
const COGVIEW_SIZES: Sizes = Sizes { multiple: 16, min_side: 512, max_side: 2048, max_pixels: 1 << 21 };

const COGVIEW: Spec =
    Spec { id: "", sizes: COGVIEW_SIZES, qualities: &["hd", "standard"], task: false, price_cny: 0.06 };

/// Zhipu's image models and prices (2026-11-13).
pub const MODELS: &[Spec] = &[
    Spec { id: "glm-image", sizes: GLM_IMAGE_SIZES, qualities: &["hd"], task: true, price_cny: 0.1 },
    Spec { id: "cogview-4-250304", ..COGVIEW },
    Spec { id: "cogview-4", ..COGVIEW },
    Spec { id: "cogview-3-flash", price_cny: 0.0, ..COGVIEW },
];

pub fn spec(model: &str) -> Option<&'static Spec> {
    MODELS.iter().find(|m| m.id == model)
}

/// Whether `model` is one of Zhipu's image models (served by this module).
pub fn is_image_model(model: &str) -> bool {
    spec(model).is_some() || model.starts_with("cogview") || model.starts_with("glm-image")
}

/// The largest size of ratio `w:h` with about `target` pixels (at most the
/// model's maximum), within its side limits.
pub fn size_for(s: &Sizes, target: u64, ratio: (u64, u64)) -> Result<(u32, u32), String> {
    let (a, b) = ratio;
    let target = target.min(s.max_pixels) as f64;
    let r = a as f64 / b as f64;
    let (mut w, mut h) = ((target * r).sqrt(), (target / r).sqrt());
    let max = s.max_side as f64;
    if w > max {
        (w, h) = (max, max / r);
    }
    if h > max {
        (w, h) = (max * r, max);
    }
    let m = s.multiple as f64;
    let (w, h) = ((w / m).floor() * m, (h / m).floor() * m);
    let (w, h) = (w as u32, h as u32);
    if w < s.min_side || h < s.min_side || (w as u64) * (h as u64) > s.max_pixels {
        return Err(format!(
            "aspect ratio {a}:{b} has no size within {}–{} px per side (multiples of {}, at most {} pixels)",
            s.min_side, s.max_side, s.multiple, s.max_pixels
        ));
    }
    Ok((w, h))
}

/// What Zhipu's image API takes (the vendor's own API: official = route).
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
        self.0.is_some()
    }
    fn tiers(&self) -> Vec<String> {
        Vec::new()
    }
    fn generation(&self) -> Generation {
        Generation { quality: self.0.is_some(), ..Default::default() }
    }
}

fn err(m: impl Into<String>) -> LlmError {
    LlmError::request(m)
}

pub(super) fn ratio(r: &str) -> Option<(u64, u64)> {
    let (a, b) = r.split_once(':')?;
    let (a, b): (u64, u64) = (a.parse().ok()?, b.parse().ok()?);
    (a > 0 && b > 0).then_some((a, b))
}

/// The prompt (the new message's text) and whether the bot leaves out an image
/// or earlier messages (said in a note).
fn prompt(history: &[Turn], model: &str) -> Result<(String, Vec<String>), LlmError> {
    let last = history
        .iter()
        .rposition(|t| t.role == Role::User)
        .ok_or_else(|| err(format!("`{model}` needs a prompt: there is no user message")))?;
    if history.iter().any(|t| t.role == Role::System && !t.answer_text().trim().is_empty()) {
        return Err(err(format!("`{model}` takes no system prompt: Zhipu's image API has a prompt only")));
    }
    let mut texts = Vec::new();
    for p in &history[last].parts {
        match p {
            Part::Text { text, .. } => {
                if !text.is_empty() {
                    texts.push(text.as_str());
                }
            }
            Part::Image { .. } | Part::File { .. } | Part::RemoteMedia { .. } => {
                return Err(err(format!(
                    "`{model}` takes text only: Zhipu documents no image or file input for its image models"
                )));
            }
            Part::Location { .. } => return Err(crate::provider::no_location(model, NAME)),
            Part::Reasoning(_) | Part::ToolCall(_) | Part::Refusal { .. } => {}
        }
    }
    let prompt = texts.join("\n");
    if prompt.trim().is_empty() {
        return Err(err(format!("`{model}` needs a prompt: the message has no text")));
    }
    let notes = Vec::new();
    let replied_images = last
        .checked_sub(1)
        .map(|i| &history[i])
        .filter(|t| t.role == Role::Assistant)
        .is_some_and(|t| t.parts.iter().any(|p| matches!(p, Part::Image { thought: false, .. })));
    // It takes no input image and no conversation: a reply would ignore both (owner's rule: an error).
    if replied_images {
        return Err(err(format!(
            "`{model}` takes no input image, so it cannot edit the image replied to: start a new message"
        )));
    } else if history[..last].iter().any(|t| t.role != Role::System && !t.answer_text().trim().is_empty()) {
        return Err(err(format!("`{model}` takes one prompt, not a conversation: start a new message")));
    }
    Ok((prompt, notes))
}

/// Whether `req` asks for anything (`tier=default`/`auto` ask for nothing).
pub(super) fn has_options(req: &Requested) -> bool {
    let mut r = req.clone();
    if matches!(r.tier.as_deref(), Some("default" | "auto")) {
        r.tier = None;
    }
    r != Requested::default()
}

pub async fn prepare(cfg: &DirectConfig, input: &CallInput<'_>, presets: &[Requested]) -> Result<Prepared, LlmError> {
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
    let spec = spec(model);
    if spec.is_none() && has_options(&req) {
        return Err(err(format!(
            "`{model}` is not in the bot's Zhipu image model table, so its options cannot be checked: send it \
             without options"
        )));
    }
    let mut basic = req.clone();
    basic.image_size = None;
    basic.aspect_ratio = None;
    request::validate(model, &basic, &Caps(spec)).map_err(LlmError::request)?;
    let (prompt, notes) = prompt(input.history, model)?;

    let src = |by_user: bool| if by_user { "option" } else { "default" }.to_string();
    let mut body = json!({"model": model, "prompt": prompt});
    let mut applied = Vec::new();
    let mut settings = Vec::new();
    let mut warnings = Vec::new();
    match spec {
        Some(s) => {
            if s.id == "glm-image" && prompt.chars().count() > GLM_IMAGE_PROMPT_CHARS {
                return Err(err(format!(
                    "`{model}` takes at most {GLM_IMAGE_PROMPT_CHARS} characters of prompt (this one has {})",
                    prompt.chars().count()
                )));
            }
            let quality = req.quality.clone().unwrap_or_else(|| s.qualities[0].to_string());
            if !s.qualities.contains(&quality.as_str()) {
                return Err(err(format!(
                    "`{model}` has no quality `{quality}` (`q=`); qualities: {}",
                    s.qualities.join(", ")
                )));
            }
            let target = match req.image_size.as_deref() {
                None => s.sizes.max_pixels,
                Some("1K") => 1 << 20,
                Some("2K") if s.sizes.max_pixels >= 1 << 22 => 1 << 22,
                Some(t) => {
                    let sizes = if s.sizes.max_pixels >= 1 << 22 { "1K, 2K" } else { "1K" };
                    return Err(err(format!(
                        "image size {t} is not available for `{model}`; sizes: {sizes} (the default is the largest)"
                    )));
                }
            };
            let r = match req.aspect_ratio.as_deref() {
                Some(a) => ratio(a).ok_or_else(|| err(format!("`{a}` is not an aspect ratio")))?,
                None => (1, 1),
            };
            let (w, h) = size_for(&s.sizes, target, r).map_err(|e| err(format!("`{model}`: {e}")))?;
            let size = format!("{w}x{h}");
            body["quality"] = json!(quality);
            body["size"] = json!(size);
            settings.push(format!("quality {quality}"));
            settings.push(match &req.aspect_ratio {
                Some(a) => format!("size {size} ({a})"),
                None => format!("size {size}"),
            });
            applied.push(Applied { source: src(user.quality.is_some()), patch: json!({"quality": quality}) });
            applied.push(Applied {
                source: src(user.image_size.is_some() || user.aspect_ratio.is_some()),
                patch: json!({"size": size}),
            });
        }
        None => warnings.push(format!(
            "`{model}` is not in the bot's Zhipu image model table: sizes and price unknown (Zhipu's default size \
             and quality; cost not estimated)"
        )),
    }
    body["watermark_enabled"] = json!(false);
    settings.push("no watermark".into());
    applied.push(Applied { source: "default".into(), patch: json!({"watermark_enabled": false}) });
    if let Some(u) = input.user.as_deref().filter(|u| (6..=128).contains(&u.chars().count())) {
        body["user_id"] = json!(u);
    }
    let path = if spec.is_some_and(|s| s.task) { "/async/images/generations" } else { "/images/generations" };
    Ok(Prepared {
        dialect: DIALECT,
        model: model.to_string(),
        url: cfg.url(path),
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

/// The request that resumes a GLM-Image task from its checkpoint (after a
/// restart or a dropped poll): poll `/async-result/{id}` instead of creating it.
pub fn continuation_request(p: &Prepared, item: &Value) -> Prepared {
    let mut q = p.clone();
    if let Some(id) = item.get("task_id").and_then(Value::as_str) {
        let base = q.url.strip_suffix("/async/images/generations").unwrap_or(&q.url).to_string();
        q.url = format!("{base}/async-result/{id}");
    }
    q
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

/// One request's calls to BigModel (shared with [`super::videos`]): JSON
/// calls with the log id as `Meta`, and downloads of generated files.
pub(super) struct Run<'a> {
    pub(super) http: &'a reqwest::Client,
    pub(super) cfg: &'a DirectConfig,
    pub(super) sink: &'a mut dyn EventSink,
    pub(super) observer: &'a dyn Observer,
    pub(super) cancel: &'a CancellationToken,
}

impl Run<'_> {
    /// Send a request and read its JSON answer (errors classified as Zhipu's).
    pub(super) async fn call(
        &mut self,
        rb: reqwest::RequestBuilder,
        url: &str,
        body: &Value,
    ) -> Result<Value, LlmError> {
        self.observer.request(url, body);
        let timeout = self.cfg.idle_timeout;
        let resp = tokio::select! {
            _ = self.cancel.cancelled() => return Err(LlmError::cancelled()),
            r = tokio::time::timeout(timeout, rb.send()) => match r {
                Err(_) => return Err(LlmError::new(ErrorKind::Timeout, Retry::Backoff, "no response headers within idle timeout")),
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
        self.observer.response_status(status, &headers);
        if let Some(id) = resp.headers().get(ID_HEADER).and_then(|v| v.to_str().ok()) {
            self.sink.event(StreamEvent::Meta { upstream_id: Some(id.to_string()), provider: None, model: None });
        }
        let retry_after =
            resp.headers().get("retry-after").and_then(|v| v.to_str().ok()).and_then(|s| s.trim().parse::<u64>().ok());
        let text = tokio::select! {
            _ = self.cancel.cancelled() => return Err(LlmError::cancelled()),
            t = tokio::time::timeout(timeout, resp.text()) => match t {
                Err(_) => return Err(LlmError::new(ErrorKind::Timeout, Retry::Backoff, "no response body within idle timeout")),
                Ok(Err(e)) => return Err(LlmError::network(format!("response read error: {e}"))),
                Ok(Ok(t)) => t,
            }
        };
        self.observer.body_text(&text);
        if !(200..300).contains(&status) {
            return Err(crate::chatstream::http_error(status, retry_after, &text, error_from_json));
        }
        let v: Value = serde_json::from_str(&text).map_err(|e| {
            LlmError::new(ErrorKind::Decode, Retry::Backoff, format!("bad JSON response: {e}"))
                .with_details(Value::String(text.clone()))
        })?;
        if let Some(e) = v.get("error").filter(|e| e.is_object()) {
            let mut le = error_from_json(e, Some(status), retry_after, ErrorKind::InBand);
            le.details = Some(v.clone());
            return Err(le);
        }
        Ok(v)
    }

    /// Download a generated file from Zhipu's temporary link (no credentials
    /// sent): its bytes and `content-type`. `what` names it in errors.
    pub(super) async fn download(&mut self, url: &str, what: &str) -> Result<(Vec<u8>, Option<String>), LlmError> {
        self.observer.request(url, &Value::Null);
        let timeout = self.cfg.idle_timeout;
        let timed_out = || LlmError::new(ErrorKind::Timeout, Retry::Backoff, format!("{what} download timed out"));
        let resp = tokio::select! {
            _ = self.cancel.cancelled() => return Err(LlmError::cancelled()),
            r = tokio::time::timeout(timeout, self.http.get(url).send()) => match r {
                Err(_) => return Err(timed_out()),
                Ok(Err(e)) => return Err(LlmError::network(format!("{what} download failed: {e}"))),
                Ok(Ok(r)) => r,
            }
        };
        let status = resp.status().as_u16();
        self.observer.response_status(status, &[]);
        if !(200..300).contains(&status) {
            let mut e = LlmError::new(
                ErrorKind::Http,
                crate::error::retry_for_status(status, None),
                format!("downloading the generated {what} failed: HTTP {status}"),
            );
            e.status = Some(status);
            e.details = Some(json!({"url": url}));
            return Err(e);
        }
        let header = resp.headers().get("content-type").and_then(|v| v.to_str().ok()).map(str::to_string);
        let bytes = tokio::select! {
            _ = self.cancel.cancelled() => return Err(LlmError::cancelled()),
            b = tokio::time::timeout(timeout, resp.bytes()) => match b {
                Err(_) => return Err(timed_out()),
                Ok(Err(e)) => return Err(LlmError::network(format!("{what} download failed: {e}"))),
                Ok(Ok(b)) => b.to_vec(),
            }
        };
        self.observer.body_text(&format!("({} bytes of {})", bytes.len(), header.as_deref().unwrap_or("unknown type")));
        Ok((bytes, header))
    }

    /// Download a generated image (Zhipu's temporary link).
    async fn fetch(&mut self, url: &str) -> Result<(Vec<u8>, String), LlmError> {
        let (bytes, header) = self.download(url, "image").await?;
        let mime = image_mime(&bytes, header.as_deref());
        Ok((bytes, mime))
    }

    /// Show the images of a finished request and bill them.
    async fn deliver(&mut self, model: &str, urls: Vec<String>) -> Result<(), LlmError> {
        if urls.is_empty() {
            let mut e = LlmError::new(ErrorKind::InBand, Retry::Never, "Zhipu returned no image");
            e.error_type = Some("error".into());
            return Err(e);
        }
        let n = urls.len();
        for url in urls {
            let (data, mime) = self.fetch(&url).await?;
            self.sink.event(StreamEvent::Image { data, mime });
        }
        let cost = spec(model).and_then(|s| usd(s.price_cny * n as f64, Currency::Cny, self.cfg.cny_per_usd));
        self.sink.event(StreamEvent::Usage(Usage {
            cost_usd: cost,
            cost_source: if cost.is_some() { CostSource::Estimated } else { CostSource::Unknown },
            raw: json!({"images": n}),
            ..Default::default()
        }));
        self.sink.event(StreamEvent::Finish { reason: Some("stop".into()), native: None });
        Ok(())
    }

    /// Poll a task until it is done; returns its final result.
    async fn poll(&mut self, url: &str, model: &str) -> Result<Value, LlmError> {
        let mut seen = None::<String>;
        loop {
            let rb = self.http.get(url).bearer_auth(&self.cfg.api_key);
            let v = self.call(rb, url, &Value::Null).await?;
            let status = v.get("task_status").and_then(Value::as_str).unwrap_or("").to_string();
            if seen.as_deref() != Some(status.as_str()) {
                if let Some(m) = v.get("model").and_then(Value::as_str) {
                    self.sink.event(StreamEvent::Meta {
                        upstream_id: None,
                        provider: None,
                        model: Some(m.to_string()),
                    });
                }
                seen = Some(status.clone());
            }
            match status.as_str() {
                "SUCCESS" => {
                    let urls = urls(&v, "image_result");
                    self.deliver(model, urls).await?;
                    return Ok(v);
                }
                "FAIL" => return Err(task_failed(v, "image")),
                _ => {}
            }
            tokio::select! {
                _ = self.cancel.cancelled() => return Err(LlmError::cancelled()),
                _ = tokio::time::sleep(POLL_EVERY) => {}
            }
        }
    }
}

/// A task that ended `FAIL`. Zhipu documents no reason for it (the result
/// has `model`, `task_status`, `image_result`, `request_id`): a FAIL without
/// one is an upstream failure that a new task may get past, so it is retried
/// (live, 2026-11-13: a 2048x2048 task failed after about 250 s and the same
/// request then succeeded). Within the attempt the bot resumes from the
/// checkpointed id (polling the same task, which answers FAIL again at once);
/// the next attempt creates a new task. An error object or a content-filter
/// result in it is the reason: classified as such (a filter is never retried).
/// `what` names the task (`image`, `video`: [`super::videos`] shares this).
pub(super) fn task_failed(v: Value, what: &str) -> LlmError {
    if let Some(err) = v.get("error").filter(|e| e.is_object()) {
        let mut e = super::error_from_json(err, None, None, ErrorKind::InBand);
        e.details = Some(v);
        return e;
    }
    let filtered = v.get("content_filter").and_then(Value::as_array).is_some_and(|a| !a.is_empty());
    let (retry, message) = if filtered {
        (Retry::Never, format!("Zhipu's {what} task failed (task_status FAIL) with a content filter result"))
    } else {
        (
            Retry::Backoff,
            format!("Zhipu's {what} task failed without a reason (task_status FAIL); a new task may succeed"),
        )
    };
    let mut e = LlmError::new(ErrorKind::InBand, retry, message);
    e.error_type = Some("FAIL".into());
    e.details = Some(v);
    e
}

fn urls(v: &Value, key: &str) -> Vec<String> {
    v.get(key)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|d| d.get("url").and_then(Value::as_str))
        .filter(|u| !u.is_empty())
        .map(str::to_string)
        .collect()
}

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
    let result = async {
        if p.url.contains("/async-result/") {
            // Resuming a task: poll it.
            let id = p.url.rsplit('/').next().unwrap_or("").to_string();
            outcome.upstream_id = Some(id.clone());
            outcome.continuation = Some(json!({"task_id": id}));
            return run.poll(&p.url, &p.model).await;
        }
        let rb = http.post(&p.url).bearer_auth(&cfg.api_key).json(&p.body);
        let created = run.call(rb, &p.url, &p.body).await?;
        if p.url.ends_with("/async/images/generations") {
            let Some(id) = created.get("id").and_then(Value::as_str).filter(|s| !s.is_empty()) else {
                let mut e = LlmError::new(ErrorKind::InBand, Retry::Never, "Zhipu created no task (no `id`)");
                e.details = Some(created);
                return Err(e);
            };
            let item = json!({"task_id": id});
            outcome.upstream_id = Some(id.to_string());
            outcome.continuation = Some(item.clone());
            run.sink.event(StreamEvent::Meta { upstream_id: Some(id.to_string()), provider: None, model: None });
            // A restart resumes polling this task instead of paying for another.
            run.sink.event(StreamEvent::Checkpoint(item));
            let poll = continuation_request(p, &json!({"task_id": id}));
            let mut done = run.poll(&poll.url, &p.model).await?;
            done["task"] = created;
            return Ok(done);
        }
        let urls = urls(&created, "data");
        run.deliver(&p.model, urls).await?;
        Ok(created)
    }
    .await;
    (result, outcome)
}

#[cfg(test)]
mod tests;
