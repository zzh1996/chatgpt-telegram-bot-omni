//! Alibaba's Responses API (`POST /compatible-mode/v1/responses`), used for the
//! code interpreter (and, together with it, web search and the web extractor):
//! help.aliyun.com/zh/model-studio/qwen-api-via-openai-responses, 2026-11-12.
//!
//! - Sent with `store: false` (storing is the default), so nothing is kept on
//!   Alibaba's side and the whole history goes with every request.
//! - Input: text and images (data URLs). Videos and PDFs are only taken as
//!   public URLs here: rejected.
//! - Replay: the output items of the same model that the API takes back as
//!   input (`message`, `reasoning`, `web_search_call`); code-interpreter and
//!   web-extractor calls are no input item type (their results are in the
//!   answer). Chat turns go as text: a `reasoning` input item needs the id of a
//!   response.
//! - Stream: `response.*` events (reasoning as `reasoning_text` deltas); tool
//!   calls carry their code and logs, queries and sources, URLs and extracts.

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
pub(super) async fn input_items(
    history: &[Turn],
    model: &str,
    spec: &ModelSpec,
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
                                "`{model}`: the code interpreter (`+c`) needs Alibaba's Responses API, which takes \
                                 videos only as public URLs"
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
/// call details, cost).
pub(super) struct Parser {
    inner: EventParser,
    spec: Option<&'static ModelSpec>,
    cny_per_usd: f64,
    annotation_events: bool,
}

impl Parser {
    pub(super) fn new(spec: Option<&'static ModelSpec>, cny_per_usd: f64) -> Self {
        Self { inner: EventParser::default(), spec, cny_per_usd, annotation_events: false }
    }

    /// The output items, for replay.
    pub(super) fn native_payload(&self) -> Value {
        self.inner.native_payload()
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

    /// Tokens plus web searches (billed like the `agent` strategy).
    fn price(&self, v: &Value, u: &mut Usage) {
        let r = v.get("response");
        let searches = u
            .raw
            .pointer("/x_tools/web_search/count")
            .or_else(|| r.and_then(|r| r.pointer("/x_tools/web_search/count")))
            .and_then(Value::as_u64)
            .unwrap_or_else(|| {
                r.and_then(|r| r.get("output"))
                    .and_then(Value::as_array)
                    .map_or(0, |o| o.iter().filter(|i| i["type"] == "web_search_call").count() as u64)
            });
        u.cost_usd = self.spec.and_then(|s| super::estimate_cost(s, u, searches, "agent", self.cny_per_usd));
        u.cost_source = if u.cost_usd.is_some() { CostSource::Estimated } else { CostSource::Unknown };
    }
}

/// Tool details from a full output item: search query and sources, extracted
/// URLs and content.
fn enrich(item: &Value, mut t: ToolCall) -> ToolCall {
    let get = |k: &str| item.get(k).filter(|v| !v.is_null());
    match item.get("type").and_then(Value::as_str) {
        Some("web_search_call") => {
            let action = item.get("action");
            let field = |k: &str| action.and_then(|a| a.get(k)).filter(|v| !v.is_null());
            t.input =
                field("query").map(|q| json!({"query": q})).or_else(|| field("queries").map(|q| json!({"queries": q})));
            t.output = field("sources").map(|s| json!({"sources": s}));
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
            "response.output_text.annotation.added" => self.annotation_events = true,
            _ => {}
        }
        let item = v.get("item");
        let mut out = Vec::with_capacity(evs.len() + 1);
        for ev in evs {
            out.push(match ev {
                StreamEvent::Tool(t) => StreamEvent::Tool(match item {
                    Some(i) => enrich(i, t),
                    None => t,
                }),
                StreamEvent::Usage(mut u) => {
                    self.price(v, &mut u);
                    StreamEvent::Usage(u)
                }
                other => other,
            });
        }
        if ty == "response.output_item.done"
            && !self.annotation_events
            && let Some(i) = item.filter(|i| i.get("type").and_then(Value::as_str) == Some("message"))
        {
            let cites = citations(i);
            if !cites.is_empty() {
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
