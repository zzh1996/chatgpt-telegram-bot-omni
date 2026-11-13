//! Claude models on the Anthropic API: limits, thinking and effort options,
//! server-tool versions, fast mode and list prices.
//!
//! Source: the official docs fetched 2026-11-12 (models overview, thinking,
//! effort, extended thinking, fast mode, web search / web fetch / code
//! execution tools, mid-conversation system messages, pricing); summarized
//! in docs/research/anthropic.md §1.2–1.5, §3, §7. Prices are USD per million
//! tokens on the Claude API (first party).

use omni_core::Usage;
use serde_json::Value;

/// How the model's thinking is configured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Thinking {
    /// `thinking: {type: "adaptive"}` with `output_config.effort`. `budget`:
    /// manual `{type: "enabled", budget_tokens}` is still accepted
    /// (deprecated on Opus 4.6 and Sonnet 4.6, a 400 on 4.7 and later).
    Adaptive { budget: bool },
    /// Only manual thinking, `{type: "enabled", budget_tokens}` (the 4.5
    /// generation; `adaptive` is a 400 there).
    Budget,
}

/// How `e=none` (reasoning off) is sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Off {
    /// Thinking cannot be turned off (Fable/Mythos 5.x, Opus 5.5: `disabled` is a 400).
    Never,
    /// `thinking: {type: "disabled"}` (Opus 5 and Haiku 5.5 accept it only at
    /// effort `high` or below: the effort is then left at the API default,
    /// `high` and `medium`).
    Disabled,
    /// `thinking: {type: "between_tools"}`: no up-front thinking, only the
    /// progress notes between tool calls (Sonnet 5.5, whose `disabled` is a
    /// 400; the docs call it "the lowest thinking setting" and the way "to turn
    /// thinking off"). It takes no other field and needs effort `high` or below
    /// (the API default `high` applies).
    BetweenTools,
}

/// Fast mode (`speed: "fast"`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Fast {
    /// USD per MTok (cache multipliers apply on top).
    pub input: f64,
    pub output: f64,
    /// Needs the `fast-mode-2026-02-01` beta header (research preview on
    /// Opus 5, deprecated on Opus 4.8).
    pub beta: bool,
}

/// Higher prices for long prompts (Haiku 5.5).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LongTier {
    /// Prompts (all input tokens, cache reads and writes included) above this
    /// many tokens pay the higher prices.
    pub above: u64,
    pub input: f64,
    pub output: f64,
}

/// What the bot knows about one Claude model.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ModelSpec {
    /// Claude API id. Dated snapshots of it (`<id>-YYYYMMDD`) match too.
    pub id: &'static str,
    pub context: u64,
    /// Max `max_tokens` on the Messages API (thinking included).
    pub max_output: u64,
    pub thinking: Thinking,
    /// `output_config.effort` levels, highest first (empty: no effort parameter).
    pub efforts: &'static [&'static str],
    /// The default level: `high` on most models, `medium` on Opus 5.5 and
    /// Haiku 5.5 (effort docs); `none` where thinking needs a budget (Opus 4.5,
    /// Haiku 4.5: no thinking unless asked). Opus/Sonnet 4.6–4.8 run without
    /// thinking when it is omitted, but Anthropic recommends adaptive thinking:
    /// the bot turns it on at the default level.
    pub effort: &'static str,
    pub off: Off,
    /// `thinking.display` defaults to `"omitted"` (empty thinking text): the
    /// bot sends `"summarized"`, the most detailed display the API has. On
    /// older models `summarized` is already the default and nothing is sent.
    pub display_omitted: bool,
    /// The web tools with dynamic filtering (`web_search_20260318`,
    /// `web_fetch_20260318`; Claude 4.6 and later); else the basic versions.
    pub dynamic_web: bool,
    /// Manual thinking interleaves between tool calls only with the
    /// `interleaved-thinking-2025-05-14` beta header (Opus 4.5, Sonnet 4.5;
    /// deprecated but working on Sonnet 4.6).
    pub interleaved_beta: bool,
    /// Accepts `role: "system"` messages after the start of the conversation.
    pub mid_system: bool,
    pub fast: Option<Fast>,
    /// USD per MTok.
    pub input: f64,
    pub output: f64,
    /// Cache reads as a multiple of the input price (writes: 1.25× for 5
    /// minutes, 2× for 1 hour, on every model).
    pub cache_read: f64,
    pub long: Option<LongTier>,
    /// `inference_geo: "us"` costs 1.1× on all token prices (Claude 4.6 and later).
    pub us_geo_premium: bool,
}

const ALL: &[&str] = &["max", "xhigh", "high", "medium", "low"];
/// Opus 4.6 and Sonnet 4.6 have no `xhigh`.
const NO_XHIGH: &[&str] = &["max", "high", "medium", "low"];
/// Opus 4.5: effort composes with the thinking budget; `high` is the highest.
const OPUS_45: &[&str] = &["high", "medium", "low"];

/// The Claude 5.x surface: adaptive thinking only, all effort levels, thinking
/// hidden unless `display: "summarized"`, dynamic-filtering web tools, 1M
/// context, 128K output.
const MODERN: ModelSpec = ModelSpec {
    id: "",
    context: 1_000_000,
    max_output: 128_000,
    thinking: Thinking::Adaptive { budget: false },
    efforts: ALL,
    effort: "high",
    off: Off::Never,
    display_omitted: true,
    dynamic_web: true,
    interleaved_beta: false,
    mid_system: true,
    fast: None,
    input: 0.0,
    output: 0.0,
    cache_read: 0.1,
    long: None,
    us_geo_premium: true,
};

/// The 4.5 generation: 200K context, 64K output, manual thinking only, basic web tools.
const GEN_45: ModelSpec = ModelSpec {
    context: 200_000,
    max_output: 64_000,
    thinking: Thinking::Budget,
    efforts: &[],
    effort: "none",
    off: Off::Disabled,
    display_omitted: false,
    dynamic_web: false,
    interleaved_beta: true,
    mid_system: false,
    us_geo_premium: false,
    ..MODERN
};

/// Every model the bot knows (2026-11-12). Retired models (Opus 4.1 and
/// earlier, Sonnet 4, Claude 3.x) are not listed; Claude Mythos Preview is
/// left out (invitation-only, its own rules).
pub const MODELS: &[ModelSpec] = &[
    // Fable / Mythos 5.2: always-on thinking, default effort high, fast mode $16/$80.
    ModelSpec {
        id: "claude-fable-5-2",
        fast: Some(Fast { input: 16.0, output: 80.0, beta: false }),
        input: 8.0,
        output: 40.0,
        cache_read: 0.025,
        ..MODERN
    },
    ModelSpec {
        id: "claude-mythos-5-2",
        fast: Some(Fast { input: 16.0, output: 80.0, beta: false }),
        input: 8.0,
        output: 40.0,
        cache_read: 0.025,
        ..MODERN
    },
    ModelSpec { id: "claude-fable-5-1", input: 10.0, output: 50.0, cache_read: 0.025, ..MODERN },
    ModelSpec { id: "claude-mythos-5-1", input: 10.0, output: 50.0, cache_read: 0.025, ..MODERN },
    ModelSpec { id: "claude-fable-5", input: 10.0, output: 50.0, ..MODERN },
    ModelSpec { id: "claude-mythos-5", input: 10.0, output: 50.0, ..MODERN },
    // Opus 5.5: thinking always on, default effort medium, fast mode $8/$40 (no beta).
    ModelSpec {
        id: "claude-opus-5-5",
        effort: "medium",
        fast: Some(Fast { input: 8.0, output: 40.0, beta: false }),
        input: 4.0,
        output: 20.0,
        cache_read: 0.05,
        ..MODERN
    },
    // Opus 5: `disabled` at effort high or below; fast mode research preview (beta, access needed).
    ModelSpec {
        id: "claude-opus-5",
        off: Off::Disabled,
        fast: Some(Fast { input: 10.0, output: 50.0, beta: true }),
        input: 5.0,
        output: 25.0,
        ..MODERN
    },
    // Opus 4.8: thinking off unless set; fast mode deprecated (beta), removed on 2026-11-30.
    ModelSpec {
        id: "claude-opus-4-8",
        off: Off::Disabled,
        fast: Some(Fast { input: 10.0, output: 50.0, beta: true }),
        input: 5.0,
        output: 25.0,
        ..MODERN
    },
    ModelSpec { id: "claude-opus-4-7", off: Off::Disabled, mid_system: false, input: 5.0, output: 25.0, ..MODERN },
    ModelSpec {
        id: "claude-opus-4-6",
        thinking: Thinking::Adaptive { budget: true },
        efforts: NO_XHIGH,
        off: Off::Disabled,
        display_omitted: false,
        // Manual mode on Opus 4.6 never interleaves.
        interleaved_beta: false,
        mid_system: false,
        input: 5.0,
        output: 25.0,
        ..MODERN
    },
    ModelSpec { id: "claude-opus-4-5", efforts: OPUS_45, input: 5.0, output: 25.0, ..GEN_45 },
    // Sonnet 5.5: `between_tools` is its thinking-off setting.
    ModelSpec { id: "claude-sonnet-5-5", off: Off::BetweenTools, input: 2.0, output: 10.0, cache_read: 0.05, ..MODERN },
    ModelSpec { id: "claude-sonnet-5", off: Off::Disabled, mid_system: false, input: 2.0, output: 10.0, ..MODERN },
    ModelSpec {
        id: "claude-sonnet-4-6",
        thinking: Thinking::Adaptive { budget: true },
        efforts: NO_XHIGH,
        off: Off::Disabled,
        display_omitted: false,
        interleaved_beta: true,
        mid_system: false,
        input: 3.0,
        output: 15.0,
        ..MODERN
    },
    // Haiku 5.5: `disabled` at effort high or below; prompts over 100K tokens cost 5×.
    ModelSpec {
        id: "claude-haiku-5-5",
        effort: "medium",
        off: Off::Disabled,
        input: 0.10,
        output: 0.50,
        long: Some(LongTier { above: 100_000, input: 0.50, output: 2.50 }),
        ..MODERN
    },
    // Haiku 4.5: no effort parameter, no interleaved thinking.
    ModelSpec { id: "claude-haiku-4-5", interleaved_beta: false, input: 1.0, output: 5.0, ..GEN_45 },
];

/// The spec of `model`: its exact id or a dated snapshot of it
/// (`claude-opus-4-5-20251101`). `claude-opus-5-6` does not match `claude-opus-5`.
pub fn model_spec(model: &str) -> Option<&'static ModelSpec> {
    MODELS.iter().find(|m| {
        model == m.id
            || model
                .strip_prefix(m.id)
                .and_then(|rest| rest.strip_prefix('-'))
                .is_some_and(|d| d.len() == 8 && d.bytes().all(|b| b.is_ascii_digit()))
    })
}

/// Web search: $10 per 1,000 searches (failed searches are not billed).
pub const WEB_SEARCH_USD: f64 = 0.01;

/// Estimated cost in USD of one request, from Anthropic's usage object
/// (`u.raw`; `u` as built by `usage_from_anthropic`: `input_tokens` is the
/// whole prompt, cache reads and writes included). `None` when it cannot be
/// priced: no token counts, Priority Tier (billed against a commitment), or a
/// speed the price list does not have.
pub fn estimate_cost(spec: &ModelSpec, u: &Usage) -> Option<f64> {
    let raw = &u.raw;
    let s = |p: &str| raw.pointer(p).and_then(Value::as_str);
    if s("/service_tier") == Some("priority") {
        return None;
    }
    let prompt = u.input_tokens?;
    let output = u.output_tokens? as f64;
    let read = u.cached_input_tokens.unwrap_or(0);
    let written = u.cache_write_tokens.unwrap_or(0);
    // The TTL breakdown arrives with message_start; writes added later (server
    // tools add 5-minute breakpoints after their results) count as 5-minute.
    let written_1h =
        raw.pointer("/cache_creation/ephemeral_1h_input_tokens").and_then(Value::as_u64).unwrap_or(0).min(written);
    let written_5m = written - written_1h;
    let uncached = prompt.saturating_sub(read + written);
    let (input, out) = match (s("/speed"), spec.fast, spec.long) {
        (Some("fast"), Some(f), _) => (f.input, f.output),
        (Some("fast"), None, _) => return None,
        (_, _, Some(l)) if prompt > l.above => (l.input, l.output),
        _ => (spec.input, spec.output),
    };
    let tokens = uncached as f64 * input
        + read as f64 * input * spec.cache_read
        + written_5m as f64 * input * 1.25
        + written_1h as f64 * input * 2.0
        + output * out;
    let geo = if spec.us_geo_premium && s("/inference_geo") == Some("us") { 1.1 } else { 1.0 };
    let searches = raw.pointer("/server_tool_use/web_search_requests").and_then(Value::as_u64).unwrap_or(0);
    Some(tokens * geo / 1e6 + searches as f64 * WEB_SEARCH_USD)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn usage(raw: Value) -> Usage {
        crate::openrouter::messages::usage_from_anthropic(&raw)
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-12
    }

    #[test]
    fn ids_and_snapshots_match_exactly() {
        assert_eq!(model_spec("claude-opus-5-5").unwrap().id, "claude-opus-5-5");
        assert_eq!(model_spec("claude-opus-5").unwrap().id, "claude-opus-5");
        assert_eq!(model_spec("claude-opus-4-5-20251101").unwrap().id, "claude-opus-4-5");
        assert_eq!(model_spec("claude-haiku-4-5-20251001").unwrap().max_output, 64_000);
        // Retiring models are not in the table (owner's rule).
        assert!(model_spec("claude-sonnet-4-5-20250929").is_none());
        // A newer model is not taken for an older one with a shorter id.
        assert!(model_spec("claude-opus-5-6").is_none());
        assert!(model_spec("claude-fable-5-3").is_none());
        assert!(model_spec("claude-opus-5-5-fast").is_none());
        assert!(model_spec("claude-3-opus-20240229").is_none());
        assert!(model_spec("anthropic/claude-opus-5.5").is_none(), "OpenRouter slugs are not API ids");
    }

    /// The official ids of the configured OpenRouter aliases (config/omni.example.toml).
    #[test]
    fn covers_the_configured_aliases() {
        for (slug, id) in [
            ("anthropic/claude-opus-5.5", "claude-opus-5-5"),
            ("anthropic/claude-opus-5", "claude-opus-5"),
            ("anthropic/claude-fable-5.2", "claude-fable-5-2"),
            ("anthropic/claude-fable-5.1", "claude-fable-5-1"),
            ("anthropic/claude-fable-5", "claude-fable-5"),
            ("anthropic/claude-sonnet-5.5", "claude-sonnet-5-5"),
            ("anthropic/claude-sonnet-5", "claude-sonnet-5"),
            ("anthropic/claude-haiku-5.5", "claude-haiku-5-5"),
        ] {
            assert!(model_spec(id).is_some(), "{slug} -> {id}");
        }
    }

    #[test]
    fn table_is_consistent() {
        for m in MODELS {
            assert!(m.max_output <= m.context, "{}", m.id);
            assert!(m.input > 0.0 && m.output > m.input, "{}", m.id);
            assert!([0.1, 0.05, 0.025].contains(&m.cache_read), "{}", m.id);
            if m.thinking == Thinking::Budget {
                assert_eq!(m.off, Off::Disabled, "{}", m.id);
                assert!(!m.display_omitted && !m.dynamic_web, "{}", m.id);
            }
            if let Some(f) = m.fast {
                assert!(close(f.input, 2.0 * m.input) && close(f.output, 2.0 * m.output), "{}: fast mode is 2×", m.id);
            }
            assert!(MODELS.iter().filter(|x| x.id == m.id).count() == 1, "{}", m.id);
        }
    }

    #[test]
    fn standard_price_with_cache_reads_and_writes() {
        let opus = model_spec("claude-opus-5-5").unwrap();
        // 1M uncached + 2M cache reads + 1M 5-minute writes + 0.5M 1-hour writes, 0.1M output.
        let u = usage(json!({
            "input_tokens": 1_000_000, "cache_read_input_tokens": 2_000_000,
            "cache_creation_input_tokens": 1_500_000,
            "cache_creation": {"ephemeral_5m_input_tokens": 1_000_000, "ephemeral_1h_input_tokens": 500_000},
            "output_tokens": 100_000, "speed": "standard", "service_tier": "standard"
        }));
        let c = estimate_cost(opus, &u).unwrap();
        // $4 + 2 × $0.20 + $5 + 0.5 × $8 + 0.1 × $20
        assert!(close(c, 4.0 + 0.4 + 5.0 + 4.0 + 2.0), "{c}");
    }

    #[test]
    fn fast_mode_long_prompts_geo_and_search_fees() {
        let fable = model_spec("claude-fable-5-2").unwrap();
        let fast = usage(json!({"input_tokens": 1_000_000, "output_tokens": 1_000_000, "speed": "fast"}));
        assert!(close(estimate_cost(fable, &fast).unwrap(), 16.0 + 80.0));
        // Cache reads at 0.025× of the fast input price.
        let fast_read = usage(
            json!({"input_tokens": 0, "cache_read_input_tokens": 1_000_000, "output_tokens": 0, "speed": "fast"}),
        );
        assert!(close(estimate_cost(fable, &fast_read).unwrap(), 0.40));
        // A model without fast mode cannot be priced at fast speed.
        let sonnet = model_spec("claude-sonnet-5-5").unwrap();
        assert_eq!(estimate_cost(sonnet, &fast), None);

        let haiku = model_spec("claude-haiku-5-5").unwrap();
        let short = usage(json!({"input_tokens": 100_000, "output_tokens": 1_000_000}));
        assert!(close(estimate_cost(haiku, &short).unwrap(), 0.01 + 0.50));
        // Over 100K prompt tokens (cache reads count): every token at the higher prices.
        let long = usage(json!({"input_tokens": 1, "cache_read_input_tokens": 100_000, "output_tokens": 1_000_000}));
        assert!(close(estimate_cost(haiku, &long).unwrap(), (0.50 + 100_000.0 * 0.05) / 1e6 + 2.50));

        let us = usage(json!({"input_tokens": 1_000_000, "output_tokens": 0, "inference_geo": "us"}));
        assert!(close(estimate_cost(sonnet, &us).unwrap(), 2.0 * 1.1));
        // Before Claude 4.6 there is no geo premium.
        assert!(close(estimate_cost(model_spec("claude-opus-4-5").unwrap(), &us).unwrap(), 5.0));

        let searched =
            usage(json!({"input_tokens": 0, "output_tokens": 0, "server_tool_use": {"web_search_requests": 3}}));
        assert!(close(estimate_cost(sonnet, &searched).unwrap(), 0.03));
        let priority = usage(json!({"input_tokens": 10, "output_tokens": 10, "service_tier": "priority"}));
        assert_eq!(estimate_cost(model_spec("claude-opus-4-8").unwrap(), &priority), None);
    }

    /// OpenRouter's reported cost of two recorded Claude Haiku 5.5 calls
    /// (tests/fixtures/errors/msg_*.json, its `upstream_inference_cost`) agrees
    /// with this price list.
    #[test]
    fn matches_recorded_costs() {
        let haiku = model_spec("claude-haiku-5-5").unwrap();
        for (input, output, cost) in [(23, 16, 1.03e-05), (10, 14, 8e-06)] {
            let u = usage(
                json!({"input_tokens": input, "output_tokens": output, "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0}),
            );
            assert!(close(estimate_cost(haiku, &u).unwrap(), cost));
        }
    }
}
