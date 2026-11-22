//! The Messages stream of the direct API: Anthropic's events, parsed by the
//! shared Messages parser ([`crate::openrouter::messages::EventParser`]) and
//! adjusted here:
//!
//! - in-band `error` events are classified with Anthropic's error types;
//! - thinking text is a summary (the API never returns the raw chain of
//!   thought): reasoning events are `ReasoningKind::Summary`;
//! - tool results are their API objects without the opaque encrypted content
//!   (search hits, the fetched page without its body, stdout/stderr/return
//!   code/`abort_reason` and output `file_id`s, file views and edits, error
//!   codes), and failed results are marked `failed`; a call made by code
//!   execution (programmatic tool calling, dynamic filtering: `caller.type`
//!   other than `direct`) has its `caller` in the tool call's input;
//! - citations of every kind: web search results (url, title, cited text),
//!   and locations in documents (`char_location`, `page_location`,
//!   `content_block_location`; user documents and pages fetched by
//!   `web_fetch`) with the document's title, the fetched page's URL when the
//!   document is one, the cited text, and the location in `extra`;
//! - the upstream id is the `request-id` header (support uses it), not the
//!   message id, which is kept in the native payload (`id`) for the next
//!   turn's cache diagnostics; a cache miss the diagnostics attribute to a
//!   change (`*_changed`) is a notice, the rest stays in the response (`/info`);
//! - a refusal's `stop_details` (category, explanation) make the call's
//!   failure ([`Parser::refusal`]);
//! - usage is priced from the list (per request), and a refusal before any
//!   output in a category Anthropic does not bill is free;
//! - `pause_turn` (the server-tool loop paused) is not a finish: the caller
//!   continues the turn with another request, which this parser follows too
//!   (content, usage and cost of all requests together).

use omni_core::Usage;
use omni_core::content::{Citation, ReasoningKind, ToolCall};
use omni_core::usage::CostSource;
use serde_json::{Value, json};

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
    /// `id` of the current request's message (`msg_…`).
    message_id: Option<String>,
    /// `stop_details` of a refusal stop.
    refusal: Option<Value>,
    /// The documents citations can point at, by `document_index`: the
    /// request's (user documents, pages fetched in earlier turns), then the
    /// pages fetched in this turn.
    docs: Vec<DocRef>,
    /// Callers of tool calls made by code execution, by call id.
    callers: Vec<(String, Value)>,
    /// The request had citations on for every document and for `web_fetch`
    /// (kept in the native payload: later turns keep the same setting).
    citations: Option<bool>,
}

/// A document of the request (or fetched in it), for citations.
#[derive(Debug, Clone, Default, PartialEq)]
struct DocRef {
    title: Option<String>,
    url: Option<String>,
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
            message_id: None,
            refusal: None,
            docs: Vec::new(),
            callers: Vec::new(),
            citations: None,
        }
    }

    /// A parser for the turn that `body` (the first request) starts: the
    /// documents its citations can point at, and its citation setting.
    pub fn with_request(spec: Option<&'static ModelSpec>, body: &Value) -> Self {
        let mut p = Self::new(spec);
        p.docs = request_documents(body);
        p.citations = Some(citations_on(body));
        p
    }

    /// The `id` of the current request's message.
    pub fn message_id(&self) -> Option<&str> {
        self.message_id.as_deref()
    }

    /// The failure of a turn the model refused (`stop_reason: refusal`): titled
    /// with the category (`refusal (bio)`), Anthropic's `stop_details` verbatim
    /// as details. Not retried.
    pub fn refusal(&self) -> Option<LlmError> {
        if self.stop_reason.as_deref() != Some("refusal") {
            return None;
        }
        let details = self.refusal.clone().unwrap_or_else(|| json!({"type": "refusal"}));
        let message = match details.get("category").and_then(Value::as_str).filter(|c| !c.is_empty()) {
            Some(c) => format!("refusal ({c})"),
            None => "refusal".to_string(),
        };
        let mut e = LlmError::new(ErrorKind::InBand, Retry::Never, message);
        e.error_type = Some("content_filter".into());
        e.details = Some(details);
        e.partial_output = crate::chatstream::EventParser::any_output(self);
        Some(e)
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
        self.refusal = None;
    }

    /// The assistant turn for replay: all content blocks, the request's `tools`
    /// (replay needs the same ones), the number of blocks of each request
    /// when the turn took more than one, the last message's `id` (cache
    /// diagnostics of the next turn) and the citation setting.
    pub fn native_payload(&self, tools: Option<&Value>) -> Value {
        let current = self.content();
        let mut hops: Vec<usize> = self.earlier.iter().map(Vec::len).collect();
        hops.push(current.len());
        let mut content: Vec<Value> = self.earlier.iter().flatten().cloned().collect();
        content.extend(current);
        let mut p = json!({"role": "assistant", "content": content});
        if let Some(id) = &self.message_id {
            p["id"] = json!(id);
        }
        if let Some(t) = tools {
            p["tools"] = t.clone();
        }
        if hops.len() > 1 {
            p["hops"] = json!(hops);
        }
        if let Some(c) = self.citations {
            p["citations"] = json!(c);
        }
        p
    }

    pub fn event(&mut self, name: Option<&str>, v: &Value) -> Result<Vec<StreamEvent>, LlmError> {
        let ty = v.get("type").and_then(Value::as_str).or(name).unwrap_or("");
        let mut out = Vec::new();
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
                if let Some(id) = v.pointer("/message/id").and_then(Value::as_str).filter(|i| !i.is_empty()) {
                    self.message_id = Some(id.to_string());
                }
                if let Some(n) = v.pointer("/message/diagnostics/cache_miss_reason").and_then(cache_miss_notice) {
                    out.push(StreamEvent::Notice(n));
                }
            }
            "content_block_start" => {
                self.blocks += 1;
                if let Some(i) = v.get("index").and_then(Value::as_u64) {
                    self.max_index = Some(self.max_index.map_or(i as usize, |m| m.max(i as usize)));
                }
                let cb = &v["content_block"];
                match cb.get("type").and_then(Value::as_str) {
                    Some("server_tool_use") => {
                        if let (Some(id), Some(caller)) =
                            (cb.get("id").and_then(Value::as_str), cb.get("caller").filter(|c| is_nested(c)))
                        {
                            self.callers.push((id.to_string(), caller.clone()));
                        }
                    }
                    // A fetched page is a document that later text can cite.
                    Some("web_fetch_tool_result") => {
                        if let Some(d) = fetched_document(cb) {
                            self.docs.push(d);
                        }
                    }
                    // Citations that come with the block instead of as deltas.
                    Some("text") => {
                        let cs: Vec<Citation> = cb
                            .get("citations")
                            .and_then(Value::as_array)
                            .into_iter()
                            .flatten()
                            .map(|c| self.citation(c))
                            .collect();
                        if !cs.is_empty() {
                            out.push(StreamEvent::Citations(cs));
                        }
                    }
                    _ => {}
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
                    if r == "refusal" {
                        self.refusal = v.pointer("/delta/stop_details").filter(|d| d.is_object()).cloned();
                    }
                }
            }
            _ => {}
        }
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
                    self.with_caller(&mut call);
                    out.push(StreamEvent::Tool(call));
                }
                // Every kind of citation (the shared parser knows web search results only).
                StreamEvent::Citations(_) => {
                    if let Some(c) = v.pointer("/delta/citation") {
                        out.push(StreamEvent::Citations(vec![self.citation(c)]));
                    }
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

    /// A call made by code execution: its `caller` goes into the call's input.
    fn with_caller(&self, call: &mut ToolCall) {
        let Some(id) = call.id.as_deref() else { return };
        let Some((_, caller)) = self.callers.iter().find(|(x, _)| x == id) else { return };
        if let Some(o) = call.input.as_mut().and_then(Value::as_object_mut) {
            o.insert("caller".into(), caller.clone());
        }
    }

    /// One of Anthropic's citations: a web search result (url, title, cited
    /// text), or a location in a document (its title, the page's URL for a
    /// fetched page, the cited text; the location and the rest in `extra`).
    fn citation(&self, c: &Value) -> Citation {
        let field = |k: &str| c.get(k).and_then(Value::as_str).filter(|x| !x.is_empty()).map(str::to_string);
        let cited_text = field("cited_text");
        if c.get("type").and_then(Value::as_str) == Some("web_search_result_location") {
            return Citation { url: field("url"), title: field("title"), cited_text, range: None, extra: None };
        }
        let doc = self.document(c);
        let source_url = field("source").filter(|s| s.starts_with("https://") || s.starts_with("http://"));
        let url = field("url").or(source_url).or_else(|| doc.and_then(|d| d.url.clone()));
        let title = field("document_title").or_else(|| field("title")).or_else(|| doc.and_then(|d| d.title.clone()));
        let mut extra = c.clone();
        if let Some(o) = extra.as_object_mut() {
            o.remove("cited_text");
        }
        Citation { url, title, cited_text, range: None, extra: Some(extra) }
    }

    /// The document a citation points at: the one at its `document_index`
    /// when the titles agree, else the only one with its title.
    fn document(&self, c: &Value) -> Option<&DocRef> {
        let title = c.get("document_title").and_then(Value::as_str).filter(|t| !t.is_empty());
        let agrees = |d: &&DocRef| match (title, d.title.as_deref()) {
            (Some(a), Some(b)) => a == b,
            _ => true,
        };
        let at = c.get("document_index").and_then(Value::as_u64).and_then(|i| self.docs.get(i as usize));
        if let Some(d) = at.filter(agrees) {
            return Some(d);
        }
        let title = title?;
        let mut same = self.docs.iter().filter(|d| d.title.as_deref() == Some(title));
        match (same.next(), same.next()) {
            (Some(d), None) => Some(d),
            _ => None,
        }
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
        unbilled: Vec::new(),
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

/// A tool result's content as the API sent it (search hits with their page
/// age, the fetch result with its URL, title, type and `retrieved_at`,
/// stdout/stderr/return code/`abort_reason` and output `file_id`s, file
/// views and edits, `error_code`s and `error_message`s), without what only
/// the model reads: encrypted content (`encrypted_*`), the fetched page's
/// body and the bytes of a viewed image or PDF (each replaced by its length).
fn result_summary(content: &Value) -> Value {
    let mut v = without_opaque(content);
    match v.get("type").and_then(Value::as_str) {
        Some("web_fetch_result") => elide(&mut v, "/content/source/data"),
        Some("text_editor_code_execution_view_result")
            if v.get("file_type").and_then(Value::as_str).is_some_and(|t| t != "text") =>
        {
            elide(&mut v, "/content")
        }
        _ => {}
    }
    v
}

/// The string at `ptr` replaced by a note of its length.
fn elide(v: &mut Value, ptr: &str) {
    if let Some(slot) = v.pointer_mut(ptr)
        && let Some(n) = slot.as_str().map(|s| s.chars().count())
    {
        *slot = json!(format!("({n} characters, sent back to the model in later turns; not shown)"));
    }
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

/// A `caller` of a call that code execution made (not `direct`).
fn is_nested(caller: &Value) -> bool {
    caller.get("type").and_then(Value::as_str).is_some_and(|t| t != "direct")
}

/// The page a `web_fetch_tool_result` holds, as a citable document.
fn fetched_document(block: &Value) -> Option<DocRef> {
    let c = block.get("content")?;
    if c.get("type").and_then(Value::as_str) != Some("web_fetch_result") {
        return None;
    }
    let s = |v: Option<&Value>| v.and_then(Value::as_str).filter(|x| !x.is_empty()).map(str::to_string);
    Some(DocRef { title: s(c.pointer("/content/title")), url: s(c.get("url")) })
}

/// The documents of a request in order (citations count "all document
/// content blocks in the request, spanning across all messages"): user
/// documents, and pages fetched in earlier turns (their `web_fetch` results
/// go back in the assistant messages).
fn request_documents(body: &Value) -> Vec<DocRef> {
    let mut out = Vec::new();
    for m in body.get("messages").and_then(Value::as_array).into_iter().flatten() {
        for b in m.get("content").and_then(Value::as_array).into_iter().flatten() {
            match b.get("type").and_then(Value::as_str) {
                Some("document") => {
                    let s = |v: Option<&Value>| v.and_then(Value::as_str).filter(|x| !x.is_empty()).map(str::to_string);
                    let url = (b.pointer("/source/type").and_then(Value::as_str) == Some("url"))
                        .then(|| s(b.pointer("/source/url")))
                        .flatten();
                    out.push(DocRef { title: s(b.get("title")), url });
                }
                Some("web_fetch_tool_result") => out.extend(fetched_document(b)),
                _ => {}
            }
        }
    }
    out
}

/// Every user document of the request and its `web_fetch` tool (if any)
/// have citations on: the setting the request was made with.
pub fn citations_on(body: &Value) -> bool {
    let enabled = |v: &Value| v.pointer("/citations/enabled").and_then(Value::as_bool) == Some(true);
    let docs = body
        .get("messages")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|m| m.get("role").and_then(Value::as_str) == Some("user"))
        .flat_map(|m| m.get("content").and_then(Value::as_array).into_iter().flatten())
        .filter(|b| b.get("type").and_then(Value::as_str) == Some("document"))
        .all(enabled);
    let fetch = body
        .get("tools")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|t| t.get("name").and_then(Value::as_str) == Some("web_fetch"))
        .all(enabled);
    docs && fetch
}

/// A notice for a cache miss that the cache diagnostics attribute to a change
/// in the request (`model_changed`, `system_changed`, `tools_changed`,
/// `messages_changed`); the other results (`previous_message_not_found`,
/// `unavailable`, a pending `null`) stay in the response for `/info`.
fn cache_miss_notice(reason: &Value) -> Option<String> {
    let ty = reason.get("type").and_then(Value::as_str)?;
    let what = match ty {
        "model_changed" => "the model or speed differs from the previous request's",
        "system_changed" => "the system prompt differs from the previous request's",
        "tools_changed" => "the tools differ from the previous request's",
        "messages_changed" => "an earlier message differs from the previous request's",
        _ => return None,
    };
    let tokens = reason
        .get("cache_missed_input_tokens")
        .and_then(Value::as_u64)
        .map(|n| format!("; about {} input tokens were not read from the cache", thousands(n)))
        .unwrap_or_default();
    Some(format!("Anthropic's cache diagnostics: {ty} ({what}){tokens}"))
}

/// `41850` → `41,850`.
fn thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
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
            None => crate::error::transient_or_never(&[Some(ty), Some(message.as_str())]),
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
