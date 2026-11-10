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
//! - Images default to `detail: "xhigh"` (up to 9 MP).
//! - Errors are `{"error":{"code","message","param","type"}}` with string codes.
//! - No cost in responses: estimated from a CNY price table.

use std::sync::Arc;
use std::time::Duration;

use omni_core::Usage;
use omni_core::json::merge_patch;
use omni_core::trigger::OptionToken;
use omni_core::usage::CostSource;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::error::{ErrorKind, LlmError, Retry, retry_for_status};
use crate::event::StreamEvent;
use crate::openrouter::chat::{self, Flavor};
use crate::openrouter::options::Applied;
use crate::{BlobLoader, CallInput, CallOutcome, EventSink, Observer, Prepared};

pub const DIALECT: &str = "ark.chat";
pub const DEFAULT_BASE_URL: &str = "https://ark.cn-beijing.volces.com/api/v3";

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

pub const FLAVOR: Flavor = Flavor { dialect: DIALECT, image_detail: "xhigh", replay };

/// Options understood for Ark (anything else is rejected, never silently ignored).
pub const OPTIONS_HELP: &str = "\
Volcengine Ark (ark:…) options:
e=<none|minimal|low|medium|high>   reasoning_effort (default high; minimal = no thinking)
nothink                            thinking disabled (thinking.type=disabled, effort minimal)
max=<n>                            max_tokens (answer only; default: model max, capped)
t=<x> / top_p=<x>                  temperature / top_p
detail=<low|high|xhigh>            image detail (default xhigh)
s                                  Doubao Search (web_search_options; must be enabled in the Ark console)
json                               response_format json_object
tier=<default|fast|flex|auto>      service_tier
pure                               no automatic defaults
{json}                             JSON merge patch applied to the request body last";

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
        config_presets: &[(String, Value)],
        blobs: &dyn BlobLoader,
    ) -> Result<Prepared, LlmError> {
        let (messages, notes) = chat::convert_history_with(input.history, input.model, None, blobs, &FLAVOR).await?;
        let mut body = json!({
            "model": input.model,
            "messages": messages,
            "stream": true,
            "stream_options": {"include_usage": true},
        });
        let mut applied: Vec<Applied> = Vec::new();
        let mut warnings = Vec::new();
        let mut apply = |body: &mut Value, source: &str, patch: Value| {
            merge_patch(body, &patch);
            applied.push(Applied { source: source.to_string(), patch });
        };
        let pure = input.options.iter().any(|o| matches!(o, OptionToken::Flag(f) if f == "pure"));
        let auto_max = input.max_tokens_override.or_else(|| self.auto_max_tokens(input));
        if !pure {
            let mut p = json!({"thinking": {"type": "enabled"}, "reasoning_effort": "high"});
            if let Some(m) = auto_max {
                p["max_tokens"] = json!(m);
            }
            apply(&mut body, "auto-max", p);
        }
        for (pattern, patch) in config_presets {
            apply(&mut body, &format!("preset:{pattern}"), patch.clone());
        }
        let mut detail: Option<String> = None;
        let mut json_patches = Vec::new();
        for o in input.options {
            let (source, patch) = match o {
                OptionToken::Json(v) => {
                    json_patches.push(v.clone());
                    continue;
                }
                OptionToken::Flag(f) => match f.as_str() {
                    "pure" => continue,
                    "nothink" => (
                        "option:nothink".to_string(),
                        json!({"thinking": {"type": "disabled"}, "reasoning_effort": "minimal"}),
                    ),
                    "s" | "search" | "web" => (format!("option:{f}"), json!({"web_search_options": {}})),
                    "json" => ("option:json".into(), json!({"response_format": {"type": "json_object"}})),
                    "debug" => {
                        warnings.push("`+debug`: Ark has no upstream body echo; the request is shown by /info".into());
                        continue;
                    }
                    other => {
                        return Err(LlmError::request(format!("unknown option `+{other}` for Ark\n\n{OPTIONS_HELP}")));
                    }
                },
                OptionToken::KeyValue(k, v) => {
                    let num = |v: &str| -> Result<Value, LlmError> {
                        v.parse::<f64>()
                            .map(|x| json!(x))
                            .map_err(|_| LlmError::request(format!("`+{k}=` needs a number, got `{v}`")))
                    };
                    let src = format!("option:{k}={v}");
                    match k.as_str() {
                        "e" | "effort" => (src, json!({"reasoning_effort": v})),
                        "max" | "max_tokens" => {
                            let n: u64 = v
                                .parse()
                                .map_err(|_| LlmError::request(format!("`+{k}=` needs an integer, got `{v}`")))?;
                            (src, json!({"max_tokens": n}))
                        }
                        "t" | "temperature" => (src, json!({"temperature": num(v)?})),
                        "top_p" => (src, json!({"top_p": num(v)?})),
                        "tier" | "service_tier" => (src, json!({"service_tier": v})),
                        "detail" => {
                            detail = Some(v.clone());
                            continue;
                        }
                        other => {
                            return Err(LlmError::request(format!(
                                "unknown option `+{other}=` for Ark\n\n{OPTIONS_HELP}"
                            )));
                        }
                    }
                }
            };
            apply(&mut body, &source, patch);
        }
        if let Some(d) = detail {
            set_image_detail(&mut body, &d);
            applied.push(Applied {
                source: format!("option:detail={d}"),
                patch: json!({"messages[].image_url.detail": d}),
            });
        }
        for p in json_patches {
            merge_patch(&mut body, &p);
            applied.push(Applied { source: "json".into(), patch: p });
        }
        let auto_max_tokens = auto_max.filter(|v| body.get("max_tokens").and_then(Value::as_u64) == Some(*v));
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
            auto_max_tokens,
            long_decode: false,
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
        let crate::chatstream::Streamed { result, parser, header_id } = crate::chatstream::run(
            rb,
            &p.url,
            &p.body,
            self.cfg.idle_timeout,
            Some("x-request-id"),
            error_from_json,
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

/// Set `image_url.detail` on every image part.
fn set_image_detail(body: &mut Value, detail: &str) {
    let Some(msgs) = body.get_mut("messages").and_then(Value::as_array_mut) else { return };
    for m in msgs {
        if let Some(parts) = m.get_mut("content").and_then(Value::as_array_mut) {
            for p in parts {
                if let Some(iu) = p.get_mut("image_url").and_then(Value::as_object_mut) {
                    iu.insert("detail".into(), json!(detail));
                }
            }
        }
    }
}

/// Estimated cost in USD from the list price (lowest input tier).
pub fn estimate_cost(model: &str, u: &Usage, cny_per_usd: f64) -> Option<f64> {
    let s = model_spec(model)?;
    let input = u.input_tokens? as f64;
    let output = u.output_tokens? as f64;
    let cached = u.cached_input_tokens.unwrap_or(0) as f64;
    let (pi, pc, po) = s.price_cny;
    let cny = ((input - cached).max(0.0) * pi + cached * pc + output * po) / 1e6;
    (cny_per_usd > 0.0).then(|| cny / cny_per_usd)
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
        let opts = vec![OptionToken::Flag("nothink".into()), OptionToken::KeyValue("max".into(), "100".into())];
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
        assert_eq!(p.body["max_tokens"], 100);
        assert_eq!(p.body["stream_options"]["include_usage"], true);
        assert!(p.body.get("user").is_none(), "Ark Chat has no `user` field");
        // Defaults without options: max effort, explicit max_tokens (answer only, capped).
        let input = CallInput { options: &[], ..input };
        let p = ark().prepare_chat(&input, &[], &NoBlobs).await.unwrap();
        assert_eq!(p.body["reasoning_effort"], "high");
        assert_eq!(p.body["max_tokens"], 65_536);
        assert_eq!(p.auto_max_tokens, Some(65_536));
        // Unknown options are rejected, never ignored.
        let bad = vec![OptionToken::Flag("frobnicate".into())];
        assert!(ark().prepare_chat(&CallInput { options: &bad, ..input }, &[], &NoBlobs).await.is_err());
    }

    #[test]
    fn replay_keeps_encrypted_reasoning() {
        let p = json!({"role": "assistant", "content": "hi", "reasoning": "sum", "encrypted_content": "abc"});
        let m = replay(&p);
        assert_eq!(m["reasoning_content"], "sum");
        assert_eq!(m["encrypted_content"], "abc");
    }
}
