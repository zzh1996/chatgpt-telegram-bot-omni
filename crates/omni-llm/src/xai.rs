//! xAI's own API: the Responses API (`POST {base}/responses`, base
//! `https://api.x.ai/v1`), used statelessly (docs/research/xai_deepseek.md
//! Part A; docs.x.ai pages and `openapi.json` fetched 2026-11-12). The wire
//! format is OpenAI's: see [`crate::openai::wire`].
//!
//! What the bot sends:
//! - `store: false` + `include: ["reasoning.encrypted_content"]`; earlier
//!   turns of the same model replay all their output items verbatim
//!   (reasoning with its encrypted content, messages, server tool calls).
//! - Defaults (D3): the model's highest `reasoning.effort` (grok-4.7/4.6
//!   `xhigh`, grok-4.5 `high`, grok-4.3 `xhigh`); models without effort
//!   levels get none. No `reasoning.summary`: xAI takes it "only for
//!   compatibility" and always returns the detailed summary.
//!   `max_output_tokens` counts visible output only and defaults to 128,000
//!   (no output limit is documented): it is sent up to the context window.
//!   Images at `detail: "high"`.
//! - `+s` → `web_search` (+ `include: web_search_call.action.sources`), `+c`
//!   → `code_interpreter` (+ `include: code_interpreter_call.outputs`). xAI's
//!   `x_search` is not in the option vocabulary; there is no URL fetching.
//!   `tier=priority` / `tier=fast` (the same, 2× price) → `service_tier`.
//! - `safety_identifier` = the hashed user, `prompt_cache_key` = the
//!   conversation (routes it to the same server: cache hits).
//! - Files: PDF and text-based files as inline `input_file`s, which xAI
//!   reads with its `attachment_search` tool; other documents, audio and
//!   video are errors.
//! - Cost: as billed (`usage.cost_in_usd_ticks`), else estimated from the
//!   price table. Usage `output_tokens` exclude reasoning on xAI: normalized
//!   to include it, as everywhere else.
//! - Errors: HTTP bodies are `{"code", "error"}` (probed 2026-11-12), kept
//!   verbatim; billing 429s ("credits", "spending limit") are never retried.

use omni_core::Usage;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::error::{ErrorKind, LlmError, Retry, retry_for_status};
use crate::openai::wire::{self, Platform, Price, Scale};
use crate::openrouter::options::{Applied, overlay};
use crate::provider::{DirectConfig, Provider};
use crate::request::{self, Requested, RouteCaps, Tool};
use crate::{BlobLoader, CallInput, CallOutcome, EventSink, Observer, Prepared, budget};

pub const DIALECT: &str = "xai.responses";

const NAME: &str = "xAI";

/// Limits, reasoning levels and list prices of a Grok model.
#[derive(Debug, Clone, Copy)]
pub struct ModelSpec {
    pub id: &'static str,
    /// Other ids of the same model.
    pub aliases: &'static [&'static str],
    /// Context window.
    pub context: u64,
    /// `reasoning.effort` levels, highest first; `None`: no effort parameter.
    pub efforts: Option<&'static [&'static str]>,
    /// A reasoning model (returns encrypted reasoning for replay).
    pub reasoning: bool,
    /// Prompts under 200K tokens (USD per 1M tokens); from 200K on every
    /// token of the request costs twice as much.
    pub price: Price,
}

const fn price(input: f64, cached: f64, output: f64) -> Price {
    Price { input, cached, cache_write: None, output }
}

/// xAI's text models, 2026-11-12: docs.x.ai/developers/models and the model
/// pages (limits, efforts, prices), the reasoning guide (grok-4.5 treats
/// `xhigh` as `high`, so it is not offered). `grok-4.20-multi-agent` is left
/// out: its effort sets the number of agents and it takes no output limit.
pub const MODELS: &[ModelSpec] = &[
    ModelSpec {
        id: "grok-4.7",
        aliases: &[],
        context: 500_000,
        efforts: Some(&["xhigh", "high", "medium", "low"]),
        reasoning: true,
        price: price(2.0, 0.5, 6.0),
    },
    ModelSpec {
        id: "grok-4.6",
        aliases: &[],
        context: 500_000,
        efforts: Some(&["xhigh", "high", "medium", "low"]),
        reasoning: true,
        price: price(2.0, 0.5, 6.0),
    },
    ModelSpec {
        id: "grok-4.5",
        aliases: &["grok-4.5-latest", "grok-build-latest"],
        context: 500_000,
        efforts: Some(&["high", "medium", "low"]),
        reasoning: true,
        price: price(2.0, 0.3, 6.0),
    },
    ModelSpec {
        id: "grok-4.3",
        aliases: &["grok-4.3-latest"],
        context: 1_000_000,
        efforts: Some(&["xhigh", "high", "medium", "low", "none"]),
        reasoning: true,
        price: price(1.25, 0.2, 2.5),
    },
    ModelSpec {
        id: "grok-4.20-0309-reasoning",
        aliases: &["grok-4.20", "grok-4.20-reasoning", "grok-4.20-0309", "grok-4.20-reasoning-latest"],
        context: 1_000_000,
        efforts: None,
        reasoning: true,
        price: price(1.25, 0.2, 2.5),
    },
    ModelSpec {
        id: "grok-4.20-0309-non-reasoning",
        aliases: &["grok-4.20-non-reasoning", "grok-4.20-non-reasoning-latest"],
        context: 1_000_000,
        efforts: None,
        reasoning: false,
        price: price(1.25, 0.2, 2.5),
    },
    ModelSpec {
        id: "grok-build-0.1",
        aliases: &["grok-code-fast-1", "grok-code-fast", "grok-code-fast-1-0825"],
        context: 256_000,
        efforts: None,
        reasoning: true,
        price: price(1.0, 0.2, 2.0),
    },
];

/// The table entry of `model` (its id or an alias).
pub fn model_spec(model: &str) -> Option<&'static ModelSpec> {
    MODELS.iter().find(|s| s.id == model || s.aliases.contains(&model))
}

/// Every effort value xAI documents: for models not in the table, xAI checks.
const ANY_EFFORT: &[&str] = &["xhigh", "high", "medium", "low", "none"];

/// What xAI offers for a model (the vendor's own API: official = route).
struct Caps(Option<&'static ModelSpec>);

impl RouteCaps for Caps {
    fn name(&self) -> &str {
        NAME
    }
    fn tool(&self, tool: Tool) -> bool {
        matches!(tool, Tool::Search | Tool::Code)
    }
    fn efforts(&self) -> Option<Vec<String>> {
        let levels = match self.0 {
            Some(s) => s.efforts?,
            None => ANY_EFFORT,
        };
        Some(levels.iter().map(|e| e.to_string()).collect())
    }
    fn can_disable_reasoning(&self) -> bool {
        self.efforts().is_some_and(|l| l.iter().any(|e| e == "none"))
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
        // Priority processing: `"fast"` and `"priority"` are interchangeable.
        vec!["priority".into(), "fast".into()]
    }
}

pub struct Xai {
    cfg: DirectConfig,
    http: reqwest::Client,
}

impl Xai {
    pub fn new(cfg: DirectConfig) -> anyhow::Result<Self> {
        let http = cfg.http()?;
        Ok(Self { cfg, http })
    }

    async fn build(
        &self,
        input: &CallInput<'_>,
        presets: &[Requested],
        blobs: &dyn BlobLoader,
    ) -> Result<Prepared, LlmError> {
        let model = input.model;
        let spec = model_spec(model);
        let user = request::parse(input.options).map_err(LlmError::request)?;
        let mut req = Requested::default();
        for p in presets {
            req = overlay(&req, p);
        }
        let req = overlay(&req, &user);
        if req.providers.is_some() {
            return Err(LlmError::request(format!("`p=` (provider choice) does not apply to {NAME}")));
        }
        request::validate(model, &req, &Caps(spec)).map_err(LlmError::request)?;

        let inputs = wire::Inputs { platform: Platform::Xai, model, image_detail: "high" };
        let mut conv = wire::convert_history(input.history, &inputs, blobs).await?;
        let mut body = json!({"model": model, "input": conv.input, "stream": true, "store": false});
        let src = |by_user: bool| if by_user { "option" } else { "default" };
        let mut applied = Vec::new();
        let mut settings = Vec::new();
        let mut include = Vec::new();

        let reasoning = match (spec, req.effort.as_deref()) {
            (_, Some("none")) => Some(("none".to_string(), "reasoning off".to_string())),
            (Some(s), e) => s
                .efforts
                .and_then(|l| l.first())
                .map(|top| e.unwrap_or(top))
                .map(|e| (e.to_string(), format!("effort {e}"))),
            // Not in the table: only what the user asked for.
            (None, e) => e.map(|e| (e.to_string(), format!("effort {e}"))),
        };
        if let Some((effort, desc)) = reasoning {
            body["reasoning"] = json!({"effort": effort});
            applied.push(Applied {
                source: src(user.effort.is_some()).into(),
                patch: json!({"reasoning": body["reasoning"]}),
            });
            settings.push(desc);
        }
        // Stateless replay needs the encrypted reasoning (grok-4.7 returns it anyway).
        if spec.is_none_or(|s| s.reasoning) {
            include.push("reasoning.encrypted_content");
            applied
                .push(Applied { source: "default".into(), patch: json!({"include": ["reasoning.encrypted_content"]}) });
        }

        // Visible output only (reasoning is not counted), up to the context window.
        let auto_max = input.max_tokens_override.or_else(|| {
            let s = spec?;
            let prompt = budget::estimate_prompt(input.history, input.known_context);
            budget::choose(Some(s.context), Some(s.context), self.cfg.max_tokens_cap, prompt)
        });
        if let Some(m) = auto_max {
            body["max_output_tokens"] = json!(m);
            applied.push(Applied { source: "budget".into(), patch: json!({"max_output_tokens": m}) });
        }

        let mut tools = Vec::new();
        for &t in &req.tools {
            let (tool, inc) = match t {
                Tool::Search => (json!({"type": "web_search"}), "web_search_call.action.sources"),
                Tool::Code => (json!({"type": "code_interpreter"}), "code_interpreter_call.outputs"),
                Tool::Fetch | Tool::Maps => {
                    return Err(LlmError::request(format!("`{model}` has no {} (`+{}`)", t.describe(), t.letter())));
                }
            };
            applied.push(Applied {
                source: src(user.tools.contains(&t)).into(),
                patch: json!({"tools": [tool], "include": [inc]}),
            });
            settings.push(t.describe().to_string());
            tools.push(tool);
            include.push(inc);
        }
        if !tools.is_empty() {
            body["tools"] = json!(tools);
        }
        if !include.is_empty() {
            body["include"] = json!(include);
        }
        if let Some(t) = req.tier.as_deref().filter(|t| !matches!(*t, "default" | "auto")) {
            body["service_tier"] = json!(t);
            applied.push(Applied { source: src(user.tier.is_some()).into(), patch: json!({"service_tier": t}) });
            settings.push(format!("tier {t}"));
        }
        if conv.images > 0 {
            applied.push(Applied { source: "default".into(), patch: json!({"input_image": {"detail": "high"}}) });
        }
        if conv.files > 0 {
            conv.notes.push(
                "📎 xAI reads attached files through its attachment_search tool (a server-side search, billed per call)"
                    .into(),
            );
        }
        if let Some(u) = &input.user {
            body["safety_identifier"] = json!(u);
        }
        if let Some(s) = &input.session_id {
            body["prompt_cache_key"] = json!(s);
        }
        let mut warnings = Vec::new();
        if spec.is_none() {
            warnings.push(format!(
                "`{model}` is not in the bot's xAI model table: limits, price and reasoning levels unknown \
                 (the bot's defaults are not sent; max_output_tokens {})",
                body.get("max_output_tokens").map(|v| v.to_string()).unwrap_or_else(|| "not set".into())
            ));
        }
        Ok(Prepared {
            dialect: DIALECT,
            model: model.to_string(),
            url: self.cfg.url("/responses"),
            body,
            applied,
            warnings,
            notes: conv.notes,
            show_debug: false,
            auto_max_tokens: auto_max,
            long_decode: false,
            settings,
        })
    }
}

const WIRE: wire::Wire =
    wire::Wire { platform: Platform::Xai, price: estimate_cost, map_error: error_from_json, refine };

#[async_trait::async_trait]
impl Provider for Xai {
    fn id(&self) -> &'static str {
        "xai"
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
        self.build(input, presets, blobs).await
    }
    async fn stream(
        &self,
        p: &Prepared,
        sink: &mut dyn EventSink,
        observer: &dyn Observer,
        cancel: &CancellationToken,
    ) -> (Result<Value, LlmError>, CallOutcome) {
        wire::stream(&self.http, &self.cfg.api_key, self.cfg.idle_timeout, &WIRE, p, sink, observer, cancel).await
    }
}

// ------------------------------------------------------------------ cost

/// From this many prompt tokens on, every token of the request costs 2×.
const LONG_CONTEXT_FROM: u64 = 200_000;
/// web_search, code_interpreter, attachment_search: $5 / 1k calls.
const TOOL_CALL: f64 = 0.005;

/// Estimated cost in USD, for responses without `cost_in_usd_ticks`: tokens
/// (long context, priority 2× when the response reports it) and the server
/// tool calls in `usage.server_side_tool_usage_details`.
pub fn estimate_cost(model: &str, response: &Value, u: &Usage) -> Option<f64> {
    let s = model_spec(model)?;
    let mut m = if u.input_tokens? >= LONG_CONTEXT_FROM { 2.0 } else { 1.0 };
    if matches!(response.get("service_tier").and_then(Value::as_str), Some("priority" | "fast")) {
        m *= 2.0;
    }
    let tokens = wire::token_cost(&s.price, u, 0, Scale::ONE.times(m))?;
    let n = |k: &str| {
        u.raw.pointer(&format!("/server_side_tool_usage_details/{k}")).and_then(Value::as_u64).unwrap_or(0) as f64
    };
    let tools = (n("web_search_calls") + n("code_interpreter_calls") + n("document_search_calls")) * TOOL_CALL
        + n("file_search_calls") * 0.0025
        + n("x_posts_fetched") * 0.005
        + n("x_users_fetched") * 0.01;
    Some(tokens + tools)
}

// ------------------------------------------------------------------ errors

/// Classify xAI's errors. HTTP bodies are `{"code": "...", "error": "<message>"}`
/// (the HTTP layer passes the `error` string; [`refine`] adds the code);
/// in-band errors are objects with `code` and `message`.
pub fn error_from_json(err: &Value, status: Option<u16>, retry_after: Option<u64>, kind: ErrorKind) -> LlmError {
    let text = |k: &str| err.get(k).and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string);
    let (code, message) = match err {
        Value::String(s) => (None, s.clone()),
        _ => (text("code"), text("message").or_else(|| text("error")).unwrap_or_else(|| "(no message)".into())),
    };
    let lower = message.to_ascii_lowercase();
    // "Your team … has either used all available credits or reached its monthly spending limit" (HTTP 429).
    let billing = ["credits", "spending limit"].iter().any(|k| lower.contains(k));
    let context = ["prompt length", "context length", "context window", "maximum context", "prompt is too long"]
        .iter()
        .any(|k| lower.contains(k));
    let retry = if billing || context {
        Retry::Never
    } else if let Some(s) = status {
        retry_for_status(s, retry_after)
    } else {
        wire::in_band_retry(code.as_deref())
    };
    let error_type = if context { Some("context_length_exceeded".to_string()) } else { code };
    LlmError { kind, retry, status, error_type, message, details: Some(err.clone()), partial_output: false }
}

/// HTTP errors: the `code` of the body (kept whole in `details`) as the type.
fn refine(mut e: LlmError) -> LlmError {
    if e.error_type.is_none()
        && let Some(code) = e.details.as_ref().and_then(|d| d.get("code")).and_then(Value::as_str)
    {
        e.error_type = Some(code.to_string());
    }
    e
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::StreamEvent;
    use crate::openai::wire::tests::{MemBlobs, file, photo, serve, user};
    use omni_core::Turn;
    use omni_core::content::{Part, ReasoningKind};
    use omni_core::trigger::OptionToken;
    use omni_core::usage::CostSource;

    fn cfg(base: &str) -> DirectConfig {
        DirectConfig {
            api_key: "xai-test".into(),
            base_url: base.into(),
            connect_timeout: Duration::from_secs(5),
            idle_timeout: Duration::from_secs(5),
            max_tokens_cap: None,
            cny_per_usd: 7.1,
            long_decode_budget: 393_216,
        }
    }

    fn xai() -> Xai {
        Xai::new(cfg(crate::provider::info("xai").unwrap().base_url)).unwrap()
    }

    fn flag(f: &str) -> OptionToken {
        OptionToken::Flag(f.into())
    }

    fn kv(k: &str, v: &str) -> OptionToken {
        OptionToken::KeyValue(k.into(), v.into())
    }

    async fn prep_with(x: &Xai, model: &str, opts: &[OptionToken], history: &[Turn], blobs: &MemBlobs) -> Prepared {
        let input = CallInput {
            model,
            options: opts,
            history,
            session_id: Some("tg-3".into()),
            user: Some("u-hash".into()),
            known_context: None,
            max_tokens_override: None,
        };
        x.prepare(&input, &[], blobs).await.unwrap()
    }

    async fn prep(model: &str, opts: &[OptionToken]) -> Prepared {
        prep_with(&xai(), model, opts, &[Turn::user_text("hi")], &MemBlobs(vec![])).await
    }

    async fn err(model: &str, opts: &[OptionToken]) -> String {
        let input = CallInput {
            model,
            options: opts,
            history: &[Turn::user_text("hi")],
            session_id: None,
            user: None,
            known_context: None,
            max_tokens_override: None,
        };
        xai().prepare(&input, &[], &MemBlobs(vec![])).await.unwrap_err().message
    }

    #[test]
    fn table_covers_the_configured_aliases() {
        // OpenRouter ids of config/omni.example.toml → official ids.
        for (or, official) in
            [("x-ai/grok-4.7", "grok-4.7"), ("x-ai/grok-4.6", "grok-4.6"), ("x-ai/grok-4.5", "grok-4.5")]
        {
            assert_eq!(model_spec(official).map(|s| s.id), Some(official), "{or}");
        }
        assert_eq!(model_spec("grok-4.20").unwrap().id, "grok-4.20-0309-reasoning");
        assert_eq!(model_spec("grok-code-fast-1").unwrap().id, "grok-build-0.1");
        assert!(model_spec("grok-4.20-multi-agent").is_none());
    }

    #[tokio::test]
    async fn defaults_are_the_most_the_model_offers() {
        let p = prep("grok-4.7", &[]).await;
        assert_eq!(p.url, "https://api.x.ai/v1/responses");
        assert_eq!(p.dialect, DIALECT);
        let b = &p.body;
        assert_eq!(b["reasoning"], json!({"effort": "xhigh"}));
        assert_eq!(b["include"], json!(["reasoning.encrypted_content"]));
        assert_eq!((b["store"].clone(), b["stream"].clone()), (json!(false), json!(true)));
        assert_eq!((b["safety_identifier"].clone(), b["prompt_cache_key"].clone()), (json!("u-hash"), json!("tg-3")));
        // Visible output, up to the context window less the prompt and a margin.
        let m = b["max_output_tokens"].as_u64().unwrap();
        assert!(m > 480_000 && m < 500_000, "{m}");
        assert_eq!(p.auto_max_tokens, Some(m));
        let keys: Vec<&str> = b.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            [
                "model",
                "input",
                "stream",
                "store",
                "reasoning",
                "max_output_tokens",
                "include",
                "safety_identifier",
                "prompt_cache_key"
            ]
        );
        assert_eq!(p.settings, vec!["effort xhigh"]);
        assert!(p.warnings.is_empty() && p.notes.is_empty());

        assert_eq!(prep("grok-4.6", &[]).await.body["reasoning"]["effort"], "xhigh");
        assert_eq!(prep("grok-4.5", &[]).await.body["reasoning"]["effort"], "high");
        assert_eq!(prep("grok-4.3", &[]).await.body["reasoning"]["effort"], "xhigh");
        // No effort parameter: nothing sent; non-reasoning: no encrypted reasoning either.
        let p = prep("grok-4.20", &[]).await;
        assert!(p.body.get("reasoning").is_none());
        assert_eq!(p.body["include"], json!(["reasoning.encrypted_content"]));
        let p = prep("grok-4.20-0309-non-reasoning", &[]).await;
        assert!(p.body.get("reasoning").is_none() && p.body.get("include").is_none());
        // The configured cap.
        let capped = Xai::new(DirectConfig { max_tokens_cap: Some(393_216), ..cfg("https://api.x.ai/v1") }).unwrap();
        let p = prep_with(&capped, "grok-4.7", &[], &[Turn::user_text("hi")], &MemBlobs(vec![])).await;
        assert_eq!(p.body["max_output_tokens"], 393_216);
        // Unknown models: no guessed defaults, a warning.
        let p = prep("grok-5", &[]).await;
        assert!(p.body.get("reasoning").is_none() && p.body.get("max_output_tokens").is_none());
        assert_eq!(p.body["include"], json!(["reasoning.encrypted_content"]));
        assert!(p.warnings[0].contains("not in the bot's xAI model table"), "{}", p.warnings[0]);
    }

    #[tokio::test]
    async fn options_map_to_the_request() {
        let p = prep("grok-4.7", &[flag("sc"), kv("e", "low"), kv("tier", "priority")]).await;
        let b = &p.body;
        assert_eq!(b["reasoning"], json!({"effort": "low"}));
        assert_eq!(b["tools"], json!([{"type": "web_search"}, {"type": "code_interpreter"}]));
        assert_eq!(
            b["include"],
            json!(["reasoning.encrypted_content", "web_search_call.action.sources", "code_interpreter_call.outputs"])
        );
        assert_eq!(b["service_tier"], "priority");
        assert_eq!(p.settings, vec!["effort low", "web search", "code execution", "tier priority"]);
        assert_eq!(prep("grok-4.7", &[kv("tier", "fast")]).await.body["service_tier"], "fast");
        assert_eq!(prep("grok-4.3", &[kv("e", "none")]).await.body["reasoning"], json!({"effort": "none"}));
        // What xAI does not offer.
        assert!(err("grok-4.7", &[flag("u")]).await.contains("has no URL fetching (`+u`)"));
        assert_eq!(err("grok-4.7", &[kv("e", "none")]).await, "`grok-4.7` cannot turn reasoning off (e=none) via xAI");
        assert!(err("grok-4.5", &[kv("e", "xhigh")]).await.contains("levels: high, medium, low"));
        assert!(err("grok-4.20", &[kv("e", "high")]).await.contains("has no reasoning effort levels via xAI"));
        assert_eq!(err("grok-4.7", &[kv("v", "low")]).await, "`grok-4.7` has no verbosity setting");
        assert!(err("grok-4.7", &[kv("tier", "flex")]).await.contains("tiers: priority, fast"));
        assert!(err("grok-4.7", &[kv("budget", "100")]).await.contains("no reasoning token budget"));
        assert!(err("grok-4.7", &[flag("W")]).await.contains("no image size/aspect options"));
        assert_eq!(err("grok-4.7", &[kv("p", "xai")]).await, "`p=` (provider choice) does not apply to xAI");
    }

    #[tokio::test]
    async fn media_and_the_attachment_search_note() {
        let (img, b1) = photo("image/png", b"\x89PNG");
        let (pdf, b2) = file("r.pdf", "application/pdf", b"%PDF");
        let blobs = MemBlobs(vec![b1, b2]);
        let history = vec![user(vec![Part::text("?"), img, pdf])];
        let p = prep_with(&xai(), "grok-4.7", &[], &history, &blobs).await;
        let content = &p.body["input"][0]["content"];
        assert_eq!(content[1]["detail"], "high");
        assert_eq!(content[2]["type"], "input_file");
        assert!(p.notes[0].contains("attachment_search"), "{:?}", p.notes);
        assert!(p.applied.iter().any(|a| a.patch == json!({"input_image": {"detail": "high"}})));
    }

    fn usage(input: u64, output: u64, raw: Value) -> Usage {
        Usage { input_tokens: Some(input), output_tokens: Some(output), raw, ..Default::default() }
    }

    #[test]
    fn cost_estimate_when_none_is_reported() {
        let std = json!({"service_tier": "default"});
        // grok-4.7: $2 in, $6 out under 200K prompt tokens.
        let c = estimate_cost("grok-4.7", &std, &usage(100_000, 10_000, Value::Null)).unwrap();
        assert!((c - (0.2 + 0.06)).abs() < 1e-9, "{c}");
        // From 200K on, every token costs twice; priority doubles again.
        let c = estimate_cost("grok-4.7", &std, &usage(200_000, 10_000, Value::Null)).unwrap();
        assert!((c - 2.0 * (0.4 + 0.06)).abs() < 1e-9, "{c}");
        let c =
            estimate_cost("grok-4.7", &json!({"service_tier": "priority"}), &usage(100_000, 0, Value::Null)).unwrap();
        assert!((c - 0.4).abs() < 1e-9, "{c}");
        // Server tool calls.
        let raw = json!({"server_side_tool_usage_details": {"web_search_calls": 2, "code_interpreter_calls": 1, "x_search_calls": 0,
            "x_posts_fetched": 0, "x_users_fetched": 0, "file_search_calls": 0, "mcp_calls": 0, "document_search_calls": 0, "image_generation_calls": 0}});
        let c = estimate_cost("grok-4.7", &std, &usage(0, 0, raw)).unwrap();
        assert!((c - 0.015).abs() < 1e-9, "{c}");
        assert!(estimate_cost("grok-5", &std, &usage(1, 1, Value::Null)).is_none());
    }

    #[test]
    fn errors_are_classified() {
        let http = |status: u16, retry_after: Option<u64>, body: &str| {
            refine(crate::chatstream::http_error(status, retry_after, body, error_from_json))
        };
        // Probed 2026-11-12 (no key / a wrong key).
        let e = http(401, None, r#"{"code":"unauthenticated:no-credentials","error":"No credentials presented."}"#);
        assert_eq!((e.retry, e.error_type.as_deref()), (Retry::Never, Some("unauthenticated:no-credentials")));
        assert_eq!(e.message, "No credentials presented.");
        let body = r#"{"code":"invalid-argument","error":"Incorrect API key provided. You can obtain an API key from https://console.x.ai."}"#;
        let e = http(400, None, body);
        assert_eq!((e.retry, e.error_type.as_deref()), (Retry::Never, Some("invalid-argument")));
        assert_eq!(e.details, Some(serde_json::from_str::<Value>(body).unwrap()), "the whole body is kept");
        // Out of credits (HTTP 429): never retried; plain rate limits wait.
        let e = http(
            429,
            None,
            r#"{"code":"Some resource has been exhausted","error":"Your team 0000 has either used all available credits or reached its monthly spending limit. To continue making API requests, please purchase more credits or raise your spending limit."}"#,
        );
        assert_eq!(e.retry, Retry::Never);
        let e = http(429, Some(3), r#"{"code":"resource-exhausted","error":"Too many requests"}"#);
        assert_eq!(e.retry, Retry::After(3));
        assert_eq!(http(429, None, r#"{"code":"x","error":"rate limited"}"#).retry, Retry::After(20));
        assert_eq!(http(503, None, r#"{"code":"unavailable","error":"overloaded"}"#).retry, Retry::Backoff);
        assert_eq!(http(500, None, "internal error").retry, Retry::Backoff);
        assert_eq!(http(422, None, r#"{"code":"x","error":"invalid field"}"#).retry, Retry::Never);
        let e = http(
            400,
            None,
            r#"{"code":"invalid-argument","error":"This model's maximum prompt length is 500000 but the request contains 612345 tokens."}"#,
        );
        assert!(e.is_context_overflow());
        assert_eq!(e.retry, Retry::Never);
        // In-band.
        let ib = |v: Value| error_from_json(&v, None, None, ErrorKind::InBand);
        assert_eq!(ib(json!({"code": "internal_error", "message": "x"})).retry, Retry::Backoff);
        assert_eq!(ib(json!({"message": "stream failed"})).retry, Retry::Backoff);
        assert_eq!(ib(json!({"code": "permission_denied", "message": "x"})).retry, Retry::Never);
    }

    #[tokio::test]
    async fn streams_end_to_end() {
        let p = prep("grok-4.7", &[flag("s")]).await;
        let events = [
            json!({"type": "response.created", "response": {"id": "5808284d-ae14-9981-9289-73515f67ebda", "model": "grok-4.7", "status": "in_progress", "output": []}}),
            json!({"type": "response.output_item.added", "output_index": 0, "item": {"type": "reasoning", "id": "rs_1", "summary": [], "status": "in_progress"}}),
            json!({"type": "response.reasoning_summary_text.delta", "item_id": "rs_1", "output_index": 0, "summary_index": 0, "delta": "Looking up xAI."}),
            json!({"type": "response.output_item.done", "output_index": 0, "item": {"type": "reasoning", "id": "rs_1", "status": "completed",
                "summary": [{"type": "summary_text", "text": "Looking up xAI."}], "encrypted_content": "ENC"}}),
            json!({"type": "response.output_item.added", "output_index": 1, "item": {"type": "web_search_call", "id": "ws_1", "status": "in_progress"}}),
            json!({"type": "response.output_item.done", "output_index": 1, "item": {"type": "web_search_call", "id": "ws_1", "status": "completed",
                "action": {"type": "search", "query": "xAI", "sources": [{"type": "url", "url": "https://x.ai/company"}]}}}),
            json!({"type": "response.output_text.delta", "output_index": 2, "item_id": "msg_1", "content_index": 0, "delta": "xAI builds Grok.[[1]](https://x.ai/company)"}),
            json!({"type": "response.output_text.annotation.added", "output_index": 2, "item_id": "msg_1", "content_index": 0,
                "annotation": {"type": "url_citation", "url": "https://x.ai/company", "start_index": 16, "end_index": 43, "title": "1"}}),
            json!({"type": "response.completed", "response": {"id": "5808284d-ae14-9981-9289-73515f67ebda", "status": "completed", "service_tier": "default",
                "output": [
                    {"type": "reasoning", "id": "rs_1", "status": "completed", "summary": [{"type": "summary_text", "text": "Looking up xAI."}], "encrypted_content": "ENC"},
                    {"type": "web_search_call", "id": "ws_1", "status": "completed", "action": {"type": "search", "query": "xAI", "sources": [{"type": "url", "url": "https://x.ai/company"}]}},
                    {"type": "message", "id": "msg_1", "role": "assistant", "status": "completed", "content": [{"type": "output_text", "text": "xAI builds Grok.[[1]](https://x.ai/company)", "logprobs": [],
                        "annotations": [{"type": "url_citation", "url": "https://x.ai/company", "start_index": 16, "end_index": 43, "title": "1"}]}]}
                ],
                "usage": {"input_tokens": 900, "input_tokens_details": {"cached_tokens": 100}, "output_tokens": 20, "output_tokens_details": {"reasoning_tokens": 300},
                    "total_tokens": 1220, "num_sources_used": 0, "num_server_side_tools_used": 1, "cost_in_usd_ticks": 158500000,
                    "server_side_tool_usage_details": {"web_search_calls": 1, "x_search_calls": 0, "x_posts_fetched": 0, "x_users_fetched": 0,
                        "code_interpreter_calls": 0, "file_search_calls": 0, "mcp_calls": 0, "document_search_calls": 0, "image_generation_calls": 0}}}}),
        ];
        // xAI's OpenAPI text promises a `data: [DONE]` terminator: both are handled.
        let mut chunks: Vec<String> = events.iter().map(|e| format!("data: {e}\n\n")).collect();
        chunks.push("data: [DONE]\n\n".into());
        let (base, captured) = serve(200, "text/event-stream", "", chunks).await;
        let x = Xai::new(cfg(&base)).unwrap();
        let p = Prepared { url: x.cfg.url("/responses"), ..p };
        let mut evs = Vec::new();
        let mut sink = |e: StreamEvent| evs.push(e);
        let (r, outcome) = x.stream(&p, &mut sink, &crate::NullObserver, &CancellationToken::new()).await;
        let native = r.unwrap();
        assert_eq!(native["output"][0]["encrypted_content"], "ENC");
        assert_eq!(outcome.upstream_id.as_deref(), Some("5808284d-ae14-9981-9289-73515f67ebda"));
        let req = captured.lock().unwrap().take().unwrap();
        assert_eq!((req.path.as_str(), req.auth.as_deref()), ("/v1/responses", Some("Bearer xai-test")));
        assert_eq!(req.body, p.body);
        let usage =
            evs.iter().find_map(|e| if let StreamEvent::Usage(u) = e { Some(u.clone()) } else { None }).unwrap();
        assert_eq!((usage.output_tokens, usage.reasoning_tokens), (Some(320), Some(300)));
        assert!((usage.cost_usd.unwrap() - 0.01585).abs() < 1e-12);
        assert_eq!(usage.cost_source, CostSource::Reported);
        let mut acc = crate::Accumulator::new();
        evs.into_iter().for_each(|e| acc.apply(e));
        let parts = acc.parts();
        assert!(matches!(&parts[0], Part::Reasoning(r) if r.kind == ReasoningKind::Summary));
        assert!(matches!(&parts[1], Part::ToolCall(t) if t.name == "web_search" && t.output.is_some()));
        let Part::Text { text, citations } = &parts[2] else { panic!("{:?}", parts[2]) };
        assert!(text.ends_with("[[1]](https://x.ai/company)"));
        assert_eq!(citations[0].extra, Some(json!({"label": "1"})));
        assert_eq!(acc.finish_reason.as_deref(), Some("stop"));
        // The next turn replays it all verbatim (same model), with the tool's sources.
        let mut turn = acc.turn();
        turn.native = Some(omni_core::content::NativeOutput {
            dialect: DIALECT.into(),
            model: "grok-4.7".into(),
            payload: native.clone(),
            context_tokens: None,
        });
        let history = vec![Turn::user_text("hi"), turn, Turn::user_text("more")];
        let next = prep_with(&xai(), "grok-4.7", &[], &history, &MemBlobs(vec![])).await;
        assert_eq!(next.body["input"].as_array().unwrap()[1..4], native["output"].as_array().unwrap()[..]);
        assert_eq!(next.body["input"].as_array().unwrap().len(), 5);
    }
}
