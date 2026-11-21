//! OpenRouter Messages skin (`openrouter.messages`, `/messages`): Anthropic's
//! own API shape, used for Claude models.
//!
//! - History: earlier assistant turns of the same model are replayed as their
//!   original content blocks (thinking with signatures, text with citations,
//!   server tool use and results); other turns become plain text.
//! - Thinking: adaptive with `output_config.effort` (the catalog's default level
//!   unless an option sets one) on models with effort levels, a token budget on
//!   older ones; `display: summarized` where the model has the option (its default,
//!   `omitted`, would hide the thinking).
//! - Stream: Anthropic's events (`content_block_*`, `message_delta`, …).

use omni_core::content::{Citation, Part, ReasoningKind, Role, ToolCall, Turn};
use serde_json::{Value, json};

use super::catalog::ModelInfo;
use super::options::{Plan, Reasoning, fetch_tool, search_tool};
use crate::error::{ErrorKind, LlmError};
use crate::event::StreamEvent;
use crate::request::Tool;
use crate::{BlobLoader, MediaClass, classify_mime};

pub const DIALECT: &str = "openrouter.messages";

/// Output budget when nothing better is known (Anthropic requires `max_tokens`).
pub const FALLBACK_MAX_TOKENS: u64 = 32_000;

fn b64(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

async fn load(blobs: &dyn BlobLoader, r: &omni_core::BlobRef) -> Result<Vec<u8>, LlmError> {
    blobs.load(r).await.map_err(|e| LlmError::request(format!("cannot load {r}: {e:#}")))
}

/// Claude models with the `thinking.display` option (5.x, Opus 4.7/4.8).
pub fn has_display_option(model: &str) -> bool {
    let m = model.strip_prefix("anthropic/").unwrap_or(model);
    let ver: Option<(u32, u32)> = m.split('-').skip(1).find_map(|seg| {
        let (a, b) = seg.split_once('.').unwrap_or((seg, "0"));
        Some((a.parse().ok()?, b.parse().ok()?))
    });
    matches!(ver, Some((maj, min)) if maj >= 5 || (maj == 4 && min >= 7))
}

/// Convert the conversation: (`system`, `messages`, notes).
pub async fn convert_history(
    history: &[Turn],
    model: &str,
    info: Option<&ModelInfo>,
    blobs: &dyn BlobLoader,
) -> Result<(Option<Value>, Vec<Value>, Vec<String>), LlmError> {
    let mut notes = Vec::new();
    let mut system: Vec<Value> = Vec::new();
    let mut out: Vec<Value> = Vec::new();
    for turn in history {
        match turn.role {
            Role::System => system.push(json!({"type": "text", "text": turn.answer_text()})),
            Role::Assistant => {
                if let Some(n) = &turn.native
                    && n.dialect == DIALECT
                    && n.model == model
                    && let Some(blocks) = n.payload.get("content").and_then(Value::as_array)
                {
                    out.push(json!({"role": "assistant", "content": blocks}));
                    continue;
                }
                out.push(json!({"role": "assistant", "content": [{"type": "text", "text": turn.answer_text()}]}));
            }
            Role::User => {
                let mut content: Vec<Value> = Vec::new();
                for part in &turn.parts {
                    match part {
                        Part::Text { text, .. } if !text.is_empty() => {
                            content.push(json!({"type": "text", "text": text}))
                        }
                        Part::Text { .. } => {}
                        Part::Image { blob, .. } => {
                            if let Some(i) = info
                                && !i.accepts("image")
                            {
                                return Err(LlmError::request(format!("{model} does not accept image input")));
                            }
                            let bytes = load(blobs, blob).await?;
                            content.push(json!({"type": "image", "source": {"type": "base64", "media_type": blob.mime, "data": b64(&bytes)}}));
                        }
                        Part::File { blob, filename, .. } => {
                            let bytes = load(blobs, blob).await?;
                            let name = filename.clone().unwrap_or_else(|| "file".into());
                            match classify_mime(&blob.mime, filename.as_deref()) {
                                MediaClass::Pdf => content.push(json!({
                                    "type": "document",
                                    "source": {"type": "base64", "media_type": "application/pdf", "data": b64(&bytes)},
                                    "title": name,
                                })),
                                MediaClass::Image => content.push(json!({
                                    "type": "image", "source": {"type": "base64", "media_type": blob.mime, "data": b64(&bytes)}
                                })),
                                MediaClass::Text => {
                                    let text = String::from_utf8(bytes)
                                        .map_err(|_| LlmError::request(format!("{name} is not valid UTF-8 text")))?;
                                    content.push(json!({"type": "text", "text": text}));
                                }
                                MediaClass::Document | MediaClass::Audio | MediaClass::Video | MediaClass::Other => {
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
                        Part::Location { .. } => return Err(LlmError::request(super::NO_LOCATION)),
                        Part::Reasoning(_) | Part::ToolCall(_) | Part::Refusal { .. } => {}
                    }
                }
                out.push(json!({"role": "user", "content": content}));
            }
        }
    }
    let own = |t: &Turn| t.native.as_ref().is_some_and(|n| n.dialect == DIALECT && n.model == model);
    notes.extend(crate::provider::text_only_note(history, own));
    Ok(((!system.is_empty()).then_some(Value::Array(system)), out, notes))
}

/// The request body (without OpenRouter routing fields).
pub fn body(model: &str, system: Option<Value>, messages: Vec<Value>, plan: &Plan, budget_models: bool) -> Value {
    let max_tokens = plan.max_tokens.unwrap_or(FALLBACK_MAX_TOKENS);
    let mut b = json!({"model": model, "messages": messages, "max_tokens": max_tokens, "stream": true});
    if let Some(s) = system {
        b["system"] = s;
    }
    let display = has_display_option(model);
    let with_display = |mut t: Value| {
        if display {
            t["display"] = json!("summarized");
        }
        t
    };
    match &plan.reasoning {
        Reasoning::Effort(e) => {
            b["thinking"] = with_display(json!({"type": "adaptive"}));
            b["output_config"] = json!({"effort": e});
        }
        Reasoning::Budget(n) => b["thinking"] = with_display(json!({"type": "enabled", "budget_tokens": n})),
        Reasoning::On if budget_models => {
            // Budget-based thinking (older models): the largest budget below max_tokens.
            let n = max_tokens.saturating_sub(1).max(1024);
            b["thinking"] = with_display(json!({"type": "enabled", "budget_tokens": n}));
        }
        Reasoning::On => b["thinking"] = with_display(json!({"type": "adaptive"})),
        Reasoning::Off => b["thinking"] = json!({"type": "disabled"}),
        Reasoning::Unset => {}
    }
    let tools: Vec<Value> = plan
        .tools
        .iter()
        .filter_map(|t| match t {
            Tool::Search => Some(search_tool()),
            Tool::Fetch => Some(fetch_tool()),
            Tool::Code | Tool::Maps | Tool::Image => None, // rejected by validation
        })
        .collect();
    if !tools.is_empty() {
        b["tools"] = Value::Array(tools);
    }
    b
}

/// Streaming state for one Messages call.
#[derive(Debug, Default)]
pub struct EventParser {
    /// Content blocks by index (deltas applied).
    blocks: Vec<Value>,
    /// Partial JSON input of tool-use blocks, by index.
    partial_json: Vec<(usize, String)>,
    pub finished: bool,
    any_output: bool,
    input_usage: Option<Value>,
}

fn results_summary(block: &Value) -> Option<Value> {
    let content = block.get("content")?;
    match content.as_array() {
        Some(items) => {
            Some(Value::Array(items.iter().map(|r| json!({"title": r.get("title"), "url": r.get("url")})).collect()))
        }
        None => Some(json!({"type": content.get("type"), "url": content.get("url")})),
    }
}

impl EventParser {
    fn block_mut(&mut self, idx: usize) -> &mut Value {
        if self.blocks.len() <= idx {
            self.blocks.resize(idx + 1, Value::Null);
        }
        &mut self.blocks[idx]
    }

    pub fn event(&mut self, name: Option<&str>, v: &Value) -> Result<Vec<StreamEvent>, LlmError> {
        let mut evs = Vec::new();
        let ty = v.get("type").and_then(Value::as_str).or(name).unwrap_or("");
        let idx = v.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
        match ty {
            "message_start" => {
                let m = v.get("message");
                let id = m.and_then(|m| m.get("id")).and_then(Value::as_str).map(str::to_string);
                let model = m.and_then(|m| m.get("model")).and_then(Value::as_str).map(str::to_string);
                evs.push(StreamEvent::Meta { upstream_id: id, provider: None, model });
                self.input_usage = m.and_then(|m| m.get("usage")).cloned();
                if let Some(u) = &self.input_usage {
                    // Input counts are final already: show them while the answer streams.
                    let mut live = usage_from_anthropic(u);
                    live.output_tokens = None;
                    live.reasoning_tokens = None;
                    live.cost_usd = None;
                    live.cost_source = omni_core::usage::CostSource::Unknown;
                    evs.push(StreamEvent::LiveUsage(live));
                }
            }
            "content_block_start" => {
                let cb = v.get("content_block").cloned().unwrap_or(Value::Null);
                match cb.get("type").and_then(Value::as_str) {
                    Some("redacted_thinking") => {
                        evs.push(StreamEvent::OpaqueReasoning { kind: ReasoningKind::Redacted })
                    }
                    Some("server_tool_use") => evs.push(StreamEvent::Tool(ToolCall {
                        id: cb.get("id").and_then(Value::as_str).map(str::to_string),
                        name: cb.get("name").and_then(Value::as_str).unwrap_or("tool").to_string(),
                        status: Some("in_progress".into()),
                        input: None,
                        output: None,
                    })),
                    Some(t) if t.ends_with("_tool_result") => {
                        let name = t.trim_end_matches("_tool_result").to_string();
                        evs.push(StreamEvent::Tool(ToolCall {
                            id: cb.get("tool_use_id").and_then(Value::as_str).map(str::to_string),
                            name,
                            status: Some("completed".into()),
                            input: None,
                            output: results_summary(&cb),
                        }));
                    }
                    _ => {}
                }
                *self.block_mut(idx) = cb;
            }
            "content_block_delta" => {
                let d = v.get("delta").cloned().unwrap_or(Value::Null);
                match d.get("type").and_then(Value::as_str) {
                    Some("thinking_delta") => {
                        let t = d.get("thinking").and_then(Value::as_str).unwrap_or("").to_string();
                        if !t.is_empty() {
                            self.any_output = true;
                            let b = self.block_mut(idx);
                            let cur = b.get("thinking").and_then(Value::as_str).unwrap_or("").to_string();
                            b["thinking"] = json!(cur + &t);
                            evs.push(StreamEvent::Reasoning { kind: ReasoningKind::Text, text: t, block: idx });
                        }
                    }
                    Some("signature_delta") => {
                        let s = d.get("signature").and_then(Value::as_str).unwrap_or("").to_string();
                        let b = self.block_mut(idx);
                        let cur = b.get("signature").and_then(Value::as_str).unwrap_or("").to_string();
                        b["signature"] = json!(cur + &s);
                    }
                    Some("text_delta") => {
                        let t = d.get("text").and_then(Value::as_str).unwrap_or("").to_string();
                        if !t.is_empty() {
                            self.any_output = true;
                            let b = self.block_mut(idx);
                            let cur = b.get("text").and_then(Value::as_str).unwrap_or("").to_string();
                            b["text"] = json!(cur + &t);
                            evs.push(StreamEvent::Text(t));
                        }
                    }
                    Some("citations_delta") => {
                        if let Some(c) = d.get("citation") {
                            let b = self.block_mut(idx);
                            let mut list = b.get("citations").and_then(Value::as_array).cloned().unwrap_or_default();
                            list.push(c.clone());
                            b["citations"] = Value::Array(list);
                            evs.push(StreamEvent::Citations(vec![Citation {
                                url: c.get("url").and_then(Value::as_str).map(str::to_string),
                                title: c.get("title").and_then(Value::as_str).map(str::to_string),
                                cited_text: c.get("cited_text").and_then(Value::as_str).map(str::to_string),
                                range: None,
                                extra: None,
                            }]));
                        }
                    }
                    Some("input_json_delta") => {
                        let pj = d.get("partial_json").and_then(Value::as_str).unwrap_or("");
                        match self.partial_json.iter_mut().find(|(i, _)| *i == idx) {
                            Some((_, s)) => s.push_str(pj),
                            None => self.partial_json.push((idx, pj.to_string())),
                        }
                    }
                    _ => {}
                }
            }
            "content_block_stop" => {
                if let Some(pos) = self.partial_json.iter().position(|(i, _)| *i == idx) {
                    let (_, js) = self.partial_json.remove(pos);
                    let input: Value = serde_json::from_str(&js).unwrap_or(Value::String(js));
                    let b = self.block_mut(idx);
                    b["input"] = input.clone();
                    if b.get("type").and_then(Value::as_str) == Some("server_tool_use") {
                        evs.push(StreamEvent::Tool(ToolCall {
                            id: b.get("id").and_then(Value::as_str).map(str::to_string),
                            name: b.get("name").and_then(Value::as_str).unwrap_or("tool").to_string(),
                            status: Some("in_progress".into()),
                            input: Some(input),
                            output: None,
                        }));
                    }
                }
            }
            "message_delta" => {
                if let Some(u) = v.get("usage").filter(|u| u.is_object()) {
                    // input counts arrive in message_start; output (and OpenRouter's cost) here.
                    let mut merged = self.input_usage.clone().unwrap_or_else(|| json!({}));
                    if let (Some(m), Some(o)) = (merged.as_object_mut(), u.as_object()) {
                        // OpenRouter sends `null` for counts it does not repeat: keep message_start's.
                        for (k, val) in o.iter().filter(|(_, v)| !v.is_null()) {
                            m.insert(k.clone(), val.clone());
                        }
                    }
                    evs.push(StreamEvent::Usage(usage_from_anthropic(&merged)));
                }
                if let Some(d) = v.pointer("/delta/stop_details").filter(|d| !d.is_null()) {
                    evs.push(StreamEvent::FinishDetails(d.clone()));
                }
                if let Some(sr) = v.pointer("/delta/stop_reason").and_then(Value::as_str) {
                    let reason = match sr {
                        "end_turn" | "stop_sequence" | "tool_use" => "stop",
                        "max_tokens" | "model_context_window_exceeded" => "length",
                        "refusal" => "content_filter",
                        other => other,
                    };
                    evs.push(StreamEvent::Finish { reason: Some(reason.to_string()), native: Some(sr.to_string()) });
                }
            }
            "message_stop" => self.finished = true,
            "error" => {
                let err = v.get("error").cloned().unwrap_or_else(|| v.clone());
                let mut e = super::chat::error_from_json(&err, None, None, ErrorKind::InBand);
                e.details = Some(v.clone());
                return Err(e);
            }
            _ => {}
        }
        Ok(evs)
    }

    /// The assistant content blocks for replay.
    pub fn native_payload(&self) -> Value {
        let blocks: Vec<Value> = self.blocks.iter().filter(|b| !b.is_null()).cloned().collect();
        json!({"role": "assistant", "content": blocks})
    }
}

/// Usage from Anthropic's counters (+ OpenRouter's cost when present).
pub fn usage_from_anthropic(u: &Value) -> omni_core::Usage {
    let g = |k: &str| u.get(k).and_then(Value::as_u64);
    let cache_read = g("cache_read_input_tokens");
    let cache_write = g("cache_creation_input_tokens");
    let input = g("input_tokens").map(|i| i + cache_read.unwrap_or(0) + cache_write.unwrap_or(0));
    omni_core::Usage {
        input_tokens: input,
        output_tokens: g("output_tokens"),
        // Billed thinking (the full thinking, whatever `display` shows); only in the
        // final `message_delta` when streaming.
        reasoning_tokens: u.pointer("/output_tokens_details/thinking_tokens").and_then(Value::as_u64),
        cached_input_tokens: cache_read,
        cache_write_tokens: cache_write,
        cost_usd: u.get("cost").and_then(Value::as_f64),
        cost_source: if u.get("cost").is_some() {
            omni_core::usage::CostSource::Reported
        } else {
            omni_core::usage::CostSource::Unknown
        },
        unbilled: Vec::new(),
        raw: u.clone(),
    }
}

impl crate::chatstream::EventParser for EventParser {
    fn event(&mut self, name: Option<&str>, data: &Value) -> Result<Vec<StreamEvent>, LlmError> {
        EventParser::event(self, name, data)
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
    fn display_option_by_version() {
        assert!(has_display_option("anthropic/claude-opus-5.5"));
        assert!(has_display_option("anthropic/claude-opus-4.8"));
        assert!(!has_display_option("anthropic/claude-sonnet-4.6"));
    }

    /// Recorded 2026-11-11: Claude Fable 5.2 refused (Anthropic's cyber
    /// classifier); a "successful" stream with `stop_reason: refusal`.
    #[test]
    fn recorded_refusal_keeps_the_providers_details() {
        let mut p = EventParser::default();
        let mut evs = Vec::new();
        for e in crate::fixtures::sse("messages_refusal_fable52.sse") {
            evs.extend(p.event(None, &e).unwrap());
        }
        assert!(p.finished);
        let details = evs.iter().find_map(|e| if let StreamEvent::FinishDetails(d) = e { Some(d) } else { None });
        let d = details.expect("stop_details");
        assert_eq!((d["type"].as_str(), d["category"].as_str()), (Some("refusal"), Some("cyber")));
        assert!(d["explanation"].as_str().unwrap().contains("Usage Policy"));
        assert!(evs.iter().any(|e| matches!(e, StreamEvent::Finish { reason: Some(r), native: Some(n) } if r == "content_filter" && n == "refusal")));
        // The input count from message_start survives the nulls of message_delta.
        let u = evs.iter().find_map(|e| if let StreamEvent::Usage(u) = e { Some(u) } else { None }).unwrap();
        assert_eq!((u.input_tokens, u.output_tokens, u.cost_usd), (Some(25), Some(0), None));
    }

    #[test]
    fn parses_thinking_text_and_search() {
        let mut p = EventParser::default();
        let evs = [
            json!({"type": "message_start", "message": {"id": "gen-1", "model": "claude", "usage": {"input_tokens": 10, "cache_read_input_tokens": 5}}}),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "thinking", "thinking": ""}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": "hmm"}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "signature_delta", "signature": "sig"}}),
            json!({"type": "content_block_stop", "index": 0}),
            json!({"type": "content_block_start", "index": 1, "content_block": {"type": "server_tool_use", "id": "srv_1", "name": "web_search", "input": {}}}),
            json!({"type": "content_block_delta", "index": 1, "delta": {"type": "input_json_delta", "partial_json": "{\"query\":\"rust\"}"}}),
            json!({"type": "content_block_stop", "index": 1}),
            json!({"type": "content_block_start", "index": 2, "content_block": {"type": "web_search_tool_result", "tool_use_id": "srv_1", "content": [{"type": "web_search_result", "title": "Rust", "url": "https://rust-lang.org", "encrypted_content": "x"}]}}),
            json!({"type": "content_block_start", "index": 3, "content_block": {"type": "text", "text": ""}}),
            json!({"type": "content_block_delta", "index": 3, "delta": {"type": "text_delta", "text": "Rust is"}}),
            json!({"type": "content_block_delta", "index": 3, "delta": {"type": "citations_delta", "citation": {"type": "web_search_result_location", "url": "https://rust-lang.org", "title": "Rust", "cited_text": "Rust"}}}),
            json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 20, "output_tokens_details": {"thinking_tokens": 12}, "cost": 0.002}}),
            json!({"type": "message_stop"}),
        ];
        let mut acc = crate::Accumulator::new();
        for e in &evs {
            for x in p.event(None, e).unwrap() {
                acc.apply(x);
            }
        }
        assert!(p.finished);
        let parts = acc.parts();
        assert!(matches!(&parts[0], Part::Reasoning(r) if r.text.as_deref() == Some("hmm")));
        assert!(
            matches!(&parts[1], Part::ToolCall(t) if t.name == "web_search" && t.status.as_deref() == Some("completed") && t.input.is_some())
        );
        assert!(matches!(&parts[2], Part::Text { text, citations } if text == "Rust is" && citations.len() == 1));
        // Input tokens are shown as soon as the stream starts (display only).
        let start = EventParser::default().event(None, &evs[0]).unwrap();
        assert!(
            matches!(&start[1], StreamEvent::LiveUsage(u) if u.input_tokens == Some(15) && u.output_tokens.is_none() && u.cost_usd.is_none())
        );
        let u = acc.usage.clone().unwrap();
        assert_eq!((u.input_tokens, u.output_tokens, u.cost_usd), (Some(15), Some(20), Some(0.002)));
        assert_eq!(u.reasoning_tokens, Some(12));
        let native = p.native_payload();
        assert_eq!(native["content"][0]["signature"], "sig");
        assert_eq!(native["content"][1]["input"]["query"], "rust");
        assert_eq!(native["content"][3]["citations"][0]["url"], "https://rust-lang.org");
    }
}
