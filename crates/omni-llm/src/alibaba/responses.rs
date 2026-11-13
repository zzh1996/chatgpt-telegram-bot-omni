//! Alibaba's Responses API (`POST /compatible-mode/v1/responses`), used for web
//! search and the code interpreter (and the web extractor, which needs web
//! search): help.aliyun.com/zh/model-studio/qwen-api-via-openai-responses and
//! `web-search`, 2026-11-13.
//!
//! - Sent with `store: false` (storing is the default), so nothing is kept on
//!   Alibaba's side and the whole history goes with every request.
//! - Input: text and images (data URLs). Videos and PDFs are only taken as
//!   public URLs here: rejected. Assistant messages carry `output_text` only.
//! - Replay: the output items of the same model that the API takes back as
//!   input (`message`, `reasoning`, `web_search_call`); code-interpreter and
//!   web-extractor calls are no input item type (their results are in the
//!   answer). Chat turns go as text: a `reasoning` input item needs the id of a
//!   response.
//! - Stream: `response.*` events (reasoning as `reasoning_text` deltas); tool
//!   calls carry their code and logs, URLs and extracts, and each search its
//!   `action` (the query; the sources come with the finished item). The
//!   answer's `annotations` are "usually empty", so when it has no
//!   `url_citation` the sources of the searches before it are its sources.
//! - Searches: the model decides whether and how often to search. Each one is
//!   a `web_search_call`; the billed count is `tool_usage.web_search.num_requests`
//!   (else `usage.x_tools.web_search.count`, else the calls). A response
//!   without a search has neither calls nor counts ([`Parser::searched`]): the
//!   answer then notes that none ran.

use omni_core::Usage;
use omni_core::content::{Citation, Role, ToolCall, Turn};
use omni_core::usage::CostSource;
use serde_json::{Value, json};

use super::{Content, ModelSpec};
use crate::error::{ErrorKind, LlmError};
use crate::event::StreamEvent;
use crate::openrouter::responses::EventParser;
use crate::{BlobLoader, chatstream};

/// Output item types the API takes back as input items.
const REPLAYED: &[&str] = &["message", "reasoning", "web_search_call"];

/// The conversation as Responses `input` items; also whether it has images.
/// `why`: what needs this API ("web search (`+s`) needs"), for errors.
pub(super) async fn input_items(
    history: &[Turn],
    model: &str,
    spec: &ModelSpec,
    why: &str,
    blobs: &dyn BlobLoader,
    notes: &mut Vec<String>,
) -> Result<(Vec<Value>, bool), LlmError> {
    let mut out = Vec::with_capacity(history.len());
    let mut images = false;
    for turn in history {
        match turn.role {
            Role::System => out.push(json!({
                "type": "message", "role": "system",
                "content": [{"type": "input_text", "text": turn.answer_text()}],
            })),
            Role::Assistant => out.extend(assistant_items(turn, model)),
            Role::User => {
                let mut parts = Vec::new();
                for c in super::user_content(turn, model, Some(spec), blobs, notes).await? {
                    parts.push(match c {
                        Content::Text(t) => json!({"type": "input_text", "text": t}),
                        Content::Image(u) => {
                            images = true;
                            json!({"type": "input_image", "image_url": u})
                        }
                        Content::Video(_) => {
                            return Err(LlmError::request(format!(
                                "`{model}`: {why} Alibaba's Responses API, which takes videos only as public URLs"
                            )));
                        }
                    });
                }
                out.push(json!({"type": "message", "role": "user", "content": parts}));
            }
        }
    }
    Ok((out, images))
}

fn assistant_items(turn: &Turn, model: &str) -> Vec<Value> {
    if let Some(n) = &turn.native
        && n.model == model
        && n.dialect == super::RESPONSES_DIALECT
        && let Some(items) = n.payload.get("output").and_then(Value::as_array)
    {
        return items
            .iter()
            .filter(|i| i.get("type").and_then(Value::as_str).is_some_and(|t| REPLAYED.contains(&t)))
            .cloned()
            .collect();
    }
    vec![
        json!({"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": turn.answer_text()}]}),
    ]
}

/// This dialect's output items as one Chat Completions assistant message
/// (`reasoning_content` + `content`).
pub(super) fn to_chat_message(payload: &Value) -> Value {
    let mut reasoning: Vec<String> = Vec::new();
    let mut text = String::new();
    for item in payload.get("output").and_then(Value::as_array).into_iter().flatten() {
        match item.get("type").and_then(Value::as_str) {
            Some("reasoning") => {
                let parts = item
                    .get("summary")
                    .and_then(Value::as_array)
                    .filter(|a| !a.is_empty())
                    .or_else(|| item.get("content").and_then(Value::as_array));
                let t: String =
                    parts.into_iter().flatten().filter_map(|p| p.get("text").and_then(Value::as_str)).collect();
                if !t.is_empty() {
                    reasoning.push(t);
                }
            }
            Some("message") => {
                for c in item.get("content").and_then(Value::as_array).into_iter().flatten() {
                    if c.get("type").and_then(Value::as_str) == Some("output_text")
                        && let Some(t) = c.get("text").and_then(Value::as_str)
                    {
                        text.push_str(t);
                    }
                }
            }
            _ => {}
        }
    }
    let mut m = json!({"role": "assistant", "content": text});
    if !reasoning.is_empty() {
        m["reasoning_content"] = json!(reasoning.join("\n\n"));
    }
    m
}

/// Responses events: the shared parser plus Alibaba's specifics (errors, tool
/// call details, search sources as the answer's sources, cost).
pub(super) struct Parser {
    inner: EventParser,
    spec: Option<&'static ModelSpec>,
    cny_per_usd: f64,
    /// Citations came as `response.output_text.annotation.added` events or in
    /// the answer's `annotations`: the search sources stay with the searches.
    annotated: bool,
    /// Ids of the `web_search_call` items.
    searches: Vec<String>,
    /// The searches the finished response counts (see [`reported_searches`]).
    counted: u64,
    /// Search sources not attached to an answer yet.
    sources: Vec<Citation>,
    /// Every URL taken as a source (each is cited once).
    cited: Vec<String>,
    /// Answer text was streamed.
    text: bool,
    /// The normalized finish reason.
    finish: Option<String>,
}

impl Parser {
    pub(super) fn new(spec: Option<&'static ModelSpec>, cny_per_usd: f64) -> Self {
        Self {
            inner: EventParser::default(),
            spec,
            cny_per_usd,
            annotated: false,
            searches: Vec::new(),
            counted: 0,
            sources: Vec::new(),
            cited: Vec::new(),
            text: false,
            finish: None,
        }
    }

    /// The output items, for replay.
    pub(super) fn native_payload(&self) -> Value {
        self.inner.native_payload()
    }

    /// Whether a web search ran: a `web_search_call`, or a search count in the
    /// finished response (neither when the model did not search).
    pub(super) fn searched(&self) -> bool {
        !self.searches.is_empty() || self.counted > 0
    }

    /// The response ended normally (completed, or cut at the output limit) and
    /// ran no search: worth a note when one was asked for.
    pub(super) fn answered_without_search(&self) -> bool {
        matches!(self.finish.as_deref(), Some("stop" | "length")) && !self.searched()
    }

    fn error(&self, v: &Value) -> LlmError {
        let err = v
            .pointer("/response/error")
            .filter(|e| e.is_object())
            .or_else(|| v.get("error").filter(|e| e.is_object()))
            .unwrap_or(v);
        let mut e = super::error_from_json(err, None, None, ErrorKind::InBand);
        e.details = Some(v.clone());
        e.partial_output = chatstream::EventParser::any_output(&self.inner);
        e
    }

    /// Tokens plus the billed web searches (billed like the `agent` strategy).
    fn price(&self, v: &Value, u: &mut Usage) {
        let in_output = v.pointer("/response/output").and_then(Value::as_array).map_or(0, |o| {
            o.iter().filter(|i| i.get("type").and_then(Value::as_str) == Some("web_search_call")).count()
        });
        let searches = reported_searches(v).unwrap_or(self.searches.len().max(in_output) as u64);
        u.cost_usd = self.spec.and_then(|s| super::estimate_cost(s, u, searches, self.cny_per_usd));
        u.cost_source = if u.cost_usd.is_some() { CostSource::Estimated } else { CostSource::Unknown };
    }

    /// A search item (`index`: its output index): its id, and its sources
    /// (once per URL) for the answer.
    fn search_item(&mut self, item: &Value, index: Option<u64>) {
        let key = match item.get("id").and_then(Value::as_str) {
            Some(id) => id.to_string(),
            None => format!("#{}", index.unwrap_or(0)),
        };
        if !self.searches.contains(&key) {
            self.searches.push(key);
        }
        for s in item.pointer("/action/sources").and_then(Value::as_array).into_iter().flatten() {
            let Some(url) = s.get("url").and_then(Value::as_str).filter(|u| !u.is_empty()) else { continue };
            if self.cited.iter().any(|c| c == url) {
                continue;
            }
            self.cited.push(url.to_string());
            self.sources.push(Citation {
                url: Some(url.to_string()),
                title: s.get("title").and_then(Value::as_str).filter(|t| !t.is_empty()).map(str::to_string),
                cited_text: None,
                range: None,
                extra: None,
            });
        }
    }

    /// The search sources not attached yet, as the answer's sources (unless
    /// the answer cites itself).
    fn sources_event(&mut self) -> Option<StreamEvent> {
        (!self.annotated && self.text && !self.sources.is_empty())
            .then(|| StreamEvent::Citations(std::mem::take(&mut self.sources)))
    }
}

/// The searches a finished response (`response.completed`/`incomplete`)
/// counts: the billed ones (`tool_usage.web_search.num_requests`, "计费搜索次数"),
/// else `usage.x_tools.web_search.count`.
fn reported_searches(v: &Value) -> Option<u64> {
    let r = v.get("response")?;
    r.pointer("/tool_usage/web_search/num_requests")
        .or_else(|| r.pointer("/usage/x_tools/web_search/count"))
        .and_then(Value::as_u64)
}

/// Tool details from a full output item: the search action (the query; its
/// sources as the output), extracted URLs and content.
fn enrich(item: &Value, mut t: ToolCall) -> ToolCall {
    let get = |k: &str| item.get(k).filter(|v| !v.is_null());
    match item.get("type").and_then(Value::as_str) {
        Some("web_search_call") => {
            if let Some(action) = item.get("action").and_then(Value::as_object) {
                let mut action = action.clone();
                let sources = action.remove("sources").filter(|s| !s.is_null());
                t.input = Some(Value::Object(action));
                t.output = sources.map(|s| json!({"sources": s}));
            }
        }
        Some("web_extractor_call") => {
            let mut input = serde_json::Map::new();
            for k in ["urls", "goal"] {
                if let Some(v) = get(k) {
                    input.insert(k.into(), v.clone());
                }
            }
            t.input = (!input.is_empty()).then_some(Value::Object(input));
            t.output = get("output").cloned();
        }
        _ => {}
    }
    t
}

fn citations(item: &Value) -> Vec<Citation> {
    item.get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|c| c.get("annotations").and_then(Value::as_array))
        .flatten()
        .filter(|a| a.get("type").and_then(Value::as_str) == Some("url_citation"))
        .map(|a| Citation {
            url: a.get("url").and_then(Value::as_str).map(str::to_string),
            title: a.get("title").and_then(Value::as_str).map(str::to_string),
            cited_text: None,
            range: match (a.get("start_index").and_then(Value::as_u64), a.get("end_index").and_then(Value::as_u64)) {
                (Some(s), Some(e)) => Some((s as usize, e as usize)),
                _ => None,
            },
            extra: None,
        })
        .collect()
}

impl chatstream::EventParser for Parser {
    fn event(&mut self, _name: Option<&str>, v: &Value) -> Result<Vec<StreamEvent>, LlmError> {
        let ty = v.get("type").and_then(Value::as_str).unwrap_or("");
        let Ok(evs) = self.inner.event(v) else { return Err(self.error(v)) };
        match ty {
            // Summary-part separators (OpenAI's multi-part summaries) do not apply: Qwen streams `reasoning_text`.
            "response.reasoning_summary_part.done" => return Ok(Vec::new()),
            "response.output_text.annotation.added" => self.annotated = true,
            "response.completed" | "response.incomplete" => self.counted = reported_searches(v).unwrap_or(0),
            _ => {}
        }
        let item = v.get("item");
        if let Some(i) = item.filter(|i| i.get("type").and_then(Value::as_str) == Some("web_search_call")) {
            self.search_item(i, v.get("output_index").and_then(Value::as_u64));
        }
        let mut out = Vec::with_capacity(evs.len() + 1);
        for ev in evs {
            match ev {
                StreamEvent::Tool(t) => out.push(StreamEvent::Tool(match item {
                    Some(i) => enrich(i, t),
                    None => t,
                })),
                StreamEvent::Usage(mut u) => {
                    self.price(v, &mut u);
                    out.push(StreamEvent::Usage(u));
                }
                StreamEvent::Text(t) => {
                    self.text = true;
                    out.push(StreamEvent::Text(t));
                }
                // Sources still waiting (no finished answer item came) go to the answer before the end.
                StreamEvent::Finish { reason, native } => {
                    out.extend(self.sources_event());
                    self.finish.clone_from(&reason);
                    out.push(StreamEvent::Finish { reason, native });
                }
                other => out.push(other),
            }
        }
        if ty == "response.output_item.done"
            && let Some(i) = item.filter(|i| i.get("type").and_then(Value::as_str) == Some("message"))
        {
            let cites = if self.annotated { Vec::new() } else { citations(i) };
            if cites.is_empty() {
                out.extend(self.sources_event());
            } else {
                self.annotated = true;
                out.push(StreamEvent::Citations(cites));
            }
        }
        Ok(out)
    }
    fn finished(&self) -> bool {
        self.inner.finished
    }
    fn any_output(&self) -> bool {
        chatstream::EventParser::any_output(&self.inner)
    }
}
