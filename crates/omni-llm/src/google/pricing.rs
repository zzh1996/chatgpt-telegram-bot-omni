//! Cost estimate from the Gemini API price list (the API reports no cost).
//!
//! Prices: <https://aistudio.google.com/docs/pricing>, fetched 2026-11-12
//! (Gemini 4 Hydrogen and Gemini 2.5 Flash Image from the page as
//! re-fetched 2026-11-13, Gemini 4 Neon 2026-11-19), paid tier, USD per 1M tokens, by service tier
//! (Standard / Flex / Priority). Dated changes are applied by the date of the call (UTC):
//! Gemini 4 doubles on 2026-12-01 (end of the introductory price), 3.8 / 3.6
//! Flash double on 2027-01-01, and 2.5 Pro / 2.5 Flash are routed to (and
//! billed as) 3.8 Flash / 3.5 Flash-Lite from 2026-12-01.
//!
//! Formula (docs/research/gemini.md §9, derived, not an official formula):
//! the input is `promptTokenCount + toolUsePromptTokenCount`; of it the
//! `cachedContentTokenCount` at the cache rate and the rest at the input rate
//! (audio at its own rates where the model has them). The cached tokens can
//! exceed `promptTokenCount`: with code execution or search the model re-reads
//! the tool results, and those come from the cache too (2026-11-13: prompt
//! 5,003, tool use 321,767, cached 224,091); `candidatesTokenCount + thoughtsTokenCount` at the
//! output rate (image tokens of `candidatesTokensDetails` at the image output
//! rate, see [`estimate`]), the >200K-token rates when `promptTokenCount` exceeds 200,000,
//! the tier actually served (`usageMetadata.serviceTier`).
//!
//! Grounding is priced apart and left out of the cost ([`grounding_fees`]):
//! Gemini 3 and newer $14 per 1,000 executed (unique, non-empty) search
//! queries for Google Search and Google Maps alike; Gemini 2.5 $35 (Search)
//! / $25 (Maps) per 1,000 grounded prompts. The free allowances (5,000
//! queries a month shared by all Gemini 3+ models; 1,500 / 10,000 grounded
//! prompts a day on 2.5) depend on the whole project's usage, which the bot
//! cannot see; the owner's rule (2026-11-13): the fee is shown after the cost
//! (`+ 24 searches ≈$0.336`, what it costs once the allowance is used up) and
//! spend limits do not count it. Storage of explicit caches is not used.

use omni_core::usage::{Counted, Unbilled};
use serde_json::Value;

/// 2026-12-01T00:00:00Z.
pub const DEC_1_2026: i64 = 1_796_083_200;
/// 2027-01-01T00:00:00Z.
pub const JAN_1_2027: i64 = 1_798_761_600;

/// USD per 1M tokens.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rates {
    /// Text, image, video and PDF input (and audio unless `input_audio`).
    pub input: f64,
    pub input_audio: Option<f64>,
    /// Cache reads (implicit caching).
    pub cached: f64,
    pub cached_audio: Option<f64>,
    /// Text and thinking output.
    pub output: f64,
    /// Generated images (image models).
    pub image_output: Option<f64>,
}

const fn r(input: f64, cached: f64, output: f64) -> Rates {
    Rates { input, input_audio: None, cached, cached_audio: None, output, image_output: None }
}

const fn ra(input: f64, audio: f64, cached: f64, cached_audio: f64, output: f64) -> Rates {
    Rates { input, input_audio: Some(audio), cached, cached_audio: Some(cached_audio), output, image_output: None }
}

/// Image models: no context caching (cache reads at the input rate).
const fn ri(input: f64, output: f64, image: f64) -> Rates {
    Rates { input, input_audio: None, cached: input, cached_audio: None, output, image_output: Some(image) }
}

/// Rates for a call, with the >200K-token tier where the model has one.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Price {
    pub base: Rates,
    pub over_200k: Option<Rates>,
    pub grounding: Grounding,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Grounding {
    /// USD per executed search query (Gemini 3 and newer; Search and Maps).
    PerQuery(f64),
    /// USD per grounded prompt (Gemini 2.5): Google Search, Google Maps.
    PerPrompt { search: f64, maps: f64 },
}

const PER_QUERY: Grounding = Grounding::PerQuery(14.0 / 1000.0);
const PER_PROMPT: Grounding = Grounding::PerPrompt { search: 35.0 / 1000.0, maps: 25.0 / 1000.0 };

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    Standard,
    Flex,
    Priority,
}

impl Tier {
    pub fn name(self) -> &'static str {
        match self {
            Tier::Standard => "standard",
            Tier::Flex => "flex",
            Tier::Priority => "priority",
        }
    }

    /// From a request's `service_tier` or a response's `usageMetadata.serviceTier`
    /// (`unspecified` = standard).
    pub fn parse(s: &str) -> Option<Tier> {
        let s = s.to_ascii_lowercase();
        let s = s.strip_prefix("service_tier_").unwrap_or(&s);
        match s {
            "standard" | "unspecified" | "default" => Some(Tier::Standard),
            "flex" => Some(Tier::Flex),
            "priority" => Some(Tier::Priority),
            _ => None,
        }
    }
}

fn single(base: Rates) -> Price {
    Price { base, over_200k: None, grounding: PER_QUERY }
}

/// The price of `model` on `tier` at `now` (Unix seconds); `None` if unknown.
pub fn price(model: &str, tier: Tier, now: i64) -> Option<Price> {
    use Tier::*;
    let intro_g4 = now < DEC_1_2026;
    let intro_flash = now < JAN_1_2027;
    let flash_38 = |tier: Tier| {
        single(match (tier, intro_flash) {
            (Standard, true) => r(0.75, 0.075, 3.75),
            (Flex, true) => r(0.375, 0.0375, 1.875),
            (Priority, true) => r(1.35, 0.135, 6.75),
            (Standard, false) => r(1.50, 0.15, 7.50),
            (Flex, false) => r(0.75, 0.075, 3.75),
            (Priority, false) => r(2.70, 0.27, 13.50),
        })
    };
    let flash_lite_35 = |tier: Tier| {
        single(match tier {
            Standard => r(0.30, 0.03, 2.50),
            Flex => r(0.15, 0.02, 1.25),
            Priority => r(0.54, 0.05, 4.50),
        })
    };
    Some(match model {
        // No introductory price; flat across the 1M window, implicit cache reads at 5%.
        "gemini-4-neon" => single(match tier {
            Standard => r(3.00, 0.15, 15.00),
            Flex => r(1.50, 0.075, 7.50),
            Priority => r(5.40, 0.27, 27.00),
        }),
        "gemini-4-hydrogen" => single(match tier {
            Standard => r(1.50, 0.075, 7.50),
            Flex => r(0.75, 0.0375, 3.75),
            Priority => r(2.70, 0.135, 13.50),
        }),
        "gemini-4-carbon" | "gemini-4-argon" => single(match (tier, intro_g4) {
            (Standard, true) => r(2.00, 0.10, 10.00),
            (Flex, true) => r(1.00, 0.05, 5.00),
            (Priority, true) => r(3.60, 0.18, 18.00),
            (Standard, false) => r(4.00, 0.20, 20.00),
            (Flex, false) => r(2.00, 0.10, 10.00),
            (Priority, false) => r(7.20, 0.36, 36.00),
        }),
        // 3.7 Flash and 3.5 Flash are routed to 3.8 / 3.6 Flash (same price list).
        "gemini-3.8-flash" | "gemini-3.7-flash" | "gemini-3.6-flash" | "gemini-3.5-flash" => flash_38(tier),
        "gemini-3.5-flash-lite" => flash_lite_35(tier),
        "gemini-3.1-pro-preview" => {
            let (base, over) = match tier {
                Standard => (r(2.00, 0.20, 12.00), r(4.00, 0.40, 18.00)),
                // Flex cache reads are "same as Standard".
                Flex => (r(1.00, 0.20, 6.00), r(2.00, 0.40, 9.00)),
                Priority => (r(3.60, 0.36, 21.60), r(7.20, 0.72, 32.40)),
            };
            Price { base, over_200k: Some(over), grounding: PER_QUERY }
        }
        "gemini-3.1-flash-lite" => single(match tier {
            Standard => ra(0.25, 0.50, 0.025, 0.05, 1.50),
            Flex => ra(0.125, 0.25, 0.0125, 0.025, 0.75),
            Priority => ra(0.45, 0.90, 0.045, 0.09, 2.70),
        }),
        "gemini-3-flash-preview" => single(match tier {
            Standard => ra(0.50, 1.00, 0.05, 0.10, 3.00),
            // Flex cache reads are "same as Standard".
            Flex => ra(0.25, 0.50, 0.05, 0.10, 1.50),
            Priority => ra(0.90, 1.80, 0.09, 0.18, 5.40),
        }),
        // Image models: Standard only (no Flex/Priority); images at their own output rate.
        "gemini-nano-banana-2.1" if tier == Standard => single(ri(1.50, 7.50, 30.00)),
        "gemini-3.1-flash-image" if tier == Standard => single(ri(0.50, 3.00, 60.00)),
        "gemini-3.1-flash-lite-image" if tier == Standard => single(ri(0.25, 1.50, 30.00)),
        "gemini-3-pro-image" if tier == Standard => single(ri(2.00, 12.00, 120.00)),
        // "$0.039 per image": $30 per 1M image tokens, 1,290 tokens per image; "Text
        // input and output is priced the same as 2.5 Flash".
        "gemini-2.5-pro" if !intro_g4 => flash_38(tier),
        "gemini-2.5-pro" => {
            let (base, over) = match tier {
                Standard => (r(1.25, 0.125, 10.00), r(2.50, 0.25, 15.00)),
                Flex => (r(0.625, 0.125, 5.00), r(1.25, 0.25, 7.50)),
                Priority => (r(2.25, 0.225, 18.00), r(4.50, 0.45, 27.00)),
            };
            Price { base, over_200k: Some(over), grounding: PER_PROMPT }
        }
        "gemini-2.5-flash" if !intro_g4 => flash_lite_35(tier),
        "gemini-2.5-flash" => Price {
            grounding: PER_PROMPT,
            ..single(match tier {
                Standard => ra(0.30, 1.00, 0.03, 0.10, 2.50),
                Flex => ra(0.15, 0.50, 0.03, 0.10, 1.25),
                Priority => ra(0.54, 1.80, 0.054, 0.18, 4.50),
            })
        },
        "gemini-2.5-flash-lite" => Price {
            grounding: PER_PROMPT,
            ..single(match tier {
                Standard => ra(0.10, 0.30, 0.01, 0.03, 0.40),
                Flex => ra(0.05, 0.15, 0.01, 0.03, 0.20),
                Priority => ra(0.18, 0.54, 0.018, 0.054, 0.72),
            })
        },
        _ => return None,
    })
}

/// Grounding work seen in a response (for the per-query / per-prompt fees).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GroundingUse {
    /// Unique non-empty search queries (`webSearchQueries` + `imageSearchQueries`;
    /// Maps grounding reports its queries there too).
    pub queries: usize,
    /// Web search sources were returned (2.5: a grounded Search prompt).
    pub web_results: bool,
    /// Google Maps sources were returned (2.5: a grounded Maps prompt).
    pub maps_results: bool,
}

fn count(u: &Value, key: &str) -> u64 {
    u.get(key).and_then(Value::as_u64).unwrap_or(0)
}

/// Tokens of one modality in a `*TokensDetails` list.
fn modality(u: &Value, key: &str, m: &str) -> u64 {
    u.get(key)
        .and_then(Value::as_array)
        .map(|l| {
            l.iter()
                .filter(|d| d.get("modality").and_then(Value::as_str) == Some(m))
                .filter_map(|d| d.get("tokenCount").and_then(Value::as_u64))
                .sum()
        })
        .unwrap_or(0)
}

/// Whether a `*TokensDetails` list has an entry for modality `m`.
fn has_modality(u: &Value, key: &str, m: &str) -> bool {
    u.get(key)
        .and_then(Value::as_array)
        .is_some_and(|l| l.iter().any(|d| d.get("modality").and_then(Value::as_str) == Some(m)))
}

/// Estimated cost in USD of one response's `usageMetadata` (`requested`: the
/// tier the request asked for, used when the response does not say).
///
/// Image output follows the price list: an image costs the IMAGE tokens of
/// `candidatesTokensDetails` at the image rate (the list's per-image amounts:
/// 1,120 tokens for a 1K image, 1,680 for 2K, 3,780 / 2,520 for 4K, 747 for
/// 512 px, 1,290 on 2.5 Flash Image), the rest of the answer (its TEXT tokens,
/// or what `candidatesTokenCount` has beyond the image tokens) at the text
/// output rate. UNVERIFIED which count Google bills: on 2026-11-12
/// `gemini-3.1-flash-lite-image` responses reported `candidatesTokenCount` 305
/// and 325 with `candidatesTokensDetails` `[{IMAGE: 1120}]` (and a
/// `totalTokenCount` built from 305); the estimate takes the 1,120 of the list.
///
/// `free_media`: the VIDEO and AUDIO tokens are free (they all come from
/// YouTube links, `super::stream::youtube_only`), read from the cache or not.
/// Grounding fees are not in the estimate: see [`grounding_fees`].
pub fn estimate(model: &str, usage: &Value, requested: Option<Tier>, now: i64, free_media: bool) -> Option<f64> {
    let served = usage.get("serviceTier").and_then(Value::as_str).and_then(Tier::parse);
    let tier = served.or(requested).unwrap_or(Tier::Standard);
    let p = price(model, tier, now)?;
    let prompt = count(usage, "promptTokenCount");
    let rates = match p.over_200k {
        Some(over) if prompt > 200_000 => over,
        _ => p.base,
    };
    // The cached tokens are a part of the whole input, the tool-use prompt included.
    let all_input = prompt + count(usage, "toolUsePromptTokenCount");
    let cached = count(usage, "cachedContentTokenCount").min(all_input);
    let candidates = count(usage, "candidatesTokenCount");
    let thoughts = count(usage, "thoughtsTokenCount");
    // Tokens of these modalities in the input: cached, and in all.
    let of = |kinds: &[&str]| {
        let sum = |key: &str| kinds.iter().map(|m| modality(usage, key, m)).sum::<u64>();
        (sum("cacheTokensDetails").min(cached), sum("promptTokensDetails") + sum("toolUsePromptTokensDetails"))
    };
    let (free_cached, free_all) = if free_media { of(&["VIDEO", "AUDIO"]) } else { (0, 0) };
    let free_uncached = free_all.saturating_sub(free_cached).min(all_input - cached);
    let cached = cached - free_cached;
    let uncached = all_input - free_cached - cached - free_uncached;
    let (audio_cached, audio_all) = if free_media { (0, 0) } else { of(&["AUDIO"]) };
    let audio_uncached = audio_all.saturating_sub(audio_cached).min(uncached);
    let image_out = modality(usage, "candidatesTokensDetails", "IMAGE");
    let text_out = if image_out == 0 {
        candidates
    } else {
        let text = modality(usage, "candidatesTokensDetails", "TEXT");
        let rest = candidates.saturating_sub(image_out);
        if has_modality(usage, "candidatesTokensDetails", "TEXT") { text.max(rest) } else { rest }
    };
    let f = |n: u64| n as f64;
    let input = f(uncached - audio_uncached) * rates.input
        + f(audio_uncached) * rates.input_audio.unwrap_or(rates.input)
        + f(cached - audio_cached) * rates.cached
        + f(audio_cached) * rates.cached_audio.unwrap_or(rates.cached);
    let output = f(text_out + thoughts) * rates.output + f(image_out) * rates.image_output.unwrap_or(rates.output);
    Some((input + output) / 1e6)
}

/// The grounding fees of a response, left out of the cost (owner, 2026-11-13):
/// the free allowances belong to the whole project, which the bot cannot see,
/// so the reply shows what they cost once the allowance is used up and spend
/// limits do not count them. Empty when nothing was grounded or the model is
/// not in the price list.
pub fn grounding_fees(model: &str, now: i64, g: &GroundingUse) -> Vec<Unbilled> {
    // The fees are the same on every service tier.
    let Some(p) = price(model, Tier::Standard, now) else { return Vec::new() };
    match p.grounding {
        Grounding::PerQuery(q) if g.queries > 0 => {
            vec![Unbilled { what: Counted::SearchQueries, count: g.queries as u64, usd: g.queries as f64 * q }]
        }
        Grounding::PerQuery(_) => Vec::new(),
        // A prompt counts as grounded when it returned sources of that kind.
        Grounding::PerPrompt { search, maps } => {
            let fees = [(g.web_results, search), (g.maps_results, maps)];
            let grounded: Vec<f64> = fees.iter().filter(|(used, _)| *used).map(|(_, usd)| *usd).collect();
            if grounded.is_empty() {
                return Vec::new();
            }
            vec![Unbilled { what: Counted::GroundedPrompts, count: grounded.len() as u64, usd: grounded.iter().sum() }]
        }
    }
}
