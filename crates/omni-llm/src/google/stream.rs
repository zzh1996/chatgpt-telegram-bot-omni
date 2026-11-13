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
//! - The built-in tools' own calls and results (`toolCall` / `toolResponse`
//!   parts, returned with `toolConfig.includeServerSideToolInvocations`) →
//!   tool calls where they ran: the call's `args` (queries, URLs) as input,
//!   the response (Search Suggestions, places, URL statuses) as output,
//!   paired by `id`, named by `toolType` (`google_search`,
//!   `google_search_image`, `google_maps`, `url_context`, `file_search`,
//!   `media_processing`). They replace the calls made up from the grounding
//!   metadata below; sources and suggestions still come from it.
//! - Grounding: `webSearchQueries` → a `google_search` (or `google_maps`)
//!   tool call as soon as they arrive (with `imageSearchQueries` too), with
//!   the Search Suggestions: the chips of `searchEntryPoint.renderedContent`
//!   (the only form Google sent in the 2026-11-12 traffic; `sdkBlob` is read
//!   when present), also listed with the sources (Google's terms require
//!   showing them with grounded answers); `groundingChunks` (web, maps with
//!   their review snippets, image search results linked to the page that has
//!   the image, retrieved context) → citations when the candidate finishes,
//!   each with the byte range of the first text segment it supports
//!   (`groundingSupports`: UTF-8 byte offsets in the part at `partIndex`,
//!   chunk indices across the whole stream). `urlContextMetadata` → a
//!   `url_context` tool call; `citationMetadata` (recitation) → citations
//!   labelled as recitations (entries without a `uri` are skipped).
//! - Long decode (Gemini 4): a chunk with `continuationToken` and no
//!   `finishReason` is a checkpoint (after 32K tokens, every ~5K): the usage so
//!   far is billed (`Usage`: a stream dropped after a checkpoint is paid up to
//!   it) and a `Checkpoint` carries the token and the parts committed so far.
//!   The final `continuationToken` (with `CONTINUATION` or `MAX_TOKENS`) is
//!   returned in `CallOutcome.continuation`.
//! - Usage: `usageMetadata` is cumulative within one internal model call:
//!   `LiveUsage` while streaming (display; after a code execution result the
//!   next call's output counts start again, so the finished calls' counts are
//!   added: the sums equal the final chunk's totals; after any tool result,
//!   counts that drop are taken as such a new call), `Usage` at checkpoints
//!   and at the end (as reported), with the estimated cost.
//! - What served the request (`modelVersion`, `usageMetadata.serviceTier`)
//!   is the bot's footer (from the response envelope); a `modelStatus` that is
//!   not STABLE is a notice, and so is a safety rating that blocked something
//!   or rates it MEDIUM or HIGH (prompt or answer; the ratings stay in the
//!   response for `/info`).
//! - Blocks: `promptFeedback.blockReason` and abnormal `finishReason`s become
//!   `Finish{content_filter|error}` with the provider's account in
//!   `FinishDetails`, and the call fails with an error carrying Google's
//!   `finishMessage` ([`ChunkParser::abnormal`]).

use base64::Engine as _;
use omni_core::Usage;
use omni_core::content::{Citation, ReasoningKind, ToolCall};
use omni_core::usage::CostSource;
use serde_json::{Value, json};

use super::errors::error_from_json;
use super::pricing::{self, GroundingUse, Tier};
use crate::error::{ErrorKind, LlmError, Retry};
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
    /// Output counts (candidates, thoughts) of finished internal model calls
    /// (code execution splits a response into several), and of the current one.
    calls_done: (u64, u64),
    call_now: (u64, u64),
    /// A tool result was fed back: the counts that follow may start again.
    may_restart: bool,
    /// Search Suggestions from `renderedContent` (label, link).
    chips: Vec<(String, String)>,
    chips_cited: bool,
    noted_status: bool,
    /// Safety ratings already noted (source, category, probability, blocked).
    noted_safety: Vec<String>,
    /// `toolType`s of the built-in tool calls returned as parts.
    traced: Vec<String>,
    /// Those calls still waiting for their response: (`toolType`, id).
    pending_tools: Vec<(String, String)>,
    /// The provider stopped the output abnormally: the call's error.
    abnormal: Option<LlmError>,
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
        // Tool parts already shown before the checkpoint this request resumes from.
        let mut traced: Vec<String> = Vec::new();
        for p in &carried {
            let t = ["toolCall", "toolResponse", "tool_call", "tool_response"].iter().find_map(|k| p.get(*k));
            if let Some(ty) = t.and_then(|t| get(t, "toolType", "tool_type")).and_then(Value::as_str)
                && !traced.iter().any(|x| x == ty)
            {
                traced.push(ty.to_string());
            }
        }
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
            calls_done: (0, 0),
            call_now: (0, 0),
            may_restart: false,
            chips: Vec::new(),
            chips_cited: false,
            noted_status: false,
            noted_safety: Vec::new(),
            traced,
            pending_tools: Vec::new(),
            abnormal: None,
            finished: false,
            continuation: None,
            any_output: false,
        }
    }

    /// The error of a stream the provider stopped abnormally (blocked prompt,
    /// content filter, error finish), with Google's `finishMessage`.
    pub fn abnormal(&self) -> Option<&LlmError> {
        self.abnormal.as_ref()
    }

    fn stopped(&mut self, reason: &str, message: String, details: Value) {
        let mut e = LlmError::new(ErrorKind::InBand, Retry::Never, message);
        e.error_type = Some(reason.to_string());
        e.details = Some(details);
        e.partial_output = self.any_output;
        self.abnormal = Some(e);
    }

    /// The usage to display while streaming: output counts of finished internal
    /// calls added to the current call's.
    fn live_view(&self, u: &Value) -> Value {
        if self.calls_done == (0, 0) {
            return u.clone();
        }
        let mut v = u.clone();
        v["candidatesTokenCount"] = json!(self.calls_done.0 + self.call_now.0);
        v["thoughtsTokenCount"] = json!(self.calls_done.1 + self.call_now.1);
        v
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

    /// A tool result was fed back: the next output counts may belong to a new
    /// internal call (they do after a code execution result; counted as one
    /// when they drop, see [`Self::counts`]).
    fn next_call(&mut self) {
        self.may_restart = true;
    }

    /// The output counts of a chunk: after a tool result, counts lower than
    /// before are a new internal call's (the finished call's are kept).
    fn counts(&mut self, now: (u64, u64)) {
        if std::mem::take(&mut self.may_restart) && (now.0 < self.call_now.0 || now.1 < self.call_now.1) {
            self.calls_done.0 += self.call_now.0;
            self.calls_done.1 += self.call_now.1;
        }
        self.call_now = now;
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
        // Every executed query is billed: web and image searches apart.
        let unique = |list: &[String]| {
            let mut q: Vec<&str> = list.iter().map(|q| q.trim()).filter(|q| !q.is_empty()).collect();
            q.sort_unstable();
            q.dedup();
            q.len()
        };
        GroundingUse {
            queries: unique(&self.queries) + unique(&self.image_queries),
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
        if !self.noted_status
            && let Some(ms) = get(data, "modelStatus", "model_status")
        {
            let stage = s(get(ms, "modelStage", "model_stage")).unwrap_or_default();
            if !matches!(stage.as_str(), "" | "STABLE" | "MODEL_STAGE_UNSPECIFIED") {
                self.noted_status = true;
                let mut n = format!("Google: `{}` is {stage}", self.model);
                if let Some(t) = s(get(ms, "retirementTime", "retirement_time")).filter(|t| !t.is_empty()) {
                    n.push_str(&format!(", retiring {t}"));
                }
                if let Some(m) = s(ms.get("message")).filter(|m| !m.is_empty()) {
                    n.push_str(&format!(": {m}"));
                }
                evs.push(StreamEvent::Notice(n));
            }
        }
        if let Some(r) =
            get(data, "promptFeedback", "prompt_feedback").and_then(|f| get(f, "safetyRatings", "safety_ratings"))
        {
            self.safety("prompt", r, &mut evs);
        }
        if let Some(r) = data
            .get("candidates")
            .and_then(Value::as_array)
            .and_then(|c| c.first())
            .and_then(|c| get(c, "safetyRatings", "safety_ratings"))
        {
            self.safety("answer", r, &mut evs);
        }
        if let Some(pf) = get(data, "promptFeedback", "prompt_feedback")
            && let Some(reason) = get(pf, "blockReason", "block_reason").and_then(Value::as_str)
        {
            evs.push(StreamEvent::FinishDetails(pf.clone()));
            evs.push(StreamEvent::Finish { reason: Some("content_filter".into()), native: Some(reason.into()) });
            self.finished = true;
            self.stopped("content_filter", format!("the prompt was blocked: {reason}"), pf.clone());
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
                    let message = s(get(cand, "finishMessage", "finish_message")).filter(|m| !m.trim().is_empty());
                    if abnormal {
                        let mut d = cand.clone();
                        if let Some(o) = d.as_object_mut() {
                            o.remove("content");
                        }
                        evs.push(StreamEvent::FinishDetails(d.clone()));
                        let text = message.unwrap_or_else(|| {
                            if reason == "content_filter" {
                                "stopped by the provider's content filter".to_string()
                            } else {
                                "the provider stopped the generation with an error".to_string()
                            }
                        });
                        self.stopped(reason, format!("{fr}: {text}"), d);
                    } else if let Some(m) = message {
                        evs.push(StreamEvent::Notice(format!("Google ({fr}): {m}")));
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
            self.counts((
                u.get("candidatesTokenCount").and_then(Value::as_u64).unwrap_or(0),
                u.get("thoughtsTokenCount").and_then(Value::as_u64).unwrap_or(0),
            ));
        }
        if let Some(u) = &self.usage {
            if finishing || checkpoint.is_some() || (self.finished && chunk_usage.is_some()) {
                evs.push(StreamEvent::Usage(self.priced(u)));
            } else if chunk_usage.is_some() {
                evs.push(StreamEvent::LiveUsage(self.priced(&self.live_view(u))));
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
                // A draft made while thinking stays with the reasoning.
                Ok(bytes) if mime.starts_with("image/") && thought => {
                    self.shown(true);
                    evs.push(StreamEvent::ThoughtImage { data: bytes, mime });
                }
                Ok(bytes) if mime.starts_with("image/") => {
                    self.shown(false);
                    evs.push(StreamEvent::Image { data: bytes, mime });
                }
                // A file written by code execution (anything but an image).
                Ok(bytes) => {
                    self.shown(false);
                    let name = s(get(d, "displayName", "display_name")).filter(|n| !n.is_empty());
                    evs.push(StreamEvent::File { data: bytes, mime, name });
                }
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
            self.next_call();
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
        } else if let Some((key, done)) = [("toolCall", false), ("toolResponse", true)]
            .into_iter()
            .find(|(k, _)| p.get(*k).is_some() || p.get(snake(k)).is_some())
        {
            // A built-in tool's own call or result (`includeServerSideToolInvocations`), where it ran.
            let t = get(p, key, snake(key)).unwrap_or(&Value::Null);
            self.tool_part(t, done, evs);
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

    /// A `toolCall` (`done`: a `toolResponse`) part: the call where it ran,
    /// its result paired by `id` (or with the oldest open call of its type).
    fn tool_part(&mut self, t: &Value, done: bool, evs: &mut Vec<StreamEvent>) {
        if done {
            self.next_call();
        }
        let ty = s(get(t, "toolType", "tool_type")).unwrap_or_default();
        let name = match ty.as_str() {
            // The tool the request names (`googleSearch`), as without these parts.
            "GOOGLE_SEARCH_WEB" => "google_search".to_string(),
            "GOOGLE_SEARCH_IMAGE" | "GOOGLE_MAPS" | "URL_CONTEXT" | "FILE_SEARCH" | "MEDIA_PROCESSING" => {
                ty.to_ascii_lowercase()
            }
            _ => s(get(t, "toolName", "tool_name"))
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| if ty.is_empty() { "tool".into() } else { ty.to_ascii_lowercase() }),
        };
        let id = match s(t.get("id")).filter(|i| !i.is_empty()) {
            Some(id) => {
                if done {
                    self.pending_tools.retain(|(_, x)| *x != id);
                } else {
                    self.pending_tools.push((ty.clone(), id.clone()));
                }
                id
            }
            None if done => match self.pending_tools.iter().position(|(x, _)| *x == ty) {
                Some(i) => self.pending_tools.remove(i).1,
                None => self.new_id("tool"),
            },
            None => {
                let id = self.new_id("tool");
                self.pending_tools.push((ty.clone(), id.clone()));
                id
            }
        };
        if !self.traced.contains(&ty) {
            self.traced.push(ty.clone());
        }
        let args = get(t, "args", "args").cloned();
        let response = t.get("response").cloned();
        if !done {
            // Billed per query (Maps queries are search queries too).
            let list = if ty == "GOOGLE_SEARCH_IMAGE" { &mut self.image_queries } else { &mut self.queries };
            if matches!(ty.as_str(), "GOOGLE_SEARCH_WEB" | "GOOGLE_SEARCH_IMAGE" | "GOOGLE_MAPS") {
                for q in args.as_ref().and_then(|a| a.get("queries")).and_then(Value::as_array).into_iter().flatten() {
                    if let Some(q) = q.as_str()
                        && !list.iter().any(|x| x == q)
                    {
                        list.push(q.to_string());
                    }
                }
            }
        } else if ty.starts_with("GOOGLE_SEARCH")
            && self.chips.is_empty()
            && let Some(html) = response.as_ref().and_then(|r| r.get("search_suggestions")).and_then(Value::as_str)
        {
            // The suggestions as the rendered widget, when the metadata has none.
            self.chips = search_chips(html);
        }
        let status = if !done {
            "in_progress"
        } else if ty == "URL_CONTEXT" && response.as_ref().is_some_and(all_urls_failed) {
            "failed"
        } else {
            "completed"
        };
        self.shown(false);
        evs.push(StreamEvent::Tool(ToolCall {
            id: Some(id),
            name,
            status: Some(status.into()),
            input: if done { None } else { args },
            output: if done { response } else { None },
        }));
    }

    /// A notice for each safety rating of `what` (prompt, answer) that blocked
    /// it or rates it MEDIUM or HIGH (once each).
    fn safety(&mut self, what: &str, ratings: &Value, evs: &mut Vec<StreamEvent>) {
        for r in ratings.as_array().into_iter().flatten() {
            let category = s(r.get("category")).unwrap_or_else(|| "HARM_CATEGORY_UNSPECIFIED".into());
            let probability = s(r.get("probability")).unwrap_or_default();
            let blocked = r.get("blocked").and_then(Value::as_bool) == Some(true);
            if !blocked && !matches!(probability.as_str(), "MEDIUM" | "HIGH") {
                continue;
            }
            let key = format!("{what} {category} {probability} {blocked}");
            if self.noted_safety.contains(&key) {
                continue;
            }
            self.noted_safety.push(key);
            let rated = if probability.is_empty() { String::new() } else { format!(" {probability}") };
            let blocked = if blocked { " (blocked)" } else { "" };
            evs.push(StreamEvent::Notice(format!("Google's safety rating of the {what}: {category}{rated}{blocked}")));
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
        let entry = get(g, "searchEntryPoint", "search_entry_point");
        // Search Suggestions without HTML: `sdkBlob` is "base64 encoded JSON representing array of tuple".
        if let Some(blob) = entry.and_then(|e| get(e, "sdkBlob", "sdk_blob")).and_then(Value::as_str)
            && let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(blob.trim())
            && let Ok(v) = serde_json::from_slice::<Value>(&bytes)
            && self.suggestions.as_ref() != Some(&v)
        {
            self.suggestions = Some(v);
            changed = true;
        }
        // The chips of the rendered widget (what Google actually sends): the label
        // and the link exactly as given.
        if let Some(html) = entry.and_then(|e| get(e, "renderedContent", "rendered_content")).and_then(Value::as_str) {
            let chips = search_chips(html);
            if !chips.is_empty() && chips != self.chips {
                if self.suggestions.is_none() || self.suggestions == Some(chips_json(&self.chips)) {
                    self.suggestions = Some(chips_json(&chips));
                }
                self.chips = chips;
                changed = true;
            }
        }
        if !changed || (self.queries.is_empty() && self.image_queries.is_empty()) {
            return;
        }
        // The real calls came as parts, where they ran.
        if self.traced.iter().any(|t| t.starts_with("GOOGLE_SEARCH") || t == "GOOGLE_MAPS") {
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
        // Reported with the last chunk, but run before the answer it grounds.
        evs.push(StreamEvent::ToolBefore(ToolCall {
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
        // Unchanged, or reported as parts where it ran.
        if !changed || self.traced.iter().any(|t| t == "URL_CONTEXT") {
            return;
        }
        let urls: Vec<Value> =
            self.url_meta.iter().filter_map(|m| get(m, "retrievedUrl", "retrieved_url").cloned()).collect();
        let ok = self.url_meta.iter().any(|m| {
            get(m, "urlRetrievalStatus", "url_retrieval_status").and_then(Value::as_str)
                == Some("URL_RETRIEVAL_STATUS_SUCCESS")
        });
        self.shown(false);
        // Retrieved before the answer it grounds, reported with it.
        evs.push(StreamEvent::ToolBefore(ToolCall {
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
                // (Google's titles usually carry it already: "People's Square - Google Maps".)
                let attributed = |t: Option<String>| {
                    Some(match t.filter(|t| !t.is_empty()) {
                        Some(t) if t.contains("Google Maps") => t,
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
        // The Search Suggestions, after the sources (Google's terms: grounded results are
        // shown with their suggestions; the links are kept exactly as Google gave them).
        if !self.chips_cited && !self.chips.is_empty() {
            self.chips_cited = true;
            for (label, url) in &self.chips {
                out.push(Citation {
                    url: Some(url.clone()),
                    title: Some(format!("Google Search: {label}")),
                    cited_text: None,
                    range: None,
                    extra: Some(json!({"searchSuggestion": {"text": label, "url": url}})),
                });
            }
        }
        for c in &self.recitations[self.recited..] {
            // Recitations without a source link say nothing a reader can follow.
            let Some(uri) = c.get("uri").and_then(Value::as_str).filter(|u| !u.trim().is_empty()) else {
                continue;
            };
            let uri = Some(uri.to_string());
            let start = c.get("startIndex").and_then(Value::as_u64).unwrap_or(0) as usize;
            let range = c.get("endIndex").and_then(Value::as_u64).map(|e| (start, e as usize)).filter(|(x, y)| {
                x <= y
                    && *y <= self.answer.len()
                    && self.answer.is_char_boundary(*x)
                    && self.answer.is_char_boundary(*y)
            });
            let title = Some(match c.get("license").and_then(Value::as_str).filter(|l| !l.is_empty()) {
                Some(l) => format!("{} (recitation, license: {l})", uri.as_deref().unwrap_or("source")),
                None => format!("{} (recitation)", uri.as_deref().unwrap_or("source")),
            });
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

/// `toolCall` → `tool_call` (proto3 JSON takes both spellings).
fn snake(camel: &str) -> &'static str {
    match camel {
        "toolCall" => "tool_call",
        "toolResponse" => "tool_response",
        _ => "",
    }
}

/// A URL-context tool response in which no URL was retrieved.
fn all_urls_failed(response: &Value) -> bool {
    let list = get(response, "url_metadata", "urlMetadata").and_then(Value::as_array);
    list.is_some_and(|l| {
        !l.is_empty()
            && !l.iter().any(|m| {
                get(m, "url_retrieval_status", "urlRetrievalStatus").and_then(Value::as_str)
                    == Some("URL_RETRIEVAL_STATUS_SUCCESS")
            })
    })
}

/// Search Suggestions as the tool call's output: `[{"text", "url"}]`.
fn chips_json(chips: &[(String, String)]) -> Value {
    Value::Array(chips.iter().map(|(t, u)| json!({"text": t, "url": u})).collect())
}

/// The Search Suggestion chips of `searchEntryPoint.renderedContent`: every
/// `<a class="chip" href="…">label</a>`, as (label, link), with HTML entities
/// decoded and the link otherwise exactly as given.
pub fn search_chips(html: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut rest = html;
    while let Some(i) = rest.find("<a") {
        let after = &rest[i + 2..];
        if !after.starts_with(|c: char| c.is_ascii_whitespace()) {
            rest = after;
            continue;
        }
        let Some(end) = after.find('>') else { break };
        let attrs = &after[..end];
        let body = &after[end + 1..];
        let Some(close) = body.find("</a>") else { break };
        let inner = &body[..close];
        rest = &body[close + 4..];
        if !attr(attrs, "class").is_some_and(|c| c.split_ascii_whitespace().any(|x| x == "chip")) {
            continue;
        }
        let Some(href) = attr(attrs, "href").map(|h| unescape(&h)).filter(|h| !h.is_empty()) else { continue };
        let label = unescape(&strip_tags(inner)).split_whitespace().collect::<Vec<_>>().join(" ");
        if !label.is_empty() && !out.iter().any(|(l, u)| *l == label && *u == href) {
            out.push((label, href));
        }
    }
    out
}

/// The value of attribute `name` in the inside of a tag (`a class="x" href=…`).
fn attr(tag: &str, name: &str) -> Option<String> {
    let mut from = 0;
    while let Some(pos) = tag[from..].find(name) {
        let start = from + pos;
        from = start + name.len();
        let boundary = start == 0 || tag[..start].ends_with(|c: char| c.is_ascii_whitespace());
        let Some(v) = tag[from..].trim_start().strip_prefix('=') else { continue };
        if !boundary {
            continue;
        }
        let v = v.trim_start();
        return Some(match v.chars().next() {
            Some(q @ ('"' | '\'')) => {
                let v = &v[1..];
                v[..v.find(q)?].to_string()
            }
            _ => v[..v.find(|c: char| c.is_ascii_whitespace()).unwrap_or(v.len())].to_string(),
        });
    }
    None
}

fn strip_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            c if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

/// HTML character references (`&amp;`, `&#39;`, `&#x2F;`, …) decoded.
fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        let tail = &rest[i..];
        let semi = tail.char_indices().take(12).find(|(_, c)| *c == ';').map(|(j, _)| j);
        let decoded = semi.and_then(|j| {
            let ent = &tail[1..j];
            let c = match ent {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                "nbsp" => Some('\u{a0}'),
                _ => match ent.strip_prefix('#') {
                    Some(h) if h.starts_with(['x', 'X']) => {
                        u32::from_str_radix(&h[1..], 16).ok().and_then(char::from_u32)
                    }
                    Some(d) => d.parse().ok().and_then(char::from_u32),
                    None => None,
                },
            };
            c.map(|c| (c, j))
        });
        match decoded {
            Some((c, j)) => {
                out.push(c);
                rest = &tail[j + 1..];
            }
            None => {
                out.push('&');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
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
