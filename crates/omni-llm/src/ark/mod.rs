//! Volcengine Ark (火山方舟) direct provider for ByteDance Doubao / Seed models:
//! the OpenAI-compatible Chat API at `https://ark.cn-beijing.volces.com/api/v3`.
//!
//! Ark specifics handled here (see `docs/research/doubao.md`):
//! - `max_tokens` counts the **answer only** and defaults to 4096 (silent
//!   truncation), so it is always sent; the reasoning has its own model limit.
//!   Ark pre-deducts TPM quota by `prompt + max_tokens`, hence a cap.
//! - Thinking is on by default at the model's official effort (2.1 and
//!   Evolving `high`, 2.0 and Character `medium`); the highest effort is `high`
//!   (`xhigh`/`max` are accepted and mapped to it).
//! - Summary-mode models (2.1, Evolving, 2.0-lite-260428) return a summary in
//!   `reasoning_content` plus the encrypted original in `encrypted_content`;
//!   both are replayed verbatim for the same model.
//! - Images default to `detail: "xhigh"` (up to 9 MP). Assistant messages
//!   carry text only (`content` is a string): media other models generated in
//!   earlier turns are an error (never dropped quietly).
//! - Web search (`+s`): Doubao Search through `web_search_options: {}`; the
//!   model decides whether to search. Ark reports the searches it actually ran
//!   only as a count (`usage.tool_usage.web_search`), so each becomes a
//!   `web_search` call without query or results, where the count first shows
//!   (before the answer if the count comes after it); the cited sources come
//!   as `annotations` (title and URL). A searched answer whose usage reports
//!   none says that no search ran.
//! - Errors are `{"error":{"code","message","param","type"}}` with string codes.
//!   Their messages name the account (`Your account 2100123456 has not
//!   activated …`); the number is removed from what the bot shows and stores
//!   ([`hide_account`]), only the raw log keeps it.
//! - No cost in responses: estimated from a CNY price table, plus Doubao
//!   Search's pay-as-you-go fee per reported search.
//! - The served model, tier (`service_tier`) and a model fallback
//!   (`service_status.model_fallback`) are read from the response envelope by
//!   [`crate::meta`].
//!
//! Seedream image models (`doubao-seedream-…`) and Seedance video models
//! (`doubao-seedance-…`) have no chat API: their ids go to Ark's image API
//! ([`images`]) and video generation API ([`video`]).

pub mod images;
pub mod video;

use std::sync::{Arc, LazyLock};
use std::time::Duration;

use omni_core::Usage;
use omni_core::content::{ToolCall, Turn};
use omni_core::usage::CostSource;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::chatstream::EventParser;
use crate::error::{ErrorKind, LlmError, Retry, retry_for_status};
use crate::event::StreamEvent;
use crate::openrouter::chat::{self, ChunkParser, Flavor};
use crate::openrouter::options::Applied;
use crate::{BlobLoader, CallInput, CallOutcome, EventSink, Observer, Prepared};

pub const DIALECT: &str = "ark.chat";
pub const DEFAULT_BASE_URL: &str = "https://ark.cn-beijing.volces.com/api/v3";
/// Doubao Search (Custom) per executed search, pay-as-you-go: ¥0.020
/// ("融合信息搜索Custom版-web搜索 0.020元/次", docs.volcengine.com
/// `networked-search/online-search-product-billing`, 2026-11-13). The monthly
/// packs are cheaper; the bot cannot see them, so this is an upper estimate.
pub const WEB_SEARCH_CNY: f64 = 0.02;
/// A `+s` answer whose usage reports no executed search.
const NO_SEARCH: &str = "no web search ran (the model decides)";

#[derive(Debug, Clone)]
pub struct ArkConfig {
    pub api_key: String,
    pub base_url: String,
    pub connect_timeout: Duration,
    pub idle_timeout: Duration,
    /// Cap for the automatic `max_tokens` (Ark reserves TPM quota by it).
    pub max_tokens_cap: u64,
    /// CNY per USD, for cost estimates.
    pub cny_per_usd: f64,
}

/// CNY per 1M tokens in one input-length tier: input, cache hit, output, and
/// audio input / audio cache hit for the models that take audio.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rates {
    pub input: f64,
    pub cached: f64,
    pub output: f64,
    pub audio: Option<(f64, f64)>,
}

const fn r(input: f64, cached: f64, output: f64) -> Rates {
    Rates { input, cached, output, audio: None }
}

const fn ra(input: f64, cached: f64, output: f64, audio: f64, audio_cached: f64) -> Rates {
    Rates { input, cached, output, audio: Some((audio, audio_cached)) }
}

/// Prices by input length, ascending: (largest prompt of the tier in K tokens, rates).
/// K is 1,024: the top tiers ("(128, 256]", "[0, 1024]") are the context windows
/// (262,144 and 1,048,576 tokens), which 1,000 would not cover.
pub type Tiers = &'static [(u64, Rates)];

/// Known limits and list prices of Ark models (matched by id prefix, longest first).
#[derive(Debug, Clone, Copy)]
pub struct ModelSpec {
    pub prefix: &'static str,
    pub context: u64,
    /// Max answer tokens (`max_tokens`), excluding reasoning.
    pub max_answer: u64,
    /// 在线推理（常规）: `service_tier` `default`.
    pub standard: Tiers,
    /// 在线推理（低延迟）: `fast` (empty: not offered).
    pub fast: Tiers,
    /// 在线推理（低优）: `flex` (empty: not offered).
    pub flex: Tiers,
    /// The default `reasoning_effort` (Ark's thinking guide: 2.1 and Evolving
    /// `high`, the top level; 2.0 and character `medium`).
    pub effort: &'static str,
}

const PRO_21: Tiers = &[(1024, r(6.0, 1.2, 30.0))];
const SEED_20_PRO: Tiers = &[(32, r(3.2, 0.64, 16.0)), (128, r(4.8, 0.96, 24.0)), (256, r(9.6, 1.92, 48.0))];

/// Limits from the Ark model list, 2026-11-11 (docs/research/doubao.md §1.1);
/// prices from docs.volcengine.com `ark/model-pricing`, 2026-11-13. The tier
/// a response reports (`service_tier`) is the one billed ("实际计费以响应回显档位为准").
pub const MODELS: &[ModelSpec] = &[
    ModelSpec {
        prefix: "doubao-seed-evolving",
        effort: "high",
        context: 1_048_576,
        max_answer: 262_144,
        standard: PRO_21,
        fast: &[],
        flex: &[],
    },
    ModelSpec {
        prefix: "doubao-seed-2-1-pro",
        effort: "high",
        context: 1_048_576,
        max_answer: 262_144,
        standard: PRO_21,
        fast: &[],
        flex: &[(1024, r(3.0, 1.2, 15.0))],
    },
    ModelSpec {
        prefix: "doubao-seed-2-1-lite",
        effort: "high",
        context: 1_048_576,
        max_answer: 262_144,
        standard: &[(1024, ra(0.8, 0.16, 2.7, 12.0, 2.4))],
        fast: &[(1024, ra(1.6, 0.32, 5.4, 24.0, 4.8))],
        flex: &[(1024, ra(0.4, 0.16, 1.35, 6.0, 2.4))],
    },
    ModelSpec {
        prefix: "doubao-seed-2-1-turbo",
        effort: "high",
        context: 262_144,
        max_answer: 262_144,
        standard: &[(256, r(3.0, 0.6, 15.0))],
        fast: &[(256, r(6.0, 1.2, 30.0))],
        flex: &[(256, r(1.5, 0.6, 7.5))],
    },
    ModelSpec {
        prefix: "doubao-seed-2-0-lite",
        effort: "medium",
        context: 262_144,
        max_answer: 131_072,
        standard: &[
            (32, ra(0.6, 0.12, 3.6, 9.0, 1.8)),
            (128, ra(0.9, 0.18, 5.4, 13.5, 2.7)),
            (256, ra(1.8, 0.36, 10.8, 27.0, 5.4)),
        ],
        fast: &[
            (32, ra(1.2, 0.24, 7.2, 18.0, 3.6)),
            (128, ra(1.8, 0.36, 10.8, 27.0, 5.4)),
            (256, ra(3.6, 0.72, 21.6, 54.0, 10.8)),
        ],
        flex: &[
            (32, ra(0.3, 0.12, 1.8, 4.5, 1.8)),
            (128, ra(0.45, 0.18, 2.7, 6.75, 2.7)),
            (256, ra(0.9, 0.36, 5.4, 13.5, 5.4)),
        ],
    },
    ModelSpec {
        prefix: "doubao-seed-2-0-mini",
        effort: "medium",
        context: 262_144,
        max_answer: 131_072,
        standard: &[
            (32, ra(0.2, 0.04, 2.0, 3.0, 0.6)),
            (128, ra(0.4, 0.08, 4.0, 6.0, 1.2)),
            (256, ra(0.8, 0.16, 8.0, 12.0, 2.4)),
        ],
        fast: &[
            (32, ra(0.4, 0.08, 4.0, 6.0, 1.2)),
            (128, ra(0.8, 0.16, 8.0, 12.0, 2.4)),
            (256, ra(1.6, 0.32, 16.0, 24.0, 4.8)),
        ],
        flex: &[
            (32, ra(0.1, 0.04, 1.0, 1.5, 0.6)),
            (128, ra(0.2, 0.08, 2.0, 3.0, 1.2)),
            (256, ra(0.4, 0.16, 4.0, 6.0, 2.4)),
        ],
    },
    ModelSpec {
        prefix: "doubao-seed-2-0-pro",
        effort: "medium",
        context: 262_144,
        max_answer: 131_072,
        standard: SEED_20_PRO,
        fast: &[(32, r(9.6, 1.92, 48.0)), (128, r(14.4, 2.88, 72.0)), (256, r(28.8, 5.76, 144.0))],
        flex: &[(32, r(1.6, 0.64, 8.0)), (128, r(2.4, 0.96, 12.0)), (256, r(4.8, 1.92, 24.0))],
    },
    ModelSpec {
        prefix: "doubao-seed-2-0-code",
        effort: "medium",
        context: 262_144,
        max_answer: 131_072,
        standard: SEED_20_PRO,
        fast: &[],
        flex: &[],
    },
    ModelSpec {
        prefix: "doubao-seed-character-260628",
        effort: "medium",
        context: 131_072,
        max_answer: 32_768,
        standard: &[(32, r(0.8, 0.16, 2.0)), (128, r(1.2, 0.16, 6.0))],
        fast: &[],
        flex: &[],
    },
];

pub fn model_spec(model: &str) -> Option<&'static ModelSpec> {
    MODELS.iter().filter(|m| model.starts_with(m.prefix)).max_by_key(|m| m.prefix.len())
}

fn replay(p: &Value) -> Value {
    let mut m = json!({"role": "assistant", "content": p.get("content").cloned().unwrap_or(json!(""))});
    if let Some(r) = p.get("reasoning").and_then(Value::as_str).filter(|s| !s.is_empty()) {
        m["reasoning_content"] = json!(r);
    }
    if let Some(e) = p.get("encrypted_content").and_then(Value::as_str).filter(|s| !s.is_empty()) {
        m["encrypted_content"] = json!(e);
    }
    m
}

pub const FLAVOR: Flavor = Flavor { dialect: DIALECT, image_detail: Some("xhigh"), replay };

/// What Ark's Chat API offers (the vendor's own API, so official = route).
struct ArkCaps(Option<&'static ModelSpec>);

impl crate::request::RouteCaps for ArkCaps {
    fn name(&self) -> &str {
        "Volcengine Ark"
    }
    fn tool(&self, tool: crate::request::Tool) -> bool {
        tool == crate::request::Tool::Search // Doubao search (`web_search_options`)
    }
    fn efforts(&self) -> Option<Vec<String>> {
        Some(["high", "medium", "low", "minimal"].map(String::from).to_vec())
    }
    fn can_disable_reasoning(&self) -> bool {
        true
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
        vec!["flex".into(), "fast".into()]
    }
    /// A model outside the table: nothing is sent, Ark applies its default.
    fn default_effort(&self) -> Option<String> {
        self.0.map(|s| s.effort.to_string())
    }
}

#[derive(Clone)]
pub struct Ark {
    cfg: Arc<ArkConfig>,
    http: reqwest::Client,
}

impl Ark {
    pub fn new(cfg: ArkConfig) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder()
            .connect_timeout(cfg.connect_timeout)
            .tcp_keepalive(Duration::from_secs(30))
            .pool_idle_timeout(Duration::from_secs(90))
            .build()?;
        Ok(Self { cfg: Arc::new(cfg), http })
    }

    pub fn config(&self) -> &ArkConfig {
        &self.cfg
    }

    fn auto_max_tokens(&self, input: &CallInput<'_>) -> Option<u64> {
        let spec = model_spec(input.model);
        let prompt = crate::budget::estimate_prompt(input.history, input.known_context);
        let cap = (self.cfg.max_tokens_cap > 0).then_some(self.cfg.max_tokens_cap);
        match spec {
            Some(s) => crate::budget::choose(Some(s.context), Some(s.max_answer), cap, prompt),
            // Unknown model: Ark's own default (4096) would truncate silently; use the cap.
            None => cap,
        }
    }

    pub async fn prepare_chat(
        &self,
        input: &CallInput<'_>,
        presets: &[crate::request::Requested],
        blobs: &dyn BlobLoader,
    ) -> Result<Prepared, LlmError> {
        let user = crate::request::parse(input.options).map_err(LlmError::request)?;
        let mut req = crate::request::Requested::default();
        for p in presets {
            req = crate::openrouter::options::overlay(&req, p);
        }
        let mut req = crate::openrouter::options::overlay(&req, &user);
        if req.providers.is_some() {
            return Err(LlmError::request("`p=` (provider choice) does not apply to Volcengine Ark"));
        }
        let caps = ArkCaps(model_spec(input.model));
        crate::request::validate(input.model, &req, &caps).map_err(LlmError::request)?;
        let level_note = crate::request::resolve_reasoning(input.model, &mut req, &caps).map_err(LlmError::request)?;
        let (messages, mut notes) =
            chat::convert_history_with(input.history, input.model, None, blobs, &FLAVOR).await?;
        notes.notes.splice(0..0, level_note);
        let mut body = json!({
            "model": input.model,
            "messages": messages,
            "stream": true,
            // Cumulative usage on every chunk: tokens and cost update while streaming.
            "stream_options": {"include_usage": true, "chunk_include_usage": true},
        });
        let mut applied = Vec::new();
        let mut settings = Vec::new();
        // Reasoning: the model's default level (sent explicitly); e=none turns thinking off.
        match req.effort.as_deref() {
            Some("none") | Some("minimal") => {
                body["thinking"] = json!({"type": "disabled"});
                body["reasoning_effort"] = json!("minimal");
                settings.push("thinking off".to_string());
            }
            Some(e) => {
                body["thinking"] = json!({"type": "enabled"});
                body["reasoning_effort"] = json!(e);
                settings.push(format!("effort {e}"));
            }
            // A model outside the table: Ark's own default.
            None => settings.push("effort: Ark's default".to_string()),
        }
        if req.effort.is_some() {
            applied.push(Applied {
                source: if user.effort.is_some() || user.level.is_some() { "option" } else { "default" }.into(),
                patch: json!({"thinking": body["thinking"], "reasoning_effort": body["reasoning_effort"]}),
            });
        }
        let auto_max = input.max_tokens_override.or_else(|| self.auto_max_tokens(input));
        if let Some(m) = auto_max {
            body["max_tokens"] = json!(m);
            applied.push(Applied { source: "budget".into(), patch: json!({"max_tokens": m}) });
        }
        if req.tools.contains(&crate::request::Tool::Search) {
            body["web_search_options"] = json!({});
            settings.push("Doubao search".into());
            applied.push(Applied { source: "option".into(), patch: json!({"web_search_options": {}}) });
        }
        if let Some(t) = req.tier.as_deref().filter(|t| !matches!(*t, "default" | "auto")) {
            body["service_tier"] = json!(t);
            settings.push(format!("tier {t}"));
            applied.push(Applied { source: "option".into(), patch: json!({"service_tier": t}) });
        }
        // Routes the conversation's requests to the same implicit cache ("routing hint
        // for implicit cache"; measured 2026-11-13: without it a 15K-token follow-up
        // had no cache hit).
        if let Some(k) = &input.session_id {
            body["prompt_cache_key"] = json!(k);
            applied.push(Applied { source: "default".into(), patch: json!({"prompt_cache_key": k}) });
        }
        let mut warnings = Vec::new();
        // This model's own answers are replayed (with their reasoning); other turns go as their text.
        let own = |t: &Turn| t.native.as_ref().is_some_and(|n| n.dialect == DIALECT && n.model == input.model);
        crate::provider::unsent_media(input.history, own)?;
        if model_spec(input.model).is_none() {
            warnings.push(format!(
                "`{}` is not in the bot's Ark model table: limits and price unknown (max_tokens {}; cost not estimated)",
                input.model,
                body.get("max_tokens").map(|v| v.to_string()).unwrap_or_else(|| "not set".into())
            ));
        }
        Ok(Prepared {
            dialect: DIALECT,
            model: input.model.to_string(),
            url: format!("{}/chat/completions", self.cfg.base_url.trim_end_matches('/')),
            body,
            applied,
            warnings,
            notes: notes.notes,
            show_debug: false,
            auto_max_tokens: auto_max,
            long_decode: false,
            settings,
        })
    }

    /// Run a prepared request. Usage events get an estimated cost.
    pub async fn stream_chat(
        &self,
        p: &Prepared,
        sink: &mut dyn EventSink,
        observer: &dyn Observer,
        cancel: &CancellationToken,
    ) -> (Result<Value, LlmError>, CallOutcome) {
        let rb = self.http.post(&p.url).bearer_auth(&self.cfg.api_key).json(&p.body);
        let crate::chatstream::Streamed { result, parser, header_id } = crate::chatstream::run_with(
            rb,
            &p.url,
            &p.body,
            self.cfg.idle_timeout,
            Some("x-request-id"),
            error_from_json,
            ChatParser::new(&p.model, &p.body, self.cfg.cny_per_usd),
            sink,
            observer,
            cancel,
        )
        .await;
        let outcome = CallOutcome { upstream_id: header_id, ..Default::default() };
        match result {
            Err(e) => (Err(e), outcome),
            Ok(()) => {
                if p.body.get("web_search_options").is_some() && parser.no_search() {
                    sink.event(StreamEvent::Notice(NO_SEARCH.into()));
                }
                (Ok(parser.inner.native_payload()), outcome)
            }
        }
    }
}

#[async_trait::async_trait]
impl crate::provider::Provider for Ark {
    fn id(&self) -> &'static str {
        "ark"
    }
    fn name(&self) -> &'static str {
        "Volcengine Ark"
    }
    fn knows(&self, model: &str) -> bool {
        model_spec(model).is_some() || images::spec(model).is_some() || video::spec(model).is_some()
    }
    async fn prepare(
        &self,
        input: &CallInput<'_>,
        presets: &[crate::request::Requested],
        blobs: &dyn BlobLoader,
    ) -> Result<Prepared, LlmError> {
        if !self.knows(input.model) {
            return Err(crate::provider::not_in_table(self.name(), input.model));
        }
        if images::is_image_model(input.model) {
            return images::prepare(&self.cfg, input, presets, blobs).await;
        }
        if video::is_video_model(input.model) {
            return video::prepare(&self.cfg, input, presets, blobs).await;
        }
        self.prepare_chat(input, presets, blobs).await
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
        if p.dialect == video::DIALECT {
            return video::stream(&self.http, &self.cfg, p, sink, observer, cancel).await;
        }
        self.stream_chat(p, sink, observer, cancel).await
    }
}

/// Estimated cost in USD, plus the searches the usage reports
/// (`tool_usage.web_search`, [`WEB_SEARCH_CNY`] each). `tier`: the response's
/// `service_tier` (`default`, `fast`, `flex`; the request's when the response
/// has none); `None` for a tier the model's price list does not have. The
/// input-length tier is chosen by the whole prompt (`prompt_tokens`, cache hits
/// included), as Ark's example does ("200K 输入 … (128, 256]"). `prompt_tokens`
/// includes the audio (measured 2026-11-13: 57 tokens of text, 90 with a 5 s
/// clip of `audio_tokens` 32), which costs the audio rates.
pub fn estimate_cost(model: &str, u: &Usage, tier: Option<&str>, cny_per_usd: f64) -> Option<f64> {
    let s = model_spec(model)?;
    let tiers = match tier {
        Some("fast") => s.fast,
        Some("flex") => s.flex,
        _ => s.standard,
    };
    let prompt = u.input_tokens?;
    let (_, rates) = tiers.iter().find(|(k, _)| prompt <= k * 1024).or(tiers.last())?;
    let output = u.output_tokens? as f64;
    let cached = u.cached_input_tokens.unwrap_or(0).min(prompt);
    let n = |p: &str| u.raw.pointer(p).and_then(Value::as_u64).unwrap_or(0);
    let (audio_rate, audio_cached_rate) = rates.audio.unwrap_or((rates.input, rates.cached));
    let audio_cached = n("/prompt_tokens_details/audio_cached_tokens").min(cached);
    let audio = n("/prompt_tokens_details/audio_tokens").saturating_sub(audio_cached).min(prompt - cached);
    let f = |x: u64| x as f64;
    let searches = n("/tool_usage/web_search") as f64;
    let cny = (f(prompt - cached - audio) * rates.input
        + f(audio) * audio_rate
        + f(cached - audio_cached) * rates.cached
        + f(audio_cached) * audio_cached_rate
        + output * rates.output)
        / 1e6
        + searches * WEB_SEARCH_CNY;
    (cny_per_usd > 0.0).then(|| cny / cny_per_usd)
}

// ------------------------------------------------------------------ stream

/// Chat chunks, plus the searches Ark reports: `usage.tool_usage.web_search`
/// ("本次请求内实际执行的联网搜索次数", the searches actually run), cumulative
/// like the rest of the usage. Each new one is a `web_search` call (Ark gives
/// no query or results): where the count rises, or before the answer when it
/// rises after the answer began (the model answers after searching).
struct ChatParser {
    inner: ChunkParser,
    /// For the cost: the model, CNY per USD, and the tier (the request's until
    /// a chunk reports the tier used, `service_tier`).
    model: String,
    cny_per_usd: f64,
    tier: Option<String>,
    /// Searches shown so far.
    searches: u64,
    /// Answer text has been streamed.
    text: bool,
    /// A usage object came (Ark sends one with every chunk).
    usage: bool,
    /// The answer cites search results (`annotations`).
    cited: bool,
    /// The finish reason.
    finish: Option<String>,
}

impl ChatParser {
    fn new(model: &str, body: &Value, cny_per_usd: f64) -> Self {
        let mut inner = ChunkParser::default();
        inner.map_error = Some(error_from_json);
        Self {
            inner,
            model: model.to_string(),
            cny_per_usd,
            tier: body.get("service_tier").and_then(Value::as_str).map(str::to_string),
            searches: 0,
            text: false,
            usage: false,
            cited: false,
            finish: None,
        }
    }

    /// The answer ended normally (or at the output limit) and the usage
    /// reports no executed search (and nothing was cited).
    fn no_search(&self) -> bool {
        matches!(self.finish.as_deref(), Some("stop" | "length")) && self.usage && self.searches == 0 && !self.cited
    }
}

impl EventParser for ChatParser {
    fn event(&mut self, _name: Option<&str>, v: &Value) -> Result<Vec<StreamEvent>, LlmError> {
        if let Some(t) = v.get("service_tier").and_then(Value::as_str) {
            self.tier = Some(t.to_string());
        }
        let mut evs = self.inner.chunk(v)?;
        for e in &mut evs {
            if let StreamEvent::Usage(u) = e
                && u.cost_usd.is_none()
            {
                u.cost_usd = estimate_cost(&self.model, u, self.tier.as_deref(), self.cny_per_usd);
                u.cost_source = if u.cost_usd.is_some() { CostSource::Estimated } else { CostSource::Unknown };
            }
        }
        let mut out = Vec::with_capacity(evs.len() + 1);
        if let Some(n) = v.pointer("/usage/tool_usage/web_search").and_then(Value::as_u64)
            && n > self.searches
        {
            for i in self.searches + 1..=n {
                let call = ToolCall {
                    id: Some(format!("web_search_{i}")),
                    name: "web_search".into(),
                    status: Some("completed".into()),
                    input: None,
                    output: None,
                };
                out.push(if self.text { StreamEvent::ToolBefore(call) } else { StreamEvent::Tool(call) });
            }
            self.searches = n;
        }
        self.usage |= v.get("usage").is_some_and(Value::is_object);
        for e in &evs {
            match e {
                StreamEvent::Text(_) => self.text = true,
                StreamEvent::Citations(c) if !c.is_empty() => self.cited = true,
                StreamEvent::Finish { reason: Some(r), .. } => self.finish = Some(r.clone()),
                _ => {}
            }
        }
        out.extend(evs);
        Ok(out)
    }
    fn finished(&self) -> bool {
        self.inner.finished
    }
    fn any_output(&self) -> bool {
        self.inner.any_output()
    }
}

/// Ark's messages name the account (`Your account 2100123456 has not activated
/// the model …`): the number goes, in every string of `v`.
pub fn hide_account(v: &mut Value) {
    static ACCOUNT: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(r"(?i)(\baccount|账号|账户)(?:\s*id)?\s*[:=：]?\s*\d{6,}\b").expect("valid regex")
    });
    match v {
        Value::String(s) => {
            if let std::borrow::Cow::Owned(n) = ACCOUNT.replace_all(s, "$1") {
                *s = n;
            }
        }
        Value::Array(a) => a.iter_mut().for_each(hide_account),
        Value::Object(o) => o.values_mut().for_each(hide_account),
        _ => {}
    }
}

/// Classify Ark's error objects (docs/research/doubao.md §7), without the
/// account number ([`hide_account`]).
pub fn error_from_json(err: &Value, http_status: Option<u16>, retry_after: Option<u64>, kind: ErrorKind) -> LlmError {
    let mut err = err.clone();
    hide_account(&mut err);
    let code = err.get("code").and_then(Value::as_str).unwrap_or("").to_string();
    let message = err.get("message").and_then(Value::as_str).unwrap_or("(no message)").to_string();
    let rate_limited = code.starts_with("RateLimitExceeded")
        || (code.starts_with("ModelAccount") && code.contains("RateLimitExceeded"))
        || matches!(
            code.as_str(),
            "AccountRateLimitExceeded"
                | "ModelAccountConcurrentRequestsLimitExceeded"
                | "RequestBurstTooFast"
                | "ServerOverloaded"
        );
    let retry = if rate_limited {
        Retry::After(retry_after.unwrap_or(20))
    } else {
        match code.as_str() {
            "InternalServiceError"
            | "ModelLoading"
            | "ServiceConnectionRefused"
            | "NoAvailableService"
            | "RequestTimeout" => match retry_after {
                Some(s) => Retry::After(s),
                None => Retry::Backoff,
            },
            "" => http_status.map(|s| retry_for_status(s, retry_after)).unwrap_or(Retry::Backoff),
            // Quotas, content blocks, invalid requests, auth, unknown models: retrying cannot help.
            _ => Retry::Never,
        }
    };
    let error_type = match code.as_str() {
        "OutofContextError" => Some("context_length_exceeded".to_string()),
        "" => None,
        c => Some(c.to_string()),
    };
    LlmError { kind, retry, status: http_status, error_type, message, details: Some(err), partial_output: false }
}

#[cfg(test)]
mod tests {
    use omni_core::content::{Part, Role};

    use super::*;

    #[test]
    fn model_table_prefers_longest_prefix() {
        assert_eq!(model_spec("doubao-seed-2-1-pro-260915").unwrap().prefix, "doubao-seed-2-1-pro");
        assert_eq!(model_spec("doubao-seed-2-0-mini-260428").unwrap().max_answer, 131_072);
        assert!(model_spec("doubao-1.5-pro-32k").is_none());
    }

    #[test]
    fn cost_estimate() {
        let u = Usage {
            input_tokens: Some(1_000_000),
            cached_input_tokens: Some(0),
            output_tokens: Some(100_000),
            ..Default::default()
        };
        // 6 CNY + 3 CNY at 7.0 CNY/USD
        let c = estimate_cost("doubao-seed-2-1-pro-260915", &u, None, 7.0).unwrap();
        assert!((c - 9.0 / 7.0).abs() < 1e-9);
        assert!((estimate_cost("doubao-seed-2-1-pro-260915", &u, Some("default"), 7.0).unwrap() - c).abs() < 1e-12);
        // Flex: half on input and output (3 + 1.5 CNY); no fast tier on 2.1 Pro.
        let flex = estimate_cost("doubao-seed-2-1-pro-260915", &u, Some("flex"), 7.0).unwrap();
        assert!((flex - 4.5 / 7.0).abs() < 1e-9, "{flex}");
        assert!(estimate_cost("doubao-seed-2-1-pro-260915", &u, Some("fast"), 7.0).is_none());
    }

    /// Prices by input length (K = 1,024, the whole prompt, cache hits included)
    /// and by the tier used, from Ark's price list (2026-11-13).
    #[test]
    fn cost_by_input_length_and_tier() {
        let usage = |prompt: u64, cached: u64| Usage {
            input_tokens: Some(prompt),
            cached_input_tokens: Some(cached),
            output_tokens: Some(1_000_000),
            ..Default::default()
        };
        let cny = |model: &str, u: &Usage, tier: Option<&str>| estimate_cost(model, u, tier, 1.0).unwrap();
        let near = |a: f64, b: f64| (a - b).abs() < 1e-9;
        // 2.0 Pro: [0, 32K] 3.2/0.64/16, (32K, 128K] 4.8/0.96/24, (128K, 256K] 9.6/1.92/48.
        let pro = "doubao-seed-2-0-pro-260215";
        assert!(near(cny(pro, &usage(32_768, 0), None), (32_768.0 * 3.2) / 1e6 + 16.0));
        assert!(near(cny(pro, &usage(32_769, 0), None), (32_769.0 * 4.8) / 1e6 + 24.0));
        assert!(near(cny(pro, &usage(200_000, 150_000), None), (50_000.0 * 9.6 + 150_000.0 * 1.92) / 1e6 + 48.0));
        // Fast 3×, flex: half, cache hits at the standard price.
        assert!(near(cny(pro, &usage(10_000, 0), Some("fast")), 10_000.0 * 9.6 / 1e6 + 48.0));
        assert!(near(cny(pro, &usage(10_000, 5_000), Some("flex")), (5_000.0 * 1.6 + 5_000.0 * 0.64) / 1e6 + 8.0));
        // Character: (32K, 128K] 1.2/0.16/6.
        assert!(near(cny("doubao-seed-character-260628", &usage(40_000, 0), None), 40_000.0 * 1.2 / 1e6 + 6.0));
        // Single-tier models: 2.1 Turbo fast 6/1.2/30.
        assert!(near(
            cny("doubao-seed-2-1-turbo-260915", &usage(200_000, 0), Some("fast")),
            200_000.0 * 6.0 / 1e6 + 30.0
        ));
        // Audio at its own rates (2.1 Lite: 12, cache hits 2.4), out of `prompt_tokens`.
        let audio = Usage {
            input_tokens: Some(90),
            cached_input_tokens: Some(0),
            output_tokens: Some(115),
            raw: json!({"prompt_tokens": 90, "prompt_tokens_details": {"cached_tokens": 0, "audio_tokens": 32}}),
            ..Default::default()
        };
        assert!(near(cny("doubao-seed-2-1-lite-260915", &audio, None), (58.0 * 0.8 + 32.0 * 12.0 + 115.0 * 2.7) / 1e6));
        let cached_audio = Usage {
            input_tokens: Some(1000),
            cached_input_tokens: Some(600),
            output_tokens: Some(0),
            raw: json!({"prompt_tokens_details": {"cached_tokens": 600, "audio_tokens": 500, "audio_cached_tokens": 400}}),
            ..Default::default()
        };
        // Text: 300 uncached at 0.8, 200 cached at 0.16; audio: 100 at 12, 400 cached at 2.4.
        let want = (300.0 * 0.8 + 200.0 * 0.16 + 100.0 * 12.0 + 400.0 * 2.4) / 1e6;
        assert!(near(cny("doubao-seed-2-1-lite-260915", &cached_audio, None), want));
    }

    /// The tier a response reports is the one billed.
    #[test]
    fn the_tier_used_is_billed() {
        let mut c = chunk(json!({"content": "hi"}), Some("stop"), 1_000_000, 100_000, None);
        c["service_tier"] = json!("flex");
        let mut p = ChatParser::new(MODEL, &json!({"service_tier": "flex"}), 7.0);
        let evs = p.event(None, &c).unwrap();
        let u = evs.iter().find_map(|e| if let StreamEvent::Usage(u) = e { Some(u) } else { None }).unwrap();
        assert!((u.cost_usd.unwrap() - 4.5 / 7.0).abs() < 1e-9);
        // Asked for flex, served at the default tier: the default price.
        c["service_tier"] = json!("default");
        let mut p = ChatParser::new(MODEL, &json!({"service_tier": "flex"}), 7.0);
        let evs = p.event(None, &c).unwrap();
        let u = evs.iter().find_map(|e| if let StreamEvent::Usage(u) = e { Some(u) } else { None }).unwrap();
        assert!((u.cost_usd.unwrap() - 9.0 / 7.0).abs() < 1e-9);
    }

    #[test]
    fn errors_are_classified() {
        let e = error_from_json(
            &json!({"code": "RateLimitExceeded.EndpointTPMExceeded", "message": "x"}),
            Some(429),
            None,
            ErrorKind::Http,
        );
        assert!(matches!(e.retry, Retry::After(20)));
        let e = error_from_json(&json!({"code": "QuotaExceeded", "message": "x"}), Some(429), None, ErrorKind::Http);
        assert!(matches!(e.retry, Retry::Never));
        let e = error_from_json(
            &json!({"code": "OutofContextError", "message": "too long"}),
            Some(400),
            None,
            ErrorKind::Http,
        );
        assert!(e.is_context_overflow());
        let e =
            error_from_json(&json!({"code": "InternalServiceError", "message": "x"}), Some(500), None, ErrorKind::Http);
        assert!(matches!(e.retry, Retry::Backoff));
    }

    /// The account number is neither shown nor stored: not in the message, not
    /// in the verbatim JSON of the error fold.
    #[test]
    fn errors_hide_the_account() {
        let e = error_from_json(
            &json!({"code": "ModelNotOpen", "message": "Your account 2100123456 has not activated the model \
                doubao-seedream-5-5-261028. Please activate the model service in the Ark Console. Request id: \
                0217630000000000000000000000000000000000000000000000", "param": "", "type": "NotFound"}),
            Some(404),
            None,
            ErrorKind::Http,
        );
        assert!(
            e.message.starts_with("Your account has not activated the model doubao-seedream-5-5-261028."),
            "{}",
            e.message
        );
        assert!(e.message.contains("Request id: 0217630000"), "{}", e.message);
        assert!(!e.details.as_ref().unwrap().to_string().contains("2100123456"));
        for (raw, shown) in [
            ("account ID: 2100123456 is overdue", "account is overdue"),
            ("AccountId=2100123456", "Account"),
            ("您的账号 2100123456 未开通该模型", "您的账号 未开通该模型"),
            ("账户ID：2100123456", "账户"),
            ("an account of 42 users", "an account of 42 users"),
        ] {
            let mut v = json!(raw);
            hide_account(&mut v);
            assert_eq!(v, json!(shown), "{raw}");
        }
    }

    struct NoBlobs;
    impl BlobLoader for NoBlobs {
        fn load<'a>(&'a self, _r: &'a omni_core::BlobRef) -> crate::BoxFuture<'a, anyhow::Result<Vec<u8>>> {
            Box::pin(async { anyhow::bail!("no blobs") })
        }
    }

    fn ark() -> Ark {
        Ark::new(ArkConfig {
            api_key: "k".into(),
            base_url: DEFAULT_BASE_URL.into(),
            connect_timeout: Duration::from_secs(1),
            idle_timeout: Duration::from_secs(1),
            max_tokens_cap: 65_536,
            cny_per_usd: 7.0,
        })
        .unwrap()
    }

    #[tokio::test]
    async fn request_defaults_and_options() {
        let history = vec![omni_core::Turn::user_text("hi")];
        let opts = vec![omni_core::trigger::OptionToken::KeyValue("e".into(), "none".into())];
        let input = CallInput {
            model: "doubao-seed-2-1-pro-260915",
            options: &opts,
            history: &history,
            session_id: None,
            user: None,
            known_context: None,
            max_tokens_override: None,
        };
        let p = ark().prepare_chat(&input, &[], &NoBlobs).await.unwrap();
        assert_eq!(p.url, "https://ark.cn-beijing.volces.com/api/v3/chat/completions");
        assert_eq!(p.body["thinking"]["type"], "disabled");
        assert_eq!(p.body["reasoning_effort"], "minimal");
        assert_eq!(p.body["max_tokens"], 65_536);
        assert_eq!(p.body["stream_options"]["include_usage"], true);
        assert_eq!(p.body["stream_options"]["chunk_include_usage"], true);
        assert!(p.body.get("user").is_none(), "Ark Chat has no `user` field");
        assert!(p.body.get("prompt_cache_key").is_none(), "no conversation, no cache key");
        // The official default: high on 2.1 (the top, no note), medium on 2.0 (a note); +h / +l.
        let run = |model: &'static str, flag: &'static str| {
            let opts: Vec<omni_core::trigger::OptionToken> =
                if flag.is_empty() { vec![] } else { vec![omni_core::trigger::OptionToken::Flag(flag.into())] };
            let history = history.clone();
            async move {
                let input = CallInput {
                    model,
                    options: &opts,
                    history: &history,
                    session_id: None,
                    user: None,
                    known_context: None,
                    max_tokens_override: None,
                };
                ark().prepare_chat(&input, &[], &NoBlobs).await.unwrap()
            }
        };
        let p = run("doubao-seed-2-1-lite-260915", "").await;
        assert_eq!((p.body["reasoning_effort"].as_str(), p.notes.len()), (Some("high"), 0));
        let p = run("doubao-seed-2-0-lite-260428", "").await;
        assert_eq!(p.body["reasoning_effort"], "medium");
        assert_eq!(p.notes, ["reasoning effort is medium by default, not the highest (high); add +h for the highest"]);
        let p = run("doubao-seed-2-0-lite-260428", "h").await;
        assert_eq!((p.body["reasoning_effort"].as_str(), p.notes.len()), (Some("high"), 0));
        let p = run("doubao-seed-2-0-lite-260428", "l").await;
        assert_eq!(p.body["thinking"], json!({"type": "disabled"}));
        let keyed = CallInput { session_id: Some("tg-9".into()), user: None, ..input };
        let p = ark().prepare_chat(&keyed, &[], &NoBlobs).await.unwrap();
        assert_eq!(p.body["prompt_cache_key"], "tg-9", "the conversation routes to one implicit cache");
        // Defaults without options: the official effort, explicit max_tokens (answer only, capped).
        let input = CallInput { options: &[], ..input };
        let p = ark().prepare_chat(&input, &[], &NoBlobs).await.unwrap();
        assert_eq!(p.body["reasoning_effort"], "high");
        assert_eq!(p.body["max_tokens"], 65_536);
        assert_eq!(p.auto_max_tokens, Some(65_536));
        // Unknown options are rejected, never ignored.
        let bad = vec![omni_core::trigger::OptionToken::Flag("frobnicate".into())];
        assert!(ark().prepare_chat(&CallInput { options: &bad, ..input }, &[], &NoBlobs).await.is_err());
    }

    #[test]
    fn replay_keeps_encrypted_reasoning() {
        let p = json!({"role": "assistant", "content": "hi", "reasoning": "sum", "encrypted_content": "abc"});
        let m = replay(&p);
        assert_eq!(m["reasoning_content"], "sum");
        assert_eq!(m["encrypted_content"], "abc");
    }

    const MODEL: &str = "doubao-seed-2-1-pro-260915";

    /// A streamed Chat chunk with Ark's cumulative usage (`chunk_include_usage`).
    fn chunk(delta: Value, finish: Option<&str>, prompt: u64, completion: u64, searches: Option<u64>) -> Value {
        let mut usage = json!({"prompt_tokens": prompt, "completion_tokens": completion, "total_tokens": prompt + completion,
                               "prompt_tokens_details": {"cached_tokens": 0}, "completion_tokens_details": {"reasoning_tokens": completion}});
        if let Some(n) = searches {
            usage["tool_usage"] = json!({"web_search": n});
        }
        json!({"id": "0217885", "object": "chat.completion.chunk", "created": 1788491002, "model": MODEL, "service_tier": "default",
               "choices": [{"index": 0, "delta": delta, "finish_reason": finish}], "usage": usage})
    }

    fn final_usage(prompt: u64, completion: u64, searches: Option<u64>) -> Value {
        let mut c = chunk(json!({}), None, prompt, completion, searches);
        c["choices"] = json!([]);
        c
    }

    fn run(chunks: &[Value]) -> (ChatParser, crate::Accumulator, Vec<StreamEvent>) {
        let mut p = ChatParser::new(MODEL, &json!({}), 7.0);
        let mut acc = crate::Accumulator::new();
        let mut all = Vec::new();
        for c in chunks {
            all.extend(p.event(None, c).unwrap());
        }
        for e in &all {
            acc.apply(e.clone());
        }
        (p, acc, all)
    }

    fn kinds(acc: &crate::Accumulator) -> Vec<&'static str> {
        acc.parts()
            .iter()
            .map(|p| match p {
                Part::Reasoning(_) => "reasoning",
                Part::ToolCall(_) => "tool",
                Part::Text { .. } => "text",
                _ => "other",
            })
            .collect()
    }

    /// The count rises while the model thinks: the searches show where they ran.
    #[test]
    fn searches_from_the_live_usage() {
        let (p, acc, _) = run(&[
            chunk(json!({"role": "assistant", "reasoning_content": "需要搜索。"}), None, 40, 5, None),
            chunk(json!({"reasoning_content": "结果显示……"}), None, 2710, 300, Some(2)),
            chunk(json!({"content": "GPT-6 Astra 发布了。"}), None, 2710, 520, Some(2)),
            chunk(
                json!({"content": "", "annotations": [{"type": "url_citation", "url_citation": {"title": "OpenAI 发布 GPT-6", "url": "https://news.sina.cn/x.html"}}]}),
                Some("stop"),
                2710,
                528,
                Some(2),
            ),
            final_usage(2710, 528, Some(2)),
        ]);
        assert_eq!(kinds(&acc), ["reasoning", "tool", "tool", "reasoning", "text"]);
        let Part::ToolCall(t) = &acc.parts()[1] else { panic!() };
        assert_eq!(
            (t.id.as_deref(), t.name.as_str(), t.status.as_deref()),
            (Some("web_search_1"), "web_search", Some("completed"))
        );
        assert_eq!((t.input.as_ref(), t.output.as_ref()), (None, None), "Ark reports only the count");
        let Some(Part::Text { citations, .. }) = acc.parts().last() else { panic!() };
        assert_eq!(citations[0].title.as_deref(), Some("OpenAI 发布 GPT-6"));
        assert!(!p.no_search());
        // 2710 × ¥6/M + 528 × ¥30/M + 2 searches × ¥0.02, at 7 CNY/USD.
        let cny = (2710.0 * 6.0 + 528.0 * 30.0) / 1e6 + 0.04;
        assert!((acc.usage.unwrap().cost_usd.unwrap() - cny / 7.0).abs() < 1e-12);
    }

    /// The count only in the last usage (after the answer): the search goes before the answer.
    #[test]
    fn searches_reported_after_the_answer() {
        let (_, acc, evs) = run(&[
            chunk(json!({"role": "assistant", "reasoning_content": "想"}), None, 40, 1, None),
            chunk(json!({"content": "答"}), None, 2710, 2, None),
            chunk(json!({"content": ""}), Some("stop"), 2710, 2, None),
            final_usage(2710, 2, Some(1)),
        ]);
        assert!(evs.iter().any(|e| matches!(e, StreamEvent::ToolBefore(_))));
        assert_eq!(kinds(&acc), ["reasoning", "tool", "text"]);
    }

    /// `+s`, but the usage reports no search (turn 284 of 2026-11-13).
    #[test]
    fn no_search_reported() {
        let (p, acc, _) = run(&[
            chunk(json!({"role": "assistant", "content": "1.85.0"}), Some("stop"), 67, 3, None),
            final_usage(67, 3, None),
        ]);
        assert!(p.no_search());
        assert_eq!(kinds(&acc), ["text"]);
        let (p, _, _) =
            run(&[chunk(json!({"content": "x"}), Some("length"), 67, 3, Some(0)), final_usage(67, 3, Some(0))]);
        assert!(p.no_search());
        // A blocked answer is a failure: nothing to say about the search.
        let (p, _, _) = run(&[chunk(json!({"content": ""}), Some("content_filter"), 67, 0, None)]);
        assert!(!p.no_search());
        // Cited search results mean a search ran, even without a count.
        let (p, _, _) = run(&[chunk(
            json!({"content": "x", "annotations": [{"type": "url_citation", "url_citation": {"title": "T", "url": "https://a.cn"}}]}),
            Some("stop"),
            67,
            3,
            None,
        )]);
        assert!(!p.no_search());
    }

    #[test]
    fn cost_with_searches() {
        let u = Usage {
            input_tokens: Some(1_000_000),
            output_tokens: Some(100_000),
            raw: json!({"tool_usage": {"web_search": 3}}),
            ..Default::default()
        };
        // 6 + 3 CNY + 3 × 0.02 CNY at 7.0 CNY/USD
        let c = estimate_cost(MODEL, &u, None, 7.0).unwrap();
        assert!((c - 9.06 / 7.0).abs() < 1e-9);
    }

    /// Media other models generated in earlier turns: assistant messages carry text only.
    #[tokio::test]
    async fn generated_media_of_other_models_are_not_sent() {
        let img = omni_core::BlobRef::for_bytes(b"\x89PNG", "image/png");
        let earlier = Turn {
            role: Role::Assistant,
            origin: omni_core::Origin::Live,
            parts: vec![
                Part::text("画好了"),
                Part::Image { blob: img.clone(), filename: None, generated: true, dims: None, thought: false },
            ],
            native: Some(omni_core::content::NativeOutput {
                dialect: images::DIALECT.into(),
                model: "doubao-seedream-5-5-261028".into(),
                payload: json!({}),
                context_tokens: None,
            }),
        };
        let history = vec![Turn::user_text("画猫"), earlier.clone(), Turn::user_text("描述一下")];
        let input = CallInput {
            model: MODEL,
            options: &[],
            history: &history,
            session_id: None,
            user: None,
            known_context: None,
            max_tokens_override: None,
        };
        // Assistant messages carry text only: the image cannot go along, which refuses the request.
        let e = ark().prepare_chat(&input, &[], &NoBlobs).await.unwrap_err();
        assert_eq!(
            e.message,
            "1 image generated in an earlier turn cannot be sent: this model sees only the text of other models' answers; send it the file as an attachment"
        );
        // The answer's text alone goes as an assistant message.
        let text_only = Turn { parts: vec![Part::text("画好了")], ..earlier };
        let history = vec![Turn::user_text("画猫"), text_only, Turn::user_text("描述一下")];
        let p = ark().prepare_chat(&CallInput { history: &history, ..input }, &[], &NoBlobs).await.unwrap();
        assert!(p.warnings.is_empty(), "{:?}", p.warnings);
        assert_eq!(p.body["messages"][1], json!({"role": "assistant", "content": "画好了"}));
    }

    /// One HTTP exchange on a local port: the base URL and a handle yielding the JSON body sent.
    async fn serve(body: String) -> (String, tokio::task::JoinHandle<Value>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/api/v3", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let (mut buf, mut tmp) = (Vec::new(), [0u8; 8192]);
            let sent = loop {
                let n = sock.read(&mut tmp).await.unwrap();
                buf.extend_from_slice(&tmp[..n]);
                if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&buf[..pos]).to_ascii_lowercase();
                    let len: usize = head
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length:").map(|v| v.trim().parse().unwrap()))
                        .unwrap_or(0);
                    while buf.len() < pos + 4 + len {
                        let n = sock.read(&mut tmp).await.unwrap();
                        buf.extend_from_slice(&tmp[..n]);
                    }
                    break serde_json::from_slice(&buf[pos + 4..pos + 4 + len]).unwrap_or(Value::Null);
                }
            };
            let resp = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            sock.write_all(resp.as_bytes()).await.unwrap();
            let _ = sock.shutdown().await;
            sent
        });
        (base, server)
    }

    /// `+s` end to end: a notice when Ark reports no search; none when it ran one.
    #[tokio::test]
    async fn end_to_end_search_notice() {
        let sse = |chunks: &[Value]| {
            let mut b: String = chunks.iter().map(|c| format!("data: {c}\n\n")).collect();
            b.push_str("data: [DONE]\n\n");
            b
        };
        let history = vec![omni_core::Turn::user_text("今天的新闻")];
        let opts = [omni_core::trigger::OptionToken::Flag("s".into())];
        for (searches, notice) in [(None, true), (Some(1), false)] {
            let (base, server) = serve(sse(&[
                chunk(json!({"role": "assistant", "content": "答"}), Some("stop"), 67, 3, searches),
                final_usage(67, 3, searches),
            ]))
            .await;
            let a = Ark::new(ArkConfig { base_url: base, ..ark().config().clone() }).unwrap();
            let input = CallInput {
                model: MODEL,
                options: &opts,
                history: &history,
                session_id: None,
                user: None,
                known_context: None,
                max_tokens_override: None,
            };
            let p = a.prepare_chat(&input, &[], &NoBlobs).await.unwrap();
            let mut evs = Vec::new();
            let (r, _) = a
                .stream_chat(&p, &mut |e: StreamEvent| evs.push(e), &crate::NullObserver, &CancellationToken::new())
                .await;
            assert!(r.is_ok());
            assert_eq!(server.await.unwrap()["web_search_options"], json!({}));
            let notices: Vec<_> = evs
                .iter()
                .filter_map(|e| if let StreamEvent::Notice(n) = e { Some(n.as_str()) } else { None })
                .collect();
            assert_eq!(
                notices,
                if notice { vec!["no web search ran (the model decides)"] } else { vec![] },
                "{searches:?}"
            );
        }
    }
}
