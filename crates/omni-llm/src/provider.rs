//! Direct (official) APIs as providers: one interface, a common config, and
//! pricing helpers. Every platform lives in its own module (`openai`,
//! `anthropic`, `google`, `xai`, `deepseek`, `zhipu`, `moonshot`, `alibaba`,
//! `ark`); OpenRouter keeps its own client (catalog, generation lookups).
//!
//! Conventions shared by all of them (see docs/decisions.md D26, D3):
//! - options are validated against what the platform officially offers, and
//!   anything it does not offer is an error, never silently dropped;
//! - defaults are the highest the model offers (effort, output, detail);
//! - the request is sent as built (visible in /info), nothing is injected;
//! - the platform's error objects are kept verbatim.

use std::sync::Arc;
use std::time::Duration;

use omni_core::Usage;
use omni_core::usage::CostSource;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::error::LlmError;
use crate::event::StreamEvent;
use crate::request::Requested;
use crate::{BlobLoader, CallInput, CallOutcome, EventSink, Observer, Prepared};

/// Settings of one direct provider (`[providers.<id>]`), resolved.
#[derive(Debug, Clone)]
pub struct DirectConfig {
    pub api_key: String,
    /// API base URL (the platform's default unless configured), no trailing
    /// slash. The Chinese platforms are always their domestic (China) sites,
    /// as in the old bot; their price lists are in CNY.
    pub base_url: String,
    pub connect_timeout: Duration,
    /// Silence in a stream after which the attempt counts as stalled.
    pub idle_timeout: Duration,
    /// Cap for the automatic output budget; `None` = the model's maximum.
    pub max_tokens_cap: Option<u64>,
    /// CNY per USD, for platforms that bill in CNY (cost estimates).
    pub cny_per_usd: f64,
    /// Total output budget of a long-decode answer (Gemini 4), across hops.
    pub long_decode_budget: u64,
}

impl DirectConfig {
    /// `base_url` + `path` (`path` starts with `/`).
    pub fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url.trim_end_matches('/'), path)
    }

    /// An HTTP client with the configured timeouts.
    pub fn http(&self) -> anyhow::Result<reqwest::Client> {
        Ok(reqwest::Client::builder()
            .connect_timeout(self.connect_timeout)
            .tcp_keepalive(Duration::from_secs(30))
            .pool_idle_timeout(Duration::from_secs(90))
            .build()?)
    }
}

/// A platform's own API.
#[async_trait::async_trait]
pub trait Provider: Send + Sync {
    /// Config key and model-id prefix (`openai` in `openai:gpt-6.1-sol`).
    fn id(&self) -> &'static str;
    /// Name used in messages ("OpenAI does not support …").
    fn name(&self) -> &'static str;
    /// Whether the model is in this provider's model table (limits and price known).
    fn knows(&self, model: &str) -> bool;
    /// Build the request: options parsed and checked against what the platform
    /// offers for the model (`presets` from the config first, then the user's).
    async fn prepare(
        &self,
        input: &CallInput<'_>,
        presets: &[Requested],
        blobs: &dyn BlobLoader,
    ) -> Result<Prepared, LlmError>;
    /// Run a prepared request, streaming normalized events into `sink`. Usage
    /// events carry the cost (reported by the API, or estimated from the price
    /// list). On success returns the native assistant payload for replay.
    async fn stream(
        &self,
        p: &Prepared,
        sink: &mut dyn EventSink,
        observer: &dyn Observer,
        cancel: &CancellationToken,
    ) -> (Result<Value, LlmError>, CallOutcome);
    /// Account balance for /credits, verbatim; `None` if the platform has no
    /// such API (for normal API keys).
    async fn balance(&self) -> Option<anyhow::Result<Value>> {
        None
    }
}

/// What the bot knows about each direct provider (config, help, docs).
#[derive(Debug, Clone, Copy)]
pub struct ProviderInfo {
    pub id: &'static str,
    pub name: &'static str,
    /// Environment variable the example config reads the key from.
    pub env: &'static str,
    /// Default base URL.
    pub base_url: &'static str,
    /// Default idle timeout (seconds).
    pub idle_timeout_secs: u64,
}

/// All direct providers, in display order.
pub const PROVIDERS: &[ProviderInfo] = &[
    ProviderInfo {
        id: "openai",
        name: "OpenAI",
        env: "OPENAI_API_KEY",
        base_url: "https://api.openai.com/v1",
        idle_timeout_secs: 3600,
    },
    ProviderInfo {
        id: "anthropic",
        name: "Anthropic",
        env: "ANTHROPIC_API_KEY",
        base_url: "https://api.anthropic.com/v1",
        idle_timeout_secs: 3600,
    },
    ProviderInfo {
        id: "google",
        name: "Gemini API",
        env: "GEMINI_API_KEY",
        base_url: "https://generativelanguage.googleapis.com/v1beta",
        idle_timeout_secs: 3600,
    },
    ProviderInfo {
        id: "xai",
        name: "xAI",
        env: "XAI_API_KEY",
        base_url: "https://api.x.ai/v1",
        idle_timeout_secs: 3600,
    },
    ProviderInfo {
        id: "deepseek",
        name: "DeepSeek",
        env: "DEEPSEEK_API_KEY",
        base_url: "https://api.deepseek.com",
        idle_timeout_secs: 3600,
    },
    ProviderInfo {
        id: "zhipu",
        name: "Zhipu BigModel",
        env: "ZHIPUAI_API_KEY",
        base_url: "https://open.bigmodel.cn/api/paas/v4",
        idle_timeout_secs: 3600,
    },
    ProviderInfo {
        id: "moonshot",
        name: "Moonshot",
        env: "MOONSHOT_API_KEY",
        base_url: "https://api.moonshot.cn/v1",
        idle_timeout_secs: 3600,
    },
    ProviderInfo {
        id: "alibaba",
        name: "Alibaba Model Studio",
        env: "DASHSCOPE_API_KEY",
        // As in the old bot. Alibaba recommends workspace domains
        // (`https://{WorkspaceId}.cn-beijing.maas.aliyuncs.com/compatible-mode/v1`):
        // the legacy domain gets no new features after 2026-09-30.
        base_url: "https://dashscope.aliyuncs.com/compatible-mode/v1",
        idle_timeout_secs: 3600,
    },
    ProviderInfo {
        id: "ark",
        name: "Volcengine Ark",
        env: "ARK_API_KEY",
        base_url: crate::ark::DEFAULT_BASE_URL,
        idle_timeout_secs: 1800,
    },
];

pub fn info(id: &str) -> Option<&'static ProviderInfo> {
    PROVIDERS.iter().find(|p| p.id == id)
}

/// Construct the provider `id` from its resolved config.
pub fn build(id: &str, cfg: DirectConfig) -> anyhow::Result<Arc<dyn Provider>> {
    Ok(match id {
        "openai" => Arc::new(crate::openai::OpenAi::new(cfg)?),
        "anthropic" => Arc::new(crate::anthropic::Anthropic::new(cfg)?),
        "google" => Arc::new(crate::google::Google::new(cfg)?),
        "xai" => Arc::new(crate::xai::Xai::new(cfg)?),
        "deepseek" => Arc::new(crate::deepseek::DeepSeek::new(cfg)?),
        "zhipu" => Arc::new(crate::zhipu::Zhipu::new(cfg)?),
        "moonshot" => Arc::new(crate::moonshot::Moonshot::new(cfg)?),
        "alibaba" => Arc::new(crate::alibaba::Alibaba::new(cfg)?),
        "ark" => Arc::new(crate::ark::Ark::new(crate::ark::ArkConfig {
            api_key: cfg.api_key,
            base_url: cfg.base_url,
            connect_timeout: cfg.connect_timeout,
            idle_timeout: cfg.idle_timeout,
            max_tokens_cap: cfg.max_tokens_cap.unwrap_or(0),
            cny_per_usd: cfg.cny_per_usd,
        })?),
        other => anyhow::bail!("unknown provider `{other}`"),
    })
}

// ------------------------------------------------------------------ pricing

/// The currency of a price list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Currency {
    Usd,
    Cny,
}

/// List prices per 1M tokens, in the platform's currency.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TokenPrice {
    pub input: f64,
    /// Cache reads.
    pub cached_input: f64,
    /// Cache writes (where the platform bills them separately; else = `input`).
    pub cache_write: f64,
    pub output: f64,
}

impl TokenPrice {
    /// Cost of `u` in the price list's currency: uncached input, cache reads,
    /// cache writes and output (reasoning is part of output everywhere).
    /// `None` without input or output counts.
    pub fn cost(&self, u: &Usage) -> Option<f64> {
        let input = u.input_tokens? as f64;
        let output = u.output_tokens? as f64;
        let cached = u.cached_input_tokens.unwrap_or(0) as f64;
        let written = u.cache_write_tokens.unwrap_or(0) as f64;
        let uncached = (input - cached - written).max(0.0);
        Some(
            (uncached * self.input + cached * self.cached_input + written * self.cache_write + output * self.output)
                / 1e6,
        )
    }
}

/// An amount in USD.
pub fn usd(amount: f64, currency: Currency, cny_per_usd: f64) -> Option<f64> {
    match currency {
        Currency::Usd => Some(amount),
        Currency::Cny => (cny_per_usd > 0.0).then(|| amount / cny_per_usd),
    }
}

/// Fills in the cost of usage events that carry none, with `estimate`
/// (`CostSource::Estimated`; `Unknown` when it returns `None`).
pub struct PricingSink<'a> {
    pub inner: &'a mut dyn EventSink,
    pub estimate: &'a (dyn Fn(&Usage) -> Option<f64> + Send + Sync),
}

impl EventSink for PricingSink<'_> {
    fn event(&mut self, ev: StreamEvent) {
        match ev {
            StreamEvent::Usage(mut u) if u.cost_usd.is_none() => {
                u.cost_usd = (self.estimate)(&u);
                u.cost_source = if u.cost_usd.is_some() { CostSource::Estimated } else { CostSource::Unknown };
                self.inner.event(StreamEvent::Usage(u));
            }
            other => self.inner.event(other),
        }
    }
}

// ------------------------------------------------------------------ helpers

/// Load a blob for a request.
pub async fn load(blobs: &dyn BlobLoader, r: &omni_core::BlobRef) -> Result<Vec<u8>, LlmError> {
    blobs.load(r).await.map_err(|e| LlmError::request(format!("cannot load {r}: {e:#}")))
}

pub fn b64(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

pub fn data_url(mime: &str, bytes: &[u8]) -> String {
    format!("data:{mime};base64,{}", b64(bytes))
}

/// The message for a location sent to a model whose API takes none.
pub fn no_location(model: &str, via: &str) -> LlmError {
    LlmError::request(format!(
        "`{model}` does not take a location via {via} (only Gemini's Google Maps grounding does: `google:` models with +m)"
    ))
}

/// What other models generated in `turns` — earlier answers that go to the API
/// as their text, so their images and files are not sent — as a warning ending
/// with `why`: `2 images and 1 video generated in earlier turns were not sent:
/// assistant messages here carry text only`. Drafts made while thinking are
/// reasoning, which never goes to another model, and are not counted.
pub fn media_not_sent<'a>(turns: impl IntoIterator<Item = &'a omni_core::Turn>, why: &str) -> Option<String> {
    use omni_core::content::Part;
    let mut kinds =
        [("image", "images", 0usize), ("video", "videos", 0), ("audio file", "audio files", 0), ("file", "files", 0)];
    let mut with_media = 0usize;
    for t in turns {
        let before: usize = kinds.iter().map(|k| k.2).sum();
        for part in &t.parts {
            let k = match part {
                Part::Image { generated: true, thought: false, .. } => 0,
                Part::File { generated: true, blob, .. } => match blob.mime.split('/').next() {
                    Some("image") => 0,
                    Some("video") => 1,
                    Some("audio") => 2,
                    _ => 3,
                },
                _ => continue,
            };
            kinds[k].2 += 1;
        }
        with_media += usize::from(kinds.iter().map(|k| k.2).sum::<usize>() > before);
    }
    let n: usize = kinds.iter().map(|k| k.2).sum();
    let what: Vec<String> = kinds
        .iter()
        .filter(|k| k.2 > 0)
        .map(|(one, many, n)| format!("{n} {}", if *n == 1 { one } else { many }))
        .collect();
    let what = match what.split_last()? {
        (last, []) => last.clone(),
        (last, rest) => format!("{} and {last}", rest.join(", ")),
    };
    let turns = if with_media == 1 { "an earlier turn" } else { "earlier turns" };
    Some(format!("{what} generated in {turns} {} not sent: {why}", if n == 1 { "was" } else { "were" }))
}

/// [`media_not_sent`] for the assistant turns of `history` that are not `own`
/// (the model's own answers, replayed natively), in Chat Completions APIs whose
/// assistant messages carry text only.
pub fn unsent_media(history: &[omni_core::Turn], own: impl Fn(&omni_core::Turn) -> bool) -> Option<String> {
    media_not_sent(
        history.iter().filter(|t| t.role == omni_core::Role::Assistant && !own(t)),
        "assistant messages here carry text only",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_price() {
        let p = TokenPrice { input: 2.0, cached_input: 0.2, cache_write: 2.5, output: 10.0 };
        let u = Usage {
            input_tokens: Some(1_000_000),
            cached_input_tokens: Some(400_000),
            cache_write_tokens: Some(100_000),
            output_tokens: Some(100_000),
            ..Default::default()
        };
        // 0.5M × 2 + 0.4M × 0.2 + 0.1M × 2.5 + 0.1M × 10
        assert!((p.cost(&u).unwrap() - (1.0 + 0.08 + 0.25 + 1.0)).abs() < 1e-9);
        assert_eq!(usd(7.1, Currency::Cny, 7.1), Some(1.0));
    }

    #[test]
    fn domestic_sites() {
        assert_eq!(info("zhipu").unwrap().base_url, "https://open.bigmodel.cn/api/paas/v4");
        assert_eq!(info("moonshot").unwrap().base_url, "https://api.moonshot.cn/v1");
        assert!(PROVIDERS.iter().all(|p| !p.base_url.ends_with('/')));
    }
}
