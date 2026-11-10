//! Token usage and cost, normalized across providers. The provider's raw usage
//! object is always kept verbatim in `raw`.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CostSource {
    /// Cost reported by the API in the response (e.g. OpenRouter `usage.cost`).
    #[default]
    Reported,
    /// Cost fetched afterwards (e.g. OpenRouter `/generation`).
    Fetched,
    /// Estimated locally from token counts and a price table.
    Estimated,
    /// Unknown.
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Usage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    #[serde(default)]
    pub cost_source: CostSource,
    /// Provider usage object, verbatim.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub raw: Value,
}

impl Usage {
    pub fn total_tokens(&self) -> Option<u64> {
        match (self.input_tokens, self.output_tokens) {
            (Some(a), Some(b)) => Some(a + b),
            (Some(a), None) | (None, Some(a)) => Some(a),
            _ => None,
        }
    }
}
