//! The Gemini API models the bot knows: limits, thinking controls, built-in
//! tools, service tiers, input modalities and image output options.
//!
//! Sources (fetched 2026-11-12): the model pages
//! `https://aistudio.google.com/docs/models/<id>`, the thinking guide (levels
//! per model), the image generation guide (sizes, aspect ratios, thinking
//! levels of the image models), the Maps / Search / URL context guides
//! (supported models), the deprecation page (auto-routed ids) and
//! docs/research/gemini.md §0.1.
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
    pub sizes: &'static [&'static str],
    pub ratios: &'static [&'static str],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelSpec {
    pub id: &'static str,
    /// Google serves this id with another model (deprecated ids that are
    /// "automatically routed"); limits, levels and prices are the target's.
    pub routes_to: Option<&'static str>,
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
    pub tools: &'static [Tool],
    /// Google Search and Google Maps grounding in one request ("Gemini 3.5
    /// Flash and later").
    pub search_with_maps: bool,
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
        routes_to: None,
        family,
        context: 1_048_576,
        max_output: 65_536,
        unified: false,
        long_decode: false,
        thinking: Thinking::Levels(levels),
        tools: ALL_TOOLS,
        search_with_maps,
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

const fn routed(spec: ModelSpec, id: &'static str, target: &'static str) -> ModelSpec {
    ModelSpec { id, routes_to: Some(target), ..spec }
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
        routes_to: None,
        family: Family::Gemini3,
        context,
        max_output,
        unified: false,
        long_decode: false,
        thinking,
        tools,
        search_with_maps: false,
        tiers: NO_TIERS,
        inputs,
        image_out: Some(image_out),
        per_part_media_resolution: false,
        known: true,
    }
}

const fn gemini25(id: &'static str, min: u64, max: u64, can_disable: bool) -> ModelSpec {
    ModelSpec {
        family: Family::Gemini25,
        thinking: Thinking::Budget { min, max, can_disable },
        search_with_maps: false,
        per_part_media_resolution: false,
        ..text_model(id, Family::Gemini25, LEVELS_HML, false)
    }
}

const GEMINI_38_FLASH: ModelSpec = text_model("gemini-3.8-flash", Family::Gemini3, LEVELS_HML, true);
const GEMINI_36_FLASH: ModelSpec = text_model("gemini-3.6-flash", Family::Gemini3, LEVELS_ALL, true);

/// The table (2026-11-12). Levels from the thinking guide's per-model table;
/// tools from the model pages' capability lists and the tools' "supported
/// models" tables; Search together with Maps on "Gemini 3.5 Flash and later".
pub const MODELS: &[ModelSpec] = &[
    gemini4("gemini-4-carbon"),
    gemini4("gemini-4-argon"),
    GEMINI_38_FLASH,
    // Deprecated; "the endpoint is not shutting down and automatically routes API
    // requests to gemini-3.8-flash" (deprecations page).
    routed(GEMINI_38_FLASH, "gemini-3.7-flash", "gemini-3.8-flash"),
    GEMINI_36_FLASH,
    // Deprecated; routed to gemini-3.6-flash.
    routed(GEMINI_36_FLASH, "gemini-3.5-flash", "gemini-3.6-flash"),
    // "Gemini 3.5 Flash and later" for Search + Maps: taken to include 3.5 Flash-Lite
    // (UNVERIFIED; Google answers with an error if not).
    text_model("gemini-3.5-flash-lite", Family::Gemini3, LEVELS_ALL, true),
    text_model("gemini-3.1-pro-preview", Family::Gemini3, LEVELS_HML, false),
    text_model("gemini-3.1-flash-lite", Family::Gemini3, LEVELS_ALL, false),
    text_model("gemini-3-flash-preview", Family::Gemini3, LEVELS_ALL, false),
    image_model(
        "gemini-nano-banana-2.1",
        131_072,
        32_768,
        Thinking::Levels(&["high", "medium", "minimal"]),
        SEARCH_ONLY,
        IMAGE_MODEL,
        ImageOut { sizes: &["1K", "2K", "4K"], ratios: RATIOS_EXTENDED },
    ),
    image_model(
        "gemini-3.1-flash-image",
        131_072,
        32_768,
        Thinking::Levels(&["high", "minimal"]),
        SEARCH_ONLY,
        IMAGE_MODEL,
        ImageOut { sizes: &["512", "1K", "2K", "4K"], ratios: RATIOS_EXTENDED },
    ),
    // "Supported image_size values: 1024px (1K). (Note: 2K and 4K are unsupported)";
    // the model page lists the ten base ratios (while also saying "14").
    image_model(
        "gemini-3.1-flash-lite-image",
        65_536,
        4_096,
        Thinking::Levels(&["high", "minimal"]),
        NO_TOOLS,
        IMAGE_MODEL,
        ImageOut { sizes: &["1K"], ratios: RATIOS_BASE },
    ),
    image_model(
        "gemini-3-pro-image",
        65_536,
        32_768,
        Thinking::AlwaysOn,
        SEARCH_ONLY,
        PRO_IMAGE,
        ImageOut { sizes: &["1K", "2K", "4K"], ratios: RATIOS_BASE },
    ),
    // Gemini 2.5 (deprecated, access limited to earlier users; routed to 3.8 Flash /
    // 3.5 Flash-Lite from 2026-12-01). Budget ranges from the thinking guide.
    gemini25("gemini-2.5-pro", 128, 32_768, false),
    gemini25("gemini-2.5-flash", 0, 24_576, true),
    gemini25("gemini-2.5-flash-lite", 512, 24_576, true),
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
        routes_to: None,
        family,
        context: if gemini4 { 1_048_576 } else { 0 },
        max_output: if gemini4 { 65_536 } else { 0 },
        unified: gemini4,
        long_decode: gemini4,
        thinking,
        tools: ALL_TOOLS,
        search_with_maps: true,
        tiers: TIERS,
        inputs: FULL,
        image_out: image.then_some(ImageOut { sizes: &["512", "1K", "2K", "4K"], ratios: RATIOS_EXTENDED }),
        per_part_media_resolution: matches!(family, Family::Gemini4 | Family::Gemini3) && !image,
        known: false,
    }
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
            Thinking::Budget { .. } | Thinking::AlwaysOn => None,
        }
    }
    fn can_disable_reasoning(&self) -> bool {
        match self.0.thinking {
            Thinking::Budget { can_disable, .. } => can_disable,
            Thinking::Unknown => true,
            Thinking::Levels(_) | Thinking::AlwaysOn => false,
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
}
