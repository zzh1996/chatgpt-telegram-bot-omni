//! OpenRouter Chat Completions dialect (`openrouter.chat`): history conversion
//! and stream chunk parsing.

use base64::Engine as _;
use omni_core::content::{Citation, Part, ReasoningKind, Role, Turn};
use serde_json::{Map, Value, json};

use crate::error::{ErrorKind, LlmError, Retry, retry_for_status};
use crate::event::StreamEvent;
use crate::{BlobLoader, MediaClass, classify_mime};

use super::catalog::ModelInfo;

pub const DIALECT: &str = "openrouter.chat";

/// `id` of Gemini long-decode continuation items in `reasoning_details`.
pub const CONTINUATION_ID: &str = "gemini-continuation";

/// Notes produced while converting history (shown to the user for transparency).
#[derive(Debug, Default, Clone, PartialEq)]
pub struct ConvertNotes {
    pub notes: Vec<String>,
    pub needs_file_parser: bool,
}

fn data_url(mime: &str, bytes: &[u8]) -> String {
    format!("data:{mime};base64,{}", base64::engine::general_purpose::STANDARD.encode(bytes))
}

fn audio_format(mime: &str, filename: Option<&str>) -> String {
    let ext = filename.and_then(|f| f.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()));
    match mime {
        "audio/mpeg" | "audio/mp3" => "mp3".into(),
        "audio/wav" | "audio/x-wav" | "audio/wave" => "wav".into(),
        "audio/ogg" | "audio/opus" => "ogg".into(),
        "audio/flac" | "audio/x-flac" => "flac".into(),
        "audio/aac" => "aac".into(),
        "audio/mp4" | "audio/m4a" | "audio/x-m4a" => "m4a".into(),
        "audio/aiff" | "audio/x-aiff" => "aiff".into(),
        _ => ext.unwrap_or_else(|| mime.trim_start_matches("audio/").to_string()),
    }
}

/// Provider-specific details of an OpenAI-compatible Chat Completions API.
pub struct Flavor {
    /// Dialect tag stored with native outputs (replay only within the same dialect and model).
    pub dialect: &'static str,
    /// `image_url.detail` sent with images (the highest fidelity the API offers),
    /// or `None` where the vendor has no such parameter.
    pub image_detail: Option<&'static str>,
    /// Build the replayed assistant message from a stored native payload.
    pub replay: fn(&Value) -> Value,
}

fn openrouter_replay(p: &Value) -> Value {
    let mut msg = Map::new();
    msg.insert("role".into(), json!("assistant"));
    msg.insert("content".into(), p.get("content").cloned().unwrap_or(json!("")));
    if let Some(rd) = p.get("reasoning_details").filter(|v| v.as_array().is_some_and(|a| !a.is_empty())) {
        msg.insert("reasoning_details".into(), rd.clone());
    }
    Value::Object(msg)
}

/// Chat Completions on OpenRouter serves Gemini, DeepSeek, GLM, Kimi, Qwen…:
/// none of them has an image `detail` parameter, so it is not sent.
pub const OPENROUTER: Flavor = Flavor { dialect: DIALECT, image_detail: None, replay: openrouter_replay };

/// The Chat Completions request body (without OpenRouter routing fields).
pub fn body(model: &str, messages: Vec<Value>, plan: &super::options::Plan) -> Value {
    use super::options::Reasoning;
    let mut b = json!({"model": model, "messages": messages, "stream": true});
    match &plan.reasoning {
        Reasoning::Effort(e) => b["reasoning"] = json!({"effort": e}),
        Reasoning::Budget(n) => b["reasoning"] = json!({"max_tokens": n}),
        Reasoning::On => b["reasoning"] = json!({"enabled": true}),
        Reasoning::Off => b["reasoning"] = json!({"enabled": false}),
        Reasoning::Unset => {}
    }
    if let Some(n) = plan.max_tokens {
        b["max_tokens"] = json!(n);
    }
    if let Some(v) = &plan.verbosity {
        b["verbosity"] = json!(v);
    }
    let tools: Vec<Value> = plan
        .tools
        .iter()
        .filter_map(|t| match t {
            crate::request::Tool::Search => Some(super::options::search_tool()),
            crate::request::Tool::Fetch => Some(super::options::fetch_tool()),
            crate::request::Tool::Code | crate::request::Tool::Maps => None, // rejected by validation on this skin
        })
        .collect();
    if !tools.is_empty() {
        b["tools"] = Value::Array(tools);
    }
    if plan.image_output {
        b["modalities"] = json!(["image", "text"]);
        let mut ic = serde_json::Map::new();
        if let Some(s) = &plan.image_size {
            ic.insert("image_size".into(), json!(s));
        }
        if let Some(a) = &plan.aspect_ratio {
            ic.insert("aspect_ratio".into(), json!(a));
        }
        if !ic.is_empty() {
            b["image_config"] = Value::Object(ic);
        }
    }
    b
}

/// Convert the conversation to OpenRouter Chat Completions `messages`.
pub async fn convert_history(
    history: &[Turn],
    target_model: &str,
    info: Option<&ModelInfo>,
    blobs: &dyn BlobLoader,
) -> Result<(Vec<Value>, ConvertNotes), LlmError> {
    convert_history_with(history, target_model, info, blobs, &OPENROUTER).await
}

/// Convert the conversation to Chat Completions `messages` for `flavor`.
/// `info` (OpenRouter catalog) enables input-modality checks.
pub async fn convert_history_with(
    history: &[Turn],
    target_model: &str,
    info: Option<&ModelInfo>,
    blobs: &dyn BlobLoader,
    flavor: &Flavor,
) -> Result<(Vec<Value>, ConvertNotes), LlmError> {
    let mut notes = ConvertNotes::default();
    let mut out = Vec::with_capacity(history.len());
    for turn in history {
        match turn.role {
            Role::System => {
                out.push(json!({"role": "system", "content": turn.answer_text()}));
            }
            Role::Assistant => {
                // Exact replay when produced by this dialect and the same model.
                if let Some(n) = &turn.native
                    && n.dialect == flavor.dialect
                    && n.model == target_model
                {
                    out.push((flavor.replay)(&n.payload));
                    continue;
                }
                out.push(json!({"role": "assistant", "content": turn.answer_text()}));
            }
            Role::User => {
                let mut content: Vec<Value> = Vec::new();
                for part in &turn.parts {
                    match part {
                        Part::Text { text, .. } => {
                            if !text.is_empty() {
                                content.push(json!({"type": "text", "text": text}));
                            }
                        }
                        Part::Image { blob, .. } => {
                            if let Some(i) = info
                                && !i.accepts("image")
                            {
                                return Err(LlmError::request(format!(
                                    "{target_model} does not accept image input (input modalities: {})",
                                    i.architecture.input_modalities.join(", ")
                                )));
                            }
                            let bytes = load(blobs, blob).await?;
                            content.push(image_part(data_url(&blob.mime, &bytes), flavor.image_detail));
                        }
                        Part::File { blob, filename, .. } => {
                            let bytes = load(blobs, blob).await?;
                            match classify_mime(&blob.mime, filename.as_deref()) {
                                MediaClass::Pdf | MediaClass::Document => {
                                    if let Some(i) = info
                                        && !i.accepts("file")
                                    {
                                        return Err(LlmError::request(format!(
                                            "{target_model} has no native file input on OpenRouter (input modalities: {}); \
                                                 refusing to let OpenRouter OCR/parse it",
                                            i.architecture.input_modalities.join(", ")
                                        )));
                                    }
                                    notes.needs_file_parser = true;
                                    content.push(json!({"type": "file", "file": {
                                        "filename": filename.clone().unwrap_or_else(|| "file.pdf".into()),
                                        "file_data": data_url(&blob.mime, &bytes),
                                    }}));
                                }
                                MediaClass::Audio => {
                                    if let Some(i) = info
                                        && !i.accepts("audio")
                                    {
                                        return Err(LlmError::request(format!(
                                            "{target_model} does not accept audio input (input modalities: {})",
                                            i.architecture.input_modalities.join(", ")
                                        )));
                                    }
                                    content.push(json!({"type": "input_audio", "input_audio": {
                                        "data": base64::engine::general_purpose::STANDARD.encode(&bytes),
                                        "format": audio_format(&blob.mime, filename.as_deref()),
                                    }}));
                                }
                                MediaClass::Video => {
                                    if let Some(i) = info
                                        && !i.accepts("video")
                                    {
                                        return Err(LlmError::request(format!(
                                            "{target_model} does not accept video input (input modalities: {})",
                                            i.architecture.input_modalities.join(", ")
                                        )));
                                    }
                                    content.push(json!({"type": "video_url", "video_url": {"url": data_url(&blob.mime, &bytes)}}));
                                }
                                MediaClass::Image => {
                                    content.push(image_part(data_url(&blob.mime, &bytes), flavor.image_detail));
                                }
                                MediaClass::Text => {
                                    // No native text-document input on Chat Completions: inline verbatim as a separate part.
                                    let text = String::from_utf8(bytes).map_err(|_| {
                                        LlmError::request(format!(
                                            "{} is not valid UTF-8 text",
                                            filename.as_deref().unwrap_or("file")
                                        ))
                                    })?;
                                    notes.notes.push(format!(
                                        "📎 {} inlined as a separate text part by the bot (no native text-file input on this API)",
                                        filename.as_deref().unwrap_or("file")
                                    ));
                                    content.push(json!({"type": "text", "text": text}));
                                }
                                MediaClass::Other => {
                                    return Err(LlmError::request(format!(
                                        "unsupported file type {} ({})",
                                        blob.mime,
                                        filename.as_deref().unwrap_or("file")
                                    )));
                                }
                            }
                        }
                        Part::RemoteMedia { url, .. } => {
                            content.push(json!({"type": "video_url", "video_url": {"url": url}}));
                        }
                        Part::Location { .. } => {
                            return Err(LlmError::request(if flavor.dialect == DIALECT {
                                super::NO_LOCATION.to_string()
                            } else {
                                format!(
                                    "{target_model} does not take a location (only Gemini's Google Maps grounding does)"
                                )
                            }));
                        }
                        // Reasoning/tool parts never appear in user turns.
                        Part::Reasoning(_) | Part::ToolCall(_) | Part::Refusal { .. } => {}
                    }
                }
                // A single plain text part is sent as a string (most compatible).
                if content.len() == 1 && content[0]["type"] == "text" {
                    out.push(json!({"role": "user", "content": content[0]["text"].clone()}));
                } else {
                    out.push(json!({"role": "user", "content": content}));
                }
            }
        }
    }
    Ok((out, notes))
}

fn image_part(url: String, detail: Option<&str>) -> Value {
    match detail {
        Some(d) => json!({"type": "image_url", "image_url": {"url": url, "detail": d}}),
        None => json!({"type": "image_url", "image_url": {"url": url}}),
    }
}

async fn load(blobs: &dyn BlobLoader, r: &omni_core::BlobRef) -> Result<Vec<u8>, LlmError> {
    blobs.load(r).await.map_err(|e| LlmError::request(format!("cannot load {r}: {e:#}")))
}

/// Streaming state for one Chat Completions call.
#[derive(Debug, Default)]
pub struct ChunkParser {
    content: String,
    refusal: String,
    reasoning_text: String,
    /// Merged `reasoning_details`, keyed by position in this vector.
    details: Vec<Value>,
    images: Vec<Value>,
    annotations: Vec<Value>,
    pub finished: bool,
    pub saw_error: bool,
    pub any_output: bool,
    pub upstream_body_echo: Vec<Value>,
    pub router_metadata: Option<Value>,
    /// Latest long-decode continuation item (`id: "gemini-continuation"`).
    continuation: Option<Value>,
    /// Encrypted original reasoning (Volcengine Ark summary-mode models), kept whole.
    encrypted_content: Option<String>,
    /// Maps in-band error objects (default: OpenRouter's classification).
    pub map_error: Option<ErrorMapper>,
}

/// Builds an [`LlmError`] from an API error object: `(error, http status, retry-after, kind)`.
pub type ErrorMapper = fn(&Value, Option<u16>, Option<u64>, ErrorKind) -> LlmError;

impl ChunkParser {
    /// Parse one `data:` JSON chunk. Returns events or an in-band error.
    pub fn chunk(&mut self, v: &Value) -> Result<Vec<StreamEvent>, LlmError> {
        let mut evs = Vec::new();
        if let Some(d) = v.get("debug")
            && let Some(b) = d.get("echo_upstream_body")
        {
            self.upstream_body_echo.push(b.clone());
        }
        if let Some(m) = v.get("openrouter_metadata") {
            self.router_metadata = Some(m.clone());
        }
        let id = v.get("id").and_then(Value::as_str).map(str::to_string);
        let provider = v.get("provider").and_then(Value::as_str).map(str::to_string);
        let model = v.get("model").and_then(Value::as_str).map(str::to_string);
        if id.is_some() || provider.is_some() || model.is_some() {
            evs.push(StreamEvent::Meta { upstream_id: id, provider, model });
        }
        let map_error = self.map_error.unwrap_or(error_from_json);
        if let Some(err) = v.get("error") {
            self.saw_error = true;
            let mut e = map_error(err, None, None, ErrorKind::InBand);
            e.partial_output = self.any_output;
            e.details = Some(v.clone());
            return Err(e);
        }
        if let Some(choice) = v.get("choices").and_then(Value::as_array).and_then(|a| a.first()) {
            let delta = choice.get("delta").or_else(|| choice.get("message"));
            if let Some(delta) = delta {
                self.delta(delta, &mut evs);
            }
            if let Some(err) = choice.get("error") {
                self.saw_error = true;
                let mut e = map_error(err, None, None, ErrorKind::InBand);
                e.partial_output = self.any_output;
                e.details = Some(v.clone());
                return Err(e);
            }
            let fr = choice.get("finish_reason").and_then(Value::as_str).map(str::to_string);
            let nfr = choice.get("native_finish_reason").and_then(Value::as_str).map(str::to_string);
            if matches!(fr.as_deref(), Some("content_filter" | "error")) {
                // The provider's account of the stop, verbatim (without the content itself).
                let mut c = choice.clone();
                if let Some(o) = c.as_object_mut() {
                    for k in ["delta", "message", "logprobs"] {
                        o.remove(k);
                    }
                }
                evs.push(StreamEvent::FinishDetails(c));
            }
            if fr.is_some() || nfr.is_some() {
                self.finished = true;
                evs.push(StreamEvent::Finish { reason: fr, native: nfr });
            }
        }
        if let Some(u) = v.get("usage").filter(|u| u.is_object()) {
            evs.push(StreamEvent::Usage(usage_from_json(u)));
        }
        Ok(evs)
    }

    fn delta(&mut self, d: &Value, evs: &mut Vec<StreamEvent>) {
        let details = d.get("reasoning_details").and_then(Value::as_array);
        if let Some(arr) = details {
            for item in arr {
                if let Some(ev) = self.merge_detail(item) {
                    evs.push(ev);
                }
            }
        } else if let Some(r) = d.get("reasoning").or_else(|| d.get("reasoning_content")).and_then(Value::as_str)
            && !r.is_empty()
        {
            self.reasoning_text.push_str(r);
            self.any_output = true;
            evs.push(StreamEvent::Reasoning { kind: ReasoningKind::Text, text: r.to_string(), block: 0 });
        }
        if let Some(e) = d.get("encrypted_content").and_then(Value::as_str).filter(|s| !s.is_empty()) {
            // Sent whole in one dedicated chunk; replace rather than concatenate.
            self.encrypted_content = Some(e.to_string());
            evs.push(StreamEvent::OpaqueReasoning { kind: ReasoningKind::Encrypted });
        }
        if let Some(c) = d.get("content") {
            match c {
                Value::String(s) if !s.is_empty() => {
                    self.content.push_str(s);
                    self.any_output = true;
                    evs.push(StreamEvent::Text(s.clone()));
                }
                Value::Array(parts) => {
                    for p in parts {
                        if let Some(t) = p.get("text").and_then(Value::as_str) {
                            self.content.push_str(t);
                            self.any_output = true;
                            evs.push(StreamEvent::Text(t.to_string()));
                        }
                    }
                }
                _ => {}
            }
        }
        if let Some(r) = d.get("refusal").and_then(Value::as_str)
            && !r.is_empty()
        {
            self.refusal.push_str(r);
            self.any_output = true;
            evs.push(StreamEvent::Refusal(r.to_string()));
        }
        if let Some(imgs) = d.get("images").and_then(Value::as_array) {
            for img in imgs {
                self.images.push(img.clone());
                if let Some(url) = img.pointer("/image_url/url").and_then(Value::as_str)
                    && let Some((mime, bytes)) = decode_data_url(url)
                {
                    self.any_output = true;
                    evs.push(StreamEvent::Image { data: bytes, mime });
                }
            }
        }
        if let Some(anns) = d.get("annotations").and_then(Value::as_array) {
            let mut cites = Vec::new();
            for a in anns {
                self.annotations.push(a.clone());
                if a.get("type").and_then(Value::as_str) == Some("url_citation") {
                    let c = a.get("url_citation").unwrap_or(a);
                    cites.push(Citation {
                        url: c.get("url").and_then(Value::as_str).map(str::to_string),
                        title: c.get("title").and_then(Value::as_str).map(str::to_string),
                        cited_text: c.get("content").and_then(Value::as_str).map(str::to_string),
                        range: match (
                            c.get("start_index").and_then(Value::as_u64),
                            c.get("end_index").and_then(Value::as_u64),
                        ) {
                            (Some(a), Some(b)) => Some((a as usize, b as usize)),
                            _ => None,
                        },
                        extra: None,
                    });
                } else if a.get("type").and_then(Value::as_str) == Some("file") {
                    let name = a.pointer("/file/name").and_then(Value::as_str).unwrap_or("file");
                    evs.push(StreamEvent::Notice(format!(
                        "⚠️ OpenRouter parsed {name} itself (file-parser annotation present) — the model did not read the original file"
                    )));
                }
            }
            if !cites.is_empty() {
                evs.push(StreamEvent::Citations(cites));
            }
        }
    }

    /// Merge one streamed `reasoning_details` fragment. Fragments with the same
    /// `index` belong to the same block; string payloads are concatenated.
    fn merge_detail(&mut self, item: &Value) -> Option<StreamEvent> {
        // Continuation items are complete, opaque and replace each other; never merge them.
        if item.get("id").and_then(Value::as_str) == Some(CONTINUATION_ID) {
            let mut it = item.clone();
            if let Some(o) = it.as_object_mut() {
                o.remove("index");
            }
            self.continuation = Some(it.clone());
            return Some(StreamEvent::Checkpoint(it));
        }
        let ty = item.get("type").and_then(Value::as_str).unwrap_or("").to_string();
        let index = item.get("index").and_then(Value::as_u64);
        let pos = index.and_then(|i| {
            self.details.iter().position(|d| {
                d.get("index").and_then(Value::as_u64) == Some(i)
                    && d.get("type").and_then(Value::as_str) == Some(ty.as_str())
            })
        });
        let pos = match pos {
            Some(p) => {
                let existing = self.details[p].as_object_mut().unwrap();
                for (k, v) in item.as_object().into_iter().flatten() {
                    match (k.as_str(), v) {
                        ("text" | "summary" | "data", Value::String(s)) => {
                            let cur = existing.entry(k.clone()).or_insert(json!(""));
                            if let Value::String(c) = cur {
                                c.push_str(s);
                            } else {
                                *cur = v.clone();
                            }
                        }
                        (_, Value::Null) => {}
                        _ => {
                            existing.insert(k.clone(), v.clone());
                        }
                    }
                }
                p
            }
            None => {
                self.details.push(item.clone());
                self.details.len() - 1
            }
        };
        let block = pos;
        match ty.as_str() {
            "reasoning.text" => {
                let t = item.get("text").and_then(Value::as_str).unwrap_or("");
                if t.is_empty() {
                    return None;
                }
                self.any_output = true;
                Some(StreamEvent::Reasoning { kind: ReasoningKind::Text, text: t.to_string(), block })
            }
            "reasoning.summary" => {
                let t = item.get("summary").and_then(Value::as_str).unwrap_or("");
                if t.is_empty() {
                    return None;
                }
                self.any_output = true;
                Some(StreamEvent::Reasoning { kind: ReasoningKind::Summary, text: t.to_string(), block })
            }
            "reasoning.encrypted" => Some(StreamEvent::OpaqueReasoning { kind: ReasoningKind::Encrypted }),
            _ => None,
        }
    }

    /// Latest continuation item seen in this stream, if any.
    pub fn continuation(&self) -> Option<&Value> {
        self.continuation.as_ref()
    }

    /// The assistant message as it should be replayed to the same model
    /// (continuation items are excluded: they only matter for resuming).
    pub fn native_payload(&self) -> Value {
        let mut m = json!({"role": "assistant", "content": self.content});
        if !self.reasoning_text.is_empty() {
            m["reasoning"] = json!(self.reasoning_text);
        }
        if !self.details.is_empty() {
            m["reasoning_details"] = Value::Array(self.details.clone());
        }
        if !self.images.is_empty() {
            m["images"] = Value::Array(self.images.clone());
        }
        if !self.annotations.is_empty() {
            m["annotations"] = Value::Array(self.annotations.clone());
        }
        if !self.refusal.is_empty() {
            m["refusal"] = json!(self.refusal);
        }
        if let Some(e) = &self.encrypted_content {
            m["encrypted_content"] = json!(e);
        }
        m
    }

    pub fn any_output(&self) -> bool {
        self.any_output
    }
}

pub fn decode_data_url(url: &str) -> Option<(String, Vec<u8>)> {
    let rest = url.strip_prefix("data:")?;
    let (meta, data) = rest.split_once(',')?;
    let mime = meta.split(';').next().unwrap_or("application/octet-stream").to_string();
    if !meta.ends_with(";base64") {
        return None;
    }
    let bytes = base64::engine::general_purpose::STANDARD.decode(data.trim()).ok()?;
    Some((mime, bytes))
}

pub fn usage_from_json(u: &Value) -> omni_core::Usage {
    let g = |p: &str| u.pointer(p).and_then(Value::as_u64);
    omni_core::Usage {
        input_tokens: g("/prompt_tokens").or_else(|| g("/input_tokens")),
        output_tokens: g("/completion_tokens").or_else(|| g("/output_tokens")),
        reasoning_tokens: g("/completion_tokens_details/reasoning_tokens")
            .or_else(|| g("/output_tokens_details/reasoning_tokens")),
        cached_input_tokens: g("/prompt_tokens_details/cached_tokens")
            .or_else(|| g("/input_tokens_details/cached_tokens")),
        cache_write_tokens: g("/prompt_tokens_details/cache_write_tokens")
            .or_else(|| g("/input_tokens_details/cache_write_tokens")),
        cost_usd: u.get("cost").and_then(Value::as_f64),
        cost_source: if u.get("cost").is_some() {
            omni_core::usage::CostSource::Reported
        } else {
            omni_core::usage::CostSource::Unknown
        },
        raw: u.clone(),
    }
}

/// Build an [`LlmError`] from an OpenRouter error object (`{code, message, metadata}`).
pub fn error_from_json(err: &Value, http_status: Option<u16>, retry_after: Option<u64>, kind: ErrorKind) -> LlmError {
    let code =
        err.get("code").and_then(|c| c.as_u64().or_else(|| c.as_str().and_then(|s| s.parse().ok()))).map(|c| c as u16);
    let status = http_status.or(code);
    let message = err.get("message").and_then(Value::as_str).unwrap_or("(no message)").to_string();
    let error_type = err
        .pointer("/metadata/error_type")
        .or_else(|| err.get("error_type"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let limit_source = err.pointer("/metadata/limit_source").and_then(Value::as_str);
    let lower = message.to_ascii_lowercase();
    let routing_dead_end = lower.contains("no endpoints found")
        || lower.contains("no allowed providers")
        || lower.contains("no providers available");
    let retry = match error_type.as_deref() {
        _ if routing_dead_end => Retry::Never,
        Some("rate_limit_exceeded") => Retry::After(retry_after.unwrap_or(20)),
        Some("server" | "unmapped" | "provider_unavailable" | "provider_overloaded" | "timeout") => match retry_after {
            Some(s) => Retry::After(s),
            None => Retry::Backoff,
        },
        Some("payment_required") if limit_source == Some("openrouter_in_flight_budget") => {
            Retry::After(retry_after.unwrap_or(30))
        }
        Some(_) => Retry::Never,
        None => status.map(|s| retry_for_status(s, retry_after)).unwrap_or(Retry::Backoff),
    };
    LlmError { kind, retry, status, error_type, message, details: Some(err.clone()), partial_output: false }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_stream_and_merges_details() {
        let mut p = ChunkParser::default();
        let chunks = [
            json!({"id": "gen-1", "provider": "Anthropic", "model": "anthropic/claude", "choices": [{"index": 0, "delta": {"role": "assistant", "content": "", "reasoning": "Let ", "reasoning_details": [{"type": "reasoning.text", "text": "Let ", "index": 0, "format": "anthropic-claude-v1"}]}}]}),
            json!({"id": "gen-1", "choices": [{"index": 0, "delta": {"reasoning": "me", "reasoning_details": [{"type": "reasoning.text", "text": "me", "index": 0}]}}]}),
            json!({"id": "gen-1", "choices": [{"index": 0, "delta": {"reasoning_details": [{"type": "reasoning.text", "text": "", "signature": "SIG", "index": 0}]}}]}),
            json!({"id": "gen-1", "choices": [{"index": 0, "delta": {"content": "Hi"}}]}),
            json!({"id": "gen-1", "choices": [{"index": 0, "delta": {"content": ""}, "finish_reason": "stop", "native_finish_reason": "end_turn"}], "usage": {"prompt_tokens": 5, "completion_tokens": 7, "cost": 0.01, "completion_tokens_details": {"reasoning_tokens": 3}}}),
        ];
        let mut all = Vec::new();
        for c in &chunks {
            all.extend(p.chunk(c).unwrap());
        }
        let native = p.native_payload();
        assert_eq!(native["content"], "Hi");
        assert_eq!(
            native["reasoning_details"],
            json!([{"type": "reasoning.text", "text": "Let me", "index": 0, "format": "anthropic-claude-v1", "signature": "SIG"}])
        );
        assert!(p.finished);
        assert!(
            all.iter().any(
                |e| matches!(e, StreamEvent::Usage(u) if u.cost_usd == Some(0.01) && u.reasoning_tokens == Some(3))
            )
        );
        let texts: String = all
            .iter()
            .filter_map(|e| if let StreamEvent::Reasoning { text, .. } = e { Some(text.as_str()) } else { None })
            .collect();
        assert_eq!(texts, "Let me"); // `reasoning` string not double counted
    }

    #[test]
    fn in_band_error_classification() {
        let mut p = ChunkParser::default();
        let e = p
            .chunk(&json!({"id": "gen-x", "error": {"code": 429, "message": "Rate limit exceeded", "metadata": {"error_type": "rate_limit_exceeded"}}, "choices": [{"index": 0, "delta": {"content": ""}, "finish_reason": "error"}]}))
            .unwrap_err();
        assert_eq!(e.kind, ErrorKind::InBand);
        assert_eq!(e.retry, Retry::After(20));
        let e = error_from_json(
            &json!({"code": 400, "message": "context too long", "metadata": {"error_type": "context_length_exceeded"}}),
            Some(400),
            None,
            ErrorKind::Http,
        );
        assert_eq!(e.retry, Retry::Never);
        let e = error_from_json(
            &json!({"code": 404, "message": "No endpoints found that support all the parameters"}),
            Some(404),
            None,
            ErrorKind::Http,
        );
        assert_eq!(e.retry, Retry::Never);
        let e = error_from_json(&json!({"code": 502, "message": "bad gateway"}), Some(502), None, ErrorKind::Http);
        assert_eq!(e.retry, Retry::Backoff);
    }

    #[test]
    fn images_and_citations() {
        let mut p = ChunkParser::default();
        let png = base64::engine::general_purpose::STANDARD.encode(b"\x89PNG....");
        let evs = p
            .chunk(&json!({"choices": [{"index": 0, "delta": {
                "images": [{"type": "image_url", "image_url": {"url": format!("data:image/png;base64,{png}")}}],
                "annotations": [{"type": "url_citation", "url_citation": {"url": "https://a", "title": "A", "start_index": 0, "end_index": 3}}]
            }}]}))
            .unwrap();
        assert!(evs.iter().any(|e| matches!(e, StreamEvent::Image { mime, .. } if mime == "image/png")));
        assert!(evs.iter().any(|e| matches!(e, StreamEvent::Citations(c) if c[0].url.as_deref() == Some("https://a"))));
    }
}
