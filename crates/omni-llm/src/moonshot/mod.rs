//! Moonshot's own API: the Kimi open platform, China site
//! (`https://api.moonshot.cn/v1`, as in the old bot). Spec:
//! docs/research/glm_kimi_qwen.md §2, checked against platform.kimi.com/docs
//! on 2026-11-12.
//!
//! - **Chat Completions** (`moonshot.chat`) for every request without a tool.
//!   The K3 series (`kimi-k3.1-pro`, `kimi-k3.1-pro-highspeed`, `kimi-k3`)
//!   always thinks; `reasoning_effort` low | high | max (default max, sent
//!   explicitly). `kimi-k2.6` switches thinking with `thinking.type`;
//!   `kimi-k2.7-code` always thinks and takes no thinking parameter. Sampling
//!   parameters are fixed by Moonshot (other values are errors): never sent.
//! - **Web search** (`+s`, K3 series only): the Responses API
//!   (`moonshot.responses`, see [`responses`]) with its server-side
//!   `web_search` tool and its sources (`include`); they become the answer's
//!   sources. Moonshot decides from the input whether to search: an answer
//!   without a search says so. Moonshot's Responses API keeps no state
//!   (`store`, `previous_response_id` are fixed off).
//! - The output budget is `max_completion_tokens` (`max_tokens` is
//!   deprecated); Moonshot lowers it itself when prompt + budget exceed the
//!   window, so it never causes a context overflow.
//! - `reasoning_content` is replayed verbatim for the same model (required
//!   on K3: Preserved Thinking is always on). Other assistant turns go as their
//!   text: media other models generated there cannot go along, which is an
//!   error (Moonshot describes its image and video parts as "用于多模态输入",
//!   for input).
//! - Only `data: [DONE]` ends a Chat stream ("do not use finish_reason",
//!   Moonshot's streaming guide).
//! - Images and videos go as base64 data URLs (public URLs are rejected by
//!   Moonshot). PDFs and documents need the stateful `file-extract` upload,
//!   which the bot does not use (D28): rejected. Text files are inlined (D6).
//! - Errors: `{"error":{"type","message"}}`; `content_filter` (HTTP 400) may be
//!   triggered by the input or by the output.
//! - No cost in responses: estimated from the CNY price list.

mod responses;

use omni_core::Usage;
use omni_core::content::{Part, Role, Turn};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::error::{ErrorKind, LlmError, Retry, retry_for_status};
use crate::event::StreamEvent;
use crate::openrouter::chat::ChunkParser;
use crate::openrouter::options::{Applied, overlay};
use crate::provider::{Currency, DirectConfig, PricingSink, Provider, TokenPrice, usd};
use crate::request::{Requested, RouteCaps, Tool};
use crate::{BlobLoader, CallInput, CallOutcome, EventSink, MediaClass, Observer, Prepared, classify_mime};

pub const DIALECT: &str = "moonshot.chat";
/// The Responses API, used for web search (K3 series).
pub const RESPONSES_DIALECT: &str = "moonshot.responses";
const NAME: &str = "Moonshot";
/// Response header with Moonshot's request id.
const REQUEST_ID_HEADER: &str = "msh-request-id";
/// One executed Responses-API web search (`web_search_call` in the output), CNY.
/// platform.kimi.com/docs/pricing/websearch, 2026-11-12.
pub const WEB_SEARCH_CNY: f64 = 0.03;
/// A `+s` answer without a `web_search_call`: Moonshot judged that the input
/// needs no search (platform.kimi.com/docs/api/responses, 2026-11-13).
const NO_SEARCH: &str = "no web search ran (Moonshot decides from the input whether to search)";

/// How a model thinks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Thinking {
    /// Always on; `reasoning_effort` low | high | max, default max (K3 series).
    Effort,
    /// Always on (with Preserved Thinking); no switch, no levels (`kimi-k2.7-code`).
    Always,
    /// `thinking.type` enabled (default) | disabled (`kimi-k2.6`).
    Switch,
}

/// Known limits and list prices of a Kimi model.
#[derive(Debug, Clone, Copy)]
pub struct ModelSpec {
    pub id: &'static str,
    pub context: u64,
    /// Largest `max_completion_tokens` / `max_output_tokens`.
    pub max_output: u64,
    pub thinking: Thinking,
    /// On the Responses API, with its `web_search` tool (the K3 series).
    pub responses: bool,
    /// CNY per 1M tokens; cache writes at the default 5-minute TTL (the bot never asks for 1 h).
    pub price: TokenPrice,
}

const fn cny(input: f64, cached_input: f64, cache_write: f64, output: f64) -> TokenPrice {
    TokenPrice { input, cached_input, cache_write, output }
}

const K3_WINDOW: u64 = 1_048_576;

/// Kimi models of the China site: platform.kimi.com/docs `models`, `pricing/chat`,
/// `api/models-overview`, `api/chat` (fetched 2026-11-12).
/// - K3 series: 1,048,576-token window; `max_completion_tokens` default 131,072,
///   up to 1,048,576. Cache writes are billed: 5 min = the input price (1 h =
///   2×, never requested by the bot).
pub const MODELS: &[ModelSpec] = &[
    ModelSpec {
        id: "kimi-k3.1-pro",
        context: K3_WINDOW,
        max_output: K3_WINDOW,
        thinking: Thinking::Effort,
        responses: true,
        price: cny(13.0, 1.3, 13.0, 68.0),
    },
    ModelSpec {
        id: "kimi-k3.1-pro-highspeed",
        context: K3_WINDOW,
        max_output: K3_WINDOW,
        thinking: Thinking::Effort,
        responses: true,
        price: cny(26.0, 1.3, 26.0, 136.0),
    },
    ModelSpec {
        id: "kimi-k3",
        context: K3_WINDOW,
        max_output: K3_WINDOW,
        thinking: Thinking::Effort,
        responses: true,
        price: cny(20.0, 2.0, 20.0, 100.0),
    },
];

pub fn model_spec(model: &str) -> Option<&'static ModelSpec> {
    MODELS.iter().find(|m| m.id == model)
}

/// K3 effort levels, highest first.
const EFFORTS: [&str; 3] = ["max", "high", "low"];

/// What Moonshot's API offers for a model (the vendor's own API: official = route).
struct Caps<'a>(&'a ModelSpec);

impl RouteCaps for Caps<'_> {
    fn name(&self) -> &str {
        NAME
    }
    fn tool(&self, tool: Tool) -> bool {
        tool == Tool::Search && self.0.responses
    }
    fn efforts(&self) -> Option<Vec<String>> {
        (self.0.thinking == Thinking::Effort).then(|| EFFORTS.map(String::from).to_vec())
    }
    fn can_disable_reasoning(&self) -> bool {
        self.0.thinking == Thinking::Switch
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

/// Checks with more precise messages than the generic validation.
fn check(spec: &ModelSpec, req: &Requested) -> Result<(), LlmError> {
    if req.tools.contains(&Tool::Search) && !spec.responses {
        return Err(LlmError::request(format!(
            "`{}` has no web search (`+s`) on Moonshot's API: its built-in search is the Responses API \
             `web_search` tool, which only the K3 series (kimi-k3.1-pro, kimi-k3) has",
            spec.id
        )));
    }
    if req.budget.is_some() {
        return Err(LlmError::request(format!(
            "`{}` has no reasoning token budget on Moonshot's API{}",
            spec.id,
            if spec.thinking == Thinking::Effort { " (use e=max|high|low)" } else { "" }
        )));
    }
    Ok(())
}

/// The reasoning setting for a known model: body fields, footer text, source.
struct ReasoningSetting {
    /// `reasoning_effort` (Chat) / `reasoning.effort` (Responses) for the K3 series.
    effort: Option<String>,
    /// `thinking` object (kimi-k2.6).
    thinking: Option<Value>,
    described: String,
    by_user: bool,
}

fn reasoning(spec: &ModelSpec, req: &Requested, user: &Requested) -> ReasoningSetting {
    let by_user = user.effort.is_some();
    match spec.thinking {
        Thinking::Effort => {
            let e = req.effort.clone().unwrap_or_else(|| EFFORTS[0].to_string());
            ReasoningSetting { described: format!("effort {e}"), effort: Some(e), thinking: None, by_user }
        }
        Thinking::Switch => {
            let off = req.effort.as_deref() == Some("none");
            ReasoningSetting {
                effort: None,
                thinking: Some(json!({"type": if off { "disabled" } else { "enabled" }})),
                described: if off { "thinking off" } else { "thinking on" }.into(),
                by_user,
            }
        }
        Thinking::Always => {
            ReasoningSetting { effort: None, thinking: None, described: "thinking (always on)".into(), by_user: false }
        }
    }
}

/// Whether `req` asks for anything (`tier=default|auto` changes nothing).
fn has_options(req: &Requested) -> bool {
    let mut r = req.clone();
    if matches!(r.tier.as_deref(), Some("default" | "auto")) {
        r.tier = None;
    }
    r != Requested::default()
}

pub struct Moonshot {
    cfg: DirectConfig,
    http: reqwest::Client,
}

impl Moonshot {
    pub fn new(cfg: DirectConfig) -> anyhow::Result<Self> {
        let http = cfg.http()?;
        Ok(Self { cfg, http })
    }

    fn output_budget(&self, input: &CallInput<'_>, spec: Option<&ModelSpec>) -> Option<u64> {
        if let Some(n) = input.max_tokens_override {
            return Some(n);
        }
        let prompt = crate::budget::estimate_prompt(input.history, input.known_context);
        match spec {
            Some(s) => crate::budget::choose(Some(s.context), Some(s.max_output), self.cfg.max_tokens_cap, prompt),
            // Unknown model: Moonshot's own default applies unless a cap is configured.
            None => self.cfg.max_tokens_cap,
        }
    }

    async fn prepare_request(
        &self,
        input: &CallInput<'_>,
        presets: &[Requested],
        blobs: &dyn BlobLoader,
    ) -> Result<Prepared, LlmError> {
        let user = crate::request::parse(input.options).map_err(LlmError::request)?;
        let mut req = Requested::default();
        for p in presets {
            req = overlay(&req, p);
        }
        let req = overlay(&req, &user);
        if req.providers.is_some() {
            return Err(LlmError::request("`p=` (provider choice) does not apply to Moonshot"));
        }
        let spec = model_spec(input.model);
        match spec {
            Some(s) => {
                check(s, &req)?;
                crate::request::validate(input.model, &req, &Caps(s)).map_err(LlmError::request)?;
            }
            None if has_options(&req) => {
                return Err(LlmError::request(format!(
                    "`{}` is not in the bot's Moonshot model table, so its options cannot be checked: \
                     send it without options",
                    input.model
                )));
            }
            None => {}
        }
        let search = req.tools.contains(&Tool::Search);
        let notes = Vec::new();
        let mut applied = Vec::new();
        let mut settings = Vec::new();
        let auto_max = self.output_budget(input, spec);
        let (dialect, url, mut body) = if search {
            let items = responses::input_items(input.history, input.model, blobs).await?;
            let mut body = json!({"model": input.model, "input": items, "stream": true});
            if let Some(n) = auto_max {
                body["max_output_tokens"] = json!(n);
            }
            (RESPONSES_DIALECT, self.cfg.url("/responses"), body)
        } else {
            let messages = chat_messages(input.history, input.model, blobs).await?;
            let mut body = json!({
                "model": input.model,
                "messages": messages,
                "stream": true,
                "stream_options": {"include_usage": true},
            });
            if let Some(n) = auto_max {
                body["max_completion_tokens"] = json!(n);
            }
            (DIALECT, self.cfg.url("/chat/completions"), body)
        };
        if let Some(s) = spec {
            let r = reasoning(s, &req, &user);
            let mut patch = serde_json::Map::new();
            if let Some(e) = &r.effort {
                if search {
                    body["reasoning"] = json!({"effort": e});
                    patch.insert("reasoning".into(), body["reasoning"].clone());
                } else {
                    body["reasoning_effort"] = json!(e);
                    patch.insert("reasoning_effort".into(), json!(e));
                }
            }
            if let Some(t) = &r.thinking {
                body["thinking"] = t.clone();
                patch.insert("thinking".into(), t.clone());
            }
            if !patch.is_empty() {
                applied.push(Applied {
                    source: if r.by_user { "option" } else { "default" }.into(),
                    patch: Value::Object(patch),
                });
            }
            settings.push(r.described);
        }
        if let Some(n) = auto_max {
            let key = if search { "max_output_tokens" } else { "max_completion_tokens" };
            let mut patch = serde_json::Map::new();
            patch.insert(key.into(), json!(n));
            applied.push(Applied { source: "budget".into(), patch: Value::Object(patch) });
        }
        if search {
            body["tools"] = json!([{"type": "web_search"}]);
            // The sources of each search, shown with the search call.
            body["include"] = json!(["web_search_call.action.sources"]);
            applied.push(Applied {
                source: if user.tools.contains(&Tool::Search) { "option" } else { "default" }.into(),
                patch: json!({"tools": body["tools"], "include": body["include"]}),
            });
            settings.push("web search".into());
        }
        // Moonshot's documented identity / cache-routing fields.
        if let Some(k) = &input.session_id {
            body["prompt_cache_key"] = json!(k);
            applied.push(Applied { source: "default".into(), patch: json!({"prompt_cache_key": k}) });
        }
        if let Some(u) = &input.user {
            body["safety_identifier"] = json!(u);
            applied.push(Applied { source: "default".into(), patch: json!({"safety_identifier": u}) });
        }
        let mut warnings = Vec::new();
        // This model's own answers are replayed (text and reasoning); other turns go as their text.
        let own = |t: &Turn| {
            t.native
                .as_ref()
                .is_some_and(|n| n.model == input.model && [DIALECT, RESPONSES_DIALECT].contains(&n.dialect.as_str()))
        };
        crate::provider::unsent_media(input.history, own)?;
        if spec.is_none() {
            warnings.push(format!(
                "`{}` is not in the bot's Moonshot model table: limits and price unknown (sent without reasoning \
                 settings; max_completion_tokens {}; cost not estimated)",
                input.model,
                auto_max.map(|n| n.to_string()).unwrap_or_else(|| "not set".into())
            ));
        }
        Ok(Prepared {
            dialect,
            model: input.model.to_string(),
            url,
            body,
            applied,
            warnings,
            notes,
            show_debug: false,
            auto_max_tokens: auto_max,
            long_decode: false,
            settings,
        })
    }

    async fn stream_request(
        &self,
        p: &Prepared,
        sink: &mut dyn EventSink,
        observer: &dyn Observer,
        cancel: &CancellationToken,
    ) -> (Result<Value, LlmError>, CallOutcome) {
        let rb = self.http.post(&p.url).bearer_auth(&self.cfg.api_key).json(&p.body);
        let spec = model_spec(&p.model);
        let rate = self.cfg.cny_per_usd;
        if p.dialect == RESPONSES_DIALECT {
            let parser = responses::Parser::new(spec, rate);
            let crate::chatstream::Streamed { result, parser, header_id } = crate::chatstream::run_with(
                rb,
                &p.url,
                &p.body,
                self.cfg.idle_timeout,
                Some(REQUEST_ID_HEADER),
                error_from_json,
                parser,
                sink,
                observer,
                cancel,
            )
            .await;
            let outcome = CallOutcome { upstream_id: header_id, ..Default::default() };
            return match result {
                Err(e) => (Err(e), outcome),
                Ok(()) => {
                    let asked = p.body["tools"].as_array().is_some_and(|t| t.iter().any(|t| t["type"] == "web_search"));
                    if asked && parser.answered_without_search() {
                        sink.event(StreamEvent::Notice(NO_SEARCH.into()));
                    }
                    (Ok(parser.native_payload()), outcome)
                }
            };
        }
        let estimate = move |u: &Usage| estimate_cost(spec?, u, 0, rate);
        let mut priced = PricingSink { inner: sink, estimate: &estimate };
        let crate::chatstream::Streamed { result, parser, header_id } = crate::chatstream::run_with(
            rb,
            &p.url,
            &p.body,
            self.cfg.idle_timeout,
            Some(REQUEST_ID_HEADER),
            error_from_json,
            ChatParser::default(),
            &mut priced,
            observer,
            cancel,
        )
        .await;
        let outcome = CallOutcome { upstream_id: header_id, ..Default::default() };
        match result {
            Err(e) => (Err(e), outcome),
            Ok(()) => (Ok(parser.native_payload()), outcome),
        }
    }
}

#[async_trait::async_trait]
impl Provider for Moonshot {
    fn id(&self) -> &'static str {
        "moonshot"
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
        if !self.knows(input.model) {
            return Err(crate::provider::not_in_table(self.name(), input.model));
        }
        self.prepare_request(input, presets, blobs).await
    }
    async fn stream(
        &self,
        p: &Prepared,
        sink: &mut dyn EventSink,
        observer: &dyn Observer,
        cancel: &CancellationToken,
    ) -> (Result<Value, LlmError>, CallOutcome) {
        self.stream_request(p, sink, observer, cancel).await
    }
    /// `GET /v1/users/me/balance` (CNY on the China site), verbatim.
    async fn balance(&self) -> Option<anyhow::Result<Value>> {
        Some(self.fetch_balance().await)
    }
}

impl Moonshot {
    async fn fetch_balance(&self) -> anyhow::Result<Value> {
        let resp = self
            .http
            .get(self.cfg.url("/users/me/balance"))
            .bearer_auth(&self.cfg.api_key)
            .timeout(std::time::Duration::from_secs(30))
            .send()
            .await?;
        let status = resp.status();
        let text = resp.text().await?;
        anyhow::ensure!(status.is_success(), "HTTP {}: {text}", status.as_u16());
        Ok(serde_json::from_str(&text)?)
    }
}

// ------------------------------------------------------------------ history

/// User content, independent of the API surface.
enum Content {
    Text(String),
    /// A data URL.
    Image(String),
    /// A data URL.
    Video(String),
}

/// The user parts Moonshot takes (images and videos as base64 data URLs).
async fn user_content(turn: &Turn, model: &str, blobs: &dyn BlobLoader) -> Result<Vec<Content>, LlmError> {
    let mut out = Vec::new();
    for part in &turn.parts {
        match part {
            Part::Text { text, .. } => {
                if !text.is_empty() {
                    out.push(Content::Text(text.clone()));
                }
            }
            Part::Image { blob, filename, .. } => out.push(image(model, blob, filename.as_deref(), blobs).await?),
            Part::File { blob, filename, .. } => {
                let name = filename.as_deref().unwrap_or("file");
                match classify_mime(&blob.mime, filename.as_deref()) {
                    MediaClass::Image => out.push(image(model, blob, filename.as_deref(), blobs).await?),
                    MediaClass::Video => {
                        let bytes = crate::provider::load(blobs, blob).await?;
                        out.push(Content::Video(crate::provider::data_url(&blob.mime, &bytes)));
                    }
                    MediaClass::Text => {
                        let bytes = crate::provider::load(blobs, blob).await?;
                        let text = String::from_utf8(bytes)
                            .map_err(|_| LlmError::request(format!("{name} is not valid UTF-8 text")))?;
                        out.push(Content::Text(text));
                    }
                    MediaClass::Pdf | MediaClass::Document => {
                        return Err(LlmError::request(format!(
                            "`{model}` takes no {} ({name}) on Moonshot's API: documents only go through the \
                             `file-extract` upload, a stored server-side file the bot does not use",
                            blob.mime
                        )));
                    }
                    MediaClass::Audio => {
                        return Err(LlmError::request(format!(
                            "`{model}` takes no audio input on Moonshot's API ({name})"
                        )));
                    }
                    MediaClass::Other => {
                        return Err(LlmError::request(format!(
                            "`{model}` takes no {} input on Moonshot's API ({name})",
                            blob.mime
                        )));
                    }
                }
            }
            Part::RemoteMedia { url, .. } => {
                return Err(LlmError::request(format!(
                    "`{model}` cannot read remote media on Moonshot's API ({url}): only uploaded images and videos"
                )));
            }
            Part::Location { .. } => return Err(crate::provider::no_location(model, NAME)),
            // Reasoning/tool parts never appear in user turns.
            Part::Reasoning(_) | Part::ToolCall(_) | Part::Refusal { .. } => {}
        }
    }
    Ok(out)
}

async fn image(
    model: &str,
    blob: &omni_core::BlobRef,
    filename: Option<&str>,
    blobs: &dyn BlobLoader,
) -> Result<Content, LlmError> {
    if blob.mime.eq_ignore_ascii_case("image/svg+xml") {
        return Err(LlmError::request(format!(
            "`{model}`: Moonshot rejects SVG images ({}); send the SVG source as a text file instead",
            filename.unwrap_or("image")
        )));
    }
    let bytes = crate::provider::load(blobs, blob).await?;
    Ok(Content::Image(crate::provider::data_url(&blob.mime, &bytes)))
}

/// The conversation as Chat Completions `messages`.
async fn chat_messages(history: &[Turn], model: &str, blobs: &dyn BlobLoader) -> Result<Vec<Value>, LlmError> {
    let mut out = Vec::with_capacity(history.len());
    for turn in history {
        match turn.role {
            Role::System => out.push(json!({"role": "system", "content": turn.answer_text()})),
            Role::Assistant => out.push(chat_assistant(turn, model)),
            Role::User => {
                let content = user_content(turn, model, blobs).await?;
                let parts: Vec<Value> = content
                    .into_iter()
                    .map(|c| match c {
                        Content::Text(t) => json!({"type": "text", "text": t}),
                        Content::Image(u) => json!({"type": "image_url", "image_url": {"url": u}}),
                        Content::Video(u) => json!({"type": "video_url", "video_url": {"url": u}}),
                    })
                    .collect();
                // A single text part as a plain string (most compatible).
                if parts.len() == 1 && parts[0]["type"] == "text" {
                    out.push(json!({"role": "user", "content": parts[0]["text"].clone()}));
                } else {
                    out.push(json!({"role": "user", "content": parts}));
                }
            }
        }
    }
    Ok(out)
}

/// An assistant turn for Chat Completions: the model's own message replayed
/// verbatim (with `reasoning_content`) when it is this model's, else its text.
fn chat_assistant(turn: &Turn, model: &str) -> Value {
    if let Some(n) = &turn.native
        && n.model == model
    {
        if n.dialect == DIALECT {
            return n.payload.clone();
        }
        if n.dialect == RESPONSES_DIALECT {
            return responses::to_chat_message(&n.payload);
        }
    }
    json!({"role": "assistant", "content": turn.answer_text()})
}

// ------------------------------------------------------------------ stream

/// Chat Completions chunks: the shared parser plus Moonshot's specifics.
#[derive(Debug)]
struct ChatParser {
    inner: ChunkParser,
}

impl Default for ChatParser {
    fn default() -> Self {
        let mut inner = ChunkParser::default();
        inner.map_error = Some(error_from_json);
        Self { inner }
    }
}

impl ChatParser {
    /// The assistant message as Moonshot returned it (`content`, `reasoning_content`).
    fn native_payload(&self) -> Value {
        let n = self.inner.native_payload();
        let mut m = json!({"role": "assistant", "content": n.get("content").cloned().unwrap_or(json!(""))});
        if let Some(r) = n.get("reasoning").and_then(Value::as_str).filter(|s| !s.is_empty()) {
            m["reasoning_content"] = json!(r);
        }
        m
    }
}

impl crate::chatstream::EventParser for ChatParser {
    fn event(&mut self, _name: Option<&str>, v: &Value) -> Result<Vec<StreamEvent>, LlmError> {
        let mut evs = self.inner.chunk(v)?;
        for ev in &mut evs {
            if let StreamEvent::Usage(u) = ev {
                fill_cached(u);
            }
        }
        // The finish chunk may carry the usage inside the choice (streaming guide);
        // the final `choices: []` chunk's top-level usage replaces it.
        if !v.get("usage").is_some_and(Value::is_object)
            && let Some(u) = v.pointer("/choices/0/usage").filter(|u| u.is_object())
        {
            let mut u = crate::openrouter::chat::usage_from_json(u);
            fill_cached(&mut u);
            evs.push(StreamEvent::Usage(u));
        }
        Ok(evs)
    }
    /// Never: only `data: [DONE]` completes a Moonshot stream (chatstream sees it),
    /// a finish reason alone does not.
    fn finished(&self) -> bool {
        false
    }
    fn any_output(&self) -> bool {
        self.inner.any_output()
    }
}

/// Moonshot also reports cache hits as a top-level `usage.cached_tokens`.
fn fill_cached(u: &mut Usage) {
    if u.cached_input_tokens.is_none() {
        u.cached_input_tokens = u.raw.get("cached_tokens").and_then(Value::as_u64);
    }
}

// ------------------------------------------------------------------ cost

/// Estimated cost in USD: tokens at the CNY list price (cache hits, cache
/// writes at the 5-minute price, output incl. reasoning) plus executed web
/// searches.
pub fn estimate_cost(spec: &ModelSpec, u: &Usage, searches: u64, cny_per_usd: f64) -> Option<f64> {
    let tokens = spec.price.cost(u)?;
    usd(tokens + searches as f64 * WEB_SEARCH_CNY, Currency::Cny, cny_per_usd)
}

// ------------------------------------------------------------------ errors

/// Classify Moonshot's error objects (`{"type","message"[,"code"]}`; the
/// Responses API's failed responses carry `{"code","message"}`), HTTP or
/// in-band (platform.kimi.com/docs/api/errors, 2026-11-12).
pub fn error_from_json(err: &Value, http_status: Option<u16>, retry_after: Option<u64>, kind: ErrorKind) -> LlmError {
    let str_of = |k: &str| err.get(k).and_then(Value::as_str).filter(|s| !s.is_empty());
    // (`type: "error"` is the event type of a Responses `error` event, not an error type.)
    let ty = str_of("type").filter(|t| *t != "error").or_else(|| str_of("code")).unwrap_or("");
    let message = str_of("message").unwrap_or("(no message)").to_string();
    let retry = match ty {
        // Rate limits and capacity: wait, then retry. A daily token limit (TPD) only resets tomorrow.
        "rate_limit_reached_error" if message.contains("TPD") => Retry::Never,
        "engine_overloaded_error" | "rate_limit_reached_error" => Retry::After(retry_after.unwrap_or(20)),
        "server_error" | "unexpected_output" | "server_unavailable" | "client_closed_request" => match retry_after {
            Some(s) => Retry::After(s),
            None => Retry::Backoff,
        },
        // Balance, content policy, invalid request, auth, permission, unknown model.
        "exceeded_current_quota_error"
        | "content_filter"
        | "invalid_request_error"
        | "invalid_authentication_error"
        | "incorrect_api_key_error"
        | "permission_denied_error"
        | "resource_not_found_error" => Retry::Never,
        _ => http_status.map(|s| retry_for_status(s, retry_after)).unwrap_or(Retry::Never),
    };
    LlmError {
        kind,
        retry,
        status: http_status,
        // No context-overflow mapping: Moonshot lowers the output budget itself, and
        // "Input token length too long" (the prompt alone) cannot be fixed by a smaller one.
        error_type: (!ty.is_empty()).then(|| ty.to_string()),
        message,
        details: Some(err.clone()),
        partial_output: false,
    }
}

#[cfg(test)]
mod tests;
