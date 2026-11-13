//! Anthropic's own API: the Messages API, `POST {base}/messages`, streaming.
//!
//! Spec: docs/research/anthropic.md §0–9 and the official docs (fetched
//! 2026-11-12). What is sent, and why:
//!
//! - **Headers**: `x-api-key`, `anthropic-version: 2026-10-21` for every
//!   model. It is the current version; what it changes for the Messages API is
//!   that `temperature`/`top_p`/`top_k` are rejected (the bot never sends
//!   sampling parameters), a `document` block directly in an assistant message
//!   is rejected (replayed turns only hold documents inside web-fetch results,
//!   which stay accepted) and user text blocks may not carry citations (the
//!   bot sends none). `anthropic-beta` only for features that still need one:
//!   `fast-mode-2026-02-01` (fast mode on Opus 5 and Opus 4.8) and
//!   `interleaved-thinking-2025-05-14` (thinking between tool calls with a
//!   manual budget on Opus 4.5, Sonnet 4.5, Sonnet 4.6). Both are listed in
//!   `applied` (/info).
//! - **Thinking** (D3, the highest the model offers): adaptive thinking with
//!   `output_config.effort` at the model's top level, `thinking.display:
//!   "summarized"` where the default `omitted` would hide the thinking; on
//!   budget-only models (the 4.5 generation) the largest valid budget
//!   (`max_tokens - 1`), plus `effort: high` on Opus 4.5. `e=` takes the
//!   model's documented levels; `e=none` sends `disabled`, or `between_tools`
//!   on Sonnet 5.5 (its documented thinking-off setting), and is an error on
//!   Fable/Mythos 5.x and Opus 5.5. `budget=` only where manual thinking is
//!   accepted (4.5 generation; deprecated on Opus/Sonnet 4.6).
//! - **`max_tokens`** (required): the automatic budget (A2) from the model's
//!   context and max output.
//! - **Tools** (only when asked): `s` web search, `u` web fetch, `c` code
//!   execution, Anthropic's own server tools, at their latest versions
//!   (`web_search_20260318`, `web_fetch_20260318` with dynamic filtering on
//!   Claude 4.6 and later, the basic `_20250305` / `_20250910` before;
//!   `code_execution_20260521`), with their defaults, except that web fetch
//!   has citations on (`citations: {enabled: true}`, off by default) like
//!   every document (see [`history`]). No container is reused across turns
//!   (stateless, D28); within a turn the container goes along with a
//!   `pause_turn` continuation, as the API requires. Files that code
//!   execution writes are fetched from the Files API as their tool result
//!   arrives and shown there ([`files`]).
//! - **Cache diagnostics**: `diagnostics: {previous_message_id}` with the
//!   message id of the latest answer of the same model in the conversation
//!   (`null` without one; a `pause_turn` continuation names the paused
//!   message). Free and stable on the Claude API; it changes nothing the model
//!   sees. A miss the API attributes to a change in the request is a notice;
//!   the result is in the response either way (`/info`).
//! - **Prompt caching**: automatic caching (top-level `cache_control`,
//!   5-minute TTL) plus one breakpoint at the end of the previous user turn,
//!   where the previous request's cache entry is. Caching never changes the
//!   output ("the response you receive is identical"), cache markers are not
//!   part of the preserved-thinking prefix, and follow-ups, retries and the
//!   iterations of a server-tool loop (the API adds 5-minute breakpoints after
//!   tool results once a request caches) read the conversation at 0.1×
//!   (0.05×, 0.025× on some models) instead of 1× input; the price is 1.25×
//!   on input written to the cache (prompts below the model's minimum,
//!   512–4096 tokens, are never cached). The old bot cached every request too.
//! - **`metadata.user_id`**: the hashed user, if any (abuse detection). No
//!   prompt-cache routing key exists on this API.
//! - **Tiers**: `tier=fast` sends `speed: "fast"` on models with fast mode.
//!   Priority Tier is no longer sold and has no request value of its own
//!   (`service_tier: "auto"`, the default, already uses a commitment if the
//!   organization has one), so nothing else is offered.
//! - **`pause_turn`**: when the server-tool loop pauses, the turn is continued
//!   as documented: the same request with the paused content appended as an
//!   assistant message (and the container), up to [`MAX_REQUESTS`] requests.
//!
//! Costs are estimated from the list prices in [`models`]; errors are
//! classified in [`stream::error_from_json`]; a refusal (`stop_reason:
//! refusal`) fails with its `stop_details` ([`stream::Parser::refusal`]).

pub mod files;
pub mod history;
pub mod models;
pub mod stream;

use std::collections::VecDeque;

use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::error::LlmError;
use crate::event::StreamEvent;
use crate::openrouter::options::{Applied, overlay};
use crate::provider::{DirectConfig, Provider};
use crate::request::{self, Requested, RouteCaps, Tool};
use crate::{BlobLoader, CallInput, CallOutcome, EventSink, Observer, Prepared};
use models::{ModelSpec, Off, Thinking};

pub const DIALECT: &str = "anthropic.messages";
pub const NAME: &str = "Anthropic";
pub const API_VERSION: &str = "2026-10-21";
pub const FAST_MODE_BETA: &str = "fast-mode-2026-02-01";
pub const INTERLEAVED_THINKING_BETA: &str = "interleaved-thinking-2025-05-14";
/// `context_management` (the bot uses `clear_thinking_20251015` with
/// `keep: "all"` on models that strip earlier thinking).
pub const CONTEXT_MANAGEMENT_BETA: &str = "context-management-2025-06-27";

/// Requests per turn at most: the first and its `pause_turn` continuations
/// (the docs' own example caps the loop at 5).
pub const MAX_REQUESTS: usize = 5;

/// `max_tokens` for a model the bot does not know (every current Claude
/// model's maximum; a smaller limit is a 400 that the bot answers by
/// retrying at once with a smaller budget).
pub const UNKNOWN_MODEL_MAX_TOKENS: u64 = 128_000;

/// Smallest thinking budget the API accepts.
const MIN_BUDGET: u64 = 1024;

pub struct Anthropic {
    cfg: DirectConfig,
    http: reqwest::Client,
}

impl Anthropic {
    pub fn new(cfg: DirectConfig) -> anyhow::Result<Self> {
        let http = cfg.http()?;
        Ok(Self { cfg, http })
    }

    fn auto_max_tokens(&self, input: &CallInput<'_>, spec: Option<&ModelSpec>) -> u64 {
        match spec {
            Some(s) => {
                let prompt = crate::budget::estimate_prompt(input.history, input.known_context);
                crate::budget::choose(Some(s.context), Some(s.max_output), self.cfg.max_tokens_cap, prompt)
                    .unwrap_or(s.max_output)
            }
            None => self.cfg.max_tokens_cap.map_or(UNKNOWN_MODEL_MAX_TOKENS, |c| c.min(UNKNOWN_MODEL_MAX_TOKENS)),
        }
    }

    fn post(&self, url: &str, model: &str, body: &Value) -> reqwest::RequestBuilder {
        let mut rb = self
            .http
            .post(url)
            .header("x-api-key", &self.cfg.api_key)
            .header("anthropic-version", API_VERSION)
            .json(body);
        let betas = betas(model, body);
        if !betas.is_empty() {
            rb = rb.header("anthropic-beta", betas.join(","));
        }
        rb
    }
}

/// The beta headers a request body needs.
pub fn betas(model: &str, body: &Value) -> Vec<&'static str> {
    let Some(spec) = models::model_spec(model) else { return Vec::new() };
    let mut out = Vec::new();
    if body.get("speed").and_then(Value::as_str) == Some("fast") && spec.fast.is_some_and(|f| f.beta) {
        out.push(FAST_MODE_BETA);
    }
    let manual = body.pointer("/thinking/type").and_then(Value::as_str) == Some("enabled");
    let tools = body.get("tools").and_then(Value::as_array).is_some_and(|t| !t.is_empty());
    if spec.interleaved_beta && manual && tools {
        out.push(INTERLEAVED_THINKING_BETA);
    }
    if body.get("context_management").is_some() {
        out.push(CONTEXT_MANAGEMENT_BETA);
    }
    out
}

/// What the Messages API offers for a model (the platform's own API, so this
/// is the official capability set). Unknown models: everything the API has,
/// left to the API to check. With the request's `max_tokens` (the largest
/// thinking budget is one below it).
struct Caps(Option<&'static ModelSpec>, u64);

const ALL_EFFORTS: &[&str] = &["max", "xhigh", "high", "medium", "low"];

impl RouteCaps for Caps {
    fn name(&self) -> &str {
        NAME
    }
    fn tool(&self, tool: Tool) -> bool {
        // web_search, web_fetch and code_execution run on every current model;
        // Claude has no image generation tool.
        matches!(tool, Tool::Search | Tool::Fetch | Tool::Code)
    }
    fn efforts(&self) -> Option<Vec<String>> {
        let levels = self.0.map_or(ALL_EFFORTS, |s| s.efforts);
        (!levels.is_empty()).then(|| levels.iter().map(|l| l.to_string()).collect())
    }
    fn can_disable_reasoning(&self) -> bool {
        self.0.is_none_or(|s| s.off != Off::Never)
    }
    fn budget(&self) -> bool {
        self.0.is_none_or(|s| matches!(s.thinking, Thinking::Budget | Thinking::Adaptive { budget: true }))
    }
    fn verbosity(&self) -> bool {
        false
    }
    fn image_options(&self) -> bool {
        false
    }
    fn tiers(&self) -> Vec<String> {
        if self.0.is_none_or(|s| s.fast.is_some()) { vec!["fast".into()] } else { Vec::new() }
    }
    fn default_effort(&self) -> Option<String> {
        self.0.map(|s| s.effort.to_string())
    }
    /// Budget thinking (Opus 4.5, Haiku 4.5): `+h` the largest budget, `+l` the smallest.
    fn budget_range(&self) -> Option<(u64, u64)> {
        self.0.filter(|s| s.thinking == Thinking::Budget).map(|_| (self.1.saturating_sub(1), MIN_BUDGET))
    }
}

/// The server tools for the requested options, in a fixed order (the tool
/// list is part of what replayed thinking is checked against); `cite`: web
/// fetch with citations.
fn tool_defs(spec: Option<&ModelSpec>, wanted: &[Tool], cite: bool) -> Vec<Value> {
    let dynamic = spec.is_none_or(|s| s.dynamic_web);
    let mut out = Vec::new();
    if wanted.contains(&Tool::Search) {
        let ty = if dynamic { "web_search_20260318" } else { "web_search_20250305" };
        out.push(json!({"type": ty, "name": "web_search"}));
    }
    if wanted.contains(&Tool::Fetch) {
        let ty = if dynamic { "web_fetch_20260318" } else { "web_fetch_20250910" };
        let mut t = json!({"type": ty, "name": "web_fetch"});
        if cite {
            t["citations"] = json!({"enabled": true});
        }
        out.push(t);
    }
    if wanted.contains(&Tool::Code) {
        out.push(json!({"type": "code_execution_20260521", "name": "code_execution"}));
    }
    out
}

/// The thinking part of a request.
#[derive(Debug, Default)]
struct ThinkingPlan {
    thinking: Option<Value>,
    effort: Option<String>,
    described: Vec<String>,
}

fn plan_thinking(
    model: &str,
    spec: Option<&ModelSpec>,
    req: &Requested,
    max_tokens: u64,
    interleaved: bool,
) -> Result<ThinkingPlan, String> {
    let check_budget = |n: u64| -> Result<(), String> {
        if n < MIN_BUDGET {
            return Err(format!("budget= must be at least {MIN_BUDGET} tokens on {NAME} (got {n})"));
        }
        if n >= max_tokens && !interleaved {
            return Err(format!("budget= must be below max_tokens ({max_tokens}) for `{model}` (got {n})"));
        }
        Ok(())
    };
    let summarized = |mut t: Value| {
        if spec.is_none_or(|s| s.display_omitted) {
            t["display"] = json!("summarized");
        }
        t
    };
    let largest_budget = || -> Result<u64, String> {
        let n = max_tokens.saturating_sub(1);
        if n < MIN_BUDGET {
            return Err(format!(
                "max_tokens {max_tokens} leaves no room for a thinking budget (at least {MIN_BUDGET}); use e=none"
            ));
        }
        Ok(n)
    };
    let mut p = ThinkingPlan::default();
    match (req.effort.as_deref(), req.budget) {
        (Some("none"), _) => match spec.map_or(Off::Disabled, |s| s.off) {
            Off::Disabled => {
                p.thinking = Some(json!({"type": "disabled"}));
                p.described.push("thinking off".into());
            }
            Off::BetweenTools => {
                p.thinking = Some(json!({"type": "between_tools"}));
                p.described.push("thinking between_tools (no up-front thinking)".into());
            }
            Off::Never => return Err(format!("`{model}` cannot turn reasoning off (e=none) via {NAME}")),
        },
        (Some(e), _) => {
            if spec.is_some_and(|s| s.thinking == Thinking::Budget) {
                // Opus 4.5: effort composes with the (largest) thinking budget.
                let n = largest_budget()?;
                p.thinking = Some(json!({"type": "enabled", "budget_tokens": n}));
                p.described.push(format!("effort {e}"));
                p.described.push(format!("thinking budget {n}"));
            } else {
                p.thinking = Some(summarized(json!({"type": "adaptive"})));
                p.described.push(format!("effort {e}"));
            }
            p.effort = Some(e.to_string());
        }
        (None, Some(n)) => {
            if req.level.is_some() && n < MIN_BUDGET {
                // +h on a max_tokens too small for any budget.
                largest_budget()?;
            }
            check_budget(n)?;
            p.thinking = Some(summarized(json!({"type": "enabled", "budget_tokens": n})));
            // Opus 4.5: the effort (its highest by default) composes with the budget.
            if let Some(e) = spec.filter(|s| s.thinking == Thinking::Budget).and_then(|s| s.efforts.first()) {
                p.effort = Some(e.to_string());
                p.described.push(format!("effort {e}"));
            }
            p.described.push(format!("thinking budget {n}"));
        }
        (None, None) => match spec {
            // A model the bot does not know: the API's own defaults.
            None => p.described.push("thinking: API default".into()),
            Some(s) => {
                let top = s.efforts.first().copied();
                match s.thinking {
                    Thinking::Adaptive { .. } => {
                        p.thinking = Some(summarized(json!({"type": "adaptive"})));
                    }
                    Thinking::Budget => {
                        let n = largest_budget()?;
                        p.thinking = Some(json!({"type": "enabled", "budget_tokens": n}));
                        p.described.push(format!("thinking budget {n}"));
                    }
                }
                if let Some(e) = top {
                    p.effort = Some(e.to_string());
                    p.described.insert(0, format!("effort {e}"));
                }
            }
        },
    }
    Ok(p)
}

#[async_trait::async_trait]
impl Provider for Anthropic {
    fn id(&self) -> &'static str {
        "anthropic"
    }
    fn name(&self) -> &'static str {
        NAME
    }
    fn knows(&self, model: &str) -> bool {
        models::model_spec(model).is_some()
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
        let model = input.model;
        let spec = models::model_spec(model);
        let user = request::parse(input.options).map_err(LlmError::request)?;
        let mut req = Requested::default();
        for p in presets {
            req = overlay(&req, p);
        }
        let mut req = overlay(&req, &user);
        if req.providers.is_some() {
            return Err(LlmError::request(format!("`p=` (provider choice) does not apply to {NAME}")));
        }
        let max_tokens = input.max_tokens_override.unwrap_or_else(|| self.auto_max_tokens(input, spec));
        let caps = Caps(spec, max_tokens);
        request::validate(model, &req, &caps).map_err(LlmError::request)?;
        let level_note = request::resolve_reasoning(model, &mut req, &caps).map_err(LlmError::request)?;
        let src = |by_user: bool| if by_user { "option" } else { "default" }.to_string();
        let mut applied = vec![Applied { source: "default".into(), patch: json!({"anthropic-version": API_VERSION}) }];
        let mut settings = Vec::new();

        let cite = history::citations_allowed(input.history, model);
        let tools = tool_defs(spec, &req.tools, cite);
        let interleaved = spec.is_some_and(|s| s.interleaved_beta) && !tools.is_empty();
        let think = plan_thinking(model, spec, &req, max_tokens, interleaved).map_err(LlmError::request)?;
        let tools_value = (!tools.is_empty()).then(|| Value::Array(tools.clone()));
        let mut conv = history::convert(input.history, model, spec, tools_value.as_ref(), cite, blobs).await?;
        let marker = history::mark_previous_user_turn(&mut conv.messages);

        let mut body = json!({"model": model, "max_tokens": max_tokens, "stream": true});
        if let Some(s) = conv.system.take() {
            body["system"] = s;
        }
        body["messages"] = Value::Array(std::mem::take(&mut conv.messages));
        let mut reasoning = serde_json::Map::new();
        if let Some(t) = &think.thinking {
            body["thinking"] = t.clone();
            reasoning.insert("thinking".into(), t.clone());
        }
        if let Some(e) = &think.effort {
            body["output_config"] = json!({"effort": e});
            reasoning.insert("output_config".into(), body["output_config"].clone());
        }
        if !reasoning.is_empty() {
            applied.push(Applied {
                source: src(user.effort.is_some() || user.budget.is_some() || user.level.is_some()),
                patch: Value::Object(reasoning),
            });
        }
        settings.extend(think.described);
        // Earlier turns' thinking, which this model strips by default, stays in
        // context (owner, 2026-11-14). The strategy needs thinking on.
        let thinking_on =
            think.thinking.as_ref().and_then(|t| t["type"].as_str()).is_some_and(|t| t == "enabled" || t == "adaptive");
        if spec.is_some_and(|s| s.strips_thinking) && thinking_on {
            let cm = json!({"edits": [{"type": "clear_thinking_20251015", "keep": "all"}]});
            body["context_management"] = cm.clone();
            applied.push(Applied { source: "default".into(), patch: json!({"context_management": cm}) });
            settings.push("earlier thinking kept".into());
        }
        let manual = think.thinking.as_ref().and_then(|t| t["type"].as_str()) == Some("enabled");
        if interleaved && manual {
            settings.push("interleaved thinking".into());
        }
        applied.push(Applied { source: "budget".into(), patch: json!({"max_tokens": max_tokens}) });
        for (t, def) in
            [Tool::Search, Tool::Fetch, Tool::Code].into_iter().filter(|t| req.tools.contains(t)).zip(&tools)
        {
            settings.push(t.describe().to_string());
            applied.push(Applied { source: src(user.tools.contains(&t)), patch: json!({"tools": [def]}) });
        }
        if let Some(t) = tools_value {
            body["tools"] = t;
        }
        let cache = json!({"type": "ephemeral"});
        body["cache_control"] = cache.clone();
        applied.push(Applied { source: "default".into(), patch: json!({"cache_control": cache}) });
        if let Some(at) = marker {
            applied.push(Applied { source: "default".into(), patch: json!({format!("{at}.cache_control"): cache}) });
        }
        if conv.documents && cite {
            applied.push(Applied {
                source: "default".into(),
                patch: json!({"messages[].content[document].citations": {"enabled": true}}),
            });
        }
        let diagnostics = json!({"previous_message_id": history::previous_message_id(input.history, model)});
        body["diagnostics"] = diagnostics.clone();
        applied.push(Applied { source: "default".into(), patch: json!({"diagnostics": diagnostics}) });
        if let Some(u) = &input.user {
            body["metadata"] = json!({"user_id": u});
        }
        if req.tier.as_deref() == Some("fast") {
            body["speed"] = json!("fast");
            settings.push("tier fast".into());
            applied.push(Applied { source: src(user.tier.is_some()), patch: json!({"speed": "fast"}) });
        }
        let betas = betas(model, &body);
        if !betas.is_empty() {
            applied.push(Applied { source: "default".into(), patch: json!({"anthropic-beta": betas.join(",")}) });
        }

        let notes: Vec<String> = level_note.into_iter().chain(conv.notes).collect();
        // Container time is billed apart and not in the estimate (free with web
        // search or fetch): a marker after the cost (`crate::Prepared::settings`).
        let free_code = spec.is_none_or(|s| s.dynamic_web)
            && (req.tools.contains(&Tool::Search) || req.tools.contains(&Tool::Fetch));
        if req.tools.contains(&Tool::Code) && !free_code {
            settings.push("+ code execution".into());
        }
        let mut warnings = std::mem::take(&mut conv.warnings);
        if spec.is_none() {
            warnings.push(format!(
                "`{model}` is not in the bot's {NAME} model table: limits, capabilities and price unknown \
                 (max_tokens {max_tokens}; thinking and effort at the API's defaults unless set with e= or budget=; \
                 options are left to the API to check; cost not estimated)"
            ));
        }
        Ok(Prepared {
            dialect: DIALECT,
            model: model.to_string(),
            url: self.cfg.url("/messages"),
            body,
            applied,
            warnings,
            notes,
            show_debug: false,
            auto_max_tokens: Some(max_tokens),
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
        let tools = p.body.get("tools").cloned();
        let mut parser = stream::Parser::with_request(models::model_spec(&p.model), &p.body);
        let mut body = p.body.clone();
        let files = files::Fetcher::new(self.http.clone(), &self.cfg.base_url, &self.cfg.api_key, API_VERSION);
        let mut out = TurnSink::new(sink, files);
        let mut requests = 0;
        loop {
            requests += 1;
            let rb = self.post(&p.url, &p.model, &body);
            let crate::chatstream::Streamed { result, parser: back, .. } = crate::chatstream::run_with(
                rb,
                &p.url,
                &body,
                self.cfg.idle_timeout,
                Some("request-id"),
                stream::error_from_json,
                parser,
                &mut out,
                observer,
                cancel,
            )
            .await;
            parser = back;
            // Files still being fetched come before anything that follows.
            out.drain(cancel).await;
            let outcome = CallOutcome { upstream_id: out.joined(), ..Default::default() };
            if let Err(e) = result {
                return (Err(e), outcome);
            }
            if !parser.paused() {
                if let Some(e) = parser.refusal() {
                    return (Err(e), outcome);
                }
                return (Ok(parser.native_payload(tools.as_ref())), outcome);
            }
            let content = parser.content();
            if requests == MAX_REQUESTS || content.is_empty() {
                out.event(StreamEvent::Notice(format!(
                    "the API paused the turn (pause_turn) after {requests} request(s) and the bot did not continue it: \
                     the answer may be incomplete"
                )));
                out.event(StreamEvent::Finish { reason: Some("pause_turn".into()), native: Some("pause_turn".into()) });
                return (Ok(parser.native_payload(tools.as_ref())), outcome);
            }
            // The documented continuation: the paused content goes back as an
            // assistant message, with the same tools and the same container.
            if let Some(m) = body.get_mut("messages").and_then(Value::as_array_mut) {
                m.push(json!({"role": "assistant", "content": content}));
            }
            if let Some(c) = parser.container() {
                body["container"] = json!(c);
            }
            // The paused message is the previous request now.
            if let Some(id) = parser.message_id()
                && body.get("diagnostics").is_some()
            {
                body["diagnostics"] = json!({"previous_message_id": id});
            }
            out.event(StreamEvent::Notice(
                "the API paused its server-tool loop (pause_turn); the bot continued the turn with another request"
                    .into(),
            ));
            parser.next_request();
        }
    }
}

/// The turn's events on their way to the bot: the upstream ids of all its
/// requests (the `request-id` of each, joined), and the files that code
/// execution wrote, fetched as soon as their tool result arrives. Events
/// after such a result wait until its files are in (strict order, D30); the
/// stream itself is read on meanwhile.
struct TurnSink<'a> {
    inner: &'a mut dyn EventSink,
    ids: Vec<String>,
    files: files::Fetcher,
    queue: VecDeque<Queued>,
}

// Each event passes through the queue once; boxing it would allocate for every one.
#[allow(clippy::large_enum_variant)]
enum Queued {
    Event(StreamEvent),
    /// The events of a file-fetching task, due here.
    Files(tokio::task::JoinHandle<Vec<StreamEvent>>),
}

impl<'a> TurnSink<'a> {
    fn new(inner: &'a mut dyn EventSink, files: files::Fetcher) -> Self {
        Self { inner, ids: Vec::new(), files, queue: VecDeque::new() }
    }

    fn joined(&self) -> Option<String> {
        (!self.ids.is_empty()).then(|| self.ids.join(","))
    }

    /// Pass on what is due: events up to the first fetch still running.
    fn pump(&mut self) {
        loop {
            let ready = match self.queue.front() {
                None => return,
                Some(Queued::Event(_)) => true,
                Some(Queued::Files(h)) => h.is_finished(),
            };
            if !ready {
                return;
            }
            match self.queue.pop_front() {
                Some(Queued::Event(e)) => self.inner.event(e),
                Some(Queued::Files(mut h)) => match futures::FutureExt::now_or_never(&mut h) {
                    Some(r) => {
                        for e in files::joined(r) {
                            self.inner.event(e);
                        }
                    }
                    None => {
                        self.queue.push_front(Queued::Files(h));
                        return;
                    }
                },
                None => return,
            }
        }
    }

    /// Wait for the fetches still running (stopped when cancelled) and pass everything on.
    async fn drain(&mut self, cancel: &CancellationToken) {
        while let Some(q) = self.queue.pop_front() {
            match q {
                Queued::Event(e) => self.inner.event(e),
                Queued::Files(mut h) => {
                    let evs = tokio::select! {
                        r = &mut h => files::joined(r),
                        _ = cancel.cancelled() => {
                            h.abort();
                            Vec::new()
                        }
                    };
                    for e in evs {
                        self.inner.event(e);
                    }
                }
            }
        }
    }
}

impl Drop for TurnSink<'_> {
    fn drop(&mut self) {
        for q in &self.queue {
            if let Queued::Files(h) = q {
                h.abort();
            }
        }
    }
}

impl EventSink for TurnSink<'_> {
    fn event(&mut self, ev: StreamEvent) {
        let ev = match ev {
            StreamEvent::Meta { upstream_id: Some(id), provider, model } => {
                if !self.ids.contains(&id) {
                    self.ids.push(id);
                }
                StreamEvent::Meta { upstream_id: self.joined(), provider, model }
            }
            other => other,
        };
        let fetch = match &ev {
            StreamEvent::Tool(call) => self.files.start(call),
            _ => None,
        };
        self.queue.push_back(Queued::Event(ev));
        if let Some(h) = fetch {
            self.queue.push_back(Queued::Files(h));
        }
        self.pump();
    }
}

#[cfg(test)]
mod tests;
