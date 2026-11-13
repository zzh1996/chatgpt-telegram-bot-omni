//! Volcengine Ark (火山方舟) direct provider for ByteDance Doubao / Seed models:
//! the OpenAI-compatible Chat API at `https://ark.cn-beijing.volces.com/api/v3`.
//!
//! Ark specifics handled here (see `docs/research/doubao.md`):
//! - `max_tokens` counts the **answer only** and defaults to 4096 (silent
//!   truncation), so it is always sent; the reasoning has its own model limit.
//!   Ark pre-deducts TPM quota by `prompt + max_tokens`, hence a cap.
//! - Thinking is on by default; the highest effort is `high` (`xhigh`/`max`
//!   are accepted and mapped to it).
//! - Summary-mode models (2.1, Evolving, 2.0-lite-260428) return a summary in
//!   `reasoning_content` plus the encrypted original in `encrypted_content`;
//!   both are replayed verbatim for the same model.
//! - Images default to `detail: "xhigh"` (up to 9 MP). Assistant messages
//!   carry text only (`content` is a string): media other models generated in
//!   earlier turns are not sent (a warning says what).
//! - Web search (`+s`): Doubao Search through `web_search_options: {}`; the
//!   model decides whether to search. Ark reports the searches it actually ran
//!   only as a count (`usage.tool_usage.web_search`), so each becomes a
//!   `web_search` call without query or results, where the count first shows
//!   (before the answer if the count comes after it); the cited sources come
//!   as `annotations` (title and URL). A searched answer whose usage reports
//!   none says that no search ran.
//! - Errors are `{"error":{"code","message","param","type"}}` with string codes.
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

use std::sync::Arc;
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

/// Known limits and list prices of Ark models (matched by id prefix, longest first).
#[derive(Debug, Clone, Copy)]
pub struct ModelSpec {
    pub prefix: &'static str,
    pub context: u64,
    /// Max answer tokens (`max_tokens`), excluding reasoning.
    pub max_answer: u64,
    /// CNY per 1M tokens: input, cached input, output (lowest input tier).
    pub price_cny: (f64, f64, f64),
}

/// From the Ark model list, 2026-11-11 (docs/research/doubao.md §1.1).
pub const MODELS: &[ModelSpec] = &[
    ModelSpec { prefix: "doubao-seed-evolving", context: 1_048_576, max_answer: 262_144, price_cny: (6.0, 1.2, 30.0) },
    ModelSpec { prefix: "doubao-seed-2-1-pro", context: 1_048_576, max_answer: 262_144, price_cny: (6.0, 1.2, 30.0) },
    ModelSpec { prefix: "doubao-seed-2-1-lite", context: 1_048_576, max_answer: 262_144, price_cny: (0.8, 0.16, 2.7) },
    ModelSpec { prefix: "doubao-seed-2-1-turbo", context: 262_144, max_answer: 262_144, price_cny: (3.0, 0.6, 15.0) },
    ModelSpec { prefix: "doubao-seed-2-0-lite", context: 262_144, max_answer: 131_072, price_cny: (0.6, 0.12, 3.6) },
    ModelSpec { prefix: "doubao-seed-2-0-mini", context: 262_144, max_answer: 131_072, price_cny: (0.2, 0.04, 2.0) },
    ModelSpec { prefix: "doubao-seed-2-0-pro", context: 262_144, max_answer: 131_072, price_cny: (3.2, 0.64, 16.0) },
    ModelSpec { prefix: "doubao-seed-2-0-code", context: 262_144, max_answer: 131_072, price_cny: (3.2, 0.64, 16.0) },
    ModelSpec {
        prefix: "doubao-seed-character-260628",
        context: 131_072,
        max_answer: 32_768,
        price_cny: (0.8, 0.16, 2.0),
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
struct ArkCaps;

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
        let req = crate::openrouter::options::overlay(&req, &user);
        if req.providers.is_some() {
            return Err(LlmError::request("`p=` (provider choice) does not apply to Volcengine Ark"));
        }
        crate::request::validate(input.model, &req, &ArkCaps).map_err(LlmError::request)?;
        let (messages, notes) = chat::convert_history_with(input.history, input.model, None, blobs, &FLAVOR).await?;
        let mut body = json!({
            "model": input.model,
            "messages": messages,
            "stream": true,
            // Cumulative usage on every chunk: tokens and cost update while streaming.
            "stream_options": {"include_usage": true, "chunk_include_usage": true},
        });
        let mut applied = Vec::new();
        let mut settings = Vec::new();
        // Reasoning: the highest level by default; e=none turns thinking off.
        match req.effort.as_deref() {
            Some("none") | Some("minimal") => {
                body["thinking"] = json!({"type": "disabled"});
                body["reasoning_effort"] = json!("minimal");
                settings.push("thinking off".to_string());
            }
            e => {
                let e = e.unwrap_or("high");
                body["thinking"] = json!({"type": "enabled"});
                body["reasoning_effort"] = json!(e);
                settings.push(format!("effort {e}"));
            }
        }
        applied.push(Applied {
            source: if user.effort.is_some() { "option" } else { "default" }.into(),
            patch: json!({"thinking": body["thinking"], "reasoning_effort": body["reasoning_effort"]}),
        });
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
        if let Some(t) = &req.tier {
            body["service_tier"] = json!(t);
            settings.push(format!("tier {t}"));
            applied.push(Applied { source: "option".into(), patch: json!({"service_tier": t}) });
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
        let mut priced = PricingSink { inner: sink, model: &p.model, cny_per_usd: self.cfg.cny_per_usd };
        let crate::chatstream::Streamed { result, parser, header_id } = crate::chatstream::run_with(
            rb,
            &p.url,
            &p.body,
            self.cfg.idle_timeout,
            Some("x-request-id"),
            error_from_json,
            ChatParser::new(),
            &mut priced,
            observer,
            cancel,
        )
        .await;
        let outcome = CallOutcome { upstream_id: header_id, ..Default::default() };
        match result {
            Err(e) => (Err(e), outcome),
            Ok(()) => {
                if p.body.get("web_search_options").is_some() && parser.no_search() {
                    priced.event(StreamEvent::Notice(NO_SEARCH.into()));
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

/// Estimated cost in USD from the list price (lowest input tier), plus the
/// searches the usage reports (`tool_usage.web_search`, [`WEB_SEARCH_CNY`] each).
pub fn estimate_cost(model: &str, u: &Usage, cny_per_usd: f64) -> Option<f64> {
    let s = model_spec(model)?;
    let input = u.input_tokens? as f64;
    let output = u.output_tokens? as f64;
    let cached = u.cached_input_tokens.unwrap_or(0) as f64;
    let searches = u.raw.pointer("/tool_usage/web_search").and_then(Value::as_u64).unwrap_or(0) as f64;
    let (pi, pc, po) = s.price_cny;
    let cny = ((input - cached).max(0.0) * pi + cached * pc + output * po) / 1e6 + searches * WEB_SEARCH_CNY;
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
    fn new() -> Self {
        let mut inner = ChunkParser::default();
        inner.map_error = Some(error_from_json);
        Self { inner, searches: 0, text: false, usage: false, cited: false, finish: None }
    }

    /// The answer ended normally (or at the output limit) and the usage
    /// reports no executed search (and nothing was cited).
    fn no_search(&self) -> bool {
        matches!(self.finish.as_deref(), Some("stop" | "length")) && self.usage && self.searches == 0 && !self.cited
    }
}

impl EventParser for ChatParser {
    fn event(&mut self, _name: Option<&str>, v: &Value) -> Result<Vec<StreamEvent>, LlmError> {
        let evs = self.inner.chunk(v)?;
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

struct PricingSink<'a> {
    inner: &'a mut dyn EventSink,
    model: &'a str,
    cny_per_usd: f64,
}

impl EventSink for PricingSink<'_> {
    fn event(&mut self, ev: StreamEvent) {
        match ev {
            StreamEvent::Usage(mut u) if u.cost_usd.is_none() => {
                u.cost_usd = estimate_cost(self.model, &u, self.cny_per_usd);
                u.cost_source = if u.cost_usd.is_some() { CostSource::Estimated } else { CostSource::Unknown };
                self.inner.event(StreamEvent::Usage(u));
            }
            other => self.inner.event(other),
        }
    }
}

/// Classify Ark's error objects (docs/research/doubao.md §7).
pub fn error_from_json(err: &Value, http_status: Option<u16>, retry_after: Option<u64>, kind: ErrorKind) -> LlmError {
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
        let c = estimate_cost("doubao-seed-2-1-pro-260915", &u, 7.0).unwrap();
        assert!((c - 9.0 / 7.0).abs() < 1e-9);
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
        // Defaults without options: max effort, explicit max_tokens (answer only, capped).
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
        let mut p = ChatParser::new();
        let mut acc = crate::Accumulator::new();
        let mut all = Vec::new();
        let model = MODEL.to_string();
        let mut priced = |e: StreamEvent| all.push(e);
        let mut sink = PricingSink { inner: &mut priced, model: &model, cny_per_usd: 7.0 };
        for c in chunks {
            for e in p.event(None, c).unwrap() {
                sink.event(e);
            }
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
        let c = estimate_cost(MODEL, &u, 7.0).unwrap();
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
            "1 image generated in an earlier turn cannot be sent: assistant messages here carry text only"
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
