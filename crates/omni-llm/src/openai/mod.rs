//! OpenAI's own API: the Responses API (`POST {base}/responses`), used
//! statelessly (docs/research/openai.md; model pages, pricing and guides
//! fetched 2026-11-11/12).
//!
//! What the bot sends:
//! - `store: false`, `stream: true`; earlier turns of the same model replay
//!   all their output items verbatim (reasoning with the `encrypted_content`
//!   stateless responses carry by default, messages with `phase`, hosted tool
//!   calls); other assistant turns are plain text.
//! - Defaults (D3): the model's highest `reasoning.effort` with
//!   `reasoning.summary: "detailed"`; the full output budget
//!   (`max_output_tokens`, which counts reasoning too); images at
//!   `detail: "original"` where the vision guide documents it, else `"high"`;
//!   PDFs at `detail: "high"`. `chat-latest` documents no effort levels: only
//!   the summary is requested, as the old bot did.
//! - Left at OpenAI's defaults: `reasoning.context` and `reasoning.mode`,
//!   `truncation` (`disabled`: an overflow is a 400, after which the bot
//!   shrinks the budget), `prompt_cache_options`, web search
//!   `search_context_size` / `user_location`, the container memory (1 GB).
//! - `+s` → `web_search` (+ `include: web_search_call.action.sources`),
//!   `+c` → `code_interpreter` with `container: {type: auto}`
//!   (+ `include: code_interpreter_call.outputs`); OpenAI has no URL fetching.
//!   `v=` → `text.verbosity`; `tier=` → `service_tier` (flex, priority/fast,
//!   ultrafast, per model).
//! - `safety_identifier` = the hashed user, `prompt_cache_key` = the
//!   conversation, `stream_options.include_obfuscation: false`.
//! - Files: `input_file` takes PDFs, documents, presentations, spreadsheets
//!   and ~100 text/code types natively; other text files are inlined (D6);
//!   audio and video are errors (the Responses API has no such input).
//! - No cost in responses: estimated from the dated price table below.

pub(crate) mod wire;

use omni_core::Usage;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::error::{ErrorKind, LlmError, Retry, retry_for_status};
use crate::openrouter::options::{Applied, overlay};
use crate::provider::{DirectConfig, Provider};
use crate::request::{self, Requested, RouteCaps, Tool};
use crate::{BlobLoader, CallInput, CallOutcome, EventSink, Observer, Prepared, budget};
use wire::{Platform, Price, Scale};

pub const DIALECT: &str = "openai.responses";

const NAME: &str = "OpenAI";

/// Limits, reasoning levels, tiers and list prices of an OpenAI model.
#[derive(Debug, Clone, Copy)]
pub struct ModelSpec {
    pub id: &'static str,
    /// Other ids of the same model (dated snapshots `id-YYYY-MM-DD` match too).
    pub aliases: &'static [&'static str],
    /// Context window (input + output).
    pub context: u64,
    /// Maximum `max_output_tokens` (reasoning included).
    pub max_output: u64,
    /// `reasoning.effort` levels, highest first; `None`: the model documents none.
    pub efforts: Option<&'static [&'static str]>,
    /// Takes `reasoning.summary`.
    pub summary: bool,
    /// Takes `text.verbosity`.
    pub verbosity: bool,
    /// `input_image.detail` sent: `original` where documented, else `high`.
    pub image_detail: &'static str,
    /// Standard tier, prompts up to 272K input tokens (USD per 1M tokens).
    pub price: Price,
    /// Prompts over 272K input tokens bill the whole request at 2× input /
    /// cache and 1.5× output.
    pub long_context: bool,
    /// `service_tier: "flex"` (Batch rates: half price).
    pub flex: bool,
    /// Fast mode (`service_tier: "fast"` or its old name `"priority"`), as a
    /// multiple of the standard price.
    pub fast: Option<f64>,
    /// `service_tier: "ultrafast"` (6× the standard price).
    pub ultrafast: bool,
}

const MAX_TO_LOW: &[&str] = &["max", "xhigh", "high", "medium", "low"];
const MAX_TO_NONE: &[&str] = &["max", "xhigh", "high", "medium", "low", "none"];
const XHIGH_TO_NONE: &[&str] = &["xhigh", "high", "medium", "low", "none"];
const PRO: &[&str] = &["xhigh", "high", "medium"];

const fn price(input: f64, cached: f64, cache_write: Option<f64>, output: f64) -> Price {
    Price { input, cached, cache_write, output }
}

/// The GPT-6 / GPT-5.6 shape; each entry below changes what differs.
const BASE: ModelSpec = ModelSpec {
    id: "",
    aliases: &[],
    context: 1_050_000,
    max_output: 128_000,
    efforts: Some(MAX_TO_NONE),
    summary: true,
    verbosity: true,
    image_detail: "high",
    price: price(0.0, 0.0, None, 0.0),
    long_context: true,
    flex: true,
    fast: Some(2.0),
    ultrafast: false,
};

/// OpenAI's models, 2026-11-12. Limits and reasoning levels: the model pages
/// (docs/research/openai.md §1.1); image `original` detail: the images and
/// vision guide; prices and tiers: developers.openai.com/api/docs/pricing
/// (Standard, Flex, Fast and Ultrafast tables). `gpt-5.6-sol` is at its
/// promotional price, "available until November 30, 2026" (the regular
/// price is not published). Pro models list no cached-input price (billed
/// as input). Regional (data residency) endpoints cost 10% more for models
/// from 2026-03-05 on: not modeled.
pub const MODELS: &[ModelSpec] = &[
    // "Latest Instant model used in ChatGPT": no effort levels, no tiers, no Batch.
    ModelSpec {
        id: "chat-latest",
        context: 400_000,
        efforts: None,
        verbosity: false,
        price: price(5.0, 0.5, None, 30.0),
        long_context: false,
        flex: false,
        fast: None,
        ..BASE
    },
    ModelSpec {
        id: "gpt-6-astra",
        efforts: Some(MAX_TO_LOW),
        image_detail: "original",
        price: price(10.0, 1.0, Some(12.5), 50.0),
        ultrafast: true,
        ..BASE
    },
    ModelSpec {
        id: "gpt-6.1-sol",
        efforts: Some(MAX_TO_LOW),
        price: price(2.0, 0.1, Some(2.5), 10.0),
        ultrafast: true,
        ..BASE
    },
    ModelSpec { id: "gpt-6.1-terra", price: price(1.0, 0.05, Some(1.25), 5.0), ..BASE },
    ModelSpec { id: "gpt-6-sol", price: price(2.0, 0.2, Some(2.5), 10.0), ..BASE },
    ModelSpec { id: "gpt-6-luna", price: price(0.1, 0.01, Some(0.125), 0.5), ..BASE },
    ModelSpec {
        id: "gpt-5.6-sol",
        aliases: &["gpt-5.6"],
        image_detail: "original",
        price: price(4.0, 0.4, Some(5.0), 20.0),
        ..BASE
    },
    ModelSpec { id: "gpt-5.6-terra", image_detail: "original", price: price(2.0, 0.2, Some(2.5), 12.0), ..BASE },
    ModelSpec { id: "gpt-5.6-luna", image_detail: "original", price: price(0.2, 0.02, Some(0.25), 1.2), ..BASE },
    ModelSpec {
        id: "gpt-5.5",
        efforts: Some(XHIGH_TO_NONE),
        image_detail: "original",
        price: price(5.0, 0.5, None, 30.0),
        fast: Some(2.5),
        ..BASE
    },
    ModelSpec { id: "gpt-5.5-pro", efforts: Some(PRO), price: price(30.0, 30.0, None, 180.0), fast: None, ..BASE },
    ModelSpec {
        id: "gpt-5.4",
        efforts: Some(XHIGH_TO_NONE),
        image_detail: "original",
        price: price(2.5, 0.25, None, 15.0),
        ..BASE
    },
    ModelSpec {
        id: "gpt-5.4-mini",
        context: 400_000,
        efforts: Some(XHIGH_TO_NONE),
        image_detail: "original",
        price: price(0.75, 0.075, None, 4.5),
        long_context: false,
        ..BASE
    },
    ModelSpec {
        id: "gpt-5.4-nano",
        context: 400_000,
        efforts: Some(XHIGH_TO_NONE),
        image_detail: "original",
        price: price(0.2, 0.02, None, 1.25),
        long_context: false,
        fast: None,
        ..BASE
    },
    ModelSpec { id: "gpt-5.4-pro", efforts: Some(PRO), price: price(30.0, 30.0, None, 180.0), fast: None, ..BASE },
    ModelSpec {
        id: "gpt-5.2",
        context: 400_000,
        efforts: Some(XHIGH_TO_NONE),
        price: price(1.75, 0.175, None, 14.0),
        long_context: false,
        ..BASE
    },
    ModelSpec {
        id: "gpt-5.2-pro",
        context: 400_000,
        efforts: Some(PRO),
        price: price(21.0, 21.0, None, 168.0),
        long_context: false,
        flex: false,
        fast: None,
        ..BASE
    },
    // Deprecated.
    ModelSpec {
        id: "gpt-5.1",
        context: 400_000,
        efforts: Some(&["high", "medium", "low", "none"]),
        price: price(1.25, 0.125, None, 10.0),
        long_context: false,
        ..BASE
    },
    ModelSpec {
        id: "gpt-5",
        context: 400_000,
        efforts: Some(&["high", "medium", "low", "minimal"]),
        price: price(1.25, 0.125, None, 10.0),
        long_context: false,
        ..BASE
    },
];

/// `gpt-5.4-2026-03-05` → `gpt-5.4`.
fn strip_snapshot(model: &str) -> &str {
    let n = model.len();
    let Some(tail) = n.checked_sub(11).and_then(|i| model.get(i..)) else { return model };
    let t = tail.as_bytes();
    let dated = t[0] == b'-'
        && t[5] == b'-'
        && t[8] == b'-'
        && [1, 2, 3, 4, 6, 7, 9, 10].iter().all(|&i| t[i].is_ascii_digit());
    if dated { &model[..n - 11] } else { model }
}

/// The table entry of `model` (its id, an alias, or a dated snapshot of either).
pub fn model_spec(model: &str) -> Option<&'static ModelSpec> {
    let find = |m: &str| MODELS.iter().find(|s| s.id == m || s.aliases.contains(&m));
    find(model).or_else(|| find(strip_snapshot(model)))
}

/// Every `reasoning.effort` value the API documents: for models not in the
/// table, OpenAI itself checks what the model takes.
const ANY_EFFORT: &[&str] = &["max", "xhigh", "high", "medium", "low", "minimal", "none"];

/// What OpenAI offers for a model (the vendor's own API: official = route).
struct Caps(Option<&'static ModelSpec>);

impl RouteCaps for Caps {
    fn name(&self) -> &str {
        NAME
    }
    fn tool(&self, tool: Tool) -> bool {
        // web_search and code_interpreter (every model in the table lists both).
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
        self.0.is_none_or(|s| s.verbosity)
    }
    fn image_options(&self) -> bool {
        false
    }
    fn tiers(&self) -> Vec<String> {
        let (flex, fast, ultrafast) = match self.0 {
            Some(s) => (s.flex, s.fast.is_some(), s.ultrafast),
            None => (true, true, true),
        };
        let mut t = Vec::new();
        if flex {
            t.push("flex");
        }
        if fast {
            t.extend(["priority", "fast"]);
        }
        if ultrafast {
            t.push("ultrafast");
        }
        t.into_iter().map(String::from).collect()
    }
}

const VERBOSITY: &[&str] = &["low", "medium", "high"];

pub struct OpenAi {
    cfg: DirectConfig,
    http: reqwest::Client,
}

impl OpenAi {
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
        let caps = Caps(spec);
        // `request::official` knows OpenAI's verbosity only by OpenRouter ids (`openai/…`): checked here.
        let mut checked = req.clone();
        checked.verbosity = None;
        request::validate(model, &checked, &caps).map_err(LlmError::request)?;
        if let Some(v) = &req.verbosity {
            if !caps.verbosity() {
                return Err(LlmError::request(format!("`{model}` has no verbosity setting")));
            }
            if !VERBOSITY.contains(&v.as_str()) {
                return Err(LlmError::request(format!(
                    "verbosity `{v}` is not available; levels: {}",
                    VERBOSITY.join(", ")
                )));
            }
        }

        let image_detail = spec.map_or("high", |s| s.image_detail);
        let inputs = wire::Inputs { platform: Platform::OpenAi, model, image_detail };
        let conv = wire::convert_history(input.history, &inputs, blobs).await?;
        let mut body = json!({
            "model": model,
            "input": conv.input,
            "stream": true,
            "stream_options": {"include_obfuscation": false},
            "store": false,
        });
        let src = |by_user: bool| if by_user { "option" } else { "default" };
        let mut applied = Vec::new();
        let mut settings = Vec::new();

        // Reasoning: the highest effort and the most detailed summary by default.
        let reasoning = match (spec, req.effort.as_deref()) {
            (_, Some("none")) => Some((json!({"effort": "none"}), "reasoning off".to_string())),
            (Some(s), e) => match (s.efforts.and_then(|l| l.first()), e) {
                (Some(top), e) => {
                    let e = e.unwrap_or(top);
                    Some((json!({"effort": e, "summary": "detailed"}), format!("effort {e}")))
                }
                (None, _) if s.summary => Some((json!({"summary": "detailed"}), String::new())),
                (None, _) => None,
            },
            // Not in the table: only what the user asked for.
            (None, Some(e)) => Some((json!({"effort": e}), format!("effort {e}"))),
            (None, None) => None,
        };
        if let Some((r, desc)) = reasoning {
            applied.push(Applied { source: src(user.effort.is_some()).into(), patch: json!({"reasoning": r}) });
            if !desc.is_empty() {
                settings.push(desc);
            }
            body["reasoning"] = r;
        }

        // The whole output budget (reasoning counts against it), within the context window.
        let auto_max = input.max_tokens_override.or_else(|| {
            let s = spec?;
            let prompt = budget::estimate_prompt(input.history, input.known_context);
            budget::choose(Some(s.context), Some(s.max_output), self.cfg.max_tokens_cap, prompt)
        });
        if let Some(m) = auto_max {
            body["max_output_tokens"] = json!(m);
            applied.push(Applied { source: "budget".into(), patch: json!({"max_output_tokens": m}) });
        }

        let mut tools = Vec::new();
        let mut include = Vec::new();
        for &t in &req.tools {
            let (tool, inc) = match t {
                Tool::Search => (json!({"type": "web_search"}), "web_search_call.action.sources"),
                Tool::Code => (
                    json!({"type": "code_interpreter", "container": {"type": "auto"}}),
                    "code_interpreter_call.outputs",
                ),
                // TODO(image generation agent): the image_generation tool.
                Tool::Fetch | Tool::Maps | Tool::Image => {
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
            body["include"] = json!(include);
        }
        if let Some(v) = &req.verbosity {
            body["text"] = json!({"verbosity": v});
            applied
                .push(Applied { source: src(user.verbosity.is_some()).into(), patch: json!({"text": body["text"]}) });
            settings.push(format!("verbosity {v}"));
        }
        if let Some(t) = req.tier.as_deref().filter(|t| !matches!(*t, "default" | "auto")) {
            body["service_tier"] = json!(t);
            applied.push(Applied { source: src(user.tier.is_some()).into(), patch: json!({"service_tier": t}) });
            settings.push(format!("tier {t}"));
        }
        if conv.images > 0 {
            applied.push(Applied { source: "default".into(), patch: json!({"input_image": {"detail": image_detail}}) });
        }
        if conv.pdfs > 0 {
            applied.push(Applied { source: "default".into(), patch: json!({"input_file (PDF)": {"detail": "high"}}) });
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
                "`{model}` is not in the bot's OpenAI model table: limits, price and reasoning levels unknown \
                 (the bot's defaults are not sent; max_output_tokens {}; cost not estimated)",
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

const WIRE: wire::Wire = wire::Wire {
    platform: Platform::OpenAi,
    price: estimate_cost,
    map_error: error_from_json,
    refine: std::convert::identity,
};

#[async_trait::async_trait]
impl Provider for OpenAi {
    fn id(&self) -> &'static str {
        "openai"
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

/// Prompts above this many input tokens bill at the long-context rates.
const LONG_CONTEXT_ABOVE: u64 = 272_000;
/// `web_search`: $10 / 1k calls (search content tokens bill as input).
const WEB_SEARCH_PER_CALL: f64 = 0.01;
/// Code interpreter: one 20-minute session of a 1 GB container (the default
/// memory limit), $0.03. Billed by the minute with a 5-minute minimum; the
/// estimate counts a full session per container the response used.
const CONTAINER_SESSION: f64 = 0.03;

/// Estimated cost in USD of a finished response: tokens at the rates of the
/// tier that served it (`service_tier` in the response; Fast reports
/// `priority`) and of its context length, plus tool fees.
pub fn estimate_cost(model: &str, response: &Value, u: &Usage) -> Option<f64> {
    let s = model_spec(model)?;
    let long = s.long_context && u.input_tokens? > LONG_CONTEXT_ABOVE;
    let scale = if long { Scale { input: 2.0, output: 1.5 } } else { Scale::ONE };
    let tier = match response.get("service_tier").and_then(Value::as_str) {
        Some("flex") => 0.5,
        Some("priority" | "fast") => s.fast.unwrap_or(2.0),
        Some("ultrafast") => 6.0,
        _ => 1.0,
    };
    let written_12h = u
        .raw
        .pointer("/input_tokens_details/cache_write_tokens_details/ttl_12h_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let tokens = wire::token_cost(&s.price, u, written_12h, scale.times(tier))?;
    Some(tokens + tool_fees(response))
}

/// Web searches (`tool_usage.web_search.num_requests` counts the billable
/// ones; without it, the `search` actions) and code containers.
fn tool_fees(response: &Value) -> f64 {
    let searches =
        response.pointer("/tool_usage/web_search/num_requests").and_then(Value::as_u64).unwrap_or_else(|| {
            wire::items_of(response, "web_search_call")
                .filter(|i| i.pointer("/action/type").and_then(Value::as_str) == Some("search"))
                .count() as u64
        });
    searches as f64 * WEB_SEARCH_PER_CALL + wire::containers(response) as f64 * CONTAINER_SESSION
}

// ------------------------------------------------------------------ errors

/// Quota and billing codes (some come as HTTP 429): retrying cannot help.
const BILLING: &[&str] = &[
    "insufficient_quota",
    "credit_balance_exhausted",
    "organization_spend_limit_exceeded",
    "project_spend_limit_exceeded",
    "organization_usage_limit_exceeded",
    "billing_hard_limit_reached",
    "billing_not_active",
];

/// Classify OpenAI's error objects (`{"message", "type", "param", "code"}`,
/// docs/research/openai.md §8): HTTP errors, and in-band `error` events and
/// `response.failed` errors (`status` `None`). The object is kept verbatim.
pub fn error_from_json(err: &Value, status: Option<u16>, retry_after: Option<u64>, kind: ErrorKind) -> LlmError {
    let field = |k: &str| {
        err.get(k)
            .and_then(|v| v.as_str().map(str::to_string).or_else(|| v.as_u64().map(|n| n.to_string())))
            .filter(|s| !s.is_empty())
    };
    let code = field("code");
    let ty = field("type");
    let message = field("message").unwrap_or_else(|| "(no message)".into());
    let lower = message.to_ascii_lowercase();
    let is = |set: &[&str]| [&code, &ty].iter().any(|v| v.as_deref().is_some_and(|v| set.contains(&v)));
    // Also by the message ("You exceeded your current quota, please check your plan and billing details"),
    // for billing codes not listed.
    let billing = is(BILLING) || ["quota", "billing", "spend limit"].iter().any(|k| lower.contains(k));
    let context = code.as_deref() == Some("context_length_exceeded")
        || ["context window", "context length", "maximum context"].iter().any(|k| lower.contains(k));
    let retry = if billing || context {
        Retry::Never
    } else if is(&["rate_limit_exceeded", "slow_down"]) || status == Some(429) {
        // Rate limits, ramp-up limits, Flex "Resource Unavailable" (not charged).
        Retry::After(retry_after.unwrap_or(20))
    } else if let Some(s) = status {
        retry_for_status(s, retry_after)
    } else {
        wire::in_band_retry(code.as_deref())
    };
    let error_type = if context { Some("context_length_exceeded".to_string()) } else { code.or(ty) };
    LlmError { kind, retry, status, error_type, message, details: Some(err.clone()), partial_output: false }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::wire::tests::{MemBlobs, file, photo, serve, user};
    use super::*;
    use crate::StreamEvent;
    use omni_core::Turn;
    use omni_core::content::Part;
    use omni_core::trigger::OptionToken;
    use omni_core::usage::CostSource;

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

    fn openai() -> OpenAi {
        OpenAi::new(cfg(crate::provider::info("openai").unwrap().base_url)).unwrap()
    }

    fn flag(f: &str) -> OptionToken {
        OptionToken::Flag(f.into())
    }

    fn kv(k: &str, v: &str) -> OptionToken {
        OptionToken::KeyValue(k.into(), v.into())
    }

    async fn prep_with(p: &OpenAi, model: &str, opts: &[OptionToken], history: &[Turn]) -> Result<Prepared, LlmError> {
        let input = CallInput {
            model,
            options: opts,
            history,
            session_id: Some("tg-7".into()),
            user: Some("u-hash".into()),
            known_context: None,
            max_tokens_override: None,
        };
        p.prepare(&input, &[], &MemBlobs(vec![])).await
    }

    async fn prep(model: &str, opts: &[OptionToken]) -> Result<Prepared, LlmError> {
        prep_with(&openai(), model, opts, &[Turn::user_text("hi")]).await
    }

    async fn err(model: &str, opts: &[OptionToken]) -> String {
        prep(model, opts).await.unwrap_err().message
    }

    #[test]
    fn table_covers_the_configured_aliases() {
        // OpenRouter ids of config/omni.example.toml → official ids.
        for (or, official) in [
            ("openai/gpt-chat-latest", "chat-latest"),
            ("openai/gpt-6.1-sol", "gpt-6.1-sol"),
            ("openai/gpt-6.1-terra", "gpt-6.1-terra"),
            ("openai/gpt-6-astra", "gpt-6-astra"),
            ("openai/gpt-6-sol", "gpt-6-sol"),
            ("openai/gpt-6-luna", "gpt-6-luna"),
            ("openai/gpt-5.6-sol", "gpt-5.6-sol"),
            ("openai/gpt-5.6-terra", "gpt-5.6-terra"),
            ("openai/gpt-5.6-luna", "gpt-5.6-luna"),
            ("openai/gpt-5.5", "gpt-5.5"),
            ("openai/gpt-5.4", "gpt-5.4"),
            ("openai/gpt-5.4-mini", "gpt-5.4-mini"),
            ("openai/gpt-5.4-nano", "gpt-5.4-nano"),
        ] {
            assert_eq!(model_spec(official).map(|s| s.id), Some(official), "{or}");
        }
        // Dated snapshots (the old bot used them) and aliases.
        assert_eq!(model_spec("gpt-5.4-2026-03-05").unwrap().id, "gpt-5.4");
        assert_eq!(model_spec("gpt-5.4-mini-2026-03-17").unwrap().id, "gpt-5.4-mini");
        assert_eq!(model_spec("gpt-5.6").unwrap().id, "gpt-5.6-sol");
        assert!(model_spec("gpt-5.6-cyber").is_none());
        assert!(model_spec("gpt-5.4-2026-03-0x").is_none());
        assert!(model_spec("模型-2026-03-05").is_none() && model_spec("é").is_none(), "no panic on non-ASCII ids");
        assert!(model_spec("gpt-4o").is_none());
        for s in MODELS {
            assert!(s.max_output <= s.context && s.price.output > 0.0, "{}", s.id);
            assert!(s.efforts.is_none_or(|l| !l.is_empty()), "{}", s.id);
        }
    }

    #[tokio::test]
    async fn defaults_are_the_most_the_model_offers() {
        let p = prep("gpt-6.1-sol", &[]).await.unwrap();
        assert_eq!(p.url, "https://api.openai.com/v1/responses");
        assert_eq!(p.dialect, DIALECT);
        let b = &p.body;
        assert_eq!(b["reasoning"], json!({"effort": "max", "summary": "detailed"}));
        assert_eq!(b["max_output_tokens"], 128_000);
        assert_eq!(p.auto_max_tokens, Some(128_000));
        assert_eq!((b["store"].clone(), b["stream"].clone()), (json!(false), json!(true)));
        assert_eq!(b["stream_options"], json!({"include_obfuscation": false}));
        assert_eq!(b["safety_identifier"], "u-hash");
        assert_eq!(b["prompt_cache_key"], "tg-7");
        assert_eq!(
            b["input"],
            json!([{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]}])
        );
        // Nothing else: no sampling, no instructions, no include, no tools, no tier.
        let keys: Vec<&str> = b.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            [
                "model",
                "input",
                "stream",
                "stream_options",
                "store",
                "reasoning",
                "max_output_tokens",
                "safety_identifier",
                "prompt_cache_key"
            ]
        );
        assert_eq!(p.settings, vec!["effort max"]);
        assert_eq!(p.applied[0].source, "default");
        assert_eq!(p.applied[1].patch, json!({"max_output_tokens": 128_000}));
        assert!(p.warnings.is_empty());

        assert_eq!(prep("gpt-5.4", &[]).await.unwrap().body["reasoning"]["effort"], "xhigh");
        assert_eq!(prep("gpt-5.5-pro", &[]).await.unwrap().body["reasoning"]["effort"], "xhigh");
        assert_eq!(prep("gpt-5", &[]).await.unwrap().body["reasoning"]["effort"], "high");
        // chat-latest: no effort levels documented, only the summary (as the old bot sent).
        let p = prep("chat-latest", &[]).await.unwrap();
        assert_eq!(p.body["reasoning"], json!({"summary": "detailed"}));
        assert!(p.settings.is_empty());
        // The cap and the context window bound the budget.
        let capped =
            OpenAi::new(DirectConfig { max_tokens_cap: Some(32_000), ..cfg("https://api.openai.com/v1") }).unwrap();
        let p = prep_with(&capped, "gpt-6-luna", &[], &[Turn::user_text("hi")]).await.unwrap();
        assert_eq!(p.body["max_output_tokens"], 32_000);
        let long = vec![Turn::user_text("a".repeat(900_000))];
        let p = prep_with(&openai(), "chat-latest", &[], &long).await.unwrap();
        assert!(p.body["max_output_tokens"].as_u64().unwrap() < 128_000);
        let input = CallInput {
            model: "gpt-6-luna",
            options: &[],
            history: &[Turn::user_text("hi")],
            session_id: None,
            user: None,
            known_context: None,
            max_tokens_override: Some(5_000),
        };
        let p = openai().prepare(&input, &[], &MemBlobs(vec![])).await.unwrap();
        assert_eq!(p.body["max_output_tokens"], 5_000);
        assert!(p.body.get("safety_identifier").is_none() && p.body.get("prompt_cache_key").is_none());
    }

    #[tokio::test]
    async fn unknown_models_get_no_guessed_defaults() {
        let p = prep("gpt-7-nova", &[]).await.unwrap();
        assert!(p.body.get("reasoning").is_none());
        assert!(p.body.get("max_output_tokens").is_none());
        assert_eq!(p.warnings.len(), 1);
        assert!(p.warnings[0].contains("not in the bot's OpenAI model table"), "{}", p.warnings[0]);
        // Options pass to OpenAI, which checks them for the model.
        let p = prep("gpt-7-nova", &[kv("e", "high"), kv("tier", "flex")]).await.unwrap();
        assert_eq!(p.body["reasoning"], json!({"effort": "high"}));
        assert_eq!(p.body["service_tier"], "flex");
    }

    #[tokio::test]
    async fn options_map_to_the_request() {
        let p = prep("gpt-6.1-sol", &[flag("sc"), kv("e", "low"), kv("v", "high"), kv("tier", "flex")]).await.unwrap();
        let b = &p.body;
        assert_eq!(b["reasoning"], json!({"effort": "low", "summary": "detailed"}));
        assert_eq!(
            b["tools"],
            json!([{"type": "web_search"}, {"type": "code_interpreter", "container": {"type": "auto"}}])
        );
        assert_eq!(b["include"], json!(["web_search_call.action.sources", "code_interpreter_call.outputs"]));
        assert_eq!(b["text"], json!({"verbosity": "high"}));
        assert_eq!(b["service_tier"], "flex");
        assert_eq!(p.settings, vec!["effort low", "web search", "code execution", "verbosity high", "tier flex"]);
        assert!(p.applied.iter().all(|a| a.source != "default" || a.patch.get("reasoning").is_none()));
        // e=none turns reasoning off where the model can (no summary then).
        let p = prep("gpt-6-luna", &[kv("e", "none")]).await.unwrap();
        assert_eq!(p.body["reasoning"], json!({"effort": "none"}));
        assert_eq!(p.settings, vec!["reasoning off"]);
        // Fast mode under both names, ultrafast where offered.
        assert_eq!(prep("gpt-6-sol", &[kv("tier", "priority")]).await.unwrap().body["service_tier"], "priority");
        assert_eq!(prep("gpt-6-sol", &[kv("tier", "fast")]).await.unwrap().body["service_tier"], "fast");
        assert_eq!(prep("gpt-6-astra", &[kv("tier", "ultrafast")]).await.unwrap().body["service_tier"], "ultrafast");
        // default / auto change nothing.
        assert!(prep("gpt-6-sol", &[kv("tier", "default")]).await.unwrap().body.get("service_tier").is_none());
        // Presets apply under the user's options.
        let input = CallInput {
            model: "gpt-6-sol",
            options: &[kv("e", "high")],
            history: &[Turn::user_text("hi")],
            session_id: None,
            user: None,
            known_context: None,
            max_tokens_override: None,
        };
        let preset = request::parse(&[kv("e", "low"), flag("s")]).unwrap();
        let p = openai().prepare(&input, &[preset], &MemBlobs(vec![])).await.unwrap();
        assert_eq!(p.body["reasoning"]["effort"], "high");
        assert_eq!(p.body["tools"], json!([{"type": "web_search"}]));
        assert_eq!(p.applied[0].source, "option");
        assert!(p.applied.iter().any(|a| a.source == "default" && a.patch["tools"][0]["type"] == "web_search"));
    }

    #[tokio::test]
    async fn what_openai_does_not_offer_is_an_error() {
        assert!(err("gpt-6.1-sol", &[flag("u")]).await.contains("has no URL fetching (`+u`)"));
        assert!(err("gpt-6.1-sol", &[flag("m")]).await.contains("has no Google Maps"));
        assert_eq!(
            err("gpt-6.1-sol", &[kv("e", "none")]).await,
            "`gpt-6.1-sol` cannot turn reasoning off (e=none) via OpenAI"
        );
        assert!(err("gpt-6-astra", &[kv("e", "none")]).await.contains("cannot turn reasoning off"));
        assert!(err("gpt-5.5", &[kv("e", "max")]).await.contains("levels: xhigh, high, medium, low, none"));
        assert!(err("gpt-5", &[kv("e", "none")]).await.contains("cannot turn reasoning off"));
        assert!(err("chat-latest", &[kv("e", "high")]).await.contains("has no reasoning effort levels via OpenAI"));
        assert_eq!(err("chat-latest", &[kv("v", "low")]).await, "`chat-latest` has no verbosity setting");
        assert!(err("gpt-6-sol", &[kv("v", "max")]).await.contains("levels: low, medium, high"));
        assert!(err("chat-latest", &[kv("tier", "flex")]).await.contains("has no service tiers via OpenAI"));
        assert!(err("gpt-6-luna", &[kv("tier", "ultrafast")]).await.contains("tiers: flex, priority, fast"));
        assert!(err("gpt-5.4-nano", &[kv("tier", "fast")]).await.contains("tiers: flex"));
        assert!(err("gpt-6-sol", &[kv("tier", "scale")]).await.contains("not available"));
        assert!(err("gpt-6-sol", &[kv("budget", "1000")]).await.contains("no reasoning token budget"));
        assert!(err("gpt-6-sol", &[flag("4")]).await.contains("no image size/aspect options"));
        assert_eq!(err("gpt-6-sol", &[kv("p", "azure")]).await, "`p=` (provider choice) does not apply to OpenAI");
        assert!(err("gpt-6-sol", &[flag("x")]).await.contains("unknown option"));
    }

    #[tokio::test]
    async fn media_use_the_highest_documented_detail() {
        let (img, b1) = photo("image/jpeg", b"\xff\xd8");
        let (pdf, b2) = file("a.pdf", "application/pdf", b"%PDF");
        let blobs = MemBlobs(vec![b1, b2]);
        let history = vec![user(vec![img, pdf, Part::text("?")])];
        for (model, detail) in [
            ("gpt-6-astra", "original"),
            ("gpt-5.4-nano", "original"),
            ("gpt-6.1-sol", "high"),
            ("chat-latest", "high"),
        ] {
            let input = CallInput {
                model,
                options: &[],
                history: &history,
                session_id: None,
                user: None,
                known_context: None,
                max_tokens_override: None,
            };
            let p = openai().prepare(&input, &[], &blobs).await.unwrap();
            let content = &p.body["input"][0]["content"];
            assert_eq!(content[0]["detail"], detail, "{model}");
            assert_eq!(content[1]["detail"], "high", "{model}");
            assert!(p.applied.iter().any(|a| a.patch == json!({"input_image": {"detail": detail}})), "{model}");
        }
    }

    fn usage(input: u64, cached: u64, written: u64, output: u64) -> Usage {
        Usage {
            input_tokens: Some(input),
            cached_input_tokens: Some(cached),
            cache_write_tokens: Some(written),
            output_tokens: Some(output),
            ..Default::default()
        }
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn cost_follows_the_price_table() {
        let std = json!({"service_tier": "default"});
        // gpt-6.1-sol: $2 in, $0.1 cached, $2.5 cache write, $10 out.
        let c = estimate_cost("gpt-6.1-sol", &std, &usage(200_000, 100_000, 50_000, 10_000)).unwrap();
        assert!(close(c, (50_000.0 * 2.0 + 100_000.0 * 0.1 + 50_000.0 * 2.5 + 10_000.0 * 10.0) / 1e6), "{c}");
        // 12-hour cache writes cost twice the 30-minute ones.
        let mut u = usage(100_000, 0, 100_000, 0);
        u.raw = json!({"input_tokens_details": {"cache_write_tokens_details": {"ttl_12h_tokens": 100_000, "ttl_30m_tokens": 0}}});
        assert!(close(estimate_cost("gpt-6.1-sol", &std, &u).unwrap(), 0.5));
        // Over 272K input: 2× input, 1.5× output for the whole request.
        let c = estimate_cost("gpt-6-astra", &std, &usage(300_000, 0, 0, 10_000)).unwrap();
        assert!(close(c, (300_000.0 * 20.0 + 10_000.0 * 75.0) / 1e6), "{c}");
        // …but not on models without long-context pricing.
        let c = estimate_cost("gpt-5.4-mini", &std, &usage(300_000, 0, 0, 0)).unwrap();
        assert!(close(c, 300_000.0 * 0.75 / 1e6), "{c}");
        // The tier the response reports: flex half, fast (reported as priority) per model, ultrafast 6×.
        let short = usage(200_000, 0, 0, 0);
        assert!(close(estimate_cost("gpt-6-sol", &json!({"service_tier": "flex"}), &short).unwrap(), 0.2));
        assert!(close(estimate_cost("gpt-6-sol", &json!({"service_tier": "priority"}), &short).unwrap(), 0.8));
        assert!(close(estimate_cost("gpt-5.5", &json!({"service_tier": "priority"}), &short).unwrap(), 2.5));
        assert!(close(estimate_cost("gpt-6.1-sol", &json!({"service_tier": "ultrafast"}), &short).unwrap(), 2.4));
        // Long context under Flex: half the long-context rate ($2 instead of $4 per 1M input).
        let c = estimate_cost("gpt-6-sol", &json!({"service_tier": "flex"}), &usage(300_000, 0, 0, 0)).unwrap();
        assert!(close(c, 300_000.0 * 2.0 / 1e6), "{c}");
        // Tool fees: billable searches from tool_usage, else search actions; one session per container.
        let r = json!({"service_tier": "default", "tool_usage": {"web_search": {"num_requests": 3}},
            "output": [{"type": "code_interpreter_call", "container_id": "cntr_1"}, {"type": "code_interpreter_call", "container_id": "cntr_1"},
                       {"type": "code_interpreter_call", "container_id": "cntr_2"}]});
        assert!(close(estimate_cost("gpt-6-luna", &r, &usage(0, 0, 0, 0)).unwrap(), 0.03 + 0.06));
        let r = json!({"output": [{"type": "web_search_call", "action": {"type": "search"}}, {"type": "web_search_call", "action": {"type": "open_page"}}]});
        assert!(close(estimate_cost("gpt-6-luna", &r, &usage(0, 0, 0, 0)).unwrap(), 0.01));
        assert!(estimate_cost("gpt-7", &std, &short).is_none());
    }

    #[test]
    fn errors_are_classified() {
        let http = |status: u16, retry_after: Option<u64>, body: &str| {
            crate::chatstream::http_error(status, retry_after, body, error_from_json)
        };
        // Probed 2026-11-12 (no key / a wrong key).
        let e = http(
            401,
            None,
            r#"{"error":{"message":"Missing bearer or basic authentication in header","type":"invalid_request_error","param":null,"code":null}}"#,
        );
        assert_eq!((e.retry, e.error_type.as_deref()), (Retry::Never, Some("invalid_request_error")));
        let e = http(
            401,
            None,
            r#"{"error":{"message":"Incorrect API key provided: sk-invalid.","type":"invalid_request_error","code":"invalid_api_key","param":null},"status":401}"#,
        );
        assert_eq!((e.retry, e.error_type.as_deref()), (Retry::Never, Some("invalid_api_key")));
        assert_eq!(e.details.as_ref().unwrap()["status"], 401, "the whole body is kept");
        // Rate limits wait; billing 429s never retry.
        let e = http(
            429,
            Some(7),
            r#"{"error":{"message":"Rate limit reached for gpt-6.1-sol on tokens per min (TPM)","type":"tokens","param":null,"code":"rate_limit_exceeded"}}"#,
        );
        assert_eq!(e.retry, Retry::After(7));
        let e = http(
            429,
            None,
            r#"{"error":{"message":"slow down","type":"rate_limit_error","param":null,"code":"slow_down"}}"#,
        );
        assert_eq!(e.retry, Retry::After(20));
        let e = http(
            429,
            None,
            r#"{"error":{"message":"You exceeded your current quota","type":"insufficient_quota","param":null,"code":"insufficient_quota"}}"#,
        );
        assert_eq!(e.retry, Retry::Never);
        let e = http(
            429,
            None,
            r#"{"error":{"message":"Spend limit reached","type":"insufficient_quota","param":null,"code":"project_spend_limit_exceeded"}}"#,
        );
        assert_eq!(e.retry, Retry::Never);
        let e = http(
            429,
            None,
            r#"{"error":{"message":"Resource Unavailable","type":"server_error","param":null,"code":null}}"#,
        );
        assert_eq!(e.retry, Retry::After(20));
        // Context overflow → the bot shrinks the budget.
        let e = http(
            400,
            None,
            r#"{"error":{"message":"Your input exceeds the context window of this model. Please adjust your input and try again.","type":"invalid_request_error","param":"input","code":"context_length_exceeded"}}"#,
        );
        assert!(e.is_context_overflow());
        assert_eq!(e.retry, Retry::Never);
        let e = http(
            400,
            None,
            r#"{"error":{"message":"Unsupported value: 'none'","type":"invalid_request_error","param":"reasoning.effort","code":"unsupported_value"}}"#,
        );
        assert_eq!((e.retry, e.error_type.as_deref()), (Retry::Never, Some("unsupported_value")));
        assert!(!e.is_context_overflow());
        assert_eq!(
            http(500, None, r#"{"error":{"message":"x","type":"server_error","param":null,"code":null}}"#).retry,
            Retry::Backoff
        );
        assert_eq!(http(503, Some(30), "upstream connect error").retry, Retry::After(30));
        // In-band (`response.failed`, `error` events).
        let ib = |v: Value| error_from_json(&v, None, None, ErrorKind::InBand);
        assert_eq!(
            ib(json!({"code": "server_error", "message": "The model failed to generate a response."})).retry,
            Retry::Backoff
        );
        assert_eq!(ib(json!({"code": "rate_limit_exceeded", "message": "x"})).retry, Retry::After(20));
        assert_eq!(ib(json!({"code": null, "message": "An error occurred"})).retry, Retry::Backoff);
        for code in ["invalid_prompt", "bio_policy", "misalignment_policy_violation", "invalid_image", "ERR_SOMETHING"]
        {
            let e = ib(json!({"code": code, "message": "x"}));
            assert_eq!(e.retry, Retry::Never, "{code}");
            assert!(!e.is_context_overflow(), "{code}: on OpenAI invalid_prompt is a policy block, not an overflow");
        }
        assert!(ib(json!({"code": "context_length_exceeded", "message": "x"})).is_context_overflow());
    }

    fn sse(events: &[Value]) -> Vec<String> {
        events.iter().map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap())).collect()
    }

    async fn call(base: String, p: &Prepared) -> (Result<Value, LlmError>, CallOutcome, Vec<StreamEvent>) {
        let o = OpenAi::new(cfg(&base)).unwrap();
        let p = Prepared { url: o.cfg.url("/responses"), ..p.clone() };
        let mut events = Vec::new();
        let mut sink = |e: StreamEvent| events.push(e);
        let (r, outcome) = o.stream(&p, &mut sink, &crate::NullObserver, &CancellationToken::new()).await;
        (r, outcome, events)
    }

    #[tokio::test]
    async fn streams_end_to_end() {
        let p = prep("gpt-6.1-sol", &[flag("s")]).await.unwrap();
        let events = [
            json!({"type": "response.created", "sequence_number": 0, "response": {"id": "resp_9", "status": "in_progress", "model": "gpt-6.1-sol-2026-10-20", "output": []}}),
            json!({"type": "response.output_item.added", "sequence_number": 1, "output_index": 0, "item": {"type": "web_search_call", "id": "ws_1", "status": "in_progress"}}),
            json!({"type": "response.output_item.done", "sequence_number": 2, "output_index": 0, "item": {"type": "web_search_call", "id": "ws_1", "status": "completed", "action": {"type": "search", "query": "q", "sources": [{"type": "url", "url": "https://a.b"}]}}}),
            json!({"type": "response.output_item.added", "sequence_number": 3, "output_index": 1, "item": {"type": "message", "id": "msg_1", "status": "in_progress", "role": "assistant", "content": [], "phase": "final_answer"}}),
            json!({"type": "response.output_text.delta", "sequence_number": 4, "output_index": 1, "item_id": "msg_1", "content_index": 0, "delta": "Hello", "logprobs": []}),
            json!({"type": "response.output_item.done", "sequence_number": 5, "output_index": 1, "item": {"type": "message", "id": "msg_1", "status": "completed", "role": "assistant", "phase": "final_answer", "content": [{"type": "output_text", "text": "Hello", "annotations": [], "logprobs": []}]}}),
            json!({"type": "response.completed", "sequence_number": 6, "response": {"id": "resp_9", "status": "completed", "service_tier": "default",
                "output": [
                    {"type": "web_search_call", "id": "ws_1", "status": "completed", "action": {"type": "search", "query": "q", "sources": [{"type": "url", "url": "https://a.b"}]}},
                    {"type": "message", "id": "msg_1", "status": "completed", "role": "assistant", "phase": "final_answer", "content": [{"type": "output_text", "text": "Hello", "annotations": [], "logprobs": []}]}
                ],
                "usage": {"input_tokens": 1000, "input_tokens_details": {"cached_tokens": 0, "cache_write_tokens": 0}, "output_tokens": 500, "output_tokens_details": {"reasoning_tokens": 400}, "total_tokens": 1500},
                "tool_usage": {"web_search": {"num_requests": 1}, "image_gen": {"input_tokens": 0, "input_tokens_details": {"image_tokens": 0, "text_tokens": 0}, "output_tokens": 0, "total_tokens": 0}}}}),
        ];
        let (base, captured) = serve(200, "text/event-stream", "x-request-id: req_abc\r\n", sse(&events)).await;
        let (r, outcome, evs) = call(base, &p).await;
        let native = r.unwrap();
        assert_eq!(native["output"][1]["phase"], "final_answer");
        assert_eq!(outcome.upstream_id.as_deref(), Some("resp_9"));
        let req = captured.lock().unwrap().take().unwrap();
        assert_eq!(req.path, "/v1/responses");
        assert_eq!(req.auth.as_deref(), Some("Bearer sk-test"));
        assert_eq!(req.body, p.body);
        assert!(matches!(&evs[0], StreamEvent::Meta { upstream_id: Some(id), .. } if id == "req_abc"));
        let usage =
            evs.iter().find_map(|e| if let StreamEvent::Usage(u) = e { Some(u.clone()) } else { None }).unwrap();
        assert_eq!(usage.cost_source, CostSource::Estimated);
        assert!(close(usage.cost_usd.unwrap(), (1000.0 * 2.0 + 500.0 * 10.0) / 1e6 + 0.01));
        assert!(matches!(evs.last(), Some(StreamEvent::Finish { reason: Some(r), .. }) if r == "stop"));
        let mut acc = crate::Accumulator::new();
        evs.into_iter().for_each(|e| acc.apply(e));
        assert!(
            matches!(&acc.parts()[0], Part::ToolCall(t) if t.output == Some(json!({"sources": [{"type": "url", "url": "https://a.b"}]})))
        );
        assert_eq!(acc.turn().answer_text(), "Hello");
    }

    #[tokio::test]
    async fn broken_streams_and_http_errors() {
        let p = prep("gpt-6-luna", &[]).await.unwrap();
        let start = json!({"type": "response.created", "response": {"id": "resp_1", "status": "in_progress"}});
        let delta =
            json!({"type": "response.output_text.delta", "output_index": 0, "content_index": 0, "delta": "Hel"});
        // Cut off before response.completed: a retryable network error.
        let (base, _) = serve(200, "text/event-stream", "", sse(&[start.clone(), delta.clone()])).await;
        let (r, _, _) = call(base, &p).await;
        let e = r.unwrap_err();
        assert_eq!((e.kind, e.retry, e.partial_output), (ErrorKind::Network, Retry::Backoff, true));
        // `[DONE]` without response.completed is cut off too.
        let mut chunks = sse(&[start.clone(), delta]);
        chunks.push("data: [DONE]\n\n".into());
        let (base, _) = serve(200, "text/event-stream", "", chunks).await;
        let e = call(base, &p).await.0.unwrap_err();
        assert_eq!(e.message, "stream ended without response.completed");
        // In-band failure after the stream started; its usage is still reported (billed).
        let failed = json!({"type": "response.failed", "response": {"id": "resp_1", "status": "failed",
            "error": {"code": "bio_policy", "message": "blocked"}, "usage": {"input_tokens": 10, "output_tokens": 0, "total_tokens": 10}}});
        let (base, _) = serve(200, "text/event-stream", "", sse(&[start, failed])).await;
        let (r, _, evs) = call(base, &p).await;
        let e = r.unwrap_err();
        assert_eq!((e.kind, e.retry, e.error_type.as_deref()), (ErrorKind::InBand, Retry::Never, Some("bio_policy")));
        assert!(
            matches!(evs.last(), Some(StreamEvent::Usage(u)) if u.input_tokens == Some(10) && u.cost_usd.is_some())
        );
        // HTTP errors keep the body verbatim.
        let body = r#"{"error":{"message":"You exceeded your current quota","type":"insufficient_quota","param":null,"code":"insufficient_quota"}}"#;
        let (base, _) = serve(429, "application/json", "retry-after: 3\r\n", vec![body.into()]).await;
        let e = call(base, &p).await.0.unwrap_err();
        assert_eq!((e.kind, e.status, e.retry), (ErrorKind::Http, Some(429), Retry::Never));
        assert_eq!(e.details, Some(serde_json::from_str::<Value>(body).unwrap()));
    }
}
