//! What each upstream call reports about itself besides the content: the HTTP
//! status, the response headers worth keeping (request ids, rate limits,
//! timing, serving), the response envelope (ids, the model and tier that
//! served it, finish details, usage, echoed settings) and timings.
//!
//! [`Collector`] gathers it from the raw traffic ([`Observer`]), the same way
//! for every provider: the envelope's shape is the API family's (Responses,
//! Messages, Gemini `generateContent`, Chat Completions), so no provider has
//! to parse it. The bot stores the calls per attempt for `/info`, and
//! [`Collector::served`] says what the reply's footer should add when the
//! serving differs from the request (another model, another tier).

use std::time::Instant;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::Observer;

/// One upstream call.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CallMeta {
    /// The endpoint, without the query string.
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    /// Response headers worth keeping, never account identifiers (organization,
    /// project, user ids, cookies): `/info` is visible to the whole chat.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub headers: Vec<(String, String)>,
    /// The response object without its content, merged over the stream.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub response: Map<String, Value>,
    /// Milliseconds from sending the request to the response headers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ms_headers: Option<u64>,
    /// Milliseconds from sending the request to the first and the last event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ms_first_event: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ms_last_event: Option<u64>,
    #[serde(default)]
    pub events: u64,
}

impl CallMeta {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    /// The request id support asks for (from the headers; else the response id).
    pub fn request_id(&self) -> Option<&str> {
        const IDS: [&str; 6] =
            ["x-request-id", "request-id", "x-ds-trace-id", "msh-request-id", "x-log-id", "x-goog-request-id"];
        IDS.iter().find_map(|h| self.header(h))
    }
}

/// How a reply was served, where it differs from the request.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Served {
    /// The model that answered, when it is not the requested one (a dated
    /// snapshot or the "latest" version of the requested id is the same model).
    pub model: Option<String>,
    /// `(requested, served)` when the served tier is not the requested one, or
    /// `(None, served)` when none was asked for and it is not the default.
    pub tier: Option<(Option<String>, String)>,
    /// Other facts for the footer (`geo us`).
    pub flags: Vec<String>,
    /// Something unusual worth a note (a fallback to another model, nearly
    /// exhausted rate limits).
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Family {
    Responses,
    Messages,
    Gemini,
    Chat,
    Other,
}

fn family(dialect: &str) -> Family {
    if dialect.ends_with(".responses") {
        Family::Responses
    } else if dialect.ends_with(".messages") {
        Family::Messages
    } else if dialect.starts_with("google.generate") {
        Family::Gemini
    } else if dialect.ends_with(".chat") {
        Family::Chat
    } else {
        Family::Other
    }
}

/// Collects [`CallMeta`] for every call made through it while passing all
/// traffic on to `inner` (the raw log).
pub struct Collector {
    family: Family,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    calls: Vec<CallMeta>,
    started: Option<Instant>,
    /// Calls already handed out by [`Collector::take`].
    taken: usize,
}

impl Collector {
    pub fn new(dialect: &str) -> Self {
        Self { family: family(dialect), state: Mutex::new(State::default()) }
    }

    /// An observer for one stream: records here and forwards to `inner`.
    pub fn tee<'a>(&'a self, inner: &'a dyn Observer) -> Tee<'a> {
        Tee { inner, meta: self }
    }

    /// The calls recorded since the last `take`.
    pub fn take(&self) -> Vec<CallMeta> {
        let mut s = self.state.lock();
        let from = s.taken.min(s.calls.len());
        s.taken = s.calls.len();
        s.calls[from..].to_vec()
    }

    /// Every call recorded so far.
    pub fn calls(&self) -> Vec<CallMeta> {
        self.state.lock().calls.clone()
    }

    /// How the latest call was served compared with `request` (its body) for
    /// `model`: the latest call that returned a response envelope (not a
    /// download or a poll that found nothing), else the latest with headers.
    pub fn served(&self, request: &Value, model: &str) -> Served {
        let s = self.state.lock();
        let calls = s.calls.iter().rev();
        match calls.clone().find(|c| !c.response.is_empty()).or_else(|| calls.clone().find(|c| !c.headers.is_empty())) {
            Some(c) => served(self.family, c, request, model),
            None => Served::default(),
        }
    }

    fn begin(&self, url: &str) {
        let mut s = self.state.lock();
        s.started = Some(Instant::now());
        let url = url.split('?').next().unwrap_or(url).to_string();
        s.calls.push(CallMeta { url, ..Default::default() });
    }

    fn with_call(&self, f: impl FnOnce(&mut CallMeta, u64)) {
        let mut s = self.state.lock();
        let ms = s.started.map(|t| t.elapsed().as_millis() as u64).unwrap_or(0);
        if s.calls.is_empty() {
            s.calls.push(CallMeta::default());
        }
        let c = s.calls.last_mut().unwrap();
        f(c, ms);
    }

    fn status(&self, status: u16, headers: &[(String, String)]) {
        self.with_call(|c, ms| {
            c.status = Some(status);
            c.ms_headers = Some(ms);
            c.headers = headers
                .iter()
                .filter(|(k, _)| keep_header(k))
                .map(|(k, v)| (k.to_ascii_lowercase(), v.chars().take(300).collect()))
                .collect();
        });
    }

    fn event(&self, data: &str) {
        let parsed = serde_json::from_str::<Value>(data).ok();
        let family = self.family;
        self.with_call(|c, ms| {
            c.events += 1;
            c.ms_first_event.get_or_insert(ms);
            c.ms_last_event = Some(ms);
            if let Some(v) = parsed {
                absorb(family, &mut c.response, &v);
            }
        });
    }

    fn body(&self, text: &str) {
        let Ok(v) = serde_json::from_str::<Value>(text) else { return };
        let family = self.family;
        self.with_call(|c, ms| {
            c.ms_last_event = Some(ms);
            // A non-streamed success (an image API); error bodies are kept with the error.
            if c.status.is_some_and(|s| (200..300).contains(&s)) {
                absorb(family, &mut c.response, &v);
            }
        });
    }
}

/// The observer handed to a provider: see [`Collector::tee`].
pub struct Tee<'a> {
    inner: &'a dyn Observer,
    meta: &'a Collector,
}

impl Observer for Tee<'_> {
    fn request(&self, url: &str, body: &Value) {
        self.meta.begin(url);
        self.inner.request(url, body);
    }
    fn response_status(&self, status: u16, headers: &[(String, String)]) {
        self.meta.status(status, headers);
        self.inner.response_status(status, headers);
    }
    fn sse_comment(&self, comment: &str) {
        self.inner.sse_comment(comment);
    }
    fn sse_data(&self, data: &str) {
        self.meta.event(data);
        self.inner.sse_data(data);
    }
    fn body_text(&self, text: &str) {
        self.meta.body(text);
        self.inner.body_text(text);
    }
}

/// Request ids, rate limits, retry hints, timing, serving and deprecation
/// headers; never account identifiers or cookies.
fn keep_header(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    const NEVER: [&str; 7] = ["organization", "project", "uid", "cookie", "workspace", "org-id", "account"];
    if NEVER.iter().any(|d| n.contains(d)) {
        return false;
    }
    const EXACT: [&str; 14] = [
        "x-log-id",
        "x-should-retry",
        "openai-processing-ms",
        "openai-version",
        "cf-ray",
        "traceparent",
        "traceresponse",
        "x-watermark",
        "warning",
        "sunset",
        "msh-gid",
        "x-envoy-upstream-service-time",
        "x-dashscope-timeout",
        "x-generation-id",
    ];
    EXACT.contains(&n.as_str())
        || n.ends_with("request-id")
        || n.ends_with("trace-id")
        || n.ends_with("traceid")
        || n.contains("ratelimit")
        || n.starts_with("retry-after")
        || n.starts_with("server-timing")
        || n.contains("service-tier")
        || n.contains("data-retention")
        || n.contains("deprecat")
        || n.contains("inference-capacity")
        || n.starts_with("msh-usage")
        || ((n.starts_with("req-") || n.starts_with("resp-")) && n.contains("time"))
}

/// Lay `src`'s keys over `dst` (objects merge recursively; nulls and `skip` keys are ignored).
fn merge(dst: &mut Map<String, Value>, src: &Value, skip: &[&str]) {
    let Some(src) = src.as_object() else { return };
    for (k, v) in src {
        if v.is_null() || skip.contains(&k.as_str()) {
            continue;
        }
        match (dst.get_mut(k), v) {
            (Some(Value::Object(d)), Value::Object(_)) => merge(d, v, &[]),
            _ => {
                dst.insert(k.clone(), v.clone());
            }
        }
    }
}

/// Merge the indexed items of `list` (without `skip` keys) into `dst[key][index]`.
fn merge_indexed(dst: &mut Map<String, Value>, key: &str, list: Option<&Value>, skip: &[&str]) {
    let Some(items) = list.and_then(Value::as_array) else { return };
    for (pos, item) in items.iter().enumerate() {
        let i = item.get("index").and_then(Value::as_u64).map_or(pos, |i| i as usize);
        let arr = dst.entry(key).or_insert_with(|| Value::Array(vec![]));
        let Some(arr) = arr.as_array_mut() else { continue };
        while arr.len() <= i {
            arr.push(Value::Object(Map::new()));
        }
        if let Some(m) = arr[i].as_object_mut() {
            merge(m, item, skip);
        }
    }
}

/// Take the envelope fields of one streamed event (or response body).
fn absorb(family: Family, dst: &mut Map<String, Value>, v: &Value) {
    let ty = v.get("type").and_then(Value::as_str).unwrap_or("");
    match family {
        Family::Responses => {
            if ty == "error" {
                dst.insert("error".into(), v.clone());
            } else if let Some(r) = v.get("response").filter(|r| r.is_object()) {
                // response.created / in_progress / queued / completed / incomplete / failed
                merge(dst, r, &["output", "instructions"]);
            } else if v.get("object").and_then(Value::as_str) == Some("response") {
                merge(dst, v, &["output", "instructions"]);
            }
        }
        Family::Messages => match ty {
            "message_start" => merge(dst, &v["message"], &["content"]),
            "message_delta" => {
                merge(dst, &v["delta"], &[]);
                for k in ["usage", "context_management"] {
                    if let Some(x) = v.get(k).filter(|x| x.is_object()) {
                        merge(dst, &serde_json::json!({ k: x }), &[]);
                    }
                }
            }
            "error" => {
                dst.insert("error".into(), v.clone());
            }
            "" if v.get("type").is_none() && v.get("content").is_some() => merge(dst, v, &["content"]),
            _ => {}
        },
        Family::Gemini => {
            merge(dst, v, &["candidates"]);
            merge_indexed(dst, "candidates", v.get("candidates"), &["content"]);
        }
        Family::Chat => {
            merge(dst, v, &["choices"]);
            merge_indexed(dst, "choices", v.get("choices"), &["delta", "message", "logprobs"]);
        }
        Family::Other => {
            // Scalars, usage and errors; never the payload (images, video data).
            if let Some(o) = v.as_object() {
                for (k, x) in o {
                    let small = match x {
                        Value::String(s) => s.len() <= 300,
                        Value::Number(_) | Value::Bool(_) => true,
                        Value::Object(_) => matches!(k.as_str(), "usage" | "error" | "usage_metadata"),
                        _ => false,
                    };
                    if small {
                        merge(dst, &serde_json::json!({ k: x }), &[]);
                    }
                }
            }
        }
    }
}

fn s<'a>(v: &'a Map<String, Value>, path: &[&str]) -> Option<&'a str> {
    let mut cur = v.get(path[0])?;
    for k in &path[1..] {
        cur = cur.get(k)?;
    }
    cur.as_str().filter(|x| !x.is_empty())
}

/// `requested` and `served` name the same model: equal, or `served` is the
/// requested id plus a date/version (`gpt-5.4-nano-2026-03-17`,
/// `claude-x-20261001`, `doubao-seed-2-0-pro-260215`) or a "latest" suffix.
pub fn same_model(requested: &str, served: &str) -> bool {
    let norm = |m: &str| {
        let m = m.strip_prefix("models/").unwrap_or(m);
        m.rsplit('/').next().unwrap_or(m).to_ascii_lowercase()
    };
    let (r, sv) = (norm(requested), norm(served));
    if r == sv {
        return true;
    }
    let Some(rest) = sv.strip_prefix(&r).and_then(|x| x.strip_prefix(['-', '@', ':'])) else { return false };
    rest.starts_with("latest") || (!rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit() || c == '-'))
}

fn norm_tier(t: &str) -> String {
    let t = t.to_ascii_lowercase();
    t.strip_prefix("service_tier_").map(str::to_string).unwrap_or(t)
}

fn default_tier(t: &str) -> bool {
    matches!(t, "default" | "standard" | "auto" | "on_demand" | "standard_only" | "unspecified" | "")
}

fn served(family: Family, c: &CallMeta, request: &Value, model: &str) -> Served {
    let r = &c.response;
    let mut out = Served::default();
    let (served_model, served_tier) = match family {
        Family::Responses | Family::Chat | Family::Other => (s(r, &["model"]), s(r, &["service_tier"])),
        Family::Messages => (s(r, &["model"]), s(r, &["usage", "service_tier"])),
        Family::Gemini => (
            s(r, &["modelVersion"]),
            s(r, &["usageMetadata", "serviceTier"]).or_else(|| c.header("x-gemini-service-tier")),
        ),
    };
    if let Some(m) = served_model
        && !same_model(model, m)
    {
        out.model = Some(m.to_string());
    }
    let requested = request.get("service_tier").and_then(Value::as_str).map(norm_tier).filter(|t| t != "auto");
    if family == Family::Messages && request.get("speed").and_then(Value::as_str) == Some("fast") {
        // Claude fast mode is the bot's `tier fast`.
        if let Some(sp) = s(r, &["usage", "speed"]).filter(|sp| *sp != "fast") {
            out.tier = Some((Some("fast".into()), sp.to_string()));
        }
    } else if let Some(t) = served_tier.map(norm_tier) {
        match &requested {
            Some(req) if *req != t => out.tier = Some((Some(req.clone()), t)),
            None if !default_tier(&t) => out.tier = Some((None, t)),
            _ => {}
        }
    }
    if family == Family::Messages {
        if let Some(g) = s(r, &["usage", "inference_geo"]).filter(|g| !matches!(*g, "global" | "not_available")) {
            out.flags.push(format!("geo {g}"));
        }
        if request.get("speed").is_none() && s(r, &["usage", "speed"]) == Some("fast") {
            out.flags.push("fast".into());
        }
    }
    // Ark: the request fell back to another model.
    if let Some(fb) = r.get("service_status").and_then(|x| x.get("model_fallback"))
        && fb.get("fallback_triggered").and_then(Value::as_bool) == Some(true)
    {
        let orig = fb.get("original_model").and_then(Value::as_str).unwrap_or(model);
        let to = served_model.unwrap_or("another model");
        out.notes.push(format!("the platform fell back from {orig} to {to}"));
    }
    // The effort the API applied when the request named none (D20: the effort is always shown).
    if family == Family::Responses
        && request.pointer("/reasoning/effort").is_none()
        && let Some(e) = s(r, &["reasoning", "effort"])
    {
        out.flags.push(format!("effort {e}"));
    }
    if c.header("x-watermark") == Some("true") {
        out.notes.push("xAI watermarked this output".into());
    }
    if let Some(n) = rate_limit_note(&c.headers) {
        out.notes.push(n);
    }
    out
}

/// A note when some rate limit is below 10% of its limit: `x-ratelimit-remaining-<dim>`
/// with `x-ratelimit-limit-<dim>` (OpenAI, xAI) or `anthropic-ratelimit-<dim>-remaining`
/// with `…-limit` (Claude), and the reset time when given.
pub fn rate_limit_note(headers: &[(String, String)]) -> Option<String> {
    let get = |k: &str| headers.iter().find(|(n, _)| n.eq_ignore_ascii_case(k)).map(|(_, v)| v.trim());
    let mut low = Vec::new();
    for (name, value) in headers {
        let n = name.to_ascii_lowercase();
        let (dim, limit_h, reset_h) = if let Some(d) = n.strip_prefix("x-ratelimit-remaining-") {
            (d.to_string(), format!("x-ratelimit-limit-{d}"), format!("x-ratelimit-reset-{d}"))
        } else if let Some(d) = n.strip_prefix("anthropic-ratelimit-").and_then(|x| x.strip_suffix("-remaining")) {
            (d.to_string(), format!("anthropic-ratelimit-{d}-limit"), format!("anthropic-ratelimit-{d}-reset"))
        } else {
            continue;
        };
        let (Ok(rem), Some(Ok(limit))) = (value.trim().parse::<u64>(), get(&limit_h).map(str::parse::<u64>)) else {
            continue;
        };
        if limit > 0 && rem * 10 < limit {
            let reset = get(&reset_h).map(|r| format!(", resets {r}")).unwrap_or_default();
            low.push(format!("{rem} of {limit} {} left{reset}", dim.replace('-', " ")));
        }
    }
    (!low.is_empty()).then(|| format!("rate limit nearly used up: {}", low.join("; ")))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::NullObserver;

    fn feed(dialect: &str, events: &[Value], headers: &[(&str, &str)]) -> Collector {
        let c = Collector::new(dialect);
        let t = c.tee(&NullObserver);
        t.request("https://api.example.com/v1/x?key=secret", &json!({}));
        let h: Vec<(String, String)> = headers.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        t.response_status(200, &h);
        for e in events {
            t.sse_data(&e.to_string());
        }
        c
    }

    #[test]
    fn responses_envelope_without_content_and_served_facts() {
        let c = feed(
            "openai.responses",
            &[
                json!({"type": "response.created", "response": {"id": "resp_1", "model": "gpt-5.4-nano-2026-03-17", "status": "in_progress", "output": [], "instructions": "sys"}}),
                json!({"type": "response.output_text.delta", "delta": "hi"}),
                json!({"type": "response.completed", "response": {"id": "resp_1", "status": "completed", "service_tier": "default", "output": [{"type": "message"}], "usage": {"input_tokens": 3}}}),
            ],
            &[
                ("x-request-id", "req_9"),
                ("openai-organization", "org-x"),
                ("set-cookie", "a=b"),
                ("openai-processing-ms", "120"),
            ],
        );
        let calls = c.calls();
        assert_eq!(calls.len(), 1);
        let m = &calls[0];
        assert_eq!(m.url, "https://api.example.com/v1/x");
        assert_eq!(m.response.get("output"), None);
        assert_eq!(m.response.get("instructions"), None);
        assert_eq!(m.response["status"], "completed");
        assert_eq!(m.response["usage"]["input_tokens"], 3);
        assert_eq!(m.request_id(), Some("req_9"));
        assert_eq!(m.headers.len(), 2, "{:?}", m.headers);
        assert_eq!(m.events, 3);
        // A dated snapshot is the same model; the fast tier fell back to default.
        let sv = c.served(&json!({"service_tier": "priority"}), "gpt-5.4-nano");
        assert_eq!(sv.model, None);
        assert_eq!(sv.tier, Some((Some("priority".into()), "default".into())));
        assert_eq!(c.served(&json!({}), "gpt-5.4-nano").tier, None);
        // The response says which effort applied: shown when the request named none.
        let e = feed(
            "openai.responses",
            &[
                json!({"type": "response.completed", "response": {"model": "chat-latest", "reasoning": {"effort": "medium"}}}),
            ],
            &[],
        );
        assert_eq!(e.served(&json!({}), "chat-latest").flags, vec!["effort medium"]);
        assert!(e.served(&json!({"reasoning": {"effort": "high"}}), "chat-latest").flags.is_empty());
    }

    #[test]
    fn messages_usage_merges_and_claude_serving_flags() {
        let c = feed(
            "anthropic.messages",
            &[
                json!({"type": "message_start", "message": {"id": "msg_1", "model": "claude-opus-5-5", "content": [], "usage": {"input_tokens": 4, "service_tier": "priority", "inference_geo": "us", "speed": "standard"}}}),
                json!({"type": "content_block_delta", "delta": {"type": "text_delta", "text": "x"}}),
                json!({"type": "message_delta", "delta": {"stop_reason": "end_turn", "stop_details": null}, "usage": {"output_tokens": 9}}),
            ],
            &[
                ("request-id", "req_1"),
                ("anthropic-organization-id", "o"),
                ("anthropic-ratelimit-requests-remaining", "3"),
                ("anthropic-ratelimit-requests-limit", "50"),
                ("anthropic-ratelimit-requests-reset", "2026-11-13T00:00:01Z"),
            ],
        );
        let m = &c.calls()[0];
        assert_eq!(m.response["stop_reason"], "end_turn");
        assert_eq!(
            m.response["usage"],
            json!({"input_tokens": 4, "service_tier": "priority", "inference_geo": "us", "speed": "standard", "output_tokens": 9})
        );
        assert!(m.header("anthropic-organization-id").is_none());
        let sv = c.served(&json!({}), "claude-opus-5-5");
        assert_eq!(sv.tier, Some((None, "priority".into())));
        assert_eq!(sv.flags, vec!["geo us"]);
        assert_eq!(sv.notes, vec!["rate limit nearly used up: 3 of 50 requests left, resets 2026-11-13T00:00:01Z"]);
        let fast = c.served(&json!({"speed": "fast"}), "claude-opus-5-5");
        assert_eq!(fast.tier, Some((Some("fast".into()), "standard".into())));
    }

    #[test]
    fn gemini_candidates_merge_by_index_and_served_model() {
        let c = feed(
            "google.generate",
            &[
                json!({"candidates": [{"content": {"parts": [{"text": "a"}]}, "index": 0}], "modelVersion": "gemini-3.8-flash", "responseId": "r1", "usageMetadata": {"promptTokenCount": 5}}),
                json!({"candidates": [{"content": {"parts": [{"text": "b"}]}, "finishReason": "STOP", "groundingMetadata": {"webSearchQueries": ["q"]}}], "usageMetadata": {"promptTokenCount": 5, "candidatesTokenCount": 2, "serviceTier": "SERVICE_TIER_STANDARD"}}),
            ],
            &[("server-timing", "gfet4t7; dur=812")],
        );
        let m = &c.calls()[0];
        assert_eq!(
            m.response["candidates"],
            json!([{"index": 0, "finishReason": "STOP", "groundingMetadata": {"webSearchQueries": ["q"]}}])
        );
        assert_eq!(m.response["usageMetadata"]["candidatesTokenCount"], 2);
        let sv = c.served(&json!({}), "gemini-3.7-flash");
        assert_eq!(sv.model.as_deref(), Some("gemini-3.8-flash"));
        assert_eq!(sv.tier, None);
        assert_eq!(
            c.served(&json!({"service_tier": "priority"}), "gemini-3.8-flash").tier,
            Some((Some("priority".into()), "standard".into()))
        );
    }

    #[test]
    fn chat_chunks_and_ark_fallback() {
        let c = feed(
            "ark.chat",
            &[
                json!({"id": "c1", "model": "doubao-seed-2-0-lite-260215", "service_tier": "default", "choices": [{"index": 0, "delta": {"content": "x"}}]}),
                json!({"id": "c1", "model": "doubao-seed-2-0-lite-260215", "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}], "service_status": {"model_fallback": {"fallback_triggered": true, "original_model": "doubao-seed-2-0-pro"}}, "usage": {"prompt_tokens": 3}}),
            ],
            &[],
        );
        let m = &c.calls()[0];
        assert_eq!(m.response["choices"], json!([{"index": 0, "finish_reason": "stop"}]));
        let sv = c.served(&json!({}), "doubao-seed-2-0-pro");
        assert_eq!(sv.model.as_deref(), Some("doubao-seed-2-0-lite-260215"));
        assert_eq!(sv.notes, vec!["the platform fell back from doubao-seed-2-0-pro to doubao-seed-2-0-lite-260215"]);
    }

    #[test]
    fn take_returns_new_calls_only() {
        let c = feed("deepseek.chat", &[json!({"id": "a", "model": "deepseek-flash", "choices": []})], &[]);
        assert_eq!(c.take().len(), 1);
        assert!(c.take().is_empty());
        let t = c.tee(&NullObserver);
        t.request("https://x/2", &json!({}));
        assert_eq!(c.take().len(), 1);
        assert_eq!(c.calls().len(), 2);
    }

    #[test]
    fn same_model_rules() {
        assert!(same_model("gpt-5.4-nano", "gpt-5.4-nano-2026-03-17"));
        assert!(same_model("claude-opus-5-5", "claude-opus-5-5-20261001"));
        assert!(same_model("doubao-seed-evolving", "doubao-seed-evolving-latest-version"));
        assert!(same_model("google/gemini-3.7-flash", "gemini-3.7-flash"));
        assert!(same_model("gemini-3.7-flash", "models/gemini-3.7-flash"));
        assert!(!same_model("gpt-5.4", "gpt-5.4-pro"));
        assert!(!same_model("gemini-3.7-flash", "gemini-3.8-flash"));
    }

    #[test]
    fn headers_kept_and_dropped() {
        for h in [
            "x-request-id",
            "request-id",
            "x-ds-trace-id",
            "msh-request-id",
            "x-ratelimit-remaining-tokens",
            "retry-after-ms",
            "x-should-retry",
            "server-timing",
            "x-gemini-service-tier",
            "msh-usage-cache-write-tokens-5m",
            "req-cost-time",
            "x-zero-data-retention",
        ] {
            assert!(keep_header(h), "{h}");
        }
        for h in [
            "openai-organization",
            "openai-project",
            "set-cookie",
            "msh-uid",
            "msh-org-id",
            "msh-project-id",
            "anthropic-organization-id",
            "content-type",
            "date",
        ] {
            assert!(!keep_header(h), "{h}");
        }
    }
}
