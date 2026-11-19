//! OpenRouter model catalog (`/api/v1/models`, `/api/v1/models/{id}/endpoints`).
//!
//! Lets the bot support brand-new models with no code or config change: the
//! capabilities (modalities, supported parameters, reasoning efforts, max
//! output tokens) come from the live catalog.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Architecture {
    #[serde(default)]
    pub input_modalities: Vec<String>,
    #[serde(default)]
    pub output_modalities: Vec<String>,
    /// Every other field OpenRouter sends, kept as it came (shown by `/model`).
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ReasoningInfo {
    /// Highest first. `None` means "all levels".
    #[serde(default)]
    pub supported_efforts: Option<Vec<String>>,
    #[serde(default)]
    pub default_effort: Option<String>,
    #[serde(default)]
    pub default_enabled: Option<bool>,
    #[serde(default)]
    pub mandatory: Option<bool>,
    #[serde(default)]
    pub supports_max_tokens: Option<bool>,
    /// Every other field OpenRouter sends, kept as it came (shown by `/model`).
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TopProvider {
    #[serde(default)]
    pub context_length: Option<u64>,
    #[serde(default)]
    pub max_completion_tokens: Option<u64>,
    #[serde(default)]
    pub is_moderated: Option<bool>,
    /// Every other field OpenRouter sends, kept as it came (shown by `/model`).
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    #[serde(default)]
    pub canonical_slug: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub context_length: Option<u64>,
    #[serde(default)]
    pub architecture: Architecture,
    #[serde(default)]
    pub supported_parameters: Vec<String>,
    #[serde(default)]
    pub reasoning: Option<ReasoningInfo>,
    #[serde(default)]
    pub top_provider: TopProvider,
    #[serde(default)]
    pub pricing: Value,
    #[serde(default)]
    pub description: Option<String>,
    /// Every other field OpenRouter sends, kept as it came (shown by `/model`).
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}

impl ModelInfo {
    pub fn author(&self) -> &str {
        self.id.split('/').next().unwrap_or("")
    }
    pub fn accepts(&self, modality: &str) -> bool {
        self.architecture.input_modalities.iter().any(|m| m == modality)
    }
    pub fn outputs(&self, modality: &str) -> bool {
        self.architecture.output_modalities.iter().any(|m| m == modality)
    }
    pub fn supports(&self, param: &str) -> bool {
        self.supported_parameters.iter().any(|p| p == param)
    }
    /// Highest supported effort, if the model exposes an effort list.
    pub fn max_effort(&self) -> Option<&str> {
        self.reasoning.as_ref()?.supported_efforts.as_ref()?.first().map(String::as_str)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EndpointInfo {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub provider_name: Option<String>,
    /// Routing slug (e.g. `anthropic`, `google-vertex/global`).
    #[serde(default)]
    pub tag: Option<String>,
    #[serde(default)]
    pub quantization: Option<String>,
    #[serde(default)]
    pub status: Option<Value>,
    #[serde(default)]
    pub context_length: Option<u64>,
    #[serde(default)]
    pub max_completion_tokens: Option<u64>,
    #[serde(default)]
    pub supported_parameters: Vec<String>,
    #[serde(default)]
    pub native_tools: Option<Value>,
    #[serde(default)]
    pub service_tier: Option<String>,
    /// Every other field OpenRouter sends, kept as it came (shown by `/model`).
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}

impl EndpointInfo {
    /// Base provider slug (`google-vertex/global` -> `google-vertex`).
    pub fn base_slug(&self) -> Option<&str> {
        self.tag.as_deref().map(|t| t.split('/').next().unwrap_or(t))
    }
    pub fn has_native_tool(&self, tool: &str) -> bool {
        self.native_tools.as_ref().and_then(|v| v.get(tool)).is_some_and(|v| !v.is_null())
    }
}

// ------------------------------------------------------------------ /model

/// OpenRouter prices per token (USD); shown per 1M tokens.
const PER_TOKEN: &[(&str, &str)] = &[
    ("prompt", "input"),
    ("completion", "output"),
    ("internal_reasoning", "reasoning"),
    ("input_cache_read", "cache read"),
    ("input_cache_write", "cache write"),
    ("input_cache_write_1h", "cache write (1 hour)"),
    ("audio", "audio input"),
    ("input_audio_cache", "cached audio input"),
    ("audio_output", "audio output"),
    ("image_token", "image tokens"),
];

/// One `pricing` field, readable: per-token prices per 1M tokens, the others
/// in their own unit (`image` and `image_output` as OpenRouter gives them).
fn price_row(k: &str, v: &Value) -> (String, String) {
    use crate::card::{usd, usd_m};
    let num = v.as_str().and_then(|s| s.parse::<f64>().ok()).or_else(|| v.as_f64());
    match (PER_TOKEN.iter().find(|(key, _)| *key == k), num) {
        (Some((_, label)), Some(x)) => (label.to_string(), usd_m(x * 1e6)),
        (None, Some(x)) => match k {
            "web_search" => ("web search".into(), format!("{} per search", usd(x))),
            "request" => ("per request".into(), usd(x)),
            "discount" => ("discount".into(), format!("{}%", x * 100.0)),
            _ => (k.to_string(), format!("{} (OpenRouter's unit)", usd(x))),
        },
        _ => (k.to_string(), crate::card::value(v)),
    }
}

/// Every leaf of `v` under `path` as label / value rows (objects as dotted paths).
fn leaves(path: &str, v: &Value, out: &mut Vec<(String, String)>) {
    match v {
        Value::Object(m) if !m.is_empty() => {
            for (k, v) in m {
                let p = if path.is_empty() { k.clone() } else { format!("{path}.{k}") };
                leaves(&p, v, out);
            }
        }
        v => out.push((path.to_string(), crate::card::value(v))),
    }
}

/// What `/model` shows of a catalog entry: every field, readable (the
/// description apart). Fields the bot does not know go to the notes as they came.
pub fn card(m: &ModelInfo) -> crate::card::ModelCard {
    use crate::card::{self, INPUT, LIMITS, MODEL, NOTES, OUTPUT, PARAMETERS, PRICE, REASONING};
    let mut c = card::ModelCard::default();
    let v = serde_json::to_value(m).unwrap_or_default();
    let Some(obj) = v.as_object() else { return c };
    for (k, val) in obj {
        match k.as_str() {
            "description" => {}
            "id" => {
                c.row(MODEL, "id", card::value(val));
            }
            "name" => {
                c.row(MODEL, "name", card::value(val));
            }
            "canonical_slug" => {
                c.row(MODEL, "canonical slug", card::value(val));
            }
            "created" => {
                let when = val.as_i64().map(card::utc).unwrap_or_else(|| card::value(val));
                c.row(MODEL, "added", when);
            }
            "hugging_face_id" => {
                c.row(MODEL, "Hugging Face", card::value(val));
            }
            "knowledge_cutoff" => {
                c.row(MODEL, "knowledge cutoff", card::value(val));
            }
            "expiration_date" => {
                c.row(MODEL, "expires", card::value(val));
            }
            "context_length" => {
                let t = val.as_u64().map(card::tokens).unwrap_or_else(|| card::value(val));
                c.row(LIMITS, "context window", t);
            }
            "top_provider" => {
                for (sub, v) in val.as_object().into_iter().flatten() {
                    let label = match sub.as_str() {
                        "context_length" => "top provider's context window".to_string(),
                        "max_completion_tokens" => "top provider's max output".to_string(),
                        "is_moderated" => "moderated".to_string(),
                        other => format!("top provider: {other}"),
                    };
                    let shown = v
                        .as_u64()
                        .filter(|_| sub != "is_moderated")
                        .map(card::tokens)
                        .unwrap_or_else(|| card::value(v));
                    c.row(LIMITS, label, shown);
                }
            }
            "per_request_limits" => {
                c.row(LIMITS, "per-request limits", card::value(val));
            }
            "architecture" => {
                for (sub, v) in val.as_object().into_iter().flatten() {
                    match sub.as_str() {
                        "input_modalities" => c.row(INPUT, "modalities", card::value(v)),
                        "output_modalities" => c.row(OUTPUT, "modalities", card::value(v)),
                        "modality" => c.row(NOTES, "modality", card::value(v)),
                        "tokenizer" => c.row(NOTES, "tokenizer", card::value(v)),
                        "instruct_type" => c.row(NOTES, "instruct type", card::value(v)),
                        other => c.row(NOTES, format!("architecture.{other}"), card::value(v)),
                    };
                }
            }
            "reasoning" => {
                for (sub, v) in val.as_object().into_iter().flatten() {
                    let label = match sub.as_str() {
                        "mandatory" => "always on",
                        "supported_efforts" => "levels",
                        "default_effort" => "default level (OpenRouter's)",
                        "default_enabled" => "on by default",
                        "supports_max_tokens" => "takes a token budget",
                        other => other,
                    };
                    c.row(REASONING, label, card::value(v));
                }
            }
            "pricing" => {
                for (sub, v) in val.as_object().into_iter().flatten() {
                    let (label, shown) = price_row(sub, v);
                    c.row(PRICE, label, shown);
                }
            }
            "supported_parameters" => {
                c.row(PARAMETERS, "supported", card::value(val));
            }
            "default_parameters" => {
                c.row(PARAMETERS, "defaults", card::value(val));
            }
            "supported_voices" => {
                c.row(OUTPUT, "voices", card::value(val));
            }
            "endpoints" => {
                c.row(NOTES, "APIs", card::value(val));
            }
            other => {
                let mut rows = Vec::new();
                leaves(other, val, &mut rows);
                for (l, v) in rows {
                    c.row(NOTES, l, v);
                }
            }
        }
    }
    c
}

/// Every field of an endpoint as label / value rows (prices per 1M tokens).
pub fn endpoint_rows(e: &EndpointInfo) -> Vec<(String, String)> {
    let v = serde_json::to_value(e).unwrap_or_default();
    let mut out = Vec::new();
    for (k, val) in v.as_object().into_iter().flatten() {
        if k == "pricing" {
            for (sub, v) in val.as_object().into_iter().flatten() {
                let (label, shown) = price_row(sub, v);
                out.push((format!("price: {label}"), shown));
            }
        } else if k == "max_completion_tokens" || k == "context_length" || k == "max_prompt_tokens" {
            out.push((k.clone(), val.as_u64().map(crate::card::tokens).unwrap_or_else(|| crate::card::value(val))));
        } else {
            leaves(k, val, &mut out);
        }
    }
    out
}

/// An endpoint's price per 1M tokens (input, output), from its `pricing`.
pub fn endpoint_price(e: &EndpointInfo) -> Option<(f64, f64)> {
    let p = e.extra.get("pricing")?;
    let f = |k: &str| p.get(k).and_then(Value::as_str).and_then(|s| s.parse::<f64>().ok());
    Some((f("prompt")? * 1e6, f("completion")? * 1e6))
}

#[derive(Default)]
struct CatalogInner {
    models: HashMap<String, Arc<ModelInfo>>,
    fetched_at: Option<Instant>,
    endpoints: HashMap<String, (Instant, Arc<Vec<EndpointInfo>>)>,
}

/// Shared, refreshable catalog cache.
#[derive(Clone, Default)]
pub struct Catalog {
    inner: Arc<RwLock<CatalogInner>>,
}

pub const CATALOG_TTL: Duration = Duration::from_secs(600);
pub const ENDPOINTS_TTL: Duration = Duration::from_secs(600);

impl Catalog {
    pub fn replace(&self, models: Vec<ModelInfo>) {
        let mut g = self.inner.write();
        g.models.clear();
        for m in models {
            let m = Arc::new(m);
            if let Some(slug) = &m.canonical_slug {
                g.models.entry(slug.clone()).or_insert_with(|| m.clone());
            }
            g.models.insert(m.id.clone(), m);
        }
        g.fetched_at = Some(Instant::now());
    }

    pub fn is_stale(&self) -> bool {
        self.inner.read().fetched_at.is_none_or(|t| t.elapsed() > CATALOG_TTL)
    }

    pub fn len(&self) -> usize {
        self.inner.read().models.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Look up a model id, ignoring routing suffixes like `:nitro`.
    pub fn get(&self, id: &str) -> Option<Arc<ModelInfo>> {
        let g = self.inner.read();
        if let Some(m) = g.models.get(id) {
            return Some(m.clone());
        }
        let base = id.split(':').next().unwrap_or(id);
        g.models.get(base).cloned()
    }

    pub fn search(&self, needle: &str, limit: usize) -> Vec<Arc<ModelInfo>> {
        let g = self.inner.read();
        let needle = needle.to_ascii_lowercase();
        let mut v: Vec<_> = g
            .models
            .iter()
            .filter(|(k, m)| *k == &m.id && m.id.to_ascii_lowercase().contains(&needle))
            .map(|(_, m)| m.clone())
            .collect();
        v.sort_by(|a, b| a.id.cmp(&b.id));
        v.truncate(limit);
        v
    }

    pub fn endpoints(&self, id: &str) -> Option<Arc<Vec<EndpointInfo>>> {
        let g = self.inner.read();
        g.endpoints.get(id).filter(|(t, _)| t.elapsed() < ENDPOINTS_TTL).map(|(_, e)| e.clone())
    }

    pub fn put_endpoints(&self, id: &str, eps: Vec<EndpointInfo>) -> Arc<Vec<EndpointInfo>> {
        let eps = Arc::new(eps);
        self.inner.write().endpoints.insert(id.to_string(), (Instant::now(), eps.clone()));
        eps
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `/model` shows every field of a catalog entry: known ones readable,
    /// unknown ones (here `benchmarks`, `brand_new`) as they came.
    #[test]
    fn the_whole_entry_is_shown_readably() {
        use crate::card::{LIMITS, MODEL, NOTES, PRICE, REASONING};
        let m: ModelInfo = serde_json::from_value(serde_json::json!({
            "id": "google/gemini-4-neon", "canonical_slug": "google/gemini-4-neon-20261118", "name": "Google: Gemini 4 Neon",
            "created": 1795006977, "description": "d", "context_length": 1048576, "hugging_face_id": null,
            "architecture": {"modality": "text+image->text", "input_modalities": ["text", "image"], "output_modalities": ["text"], "tokenizer": "Gemini"},
            "pricing": {"prompt": "0.000003", "completion": "0.000015", "input_cache_read": "0.00000015", "web_search": "0.014", "image": "0.000003", "discount": 0},
            "top_provider": {"context_length": 1048576, "max_completion_tokens": 65536, "is_moderated": false},
            "supported_parameters": ["reasoning", "tools"], "default_parameters": {},
            "reasoning": {"mandatory": true, "supported_efforts": ["xhigh", "high"], "default_effort": "medium"},
            "benchmarks": {"artificial_analysis": {"intelligence_index": 61.4}}, "brand_new": [1, 2]
        }))
        .unwrap();
        assert_eq!(m.extra.get("brand_new"), Some(&serde_json::json!([1, 2])), "kept");
        let c = card(&m);
        assert_eq!(c.get(MODEL, "added"), Some("2026-11-18 13:02 UTC"));
        assert_eq!(c.get(MODEL, "Hugging Face"), Some("none"));
        assert_eq!(c.get(LIMITS, "context window"), Some("1,048,576 tokens"));
        assert_eq!(c.get(LIMITS, "top provider's max output"), Some("65,536 tokens"));
        assert_eq!(c.get(LIMITS, "moderated"), Some("no"));
        assert_eq!(c.get(REASONING, "levels"), Some("xhigh, high"));
        assert_eq!(c.get(PRICE, "input"), Some("$3.00 / 1M tokens"));
        assert_eq!(c.get(PRICE, "cache read"), Some("$0.15 / 1M tokens"));
        assert_eq!(c.get(PRICE, "web search"), Some("$0.014 per search"));
        assert_eq!(c.get(PRICE, "image"), Some("$0.000003 (OpenRouter's unit)"));
        assert_eq!(c.get(PRICE, "discount"), Some("0%"));
        assert_eq!(c.get(NOTES, "benchmarks.artificial_analysis.intelligence_index"), Some("61.4"));
        assert_eq!(c.get(NOTES, "brand_new"), Some("1, 2"));
        assert!(c.sections.iter().all(|s| s.rows.iter().all(|(l, _)| l != "description")), "shown apart");

        let e: EndpointInfo = serde_json::from_value(serde_json::json!({
            "tag": "google-ai-studio/flex", "max_completion_tokens": 943718, "uptime_last_1d": 99.2,
            "pricing": {"prompt": "0.0000015", "completion": "0.0000075"}, "supports_tool_choice": {"auto": true}
        }))
        .unwrap();
        let rows = endpoint_rows(&e);
        let get = |l: &str| rows.iter().find(|(k, _)| k == l).map(|(_, v)| v.as_str());
        assert_eq!(get("max_completion_tokens"), Some("943,718 tokens"));
        assert_eq!(get("uptime_last_1d"), Some("99.2"));
        assert_eq!(get("price: input"), Some("$1.50 / 1M tokens"));
        assert_eq!(get("supports_tool_choice.auto"), Some("yes"));
        assert_eq!(endpoint_price(&e), Some((1.5, 7.5)));
    }

    #[test]
    fn parses_live_shape() {
        let j = serde_json::json!({
            "id": "anthropic/claude-opus-5.5",
            "canonical_slug": "anthropic/claude-opus-5.5-20261001",
            "architecture": {"input_modalities": ["text", "image", "file"], "output_modalities": ["text"], "tokenizer": "Claude"},
            "supported_parameters": ["max_tokens", "reasoning", "tools"],
            "reasoning": {"supported_efforts": ["max", "xhigh", "high", "medium", "low"], "default_effort": "high", "mandatory": true},
            "top_provider": {"context_length": 1000000, "max_completion_tokens": 128000, "is_moderated": true},
            "pricing": {"prompt": "0.000004"},
            "unknown_field": 1
        });
        let m: ModelInfo = serde_json::from_value(j).unwrap();
        assert_eq!(m.max_effort(), Some("max"));
        assert!(m.accepts("file"));
        let c = Catalog::default();
        c.replace(vec![m]);
        assert!(c.get("anthropic/claude-opus-5.5:nitro").is_some());
        assert!(c.get("anthropic/claude-opus-5.5-20261001").is_some());
    }
}
