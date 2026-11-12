//! DeepSeek's own API on its domestic site (`https://api.deepseek.com`, as in
//! the old bot); see docs/research/xai_deepseek.md Part B. Docs re-checked
//! 2026-11-12 (api-docs.deepseek.com, `.md` pages).
//!
//! - **Chat Completions** (`POST /chat/completions`, dialect `deepseek.chat`)
//!   for every request without web search: thinking on with
//!   `reasoning_effort: "max"` by default (`e=low|high`; `e=none` turns
//!   thinking off), an explicit `max_tokens` (the API defaults to 128K at
//!   `max` and allows 384K), images at `detail: "original"` (deepseek-flash
//!   only). No `user_id`: DeepSeek uses it to isolate the KV cache per user,
//!   so people continuing one conversation in a group would stop sharing cache
//!   hits (on flash a hit costs 1/50 of a miss); the old bot did not send it
//!   either.
//! - **Web search** (`+s`) exists only as a server tool of the
//!   Anthropic-compatible Messages endpoint and of the Responses API, and only
//!   for `deepseek-v4-pro`. The Messages endpoint
//!   (`POST /anthropic/v1/messages`, dialect `deepseek.messages`) takes the same
//!   thinking switch (`thinking.type`) and effort levels
//!   (`output_config.effort`), so `+s` requests go there. Its stream is
//!   Anthropic's (OpenRouter's Messages parser, plus DeepSeek's unnamed
//!   `{"error":…}` frame).
//! - **Replay** only within the same dialect and model: Chat `content` +
//!   `reasoning_content` (the official multi-turn sample sends it back; without
//!   `tools` the API ignores it), Messages content blocks verbatim (thinking
//!   with its signature, the search call and its results).
//! - **Finishes**: `content_filter` is a block (not retried);
//!   `insufficient_system_resource` and `aborted` interrupt the answer, so
//!   they are retryable failures.
//! - **Cost**: the CNY price list with peak hours (Beijing time 9–12 and
//!   14–18 on weekdays that are not Chinese public holidays, judged at the
//!   request time), plus 5 CNY per 1000 successful searches.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use omni_core::Usage;
use omni_core::content::{Part, Role, Turn};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::chatstream::{self, EventParser};
use crate::error::{ErrorKind, LlmError, Retry, retry_for_status};
use crate::event::StreamEvent;
use crate::openrouter::chat::ChunkParser;
use crate::openrouter::messages;
use crate::openrouter::options::{Applied, overlay};
use crate::provider::{self, Currency, DirectConfig, PricingSink, Provider, TokenPrice};
use crate::request::{self, Requested, RouteCaps, Tool};
use crate::{BlobLoader, CallInput, CallOutcome, EventSink, MediaClass, Observer, Prepared, classify_mime};

/// Chat Completions.
pub const DIALECT: &str = "deepseek.chat";
/// The Anthropic-compatible Messages endpoint (web search).
pub const MESSAGES_DIALECT: &str = "deepseek.messages";

const NAME: &str = "DeepSeek";
/// Response header with DeepSeek's trace id.
const ID_HEADER: &str = "x-ds-trace-id";
/// `max_tokens` must be between 1 and 384K on Chat Completions.
const MAX_TOKENS_LIMIT: u64 = 393_216;
/// Effort levels while thinking (`/models` `effort.supported_levels`), highest
/// first. The aliases the API also accepts (`minimal` → low, `medium`/`xhigh`
/// → high) are not offered: they would be remapped silently.
const EFFORTS: &[&str] = &["max", "high", "low"];
/// The server-side search tool of the Messages endpoint (both documented
/// versions select the same tool). `max_uses` is left at its default, which is
/// also the cap (5).
const SEARCH_TOOL: &str = "web_search_20260209";
/// CNY per successful search (`usage.server_tool_use.tool_search`): 5 CNY per
/// 1000 successful requests, the same at all hours.
pub const SEARCH_CNY: f64 = 0.005;

/// A model id the API accepts, with its limits and list prices.
#[derive(Debug, Clone, Copy)]
pub struct ModelSpec {
    pub id: &'static str,
    /// The model that serves the id.
    pub serves: &'static str,
    pub context: u64,
    pub max_output: u64,
    /// Image input.
    pub vision: bool,
    /// The server-side web search tool.
    pub search: bool,
    /// CNY per 1M tokens: off-peak, and peak (twice as much).
    pub off_peak: TokenPrice,
    pub peak: TokenPrice,
}

const fn cny(miss: f64, hit: f64, output: f64) -> TokenPrice {
    // No separate cache-write price: misses are simply input.
    TokenPrice { input: miss, cached_input: hit, cache_write: miss, output }
}

const FLASH: ModelSpec = ModelSpec {
    id: "deepseek-flash",
    serves: "DeepSeek-V4.1-Flash-1017",
    context: 1_048_576,
    max_output: 393_216,
    vision: true,
    search: false,
    off_peak: cny(1.0, 0.02, 4.0),
    peak: cny(2.0, 0.04, 8.0),
};

/// Model table and CNY prices from the Chinese price page
/// (api-docs.deepseek.com/zh-cn/quick_start/pricing, fetched 2026-11-12).
pub const MODELS: &[ModelSpec] = &[
    FLASH,
    ModelSpec {
        id: "deepseek-v4-pro",
        serves: "DeepSeek-V4-Pro-0813",
        vision: false,
        search: true,
        off_peak: cny(4.5, 0.15, 13.5),
        peak: cny(9.0, 0.30, 27.0),
        ..FLASH
    },
    // Legacy names, still accepted: served by V4.1-Flash-1017 at the Flash price.
    ModelSpec { id: "deepseek-v4-flash", ..FLASH },
    ModelSpec { id: "deepseek-v4-flash-vision-exp", ..FLASH },
];

pub fn model_spec(model: &str) -> Option<&'static ModelSpec> {
    MODELS.iter().find(|m| m.id == model)
}

/// What DeepSeek's API offers for a model (the vendor's own API: official = route).
struct Caps(Option<&'static ModelSpec>);

impl RouteCaps for Caps {
    fn name(&self) -> &str {
        NAME
    }
    fn tool(&self, tool: Tool) -> bool {
        tool == Tool::Search && self.0.is_some_and(|s| s.search)
    }
    fn efforts(&self) -> Option<Vec<String>> {
        Some(EFFORTS.iter().map(|e| e.to_string()).collect())
    }
    fn can_disable_reasoning(&self) -> bool {
        true
    }
    fn budget(&self) -> bool {
        false // `budget_tokens` is ignored by the Messages endpoint; Chat has none
    }
    fn verbosity(&self) -> bool {
        false
    }
    fn image_options(&self) -> bool {
        false
    }
    fn tiers(&self) -> Vec<String> {
        Vec::new() // no service tiers (`service_tier` is ignored on Messages)
    }
}

pub struct DeepSeek {
    cfg: DirectConfig,
    http: reqwest::Client,
}

impl DeepSeek {
    pub fn new(cfg: DirectConfig) -> anyhow::Result<Self> {
        let http = cfg.http()?;
        Ok(Self { cfg, http })
    }

    fn auto_max_tokens(&self, input: &CallInput<'_>, spec: Option<&ModelSpec>) -> Option<u64> {
        let prompt = crate::budget::estimate_prompt(input.history, input.known_context);
        match spec {
            Some(s) => crate::budget::choose(Some(s.context), Some(s.max_output), self.cfg.max_tokens_cap, prompt),
            // Unknown id: the API's own default (128K at max effort) would truncate
            // silently; the cap, within the API's range.
            None => self.cfg.max_tokens_cap.map(|c| c.min(MAX_TOKENS_LIMIT)),
        }
    }
}

#[async_trait::async_trait]
impl Provider for DeepSeek {
    fn id(&self) -> &'static str {
        "deepseek"
    }
    fn name(&self) -> &'static str {
        NAME
    }
    fn knows(&self, model: &str) -> bool {
        model_spec(model).is_some()
    }

    async fn prepare(
        &self,
        input: &CallInput<'_>,
        presets: &[Requested],
        blobs: &dyn BlobLoader,
    ) -> Result<Prepared, LlmError> {
        let user = request::parse(input.options).map_err(LlmError::request)?;
        let mut req = Requested::default();
        for p in presets {
            req = overlay(&req, p);
        }
        let req = overlay(&req, &user);
        if req.providers.is_some() {
            return Err(LlmError::request("`p=` (provider choice) does not apply to DeepSeek"));
        }
        let model = input.model;
        let spec = model_spec(model);
        let search = req.tools.contains(&Tool::Search);
        if search && !spec.is_some_and(|s| s.search) {
            return Err(LlmError::request(format!(
                "`{model}` has no web search (`+s`): DeepSeek's server-side web search is documented for \
                 deepseek-v4-pro only"
            )));
        }
        request::validate(model, &req, &Caps(spec)).map_err(LlmError::request)?;

        let mut applied = Vec::new();
        let mut settings = Vec::new();
        let effort = req.effort.as_deref().unwrap_or(EFFORTS[0]);
        let source = |by_user: bool| if by_user { "option" } else { "default" };
        // Thinking: an explicit `thinking.type` decides the mode on both endpoints.
        let reasoning = match (effort, search) {
            ("none", _) => json!({"thinking": {"type": "disabled"}}),
            (e, false) => json!({"thinking": {"type": "enabled"}, "reasoning_effort": e}),
            (e, true) => json!({"thinking": {"type": "enabled"}, "output_config": {"effort": e}}),
        };
        settings.push(if effort == "none" { "thinking off".to_string() } else { format!("effort {effort}") });
        applied.push(Applied { source: source(user.effort.is_some()).into(), patch: reasoning.clone() });

        let (mut body, url, dialect, notes) = if search {
            let (system, msgs, notes) = messages_request(input.history, model, blobs).await?;
            let mut body = json!({"model": model, "messages": msgs, "stream": true});
            if let Some(s) = system {
                body["system"] = s;
            }
            (body, self.cfg.url("/anthropic/v1/messages"), MESSAGES_DIALECT, notes)
        } else {
            let (msgs, notes) = chat_messages(input.history, model, spec, blobs).await?;
            (
                json!({"model": model, "messages": msgs, "stream": true}),
                self.cfg.url("/chat/completions"),
                DIALECT,
                notes,
            )
        };
        for (k, v) in reasoning.as_object().into_iter().flatten() {
            body[k] = v.clone();
        }
        let max_tokens = input.max_tokens_override.or_else(|| self.auto_max_tokens(input, spec));
        if let Some(n) = max_tokens {
            body["max_tokens"] = json!(n);
            applied.push(Applied { source: "budget".into(), patch: json!({"max_tokens": n}) });
        }
        if search {
            let tools = json!([{"type": SEARCH_TOOL, "name": "web_search"}]);
            body["tools"] = tools.clone();
            settings.push(Tool::Search.describe().to_string());
            applied.push(Applied {
                source: source(user.tools.contains(&Tool::Search)).into(),
                patch: json!({"tools": tools}),
            });
        }

        let mut warnings = Vec::new();
        if spec.is_none() {
            warnings.push(format!(
                "`{model}` is not in the bot's DeepSeek model table: limits and price unknown (max_tokens {}; cost not \
                 estimated)",
                max_tokens.map(|n| n.to_string()).unwrap_or_else(|| "not set".into())
            ));
        }
        if beijing_now().0 > CN_HOLIDAYS_UNTIL {
            warnings.push(format!(
                "The bot's calendar of Chinese public holidays ends with {CN_HOLIDAYS_UNTIL}: holidays on weekdays \
                 are estimated at DeepSeek's peak price"
            ));
        }
        Ok(Prepared {
            dialect,
            model: model.to_string(),
            url,
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
        // Peak or off-peak price: by the time the request is sent.
        let peak = is_peak(unix_now());
        let model = p.model.clone();
        let cny_per_usd = self.cfg.cny_per_usd;
        let estimate = move |u: &Usage| estimate_cost(&model, u, peak, cny_per_usd);
        let mut priced = PricingSink { inner: sink, estimate: &estimate };
        let rb = self.http.post(&p.url).bearer_auth(&self.cfg.api_key).json(&p.body);
        let idle = self.cfg.idle_timeout;
        if p.dialect == MESSAGES_DIALECT {
            let s = chatstream::run_with(
                rb,
                &p.url,
                &p.body,
                idle,
                Some(ID_HEADER),
                error_from_json,
                MessagesParser::default(),
                &mut priced,
                observer,
                cancel,
            )
            .await;
            let outcome = CallOutcome { upstream_id: s.header_id, ..Default::default() };
            match s.result {
                Err(e) => (Err(e), outcome),
                Ok(()) => (Ok(s.parser.inner.native_payload()), outcome),
            }
        } else {
            let s = chatstream::run_with(
                rb,
                &p.url,
                &p.body,
                idle,
                Some(ID_HEADER),
                error_from_json,
                ChatParser::new(),
                &mut priced,
                observer,
                cancel,
            )
            .await;
            let outcome = CallOutcome { upstream_id: s.header_id, ..Default::default() };
            match (s.result, s.parser.abnormal) {
                (Err(e), _) | (Ok(()), Some(e)) => (Err(e), outcome),
                (Ok(()), None) => (Ok(chat_native(&s.parser.inner)), outcome),
            }
        }
    }

    /// `GET /user/balance`, verbatim (`is_available`, `balance_infos` per currency).
    async fn balance(&self) -> Option<anyhow::Result<Value>> {
        Some(
            async {
                let resp = self
                    .http
                    .get(self.cfg.url("/user/balance"))
                    .bearer_auth(&self.cfg.api_key)
                    .timeout(Duration::from_secs(60))
                    .send()
                    .await?;
                let status = resp.status();
                let text = resp.text().await?;
                let v = serde_json::from_str::<Value>(&text).unwrap_or(Value::String(text));
                anyhow::ensure!(status.is_success(), "HTTP {}: {v}", status.as_u16());
                Ok(v)
            }
            .await,
        )
    }
}

// ------------------------------------------------------------------ history

/// The assistant message replayed to the same model on Chat Completions.
fn replay_chat(p: &Value) -> Value {
    let mut m = json!({"role": "assistant", "content": p.get("content").cloned().unwrap_or(json!(""))});
    if let Some(r) = p.get("reasoning_content").and_then(Value::as_str).filter(|s| !s.is_empty()) {
        m["reasoning_content"] = json!(r);
    }
    m
}

/// The native payload of a Chat Completions answer: the assistant message as
/// it is replayed (`content`, `reasoning_content`).
fn chat_native(p: &ChunkParser) -> Value {
    let n = p.native_payload();
    let mut m = json!({"role": "assistant", "content": n.get("content").cloned().unwrap_or(json!(""))});
    if let Some(r) = n.get("reasoning").and_then(Value::as_str).filter(|s| !s.is_empty()) {
        m["reasoning_content"] = json!(r);
    }
    m
}

fn file_name(filename: &Option<String>) -> &str {
    filename.as_deref().unwrap_or("file")
}

/// A text file inlined as a separate text part (DeepSeek has no text-file input).
async fn inline_text(
    blobs: &dyn BlobLoader,
    blob: &omni_core::BlobRef,
    filename: &Option<String>,
    notes: &mut Vec<String>,
) -> Result<Value, LlmError> {
    let name = file_name(filename);
    let text = String::from_utf8(provider::load(blobs, blob).await?)
        .map_err(|_| LlmError::request(format!("{name} is not valid UTF-8 text")))?;
    notes.push(format!("📎 {name} inlined as a separate text part by the bot (no native text-file input on this API)"));
    Ok(json!({"type": "text", "text": text}))
}

/// Media DeepSeek cannot take (anything but text and, on vision models, images).
fn no_media(model: &str, what: &str, name: &str, vision: bool) -> LlmError {
    let takes = if vision { "text and images" } else { "text only" };
    LlmError::request(format!("`{model}` does not take {what} input via DeepSeek ({name}): it takes {takes}"))
}

/// An image part (deepseek-flash): JPEG, PNG, GIF or WebP, at the original resolution.
async fn image_part(
    model: &str,
    vision: bool,
    blob: &omni_core::BlobRef,
    name: &str,
    blobs: &dyn BlobLoader,
) -> Result<Value, LlmError> {
    if !vision {
        return Err(LlmError::request(format!(
            "`{model}` does not take image input via DeepSeek ({name}): images only on deepseek-flash"
        )));
    }
    if !matches!(blob.mime.as_str(), "image/jpeg" | "image/jpg" | "image/png" | "image/gif" | "image/webp") {
        return Err(LlmError::request(format!(
            "DeepSeek takes JPEG, PNG, GIF and WebP images, not {} ({name})",
            blob.mime
        )));
    }
    let url = provider::data_url(&blob.mime, &provider::load(blobs, blob).await?);
    Ok(json!({"type": "image_url", "image_url": {"url": url, "detail": "original"}}))
}

/// The conversation as Chat Completions `messages`, with conversion notes.
async fn chat_messages(
    history: &[Turn],
    model: &str,
    spec: Option<&ModelSpec>,
    blobs: &dyn BlobLoader,
) -> Result<(Vec<Value>, Vec<String>), LlmError> {
    // Unknown ids: let the API decide about images.
    let vision = spec.is_none_or(|s| s.vision);
    let mut notes = Vec::new();
    let mut out = Vec::with_capacity(history.len());
    for turn in history {
        match turn.role {
            // Chat Completions takes system messages anywhere in the conversation.
            Role::System => out.push(json!({"role": "system", "content": turn.answer_text()})),
            Role::Assistant => match &turn.native {
                Some(n) if n.dialect == DIALECT && n.model == model => out.push(replay_chat(&n.payload)),
                _ => out.push(json!({"role": "assistant", "content": turn.answer_text()})),
            },
            Role::User => {
                let mut content: Vec<Value> = Vec::new();
                for part in &turn.parts {
                    match part {
                        Part::Text { text, .. } => {
                            if !text.is_empty() {
                                content.push(json!({"type": "text", "text": text}));
                            }
                        }
                        Part::Image { blob, filename, .. } => {
                            content.push(image_part(model, vision, blob, file_name(filename), blobs).await?)
                        }
                        Part::File { blob, filename, .. } => match classify_mime(&blob.mime, filename.as_deref()) {
                            MediaClass::Image => {
                                content.push(image_part(model, vision, blob, file_name(filename), blobs).await?)
                            }
                            MediaClass::Text => content.push(inline_text(blobs, blob, filename, &mut notes).await?),
                            MediaClass::Pdf => return Err(no_media(model, "PDF", file_name(filename), vision)),
                            _ => return Err(no_media(model, &blob.mime, file_name(filename), vision)),
                        },
                        Part::RemoteMedia { url, .. } => {
                            return Err(LlmError::request(format!(
                                "`{model}` cannot read remote media via DeepSeek ({url})"
                            )));
                        }
                        Part::Location { .. } => return Err(provider::no_location(model, NAME)),
                        Part::Reasoning(_) | Part::ToolCall(_) | Part::Refusal { .. } => {}
                    }
                }
                out.push(user_message(content));
            }
        }
    }
    Ok((out, notes))
}

/// A user message: a single text part as a plain string, otherwise the parts.
fn user_message(content: Vec<Value>) -> Value {
    if content.len() == 1 && content[0]["type"] == "text" {
        json!({"role": "user", "content": content[0]["text"].clone()})
    } else {
        json!({"role": "user", "content": content})
    }
}

/// The conversation for the Messages endpoint: (`system`, `messages`, notes).
/// Only deepseek-v4-pro (text only) comes here.
async fn messages_request(
    history: &[Turn],
    model: &str,
    blobs: &dyn BlobLoader,
) -> Result<(Option<Value>, Vec<Value>, Vec<String>), LlmError> {
    let mut notes = Vec::new();
    let mut system = Vec::new();
    let mut out = Vec::with_capacity(history.len());
    for turn in history {
        match turn.role {
            Role::System => {
                // The Messages format has only the leading system prompt (and
                // deepseek-v4-pro reads only that one).
                if !out.is_empty() {
                    return Err(LlmError::request(format!(
                        "`{model}` with web search (+s) runs on DeepSeek's Anthropic-compatible API, which takes a \
                         system prompt only at the start of the conversation"
                    )));
                }
                system.push(json!({"type": "text", "text": turn.answer_text()}));
            }
            Role::Assistant => match &turn.native {
                Some(n) if n.dialect == MESSAGES_DIALECT && n.model == model => {
                    let blocks = n.payload.get("content").cloned().unwrap_or(json!([]));
                    out.push(json!({"role": "assistant", "content": blocks}));
                }
                _ => out.push(json!({"role": "assistant", "content": turn.answer_text()})),
            },
            Role::User => {
                let mut content: Vec<Value> = Vec::new();
                for part in &turn.parts {
                    match part {
                        Part::Text { text, .. } => {
                            if !text.is_empty() {
                                content.push(json!({"type": "text", "text": text}));
                            }
                        }
                        Part::Image { blob, filename, .. } => {
                            content.push(image_part(model, false, blob, file_name(filename), blobs).await?)
                        }
                        Part::File { blob, filename, .. } => match classify_mime(&blob.mime, filename.as_deref()) {
                            MediaClass::Text => content.push(inline_text(blobs, blob, filename, &mut notes).await?),
                            MediaClass::Image => {
                                content.push(image_part(model, false, blob, file_name(filename), blobs).await?)
                            }
                            MediaClass::Pdf => return Err(no_media(model, "PDF", file_name(filename), false)),
                            _ => return Err(no_media(model, &blob.mime, file_name(filename), false)),
                        },
                        Part::RemoteMedia { url, .. } => {
                            return Err(LlmError::request(format!(
                                "`{model}` cannot read remote media via DeepSeek ({url})"
                            )));
                        }
                        Part::Location { .. } => return Err(provider::no_location(model, NAME)),
                        Part::Reasoning(_) | Part::ToolCall(_) | Part::Refusal { .. } => {}
                    }
                }
                out.push(user_message(content));
            }
        }
    }
    Ok(((!system.is_empty()).then_some(Value::Array(system)), out, notes))
}

// ------------------------------------------------------------------ streams

/// Chat Completions chunks, with DeepSeek's abnormal finishes.
struct ChatParser {
    inner: ChunkParser,
    /// The answer was interrupted (`insufficient_system_resource`, `aborted`):
    /// the call fails (retryably) once the stream has ended.
    abnormal: Option<LlmError>,
}

impl ChatParser {
    fn new() -> Self {
        let mut inner = ChunkParser::default();
        inner.map_error = Some(error_from_json);
        Self { inner, abnormal: None }
    }
}

/// The finish choice without its content: the provider's own account of the stop.
fn finish_choice(chunk: &Value) -> Value {
    let mut c = chunk.pointer("/choices/0").cloned().unwrap_or(json!({}));
    if let Some(o) = c.as_object_mut() {
        for k in ["delta", "message", "logprobs"] {
            o.remove(k);
        }
    }
    c
}

impl EventParser for ChatParser {
    fn event(&mut self, _name: Option<&str>, data: &Value) -> Result<Vec<StreamEvent>, LlmError> {
        let evs = self.inner.chunk(data)?;
        let mut out = Vec::with_capacity(evs.len() + 1);
        for ev in evs {
            match ev {
                StreamEvent::Finish { reason: Some(r), .. }
                    if r == "insufficient_system_resource" || r == "aborted" =>
                {
                    let what = if r == "aborted" {
                        "the generation was interrupted"
                    } else {
                        "the inference system ran out of resources"
                    };
                    let mut e = LlmError::new(
                        ErrorKind::InBand,
                        Retry::Backoff,
                        format!("DeepSeek stopped the answer: {what} (finish_reason: {r})"),
                    );
                    e.error_type = Some(r.clone());
                    e.details = Some(data.clone());
                    e.partial_output = self.inner.any_output();
                    self.abnormal = Some(e);
                    out.push(StreamEvent::FinishDetails(finish_choice(data)));
                    out.push(StreamEvent::Finish { reason: Some("error".into()), native: Some(r) });
                }
                StreamEvent::Finish { reason, native: None } => {
                    out.push(StreamEvent::Finish { native: reason.clone(), reason })
                }
                other => out.push(other),
            }
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

/// Anthropic events from DeepSeek's Messages endpoint.
#[derive(Default)]
struct MessagesParser {
    inner: messages::EventParser,
}

/// A summary of `web_search_tool_result` content (titles and URLs, or the
/// error codes), and whether every item is an error.
fn search_results(block: &Value) -> (Option<Value>, bool) {
    let is_error = |r: &Value| r.get("type").and_then(Value::as_str) == Some("web_search_tool_result_error");
    match block.get("content") {
        Some(Value::Array(items)) => {
            let summary = items
                .iter()
                .map(|r| {
                    if is_error(r) {
                        json!({"error_code": r.get("error_code")})
                    } else {
                        let mut o = json!({"title": r.get("title"), "url": r.get("url")});
                        if let Some(a) = r.get("page_age").filter(|a| !a.is_null()) {
                            o["page_age"] = a.clone();
                        }
                        o
                    }
                })
                .collect();
            (Some(Value::Array(summary)), !items.is_empty() && items.iter().all(is_error))
        }
        Some(o) if is_error(o) => (Some(json!({"error_code": o.get("error_code")})), true),
        _ => (None, false),
    }
}

impl EventParser for MessagesParser {
    fn event(&mut self, name: Option<&str>, data: &Value) -> Result<Vec<StreamEvent>, LlmError> {
        let ty = data.get("type").and_then(Value::as_str).or(name);
        // Errors: an unnamed `{"error":{"message":…}}` frame (no event name, no
        // type) when the wait for generation times out, or an Anthropic `error` event.
        if ty == Some("error") || (ty.is_none() && data.get("error").is_some()) {
            let err = data.get("error").cloned().unwrap_or_else(|| data.clone());
            let mut e = error_from_json(&err, None, None, ErrorKind::InBand);
            e.details = Some(data.clone());
            e.partial_output = EventParser::any_output(&self.inner);
            return Err(e);
        }
        let mut evs = self.inner.event(name, data)?;
        if ty == Some("content_block_start") {
            let cb = data.get("content_block").unwrap_or(&Value::Null);
            match cb.get("type").and_then(Value::as_str) {
                // The query may come whole in the block start rather than as deltas.
                Some("server_tool_use") => {
                    if let Some(input) = cb.get("input").filter(|i| i.as_object().is_some_and(|o| !o.is_empty())) {
                        for ev in &mut evs {
                            if let StreamEvent::Tool(t) = ev {
                                t.input = Some(input.clone());
                            }
                        }
                    }
                }
                Some("web_search_tool_result") => {
                    let (summary, failed) = search_results(cb);
                    for ev in &mut evs {
                        if let StreamEvent::Tool(t) = ev {
                            t.output = summary.clone();
                            if failed {
                                t.status = Some("failed".into());
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        Ok(evs)
    }
    fn finished(&self) -> bool {
        self.inner.finished
    }
    fn any_output(&self) -> bool {
        EventParser::any_output(&self.inner)
    }
}

// ------------------------------------------------------------------ errors

/// Classify DeepSeek's errors (`{"error":{"message","type","param","code"}}`,
/// also on the Messages endpoint; docs/research/xai_deepseek.md §B9): 400
/// invalid request, 401 auth, 402 insufficient balance, 422 invalid parameters
/// are final; 429 (rate or concurrency limit), 500 and 503 are retried.
pub fn error_from_json(err: &Value, http_status: Option<u16>, retry_after: Option<u64>, kind: ErrorKind) -> LlmError {
    let message = err.get("message").and_then(Value::as_str).unwrap_or("(no message)").to_string();
    let lower = message.to_ascii_lowercase();
    let overflow = ["maximum context length", "context length", "context window"].iter().any(|k| lower.contains(k));
    let retry = match http_status {
        Some(429) => Retry::After(retry_after.unwrap_or(20)),
        Some(s) => retry_for_status(s, retry_after),
        // In-band: the documented case is the wait for generation timing out.
        None => retry_after.map_or(Retry::Backoff, Retry::After),
    };
    let error_type = if overflow {
        Some("context_length_exceeded".to_string())
    } else {
        err.get("type").or_else(|| err.get("code")).and_then(Value::as_str).map(str::to_string)
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

/// Estimated cost in USD: tokens at the peak or off-peak CNY price (cache hits,
/// misses, output incl. reasoning) plus successful searches.
pub fn estimate_cost(model: &str, u: &Usage, peak: bool, cny_per_usd: f64) -> Option<f64> {
    let s = model_spec(model)?;
    let price = if peak { s.peak } else { s.off_peak };
    let tokens = price.cost(u)?;
    let searches = u.raw.pointer("/server_tool_use/tool_search").and_then(Value::as_u64).unwrap_or(0);
    provider::usd(tokens + searches as f64 * SEARCH_CNY, Currency::Cny, cny_per_usd)
}

fn unix_now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// A calendar date: (year, month, day).
type Date = (i64, u32, u32);

/// The last year of [`CN_DAYS_OFF`].
pub const CN_HOLIDAYS_UNTIL: i64 = 2027;

/// Days off of China's public holidays (inclusive ranges), from the State
/// Council's arrangements for 2026 (published 2025-11-04) and 2027 (published
/// 2026-11-05). All days off count, not only the statutory ones; make-up
/// working days fall on weekends, which DeepSeek prices off-peak anyway.
const CN_DAYS_OFF: &[(Date, Date)] = &[
    ((2026, 1, 1), (2026, 1, 3)),
    ((2026, 2, 15), (2026, 2, 23)),
    ((2026, 4, 4), (2026, 4, 6)),
    ((2026, 5, 1), (2026, 5, 5)),
    ((2026, 6, 19), (2026, 6, 21)),
    ((2026, 9, 25), (2026, 9, 27)),
    ((2026, 10, 1), (2026, 10, 7)),
    ((2027, 1, 1), (2027, 1, 3)),
    ((2027, 2, 5), (2027, 2, 13)),
    ((2027, 4, 3), (2027, 4, 5)),
    ((2027, 5, 1), (2027, 5, 5)),
    ((2027, 6, 9), (2027, 6, 9)),
    ((2027, 9, 15), (2027, 9, 15)),
    ((2027, 10, 1), (2027, 10, 7)),
];

/// (year, month, day) of a day number counted from 1970-01-01 (proleptic Gregorian).
fn civil_from_days(z: i64) -> Date {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// Beijing time (UTC+8, no DST) of `unix`: ((year, month, day), weekday with
/// Monday = 0, minute of the day).
fn beijing(unix: i64) -> (Date, i64, i64) {
    let t = unix + 8 * 3600;
    let days = t.div_euclid(86_400);
    (civil_from_days(days), (days + 3).rem_euclid(7), t.rem_euclid(86_400) / 60)
}

/// Today's date in Beijing.
fn beijing_now() -> Date {
    beijing(unix_now()).0
}

/// DeepSeek's peak hours: Beijing time 9:00–12:00 and 14:00–18:00, Monday to
/// Friday, except Chinese public holidays (from the bot's calendar; days after
/// it count as working days).
pub fn is_peak(unix: i64) -> bool {
    let (date, weekday, minute) = beijing(unix);
    let day_off = CN_DAYS_OFF.iter().any(|(from, to)| *from <= date && date <= *to);
    weekday < 5 && !day_off && ((540..720).contains(&minute) || (840..1080).contains(&minute))
}

#[cfg(test)]
mod tests {
    use super::*;
    use omni_core::content::{NativeOutput, Origin, ToolCall};
    use omni_core::trigger::OptionToken;
    use omni_core::{BlobRef, Turn};

    struct Blobs(Vec<(BlobRef, Vec<u8>)>);
    impl BlobLoader for Blobs {
        fn load<'a>(&'a self, r: &'a BlobRef) -> crate::BoxFuture<'a, anyhow::Result<Vec<u8>>> {
            let found = self.0.iter().find(|(b, _)| b == r).map(|(_, d)| d.clone());
            Box::pin(async move { found.ok_or_else(|| anyhow::anyhow!("no blob")) })
        }
    }

    fn ds() -> DeepSeek {
        ds_at("https://api.deepseek.com")
    }

    fn ds_at(base_url: &str) -> DeepSeek {
        DeepSeek::new(DirectConfig {
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
        ds().prepare(&input(model, options, history), &[], &Blobs(vec![])).await
    }

    #[tokio::test]
    async fn chat_defaults_are_the_maximum() {
        let h = vec![Turn::user_text("hi")];
        let p = prepare("deepseek-v4-pro", &[], &h).await.unwrap();
        assert_eq!(p.dialect, DIALECT);
        assert_eq!(p.url, "https://api.deepseek.com/chat/completions");
        assert_eq!(p.body["thinking"], json!({"type": "enabled"}));
        assert_eq!(p.body["reasoning_effort"], "max");
        assert_eq!(p.body["max_tokens"], 393_216);
        assert_eq!(p.auto_max_tokens, Some(393_216));
        assert_eq!(p.body["stream"], true);
        assert!(p.body.get("stream_options").is_none(), "usage comes in the last chunk anyway");
        assert!(p.body.get("user_id").is_none(), "no user_id: it would split the KV cache per user");
        assert_eq!(p.body["messages"], json!([{"role": "user", "content": "hi"}]));
        for k in ["temperature", "top_p", "tools", "service_tier", "metadata"] {
            assert!(p.body.get(k).is_none(), "{k} must not be sent");
        }
        assert_eq!(p.settings, vec!["effort max"]);
        assert_eq!(p.applied[0].source, "default");
        assert_eq!(p.applied[0].patch, json!({"thinking": {"type": "enabled"}, "reasoning_effort": "max"}));
        assert_eq!(p.applied[1].patch, json!({"max_tokens": 393_216}));
        assert!(p.warnings.iter().all(|w| !w.contains("model table")));
    }

    #[tokio::test]
    async fn effort_levels_and_thinking_off() {
        let h = vec![Turn::user_text("hi")];
        let p = prepare("deepseek-flash", &[kv("e", "low")], &h).await.unwrap();
        assert_eq!(p.body["reasoning_effort"], "low");
        assert_eq!(p.settings, vec!["effort low"]);
        assert_eq!(p.applied[0].source, "option");
        let p = prepare("deepseek-flash", &[kv("e", "none")], &h).await.unwrap();
        assert_eq!(p.body["thinking"], json!({"type": "disabled"}));
        assert!(p.body.get("reasoning_effort").is_none());
        assert_eq!(p.settings, vec!["thinking off"]);
        // Aliases the API would remap silently are not offered.
        for e in ["xhigh", "medium", "minimal", "ultra"] {
            let err = prepare("deepseek-flash", &[kv("e", e)], &h).await.unwrap_err();
            assert!(err.message.contains("levels: max, high, low"), "{}", err.message);
        }
    }

    #[tokio::test]
    async fn presets_then_user_options() {
        let h = vec![Turn::user_text("hi")];
        let preset = Requested { effort: Some("low".into()), tools: vec![Tool::Search], ..Default::default() };
        let p = ds()
            .prepare(&input("deepseek-v4-pro", &[], &h), std::slice::from_ref(&preset), &Blobs(vec![]))
            .await
            .unwrap();
        assert_eq!((p.dialect, p.body["output_config"]["effort"].as_str()), (MESSAGES_DIALECT, Some("low")));
        assert_eq!((p.applied[0].source.as_str(), p.applied[2].source.as_str()), ("default", "default"));
        let p =
            ds().prepare(&input("deepseek-v4-pro", &[kv("e", "high")], &h), &[preset], &Blobs(vec![])).await.unwrap();
        assert_eq!(
            (p.body["output_config"]["effort"].as_str(), p.applied[0].source.as_str()),
            (Some("high"), "option")
        );
    }

    #[tokio::test]
    async fn unsupported_options_are_errors() {
        let h = vec![Turn::user_text("hi")];
        let cases: &[(&str, &[OptionToken], &str)] = &[
            ("deepseek-flash", &[flag("s")], "documented for deepseek-v4-pro only"),
            ("deepseek-v4-pro", &[flag("c")], "has no code execution"),
            ("deepseek-v4-pro", &[flag("u")], "has no URL fetching"),
            ("deepseek-v4-pro", &[flag("m")], "has no Google Maps"),
            ("deepseek-v4-pro", &[kv("p", "x")], "does not apply to DeepSeek"),
            ("deepseek-v4-pro", &[kv("tier", "flex")], "no service tiers"),
            ("deepseek-v4-pro", &[kv("budget", "1000")], "no reasoning token budget"),
            ("deepseek-v4-pro", &[kv("v", "low")], "no verbosity"),
            ("deepseek-v4-pro", &[flag("4")], "no image size"),
            ("deepseek-v4-pro", &[flag("frob")], "unknown option"),
        ];
        for (model, opts, want) in cases {
            let e = prepare(model, opts, &h).await.unwrap_err();
            assert!(e.message.contains(want), "{opts:?}: {}", e.message);
            assert_eq!(e.retry, Retry::Never);
        }
        // `tier=default` changes nothing.
        assert!(prepare("deepseek-v4-pro", &[kv("tier", "default")], &h).await.is_ok());
    }

    #[tokio::test]
    async fn search_goes_to_the_messages_endpoint() {
        let h = vec![
            Turn { role: Role::System, origin: Origin::Synthetic, parts: vec![Part::text("be brief")], native: None },
            Turn::user_text("news?"),
        ];
        let p = prepare("deepseek-v4-pro", &[flag("s")], &h).await.unwrap();
        assert_eq!(p.dialect, MESSAGES_DIALECT);
        assert_eq!(p.url, "https://api.deepseek.com/anthropic/v1/messages");
        assert_eq!(p.body["thinking"], json!({"type": "enabled"}));
        assert_eq!(p.body["output_config"], json!({"effort": "max"}));
        assert!(p.body.get("reasoning_effort").is_none());
        assert_eq!(p.body["tools"], json!([{"type": "web_search_20260209", "name": "web_search"}]));
        assert_eq!(p.body["max_tokens"], 393_216);
        assert!(p.body.get("metadata").is_none(), "no user_id: it would split the KV cache per user");
        assert!(p.body.get("user_id").is_none());
        assert_eq!(p.body["system"], json!([{"type": "text", "text": "be brief"}]));
        assert_eq!(p.body["messages"], json!([{"role": "user", "content": "news?"}]));
        assert_eq!(p.settings, vec!["effort max", "web search"]);
        let p = prepare("deepseek-v4-pro", &[flag("s"), kv("e", "none")], &h).await.unwrap();
        assert_eq!(p.body["thinking"], json!({"type": "disabled"}));
        assert!(p.body.get("output_config").is_none());
        // A system prompt later in the conversation cannot be expressed there.
        let mut late = h.clone();
        late.push(Turn::assistant_text("ok"));
        late.push(Turn { role: Role::System, origin: Origin::Synthetic, parts: vec![Part::text("x")], native: None });
        late.push(Turn::user_text("more"));
        let e = prepare("deepseek-v4-pro", &[flag("s")], &late).await.unwrap_err();
        assert!(e.message.contains("only at the start"), "{}", e.message);
        // ... while Chat Completions keeps it in place.
        let p = prepare("deepseek-v4-pro", &[], &late).await.unwrap();
        assert_eq!(p.body["messages"][3], json!({"role": "system", "content": "x"}));
    }

    fn native(dialect: &str, model: &str, payload: Value) -> Turn {
        Turn {
            role: Role::Assistant,
            origin: Origin::Live,
            parts: vec![Part::text("answer")],
            native: Some(NativeOutput { dialect: dialect.into(), model: model.into(), payload, context_tokens: None }),
        }
    }

    #[tokio::test]
    async fn native_replay_only_within_dialect_and_model() {
        let chat = json!({"role": "assistant", "content": "answer", "reasoning_content": "because"});
        let msgs = json!({"role": "assistant", "content": [
            {"type": "thinking", "thinking": "hmm", "signature": "sig"},
            {"type": "server_tool_use", "id": "srv_1", "name": "web_search", "input": {"query": "q"}, "caller": {"type": "direct"}},
            {"type": "web_search_tool_result", "tool_use_id": "srv_1", "content": [{"type": "web_search_result", "title": "T", "url": "https://t", "encrypted_content": "E"}]},
            {"type": "text", "text": "answer"}
        ]});
        let h = vec![
            Turn::user_text("q1"),
            native(DIALECT, "deepseek-v4-pro", chat.clone()),
            Turn::user_text("q2"),
            native(MESSAGES_DIALECT, "deepseek-v4-pro", msgs.clone()),
            Turn::user_text("q3"),
            native(DIALECT, "deepseek-flash", chat.clone()),
            Turn::user_text("q4"),
        ];
        let p = prepare("deepseek-v4-pro", &[], &h).await.unwrap();
        let m = &p.body["messages"];
        assert_eq!(m[1], chat, "same dialect and model: content + reasoning_content");
        assert_eq!(m[3], json!({"role": "assistant", "content": "answer"}), "other dialect: text");
        assert_eq!(m[5], json!({"role": "assistant", "content": "answer"}), "other model: text");
        let p = prepare("deepseek-v4-pro", &[flag("s")], &h).await.unwrap();
        let m = &p.body["messages"];
        assert_eq!(m[1], json!({"role": "assistant", "content": "answer"}));
        assert_eq!(m[3], msgs, "content blocks verbatim");
    }

    #[tokio::test]
    async fn media_per_model() {
        let png = b"\x89PNG\r\n".to_vec();
        let img = BlobRef::for_bytes(&png, "image/png");
        let txt = BlobRef::for_bytes(b"fn main() {}", "text/x-rust");
        let pdf = BlobRef::for_bytes(b"%PDF", "application/pdf");
        let bmp = BlobRef::for_bytes(b"BM", "image/bmp");
        let blobs = Blobs(vec![(img.clone(), png.clone()), (txt.clone(), b"fn main() {}".to_vec())]);
        let turn = |parts: Vec<Part>| vec![Turn { role: Role::User, origin: Origin::Live, parts, native: None }];
        let image = Part::Image { blob: img.clone(), filename: None, generated: false, dims: None };
        let h = turn(vec![Part::text("what?"), image.clone()]);
        let p = ds().prepare(&input("deepseek-flash", &[], &h), &[], &blobs).await.unwrap();
        assert_eq!(
            p.body["messages"][0]["content"],
            json!([{"type": "text", "text": "what?"}, {"type": "image_url", "image_url": {"url": provider::data_url("image/png", &png), "detail": "original"}}])
        );
        let e = ds().prepare(&input("deepseek-v4-pro", &[], &h), &[], &blobs).await.unwrap_err();
        assert!(e.message.contains("images only on deepseek-flash"), "{}", e.message);
        let h = turn(vec![Part::Image { blob: bmp, filename: None, generated: false, dims: None }]);
        let e = ds().prepare(&input("deepseek-flash", &[], &h), &[], &blobs).await.unwrap_err();
        assert!(e.message.contains("not image/bmp"), "{}", e.message);
        // Text files are inlined as their own part, with a note.
        let h = turn(vec![
            Part::text("review"),
            Part::File { blob: txt.clone(), filename: Some("main.rs".into()), generated: false },
        ]);
        let p = ds().prepare(&input("deepseek-v4-pro", &[], &h), &[], &blobs).await.unwrap();
        assert_eq!(p.body["messages"][0]["content"][1], json!({"type": "text", "text": "fn main() {}"}));
        assert_eq!(
            p.notes,
            vec!["📎 main.rs inlined as a separate text part by the bot (no native text-file input on this API)"]
        );
        let h = turn(vec![Part::File { blob: pdf, filename: Some("a.pdf".into()), generated: false }]);
        let e = ds().prepare(&input("deepseek-flash", &[], &h), &[], &blobs).await.unwrap_err();
        assert!(e.message.contains("does not take PDF input via DeepSeek (a.pdf)"), "{}", e.message);
        let h =
            turn(vec![Part::Location { latitude: 1.0, longitude: 2.0, accuracy_m: None, title: None, address: None }]);
        let e = ds().prepare(&input("deepseek-flash", &[], &h), &[], &blobs).await.unwrap_err();
        assert!(e.message.contains("does not take a location via DeepSeek"), "{}", e.message);
    }

    #[tokio::test]
    async fn budget_and_unknown_models() {
        // A long conversation leaves less room in the context.
        let h = vec![Turn::user_text("字".repeat(800_000))];
        let p = prepare("deepseek-flash", &[], &h).await.unwrap();
        let n = p.body["max_tokens"].as_u64().unwrap();
        assert!((1_024..160_000).contains(&n), "{n}");
        // The bot's override after a context overflow wins.
        let h = vec![Turn::user_text("hi")];
        let mut i = input("deepseek-flash", &[], &h);
        i.max_tokens_override = Some(5_000);
        let p = ds().prepare(&i, &[], &Blobs(vec![])).await.unwrap();
        assert_eq!(p.body["max_tokens"], 5_000);
        // Unknown ids are allowed, with a warning.
        let p = prepare("deepseek-v5", &[], &h).await.unwrap();
        assert_eq!(p.body["max_tokens"], 393_216);
        assert!(p.warnings.iter().any(|w| w.contains("not in the bot's DeepSeek model table")), "{:?}", p.warnings);
        assert!(!ds().knows("deepseek-v5") && ds().knows("deepseek-v4-flash-vision-exp"));
    }

    fn feed(p: &mut impl EventParser, chunks: &[Value]) -> Result<Vec<StreamEvent>, LlmError> {
        let mut all = Vec::new();
        for c in chunks {
            all.extend(p.event(None, c)?);
        }
        Ok(all)
    }

    /// The documented stream: reasoning first, then the answer; usage with the finish.
    #[test]
    fn chat_stream_with_reasoning_and_usage() {
        let c = |delta: Value, fr: Value, usage: Value| {
            json!({"id": "071f229d", "object": "chat.completion.chunk", "created": 1793084907, "model": "deepseek-flash",
                   "system_fingerprint": "fp", "choices": [{"index": 0, "delta": delta, "logprobs": null, "finish_reason": fr}], "usage": usage})
        };
        let chunks = [
            c(json!({"role": "assistant", "content": null, "reasoning_content": "Think"}), json!(null), json!(null)),
            c(json!({"reasoning_content": "ing."}), json!(null), json!(null)),
            c(json!({"content": "Hello"}), json!(null), json!(null)),
            c(
                json!({"content": ""}),
                json!("stop"),
                json!({"prompt_tokens": 10, "completion_tokens": 20, "total_tokens": 30,
                "prompt_tokens_details": {"cached_tokens": 4}, "prompt_cache_hit_tokens": 4, "prompt_cache_miss_tokens": 6,
                "completion_tokens_details": {"reasoning_tokens": 15}}),
            ),
        ];
        let mut p = ChatParser::new();
        let evs = feed(&mut p, &chunks).unwrap();
        let mut acc = crate::Accumulator::new();
        for e in evs.clone() {
            acc.apply(e);
        }
        assert!(p.finished());
        assert!(matches!(&acc.parts()[0], Part::Reasoning(r) if r.text.as_deref() == Some("Thinking.")));
        assert_eq!(acc.turn().answer_text(), "Hello");
        assert_eq!(acc.finish_reason.as_deref(), Some("stop"));
        assert_eq!(acc.native_finish_reason.as_deref(), Some("stop"));
        let u = acc.usage.clone().unwrap();
        assert_eq!(
            (u.input_tokens, u.cached_input_tokens, u.output_tokens, u.reasoning_tokens),
            (Some(10), Some(4), Some(20), Some(15))
        );
        assert!(p.abnormal.is_none());
        assert_eq!(
            chat_native(&p.inner),
            json!({"role": "assistant", "content": "Hello", "reasoning_content": "Thinking."})
        );
    }

    #[test]
    fn abnormal_finishes() {
        let chunk = |fr: &str| {
            json!({"id": "x", "choices": [{"index": 0, "delta": {"content": ""}, "finish_reason": fr}],
                   "usage": {"prompt_tokens": 5, "completion_tokens": 7}})
        };
        for fr in ["insufficient_system_resource", "aborted"] {
            let mut p = ChatParser::new();
            let mut evs =
                p.event(None, &json!({"id": "x", "choices": [{"index": 0, "delta": {"content": "par"}}]})).unwrap();
            evs.extend(p.event(None, &chunk(fr)).unwrap());
            // The usage of the interrupted answer still arrives…
            assert!(evs.iter().any(|e| matches!(e, StreamEvent::Usage(u) if u.output_tokens == Some(7))));
            assert!(evs.iter().any(
                |e| matches!(e, StreamEvent::Finish { reason: Some(r), native: Some(n) } if r == "error" && n == fr)
            ));
            assert!(evs.iter().any(|e| matches!(e, StreamEvent::FinishDetails(d) if d["finish_reason"] == fr)));
            // …and the call fails retryably, with the chunk verbatim.
            let e = p.abnormal.unwrap();
            assert_eq!((e.kind, e.retry, e.error_type.as_deref()), (ErrorKind::InBand, Retry::Backoff, Some(fr)));
            assert!(e.partial_output);
            assert_eq!(e.details.unwrap()["choices"][0]["finish_reason"], fr);
        }
        // A content-filter stop is a block (the bot does not retry it).
        let mut p = ChatParser::new();
        let evs = p.event(None, &chunk("content_filter")).unwrap();
        assert!(evs.iter().any(|e| matches!(e, StreamEvent::Finish { reason: Some(r), .. } if r == "content_filter")));
        assert!(evs.iter().any(|e| matches!(e, StreamEvent::FinishDetails(_))));
        assert!(p.abnormal.is_none());
        // `length` is a normal end.
        let mut p = ChatParser::new();
        let evs = p.event(None, &chunk("length")).unwrap();
        assert!(evs.iter().any(|e| matches!(e, StreamEvent::Finish { reason: Some(r), .. } if r == "length")));
    }

    #[test]
    fn errors_are_classified() {
        let e = chatstream::http_error(
            401,
            None,
            r#"{"error":{"message":"Authentication Fails, Your api key: ****alid is invalid","type":"authentication_error","param":null,"code":"invalid_request_error"}}"#,
            error_from_json,
        );
        assert_eq!(
            (e.retry, e.status, e.error_type.as_deref()),
            (Retry::Never, Some(401), Some("authentication_error"))
        );
        assert_eq!(e.details.unwrap()["error"]["type"], "authentication_error");
        // Recorded 2026-11-12 without a key: a plain-text body.
        let e = chatstream::http_error(401, None, "Authentication Fails (governor)", error_from_json);
        assert_eq!((e.retry, e.details), (Retry::Never, Some(json!("Authentication Fails (governor)"))));
        let body = |m: &str| format!(r#"{{"error":{{"message":"{m}","type":"x","param":null,"code":null}}}}"#);
        assert_eq!(
            chatstream::http_error(402, None, &body("Insufficient Balance"), error_from_json).retry,
            Retry::Never
        );
        assert_eq!(chatstream::http_error(422, None, &body("bad"), error_from_json).retry, Retry::Never);
        assert_eq!(
            chatstream::http_error(429, None, &body("Rate Limit Reached"), error_from_json).retry,
            Retry::After(20)
        );
        assert_eq!(chatstream::http_error(429, Some(45), &body("busy"), error_from_json).retry, Retry::After(45));
        assert_eq!(chatstream::http_error(500, None, &body("Server Error"), error_from_json).retry, Retry::Backoff);
        assert_eq!(
            chatstream::http_error(503, None, &body("Server Overloaded"), error_from_json).retry,
            Retry::Backoff
        );
        let e = chatstream::http_error(
            400,
            None,
            &body(
                "This model's maximum context length is 1048576 tokens. However, you requested 1100000 tokens (1000000 in the messages, 100000 in the completion).",
            ),
            error_from_json,
        );
        assert_eq!(e.error_type.as_deref(), Some("context_length_exceeded"));
        assert!(e.is_context_overflow());
        // In-band errors in a Chat stream go through the same mapper.
        let mut p = ChatParser::new();
        let e = p.event(None, &json!({"error": {"message": "timeout", "type": "server_error"}})).unwrap_err();
        assert_eq!((e.kind, e.retry), (ErrorKind::InBand, Retry::Backoff));
    }

    /// An Anthropic-format stream from DeepSeek with a server-side search,
    /// per the documented event schema.
    #[test]
    fn messages_stream_with_search() {
        let evs = [
            json!({"type": "message_start", "message": {"id": "e6ef6a63", "type": "message", "role": "assistant", "model": "deepseek-v4-pro", "content": [], "stop_reason": null, "stop_sequence": null,
                "usage": {"input_tokens": 2912, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 256, "output_tokens": 0, "service_tier": "standard"}}}),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "thinking", "thinking": "", "signature": ""}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": "Search it."}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "signature_delta", "signature": "sig"}}),
            json!({"type": "content_block_stop", "index": 0}),
            json!({"type": "ping"}),
            json!({"type": "content_block_start", "index": 1, "content_block": {"type": "server_tool_use", "id": "srv_1", "name": "web_search", "input": {"query": "python official site"}, "caller": {"type": "direct"}}}),
            json!({"type": "content_block_stop", "index": 1}),
            json!({"type": "content_block_start", "index": 2, "content_block": {"type": "web_search_tool_result", "tool_use_id": "srv_1", "content": [
                {"type": "web_search_result", "title": "Welcome to Python.org", "url": "https://www.python.org/", "encrypted_content": "ENC", "page_age": null},
                {"type": "web_search_tool_result_error", "error_code": "max_uses_exceeded"}]}}),
            json!({"type": "content_block_stop", "index": 2}),
            json!({"type": "content_block_start", "index": 3, "content_block": {"type": "text", "text": ""}}),
            json!({"type": "content_block_delta", "index": 3, "delta": {"type": "text_delta", "text": "https://www.python.org/"}}),
            json!({"type": "content_block_stop", "index": 3}),
            json!({"type": "message_delta", "delta": {"stop_reason": "end_turn", "stop_sequence": null},
                "usage": {"input_tokens": 2912, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 256, "output_tokens": 52, "service_tier": "standard",
                          "server_tool_use": {"web_search_requests": 2, "tool_search": 1}}}),
            json!({"type": "message_stop"}),
        ];
        let mut p = MessagesParser::default();
        let mut acc = crate::Accumulator::new();
        let mut live = None;
        for e in &evs {
            let name = e["type"].as_str().map(str::to_string);
            for x in p.event(name.as_deref(), e).unwrap() {
                if let StreamEvent::LiveUsage(u) = &x {
                    live = Some(u.clone());
                }
                acc.apply(x);
            }
        }
        assert!(p.finished());
        assert_eq!(live.unwrap().input_tokens, Some(3168));
        let parts = acc.parts();
        assert!(matches!(&parts[0], Part::Reasoning(r) if r.text.as_deref() == Some("Search it.")));
        let Part::ToolCall(ToolCall { name, status, input, output, .. }) = &parts[1] else { panic!("{parts:?}") };
        assert_eq!((name.as_str(), status.as_deref()), ("web_search", Some("completed")));
        assert_eq!(input.as_ref().unwrap()["query"], "python official site");
        assert_eq!(
            output.as_ref().unwrap(),
            &json!([{"title": "Welcome to Python.org", "url": "https://www.python.org/"}, {"error_code": "max_uses_exceeded"}])
        );
        assert_eq!(acc.turn().answer_text(), "https://www.python.org/");
        assert_eq!(acc.finish_reason.as_deref(), Some("stop"));
        let u = acc.usage.clone().unwrap();
        assert_eq!((u.input_tokens, u.cached_input_tokens, u.output_tokens), (Some(3168), Some(256), Some(52)));
        // 2912 misses × 9 + 256 hits × 0.3 + 52 × 27 (peak, CNY/1M) + 1 search × 0.005 CNY.
        let cny = (2912.0 * 9.0 + 256.0 * 0.3 + 52.0 * 27.0) / 1e6 + 0.005;
        assert!((estimate_cost("deepseek-v4-pro", &u, true, 7.0).unwrap() - cny / 7.0).abs() < 1e-12);
        let native = p.inner.native_payload();
        assert_eq!(native["content"][0], json!({"type": "thinking", "thinking": "Search it.", "signature": "sig"}));
        assert_eq!(native["content"][1]["input"]["query"], "python official site");
        assert_eq!(native["content"][2]["content"][0]["encrypted_content"], "ENC");
    }

    #[test]
    fn messages_in_band_errors_and_refusals() {
        // The unnamed frame sent when the wait for generation times out.
        let mut p = MessagesParser::default();
        let e = p.event(None, &json!({"error": {"message": "wait timeout"}})).unwrap_err();
        assert_eq!((e.kind, e.retry, e.message.as_str()), (ErrorKind::InBand, Retry::Backoff, "wait timeout"));
        assert_eq!(e.details.unwrap(), json!({"error": {"message": "wait timeout"}}));
        let mut p = MessagesParser::default();
        let evs = p
            .event(Some("message_delta"), &json!({"type": "message_delta", "delta": {"stop_reason": "refusal", "stop_sequence": null}, "usage": {"input_tokens": 1, "output_tokens": 0}}))
            .unwrap();
        assert!(evs.iter().any(|e| matches!(e, StreamEvent::Finish { reason: Some(r), native: Some(n) } if r == "content_filter" && n == "refusal")));
        let evs = p
            .event(Some("message_delta"), &json!({"type": "message_delta", "delta": {"stop_reason": "max_tokens", "stop_sequence": null}, "usage": {"input_tokens": 1, "output_tokens": 9}}))
            .unwrap();
        assert!(evs.iter().any(|e| matches!(e, StreamEvent::Finish { reason: Some(r), .. } if r == "length")));
    }

    #[test]
    fn off_peak_cost() {
        let u = Usage {
            input_tokens: Some(1_000_000),
            cached_input_tokens: Some(400_000),
            output_tokens: Some(100_000),
            ..Default::default()
        };
        // 0.6M × 1 + 0.4M × 0.02 + 0.1M × 4 CNY = 1.008 CNY
        assert!((estimate_cost("deepseek-flash", &u, false, 7.0).unwrap() - 1.008 / 7.0).abs() < 1e-12);
        assert!((estimate_cost("deepseek-v4-flash", &u, true, 7.0).unwrap() - 2.016 / 7.0).abs() < 1e-12);
        assert!(estimate_cost("deepseek-v5", &u, false, 7.0).is_none());
    }

    /// Unix time of a Beijing wall-clock time.
    fn bj(y: i64, m: u32, d: u32, h: i64, min: i64) -> i64 {
        // Days from 1970-01-01 (the inverse of `civil_from_days`).
        let (y, m, d) = (if m <= 2 { y - 1 } else { y }, i64::from(m), i64::from(d));
        let era = y.div_euclid(400);
        let yoe = y - era * 400;
        let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
        let days = era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468;
        days * 86_400 + h * 3600 + min * 60 - 8 * 3600
    }

    #[test]
    fn peak_hours() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(20_769), (2026, 11, 12));
        assert_eq!(beijing(bj(2026, 11, 12, 10, 0)).1, 3, "2026-11-12 is a Thursday");
        assert!(is_peak(bj(2026, 11, 12, 9, 0)));
        assert!(is_peak(bj(2026, 11, 12, 11, 59)));
        assert!(!is_peak(bj(2026, 11, 12, 12, 0)));
        assert!(!is_peak(bj(2026, 11, 12, 13, 59)));
        assert!(is_peak(bj(2026, 11, 12, 14, 0)));
        assert!(is_peak(bj(2026, 11, 12, 17, 59)));
        assert!(!is_peak(bj(2026, 11, 12, 18, 0)));
        assert!(!is_peak(bj(2026, 11, 12, 8, 59)));
        // UTC 01:00 is Beijing 09:00.
        assert_eq!(bj(2026, 11, 12, 9, 0) % 86_400, 3600);
        // Weekends, also make-up working days (Saturday 2026-02-14).
        assert!(!is_peak(bj(2026, 11, 14, 10, 0)));
        assert!(!is_peak(bj(2026, 2, 14, 10, 0)));
        // Public holidays on weekdays: Spring Festival, National Day, 2027's Dragon Boat day.
        assert!(!is_peak(bj(2026, 2, 16, 10, 0)));
        assert!(!is_peak(bj(2026, 2, 23, 15, 0)));
        assert!(is_peak(bj(2026, 2, 24, 15, 0)));
        assert!(!is_peak(bj(2026, 10, 7, 10, 0)));
        assert!(!is_peak(bj(2027, 6, 9, 10, 0)));
        assert!(is_peak(bj(2027, 6, 10, 10, 0)));
    }

    #[test]
    fn model_table() {
        assert_eq!(model_spec("deepseek-v4-pro").unwrap().serves, "DeepSeek-V4-Pro-0813");
        assert_eq!(model_spec("deepseek-flash").unwrap().serves, "DeepSeek-V4.1-Flash-1017");
        for legacy in ["deepseek-v4-flash", "deepseek-v4-flash-vision-exp"] {
            let s = model_spec(legacy).unwrap();
            assert_eq!((s.serves, s.vision, s.off_peak), (FLASH.serves, true, FLASH.off_peak));
        }
        assert!(model_spec("deepseek-chat").is_none(), "retired 2026-07-24");
        assert!(MODELS.iter().all(|m| m.peak.output == 2.0 * m.off_peak.output));
    }

    /// One HTTP exchange on a local port: returns the base URL and a handle
    /// yielding the request head and JSON body.
    async fn serve(status: u16, headers: &str, body: String) -> (String, tokio::task::JoinHandle<(String, Value)>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
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

    async fn run(d: &DeepSeek, p: &Prepared) -> (Result<Value, LlmError>, CallOutcome, Vec<StreamEvent>) {
        let mut evs = Vec::new();
        let (r, o) =
            d.stream(p, &mut |e: StreamEvent| evs.push(e), &crate::NullObserver, &CancellationToken::new()).await;
        (r, o, evs)
    }

    #[tokio::test]
    async fn chat_end_to_end() {
        let usage = json!({"prompt_tokens": 1000, "completion_tokens": 100, "total_tokens": 1100, "prompt_tokens_details": {"cached_tokens": 200},
                           "prompt_cache_hit_tokens": 200, "prompt_cache_miss_tokens": 800, "completion_tokens_details": {"reasoning_tokens": 60}});
        let chunks = [
            json!({"id": "c1", "model": "deepseek-flash", "choices": [{"index": 0, "delta": {"role": "assistant", "content": null, "reasoning_content": "Hm."}, "finish_reason": null}]}),
            json!({"id": "c1", "model": "deepseek-flash", "choices": [{"index": 0, "delta": {"content": "Hi!"}, "finish_reason": null}]}),
            json!({"id": "c1", "model": "deepseek-flash", "choices": [{"index": 0, "delta": {"content": ""}, "finish_reason": "stop"}], "usage": usage}),
        ];
        let mut body = ": keep-alive\n\n".to_string();
        for c in &chunks {
            body.push_str(&format!("data: {c}\n\n"));
        }
        body.push_str("data: [DONE]\n\n");
        let (base, server) = serve(200, "content-type: text/event-stream\r\nx-ds-trace-id: trace-1\r\n", body).await;
        let d = ds_at(&base);
        let h = vec![Turn::user_text("hi")];
        let p = d.prepare(&input("deepseek-flash", &[], &h), &[], &Blobs(vec![])).await.unwrap();
        let (r, outcome, evs) = run(&d, &p).await;
        let (head, sent) = server.await.unwrap();
        assert!(head.starts_with("POST /chat/completions HTTP/1.1"), "{head}");
        assert!(head.to_ascii_lowercase().contains("authorization: bearer k"), "{head}");
        assert_eq!(sent, p.body);
        assert_eq!(r.unwrap(), json!({"role": "assistant", "content": "Hi!", "reasoning_content": "Hm."}));
        assert_eq!(outcome.upstream_id.as_deref(), Some("trace-1"));
        let u = evs.iter().find_map(|e| if let StreamEvent::Usage(u) = e { Some(u.clone()) } else { None }).unwrap();
        assert_eq!(u.cost_source, omni_core::usage::CostSource::Estimated);
        // 800 misses, 200 hits, 100 output: off-peak or peak, by the time of the test.
        let off = (800.0 * 1.0 + 200.0 * 0.02 + 100.0 * 4.0) / 1e6 / 7.0;
        let cost = u.cost_usd.unwrap();
        assert!((cost - off).abs() < 1e-12 || (cost - 2.0 * off).abs() < 1e-12, "{cost}");
    }

    #[tokio::test]
    async fn chat_end_to_end_failures() {
        // An interrupted answer: a retryable error, after the partial output.
        let body = format!(
            "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
            json!({"id": "c2", "choices": [{"index": 0, "delta": {"content": "Par"}, "finish_reason": null}]}),
            json!({"id": "c2", "choices": [{"index": 0, "delta": {"content": ""}, "finish_reason": "insufficient_system_resource"}],
                   "usage": {"prompt_tokens": 3, "completion_tokens": 1}})
        );
        let (base, server) = serve(200, "content-type: text/event-stream\r\n", body).await;
        let d = ds_at(&base);
        let h = vec![Turn::user_text("hi")];
        let p = d.prepare(&input("deepseek-v4-pro", &[], &h), &[], &Blobs(vec![])).await.unwrap();
        let (r, _, evs) = run(&d, &p).await;
        server.await.unwrap();
        let e = r.unwrap_err();
        assert_eq!(
            (e.retry, e.error_type.as_deref(), e.partial_output),
            (Retry::Backoff, Some("insufficient_system_resource"), true)
        );
        assert!(evs.iter().any(|e| matches!(e, StreamEvent::Text(t) if t == "Par")));
        assert!(evs.iter().any(|e| matches!(e, StreamEvent::Usage(u) if u.cost_usd.is_some())));
        // Insufficient balance: final, the body verbatim.
        let err = r#"{"error":{"message":"Insufficient Balance","type":"unknown_error","param":null,"code":"invalid_request_error"}}"#;
        let (base, server) = serve(402, "content-type: application/json\r\n", err.to_string()).await;
        let d = ds_at(&base);
        let p = d.prepare(&input("deepseek-v4-pro", &[], &h), &[], &Blobs(vec![])).await.unwrap();
        let (r, _, _) = run(&d, &p).await;
        server.await.unwrap();
        let e = r.unwrap_err();
        assert_eq!((e.kind, e.status, e.retry), (ErrorKind::Http, Some(402), Retry::Never));
        assert_eq!(e.details.unwrap(), serde_json::from_str::<Value>(err).unwrap());
        // A stream cut before its end: a retryable network error.
        let body = format!("data: {}\n\n", json!({"id": "c3", "choices": [{"index": 0, "delta": {"content": "x"}}]}));
        let (base, server) = serve(200, "content-type: text/event-stream\r\n", body).await;
        let d = ds_at(&base);
        let p = d.prepare(&input("deepseek-v4-pro", &[], &h), &[], &Blobs(vec![])).await.unwrap();
        let (r, _, _) = run(&d, &p).await;
        server.await.unwrap();
        let e = r.unwrap_err();
        assert_eq!((e.kind, e.retry), (ErrorKind::Network, Retry::Backoff));
    }

    #[tokio::test]
    async fn messages_end_to_end() {
        let events = [
            json!({"type": "message_start", "message": {"id": "m1", "type": "message", "role": "assistant", "model": "deepseek-v4-pro", "content": [],
                "usage": {"input_tokens": 10, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0, "output_tokens": 0, "service_tier": "standard"}}}),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "Done."}}),
            json!({"type": "content_block_stop", "index": 0}),
            json!({"type": "message_delta", "delta": {"stop_reason": "end_turn", "stop_sequence": null},
                "usage": {"input_tokens": 10, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0, "output_tokens": 2, "service_tier": "standard"}}),
            json!({"type": "message_stop"}),
        ];
        let body: String =
            events.iter().map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap())).collect();
        let (base, server) = serve(200, "content-type: text/event-stream\r\nx-ds-trace-id: trace-2\r\n", body).await;
        let d = ds_at(&base);
        let h = vec![Turn::user_text("search")];
        let p = d.prepare(&input("deepseek-v4-pro", &[flag("s")], &h), &[], &Blobs(vec![])).await.unwrap();
        let (r, outcome, evs) = run(&d, &p).await;
        let (head, sent) = server.await.unwrap();
        assert!(head.starts_with("POST /anthropic/v1/messages HTTP/1.1"), "{head}");
        assert!(head.to_ascii_lowercase().contains("authorization: bearer k"), "{head}");
        assert_eq!(sent["tools"][0]["name"], "web_search");
        assert_eq!(r.unwrap(), json!({"role": "assistant", "content": [{"type": "text", "text": "Done."}]}));
        assert_eq!(outcome.upstream_id.as_deref(), Some("trace-2"));
        assert!(evs.iter().any(|e| matches!(e, StreamEvent::LiveUsage(u) if u.input_tokens == Some(10))));
        assert!(
            evs.iter()
                .any(|e| matches!(e, StreamEvent::Usage(u) if u.output_tokens == Some(2) && u.cost_usd.is_some()))
        );
    }
}
