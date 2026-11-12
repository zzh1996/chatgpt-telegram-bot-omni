//! Parser of `streamGenerateContent?alt=sse`: every SSE `data:` is one
//! `GenerateContentResponse` chunk (docs/research/gemini.md §7; the stream
//! has no `[DONE]`, it ends after the chunk with `finishReason`).
//!
//! - Parts arrive as deltas and are kept **exactly as received** (one stored
//!   part per streamed part: thought signatures stay on the part that carried
//!   them, nothing merged or split) for replay; display events come from them
//!   in order: thought summaries (`thought: true`) → `Reasoning{Summary}`,
//!   text → `Text`, `inlineData` images (also "thought images" of the image
//!   models) → `Image`, `executableCode` / `codeExecutionResult` → one
//!   `code_execution` tool call.
//! - Grounding: `webSearchQueries` → a `google_search` (or `google_maps`)
//!   tool call as soon as they arrive, with the Search Suggestions of
//!   `searchEntryPoint.sdkBlob`; `groundingChunks` (web, maps with their review
//!   snippets, image, retrieved context) → citations when the candidate
//!   finishes, each with the byte range of the first text segment it supports
//!   (`groundingSupports`: UTF-8 byte offsets in the part at `partIndex`,
//!   chunk indices across the whole stream). `urlContextMetadata` → a
//!   `url_context` tool call; `citationMetadata` (recitation) → citations.
//! - Long decode (Gemini 4): a chunk with `continuationToken` and no
//!   `finishReason` is a checkpoint (after 32K tokens, every ~5K): the usage so
//!   far is billed (`Usage`: a stream dropped after a checkpoint is paid up to
//!   it) and a `Checkpoint` carries the token and the parts committed so far.
//!   The final `continuationToken` (with `CONTINUATION` or `MAX_TOKENS`) is
//!   returned in `CallOutcome.continuation`.
//! - Usage: `usageMetadata` is cumulative within a stream: `LiveUsage` while
//!   streaming (display), `Usage` at checkpoints and at the end, with the
//!   estimated cost.
//! - Blocks: `promptFeedback.blockReason` and abnormal `finishReason`s become
//!   `Finish{content_filter|error}` with the provider's account in `FinishDetails`.

use base64::Engine as _;
use omni_core::Usage;
use omni_core::content::{Citation, ReasoningKind, ToolCall};
use omni_core::usage::CostSource;
use serde_json::{Value, json};

use super::errors::error_from_json;
use super::pricing::{self, GroundingUse, Tier};
use crate::error::{ErrorKind, LlmError};
use crate::event::StreamEvent;

/// Kind of a part, for `partIndex` mapping (consecutive text deltas of one kind
/// form one part of the response).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Thought,
    Text,
    Other,
}

#[derive(Debug, Clone, Copy)]
struct Logical {
    kind: Kind,
    /// Byte offset of the part in this stream's answer text (for text parts).
    start: usize,
}

/// `finishReason` → (normalized reason, abnormal: the provider's details are kept).
pub fn finish_reason(native: &str) -> (&'static str, bool) {
    match native {
        "STOP" => ("stop", false),
        "MAX_TOKENS" | "CONTINUATION" => ("length", false),
        "SAFETY"
        | "RECITATION"
        | "LANGUAGE"
        | "BLOCKLIST"
        | "PROHIBITED_CONTENT"
        | "SPII"
        | "IMAGE_SAFETY"
        | "IMAGE_PROHIBITED_CONTENT"
        | "IMAGE_RECITATION"
        | "ESCALATION"
        | "PUP_LIMITED_DISABLED" => ("content_filter", true),
        // OTHER, MALFORMED_FUNCTION_CALL, UNEXPECTED_TOOL_CALL, TOO_MANY_TOOL_CALLS,
        // MISSING_THOUGHT_SIGNATURE, MALFORMED_RESPONSE, IMAGE_OTHER, NO_IMAGE, and
        // anything new: a failure, shown with the provider's details.
        _ => ("error", true),
    }
}

/// Normalized usage from `usageMetadata`: input = prompt (cached included) +
/// tool-use prompt; output = candidates + thoughts; reasoning = thoughts.
pub fn usage_from_gemini(u: &Value) -> Usage {
    let g = |k: &str| u.get(k).and_then(Value::as_u64);
    let sum = |a: Option<u64>, b: Option<u64>| match (a, b) {
        (None, None) => None,
        (a, b) => Some(a.unwrap_or(0) + b.unwrap_or(0)),
    };
    Usage {
        input_tokens: sum(g("promptTokenCount"), g("toolUsePromptTokenCount")),
        output_tokens: sum(g("candidatesTokenCount"), g("thoughtsTokenCount")),
        reasoning_tokens: g("thoughtsTokenCount"),
        cached_input_tokens: g("cachedContentTokenCount"),
        cache_write_tokens: None,
        cost_usd: None,
        cost_source: CostSource::Unknown,
        raw: u.clone(),
    }
}

fn get<'a>(v: &'a Value, camel: &str, snake: &str) -> Option<&'a Value> {
    v.get(camel).or_else(|| v.get(snake))
}

fn s(v: Option<&Value>) -> Option<String> {
    v.and_then(Value::as_str).map(str::to_string)
}

/// Streaming state of one `streamGenerateContent` call (one long-decode hop).
#[derive(Debug)]
pub struct ChunkParser {
    model: String,
    tier: Option<Tier>,
    now: i64,
    search_on: bool,
    maps_on: bool,
    /// The model's parts, as received (parts carried over from a dropped hop first).
    parts: Vec<Value>,
    response_id: Option<String>,
    meta_sent: bool,
    in_reasoning: bool,
    emitted: bool,
    block: usize,
    /// Answer (non-thought) text of this stream.
    answer: String,
    logical: Vec<Logical>,
    last_kind: Option<Kind>,
    pending_code: Vec<String>,
    seq: usize,
    chunks: Vec<Value>,
    supports: Vec<Value>,
    queries: Vec<String>,
    image_queries: Vec<String>,
    suggestions: Option<Value>,
    search_call: Option<(String, String)>,
    cited: usize,
    url_meta: Vec<Value>,
    recitations: Vec<Value>,
    recited: usize,
    usage: Option<Value>,
    pub finished: bool,
    continuation: Option<Value>,
    any_output: bool,
}

impl ChunkParser {
    /// `body`: the request (tier and tools, for pricing and tool names);
    /// `now`: Unix time of the call (dated prices); `carried`: parts committed
    /// before the checkpoint this request resumes from.
    pub fn new(model: &str, body: &Value, now: i64, carried: Vec<Value>) -> Self {
        let tools = body.get("tools").and_then(Value::as_array);
        let has = |k: &str| tools.is_some_and(|t| t.iter().any(|x| x.get(k).is_some()));
        ChunkParser {
            model: model.to_string(),
            tier: body.get("service_tier").and_then(Value::as_str).and_then(Tier::parse),
            now,
            search_on: has("googleSearch"),
            maps_on: has("googleMaps"),
            parts: carried,
            response_id: None,
            meta_sent: false,
            in_reasoning: false,
            emitted: false,
            block: 0,
            answer: String::new(),
            logical: Vec::new(),
            last_kind: None,
            pending_code: Vec::new(),
            seq: 0,
            chunks: Vec::new(),
            supports: Vec::new(),
            queries: Vec::new(),
            image_queries: Vec::new(),
            suggestions: None,
            search_call: None,
            cited: 0,
            url_meta: Vec::new(),
            recitations: Vec::new(),
            recited: 0,
            usage: None,
            finished: false,
            continuation: None,
            any_output: false,
        }
    }

    /// The model turn for replay: all parts of this hop, exactly as received.
    pub fn native_payload(&self) -> Value {
        json!({"role": "model", "parts": self.parts})
    }

    /// The latest continuation item (checkpoint, or the final token).
    pub fn continuation(&self) -> Option<&Value> {
        self.continuation.as_ref()
    }

    pub fn response_id(&self) -> Option<&str> {
        self.response_id.as_deref()
    }

    fn new_id(&mut self, what: &str) -> String {
        self.seq += 1;
        format!("{}#{what}-{}", self.response_id.as_deref().unwrap_or("gemini"), self.seq)
    }

    /// A part of `kind` begins or continues (thought and text deltas continue a
    /// part of the same kind; anything else is a part of its own).
    fn kind(&mut self, kind: Kind) {
        if kind == Kind::Other || self.last_kind != Some(kind) {
            self.logical.push(Logical { kind, start: self.answer.len() });
        }
        self.last_kind = Some(kind);
    }

    fn shown(&mut self, reasoning: bool) {
        if reasoning && !self.in_reasoning && self.emitted {
            self.block += 1;
        }
        self.in_reasoning = reasoning;
        self.emitted = true;
        self.any_output = true;
    }

    fn priced(&self, u: &Value) -> Usage {
        let mut usage = usage_from_gemini(u);
        usage.cost_usd = pricing::estimate(&self.model, u, self.tier, self.now, &self.grounding_use());
        usage.cost_source = if usage.cost_usd.is_some() { CostSource::Estimated } else { CostSource::Unknown };
        usage
    }

    fn grounding_use(&self) -> GroundingUse {
        let mut q: Vec<&str> = self.queries.iter().chain(&self.image_queries).map(|q| q.trim()).collect();
        q.retain(|q| !q.is_empty());
        q.sort_unstable();
        q.dedup();
        GroundingUse {
            queries: q.len(),
            web_results: self.chunks.iter().any(|c| c.get("web").is_some() || c.get("image").is_some()),
            maps_results: self.chunks.iter().any(|c| c.get("maps").is_some()),
        }
    }

    /// One `data:` chunk.
    pub fn chunk(&mut self, data: &Value) -> Result<Vec<StreamEvent>, LlmError> {
        let mut evs = Vec::new();
        if let Some(err) = data.get("error") {
            let mut e = error_from_json(err, None, None, ErrorKind::InBand);
            e.details = Some(data.clone());
            e.partial_output = self.any_output;
            return Err(e);
        }
        if !self.meta_sent {
            let id = s(get(data, "responseId", "response_id"));
            let model = s(get(data, "modelVersion", "model_version"));
            if id.is_some() || model.is_some() {
                self.meta_sent = true;
                self.response_id = id.clone();
                evs.push(StreamEvent::Meta { upstream_id: id, provider: None, model });
            }
        }
        if let Some(pf) = get(data, "promptFeedback", "prompt_feedback")
            && let Some(reason) = get(pf, "blockReason", "block_reason").and_then(Value::as_str)
        {
            evs.push(StreamEvent::FinishDetails(pf.clone()));
            evs.push(StreamEvent::Finish { reason: Some("content_filter".into()), native: Some(reason.into()) });
            self.finished = true;
        }
        let mut checkpoint: Option<String> = None;
        let mut finishing = false;
        if let Some(cand) = data.get("candidates").and_then(Value::as_array).and_then(|c| c.first()) {
            if let Some(parts) = cand.pointer("/content/parts").and_then(Value::as_array) {
                for p in parts {
                    self.part(p, &mut evs);
                }
            }
            if let Some(g) = get(cand, "groundingMetadata", "grounding_metadata") {
                self.grounding(g, &mut evs);
            }
            if let Some(u) = get(cand, "urlContextMetadata", "url_context_metadata") {
                self.url_context(u, &mut evs);
            }
            if let Some(c) = get(cand, "citationMetadata", "citation_metadata")
                .and_then(|c| get(c, "citationSources", "citation_sources"))
                .and_then(Value::as_array)
            {
                self.recitations.extend(c.iter().cloned());
            }
            let token = s(get(cand, "continuationToken", "continuation_token")).filter(|t| !t.is_empty());
            let fr = s(get(cand, "finishReason", "finish_reason"))
                .filter(|f| !f.is_empty() && f != "FINISH_REASON_UNSPECIFIED");
            match fr {
                Some(fr) => {
                    finishing = true;
                    // Only the final token resumes after this hop (none: nothing to continue).
                    self.continuation = token.map(|t| json!({ "continuationToken": t }));
                    self.citations(&mut evs);
                    let (reason, abnormal) = finish_reason(&fr);
                    if abnormal {
                        let mut d = cand.clone();
                        if let Some(o) = d.as_object_mut() {
                            o.remove("content");
                        }
                        evs.push(StreamEvent::FinishDetails(d));
                    }
                    evs.push(StreamEvent::Finish { reason: Some(reason.into()), native: Some(fr) });
                    self.finished = true;
                }
                None if !self.finished => checkpoint = token,
                // Metadata after the finishing chunk.
                None => self.citations(&mut evs),
            }
        }
        let chunk_usage = get(data, "usageMetadata", "usage_metadata").filter(|u| u.is_object());
        if let Some(u) = chunk_usage {
            self.usage = Some(u.clone());
        }
        if let Some(u) = &self.usage {
            if finishing || checkpoint.is_some() || (self.finished && chunk_usage.is_some()) {
                evs.push(StreamEvent::Usage(self.priced(u)));
            } else if chunk_usage.is_some() {
                evs.push(StreamEvent::LiveUsage(self.priced(u)));
            }
        }
        if let Some(t) = checkpoint {
            let item = json!({"continuationToken": t, "parts": self.parts});
            self.continuation = Some(item.clone());
            evs.push(StreamEvent::Checkpoint(item));
        }
        Ok(evs)
    }

    fn part(&mut self, p: &Value, evs: &mut Vec<StreamEvent>) {
        self.parts.push(p.clone());
        let thought = p.get("thought").and_then(Value::as_bool) == Some(true);
        if let Some(text) = p.get("text").and_then(Value::as_str) {
            if text.is_empty() {
                // e.g. the empty part carrying the final thought signature
                return;
            }
            if thought {
                self.kind(Kind::Thought);
                self.shown(true);
                evs.push(StreamEvent::Reasoning { kind: ReasoningKind::Summary, text: text.into(), block: self.block });
            } else {
                self.kind(Kind::Text);
                self.answer.push_str(text);
                self.shown(false);
                evs.push(StreamEvent::Text(text.into()));
            }
            return;
        }
        self.kind(Kind::Other);
        if let Some(d) = get(p, "inlineData", "inline_data") {
            let mime = s(get(d, "mimeType", "mime_type")).unwrap_or_else(|| "application/octet-stream".into());
            let data = d.get("data").and_then(Value::as_str).unwrap_or("");
            match base64::engine::general_purpose::STANDARD.decode(data.trim()) {
                Ok(bytes) if mime.starts_with("image/") => {
                    self.shown(false);
                    evs.push(StreamEvent::Image { data: bytes, mime });
                }
                Ok(bytes) => evs.push(StreamEvent::Notice(format!(
                    "the model returned a {mime} file ({} bytes): kept in the conversation, not displayed",
                    bytes.len()
                ))),
                Err(_) => evs.push(StreamEvent::Notice(format!("the model returned {mime} data that is not base64"))),
            }
        } else if let Some(c) = get(p, "executableCode", "executable_code") {
            let id = match s(c.get("id")) {
                Some(id) => id,
                None => self.new_id("code"),
            };
            self.pending_code.push(id.clone());
            self.shown(false);
            evs.push(StreamEvent::Tool(ToolCall {
                id: Some(id),
                name: "code_execution".into(),
                status: Some("in_progress".into()),
                input: Some(c.clone()),
                output: None,
            }));
        } else if let Some(r) = get(p, "codeExecutionResult", "code_execution_result") {
            let id = match s(r.get("id")) {
                Some(id) => {
                    self.pending_code.retain(|x| *x != id);
                    id
                }
                None if !self.pending_code.is_empty() => self.pending_code.remove(0),
                None => self.new_id("code"),
            };
            let status = match r.get("outcome").and_then(Value::as_str).unwrap_or("") {
                "OUTCOME_OK" => "completed".to_string(),
                "OUTCOME_FAILED" => "failed".to_string(),
                "OUTCOME_DEADLINE_EXCEEDED" => "deadline_exceeded".to_string(),
                other => other.to_ascii_lowercase(),
            };
            self.shown(false);
            evs.push(StreamEvent::Tool(ToolCall {
                id: Some(id),
                name: "code_execution".into(),
                status: Some(status),
                input: None,
                output: Some(r.clone()),
            }));
        } else if let Some((key, done)) =
            [("toolCall", false), ("toolResponse", true)].into_iter().find(|(k, _)| p.get(*k).is_some())
        {
            // Server-side tool trace (only returned when asked for, or for agentic video).
            let t = &p[key];
            let name = s(t.get("toolName"))
                .or_else(|| s(t.get("toolType")).map(|x| x.to_ascii_lowercase()))
                .unwrap_or_else(|| "tool".into());
            let id = match s(t.get("id")) {
                Some(id) => id,
                None => self.new_id("tool"),
            };
            self.shown(false);
            evs.push(StreamEvent::Tool(ToolCall {
                id: Some(id),
                name,
                status: Some(if done { "completed" } else { "in_progress" }.into()),
                input: if done { None } else { t.get("args").cloned() },
                output: if done { t.get("response").cloned() } else { None },
            }));
        } else if let Some(f) = get(p, "functionCall", "function_call") {
            // No function declarations are sent; shown if the model emits one anyway.
            self.shown(false);
            evs.push(StreamEvent::Tool(ToolCall {
                id: s(f.get("id")),
                name: s(f.get("name")).unwrap_or_else(|| "function".into()),
                status: Some("requested".into()),
                input: f.get("args").cloned(),
                output: None,
            }));
        } else if p.as_object().is_some_and(|o| o.keys().all(|k| k == "thoughtSignature" || k == "thought")) {
            // A signature-only part: kept for replay, nothing to show.
        } else {
            let keys: Vec<&str> = p.as_object().map(|o| o.keys().map(String::as_str).collect()).unwrap_or_default();
            evs.push(StreamEvent::Notice(format!(
                "the response contained a part the bot does not display ({}); it is kept in the conversation",
                keys.join(", ")
            )));
        }
    }

    fn grounding(&mut self, g: &Value, evs: &mut Vec<StreamEvent>) {
        let mut changed = false;
        for (camel, snake, image) in
            [("webSearchQueries", "web_search_queries", false), ("imageSearchQueries", "image_search_queries", true)]
        {
            for q in get(g, camel, snake).and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str) {
                let list = if image { &mut self.image_queries } else { &mut self.queries };
                if !list.iter().any(|x| x == q) {
                    list.push(q.to_string());
                    changed = true;
                }
            }
        }
        if let Some(c) = get(g, "groundingChunks", "grounding_chunks").and_then(Value::as_array) {
            self.chunks.extend(c.iter().cloned());
        }
        if let Some(sp) = get(g, "groundingSupports", "grounding_supports").and_then(Value::as_array) {
            self.supports.extend(sp.iter().cloned());
        }
        // Search Suggestions without HTML: `sdkBlob` is "base64 encoded JSON representing array of tuple".
        if let Some(blob) = get(g, "searchEntryPoint", "search_entry_point")
            .and_then(|e| get(e, "sdkBlob", "sdk_blob"))
            .and_then(Value::as_str)
            && let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(blob.trim())
            && let Ok(v) = serde_json::from_slice::<Value>(&bytes)
            && self.suggestions.as_ref() != Some(&v)
        {
            self.suggestions = Some(v);
            changed = true;
        }
        if !changed || (self.queries.is_empty() && self.image_queries.is_empty()) {
            return;
        }
        let (id, name) = match &self.search_call {
            Some(c) => c.clone(),
            None => {
                let maps_only = self.chunks.iter().any(|c| c.get("maps").is_some())
                    && !self.chunks.iter().any(|c| c.get("web").is_some());
                let name = if self.maps_on && (!self.search_on || maps_only) { "google_maps" } else { "google_search" };
                let call = (format!("{}#{name}", self.response_id.as_deref().unwrap_or("gemini")), name.to_string());
                self.search_call = Some(call.clone());
                call
            }
        };
        let mut input = json!({ "queries": self.queries });
        if !self.image_queries.is_empty() {
            input["image_queries"] = json!(self.image_queries);
        }
        self.shown(false);
        evs.push(StreamEvent::Tool(ToolCall {
            id: Some(id),
            name,
            status: Some("completed".into()),
            input: Some(input),
            output: self.suggestions.as_ref().map(|s| json!({ "search_suggestions": s })),
        }));
    }

    fn url_context(&mut self, u: &Value, evs: &mut Vec<StreamEvent>) {
        let Some(list) = get(u, "urlMetadata", "url_metadata").and_then(Value::as_array) else { return };
        let mut changed = false;
        for m in list {
            if !self.url_meta.contains(m) {
                self.url_meta.push(m.clone());
                changed = true;
            }
        }
        if !changed {
            return;
        }
        let urls: Vec<Value> =
            self.url_meta.iter().filter_map(|m| get(m, "retrievedUrl", "retrieved_url").cloned()).collect();
        let ok = self.url_meta.iter().any(|m| {
            get(m, "urlRetrievalStatus", "url_retrieval_status").and_then(Value::as_str)
                == Some("URL_RETRIEVAL_STATUS_SUCCESS")
        });
        self.shown(false);
        evs.push(StreamEvent::Tool(ToolCall {
            id: Some(format!("{}#url_context", self.response_id.as_deref().unwrap_or("gemini"))),
            name: "url_context".into(),
            status: Some(if ok { "completed" } else { "failed" }.into()),
            input: Some(json!({ "urls": urls })),
            output: Some(json!({ "url_metadata": self.url_meta })),
        }));
    }

    /// Byte range in this stream's answer text of a support segment.
    fn resolve(&self, seg: &Value) -> Option<(usize, usize)> {
        let idx = |camel: &str, snake: &str| get(seg, camel, snake).and_then(Value::as_u64).map(|n| n as usize);
        // proto3 JSON leaves out zero values.
        let start = idx("startIndex", "start_index").unwrap_or(0);
        let end = idx("endIndex", "end_index")?;
        let part = idx("partIndex", "part_index").unwrap_or(0);
        let text = seg.get("text").and_then(Value::as_str);
        let a = &self.answer;
        let fits = |x: usize, y: usize| {
            x <= y
                && y <= a.len()
                && a.is_char_boundary(x)
                && a.is_char_boundary(y)
                && text.is_none_or(|t| &a[x..y] == t)
        };
        if let Some(l) = self.logical.get(part).filter(|l| l.kind == Kind::Text)
            && fits(l.start + start, l.start + end)
        {
            return Some((l.start + start, l.start + end));
        }
        if fits(start, end) {
            return Some((start, end));
        }
        let t = text.filter(|t| !t.is_empty())?;
        a.find(t).map(|x| (x, x + t.len()))
    }

    /// Citations for the grounding chunks (and recitation sources) not cited yet.
    fn citations(&mut self, evs: &mut Vec<StreamEvent>) {
        let mut out = Vec::new();
        for i in self.cited..self.chunks.len() {
            let chunk = &self.chunks[i];
            let supports: Vec<&Value> = self
                .supports
                .iter()
                .filter(|s| {
                    get(s, "groundingChunkIndices", "grounding_chunk_indices")
                        .and_then(Value::as_array)
                        .is_some_and(|l| l.iter().any(|x| x.as_u64() == Some(i as u64)))
                })
                .collect();
            let range = supports.iter().find_map(|s| s.get("segment").and_then(|seg| self.resolve(seg)));
            let extra = Some(json!({"groundingChunk": chunk, "supports": supports}));
            let field = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
            if let Some(w) = chunk.get("web") {
                out.push(Citation { url: field(w, "uri"), title: field(w, "title"), cited_text: None, range, extra });
            } else if let Some(m) = chunk.get("maps") {
                // Google Maps sources: attributed to "Google Maps", linked with `uri`/`googleMapsUri`.
                let attributed = |t: Option<String>| {
                    Some(match t.filter(|t| !t.is_empty()) {
                        Some(t) => format!("{t} · Google Maps"),
                        None => "Google Maps".to_string(),
                    })
                };
                out.push(Citation {
                    url: field(m, "uri").or_else(|| field(m, "googleMapsUri")),
                    title: attributed(field(m, "title")),
                    cited_text: field(m, "text"),
                    range,
                    extra,
                });
                for r in m.pointer("/placeAnswerSources/reviewSnippets").and_then(Value::as_array).into_iter().flatten()
                {
                    out.push(Citation {
                        url: field(r, "googleMapsUri").or_else(|| field(r, "uri")),
                        title: attributed(field(r, "title")),
                        cited_text: None,
                        range: None,
                        extra: Some(json!({ "reviewSnippet": r })),
                    });
                }
            } else if let Some(img) = chunk.get("image") {
                // Image search: link to the page containing the image.
                out.push(Citation {
                    url: field(img, "sourceUri"),
                    title: field(img, "title").or_else(|| field(img, "domain")),
                    cited_text: None,
                    range,
                    extra,
                });
            } else if let Some(rc) = chunk.get("retrievedContext") {
                out.push(Citation {
                    url: field(rc, "uri"),
                    title: field(rc, "title"),
                    cited_text: field(rc, "text"),
                    range,
                    extra,
                });
            } else {
                out.push(Citation { url: None, title: None, cited_text: None, range, extra });
            }
        }
        self.cited = self.chunks.len();
        for c in &self.recitations[self.recited..] {
            let uri = c.get("uri").and_then(Value::as_str).map(str::to_string);
            let start = c.get("startIndex").and_then(Value::as_u64).unwrap_or(0) as usize;
            let range = c.get("endIndex").and_then(Value::as_u64).map(|e| (start, e as usize)).filter(|(x, y)| {
                x <= y
                    && *y <= self.answer.len()
                    && self.answer.is_char_boundary(*x)
                    && self.answer.is_char_boundary(*y)
            });
            let title = c
                .get("license")
                .and_then(Value::as_str)
                .filter(|l| !l.is_empty())
                .map(|l| format!("{} (license: {l})", uri.as_deref().unwrap_or("source")));
            out.push(Citation {
                url: uri,
                title,
                cited_text: None,
                range,
                extra: Some(json!({ "citationSource": c })),
            });
        }
        self.recited = self.recitations.len();
        if !out.is_empty() {
            evs.push(StreamEvent::Citations(out));
        }
    }
}

impl crate::chatstream::EventParser for ChunkParser {
    fn event(&mut self, _name: Option<&str>, data: &Value) -> Result<Vec<StreamEvent>, LlmError> {
        self.chunk(data)
    }
    fn finished(&self) -> bool {
        self.finished
    }
    fn any_output(&self) -> bool {
        self.any_output
    }
}
