//! Moonshot's Responses API (`POST /v1/responses`, K3 series only), used for
//! web search: the `web_search` tool runs on Moonshot's side
//! (platform.kimi.com/docs/api/responses, 2026-11-12).
//!
//! Moonshot keeps no state (`store` is fixed to false), so the whole history
//! is sent: this dialect's output items verbatim for the same model (a
//! replayed `web_search_call` is ignored by Moonshot: "the results are carried
//! by the assistant message that follows"), Chat turns of the same model as
//! their reasoning (`reasoning_text`, required on K3) plus message, other turns
//! as text (assistant messages carry `output_text` only). Moonshot's reasoning
//! items carry the reasoning itself (the API calls it 推理内容, "reasoning
//! content") in `summary[]`, so it is shown as reasoning, not as a summary.
//!
//! The search: Moonshot first judges from the input whether one is needed
//! ("服务端会先根据输入判断是否需要搜索"); if so it runs it and puts one
//! `web_search_call` at the start of the output, with its query and (asked for
//! with `include`) its sources, which become the answer's sources: the
//! answer's `annotations` (their shape is unspecified) were empty in every
//! answer seen (2026-11-13). No `web_search_call` means no search ran (and
//! none is billed).

use omni_core::Usage;
use omni_core::content::{Citation, ReasoningKind, Role, ToolCall, Turn};
use omni_core::usage::CostSource;
use serde_json::{Value, json};

use super::{Content, ModelSpec};
use crate::error::{ErrorKind, LlmError};
use crate::event::StreamEvent;
use crate::openrouter::responses::EventParser;
use crate::{BlobLoader, chatstream};

/// The highest `input_image.detail` (`auto|low|high|original`).
pub(super) const IMAGE_DETAIL: &str = "original";

/// The conversation as Responses `input` items.
pub(super) async fn input_items(history: &[Turn], model: &str, blobs: &dyn BlobLoader) -> Result<Vec<Value>, LlmError> {
    let mut out = Vec::with_capacity(history.len());
    for turn in history {
        match turn.role {
            // Moonshot's message roles are user | assistant | developer ("handled as system instructions").
            Role::System => out.push(json!({"type": "message", "role": "developer", "content": turn.answer_text()})),
            Role::Assistant => out.extend(assistant_items(turn, model)),
            Role::User => {
                let mut parts = Vec::new();
                for c in super::user_content(turn, model, blobs).await? {
                    parts.push(match c {
                        Content::Text(t) => json!({"type": "input_text", "text": t}),
                        Content::Image(u) => json!({"type": "input_image", "image_url": u, "detail": IMAGE_DETAIL}),
                        Content::Video(_) => {
                            return Err(LlmError::request(format!(
                                "`{model}` takes no video on Moonshot's Responses API, which web search (+s) needs \
                                 (only Chat Completions takes video)"
                            )));
                        }
                    });
                }
                out.push(json!({"type": "message", "role": "user", "content": parts}));
            }
        }
    }
    Ok(out)
}

fn assistant_items(turn: &Turn, model: &str) -> Vec<Value> {
    if let Some(n) = &turn.native
        && n.model == model
    {
        if n.dialect == super::RESPONSES_DIALECT
            && let Some(items) = n.payload.get("output").and_then(Value::as_array)
        {
            return items.clone();
        }
        if n.dialect == super::DIALECT {
            return from_chat_message(&n.payload);
        }
    }
    vec![
        json!({"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": turn.answer_text()}]}),
    ]
}

/// A Chat Completions assistant message of the same model as Responses input items.
fn from_chat_message(m: &Value) -> Vec<Value> {
    let mut items = Vec::new();
    if let Some(r) = m.get("reasoning_content").and_then(Value::as_str).filter(|s| !s.is_empty()) {
        items.push(json!({"type": "reasoning", "content": [{"type": "reasoning_text", "text": r}]}));
    }
    let text = m.get("content").and_then(Value::as_str).unwrap_or("");
    items.push(json!({"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": text}]}));
    items
}

/// This dialect's output items as one Chat Completions assistant message
/// (`reasoning_content` + `content`); search calls are left out (the message
/// carries their results).
pub(super) fn to_chat_message(payload: &Value) -> Value {
    let mut reasoning: Vec<String> = Vec::new();
    let mut text = String::new();
    for item in payload.get("output").and_then(Value::as_array).into_iter().flatten() {
        match item.get("type").and_then(Value::as_str) {
            Some("reasoning") => {
                // `content` (reasoning_text) takes precedence over `summary`, as in Moonshot's input rule.
                let parts = item
                    .get("content")
                    .and_then(Value::as_array)
                    .filter(|a| !a.is_empty())
                    .or_else(|| item.get("summary").and_then(Value::as_array));
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

/// Responses events: the shared parser plus Moonshot's specifics (errors,
/// search calls with their query and sources, the sources as the answer's
/// sources, reasoning kind, cost).
pub(super) struct Parser {
    inner: EventParser,
    spec: Option<&'static ModelSpec>,
    cny_per_usd: f64,
    /// Citations came as `response.output_text.annotation.added` events or in
    /// the answer's `annotations`: the search sources stay with the search.
    annotated: bool,
    /// Ids of the `web_search_call` items.
    searches: Vec<String>,
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

    /// Whether a web search ran (Moonshot returns a `web_search_call` exactly then).
    pub(super) fn searched(&self) -> bool {
        !self.searches.is_empty()
    }

    /// The response ended normally (completed, or cut at the output limit) and
    /// ran no search: worth a note when one was asked for.
    pub(super) fn answered_without_search(&self) -> bool {
        matches!(self.finish.as_deref(), Some("stop" | "length")) && !self.searched()
    }

    /// A `response.failed` / `error` event as Moonshot's error, verbatim in `details`.
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

    /// The cost of the response: tokens plus one fee per executed search
    /// (each `web_search_call` in the output, else in the stream).
    fn price(&self, v: &Value, u: &mut Usage) {
        let searches = v.pointer("/response/output").and_then(Value::as_array).map_or(0, |o| {
            o.iter().filter(|i| i.get("type").and_then(Value::as_str) == Some("web_search_call")).count()
        });
        let searches = searches.max(self.searches.len());
        u.cost_usd = self.spec.and_then(|s| super::estimate_cost(s, u, searches as u64, self.cny_per_usd));
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

/// A search call: its `action` (the query) as input, its sources as output.
fn search_call(item: &Value, mut t: ToolCall) -> ToolCall {
    if let Some(action) = item.get("action").and_then(Value::as_object) {
        let mut action = action.clone();
        let sources = action.remove("sources").filter(|s| !s.is_null());
        t.input = Some(Value::Object(action));
        t.output = sources.map(|s| json!({"sources": s}));
    }
    t
}

/// `url_citation` annotations of a finished message item (their format is not
/// specified by Moonshot; anything else is ignored).
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
            // Only separates summary parts; Moonshot's reasoning is one text.
            "response.reasoning_summary_part.done" => return Ok(Vec::new()),
            "response.output_text.annotation.added" => self.annotated = true,
            _ => {}
        }
        let item = v.get("item");
        if let Some(i) = item.filter(|i| i.get("type").and_then(Value::as_str) == Some("web_search_call")) {
            self.search_item(i, v.get("output_index").and_then(Value::as_u64));
        }
        let mut out = Vec::with_capacity(evs.len() + 1);
        for ev in evs {
            match ev {
                StreamEvent::Reasoning { kind: ReasoningKind::Summary, text, block } => {
                    out.push(StreamEvent::Reasoning { kind: ReasoningKind::Text, text, block })
                }
                StreamEvent::Tool(t) if t.name == "web_search" => out.push(StreamEvent::Tool(match item {
                    Some(i) => search_call(i, t),
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
