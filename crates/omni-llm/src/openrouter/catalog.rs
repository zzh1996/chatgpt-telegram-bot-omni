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
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TopProvider {
    #[serde(default)]
    pub context_length: Option<u64>,
    #[serde(default)]
    pub max_completion_tokens: Option<u64>,
    #[serde(default)]
    pub is_moderated: Option<bool>,
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
