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
    /// Charges left out of `cost_usd`, and so of spend limits (see [`Unbilled`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unbilled: Vec<Unbilled>,
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

    /// `other`'s unbilled charges added to these: counts and amounts of the
    /// same kind summed.
    pub fn add_unbilled(&mut self, other: &[Unbilled]) {
        for o in other {
            match self.unbilled.iter_mut().find(|u| u.what == o.what) {
                Some(u) => {
                    u.count += o.count;
                    u.usd += o.usd;
                }
                None => self.unbilled.push(o.clone()),
            }
        }
    }
}

/// A charge left out of the cost because a free allowance the bot cannot see
/// may cover it (the owner's rule, 2026-11-13: Google's 5,000 free search
/// queries a month belong to the whole project). Replies show it after the
/// cost with what it costs once the allowance is used up (`+ 24 searches
/// ≈$0.336`); spend limits do not count it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Unbilled {
    pub what: Counted,
    pub count: u64,
    /// USD without the allowance.
    pub usd: f64,
}

/// What an [`Unbilled`] charge counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Counted {
    /// Executed search queries (Google Search, image search and Maps on Gemini 3 and newer).
    SearchQueries,
    /// Prompts grounded with Google Search or Maps (Gemini 2.5).
    GroundedPrompts,
}

impl Counted {
    /// `count` of these in words: `1 search`, `24 searches`.
    pub fn words(self, count: u64) -> String {
        let (one, many) = match self {
            Counted::SearchQueries => ("search", "searches"),
            Counted::GroundedPrompts => ("grounded prompt", "grounded prompts"),
        };
        format!("{count} {}", if count == 1 { one } else { many })
    }
}
