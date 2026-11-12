//! The Messages stream of the direct API: Anthropic's events, parsed by the
//! shared Messages parser ([`crate::openrouter::messages::EventParser`]) and
//! adjusted here:
//!
//! - in-band `error` events are classified with Anthropic's error types;
//! - thinking text is a summary (the API never returns the raw chain of
//!   thought): reasoning events are `ReasoningKind::Summary`;
//! - tool results get a readable summary (search hits, fetched page, stdout,
//!   error codes) without the opaque encrypted content, and failed results
//!   are marked `failed`;
//! - the upstream id is the `request-id` header (support uses it), not the
//!   message id;
//! - usage is priced from the list (per request), and a refusal before any
//!   output in a category Anthropic does not bill is free;
//! - `pause_turn` (the server-tool loop paused) is not a finish: the caller
//!   continues the turn with another request, which this parser follows too
//!   (content, usage and cost of all requests together).

use omni_core::Usage;
use omni_core::content::{ReasoningKind, ToolCall};
use omni_core::usage::CostSource;
use serde_json::{Map, Value, json};

use super::models::{ModelSpec, estimate_cost};
use crate::error::{ErrorKind, LlmError, Retry, retry_for_status};
use crate::event::StreamEvent;
use crate::openrouter::messages;

/// Refusal categories that Anthropic bills even before any output.
const BILLED_REFUSALS: &[&str] = &["bio", "frontier_llm", "reasoning_extraction"];

/// Stream state of one turn (one or more requests).
pub struct Parser {
    inner: messages::EventParser,
    spec: Option<&'static ModelSpec>,
    /// Content of the earlier requests of this turn (before `pause_turn`).
    earlier: Vec<Vec<Value>>,
    /// Their usage, summed (tokens and cost), and their usage objects.
    earlier_usage: Option<Usage>,
    earlier_raw: Vec<Value>,
    earlier_output: bool,
    /// Content block indices of earlier requests (reasoning blocks stay apart).
    offset: usize,
    // The current request:
    max_index: Option<usize>,
    blocks: usize,
    stop_reason: Option<String>,
    container: Option<String>,
    usage: Option<Usage>,
}

impl Parser {
    pub fn new(spec: Option<&'static ModelSpec>) -> Self {
        Self {
            inner: messages::EventParser::default(),
            spec,
            earlier: Vec::new(),
            earlier_usage: None,
            earlier_raw: Vec::new(),
            earlier_output: false,
            offset: 0,
            max_index: None,
            blocks: 0,
            stop_reason: None,
            container: None,
            usage: None,
        }
    }

    /// The current request ended with `stop_reason: "pause_turn"`.
    pub fn paused(&self) -> bool {
        self.inner.finished && self.stop_reason.as_deref() == Some("pause_turn")
    }

    /// The code-execution container of this turn, if any (a continuation must reuse it).
    pub fn container(&self) -> Option<&str> {
        self.container.as_deref()
    }

    /// The content blocks of the current request, as received.
    pub fn content(&self) -> Vec<Value> {
        self.inner.native_payload().get("content").and_then(Value::as_array).cloned().unwrap_or_default()
    }

    /// Start the next request of the turn (after `pause_turn`).
    pub fn next_request(&mut self) {
        let content = self.content();
        self.earlier.push(content);
        self.offset += self.max_index.map_or(0, |i| i + 1);
        if let Some(u) = self.usage.take() {
            self.earlier_usage = Some(combine(self.earlier_usage.as_ref(), &self.earlier_raw, u.clone()));
            self.earlier_raw.push(u.raw);
        }
        self.earlier_output |= crate::chatstream::EventParser::any_output(&self.inner);
        self.inner = messages::EventParser::default();
        self.max_index = None;
        self.blocks = 0;
        self.stop_reason = None;
    }

    /// The assistant turn for replay: all content blocks, the request's `tools`
    /// (replay needs the same ones), and the number of blocks of each request
    /// when the turn took more than one.
    pub fn native_payload(&self, tools: Option<&Value>) -> Value {
        let current = self.content();
        let mut hops: Vec<usize> = self.earlier.iter().map(Vec::len).collect();
        hops.push(current.len());
        let mut content: Vec<Value> = self.earlier.iter().flatten().cloned().collect();
        content.extend(current);
        let mut p = json!({"role": "assistant", "content": content});
        if let Some(t) = tools {
            p["tools"] = t.clone();
        }
        if hops.len() > 1 {
            p["hops"] = json!(hops);
        }
        p
    }

    pub fn event(&mut self, name: Option<&str>, v: &Value) -> Result<Vec<StreamEvent>, LlmError> {
        let ty = v.get("type").and_then(Value::as_str).or(name).unwrap_or("");
        match ty {
            "error" => {
                let err = v.get("error").cloned().unwrap_or_else(|| v.clone());
                let mut e = error_from_json(&err, None, None, ErrorKind::InBand);
                e.details = Some(v.clone());
                e.partial_output = crate::chatstream::EventParser::any_output(self);
                return Err(e);
            }
            "message_start" => {
                if let Some(c) = v.pointer("/message/container/id").and_then(Value::as_str) {
                    self.container = Some(c.to_string());
                }
            }
            "content_block_start" => {
                self.blocks += 1;
                if let Some(i) = v.get("index").and_then(Value::as_u64) {
                    self.max_index = Some(self.max_index.map_or(i as usize, |m| m.max(i as usize)));
                }
            }
            // A tool input often starts with an empty chunk; skipping it changes
            // nothing, and an input whose chunks are all empty stays the `{}` of
            // the block start (instead of an unparsable "" that cannot be replayed).
            "content_block_delta"
                if v.pointer("/delta/type").and_then(Value::as_str) == Some("input_json_delta")
                    && v.pointer("/delta/partial_json").and_then(Value::as_str) == Some("") =>
            {
                return Ok(Vec::new());
            }
            "message_delta" => {
                if let Some(c) = v.pointer("/delta/container/id").and_then(Value::as_str) {
                    self.container = Some(c.to_string());
                }
                if let Some(r) = v.pointer("/delta/stop_reason").and_then(Value::as_str) {
                    self.stop_reason = Some(r.to_string());
                }
            }
            _ => {}
        }
        let mut out = Vec::new();
        for ev in self.inner.event(name, v)? {
            match ev {
                // The request-id header is the upstream id; keep the served model.
                StreamEvent::Meta { model, .. } => {
                    out.push(StreamEvent::Meta { upstream_id: None, provider: None, model })
                }
                StreamEvent::Reasoning { kind, text, block } => out.push(StreamEvent::Reasoning {
                    kind: if kind == ReasoningKind::Text { ReasoningKind::Summary } else { kind },
                    text,
                    block: block + self.offset,
                }),
                StreamEvent::Tool(mut call) => {
                    if ty == "content_block_start" {
                        refine_tool(&mut call, &v["content_block"]);
                    }
                    out.push(StreamEvent::Tool(call));
                }
                StreamEvent::LiveUsage(u) => {
                    let mut live = combine(self.earlier_usage.as_ref(), &self.earlier_raw, u);
                    live.cost_usd = None;
                    live.cost_source = CostSource::Unknown;
                    out.push(StreamEvent::LiveUsage(live));
                }
                StreamEvent::Usage(u) => {
                    let u = self.priced(u, unbilled_refusal(v, self.blocks));
                    self.usage = Some(u.clone());
                    out.push(StreamEvent::Usage(combine(self.earlier_usage.as_ref(), &self.earlier_raw, u)));
                }
                // The turn goes on in another request (the caller decides).
                StreamEvent::Finish { native: Some(n), .. } if n == "pause_turn" => {}
                other => out.push(other),
            }
        }
        Ok(out)
    }

    /// This request's usage with its estimated cost.
    fn priced(&self, mut u: Usage, free: bool) -> Usage {
        if u.cost_usd.is_some() {
            return u; // reported by the API (it does not report costs today)
        }
        u.cost_usd = if free { Some(0.0) } else { self.spec.and_then(|s| estimate_cost(s, &u)) };
        u.cost_source = if u.cost_usd.is_some() { CostSource::Estimated } else { CostSource::Unknown };
        u
    }
}

impl crate::chatstream::EventParser for Parser {
    fn event(&mut self, name: Option<&str>, data: &Value) -> Result<Vec<StreamEvent>, LlmError> {
        Parser::event(self, name, data)
    }
    fn finished(&self) -> bool {
        self.inner.finished
    }
    fn any_output(&self) -> bool {
        self.earlier_output || crate::chatstream::EventParser::any_output(&self.inner)
    }
}

/// A refusal before any output, in a category Anthropic does not bill
/// (docs: refusals-and-fallback, "How refusals are billed").
fn unbilled_refusal(v: &Value, blocks: usize) -> bool {
    v.pointer("/delta/stop_reason").and_then(Value::as_str) == Some("refusal")
        && blocks == 0
        && v.pointer("/usage/output_tokens").and_then(Value::as_u64).unwrap_or(0) == 0
        && !v
            .pointer("/delta/stop_details/category")
            .and_then(Value::as_str)
            .is_some_and(|c| BILLED_REFUSALS.contains(&c))
}

/// `cur` added to the usage of the turn's earlier requests (their usage
/// objects and this one's, in order, as `raw`).
fn combine(earlier: Option<&Usage>, earlier_raw: &[Value], cur: Usage) -> Usage {
    let Some(e) = earlier else { return cur };
    let s = |a: Option<u64>, b: Option<u64>| match (a, b) {
        (None, None) => None,
        (a, b) => Some(a.unwrap_or(0) + b.unwrap_or(0)),
    };
    let cost = match (e.cost_usd, cur.cost_usd) {
        (Some(a), Some(b)) => Some(a + b),
        _ => None,
    };
    let mut raw = earlier_raw.to_vec();
    raw.push(cur.raw);
    Usage {
        input_tokens: s(e.input_tokens, cur.input_tokens),
        output_tokens: s(e.output_tokens, cur.output_tokens),
        reasoning_tokens: s(e.reasoning_tokens, cur.reasoning_tokens),
        cached_input_tokens: s(e.cached_input_tokens, cur.cached_input_tokens),
        cache_write_tokens: s(e.cache_write_tokens, cur.cache_write_tokens),
        cost_usd: cost,
        cost_source: if cost.is_some() { cur.cost_source } else { CostSource::Unknown },
        raw: Value::Array(raw),
    }
}

/// Fill in what the shared parser leaves out of a tool event at block start:
/// the input of a `server_tool_use` that arrives whole, and a readable result.
fn refine_tool(call: &mut ToolCall, block: &Value) {
    let ty = block.get("type").and_then(Value::as_str).unwrap_or("");
    if ty == "server_tool_use" {
        if let Some(input) = block.get("input").filter(|i| i.as_object().is_some_and(|o| !o.is_empty())) {
            call.input = Some(input.clone());
        }
        return;
    }
    if !ty.ends_with("_tool_result") {
        return;
    }
    let content = block.get("content").unwrap_or(&Value::Null);
    let failed = content.get("type").and_then(Value::as_str).is_some_and(|t| t.ends_with("_error"));
    call.status = Some(if failed { "failed" } else { "completed" }.into());
    call.output = Some(result_summary(content));
}

/// What a tool result shows: search hits (title, url, page age), the fetched
/// page (url, title, type, time), stdout/stderr/return code and output files,
/// file views and edits, or the error code. Encrypted content (only the API
/// can read it) and fetched document bodies are left out.
fn result_summary(content: &Value) -> Value {
    if content.get("type").and_then(Value::as_str) == Some("web_fetch_result") {
        let doc = &content["content"];
        let mut o = Map::new();
        for (k, v) in [
            ("url", content.get("url")),
            ("title", doc.get("title")),
            ("media_type", doc.pointer("/source/media_type")),
            ("retrieved_at", content.get("retrieved_at")),
        ] {
            if let Some(v) = v.filter(|v| !v.is_null()) {
                o.insert(k.into(), v.clone());
            }
        }
        return Value::Object(o);
    }
    without_opaque(content)
}

fn without_opaque(v: &Value) -> Value {
    match v {
        Value::Object(o) => Value::Object(
            o.iter()
                .filter(|(k, _)| !k.starts_with("encrypted_"))
                .map(|(k, v)| (k.clone(), without_opaque(v)))
                .collect(),
        ),
        Value::Array(a) => Value::Array(a.iter().map(without_opaque).collect()),
        other => other.clone(),
    }
}

/// Classify Anthropic's error objects (`{"type", "message", "details"?}`; the
/// whole body is kept as details by the caller). Retried: rate limits (after
/// `retry-after`, at least 20 s without it), overload, internal errors and
/// timeouts. Never retried: invalid requests (spend limits you set included),
/// authentication, billing, permission, unknown resources, oversized
/// requests, and the monthly spend-cap 429 (`enforced_spend_limit_reached`,
/// which fails until the next month). Unknown types: by HTTP status, else
/// (in-band) retried with backoff.
pub fn error_from_json(err: &Value, http_status: Option<u16>, retry_after: Option<u64>, kind: ErrorKind) -> LlmError {
    let ty = err.get("type").and_then(Value::as_str).unwrap_or("");
    let message = err.get("message").and_then(Value::as_str).unwrap_or("(no message)").to_string();
    let code = err.pointer("/details/error_code").and_then(Value::as_str);
    let after_or_backoff = || retry_after.map_or(Retry::Backoff, Retry::After);
    let retry = match ty {
        "rate_limit_error" if code == Some("enforced_spend_limit_reached") => Retry::Never,
        "rate_limit_error" => Retry::After(retry_after.unwrap_or(20)),
        "overloaded_error" | "api_error" | "timeout_error" | "conflict_error" => after_or_backoff(),
        "invalid_request_error"
        | "authentication_error"
        | "billing_error"
        | "permission_error"
        | "not_found_error"
        | "request_too_large" => Retry::Never,
        _ => match http_status {
            Some(s) => retry_for_status(s, retry_after),
            None => Retry::Backoff,
        },
    };
    let error_type = if message.contains("prompt is too long") {
        // The prompt alone does not fit the context window.
        Some("context_length_exceeded".to_string())
    } else if is_max_tokens_over_limit(&message) {
        // `max_tokens: 130000 > 128000, which is the maximum allowed number of output tokens for …`
        Some("max_tokens_exceeded".to_string())
    } else {
        code.or(Some(ty)).filter(|t| !t.is_empty()).map(str::to_string)
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

/// `max_tokens: N > M…`: the requested output budget is above the model's limit.
fn is_max_tokens_over_limit(message: &str) -> bool {
    let Some(rest) = message.strip_prefix("max_tokens: ") else { return false };
    let (n, rest) = rest.split_once(' ').unwrap_or((rest, ""));
    n.bytes().all(|b| b.is_ascii_digit()) && !n.is_empty() && rest.starts_with('>')
}
