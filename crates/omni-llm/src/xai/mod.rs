//! xAI's own API: the Responses API (`POST {base}/responses`, base
//! `https://api.x.ai/v1`), used statelessly (docs/research/xai_deepseek.md
//! Part A; docs.x.ai pages and `openapi.json` fetched 2026-11-12). The wire
//! format is OpenAI's: see [`crate::openai::wire`].
//!
//! What the bot sends:
//! - `store: false` + `include: ["reasoning.encrypted_content"]`; earlier
//!   turns of the same model replay all their output items verbatim
//!   (reasoning with its encrypted content, messages, server tool calls).
//!   Other assistant turns are an assistant message with their text and
//!   their media where xAI takes them (a message's content takes
//!   `input_image` and `input_file` whatever its role, `openapi.json`):
//!   JPEG/PNG images, PDF and text files; what it cannot take (video, audio,
//!   other files) is said in a warning. A broken stream is not recovered
//!   from a stored copy as on OpenAI: xAI documents `GET
//!   /v1/responses/{id}` for stored responses, but not that a response whose
//!   stream broke is finished and stored, and advises against storing
//!   requests with images ("the request may fail").
//! - Defaults (D3): the model's highest `reasoning.effort` (grok-4.7/4.6
//!   `xhigh`, grok-4.5 `high`, grok-4.3 `xhigh`); models without effort
//!   levels get none. No `reasoning.summary`: xAI takes it "only for
//!   compatibility" and always returns the detailed summary.
//!   `max_output_tokens` counts visible output only and defaults to 128,000
//!   (no output limit is documented): it is sent up to the context window.
//!   Images at `detail: "high"`.
//! - `+s` → `web_search` (+ `include: web_search_call.action.sources`), `+c`
//!   → `code_interpreter` (+ `include: code_interpreter_call.outputs`), `+i`
//!   → `image_generation` (docs.x.ai "Image Generation Tool": Grok Imagine
//!   images inside the conversation, generated or edited; the tool takes no
//!   size, ratio or quality — Grok picks the ratio, asked for in the prompt —
//!   and streams no previews; its `image_generation_call` items carry the
//!   image and the prompt Grok wrote, and replay in later turns, so a reply
//!   edits in context). xAI's `x_search` is not in the option vocabulary;
//!   there is no URL fetching.
//!   `tier=priority` / `tier=fast` (the same, 2× price) → `service_tier`.
//! - Bare Grok Imagine ids (`grok-imagine-image…`, `grok-imagine-video…`) go
//!   to the Imagine image and video APIs instead ([`imagine`]).
//! - `safety_identifier` = the hashed user, `prompt_cache_key` = the
//!   conversation (routes it to the same server: cache hits).
//! - Files: PDF and text-based files as inline `input_file`s; other
//!   documents, audio and video are errors. xAI's docs say attached files
//!   are read by its `attachment_search` tool (billed per call), but inline
//!   files did not trigger it (2026-11-13): a notice says so only when the
//!   usage reports document searches.
//! - The conversation's exact size after a turn is `usage.context_details`
//!   (input + output of the latest context, also after server-side tools),
//!   kept for budgeting the next turn.
//! - Cost: as billed (`usage.cost_in_usd_ticks`), else estimated from the
//!   price table. Usage `output_tokens` include the reasoning tokens in every
//!   response seen (`total_tokens` = input + output; the REST reference's
//!   example still counts them apart, which the total shows: see
//!   [`wire::usage_from`]).
//! - Errors: HTTP bodies are `{"code", "error"}` (probed 2026-11-12), kept
//!   verbatim; billing 429s ("credits", "spending limit") are never retried.

pub mod imagine;

use std::time::Duration;

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
        matches!(tool, Tool::Search | Tool::Code | Tool::Image)
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
    /// How often video jobs are polled.
    poll: Duration,
}

impl Xai {
    pub fn new(cfg: DirectConfig) -> anyhow::Result<Self> {
        let http = cfg.http()?;
        Ok(Self { cfg, http, poll: imagine::POLL_EVERY })
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
        // Bare Grok Imagine models: the Imagine APIs, no chat model.
        if imagine::is_image_model(model) {
            return imagine::prepare_image(&self.cfg, input, &req, &user, blobs).await;
        }
        if imagine::is_video_model(model) {
            return imagine::prepare_video(&self.cfg, input, &req, &user, blobs).await;
        }
        // Sizes and ratios: `request::official` knows them only for Gemini.
        let mut checked = req.clone();
        checked.image_size = None;
        checked.aspect_ratio = None;
        request::validate(model, &checked, &Caps(spec)).map_err(LlmError::request)?;
        if req.image_size.is_some() || req.aspect_ratio.is_some() {
            return Err(LlmError::request(if req.tools.contains(&Tool::Image) {
                "xAI's image generation tool takes no image size or aspect ratio: Grok picks the ratio (ask for it \
                 in the prompt), or use a grok-imagine-image model"
                    .to_string()
            } else {
                format!("`{model}` has no image size/aspect options")
            }));
        }

        let inputs = wire::Inputs { platform: Platform::Xai, model, image_detail: "high" };
        let conv = wire::convert_history(input.history, &inputs, blobs).await?;
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
                Tool::Search => (json!({"type": "web_search"}), Some("web_search_call.action.sources")),
                Tool::Code => (json!({"type": "code_interpreter"}), Some("code_interpreter_call.outputs")),
                // The image comes in the `image_generation_call` item itself.
                Tool::Image => (json!({"type": "image_generation"}), None),
                Tool::Fetch | Tool::Maps => {
                    return Err(LlmError::request(format!("`{model}` has no {} (`+{}`)", t.describe(), t.letter())));
                }
            };
            let patch = match inc {
                Some(inc) => json!({"tools": [tool], "include": [inc]}),
                None => json!({"tools": [tool]}),
            };
            applied.push(Applied { source: src(user.tools.contains(&t)).into(), patch });
            settings.push(t.describe().to_string());
            tools.push(tool);
            include.extend(inc);
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
        warnings.extend(conv.warnings);
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
        model_spec(model).is_some() || imagine::image_spec(model).is_some() || imagine::video_spec(model).is_some()
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
        self.build(input, presets, blobs).await
    }
    async fn stream(
        &self,
        p: &Prepared,
        sink: &mut dyn EventSink,
        observer: &dyn Observer,
        cancel: &CancellationToken,
    ) -> (Result<Value, LlmError>, CallOutcome) {
        match p.dialect {
            imagine::IMAGE_DIALECT => imagine::stream_image(&self.http, &self.cfg, p, sink, observer, cancel).await,
            imagine::VIDEO_DIALECT => {
                imagine::stream_video(&self.http, &self.cfg, self.poll, p, sink, observer, cancel).await
            }
            _ => {
                let run = wire::Run {
                    http: &self.http,
                    api_key: &self.cfg.api_key,
                    idle_timeout: self.cfg.idle_timeout,
                    recovery: None,
                };
                wire::stream(&run, &WIRE, p, sink, observer, cancel).await
            }
        }
    }
}

/// The request that resumes a video job after a restart or a dropped
/// connection (see [`imagine::continuation_request`]).
pub fn video_continuation(p: &Prepared, item: &Value) -> Prepared {
    imagine::continuation_request(p, item)
}

/// The dialect of xAI's video jobs (resumable from a checkpoint).
pub const VIDEO_DIALECT: &str = imagine::VIDEO_DIALECT;

// ------------------------------------------------------------------ cost

/// From this many prompt tokens on, every token of the request costs 2×.
const LONG_CONTEXT_FROM: u64 = 200_000;
/// web_search, code_interpreter, attachment_search: $5 / 1k calls.
const TOOL_CALL: f64 = 0.005;
/// image_generation: "Imagine API rates" — grok-imagine-image-2.0's lowest
/// tier ($0.04 per image) as the estimate (a lower bound; xAI reports the
/// billed cost anyway).
const IMAGE_TOOL_CALL: f64 = 0.04;

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
        + n("x_users_fetched") * 0.01
        + n("image_generation_calls") * IMAGE_TOOL_CALL;
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
pub(crate) fn refine(mut e: LlmError) -> LlmError {
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
    use omni_core::content::{Part, ReasoningKind};
    use omni_core::trigger::OptionToken;
    use omni_core::usage::CostSource;
    use omni_core::{BlobRef, Turn};

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
        // Unknown models: their price is unknown to the bot, so they are not supported.
        assert_eq!(
            err("grok-5", &[]).await,
            "`grok-5` is not in the bot's xAI model table (its price is unknown), so it is not supported"
        );
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
    async fn media_and_no_attachment_search_note_before_one_ran() {
        let (img, b1) = photo("image/png", b"\x89PNG");
        let (pdf, b2) = file("r.pdf", "application/pdf", b"%PDF");
        let blobs = MemBlobs(vec![b1, b2]);
        let history = vec![user(vec![Part::text("?"), img, pdf])];
        let p = prep_with(&xai(), "grok-4.7", &[], &history, &blobs).await;
        let content = &p.body["input"][0]["content"];
        assert_eq!(content[1]["detail"], "high");
        assert_eq!(content[2]["type"], "input_file");
        // Inline files did not trigger attachment_search (2026-11-13): said only when the usage reports it.
        assert!(p.notes.is_empty(), "{:?}", p.notes);
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
                        "code_interpreter_calls": 0, "file_search_calls": 0, "mcp_calls": 0, "document_search_calls": 0, "image_generation_calls": 0},
                    "context_details": {"input_tokens": 612, "output_tokens": 320}}}}),
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
        // The conversation's size after the turn, also after a search (not the billed 900 + 20).
        assert!(evs.iter().any(|e| matches!(e, StreamEvent::ContextSize(932))));
        assert!(!evs.iter().any(|e| matches!(e, StreamEvent::Notice(_))), "no attachment_search ran");
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

    #[tokio::test]
    async fn the_image_generation_tool_on_chat_models() {
        let p = prep("grok-4.7", &[flag("si")]).await;
        assert_eq!(p.body["tools"], json!([{"type": "web_search"}, {"type": "image_generation"}]));
        assert_eq!(p.body["include"], json!(["reasoning.encrypted_content", "web_search_call.action.sources"]));
        assert_eq!(p.settings, vec!["effort xhigh", "web search", "image generation"]);
        assert!(p.applied.iter().any(|a| a.patch == json!({"tools": [{"type": "image_generation"}]})));
        // The tool takes no size, ratio, quality or background.
        assert!(err("grok-4.7", &[flag("iW")]).await.contains("takes no image size or aspect ratio"));
        assert!(
            err("grok-4.7", &[flag("i"), kv("q", "medium")])
                .await
                .contains("does not take an image quality (`q=`) via xAI")
        );
        assert!(err("grok-4.7", &[flag("i"), kv("bg", "opaque")]).await.contains("(`bg=`) via xAI"));
        assert!(err("grok-4.7", &[flag("i"), kv("n", "2")]).await.contains("(`n=`) via xAI"));
        // Bare Grok Imagine ids go to the Imagine APIs.
        assert_eq!(prep("grok-imagine-image-2.0", &[]).await.dialect, imagine::IMAGE_DIALECT);
        assert_eq!(prep("grok-imagine-video-1.5", &[]).await.dialect, imagine::VIDEO_DIALECT);
        assert!(xai().knows("grok-imagine-video-1.5-preview") && xai().knows("grok-imagine-image"));
    }

    #[tokio::test]
    async fn generated_images_stream_as_tool_calls_and_replay() {
        let p = prep("grok-4.7", &[flag("i")]).await;
        let jpeg = crate::provider::b64(b"\xff\xd8\xff\xe0-corgi");
        let item = json!({"type": "image_generation_call", "id": "ig_d817_call-a1", "status": "completed",
            "prompt": "A corgi surfing a big wave, Japanese woodblock print style", "result": jpeg});
        let message = json!({"type": "message", "id": "msg_1", "role": "assistant", "status": "completed",
            "content": [{"type": "output_text", "text": "Here you go.", "annotations": [], "logprobs": []}]});
        let events = [
            json!({"type": "response.created", "response": {"id": "r-1", "model": "grok-4.7", "status": "in_progress", "output": []}}),
            json!({"type": "response.output_item.added", "output_index": 0, "item": {"type": "image_generation_call", "id": "ig_d817_call-a1", "status": "in_progress"}}),
            json!({"type": "response.image_generation_call.in_progress", "output_index": 0, "item_id": "ig_d817_call-a1"}),
            json!({"type": "response.image_generation_call.generating", "output_index": 0, "item_id": "ig_d817_call-a1"}),
            json!({"type": "response.image_generation_call.completed", "output_index": 0, "item_id": "ig_d817_call-a1"}),
            json!({"type": "response.output_item.done", "output_index": 0, "item": item}),
            json!({"type": "response.output_text.delta", "output_index": 1, "item_id": "msg_1", "content_index": 0, "delta": "Here you go."}),
            json!({"type": "response.completed", "response": {"id": "r-1", "status": "completed", "output": [item, message],
                "usage": {"input_tokens": 500, "output_tokens": 10, "output_tokens_details": {"reasoning_tokens": 90}, "total_tokens": 600,
                    "num_server_side_tools_used": 1, "cost_in_usd_ticks": 420_000_000,
                    "server_side_tool_usage_details": {"web_search_calls": 0, "image_generation_calls": 1}}}}),
        ];
        let chunks: Vec<String> = events.iter().map(|e| format!("data: {e}\n\n")).collect();
        let (base, _) = serve(200, "text/event-stream", "", chunks).await;
        let x = Xai::new(cfg(&base)).unwrap();
        let p = Prepared { url: x.cfg.url("/responses"), ..p };
        let mut evs = Vec::new();
        let mut sink = |e: StreamEvent| evs.push(e);
        let (r, _) = x.stream(&p, &mut sink, &crate::NullObserver, &CancellationToken::new()).await;
        let native = r.unwrap();
        // The JPEG where the call finished, then the answer; no previews on xAI.
        let at = |f: &dyn Fn(&StreamEvent) -> bool| evs.iter().position(f).unwrap();
        let image = at(&|e| matches!(e, StreamEvent::Image { mime, .. } if mime == "image/jpeg"));
        let text = at(&|e| matches!(e, StreamEvent::Text(_)));
        assert!(image < text);
        assert!(matches!(&evs[image + 1], StreamEvent::PreviewDone { slot } if slot == "ig_d817_call-a1"));
        assert!(!evs.iter().any(|e| matches!(e, StreamEvent::PartialImage { .. })));
        let usage =
            evs.iter().find_map(|e| if let StreamEvent::Usage(u) = e { Some(u.clone()) } else { None }).unwrap();
        assert_eq!((usage.cost_usd, usage.cost_source), (Some(0.042), CostSource::Reported));
        let mut acc = crate::Accumulator::new();
        evs.into_iter().for_each(|e| acc.apply(e));
        let Part::ToolCall(t) = &acc.parts()[0] else { panic!("{:?}", acc.parts()) };
        assert_eq!(t.name, "image_generation");
        assert_eq!(t.status.as_deref(), Some("completed"));
        assert_eq!(t.input, Some(json!("A corgi surfing a big wave, Japanese woodblock print style")));
        // Replayed verbatim (with its image) on the next turn.
        let mut turn = acc.turn();
        turn.native = Some(omni_core::content::NativeOutput {
            dialect: DIALECT.into(),
            model: "grok-4.7".into(),
            payload: native,
            context_tokens: None,
        });
        let history = vec![Turn::user_text("a corgi"), turn, Turn::user_text("now at night")];
        let next = prep_with(&xai(), "grok-4.7", &[flag("i")], &history, &MemBlobs(vec![])).await;
        assert_eq!(next.body["input"][1], item);
    }

    #[test]
    fn the_context_size_and_document_searches_come_with_the_usage() {
        fn none(_: &str, _: &Value, _: &Usage) -> Option<f64> {
            None
        }
        let terminal = |usage: Value| {
            let mut p = wire::Parser::new(Platform::Xai, "grok-4.3", none, error_from_json);
            p.event(&json!({"type": "response.completed", "response": {"id": "r", "status": "completed", "output": [], "usage": usage}}))
                .unwrap()
        };
        // Turn 279 (grok-4.3 +s, 2026-11-13): billed 19,962 in over three searches, the context 12,708 + 171.
        let evs = terminal(
            json!({"input_tokens": 19962, "input_tokens_details": {"cached_tokens": 6400}, "output_tokens": 526,
            "output_tokens_details": {"reasoning_tokens": 422}, "total_tokens": 20488, "num_sources_used": 0, "num_server_side_tools_used": 3,
            "cost_in_usd_ticks": 345475000, "server_side_tool_usage_details": {"web_search_calls": 3, "x_search_calls": 0,
                "x_posts_fetched": 0, "x_users_fetched": 0, "code_interpreter_calls": 0, "file_search_calls": 0, "mcp_calls": 0,
                "document_search_calls": 0, "image_generation_calls": 0},
            "context_details": {"input_tokens": 12708, "output_tokens": 171}}),
        );
        assert!(matches!(&evs[0], StreamEvent::ContextSize(12879)));
        assert!(!evs.iter().any(|e| matches!(e, StreamEvent::Notice(_))));
        // Files read by attachment_search: said, with the count.
        let evs = terminal(json!({"input_tokens": 3000, "output_tokens": 50, "total_tokens": 3050,
            "server_side_tool_usage_details": {"document_search_calls": 2}}));
        assert!(
            evs.iter().any(|e| matches!(e, StreamEvent::Notice(n) if n == "📎 xAI searched the attached files (×2)"))
        );
        assert!(!evs.iter().any(|e| matches!(e, StreamEvent::ContextSize(_))), "none reported");
    }

    #[tokio::test]
    async fn media_of_other_answers_go_in_the_assistant_message() {
        let (png, png_bytes) = (BlobRef::for_bytes(b"\x89PNG-gen", "image/png"), b"\x89PNG-gen".to_vec());
        let webp = BlobRef::for_bytes(b"RIFF----WEBPVP8 ", "image/webp");
        let (pdf, pdf_bytes) = (BlobRef::for_bytes(b"%PDF-1.7", "application/pdf"), b"%PDF-1.7".to_vec());
        let video = BlobRef::for_bytes(b"....ftypmp42", "video/mp4");
        let other = Turn {
            role: omni_core::Role::Assistant,
            origin: omni_core::Origin::Live,
            parts: vec![
                Part::text("Here it is:"),
                Part::Image { blob: png.clone(), filename: None, generated: true, dims: None, thought: false },
                Part::Image { blob: webp, filename: None, generated: true, dims: None, thought: false },
                Part::File { blob: pdf.clone(), filename: Some("report.pdf".into()), generated: true },
                Part::File { blob: video, filename: Some("v.mp4".into()), generated: true },
                Part::text("Enjoy."),
            ],
            native: None,
        };
        let blobs = MemBlobs(vec![(png, png_bytes), (pdf, pdf_bytes)]);
        // What xAI cannot take (a WebP image, a video) refuses the request, every reason said.
        let history = vec![Turn::user_text("draw"), other.clone(), Turn::user_text("now blue")];
        let input = CallInput {
            model: "grok-4.7",
            options: &[],
            history: &history,
            session_id: None,
            user: None,
            known_context: None,
            max_tokens_override: None,
        };
        let e = xai().prepare(&input, &[], &blobs).await.unwrap_err();
        assert_eq!(
            e.message,
            "1 image from earlier answers cannot be sent: xAI takes JPEG or PNG images of up to 20 MiB; 1 video from \
             earlier answers cannot be sent: xAI takes no video input"
        );
        // What it takes goes in the assistant message, in order.
        let taken = Turn {
            parts: other
                .parts
                .iter()
                .enumerate()
                .filter(|(i, _)| ![2, 4].contains(i))
                .map(|(_, p)| p.clone())
                .collect(),
            ..other
        };
        let history = vec![Turn::user_text("draw"), taken, Turn::user_text("now blue")];
        let p = prep_with(&xai(), "grok-4.7", &[], &history, &blobs).await;
        let a = &p.body["input"][1];
        assert_eq!(a["role"], "assistant");
        let content = a["content"].as_array().unwrap();
        let kinds: Vec<&str> = content.iter().map(|c| c["type"].as_str().unwrap()).collect();
        assert_eq!(kinds, ["output_text", "input_image", "input_file", "output_text"]);
        assert_eq!(content[1]["detail"], "high");
        assert_eq!(
            (content[2]["filename"].clone(), content[2]["mime_type"].clone()),
            (json!("report.pdf"), json!("application/pdf"))
        );
        assert!(p.warnings.is_empty(), "{:?}", p.warnings);
        // Text only: as before.
        let history = vec![Turn::user_text("hi"), Turn::assistant_text("hello"), Turn::user_text("more")];
        let p = prep_with(&xai(), "grok-4.7", &[], &history, &MemBlobs(vec![])).await;
        assert_eq!(p.body["input"][1], json!({"type": "message", "role": "assistant", "content": "hello"}));
        assert!(p.warnings.is_empty());
    }

    #[test]
    fn image_tool_calls_are_estimated_without_a_billed_cost() {
        let raw = json!({"server_side_tool_usage_details": {"image_generation_calls": 2}});
        let c = estimate_cost("grok-4.7", &json!({}), &usage(0, 0, raw)).unwrap();
        assert!((c - 0.08).abs() < 1e-9, "{c}");
    }
}
