//! OpenRouter Responses skin (`openrouter.responses`, `/responses`): OpenAI's
//! own API shape, used for OpenAI and xAI models.
//!
//! - History: the output items of earlier turns of the same model are
//!   replayed verbatim (encrypted reasoning, `phase` on messages, hosted tool
//!   calls), as OpenAI requires for stateless use; other turns become plain
//!   messages.
//! - Stream: `response.*` events → reasoning summaries, answer text,
//!   citations, hosted tool calls (code interpreter, web search).

use omni_core::content::{Citation, Part, ReasoningKind, Role, ToolCall, Turn};
use serde_json::{Map, Value, json};

use super::catalog::ModelInfo;
use super::options::{Plan, Reasoning, fetch_tool, search_tool};
use crate::error::{ErrorKind, LlmError};
use crate::event::StreamEvent;
use crate::request::Tool;
use crate::{BlobLoader, MediaClass, classify_mime};

pub const DIALECT: &str = "openrouter.responses";

fn data_url(mime: &str, bytes: &[u8]) -> String {
    use base64::Engine as _;
    format!("data:{mime};base64,{}", base64::engine::general_purpose::STANDARD.encode(bytes))
}

async fn load(blobs: &dyn BlobLoader, r: &omni_core::BlobRef) -> Result<Vec<u8>, LlmError> {
    blobs.load(r).await.map_err(|e| LlmError::request(format!("cannot load {r}: {e:#}")))
}

/// Convert the conversation to Responses `input` items.
pub async fn convert_history(
    history: &[Turn],
    model: &str,
    info: Option<&ModelInfo>,
    image_detail: &str,
    blobs: &dyn BlobLoader,
) -> Result<(Vec<Value>, Vec<String>), LlmError> {
    let mut notes = Vec::new();
    let mut out = Vec::new();
    for turn in history {
        match turn.role {
            Role::System => out.push(json!({
                "type": "message", "role": "system",
                "content": [{"type": "input_text", "text": turn.answer_text()}],
            })),
            Role::Assistant => {
                if let Some(n) = &turn.native
                    && n.dialect == DIALECT
                    && n.model == model
                    && let Some(items) = n.payload.get("output").and_then(Value::as_array)
                {
                    out.extend(items.iter().cloned());
                    continue;
                }
                out.push(json!({
                    "type": "message", "role": "assistant",
                    "content": [{"type": "output_text", "text": turn.answer_text()}],
                }));
            }
            Role::User => {
                let mut content: Vec<Value> = Vec::new();
                for part in &turn.parts {
                    match part {
                        Part::Text { text, .. } if !text.is_empty() => {
                            content.push(json!({"type": "input_text", "text": text}))
                        }
                        Part::Text { .. } => {}
                        Part::Image { blob, .. } => {
                            if let Some(i) = info
                                && !i.accepts("image")
                            {
                                return Err(LlmError::request(format!("{model} does not accept image input")));
                            }
                            let bytes = load(blobs, blob).await?;
                            content.push(json!({"type": "input_image", "image_url": data_url(&blob.mime, &bytes), "detail": image_detail}));
                        }
                        Part::File { blob, filename, .. } => {
                            let bytes = load(blobs, blob).await?;
                            let name = filename.clone().unwrap_or_else(|| "file".into());
                            match classify_mime(&blob.mime, filename.as_deref()) {
                                MediaClass::Pdf | MediaClass::Document => {
                                    if let Some(i) = info
                                        && !i.accepts("file")
                                    {
                                        return Err(LlmError::request(format!("{model} does not accept file input")));
                                    }
                                    content.push(json!({"type": "input_file", "filename": name, "file_data": data_url(&blob.mime, &bytes)}));
                                }
                                MediaClass::Image => {
                                    content.push(json!({"type": "input_image", "image_url": data_url(&blob.mime, &bytes), "detail": image_detail}));
                                }
                                MediaClass::Text => {
                                    let text = String::from_utf8(bytes)
                                        .map_err(|_| LlmError::request(format!("{name} is not valid UTF-8 text")))?;
                                    notes.push(format!(
                                        "📎 {name} inlined as a separate text part by the bot (no native text-file input on this API)"
                                    ));
                                    content.push(json!({"type": "input_text", "text": text}));
                                }
                                MediaClass::Audio | MediaClass::Video | MediaClass::Other => {
                                    return Err(LlmError::request(format!(
                                        "{model} does not take {} input ({name})",
                                        blob.mime
                                    )));
                                }
                            }
                        }
                        Part::RemoteMedia { url, .. } => {
                            return Err(LlmError::request(format!("{model} cannot read remote media ({url})")));
                        }
                        Part::Reasoning(_) | Part::ToolCall(_) | Part::Refusal { .. } => {}
                    }
                }
                out.push(json!({"type": "message", "role": "user", "content": content}));
            }
        }
    }
    Ok((out, notes))
}

/// The request body (without OpenRouter routing fields).
pub fn body(model: &str, input: Vec<Value>, plan: &Plan) -> Value {
    let mut b = json!({"model": model, "input": input, "stream": true, "store": false});
    let mut reasoning = Map::new();
    match &plan.reasoning {
        Reasoning::Effort(e) => {
            reasoning.insert("effort".into(), json!(e));
        }
        Reasoning::Off => {
            reasoning.insert("effort".into(), json!("none"));
        }
        Reasoning::Budget(_) | Reasoning::On | Reasoning::Unset => {}
    }
    if let Some(s) = plan.summary {
        reasoning.insert("summary".into(), json!(s));
    }
    if !reasoning.is_empty() {
        b["reasoning"] = Value::Object(reasoning);
        // Stateless replay needs the encrypted reasoning items.
        b["include"] = json!(["reasoning.encrypted_content"]);
    }
    if let Some(n) = plan.max_tokens {
        b["max_output_tokens"] = json!(n);
    }
    if let Some(v) = &plan.verbosity {
        b["text"] = json!({"verbosity": v});
    }
    let tools: Vec<Value> = plan
        .tools
        .iter()
        .map(|t| match t {
            Tool::Search => search_tool(),
            Tool::Code => json!({"type": "code_interpreter", "container": {"type": "auto"}}),
            Tool::Fetch => fetch_tool(),
        })
        .collect();
    if !tools.is_empty() {
        b["tools"] = Value::Array(tools);
    }
    b
}

/// Streaming state for one Responses call.
#[derive(Debug, Default)]
pub struct EventParser {
    /// Output items by `output_index`, as finally reported.
    items: Vec<Option<Value>>,
    /// Full output from `response.completed` / `response.incomplete`.
    final_output: Option<Vec<Value>>,
    /// Code being streamed per item id.
    code: Vec<(String, String)>,
    pub finished: bool,
    any_output: bool,
    pub upstream_body_echo: Vec<Value>,
}

fn tool_from_item(item: &Value) -> Option<ToolCall> {
    let ty = item.get("type").and_then(Value::as_str)?;
    let id = item.get("id").and_then(Value::as_str).map(str::to_string);
    let status = item.get("status").and_then(Value::as_str).map(str::to_string);
    match ty {
        "code_interpreter_call" => Some(ToolCall {
            id,
            name: "code_interpreter".into(),
            status,
            input: item.get("code").filter(|v| !v.is_null()).cloned(),
            output: item.get("outputs").filter(|v| !v.is_null()).cloned(),
        }),
        "web_search_call" => Some(ToolCall {
            id,
            name: "web_search".into(),
            status,
            input: item.get("action").filter(|v| !v.is_null()).cloned(),
            output: None,
        }),
        t if t.ends_with("_call") || t.starts_with("openrouter:") => {
            Some(ToolCall { id, name: t.trim_end_matches("_call").to_string(), status, input: None, output: None })
        }
        _ => None,
    }
}

fn citation(a: &Value) -> Option<Citation> {
    if a.get("type").and_then(Value::as_str) != Some("url_citation") {
        return None;
    }
    Some(Citation {
        url: a.get("url").and_then(Value::as_str).map(str::to_string),
        title: a.get("title").and_then(Value::as_str).map(str::to_string),
        cited_text: None,
        range: match (a.get("start_index").and_then(Value::as_u64), a.get("end_index").and_then(Value::as_u64)) {
            (Some(s), Some(e)) => Some((s as usize, e as usize)),
            _ => None,
        },
        extra: None,
    })
}

pub fn error_from_event(v: &Value) -> LlmError {
    let err = v.pointer("/response/error").or_else(|| v.get("error")).cloned().unwrap_or_else(|| v.clone());
    let mut e = super::chat::error_from_json(&err, None, None, ErrorKind::InBand);
    if e.error_type.is_none() {
        e.error_type = v.pointer("/response/error_type").and_then(Value::as_str).map(str::to_string);
    }
    if e.error_type.as_deref() == Some("invalid_prompt")
        || err.get("code").and_then(Value::as_str) == Some("invalid_prompt")
    {
        e.error_type = Some("context_length_exceeded".into());
        e.retry = crate::error::Retry::Never;
    }
    e.details = Some(v.clone());
    e
}

impl EventParser {
    fn set_item(&mut self, idx: usize, item: Value) {
        if self.items.len() <= idx {
            self.items.resize(idx + 1, None);
        }
        self.items[idx] = Some(item);
    }

    pub fn event(&mut self, v: &Value) -> Result<Vec<StreamEvent>, LlmError> {
        let mut evs = Vec::new();
        let ty = v.get("type").and_then(Value::as_str).unwrap_or("");
        let idx = v.get("output_index").and_then(Value::as_u64).unwrap_or(0) as usize;
        match ty {
            "response.created" | "response.in_progress" => {
                let r = v.get("response");
                let id = r.and_then(|r| r.get("id")).and_then(Value::as_str).map(str::to_string);
                let model = r.and_then(|r| r.get("model")).and_then(Value::as_str).map(str::to_string);
                if id.is_some() || model.is_some() {
                    evs.push(StreamEvent::Meta { upstream_id: id, provider: None, model });
                }
            }
            "response.debug" => {
                if let Some(b) = v.pointer("/debug/echo_upstream_body").or_else(|| v.get("echo_upstream_body")) {
                    self.upstream_body_echo.push(b.clone());
                }
            }
            "response.output_item.added" => {
                if let Some(item) = v.get("item") {
                    if let Some(t) = tool_from_item(item) {
                        evs.push(StreamEvent::Tool(t));
                    }
                    self.set_item(idx, item.clone());
                }
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                let d = v.get("delta").and_then(Value::as_str).unwrap_or("");
                if !d.is_empty() {
                    self.any_output = true;
                    let summary = ty.contains("summary");
                    let si =
                        v.get("summary_index").or_else(|| v.get("content_index")).and_then(Value::as_u64).unwrap_or(0);
                    evs.push(StreamEvent::Reasoning {
                        kind: if summary { ReasoningKind::Summary } else { ReasoningKind::Text },
                        text: d.to_string(),
                        block: idx * 1000 + si as usize,
                    });
                }
            }
            "response.reasoning_summary_part.done" => {
                // Separate consecutive summary parts.
                evs.push(StreamEvent::Reasoning {
                    kind: ReasoningKind::Summary,
                    text: "\n\n".into(),
                    block: idx * 1000 + v.get("summary_index").and_then(Value::as_u64).unwrap_or(0) as usize,
                });
            }
            "response.output_text.delta" => {
                let d = v.get("delta").and_then(Value::as_str).unwrap_or("");
                if !d.is_empty() {
                    self.any_output = true;
                    evs.push(StreamEvent::Text(d.to_string()));
                }
            }
            "response.output_text.annotation.added" => {
                if let Some(c) = v.get("annotation").and_then(citation) {
                    evs.push(StreamEvent::Citations(vec![c]));
                }
            }
            "response.refusal.delta" => {
                let d = v.get("delta").and_then(Value::as_str).unwrap_or("");
                if !d.is_empty() {
                    self.any_output = true;
                    evs.push(StreamEvent::Refusal(d.to_string()));
                }
            }
            "response.code_interpreter_call_code.delta" => {
                let id = v.get("item_id").and_then(Value::as_str).unwrap_or("").to_string();
                let d = v.get("delta").and_then(Value::as_str).unwrap_or("");
                let code = match self.code.iter_mut().find(|(i, _)| *i == id) {
                    Some((_, c)) => {
                        c.push_str(d);
                        c.clone()
                    }
                    None => {
                        self.code.push((id.clone(), d.to_string()));
                        d.to_string()
                    }
                };
                self.any_output = true;
                evs.push(StreamEvent::Tool(ToolCall {
                    id: Some(id),
                    name: "code_interpreter".into(),
                    status: Some("in_progress".into()),
                    input: Some(json!(code)),
                    output: None,
                }));
            }
            t if t.starts_with("response.code_interpreter_call.") || t.starts_with("response.web_search_call.") => {
                let status = t.rsplit('.').next().unwrap_or("").to_string();
                let name = if t.contains("code_interpreter") { "code_interpreter" } else { "web_search" };
                evs.push(StreamEvent::Tool(ToolCall {
                    id: v.get("item_id").and_then(Value::as_str).map(str::to_string),
                    name: name.into(),
                    status: Some(status),
                    input: None,
                    output: None,
                }));
            }
            "response.output_item.done" => {
                if let Some(item) = v.get("item") {
                    match item.get("type").and_then(Value::as_str) {
                        Some("reasoning") => {
                            let has_summary =
                                item.get("summary").and_then(Value::as_array).is_some_and(|s| !s.is_empty());
                            if !has_summary && item.get("encrypted_content").is_some_and(|e| !e.is_null()) {
                                evs.push(StreamEvent::OpaqueReasoning { kind: ReasoningKind::Encrypted });
                            }
                        }
                        _ => {
                            if let Some(t) = tool_from_item(item) {
                                evs.push(StreamEvent::Tool(t));
                            }
                        }
                    }
                    self.set_item(idx, item.clone());
                }
            }
            "response.completed" | "response.incomplete" => {
                self.finished = true;
                let r = v.get("response").cloned().unwrap_or(Value::Null);
                if let Some(out) = r.get("output").and_then(Value::as_array) {
                    self.final_output = Some(out.clone());
                }
                if let Some(d) = r.get("incomplete_details").filter(|d| !d.is_null()) {
                    evs.push(StreamEvent::FinishDetails(d.clone()));
                }
                let (reason, native) = if ty == "response.incomplete" {
                    let why = r.pointer("/incomplete_details/reason").and_then(Value::as_str).unwrap_or("incomplete");
                    let norm = match why {
                        "max_output_tokens" => "length",
                        "content_filter" => "content_filter",
                        _ => "length",
                    };
                    (norm.to_string(), Some(why.to_string()))
                } else {
                    ("stop".to_string(), r.get("status").and_then(Value::as_str).map(str::to_string))
                };
                if let Some(u) = r.get("usage").filter(|u| u.is_object()) {
                    evs.push(StreamEvent::Usage(super::chat::usage_from_json(u)));
                }
                evs.push(StreamEvent::Finish { reason: Some(reason), native });
            }
            "response.failed" | "error" => return Err(error_from_event(v)),
            _ => {}
        }
        Ok(evs)
    }

    /// The assistant output items for replay.
    pub fn native_payload(&self) -> Value {
        let items: Vec<Value> = match &self.final_output {
            Some(o) if !o.is_empty() => o.clone(),
            _ => self.items.iter().flatten().cloned().collect(),
        };
        json!({"output": items})
    }
}

impl crate::chatstream::EventParser for EventParser {
    fn event(&mut self, _name: Option<&str>, data: &Value) -> Result<Vec<StreamEvent>, LlmError> {
        EventParser::event(self, data)
    }
    fn finished(&self) -> bool {
        self.finished
    }
    fn any_output(&self) -> bool {
        self.any_output
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_stream_with_reasoning_code_and_text() {
        let mut p = EventParser::default();
        let evs = [
            json!({"type": "response.created", "response": {"id": "gen-1", "model": "openai/gpt-6-luna"}}),
            json!({"type": "response.output_item.added", "output_index": 0, "item": {"type": "reasoning", "id": "rs_1"}}),
            json!({"type": "response.reasoning_summary_text.delta", "output_index": 0, "summary_index": 0, "delta": "Think"}),
            json!({"type": "response.output_item.done", "output_index": 0, "item": {"type": "reasoning", "id": "rs_1", "summary": [{"type": "summary_text", "text": "Think"}], "encrypted_content": "enc"}}),
            json!({"type": "response.output_item.added", "output_index": 1, "item": {"type": "code_interpreter_call", "id": "ci_1", "status": "in_progress"}}),
            json!({"type": "response.code_interpreter_call_code.delta", "output_index": 1, "item_id": "ci_1", "delta": "print(1)"}),
            json!({"type": "response.output_item.done", "output_index": 1, "item": {"type": "code_interpreter_call", "id": "ci_1", "status": "completed", "code": "print(1)", "outputs": null}}),
            json!({"type": "response.output_text.delta", "output_index": 2, "delta": "1"}),
            json!({"type": "response.output_text.annotation.added", "output_index": 2, "annotation": {"type": "url_citation", "url": "https://a.b", "title": "A"}}),
            json!({"type": "response.completed", "response": {"status": "completed", "output": [{"type": "reasoning", "id": "rs_1"}, {"type": "message", "id": "msg_1", "phase": "final_answer", "content": [{"type": "output_text", "text": "1"}]}], "usage": {"input_tokens": 5, "output_tokens": 7, "cost": 0.01}}}),
        ];
        let mut acc = crate::Accumulator::new();
        for e in &evs {
            for x in p.event(e).unwrap() {
                acc.apply(x);
            }
        }
        assert!(p.finished);
        let parts = acc.parts();
        assert!(matches!(&parts[0], Part::Reasoning(r) if r.text.as_deref() == Some("Think")));
        assert!(
            matches!(&parts[1], Part::ToolCall(t) if t.name == "code_interpreter" && t.status.as_deref() == Some("completed"))
        );
        assert!(matches!(&parts[2], Part::Text { text, citations } if text == "1" && citations.len() == 1));
        assert_eq!(acc.finish_reason.as_deref(), Some("stop"));
        let native = p.native_payload();
        assert_eq!(native["output"][1]["phase"], "final_answer");
    }

    #[test]
    fn incomplete_is_length() {
        let mut p = EventParser::default();
        let evs = p
            .event(&json!({"type": "response.incomplete", "response": {"incomplete_details": {"reason": "max_output_tokens"}, "output": []}}))
            .unwrap();
        assert!(evs.iter().any(|e| matches!(e, StreamEvent::Finish { reason: Some(r), native: Some(n) } if r == "length" && n == "max_output_tokens")));
    }
}
