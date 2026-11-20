//! The Gemini API models the bot knows: limits, thinking controls, built-in
//! tools, service tiers, input modalities and image output options.
//!
//! Sources (fetched 2026-11-12, re-checked 2026-11-13): the model pages
//! `https://aistudio.google.com/docs/models/<id>`, the thinking guide (levels
//! per model), the image generation guide (sizes, aspect ratios, thinking
//! levels of the image models), the Maps / Search / URL context guides
//! (supported models), the deprecation page (auto-routed ids) and
//! docs/research/gemini.md §0.1. Gemini 4 Hydrogen: model page and "What's new
//! in Gemini 4 Hydrogen" (GA 2026-11-12). Gemini 4 Neon: model page and "What's
//! new in Gemini 4 Neon" (GA 2026-11-18; "sits between Gemini 4 Hydrogen and
//! Gemini 4 Carbon"), fetched 2026-11-19.
//!
//! **Image generation** on the Gemini API is the Nano Banana models only
//! (`generateContent`, images as parts; edits are conversational: earlier
//! images and their thought signatures are replayed). Imagen (`:predict`) is
//! shut down: the last Imagen 4 ids (`imagen-4.0-{generate,ultra-generate,
//! fast-generate}-001`) on 2026-08-17 (deprecations page), and the Imagen
//! guide reads "Imagen models are shut down. Use Nano Banana for image
//! generation" (2026-11-13). The image models take no image count
//! (`candidateCount` is unsupported on Gemini 3 and later; "The model won't
//! always follow the exact number of image outputs that the user explicitly
//! asks for"), no quality and no background option, and `imageConfig` has
//! only `aspectRatio` and `imageSize` (no person-generation or safety
//! setting; the adjustable safety filters are off by default since 2.5).
//! **Video generation** is not in this table: Gemini Omni Flash runs on the
//! Interactions API ([`super::omni`]), and Veo is shut down.
//!
//! **Tool traces**: on Gemini 3 and 4 the built-in tools' own calls and
//! results can come back as parts (`toolConfig.includeServerSideToolInvocations`,
//! preview; [`ModelSpec::tool_invocations`]); Nano Banana 2.1 and 3.1 Flash
//! Image search images as well as the web ([`ModelSpec::image_search`]).
//!
//! Ids are matched exactly: Google's ids are short and many share prefixes
//! (`gemini-3.8-flash` vs `gemini-3.8-flash-tts`). Unknown ids get a
//! *family* guess ([`spec`]) so that newly released models work with
//! `allow_unlisted_models` (with a warning that limits and price are unknown).

use crate::request::{RouteCaps, Tool};

/// Model generation, for defaults of unknown ids and for the fee rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    Gemini4,
    Gemini3,
    Gemini25,
    Other,
}

/// How a model's thinking is controlled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Thinking {
    /// `thinkingConfig.thinkingLevel`, levels highest first (Gemini 3 and 4;
    /// none of them can turn thinking off).
    Levels(&'static [&'static str]),
    /// `thinkingConfig.thinkingBudget` (Gemini 2.5): range, and whether `0`
    /// turns thinking off.
    Budget { min: u64, max: u64, can_disable: bool },
    /// Always thinks, nothing to set but `includeThoughts` (Gemini 3 Pro Image).
    AlwaysOn,
    /// No thinking at all (Gemini 2.5 Flash Image): nothing is sent.
    Unsupported,
    /// Unknown model outside the Gemini 3/4 families: only what the user asks for is sent.
    Unknown,
}

/// Input modalities beyond text (text files are taken as `text/plain`-like
/// inline data by every model).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Inputs {
    pub image: bool,
    pub pdf: bool,
    pub audio: bool,
    pub video: bool,
    /// Office / OpenDocument files (docx, pptx, xlsx, odt, …).
    pub office: bool,
}

/// Image output options of an image model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageOut {
    /// Sizes, smallest first (the last is the largest).
    pub sizes: &'static [&'static str],
    pub ratios: &'static [&'static str],
    /// The size sent when none is asked for (D79: Google's default, 1K, is
    /// not sent; a larger size at the same price is sent instead).
    pub free_size: Option<&'static str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelSpec {
    pub id: &'static str,
    pub family: Family,
    /// Input token limit; for `unified` models the window input and output share.
    pub context: u64,
    /// Output tokens per request (Gemini 4: per streamed hop).
    pub max_output: u64,
    /// Input, thinking and output share `context` (Gemini 4); output beyond
    /// `max_output` comes in long-decode hops.
    pub unified: bool,
    pub long_decode: bool,
    pub thinking: Thinking,
    /// The default `thinkingLevel` (the thinking guide's per-model table, `*`);
    /// `None`: nothing is sent and Gemini decides.
    pub default_level: Option<&'static str>,
    pub tools: &'static [Tool],
    /// Google Search and Google Maps grounding in one request ("Gemini 3.5
    /// Flash and later").
    pub search_with_maps: bool,
    /// Google Maps grounding searches "near me" around the position the bot
    /// sends (`toolConfig.retrievalConfig.latLng`). Gemini 4 Carbon ignores it
    /// (tested 2026-11-20: with a position in Beijing its "near me" searches
    /// came back from the country Google places the bot's server in; named
    /// places work), so a location there is an error (owner's choice).
    pub maps_position: bool,
    /// The response can carry the built-in tools' own calls and results
    /// (`toolCall` / `toolResponse` parts) with
    /// `toolConfig.includeServerSideToolInvocations` (preview, "Gemini 4 and
    /// Gemini 3 models": the tool-combination guide; the image models are not
    /// named there).
    pub tool_invocations: bool,
    /// Google Search for images besides web search (`googleSearch.searchTypes`:
    /// "only available for the Gemini Nano Banana 2.1 and Gemini 3.1 Flash
    /// Image models").
    pub image_search: bool,
    /// Service tiers besides the default (`service_tier`).
    pub tiers: &'static [&'static str],
    pub inputs: Inputs,
    pub image_out: Option<ImageOut>,
    /// Per-part `mediaResolution` (Gemini 3 and 4, not the image generation models).
    pub per_part_media_resolution: bool,
    /// In the table (limits and price known); `false` for a family guess.
    pub known: bool,
}

const ALL_TOOLS: &[Tool] = &[Tool::Search, Tool::Code, Tool::Fetch, Tool::Maps];
const SEARCH_ONLY: &[Tool] = &[Tool::Search];
const NO_TOOLS: &[Tool] = &[];
const TIERS: &[&str] = &["flex", "priority"];
const NO_TIERS: &[&str] = &[];

const LEVELS_HML: &[&str] = &["high", "medium", "low"];
/// Gemini 4 Hydrogen: "a fourth thinking level, `xhigh`, alongside `low`,
/// `medium`, and the default `high`"; `minimal` is rejected. Gemini 4 Neon the
/// same ("`low`, `medium`, `high` (default), and `xhigh`"; "rejects `minimal`
/// with an error").
const LEVELS_XHML: &[&str] = &["xhigh", "high", "medium", "low"];
const LEVELS_ALL: &[&str] = &["high", "medium", "low", "minimal"];

const FULL: Inputs = Inputs { image: true, pdf: true, audio: true, video: true, office: true };
/// Nano Banana 2.1, 3.1 Flash Image, 3.1 Flash Lite Image: "Text, Image, Video, and PDF".
const IMAGE_MODEL: Inputs = Inputs { image: true, pdf: true, audio: false, video: true, office: false };
/// Gemini 3 Pro Image: "Image and Text".
const PRO_IMAGE: Inputs = Inputs { image: true, pdf: false, audio: false, video: false, office: false };

pub const RATIOS_BASE: &[&str] = &["1:1", "2:3", "3:2", "3:4", "4:3", "4:5", "5:4", "9:16", "16:9", "21:9"];
pub const RATIOS_EXTENDED: &[&str] =
    &["1:1", "1:4", "1:8", "2:3", "3:2", "3:4", "4:1", "4:3", "4:5", "5:4", "8:1", "9:16", "16:9", "21:9"];

/// A Gemini 3/4 text model with every built-in tool and both tiers.
const fn text_model(
    id: &'static str,
    family: Family,
    levels: &'static [&'static str],
    search_with_maps: bool,
) -> ModelSpec {
    ModelSpec {
        id,
        family,
        context: 1_048_576,
        max_output: 65_536,
        unified: false,
        long_decode: false,
        thinking: Thinking::Levels(levels),
        default_level: None,
        tools: ALL_TOOLS,
        search_with_maps,
        maps_position: true,
        tool_invocations: matches!(family, Family::Gemini3 | Family::Gemini4),
        image_search: false,
        tiers: TIERS,
        inputs: FULL,
        image_out: None,
        per_part_media_resolution: true,
        known: true,
    }
}

const fn gemini4(id: &'static str) -> ModelSpec {
    ModelSpec { unified: true, long_decode: true, ..text_model(id, Family::Gemini4, LEVELS_HML, true) }
}

const fn image_model(
    id: &'static str,
    context: u64,
    max_output: u64,
    thinking: Thinking,
    tools: &'static [Tool],
    inputs: Inputs,
    image_out: ImageOut,
) -> ModelSpec {
    ModelSpec {
        id,
        family: Family::Gemini3,
        context,
        max_output,
        unified: false,
        long_decode: false,
        thinking,
        default_level: None,
        tools,
        search_with_maps: false,
        maps_position: false,
        tool_invocations: false,
        image_search: false,
        tiers: NO_TIERS,
        inputs,
        image_out: Some(image_out),
        per_part_media_resolution: false,
        known: true,
    }
}

const GEMINI_38_FLASH: ModelSpec =
    ModelSpec { default_level: Some("medium"), ..text_model("gemini-3.8-flash", Family::Gemini3, LEVELS_HML, true) };
const GEMINI_36_FLASH: ModelSpec =
    ModelSpec { default_level: Some("medium"), ..text_model("gemini-3.6-flash", Family::Gemini3, LEVELS_ALL, true) };

/// The table (2026-11-12). Levels from the thinking guide's per-model table;
/// tools from the model pages' capability lists and the tools' "supported
/// models" tables; Search together with Maps on "Gemini 3.5 Flash and later".
pub const MODELS: &[ModelSpec] = &[
    ModelSpec { thinking: Thinking::Levels(LEVELS_XHML), default_level: Some("high"), ..gemini4("gemini-4-neon") },
    ModelSpec { thinking: Thinking::Levels(LEVELS_XHML), default_level: Some("high"), ..gemini4("gemini-4-hydrogen") },
    ModelSpec { default_level: Some("medium"), maps_position: false, ..gemini4("gemini-4-carbon") },
    ModelSpec { default_level: Some("medium"), ..gemini4("gemini-4-argon") },
    GEMINI_38_FLASH,
    GEMINI_36_FLASH,
    // "Gemini 3.5 Flash and later" for Search + Maps: taken to include 3.5 Flash-Lite
    // (UNVERIFIED; Google answers with an error if not).
    ModelSpec {
        default_level: Some("minimal"),
        ..text_model("gemini-3.5-flash-lite", Family::Gemini3, LEVELS_ALL, true)
    },
    ModelSpec {
        default_level: Some("high"),
        ..text_model("gemini-3.1-pro-preview", Family::Gemini3, LEVELS_HML, false)
    },
    ModelSpec {
        default_level: Some("minimal"),
        ..text_model("gemini-3.1-flash-lite", Family::Gemini3, LEVELS_ALL, false)
    },
    ModelSpec {
        default_level: Some("high"),
        ..text_model("gemini-3-flash-preview", Family::Gemini3, LEVELS_ALL, false)
    },
    ModelSpec {
        image_search: true,
        default_level: Some("medium"),
        ..image_model(
            "gemini-nano-banana-2.1",
            131_072,
            32_768,
            Thinking::Levels(&["high", "medium", "minimal"]),
            SEARCH_ONLY,
            IMAGE_MODEL,
            ImageOut { sizes: &["1K", "2K", "4K"], ratios: RATIOS_EXTENDED, free_size: None },
        )
    },
    ModelSpec {
        image_search: true,
        default_level: Some("minimal"),
        ..image_model(
            "gemini-3.1-flash-image",
            131_072,
            32_768,
            Thinking::Levels(&["high", "minimal"]),
            SEARCH_ONLY,
            IMAGE_MODEL,
            ImageOut { sizes: &["512", "1K", "2K", "4K"], ratios: RATIOS_EXTENDED, free_size: None },
        )
    },
    // "Supported image_size values: 1024px (1K). (Note: 2K and 4K are unsupported)";
    // the model page lists the ten base ratios (while also saying "14").
    ModelSpec {
        default_level: Some("minimal"),
        ..image_model(
            "gemini-3.1-flash-lite-image",
            65_536,
            4_096,
            Thinking::Levels(&["high", "minimal"]),
            NO_TOOLS,
            IMAGE_MODEL,
            ImageOut { sizes: &["1K"], ratios: RATIOS_BASE, free_size: None },
        )
    },
    image_model(
        "gemini-3-pro-image",
        65_536,
        32_768,
        Thinking::AlwaysOn,
        SEARCH_ONLY,
        PRO_IMAGE,
        // 1K and 2K cost the same ($0.134, pricing page): 2K by default.
        ImageOut { sizes: &["1K", "2K", "4K"], ratios: RATIOS_BASE, free_size: Some("2K") },
    ),
];

/// The model's spec: the table entry, else a guess from its family.
pub fn spec(model: &str) -> ModelSpec {
    MODELS.iter().find(|m| m.id == model).copied().unwrap_or_else(|| guess(model))
}

pub fn known(model: &str) -> bool {
    MODELS.iter().any(|m| m.id == model)
}

/// Defaults for an id that is not in the table, from its family: Gemini 4
/// (thinking levels, unified 1M window, long decode), Gemini 3 (thinking
/// levels), anything else (only what the user asks for). Everything is
/// allowed: Google validates what the model really supports.
fn guess(model: &str) -> ModelSpec {
    let family = if model.starts_with("gemini-4") {
        Family::Gemini4
    } else if model.starts_with("gemini-3") {
        Family::Gemini3
    } else if model.starts_with("gemini-2.5") {
        Family::Gemini25
    } else {
        Family::Other
    };
    let image = model.contains("image") || model.contains("nano-banana");
    let thinking = match family {
        Family::Gemini4 => Thinking::Levels(LEVELS_HML),
        Family::Gemini3 => Thinking::Levels(LEVELS_ALL),
        _ => Thinking::Unknown,
    };
    let gemini4 = family == Family::Gemini4;
    ModelSpec {
        id: "",
        family,
        context: if gemini4 { 1_048_576 } else { 0 },
        max_output: if gemini4 { 65_536 } else { 0 },
        unified: gemini4,
        long_decode: gemini4,
        thinking,
        default_level: None,
        tools: ALL_TOOLS,
        maps_position: true,
        search_with_maps: true,
        tool_invocations: matches!(family, Family::Gemini4 | Family::Gemini3) && !image,
        image_search: false,
        tiers: TIERS,
        inputs: FULL,
        image_out: image.then_some(ImageOut {
            sizes: &["512", "1K", "2K", "4K"],
            ratios: RATIOS_EXTENDED,
            free_size: None,
        }),
        per_part_media_resolution: matches!(family, Family::Gemini4 | Family::Gemini3) && !image,
        known: false,
    }
}

/// What `/model` shows for a model of the table (`budget`: the long-decode
/// budget the bot asks for across continuations).
pub fn card(s: &ModelSpec, budget: u64) -> crate::card::ModelCard {
    use super::pricing::{self, Grounding, Rates, Tier};
    use crate::card::{self, INPUT, LIMITS, OUTPUT, PRICE, REASONING, TOOLS, usd};
    let mut c = card::ModelCard::default();
    let shared = if s.unified { " (input, thinking and output share it)" } else { "" };
    c.row(LIMITS, "context window", format!("{}{shared}", card::tokens(s.context)));
    c.row(LIMITS, "max output", format!("{} per request", card::tokens(s.max_output)));
    if s.long_decode {
        c.row(
            LIMITS,
            "long decode",
            format!(
                "{} across continuations by default (at most the window minus the input; /continue goes on)",
                card::tokens(budget)
            ),
        );
    }
    c.caps(&Caps(s));
    if !matches!(s.thinking, Thinking::Unsupported) {
        c.row(REASONING, "thought summaries", "included (includeThoughts, sent)");
    }
    if s.tools.contains(&Tool::Maps) {
        let position = if s.maps_position {
            "your latest Telegram location in the conversation, for \"near me\" searches"
        } else {
            "not used: this model ignores it (tested 2026-11-20), so a location is an error; named places work"
        };
        c.row(TOOLS, "+m position", position);
    }
    if s.search_with_maps {
        c.row(TOOLS, "+s with +m", "Search and Maps together in one request");
    }
    if s.tool_invocations {
        c.row(TOOLS, "tool calls", "the tools' own calls and results are shown");
    }
    if s.image_search {
        c.row(TOOLS, "+s", "searches images as well as the web");
    }
    let i = s.inputs;
    let mut takes = vec!["text files"];
    for (on, what) in [
        (i.image, "images"),
        (i.pdf, "PDFs"),
        (i.audio, "audio"),
        (i.video, "video and YouTube links"),
        (i.office, "Office and OpenDocument files"),
    ] {
        if on {
            takes.push(what);
        }
    }
    c.row(INPUT, "takes", takes.join(", "));
    if s.per_part_media_resolution {
        c.row(INPUT, "media resolution", "the highest: images ultra high, PDFs and video high (sent per part)");
    }
    if let Some(o) = s.image_out {
        c.row(OUTPUT, "sizes (size=)", o.sizes.join(", "));
        c.row(OUTPUT, "aspect ratios (ar=)", o.ratios.join(", "));
        if let Some(f) = o.free_size {
            c.row(OUTPUT, "default size", format!("{f} (costs the same as the smaller one)"));
        }
    }
    let rates = |r: &Rates| {
        let mut parts = vec![format!("input {}", usd(r.input))];
        if let Some(a) = r.input_audio {
            parts.push(format!("audio input {}", usd(a)));
        }
        if r.cached < r.input {
            parts.push(format!("cached {}", usd(r.cached)));
        }
        if let Some(a) = r.cached_audio {
            parts.push(format!("cached audio {}", usd(a)));
        }
        parts.push(format!("output {}", usd(r.output)));
        if let Some(img) = r.image_output {
            parts.push(format!("images {}", usd(img)));
        }
        format!("{} / 1M tokens", parts.join(", "))
    };
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
    let mut tiers = vec![Tier::Standard];
    tiers.extend(s.tiers.iter().filter_map(|t| Tier::parse(t)));
    let mut grounding = None;
    for tier in tiers {
        let Some(p) = pricing::price(s.id, tier, now) else { continue };
        grounding.get_or_insert(p.grounding);
        c.row(PRICE, tier.name(), rates(&p.base));
        if let Some(o) = p.over_200k {
            c.row(PRICE, format!("{} over 200K input tokens", tier.name()), rates(&o));
        }
        for change in [pricing::DEC_1_2026, pricing::JAN_1_2027] {
            if change > now
                && let Some(later) = pricing::price(s.id, tier, change).filter(|l| *l != p)
            {
                c.row(PRICE, format!("{} from {}", tier.name(), card::utc(change)), rates(&later.base));
            }
        }
    }
    match grounding {
        Some(Grounding::PerQuery(q)) if s.tools.contains(&Tool::Search) => {
            c.row(
                PRICE,
                "+s / +m queries",
                format!("{} per 1,000 after a free monthly allowance (shown after the cost)", usd(q * 1000.0)),
            );
        }
        Some(Grounding::PerPrompt { search, maps }) => {
            c.row(
                PRICE,
                "grounded prompts",
                format!("+s {}, +m {} per 1,000", usd(search * 1000.0), usd(maps * 1000.0)),
            );
        }
        _ => {}
    }
    c
}

/// What the Gemini API offers for a model (the vendor's own API: official = route).
pub struct Caps<'a>(pub &'a ModelSpec);

impl RouteCaps for Caps<'_> {
    fn name(&self) -> &str {
        super::NAME
    }
    fn tool(&self, tool: Tool) -> bool {
        self.0.tools.contains(&tool)
    }
    fn efforts(&self) -> Option<Vec<String>> {
        match self.0.thinking {
            Thinking::Levels(l) => Some(l.iter().map(|s| s.to_string()).collect()),
            Thinking::Unknown => Some(LEVELS_ALL.iter().map(|s| s.to_string()).collect()),
            Thinking::Budget { .. } | Thinking::AlwaysOn | Thinking::Unsupported => None,
        }
    }
    fn can_disable_reasoning(&self) -> bool {
        match self.0.thinking {
            Thinking::Budget { can_disable, .. } => can_disable,
            Thinking::Unknown => true,
            Thinking::Levels(_) | Thinking::AlwaysOn | Thinking::Unsupported => false,
        }
    }
    fn budget(&self) -> bool {
        matches!(self.0.thinking, Thinking::Budget { .. } | Thinking::Unknown)
    }
    fn verbosity(&self) -> bool {
        false
    }
    fn image_options(&self) -> bool {
        self.0.image_out.is_some()
    }
    fn tiers(&self) -> Vec<String> {
        self.0.tiers.iter().map(|s| s.to_string()).collect()
    }
    fn default_effort(&self) -> Option<String> {
        self.0.default_level.map(str::to_string)
    }
    /// Gemini 3 Pro Image always thinks; nothing to set.
    fn fixed_reasoning(&self) -> bool {
        self.0.thinking == Thinking::AlwaysOn
    }
    /// Gemini 2.5: `+h` the largest `thinkingBudget`, `+l` the smallest (or off where 0 is allowed).
    fn budget_range(&self) -> Option<(u64, u64)> {
        match self.0.thinking {
            Thinking::Budget { min, max, .. } => Some((max, min)),
            _ => None,
        }
    }
}
