//! Zhipu's own API on its domestic BigModel platform
//! (`https://open.bigmodel.cn/api/paas/v4`, as in the old bot): Chat
//! Completions; see docs/research/glm_kimi_qwen.md §0–§1. Docs re-checked
//! 2026-11-12 (docs.bigmodel.cn `.md` pages, the chat completion OpenAPI).
//!
//! - Auth: `Authorization: Bearer <key>` (the old bot signed a JWT from the
//!   key; the current docs take the key itself).
//! - Thinking at the highest level: `thinking.type: "enabled"` plus
//!   `reasoning_effort: "max"` where the model has levels (GLM-5.3 family:
//!   max/high/low, always on; GLM-5.2: max/high, `e=none` turns it off). The
//!   other GLM-4.5+ models only have the switch.
//! - Preserved thinking: `thinking.clear_thinking: false`, so earlier turns'
//!   reasoning stays in the context (the platform default `true` drops it).
//!   Zhipu then requires the earlier `reasoning_content` unmodified and in
//!   order: it is replayed byte for byte for the same model.
//! - An explicit `max_tokens` (the platform default is 64K of the 128K).
//! - Web search (`+s`): the chat `web_search` tool with Zhipu's own advanced
//!   engine `search_pro` at full depth (the documented maximum of 50 results,
//!   `content_size: "high"`), the sources returned (`search_result: true`)
//!   ahead of the answer (`result_sequence: "before"`: the search is shown
//!   where it happened). Only the text models take it (the vision request
//!   schema has function tools only). The top-level `web_search[]` becomes a
//!   `web_search` tool call (every result verbatim; the API returns no query)
//!   and the answer's sources. Zhipu's search-intent detection decides
//!   whether to search: an answer without `web_search[]` says that none ran.
//! - Assistant messages carry text only: media other models generated in
//!   earlier turns are not sent (a warning says what).
//! - Media per model: JPEG/PNG images and MP4/MKV/MOV videos (as data URLs)
//!   on the vision models; files (PDF, Word, xlsx, pptx, as `file_data`) on
//!   the GLM-5.3-Flash family; text files are inlined (the `file` part does
//!   not parse them, measured 2026-11-13).
//! - Usage on every chunk (`stream_options.chunk_include_usage`). Finishes:
//!   `sensitive` is a content-filter stop, `network_error` a retryable
//!   failure, `model_context_window_exceeded` a truncation (like `length`).
//! - Cost from the CNY price list (tiers by input length), plus 0.03 CNY per
//!   search.
//!
//! The image models (GLM-Image, CogView) have their own API: [`images`]; so
//! have the video models (CogVideoX, Vidu): [`videos`].

pub mod images;
pub mod videos;

use omni_core::Usage;
use omni_core::content::{Citation, Part, Role, ToolCall, Turn};
use omni_core::usage::CostSource;
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use crate::chatstream::{self, EventParser};
use crate::error::{ErrorKind, LlmError, Retry, retry_for_status};
use crate::event::StreamEvent;
use crate::openrouter::chat::ChunkParser;
use crate::openrouter::options::{Applied, overlay};
use crate::provider::{self, Currency, DirectConfig, Provider, TokenPrice};
use crate::request::{self, Requested, RouteCaps, Tool};
use crate::{BlobLoader, CallInput, CallOutcome, EventSink, MediaClass, Observer, Prepared, classify_mime};

pub const DIALECT: &str = "zhipu.chat";

const NAME: &str = "Zhipu BigModel";
/// Response header with the request's log id.
const ID_HEADER: &str = "x-log-id";
/// The search engine for `+s`: Zhipu's own advanced engine (高级版), "multi-engine
/// collaboration" with higher recall and accuracy than `search_std`; Sogou and
/// Quark are third-party engines.
pub const SEARCH_ENGINE: &str = "search_pro";
/// CNY per search with [`SEARCH_ENGINE`] (docs.bigmodel.cn pricing, 2026-11-13).
pub const SEARCH_CNY: f64 = 0.03;
/// Results per search: the documented maximum ("可填范围：1-50，最大单次搜索返回50条，
/// 默认为10"; docs.bigmodel.cn chat completion reference, 2026-11-13). The fee is per search.
pub const SEARCH_COUNT: u64 = 50;
/// The longest page extracts ("high：最大化上下文，信息量较大但内容详细"; the
/// default `medium` returns summaries).
pub const SEARCH_CONTENT_SIZE: &str = "high";
/// A `+s` answer without `web_search[]`: Zhipu's search-intent detection (on
/// by default) found no need to search.
const NO_SEARCH: &str = "no web search ran (Zhipu's search-intent detection decides)";
/// The id given to the search call (the API returns none).
const SEARCH_CALL_ID: &str = "web_search";
/// Effort levels accepted for models outside the table (the API's own set for
/// GLM-5.2 and later; the API judges).
const UNKNOWN_EFFORTS: &[&str] = &["max", "high", "low"];

/// How a model thinks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Thinking {
    /// Always on (GLM-5.3 family; `disabled` is an error), with effort levels.
    Always(&'static [&'static str]),
    /// On by default, `thinking.type: "disabled"` turns it off; effort levels
    /// where the model has them (GLM-5.2), otherwise the switch only.
    Switch(&'static [&'static str]),
}

impl Thinking {
    pub fn efforts(self) -> &'static [&'static str] {
        match self {
            Thinking::Always(l) | Thinking::Switch(l) => l,
        }
    }
    pub fn can_disable(self) -> bool {
        matches!(self, Thinking::Switch(_))
    }
}

/// One price tier: applies while the input (and output, where tiered) stay
/// below the bounds.
#[derive(Debug, Clone, Copy)]
pub struct Tier {
    pub input_below: Option<u64>,
    pub output_below: Option<u64>,
    /// CNY per 1M tokens.
    pub price: TokenPrice,
}

#[derive(Debug, Clone, Copy)]
pub struct ModelSpec {
    pub id: &'static str,
    pub context: u64,
    pub max_output: u64,
    pub image: bool,
    pub video: bool,
    /// The `file` part (PDF, Word, xlsx, pptx, txt, jsonl).
    pub file: bool,
    pub thinking: Thinking,
    /// The chat `web_search` tool (text models).
    pub search: bool,
    /// Price tiers, first match wins; empty: not on the CN price list.
    pub prices: &'static [Tier],
}

const fn price(input: f64, output: f64, hit: f64) -> TokenPrice {
    // Cache storage is free for now (限时免费); there are no cache writes.
    TokenPrice { input, cached_input: hit, cache_write: input, output }
}
const fn flat(input: f64, output: f64, hit: f64) -> Tier {
    Tier { input_below: None, output_below: None, price: price(input, output, hit) }
}
/// "输入长度 [0, 32K)": K is 1000 on the price page (its output tier is "0.2K").
const K32: u64 = 32_000;
const fn short(input: f64, output: f64, hit: f64) -> Tier {
    Tier { input_below: Some(K32), output_below: None, price: price(input, output, hit) }
}
/// Input below 32K and output below 0.2K.
const fn short_reply(input: f64, output: f64, hit: f64) -> Tier {
    Tier { input_below: Some(K32), output_below: Some(200), price: price(input, output, hit) }
}
const FREE: Tier = flat(0.0, 0.0, 0.0);

const E53: &[&str] = &["max", "high", "low"];
/// GLM-5.2 also accepts xhigh/medium/low/minimal, which it remaps silently
/// (to max/high/high/none): only the distinct levels are offered.
const E52: &[&str] = &["max", "high"];
const M1: u64 = 1_048_576;

const TEXT: ModelSpec = ModelSpec {
    id: "",
    context: 200_000,
    max_output: 131_072,
    image: false,
    video: false,
    file: false,
    thinking: Thinking::Switch(&[]),
    search: true,
    prices: &[],
};
const VISION: ModelSpec = ModelSpec { image: true, video: true, search: false, ..TEXT };
const FLASH: ModelSpec = ModelSpec {
    id: "glm-5.3-flash",
    context: M1,
    file: true,
    thinking: Thinking::Always(E53),
    prices: &[flat(0.8, 2.8, 0.23)],
    ..VISION
};

/// Limits from the model overview and `max_tokens` table, prices (CNY per 1M:
/// input, output, cache hit) from the CN price page, inputs, thinking and
/// tools from the chat completion OpenAPI: docs.bigmodel.cn, fetched 2026-11-12.
pub const MODELS: &[ModelSpec] = &[
    ModelSpec { id: "glm-5.3", context: M1, thinking: Thinking::Always(E53), prices: &[flat(8.0, 28.0, 2.0)], ..TEXT },
    FLASH,
    ModelSpec { id: "glm-5.3-flashx", prices: &[flat(2.0, 7.0, 0.57)], ..FLASH },
    ModelSpec { id: "glm-5.3-flashx-legal", prices: &[flat(2.0, 7.0, 0.57)], ..FLASH },
    ModelSpec { id: "glm-5.2", context: M1, thinking: Thinking::Switch(E52), prices: &[flat(8.0, 28.0, 2.0)], ..TEXT },
    ModelSpec { id: "glm-5.1", prices: &[short(6.0, 24.0, 1.3), flat(8.0, 28.0, 2.0)], ..TEXT },
    ModelSpec { id: "glm-5-turbo", prices: &[short(5.0, 22.0, 1.2), flat(7.0, 26.0, 1.8)], ..TEXT },
    ModelSpec { id: "glm-5", prices: &[short(4.0, 18.0, 1.0), flat(6.0, 22.0, 1.5)], ..TEXT },
    ModelSpec { id: "glm-5v-turbo", prices: &[short(5.0, 22.0, 1.2), flat(7.0, 26.0, 1.8)], ..VISION },
    ModelSpec {
        id: "glm-4.7",
        prices: &[short_reply(2.0, 8.0, 0.4), short(3.0, 14.0, 0.6), flat(4.0, 16.0, 0.8)],
        ..TEXT
    },
    ModelSpec { id: "glm-4.7-flashx", prices: &[flat(0.5, 3.0, 0.1)], ..TEXT },
    ModelSpec { id: "glm-4.7-flash", prices: &[FREE], ..TEXT },
    ModelSpec {
        id: "glm-4.6v",
        context: 131_072,
        max_output: 32_768,
        prices: &[short(1.0, 3.0, 0.2), flat(2.0, 6.0, 0.4)],
        ..VISION
    },
    ModelSpec {
        id: "glm-4.6v-flashx",
        context: 131_072,
        max_output: 32_768,
        prices: &[short(0.15, 1.5, 0.03), flat(0.3, 3.0, 0.03)],
        ..VISION
    },
    ModelSpec { id: "glm-4.6v-flash", context: 131_072, max_output: 32_768, prices: &[FREE], ..VISION },
    ModelSpec {
        id: "glm-4.5-air",
        context: 131_072,
        max_output: 98_304,
        prices: &[short_reply(0.8, 2.0, 0.16), short(0.8, 6.0, 0.16), flat(1.2, 8.0, 0.24)],
        ..TEXT
    },
];

pub fn model_spec(model: &str) -> Option<&'static ModelSpec> {
    MODELS.iter().find(|m| m.id == model)
}

/// What Zhipu's API offers for a model (the vendor's own API: official = route).
/// Models outside the table get the general set; the API judges.
struct Caps(Option<&'static ModelSpec>);

impl RouteCaps for Caps {
    fn name(&self) -> &str {
        NAME
    }
    fn tool(&self, tool: Tool) -> bool {
        tool == Tool::Search && self.0.is_none_or(|s| s.search)
    }
    fn efforts(&self) -> Option<Vec<String>> {
        let levels = self.0.map_or(UNKNOWN_EFFORTS, |s| s.thinking.efforts());
        (!levels.is_empty()).then(|| levels.iter().map(|e| e.to_string()).collect())
    }
    fn can_disable_reasoning(&self) -> bool {
        self.0.is_none_or(|s| s.thinking.can_disable())
    }
    fn budget(&self) -> bool {
        false
    }
    fn verbosity(&self) -> bool {
        false
    }
    fn image_options(&self) -> bool {
        false
    }
    fn tiers(&self) -> Vec<String> {
        Vec::new()
    }
}

pub struct Zhipu {
    cfg: DirectConfig,
    http: reqwest::Client,
}

impl Zhipu {
    pub fn new(cfg: DirectConfig) -> anyhow::Result<Self> {
        let http = cfg.http()?;
        Ok(Self { cfg, http })
    }
}

#[async_trait::async_trait]
impl Provider for Zhipu {
    fn id(&self) -> &'static str {
        "zhipu"
    }
    fn name(&self) -> &'static str {
        NAME
    }
    fn knows(&self, model: &str) -> bool {
        model_spec(model).is_some() || images::spec(model).is_some() || videos::spec(model).is_some()
    }

    async fn prepare(
        &self,
        input: &CallInput<'_>,
        presets: &[Requested],
        blobs: &dyn BlobLoader,
    ) -> Result<Prepared, LlmError> {
        if !self.knows(input.model) {
            return Err(crate::provider::not_in_table(self.name(), input.model));
        }
        if images::is_image_model(input.model) {
            return images::prepare(&self.cfg, input, presets).await;
        }
        if videos::is_video_model(input.model) {
            return videos::prepare(&self.cfg, input, presets, blobs).await;
        }
        let user = request::parse(input.options).map_err(LlmError::request)?;
        let mut req = Requested::default();
        for p in presets {
            req = overlay(&req, p);
        }
        let req = overlay(&req, &user);
        if req.providers.is_some() {
            return Err(LlmError::request("`p=` (provider choice) does not apply to Zhipu BigModel"));
        }
        let model = input.model;
        let spec = model_spec(model);
        let search = req.tools.contains(&Tool::Search);
        if search && spec.is_some_and(|s| !s.search) {
            return Err(LlmError::request(format!(
                "`{model}` has no web search (`+s`) via Zhipu BigModel: the chat `web_search` tool is documented for \
                 the text models only (glm-5.3, glm-5.2, …), not for the vision models"
            )));
        }
        request::validate(model, &req, &Caps(spec)).map_err(LlmError::request)?;

        let (messages, notes) = chat_messages(input.history, model, spec, blobs).await?;
        let mut body = json!({
            "model": model,
            "messages": messages,
            "stream": true,
            // Cumulative usage on every chunk: tokens and cost update while streaming.
            "stream_options": {"chunk_include_usage": true},
        });
        let mut applied = Vec::new();
        let mut settings = Vec::new();
        let source = |by_user: bool| if by_user { "option" } else { "default" };

        // Thinking: the highest level by default; earlier reasoning preserved.
        let mut reasoning = Map::new();
        let described = match (spec, req.effort.as_deref()) {
            (Some(_), Some("none")) => {
                reasoning.insert("thinking".into(), json!({"type": "disabled", "clear_thinking": false}));
                "thinking off".to_string()
            }
            (Some(s), e) => {
                reasoning.insert("thinking".into(), json!({"type": "enabled", "clear_thinking": false}));
                match e.or(s.thinking.efforts().first().copied()) {
                    Some(e) => {
                        reasoning.insert("reasoning_effort".into(), json!(e));
                        format!("effort {e}")
                    }
                    None => "thinking on".to_string(),
                }
            }
            (None, Some("none")) => {
                reasoning.insert("thinking".into(), json!({"type": "disabled"}));
                "thinking off".to_string()
            }
            (None, Some(e)) => {
                reasoning.insert("thinking".into(), json!({"type": "enabled"}));
                reasoning.insert("reasoning_effort".into(), json!(e));
                format!("effort {e}")
            }
            (None, None) => "reasoning: Zhipu's default".to_string(),
        };
        settings.push(described);
        if !reasoning.is_empty() {
            for (k, v) in &reasoning {
                body[k] = v.clone();
            }
            applied.push(Applied { source: source(user.effort.is_some()).into(), patch: Value::Object(reasoning) });
        }

        // Output budget: explicit (the default, 64K, would cut 128K models short).
        let max_tokens = input.max_tokens_override.or_else(|| {
            let s = spec?;
            let prompt = crate::budget::estimate_prompt(input.history, input.known_context);
            crate::budget::choose(Some(s.context), Some(s.max_output), self.cfg.max_tokens_cap, prompt)
        });
        if let Some(n) = max_tokens {
            body["max_tokens"] = json!(n);
            applied.push(Applied { source: "budget".into(), patch: json!({"max_tokens": n}) });
        }
        if search {
            let tools = json!([{"type": "web_search", "web_search": {
                "enable": true,
                "search_engine": SEARCH_ENGINE,
                "count": SEARCH_COUNT,
                "content_size": SEARCH_CONTENT_SIZE,
                "search_result": true,
                "result_sequence": "before",
            }}]);
            body["tools"] = tools.clone();
            settings.push(format!(
                "{} ({SEARCH_ENGINE}, count {SEARCH_COUNT}, content_size {SEARCH_CONTENT_SIZE})",
                Tool::Search.describe()
            ));
            applied.push(Applied {
                source: source(user.tools.contains(&Tool::Search)).into(),
                patch: json!({"tools": tools}),
            });
        }
        // The end user (abuse handling per end user), 6–128 characters.
        if let Some(u) = input.user.as_deref().filter(|u| (6..=128).contains(&u.chars().count())) {
            body["user_id"] = json!(u);
        }

        let mut warnings = Vec::new();
        // This model's own answers are replayed (text and reasoning); other turns go as their text.
        let own = |t: &Turn| t.native.as_ref().is_some_and(|n| n.dialect == DIALECT && n.model == model);
        crate::provider::unsent_media(input.history, own)?;
        match spec {
            None => warnings.push(format!(
                "`{model}` is not in the bot's Zhipu model table: limits, price and reasoning options unknown \
                 (max_tokens{} not set: Zhipu's defaults apply; cost not estimated)",
                if req.effort.is_none() { " and thinking" } else { "" }
            )),
            Some(s) if s.prices.is_empty() => {
                warnings.push(format!("`{model}` is not on Zhipu's CN price list: cost not estimated"))
            }
            Some(_) => {}
        }
        Ok(Prepared {
            dialect: DIALECT,
            model: model.to_string(),
            url: self.cfg.url("/chat/completions"),
            body,
            applied,
            warnings,
            notes,
            show_debug: false,
            auto_max_tokens: max_tokens,
            long_decode: false,
            settings,
        })
    }

    async fn stream(
        &self,
        p: &Prepared,
        sink: &mut dyn EventSink,
        observer: &dyn Observer,
        cancel: &CancellationToken,
    ) -> (Result<Value, LlmError>, CallOutcome) {
        if p.dialect == images::DIALECT {
            return images::stream(&self.http, &self.cfg, p, sink, observer, cancel).await;
        }
        if p.dialect == videos::DIALECT {
            return videos::stream(&self.http, &self.cfg, p, sink, observer, cancel).await;
        }
        let mut priced = PricedSink { inner: sink, model: &p.model, cny_per_usd: self.cfg.cny_per_usd, searches: 0 };
        let rb = self.http.post(&p.url).bearer_auth(&self.cfg.api_key).json(&p.body);
        let s = chatstream::run_with(
            rb,
            &p.url,
            &p.body,
            self.cfg.idle_timeout,
            Some(ID_HEADER),
            error_from_json,
            GlmParser::new(),
            &mut priced,
            observer,
            cancel,
        )
        .await;
        let outcome = CallOutcome { upstream_id: s.header_id, ..Default::default() };
        let no_search = p.body.get("tools").is_some() && s.parser.answered_without_search();
        match (s.result, s.parser.abnormal) {
            (Err(e), _) | (Ok(()), Some(e)) => (Err(e), outcome),
            (Ok(()), None) => {
                if no_search {
                    priced.event(StreamEvent::Notice(NO_SEARCH.into()));
                }
                let native = native_payload(&s.parser.inner, s.parser.web_search.as_ref());
                (Ok(native), outcome)
            }
        }
    }
}

// ------------------------------------------------------------------ history

/// The assistant message replayed to the same model: the answer and its
/// reasoning exactly as streamed.
fn replay(p: &Value) -> Value {
    let mut m = json!({"role": "assistant", "content": p.get("content").cloned().unwrap_or(json!(""))});
    if let Some(r) = p.get("reasoning_content").and_then(Value::as_str).filter(|s| !s.is_empty()) {
        m["reasoning_content"] = json!(r);
    }
    m
}

/// The native payload: the message as replayed, plus the search results (kept,
/// not replayed: they are not part of the assistant message).
fn native_payload(p: &ChunkParser, web_search: Option<&Value>) -> Value {
    let n = p.native_payload();
    let mut m = json!({"role": "assistant", "content": n.get("content").cloned().unwrap_or(json!(""))});
    if let Some(r) = n.get("reasoning").and_then(Value::as_str).filter(|s| !s.is_empty()) {
        m["reasoning_content"] = json!(r);
    }
    if let Some(ws) = web_search {
        m["web_search"] = ws.clone();
    }
    m
}

fn file_name(filename: &Option<String>) -> &str {
    filename.as_deref().unwrap_or("file")
}

fn extension(name: &str) -> String {
    name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default()
}

/// What a model takes, for error messages.
fn takes(spec: &ModelSpec) -> &'static str {
    match (spec.image, spec.file) {
        (false, _) => "text only",
        (true, false) => "text, images and videos",
        (true, true) => "text, images, videos and files",
    }
}

fn no_input(model: &str, what: &str, name: &str, spec: Option<&ModelSpec>) -> LlmError {
    let takes = spec.map(|s| format!(": it takes {}", takes(s))).unwrap_or_default();
    LlmError::request(format!("`{model}` does not take {what} input via Zhipu BigModel ({name}){takes}"))
}

/// Word, Excel and PowerPoint files the `file` part takes (doc/docx, xlsx, pptx).
fn native_document(mime: &str, name: &str) -> bool {
    matches!(extension(name).as_str(), "doc" | "docx" | "xlsx" | "pptx")
        || mime == "application/msword"
        || ["wordprocessingml", "spreadsheetml", "presentationml"].iter().any(|k| mime.contains(k))
}

async fn data_url(blobs: &dyn BlobLoader, blob: &omni_core::BlobRef) -> Result<String, LlmError> {
    Ok(provider::data_url(&blob.mime, &provider::load(blobs, blob).await?))
}

/// A text file inlined as a separate text part.
async fn inline_text(blobs: &dyn BlobLoader, blob: &omni_core::BlobRef, name: &str) -> Result<Value, LlmError> {
    let text = String::from_utf8(provider::load(blobs, blob).await?)
        .map_err(|_| LlmError::request(format!("{name} is not valid UTF-8 text")))?;
    Ok(json!({"type": "text", "text": text}))
}

/// A `file` part: the file inline (`data:<MIME>;base64,…`), parsed by Zhipu.
fn file_part(data_url: String, name: &str) -> Value {
    json!({"type": "file", "file": {"file_data": data_url, "filename": name}})
}

/// The conversation as Chat Completions `messages`, with conversion notes.
async fn chat_messages(
    history: &[Turn],
    model: &str,
    spec: Option<&'static ModelSpec>,
    blobs: &dyn BlobLoader,
) -> Result<(Vec<Value>, Vec<String>), LlmError> {
    // Unknown ids: media go natively and the API judges (text files are inlined).
    let (image, video, file) = spec.map_or((true, true, false), |s| (s.image, s.video, s.file));
    let notes = Vec::new();
    let mut out = Vec::with_capacity(history.len());
    for turn in history {
        match turn.role {
            Role::System => out.push(json!({"role": "system", "content": turn.answer_text()})),
            Role::Assistant => match &turn.native {
                Some(n) if n.dialect == DIALECT && n.model == model => out.push(replay(&n.payload)),
                _ => out.push(json!({"role": "assistant", "content": turn.answer_text()})),
            },
            Role::User => {
                let mut content: Vec<Value> = Vec::new();
                for part in &turn.parts {
                    let (blob, name, class) = match part {
                        Part::Text { text, .. } => {
                            if !text.is_empty() {
                                content.push(json!({"type": "text", "text": text}));
                            }
                            continue;
                        }
                        Part::Image { blob, filename, .. } => (blob, file_name(filename), MediaClass::Image),
                        Part::File { blob, filename, .. } => {
                            (blob, file_name(filename), classify_mime(&blob.mime, filename.as_deref()))
                        }
                        Part::RemoteMedia { url, .. } => {
                            return Err(LlmError::request(format!(
                                "`{model}` cannot read remote media via Zhipu BigModel ({url})"
                            )));
                        }
                        Part::Location { .. } => return Err(provider::no_location(model, NAME)),
                        Part::Reasoning(_) | Part::ToolCall(_) | Part::Refusal { .. } => continue,
                    };
                    let mime = blob.mime.as_str();
                    match class {
                        MediaClass::Image => {
                            if !image {
                                return Err(no_input(model, "image", name, spec));
                            }
                            if !matches!(mime, "image/jpeg" | "image/jpg" | "image/png") {
                                return Err(LlmError::request(format!(
                                    "Zhipu BigModel takes JPEG and PNG images, not {mime} ({name})"
                                )));
                            }
                            let url = data_url(blobs, blob).await?;
                            content.push(json!({"type": "image_url", "image_url": {"url": url}}));
                        }
                        MediaClass::Video => {
                            if !video {
                                return Err(no_input(model, "video", name, spec));
                            }
                            let ok = matches!(mime, "video/mp4" | "video/x-matroska" | "video/quicktime")
                                || matches!(extension(name).as_str(), "mp4" | "mkv" | "mov");
                            if !ok {
                                return Err(LlmError::request(format!(
                                    "Zhipu BigModel takes MP4, MKV and MOV videos, not {mime} ({name})"
                                )));
                            }
                            let url = data_url(blobs, blob).await?;
                            content.push(json!({"type": "video_url", "video_url": {"url": url}}));
                        }
                        MediaClass::Pdf | MediaClass::Document => {
                            if class == MediaClass::Document && !native_document(mime, name) {
                                return Err(LlmError::request(format!(
                                    "Zhipu BigModel's file input takes PDF, Word, xlsx, pptx, txt and jsonl, \
                                     not {mime} ({name})"
                                )));
                            }
                            if !file {
                                let what = if class == MediaClass::Pdf { "PDF" } else { mime };
                                return Err(no_input(model, what, name, spec));
                            }
                            content.push(file_part(data_url(blobs, blob).await?, name));
                        }
                        // Inlined verbatim as a separate part: the `file` part does not
                        // parse text files although the docs list txt and jsonl (measured
                        // 2026-11-13: "文件解析失败" for a 45-byte txt with every data-URL
                        // MIME; PDFs parse).
                        MediaClass::Text => content.push(inline_text(blobs, blob, name).await?),
                        MediaClass::Audio => return Err(no_input(model, "audio", name, spec)),
                        MediaClass::Other => {
                            return Err(LlmError::request(format!("unsupported file type {mime} ({name})")));
                        }
                    }
                }
                // A single text part is sent as a plain string (the text models' documented form).
                if content.len() == 1 && content[0]["type"] == "text" {
                    out.push(json!({"role": "user", "content": content[0]["text"].clone()}));
                } else {
                    out.push(json!({"role": "user", "content": content}));
                }
            }
        }
    }
    Ok((out, notes))
}

// ------------------------------------------------------------------ stream

/// Chat Completions chunks with Zhipu's extras: `web_search[]`, `content_filter[]`
/// and its finish reasons.
struct GlmParser {
    inner: ChunkParser,
    /// The search results (top-level `web_search[]`), as received.
    web_search: Option<Value>,
    /// Sources for the answer, emitted with the finish (after the text they belong to).
    citations: Vec<Citation>,
    /// `content_filter[]` entries seen in the stream.
    content_filter: Vec<Value>,
    /// The inference failed (`network_error`): the call fails (retryably) once
    /// the stream has ended.
    abnormal: Option<LlmError>,
    /// The normalized finish reason.
    finish: Option<String>,
}

impl GlmParser {
    fn new() -> Self {
        let mut inner = ChunkParser::default();
        inner.map_error = Some(error_from_json);
        Self {
            inner,
            web_search: None,
            citations: Vec::new(),
            content_filter: Vec::new(),
            abnormal: None,
            finish: None,
        }
    }

    /// The answer ended normally (or at the output limit) without `web_search[]`.
    fn answered_without_search(&self) -> bool {
        matches!(self.finish.as_deref(), Some("stop" | "length")) && self.web_search.is_none()
    }
}

/// `{k: v}` for the keys of `r` that are present and not null.
fn pick(r: &Value, keys: &[&str]) -> Value {
    let mut o = Map::new();
    for k in keys {
        if let Some(v) = r.get(*k).filter(|v| !v.is_null()) {
            o.insert(k.to_string(), v.clone());
        }
    }
    Value::Object(o)
}

/// The search as a tool call: every result verbatim (`refer`, `title`, `link`,
/// `media`, `icon`, `publish_date`, `content`); Zhipu returns no query.
fn search_call(results: &Value) -> ToolCall {
    ToolCall {
        id: Some(SEARCH_CALL_ID.into()),
        name: "web_search".into(),
        status: Some("completed".into()),
        input: None,
        output: Some(results.clone()),
    }
}

fn search_citations(results: &Value) -> Vec<Citation> {
    results
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|r| {
            let url = r.get("link").and_then(Value::as_str).filter(|u| !u.is_empty())?;
            let extra = pick(r, &["refer", "media", "publish_date"]);
            Some(Citation {
                url: Some(url.to_string()),
                title: r.get("title").and_then(Value::as_str).map(str::to_string),
                cited_text: r.get("content").and_then(Value::as_str).map(str::to_string),
                range: None,
                extra: extra.as_object().is_some_and(|o| !o.is_empty()).then_some(extra),
            })
        })
        .collect()
}

/// The finish choice without its content, with the content-filter entries.
fn finish_details(chunk: &Value, content_filter: &[Value]) -> Value {
    let mut c = chunk.pointer("/choices/0").cloned().unwrap_or(json!({}));
    if let Some(o) = c.as_object_mut() {
        o.remove("delta");
        o.remove("message");
        if !content_filter.is_empty() {
            o.insert("content_filter".into(), Value::Array(content_filter.to_vec()));
        }
    }
    c
}

impl EventParser for GlmParser {
    fn event(&mut self, _name: Option<&str>, data: &Value) -> Result<Vec<StreamEvent>, LlmError> {
        let mut out = Vec::new();
        if let Some(ws) = data.get("web_search").filter(|w| w.is_array())
            && self.web_search.as_ref() != Some(ws)
        {
            self.web_search = Some(ws.clone());
            self.citations = search_citations(ws);
            out.push(StreamEvent::Tool(search_call(ws)));
            // Results after the finish: their sources go to the answer right away.
            if self.inner.finished && !self.citations.is_empty() {
                out.push(StreamEvent::Citations(std::mem::take(&mut self.citations)));
            }
        }
        if let Some(cf) = data.get("content_filter").and_then(Value::as_array) {
            self.content_filter.extend(cf.iter().cloned());
        }
        // Built-in tool calls are not expected in deltas (no function tools are
        // sent); should one appear, it is shown rather than dropped.
        for c in data.pointer("/choices/0/delta/tool_calls").and_then(Value::as_array).into_iter().flatten() {
            let ty = c.get("type").and_then(Value::as_str).unwrap_or("tool");
            let name = if ty == "function" { c.pointer("/function/name").and_then(Value::as_str) } else { None };
            let mut input = c.clone();
            if let Some(o) = input.as_object_mut() {
                o.remove("index");
            }
            out.push(StreamEvent::Tool(ToolCall {
                id: c.get("id").and_then(Value::as_str).map(str::to_string),
                name: name.unwrap_or(ty).to_string(),
                status: None,
                input: Some(input),
                output: None,
            }));
        }
        for ev in self.inner.chunk(data)? {
            let StreamEvent::Finish { reason: Some(r), .. } = ev else {
                out.push(ev);
                continue;
            };
            if !self.citations.is_empty() {
                out.push(StreamEvent::Citations(std::mem::take(&mut self.citations)));
            }
            let reason = match r.as_str() {
                // Blocked by content moderation (the text shown so far should be withdrawn).
                "sensitive" => {
                    out.push(StreamEvent::FinishDetails(finish_details(data, &self.content_filter)));
                    "content_filter"
                }
                "network_error" => {
                    let mut e = LlmError::new(
                        ErrorKind::InBand,
                        Retry::Backoff,
                        "Zhipu stopped the answer: model inference failed (finish_reason: network_error)",
                    );
                    e.error_type = Some(r.clone());
                    e.details = Some(data.clone());
                    e.partial_output = self.inner.any_output();
                    self.abnormal = Some(e);
                    out.push(StreamEvent::FinishDetails(finish_details(data, &self.content_filter)));
                    "error"
                }
                // The context window filled up: the answer is cut short, as with `length`.
                "model_context_window_exceeded" => "length",
                other => other,
            };
            self.finish = Some(reason.to_string());
            out.push(StreamEvent::Finish { reason: Some(reason.to_string()), native: Some(r) });
        }
        Ok(out)
    }
    fn finished(&self) -> bool {
        self.inner.finished
    }
    fn any_output(&self) -> bool {
        self.inner.any_output()
    }
}

// ------------------------------------------------------------------ errors

/// Business codes that retrying cannot fix (docs.bigmodel.cn/cn/api/api-code):
/// auth, arrears, invalid requests and unknown models, content safety, usage
/// windows and plan limits.
const FINAL_CODES: &[&str] = &[
    "1000", "1001", "1003", "1005", "1113", "1210", "1211", "1212", "1213", "1214", "1215", "1220", "1221", "1222",
    "1261", "1301", "1308", "1309", "1310", "1311", "1313", "1314", "1315", "1316", "1317", "1318", "1319", "1320",
    "1321",
];

/// Classify Zhipu's errors: `{"error":{"code":"1214","message":"…"}}` with a
/// string business code (docs/research/glm_kimi_qwen.md §1.7).
pub fn error_from_json(err: &Value, http_status: Option<u16>, retry_after: Option<u64>, kind: ErrorKind) -> LlmError {
    let code = match err.get("code") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    };
    let message = err.get("message").and_then(Value::as_str).unwrap_or("(no message)").to_string();
    let retry = match code.as_str() {
        // Rate limit (per-model concurrency) and overload: retry, the latter after longer.
        "1302" => Retry::After(retry_after.unwrap_or(20)),
        "1305" => Retry::After(retry_after.unwrap_or(30)),
        // Server-side failures ("try again later").
        "1200" | "1230" | "1234" => retry_after.map_or(Retry::Backoff, Retry::After),
        c if FINAL_CODES.contains(&c) => Retry::Never,
        _ => match http_status {
            Some(s) => retry_for_status(s, retry_after),
            None => Retry::Never,
        },
    };
    let error_type = match code.as_str() {
        // "Prompt 超长"
        "1261" => Some("context_length_exceeded".to_string()),
        "" => None,
        c => Some(c.to_string()),
    };
    LlmError {
        kind,
        retry,
        status: http_status,
        error_type,
        message,
        details: Some(err.clone()),
        partial_output: false,
    }
}

// ------------------------------------------------------------------ cost

/// Estimated cost in USD: tokens at the tier's CNY price (by input length, and
/// output length where tiered) plus searches.
pub fn estimate_cost(model: &str, u: &Usage, searches: u64, cny_per_usd: f64) -> Option<f64> {
    let s = model_spec(model)?;
    let (input, output) = (u.input_tokens?, u.output_tokens?);
    let tier = s
        .prices
        .iter()
        .find(|t| t.input_below.is_none_or(|b| input < b) && t.output_below.is_none_or(|b| output < b))?;
    let tokens = tier.price.cost(u)?;
    provider::usd(tokens + searches as f64 * SEARCH_CNY, Currency::Cny, cny_per_usd)
}

/// Prices usage events, counting the search (the usage does not report it).
struct PricedSink<'a> {
    inner: &'a mut dyn EventSink,
    model: &'a str,
    cny_per_usd: f64,
    searches: u64,
}

impl EventSink for PricedSink<'_> {
    fn event(&mut self, ev: StreamEvent) {
        match ev {
            StreamEvent::Tool(t) if t.id.as_deref() == Some(SEARCH_CALL_ID) => {
                self.searches = 1;
                self.inner.event(StreamEvent::Tool(t));
            }
            StreamEvent::Usage(mut u) if u.cost_usd.is_none() => {
                u.cost_usd = estimate_cost(self.model, &u, self.searches, self.cny_per_usd);
                u.cost_source = if u.cost_usd.is_some() { CostSource::Estimated } else { CostSource::Unknown };
                self.inner.event(StreamEvent::Usage(u));
            }
            other => self.inner.event(other),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use omni_core::content::{NativeOutput, Origin};
    use omni_core::trigger::OptionToken;
    use omni_core::{BlobRef, Turn};
    use std::time::Duration;

    struct Blobs(Vec<(BlobRef, Vec<u8>)>);
    impl BlobLoader for Blobs {
        fn load<'a>(&'a self, r: &'a BlobRef) -> crate::BoxFuture<'a, anyhow::Result<Vec<u8>>> {
            let found = self.0.iter().find(|(b, _)| b == r).map(|(_, d)| d.clone());
            Box::pin(async move { found.ok_or_else(|| anyhow::anyhow!("no blob")) })
        }
    }

    fn zp() -> Zhipu {
        zp_at("https://open.bigmodel.cn/api/paas/v4")
    }

    fn zp_at(base_url: &str) -> Zhipu {
        Zhipu::new(DirectConfig {
            api_key: "k".into(),
            base_url: base_url.into(),
            connect_timeout: Duration::from_secs(1),
            idle_timeout: Duration::from_secs(1),
            max_tokens_cap: Some(393_216),
            cny_per_usd: 7.0,
            long_decode_budget: 393_216,
        })
        .unwrap()
    }

    fn input<'a>(model: &'a str, options: &'a [OptionToken], history: &'a [Turn]) -> CallInput<'a> {
        CallInput {
            model,
            options,
            history,
            session_id: Some("conv".into()),
            user: Some("u1234567".into()),
            known_context: None,
            max_tokens_override: None,
        }
    }

    fn flag(f: &str) -> OptionToken {
        OptionToken::Flag(f.into())
    }

    fn kv(k: &str, v: &str) -> OptionToken {
        OptionToken::KeyValue(k.into(), v.into())
    }

    async fn prepare(model: &str, options: &[OptionToken], history: &[Turn]) -> Result<Prepared, LlmError> {
        zp().prepare(&input(model, options, history), &[], &Blobs(vec![])).await
    }

    #[tokio::test]
    async fn defaults_are_the_maximum() {
        let h = vec![Turn::user_text("hi")];
        let p = prepare("glm-5.3", &[], &h).await.unwrap();
        assert_eq!(p.dialect, DIALECT);
        assert_eq!(p.url, "https://open.bigmodel.cn/api/paas/v4/chat/completions");
        assert_eq!(p.body["thinking"], json!({"type": "enabled", "clear_thinking": false}));
        assert_eq!(p.body["reasoning_effort"], "max");
        assert_eq!(p.body["max_tokens"], 131_072);
        assert_eq!(p.body["stream_options"], json!({"chunk_include_usage": true}));
        assert_eq!(p.body["user_id"], "u1234567");
        assert_eq!(p.body["messages"], json!([{"role": "user", "content": "hi"}]));
        for k in ["temperature", "top_p", "do_sample", "tools", "request_id", "tool_choice"] {
            assert!(p.body.get(k).is_none(), "{k} must not be sent");
        }
        assert_eq!(p.settings, vec!["effort max"]);
        assert_eq!(p.applied[0].source, "default");
        assert_eq!(p.applied[1].patch, json!({"max_tokens": 131_072}));
        assert!(p.warnings.is_empty(), "{:?}", p.warnings);
        // Models with only the switch: thinking on, nothing else.
        let p = prepare("glm-5.1", &[], &h).await.unwrap();
        assert_eq!(p.body["thinking"], json!({"type": "enabled", "clear_thinking": false}));
        assert!(p.body.get("reasoning_effort").is_none());
        assert_eq!(p.settings, vec!["thinking on"]);
        // Smaller limits per model.
        let p = prepare("glm-4.6v", &[], &h).await.unwrap();
        assert_eq!(p.body["max_tokens"], 32_768);
    }

    #[tokio::test]
    async fn effort_levels_per_model() {
        let h = vec![Turn::user_text("hi")];
        let p = prepare("glm-5.3-flash", &[kv("e", "low")], &h).await.unwrap();
        assert_eq!((p.body["reasoning_effort"].as_str(), p.applied[0].source.as_str()), (Some("low"), "option"));
        let e = prepare("glm-5.3", &[kv("e", "none")], &h).await.unwrap_err();
        assert!(e.message.contains("cannot turn reasoning off"), "{}", e.message);
        let e = prepare("glm-5.3", &[kv("e", "xhigh")], &h).await.unwrap_err();
        assert!(e.message.contains("levels: max, high, low"), "{}", e.message);
        // GLM-5.2: max/high, or off.
        let p = prepare("glm-5.2", &[kv("e", "high")], &h).await.unwrap();
        assert_eq!(p.body["reasoning_effort"], "high");
        let e = prepare("glm-5.2", &[kv("e", "low")], &h).await.unwrap_err();
        assert!(e.message.contains("levels: max, high"), "{}", e.message);
        let p = prepare("glm-5.2", &[kv("e", "none")], &h).await.unwrap();
        assert_eq!(p.body["thinking"], json!({"type": "disabled", "clear_thinking": false}));
        assert!(p.body.get("reasoning_effort").is_none());
        assert_eq!(p.settings, vec!["thinking off"]);
        // Models without levels.
        let e = prepare("glm-4.7", &[kv("e", "high")], &h).await.unwrap_err();
        assert!(e.message.contains("has no reasoning effort levels"), "{}", e.message);
        assert!(prepare("glm-4.7", &[kv("e", "none")], &h).await.is_ok());
    }

    #[tokio::test]
    async fn options_and_errors() {
        let h = vec![Turn::user_text("hi")];
        let p = prepare("glm-5.3", &[flag("s")], &h).await.unwrap();
        assert_eq!(
            p.body["tools"],
            json!([{"type": "web_search", "web_search": {"enable": true, "search_engine": "search_pro", "count": 50,
                    "content_size": "high", "search_result": true, "result_sequence": "before"}}])
        );
        assert_eq!(p.settings, vec!["effort max", "web search (search_pro, count 50, content_size high)"]);
        assert_eq!(p.applied.last().unwrap().source, "option");
        let cases: &[(&str, &[OptionToken], &str)] = &[
            ("glm-5.3-flash", &[flag("s")], "documented for the text models only"),
            ("glm-5.3", &[flag("c")], "has no code execution"),
            ("glm-5.3", &[flag("u")], "has no URL fetching"),
            ("glm-5.3", &[kv("p", "z-ai")], "does not apply to Zhipu BigModel"),
            ("glm-5.3", &[kv("tier", "flex")], "no service tiers"),
            ("glm-5.3", &[kv("budget", "100")], "no reasoning token budget"),
            ("glm-5.3", &[flag("W")], "no image size"),
        ];
        for (model, opts, want) in cases {
            let e = prepare(model, opts, &h).await.unwrap_err();
            assert!(e.message.contains(want), "{opts:?}: {}", e.message);
        }
    }

    #[tokio::test]
    async fn models_outside_the_table_are_not_supported() {
        // Unknown ids, and those without a price on Zhipu's CN price list (glm-4.6, glm-4.5-airx):
        // the cost cannot be known, so they are not supported, whatever the options.
        let h = vec![Turn::user_text("hi")];
        for id in ["glm-6", "glm-4.6", "glm-4.5-airx"] {
            for opts in [&[][..], &[kv("e", "high"), flag("s")]] {
                let e = prepare(id, opts, &h).await.unwrap_err();
                assert_eq!(
                    e.message,
                    format!(
                        "`{id}` is not in the bot's Zhipu BigModel model table (its price is unknown), so it is not \
                         supported"
                    )
                );
            }
            assert!(!zp().knows(id), "{id}");
        }
        assert!(zp().knows("glm-5.3-flashx-legal"));
    }

    #[tokio::test]
    async fn preserved_thinking_replays_reasoning_verbatim() {
        let payload = json!({"role": "assistant", "content": "答案", "reasoning_content": "先想一想……\n", "web_search": [{"title": "T"}]});
        let answer = |model: &str| Turn {
            role: Role::Assistant,
            origin: Origin::Live,
            parts: vec![Part::text("答案")],
            native: Some(NativeOutput {
                dialect: DIALECT.into(),
                model: model.into(),
                payload: payload.clone(),
                context_tokens: None,
            }),
        };
        let h = vec![
            Turn { role: Role::System, origin: Origin::Synthetic, parts: vec![Part::text("sys")], native: None },
            Turn::user_text("q1"),
            answer("glm-5.3"),
            Turn::user_text("q2"),
            answer("glm-5.2"),
            Turn::user_text("q3"),
        ];
        let p = prepare("glm-5.3", &[], &h).await.unwrap();
        let m = &p.body["messages"];
        assert_eq!(m[0], json!({"role": "system", "content": "sys"}));
        assert_eq!(m[2], json!({"role": "assistant", "content": "答案", "reasoning_content": "先想一想……\n"}));
        assert_eq!(m[4], json!({"role": "assistant", "content": "答案"}), "another model: the answer only");
    }

    #[tokio::test]
    async fn media_per_model() {
        let jpg = b"\xff\xd8\xff".to_vec();
        let img = BlobRef::for_bytes(&jpg, "image/jpeg");
        let webp = BlobRef::for_bytes(b"RIFF", "image/webp");
        let pdf = BlobRef::for_bytes(b"%PDF-1.7", "application/pdf");
        let mp4 = BlobRef::for_bytes(b"\0\0\0 ftyp", "video/mp4");
        let txt = BlobRef::for_bytes(b"notes", "text/plain");
        let code = BlobRef::for_bytes(b"print(1)", "text/x-python");
        let xls = BlobRef::for_bytes(b"xls", "application/vnd.ms-excel");
        let ogg = BlobRef::for_bytes(b"OggS", "audio/ogg");
        let blobs = Blobs(vec![
            (img.clone(), jpg.clone()),
            (pdf.clone(), b"%PDF-1.7".to_vec()),
            (mp4.clone(), b"\0\0\0 ftyp".to_vec()),
            (txt.clone(), b"notes".to_vec()),
            (code.clone(), b"print(1)".to_vec()),
        ]);
        let user = |parts: Vec<Part>| vec![Turn { role: Role::User, origin: Origin::Live, parts, native: None }];
        let file = |b: &BlobRef, n: &str| Part::File { blob: b.clone(), filename: Some(n.into()), generated: false };
        let image =
            |b: &BlobRef| Part::Image { blob: b.clone(), filename: None, generated: false, dims: None, thought: false };
        let run = |model: &'static str, h: Vec<Turn>| {
            let blobs = &blobs;
            async move { zp().prepare(&input(model, &[], &h), &[], blobs).await }
        };
        // GLM-5.3-Flash: images, videos and documents natively; text files inlined.
        let h = user(vec![
            Part::text("look"),
            image(&img),
            file(&mp4, "clip.mp4"),
            file(&pdf, "a.pdf"),
            file(&txt, "notes.txt"),
            file(&code, "x.py"),
        ]);
        let p = run("glm-5.3-flash", h.clone()).await.unwrap();
        let c = &p.body["messages"][0]["content"];
        assert_eq!(c[0], json!({"type": "text", "text": "look"}));
        assert_eq!(c[1], json!({"type": "image_url", "image_url": {"url": provider::data_url("image/jpeg", &jpg)}}));
        assert_eq!(c[2]["video_url"]["url"], provider::data_url("video/mp4", b"\0\0\0 ftyp"));
        assert_eq!(
            c[3],
            json!({"type": "file", "file": {"file_data": provider::data_url("application/pdf", b"%PDF-1.7"), "filename": "a.pdf"}})
        );
        assert_eq!(c[4], json!({"type": "text", "text": "notes"}), "a txt does not parse as a `file` part");
        assert_eq!(c[5], json!({"type": "text", "text": "print(1)"}));
        assert!(p.notes.is_empty(), "{:?}", p.notes);
        // GLM-5.3 is text only: text files are inlined, media rejected.
        let p = run("glm-5.3", user(vec![Part::text("x"), file(&txt, "notes.txt")])).await.unwrap();
        assert_eq!(p.body["messages"][0]["content"][1], json!({"type": "text", "text": "notes"}));
        let e = run("glm-5.3", user(vec![image(&img)])).await.unwrap_err();
        assert!(
            e.message.contains("does not take image input via Zhipu BigModel (file): it takes text only"),
            "{}",
            e.message
        );
        let e = run("glm-5.3", user(vec![file(&pdf, "a.pdf")])).await.unwrap_err();
        assert!(e.message.contains("does not take PDF input"), "{}", e.message);
        // Vision models without file input.
        let e = run("glm-4.6v", user(vec![file(&pdf, "a.pdf")])).await.unwrap_err();
        assert!(e.message.contains("it takes text, images and videos"), "{}", e.message);
        // Formats and kinds Zhipu does not take.
        let e = run("glm-5.3-flash", user(vec![image(&webp)])).await.unwrap_err();
        assert!(e.message.contains("JPEG and PNG images, not image/webp"), "{}", e.message);
        let e = run("glm-5.3-flash", user(vec![file(&xls, "a.xls")])).await.unwrap_err();
        assert!(e.message.contains("not application/vnd.ms-excel"), "{}", e.message);
        let e = run("glm-5.3-flash", user(vec![file(&ogg, "v.ogg")])).await.unwrap_err();
        assert!(e.message.contains("does not take audio input"), "{}", e.message);
        let e = run(
            "glm-5.3-flash",
            user(vec![Part::Location { latitude: 1.0, longitude: 2.0, accuracy_m: None, title: None, address: None }]),
        )
        .await
        .unwrap_err();
        assert!(e.message.contains("does not take a location via Zhipu BigModel"), "{}", e.message);
    }

    fn chunk(delta: Value, finish: Option<&str>, usage: Option<Value>) -> Value {
        let mut choice = json!({"index": 0, "delta": delta});
        if let Some(f) = finish {
            choice["finish_reason"] = json!(f);
        }
        let mut c = json!({"id": "2026101511124951e1254d36324fc5", "created": 1792033969, "object": "chat.completion.chunk", "model": "glm-5.3", "choices": [choice]});
        if let Some(u) = usage {
            c["usage"] = u;
        }
        c
    }

    fn usage(prompt: u64, completion: u64, reasoning: u64) -> Value {
        json!({"prompt_tokens": prompt, "completion_tokens": completion, "total_tokens": prompt + completion,
               "prompt_tokens_details": {"cached_tokens": 0}, "completion_tokens_details": {"reasoning_tokens": reasoning}})
    }

    fn run_stream(chunks: &[Value]) -> (GlmParser, crate::Accumulator, Vec<StreamEvent>) {
        let mut p = GlmParser::new();
        let mut acc = crate::Accumulator::new();
        let mut evs = Vec::new();
        for c in chunks {
            for e in p.event(None, c).unwrap() {
                evs.push(e.clone());
                acc.apply(e);
            }
        }
        (p, acc, evs)
    }

    /// The documented stream with per-chunk cumulative usage.
    #[test]
    fn stream_with_reasoning_and_cumulative_usage() {
        let (p, acc, evs) = run_stream(&[
            chunk(json!({"role": "assistant", "reasoning_content": "收到"}), None, Some(usage(13, 1, 1))),
            chunk(json!({"role": "assistant", "reasoning_content": "用户的"}), None, Some(usage(13, 2, 2))),
            chunk(json!({"content": "你好"}), None, Some(usage(13, 3, 2))),
            chunk(json!({"role": "assistant", "content": ""}), Some("stop"), Some(usage(13, 248, 198))),
        ]);
        assert!(p.finished());
        assert!(matches!(&acc.parts()[0], Part::Reasoning(r) if r.text.as_deref() == Some("收到用户的")));
        assert_eq!(acc.turn().answer_text(), "你好");
        assert_eq!((acc.finish_reason.as_deref(), acc.native_finish_reason.as_deref()), (Some("stop"), Some("stop")));
        assert_eq!(evs.iter().filter(|e| matches!(e, StreamEvent::Usage(_))).count(), 4);
        let u = acc.usage.unwrap();
        assert_eq!((u.input_tokens, u.output_tokens, u.reasoning_tokens), (Some(13), Some(248), Some(198)));
        assert_eq!(
            native_payload(&p.inner, p.web_search.as_ref()),
            json!({"role": "assistant", "content": "你好", "reasoning_content": "收到用户的"})
        );
    }

    #[test]
    fn web_search_results_become_a_tool_call_and_sources() {
        let results = json!([
            {"icon": "https://i", "title": "2025年5月23日财经早资讯", "link": "https://www.sohu.com/a/1", "media": "搜狐", "publish_date": "2025-05-23", "content": "一、1-4月…", "refer": "ref_1"},
            {"title": "No link"}
        ]);
        let mut first = chunk(json!({"role": "assistant", "content": ""}), None, None);
        first["web_search"] = results.clone();
        let mut again = chunk(json!({"reasoning_content": "看看结果"}), None, None);
        again["web_search"] = results.clone();
        let (p, acc, evs) = run_stream(&[
            first,
            again,
            chunk(json!({"content": "根据 [ref_1] …"}), None, None),
            chunk(json!({"content": ""}), Some("stop"), Some(usage(4199, 868, 0))),
        ]);
        // One call, first (the results come before the answer), then reasoning and text.
        assert_eq!(evs.iter().filter(|e| matches!(e, StreamEvent::Tool(_))).count(), 1);
        let parts = acc.parts();
        let Part::ToolCall(t) = &parts[0] else { panic!("{parts:?}") };
        assert_eq!((t.name.as_str(), t.status.as_deref()), ("web_search", Some("completed")));
        assert_eq!(t.output.as_ref(), Some(&results), "every result field, verbatim");
        assert_eq!(t.input, None, "Zhipu returns no query");
        assert!(matches!(&parts[1], Part::Reasoning(_)));
        let Part::Text { text, citations } = &parts[2] else { panic!("{parts:?}") };
        assert_eq!(text, "根据 [ref_1] …");
        assert_eq!(citations.len(), 1, "results without a link are not sources");
        assert_eq!(citations[0].url.as_deref(), Some("https://www.sohu.com/a/1"));
        assert_eq!(citations[0].cited_text.as_deref(), Some("一、1-4月…"));
        assert_eq!(citations[0].extra.as_ref().unwrap()["refer"], "ref_1");
        assert_eq!(native_payload(&p.inner, p.web_search.as_ref())["web_search"], results);
        // The search fee goes into the cost: 4199 × 8 + 868 × 28 CNY/1M + 0.03 CNY.
        let mut got = Vec::new();
        let mut sink = |e: StreamEvent| got.push(e);
        {
            let mut priced = PricedSink { inner: &mut sink, model: "glm-5.3", cny_per_usd: 7.0, searches: 0 };
            for e in evs {
                priced.event(e);
            }
        }
        let cost = got.iter().find_map(|e| if let StreamEvent::Usage(u) = e { u.cost_usd } else { None }).unwrap();
        assert!((cost - ((4199.0 * 8.0 + 868.0 * 28.0) / 1e6 + 0.03) / 7.0).abs() < 1e-12);
    }

    /// Results returned after the answer (`result_sequence: after`, Zhipu's
    /// default) still become the answer's sources.
    #[test]
    fn web_search_after_the_finish() {
        let mut tail = json!({"id": "x", "choices": [], "usage": usage(10, 2, 0)});
        tail["web_search"] = json!([{"title": "A", "link": "https://a.example"}]);
        let (_, acc, _) = run_stream(&[chunk(json!({"content": "答"}), Some("stop"), None), tail]);
        let parts = acc.parts();
        assert!(
            matches!(&parts[0], Part::Text { text, citations } if text == "答" && citations.len() == 1),
            "{parts:?}"
        );
        assert!(matches!(&parts[1], Part::ToolCall(t) if t.name == "web_search"));
    }

    #[test]
    fn abnormal_finishes() {
        // Moderation stop mid-stream (docs' example): a block, with the filter entries.
        let mut stop = chunk(json!({"content": "", "role": "user"}), Some("sensitive"), None);
        stop["content_filter"] = json!([{"role": "user", "level": 1}]);
        let (p, acc, _) = run_stream(&[chunk(json!({"content": "部分"}), None, None), stop]);
        assert_eq!(
            (acc.finish_reason.as_deref(), acc.native_finish_reason.as_deref()),
            (Some("content_filter"), Some("sensitive"))
        );
        let d = acc.finish_details.unwrap();
        assert_eq!(
            (d["finish_reason"].as_str(), d["content_filter"][0]["level"].as_u64()),
            (Some("sensitive"), Some(1))
        );
        assert!(d.get("delta").is_none());
        assert!(p.abnormal.is_none());
        // Inference failure: a retryable error after the stream, usage kept.
        let (p, acc, evs) = run_stream(&[
            chunk(json!({"content": "部分"}), None, None),
            chunk(json!({}), Some("network_error"), Some(usage(5, 2, 0))),
        ]);
        assert_eq!(acc.finish_reason.as_deref(), Some("error"));
        assert!(evs.iter().any(|e| matches!(e, StreamEvent::Usage(_))));
        let e = p.abnormal.unwrap();
        assert_eq!(
            (e.kind, e.retry, e.error_type.as_deref(), e.partial_output),
            (ErrorKind::InBand, Retry::Backoff, Some("network_error"), true)
        );
        // Context window full: truncated, a normal end.
        let (p, acc, _) = run_stream(&[chunk(json!({"content": "x"}), Some("model_context_window_exceeded"), None)]);
        assert_eq!(
            (acc.finish_reason.as_deref(), acc.native_finish_reason.as_deref()),
            (Some("length"), Some("model_context_window_exceeded"))
        );
        assert!(p.abnormal.is_none());
        // An error object inside the stream (code 1301 while streaming).
        let mut p = GlmParser::new();
        let e = p
            .event(
                None,
                &json!({"error": {"code": "1301", "message": "系统检测到输入或生成内容可能包含不安全或敏感内容"}}),
            )
            .unwrap_err();
        assert_eq!((e.kind, e.retry, e.error_type.as_deref()), (ErrorKind::InBand, Retry::Never, Some("1301")));
    }

    #[test]
    fn errors_are_classified() {
        // Recorded 2026-11-12 without a key / with a bad key.
        let e = chatstream::http_error(
            401,
            None,
            r#"{"error":{"code":"1001","message":"Header中未收到Authorization参数，无法进行身份验证。"}}"#,
            error_from_json,
        );
        assert_eq!((e.retry, e.status, e.error_type.as_deref()), (Retry::Never, Some(401), Some("1001")));
        let e = chatstream::http_error(
            401,
            None,
            r#"{"error":{"code":"1000","message":"身份验证失败。"}}"#,
            error_from_json,
        );
        assert_eq!((e.retry, e.message.as_str()), (Retry::Never, "身份验证失败。"));
        let body = |code: &str| format!(r#"{{"error":{{"code":"{code}","message":"m"}}}}"#);
        let retry = |status: u16, code: &str| chatstream::http_error(status, None, &body(code), error_from_json).retry;
        assert_eq!(retry(429, "1302"), Retry::After(20));
        assert_eq!(retry(429, "1305"), Retry::After(30));
        assert_eq!(retry(500, "1200"), Retry::Backoff);
        assert_eq!(retry(500, "1234"), Retry::Backoff);
        for (status, code) in
            [(429, "1113"), (400, "1211"), (400, "1214"), (403, "1220"), (400, "1301"), (429, "1308"), (429, "1310")]
        {
            assert_eq!(retry(status, code), Retry::Never, "{code}");
        }
        assert_eq!(retry(503, "9999"), Retry::Backoff, "unknown codes: by HTTP status");
        let e = chatstream::http_error(400, None, &body("1261"), error_from_json);
        assert!(e.is_context_overflow());
        // The moderation block keeps its top-level `contentFilter` in the details.
        let e = chatstream::http_error(
            400,
            None,
            r#"{"contentFilter":[{"level":1,"role":"user"}],"error":{"code":"1301","message":"系统检测到输入或生成内容可能包含不安全或敏感内容"}}"#,
            error_from_json,
        );
        assert_eq!(e.details.unwrap()["contentFilter"][0]["role"], "user");
        // Numeric codes too (the intl schema says int32).
        assert_eq!(
            error_from_json(&json!({"code": 1302, "message": "m"}), Some(429), None, ErrorKind::Http).retry,
            Retry::After(20)
        );
    }

    #[test]
    fn price_tiers() {
        let u = |input: u64, cached: u64, output: u64| Usage {
            input_tokens: Some(input),
            cached_input_tokens: Some(cached),
            output_tokens: Some(output),
            ..Default::default()
        };
        let cost = |model: &str, u: &Usage| estimate_cost(model, u, 0, 1.0).unwrap();
        // glm-5.3: 1M input with 0.4M hits, 0.1M output = 0.6 × 8 + 0.4 × 2 + 0.1 × 28 CNY.
        assert!((cost("glm-5.3", &u(1_000_000, 400_000, 100_000)) - 8.4).abs() < 1e-9);
        // glm-5.1 by input length.
        assert!((cost("glm-5.1", &u(31_999, 0, 0)) - 31_999.0 * 6.0 / 1e6).abs() < 1e-12);
        assert!((cost("glm-5.1", &u(32_000, 0, 0)) - 32_000.0 * 8.0 / 1e6).abs() < 1e-12);
        // glm-4.7 by input and output length.
        assert!((cost("glm-4.7", &u(1_000, 0, 199)) - (1_000.0 * 2.0 + 199.0 * 8.0) / 1e6).abs() < 1e-12);
        assert!((cost("glm-4.7", &u(1_000, 0, 200)) - (1_000.0 * 3.0 + 200.0 * 14.0) / 1e6).abs() < 1e-12);
        assert!((cost("glm-4.7", &u(40_000, 0, 10)) - (40_000.0 * 4.0 + 10.0 * 16.0) / 1e6).abs() < 1e-12);
        assert_eq!(cost("glm-4.7-flash", &u(1_000, 0, 1_000)), 0.0);
        assert!(estimate_cost("glm-4.6", &u(1, 0, 1), 0, 7.0).is_none(), "not on the price list");
        assert!(estimate_cost("glm-6", &u(1, 0, 1), 0, 7.0).is_none());
    }

    #[test]
    fn model_table() {
        let s = model_spec("glm-5.3").unwrap();
        assert_eq!((s.context, s.max_output, s.image, s.search), (M1, 131_072, false, true));
        assert!(!s.thinking.can_disable());
        let f = model_spec("glm-5.3-flash").unwrap();
        assert_eq!((f.image, f.video, f.file, f.search), (true, true, true, false));
        assert_eq!(model_spec("glm-5.2").unwrap().thinking, Thinking::Switch(&["max", "high"]));
        assert!(MODELS.iter().all(|m| m.max_output <= 131_072 && m.context >= m.max_output));
        assert!(MODELS.iter().filter(|m| m.image).all(|m| !m.search));
    }

    /// One HTTP exchange on a local port: returns the base URL and a handle
    /// yielding the request head and JSON body.
    async fn serve(status: u16, headers: &str, body: String) -> (String, tokio::task::JoinHandle<(String, Value)>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/api/paas/v4", listener.local_addr().unwrap());
        let headers = headers.to_string();
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let (mut buf, mut tmp) = (Vec::new(), [0u8; 8192]);
            let (head, sent) = loop {
                let n = sock.read(&mut tmp).await.unwrap();
                buf.extend_from_slice(&tmp[..n]);
                if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&buf[..pos]).to_string();
                    let len = head
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse().unwrap())
                        })
                        .unwrap_or(0usize);
                    while buf.len() < pos + 4 + len {
                        let n = sock.read(&mut tmp).await.unwrap();
                        buf.extend_from_slice(&tmp[..n]);
                    }
                    break (head, serde_json::from_slice(&buf[pos + 4..pos + 4 + len]).unwrap_or(Value::Null));
                }
            };
            let resp = format!(
                "HTTP/1.1 {status} X\r\n{headers}content-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            sock.write_all(resp.as_bytes()).await.unwrap();
            let _ = sock.shutdown().await;
            (head, sent)
        });
        (base, server)
    }

    async fn exchange(
        status: u16,
        headers: &str,
        body: String,
        model: &str,
        options: &[OptionToken],
    ) -> (Result<Value, LlmError>, CallOutcome, Vec<StreamEvent>, String, Value) {
        let (base, server) = serve(status, headers, body).await;
        let z = zp_at(&base);
        let h = vec![Turn::user_text("hi")];
        let p = z.prepare(&input(model, options, &h), &[], &Blobs(vec![])).await.unwrap();
        let mut evs = Vec::new();
        let (r, o) =
            z.stream(&p, &mut |e: StreamEvent| evs.push(e), &crate::NullObserver, &CancellationToken::new()).await;
        let (head, sent) = server.await.unwrap();
        assert_eq!(sent, p.body);
        (r, o, evs, head, sent)
    }

    fn sse(chunks: &[Value]) -> String {
        let mut body: String = chunks.iter().map(|c| format!("data: {c}\n\n")).collect();
        body.push_str("data: [DONE]\n\n");
        body
    }

    #[tokio::test]
    async fn end_to_end_with_search() {
        let mut first = chunk(json!({"role": "assistant", "content": ""}), None, Some(usage(20, 0, 0)));
        first["web_search"] = json!([{"title": "A", "link": "https://a.example", "content": "…", "refer": "ref_1"}]);
        let body = sse(&[
            first,
            chunk(json!({"reasoning_content": "想"}), None, Some(usage(20, 1, 1))),
            chunk(json!({"content": "答"}), None, Some(usage(20, 2, 1))),
            chunk(json!({"content": ""}), Some("stop"), Some(usage(20, 2, 1))),
        ]);
        let (r, outcome, evs, head, sent) =
            exchange(200, "content-type: text/event-stream\r\nx-log-id: log-1\r\n", body, "glm-5.3", &[flag("s")])
                .await;
        assert!(head.starts_with("POST /api/paas/v4/chat/completions HTTP/1.1"), "{head}");
        assert!(head.to_ascii_lowercase().contains("authorization: bearer k"), "{head}");
        assert_eq!(sent["tools"][0]["web_search"]["search_engine"], "search_pro");
        assert_eq!(sent["tools"][0]["web_search"]["count"], 50);
        assert_eq!(sent["tools"][0]["web_search"]["content_size"], "high");
        assert_eq!(outcome.upstream_id.as_deref(), Some("log-1"));
        assert!(!evs.iter().any(|e| matches!(e, StreamEvent::Notice(_))), "a search ran");
        let native = r.unwrap();
        assert_eq!((native["content"].as_str(), native["reasoning_content"].as_str()), (Some("答"), Some("想")));
        assert_eq!(native["web_search"][0]["refer"], "ref_1");
        // Every chunk's cumulative usage is priced, with the search fee from the start.
        let costs: Vec<f64> =
            evs.iter().filter_map(|e| if let StreamEvent::Usage(u) = e { u.cost_usd } else { None }).collect();
        assert_eq!(costs.len(), 4);
        assert!((costs[3] - ((20.0 * 8.0 + 2.0 * 28.0) / 1e6 + 0.03) / 7.0).abs() < 1e-12, "{costs:?}");
        assert!(costs.iter().all(|c| *c >= 0.03 / 7.0));
    }

    /// `+s`, but Zhipu's intent detection ran no search: no `web_search[]`, a notice.
    #[tokio::test]
    async fn end_to_end_without_a_search() {
        let body = sse(&[
            chunk(json!({"role": "assistant", "content": "你好"}), None, Some(usage(20, 2, 0))),
            chunk(json!({"content": ""}), Some("stop"), Some(usage(20, 2, 0))),
        ]);
        let (r, _, evs, _, _) =
            exchange(200, "content-type: text/event-stream\r\n", body.clone(), "glm-5.3", &[flag("s")]).await;
        assert!(r.is_ok());
        assert!(!evs.iter().any(|e| matches!(e, StreamEvent::Tool(_))));
        let notices: Vec<_> =
            evs.iter().filter_map(|e| if let StreamEvent::Notice(n) = e { Some(n.as_str()) } else { None }).collect();
        assert_eq!(notices, ["no web search ran (Zhipu's search-intent detection decides)"]);
        // No search fee either: 20 × 8 + 2 × 28 CNY/1M.
        let cost =
            evs.iter().rev().find_map(|e| if let StreamEvent::Usage(u) = e { u.cost_usd } else { None }).unwrap();
        assert!((cost - (20.0 * 8.0 + 2.0 * 28.0) / 1e6 / 7.0).abs() < 1e-12);
        // Without `+s`, nothing to say.
        let (_, _, evs, _, _) = exchange(200, "content-type: text/event-stream\r\n", body, "glm-5.3", &[]).await;
        assert!(!evs.iter().any(|e| matches!(e, StreamEvent::Notice(_))));
    }

    /// Media other models generated in earlier turns: assistant messages carry
    /// text only, so the request is refused.
    #[tokio::test]
    async fn generated_media_of_other_models_are_not_sent() {
        let png = BlobRef::for_bytes(b"\x89PNG", "image/png");
        let pdf = BlobRef::for_bytes(b"%PDF", "application/pdf");
        let earlier = Turn {
            role: Role::Assistant,
            origin: Origin::Live,
            parts: vec![
                Part::text("Done:"),
                Part::Image { blob: png, filename: None, generated: true, dims: None, thought: false },
                Part::File { blob: pdf, filename: Some("r.pdf".into()), generated: true },
            ],
            native: None,
        };
        let h = vec![Turn::user_text("make"), earlier.clone(), Turn::user_text("summarize")];
        let e = prepare("glm-5.3", &[], &h).await.unwrap_err();
        assert_eq!(
            e.message,
            "1 image and 1 file generated in an earlier turn cannot be sent: this model sees only the text of other models' answers; send it the file as an attachment"
        );
        // The answer's text alone goes as an assistant message.
        let text = Turn { parts: vec![Part::text("Done:")], ..earlier };
        let p = prepare("glm-5.3", &[], &[Turn::user_text("make"), text, Turn::user_text("summarize")]).await.unwrap();
        assert_eq!(p.body["messages"][1], json!({"role": "assistant", "content": "Done:"}));
        assert!(p.warnings.is_empty(), "{:?}", p.warnings);
    }

    #[tokio::test]
    async fn end_to_end_failures() {
        // Rate limited: retry after at least 20 s; the body verbatim.
        let err = r#"{"error":{"code":"1302","message":"您的账户已达到速率限制，请您控制请求频率"}}"#;
        let (r, _, _, _, _) =
            exchange(429, "content-type: application/json\r\n", err.to_string(), "glm-5.3", &[]).await;
        let e = r.unwrap_err();
        assert_eq!(
            (e.kind, e.status, e.retry, e.error_type.as_deref()),
            (ErrorKind::Http, Some(429), Retry::After(20), Some("1302"))
        );
        assert_eq!(e.details.unwrap(), serde_json::from_str::<Value>(err).unwrap());
        // Inference failure mid-stream: a retryable error once the stream has ended.
        let body = sse(&[
            chunk(json!({"content": "部分"}), None, None),
            chunk(json!({}), Some("network_error"), Some(usage(5, 1, 0))),
        ]);
        let (r, _, evs, _, _) = exchange(200, "content-type: text/event-stream\r\n", body, "glm-5.3-flash", &[]).await;
        let e = r.unwrap_err();
        assert_eq!((e.retry, e.error_type.as_deref(), e.partial_output), (Retry::Backoff, Some("network_error"), true));
        assert!(evs.iter().any(|e| matches!(e, StreamEvent::Usage(u) if u.cost_usd.is_some())));
        // Moderation stop: the stream "succeeds" with a content-filter finish (a failure for the bot).
        let body = sse(&[
            chunk(json!({"content": "部分"}), None, None),
            chunk(json!({"content": ""}), Some("sensitive"), None),
        ]);
        let (r, _, evs, _, _) = exchange(200, "content-type: text/event-stream\r\n", body, "glm-5.3", &[]).await;
        assert!(r.is_ok());
        assert!(evs.iter().any(|e| matches!(e, StreamEvent::Finish { reason: Some(r), .. } if r == "content_filter")));
    }
}
