//! Google's Gemini API (Gemini Developer API, AI Studio keys) as a direct
//! provider: `generateContent` streaming,
//! `POST {base}/models/{model}:streamGenerateContent?alt=sse` with the key in
//! `x-goog-api-key`. Spec: docs/research/gemini.md §0–9 and the official pages
//! it links (fetched 2026-11-11/12).
//!
//! - [`models`]: model table (limits, thinking levels, tools, tiers, inputs, image options);
//! - `build`: options, defaults (the model's official thinking level, thought
//!   summaries, full output budget, highest media resolution) and the request body;
//! - [`convert`]: history → `contents` / `systemInstruction`, native replay;
//! - [`stream`]: the SSE chunk parser (thoughts, text, images, code
//!   execution, grounding, URL context, long-decode checkpoints, usage, blocks);
//! - [`pricing`]: dated price list and cost estimate;
//! - [`errors`]: classification of Google's error objects;
//! - [`omni`]: video generation (Gemini Omni Flash on the Interactions API).
//!
//! **Long decode** (Gemini 4): `maxOutputTokens` is the total of all hops; a
//! hop that must continue ends with `finishReason: CONTINUATION` and a
//! `continuationToken`. The next hop is the *identical* request plus that
//! token ([`continuation_request`]); the parts of all hops form the model turn
//! ([`merge_natives`]). Streams send a checkpoint token every ~5K tokens after
//! 32K: after a drop, the bot resumes from the latest one. Its item carries the
//! parts committed so far, which the resumed stream returns with its own (the
//! bot keeps only the natives of completed hops).
//!
//! **Video** is Gemini Omni Flash on the Interactions API ([`omni`]: bare
//! `gemini-omni-…` ids; Veo is shut down). The chat models do not use the
//! Interactions API (Google's recommended interface for new projects, with
//! server-side state, background runs and stream resumption by
//! `last_event_id`). Not used either: uploads to the Files API, explicit
//! caching, function calling.

mod build;
pub mod convert;
pub mod errors;
pub mod models;
pub mod omni;
pub mod pricing;
pub mod stream;
#[cfg(test)]
mod tests;

use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::error::LlmError;
use crate::provider::{DirectConfig, Provider};
use crate::request::Requested;
use crate::{BlobLoader, CallInput, CallOutcome, EventSink, Observer, Prepared};

pub const DIALECT: &str = "google.generate";
pub const NAME: &str = "Gemini API";

pub struct Google {
    cfg: DirectConfig,
    http: reqwest::Client,
}

impl Google {
    pub fn new(cfg: DirectConfig) -> anyhow::Result<Self> {
        let http = cfg.http()?;
        Ok(Self { cfg, http })
    }
}

#[async_trait::async_trait]
impl Provider for Google {
    fn id(&self) -> &'static str {
        "google"
    }
    fn name(&self) -> &'static str {
        NAME
    }
    fn knows(&self, model: &str) -> bool {
        models::known(model) || omni::spec(model).is_some()
    }
    async fn prepare(
        &self,
        input: &CallInput<'_>,
        presets: &[Requested],
        blobs: &dyn BlobLoader,
    ) -> Result<Prepared, LlmError> {
        if !self.knows(input.model) {
            return Err(crate::provider::not_in_table(self.name(), input.model));
        }
        if omni::is_video_model(input.model) {
            return omni::prepare(&self.cfg, input, presets, blobs).await;
        }
        build::prepare(&self.cfg, input, presets, blobs).await
    }
    async fn stream(
        &self,
        p: &Prepared,
        sink: &mut dyn EventSink,
        observer: &dyn Observer,
        cancel: &CancellationToken,
    ) -> (Result<Value, LlmError>, CallOutcome) {
        if p.dialect == omni::DIALECT {
            return omni::stream(&self.http, &self.cfg, p, sink, observer, cancel).await;
        }
        let carried = p.body.get("continuationToken").and_then(Value::as_str).and_then(carry::get).unwrap_or_default();
        let parser = stream::ChunkParser::new(&p.model, &p.body, build::now_unix(), carried);
        let rb = self.http.post(&p.url).header("x-goog-api-key", &self.cfg.api_key).json(&p.body);
        let crate::chatstream::Streamed { result, parser, header_id } = crate::chatstream::run_with(
            rb,
            &p.url,
            &p.body,
            self.cfg.idle_timeout,
            None,
            errors::error_from_json,
            parser,
            sink,
            observer,
            cancel,
        )
        .await;
        let outcome = CallOutcome {
            upstream_id: header_id.or_else(|| parser.response_id().map(str::to_string)),
            continuation: parser.continuation().cloned(),
            ..Default::default()
        };
        // A stream Google stopped (blocked prompt, content filter, error finish) fails
        // with Google's own words (`finishMessage`), after its usage was billed.
        let result = match (result, parser.abnormal()) {
            (Ok(()), Some(e)) => Err(e.clone()),
            (r, _) => r,
        };
        (result.map(|()| parser.native_payload()), outcome)
    }
}

/// The request that resumes a long-decode answer from a continuation item
/// (see [`crate::Prepared::with_continuation`]): the identical request (same
/// `contents`, `systemInstruction`, `tools`, `generationConfig` and
/// `maxOutputTokens`, the total of all hops) plus `continuationToken`. A
/// checkpoint item's committed parts are handed to the resumed stream.
pub fn continuation_request(p: &Prepared, item: &Value) -> Prepared {
    let mut q = p.clone();
    if let Some(token) = item.get("continuationToken").and_then(Value::as_str) {
        q.body["continuationToken"] = json!(token);
        if let Some(parts) = item.get("parts").and_then(Value::as_array).filter(|a| !a.is_empty()) {
            carry::put(token, parts.clone());
        }
    }
    q
}

/// One model turn for replay from the payloads of several long-decode hops:
/// their parts in order (Google: "combine the parts from all hops into one
/// model turn"). `full_text` is the whole answer as shown: text a hop lost
/// (resumed after a restart, when its committed parts were not handed over)
/// is put back as plain text parts where it is missing.
pub fn merge_natives(natives: &[Value], full_text: &str) -> Value {
    let mut parts: Vec<Value> =
        natives.iter().filter_map(|n| n.get("parts").and_then(Value::as_array)).flatten().cloned().collect();
    if let Some(fixed) = fill_text_gaps(&parts, full_text) {
        parts = fixed;
    }
    json!({"role": "model", "parts": parts})
}

fn answer_text(p: &Value) -> Option<&str> {
    (p.get("thought").and_then(Value::as_bool) != Some(true)).then(|| p.get("text").and_then(Value::as_str)).flatten()
}

/// `parts` with the text of `full_text` they lack inserted as plain parts, or
/// `None` if nothing is missing or the parts do not align with the text.
fn fill_text_gaps(parts: &[Value], full_text: &str) -> Option<Vec<Value>> {
    let joined: String = parts.iter().filter_map(answer_text).collect();
    if joined == full_text {
        return None;
    }
    let mut out = Vec::with_capacity(parts.len() + 2);
    let mut pos = 0;
    for p in parts {
        if let Some(t) = answer_text(p).filter(|t| !t.is_empty()) {
            let at = full_text[pos..].find(t)?;
            if at > 0 {
                out.push(json!({ "text": &full_text[pos..pos + at] }));
            }
            pos += at + t.len();
        }
        out.push(p.clone());
    }
    if pos < full_text.len() {
        out.push(json!({ "text": &full_text[pos..] }));
    }
    Some(out)
}

/// Parts committed before a checkpoint by a stream that then dropped, keyed by
/// the checkpoint's token: [`continuation_request`] puts them here, the resumed
/// [`Google::stream`] returns them first in its native payload. (Process-local:
/// after a restart the item is re-read from the checkpoint and put here again.)
mod carry {
    use std::hash::{DefaultHasher, Hash, Hasher};
    use std::sync::Mutex;

    use serde_json::Value;

    const KEEP: usize = 16;
    static CARRY: Mutex<Vec<(u64, Vec<Value>)>> = Mutex::new(Vec::new());

    fn key(token: &str) -> u64 {
        let mut h = DefaultHasher::new();
        token.hash(&mut h);
        h.finish()
    }

    pub fn put(token: &str, parts: Vec<Value>) {
        let k = key(token);
        let mut c = CARRY.lock().unwrap_or_else(|e| e.into_inner());
        c.retain(|(x, _)| *x != k);
        c.push((k, parts));
        let n = c.len();
        if n > KEEP {
            c.drain(..n - KEEP);
        }
    }

    pub fn get(token: &str) -> Option<Vec<Value>> {
        let k = key(token);
        CARRY.lock().unwrap_or_else(|e| e.into_inner()).iter().find(|(x, _)| *x == k).map(|(_, p)| p.clone())
    }
}
