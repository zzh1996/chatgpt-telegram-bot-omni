//! What the bot knows about a model, for `/model`: labelled facts in sections,
//! taken from the same tables the requests and the cost estimates use (so the
//! card cannot say something the bot does not do).
//!
//! The rows every route shares (reasoning levels and defaults, built-in tools,
//! service tiers, generation options) come from its [`RouteCaps`]
//! ([`ModelCard::caps`]); each provider adds its limits, inputs and prices.

use crate::provider::{Currency, TokenPrice};
use crate::request::{RouteCaps, Tool};

pub const MODEL: &str = "Model";
pub const LIMITS: &str = "Limits";
pub const REASONING: &str = "Reasoning";
pub const INPUT: &str = "Input";
pub const TOOLS: &str = "Tools";
pub const OUTPUT: &str = "Output";
pub const PARAMETERS: &str = "Parameters";
pub const TIERS: &str = "Service tiers";
pub const PRICE: &str = "Price";
pub const NOTES: &str = "Notes";
/// The order of the sections, whatever order the rows come in.
const ORDER: &[&str] = &[MODEL, LIMITS, REASONING, TOOLS, INPUT, OUTPUT, PARAMETERS, TIERS, PRICE, NOTES];

/// The facts, section by section, in the order they were added.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ModelCard {
    pub sections: Vec<Section>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Section {
    pub title: String,
    pub rows: Vec<(String, String)>,
}

impl ModelCard {
    /// A row in `section` (the section is placed in [`ORDER`] the first time).
    pub fn row(&mut self, section: &str, label: impl Into<String>, value: impl Into<String>) -> &mut Self {
        let row = (label.into(), value.into());
        match self.sections.iter_mut().find(|s| s.title == section) {
            Some(s) => s.rows.push(row),
            None => {
                let rank = |t: &str| ORDER.iter().position(|o| *o == t).unwrap_or(ORDER.len());
                let at =
                    self.sections.iter().position(|s| rank(&s.title) > rank(section)).unwrap_or(self.sections.len());
                self.sections.insert(at, Section { title: section.to_string(), rows: vec![row] });
            }
        }
        self
    }

    /// The value of a row (tests and callers that look one up).
    pub fn get(&self, section: &str, label: &str) -> Option<&str> {
        let s = self.sections.iter().find(|s| s.title == section)?;
        s.rows.iter().find(|(l, _)| l == label).map(|(_, v)| v.as_str())
    }

    /// The rows every route shares, from what it offers for the model:
    /// reasoning (levels, the default the bot sends, what `+h` and `+l` mean,
    /// budgets), verbosity, built-in tools, generation options and tiers.
    pub fn caps(&mut self, caps: &dyn RouteCaps) -> &mut Self {
        match caps.efforts().filter(|l| !l.is_empty()) {
            Some(levels) => {
                self.row(REASONING, "levels (e=)", levels.join(", "));
                let default = match caps.default_effort() {
                    Some(d) => format!("{d} (sent explicitly)"),
                    None => "the API's own (nothing sent)".to_string(),
                };
                self.row(REASONING, "default", default);
                self.row(REASONING, "+h", levels[0].clone());
                let low = if caps.can_disable_reasoning() {
                    "none (reasoning off)".to_string()
                } else {
                    levels.last().cloned().unwrap_or_default()
                };
                self.row(REASONING, "+l", low);
            }
            None if caps.fixed_reasoning() => {
                self.row(
                    REASONING,
                    "levels",
                    "none to choose: the model sets its own depth (+h and +l change nothing)",
                );
            }
            None => {}
        }
        if let Some((max, min)) = caps.budget_range() {
            self.row(REASONING, "budget=", format!("{} to {} tokens (+h the most, +l the least)", n(min), n(max)));
        } else if caps.budget() {
            self.row(REASONING, "budget=", "a reasoning token budget can be set");
        }
        if caps.efforts().is_none() && !caps.fixed_reasoning() && caps.can_disable_reasoning() {
            self.row(REASONING, "e=none", "turns reasoning off");
        }
        if caps.verbosity() {
            self.row(OUTPUT, "v=", "low, medium, high (text verbosity)");
        }
        let tools: Vec<String> = [Tool::Search, Tool::Code, Tool::Fetch, Tool::Maps, Tool::Image]
            .into_iter()
            .filter(|t| caps.tool(*t))
            .map(|t| format!("+{} {}", t.letter(), t.describe()))
            .collect();
        if !tools.is_empty() {
            self.row(TOOLS, "built-in tools", tools.join(", "));
        }
        let g = caps.generation();
        let mut options = Vec::new();
        if caps.image_options() {
            options.push("size= / ar= (size and aspect ratio)");
        }
        for (on, what) in [
            (g.quality, "q= (quality)"),
            (g.count, "n= (how many)"),
            (g.background, "bg= (background)"),
            (g.seconds, "sec= (length)"),
            (g.audio, "audio= (sound on or off)"),
            (g.rewrite, "rewrite= (prompt rewriting)"),
        ] {
            if on {
                options.push(what);
            }
        }
        if !options.is_empty() {
            self.row(OUTPUT, "options", options.join(", "));
        }
        let tiers = caps.tiers();
        if !tiers.is_empty() {
            self.row(TIERS, "tier=", tiers.join(", "));
        }
        self
    }

    /// Prices by output size: `(up to, price)` ascending, the last open-ended
    /// (`u64::MAX`), each a row `label` + the pixel range.
    pub fn pixel_tiers(&mut self, tiers: &[(u64, f64)], price: impl Fn(f64) -> String) -> &mut Self {
        let mut low = 0;
        for (i, (up_to, p)) in tiers.iter().enumerate() {
            let label = match (tiers.len(), i, *up_to) {
                (1, _, _) => "per image".to_string(),
                (_, 0, u) => format!("per image up to {} px", n(u)),
                (_, _, u64::MAX) => format!("per image over {} px", n(low)),
                (_, _, u) => format!("per image of {} to {} px", n(low), n(u)),
            };
            self.row(PRICE, label, price(*p));
            low = *up_to;
        }
        self
    }
}

/// A JSON value for a table cell: strings bare, `yes` / `no`, `none` for null,
/// lists comma-separated, anything else on one line.
pub fn value(v: &serde_json::Value) -> String {
    use serde_json::Value;
    match v {
        Value::Null => "none".into(),
        Value::Bool(b) => if *b { "yes" } else { "no" }.into(),
        Value::String(s) => s.clone(),
        Value::Array(a) if a.is_empty() => "none".into(),
        Value::Array(a) if a.iter().all(|x| x.is_string() || x.is_number()) => {
            a.iter().map(value).collect::<Vec<_>>().join(", ")
        }
        Value::Object(o) if o.is_empty() => "none".into(),
        v => v.to_string(),
    }
}

/// `1048576` → `1,048,576`.
pub fn n(v: u64) -> String {
    let s = v.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// `1048576` → `1,048,576 tokens`.
pub fn tokens(v: u64) -> String {
    format!("{} tokens", n(v))
}

/// A price with at least two decimals and no trailing zeros beyond them:
/// `3` → `3.00`, `0.075` → `0.075`.
pub fn amount(v: f64) -> String {
    let mut s = format!("{v:.6}");
    while s.ends_with('0') && s.split('.').nth(1).is_some_and(|d| d.len() > 2) {
        s.pop();
    }
    s
}

/// `$3.00`.
pub fn usd(v: f64) -> String {
    format!("${}", amount(v))
}

/// `¥3.00`.
pub fn cny(v: f64) -> String {
    format!("¥{}", amount(v))
}

/// `$3.00 / 1M tokens`.
pub fn usd_m(v: f64) -> String {
    format!("{} / 1M tokens", usd(v))
}

/// `¥3.00 / 1M tokens`.
pub fn cny_m(v: f64) -> String {
    format!("{} / 1M tokens", cny(v))
}

/// An amount in a price list's currency.
pub fn money(c: Currency, v: f64) -> String {
    match c {
        Currency::Usd => usd(v),
        Currency::Cny => cny(v),
    }
}

/// A token price list on one line: `input ¥1.00, cached ¥0.02, output ¥4.00 / 1M tokens`.
pub fn token_price(c: Currency, p: &TokenPrice) -> String {
    let mut parts = vec![format!("input {}", money(c, p.input))];
    if p.cached_input < p.input {
        parts.push(format!("cached {}", money(c, p.cached_input)));
    }
    if p.cache_write != p.input {
        parts.push(format!("cache write {}", money(c, p.cache_write)));
    }
    parts.push(format!("output {}", money(c, p.output)));
    format!("{} / 1M tokens", parts.join(", "))
}

impl ModelCard {
    /// The note for a price list in CNY: costs are shown in USD at the configured rate.
    pub fn cny_rate(&mut self, cny_per_usd: f64) -> &mut Self {
        self.row(NOTES, "currency", format!("CNY prices; costs are shown in USD at {cny_per_usd} CNY per USD (config)"))
    }
}

/// A Unix time as a UTC date and time (`2026-12-01 00:00 UTC`).
pub fn utc(t: i64) -> String {
    let days = t.div_euclid(86_400);
    let secs = t.rem_euclid(86_400);
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02} UTC", secs / 3600, secs % 3600 / 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_prices_and_dates() {
        assert_eq!(n(1_048_576), "1,048,576");
        assert_eq!(n(999), "999");
        assert_eq!(tokens(128_000), "128,000 tokens");
        assert_eq!(usd(3.0), "$3.00");
        assert_eq!(usd(0.075), "$0.075");
        assert_eq!(usd(12.5), "$12.50");
        assert_eq!(usd(0.0375), "$0.0375");
        assert_eq!(cny_m(0.8), "¥0.80 / 1M tokens");
        assert_eq!(utc(1_796_083_200), "2026-12-01 00:00 UTC");
        assert_eq!(utc(1_803_888_000), "2027-03-01 08:00 UTC");
        assert_eq!(utc(0), "1970-01-01 00:00 UTC");
    }

    /// Every model in every table gets a card with its price (the tables are
    /// what the bot serves, so `/model` has something for each).
    #[test]
    fn every_table_model_has_a_card_with_a_price() {
        use std::time::Duration;
        let cfg = |id: &str| crate::provider::DirectConfig {
            api_key: "k".into(),
            base_url: format!("https://{id}.invalid"),
            connect_timeout: Duration::from_secs(1),
            idle_timeout: Duration::from_secs(1),
            max_tokens_cap: None,
            cny_per_usd: 7.0,
            long_decode_budget: 393_216,
        };
        let tables: Vec<(&str, Vec<&str>)> = vec![
            (
                "openai",
                crate::openai::MODELS
                    .iter()
                    .map(|m| m.id)
                    .chain(crate::openai::images::IMAGE_MODELS.iter().map(|m| m.id))
                    .collect(),
            ),
            ("anthropic", crate::anthropic::models::MODELS.iter().map(|m| m.id).collect()),
            (
                "google",
                crate::google::models::MODELS
                    .iter()
                    .map(|m| m.id)
                    .chain(crate::google::omni::MODELS.iter().map(|m| m.id))
                    .collect(),
            ),
            (
                "xai",
                crate::xai::MODELS
                    .iter()
                    .map(|m| m.id)
                    .chain(crate::xai::imagine::IMAGE_MODELS.iter().map(|m| m.id))
                    .chain(crate::xai::imagine::VIDEO_MODELS.iter().map(|m| m.id))
                    .collect(),
            ),
            ("deepseek", crate::deepseek::MODELS.iter().map(|m| m.id).collect()),
            (
                "zhipu",
                crate::zhipu::MODELS
                    .iter()
                    .map(|m| m.id)
                    .chain(crate::zhipu::images::MODELS.iter().map(|m| m.id))
                    .chain(crate::zhipu::videos::MODELS.iter().map(|m| m.id))
                    .collect(),
            ),
            ("moonshot", crate::moonshot::MODELS.iter().map(|m| m.id).collect()),
            (
                "alibaba",
                crate::alibaba::MODELS
                    .iter()
                    .map(|m| m.id)
                    .chain(crate::alibaba::images::MODELS.iter().map(|m| m.id))
                    .chain(crate::alibaba::video::MODELS.iter().map(|m| m.id))
                    .collect(),
            ),
            (
                "ark",
                crate::ark::MODELS
                    .iter()
                    .map(|m| m.prefix)
                    .chain(crate::ark::images::MODELS.iter().map(|m| m.id))
                    .chain(crate::ark::video::MODELS.iter().map(|m| m.id))
                    .collect(),
            ),
        ];
        let mut n = 0;
        for (provider, ids) in tables {
            let p = crate::provider::build(provider, cfg(provider)).unwrap();
            for id in ids {
                let card = p.card(id).unwrap_or_else(|| panic!("{provider}:{id} has no card"));
                let price = card.sections.iter().find(|s| s.title == PRICE);
                assert!(price.is_some_and(|s| !s.rows.is_empty()), "{provider}:{id} has no price");
                assert!(
                    card.sections
                        .iter()
                        .all(|s| !s.rows.is_empty() && s.rows.iter().all(|(l, v)| !l.is_empty() && !v.is_empty())),
                    "{provider}:{id}"
                );
                n += 1;
            }
            assert!(p.card("no-such-model").is_none(), "{provider}");
        }
        assert!(n > 50, "{n} models");
    }

    #[test]
    fn rows_keep_their_sections_in_order() {
        let mut c = ModelCard::default();
        c.row(PRICE, "input", "2").row(LIMITS, "context", "1").row(LIMITS, "output", "3").row("Other", "x", "4");
        assert_eq!(c.sections.iter().map(|s| s.title.as_str()).collect::<Vec<_>>(), [LIMITS, PRICE, "Other"]);
        assert_eq!(c.sections[0].rows, vec![("context".into(), "1".into()), ("output".into(), "3".into())]);
        assert_eq!(c.get(PRICE, "input"), Some("2"));
    }
}
